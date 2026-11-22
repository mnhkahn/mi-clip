//! Start and present the native clipboard history on AppKit's main thread.
unsafe extern "C" {
    fn clip_history_start();
    fn clip_history_open();
    fn clip_history_stop();
}

pub fn start() {
    unsafe { clip_history_start() }
}

pub fn open() {
    unsafe { clip_history_open() }
}

pub fn stop() {
    unsafe { clip_history_stop() }
}
