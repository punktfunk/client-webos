//! Maps SDL3 mouse events (Magic Remote pointer mode) to punktfunk wire `InputEvents`.
//! Allows pointing/clicking remote to drive host cursor during stream.
use punktfunk_core::input::{InputEvent, InputKind};
use sdl3::mouse::MouseButton;

/// `GameStream`'s classic mouse-button numbering (1=left..5=X2) — the convention
/// `punktfunk-host`'s injectors expect in `MouseButtonDown`/`MouseButtonUp`'s `code`
/// (confirmed via `gs_button_to_evdev` in `punktfunk-host/src/inject.rs`).
fn button_code(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(1),
        MouseButton::Middle => Some(2),
        MouseButton::Right => Some(3),
        MouseButton::X1 => Some(4),
        MouseButton::X2 => Some(5),
        MouseButton::Unknown => None,
    }
}

/// Whether this is a mouse event synthesized from a touch device (`which` is
/// `SDL_TOUCH_MOUSEID`) rather than one a real pointer reported. Dropped at the head of both of
/// `runtime`'s event loops, so nothing downstream has to know the difference.
///
/// A `DualSense` publishes its touchpad as its own absolute/multitouch evdev node, which the
/// compositor picks up as a touch device; the emulation then holds the left button down for as
/// long as a finger is on the pad, so a thumb resting there drags whatever the host cursor is
/// over. `SDL_TOUCH_MOUSE_EVENTS=0` (set in `runtime::stream`) turns SDL's own half off, and
/// [`super::evdev`] claims the node so the compositor can't drive the TV cursor from it either;
/// this is the last of the three, covering anything synthesized before SDL sees it.
///
/// `SDL_TOUCH_MOUSEID` is `(Uint32)-1`; rust-sdl3 doesn't re-export it.
pub fn is_touch_emulated(event: &sdl3::event::Event) -> bool {
    use sdl3::event::Event;
    let (Event::MouseMotion { which, .. }
    | Event::MouseButtonDown { which, .. }
    | Event::MouseButtonUp { which, .. }
    | Event::MouseWheel { which, .. }) = *event
    else {
        return false;
    };
    which == u32::MAX
}

/// `None` for a button id the host has no mapping for (`MouseButton::Unknown`) —
/// the caller just drops the event.
pub fn button_event(button: MouseButton, pressed: bool) -> Option<InputEvent> {
    Some(raw_button_event(button_code(button)?, pressed))
}

/// Same wire event from an already-mapped button number — for [`super::evdev`], whose
/// buttons come off evdev and never pass through an `sdl3::mouse::MouseButton`.
pub fn raw_button_event(code: u32, pressed: bool) -> InputEvent {
    InputEvent {
        kind: if pressed {
            InputKind::MouseButtonDown
        } else {
            InputKind::MouseButtonUp
        },
        _pad: [0; 3],
        code,
        x: 0,
        y: 0,
        flags: 0,
    }
}

/// The right button, in `button_code`'s numbering.
const RIGHT: u32 = 3;

/// The Magic Remote's Red key, mirrored onto the right button press for press, and the one
/// place that held state lives — so [`Self::release_held`] can leave the host holding nothing
/// when releases stop arriving (the disconnect dialog swallows input, or the stream ends).
#[derive(Default)]
pub struct RemoteButtons {
    red_down: bool,
}

impl RemoteButtons {
    pub fn red(&mut self, down: bool, send: impl FnOnce(&InputEvent)) {
        self.red_down = down;
        send(&raw_button_event(RIGHT, down));
    }

    /// Releases the right button if Red holds it. Idempotent.
    pub fn release_held(&mut self, send: impl FnOnce(&InputEvent)) {
        if std::mem::take(&mut self.red_down) {
            send(&raw_button_event(RIGHT, false));
        }
    }
}

/// Relative motion, for a captured stream. Absolute coordinates can't leave the panel —
/// webOS's pointer stops at the screen edge, so `MouseMoveAbs` saturates there and the
/// host cursor can neither cross onto another display nor keep turning in a game that
/// wants continuous motion. Deltas have no such ceiling.
pub fn move_relative_event(dx: i32, dy: i32) -> InputEvent {
    InputEvent {
        kind: InputKind::MouseMove,
        _pad: [0; 3],
        code: 0,
        x: dx,
        y: dy,
        flags: 0,
    }
}

/// Retains subpixel SDL motion until it reaches a whole wire pixel.
#[derive(Default)]
pub struct RelativeMotion {
    x: f32,
    y: f32,
}

impl RelativeMotion {
    pub fn take(&mut self, dx: f32, dy: f32) -> (i32, i32) {
        self.x += dx;
        self.y += dy;
        let whole = (self.x as i32, self.y as i32);
        self.x -= whole.0 as f32;
        self.y -= whole.1 as f32;
        whole
    }
}

/// Absolute pointer position — `client_w`/`client_h` is this app's own coordinate
/// space (the physical panel resolution the SDL window/mouse coordinates are in,
/// not necessarily the negotiated stream resolution); the host normalizes against
/// it before mapping into the output region (see `InputKind::MouseMoveAbs` docs) —
/// the same absolute-pointer path the pre-stream menu's hover/click already rides,
/// just forwarded to the host instead of used for local UI focus.
pub fn move_event(x: i32, y: i32, client_w: u32, client_h: u32) -> InputEvent {
    InputEvent {
        kind: InputKind::MouseMoveAbs,
        _pad: [0; 3],
        code: 0,
        x,
        y,
        flags: (client_w << 16) | (client_h & 0xffff),
    }
}

/// Scales wheel detent to wire's `WHEEL_DELTA` (120) convention.
///
/// `delta` is whole detents—SDL3's `MouseWheel::integer_*` and evdev's `REL_WHEEL` are
/// one unit per notch. No fractional remainder with integral input scaled ×120.
///
/// `horizontal` maps to wire `code` (0=vertical, 1=horizontal).
pub fn scroll_event(delta: i32, horizontal: bool) -> InputEvent {
    InputEvent {
        kind: InputKind::MouseScroll,
        _pad: [0; 3],
        code: u32::from(horizontal),
        x: delta * 120,
        y: 0,
        flags: 0,
    }
}

#[cfg(test)]
mod relative_motion_tests {
    use super::RelativeMotion;

    #[test]
    fn subpixel_motion_accumulates_in_both_directions() {
        let mut motion = RelativeMotion::default();
        for _ in 0..3 {
            assert_eq!(motion.take(0.25, -0.25), (0, 0));
        }
        assert_eq!(motion.take(0.25, -0.25), (1, -1));
        assert_eq!(motion.take(1.5, -2.5), (1, -2));
        assert_eq!(motion.take(-0.5, 0.5), (0, 0));
        assert_eq!(motion.take(0.0, 0.0), (0, 0));
    }
}
