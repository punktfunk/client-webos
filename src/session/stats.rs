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
    /// Effective presentation cushion in µs: core's adaptive figure plus the Smoothness budget.
    /// The one counter that MOVES when the presentation setting does — jitter is the measured
    /// residual and is independent of it by construction, and `pacing_late` is cumulative, so
    /// without this the overlay cannot show the setting doing anything.
    pub pacing_cushion_us: AtomicU32,
    /// Tightest complete-AU deadline margin of the last window, in µs — see
    /// `Pacing::note_submitted`. Legitimately negative (that is the whole point), so it carries no
    /// sentinel; `i32::MIN` is "not sampled this window".
    pub pacing_min_slack_us: AtomicI32,
    /// Audio-plane queue depth in ms (`NdlVideo::audio_plane_lead_ms`). A video figure as much as
    /// an audio one — NDL paces the picture on this — and can legitimately be negative, so there
    /// is no sentinel: the overlay prints it only on a route that has a plane.
    pub audio_plane_lead_ms: AtomicI32,
    /// How far sound trails the picture, in ms: the plane's lead less the picture's cushion.
    /// Positive is sound behind picture, negative is sound ahead. Like the lead above it carries no
    /// sentinel, and it is only written on a route where real audio rides the plane.
    ///
    /// ⚠ **Stamp domain, not on-glass.** NDL's decode and panel transit are not observable from the
    /// app and bias the picture later, so the true offset is smaller than this reads. A trend and a
    /// sign, never a calibration.
    pub av_offset_ms: AtomicI32,
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
