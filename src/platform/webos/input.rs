//! Raw SDL3 keyboard/gamepad input mapped to debounced `MenuEvent`s.

use crate::core::event::MenuEvent;

/// Turns off unused event families.
///
/// 🛑 The raw joystick AXIS/BUTTON/HAT lanes must stay ON: SDL3 synthesizes all gamepad events
/// from them, so muting them kills all pad input. Only `JoyDevice{Added,Removed}` are safe.
///
/// Touch lane must not become pointer input: `SDL_TOUCH_MOUSE_EVENTS` already blocks pad
/// touchpad synthesis. Muting finger events leaves no accidental reader. Pen, drag-drop, clipboard,
/// IME pre-edit, and gamepad touchpad have no TV reader at all.
pub fn mute_unused_events() {
    use sdl3::event::EventType as E;
    const UNUSED: &[E] = &[
        E::JoyDeviceAdded,
        E::JoyDeviceRemoved,
        E::GamepadTouchpadDown,
        E::GamepadTouchpadMotion,
        E::GamepadTouchpadUp,
        E::FingerDown,
        E::FingerUp,
        E::FingerMotion,
        E::FingerCanceled,
        E::PenProximityIn,
        E::PenProximityOut,
        E::PenDown,
        E::PenUp,
        E::PenButtonDown,
        E::PenButtonUp,
        E::PenMotion,
        E::PenAxis,
        E::DropFile,
        E::DropText,
        E::DropBegin,
        E::DropComplete,
        E::ClipboardUpdate,
        // Pre-edit state for a composing IME. Only the committed `TextInput` is read.
        E::TextEditing,
    ];
    for &event in UNUSED {
        sdl3::EventSubsystem::set_event_enabled(event, false);
    }
}

/// Sleeps until an event is queued or `timeout` passes, leaving the event for the loop's own
/// poll: an input wakes a loop at once instead of at the end of a fixed sleep.
pub fn wait_for_event(timeout: std::time::Duration) {
    let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: SDL documents a null event pointer as "wait, but leave the event queued"; the
    // call touches nothing this side owns.
    unsafe {
        sdl3::sys::events::SDL_WaitEventTimeout(std::ptr::null_mut(), ms);
    }
}

pub fn menu_event_for_key(keycode: sdl3::keyboard::Keycode) -> Option<MenuEvent> {
    use sdl3::keyboard::Keycode;
    Some(match keycode {
        Keycode::Up => MenuEvent::Up,
        Keycode::Down => MenuEvent::Down,
        Keycode::Left => MenuEvent::Left,
        Keycode::Right => MenuEvent::Right,
        Keycode::Return | Keycode::Return2 | Keycode::KpEnter => MenuEvent::Confirm,
        // Map Backspace/Escape/AcBack so Back works with any remote variant.
        Keycode::Backspace | Keycode::Escape | Keycode::AcBack => MenuEvent::Back,
        Keycode::Delete => MenuEvent::Secondary,
        _ => return None,
    })
}

pub fn menu_event_for_button(which: sdl3::joystick::JoystickId, button: sdl3::gamepad::Button) -> Option<MenuEvent> {
    use sdl3::gamepad::Button;
    let [a, b, _, y] = face_by_label(which);
    Some(match button {
        Button::DPadUp => MenuEvent::Up,
        Button::DPadDown => MenuEvent::Down,
        Button::DPadLeft => MenuEvent::Left,
        Button::DPadRight => MenuEvent::Right,
        // WHY: Magic Remote's Back doesn't arrive as B; Back is low-risk guess.
        Button::Back => MenuEvent::Back,
        _ if button == a => MenuEvent::Confirm,
        _ if button == b => MenuEvent::Back,
        _ if button == y => MenuEvent::Secondary,
        _ => return None,
    })
}

/// The positions of the A, B, X and Y labels on pad `which`. SDL3 reports face buttons by
/// position; menus follow the labels, SDL2's default, so a Nintendo pad confirms with its A.
/// The wire stays positional (`gamepad::button_bit`).
pub fn face_by_label(which: sdl3::joystick::JoystickId) -> [sdl3::gamepad::Button; 4] {
    use sdl3::gamepad::Button;
    use sdl3::sys::gamepad as sys;
    // SAFETY: plain queries by instance id; an unknown id yields an unknown type.
    let south_is_b = unsafe {
        let kind = sys::SDL_GetGamepadTypeForID(which.into());
        sys::SDL_GetGamepadButtonLabelForType(kind, sys::SDL_GamepadButton::SOUTH) == sys::SDL_GamepadButtonLabel::B
    };
    if south_is_b {
        [Button::East, Button::South, Button::North, Button::West]
    } else {
        [Button::South, Button::East, Button::West, Button::North]
    }
}

/// Stick deflection threshold for directional press (well past center noise).
pub const STICK_MENU_DEADZONE: i16 = 16_000;

/// Edge-detect left stick X/Y to `MenuEvents` (one-shot per cross, repeats on re-center).
#[derive(Default)]
pub struct StickMenuNav {
    x: Option<MenuEvent>,
    y: Option<MenuEvent>,
}

impl StickMenuNav {
    pub fn axis_event(&mut self, axis: sdl3::gamepad::Axis, value: i16) -> Option<MenuEvent> {
        use sdl3::gamepad::Axis;
        match axis {
            Axis::LeftX => Self::edge(&mut self.x, value, MenuEvent::Left, MenuEvent::Right),
            Axis::LeftY => Self::edge(&mut self.y, value, MenuEvent::Up, MenuEvent::Down),
            _ => None,
        }
    }

    /// Whether `value` is inside the centre deadzone — i.e. this axis is holding no
    /// direction. The threshold's one reader outside [`edge`](Self::edge), for a caller
    /// running its own hold timer off the crossings [`axis_event`](Self::axis_event) reports.
    pub const fn centred(value: i16) -> bool {
        value.unsigned_abs() < STICK_MENU_DEADZONE.unsigned_abs()
    }

    fn edge(state: &mut Option<MenuEvent>, value: i16, neg: MenuEvent, pos: MenuEvent) -> Option<MenuEvent> {
        let dir = if value <= -STICK_MENU_DEADZONE {
            Some(neg)
        } else if value >= STICK_MENU_DEADZONE {
            Some(pos)
        } else {
            None
        };
        if dir == *state {
            return None;
        }
        *state = dir;
        dir
    }
}

/// webOS Home key scancode. Polled because it sits outside rust-sdl3's `Scancode` enum.
///
/// ⚠ 364, not 384: the webOS block sits at 352-375. Home and [`WEBOS_EXIT_SCANCODE`] are the
/// only two keys read by polling; colour keys and Back use the event `raw` field instead — see
/// [`RemoteKey`].
///
/// Polled to re-open the launcher when `KEYS_HOME` capture blocks the OS. A USB Super key
/// lands here too but is indistinguishable, so the host gets it via evdev instead.
pub const WEBOS_HOME_SCANCODE: i32 = 364;

/// webOS EXIT key scancode. A held Back becomes an EXIT gesture, distinct from a short Back tap.
/// Reliable signal for opening the disconnect/quit dialog. Requires
/// `SDL_WEBOS_ACCESS_POLICY_KEYS_EXIT` at window creation to prevent `SIGTERM`. See `docs/NOTES.md`.
pub const WEBOS_EXIT_SCANCODE: i32 = 375;

/// The Magic Remote's own keys, identified by the evdev code SDL3 hands back on `raw`.
///
/// SDL3 gives no scancode or keycode for these keys, sitting outside rust-sdl3's enums.
/// `raw` carries the plain evdev code as-is. Polling the state array failed: Back and Red never
/// set a bit, and bits in that range get stuck down for the whole session.
/// Matching the event restores true edges and filters pad echoes through `RemoteGate`.
///
/// `KEY_RED`..`KEY_BLUE` are 0x18e-0x191; `KEY_PREVIOUS` (Back) is 0x19c. Green, Yellow, Blue
/// and Back confirmed on glass; Red is the same run, one below Green.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u16)]
pub enum RemoteKey {
    Red = 0x18e,
    Green = 0x18f,
    Yellow = 0x190,
    Blue = 0x191,
    Back = 0x19c,
}

impl RemoteKey {
    pub const fn from_raw(raw: u16) -> Option<Self> {
        Some(match raw {
            0x18e => Self::Red,
            0x18f => Self::Green,
            0x190 => Self::Yellow,
            0x191 => Self::Blue,
            0x19c => Self::Back,
            _ => return None,
        })
    }

    /// This key's slot in [`RemoteKeys`]'s held-state array.
    const fn index(self) -> usize {
        match self {
            Self::Red => 0,
            Self::Green => 1,
            Self::Yellow => 2,
            Self::Blue => 3,
            Self::Back => 4,
        }
    }
}

/// Check a Magic Remote button via raw SDL keyboard state (safe after `sdl3::init`).
///
/// Only Home and Exit are polled; they set a state bit and have no event worth matching.
/// Colour keys and Back use [`RemoteKeys`] instead — see there for why.
pub fn webos_scancode_down(scancode: i32) -> bool {
    unsafe {
        let mut count = 0;
        let state = sdl3::sys::keyboard::SDL_GetKeyboardState(&mut count);
        // SDL3's state array is `*const bool`, one byte per scancode.
        !state.is_null() && scancode >= 0 && scancode < count && *state.offset(scancode as isize)
    }
}

/// The raw edge from `event`: the remote key and whether it went down. Auto-repeat unfiltered;
/// [`RemoteKeys`] owns deduping.
fn raw_edge(event: &sdl3::event::Event) -> Option<(RemoteKey, bool)> {
    use sdl3::event::Event;
    match *event {
        Event::KeyDown { raw, .. } => RemoteKey::from_raw(raw).map(|k| (k, true)),
        Event::KeyUp { raw, .. } => RemoteKey::from_raw(raw).map(|k| (k, false)),
        _ => None,
    }
}

/// Turns remote keys into clean edges: one press per physical press, matching release,
/// no auto-repeats.
///
/// 🛑 One per event loop, resolved ONCE per event: both methods consume the edge they report,
/// so a second resolver on the same event sees a different answer. Back bug: two handlers
/// deriving Back from the same event (press and release) navigated twice with one tap.
///
/// Necessary because remote keys carry no scancode or keycode under SDL3, so they cannot
/// ride normal `keycode: Some(k)` paths. Which edges arrive depends on where the loop started.
#[derive(Default)]
pub struct RemoteKeys {
    held: [bool; 5],
}

impl RemoteKeys {
    /// Both edges, auto-repeat suppressed: `(key, true)` on down, `(key, false)` on release.
    /// For code that mirrors held keys to the host; dropping the release would leave it held.
    /// `back_admitted` is resolved before this call; rejected echoes must not change held state.
    pub fn edge(&mut self, event: &sdl3::event::Event, back_admitted: bool) -> Option<(RemoteKey, bool)> {
        self.forget_on_focus_loss(event);
        let (key, down) = raw_edge(event)?;
        // Reject compositor echoes before they can suppress a later real Back press.
        if key == RemoteKey::Back && !back_admitted {
            return None;
        }
        let held = &mut self.held[key.index()];
        if down == *held {
            // An OS auto-repeat, or a release for a key this loop never saw pressed.
            return None;
        }
        *held = down;
        Some((key, down))
    }

    /// Forgets every held key, for when their releases will not arrive.
    pub fn reset(&mut self) {
        self.held = [false; 5];
    }

    /// Every loop gets this from its own event stream: after focus loss the releases go to
    /// another app, and SDL's reset synthesizes them with `raw == 0`, which [`raw_edge`] cannot
    /// resolve. A held flag left behind would swallow the next press.
    fn forget_on_focus_loss(&mut self, event: &sdl3::event::Event) {
        if let sdl3::event::Event::Window {
            win_event: sdl3::event::WindowEvent::FocusLost,
            ..
        } = event
        {
            self.reset();
        }
    }

    /// Exactly one press per physical press (for menus). A release with no press counts as the
    /// press: a loop entered mid-press never saw the key go down (happens on stream return,
    /// where the stream loop ate the key-down and only key-up reaches the menu).
    pub fn press(&mut self, event: &sdl3::event::Event) -> Option<RemoteKey> {
        self.forget_on_focus_loss(event);
        let (key, down) = raw_edge(event)?;
        let held = &mut self.held[key.index()];
        // Unheld either way: a fresh press, or an orphan release (see the doc above).
        let fire = !*held;
        *held = down;
        fire.then_some(key)
    }
}

/// Extract digit from Magic Remote number buttons (0-9 direct PIN entry).
pub fn digit_key_value(keycode: sdl3::keyboard::Keycode) -> Option<u8> {
    use sdl3::keyboard::Keycode;
    Some(match keycode {
        Keycode::_0 | Keycode::Kp0 => 0,
        Keycode::_1 | Keycode::Kp1 => 1,
        Keycode::_2 | Keycode::Kp2 => 2,
        Keycode::_3 | Keycode::Kp3 => 3,
        Keycode::_4 | Keycode::Kp4 => 4,
        Keycode::_5 | Keycode::Kp5 => 5,
        Keycode::_6 | Keycode::Kp6 => 6,
        Keycode::_7 | Keycode::Kp7 => 7,
        Keycode::_8 | Keycode::Kp8 => 8,
        Keycode::_9 | Keycode::Kp9 => 9,
        _ => return None,
    })
}

/// Builds an SDL3 key event for tests. Shared because `Event::Key{Down,Up}` has eight fields,
/// and spelling it out by hand accumulated four near-identical copies across test modules.
#[cfg(test)]
pub(crate) fn test_key_event(
    scancode: Option<sdl3::keyboard::Scancode>,
    keycode: Option<sdl3::keyboard::Keycode>,
    raw: u16,
    down: bool,
    repeat: bool,
) -> sdl3::event::Event {
    use sdl3::event::Event;
    let (timestamp, window_id, keymod, which) = (0, 0, sdl3::keyboard::Mod::NOMOD, 0);
    if down {
        Event::KeyDown {
            timestamp,
            window_id,
            keycode,
            scancode,
            keymod,
            repeat,
            which,
            raw,
        }
    } else {
        Event::KeyUp {
            timestamp,
            window_id,
            keycode,
            scancode,
            keymod,
            repeat,
            which,
            raw,
        }
    }
}

#[cfg(test)]
mod remote_keys_tests {
    use super::*;

    fn key(raw: u16, down: bool) -> sdl3::event::Event {
        test_key_event(None, None, raw, down, false)
    }

    fn back(down: bool) -> sdl3::event::Event {
        key(RemoteKey::Back as u16, down)
    }

    /// The invariant every Back bug broke: one physical press, one menu action.
    /// Release must not read as a second press (it opened the quit dialog then dismissed it).
    #[test]
    fn a_press_and_its_release_are_one_menu_press() {
        let mut keys = RemoteKeys::default();
        assert_eq!(keys.press(&back(true)), Some(RemoteKey::Back));
        assert_eq!(keys.press(&back(false)), None, "the release is not a second press");
        assert_eq!(
            keys.press(&back(true)),
            Some(RemoteKey::Back),
            "the next tap still fires"
        );
    }

    /// A loop entered mid-press (stream ate the key-down) sees only release and must act on it.
    #[test]
    fn an_orphan_release_is_the_press() {
        let mut keys = RemoteKeys::default();
        assert_eq!(keys.press(&back(false)), Some(RemoteKey::Back));
        assert_eq!(keys.press(&back(false)), Some(RemoteKey::Back), "still orphaned");
    }

    /// Stream mirrors held keys to host; needs both edges, one each, or auto-repeat re-presses.
    #[test]
    fn edges_are_reported_once_each_and_repeats_dropped() {
        let mut keys = RemoteKeys::default();
        assert_eq!(keys.edge(&back(true), true), Some((RemoteKey::Back, true)));
        assert_eq!(keys.edge(&back(true), true), None, "auto-repeat is not a fresh press");
        assert_eq!(keys.edge(&back(false), true), Some((RemoteKey::Back, false)));
        assert_eq!(keys.edge(&back(false), true), None, "nothing is held to release");
    }

    /// Held state per key; colour press cannot swallow Back's.
    #[test]
    fn keys_do_not_share_held_state() {
        let mut keys = RemoteKeys::default();
        assert_eq!(keys.press(&back(true)), Some(RemoteKey::Back));
        assert_eq!(keys.press(&key(RemoteKey::Red as u16, true)), Some(RemoteKey::Red));
        assert_eq!(keys.press(&back(false)), None);
    }

    /// Non-remote keys are left entirely alone.
    #[test]
    fn other_keys_are_not_claimed() {
        let mut keys = RemoteKeys::default();
        assert_eq!(keys.press(&key(30, true)), None);
        assert_eq!(keys.edge(&key(30, true), true), None);
    }
}
