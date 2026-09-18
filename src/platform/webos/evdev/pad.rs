//! `PlayStation` pad nodes: the touchpad and the motion sensors.
//!
//! The kernel's `hid-playstation` publishes a `DualSense` as three evdev nodes — the pad itself,
//! its touchpad and its motion sensors. SDL reads only the first here (see `input::mute_unused_events`),
//! so the other two reach the
//! host's virtual pad only if this reader claims them: [`RichInput::Touchpad`] contacts for a
//! game's swipes, [`RichInput::Motion`] samples for gyro aiming. Claiming is not optional either
//! way — both advertise absolute axes the compositor would otherwise turn into a second cursor,
//! the touchpad with a stuck left button (see [`is_pad_touchpad`]).
//!
//! Everything here is pad-shaped; the generic mouse/keyboard reader is [`super`].
use std::os::unix::io::RawFd;

use punktfunk_core::quic::RichInput;

use super::{abs_range, abs_resolution, bit, device_vendor, InputEventRaw, ABS_X, EV_ABS, EV_SYN, SYN_REPORT};

/// A claimed pad node, decoded. Never both at once — they are separate nodes — which is why this
/// is one enum on [`super::Device`] rather than two `Option`s that must not overlap.
pub(super) enum Pad {
    Touchpad(Touchpad),
    Motion(Sensors),
}

impl Pad {
    /// Claims `fd` if it is one of the two pad nodes, from its capability bits alone. `None`
    /// leaves the node to the generic probe in [`super::open_hid`].
    pub(super) fn probe(fd: RawFd, abs: &[u8; 128], key: &[u8; 128], absolute_pointer: bool) -> Option<Self> {
        let vendor = || device_vendor(fd);
        if is_pad_touchpad(absolute_pointer, bit(key, BTN_TOUCH), vendor) {
            Some(Self::Touchpad(Touchpad::new(fd)))
        } else if is_pad_motion(abs, bit(key, BTN_TOUCH) || bit(key, BTN_SOUTH), vendor) {
            Some(Self::Motion(Sensors::new(fd)))
        } else {
            None
        }
    }

    /// Decodes one read burst. A pad node shares none of the mouse/keyboard decode — different
    /// axes, different wire plane — so it takes the whole burst on its own path.
    pub(super) fn read(&mut self, buf: &[u8], size: usize, sink: &impl Fn(RichInput)) {
        match self {
            Self::Touchpad(pad) => read_touch(pad, buf, size, sink),
            Self::Motion(sensors) => read_sensors(sensors, buf, size),
        }
    }

    /// Sends what the burst accumulated. Touch contacts already went out per `SYN_REPORT` (a
    /// gesture is a stream of positions, not one level), so only motion has anything pending.
    pub(super) fn flush(&mut self, sink: &impl Fn(RichInput)) {
        if let Self::Motion(sensors) = self {
            flush_sensors(sensors, sink);
        }
    }

    /// Lifts every contact the host still believes is down. A touch is a level, not an edge: a
    /// pad unplugged (or a reader stopped) mid-gesture would otherwise leave the finger down on
    /// the host's virtual pad with nothing left to release it. Motion needs no equivalent — a
    /// dropped sample is an attitude the host keeps, not a stuck input.
    pub(super) fn release(&mut self, sink: &impl Fn(RichInput)) {
        let Self::Touchpad(pad) = self else {
            return;
        };
        let (x_range, y_range) = (pad.x_range, pad.y_range);
        for (i, finger) in pad.fingers.iter_mut().enumerate() {
            if !std::mem::take(&mut finger.sent) {
                continue;
            }
            finger.active = false;
            finger.dirty = false;
            if let Some(contact) = contact(i, finger, x_range, y_range) {
                sink(contact);
            }
        }
    }

    /// How the node is named in the open log.
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::Touchpad(_) => "pad touchpad",
            Self::Motion(_) => "pad motion",
        }
    }
}

/// The motion node's six axes, contiguous and in the `DualSense` report's own order: `ABS_X`..`_Z`
/// accelerometer, `ABS_RX`..`_RZ` gyro (pitch/yaw/roll).
const ABS_Z: u16 = 0x02;
const ABS_RX: u16 = 0x03;
const ABS_RZ: u16 = 0x05;
/// Multitouch protocol B: which contact the following `ABS_MT_*` values belong to.
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
/// `-1` lifts the slot's contact; anything else is a new or continuing one.
const ABS_MT_TRACKING_ID: u16 = 0x39;

/// Finger-on-surface, which only touch-class nodes carry — the pad's own node reports
/// `BTN_SOUTH`/`BTN_EAST` and never this. See [`is_pad_touchpad`].
const BTN_TOUCH: u16 = 0x14a;

/// The pad's own south face button — what parts the gamepad node from the motion node, which
/// advertises the same six absolute axes but no buttons at all. See [`is_pad_motion`].
const BTN_SOUTH: u16 = 0x130;

/// Sony's USB/BT vendor id. `hid-playstation` binds only Sony pads, and it's the split-node
/// layout that creates the touchpad node this identifies.
const VENDOR_SONY: u16 = 0x054c;

/// Motion decode state. Sensor readings are levels, not deltas, so a read burst collapses to its
/// newest sample and an unchanged sample is worth no datagram at all — a resting pad reports at
/// its full rate forever.
#[derive(Default)]
pub(super) struct Sensors {
    /// Pitch/yaw/roll then accelerometer, in the node's own units, as the wire orders them.
    axes: [i32; 6],
    /// A `SYN_REPORT` completed a sample since the last send.
    dirty: bool,
    /// Last sample actually sent, for the unchanged-sample skip.
    sent: Option<[i16; 6]>,
    /// Per-axis node units → wire units, `SCALE_SHIFT` fixed point — see [`Sensors::new`].
    scale: [i64; 6],
    /// Kernel time of the last report, µs — see [`Sensors::note_report`].
    last_report_us: Option<i64>,
}

/// Two consecutive reports further apart than an awake link allows: a `DualSense` motion node
/// reports every ~2.5 ms, and Bluetooth sniff's anchor shows as gaps of 77.5 ms and multiples.
const SNIFF_GAP_US: i64 = 50_000;

/// Multitouch decode state for a claimed pad touchpad, plus the axis ranges its coordinates are
/// normalized against.
///
/// Protocol B (`ABS_MT_SLOT` + `ABS_MT_TRACKING_ID`), which is what `hid-playstation` reports and
/// all the wire has room for: two contacts, matching the `finger` field's `0`/`1`.
pub(super) struct Touchpad {
    /// The slot `ABS_MT_*` values currently apply to. Out-of-range slots are parked on the last
    /// finger rather than dropped, so a driver reporting more contacts than the wire carries
    /// can't silently write past the array.
    slot: usize,
    fingers: [Finger; 2],
    /// `(min, max)` of `ABS_MT_POSITION_X` / `_Y`, for the 0..=65535 normalization.
    x_range: (i32, i32),
    y_range: (i32, i32),
}

#[derive(Clone, Copy, Default)]
struct Finger {
    active: bool,
    /// The host currently believes this contact is down. Gates the lift: a node claimed with a
    /// finger already on it reports coordinates without a `ABS_MT_TRACKING_ID`, which would
    /// otherwise send a lift for a contact the host never saw start.
    sent: bool,
    x: i32,
    y: i32,
    /// Something in this report changed the contact — sent at the next `SYN_REPORT` and cleared.
    dirty: bool,
}

/// A `PlayStation` pad's touchpad, which the kernel publishes as its own absolute/multitouch node
/// alongside the pad itself — the source of the stuck left button [`super::mouse::is_touch_emulated`]
/// describes, since the compositor drives the TV cursor from it. Claiming it takes it away from
/// the compositor entirely and gives [`read_touch`] the contacts to forward as
/// [`RichInput::Touchpad`], which is what makes a game's touchpad swipes work. The pad's own
/// *click* doesn't come through here at all — that arrives on the pad node as `BTN_TOUCHPAD`
/// through SDL's controller layer.
///
/// Matched on ids and capability bits, not the device name, which varies by driver and firmware:
/// `BTN_TOUCH` on an absolute pointer (the pad's own node reports face buttons instead), from a
/// Sony vendor id. `vendor` is lazy because it costs an ioctl the bit tests don't.
fn is_pad_touchpad(absolute_pointer: bool, has_btn_touch: bool, vendor: impl FnOnce() -> u16) -> bool {
    absolute_pointer && has_btn_touch && vendor() == VENDOR_SONY
}

/// A `PlayStation` pad's motion sensors, published as a third node beside the pad and its
/// touchpad. SDL never reads it here, so gyro aiming reaches the host only if this reader forwards it
/// as [`RichInput::Motion`].
///
/// Matched on capability bits and vendor, like [`is_pad_touchpad`]: all six sensor axes and no
/// buttons. Both other nodes carry those same axes — the pad node's are its sticks and triggers,
/// the touchpad's are the contact — so the buttons (`BTN_SOUTH`, `BTN_TOUCH`) are what part them.
/// Matching the pad node here would grab the gamepad away from SDL entirely.
fn is_pad_motion(abs: &[u8; 128], has_buttons: bool, vendor: impl FnOnce() -> u16) -> bool {
    (ABS_X..=ABS_RZ).all(|c| bit(abs, c)) && !has_buttons && vendor() == VENDOR_SONY
}

/// The units the wire carries: the raw `DualSense` report scale the host's *virtual* pad is
/// calibrated for, since the host injects inputtino's fixed calibration blob. Same convention as
/// the Linux and Apple clients.
const WIRE_GYRO_LSB_PER_DEG_S: i32 = 20;
const WIRE_ACCEL_LSB_PER_G: i32 = 10_000;

/// `hid-playstation`'s own output scale, for a node that publishes no `resolution`.
const DS_GYRO_RES_PER_DEG_S: i32 = 1024;
const DS_ACC_RES_PER_G: i32 = 8192;

/// Fixed-point fraction bits of [`Sensors::scale`]. Both real ratios (20/1024, 10000/8192) are
/// exact at 16.
const SCALE_SHIFT: u32 = 16;

impl Sensors {
    /// Builds the per-axis wire scale once, at open. The node's `resolution` is per device — the
    /// driver derives it from *this* pad's factory calibration blob — so it is the only honest
    /// answer to what a raw count here means. Reciprocal rather than a divisor: armv7 has no
    /// 64-bit divide, and [`flush_sensors`] runs per read burst.
    fn new(fd: RawFd) -> Self {
        let scale = std::array::from_fn(|i| {
            // Gyro axes first, then accelerometer — the order `read_sensors` fills.
            let (code, wire, fallback) = if i < 3 {
                (ABS_RX + i as u16, WIRE_GYRO_LSB_PER_DEG_S, DS_GYRO_RES_PER_DEG_S)
            } else {
                (ABS_X + (i - 3) as u16, WIRE_ACCEL_LSB_PER_G, DS_ACC_RES_PER_G)
            };
            let res = match abs_resolution(fd, code) {
                r if r > 0 => r,
                _ => fallback,
            };
            (i64::from(wire) << SCALE_SHIFT) / i64::from(res)
        });
        Self {
            scale,
            ..Default::default()
        }
    }

    /// Flags a sniff-sized gap before this report, from its kernel timestamp, so `pad_link`
    /// re-wakes the link at once instead of at its next timer.
    fn note_report(&mut self, time: libc::timeval) {
        let at_us = i64::from(time.tv_sec) * 1_000_000 + i64::from(time.tv_usec);
        if self
            .last_report_us
            .replace(at_us)
            .is_some_and(|last| at_us - last > SNIFF_GAP_US)
        {
            crate::platform::webos::pad_link::SNIFF_SUSPECT.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl Touchpad {
    /// Reads the node's axis ranges once, at open. A node whose `ABS_MT_POSITION_*` ranges are
    /// unusable is still worth claiming (the compositor must not have it) — it just reports no
    /// contacts, which [`normalize`] enforces.
    fn new(fd: RawFd) -> Self {
        Self {
            slot: 0,
            fingers: [Finger::default(); 2],
            x_range: abs_range(fd, ABS_MT_POSITION_X),
            y_range: abs_range(fd, ABS_MT_POSITION_Y),
        }
    }
}

/// Device units → the wire's `0..=65535` across the pad. `None` when the driver gave no range to
/// scale against, since a bogus coordinate is worse than no contact at all.
fn normalize(x: i32, y: i32, x_range: (i32, i32), y_range: (i32, i32)) -> Option<(u16, u16)> {
    let scale = |v: i32, (min, max): (i32, i32)| {
        let span = i64::from(max) - i64::from(min);
        (span > 0).then(|| ((i64::from(v.clamp(min, max)) - i64::from(min)) * 65535 / span) as u16)
    };
    // evdev's +y is already down, the direction the wire and the host's virtual pad expect.
    Some((scale(x, x_range)?, scale(y, y_range)?))
}

/// Decodes one read burst off a claimed pad touchpad into [`RichInput::Touchpad`] contacts.
///
/// Contacts are sent at `SYN_REPORT`, not per axis event: a moving finger reports x and y as two
/// events, and sending between them would put half the samples on a stale axis. Only contacts a
/// report actually touched are sent, so a resting finger costs one burst of decode and no
/// datagrams at all.
fn read_touch(pad: &mut Touchpad, buf: &[u8], size: usize, sink: &impl Fn(RichInput)) {
    for chunk in buf.chunks_exact(size) {
        // SAFETY: as `read_device` — exact-size chunk of plain `repr(C)` integers, read unaligned.
        let ev = unsafe { chunk.as_ptr().cast::<InputEventRaw>().read_unaligned() };
        match (ev.kind, ev.code) {
            // A slot the wire can't carry is parked on the last finger, where its coordinates are
            // simply overwritten by a contact that does fit — the pad only ever reports two.
            (EV_ABS, ABS_MT_SLOT) => pad.slot = (ev.value.max(0) as usize).min(pad.fingers.len() - 1),
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                let finger = &mut pad.fingers[pad.slot];
                finger.active = ev.value >= 0;
                finger.dirty = true;
            }
            (EV_ABS, ABS_MT_POSITION_X | ABS_MT_POSITION_Y) => {
                let finger = &mut pad.fingers[pad.slot];
                if ev.code == ABS_MT_POSITION_X {
                    finger.x = ev.value;
                } else {
                    finger.y = ev.value;
                }
                finger.dirty = true;
            }
            (EV_SYN, SYN_REPORT) => {
                let (x_range, y_range) = (pad.x_range, pad.y_range);
                for (i, finger) in pad.fingers.iter_mut().enumerate() {
                    if !finger.dirty {
                        continue;
                    }
                    finger.dirty = false;
                    if !finger.active && !finger.sent {
                        continue; // never announced — nothing to lift
                    }
                    let Some(contact) = contact(i, finger, x_range, y_range) else {
                        continue;
                    };
                    finger.sent = finger.active;
                    sink(contact);
                }
            }
            _ => {}
        }
    }
}

/// One contact for the wire, or `None` when the node gave no usable axis range ([`normalize`]).
///
/// A lift's coordinates are whatever the contact last had, which is what the host wants — the
/// release still has to land where the finger was. `pad` is 0 here; the caller routes it to the
/// slot of the controller this node belongs to.
fn contact(finger_idx: usize, finger: &Finger, x_range: (i32, i32), y_range: (i32, i32)) -> Option<RichInput> {
    let (x, y) = normalize(finger.x, finger.y, x_range, y_range)?;
    Some(RichInput::Touchpad {
        pad: 0,
        finger: finger_idx as u8,
        active: finger.active,
        x,
        y,
    })
}

/// Decodes one read burst off a claimed motion node into the newest complete sample.
fn read_sensors(sensors: &mut Sensors, buf: &[u8], size: usize) {
    for chunk in buf.chunks_exact(size) {
        // SAFETY: as `read_device` — exact-size chunk of plain `repr(C)` integers, read unaligned.
        let ev = unsafe { chunk.as_ptr().cast::<InputEventRaw>().read_unaligned() };
        match (ev.kind, ev.code) {
            // Gyro first, then accelerometer: the wire's order, so no shuffling at send.
            (EV_ABS, ABS_RX..=ABS_RZ) => sensors.axes[(ev.code - ABS_RX) as usize] = ev.value,
            (EV_ABS, ABS_X..=ABS_Z) => sensors.axes[(ev.code - ABS_X) as usize + 3] = ev.value,
            // Only a completed report is a sample: sending mid-report would mix axes from two.
            (EV_SYN, SYN_REPORT) => {
                sensors.dirty = true;
                sensors.note_report(ev.time);
            }
            _ => {}
        }
    }
}

/// Sends the newest sample once per read burst, and only when it moved — a pad lying still
/// reports forever, and re-sending its resting attitude at 500 Hz would cost a datagram per
/// sample to tell the host nothing.
///
/// Rescaled to the wire's units before the `i16` clamp: this node reports `hid-playstation`'s
/// *calibrated* readings (~1024 counts per deg/s) where the wire wants the host virtual pad's raw
/// report scale (~20), so forwarding them verbatim rails every axis on the slightest movement.
/// The divide also quantizes a resting pad's bias jitter to a clean zero, which the skip below
/// then stops sending.
fn flush_sensors(sensors: &mut Sensors, sink: &impl Fn(RichInput)) {
    if !std::mem::take(&mut sensors.dirty) {
        return;
    }
    let axes: [i16; 6] = std::array::from_fn(|i| {
        // Truncating divide, not an arithmetic shift: a shift floors, so a resting pad's
        // negative bias jitter would quantize to -1 where the positive half quantizes to 0 and
        // the skip below would never catch it. The divisor is a power of two, so this is still
        // shifts, not a 64-bit divide call.
        ((i64::from(sensors.axes[i]) * sensors.scale[i]) / (1 << SCALE_SHIFT))
            .clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
    });
    if sensors.sent == Some(axes) {
        return;
    }
    sensors.sent = Some(axes);
    let [gx, gy, gz, ax, ay, az] = axes;
    // Pad 0 until routed, as in `contact`.
    sink(RichInput::Motion {
        pad: 0,
        gyro: [gx, gy, gz],
        accel: [ax, ay, az],
    });
}
