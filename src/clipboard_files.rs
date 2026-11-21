//! A single live clipboard offer backed by pinned files or an image snapshot.
//! Peer requests contain numeric item IDs; remote paths are never accepted.
use crate::wire::{self, Field};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
};

#[derive(PartialEq, Eq)]
struct Fingerprint(u64, u64, u64, i64, i64);
fn fingerprint(file: &File) -> anyhow::Result<Fingerprint> {
    let m = file.metadata()?;
    anyhow::ensure!(m.is_file(), "Clipboard source must be a regular file");
    Ok(Fingerprint(
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
    ))
}
pub(crate) struct Source {
    data: SourceData,
    pub name: String,
    pub size: u64,
    pub mime: &'static str,
    code: u8,
}
enum SourceData {
    File(File, Fingerprint),
    Image(Vec<u8>),
}
fn file_type(name: &str) -> (u8, &'static str) {
    // Native MimeMapper codes, cross-checked offline. Unlike universal text
    // (236), text/plain (189) here describes a file and remains a lazy offer.
    match std::path::Path::new(name)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "heic" => (241, "image/heic"),
        "heif" => (242, "image/heif"),
        "jpg" | "jpeg" => (155, "image/jpeg"),
        "png" => (157, "image/png"),
        "gif" => (152, "image/gif"),
        "bmp" => (151, "image/bmp"),
        "tif" | "tiff" => (159, "image/tiff"),
        "svg" => (158, "image/svg+xml"),
        "pdf" => (11, "application/pdf"),
        "zip" => (18, "application/zip"),
        "doc" => (35, "application/msword"),
        "docx" => (
            36,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        "xlsx" => (
            39,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        "pptx" => (
            42,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        "txt" => (189, "text/plain"),
        "mp4" => (222, "video/mp4"),
        "mp3" => (138, "audio/mpeg"),
        _ => (0, "application/octet-stream"),
    }
}
pub(crate) struct Offer {
    pub sequence: u8,
    pub files: Vec<Source>,
}
impl Offer {
    pub fn from_content(content: crate::clipboard::Content, sequence: u8) -> anyhow::Result<Self> {
        match content {
            crate::clipboard::Content::Files(paths) => Self::open(paths, sequence),
            crate::clipboard::Content::Image(image) => {
                anyhow::ensure!(
                    (1..=crate::clipboard::MAX_IMAGE_BYTES).contains(&image.bytes.len()),
                    "Clipboard image exceeds size limit"
                );
                let (code, mime) = file_type(image.name);
                let nonce = u64::from_ne_bytes(crate::private::random()?);
                let extension = image.name.rsplit('.').next().unwrap_or("png");
                Ok(Self {
                    sequence: sequence & 127,
                    files: vec![Source {
                        name: format!("Clipboard-{nonce:016x}.{extension}"),
                        size: image.bytes.len() as u64,
                        data: SourceData::Image(image.bytes),
                        code,
                        mime,
                    }],
                })
            }
            _ => anyhow::bail!("Clipboard is not a file or image"),
        }
    }
    pub fn open(paths: Vec<PathBuf>, sequence: u8) -> anyhow::Result<Self> {
        Self::open_files(paths, sequence, crate::file_store::MAX_FILE_SIZE)
    }
    pub fn open_for_share(paths: Vec<PathBuf>) -> anyhow::Result<Self> {
        Self::open_files(paths, 0, crate::file_protocol::MAX_OUTGOING_SHARE_SIZE)
    }
    fn open_files(paths: Vec<PathBuf>, sequence: u8, max_total_size: u64) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=128).contains(&paths.len()),
            "Unsupported clipboard file count"
        );
        let mut total = 0u64;
        let mut files = Vec::new();
        for path in paths {
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("Invalid clipboard filename"))?;
            anyhow::ensure!(
                crate::file_store::valid_name(name),
                "Unsupported clipboard filename"
            );
            // macOS O_NOFOLLOW. Pin the selected inode so a later path swap
            // cannot expose a different file through an old clipboard offer.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(0x104)
                .open(&path)?;
            let stamp = fingerprint(&file)?;
            let size = stamp.2;
            total = total
                .checked_add(size)
                .ok_or_else(|| anyhow::anyhow!("Clipboard size overflow"))?;
            anyhow::ensure!(total <= max_total_size, "Offered files exceed size limit");
            let (code, mime) = file_type(name);
            files.push(Source {
                data: SourceData::File(file, stamp),
                name: name.into(),
                size,
                code,
                mime,
            });
        }
        Ok(Self {
            sequence: sequence & 127,
            files,
        })
    }
    pub fn announcement(&self) -> Vec<u8> {
        crate::message_center::file_payload(
            &self.files[0].name,
            self.sequence,
            &self.files.iter().map(|f| f.code).collect::<Vec<_>>(),
        )
        .unwrap()
    }
    pub fn check(&self, index: usize) -> anyhow::Result<&Source> {
        let file = self
            .files
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("Unknown clipboard item"))?;
        if let SourceData::File(source, stamp) = &file.data {
            anyhow::ensure!(
                fingerprint(source)? == *stamp,
                "Copied file changed; copy it again"
            );
        }
        Ok(file)
    }
    pub fn read(&self, index: usize, offset: u64) -> anyhow::Result<Vec<u8>> {
        let source = self.check(index)?;
        anyhow::ensure!(offset <= source.size, "Invalid clipboard offset");
        let file = match &source.data {
            SourceData::Image(bytes) => {
                let start = offset as usize;
                return Ok(bytes[start..bytes.len().min(start + 16384)].to_vec());
            }
            SourceData::File(file, _) => file,
        };
        let mut bytes = vec![0; (source.size - offset).min(16384) as usize];
        let mut filled = 0;
        while filled < bytes.len() {
            let count = file.read_at(&mut bytes[filled..], offset + filled as u64)?;
            anyhow::ensure!(count != 0, "Copied file truncated");
            filled += count;
        }
        self.check(index)?;
        Ok(bytes)
    }
    pub fn reply(&self, bytes: &[u8]) -> anyhow::Result<Reply> {
        let root = fields(bytes)?;
        let kind = number(&root, 1)?;
        let body_field = match kind {
            1 => 3,
            4 => 6,
            _ => anyhow::bail!("Unsupported clipboard request"),
        };
        anyhow::ensure!(root.len() == 2, "Ambiguous clipboard envelope");
        let body = fields(data(&root, body_field)?)?;
        let sequence = number(&body, 1)?;
        anyhow::ensure!(
            sequence == u64::from(self.sequence),
            "Expired clipboard offer"
        );
        match kind {
            1 => {
                anyhow::ensure!(
                    body.len() <= 1 && body.iter().all(|f| matches!(f, Field::Integer(1, _))),
                    "Invalid clipboard metadata request"
                );
                let mut response = wire::integer(1, sequence);
                for (index, _) in self.files.iter().enumerate() {
                    let file = self.check(index)?;
                    let mut info = wire::message(1, file.name.as_bytes());
                    info.extend(wire::integer(2, file.size));
                    let mut entry = wire::integer(1, index as u64);
                    entry.extend(wire::message(2, &info));
                    response.extend(wire::message(3, &entry));
                }
                Ok(Reply {
                    message: envelope(3, 5, &response),
                    file: None,
                })
            }
            4 => {
                // Proto3 omits zero for the first item index.
                let index = number(&body, 2)? as usize;
                anyhow::ensure!(
                    body.len() <= 2 && body.iter().all(|f| matches!(f, Field::Integer(1 | 2, _))),
                    "Invalid clipboard file request"
                );
                self.check(index)?;
                let mut response = wire::integer(1, sequence);
                response.extend(wire::integer(2, index as u64));
                Ok(Reply {
                    message: envelope(5, 7, &response),
                    file: Some(index),
                })
            }
            _ => unreachable!(),
        }
    }
}
pub(crate) struct Reply {
    pub message: Vec<u8>,
    pub file: Option<usize>,
}
fn envelope(kind: u64, field: u32, body: &[u8]) -> Vec<u8> {
    let mut out = wire::integer(1, kind);
    out.extend(wire::message(field, body));
    out
}
fn fields(input: &[u8]) -> anyhow::Result<Vec<Field<'_>>> {
    anyhow::ensure!(input.len() <= 16384, "Oversized clipboard request");
    wire::fields(input).ok_or_else(|| anyhow::anyhow!("Malformed clipboard request"))
}
fn number(fields: &[Field<'_>], id: u32) -> anyhow::Result<u64> {
    let mut found = fields
        .iter()
        .filter(|f| matches!(f, Field::Integer(n, _) | Field::Bytes(n, _) if *n == id));
    let value = match found.next() {
        Some(Field::Integer(_, n)) => *n,
        None => 0,
        _ => anyhow::bail!("Invalid clipboard integer"),
    };
    anyhow::ensure!(found.next().is_none(), "Duplicate clipboard field");
    Ok(value)
}
fn data<'a>(fields: &[Field<'a>], id: u32) -> anyhow::Result<&'a [u8]> {
    let mut found = fields
        .iter()
        .filter(|f| matches!(f, Field::Integer(n, _) | Field::Bytes(n, _) if *n == id));
    let Some(Field::Bytes(_, bytes)) = found.next() else {
        anyhow::bail!("Missing clipboard body")
    };
    anyhow::ensure!(found.next().is_none(), "Duplicate clipboard body");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_snapshot_serves_exact_bytes_in_chunks_and_rejects_stale_requests() {
        let bytes: Vec<u8> = (0..40_000).map(|i| (i % 251) as u8).collect();
        let offer = Offer::from_content(
            crate::clipboard::Content::Image(crate::clipboard::Image {
                name: "Clipboard.png",
                bytes: bytes.clone(),
            }),
            9,
        )
        .unwrap();
        assert_eq!(offer.check(0).unwrap().mime, "image/png");
        assert_eq!(offer.check(0).unwrap().size, bytes.len() as u64);
        let mut received = Vec::new();
        while received.len() < bytes.len() {
            received.extend(offer.read(0, received.len() as u64).unwrap());
        }
        assert_eq!(received, bytes);
        assert!(offer.read(0, 40_000).unwrap().is_empty());
        assert!(offer.read(0, 40_001).is_err());
        assert!(offer.read(1, 0).is_err());
        let mut request = wire::integer(1, 4);
        request.extend(wire::message(6, &wire::integer(1, 8)));
        assert!(offer.reply(&request).is_err());
        assert!(
            Offer::from_content(
                crate::clipboard::Content::Image(crate::clipboard::Image {
                    name: "Clipboard.png",
                    bytes: vec![],
                }),
                1
            )
            .is_err()
        );
    }
    #[test]
    fn only_copied_pinned_unchanged_files_can_be_requested() {
        let root = std::env::temp_dir().join(format!(
            "clip-offer-{:016x}",
            u64::from_ne_bytes(crate::private::random().unwrap())
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sample.bin");
        std::fs::write(&path, b"test").unwrap();
        let offer = Offer::open(vec![path.clone()], 7).unwrap();
        let mut request = wire::integer(1, 4);
        request.extend(wire::message(6, &wire::integer(1, 7)));
        assert_eq!(offer.reply(&request).unwrap().file, Some(0));
        assert_eq!(offer.read(0, 0).unwrap(), b"test");
        assert!(offer.check(1).is_err());
        let mut wrong = wire::integer(1, 4);
        wrong.extend(wire::message(6, &wire::integer(1, 6)));
        assert!(offer.reply(&wrong).is_err());
        std::fs::rename(&path, root.join("old.bin")).unwrap();
        std::fs::write(&path, b"private replacement").unwrap();
        assert_eq!(offer.read(0, 0).unwrap(), b"test");
        std::fs::write(root.join("old.bin"), b"changed").unwrap();
        assert!(offer.read(0, 0).is_err());
        assert!(Offer::open(vec![root.clone()], 1).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
