//! Native capture UI runs on AppKit's main thread, sharing the app's identity.
unsafe extern "C" {
    fn clip_capture_start();
    fn clip_capture_open();
    fn clip_capture_settings();
    fn clip_capture_stop();
}
pub fn start() {
    unsafe { clip_capture_start() }
}
pub fn open() {
    unsafe { clip_capture_open() }
}
pub fn settings() {
    unsafe { clip_capture_settings() }
}
pub fn stop() {
    unsafe { clip_capture_stop() }
}
