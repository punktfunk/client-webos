//! The settings page on the kit: one large glass card, a page column on the left with the
//! desktop's six marks, and the open page's rows in a `MenuList` on the right.
//!
//! [`layout`] is what the pointer measures; the rows are hit through the list's own rects.

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, W};
use pf_console_ui::widgets::{MenuList, RowSpec};
use skia_safe::{Contains, Point, Rect};

use super::{focus_face, glass_card, with_pop, FocusEase, Frame};
use crate::app::state::settingspage::Page;

/// The card as a share of the screen, both axes.
const FRAC: f32 = 0.86;
/// Design units.
const PAD: f32 = 24.0;
const CORNER: f32 = 18.0;
const TITLE_SIZE: f64 = 22.0;
const COLUMN_W: f32 = 200.0;
const ENTRY_H: f32 = 46.0;
const ENTRY_GAP: f32 = 4.0;
const ENTRY_CORNER: f32 = 12.0;
const ENTRY_SIZE: f64 = 15.0;
const CLOSE_BOX: f32 = 30.0;
const ICON_BOX: f32 = 18.0;

pub(crate) struct Layout {
    pub card: Rect,
    pub close: Rect,
    /// One rect per page entry, in `Page::ALL` order.
    pub entries: Vec<Rect>,
    /// Where the `MenuList` draws.
    pub rows: Rect,
    pub title_baseline: f32,
}

impl Layout {
    pub fn on_close(&self, x: i32, y: i32) -> bool {
        self.close.contains(Point::new(x as f32, y as f32))
    }

    pub fn entry_at(&self, x: i32, y: i32) -> Option<usize> {
        let p = Point::new(x as f32, y as f32);
        self.entries.iter().position(|r| r.contains(p))
    }
}

pub(crate) fn layout(fw: f32, fh: f32, k: f32) -> Layout {
    let w = (fw * FRAC).round();
    let h = (fh * FRAC).round();
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), ((fh - h) / 2.0).round(), w, h);
    let title_h = super::line_h(TITLE_SIZE * f64::from(k)) as f32;
    let title_baseline = card.top + PAD * k + title_h * 0.8;
    let body_top = card.top + PAD * k + title_h + PAD * 0.6 * k;
    let col_x = card.left + PAD * k;
    let entries = Page::ALL
        .iter()
        .enumerate()
        .map(|(i, _)| {
            Rect::from_xywh(
                col_x,
                body_top + i as f32 * (ENTRY_H + ENTRY_GAP) * k,
                COLUMN_W * k,
                ENTRY_H * k,
            )
        })
        .collect();
    let rows_left = col_x + (COLUMN_W + PAD) * k;
    let rows = Rect::from_xywh(
        rows_left,
        body_top,
        card.right - PAD * 0.5 * k - rows_left,
        card.bottom - PAD * k - body_top,
    );
    let close = Rect::from_xywh(
        card.right - (PAD * 0.6 + CLOSE_BOX) * k,
        card.top + PAD * 0.6 * k,
        CLOSE_BOX * k,
        CLOSE_BOX * k,
    );
    Layout {
        card,
        close,
        entries,
        rows,
        title_baseline,
    }
}

/// Draw the page. `column` is whether focus is on the page column; `page` is the open one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    f: &Frame<'_>,
    list: &mut MenuList,
    tabs: &mut FocusEase,
    l: &Layout,
    page: Page,
    column: bool,
    rows: &[RowSpec],
    hover_close: bool,
    alpha: f32,
    dy: f32,
    dt: f64,
    active: bool,
) {
    let c = f.canvas;
    let k = f.k;
    c.save();
    c.translate((0.0, dy));
    c.save_layer_alpha_f(Some(l.card), alpha);
    glass_card(c, l.card, CORNER, k);
    f.fonts.draw(
        c,
        "Settings",
        f64::from(l.card.left + PAD * k),
        f64::from(l.title_baseline),
        W::SemiBold,
        TITLE_SIZE * f64::from(k),
        theme::fg(1.0),
    );
    if let Some(x) = by_name("x") {
        draw_icon(
            c,
            x,
            l.close.center_x(),
            l.close.center_y(),
            ICON_BOX * k,
            theme::fg(if hover_close { 1.0 } else { 0.5 }),
        );
    }
    tabs.step(Page::ALL.len(), (column && active).then(|| page.index()), dt);
    for (i, (p, r)) in Page::ALL.iter().zip(&l.entries).enumerate() {
        let open = i == page.index();
        let fo = tabs.at(i);
        let color = if open { theme::fg(1.0) } else { theme::fg(0.6) };
        with_pop(c, *r, fo, |c| {
            focus_face(c, *r, ENTRY_CORNER, fo, open, k);
            if let Some(icon) = by_name(p.icon()) {
                draw_icon(c, icon, r.left + 24.0 * k, r.center_y(), ICON_BOX * k, color);
            }
            f.fonts.draw(
                c,
                p.label(),
                f64::from(r.left + 44.0 * k),
                f64::from(r.center_y() + ENTRY_SIZE as f32 * k * 0.35),
                if open { W::SemiBold } else { W::Medium },
                ENTRY_SIZE * f64::from(k),
                color,
            );
        });
    }
    list.render(c, l.rows, rows, f.fonts, f64::from(k), dt, active && !column);
    c.restore();
    c.restore();
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_console_ui::theme::Ink;
    use skia_safe::Color4f;

    /// The Display page over a flat ground, with the rows the console's engine builds for a
    /// default document; `PF_WEBOS_DUMP` writes the PNG.
    #[test]
    fn display_page_renders_its_column_and_rows() {
        theme::set_ink(Ink::of(pf_console_ui::library::palette("violet")));
        let fonts = theme::build_fonts().unwrap();
        let (w, h) = (1920u32, 1080u32);
        let k = super::super::scale(h);
        let rows = vec![
            RowSpec::choice("Resolution", "Native").with_header("Resolution"),
            RowSpec::choice("Refresh rate", "60 Hz"),
            RowSpec::slider("Bitrate", "Automatic", 0.0).with_header("Quality"),
            RowSpec::choice("Video codec", "Automatic"),
            RowSpec::toggle("HDR", true).with_note("10-bit, BT.2020 PQ"),
            RowSpec::action("Calibrate HDR…", true),
            RowSpec::toggle("Game mode", false)
                .with_header("TV")
                .locked("Needs a rooted TV"),
        ];
        let l = layout(w as f32, h as f32, k);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
        let mut list = MenuList::new();
        let mut tabs = FocusEase::default();
        // Rows focused first, then the page column: both faces settle within the frames.
        for (column, name) in [(false, "settings-display"), (true, "settings-column")] {
            for _ in 0..90 {
                surface.canvas().clear(Color4f::new(0.075, 0.063, 0.16, 1.0));
                let f = Frame::new(surface.canvas(), &fonts, w, h);
                draw(
                    &f,
                    &mut list,
                    &mut tabs,
                    &l,
                    Page::Display,
                    column,
                    &rows,
                    false,
                    1.0,
                    0.0,
                    1.0 / 60.0,
                    true,
                );
            }
            assert!(!tabs.animating());
            if let Ok(dir) = std::env::var("PF_WEBOS_DUMP") {
                let png = surface
                    .image_snapshot()
                    .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                    .unwrap();
                std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
            }
        }
    }

    #[test]
    fn the_column_and_the_rows_share_the_card_without_overlap() {
        let l = layout(1920.0, 1080.0, super::super::scale(1080));
        assert_eq!(l.entries.len(), Page::ALL.len());
        for e in &l.entries {
            assert!(l.card.contains(*e));
            assert!(e.right <= l.rows.left);
        }
        assert!(l.card.contains(l.rows));
        assert_eq!(
            l.entry_at(l.entries[2].center_x() as i32, l.entries[2].center_y() as i32),
            Some(2)
        );
        assert!(l.on_close(l.close.center_x() as i32, l.close.center_y() as i32));
    }
}
