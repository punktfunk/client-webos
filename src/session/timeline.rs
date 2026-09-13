//! Host timestamps mapped into the NDL load clock, with a bounded presentation cushion.

use pf_client_core::trust::PresentPriority;

/// Correct clock offset and drift without replacing source cadence with arrival cadence.
/// Smoothness reserves the selected number of stream periods inside NDL; the default
/// retains core's adaptive cushion. Neither mode delays compressed input in a Rust queue.
pub struct Pacing {
    clock: punktfunk_core::phase::CadenceClock,
    /// Source period caps the default adaptive cushion; Smoothness can reserve several periods.
    source_interval_ns: i64,
    smooth_cushion_ns: Option<i64>,
    /// Backward timestamps can make NDL rewind and mute output; keep the floor across recovery.
    last_base_ns: u64,
    /// Repeated source stamps provide no cadence observation and must not train clock drift.
    last_host_pts_ns: Option<u64>,
    /// Count after cushion substitution and monotonic clamping, which core cannot observe.
    late_stamps: u64,
    late_submissions: u64,
}

impl Pacing {
    /// Use the negotiated source period, not the panel period. The frame count is
    /// the total cushion, not an addition to the adaptive or Aurora cushion.
    /// Pass a priority resolved by the shared settings resolver (including Automatic).
    pub fn new(source_interval_ns: u64, priority: PresentPriority) -> Self {
        let interval = i64::try_from(source_interval_ns).unwrap_or(i64::MAX).max(1);
        // `fifo_capacity` is core's own reading of the enum: 0 means the latency intent, and
        // `PresentPriority::resolve` has already clamped the buffer to 1–3.
        let frames = priority.fifo_capacity();
        let smooth_cushion_ns = (frames > 0).then(|| interval.saturating_mul(i64::from(frames)));
        Self {
            // Default tuning assumes roughly half a refresh of panel-latch slack. Smoothness replaces
            // its cushion; the firmware scheduling assumption still needs device validation.
            clock: punktfunk_core::phase::CadenceClock::new(punktfunk_core::phase::CadenceTuning::snapping()),
            source_interval_ns: interval,
            smooth_cushion_ns,
            last_base_ns: 0,
            last_host_pts_ns: None,
            late_stamps: 0,
            late_submissions: 0,
        }
    }

    /// Map once at the first AU piece; all subsequent pieces reuse this timestamp.
    /// Late arrivals spend the cushion instead of moving the deadline to arrival + cushion.
    /// The offset absorbs the constant between host and player clocks; no shared epoch is needed.
    pub fn map(&mut self, host_pts_ns: u64, player_clock_ns: u64) -> u64 {
        let ready = i64::try_from(player_clock_ns).unwrap_or(i64::MAX);
        let repeated = self.last_host_pts_ns == Some(host_pts_ns);
        let mut due = if repeated {
            self.clock.note_off_cadence(ready, self.source_interval_ns)
        } else {
            self.clock.due_ns(host_pts_ns, ready, self.source_interval_ns)
        };
        self.last_host_pts_ns = Some(host_pts_ns);
        let base = if let Some(cushion) = self.smooth_cushion_ns {
            due = due.saturating_sub(self.clock.cushion_ns()).saturating_add(cushion);
            let base = u64::try_from(due).unwrap_or(0).max(self.last_base_ns);
            // Choose the later integer-ms timestamp; this adds less than 1 ms to the target.
            base.div_ceil(1_000_000) * 1_000_000
        } else {
            u64::try_from(due).unwrap_or(0).max(self.last_base_ns)
        };
        self.last_base_ns = base;
        if base <= player_clock_ns {
            self.late_stamps += 1;
        }
        base
    }

    /// Includes tail arrival, lock waits and feed time; still not a hardware decode
    /// or display timestamp. Compare in the integer-ms domain NDL actually receives.
    pub fn note_submitted(&mut self, pts_ns: u64, player_clock_ns: u64) {
        if pts_ns / 1_000_000 <= player_clock_ns / 1_000_000 {
            self.late_submissions += 1;
        }
    }

    /// Re-anchor after loss while preserving timestamp monotonicity: recovery does
    /// not flush NDL, so its previous timestamps remain live.
    pub fn reset(&mut self) {
        self.clock.reset();
        self.last_host_pts_ns = None;
    }

    pub fn health(&self) -> PacingHealth {
        let h = self.clock.health();
        PacingHealth {
            jitter_ns: h.jitter_ns,
            cushion_ns: self.smooth_cushion_ns.unwrap_or(h.cushion_ns),
            late_stamps: self.late_stamps,
            late_submissions: self.late_submissions,
            reanchors: h.reanchors,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct PacingHealth {
    /// Measured clock residual (mean absolute deviation), independent of the chosen cushion.
    pub jitter_ns: i64,
    /// Effective cushion: adaptive in Lowest latency, selected frame budget in Smoothness.
    pub cushion_ns: i64,
    /// First-piece mapping already late, before NDL's millisecond conversion.
    pub late_stamps: u64,
    /// Complete AU submitted at or after its integer-ms timestamp.
    pub late_submissions: u64,
    /// Times the clock established a new source-to-player offset.
    pub reanchors: u64,
}

/// Nanoseconds as milliseconds, for log lines.
pub(super) fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}
