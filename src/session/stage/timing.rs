//! Feed-path timing: whether the clock is read at all, and the per-PICTURE submit cost.
//!
//! Split out because it was threaded through [`super::VideoStage::feed`] by hand as four separate
//! `if timed` blocks, which is what made the ~20 lines of actual feed logic hard to read against
//! the reference implementation.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::session::StreamStats;

/// Feed calls slower than this suggest decoder backpressure rather than network loss.
const FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);

/// The feed is timed only where something reads the figure: the host's ABR controller, or the
/// overlay. With neither listening it is untimed, the clock is never read, and the slow-feed
/// warning cannot fire — turn the overlay on to get it back.
pub(super) struct FeedTiming {
    /// Whether the host asked for decode-latency reports. Fixed for the session.
    report_decode_latency: bool,
    /// Whether anything reads the diagnostic figures. Latched from [`StreamStats`] on the pump's
    /// heartbeat, so it can flip mid-stream — including in the middle of a partial AU.
    diagnostics: bool,
    /// Submit time accumulated across the pieces of the AU currently being fed, so `feed_us` stays
    /// a figure per PICTURE. Stored per-piece it reported only the last slice, which on a
    /// slice-progressive session is a fraction of the AU's real submission cost.
    au_feed_us: u32,
}

impl FeedTiming {
    pub(super) fn new(report_decode_latency: bool) -> Self {
        Self {
            report_decode_latency,
            diagnostics: false,
            au_feed_us: 0,
        }
    }

    pub(super) fn timed(&self) -> bool {
        self.report_decode_latency || self.diagnostics
    }

    pub(super) fn set_diagnostics(&mut self, on: bool) {
        self.diagnostics = on;
    }

    /// Start timing one piece, or don't. Accumulation survives a mid-AU flip either way: a piece
    /// begun while timed still folds in, and one begun untimed adds nothing.
    pub(super) fn begin(&self) -> Option<Instant> {
        self.timed().then(Instant::now)
    }

    /// Fold this piece's submit time into the open AU. Returns the elapsed time only when it is
    /// past [`FEED_BACKPRESSURE_WARN`], which is the sole reason the caller wants the figure.
    pub(super) fn end(&mut self, started: Option<Instant>) -> Option<Duration> {
        let elapsed = started?.elapsed();
        self.au_feed_us = self
            .au_feed_us
            .saturating_add(u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX));
        (elapsed >= FEED_BACKPRESSURE_WARN).then_some(elapsed)
    }

    /// Cost accumulated so far in the open AU.
    pub(super) fn open_au_us(&self) -> u32 {
        self.au_feed_us
    }

    /// Returns per-PICTURE cost for the ABR controller. Both consumers require accumulated cost,
    /// not per-slice: a final-slice figure would undercount in slice-progressive sessions.
    pub(super) fn finish_au(&mut self, stats: &StreamStats) -> u32 {
        let au_feed_us = self.au_feed_us;
        if self.timed() {
            stats.feed_us.store(au_feed_us, Ordering::Relaxed);
        }
        self.au_feed_us = 0;
        au_feed_us
    }

    /// Clears accumulated cost so it doesn't carry forward to the next AU.
    pub(super) fn abandon(&mut self) {
        self.au_feed_us = 0;
    }

    /// Whether the host's ABR controller is listening, which is narrower than [`Self::timed`].
    pub(super) fn reports_decode(&self) -> bool {
        self.report_decode_latency
    }
}
