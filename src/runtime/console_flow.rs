//! The shared gamepad shell, this client's menu. It hands a launch or a quit back to the
//! streaming loop as a [`UiOutcome`], and reloads the settings document on every entry.
//!
//! The shell renders through its own GL context on the app's window. See `console::gl`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use pf_client_core::console::{OverlayAction, PointerButton, PointerInput, SessionPhase};
use pf_client_core::menu_nav::{MenuEvent, MenuNav, MenuSample, PadInfo};
use pf_console_ui::{Console, ConsoleEntry, ConsoleHandles, ConsoleOptions, InputSource, Key, Platform, Viewport};

use super::*;
use crate::console::Service;
use crate::core::perf::{ArtSnapshot, Perf};
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

/// Run the shell until it commits a launch or asks to leave.
pub(super) fn run(
    canvas: &mut sdl3::render::Canvas<sdl3::video::Window>,
    gl: &mut Option<ConsoleGl>,
    events: &mut sdl3::EventPump,
    game_controller: &sdl3::GamepadSubsystem,
    open_pads: &mut pads::Pads,
    identity: &(String, String),
    // Why the last stream bounced back here, if it did.
    notice: Option<String>,
) -> Result<UiOutcome> {
    let state = store::load();
    let writer = Arc::new(StateWriter::spawn(state.clone()));
    let store = Arc::new(ConsoleStore::new(state, writer));
    let handles = ConsoleHandles::new();
    let mut service = Service::new(handles.clone(), store.clone(), identity.clone());

    let console_gl = bring_up(gl, canvas, true).context("console: GL host")?;

    let opts = ConsoleOptions {
        device_name: "webOS TV".into(),
        deck: false,
        tv: true,
        // The shell is the only UI: no "Controller-optimized UI" row to turn it off.
        fallback_ui: false,
        // NDL decodes H.264 and HEVC only; the Hello never offers PyroWave or AV1.
        pyrowave_ok: false,
        av1_ok: false,
        store: Some(store.clone()),
        platform: Platform::WebOS,
        gpu_cache_bytes: crate::console::GPU_CACHE_BYTES,
        screen: None,
    };
    // Land back on the shelf the last stream was launched from (`selected_host`). The fetch is seeded here because the shell only asks
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
    let mut console = Console::new(opts, entry, &handles).context("console: shell")?;
    // Every tab drawn once, unseen, so the GL driver compiles their programs now rather than
    // stalling 50–170 ms on the first visit to each. Each frame is flushed and waited out
    // behind the splash: one flush for the whole tour allocates every frame's textures at once.
    {
        let warm = Instant::now();
        let (w, h) = canvas.window().size_in_pixels();
        if let Ok(surface) = console_gl.surface(w, h) {
            console.warm_up(surface.canvas(), &Viewport::plain(w, h), |canvas| {
                if let Some(mut gpu) = canvas.direct_context() {
                    gpu.flush_submit_and_sync_cpu();
                }
            });
        }
        tracing::info!("console: warmed the tabs in {} ms", warm.elapsed().as_millis());
    }
    if let Some(notice) = notice {
        handles.console.set_notice(notice);
    }

    // What this panel costs, for the log "Send logs to host" carries — see `core::perf`.
    // Labelled by where the shell came up: entering on a shelf is the case that loads art, and
    // the case the CX reports as slow.
    let mut perf = Perf::new();
    perf.mark(if opened_on_a_shelf { "library" } else { "home" });
    let mut nav = MenuNav::new();
    let mut sample = MenuSample::default();
    let mut pads: Vec<PadInfo> = Vec::new();
    // The open pad's name, or `None` when what SDL has open is the Magic Remote. Refreshed on
    // hotplug only to avoid per-frame string allocations.
    open_pads.sync(game_controller);
    let mut pad_name: Option<String> = open_pads.first_name();
    // What the shell was told last frame, so `pads` is rebuilt only when it changes.
    // `None` means "not built yet": an inner `None` is a real answer (no pad), so a plain
    // `Option` could not tell the two apart and left a removed pad's legend standing.
    let mut last_pref: Option<Option<punktfunk_core::config::GamepadPref>> = None;
    let mut menu_out: Vec<MenuEvent> = Vec::new();
    let mut last_input = Instant::now();
    // Seeded from live key state, as the stream loop does: these are rising-edge polls, and the
    // console is often entered BY a held Back — the EXIT gesture that left HDR calibration or
    // cancelled a reconnect. Seeded `false`, its still-down key read as a fresh press on the
    // first tick and quit the app.
    let mut home_held =
        crate::platform::webos::input::webos_scancode_down(crate::platform::webos::input::WEBOS_HOME_SCANCODE);
    let mut exit_held =
        crate::platform::webos::input::webos_scancode_down(crate::platform::webos::input::WEBOS_EXIT_SCANCODE);
    // The one resolver for the remote's own keys in this loop — see `RemoteKeys`.
    let mut remote_keys = crate::platform::webos::input::RemoteKeys::default();
    // A launch the shell committed: the connect runs while the shell keeps drawing its
    // Connecting card.
    let mut connect: Option<Started> = None;
    // A Request access launch waiting on the host's approval — see `Service::request_access`.
    let mut access: Option<Launch> = None;

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
        // A held/root-level Back arrives as the discrete EXIT key, not as a long Back.
        if exit_gesture_fired(&mut exit_held) {
            tracing::info!("console: EXIT gesture — quitting app");
            break 'ui UiOutcome::Quit(exit_plan(&service, identity));
        }
        // webOS is documented to hand back a drawable that is not the window it was asked for
        // (handoff trap 10), while SDL reports pointer events in WINDOW coordinates. The shell
        // hit-tests in surface pixels, so the two have to be reconciled before it sees them.
        let scale = pointer_scale(canvas);

        for event in events.poll_iter() {
            use sdl3::event::Event;
            // Resolved once, here, and read just below. Never re-derive it.
            let remote = remote_keys.press(&event);
            // A thumb resting on a pad's touchpad must not hover and click rows.
            if crate::platform::webos::mouse::is_touch_emulated(&event) {
                continue;
            }
            // The Magic Remote's own keys, ahead of the match because SDL3 gives them neither a
            // scancode nor a keycode — the keycode arm would never see them (see
            // `platform::webos::input::RemoteKeys`). Read off the answer `remote_keys` already
            // gave, not a second look at the event: an OS auto-repeat resolves to `None` here
            // and is left to arms that cannot match a keycode-less key anyway.
            if let Some(key) = remote {
                use crate::platform::webos::input::RemoteKey;
                last_input = Instant::now();
                let ev = match key {
                    RemoteKey::Back => MenuEvent::Back,
                    // Green and Red make the shell's Secondary/Tertiary REACHABLE from a
                    // Magic Remote at all: the library screen puts Collections on Secondary
                    // and Options — the whole host menu — on Tertiary, and unlike the home
                    // screen it has no d-pad fallback.
                    RemoteKey::Red => MenuEvent::Secondary,
                    RemoteKey::Green => MenuEvent::Tertiary,
                    // HDR calibration has no row in the shared shell, so Blue opens it.
                    RemoteKey::Blue if !console.editing() => {
                        if crate::core::caps::video_caps().hdr {
                            tracing::info!("console: opening HDR calibration");
                            break 'ui UiOutcome::Calibrate(exit_plan(&service, identity));
                        }
                        handles
                            .console
                            .set_notice("HDR calibration needs an HDR-capable TV.".to_string());
                        continue;
                    }
                    _ => continue,
                };
                console.menu(ev, InputSource::Keys);
                continue;
            }
            match event {
                Event::Quit { .. } => {
                    tracing::info!("quit during the console");
                    break 'ui UiOutcome::Quit(exit_plan(&service, identity));
                }
                Event::GamepadAdded { which, .. } => {
                    if open_pads.add(game_controller, which).is_some() {
                        pad_name = open_pads.first_name();
                        // The legend describes the handle, so it is rebuilt with it.
                        last_pref = None;
                    }
                }
                Event::GamepadRemoved { which, .. } => {
                    // Only pads we hold: webOS enumerates the Magic Remote as a controller and it
                    // drops constantly, and clearing on its removal took the real pad's input
                    // away with it.
                    if open_pads.remove(which).is_some() {
                        // An unplugged pad sends no releases: drop what the synthesizer holds.
                        nav.reset();
                        sample = MenuSample::default();
                        pad_name = open_pads.first_name();
                        last_pref = None;
                    }
                }
                Event::KeyDown {
                    keycode: Some(k),
                    repeat,
                    keymod,
                    ..
                } => {
                    last_input = Instant::now();
                    let shift = keymod.intersects(sdl3::keyboard::Mod::LSHIFTMOD | sdl3::keyboard::Mod::RSHIFTMOD);
                    // OK acts on release; held, it opens the focused card's menu. In a field it
                    // presses the on-screen key: as a keyboard Enter it would close the field.
                    if is_ok(k) {
                        if !repeat {
                            console.ok(true, InputSource::Keys);
                        }
                        continue;
                    }
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
                Event::KeyUp { keycode: Some(k), .. } if is_ok(k) => {
                    last_input = Instant::now();
                    console.ok(false, InputSource::Keys);
                }
                Event::TextInput { text, .. } => {
                    last_input = Instant::now();
                    console.text(&text);
                }
                Event::MouseMotion { x, y, .. } => {
                    last_input = Instant::now();
                    let (x, y) = scale.at(x, y);
                    console.pointer(PointerInput::Move { x, y });
                }
                Event::MouseButtonDown { x, y, mouse_btn, .. } => {
                    last_input = Instant::now();
                    if let Some(button) = pointer_button(mouse_btn) {
                        let (x, y) = scale.at(x, y);
                        console.pointer(PointerInput::Down {
                            x,
                            y,
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
                        let (x, y) = scale.at(x, y);
                        console.pointer(PointerInput::Up { x, y, button });
                    }
                }
                Event::MouseWheel {
                    y: dy,
                    mouse_x,
                    mouse_y,
                    ..
                } => {
                    last_input = Instant::now();
                    let (x, y) = scale.at(mouse_x, mouse_y);
                    console.pointer(PointerInput::Wheel { x, y, dy });
                }
                _ => {}
            }
        }

        // Every open pad through the shared synthesizer, so repeats, dead zone and hysteresis
        // match every other client rather than being re-invented here. The remote is never
        // opened; its buttons still reach the shell, as keys.
        if !open_pads.is_empty() {
            sample = merge_samples(open_pads.iter().map(pad_sample));
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
                    preset: profile,
                    request_access,
                } => {
                    let want = Launch {
                        addr,
                        port,
                        fp_hex,
                        launch,
                        profile,
                    };
                    if request_access {
                        // The host parks this TV until its operator approves; the launch
                        // follows once the knock comes back and the pairing is saved.
                        let Some(pin) = shared::parse_fp(&want.fp_hex) else {
                            console.session_phase(SessionPhase::Failed(
                                "This host didn't say who it is, so pair it with its PIN.",
                            ));
                            continue;
                        };
                        tracing::info!("console: requesting access to {}:{}", want.addr, want.port);
                        service.request_access(want.addr.clone(), want.port, pin);
                        access = Some(want);
                        continue;
                    }
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
                    if connect.take().is_some() || access.take().is_some() {
                        tracing::info!("console: connect cancelled");
                        service.cancel_access();
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

        // Approved: the host is paired now, so the launch the knock held back goes ahead.
        if access.is_some() {
            match service.drain_access() {
                Some(Ok(())) => {
                    let want = access.take().expect("just checked");
                    match start_launch(&store, identity, game_controller, want) {
                        Ok(started) => connect = Some(started),
                        Err(e) => console.session_phase(SessionPhase::Failed(&format!("Couldn't start — {e}"))),
                    }
                }
                Some(Err(e)) => {
                    access = None;
                    console.session_phase(SessionPhase::Failed(&e));
                }
                None => {}
            }
        }

        // The handshake landed (or failed): the streaming loop takes it from here, and a
        // failure comes back here with the reason.
        if connect.as_ref().is_some_and(|(h, ..)| h.is_finished()) {
            let (handle, target, settings, gamepad_auto) = connect.take().expect("just checked");
            break 'ui UiOutcome::Launch(Box::new(ConnectOutcome {
                handle,
                target,
                settings,
                gamepad_auto,
                exit_plan: exit_plan(&service, identity),
            }));
        }

        // Draw.
        let mut idled = Duration::ZERO;
        if last_input.elapsed() >= IDLE_AFTER {
            std::thread::sleep(IDLE_FRAME_STEP);
            idled = IDLE_FRAME_STEP;
        }
        let (w, h) = canvas.window().size_in_pixels();
        // Read in place: a whole-document `snapshot` clone per frame allocates every known host.
        let stored_kind = store.with(|s| s.settings.gamepad_type());
        // 🛑 `None` unless a real pad is open, not the stored preference: this picks the GLYPH
        // LEGEND, and claiming a pad prints button marks for buttons that are not in the room.
        // It is also what the home screen reads to put Options and Settings on the d-pad
        // instead of on Y and X (`pads.is_empty()`).
        let pad_pref = pad_name.as_ref().map(|_| {
            let kind = if stored_kind == store::GamepadType::Auto {
                open_pads.detected_type().unwrap_or(stored_kind)
            } else {
                stored_kind
            };
            kind.to_core()
        });
        // Rebuilt only when the legend it prints changes: every field but `pref` is fixed for
        // the life of the handle, and building it allocated four strings a frame.
        if last_pref != Some(pad_pref) {
            last_pref = Some(pad_pref);
            pads.clear();
            if pad_pref.is_some() {
                pads.extend(open_pads.pad_infos(stored_kind));
            }
        }
        let label = pad_name.as_deref();
        {
            let surface = console_gl.surface(w, h)?;
            console.frame(
                surface.canvas(),
                // No insets: webOS hands a native app a clean 1080p surface with no overscan
                // margin to keep chrome out of.
                //
                // No panel-size correction either: the kit's default scale is `height / 800`,
                // and every shell screen is laid out to fill that 800-unit box.
                &Viewport::plain(w, h),
                label,
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

/// Bring up (or reuse) the shell's GL context and make it current — for the console and for
/// every overlay frame (`overlay::frame`), which share the one context.
/// `vsync` blocks the swap on the panel: true for a menu, whose loop has nothing else to do,
/// false over live video — see [`ConsoleGl::set_swap_interval`].
pub(super) fn bring_up<'a>(
    gl: &'a mut Option<ConsoleGl>,
    canvas: &sdl3::render::Canvas<sdl3::video::Window>,
    vsync: bool,
) -> Result<&'a mut ConsoleGl> {
    if gl.is_none() {
        // The first entry of the process pays for the context and the shader warm-up; every
        // later one reuses both (see `ConsoleGl::ctx`).
        *gl = Some(ConsoleGl::new(canvas.window(), canvas.window().subsystem())?);
    }
    let gl = gl.as_mut().expect("just built");
    // The stream overlays may have left the context current on another surface state.
    gl.make_current(canvas.window())?;
    // After `make_current`, since the interval belongs to the shared window surface.
    gl.set_swap_interval(canvas.window().subsystem(), vsync);
    Ok(gl)
}

/// What the shell committed to launch: the host, a title or the desktop, and a one-off profile.
struct Launch {
    addr: String,
    port: u16,
    fp_hex: String,
    launch: Option<String>,
    profile: Option<String>,
}

/// A connect in flight: its thread, what it dialled, the session settings, and whether the
/// pad kind was `Automatic`.
type Started = (PendingConnect, ConnectTarget, store::Settings, bool);

/// Start the connect for a launch the shell committed.
fn start_launch(
    store: &Arc<ConsoleStore>,
    identity: &(String, String),
    game_controller: &sdl3::GamepadSubsystem,
    want: Launch,
) -> Result<Started> {
    let Launch {
        addr,
        port,
        fp_hex,
        launch,
        profile,
    } = want;
    let state = store.snapshot();
    let known = state.known_hosts.iter().find(|h| h.addr == addr && h.port == port);
    let delivery = known.and_then(|h| h.delivery);
    let fingerprint = known
        .and_then(crate::core::model::KnownHost::fingerprint)
        .or_else(|| shared::parse_fp(&fp_hex))
        .context("that host isn't paired with this TV yet")?;
    let mut settings = shared::launch_settings(&state, &addr, port, launch.as_deref(), profile.as_deref());
    let gamepad_auto = settings.gamepad_type() == store::GamepadType::Auto;
    settings = resolve_gamepad_type(settings, game_controller);
    let target = ConnectTarget {
        host: addr,
        port,
        fingerprint,
        launch,
        delivery,
    };
    let handle = spawn_connect(identity.clone(), target.clone(), settings.clone())?;
    Ok((handle, target, settings, gamepad_auto))
}

/// What to do to the selected host on the way out: the selected host only, and only one that answered its last reachability check — a host already down
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
fn pad_sample(slot: &pads::Slot) -> MenuSample {
    use sdl3::gamepad::{Axis, Button};
    let pad = &slot.pad;
    let [a, b, x, y] = slot.face.map(|button| pad.button(button));
    MenuSample {
        buttons: [
            a,
            b,
            x,
            y,
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

/// Every open pad folded into the one sample the synthesizer steps: buttons and d-pad OR'd,
/// the stick from whichever is furthest off centre. Two pads pushing at once read as one hand
/// instead of cancelling, and one `MenuNav` keeps one repeat clock.
fn merge_samples(samples: impl Iterator<Item = MenuSample>) -> MenuSample {
    let mut out = MenuSample::default();
    let mut best = -1i32;
    for s in samples {
        for (o, b) in out.buttons.iter_mut().zip(s.buttons) {
            *o |= b;
        }
        for (o, b) in out.dpad.iter_mut().zip(s.dpad) {
            *o |= b;
        }
        let mag = i32::from(s.lx).pow(2) + i32::from(s.ly).pow(2);
        if mag > best {
            best = mag;
            out.lx = s.lx;
            out.ly = s.ly;
        }
    }
    out
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
    /// SDL3 reports the pointer in f32 window pixels; the shell hit-tests in surface pixels.
    fn at(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.x, y * self.y)
    }
}

fn pointer_scale(canvas: &sdl3::render::Canvas<sdl3::video::Window>) -> PointerScale {
    let (win_w, win_h) = canvas.window().size();
    let (draw_w, draw_h) = canvas.window().size_in_pixels();
    PointerScale {
        x: draw_w as f32 / win_w.max(1) as f32,
        y: draw_h as f32 / win_h.max(1) as f32,
    }
}

fn pointer_button(button: sdl3::mouse::MouseButton) -> Option<PointerButton> {
    match button {
        sdl3::mouse::MouseButton::Left => Some(PointerButton::Primary),
        // The console reads a secondary press as Back.
        sdl3::mouse::MouseButton::Right => Some(PointerButton::Secondary),
        _ => None,
    }
}

/// A remote or keyboard key as a menu move: the stream overlays' keys (`input::menu_event_for_key`)
/// plus the shell's page jumps. OK never gets here — [`is_ok`] takes it first. The Magic Remote's
/// own Back is NOT here either: it has no `Keycode` rust-sdl3 can name, so the arm above matches
/// it — with the colour keys — on the key event's `raw` evdev code instead.
fn menu_event(k: sdl3::keyboard::Keycode) -> Option<MenuEvent> {
    use sdl3::keyboard::Keycode as K;
    match k {
        K::PageUp => Some(MenuEvent::JumpBack),
        K::PageDown => Some(MenuEvent::JumpForward),
        _ => crate::platform::webos::input::menu_event_for_key(k),
    }
}

/// The remote's OK and a keyboard's Enter: [`Console::ok`] takes both edges, never `menu_event`.
fn is_ok(k: sdl3::keyboard::Keycode) -> bool {
    use sdl3::keyboard::Keycode as K;
    matches!(k, K::Return | K::Return2 | K::KpEnter)
}

/// The keys a text field wants while it is being edited. OK is not one: see [`is_ok`].
fn editing_key(k: sdl3::keyboard::Keycode) -> Option<Key> {
    use sdl3::keyboard::Keycode as K;
    Some(match k {
        K::Left => Key::Left,
        K::Right => Key::Right,
        K::Up => Key::Up,
        K::Down => Key::Down,
        K::Space => Key::Space,
        K::Escape => Key::Escape,
        K::Backspace => Key::Backspace,
        K::Tab => Key::Tab,
        _ => return None,
    })
}
