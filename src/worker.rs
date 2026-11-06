use crate::tray::TrayEvent;
use async_channel::Sender;
use mi_clip::handshake::{self, SyncEvent};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// Discovery publishes the current catalogue; each endpoint owns its socket,
/// authentication state, clipboard cursor and retry loop. A pending share adds
/// its target without disrupting any existing session.
pub fn start(config: PathBuf, events: Sender<TrayEvent>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut sessions = std::collections::HashMap::new();
        let _ = events.try_send(TrayEvent::Status("连接中", "正在发现局域网设备".into()));
        while !stop.load(Ordering::Relaxed) {
            let fallback = match handshake::configured_endpoint(&config) {
                Ok(endpoint) => endpoint,
                Err(error) => {
                    let detail = format!("发现设备／读取配置失败：{error:#}");
                    let _ = events.try_send(TrayEvent::Status(
                        "等待重连",
                        format!("{detail} · 2 秒后重试"),
                    ));
                    for _ in 0..8 {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    let _ = events.try_send(TrayEvent::Status("连接中", detail));
                    continue;
                }
            };
            let mut targets: Vec<_> = mi_clip::share_queue::discovered_devices()
                .into_iter()
                .map(|device| device.endpoint)
                .collect();
            if targets.is_empty() {
                targets.extend(fallback);
            }
            targets.extend(mi_clip::share_queue::pending_targets());
            for target in targets {
                sessions.entry(target).or_insert_with(|| {
                    start_session(config.clone(), target, events.clone(), stop.clone())
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        for (_, session) in sessions {
            let _ = session.join();
        }
    });
}

fn start_session(
    config: PathBuf,
    target: std::net::SocketAddr,
    events: Sender<TrayEvent>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut backoff = 2;
        let mut last_error = String::new();
        let analytics = std::cell::RefCell::new(mi_clip::analytics::Session::default());
        while !stop.load(Ordering::Relaxed) {
            analytics.borrow_mut().begin();
            let _ = events.send_blocking(TrayEvent::SessionStatus(
                target,
                "连接中",
                last_error.clone(),
            ));
            let connected = std::cell::Cell::new(false);
            let attempted = std::cell::Cell::new(None);
            let phase = std::cell::Cell::new("发现设备／读取配置");
            let result = handshake::sync_target(&config, target, &stop, &|event| {
                if let Some(event) = analytics.borrow_mut().observe(&event) {
                    mi_clip::analytics::capture(event);
                }
                let status = match event {
                    SyncEvent::File(event) => {
                        let _ = events.send_blocking(TrayEvent::SessionFile(target, event));
                        return;
                    }
                    SyncEvent::Connecting(endpoint) => {
                        attempted.set(Some(endpoint));
                        phase.set("建立设备通道");
                        ("连接中", last_error.clone())
                    }
                    SyncEvent::Authenticating => {
                        phase.set("设备认证");
                        ("认证中", last_error.clone())
                    }
                    SyncEvent::Connected => {
                        mi_clip::share_queue::set_connected(target, true);
                        phase.set("已连接／同步");
                        connected.set(true);
                        ("已连接", String::new())
                    }
                    SyncEvent::Received(_) | SyncEvent::Sent(_) => ("已连接", String::new()),
                    SyncEvent::Unsupported => ("已连接", "已跳过超出长度限制的内容".into()),
                    SyncEvent::FileClipboardSkipped => {
                        ("已连接", "该文件暂不支持同步，请复制普通文件后重试".into())
                    }
                    SyncEvent::FileClipboardOffered(files) => (
                        "已连接",
                        format!("{files} 个文件已就绪 · 可在设备兼容应用粘贴"),
                    ),
                    SyncEvent::ImageClipboardOffered => {
                        ("已连接", "图片已就绪 · 可在设备兼容应用粘贴".into())
                    }
                    SyncEvent::FileClipboardRequested => {
                        ("已连接", "设备正在取用复制的文件".into())
                    }
                    SyncEvent::FileClipboardSent(files) => {
                        ("已连接", format!("已向设备发送 {files} 个文件"))
                    }
                    SyncEvent::FileClipboardFailed => {
                        ("已连接", "文件取用失败 · 请重新复制后粘贴".into())
                    }
                    SyncEvent::ShareStarted => ("已连接", "正在连接所选分享设备".into()),
                    SyncEvent::ShareCompleted(files) => {
                        ("已连接", format!("设备已确认接收 {files} 个文件"))
                    }
                    SyncEvent::ShareFailed => {
                        ("已连接", "文件分享失败 · 请查看共享窗口后重试".into())
                    }
                    SyncEvent::UnsupportedClipboard => {
                        ("已连接", "设备复制的此类内容暂不支持".into())
                    }
                };
                let _ = events.send_blocking(TrayEvent::SessionStatus(target, status.0, status.1));
            });
            if connected.get() {
                mi_clip::share_queue::set_connected(target, false);
            }
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if result.is_err() {
                for event in analytics.borrow_mut().failed() {
                    mi_clip::analytics::capture(event);
                }
            }
            let _ = events.try_send(TrayEvent::SessionFile(
                target,
                mi_clip::file_feedback::Event::Failed(
                    mi_clip::file_feedback::Failure::Disconnected,
                ),
            ));
            if connected.get() {
                backoff = 2;
            }
            last_error = match result {
                Err(error) => {
                    let detail = format!("{}失败：{error:#}", phase.get());
                    if let Some(target) = attempted.get() {
                        mi_clip::share_queue::fail_pending(target, &detail);
                    }
                    let timestamp = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    eprintln!("[{timestamp}] Mi Clip: {detail}; retry in {backoff}s");
                    detail
                }
                Ok(()) => "连接已结束".into(),
            };
            let _ = events.send_blocking(TrayEvent::SessionStatus(
                target,
                "等待重连",
                format!("{last_error} · {backoff} 秒后重试"),
            ));
            for _ in 0..backoff * 4 {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            backoff = (backoff * 2).min(30);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_devices_receive_handshakes_before_either_authenticates() {
        let root = std::env::temp_dir().join(format!("miclip-multi-udp-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("session.json"), br#"{"sid":"miconnect","userId":"synthetic","cUserId":"test","ssecurity":"test","serviceToken":"test"}"#).unwrap();
        std::fs::write(
            root.join("identity.json"),
            br#"{"privateKey":"synthetic-unused","did":"AQIDBA"}"#,
        )
        .unwrap();
        let config = root.join("config.json");
        std::fs::write(&config, br#"{"session":"session.json","identity":"identity.json","account_certificate":"unused.pem"}"#).unwrap();
        let servers: Vec<_> = (0..2)
            .map(|_| {
                let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                socket
            })
            .collect();
        let (events, _receiver) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = servers
            .iter()
            .map(|server| {
                start_session(
                    config.clone(),
                    server.local_addr().unwrap(),
                    events.clone(),
                    stop.clone(),
                )
            })
            .collect();
        let mut clients = Vec::new();
        for server in &servers {
            let mut packet = [0; 2048];
            let (size, client) = server.recv_from(&mut packet).unwrap();
            assert!(size > 24);
            assert_eq!(packet[4], 81);
            clients.push(client);
        }
        stop.store(true, Ordering::Relaxed);
        for worker in workers {
            worker.join().unwrap();
        }
        assert_ne!(
            clients[0], clients[1],
            "each device must own its UDP socket"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn multiple_sessions_report_and_retry_independently() {
        let (events, receiver) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let missing = std::env::temp_dir()
            .join(format!("mi-clip-multi-missing-{}", std::process::id()))
            .join("config.json");
        let targets = [
            "192.0.2.1:1234".parse().unwrap(),
            "192.0.2.2:1234".parse().unwrap(),
        ];
        let workers: Vec<_> = targets
            .iter()
            .map(|target| start_session(missing.clone(), *target, events.clone(), stop.clone()))
            .collect();
        let mut retries = std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while retries.len() < 2 && std::time::Instant::now() < deadline {
            if let Ok(TrayEvent::SessionStatus(target, "等待重连", detail)) = receiver.try_recv()
            {
                assert!(detail.contains("os error 2"));
                retries.insert(target);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(targets.iter().all(|target| retries.contains(target)));
    }

    #[test]
    fn missing_config_reports_the_error_and_keeps_it_during_retry() {
        let (events, receiver) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let missing = std::env::temp_dir()
            .join(format!("mi-clip-missing-{}", std::process::id()))
            .join("config.json");
        assert!(!missing.exists());
        start(missing, events, stop.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut failure = None;
        let mut retained = false;
        while std::time::Instant::now() < deadline {
            if let Ok(TrayEvent::Status(status, detail)) = receiver.try_recv() {
                if status == "等待重连" {
                    assert!(detail.contains("发现设备／读取配置失败"));
                    assert!(detail.contains("os error 2"));
                    assert!(detail.contains("2 秒后重试"));
                    failure = Some(detail);
                } else if status == "连接中" && failure.is_some() {
                    retained = detail.contains("os error 2");
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        assert!(failure.is_some(), "connection error was not reported");
        assert!(retained, "retry cleared the previous connection error");
    }
}
