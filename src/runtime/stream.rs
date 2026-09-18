use super::overlay::{self, ConfirmAction, ConfirmDialog};
use super::*;
use crate::core::dial::PadRoute;
use crate::core::settings::TvSettings;
use crate::platform::webos::device;
use crate::platform::webos::input::{
    webos_scancode_down as key_down, WEBOS_BLUE_KEYCODE, WEBOS_EXIT_SCANCODE, WEBOS_GREEN_SCANCODE,
    WEBOS_HOME_SCANCODE, WEBOS_YELLOW_SCANCODE,
};
use pf_client_core::ring::{RingCommand, RingFacts, RingInput};
use punktfunk_core::hud::{self, Extra, HudLine, Role, StatsSnapshot, StatsVerbosity};
use std::sync::Arc;

/// One frame of the dial's animation. The loop runs every 2 ms; the ring needs no more than 60 Hz.
const RING_FRAME: Duration = Duration::from_millis(16);
/// A synthetic system-button tap holds this long, so the host sees the press.
const TAP_PRESS: Duration = Duration::from_millis(50);
/// How long the finished launch frame is held waiting for the first frame to reach the decoder
/// before uncovering the video plane regardless. `None` only when the loading screen never
/// started this budget — it waits on the same signal (`app::hero::handover_ready`) and hands
/// over the deadline it was already running, so the two screens share one budget rather than
/// spending it twice in a row.
fn reveal_deadline(started: Option<Instant>) -> Instant {
    started.unwrap_or_else(|| Instant::now() + crate::app::hero::FIRST_FRAME_WAIT)
}

/// How many times a lost link is dialled again before the menu, and the pause before each
/// dial. The host lingers a dropped session for a reconnect; a link that is still down
/// costs the connect budget (`budget::PROBE`) per attempt, with the toast up the whole time.
const RECONNECT_ATTEMPTS: u8 = 3;
const RECONNECT_PAUSE: Duration = Duration::from_secs(1);
/// A session that streamed this long earns the full attempt budget back: the count is for a
/// link that keeps failing, not for every drop in an evening.
const RECONNECT_RESET_AFTER: Duration = Duration::from_secs(60);

/// Whether a cosmetic frame drew, warning once per streak. webOS composites this app's
/// punch-through plane only while it holds the SAM foreground, so a TV panel over the stream —
/// the settings one the remote opens — fails every GL call on this surface until it closes. That
/// has to freeze the overlay and nothing else: these calls sit in the stream loop, whose `Err`
/// leaves `run_inner` and ends the process.
fn overlay_drawn(result: Result<()>, warned: &mut bool) -> bool {
    let Err(e) = result else {
        *warned = false;
        return true;
    };
    if !std::mem::replace(warned, true) {
        tracing::warn!("overlay frame skipped — this surface is not ours to draw on: {e:#}");
    }
    false
}

/// Puts the reconnect toast up over the emptied video plane and starts dial `attempt`.
fn redial(
    attempt: u8,
    gl: &mut Option<console_flow::ConsoleGl>,
    canvas: &sdl2::render::WindowCanvas,
    fonts: &pf_console_ui::theme::Fonts,
    display: (u32, u32),
    identity: &(String, String),
    dial: (&crate::app::ConnectTarget, &store::Settings),
) -> Result<crate::runtime::PendingConnect> {
    tracing::warn!("connection lost — reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
    let text = format!("Connection lost — reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
    let frame = overlay::frame(gl, canvas, fonts, display, overlay::TRANSPARENT, |f| {
        overlay::toast(f, &text, 1.0);
    });
    overlay_drawn(frame, &mut false);
    std::thread::sleep(RECONNECT_PAUSE);
    spawn_connect(identity.clone(), dial.0.clone(), dial.1.clone())
}

/// Waits for a reconnect's handshake with the toast up, reading input meanwhile: a Back tap,
/// the EXIT gesture or a quit gives up (`true`). The worker runs to completion and drops what
/// it built, which is the cancel — nothing joins it.
fn wait_for_dial(handle: &crate::runtime::PendingConnect, events: &mut sdl2::EventPump) -> bool {
    let mut exit_held = key_down(WEBOS_EXIT_SCANCODE);
    while !handle.is_finished() {
        for event in events.poll_iter() {
            if let sdl2::event::Event::KeyDown {
                keycode: Some(k),
                scancode: None,
                repeat: false,
                ..
            } = event
            {
                if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) {
                    return true;
                }
            }
        }
        if exit_gesture_fired(&mut exit_held) || QUIT_REQUESTED.load(Ordering::Relaxed) {
            return true;
        }
        crate::platform::webos::input::wait_for_event(Duration::from_millis(20));
    }
    false
}

/// How long a freeze-until-reanchor hold must last before the toast names it. An RFI recovery
/// lifts a hold within a round trip and the startup capacity probe's own loss clears at the
/// burst's end; a toast for those flashed on every blip and said nothing the picture did not.
const HOLD_TOAST_AFTER: Duration = Duration::from_millis(300);

pub(super) fn run_inner() -> Result<()> {
    // Stops webOS's launcher intercepting Back/Guide as its own shortcut (see `gamepad.rs`'s
    // BTN_GUIDE mapping). Must be set before window creation — these hints only latch there.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_BACK", "true");
    // Without this webOS SIGTERMs the app on a held/root-level Back before it can react.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT", "true");
    // Same for the remote's Home button, which otherwise backgrounds and kills the app; the
    // input loop re-opens the launcher itself via `luna::launch_home` instead. No `KEYS_META`
    // alongside it: a keyboard's Super key is Home-class, so that hint never suppressed it, and
    // in-stream it now reaches the host over evdev anyway (`platform::webos::evdev`).
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_HOME", "true");
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_KEYS_GUIDE", "true");
    // Suppress webOS's launcher ribbon popping over the foregrounded app.
    sdl2::hint::set("SDL_WEBOS_ACCESS_POLICY_RIBBON", "false");
    // Linear texture filtering — the focus-pop scale shimmers on SDL's default nearest.
    sdl2::hint::set("SDL_RENDER_SCALE_QUALITY", "1");
    // Nothing here wants a mouse synthesized from touch — the Magic Remote is a real pointer,
    // and a pad's touchpad must not be one at all (`mouse::is_touch_emulated`).
    sdl2::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");
    let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL_Init: {e}"))?;
    let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video subsystem: {e}"))?;
    let game_controller = sdl
        .game_controller()
        .map_err(|e| anyhow::anyhow!("SDL game controller subsystem: {e}"))?;
    let sdl_audio = sdl.audio().map_err(|e| anyhow::anyhow!("SDL audio subsystem: {e}"))?;
    tracing::info!("SDL video subsystem up (driver: {})", video.current_video_driver());

    let display_mode = video
        .current_display_mode(0)
        .map_err(|e| anyhow::anyhow!("current_display_mode: {e}"))?;
    tracing::info!(
        "display mode: {}x{}@{}",
        display_mode.w,
        display_mode.h,
        display_mode.refresh_rate
    );

    // The stream clears to alpha 0 for NDL's punch-through plane, and the GLES2 renderer's EGL
    // config carries no alpha channel by default — without this every transparent clear composites
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
    let mut canvas = window
        .into_canvas()
        // Explicit, so a GLES2 renderer that won't come up is a hard error rather than a silent
        // fall back to SDL's software path — ~25-45ms/frame on this SoC.
        .accelerated()
        .build()
        .map_err(|e| anyhow::anyhow!("create canvas: {e}"))?;
    tracing::info!("window + canvas created (renderer: {})", canvas.info().name);

    let mut events = sdl.event_pump().map_err(|e| anyhow::anyhow!("event pump: {e}"))?;

    let identity = store::load_or_create_identity().context("load_or_create_identity")?;

    // The overlays' fonts: the kit's, the same faces every screen draws with.
    let overlay_fonts = pf_console_ui::theme::build_fonts().context("overlay fonts")?;
    let display = (display_mode.w as u32, display_mode.h as u32);
    // The menu's layout box: the real mode divided by the panel-size correction, so a smaller
    // set lays out fewer, larger units and the canvas scale below makes them back up to
    // pixels. The window, the stream and the IME rect all keep the real mode.
    let ui_mode = {
        let k = crate::app::draw::panel_k();
        sdl2::video::DisplayMode::new(
            display_mode.format,
            (display_mode.w as f32 / k).round() as i32,
            (display_mode.h as f32 / k).round() as i32,
            display_mode.refresh_rate,
        )
    };

    // Owned above the loop, not re-declared per iteration: `ControllerDeviceAdded` fires only
    // once per physical (re)connection, so pads opened earlier must carry across screens.
    let mut pads = pads::Pads::default();
    // Why the *last* stream attempt bounced to the menu, shown on the fresh Home screen.
    let mut menu_status: Option<String> = None;
    // Same, but for a toast popup (e.g. the host closed the session) instead of the
    // bottom status line — shown on the Home screen right after re-entering the menu.
    let mut menu_toast: Option<String> = None;

    // Held across menu entries rather than per entry: the shell's GL context carries every
    // compiled shader and its glyph atlas, and rebuilding those costs the cold-start stutter
    // `console::gl` describes. `None` until the shell is first asked for, so a TV that never
    // turns it on never creates a second GL context at all.
    let mut console_gl: Option<console_flow::ConsoleGl> = None;
    // The loop's value is the exit action owed on the way out — see the single fire site
    // below it, which is the only reason this is a `break`-with-value rather than a `return`.
    let exit_plan = 'menu: loop {
        // Which of the two menus this entry draws. Asked per entry, not once, because that is
        // what makes the flip live: either side can write the setting and the other picks it
        // up on the next return here.
        let ui = if console_flow::wanted(crate::platform::webos::gamepad::any_pad_connected(&game_controller)) {
            console_flow::run(
                &mut canvas,
                &mut console_gl,
                &mut events,
                &game_controller,
                &mut pads,
                &identity,
                menu_toast.take().or_else(|| menu_status.take()),
            )?
        } else {
            run_ui_flow(
                &mut canvas,
                &mut console_gl,
                &mut events,
                &game_controller,
                &mut pads,
                &identity,
                ui_mode,
                menu_status.take(),
                menu_toast.take(),
            )?
        };
        // A `let ... else` can't bind out of its own else arm, and the quit case is exactly
        // where the value is.
        let ConnectOutcome {
            handle: connect_thread,
            target,
            settings,
            gamepad_auto,
            first_frame_deadline,
            exit_plan,
        } = match ui {
            UiOutcome::Launch(outcome) => *outcome,
            UiOutcome::Quit(plan) => break 'menu plan,
            // The flip: re-enter and read the setting again.
            UiOutcome::Reenter => continue 'menu,
        };
        tracing::debug!("settings: {settings:?}");

        // One pass per dial: the launch, then each reconnect of a lost link (`RECONNECT_ATTEMPTS`).
        let mut connect_thread = connect_thread;
        let mut first_frame_deadline = first_frame_deadline;
        let mut reconnects: u8 = 0;
        let outcome = 'session: loop {
            // A reconnect dial can be given up on from the remote; the first dial was waited
            // out by the loading screen, which read the remote itself.
            if reconnects > 0 && wait_for_dial(&connect_thread, &mut events) {
                tracing::info!("reconnect given up — returning to the menu");
                drop(connect_thread);
                menu_toast = Some("Connection lost".to_string());
                break 'session StreamOutcome::ReturnToMenu;
            }
            // Joined BEFORE the window is cleared transparent, so the finished launch zoom stays
            // on screen across the handshake and NDL load instead of a black punch-through hole,
            // and a failed connect never uncovers the plane at all.
            let connected = match connect_thread.join().expect("connect thread panicked") {
                Ok(c) => c,
                Err(e) if reconnects > 0 && reconnects < RECONNECT_ATTEMPTS => {
                    tracing::warn!("reconnect failed: {e:#}");
                    reconnects += 1;
                    connect_thread = redial(
                        reconnects,
                        &mut console_gl,
                        &canvas,
                        &overlay_fonts,
                        display,
                        &identity,
                        (&target, &settings),
                    )?;
                    continue 'session;
                }
                Err(e) => {
                    // Return to the menu with the reason on screen instead of `?`-ing the app down.
                    tracing::error!("session connect failed: {e:#}");
                    let what = if reconnects > 0 { "reconnect" } else { "connect" };
                    menu_status = Some(format!("Couldn't {what}: {}", crate::core::errors::friendly(&e)));
                    break 'session StreamOutcome::ReturnToMenu;
                }
            };
            tracing::info!("session connected, entering event loop");
            let session_started = Instant::now();
            // `connect` returns with the load issued and the pump feeding; the reveal then waits for
            // a frame to actually reach NDL, so the menu is swapped straight for live video. NDL's own
            // `PLAYING` is NOT that signal — it lands during `load()`, before anything is fed, and
            // `LOADCOMPLETED` is not one either: some sets report it only once a frame has been fed.
            // Bounded — a host that never sends must not leave a stale menu frame up.
            let reveal_wait = Instant::now();
            let deadline = reveal_deadline(first_frame_deadline);
            while !crate::platform::webos::ndl::presented() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(4));
            }
            tracing::info!(
                "NDL reveal after {:?} (presented={} playing={})",
                reveal_wait.elapsed(),
                crate::platform::webos::ndl::presented(),
                crate::platform::webos::ndl::playing(),
            );

            // `hide()` unmaps the surface entirely, silently breaking the Magic Remote's pointer
            // forwarding since Wayland has nowhere left to route motion. aurora-tv never hides its
            // window either — stays mapped, cleared fully transparent so the video shows through.
            overlay::wipe(&mut console_gl, &canvas, &overlay_fonts)?;
            // Local pointer hidden unless "Cursor capture" is off — otherwise it and the host's own
            // forwarded-position cursor read as "the pointer doesn't match the mouse".
            let mut cursor = cursor::Cursor::new(sdl.mouse());
            cursor.set_captured(settings.cursor_capture());
            cursor.flush(canvas.window());

            // `None` when the session decodes audio somewhere other than here (punktfunk's NDL Opus
            // offload) — a second unfed audio device would still claim a PulseAudio sink.
            // The device is held here for the length of the stream — dropping it stops playback — while
            // the feed half moves to its own decode thread.
            let audio = match connected.audio_channels() {
                None => None,
                Some(channels) => {
                    match crate::platform::webos::audio::AudioPlayer::new(
                        &sdl_audio,
                        channels,
                        connected.audio_buffer_cell(),
                    )
                    .and_then(|(player, sink)| {
                        let stage = crate::session::audio::AudioStage::new(
                            std::sync::Arc::new(sink),
                            channels,
                            connected.audio_layout_id(),
                        )?;
                        Ok((player, connected.spawn_audio_feed(stage)?))
                    }) {
                        Ok(pair) => Some(pair),
                        Err(e) => {
                            // Same no-crash policy as the connect above, plus the video teardown a
                            // loaded decoder now needs.
                            tracing::error!("audio player init failed: {e:#}");
                            connected.disconnect_quit();
                            connected.shutdown_and_quit();
                            cursor.set_captured(false);
                            cursor.flush(canvas.window());
                            menu_status = Some(format!("Couldn't start audio: {e:#}"));
                            break 'session StreamOutcome::ReturnToMenu;
                        }
                    }
                }
            };
            // Experimental: Game picture/sound mode, app-plane stand-in for HDMI ALLM. Best-effort;
            // reverted on stream exit. See `game_mode`.
            let restore_tv_modes = if settings.game_mode() {
                crate::platform::webos::game_mode::enter(connected.hdr())
            } else {
                Vec::new()
            };

            // Pad audio (`0xD1`): each pad's render caps ride its arrival. Only toward a host that has
            // the plane — an older host reads arrival flags as the bare pad index.
            let pad_audio = (connected.client.host_caps() & punktfunk_core::quic::HOST_CAP_PAD_AUDIO != 0
                && crate::session::pad_audio::wanted(&settings))
            .then(|| Arc::new(crate::session::pad_audio::Envelopes::default()));
            let mut pad_audio_thread = None;
            if let Some(envelopes) = &pad_audio {
                match crate::session::pad_audio::spawn(
                    connected.client.clone(),
                    connected.stop.clone(),
                    envelopes.clone(),
                ) {
                    Ok(handle) => pad_audio_thread = Some(handle),
                    Err(e) => tracing::warn!("pad audio off: {e:#}"),
                }
            }
            // Every pad starts on the handshake's kind; each one that is really another declares
            // itself, and so does any pad past the first under an explicit pick.
            let kind_setting = if gamepad_auto {
                store::GamepadType::Auto
            } else {
                settings.gamepad_type()
            };
            // Every Bluetooth pad's input arrives in 77.5 ms batches unless its link is held out of
            // sniff — see `pad_link`. Dropped with the session, which hands the links back.
            let pad_link = crate::platform::webos::pad_link::PadLink::start();
            // The connect wait polled SDL's queue without looking at pads.
            pads.sync(&game_controller);
            let pad_routes = pads::PadRoutes::default();
            pads.publish_routes(&pad_routes);
            for slot in pads.iter_mut() {
                slot.begin_session(settings.gamepad_type());
            }
            let ids: Vec<u32> = pads.iter().map(|slot| slot.id).collect();
            for id in ids {
                pad_session::bring_up(&connected, &mut pads, id, kind_setting, &settings, pad_audio.as_ref());
            }

            let mut scroll_acc = mouse::ScrollAccumulator::default();
            // Every button the client synthesizes rather than forwards: the remote's OK gestures and
            // its Red key — see `RemoteButtons`. Fed only the remote's own input; a real HID mouse's
            // clicks never reach it.
            let mut buttons = mouse::RemoteButtons::default();
            // Raw evdev HID, sending on the reader thread rather than queued for the ~2ms main loop
            // so a 1000 Hz mouse isn't re-resampled. Keyboards are grabbed whatever Capture says, or
            // the compositor sees Ctrl/Alt/Shift and warps its pointer mid-click; mouse nodes follow
            // Capture: on = exclusive relative grab, off = compositor keeps the pointer to aim with.
            let input = connected.input();
            let routes = pad_routes.clone();
            let hid = crate::platform::webos::evdev::HidInput::start(true, settings.cursor_capture(), move |report| {
                use crate::platform::webos::evdev::HidReport;
                match report {
                    HidReport::Input(source, ev) => input.send(source, ev),
                    HidReport::Release(source) => input.release(source),
                    HidReport::Rich(rich, uniq) => {
                        if let Some(rich) = pads::route_rich(&routes, rich, uniq) {
                            input.send_rich(rich);
                        }
                    }
                }
            });
            // Tells the remote's keys from a pad's echo, off the remote's own nodes.
            let mut remote_gate = RemoteGate::default();
            // Flips once a HID mouse is found — `HidInput::start` no longer scans before returning
            // (that blocked every stream connect on the node-open cost), so presence is only known
            // once the reader thread's own scan catches up; checked each tick below.
            let mut hid_device_seen = false;
            // Stats overlay: refreshed ~2Hz onto the transparent stream window, over the
            // punch-through video plane via per-pixel alpha — window is never shown/hidden (that
            // crashed an earlier attempt, see docs/NOTES.md). Green button cycles the tier, session-only.
            // The pumps read this too — with nothing on glass to show them, the video thread skips
            // every counter the overlay is the only reader of. Seeded here; from the first tick on it
            // is DERIVED from the fade below rather than set alongside the toggle, so there is one
            // writer and no second copy of the state to keep in step.
            let mut stats_tier = settings.stats_verbosity();
            let mut stats_enabled = stats_tier != StatsVerbosity::Off;
            let advanced_stats = settings.advanced_stats;
            connected.stats().set_diagnostics(stats_enabled);
            connected.set_hud_enabled(stats_enabled);
            // Fades in/out on the same curve as the toast below — see `ModalFade::visibility_alpha`.
            let mut stats_fade = crate::ui::fade::ModalFade::<()>::overlay();
            if stats_enabled {
                stats_fade.open();
            }
            // Seeded from live key state, not `false`: these are rising-edge polls, and the launch
            // itself is a keypress. A key still down when the stream loop starts (webOS's EXIT
            // gesture in particular — a synthetic press whose key-up may never arrive) would read as
            // a fresh press on the first tick and, for EXIT, open the disconnect dialog over the
            // video the instant the stream began.
            let mut green_held = key_down(WEBOS_GREEN_SCANCODE);
            let mut yellow_held = key_down(WEBOS_YELLOW_SCANCODE);
            let mut home_held = key_down(WEBOS_HOME_SCANCODE);
            // Blue controls text input because streams have no focused text field.
            let mut text_input = TextInputController::new(canvas.window().subsystem().text_input());
            // Transient toasts. `overlay_was_active` catches the fade-out edge so the canvas gets
            // wiped once; `stats_dst`/`log_dst` recomposite each frame at their own slower cadence.
            let mut notif = overlay::Notification::new();
            // One line per streak of undrawable frames — see `overlay_drawn`.
            let mut overlay_warned = false;
            // When the current freeze-until-reanchor hold began (`stats.holding`, see `session::pump`),
            // and whether it has been announced — see `HOLD_TOAST_AFTER`.
            let mut hold_since: Option<Instant> = None;
            let mut hold_toasted = false;
            let mut overlay_was_active = false;
            // The stats card's lines, rebuilt from the core's window once a second (and on a
            // tier change), and the log tail on its 500 ms cadence; drawn as they stand between.
            let mut stats_lines: Vec<HudLine> = Vec::new();
            let mut stats_snap: Option<StatsSnapshot> = None;
            let mut stats_built_at: Option<Instant> = None;
            let mut overlay_last: Option<Instant> = None;
            let mut prev_cpu: Option<(u64, Instant)> = None;
            // 0 = "Disconnect" focused, 1 = "Cancel" (default on open — safer).
            let mut disconnect = ConfirmDialog::new(
                "Stop streaming?",
                "The stream will end and you'll return to the menu.",
                Some(crate::app::view::icons::ICON_CLOSE),
                "Stop streaming",
                crate::app::screens::confirm::Tone::Danger,
            );
            // The quick-action dial: Select+A on the pad, drawn over the video (`core::dial`).
            let mut ring = pf_console_ui::Ring::new();
            let mut ring_was_open = false;
            let mut ring_drawn = 0u64;
            let mut ring_drawn_at = Instant::now();
            let native_mode = connected.client.mode();
            // A dial tap of Guide or QAM still owes its release: `(bit, due)`.
            let mut tap_up: Option<(u32, u8, Instant)> = None;
            let mut ring_stats = false;
            // Short Back tap forwards Esc; a held Back becomes webOS's EXIT gesture, polled below.
            // Seeded like the colour keys above — see there.
            let mut exit_held = key_down(WEBOS_EXIT_SCANCODE);
            // Waits for close-fade to finish.
            let mut pending_outcome: Option<StreamOutcome> = None;
            // Set when the user confirms the disconnect dialog — distinguishes that from the
            // host ending the session or the network dropping out, so the toast below only
            // fires for the latter. (SIGTERM/window-close also call `disconnect_quit()`, but
            // those break with `StreamOutcome::Quit` and never reach the check below.)
            let mut client_initiated_disconnect = false;
            // The link died under a session the user did not end: the one end worth dialling again.
            let mut lost = false;
            let mut input_suspended = false;
            let outcome = 'running: loop {
                if QUIT_REQUESTED.load(Ordering::Relaxed) {
                    tracing::warn!("SIGTERM/SIGINT received — disconnecting before exit");
                    connected.disconnect_quit();
                    break 'running StreamOutcome::Quit;
                }
                if settings.cursor_capture()
                    && !hid_device_seen
                    && hid
                        .as_ref()
                        .is_some_and(crate::platform::webos::evdev::HidInput::has_mouse)
                {
                    hid_device_seen = true;
                    cursor.disable_sdl_relative();
                    // Only now is the node grabbed, so only now can a compositor hide stick — the one
                    // at connect raced the reader thread's scan. Usually a no-op, since the call
                    // above re-issued it already; kept so the retract doesn't hinge on that.
                    cursor.reassert_hidden();
                    cursor.flush(canvas.window());
                }
                // The remote's presses, read before SDL's events so each is there to claim the key it
                // caused. A key the remote did not press is a pad's echo (webOS 23+), unless the
                // on-screen keyboard, which has no node of its own, typed it.
                let now = Instant::now();
                if let Some(hid) = hid.as_ref() {
                    remote_gate.adopt(hid.take_remote_nodes());
                }
                remote_gate.poll(now);
                let osk = text_input.is_shown(canvas.window());
                for event in events.poll_iter() {
                    use sdl2::event::Event;
                    // Never real pointer input, so never the host's — see `mouse::is_touch_emulated`.
                    if mouse::is_touch_emulated(&event) {
                        continue;
                    }
                    // Which SDL events are the compositor's echo of input this app already read off
                    // evdev. Owning the pointer node is the whole answer for buttons — it's decided
                    // in `evdev` (Capture, and whether the keyboard shares the node), so it isn't
                    // re-derived from the setting here. Keys go by recency instead, so the Magic
                    // Remote's keys — which never appear on a node we hold — still pass. Read once
                    // per event, not per guard: the window is 250ms, so per-arm freshness buys
                    // nothing.
                    let (hid_motion, hid_clicks, hid_keys) = match hid.as_ref() {
                        Some(hid) => {
                            let pointer = hid.has_mouse();
                            // A keypress with the pointer left to the compositor still moves it:
                            // webOS warps to screen centre, which would drag the host cursor along.
                            let keys = hid.keyboard_busy();
                            (pointer || keys, pointer, keys)
                        }
                        None => (false, false, false),
                    };
                    match event {
                        Event::Quit { .. } => {
                            connected.disconnect_quit();
                            break 'running StreamOutcome::Quit;
                        }
                        Event::ControllerDeviceAdded { which, .. } => {
                            // Under `Automatic` the handshake settled the pad kind from whatever was
                            // attached at connect time — usually nothing — so a pad arriving now has
                            // to declare itself or the host keeps driving its default Xbox pad.
                            if let Some(id) = pads.add(&game_controller, which).map(|slot| slot.id) {
                                pad_session::bring_up(
                                    &connected,
                                    &mut pads,
                                    id,
                                    kind_setting,
                                    &settings,
                                    pad_audio.as_ref(),
                                );
                                pads.publish_routes(&pad_routes);
                            }
                        }
                        // Only pads we hold: the Magic Remote drops and re-adds constantly.
                        Event::ControllerDeviceRemoved { which, .. } => {
                            if let Some(mut slot) = pads.remove(which) {
                                pads.publish_routes(&pad_routes);
                                // A dial tap's owed release would re-create the removed pad on the host.
                                if tap_up.is_some_and(|(_, pad, _)| pad == slot.index) {
                                    tap_up = None;
                                }
                                // An unplugged pad sends no releases: lift what the host holds, then
                                // free the index so a replug or another pad can take it.
                                slot.release_held(&connected);
                                connected.send_input(&gamepad::remove_event(slot.index));
                                // Handing a vanished pad back can wait on a Bluetooth reply or a card
                                // write that will not come; the stream loop must not.
                                slot.extras.retire();
                            }
                        }
                        // Dialog open: navigate it only, don't forward input to the host. A key only if
                        // the remote pressed it: a pad's echo would move it a second time.
                        _ if disconnect.is_open() => {
                            if remote_gate.admits(&event, now) {
                                match disconnect.handle_event(&event, &overlay_fonts, display.0, display.1) {
                                    Some(ConfirmAction::Confirmed) => {
                                        tracing::info!("disconnecting to menu");
                                        client_initiated_disconnect = true;
                                        connected.disconnect_quit();
                                        disconnect.dismiss();
                                        pending_outcome = Some(StreamOutcome::ReturnToMenu);
                                    }
                                    Some(ConfirmAction::Dismissed) => overlay_last = None,
                                    Some(ConfirmAction::Navigated) | None => {}
                                }
                            }
                        }
                        // Dial open: the remote's keys drive it, and no key or pointer input reaches
                        // the host. The pad drives it through `dial`, so its echo must not.
                        Event::KeyDown {
                            keycode: Some(k),
                            repeat: false,
                            ..
                        } if ring.open() && remote_gate.admits(&event, now) => {
                            if let Some(ev) = ring_event_for_key(k) {
                                ring.menu(ev);
                            }
                        }
                        Event::KeyDown { .. }
                        | Event::KeyUp { .. }
                        | Event::TextInput { .. }
                        | Event::MouseMotion { .. }
                        | Event::MouseButtonDown { .. }
                        | Event::MouseButtonUp { .. }
                        | Event::MouseWheel { .. }
                            if ring.open() =>
                        {
                            // Still shown to the gate, so a key admitted into the dial releases there.
                            remote_gate.admits(&event, now);
                        }
                        // Scancode keys are real game input — forward only, never open the dialog.
                        Event::KeyDown { scancode: Some(sc), .. }
                            if !hid_keys && (remote_gate.admits(&event, now) || osk) =>
                        {
                            if let Some(ev) = keyboard::key_event(sc, true) {
                                connected.send_input(&ev);
                            }
                        }
                        // Forward composed IME text, which has no scancode.
                        Event::TextInput { text, .. } => {
                            for ev in keyboard::text_key_events(&text) {
                                connected.send_input(&ev);
                            }
                        }
                        // Magic Remote Red — the right button (see `RemoteButtons::red`). Like Back
                        // it carries only a keycode (see `WEBOS_RED_KEYCODE`); `repeat: false` so the
                        // OS's auto-repeat while it's held doesn't restate the press.
                        Event::KeyDown {
                            keycode: Some(k),
                            repeat: false,
                            ..
                        } if k.into_i32() == crate::platform::webos::input::WEBOS_RED_KEYCODE => {
                            buttons.red(true, |ev| connected.send_input(ev));
                        }
                        Event::KeyUp { keycode: Some(k), .. }
                            if k.into_i32() == crate::platform::webos::input::WEBOS_RED_KEYCODE =>
                        {
                            buttons.red(false, |ev| connected.send_input(ev));
                        }
                        // Magic Remote Blue — raises the on-screen keyboard, so a game needing text
                        // (chat, a search box) doesn't require dropping back to the menu. Like Red
                        // it's matched by keycode, not polled by scancode: confirmed on-device that
                        // `WEBOS_BLUE_SCANCODE`'s bit misses the first press of a session, while the
                        // keycode arrives reliably from the very first press (see
                        // `WEBOS_BLUE_KEYCODE`'s doc). `repeat: false` so a held Blue doesn't re-raise
                        // on every OS auto-repeat tick. Hiding is Back's job — webOS's IME dismisses
                        // on it, and this app never calls `stop()` in response to that.
                        Event::KeyDown {
                            keycode: Some(k),
                            repeat: false,
                            ..
                        } if k.into_i32() == WEBOS_BLUE_KEYCODE => {
                            raise_keyboard(&mut text_input, display_mode.w, display_mode.h);
                        }
                        // Magic Remote Back has no scancode — forwarded as Esc. A held Back never
                        // arrives here; webOS delivers it as the EXIT gesture polled below instead.
                        Event::KeyDown {
                            keycode: Some(k),
                            scancode: None,
                            repeat: false,
                            ..
                        } if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back)
                            && remote_gate.admits(&event, now) =>
                        {
                            if let Some(ev) = keyboard::key_event(sdl2::keyboard::Scancode::Escape, true) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::KeyUp {
                            keycode: Some(k),
                            scancode: None,
                            ..
                        } if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back)
                            && remote_gate.admits(&event, now) =>
                        {
                            if let Some(ev) = keyboard::key_event(sdl2::keyboard::Scancode::Escape, false) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::KeyUp { scancode: Some(sc), .. }
                            if !hid_keys && (remote_gate.admits(&event, now) || osk) =>
                        {
                            if let Some(ev) = keyboard::key_event(sc, false) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::ControllerButtonDown { which, button, .. }
                        | Event::ControllerButtonUp { which, button, .. } => {
                            let down = matches!(event, Event::ControllerButtonDown { .. });
                            let open = ring.open();
                            let Some(slot) = pads.get_mut(which) else {
                                continue;
                            };
                            if !open {
                                slot.chord.set(button, down);
                            }
                            // Forwarded buttons still reach the host: the hold requirement is what
                            // keeps game input and the disconnect shortcut apart.
                            match slot.dial.button(gamepad::button_bit(button), down, open) {
                                PadRoute::Forward => {
                                    connected.send_input(&gamepad::button_event(button, down, slot.index));
                                }
                                PadRoute::Open => {
                                    ring.set_facts(&ring_facts(&settings, &connected, stats_tier, native_mode, true));
                                    ring.input(RingInput::Toggle {
                                        x: display.0 as f32 / 2.0,
                                        y: display.1 as f32 / 2.0,
                                    });
                                }
                                PadRoute::Menu(ev) => {
                                    ring.menu(ev);
                                }
                                PadRoute::Drop => {}
                            }
                        }
                        Event::ControllerAxisMotion { which, axis, value, .. } => {
                            let open = ring.open();
                            let Some(slot) = pads.get_mut(which) else {
                                continue;
                            };
                            if matches!(axis, sdl2::controller::Axis::LeftX | sdl2::controller::Axis::LeftY) {
                                if let Some(ev) =
                                    slot.dial.left_stick(axis == sdl2::controller::Axis::LeftX, value, open)
                                {
                                    ring.menu(ev);
                                }
                            }
                            if !open {
                                connected.send_input(&gamepad::axis_event(axis, value, slot.index));
                            }
                        }
                        // Magic Remote pointer mode surfaces as plain SDL2 mouse events, forwarded
                        // to the host instead of driving local UI focus (see `mouse.rs`).
                        Event::MouseMotion { x, y, xrel, yrel, .. } => {
                            if !hid_motion {
                                // Relative only for the remote alone: SDL's warp emulation is off
                                // whenever the evdev reader owns motion, so the remote sends
                                // absolute — also the better fit for a device the user aims.
                                let relative = settings.cursor_capture() && !hid_device_seen;
                                let ev = if relative {
                                    mouse::move_relative_event(xrel, yrel)
                                } else {
                                    mouse::move_event(x, y, display_mode.w as u32, display_mode.h as u32)
                                };
                                // Drift/drag arbitration for an OK press in flight, off whichever of
                                // the two the pointer actually reports meaningfully. Runs *before*
                                // the motion is forwarded: when this is the motion that commits to a
                                // drag, the host must see the button go down at the press point and
                                // only then the travel, or the drag grabs `DRAG_SLOP` px late — which
                                // moves a window by the wrong offset and starts a selection
                                // rectangle in the wrong place.
                                if relative {
                                    buttons.motion_rel(xrel, yrel, |ev| connected.send_input(ev));
                                } else {
                                    buttons.motion_abs(x, y, |ev| connected.send_input(ev));
                                }
                                connected.send_input(&ev);
                            }
                        }
                        // With `cursor_gestures` on, the remote's only pointer button carries
                        // three gestures. Off (the default), and for every other button, and for
                        // a real mouse's clicks, the arms below pass the press straight through
                        // as they always have.
                        Event::MouseButtonDown {
                            mouse_btn: sdl2::mouse::MouseButton::Left,
                            x,
                            y,
                            ..
                        } if !hid_clicks && settings.cursor_gestures() => buttons.ok_press(x, y),
                        Event::MouseButtonUp {
                            mouse_btn: sdl2::mouse::MouseButton::Left,
                            ..
                        } if !hid_clicks && settings.cursor_gestures() => {
                            buttons.ok_release(|ev| connected.send_input(ev))
                        }
                        Event::MouseButtonDown { mouse_btn, .. } if !hid_clicks => {
                            if let Some(ev) = mouse::button_event(mouse_btn, true) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::MouseButtonUp { mouse_btn, .. } if !hid_clicks => {
                            if let Some(ev) = mouse::button_event(mouse_btn, false) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::MouseWheel { x, y, .. } if !hid_clicks => {
                            if y != 0 {
                                if let Some(ev) = scroll_acc.scroll_event(y, false) {
                                    connected.send_input(&ev);
                                }
                            }
                            if x != 0 {
                                if let Some(ev) = scroll_acc.scroll_event(x, true) {
                                    connected.send_input(&ev);
                                }
                            }
                        }
                        _ => {}
                    }
                }
                // The dial took the pad: the host must see nothing held. On close the sticks are
                // re-sent, since SDL only reports changes and a held stick would stay dead there.
                let ring_open = ring.open();
                if ring_open != ring_was_open {
                    ring_was_open = ring_open;
                    if ring_open {
                        for slot in pads.iter_mut() {
                            slot.chord.clear();
                            slot.release_held(&connected);
                        }
                        connected.release_input();
                        buttons.release_held(|ev| connected.send_input(ev));
                    } else {
                        for slot in pads.iter_mut() {
                            slot.dial.closed();
                            slot.resend_sticks(&connected);
                        }
                    }
                }
                if ring_open {
                    ring.set_facts(&ring_facts(
                        &settings,
                        &connected,
                        stats_tier,
                        native_mode,
                        !pads.is_empty(),
                    ));
                }
                ring.tick();
                while let Some(cmd) = ring.take_command() {
                    tracing::info!(?cmd, "dial");
                    match cmd {
                        RingCommand::EndStream => {
                            connected.disconnect_quit();
                            break 'running StreamOutcome::ReturnToMenu;
                        }
                        // No quit code: the host keeps the session for a reconnect.
                        RingCommand::DisconnectLinger => {
                            break 'running StreamOutcome::ReturnToMenu;
                        }
                        RingCommand::CycleStats => ring_stats = true,
                        RingCommand::Keyboard => raise_keyboard(&mut text_input, display_mode.w, display_mode.h),
                        RingCommand::RequestMode {
                            width,
                            height,
                            refresh_hz,
                        } => {
                            let mode = punktfunk_core::config::Mode {
                                width,
                                height,
                                refresh_hz,
                            };
                            if let Err(e) = connected.client.request_mode(mode) {
                                tracing::warn!("dial: mode request: {e}");
                            }
                        }
                        RingCommand::Shortcut(keys) => send_shortcut(&connected, &keys),
                        RingCommand::TapButton(bit) => {
                            // Pad 0 may be unplugged while another pad holds the dial.
                            let pad = pads.first().map_or(0, |slot| slot.index);
                            connected.send_input(&gamepad::bit_event(bit, true, pad));
                            tap_up = Some((bit, pad, Instant::now() + TAP_PRESS));
                        }
                        RingCommand::TogglePadMouse => toggle_pad_mouse(&connected, !pads.is_empty()),
                        // No microphone and no touch surface on a TV. Stream mute needs a zeroed
                        // decoded frame, and NDL's audio plane decodes Opus itself.
                        RingCommand::ToggleMic | RingCommand::CycleTouchMode | RingCommand::ToggleStreamMute => {}
                    }
                }
                // Host actions: this client keeps no action cache, so their slots stay dimmed.
                drop(ring.take_cmds());
                if let Some((bit, pad, due)) = tap_up {
                    if Instant::now() >= due {
                        connected.send_input(&gamepad::bit_event(bit, false, pad));
                        tap_up = None;
                    }
                }
                // An open dialog swallows pointer input from here on, so no release ever arrives for
                // whatever is down — the same trap `DisconnectChord::clear` covers for the pad. Done
                // here rather than at each `open` site so every path into the dialog is covered.
                // Otherwise: a held OK commits to a drag once `DRAG_HOLD` is up, and a stationary
                // hold emits no events at all, so this tick is the only thing that can notice.
                if disconnect.is_open() {
                    if !input_suspended {
                        connected.release_input();
                    }
                    buttons.release_held(|ev| connected.send_input(ev));
                } else {
                    buttons.tick(|ev| connected.send_input(ev));
                }
                // Chord held long enough — open the dialog, then forget it so it fires once per hold.
                if !disconnect.is_open() && pads.chord_held(EXIT_HOLD) {
                    tracing::info!("disconnect shortcut held — opening dialog");
                    pads.clear_chords();
                    disconnect.open(1);
                }
                // EXIT gesture (held Back) opens the dialog; a short tap is Esc, above.
                if exit_gesture_fired(&mut exit_held) && !disconnect.is_open() {
                    tracing::info!("EXIT gesture — opening disconnect dialog");
                    disconnect.open(1);
                }
                // The dialog owns input and the canvas, so the dial gives way to it.
                if disconnect.is_open() && ring.open() {
                    ring.input(RingInput::Cancel);
                }
                // Re-opens the webOS launcher; a long Back fires EXIT above, never this.
                if home_key_fired(&mut home_held) {
                    crate::platform::webos::luna::launch_home();
                }
                // SDL2 lacks these colour scancodes. Ignore them while the dialog owns input.
                let dialog_open = disconnect.is_open();
                // `|`, not `||`: the green key's edge must be read every tick, and so must the dial's.
                if rising_edge(!dialog_open && key_down(WEBOS_GREEN_SCANCODE), &mut green_held)
                    | std::mem::take(&mut ring_stats)
                {
                    stats_tier = stats_tier.next();
                    let was_enabled = stats_enabled;
                    stats_enabled = stats_tier != StatsVerbosity::Off;
                    overlay_last = None; // force an immediate redraw
                    if stats_enabled && !was_enabled {
                        stats_fade.reopen();
                    } else if !stats_enabled {
                        stats_fade.close(());
                    }
                    // A fading-out card keeps its last lines; a visible one re-renders at once.
                    if let (true, Some(snap)) = (stats_enabled, &stats_snap) {
                        stats_lines = hud::format(snap, stats_tier, advanced_stats);
                    }
                }
                if rising_edge(!dialog_open && key_down(WEBOS_YELLOW_SCANCODE), &mut yellow_held) {
                    cycle_log_overlay();
                    overlay_last = None; // force an immediate redraw with the new state
                }
                // Connection-issue toast, once per hold that outlasts `HOLD_TOAST_AFTER` — the same
                // "network trouble" signal the stats overlay's "Beat" line reads, visible without the
                // overlay open. No matching "recovered" toast: the picture resuming is that signal.
                if connected.stats().holding.load(Ordering::Relaxed) {
                    let since = *hold_since.get_or_insert_with(Instant::now);
                    if !hold_toasted && since.elapsed() >= HOLD_TOAST_AFTER {
                        hold_toasted = true;
                        tracing::warn!("connection issues detected (freeze-until-reanchor)");
                        notif.show("Connection issues — recovering...");
                        overlay_last = None;
                    }
                } else {
                    hold_since = None;
                    hold_toasted = false;
                }
                // The dialog is navigated with the Magic Remote's pointer, so a captured stream
                // must hand the pointer back while it's up — hidden/relative there'd be nothing
                // to aim with. Recaptured on dismiss. The evdev reader releases its grabs for the
                // same window in either Capture mode — the dialog needs the remote's keys as much
                // as its pointer, and holding a grab would only leave a HID device dead meanwhile.
                let want_captured = settings.cursor_capture() && !disconnect.is_open();
                if want_captured != cursor.is_captured() {
                    cursor.set_captured(want_captured);
                }
                if let Some(hid) = &hid {
                    hid.set_active(!disconnect.is_open() && !ring.open());
                }
                input_suspended = disconnect.is_open();
                // True during fade-out, past `is_open()`; gates the stats overlay below.
                let dialog_animating = disconnect.tick();
                let dialog_frame = disconnect.frame();
                if dialog_frame.is_some() && disconnect.redraw_due(dialog_animating) {
                    // Own pass over the punch-through video: the dialog alone, on a transparent
                    // clear (NDL video is on a hardware plane below this surface, so no blur).
                    let frame = overlay::frame(
                        &mut console_gl,
                        &canvas,
                        &overlay_fonts,
                        display,
                        overlay::TRANSPARENT,
                        |f| {
                            disconnect.draw(f);
                        },
                    );
                    overlay_drawn(frame, &mut overlay_warned);
                } else if dialog_frame.is_none() && dialog_animating {
                    // Close-fade just finished. Confirmed Disconnect: break now, nothing to wipe
                    // since the pre-stream UI takes the canvas next.
                    if let Some(outcome) = pending_outcome.take() {
                        break 'running outcome;
                    }
                    // Cancel/Back: wipe the last frame so it doesn't stick over the video.
                    let wipe = overlay::wipe(&mut console_gl, &canvas, &overlay_fonts);
                    overlay_drawn(wipe, &mut overlay_warned);
                }
                // Audio drains on its own threads either way now — the software path on
                // `session::pump`'s feed thread into SDL's audio callback, the offloaded path on
                // its NDL audio pump. Nothing for this loop to do.
                //
                // Unconditional so both feedback planes keep draining with no pad attached.
                connected.pump_feedback_once(&mut pads);
                // Skipped while the dialog owns the canvas. Stats/log share one clear/execute/present
                // so neither erases the other's tile.
                //
                // `log_overlay_lines()` deferred to the throttled block below, not called every
                // ~2ms tick — it locks the same mutex log writes contend on ~500x/s.
                let notif_frame = if dialog_frame.is_none() {
                    notif.frame().map(|(t, a)| (t.to_string(), a))
                } else {
                    None
                };
                let notif_active = notif_frame.is_some();
                // Fade in/out on the toast's curve instead of cutting instantly; `visibility_alpha`
                // keeps returning `Some` through the close fade after the toggle itself flips off.
                let stats_alpha = stats_fade.visibility_alpha(stats_enabled);
                // The counters follow what is VISIBLE, fade included — stopping them at the toggle
                // freezes the figures for the last frames of the fade-out.
                connected.stats().set_diagnostics(stats_alpha.is_some());
                connected.set_hud_enabled(stats_alpha.is_some());
                let log_overlay_on = log_overlay_state() != LogOverlayState::Off;
                let ring_damage = ring.damage();
                let ring_visible = ring_damage != 0;
                let overlay_active = stats_alpha.is_some() || log_overlay_on || notif_active || ring_visible;
                if overlay_was_active && !overlay_active {
                    // Nothing else clears this window when the last overlay disappears.
                    // A wipe that could not draw stays owed, so the next tick tries it again.
                    let wipe = overlay::wipe(&mut console_gl, &canvas, &overlay_fonts);
                    overlay_was_active = !overlay_drawn(wipe, &mut overlay_warned);
                } else {
                    overlay_was_active = overlay_active;
                }
                // A fade in flight needs frequent frames; steady-state stats/log are fine at ~2Hz.
                let fading = notif_active || stats_fade.is_animating();
                let redraw_interval = if fading {
                    Duration::from_millis(33)
                } else {
                    Duration::from_millis(500)
                };
                let ring_due = ring_visible && ring_damage != ring_drawn && ring_drawn_at.elapsed() >= RING_FRAME;
                if overlay_active
                    && dialog_frame.is_none()
                    && (ring_due || overlay_last.is_none_or(|t| t.elapsed() >= redraw_interval))
                {
                    overlay_last = Some(Instant::now());
                    let ring_dt = ring_drawn_at.elapsed().as_secs_f64();
                    ring_drawn_at = Instant::now();
                    ring_drawn = ring_damage;
                    // The core window closes once a second: every rate it reports is per window.
                    if stats_enabled && stats_built_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
                        stats_built_at = Some(Instant::now());
                        let mut snap = connected.hud_snapshot();
                        snap.extras = tv_extras(&connected, &mut prev_cpu);
                        stats_lines = hud::format(&snap, stats_tier, advanced_stats);
                        stats_snap = Some(snap);
                    }
                    let log_lines = log_overlay_lines();
                    let frame = overlay::frame(
                        &mut console_gl,
                        &canvas,
                        &overlay_fonts,
                        display,
                        overlay::TRANSPARENT,
                        |f| {
                            if let Some(alpha) = stats_alpha {
                                overlay::stats(f, &stats_lines, stats_hint(stats_tier), alpha);
                            }
                            if let Some(lines) = &log_lines {
                                overlay::log(f, lines);
                            }
                            if let Some((text, alpha)) = &notif_frame {
                                overlay::toast(f, text, *alpha);
                            }
                            if ring_visible {
                                ring.render(f.canvas, f.w as u32, f.h as u32, f.k, f.fonts, ring_dt);
                            }
                        },
                    );
                    overlay_drawn(frame, &mut overlay_warned);
                }
                // The decoder is gone for this load (`core::media::VideoSink::is_dead`, set by the
                // pump). The transport is still healthy, so nothing below would ever end the session
                // and the user would sit in front of a frozen picture — end it here instead.
                if connected.stats().decoder_dead.load(Ordering::Relaxed) {
                    tracing::error!("decoder failed for good — returning to the menu");
                    menu_toast = Some("Video decoder failed — session ended".to_string());
                    break 'running StreamOutcome::ReturnToMenu;
                }
                if connected.is_session_ended() {
                    let reason = connected.end_reason();
                    tracing::info!("session ended: {reason:?}");
                    // Also flips true right after *our own* `disconnect_quit()` calls above
                    // (Back/dialog, SIGTERM) — no toast for those, the user just asked for it.
                    if !client_initiated_disconnect {
                        lost = reason == punktfunk_core::client::PunktfunkEndReason::Lost;
                        menu_toast = Some(connected.end_message());
                    }
                    break 'running StreamOutcome::ReturnToMenu;
                }

                // Bounds staleness of forwarded input/audio (video has its own thread). 2ms keeps
                // added latency near zero; the wakeup rate is noise even on this SoC.
                crate::platform::webos::input::wait_for_event(Duration::from_millis(2));
            };
            connected.release_input();
            text_input.stop();

            // Hand the Bluetooth links back to the TV's sniff policy.
            drop(pad_link);
            // Trigger resistance is firmware state that outlives the session — hand every pad back
            // first or a game that ended with R2 stiff leaves it stiff on the TV home screen. Dropping
            // the extras also stops each wired card writer. Rumble is likewise pad state.
            for slot in pads.iter_mut() {
                slot.extras = pad_session::Extras::default();
                let _ = slot.pad.set_rumble(0, 0, 0);
            }
            // Stopping the threads, the QUIC close and the NDL unload take a second or two; the
            // menu comes back now and they finish behind it. The next load waits for them.
            connected.stop.store(true, Ordering::Relaxed);
            // The SDL device stays on this thread; its feed thread joins with the rest. A late
            // `try_send` into the dropped ring is a no-op.
            let audio_feed = audio.map(|(_player, feed_thread)| feed_thread);
            crate::platform::webos::ndl::defer_teardown(move || {
                if let Some(handle) = pad_audio_thread {
                    crate::services::join::join_with_timeout(
                        handle,
                        crate::services::join::SHUTDOWN_JOIN_TIMEOUT,
                        "pad-audio",
                        || (),
                    );
                }
                if let Some(feed_thread) = audio_feed {
                    connected.stop_audio_feed(feed_thread);
                }
                // Joins the video thread and drops `client` so the QUIC close frame actually sends.
                connected.shutdown_and_quit();
                // Put the TV's picture/sound modes back (no-op unless game mode switched them).
                crate::platform::webos::game_mode::restore(restore_tv_modes);
                tracing::info!("session torn down");
            });
            cursor.set_captured(false);
            cursor.flush(canvas.window());
            // A lost link is dialled again with the same target and settings, the toast up over
            // the (now empty) video plane while the handshake runs. Anything else ends here.
            if session_started.elapsed() >= RECONNECT_RESET_AFTER {
                reconnects = 0;
            }
            let again = lost
                && matches!(outcome, StreamOutcome::ReturnToMenu)
                && reconnects < RECONNECT_ATTEMPTS
                && !QUIT_REQUESTED.load(Ordering::Relaxed);
            if !again {
                break 'session outcome;
            }
            reconnects += 1;
            menu_toast = None;
            // A fresh first-frame budget: the loading screen's deadline belongs to the first dial.
            first_frame_deadline = None;
            connect_thread = redial(
                reconnects,
                &mut console_gl,
                &canvas,
                &overlay_fonts,
                display,
                &identity,
                (&target, &settings),
            )?;
        };
        match outcome {
            StreamOutcome::Quit => break 'menu exit_plan,
            StreamOutcome::ReturnToMenu => continue,
        }
    };
    // The one place the app acts on a host's exit behaviour. Every way out of the loop above
    // has already torn down whatever session it had, which is the ordering this action needs:
    // the host refuses a cert-lane power action while a session is live. Blocking, because the
    // process is ending — a request abandoned mid-flight does nothing.
    crate::platform::webos::ndl::await_teardown();
    if let Some(plan) = exit_plan {
        plan.run();
    }
    tracing::info!("punktfunk-webos exiting cleanly");
    Ok(())
}

/// What the dial's slots read this frame. Controller mouse targets pad 0.
fn ring_facts(
    settings: &store::Settings,
    connected: &crate::session::Connected,
    stats: StatsVerbosity,
    native: punktfunk_core::config::Mode,
    pad: bool,
) -> RingFacts {
    let c = &connected.client;
    let m = c.mode();
    RingFacts {
        overlay_actions: settings.overlay_actions.clone(),
        stats_tier: stats.label().into(),
        pad_mouse_target: u16::from(pad),
        pad_mouse_on: pad && c.pad_mouse() & 1 != 0,
        pointer_granted: c.access_grants() & punktfunk_core::quic::GRANT_POINTER != 0,
        mode: (m.width, m.height, m.refresh_hz),
        native_mode: (native.width, native.height, native.refresh_hz),
        ..RingFacts::default()
    }
}

/// Flips pad 0 between controller mouse and the game.
fn toggle_pad_mouse(connected: &crate::session::Connected, pad: bool) {
    if !pad {
        return;
    }
    let on = connected.client.pad_mouse();
    let next = if on & 1 != 0 { on & !1 } else { on | 1 };
    if let Err(e) = connected.client.set_pad_mouse(next) {
        tracing::warn!("dial: controller mouse: {e}");
    }
}

/// A dial shortcut: every key down in order, then up in reverse. A key this build cannot name
/// sends nothing, like the other clients.
fn send_shortcut(connected: &crate::session::Connected, keys: &[String]) {
    let vks: Vec<u8> = keys
        .iter()
        .filter_map(|k| pf_client_core::overlay_actions::key_vk(k))
        .collect();
    if vks.is_empty() || vks.len() != keys.len() {
        return;
    }
    let key = |vk: u8, down: bool| punktfunk_core::input::InputEvent {
        kind: if down {
            punktfunk_core::input::InputKind::KeyDown
        } else {
            punktfunk_core::input::InputKind::KeyUp
        },
        _pad: [0; 3],
        code: u32::from(vk),
        x: 0,
        y: 0,
        flags: 0,
    };
    for &vk in &vks {
        connected.send_input(&key(vk, true));
    }
    for &vk in vks.iter().rev() {
        connected.send_input(&key(vk, false));
    }
}

/// Raises the on-screen keyboard. webOS wants a rectangle before it enables the IME.
fn raise_keyboard(text_input: &mut TextInputController, w: i32, h: i32) {
    let width = 400i32.min(w);
    text_input.raise(sdl2::rect::Rect::new((w - width) / 2, h - 120, width as u32, 60));
}

/// The remote's keys as dial events.
fn ring_event_for_key(k: sdl2::keyboard::Keycode) -> Option<pf_client_core::menu_nav::MenuEvent> {
    use crate::core::event::MenuEvent as E;
    use pf_client_core::menu_nav::{MenuDir, MenuEvent as K};
    Some(match crate::platform::webos::input::menu_event_for_key(k)? {
        E::Up => K::Move(MenuDir::Up),
        E::Down => K::Move(MenuDir::Down),
        E::Left => K::Move(MenuDir::Left),
        E::Right => K::Move(MenuDir::Right),
        E::Confirm => K::Confirm,
        E::Back => K::Back,
        E::Secondary => K::Secondary,
    })
}

/// What the green button does next: more detail, or hide from the top tier.
fn stats_hint(tier: StatsVerbosity) -> &'static str {
    match tier {
        StatsVerbosity::Detailed | StatsVerbosity::Off => "Press green to hide this overlay",
        StatsVerbosity::Compact | StatsVerbosity::Normal => "Press green for more detail",
    }
}

/// Lines only this client measures: NDL's feed, backlog and hold, the audio route, the pacing
/// loop, and this process's CPU and memory. `prev_cpu` spans CPU ticks between two calls.
fn tv_extras(connected: &session::Connected, prev_cpu: &mut Option<(u64, Instant)>) -> Vec<Extra> {
    let stats = connected.stats();
    let backlog = stats.render_backlog.load(Ordering::Relaxed);
    let mut ndl = format!(
        "NDL feed {:.1} ms · backlog {}",
        stats.feed_us.load(Ordering::Relaxed) as f32 / 1000.0,
        if backlog < 0 {
            "n/a".to_string()
        } else {
            backlog.to_string()
        },
    );
    if stats.holding.load(Ordering::Relaxed) {
        ndl.push_str(" · holding");
    }
    // NDL paces the picture on the plane's lead, so a lead sagging towards zero reads as stutter.
    let layout = connected.audio_layout();
    let audio = if connected.audio_route.on_ndl_plane() {
        format!(
            "{} {layout} · NDL · lead {} ms · av stamp {:+} ms",
            connected.audio_route.overlay_tag(),
            stats.audio_plane_lead_ms.load(Ordering::Relaxed),
            stats.av_offset_ms.load(Ordering::Relaxed),
        )
    } else {
        format!(
            "{} {layout} · buf {} ms",
            connected.audio_route.overlay_tag(),
            connected.audio_buffer_ms()
        )
    };
    // `cushion` leads: it is the only figure here that answers "is the presentation setting doing
    // anything". Jitter is the measured residual, independent of the cushion by construction, and
    // `late` is cumulative — neither moves when the setting does.
    let slack = stats.pacing_min_slack_us.load(Ordering::Relaxed);
    let pacing = format!(
        "pace cushion {:.1} ms · jitter {:.1} ms · late {}{}",
        stats.pacing_cushion_us.load(Ordering::Relaxed) as f32 / 1000.0,
        stats.pacing_jitter_us.load(Ordering::Relaxed) as f32 / 1000.0,
        stats.pacing_late.load(Ordering::Relaxed),
        // Worst complete-AU margin of the last window. Negative while jitter reads healthy is a
        // large frame finishing against the deadline its first piece set.
        if slack == i32::MIN {
            String::new()
        } else {
            format!(" · slack {:+.1} ms", slack as f32 / 1000.0)
        },
    );
    let mut out = vec![Extra::detail(ndl), Extra::detail(audio), Extra::detail(pacing)];
    // The set's own headroom, in both vocabularies. CPU shows from the second sample on.
    if let Some((ticks, mem_bytes)) = device::process_cpu_mem() {
        let cpu = prev_cpu.map(|(prev, at)| {
            let secs = at.elapsed().as_secs_f64().max(0.001);
            let pct = ticks.saturating_sub(prev) as f64 / device::clock_ticks_per_sec() as f64 / secs * 100.0;
            format!("CPU {pct:.0}% · ")
        });
        *prev_cpu = Some((ticks, Instant::now()));
        out.push(Extra {
            text: format!(
                "{}RAM {:.0} MB",
                cpu.unwrap_or_default(),
                mem_bytes as f64 / (1024.0 * 1024.0)
            ),
            tier: StatsVerbosity::Detailed,
            advanced_only: false,
            role: Role::Muted,
        });
    }
    out
}
