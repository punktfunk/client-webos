//! Where each SDL event goes while a stream runs: the host, the dial, the stop dialog, or
//! nowhere — and which of them are the compositor's echo of input `evdev` already sent.

use std::ops::ControlFlow;

use super::*;
use crate::core::dial::PadRoute;
use crate::runtime::overlay::ConfirmAction;
use pf_client_core::ring::RingInput;
use sdl3::event::Event;

impl Stream {
    /// Routes one SDL event. `now` is the tick's, for the remote gate; `osk` is whether the
    /// on-screen keyboard is up, whose keys have no node of their own.
    pub(super) fn handle_event(
        &mut self,
        cx: &mut Cx<'_>,
        event: Event,
        now: Instant,
        osk: bool,
    ) -> ControlFlow<StreamOutcome> {
        // Never real pointer input, so never the host's — see `mouse::is_touch_emulated`.
        if mouse::is_touch_emulated(&event) {
            return ControlFlow::Continue(());
        }
        // Which SDL events are the compositor's echo of input this app already read off
        // evdev. Owning the pointer node is the whole answer for buttons — it's decided
        // in `evdev` (Capture, and whether the keyboard shares the node), so it isn't
        // re-derived from the setting here. Keys go by recency instead, so the Magic
        // Remote's keys — which never appear on a node we hold — still pass. Read once
        // per event, not per guard: the window is 250ms, so per-arm freshness buys
        // nothing.
        let (hid_motion, hid_clicks, hid_keys) = match self.hid.as_ref() {
            Some(hid) => {
                let pointer = hid.has_mouse();
                // A keypress with the pointer left to the compositor still moves it:
                // webOS warps to screen centre, which would drag the host cursor along.
                let keys = hid.keyboard_busy();
                (pointer || keys, pointer, keys)
            }
            None => (false, false, false),
        };
        // Preserve HID ownership: outside local overlays its scancode events
        // must not consume a remote press. All other events visit the gate once.
        let hid_owned_key = hid_keys
            && !self.disconnect.is_open()
            && !self.ring.open()
            && matches!(
                event,
                Event::KeyDown { scancode: Some(_), .. } | Event::KeyUp { scancode: Some(_), .. }
            );
        let key_admitted = !hid_owned_key && self.remote_gate.admits(&event, now);
        let remote = self.remote_keys.edge(&event, key_admitted);
        let remote_press = remote.and_then(|(key, down)| down.then_some(key));
        match event {
            Event::Quit { .. } => {
                cx.connected.disconnect_quit();
                return ControlFlow::Break(StreamOutcome::Quit);
            }
            // Home or a TV panel took focus: releases for what is held go elsewhere,
            // so the host would keep them down.
            Event::Window {
                win_event: sdl3::event::WindowEvent::FocusLost,
                ..
            } => {
                tracing::debug!("focus lost — releasing held input");
                self.focused = false;
                self.release_all(cx);
            }
            Event::Window {
                win_event: sdl3::event::WindowEvent::FocusGained,
                ..
            } => self.focused = true,
            Event::GamepadAdded { which, .. } => {
                // Under `Automatic` the handshake settled the pad kind from whatever was
                // attached at connect time — usually nothing — so a pad arriving now has
                // to declare itself or the host keeps driving its default Xbox pad.
                if let Some(id) = cx.pads.add(cx.game_controller, which).map(|slot| slot.id) {
                    pad_session::bring_up(
                        cx.connected,
                        cx.pads,
                        id,
                        self.kind_setting,
                        cx.settings,
                        self.pad_audio.as_ref(),
                    );
                    cx.pads.publish_routes(&self.pad_routes);
                }
            }
            // Only pads we hold: the Magic Remote drops and re-adds constantly.
            Event::GamepadRemoved { which, .. } => {
                if let Some(mut slot) = cx.pads.remove(which) {
                    cx.pads.publish_routes(&self.pad_routes);
                    // A dial tap's owed release would re-create the removed pad on the host.
                    if self.tap_up.is_some_and(|(_, pad, _)| pad == slot.index) {
                        self.tap_up = None;
                    }
                    // An unplugged pad sends no releases: lift what the host holds, then
                    // free the index so a replug or another pad can take it.
                    slot.release_held(cx.connected);
                    cx.connected.send_input(&gamepad::remove_event(slot.index));
                    // Handing a vanished pad back can wait on a Bluetooth reply or a card
                    // write that will not come; the stream loop must not.
                    slot.extras.retire();
                }
            }
            // Dialog open: navigate it only, don't forward input to the host. A key only if
            // the remote pressed it: a pad's echo would move it a second time.
            _ if self.disconnect.is_open() => {
                if key_admitted {
                    match self
                        .disconnect
                        .handle_event(&event, remote_press, cx.fonts, cx.display.0, cx.display.1)
                    {
                        Some(ConfirmAction::Confirmed) => {
                            tracing::info!("disconnecting to menu");
                            self.client_initiated_disconnect = true;
                            cx.connected.disconnect_quit();
                            self.disconnect.dismiss();
                            self.pending_outcome = Some(StreamOutcome::ReturnToMenu);
                        }
                        Some(ConfirmAction::Dismissed) => self.hud.redraw_now(),
                        Some(ConfirmAction::Navigated) | None => {}
                    }
                }
            }
            // Dial open: the remote's keys drive it, and no key or pointer input reaches
            // the host. The pad drives it through `dial`, so its echo must not.
            // Back first: it closes the dial and SDL3 gives it no keycode.
            _ if self.ring.open() && remote_press == Some(RemoteKey::Back) && key_admitted => {
                self.ring.menu(MenuEvent::Back);
            }
            Event::KeyDown {
                keycode: Some(k),
                repeat: false,
                ..
            } if self.ring.open() && key_admitted => {
                if let Some(ev) = crate::platform::webos::input::menu_event_for_key(k) {
                    self.ring.menu(ev);
                }
            }
            _ if self.ring.open() && (event.is_keyboard() || event.is_text() || event.is_mouse()) => {}
            // Scancode keys are real game input — forward only, never open the dialog.
            Event::KeyDown { scancode: Some(sc), .. } if !hid_keys && (key_admitted || osk) => {
                if let Some(ev) = keyboard::key_event(sc, true) {
                    cx.connected.send_input(&ev);
                }
            }
            // The Magic Remote's own keys, all matched on the event's `raw` — SDL3
            // gives them no scancode and no keycode, so every arm that reads those
            // misses them (see `RemoteKey`). A HELD Back never arrives at all;
            // webOS turns that into the EXIT gesture polled separately.
            _ if remote.is_some() => {
                let Some((key, down)) = remote else {
                    return ControlFlow::Continue(());
                };
                match key {
                    // Forwarded as Esc. `remote_keys.edge` already refused a pad
                    // echoing Back, so it never reaches the host as a phantom press.
                    RemoteKey::Back => {
                        if let Some(ev) = keyboard::key_event(sdl3::keyboard::Scancode::Escape, down) {
                            cx.connected.send_input(&ev);
                        }
                    }
                    // The right button. BOTH edges matter — a press whose release is
                    // dropped leaves the host holding the button down.
                    RemoteKey::Red => self.buttons.red(down, |ev| cx.connected.send_input(ev)),
                    RemoteKey::Green if down => self.green_pressed = true,
                    RemoteKey::Yellow if down => self.yellow_pressed = true,
                    RemoteKey::Blue if down => self.blue_pressed = true,
                    _ => {}
                }
            }
            // Forward composed IME text, which has no scancode.
            Event::TextInput { text, .. } => {
                for ev in keyboard::text_key_events(&text) {
                    cx.connected.send_input(&ev);
                }
            }
            Event::KeyUp { scancode: Some(sc), .. } if !hid_keys && (key_admitted || osk) => {
                if let Some(ev) = keyboard::key_event(sc, false) {
                    cx.connected.send_input(&ev);
                }
            }
            Event::GamepadButtonDown { which, button, .. } | Event::GamepadButtonUp { which, button, .. } => {
                let down = matches!(event, Event::GamepadButtonDown { .. });
                let open = self.ring.open();
                let Some(slot) = cx.pads.get_mut(which) else {
                    return ControlFlow::Continue(());
                };
                if !open {
                    slot.chord.set(button, down);
                }
                // A button the wire has no bit for is not forwarded and cannot
                // work the chord either.
                let Some(bit) = gamepad::button_bit(button) else {
                    return ControlFlow::Continue(());
                };
                // Forwarded buttons still reach the host: the hold requirement is what
                // keeps game input and the disconnect shortcut apart.
                match slot.dial.button(bit, down, open) {
                    PadRoute::Forward => {
                        cx.connected.send_input(&gamepad::bit_event(bit, down, slot.index));
                    }
                    PadRoute::Open => {
                        self.ring.set_facts(&live::ring_facts(
                            cx.settings,
                            cx.connected,
                            self.hud.tier(),
                            self.native_mode,
                            true,
                        ));
                        self.ring.input(RingInput::Toggle {
                            x: cx.display.0 as f32 / 2.0,
                            y: cx.display.1 as f32 / 2.0,
                        });
                    }
                    PadRoute::Menu(ev) => {
                        self.ring.menu(ev);
                    }
                    PadRoute::Drop => {}
                }
            }
            Event::GamepadAxisMotion { which, axis, value, .. } => {
                let open = self.ring.open();
                let Some(slot) = cx.pads.get_mut(which) else {
                    return ControlFlow::Continue(());
                };
                if matches!(axis, sdl3::gamepad::Axis::LeftX | sdl3::gamepad::Axis::LeftY) {
                    if let Some(ev) = slot.dial.left_stick(axis == sdl3::gamepad::Axis::LeftX, value, open) {
                        self.ring.menu(ev);
                    }
                }
                if !open {
                    cx.connected.send_input(&gamepad::axis_event(axis, value, slot.index));
                }
            }
            // Magic Remote pointer mode surfaces as plain SDL3 mouse events, forwarded
            // to the host instead of driving local UI focus (see `mouse.rs`).
            Event::MouseMotion { x, y, xrel, yrel, .. } => {
                // SDL3 gives f32; wire and injector take whole pixels.
                let (x, y) = (x as i32, y as i32);
                if !hid_motion {
                    // Relative only for the remote alone: SDL's warp emulation is off
                    // whenever the evdev reader owns motion, so the remote sends
                    // absolute — also the better fit for a device the user aims.
                    let relative = cx.settings.cursor_capture() && !self.hid_device_seen;
                    let (xrel, yrel) = if relative {
                        self.relative_motion.take(xrel, yrel)
                    } else {
                        (0, 0)
                    };
                    let ev = if relative {
                        mouse::move_relative_event(xrel, yrel)
                    } else {
                        mouse::move_event(x, y, cx.display.0, cx.display.1)
                    };
                    cx.connected.send_input(&ev);
                }
            }
            Event::MouseButtonDown { mouse_btn, .. } if !hid_clicks => {
                if let Some(ev) = mouse::button_event(mouse_btn, true) {
                    cx.connected.send_input(&ev);
                }
            }
            Event::MouseButtonUp { mouse_btn, .. } if !hid_clicks => {
                if let Some(ev) = mouse::button_event(mouse_btn, false) {
                    cx.connected.send_input(&ev);
                }
            }
            // integer_x/y, not x/y: the wire takes whole detents. Un-flipped to the
            // physical direction: the host applies its own scroll preference, as it
            // does for evdev. Local menus keep SDL's flip, which is the user's choice.
            Event::MouseWheel {
                integer_x,
                integer_y,
                direction,
                ..
            } if !hid_clicks => {
                let sign = if direction == sdl3::mouse::MouseWheelDirection::Flipped {
                    -1
                } else {
                    1
                };
                let (x, y) = (integer_x * sign, integer_y * sign);
                if y != 0 {
                    cx.connected.send_input(&mouse::scroll_event(y, false));
                }
                if x != 0 {
                    cx.connected.send_input(&mouse::scroll_event(x, true));
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}
