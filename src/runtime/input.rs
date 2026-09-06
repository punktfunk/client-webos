//! Pre-stream menu input plumbing: hold/chord gestures, the confirm dialog, and the
//! SDL-event → `MenuEvent` routing shared by the `ui_flow` and `stream` loops.
//!
//! Split out of `runtime/mod.rs` (which keeps run/connect/signals/log-overlay). Re-exported
//! there via `use input::*` so the sibling loop modules pick these up through `use super::*`.

use super::*;

/// How long a controller shortcut ([`DisconnectChord`]) must be held before its dialog
/// opens — the in-stream disconnect dialog while streaming, the quit dialog in the menu.
/// Every button in these shortcuts is also real game input, so a hold — not a press — is
/// the only safe trigger (L1+R1 in particular is a common in-game bind); the hold window
/// is the margin against a stream dying mid-play. Shared by both loops so the remote's
/// held-Back EXIT gesture and the controller chord feel the same in either context —
/// 1s to match webOS's own long-press threshold on the EXIT gesture.
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

/// The gamepad routes to the disconnect dialog (streaming) or quit dialog (menu): Guide,
/// both shoulders, or Start+Back, each held for [`EXIT_HOLD`].
///
/// Tracked as button state rather than read back from SDL because SDL only reports
/// transitions here — and a chord needs to know what is down *now*, not what changed
/// last. Three shortcuts share one timer: the gesture is "some disconnect chord has been
/// complete for long enough", so sliding from one chord into another (releasing Start
/// while both shoulders stay down) is one continuous hold rather than a restart.
#[derive(Default)]
pub(super) struct DisconnectChord {
    guide: bool,
    left_shoulder: bool,
    right_shoulder: bool,
    start: bool,
    back: bool,
    /// When the currently-held chord became complete; `None` when none is.
    since: Option<Instant>,
}

impl DisconnectChord {
    /// Records one button transition and arms or disarms the hold timer.
    pub(super) fn set(&mut self, button: sdl2::controller::Button, down: bool) {
        use sdl2::controller::Button;
        match button {
            Button::Guide => self.guide = down,
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
        self.guide || (self.left_shoulder && self.right_shoulder) || (self.start && self.back)
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

/// Fires once when `down` changes from false to true.
pub(super) fn rising_edge(down: bool, prev: &mut bool) -> bool {
    let fired = down && !*prev;
    *prev = down;
    fired
}

/// Rising-edge detect on a raw webOS scancode (polled since these sit outside
/// rust-sdl2's `Scancode` enum). `prev` carries the last-frame state across calls.
fn scancode_rising_edge(scancode: i32, prev: &mut bool) -> bool {
    rising_edge(crate::platform::webos::input::webos_scancode_down(scancode), prev)
}

/// Controls the webOS on-screen keyboard for UI and streaming loops.
pub(super) struct TextInputController {
    util: sdl2::keyboard::TextInputUtil,
    /// Last requested SDL state. The compositor may dismiss the panel independently.
    active: bool,
}

impl TextInputController {
    pub(super) fn new(util: sdl2::keyboard::TextInputUtil) -> Self {
        Self { util, active: false }
    }

    pub(super) fn has_screen_keyboard_support(&self) -> bool {
        self.util.has_screen_keyboard_support()
    }

    pub(super) fn is_shown(&self, window: &sdl2::video::Window) -> bool {
        self.util.is_screen_keyboard_shown(window)
    }

    /// Matches text input state to the active UI screen.
    pub(super) fn set_active(&mut self, want: bool, rect: Option<sdl2::rect::Rect>) {
        if want == self.active {
            return;
        }
        self.active = want;
        if want {
            if let Some(r) = rect {
                self.util.set_rect(r);
            }
            self.util.start();
        } else {
            self.util.stop();
        }
        tracing::debug!("text input requested: {want}");
    }

    /// Raises the panel unconditionally, even if `active` already says it's up — the stream
    /// loop never polls `is_shown`, so `active` can't tell a still-open panel from one Back
    /// silently dismissed underneath it. Closing it again is the TV's job — Back dismisses it
    /// through webOS's own IME, and this app never calls `stop()` in response to that.
    pub(super) fn raise(&mut self, rect: sdl2::rect::Rect) {
        self.active = false;
        self.set_active(true, Some(rect));
    }

    /// Stops text input at loop exit.
    pub(super) fn stop(&mut self) {
        self.util.stop();
        self.active = false;
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
/// `run_ui_flow` now starts text input whenever a screen that edits text is open and
/// stops it on the way out, and `SDL_SetTextInputRect` tells webOS where the field is
/// so the panel doesn't cover it. Committed text arrives as `Event::TextInput`.
pub(super) fn text_input_screen(screen: Screen) -> bool {
    matches!(
        screen,
        Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::RenameProfile
    )
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
    Key(sdl2::keyboard::Keycode),
    Button(sdl2::controller::Button),
    Axis(sdl2::controller::Axis),
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
pub(super) fn is_menu_press(event: &sdl2::event::Event, want: MenuEvent, allow_repeat: bool) -> bool {
    use sdl2::event::Event;
    match *event {
        Event::KeyDown {
            keycode: Some(k),
            repeat,
            ..
        } => (allow_repeat || !repeat) && crate::platform::webos::input::menu_event_for_key(k) == Some(want),
        Event::ControllerButtonDown { button, .. } => {
            crate::platform::webos::input::menu_event_for_button(button) == Some(want)
        }
        _ => false,
    }
}

/// The release half of [`is_menu_press`], for the gestures that resolve on the way up.
pub(super) fn is_menu_release(event: &sdl2::event::Event, want: MenuEvent) -> bool {
    use sdl2::event::Event;
    match *event {
        Event::KeyUp { keycode: Some(k), .. } => crate::platform::webos::input::menu_event_for_key(k) == Some(want),
        Event::ControllerButtonUp { button, .. } => {
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
    /// The wheel's claim on focus, while it has one.
    wheel: WheelFocus,
    /// A click was spent confirming the scrolled focus — its release is the same press and
    /// must not act a second time (as a tap, or as the end of a slider drag).
    wheel_click: bool,
    /// The held direction being autorepeated, if any.
    nav_repeat: Option<NavRepeat>,
}

impl UiInput {
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

/// How far the pointer must travel from where it sat when the wheel took focus before it
/// takes focus back, in screen px. Scrolling emits motion at the same time — the Magic Remote
/// keeps moving while its wheel turns, and a hand on a HID mouse never holds still — which
/// would hand focus to whatever row is under the cursor and undo the scroll. Measured from a
/// fixed anchor rather than summed, so the wobble of a hand holding still never adds up to a
/// release, and generous enough to sit outside it; a deliberate reach for a row clears it in
/// the first few frames of the movement.
const WHEEL_RELEASE_PX: i32 = 96;

/// The wheel's claim on focus: it holds from the first detent until the pointer is
/// deliberately moved (see [`WHEEL_RELEASE_PX`]) or a click spends it. No clock — a user who
/// scrolls and then sits still keeps the row they scrolled to however long they read it.
#[derive(Default)]
struct WheelFocus {
    /// Where the pointer was when the claim started. `None` while held but not yet placed:
    /// SDL's wheel event carries scroll deltas, not a position, so the anchor is the first
    /// motion after the detent.
    anchor: Option<(i32, i32)>,
    held: bool,
}

impl WheelFocus {
    /// A detent arrived: the wheel owns focus from here, measured afresh. Re-anchoring on
    /// every detent is what makes a long scroll safe — drift that stayed under the threshold
    /// during the last one must not be carried forward into this one and add up to a release.
    fn claim(&mut self) {
        self.held = true;
        self.anchor = None;
    }

    /// Gives focus back to the pointer.
    fn release(&mut self) {
        *self = Self::default();
    }

    /// Whether the wheel still owns focus — pointer motion ignored, and a click confirming
    /// what was scrolled to rather than what happens to be under the cursor.
    fn holds(&self) -> bool {
        self.held
    }

    /// Whether this motion should be ignored. The first one after a detent anchors the claim;
    /// a later one far enough from that anchor is a deliberate reach and ends it.
    fn swallows_motion(&mut self, x: i32, y: i32) -> bool {
        if !self.held {
            return false;
        }
        let Some((ax, ay)) = self.anchor else {
            self.anchor = Some((x, y));
            return true;
        };
        if (x - ax).pow(2) + (y - ay).pow(2) > WHEEL_RELEASE_PX.pow(2) {
            self.release();
            return false;
        }
        true
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
    event: &sdl2::event::Event,
    input: &mut UiInput,
    display_mode: sdl2::video::DisplayMode,
    dirty: &mut bool,
) -> Option<EventAction> {
    use sdl2::event::Event;
    let (w, h) = (display_mode.w as u32, display_mode.h as u32);
    // The Magic Remote's pointer delivers OK as a left mouse button, so give it the same
    // hold gesture the D-pad's Confirm has: a press on a hovered Home card starts the hold
    // and is swallowed (the card's menu opens on the hold-elapsed tick, same as `CARD_HOLD`
    // above), and the tap/launch comes only from the release. A press on anything else falls
    // through to the normal click path.
    if let Event::MouseButtonDown {
        mouse_btn: sdl2::mouse::MouseButton::Left,
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
        // Land hover focus on the press point first — a button press can jostle the
        // remote off the last motion position.
        *dirty |= app.handle_mouse_motion(x, y, w, h);
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
        mouse_btn: sdl2::mouse::MouseButton::Left,
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
        return Some(if app.handle_mouse_click(x, y, w, h).is_some() {
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
        if matches!(app.nav.screen, Screen::Home)
            && app.card_menu.is_none()
            && arm_card_hold(input, app, display_mode.w as u32)
        {
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
    let launched = tapped && app.press(display_mode.w as u32, display_mode.h as u32).is_some();
    Some(if launched {
        EventAction::Launch
    } else {
        EventAction::Next
    })
}

/// Feeds a resolved `MenuEvent` to the app, translating what it returns into this
/// loop's terms. The per-screen routing is `App::handle_menu_event`.
pub(super) fn dispatch_menu_event(
    app: &mut App,
    menu_ev: MenuEvent,
    display_mode: sdl2::video::DisplayMode,
) -> EventAction {
    let (w, h) = (display_mode.w as u32, display_mode.h as u32);
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
    event: sdl2::event::Event,
    input: &mut UiInput,
    display_mode: sdl2::video::DisplayMode,
    dirty: &mut bool,
) -> EventAction {
    use sdl2::event::Event;
    let (w, h) = (display_mode.w as u32, display_mode.h as u32);
    // The Magic Remote's pointer mode surfaces as a plain SDL2 MouseMotion
    // event fired continuously while the remote is moving — unlike every other
    // event handled below, redraw only if the motion actually changed the
    // focused/hovered element, not on every no-op tick.
    if let Event::MouseMotion { x, y, .. } = event {
        if !input.wheel.swallows_motion(x, y) {
            *dirty |= app.handle_mouse_motion(x, y, w, h);
        }
        return EventAction::Next;
    }
    // The Magic Remote's scroll wheel — scrolls the game grid on Home (wheel
    // y > 0 = "scroll up" = content moves down). Like motion above, only
    // redraws when the offset actually moved (a wheel tick at either clamp
    // edge is a no-op).
    if let Event::MouseWheel { y: wheel_y, .. } = event {
        input.wheel.claim();
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
            dispatch_menu_event(app, menu_ev, display_mode);
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
    // OK pressed while the wheel still owns focus (see `WheelFocus`): the user is
    // acting on the row they just scrolled to, not on whatever the pointer drifted over while
    // they scrolled — so it confirms the focused row, exactly as the remote's OK would, and
    // the pointer position is ignored. Ends the window: the click is the end of the gesture.
    if let Event::MouseButtonDown {
        mouse_btn: sdl2::mouse::MouseButton::Left,
        ..
    } = event
    {
        if input.wheel.holds() {
            input.wheel.release();
            input.wheel_click = true;
            *dirty = true;
            return dispatch_menu_event(app, MenuEvent::Confirm, display_mode);
        }
        // Otherwise a click is deliberate input at its own position: it takes focus at the
        // press point, so drop the claim before either press path resolves hover focus.
        input.wheel.release();
    }
    // The release of that same click carries no second action.
    if matches!(
        event,
        Event::MouseButtonUp {
            mouse_btn: sdl2::mouse::MouseButton::Left,
            ..
        }
    ) && std::mem::take(&mut input.wheel_click)
    {
        return EventAction::Next;
    }
    if let Some(action) = card_hold_gate(app, &event, input, display_mode, dirty) {
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
            mouse_btn: sdl2::mouse::MouseButton::Left,
            x,
            y,
            ..
        } => {
            // A grid-card click resolves via `confirm_grid_card`'s async check,
            // same as a remote Confirm — never a target directly here.
            return if app.handle_mouse_click(x, y, w, h).is_some() {
                EventAction::Launch
            } else {
                EventAction::Next
            };
        }
        // Ends a Bitrate drag armed in `handle_mouse_click` — without this the slider
        // would keep tracking the pointer (or a stale last position) past the release.
        Event::MouseButtonUp {
            mouse_btn: sdl2::mouse::MouseButton::Left,
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
            if k == sdl2::keyboard::Keycode::Backspace && app.erase_text_entry() {
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
        Event::ControllerButtonDown { button, .. } => {
            let ev = edge_trigger_back(
                crate::platform::webos::input::menu_event_for_button(button),
                &mut input.menu_back_down,
            );
            input.press_nav(NavSource::Button(button), ev)
        }
        Event::ControllerButtonUp { button, .. } => {
            if crate::platform::webos::input::menu_event_for_button(button) == Some(MenuEvent::Back) {
                input.menu_back_down = false;
            }
            input.release_nav_repeat(NavSource::Button(button));
            None
        }
        Event::ControllerAxisMotion { axis, value, .. } => {
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
    dispatch_menu_event(app, menu_ev, display_mode)
}
