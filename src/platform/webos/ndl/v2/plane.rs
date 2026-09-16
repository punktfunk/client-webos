//! NDL's audio plane: the Opus feed the picture is paced against, and the silent metronome that
//! keeps it fed.
//!
//! A fed audio plane is what makes NDL pace the picture at all (docs/NOTES.md § "NDL's audio
//! plane"), so this is video machinery that happens to carry sound.

use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{bail, Result};
use punktfunk_core::audio::OpusLayout;

use crate::core::media::{AudioFormat, AudioPlane, AudioSink, Samples};

use super::super::{ffi, lock_ffi, LOAD_COMPLETED};
use super::NdlVideo;

/// One empty Opus frame — `mariotaku/ss4s`'s `opus_empty_frame_211`. Its TOC declares STEREO,
/// matching the load; the generic `0xF8 0xFF 0xFE` declares mono. (A CX took both.)
pub(super) const OPUS_SILENCE: [u8; 3] = [0xec, 0xff, 0xfe];

/// One empty frame in [`OPUS_51_LAYOUT`] — ss4s's `opus_empty_frame_642`: four 5 ms streams,
/// two coupled, the first three self-delimited.
pub const OPUS_51_SILENCE: [u8; 15] = [
    0xec, 0x02, 0xff, 0xfe, 0xec, 0x02, 0xff, 0xfe, 0xe8, 0x02, 0xff, 0xfe, 0xe8, 0xff, 0xfe,
];

/// The one 5.1 Opus layout NDL decodes on its plane — the standard coupling, which ss4s's
/// `IsOpusPassthroughSupported` checks for. Every session asks the host for it
/// (`Hello::audio_layout`); `session::audio` re-encodes into it against a host that answers
/// legacy.
pub const OPUS_51_LAYOUT: OpusLayout = punktfunk_core::audio::LAYOUT_51_STANDARD;

/// One [`PRIME_PACKET_MS`] silent packet for a plane loaded with `channels`.
pub(super) fn silence(channels: u8) -> &'static [u8] {
    if channels == OPUS_51_LAYOUT.channels {
        &OPUS_51_SILENCE
    } else {
        &OPUS_SILENCE
    }
}

/// Packet duration of the prime's stamps (ms), matching the real audio plane's 48 kHz / 5 ms
/// (`SAMPLE_RATE` in `platform::webos::audio`).
pub(super) const PRIME_PACKET_MS: i64 = 5;

/// How far ahead of wall-clock the prime's stamps may run, in packets — a burst big enough to
/// configure a decoder, and the bound on how far its `last_audio_pts_ms` ceiling overshoots.
pub(super) const PRIME_LEAD: i64 = 8;

/// Lead the REAL stream's stamps carry over the player clock, on top of the packet's own arrival
/// time. **Zero — aurora parity.** `ndl_audio.c:126-154` stamps every packet with `GetPts` and
/// adds nothing, and that is the shipping reference for NDL smoothness.
///
/// ⚠ **A lead was never what paced the picture** (CX, 2026-09-16). At lead 0 the plane sat at 0 to
/// −4 ms of depth, pacing was unchanged, audio was clean over a 40 s run, and the A/V offset moved
/// from ~+36 ms to roughly aligned. The old 40 ms was `PRIME_LEAD * PRIME_PACKET_MS` — the prime's
/// ceiling reused as a steady-state constant, never a figure derived from a measurement.
///
/// The one job it did do was buy the AUDIO stream jitter slack: a late packet with no depth ahead
/// of it is a dropout, and the arrival tail was measured 24 ms past the estimate. That is the
/// standing risk of zero, and the 40 s run does not clear it — if offload audio drops out on a
/// lossy link, raise this before looking anywhere else. It no longer has a second job: the depth
/// the silent metronome used to restore went with the metronome.
///
/// ⚠ **Lip sync is what zero buys.** Sound lands this far behind the player clock while the
/// picture lands at its mapped cushion (`session::timeline::Pacing`), so the stamp-domain offset is
/// `PLANE_LEAD_MS − cushion` — worst under Lowest latency, which holds the shallowest cushion.
///
/// The **Smoothness buffer is sync-neutral**, because [`NdlVideo::set_plane_extra_lead_ms`] moves
/// this plane by the same budget it gave the picture. Without that the deeper settings walk the
/// sound ahead of the picture — at 60 Hz the two-frame default already crosses over, at 30 Hz even
/// one frame does — and ahead is the more audible direction. `av stamp` on the overlay is where the
/// residual shows; `plane_lead` is the audio half alone.
const PLANE_LEAD_MS: i64 = 0;

/// Gap between prime bursts. Polled through, not slept through — the callback lands mid-gap,
/// and this is launch-path time, i.e. black screen.
pub(super) const PRIME_RETRY: Duration = Duration::from_millis(20);

/// How long past the first accepted frame an audio-enabled load has to report `LOADCOMPLETED`
/// before the plane is called refused **in the log**. Diagnostic only — nothing is recovered.
///
/// This is the one window where the two indistinguishable cases separate. Before a frame is fed,
/// "no callback yet" means either a healthy ingest-gated set or a pipeline that rejected the Opus
/// config asynchronously (which then accepts every frame into a decoder that never runs — a whole
/// black session with no error on any call). After a frame, a healthy set answers: the measured
/// QNED takes 26 ms. Generous against that, since the cost of being early is a false alarm in the
/// log and the cost of being late is nothing.
///
/// **Deliberately not a fallback.** Recovering means unloading and re-loading mid-session, which
/// was written for issue #188 and reverted: it re-applies HDR metadata to the new pipeline, and
/// mode re-entry on that path is itself a suspected cause of #188 (docs/NOTES.md § "NDL's audio
/// plane"). Until a set is measured that genuinely refuses the plane this way, the honest move is
/// to name it rather than to act on a guess — the whole of #188 was a misread of this signal.
pub(super) const PLANE_CONFIRM_GRACE: Duration = Duration::from_millis(750);

/// The audio plane every V2 load asks for: Opus at 48 kHz, stereo or [`OPUS_51_LAYOUT`].
///
/// **Every accepted V2 load asks for a plane** — NDL only paces the picture against a fed audio
/// plane (docs/NOTES.md § "NDL's audio plane"). The offload route puts the session's Opus on it;
/// every other session runs [`NdlVideo::run_clock_plane`]'s metronome instead, whose silence
/// [`silence`] shapes to the same channel count.
pub(super) fn plane_config(channels: u8) -> ffi::AudioUnion {
    ffi::AudioOpusInfo {
        kind: 3, // NDL_AUDIO_TYPE_OPUS
        unknown1: 0,
        channels: std::ffi::c_int::from(channels),
        unknown2: 0,
        // kHz, not Hz — NDL's own unit, and what ss4s passes (`info->sampleRate / 1000.0`).
        sample_rate: 48.0,
        stream_header: std::ptr::null(),
        _padding: [0; 4],
    }
    .to_union()
}

impl NdlVideo {
    /// Whether this load asked for an audio plane — the picture's only pacing reference, see
    /// [`Self::run_clock_plane`]. False only for a load that was refused one outright, which is
    /// a session with no pacing reference at all.
    ///
    /// Not the same question as "may the real stream ride it" — that is
    /// [`AudioPlane::accepts_stream`], and it is the stricter of the two.
    pub fn has_audio_plane(&self) -> bool {
        self.audio
    }
    /// Feed one Opus packet to the audio plane, stamped on the PLAYER clock.
    ///
    /// **The host's capture PTS is deliberately ignored.** Deriving the audio stamp from it (via
    /// the session clock, plus a skew re-derived on every re-anchor) ratchets: a video freeze
    /// stalls the mapped timeline while packets keep arriving, the run resumes below the ceiling
    /// it already reached, and the only monotonic repair is to add lead — which nothing in the
    /// session can ever pay back. Measured on a CX: five re-anchors inside four seconds walked the
    /// plane from 78 ms to 124 ms of lead and the session went silent for good, with healthy depth
    /// and sane per-latch skew the whole way. `mariotaku/ss4s` removed the same apparatus in
    /// `ef0c0ae` and stamps both planes off `CLOCK_MONOTONIC` since load; moonlight-tv#493 is the
    /// unfixed version of this failure.
    ///
    /// A wall clock cannot ratchet. It advances at the same rate whatever the host PTS does across
    /// a freeze, so a resumed run lands where an uninterrupted one would have, and the ceiling
    /// below is left with nothing to do but absorb reordering.
    ///
    /// Every stamp carries [`PLANE_LEAD_MS`] on top of it — currently 0, i.e. the packet is
    /// stamped where it arrives. Read that constant before touching this arithmetic.
    ///
    /// **Both planes are still in one time base** — NDL synchronises them against each other, and
    /// the video plane's own stamps are the player clock too (`session::timeline::Pacing` maps the
    /// host PTS onto it). What changed is that audio no longer rides the mapping's jumps.
    pub fn play_audio(&self, packet: &[u8]) -> Result<()> {
        // The plane's start gate. Feeding a pipeline NDL has not finished loading costs the
        // session its audio outright, and unlike the video feed this path has no `ensure_loaded`
        // of its own — the prime is what carries the plane through the load window.
        //
        // The CALLBACK, not `feed_unblocked`: that flag is the video feed's gate and latches
        // optimistically once the frames have to flow regardless (see [`Self::ensure_loaded`]).
        // Audio has no such deadline — a plane fed early is a plane lost — so it waits for the
        // real thing.
        if !self.plane_ready() {
            return Ok(());
        }
        let now_ms = (self.elapsed_ns() / 1_000_000) as i64;
        let target_ms = now_ms + PLANE_LEAD_MS + self.extra_lead_ms();
        {
            let _ffi = lock_ffi();
            // Floor only: `target_ms` is already ahead of anything the clock plane can have fed,
            // so this bites solely on a packet arriving out of order or inside the same
            // millisecond as its predecessor.
            let pts_ms = self
                .last_audio_pts_ms
                .fetch_max(target_ms, Ordering::Relaxed)
                .max(target_ms);
            self.fns.audio_play(packet, pts_ms)?;
        }
        Ok(())
    }

    /// Feed silence from `from_ms` up to `target_ms`, returning the last stamp fed.
    ///
    /// One `lock_ffi` for the whole burst — the video feed shares that guard, so per-packet
    /// acquires would be up to 60 of them in the picture's way.
    fn burst_silence(&self, from_ms: i64, target_ms: i64) -> Result<i64> {
        let silence = silence(self.plane_channels);
        let _ffi = lock_ffi();
        let mut pts_ms = from_ms.max(self.last_audio_pts_ms.load(Ordering::Relaxed));
        while pts_ms < target_ms {
            pts_ms += PRIME_PACKET_MS;
            if let Err(e) = self.fns.audio_play(silence, pts_ms) {
                // Publish before unwinding: this burst has already handed NDL stamps above the old
                // ceiling, and leaving it stale lets the next real packet floor below them — a
                // rewind, which mutes the session for good.
                self.last_audio_pts_ms.fetch_max(pts_ms, Ordering::Relaxed);
                return Err(e);
            }
        }
        self.last_audio_pts_ms.fetch_max(pts_ms, Ordering::Relaxed);
        Ok(pts_ms)
    }

    /// Carry the load prime until the load confirms, then stop. Blocks, so the caller gives it a
    /// thread.
    ///
    /// **The steady-state metronome is gone.** It used to feed silence every [`PRIME_RETRY`] for
    /// the life of the session — on the software route as the plane's only feed, on offload as a
    /// filler after a grace period of host silence. Measured on a CX (2026-09-16): with the
    /// plane left 136 seconds stale, across an idle stretch where the host sent no frames either,
    /// the picture paced normally and submission margins came out no worse. Nothing was recovered
    /// by feeding it.
    ///
    /// That narrows #130 rather than contradicting it. #130 measured a load with NO audio arm
    /// ignoring presentation timestamps; every V2 load still REQUESTS a plane and PRIMES it.
    ///
    /// ⚠ **What is NOT retired is the prime's continuation through an unconfirmed load.**
    /// [`Self::prime_audio`] is allowed to give up before `LOADCOMPLETED` and hand the plane over
    /// still unconfirmed, and [`Self::play_audio`] refuses to feed a plane in that state — so
    /// without this loop a set that confirms late would have its plane fed by nobody, which is the
    /// condition #188 was about. Every CX run confirmed during the prime, so that path has never
    /// been exercised under the new default; feeding until the callback keeps the old behaviour
    /// exactly where it was load-bearing and retires only the part that was measured.
    ///
    /// The thread also owns [`Self::check_plane_confirmed`], kept off the feed path, and exits once
    /// neither job can fire again.
    pub fn run_clock_plane(&self, stop: &std::sync::atomic::AtomicBool, yields_to_real: bool) {
        if !self.audio {
            tracing::info!("NDL clock plane: the load has no audio plane — nothing to pace against");
            return;
        }
        // Logged per-load, so every capture carries the config that produced it.
        tracing::info!(
            "NDL clock plane: route={} feed=prime-until-confirmed plane_lead={PLANE_LEAD_MS}ms",
            if yields_to_real { "offload" } else { "software" },
        );
        while !stop.load(Ordering::Relaxed) {
            self.check_plane_confirmed();
            if LOAD_COMPLETED.fired() {
                // Confirmed: stop polling (50x/sec otherwise) since the prime needs no carry-through.
                tracing::info!("NDL clock plane: load confirmed, leaving the plane on its prime");
                return;
            } else {
                // Carry the prime at its own ceiling, so the handover is seamless whenever the
                // callback lands.
                let now_ms = (self.elapsed_ns() / 1_000_000) as i64;
                // The PRIME's lead, not [`PLANE_LEAD_MS`]: this is the prime continuing, and the
                // real stream's lead is 0, which would burst nothing at all.
                if let Err(e) = self.burst_silence(now_ms, now_ms + PRIME_LEAD * PRIME_PACKET_MS) {
                    tracing::warn!("NDL clock plane: carrying the prime failed: {e:#}");
                    return;
                }
            }
            std::thread::sleep(PRIME_RETRY);
        }
    }

    pub(super) fn extra_lead_ms(&self) -> i64 {
        self.extra_lead_ms.load(Ordering::Relaxed)
    }

    /// Hold `ms` of depth beyond [`PLANE_LEAD_MS`], matching the lead the Smoothness buffer gave
    /// the picture.
    ///
    /// **Lip sync is the whole point.** Both planes stamp on one clock, so the offset between them
    /// is `audio_lead − video_lead`. Left alone, a deeper picture cushion walks the sound ahead of
    /// the picture — at 60 Hz the two-frame default already crosses over, and sound ahead is the
    /// more audible direction. Moving the plane by the same figure holds the offset wherever it was.
    ///
    /// ⚠ **Static, and set once before the plane threads start.** Only the Smoothness budget may
    /// be passed here — it is fixed for the session. Tracking the ADAPTIVE cushion instead would
    /// make the lead follow a moving target, and [`Self::last_audio_pts_ms`] is a floor: every rise
    /// is kept and no fall can ever be paid back. That ratchet is what took a CX session silent
    /// (see the note on [`Self::play_audio`]).
    /// Named apart from the trait method that forwards to it: `Self::set_extra_lead_ms` inside
    /// `impl AudioPlane` would resolve to this only by inherent-first precedence, and losing that
    /// silently turns the forward into unbounded recursion.
    pub fn set_plane_extra_lead_ms(&self, ms: i64) {
        self.extra_lead_ms.store(ms.max(0), Ordering::Relaxed);
    }

    /// How far the audio plane's stamps run ahead of the player clock, in ms. Reads the ceiling,
    /// so it reports whichever feed last raised it.
    ///
    /// ⚠ **A deeply negative reading is NOT a fault, and not a stutter signature.** It was
    /// described as one while the metronome kept the plane topped up; since the metronome was
    /// retired the software route reads −N seconds for the whole session by design, and a CX
    /// measurement found pacing unaffected at −136 s. What the figure IS good for is saying whether
    /// anything is feeding the plane at all: on offload it tracks `PLANE_LEAD_MS` plus whatever
    /// [`Self::set_plane_extra_lead_ms`] holds while the real stream rides it, and falls away when
    /// that stream stops.
    pub fn audio_plane_lead_ms(&self) -> i64 {
        self.last_audio_pts_ms.load(Ordering::Relaxed) - (self.elapsed_ns() / 1_000_000) as i64
    }
    /// Whether the REAL stream may ride the plane: this load asked for one AND NDL has confirmed
    /// it. The silent metronome does not ask this — see [`Self::run_clock_plane`].
    ///
    /// Both halves matter. The callback latch alone is not enough: a video-only load confirms
    /// normally and has no audio arm, and feeding that is a silent session. [`Self::feed_unblocked`]
    /// is not the latch either — that one is the video feed's gate and latches optimistically
    /// (see [`Self::play_audio`]).
    fn plane_ready(&self) -> bool {
        self.audio && LOAD_COMPLETED.fired()
    }
    /// Name a plane that never confirmed, once, past [`PLANE_CONFIRM_GRACE`].
    ///
    /// Runs on the clock-plane thread, which already wakes every [`PRIME_RETRY`], rather than on
    /// the video feed. The deadline is latched by the first accepted frame (`play`) into an atomic
    /// both threads already share, so moving the CHECK here needs no handoff — and the feed stops
    /// paying a relaxed load per picture for the life of the session to emit at most one line.
    fn check_plane_confirmed(&self) {
        let deadline = self.plane_check_ms.load(Ordering::Relaxed);
        if deadline == i64::MAX || (self.elapsed_ns() / 1_000_000) as i64 <= deadline {
            return;
        }
        self.plane_check_ms.store(i64::MAX, Ordering::Relaxed);
        if self.plane_ready() {
            return;
        }
        // Loud: nothing is recovered, so this line is the only trace. What it can and cannot mean
        // is on [`PLANE_CONFIRM_GRACE`].
        tracing::warn!("NDL: no LOADCOMPLETED {PLANE_CONFIRM_GRACE:?} after the first frame — the audio plane was likely refused, the picture will not be paced");
    }
}

impl AudioSink for NdlVideo {
    fn name(&self) -> &'static str {
        "NDL Opus plane"
    }

    /// What the load asked for, and what every silence burst here already speaks — see
    /// [`plane_config`].
    fn format(&self) -> AudioFormat {
        AudioFormat::Opus {
            channels: self.plane_channels,
        }
    }

    /// `host_pts_ns` is ignored — the plane stamps off the player clock, which is the whole point
    /// of [`NdlVideo::play_audio`].
    fn feed(&self, samples: Samples<'_>, _host_pts_ns: u64) -> Result<()> {
        let Samples::Opus(packet) = samples else {
            // The plane decodes; decoded samples are the SDL device's shape and never reach here.
            bail!("NDL audio plane takes Opus packets only");
        };
        self.play_audio(packet)
    }

    fn depth_ms(&self) -> Option<i64> {
        Some(self.audio_plane_lead_ms())
    }
}

impl AudioPlane for NdlVideo {
    fn lead_ms(&self) -> i64 {
        self.audio_plane_lead_ms()
    }

    fn run_keepalive(&self, stop: &std::sync::atomic::AtomicBool, yields_to_real: bool) {
        self.run_clock_plane(stop, yields_to_real);
    }

    fn set_extra_lead_ms(&self, ms: i64) {
        self.set_plane_extra_lead_ms(ms);
    }

    fn accepts_stream(&self) -> bool {
        self.plane_proven
    }
}
