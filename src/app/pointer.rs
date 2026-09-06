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
use crate::app::{menu, view, App, ConnectTarget, HomeFocus, PairingFocus, Screen};
use crate::core::event::MenuEvent;
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
    pub fn address_field_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> Option<Rect> {
        let form = self.text_form()?;
        Some(view::textform::field_rect(
            screen_w,
            screen_h,
            fonts,
            &form.subtitle,
            form.hint.is_some(),
            self.keyboard_shown,
        ))
    }

    /// Updates focus/hover to whatever the Magic Remote's pointer is over, returning
    /// whether that changed anything visible — Magic Remote pointer mode fires
    /// `MouseMotion` continuously while moving, so callers redraw only when this is
    /// `true` rather than on every event.
    pub fn handle_mouse_motion(
        &mut self,
        x: i32,
        y: i32,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> bool {
        // A press already landed on a slider track (see `handle_mouse_click`) — every motion
        // until release drags that thumb, rather than re-hit-testing the row list under a
        // pointer that may have wandered off it.
        if self.settings_ui.slider_drag {
            self.drag_slider(x, screen_w, screen_h, fonts);
            return true;
        }
        let focus = self.hover_focus_at(x, y, screen_w, screen_h, fonts);
        // Parity with the D-pad: a hover that moves modal focus to another row replays the
        // focus-pop zoom (and shows the new row's caption). Home drives its own `focus_anim`
        // instead, so it's excluded. An open dropdown is excluded too — hover there only
        // moves the option cursor, so popping the parent row (as the D-pad also declines to)
        // is wrong.
        if focus.row && self.settings_ui.dropdown.is_none() && !matches!(self.nav.screen, Screen::Home) {
            self.render.modal.focus_anim = Some(Instant::now());
        }
        let close_changed = self.hover_close_at(x, y, screen_w, screen_h, fonts);
        focus.any || close_changed
    }

    /// Button index under `(x, y)` for a two-button confirm modal with `subtitle`, or
    /// `None` off both buttons — every confirm modal's hover arm shares this, against the
    /// same `confirm_dialog_layout` geometry the modal is drawn with.
    pub(crate) fn confirm_button_at(
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
        subtitle: &str,
        x: i32,
        y: i32,
    ) -> Option<usize> {
        let (_, content) = ui::tiles::confirm_dialog_layout(screen_w, screen_h, fonts, subtitle);
        ui::tiles::confirm_button_at(content, x, y)
    }

    /// Rect of confirm button `index` for a two-button modal with `subtitle` — the shared
    /// geometry the focused-button tile and its hit-rect are positioned against.
    pub(crate) fn confirm_focus_button_rect(
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
        subtitle: &str,
        index: usize,
    ) -> Rect {
        let (_, content) = ui::tiles::confirm_dialog_layout(screen_w, screen_h, fonts, subtitle);
        ui::widgets::confirm_button_rect(content, index)
    }

    /// Moves the positional focus/selection onto whatever interactive element sits
    /// under the pointer, so the Magic Remote's pointer highlights elements on hover
    /// exactly where a click would land. Returns whether the selection actually
    /// moved. Hovering empty space (gaps, row padding, the area between rows) leaves
    /// the current selection put rather than clearing it, so a resting pointer never
    /// fights the D-pad.
    fn hover_focus_at(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> HoverChange {
        // An open dropdown overlays the row list — hover moves its option cursor and
        // nothing behind it. Shared by whichever screen owns the dropdown (Settings or
        // Diagnostics), and uses the same overlay geometry the renderer draws against.
        if let Some(i) = self.dropdown_option_at(x, y, screen_w, screen_h, fonts) {
            let dd = self
                .settings_ui
                .dropdown
                .as_mut()
                .expect("dropdown_option_at yields Some only when one is open");
            let changed = dd.focused != i;
            dd.focused = i;
            return HoverChange::row(changed);
        }
        // A dropdown open but not hovered still swallows hover — the row list behind
        // it must not take the selection.
        if self.settings_ui.dropdown.is_some() {
            return HoverChange::NONE;
        }
        match self.nav.screen {
            // The held card's submenu takes hover whole, like an open dropdown does — the
            // grid behind it must not steal focus out from under an open menu.
            Screen::Home if self.card_menu.is_some() => {
                let Some(row) = self.card_menu_row_at(x, y, screen_w, fonts) else {
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
                    view::sidebar::hit_test_menu_button(x, y, self.hosts.entries.len(), self.sidebar_len(), screen_h)
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
            screen @ (Screen::Settings(_) | Screen::Collections) => {
                // A held row follows the d-pad, not the pointer: hovering elsewhere must not
                // drag the cursor out from under it.
                if self.screens.collections.dragging.is_some() {
                    return HoverChange::NONE;
                }
                let Some(row) = self.scroll_list_row_at(x, y, screen_w, screen_h) else {
                    return HoverChange::NONE;
                };
                let button = self.scroll_list_row_button_at(x, y, screen_w, screen_h);
                let key = ScreenKey::of(screen);
                let row_changed = self.nav.cursor(key) != row;
                let button_changed = self.screens.row_button != button;
                self.nav.set_cursor(key, row);
                self.screens.row_button = button;
                HoverChange::split(row_changed, button_changed)
            }
            Screen::HostMenu => {
                let Some((i, button)) = self.list_modal_row_button_at(x, y, screen_w, screen_h, fonts) else {
                    return HoverChange::NONE;
                };
                let row_changed = self.nav.cursor(ScreenKey::HostMenu) != i;
                let button_changed = self.screens.row_button != button;
                self.nav.set_cursor(ScreenKey::HostMenu, i);
                self.screens.row_button = button;
                HoverChange::split(row_changed, button_changed)
            }
            // Identical row-list geometry; only which focus field they carry differs.
            Screen::HostPower
            | Screen::Diagnostics
            | Screen::Experimental
            | Screen::HdrCalibration
            | Screen::CursorSettings(_)
            | Screen::ControllerSettings(_) => {
                let Some((row, button)) = self.list_modal_row_button_at(x, y, screen_w, screen_h, fonts) else {
                    return HoverChange::NONE;
                };
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
                let card = view::pairing::card_rect(screen_w, screen_h, fonts);
                if view::pairing::request_button_rect(card, fonts).contains_point((x, y)) {
                    let changed = self.screens.pairing_focus != PairingFocus::RequestAccess;
                    self.screens.pairing_focus = PairingFocus::RequestAccess;
                    HoverChange::row(changed)
                } else {
                    HoverChange::NONE
                }
            }
            // Two-button confirm modals (Forget/SendLogs/Wake/finished SpeedTest — the same
            // modal type as the in-stream Disconnect dialog): hovering a button focuses it,
            // so the pointer can pick action-vs-Cancel, not just confirm whatever the D-pad
            // last focused. `confirm_subtitle` is `None` while a test is still measuring,
            // which reads as nothing to hover.
            Screen::ForgetHost
            | Screen::SendLogs
            | Screen::Wake
            | Screen::SpeedTest
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::ResetGameSettings => {
                let Some(subtitle) = self.confirm_subtitle() else {
                    return HoverChange::NONE;
                };
                let Some(i) = Self::confirm_button_at(screen_w, screen_h, fonts, &subtitle, x, y) else {
                    return HoverChange::NONE;
                };
                HoverChange::row(self.set_confirm_focused(i))
            }
            // No positional focus to move: single-card info/entry modals (AddHost,
            // EditHost, About) and Settings with a dropdown open.
            _ => HoverChange::NONE,
        }
    }

    /// The `(content viewport, pixel scroll offset)` an open dropdown anchors its
    /// option overlay to, matching what `draw_list` renders so hit-testing lands
    /// exactly where options are drawn. `None` for a screen with no dropdown.
    pub(crate) fn dropdown_geom(&self, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> Option<(Rect, i32)> {
        match self.nav.screen {
            // Anchored to the animated offset (`settings_content_scroll`) so an open
            // dropdown stays attached to its row while the list is still settling.
            Screen::Settings(_) => self.scroll_list_content_scroll(screen_w, screen_h),
            // No list modal scrolls, so 0.
            s if crate::app::screens::is_list_modal(s) => {
                Some((self.modal_list_geometry(screen_w, screen_h, fonts)?.1, 0))
            }
            _ => None,
        }
    }

    /// The open scrolling list's display-row index under the pointer, using the same animated
    /// `modal_scroll_px` the rows render with — a fixed-offset hit-test drifts a row off once
    /// the list has scrolled. `None` outside the viewport, in a row gap, or off the family.
    pub(crate) fn scroll_list_row_at(&self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> Option<usize> {
        let (content, scroll_px) = self.scroll_list_content_scroll(screen_w, screen_h)?;
        Self::row_at(content, self.scroll_list_row_count(), scroll_px, x, y)
    }

    /// The open scrolling list's content viewport and its current animated scroll offset —
    /// the shared geometry the hit test and `scroll_list_row_rect`'s lookup both index into,
    /// so a scrolled list can't put them at odds.
    fn scroll_list_content_scroll(&self, screen_w: u32, screen_h: u32) -> Option<(Rect, i32)> {
        let (_, content) = self.scroll_list_layout(self.nav.screen, screen_w, screen_h)?;
        let stride = ui::widgets::focus_row_stride() as i32;
        let total = self.scroll_list_row_count();
        Some((content, self.clamped_scroll_px(total, stride, content.height())))
    }

    /// Display row `row`'s on-screen rect, same animated scroll offset the hit test uses —
    /// the geometry the Bitrate drag anchors to.
    fn scroll_list_row_rect(&self, row: usize, screen_w: u32, screen_h: u32) -> Rect {
        self.scroll_list_content_scroll(screen_w, screen_h).map_or_else(
            || Rect::new(0, 0, 0, 0),
            |(content, px)| ui::widgets::focus_row_rect_at_px(content, row, px),
        )
    }

    /// The Bitrate row's rect and track — `settings_focused` is already that row (set by
    /// whatever press started the drag), so this is the one geometry lookup both the arming
    /// click and every later drag motion need.
    fn bitrate_row_and_track(&self, screen_w: u32, screen_h: u32) -> (Rect, Rect) {
        let row_rect = self.scroll_list_row_rect(self.nav.cursor(ScreenKey::Settings), screen_w, screen_h);
        // A marked row's track shifts left (see `row_layout`), so the drag reads the same
        // geometry the draw did rather than deriving its own.
        let marked = menu::override_is_set(&self.editing_override(), menu::SettingsRow::Bitrate);
        (row_rect, ui::widgets::row_layout(row_rect, marked).track)
    }

    /// The calibration row's rect and slider track, taken from the row itself so they are the
    /// rects the renderer drew and not a second guess at them. Its buttons come from
    /// `row_button_at`, like every other row's.
    fn hdr_row_and_track(&self, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> Option<(Rect, Rect)> {
        let rows = self.list_modal_rows()?;
        let row = rows.get(view::hdrcalibration::ROW_SLIDER)?;
        let rect = ui::widgets::focus_row_rect(
            self.modal_list_content(screen_w, screen_h, fonts),
            view::hdrcalibration::ROW_SLIDER,
        );
        Some((rect, ui::widgets::row_geom(rect, row).track))
    }

    /// Sets the Bitrate row from the pointer's current x against its track — shared by the
    /// initial press (which also has to decide whether the click landed on the track at
    /// all) and every drag motion after it.
    fn set_bitrate_from_x(&mut self, x: i32, track: Rect) {
        menu::set_bitrate_fraction(self.settings_target_mut(), track_fraction(x, track));
        self.capture_game_override(menu::SettingsRow::Bitrate);
    }

    /// Drags whichever slider the armed press landed on to `x` — the one place that knows which
    /// screen's slider that is, shared by the arming press and every motion after it.
    fn drag_slider(&mut self, x: i32, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) {
        match self.nav.screen {
            Screen::HdrCalibration => {
                if let Some((_, track)) = self.hdr_row_and_track(screen_w, screen_h, fonts) {
                    self.set_hdr_fraction(track_fraction(x, track));
                }
            }
            _ => {
                let (_, track) = self.bitrate_row_and_track(screen_w, screen_h);
                self.set_bitrate_from_x(x, track);
            }
        }
    }

    /// The dropdown option index under the pointer, if a dropdown is open and the
    /// pointer is over one of its options. Shares `dropdown_geom` +
    /// `ui::widgets::dropdown_option_rect` with the renderer so hover previews exactly what a
    /// click confirms.
    pub(crate) fn dropdown_option_at(
        &self,
        x: i32,
        y: i32,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<usize> {
        let dd = self.settings_ui.dropdown.as_ref()?;
        let (content, scroll_px) = self.dropdown_geom(screen_w, screen_h, fonts)?;
        let overlay = view::scrolllist::dropdown_overlay_rect_at_px(content, dd.row, scroll_px);
        let options_len = self.dropdown_len(dd.row);
        (0..options_len).find(|&i| ui::widgets::dropdown_option_rect(overlay, i).contains_point((x, y)))
    }

    /// `(row index, which of its trailing buttons)` under the pointer on the host menu.
    /// Hover and click both go through this, so hovering previews exactly what clicking will
    /// do — a click on a row's ⋯ opens that instead of the row's own action, the same split
    /// as a sidebar host row's button.
    fn list_modal_row_button_at(
        &self,
        x: i32,
        y: i32,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<(usize, Option<RowButton>)> {
        let row = self.modal_list_row_at(x, y, screen_w, screen_h, fonts)?;
        let (_, content) = self.modal_list_geometry(screen_w, screen_h, fonts)?;
        let button = self.row_button_at(row, ui::widgets::focus_row_rect(content, row), x, y);
        Some((row, button))
    }

    /// The same, on a scrolling list — measured at the animated scroll offset the rows are
    /// drawn at, so a button is clickable exactly where it looks.
    fn scroll_list_row_button_at(&self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> Option<RowButton> {
        let (content, scroll_px) = self.scroll_list_content_scroll(screen_w, screen_h)?;
        let row = self.scroll_list_row_at(x, y, screen_w, screen_h)?;
        self.row_button_at(row, ui::widgets::focus_row_rect_at_px(content, row, scroll_px), x, y)
    }

    /// The list-modal row index under the pointer. `None` on a screen with no row list, or
    /// when the pointer misses every row. Measured off `modal_list_geometry` — the viewport
    /// the painter draws into — so hover and click land on the rows that are on screen.
    pub(crate) fn modal_list_row_at(
        &self,
        x: i32,
        y: i32,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<usize> {
        let (_, content) = self.modal_list_geometry(screen_w, screen_h, fonts)?;
        // A non-scrolling list modal is its rows' exact height, so the count comes off the
        // viewport rather than a per-screen table — a second table is a second thing to keep
        // in step — and its scroll offset is always zero.
        let rows = (content.height() / ui::widgets::focus_row_stride()) as usize;
        Self::row_at(content, rows, 0, x, y)
    }

    /// Which of `rows` rows in a `content` viewport `(x, y)` is on at `scroll_px`, if any.
    /// One hit test for both list families: the plain modals pass a zero offset, the
    /// scrolling ones the animated one their rows are drawn at — a fixed-offset test on a
    /// scrolled list picks the wrong row.
    fn row_at(content: Rect, rows: usize, scroll_px: i32, x: i32, y: i32) -> Option<usize> {
        if !content.contains_point((x, y)) {
            return None;
        }
        (0..rows).find(|&r| {
            let rect = ui::widgets::focus_row_rect_at_px(content, r, scroll_px);
            // Clipped edge rows aren't hoverable: a focused row composites on its own
            // unclipped tile, so hovering one would pop it outside the card.
            rect.y() >= content.y() && rect.bottom() <= content.bottom() && rect.contains_point((x, y))
        })
    }

    fn hover_close_at(&mut self, x: i32, y: i32, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> bool {
        let Some(card) = self.modal_card_rect(screen_w, screen_h, fonts) else {
            // Home draws no close button, but `hover_close` is only ever set true by a
            // modal branch — without clearing it on the way back to Home it stayed stuck
            // `true` forever (nothing on Home reset it), and `handle_mouse_click`'s
            // `if self.render.hover_close { return self.back() }` then swallowed every Home
            // click. Not reported as a visible change: Home draws no close button.
            self.render.hover_close = false;
            return false;
        };
        self.set_hover_close(ui::widgets::modal_close_rect(card).contains_point((x, y)))
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
    pub fn handle_mouse_click(
        &mut self,
        x: i32,
        y: i32,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<ConnectTarget> {
        // Re-sync the close-button hover to the click's own position first — a
        // MouseButtonDown can carry a slightly different (x, y) than the last
        // MouseMotion (the physical button press can jostle the remote a little).
        self.handle_mouse_motion(x, y, screen_w, screen_h, fonts);
        if self.render.hover_close {
            // Same "what Back means here" as everywhere else — see `back`'s docs.
            return self.back(screen_w, screen_h, fonts);
        }
        // An open dropdown owns the click wherever it landed, and always takes it as a pick of
        // the highlighted option: the one under the pointer when it is over one (hover made it
        // the cursor just above), and otherwise the one the list is already on. Not
        // tap-outside-to-close — the pointer is often nowhere near what the user is looking at,
        // since the wheel scrolls the list without the cursor following and a hand holding the
        // remote drifts off the panel.
        if self.settings_ui.dropdown.is_some() {
            self.handle_menu_event(MenuEvent::Confirm, screen_w, screen_h, fonts);
            return None;
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
                if let Some(row) = self.card_menu_row_at(x, y, screen_w, fonts) {
                    if let Some(menu) = self.card_menu.as_mut() {
                        menu.focus(row);
                    }
                }
            }
            Screen::Home => {
                // The ⋯ button sits inside its row, so it has to be tested first or the
                // click just reads as a click on the host.
                if let Some(idx) =
                    view::sidebar::hit_test_menu_button(x, y, self.hosts.entries.len(), self.sidebar_len(), screen_h)
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
                if let Some(row) = self.scroll_list_row_at(x, y, screen_w, screen_h) {
                    self.nav.set_cursor(ScreenKey::Collections, row);
                    self.screens.row_button = self.scroll_list_row_button_at(x, y, screen_w, screen_h);
                } else {
                    // No row under the pointer, so no trailing button either: the press is on
                    // the focused row itself.
                    self.screens.row_button = None;
                }
            }
            Screen::Settings(_) => {
                let hit = self.scroll_list_row_at(x, y, screen_w, screen_h);
                if let Some(row) = hit {
                    self.nav.set_cursor(ScreenKey::Settings, row);
                }
                // A press on the Bitrate track sets the value under the cursor directly and
                // arms the drag (see `handle_mouse_motion`) instead of nudging one notch the
                // way `Confirm` below would — a slider is for landing on a value, not stepping
                // to it one click at a time.
                if menu::settings_logical_row(self.settings_scope(), self.nav.cursor(ScreenKey::Settings))
                    == Some(menu::SettingsRow::Bitrate)
                    && menu::row_lock(
                        menu::SettingsRow::Bitrate,
                        self.settings_target(),
                        self.detected_gamepad_type,
                    )
                    .is_none()
                    // A drag starts from the track itself, so it needs a real hit.
                    && hit.is_some()
                {
                    let (row_rect, track) = self.bitrate_row_and_track(screen_w, screen_h);
                    if on_track(x, y, track, row_rect) {
                        self.settings_ui.slider_drag = true;
                        self.drag_slider(x, screen_w, screen_h, fonts);
                        return None;
                    }
                }
            }
            Screen::Pairing => {
                // The Magic Remote pointer is the most reliable input on this TV, so the
                // "Request access" button is clickable directly: focus it and confirm.
                let card = view::pairing::card_rect(screen_w, screen_h, fonts);
                if !view::pairing::request_button_rect(card, fonts).contains_point((x, y)) {
                    return None;
                }
                self.screens.pairing_focus = PairingFocus::RequestAccess;
            }
            Screen::HostMenu => {
                let hit = self.list_modal_row_button_at(x, y, screen_w, screen_h, fonts);
                if let Some((row, _)) = hit {
                    self.nav.set_cursor(ScreenKey::HostMenu, row);
                }
                self.screens.row_button = hit.and_then(|(_, button)| button);
            }
            // Identical row-list geometry; only which focus field they carry differs.
            Screen::HostPower
            | Screen::Diagnostics
            | Screen::Experimental
            | Screen::CursorSettings(_)
            | Screen::ControllerSettings(_) => {
                let hit = self.list_modal_row_button_at(x, y, screen_w, screen_h, fonts);
                if let Some((row, _)) = hit {
                    *self.list_modal_focused_mut()? = row;
                }
                self.screens.row_button = hit.and_then(|(_, button)| button);
            }
            // The one row is a track and a button, so a press is one or the other. Only the
            // button falls through to `press` below, which is what advances the step.
            Screen::HdrCalibration => {
                let (_, button) = self.list_modal_row_button_at(x, y, screen_w, screen_h, fonts)?;
                // Focus follows the click, exactly as it does on a collection row's buttons.
                self.screens.row_button = button;
                if button.is_none() {
                    let (row_rect, track) = self.hdr_row_and_track(screen_w, screen_h, fonts)?;
                    if on_track(x, y, track, row_rect) {
                        self.settings_ui.slider_drag = true;
                        self.drag_slider(x, screen_w, screen_h, fonts);
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
            | Screen::ResetGameSettings => {}
            // Nothing clickable but the close button (handled above).
            Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::About => return None,
        }
        self.press(screen_w, screen_h, fonts)
    }
}

/// Whether a press at `(x, y)` counts as landing on `track`. The full row height counts, not
/// just the thin track: vertical precision on a slider isn't worth demanding of a Magic Remote
/// pointer.
fn on_track(x: i32, y: i32, track: Rect, row_rect: Rect) -> bool {
    x >= track.x() && x < track.right() && y >= row_rect.y() && y < row_rect.bottom()
}

/// Where `x` sits along `track`, as 0..1. Shared by the press that arms a drag and every
/// motion after it, so the thumb lands under the cursor exactly where the track was drawn.
fn track_fraction(x: i32, track: Rect) -> f32 {
    ((x - track.x()) as f32 / track.width().max(1) as f32).clamp(0.0, 1.0)
}
