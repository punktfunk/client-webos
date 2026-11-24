//! A wired `DualSense`'s motion sensors, read straight off its hidraw node.
//!
//! `hid-playstation` normally publishes the gyro and accelerometer as an evdev node of their own
//! ("… Motion Sensors"), which [`super::evdev`] claims and forwards. Some TV kernels publish only
//! the pad node: an LG C1 (webOS 6.5, kernel 4.4) lists a lone `DualSense` entry in
//! `/proc/bus/input/devices`, so gyro aiming never reached the host (`rich=0`). The samples are
//! still in every USB input report, and hidraw hands each report to every reader that has the node
//! open, so reading it here takes nothing from SDL or the kernel's own input path.

use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use punktfunk_core::quic::RichInput;

/// The USB input report's id, and the length the pad sends it at.
const INPUT_REPORT_ID: u8 = 0x01;
const INPUT_REPORT_LEN: usize = 64;

/// Offset of the first sensor axis in report `0x01`, report id included: three gyro then three
/// accelerometer axes, little-endian `i16`. These are the pad's raw report units — the scale the
/// host's virtual pad replays as it is, so they cross the wire unscaled.
const SENSORS_AT: usize = 16;

/// Shortest gap between two sends. A pad on a 1000 Hz dongle reports every millisecond; half
/// that rate is what the evdev route sends at, and a datagram per report would buy nothing.
const MIN_INTERVAL: Duration = Duration::from_millis(2);

/// How long one `poll` waits, which bounds how late the reader notices its stop flag.
const POLL_MS: libc::c_int = 100;

/// Whether the kernel publishes a motion node, which the evdev reader then forwards. Reading the
/// sensors here as well would send every sample twice. An unreadable file reads as "no".
pub fn has_motion_node() -> bool {
    std::fs::read_to_string("/proc/bus/input/devices").is_ok_and(|devices| devices.contains("Motion Sensors"))
}

/// One sample — `(gyro, accel)` — out of a USB input report, or `None` for any other report.
pub fn parse(report: &[u8]) -> Option<([i16; 3], [i16; 3])> {
    if report.len() < SENSORS_AT + 12 || report[0] != INPUT_REPORT_ID {
        return None;
    }
    let axis = |i: usize| i16::from_le_bytes([report[SENSORS_AT + 2 * i], report[SENSORS_AT + 2 * i + 1]]);
    Some(([axis(0), axis(1), axis(2)], [axis(3), axis(4), axis(5)]))
}

/// A read-only descriptor, closed on drop.
struct Fd(libc::c_int);

impl Fd {
    fn open(path: &str) -> Option<Self> {
        let c_path = CString::new(path).ok()?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        (fd >= 0).then_some(Self(fd))
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from `open` and is closed exactly once.
        unsafe { libc::close(self.0) };
    }
}

/// The thread that forwards one pad's motion samples. Dropping it stops and joins the thread.
pub struct Reader {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Reader {
    /// Starts forwarding pad `pad`'s samples from the hidraw node at `path` through `send`, or
    /// `None` when the node cannot be opened for reading.
    pub fn spawn(path: &str, pad: u8, send: impl Fn(RichInput) + Send + 'static) -> Option<Self> {
        let fd = Fd::open(path)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("ds5-sensors".into())
            .spawn(move || read_loop(&fd, pad, &flag, &send))
            .ok()?;
        tracing::info!("pad {pad}: motion sensors read off the pad's hidraw node ({path})");
        Some(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Reads reports until `stop` or the node goes away, sending the newest sample when it moved.
fn read_loop(fd: &Fd, pad: u8, stop: &AtomicBool, send: &impl Fn(RichInput)) {
    let mut buf = [0u8; INPUT_REPORT_LEN];
    let mut pending = None;
    let mut sent = None;
    let mut sent_at: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd {
            fd: fd.0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a live local and the count matches.
        let rc = unsafe { libc::poll(&raw mut pfd, 1, POLL_MS) };
        if rc < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            break;
        }
        if !drain(fd, &mut buf, &mut pending) {
            break;
        }
        let Some(sample) = pending else { continue };
        if sent == Some(sample) {
            pending = None;
            continue;
        }
        if sent_at.is_some_and(|at| at.elapsed() < MIN_INTERVAL) {
            continue;
        }
        let (gyro, accel) = sample;
        send(RichInput::Motion { pad, gyro, accel });
        sent = Some(sample);
        sent_at = Some(Instant::now());
        pending = None;
    }
    tracing::info!("pad {pad}: hidraw motion reader stopped");
}

/// Reads every queued report, keeping the newest sample in `pending`. `false` once the node is
/// gone (an unplugged pad reads as an error other than "try again").
fn drain(fd: &Fd, buf: &mut [u8; INPUT_REPORT_LEN], pending: &mut Option<([i16; 3], [i16; 3])>) -> bool {
    loop {
        // SAFETY: `buf` is a live local of the length passed; the descriptor is open.
        let n = unsafe { libc::read(fd.0, buf.as_mut_ptr().cast(), buf.len()) };
        match usize::try_from(n) {
            Ok(0) => return false,
            Ok(len) => {
                if let Some(sample) = parse(&buf[..len]) {
                    *pending = Some(sample);
                }
            }
            Err(_) => {
                let err = std::io::Error::last_os_error();
                return matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{parse, INPUT_REPORT_LEN, SENSORS_AT};

    #[test]
    fn a_usb_input_report_yields_its_gyro_then_accel() {
        let mut report = [0u8; INPUT_REPORT_LEN];
        report[0] = 0x01;
        let axes: [i16; 6] = [1, -2, 300, -8192, 8192, -32768];
        for (i, value) in axes.iter().enumerate() {
            report[SENSORS_AT + 2 * i..SENSORS_AT + 2 * i + 2].copy_from_slice(&value.to_le_bytes());
        }
        assert_eq!(parse(&report), Some(([1, -2, 300], [-8192, 8192, -32768])));
    }

    #[test]
    fn other_reports_and_short_reads_are_ignored() {
        let mut report = [0u8; INPUT_REPORT_LEN];
        report[0] = 0x31;
        assert_eq!(parse(&report), None, "a Bluetooth-framed report is not this layout");
        report[0] = 0x01;
        assert_eq!(parse(&report[..SENSORS_AT + 11]), None);
    }
}
