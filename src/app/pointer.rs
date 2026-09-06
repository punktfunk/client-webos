//! Pointer input: what the Magic Remote's cursor is over, and what a click there does.
//!
//! The webOS remote is a pointer as well as a d-pad, so every focusable widget needs a
//! hit test beside its `MenuEvent` handler. Both entry points ([`App::handle_mouse_motion`]
//! and [`App::handle_mouse_click`]) are one `match self.nav.screen`, and each arm measures
//! against the same `app::view` geometry the painter draws with — hover previews exactly
//! what a click will hit because neither side derives the rects itself.
//!
//! Split out of `app/mod.rs`, which held this next to the state machine and the whole
//! render path.

use std::time::Instant;

use crate::app::nav::ScreenKey;
use crate::app::screens::rowbuttons::RowButton;
use crate::app::{view, App, ConnectTarget, HomeFocus, PairingFocus, Screen};
use crate::ui;
use crate::ui::render::Rect;

/// What a hover moved. `any` is whether anything visible changed at all; `row` is whether
/// the *row* under the pointer changed. Stepping onto a button a row carries (its ⋯, a
/// collection's drag/rename/remove) leaves the row itself put, and a row that keeps focus
/// must not replay its focus pop — the same rule the D-pad's `step_row_button` follows, and
/// the sidebar follows when focus moves between a host row and its own ⋯ button.
#[derive(Clone, Copy)]
struct HoverChange {
    any: bool,
    row: bool,
}

impl HoverChange {
    const NONE: Self = Self { any: false, row: false };

    /// A hover that either moved the row or moved nothing.
    const fn row(changed: bool) -> Self {
        Self {
            any: changed,
            row: changed,
        }
    }

    /// A hover on a row list, where the row and the button it carries move independently.
    const fn split(row: bool, button: bool) -> Self {
        Self {
            any: row || button,
            row,
        }
    }
}

impl App {
    /// Handed to `SDL_SetTextInputRect` by the render loop. `None` off the text forms, which
    /// are the only screens that take text input at all.
    pub fn address_field_rect(&self, screen_w: u32, screen_h: u32) -> Option<Rect> {
        let form = self.text_form()?;
        let l = crate::app::draw::form::layout(
            &self.fonts,
            screen_w as f32,
            screen_h as f32,
            crate::app::draw::scale(screen_h),
            &form.subtitle,
            form.hint.is_some(),
            self.keyboard_shown,
        );
        Some(l.field_rect())
    }

    /// Updates focus/hover to whatever the Magic Remote's pointer is over, returning
    /// whether that changed anything visible — Magic Remote pointer mode fires
    /// `MouseMotion` continuously while moving, so callers redraw only when this is
    /// `true` rather than on every event.
    pub fn handle_mouse_motion(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> bool {
        // A press already landed on a slider track (see `handle_mouse_click`) — every motion
        // until release drags that thumb, rather than re-hit-testing the row list under a
        // pointer that may have wandered off it.
        if self.settings_ui.slider_drag {
            self.drag_slider(x);
            return true;
        }
        let focus = self.hover_focus_at(x, y, screen_w, screen_h);
        // Parity with the D-pad: a hover that moves modal focus to another row replays the
        // focus-pop zoom (and shows the new row's caption). Home drives its own `focus_anim`
        // instead, so it's excluded. An open dropdown is excluded too — hover there only
        // moves the option cursor, so popping the parent row (as the D-pad also declines to)
        // is wrong.
        if focus.row && !matches!(self.nav.screen, Screen::Home) {
            self.render.modal.focus_anim = Some(Instant::now());
        }
        let close_changed = self.hover_close_at(x, y, screen_w, screen_h);
        focus.any || close_changed
    }

    /// Moves the positional focus/selection onto whatever interactive element sits
    /// under the pointer, so the Magic Remote's pointer highlights elements on hover
    /// exactly where a click would land. Returns whether the selection actually
    /// moved. Hovering empty space (gaps, row padding, the area between rows) leaves
    /// the current selection put rather than clearing it, so a resting pointer never
    /// fights the D-pad.
    fn hover_focus_at(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> HoverChange {
        match self.nav.screen {
            // The held card's submenu takes hover whole, like an open dropdown does — the
            // grid behind it must not steal focus out from under an open menu.
            Screen::Home if self.card_menu.is_some() => {
                let Some(row) = self.card_menu_row_at(x, y, screen_w, screen_h) else {
                    return HoverChange::NONE;
                };
                let menu = self.card_menu.as_mut().expect("guarded by the arm");
                let changed = menu.focused != row;
                menu.focus(row);
                HoverChange::row(changed)
            }
            Screen::Home => {
                // The ⋯ button sits inside its row, so it's tested first — same order
                // as `handle_mouse_click`, so hover previews exactly what a click hits.
                if let Some(idx) =
                    view::sidebar::hit_test_menu_button(x, y, &self.hosts.entries, self.sidebar_len(), screen_h)
                {
                    return HoverChange::row(self.set_home_focus(HomeFocus::SidebarMenu(idx)));
                }
                if let Some(idx) = view::sidebar::hit_test_row(x, y, self.sidebar_len(), screen_h) {
                    return HoverChange::row(self.set_home_focus(HomeFocus::Sidebar(idx)));
                }
                let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
                let columns = view::home::grid_columns(available_w);
                if let Some(idx) = self.hit_test_grid_card(x, y, columns, available_w) {
                    // Padding after a partial pinned row isn't a real card — nothing to land on.
                    if self.is_grid_card(idx, columns) {
                        return HoverChange::row(self.set_home_focus(HomeFocus::Grid(idx)));
                    }
                }
                HoverChange::NONE
            }
            // Dropdown case already handled above.
            // The two scrolling lists share one hit test and one cursor lookup.
            Screen::Collections => {
                // A held row follows the d-pad, not the pointer: hovering elsewhere must not
                // drag the cursor out from under it.
                if self.screens.collections.dragging.is_some() {
                    return HoverChange::NONE;
                }
                let Some(row) = self.kit_list_row_at(x, y) else {
                    return HoverChange::NONE;
                };
                let button = self
                    .kit_list_button_at(x, y)
                    .filter(|(r, _)| *r == row)
                    .map(|(_, b)| RowButton::Trailing(b));
                let key = ScreenKey::Collections;
                let row_changed = self.nav.cursor(key) != row;
                let button_changed = self.screens.row_button != button;
                self.nav.set_cursor(key, row);
                self.screens.row_button = button;
                HoverChange::split(row_changed, button_changed)
            }
            Screen::SettingsPage => {
                let l = crate::app::draw::settings::layout(
                    screen_w as f32,
                    screen_h as f32,
                    crate::app::draw::scale(screen_h),
                );
                if let Some(i) = l.entry_at(x, y) {
                    let was = (self.screens.settings_page.page, self.screens.settings_page.column);
                    self.screens.settings_page.column = true;
                    self.show_page(crate::app::state::settingspage::Page::ALL[i]);
                    return HoverChange::row(was != (self.screens.settings_page.page, true));
                }
                let Some(i) = self.kit_list_row_at(x, y) else {
                    return HoverChange::NONE;
                };
                let changed = self.nav.cursor(ScreenKey::SettingsPage) != i || self.screens.settings_page.column;
                self.screens.settings_page.column = false;
                self.nav.set_cursor(ScreenKey::SettingsPage, i);
                HoverChange::row(changed)
            }
            // A list drawn on the kit: hover focuses the row under the pointer, through the
            // list's own last-drawn rects (`app::draw::list`).
            screen @ (Screen::HostMenu | Screen::HostPower | Screen::PickProfile) => {
                let Some(i) = self.kit_list_row_at(x, y) else {
                    return HoverChange::NONE;
                };
                let key = ScreenKey::of(screen);
                let changed = self.nav.cursor(key) != i;
                self.nav.set_cursor(key, i);
                HoverChange::row(changed)
            }
            // Identical row-list geometry; only which focus field they carry differs.
            Screen::HdrCalibration => {
                let Some(row) = self.kit_list_row_at(x, y) else {
                    return HoverChange::NONE;
                };
                let button = self
                    .kit_list_button_at(x, y)
                    .filter(|(r, _)| *r == row)
                    .map(|(_, b)| RowButton::Trailing(b));
                // Same per-screen field table the keyboard path indexes, so hover and
                // D-pad focus can never name different fields.
                let Some(focused) = self.list_modal_focused_mut() else {
                    return HoverChange::NONE;
                };
                let row_changed = *focused != row;
                *focused = row;
                let button_changed = self.screens.row_button != button;
                self.screens.row_button = button;
                HoverChange::split(row_changed, button_changed)
            }
            Screen::Pairing => {
                let l = self.pair_layout(screen_w, screen_h);
                if l.on_button(x, y) {
                    let changed = self.screens.pairing_focus != PairingFocus::RequestAccess;
                    self.screens.pairing_focus = PairingFocus::RequestAccess;
                    HoverChange::row(changed)
                } else if let Some(i) = l.digit_at(x, y) {
                    let changed = self.screens.pairing_focus != PairingFocus::Pin || self.screens.pin_digit_index != i;
                    self.screens.pairing_focus = PairingFocus::Pin;
                    self.screens.pin_digit_index = i;
                    HoverChange::row(changed)
                } else {
                    HoverChange::NONE
                }
            }
            // Two-button confirm modals (Forget/SendLogs/Wake/finished SpeedTest — the same
            // modal type as the in-stream Disconnect dialog): hovering a button focuses it,
            // so the pointer can pick action-vs-Cancel, not just confirm whatever the D-pad
            // last focused. `confirm_subtitle` is `None` for the variants with no buttons up
            // (a Wake with no MAC, a test still running), which reads as nothing to hover.
            screen if crate::app::draw::ported(screen) => {
                let Some(i) = self.dialog_layout(screen_w, screen_h).and_then(|l| l.button_at(x, y)) else {
                    return HoverChange::NONE;
                };
                HoverChange::row(self.set_confirm_focused(i))
            }
            // No positional focus to move: single-card info/entry modals (AddHost,
            // EditHost, About) and Settings with a dropdown open.
            _ => HoverChange::NONE,
        }
    }

    /// Drags whichever slider the armed press landed on to `x` — the one place that knows which
    /// screen's slider that is, shared by the arming press and every motion after it.
    fn drag_slider(&mut self, x: i32) {
        if self.nav.screen == Screen::HdrCalibration {
            let row = view::hdrcalibration::ROW_SLIDER;
            if let Some(frac) = self.kit_list(Screen::HdrCalibration).track_frac(row, f64::from(x)) {
                self.set_hdr_fraction(frac);
            }
        }
    }

    fn hover_close_at(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> bool {
        if let Some(on_close) = self.ported_close_hit(x, y, screen_w, screen_h) {
            return self.set_hover_close(on_close);
        }
        // Home draws no close button; a stale `true` would swallow every Home click.
        self.render.hover_close = false;
        false
    }

    /// Updates `hover_close` and reports whether it actually changed — every modal
    /// screen's close-button hover check in `handle_mouse_motion` follows this same
    /// shape.
    pub(crate) fn set_hover_close(&mut self, hover_close: bool) -> bool {
        let changed = hover_close != self.render.hover_close;
        self.render.hover_close = hover_close;
        changed
    }

    /// A pointer click confirms whatever's currently hovered/focused, or triggers
    /// Back if the modal's close (X) button itself is what's hovered.
    pub fn handle_mouse_click(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> Option<ConnectTarget> {
        // Re-sync the close-button hover to the click's own position first — a
        // MouseButtonDown can carry a slightly different (x, y) than the last
        // MouseMotion (the physical button press can jostle the remote a little).
        self.handle_mouse_motion(x, y, screen_w, screen_h);
        if self.render.hover_close {
            // The X closes the card outright. On Settings that differs from Back, which first
            // steps from the rows back to the page column.
            if self.nav.screen == Screen::SettingsPage {
                self.screens.settings_page.column = true;
            }
            // Same "what Back means here" as everywhere else — see `back`'s docs.
            return self.back(screen_w, screen_h);
        }
        // Unlike hover, a click DOES move `home_focus`/`settings_focused` — fresh at
        // the click's own position, so it confirms what was actually clicked rather
        // than whatever the keyboard/remote last focused elsewhere. Each arm only
        // *places* focus; the shared `press` below is what confirms it, so a click and an OK
        // press act alike.
        //
        // A click that lands on no row of an open list leaves the focus where it is and
        // confirms *that* — the pointer is often nowhere near what the user is looking at
        // (the wheel scrolls without the cursor following, and a hand holding the remote
        // drifts off the panel), so a press off the rows means "take the highlighted one".
        // Dismissing is Back's job, and the modal's close button's.
        match self.nav.screen {
            Screen::Home if self.card_menu.is_some() => {
                // The held card's submenu is over the grid: a click either picks one of its
                // rows or dismisses it. Nothing underneath is reachable while it is up.
                // Mid-reorder the panel is collapsed and there are no rows: the click means
                // "leave it there", exactly as an OK press does.
                if self.fix_card_position() {
                    return None;
                }
                if let Some(row) = self.card_menu_row_at(x, y, screen_w, screen_h) {
                    if let Some(menu) = self.card_menu.as_mut() {
                        menu.focus(row);
                    }
                }
            }
            Screen::Home => {
                // The ⋯ button sits inside its row, so it has to be tested first or the
                // click just reads as a click on the host.
                if let Some(idx) =
                    view::sidebar::hit_test_menu_button(x, y, &self.hosts.entries, self.sidebar_len(), screen_h)
                {
                    self.set_home_focus(HomeFocus::SidebarMenu(idx));
                    self.open_host_menu(idx);
                    return None;
                }
                if let Some(idx) = view::sidebar::hit_test_row(x, y, self.sidebar_len(), screen_h) {
                    self.set_home_focus(HomeFocus::Sidebar(idx));
                } else {
                    let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
                    let columns = view::home::grid_columns(available_w);
                    // Clicked empty space — either between cards (`?`'s early
                    // `None`) or the padding after a partial pinned row.
                    let idx = self.hit_test_grid_card(x, y, columns, available_w)?;
                    if !self.is_grid_card(idx, columns) {
                        return None;
                    }
                    self.set_home_focus(HomeFocus::Grid(idx));
                }
            }
            Screen::Collections => {
                // A click is one of the inputs that drops a held row — and only that.
                if self.screens.collections.dragging.is_some() {
                    self.commit_collection_drag();
                    return None;
                }
                if let Some(row) = self.kit_list_row_at(x, y) {
                    self.nav.set_cursor(ScreenKey::Collections, row);
                    self.screens.row_button = self
                        .kit_list_button_at(x, y)
                        .filter(|(r, _)| *r == row)
                        .map(|(_, b)| RowButton::Trailing(b));
                } else {
                    // No row under the pointer, so no trailing button either: the press is on
                    // the focused row itself.
                    self.screens.row_button = None;
                }
            }
            Screen::Pairing => {
                // The Magic Remote pointer is the most reliable input on this TV, so the
                // "Request access" button is clickable directly: focus it and confirm. A
                // digit box focuses that digit; a press then steps it, as OK does.
                let l = self.pair_layout(screen_w, screen_h);
                if l.on_button(x, y) {
                    self.screens.pairing_focus = PairingFocus::RequestAccess;
                } else {
                    let i = l.digit_at(x, y)?;
                    self.screens.pairing_focus = PairingFocus::Pin;
                    self.screens.pin_digit_index = i;
                }
            }
            Screen::SettingsPage => {
                let l = crate::app::draw::settings::layout(
                    screen_w as f32,
                    screen_h as f32,
                    crate::app::draw::scale(screen_h),
                );
                if let Some(i) = l.entry_at(x, y) {
                    self.show_page(crate::app::state::settingspage::Page::ALL[i]);
                    self.screens.settings_page.column = false;
                    return None;
                }
                let row = self.kit_list_row_at(x, y)?;
                self.screens.settings_page.column = false;
                self.nav.set_cursor(ScreenKey::SettingsPage, row);
            }
            // A click on a kit list picks the row under it; off the rows it confirms the
            // focused one, as an OK press does.
            screen @ (Screen::HostMenu | Screen::HostPower | Screen::PickProfile) => {
                if let Some(row) = self.kit_list_row_at(x, y) {
                    self.nav.set_cursor(ScreenKey::of(screen), row);
                }
            }
            // The one row is a track and a button, so a press is one or the other. Only the
            // button falls through to `press` below, which is what advances the step.
            Screen::HdrCalibration => {
                let row = self.kit_list_row_at(x, y)?;
                let button = self
                    .kit_list_button_at(x, y)
                    .filter(|(r, _)| *r == row)
                    .map(|(_, b)| RowButton::Trailing(b));
                // Focus follows the click, exactly as it does on a collection row's buttons.
                self.screens.row_button = button;
                if button.is_none() {
                    let p = pf_console_ui::pointer::Pointer {
                        x: f64::from(x),
                        y: f64::from(y),
                        kind: pf_console_ui::pointer::PointerKind::Press,
                    };
                    if self.kit_list(Screen::HdrCalibration).on_track(row, p) {
                        self.settings_ui.slider_drag = true;
                        self.drag_slider(x);
                    }
                    return None;
                }
            }
            // Nothing positional to hit: the confirm dialogs confirm whichever button
            // already has focus.
            Screen::Wake
            | Screen::ForgetHost
            | Screen::SpeedTest
            | Screen::SendLogs
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::DeleteProfile => {}
            // Nothing clickable but the close button (handled above).
            Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::RenameProfile | Screen::About => {
                return None
            }
        }
        self.press(screen_w, screen_h)
    }
}
