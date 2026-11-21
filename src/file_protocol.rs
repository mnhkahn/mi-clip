//! File-share request metadata, carried by authenticated Express byte packets.
//! Acceptance permits transport inspection; it does not publish a file.
use crate::wire::{self, Field};

// Express stream offers use signed 64-bit file lengths.
pub(crate) const MAX_OUTGOING_SHARE_SIZE: u64 = i64::MAX as u64;

pub(crate) struct Manifest<'a> {
    kind: u32,
    pub task: &'a [u8],
    pub files: Vec<FileInfo<'a>>,
}
pub(crate) struct FileInfo<'a> {
    pub name: &'a str,
    pub id: &'a [u8],
    pub size: u64,
}

fn fields(input: &[u8]) -> anyhow::Result<Vec<Field<'_>>> {
    wire::fields_with_limit(input, 4 * 1024 * 1024)
        .ok_or_else(|| anyhow::anyhow!("Malformed file-share metadata"))
}
fn bytes<'a>(fields: &[Field<'a>], id: u32) -> anyhow::Result<&'a [u8]> {
    let mut matching = fields.iter().filter(|f| match f {
        Field::Bytes(n, _) | Field::Integer(n, _) => *n == id,
    });
    let Some(Field::Bytes(_, value)) = matching.next() else {
        anyhow::bail!("Missing file-share byte field");
    };
    anyhow::ensure!(matching.next().is_none(), "Duplicate file-share field");
    Ok(value)
}
fn integer(fields: &[Field<'_>], id: u32) -> anyhow::Result<u64> {
    let mut matching = fields.iter().filter(|f| match f {
        Field::Bytes(n, _) | Field::Integer(n, _) => *n == id,
    });
    let Some(Field::Integer(_, value)) = matching.next() else {
        anyhow::bail!("Missing file-share integer");
    };
    anyhow::ensure!(matching.next().is_none(), "Duplicate file-share field");
    Ok(*value)
}
fn identifier(value: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(c)),
        "Invalid file-share identifier"
    );
    Ok(())
}

pub(crate) fn request(input: &[u8]) -> anyhow::Result<Manifest<'_>> {
    request_with_limit(input, crate::file_store::MAX_FILE_SIZE)
}

pub(crate) fn request_with_limit(
    input: &[u8],
    max_total_size: u64,
) -> anyhow::Result<Manifest<'_>> {
    let root = fields(input)?;
    anyhow::ensure!(integer(&root, 1)? == 1, "Not a file-send request");
    let request = fields(bytes(&root, 2)?)?;
    let kind = u32::try_from(integer(&request, 1)?)?;
    let task = bytes(&request, 3)?;
    identifier(task)?;
    let mut files = Vec::new();
    let mut total = 0u64;
    for value in request.iter().filter_map(|f| match f {
        Field::Bytes(5, value) => Some(*value),
        _ => None,
    }) {
        anyhow::ensure!(files.len() < 128, "Too many shared files");
        let file = fields(value)?;
        let name = std::str::from_utf8(bytes(&file, 1)?)?;
        anyhow::ensure!(
            crate::file_store::valid_name(name),
            "Unsafe shared filename"
        );
        let id = bytes(&file, 3)?;
        identifier(id)?;
        anyhow::ensure!(
            files.iter().all(|f: &FileInfo<'_>| f.id != id),
            "Duplicate shared file ID"
        );
        let size = integer(&file, 2)?;
        total = total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("Shared file size overflow"))?;
        anyhow::ensure!(total <= max_total_size, "Shared files exceed size limit");
        files.push(FileInfo { name, id, size });
    }
    anyhow::ensure!(!files.is_empty(), "File-share request contains no files");
    Ok(Manifest { kind, task, files })
}

/// Native FileSendRequestResponse field 3 is RejectReason: zero accepts.
/// The native receive path starts transfer for zero and destroys the channel
/// for one. Field 1 echoes the request kind; field 2 identifies its task.
pub(crate) fn accept(manifest: &Manifest<'_>) -> Vec<u8> {
    let mut response = wire::integer(1, manifest.kind.into());
    response.extend(wire::message(2, manifest.task));
    response.extend(wire::integer(3, 0));
    let mut root = wire::integer(1, 2);
    root.extend(wire::message(2, &response));
    root
}

/// The native receiver sends command 8 after all file-success callbacks. Its
/// payload contains the original request kind, not a count or success boolean.
pub(crate) fn completion(manifest: &Manifest<'_>) -> Vec<u8> {
    let mut root = wire::integer(1, 8);
    root.extend(wire::message(2, &wire::integer(1, manifest.kind.into())));
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(name: &[u8], size: u64) -> Vec<u8> {
        let mut file = wire::message(1, name);
        file.extend(wire::integer(2, size));
        file.extend(wire::message(3, b"synthetic-file"));
        let mut body = wire::integer(1, 7);
        body.extend(wire::message(3, b"synthetic-task"));
        body.extend(wire::message(5, &file));
        let mut root = wire::integer(1, 1);
        root.extend(wire::message(2, &body));
        root
    }
    #[test]
    fn large_preview_is_bounded_without_widening_other_protocol_messages() {
        let input = packet(b"sample.bin", 4);
        let root = fields(&input).unwrap();
        let mut body = bytes(&root, 2).unwrap().to_vec();
        body.extend(wire::message(4, &vec![0; 160 * 1024]));
        let mut input = wire::integer(1, 1);
        input.extend(wire::message(2, &body));
        assert_eq!(request(&input).unwrap().files[0].size, 4);
        assert!(wire::fields(&input).is_none());
        assert!(fields(&vec![0; 4 * 1024 * 1024 + 1]).is_err());
    }
    #[test]
    fn request_binds_ack_to_task_and_rejects_unsafe_or_ambiguous_metadata() {
        let input = packet(b"example.txt", 7);
        let manifest = request(&input).unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].name, "example.txt");
        assert_eq!(manifest.files[0].size, 7);
        let accepted = accept(&manifest);
        let root = fields(&accepted).unwrap();
        assert_eq!(integer(&root, 1).unwrap(), 2);
        let body = fields(bytes(&root, 2).unwrap()).unwrap();
        assert_eq!(integer(&body, 1).unwrap(), 7);
        assert_eq!(integer(&body, 3).unwrap(), 0);
        assert_eq!(bytes(&body, 2).unwrap(), b"synthetic-task");
        let completed = completion(&manifest);
        let root = fields(&completed).unwrap();
        assert_eq!(integer(&root, 1).unwrap(), 8);
        let body = fields(bytes(&root, 2).unwrap()).unwrap();
        assert_eq!(body.len(), 1);
        assert_eq!(integer(&body, 1).unwrap(), 7);
        for name in [
            b"../escape".as_slice(),
            b".",
            b"a\\b",
            b"a:b",
            b"a\0b",
            b"\xff",
        ] {
            assert!(request(&packet(name, 1)).is_err());
        }
        assert!(request(&packet(b"large", u64::MAX)).is_err());
        let mut duplicate = input.clone();
        duplicate.extend(wire::integer(1, 1));
        assert!(request(&duplicate).is_err());
        for end in 0..input.len() {
            assert!(request(&input[..end]).is_err());
        }
    }
}
