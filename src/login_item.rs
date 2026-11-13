//! Register the containing app for launch at login on normal startup.
unsafe extern "C" {
    fn clip_login_item_ensure_enabled();
}

pub fn ensure_enabled() {
    // SAFETY: Called from the normal AppKit startup callback on the main thread.
    unsafe { clip_login_item_ensure_enabled() };
}
