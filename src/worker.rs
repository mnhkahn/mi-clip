use crate::tray::TrayEvent;
use async_channel::Sender;
use mi_clip::handshake::{self, SyncEvent};
use std::{
    net::SocketAddr,
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
    supervise(config, events, stop, || {
        let mut targets: Vec<_> = mi_clip::share_queue::discovered_devices()
            .into_iter()
            .map(|device| device.endpoint)
            .collect();
        targets.extend(mi_clip::share_queue::pending_targets());
        targets
    });
}

fn supervise(
    config: PathBuf,
    events: Sender<TrayEvent>,
    stop: Arc<AtomicBool>,
    candidates: impl Fn() -> Vec<SocketAddr> + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut sessions: std::collections::HashMap<
            _,
            (std::thread::JoinHandle<()>, Arc<AtomicBool>),
        > = std::collections::HashMap::new();
        let _ = events.try_send(TrayEvent::Status("等待设备", "设备上线后将自动连接".into()));
        let mut config_available = true;
        while !stop.load(Ordering::Relaxed) {
            match handshake::validate_config(&config) {
                Ok(()) if !config_available => {
                    config_available = true;
                    let _ = events
                        .try_send(TrayEvent::Status("等待设备", "设备上线后将自动连接".into()));
                }
                Ok(()) => (),
                Err(error) => {
                    if config_available {
                        config_available = false;
                        eprintln!("Mi Clip account configuration unavailable: {error:#}");
                        let _ = events
                            .try_send(TrayEvent::Status("尚未登录", "请检查小米账号配置".into()));
                    }
                    std::thread::sleep(Duration::from_millis(250));
                    continue;
                }
            }
            let mut targets = candidates();
            targets.retain(|target| !mi_clip::discovery::is_local_address(*target));
            sessions.retain(|target, (handle, session_stop)| {
                if !targets.contains(target) && !mi_clip::share_queue::is_connected(*target) {
                    session_stop.store(true, Ordering::Relaxed);
                }
                if handle.is_finished() {
                    let _ = events.send_blocking(TrayEvent::SessionStatus(
                        *target,
                        "未发现",
                        String::new(),
                    ));
                    false
                } else {
                    true
                }
            });
            for target in targets {
                sessions.entry(target).or_insert_with(|| {
                    let session_stop = Arc::new(AtomicBool::new(false));
                    let handle =
                        start_session(config.clone(), target, events.clone(), session_stop.clone());
                    (handle, session_stop)
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        for (_, (_, session_stop)) in &sessions {
            session_stop.store(true, Ordering::Relaxed);
        }
        for (_, (session, _)) in sessions {
            let _ = session.join();
        }
    })
}

fn start_session(
    config: PathBuf,
    target: std::net::SocketAddr,
    events: Sender<TrayEvent>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut backoff = 2;
        let mut waiting = false;
        let analytics = std::cell::RefCell::new(mi_clip::analytics::Session::default());
        while !stop.load(Ordering::Relaxed) {
            analytics.borrow_mut().begin();
            if !waiting {
                let _ =
                    events.send_blocking(TrayEvent::SessionStatus(target, "连接中", String::new()));
            }
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
                        return;
                    }
                    SyncEvent::Authenticating => {
                        phase.set("设备认证");
                        ("认证中", String::new())
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
            match result {
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
                }
                Ok(()) => (),
            };
            waiting = true;
            let _ = events.send_blocking(TrayEvent::SessionStatus(
                target,
                "等待设备",
                "暂未连接 · 设备可用时自动连接".into(),
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
            if let Ok(TrayEvent::SessionStatus(target, "等待设备", detail)) = receiver.try_recv()
            {
                assert!(detail.contains("设备可用时自动连接"));
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
    fn ignores_saved_target_and_retires_withdrawn_discovery() {
        let root = std::env::temp_dir().join(format!("miclip-withdrawn-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let config = root.join("config.json");
        std::fs::write(&config, br#"{"endpoint":"192.0.2.1:1234","session":"absent.json","identity":"absent.json","account_certificate":"unused.pem"}"#).unwrap();
        let target: SocketAddr = "192.0.2.2:1234".parse().unwrap();
        let advertised = Arc::new(AtomicBool::new(false));
        let discovery = advertised.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (events, receiver) = async_channel::unbounded();
        let (scanned, scans) = std::sync::mpsc::channel();
        let supervisor = supervise(config, events, stop.clone(), move || {
            let _ = scanned.send(());
            discovery
                .load(Ordering::Relaxed)
                .then_some(target)
                .into_iter()
                .collect()
        });
        let session_status = |expected| {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while std::time::Instant::now() < deadline {
                if let Ok(TrayEvent::SessionStatus(address, status, _)) = receiver.try_recv() {
                    assert_eq!(address, target, "a saved address must not become a session");
                    if status == expected {
                        return true;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        };
        scans.recv_timeout(Duration::from_secs(3)).unwrap();
        scans.recv_timeout(Duration::from_secs(3)).unwrap();
        advertised.store(true, Ordering::Relaxed);
        assert!(session_status("等待设备"));
        advertised.store(false, Ordering::Relaxed);
        assert!(session_status("未发现"));
        stop.store(true, Ordering::Relaxed);
        supervisor.join().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_config_prompts_for_account_instead_of_connection_errors() {
        let (events, receiver) = async_channel::unbounded();
        let stop = Arc::new(AtomicBool::new(false));
        let missing = std::env::temp_dir()
            .join(format!("mi-clip-missing-{}", std::process::id()))
            .join("config.json");
        assert!(!missing.exists());
        start(missing, events, stop.clone());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut prompted = false;
        while std::time::Instant::now() < deadline {
            if let Ok(TrayEvent::Status(status, detail)) = receiver.try_recv() {
                assert_ne!(status, "连接中");
                assert!(!detail.contains("重试"));
                if status == "尚未登录" {
                    assert_eq!(detail, "请检查小米账号配置");
                    prompted = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.store(true, Ordering::Relaxed);
        assert!(prompted, "missing account configuration was not reported");
    }
}
