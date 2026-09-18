//! NDL `DirectMedia` **v2** (webOS 5+): `NDL_DirectMediaLoad` + `NDL_DirectVideoPlay(buffer, size, pts)`,
//! render-buffer query, flush, and HDR metadata. The path every working TV takes.
//!
//! Never calls `NDL_DirectVideoSetArea` (stutters above 1080p); v2 sizes its own punch-through plane
//! (v1 can't; see [`super::v1`]). Handle and video feed here; [`load`] holds pipeline prerequisites,
//! [`plane`] holds the audio pace reference.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;

use crate::core::media::{AudioPlane, MediaClock, VideoSink, VideoSinkCaps};

use super::{arm_load, ffi, lock_ffi, mark_frame_fed_logged, LOAD_COMPLETED};

mod load;
mod plane;

pub use plane::OPUS_51_LAYOUT;
#[cfg(test)]
pub use plane::OPUS_51_SILENCE;

use plane::PLANE_CONFIRM_GRACE;

/// How long every frame must be refused before the pipeline counts as gone. A re-anchor after
/// loss has its frames back within a round trip; a plane the TV took for itself does not come
/// back while it holds it. Generous on purpose — a panel sitting over the stream for a moment
/// must cost the picture, never the session.
const DEAD_AFTER_REFUSED: Duration = Duration::from_secs(10);

/// How long the pipeline has been refusing every frame, as ns-since-load of the streak's first
/// refusal (0 = frames are landing).
///
/// NDL only latches [`super::FATAL`] for the one state measured to kill a load, and deliberately
/// refuses to read an unmapped state as fatal. A pipeline whose resources the TV reclaimed can
/// therefore die with no callback anyone can name, and the feeds are then the only evidence.
#[derive(Default)]
struct RefusalStreak(AtomicU64);

impl RefusalStreak {
    /// Records one feed at `now_ns` (ns since load). The streak keeps its FIRST refusal, since
    /// its age is what [`Self::len`] answers with.
    fn note(&self, fed: bool, now_ns: u64) {
        if fed {
            self.0.store(0, Ordering::Relaxed);
        } else {
            // `max(1)` so a refusal in the first ns of a load still reads as a streak.
            let _ = self
                .0
                .compare_exchange(0, now_ns.max(1), Ordering::Relaxed, Ordering::Relaxed);
        }
    }

    /// How long every feed has failed for; zero while any is landing.
    fn len(&self, now_ns: u64) -> Duration {
        match self.0.load(Ordering::Relaxed) {
            0 => Duration::ZERO,
            since => Duration::from_nanos(now_ns.saturating_sub(since)),
        }
    }
}

/// One loaded NDL v2 video decode session. Dropping unloads it (not `NDL_DirectMediaQuit`).
pub struct NdlVideo {
    fns: &'static ffi::V2,
    /// PTS ms since load (NDL clock). Stamped at `NDL_DirectMediaLoad` CALL, not after wait.
    ///
    /// ⚠ Shares zero with [`Self::prime_audio`]'s stamps (both count from load CALL). Post-wait
    /// stamping put the prime a whole load duration ahead, forcing every consumer to correct for
    /// the gap: metronome cached the base, offload floored load-duration packets to one stamp,
    /// `plane_lead` reported gap instead of depth. One origin removes all three.
    load_instant: Instant,
    /// Audio plane for this session (picture paces against it). Fixed for session life.
    audio: bool,
    /// Whether the pipeline is still taking frames — see [`RefusalStreak`].
    refused: RefusalStreak,
    /// Highest audio stamp fed (ms); shared by [`Self::play_audio`] and clock plane to prevent
    /// backward timestamps (NDL reads rewind as seek, mutes session). A floor (never driver):
    /// feeders target player clock + [`PLANE_LEAD_MS`], so the ceiling stays one lead ahead of
    /// real time.
    last_audio_pts_ms: AtomicI64,
    /// Extra depth on REAL stream stamps (on top of [`PLANE_LEAD_MS`]) when Smoothness buffer
    /// moves picture later. See [`Self::set_plane_extra_lead_ms`].
    extra_lead_ms: AtomicI64,
    /// VIDEO feed gate: false while held for load, latched once (steady-state costs one load).
    /// Not "NDL confirmed load" — [`Self::ensure_loaded`] latches it when frames must flow.
    /// For pipeline state (not this gate), use [`Self::plane_ready`]: feeding audio plane
    /// early costs the session its audio permanently.
    feed_unblocked: AtomicBool,
    /// Log deadline (player-clock ms) for unconfirmed plane, stamped on first accepted frame.
    /// `i64::MAX` before that, spent once. See [`PLANE_CONFIRM_GRACE`].
    plane_check_ms: AtomicI64,
    /// Load confirmed plane within budget (safe for session's sole audio). See
    /// [`AudioPlane::accepts_stream`]. Unconfirmed plane still worth feeding (produces pacing,
    /// confirmation on ingest-gated sets), but cannot be sole audio path (no route re-pick once
    /// running).
    plane_proven: bool,
    /// Channels the plane was loaded with: 2, or 6 in [`OPUS_51_LAYOUT`]. Fixed for the handle.
    plane_channels: u8,
    /// HDR metadata arriving before plane ingests first frame. `NDL_DirectVideoSetHDRInfo`
    /// succeeds on non-ingesting pipeline but does nothing — panel stays dark, host sends once
    /// on change. Observed on CX as all-black launch (reconnect fixed).
    ///
    /// Gate is first ACCEPTED frame (not LOADCOMPLETED): load callback ≠ ingesting pipeline.
    pending_hdr: Mutex<Option<ffi::HdrInfo>>,
    /// Last HDR metadata handed to `NDL_DirectVideoSetHDRInfo` (dedup unchanged calls).
    ///
    /// Host re-sends packet unchanged (3x in 10 ms at start). Each call re-enters panel HDR mode —
    /// on CX, drops 1440p120 out of high-rate, blacks the stream. (Latent until metadata
    /// deferred; repeats used to hit non-ingesting pipeline.)
    applied_hdr: Mutex<Option<ffi::HdrInfo>>,
}

impl NdlVideo {
    /// Nanoseconds since load (NDL PTS domain). Sink anchors host PTS here (`session::timeline::Pacing`).
    pub(crate) fn elapsed_ns(&self) -> u64 {
        self.load_instant.elapsed().as_nanos() as u64
    }
    /// Feed one access unit at `pts_ns` (ns since load, truncated to ms for NDL).
    /// Pass mapped base (`session::timeline::Pacing`), not raw `elapsed_ns()`,
    /// to sync video and offloaded audio.
    pub fn play(&self, au: &[u8], pts_ns: u64) -> Result<()> {
        self.ensure_loaded()?;
        let pts_ms = (pts_ns / 1_000_000) as i64;
        let first_frame = {
            let _ffi = lock_ffi();
            let fed = self.fns.video_play(au, pts_ms);
            self.refused.note(fed.is_ok(), self.elapsed_ns());
            fed?;
            mark_frame_fed_logged("NDL", self.load_instant)
        };
        // Outside the FFI guard — `replay_pending_hdr` takes it again, and it isn't reentrant.
        if first_frame {
            self.replay_pending_hdr();
            // Start grace timer for unconfirmed plane.
            if self.audio && !LOAD_COMPLETED.fired() {
                let now_ms = (self.elapsed_ns() / 1_000_000) as i64;
                self.plane_check_ms
                    .store(now_ms + PLANE_CONFIRM_GRACE.as_millis() as i64, Ordering::Relaxed);
            }
        }
        Ok(())
    }
    /// Buffered frames in NDL (None if query fails). Rising = decoder lagging;
    /// flat near-zero with stutter = upstream problem.
    pub fn render_buffer_length(&self) -> Option<i32> {
        let _ffi = lock_ffi();
        self.fns.render_buffer_length()
    }

    pub fn flush(&self) -> Result<()> {
        // Never pre-LOADCOMPLETED: flush silently kills audio plane (see NotReady), no buffer
        // to discard yet. Sink's loss path flushes before play, so guard must live here.
        // Check feed_unblocked (PIPELINE gate), not plane_ready: video-only load has buffer.
        if !self.feed_unblocked.load(Ordering::Relaxed) && !LOAD_COMPLETED.fired() {
            return Ok(());
        }
        let _ffi = lock_ffi();
        self.fns.flush_render_buffer()
    }
}

impl Drop for NdlVideo {
    fn drop(&mut self) {
        // Re-arm so `playing()` stops reporting the load being torn down here.
        arm_load();
        self.fns.unload();
    }
}

impl MediaClock for NdlVideo {
    fn now_ns(&self) -> u64 {
        self.elapsed_ns()
    }
}

/// For `Arc`, not `NdlVideo`: audio plane shares video handle (NDL has no per-plane context).
/// Plane threads must keep load alive; process-global Drop unload can't run during FFI calls.
impl VideoSink for std::sync::Arc<NdlVideo> {
    fn name(&self) -> &'static str {
        "NDL v2"
    }

    fn caps(&self) -> VideoSinkCaps {
        VideoSinkCaps {
            pts: true,
            partial_au: false,
            flush: true,
        }
    }

    fn feed(&self, au: &[u8], pts_ns: u64) -> Result<()> {
        self.play(au, pts_ns)
    }

    fn flush(&self) -> Result<()> {
        NdlVideo::flush(self)
    }

    fn queue_depth(&self) -> Option<u32> {
        self.render_buffer_length().and_then(|d| u32::try_from(d).ok())
    }

    fn set_color(
        &self,
        meta: Option<&punktfunk_core::quic::HdrMeta>,
        color: punktfunk_core::quic::ColorInfo,
    ) -> Result<()> {
        self.set_color_info(meta, color)
    }

    fn clock(&self) -> Option<&dyn MediaClock> {
        Some(self.as_ref())
    }

    fn audio_plane(&self) -> Option<std::sync::Arc<dyn AudioPlane>> {
        self.has_audio_plane()
            .then(|| Self::clone(self) as std::sync::Arc<dyn AudioPlane>)
    }

    /// The named fatal state, or a pipeline that has taken nothing for [`DEAD_AFTER_REFUSED`]:
    /// a reclaimed plane can refuse every frame without ever reporting a state anyone can name,
    /// and the session must end rather than sit on a picture that cannot come back.
    fn is_dead(&self) -> bool {
        let refused = self.refused.len(self.elapsed_ns());
        if refused > DEAD_AFTER_REFUSED {
            tracing::error!("NDL refused every frame for {refused:?} — the pipeline is gone");
            return true;
        }
        super::fatal()
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::{RefusalStreak, DEAD_AFTER_REFUSED};

    const S: u64 = 1_000_000_000;

    /// A streak times from its first refusal, and one accepted frame ends it.
    #[test]
    fn only_an_unbroken_streak_ages() {
        let streak = RefusalStreak::default();
        assert_eq!(
            streak.len(S),
            std::time::Duration::ZERO,
            "nothing fed yet is not a refusal"
        );
        streak.note(false, S);
        streak.note(false, 5 * S);
        assert_eq!(
            streak.len(11 * S).as_secs(),
            10,
            "aged from the FIRST refusal, not the last"
        );
        assert!(
            streak.len(11 * S) <= DEAD_AFTER_REFUSED,
            "the ceiling itself is not yet dead"
        );
        assert!(streak.len(12 * S) > DEAD_AFTER_REFUSED);
        streak.note(true, 12 * S);
        assert_eq!(
            streak.len(30 * S),
            std::time::Duration::ZERO,
            "one accepted frame clears it"
        );
        streak.note(false, 31 * S);
        assert!(streak.len(32 * S) < DEAD_AFTER_REFUSED, "a fresh streak starts over");
    }
}
