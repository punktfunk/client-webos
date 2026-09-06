//! The shared gamepad shell as one of this client's two menu flows.
//!
//! A sibling of [`super::ui_flow`], not a replacement: it hands back the same [`UiOutcome`], so
//! the streaming loop below it cannot tell which menu produced a launch. Only one of the two is
//! live at a time, which is what lets both own the settings document in turn — each reloads it
//! on entry (`store::load`), so a value changed on one side is never stale on the other.
//!
//! The shell renders through its own GL context on the app's window; SDL's renderer takes the
//! screen back on its next `Canvas` draw. See `console::gl`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuNav, MenuSample, PadInfo};
use pf_console_ui::{Console, ConsoleEntry, ConsoleHandles, ConsoleOptions, InputSource, Key, Platform, Viewport};

use super::*;
use crate::console::Service;
use crate::core::perf::{ArtSnapshot, Perf};
use crate::core::settings::TvSettings;
use crate::services::store::console::ConsoleStore;
use crate::services::store::{shared, StateWriter};

pub(super) use crate::console::ConsoleGl;

/// Target period when the swap does not block — a driver that ignores the vsync request would
/// otherwise spin this loop at whatever the GPU can manage.
const TICK_BUDGET: Duration = Duration::from_millis(16);

/// No input for this long and the shell is being looked at, not used: one extra frame period
/// between swaps. Android's console does the same, for the same reason — an idle carousel
/// should not keep a TV's panel at full rate. Any input restores it on the next frame.
const IDLE_AFTER: Duration = Duration::from_secs(60);
const IDLE_FRAME_STEP: Duration = Duration::from_millis(16);

/// Whether this launch should draw the shared shell rather than this client's own menus.
///
/// Read from disk on every menu entry rather than cached, because either UI's own switch writes
/// it and the flip is meant to land the moment you leave that screen. `pad_connected` is the
/// other half: under the default "With a controller" mode, plugging one in IS the switch.
pub(super) fn wanted(pad_connected: bool) -> bool {
    store::load().state.settings.gamepad_ui_active(pad_connected)
}

/// Run the shell until it commits a launch or asks to leave.
pub(super) fn run(
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    gl: &mut Option<ConsoleGl>,
    events: &mut sdl2::EventPump,
    game_controller: &sdl2::GameControllerSubsystem,
    controller: &mut Option<GameController>,
    identity: &(String, String),
    // Why the last stream bounced back here, if it did. The old menus put this on the Home
    // status line; dropping it would leave a failed connect explaining nothing.
    notice: Option<String>,
) -> Result<UiOutcome> {
    // The document as it is right now — the old menus may have written it since the last entry.
    let state = store::load().state;
    let writer = Arc::new(StateWriter::spawn(state.clone()));
    let store = Arc::new(ConsoleStore::new(state, writer));
    let handles = ConsoleHandles::new();
    let mut service = Service::new(handles.clone(), store.clone(), identity.clone());

    // A preview UI must not be able to take the app down with it: if GL or Skia will not come
    // up on this set, say so, turn the shell back off in the document and let the classic menus
    // have the screen. Returning `Err` here would propagate out of the whole menu loop.
    let console_gl = match bring_up(gl, canvas) {
        Ok(gl) => gl,
        Err(e) => {
            tracing::error!("console: no GL host on this TV ({e:#}) — falling back to the classic menus");
            return Ok(leave_for_classic(&store));
        }
    };

    let opts = ConsoleOptions {
        device_name: "webOS TV".into(),
        deck: false,
        // This client does have another UI to fall back to, and the shell's own
        // "Controller-optimized UI" row is gated on saying so — turning it off there is the
        // way back to the cursor menus.
        fallback_ui: true,
        store: Some(store.clone()),
        platform: Platform::WebOS,
        gpu_cache_bytes: crate::console::GPU_CACHE_BYTES,
    };
    // Land back on the shelf the last stream was launched from, the way the classic menus
    // restore `selected_host` on entry. The fetch is seeded here because the shell only asks
    // for a library when it navigates to one, and this entry skips that navigation.
    let entry = service.selected_row().map_or(ConsoleEntry::Home, |row| {
        handles.bus.send(pf_console_ui::ConsoleCmd::FetchLibrary {
            addr: row.addr.clone(),
            mgmt: row.mgmt_port,
            fp_hex: row.fp_hex.clone(),
        });
        ConsoleEntry::Library(Box::new(row))
    });
    let opened_on_a_shelf = matches!(entry, ConsoleEntry::Library(_));
    let mut console = match Console::new(opts, entry, &handles) {
        Ok(console) => console,
        // Same reasoning as the GL bring-up above — most likely the font build.
        Err(e) => {
            tracing::error!("console: shell would not build ({e:#}) — falling back to the classic menus");
            return Ok(leave_for_classic(&store));
        }
    };
    if let Some(notice) = notice {
        handles.console.set_notice(notice);
    }

    // What this panel costs, for the log Diagnostics ▸ Send logs carries — see `core::perf`.
    // Labelled by where the shell came up: entering on a shelf is the case that loads art, and
    // the case the CX reports as slow.
    let mut perf = Perf::new();
    perf.mark(if opened_on_a_shelf { "library" } else { "home" });
    let mut nav = MenuNav::new();
    let mut sample = MenuSample::default();
    let mut pads: Vec<PadInfo> = Vec::new();
    let mut menu_out: Vec<MenuEvent> = Vec::new();
    let mut last_input = Instant::now();
    let mut home_held = false;
    let mut exit_held = false;
    let mut green_held = false;
    // Where the Magic Remote's pointer last was. SDL's wheel event carries no position, and
    // the pump cannot be asked for one from inside its own `poll_iter`.
    let mut pointer_at = (0.0f32, 0.0f32);
    // A launch the shell committed: the connect runs while the shell keeps drawing its
    // Connecting card, exactly as the old menus overlap it with the loading screen.
    let mut connect: Option<(
        std::thread::JoinHandle<Result<session::Connected>>,
        crate::app::ConnectTarget,
        store::Settings,
        bool,
    )> = None;

    let outcome = 'ui: loop {
        let frame_start = Instant::now();
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            tracing::warn!("SIGTERM/SIGINT received in the console");
            break 'ui UiOutcome::Quit(exit_plan(&service, identity));
        }
        // Captured, or webOS kills the app rather than backgrounding it.
        if home_key_fired(&mut home_held) {
            crate::platform::webos::luna::launch_home();
        }
        // A held/root-level Back arrives as the discrete EXIT key, not as a long Back — the
        // classic menus quit on it, so this does too rather than being the one screen where
        // the gesture does nothing.
        if exit_gesture_fired(&mut exit_held) {
            tracing::info!("console: EXIT gesture — quitting app");
            break 'ui UiOutcome::Quit(exit_plan(&service, identity));
        }
        // Green is polled (it is a scancode with no keycode), Red arrives as a keycode in the
        // loop below. Both exist to make the shell's Secondary/Tertiary REACHABLE from a Magic
        // Remote at all: the library screen puts Collections on Secondary and Options — the
        // whole host menu — on Tertiary, and unlike the home screen it has no d-pad fallback.
        let green =
            crate::platform::webos::input::webos_scancode_down(crate::platform::webos::input::WEBOS_GREEN_SCANCODE);
        if green && !green_held {
            last_input = Instant::now();
            console.menu(MenuEvent::Tertiary, InputSource::Keys);
        }
        green_held = green;
        // webOS is documented to hand back a drawable that is not the window it was asked for
        // (handoff trap 10), while SDL reports pointer events in WINDOW coordinates. The shell
        // hit-tests in surface pixels, so the two have to be reconciled before it sees them.
        let scale = pointer_scale(canvas);

        for event in events.poll_iter() {
            use sdl2::event::Event;
            // A thumb resting on a pad's touchpad must not hover and click rows.
            if crate::platform::webos::mouse::is_touch_emulated(&event) {
                continue;
            }
            match event {
                Event::Quit { .. } => {
                    tracing::info!("quit during the console");
                    break 'ui UiOutcome::Quit(exit_plan(&service, identity));
                }
                Event::ControllerDeviceAdded { which, .. } => {
                    if controller.is_none() {
                        match game_controller.open(which) {
                            Ok(c) => {
                                tracing::info!("controller connected: {}", c.name());
                                *controller = Some(c);
                            }
                            Err(e) => tracing::warn!("controller open failed: {e}"),
                        }
                    }
                }
                Event::ControllerDeviceRemoved { .. } => {
                    *controller = None;
                    // An unplugged pad sends no releases: drop what the synthesizer holds.
                    nav.reset();
                    sample = MenuSample::default();
                }
                Event::KeyDown {
                    keycode: Some(k),
                    repeat,
                    keymod,
                    ..
                } => {
                    last_input = Instant::now();
                    // Red is the remote's Secondary — see the Green note above for why the
                    // colour keys carry these at all. `!repeat` so a held key fires once.
                    if !repeat && k.into_i32() == crate::platform::webos::input::WEBOS_RED_KEYCODE {
                        console.menu(MenuEvent::Secondary, InputSource::Keys);
                        continue;
                    }
                    let shift = keymod.intersects(sdl2::keyboard::Mod::LSHIFTMOD | sdl2::keyboard::Mod::RSHIFTMOD);
                    // While a field is being edited the shell wants keys, not menu moves —
                    // an arrow has to walk the caret rather than the row under it.
                    if console.editing() {
                        // The remote's number pad types into the field. Fed as TEXT rather
                        // than through SDL's text input, which on webOS raises the SYSTEM
                        // on-screen keyboard — over the one the shell draws itself.
                        if let Some(digit) = crate::platform::webos::input::digit_key_value(k) {
                            console.text(&digit.to_string());
                            continue;
                        }
                        if let Some(key) = editing_key(k) {
                            console.key(key, shift, repeat);
                            continue;
                        }
                    }
                    if let Some(ev) = menu_event(k) {
                        if let Some(_pulse) = console.menu(ev, InputSource::Keys) {
                            // Nothing to feel: the remote has no haptics, and the pad's own
                            // rumble is the stream's lane (`session::pad_audio`).
                        }
                    }
                }
                Event::TextInput { text, .. } => {
                    last_input = Instant::now();
                    console.text(&text);
                }
                Event::MouseMotion { x, y, .. } => {
                    last_input = Instant::now();
                    pointer_at = scale.at(x, y);
                    console.pointer(PointerInput::Move {
                        x: pointer_at.0,
                        y: pointer_at.1,
                    });
                }
                Event::MouseButtonDown { x, y, mouse_btn, .. } => {
                    last_input = Instant::now();
                    if let Some(button) = pointer_button(mouse_btn) {
                        pointer_at = scale.at(x, y);
                        console.pointer(PointerInput::Down {
                            x: pointer_at.0,
                            y: pointer_at.1,
                            button,
                            // The Magic Remote is a real pointer, never a finger — its press
                            // acts immediately rather than waiting for a lift.
                            touch: false,
                        });
                    }
                }
                Event::MouseButtonUp { x, y, mouse_btn, .. } => {
                    last_input = Instant::now();
                    if let Some(button) = pointer_button(mouse_btn) {
                        pointer_at = scale.at(x, y);
                        console.pointer(PointerInput::Up {
                            x: pointer_at.0,
                            y: pointer_at.1,
                            button,
                        });
                    }
                }
                Event::MouseWheel { y, .. } => {
                    last_input = Instant::now();
                    console.pointer(PointerInput::Wheel {
                        x: pointer_at.0,
                        y: pointer_at.1,
                        dy: y as f32,
                    });
                }
                _ => {}
            }
        }

        // The controller SDL has open, unless it is the TV's own remote — webOS enumerates the
        // Magic Remote as a game controller, and treating it as a pad would claim a pad that is
        // not in the room. Its buttons still reach the shell, as keys.
        let pad = controller
            .as_ref()
            .filter(|c| !crate::platform::webos::gamepad::is_tv_remote(&c.name()));
        // The pad through the shared synthesizer, so repeats, dead zone and hysteresis match
        // every other client rather than being re-invented here.
        if let Some(pad) = pad {
            sample = pad_sample(pad);
        }
        menu_out.clear();
        nav.poll(&sample, Instant::now(), &mut menu_out);
        // What the synthesizer produced IS the pad's input — including the repeats a held
        // direction generates, which a raw sample comparison would read as no movement.
        if !menu_out.is_empty() {
            last_input = Instant::now();
        }
        for ev in menu_out.drain(..) {
            console.menu(ev, InputSource::Pad);
        }

        service.tick();

        // What the shell asked for.
        while let Some(action) = console.take_action() {
            match action {
                OverlayAction::Launch {
                    addr,
                    port,
                    fp_hex,
                    launch,
                    title,
                    profile,
                    request_access,
                } => {
                    if request_access {
                        // The shell's park-and-wait handshake. This client's own path for it
                        // lives in the pairing modal (`session::probe::request_access`) and has
                        // no console screen yet, so say so rather than dial and hang.
                        handles
                            .console
                            .set_notice("Pair this TV from the classic menus first".into());
                        continue;
                    }
                    let want = Launch {
                        addr,
                        port,
                        fp_hex,
                        launch,
                        profile,
                    };
                    let where_ = format!("{title} on {}:{}", want.addr, want.port);
                    match start_launch(&store, identity, game_controller, want) {
                        Ok(started) => {
                            tracing::info!("console: launching {where_}");
                            console.session_phase(SessionPhase::Connecting);
                            connect = Some(started);
                        }
                        Err(e) => {
                            tracing::warn!("console: launch refused: {e:#}");
                            handles.console.set_notice(format!("Couldn't start — {e}"));
                        }
                    }
                }
                // Nothing to swap to: this client reveals its stream by LEAVING the console
                // loop, so the hold ends when the launch commits rather than on this ask. The
                // action exists for a host that keeps the console and its stream side by side.
                OverlayAction::ShowStream => {}
                OverlayAction::CancelConnect => {
                    // Dropping the handle IS the cancel: the worker runs to completion and
                    // drops the `Connected` it built, which tears the session down cleanly —
                    // just a handshake later than the button press.
                    if connect.take().is_some() {
                        tracing::info!("console: connect cancelled");
                        console.session_phase(SessionPhase::Ended(None));
                    }
                }
                OverlayAction::Quit => {
                    tracing::info!("console: quit");
                    break 'ui UiOutcome::Quit(exit_plan(&service, identity));
                }
                // SDL owns the clipboard and it lives on this thread, which is why this is an
                // action rather than a bus command.
                OverlayAction::CopyText(text) => {
                    if let Err(e) = canvas.window().subsystem().clipboard().set_clipboard_text(&text) {
                        tracing::warn!("console: clipboard: {e}");
                    }
                }
            }
        }

        // The handshake landed (or failed): the streaming loop takes it from here, and a
        // failure goes back to the menu with the reason, exactly as the old flow does.
        if connect.as_ref().is_some_and(|(h, ..)| h.is_finished()) {
            let (handle, target, settings, gamepad_auto) = connect.take().expect("just checked");
            break 'ui UiOutcome::Launch(Box::new(ConnectOutcome {
                handle,
                target,
                settings,
                gamepad_auto,
                // The shell has no loading screen of its own to spend a budget on — the
                // streaming loop starts the first-frame wait fresh.
                first_frame_deadline: None,
                exit_plan: exit_plan(&service, identity),
            }));
        }

        // Draw.
        let mut idled = Duration::ZERO;
        if last_input.elapsed() >= IDLE_AFTER {
            std::thread::sleep(IDLE_FRAME_STEP);
            idled = IDLE_FRAME_STEP;
        }
        let (w, h) = canvas.window().drawable_size();
        pads.clear();
        // The switch, watched the way the cursor menus watch theirs: the shell's own
        // "Controller-optimized UI" row writes it, and under "With a controller" an unplugged
        // pad withdraws it without writing anything. Plain `Reenter` — `leave_for_classic`
        // would turn the switch off, and a pad going flat is not the user saying "off".
        let state = store.snapshot();
        if !state.settings.gamepad_ui_active(pad.is_some()) {
            tracing::info!("console: the controller UI no longer applies — back to the cursor menus");
            break 'ui UiOutcome::Reenter;
        }
        // 🛑 `None` unless a real pad is open, not the stored preference: this picks the GLYPH
        // LEGEND, and claiming a pad prints button marks for buttons that are not in the room.
        // It is also what the home screen reads to put Options and Settings on the d-pad
        // instead of on Y and X (`pads.is_empty()`).
        // Automatic means "whatever is plugged in", so the legend asks SDL rather than
        // printing the host's Xbox default over a DualSense. An explicit pick still wins:
        // someone who chose a pad kind wants its glyphs whatever is attached.
        let pad_pref = pad.map(|_| {
            let stored = state.settings.gamepad_type();
            let kind = if stored == store::GamepadType::Auto {
                crate::platform::webos::gamepad::detect_type(game_controller).unwrap_or(stored)
            } else {
                stored
            };
            crate::core::settings::gamepad_pref(kind)
        });
        if let (Some(pad), Some(pref)) = (pad, pad_pref) {
            pads.push(pad_info(pad, pref));
        }
        let label = pads.first().map(|p| p.name.clone());
        {
            let surface = console_gl.surface(w, h)?;
            console.frame(
                surface.canvas(),
                // No insets: webOS hands a native app a clean 1080p surface with no overscan
                // margin to keep chrome out of.
                &Viewport::plain(w, h),
                label.as_deref(),
                pad_pref,
                &pads,
            );
        }
        console_gl.flush();
        // Before the swap: `gl_swap_window` blocks on vsync, and a frame time that includes it
        // measures the panel's refresh rate rather than what this build costs. The idle step
        // comes off for the same reason — it is this loop waiting on purpose, not work.
        let cpu = frame_start.elapsed().saturating_sub(idled);
        if let Some(report) = perf.frame(cpu, art_snapshot()) {
            tracing::info!("{}", report.line());
        }
        canvas.window().gl_swap_window();

        let elapsed = frame_start.elapsed();
        if elapsed < TICK_BUDGET {
            std::thread::sleep(TICK_BUDGET - elapsed);
        }
    };

    if let Some(report) = perf.finish(art_snapshot()) {
        tracing::info!("{}", report.line());
    }
    service.stop();
    // Covers and glyph atlases go back before the stream takes the GPU; the context and its
    // compiled shaders stay, so coming back here is a re-upload rather than a cold start.
    console_gl.release_resources();
    Ok(outcome)
}

/// The shell's cover-art counters in `core::perf`'s shape. The conversion is the only thing
/// this gate carries; every decision about them is host-tested in that module.
fn art_snapshot() -> ArtSnapshot {
    let a = pf_console_ui::art_stats();
    ArtSnapshot {
        decoded: a.decoded,
        total_us: a.total_us,
        max_us: a.max_us,
        native_scaled: a.native_scaled,
    }
}

/// Bring up (or reuse) the shell's GL context and make it current. Split out so the caller can
/// answer a failure by handing the screen back rather than by failing the app.
pub(super) fn bring_up<'a>(
    gl: &'a mut Option<ConsoleGl>,
    canvas: &sdl2::render::Canvas<sdl2::video::Window>,
) -> Result<&'a mut ConsoleGl> {
    if gl.is_none() {
        // The first entry of the process pays for the context and the shader warm-up; every
        // later one reuses both (see `ConsoleGl::ctx`).
        *gl = Some(ConsoleGl::new(canvas.window(), canvas.window().subsystem())?);
    }
    let gl = gl.as_mut().expect("just built");
    // The classic menus and the stream have both made their own context current in between.
    gl.make_current(canvas.window())?;
    Ok(gl)
}

/// Turn the console off in the document, so the next menu entry lands on the classic menus.
/// Hand the menus back and turn the switch off, for the cases where this shell cannot run at
/// all. A user who turned it off in the shell's own row has already written `false`, and a pad
/// being unplugged must NOT write anything — the switch stays on, [`wanted`] simply stops
/// agreeing until the next pad arrives.
fn leave_for_classic(store: &Arc<ConsoleStore>) -> UiOutcome {
    store.edit(|state| {
        state.settings.set_gamepad_ui(false);
        true
    });
    // Not a quit: `UiOutcome::Quit` ends the app. The streaming loop's menu loop re-enters
    // with the setting now off, which is the flip.
    UiOutcome::Reenter
}

/// What the shell committed to launch: the host, a title or the desktop, and a one-off profile.
struct Launch {
    addr: String,
    port: u16,
    fp_hex: String,
    launch: Option<String>,
    profile: Option<String>,
}

/// Start the connect for a launch the shell committed.
fn start_launch(
    store: &Arc<ConsoleStore>,
    identity: &(String, String),
    game_controller: &sdl2::GameControllerSubsystem,
    want: Launch,
) -> Result<(
    std::thread::JoinHandle<Result<session::Connected>>,
    crate::app::ConnectTarget,
    store::Settings,
    bool,
)> {
    let Launch {
        addr,
        port,
        fp_hex,
        launch,
        profile,
    } = want;
    let state = store.snapshot();
    let fingerprint = state
        .known_hosts
        .iter()
        .find(|h| h.addr == addr && h.port == port)
        .and_then(crate::core::model::KnownHost::fingerprint)
        .or_else(|| shared::parse_fp(&fp_hex))
        .context("that host isn't paired with this TV yet")?;
    let mut settings = shared::launch_settings(&state, &addr, port, launch.as_deref(), profile.as_deref());
    let gamepad_auto = settings.gamepad_type() == store::GamepadType::Auto;
    settings = resolve_gamepad_type(settings, game_controller);
    let target = crate::app::ConnectTarget {
        host: addr,
        port,
        fingerprint,
        launch,
        profile,
    };
    let handle = spawn_connect(identity.clone(), target.clone(), settings.clone())?;
    Ok((handle, target, settings, gamepad_auto))
}

/// What to do to the selected host on the way out. Same rule as `App::exit_plan`: the selected
/// host only, and only one that answered its last reachability check — a host already down
/// costs the whole budget on a connection that cannot complete.
fn exit_plan(service: &Service, identity: &(String, String)) -> Option<crate::services::power::ExitPlan> {
    let state = service.store.snapshot();
    let (host, port) = state.selected_host.clone()?;
    let known = state.known_hosts.iter().find(|h| h.addr == host && h.port == port)?;
    known.exit_action.action_id()?;
    if !service.is_online(known) {
        tracing::debug!("exit action skipped: {host} was not reachable");
        return None;
    }
    Some(crate::services::power::ExitPlan {
        addr: known.addr.clone(),
        mgmt_port: known.mgmt_port.unwrap_or(crate::services::library::DEFAULT_MGMT_PORT),
        identity: identity.clone(),
        // Required, not merely pinned-if-known: a power action is the last request to send to
        // an unverified peer, and an unpaired host would refuse it anyway.
        pin: Some(known.fingerprint()?),
        action: known.exit_action,
    })
}

/// The pad as the shared synthesizer reads it: face buttons, shoulders, the left stick in wire
/// units, and the d-pad.
fn pad_sample(pad: &GameController) -> MenuSample {
    use sdl2::controller::{Axis, Button};
    MenuSample {
        buttons: [
            pad.button(Button::A),
            pad.button(Button::B),
            pad.button(Button::X),
            pad.button(Button::Y),
            pad.button(Button::LeftShoulder),
            pad.button(Button::RightShoulder),
        ],
        lx: pad.axis(Axis::LeftX),
        // SDL already reports +y as down, which is what the synthesizer expects.
        ly: pad.axis(Axis::LeftY),
        dpad: [
            pad.button(Button::DPadUp),
            pad.button(Button::DPadDown),
            pad.button(Button::DPadLeft),
            pad.button(Button::DPadRight),
        ],
    }
}

/// The controller chip's entry. Battery and rumble are reported absent rather than guessed:
/// this client reads neither here, and the actions that would use them
/// (`ConsoleCmd::PadAction`) are Android's `InputDevice` API.
fn pad_info(pad: &GameController, pref: punktfunk_core::config::GamepadPref) -> PadInfo {
    PadInfo {
        name: pad.name(),
        key: "0".into(),
        pref,
        steam_virtual: false,
        battery: None,
        detail: String::new(),
        forwarded: false,
        rumble: false,
    }
}

/// Window pixels to surface pixels. One is what SDL reports pointer events in, the other is
/// what the shell hit-tests and what Skia drew — and webOS is documented to hand back a
/// drawable that is not the window that was asked for. Both are 1080p on the sets seen so far,
/// which is exactly why a mismatch would be silent.
struct PointerScale {
    x: f32,
    y: f32,
}

impl PointerScale {
    fn at(&self, x: i32, y: i32) -> (f32, f32) {
        (x as f32 * self.x, y as f32 * self.y)
    }
}

fn pointer_scale(canvas: &sdl2::render::Canvas<sdl2::video::Window>) -> PointerScale {
    let (win_w, win_h) = canvas.window().size();
    let (draw_w, draw_h) = canvas.window().drawable_size();
    PointerScale {
        x: draw_w as f32 / win_w.max(1) as f32,
        y: draw_h as f32 / win_h.max(1) as f32,
    }
}

fn pointer_button(button: sdl2::mouse::MouseButton) -> Option<PointerButton> {
    match button {
        sdl2::mouse::MouseButton::Left => Some(PointerButton::Primary),
        // The console reads a secondary press as Back.
        sdl2::mouse::MouseButton::Right => Some(PointerButton::Secondary),
        _ => None,
    }
}

/// A remote or keyboard key as a menu move. The same vocabulary
/// `platform::webos::input::menu_event_for_key` maps for the classic menus, in the shell's terms
/// — including the Magic Remote's own Back keycode, which is not Escape or Backspace.
fn menu_event(k: sdl2::keyboard::Keycode) -> Option<MenuEvent> {
    use sdl2::keyboard::Keycode as K;
    Some(match k {
        K::Up => MenuEvent::Move(MenuDir::Up),
        K::Down => MenuEvent::Move(MenuDir::Down),
        K::Left => MenuEvent::Move(MenuDir::Left),
        K::Right => MenuEvent::Move(MenuDir::Right),
        K::Return | K::Return2 | K::KpEnter => MenuEvent::Confirm,
        K::Backspace | K::Escape | K::AcBack => MenuEvent::Back,
        K::Delete => MenuEvent::Secondary,
        K::PageUp => MenuEvent::JumpBack,
        K::PageDown => MenuEvent::JumpForward,
        k if k.into_i32() == crate::platform::webos::input::WEBOS_BACK_KEYCODE => MenuEvent::Back,
        _ => return None,
    })
}

/// The keys a text field wants while it is being edited.
fn editing_key(k: sdl2::keyboard::Keycode) -> Option<Key> {
    use sdl2::keyboard::Keycode as K;
    Some(match k {
        K::Left => Key::Left,
        K::Right => Key::Right,
        K::Up => Key::Up,
        K::Down => Key::Down,
        K::Return | K::Return2 | K::KpEnter => Key::Return,
        K::Space => Key::Space,
        K::Escape => Key::Escape,
        K::Backspace => Key::Backspace,
        K::Tab => Key::Tab,
        _ => return None,
    })
}
