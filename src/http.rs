//! Small HTTPS client over macOS system libcurl. No bundled TLS runtime.
use std::{
    ffi::{CString, c_char, c_int, c_long, c_uint, c_void},
    ptr,
    sync::OnceLock,
};
const LIMIT: usize = 1_048_576;
#[cfg(feature = "ui")]
unsafe extern "C" {
    fn clip_update_system_proxy(
        target: *const c_char,
        buffer: *mut c_char,
        capacity: isize,
    ) -> bool;
}
#[link(name = "curl")]
unsafe extern "C" {
    fn curl_global_init(flags: c_long) -> c_int;
    fn curl_easy_init() -> *mut c_void;
    fn curl_easy_cleanup(handle: *mut c_void);
    fn curl_easy_setopt(handle: *mut c_void, option: c_uint, ...) -> c_int;
    fn curl_easy_perform(handle: *mut c_void) -> c_int;
    fn curl_easy_getinfo(handle: *mut c_void, info: c_uint, ...) -> c_int;
    fn curl_slist_append(list: *mut c_void, value: *const c_char) -> *mut c_void;
    fn curl_slist_free_all(list: *mut c_void);
}
struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { curl_easy_cleanup(self.0) }
    }
}
struct Headers(*mut c_void);
impl Drop for Headers {
    fn drop(&mut self) {
        unsafe { curl_slist_free_all(self.0) }
    }
}
struct Capture {
    body: Vec<u8>,
    headers: Vec<u8>,
    body_limit: usize,
}
extern "C" fn body_callback(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
) -> usize {
    capture(data, size, count, context, false)
}
extern "C" fn header_callback(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
) -> usize {
    capture(data, size, count, context, true)
}
fn capture(
    data: *mut c_char,
    size: usize,
    count: usize,
    context: *mut c_void,
    header: bool,
) -> usize {
    let Some(length) = size.checked_mul(count) else {
        return 0;
    };
    if length == 0 {
        return 0;
    }
    if data.is_null() || context.is_null() || length > LIMIT {
        return 0;
    }
    // SAFETY: libcurl invokes callbacks synchronously, with `length` valid bytes
    // and the live, uniquely borrowed Capture installed by request().
    let state = unsafe { &mut *context.cast::<Capture>() };
    let output = if header {
        &mut state.headers
    } else {
        &mut state.body
    };
    let limit = if header { LIMIT } else { state.body_limit };
    if output.len().saturating_add(length) > limit {
        return 0;
    }
    output.extend_from_slice(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length) });
    length
}
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    headers: Vec<u8>,
}
impl Response {
    pub fn cookies(&self) -> Vec<(String, String)> {
        String::from_utf8_lossy(&self.headers)
            .lines()
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                if !name.eq_ignore_ascii_case("set-cookie") {
                    return None;
                }
                let cookie = value.trim().split(';').next()?;
                let (name, value) = cookie.split_once('=')?;
                Some((name.to_owned(), value.to_owned()))
            })
            .collect()
    }
}
pub fn request(
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
    timeout: u32,
) -> anyhow::Result<Response> {
    request_inner(url, body, headers, timeout, LIMIT, false)
}
pub(crate) fn download(url: &str, limit: usize) -> anyhow::Result<Response> {
    anyhow::ensure!(
        (1..=64 * 1024 * 1024).contains(&limit),
        "Invalid download size limit"
    );
    request_inner(
        url,
        None,
        &[("User-Agent", "MiClip-updater")],
        60,
        limit,
        true,
    )
}
fn request_inner(
    url: &str,
    body: Option<&[u8]>,
    headers: &[(&str, &str)],
    timeout: u32,
    limit: usize,
    redirects: bool,
) -> anyhow::Result<Response> {
    let parsed = url::Url::parse(url).map_err(|_| anyhow::anyhow!("Invalid HTTPS URL"))?;
    anyhow::ensure!(
        parsed.scheme() == "https"
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.port_or_known_default() == Some(443),
        "HTTPS required"
    );
    static INIT: OnceLock<c_int> = OnceLock::new();
    // SAFETY: OnceLock serializes process-wide initialization before any curl handle exists.
    anyhow::ensure!(
        *INIT.get_or_init(|| unsafe { curl_global_init(3) }) == 0,
        "HTTPS initialization failed"
    );
    let target = CString::new(url)?;
    #[cfg(feature = "ui")]
    let mut proxy = [0 as c_char; 1024];
    #[cfg(feature = "ui")]
    let use_system_proxy = redirects
        && !["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]
            .iter()
            .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()))
        // SAFETY: Swift reads the NUL-terminated target and writes within the buffer.
        && unsafe { clip_update_system_proxy(target.as_ptr(), proxy.as_mut_ptr(), proxy.len() as isize) };
    let mut capture = Capture {
        body: Vec::new(),
        headers: Vec::new(),
        body_limit: limit,
    };
    let mut list = Headers(ptr::null_mut());
    for (name, value) in headers {
        anyhow::ensure!(
            !name.is_empty() && !name.contains(['\r', '\n', ':']) && !value.contains(['\r', '\n']),
            "Invalid HTTP header"
        );
        let item = CString::new(format!("{name}: {value}"))?;
        // SAFETY: curl_slist_append copies the NUL-terminated header.
        let new = unsafe { curl_slist_append(list.0, item.as_ptr()) };
        anyhow::ensure!(!new.is_null(), "HTTP header allocation failed");
        list.0 = new;
    }
    let handle = Handle(unsafe { curl_easy_init() });
    anyhow::ensure!(!handle.0.is_null(), "HTTPS handle allocation failed");
    // SAFETY: Types below match curl.h option ABIs (long, pointer, callback).
    // Referenced C strings, body, header list and callback state outlive perform().
    unsafe {
        macro_rules! set {
            ($option:expr, $value:expr) => {
                anyhow::ensure!(
                    curl_easy_setopt(handle.0, $option, $value) == 0,
                    "HTTPS option rejected"
                );
            };
        }
        set!(10002, target.as_ptr());
        #[cfg(feature = "ui")]
        if use_system_proxy {
            set!(10004, proxy.as_ptr()); // CURLOPT_PROXY
        }
        set!(
            20011,
            body_callback as extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize
        );
        set!(
            20079,
            header_callback as extern "C" fn(*mut c_char, usize, usize, *mut c_void) -> usize
        );
        let state = (&mut capture as *mut Capture).cast::<c_void>();
        set!(10001, state);
        set!(10029, state);
        set!(10023, list.0);
        set!(13, c_long::from(timeout.clamp(1, 60)));
        set!(78, 5 as c_long);
        set!(99, 1 as c_long);
        set!(52, c_long::from(redirects));
        set!(68, 5 as c_long);
        set!(182, 2 as c_long); // Redirects may only use HTTPS.
        set!(64, 1 as c_long);
        set!(81, 2 as c_long);
        if let Some(bytes) = body {
            set!(47, 1 as c_long);
            set!(10015, bytes.as_ptr());
            set!(60, bytes.len() as c_long);
        }
        let code = curl_easy_perform(handle.0);
        anyhow::ensure!(
            code == 0,
            "HTTPS request failed (curl code {code}); secrets omitted"
        );
        let mut status = 0 as c_long;
        anyhow::ensure!(
            curl_easy_getinfo(handle.0, 0x200002, &mut status as *mut c_long) == 0,
            "HTTPS status unavailable"
        );
        Ok(Response {
            status: status as u16,
            body: capture.body,
            headers: capture.headers,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cookies_are_parsed_without_splitting_expiry_commas() {
        let r = Response { status: 200, body: vec![], headers: b"HTTP/1.1 200 OK\r\nSet-Cookie: one=a=b; Expires=Wed, 21 Oct 2030 07:28:00 GMT\r\nSet-Cookie: two=c; Secure\r\n".to_vec() };
        assert_eq!(
            r.cookies(),
            vec![("one".into(), "a=b".into()), ("two".into(), "c".into())]
        );
    }
    #[test]
    fn callback_rejects_oversized_and_overflowing_data() {
        let mut state = Capture {
            body: vec![],
            headers: vec![],
            body_limit: LIMIT,
        };
        let mut byte = 1_u8;
        let ptr = (&mut state as *mut Capture).cast();
        assert_eq!(
            body_callback((&mut byte as *mut u8).cast(), usize::MAX, 2, ptr),
            0
        );
        assert_eq!(
            body_callback((&mut byte as *mut u8).cast(), LIMIT + 1, 1, ptr),
            0
        );
    }
}
