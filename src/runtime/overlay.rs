//! The overlays drawn over live video and over the menus, on the console's GL context: the
//! stats card, the log tail, the toast, and the two-button confirm dialog (stop streaming,
//! quit). Immediate mode on the kit, like every menu screen (`app::draw`); the stream loop
//! keeps its own redraw cadence and its transparent clear for NDL's punch-through plane.

use std::time::Instant;

use anyhow::Result;
use pf_console_ui::theme::{self, Fonts, PanelStroke, W};
use skia_safe::{Color4f, RRect, Rect};

use crate::app::draw::dialog::{self, Motion};
use crate::app::draw::{line_h, wrap, Frame};
use crate::app::screens::confirm::{Confirm, Tone};
use crate::console::ConsoleGl;
use crate::core::event::MenuEvent;
use crate::ui;

/// Design units.
const STATS_PAD: f32 = 14.0;
const STATS_LINE: f64 = 14.0;
const STATS_HINT: f64 = 11.5;
const STATS_INSET: f32 = 18.0;
const STATS_CORNER: f32 = 12.0;
const LOG_PAD: f32 = 10.0;
const LOG_LINE: f64 = 11.5;
const LOG_INDENT: f32 = 16.0;
const TOAST_TOP: f32 = 18.0;
const TOAST_PAD_X: f32 = 18.0;
const TOAST_PAD_Y: f32 = 10.0;
const TOAST_SIZE: f64 = 15.0;
/// How many log lines the tail shows.
pub(super) const LOG_LINES: usize = 9;
/// How long a toast holds before its fade.
const NOTIFICATION_HOLD: std::time::Duration = std::time::Duration::from_secs(2);

/// One frame on the GL context: the surface, cleared to `clear`, scaled from `display`
/// units to the drawable. `draw` paints in display units; the frame is then swapped.
pub(super) fn frame(
    gl: &mut Option<ConsoleGl>,
    canvas: &sdl2::render::WindowCanvas,
    fonts: &Fonts,
    display: (u32, u32),
    clear: Color4f,
    draw: impl FnOnce(&Frame<'_>),
) -> Result<()> {
    let gl = super::console_flow::bring_up(gl, canvas)?;
    let (dw, dh) = canvas.window().drawable_size();
    {
        let surface = gl.surface(dw, dh)?;
        let c = surface.canvas();
        c.clear(clear);
        c.reset_matrix();
        c.scale((dw as f32 / display.0.max(1) as f32, dh as f32 / display.1.max(1) as f32));
        fonts.begin_frame();
        draw(&Frame::new(c, fonts, display.0, display.1));
    }
    gl.flush();
    canvas.window().gl_swap_window();
    Ok(())
}

/// Fully transparent: what the stream clears to so the video plane shows through.
pub(super) const TRANSPARENT: Color4f = Color4f::new(0.0, 0.0, 0.0, 0.0);

/// Two swaps of nothing, so both buffers of the window are wiped.
pub(super) fn wipe(gl: &mut Option<ConsoleGl>, canvas: &sdl2::render::WindowCanvas, fonts: &Fonts) -> Result<()> {
    for _ in 0..2 {
        frame(gl, canvas, fonts, (1, 1), TRANSPARENT, |_| {})?;
    }
    Ok(())
}

/// The stats card, top right: the first line bright, the rest muted, the hint centred under.
pub(super) fn stats(f: &Frame<'_>, lines: &[String], hint: &str, alpha: f32) {
    let k = f.k;
    let size = STATS_LINE * f64::from(k);
    let stride = line_h(size) as f32;
    let hint_size = STATS_HINT * f64::from(k);
    let widest = lines
        .iter()
        .map(|l| f.fonts.measure(l, W::Medium, size))
        .fold(f.fonts.measure(hint, W::Regular, hint_size), f32::max);
    let w = widest + 2.0 * STATS_PAD * k;
    let h = stride * lines.len() as f32 + line_h(hint_size) as f32 + 2.0 * STATS_PAD * k;
    let card = Rect::from_xywh(f.w - STATS_INSET * k - w, STATS_INSET * k, w, h);
    let c = f.canvas;
    c.save_layer_alpha_f(Some(card), alpha);
    c.draw_rrect(
        RRect::new_rect_xy(card, STATS_CORNER * k, STATS_CORNER * k),
        &theme::fill(crate::app::draw::surface()),
    );
    theme::panel(c, card, STATS_CORNER, None, PanelStroke::Plain(0.12), k);
    let x = f64::from(card.left + STATS_PAD * k);
    for (i, line) in lines.iter().enumerate() {
        let tone = theme::fg(if i == 0 { 1.0 } else { 0.7 });
        f.fonts.draw(
            c,
            line,
            x,
            f64::from(card.top + STATS_PAD * k + stride * (i as f32 + 0.8)),
            W::Medium,
            size,
            tone,
        );
    }
    let hint_w = f.fonts.measure(hint, W::Regular, hint_size);
    f.fonts.draw(
        c,
        hint,
        f64::from(card.center_x() - hint_w / 2.0),
        f64::from(card.top + STATS_PAD * k + stride * lines.len() as f32) + line_h(hint_size) * 0.8,
        W::Regular,
        hint_size,
        theme::fg(0.5),
    );
    c.restore();
}

fn log_tone(line: &str) -> Color4f {
    match line.split_whitespace().next() {
        Some("ERROR") => theme::ERROR,
        Some("WARN") => Color4f::new(1.0, 0.76, 0.03, 1.0),
        Some("INFO") => theme::fg(1.0),
        _ => theme::fg(0.6),
    }
}

/// The log tail, a full-width strip along the bottom edge, wrapped lines indented.
pub(super) fn log(f: &Frame<'_>, lines: &[String], alpha: f32) {
    let k = f.k;
    let size = LOG_LINE * f64::from(k);
    let stride = line_h(size) as f32;
    let wrap_w = f.w - 2.0 * LOG_PAD * k - LOG_INDENT * k;
    let rows: Vec<(f32, String, Color4f)> = lines
        .iter()
        .flat_map(|line| {
            let tone = log_tone(line);
            let wrapped = wrap(f.fonts, line, W::Regular, size, f64::from(wrap_w));
            wrapped.into_iter().enumerate().map(move |(i, text)| {
                let x = if i == 0 { 0.0 } else { LOG_INDENT * k };
                (x, text, tone)
            })
        })
        .collect();
    let h = stride * rows.len().max(1) as f32 + 2.0 * LOG_PAD * k;
    let strip = Rect::from_xywh(0.0, f.h - h, f.w, h);
    let c = f.canvas;
    c.save_layer_alpha_f(Some(strip), alpha);
    c.draw_rect(strip, &theme::fill(crate::app::draw::surface()));
    for (i, (dx, text, tone)) in rows.iter().enumerate() {
        f.fonts.draw(
            c,
            text,
            f64::from(LOG_PAD * k + dx),
            f64::from(strip.top + LOG_PAD * k + stride * (i as f32 + 0.8)),
            W::Regular,
            size,
            *tone,
        );
    }
    c.restore();
}

/// A toast pill along the top edge.
pub(super) fn toast(f: &Frame<'_>, text: &str, alpha: f32) {
    let k = f.k;
    let size = TOAST_SIZE * f64::from(k);
    let w = f.fonts.measure(text, W::Medium, size) + 2.0 * TOAST_PAD_X * k;
    let h = line_h(size) as f32 + 2.0 * TOAST_PAD_Y * k;
    let pill = Rect::from_xywh((f.w - w) / 2.0, TOAST_TOP * k, w, h);
    let c = f.canvas;
    c.save_layer_alpha_f(Some(pill), alpha);
    c.draw_rrect(
        RRect::new_rect_xy(pill, h / 2.0, h / 2.0),
        &theme::fill(crate::app::draw::surface()),
    );
    theme::panel(c, pill, h / 2.0 / k, None, PanelStroke::Gradient, k);
    f.fonts.draw(
        c,
        text,
        f64::from(pill.left + TOAST_PAD_X * k),
        f64::from(pill.top + TOAST_PAD_Y * k) + line_h(size) * 0.8,
        W::Medium,
        size,
        theme::fg(1.0),
    );
    c.restore();
}

/// What a [`ConfirmDialog`] event did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ConfirmAction {
    /// Primary (index 0) button activated.
    Confirmed,
    /// Cancel/Back — the close fade has started.
    Dismissed,
    /// Focus moved between buttons.
    Navigated,
}

/// A two-button confirm dialog (stop streaming mid-stream, quit in the menu) with the same
/// open/close fade as the menu's modals, drawn as `app::draw::dialog` draws them.
pub(super) struct ConfirmDialog {
    title: &'static str,
    confirm: Confirm,
    focus: Option<usize>,
    fade: ui::fade::ModalFade<usize>,
    focus_anim: Option<Instant>,
    /// The focused button's press dip, playing out over the close fade it starts.
    press: ui::animation::Press,
    hover_close: bool,
}

impl ConfirmDialog {
    pub(super) fn new(
        title: &'static str,
        subtitle: &'static str,
        icon: Option<&'static str>,
        label: &'static str,
        tone: Tone,
    ) -> Self {
        Self {
            title,
            confirm: Confirm::new(icon, label, tone, "Cancel", subtitle.to_string()),
            focus: None,
            fade: ui::fade::ModalFade::modal(),
            focus_anim: None,
            press: ui::animation::Press::default(),
            hover_close: false,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.focus.is_some()
    }

    pub(super) fn open_with(&mut self, focus: usize, subtitle: &'static str) {
        self.confirm.subtitle = subtitle.to_string();
        self.open(focus);
    }

    pub(super) fn open(&mut self, focus: usize) {
        self.focus = Some(focus);
        self.press = ui::animation::Press::default();
        self.fade.reopen();
        self.focus_anim = Some(Instant::now());
    }

    fn set_focus(&mut self, focus: usize) {
        self.focus = Some(focus);
        self.focus_anim = Some(Instant::now());
    }

    pub(super) fn dismiss(&mut self) {
        if let Some(focus) = self.focus.take() {
            self.fade.close(focus);
        }
    }

    /// `(focus, alpha, closing)` while there is anything to draw.
    pub(super) fn frame(&self) -> Option<(usize, f32, bool)> {
        if let Some((alpha, focus)) = self.fade.closing_frame() {
            return Some((focus, alpha, true));
        }
        self.focus.map(|focus| (focus, self.fade.open_alpha(), false))
    }

    pub(super) fn tick(&mut self) -> bool {
        self.fade.tick()
    }

    /// Pointer and pad input while open. The layout is the same one [`Self::draw`] draws.
    pub(super) fn handle_event(
        &mut self,
        event: &sdl2::event::Event,
        fonts: &Fonts,
        w: u32,
        h: u32,
    ) -> Option<ConfirmAction> {
        use sdl2::event::Event;
        let focus = self.focus?;
        let l = dialog::layout(
            fonts,
            w as f32,
            h as f32,
            crate::app::draw::scale(h),
            &self.confirm.subtitle,
        );
        match *event {
            Event::MouseMotion { x, y, .. } => {
                self.hover_close = l.on_close(x, y);
                return match l.button_at(x, y) {
                    Some(i) if i != focus => {
                        self.set_focus(i);
                        Some(ConfirmAction::Navigated)
                    }
                    _ => None,
                };
            }
            Event::MouseButtonDown {
                mouse_btn: sdl2::mouse::MouseButton::Left,
                x,
                y,
                ..
            } => {
                if l.on_close(x, y) {
                    self.dismiss();
                    return Some(ConfirmAction::Dismissed);
                }
                let i = l.button_at(x, y)?;
                self.press.arm();
                return Some(if i == 0 {
                    ConfirmAction::Confirmed
                } else {
                    self.dismiss();
                    ConfirmAction::Dismissed
                });
            }
            _ => {}
        }
        let nav = match event {
            Event::KeyDown {
                keycode: Some(k),
                repeat: false,
                ..
            } => crate::platform::webos::input::menu_event_for_key(*k),
            Event::ControllerButtonDown { button, .. } => crate::platform::webos::input::menu_event_for_button(*button),
            _ => None,
        };
        match nav {
            Some(MenuEvent::Left | MenuEvent::Right) => {
                self.set_focus(1 - focus);
                Some(ConfirmAction::Navigated)
            }
            Some(MenuEvent::Confirm) if focus == 0 => {
                self.press.arm();
                Some(ConfirmAction::Confirmed)
            }
            Some(ev @ (MenuEvent::Confirm | MenuEvent::Back)) => {
                if ev == MenuEvent::Confirm {
                    self.press.arm();
                }
                self.dismiss();
                Some(ConfirmAction::Dismissed)
            }
            _ => None,
        }
    }

    /// The dialog at its fade's alpha, risen with it.
    pub(super) fn draw(&self, f: &Frame<'_>) {
        let Some((focus, alpha, closing)) = self.frame() else {
            return;
        };
        let motion = Motion {
            focus_anim: self.focus_anim,
            press: self.press,
            hover_close: self.hover_close && !closing,
        };
        let dy = ui::animation::modal_rise(alpha) as f32;
        dialog::draw(f, self.title, &self.confirm, focus, Some(&motion), alpha, dy);
    }
}

/// A transient message with a hold-then-fade lifetime.
pub(super) struct Notification {
    text: String,
    shown_at: Option<Instant>,
}

impl Notification {
    pub(super) fn new() -> Self {
        Self {
            text: String::new(),
            shown_at: None,
        }
    }

    pub(super) fn show(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.shown_at = Some(Instant::now());
    }

    /// The text and its alpha while visible; clears itself once faded.
    pub(super) fn frame(&mut self) -> Option<(&str, f32)> {
        let at = self.shown_at?;
        match ui::fade::hold_alpha(at, NOTIFICATION_HOLD, ui::fade::OVERLAY_FADE) {
            Some(alpha) => Some((&self.text, alpha)),
            None => {
                self.shown_at = None;
                None
            }
        }
    }
}
