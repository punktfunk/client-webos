//! The threads that drain the transport into the pipeline: access units into the video stage,
//! packets into the audio stage. Everything wire-shaped lives here and nothing else — what a
//! delivery MEANS is the stages' business.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::{AudioPacket, NativeClient};
use punktfunk_core::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR};
use punktfunk_core::reanchor::DROP_CREDIT_WINDOW;
use punktfunk_core::PunktfunkError;

use crate::services::join::{join_with_timeout, SHUTDOWN_JOIN_TIMEOUT};
use crate::session::audio::AudioStage;
use crate::session::stage::{SinkResult, VideoStage, WireFrame};
use crate::session::timeline::{CadenceTrace, Deltas};
use crate::session::StreamStats;

/// Longest a `next_frame` call parks before the loop re-checks `stop`.
const FRAME_WAIT: Duration = Duration::from_millis(500);
/// Pump liveness check cadence; refreshes overlay backlog and logs "nothing arriving".
const HEARTBEAT: Duration = Duration::from_secs(2);
/// Cadence for verbose heartbeat detail logging (trend line).
const VIDEO_LOG_INTERVAL: Duration = Duration::from_secs(15);

/// Now on the clock `Frame::pts_ns` is stamped against — Unix-epoch ns, `CLOCK_REALTIME` — which
/// is what core's HUD differences a capture time with. A monotonic stamp would read as nonsense.
fn realtime_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// A stamp that fires once per `interval` and re-arms itself.
struct Tick {
    interval: Duration,
    last: Instant,
}

impl Tick {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Instant::now(),
        }
    }

    /// Elapsed span when due; allows reporting actual window coverage vs. nominal interval.
    fn due(&mut self) -> Option<Duration> {
        let elapsed = self.last.elapsed();
        if elapsed < self.interval {
            return None;
        }
        self.last = Instant::now();
        Some(elapsed)
    }
}

/// Drives the video thread: transport → [`VideoStage`], plus the counters and the loss/HDR
/// side-channels that ride the same loop.
struct VideoPump {
    client: Arc<NativeClient>,
    stage: VideoStage,
    stats: Arc<StreamStats>,
    /// Core's shared HUD, held rather than fetched per frame. Its endpoint on this client is the
    /// submit to NDL: nothing past that call — decode, present, panel — is observable from the
    /// app, so the capture-to-decoded figure is a lower bound on glass latency, never an estimate
    /// of it.
    hud: Arc<punktfunk_core::hud::Stats>,
    /// Whether to drain host HDR metadata (false for SDR or non-HEVC).
    is_hdr: bool,
    /// True when real audio rides the NDL plane; false when plane is silent metronome only.
    audio_rides_plane: bool,
    /// Core's cumulative drop count as of the last frame, to edge-detect new drops.
    last_dropped_seen: u64,
    /// Frame-index gaps pre-cover the reassembler's delayed drop accounting.
    drop_credit: u64,
    drop_credit_expiry: Option<Instant>,
    heartbeat: Tick,
    video_log: Tick,
    /// Session totals as of the last heartbeat, so the line can report this window's change.
    last_dropped: u64,
    last_holds: u64,
    last_held: Duration,
}

impl VideoPump {
    fn new(
        client: Arc<NativeClient>,
        stage: VideoStage,
        stats: Arc<StreamStats>,
        is_hdr: bool,
        audio_rides_plane: bool,
    ) -> Self {
        let last_dropped_seen = client.frames_dropped();
        // `StreamStats` derives `Default`, and 0 is a legitimate slack reading — so the sentinel
        // has to be written before the first heartbeat, or the overlay shows a fabricated
        // `slack +0.0 ms` for the two seconds it takes one to arrive.
        stats.pacing_min_slack_us.store(i32::MIN, Ordering::Relaxed);
        let hud = client.hud_shared();
        Self {
            client,
            stage,
            stats,
            hud,
            is_hdr,
            audio_rides_plane,
            last_dropped_seen,
            drop_credit: 0,
            drop_credit_expiry: None,
            heartbeat: Tick::new(HEARTBEAT),
            video_log: Tick::new(VIDEO_LOG_INTERVAL),
            last_dropped: 0,
            last_holds: 0,
            last_held: Duration::ZERO,
        }
    }

    fn run(&mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Relaxed) {
            match self.client.next_frame(FRAME_WAIT) {
                Ok(frame) => self.on_frame(&frame),
                // The SAME heartbeat as the frame path, not a second consumer of the tick: this
                // arm used to swallow the fire and print only the idle line, so an idle stretch
                // published nothing and left the cadence window accumulating across it — the next
                // window then covered far more than the interval it claimed.
                Err(PunktfunkError::NoFrame) => self.heartbeat(true),
                // A teardown the user asked for reaches both pumps as `Closed`, so it is not an
                // error in either — the audio pump already logged it at INFO.
                Err(PunktfunkError::Closed) => {
                    tracing::info!("video pump ending: session closed");
                    break;
                }
                Err(e) => {
                    tracing::error!("video pump: {e:#}");
                    break;
                }
            }
            self.forward_hdr_meta();
            // The connector matches each 0xCF timing to its frame for the overlay. Drained here
            // so the bounded plane never fills.
            while self.client.next_host_timing(Duration::ZERO).is_ok() {}
        }
    }

    /// Pictures the decoder took this session: the heartbeat log's figure.
    fn frames(&self) -> u64 {
        self.stage.frames()
    }

    fn on_frame(&mut self, frame: &punktfunk_core::session::Frame) {
        self.heartbeat(false);

        // Everything wire-shaped, and nothing else: whether this delivery is decodable at all,
        // and how one AU's pieces fit together, is the stage's bookkeeping.
        let wire = WireFrame {
            data: &frame.data,
            pts_ns: frame.pts_ns,
            index: frame.frame_index,
            part: frame.part,
            reanchor: frame.flags & u32::from(FLAG_SOF) != 0 || frame.flags & USER_FLAG_RECOVERY_ANCHOR != 0,
            loss: self.note_loss(frame),
        };
        // Diagnostic only — see `VideoStage::sample_backlog`. Nothing steers on the reading.
        self.stage.sample_backlog();
        match self.stage.submit(&wire) {
            SinkResult::Presented { decode_us } => {
                if let Some(us) = decode_us {
                    self.client.report_decode_us(us);
                }
                // Once per picture: a slice-progressive piece is not a frame.
                if frame.part.is_none_or(|part| part.last) {
                    self.hud.note_decoded(frame.pts_ns, realtime_ns());
                }
            }
            SinkResult::Held => {}
            SinkResult::NeedKeyframe => {
                if let Err(e) = self.client.request_keyframe() {
                    tracing::warn!("request_keyframe: {e:#}");
                }
            }
            // Nothing above this loop can revive the decoder — the load is gone and the plane
            // threads have exited with it — so the session ends and the runtime returns to the
            // menu, where the next launch builds a fresh pipeline.
            SinkResult::Dead => {
                // Once, not per frame: the stream loop needs a poll or two to notice, and the
                // stage answers `Dead` to every delivery in the meantime.
                if !self.stats.decoder_dead.swap(true, Ordering::Relaxed) {
                    tracing::error!("decoder failed for good — ending the session");
                }
            }
        }
    }

    /// Close the measurement window: take-and-re-arm all figures and difference all session counters
    /// in one place. Called once per heartbeat before listener check. Dropping the return resets accumulators.
    fn close_window(&mut self) -> WindowFigures {
        let dropped = self.client.frames_dropped();
        let (holds, held) = self.stage.hold_totals();
        let figures = WindowFigures {
            dropped: dropped.saturating_sub(self.last_dropped),
            holds: holds.saturating_sub(self.last_holds),
            held: held.saturating_sub(self.last_held),
            // A minimum rather than a mean, so one bad frame stays visible.
            min_slack_us: self.stage.take_pacing_slack_us(),
            cadence: self.stage.take_cadence_trace(),
        };
        self.last_dropped = dropped;
        self.last_holds = holds;
        self.last_held = held;
        figures
    }

    /// Refresh overlay backlog; log pump state on slower cadence. Close the window regardless,
    /// so the next one starts clean. Backlog is the sampler's last reading, not a fresh NDL query.
    fn heartbeat(&mut self, idle: bool) {
        let Some(window) = self.heartbeat.due() else {
            return;
        };
        if idle {
            // This arm says "nothing is arriving at all", which is a different fault from
            // "arriving but not presenting".
            tracing::info!("video: {} frames (idle)", self.frames());
        }
        // Latch for feed path (field read, not atomic per AU). Takes effect within one heartbeat.
        let overlay = self.stats.wants_diagnostics();
        // DEBUG level runs on real deploys; latch ensures `timed`-gated figures populate.
        let tracing_stats = tracing::enabled!(tracing::Level::DEBUG);
        self.stage.set_diagnostics(overlay || tracing_stats);
        // ⚠ Pacing lines print EVERY heartbeat (not on `video_log`'s slower tick). Take-and-re-arm
        // figures must happen here; slower cadence would silently skip windows. Heartbeat is the window.
        let log_due = self.video_log.due().is_some() && tracing::enabled!(tracing::Level::TRACE);
        // Close window before early return so unread heartbeats still reset figures.
        let window_figures = self.close_window();
        if !overlay && !tracing_stats {
            return;
        }
        let WindowFigures {
            dropped: dropped_window,
            holds: holds_window,
            held: held_window,
            min_slack_us,
            cadence,
        } = window_figures;
        let backlog = self.stage.backlog_depth();
        let pacing = self.stage.pacing_health();
        let plane_lead = self.stage.audio_plane_lead_ms();
        // Difference here: both halves in hand, one clock apart by construction.
        let av_offset = self
            .audio_rides_plane
            .then(|| plane_lead.map(|lead| lead - pacing.cushion_ns / 1_000_000))
            .flatten();
        self.stats
            .render_backlog
            .store(backlog.unwrap_or(-1), Ordering::Relaxed);
        self.stats.pacing_jitter_us.store(
            u32::try_from(pacing.jitter_ns.max(0) / 1_000).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        self.stats.pacing_late.store(pacing.late_stamps, Ordering::Relaxed);
        self.stats.pacing_cushion_us.store(
            u32::try_from(pacing.cushion_ns.max(0) / 1_000).unwrap_or(u32::MAX),
            Ordering::Relaxed,
        );
        self.stats
            .pacing_min_slack_us
            .store(min_slack_us.unwrap_or(i32::MIN), Ordering::Relaxed);
        let publish = |cell: &AtomicI32, ms: Option<i64>| {
            if let Some(ms) = ms {
                cell.store(i32::try_from(ms).unwrap_or(i32::MAX), Ordering::Relaxed);
            }
        };
        publish(&self.stats.audio_plane_lead_ms, plane_lead);
        publish(&self.stats.av_offset_ms, av_offset);
        // ⚠ **These are the OVERLAY's figures, and the overlay is where they belong** — everything
        // here is live on the stats overlay. Two lines are at DEBUG anyway, because the pacing
        // baseline has to be capturable off a normal deploy and is read as one window.
        // The per-frame video dump, which is
        // what actually buries the events worth reading, stays at TRACE.
        if tracing_stats {
            // Neither counter measures on-glass cadence; final submission includes AU tail and FFI waits.
            tracing::debug!(
                "pacing[{:.1}s]: late_stamp={}/{} late_submit={}/{} au_span={:.1}ms now(jitter={:.1}ms cushion={:.1}ms) reanchors={} av={} min_slack={} stamp_slack={}",
                window.as_secs_f32(),
                cadence.late_stamps,
                cadence.mapped,
                cadence.late_submissions,
                cadence.submissions,
                // Paired per picture: mapping to completed submission, the window's worst.
                f64::from(cadence.max_au_span_us) / 1000.0,
                pacing.jitter_ns as f64 / 1e6,
                pacing.cushion_ns as f64 / 1e6,
                pacing.reanchors,
                av_offset.map_or_else(|| "n/a".to_string(), |ms| format!("{ms}ms")),
                // Persistently negative here, while `jitter` stays healthy, is the signature of a
                // large AU finishing against a deadline its first piece set.
                min_slack_us.map_or_else(|| "n/a".to_string(), |us| format!("{:.1}ms", f64::from(us) / 1000.0)),
                // How deep the arrival tail ran past the cushion — the figure that sizes one.
                cadence
                    .min_stamp_slack_us
                    .map_or_else(|| "n/a".to_string(), |us| format!("{:.1}ms", f64::from(us) / 1000.0)),
            );
            // Plan §4 step 1's baseline, in one line: the cadence the mapping was handed, what it
            // produced, and what NDL received. `src` irregular with `out` matching it is the source;
            // `out` irregular where `src` is clean is ours. Events are apart from the deltas because
            // a gap or a repeat is not a short frame.
            tracing::debug!(
                "cadence[{:.1}s]: src={} due={} assigned={} repeats={} regressions={} gaps={}",
                window.as_secs_f32(),
                fmt_deltas(&cadence.source),
                fmt_deltas(&cadence.due),
                fmt_deltas(&cadence.assigned),
                cadence.repeats,
                cadence.regressions,
                cadence.gaps,
            );
            // On the SAME 2s window as the cadence above, because that is how a stutter report is
            // read: a plane lead sagging towards zero is what NDL stops pacing the picture on
            // (docs/NOTES.md § "NDL's audio plane"), and a hold or a backlog says the gap in the
            // cadence was ours rather than the source's.
            // `holds`/`held`/`dropped` cover the window; `backlog` and `plane_lead` are SNAPSHOTS
            // read at this instant and say nothing about what happened between two of them — a
            // plane lead that dipped and recovered is invisible here. Labelled so, because the
            // difference is exactly what an earlier read of these lines got wrong.
            tracing::debug!(
                "feed[{:.1}s]: holds={} held={:.0}ms dropped={} now(backlog={} plane_lead={})",
                window.as_secs_f32(),
                holds_window,
                held_window.as_secs_f32() * 1000.0,
                dropped_window,
                backlog.map_or_else(|| "n/a".to_string(), |b| b.to_string()),
                plane_lead.map_or_else(|| "n/a".to_string(), |ms| format!("{ms}ms")),
            );
        }
        if log_due {
            tracing::trace!(
                "video: {} frames, parts={}",
                self.frames(),
                // Against `frames`: 0 means slice-progressive delivery never fired on this mode
                // (core emits early parts only for an AU spanning more than one FEC block), so the
                // whole lever is inert here and its copy cost is not being paid either.
                self.stage.parts_fed(),
            );
        }
    }

    /// Whether loss reaches this frame — a sequence gap, or a frame the transport gave up on.
    fn note_loss(&mut self, frame: &punktfunk_core::session::Frame) -> bool {
        // From core v0.28 this returns the gap WIDTH (0 = contiguous) where it used to return a
        // bare "was there a gap" bool; `> 0` is the same predicate. Keep the width for the log
        // line — how many frames the hole swallowed is the number worth having when reading a
        // freeze report, not merely that one existed.
        // Slice-progressive pieces repeat their AU index. Observe it once, on the first piece.
        let au_first = frame.part.is_none_or(|part| part.first);
        let gap_width = if au_first {
            self.client.note_frame_index(frame.frame_index)
        } else {
            0
        };
        let dropped_now = self.client.frames_dropped();
        let dropped_delta = dropped_now.saturating_sub(self.last_dropped_seen);
        self.last_dropped_seen = dropped_now;
        // The clock is read only when a credit window is live or a gap just opened — both are loss
        // events. On the ordinary path this is the branch, not a `clock_gettime` per frame.
        if self.drop_credit_expiry.is_some() || gap_width > 0 {
            let now = Instant::now();
            if self.drop_credit_expiry.is_some_and(|expiry| now >= expiry) {
                self.drop_credit = 0;
                self.drop_credit_expiry = None;
            }
            if gap_width > 0 {
                self.drop_credit = self.drop_credit.saturating_add(u64::from(gap_width));
                self.drop_credit_expiry = Some(now + DROP_CREDIT_WINDOW);
            }
        }
        let credited = dropped_delta.min(self.drop_credit);
        self.drop_credit -= credited;
        if self.drop_credit == 0 {
            self.drop_credit_expiry = None;
        }
        let dropped = dropped_delta > credited;
        let lost = gap_width > 0 || dropped;
        if lost && !self.stage.holding() {
            // Logged with the sink's freeze report; gaps and drops point at different faults.
            tracing::warn!("loss: gap={gap_width} dropped={dropped} (frame {})", frame.frame_index);
        }
        lost
    }

    /// Hands the decoder any per-content HDR mastering metadata the host has sent.
    fn forward_hdr_meta(&mut self) {
        if !self.is_hdr {
            return;
        }
        // Collapse startup/keyframe repeats to the newest value. Applying an older queued value
        // first can delay a genuine mastering change by several frames.
        let mut latest = None;
        while let Ok(meta) = self.client.next_hdr_meta(Duration::ZERO) {
            latest = Some(meta);
        }
        let Some(meta) = latest else {
            return;
        };
        tracing::info!(
            "HDR metadata received: primaries={:?} white={:?} max_dml={} min_dml={} max_cll={} max_fall={}",
            meta.display_primaries,
            meta.white_point,
            meta.max_display_mastering_luminance,
            meta.min_display_mastering_luminance,
            meta.max_cll,
            meta.max_fall,
        );
        if let Err(e) = self.stage.set_color_info(Some(&meta), self.client.color) {
            tracing::warn!("NDL set_color_info: {e:#}");
        }
    }
}

/// The video thread's body: pump until `stop`. Owns all `Arc`s, keeping client and stats alive.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn video_pump(
    client: Arc<NativeClient>,
    stage: VideoStage,
    stop: Arc<AtomicBool>,
    stats: Arc<StreamStats>,
    is_hdr: bool,
    audio_rides_plane: bool,
) {
    VideoPump::new(client, stage, stats, is_hdr, audio_rides_plane).run(&stop);
}

/// How long an audio drain parks on an empty plane before re-checking `stop`.
const AUDIO_WAIT: Duration = Duration::from_millis(100);

/// Shared body of both audio threads: pull packets, hand each to `play`, exit on `stop` or closed plane.
///
/// Dedicated thread per core's contract (packets arrive every 5 ms); avoids stalls from
/// video pump blocking or main-loop rasterizer contention. Pull methods are one-thread-per-plane safe.
fn audio_drain(client: &NativeClient, stop: &AtomicBool, what: &str, mut play: impl FnMut(&AudioPacket)) {
    while !stop.load(Ordering::Relaxed) {
        match client.next_audio(AUDIO_WAIT) {
            Ok(packet) => play(&packet),
            Err(PunktfunkError::NoFrame) => {}
            Err(e) => {
                tracing::info!("{what} ending: {e:#}");
                break;
            }
        }
    }
}

/// The one audio pump: every route, every format.
///
/// Which sink it feeds is the route (`core::model::AudioRoutePref`), and what the sink takes is
/// the sink's own business ([`AudioStage`]) — this loop is blind to both.
///
/// Teardown safety on the plane routes: the stage holds an `Arc` of the plane, which is the same
/// handle as the video load, so the process-global NDL unload in `NdlVideo::drop` cannot run until
/// this thread has exited — a feed can never race the unload, whichever thread
/// `Connected::shutdown` happens to join first.
pub(super) fn audio_pump(client: &NativeClient, stage: &mut AudioStage, stop: &AtomicBool) {
    let what = stage.sink_name();
    let mut packets: u32 = 0;
    audio_drain(client, stop, what, |packet| {
        if let Err(e) = stage.play(packet.seq, packet.pts_ns, &packet.data) {
            tracing::warn!("audio error (seq {}): {e:#}", packet.seq);
            return;
        }
        packets = packets.wrapping_add(1);
        // ~15s, matching the video heartbeat (packets are 5ms each).
        if packets % 3_000 == 0 {
            tracing::debug!(
                "audio: {what}, depth={}, peak={:.4}",
                stage
                    .depth_ms()
                    .map_or_else(|| "n/a".to_string(), |ms| format!("{ms}ms")),
                stage.peak().unwrap_or(0.0),
            );
        }
    });
}

/// Spawns the audio thread for a session whose sink lives outside `connect` (SDL device).
pub fn spawn_audio_feed(
    client: Arc<NativeClient>,
    mut stage: AudioStage,
    stop: Arc<AtomicBool>,
) -> Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("punktfunk-webos-audio".into())
        .spawn(move || audio_pump(&client, &mut stage, &stop))
        .context("spawn audio thread")
}

/// Joins audio thread with standard teardown timeout. SDL route only; no `ndl::poison()` needed.
pub fn join_audio_feed(handle: std::thread::JoinHandle<()>) -> bool {
    join_with_timeout(handle, SHUTDOWN_JOIN_TIMEOUT, "audio-feed", || ())
}

/// One cadence series for the heartbeat: how many intervals, which way they ran, how irregular they
/// were, and the extremes. `n=0` means the series never advanced in this window.
fn fmt_deltas(d: &Deltas) -> String {
    if d.n == 0 {
        return "n=0".to_string();
    }
    format!(
        "n={} mean{:+.2} mad{:.2} [{:.2},{:.2}]ms",
        d.n,
        d.mean_err_ns() as f64 / 1e6,
        d.mad_ns() as f64 / 1e6,
        d.min_ns as f64 / 1e6,
        d.max_ns as f64 / 1e6,
    )
}

/// Everything on the heartbeat's lines that belongs to ONE window rather than to the session —
/// see [`VideoPump::close_window`].
struct WindowFigures {
    dropped: u64,
    holds: u64,
    held: Duration,
    min_slack_us: Option<i32>,
    cadence: CadenceTrace,
}
