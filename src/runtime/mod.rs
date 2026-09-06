use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::config::Mode;
use sdl2::controller::GameController;

use crate::app::hero::Connect;
use crate::app::{App, HomeFocus, Screen};
use crate::core::event::MenuEvent;
use crate::core::settings::TvSettings;
use crate::platform::webos::cursor;
use crate::platform::webos::gamepad;
use crate::platform::webos::keyboard;
use crate::platform::webos::mouse;
use crate::services::store;
use crate::session;

/// A launch handed from the menu to the streaming loop: the connect thread (started early to
/// overlap the animation), the settings it was started with, and how much of the first-frame
/// budget the loading screen has already spent.
struct ConnectOutcome {
    handle: std::thread::JoinHandle<Result<session::Connected>>,
    /// What was dialled, kept so a lost link can be dialled again (`stream`'s reconnect).
    target: crate::app::ConnectTarget,
    settings: store::Settings,
    /// Whether the user's pick was `Automatic` — `settings.gamepad_type()` has already been
    /// resolved against the attached pad, so this is the only thing left that says a pad
    /// hotplugged mid-stream should re-decide the kind rather than keep the session default.
    gamepad_auto: bool,
    /// When the wait for a decoded frame runs out — one deadline for the whole launch, set by
    /// the loading screen (`app::hero`) and honoured again by the reveal in `stream`, so a host
    /// that connects and then decodes nothing costs [`crate::app::hero::FIRST_FRAME_WAIT`] once
    /// rather than once per screen. `None` when the loading screen never got that far.
    first_frame_deadline: Option<Instant>,
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
/// loop, while `App`'s own copy (what `StateWriter` persists and what the Settings row
/// displays) keeps saying `Automatic`. Resolving into the stored value instead would turn
/// a preference that means "match my pad" into a fixed pad kind the next time a different
/// controller was plugged in.
fn resolve_gamepad_type(
    mut settings: store::Settings,
    game_controller: &sdl2::GameControllerSubsystem,
) -> store::Settings {
    if settings.gamepad_type() != store::GamepadType::Auto {
        return settings;
    }
    if let Some(detected) = gamepad::detect_type(game_controller) {
        tracing::info!("controller Automatic → {detected:?} (mirroring the attached pad)");
        settings.set_gamepad_type(detected);
    }
    settings
}

/// Set when a connect attempt returns an error, cleared as the next one is spawned. The
/// loading screen otherwise has only `is_finished`, which success and failure reach alike —
/// and a failure never presents a frame, so it sat out the whole
/// [`crate::app::hero::HERO_LOADING_MAX`] backstop before the error could be shown.
static CONNECT_FAILED: AtomicBool = AtomicBool::new(false);

/// Start the connect on its own thread. Caller joins after animation (or immediately).
fn spawn_connect(
    identity: (String, String),
    target: crate::app::ConnectTarget,
    settings: store::Settings,
) -> Result<std::thread::JoinHandle<Result<session::Connected>>> {
    let (host, port, fp, launch) = (target.host, target.port, target.fingerprint, target.launch);
    CONNECT_FAILED.store(false, Ordering::Relaxed);
    std::thread::Builder::new()
        .name("punktfunk-webos-connect".into())
        .spawn(move || {
            // SDL2/Wayland reports refresh_rate=0; use settings' nominal rate instead
            let mode = Mode {
                width: settings.width,
                height: settings.height,
                refresh_hz: settings.refresh_hz,
            };
            tracing::info!(
                "requesting {}x{}@{}",
                settings.width,
                settings.height,
                settings.refresh_hz
            );
            session::connect(&session::ConnectParams {
                host,
                port,
                mode,
                bitrate_kbps: settings.bitrate_kbps,
                hdr_enabled: settings.hdr_enabled,
                audio_channels: settings.audio_channels,
                identity,
                pin: Some(fp),
                launch,
                // A pinned host is reachable now or off, so a long budget would only hold the
                // black launch scrim. Waiting on an operator is the pairing flow's job.
                timeout: crate::services::budget::PROBE,
                codec: settings.codec_pref(),
                gamepad_type: settings.gamepad_type(),
                cursor_capture: settings.cursor_capture(),
                // `true` deliberately, whatever is attached right now: this is the SESSION-level
                // cap, and the host advertises `HOST_CAP_PAD_AUDIO` only in reply to it. A pad
                // plugged in later re-declares per-pad through `set_pad_audio_caps`, but only
                // inside a session that claimed the cap up front — probing here would cost hotplug.
                pad_audio_caps: crate::session::pad_audio::caps_for(&settings, true),
                audio_route: settings.audio_route(),
                display_hdr: settings.hdr_display().hdr_meta(),
            })
            // Flagged before the handle is joined, so the loading screen can stop waiting
            // for a stream that is not coming — the error itself still travels by `Result`.
            .inspect_err(|_| CONNECT_FAILED.store(true, Ordering::Relaxed))
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

/// Diagnostics' "Show logs" toggle, for remotes without a Yellow button. Unlike
/// `cycle_log_overlay`'s 3-state cycle this only ever lands on Off/Live; the
/// preference itself is persisted separately, in `Settings::show_logs`.
pub(crate) fn set_log_overlay_enabled(enabled: bool) {
    crate::logger::set_ring_capture(enabled);
    let next = if enabled {
        LogOverlayState::Live
    } else {
        LogOverlayState::Off
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
    // Logged before anything else can fail: a report from a model neither developer
    // owns is only actionable if the log says what it was running on.
    crate::platform::webos::device::DeviceInfo::detect().log();
    // Before settings load or any UI exists: `store::load` clamps against this and
    // `app::menu::row_shown` hides what it can't offer.
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
///
/// `Option<ConnectOutcome>` used to say this, with `None` meaning "quit" — but the quit case
/// now carries something, and a sentinel that carries a payload wants a name.
enum UiOutcome {
    /// A launch was committed; the stream loop takes it from here. Boxed: the shared settings
    /// document inside is hundreds of bytes and the other two arms carry almost nothing.
    Launch(Box<ConnectOutcome>),
    /// The user (or the OS) asked to close the app, carrying the selected host's exit action
    /// UNFIRED — see [`ConnectOutcome::exit_plan`] for why nothing runs it here.
    Quit(Option<crate::services::power::ExitPlan>),
    /// The other menu flow was asked for. The menu loop re-enters and reads
    /// [`console_flow::wanted`] again, which is the whole of the old-UI ⇄ shell flip: each
    /// side reloads the document on entry, so neither can show the other's changes stale.
    Reenter,
}

enum StreamOutcome {
    /// The system asked the app to close (not just this stream) — exit fully.
    Quit,
    /// The host ended the session, or the user held Back — go back to the
    /// host-list/settings UI instead of exiting the app.
    ReturnToMenu,
}

mod input;
mod overlay;
mod session_ext;
mod stream;
mod ui_flow;
use input::*;
use stream::run_inner;
use ui_flow::run_ui_flow;

/// The shared gamepad shell's flow. `runtime` is Linux-only, and so is the shell.
mod console_flow;
