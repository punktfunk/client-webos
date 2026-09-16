//! NDL `DirectMedia` **v2** (webOS 5+): `NDL_DirectMediaLoad` + `NDL_DirectVideoPlay(buffer, size, pts)`,
//! render-buffer query, flush, and HDR metadata. The path every working TV takes.
//!
//! Never calls `NDL_DirectVideoSetArea` (stutters above 1080p); v2 sizes its own punch-through plane
//! (v1 can't; see [`super::v1`]). Handle and video feed here; [`load`] holds pipeline prerequisites,
//! [`plane`] holds the audio pace reference.

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::Result;

use crate::core::media::{AudioPlane, MediaClock, VideoSink, VideoSinkCaps};

use super::{arm_load, ffi, lock_ffi, mark_frame_fed_logged, LOAD_COMPLETED};

mod load;
mod plane;

pub use plane::OPUS_51_LAYOUT;
#[cfg(test)]
pub use plane::OPUS_51_SILENCE;

use plane::PLANE_CONFIRM_GRACE;

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
            self.fns.video_play(au, pts_ms)?;
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
            partial_au: true,
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

    fn is_dead(&self) -> bool {
        super::fatal()
    }
}
