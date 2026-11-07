//! macOS DNS-SD discovery; no vendor runtime or subprocess dependency.
use crate::advertisement::Advertisement;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;
use std::{
    collections::HashSet,
    ffi::{CStr, c_char, c_void},
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    time::{Duration, Instant},
};
type Service = *mut c_void;
type BrowseCallback = unsafe extern "C" fn(
    Service,
    u32,
    u32,
    i32,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);
type ResolveCallback = unsafe extern "C" fn(
    Service,
    u32,
    u32,
    i32,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
#[link(name = "System")]
unsafe extern "C" {
    fn DNSServiceBrowse(
        out: *mut Service,
        flags: u32,
        interface: u32,
        kind: *const c_char,
        domain: *const c_char,
        callback: BrowseCallback,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceResolve(
        out: *mut Service,
        flags: u32,
        interface: u32,
        name: *const c_char,
        kind: *const c_char,
        domain: *const c_char,
        callback: ResolveCallback,
        context: *mut c_void,
    ) -> i32;
    fn DNSServiceRefSockFD(service: Service) -> i32;
    fn DNSServiceProcessResult(service: Service) -> i32;
    fn DNSServiceRefDeallocate(service: Service);
    fn TXTRecordGetValuePtr(
        size: u16,
        txt: *const c_void,
        key: *const c_char,
        length: *mut u8,
    ) -> *const c_void;
    fn poll(fds: *mut PollFd, count: u32, timeout: i32) -> i32;
}
#[derive(Serialize)]
pub struct Endpoint {
    pub host: String,
    pub interface: u32,
    pub kcp_port: u16,
    pub device_type: u8,
    pub model: Option<String>,
}
impl Endpoint {
    pub fn remote_addresses(&self) -> impl Iterator<Item = SocketAddr> {
        (self.host.as_str(), self.kcp_port)
            .to_socket_addrs()
            .into_iter()
            .flatten()
            .filter(|address| !is_local_address(*address))
    }
}
#[derive(Default)]
struct State {
    services: Vec<Service>,
    seen: HashSet<(u32, Vec<u8>)>,
    endpoints: Vec<Endpoint>,
    failed: bool,
}
impl Drop for State {
    fn drop(&mut self) {
        for &service in &self.services {
            unsafe {
                DNSServiceRefDeallocate(service);
            }
        }
    }
}
unsafe extern "C" fn resolved(
    _: Service,
    _: u32,
    interface: u32,
    error: i32,
    _: *const c_char,
    host: *const c_char,
    _: u16,
    size: u16,
    txt: *const u8,
    context: *mut c_void,
) {
    if error != 0 || host.is_null() || txt.is_null() {
        return;
    }
    let state = unsafe { &mut *(context as *mut State) };
    let mut length = 0;
    let value = unsafe { TXTRecordGetValuePtr(size, txt.cast(), c"AppData".as_ptr(), &mut length) };
    if value.is_null() || length == 0 {
        return;
    }
    let encoded = unsafe { std::slice::from_raw_parts(value.cast::<u8>(), length as usize) };
    let Ok(bytes) = STANDARD.decode(encoded) else {
        return;
    };
    let Ok(ad) = Advertisement::parse(&bytes) else {
        return;
    };
    let Some(port) = ad.kcp_port.filter(|&p| p != 0) else {
        return;
    };
    let Ok(host) = (unsafe { CStr::from_ptr(host) }).to_str() else {
        return;
    };
    if host.len() > 255 || state.endpoints.len() >= 64 {
        return;
    }
    if !state
        .endpoints
        .iter()
        .any(|e| e.host == host && e.kcp_port == port && e.interface == interface)
    {
        state.endpoints.push(Endpoint {
            host: host.into(),
            interface,
            kcp_port: port,
            device_type: ad.device_type,
            model: ad
                .model
                .filter(|s| !s.chars().any(char::is_control))
                .map(str::to_owned),
        });
    }
}
unsafe extern "C" fn browsed(
    _: Service,
    flags: u32,
    interface: u32,
    error: i32,
    name: *const c_char,
    kind: *const c_char,
    domain: *const c_char,
    context: *mut c_void,
) {
    if error != 0 || flags & 2 == 0 || name.is_null() || kind.is_null() || domain.is_null() {
        return;
    }
    let state = unsafe { &mut *(context as *mut State) };
    if state.services.len() >= 65
        || !state.seen.insert((
            interface,
            (unsafe { CStr::from_ptr(name) }).to_bytes().to_vec(),
        ))
    {
        return;
    }
    let mut service = std::ptr::null_mut();
    // DNS-SD delivers callbacks only through ProcessResult, after this borrow ends.
    if unsafe {
        DNSServiceResolve(
            &mut service,
            0,
            interface,
            name,
            kind,
            domain,
            resolved,
            context,
        )
    } == 0
    {
        state.services.push(service);
    }
}
/// Binding an ephemeral socket succeeds only for an address owned by this Mac.
/// Keep the IPv6 scope ID while probing link-local interface addresses.
pub fn is_local_address(mut address: SocketAddr) -> bool {
    if address.ip().is_loopback() || address.ip().is_unspecified() {
        return true;
    }
    address.set_port(0);
    UdpSocket::bind(address).is_ok()
}

pub fn discover(timeout: Duration) -> anyhow::Result<Vec<Endpoint>> {
    anyhow::ensure!(
        timeout <= Duration::from_secs(30),
        "Discovery timeout exceeds limit"
    );
    let mut state = Box::<State>::default();
    let context = (&mut *state as *mut State).cast();
    let mut service = std::ptr::null_mut();
    let error = unsafe {
        DNSServiceBrowse(
            &mut service,
            0,
            0,
            c"_lyra-mdns._udp".as_ptr(),
            c"local.".as_ptr(),
            browsed,
            context,
        )
    };
    anyhow::ensure!(error == 0, "System DNS-SD browse failed ({error})");
    state.services.push(service);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let refs = state.services.clone();
        let mut fds: Vec<PollFd> = refs
            .iter()
            .map(|&s| PollFd {
                fd: unsafe { DNSServiceRefSockFD(s) },
                events: 1,
                revents: 0,
            })
            .collect();
        anyhow::ensure!(
            fds.iter().all(|p| p.fd >= 0),
            "Invalid system DNS-SD socket"
        );
        let result = unsafe {
            poll(
                fds.as_mut_ptr(),
                fds.len() as u32,
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(200) as i32,
            )
        };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            anyhow::bail!("DNS-SD socket polling failed");
        }
        for (fd, service) in fds.iter().zip(refs) {
            anyhow::ensure!(fd.revents & (8 | 16 | 32) == 0, "DNS-SD socket closed");
            if fd.revents & 1 != 0 && unsafe { DNSServiceProcessResult(service) } != 0 {
                state.failed = true;
            }
        }
        anyhow::ensure!(!state.failed, "System DNS-SD response failed");
    }
    Ok(std::mem::take(&mut state.endpoints))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn excludes_this_mac_without_excluding_other_computers() {
        for address in ["127.0.0.1:1234", "[::1]:1234", "0.0.0.0:1234"] {
            assert!(is_local_address(address.parse().unwrap()));
        }
        assert!(!is_local_address("192.0.2.1:1234".parse().unwrap()));
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        if socket.connect("192.0.2.1:1234").is_ok() {
            assert!(is_local_address(socket.local_addr().unwrap()));
        }
    }
}
