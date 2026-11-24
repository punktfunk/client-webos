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

/// Releases the uplink when the capture thread ends, whichever way it ends.
struct Claim;

impl Drop for Claim {
    fn drop(&mut self) {
        TAKEN.store(false, Ordering::Release);
    }
}

/// Starts capturing pad `pad`'s microphone on `card` onto the session's mic uplink, or `None`
/// when another pad already feeds it or the thread cannot start. A card that refuses capture is
/// logged by the thread and ends it.
pub fn spawn(
    client: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    card: u32,
    pad: u8,
) -> Option<std::thread::JoinHandle<()>> {
    if TAKEN.swap(true, Ordering::AcqRel) {
        return None;
    }
    let spawned = std::thread::Builder::new().name("pad-mic".into()).spawn(move || {
        let _claim = Claim;
        capture(&client, &stop, card, pad);
    });
    match spawned {
        Ok(thread) => Some(thread),
        Err(_) => {
            TAKEN.store(false, Ordering::Release);
            None
        }
    }
}

fn capture(client: &NativeClient, stop: &AtomicBool, card: u32, pad: u8) {
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
    let mut buf = vec![0i16; FRAME_SAMPLES * source.channels];
    while !stop.load(Ordering::Relaxed) {
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
        mic.drain(false, |seq, pts, packet| {
            let _ = client.send_mic(seq, pts, packet.to_vec());
        });
    }
    tracing::info!("pad {pad}: microphone off after {} frames", mic.sent);
}

#[cfg(test)]
mod tests {
    use super::to_mono;

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
