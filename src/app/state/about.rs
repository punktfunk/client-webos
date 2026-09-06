//! About screen logic: the document's scroll position. Rendering is `app::draw::about`.
use crate::app::draw;
use crate::app::view;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;

impl App {
    /// Lazy-initialize about lines on first open.
    pub(crate) fn open_about(&mut self) {
        if self.render.about_lines.is_empty() {
            self.render.about_lines = view::about::lines();
        }
        self.screens.about_scroll = 0;
        self.nav.screen = Screen::About;
    }

    /// Navigate: Up/Down scroll by line, Left/Right by page.
    pub(crate) fn handle_about_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32) {
        match ev {
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {
                let (total, visible) = self.about_scroll_geometry(screen_w, screen_h);
                // Page step with anchor: show last few lines of previous page.
                let page = visible.saturating_sub(2).max(1) as i64;
                let step = match ev {
                    MenuEvent::Up => -1,
                    MenuEvent::Down => 1,
                    MenuEvent::Left => -page,
                    _ => page,
                };
                self.scroll_about_lines(step, total, visible);
            }
            // Return to Settings (not Home) to preserve settings context
            MenuEvent::Back | MenuEvent::Confirm => self.nav.resume(Screen::SettingsPage),
            MenuEvent::Secondary => {}
        }
    }

    /// Scroll by pixels (Magic Remote wheel).
    pub(crate) fn scroll_about_by(&mut self, dy_px: i32, screen_w: u32, screen_h: u32) -> bool {
        let (total, visible) = self.about_scroll_geometry(screen_w, screen_h);
        let l = draw::about::layout(screen_w as f32, screen_h as f32, draw::scale(screen_h));
        let lines = (dy_px as f32 / l.stride.max(1.0)) as i64;
        if lines == 0 {
            return false;
        }
        self.scroll_about_lines(lines, total, visible)
    }

    fn scroll_about_lines(&mut self, delta: i64, total: usize, visible: usize) -> bool {
        let max = total.saturating_sub(visible) as i64;
        let next = (self.screens.about_scroll as i64 + delta).clamp(0, max) as usize;
        let moved = next != self.screens.about_scroll;
        self.screens.about_scroll = next;
        moved
    }

    /// Total and visible line counts.
    pub(crate) fn about_scroll_geometry(&mut self, screen_w: u32, screen_h: u32) -> (usize, usize) {
        let k = draw::scale(screen_h);
        let l = draw::about::layout(screen_w as f32, screen_h as f32, k);
        self.ensure_about_wrapped(l.body.width(), k);
        let total = self.render.about_wrapped.as_ref().map_or(0, |(_, v)| v.len());
        (total, l.visible)
    }

    /// Defer text wrapping until width is known.
    pub(crate) fn ensure_about_wrapped(&mut self, width: f32, k: f32) {
        let width_key = width.round() as u32;
        let stale = !matches!(&self.render.about_wrapped, Some((w, _)) if *w == width_key);
        if stale {
            let wrapped = draw::about::wrap_document(&self.fonts, k, &self.render.about_lines, width);
            self.render.about_wrapped = Some((width_key, wrapped));
        }
    }
}
