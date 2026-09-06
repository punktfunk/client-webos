use super::overlay::{self, ConfirmAction, ConfirmDialog, Notification};
use super::*;
use crate::console::ConsoleGl;
use crate::core::settings::TvSettings;
use crate::services::store::ExitAction;
use crate::ui::render::Size;

/// Runs the UI (host list -> pairing -> settings) until the user confirms a
/// connect target or the system asks the app to close (`None`). A plain
/// function, not a closure — a closure capturing `canvas`/`events` by
/// reference would hold that borrow for as long as the closure value exists,
/// which conflicts with using them again in the streaming loop right after.
///
/// Draws on the console's GL context (`console::gl`), the same one the shell uses: every
/// screen draws itself on the kit (`app::draw`), the overlays too (`runtime::overlay`).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_ui_flow(
    canvas: &mut sdl2::render::Canvas<sdl2::video::Window>,
    gl: &mut Option<ConsoleGl>,
    events: &mut sdl2::EventPump,
    game_controller: &sdl2::GameControllerSubsystem,
    controller: &mut Option<GameController>,
    identity: &(String, String),
    display_mode: sdl2::video::DisplayMode,
    initial_status: Option<String>,
    initial_toast: Option<String>,
) -> Result<UiOutcome> {
    // Target period for this loop's render ticks, animating or not. Each active
    // (render) iteration used to sleep a flat 16ms *on top of* whatever the tick's own
    // work cost, so its real period was `work + 16ms` rather than 16ms — at a spinner
    // frame delay of ~40ms that was enough overshoot to occasionally miss a frame's window
    // and skip straight to the next one. Pacing off each tick's own start time keeps
    // the loop at a steady ~60Hz regardless of work cost, which comfortably samples
    // every 40ms spinner frame.
    const TICK_BUDGET: Duration = Duration::from_millis(16);
    canvas.window_mut().show();
    // Both menus need it now; without a GL context there is nothing to draw with.
    let gl = console_flow::bring_up(gl, canvas).context("menu: GL host")?;
    let kit_fonts = std::rc::Rc::new(pf_console_ui::theme::build_fonts().context("menu: kit fonts")?);
    tracing::info!(
        "menu fonts: {} (the kit's embedded faces)",
        kit_fonts
            .font(pf_console_ui::theme::W::Regular, 16.0)
            .typeface()
            .family_name()
    );
    let mut app = App::new(identity.clone(), kit_fonts.clone());
    // The kit widgets step on real time, like the shell's do.
    let mut last_frame = Instant::now();
    // Re-poll pad type (ControllerDeviceAdded fires once per connect, not per menu entry).
    app.set_gamepad_type(gamepad::detect_type(game_controller));
    // Seeded here for the same reason: the hotplug events fire once per connect, and this
    // entry may follow one. Refreshed on both arms below.
    let mut pad_connected = gamepad::any_pad_connected(game_controller);
    // Status from last connect attempt (sticky so reload progress doesn't erase it).
    if initial_status.is_some() {
        app.set_home_status(initial_status, true);
    }
    // Toast (same as the stream loop). Shown once as Home re-appears.
    let mut notif = Notification::new();
    if let Some(msg) = initial_toast {
        notif.show(msg);
    }
    // Rasterized-text cache (created once, threaded through every render call).
    // lifetime so repeat draws of the same (font, text, color) reuse an
    // already-rasterized+premultiplied `Pixmap` instead of re-rasterizing
    // freetype glyphs on every ~60fps tick.
    let mut input = UiInput::default();
    // Owned handle (it just clones the video subsystem's refcount), so taking it
    // here doesn't hold a borrow on `canvas` for the rest of the loop.
    let mut text_input = TextInputController::new(canvas.window().subsystem().text_input());
    tracing::info!(
        "on-screen keyboard support: {}",
        text_input.has_screen_keyboard_support()
    );
    // Redraw-on-change: outside a running animation (which the tick below asks
    // `App` about separately), pixels only ever change in reaction to an SDL
    // event or a discovery/art/library background result — anything else is a
    // no-op tick. Without this, `app.render(...)` (and the `canvas.present()`
    // vsync swap inside it) ran unconditionally every 16ms forever, even sitting
    // on an untouched menu. Starts `true` so the first frame always draws.
    let mut dirty = true;
    // Set once the reachability check passes — `spawn_connect` is already
    // running by then, so this just carries its handle out of the loop for
    // `run_inner` to join once the launch animation finishes.
    let mut connect_handle: Option<(
        std::thread::JoinHandle<Result<session::Connected>>,
        crate::app::ConnectTarget,
        store::Settings,
        bool,
    )> = None;
    // What the launch in flight is, for `services::recents` — captured where the target is
    // still in hand, spent only if the connect actually takes.
    let mut launched: Option<(String, u16, String)> = None;
    // Yellow button log overlay works here too (see streaming loop).
    let mut yellow_held = false;
    let mut home_held = false;
    let mut log_overlay_last: Option<Instant> = None;
    // The log tail's lines, refreshed on the ~2Hz cadence rather than read per frame.
    let mut log_lines: Option<Vec<String>> = None;
    // `quit_dialog_was_active` catches the close-fade's final frame so it gets one last
    // redraw-on-change tick to wipe the dialog off the menu.
    let mut quit_dialog = ConfirmDialog::new(
        "Quit app?",
        "Punktfunk will close and you'll return to the webOS home screen.",
        Some(crate::app::view::icons::ICON_CLOSE),
        "Quit",
        crate::app::screens::confirm::Tone::Danger,
    );
    let mut exit_held = false;
    // Controller routes to the quit dialog the same way it routes to the disconnect
    // dialog while streaming — see `DisconnectChord`.
    let mut chord = DisconnectChord::default();
    let mut quit_dialog_was_active = false;
    // One-shot: warm the modal text/shadow/freetype caches on the first idle tick
    // (Home already painted by then) so the first Settings/host-menu open doesn't
    // hitch on cold rasterization. Reset per menu entry — `text_cache` is too.
    'ui: loop {
        // Start of this tick, for the loop's own pacing against `TICK_BUDGET`.
        let frame_start = Instant::now();
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            tracing::warn!("SIGTERM/SIGINT received during UI");
            return Ok(quit(&app));
        }
        // Raw scancode poll (not SDL2 event); edge-detected like streaming loop.
        let yellow_down =
            crate::platform::webos::input::webos_scancode_down(crate::platform::webos::input::WEBOS_YELLOW_SCANCODE);
        if yellow_down && !yellow_held {
            cycle_log_overlay();
            dirty = true; // force an immediate redraw with the new state
            log_overlay_last = None;
        }
        yellow_held = yellow_down;
        // Home key re-opens the webOS launcher (captured, or webOS kills the app instead of
        // backgrounding it); works from any menu state, a long Back never trips it.
        if home_key_fired(&mut home_held) {
            crate::platform::webos::luna::launch_home();
        }
        // Long-press Back (root/held Back, surfaced as the EXIT gesture) quits
        // straight away from any menu state, no confirm — the short Back tap below
        // opens the dialog instead. Menu loop only; stream is unaffected.
        if exit_gesture_fired(&mut exit_held) {
            tracing::info!("EXIT gesture — quitting app");
            return Ok(quit(&app));
        }
        // Controller quit shortcut: held long enough on Home,
        // then forgotten so it fires once per hold rather than repeatedly while held.
        if !quit_dialog.is_open() && matches!(app.nav.screen, Screen::Home) && chord.held_for(EXIT_HOLD) {
            tracing::info!("quit shortcut held — opening quit dialog");
            chord.clear();
            open_quit_dialog(&mut quit_dialog, &mut input, &app);
            dirty = true;
        }
        // The flip, watched rather than signalled: the Settings row writes the switch, and a
        // pad arriving satisfies the default mode without anyone writing anything. `app` drops
        // on return, and its `StateWriter` flushes and joins in `Drop` — so the value is on
        // disk before the menu loop re-reads it.
        // `CONSOLE_UI_BUILT` first: off the TV target `console_flow::wanted` is const-false, so
        // handing the menu over there would bounce straight back here and spin the menu loop.
        if crate::app::menu::CONSOLE_UI_BUILT && app.settings_ui.settings.gamepad_ui_active(pad_connected) {
            tracing::info!("controller UI applies — handing the menu over");
            app.persist();
            text_input.stop();
            return Ok(UiOutcome::Reenter);
        }
        // Held D-pad/stick autorepeat (see `NAV_REPEAT_DELAY`) — the pad's stand-in for the
        // OS key repeat the remote and a keyboard get. Skipped while a launch is in flight,
        // like every other menu dispatch; the quit dialog ends the hold when it opens.
        if app.launch_anim.is_none() {
            if let Some(ev) = input.nav_repeat_due() {
                dirty = true;
                match dispatch_menu_event(&mut app, ev, display_mode) {
                    EventAction::Next => {}
                    EventAction::Launch => break 'ui,
                }
            }
        }
        dirty |= app.drain_jobs();
        dirty |= app.tick_screens();
        // Fire on hold elapsed, not release, so user sees it before letting go.
        if let Some(hold) = input
            .card_held
            .as_mut()
            .filter(|h| !h.fired && h.since.elapsed() >= CARD_HOLD)
        {
            hold.fired = true;
            let still_there = matches!(app.nav.screen, Screen::Home) && hold.focus == app.home_focus;
            if still_there {
                // The hold's whole effect. It no longer pins — pinning is one of the two
                // rows the menu it raises offers, and the only way to reach it.
                app.open_card_menu(display_mode.w as u32);
            }
            dirty = true;
        }
        // Start connect in parallel with launch anim (fast handshake finishes first).
        if app.launch_ready.is_some() && connect_handle.is_none() {
            app.launch_anim = Some(Instant::now());
            dirty = true;
            if let Some(target) = app.take_ready_launch() {
                // In-memory settings, not `store::load_settings()`: a just-flipped
                // toggle (e.g. audio offload) is persisted asynchronously by
                // `StateWriter`, so re-reading disk here could race the write and
                // connect with the stale value. `app.settings_ui.settings` is updated synchronously.
                // Per-game overrides merge over the global document here — the single
                // point where the game being launched is known and the settings copy that
                // rides the whole session is made. Clamped to caps like any global value.
                launched = Some((
                    target.host.clone(),
                    target.port,
                    target
                        .launch
                        .clone()
                        .unwrap_or_else(|| store::DESKTOP_PIN_ID.to_string()),
                ));
                let mut settings = app.launch_settings(&target);
                let gamepad_auto = settings.gamepad_type() == store::GamepadType::Auto;
                settings = resolve_gamepad_type(settings, game_controller);
                let handle = spawn_connect(identity.clone(), target.clone(), settings.clone())?;
                connect_handle = Some((handle, target, settings, gamepad_auto));
            }
        }
        // Without a hero the screen is handed to the streaming loop at the end of the
        // launch fade, as it always was. With one, this loop keeps animating it as the
        // loading screen until `Hero::handover_ready` is satisfied — the handshake having
        // landed (`run_inner`'s join then returns immediately), the decoder presenting (so
        // its reveal wait is satisfied too), and the hold and fade-out done.
        if let Some(t) = app.launch_anim {
            // `is_finished` is monotonic, so this needs no latch of its own.
            let connect = match connect_handle.as_ref() {
                _ if CONNECT_FAILED.load(Ordering::Relaxed) => Connect::Failed,
                Some((h, ..)) if !h.is_finished() => Connect::Pending,
                _ => Connect::Done,
            };
            // `presented`, not `presenting`: the hero's exit crossfades into the video plane,
            // and a frame merely accepted by NDL is still behind its present cushion — fading
            // out on that lands the dissolve on black (see `ndl::FIRST_PICTURE_HOLD`).
            let presented = crate::platform::webos::ndl::presented();
            if app.render.hero.handover_ready(t.elapsed(), connect, presented) {
                // Where the launch actually takes — not `confirm_grid_card`, which also fires
                // for one that bounces into the Wake dialog or fails to pair. A failed launch
                // must not reorder Library.
                if !matches!(connect, Connect::Failed) {
                    if let Some((host, port, id)) = launched.take() {
                        app.recents.record(&host, port, &id);
                    }
                }
                break 'ui;
            }
        }
        for event in events.poll_iter() {
            use sdl2::event::Event;
            // Dropped in the menu too, or a thumb resting on a pad's touchpad hovers rows and
            // clicks them — see `mouse::is_touch_emulated`.
            if mouse::is_touch_emulated(&event) {
                continue;
            }
            // Launch committed: the menu is behind the loading screen and its input would
            // move a grid the user can no longer see. Only shutdown still counts.
            if app.launch_anim.is_some() {
                if matches!(event, Event::Quit { .. }) {
                    tracing::info!("quit during launch");
                    return Ok(quit(&app));
                }
                continue;
            }
            // Device-level events, handled before anything screen-specific:
            // shutdown and controller hotplug.
            match event {
                Event::Quit { .. } => {
                    tracing::info!("quit during UI");
                    return Ok(quit(&app));
                }
                Event::ControllerDeviceAdded { which, .. } => {
                    pad_connected = gamepad::any_pad_connected(game_controller);
                    if controller.is_none() {
                        match game_controller.open(which) {
                            Ok(c) => {
                                tracing::info!("controller connected: {}", c.name());
                                *controller = Some(c);
                            }
                            Err(e) => tracing::warn!("controller open failed: {e}"),
                        }
                    }
                    // Outside the open: only the first pad becomes `controller`, but a second one
                    // plugged in after it can still be the pad `detect_type` names.
                    app.set_gamepad_type(gamepad::detect_type(game_controller));
                    continue;
                }
                Event::ControllerDeviceRemoved { .. } => {
                    pad_connected = gamepad::any_pad_connected(game_controller);
                    *controller = None;
                    // Re-poll rather than clearing: another pad may still be attached.
                    app.set_gamepad_type(gamepad::detect_type(game_controller));
                    // An unplugged pad sends no releases — drop any armed chord, and the
                    // held direction it can no longer let go of.
                    chord.clear();
                    input.clear_nav_repeat();
                    continue;
                }
                _ => {}
            }
            // Track chord state for the quit shortcut without consuming the event — the
            // buttons still flow through `handle_ui_event` for normal menu navigation.
            match event {
                Event::ControllerButtonDown { button, .. } => chord.set(button, true),
                Event::ControllerButtonUp { button, .. } => chord.set(button, false),
                _ => {}
            }
            // The quit dialog owns input while open — navigate it only, don't let the
            // event reach the menu underneath (same split as the streaming loop).
            if quit_dialog.is_open() {
                match quit_dialog.handle_event(&event, &kit_fonts, display_mode.w as u32, display_mode.h as u32) {
                    Some(ConfirmAction::Confirmed) => {
                        tracing::info!("quit confirmed from menu");
                        return Ok(quit(&app));
                    }
                    Some(_) => dirty = true,
                    None => {}
                }
                continue;
            }
            // Short Back tap on Home with sidebar focus opens the quit dialog. From a
            // game card / the ⋯ column, Back first steps focus back to the sidebar
            // (see `App::back`), so it falls through to normal dispatch instead.
            if matches!(app.nav.screen, Screen::Home)
                && matches!(app.home_focus, HomeFocus::Sidebar(_))
                && is_menu_press(&event, MenuEvent::Back, false)
            {
                tracing::info!("Back tap on Home sidebar — opening quit dialog");
                open_quit_dialog(&mut quit_dialog, &mut input, &app);
                dirty = true;
                continue;
            }
            match handle_ui_event(&mut app, event, &mut input, display_mode, &mut dirty) {
                EventAction::Next => {}
                EventAction::Launch => break 'ui,
            }
        }
        // Toggle text input off screen state — a no-op unless it actually changed.
        let wants_text = text_input_screen(app.nav.screen);
        // Track actual keyboard state (user can dismiss while field focused; moves card).
        // Only worth polling while a text screen is up or the panel is still closing.
        if wants_text || app.keyboard_shown {
            let keyboard_shown = text_input.is_shown(canvas.window());
            if keyboard_shown != app.keyboard_shown {
                app.set_keyboard_shown(keyboard_shown);
                dirty = true;
                tracing::debug!("on-screen keyboard shown: {keyboard_shown}");
            }
        }
        let rect = wants_text.then(|| {
            app.address_field_rect(display_mode.w as u32, display_mode.h as u32)
                .map(|r| sdl2::rect::Rect::new(r.x(), r.y(), r.width(), r.height()))
        });
        text_input.set_active(wants_text, rect.flatten());
        // Five reasons to render: dirty, animations running, tiles pending,
        // spinner animating, or log overlay due for refresh (~2Hz).
        // 16ms sleep when none holds keeps SoC idle.
        // The quit dialog runs its own open/close fade and focus-pop, so keep ticking
        // while it (or its close-fade) is on screen, and force one redraw on the frame it
        // finally clears so it doesn't linger over the menu.
        let quit_dialog_active = quit_dialog.frame().is_some();
        if quit_dialog_was_active && !quit_dialog_active {
            dirty = true;
        }
        quit_dialog_was_active = quit_dialog_active;
        // Anything the state machine queued this tick (`App::toast`) — a card move, so far.
        if let Some(msg) = app.take_toast() {
            notif.show(msg);
        }
        // Polled every tick like the streaming loop's toast, not gated behind
        // `content_dirty` — its own fade needs frames regardless of anything else.
        let notif_frame = notif.frame().map(|(t, a)| (t.to_string(), a));
        // The dip has played out (see `App::press`) — the tile springs back.
        if app.poll_press() {
            dirty = true;
        }
        let animating = app.tick_animations()
            || !app.render.grid.reveal.is_revealed()
            || quit_dialog_active
            || notif_frame.is_some();
        let log_overlay_due = log_overlay_state() != LogOverlayState::Off
            && log_overlay_last.is_none_or(|t| t.elapsed() >= Duration::from_millis(500));
        if !dirty && !animating && !log_overlay_due {
            // Blocked on the event queue rather than asleep for the rest of the budget:
            // nothing on this branch is animating, so the next thing that can change a pixel
            // is an SDL event, and waiting for it both wakes the SoC less often and drops the
            // up-to-16ms delay a plain sleep put between a keypress and the poll that sees it.
            // The timeout keeps the loop's own polling (discovery, art, reachability) on the
            // same cadence it had.
            let elapsed = frame_start.elapsed();
            if elapsed < TICK_BUDGET {
                crate::platform::webos::input::wait_for_event(TICK_BUDGET - elapsed);
            }
            continue;
        }
        dirty = false;
        // Advance per-tick app state (card size, modal fades) exactly once before drawing.
        app.advance_frame(display_mode.w as u32);
        app.prepare_frame(Size::new(display_mode.w as u32, display_mode.h as u32));
        // The log tail's text is read on the 500 ms cadence; between reads the last lines
        // are drawn as they were, so an animating menu does not lock the log per frame.
        if log_overlay_due {
            log_overlay_last = Some(Instant::now());
            log_lines = log_overlay_lines();
        } else if log_overlay_state() == LogOverlayState::Off {
            log_lines = None;
        }
        // The frame, on the GL context. Layout is in `display_mode` units; the drawable is
        // documented to differ from the window on webOS (handoff trap 10), so it is scaled
        // rather than assumed equal.
        let (dw, dh) = canvas.window().drawable_size();
        {
            let surface = gl.surface(dw, dh)?;
            let c = surface.canvas();
            c.clear(app.frame_clear_color());
            c.reset_matrix();
            c.scale((
                dw as f32 / display_mode.w.max(1) as f32,
                dh as f32 / display_mode.h.max(1) as f32,
            ));
            // Home, the modals, the launch transition, then the overlays (`app::draw`,
            // `runtime::overlay`).
            app.apply_ink();
            kit_fonts.begin_frame();
            let dt = last_frame.elapsed().as_secs_f64().min(0.1);
            last_frame = Instant::now();
            let frame = crate::app::draw::Frame::new(c, &kit_fonts, display_mode.w as u32, display_mode.h as u32);
            app.draw_home(&frame, dt);
            app.draw_modals(&frame, dt);
            app.draw_launch(&frame);
            if let Some(lines) = &log_lines {
                overlay::log(&frame, lines, 1.0);
            }
            if let Some((text, alpha)) = &notif_frame {
                overlay::toast(&frame, text, *alpha);
            }
            quit_dialog.draw(&frame);
        }
        gl.flush();
        canvas.window().gl_swap_window();
        let elapsed = frame_start.elapsed();
        if elapsed < TICK_BUDGET {
            std::thread::sleep(TICK_BUDGET - elapsed);
        }
    }
    text_input.stop();
    // Atlases go back before a stream takes the GPU; the context and its compiled shaders
    // stay, so the next entry is not a cold start.
    gl.release_resources();
    Ok(match connect_handle {
        Some((handle, target, settings, gamepad_auto)) => UiOutcome::Launch(Box::new(ConnectOutcome {
            handle,
            target,
            settings,
            gamepad_auto,
            first_frame_deadline: app.render.hero.first_frame_deadline(),
            // Carried into the stream so a Quit from there honours it too — that path never
            // comes back through this loop.
            exit_plan: app.exit_plan(),
        })),
        None => quit(&app),
    })
}

/// Raises the quit dialog, focused on Cancel. Every path in goes through here so none can
/// forget the hold it has to end: the releases of whatever was held when it opened go to the
/// dialog, so a repeat left armed would keep stepping the menu underneath once it closes.
fn open_quit_dialog(dialog: &mut ConfirmDialog, input: &mut UiInput, app: &App) {
    input.clear_nav_repeat();
    dialog.open_with(1, quit_subtitle(app));
}

/// What the quit dialog says Quit will do, which is whatever the exit action would actually
/// send — `None` (and so the plain wording) for a host that is unreachable or has no behaviour
/// set, because that is exactly when nothing is sent.
fn quit_subtitle(app: &App) -> &'static str {
    let action = app.exit_plan().map_or(ExitAction::None, |plan| plan.action);
    crate::core::errors::quit_subtitle(action)
}

/// The tail every way of quitting out of the menu shares: hand the selected host's exit action
/// up, unfired. Nothing runs it here — `run_inner` owns the one place it may, which is past
/// every session teardown.
fn quit(app: &App) -> UiOutcome {
    UiOutcome::Quit(app.exit_plan())
}
