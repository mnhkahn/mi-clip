//! Deliver notifications from the app itself so macOS uses its bundled icon.
#[cfg(feature = "ui")]
unsafe extern "C" {
    fn clip_notification_deliver(saved: bool);
}

pub fn deliver(saved: bool) {
    #[cfg(feature = "ui")]
    unsafe {
        clip_notification_deliver(saved);
    }
    // Research command-line binaries have no app bundle or notification identity.
    #[cfg(not(feature = "ui"))]
    let _ = saved;
}
