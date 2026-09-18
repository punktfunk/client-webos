//! Pre-stream menu input plumbing: hold/chord gestures, the confirm dialog, and the
//! SDL-event → `MenuEvent` routing shared by the `ui_flow` and `stream` loops.
//!
//! Split out of `runtime/mod.rs` (which keeps run/connect/signals/log-overlay). Re-exported
//! there via `use input::*` so the sibling loop modules pick these up through `use super::*`.

use super::*;

/// How long the controller chord ([`DisconnectChord`]) must be held before its dialog
/// opens — the in-stream disconnect dialog while streaming, the quit dialog in the menu.
/// The chord's buttons are also game input, so it fires on a hold, not a press. Shared by
/// both loops so the remote's held-Back EXIT gesture and the chord feel the same — 1s to
/// match webOS's own long-press threshold on the EXIT gesture.
pub(super) const EXIT_HOLD: Duration = Duration::from_millis(1000);

/// How long OK must be held on a focused Home game card to pin/unpin it instead
/// of launching it — see `card_hold_gate`.
pub(super) const CARD_HOLD: Duration = crate::core::event::LONG_PRESS;

/// An in-flight hold-to-pin gesture: OK is down on a pinnable Home card. The
/// toggle fires the moment `CARD_HOLD` elapses (so the pin visibly lands under
/// the still-held button), and `fired` then makes the release a no-op instead
/// of the launch a quick tap would have dispatched.
pub(super) struct CardHold {
    pub(super) since: Instant,
    pub(super) focus: HomeFocus,
    pub(super) fired: bool,
}

/// The gamepad route to the disconnect dialog (streaming) or quit dialog (menu):
/// L1+R1+Start+Select held for [`EXIT_HOLD`], the escape chord every punktfunk client uses.
/// No subset opens it — L1+R1 and a held Guide are game input.
///
/// Tracked as button state rather than read back from SDL because SDL only reports
/// transitions here — and a chord needs to know what is down *now*, not what changed
/// last.
#[derive(Default)]
pub(super) struct DisconnectChord {
    left_shoulder: bool,
    right_shoulder: bool,
    start: bool,
    back: bool,
    /// When the currently-held chord became complete; `None` when none is.
    since: Option<Instant>,
}

impl DisconnectChord {
    /// Records one button transition and arms or disarms the hold timer.
    pub(super) fn set(&mut self, button: sdl3::gamepad::Button, down: bool) {
        use sdl3::gamepad::Button;
        match button {
            Button::LeftShoulder => self.left_shoulder = down,
            Button::RightShoulder => self.right_shoulder = down,
            Button::Start => self.start = down,
            Button::Back => self.back = down,
            _ => return,
        }
        // Re-derived after every transition, so releasing any part of a chord restarts
        // the hold instead of leaving a stale deadline armed.
        self.since = match (self.complete(), self.since) {
            (true, Some(t)) => Some(t),
            (true, None) => Some(Instant::now()),
            (false, _) => None,
        };
    }

    fn complete(&self) -> bool {
        self.left_shoulder && self.right_shoulder && self.start && self.back
    }

    /// Whether a chord has now been held long enough to fire.
    pub(super) fn held_for(&self, hold: Duration) -> bool {
        self.since.is_some_and(|t| t.elapsed() >= hold)
    }

    /// Forgets all held buttons.
    ///
    /// Called when the chord fires and when the pad disconnects, because in both cases
    /// the releases that follow never reach [`set`](Self::set) — the open dialog swallows
    /// controller events, and an unplugged pad sends none. Without this the buttons would
    /// stay "down" forever and the dialog would reopen the instant it was dismissed.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Which of the compositor's keys the Magic Remote pressed. webOS 23+ also types every pad
/// press as a remote key, and a Wayland key event names no device, so SDL alone cannot tell
/// them apart. The remote's own evdev node can (`evdev::RemoteNode`): each press there admits
/// one compositor key-down of that key, and the key's repeats and release follow it.
///
/// The nodes are read on this thread, every tick and again before a key is turned away, so a
/// press behind a key SDL delivers is never still unread. The compositor still decides what a
/// press means (a pointer click, the EXIT gesture, a key for the on-screen keyboard); this only
/// says whose press it was.
///
/// Until a node is adopted the gate admits every key: a TV whose remote node the app cannot
/// open, or names differently, keeps its remote. A node that goes away afterwards does not
/// disarm it, since a key with no remote to press it is an echo; the reader opens it again
/// when it comes back.
#[derive(Default)]
pub(super) struct RemoteGate {
    /// The remote's own nodes.
    nodes: Vec<crate::platform::webos::evdev::RemoteNode>,
    /// A node was adopted at some point, so a key without a press behind it is an echo.
    armed: bool,
    /// Remote presses no compositor key-down has claimed yet, with the tick they were read in.
    owed: Vec<(u16, Instant)>,
    /// Keys admitted down, whose repeats and release pass.
    down: Vec<u16>,
}

impl RemoteGate {
    /// How long a remote press waits for its key-down. An OK the pointer turned into a click is
    /// never claimed and expires.
    const CLAIM_WINDOW: Duration = Duration::from_millis(250);

    /// Takes over remote nodes the evdev reader opened. The first one arms the gate.
    pub(super) fn adopt(&mut self, nodes: Vec<crate::platform::webos::evdev::RemoteNode>) {
        if !self.armed && !nodes.is_empty() {
            tracing::info!("remote gate armed: a key now needs a press on the remote's own node");
            self.armed = true;
        }
        self.nodes.extend(nodes);
    }

    /// Reads the remote's nodes; a node that is gone is dropped.
    pub(super) fn poll(&mut self, now: Instant) {
        self.owed.retain(|&(_, at)| now.duration_since(at) < Self::CLAIM_WINDOW);
        let owed = &mut self.owed;
        self.nodes.retain_mut(|node| node.drain(|code| owed.push((code, now))));
    }

    /// A gate with a remote node behind it, as a test stands in for one.
    #[cfg(test)]
    fn armed() -> Self {
        Self {
            armed: true,
            ..Self::default()
        }
    }

    /// A press off the remote's node, as a test stands in for one.
    #[cfg(test)]
    fn pressed(&mut self, code: u16, now: Instant) {
        self.owed.push((code, now));
    }

    /// Whether `event` may act: anything but a key does, and a key only if the remote pressed
    /// it. A pad's echo has no press behind it, so it, its repeats and its release all fail.
    pub(super) fn admits(&mut self, event: &sdl3::event::Event, now: Instant) -> bool {
        use sdl3::event::Event;
        if !self.armed {
            return true;
        }
        let (scancode, raw, down, repeat) = match *event {
            Event::KeyDown {
                scancode, raw, repeat, ..
            } => (scancode, raw, true, repeat),
            Event::KeyUp { scancode, raw, .. } => (scancode, raw, false, false),
            _ => return true,
        };
        let Some(code) = crate::platform::webos::input::remote_evdev_code(scancode, raw) else {
            return false;
        };
        self.owed.retain(|&(_, at)| now.duration_since(at) < Self::CLAIM_WINDOW);
        let held = self.down.iter().position(|&c| c == code);
        match (down, held) {
            (false, Some(i)) => {
                self.down.swap_remove(i);
                true
            }
            (true, Some(_)) => true,
            (false, None) => false,
            (true, None) if repeat => false,
            (true, None) => {
                if !self.owed.iter().any(|&(c, _)| c == code) {
                    // The press may have landed after this tick's poll: read before turning it away.
                    self.poll(now);
                }
                match self.owed.iter().position(|&(c, _)| c == code) {
                    Some(i) => {
                        self.owed.remove(i);
                        self.down.push(code);
                        true
                    }
                    None => false,
                }
            }
        }
    }
}

/// Rising-edge detect on a raw webOS scancode (polled since these sit outside
/// rust-sdl3's `Scancode` enum), firing once as it goes down. `prev` carries the last-frame
/// state across calls.
fn scancode_rising_edge(scancode: i32, prev: &mut bool) -> bool {
    let down = crate::platform::webos::input::webos_scancode_down(scancode);
    let fired = down && !*prev;
    *prev = down;
    fired
}

/// Controls the webOS on-screen keyboard for UI and streaming loops.
///
/// Holds no copy of whether input is on: SDL3 scopes text input per window and answers
/// `SDL_TextInputActive` for it, so the mirrored `bool` this used to carry - which the
/// compositor could silently invalidate by dismissing the panel behind our back, and which the
/// stream loop needed a `raise()` escape hatch to work around - has one source of truth again.
pub(super) struct TextInputController {
    util: sdl3::keyboard::TextInputUtil,
    options: Option<sdl3::keyboard::TextInputOptions>,
    /// A panel this app raised, and whether it has come up yet — see [`release_if_dismissed`].
    ///
    /// [`release_if_dismissed`]: Self::release_if_dismissed
    raised: Option<(std::time::Instant, bool)>,
}

/// Where the IME should put the caret inside the field rect. Nothing here drives a caret, so the
/// panel is told the field's start.
const IME_CURSOR: i32 = 0;

impl TextInputController {
    pub(super) fn new(util: sdl3::keyboard::TextInputUtil) -> Self {
        Self {
            util,
            options: None,
            raised: None,
        }
    }

    pub(super) fn has_screen_keyboard_support(&self) -> bool {
        self.util.has_screen_keyboard_support()
    }

    /// Whether the panel is actually up. Not the same question as [`is_active`](Self::is_active):
    /// the compositor can dismiss it on its own (Back does exactly that) while text input stays on.
    pub(super) fn is_shown(&self, window: &sdl3::video::Window) -> bool {
        self.util.is_screen_keyboard_shown(window)
    }

    /// Whether SDL is accepting text input on `window`, straight from SDL.
    fn is_active(&self, window: &sdl3::video::Window) -> bool {
        self.util.is_active(window)
    }

    /// Matches text input state to the active UI screen, telling the platform IME what kind of
    /// field it is opening for. `window` is new in SDL3, which scopes text input per window
    /// instead of globally.
    pub(super) fn set_active(
        &mut self,
        want: Option<sdl3::keyboard::TextInputOptions>,
        rect: Option<sdl3::rect::Rect>,
        window: &sdl3::video::Window,
    ) {
        let Some(options) = want else {
            if self.is_active(window) {
                self.stop(window);
                tracing::debug!("text input stopped");
            }
            self.options = None;
            return;
        };
        if let Some(r) = rect {
            // The form moves when the keyboard appears. Update its area without reopening it.
            if self.util.rect(window).ok() != Some((r, IME_CURSOR)) {
                self.util.set_rect(window, r, IME_CURSOR);
            }
        }
        if self.is_active(window) && self.options == Some(options) {
            return;
        }
        if let Err(e) = self.util.start_with_options(window, options) {
            // The options are a hint; a backend that refuses them still owes a panel.
            tracing::debug!("text input options refused ({e}) - starting plain");
            self.util.start(window);
        }
        self.options = Some(options);
        tracing::debug!("text input started: {options:?}");
    }

    /// Raises the panel for a host-side field, whatever SDL thinks the current state is - the
    /// stream loop has no screen to derive one from, and re-starting an already-active input is
    /// how SDL3 re-shows a panel the compositor dismissed underneath it.
    pub(super) fn raise(&mut self, rect: sdl3::rect::Rect, window: &sdl3::video::Window) {
        self.util.set_rect(window, rect, IME_CURSOR);
        self.util.start(window);
        self.raised = Some((std::time::Instant::now(), false));
    }

    /// Ends the text input behind a panel webOS has taken away. Call every tick with
    /// [`is_shown`](Self::is_shown)'s answer.
    ///
    /// Back dismisses the panel through webOS's own IME without telling SDL, which leaves the
    /// field focused — and webOS routes every remote key to a focused field, so the remote reads
    /// as dead and a later OK re-summons the panel. Nothing else ends it.
    ///
    /// Waits for the panel to have actually appeared: `shown` is false for the ticks it takes to
    /// animate in. [`SHOW_TIMEOUT`](Self::SHOW_TIMEOUT) covers a raise the compositor refuses
    /// outright, which would otherwise hold the field for the whole session.
    pub(super) fn release_if_dismissed(&mut self, shown: bool, window: &sdl3::video::Window) {
        let Some((raised_at, came_up)) = &mut self.raised else {
            return;
        };
        if shown {
            *came_up = true;
            return;
        }
        if !*came_up && raised_at.elapsed() < Self::SHOW_TIMEOUT {
            return;
        }
        tracing::info!("on-screen keyboard dismissed — releasing text input");
        self.stop(window);
    }

    /// How long a raised panel has to appear before [`release_if_dismissed`] gives up on it.
    ///
    /// [`release_if_dismissed`]: Self::release_if_dismissed
    const SHOW_TIMEOUT: Duration = Duration::from_secs(2);

    /// Stops text input at loop exit.
    pub(super) fn stop(&mut self, window: &sdl3::video::Window) {
        self.options = None;
        self.raised = None;
        self.util.stop(window);
    }
}

/// The webOS EXIT gesture (a held Back, delivered as `WEBOS_EXIT_SCANCODE`).
pub(super) fn exit_gesture_fired(prev: &mut bool) -> bool {
    scancode_rising_edge(crate::platform::webos::input::WEBOS_EXIT_SCANCODE, prev)
}

/// The webOS Home key (`WEBOS_HOME_SCANCODE`) — captured, so callers re-open the
/// launcher themselves via `luna::launch_home`. Distinct from EXIT, so a long Back
/// never trips it.
pub(super) fn home_key_fired(prev: &mut bool) -> bool {
    scancode_rising_edge(crate::platform::webos::input::WEBOS_HOME_SCANCODE, prev)
}

/// webOS ships a real on-screen keyboard, and the SDL fork this app links wires it
/// up (`SDL_waylandwebos_osk.c` in `webosbrew/SDL-webOS`, driving `zwp_text_input_v3`)
/// — but only for an app that actually asks for text input. Nothing here ever called
/// `SDL_StartTextInput`, so the keyboard simply never appeared on the add-host screen
/// and the only way to enter an address was the remote's number pad.
///
/// `run_ui_flow` starts text input whenever a screen that edits text is open and stops it on the
/// way out, and `SDL_SetTextInputArea` tells webOS where the field is so the panel doesn't cover
/// it. Committed text arrives as `Event::TextInput`.
///
/// What each screen asks the IME for is per-field, not one global "text input is on".
/// An address is neither a sentence nor a word, so autocorrect and auto-capitalisation are turned
/// off for it - left on, webOS's own keyboard capitalises the first character of a hostname and
/// offers to correct it to a dictionary word.
pub(super) fn text_input_options(screen: Screen) -> Option<sdl3::keyboard::TextInputOptions> {
    use sdl3::keyboard::{Capitalization, TextInputOptions, TextInputType};
    let input_type = match screen {
        Screen::AddHost | Screen::EditHost => TextInputType::Text,
        Screen::RenameCollection | Screen::RenameProfile => TextInputType::Name,
        _ => return None,
    };
    Some(TextInputOptions {
        input_type: Some(input_type),
        capitalization: Some(Capitalization::None),
        autocorrect: Some(false),
        multiline: Some(false),
        android_input_type: None,
    })
}

/// Edge-triggers Back off `held`: a repeat/OS-resent press while already held
/// produces nothing, so a single physical press dispatches Back exactly once no
/// matter how SDL reports (or misreports) repeats for it — e.g. a *held* Back
/// would otherwise cascade through every level of menu navigation in one go
/// (closing a dropdown, then the very next repeat exiting the screen it was on)
/// instead of stopping at the first. Shared by the menu loop's keyboard and
/// controller arms, which debounce identically.
fn edge_trigger_back(ev: Option<MenuEvent>, held: &mut bool) -> Option<MenuEvent> {
    if ev != Some(MenuEvent::Back) {
        return ev;
    }
    if *held {
        None
    } else {
        *held = true;
        ev
    }
}

/// How long a held direction must be down before it starts repeating, and how often it repeats
/// after that. The menu runs this timer for every input: SDL reports a pad as one press and one
/// release (so without it a held D-pad moves exactly one row and a held Left never walks the
/// Bitrate slider), and the remote/keyboard's *own* OS autorepeat is swallowed and re-paced
/// here so a held direction feels the same whichever hand it comes from.
const NAV_REPEAT_DELAY: Duration = Duration::from_millis(450);
const NAV_REPEAT_PERIOD: Duration = Duration::from_millis(90);

/// Which control is holding a direction down. Kept so the release that disarms the repeat is
/// the same physical input that armed it: a stick pushed left while the D-pad is held up must
/// not have its re-centre cancel the D-pad's repeat.
#[derive(PartialEq, Eq, Clone, Copy)]
enum NavSource {
    Key(sdl3::keyboard::Keycode),
    Button(sdl3::gamepad::Button),
    Axis(sdl3::gamepad::Axis),
}

/// A held direction, mid-autorepeat.
struct NavRepeat {
    source: NavSource,
    ev: MenuEvent,
    /// When the next repeat is due — the press itself has already dispatched.
    next: Instant,
}

/// Whether `event` is a press of `want` from any source the menu accepts one from — the
/// remote/keyboard keys and the pad alike. One predicate, so a gesture keyed on a button can
/// never end up listening to one family and not the other.
///
/// `allow_repeat` says whether the OS's auto-repeat of a held key counts: a gesture that acts
/// on the press (Back) wants only the first, one that tracks the button being *down* (the
/// card hold) has to see them all.
pub(super) fn is_menu_press(event: &sdl3::event::Event, want: MenuEvent, allow_repeat: bool) -> bool {
    use sdl3::event::Event;
    match *event {
        Event::KeyDown {
            keycode: Some(k),
            repeat,
            ..
        } => (allow_repeat || !repeat) && crate::platform::webos::input::menu_event_for_key(k) == Some(want),
        Event::GamepadButtonDown { button, .. } => {
            crate::platform::webos::input::menu_event_for_button(button) == Some(want)
        }
        _ => false,
    }
}

/// The release half of [`is_menu_press`], for the gestures that resolve on the way up.
pub(super) fn is_menu_release(event: &sdl3::event::Event, want: MenuEvent) -> bool {
    use sdl3::event::Event;
    match *event {
        Event::KeyUp { keycode: Some(k), .. } => crate::platform::webos::input::menu_event_for_key(k) == Some(want),
        Event::GamepadButtonUp { button, .. } => {
            crate::platform::webos::input::menu_event_for_button(button) == Some(want)
        }
        _ => false,
    }
}

/// The UI loop's input state that outlives a single event: the Back debounce,
/// an in-flight hold-to-pin, and analogue-stick nav.
#[derive(Default)]
pub(super) struct UiInput {
    /// Whether a Back-mapped key/button is currently held, per the
    /// keyboard/gamepad event stream — edge-detected so a single physical press
    /// dispatches Back exactly once no matter how SDL reports (or misreports)
    /// repeats for it.
    menu_back_down: bool,
    /// Hold-to-pin on Home (see `CARD_HOLD`), while OK is held on a pinnable card.
    pub(super) card_held: Option<CardHold>,
    stick_nav: crate::platform::webos::input::StickMenuNav,
    /// The non-pointer input's claim on focus, while it has one.
    nav_focus: NavFocus,
    /// A click was spent confirming the scrolled focus — its release is the same press and
    /// must not act a second time (as a tap, or as the end of a slider drag).
    nav_click: bool,
    /// The held direction being autorepeated, if any.
    nav_repeat: Option<NavRepeat>,
}

impl UiInput {
    /// Hands focus to the non-pointer input that is about to act, for callers outside this
    /// module. The remote's Back carries no keycode in SDL3, so `ui_flow` resolves and
    /// dispatches it itself (see `RemoteKeys`) and never reaches the claim `handle_ui_event`
    /// makes for every other menu key — without this, the row Back just left is re-hovered by
    /// the first wobble of the hand holding the remote.
    pub(super) fn claim_nav_focus(&mut self) {
        self.nav_focus.claim();
    }

    /// Arms autorepeat on a direction that was just pressed.
    ///
    /// Pressing a non-directional one also ends any repeat in flight: whatever the user is
    /// doing now, it isn't holding that direction with intent.
    fn arm_nav_repeat(&mut self, source: NavSource, ev: MenuEvent) {
        if !ev.is_directional() {
            self.nav_repeat = None;
            return;
        }
        self.nav_repeat = Some(NavRepeat {
            source,
            ev,
            next: Instant::now() + NAV_REPEAT_DELAY,
        });
    }

    /// Resolves one press from `source`: `None` while that control is already running a
    /// repeat (the OS autorepeats a held remote/keyboard key, and those are this timer's to
    /// pace rather than to dispatch), otherwise the event, with the hold armed.
    fn press_nav(&mut self, source: NavSource, ev: Option<MenuEvent>) -> Option<MenuEvent> {
        // Same source *and* same direction is the OS repeating a held key. A different
        // direction from the same control (a stick flicked across centre) is a new press.
        if self
            .nav_repeat
            .as_ref()
            .is_some_and(|r| r.source == source && Some(r.ev) == ev)
        {
            return None;
        }
        let ev = ev?;
        self.arm_nav_repeat(source, ev);
        Some(ev)
    }

    /// Disarms the repeat if `source` is the control currently holding it.
    fn release_nav_repeat(&mut self, source: NavSource) {
        if self.nav_repeat.as_ref().is_some_and(|r| r.source == source) {
            self.nav_repeat = None;
        }
    }

    /// Drops any armed repeat — the pad went away, or a dialog took input over, and the
    /// release that would disarm it will never arrive.
    pub(super) fn clear_nav_repeat(&mut self) {
        self.nav_repeat = None;
    }

    /// The direction due to fire this tick, if the hold has run past its delay/period. One
    /// step per call: the menu loop ticks finer than the period, and catching up with a burst
    /// would race past whatever the user is watching.
    pub(super) fn nav_repeat_due(&mut self) -> Option<MenuEvent> {
        let r = self.nav_repeat.as_mut()?;
        let now = Instant::now();
        if now < r.next {
            return None;
        }
        r.next = now + NAV_REPEAT_PERIOD;
        Some(r.ev)
    }
}

/// How far the pointer must travel before it takes focus back, in screen px. Motion arrives
/// alongside the input that claimed — the Magic Remote keeps moving while its wheel turns, and
/// a hand on a HID mouse never holds still — which would hand focus to whatever row is under
/// the cursor and undo the move. Measured from a fixed anchor rather than summed, so a held
/// hand's wobble never adds up to a release; a deliberate reach clears it in a few frames.
const NAV_RELEASE_PX: i32 = 96;

/// A non-pointer input's claim on focus — the wheel's detents, and every key/pad menu event
/// (stepping rows, and Back). No clock: it holds until the pointer is deliberately moved (see
/// [`NAV_RELEASE_PX`]) or a click spends it, so a user who steps to a row and then sits still
/// keeps it however long they read it. This is what stops a resting cursor from re-hovering
/// whatever slid under it, which on a row list is a Remove button one press away.
#[derive(Default)]
struct NavFocus {
    /// Where the pointer was when the claim started. `None` while held but not yet placed:
    /// neither a wheel detent nor a key carries a position, so the anchor is the first motion
    /// after the claim.
    anchor: Option<(i32, i32)>,
    held: bool,
}

impl NavFocus {
    /// A detent or a menu event arrived: it owns focus from here, measured afresh. Re-anchoring
    /// on every one is what makes a long scroll safe — drift that stayed under the threshold
    /// during the last one must not be carried forward and add up to a release.
    fn claim(&mut self) {
        self.held = true;
        self.anchor = None;
    }

    /// Gives focus back to the pointer.
    fn release(&mut self) {
        *self = Self::default();
    }

    /// Whether this motion should be ignored. The first one after a claim anchors it;
    /// a later one far enough from that anchor is a deliberate reach and ends it.
    fn swallows_motion(&mut self, x: i32, y: i32) -> bool {
        if !self.held {
            return false;
        }
        let Some((ax, ay)) = self.anchor else {
            self.anchor = Some((x, y));
            return true;
        };
        if (x - ax).pow(2) + (y - ay).pow(2) > NAV_RELEASE_PX.pow(2) {
            self.release();
            return false;
        }
        true
    }
}

/// The menu's layout box, in the whole units every screen lays out in: the panel's real mode
/// divided by `app::draw::panel_k`.
///
/// Its own type rather than the `sdl3::video::DisplayMode` this used to be passed as. That mode
/// was fabricated field by field - pixel density, an exact refresh numerator/denominator and a
/// null backend-data pointer - to carry two numbers into code that read nothing else, and being
/// an SDL type invited setting it on a display, which it is not valid for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct LayoutBox {
    pub(super) w: u32,
    pub(super) h: u32,
}

impl LayoutBox {
    /// SDL's IME area is in window coordinates, not the menu's scaled layout units.
    pub(super) fn window_rect(self, rect: crate::ui::render::Rect, window: (u32, u32)) -> sdl3::rect::Rect {
        let x = window.0 as f32 / self.w.max(1) as f32;
        let y = window.1 as f32 / self.h.max(1) as f32;
        sdl3::rect::Rect::new(
            (rect.x() as f32 * x).round() as i32,
            (rect.y() as f32 * y).round() as i32,
            (rect.width() as f32 * x).round() as u32,
            (rect.height() as f32 * y).round() as u32,
        )
    }

    /// The box as the `App` entry points take it.
    fn wh(self) -> (u32, u32) {
        (self.w, self.h)
    }
}

/// What the UI loop should do with the event `handle_ui_event` just consumed.
pub(super) enum EventAction {
    /// Handled — carry on with the next event.
    Next,
    /// A launch is under way; leave the UI flow.
    Launch,
}

/// Starts a hold on the focused grid card, if the focus is on one. Both the pointer press
/// and the OK press arm through here so the two gestures can never disagree about what a
/// hold is; `screen_w` is the full screen width, the sidebar taken off inside.
fn arm_card_hold(input: &mut UiInput, app: &App, screen_w: u32) -> bool {
    let columns = crate::app::view::home::grid_columns_for_screen(screen_w);
    if app.focused_pin_id(columns).is_none() {
        return false;
    }
    input.card_held = Some(CardHold {
        since: Instant::now(),
        focus: app.home_focus,
        fired: false,
    });
    true
}

/// Hold-to-pin arbitration (see `CARD_HOLD`). `MenuEvent` has no press/release
/// notion, so the gesture works off raw SDL events: OK down on a pinnable Home
/// card starts the hold and is swallowed, and the launch can only ever come
/// from the release. `Some` means the event was the gesture's and goes no
/// further.
fn card_hold_gate(
    app: &mut App,
    event: &sdl3::event::Event,
    input: &mut UiInput,
    layout: LayoutBox,
    dirty: &mut bool,
) -> Option<EventAction> {
    use sdl3::event::Event;
    let (w, h) = layout.wh();
    // The Magic Remote's pointer delivers OK as a left mouse button, so give it the same
    // hold gesture the D-pad's Confirm has: a press on a hovered Home card starts the hold
    // and is swallowed (the card's menu opens on the hold-elapsed tick, same as `CARD_HOLD`
    // above), and the tap/launch comes only from the release. A press on anything else falls
    // through to the normal click path.
    if let Event::MouseButtonDown {
        mouse_btn: sdl3::mouse::MouseButton::Left,
        x,
        y,
        ..
    } = *event
    {
        // A press while the menu is up belongs to the menu, not to a fresh gesture — the
        // hold fires on elapsed (see `ui_flow`'s `CARD_HOLD` check), so the panel is already
        // open with OK still down, and re-arming here would re-open it from row 0.
        if !matches!(app.nav.screen, Screen::Home) || app.card_menu.is_some() {
            return None;
        }
        // Land hover focus on the press point first - a button press can jostle the
        // remote off the last motion position.
        *dirty |= app.handle_mouse_motion(x as i32, y as i32, w, h);
        if input.card_held.is_some() {
            return Some(EventAction::Next);
        }
        if arm_card_hold(input, app, w) {
            return Some(EventAction::Next);
        }
        return None;
    }
    // Release of a pointer OK: resolve whatever the matching press started. A fired hold has
    // already opened the card's menu (swallow); a quick tap confirms whatever's under the
    // pointer now, exactly as an immediate click would have.
    if let Event::MouseButtonUp {
        mouse_btn: sdl3::mouse::MouseButton::Left,
        x,
        y,
        ..
    } = *event
    {
        let hold = input.card_held.take()?;
        *dirty = true;
        if hold.fired {
            return Some(EventAction::Next);
        }
        return Some(if app.handle_mouse_click(x as i32, y as i32, w, h).is_some() {
            EventAction::Launch
        } else {
            EventAction::Next
        });
    }
    // Auto-repeats count here, deliberately: while OK is held they have to be caught by the
    // gesture, not dispatched as fresh presses.
    if is_menu_press(event, MenuEvent::Confirm, true) {
        // OK stays the gesture's until released, whatever the hold put on screen: the
        // card menu opens *under the still-held button*, and the next auto-repeat KeyDown
        // would otherwise dispatch Confirm straight into it.
        if input.card_held.is_some() {
            return Some(EventAction::Next);
        }
        if matches!(app.nav.screen, Screen::Home) && app.card_menu.is_none() && arm_card_hold(input, app, layout.w) {
            return Some(EventAction::Next);
        }
        return None;
    }
    // This press was ours (tap or hold) — swallow the release.
    let hold = is_menu_release(event, MenuEvent::Confirm)
        .then(|| input.card_held.take())
        .flatten()?;
    *dirty = true;
    // A quick tap: the press never dispatched, so do it now. A hold that already opened its
    // menu, or one whose screen/focus moved out from under it, resolves to nothing.
    let tapped = !hold.fired && matches!(app.nav.screen, Screen::Home) && hold.focus == app.home_focus;
    let launched = tapped && app.press(w, h).is_some();
    Some(if launched {
        EventAction::Launch
    } else {
        EventAction::Next
    })
}

/// Feeds a resolved `MenuEvent` to the app, translating what it returns into this
/// loop's terms. The per-screen routing is `App::handle_menu_event`.
pub(super) fn dispatch_menu_event(app: &mut App, menu_ev: MenuEvent, layout: LayoutBox) -> EventAction {
    let (w, h) = layout.wh();
    if menu_ev == MenuEvent::Back {
        return if app.back(w, h).is_some() {
            EventAction::Launch
        } else {
            EventAction::Next
        };
    }
    // Confirm goes through the press animation; everything else dispatches straight.
    let launched = if menu_ev == MenuEvent::Confirm {
        app.press(w, h)
    } else {
        app.handle_menu_event(menu_ev, w, h)
    };
    if launched.is_some() {
        return EventAction::Launch;
    }
    EventAction::Next
}

/// One SDL event from the pre-stream UI's pump, routed into `app`. `dirty` is
/// set whenever the event can have changed what's on screen. Device-level
/// events (quit, controller hotplug) are the caller's and never arrive here.
// Takes the event by value: it comes straight off `poll_iter`, and the `match` arms below read
// cleaner destructuring an owned event than reborrowing every payload out of a reference.
#[allow(clippy::needless_pass_by_value)]
pub(super) fn handle_ui_event(
    app: &mut App,
    event: sdl3::event::Event,
    input: &mut UiInput,
    layout: LayoutBox,
    dirty: &mut bool,
) -> EventAction {
    use sdl3::event::Event;
    let (w, h) = layout.wh();
    // The Magic Remote's pointer mode surfaces as a plain SDL MouseMotion
    // event fired continuously while the remote is moving — unlike every other
    // event handled below, redraw only if the motion actually changed the
    // focused/hovered element, not on every no-op tick.
    //
    // SDL3 reports pointer coordinates as f32; everything below this line lays out in whole
    // pixels, so they are truncated once here rather than at each of the dozen readers.
    if let Event::MouseMotion { x, y, .. } = event {
        let (x, y) = (x as i32, y as i32);
        if !input.nav_focus.swallows_motion(x, y) {
            *dirty |= app.handle_mouse_motion(x, y, w, h);
        }
        return EventAction::Next;
    }
    // The Magic Remote's scroll wheel — scrolls the game grid on Home (wheel
    // y > 0 = "scroll up" = content moves down). Like motion above, only
    // redraws when the offset actually moved (a wheel tick at either clamp
    // edge is a no-op).
    // `integer_y`, not `y`: SDL carries both, and the detent logic below wants whole clicks,
    // not the fractional scroll a trackpad reports.
    if let Event::MouseWheel { integer_y: wheel_y, .. } = event {
        input.nav_focus.claim();
        // Anything that navigates by row — a list screen, an open dropdown, a held card's
        // submenu — takes one detent as one Up/Down press, so the wheel reaches every list
        // the D-pad does (see `App::navigates_rows`). Only the two pixel-scrolled surfaces
        // are left to handle themselves.
        if wheel_y != 0 && app.navigates_rows() {
            let menu_ev = if wheel_y > 0 { MenuEvent::Up } else { MenuEvent::Down };
            // Redraw on a move only, like the pixel-scrolled arms below: a row list gets one
            // for free off the focus pop `list_nav` arms, but an open dropdown has no such
            // animation, so its pick would move with nothing on screen following it.
            let before = app.row_focus();
            dispatch_menu_event(app, menu_ev, layout);
            *dirty |= app.row_focus() != before;
        } else {
            match app.nav.screen {
                Screen::About => {
                    /// Licence-wall px per wheel detent — a few lines at a time.
                    const ABOUT_WHEEL_STEP: i32 = 90;
                    *dirty |= app.scroll_about_by(-wheel_y * ABOUT_WHEEL_STEP, w, h);
                }
                Screen::Home => {
                    /// Grid px scrolled per wheel detent — about a third of a card
                    /// row, so a few ticks walk one row.
                    const WHEEL_STEP: i32 = 120;
                    *dirty |= app.scroll_grid_by(-wheel_y * WHEEL_STEP, w, h);
                }
                _ => {}
            }
        }
        return EventAction::Next;
    }
    // OK pressed while a non-pointer input still owns focus (see `NavFocus`): the user is
    // acting on the row they just navigated to, not on whatever the pointer drifted over on the
    // way — so it confirms the focused row, exactly as the remote's OK would.
    if let Event::MouseButtonDown {
        mouse_btn: sdl3::mouse::MouseButton::Left,
        ..
    } = event
    {
        // Either way the claim ends here: a click that didn't spend it is deliberate input at
        // its own position, and must take focus there before a press path resolves hover.
        if std::mem::take(&mut input.nav_focus).held {
            input.nav_click = true;
            *dirty = true;
            return dispatch_menu_event(app, MenuEvent::Confirm, layout);
        }
    }
    // The release of that same click carries no second action.
    if matches!(
        event,
        Event::MouseButtonUp {
            mouse_btn: sdl3::mouse::MouseButton::Left,
            ..
        }
    ) && std::mem::take(&mut input.nav_click)
    {
        return EventAction::Next;
    }
    if let Some(action) = card_hold_gate(app, &event, input, layout, dirty) {
        return action;
    }
    // Any other event might change what's on screen (focus/hover, a typed
    // digit, a screen transition) — simplest to mark dirty for all of them
    // rather than re-litigate that per event kind.
    *dirty = true;
    match event {
        // The Magic Remote's pointer delivers OK as a plain mouse click.
        // Dispatch it on press: there is no hold gesture to disambiguate any
        // more (per-host actions have their own ⋯ button — see
        // `ui::widgets::sidebar_menu_button_rect`), so nothing needs to wait for the
        // release.
        Event::MouseButtonDown {
            mouse_btn: sdl3::mouse::MouseButton::Left,
            x,
            y,
            ..
        } => {
            // A grid-card click resolves via `confirm_grid_card`'s async check,
            // same as a remote Confirm — never a target directly here.
            return if app.handle_mouse_click(x as i32, y as i32, w, h).is_some() {
                EventAction::Launch
            } else {
                EventAction::Next
            };
        }
        // Ends a Bitrate drag armed in `handle_mouse_click` — without this the slider
        // would keep tracking the pointer (or a stale last position) past the release.
        Event::MouseButtonUp {
            mouse_btn: sdl3::mouse::MouseButton::Left,
            ..
        } => {
            app.end_slider_drag();
        }
        // Direct digit entry via the remote's number buttons — PIN entry on the
        // pairing screen, IP entry on the add/edit-host screens.
        Event::KeyDown { keycode: Some(k), .. }
            if matches!(
                app.nav.screen,
                Screen::Pairing | Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::RenameProfile
            ) =>
        {
            if let Some(digit) = crate::platform::webos::input::digit_key_value(k) {
                match app.nav.screen {
                    Screen::Pairing => app.enter_pin_digit(digit),
                    Screen::AddHost | Screen::EditHost => app.enter_add_host_digit(digit),
                    // A digit is an ordinary character in a name.
                    Screen::RenameCollection => {
                        app.enter_collection_name_char((b'0' + digit) as char);
                    }
                    Screen::RenameProfile => app.enter_profile_name_char((b'0' + digit) as char),
                    _ => unreachable!(),
                }
                return EventAction::Next;
            }
            // Backspace is a *text* key here, not navigation: `menu_event_for_key` maps it to
            // Back (a remote whose Back arrives as Backspace still has to work), which would
            // close the modal on every attempt to correct a typo — from a USB keyboard and
            // from webOS's on-screen keyboard alike, since the OSK's erase key is delivered
            // as a synthetic Backspace rather than as `TextInput`. Consumed only when the
            // screen had something to erase, so on such a remote Backspace still leaves an
            // empty field the way Back does.
            if k == sdl3::keyboard::Keycode::Backspace && app.erase_text_entry() {
                return EventAction::Next;
            }
        }
        // Text committed by webOS's on-screen keyboard (see `SOFTWARE_KEYBOARD`
        // in this module): the OSK delivers whole strings via SDL_TEXTINPUT, not
        // synthetic key events, so it has to be consumed separately from the
        // number-pad path above. Each character is fed through the same entry
        // state machine, so typing "192.168.1.5" on the keyboard and tapping it
        // out on the remote produce identical results.
        Event::TextInput { ref text, .. } => {
            match app.nav.screen {
                Screen::Pairing => {
                    for d in text.chars().filter_map(|c| c.to_digit(10)) {
                        app.enter_pin_digit(d as u8);
                    }
                }
                Screen::AddHost | Screen::EditHost => {
                    for c in text.chars() {
                        app.enter_host_address_char(c);
                    }
                }
                Screen::RenameCollection => {
                    for c in text.chars() {
                        app.enter_collection_name_char(c);
                    }
                }
                Screen::RenameProfile => {
                    for c in text.chars() {
                        app.enter_profile_name_char(c);
                    }
                }
                _ => {}
            }
            return EventAction::Next;
        }
        _ => {}
    }
    let menu_ev = match event {
        // The OS autorepeats of a held key are dropped by `press_nav`, so the remote steps at
        // the rate this timer sets rather than at webOS's.
        Event::KeyDown { keycode: Some(k), .. } => {
            let ev = edge_trigger_back(
                crate::platform::webos::input::menu_event_for_key(k),
                &mut input.menu_back_down,
            );
            input.press_nav(NavSource::Key(k), ev)
        }
        Event::KeyUp { keycode: Some(k), .. } => {
            if crate::platform::webos::input::menu_event_for_key(k) == Some(MenuEvent::Back) {
                input.menu_back_down = false;
            }
            input.release_nav_repeat(NavSource::Key(k));
            None
        }
        Event::GamepadButtonDown { button, .. } => {
            let ev = edge_trigger_back(
                crate::platform::webos::input::menu_event_for_button(button),
                &mut input.menu_back_down,
            );
            input.press_nav(NavSource::Button(button), ev)
        }
        Event::GamepadButtonUp { button, .. } => {
            if crate::platform::webos::input::menu_event_for_button(button) == Some(MenuEvent::Back) {
                input.menu_back_down = false;
            }
            input.release_nav_repeat(NavSource::Button(button));
            None
        }
        Event::GamepadAxisMotion { axis, value, .. } => {
            // Back at centre ends the hold this axis was running; a fresh deflection past the
            // deadzone starts one.
            if crate::platform::webos::input::StickMenuNav::centred(value) {
                input.release_nav_repeat(NavSource::Axis(axis));
            }
            let ev = input.stick_nav.axis_event(axis, value);
            input.press_nav(NavSource::Axis(axis), ev)
        }
        _ => None,
    };
    let Some(menu_ev) = menu_ev else {
        return EventAction::Next;
    };
    // A key/pad event moves focus without moving the cursor, exactly as the wheel does: hold
    // the pointer off until it is deliberately moved, so rows sliding under a resting cursor
    // (or a Back that lands one under it) cannot steal the focus back.
    input.nav_focus.claim();
    dispatch_menu_event(app, menu_ev, layout)
}

#[cfg(test)]
mod remote_gate_tests {
    use super::*;
    use sdl3::event::Event;
    use sdl3::keyboard::{Keycode, Scancode};

    use crate::platform::webos::input::{test_key_event, RemoteKey};

    fn key(scancode: Option<Scancode>, keycode: Option<Keycode>, down: bool, repeat: bool) -> Event {
        test_key_event(scancode, keycode, 0, down, repeat)
    }

    fn up_key(down: bool, repeat: bool) -> Event {
        key(Some(Scancode::Up), Some(Keycode::Up), down, repeat)
    }

    /// The remote's Back as SDL3 actually delivers it: nothing but `raw`.
    fn back_key(down: bool) -> Event {
        test_key_event(None, None, RemoteKey::Back.to_raw(), down, false)
    }

    #[test]
    fn a_remote_press_admits_its_key_its_repeats_and_its_release() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        gate.pressed(103, t);
        let later = t + Duration::from_millis(20);
        assert!(gate.admits(&up_key(true, false), later));
        assert!(gate.admits(&up_key(true, true), later));
        assert!(gate.admits(&up_key(false, false), later));
        assert!(
            !gate.admits(&up_key(true, false), later),
            "one press admits one key-down"
        );
    }

    #[test]
    fn a_pad_echo_has_no_remote_press_behind_it() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        assert!(!gate.admits(&up_key(true, false), t));
        assert!(!gate.admits(&up_key(true, true), t));
        assert!(!gate.admits(&up_key(false, false), t));
        // The remote's Back: no scancode, no keycode, identified only by `raw` (see
        // `remote_evdev_code`). Gated like any other key - the echo is refused, the real press admitted.
        let back = || back_key(true);
        assert!(!gate.admits(&back(), t));
        gate.pressed(RemoteKey::Back.to_raw(), t);
        assert!(gate.admits(&back(), t));
    }

    #[test]
    fn rejected_back_echo_does_not_swallow_the_real_press() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        let mut keys = crate::platform::webos::input::RemoteKeys::default();
        let down = back_key(true);
        assert_eq!(keys.edge(&down, gate.admits(&down, t)), None);
        gate.pressed(RemoteKey::Back.to_raw(), t);
        assert_eq!(keys.edge(&down, gate.admits(&down, t)), Some((RemoteKey::Back, true)));
        let up = back_key(false);
        assert_eq!(keys.edge(&up, gate.admits(&up, t)), Some((RemoteKey::Back, false)));
        assert_eq!(keys.edge(&up, gate.admits(&up, t)), None);
    }

    #[test]
    fn an_unclaimed_press_expires() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        // An OK the pointer turned into a click: no key-down ever claims it.
        gate.pressed(28, t);
        let enter = key(Some(Scancode::Return), Some(Keycode::Return), true, false);
        assert!(!gate.admits(&enter, t + Duration::from_millis(300)));
    }

    #[test]
    fn keys_the_remote_lacks_never_pass_and_other_events_always_do() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        let esc = key(Some(Scancode::Escape), Some(Keycode::Escape), true, false);
        assert!(!gate.admits(&esc, t));
        assert!(gate.admits(&Event::Quit { timestamp: 0 }, t));
    }

    /// No remote node yet, or none this app can open: the old behaviour, every key passes.
    #[test]
    fn a_gate_without_a_remote_node_admits_every_key() {
        let (mut gate, t) = (RemoteGate::default(), Instant::now());
        assert!(gate.admits(&up_key(true, false), t));
        assert!(gate.admits(&up_key(false, false), t));
        let esc = key(Some(Scancode::Escape), Some(Keycode::Escape), true, false);
        assert!(gate.admits(&esc, t));
    }
}

#[cfg(test)]
mod disconnect_chord_tests {
    use super::*;
    use sdl3::gamepad::Button::{self, Back, Guide, LeftShoulder, RightShoulder, Start};

    fn held(buttons: &[Button]) -> DisconnectChord {
        let mut chord = DisconnectChord::default();
        for &b in buttons {
            chord.set(b, true);
        }
        chord
    }

    #[test]
    fn only_the_full_chord_arms_the_dialog() {
        let mut chord = held(&[LeftShoulder, RightShoulder, Start, Back]);
        assert!(chord.held_for(Duration::ZERO));
        chord.set(Start, false);
        assert!(!chord.held_for(Duration::ZERO), "a released button kept the hold armed");
        for partial in [
            &[Guide][..],
            &[LeftShoulder, RightShoulder],
            &[Start, Back],
            &[Guide, LeftShoulder, RightShoulder, Start],
        ] {
            assert!(!held(partial).held_for(Duration::ZERO), "{partial:?} armed the dialog");
        }
    }
}

#[cfg(test)]
mod sdl_tests {
    use super::*;

    #[test]
    fn ime_area_uses_window_units() {
        let layout = LayoutBox { w: 1536, h: 864 };
        let rect = crate::ui::render::Rect::new(100, 200, 400, 60);
        assert_eq!(
            layout.window_rect(rect, (1920, 1080)),
            sdl3::rect::Rect::new(125, 250, 500, 75)
        );
        assert_eq!(
            layout.window_rect(rect, (1536, 864)),
            sdl3::rect::Rect::new(100, 200, 400, 60)
        );
    }

    #[test]
    fn text_and_audio_lifetimes() {
        use crate::core::media::{AudioSink, Samples};
        use crate::platform::webos::audio::AudioPlayer;
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc;

        // SDL's main-thread state and driver hints are process-global. Isolate from other tests.
        const CHILD: &str = "PUNKTFUNK_SDL_LIFETIME_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::input::sdl_tests::text_and_audio_lifetimes",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("SDL_AUDIO_DRIVER", "dummy")
                .env("SDL_VIDEO_DRIVER", "dummy")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let sdl = sdl3::init().unwrap();
        let video = sdl.video().unwrap();
        let window = video.window("input test", 1920, 1080).hidden().build().unwrap();
        let util = video.text_input();
        let mut input = TextInputController::new(video.text_input());
        let first = sdl3::rect::Rect::new(100, 700, 400, 60);
        let moved = sdl3::rect::Rect::new(100, 400, 400, 60);
        input.set_active(text_input_options(Screen::AddHost), Some(first), &window);
        assert!(util.is_active(&window));
        assert_eq!(util.rect(&window).unwrap(), (first, IME_CURSOR));
        input.set_active(text_input_options(Screen::AddHost), Some(moved), &window);
        assert!(util.is_active(&window));
        assert_eq!(util.rect(&window).unwrap(), (moved, IME_CURSOR));
        input.set_active(None, None, &window);
        assert!(!util.is_active(&window));

        let audio = sdl.audio().unwrap();
        for channels in [2, 6, 2, 6] {
            let depth = Arc::new(AtomicU32::new(0));
            let (player, sink) = AudioPlayer::new(&audio, channels, depth.clone()).unwrap();
            let pcm = vec![0.1; 240 * usize::from(channels)];
            for _ in 0..16 {
                sink.feed(Samples::F32(&pcm), 0).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while depth.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(depth.load(Ordering::Relaxed) > 0, "SDL callback never consumed PCM");
            // Tear down while playback is active, then reopen on the same subsystem.
            drop(player);
        }
    }
}
