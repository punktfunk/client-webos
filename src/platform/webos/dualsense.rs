//! Adaptive triggers, lightbar and player LEDs on a real `DualSense`, over webOS's
//! Bluetooth HID plane.
//!
//! **Why this route exists.** SDL's own `DualSense` support (`SDL_GameControllerSendEffect`,
//! present in the bundled fork) drives the pad through `/dev/hidraw*`. A **wired** pad does get
//! such a node and this client writes it directly (see [`super::hidraw`]); a **Bluetooth** pad
//! got none in the jail on webOS 10.3, and there is no `hidraw` class in `/sys` to look one up
//! with. So for the transport this module serves, the effect bytes cannot go through SDL. What
//! *is* reachable is the TV's own Bluetooth stack:
//! `com.webos.service.bluetooth2/hid/internal/sendData` writes an arbitrary HID output report
//! to a connected HID device, and it sits in the `public` API group a dev-mode install
//! already holds — no root required.
//!
//! Two hard-won details, both verified on-device:
//!   * the payload carries `reportData` as an int array **with no `reportId` field** — adding
//!     one makes the service reject the call with a generic schema error that names nothing,
//!     and `setReport` (the method that *does* take a report id) always answers "operation can
//!     not be performed at this time";
//!   * the report needs the same CRC32 the kernel's `hid-playstation` appends, over a `0xA2`
//!     seed byte. Without it the pad silently ignores a call the service reports as success.
//!
//! Rumble deliberately does **not** go through here: it reaches the pad as ordinary evdev
//! force feedback via SDL (`GameController::set_rumble`), which works for every controller
//! type rather than only this one. Reports built here never set the vibration valid-flag, so
//! they cannot fight the kernel's force-feedback state — see [`build_report`].
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use punktfunk_core::quic::HidOutput;

use super::ls2;
use crate::session::pad_audio::{Envelope, COIL_REPORT_FRAMES, SPEAKER_IN_SAMPLES};

/// Counts trigger effects folded in, so a "triggers do nothing" report can be told apart from a
/// title that never sends one. A debug aid: it must not change what is written to the pad.
static TRIGGERS_SEEN: AtomicU32 = AtomicU32::new(0);

/// `hid/internal/sendData` — the only one of the three HID methods that works. `getReport`
/// hangs on a pad that doesn't answer; `setReport` refuses with error 4 whatever the payload.
const SEND_DATA_URI: &str = "luna://com.webos.service.bluetooth2/hid/internal/sendData";
/// Takes the pad's link out of Bluetooth sniff mode. In sniff, the stack delivers output reports
/// in bursts at the sniff anchor points; that starved the pad's audio buffer between bursts and
/// made every speaker layout choppy until this call — the single fix that made audio continuous
/// (G5, webOS 10.3). `public` group, like `sendData`. Its counterpart re-enters sniff on release.
const STOP_SNIFF_URI: &str = "luna://com.webos.service.bluetooth2/device/internal/stopSniff";
const START_SNIFF_URI: &str = "luna://com.webos.service.bluetooth2/device/internal/startSniff";

/// Bluetooth `DualSense` output report, per Linux `hid-playstation`'s
/// `dualsense_output_report_bt`: `0x31`, seq/tag, tag, the 47-byte common block, 24 reserved
/// bytes, then the CRC32. Exactly this length was verified accepted end-to-end.
const REPORT_LEN: usize = 78;
/// Where the 47-byte common block starts (after report id, `seq_tag`, tag).
const COMMON: usize = 3;

/// Bluetooth `0x32` coil report: id, seq, a 7-byte `0x11` session sub-packet, the 64-byte `0x12`
/// coil sub-packet (32 stereo s8 frames at 3 kHz), padding, CRC32. Verified on a G5: the pad
/// buzzes, with the `SAxense` flag convention (`tag | 0x80`) and the same CRC as `0x31`.
const COIL_REPORT_LEN: usize = 142;
/// One coil report per 512 samples at 48 kHz — the pad's clock. 10 ms (6.7% fast) overruns it.
const COIL_TICK: Duration = Duration::from_nanos(10_666_667);

/// Bluetooth `0x36` audio report (`LinuxAudio4Dualsense5`'s layout): config, a state sub-packet
/// carrying the same common block as `0x31`, the coil frame, then one 200-byte Opus speaker frame.
const AUDIO_REPORT_LEN: usize = 398;
/// One 10 ms Opus frame at 160 kbit/s CBR: exactly this many bytes, which the pad requires.
const SPEAKER_FRAME_LEN: usize = 200;
/// Samples per speaker frame after the 512 → 480 resample.
const SPEAKER_OUT_SAMPLES: usize = 480;
/// Speaker reports sent ahead before steady cadence: the pad's FIFO then holds ~100 ms, which
/// rides out the link's residual 35–45 ms scheduling gaps (measured; sniff off).
const SPEAKER_PREFILL: usize = 10;
/// Floor between two `stopSniff` calls. Sniff is a link-IDLE power state, so it is re-asserted on
/// the edge out of an idle lane, never on a timer — a lane at 94 reports/s never lets the link
/// idle. The floor exists only because a silence-gated host can gate audio on and off quickly.
const RESNIFF_FLOOR: Duration = Duration::from_secs(2);
/// The five config bytes of the audio report's `0x11` sub-packet ("audio buffer length").
const AUDIO_CONFIG: u8 = 64;
/// Speaker volume: the pad honours only `0x3D..=0x64`.
const SPEAKER_VOLUME: u8 = 0x64;

/// `valid_flag0` bit: the right (R2) trigger effect block is meaningful in this report.
const FLAG0_RIGHT_TRIGGER: u8 = 0x04;
/// `valid_flag0` bit: the left (L2) trigger effect block is meaningful in this report.
const FLAG0_LEFT_TRIGGER: u8 = 0x08;
/// `valid_flag1` bit: take over the lightbar colour.
const FLAG1_LIGHTBAR: u8 = 0x04;
/// `valid_flag1` bit: drive the five player-indicator LEDs.
const FLAG1_PLAYER_LEDS: u8 = 0x10;

/// Common-block offset of the right trigger's effect block.
const OFF_RIGHT_TRIGGER: usize = 10;
/// Common-block offset of the left trigger's effect block.
const OFF_LEFT_TRIGGER: usize = 21;
/// Common-block offset of the player-indicator LED bits.
const OFF_PLAYER_LEDS: usize = 43;
/// Common-block offset of lightbar red; green and blue follow it.
const OFF_LIGHTBAR_RED: usize = 44;

/// A `DualSense` trigger parameter block: effect mode byte plus up to ten parameters. Sized to
/// match `punktfunk_core::abi::PUNKTFUNK_HID_EFFECT_MAX`, which is what the host forwards
/// from the game — so a block copies straight in with no reinterpretation.
const EFFECT_LEN: usize = 11;
type Effect = [u8; EFFECT_LEN];

/// Everything this client drives on the pad, as absolute state rather than deltas.
///
/// Absolute on purpose: one report carries all of it, so re-sending the whole struct makes
/// every update idempotent and self-healing. The host's feedback plane is lossy by design
/// (newest-drops on overflow, no retransmit), and a dropped *delta* would leave a trigger
/// stuck resisting until the game happened to change it again.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct State {
    /// `None` until a game sets a colour — the system keeps painting the lightbar until then.
    lightbar: Option<(u8, u8, u8)>,
    /// `None` until a game sets them, for the same reason as `lightbar`.
    player_leds: Option<u8>,
    /// Mode `0x00` (the default) means "no resistance", so the zero value is already correct.
    right_trigger: Effect,
    left_trigger: Effect,
    /// Whether to assert the trigger valid-flags at all. Travels *with* the state rather than
    /// being re-derived when sending: an explicit release is mode `0x00` on both triggers,
    /// which is indistinguishable from "never touched" by looking at the bytes — deriving it
    /// would drop the one report that lets go of a stiffened trigger.
    triggers_owned: bool,
}

/// CRC32 (IEEE 802.3, reflected) — the `crc32_le` variant `hid-playstation` signs Bluetooth
/// output reports with. Bitwise, so no 1 KiB table: 398 bytes at 94 reports/s is ~300k inner
/// steps a second, well under the Opus encode beside it. One call per report — a two-call seal
/// inverts the running state at the seam and the pad silently drops every report.
/// Over an iterator so the seed byte the signature covers chains on without a copy.
fn crc32_le(bytes: impl IntoIterator<Item = u8>) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            // 0xEDB88320 = reflected 0x04C11DB7.
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// The pad's Bluetooth address, in the form `hid/internal/sendData`'s `address` wants.
///
/// Read from `/proc/bus/input/devices`' `U: Uniq=` line, which `hid-playstation` sets to the
/// pad's MAC. A `DualSense` publishes three input devices (pad, motion sensors, touchpad) all
/// sharing one `Uniq`, so the first match is the right one. `None` for a USB-connected pad
/// (no `Uniq`), which is correct — this path is Bluetooth-only.
pub fn find_address() -> Option<String> {
    let devices = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    address_in(&devices)
}

/// The Bluetooth address of an attached `DualSense`, or `None` when the only one is wired.
///
/// **`Uniq` is not the discriminator.** `hid-playstation` reads the pad's Bluetooth MAC out of its
/// pairing-info feature report over USB as well, so a wired pad publishes exactly the same address
/// as a paired one (verified on a G5 with one pad on both transports). `I: Bus=` is what separates
/// them: `0005` is Bluetooth, `0003` is USB. Everything this address reaches — `sendData`, the
/// sniff calls, the whole audio lane — goes through `bluetooth2`, which a wired pad is not on, so
/// handing one back would claim the coils for a transport that cannot carry them.
fn address_in(devices: &str) -> Option<String> {
    dualsense_blocks(devices)
        .filter(|block| block.lines().any(|l| l.trim_end().starts_with("I: Bus=0005")))
        .find_map(|block| {
            block
                .lines()
                .find_map(|l| l.strip_prefix("U: Uniq="))
                .map(|u| u.trim().to_ascii_lowercase())
                .filter(|u| !u.is_empty())
        })
}

/// The `/proc/bus/input/devices` records of every connected `DualSense`. One pad publishes three
/// of them, and only some carry the field a caller wants.
///
/// Matched on the `I:` ids first, and on the name only as a fallback: the kernel takes the input
/// device's name straight from the HID/Bluetooth product name (`input_dev->name = hdev->name` in
/// `hid-playstation`), and the pad advertises itself as a plain "Wireless Controller" — so a set
/// that decorates the name is the lucky case, not the rule. The ids are the pad either way, which
/// matters most to [`hid_playstation_bound`]: it asks specifically about the pads a *generic*
/// binding is handling, and those are the ones least likely to be named after themselves.
fn dualsense_blocks<'a>(devices: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    devices.split("\n\n").filter(|block| {
        block.lines().any(|l| {
            (l.starts_with("I: ") && is_dualsense_ids(l))
                || (l.starts_with("N: Name=") && l.to_ascii_lowercase().contains("dualsense"))
        })
    })
}

/// Sony's vendor plus a `DualSense`/`Edge` product on a `/proc/bus/input/devices` `I:` line.
/// The same pair [`super::hidraw`] opens a node by.
fn is_dualsense_ids(id_line: &str) -> bool {
    let field = |key: &str| {
        id_line
            .split_whitespace()
            .find_map(|f| f.strip_prefix(key))
            .and_then(|v| u16::from_str_radix(v, 16).ok())
    };
    field("Vendor=") == Some(0x054c) && field("Product=").is_some_and(|p| p == 0x0ce6 || p == 0x0df2)
}

/// Whether an attached `DualSense`/`Edge` is bound to the kernel's `hid-playstation` driver
/// (registered as `playstation`) rather than falling back to `hid-generic`.
///
/// The Bluetooth output report this module builds is a `hid-playstation` behavior, not a
/// property of the `luna` call: on a TV whose kernel never got that driver backported, the pad
/// still pairs and shows `connectedProfiles: ["hid"]`, and `hid/internal/sendData` still answers
/// `returnValue: true` for every report — but nothing reaches the pad, silently. Verified on a
/// webOS 5/6-class set (kernel 4.4.84): `/sys/bus/hid/devices/0005:054C:0CE6.0002/driver` links to
/// `hid-generic`, and neither the lightbar nor the player LEDs moved for a report identical to
/// the one confirmed working on webOS 10.3. This is the caption Settings shows for that case.
///
/// Answered from a short-lived cache. The Settings screen reads this while building its rows,
/// which the render pass does every tick the modal animates — and the uncached answer is two
/// filesystem reads. A pad binding changes only when one is plugged in or out, so a second of
/// staleness is invisible and 60 reads a second are not.
pub fn hid_playstation_bound() -> bool {
    /// How long a probe's answer stands. Human-scale: a hotplug shows up in the caption within
    /// one refresh, and nothing else in the app reacts faster than that.
    const TTL: Duration = Duration::from_secs(1);
    static CACHED: Mutex<Option<(Instant, bool)>> = Mutex::new(None);

    let mut cached = CACHED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, bound)) = *cached {
        if at.elapsed() < TTL {
            return bound;
        }
    }
    let bound = probe_hid_playstation_bound();
    *cached = Some((Instant::now(), bound));
    bound
}

/// The uncached probe behind [`hid_playstation_bound`].
fn probe_hid_playstation_bound() -> bool {
    let devices = std::fs::read_to_string("/proc/bus/input/devices").unwrap_or_default();
    // Bound for the same reason as in `find_address`.
    let bound = dualsense_blocks(&devices)
        // The evdev node's `Sysfs=` line points at `<hid-device>/input/inputN`; the driver
        // binding lives on the HID device itself, one level up.
        .filter_map(|block| block.lines().find_map(|l| l.strip_prefix("S: Sysfs=")))
        .any(|sysfs| {
            let device_dir = sysfs.split("/input/input").next().unwrap_or(sysfs);
            std::fs::read_link(format!("/sys{device_dir}/driver"))
                .is_ok_and(|d| d.file_name().is_some_and(|f| f == "playstation"))
        });
    bound
}

/// Owns the pad's feedback state and the thread that ships it.
///
/// The thread exists because a send is a fork/exec of `luna-send-pub` (see [`crate::platform::webos::luna`]),
/// which must never land on the render/input loop. Queue depth is one with latest-wins
/// replacement — the same discipline as [`crate::services::store::StateWriter`] — because the state
/// is absolute: a superseded update carries no information the newer one lacks.
pub struct Feedback {
    state: State,
    /// `None` only after [`Drop`] has taken it to close the channel.
    tx: Option<SyncSender<State>>,
    /// `None` only after [`Drop`] has taken and joined it.
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Feedback {
    /// Starts a feedback sender for the pad at Bluetooth `address`, or `None` when the
    /// platform can't support it (no `luna-send-pub`). Resolve `address` via [`find_address`].
    pub fn new(address: String, coils: Option<Arc<Envelope>>) -> Option<Self> {
        if !crate::platform::webos::luna::available() {
            return None;
        }
        // Depth 1 + `try_send`: at most one pending state, and a full queue means the sender
        // is mid-call — that pending value is stale by definition, so the next update replaces
        // it rather than queueing behind it.
        let (tx, rx) = std::sync::mpsc::sync_channel::<State>(1);
        let thread = std::thread::Builder::new()
            .name("ds5-feedback".into())
            .spawn(move || sender_loop(&address, &rx, coils))
            .ok()?;
        tracing::info!("DualSense feedback active (adaptive triggers, lightbar)");
        Some(Self {
            state: State::default(),
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// Same, for a pad on USB: the kernel's hidraw node instead of the Bluetooth service.
    ///
    /// Takes no [`Envelope`]: the `0xD1` lanes do not ride hidraw. A wired pad carries them on its
    /// own UAC audio card, so the coils are NOT claimed here and the derived rumble envelope keeps
    /// the motors — which is what gives a wired pad any vibration at all under a libScePad title.
    pub fn new_usb(node: crate::platform::webos::hidraw::Hidraw) -> Option<Self> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<State>(1);
        tracing::info!("DualSense feedback active over USB ({})", node.path);
        let thread = std::thread::Builder::new()
            .name("ds5-feedback-usb".into())
            .spawn(move || usb_sender_loop(&node, &rx))
            .ok()?;
        Some(Self {
            state: State::default(),
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    /// Folds one host feedback event into the pad state and queues a send.
    ///
    /// Variants this pad has no route for are dropped: `TrackpadHaptic` is a Steam
    /// Controller voice-coil buzz, and `HidRaw` is a passthrough report for a device the host
    /// mirrors as-is — replaying either on a `DualSense` would mean writing arbitrary bytes in
    /// the wrong protocol. `AudioCtl` is the routing/volume half of pad audio: the client does
    /// advertise `CLIENT_CAP_PAD_AUDIO` now, so a host can send it, but every audio report already
    /// re-asserts routing and the pad honours volume only in `0x3D..=0x64` — [`SPEAKER_VOLUME`]
    /// sits at that ceiling. Dropped until a host asks for something that range can express.
    pub fn apply(&mut self, event: &HidOutput) {
        match event {
            HidOutput::Led { r, g, b, .. } => self.state.lightbar = Some((*r, *g, *b)),
            HidOutput::PlayerLeds { bits, .. } => self.state.player_leds = Some(*bits),
            HidOutput::Trigger { which, effect, .. } => {
                let mut block = Effect::default();
                let n = effect.len().min(EFFECT_LEN);
                block[..n].copy_from_slice(&effect[..n]);
                // `which`: 0 = L2, 1 = R2 (`punktfunk_core::quic::HidOutput`).
                if *which == 0 {
                    self.state.left_trigger = block;
                } else {
                    self.state.right_trigger = block;
                }
                self.state.triggers_owned = true;
                // Whether a title sends trigger effects at all is the question a "triggers do
                // nothing" report actually turns on, and no other line answers it: the transport
                // is shared with the lightbar, so silence here means the host sent nothing.
                let n = TRIGGERS_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % 20 == 0 {
                    tracing::debug!(
                        "DualSense trigger effect #{n}: {} mode {:#04x}",
                        if *which == 0 { "L2" } else { "R2" },
                        block[0]
                    );
                }
            }
            HidOutput::TrackpadHaptic { .. } | HidOutput::HidRaw { .. } | HidOutput::AudioCtl { .. } => return,
        }
        // Dropping on a full queue *is* the coalescing: the waiting value is strictly older.
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(self.state);
        }
    }

    /// Releases everything this client took over — both triggers back to no resistance, the
    /// lightbar handed back to the system.
    ///
    /// Trigger resistance lives in the pad's firmware, not in the stream, so without this a
    /// game that left R2 stiff leaves it stiff on the TV's home screen and after the app
    /// exits, with nothing to connect that to punktfunk. Blocking send, unlike
    /// [`apply`](Self::apply): this one must not be the update that gets coalesced away, and
    /// [`Drop`] joins the sender right after so it is actually delivered.
    ///
    /// Called on the way out of a stream, so it delays return-to-menu by one send — tens of
    /// milliseconds normally, and at worst two [`crate::platform::webos::luna::CALL_TIMEOUT`] windows if the
    /// Bluetooth service has stopped answering.
    pub fn release(&mut self) {
        self.state = State {
            triggers_owned: true, // assert mode 0x00 explicitly rather than staying silent
            ..State::default()
        };
        if let Some(tx) = &self.tx {
            let _ = tx.send(self.state);
        }
    }
}

impl Drop for Feedback {
    fn drop(&mut self) {
        // Closing the channel ends `sender_loop`; joining lets a send in flight (and anything
        // `release` just queued) complete — the same reason `StateWriter` joins its writer.
        drop(self.tx.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Builds one Bluetooth output report for `state`.
///
/// Note which valid-flags are never set: not `0x01` (compatible vibration), so the motor
/// bytes stay ignored and this report cannot cancel rumble that SDL's evdev force-feedback
/// path is driving independently.
fn build_report(seq: u8, state: &State) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0] = 0x31; // Bluetooth output report id
    r[1] = (seq & 0x0F) << 4; // high nibble = sequence, low nibble = tag mask (0)
    r[2] = 0x10; // DS_OUTPUT_TAG
    fill_common(&mut r[COMMON..COMMON + 47], state);
    // CRC32 over a 0xA2 seed byte (the HIDP DATA/Output header the stack prepends) followed
    // by everything ahead of the CRC field itself.
    let signed = std::iter::once(0xA2).chain(r[..REPORT_LEN - 4].iter().copied());
    let crc = crc32_le(signed);
    r[REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// The 47-byte common block (`0x31` body, `0x10` sub-packet payload) for `state`.
fn fill_common(c: &mut [u8], state: &State) {
    if state.triggers_owned {
        c[0] = FLAG0_RIGHT_TRIGGER | FLAG0_LEFT_TRIGGER;
        c[OFF_RIGHT_TRIGGER..][..EFFECT_LEN].copy_from_slice(&state.right_trigger);
        c[OFF_LEFT_TRIGGER..][..EFFECT_LEN].copy_from_slice(&state.left_trigger);
    }
    if let Some((red, green, blue)) = state.lightbar {
        c[1] |= FLAG1_LIGHTBAR;
        c[OFF_LIGHTBAR_RED] = red;
        c[OFF_LIGHTBAR_RED + 1] = green;
        c[OFF_LIGHTBAR_RED + 2] = blue;
    }
    if let Some(bits) = state.player_leds {
        c[1] |= FLAG1_PLAYER_LEDS;
        c[OFF_PLAYER_LEDS] = bits & 0x1F;
    }
}

/// The one-time speaker setup: route the shared output to the speaker at full volume with the
/// pre-amp, as the Linux sink does. `0x31` with the audio valid-flags only, so it touches nothing
/// the state reports own.
fn build_speaker_setup(seq: u8) -> [u8; REPORT_LEN] {
    let mut r = [0u8; REPORT_LEN];
    r[0] = 0x31;
    r[1] = (seq & 0x0F) << 4;
    r[2] = 0x10;
    r[COMMON] = 0x80 | 0x20; // AllowAudioControl | AllowSpeakerVolume
    r[COMMON + 1] = 0x80; // AllowAudioControl2
    r[COMMON + 5] = SPEAKER_VOLUME;
    r[COMMON + 7] = 0x30; // OutputPathSelect: speaker
    r[COMMON + 37] = 0x02; // SpeakerCompPreGain
    let signed = std::iter::once(0xA2).chain(r[..REPORT_LEN - 4].iter().copied());
    let crc = crc32_le(signed);
    r[REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Builds one `0x36` audio report: config, the state sub-packet (our common block plus the audio
/// valid-flags, so the pad keeps its routing every report), the coil frame, the speaker frame.
fn build_audio_report(
    seq: u8,
    counter: u8,
    state: &State,
    coils: &[[i8; 2]; COIL_REPORT_FRAMES],
    speaker: &[u8; SPEAKER_FRAME_LEN],
) -> [u8; AUDIO_REPORT_LEN] {
    let mut r = [0u8; AUDIO_REPORT_LEN];
    r[0] = 0x36;
    r[1] = (seq & 0x0F) << 4;
    r[2] = 0x11 | 0x80;
    r[3] = 7;
    r[4] = 0xFE;
    r[5..10].fill(AUDIO_CONFIG);
    r[10] = counter;
    r[11] = 0x10 | 0x80;
    r[12] = 63;
    fill_common(&mut r[13..13 + 47], state);
    r[13] |= 0x80 | 0x20; // AllowAudioControl | AllowSpeakerVolume
    r[14] |= 0x80; // AllowAudioControl2
    r[13 + 5] = SPEAKER_VOLUME;
    r[13 + 7] = 0x30;
    r[13 + 37] = 0x02;
    r[76] = 0x12 | 0x80;
    r[77] = (COIL_REPORT_FRAMES * 2) as u8;
    for (i, [l, right]) in coils.iter().enumerate() {
        r[78 + i * 2] = *l as u8;
        r[79 + i * 2] = *right as u8;
    }
    r[142] = 0x13 | 0x80;
    r[143] = SPEAKER_FRAME_LEN as u8;
    r[144..144 + SPEAKER_FRAME_LEN].copy_from_slice(speaker);
    let signed = std::iter::once(0xA2).chain(r[..AUDIO_REPORT_LEN - 4].iter().copied());
    let crc = crc32_le(signed);
    r[AUDIO_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Linear 512 → 480 stereo resample: the host feeds 48 kHz, the pad plays one 480-sample frame
/// per 10.667 ms report. Free-standing so the ratio is checkable without an encoder — a wrong
/// step is inaudible on speech and only shows up as the pad drifting off the host's clock.
fn resample_frame(pcm: &[f32], out: &mut [f32]) {
    let step = (SPEAKER_IN_SAMPLES - 1) as f32 / (SPEAKER_OUT_SAMPLES - 1) as f32;
    for i in 0..SPEAKER_OUT_SAMPLES {
        let pos = i as f32 * step;
        let j = (pos as usize).min(SPEAKER_IN_SAMPLES - 2);
        let t = pos - j as f32;
        for ch in 0..2 {
            let a = pcm[j * 2 + ch];
            let b = pcm[(j + 1) * 2 + ch];
            out[i * 2 + ch] = a + (b - a) * t;
        }
    }
}

/// The next tick strictly after `now`, in whole [`COIL_TICK`] steps from `tick`.
///
/// Whole steps hold the pad's phase, and landing in the FUTURE is what stops a tick whose work
/// overran from firing again at once — each catch-up report is one the pad's clock has no room
/// for, and it goes out carrying silence.
fn advance_tick(mut tick: Instant, now: Instant) -> Instant {
    while tick <= now {
        tick += COIL_TICK;
    }
    tick
}

/// The speaker encoder: 512 stereo samples per report resampled to 480, Opus 160 kbit/s CBR at
/// complexity 0 — the settings both working implementations use, and the ones this `SoC` affords.
struct SpeakerLane {
    encoder: opus::Encoder,
    /// An encoded frame of silence, sent when the ring is short so the pad's clock keeps running.
    silence: [u8; SPEAKER_FRAME_LEN],
    resampled: Vec<f32>,
}

impl SpeakerLane {
    fn new() -> anyhow::Result<Self> {
        let mut encoder = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .map_err(|e| anyhow::anyhow!("opus encoder: {e}"))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(160_000))
            .map_err(|e| anyhow::anyhow!("bitrate: {e}"))?;
        encoder.set_vbr(false).map_err(|e| anyhow::anyhow!("cbr: {e}"))?;
        encoder
            .set_complexity(0)
            .map_err(|e| anyhow::anyhow!("complexity: {e}"))?;
        let mut lane = Self {
            encoder,
            silence: [0; SPEAKER_FRAME_LEN],
            resampled: vec![0.0; SPEAKER_OUT_SAMPLES * 2],
        };
        let zeros = [0f32; SPEAKER_IN_SAMPLES * 2];
        lane.silence = lane.encode(&zeros);
        Ok(lane)
    }

    /// One report's frame from 512 stereo samples: linear 512 → 480, then a CBR frame padded or
    /// cut to exactly 200 bytes (CBR lands there by itself; the guard is for a short first frame).
    fn encode(&mut self, pcm: &[f32]) -> [u8; SPEAKER_FRAME_LEN] {
        resample_frame(pcm, &mut self.resampled);
        let mut out = [0u8; SPEAKER_FRAME_LEN];
        match self.encoder.encode_float(&self.resampled, &mut out) {
            Ok(n) if n == SPEAKER_FRAME_LEN => {}
            Ok(n) => tracing::debug!("speaker lane: {n}-byte frame, expected {SPEAKER_FRAME_LEN}"),
            Err(e) => tracing::debug!("speaker lane: encode failed: {e}"),
        }
        out
    }
}

/// `reportData` as a bare int array — **no `reportId` key**. The service validates the whole
/// object against a strict schema, so one extra property fails the call outright.
fn payload_for(address: &str, report: &[u8]) -> String {
    let mut payload = String::with_capacity(report.len() * 4 + 64);
    let _ = write!(payload, "{{\"address\":\"{address}\",\"reportData\":[");
    for (i, b) in report.iter().enumerate() {
        let _ = if i == 0 {
            write!(payload, "{b}")
        } else {
            write!(payload, ",{b}")
        };
    }
    payload.push_str("]}");
    payload
}

fn send_report(address: &str, report: &[u8; REPORT_LEN]) -> anyhow::Result<()> {
    crate::platform::webos::luna::call(SEND_DATA_URI, &payload_for(address, report))
}

/// The wired pad's output report: `0x02`, the same 47-byte common block the Bluetooth `0x31`
/// carries, then 15 reserved bytes — `hid-playstation`'s `DS_OUTPUT_REPORT_USB_SIZE`, the
/// counterpart of the 78 in [`REPORT_LEN`]. No CRC: the kernel addresses a wired pad directly, so
/// nothing signs for us. **The length is load-bearing.** A report cut to the common block alone is
/// short of what the descriptor declares and the pad drops it without a word — lightbar and
/// triggers simply never move, which is exactly how this was found.
const USB_REPORT_LEN: usize = 63;
/// Where the common block sits in it: straight after the report id.
const USB_COMMON: usize = 1;

fn build_usb_report(state: &State) -> [u8; USB_REPORT_LEN] {
    let mut r = [0u8; USB_REPORT_LEN];
    r[0] = 0x02;
    fill_common(&mut r[USB_COMMON..USB_COMMON + 47], state);
    r
}

/// The wired pad's routing report: sends the pad's audio out of its **speaker** rather than the
/// headphone jack it defaults to, at the one volume it honours.
///
/// The Bluetooth lane re-asserts this inside every `0x36`; a wired pad has no such carrier, so it
/// is sent once when the card opens. Without it the coils play and the speaker stays silent —
/// which looks exactly like a broken speaker lane.
pub fn build_usb_speaker_setup() -> [u8; USB_REPORT_LEN] {
    let mut r = [0u8; USB_REPORT_LEN];
    r[0] = 0x02;
    r[USB_COMMON] = 0x80 | 0x20; // AllowAudioControl | AllowSpeakerVolume
    r[USB_COMMON + 1] = 0x80; // AllowAudioControl2
    r[USB_COMMON + 5] = SPEAKER_VOLUME;
    r[USB_COMMON + 7] = 0x30; // OutputPathSelect: speaker
    r[USB_COMMON + 37] = 0x02; // SpeakerCompPreGain
    r
}

/// Drains queued states to a wired pad.
///
/// No throttle and no dedupe interval, unlike the Luna routes: a hidraw write is one syscall, not
/// the fork/exec that blacked out the video plane, so there is nothing here to protect the render
/// loop from. Identical states are still dropped — the pad gains nothing from being told twice.
fn usb_sender_loop(node: &crate::platform::webos::hidraw::Hidraw, rx: &Receiver<State>) {
    let mut last: Option<State> = None;
    let mut failing = false;
    while let Ok(state) = rx.recv() {
        if last == Some(state) {
            continue;
        }
        last = Some(state);
        match node.write_report(&build_usb_report(&state)) {
            Ok(()) => failing = false,
            Err(e) => {
                if !failing {
                    tracing::warn!("DualSense USB feedback failed (further errors quiet): {e:#}");
                    failing = true;
                }
            }
        }
    }
}

/// Builds one Bluetooth coil report: `frames` are 32 stereo s8 samples at 3 kHz.
///
/// Sub-packet layout per `awalol/DS5Dongle` / `egormanga/SAxense`: `[tag | 0x80][len][payload]`.
/// The `0x11` session packet carries a free-running `counter`; its other bytes are what every
/// working implementation sends and mean nothing documented.
fn build_coil_report(seq: u8, counter: u8, frames: &[[i8; 2]; COIL_REPORT_FRAMES]) -> [u8; COIL_REPORT_LEN] {
    let mut r = [0u8; COIL_REPORT_LEN];
    r[0] = 0x32;
    r[1] = (seq & 0x0F) << 4;
    r[2] = 0x11 | 0x80;
    r[3] = 7;
    r[4] = 0xFE;
    r[9] = 0xFF;
    r[10] = counter;
    r[11] = 0x12 | 0x80;
    r[12] = (COIL_REPORT_FRAMES * 2) as u8;
    for (i, [l, right]) in frames.iter().enumerate() {
        r[13 + i * 2] = *l as u8;
        r[14 + i * 2] = *right as u8;
    }
    let signed = std::iter::once(0xA2).chain(r[..COIL_REPORT_LEN - 4].iter().copied());
    let crc = crc32_le(signed);
    r[COIL_REPORT_LEN - 4..].copy_from_slice(&crc.to_le_bytes());
    r
}

/// Spacing between sends on the in-process bus ([`crate::platform::webos::ls2`]): no spawn per
/// report, so this only bounds how fast a host animating the lightbar can push reports through
/// the Bluetooth service. Well inside what the pad accepts (audio runs at 10.7 ms per report).
const BUS_SEND_INTERVAL: Duration = Duration::from_millis(16);

/// Minimum wall time between two sends.
///
/// A send is a fork/exec of `luna-send-pub` (see [`crate::platform::webos::luna`]), which copies the page
/// tables of a process holding SDL, the decoder and its frame buffers. **Unthrottled, this
/// blacked out the video plane on a real stream**: a Steam/Gamescope host animates the
/// `DualSense` lightbar continuously, which turned every animation step into a process spawn —
/// dozens per second on a 2-3 core TV. Frames kept decoding while the compositor never got to
/// present, so the panel stayed black with the frame counter climbing and nothing dropped.
///
/// 250 ms is far finer than a trigger effect meaningfully changes (weapon swaps, state
/// transitions) and caps the cost at four spawns a second in the worst case.
const MIN_SEND_INTERVAL: Duration = Duration::from_millis(250);

/// Drains queued states, sending each. Ends when the channel closes (`Feedback` dropped).
///
/// Two guards keep the spawn rate down, both necessary: identical states are dropped outright
/// (a host re-asserting the same lightbar colour costs nothing), and the remainder are spaced
/// by [`MIN_SEND_INTERVAL`]. Sleeping *after* a send rather than dropping the update is what
/// makes the throttle lossless — the depth-1 channel keeps replacing the pending state while
/// this thread waits, so the newest one goes out next.
///
/// One log per run of failures, not per send: if the Bluetooth service stops accepting (pad
/// powered off mid-session) every later update would otherwise repeat the same line for as
/// long as the game keeps changing effects.
fn sender_loop(address: &str, rx: &Receiver<State>, coils: Option<Arc<Envelope>>) {
    // In-process bus first: one function call per report instead of a spawn. Its failure is the
    // normal state on a TV whose hub refuses the registration, and the spawn route still works
    // there — so it is logged as information and the throttle stays at the spawn-safe value.
    let bus = match ls2::Bus::open() {
        Ok(bus) => {
            tracing::info!("DualSense feedback: in-process Luna bus");
            Some(bus)
        }
        Err(e) => {
            tracing::info!("DualSense feedback: in-process Luna bus unavailable ({e:#}); using luna-send-pub");
            None
        }
    };
    // The coil lane needs the bus: ~94 reports a second is not a spawn rate. Claiming it here
    // parks the motor envelope for the session (`Envelope::own_coils`).
    let sniff_payload = format!("{{\"address\":\"{address}\"}}");
    // `startSniff` does NOT take the address-only payload `stopSniff` does: it wants HCI Sniff
    // Mode's own parameters, and refuses anything else with a generic schema error (verified by
    // sending both shapes tagged, webOS 10.3). Intervals are 0.625 ms slots, and they bound how
    // long the pad waits to transmit — the pad also drives this app's UI between sessions, so
    // they track the ~77 ms anchor the TV's own policy used rather than a slower power-saving one.
    let sniff_params_payload =
        format!("{{\"address\":\"{address}\",\"minInterval\":96,\"maxInterval\":124,\"attempt\":4,\"timeout\":1}}");
    let lane = match (&bus, coils) {
        (Some(bus), Some(envelope)) => {
            // Un-burst the link before the first audio report; the reply is counted like any
            // other (a refusal shows up once in the log through `REPLIES`).
            match bus.call(STOP_SNIFF_URI, &sniff_payload, ls2::Call::StopSniff) {
                Ok(()) => tracing::info!("DualSense audio: lanes over the Luna bus, sniff stopped"),
                Err(e) => tracing::warn!("DualSense audio: stopSniff refused ({e:#}); expect bursts"),
            }
            envelope.own_coils();
            Some(envelope)
        }
        _ => None,
    };
    // The speaker encoder is only built once a lane exists; a failure leaves the coils alone.
    let mut speaker = lane.as_ref().and_then(|_| match SpeakerLane::new() {
        Ok(lane) => Some(lane),
        Err(e) => {
            tracing::warn!("DualSense speaker lane off: {e:#}");
            None
        }
    });
    if let (Some(bus), Some(_)) = (&bus, &speaker) {
        // Routing + volume once; every audio report re-asserts it in its state sub-packet.
        let _ = bus.call(
            SEND_DATA_URI,
            &payload_for(address, &build_speaker_setup(0)),
            ls2::Call::SendReport,
        );
    }
    let mut speaker_pcm = [0f32; SPEAKER_IN_SAMPLES * 2];
    // `true` while the pad plays the speaker lane: cleared when it goes quiet, so the next burst
    // pre-fills again instead of starting from an empty pad buffer.
    let mut speaker_live = false;
    // Ticks still allowed to send two reports, so the pre-fill reaches the pad's FIFO.
    let mut prefill_left: usize = 0;
    // The state the audio report's sub-packet carries: the last one applied.
    let mut current: State = State::default();
    let interval = if bus.is_some() {
        BUS_SEND_INTERVAL
    } else {
        MIN_SEND_INTERVAL
    };
    let mut failing = false;
    let mut last_sent: Option<State> = None;
    let mut last_sent_at: Option<Instant> = None;
    let mut pending: Option<State> = None;
    let mut sends: u32 = 0;
    // The pad expects a changing sequence number per report; low 4 bits, so it wraps freely.
    // Shared by state and coil reports: it is per link, not per report kind.
    let mut seq: u8 = 0;
    let mut next_tick = Instant::now() + COIL_TICK;
    // Sniff is re-asserted on the edge out of an idle lane; `stopSniff` at open covers the first.
    let mut was_quiet = false;
    let mut last_resniff: Option<Instant> = None;
    let mut counter: u8 = 0;
    let mut frames = [[0i8; 2]; COIL_REPORT_FRAMES];
    let mut coil_sends: u32 = 0;
    loop {
        // A state update, or the coil tick — whichever is first. Without a lane the wait is
        // unbounded, as before; the spawn route also sleeps between sends, which the lane cannot.
        let received = match &lane {
            Some(_) => match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(state) => Some(state),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(state) => Some(state),
                Err(_) => break,
            },
        };
        if let Some(state) = received {
            if last_sent != Some(state) {
                pending = Some(state);
            }
        }
        let due = last_sent_at.is_none_or(|at| at.elapsed() >= interval);
        if let (Some(state), true) = (pending, due) {
            pending = None;
            seq = seq.wrapping_add(1);
            let report = build_report(seq, &state);
            let sent = match &bus {
                Some(bus) => bus.call(SEND_DATA_URI, &payload_for(address, &report), ls2::Call::SendReport),
                None => send_report(address, &report),
            };
            match sent {
                Ok(()) => {
                    failing = false;
                    last_sent = Some(state);
                    current = state;
                    last_sent_at = Some(Instant::now());
                    sends += 1;
                    // Rate visible in the log without one line per send — the symptom the
                    // spawn throttle exists for is invisible from the client's own counters.
                    if sends % 50 == 0 {
                        tracing::debug!("DualSense feedback: {sends} reports sent");
                    }
                }
                Err(e) => {
                    if !failing {
                        tracing::warn!("DualSense feedback send failed (further errors quiet): {e}");
                        failing = true;
                    }
                }
            }
            if bus.is_none() {
                std::thread::sleep(interval);
            }
        }
        if let (Some(envelope), Some(bus)) = (&lane, &bus) {
            let now = Instant::now();
            if now >= next_tick {
                next_tick = advance_tick(next_tick, now);
                let had_coils = envelope.take_coils(&mut frames);
                let speaker_on = speaker.is_some() && envelope.speaker_active();
                if !speaker_on {
                    speaker_live = false;
                    prefill_left = 0;
                }
                // Pre-fill: hold the first reports back until the ring holds SPEAKER_PREFILL
                // frames, then let the next few ticks send two, so the surplus lands in the pad's
                // FIFO as headroom rather than in ours.
                let mut holding = false;
                if speaker_on && !speaker_live {
                    if envelope.speaker_queued() < SPEAKER_PREFILL * SPEAKER_IN_SAMPLES {
                        holding = true;
                    } else {
                        speaker_live = true;
                        prefill_left = SPEAKER_PREFILL - 1;
                    }
                }
                // Two per tick only while that pre-fill drains. Keyed on ring depth alone it never
                // stops: the host feeds exactly the pad's rate, so depth later is clock drift, not
                // backlog, and the ring's own cap is what absorbs that.
                let reports_now = if prefill_left > 0 && envelope.speaker_queued() >= 2 * SPEAKER_IN_SAMPLES {
                    prefill_left -= 1;
                    2
                } else {
                    1
                };
                // Re-assert on the edge out of idle: that is the only window in which the link can
                // have slid back into sniff, and it costs one call per burst instead of two a second.
                let sending = !holding && (speaker_live || had_coils || envelope.active());
                let refloor = match last_resniff {
                    Some(t) => now.duration_since(t) >= RESNIFF_FLOOR,
                    None => true,
                };
                if sending && was_quiet && refloor {
                    last_resniff = Some(now);
                    let _ = bus.call(STOP_SNIFF_URI, &sniff_payload, ls2::Call::StopSniff);
                }
                was_quiet = !sending;
                for _ in 0..if holding { 0 } else { reports_now } {
                    let report: Vec<u8> = if speaker_live {
                        let lane_enc = speaker.as_mut().expect("speaker_live implies a lane");
                        let frame = if envelope.take_speaker(&mut speaker_pcm) {
                            lane_enc.encode(&speaker_pcm)
                        } else {
                            lane_enc.silence
                        };
                        seq = seq.wrapping_add(1);
                        counter = counter.wrapping_add(1);
                        build_audio_report(seq, counter, &current, &frames, &frame).to_vec()
                    } else if had_coils || envelope.active() {
                        // Keep the cadence through the hold window after the last frame — zeros, so
                        // the pad's buffer runs out cleanly rather than looping its tail.
                        seq = seq.wrapping_add(1);
                        counter = counter.wrapping_add(1);
                        build_coil_report(seq, counter, &frames).to_vec()
                    } else {
                        break;
                    };
                    if let Err(e) = bus.call(SEND_DATA_URI, &payload_for(address, &report), ls2::Call::SendReport) {
                        if !failing {
                            tracing::warn!("DualSense audio send failed (further errors quiet): {e}");
                            failing = true;
                        }
                    } else {
                        coil_sends += 1;
                        if coil_sends % 940 == 0 {
                            tracing::debug!(
                                "DualSense audio: {coil_sends} reports ({} speaker)",
                                if speaker_live { "with" } else { "no" }
                            );
                        }
                    }
                    frames = [[0; 2]; COIL_REPORT_FRAMES];
                }
            }
        }
        if let Some(bus) = &bus {
            bus.pump();
            // A refused reply is asynchronous: surface the first one per run of failures.
            if let Some(text) = ls2::REPLIES.take_failure() {
                tracing::warn!("DualSense feedback: Bluetooth service refused a report: {text}");
            }
        }
    }
    // Give the link back to sniff: the TV's own power policy, and what the pad expects at idle.
    if let (Some(bus), Some(_)) = (&bus, &lane) {
        let _ = bus.call(START_SNIFF_URI, &sniff_params_payload, ls2::Call::StartSniff);
        // Wait out the replies HERE. This loop is the last thing that can report them, and
        // `REPLIES` is process-wide, so a refusal left undispatched surfaces inside the NEXT
        // session and reads as its fault. Teardown is not latency-critical; a reply on this bus
        // has measured 1.4-2.4 ms, so 100 ms is many times over.
        for _ in 0..20 {
            bus.pump();
            if let Some(text) = ls2::REPLIES.take_failure() {
                tracing::warn!("DualSense feedback: Bluetooth service refused a report: {text}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seal is what the pad checks before it plays anything, and a wrong one is invisible:
    /// the Bluetooth service still answers `returnValue:true` for every dropped report.
    #[test]
    fn crc_matches_the_standard_check_value() {
        assert_eq!(crc32_le(*b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn an_audio_report_carries_its_sub_packets_where_the_pad_looks() {
        let coils = [[7i8, -7]; COIL_REPORT_FRAMES];
        let speaker = [0x5Au8; SPEAKER_FRAME_LEN];
        let r = build_audio_report(3, 9, &State::default(), &coils, &speaker);

        assert_eq!((r[0], r[1]), (0x36, 0x30));
        assert_eq!((r[2], r[3], r[4], r[10]), (0x91, 7, 0xFE, 9));
        assert!(r[5..10].iter().all(|&b| b == AUDIO_CONFIG));
        assert_eq!((r[11], r[12]), (0x90, 63));
        assert_eq!((r[76], r[77]), (0x92, 64));
        assert_eq!((r[78], r[79]), (7, 249));
        assert_eq!((r[142], r[143]), (0x93, SPEAKER_FRAME_LEN as u8));
        assert_eq!(&r[144..344], &speaker[..]);

        // Routing and volume ride EVERY report: the pad drops back to the jack otherwise.
        assert_eq!(r[13] & 0xA0, 0xA0);
        assert_eq!(r[14] & 0x80, 0x80);
        assert_eq!((r[18], r[20], r[50]), (SPEAKER_VOLUME, 0x30, 0x02));

        // One pass over the 0xA2 seed and every byte before the seal.
        let want = crc32_le(std::iter::once(0xA2).chain(r[..394].iter().copied()));
        assert_eq!(u32::from_le_bytes(r[394..].try_into().unwrap()), want);
    }

    #[test]
    fn a_coil_report_seals_the_same_way() {
        let frames = [[1i8, -1]; COIL_REPORT_FRAMES];
        let r = build_coil_report(1, 2, &frames);

        assert_eq!((r[0], r[1]), (0x32, 0x10));
        assert_eq!((r[2], r[3], r[4], r[9], r[10]), (0x91, 7, 0xFE, 0xFF, 2));
        assert_eq!((r[11], r[12], r[13], r[14]), (0x92, 64, 1, 255));

        let want = crc32_le(std::iter::once(0xA2).chain(r[..138].iter().copied()));
        assert_eq!(u32::from_le_bytes(r[138..].try_into().unwrap()), want);
    }

    /// The ratio IS the pad's clock: 512 in, 480 out per report. Endpoints pin the step.
    #[test]
    fn the_resample_spans_the_whole_input() {
        let mut pcm = [0f32; SPEAKER_IN_SAMPLES * 2];
        for i in 0..SPEAKER_IN_SAMPLES {
            pcm[i * 2] = i as f32;
            pcm[i * 2 + 1] = -(i as f32);
        }
        let mut out = vec![0f32; SPEAKER_OUT_SAMPLES * 2];
        resample_frame(&pcm, &mut out);

        let last = (SPEAKER_OUT_SAMPLES - 1) * 2;
        let end = (SPEAKER_IN_SAMPLES - 1) as f32;
        assert!(out[0].abs() < 1e-3 && out[1].abs() < 1e-3);
        assert!((out[last] - end).abs() < 1e-3);
        assert!((out[last + 1] + end).abs() < 1e-3);
    }

    /// A wired pad publishes the same `Uniq` as a paired one, so only `I: Bus=` tells them apart.
    /// Getting this wrong claims the coils for a transport that cannot carry them, and the pad
    /// ends up with no vibration at all: the lane is dead and the motors were handed to it.
    #[test]
    fn only_a_bluetooth_pad_has_an_address() {
        const USB: &str = "I: Bus=0003 Vendor=054c\nN: Name=\"Sony Interactive Entertainment DualSense Wireless Controller\"\nU: Uniq=14:3a:9a:1f:d1:64\n";
        const BT: &str =
            "I: Bus=0005 Vendor=054c\nN: Name=\"DualSense Wireless Controller\"\nU: Uniq=AA:BB:CC:DD:EE:FF\n";

        assert_eq!(address_in(USB), None, "a wired pad is not on the Bluetooth service");
        assert_eq!(address_in(BT), Some("aa:bb:cc:dd:ee:ff".into()));
        // Both attached: the paired one is the only one the bus can reach.
        assert_eq!(address_in(&format!("{USB}\n{BT}")), Some("aa:bb:cc:dd:ee:ff".into()));
    }

    /// The pad a generic binding is handling is the one `hid_playstation_bound` exists to find,
    /// and it is also the one least likely to be named `DualSense` — the kernel takes that name
    /// from the product string, which the pad reports as a plain "Wireless Controller". So the
    /// ids have to be enough on their own.
    #[test]
    fn a_pad_is_matched_by_its_ids_whatever_it_is_called() {
        const PLAIN: &str =
            "I: Bus=0005 Vendor=054c Product=0ce6 Version=8100\nN: Name=\"Wireless Controller\"\nU: Uniq=AA:BB:CC:DD:EE:FF\n";
        const EDGE: &str = "I: Bus=0005 Vendor=054c Product=0df2 Version=8100\nN: Name=\"Wireless Controller\"\nU: Uniq=11:22:33:44:55:66\n";
        const OTHER: &str = "I: Bus=0005 Vendor=045e Product=0b13 Version=0513\nN: Name=\"Xbox Wireless Controller\"\nU: Uniq=99:88:77:66:55:44\n";

        assert_eq!(address_in(PLAIN), Some("aa:bb:cc:dd:ee:ff".into()));
        assert_eq!(address_in(EDGE), Some("11:22:33:44:55:66".into()));
        assert_eq!(address_in(OTHER), None, "another vendor's pad is not a DualSense");
    }

    /// The overfeed that broke speech up: a tick that overran used to fire again immediately.
    #[test]
    fn an_overrun_tick_lands_in_the_future_on_the_pads_phase() {
        let tick = Instant::now();
        let now = tick + COIL_TICK * 5 + Duration::from_micros(300);
        let next = advance_tick(tick, now);

        assert!(next > now);
        let steps = (next - tick).as_nanos() / COIL_TICK.as_nanos();
        assert_eq!(steps, 6);
        assert_eq!(next - tick, COIL_TICK * 6);
    }
}
