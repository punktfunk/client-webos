//! The `0xD1` pad-audio plane on this client: the `DualSense` voice-coil ("haptics") and speaker
//! lanes the host captures from the game's own controller-audio endpoint.
//!
//! Rendering tiers, per the plan of record: tier A plays the coil lane on the pad's own coils —
//! here over Bluetooth, as `0x32` reports on the in-process Luna bus (`platform::webos::dualsense`)
//! — and tier C derives rumble from it. This module is the decode thread plus both renderers'
//! shared state, an [`Envelope`]: a 3 kHz ring the bus sender drains when it owns the coils, and
//! motor levels the main loop applies through the evdev rumble route otherwise. Without either a
//! libScePad title (Spider-Man, GTA V Enhanced…) produces **no** vibration at all on this client:
//! those games drive the coils, never the classic motors.
//!
//! Frames: kind 0 = coils, Opus 48 kHz stereo, 5 ms; kind 1 = speaker, Opus stereo, 10 ms. An
//! empty payload is the host's silence gate (deliberate silence, not loss). Lost frames are not
//! concealed here: a 5 ms hole in a rumble envelope is inaudible, and PLC is CPU this `SoC` lacks.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};

use crate::core::settings::TvSettings;
use pf_client_core::trust::Settings;

/// Arrival-flag / `set_pad_audio_caps` bit: this client renders the coil lane.
pub const CAP_HAPTICS: u8 = 0x01;
/// Same, for the speaker lane — declared only for a Bluetooth pad, the one transport that plays it.
pub const CAP_SPEAKER: u8 = 0x02;

/// Stereo 48 kHz samples the Bluetooth sender pulls per speaker report: 512, resampled to the
/// 480 the pad's 10 ms Opus frame holds — the pad plays one frame per 10.667 ms.
pub const SPEAKER_IN_SAMPLES: usize = 512;
/// Speaker ring bound, ~400 ms of stereo: a stalled sender drops the oldest.
const SPEAKER_RING_MAX: usize = 48_000 * 2 * 4 / 10;

/// Stereo 48 kHz; a 20 ms frame is the largest Opus can carry in one packet.
const MAX_FRAME_SAMPLES: usize = 960 * 2;

/// How long the derived rumble outlives the last coil frame. Past it the main loop sends one
/// explicit zero and hands the motors back to the wire rumble plane. Matches the host's own
/// silence-gate hangover, so a gated stream and a dead one look the same from here.
const HOLD: Duration = Duration::from_millis(250);

/// Per 5 ms frame: the envelope decays to ~10% in 40 ms. Attack is instant.
const RELEASE: f32 = 0.75;

/// The coils are a 3 kHz path: 48 kHz decimated by this (`dualsense-bluetooth-audio.md` §2.3).
const COIL_DECIMATION: usize = 16;
/// Stereo 3 kHz frames per Bluetooth coil report: 32 = 10.667 ms, the pad's own cadence.
pub const COIL_REPORT_FRAMES: usize = 32;
/// Ring bound, ~64 ms: a stalled sender drops the oldest rather than growing.
const COIL_RING_MAX: usize = COIL_REPORT_FRAMES * 6;

/// Fewest milliseconds between two motor writes. Each becomes an output report on the pad's
/// link, which the coil lane will share once tier A exists — 60 Hz is plenty for a motor.
const APPLY_INTERVAL: Duration = Duration::from_millis(16);

/// The render capabilities to declare, from Settings. `bt_pad` is whether a Bluetooth `DualSense` is
/// attached: the speaker lane has no other route, and a declared-but-silent lane would make the
/// host stream it for nothing.
pub fn caps_for(settings: &Settings, bt_pad: bool) -> u8 {
    let mut caps = 0;
    if settings.pad_haptics {
        caps |= CAP_HAPTICS;
    }
    if settings.pad_speaker_on() && bt_pad {
        caps |= CAP_SPEAKER;
    }
    caps
}

/// What the decode thread publishes and the main loop applies to the motors.
pub struct Envelope {
    /// `low << 16 | high`, each 0..=65535, quantised so a steady tone is not re-applied.
    levels: AtomicU32,
    /// Milliseconds since `epoch` of the last coil frame; `u64::MAX` before the first.
    last_frame_ms: AtomicU64,
    epoch: Instant,
    /// Main-loop state: the pair last written to the pad, and when — `u64::MAX` before the first
    /// write, like `last_frame_ms`. A zero here would read as "applied at the envelope's epoch"
    /// and swallow the first level for `APPLY_INTERVAL`.
    applied: AtomicU32,
    applied_at_ms: AtomicU64,
    /// Whether the motors currently hold a derived level (so expiry sends one zero, not many).
    owning: AtomicBool,
    pub frames: AtomicU32,
    pub speaker_frames: AtomicU32,
    /// Tier A: decimated coil samples waiting for the Bluetooth sender.
    coils: Mutex<VecDeque<[i8; 2]>>,
    /// Decoded speaker PCM (interleaved stereo 48 kHz) waiting for the Bluetooth sender.
    speaker: Mutex<VecDeque<f32>>,
    /// Milliseconds since `epoch` of the last speaker frame; `u64::MAX` before the first.
    last_speaker_ms: AtomicU64,
    /// Set by the sender once it has a bus: from then on the coils play the lane and the motor
    /// envelope stays idle. Never cleared — a bus that opened does not go away mid-session.
    coils_owned: AtomicBool,
    /// Coil PCM at full rate, for a wired pad. Its card takes 48 kHz on all four channels, so the
    /// 3 kHz decimation the `0x32` report needs would only throw the lane away.
    coils_pcm: Mutex<VecDeque<f32>>,
    /// Which of the two rings [`Envelope::push_coils_frame`] fills.
    usb_pcm: AtomicBool,
}

impl Envelope {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            levels: AtomicU32::new(0),
            last_frame_ms: AtomicU64::new(u64::MAX),
            epoch: Instant::now(),
            applied: AtomicU32::new(0),
            applied_at_ms: AtomicU64::new(u64::MAX),
            owning: AtomicBool::new(false),
            frames: AtomicU32::new(0),
            speaker_frames: AtomicU32::new(0),
            coils: Mutex::new(VecDeque::with_capacity(COIL_RING_MAX)),
            speaker: Mutex::new(VecDeque::with_capacity(SPEAKER_RING_MAX)),
            last_speaker_ms: AtomicU64::new(u64::MAX),
            coils_owned: AtomicBool::new(false),
            coils_pcm: Mutex::new(VecDeque::with_capacity(SPEAKER_RING_MAX)),
            usb_pcm: AtomicBool::new(false),
        })
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Whether coil frames are arriving: while true the wire rumble plane must not touch the
    /// motors, or a title's classic rumble (the host still forwards it) fights its own haptics.
    pub fn active(&self) -> bool {
        let last = self.last_frame_ms.load(Ordering::Relaxed);
        last != u64::MAX && self.now_ms().saturating_sub(last) < HOLD.as_millis() as u64
    }

    /// The motor pair the main loop should write now, if any: `Some((low, high))` for a changed
    /// level (rate-limited), `Some((0, 0))` once when the envelope expires, `None` otherwise.
    pub fn take_change(&self) -> Option<(u16, u16)> {
        if self.coils_owned() {
            return None;
        }
        let now = self.now_ms();
        if !self.active() {
            return self.owning.swap(false, Ordering::Relaxed).then(|| {
                self.applied.store(0, Ordering::Relaxed);
                (0, 0)
            });
        }
        let levels = self.levels.load(Ordering::Relaxed);
        if levels == self.applied.load(Ordering::Relaxed) {
            return None;
        }
        let applied_at = self.applied_at_ms.load(Ordering::Relaxed);
        if applied_at != u64::MAX && now.saturating_sub(applied_at) < APPLY_INTERVAL.as_millis() as u64 {
            return None;
        }
        self.applied.store(levels, Ordering::Relaxed);
        self.applied_at_ms.store(now, Ordering::Relaxed);
        self.owning.store(true, Ordering::Relaxed);
        Some(((levels >> 16) as u16, (levels & 0xFFFF) as u16))
    }

    /// The Bluetooth sender claims the lane (see `coils_owned`).
    pub fn own_coils(&self) {
        self.coils_owned.store(true, Ordering::Relaxed);
    }

    pub fn coils_owned(&self) -> bool {
        self.coils_owned.load(Ordering::Relaxed)
    }

    /// One report's worth of coil frames, zero-filled past what has arrived. `false` when the ring
    /// was empty — the sender keeps the cadence through the hold window on `active()` alone.
    pub fn take_coils(&self, out: &mut [[i8; 2]; COIL_REPORT_FRAMES]) -> bool {
        let mut ring = self.coils.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let had = !ring.is_empty();
        for slot in out.iter_mut() {
            *slot = ring.pop_front().unwrap_or([0, 0]);
        }
        had
    }

    /// Whether speaker frames are arriving (within the hold window).
    pub fn speaker_active(&self) -> bool {
        let last = self.last_speaker_ms.load(Ordering::Relaxed);
        last != u64::MAX && self.now_ms().saturating_sub(last) < HOLD.as_millis() as u64
    }

    /// Stereo samples queued for the speaker lane, so the sender can pre-fill the pad's buffer.
    pub fn speaker_queued(&self) -> usize {
        self.speaker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            / 2
    }

    /// One report's worth of speaker PCM (`SPEAKER_IN_SAMPLES` stereo samples), or `false` when
    /// fewer have arrived — the sender then sends a silent frame rather than a short one.
    pub fn take_speaker(&self, out: &mut [f32; SPEAKER_IN_SAMPLES * 2]) -> bool {
        let mut ring = self.speaker.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if ring.len() < out.len() {
            return false;
        }
        for v in out.iter_mut() {
            *v = ring.pop_front().unwrap_or(0.0);
        }
        true
    }

    fn push_speaker(&self, pcm: &[f32]) {
        let mut ring = self.speaker.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let overflow = (ring.len() + pcm.len())
            .saturating_sub(SPEAKER_RING_MAX)
            .min(ring.len());
        if overflow > 0 {
            ring.drain(..overflow);
        }
        ring.extend(pcm.iter().copied());
        self.last_speaker_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    /// The wired pad's audio card claims both lanes: the coils play at full rate on it, so the
    /// motor envelope idles exactly as it does for the Bluetooth lane — the kernel's rumble report
    /// sets `HAPTICS_SELECT` on either transport, which would mute the coils it is competing with.
    pub fn own_usb(&self) {
        self.usb_pcm.store(true, Ordering::Relaxed);
        self.coils_owned.store(true, Ordering::Relaxed);
    }

    /// One decoded coil frame into whichever ring the live transport reads.
    fn push_coils_frame(&self, pcm: &[f32]) {
        if self.usb_pcm.load(Ordering::Relaxed) {
            push_pcm(&self.coils_pcm, pcm);
        } else {
            self.push_coils(pcm);
        }
    }

    /// Up to `out.len()` samples of coil PCM, zero-filled past what has arrived. Unlike the
    /// Bluetooth lane there is no silent-frame substitute: the card wants a chunk every time.
    pub fn take_coils_pcm(&self, out: &mut [f32]) {
        drain_pcm(&self.coils_pcm, out);
    }

    /// Same for the speaker lane, on the wired transport.
    pub fn take_speaker_pcm(&self, out: &mut [f32]) {
        drain_pcm(&self.speaker, out);
    }

    /// Decimates one decoded coil frame (interleaved stereo 48 kHz) into the ring.
    fn push_coils(&self, pcm: &[f32]) {
        let mut ring = self.coils.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for block in pcm.chunks_exact(COIL_DECIMATION * 2) {
            let (mut l, mut r) = (0f32, 0f32);
            for s in block.chunks_exact(2) {
                l += s[0];
                r += s[1];
            }
            let q = |v: f32| (v / COIL_DECIMATION as f32 * 127.0).round().clamp(-127.0, 127.0) as i8;
            if ring.len() >= COIL_RING_MAX {
                ring.pop_front();
            }
            ring.push_back([q(l), q(r)]);
        }
    }

    fn publish(&self, low: f32, high: f32) {
        // 64 steps: below what a motor resolves, and what makes a held tone a single write.
        let q = |v: f32| ((v.clamp(0.0, 1.0) * 63.0).round() as u32) * 1040;
        self.levels.store((q(low) << 16) | q(high), Ordering::Relaxed);
        self.last_frame_ms.store(self.now_ms(), Ordering::Relaxed);
    }
}

/// Spawns the decode thread. Ends on `stop` (set at teardown before the join).
pub fn spawn(
    client: Arc<NativeClient>,
    stop: Arc<AtomicBool>,
    envelope: Arc<Envelope>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("punktfunk-webos-pad-audio".into())
        .spawn(move || {
            if let Err(e) = pump(&client, &stop, &envelope) {
                tracing::warn!("pad audio thread ended: {e:#}");
            }
        })
        .context("spawn pad audio thread")
}

fn pump(client: &NativeClient, stop: &AtomicBool, envelope: &Envelope) -> Result<()> {
    let mut coils =
        opus::Decoder::new(48_000, opus::Channels::Stereo).map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
    let mut speaker =
        opus::Decoder::new(48_000, opus::Channels::Stereo).map_err(|e| anyhow::anyhow!("opus decoder: {e}"))?;
    let mut pcm = vec![0f32; MAX_FRAME_SAMPLES];
    // Left coil → low (heavy) motor, right coil → high (light) motor: the pad's own left/right
    // split, and the mapping every tier-C client in the plan uses.
    let (mut low, mut high) = (0f32, 0f32);
    let mut logged_first = false;
    while !stop.load(Ordering::Relaxed) {
        let Some(frame) = client.next_pad_audio(Duration::from_millis(50)) else {
            continue;
        };
        if frame.pad != 0 {
            continue;
        }
        match frame.kind {
            PAD_AUDIO_KIND_HAPTICS => {
                let n = envelope.frames.fetch_add(1, Ordering::Relaxed) + 1;
                if !logged_first {
                    logged_first = true;
                    tracing::info!("pad audio: first coil frame (seq {}) — rendering as rumble", frame.seq);
                }
                let (peak_l, peak_r) = if frame.opus.is_empty() {
                    (0.0, 0.0)
                } else {
                    match coils.decode_float(&frame.opus, &mut pcm, false) {
                        Ok(samples) => {
                            if envelope.coils_owned() {
                                envelope.push_coils_frame(&pcm[..samples * 2]);
                            }
                            peaks(&pcm[..samples * 2])
                        }
                        Err(e) => {
                            tracing::debug!("pad audio: coil decode failed: {e}");
                            (0.0, 0.0)
                        }
                    }
                };
                low = (low * RELEASE).max(peak_l);
                high = (high * RELEASE).max(peak_r);
                envelope.publish(low, high);
                if n % 2000 == 0 {
                    tracing::debug!(
                        "pad audio: {n} coil frames, {} speaker frames, level {:.2}/{:.2}",
                        envelope.speaker_frames.load(Ordering::Relaxed),
                        low,
                        high
                    );
                }
            }
            PAD_AUDIO_KIND_SPEAKER => {
                let n = envelope.speaker_frames.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 {
                    tracing::info!("pad audio: first speaker frame (seq {})", frame.seq);
                }
                // Only the Bluetooth sender plays this lane; without it the frames are counted
                // and dropped (the lane is not declared then, so this is the rare race at start).
                if envelope.coils_owned() && !frame.opus.is_empty() {
                    match speaker.decode_float(&frame.opus, &mut pcm, false) {
                        Ok(samples) => envelope.push_speaker(&pcm[..samples * 2]),
                        Err(e) => tracing::debug!("pad audio: speaker decode failed: {e}"),
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// Appends to a PCM ring, dropping the oldest past [`SPEAKER_RING_MAX`]: a stalled reader costs
/// latency, never unbounded memory.
fn push_pcm(ring: &Mutex<VecDeque<f32>>, pcm: &[f32]) {
    let mut ring = ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let overflow = (ring.len() + pcm.len())
        .saturating_sub(SPEAKER_RING_MAX)
        .min(ring.len());
    if overflow > 0 {
        ring.drain(..overflow);
    }
    ring.extend(pcm.iter().copied());
}

/// Fills `out` from a PCM ring, zero-filling the tail when the lane is quiet.
fn drain_pcm(ring: &Mutex<VecDeque<f32>>, out: &mut [f32]) {
    let mut ring = ring.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for v in out.iter_mut() {
        *v = ring.pop_front().unwrap_or(0.0);
    }
}

/// Peak absolute sample per channel of interleaved stereo.
fn peaks(pcm: &[f32]) -> (f32, f32) {
    pcm.chunks_exact(2)
        .fold((0f32, 0f32), |(l, r), s| (l.max(s[0].abs()), r.max(s[1].abs())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_applies_once_then_releases_once() {
        let env = Envelope::new();
        assert_eq!(env.take_change(), None, "nothing before the first frame");
        env.publish(0.5, 0.0);
        let first = env.take_change().expect("a changed level is applied");
        assert!(first.0 > 30_000 && first.1 == 0);
        assert_eq!(env.take_change(), None, "a steady level is not re-written");
        // Expiry: force the last frame into the past.
        env.last_frame_ms.store(0, Ordering::Relaxed);
        std::thread::sleep(HOLD + Duration::from_millis(20));
        assert_eq!(env.take_change(), Some((0, 0)), "one explicit zero on expiry");
        assert_eq!(env.take_change(), None, "and only one");
    }

    #[test]
    fn coil_ring_decimates_and_zero_fills() {
        let env = Envelope::new();
        env.own_coils();
        // 240 stereo samples (one 5 ms frame) of a constant 0.5 left, -0.25 right → 15 frames.
        let pcm: Vec<f32> = (0..240).flat_map(|_| [0.5f32, -0.25]).collect();
        env.push_coils(&pcm);
        let mut out = [[0i8; 2]; COIL_REPORT_FRAMES];
        assert!(env.take_coils(&mut out));
        assert_eq!(out[0], [64, -32]);
        assert_eq!(out[14], [64, -32]);
        assert_eq!(out[15], [0, 0], "zero-filled past the 15 frames that arrived");
        assert!(!env.take_coils(&mut out), "drained");
        assert_eq!(env.take_change(), None, "owned coils keep the motor envelope idle");
    }

    #[test]
    fn peaks_split_channels() {
        assert_eq!(peaks(&[0.1, -0.9, -0.3, 0.2]), (0.3, 0.9));
    }
}
