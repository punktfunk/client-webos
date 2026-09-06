//! The grid family's bookkeeping: which covers are resident this frame, and whether the page
//! behind the spinner is ready to reveal.
//!
//! Every step here is windowed — an index range, never a walk of the library. The painting
//! itself is `app::draw::home`; what this pass leaves behind is the cover image per game in
//! the build window, the art requests that fill them, and the arrival clock of any card that
//! lands on a settled grid.
use std::ops::Range;
use std::time::Instant;

use crate::app::draw::home::cover_image;
use crate::app::grid::{GridLayout, CARD_KEEP_ROWS, CARD_PREFETCH_ROWS};
use crate::app::library::Library;
use crate::app::spinner::PageReady;
use crate::app::{view, App, HomeFocus, Screen};
use crate::ui;
use crate::ui::render::Size;

/// Nothing more can arrive for this card (cover in library.art or game has none).
fn art_ready(library: &Library, layout: GridLayout, idx: usize) -> bool {
    layout.card_at(&library.games, idx).is_none_or(|game| {
        library.art.contains_key(&game.id) || (game.art.portrait.is_none() && game.art.header.is_none())
    })
}

impl App {
    /// Whether the windowed pass can be skipped entirely this frame.
    fn grid_window_frozen(&self) -> bool {
        !matches!(self.nav.screen, Screen::Home)
            && self.render.grid.reveal.is_revealed()
            && !self.render.grid.dirty
            && self.render.grid.cards_dirty.is_empty()
    }

    /// Grid bookkeeping. Everything below is O(visible) via index ranges, not library scans.
    pub(super) fn prepare_grid(&mut self, screen: Size) {
        let available_w = screen.w.saturating_sub(ui::widgets::SIDEBAR_W);
        let columns = view::home::grid_columns(available_w);
        let (_, card_h) = view::home::grid_card_size(available_w, columns);
        if self.library.selected_host.is_none() {
            self.render.grid.reveal.reveal();
            return;
        }
        if self.grid_window_frozen() {
            return;
        }
        let count = self.grid_len(columns);
        self.release_stale_cards();

        let row_h = card_h as i32 + view::home::GRID_GAP;
        let visible_rows = (screen.h as i32 - view::home::GRID_TOP_Y).max(row_h) / row_h + 1;
        let first_visible_row = (self.render.grid.scroll / row_h).max(0);
        let rows = count.div_ceil(columns.max(1)) as i32;
        // Row band -> index range, clamped to the library. Ignores the section headings'
        // offsets: a row of slack either way is what the prefetch/keep rows absorb.
        let window = |lo: i32, hi: i32| {
            let lo = lo.clamp(0, rows) as usize * columns.max(1);
            let hi = (hi + 1).clamp(0, rows) as usize * columns.max(1);
            lo.min(count)..hi.min(count)
        };
        let build_window = window(
            first_visible_row - CARD_PREFETCH_ROWS,
            first_visible_row + visible_rows + CARD_PREFETCH_ROWS,
        );
        let page_window = window(first_visible_row, first_visible_row + visible_rows);
        let keep_window = window(
            first_visible_row - CARD_KEEP_ROWS,
            first_visible_row + visible_rows + CARD_KEEP_ROWS,
        );

        self.evict_cards_outside(keep_window, columns);
        self.build_card_window(build_window, columns);
        self.prefetch_focused_hero(columns);
        self.advance_grid_reveal(page_window, columns);
    }

    /// Drop stale covers: all of them on a library/host change, else just the ones whose art
    /// was refreshed.
    fn release_stale_cards(&mut self) {
        if self.render.grid.dirty {
            self.render.grid.arrivals.clear();
            self.render.covers.clear();
            self.render.grid.kept = 0..0;
            self.render.grid.card_pop_until = None;
            self.render.grid.dirty = false;
            self.render.grid.cards_dirty.clear();
            self.render.grid.reveal.restart();
        } else {
            for id in std::mem::take(&mut self.render.grid.cards_dirty) {
                self.render.covers.remove(&id);
            }
        }
    }

    /// Free covers and decoded art outside the keep window. Only when the window moved: the
    /// resident set is small but mapping each id back to its index walks the library.
    fn evict_cards_outside(&mut self, keep_window: Range<usize>, columns: usize) {
        if keep_window == self.render.grid.kept {
            return;
        }
        self.render.grid.kept = keep_window.clone();
        let layout = self.library.layout(columns);
        let dropped: Vec<String> = self
            .render
            .grid
            .arrivals
            .ids()
            .filter(|id| {
                !layout
                    .idx_for_pin_id(&self.library.games, id)
                    .is_some_and(|idx| keep_window.contains(&idx))
            })
            .map(str::to_string)
            .collect();
        for id in dropped {
            self.render.grid.arrivals.release(&id);
            self.render.covers.remove(&id);
            // Drop the decoded cover too (several × card size); the disk cache answers a
            // scroll back.
            self.library.art.remove(&id);
            if let Some(loader) = &mut self.jobs.art {
                loader.forget(&id);
            }
        }
    }

    /// Request art for the build window and turn what has landed into cover images.
    fn build_card_window(&mut self, build_window: Range<usize>, columns: usize) {
        let layout = self.library.layout(columns);
        let settled = self.render.grid.reveal.is_revealed();
        let now = Instant::now();
        for idx in build_window {
            let Some(game) = layout.card_at(&self.library.games, idx) else {
                continue;
            };
            if let Some(loader) = &mut self.jobs.art {
                loader.request(game);
            }
            // A card first seen on a settled grid arrives with a pop; an art refresh swaps in
            // place; the grid's first fill is the reveal wave's job.
            if self.render.grid.arrivals.note(&game.id) && settled {
                self.render.grid.arm_card_pop(&game.id, now);
            }
            if !self.render.covers.contains_key(&game.id) {
                if let Some(img) = self.library.art.get(&game.id).and_then(cover_image) {
                    self.render.covers.insert(game.id.clone(), img);
                }
            }
        }
    }

    /// The focused card's hero, fetched ahead so it is in hand on OK.
    fn prefetch_focused_hero(&mut self, columns: usize) {
        if !self.render.grid.reveal.is_revealed() {
            return;
        }
        let HomeFocus::Grid(focus_idx) = self.home_focus else {
            return;
        };
        if let Some(game) = self.library.layout(columns).card_at(&self.library.games, focus_idx) {
            if let Some(loader) = &mut self.jobs.art {
                loader.request_hero(game);
            }
            self.render.hero.want(&game.id);
        }
    }

    /// Reveals the page in one wave once its art is in (or the spinner's cap has passed).
    fn advance_grid_reveal(&mut self, page_window: Range<usize>, columns: usize) {
        if self.render.grid.reveal.is_revealed() {
            return;
        }
        let layout = self.library.layout(columns);
        let page_ready = || {
            let pending = page_window.clone().any(|idx| !art_ready(&self.library, layout, idx));
            if pending {
                PageReady::Art
            } else {
                PageReady::All
            }
        };
        let fetching = self.library_fetch_in_flight() || self.wake_wait_in_flight();
        self.render.grid.reveal.advance(fetching, page_ready);
    }
}
