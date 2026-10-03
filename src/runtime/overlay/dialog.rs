//! The two-button confirm card drawn over live video: title, wrapped subtitle, a close mark,
//! the action and Cancel. [`layout`] is the geometry; the hit tests and [`draw`] both read it,
//! so what is drawn is what is hit.

use std::time::{Duration, Instant};

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, Fonts, PanelStroke, W};
use skia_safe::{Color4f, Contains, Point, RRect, Rect};

use super::fade::anim_frac;
use super::{alpha_layer, line_h, opaque_card, wrap, Frame};

/// Card width as a share of the screen.
const WIDTH_FRAC: f32 = 0.40;
/// Design units.
const PAD: f32 = 26.0;
const CORNER: f32 = 16.0;
const TITLE_SIZE: f64 = 22.0;
const BODY_SIZE: f64 = 15.5;
const TITLE_GAP: f32 = 12.0;
const BODY_GAP: f32 = 24.0;
const BUTTON_H: f32 = 48.0;
const BUTTON_GAP: f32 = 12.0;
const BUTTON_CORNER: f32 = 12.0;
const CLOSE_BOX: f32 = 30.0;
const ICON_BOX: f32 = 18.0;

/// How long a focused button takes to pop to its grown size, and by how much.
pub(super) const FOCUS_POP: Duration = Duration::from_millis(140);
const FOCUS_GROWTH: f32 = 0.02;
/// How long a pressed button takes to spring back out of its dip, and how far it drops.
const PRESS_POP: Duration = Duration::from_millis(120);
const PRESS_DROP: f32 = 5.0;
/// How far the card rises as it fades in.
const RISE: f32 = 26.0;

/// The focused button's press dip. Visual only: the action runs immediately.
#[derive(Default, Clone, Copy)]
pub(super) struct Press(Option<Instant>);

impl Press {
    pub(super) fn arm(&mut self) {
        self.0 = Some(Instant::now());
    }

    pub(super) fn armed(self) -> bool {
        self.0.is_some()
    }

    pub(super) fn landed(self) -> bool {
        self.0.is_some_and(|t| t.elapsed() >= PRESS_POP)
    }

    fn drop_by(self) -> f32 {
        PRESS_DROP * (1.0 - anim_frac(self.0, PRESS_POP))
    }
}

/// Where everything is, for one subtitle. Pointer units.
pub(super) struct Layout {
    pub card: Rect,
    pub close: Rect,
    pub buttons: [Rect; 2],
    title_baseline: f32,
    body_top: f32,
    body: Vec<String>,
}

impl Layout {
    pub(super) fn button_at(&self, x: f32, y: f32) -> Option<usize> {
        self.buttons.iter().position(|b| b.contains(Point::new(x, y)))
    }

    pub(super) fn on_close(&self, x: f32, y: f32) -> bool {
        self.close.contains(Point::new(x, y))
    }
}

/// The card centred on a `fw`×`fh` frame.
pub(super) fn layout(fonts: &Fonts, fw: f32, fh: f32, k: f32, subtitle: &str) -> Layout {
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
    let body_h = line_h(BODY_SIZE * f64::from(k)) as f32 * body.len() as f32;
    let h = (PAD + TITLE_GAP + BODY_GAP + BUTTON_H + PAD) * k + title_h + body_h;
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), ((fh - h) / 2.0).round(), w, h.round());
    let body_top = card.top + PAD * k + title_h + TITLE_GAP * k;
    let row_top = body_top + body_h + BODY_GAP * k;
    let bw = (inner_w - BUTTON_GAP * k) / 2.0;
    let left = card.left + PAD * k;
    Layout {
        card,
        close: Rect::from_xywh(
            card.right - (PAD * 0.6 + CLOSE_BOX) * k,
            card.top + PAD * 0.6 * k,
            CLOSE_BOX * k,
            CLOSE_BOX * k,
        ),
        buttons: [
            Rect::from_xywh(left, row_top, bw, BUTTON_H * k),
            Rect::from_xywh(left + bw + BUTTON_GAP * k, row_top, bw, BUTTON_H * k),
        ],
        title_baseline: card.top + PAD * k + title_h * 0.8,
        body_top,
        body,
    }
}

/// The clocks a live card animates on.
pub(super) struct Motion {
    pub focus_anim: Option<Instant>,
    pub press: Press,
    pub hover_close: bool,
}

/// Draw the card laid out as `l` at `alpha`, risen with it. Button 0 is the destructive action.
pub(super) fn draw(
    f: &Frame<'_>,
    l: &Layout,
    title: &str,
    labels: [&str; 2],
    focus: usize,
    motion: &Motion,
    alpha: f32,
) {
    let (c, k) = (f.canvas, f.k);
    c.save();
    c.translate((0.0, ((1.0 - alpha) * RISE).round()));
    alpha_layer(c, l.card, alpha);
    opaque_card(f, l.card, CORNER);
    f.fonts.draw_clipped(
        c,
        title,
        f64::from(l.card.left + PAD * k),
        f64::from(l.title_baseline),
        W::SemiBold,
        TITLE_SIZE * f64::from(k),
        theme::fg(1.0),
        f64::from(l.card.width() - (2.0 * PAD + CLOSE_BOX) * k),
    );
    let body_line = line_h(BODY_SIZE * f64::from(k)) as f32;
    for (i, line) in l.body.iter().enumerate() {
        f.fonts.draw(
            c,
            line,
            f64::from(l.card.left + PAD * k),
            f64::from(l.body_top + body_line * (i as f32 + 0.8)),
            W::Regular,
            BODY_SIZE * f64::from(k),
            theme::fg(0.72),
        );
    }
    if let Some(x) = by_name("x") {
        let tone = theme::fg(if motion.hover_close { 1.0 } else { 0.5 });
        draw_icon(c, x, l.close.center_x(), l.close.center_y(), ICON_BOX * k, tone);
    }
    for (i, label) in labels.iter().enumerate() {
        let focused = i == focus;
        let rect = if focused {
            let grow = 1.0 + FOCUS_GROWTH * anim_frac(motion.focus_anim, FOCUS_POP);
            let b = l.buttons[i];
            Rect::from_xywh(
                b.center_x() - b.width() * grow / 2.0,
                b.center_y() - b.height() * grow / 2.0 + motion.press.drop_by(),
                b.width() * grow,
                b.height() * grow,
            )
        } else {
            l.buttons[i]
        };
        let tone = if i == 0 { theme::ERROR } else { theme::fg(0.9) };
        let rr = RRect::new_rect_xy(rect, BUTTON_CORNER * k, BUTTON_CORNER * k);
        if focused {
            c.draw_rrect(rr, &theme::fill(Color4f { a: 0.16, ..tone }));
            let mut sp = theme::stroke(Color4f { a: 0.85, ..tone }, 1.5 * k);
            sp.set_anti_alias(true);
            c.draw_rrect(rr, &sp);
        } else {
            theme::panel(c, rect, BUTTON_CORNER, None, PanelStroke::Plain(0.10), k);
        }
        let color = if focused { tone } else { theme::fg(0.75) };
        let size = BODY_SIZE * f64::from(k);
        let icon = (i == 0).then(|| by_name("x")).flatten();
        let icon_w = if icon.is_some() { (ICON_BOX + 8.0) * k } else { 0.0 };
        let label_w = f
            .fonts
            .measure(label, W::Medium, size)
            .min(rect.width() - icon_w - 16.0 * k);
        let start = rect.center_x() - (icon_w + label_w) / 2.0;
        if let Some(icon) = icon {
            draw_icon(
                c,
                icon,
                start + ICON_BOX * k / 2.0,
                rect.center_y(),
                ICON_BOX * k,
                color,
            );
        }
        f.fonts.draw_clipped(
            c,
            label,
            f64::from(start + icon_w),
            f64::from(rect.center_y() + size as f32 * 0.35),
            W::Medium,
            size,
            color,
            f64::from(label_w),
        );
    }
    c.restore();
    c.restore();
}
