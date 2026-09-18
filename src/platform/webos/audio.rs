//! The TV's own audio device, and the sink wrapper the pipeline drives it through.
//!
//! One of two routes (`core::model::AudioRoutePref`) and the longer of them: software Opus
//! decode (`session::audio`) into a ring, drained by SDL's audio callback, through whatever
//! `PulseAudio` adds behind it. It is also the only one whose pacing behaviour is proven on this
//! hardware, hence the default — the offload route is shorter, but the TV's own decoder is behind
//! the plane and some sets accept its load and then play nothing.
//!
//! The ring is served by `punktfunk_core::audio::JitterPolicy`, the same de-jitter state machine
//! the Linux, Windows, Android and Apple rings run. That is deliberate and was re-decided once:
//! the policy was removed on this branch for latency, and the numbers did not support it — the
//! preset's base target is 25 ms and the fixed prime that replaced it was also 25 ms, so no floor
//! was ever saved, while the adaptive floor and the crossfaded shed were lost. What the policy buys
//! that a fixed prime cannot:
//!
//! * **An adaptive floor.** The target grows only on a set that actually underruns, instead of
//!   every set pre-paying for the worst one — and a set that needs more than 25 ms now gets it
//!   rather than re-priming into the same dropout forever.
//! * **A crossfaded shed.** Drift (host capture clock vs. this DAC) is walked back to target one
//!   5 ms frame at a time, faded. The alternative this replaces is an uncrossfaded 65 ms drop,
//!   which is an audible click.
//! * **Near-miss growth, hollow de-priming and a faded hard trim**, all of which core gained after
//!   this client last used it.
//!
//! The A/V sync loop is deliberately NOT wired: `set_sync_target` is never called, which core
//! documents as reproducing unsynchronised behaviour exactly. It never steered anything here — it
//! was gated behind an `$HOME/av-trim-ms.conf` that had to be measured on hardware first — and the
//! video reference this platform can build is biased low by NDL's unobservable decode+panel term.
//! See `docs/NOTES.md` § "A/V sync".
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;

use anyhow::Result;
use punktfunk_core::audio::{crossfade_drop, JitterPolicy, JitterTuning};

use crate::core::media::{AudioFormat, AudioSink, Samples};
use crate::session::audio::SAMPLE_RATE;

/// Device buffer: 512 frames (10.67 ms). Deliberately not smaller — a smaller quantum on a 2-3
/// core TV `SoC` buys more wakeups and more missed callbacks, not less latency (docs/NOTES.md).
///
/// Set as a hint before the device opens: SDL3's `AudioSpec` carries no quantum.
const DEVICE_BUFFER_FRAMES: u16 = 512;

/// Chunks in flight between the decode thread and the callback. 5 ms each.
const CHUNK_QUEUE: usize = 64;

/// Pre-allocate to satisfy both the policy cap and device period, accounting for chunks in flight.
fn ring_capacity(device_samples: u32, channels: u8) -> usize {
    const SAMPLES_PER_MS: usize = SAMPLE_RATE as usize / 1000;
    const CHUNK_MS: usize = 5;
    let period_ms = device_samples as usize / SAMPLES_PER_MS + CHUNK_MS;
    let depth_ms = (TUNING.hard_cap_ms as usize).max(period_ms) + CHUNK_QUEUE * CHUNK_MS;
    depth_ms * SAMPLES_PER_MS * usize::from(channels)
}

/// De-jitter tuning: core's Android preset, unmodified.
///
/// It is the right one on the merits rather than by convenience — `AAudio` hands the client a raw
/// callback and makes it own the buffer, and Wi-Fi power-save bunching lands as underruns, which is
/// exactly this TV's situation on a radio `docs/NOTES.md` measures with 10-29 s black-hole stalls
/// on new flows. It is also, field for field, what this client's own local preset used to be
/// (`base 25 / max 90 / headroom 40 / cap 120`), with the old `deprime_after: 5` **callbacks** now
/// expressed as `deprime_ms: 60` — core moved that fuse to milliseconds precisely because a
/// callback count means a different span of time on every device. So there is nothing left for a
/// local copy to say, and a preset that tracks upstream is one fewer thing to re-tune by hand.
///
/// Two invariants worth re-checking if it is ever forked, both of which the parent programme
/// shipped broken once:
/// * The smooth shed must fire strictly below the hard trim, or drift correction is dead code.
///   `shed_excess_ms()` is `max(headroom/2, 2 × FRAME_MS)` = 20 ms, so the shed point is
///   target+20 = 45 ms against a trim at `min(target+40, 120)` = 65 ms. 45 < 65. ✓
/// * The base target must clear the device floor. `effective_target` floors at `want + FRAME_MS`;
///   at [`DEVICE_BUFFER_FRAMES`] that is 10.67 + 5 = 15.7 ms, under the 25 ms base — so the ring
///   cannot oscillate prime → dropout → re-prime. ✓
const TUNING: JitterTuning = JitterTuning::AAUDIO;

/// The playback device. Holding it alive is the whole job — the ring drains on SDL's own
/// audio thread from construction until this is dropped. See the module docs for when it is used
/// at all.
pub struct AudioPlayer {
    /// Owns playback and the callback state; `Drop` first disconnects SDL's callback.
    stream: sdl3::audio::AudioStreamWithCallback<RingCallback>,
}

impl AudioPlayer {
    /// Opens the device and returns it alongside the [`SdlAudioSink`] that fills it. `channels` is
    /// host-resolved (the pipeline MUST build its decoder from this, never from its own request).
    /// `buffer_ms` is where the ring publishes its depth for the stats overlay.
    ///
    /// The two halves are returned separately because they belong on different threads: the device
    /// stays wherever SDL was initialised, and the sink moves to the audio thread.
    pub fn new(
        sdl_audio: &sdl3::AudioSubsystem,
        channels: u8,
        buffer_ms: Arc<AtomicU32>,
    ) -> Result<(Self, SdlAudioSink)> {
        // Before the open, not part of it: see [`DEVICE_BUFFER_FRAMES`].
        sdl3::hint::set("SDL_AUDIO_DEVICE_SAMPLE_FRAMES", &DEVICE_BUFFER_FRAMES.to_string());
        let spec = sdl3::audio::AudioSpec {
            freq: Some(SAMPLE_RATE as i32),
            channels: Some(i32::from(channels)),
            format: Some(sdl3::audio::AudioFormat::f32_sys()),
        };
        let depth_cell = buffer_ms.clone();
        let (pcm_tx, pcm_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(CHUNK_QUEUE);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(CHUNK_QUEUE);
        let stream = sdl_audio
            .default_playback_device()
            .open_playback_stream_with_callback(
                &spec,
                RingCallback {
                    rx: pcm_rx,
                    recycle: recycle_tx,
                    ring: VecDeque::new(),
                    policy: JitterPolicy::new(TUNING, channels),
                    per_ms: (SAMPLE_RATE as usize / 1000) * channels as usize,
                    scratch: Vec::new(),
                    buffer_ms,
                    underruns: 0,
                    sheds: 0,
                    trims: 0,
                    dropped_ms: 0,
                    callbacks: 0,
                },
            )
            .map_err(|e| anyhow::anyhow!("SDL open_playback_stream: {e}"))?;
        let mut player = Self { stream };
        let (obtained, frames) = device_format(&player.stream)?;
        let dev_channels = obtained.channels.unwrap_or(i32::from(channels)).max(1) as u8;
        let dev_freq = obtained.freq.unwrap_or(SAMPLE_RATE as i32).max(1);
        let dev_frames = frames.unwrap_or(i32::from(DEVICE_BUFFER_FRAMES)).max(1) as u32;
        // Device quantum is half of this route's latency. SDL may negotiate larger than requested,
        // which silently raises the policy's target (floored at one callback + 5ms). Need to log
        // so session logs show where buffering went.
        tracing::info!(
            "SDL audio device: {} driver, {dev_channels}ch @{dev_freq}Hz, {dev_frames} frame(s) per callback ({:.1}ms), format {:?}",
            sdl_audio.current_audio_driver(),
            f64::from(dev_frames) * 1000.0 / f64::from(dev_freq),
            obtained.format,
        );
        if dev_channels != channels || dev_freq != SAMPLE_RATE as i32 {
            // Not fatal - SDL converts - but decoder/device disagreement on frame shape should
            // be logged before it is heard.
            tracing::warn!(
                "audio device negotiated {dev_channels}ch @{dev_freq}Hz, stream is {channels}ch @{}Hz",
                SAMPLE_RATE,
            );
        }
        // Convert the device period to source frames before sizing decoder-side buffers.
        let source_frames = source_frames(dev_frames, dev_freq as u32);
        {
            let mut callback = player
                .stream
                .lock()
                .ok_or_else(|| anyhow::anyhow!("SDL lock audio stream"))?;
            callback.ring.reserve(ring_capacity(source_frames, channels));
            callback
                .scratch
                .resize(source_frames as usize * usize::from(channels), 0.0);
        }
        player
            .stream
            .resume()
            .map_err(|e| anyhow::anyhow!("SDL resume audio stream: {e}"))?;
        Ok((
            player,
            SdlAudioSink {
                pcm_tx,
                recycle_rx: std::sync::Mutex::new(recycle_rx),
                channels,
                buffer_ms: depth_cell,
            },
        ))
    }
}

impl Drop for AudioPlayer {
    fn drop(&mut self) {
        // rust-sdl3 0.20 frees callback userdata before destroying its stream. Unregistering
        // under SDL's stream lock waits for an in-flight callback and prevents use-after-free.
        // SAFETY: the stream is live; this only fails for a null stream.
        unsafe {
            sdl3::sys::audio::SDL_SetAudioStreamGetCallback(self.stream.raw(), None, std::ptr::null_mut());
        }
    }
}

/// Query the stream's actual device without constructing an owning `AudioDevice` guard.
fn device_format(stream: &sdl3::audio::AudioStream) -> Result<(sdl3::audio::AudioSpec, Option<i32>)> {
    let mut spec = sdl3::sys::audio::SDL_AudioSpec::default();
    let mut frames = 0;
    // SAFETY: the stream keeps its bound device alive; both output pointers are valid.
    let ok = unsafe {
        let device = sdl3::sys::audio::SDL_GetAudioStreamDevice(stream.raw());
        sdl3::sys::audio::SDL_GetAudioDeviceFormat(device, &mut spec, &mut frames)
    };
    anyhow::ensure!(ok, "SDL device format: {}", sdl3::get_error());
    Ok((sdl3::audio::AudioSpec::from(&spec), (frames > 0).then_some(frames)))
}

fn source_frames(device_frames: u32, device_rate: u32) -> u32 {
    (u64::from(device_frames) * u64::from(SAMPLE_RATE)).div_ceil(u64::from(device_rate)) as u32
}

/// The feed half of the device, as the pipeline sees it: hand it f32 samples in punktfunk's own
/// channel order. SDL converts from that format to the playback device's format.
///
/// Timestamps are ignored — this device has no timeline to land on, it plays what it is given in
/// the order it arrives. That is the whole difference between this route and the two that ride
/// NDL's plane, and the reason the pipeline stamps every route the same way regardless.
pub struct SdlAudioSink {
    pcm_tx: SyncSender<Vec<f32>>,
    /// Drained chunk `Vec`s coming back from the callback for reuse, so steady-state playback
    /// stops allocating (~200 chunks/s otherwise). Behind a `Mutex` only because `AudioSink` is
    /// shared: one thread feeds it, and the lock is never contended.
    recycle_rx: std::sync::Mutex<Receiver<Vec<f32>>>,
    channels: u8,
    /// Where the ring publishes its depth, so [`AudioSink::depth_ms`] can read it back.
    buffer_ms: Arc<AtomicU32>,
}

impl AudioSink for SdlAudioSink {
    fn name(&self) -> &'static str {
        "SDL device"
    }

    fn format(&self) -> AudioFormat {
        AudioFormat::PcmF32 {
            channels: self.channels,
            sample_rate: SAMPLE_RATE,
        }
    }

    /// Hands one decoded chunk to the ring, reusing a pooled `Vec` where one is available.
    ///
    /// A full channel drops the chunk rather than blocking: this runs on the audio thread, and
    /// blocking it would back-pressure into the transport.
    fn feed(&self, samples: Samples<'_>, _host_pts_ns: u64) -> Result<()> {
        let Samples::F32(pcm) = samples else {
            anyhow::bail!("SDL device takes f32 samples only");
        };
        let mut buf = self
            .recycle_rx
            .lock()
            .map_or_else(|_| Vec::new(), |rx| rx.try_recv().unwrap_or_default());
        buf.clear();
        buf.extend_from_slice(pcm);
        let _ = self.pcm_tx.try_send(buf);
        Ok(())
    }

    fn depth_ms(&self) -> Option<i64> {
        Some(i64::from(self.buffer_ms.load(Ordering::Relaxed)))
    }
}

/// The playback half, owned by SDL's audio thread: drain the channel into a ring, and serve it
/// under [`TUNING`]'s de-jitter policy. Every decision about priming, drift and de-priming belongs
/// to [`JitterPolicy`] — see the module docs for why this is core's state machine and not a local
/// one.
struct RingCallback {
    rx: Receiver<Vec<f32>>,
    recycle: SyncSender<Vec<f32>>,
    ring: VecDeque<f32>,
    /// The shared de-jitter state machine. Allocation- and syscall-free by contract, which is what
    /// makes it safe to run inside a realtime audio callback.
    policy: JitterPolicy,
    /// Interleaved samples per millisecond, for the counters below.
    per_ms: usize,
    /// Buffer for ring samples before push to stream. rust-sdl3 asks for samples instead of a
    /// mutable slice, so this side owns memory - grown only when callback requests exceed device
    /// quantum, never allocated per callback.
    scratch: Vec<f32>,
    buffer_ms: Arc<AtomicU32>,
    underruns: u64,
    /// Smooth drift sheds — the policy working. Counted apart from [`Self::trims`] because they
    /// mean opposite things: sheds are drift being corrected inaudibly, trims are the link
    /// outrunning the headroom.
    sheds: u64,
    trims: u64,
    /// Total audio discarded by either correction.
    dropped_ms: u64,
    callbacks: u64,
}

impl sdl3::audio::AudioCallback<f32> for RingCallback {
    /// SDL3 inverted the contract: callback pushes audio INTO the stream instead of filling a
    /// slice SDL owns. Core logic unchanged - policy sees one read of `out.len()` samples per call.
    ///
    /// WARNING: `requested` is a sample count (f32 units), not bytes. SDL's C callback gets bytes,
    /// rust-sdl3 divides by `size_of::<Channel>()` before calling here. Dividing again gives 1/4
    /// of desired samples - audible as noise, not dropout, since ring stays full but reads wrong.
    fn callback(&mut self, stream: &mut sdl3::audio::AudioStream, requested: i32) {
        let wanted = requested.max(0) as usize;
        if wanted == 0 {
            return;
        }
        // Taken, not borrowed, so `fill` can have `&mut self`; restored below to avoid reallocating.
        let mut buf = std::mem::take(&mut self.scratch);
        if buf.len() < wanted {
            buf.resize(wanted, 0.0);
        }
        self.fill(&mut buf[..wanted]);
        let _ = stream.put_data_f32(&buf[..wanted]);
        self.scratch = buf;
        self.log_periodically();
    }
}

impl RingCallback {
    /// Serves one callback's worth of samples into `out`, under [`TUNING`]'s de-jitter policy.
    /// Always fills the whole slice — with silence while priming, and zero-padded on an underrun.
    fn fill(&mut self, out: &mut [f32]) {
        // A producer can refill while we drain; bound work before the policy trims the ring.
        for _ in 0..CHUNK_QUEUE {
            let Ok(mut chunk) = self.rx.try_recv() else { break };
            self.ring.extend(chunk.iter().copied());
            // Return the drained Vec to the pool; a full/closed pool just drops it.
            chunk.clear();
            let _ = self.recycle.try_send(chunk);
        }

        let step = self.policy.step(self.ring.len(), out.len());
        if step.drop_front > 0 {
            // Faded on BOTH paths. The hard trim used to splice raw on the reasoning that a ring
            // which blew its ceiling is already a discontinuity — but that describes the arrivals,
            // not the samples either side of the seam, and the trim is the drop that actually
            // fires in the field.
            crossfade_drop(&mut self.ring, step.drop_front, step.crossfade);
            self.dropped_ms += (step.drop_front / self.per_ms.max(1)) as u64;
            if step.hard_trim {
                self.trims += 1;
            } else {
                self.sheds += 1;
            }
        }
        // The SMOOTHED depth, not the instantaneous one: the raw figure swings by a whole device
        // quantum every callback, and this is what drift correction actually reacts to.
        self.buffer_ms.store(self.policy.avg_depth_ms(), Ordering::Relaxed);

        // Priming or re-priming after sustained drain. Ring shallower than one callback produces
        // continuous crackle (half-empty callbacks) not one gap. Skip note_read - core ignores
        // un-primed reads, deliberate silence is not an underrun.
        if step.silence {
            out.fill(0.0);
            return;
        }

        // Two copy_from_slice calls max (ring wraps once), not pop_front per sample - SDL's
        // audio thread runs against a hard deadline.
        let served = out.len().min(self.ring.len());
        let (head, tail) = self.ring.as_slices();
        let from_head = served.min(head.len());
        out[..from_head].copy_from_slice(&head[..from_head]);
        out[from_head..served].copy_from_slice(&tail[..served - from_head]);
        self.ring.drain(..served);
        let ran_short = served < out.len();
        if ran_short {
            out[served..].fill(0.0);
            self.underruns += 1;
        }
        // Drives both the de-prime hysteresis and the adaptive floor, so it must be reported for
        // every read the policy authorised — including the ones that went fine.
        self.policy.note_read(ran_short);
    }

    /// ~10 s at this device quantum. `target_ms` is the adaptive floor's current answer — the one
    /// figure that says whether this set needed more slack than the preset's base, and the
    /// evidence for whether the policy is earning its place here.
    fn log_periodically(&mut self) {
        self.callbacks += 1;
        if self.callbacks % 1_000 != 0 {
            return;
        }
        tracing::debug!(
            buffer_ms = self.policy.avg_depth_ms(),
            target_ms = self.policy.target_ms(),
            primed = self.policy.is_primed(),
            underruns = self.underruns,
            sheds = self.sheds,
            trims = self.trims,
            dropped_ms = self.dropped_ms,
            "audio playback (SDL device)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_bounds_drain_when_more_than_one_queueful_is_available() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (recycle, _) = std::sync::mpsc::sync_channel(CHUNK_QUEUE);
        for _ in 0..=CHUNK_QUEUE {
            tx.send(vec![0.0; 480]).unwrap();
        }
        let mut callback = RingCallback {
            rx,
            recycle,
            ring: VecDeque::new(),
            policy: JitterPolicy::new(TUNING, 2),
            per_ms: 96,
            scratch: Vec::new(),
            buffer_ms: Arc::new(AtomicU32::new(0)),
            underruns: 0,
            sheds: 0,
            trims: 0,
            dropped_ms: 0,
            callbacks: 0,
        };
        callback.fill(&mut [0.0; 1024]);
        assert_eq!(callback.rx.try_recv().unwrap().len(), 480);
        assert!(callback.rx.try_recv().is_err());
    }

    #[test]
    fn device_period_is_sized_in_source_frames() {
        assert_eq!(source_frames(512, 48_000), 512);
        assert_eq!(source_frames(512, 24_000), 1024);
        assert_eq!(source_frames(512, 96_000), 256);
        assert_eq!(source_frames(512, 44_100), 558);
    }
}
