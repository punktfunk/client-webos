//! The one-field text form and the pairing card on the kit.
//!
//! A form is a glass card lifted clear of the system keyboard when that is up, a title, a
//! subtitle, the field in the kit's "being edited" stroke with a caret, and a caution line
//! when the typed value is refused. The pairing card is the same card with a primary button,
//! an "or" divider and four PIN boxes. Both layouts are what the pointer measures.

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, Fonts, PanelStroke, W};
use skia_safe::{Contains, Point, RRect, Rect};

use super::{glass_card, line_h, wrap, Frame};
use crate::core::screen::PairingFocus;
use crate::ui;

const WIDTH_FRAC: f32 = 0.40;
/// The system keyboard takes roughly the bottom half of the panel (`KEYBOARD_PANEL_FRAC`).
const KEYBOARD_FRAC: f32 = 0.5;
/// Design units.
const PAD: f32 = 26.0;
const CORNER: f32 = 16.0;
const TITLE_SIZE: f64 = 22.0;
const BODY_SIZE: f64 = 15.5;
const FIELD_SIZE: f64 = 24.0;
const TITLE_GAP: f32 = 12.0;
const BODY_GAP: f32 = 18.0;
const FIELD_H: f32 = 60.0;
const FIELD_CORNER: f32 = 12.0;
const HINT_GAP: f32 = 10.0;
const CLOSE_BOX: f32 = 30.0;
const ICON_BOX: f32 = 18.0;
const BUTTON_H: f32 = 48.0;
const DIGIT_W: f32 = 48.0;
const DIGIT_H: f32 = 60.0;
const DIGIT_GAP: f32 = 10.0;

pub(crate) struct Layout {
    pub card: Rect,
    pub close: Rect,
    pub field: Rect,
    pub title_baseline: f32,
    pub body_top: f32,
    pub body: Vec<String>,
    /// Where a caution line goes, under the field.
    pub hint_baseline: f32,
}

impl Layout {
    pub fn on_close(&self, x: i32, y: i32) -> bool {
        self.close.contains(Point::new(x as f32, y as f32))
    }

    /// The field as the app's integer rect, for `SDL_SetTextInputRect`.
    pub fn field_rect(&self) -> ui::render::Rect {
        super::ui_rect(self.field)
    }
}

/// The card for a form with `subtitle`, with room for a hint when `hint`, lifted when the
/// system keyboard is shown.
pub(crate) fn layout(fonts: &Fonts, fw: f32, fh: f32, k: f32, subtitle: &str, hint: bool, keyboard: bool) -> Layout {
    let w = (fw * WIDTH_FRAC).round();
    let inner_w = w - 2.0 * PAD * k;
    let body = wrap(
        fonts,
        subtitle,
        W::Regular,
        BODY_SIZE * f64::from(k),
        f64::from(inner_w),
    );
    let title_h = line_h(TITLE_SIZE * f64::from(k)) as f32;
    let body_line = line_h(BODY_SIZE * f64::from(k)) as f32;
    let body_h = body_line * body.len() as f32;
    let hint_h = if hint {
        line_h(BODY_SIZE * f64::from(k)) as f32 + HINT_GAP * k
    } else {
        0.0
    };
    let h = (PAD + TITLE_GAP + BODY_GAP + FIELD_H + PAD) * k + title_h + body_h + hint_h;
    // Centred in the top half while the keyboard covers the bottom one, else in the screen.
    let room = if keyboard { fh * (1.0 - KEYBOARD_FRAC) } else { fh };
    let top = ((room - h) / 2.0).max(24.0 * k).round();
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), top, w, h.round());
    let title_baseline = card.top + PAD * k + title_h * 0.8;
    let body_top = card.top + PAD * k + title_h + TITLE_GAP * k;
    let field = Rect::from_xywh(
        card.left + PAD * k,
        body_top + body_h + BODY_GAP * k,
        inner_w,
        FIELD_H * k,
    );
    let hint_baseline = field.bottom + HINT_GAP * k + line_h(BODY_SIZE * f64::from(k)) as f32 * 0.8;
    let close = Rect::from_xywh(
        card.right - (PAD * 0.6 + CLOSE_BOX) * k,
        card.top + PAD * 0.6 * k,
        CLOSE_BOX * k,
        CLOSE_BOX * k,
    );
    Layout {
        card,
        close,
        field,
        title_baseline,
        body_top,
        body,
        hint_baseline,
    }
}

fn header(
    f: &Frame<'_>,
    l_card: Rect,
    title: &str,
    title_baseline: f32,
    body: &[String],
    body_top: f32,
    hover_close: bool,
) {
    let c = f.canvas;
    let k = f.k;
    f.fonts.draw_clipped(
        c,
        title,
        f64::from(l_card.left + PAD * k),
        f64::from(title_baseline),
        W::SemiBold,
        TITLE_SIZE * f64::from(k),
        theme::fg(1.0),
        f64::from(l_card.width() - (2.0 * PAD + CLOSE_BOX) * k),
    );
    let body_line = line_h(BODY_SIZE * f64::from(k)) as f32;
    for (i, line) in body.iter().enumerate() {
        f.fonts.draw(
            c,
            line,
            f64::from(l_card.left + PAD * k),
            f64::from(body_top + body_line * (i as f32 + 0.8)),
            W::Regular,
            BODY_SIZE * f64::from(k),
            theme::fg(0.72),
        );
    }
    if let Some(x) = by_name("x") {
        let close = Rect::from_xywh(
            l_card.right - (PAD * 0.6 + CLOSE_BOX) * k,
            l_card.top + PAD * 0.6 * k,
            CLOSE_BOX * k,
            CLOSE_BOX * k,
        );
        draw_icon(
            c,
            x,
            close.center_x(),
            close.center_y(),
            ICON_BOX * k,
            theme::fg(if hover_close { 1.0 } else { 0.5 }),
        );
    }
}

/// Draw the form: `typed` in the field with a caret after it, `hint` under it in the error tone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    f: &Frame<'_>,
    l: &Layout,
    title: &str,
    typed: &str,
    hint: Option<&str>,
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
    header(f, l.card, title, l.title_baseline, &l.body, l.body_top, hover_close);
    theme::panel(
        c,
        l.field,
        FIELD_CORNER,
        Some(theme::accent(0.10)),
        PanelStroke::Brand(0.7),
        k,
    );
    let size = FIELD_SIZE * f64::from(k);
    let text_x = l.field.left + 18.0 * k;
    let baseline = l.field.center_y() + size as f32 * 0.35;
    let advance = f.fonts.draw(
        c,
        typed,
        f64::from(text_x),
        f64::from(baseline),
        W::Medium,
        size,
        theme::fg(1.0),
    );
    // A blinkless caret right after what is typed: the field has no mask to say where.
    c.draw_rect(
        Rect::from_xywh(
            text_x + advance + 4.0 * k,
            l.field.top + 14.0 * k,
            2.0 * k,
            l.field.height() - 28.0 * k,
        ),
        &theme::fill(theme::accent(1.0)),
    );
    if let Some(hint) = hint {
        f.fonts.draw(
            c,
            hint,
            f64::from(l.field.left),
            f64::from(l.hint_baseline),
            W::Regular,
            BODY_SIZE * f64::from(k),
            theme::ERROR,
        );
    }
    c.restore();
    c.restore();
}

// Pairing

pub(crate) const PAIR_TITLE: &str = "Pair with host";
pub(crate) const PAIR_SUBTITLE: &str = "Two ways to pair with this host — either one works.";
const PAIR_BUTTON_CAPTION: &str = "Then approve this TV on the host.";
const PAIR_PIN_CAPTION: &str = "Enter the PIN shown on the host.";
pub(crate) const REQUEST_LABEL: &str = "Request access";

pub(crate) struct PairLayout {
    pub card: Rect,
    pub close: Rect,
    pub button: Rect,
    pub digits: [Rect; 4],
    pub title_baseline: f32,
    pub body_top: f32,
    pub body: Vec<String>,
    pub button_caption_baseline: f32,
    pub or_y: f32,
    pub pin_caption_baseline: f32,
    pub status_top: f32,
}

impl PairLayout {
    pub fn on_close(&self, x: i32, y: i32) -> bool {
        self.close.contains(Point::new(x as f32, y as f32))
    }

    pub fn on_button(&self, x: i32, y: i32) -> bool {
        self.button.contains(Point::new(x as f32, y as f32))
    }

    pub fn digit_at(&self, x: i32, y: i32) -> Option<usize> {
        let p = Point::new(x as f32, y as f32);
        self.digits.iter().position(|d| d.contains(p))
    }
}

pub(crate) fn pair_layout(fonts: &Fonts, fw: f32, fh: f32, k: f32, has_status: bool) -> PairLayout {
    let w = (fw * WIDTH_FRAC).round();
    let inner_w = w - 2.0 * PAD * k;
    let body = wrap(
        fonts,
        PAIR_SUBTITLE,
        W::Regular,
        BODY_SIZE * f64::from(k),
        f64::from(inner_w),
    );
    let title_h = line_h(TITLE_SIZE * f64::from(k)) as f32;
    let body_line = line_h(BODY_SIZE * f64::from(k)) as f32;
    let body_h = body_line * body.len() as f32;
    let caption_h = body_line;
    let status_h = if has_status { body_line * 2.0 + 8.0 * k } else { 0.0 };
    let h = (PAD + TITLE_GAP + BODY_GAP + BUTTON_H + 8.0 + 28.0 + 8.0 + DIGIT_H + PAD) * k
        + title_h
        + body_h
        + caption_h * 2.0
        + status_h;
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), ((fh - h) / 2.0).round(), w, h.round());
    let title_baseline = card.top + PAD * k + title_h * 0.8;
    let body_top = card.top + PAD * k + title_h + TITLE_GAP * k;
    let left = card.left + PAD * k;
    let button = Rect::from_xywh(left, body_top + body_h + BODY_GAP * k, inner_w, BUTTON_H * k);
    let button_caption_baseline = button.bottom + 8.0 * k + caption_h * 0.8;
    let or_y = button.bottom + 8.0 * k + caption_h + 14.0 * k;
    let pin_caption_baseline = or_y + 14.0 * k + caption_h * 0.8;
    let digits_top = pin_caption_baseline + caption_h * 0.2 + 8.0 * k;
    let row_w = 4.0 * DIGIT_W * k + 3.0 * DIGIT_GAP * k;
    let x0 = card.center_x() - row_w / 2.0;
    let digits = std::array::from_fn(|i| {
        Rect::from_xywh(
            x0 + i as f32 * (DIGIT_W + DIGIT_GAP) * k,
            digits_top,
            DIGIT_W * k,
            DIGIT_H * k,
        )
    });
    let status_top = digits_top + DIGIT_H * k + 8.0 * k;
    let close = Rect::from_xywh(
        card.right - (PAD * 0.6 + CLOSE_BOX) * k,
        card.top + PAD * 0.6 * k,
        CLOSE_BOX * k,
        CLOSE_BOX * k,
    );
    PairLayout {
        card,
        close,
        button,
        digits,
        title_baseline,
        body_top,
        body,
        button_caption_baseline,
        or_y,
        pin_caption_baseline,
        status_top,
    }
}

/// What the pairing card shows this frame.
pub(crate) struct PairState<'a> {
    pub digits: [u8; 4],
    pub digit_index: usize,
    pub focus: PairingFocus,
    pub status: Option<&'a str>,
    pub busy: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_pair(f: &Frame<'_>, l: &PairLayout, s: &PairState<'_>, hover_close: bool, alpha: f32, dy: f32) {
    let c = f.canvas;
    let k = f.k;
    c.save();
    c.translate((0.0, dy));
    c.save_layer_alpha_f(Some(l.card), alpha);
    glass_card(c, l.card, CORNER, k);
    header(
        f,
        l.card,
        PAIR_TITLE,
        l.title_baseline,
        &l.body,
        l.body_top,
        hover_close,
    );
    // The primary button: approving on the host always works, so it leads and is filled.
    let focused = s.focus == PairingFocus::RequestAccess;
    let rr = RRect::new_rect_xy(l.button, 12.0 * k, 12.0 * k);
    c.draw_rrect(rr, &theme::fill(theme::accent(if focused { 1.0 } else { 0.55 })));
    if focused {
        let mut sp = theme::stroke(theme::fg(0.9), 1.5 * k);
        sp.set_anti_alias(true);
        c.draw_rrect(rr, &sp);
    }
    let size = BODY_SIZE * f64::from(k);
    let tw = f.fonts.measure(REQUEST_LABEL, W::SemiBold, size);
    f.fonts.draw(
        c,
        REQUEST_LABEL,
        f64::from(l.button.center_x() - tw / 2.0),
        f64::from(l.button.center_y() + size as f32 * 0.35),
        W::SemiBold,
        size,
        theme::on_accent(),
    );
    centred(
        f,
        l.card,
        l.button_caption_baseline,
        PAIR_BUTTON_CAPTION,
        theme::fg(0.6),
    );
    // "or" between two hairlines.
    let or_w = f.fonts.measure("or", W::Medium, size);
    let cx = l.card.center_x();
    let inner_left = l.card.left + PAD * k;
    let inner_right = l.card.right - PAD * k;
    let line = theme::stroke(theme::fg(0.18), 1.0);
    c.draw_line((inner_left, l.or_y), (cx - or_w / 2.0 - 12.0 * k, l.or_y), &line);
    c.draw_line((cx + or_w / 2.0 + 12.0 * k, l.or_y), (inner_right, l.or_y), &line);
    f.fonts.draw(
        c,
        "or",
        f64::from(cx - or_w / 2.0),
        f64::from(l.or_y + size as f32 * 0.35),
        W::Medium,
        size,
        theme::fg(0.55),
    );
    centred(f, l.card, l.pin_caption_baseline, PAIR_PIN_CAPTION, theme::fg(0.6));
    for (i, rect) in l.digits.iter().enumerate() {
        let lit = s.focus == PairingFocus::Pin && i == s.digit_index;
        theme::panel(
            c,
            *rect,
            10.0,
            lit.then(|| theme::accent(0.25)),
            if lit {
                PanelStroke::Brand(0.9)
            } else {
                PanelStroke::Plain(0.12)
            },
            k,
        );
        let text = s.digits[i].to_string();
        let dsize = FIELD_SIZE * f64::from(k);
        let tw = f.fonts.measure(&text, W::SemiBold, dsize);
        f.fonts.draw(
            c,
            &text,
            f64::from(rect.center_x() - tw / 2.0),
            f64::from(rect.center_y() + dsize as f32 * 0.35),
            W::SemiBold,
            dsize,
            theme::fg(1.0),
        );
    }
    if let Some(status) = s.status {
        let color = if s.busy { theme::fg(0.6) } else { theme::ERROR };
        let lines = wrap(
            f.fonts,
            status,
            W::Regular,
            size,
            f64::from(l.card.width() - 2.0 * PAD * k),
        );
        let line_h = line_h(size) as f32;
        for (i, line) in lines.iter().take(2).enumerate() {
            f.fonts.draw(
                c,
                line,
                f64::from(inner_left),
                f64::from(l.status_top + line_h * (i as f32 + 0.8)),
                W::Regular,
                size,
                color,
            );
        }
    }
    c.restore();
    c.restore();
}

fn centred(f: &Frame<'_>, card: Rect, baseline: f32, text: &str, color: skia_safe::Color4f) {
    let size = BODY_SIZE * f64::from(f.k);
    let tw = f.fonts.measure(text, W::Regular, size);
    f.fonts.draw(
        f.canvas,
        text,
        f64::from(card.center_x() - tw / 2.0),
        f64::from(baseline),
        W::Regular,
        size,
        color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_lifts_clear_of_the_keyboard_and_keeps_its_field_inside() {
        let fonts = theme::build_fonts().unwrap();
        let k = super::super::scale(1080);
        let down = layout(&fonts, 1920.0, 1080.0, k, "Type the host's address.", true, false);
        let up = layout(&fonts, 1920.0, 1080.0, k, "Type the host's address.", true, true);
        assert!(
            up.card.bottom <= 1080.0 * (1.0 - KEYBOARD_FRAC) + 1.0,
            "{}",
            up.card.bottom
        );
        assert!(up.card.top < down.card.top);
        assert!(down.card.contains(down.field));
        assert!(down.hint_baseline < down.card.bottom);
        let p = pair_layout(&fonts, 1920.0, 1080.0, k, true);
        assert!(p.card.contains(p.button));
        for d in &p.digits {
            assert!(p.card.contains(*d));
        }
        assert_eq!(
            p.digit_at(p.digits[2].center_x() as i32, p.digits[2].center_y() as i32),
            Some(2)
        );
        assert!(p.on_button(p.button.center_x() as i32, p.button.center_y() as i32));
    }
}
