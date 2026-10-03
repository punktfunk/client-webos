//! The app's top level: SDL and the window ([`App`]), the menu ↔ stream alternation
//! ([`run_inner`]), and one launch's dial — the first and each reconnect of a lost link
//! ([`App::launch`]).
//!
//! One stream is split by concern: [`live`] brings it up, runs its tick ([`Stream`]) and tears it
//! down; [`events`] routes SDL's events within a tick; [`hud`] draws the overlays over the video.

use super::overlay::{self, ConfirmDialog};
use super::*;
use crate::platform::webos::input::{webos_scancode_down as key_down, RemoteKey, RemoteKeys, WEBOS_EXIT_SCANCODE};
use pf_client_core::menu_nav::MenuEvent;
use std::sync::Arc;

mod events;
mod hud;
pub(super) mod live;

/// How many times a lost link is dialled again before the menu, and the pause before each
/// dial. The host lingers a dropped session for a reconnect; a link that is still down
/// costs the connect budget (`budget::PROBE`) per attempt, with the toast up the whole time.
const RECONNECT_ATTEMPTS: u8 = 3;
const RECONNECT_PAUSE: Duration = Duration::from_secs(1);
/// A session that streamed this long earns the full attempt budget back: the count is for a
/// link that keeps failing, not for every drop in an evening.
const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(60);

/// Everything the app holds across the menu and every stream.
///
/// Fields are declared in drop order: the shell's GL host goes before the window it draws on,
/// and the window before the SDL subsystems under it.
struct App {
    /// Held across menu entries rather than per entry: the shell's GL context carries every
    /// compiled shader and its glyph atlas, and rebuilding those costs the cold-start stutter
    /// `console::gl` describes.
    console_gl: Option<console_flow::ConsoleGl>,
    /// Why the *last* stream attempt bounced to the menu, shown as the shell's notice on return.
    menu_notice: Option<String>,
    /// Owned here, not per screen: `GamepadAdded` fires only once per physical (re)connection,
    /// so pads opened earlier must carry across the menu and every stream.
    pads: pads::Pads,
    /// The panel, in the units every overlay draws in.
    display: (u32, u32),
    /// The overlays' fonts: the kit's, the same faces every screen draws with.
    fonts: pf_console_ui::theme::Fonts,
    identity: (String, String),
    events: sdl3::EventPump,
    window: sdl3::video::Window,
    sdl_audio: sdl3::AudioSubsystem,
    game_controller: sdl3::GamepadSubsystem,
    _video: sdl3::VideoSubsystem,
    sdl: sdl3::Sdl,
}

impl App {
    /// SDL, the window, and the client identity.
    fn bring_up() -> Result<Self> {
        // Stops webOS's launcher intercepting Back/Guide as its own shortcut (see `gamepad.rs`'s
        // BTN_GUIDE mapping). Must be set before window creation — these hints only latch there.
        sdl3::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
        // Without this webOS SIGTERMs the app on a held/root-level Back before it can react.
        sdl3::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT", "true");
        // Same for the remote's Home button, which otherwise backgrounds and kills the app; the
        // input loop re-opens the launcher itself via `luna::launch_home` instead. No `KEYS_META`
        // alongside it: a keyboard's Super key is Home-class, so that hint never suppressed it, and
        // in-stream it now reaches the host over evdev anyway (`platform::webos::evdev`).
        sdl3::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_HOME", "true");
        sdl3::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_GUIDE", "true");
        // Suppress webOS's launcher ribbon popping over the foregrounded app.
        sdl3::hint::set("SDL_WEBOS_ACCESS_POLICY_RIBBON", "false");
        // Magic Remote sleeps pointer after 5m; streams need it awake (remote IS the host mouse).
        // Set to 24h to make idle sleep a non-event. A webOS fork hint.
        sdl3::hint::set("SDL_WEBOS_CURSOR_SLEEP_TIME", "86400000");
        // Nothing here wants a mouse synthesized from touch — the Magic Remote is a real pointer,
        // and a pad's touchpad must not be one at all (`mouse::is_touch_emulated`).
        sdl3::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");
        // Same for a pen: muting pen events does not stop SDL synthesizing clicks from one.
        sdl3::hint::set("SDL_PEN_MOUSE_EVENTS", "0");
        // Keep Bluetooth PlayStation pads on their simple reports, SDL2's default. SDL3 defaults this to on.
        sdl3::hint::set("SDL_JOYSTICK_ENHANCED_REPORTS", "0");
        let sdl = sdl3::init().map_err(|e| anyhow::anyhow!("SDL_Init: {e}"))?;
        let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video subsystem: {e}"))?;
        let game_controller = sdl
            .gamepad()
            .map_err(|e| anyhow::anyhow!("SDL gamepad subsystem: {e}"))?;
        let sdl_audio = sdl.audio().map_err(|e| anyhow::anyhow!("SDL audio subsystem: {e}"))?;
        tracing::info!("SDL video subsystem up (driver: {})", video.current_video_driver());
        // Once per thread. native_mode read from connect thread; both fork entries touch compositor.
        crate::platform::webos::device::probe_panel();

        // SDL3 hangs modes off a `Display` instead of a bare index, and reports the rate as a float
        // (59.94 is no longer rounded to 60 — see `SDL_DisplayMode`'s numerator/denominator pair).
        let display_mode = video
            .get_primary_display()
            .and_then(|display| display.get_mode())
            .map_err(|e| anyhow::anyhow!("current display mode: {e}"))?;
        tracing::info!(
            "display mode: {}x{}@{:.3}",
            display_mode.w,
            display_mode.h,
            display_mode.refresh_rate
        );

        // The stream clears to alpha 0 for NDL's punch-through plane, and the window's EGL config
        // carries no alpha channel by default — without this every transparent clear composites
        // as opaque black. `.opengl()` below is what makes the attribute apply.
        video.gl_attr().set_alpha_size(8);
        // Skia clips paths against the stencil buffer, and the EGL config is chosen at window
        // creation — asking once the window exists is too late. Costs a stencil plane on a window
        // that already carries alpha; `console::gl` reads back what was actually granted rather
        // than assuming this was honoured.
        video.gl_attr().set_stencil_size(8);

        let window = video
            .window("punktfunk", display_mode.w as u32, display_mode.h as u32)
            .opengl()
            .fullscreen()
            .build()
            .map_err(|e| anyhow::anyhow!("create window: {e}"))?;
        // No SDL renderer: everything on screen is Skia on the console's GL context
        // (`console::gl`), which is the window's only one.
        tracing::info!("window created");

        let events = sdl.event_pump().map_err(|e| anyhow::anyhow!("event pump: {e}"))?;
        crate::platform::webos::input::mute_unused_events();

        let identity = store::load_or_create_identity().context("load_or_create_identity")?;
        let fonts = pf_console_ui::theme::build_fonts().context("overlay fonts")?;
        Ok(Self {
            console_gl: None,
            menu_notice: None,
            pads: pads::Pads::default(),
            display: (display_mode.w as u32, display_mode.h as u32),
            fonts,
            identity,
            events,
            window,
            sdl_audio,
            game_controller,
            _video: video,
            sdl,
        })
    }

    /// One launch: the dial the menu joined (its first frame already waited out), the stream,
    /// and each reconnect of a lost link (`RECONNECT_ATTEMPTS`). Leaves the reason on
    /// [`Self::menu_notice`] when it ends badly.
    fn launch(
        &mut self,
        first_dial: Result<session::Connected>,
        target: &ConnectTarget,
        settings: &store::Settings,
        gamepad_auto: bool,
    ) -> Result<StreamOutcome> {
        let mut first_dial = Some(first_dial);
        let mut connect_thread: Option<PendingConnect> = None;
        let mut reconnects: u8 = 0;
        loop {
            let dialled = match connect_thread.take() {
                None => first_dial.take().expect("only the launch has no thread"),
                Some(handle) => {
                    // A reconnect can be given up on from the remote; the launch was waited out by
                    // the shell, which read the remote itself.
                    let gave_up = wait_for_dial(&handle, &mut self.events);
                    if gave_up != DialWait::Done {
                        tracing::info!("reconnect given up: {gave_up:?}");
                        drop(handle);
                        if gave_up == DialWait::Quit {
                            return Ok(StreamOutcome::Quit);
                        }
                        self.menu_notice = Some("Connection lost".to_string());
                        return Ok(StreamOutcome::ReturnToMenu);
                    }
                    // Joined BEFORE the window is cleared transparent, so the reconnect toast stays
                    // up across the handshake and NDL load instead of a black punch-through hole.
                    let connected = handle.join().expect("connect thread panicked");
                    if connected.is_ok() {
                        // No menu is up to keep drawing: the toast holds until the first frame.
                        live::wait_first_frame();
                    }
                    connected
                }
            };
            let connected = match dialled {
                Ok(c) => c,
                Err(e) if reconnects > 0 && reconnects < RECONNECT_ATTEMPTS => {
                    tracing::warn!("reconnect failed: {e:#}");
                    reconnects += 1;
                    connect_thread = Some(self.redial(reconnects, target, settings)?);
                    continue;
                }
                Err(e) => {
                    // Return to the menu with the reason on screen instead of `?`-ing the app down.
                    tracing::error!("session connect failed: {e:#}");
                    let what = if reconnects > 0 { "reconnect" } else { "connect" };
                    self.menu_notice = Some(format!("Couldn't {what}: {}", crate::core::errors::friendly(&e)));
                    return Ok(StreamOutcome::ReturnToMenu);
                }
            };
            let session_started = Instant::now();
            let ended = self.stream(connected, settings, gamepad_auto);
            // A lost link is dialled again with the same target and settings, the toast up over
            // the (now empty) video plane while the handshake runs. Anything else ends here.
            if session_started.elapsed() >= RECONNECT_RESET_AFTER {
                reconnects = 0;
            }
            let again = ended.lost
                && matches!(ended.outcome, StreamOutcome::ReturnToMenu)
                && reconnects < RECONNECT_ATTEMPTS
                && !QUIT_REQUESTED.load(Ordering::Relaxed);
            if !again {
                return Ok(ended.outcome);
            }
            reconnects += 1;
            self.menu_notice = None;
            connect_thread = Some(self.redial(reconnects, target, settings)?);
        }
    }

    /// Puts the reconnect toast up over the emptied video plane and starts dial `attempt`.
    fn redial(&mut self, attempt: u8, target: &ConnectTarget, settings: &store::Settings) -> Result<PendingConnect> {
        tracing::warn!("connection lost — reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
        let text = format!("Connection lost — reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
        let frame = overlay::frame(
            &mut self.console_gl,
            &self.window,
            &self.fonts,
            self.display,
            overlay::TRANSPARENT,
            |f| {
                overlay::toast(f, &text, 1.0);
            },
        );
        overlay::drawn(frame, &mut false);
        std::thread::sleep(RECONNECT_PAUSE);
        spawn_connect(self.identity.clone(), target.clone(), settings.clone())
    }
}

/// How a reconnect wait ended.
#[derive(Debug, PartialEq, Eq)]
enum DialWait {
    /// The handshake finished; join it.
    Done,
    /// Back or the EXIT gesture: give up and return to the menu.
    Cancel,
    /// An SDL quit or a signal: leave the app.
    Quit,
}

/// Waits for a reconnect's handshake with the toast up, reading input meanwhile. The worker
/// runs to completion and drops what it built on a give-up, which is the cancel: nothing joins it.
fn wait_for_dial(handle: &PendingConnect, events: &mut sdl3::EventPump) -> DialWait {
    let mut exit_held = key_down(WEBOS_EXIT_SCANCODE);
    let mut remote_keys = RemoteKeys::default();
    loop {
        if handle.is_finished() {
            return DialWait::Done;
        }
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            return DialWait::Quit;
        }
        // Bounded, not blocking: completion and the signal flag never wake SDL.
        crate::platform::webos::input::wait_for_event(Duration::from_millis(20));
        for event in events.poll_iter() {
            if matches!(event, sdl3::event::Event::Quit { .. }) {
                return DialWait::Quit;
            }
            // Back has no keycode in SDL3; check before keycodes (see RemoteKeys). Down edge only:
            // `press` would read the release of a Back tapped as the link dropped as a cancel.
            if remote_keys.edge(&event, true) == Some((RemoteKey::Back, true)) {
                return DialWait::Cancel;
            }
            if let sdl3::event::Event::KeyDown {
                keycode: Some(k),
                repeat: false,
                ..
            } = event
            {
                if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) {
                    return DialWait::Cancel;
                }
            }
        }
        if exit_gesture_fired(&mut exit_held) {
            return DialWait::Cancel;
        }
    }
}

pub(super) fn run_inner() -> Result<()> {
    let mut app = App::bring_up()?;
    // The loop's value is the exit action owed on the way out — see the single fire site
    // below it, which is the only reason this is a `break`-with-value rather than a `return`.
    let exit_plan = loop {
        let ui = console_flow::run(
            &app.window,
            &mut app.console_gl,
            &mut app.events,
            &app.game_controller,
            &mut app.pads,
            &app.identity,
            app.menu_notice.take(),
        )?;
        // A `let ... else` can't bind out of its own else arm, and the quit case is exactly
        // where the value is.
        let ConnectOutcome {
            connected,
            target,
            settings,
            gamepad_auto,
            exit_plan,
        } = match ui {
            UiOutcome::Launch(outcome) => *outcome,
            UiOutcome::Quit(plan) => break plan,
            UiOutcome::Calibrate(plan) => {
                match calibration::run(
                    &app.window,
                    &mut app.console_gl,
                    &mut app.events,
                    &app.fonts,
                    app.display,
                )? {
                    calibration::Exit::Menu => continue,
                    calibration::Exit::Quit => break plan,
                }
            }
        };
        tracing::debug!("settings: {settings:?}");
        match app.launch(connected, &target, &settings, gamepad_auto)? {
            StreamOutcome::Quit => break exit_plan,
            StreamOutcome::ReturnToMenu => {}
        }
    };
    // The one place the app acts on a host's exit behaviour. Every way out of the loop above
    // has already torn down whatever session it had, which is the ordering this action needs:
    // the host refuses a cert-lane power action while a session is live. Blocking, because the
    // process is ending — a request abandoned mid-flight does nothing.
    crate::platform::webos::ndl::await_teardown();
    if let Some(plan) = exit_plan {
        // A connect abandoned on the Connecting card may still hold a host session.
        super::await_abandoned_connects();
        plan.run();
    }
    tracing::info!("punktfunk-webos exiting cleanly");
    Ok(())
}

/// How one stream ended.
struct Ended {
    outcome: StreamOutcome,
    /// The link died under a session the user did not end: the one end worth dialling again.
    lost: bool,
}

/// What a stream borrows from the [`App`] for its length, plus the session itself.
struct Cx<'a> {
    window: &'a sdl3::video::Window,
    gl: &'a mut Option<console_flow::ConsoleGl>,
    fonts: &'a pf_console_ui::theme::Fonts,
    display: (u32, u32),
    pads: &'a mut pads::Pads,
    game_controller: &'a sdl3::GamepadSubsystem,
    settings: &'a store::Settings,
    connected: &'a session::Connected,
}

/// One stream's state, carried from tick to tick. Built and run by [`live`]; SDL's events are
/// routed by [`events`].
struct Stream {
    // ---- the session's pads ------------------------------------------------------------------
    /// What each pad arriving mid-stream is declared as: `Automatic` re-decides per pad.
    kind_setting: store::GamepadType,
    /// Pad audio's envelopes, when the host has the plane and Settings wants a lane.
    pad_audio: Option<Arc<crate::session::pad_audio::Envelopes>>,
    /// Which wire pad each evdev touchpad/motion node feeds, shared with the HID reader.
    pad_routes: pads::PadRoutes,
    /// The mode the session was dialled at, for the dial's resolution slot.
    native_mode: punktfunk_core::config::Mode,

    // ---- input --------------------------------------------------------------------------------
    cursor: cursor::Cursor,
    /// Tells the remote's keys from a pad's echo, off the remote's own nodes. Declared before
    /// [`Self::hid`] so its nodes close while the reader they came from is still running.
    remote_gate: RemoteGate,
    /// Raw evdev HID, sending on the reader thread rather than queued for the ~2ms main loop
    /// so a 1000 Hz mouse isn't re-resampled.
    hid: Option<crate::platform::webos::evdev::HidInput>,
    /// Flips once a HID mouse is found — `HidInput::start` no longer scans before returning
    /// (that blocked every stream connect on the node-open cost), so presence is only known
    /// once the reader thread's own scan catches up; checked each tick.
    hid_device_seen: bool,
    /// The one resolver for the remote's own keys in this loop — see `RemoteKeys`. The
    /// stream takes both edges: it mirrors held keys to the host (Back as Esc, Red as the
    /// right button), so dropping a release would leave the host holding one down.
    remote_keys: RemoteKeys,
    /// The remote's Red key as the right button — see `RemoteButtons`. Fed only the remote's
    /// own input; a real HID mouse's clicks never reach it.
    buttons: mouse::RemoteButtons,
    relative_motion: mouse::RelativeMotion,
    /// Blue controls text input because streams have no focused text field.
    text_input: TextInputController,
    /// Rising-edge polls, seeded from live key state — see [`App::stream`].
    home_held: bool,
    exit_held: bool,
    /// Remote-key presses, set by the event arm and consumed once a tick. SDL3 gives the colour
    /// keys no scancode; state-array bits latch for the session (see `RemoteKey`).
    green_pressed: bool,
    yellow_pressed: bool,
    blue_pressed: bool,
    /// Off while webOS has another app or panel up: evdev reads the devices regardless.
    focused: bool,
    /// Whether last tick already released for the open dialog.
    input_suspended: bool,
    /// A dial tap of Guide or QAM still owes its release: `(bit, pad, due)`.
    tap_up: Option<(u32, u8, Instant)>,

    // ---- the dial and the stop dialog ---------------------------------------------------------
    /// "Stop streaming?" — 0 = "Disconnect" focused, 1 = "Cancel" (default on open — safer).
    disconnect: ConfirmDialog,
    /// The quick-action dial: Select+A on the pad, drawn over the video (`core::dial`).
    ring: pf_console_ui::Ring,
    ring_was_open: bool,
    /// When the dial's facts were last pushed; `None` owes a push on the next tick.
    ring_facts_at: Option<std::time::Instant>,
    /// The disconnect dialog takes the pads too — see [`Stream::tick_dialog_shortcuts`].
    dialog_was_open: bool,
    /// The dial asked for the next stats tier.
    ring_stats: bool,

    // ---- the overlays -------------------------------------------------------------------------
    hud: hud::Hud,

    // ---- how it ends --------------------------------------------------------------------------
    /// Waits for the dialog's close-fade to finish.
    pending_outcome: Option<StreamOutcome>,
    /// Set when the user confirms the disconnect dialog — distinguishes that from the host
    /// ending the session or the network dropping out, so the end notice only appears for the
    /// latter. (SIGTERM/window-close also call `disconnect_quit()`, but those end with
    /// `StreamOutcome::Quit` and never reach that check.)
    client_initiated_disconnect: bool,
    /// See [`Ended::lost`].
    lost: bool,
    /// Why the stream ended, for the menu.
    notice: Option<String>,
}
