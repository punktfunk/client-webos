//! The streaming loop's view of a live session.
//!
//! Inherent methods on [`Connected`] for the handful of figures the loop needs (stats, overlay,
//! teardown), kept here rather than in `session` because every one of them is shaped by what the
//! loop asks for, not by how the session works.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use punktfunk_core::input::InputEvent;

use crate::session::audio::AudioStage;
use crate::session::{self, Connected, StreamStats};

/// The input path, cloned for a thread that sends off the main loop (the HID-mouse reader).
#[derive(Clone)]
pub(crate) struct InputSender(Arc<punktfunk_core::client::NativeClient>);

impl InputSender {
    pub(crate) fn send(&self, ev: &InputEvent) {
        let _ = self.0.send_input(ev);
    }

    /// One pad touchpad contact or motion sample, on the rich-input plane the host applies to
    /// its virtual `DualSense` — the only route a swipe or gyro aim has, since neither has an
    /// `InputEvent` shape. Best-effort like every datagram, and a no-op toward a host running a
    /// different gamepad backend.
    pub(crate) fn send_rich(&self, rich: punktfunk_core::quic::RichInput) {
        let _ = self.0.send_rich_input(rich);
    }
}

impl Connected {
    pub(crate) fn input(&self) -> InputSender {
        InputSender(self.client.clone())
    }

    pub(crate) fn send_input(&self, ev: &InputEvent) {
        let _ = self.client.send_input(ev);
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

    /// The stats overlay's figures. Grouped into one call so the overlay block has one lookup
    /// rather than six.
    pub(crate) fn overlay_info(&self) -> OverlayInfo {
        let client = &self.client;
        let mode = client.mode();
        OverlayInfo {
            width: mode.width,
            height: mode.height,
            refresh_hz: mode.refresh_hz,
            codec: session::codec_name(client.codec).to_string(),
            hdr: client.color.is_hdr(),
            frames_dropped: Some(client.frames_dropped()),
            fec_recovered: Some(client.fec_recovered_shards()),
            // The CURRENT total wire budget, not the session-start negotiation: on Automatic the
            // ABR re-targets mid-session. Core v0.32 changed this from encoder rate to wire budget;
            // `0` means a host too old to report.
            target_kbps: match client.current_bitrate_kbps() {
                0 => client.resolved_bitrate_kbps,
                live => live,
            },
        }
    }
}

/// See [`Connected::overlay_info`].
pub(crate) struct OverlayInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub codec: String,
    pub hdr: bool,
    pub frames_dropped: Option<u64>,
    pub fec_recovered: Option<u64>,
    pub target_kbps: u32,
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
    /// The two planes go to different places, because each has one route that works for every
    /// controller rather than only one:
    ///   * **rumble** → SDL's evdev force feedback (`GameController::set_rumble`, plus
    ///     `set_rumble_triggers` for the impulse-trigger motors on pads that have them), which
    ///     works on any pad the TV has bound, `DualSense` included;
    ///   * **`DualSense` HID feedback** (adaptive triggers, lightbar, player LEDs) → the Bluetooth
    ///     service, since SDL's own `DualSense` path needs a hidraw node the app's jail doesn't
    ///     have (see [`crate::platform::webos::dualsense`]).
    ///
    /// Both drains run even when their sink is absent: the planes are bounded queues, and leaving
    /// one unread would let it fill and then discard the *newest* events — including, for rumble,
    /// the zero that stops a motor.
    pub(crate) fn pump_feedback_once(
        &self,
        mut controller: Option<&mut sdl2::controller::GameController>,
        mut feedback: Option<&mut crate::platform::webos::dualsense::Feedback>,
        haptics: Option<&crate::session::pad_audio::Envelope>,
    ) {
        let client = &self.client;
        // While coil frames arrive, the motors belong to the derived envelope: the host still
        // forwards the title's classic rumble, and applying both makes them fight.
        let haptics_own_motors = haptics.is_some_and(crate::session::pad_audio::Envelope::active);
        // `next_rumble_command` is the policy-engine API: it already resolves lease expiry, stale
        // legacy hosts and close-drain zeros, so commands apply verbatim — all-zero stops now.
        //
        // Queried once per tick, not per command: SDL walks its joystick list for this, and a hotplug
        // arrives as a new `GameController` rather than changing this answer mid-drain.
        let has_triggers = controller
            .as_deref()
            .is_some_and(sdl2::controller::GameController::has_rumble_triggers);
        let mut budget = FEEDBACK_DRAIN_BUDGET;
        while budget > 0 {
            let Ok(cmd) = client.next_rumble_command(Duration::ZERO) else {
                break; // NoFrame (empty) or Closed (session over)
            };
            budget -= 1;
            if haptics_own_motors {
                continue;
            }
            if let Some(pad) = controller.as_deref_mut() {
                // `backstop_ms` passes straight through, including 0: SDL2 reads a zero duration as
                // "no expiration" (`rumble_expiration = 0`, run until changed), not "stop now", which
                // is exactly the semantics wanted here — the policy engine guarantees an explicit
                // zero-level command at every stop, so a self-expiring effect would only risk
                // cutting a held rumble short. Don't "fix" this into a floor.
                //
                // Errors here are the common "this pad has no rumble motors" case, not a fault:
                // logging per command would spam a tick loop, and there is no recovery to attempt.
                let n = RUMBLE_APPLIED.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n % 30 == 0 {
                    tracing::debug!(
                        "rumble applied #{n}: low={} high={} lt={} rt={} backstop={}ms",
                        cmd.low,
                        cmd.high,
                        cmd.left_trigger,
                        cmd.right_trigger,
                        cmd.backstop_ms
                    );
                }
                let _ = pad.set_rumble(cmd.low, cmd.high, cmd.backstop_ms);
                // Dropping the trigger pair on a pad without those motors is the correct degrade;
                // folding it into the handles would turn a racing title's continuous trigger stream
                // into a handle motor droning flat-out for the whole race.
                if has_triggers {
                    let _ = pad.set_rumble_triggers(cmd.left_trigger, cmd.right_trigger, cmd.backstop_ms);
                }
            }
        }

        if let (Some(envelope), Some(pad)) = (haptics, controller) {
            if let Some((low, high)) = envelope.take_change() {
                // 0 = until changed; the envelope itself sends the zero that ends it.
                let _ = pad.set_rumble(low, high, 0);
            }
        }

        let mut budget = FEEDBACK_DRAIN_BUDGET;
        while budget > 0 {
            let Ok(event) = client.next_hidout(Duration::ZERO) else {
                break;
            };
            budget -= 1;
            if let Some(fb) = feedback.as_deref_mut() {
                fb.apply(&event);
            }
        }
    }
}
