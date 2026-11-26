//! Two layers can draw the local pointer: SDL's own cursor (`show_cursor`, works) and the
//! compositor's (`SDL_webOSCursorVisibility`, global state, re-shown on activity — see
//! [`restore_on_exit`]).
//!
//! The compositor layer is normally kept quiet by `evdev`'s `EVIOCGRAB` — starved of reports,
//! it stops drawing. But starving only stops *future* draws: an arrow already on screen when the
//! stream starts stays painted until something retracts it, which is why the hide is also
//! requested outright on each [`Cursor::apply`] — including the one that runs once the grab is
//! actually in place ([`Cursor::disable_sdl_relative`]).
//!
//! Hiding is not enough on its own: the compositor's invisible branch "let cursor be updated by
//! upcoming event" only marks the pointer hidden and waits for the next pointer event to repaint.
//! With the mouse node held by `EVIOCGRAB` no such event ever arrives, so an arrow already on
//! screen stays until something unrelated (a wheel or D-pad press on a node this app doesn't grab)
//! flushes it. [`Cursor::flush`] supplies that event. Showing is lazy the same way, so releasing
//! a capture needs the nudge too or the arrow stays gone until a button press.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;

use sdl3::mouse::MouseUtil;
use sdl3::video::Window;

use super::sdl_webos;

/// Global: shared with the panic hook, which has no [`Cursor`] to reach for.
static COMPOSITOR_HIDDEN: AtomicBool = AtomicBool::new(false);
static SUPPORT_LOGGED: AtomicBool = AtomicBool::new(false);
static OWNER_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// The local pointer's visibility on every layer, plus capture state. Drive from the SDL video thread.
pub struct Cursor {
    mouse: MouseUtil,
    captured: bool,
    /// See [`Self::set`].
    hid_drives: bool,
    sdl_relative: bool,
    /// See [`can_hide`].
    may_hide: bool,
    /// Whether the last [`set_compositor_visible`] was actually honoured — see [`Self::flush`].
    compositor_layer: bool,
}

impl Cursor {
    pub fn new(mouse: MouseUtil) -> Self {
        Self {
            mouse,
            captured: false,
            hid_drives: false,
            sdl_relative: true,
            may_hide: can_hide(),
            compositor_layer: false,
        }
    }

    /// Stop asking SDL for relative mode, for when motion is read via `super::evdev` instead:
    /// the fork emulates relative mode with a screen-centre warp per motion event, which is
    /// pure waste for a source we don't read. aurora-tv does the same under `hardware_mouse`.
    pub fn disable_sdl_relative(&mut self, window: &Window) {
        self.sdl_relative = false;
        self.apply(window);
    }

    /// Capture the pointer for the host — hidden on both layers, and SDL switched to
    /// relative mode so motion arrives as unbounded deltas instead of coordinates that
    /// stop at the panel edge. Uncaptured is the menu/desktop state: visible, absolute.
    /// Where the TV can't take a hidden cursor ([`can_hide`]), captured stays visible and absolute.
    /// No HID mouse drives the host meanwhile ([`Self::set`]).
    pub fn set_captured(&mut self, captured: bool, window: &Window) {
        self.set(captured, false, window);
    }

    /// [`Self::set_captured`], plus whether a HID mouse the evdev reader holds drives the host
    /// ([`hides`]).
    pub fn set(&mut self, captured: bool, hid_drives: bool, window: &Window) {
        self.captured = captured;
        self.hid_drives = hid_drives;
        self.apply(window);
    }

    pub fn is_captured(&self) -> bool {
        self.captured
    }

    pub fn hid_drives(&self) -> bool {
        self.hid_drives
    }

    /// SDL3 scopes relative mode per window.
    fn apply(&mut self, window: &Window) {
        let _ = OWNER_THREAD.set(std::thread::current().id());
        let hide = self.hidden();
        self.mouse.show_cursor(!hide);
        self.mouse.set_relative_mouse_mode(window, self.relative());
        self.compositor_layer = set_compositor_visible(!hide);
        COMPOSITOR_HIDDEN.store(hide, Ordering::Relaxed);
    }

    fn hidden(&self) -> bool {
        hides(self.captured, self.hid_drives, self.may_hide)
    }

    /// The fork's relative mode re-centres the pointer per motion, which only a hidden arrow can
    /// take. Capture's alone: a HID mouse never reaches SDL.
    fn relative(&self) -> bool {
        self.captured && self.may_hide && self.sdl_relative
    }

    /// Nudges the compositor into acting on the last visibility change by warping its pointer —
    /// see the module docs: `set_cursor_visibility` alone repaints nothing, it waits for a pointer
    /// event that a grabbed mouse node can no longer produce. Relative mode is dropped across the
    /// warp because the fork implements it *as* a warp per motion, and SDL swallows an explicit
    /// one while it is on.
    ///
    /// Hidden, the destination is free (relative mode re-centres anyway) so it is the centre.
    /// Visible, the pointer is the user's and must not jump, so the warp is to where it
    /// already is — a null move still counts as the event the compositor is waiting for. That
    /// second case is also why an unseen pointer is left alone: SDL reports the origin until a
    /// motion event arrives, and warping there would fling a pointer the TV is drawing mid-screen
    /// into the corner, and forward that jump to the host as an absolute move.
    ///
    /// SDL3 exposes `SDL_GetMouseState` through `EventPump::mouse_state`, not globally.
    ///
    /// No-op where the compositor layer isn't ours to drive (stock SDL3, or a TV without
    /// `wl_webos_input_manager`): there is no pending visibility change to flush, so the warp
    /// would be pure pointer displacement.
    pub fn flush(&mut self, window: &Window, events: &sdl3::EventPump) {
        if !self.compositor_layer {
            return;
        }
        let (x, y) = if self.hidden() {
            let (w, h) = window.size();
            (w as f32 / 2.0, h as f32 / 2.0)
        } else {
            let state = events.mouse_state();
            match (state.x(), state.y()) {
                (0.0, 0.0) => return,
                pos => pos,
            }
        };
        self.mouse.set_relative_mouse_mode(window, false);
        self.mouse.warp_mouse_in_window(window, x, y);
        self.mouse.set_relative_mouse_mode(window, self.relative());
    }
}

/// Whether capture alone hides the arrow. From webOS 24 (`webos_release` 9) on, moving the Magic
/// Remote under a hidden client cursor breaks the pointer TV-wide, past this app's exit, until the
/// TV sleeps. There the remote aims the TV's arrow absolutely, and the TV hides it a few seconds
/// after the remote rests. A HID mouse driving the host hides it anyway ([`hides`]).
pub fn can_hide() -> bool {
    static CAN: OnceLock<bool> = OnceLock::new();
    *CAN.get_or_init(|| {
        let webos_major = super::device::webos_major();
        let can = hides_on(webos_major);
        tracing::info!(?webos_major, can, "client cursor hide");
        can
    })
}

/// An unread release keeps the hide.
fn hides_on(webos_major: Option<u32>) -> bool {
    webos_major.is_none_or(|major| major < 9)
}

/// Capture hides the arrow, and so does a HID mouse driving the host in either cursor mode, where
/// the host draws its own pointer. Never on a TV that can't take a hidden cursor ([`can_hide`]):
/// a mouse does not stop the Magic Remote from moving under the hidden arrow.
fn hides(captured: bool, hid_drives: bool, may_hide: bool) -> bool {
    may_hide && (captured || hid_drives)
}

fn set_compositor_visible(visible: bool) -> bool {
    // `SDL_webOSCursorVisibility(true)` shows the arrow. The TV keeps the last value after this app
    // exits, so an inverted call leaves its pointer hidden TV-wide. Unresolved (stock SDL3) reports
    // "unsupported", same as a TV without `wl_webos_input_manager`.
    let supported = sdl_webos::fns().is_ok_and(|fns| {
        // SAFETY: plain integer argument, no pointers; caller is the SDL video thread.
        unsafe { (fns.cursor_visibility)(visible) }
    });
    // Logged once, for stray-cursor bug reports.
    if !SUPPORT_LOGGED.swap(true, Ordering::Relaxed) {
        tracing::info!(
            "compositor cursor visibility control: {}",
            if supported { "available" } else { "unavailable" }
        );
    }
    supported
}

/// Put the compositor pointer back if a [`Cursor`] hid it — for exits that skip its teardown
/// (the panic hook); a graceful quit already calls [`Cursor::set_captured`]`(false)`. No-op
/// off the hiding thread, since a panicking thread has no business touching its Wayland connection.
pub fn restore_on_exit() {
    if !COMPOSITOR_HIDDEN.swap(false, Ordering::Relaxed) {
        return;
    }
    if OWNER_THREAD.get() != Some(&std::thread::current().id()) {
        tracing::warn!("cursor left hidden — panic is off the SDL thread, leaving it to client teardown");
        return;
    }
    set_compositor_visible(true);
}

#[cfg(test)]
mod tests {
    use super::{hides, hides_on};

    #[test]
    fn webos_24_and_later_keep_the_arrow() {
        assert!(hides_on(Some(8)));
        assert!(!hides_on(Some(9)));
        assert!(!hides_on(Some(11)));
        assert!(hides_on(None));
    }

    #[test]
    fn a_hid_mouse_hides_the_arrow_only_where_the_tv_takes_it() {
        // An older TV: a HID mouse hides it in Desktop mode too, capture alone as before.
        assert!(hides(false, true, true));
        assert!(hides(true, true, true));
        assert!(hides(true, false, true));
        assert!(!hides(false, false, true));
        // webOS 24+: never hidden, mouse or not. The remote still moves under it.
        for (captured, hid_drives) in [(true, true), (false, true), (true, false), (false, false)] {
            assert!(!hides(captured, hid_drives, false));
        }
    }
}
