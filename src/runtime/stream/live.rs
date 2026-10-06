//! One stream, from the reveal to the teardown it hands off: [`App::stream`] builds the
//! [`Stream`], runs its [`Stream::tick`] every ~2 ms, and winds the session down behind the menu.
//!
//! A tick is a fixed sequence of steps, each its own method, run in this order:
//! 1. the quit signal, and a HID mouse coming up ([`Stream::adopt_hid_pointer`]);
//! 2. the remote's own nodes, then SDL's events ([`Stream::handle_event`]);
//! 3. the dial: its open/close edge, its facts and its commands ([`Stream::tick_ring`]);
//! 4. the stop dialog's openers and its hand-over of input ([`Stream::tick_dialog_shortcuts`]);
//! 5. the colour keys' actions and the hold toast ([`Stream::tick_remote_actions`]);
//! 6. pointer capture and the HID grab ([`Stream::sync_capture`]);
//! 7. the dialog's own frame ([`Stream::tick_dialog`]), pad feedback, the overlays
//!    ([`hud::Hud::draw`]);
//! 8. whether the session is over ([`Stream::ended`]).

use std::ops::ControlFlow;

use super::*;
use crate::platform::webos::input::{WEBOS_EXIT_SCANCODE, WEBOS_HOME_SCANCODE};
use pf_client_core::ring::{RingCommand, RingFacts, RingInput};
use punktfunk_core::hud::StatsVerbosity;

/// A synthetic system-button tap holds this long, so the host sees the press.
const TAP_PRESS: Duration = Duration::from_millis(50);
/// Longest a launch holds the menu frame waiting for the first frame to reach the decoder
/// before uncovering the video plane regardless. A host's first delivery can be seconds late
/// (its startup capacity probe, or a new UDP flow the AP holds — see `session`'s
/// `PROBE_WARMUP_CAP`), and until it lands the plane is black.
const FIRST_FRAME_WAIT: Duration = Duration::from_secs(6);

/// The first frame is on the plane, or it is late enough that a black plane beats a stale
/// menu: a host that never sends must not hold the reveal forever.
///
/// `presented` is the signal because NDL's own `PLAYING` lands during `load()`, before anything
/// is fed, and some sets report `LOADCOMPLETED` only once a frame has been.
pub(in crate::runtime) fn first_frame_ready(since: Instant) -> bool {
    crate::platform::webos::ndl::presented() || since.elapsed() >= FIRST_FRAME_WAIT
}

/// Blocks on [`first_frame_ready`] for a reconnect. The launch never comes here: the shell
/// keeps animating through that wait instead.
pub(super) fn wait_first_frame() {
    let started = Instant::now();
    while !first_frame_ready(started) {
        std::thread::sleep(Duration::from_millis(4));
    }
    log_reveal(started.elapsed());
}

/// `presented` and `playing` at the moment the video plane is uncovered.
pub(in crate::runtime) fn log_reveal(waited: Duration) {
    tracing::info!(
        "NDL reveal after {waited:?} (presented={} playing={})",
        crate::platform::webos::ndl::presented(),
        crate::platform::webos::ndl::playing(),
    );
}

impl App {
    /// Runs one stream on `connected`, the finished handshake `settings` was dialled with, and
    /// hands its teardown off. Leaves the reason on [`Self::menu_notice`] when it ends badly.
    pub(super) fn stream(
        &mut self,
        connected: session::Connected,
        settings: &store::Settings,
        gamepad_auto: bool,
    ) -> Ended {
        tracing::info!("session connected, entering event loop");
        // `hide()` unmaps the surface entirely, silently breaking the Magic Remote's pointer
        // forwarding since Wayland has nowhere left to route motion. aurora-tv never hides its
        // window either — stays mapped, cleared fully transparent so the video shows through.
        // Cosmetic like every overlay frame: a TV panel up at this moment must not end the app.
        // A wipe that could not draw stays owed (`hud::Hud`'s `was_active`).
        let initial_wipe = overlay::wipe(&mut self.console_gl, &self.window, &self.fonts);
        // Local pointer hidden unless "Cursor capture" is off — otherwise it and the host's own
        // forwarded-position cursor read as "the pointer doesn't match the mouse".
        let mut cursor = cursor::Cursor::new(self.sdl.mouse());
        cursor.set_captured(settings.cursor_capture(), &self.window);
        cursor.flush(&self.window, &self.events);

        // `None` when the session decodes audio somewhere other than here (punktfunk's NDL Opus
        // offload) — a second unfed audio device would still claim a PulseAudio sink.
        // The device is held here for the length of the stream — dropping it stops playback — while
        // the feed half moves to its own decode thread.
        let audio = match connected.audio_channels() {
            None => None,
            Some(channels) => {
                match crate::platform::webos::audio::AudioPlayer::new(
                    &self.sdl_audio,
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
                        // Same no-crash policy as the connect, plus the video teardown a loaded
                        // decoder now needs.
                        tracing::error!("audio player init failed: {e:#}");
                        connected.disconnect_quit();
                        connected.shutdown_and_quit();
                        cursor.set_captured(false, &self.window);
                        cursor.flush(&self.window, &self.events);
                        self.menu_notice = Some(format!("Couldn't start audio: {e:#}"));
                        return Ended {
                            outcome: StreamOutcome::ReturnToMenu,
                            lost: false,
                        };
                    }
                }
            }
        };

        // Pad audio (`0xD1`): each pad's render caps ride its arrival. Only toward a host that has
        // the plane — an older host reads arrival flags as the bare pad index.
        let pad_audio = (connected.client.host_caps() & punktfunk_core::quic::HOST_CAP_PAD_AUDIO != 0
            && crate::session::pad_audio::wanted(settings))
        .then(|| Arc::new(crate::session::pad_audio::Envelopes::default()));
        let mut pad_audio_thread = None;
        if let Some(envelopes) = &pad_audio {
            match crate::session::pad_audio::spawn(connected.client.clone(), connected.stop.clone(), envelopes.clone())
            {
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
        self.pads.sync(&self.game_controller);
        let pad_routes = pads::PadRoutes::default();
        self.pads.publish_routes(&pad_routes);
        for slot in self.pads.iter_mut() {
            slot.begin_session(settings.gamepad_type());
        }
        let ids: Vec<sdl3::joystick::JoystickId> = self.pads.iter().map(|slot| slot.id).collect();
        for id in ids {
            pad_session::bring_up(
                &connected,
                &mut self.pads,
                id,
                kind_setting,
                settings,
                pad_audio.as_ref(),
            );
        }

        // Keyboards are grabbed whatever Capture says, or the compositor sees Ctrl/Alt/Shift and
        // warps its pointer mid-click; mouse nodes follow Capture: on = exclusive relative grab,
        // off = compositor keeps the pointer to aim with.
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
        let mut stream = Stream {
            kind_setting,
            pad_audio,
            pad_routes,
            native_mode: connected.client.mode(),
            cursor,
            hid,
            hid_device_seen: false,
            remote_gate: RemoteGate::default(),
            remote_keys: RemoteKeys::default(),
            buttons: mouse::RemoteButtons::default(),
            relative_motion: mouse::RelativeMotion::default(),
            text_input: TextInputController::new(self.window.subsystem().text_input()),
            // Seeded from live key state, not `false`: these are rising-edge polls, and the launch
            // itself is a keypress. A key still down when the stream loop starts (webOS's EXIT
            // gesture in particular — a synthetic press whose key-up may never arrive) would read as
            // a fresh press on the first tick and, for EXIT, open the disconnect dialog over the
            // video the instant the stream began.
            home_held: key_down(WEBOS_HOME_SCANCODE),
            exit_held: key_down(WEBOS_EXIT_SCANCODE),
            green_pressed: false,
            yellow_pressed: false,
            blue_pressed: false,
            focused: true,
            input_suspended: false,
            tap_up: None,
            disconnect: ConfirmDialog::new(
                "Stop streaming?",
                "The stream will end and you'll return to the menu.",
                "Stop streaming",
            ),
            ring: pf_console_ui::Ring::new(),
            ring_was_open: false,
            ring_facts_at: None,
            dialog_was_open: false,
            ring_stats: false,
            hud: hud::Hud::new(settings, &connected, initial_wipe),
            pending_outcome: None,
            client_initiated_disconnect: false,
            lost: false,
            notice: None,
        };

        let outcome = {
            let mut cx = Cx {
                window: &self.window,
                gl: &mut self.console_gl,
                fonts: &self.fonts,
                display: self.display,
                pads: &mut self.pads,
                game_controller: &self.game_controller,
                settings,
                connected: &connected,
            };
            loop {
                if let ControlFlow::Break(outcome) = stream.tick(&mut cx, &mut self.events) {
                    break outcome;
                }
                // Bounds staleness of forwarded input/audio (video has its own thread). 2ms keeps
                // added latency near zero; the wakeup rate is noise even on this SoC.
                crate::platform::webos::input::wait_for_event(Duration::from_millis(2));
            }
        };
        if let Some(notice) = stream.notice.take() {
            self.menu_notice = Some(notice);
        }
        connected.release_input();
        stream.text_input.stop(&self.window);

        // Hand the Bluetooth links back to the TV's sniff policy.
        drop(pad_link);
        // Trigger resistance is firmware state that outlives the session — hand every pad back
        // first or a game that ended with R2 stiff leaves it stiff on the TV home screen. Dropping
        // the extras also stops each wired card writer. Rumble is likewise pad state.
        for slot in self.pads.iter_mut() {
            slot.extras = pad_session::Extras::default();
            if slot.rumble() {
                let _ = slot.pad.set_rumble(0, 0, 0);
            }
            if slot.triggers() {
                let _ = slot.pad.set_rumble_triggers(0, 0, 0);
            }
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
            tracing::info!("session torn down");
        });
        stream.cursor.set_captured(false, &self.window);
        stream.cursor.flush(&self.window, &self.events);
        Ended {
            outcome,
            lost: stream.lost,
        }
    }
}

impl Stream {
    /// One pass of the stream loop — see the module docs for the steps. `Break` ends the stream.
    fn tick(&mut self, cx: &mut Cx<'_>, events: &mut sdl3::EventPump) -> ControlFlow<StreamOutcome> {
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            tracing::warn!("SIGTERM/SIGINT received — disconnecting before exit");
            cx.connected.disconnect_quit();
            return ControlFlow::Break(StreamOutcome::Quit);
        }
        self.adopt_hid_pointer(cx, events);
        // The remote's presses, read before SDL's events so each is there to claim the key it
        // caused. A key the remote did not press is a pad's echo (webOS 23+), unless the
        // on-screen keyboard, which has no node of its own, typed it.
        let now = Instant::now();
        if let Some(hid) = self.hid.as_ref() {
            self.remote_gate.adopt(hid.take_remote_nodes());
        }
        self.remote_gate.poll(now);
        let osk = self.text_input.is_shown(cx.window);
        self.text_input.release_if_dismissed(osk, cx.window);
        for event in events.poll_iter() {
            self.handle_event(cx, event, now, osk)?;
        }
        self.tick_ring(cx)?;
        self.tick_dialog_shortcuts(cx);
        self.tick_remote_actions(cx, osk);
        self.sync_capture(cx);
        let dialog_shown = self.tick_dialog(cx)?;
        // Audio drains on its own threads either way now — the software path on
        // `session::pump`'s feed thread into SDL's audio callback, the offloaded path on
        // its NDL audio pump. Nothing for this loop to do.
        //
        // Unconditional so both feedback planes keep draining with no pad attached.
        cx.connected.pump_feedback_once(cx.pads);
        // Skipped while the dialog owns the canvas.
        self.hud.draw(cx, &mut self.ring, dialog_shown);
        self.ended(cx)
    }

    /// A HID mouse the reader's scan only now found: take the pointer off SDL's relative mode.
    fn adopt_hid_pointer(&mut self, cx: &Cx<'_>, events: &sdl3::EventPump) {
        if cx.settings.cursor_capture()
            && !self.hid_device_seen
            && self
                .hid
                .as_ref()
                .is_some_and(crate::platform::webos::evdev::HidInput::has_mouse)
        {
            self.hid_device_seen = true;
            // Re-applies the capture — and with it the compositor hide, which only sticks
            // now that the node is grabbed: the one at connect raced the reader's scan.
            self.cursor.disable_sdl_relative(cx.window);
            self.cursor.flush(cx.window, events);
        }
    }

    /// The dial: hand the pads over on its open/close edge, keep its facts current, and act on
    /// what it asks for.
    fn tick_ring(&mut self, cx: &mut Cx<'_>) -> ControlFlow<StreamOutcome> {
        // The dial took the pad: the host must see nothing held. On close the sticks are
        // re-sent, since SDL only reports changes and a held stick would stay dead there.
        let ring_open = self.ring.open();
        if ring_open != self.ring_was_open {
            self.ring_was_open = ring_open;
            self.ring_facts_at = None;
            if ring_open {
                self.release_all(cx);
            } else {
                for slot in cx.pads.iter_mut() {
                    slot.dial.closed();
                    // A dial the dialog cancelled hands the pad to the dialog, not the
                    // host: the dialog's own close re-sends them.
                    if !self.disconnect.is_open() {
                        slot.resend_sticks(cx.connected);
                    }
                }
            }
        }
        // Throttled: building them allocates, and this runs every 2 ms tick. A command or a fresh
        // open clears the stamp, so the dial never shows its own change late.
        if ring_open && self.ring_facts_at.is_none_or(|at| at.elapsed() >= RING_FACTS_EVERY) {
            self.ring_facts_at = Some(Instant::now());
            self.ring.set_facts(&ring_facts(
                cx.settings,
                cx.connected,
                self.hud.tier(),
                self.native_mode,
                !cx.pads.is_empty(),
                self.kind_setting,
            ));
        }
        self.ring.tick();
        while let Some(cmd) = self.ring.take_command() {
            tracing::info!(?cmd, "dial");
            self.ring_facts_at = None;
            self.ring_command(cx, cmd)?;
        }
        // Host actions: this client keeps no action cache, so their slots stay dimmed.
        drop(self.ring.take_cmds());
        if let Some((bit, pad, due)) = self.tap_up {
            if Instant::now() >= due {
                cx.connected.send_input(&gamepad::bit_event(bit, false, pad));
                self.tap_up = None;
            }
        }
        ControlFlow::Continue(())
    }

    /// One command off the dial.
    fn ring_command(&mut self, cx: &mut Cx<'_>, cmd: RingCommand) -> ControlFlow<StreamOutcome> {
        match cmd {
            RingCommand::EndStream => {
                cx.connected.disconnect_quit();
                return ControlFlow::Break(StreamOutcome::ReturnToMenu);
            }
            // No quit code: the host keeps the session for a reconnect.
            RingCommand::DisconnectLinger => {
                return ControlFlow::Break(StreamOutcome::ReturnToMenu);
            }
            RingCommand::CycleStats => self.ring_stats = true,
            RingCommand::Keyboard => {
                raise_keyboard(&mut self.text_input, cx.display, cx.window);
            }
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
                if let Err(e) = cx.connected.client.request_mode(mode) {
                    tracing::warn!("dial: mode request: {e}");
                }
            }
            RingCommand::Shortcut(keys) => send_shortcut(cx.connected, &keys),
            RingCommand::TapButton(bit) => {
                // Pad 0 may be unplugged while another pad holds the dial.
                let pad = cx.pads.first().map_or(0, |slot| slot.index);
                cx.connected.send_input(&gamepad::bit_event(bit, true, pad));
                self.tap_up = Some((bit, pad, Instant::now() + TAP_PRESS));
            }
            RingCommand::CyclePadMouse => {
                if let Err(e) = cx.connected.client.cycle_pad_mouse(u16::from(!cx.pads.is_empty())) {
                    tracing::warn!("dial: controller mouse: {e}");
                }
            }
            // This stream only: the next one starts from Settings.
            RingCommand::CyclePadType => {
                self.kind_setting = self.kind_setting.next_on_dial();
                pad_session::replug(
                    cx.connected,
                    cx.pads,
                    self.kind_setting,
                    cx.settings,
                    self.pad_audio.as_ref(),
                );
                cx.pads.publish_routes(&self.pad_routes);
            }
            // No microphone and no touch surface on a TV. Stream mute needs a zeroed
            // decoded frame, and NDL's audio plane decodes Opus itself. `ring_facts`
            // leaves `streamed_game` empty, so the dial never offers End game.
            RingCommand::ToggleMic
            | RingCommand::CycleTouchMode
            | RingCommand::ToggleStreamMute
            | RingCommand::EndGame { .. } => {}
            RingCommand::ToggleScrollInvert => {
                cx.connected
                    .client
                    .set_invert_scroll(!cx.connected.client.invert_scroll());
            }
        }
        ControlFlow::Continue(())
    }

    /// What opens the stop dialog — the pad chord, the EXIT gesture — and its hold on input,
    /// plus the remote's Home key.
    fn tick_dialog_shortcuts(&mut self, cx: &mut Cx<'_>) {
        // An open dialog swallows pointer input from here on, so no release ever arrives for
        // whatever is down. Done here rather than at each `open` site so every path into the
        // dialog is covered; the pads are handed over on the dialog's edge below.
        if self.disconnect.is_open() {
            if !self.input_suspended {
                cx.connected.release_input();
            }
            self.buttons.release_held(|ev| cx.connected.send_input(ev));
        }
        // Chord held long enough — open the dialog, then forget it so it fires once per hold.
        if !self.disconnect.is_open() && cx.pads.chord_held(EXIT_HOLD) {
            tracing::info!("disconnect shortcut held — opening dialog");
            cx.pads.clear_chords();
            self.disconnect.open(1, cx.fonts, cx.display);
        }
        // EXIT gesture (held Back) opens the dialog; a short tap is Esc (`events`).
        if exit_gesture_fired(&mut self.exit_held) && !self.disconnect.is_open() {
            tracing::info!("EXIT gesture — opening disconnect dialog");
            self.disconnect.open(1, cx.fonts, cx.display);
        }
        // The dialog owns input and the canvas, so the dial gives way to it.
        if self.disconnect.is_open() && self.ring.open() {
            self.ring.input(RingInput::Cancel);
        }
        // The dialog swallows every pad event while it is up, so it takes the pads the way
        // the dial does: whatever the host holds — the chord's own four buttons included —
        // is released on the way in. On the way out each pad's bookkeeping starts over, since
        // presses and releases made inside the dialog never reached it (a release the host
        // already had is harmless; a stale "held" bit would swallow the next real release),
        // and the sticks are re-sent because SDL reports only changes.
        let dialog_open = self.disconnect.is_open();
        if dialog_open != self.dialog_was_open {
            self.dialog_was_open = dialog_open;
            for slot in cx.pads.iter_mut() {
                if dialog_open {
                    slot.chord.clear();
                    slot.release_held(cx.connected);
                } else {
                    slot.dial.clear();
                    slot.resend_sticks(cx.connected);
                }
            }
        }
        // Re-opens the webOS launcher; a long Back fires EXIT above, never this.
        if home_key_fired(&mut self.home_held) {
            crate::platform::webos::luna::launch_home();
        }
    }

    /// The colour keys' actions, latched by the event arm, and the connection-issue toast.
    fn tick_remote_actions(&mut self, cx: &mut Cx<'_>, osk: bool) {
        let dialog_open = self.disconnect.is_open();
        // `|`, not `||`: the green key's edge must be read every tick, and so must the dial's.
        if (std::mem::take(&mut self.green_pressed) && !dialog_open) | std::mem::take(&mut self.ring_stats) {
            self.hud.cycle_stats();
        }
        if std::mem::take(&mut self.yellow_pressed) && !dialog_open {
            cycle_log_overlay();
            self.hud.redraw_now(); // with the new state
        }
        // Blue raises the IME, so a game needing text (chat, a search box) doesn't require
        // dropping back to the menu.
        //
        // Raise only, never dismiss. While the panel is
        // up webOS routes every remote key to it, so a second Blue is not something this
        // app can hear; Back is the way out and webOS's IME dismisses on it without being
        // told. A toggle here looks symmetric and cannot work.
        if std::mem::take(&mut self.blue_pressed) && !osk {
            raise_keyboard(&mut self.text_input, cx.display, cx.window);
        }
        self.hud.note_hold(cx.connected.stats().holding.load(Ordering::Relaxed));
    }

    /// Pointer capture and the HID grab follow the dialog, the dial and focus.
    fn sync_capture(&mut self, cx: &Cx<'_>) {
        // The dialog is navigated with the Magic Remote's pointer, so a captured stream
        // must hand the pointer back while it's up — hidden/relative there'd be nothing
        // to aim with. Recaptured on dismiss. The evdev reader releases its grabs for the
        // same window in either Capture mode — the dialog needs the remote's keys as much
        // as its pointer, and holding a grab would only leave a HID device dead meanwhile.
        let want_captured = cx.settings.cursor_capture() && !self.disconnect.is_open();
        if want_captured != self.cursor.is_captured() {
            self.cursor.set_captured(want_captured, cx.window);
        }
        if let Some(hid) = &self.hid {
            // Deactivating releases whatever HID held on the host.
            hid.set_active(self.focused && !self.disconnect.is_open() && !self.ring.open());
        }
        self.input_suspended = self.disconnect.is_open();
    }

    /// The stop dialog's own frame. `Continue(true)` while it has one on screen, which the
    /// overlays give way to; `Break` once a confirmed dialog has faded out.
    fn tick_dialog(&mut self, cx: &mut Cx<'_>) -> ControlFlow<StreamOutcome, bool> {
        // True during fade-out, past `is_open()`; gates the overlays.
        let animating = self.disconnect.tick();
        let shown = self.disconnect.frame().is_some();
        if shown && self.disconnect.redraw_due(animating) {
            // Own pass over the punch-through video: the dialog alone, on a transparent
            // clear (NDL video is on a hardware plane below this surface, so no blur).
            let disconnect = &self.disconnect;
            let frame = overlay::frame(cx.gl, cx.window, cx.fonts, cx.display, overlay::TRANSPARENT, |f| {
                disconnect.draw(f);
            });
            self.hud.drawn(frame);
        } else if !shown && animating {
            // Close-fade just finished. Confirmed Disconnect: end now, nothing to wipe
            // since the pre-stream UI takes the canvas next.
            if let Some(outcome) = self.pending_outcome.take() {
                return ControlFlow::Break(outcome);
            }
            // Cancel/Back: wipe the last frame so it doesn't stick over the video.
            self.hud.wipe(cx);
        }
        ControlFlow::Continue(shown)
    }

    /// The session is over without anyone here asking: the decoder died, or the host or the
    /// link ended it.
    fn ended(&mut self, cx: &Cx<'_>) -> ControlFlow<StreamOutcome> {
        // The decoder is gone for this load (`core::media::VideoSink::is_dead`, set by the
        // pump). The transport is still healthy, so nothing below would ever end the session
        // and the user would sit in front of a frozen picture — end it here instead.
        if cx.connected.stats().decoder_dead.load(Ordering::Relaxed) {
            tracing::error!("decoder failed for good — returning to the menu");
            self.notice = Some("Video decoder failed — session ended".to_string());
            return ControlFlow::Break(StreamOutcome::ReturnToMenu);
        }
        if cx.connected.is_session_ended() {
            let reason = cx.connected.end_reason();
            tracing::info!("session ended: {reason:?}");
            // Also flips true right after *our own* `disconnect_quit()` calls (Back/dialog,
            // SIGTERM) — no notice for those, the user just asked for it.
            if !self.client_initiated_disconnect {
                self.lost = reason == punktfunk_core::client::PunktfunkEndReason::Lost;
                self.notice = Some(cx.connected.end_message());
            }
            return ControlFlow::Break(StreamOutcome::ReturnToMenu);
        }
        ControlFlow::Continue(())
    }

    /// Lets go of everything the host holds from this client, and forgets it here too: for when
    /// the matching releases will not arrive (the dial took input, or focus left the app).
    pub(super) fn release_all(&mut self, cx: &mut Cx<'_>) {
        for slot in cx.pads.iter_mut() {
            slot.chord.clear();
            slot.release_held(cx.connected);
        }
        cx.connected.release_input();
        self.buttons.release_held(|ev| cx.connected.send_input(ev));
        self.remote_keys.reset();
    }
}

/// How stale the dial's facts may get while it is open: host-side changes (mode, grants) land
/// within this.
const RING_FACTS_EVERY: Duration = Duration::from_millis(100);

/// What the dial's slots read this frame. Controller mouse targets pad 0.
pub(super) fn ring_facts(
    settings: &store::Settings,
    connected: &session::Connected,
    stats: StatsVerbosity,
    native: punktfunk_core::config::Mode,
    pad: bool,
    pad_type: store::GamepadType,
) -> RingFacts {
    let c = &connected.client;
    let m = c.mode();
    RingFacts {
        overlay_actions: settings.overlay_actions.clone(),
        stats_tier: stats.label().into(),
        pad_mouse_target: u16::from(pad),
        pad_mouse: c.pad_mouse_mode(u16::from(pad)),
        invert_scroll: c.invert_scroll(),
        pointer_granted: c.access_grants() & punktfunk_core::quic::GRANT_POINTER != 0,
        pad_type: pad_type.to_core(),
        mode: (m.width, m.height, m.refresh_hz),
        native_mode: (native.width, native.height, native.refresh_hz),
        ..RingFacts::default()
    }
}

/// A dial shortcut: every key down in order, then up in reverse. A key this build cannot name
/// sends nothing, like the other clients.
fn send_shortcut(connected: &session::Connected, keys: &[String]) {
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
fn raise_keyboard(text_input: &mut TextInputController, display: (u32, u32), window: &sdl3::video::Window) {
    let (w, h) = (display.0 as i32, display.1 as i32);
    let width = 400i32.min(w);
    text_input.raise(
        sdl3::rect::Rect::new((w - width) / 2, h - 120, width as u32, 60),
        window,
    );
}
