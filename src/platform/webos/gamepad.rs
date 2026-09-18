//! Maps SDL3 `Gamepad` events to punktfunk wire `InputEvents`.
//! Per-transition events (universally compatible).
use punktfunk_core::input::{gamepad, InputEvent, InputKind};
use sdl3::gamepad::{Axis, Button};

/// SDL3's `Button` enum (26 variants) → punktfunk's `BTN_*` wire bit, or `None` for a button
/// the wire has no bit for (see the `Misc2..Misc6` arm) — same shape as [`mouse::button_code`],
/// so a button with nothing to send is dropped by the `if let` every input source already uses
/// rather than by remembering to test a sentinel.
///
/// [`mouse::button_code`]: crate::platform::webos::mouse::button_code
pub fn button_bit(button: Button) -> Option<u32> {
    Some(match button {
        Button::South => gamepad::BTN_A,
        Button::East => gamepad::BTN_B,
        Button::West => gamepad::BTN_X,
        Button::North => gamepad::BTN_Y,
        Button::Back => gamepad::BTN_BACK,
        Button::Guide => gamepad::BTN_GUIDE,
        Button::Start => gamepad::BTN_START,
        Button::LeftStick => gamepad::BTN_LS_CLICK,
        Button::RightStick => gamepad::BTN_RS_CLICK,
        Button::LeftShoulder => gamepad::BTN_LB,
        Button::RightShoulder => gamepad::BTN_RB,
        Button::DPadUp => gamepad::BTN_DPAD_UP,
        Button::DPadDown => gamepad::BTN_DPAD_DOWN,
        Button::DPadLeft => gamepad::BTN_DPAD_LEFT,
        Button::DPadRight => gamepad::BTN_DPAD_RIGHT,
        Button::Misc1 => gamepad::BTN_MISC1,
        Button::RightPaddle1 => gamepad::BTN_PADDLE1,
        Button::LeftPaddle1 => gamepad::BTN_PADDLE2,
        Button::RightPaddle2 => gamepad::BTN_PADDLE3,
        Button::LeftPaddle2 => gamepad::BTN_PADDLE4,
        Button::Touchpad => gamepad::BTN_TOUCHPAD,
        // SDL3 grew Misc2..Misc6 (extra vendor buttons: a Switch pad's capture key and the
        // like). punktfunk's wire has one `BTN_MISC1` and no bit to carry them, so there is
        // nothing to send for these.
        Button::Misc2 | Button::Misc3 | Button::Misc4 | Button::Misc5 | Button::Misc6 => return None,
    })
}

/// Sony's USB vendor id, and the `DualSense` Edge's product id. SDL types both Edge and plain
/// `DualSense` as `PS5` — the ids are what tells them apart, and they are what the kernel's own
/// `hid-playstation` driver matches on too (see [`crate::platform::webos::dualsense`]).
const SONY_VID: u16 = 0x054c;
const DUALSENSE_EDGE_PID: u16 = 0x0df2;

/// The kind to present for a pad SDL has already identified from its controller database, or
/// `None` to leave the choice to the host.
///
/// `SDL_GetGamepadType` is the authority here, answering from SDL's own VID/PID-keyed database.
/// Not the product string: substring-matching it misses every pad whose name does not spell its
/// family out ("Wireless Controller" is what a stock `DualShock` 4 calls itself over Bluetooth)
/// and mis-hits anything that happens to contain the word. [`type_for_name`] stays behind it for
/// a pad that database has never seen.
///
/// Only pads the Xbox default actually misrepresents are mapped. An Xbox pad, or anything
/// unrecognized, stays `None`: the host's default is already right for the former, and for
/// the latter naming a specific backend the host may not be able to build is worse than
/// letting it choose.
fn kind_for(
    sdl_type: sdl3::gamepad::GamepadType,
    ids: Option<(u16, u16)>,
    name: &str,
) -> Option<crate::services::store::GamepadType> {
    use crate::services::store::GamepadType;
    use sdl3::gamepad::GamepadType as T;
    match sdl_type {
        T::PS5 if ids == Some((SONY_VID, DUALSENSE_EDGE_PID)) => Some(GamepadType::DualSenseEdge),
        // An Edge SDL knows as a PS5 pad but whose ids did not come back (a Bluetooth link that
        // reports neither) still names itself one.
        T::PS5 => Some(type_for_name(name).unwrap_or(GamepadType::DualSense)),
        T::PS4 => Some(GamepadType::DualShock4),
        T::NintendoSwitchPro | T::NintendoSwitchJoyconPair => Some(GamepadType::SwitchPro),
        // Not in SDL's database: fall back to what the pad calls itself.
        T::Unknown | T::Standard => type_for_name(name),
        _ => None,
    }
}

/// The kind to present for an open pad — what `Automatic` mirrors.
pub fn kind_of(pad: &sdl3::gamepad::Gamepad) -> Option<crate::services::store::GamepadType> {
    let name = pad.name().unwrap_or_default();
    kind_for(pad.r#type(), pad.vendor_id().zip(pad.product_id()), &name)
}

/// The same, for a pad SDL has enumerated but nothing has opened. SDL3 answers all three from
/// the instance id, so a probe no longer has to take the pad's one slot to ask.
pub fn kind_at(
    subsystem: &sdl3::GamepadSubsystem,
    id: sdl3::joystick::JoystickId,
) -> Option<crate::services::store::GamepadType> {
    let name = subsystem.name_for_id(id).unwrap_or_default();
    kind_for(
        subsystem.type_for_id(id),
        subsystem.vendor_for_id(id).zip(subsystem.product_for_id(id)),
        &name,
    )
}

/// The controller kind to present to the host when Settings says `Automatic`, derived from
/// whichever attached pad SDL recognizes first — `None` to leave the choice to the host.
///
/// `Automatic` used to send wire `GamepadPref::Auto`, which means *the host* picks, and the
/// host picks an Xbox 360 pad. So a `DualSense` owner who never opened Settings held a
/// `DualSense` while the game saw an Xbox pad: wrong glyphs, and — the reason this matters —
/// no adaptive-trigger effects at all, since a game only emits those for a `DualSense`
/// ([`crate::platform::webos::dualsense`]).
pub fn detect_type(subsystem: &sdl3::GamepadSubsystem) -> Option<crate::services::store::GamepadType> {
    subsystem
        .gamepads()
        .ok()?
        .into_iter()
        .find_map(|id| kind_at(subsystem, id))
}

/// Maps a controller's own product string to the kind to present, for a pad SDL's database has
/// no entry for. Behind [`kind_for`], never on its own.
pub fn type_for_name(name: &str) -> Option<crate::services::store::GamepadType> {
    use crate::services::store::GamepadType;
    let name = name.to_ascii_lowercase();
    // Edge before plain: the Edge's SDL name contains "dualsense" too, so testing the
    // broader pattern first would silently downgrade every Edge to a plain DualSense.
    if name.contains("dualsense edge") {
        Some(GamepadType::DualSenseEdge)
    } else if name.contains("dualsense") {
        Some(GamepadType::DualSense)
    } else if name.contains("dualshock") || name.contains("ps4 controller") {
        Some(GamepadType::DualShock4)
    } else if name.contains("switch pro") || name.contains("pro controller") {
        Some(GamepadType::SwitchPro)
    } else {
        None
    }
}

/// Whether an SDL controller is really this TV's own remote rather than a game pad.
///
/// webOS presents the Magic Remote as a game controller — it enumerates as `Smart Remote RCU
/// Input` — so anything that trusts SDL's device list reports a pad on a set where none is
/// plugged in. That is not cosmetic in the shared shell: a non-empty pad list picks the
/// button-glyph legend AND moves the home screen's Options and Settings off the d-pad onto X
/// and Y, which a remote does not have.
///
/// Matched by name because SDL offers nothing else to tell them apart; both spellings are
/// checked since the remote's product string has varied across webOS releases.
pub fn is_tv_remote(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("remote") || name.contains("rcu")
}

/// Whether the controller with SDL instance id `id` is [the TV's remote](is_tv_remote). Neither
/// loop opens it: it would take the one pad slot and leave the real pad closed.
pub fn is_remote_at(subsystem: &sdl3::GamepadSubsystem, id: sdl3::joystick::JoystickId) -> bool {
    subsystem.name_for_id(id).is_ok_and(|name| is_tv_remote(&name))
}

/// Whether a real game pad is attached — what the "With a controller" console-UI mode reads.
///
/// [`is_tv_remote`] is the whole point of the filter: every webOS set enumerates its own remote
/// as a controller, so trusting SDL's list would make that mode mean "always" on every TV.
pub fn any_pad_connected(subsystem: &sdl3::GamepadSubsystem) -> bool {
    subsystem
        .gamepads()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| subsystem.name_for_id(id).ok())
        .any(|name| !is_tv_remote(&name))
}

/// Declares pad `pad`'s kind to the host mid-session, for a controller plugged in AFTER the
/// handshake: the session default was settled from whatever was attached at connect time, so a
/// `DualSense` connected mid-stream would otherwise drive the host's default Xbox pad — wrong
/// glyphs and no adaptive triggers. `None` for `Auto` (nothing to declare; the host's own choice
/// is what `Auto` means). Hosts without `HOST_CAP_GAMEPAD_STATE` ignore the tag.
pub fn arrival_event(kind: crate::services::store::GamepadType, pad: u8, audio_caps: u8) -> Option<InputEvent> {
    let pref = kind.to_core();
    if pref == punktfunk_core::config::GamepadPref::Auto {
        return None;
    }
    Some(InputEvent {
        kind: InputKind::GamepadArrival,
        _pad: [0; 3],
        code: u32::from(pref.to_u8()),
        x: 0,
        y: 0,
        // `audio_caps` (`session::pad_audio::CAP_*`) rides bits 8/9 toward a pad-audio host —
        // the caller passes 0 for any host or pad kind that has no lane to render.
        flags: punktfunk_core::input::encode_gamepad_arrival(pad, audio_caps),
    })
}

/// Pad `pad` is gone: the host tears down its virtual device and frees the index. The core stamps
/// the removal's seq.
pub fn remove_event(pad: u8) -> InputEvent {
    InputEvent {
        kind: InputKind::GamepadRemove,
        _pad: [0; 3],
        code: 0,
        x: 0,
        y: 0,
        flags: punktfunk_core::input::encode_gamepad_remove(pad, 0),
    }
}

/// SDL3's `Axis` enum → punktfunk's `AXIS_*` wire id.
fn axis_id(axis: Axis) -> u32 {
    match axis {
        Axis::LeftX => gamepad::AXIS_LS_X,
        Axis::LeftY => gamepad::AXIS_LS_Y,
        Axis::RightX => gamepad::AXIS_RS_X,
        Axis::RightY => gamepad::AXIS_RS_Y,
        Axis::TriggerLeft => gamepad::AXIS_LT,
        Axis::TriggerRight => gamepad::AXIS_RT,
    }
}

/// One `BTN_*` wire bit's edge on pad `pad`.
pub fn bit_event(bit: u32, pressed: bool, pad: u8) -> InputEvent {
    InputEvent {
        kind: InputKind::GamepadButton,
        _pad: [0; 3],
        code: bit,
        x: if pressed { 1 } else { 0 },
        y: 0,
        flags: u32::from(pad),
    }
}

/// SDL3 sticks are already i16 (−32768..32767) matching the wire's range, so X passes
/// straight through. Y does not: confirmed on-device (`DualSense` over Bluetooth, this
/// webOS/Linux SDL build) that pushing a stick up/forward reports a *negative* raw
/// value — the opposite of the wire's XInput/Moonlight "+y = up" convention — so both
/// sticks' Y axes are negated before sending (`saturating_neg` since raw `i16::MIN`
/// has no positive counterpart in range). Triggers arrive as SDL's 0..32767 range —
/// punktfunk wants 0..255, so those are rescaled.
pub fn axis_event(axis: Axis, value: i16, pad: u8) -> InputEvent {
    let scaled = match axis {
        Axis::TriggerLeft | Axis::TriggerRight => (i32::from(value) * 255) / 32767,
        Axis::LeftY | Axis::RightY => i32::from(value.saturating_neg()),
        _ => i32::from(value),
    };
    InputEvent {
        kind: InputKind::GamepadAxis,
        _pad: [0; 3],
        code: axis_id(axis),
        x: scaled,
        y: 0,
        flags: u32::from(pad),
    }
}
