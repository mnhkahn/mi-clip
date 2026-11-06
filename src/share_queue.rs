//! Private handoff from the sandboxed macOS Share extension. Each request owns
//! its staged files until Android confirms completion or the request fails.
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

pub(crate) struct Request {
    directory: PathBuf,
    pub target: std::net::SocketAddr,
    pub offer: std::sync::Arc<crate::clipboard_files::Offer>,
    finished: bool,
    failure: Option<String>,
    last_progress: std::time::Instant,
}
impl Request {
    #[cfg(test)]
    pub fn next(root: &Path) -> anyhow::Result<Option<Self>> {
        Self::next_for(root, None)
    }
    pub fn next_for(
        root: &Path,
        target: Option<std::net::SocketAddr>,
    ) -> anyhow::Result<Option<Self>> {
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let directory = entry.path();
            if !entry.file_type()?.is_dir() || directory.extension().is_none_or(|s| s != "ready") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&directory)?;
            anyhow::ensure!(
                metadata.uid() == unsafe { getuid() } && metadata.mode() & 0o077 == 0,
                "Insecure share request directory"
            );
            let age = metadata.modified()?.elapsed().unwrap_or_default();
            // Never silently replay a share after the UI timed out or after restart.
            if age.as_secs() > 120 || directory.join("status.json").exists() {
                continue;
            }
            let bytes = std::fs::read(directory.join("request.json"))?;
            anyhow::ensure!(bytes.len() <= 65536, "Oversized share request");
            let description: Description = match serde_json::from_slice(&bytes) {
                Ok(description) => description,
                Err(_) => {
                    // Old extensions did not carry a selected target. Never
                    // guess one, or let a stale request block newer selections.
                    let _ = crate::private::save(&directory.join("status.json"),
                        br#"{"state":"failed","sent":0,"message":"Select a device in the updated Clip sharing window"}"#);
                    continue;
                }
            };
            validate_selected(description.target)?;
            if target.is_some_and(|target| target != description.target) {
                continue;
            }
            let names = description.files;
            anyhow::ensure!((1..=128).contains(&names.len()), "Invalid share file count");
            let paths = names
                .iter()
                .map(|name| {
                    anyhow::ensure!(crate::file_store::valid_name(name), "Unsafe share filename");
                    Ok(directory.join("files").join(name))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let offer = crate::clipboard_files::Offer::open(paths, 0)?;
            let mut request = Self {
                directory,
                target: description.target,
                offer: std::sync::Arc::new(offer),
                finished: false,
                failure: None,
                last_progress: std::time::Instant::now(),
            };
            request.status("connecting", 0)?;
            return Ok(Some(request));
        }
        Ok(None)
    }
    pub fn exists(&self) -> bool {
        self.directory.is_dir()
    }
    pub fn status(&mut self, state: &str, sent: u64) -> anyhow::Result<()> {
        if state == "sending"
            && self.last_progress.elapsed() < std::time::Duration::from_millis(250)
        {
            return Ok(());
        }
        self.last_progress = std::time::Instant::now();
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let temp = self.directory.join(format!(
            "status-{:016x}.tmp",
            u64::from_ne_bytes(crate::private::random()?)
        ));
        // Do not recreate a directory removed by the extension's Cancel action.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        let result = (|| -> anyhow::Result<()> {
            file.write_all(&serde_json::to_vec(
                &serde_json::json!({"state": state, "sent": sent, "message": self.failure}),
            )?)?;
            std::fs::rename(&temp, self.directory.join("status.json"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result?;
        Ok(())
    }
    pub fn fail(&mut self, message: &str) -> anyhow::Result<()> {
        self.failure = Some(message.to_owned());
        self.status("failed", 0)?;
        self.finished = true;
        Ok(())
    }
    pub fn complete(&mut self) -> anyhow::Result<()> {
        self.status("completed", self.offer.files.iter().map(|f| f.size).sum())?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if !self.finished && self.exists() {
            let _ = self.status("failed", 0);
        }
    }
}
unsafe extern "C" {
    fn getuid() -> u32;
}

pub(crate) fn root() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("HOME")?).join(
        "Library/Containers/com.cyeam.miclip.share/Data/Library/Application Support/MiClip/Outbox",
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Description {
    files: Vec<String>,
    target: std::net::SocketAddr,
}
pub(crate) fn validate_selected(target: std::net::SocketAddr) -> anyhow::Result<()> {
    anyhow::ensure!(
        target.port() != 0 && !target.ip().is_unspecified() && !target.ip().is_multicast(),
        "Invalid selected device"
    );
    Ok(())
}
/// Inspect only ready, unclaimed requests. Opening the share sheet or loading
/// files does not create one: the user must first click a device to send.
pub fn pending_targets() -> Vec<std::net::SocketAddr> {
    pending_targets_in(root().as_deref())
}
fn pending_targets_in(root: Option<&Path>) -> Vec<std::net::SocketAddr> {
    let mut targets = Vec::new();
    let Some(root) = root else { return targets };
    let Ok(entries) = std::fs::read_dir(root) else {
        return targets;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if dir.extension().is_none_or(|e| e != "ready") || dir.join("status.json").exists() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_dir()
            || metadata
                .modified()
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
                .elapsed()
                .unwrap_or_default()
                .as_secs()
                > 120
        {
            continue;
        }
        let path = dir.join("request.json");
        if std::fs::metadata(&path).map_or(true, |m| m.len() > 65536) {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(data) = serde_json::from_slice::<Description>(&bytes) else {
            continue;
        };
        if validate_selected(data.target).is_ok() {
            if !targets.contains(&data.target) {
                targets.push(data.target);
            }
        }
    }
    targets
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ShareDevice {
    pub name: String,
    pub device_type: u8,
    pub endpoint: std::net::SocketAddr,
}
pub fn fail_pending(target: std::net::SocketAddr, message: &str) {
    if let Some(root) = root()
        && let Ok(Some(mut request)) = Request::next_for(&root, Some(target))
    {
        let _ = request.fail(message);
    }
}
pub fn take_device_refresh() -> bool {
    root()
        .and_then(|p| p.parent().map(|p| p.join("refresh-devices")))
        .is_some_and(|p| std::fs::remove_file(p).is_ok())
}
static PUBLISH: std::sync::Mutex<()> = std::sync::Mutex::new(());
static DISCOVERED: std::sync::Mutex<Vec<ShareDevice>> = std::sync::Mutex::new(Vec::new());
static CONNECTED: std::sync::Mutex<Vec<std::net::SocketAddr>> = std::sync::Mutex::new(Vec::new());
pub fn discovered_devices() -> Vec<ShareDevice> {
    DISCOVERED.lock().unwrap().clone()
}
pub fn set_connected(target: std::net::SocketAddr, connected: bool) {
    let _publish = PUBLISH.lock().unwrap();
    let mut targets = CONNECTED.lock().unwrap();
    targets.retain(|endpoint| *endpoint != target);
    if connected {
        targets.push(target);
    }
    drop(targets);
    let devices = DISCOVERED.lock().unwrap().clone();
    let _ = write_devices(devices);
}
/// Short-lived, private device catalogue for the sandboxed picker. No account
/// information, certificates, clipboard data, or file contents enter this file.
pub fn publish_devices(devices: Vec<ShareDevice>) -> anyhow::Result<()> {
    let _publish = PUBLISH.lock().unwrap();
    *DISCOVERED.lock().unwrap() = devices.clone();
    write_devices(devices)
}
fn write_devices(mut devices: Vec<ShareDevice>) -> anyhow::Result<()> {
    for endpoint in CONNECTED.lock().unwrap().iter().copied() {
        if devices.iter().any(|d| d.endpoint == endpoint) {
            continue;
        }
        devices.push(ShareDevice {
            name: "已连接的设备".into(),
            device_type: 0,
            endpoint,
        });
    }
    use std::os::unix::fs::DirBuilderExt;
    let Some(root) = root().and_then(|p| p.parent().map(Path::to_path_buf)) else {
        return Ok(());
    };
    // Wait for macOS/the extension to create its own sandbox container.
    // Creating another app's container from outside can prevent sandbox launch.
    if !root.parent().is_some_and(Path::exists) {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    crate::private::save(
        &root.join("devices.json"),
        &serde_json::to_vec(&serde_json::json!({"updated": now, "devices": devices}))?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn share_is_claimed_once_and_failure_is_published_on_disconnect() {
        let root = std::env::temp_dir().join(format!(
            "clip-queue-{:016x}",
            u64::from_ne_bytes(crate::private::random().unwrap())
        ));
        let dir = root.join("request.ready");
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.join("files/test.txt"), b"synthetic").unwrap();
        std::fs::write(
            dir.join("request.json"),
            br#"{"files":["test.txt"],"target":"127.0.0.1:1"}"#,
        )
        .unwrap();
        let request = Request::next(&root).unwrap().unwrap();
        assert!(Request::next(&root).unwrap().is_none());
        assert_eq!(request.offer.read(0, 0).unwrap(), b"synthetic");
        drop(request);
        let status: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("status.json")).unwrap()).unwrap();
        assert_eq!(status["state"], "failed");
        std::fs::remove_file(dir.join("status.json")).unwrap();
        std::fs::write(
            dir.join("request.json"),
            br#"{"files":["../private.txt"],"target":"127.0.0.1:1"}"#,
        )
        .unwrap();
        assert!(Request::next(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn multiple_pending_targets_are_discovered_and_claimed_separately() {
        let root = std::env::temp_dir().join(format!(
            "clip-multi-{:016x}",
            u64::from_ne_bytes(crate::private::random().unwrap())
        ));
        let targets = [
            "127.0.0.1:1".parse().unwrap(),
            "127.0.0.1:2".parse().unwrap(),
        ];
        for (index, target) in targets.iter().enumerate() {
            let dir = root.join(format!("{index}.ready"));
            std::fs::create_dir_all(dir.join("files")).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::write(dir.join("files/test.txt"), b"synthetic").unwrap();
            std::fs::write(
                dir.join("request.json"),
                serde_json::to_vec(&serde_json::json!({"files": ["test.txt"], "target": target}))
                    .unwrap(),
            )
            .unwrap();
        }
        assert_eq!(pending_targets_in(Some(&root)).len(), 2);
        let first = Request::next_for(&root, Some(targets[0])).unwrap().unwrap();
        assert_eq!(pending_targets_in(Some(&root)), vec![targets[1]]);
        let second = Request::next_for(&root, Some(targets[1])).unwrap().unwrap();
        assert_eq!(first.target, targets[0]);
        assert_eq!(second.target, targets[1]);
        assert!(pending_targets_in(Some(&root)).is_empty());
        drop((first, second));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn queued_selection_never_falls_back_to_another_connected_phone() {
        let root = std::env::temp_dir().join(format!(
            "clip-selection-{:016x}",
            u64::from_ne_bytes(crate::private::random().unwrap())
        ));
        let dir = root.join("request.ready");
        std::fs::create_dir_all(dir.join("files")).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.join("files/test.txt"), b"synthetic").unwrap();
        // A pre-selection request is invalid; it must not auto-pick a phone.
        std::fs::write(dir.join("request.json"), br#"{"files":["test.txt"]}"#).unwrap();
        assert!(
            Request::next_for(&root, Some("127.0.0.1:2".parse().unwrap()))
                .unwrap()
                .is_none()
        );
        assert!(dir.join("status.json").exists());
        std::fs::remove_file(dir.join("status.json")).unwrap();
        std::fs::write(
            dir.join("request.json"),
            br#"{"files":["test.txt"],"target":"127.0.0.1:1"}"#,
        )
        .unwrap();
        assert!(
            Request::next_for(&root, Some("127.0.0.1:2".parse().unwrap()))
                .unwrap()
                .is_none()
        );
        assert!(!dir.join("status.json").exists());
        let mut request = Request::next_for(&root, Some("127.0.0.1:1".parse().unwrap()))
            .unwrap()
            .unwrap();
        request.fail("Phone did not accept the request").unwrap();
        let status: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("status.json")).unwrap()).unwrap();
        assert_eq!(status["state"], "failed");
        assert_eq!(status["message"], "Phone did not accept the request");
        assert_eq!(status["sent"], 0);
        drop(request);
        std::fs::remove_dir_all(root).unwrap();
    }
}
