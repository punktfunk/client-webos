//! The two-button confirm dialog on the kit: one glass card, a title, a wrapped subtitle,
//! a close mark and two buttons. What differs between Forget host, Send logs and Reset is
//! the [`Confirm`] descriptor; what is the same is here, once.
//!
//! [`layout`] is the geometry, in the frame's pointer units. The hit tests in
//! `app::pointer` call it and [`draw`] draws from it, so the two cannot disagree.

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, PanelStroke, W};
use skia_safe::{Canvas, Color4f, Contains, Point, RRect, Rect};

use pf_console_ui::theme::Fonts;

use super::{glass_card, line_h, scale, ui_rect, wrap, Frame};
use crate::app::screens::confirm::{Confirm, Tone};
use crate::app::view;
use crate::app::App;
use crate::core::screen::Screen;
use crate::ui;

/// Card width as a share of the screen: the desktop shells' dialog width at 1080p.
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

/// Where everything is, for one subtitle. Pointer units.
pub(crate) struct Layout {
    pub card: Rect,
    pub close: Rect,
    pub buttons: [Rect; 2],
    /// Baseline of the title.
    pub title_baseline: f32,
    /// Top of the first subtitle line, and the lines themselves.
    pub body_top: f32,
    pub body: Vec<String>,
}

impl Layout {
    /// Button under `(x, y)`, or `None` off both.
    pub fn button_at(&self, x: i32, y: i32) -> Option<usize> {
        let p = Point::new(x as f32, y as f32);
        self.buttons.iter().position(|b| b.contains(p))
    }

    /// Whether `(x, y)` is on the close mark.
    pub fn on_close(&self, x: i32, y: i32) -> bool {
        self.close.contains(Point::new(x as f32, y as f32))
    }
}

/// The card and everything on it, centred on a `fw`×`fh` frame.
pub(crate) fn layout(fonts: &Fonts, fw: f32, fh: f32, k: f32, subtitle: &str) -> Layout {
    layout_with(fonts, fw, fh, k, subtitle, true)
}

/// [`layout`] for a card with nothing to press: title, body and the close mark.
pub(crate) fn message_layout(fonts: &Fonts, fw: f32, fh: f32, k: f32, subtitle: &str) -> Layout {
    layout_with(fonts, fw, fh, k, subtitle, false)
}

fn layout_with(fonts: &Fonts, fw: f32, fh: f32, k: f32, subtitle: &str, buttons: bool) -> Layout {
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
    let button_row = if buttons { BODY_GAP + BUTTON_H } else { 0.0 };
    let h = (PAD + TITLE_GAP + button_row + PAD) * k + title_h + body_h;
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), ((fh - h) / 2.0).round(), w, h.round());
    let title_baseline = card.top + PAD * k + title_h * 0.8;
    let body_top = card.top + PAD * k + title_h + TITLE_GAP * k;
    let row_top = body_top + body_h + BODY_GAP * k;
    let bw = (inner_w - BUTTON_GAP * k) / 2.0;
    let left = card.left + PAD * k;
    let buttons = [
        Rect::from_xywh(left, row_top, bw, BUTTON_H * k),
        Rect::from_xywh(left + bw + BUTTON_GAP * k, row_top, bw, BUTTON_H * k),
    ];
    let close = Rect::from_xywh(
        card.right - (PAD * 0.6 + CLOSE_BOX) * k,
        card.top + PAD * 0.6 * k,
        CLOSE_BOX * k,
        CLOSE_BOX * k,
    );
    Layout {
        card,
        close,
        buttons,
        title_baseline,
        body_top,
        body,
    }
}

/// The clocks a live card animates on; `None` for one on its way out.
pub(crate) struct Motion {
    pub focus_anim: Option<std::time::Instant>,
    pub press: ui::animation::Press,
    pub hover_close: bool,
}

/// Draw the dialog at `alpha`, risen by `dy` (the open/close motion every modal shares).
pub(crate) fn draw(
    f: &Frame<'_>,
    title: &str,
    confirm: &Confirm,
    focus: usize,
    motion: Option<&Motion>,
    alpha: f32,
    dy: f32,
) {
    let l = layout(f.fonts, f.w, f.h, f.k, &confirm.subtitle);
    draw_on(f, &l, title, confirm.body_tone(), motion, alpha, dy, |c, k| {
        draw_buttons(f, c, k, &l, confirm, focus, motion);
    });
}

/// A card with no buttons: what a wake with no address on record, or a speed test still
/// running, shows.
pub(crate) fn draw_message(
    f: &Frame<'_>,
    title: &str,
    body: &str,
    tone: Color4f,
    hover_close: bool,
    alpha: f32,
    dy: f32,
) {
    let l = message_layout(f.fonts, f.w, f.h, f.k, body);
    let motion = Motion {
        focus_anim: None,
        press: ui::animation::Press::default(),
        hover_close,
    };
    draw_on(f, &l, title, tone, Some(&motion), alpha, dy, |_, _| {});
}

/// The glass, title, body and close mark every dialog shares; `rest` draws what sits under
/// the body inside the same layer.
#[allow(clippy::too_many_arguments)]
fn draw_on(
    f: &Frame<'_>,
    l: &Layout,
    title: &str,
    body_tone: Color4f,
    motion: Option<&Motion>,
    alpha: f32,
    dy: f32,
    rest: impl FnOnce(&Canvas, f32),
) {
    let c = f.canvas;
    let k = f.k;
    c.save();
    c.translate((0.0, dy));
    c.save_layer_alpha_f(Some(l.card), alpha);
    glass_card(c, l.card, CORNER, k);
    // Title, then the body's lines.
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
            body_tone,
        );
    }
    // Close mark, lit under the pointer.
    let hover_close = motion.is_some_and(|m| m.hover_close);
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
    rest(c, k);
    c.restore();
    c.restore();
}

fn draw_buttons(
    f: &Frame<'_>,
    c: &Canvas,
    k: f32,
    l: &Layout,
    confirm: &Confirm,
    focus: usize,
    motion: Option<&Motion>,
) {
    for (i, button) in confirm.buttons.iter().enumerate() {
        let focused = i == focus;
        // The focused button pops and dips on the same clocks a tile did.
        let rect = match (motion, focused) {
            (Some(m), true) => {
                let base = ui_rect(l.buttons[i]);
                let frac = ui::animation::anim_frac(m.focus_anim, ui::animation::FOCUS_POP);
                ui_rect_to_sk(
                    m.press
                        .rect(ui::animation::zoom_rect(base, frac, ui::animation::FOCUS_GROWTH)),
                )
            }
            _ => l.buttons[i],
        };
        let rr = RRect::new_rect_xy(rect, BUTTON_CORNER * k, BUTTON_CORNER * k);
        let tone = tone_color(button.tone);
        if focused {
            c.draw_rrect(rr, &theme::fill(with_alpha(tone, 0.16)));
            let mut sp = theme::stroke(with_alpha(tone, 0.85), 1.5 * k);
            sp.set_anti_alias(true);
            c.draw_rrect(rr, &sp);
        } else {
            theme::panel(c, rect, BUTTON_CORNER, None, PanelStroke::Plain(0.10), k);
        }
        let color = if focused { tone } else { theme::fg(0.75) };
        let size = BODY_SIZE * f64::from(k);
        let icon = button.icon.and_then(by_name);
        let icon_w = if icon.is_some() { (ICON_BOX + 8.0) * k } else { 0.0 };
        let label_w = f
            .fonts
            .measure(&button.label, W::Medium, size)
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
            &button.label,
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

fn ui_rect_to_sk(r: ui::render::Rect) -> Rect {
    super::sk(r)
}

impl Confirm {
    /// What the body is drawn in.
    pub(crate) fn body_tone(&self) -> Color4f {
        if self.failed {
            theme::ERROR
        } else {
            theme::fg(0.72)
        }
    }
}

fn with_alpha(c: Color4f, a: f32) -> Color4f {
    Color4f::new(c.r, c.g, c.b, a)
}

fn tone_color(tone: Tone) -> Color4f {
    match tone {
        Tone::Danger => theme::ERROR,
        Tone::Primary => theme::accent(1.0),
        Tone::Plain => theme::fg(0.9),
    }
}

/// The title each ported dialog wears. `None` for a screen that is not one.
pub(crate) fn title_of(screen: Screen) -> Option<&'static str> {
    Some(match screen {
        Screen::ForgetHost => view::forget::TITLE,
        Screen::SendLogs => view::sendlogs::TITLE,
        Screen::RemoveCollection => view::collections::REMOVE_TITLE,
        Screen::ResetHdrCalibration => view::hdrcalibration::RESET_TITLE,
        Screen::DeleteProfile => view::profile::DELETE_TITLE,
        Screen::Wake => "Wake this host?",
        Screen::SpeedTest => view::speedtest::TITLE,
        _ => return None,
    })
}

impl App {
    /// The dialog's geometry for the confirm that is up, on a frame of the given size.
    /// `None` when no ported confirm is open.
    pub(crate) fn dialog_layout(&self, w: u32, h: u32) -> Option<Layout> {
        let screen = self.nav.screen;
        if !super::ported(screen) {
            return None;
        }
        let (fw, fh, k) = (w as f32, h as f32, scale(h));
        match self.confirm_for(screen) {
            Some(confirm) => Some(layout(&self.fonts, fw, fh, k, &confirm.subtitle)),
            None => {
                let (_, body, _) = self.message_card(screen)?;
                Some(message_layout(&self.fonts, fw, fh, k, &body))
            }
        }
    }

    /// The buttonless card a screen shows while it has nothing to confirm: title, body and
    /// the body's tone. Wake without an address on record; a speed test still running.
    pub(crate) fn message_card(&self, screen: Screen) -> Option<(&'static str, String, Color4f)> {
        match screen {
            Screen::Wake => {
                let wake = self.screens.wake.as_ref()?;
                Some(("Host unreachable", view::wake::status_text(wake), theme::fg(0.72)))
            }
            Screen::SpeedTest => {
                let state = self.screens.speed_test.as_ref();
                let failed = matches!(state, Some(crate::app::state::speedtest::SpeedTestState::Failed(_)));
                let tone = if failed { theme::ERROR } else { theme::fg(0.72) };
                Some((
                    view::speedtest::TITLE,
                    view::speedtest::status(state, &self.screens.speed_test_name),
                    tone,
                ))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_console_ui::theme::Ink;
    use skia_safe::{AlphaType, ColorType, ImageInfo};

    const W_PX: u32 = 1920;
    const H_PX: u32 = 1080;

    fn forget() -> Confirm {
        Confirm::new(
            Some(view::icons::ICON_DELETE),
            "Forget",
            Tone::Danger,
            "Cancel",
            view::forget::subtitle("Gaming PC"),
        )
    }

    /// One frame of the Forget dialog over a flat ground, and its pixels. `PF_WEBOS_DUMP=<dir>`
    /// also writes the PNG, the eyeball counterpart of the console's `PF_CONSOLE_DUMP`.
    fn render(focus: usize) -> (Layout, Vec<u8>) {
        theme::set_ink(Ink::of(pf_console_ui::library::palette("violet")));
        let fonts = theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((W_PX as i32, H_PX as i32)).unwrap();
        surface.canvas().clear(Color4f::new(0.075, 0.063, 0.16, 1.0));
        let confirm = forget();
        {
            let f = Frame::new(surface.canvas(), &fonts, W_PX, H_PX);
            let motion = Motion {
                focus_anim: None,
                press: ui::animation::Press::default(),
                hover_close: false,
            };
            draw(&f, view::forget::TITLE, &confirm, focus, Some(&motion), 1.0, 0.0);
        }
        if let Ok(dir) = std::env::var("PF_WEBOS_DUMP") {
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/dialog-forget-{focus}.png"), png.as_bytes()).unwrap();
        }
        let info = ImageInfo::new(
            (W_PX as i32, H_PX as i32),
            ColorType::RGBA8888,
            AlphaType::Unpremul,
            None,
        );
        let mut out = vec![0u8; (W_PX * H_PX * 4) as usize];
        assert!(surface.read_pixels(&info, &mut out, (W_PX * 4) as usize, (0, 0)));
        (
            layout(&fonts, W_PX as f32, H_PX as f32, scale(H_PX), &confirm.subtitle),
            out,
        )
    }

    fn px(buf: &[u8], x: f32, y: f32) -> [u8; 4] {
        let i = ((y as u32 * W_PX + x as u32) * 4) as usize;
        [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
    }

    /// The card sits centred with its buttons inside it, and a point on each button's centre
    /// hit-tests as that button: what is drawn is what is hit.
    #[test]
    fn layout_is_centred_and_hits_its_own_buttons() {
        let (l, _) = render(0);
        assert!((l.card.center_x() - W_PX as f32 / 2.0).abs() <= 1.0);
        assert!((l.card.center_y() - H_PX as f32 / 2.0).abs() <= 1.0);
        // The Send-logs copy is long enough to wrap, and a taller card stays centred.
        let fonts = theme::build_fonts().unwrap();
        let tall = layout(&fonts, W_PX as f32, H_PX as f32, scale(H_PX), view::sendlogs::SUBTITLE);
        assert!(tall.body.len() >= 2, "{:?}", tall.body);
        assert!(tall.card.height() > l.card.height());
        assert!((tall.card.center_y() - H_PX as f32 / 2.0).abs() <= 1.0);
        for (i, b) in l.buttons.iter().enumerate() {
            assert!(l.card.contains(*b), "button {i} leaves the card");
            assert_eq!(l.button_at(b.center_x() as i32, b.center_y() as i32), Some(i));
        }
        assert_eq!(l.button_at(10, 10), None);
        assert!(l.on_close(l.close.center_x() as i32, l.close.center_y() as i32));
        assert!(!l.on_close(l.card.left as i32 + 4, l.card.top as i32 + 4));
    }

    /// The card is a lighter surface than the ground, the focused Forget button carries the
    /// error tint and Cancel does not; moving focus moves the tint.
    #[test]
    fn focus_tints_the_focused_button_only() {
        let (l, a) = render(0);
        let ground = px(&a, 20.0, 20.0);
        let card = px(&a, l.card.left + 12.0, l.card.center_y());
        assert!(
            card[0] > ground[0] && card[1] > ground[1],
            "card {card:?} over ground {ground:?}"
        );
        let forget = px(&a, l.buttons[0].left + 8.0, l.buttons[0].top + 8.0);
        let cancel = px(&a, l.buttons[1].left + 8.0, l.buttons[1].top + 8.0);
        assert!(forget[0] > cancel[0] + 12, "forget {forget:?} vs cancel {cancel:?}");
        let (_, b) = render(1);
        let forget2 = px(&b, l.buttons[0].left + 8.0, l.buttons[0].top + 8.0);
        assert!(forget2[0] < forget[0], "focus left: {forget2:?} vs {forget:?}");
    }
}
