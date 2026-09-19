//! The streaming loop's view of a live session.
//!
//! Inherent methods on [`Connected`] for the handful of figures the loop needs (stats, overlay,
//! teardown), kept here rather than in `session` because every one of them is shaped by what the
//! loop asks for, not by how the session works.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use punktfunk_core::client::NativeClient;
use punktfunk_core::input::InputEvent;

use crate::session::audio::AudioStage;
use crate::session::{self, Connected, StreamStats};

/// The input path, cloned for a thread that sends off the main loop (the HID-mouse reader).
#[derive(Clone)]
pub(crate) struct InputSender {
    client: Arc<NativeClient>,
    held: Arc<std::sync::Mutex<crate::core::input::HeldInputs>>,
}

/// The UI thread's own source id. Every evdev device takes a nonzero one (`evdev::Device::source`),
/// so a release from one input route never clears what another is holding.
pub(crate) const SOURCE_UI: u32 = 0;

type Held = std::sync::Mutex<crate::core::input::HeldInputs>;

/// Buttons route through the held-input ledger for 1:1 press/release; other events bypass it.
fn send_edge(client: &NativeClient, held: &Held, source: u32, ev: &InputEvent) {
    if crate::core::input::HeldInputs::is_edge(ev) {
        held.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send(source, ev, |ev| {
                let _ = client.send_input(ev);
            });
    } else {
        let _ = client.send_input(ev);
    }
}

/// Releases everything `source` still holds — the dialog gating input, or the device going away.
fn release_held(client: &NativeClient, held: &Held, source: u32) {
    held.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .release(source, |ev| {
            let _ = client.send_input(ev);
        });
}

impl InputSender {
    pub(crate) fn send(&self, source: u32, ev: &InputEvent) {
        send_edge(&self.client, &self.held, source, ev);
    }

    pub(crate) fn release(&self, source: u32) {
        release_held(&self.client, &self.held, source);
    }

    /// One pad touchpad contact or motion sample, on the rich-input plane the host applies to
    /// its virtual `DualSense` — the only route a swipe or gyro aim has, since neither has an
    /// `InputEvent` shape. Best-effort like every datagram, and a no-op toward a host running a
    /// different gamepad backend.
    pub(crate) fn send_rich(&self, rich: punktfunk_core::quic::RichInput) {
        let _ = self.client.send_rich_input(rich);
    }
}

impl Connected {
    pub(crate) fn input(&self) -> InputSender {
        InputSender {
            client: self.client.clone(),
            held: self.input_state.clone(),
        }
    }

    pub(crate) fn send_input(&self, ev: &InputEvent) {
        send_edge(&self.client, &self.input_state, SOURCE_UI, ev);
    }

    pub(crate) fn release_input(&self) {
        release_held(&self.client, &self.input_state, SOURCE_UI);
    }

    pub(crate) fn stats(&self) -> &Arc<StreamStats> {
        &self.stats
    }

    /// Whether HDR is being applied, for the Game-mode picture pick.
    pub(crate) fn hdr(&self) -> bool {
        self.hdr
    }

    /// Channels to open the SDL audio device with, or `None` when the session needs no local
    /// device (the stream rides NDL's audio plane — see `core::model::AudioRoutePref`).
    pub(crate) fn audio_channels(&self) -> Option<u8> {
        if self.audio_route.on_ndl_plane() {
            None
        } else {
            Some(self.client.audio_channels)
        }
    }

    /// The coupling the host encodes (`Welcome::audio_layout`, verbatim), for the audio stage.
    pub(crate) fn audio_layout_id(&self) -> u8 {
        self.client.audio_layout
    }

    /// The negotiated channel layout, for the overlay's audio line. Names the layout rather than
    /// the count — "5.1" is what the user picked in Settings, `6` is not.
    pub(crate) fn audio_layout(&self) -> &'static str {
        match self.client.audio_channels {
            1 => "1.0",
            2 => "2.0",
            3 => "2.1",
            4 => "4.0",
            6 => "5.1",
            8 => "7.1",
            _ => "?",
        }
    }

    /// The cells the A/V sync loop trades through — handed to the audio player at construction.
    /// Where the fallback ring publishes its depth for the stats overlay. Owned by
    /// `NativeClient` because the overlay reads it from there.
    pub(crate) fn audio_buffer_cell(&self) -> std::sync::Arc<std::sync::atomic::AtomicU32> {
        self.client.audio_buffer_ms_shared()
    }

    /// The fallback ring's depth in ms, for the HUD. Always `0` on the NDL routes — NDL owns the
    /// depth there and reports none.
    pub(crate) fn audio_buffer_ms(&self) -> u32 {
        self.client.audio_buffer_ms()
    }

    /// Starts the audio decode/feed thread. It exits on the session's stop flag, or when the
    /// transport's audio plane closes.
    pub(crate) fn spawn_audio_feed(&self, stage: AudioStage) -> anyhow::Result<std::thread::JoinHandle<()>> {
        session::spawn_audio_feed(self.client.clone(), stage, self.stop.clone())
    }

    /// Signals the audio feed thread to stop and joins it, bounded. Sets the session's stop flag,
    /// which `shutdown()` sets moments later anyway — doing it here just means the ring stops being
    /// fed before its device is dropped.
    pub(crate) fn stop_audio_feed(&self, handle: std::thread::JoinHandle<()>) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        session::join_audio_feed(handle);
    }

    pub(crate) fn is_session_ended(&self) -> bool {
        self.client.is_session_ended()
    }

    /// Why it ended; `Lost` is the one reason worth dialling again.
    pub(crate) fn end_reason(&self) -> punktfunk_core::client::PunktfunkEndReason {
        self.client.end_reason()
    }

    /// The sentence for the menu toast when the session ended on its own.
    pub(crate) fn end_message(&self) -> String {
        use punktfunk_core::client::PunktfunkEndReason as R;
        match self.end_reason() {
            R::GameExited => "The game exited",
            R::HostEnded => "The host ended the session",
            R::HostError => "The host closed the session with an error",
            R::Lost => "Connection lost",
            R::None | R::Local => "The host closed the connection",
        }
        .to_string()
    }

    pub(crate) fn disconnect_quit(&self) {
        self.client.disconnect_quit();
    }

    /// Close the overlay window: the connector's figures, decoded by NDL.
    pub(crate) fn hud_snapshot(&self) -> punktfunk_core::hud::StatsSnapshot {
        let mut snap = self.client.hud_snapshot();
        snap.decoder = "NDL".into();
        snap
    }

    /// Sample for the overlay only while it shows.
    pub(crate) fn set_hud_enabled(&self, on: bool) {
        self.client.set_hud_enabled(on);
    }
}

/// Ceiling on feedback events handled per tick.
///
/// Both planes are human-paced (a rumble change, a weapon swap), so this is never reached in
/// normal play — it exists so a host that floods, or a plane that backed up while a modal was
/// open, cannot starve rendering and input for a tick.
const FEEDBACK_DRAIN_BUDGET: usize = 32;

/// Sampling counter for the applied-rumble log. "It always runs at full level" is the usual
/// report, and nothing on this path could say whether the levels arrived that way or the pad's
/// force feedback dropped the magnitude — so sample what is actually written to the motors.
static RUMBLE_APPLIED: AtomicU32 = AtomicU32::new(0);

impl Connected {
    /// Drains the host→client gamepad feedback planes (non-blocking) and applies them to the
    /// physical pad. Call once per main-loop tick.
    ///
    /// Every command is routed by its pad index to that pad's own handle and `DualSense` link. The
    /// two planes go to different places, because each has one route that works for every
    /// controller rather than only one:
    ///   * **rumble** → SDL's evdev force feedback (`Gamepad::set_rumble`, plus
    ///     `set_rumble_triggers` for the impulse-trigger motors on pads that have them), which
    ///     works on any pad the TV has bound, except a Bluetooth `DualSense` on SDL's HIDAPI
    ///     driver (rumble needs enhanced reports, which stay off);
    ///   * **`DualSense` HID feedback** (adaptive triggers, lightbar, player LEDs) → the pad's
    ///     Bluetooth address or wired hidraw node (see [`crate::platform::webos::dualsense`]).
    ///
    /// Both drains run even when their sink is absent: the planes are bounded queues, and leaving
    /// one unread would let it fill and then discard the *newest* events — including, for rumble,
    /// the zero that stops a motor.
    pub(super) fn pump_feedback_once(&self, pads: &mut super::pads::Pads) {
        let client = &self.client;
        // `next_rumble_command` is the policy-engine API: it already resolves lease expiry, stale
        // legacy hosts and close-drain zeros, so commands apply verbatim — all-zero stops now.
        let mut budget = FEEDBACK_DRAIN_BUDGET;
        while budget > 0 {
            let Ok(cmd) = client.next_rumble_command(Duration::ZERO) else {
                break; // NoFrame (empty) or Closed (session over)
            };
            budget -= 1;
            let Some(slot) = pads.wire_mut(cmd.pad) else {
                continue;
            };
            // While coil frames arrive, the motors belong to the derived envelope: the host still
            // forwards the title's classic rumble, and applying both makes them fight.
            if slot.extras.audio.as_ref().is_some_and(|audio| audio.envelope.active()) {
                continue;
            }
            // SDL treats 0 as "until changed" not "stop now" — desired since the policy
            // engine sends explicit zeros to stop. Don't floor to avoid cutting held rumble short.
            if slot.rumble() && slot.pad.set_rumble(cmd.low, cmd.high, cmd.backstop_ms).is_ok() {
                let n = RUMBLE_APPLIED.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % 30 == 0 {
                    tracing::debug!(
                        "rumble applied #{n}: pad={} low={} high={} backstop={}ms",
                        cmd.pad,
                        cmd.low,
                        cmd.high,
                        cmd.backstop_ms
                    );
                }
            }
            // Dropping the trigger pair on a pad without those motors is the correct degrade;
            // folding it into the handles would turn a racing title's continuous trigger stream
            // into a handle motor droning flat-out for the whole race.
            if slot.triggers() {
                let _ = slot
                    .pad
                    .set_rumble_triggers(cmd.left_trigger, cmd.right_trigger, cmd.backstop_ms);
            }
        }

        for slot in pads.iter_mut() {
            if let Some((low, high)) = slot.extras.audio.as_ref().and_then(|a| a.envelope.take_change()) {
                // 0 = until changed; envelope sends the stop.
                if slot.rumble() {
                    let _ = slot.pad.set_rumble(low, high, 0);
                }
            }
        }

        let mut budget = FEEDBACK_DRAIN_BUDGET;
        while budget > 0 {
            let Ok(event) = client.next_hidout(Duration::ZERO) else {
                break;
            };
            budget -= 1;
            if let Some(feedback) = pads.wire_mut(event.pad()).and_then(|s| s.extras.feedback.as_mut()) {
                feedback.apply(&event);
            }
        }
    }
}
