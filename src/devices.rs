//! In-memory discovery for the local device list. Never persist or log peer metadata.
use crate::tray::TrayEvent;
use async_channel::Sender;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub(crate) struct Device {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub device_type: u8,
    pub addresses: Vec<SocketAddr>,
}
impl Device {
    pub fn kind(&self) -> &'static str {
        match self.device_type {
            1 => "手机",
            2 if self
                .name
                .split(|c: char| !c.is_alphanumeric())
                .any(|word| word.eq_ignore_ascii_case("pad")) =>
            {
                "平板电脑"
            }
            2 => "电脑",
            3 => "平板电脑",
            _ => "设备",
        }
    }
}
#[derive(Clone, Default)]
pub(crate) struct Devices {
    pub entries: Vec<Device>,
    pub scanning: bool,
    pub failed: bool,
    pub sessions: std::collections::HashMap<SocketAddr, (&'static str, String)>,
}
impl Devices {
    pub fn rows(&self, _status: &str) -> Vec<(String, String)> {
        let mut matched = std::collections::HashSet::new();
        let mut rows = Vec::new();
        for device in &self.entries {
            let session = device.addresses.iter().find_map(|a| {
                self.sessions.get(a).map(|state| {
                    matched.insert(*a);
                    state
                })
            });
            let state = session.map_or(
                "已发现 · 等待连接".to_owned(),
                |(status, detail)| session_label(status, detail),
            );
            let address = device
                .addresses
                .first()
                .map(ToString::to_string)
                .unwrap_or_else(|| {
                    format!("{}:{}", device.host.trim_end_matches('.'), device.port)
                });
            rows.push((
                format!("{} · {}", device.name, device.kind()),
                format!("{state} · {address}"),
            ));
        }
        for (target, (status, detail)) in &self.sessions {
            if !matched.contains(target) {
                rows.push((
                    "设备 · 当前连接目标".into(),
                    format!("{} · {target}", session_label(status, detail)),
                ));
            }
        }
        rows
    }
}

fn session_label(status: &str, detail: &str) -> String {
    match status {
        "已连接" => "已连接 · 认证通过".into(),
        "连接中" | "认证中" => status.into(),
        _ if !detail.is_empty() => format!("{status} · {detail}"),
        _ => "未连接 · 等待设备上线".into(),
    }
}

pub fn start(events: Sender<TrayEvent>, stop: Arc<AtomicBool>) -> mpsc::SyncSender<()> {
    let (request, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut next_scan = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            if Instant::now() < next_scan && !mi_clip::share_queue::take_device_refresh() {
                match receiver.recv_timeout(Duration::from_millis(250)) {
                    Ok(()) => (),
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            while receiver.try_recv().is_ok() {}
            let _ = events.send_blocking(TrayEvent::Discovering);
            let result = mi_clip::discovery::discover(Duration::from_secs(5))
                .map(|endpoints| {
                    let mut devices = Vec::new();
                    for endpoint in endpoints {
                        // Interfaces may produce duplicate records for one device.
                        if devices.iter().any(|d: &Device| {
                            d.host == endpoint.host && d.port == endpoint.kcp_port
                        }) {
                            continue;
                        }
                        let addresses: Vec<_> = endpoint.remote_addresses().collect();
                        if addresses.is_empty() {
                            continue;
                        }
                        let name = endpoint.model.filter(|s| !s.is_empty()).unwrap_or_else(|| {
                            endpoint.host.trim_end_matches(".local.").to_owned()
                        });
                        devices.push(Device {
                            name,
                            host: endpoint.host,
                            port: endpoint.kcp_port,
                            device_type: endpoint.device_type,
                            addresses,
                        });
                    }
                    devices.sort_by_key(|d| d.device_type != 1);
                    devices
                })
                .map_err(|_| ());
            let catalogue = result
                .as_ref()
                .map(|devices| {
                    devices
                        .iter()
                        .filter_map(|device| {
                            device
                                .addresses
                                .iter()
                                .find(|a| a.is_ipv4())
                                .or_else(|| device.addresses.first())
                                .map(|endpoint| mi_clip::share_queue::ShareDevice {
                                    name: device.name.clone(),
                                    device_type: device.device_type,
                                    endpoint: *endpoint,
                                })
                        })
                        .collect()
                })
                .unwrap_or_default();
            if let Err(error) = mi_clip::share_queue::publish_devices(catalogue) {
                eprintln!("Mi Clip share device list unavailable: {error:#}");
            }
            let _ = events.send_blocking(TrayEvent::Devices(result));
            next_scan = Instant::now() + Duration::from_secs(15);
        }
    });
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tablet_and_phone_keep_independent_connection_states() {
        let phone = "192.0.2.1:1234".parse().unwrap();
        let tablet = "192.0.2.2:1234".parse().unwrap();
        let mut devices = Devices::default();
        for (name, device_type, address) in [("手机", 1, phone), ("测试 Redmi Pad Pro", 2, tablet)]
        {
            devices.entries.push(Device {
                name: name.into(),
                host: "test.local.".into(),
                port: 1234,
                device_type,
                addresses: vec![address],
            });
            devices.sessions.insert(address, ("已连接", String::new()));
        }
        let rows = devices.rows("已连接");
        assert!(rows.iter().all(|row| row.1.contains("认证通过")));
        assert!(rows[1].0.ends_with("平板电脑"));
        devices
            .sessions
            .insert(tablet, ("等待重连", "认证失败".into()));
        let rows = devices.rows("已连接");
        assert!(rows[0].1.contains("认证通过"));
        assert!(rows[1].1.contains("认证失败"));
        devices.entries.clear();
        assert_eq!(devices.rows("已连接").len(), 2);
    }

    #[test]
    fn only_matching_endpoint_is_authenticated_and_session_survives_missing_advertisement() {
        let address: SocketAddr = "192.0.2.1:1234".parse().unwrap();
        let mut devices = Devices {
            sessions: [(address, ("已连接", String::new()))].into(),
            ..Default::default()
        };
        assert!(devices.rows("已连接")[0].1.contains("认证通过"));
        devices.entries.push(Device {
            name: "测试手机".into(),
            host: "phone.local.".into(),
            port: 1234,
            device_type: 1,
            addresses: vec![address],
        });
        devices.entries.push(Device {
            name: "另一手机".into(),
            host: "other.local.".into(),
            port: 1234,
            device_type: 1,
            addresses: vec!["192.0.2.2:1234".parse().unwrap()],
        });
        let rows = devices.rows("已连接");
        assert_eq!(rows.len(), 2);
        assert!(rows[0].1.contains("认证通过"));
        assert!(rows[1].1.contains("等待连接"));
        devices
            .sessions
            .insert(address, ("等待重连", String::new()));
        assert!(devices.rows("等待重连")[0].1.contains("等待设备上线"));
    }
}
