use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::config::{CompositorPref, Mode};

use crate::core::model::ConnectTarget;
use crate::core::settings::TvSettings;
use crate::platform::webos::cursor;
use crate::platform::webos::gamepad;
use crate::platform::webos::keyboard;
use crate::platform::webos::mouse;
use crate::services::store;
use crate::session;

struct PendingConnect {
    handle: Option<std::thread::JoinHandle<Result<session::Connected>>>,
    attempt: std::sync::Arc<session::ConnectAttempt>,
}

impl PendingConnect {
    fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(std::thread::JoinHandle::is_finished)
    }

    fn join(mut self) -> std::thread::Result<Result<session::Connected>> {
        self.handle.take().expect("pending connect handle").join()
    }
}

impl Drop for PendingConnect {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else { return };
        let guard = self.attempt.cancel();
        // Hold the load gate before returning to a menu that can launch another session.
        std::thread::spawn(move || {
            if let Ok(Ok(connected)) = handle.join() {
                connected.disconnect_quit();
                connected.shutdown_and_quit();
            }
            drop(guard);
        });
    }
}

/// A launch handed from the menu to the streaming loop: the finished connect thread and the
/// settings it was started with.
struct ConnectOutcome {
    handle: PendingConnect,
    /// What was dialled, kept so a lost link can be dialled again (`stream`'s reconnect).
    target: ConnectTarget,
    settings: store::Settings,
    /// Whether the user's pick was `Automatic` — `settings.gamepad_type()` has already been
    /// resolved against the attached pad, so this is the only thing left that says a pad
    /// hotplugged mid-stream should re-decide the kind rather than keep the session default.
    gamepad_auto: bool,
    /// What to do to this host when the app exits, captured in the menu because a Quit out of
    /// the stream never returns there.
    ///
    /// Carried unfired all the way to `run_inner`'s single exit: the host refuses a cert-lane
    /// power action while a session is live, so this may only run once every teardown path has
    /// been through. Firing it where it is *decided* rather than where that is guaranteed is
    /// what the one exit site exists to prevent.
    exit_plan: Option<crate::services::power::ExitPlan>,
}

/// Resolves a `GamepadType::Auto` preference against the attached controller, for this
/// session only.
///
/// Session-only on purpose: the returned `Settings` drives the handshake and the stream
/// loop, while the stored document keeps saying `Automatic`. Resolving into the stored value instead would turn
/// a preference that means "match my pad" into a fixed pad kind the next time a different
/// controller was plugged in.
fn resolve_gamepad_type(mut settings: store::Settings, game_controller: &sdl3::GamepadSubsystem) -> store::Settings {
    if settings.gamepad_type() != store::GamepadType::Auto {
        return settings;
    }
    if let Some(detected) = gamepad::detect_type(game_controller) {
        tracing::info!("controller Automatic → {detected:?} (mirroring the attached pad)");
        settings.set_gamepad_type(detected);
    }
    settings
}

/// The mode to dial with: the document's explicit pick, or the panel where it says "Native"
/// (`0`) or "Match window" — the two the desktop clients resolve to the monitor, and which the
/// host refuses as a 0×0 request if passed through.
fn stream_mode(settings: &store::Settings, native: Mode) -> Mode {
    let explicit = !settings.match_window && settings.width != 0 && settings.height != 0;
    Mode {
        width: if explicit { settings.width } else { native.width },
        height: if explicit { settings.height } else { native.height },
        refresh_hz: if settings.refresh_hz == 0 {
            native.refresh_hz
        } else {
            settings.refresh_hz
        },
    }
}

/// Start the connect on its own thread. Caller joins after animation (or immediately).
fn spawn_connect(
    identity: (String, String),
    target: ConnectTarget,
    settings: store::Settings,
) -> Result<PendingConnect> {
    let (host, port, fp, launch, delivery) = (
        target.host,
        target.port,
        target.fingerprint,
        target.launch,
        target.delivery,
    );
    let attempt = std::sync::Arc::new(session::ConnectAttempt::default());
    let worker_attempt = attempt.clone();
    std::thread::Builder::new()
        .name("punktfunk-webos-connect".into())
        .spawn(move || {
            let mode = stream_mode(&settings, crate::platform::webos::device::native_mode());
            tracing::info!("requesting {}x{}@{}", mode.width, mode.height, mode.refresh_hz);
            session::connect(
                &session::ConnectParams {
                    host,
                    port,
                    mode,
                    bitrate_kbps: settings.bitrate_kbps,
                    hdr_enabled: settings.hdr_enabled,
                    ten_bit_sdr: settings.ten_bit_sdr,
                    audio_channels: settings.audio_channels,
                    identity,
                    pin: Some(fp),
                    launch,
                    delivery,
                    // A pinned host is reachable now or off, so a long budget would only hold the
                    // black launch scrim. Waiting on an operator is the pairing flow's job.
                    timeout: crate::services::budget::PROBE,
                    codec: settings.codec_pref(),
                    compositor: CompositorPref::from_name(&settings.compositor).unwrap_or(CompositorPref::Auto),
                    gamepad_type: settings.gamepad_type(),
                    cursor_capture: settings.cursor_capture(),
                    // `true` deliberately, whatever is attached right now: this is the SESSION-level
                    // cap, and the host advertises `HOST_CAP_PAD_AUDIO` only in reply to it. A pad
                    // plugged in later re-declares per-pad through `set_pad_audio_caps`, but only
                    // inside a session that claimed the cap up front — probing here would cost hotplug.
                    pad_audio_caps: crate::session::pad_audio::caps_for(&settings, true, true),
                    audio_route: settings.audio_route(),
                    present_priority: settings.present_priority(),
                    display_hdr: settings.hdr_display().hdr_meta(),
                },
                &worker_attempt,
            )
        })
        .map(|handle| PendingConnect {
            handle: Some(handle),
            attempt,
        })
        .context("spawn connect thread")
}

/// Set by signal handler; read as extra quit condition (webOS uses SIGTERM before SIGKILL).
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe handler: just set the flag, cleanup happens at next poll.
extern "C" fn handle_term_signal(_signum: libc::c_int) {
    QUIT_REQUESTED.store(true, Ordering::Relaxed);
}

/// Install SIGTERM/SIGINT handlers (best-effort; failure uses OS default).
fn install_signal_handlers() {
    // SAFETY: function pointer matches libc::signal's documented safe shape
    unsafe {
        libc::signal(libc::SIGTERM, handle_term_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle_term_signal as *const () as libc::sighandler_t);
    }
}

/// Turns core dumps off and deletes the ones earlier crashes left in `dir`.
///
/// A panic aborts, and the kernel writes `core.<pid>` into the app's directory: 20–130 MB
/// each, on the partition every developer app shares. The panic hook already logs the crash.
fn disable_core_dumps(dir: &std::path::Path) {
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `none` is a valid `rlimit` that outlives the call.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &none) } != 0 {
        let error = std::io::Error::last_os_error();
        tracing::warn!(%error, "core dump limit not lowered");
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(is_core_dump) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => tracing::info!(file = ?entry.file_name(), "removed an old core dump"),
            Err(error) => tracing::warn!(file = ?entry.file_name(), %error, "core dump not removed"),
        }
    }
}

/// `core.<pid>`, the name the kernel gives a dump.
fn is_core_dump(name: &str) -> bool {
    name.strip_prefix("core.")
        .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
}

/// Yellow-button log overlay state (process-lifetime, all screens).
/// Explicit discriminants: `cycle_log_overlay` stores `next as u8` and
/// `log_overlay_state` decodes it — the two must agree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogOverlayState {
    Off = 0,
    /// Live tail — updates every refresh.
    Live = 1,
    /// Frozen snapshot for stable reading.
    Frozen = 2,
}

static LOG_OVERLAY_STATE: AtomicU8 = AtomicU8::new(0);
static FROZEN_LOG_LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn frozen_log_lines() -> &'static Mutex<Vec<String>> {
    FROZEN_LOG_LINES.get_or_init(|| Mutex::new(Vec::new()))
}

fn log_overlay_state() -> LogOverlayState {
    match LOG_OVERLAY_STATE.load(Ordering::Relaxed) {
        1 => LogOverlayState::Live,
        2 => LogOverlayState::Frozen,
        _ => LogOverlayState::Off,
    }
}

/// Yellow button cycle Off → Live → Frozen → Off; capture on/off at boundaries.
fn cycle_log_overlay() {
    let next = match log_overlay_state() {
        LogOverlayState::Off => {
            crate::logger::set_ring_capture(true);
            LogOverlayState::Live
        }
        LogOverlayState::Live => {
            let mut snap = frozen_log_lines().lock().unwrap_or_else(PoisonError::into_inner);
            *snap = crate::logger::recent_lines(overlay::LOG_LINES);
            drop(snap);
            // Nothing reads the ring while frozen — stop capturing so logging threads
            // (the video pump above all) drop back to a single atomic load per event.
            crate::logger::set_ring_capture(false);
            LogOverlayState::Frozen
        }
        LogOverlayState::Frozen => LogOverlayState::Off,
    };
    LOG_OVERLAY_STATE.store(next as u8, Ordering::Relaxed);
}

/// Current lines to render; None if Off.
fn log_overlay_lines() -> Option<Vec<String>> {
    match log_overlay_state() {
        LogOverlayState::Off => None,
        LogOverlayState::Live => Some(crate::logger::recent_lines(overlay::LOG_LINES)),
        LogOverlayState::Frozen => Some(
            frozen_log_lines()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone(),
        ),
    }
}

pub fn run() -> Result<()> {
    install_signal_handlers();
    // Streams to a dev machine when `task deploy TELEMETRY=...` passed a
    // destination as a launch param; otherwise a versioned file under the app's
    // own writable directory (falls back to `/tmp` off-device, e.g. when
    // smoke-testing this binary on a Linux dev box before packaging). `_guard`
    // owns the background writer thread `non_blocking` spawns — held for the
    // whole process so logging never blocks a caller (in particular the
    // video-pump thread) on a slow disk or a dev machine not draining its
    // telemetry listener fast enough.
    let app_dir = store::app_dir();
    let _guard = crate::logger::init_subscriber(&app_dir).context("init logger")?;
    tracing::info!("punktfunk-webos starting");
    disable_core_dumps(&app_dir);
    // Logged before anything else can fail: a report from a model neither developer
    // owns is only actionable if the log says what it was running on.
    crate::platform::webos::device::DeviceInfo::detect().log();
    // Before settings load or any UI exists: `store::load` clamps against this.
    crate::core::caps::install(crate::platform::webos::device::video_caps());
    // A panic on ANY thread otherwise goes only to stderr, which a SAM-launched
    // native app has no terminal for — the app simply vanishes back to the
    // launcher with nothing written down. Routing it through `tracing` puts the
    // message and location in the same log as everything else, which is the
    // difference between "it crashed" and a diagnosable report. (This catches Rust
    // panics only; a fault inside the vendor decode libraries kills the process
    // outright and is visible only as a log that stops mid-session.)
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(
            "PANIC on thread {:?}: {info}",
            std::thread::current().name().unwrap_or("unnamed"),
        );
        // Global compositor state: a panic mid-stream would otherwise leave the whole
        // TV without a cursor.
        cursor::restore_on_exit();
        default_hook(info);
    }));

    // Errors from here on only ever reached stderr, which is invisible for a
    // webOS native app with no attached terminal.
    match run_inner() {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::error!("error: {e:#}");
            Err(e)
        }
    }
}

/// How one pass through the menu ended.
enum UiOutcome {
    /// A launch was committed; the stream loop takes it from here. Boxed: the shared settings
    /// document inside is hundreds of bytes and the other two arms carry almost nothing.
    Launch(Box<ConnectOutcome>),
    /// The user (or the OS) asked to close the app, carrying the selected host's exit action
    /// UNFIRED — see [`ConnectOutcome::exit_plan`] for why nothing runs it here.
    Quit(Option<crate::services::power::ExitPlan>),
    /// The remote's Blue key: run HDR calibration, then re-enter the menu.
    Calibrate,
}

enum StreamOutcome {
    /// The system asked the app to close (not just this stream) — exit fully.
    Quit,
    /// The host ended the session, or the user held Back — go back to the menu instead of
    /// exiting the app.
    ReturnToMenu,
}

mod calibration;
mod console_flow;
mod input;
mod overlay;
mod pad_session;
mod pads;
mod session_ext;
mod stream;
use input::*;
use stream::run_inner;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_kernel_dumps_are_swept() {
        assert!(is_core_dump("core.4007"));
        for keep in ["core", "core.", "core.json", "core.12a", "score.1", "settings.json"] {
            assert!(!is_core_dump(keep), "{keep}");
        }
    }

    /// "Native" (0) and "Match window" dial the panel; an explicit pick dials itself.
    #[test]
    fn a_native_mode_dials_the_panel() {
        let panel = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
        };
        let mut s = store::Settings::default();
        assert_eq!(stream_mode(&s, panel), panel, "the document's default is Native");
        s.match_window = true;
        s.width = 1280;
        assert_eq!(stream_mode(&s, panel), panel);
        s.match_window = false;
        s.height = 720;
        s.refresh_hz = 120;
        assert_eq!(
            stream_mode(&s, panel),
            Mode {
                width: 1280,
                height: 720,
                refresh_hz: 120
            }
        );
    }
}
