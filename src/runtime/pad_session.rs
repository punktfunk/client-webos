//! A pad's part in a running stream: what the host is told it is, the input it must let go of,
//! and its own `DualSense` effects and audio lanes. The slot table itself is [`super::pads`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::pads::{Pads, Slot};
use crate::platform::webos::dualsense::{self, Feedback, Link};
use crate::platform::webos::{gamepad, usb_audio};
use crate::services::join::{join_with_timeout, SHUTDOWN_JOIN_TIMEOUT};
use crate::services::store::{GamepadType, Settings};
use crate::session::pad_audio::{self, Envelope, Envelopes};
use crate::session::Connected;

/// How many copies of an arrival to send: it rides unreliable datagrams with no retransmit.
const ARRIVAL_SENDS: usize = 3;

/// Every pad axis, for releasing a pad and for re-sending held sticks.
const PAD_AXES: [sdl3::gamepad::Axis; 6] = [
    sdl3::gamepad::Axis::LeftX,
    sdl3::gamepad::Axis::LeftY,
    sdl3::gamepad::Axis::RightX,
    sdl3::gamepad::Axis::RightY,
    sdl3::gamepad::Axis::TriggerLeft,
    sdl3::gamepad::Axis::TriggerRight,
];

/// One `DualSense`'s own effects and audio lanes. Dropping it hands the pad back.
#[derive(Default)]
pub(super) struct Extras {
    pub(super) feedback: Option<Feedback>,
    pub(super) audio: Option<PadAudio>,
    usb_audio: Option<UsbWriter>,
}

impl Extras {
    /// Hands this pad back before the caller takes it again, waiting — but never past
    /// [`SHUTDOWN_JOIN_TIMEOUT`]. The wait is what keeps a new link from racing the release of the
    /// old one; the ceiling is because that release rides Bluetooth replies and a card write, and
    /// this runs on the stream loop. Past the ceiling the handback finishes on its own thread.
    pub(super) fn hand_back(&mut self) {
        // As `retire`: unfiled here, because the index is taken again as soon as this returns.
        self.audio = None;
        let extras = std::mem::take(self);
        if extras.feedback.is_none() && extras.usb_audio.is_none() {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("pad-handback".into())
            .spawn(move || drop(extras));
        if let Ok(handle) = spawned {
            join_with_timeout(handle, SHUTDOWN_JOIN_TIMEOUT, "pad-handback", || ());
        }
    }

    /// Unfiles this pad's audio now and finishes handing it back on a thread of its own.
    pub(super) fn retire(&mut self) {
        // Unfiled here, not on the thread: the index may be taken again before that thread runs.
        self.audio = None;
        let extras = std::mem::take(self);
        if extras.feedback.is_some() || extras.usb_audio.is_some() {
            let _ = std::thread::Builder::new()
                .name("pad-retire".into())
                .spawn(move || drop(extras));
        }
    }
}

impl Drop for Extras {
    fn drop(&mut self) {
        // Trigger resistance and the lightbar are firmware state that outlives the session.
        if let Some(feedback) = self.feedback.as_mut() {
            feedback.release();
        }
    }
}

/// This pad's coil/speaker envelope, filed in the decode thread's registry for as long as it lives.
pub(super) struct PadAudio {
    pub(super) envelope: Arc<Envelope>,
    registry: Arc<Envelopes>,
    pad: u8,
}

impl PadAudio {
    fn register(registry: &Arc<Envelopes>, pad: u8) -> Self {
        let envelope = Envelope::new();
        registry.insert(pad, envelope.clone());
        Self {
            envelope,
            registry: registry.clone(),
            pad,
        }
    }
}

impl Drop for PadAudio {
    fn drop(&mut self) {
        self.registry.remove(self.pad, &self.envelope);
    }
}

/// A wired pad's card writer, on its own stop so an unplug ends it mid-session.
struct UsbWriter {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for UsbWriter {
    fn drop(&mut self) {
        // The writer blocks in `snd_pcm_writei`, so it wakes at most a chunk after the flag.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Declares pad `id` to the host and, for a real `DualSense`, starts its own effects and audio
/// lanes on its own link — so several pads each get theirs. The pad-audio caps ride the arrival,
/// which is why the two happen together. `registry` is `None` when the session renders no pad audio.
pub(super) fn bring_up(
    connected: &Connected,
    pads: &mut Pads,
    id: sdl3::joystick::JoystickId,
    setting: GamepadType,
    settings: &Settings,
    registry: Option<&Arc<Envelopes>>,
) {
    let Some(slot) = pads.get_mut(id) else { return };
    // Hands back whatever link this pad held before, so the new one does not race it.
    slot.extras.hand_back();
    let link = if slot.is_dualsense(setting) {
        let link = dualsense::link_for(slot.path.as_deref(), slot.serial.as_deref());
        if link.is_none() {
            tracing::info!(
                "pad {}: no Bluetooth or hidraw route to this DualSense — adaptive triggers off",
                slot.index
            );
        }
        link
    } else {
        None
    };
    // A wired pad's own card: the speaker lane needs a transport that can play it, and a paired
    // pad takes `0x36` reports on the Luna bus instead.
    let card = match &link {
        Some(Link::Usb(node)) => usb_audio::find_card(node.usb_path().as_deref()),
        _ => None,
    };
    // Haptics need no transport — without one the coils are rendered as motor rumble — so only the
    // speaker waits on a link that can play it.
    let caps = if registry.is_some() && slot.is_dualsense(setting) {
        pad_audio::caps_for(settings, matches!(link, Some(Link::Bluetooth(_))) || card.is_some())
    } else {
        0
    };
    if connected.client.host_caps() & punktfunk_core::quic::HOST_CAP_PAD_AUDIO != 0 {
        // Always set: wire indices are reused, and a stale bit would stick to the next pad.
        connected.client.set_pad_audio_caps(slot.index, caps);
    }
    slot.declare(connected, setting, caps);

    slot.extras.audio = registry
        .filter(|_| caps != 0)
        .map(|r| PadAudio::register(r, slot.index));
    let envelope = slot.extras.audio.as_ref().map(|a| a.envelope.clone());
    match link {
        // The envelope rides along so a Bluetooth pad plays the coil lane itself (tier A).
        Some(Link::Bluetooth(address)) => {
            tracing::info!("pad {}: DualSense over Bluetooth ({address})", slot.index);
            slot.extras.feedback = Feedback::new(address, envelope);
        }
        // Audio does not ride hidraw: a wired pad plays both lanes on its own card.
        Some(Link::Usb(node)) => {
            let path = node.path.clone();
            tracing::info!("pad {}: DualSense over USB ({path})", slot.index);
            slot.extras.feedback = Feedback::new_usb(node);
            if let (Some(envelope), Some(card)) = (envelope, card) {
                let stop = Arc::new(AtomicBool::new(false));
                slot.extras.usb_audio = usb_audio::spawn(envelope, stop.clone(), card, path).map(|thread| UsbWriter {
                    stop,
                    thread: Some(thread),
                });
            }
        }
        None => {}
    }
}

impl Slot {
    /// Tells the host what this pad is when that differs from what it last heard. Nonzero `caps`
    /// always re-declare: they ride the arrival.
    fn declare(&mut self, connected: &Connected, setting: GamepadType, caps: u8) {
        let kind = self.host_kind(setting);
        if self.declared == Some(kind) && caps == 0 {
            return;
        }
        let Some(ev) = gamepad::arrival_event(kind, self.index, caps) else {
            return;
        };
        tracing::info!("pad {} is {kind:?} — declaring it to the host", self.index);
        // Sent by hand rather than by the core's own arrival path, so it carries no retransmit of
        // its own. Re-declaring is idempotent host-side.
        for _ in 0..ARRIVAL_SENDS {
            connected.send_input(&ev);
        }
        self.declared = Some(kind);
    }

    /// A new session: the handshake built pad 0 as `handshake_kind` and nothing else yet, and no
    /// shortcut or dial state carries over.
    pub(super) fn begin_session(&mut self, handshake_kind: GamepadType) {
        self.declared = (self.index == 0).then_some(handshake_kind);
        self.chord.clear();
        self.dial.clear();
    }

    /// Lifts every button and axis this pad holds on the host.
    pub(super) fn release_held(&mut self, connected: &Connected) {
        let held = self.dial.opened();
        for bit in (0..32).map(|i| 1u32 << i).filter(|bit| held & bit != 0) {
            connected.send_input(&gamepad::bit_event(bit, false, self.index));
        }
        for axis in PAD_AXES {
            connected.send_input(&gamepad::axis_event(axis, 0, self.index));
        }
    }

    /// Re-sends where the sticks and triggers are now: SDL only reports changes, so a stick held
    /// through a release would stay dead on the host.
    pub(super) fn resend_sticks(&self, connected: &Connected) {
        for axis in PAD_AXES {
            connected.send_input(&gamepad::axis_event(axis, self.pad.axis(axis), self.index));
        }
    }
}
