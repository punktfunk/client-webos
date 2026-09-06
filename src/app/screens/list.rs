//! The plain list screens: Host menu, Host power, HDR calibration.
//!
//! Each is a card holding one `FocusRow` per line, focused by row index. Their handlers all
//! opened the same way — count the rows, move the cursor, arm the focus pop, return — and
//! their tiles were built by a `match self.screen` picking which `rows()` to call. Both of
//! those are here now, so a screen's own module holds only what it actually does differently:
//! which rows it lists, and what a press on one means.
use crate::app::nav::ScreenKey;
use crate::app::view;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use std::time::Instant;

impl App {
    /// How many rows the open list modal has — the count without the labels, for the paths
    /// that only navigate (see `app::view::hostmenu::Metrics` for why that matters).
    pub(crate) fn list_modal_row_count(&self) -> usize {
        match self.nav.screen {
            Screen::HostMenu => self.host_menu_actions().len(),
            Screen::HostPower => view::hostpower::ROW_COUNT,
            Screen::PickProfile => self.pick_profile_rows().len(),
            Screen::HdrCalibration => view::hdrcalibration::ROW_COUNT,
            // Exhaustive for the same reason `list_modal_rows` is: this is the second half of
            // the family's table — the labels there, the count here — and a screen listed by
            // one but missed by the other navigates a list it cannot draw.
            Screen::Home
            | Screen::Pairing
            | Screen::AddHost
            | Screen::Wake
            | Screen::ForgetHost
            | Screen::EditHost
            | Screen::About
            | Screen::SpeedTest
            | Screen::SendLogs
            // A scrolling list, counted by `scroll_list_row_count`; its name dialog is a
            // text form with no rows at all.
            | Screen::Collections
            | Screen::RenameCollection
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::SettingsPage
            | Screen::RenameProfile
            | Screen::DeleteProfile => 0,
        }
    }

    /// How many rows the open row list has, whichever of the two families it is in. The one
    /// count every nav path asks for: the two tables each return 0 off their own family, so a
    /// screen counted against the wrong one navigates a list it cannot draw (and freezes).
    pub(crate) fn row_count(&self) -> usize {
        if self.nav.screen == Screen::SettingsPage {
            self.settings_page_rows().len()
        } else if self.nav.screen == Screen::Collections {
            self.collections_row_count()
        } else {
            self.list_modal_row_count()
        }
    }

    /// Whether what's on screen navigates by row — a list screen, or a row list hanging over
    /// one that doesn't (an open dropdown, a held card's submenu). Asked before choosing
    /// between stepping focus and scrolling pixels, so a caller lands wherever an Up/Down
    /// press would.
    pub(crate) fn navigates_rows(&self) -> bool {
        self.card_menu.is_some() || self.row_count() > 0
    }

    /// Where row focus is now, across the two places it can live. Only ever compared with
    /// itself: a caller that navigates without an animation to redraw off (the wheel) samples
    /// it either side of the step to tell a move from a press against the end of the list.
    pub(crate) fn row_focus(&self) -> (usize, Option<usize>) {
        (
            self.nav.cursor(ScreenKey::of(self.nav.screen)),
            self.card_menu.as_ref().map(|m| m.focused),
        )
    }

    /// The nav half of a list screen's event handling: moves the cursor and arms the focus
    /// pop, reporting whether the event was spent doing so. Every list handler starts here,
    /// and none of them counts its own rows any more.
    pub(crate) fn list_nav_event(&mut self, ev: MenuEvent) -> bool {
        let len = self.row_count();
        let key = ScreenKey::of(self.nav.screen);
        if crate::ui::widgets::list_nav(self.nav.cursor_mut(key), len, crate::app::menu::nav_dir(ev)) {
            // A trailing button belongs to the row it is on, so leaving that row leaves it.
            self.screens.row_button = None;
            self.render.modal.focus_anim = Some(Instant::now());
            return true;
        }
        false
    }

    /// Starts the focused row's switch slide from the value it is leaving, so the knob slides
    /// rather than snapping — the shared tail of every toggle row on every list screen.
    pub(crate) fn arm_switch_anim(&mut self, from: bool) {
        let row = self.nav.cursor(ScreenKey::of(self.nav.screen));
        self.render.modal.switch_anim = Some((Instant::now(), from, row));
    }
}
