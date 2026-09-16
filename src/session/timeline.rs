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
    /// Session total for overlay; per-window figures live in [`CadenceTrace`].
    late_stamps: u64,
    /// Baseline window: source, mapped, emitted cadence.
    trace: CadenceTrace,
    /// Previous mapped value, for trace deltas. Separate from monotonic floor `last_base_ns`.
    last_due_ns: Option<i64>,
    last_emitted_ns: Option<u64>,
    /// Player clock when open picture was mapped; for span measurement in `note_submitted`.
    map_clock_ns: Option<u64>,
    /// Tightest deadline margin since last read, µs; taken (cleared) on read.
    min_slack_us: Option<i32>,
}

impl Pacing {
    /// Use source period, not panel period. Priority includes Automatic from settings.
    ///
    /// ⚠ This client reads frame count as EXTRA (core clamps adaptive to 1 period, making
    /// one-frame budget unreachable); the honest fix needs additive semantics in core.
    pub fn new(source_interval_ns: u64, priority: PresentPriority) -> Self {
        let interval = i64::try_from(source_interval_ns).unwrap_or(i64::MAX).max(1);
        // `fifo_capacity` is core's own reading of the enum: 0 means the latency intent, and
        // `PresentPriority::resolve` has already clamped the buffer to 1–3.
        let frames = priority.fifo_capacity();
        let smooth_cushion_ns = interval.saturating_mul(i64::from(frames));
        Self {
            // Default tuning assumes roughly half a refresh of panel-latch slack. Unverified on
            // NDL: nothing in ss4s claims it, aurora's software grid observes no display phase,
            // and NDL's scheduling is undocumented (docs/NOTES.md § "Cadence pacing").
            clock: punktfunk_core::phase::CadenceClock::new(punktfunk_core::phase::CadenceTuning::snapping()),
            source_interval_ns: interval,
            smooth_cushion_ns,
            last_base_ns: 0,
            last_host_pts_ns: None,
            late_stamps: 0,
            trace: CadenceTrace::default(),
            last_due_ns: None,
            last_emitted_ns: None,
            map_clock_ns: None,
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
        // Keep the three series aligned: excluded frames can't be counted in one and not others.
        // Without this, src/due/emitted count different frames and their MADs don't compare.
        let usable = self
            .trace
            .note_source(self.last_host_pts_ns, host_pts_ns, self.source_interval_ns);
        self.last_host_pts_ns = Some(host_pts_ns);
        let due = due.saturating_add(self.smooth_cushion_ns);
        let base = u64::try_from(due).unwrap_or(0).max(self.last_base_ns);
        // Round up to integer ms; NDL truncates so rounding down wastes cushion.
        let base = base.div_ceil(1_000_000) * 1_000_000;
        self.last_base_ns = base;
        self.trace.mapped += 1;
        if base <= player_clock_ns {
            self.late_stamps += 1;
            self.trace.late_stamps += 1;
        }
        let stamp_slack_us = saturating_us(i64::try_from(base).unwrap_or(i64::MAX) - ready);
        self.trace.min_stamp_slack_us = Some(
            self.trace
                .min_stamp_slack_us
                .map_or(stamp_slack_us, |m| m.min(stamp_slack_us)),
        );
        // Store previous values on every frame, excluded or not. Clearing on exclusion broke
        // the next frame's delta — skipped gap then normal frame gave src=100, due=0.
        if usable {
            if let Some(last) = self.last_due_ns {
                self.trace.due.note(due - last, self.source_interval_ns);
            }
            if let Some(last) = self.last_emitted_ns {
                self.trace
                    .emitted
                    .note(i64::try_from(base - last).unwrap_or(0), self.source_interval_ns);
            }
        }
        self.last_due_ns = Some(due);
        self.last_emitted_ns = Some(base);
        self.map_clock_ns = Some(player_clock_ns);
        base
    }

    /// Take the window's cadence figures, arming the next one — the take IS the re-arm, exactly
    /// like [`Self::take_min_slack_us`].
    pub fn take_trace(&mut self) -> CadenceTrace {
        std::mem::take(&mut self.trace)
    }

    /// Note AU submission; includes tail arrival and lock waits, not hardware timestamp.
    /// Compare in integer-ms domain (NDL's domain). Keeps tightest margin, not just miss count;
    /// estimator only sees first piece, never completion. This span shows what the margin hides.
    pub fn note_submitted(&mut self, pts_ns: u64, player_clock_ns: u64) {
        self.trace.submissions += 1;
        // Span for this picture (complete − map); window minima can't measure this (different pictures).
        if let Some(mapped_at) = self.map_clock_ns.take() {
            let span_us = i64::try_from(player_clock_ns.saturating_sub(mapped_at)).unwrap_or(i64::MAX) / 1_000;
            let span_us = u32::try_from(span_us).unwrap_or(u32::MAX);
            self.trace.max_au_span_us = self.trace.max_au_span_us.max(span_us);
        }
        if pts_ns / 1_000_000 <= player_clock_ns / 1_000_000 {
            self.trace.late_submissions += 1;
        }
        let slack_us = saturating_us(
            i64::try_from(pts_ns).unwrap_or(i64::MAX) - i64::try_from(player_clock_ns).unwrap_or(i64::MAX),
        );
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
        // The timeline jumped, so the first delta after it is not a cadence observation.
        self.last_due_ns = None;
        self.last_emitted_ns = None;
        self.map_clock_ns = None;
    }

    pub fn health(&self) -> PacingHealth {
        let h = self.clock.health();
        PacingHealth {
            jitter_ns: h.jitter_ns,
            cushion_ns: h.cushion_ns.saturating_add(self.smooth_cushion_ns),
            late_stamps: self.late_stamps,
            reanchors: h.reanchors,
        }
    }
}

/// Frame-to-frame interval statistics over a window; windowed summary, not per-frame logs.
#[derive(Clone, Copy, Default)]
pub struct Deltas {
    /// Intervals observed. `0` means the series never advanced — report it, never divide by it.
    pub n: u32,
    /// Σ(delta − nominal): which way the series runs, and by how much in total.
    sum_err_ns: i64,
    /// Σ|delta − nominal|: spread, the figure a mean alone hides.
    sum_abs_err_ns: i64,
    pub min_ns: i64,
    pub max_ns: i64,
}

impl Deltas {
    fn note(&mut self, delta_ns: i64, nominal_ns: i64) {
        let err = delta_ns - nominal_ns;
        if self.n == 0 {
            self.min_ns = delta_ns;
            self.max_ns = delta_ns;
        } else {
            self.min_ns = self.min_ns.min(delta_ns);
            self.max_ns = self.max_ns.max(delta_ns);
        }
        self.n += 1;
        self.sum_err_ns = self.sum_err_ns.saturating_add(err);
        self.sum_abs_err_ns = self.sum_abs_err_ns.saturating_add(err.abs());
    }

    /// Signed mean departure from the nominal period, in ns. Drift shows here; jitter cancels out.
    pub fn mean_err_ns(&self) -> i64 {
        if self.n == 0 {
            0
        } else {
            self.sum_err_ns / i64::from(self.n)
        }
    }

    /// Mean ABSOLUTE departure — irregularity, whichever way each frame went.
    pub fn mad_ns(&self) -> i64 {
        if self.n == 0 {
            0
        } else {
            self.sum_abs_err_ns / i64::from(self.n)
        }
    }
}

/// Source, mapped, emitted cadence over the same window; frame events counted separately.
/// Read all three together or not at all — neither has meaning across gaps, repeats, or re-anchors.
#[derive(Clone, Copy, Default)]
pub struct CadenceTrace {
    /// Host capture PTS deltas — the cadence the mapping was handed.
    pub source: Deltas,
    /// Mapped deadline deltas, before the monotonic clamp and the millisecond rounding.
    pub due: Deltas,
    /// What NDL actually received, after both.
    pub emitted: Deltas,
    /// Frames repeating the previous host PTS: no cadence observation, and excluded from `source`.
    pub repeats: u64,
    /// Host PTS going backwards. Excluded from `source` — a negative delta is not a short frame.
    pub regressions: u64,
    /// Host PTS gaps over 1.5 nominal periods; excluded from source.
    pub gaps: u64,
    /// Pictures mapped in this window, including excluded ones.
    pub mapped: u64,
    /// Mapped pictures already at or past the player clock.
    pub late_stamps: u64,
    /// Complete AUs timed in this window; separate denominator (timing is gated).
    pub submissions: u64,
    pub late_submissions: u64,
    /// Tightest headroom at mapping (stamp − clock), µs; answers "how deep the arrival tail".
    pub min_stamp_slack_us: Option<i32>,
    /// Longest single picture span (map to submit), µs; paired per-picture, not window minima.
    pub max_au_span_us: u32,
}

impl CadenceTrace {
    /// Classify one source interval. Returns true if it's a usable cadence observation.
    /// False for first frame and counted events (gap/repeat/regression); caller excludes them downstream.
    fn note_source(&mut self, last: Option<u64>, host_pts_ns: u64, nominal_ns: i64) -> bool {
        let Some(last) = last else {
            return false;
        };
        if host_pts_ns == last {
            self.repeats += 1;
            return false;
        }
        if host_pts_ns < last {
            self.regressions += 1;
            return false;
        }
        let delta = i64::try_from(host_pts_ns - last).unwrap_or(i64::MAX);
        if delta > nominal_ns.saturating_mul(3) / 2 {
            self.gaps += 1;
            return false;
        }
        self.source.note(delta, nominal_ns);
        true
    }
}

#[derive(Clone, Copy, Default)]
pub struct PacingHealth {
    /// Measured clock residual (mean absolute deviation), independent of the chosen cushion.
    pub jitter_ns: i64,
    /// Effective cushion: the adaptive figure, plus the selected frame budget in Smoothness.
    pub cushion_ns: i64,
    /// First-piece mapping already late, counted AFTER the monotonic clamp and the millisecond
    /// round-up — i.e. against the stamp NDL was actually handed, not the mapped deadline.
    pub late_stamps: u64,
    /// Times the clock established a new source-to-player offset.
    pub reanchors: u64,
}

/// Static Smoothness budget (ms), the only cushion part audio plane may safely match.
pub fn smooth_cushion_ms(stream_hz: u32, priority: PresentPriority) -> i64 {
    i64::from(priority.fifo_capacity()) * 1_000 / i64::from(stream_hz.max(1))
}

/// Convert ns to µs, saturating toward the sign. Clamping huge miss to MAX would flip worst to roomiest.
fn saturating_us(ns: i64) -> i32 {
    let us = ns / 1_000;
    i32::try_from(us).unwrap_or(if us.is_negative() { i32::MIN } else { i32::MAX })
}

/// Nanoseconds as milliseconds, for log lines.
pub(super) fn ms(ns: u64) -> f64 {
    ns as f64 / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three series must cover the same frames or MADs don't compare.
    /// Bug: excluding an event's successor broke next frame's delta (gap/regular: src=100, due=0).
    #[test]
    fn every_cadence_series_counts_the_same_intervals() {
        const I: u64 = 16_666_666;
        let mut p = Pacing::new(I, PresentPriority::Latency);
        let mut pts = 0u64;
        let mut now = 0u64;
        let feed = |p: &mut Pacing, pts: u64, now: u64| {
            p.map(pts, now);
        };

        // Startup, a gap, a repeat, and a run of regular frames on either side of each.
        for _ in 0..5 {
            pts += I;
            now += I;
            feed(&mut p, pts, now);
        }
        pts += 3 * I; // gap
        now += 3 * I;
        feed(&mut p, pts, now);
        for _ in 0..3 {
            pts += I;
            now += I;
            feed(&mut p, pts, now);
        }
        now += I; // repeat: same source stamp, later arrival
        feed(&mut p, pts, now);
        for _ in 0..3 {
            pts += I;
            now += I;
            feed(&mut p, pts, now);
        }

        let t = p.take_trace();
        assert_eq!(t.gaps, 1);
        assert_eq!(t.repeats, 1);
        assert_eq!(t.regressions, 0);
        assert_eq!(
            (t.source.n, t.due.n, t.emitted.n),
            (t.source.n, t.source.n, t.source.n),
            "series disagree: src={} due={} out={}",
            t.source.n,
            t.due.n,
            t.emitted.n,
        );
        // 13 mapped pictures: the first has no predecessor, the gap and the repeat are excluded.
        assert_eq!(t.mapped, 13);
        assert_eq!(t.source.n, 10);
    }

    /// Regression test for gap suppressing next frame's mapped delta.
    #[test]
    fn a_gap_does_not_suppress_the_next_intervals_mapped_delta() {
        const I: u64 = 8_333_333;
        let mut p = Pacing::new(I, PresentPriority::Latency);
        let (mut pts, mut now) = (0u64, 0u64);
        for i in 0..100 {
            let step = if i % 2 == 0 { 3 * I } else { I };
            pts += step;
            now += step;
            p.map(pts, now);
        }
        let t = p.take_trace();
        assert_eq!(t.source.n, t.due.n);
        assert_eq!(t.source.n, t.emitted.n);
        assert!(t.source.n > 0, "every regular interval was excluded");
    }
}
