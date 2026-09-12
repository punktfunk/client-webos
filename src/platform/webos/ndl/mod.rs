//! webOS NDL `DirectMedia` video. Video only; audio goes through SDL2
//! (`platform::webos::audio`).
//!
//! Two generations of the same C API, in the same device library, chosen by
//! `device::ndl_generation()` from the detected `sdkVersion`:
//!
//! - [`v2`] — webOS 5+. `NDL_DirectMediaLoad` + `NDL_DirectVideoPlay(buffer, size, pts)`, with
//!   a render-buffer query, a flush, and HDR mastering metadata. The path every currently
//!   working TV takes.
//! - [`v1`] — webOS 3.5-4.x. `NDL_DirectVideoOpen/SetCallback/SetArea/PlayWithCallback/Close`:
//!   H.264 only, SDR, and **no PTS input**.
//!
//! Neither generation has a decode-context handle — every call is a process singleton — so what
//! both share lives here: the load-state counters behind [`presenting`]/[`playing`], the
//! one-time `NDL_DirectMediaInit`/[`quit`] pair, and the [`poison`] gate.
//!
//! **Everything is `dlopen`'d, and must stay that way.** `libNDL_directmedia.so.1` exists on
//! every supported TV but its *symbol set* does not — webOS 3.5-4.x has none of the v2 entry
//! points. This binary links with `DT_BIND_NOW`/`DF_1_NOW`, so a `DT_NEEDED` reference to that
//! library makes the dynamic loader resolve `NDL_DirectMediaLoad` at exec time and **refuse to
//! start the process at all** on webOS 4 — before `main()`, with nothing logged. A
//! `#[link(name = "NDL_directmedia")]` block is therefore a launch-time regression on every
//! webOS 4 device, not a style choice; see `docs/NOTES.md`.
//!
//! `device::ndl_generation` decides which generation to *try*; [`ffi`]'s `dlsym` probe decides
//! whether it's there. A miss is a named error, never a fallback to the other generation — on
//! webOS 4 the v2 symbols are absent by construction, so falling back buys a doomed connect.
mod ffi;
pub mod v1;
mod v2;

use std::ffi::{c_char, c_int, c_longlong, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use super::device::{self, NdlGeneration};

pub use v2::{NdlVideo, OPUS_51_LAYOUT};

/// `NDL_VIDEO_TYPE` values this client can request (matches the codec the host's
/// `Welcome` resolved — see `punktfunk_core::quic::CODEC_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdlCodec {
    H264,
    H265,
}

impl NdlCodec {
    fn ndl_type(self) -> c_int {
        match self {
            Self::H264 => 1,
            Self::H265 => 2,
        }
    }

    pub fn from_wire(codec: u8) -> Option<Self> {
        match codec {
            punktfunk_core::quic::CODEC_H264 => Some(Self::H264),
            punktfunk_core::quic::CODEC_HEVC => Some(Self::H265),
            _ => None,
        }
    }
}

/// NDL load-state values reported through the v2 media-load callback.
const STATE_LOADCOMPLETED: c_int = 0x16;
const STATE_UNLOADCOMPLETED: c_int = 0x17;
const STATE_PLAYING: c_int = 0x1a;
/// The one state measured to kill a load for good: seen on a CX as `0x12` with `errorCode 600`,
/// after which every feed fails and NDL unloads itself (docs/NOTES.md § "A/V sync"). Only this
/// one latches [`FATAL`] — the enum is sparse (0x18/0x19 are unmapped) and a benign notification
/// treated as fatal would end a healthy session outright.
const STATE_ERROR: c_int = 0x12;

/// Bound, not a requirement: feeding an unloaded decoder is the first-frames-black cause,
/// but a model that never delivers the callback must still stream.
const LOAD_COMPLETE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// How long an audio-enabled load is primed while waiting for `LOADCOMPLETED` (see
/// `v2::NdlVideo::prime_audio`). Sized for a set that answers promptly — a CX confirms in ~40 ms
/// — because a set that answers at all answers fast, and one that does not is not waiting on
/// time. Issue #188: a 2025 QNED reports the callback only once a video FRAME has been fed, which
/// cannot happen inside this wait, so every ms past a prompt answer is black screen bought for
/// nothing. The plane is not judged by this wait; see [`AUDIO_PROVE_BUDGET`].
pub const AUDIO_PRIME_BUDGET: Duration = Duration::from_millis(500);

/// The budget a session spends when it intends to put its REAL audio on the plane, i.e. the user
/// asked for the offload route. Longer than [`AUDIO_PRIME_BUDGET`] on purpose: an unconfirmed
/// plane costs that session the route outright (`AudioPlane::accepts_stream`), and the route is
/// picked once, before a frame has been fed, so an answer arriving later cannot be used. Every
/// other session takes the short budget and loses nothing by it — the metronome rides an
/// unconfirmed plane happily, and that is what paces the picture.
pub const AUDIO_PROVE_BUDGET: Duration = LOAD_COMPLETE_TIMEOUT;

/// Grace for a rejected load's callbacks to land before the video-only retry arms. The callback
/// carries nothing identifying its load, so separating the two in TIME is the only way to stop a
/// stale `LOADCOMPLETED` satisfying the retry's wait — and feeding an unloaded decoder is what
/// turns a launch black.
const CALLBACK_SETTLE: Duration = Duration::from_millis(400);

const POLL: Duration = Duration::from_millis(2);

/// A process-global NDL event, counted rather than flagged: a late event still increments, so it
/// stays attributable to the load it came from — a sticky bool cannot tell "this load completed"
/// from "the previous one's callback arrived a moment too late". [`Self::arm`] stamps the count a
/// new load starts from; [`Self::fired`] then answers only about that load.
struct EventSeq {
    seq: AtomicU64,
    base: AtomicU64,
}

impl EventSeq {
    const fn new() -> Self {
        Self {
            seq: AtomicU64::new(0),
            base: AtomicU64::new(0),
        }
    }

    fn bump(&self) {
        self.seq.fetch_add(1, Ordering::SeqCst);
    }

    fn bump_first(&self) -> bool {
        if self.fired() {
            return false;
        }
        self.bump();
        true
    }

    fn arm(&self) {
        self.base.store(self.seq.load(Ordering::SeqCst), Ordering::SeqCst);
    }

    /// Acquire, not `SeqCst`: only needs to order this read against the callback's bump.
    fn fired(&self) -> bool {
        self.seq.load(Ordering::Acquire) > self.base.load(Ordering::Acquire)
    }

    fn count(&self) -> u64 {
        self.seq.load(Ordering::Acquire)
    }
}

static LOAD_COMPLETED: EventSeq = EventSeq::new();
static UNLOAD_COMPLETED: EventSeq = EventSeq::new();
/// NDL's own present-pipeline signal (docs/NDL-FRAMERATE-INVESTIGATION.md). Measured on a G5:
/// it lands during `load()`, BEFORE any frame is fed, so it says nothing about there being a
/// picture — kept for the log line only, never as a reveal gate (see [`FRAME_FED`]).
static PLAYING: EventSeq = EventSeq::new();
/// Bumped by the first feed NDL accepts for the armed load. This, not `PLAYING`, is what makes
/// uncovering the punch-through plane safe: before it there is provably nothing on the plane,
/// and a host that delivers its first frame seconds late (new-flow stall, startup capacity
/// probe) would otherwise show as seconds of black.
static FRAME_FED: EventSeq = EventSeq::new();
/// Set by [`STATE_ERROR`] — in the field, `0x12` with `errorCode 600`, which NDL follows by failing
/// every `play` until the pipeline unloads itself. Latched rather than pulsed: the load is gone,
/// and there is no later callback that takes it back. Cleared only by [`arm_load`], which is to
/// say by a NEW load.
static FATAL: EventSeq = EventSeq::new();
/// When the armed load's first frame was accepted — [`FRAME_FED`]'s clock, for the holds that
/// are measured from the picture rather than from the load.
static FIRST_FRAME_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// Records v2 load-state transitions so loads, feeds and the UI reveal can wait on them.
extern "C" fn on_load_state(state: c_int, num: c_longlong, detail: *const c_char) {
    let name = match state {
        STATE_LOADCOMPLETED => {
            LOAD_COMPLETED.bump();
            "LOADCOMPLETED"
        }
        STATE_UNLOADCOMPLETED => {
            UNLOAD_COMPLETED.bump();
            "UNLOADCOMPLETED"
        }
        STATE_PLAYING => {
            PLAYING.bump();
            "PLAYING"
        }
        // Never swallowed: an error state here is the only signal a load rejected async.
        _ => {
            // SAFETY: NDL passes a NUL-terminated string or null, valid for this call only.
            let detail = if detail.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(detail) }.to_string_lossy().into_owned()
            };
            if state == STATE_ERROR {
                // Feeding on produces nothing but a `play` error per frame — see [`fatal`]. Logged
                // once per load; NDL repeats the state while the pipeline tears itself down.
                if FATAL.bump_first() {
                    tracing::error!("NDL load state: fatal 0x{state:x} ({state}) num={num} {detail}");
                }
            } else {
                // Unmapped, and not the state that has ever been seen to kill a load. Logged so a
                // device trace can identify it, but NOT latched: ending a healthy session on a
                // notification we simply don't have a name for is the worse failure.
                tracing::warn!("NDL load state: unmapped 0x{state:x} ({state}) num={num} {detail}");
            }
            return;
        }
    };
    tracing::info!("NDL load state: {name} (0x{state:x})");
}

/// Takes the counters as the baseline for the load about to be issued. Call immediately before
/// every load, and after an unload so [`playing`] stops reporting a dead one.
fn arm_load() {
    *FIRST_FRAME_AT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    LOAD_COMPLETED.arm();
    PLAYING.arm();
    FRAME_FED.arm();
    FATAL.arm();
}

/// Whether the current load has reported a fatal state — see [`FATAL`]. The session reads this to
/// tell "NDL is gone" from "these frames failed", which are the same thing at the `play` call and
/// want opposite responses: the second is a re-anchor, the first cannot be recovered from without
/// a new load.
pub fn fatal() -> bool {
    FATAL.fired()
}

/// Diagnostics only — see [`PLAYING`].
pub fn playing() -> bool {
    PLAYING.fired()
}

/// Whether a frame of the current load has reached the decoder: the plane is safe to uncover.
/// Both NDL generations report through this, so `runtime`'s reveal gate is generation-blind.
pub fn presenting() -> bool {
    FRAME_FED.fired()
}

/// How long past its first accepted frame a load has a picture on the panel.
///
/// NDL reports no first-*present* event on either generation — v2 has load states only
/// (`PLAYING` lands before anything is fed) and v1's frame callback is a pipeline-alive echo.
/// What it does have is a standing present cushion: `DirectMedia` presents an access unit at
/// its PTS, a couple of frames behind the feed (see `docs/NOTES.md` on present lead). So the
/// first accepted frame is decode-queued, not visible, and anything crossfading to live video
/// on [`presenting`] alone crossfades into black. Measured from that first frame, so it holds
/// the *pipeline* only — waiting for the host's first delivery has its own budget
/// (`app::hero::FIRST_FRAME_WAIT`).
const FIRST_PICTURE_HOLD: Duration = Duration::from_millis(250);

/// Whether the panel should be *showing* the current load, not just holding its first frame —
/// what a crossfade from a still to live video has to wait for. See [`FIRST_PICTURE_HOLD`].
pub fn presented() -> bool {
    first_frame_at().is_some_and(|t| t.elapsed() >= FIRST_PICTURE_HOLD)
}

fn first_frame_at() -> Option<Instant> {
    *FIRST_FRAME_AT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn mark_frame_fed() -> bool {
    let first = FRAME_FED.bump_first();
    if first {
        *FIRST_FRAME_AT.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
    }
    first
}

fn mark_frame_fed_logged(backend: &str, since: Instant) -> bool {
    let first = mark_frame_fed();
    if first {
        tracing::info!("{backend} first frame fed {:?} after load", since.elapsed());
    }
    first
}

/// Every NDL entry point is a process singleton and none is documented as thread-safe, so one lock
/// serializes all of them. Poison-tolerant: a panic mid-FFI leaves no Rust state to corrupt, and
/// refusing the lock afterwards would only turn it into a dead video plane.
static FFI_LOCK: Mutex<()> = Mutex::new(());

fn lock_ffi() -> MutexGuard<'static, ()> {
    FFI_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sleeps in [`POLL`] steps until `done` or `limit` elapses; `true` if `done` won. Polled, not
/// blocked on: every wait here is for a callback on NDL's own thread.
fn poll_until(limit: Duration, done: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while !done() {
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(POLL);
    }
    true
}

fn unload_count() -> u64 {
    UNLOAD_COMPLETED.count()
}

/// Lets the rejected audio-enabled load's callbacks land before the video-only retry is armed:
/// waits for its `UNLOADCOMPLETED`, then a fixed settle for anything still in flight behind it.
/// `unloads_before` is [`unload_count`] from before the rejected load was attempted: the caller's
/// own teardown may have unloaded already, and a spent callback must not be waited out.
fn settle_before_retry(unloads_before: u64) {
    poll_until(CALLBACK_SETTLE, || UNLOAD_COMPLETED.count() != unloads_before);
    std::thread::sleep(CALLBACK_SETTLE);
}

/// Blocks up to [`LOAD_COMPLETE_TIMEOUT`] for the armed load's `LOADCOMPLETED`. Returns `false` on
/// timeout. The audio-enabled load has its own wait — see `v2::NdlVideo::prime_audio`.
fn wait_load_completed() -> bool {
    // Also exits on FATAL: it won't change by waiting, unlike pending callbacks.
    poll_until(LOAD_COMPLETE_TIMEOUT, || LOAD_COMPLETED.fired() || fatal());
    if LOAD_COMPLETED.fired() {
        return true;
    }
    if fatal() {
        tracing::warn!("NDL load reported a fatal state — not waiting out the rest of {LOAD_COMPLETE_TIMEOUT:?}");
    } else {
        tracing::warn!("NDL load: no LOADCOMPLETED within {LOAD_COMPLETE_TIMEOUT:?} — holding the first frames");
    }
    false
}

/// Count of video/audio pump threads leaked past `SHUTDOWN_JOIN_TIMEOUT` (see
/// `session::join`), not yet confirmed exited. A leaked thread may still be
/// inside an `NDL_Direct*` call and still holds a live decode session with its own
/// unsynchronized `ffi` mutex — a second load on top of that races it instead of starting
/// clean, reproducing as an undecodable stream rather than a clean failure.
///
/// No way to force this: an OS thread can't be safely cancelled mid-FFI-call, and racing the
/// unload against it is the exact hazard this guards against. So every `load()` refuses while
/// nonzero, and dropping the [`LeakGuard`] clears it in-process (no restart) once the leaked
/// thread actually returns — its handle's `Drop` has run the real unload by then.
static LEAKED_THREADS: AtomicUsize = AtomicUsize::new(0);

/// One leaked NDL-touching thread, for as long as this value lives (see [`LEAKED_THREADS`]).
///
/// A guard rather than a `poison`/`recovered` pair of calls: both ways of mispairing them are
/// silent — a missed decrement refuses streaming until restart, an extra one re-permits it while a
/// thread is still inside NDL. Ownership makes the pairing structural.
pub struct LeakGuard(());

impl Drop for LeakGuard {
    fn drop(&mut self) {
        if LEAKED_THREADS.fetch_sub(1, Ordering::SeqCst) == 1 {
            tracing::info!(
                "NDL recovered: the wedged decode thread finished and unloaded cleanly — streaming re-enabled"
            );
        }
    }
}

/// Caller must arrange the guard to drop when the thread actually finishes, however late.
/// Leaking it (`mem::forget`) means recovery is never signalled.
#[must_use = "NDL stays poisoned until this guard drops — hold it until the wedged thread returns"]
pub fn poison() -> LeakGuard {
    if LEAKED_THREADS.fetch_add(1, Ordering::SeqCst) == 0 {
        tracing::error!(
            "NDL poisoned: a decode thread is wedged past its join deadline — streaming \
             refused until it actually finishes (no safe way to force it sooner)"
        );
    }
    LeakGuard(())
}

/// Checked early by `session::connect` to avoid holding a host slot for a connect that can only fail.
pub fn ensure_not_poisoned() -> Result<()> {
    if LEAKED_THREADS.load(Ordering::SeqCst) > 0 {
        bail!("NDL is still tearing down a wedged decode thread from the previous session — try reconnecting shortly");
    }
    Ok(())
}

static INIT_DONE: AtomicBool = AtomicBool::new(false);

/// Calls `NDL_DirectMediaInit` once (process-global, idempotent-guarded). One symbol, two
/// prototypes: `api2` picks the app-id-only form v2 declares, against v1's app id plus
/// resource-released callback (always NULL here). See [`ffi::Common`].
fn ensure_init(app_id: &str, api2: bool) -> Result<()> {
    if INIT_DONE.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let fns = ffi::common()?;
    let c_app_id = CString::new(app_id).unwrap_or_default();
    if let Err(e) = fns.init(&c_app_id, api2) {
        INIT_DONE.store(false, Ordering::SeqCst);
        return Err(e);
    }
    Ok(())
}

/// Logs whether the TV's Sound Out passes multi-channel PCM right now — the first line to read
/// when a surround session sounds like stereo. Called by `session::connect` for sessions wider
/// than stereo.
///
/// Diagnostic only: the session asks for the user's layout regardless and webOS folds what its
/// output can't pass. The answer describes NDL's own PCM path, not the SDL device the software
/// route plays through, and reads `Supported` only with Sound Out on Pass Through.
pub fn log_audio_output() {
    if device::ndl_generation() != NdlGeneration::V2 {
        return;
    }
    // The query answers nothing before `NDL_DirectMediaInit`, and this runs a moment before the
    // load would have called it anyway — process-global and idempotent, so it is the same init.
    if let Err(e) = ensure_init(&app_id(), true) {
        tracing::warn!("NDL init for the audio-output query: {e:#}");
        return;
    }
    tracing::info!("NDL audio output: {:?}", ffi::multichannel_pcm_status());
}

/// Spawns the metronome that keeps the audio plane fed.
///
/// NDL paces the *picture* off a fed audio plane — without one it ignores presentation times and
/// the picture stalls (docs/NOTES.md § "NDL's audio plane"). Both callers spawn the same thread
/// for the same reason: a stream whose audio decodes in software, and the HDR calibration feed.
pub fn spawn_clock_plane(
    plane: std::sync::Arc<dyn crate::core::media::AudioPlane>,
    stop: std::sync::Arc<AtomicBool>,
    yields_to_real: bool,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("punktfunk-webos-clock".into())
        .spawn(move || {
            plane.run_keepalive(&stop, yields_to_real);
        })
}

/// Overridable for dev builds. NDL keys its session on the caller's app id, so a mismatch fails
/// the load.
pub fn app_id() -> String {
    std::env::var("APPID").unwrap_or_else(|_| "io.unom.punktfunk.client-webos".into())
}

/// Process-wide NDL teardown — call once at exit, after every decode session has dropped.
pub fn quit() {
    if !INIT_DONE.swap(false, Ordering::SeqCst) {
        return;
    }
    // Same symbol on both generations, so no branch needed — and the table must have resolved
    // for `INIT_DONE` to have been set.
    if let Ok(fns) = ffi::common() {
        fns.quit();
    }
}
