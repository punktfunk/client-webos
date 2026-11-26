//! A wired `DualSense`'s microphone, captured off its own audio card and sent on the mic uplink.
//!
//! The pad's UAC card has a capture side next to the playback one [`super::usb_audio`] writes: a
//! real pad's stereo mic pair, or the mono "Headset Microphone" a `DS5Dongle` exposes for a pad on
//! Bluetooth behind it. `PulseAudio` mints no source for it, so it is opened directly, like the
//! playback side. The host already runs a virtual microphone for every session (`0xCB`); this is
//! only the client half that was missing — `mic=0` in the host's input summary.
//!
//! One pad feeds the uplink at a time: the host decodes a single mono stream, and two encoders
//! with their own `seq` would interleave into noise.
//!
//! The pad's mute button is this lane's to answer. Nothing on the host acts on it — a PS5 mutes
//! in its own system software, and Windows has no equivalent — so a press toggles the mute here:
//! frames stop going out, and the pad's mute LED and hardware mic mute follow, set over its
//! hidraw node the way `hid-playstation` sets them.

use std::ffi::{c_char, c_int, c_ulong, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{bail, Context, Result};
use punktfunk_core::audio::mic::{MicEncoder, FRAME_SAMPLES};
use punktfunk_core::client::NativeClient;

use super::dl;

const ASOUND_LIB: &CStr = c"libasound.so.2";

/// `SND_PCM_STREAM_CAPTURE`.
const STREAM_CAPTURE: c_int = 1;
/// `SND_PCM_FORMAT_S16_LE`.
const FORMAT_S16_LE: c_int = 2;
/// `SND_PCM_ACCESS_RW_INTERLEAVED`.
const ACCESS_RW_INTERLEAVED: c_int = 3;
const RATE: u32 = 48_000;
/// What the card is asked to buffer: a mic frame's worth twice over, so a late read does not
/// overrun, and short enough that the voice is not noticeably behind.
const LATENCY_US: u32 = 20_000;

/// USB input report `0x01`: the byte holding the mute button (report id included), and its bit.
const INPUT_REPORT_ID: u8 = 0x01;
const MUTE_BUTTON_AT: usize = 10;
const MUTE_BUTTON_BIT: u8 = 0x04;

/// USB output report `0x02`, laid out as `dualsense::build_usb_report`'s common block: the second
/// valid-flag byte, the mute LED and the power-save byte whose bit 4 mutes the mic in hardware.
const OUTPUT_REPORT_LEN: usize = 63;
const OUT_VALID_FLAG1: usize = 2;
const FLAG1_MUTE_LED: u8 = 0x01;
const FLAG1_POWER_SAVE: u8 = 0x02;
const OUT_MUTE_LED: usize = 9;
const OUT_POWER_SAVE: usize = 10;
const POWER_SAVE_MIC_MUTE: u8 = 0x10;

/// Whether a pad already feeds the uplink this session.
static TAKEN: AtomicBool = AtomicBool::new(false);

type Pcm = *mut c_void;

struct Fns {
    open: unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(Pcm, c_int, c_int, u32, u32, c_int, u32) -> c_int,
    readi: unsafe extern "C" fn(Pcm, *mut c_void, c_ulong) -> isize,
    prepare: unsafe extern "C" fn(Pcm) -> c_int,
    close: unsafe extern "C" fn(Pcm) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
}

fn fns() -> Result<&'static Fns> {
    static FNS: OnceLock<std::result::Result<Fns, String>> = OnceLock::new();
    dl::cached(&FNS, ASOUND_LIB, |lib| {
        Ok(Fns {
            open: lib.sym(c"snd_pcm_open")?,
            set_params: lib.sym(c"snd_pcm_set_params")?,
            readi: lib.sym(c"snd_pcm_readi")?,
            prepare: lib.sym(c"snd_pcm_prepare")?,
            close: lib.sym(c"snd_pcm_close")?,
            strerror: lib.sym(c"snd_strerror")?,
        })
    })
}

/// ALSA's own text for a negative return code.
fn describe(fns: &Fns, rc: c_int) -> String {
    // SAFETY: `snd_strerror` returns a static NUL-terminated string for any input.
    let msg = unsafe { (fns.strerror)(rc) };
    if msg.is_null() {
        return format!("error {rc}");
    }
    // SAFETY: non-null and NUL-terminated, owned by the library.
    unsafe { CStr::from_ptr(msg) }.to_string_lossy().into_owned()
}

/// An open capture stream on a pad's card.
struct PadSource {
    pcm: Pcm,
    fns: &'static Fns,
    channels: usize,
    name: String,
}

impl PadSource {
    /// The first device shape the card accepts: mono through ALSA's converting `plughw` (which
    /// downmixes a real pad's stereo pair), then the raw device as mono (`DS5Dongle`) or stereo.
    fn open(card: u32) -> Result<Self> {
        let fns = fns().context("libasound")?;
        let mut last = String::new();
        for (device, channels) in [("plughw", 1), ("hw", 1), ("hw", 2)] {
            let name = format!("{device}:{card},0");
            match Self::try_open(fns, &name, channels) {
                Ok(source) => return Ok(source),
                Err(e) => last = format!("{e:#}"),
            }
        }
        bail!("no capture shape accepted on card {card}: {last}")
    }

    fn try_open(fns: &'static Fns, name: &str, channels: usize) -> Result<Self> {
        let c_name = CString::new(name)?;
        let mut pcm: Pcm = std::ptr::null_mut();
        // SAFETY: `c_name` is NUL-terminated and outlives the call; `pcm` is a live local.
        let rc = unsafe { (fns.open)(&raw mut pcm, c_name.as_ptr(), STREAM_CAPTURE, 0) };
        if rc < 0 || pcm.is_null() {
            bail!("snd_pcm_open({name}): {}", describe(fns, rc));
        }
        let source = Self {
            pcm,
            fns,
            channels,
            name: name.to_string(),
        };
        // SAFETY: `pcm` is the handle just opened, live until `source` is dropped.
        let rc = unsafe {
            (fns.set_params)(
                source.pcm,
                FORMAT_S16_LE,
                ACCESS_RW_INTERLEAVED,
                channels as u32,
                RATE,
                1,
                LATENCY_US,
            )
        };
        if rc < 0 {
            bail!("snd_pcm_set_params({name}, {channels} ch): {}", describe(fns, rc));
        }
        Ok(source)
    }

    /// Reads whole frames into `buf` (interleaved), returning how many frames arrived. An
    /// overrun is recovered here and reads as zero frames.
    fn read(&self, buf: &mut [i16]) -> Result<usize> {
        let frames = buf.len() / self.channels;
        // SAFETY: `buf` holds `frames` whole interleaved frames; the handle is live.
        let n = unsafe { (self.fns.readi)(self.pcm, buf.as_mut_ptr().cast(), frames as c_ulong) };
        if let Ok(n) = usize::try_from(n) {
            return Ok(n);
        }
        let rc = n as c_int;
        match -rc {
            libc::EINTR | libc::EAGAIN => Ok(0),
            libc::EPIPE => {
                // SAFETY: the handle is live; prepare restarts a stream after an overrun.
                let rc = unsafe { (self.fns.prepare)(self.pcm) };
                if rc < 0 {
                    bail!("snd_pcm_prepare: {}", describe(self.fns, rc));
                }
                Ok(0)
            }
            _ => bail!("snd_pcm_readi: {}", describe(self.fns, rc)),
        }
    }
}

impl Drop for PadSource {
    fn drop(&mut self) {
        // SAFETY: `pcm` came from `snd_pcm_open` and is closed exactly once.
        unsafe { (self.fns.close)(self.pcm) };
    }
}

/// Folds interleaved `channels`-wide frames to mono floats.
fn to_mono(interleaved: &[i16], channels: usize) -> impl Iterator<Item = f32> + '_ {
    interleaved.chunks_exact(channels).map(move |frame| {
        let sum: i32 = frame.iter().map(|&s| i32::from(s)).sum();
        sum as f32 / (channels as f32 * 32768.0)
    })
}

/// The pad's hidraw node, read for the mute button and written for its LED. Every reader of a
/// hidraw node gets every report, so this takes nothing from the other readers.
struct MuteButton {
    fd: libc::c_int,
    held: bool,
}

impl MuteButton {
    fn open(path: &str) -> Option<Self> {
        let c_path = CString::new(path).ok()?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call.
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        (fd >= 0).then_some(Self { fd, held: false })
    }

    /// Reads every queued report; `true` if the button went down since the last call.
    fn pressed(&mut self) -> bool {
        let mut buf = [0u8; 64];
        let mut pressed = false;
        loop {
            // SAFETY: `buf` is a live local of the length passed; the descriptor is open.
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            let Ok(len) = usize::try_from(n) else { break };
            if len == 0 {
                break;
            }
            if let Some(held) = mute_held(&buf[..len]) {
                pressed |= held && !self.held;
                self.held = held;
            }
        }
        pressed
    }

    /// Sets the pad's mute LED and hardware mic mute to `muted`.
    fn show(&self, muted: bool) {
        let report = mute_report(muted);
        // SAFETY: `report` is a live local; the descriptor is open.
        let n = unsafe { libc::write(self.fd, report.as_ptr().cast(), report.len()) };
        if usize::try_from(n).ok() != Some(report.len()) {
            tracing::debug!("pad mic: mute LED report not written");
        }
    }
}

impl Drop for MuteButton {
    fn drop(&mut self) {
        // SAFETY: the descriptor came from `open` and is closed exactly once.
        unsafe { libc::close(self.fd) };
    }
}

/// Whether the mute button is held in a USB input report, or `None` for any other report.
fn mute_held(report: &[u8]) -> Option<bool> {
    (report.len() > MUTE_BUTTON_AT && report[0] == INPUT_REPORT_ID)
        .then(|| report[MUTE_BUTTON_AT] & MUTE_BUTTON_BIT != 0)
}

/// The output report that sets only the mute LED and the hardware mic mute.
fn mute_report(muted: bool) -> [u8; OUTPUT_REPORT_LEN] {
    let mut r = [0u8; OUTPUT_REPORT_LEN];
    r[0] = 0x02;
    r[OUT_VALID_FLAG1] = FLAG1_MUTE_LED | FLAG1_POWER_SAVE;
    r[OUT_MUTE_LED] = u8::from(muted);
    r[OUT_POWER_SAVE] = if muted { POWER_SAVE_MIC_MUTE } else { 0 };
    r
}

/// Releases the uplink when the capture thread ends, whichever way it ends.
struct Claim;

impl Drop for Claim {
    fn drop(&mut self) {
        TAKEN.store(false, Ordering::Release);
    }
}

/// Starts capturing pad `pad`'s microphone on `card` onto the session's mic uplink, its mute
/// button read off the hidraw node at `node`, or `None` when another pad already feeds the uplink
/// or the thread cannot start. A card that refuses capture is logged by the thread and ends it.
pub fn spawn(
    client: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    card: u32,
    node: String,
    pad: u8,
) -> Option<std::thread::JoinHandle<()>> {
    if TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    let spawned = std::thread::Builder::new().name("pad-mic".into()).spawn(move || {
        let _claim = Claim;
        capture(&client, &stop, card, &node, pad);
    });
    match spawned {
        Ok(thread) => Some(thread),
        Err(_) => {
            TAKEN.store(false, Ordering::Release);
            None
        }
    }
}

fn capture(client: &NativeClient, stop: &AtomicBool, card: u32, node: &str, pad: u8) {
    let source = match PadSource::open(card) {
        Ok(source) => source,
        Err(e) => {
            tracing::info!("pad {pad}: no microphone on the pad's card ({e:#})");
            return;
        }
    };
    let mut mic = match MicEncoder::new(false) {
        Ok(mic) => mic,
        Err(e) => {
            tracing::warn!("pad {pad}: mic encoder unavailable: {e}");
            return;
        }
    };
    tracing::info!(
        "pad {pad}: microphone captured from the pad's card ({}, {} ch)",
        source.name,
        source.channels
    );
    let mut button = MuteButton::open(node);
    if button.is_none() {
        tracing::info!("pad {pad}: no hidraw node for the mic's mute button");
    }
    let mut muted = false;
    if let Some(button) = &button {
        button.show(muted);
    }
    let mut buf = vec![0i16; FRAME_SAMPLES * source.channels];
    while !stop.load(Ordering::Relaxed) {
        if button.as_mut().is_some_and(MuteButton::pressed) {
            muted = !muted;
            if let Some(button) = &button {
                button.show(muted);
            }
            tracing::info!("pad {pad}: microphone {}", if muted { "muted" } else { "live" });
        }
        let frames = match source.read(&mut buf) {
            Ok(frames) => frames,
            Err(e) => {
                tracing::warn!("pad {pad}: microphone stopped: {e:#}");
                break;
            }
        };
        if frames == 0 {
            continue;
        }
        mic.push(to_mono(&buf[..frames * source.channels], source.channels));
        mic.drain(muted, |seq, pts, packet| {
            let _ = client.send_mic(seq, pts, packet.to_vec());
        });
    }
    // A mute this lane set is not left on the pad for whatever uses it next.
    if let (Some(button), true) = (&button, muted) {
        button.show(false);
    }
    tracing::info!("pad {pad}: microphone off after {} frames", mic.sent);
}

#[cfg(test)]
mod tests {
    use super::{mute_held, mute_report, to_mono, MUTE_BUTTON_AT};

    #[test]
    fn the_mute_button_is_read_off_input_reports_only() {
        let mut report = [0u8; 64];
        report[0] = 0x01;
        assert_eq!(mute_held(&report), Some(false));
        report[MUTE_BUTTON_AT] = 0x04;
        assert_eq!(mute_held(&report), Some(true));
        report[0] = 0x31;
        assert_eq!(mute_held(&report), None);
    }

    #[test]
    fn the_mute_report_sets_only_the_led_and_mic_mute() {
        let on = mute_report(true);
        assert_eq!((on[0], on[1], on[2], on[9], on[10]), (0x02, 0, 0x03, 1, 0x10));
        let off = mute_report(false);
        assert_eq!((off[9], off[10]), (0, 0));
    }

    #[test]
    fn a_stereo_pair_folds_to_its_mean() {
        let mono: Vec<f32> = to_mono(&[16384, -16384, 32767, 32767], 2).collect();
        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 1e-6);
        assert!((mono[1] - 32767.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn mono_passes_through_scaled() {
        let mono: Vec<f32> = to_mono(&[-32768, 0], 1).collect();
        assert_eq!(mono, [-1.0, 0.0]);
    }
}
