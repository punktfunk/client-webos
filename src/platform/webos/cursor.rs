//! Two layers can draw the local pointer: SDL's own cursor (`show_cursor`, works) and the
//! compositor's (`SDL_webOSCursorVisibility`, global state, re-shown on activity — see
//! [`restore_on_exit`]).
//!
//! The compositor layer is normally kept quiet by `evdev`'s `EVIOCGRAB` — starved of reports,
//! it stops drawing. But starving only stops *future* draws: an arrow already on screen when the
//! stream starts stays painted until something retracts it, which is why the hide is also
//! requested outright on each [`Cursor::apply`] and again once the grab is actually in place
//! ([`Cursor::reassert_hidden`]).
//!
//! Hiding is not enough on its own: the compositor's invisible branch "let cursor be updated by
//! upcoming event" only marks the pointer hidden and waits for the next pointer event to repaint.
//! With the mouse node held by `EVIOCGRAB` no such event ever arrives, so an arrow already on
//! screen stays until something unrelated (a wheel or D-pad press on a node this app doesn't grab)
//! flushes it. [`Cursor::flush`] supplies that event. Showing is lazy the same way, so releasing
//! a capture needs the nudge too or the arrow stays gone until a button press.
//!
//! Re-asserting the hide on a timer was tried twice and dropped both times — unbounded, and
//! bounded to the seconds after a capture to chase the arrow the panel repaints on the HDR mode
//! switch. Neither retracted it: on webOS 26 the compositor re-shows its arrow on pointer
//! activity regardless, so the loop only spent Wayland requests.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use sdl3::mouse::MouseUtil;
use sdl3::video::Window;

use super::sdl_webos;

/// Debounce between compositor hide requests, so a caller pairing [`Cursor::reassert_hidden`]
/// with a state change that already ran [`Cursor::apply`] doesn't duplicate the request.
const REASSERT_INTERVAL: Duration = Duration::from_millis(250);

/// Global: shared with the panic hook, which has no [`Cursor`] to reach for.
static COMPOSITOR_HIDDEN: AtomicBool = AtomicBool::new(false);
static SUPPORT_LOGGED: AtomicBool = AtomicBool::new(false);
static OWNER_THREAD: OnceLock<ThreadId> = OnceLock::new();

/// The local pointer's visibility on every layer, plus capture state. Drive from the SDL video thread.
pub struct Cursor {
    mouse: MouseUtil,
    last_assert: Instant,
    captured: bool,
    sdl_relative: bool,
    /// Whether the last [`set_compositor_visible`] was actually honoured — see [`Self::flush`].
    compositor_layer: bool,
}

impl Cursor {
    pub fn new(mouse: MouseUtil) -> Self {
        Self {
            mouse,
            last_assert: Instant::now(),
            captured: false,
            sdl_relative: true,
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
    pub fn set_captured(&mut self, captured: bool, window: &Window) {
        self.captured = captured;
        self.apply(window);
    }

    pub fn is_captured(&self) -> bool {
        self.captured
    }

    /// SDL3 scopes relative mode per window.
    fn apply(&mut self, window: &Window) {
        let _ = OWNER_THREAD.set(std::thread::current().id());
        self.mouse.show_cursor(!self.captured);
        self.mouse
            .set_relative_mouse_mode(window, self.captured && self.sdl_relative);
        self.compositor_layer = set_compositor_visible(!self.captured);
        COMPOSITOR_HIDDEN.store(self.captured, Ordering::Relaxed);
        self.last_assert = Instant::now();
    }

    /// Nudges the compositor into acting on the last visibility change by warping its pointer —
    /// see the module docs: `set_cursor_visibility` alone repaints nothing, it waits for a pointer
    /// event that a grabbed mouse node can no longer produce. Relative mode is dropped across the
    /// warp because the fork implements it *as* a warp per motion, and SDL swallows an explicit
    /// one while it is on.
    ///
    /// Captured, the destination is free (relative mode re-centres anyway) so it is the centre.
    /// Uncaptured the pointer is the user's again and must not jump, so the warp is to where it
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
        let (x, y) = if self.captured {
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
        self.mouse
            .set_relative_mouse_mode(window, self.captured && self.sdl_relative);
    }

    /// Asks the compositor once more to drop its pointer. For the point where the evdev grab has
    /// actually landed — [`apply`](Self::apply) runs before `evdev`'s background scan finds a
    /// node, so any motion in that window can repaint the arrow it just retracted. No-op while
    /// uncaptured.
    pub fn reassert_hidden(&mut self) {
        if !self.captured || self.last_assert.elapsed() < REASSERT_INTERVAL {
            return;
        }
        set_compositor_visible(false);
        self.last_assert = Instant::now();
    }
}

fn set_compositor_visible(visible: bool) -> bool {
    // Unresolved (stock SDL3) reports "unsupported", same as a TV without
    // `wl_webos_input_manager`; the caller then uses SDL's own `show_cursor`.
    let supported = sdl_webos::fns().is_ok_and(|fns| {
        // SAFETY: plain integer argument, no pointers; caller is the SDL video thread.
        unsafe { (fns.cursor_visibility)(!visible) }
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
