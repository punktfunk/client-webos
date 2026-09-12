//! What the shared shell costs on this panel, in a form someone can read off a log.
//!
//! The shell's frame numbers were measured once, on a G5, by a spike that never merged. Every
//! panel since has been a guess, and "it lags a bit on my CX" is the only signal a 2020 set has
//! ever produced. This turns that into figures: how long a frame takes on the CPU here, and how
//! much of it is cover art being decoded.
//!
//! Summaries go to the log at INFO, so "Send logs to host" carries them without anyone
//! needing a shell on the TV. They are emitted on a timer and once on the way out — a handful
//! of lines per session rather than a stream.
//!
//! Ungated on purpose, and holding every decision this makes: the console module is armv7-only,
//! and `task test` builds the host target, so anything asserted behind that gate would compile
//! and never run. The caller passes the art counters in rather than this module reading them,
//! which is what keeps the shell's types out of here.

use std::time::{Duration, Instant};

/// How often a summary goes out while the shell is up. Long enough to stay out of the way,
/// short enough that a browse produces several.
const REPORT_EVERY: Duration = Duration::from_secs(10);

/// Frames kept per interval. Ten seconds at 60 Hz is 600; this only bounds the memory, and
/// anything past it is counted and reported rather than silently dropped.
const WINDOW: usize = 1_024;

/// The shell's cover-art counters, as this module needs them. A plain mirror of
/// `pf_console_ui::ArtStats` so the gated caller does the converting and this stays testable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArtSnapshot {
    pub decoded: u64,
    pub total_us: u64,
    pub max_us: u64,
    pub native_scaled: u64,
}

/// Rolling CPU-side frame times plus the art counters, summarised on a timer.
pub struct Perf {
    /// Microseconds per frame this interval. Sorted in place to summarise, hence drained.
    frames: Vec<u32>,
    /// Frames past [`WINDOW`] — reported, so a summary can never quietly describe a sample of
    /// a much longer stretch.
    overflowed: u64,
    started: Instant,
    last_report: Instant,
    /// Art counters as of the last report, so each line describes its own interval.
    art_mark: ArtSnapshot,
    /// What these frames are doing — "browsing", "library" — so a line can be placed without
    /// counting timestamps.
    label: &'static str,
}

impl Perf {
    #[must_use]
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            frames: Vec::with_capacity(WINDOW),
            overflowed: 0,
            started: now,
            last_report: now,
            art_mark: ArtSnapshot::default(),
            label: "browsing",
        }
    }

    /// Name what the frames after this call are doing, so the interval a library load falls in
    /// says so.
    pub fn mark(&mut self, label: &'static str) {
        self.label = label;
    }

    /// One frame's CPU-side time. Returns the summary to log when the interval is up.
    ///
    /// The swap belongs OUTSIDE what the caller times: `gl_swap_window` blocks on vsync, so
    /// including it would measure the panel's refresh rate and hide the thing being asked about.
    pub fn frame(&mut self, took: Duration, art: ArtSnapshot) -> Option<Report> {
        let us = u32::try_from(took.as_micros()).unwrap_or(u32::MAX);
        if self.frames.len() < WINDOW {
            self.frames.push(us);
        } else {
            self.overflowed += 1;
        }
        (self.last_report.elapsed() >= REPORT_EVERY).then(|| self.take(art))
    }

    /// Close the interval now — for the way out, where the last stretch before a launch or a
    /// quit is often the interesting one. `None` when no frame has been recorded since the
    /// last report, because a report of nothing says nothing.
    pub fn finish(&mut self, art: ArtSnapshot) -> Option<Report> {
        (!self.frames.is_empty()).then(|| self.take(art))
    }

    fn take(&mut self, art: ArtSnapshot) -> Report {
        let interval = self.last_report.elapsed();
        self.last_report = Instant::now();
        self.frames.sort_unstable();
        let frames = Summary::of(&self.frames);
        let decoded = art.decoded.saturating_sub(self.art_mark.decoded);
        let total_us = art.total_us.saturating_sub(self.art_mark.total_us);
        let report = Report {
            label: self.label,
            frames,
            counted: self.frames.len() as u64,
            overflowed: self.overflowed,
            interval,
            uptime: self.started.elapsed(),
            decoded,
            native_scaled: art.native_scaled.saturating_sub(self.art_mark.native_scaled),
            art_total_us: total_us,
            // A high-water mark across the run, not a difference — the worst decode is the
            // number that explains a stutter, and it does not belong to one interval.
            art_max_us: art.max_us,
            art_mean_us: total_us.checked_div(decoded).unwrap_or(0),
        };
        self.art_mark = art;
        self.frames.clear();
        self.overflowed = 0;
        report
    }
}

impl Default for Perf {
    fn default() -> Self {
        Self::new()
    }
}

/// One interval, ready to log. Rendered by [`Report::line`] so the wording lives with the
/// numbers rather than at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub label: &'static str,
    pub frames: Summary,
    pub counted: u64,
    pub overflowed: u64,
    pub interval: Duration,
    pub uptime: Duration,
    pub decoded: u64,
    pub native_scaled: u64,
    pub art_total_us: u64,
    pub art_max_us: u64,
    pub art_mean_us: u64,
}

impl Report {
    /// One line, because it is read in a log next to everything else the app says.
    #[must_use]
    pub fn line(&self) -> String {
        let ms = |us: u64| us as f64 / 1000.0;
        let over = if self.overflowed > 0 {
            format!(" (+{} past the window)", self.overflowed)
        } else {
            String::new()
        };
        format!(
            "shell {label}: {counted} frames in {secs:.1}s{over} — cpu p50 {p50:.1}ms \
             p90 {p90:.1}ms p99 {p99:.1}ms max {max:.1}ms | art {decoded} decoded \
             ({native} at codec scale) {total:.0}ms, mean {mean:.1}ms, worst {worst:.1}ms \
             | up {up:.0}s",
            label = self.label,
            counted = self.counted,
            secs = self.interval.as_secs_f64(),
            over = over,
            p50 = ms(u64::from(self.frames.p50)),
            p90 = ms(u64::from(self.frames.p90)),
            p99 = ms(u64::from(self.frames.p99)),
            max = ms(u64::from(self.frames.max)),
            decoded = self.decoded,
            native = self.native_scaled,
            total = ms(self.art_total_us),
            mean = ms(self.art_mean_us),
            worst = ms(self.art_max_us),
            up = self.uptime.as_secs_f64(),
        )
    }
}

/// The percentiles one interval is reduced to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub p50: u32,
    pub p90: u32,
    pub p99: u32,
    pub max: u32,
}

impl Summary {
    /// `sorted` ascending and non-empty. Nearest-rank, so every figure is a frame that actually
    /// happened rather than an interpolation between two that did not.
    #[must_use]
    pub fn of(sorted: &[u32]) -> Self {
        if sorted.is_empty() {
            return Self::default();
        }
        let last = sorted.len() - 1;
        let at = |q: f64| {
            let rank = (q * sorted.len() as f64).ceil() as usize;
            sorted[rank.saturating_sub(1).min(last)]
        };
        Self {
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            max: sorted[last],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nearest-rank on a known set, including the one- and two-sample cases a first report can
    /// hit — no rank may fall off either end.
    #[test]
    fn percentiles_name_frames_that_actually_happened() {
        let sorted: Vec<u32> = (1..=100).collect();
        let s = Summary::of(&sorted);
        assert_eq!((s.p50, s.p90, s.p99, s.max), (50, 90, 99, 100));

        let one = Summary::of(&[7]);
        assert_eq!((one.p50, one.p90, one.p99, one.max), (7, 7, 7, 7));

        let two = Summary::of(&[3, 9]);
        assert_eq!((two.p50, two.max), (3, 9));

        assert_eq!(Summary::of(&[]), Summary::default());
    }

    /// Each report covers its own interval: decode counts subtract, and the worst decode is a
    /// mark across the run rather than a difference.
    #[test]
    fn a_report_covers_its_own_interval() {
        let mut perf = Perf::new();
        perf.mark("library");
        let art = ArtSnapshot {
            decoded: 12,
            total_us: 24_000,
            max_us: 9_000,
            native_scaled: 12,
        };
        assert!(
            perf.frame(Duration::from_millis(4), art).is_none(),
            "the timer has not come round"
        );
        let first = perf.finish(art).expect("a frame was recorded");
        assert_eq!(first.label, "library");
        assert_eq!(first.counted, 1);
        assert_eq!(first.decoded, 12);
        assert_eq!(first.art_mean_us, 2_000);
        assert_eq!(first.frames.max, 4_000);

        // Second interval: only the new decodes count, and the run's worst still shows.
        let later = ArtSnapshot {
            decoded: 20,
            total_us: 28_000,
            max_us: 9_000,
            native_scaled: 19,
        };
        perf.frame(Duration::from_millis(11), later);
        let second = perf.finish(later).expect("a frame was recorded");
        assert_eq!(second.decoded, 8, "eight since the last report, not twenty");
        assert_eq!(second.art_total_us, 4_000);
        assert_eq!(second.native_scaled, 7);
        assert_eq!(second.art_max_us, 9_000, "the worst decode is a run-long mark");
        assert_eq!(second.counted, 1, "the previous interval's frame is not counted twice");

        // Nothing recorded since: nothing to say.
        assert!(perf.finish(later).is_none());
    }

    /// The line is what a remote dev actually sends back, so its shape is worth pinning.
    #[test]
    fn the_line_names_the_numbers_it_carries() {
        let mut perf = Perf::new();
        perf.mark("library");
        perf.frame(Duration::from_millis(25), ArtSnapshot::default());
        let line = perf
            .finish(ArtSnapshot {
                decoded: 3,
                total_us: 60_000,
                max_us: 30_000,
                native_scaled: 3,
            })
            .expect("a frame was recorded")
            .line();
        assert!(line.contains("shell library:"), "{line}");
        assert!(line.contains("max 25.0ms"), "{line}");
        assert!(line.contains("3 decoded (3 at codec scale)"), "{line}");
        assert!(line.contains("worst 30.0ms"), "{line}");
    }
}
