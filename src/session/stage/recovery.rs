//! What the stage does when a picture cannot be trusted: the freeze-until-reanchor hold, the
//! keyframe-request throttle, and the play-error policy.
//!
//! Separated from the feed path so the ~20 lines that actually stamp and submit a picture can be
//! read against the reference implementation without paging through recovery.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::media::{NotReady, VideoSink, VideoSinkCaps};
use crate::session::timeline::ms;
use crate::session::StreamStats;

use super::{FrameFlags, SinkResult};

/// Minimum spacing between [`SinkResult::NeedKeyframe`] results: the request travels on its own
/// QUIC control stream, so a tight interval costs nothing but the request itself.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// What [`Recovery::gate`] decided about this frame.
pub(super) enum HoldGate {
    /// Feed it — not holding.
    Feed,
    /// This is the frame that lifts the hold. The timeline just jumped, so the caller re-anchors
    /// its mapping before feeding.
    Resume,
    /// Still frozen — skip it, and report this instead.
    Skip(SinkResult),
}

/// Hold state and the keyframe throttle.
pub(super) struct Recovery {
    stats: Arc<StreamStats>,
    /// Freeze-until-reanchor: while holding, frames are skipped rather than fed — the
    /// punch-through plane keeps the last good picture. Resumes on an IDR or an LTR-RFI
    /// recovery anchor only. `Some` for exactly as long as the hold lasts.
    hold_started: Option<Instant>,
    last_keyframe_request: Option<Instant>,
    /// Holds BEGUN and total time held, both session totals the heartbeat differences. The live
    /// `holding` flag is a snapshot and cannot see a hold that started and ended between two
    /// heartbeats — which is exactly the shape a stutter report is about.
    holds: u64,
    held: Duration,
}

impl Recovery {
    pub(super) fn new(stats: Arc<StreamStats>) -> Self {
        Self {
            stats,
            hold_started: None,
            last_keyframe_request: None,
            holds: 0,
            held: Duration::ZERO,
        }
    }

    pub(super) fn holding(&self) -> bool {
        self.hold_started.is_some()
    }

    pub(super) fn begin_hold(&mut self) {
        self.stats.holding.store(true, Ordering::Relaxed);
        if self.hold_started.is_none() {
            self.hold_started = Some(Instant::now());
            self.holds += 1;
        }
    }

    pub(super) fn hold_totals(&self) -> (u64, Duration) {
        // A hold still running counts its time so far, or a long one would read as zero until it
        // lifted.
        let running = self.hold_started.map_or(Duration::ZERO, |t| t.elapsed());
        (self.holds, self.held + running)
    }

    /// Stamps this keyframe request as sent, if throttle allows.
    pub(super) fn take_keyframe_slot(&mut self) -> bool {
        let ready = self
            .last_keyframe_request
            .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL);
        if ready {
            self.last_keyframe_request = Some(Instant::now());
        }
        ready
    }

    /// Forget when the last keyframe request went out, so the next one is allowed immediately.
    #[cfg(test)]
    pub(super) fn clear_keyframe_throttle(&mut self) {
        self.last_keyframe_request = None;
    }

    /// Run before every feed.
    pub(super) fn gate(&mut self, flags: &FrameFlags) -> HoldGate {
        if flags.loss {
            let newly_holding = !self.holding();
            self.begin_hold();
            if newly_holding {
                tracing::warn!("loss (frame {}) — freezing", flags.index);
            }
            // NO FLUSH on the loss hold — this is the last structural difference from `ss4s`, which
            // never flushes mid-stream (its only recovery is unload+load) and does not lose its
            // Opus plane. Every flush here stops the pipeline: each one is followed by NDL
            // reporting `PLAYING (0x1a)`, a transition it only makes from not-playing. A CX
            // survived 32 of them in one storm with perfectly monotonic audio stamps at a constant
            // 40 ms lead and still went permanently silent, so what kills the plane is the restart,
            // not anything the feed says (see docs/NOTES.md § "NDL's audio plane").
            //
            // The decode-error path below still flushes: there the pipeline has actually errored
            // and discarding its queue is the documented response. Loss is a network event — NDL's
            // queue holds good frames that the hold is about to present anyway.
        }
        let Some(started) = self.hold_started else {
            return HoldGate::Feed;
        };
        // An IDR or an RFI anchor predicts from nothing NDL lacks. An intra-refresh wave heals
        // only a decoder that decoded every frame of it, and the hold skipped those: lifting on
        // its marks would feed NDL a picture whose references it never saw.
        if !flags.reanchor {
            // The slot is taken only while frames are still skipped. The frame that lifts the hold
            // restarts decoding by itself, and reporting a request on it made `submit` read the
            // feed as refused — abandoning the open AU, i.e. truncating the very keyframe that
            // resumed the picture on a slice-progressive session, and re-arming the hold.
            return HoldGate::Skip(if self.take_keyframe_slot() {
                SinkResult::NeedKeyframe
            } else {
                SinkResult::Held
            });
        }
        tracing::info!(
            "resuming after {:.0}ms (frame {})",
            started.elapsed().as_secs_f32() * 1000.0,
            flags.index,
        );
        self.stats.holding.store(false, Ordering::Relaxed);
        self.held += started.elapsed();
        self.hold_started = None;
        HoldGate::Resume
    }

    pub(super) fn on_play_error(
        &mut self,
        e: &anyhow::Error,
        flags: &FrameFlags,
        base_ns: u64,
        sink: &dyn VideoSink,
        caps: VideoSinkCaps,
    ) -> bool {
        tracing::warn!(
            "{} error (frame {}, pts {:.2}ms): {e:#}",
            sink.name(),
            flags.index,
            ms(base_ns),
        );
        if !self.take_keyframe_slot() {
            return false;
        }
        // A frame refused because the pipeline hasn't finished loading is NOT a decode error, and
        // gets neither loss response.
        //
        // No flush: against a not-yet-loaded pipeline it silently kills the audio plane for the
        // session (video recovers, audio never does — observed on CX), and nothing is queued in
        // NDL to discard anyway.
        //
        // No hold: freeze-until-reanchor is mid-stream recovery, and at frame 0 there is no
        // last-good picture to freeze on. Worse, holding short-circuits `submit` before `play`,
        // the only caller of the feed-anyway escape, so the hold outlives its own cause — release
        // then needs the host's reanchor, evaluated only when a frame arrives, and a static desktop
        // sends none. Request a keyframe and let the next frame retry.
        if e.downcast_ref::<NotReady>().is_none() {
            if caps.flush {
                let _ = sink.flush();
            }
            self.begin_hold();
        }
        true
    }
}
