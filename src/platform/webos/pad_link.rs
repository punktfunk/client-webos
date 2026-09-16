//! Holds every Bluetooth gamepad's link out of sniff mode for the length of a stream.
//!
//! In sniff, the TV's Bluetooth stack delivers a pad's input reports in batches at its ~77.5 ms
//! anchor: every press, stick and gyro change waits for the next batch, and a tap shorter than one
//! batch arrives as press and release together, which the pad-state snapshot folds into "released".
//! The stack slides back into sniff on its own within seconds of a `stopSniff`, so one call at
//! session start does not hold. This thread re-asserts on a short timer, and at once when the pad
//! reader measures a sniff-sized gap ([`SNIFF_SUSPECT`]); the TV's own policy comes back at the end.
//!
//! Every Bluetooth pad, whatever it is: the batching is the link's, not the controller's. A wired pad
//! is not on `bluetooth2` and is never listed ([`bluetooth_pads`]).
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::ls2;

/// Raised by the pad reader when two consecutive reports sit further apart than an awake link
/// allows — see `evdev::pad`. Taken by the keeper thread.
pub static SNIFF_SUSPECT: AtomicBool = AtomicBool::new(false);

const STOP_SNIFF_URI: &str = "luna://com.webos.service.bluetooth2/device/internal/stopSniff";
const START_SNIFF_URI: &str = "luna://com.webos.service.bluetooth2/device/internal/startSniff";

/// Bounds a slip on a pad with no motion node, which nothing else can see. A call replies in 1–3 ms.
const REASSERT: Duration = Duration::from_millis(250);
/// One call for a run of late reports, not one per report.
const SUSPECT_FLOOR: Duration = Duration::from_millis(200);
/// Pads attach and leave mid-session.
const RESCAN: Duration = Duration::from_secs(2);
/// How long a sniff-sized gap can wait for the thread to notice it.
const TICK: Duration = Duration::from_millis(20);

/// The keeper thread; stops, gives the links back to the TV's policy and joins on drop.
pub struct PadLink {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl PadLink {
    pub fn start() -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        match std::thread::Builder::new()
            .name("pf-pad-link".into())
            .spawn(move || run(&flag))
        {
            Ok(thread) => Some(Self {
                stop,
                thread: Some(thread),
            }),
            Err(e) => {
                tracing::warn!("pad link keeper did not start: {e}");
                None
            }
        }
    }
}

impl Drop for PadLink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(stop: &AtomicBool) {
    let bus = match ls2::Bus::open() {
        Ok(bus) => bus,
        Err(e) => {
            tracing::info!("pad link: no Luna bus ({e:#}); Bluetooth pads keep the TV's sniff policy");
            return;
        }
    };
    let mut pads: Vec<String> = Vec::new();
    let mut scanned: Option<Instant> = None;
    let mut asserted: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if scanned.is_none_or(|t| now.duration_since(t) >= RESCAN) {
            scanned = Some(now);
            let found = std::fs::read_to_string("/proc/bus/input/devices")
                .map(|devices| bluetooth_pads(&devices))
                .unwrap_or_default();
            if found != pads {
                tracing::info!("pad link: holding {found:?} out of Bluetooth sniff");
                asserted = None;
                pads = found;
            }
        }
        let suspect =
            SNIFF_SUSPECT.load(Ordering::Relaxed) && asserted.is_none_or(|t| now.duration_since(t) >= SUSPECT_FLOOR);
        if !pads.is_empty() && (suspect || asserted.is_none_or(|t| now.duration_since(t) >= REASSERT)) {
            SNIFF_SUSPECT.store(false, Ordering::Relaxed);
            asserted = Some(now);
            for address in &pads {
                let _ = bus.call(
                    STOP_SNIFF_URI,
                    &format!("{{\"address\":\"{address}\"}}"),
                    ls2::Call::StopSniff,
                );
            }
        }
        bus.pump();
        std::thread::sleep(TICK);
    }
    // `startSniff` wants HCI Sniff Mode's own parameters, not the address alone (a bare address is
    // refused with a schema error). Slots are 0.625 ms: 96–124 is the TV's own ~77 ms policy, which
    // the pad still drives the menus under.
    for address in &pads {
        let payload =
            format!("{{\"address\":\"{address}\",\"minInterval\":96,\"maxInterval\":124,\"attempt\":4,\"timeout\":1}}");
        let _ = bus.call(START_SNIFF_URI, &payload, ls2::Call::StartSniff);
    }
}

/// Bluetooth addresses of the attached gamepads, from `/proc/bus/input/devices`.
///
/// A gamepad is a node with a joystick handler (`jsN`); that also skips a pad's touchpad and motion
/// nodes, which share its address. `I: Bus=0005` is Bluetooth — a wired pad publishes its MAC in
/// `U: Uniq=` too, so the address alone would claim a link that is not there.
pub fn bluetooth_pads(devices: &str) -> Vec<String> {
    let mut out = Vec::new();
    for block in devices.split("\n\n") {
        let bluetooth = block.lines().any(|l| l.trim_start().starts_with("I: Bus=0005"));
        let joystick = block
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("H: Handlers="))
            .any(|h| h.split_whitespace().any(|n| n.starts_with("js")));
        let address = block
            .lines()
            .find_map(|l| l.trim_start().strip_prefix("U: Uniq="))
            .map(|u| u.trim().to_ascii_lowercase())
            .filter(|u| !u.is_empty());
        if let (true, true, Some(address)) = (bluetooth, joystick, address) {
            if !out.contains(&address) {
                out.push(address);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::bluetooth_pads;

    /// A Bluetooth `DualSense`'s gamepad node counts once, its motion node does not, a wired pad and
    /// the Magic Remote never do.
    #[test]
    fn only_bluetooth_joysticks_are_held() {
        let devices = "\
I: Bus=0005 Vendor=054c Product=0ce6 Version=8100
N: Name=\"DualSense Wireless Controller\"
U: Uniq=A0:AB:51:12:34:56
H: Handlers=event14 js0

I: Bus=0005 Vendor=054c Product=0ce6 Version=8100
N: Name=\"DualSense Wireless Controller Motion Sensors\"
U: Uniq=A0:AB:51:12:34:56
H: Handlers=event15

I: Bus=0003 Vendor=054c Product=0ce6 Version=8111
N: Name=\"DualSense Wireless Controller\"
U: Uniq=a0:ab:51:65:43:21
H: Handlers=event17 js1

I: Bus=0005 Vendor=045e Product=0b13 Version=0509
N: Name=\"Xbox Wireless Controller\"
U: Uniq=98:7a:14:00:11:22
H: Handlers=event18 js2

I: Bus=0005 Vendor=1ea4 Product=0007 Version=0001
N: Name=\"LGE M-RCU - Builtin [0]\"
U: Uniq=
H: Handlers=kbd event2
";
        assert_eq!(bluetooth_pads(devices), vec!["a0:ab:51:12:34:56", "98:7a:14:00:11:22"]);
    }
}
