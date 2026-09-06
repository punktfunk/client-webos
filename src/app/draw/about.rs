//! The About & licences page on the kit: a tall glass card, the version, and the notices
//! document scrolled a line at a time. Only the visible window is drawn; the wrap is done
//! once per width and kept.

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, Fonts, W};
use skia_safe::{Contains, Point, Rect};

use super::{glass_card, line_h, wrap, Frame};

const WIDTH_FRAC: f32 = 0.78;
const HEIGHT_FRAC: f32 = 0.84;
/// Design units.
const PAD: f32 = 26.0;
const CORNER: f32 = 18.0;
const TITLE_SIZE: f64 = 22.0;
const SUB_SIZE: f64 = 14.5;
const BODY_SIZE: f64 = 13.0;
const HEADER_GAP: f32 = 16.0;
const CLOSE_BOX: f32 = 30.0;
const ICON_BOX: f32 = 18.0;

pub(crate) struct Layout {
    pub card: Rect,
    pub close: Rect,
    pub body: Rect,
    pub title_baseline: f32,
    pub sub_baseline: f32,
    /// Pixel stride between visual lines.
    pub stride: f32,
    /// How many visual lines the body shows.
    pub visible: usize,
}

impl Layout {
    pub fn on_close(&self, x: i32, y: i32) -> bool {
        self.close.contains(Point::new(x as f32, y as f32))
    }
}

pub(crate) fn layout(fw: f32, fh: f32, k: f32) -> Layout {
    let w = (fw * WIDTH_FRAC).round();
    let h = (fh * HEIGHT_FRAC).round();
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), ((fh - h) / 2.0).round(), w, h);
    let title_h = line_h(TITLE_SIZE * f64::from(k)) as f32;
    let sub_h = line_h(SUB_SIZE * f64::from(k)) as f32;
    let title_baseline = card.top + PAD * k + title_h * 0.8;
    let sub_baseline = card.top + PAD * k + title_h + sub_h * 0.8;
    let body_top = card.top + PAD * k + title_h + sub_h + HEADER_GAP * k;
    let stride = line_h(BODY_SIZE * f64::from(k)) as f32;
    let body = Rect::from_xywh(
        card.left + PAD * k,
        body_top,
        w - 2.0 * PAD * k,
        card.bottom - PAD * k - body_top,
    );
    let visible = ((body.height() / stride).floor() as usize).max(1);
    let close = Rect::from_xywh(
        card.right - (PAD * 0.6 + CLOSE_BOX) * k,
        card.top + PAD * 0.6 * k,
        CLOSE_BOX * k,
        CLOSE_BOX * k,
    );
    Layout {
        card,
        close,
        body,
        title_baseline,
        sub_baseline,
        stride,
        visible,
    }
}

/// The document wrapped to `max_w` at the body size. Lines that cannot exceed the width are
/// not measured: the notices are ~12,000 lines, most of them short.
pub(crate) fn wrap_document(fonts: &Fonts, k: f32, lines: &[&'static str], max_w: f32) -> Vec<String> {
    let size = BODY_SIZE * f64::from(k);
    // Widest plausible glyph run per character, from a wide sample.
    let per_char = f64::from(fonts.measure("MMMMMMMMMM", W::Regular, size)) / 10.0;
    let safe_chars = (f64::from(max_w) / per_char).floor() as usize;
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        if line.trim().is_empty() {
            out.push(String::new());
        } else if line.chars().count() <= safe_chars {
            out.push((*line).to_string());
        } else {
            out.extend(wrap(fonts, line, W::Regular, size, f64::from(max_w)));
        }
    }
    out
}

/// Draw the page with visual lines from `scroll` on. `alpha` and `dy` are the modal's
/// open motion.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    f: &Frame<'_>,
    l: &Layout,
    title: &str,
    subtitle: &str,
    lines: &[String],
    scroll: usize,
    hover_close: bool,
    alpha: f32,
    dy: f32,
) {
    let c = f.canvas;
    let k = f.k;
    c.save();
    c.translate((0.0, dy));
    c.save_layer_alpha_f(Some(l.card), alpha);
    glass_card(c, l.card, CORNER, k);
    let x = f64::from(l.card.left + PAD * k);
    f.fonts.draw(
        c,
        title,
        x,
        f64::from(l.title_baseline),
        W::SemiBold,
        TITLE_SIZE * f64::from(k),
        theme::fg(1.0),
    );
    f.fonts.draw(
        c,
        subtitle,
        x,
        f64::from(l.sub_baseline),
        W::Regular,
        SUB_SIZE * f64::from(k),
        theme::fg(0.6),
    );
    if let Some(mark) = by_name("x") {
        draw_icon(
            c,
            mark,
            l.close.center_x(),
            l.close.center_y(),
            ICON_BOX * k,
            theme::fg(if hover_close { 1.0 } else { 0.5 }),
        );
    }
    c.save();
    c.clip_rect(l.body, None, true);
    let size = BODY_SIZE * f64::from(k);
    for (i, line) in lines.iter().skip(scroll).take(l.visible).enumerate() {
        if line.is_empty() {
            continue;
        }
        f.fonts.draw(
            c,
            line,
            f64::from(l.body.left),
            f64::from(l.body.top + l.stride * (i as f32 + 0.8)),
            W::Regular,
            size,
            theme::fg(0.85),
        );
    }
    c.restore();
    // A thin track on the right says how far along the document the window is.
    if lines.len() > l.visible {
        let track = Rect::from_xywh(l.card.right - 12.0 * k, l.body.top, 4.0 * k, l.body.height());
        c.draw_rrect(
            skia_safe::RRect::new_rect_xy(track, 2.0 * k, 2.0 * k),
            &theme::fill(theme::fg(0.12)),
        );
        let frac = l.visible as f32 / lines.len() as f32;
        let at = scroll as f32 / (lines.len() - l.visible).max(1) as f32;
        let thumb_h = (track.height() * frac).max(24.0 * k);
        let thumb = Rect::from_xywh(
            track.left,
            track.top + (track.height() - thumb_h) * at,
            track.width(),
            thumb_h,
        );
        c.draw_rrect(
            skia_safe::RRect::new_rect_xy(thumb, 2.0 * k, 2.0 * k),
            &theme::fill(theme::fg(0.45)),
        );
    }
    c.restore();
    c.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_pass_unmeasured_and_long_ones_wrap() {
        let fonts = theme::build_fonts().unwrap();
        let k = super::super::scale(1080);
        let long: &'static str = Box::leak("word ".repeat(120).into_boxed_str());
        let doc = wrap_document(&fonts, k, &["short", "", long], 400.0);
        assert_eq!(doc[0], "short");
        assert_eq!(doc[1], "");
        assert!(doc.len() > 5, "{} lines", doc.len());
        let l = layout(1920.0, 1080.0, k);
        assert!(l.visible > 10 && l.body.top > l.sub_baseline);
    }
}
