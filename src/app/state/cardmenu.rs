//! The submenu a held grid card raises over its own title strip: where the card lives, and
//! its per-game settings.
//!
//! It is not a `Screen` — it lives on Home, over one card, and Home's own navigation is
//! simply suspended while it is up (see `App::handle_home_event`). That keeps the card, its
//! focus ring and its frosted title strip on screen underneath, which is the whole point:
//! the menu belongs to *that* card, not to a modal that happens to know its id.
use std::time::Instant;

use crate::app::menu::nav_dir;
use crate::app::{view, App};
use crate::core::event::MenuEvent;
use crate::core::screen::HomeFocus;
use crate::ui;

/// What one submenu row does. Two or three, depending on whether the card is in a collection
/// at all — [`App::card_menu_row_kinds`] is the one table, so the labels, the panel's baked
/// height, its tile key, its hit test and this handler cannot disagree about the row count.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum CardMenuRow {
    /// Opens `Screen::Collections` over this card.
    MoveTo,
    /// Back to Library, with no dialog. Only on a card a collection holds.
    Remove,
    /// The shell's bind list: which catalog profile this title streams with.
    Profile,
    /// Creates-or-opens the bound profile in profile scope.
    Settings,
}

/// The line shown once, on the release that added this menu, when the first library lands: the
/// feature is behind a hold, which nothing on screen would otherwise reveal. Phrased as what it
/// buys the user, not as a keypress.
pub(crate) const INTRO_HINT: &str =
    "New: hold OK on a card to give that game its own settings — some run smoother with a codec, \
     bitrate or resolution of their own.";

/// The open card submenu. `Some` exactly while it is up.
pub struct CardMenu {
    /// The card it belongs to (a `GameEntry::id`, or `store::DESKTOP_PIN_ID`).
    pub pin_id: String,
    /// That card's grid index, as it was when the menu opened. Latched rather than looked up
    /// per frame: `idx_for_pin_id` scans the whole library, and the pointer path would pay
    /// that on every `MouseMotion`. Safe to latch because every action below closes the menu
    /// before it can reorder the grid.
    pub idx: usize,
    /// That card's title — reused as the per-game settings screen's dim title suffix.
    pub title: String,
    pub focused: usize,
    /// When focus last moved — the band's zoom-in clock, read through
    /// `ui::animation::focus_tile_rect` exactly as a modal's focused row is
    /// (`ModalState::focus_anim`). `None` once the pop has played out.
    pub focus_anim: Option<Instant>,
    /// When it opened, for the panel's rise. Its own clock, not `focus_anim`: that one is
    /// re-armed by every focus move, and the panel must not restart with the card's zoom.
    pub since: Instant,
    /// The rise has had its final frame drawn (see [`CardMenu::tick`]).
    risen: bool,
    /// The card has been moved inside its collection and the new order is not written yet
    /// (see [`App::swap_card_in_collection`]). Drives the rest of the collection's dim, the
    /// collapse of the panel to a bare title strip, and the commit on the way out.
    pub moved: bool,
}

impl CardMenu {
    /// Moves focus and arms the band's pop. No-op when `row` is already focused, so a
    /// pointer resting on a row can't restart an animation that shows nothing.
    pub(crate) fn focus(&mut self, row: usize) {
        if row != self.focused {
            self.focus_anim = Some(Instant::now());
            self.focused = row;
        }
    }

    /// Plays the panel's rise again from now — what leaving reorder mode does, since the
    /// panel was collapsed to a title strip for the duration and has to come back the way it
    /// arrived rather than snapping to full height.
    pub(crate) fn rise_again(&mut self) {
        self.since = Instant::now();
        self.risen = false;
    }

    /// Whether the panel still owes frames — the rise, and the band's focus pop.
    ///
    /// Both report `true` on the tick their clock *expires*, not just while it runs. That
    /// last tick is the one that draws the animation at its final value: report `false`
    /// there and the render loop parks in `wait_for_event` one frame short, leaving the
    /// panel a percent below its resting place until some unrelated event sets `dirty` —
    /// which, during a hold, is the button coming back up. It reads as the panel stalling
    /// just before the end and then finishing on release. `focus_anim` and friends avoid it
    /// by clearing their `Option` on the expiring tick and still reporting that tick.
    pub fn tick(&mut self) -> bool {
        let mut owed = false;
        if !self.risen {
            self.risen = self.since.elapsed() >= ui::animation::CARD_MENU_RISE;
            owed = true;
        }
        match self.focus_anim {
            Some(t) if t.elapsed() < ui::animation::FOCUS_POP => owed = true,
            Some(_) => {
                self.focus_anim = None;
                owed = true;
            }
            None => {}
        }
        owed
    }
}

impl App {
    /// Raises the submenu over the focused card. The long-hold's whole effect — see
    /// `runtime::ui_flow`.
    pub(crate) fn open_card_menu(&mut self, screen_w: u32) {
        let columns = view::home::grid_columns_for_screen(screen_w);
        let HomeFocus::Grid(idx) = self.home_focus else {
            return;
        };
        let Some(pin_id) = self.pin_id_at_grid_idx(idx, columns).map(str::to_string) else {
            return;
        };
        let title = self.grid_card_entry(idx, columns).title.clone();
        self.card_menu = Some(CardMenu {
            pin_id,
            idx,
            title,
            focused: 0,
            // Armed at open: the band pops in with the panel's rise, the same motion a row
            // change plays later.
            focus_anim: Some(Instant::now()),
            since: Instant::now(),
            risen: false,
            moved: false,
        });
    }

    /// Drops the submenu, fixing any in-collection reorder it was holding on the way out —
    /// every way of leaving commits, none discards. The one place `card_menu` is cleared.
    pub(crate) fn close_card_menu(&mut self) {
        self.commit_card_reorder();
        self.card_menu = None;
    }

    /// Moves the held card one slot inside its own collection: a swap, so every other card
    /// keeps its slot and the grid needs no new geometry (see `docs/COLLECTIONS-PLAN.md`).
    /// `false` when there is nowhere to go — either end of the block, or Library, whose
    /// order is recency.
    fn swap_card_in_collection(&mut self, forward: bool, screen_w: u32, screen_h: u32) -> bool {
        let Some(pin_id) = self.card_menu.as_ref().map(|m| m.pin_id.clone()) else {
            return false;
        };
        // Read before the swap: it names the card the grid has to bring along with the
        // collection, which is what keeps the two orders in step without a regroup.
        let Some(other) = self
            .selected_known_host()
            .and_then(|h| h.collection_neighbour(&pin_id, forward))
            .map(str::to_string)
        else {
            return false;
        };
        let Some(host) = self.selected_known_host_mut() else {
            return false;
        };
        if !host.swap_within_collection(&pin_id, forward) {
            return false;
        }
        self.library.swap_games(&pin_id, &other);
        // Nothing is written per press; the order is fixed when the menu closes.
        let columns = view::home::grid_columns_for_screen(screen_w);
        if let Some(idx) = self.grid_idx_for_pin_id(&pin_id, columns) {
            // The panel, its band and its hit test all hang off `idx`, so the menu has to
            // travel with the card it belongs to.
            if let Some(menu) = self.card_menu.as_mut() {
                menu.idx = idx;
            }
            self.set_home_focus(HomeFocus::Grid(idx));
            // A swap onto another row would otherwise leave the card half off screen.
            self.ensure_grid_visible(idx, columns, screen_w, screen_h);
        }
        if let Some(menu) = self.card_menu.as_mut() {
            menu.moved = true;
        }
        true
    }

    /// Fixes the order a swap left unwritten: one save, and the same "everything pops back
    /// in" gesture a move between collections plays, scoped to the collection that changed.
    /// Costs nothing when nothing moved.
    fn commit_card_reorder(&mut self) {
        if self.card_menu.as_ref().is_some_and(|m| m.moved) {
            self.persist();
        }
    }

    /// Whether the held card is mid-reorder: the rest of its collection is dimmed, the panel
    /// is collapsed to its title strip, and the next Confirm fixes the card where it sits.
    pub(crate) fn card_menu_reordering(&self) -> bool {
        self.card_menu.as_ref().is_some_and(|m| m.moved)
    }

    /// Fixes the held card where it now sits: writes the order and leaves reorder mode with
    /// the menu still up, so the panel rises again over the card. `false` when no reorder was
    /// under way, which is what lets Confirm go on to mean the focused row.
    pub(crate) fn fix_card_position(&mut self) -> bool {
        if !self.card_menu_reordering() {
            return false;
        }
        self.commit_card_reorder();
        if let Some(menu) = self.card_menu.as_mut() {
            menu.moved = false;
            menu.rise_again();
        }
        true
    }

    /// Handles one menu event while the submenu is up. Returns `false` when it isn't, so
    /// Home's own handler runs instead.
    pub(crate) fn handle_card_menu_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32) -> bool {
        let Some(menu) = self.card_menu.as_ref() else {
            return false;
        };
        let (pin_id, title) = (menu.pin_id.clone(), menu.title.clone());
        let rows = self.card_menu_row_kinds(&pin_id);
        let Some(menu) = self.card_menu.as_mut() else {
            return false;
        };
        // Wraps, like every other row list in the app (`list_nav`); `focus` is what arms the
        // band's slide, so the move goes through it rather than writing `focused` directly.
        let mut next = menu.focused;
        if ui::widgets::list_nav(&mut next, rows.len(), nav_dir(ev)) {
            menu.focus(next);
            return true;
        }
        // The first Confirm after a move means "there", not "open what this row does" — the
        // panel is collapsed while reordering, so there is no row on screen to have meant.
        if ev == MenuEvent::Confirm && self.fix_card_position() {
            return true;
        }
        let Some(menu) = self.card_menu.as_ref() else {
            return true;
        };
        match (ev, rows.get(menu.focused)) {
            // Both leave the panel up behind what they raise: each is a step *into* this
            // menu, and collapsing it underneath makes going back read as having landed
            // somewhere else. The screen they open owns every event while it is up (this
            // handler runs on Home only), and leaving it closes the menu.
            (MenuEvent::Confirm, Some(CardMenuRow::MoveTo)) => self.open_collections(&pin_id, &title),
            (MenuEvent::Confirm, Some(CardMenuRow::Settings)) => self.open_game_profile(&pin_id, &title),
            (MenuEvent::Confirm, Some(CardMenuRow::Profile)) => {
                self.open_pick_profile(crate::app::state::profilepick::ProfilePick::BindGame { pin_id, title });
            }
            // No dialog: the card is one press from wherever it was, and the section heading
            // it lands under says where it went.
            (MenuEvent::Confirm, Some(CardMenuRow::Remove)) => {
                self.close_card_menu();
                self.move_focused_card(None, screen_w, screen_h);
            }
            // Left/Right move the card itself inside its collection while the menu is up.
            // Where it cannot (Library, or either end of the block) the press dip stands in
            // for a nudge, so "it stopped" and "it ignored me" stay tellable apart — and the
            // menu stays up either way, since the gesture is the reorder, not a dismissal.
            (MenuEvent::Left | MenuEvent::Right, _) => {
                if !self.swap_card_in_collection(ev == MenuEvent::Right, screen_w, screen_h) {
                    self.render.press.arm();
                }
            }
            // Secondary would otherwise forget a host from under an open menu.
            _ => self.close_card_menu(),
        }
        true
    }
}
