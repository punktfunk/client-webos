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

pub fn menu_event_for_button(button: sdl3::gamepad::Button) -> Option<MenuEvent> {
    use sdl3::gamepad::Button;
    Some(match button {
        Button::DPadUp => MenuEvent::Up,
        Button::DPadDown => MenuEvent::Down,
        Button::DPadLeft => MenuEvent::Left,
        Button::DPadRight => MenuEvent::Right,
        Button::South => MenuEvent::Confirm,
        // WHY: Magic Remote's Back doesn't arrive as B; Back is low-risk guess.
        Button::East | Button::Back => MenuEvent::Back,
        Button::North => MenuEvent::Secondary,
        _ => return None,
    })
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
/// `repr(u8)` from zero makes the discriminant match [`RemoteKeys`]'s held-state array slot.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(u8)]
pub enum RemoteKey {
    Red = 0,
    Green,
    Yellow,
    Blue,
    Back,
}

/// `KEY_RED`..`KEY_BLUE` are 0x18e-0x191; `KEY_PREVIOUS` (Back) is 0x19c. Green, Yellow, Blue
/// and Back confirmed on glass; Red is the same run, one below Green.
impl RemoteKey {
    /// How many variants there are, for [`RemoteKeys`]'s held-state array.
    pub const COUNT: usize = 5;

    /// This key's slot in that array — its discriminant, so the two cannot drift apart.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Each key's evdev code, in discriminant order — one table both directions read, so a
    /// code and the key it names cannot drift apart.
    const RAW: [u16; Self::COUNT] = [0x18e, 0x18f, 0x190, 0x191, 0x19c];

    /// Every variant, in the same order, so [`from_raw`](Self::from_raw) can walk them.
    const ALL: [Self; Self::COUNT] = [Self::Red, Self::Green, Self::Yellow, Self::Blue, Self::Back];

    pub const fn from_raw(raw: u16) -> Option<Self> {
        // `const fn`, so a hand-rolled loop rather than `iter().position()`.
        let mut i = 0;
        while i < Self::COUNT {
            if Self::RAW[i] == raw {
                return Some(Self::ALL[i]);
            }
            i += 1;
        }
        None
    }

    pub const fn to_raw(self) -> u16 {
        Self::RAW[self.index()]
    }
}

/// The evdev code the remote reports: arrows, OK, Back, digits.
///
/// Lives here (not `runtime`) to keep remote key knowledge in one module. Back comes off `raw`
/// (SDL3 gives no scancode/keycode); others come from their scancodes. `None` means the remote
/// has no such key, what `RemoteGate` uses to deny unknown keys.
pub fn remote_evdev_code(scancode: Option<sdl3::keyboard::Scancode>, raw: u16) -> Option<u16> {
    use sdl3::keyboard::Scancode as S;
    if raw == RemoteKey::Back.to_raw() {
        return Some(raw);
    }
    Some(match scancode? {
        S::Up => 103,
        S::Down => 108,
        S::Left => 105,
        S::Right => 106,
        S::Return | S::KpEnter => 28,
        S::_1 => 2,
        S::_2 => 3,
        S::_3 => 4,
        S::_4 => 5,
        S::_5 => 6,
        S::_6 => 7,
        S::_7 => 8,
        S::_8 => 9,
        S::_9 => 10,
        S::_0 => 11,
        _ => return None,
    })
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
    held: [bool; RemoteKey::COUNT],
}

impl RemoteKeys {
    /// Both edges, auto-repeat suppressed: `(key, true)` on down, `(key, false)` on release.
    /// For code that mirrors held keys to the host; dropping the release would leave it held.
    /// `back_admitted` is resolved before this call; rejected echoes must not change held state.
    pub fn edge(&mut self, event: &sdl3::event::Event, back_admitted: bool) -> Option<(RemoteKey, bool)> {
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

    /// Exactly one press per physical press (for menus). A release with no press counts as the
    /// press: a loop entered mid-press never saw the key go down (happens on stream return,
    /// where the stream loop ate the key-down and only key-up reaches the menu).
    pub fn press(&mut self, event: &sdl3::event::Event) -> Option<RemoteKey> {
        let (key, down) = raw_edge(event)?;
        let held = &mut self.held[key.index()];
        let orphan_release = !down && !*held;
        let fresh_press = down && !*held;
        *held = down;
        (fresh_press || orphan_release).then_some(key)
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
        key(RemoteKey::Back.to_raw(), down)
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
        assert_eq!(keys.press(&key(RemoteKey::Red.to_raw(), true)), Some(RemoteKey::Red));
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
