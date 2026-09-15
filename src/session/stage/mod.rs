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

use pf_client_core::trust::PresentPriority;
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
    /// Loss was detected at or before this frame — a sequence gap, or a frame the transport
    /// dropped.
    pub loss: bool,
}

/// What the stage worked out about one delivery before feeding it.
#[derive(Clone, Copy)]
struct FrameFlags {
    reanchor: bool,
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
    /// Nothing reached the decoder — frozen, or the play was refused — and no keyframe request
    /// is due yet.
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
    /// Negotiated host cadence — never the panel's: it is the adaptive cushion's ceiling in
    /// [`Pacing`], and what converts the smoothness budget into time.
    pub stream_hz: u32,
    pub present_priority: PresentPriority,
    /// Whether the host asked for decode-latency reports (its ABR controller).
    pub report_decode_latency: bool,
}

/// Minimum spacing between [`SinkResult::NeedKeyframe`] results: the request travels on its own
/// QUIC control stream, so a tight interval costs nothing but the request itself.
const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Conservative recovery threshold for sustained reported backlog. This opaque count does
/// not reliably measure timestamp headroom; observed smooth and stuttery sessions both read 0–1.
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
    /// punch-through plane keeps the last good picture. Resumes on an IDR or an LTR-RFI
    /// recovery anchor only. `Some` for exactly as long as the hold lasts (see [`Self::holding`]).
    hold_started: Option<Instant>,
    /// Backpressure sampling (`backpressure`): when the depth was last read, and how many reads
    /// in a row found it past [`BACKLOG_HOLD_FRAMES`].
    backlog_sampled: Option<Instant>,
    /// The depth that sampling last saw, so the heartbeat's diagnostic can read a figure the
    /// control path already paid for instead of taking NDL's lock again — see
    /// [`Self::backlog_depth`].
    last_backlog: Option<u32>,
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
        let paced = sink.clock().is_some() && audio_plane.is_some();
        let priority = match cfg.present_priority {
            PresentPriority::Smooth { .. } if !paced => {
                tracing::warn!("smoothness unavailable: decoder has no paced audio/video timeline");
                PresentPriority::Latency
            }
            p => p,
        };
        let pacing = Pacing::new(1_000_000_000 / u64::from(stream_hz), priority);
        tracing::info!(?priority, stream_hz, "video presentation");
        Self {
            parts: AuParts::default(),
            caps,
            sink,
            audio_plane,
            stats,
            pacing,
            au_base_ns: None,
            cfg,
            last_keyframe_request: None,
            hold_started: None,
            backlog_sampled: None,
            last_backlog: None,
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
    }

    /// What the live mapping has to say for itself — see [`PacingHealth`]. The whole point of
    /// publishing it on both mappings is that `late_stamps` makes them comparable.
    pub fn pacing_health(&self) -> PacingHealth {
        self.pacing.health()
    }

    /// The window's tightest complete-AU deadline margin — see [`Pacing::note_submitted`]. Only
    /// ever `Some` while the feed is timed (`report_decode_latency || diagnostics`), since that is
    /// what gates the submission clock read.
    pub fn take_pacing_slack_us(&mut self) -> Option<i32> {
        self.pacing.take_min_slack_us()
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

    /// Decoder backlog depth for the heartbeat/overlay, or `None` before the first sample (or on a
    /// backend with no queue to read — which must not be reported as an empty one).
    ///
    /// **Reads [`Self::backpressure`]'s last sample rather than querying NDL.** The query is an FFI
    /// call behind the lock the picture's own feed needs, and the control path already makes it
    /// every [`BACKLOG_SAMPLE`]; a diagnostic must not take that lock a second time on the video
    /// thread. The figure is therefore up to one sample interval old, which is well inside the
    /// heartbeat's own cadence. A hold suspends sampling, so it also goes stale for the length of
    /// one — `holding` is published beside it and says so.
    pub fn backlog_depth(&self) -> Option<i32> {
        self.last_backlog.map(|d| i32::try_from(d).unwrap_or(i32::MAX))
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
        let depth = self.sink.queue_depth();
        self.last_backlog = depth;
        let Some(depth) = depth else {
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
            self.abandon_open_au();
            return SinkResult::Held;
        };
        let flags = FrameFlags {
            reanchor: frame.reanchor,
            loss: frame.loss || lost_parts,
            index: u64::from(frame.index),
            partial,
        };
        let result = self.feed(frame.data, frame.pts_ns, flags);
        if matches!(result, SinkResult::Presented { .. }) {
            // Counted once the decoder took it, never on arrival: the overlay reads `frames` as
            // pictures per second, and a held delivery is not one. Only a completed AU is a
            // picture — pieces would count one several times.
            if partial {
                self.parts_fed += 1;
            } else {
                self.frames += 1;
            }
        } else {
            // The piece never reached the decoder (a hold swallowed it, an error refused it, the
            // decoder is gone), so the AU cannot be completed. Forgetting it costs the rest of
            // one AU; keeping it would eventually feed a frame with a hole in it.
            self.parts.drop_open();
            self.abandon_open_au();
        }
        result
    }

    /// Forget the AU currently open: its accumulated submission time must not be charged to
    /// whatever AU comes next, and its stamp must not be repeated onto one — the next AU is a new
    /// picture and maps itself.
    fn abandon_open_au(&mut self) {
        self.au_feed_us = 0;
        self.au_base_ns = None;
    }

    fn feed(&mut self, au: &[u8], pts_ns: u64, flags: FrameFlags) -> SinkResult {
        // Checked before the gate, not inside it: a dead decoder also fails every `flush` the hold
        // path takes, so the hold would keep re-deciding this per frame.
        if self.sink.is_dead() {
            return SinkResult::Dead;
        }
        if let HoldGate::Skip(result) = self.gate(&flags) {
            return result;
        }

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

        match play_result {
            Ok(()) if flags.partial => SinkResult::Presented { decode_us: None },
            Ok(()) => {
                // Behind `timed` like every other figure here: the clock read is a per-picture
                // vDSO call, and nothing reads `late_submit` unless the diagnostics are on.
                if timed {
                    if let Some(clock) = self.sink.clock() {
                        self.pacing.note_submitted(base_ns, clock.now_ns());
                    }
                }
                // NDL exposes no decoded-output callback. Render depth measures neither decode
                // duration nor reliable presentation headroom, so ABR uses timed feed calls
                // as a decoder-pressure proxy instead.
                SinkResult::Presented {
                    decode_us: self.cfg.report_decode_latency.then_some(au_feed_us),
                }
            }
            // A refused piece was not presented whatever the throttle says; `Held` keeps the
            // caller's AU bookkeeping honest where a request is not due yet.
            Err(e) => {
                if self.on_play_error(&e, &flags, base_ns) {
                    SinkResult::NeedKeyframe
                } else {
                    SinkResult::Held
                }
            }
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
            return HoldGate::Feed;
        };
        // An IDR or an RFI anchor predicts from nothing NDL lacks. An intra-refresh wave heals
        // only a decoder that decoded every frame of it, and the hold skipped those: lifting on
        // its marks would feed NDL a picture whose references it never saw.
        let recovered = flags.reanchor;
        if !recovered {
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
        // The real timeline just jumped (freeze then reanchor) — nothing about
        // the pre-hold accumulator is worth continuing.
        self.reset_timeline();
        self.stats.holding.store(false, Ordering::Relaxed);
        self.hold_started = None;
        HoldGate::Feed
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
    /// Feed it — not holding, or this is the frame that lifts the hold.
    Feed,
    /// Still frozen — skip it, and report this instead.
    Skip(SinkResult),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A decoder that takes pieces, accepts everything (or refuses everything as not loaded), and
    /// reports whatever depth the test sets.
    struct FakeSink {
        depth: Cell<Option<u32>>,
        refuse: bool,
    }

    // `Cell` is `!Sync`; the trait wants `Send` only and the test never shares it.
    impl VideoSink for FakeSink {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> VideoSinkCaps {
            VideoSinkCaps {
                pts: true,
                partial_au: true,
                flush: false,
            }
        }
        fn feed(&self, _au: &[u8], _pts_ns: u64) -> anyhow::Result<()> {
            if self.refuse {
                return Err(NotReady.into());
            }
            Ok(())
        }
        fn queue_depth(&self) -> Option<u32> {
            self.depth.get()
        }
    }

    fn stage(depth: Option<u32>) -> VideoStage {
        stage_on(FakeSink {
            depth: Cell::new(depth),
            refuse: false,
        })
    }

    fn stage_on(sink: FakeSink) -> VideoStage {
        VideoStage::new(
            Box::new(sink),
            Arc::new(StreamStats::default()),
            SinkConfig {
                stream_hz: 60,
                present_priority: PresentPriority::Latency,
                report_decode_latency: false,
            },
        )
    }

    fn frame(index: u32, part: Option<(bool, bool, u32)>, reanchor: bool, loss: bool) -> WireFrame<'static> {
        WireFrame {
            data: &[0u8; 4],
            pts_ns: u64::from(index) * 16_666_667,
            index,
            part: part.map(|(first, last, offset)| punktfunk_core::session::FramePart { offset, first, last }),
            reanchor,
            loss,
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
        assert!(matches!(s.submit(&frame(1, None, false, false)), SinkResult::Held));
        assert!(matches!(
            s.submit(&frame(2, None, true, false)),
            SinkResult::Presented { .. }
        ));
        assert!(!s.holding());

        let mut shallow = stage(Some(1));
        shallow.backlog_sampled = None;
        assert!(!shallow.backpressure());
        shallow.backlog_sampled = None;
        assert!(!shallow.backpressure());
        assert!(!stage(None).backpressure(), "no queue to read, nothing to steer on");
    }

    /// A loss hold lifts on a reanchor alone. Its frames never reach NDL, so an intra-refresh
    /// wave cannot heal the picture NDL would resume on; an IDR or an RFI anchor predicts only
    /// from pictures NDL still holds.
    #[test]
    fn a_loss_hold_lifts_on_a_reanchor_alone() {
        let mut s = stage(None);
        assert!(matches!(
            s.submit(&frame(1, None, false, true)),
            SinkResult::NeedKeyframe
        ));
        for index in 2..40 {
            assert!(
                !matches!(
                    s.submit(&frame(index, None, false, false)),
                    SinkResult::Presented { .. }
                ),
                "frame {index} reached NDL during the hold"
            );
        }
        assert!(s.holding());
        assert!(matches!(
            s.submit(&frame(40, None, true, false)),
            SinkResult::Presented { .. }
        ));
        assert!(!s.holding());
    }

    /// A keyframe lifting a hold is fed whole, however long since the last request — reporting a
    /// request on its first piece made `submit` abandon the AU and re-arm the hold on the next.
    #[test]
    fn the_resume_keyframe_keeps_its_au_open() {
        let mut s = stage(None);
        assert!(matches!(
            s.submit(&frame(1, None, false, true)),
            SinkResult::NeedKeyframe
        ));
        assert!(s.holding());
        // The throttle has long expired by the time the host's keyframe lands.
        s.last_keyframe_request = None;
        let before = s.frames();
        assert!(matches!(
            s.submit(&frame(2, Some((true, false, 0)), true, false)),
            SinkResult::Presented { .. }
        ));
        assert!(!s.holding());
        assert!(matches!(
            s.submit(&frame(2, Some((false, true, 4)), true, false)),
            SinkResult::Presented { .. }
        ));
        assert_eq!(s.frames(), before + 1, "two pieces, one picture");
        assert!(matches!(
            s.submit(&frame(3, None, false, false)),
            SinkResult::Presented { .. }
        ));
        assert!(!s.holding(), "the next AU must not read the resumed one as lost");
    }

    /// `frames` is pictures the decoder took: a held delivery is not one, and neither is a
    /// refused play — which reports `Held`, not `Presented`, while its request is throttled.
    #[test]
    fn only_a_fed_picture_counts() {
        let mut s = stage(None);
        assert!(matches!(
            s.submit(&frame(1, None, false, true)),
            SinkResult::NeedKeyframe
        ));
        assert_eq!(s.frames(), 0, "held on arrival");
        assert!(matches!(
            s.submit(&frame(2, None, true, false)),
            SinkResult::Presented { .. }
        ));
        assert_eq!(s.frames(), 1);

        let mut r = stage_on(FakeSink {
            depth: Cell::new(None),
            refuse: true,
        });
        assert!(matches!(
            r.submit(&frame(1, None, false, false)),
            SinkResult::NeedKeyframe
        ));
        assert!(matches!(r.submit(&frame(2, None, false, false)), SinkResult::Held));
        assert_eq!(r.frames(), 0, "refused twice");
    }
}
