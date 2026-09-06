//! The single place that talks to the video decoder.
//!
//! Everything between "an access unit arrived" and "NDL has been fed" lives here: host-PTS
//! mapping on the refresh-rate-reconciled frame interval, backpressure metering,
//! freeze-until-reanchor, and keyframe-request throttling. The video pump keeps only the
//! parts that are wire-shaped — pulling frames, and *how* a keyframe is asked for, which it
//! answers to [`SinkResult::NeedKeyframe`] with `NativeClient::request_keyframe`.
//!
//! Slice-progressive reassembly sits in [`parts`], on its own so it stays testable.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use punktfunk_core::quic;

use crate::core::media::{AudioPlane, NotReady, VideoSink, VideoSinkCaps};
use crate::session::timeline::{ms, Pacing, PacingHealth};
use crate::session::StreamStats;

mod parts;

use parts::{AuParts, PartStep};

/// Feed calls slower than this suggest decoder backpressure rather than network loss.
const FEED_BACKPRESSURE_WARN: Duration = Duration::from_millis(20);
/// One delivery off the transport, as the pump sees it — the stage decides what it means.
pub struct WireFrame<'a> {
    pub data: &'a [u8],
    /// Host capture-clock PTS.
    pub pts_ns: u64,
    pub index: u32,
    /// Slice-progressive piece info, `None` for a whole-AU delivery
    /// (`punktfunk_core::session::FramePart`).
    pub part: Option<punktfunk_core::session::FramePart>,
    /// This frame can restart decoding on its own (IDR, or an LTR recovery anchor).
    pub reanchor: bool,
    /// One intra-refresh wave boundary. Two after loss prove a clean picture.
    pub recovery_mark: bool,
    /// Loss was detected at or before this frame — a sequence gap, or a frame the transport
    /// dropped.
    pub loss: bool,
}

/// What the stage worked out about one delivery before feeding it.
#[derive(Clone, Copy)]
struct FrameFlags {
    reanchor: bool,
    recovery_mark: bool,
    loss: bool,
    /// Host frame index, for logs only.
    index: u64,
    /// This feed is one PIECE of an access unit whose end has not arrived yet
    /// (slice-progressive delivery — `punktfunk_core::session::FramePart`, on for every NDL v2
    /// session). The decoder still takes the bytes; what a piece is NOT is a
    /// presentable frame, so the per-frame reference points hang off the piece that completes the
    /// AU instead. `false` for every feed on the whole-AU path.
    partial: bool,
}

/// Outcome of one [`VideoStage::submit`].
pub enum SinkResult {
    /// Fed to the decoder. `decode_us` is the latency figure for the host's ABR
    /// controller, present only when the sink was built with `report_decode_latency`.
    Presented { decode_us: Option<u32> },
    /// Skipped — still frozen, waiting for a re-anchor.
    Held,
    /// Skipped or failed, and the throttle allows asking the host for a keyframe now.
    NeedKeyframe,
    /// The decoder is gone and no frame will present again on it — see
    /// [`crate::core::media::VideoSink::is_dead`]. The pump ends the session on this rather than
    /// re-anchoring, which is the response to lost FRAMES and does nothing for a lost PIPELINE.
    Dead,
}

/// Everything the sink needs to know up front.
pub struct SinkConfig {
    /// The host's frame cadence — the cushion's ceiling in [`Pacing`].
    pub stream_hz: u32,
    /// Whether the host asked for decode-latency reports (its ABR controller).
    pub report_decode_latency: bool,
}

/// Minimum spacing between [`SinkResult::NeedKeyframe`] results: the request travels on its own
/// QUIC control stream, so a tight interval costs nothing but the request itself.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Render-buffer depth past which the decoder is behind for real: eight frames is 133 ms of
/// picture at 60 fps that NDL accepted and has not shown. Sessions measure 0–1 whether smooth
/// or stuttering, so this fires on a stall, not on jitter.
const BACKLOG_HOLD_FRAMES: u32 = 8;
/// Deep samples in a row before acting; one can catch a burst mid-drain.
const BACKLOG_HOLD_SAMPLES: u8 = 2;
/// Sampling cadence for it: one FFI call under the feed lock.
const BACKLOG_SAMPLE: Duration = Duration::from_millis(500);

/// Everything between "an access unit arrived" and "the decoder has been fed", on any backend.
///
/// Backend-blind by construction: it holds a [`VideoSink`] and asks it what it can do
/// ([`VideoSinkCaps`](crate::core::media::VideoSinkCaps)) rather than which one it is.
pub struct VideoStage {
    sink: Box<dyn VideoSink>,
    /// What this backend can be asked to do — read instead of matching on which backend it is.
    caps: VideoSinkCaps,
    /// The audio plane this load produced, if any — kept for its depth reading. The stage
    /// publishes nothing to it: the plane stamps off the player clock on its own.
    audio_plane: Option<std::sync::Arc<dyn AudioPlane>>,
    /// Slice-progressive reassembly state — a pass-through on a backend that doesn't take parts
    /// (see [`AuParts`]).
    parts: AuParts,
    stats: Arc<StreamStats>,
    cfg: SinkConfig,
    /// NDL host-PTS→player-clock mapping — see `session::timeline::Pacing`.
    pacing: Pacing,
    /// The stamp the access unit currently being fed was given, while it is still open. Every piece
    /// of one AU must carry the SAME timestamp (NDL finds AU boundaries by start code and has no
    /// boundary flag of its own), and — the reason this is a field rather than a recomputation — a
    /// picture must be mapped exactly ONCE: slice-progressive delivery repeats an AU's host PTS
    /// across its pieces at increasing arrival times, so re-mapping per piece teaches the mapping
    /// the AU's TAIL arrival and inflates the measured jitter by the AU's own transmission time.
    /// `None` on the whole-AU path and between AUs.
    au_base_ns: Option<u64>,
    last_keyframe_request: Option<Instant>,
    /// Submit time accumulated across the pieces of the AU currently being fed, so the overlay's
    /// `feed_us` stays a figure per PICTURE. Stored per-piece it reported only the last slice,
    /// which on a slice-progressive session is a fraction of the AU's real submission cost.
    au_feed_us: u32,
    /// Pieces handed to the decoder this session. Only meaningful against the completed-AU count
    /// (`frames`): the ratio is the one number that says whether slice-progressive delivery is
    /// doing anything at all on this mode, which cannot be read any other way — core only emits early
    /// parts for an AU spanning more than one FEC block, so at small AU sizes the feature is inert.
    parts_fed: u64,
    /// Freeze-until-reanchor: while holding, frames are skipped rather than fed — the
    /// punch-through plane keeps the last good picture. Resumes on IDR / LTR-RFI recovery
    /// anchor, or two intra-refresh recovery marks. `Some` for exactly as long as the hold lasts
    /// (see [`Self::holding`]).
    hold_started: Option<Instant>,
    /// Intra-refresh wave boundaries observed since the latest loss.
    recovery_marks: u32,
    /// Backpressure sampling (`backpressure`): when the depth was last read, and how many reads
    /// in a row found it past [`BACKLOG_HOLD_FRAMES`].
    backlog_sampled: Option<Instant>,
    deep_samples: u8,
    /// Completed access units fed this session. A plain counter, mirrored into the overlay's cell
    /// by the pump — nothing else writes it.
    frames: u64,
    /// Whether anything reads the diagnostic figures, latched from `StreamStats` on the pump's
    /// heartbeat so the feed path never loads an atomic for it.
    diagnostics: bool,
}

impl VideoStage {
    pub fn new(sink: Box<dyn VideoSink>, stats: Arc<StreamStats>, cfg: SinkConfig) -> Self {
        let stream_hz = cfg.stream_hz.max(1);
        let audio_plane = sink.audio_plane();
        let caps = sink.caps();
        Self {
            parts: AuParts::default(),
            caps,
            sink,
            audio_plane,
            stats,
            // The cushion's ceiling, so it describes the cadence the HOST produces — never the
            // panel's. Core says so with a test of its own
            // (`the_cadence_interval_comes_from_the_stream_mode_not_the_panel`): a 120 fps
            // stream on a 60 Hz panel would otherwise license twice the hold the source's own
            // cadence can justify.
            pacing: Pacing::new(1_000_000_000 / u64::from(stream_hz)),
            au_base_ns: None,
            cfg,
            last_keyframe_request: None,
            hold_started: None,
            recovery_marks: 0,
            backlog_sampled: None,
            deep_samples: 0,
            au_feed_us: 0,
            parts_fed: 0,
            frames: 0,
            diagnostics: false,
        }
    }

    pub fn set_color_info(&self, meta: Option<&quic::HdrMeta>, color: quic::ColorInfo) -> anyhow::Result<()> {
        self.sink.set_color(meta, color)
    }

    /// True when the throttle allows a keyframe request now; stamps it as sent.
    fn take_keyframe_slot(&mut self) -> bool {
        let ready = self
            .last_keyframe_request
            .is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_MIN_INTERVAL);
        if ready {
            self.last_keyframe_request = Some(Instant::now());
        }
        ready
    }

    /// Drop everything derived from a mapping that no longer holds: the host anchor and the audio
    /// plane's copy of it. The two move in lockstep or the planes end up on timelines that
    /// disagree.
    fn reset_timeline(&mut self) {
        self.pacing.reset();
        self.au_base_ns = None;
    }

    /// This frame's stamp in the sink's own clock domain.
    ///
    /// A sink with a clock has no PTS clock of its own (NDL counts from its load),
    /// so the host's capture PTS is mapped onto it (`session::timeline::Pacing`) — which is also what keeps
    /// video and any audio plane in ONE timeline. A sink without one presents in feed order and
    /// the stamp is discarded at the feed, so the host PTS passes through untouched.
    /// This AU's stamp: computed on the piece that opens it and repeated for the rest — see
    /// [`Self::au_base_ns`]. `partial` is whether this piece leaves the AU open.
    fn au_stamp_ns(&mut self, frame_pts_ns: u64, partial: bool) -> u64 {
        let base = match self.au_base_ns {
            Some(open) => open,
            None => self.pts_base_ns(frame_pts_ns),
        };
        self.au_base_ns = partial.then_some(base);
        base
    }

    fn pts_base_ns(&mut self, frame_pts_ns: u64) -> u64 {
        match self.sink.clock() {
            Some(clock) => {
                let now = clock.now_ns();
                self.pacing.map(frame_pts_ns, now)
            }
            None => frame_pts_ns,
        }
    }

    fn begin_hold(&mut self) {
        self.stats.holding.store(true, Ordering::Relaxed);
        self.hold_started.get_or_insert_with(Instant::now);
        self.recovery_marks = 0;
    }

    /// What the live mapping has to say for itself — see [`PacingHealth`]. The whole point of
    /// publishing it on both mappings is that `late_stamps` makes them comparable.
    pub fn pacing_health(&self) -> PacingHealth {
        self.pacing.health()
    }

    /// Audio-plane queue depth in ms, or `None` on a session with no plane — see
    /// `NdlVideo::audio_plane_lead_ms`. Here because it is a *video* symptom: the plane's depth is
    /// what NDL paces the picture on, so it belongs next to the backlog in the video heartbeat.
    pub fn audio_plane_lead_ms(&self) -> Option<i64> {
        self.audio_plane.as_deref().map(AudioPlane::lead_ms)
    }

    /// Completed access units fed this session — see the `frames` field.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Latch whether anything reads the diagnostic figures — see the `diagnostics` field.
    pub fn set_diagnostics(&mut self, on: bool) {
        self.diagnostics = on;
    }

    /// Pieces fed this session — see [`Self::parts_fed`]. Read against the completed-AU count.
    pub fn parts_fed(&self) -> u64 {
        self.parts_fed
    }

    /// Whether a freeze-until-reanchor hold is currently active (stats/logging).
    pub fn holding(&self) -> bool {
        self.hold_started.is_some()
    }

    /// Decoder backlog depth for the heartbeat/overlay, or `None` if the backend has no queue to
    /// read (or the query failed — which must not read as an empty one).
    ///
    /// Diagnostics only: nothing steers on it, so the caller asks only when something is going to
    /// read the answer, and the FFI call rides that cadence rather than one of its own.
    pub fn backlog_depth(&self) -> Option<i32> {
        self.sink.queue_depth().map(|d| i32::try_from(d).unwrap_or(i32::MAX))
    }

    /// Backpressure, sampled on the feed path. A render buffer that stays deep means the decoder
    /// is behind, and feeding on only grows the latency. The response is the loss path's: freeze
    /// and ask for a keyframe, no flush (a flush restart kills the audio plane), so the frames
    /// skipped are ones the decoder had no time for anyway. Returns whether to request one.
    pub fn backpressure(&mut self) -> bool {
        if self.holding() || self.backlog_sampled.is_some_and(|t| t.elapsed() < BACKLOG_SAMPLE) {
            return false;
        }
        self.backlog_sampled = Some(Instant::now());
        let Some(depth) = self.sink.queue_depth() else {
            return false;
        };
        if depth < BACKLOG_HOLD_FRAMES {
            self.deep_samples = 0;
            return false;
        }
        self.deep_samples = self.deep_samples.saturating_add(1);
        if self.deep_samples < BACKLOG_HOLD_SAMPLES {
            return false;
        }
        self.deep_samples = 0;
        tracing::warn!("decoder {depth} frames behind — freezing until the host reanchors");
        self.begin_hold();
        self.take_keyframe_slot()
    }

    /// Present one delivery, or decide not to. The stage owns everything past the wire: how the
    /// pieces of an access unit fit together, whether the timeline holds, and what the decoder's
    /// clock domain calls this frame.
    pub fn submit(&mut self, frame: &WireFrame<'_>) -> SinkResult {
        // A piece of an AU whose predecessor went missing has nowhere to go: the decoder holds a
        // truncated input and only a re-anchor clears it. `Discard` says so; `Feed` carries whether
        // the pieces so far leave this AU decodable.
        let PartStep::Feed { partial, lost_parts } = self.parts.step(frame, self.caps.partial_au) else {
            // Same reason as the `drop_open` below: the AU this was accumulating submission time
            // for is abandoned, so its cost must not be charged to whatever AU comes next. Its
            // stamp goes with it — the next AU is a new picture and maps itself.
            self.au_feed_us = 0;
            self.au_base_ns = None;
            return SinkResult::Held;
        };
        // Only a completed AU is a frame — a session feeding pieces would otherwise count one
        // picture several times, and the overlay reads this figure as pictures per second.
        if partial {
            self.parts_fed += 1;
        } else {
            self.frames += 1;
        }
        let flags = FrameFlags {
            reanchor: frame.reanchor,
            recovery_mark: frame.recovery_mark,
            loss: frame.loss || lost_parts,
            index: u64::from(frame.index),
            partial,
        };
        let result = self.feed(frame.data, frame.pts_ns, flags);
        // Either the piece never reached the decoder (a hold swallows it, an error refused it) or
        // it did and a keyframe has just been asked for regardless — on both paths the AU cannot
        // be completed. Forgetting it costs the rest of one AU; keeping it would eventually feed a
        // frame with a hole in it.
        if !matches!(result, SinkResult::Presented { .. }) {
            self.parts.drop_open();
            // The AU this was accumulating for will never complete, so it must not be added to
            // whatever AU comes next — nor may its stamp be repeated onto one.
            self.au_feed_us = 0;
            self.au_base_ns = None;
        }
        result
    }

    fn feed(&mut self, au: &[u8], pts_ns: u64, flags: FrameFlags) -> SinkResult {
        // Checked before the gate, not inside it: a dead decoder also fails every `flush` the hold
        // path takes, so the hold would keep re-deciding this per frame.
        if self.sink.is_dead() {
            return SinkResult::Dead;
        }
        let request_keyframe = match self.gate(&flags) {
            HoldGate::Skip(result) => return result,
            HoldGate::Feed { request_keyframe } => request_keyframe,
        };

        let base_ns = self.au_stamp_ns(pts_ns, flags.partial);
        // The feed is timed only where something reads the figure: the host's ABR controller, or
        // the overlay. With neither listening it is untimed and the backpressure warning below
        // cannot fire — turn the overlay on to get it back.
        let timed = self.cfg.report_decode_latency || self.diagnostics;
        let feed_start = timed.then(Instant::now);
        let play_result = self.sink.feed(au, base_ns);
        if let Some(elapsed) = feed_start.map(|t| t.elapsed()) {
            self.au_feed_us = self
                .au_feed_us
                .saturating_add(u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX));
            if elapsed >= FEED_BACKPRESSURE_WARN {
                tracing::warn!(
                    "NDL slow: {:.1}ms (frame {}, pts {:.2}ms)",
                    elapsed.as_secs_f32() * 1000.0,
                    flags.index,
                    ms(base_ns),
                );
            }
        }
        // Read before the reset below, because BOTH consumers are per PICTURE: the overlay's
        // figure and the ABR submission term. Taking the last slice's elapsed time for the
        // latter reports a fraction of the AU's real cost on a slice-progressive session.
        let au_feed_us = self.au_feed_us;
        if !flags.partial {
            if timed {
                self.stats.feed_us.store(au_feed_us, Ordering::Relaxed);
            }
            self.au_feed_us = 0;
        }

        let (decode_us, failed_keyframe) = match play_result {
            Ok(()) if flags.partial => (None, false),
            Ok(()) => {
                // NDL exposes no decoded-output callback. Its render-buffer depth is presentation
                // lead, not decoder latency, and feeding it into ABR created false learned caps at
                // 4K120. `play` duration is the only measured decoder-pressure signal available:
                // when input backpressures, it rises naturally.
                let decode_us = self.cfg.report_decode_latency.then_some(au_feed_us);
                (decode_us, false)
            }
            Err(e) => (None, self.on_play_error(&e, &flags, base_ns)),
        };

        if request_keyframe || failed_keyframe {
            SinkResult::NeedKeyframe
        } else {
            SinkResult::Presented { decode_us }
        }
    }

    /// The freeze-until-reanchor gate, run before every feed.
    fn gate(&mut self, flags: &FrameFlags) -> HoldGate {
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
            return HoldGate::Feed {
                request_keyframe: false,
            };
        };
        if flags.recovery_mark {
            self.recovery_marks = self.recovery_marks.saturating_add(1);
        }
        let request_keyframe = self.take_keyframe_slot();
        let recovered = flags.reanchor || self.recovery_marks >= punktfunk_core::reanchor::REANCHOR_MARKS_TO_LIFT;
        if !recovered {
            return HoldGate::Skip(if request_keyframe {
                SinkResult::NeedKeyframe
            } else {
                SinkResult::Held
            });
        }
        tracing::info!(
            "resuming after {:.0}ms (frame {}, reanchor={}, recovery_marks={})",
            started.elapsed().as_secs_f32() * 1000.0,
            flags.index,
            flags.reanchor,
            self.recovery_marks,
        );
        // The real timeline just jumped (freeze then reanchor) — nothing about
        // the pre-hold accumulator is worth continuing.
        self.reset_timeline();
        self.stats.holding.store(false, Ordering::Relaxed);
        self.hold_started = None;
        self.recovery_marks = 0;
        HoldGate::Feed { request_keyframe }
    }

    /// Handles a refused feed; returns whether to ask the host for a keyframe.
    fn on_play_error(&mut self, e: &anyhow::Error, flags: &FrameFlags, base_ns: u64) -> bool {
        tracing::warn!(
            "{} error (frame {}, pts {:.2}ms): {e:#}",
            self.sink.name(),
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
            if self.caps.flush {
                let _ = self.sink.flush();
            }
            self.begin_hold();
        }
        true
    }
}

/// What [`VideoStage::gate`] decided about this frame.
enum HoldGate {
    /// Feed it. `request_keyframe` is set when a hold released on this frame and the throttle
    /// allowed asking for one — the frame is still fed, but the request is what gets reported.
    Feed { request_keyframe: bool },
    /// Still frozen — skip it, and report this instead.
    Skip(SinkResult),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A decoder that accepts everything and reports whatever depth the test sets.
    struct FakeSink {
        depth: Cell<Option<u32>>,
    }

    // `Cell` is `!Sync`; the trait wants `Send` only and the test never shares it.
    impl VideoSink for FakeSink {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> VideoSinkCaps {
            VideoSinkCaps::FEED_ONLY
        }
        fn feed(&self, _au: &[u8], _pts_ns: u64) -> anyhow::Result<()> {
            Ok(())
        }
        fn queue_depth(&self) -> Option<u32> {
            self.depth.get()
        }
    }

    fn stage(depth: Option<u32>) -> VideoStage {
        VideoStage::new(
            Box::new(FakeSink {
                depth: Cell::new(depth),
            }),
            Arc::new(StreamStats::default()),
            SinkConfig {
                stream_hz: 60,
                report_decode_latency: false,
            },
        )
    }

    fn frame(index: u32, reanchor: bool) -> WireFrame<'static> {
        WireFrame {
            data: &[0u8; 4],
            pts_ns: u64::from(index) * 16_666_667,
            index,
            part: None,
            reanchor,
            recovery_mark: false,
            loss: false,
        }
    }

    /// Two deep samples freeze the feed and ask for a keyframe; the reanchor lifts the hold.
    /// One deep sample, or a backend with no queue, never does.
    #[test]
    fn a_deep_render_buffer_freezes_until_the_host_reanchors() {
        let mut s = stage(Some(BACKLOG_HOLD_FRAMES));
        assert!(!s.backpressure(), "one deep sample is a burst mid-drain");
        s.backlog_sampled = None;
        assert!(s.backpressure(), "the second asks for a keyframe");
        assert!(s.holding());
        assert!(matches!(s.submit(&frame(1, false)), SinkResult::Held));
        assert!(matches!(s.submit(&frame(2, true)), SinkResult::Presented { .. }));
        assert!(!s.holding());

        let mut shallow = stage(Some(1));
        shallow.backlog_sampled = None;
        assert!(!shallow.backpressure());
        shallow.backlog_sampled = None;
        assert!(!shallow.backpressure());
        assert!(!stage(None).backpressure(), "no queue to read, nothing to steer on");
    }
}
