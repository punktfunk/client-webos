pub mod audio;
pub mod cursor;
pub mod device;
pub(crate) mod dl;
pub mod dualsense;
pub mod evdev;
pub mod gamepad;
pub mod hdr_pattern;
pub mod hidraw;
pub mod input;
pub(crate) mod ioctl;
pub mod keyboard;
pub mod ls2;
pub mod luna;
pub mod mouse;
pub mod ndl;
pub mod pad_link;
pub(crate) mod proc_input;
pub mod sdl_webos;
pub mod usb_audio;

use std::time::{Duration, Instant};

/// Sleeps in 2 ms steps until `done` or `limit` elapses; `true` if `done` won. For waits on work
/// another thread finishes (NDL's callbacks, Luna replies) that offer nothing to block on.
pub(crate) fn poll_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    true
}
