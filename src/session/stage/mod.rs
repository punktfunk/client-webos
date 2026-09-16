//! The single place that talks to the video decoder.
//!
//! Everything between "an access unit arrived" and "NDL has been fed" lives here: host-PTS
//! mapping on the refresh-rate-reconciled frame interval, backlog sampling,
//! freeze-until-reanchor, and keyframe-request throttling. The video pump keeps only the
//! parts that are wire-shaped — pulling frames, and *how* a keyframe is asked for, which it
//! answers to [`SinkResult::NeedKeyframe`] with `NativeClient::request_keyframe`.
//!
//! Slice-progressive reassembly sits in [`parts`], on its own so it stays testable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pf_client_core::trust::PresentPriority;
use punktfunk_core::quic;

use crate::core::media::{AudioPlane, VideoSink, VideoSinkCaps};
use crate::session::timeline::{ms, CadenceTrace, Pacing, PacingHealth};
use crate::session::StreamStats;

mod parts;
mod recovery;
mod timing;

use parts::{AuParts, PartStep};
use recovery::{HoldGate, Recovery};
use timing::FeedTiming;

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
    /// One piece of a slice-progressive AU; decoder takes it but can't present. Reference points
    /// hang off the piece that completes the AU. `false` on whole-AU path.
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
#[derive(Clone, Copy)]
pub struct SinkConfig {
    /// Negotiated host cadence — never the panel's: it is the adaptive cushion's ceiling in
    /// [`Pacing`], and what converts the smoothness budget into time.
    pub stream_hz: u32,
    pub present_priority: PresentPriority,
    /// Whether the host asked for decode-latency reports (its ABR controller).
    pub report_decode_latency: bool,
}

/// Sampling cadence for the backlog diagnostic: one FFI call under the feed lock.
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
    /// NDL host-PTS→player-clock mapping — see `session::timeline::Pacing`.
    pacing: Pacing,
    /// AU's mapped timestamp, held open across its pieces. Mapped once per AU; repeated mapping
    /// per piece teaches the mapping the AU's tail arrival and inflates jitter.
    au_base_ns: Option<u64>,
    /// Hold state and the keyframe throttle — see [`Recovery`].
    recovery: Recovery,
    /// Whether the feed path reads a clock, and the open picture's submit cost — see [`FeedTiming`].
    timing: FeedTiming,
    /// Pieces fed this session. Ratio to `frames` shows whether slice-progressive delivery is active
    /// (core only emits early parts for AU spanning >1 FEC block).
    parts_fed: u64,
    /// When the backlog depth was last sampled — see [`Self::sample_backlog`].
    backlog_sampled: Option<Instant>,
    /// The depth that sampling last saw, so the heartbeat's diagnostic can read a figure the
    /// control path already paid for instead of taking NDL's lock again — see
    /// [`Self::backlog_depth`].
    last_backlog: Option<u32>,
    /// Completed access units fed this session. A plain counter, mirrored into the overlay's cell
    /// by the pump — nothing else writes it.
    frames: u64,
}

impl VideoStage {
    pub fn new(sink: Box<dyn VideoSink>, stats: Arc<StreamStats>, cfg: &SinkConfig) -> Self {
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
        let timing = FeedTiming::new(cfg.report_decode_latency);
        let recovery = Recovery::new(Arc::clone(&stats));
        tracing::info!(?priority, stream_hz, "video presentation");
        Self {
            parts: AuParts::default(),
            caps,
            sink,
            audio_plane,
            stats,
            pacing,
            au_base_ns: None,
            recovery,
            backlog_sampled: None,
            last_backlog: None,
            timing,
            parts_fed: 0,
            frames: 0,
        }
    }

    pub fn set_color_info(&self, meta: Option<&quic::HdrMeta>, color: quic::ColorInfo) -> anyhow::Result<()> {
        self.sink.set_color(meta, color)
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

    /// What the live mapping has to say for itself — see [`PacingHealth`]. The whole point of
    /// publishing it on both mappings is that `late_stamps` makes them comparable.
    pub fn pacing_health(&self) -> PacingHealth {
        self.pacing.health()
    }

    /// Window's tightest complete-AU deadline margin. `Some` only while feed is timed.
    pub fn take_pacing_slack_us(&mut self) -> Option<i32> {
        self.pacing.take_min_slack_us()
    }

    /// The window's source/mapped/emitted cadence — see [`CadenceTrace`]. Take-and-re-arm, so it
    /// belongs on the heartbeat next to the slack figure and nowhere else.
    pub fn take_cadence_trace(&mut self) -> CadenceTrace {
        self.pacing.take_trace()
    }

    /// Audio plane queue depth (ms). Published here because plane depth paces the video.
    pub fn audio_plane_lead_ms(&self) -> Option<i64> {
        self.audio_plane.as_deref().map(AudioPlane::lead_ms)
    }

    /// Completed access units fed this session — see the `frames` field.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Latch whether anything reads the diagnostic figures — see [`FeedTiming`].
    pub fn set_diagnostics(&mut self, on: bool) {
        self.timing.set_diagnostics(on);
    }

    /// Pieces fed this session — see [`Self::parts_fed`]. Read against the completed-AU count.
    pub fn parts_fed(&self) -> u64 {
        self.parts_fed
    }

    /// Whether a freeze-until-reanchor hold is currently active (stats/logging). A SNAPSHOT — for
    /// whether one happened at all, see [`Self::hold_totals`].
    pub fn holding(&self) -> bool {
        self.recovery.holding()
    }

    /// Holds begun and total time held this session, for the heartbeat to difference into a window.
    pub fn hold_totals(&self) -> (u64, Duration) {
        self.recovery.hold_totals()
    }

    /// Decoder backlog depth for heartbeat/overlay, `None` before first sample or if backend
    /// has no queue. Reads [`Self::sample_backlog`]'s last reading; re-querying NDL would
    /// double-lock the video thread. Figure is ≤1 sample interval old. A hold stales it.
    pub fn backlog_depth(&self) -> Option<i32> {
        self.last_backlog.map(|d| i32::try_from(d).unwrap_or(i32::MAX))
    }

    /// Sample decoder backlog for the heartbeat, at most once per [`BACKLOG_SAMPLE`].
    ///
    /// **Diagnostic only** — used to drive a freeze but proved unreliable (smooth and stuttery
    /// sessions both read 0-1). Sampling preserved: heartbeat reads `last_backlog` which only
    /// this updates. Skipped while holding (held stage feeds nothing, depth is meaningless).
    pub fn sample_backlog(&mut self) {
        if self.holding() || self.backlog_sampled.is_some_and(|t| t.elapsed() < BACKLOG_SAMPLE) {
            return;
        }
        self.backlog_sampled = Some(Instant::now());
        self.last_backlog = self.sink.queue_depth();
    }

    /// Present one delivery or skip it. Owns AU reassembly, timeline, and timestamp mapping.
    pub fn submit(&mut self, frame: &WireFrame<'_>) -> SinkResult {
        // AU with missing predecessor is undecodable; only reanchor clears it.
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
            // Count once decoded, not on arrival. Overlay reads frames as pictures/sec.
            if partial {
                self.parts_fed += 1;
            } else {
                self.frames += 1;
            }
        } else {
            // Piece didn't reach decoder; can't complete AU. Truncating it prevents holes.
            self.parts.drop_open();
            self.abandon_open_au();
        }
        result
    }

    /// Forget the AU currently open: its accumulated submission time must not be charged to
    /// whatever AU comes next, and its stamp must not be repeated onto one — the next AU is a new
    /// picture and maps itself.
    fn abandon_open_au(&mut self) {
        self.timing.abandon();
        self.au_base_ns = None;
    }

    fn feed(&mut self, au: &[u8], pts_ns: u64, flags: FrameFlags) -> SinkResult {
        // Check before gate; otherwise hold re-decides this on every frame.
        if self.sink.is_dead() {
            return SinkResult::Dead;
        }
        match self.recovery.gate(&flags) {
            HoldGate::Feed => {}
            // The real timeline just jumped (freeze then reanchor) — nothing about the pre-hold
            // accumulator is worth continuing.
            HoldGate::Resume => self.reset_timeline(),
            HoldGate::Skip(result) => return result,
        }

        let base_ns = self.au_stamp_ns(pts_ns, flags.partial);
        let started = self.timing.begin();
        let play_result = self.sink.feed(au, base_ns);
        if let Some(slow) = self.timing.end(started) {
            tracing::warn!(
                "NDL slow: {:.1}ms (frame {}, pts {:.2}ms)",
                slow.as_secs_f32() * 1000.0,
                flags.index,
                ms(base_ns),
            );
        }
        // Read before AU closes; both consumers are per-picture: overlay and ABR.
        let au_feed_us = if flags.partial {
            self.timing.open_au_us()
        } else {
            self.timing.finish_au(&self.stats)
        };

        match play_result {
            Ok(()) if flags.partial => SinkResult::Presented { decode_us: None },
            Ok(()) => {
                // Clock read is per-picture vDSO; guarded by timing gate.
                if self.timing.timed() {
                    if let Some(clock) = self.sink.clock() {
                        self.pacing.note_submitted(base_ns, clock.now_ns());
                    }
                }
                // No decoded-output callback; feed latency proxies decoder pressure for ABR.
                SinkResult::Presented {
                    decode_us: self.timing.reports_decode().then_some(au_feed_us),
                }
            }
            // Refused piece wasn't presented; Held keeps caller's AU bookkeeping honest.
            Err(e) => {
                if self
                    .recovery
                    .on_play_error(&e, &flags, base_ns, self.sink.as_ref(), self.caps)
                {
                    SinkResult::NeedKeyframe
                } else {
                    SinkResult::Held
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Mutex;

    use crate::core::media::{AudioFormat, MediaClock, NotReady, Samples};

    /// Hand-driven clock for deterministic mapping. Without it, sink reports no clock.
    #[derive(Default)]
    struct FakeClock {
        now: AtomicU64,
        /// Clock reads count [`Pacing::map`] calls — observable form of once-per-picture invariant.
        reads: AtomicU64,
    }

    impl FakeClock {
        fn set(&self, ns: u64) {
            self.now.store(ns, Ordering::Relaxed);
        }
        fn maps(&self) -> u64 {
            self.reads.load(Ordering::Relaxed)
        }
    }

    impl MediaClock for FakeClock {
        fn now_ns(&self) -> u64 {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.now.load(Ordering::Relaxed)
        }
    }

    /// Stub plane to enable Smoothness mode. Only checks for clock + plane existence.
    struct FakePlane;

    impl crate::core::media::AudioSink for FakePlane {
        fn name(&self) -> &'static str {
            "fake-plane"
        }
        fn format(&self) -> AudioFormat {
            AudioFormat::Opus { channels: 2 }
        }
        fn feed(&self, _samples: Samples<'_>, _host_pts_ns: u64) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl AudioPlane for FakePlane {
        fn lead_ms(&self) -> i64 {
            0
        }
        fn run_keepalive(&self, _stop: &AtomicBool, _yields_to_real: bool) {}
    }

    /// Test decoder. Takes pieces, accepts/refuses per test config, reports depth, records stamps.
    struct FakeSink {
        depth: Cell<Option<u32>>,
        refuse: bool,
        clock: Arc<FakeClock>,
        plane: Option<Arc<dyn AudioPlane>>,
        fed: Mutex<Vec<u64>>,
    }

    impl Default for FakeSink {
        fn default() -> Self {
            Self {
                depth: Cell::new(None),
                refuse: false,
                clock: Arc::new(FakeClock::default()),
                plane: None,
                fed: Mutex::new(Vec::new()),
            }
        }
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
        fn feed(&self, _au: &[u8], pts_ns: u64) -> anyhow::Result<()> {
            if self.refuse {
                return Err(NotReady.into());
            }
            self.fed.lock().expect("fed").push(pts_ns);
            Ok(())
        }
        fn queue_depth(&self) -> Option<u32> {
            self.depth.get()
        }
        fn clock(&self) -> Option<&dyn MediaClock> {
            Some(self.clock.as_ref())
        }
        fn audio_plane(&self) -> Option<Arc<dyn AudioPlane>> {
            self.plane.clone()
        }
    }

    /// Stage + references test needs after handing the sink over.
    struct Harness {
        stage: VideoStage,
        clock: Arc<FakeClock>,
        fed: Arc<Mutex<Vec<u64>>>,
    }

    /// Distinct stamps in feed order. Slice-progressive pieces collapse to one mapping.
    impl Harness {
        fn distinct_stamps(&self) -> Vec<u64> {
            let fed = self.fed.lock().expect("fed");
            let mut out: Vec<u64> = Vec::new();
            for &s in fed.iter() {
                if out.last() != Some(&s) {
                    out.push(s);
                }
            }
            out
        }
    }

    fn stage(depth: Option<u32>) -> VideoStage {
        harness_on(FakeSink {
            depth: Cell::new(depth),
            ..FakeSink::default()
        })
        .stage
    }

    fn stage_on(sink: FakeSink) -> VideoStage {
        harness_on(sink).stage
    }

    fn harness() -> Harness {
        harness_on(FakeSink::default())
    }

    fn harness_on(sink: FakeSink) -> Harness {
        harness_with(sink, PresentPriority::Latency)
    }

    fn harness_with(sink: FakeSink, present_priority: PresentPriority) -> Harness {
        let clock = Arc::clone(&sink.clock);
        // Recorder outlives the boxed sink; kept as Arc before sink transfer.
        let fed = Arc::new(Mutex::new(Vec::new()));
        let sink = FakeSink {
            fed: Mutex::new(Vec::new()),
            ..sink
        };
        let recorder = Arc::clone(&fed);
        let sink = RecordingSink { inner: sink, recorder };
        let stage = VideoStage::new(
            Box::new(sink),
            Arc::new(StreamStats::default()),
            &SinkConfig {
                stream_hz: 60,
                present_priority,
                report_decode_latency: false,
            },
        );
        Harness { stage, clock, fed }
    }

    /// Wraps `FakeSink` to keep a handle on stamps post-ownership transfer.
    struct RecordingSink {
        inner: FakeSink,
        recorder: Arc<Mutex<Vec<u64>>>,
    }

    impl VideoSink for RecordingSink {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn caps(&self) -> VideoSinkCaps {
            self.inner.caps()
        }
        fn feed(&self, au: &[u8], pts_ns: u64) -> anyhow::Result<()> {
            self.inner.feed(au, pts_ns)?;
            self.recorder.lock().expect("fed").push(pts_ns);
            Ok(())
        }
        fn queue_depth(&self) -> Option<u32> {
            self.inner.queue_depth()
        }
        fn clock(&self) -> Option<&dyn MediaClock> {
            self.inner.clock()
        }
        fn audio_plane(&self) -> Option<Arc<dyn AudioPlane>> {
            self.inner.audio_plane()
        }
        fn is_dead(&self) -> bool {
            self.inner.is_dead()
        }
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

    /// The sampler feeds the heartbeat and steers nothing: a depth that would once have frozen the
    /// stage now only updates the reading, and a backend with no queue still reports `None` rather
    /// than an empty one.
    #[test]
    fn the_backlog_sampler_only_reports() {
        let mut s = stage(Some(64));
        assert_eq!(s.backlog_depth(), None, "nothing sampled yet");
        s.sample_backlog();
        assert_eq!(s.backlog_depth(), Some(64));
        assert!(!s.holding(), "a deep queue no longer freezes the feed");
        assert!(matches!(
            s.submit(&frame(1, None, true, false)),
            SinkResult::Presented { .. }
        ));

        let mut none = stage(None);
        none.sample_backlog();
        assert_eq!(none.backlog_depth(), None, "no queue is not an empty queue");
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

    /// Resuming keyframe is fed whole across its pieces without re-arming the hold.
    #[test]
    fn the_resume_keyframe_keeps_its_au_open() {
        let mut s = stage(None);
        assert!(matches!(
            s.submit(&frame(1, None, false, true)),
            SinkResult::NeedKeyframe
        ));
        assert!(s.holding());
        // Throttle expired by host keyframe arrival.
        s.recovery.clear_keyframe_throttle();
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

    /// Only decoded pictures increment `frames`. Held and refused deliveries don't.
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
            refuse: true,
            ..FakeSink::default()
        });
        assert!(matches!(
            r.submit(&frame(1, None, false, false)),
            SinkResult::NeedKeyframe
        ));
        assert!(matches!(r.submit(&frame(2, None, false, false)), SinkResult::Held));
        assert_eq!(r.frames(), 0, "refused twice");
    }

    /// Picture maps once per arrival, regardless of piece count. Pinned control-flow invariant.
    #[test]
    fn a_picture_maps_exactly_once_however_many_pieces_it_has() {
        let h = &mut harness();
        for (i, index) in (1u32..=4).enumerate() {
            h.clock.set(i as u64 * 16_666_667);
            for piece in 0..3u32 {
                let part = Some((piece == 0, piece == 2, piece * 4));
                assert!(matches!(
                    h.stage.submit(&frame(index, part, index == 1, false)),
                    SinkResult::Presented { .. }
                ));
            }
        }
        assert_eq!(h.stage.frames(), 4);
        assert_eq!(h.stage.parts_fed(), 8, "two non-final pieces per picture");
        assert_eq!(h.clock.maps(), 4, "one mapping per picture, not per piece");
        assert_eq!(h.distinct_stamps().len(), 4, "every piece of an AU shares its stamp");
    }

    /// Abandoned AU stamps don't leak to next picture. Discarded remainder doesn't re-map.
    #[test]
    fn an_abandoned_au_maps_once_and_leaves_no_stamp_behind() {
        let h = &mut harness();
        assert!(matches!(
            h.stage.submit(&frame(1, Some((true, false, 0)), true, false)),
            SinkResult::Presented { .. }
        ));
        // Wrong offset kills AU; successors are discarded.
        assert!(matches!(
            h.stage.submit(&frame(1, Some((false, false, 99)), false, false)),
            SinkResult::Held
        ));
        assert!(matches!(
            h.stage.submit(&frame(1, Some((false, true, 8)), false, false)),
            SinkResult::Held
        ));
        assert_eq!(h.clock.maps(), 1, "the abandoned remainder never re-maps");

        h.clock.set(16_666_667);
        // Next AU reports loss; needs reanchor to feed (decoder has truncated input).
        assert!(matches!(
            h.stage.submit(&frame(2, Some((true, true, 0)), true, false)),
            SinkResult::Presented { .. }
        ));
        assert_eq!(h.clock.maps(), 2, "the new picture maps for itself");
        let stamps = h.distinct_stamps();
        assert_eq!(stamps.len(), 2);
        assert!(
            stamps[1] > stamps[0],
            "the new picture did not reuse the dead AU's stamp"
        );
    }

    /// Held frames don't map. Reanchor resets timeline, doesn't continue pre-hold mapping.
    #[test]
    fn a_held_stretch_maps_nothing_and_the_reanchor_resets_the_timeline() {
        let h = &mut harness();
        assert!(matches!(
            h.stage.submit(&frame(1, None, true, false)),
            SinkResult::Presented { .. }
        ));
        assert_eq!(h.clock.maps(), 1);
        assert!(matches!(
            h.stage.submit(&frame(2, None, false, true)),
            SinkResult::NeedKeyframe
        ));
        for index in 3..20 {
            h.clock.set(u64::from(index) * 16_666_667);
            assert!(!matches!(
                h.stage.submit(&frame(index, None, false, false)),
                SinkResult::Presented { .. }
            ));
        }
        assert_eq!(h.clock.maps(), 1, "nothing mapped while frozen");

        let before = h.stage.pacing_health().reanchors;
        h.clock.set(20 * 16_666_667);
        assert!(matches!(
            h.stage.submit(&frame(20, None, true, false)),
            SinkResult::Presented { .. }
        ));
        assert_eq!(h.clock.maps(), 2);
        assert!(
            h.stage.pacing_health().reanchors > before,
            "the resume re-anchored the mapping"
        );
    }

    /// Timestamps are monotonic across holds. No flush means pre-hold timestamps are live.
    #[test]
    fn stamps_are_monotonic_across_a_hold() {
        let h = &mut harness();
        for index in 1..6u32 {
            h.clock.set(u64::from(index) * 16_666_667);
            let _ = h.stage.submit(&frame(index, None, index == 1, false));
        }
        assert!(matches!(
            h.stage.submit(&frame(6, None, false, true)),
            SinkResult::NeedKeyframe
        ));
        // The host's capture clock jumps backwards on the re-anchor; the player clock does not.
        h.clock.set(6 * 16_666_667);
        let _ = h.stage.submit(&WireFrame {
            data: &[0u8; 4],
            pts_ns: 0,
            index: 6,
            part: None,
            reanchor: true,
            loss: false,
        });
        let stamps = h.distinct_stamps();
        assert!(
            stamps.windows(2).all(|w| w[1] >= w[0]),
            "stamps went backwards: {stamps:?}"
        );
    }

    /// Smoothness requires plane. With one, budget manifests as real lead. Without, falls to Latency.
    #[test]
    fn smoothness_needs_a_plane_and_then_adds_its_budget() {
        let budget = PresentPriority::Smooth { buffer: 2 };
        let bare = harness_with(FakeSink::default(), budget);
        let paced = harness_with(
            FakeSink {
                plane: Some(Arc::new(FakePlane)),
                ..FakeSink::default()
            },
            budget,
        );
        let cushion = |mut h: Harness| {
            h.stage.submit(&frame(1, None, true, false));
            h.stage.pacing_health().cushion_ns
        };
        let (bare, paced) = (cushion(bare), cushion(paced));
        assert!(
            paced > bare,
            "the plane-backed stage reserved the budget: {paced} vs {bare}"
        );
        assert!(paced - bare >= 2 * 16_666_666, "two stream periods of extra lead");
    }
}
