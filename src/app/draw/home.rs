//! The Home screen on the kit: the host sidebar, the game grid and its covers, the status
//! band, and the launch transition with its hero backdrop. Geometry stays in
//! `view::{home, sidebar}` — the pointer path and the focus map hit against it — and this
//! file only paints what those rects say is there.

use std::collections::HashMap;
use std::time::Instant;

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, W};
use pf_console_ui::{brand, launcher_icons, os_marks};
use skia_safe::{
    images, BlendMode, BlurStyle, ClipOp, Color4f, Data, FilterMode, Image, ImageInfo, MaskFilter, MipmapMode, Paint,
    RRect, Rect, SamplingOptions,
};

use super::{focus_face, line_h, panel, sk, with_pop, Frame};
use crate::app::grid::{Entrance, GridLayout};
use crate::app::hosts::HostEntry;
use crate::app::state::cardmenu::CardMenuRow;
use crate::app::{hero, view, App, HomeFocus, Screen, CARD_GROWTH, LAUNCH_GROWTH, STATUS_BG_PAD};
use crate::core::model::GameEntry;
use crate::ui;
use crate::ui::animation::{anim_frac, anim_frac_smooth, pop_in_rect, zoom_rect, CARD_FOCUS_POP, CARD_MENU_RISE};
use crate::ui::widgets::{SIDEBAR_PAD, SIDEBAR_W};

/// The old SDL font sizes, px at 1080p; scaled by the frame height like they were.
const LABEL: f32 = 22.0;
const VALUE: f32 = 20.0;
const TITLE: f32 = 40.0;
const CAPTION: f32 = 14.0;
pub(crate) const CARD_RADIUS: f32 = 10.0;
/// The app mark top left, on its own — the host rows say what the column is. Its left edge
/// lines up with the row icons, not the panel padding, or it reads as hanging off the edge.
/// `MARK_SIDE` is the discs' box, not an icon tile; [`view::sidebar::TOP_Y`] follows it.
const MARK_SIDE: f32 = 64.0;
const SIDEBAR_ICON: f32 = 30.0;
const SIDEBAR_ICON_PAD: f32 = 20.0;
const MENU_GLYPH: f32 = 26.0;
const PRESENCE_DOT: f32 = 9.0;
const STRIP_PAD: f32 = 16.0;
const STRIP_INSET: f32 = 8.0;
pub(crate) const MENU_ROW_H: f32 = 54.0;
pub(crate) const MENU_ROWS_PAD: f32 = 10.0;
const MENU_BAND_INSET: f32 = 10.0;
const MENU_ICON_INSET: f32 = 14.0;
const MARK_DOT_R: f32 = 4.0;
const MARK_DOT_INSET: f32 = 16.0;
const GLOW_BLUR: f32 = 18.0;
const SPINNER_R: f64 = 24.0;

fn px(f: &Frame<'_>, size: f32) -> f64 {
    f64::from(size * f.h / 1080.0)
}

fn linear() -> SamplingOptions {
    SamplingOptions::new(FilterMode::Linear, MipmapMode::None)
}

fn fade(c: Color4f, alpha: f32) -> Color4f {
    Color4f::new(c.r, c.g, c.b, c.a * alpha)
}

fn alpha_paint(alpha: f32) -> Paint {
    let mut p = Paint::default();
    p.set_alpha_f(alpha);
    p
}

fn rr(r: Rect) -> RRect {
    RRect::new_rect_xy(r, CARD_RADIUS, CARD_RADIUS)
}

/// A raw upload's pixel layout: the hero's decoded art, the dissolve masks, the app icon.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RawFormat {
    Rgb565,
    /// Straight alpha, R first in memory.
    Rgba8888,
}

impl RawFormat {
    fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb565 => 2,
            Self::Rgba8888 => 4,
        }
    }

    fn info(self, w: u32, h: u32) -> ImageInfo {
        let (ct, at) = match self {
            Self::Rgb565 => (skia_safe::ColorType::RGB565, skia_safe::AlphaType::Opaque),
            Self::Rgba8888 => (skia_safe::ColorType::RGBA8888, skia_safe::AlphaType::Unpremul),
        };
        ImageInfo::new((w as i32, h as i32), ct, at, None)
    }
}

/// A Skia image over straight-alpha RGBA8 or RGB565 pixels, copied once.
pub(crate) fn raw_image(w: u32, h: u32, format: RawFormat, pixels: &[u8]) -> Option<Image> {
    let row_bytes = w as usize * format.bytes_per_pixel();
    if pixels.len() != row_bytes * h as usize {
        return None;
    }
    images::raster_from_data(&format.info(w, h), Data::new_copy(pixels), row_bytes)
}

/// A cover from the library's decoded art, straight RGBA8 at card size.
pub(crate) fn cover_image(art: &crate::services::art::CardArt) -> Option<Image> {
    raw_image(art.width, art.height, RawFormat::Rgba8888, &art.pixels)
}

/// Card tint for a coverless poster: hashed per title so a library reads as varied, on the
/// kit's face colour so it follows the palette.
fn face_for(title: &str) -> Color4f {
    let hash = title
        .bytes()
        .fold(5381u32, |h, b| h.wrapping_mul(33).wrapping_add(u32::from(b)));
    theme::card_face(0.28 + (hash % 6) as f32 * 0.06)
}

/// The strip's height: one value line plus air, never more than a third of the card.
/// `screen_h` scales the line like the fonts are.
pub(crate) fn strip_h(screen_h: f32, card_h: f32) -> f32 {
    (line_h(f64::from(VALUE * screen_h / 1080.0)) as f32 + STRIP_PAD)
        .min(card_h / 3.0)
        .max(1.0)
}

pub(crate) fn menu_rows_h(rows: usize) -> f32 {
    rows as f32 * MENU_ROW_H + 2.0 * MENU_ROWS_PAD
}

impl App {
    /// Everything under the modals: the grid or what stands in for it, the status band, the
    /// sidebar. Skipped over live video, where all of it would cover the picture.
    pub(crate) fn draw_home(&mut self, f: &Frame<'_>, dt: f64) {
        if self.over_video_layers() {
            return;
        }
        let grid_x = SIDEBAR_W as f32;
        let available_w = f.w - grid_x;
        if self.library.selected_host.is_none() {
            f.fonts.draw(
                f.canvas,
                "No host selected — pick one from the list, or add one.",
                f64::from(grid_x + view::home::GRID_PAD as f32),
                f64::from(view::home::GRID_TOP_Y as f32) + px(f, LABEL),
                W::Medium,
                px(f, LABEL),
                theme::fg(0.6),
            );
        } else if !self.render.grid.reveal.is_revealed() {
            // 40% down rather than dead-centre, which reads as slightly low on a TV.
            let area_h = f.h - view::home::GRID_TOP_Y as f32;
            let cy = view::home::GRID_TOP_Y as f32 + area_h * 0.4;
            theme::spinner(
                f.canvas,
                f64::from(grid_x + available_w / 2.0),
                f64::from(cy),
                SPINNER_R,
                f64::from(self.render.grid.reveal.phase()),
            );
        } else {
            self.draw_grid(f, grid_x, available_w);
        }
        self.draw_status(f, grid_x, available_w);
        self.draw_sidebar(f, dt);
    }

    fn draw_status(&self, f: &Frame<'_>, grid_x: f32, available_w: f32) {
        let (Some(alpha), Some(text)) = (self.home_status_alpha(), self.home_status.as_deref()) else {
            return;
        };
        let size = px(f, LABEL);
        let stride = line_h(size) as f32 + 6.0;
        let box_h = 2.0 * stride + 2.0 * STATUS_BG_PAD as f32;
        let block = Rect::from_xywh(grid_x, f.h - box_h, available_w, box_h);
        let c = f.canvas;
        c.save_layer_alpha_f(Some(block), alpha);
        // Square-cornered: a full-width cut across the bottom edge, not a card.
        c.draw_rect(block, &theme::fill(super::surface()));
        let max_w = available_w - 2.0 * view::home::GRID_PAD as f32;
        let lines = super::wrap(f.fonts, text, W::Medium, size, f64::from(max_w));
        let shown = lines.len().min(2);
        let top = block.top + (box_h - shown as f32 * stride) / 2.0;
        for (i, line) in lines.iter().take(shown).enumerate() {
            f.fonts.draw(
                c,
                line,
                f64::from(grid_x + view::home::GRID_PAD as f32),
                f64::from(top + stride * (i as f32 + 0.8)),
                W::Medium,
                size,
                theme::fg(0.6),
            );
        }
        c.restore();
    }

    fn draw_sidebar(&mut self, f: &Frame<'_>, dt: f64) {
        let c = f.canvas;
        let panel_rect = Rect::from_xywh(0.0, 0.0, SIDEBAR_W as f32, f.h);
        // Opaque on every look, glass included: a lit edge against the grid reads as a seam.
        c.draw_rect(panel_rect, &theme::fill(panel()));
        let x = SIDEBAR_PAD as f32;
        // The entrance plays once, from the first frame the sidebar shows.
        let shown = *self.render.mark_shown_at.get_or_insert_with(Instant::now);
        let intro = brand::intro_progress(shown.elapsed().as_secs_f32());
        brand::draw(c, x + SIDEBAR_ICON_PAD, x, MARK_SIDE, intro);

        let entries = &self.hosts.entries;
        let add_row = entries.len();
        let settings_row = entries.len() + 1;
        let rows = view::sidebar::nav_rows(settings_row + 1, f.h as u32);
        let (focused, menu_focused) = match self.home_focus {
            HomeFocus::Sidebar(i) => (Some(i), false),
            HomeFocus::SidebarMenu(i) => (Some(i), true),
            HomeFocus::Grid(_) => (None, false),
        };
        let selected = self.sidebar_index_of_selected_host();
        let press = self.press_dip(Screen::Home);
        self.render.sidebar_focus.step(rows.len(), focused, dt);
        for (i, base) in rows.iter().copied().enumerate() {
            let is_focused = focused == Some(i);
            let rect = if is_focused { sk(press.rect(base)) } else { sk(base) };
            if i == settings_row {
                let y = rect.top - 14.0;
                c.draw_line((rect.left, y), (rect.right, y), &theme::stroke(theme::fg(0.12), 1.0));
            }
            let fo = self.render.sidebar_focus.at(i);
            with_pop(c, rect, fo, |c| {
                focus_face(c, rect, CARD_RADIUS, fo, selected == Some(i), 1.0);
                let (mark, label): (&str, &str) = match entries.get(i) {
                    Some(entry @ HostEntry::Pinned { .. }) => ("pin", entry.name()),
                    Some(entry) => (if entry.is_paired() { "tv" } else { "lock" }, entry.name()),
                    None if i == add_row => ("plus", "Add host"),
                    None => ("settings", "Settings"),
                };
                let tone = theme::fg(if is_focused { 1.0 } else { 0.6 });
                let icon = Rect::from_xywh(
                    rect.left + SIDEBAR_ICON_PAD,
                    rect.center_y() - SIDEBAR_ICON / 2.0,
                    SIDEBAR_ICON,
                    SIDEBAR_ICON,
                );
                if let Some(m) = by_name(mark) {
                    draw_icon(c, m, icon.center_x(), icon.center_y(), SIDEBAR_ICON, tone);
                }
                let has_menu = entries.get(i).is_some_and(HostEntry::has_menu);
                let reserve = if has_menu {
                    ui::widgets::SIDEBAR_MENU_BTN as f32 + 10.0
                } else {
                    0.0
                };
                let text_x = rect.left + SIDEBAR_ICON_PAD + SIDEBAR_ICON + 16.0;
                let max_w = rect.right - 20.0 - reserve - text_x;
                let size = px(f, LABEL);
                f.fonts.draw_clipped(
                    c,
                    label,
                    f64::from(text_x),
                    f64::from(rect.center_y()) + size * 0.36,
                    W::Medium,
                    size,
                    tone,
                    f64::from(max_w),
                );
                if let Some(entry) = entries.get(i).filter(|e| e.has_menu()) {
                    // Badged onto the icon's corner: a presence dot on the thing it describes.
                    if let Some(online) = self.entry_online(entry) {
                        let (cx, cy) = (icon.right - 1.0, icon.bottom - 2.0);
                        c.draw_circle((cx, cy), PRESENCE_DOT / 2.0 + 2.0, &theme::fill(panel()));
                        let tone = if online { theme::ONLINE_GREEN } else { theme::fg(0.35) };
                        c.draw_circle((cx, cy), PRESENCE_DOT / 2.0, &theme::fill(tone));
                    }
                    let btn = sk(ui::widgets::sidebar_menu_button_rect(super::ui_rect(rect)));
                    let lit = is_focused && menu_focused;
                    if lit {
                        c.draw_rrect(
                            RRect::new_rect_xy(btn, btn.height() / 2.0, btn.height() / 2.0),
                            &theme::fill(theme::accent(0.9)),
                        );
                    }
                    if let Some(m) = by_name("ellipsis") {
                        let tone = if lit {
                            theme::on_accent()
                        } else {
                            theme::fg(if is_focused { 1.0 } else { 0.6 })
                        };
                        draw_icon(c, m, btn.center_x(), btn.center_y(), MENU_GLYPH, tone);
                    }
                }
            });
        }
    }

    fn draw_grid(&mut self, f: &Frame<'_>, grid_x: f32, available_w: f32) {
        let c = f.canvas;
        let grid_xi = grid_x as i32;
        let available_wi = available_w as u32;
        let columns = view::home::grid_columns(available_wi);
        let count = self.grid_len(columns);
        let focused = match self.home_focus {
            HomeFocus::Grid(i) if i < count => Some(i),
            HomeFocus::Grid(_) | HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => None,
        };
        let layout = self.library.layout(columns);
        let scroll = self.render.grid.scroll;
        let card_rect = |idx| view::home::scrolled_card_rect(idx, grid_xi, available_wi, layout, scroll);
        let pad = 24;
        let visible = view::home::visible_cards(available_wi, layout, scroll, f.h as i32, pad);
        // A held card's collection dims to the scrim's level while its order is unwritten.
        let unfixed = self.reordering_slots(layout);
        // The modal scrim's strength: half.
        let dimmed = 0.5;
        let now = Instant::now();
        for idx in visible {
            if Some(idx) == focused {
                continue;
            }
            let Some(game) = layout.card_at(&self.library.games, idx) else {
                continue;
            };
            let (pop, shrink) = Entrance::progress_of(self.render.grid.arrivals.pop(&game.id), now);
            let dim = if unfixed.as_ref().is_some_and(|s| s.contains(&idx)) {
                dimmed
            } else {
                1.0
            };
            let alpha = pop * dim;
            if alpha <= 0.0 {
                continue;
            }
            let r = sk(pop_in_rect(card_rect(idx), pop, shrink));
            theme::drop_shadow(c, r, CARD_RADIUS, 1.0, 0.45 * alpha);
            self.poster(f, r, game, alpha);
        }
        // One heading per section, scrolled with the cards it names.
        let size = px(f, TITLE);
        for (first_idx, group) in layout.headings() {
            let band = view::home::section_heading_rect(first_idx, grid_xi, available_wi, layout, scroll);
            if band.bottom() < 0 || band.y() > f.h as i32 {
                continue;
            }
            f.fonts.draw(
                c,
                &group.name,
                f64::from(band.x()),
                f64::from(band.bottom() - view::home::SECTION_HEADING_PAD) - size * 0.22,
                W::SemiBold,
                size,
                theme::fg(0.6),
            );
        }
        if let Some(idx) = focused {
            if let Some(game) = layout.card_at(&self.library.games, idx) {
                let r = self.press_dip(Screen::Home).rect(card_rect(idx));
                self.draw_focused_card(f, game, r, now);
            }
        }
        // The reveal's dissolve: a background-coloured cover whose alpha falls away as the
        // wave passes, so the page uncovers as one surface.
        if self.render.grid.reveal.dissolving() {
            let (mw, mh, px) = self.render.grid.reveal.dissolve_mask(now);
            if let Some(mask) = raw_image(mw, mh, RawFormat::Rgba8888, px) {
                let cover = Rect::from_xywh(grid_x, 0.0, available_w, f.h);
                c.draw_image_rect_with_sampling_options(&mask, None, cover, linear(), &Paint::default());
            }
        }
    }

    /// The grid slots of the collection whose order a held card has changed and not yet
    /// fixed, if any.
    fn reordering_slots(&self, layout: GridLayout<'_>) -> Option<std::ops::Range<usize>> {
        let menu = self.card_menu.as_ref().filter(|m| m.moved)?;
        layout
            .placed()
            .find(|p| p.slots().contains(&menu.idx))
            .map(|p| p.slots())
    }

    /// Cover art clipped to the card, or a tinted face with the launcher's mark or the
    /// wrapped title on it.
    fn poster(&self, f: &Frame<'_>, r: Rect, game: &GameEntry, alpha: f32) {
        let c = f.canvas;
        if let Some(img) = self.render.covers.get(&game.id) {
            c.save();
            c.clip_rrect(rr(r), ClipOp::Intersect, true);
            c.draw_image_rect_with_sampling_options(img, None, r, linear(), &alpha_paint(alpha));
            c.restore();
            return;
        }
        c.draw_rrect(rr(r), &theme::fill(fade(face_for(&game.title), alpha)));
        let side = (r.width().min(r.height()) * 0.45).max(1.0);
        let box_ = Rect::from_xywh(r.center_x() - side / 2.0, r.center_y() - side / 2.0, side, side);
        let mark = game.icon.as_deref().and_then(|token| match token.strip_prefix("os/") {
            Some(chain) => os_marks::os_mark(chain, box_),
            None => launcher_icons::launcher_mark(token, box_),
        });
        if let Some(path) = mark {
            c.draw_path(&path, &theme::fill(theme::fg(0.92 * alpha)));
            return;
        }
        let pad = 18.0;
        let max_w = (r.width() - 2.0 * pad).max(1.0);
        // The largest size the title fits at, down a short ladder.
        let size = [TITLE, LABEL, VALUE, CAPTION]
            .into_iter()
            .map(|s| px(f, s))
            .find(|&s| f.fonts.measure(&game.title, W::Medium, s) <= max_w)
            .unwrap_or(px(f, CAPTION));
        let stride = line_h(size) as f32 + 4.0;
        let mut lines = super::wrap(f.fonts, &game.title, W::Medium, size, f64::from(max_w));
        let max_lines = (((r.height() - 2.0 * pad) / stride).floor() as usize).max(1);
        lines.truncate(max_lines);
        let block_h = lines.len() as f32 * stride - 4.0;
        let mut y = r.center_y() - block_h / 2.0;
        for line in &lines {
            let w = f.fonts.measure(line, W::Medium, size).min(max_w);
            f.fonts.draw(
                c,
                line,
                f64::from(r.center_x() - w / 2.0),
                f64::from(y) + size * 0.8,
                W::Medium,
                size,
                theme::fg(0.85 * alpha),
            );
            y += stride;
        }
    }

    /// The focused card, drawn last and on top of its neighbours: glow, contact shadow,
    /// focus pop, the title strip or the menu panel a hold grew out of it, and the lit edge.
    fn draw_focused_card(&self, f: &Frame<'_>, game: &GameEntry, base: ui::render::Rect, now: Instant) {
        let c = f.canvas;
        let focus = anim_frac_smooth(self.render.focus_anim, CARD_FOCUS_POP);
        let (pop, shrink) = Entrance::progress_of(self.render.grid.arrivals.pop(&game.id), now);
        let r = sk(pop_in_rect(zoom_rect(base, focus, CARD_GROWTH), pop, shrink));
        // Glow first — a halo behind the card, blooming over the whole travel.
        let mut glow = theme::stroke(theme::accent(0.85 * focus * pop), 6.0);
        glow.set_mask_filter(MaskFilter::blur(BlurStyle::Normal, GLOW_BLUR / 2.0, None));
        c.draw_rrect(rr(r), &glow);
        theme::drop_shadow(c, r, CARD_RADIUS, 1.0, 0.5 * pop);
        self.poster(f, r, game, pop);
        self.draw_card_strip(f, game, r, pop);
        // The lit edge last, over the art and the strip, so the halo has a boundary to end on.
        c.draw_rrect(rr(r), &theme::stroke(theme::accent(0.82 * focus * pop), 1.5));
    }

    /// The title strip wiping up the card's bottom edge, or the taller menu panel grown from
    /// it. Everything is placed in the card's own (already zoomed) rect, so the frost stays
    /// registered with the art beneath it.
    fn draw_card_strip(&self, f: &Frame<'_>, game: &GameEntry, r: Rect, pop: f32) {
        let c = f.canvas;
        let title_h = strip_h(f.h, r.height());
        let pin_id = game.id.as_str();
        // Collapsed to the bare strip while the card is being reordered: Confirm then means
        // "leave it there" rather than any of the rows.
        let menu = self.card_menu.as_ref().filter(|m| !m.moved && m.pin_id == pin_id);
        let kinds = self.card_menu_row_kinds(pin_id);
        let panel_h = (title_h + menu_rows_h(kinds.len())).min(r.height());
        let (shown, wipe) = match menu {
            Some(m) => {
                let wipe = anim_frac_smooth(Some(m.since), CARD_MENU_RISE);
                (title_h + (panel_h - title_h) * wipe, wipe)
            }
            None => (title_h * anim_frac(self.render.focus_anim, CARD_FOCUS_POP), 0.0),
        };
        if shown <= 0.0 {
            return;
        }
        let window = Rect::from_xywh(r.left, r.bottom - shown, r.width(), shown);
        c.save();
        c.clip_rrect(rr(r), ClipOp::Intersect, true);
        c.clip_rect(window, ClipOp::Intersect, true);
        // An opaque strip over the art, the same face every card wears.
        c.draw_rect(window, &theme::fill(super::surface()));
        let overridden = self.game_is_bound(pin_id);
        let size = px(f, VALUE);
        let title_top = window.top;
        let dot_x = r.right - MARK_DOT_INSET - MARK_DOT_R;
        let marked_title = overridden && menu.is_none();
        let right_pad = if marked_title {
            MARK_DOT_INSET + 2.0 * MARK_DOT_R + 8.0
        } else {
            STRIP_INSET
        };
        f.fonts.draw_clipped(
            c,
            &game.title,
            f64::from(r.left + STRIP_INSET),
            f64::from(title_top + title_h / 2.0) + size * 0.36,
            W::Regular,
            size,
            theme::fg(pop),
            f64::from(r.width() - STRIP_INSET - right_pad),
        );
        if marked_title {
            c.draw_circle(
                (dot_x, title_top + title_h / 2.0),
                MARK_DOT_R,
                &theme::fill(theme::accent(pop)),
            );
        }
        if let Some(m) = menu {
            let rows_top = title_top + title_h;
            let band_x = r.left + MENU_BAND_INSET;
            let band_w = r.width() - 2.0 * MENU_BAND_INSET;
            for (i, kind) in kinds.iter().enumerate() {
                let row = Rect::from_xywh(
                    band_x,
                    rows_top + MENU_ROWS_PAD + i as f32 * MENU_ROW_H,
                    band_w,
                    MENU_ROW_H,
                );
                let lit = i == m.focused && wipe >= 1.0;
                if lit {
                    let popped = zoom_rect(
                        super::ui_rect(row),
                        anim_frac(m.focus_anim, ui::animation::FOCUS_POP),
                        ui::animation::FOCUS_GROWTH,
                    );
                    c.draw_rrect(rr(sk(popped)), &theme::fill(theme::accent(0.9 * pop)));
                }
                let tone = if lit { theme::on_accent() } else { theme::fg(0.6 * pop) };
                let (mark, label) = match kind {
                    CardMenuRow::MoveTo => ("pin", view::collections::menu_row_label(self.card_is_held(pin_id))),
                    CardMenuRow::Remove => ("trash-2", "Remove"),
                    CardMenuRow::Profile => ("wrench", "Settings profile\u{2026}"),
                    CardMenuRow::Settings => ("settings", "Settings"),
                };
                let icon_x = row.left + MENU_ICON_INSET;
                if let Some(mk) = by_name(mark) {
                    draw_icon(c, mk, icon_x + 11.0, row.center_y(), 22.0, tone);
                }
                let text_x = icon_x + 22.0 + 10.0;
                let marked = overridden && *kind == CardMenuRow::Settings;
                let right_pad = if marked {
                    MARK_DOT_INSET + 2.0 * MARK_DOT_R + 8.0
                } else {
                    STRIP_INSET
                };
                f.fonts.draw_clipped(
                    c,
                    label,
                    f64::from(text_x),
                    f64::from(row.center_y()) + size * 0.36,
                    W::Regular,
                    size,
                    tone,
                    f64::from(row.right - right_pad - text_x),
                );
                if marked {
                    c.draw_circle(
                        (row.right - MARK_DOT_INSET - MARK_DOT_R, row.center_y()),
                        MARK_DOT_R,
                        &theme::fill(theme::accent(pop)),
                    );
                }
            }
        }
        c.restore();
    }

    /// The launch transition, over everything else: the confirmed card zooming in under a
    /// black scrim, then the hero backdrop fading in, panning, and dissolving into the video.
    pub(crate) fn draw_launch(&mut self, f: &Frame<'_>) {
        let c = f.canvas;
        let Some(t) = self.launch_anim else {
            return;
        };
        if !self.over_video_layers() {
            let frac = anim_frac(Some(t), hero::LAUNCH_FADE);
            let grid_x = SIDEBAR_W as i32;
            let available_w = f.w as u32 - SIDEBAR_W;
            let columns = view::home::grid_columns(available_w);
            let layout = self.library.layout(columns);
            if let Some(game) = self
                .launch_anim_idx
                .and_then(|idx| Some((idx, layout.card_at(&self.library.games, idx)?)))
            {
                let base = view::home::scrolled_card_rect(game.0, grid_x, available_w, layout, self.render.grid.scroll);
                self.poster(f, sk(zoom_rect(base, frac, LAUNCH_GROWTH)), game.1, 1.0);
            }
            c.draw_rect(
                Rect::from_xywh(0.0, 0.0, f.w, f.h),
                &theme::fill(Color4f::new(0.0, 0.0, 0.0, frac)),
            );
        }
        let Some(hero) = self.render.hero.visible() else {
            return;
        };
        let Some(img) = self.render.hero_image.as_ref() else {
            return;
        };
        let dissolving = self.render.hero.dissolving();
        let opacity = if dissolving {
            self.render.hero.fade_in()
        } else {
            self.render.hero.opacity()
        };
        let dst = hero::hero_pan_dst(
            hero.width,
            hero.height,
            f.w as u32,
            f.h as u32,
            self.render.hero.panned_for(),
        );
        c.draw_image_rect_with_sampling_options(
            img,
            None,
            Rect::from_xywh(dst.x, dst.y, dst.w, dst.h),
            linear(),
            &alpha_paint(opacity),
        );
        let scrim = if dissolving {
            self.render.hero.exit_scrim()
        } else {
            hero::HERO_SCRIM_ALPHA * opacity
        };
        c.draw_rect(
            Rect::from_xywh(0.0, 0.0, f.w, f.h),
            &theme::fill(Color4f::new(0.0, 0.0, 0.0, scrim / 255.0)),
        );
        if dissolving {
            // Both taken away again per pixel as the wave passes: what is left is the video.
            let (mw, mh, px) = self.render.hero.dissolve_mask(Instant::now());
            if let Some(mask) = raw_image(mw, mh, RawFormat::Rgba8888, px) {
                let mut erase = Paint::default();
                erase.set_blend_mode(BlendMode::DstOut);
                c.draw_image_rect_with_sampling_options(
                    &mask,
                    None,
                    Rect::from_xywh(0.0, 0.0, f.w, f.h),
                    linear(),
                    &erase,
                );
            }
        }
    }
}

/// Covers built from the library's art, by game id. Skia moves a raster image to the GPU on
/// first draw and keeps the texture, so a card costs one copy when its art lands.
pub(crate) type Covers = HashMap<String, Image>;

#[cfg(test)]
mod tests {
    use super::*;

    /// The lens is lighter than the deep disc, and the box's corners stay clear.
    #[test]
    fn the_mark_is_two_discs_and_a_lens() {
        theme::set_ink(pf_console_ui::theme::Ink::of(pf_console_ui::library::palette("violet")));
        let mut surface = skia_safe::surfaces::raster_n32_premul((60, 60)).unwrap();
        surface.canvas().clear(Color4f::new(0.0, 0.0, 0.0, 1.0));
        brand::draw(surface.canvas(), 6.0, 6.0, 48.0, 1.0);
        let img = surface.image_snapshot();
        if let Ok(dir) = std::env::var("PF_WEBOS_DUMP") {
            let png = img.encode(None, skia_safe::EncodedImageFormat::PNG, 100).unwrap();
            std::fs::write(format!("{dir}/mark.png"), png.as_bytes()).unwrap();
        }
        let px = |x: i32, y: i32| {
            let mut buf = [0u8; 4];
            let info = ImageInfo::new(
                (1, 1),
                skia_safe::ColorType::RGBA8888,
                skia_safe::AlphaType::Unpremul,
                None,
            );
            assert!(img.read_pixels(&info, &mut buf, 4, (x, y), skia_safe::image::CachingHint::Allow));
            buf
        };
        let corner = px(7, 7);
        let deep = px(6 + 40, 6 + 8);
        let lens = px(30, 30);
        assert_eq!(&corner[..3], &[0, 0, 0]);
        assert!(lens[0] > deep[0] && lens[1] > deep[1]);
    }

    #[test]
    fn coverless_posters_pick_a_face_per_title() {
        let a = face_for("Portal");
        let b = face_for("Portal");
        assert_eq!(a, b);
        let art = crate::services::art::CardArt {
            width: 3,
            height: 4,
            pixels: vec![0; 48],
        };
        assert!(cover_image(&art).is_some());
        assert!(raw_image(2, 2, RawFormat::Rgb565, &[0; 8]).is_some());
        assert!(raw_image(2, 2, RawFormat::Rgb565, &[0; 7]).is_none());
    }
}
