//! Live session counters and process metrics the stats overlay reads.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};

/// Live video-pump counters for stats overlay (read at ~2Hz); relaxed atomics written per frame.
#[derive(Default)]
pub struct StreamStats {
    /// Freeze-until-reanchor hold active.
    pub holding: AtomicBool,
    /// Most recent decoder feed duration (µs).
    pub feed_us: AtomicU32,
    /// NDL render-buffer backlog or -1 if unavailable.
    pub render_backlog: AtomicI32,
    /// The live mapping's measured jitter (mean absolute deviation of `ready − pts`), in µs, and
    /// the frames it stamped too late to pace — see `session::timeline::PacingHealth`. Published on
    /// the heartbeat's cadence under both mappings, so a stutter report can be read against them
    /// whichever one produced it.
    pub pacing_jitter_us: AtomicU32,
    pub pacing_late: AtomicU64,
    /// Audio-plane queue depth in ms (`NdlVideo::audio_plane_lead_ms`). A video figure as much as
    /// an audio one — NDL paces the picture on this — and can legitimately be negative, so there
    /// is no sentinel: the overlay prints it only on a route that has a plane.
    pub audio_plane_lead_ms: AtomicI32,
    /// Whether anything is going to READ the figures above — today that is the stats overlay, and
    /// the flag is named for the demand rather than for the widget so a second consumer can set it
    /// without every producer re-deriving what "listening" means. Private: it is the session's own
    /// copy of that state, so both directions go through the accessors below.
    diagnostics: AtomicBool,
    /// The decoder failed in a way no re-anchor undoes (`core::media::VideoSink::is_dead`). Read by
    /// the stream loop, which ends the session on it: the transport is still healthy, so nothing
    /// else would ever end it, and the user would sit in front of a frozen picture with no audio.
    pub decoder_dead: AtomicBool,
}

impl StreamStats {
    /// Whether anything reads the diagnostic counters right now — see the field.
    pub fn wants_diagnostics(&self) -> bool {
        self.diagnostics.load(Ordering::Relaxed)
    }

    /// The one writer, so the flag never has a second copy to keep in sync with.
    pub fn set_diagnostics(&self, on: bool) {
        self.diagnostics.store(on, Ordering::Relaxed);
    }
}
