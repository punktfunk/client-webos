use super::overlay::{self, ConfirmAction, ConfirmDialog};
use super::*;
use crate::core::settings::TvSettings;
use crate::platform::webos::device;
use crate::platform::webos::input::{
    webos_scancode_down as key_down, WEBOS_BLUE_KEYCODE, WEBOS_EXIT_SCANCODE, WEBOS_GREEN_SCANCODE,
    WEBOS_HOME_SCANCODE, WEBOS_YELLOW_SCANCODE,
};

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

/// Puts the reconnect toast up over the emptied video plane and starts dial `attempt`.
fn redial(
    attempt: u8,
    gl: &mut Option<console_flow::ConsoleGl>,
    canvas: &sdl2::render::WindowCanvas,
    fonts: &pf_console_ui::theme::Fonts,
    display: (u32, u32),
    identity: &(String, String),
    dial: (&crate::app::ConnectTarget, &store::Settings),
) -> Result<std::thread::JoinHandle<Result<session::Connected>>> {
    tracing::warn!("connection lost — reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
    let text = format!("Connection lost — reconnecting ({attempt}/{RECONNECT_ATTEMPTS})");
    overlay::frame(gl, canvas, fonts, display, overlay::TRANSPARENT, |f| {
        overlay::toast(f, &text, 1.0);
    })?;
    std::thread::sleep(RECONNECT_PAUSE);
    spawn_connect(identity.clone(), dial.0.clone(), dial.1.clone())
}

/// Waits for a reconnect's handshake with the toast up, reading input meanwhile: a Back tap,
/// the EXIT gesture or a quit gives up (`true`). The worker runs to completion and drops what
/// it built, which is the cancel — nothing joins it.
fn wait_for_dial(handle: &std::thread::JoinHandle<Result<session::Connected>>, events: &mut sdl2::EventPump) -> bool {
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
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// How many copies of a mid-stream `GamepadArrival` to send — see its one caller.
const ARRIVAL_SENDS: usize = 3;

/// `DualSense` HID feedback (adaptive triggers, lightbar), opened only for a pad kind that emits
/// it — anything else never sends these events. Not found (USB pad, no `luna-send-pub`) isn't an
/// error: logged once, the feature just stays off.
fn open_ds_feedback(
    kind: store::GamepadType,
    coils: Option<std::sync::Arc<crate::session::pad_audio::Envelope>>,
) -> Option<crate::platform::webos::dualsense::Feedback> {
    if !kind.is_dualsense() {
        return None;
    }
    if let Some(addr) = crate::platform::webos::dualsense::find_address() {
        return crate::platform::webos::dualsense::Feedback::new(addr, coils);
    }
    // A wired pad is not on `bluetooth2`, but the kernel gives it a hidraw node and the same
    // effects reach it as a 48-byte `0x02` report. Audio does not come this way: those lanes ride
    // the pad's own UAC card, so the coil envelope is left with the motors here.
    if let Some(node) = crate::platform::webos::hidraw::Hidraw::find_dualsense() {
        return crate::platform::webos::dualsense::Feedback::new_usb(node);
    }
    tracing::info!(
        "no DualSense on the Bluetooth service or on a hidraw node — \
         adaptive triggers off for this session"
    );
    None
}

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

    // Owned above the loop, not re-declared per iteration: `ControllerDeviceAdded` fires only
    // once per physical (re)connection, so a pad opened earlier must carry across screens.
    let mut controller: Option<GameController> = None;
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
                &mut controller,
                &identity,
                menu_toast.take().or_else(|| menu_status.take()),
            )?
        } else {
            run_ui_flow(
                &mut canvas,
                &mut console_gl,
                &mut events,
                &game_controller,
                &mut controller,
                &identity,
                display_mode,
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
                        let stage = crate::session::audio::AudioStage::new(std::sync::Arc::new(sink), channels)?;
                        Ok((player, connected.spawn_audio_feed(stage)?))
                    }) {
                        Ok(pair) => Some(pair),
                        Err(e) => {
                            // Same no-crash policy as the connect above, plus the video teardown a
                            // loaded decoder now needs.
                            tracing::error!("audio player init failed: {e:#}");
                            connected.disconnect_quit();
                            if connected.shutdown() {
                                crate::platform::webos::ndl::quit();
                            } else {
                                tracing::warn!("session teardown timed out — skipping NDL unload for this run");
                            }
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

            // Pad audio (`0xD1`): a pad's render caps ride its arrival, so the session-default
            // DualSense still needs one declared at start. Only toward a host that has the plane —
            // an older host reads arrival flags as the bare pad index.
            let pad_audio_caps = if connected.client.host_caps() & punktfunk_core::quic::HOST_CAP_PAD_AUDIO != 0 {
                // The speaker lane needs a transport that can play it, and there are two: a paired
                // pad takes `0x36` reports on the Luna bus, a wired one has its own 4-channel card.
                let speaker_route = crate::platform::webos::dualsense::find_address().is_some()
                    || crate::platform::webos::usb_audio::card_present();
                crate::session::pad_audio::caps_for(&settings, speaker_route)
            } else {
                0
            };
            let haptics = (pad_audio_caps != 0).then(crate::session::pad_audio::Envelope::new);
            let mut pad_audio_thread = None;
            if let Some(envelope) = &haptics {
                connected.client.set_pad_audio_caps(0, pad_audio_caps);
                if settings.gamepad_type().is_dualsense() {
                    if let Some(ev) = gamepad::arrival_event(settings.gamepad_type(), 0, pad_audio_caps) {
                        for _ in 0..ARRIVAL_SENDS {
                            connected.send_input(&ev);
                        }
                    }
                }
                match crate::session::pad_audio::spawn(
                    connected.client.clone(),
                    connected.stop.clone(),
                    envelope.clone(),
                ) {
                    Ok(handle) => pad_audio_thread = Some(handle),
                    Err(e) => tracing::warn!("pad audio off: {e:#}"),
                }
            }
            // A wired pad plays both lanes on its own card. Started only when no paired pad is around:
            // that one carries the lanes over Bluetooth, and two writers would fight for the coils.
            let usb_audio_thread = match (&haptics, crate::platform::webos::dualsense::find_address()) {
                (Some(envelope), None) => {
                    crate::platform::webos::usb_audio::spawn(envelope.clone(), connected.stop.clone())
                }
                _ => None,
            };
            // See `open_ds_feedback`. The envelope rides along so a Bluetooth pad plays the coil lane
            // itself (tier A); it is harmless on the spawn route, which never claims it.
            let mut ds_feedback = open_ds_feedback(settings.gamepad_type(), haptics.clone());
            // The pad kind the host currently builds for pad 0: the handshake default until a
            // controller plugged in mid-stream declares another one.
            let mut declared_pad = settings.gamepad_type();

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
            let hid = crate::platform::webos::evdev::HidInput::start(true, settings.cursor_capture(), move |report| {
                use crate::platform::webos::evdev::HidReport;
                match report {
                    HidReport::Input(ev) => input.send(ev),
                    HidReport::Rich(rich) => input.send_rich(rich),
                }
            });
            // Flips once a HID mouse is found — `HidInput::start` no longer scans before returning
            // (that blocked every stream connect on the node-open cost), so presence is only known
            // once the reader thread's own scan catches up; checked each tick below.
            let mut hid_device_seen = false;
            // Stats overlay: refreshed ~2Hz onto the transparent stream window, over the
            // punch-through video plane via per-pixel alpha — window is never shown/hidden (that
            // crashed an earlier attempt, see docs/NOTES.md). Green button flips it live, session-only.
            // The pumps read this too — with nothing on glass to show them, the video thread skips
            // every counter the overlay is the only reader of. Seeded here; from the first tick on it
            // is DERIVED from the fade below rather than set alongside the toggle, so there is one
            // writer and no second copy of the state to keep in step.
            let mut stats_enabled = settings.stats_overlay();
            connected.stats().set_diagnostics(stats_enabled);
            // Fades in/out on the same curve as the toast below — see `ModalFade::visibility_alpha`.
            let mut stats_fade = crate::ui::fade::ModalFade::<()>::overlay();
            if stats_enabled {
                stats_fade.open();
            }
            let mut log_fade = crate::ui::fade::ModalFade::<()>::overlay();
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
            // Edge-detects `stats.holding` (freeze-until-reanchor — see `session::pump`'s video pump) so a
            // packet-loss stall surfaces as a toast even with the stats overlay off, same signal the
            // overlay's "Beat" line already reads.
            let mut was_holding = false;
            let mut overlay_was_active = false;
            // The stats card's lines and the log tail, rebuilt on their 500 ms cadence and drawn
            // as they stand on every overlay frame between.
            let mut stats_lines: Vec<String> = Vec::new();
            let mut log_lines: Vec<String> = Vec::new();
            let mut stats_built_at: Option<Instant> = None;
            let mut overlay_last: Option<Instant> = None;
            let mut overlay_prev_frames: u64 = 0;
            let mut overlay_prev_bytes: u64 = 0;
            let mut overlay_prev_cpu_ticks: Option<u64> = None;
            let mut overlay_prev_at = Instant::now();
            // 0 = "Disconnect" focused, 1 = "Cancel" (default on open — safer).
            let mut disconnect = ConfirmDialog::new(
                "Stop streaming?",
                "The stream will end and you'll return to the menu.",
                Some(crate::app::view::icons::ICON_CLOSE),
                "Stop streaming",
                crate::app::screens::confirm::Tone::Danger,
            );
            // Gamepad routes to the disconnect dialog — see `DisconnectChord`.
            let mut chord = DisconnectChord::default();
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
                            if controller.is_none() {
                                match game_controller.open(which) {
                                    Ok(c) => {
                                        tracing::info!("controller connected: {}", c.name());
                                        controller = Some(c);
                                    }
                                    Err(e) => tracing::warn!("controller open failed: {e}"),
                                }
                            }
                            // Outside the open, as in the menu loop: only the first pad becomes
                            // `controller`, but a later one is still what `detect_type` names.
                            // Under `Automatic` the handshake settled the pad kind from whatever was
                            // attached at connect time — usually nothing — so a pad arriving now has
                            // to declare itself or the host keeps driving its default Xbox pad. An
                            // explicit pick needs none of this: the session default already is it.
                            let kind = if gamepad_auto {
                                gamepad::detect_type(&game_controller).unwrap_or_default()
                            } else {
                                declared_pad
                            };
                            if kind != declared_pad {
                                let caps = if kind.is_dualsense() { pad_audio_caps } else { 0 };
                                if let Some(ev) = gamepad::arrival_event(kind, 0, caps) {
                                    tracing::info!("pad 0 is {kind:?} (hotplug) — declaring it to the host");
                                    // Sent by hand rather than by the core's own arrival path, so it
                                    // carries no retransmit of its own; input rides unreliable
                                    // datagrams. Re-declaring is idempotent host-side.
                                    for _ in 0..ARRIVAL_SENDS {
                                        connected.send_input(&ev);
                                    }
                                    declared_pad = kind;
                                    // Adaptive triggers/lightbar follow the kind the host now builds.
                                    ds_feedback = kind
                                        .is_dualsense()
                                        .then(|| open_ds_feedback(kind, haptics.clone()))
                                        .flatten();
                                }
                            }
                        }
                        Event::ControllerDeviceRemoved { .. } => {
                            controller = None;
                            // An unplugged pad sends no releases, so a held chord would stay armed forever.
                            chord.clear();
                        }
                        // Dialog open: navigate it only, don't forward input to the host.
                        _ if disconnect.is_open() => {
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
                        // Scancode keys are real game input — forward only, never open the dialog.
                        Event::KeyDown { scancode: Some(sc), .. } if !hid_keys => {
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
                            // webOS requires a rectangle before enabling the IME.
                            let w = 400i32.min(display_mode.w);
                            text_input.raise(sdl2::rect::Rect::new(
                                (display_mode.w - w) / 2,
                                display_mode.h - 120,
                                w as u32,
                                60,
                            ));
                        }
                        // Magic Remote Back has no scancode — forwarded as Esc. A held Back never
                        // arrives here; webOS delivers it as the EXIT gesture polled below instead.
                        Event::KeyDown {
                            keycode: Some(k),
                            scancode: None,
                            repeat: false,
                            ..
                        } if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                            if let Some(ev) = keyboard::key_event(sdl2::keyboard::Scancode::Escape, true) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::KeyUp {
                            keycode: Some(k),
                            scancode: None,
                            ..
                        } if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) => {
                            if let Some(ev) = keyboard::key_event(sdl2::keyboard::Scancode::Escape, false) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::KeyUp { scancode: Some(sc), .. } if !hid_keys => {
                            if let Some(ev) = keyboard::key_event(sc, false) {
                                connected.send_input(&ev);
                            }
                        }
                        Event::ControllerButtonDown { button, .. } => {
                            chord.set(button, true);
                            // Still forwarded: the hold requirement is what keeps game input and
                            // the shortcut apart.
                            let ev = gamepad::button_event(button, true, 0);
                            connected.send_input(&ev);
                        }
                        Event::ControllerButtonUp { button, .. } => {
                            chord.set(button, false);
                            let ev = gamepad::button_event(button, false, 0);
                            connected.send_input(&ev);
                        }
                        Event::ControllerAxisMotion { axis, value, .. } => {
                            let ev = gamepad::axis_event(axis, value, 0);
                            connected.send_input(&ev);
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
                // An open dialog swallows pointer input from here on, so no release ever arrives for
                // whatever is down — the same trap `DisconnectChord::clear` covers for the pad. Done
                // here rather than at each `open` site so every path into the dialog is covered.
                // Otherwise: a held OK commits to a drag once `DRAG_HOLD` is up, and a stationary
                // hold emits no events at all, so this tick is the only thing that can notice.
                if disconnect.is_open() {
                    buttons.release_held(|ev| connected.send_input(ev));
                } else {
                    buttons.tick(|ev| connected.send_input(ev));
                }
                // Chord held long enough — open the dialog, then forget it so it fires once per hold.
                if !disconnect.is_open() && chord.held_for(EXIT_HOLD) {
                    tracing::info!("disconnect shortcut held — opening dialog");
                    chord.clear();
                    disconnect.open(1);
                }
                // EXIT gesture (held Back) opens the dialog; a short tap is Esc, above.
                if exit_gesture_fired(&mut exit_held) && !disconnect.is_open() {
                    tracing::info!("EXIT gesture — opening disconnect dialog");
                    disconnect.open(1);
                }
                // Re-opens the webOS launcher; a long Back fires EXIT above, never this.
                if home_key_fired(&mut home_held) {
                    crate::platform::webos::luna::launch_home();
                }
                // SDL2 lacks these colour scancodes. Ignore them while the dialog owns input.
                let dialog_open = disconnect.is_open();
                if rising_edge(!dialog_open && key_down(WEBOS_GREEN_SCANCODE), &mut green_held) {
                    stats_enabled = !stats_enabled;
                    overlay_last = None; // force an immediate redraw
                    if stats_enabled {
                        stats_fade.reopen();
                    } else {
                        stats_fade.close(());
                    }
                }
                if rising_edge(!dialog_open && key_down(WEBOS_YELLOW_SCANCODE), &mut yellow_held) {
                    let was_on = log_overlay_state() != LogOverlayState::Off;
                    cycle_log_overlay();
                    let now_on = log_overlay_state() != LogOverlayState::Off;
                    overlay_last = None; // force an immediate redraw with the new state
                    if now_on && !was_on {
                        log_fade.reopen();
                    } else if was_on && !now_on {
                        log_fade.close(());
                    }
                }
                // Connection-issue toast: fires on the rising edge of a freeze-until-reanchor hold
                // (dropped/gapped frames — see `session::pump`), which is the same "network
                // trouble" signal the stats overlay's "Beat" line reads, just edge-triggered here so
                // it's visible without the overlay open. No matching "recovered" toast — the video
                // itself resuming is the recovery signal.
                let holding_now = connected.stats().holding.load(Ordering::Relaxed);
                if holding_now && !was_holding {
                    tracing::warn!("connection issues detected (freeze-until-reanchor)");
                    notif.show("Connection issues — recovering...");
                    overlay_last = None;
                }
                was_holding = holding_now;
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
                    hid.set_active(!disconnect.is_open());
                }
                // Wider than `is_open()`: a dismissed dialog still draws (fading out) a few more
                // ticks, used below to skip the stats overlay for exactly those ticks.
                let dialog_frame = disconnect.frame();
                if dialog_frame.is_some() {
                    // Own pass over the punch-through video: the dialog alone, on a transparent
                    // clear (NDL video is on a hardware plane below this surface, so no blur).
                    overlay::frame(
                        &mut console_gl,
                        &canvas,
                        &overlay_fonts,
                        display,
                        overlay::TRANSPARENT,
                        |f| {
                            disconnect.draw(f);
                        },
                    )?;
                } else if disconnect.tick() {
                    // Close-fade just finished. Confirmed Disconnect: break now, nothing to wipe
                    // since the pre-stream UI takes the canvas next.
                    if let Some(outcome) = pending_outcome.take() {
                        break 'running outcome;
                    }
                    // Cancel/Back: wipe the last frame so it doesn't stick over the video.
                    overlay::wipe(&mut console_gl, &canvas, &overlay_fonts)?;
                }
                // Audio drains on its own threads either way now — the software path on
                // `session::pump`'s feed thread into SDL's audio callback, the offloaded path on
                // its NDL audio pump. Nothing for this loop to do.
                //
                // Unconditional so both feedback planes keep draining with no pad attached.
                connected.pump_feedback_once(controller.as_mut(), ds_feedback.as_mut(), haptics.as_deref());
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
                let log_overlay_on = log_overlay_state() != LogOverlayState::Off;
                let log_alpha = log_fade.visibility_alpha(log_overlay_on);
                let overlay_active = stats_alpha.is_some() || log_alpha.is_some() || notif_active;
                if overlay_was_active && !overlay_active {
                    // Nothing else clears this window — the faded-out card would stick otherwise.
                    overlay::wipe(&mut console_gl, &canvas, &overlay_fonts)?;
                }
                overlay_was_active = overlay_active;
                // A fade in flight needs frequent frames; steady-state stats/log are fine at ~2Hz.
                let fading = notif_active || stats_fade.is_animating() || log_fade.is_animating();
                let redraw_interval = if fading {
                    Duration::from_millis(33)
                } else {
                    Duration::from_millis(500)
                };
                if overlay_active
                    && dialog_frame.is_none()
                    && overlay_last.is_none_or(|t| t.elapsed() >= redraw_interval)
                {
                    overlay_last = Some(Instant::now());
                    // Content stays on a 500ms cadence even when a toast fade runs the loop faster.
                    if stats_enabled && stats_built_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(500)) {
                        stats_built_at = Some(Instant::now());
                        let frames = connected.stats().frames.load(Ordering::Relaxed);
                        let bytes = connected.stats().bytes.load(Ordering::Relaxed);
                        let dt = overlay_prev_at.elapsed().as_secs_f32().max(0.001);
                        let fps = (frames.saturating_sub(overlay_prev_frames)) as f32 / dt;
                        // Measured, vs. negotiated `resolved_bitrate_kbps`.
                        let actual_kbps = (bytes.saturating_sub(overlay_prev_bytes)) as f32 * 8.0 / 1000.0 / dt;
                        overlay_prev_frames = frames;
                        overlay_prev_bytes = bytes;
                        overlay_prev_at = Instant::now();
                        let info = connected.overlay_info();
                        let feed_ms = connected.stats().feed_us.load(Ordering::Relaxed) as f32 / 1000.0;
                        let holding = connected.stats().holding.load(Ordering::Relaxed);
                        // CPU% (one core) + RSS, only read while the overlay is up.
                        let cpu_mem_line = device::process_cpu_mem().map(|(cpu_ticks, mem_bytes)| {
                            // No baseline on the first sample, so CPU shows from the 2nd on.
                            let cpu = overlay_prev_cpu_ticks.map(|prev| {
                                let pct =
                                    (cpu_ticks.saturating_sub(prev)) as f32 / device::clock_ticks_per_sec() as f32 / dt
                                        * 100.0;
                                format!("CPU {pct:.0}% · ")
                            });
                            overlay_prev_cpu_ticks = Some(cpu_ticks);
                            format!(
                                "{}RAM {:.0} MB",
                                cpu.unwrap_or_default(),
                                mem_bytes as f32 / (1024.0 * 1024.0)
                            )
                        });
                        let mut lines = vec![
                            format!(
                                "{}x{}@{} {}{}",
                                info.width,
                                info.height,
                                info.refresh_hz,
                                info.codec,
                                if info.hdr { " HDR" } else { "" },
                            ),
                            format!("Video {fps:.1} fps · {frames} frames"),
                            {
                                // NDL's undecoded/unpresented depth: rising means decode is behind,
                                // flat-near-zero while stuttering means the problem is upstream.
                                let backlog = connected.stats().render_backlog.load(Ordering::Relaxed);
                                let backlog = if backlog < 0 {
                                    "n/a".to_string()
                                } else {
                                    backlog.to_string()
                                };
                                // "n/a" rather than 0 where there is no such counter — a zero would
                                // read as "no loss", which is a different claim.
                                let or_na = |v: Option<u64>| v.map_or_else(|| "n/a".to_string(), |v| v.to_string());
                                format!(
                                    "Drop {} · FEC {} · hold {} · buf {backlog}",
                                    or_na(info.frames_dropped),
                                    or_na(info.fec_recovered),
                                    if holding { "yes" } else { "no" },
                                )
                            },
                            format!(
                                "Feed {feed_ms:.1} ms · {:.0}/{} Mbps",
                                actual_kbps / 1000.0,
                                info.target_kbps / 1000,
                            ),
                        ];
                        // Audio's own line. Before this the plane published nothing a surface could
                        // render, so "the audio is late" had no instrument behind it at all — and on
                        // this client the A/V offset is the number that says whether the sync loop is
                        // working. `buf` is what is queued ahead of the speaker; `A/V` is positive when
                        // audio plays BEHIND the picture. Both read 0 until the loop has evidence
                        // (100 observations, and a frame on the glass to compare against).
                        //
                        // Which decoder is running leads the line: the two paths fail differently
                        // (HW plays or is silent with nothing to measure; SW underruns visibly in
                        // `buf`), so reading the numbers without knowing which one produced them
                        // has already cost real debugging time. HW carries no ring and no sync loop
                        // of its own — NDL owns both — so the two figures are omitted there rather
                        // than printed as a pair of zeroes that look like a stalled plane.
                        let layout = connected.audio_layout();
                        if connected.audio_route.on_ndl_plane() {
                            // `lead` is how far the plane's stamps run ahead of NDL's player clock —
                            // the one figure this route does publish, and the one that matters most:
                            // NDL paces the PICTURE on that depth, so a lead sagging towards zero
                            // reads as video stutter, not as an audio fault (see `PLANE_LEAD_MS`).
                            lines.push(format!(
                                "{} {layout} · NDL · lead {} ms",
                                connected.audio_route.overlay_tag(),
                                connected.stats().audio_plane_lead_ms.load(Ordering::Relaxed),
                            ));
                        } else {
                            lines.push(format!(
                                "{} {layout} · buf {} ms",
                                connected.audio_route.overlay_tag(),
                                connected.audio_buffer_ms()
                            ));
                        }
                        // `late` is the judder, counted: frames NDL was handed too late to pace them.
                        // `jitter` is what the cadence loop sizes its cushion from.
                        let late = connected.stats().pacing_late.load(Ordering::Relaxed);
                        lines.push(format!(
                            "Pace jitter {:.1} ms · late {late}",
                            connected.stats().pacing_jitter_us.load(Ordering::Relaxed) as f32 / 1000.0,
                        ));
                        if let Some(line) = cpu_mem_line {
                            lines.push(line);
                        }
                        stats_lines = lines;
                    }
                    // `None` during fade-out once the toggle flips Off — the fade keeps drawing
                    // the last lines read.
                    if let Some(lines) = log_overlay_lines() {
                        log_lines = lines;
                    }
                    overlay::frame(
                        &mut console_gl,
                        &canvas,
                        &overlay_fonts,
                        display,
                        overlay::TRANSPARENT,
                        |f| {
                            if let Some(alpha) = stats_alpha {
                                overlay::stats(f, &stats_lines, "Press green button to hide this overlay", alpha);
                            }
                            if let Some(alpha) = log_alpha {
                                overlay::log(f, &log_lines, alpha);
                            }
                            if let Some((text, alpha)) = &notif_frame {
                                overlay::toast(f, text, *alpha);
                            }
                        },
                    )?;
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
                std::thread::sleep(Duration::from_millis(2));
            };
            text_input.stop();

            // Trigger resistance is firmware state that outlives the session — hand the pad back
            // first or a game that ended with R2 stiff leaves it stiff on the TV home screen.
            if let Some(mut fb) = ds_feedback.take() {
                fb.release();
            }
            // Rumble is likewise pad state, not stream state.
            if let Some(pad) = controller.as_mut() {
                let _ = pad.set_rumble(0, 0, 0);
            }
            if let Some(handle) = pad_audio_thread.take() {
                connected.stop.store(true, Ordering::Relaxed);
                let _ = handle.join();
            }
            // The wired writer blocks in `snd_pcm_writei`, so it wakes at most a chunk (5 ms) after
            // the stop flag — the card is closed on the way out, which is what releases the pad.
            if let Some(handle) = usb_audio_thread {
                connected.stop.store(true, Ordering::Relaxed);
                let _ = handle.join();
            }
            // Stop feeding before the transport goes away, and drop the device with it. Ordered ahead
            // of `shutdown()` only for tidiness — the feed thread also exits on the session's stop flag
            // and on the audio plane closing, and a late `try_send` into a dropped ring is a no-op.
            if let Some((player, feed_thread)) = audio {
                connected.stop_audio_feed(feed_thread);
                drop(player);
            }
            // `shutdown()` joins the video thread and drops `client` so the QUIC close frame
            // actually sends. `false` means a teardown thread is wedged in FFI — skip the NDL
            // unload rather than race it, and accept the leak for this run.
            if connected.shutdown() {
                crate::platform::webos::ndl::quit();
            } else {
                tracing::warn!("session teardown timed out — skipping NDL unload for this run");
            }
            // Put the TV's picture/sound modes back (no-op unless game mode switched them).
            crate::platform::webos::game_mode::restore(restore_tv_modes);
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
    if let Some(plan) = exit_plan {
        plan.run();
    }
    tracing::info!("punktfunk-webos exiting cleanly");
    Ok(())
}
