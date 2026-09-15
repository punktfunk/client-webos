//! Host timestamps mapped into the NDL load clock, with a bounded presentation cushion.

use pf_client_core::trust::PresentPriority;

/// Correct clock offset and drift without replacing source cadence with arrival cadence.
/// Smoothness reserves the selected number of stream periods on TOP of core's adaptive
/// cushion; the default keeps the adaptive cushion alone. Neither mode delays compressed
/// input in a Rust queue.
pub struct Pacing {
    clock: punktfunk_core::phase::CadenceClock,
    /// Source period, and the ceiling core clamps its own adaptive cushion to.
    source_interval_ns: i64,
    /// Extra lead Smoothness adds over the adaptive cushion; `0` under the latency intent.
    smooth_cushion_ns: i64,
    /// Backward timestamps can make NDL rewind and mute output; keep the floor across recovery.
    last_base_ns: u64,
    /// Repeated source stamps provide no cadence observation and must not train clock drift.
    last_host_pts_ns: Option<u64>,
    /// Count after the added cushion and monotonic clamping, which core cannot observe.
    late_stamps: u64,
    late_submissions: u64,
    /// Tightest deadline margin seen since the last read, in µs — see [`Self::note_submitted`].
    /// `None` once taken, until another AU completes.
    min_slack_us: Option<i32>,
}

impl Pacing {
    /// Use the negotiated source period, not the panel period. Pass a priority resolved by the
    /// shared settings resolver (including Automatic).
    ///
    /// ⚠ **This client reads the frame count as EXTRA, where `PresentPriority::fifo_capacity` and
    /// the shared option help ("Automatic holds two") describe a TOTAL.** A deliberate divergence:
    /// core clamps its own adaptive cushion at one source period, so a substituted one-frame budget
    /// is unreachable on any link with real jitter and the first step of the setting does nothing.
    /// The honest fix is additive semantics in core, which would need the shared crate and the help
    /// text to change together.
    pub fn new(source_interval_ns: u64, priority: PresentPriority) -> Self {
        let interval = i64::try_from(source_interval_ns).unwrap_or(i64::MAX).max(1);
        // `fifo_capacity` is core's own reading of the enum: 0 means the latency intent, and
        // `PresentPriority::resolve` has already clamped the buffer to 1–3.
        let frames = priority.fifo_capacity();
        let smooth_cushion_ns = interval.saturating_mul(i64::from(frames));
        Self {
            // Default tuning assumes roughly half a refresh of panel-latch slack; the firmware
            // scheduling assumption still needs device validation.
            clock: punktfunk_core::phase::CadenceClock::new(punktfunk_core::phase::CadenceTuning::snapping()),
            source_interval_ns: interval,
            smooth_cushion_ns,
            last_base_ns: 0,
            last_host_pts_ns: None,
            late_stamps: 0,
            late_submissions: 0,
            min_slack_us: None,
        }
    }

    /// Map once at the first AU piece; all subsequent pieces reuse this timestamp.
    /// Late arrivals spend the cushion instead of moving the deadline to arrival + cushion.
    /// The offset absorbs the constant between host and player clocks; no shared epoch is needed.
    pub fn map(&mut self, host_pts_ns: u64, player_clock_ns: u64) -> u64 {
        let ready = i64::try_from(player_clock_ns).unwrap_or(i64::MAX);
        let repeated = self.last_host_pts_ns == Some(host_pts_ns);
        let due = if repeated {
            self.clock.note_off_cadence(ready, self.source_interval_ns)
        } else {
            self.clock.due_ns(host_pts_ns, ready, self.source_interval_ns)
        };
        self.last_host_pts_ns = Some(host_pts_ns);
        let due = due.saturating_add(self.smooth_cushion_ns);
        let base = u64::try_from(due).unwrap_or(0).max(self.last_base_ns);
        // Choose the later integer-ms timestamp; NDL truncates, so rounding down would spend up to
        // a millisecond of whatever cushion was reserved. Both intents, since both are truncated —
        // it applied only under Smoothness when that branch owned the rounding.
        let base = base.div_ceil(1_000_000) * 1_000_000;
        self.last_base_ns = base;
        if base <= player_clock_ns {
            self.late_stamps += 1;
        }
        base
    }

    /// Includes tail arrival, lock waits and feed time; still not a hardware decode
    /// or display timestamp. Compare in the integer-ms domain NDL actually receives.
    ///
    /// Also keeps the tightest MARGIN, not just the count of misses. The estimator folds an AU's
    /// FIRST piece (see `VideoStage::au_base_ns`), so it never observes when a slice-progressive
    /// picture COMPLETED: a large frame can finish against a deadline that small frames set and
    /// healthy arrival statistics will not show it. This is the figure that would.
    pub fn note_submitted(&mut self, pts_ns: u64, player_clock_ns: u64) {
        if pts_ns / 1_000_000 <= player_clock_ns / 1_000_000 {
            self.late_submissions += 1;
        }
        let slack_us =
            (i64::try_from(pts_ns).unwrap_or(i64::MAX) - i64::try_from(player_clock_ns).unwrap_or(i64::MAX)) / 1_000;
        // Saturate towards the sign it had: clamping a huge miss to `i32::MAX` would report the
        // worst frame of the session as the roomiest one.
        let slack_us = i32::try_from(slack_us).unwrap_or(if slack_us.is_negative() { i32::MIN } else { i32::MAX });
        self.min_slack_us = Some(self.min_slack_us.map_or(slack_us, |m| m.min(slack_us)));
    }

    /// Take the window's tightest deadline margin, arming the next one. Called only where the
    /// figure is actually read — the take IS the re-arm, so a caller that drops the answer silently
    /// shortens the window — and a minimum rather than a mean, so one bad frame stays visible.
    pub fn take_min_slack_us(&mut self) -> Option<i32> {
        self.min_slack_us.take()
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
            cushion_ns: h.cushion_ns.saturating_add(self.smooth_cushion_ns),
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
    /// Effective cushion: the adaptive figure, plus the selected frame budget in Smoothness.
    pub cushion_ns: i64,
    /// First-piece mapping already late, before NDL's millisecond conversion.
    pub late_stamps: u64,
    /// Complete AU submitted at or after its integer-ms timestamp.
    pub late_submissions: u64,
    /// Times the clock established a new source-to-player offset.
    pub reanchors: u64,
}

/// The STATIC part of the picture's lead: the Smoothness budget, in ms. Fixed for the session, so
/// it is the only part of the cushion an audio plane may safely match (`AudioPlane::set_extra_lead_ms`
/// — a monotonic plane can never give depth back). The adaptive remainder is deliberately excluded.
pub fn smooth_cushion_ms(stream_hz: u32, priority: PresentPriority) -> i64 {
    i64::from(priority.fifo_capacity()) * 1_000 / i64::from(stream_hz.max(1))
}

/// Nanoseconds as milliseconds, for log lines.
pub(super) fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}
