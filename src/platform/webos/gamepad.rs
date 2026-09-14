//! Maps SDL2 `GameController` events to punktfunk wire `InputEvents`.
//! Per-transition events (universally compatible).
use punktfunk_core::input::{gamepad, InputEvent, InputKind};
use sdl2::controller::{Axis, Button};

/// SDL2's `Button` enum (exhaustively matched — all 20 current variants) → punktfunk's
/// `BTN_*` wire bit.
pub fn button_bit(button: Button) -> u32 {
    match button {
        Button::A => gamepad::BTN_A,
        Button::B => gamepad::BTN_B,
        Button::X => gamepad::BTN_X,
        Button::Y => gamepad::BTN_Y,
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
        Button::Paddle1 => gamepad::BTN_PADDLE1,
        Button::Paddle2 => gamepad::BTN_PADDLE2,
        Button::Paddle3 => gamepad::BTN_PADDLE3,
        Button::Paddle4 => gamepad::BTN_PADDLE4,
        Button::Touchpad => gamepad::BTN_TOUCHPAD,
    }
}

/// The controller kind to present to the host when Settings says `Automatic`, derived from
/// whichever attached pad SDL recognizes first — `None` to leave the choice to the host.
///
/// `Automatic` used to send wire `GamepadPref::Auto`, which means *the host* picks, and the
/// host picks an Xbox 360 pad. So a `DualSense` owner who never opened Settings held a
/// `DualSense` while the game saw an Xbox pad: wrong glyphs, and — the reason this matters —
/// no adaptive-trigger effects at all, since a game only emits those for a `DualSense`
/// ([`crate::platform::webos::dualsense`]).
///
/// Only pads the Xbox default actually misrepresents are mapped. An Xbox pad, or anything
/// unrecognized, stays `None`: the host's default is already right for the former, and for
/// the latter naming a specific backend the host may not be able to build is worse than
/// letting it choose.
pub fn detect_type(subsystem: &sdl2::GameControllerSubsystem) -> Option<crate::services::store::GamepadType> {
    let count = subsystem.num_joysticks().ok()?;
    (0..count)
        .filter(|&i| subsystem.is_game_controller(i))
        .filter_map(|i| subsystem.name_for_index(i).ok())
        .find_map(|name| type_for_name(&name))
}

/// Maps an SDL controller name to the kind to present. Names come from SDL's controller
/// database (`SDL_GameControllerNameForIndex`), so they are stable strings like
/// "`DualSense` Wireless Controller" rather than raw USB product strings.
fn type_for_name(name: &str) -> Option<crate::services::store::GamepadType> {
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

/// Whether a real game pad is attached — what the "With a controller" console-UI mode reads.
///
/// [`is_tv_remote`] is the whole point of the filter: every webOS set enumerates its own remote
/// as a controller, so trusting SDL's list would make that mode mean "always" on every TV.
pub fn any_pad_connected(subsystem: &sdl2::GameControllerSubsystem) -> bool {
    let Ok(count) = subsystem.num_joysticks() else {
        return false;
    };
    (0..count)
        .filter(|&i| subsystem.is_game_controller(i))
        .filter_map(|i| subsystem.name_for_index(i).ok())
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

/// SDL2's `Axis` enum → punktfunk's `AXIS_*` wire id.
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

/// `pad` is the wire pad index (`flags`) — 0 for the single-controller case this phase
/// targets (multi-pad indexing is a follow-up once one controller round-trips cleanly).
pub fn button_event(button: Button, pressed: bool, pad: u8) -> InputEvent {
    bit_event(button_bit(button), pressed, pad)
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

/// SDL2 sticks are already i16 (−32768..32767) matching the wire's range, so X passes
/// straight through. Y does not: confirmed on-device (`DualSense` over Bluetooth, this
/// webOS/Linux SDL2 build) that pushing a stick up/forward reports a *negative* raw
/// value — the opposite of the wire's XInput/Moonlight "+y = up" convention — so both
/// sticks' Y axes are negated before sending (`saturating_neg` since raw `i16::MIN`
/// has no positive counterpart in range). Triggers arrive as SDL2's 0..32767 range —
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
