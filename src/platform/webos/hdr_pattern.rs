//! Plays synthetic HDR calibration patterns on the NDL video plane.
//!
//! Deliberately the same path a stream takes — NDL `DirectMedia`, an HEVC access unit at a time,
//! the same colour metadata call — because the point of the exercise is to measure what a stream
//! will actually look like. A pattern drawn on the graphics plane instead would be measuring the
//! compositor.
//!
//! Teardown mirrors a session's exactly: join the feed, drop the player, then `ndl::quit()`.
//! Anything else leaves NDL warm, and a warm NDL is what breaks the *next* load (see
//! `docs/NOTES.md`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use punktfunk_core::quic::{ColorInfo, HdrMeta};

use crate::core::media::VideoSink;
use crate::platform::webos::device::{self, NdlGeneration};
use crate::platform::webos::ndl::{self, NdlCodec, NdlVideo};
use crate::services::hevc::{self, Patch};
use crate::services::join::join_with_timeout;

/// Feed cadence. A still picture needs no more than this, and every frame is a whole PCM-coded
/// IDR — see `services::hevc` on why the coded picture is small.
const FRAME_TIME: Duration = Duration::from_millis(100);
/// Frames the decoder may be holding before the feed stops offering more — see the queue check
/// in [`feed`]. Two is enough for NDL to pace against and shallow enough that a frame the panel
/// never presents cannot block the thread.
const MAX_QUEUE_FRAMES: i32 = 2;

/// How long the plane may go without presenting before the screen stops waiting for it.
///
/// Must clear the worst-case load, or it fires mid-load and tells the user the plane was rejected
/// while it is still coming up. A plane the pipeline refuses outright costs the prime budget, an
/// unload, the retry settle (up to 800 ms) and then the video-only load's own 2 s wait — about
/// 3.3 s — so the old 3 s could never clear it. Erring long is the cheap direction: late costs a
/// slow screen, early costs a wrong answer.
const PRESENT_DEADLINE: Duration = Duration::from_secs(5);
/// How long the feed thread is given to notice `stop` and return — the same ceiling, for the same
/// reason, as a stream's teardown.
const JOIN_TIMEOUT: Duration = crate::services::join::SHUTDOWN_JOIN_TIMEOUT;

pub struct Pattern {
    pub background: u16,
    pub patches: Vec<Patch>,
}

/// What the feed thread is asked to show next. Encoding happens over there, not on the caller's
/// thread: a 1080p PCM frame is milliseconds of packing, and a slider held down would otherwise
/// stutter the menu it lives on.
type Command = (HdrMeta, Pattern);

pub struct Playback {
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Command>,
    handle: Option<JoinHandle<()>>,
    started: Instant,
}

impl Playback {
    pub fn start(meta: HdrMeta, pattern: Pattern) -> Result<Self> {
        if device::ndl_generation() != NdlGeneration::V2 {
            bail!("HDR calibration needs NDL DirectMedia v2");
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let worker_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("hdr-pattern".into())
            .spawn(move || {
                if let Err(e) = run(&worker_stop, &rx, meta, &pattern) {
                    tracing::error!("HDR calibration pattern feed: {e:#}");
                }
            })
            .context("spawn HDR pattern thread")?;
        Ok(Self {
            stop,
            tx,
            handle: Some(handle),
            started: Instant::now(),
        })
    }

    /// Dropped silently if the feed has already gone — the caller's own `presenting`/`stalled`
    /// reporting surfaces that.
    pub fn show(&self, meta: HdrMeta, pattern: Pattern) {
        let _ = self.tx.send((meta, pattern));
    }

    /// Whether a frame has reached the decoder. Per-process only; only meaningful because nothing
    /// else can play while this is up.
    #[must_use]
    pub fn presenting(&self) -> bool {
        ndl::presenting()
    }

    /// Plane rejected the stream, or there is no plane. Screen reports it rather than sitting on black.
    #[must_use]
    pub fn stalled(&self) -> bool {
        !self.presenting() && self.started.elapsed() > PRESENT_DEADLINE
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let Some(handle) = self.handle.take() else { return };
        // A thread wedged inside FFI must not be raced by the unload, so a timed-out join poisons
        // NDL instead of quitting it — same contract, and the same helper, as a stream's teardown.
        if join_with_timeout(handle, JOIN_TIMEOUT, "hdr-pattern", ndl::poison) {
            ndl::quit();
        } else {
            tracing::warn!("HDR pattern feed did not stop in time — skipping NDL unload for this run");
        }
    }
}

fn run(stop: &Arc<AtomicBool>, rx: &mpsc::Receiver<Command>, meta: HdrMeta, pattern: &Pattern) -> Result<()> {
    let video = Arc::new(NdlVideo::load(
        &ndl::app_id(),
        hevc::WIDTH as i32,
        hevc::HEIGHT as i32,
        NdlCodec::H265,
        // The short budget: this screen never routes real audio, it only needs the plane fed so
        // NDL paces the pattern. An unconfirmed plane does that (see `NdlVideo::run_clock_plane`).
        Some(ndl::AUDIO_PRIME_BUDGET),
        2,
    )?);
    let color = ColorInfo {
        primaries: ColorInfo::CP_BT2020,
        transfer: ColorInfo::TRC_PQ,
        matrix: ColorInfo::MC_BT2020_NCL,
        full_range: 0,
    };
    video.set_color_info(Some(&meta), color)?;
    // NDL paces the video plane off a fed audio plane; without one it ignores presentation times
    // and the picture stalls. A silent clock plane is the whole fix — the same one a
    // software-audio stream runs.
    let clock = video
        .audio_plane()
        .map(|plane| ndl::spawn_clock_plane(plane, Arc::clone(stop), false))
        .transpose()
        .context("spawn HDR pattern clock plane")?;

    let result = feed(&video, stop, rx, meta, color, pattern);
    stop.store(true, Ordering::Relaxed);
    if let Some(clock) = clock {
        let _ = clock.join();
    }
    result
}

fn feed(
    video: &NdlVideo,
    stop: &AtomicBool,
    rx: &mpsc::Receiver<Command>,
    mut meta: HdrMeta,
    color: ColorInfo,
    pattern: &Pattern,
) -> Result<()> {
    // One encoder for the life of the feed: it keeps the parameter sets and every frame-sized
    // buffer, so a dragged slider re-fills them instead of allocating a frame's worth per step.
    let mut enc = hevc::Encoder::new();
    enc.encode(pattern.background, &pattern.patches);
    let started = Instant::now();
    let mut frame = 0_u64;
    let mut errors = 0_u32;
    while !stop.load(Ordering::Relaxed) {
        let due = started + FRAME_TIME.mul_f64(frame as f64);
        // Waits on the command channel rather than polling it: a new pattern is picked up the
        // moment it is sent, and the wait ends by itself when the next frame falls due. Capped at
        // one frame so `stop` is still noticed promptly.
        let wait = due.saturating_duration_since(Instant::now()).min(FRAME_TIME);
        let woken = match rx.recv_timeout(wait) {
            Ok(cmd) => Some(cmd),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            // The screen is gone; `stop` is already set, or about to be.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // Only the newest request matters: a dragged slider queues several, and encoding the ones
        // already superseded would put the feed further behind with every step.
        if let Some((latest_meta, latest)) = woken.into_iter().chain(rx.try_iter()).last() {
            enc.encode(latest.background, &latest.patches);
            if latest_meta != meta {
                meta = latest_meta;
                video.set_color_info(Some(&meta), color)?;
            }
        }
        if Instant::now() < due || stop.load(Ordering::Relaxed) {
            continue;
        }
        // **The queue is checked before every feed, never after.** `NDL_DirectVideoPlay` blocks
        // for as long as the decoder has no room, and there is no way to interrupt a thread
        // inside it: one blocked call outlives the join deadline in `Drop`, which then has to
        // poison NDL and refuse the next load (streaming included). A still picture has nothing
        // to gain from a deep queue, so the feed simply declines to hand over a frame the
        // decoder has not made room for.
        if video.render_buffer_length().is_some_and(|d| d >= MAX_QUEUE_FRAMES) {
            // Parked, not spun: `due` is already past, so an unguarded `continue` would re-query
            // the queue as fast as the FFI lock allows until the decoder drains — and NDL holds a
            // standing cushion, so that state is not brief.
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        match video.play(enc.frame(), frame * FRAME_TIME.as_nanos() as u64) {
            Ok(()) => frame += 1,
            // A full decoder queue is back-pressure, not a failure; the frame is simply re-offered.
            Err(e) if e.downcast_ref::<crate::core::media::NotReady>().is_some() => {
                std::thread::sleep(Duration::from_millis(5));
            }
            // A rejected frame is not worth abandoning the screen for — the sliders still work
            // and the next frame may well land. Logged once so the reason is on record without
            // filling the log at the feed's cadence.
            Err(e) => {
                errors += 1;
                if errors == 1 {
                    tracing::warn!("HDR pattern frame rejected (continuing): {e:#}");
                }
                std::thread::sleep(FRAME_TIME);
            }
        }
    }
    Ok(())
}
