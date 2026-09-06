//! The held card's submenu — its rows and its screen-space geometry, which the pointer path
//! hits against. Logic lives in `app::state::cardmenu`; painting in `app::draw::home`.
use crate::app::state::cardmenu::CardMenuRow;
use crate::app::{draw, view, App};
use crate::ui;
use crate::ui::render::Rect;

impl App {
    /// Which rows `pin_id`'s card shows. The one table over the submenu's shape: its labels,
    /// its height, its hit test and its handler all count rows through here.
    pub(crate) fn card_menu_row_kinds(&self, pin_id: &str) -> &'static [CardMenuRow] {
        // Nothing to remove a Library card from — Library *is* "in no collection".
        // The bind list only when there is a catalog to bind from.
        match (self.card_is_held(pin_id), self.profiles.is_empty()) {
            (true, false) => &[
                CardMenuRow::MoveTo,
                CardMenuRow::Remove,
                CardMenuRow::Profile,
                CardMenuRow::Settings,
            ],
            (true, true) => &[CardMenuRow::MoveTo, CardMenuRow::Remove, CardMenuRow::Settings],
            (false, false) => &[CardMenuRow::MoveTo, CardMenuRow::Profile, CardMenuRow::Settings],
            (false, true) => &[CardMenuRow::MoveTo, CardMenuRow::Settings],
        }
    }

    /// How many rows the submenu on `pin_id`'s card has — what its geometry divides by.
    pub(crate) fn card_menu_row_count(&self, pin_id: &str) -> usize {
        self.card_menu_row_kinds(pin_id).len()
    }

    /// The rows block of the open submenu on screen: under the title strip, scaled with the
    /// card's focus pop. `None` mid-reorder, when the panel is collapsed to the strip, and
    /// once a library reload has moved the card out from under the latched index.
    pub(crate) fn card_menu_rows_rect(&self, screen_w: u32, screen_h: u32) -> Option<Rect> {
        let menu = self.card_menu.as_ref().filter(|m| !m.moved)?;
        let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
        let columns = view::home::grid_columns(available_w);
        if self.pin_id_at_grid_idx(menu.idx, columns) != Some(menu.pin_id.as_str()) {
            return None;
        }
        let card = self.scrolled_card_rect(menu.idx, columns, ui::widgets::SIDEBAR_W as i32, available_w);
        let rows = self.card_menu_row_count(&menu.pin_id);
        let title_h = draw::home::strip_h(screen_h as f32, card.height() as f32);
        let panel_h = (title_h + draw::home::menu_rows_h(rows)).min(card.height() as f32);
        let top = card.bottom() - panel_h as i32 + title_h as i32;
        let band = Rect::new(card.x(), top, card.width(), (panel_h - title_h).max(0.0) as u32);
        Some(ui::animation::scale_about(
            band,
            card,
            self.focused_card_scale(&menu.pin_id),
        ))
    }

    /// The transform the focused card and everything on it is drawn with: the focus zoom and
    /// the appear pop, both about the card's own centre.
    pub(crate) fn focused_card_scale(&self, pin_id: &str) -> f32 {
        let f = ui::animation::anim_frac_smooth(self.render.focus_anim, ui::animation::CARD_FOCUS_POP);
        ui::animation::zoom_scale(f, crate::app::CARD_GROWTH)
            * ui::animation::pop_in_scale(self.card_pop_frac(pin_id), crate::app::CARD_POP_SHRINK)
    }

    /// The submenu row under `(x, y)`, if the pointer is on the rows block at all. Into the
    /// panel's own coordinates first: the band carries the card's scale.
    pub(crate) fn card_menu_row_at(&self, x: i32, y: i32, screen_w: u32, screen_h: u32) -> Option<usize> {
        let band = self.card_menu_rows_rect(screen_w, screen_h)?;
        if !band.contains_point((x, y)) {
            return None;
        }
        let count = self.card_menu_row_count(&self.card_menu.as_ref()?.pin_id);
        let local = (y - band.y()) as f32 * draw::home::menu_rows_h(count) / band.height().max(1) as f32;
        let row = ((local - draw::home::MENU_ROWS_PAD).max(0.0) / draw::home::MENU_ROW_H) as usize;
        Some(row.min(count.saturating_sub(1)))
    }
}
