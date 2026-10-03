//! The overlays drawn over live video on the console's GL context: the stats card, the log
//! tail, the toast, and the stop-streaming dialog. Immediate mode on the kit; the stream loop
//! keeps its own redraw cadence and its transparent clear for NDL's punch-through plane.

use std::time::{Duration, Instant};

use anyhow::Result;
use pf_console_ui::theme::{self, Fonts, PanelStroke, W};
use punktfunk_core::hud::{HudCorner, HudLine, Role};
use skia_safe::{Canvas, Color4f, RRect, Rect};

use crate::console::ConsoleGl;
use crate::platform::webos::input::RemoteKey;
use pf_client_core::menu_nav::{MenuDir, MenuEvent};

mod dialog;
pub(super) mod fade;

use dialog::{Motion, Press, FOCUS_POP};

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
pub(super) const LOG_LINES: usize = 9;
const NOTIFICATION_HOLD: Duration = Duration::from_secs(2);

/// One frame's canvas and its size in display units.
#[derive(Clone, Copy)]
pub(super) struct Frame<'a> {
    pub canvas: &'a Canvas,
    pub fonts: &'a Fonts,
    pub w: f32,
    pub h: f32,
    /// Pixels per design unit.
    pub k: f32,
}

impl<'a> Frame<'a> {
    fn new(canvas: &'a Canvas, fonts: &'a Fonts, w: u32, h: u32) -> Self {
        Self {
            canvas,
            fonts,
            w: w as f32,
            h: h as f32,
            k: scale(h),
        }
    }
}

/// The kit's own default: 800 design units tall, clamped between a Deck and a 4K panel.
fn scale(h: u32) -> f32 {
    (h as f32 / 800.0).clamp(0.75, 3.0)
}

/// Geist's line box at `size`: the ascent-to-descent span comes out near 1.25 em.
pub(super) fn line_h(size: f64) -> f64 {
    size * 1.25
}

/// Greedy word wrap on the kit's single-line measure. A word wider than `max_w` stands alone.
pub(super) fn wrap(fonts: &Fonts, text: &str, w: W, size: f64, max_w: f64) -> Vec<String> {
    let font = fonts.font(w, size);
    let measure = |s: &str| f64::from(font.measure_str(s, None).0);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if line.is_empty() {
            line.push_str(word);
            continue;
        }
        let kept = line.len();
        line.push(' ');
        line.push_str(word);
        if measure(&line) > max_w {
            line.truncate(kept);
            lines.push(std::mem::replace(&mut line, word.to_string()));
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// The overlays' card face.
fn surface() -> Color4f {
    theme::card_face(0.16)
}

/// An opaque card. Never frosted: the video sits on a hardware plane our GL context cannot read.
pub(super) fn opaque_card(f: &Frame<'_>, rect: Rect, corner: f32) {
    f.canvas.draw_rrect(
        RRect::new_rect_xy(rect, corner * f.k, corner * f.k),
        &theme::fill(surface()),
    );
    theme::panel(f.canvas, rect, corner, None, PanelStroke::Gradient, f.k);
}

/// Layer with optional alpha, skipped at full alpha. Caller must pair with one `restore`.
/// The clip stops overflow (unwrapped words, shadows).
pub(super) fn alpha_layer(c: &Canvas, r: Rect, alpha: f32) {
    if alpha >= 1.0 {
        c.save();
        c.clip_rect(r, skia_safe::ClipOp::Intersect, false);
        return;
    }
    c.save_layer_alpha_f(Some(r), alpha);
}

/// One frame on the GL context: the surface, cleared to `clear`, scaled from `display`
/// units to the drawable. `draw` paints in display units; the frame is then swapped.
pub(super) fn frame(
    gl: &mut Option<ConsoleGl>,
    window: &sdl3::video::Window,
    fonts: &Fonts,
    display: (u32, u32),
    clear: Color4f,
    draw: impl FnOnce(&Frame<'_>),
) -> Result<()> {
    // Never vsync: every caller here draws over live video, on the thread forwarding input.
    let gl = super::console_flow::bring_up(gl, window, false)?;
    let (dw, dh) = window.size_in_pixels();
    {
        let surface = gl.surface(dw, dh)?;
        let c = surface.canvas();
        c.clear(clear);
        c.scale((dw as f32 / display.0.max(1) as f32, dh as f32 / display.1.max(1) as f32));
        fonts.begin_frame();
        draw(&Frame::new(c, fonts, display.0, display.1));
    }
    gl.flush();
    window.gl_swap_window();
    Ok(())
}

/// Whether a cosmetic frame drew, warning once per streak. webOS composites this app's
/// punch-through plane only while it holds the SAM foreground, so a TV panel over the video —
/// the settings one the remote opens — fails every GL call on this surface until it closes. That
/// has to freeze the overlay and nothing else: the stream and calibration loops' `Err` leaves
/// `run_inner` and ends the process.
pub(super) fn drawn(result: Result<()>, warned: &mut bool) -> bool {
    let Err(e) = result else {
        *warned = false;
        return true;
    };
    if !std::mem::replace(warned, true) {
        tracing::warn!("overlay frame skipped — this surface is not ours to draw on: {e:#}");
    }
    false
}

/// Fully transparent: what the stream clears to so the video plane shows through.
pub(super) const TRANSPARENT: Color4f = Color4f::new(0.0, 0.0, 0.0, 0.0);

/// Two swaps of nothing, so both buffers of the window are wiped.
pub(super) fn wipe(gl: &mut Option<ConsoleGl>, window: &sdl3::video::Window, fonts: &Fonts) -> Result<()> {
    for _ in 0..2 {
        frame(gl, window, fonts, (1, 1), TRANSPARENT, |_| {})?;
    }
    Ok(())
}

/// The log tail's WARN tone, shared with the stats card's warning lines.
const WARN_AMBER: Color4f = Color4f::new(1.0, 0.76, 0.03, 1.0);

/// Headline bright, breakdowns softer, asides dimmer, warnings amber.
fn role_tone(role: Role) -> Color4f {
    match role {
        Role::Primary => theme::fg(1.0),
        Role::Detail => theme::fg(0.78),
        Role::Muted => theme::fg(0.6),
        Role::Warn => WARN_AMBER,
    }
}

/// The stats card in `corner`, at `scale` times the display's size (`Settings::stats_scale_pct`).
pub(super) fn stats(f: &Frame<'_>, lines: &[HudLine], hint: &str, alpha: f32, corner: HudCorner, scale: f32) {
    let k = f.k * scale;
    let size = STATS_LINE * f64::from(k);
    let stride = line_h(size) as f32;
    let hint_size = STATS_HINT * f64::from(k);
    let widest = lines
        .iter()
        .map(|l| f.fonts.measure(&l.text, W::Medium, size))
        .fold(f.fonts.measure(hint, W::Regular, hint_size), f32::max);
    let w = widest + 2.0 * STATS_PAD * k;
    let h = stride * lines.len() as f32 + line_h(hint_size) as f32 + 2.0 * STATS_PAD * k;
    let inset = STATS_INSET * f.k;
    let x = if corner.right() { f.w - inset - w } else { inset };
    let y = if corner.bottom() { f.h - inset - h } else { inset };
    let card = Rect::from_xywh(x, y, w, h);
    let c = f.canvas;
    alpha_layer(c, card, alpha);
    c.draw_rrect(
        RRect::new_rect_xy(card, STATS_CORNER * k, STATS_CORNER * k),
        &theme::fill(surface()),
    );
    theme::panel(c, card, STATS_CORNER, None, PanelStroke::Plain(0.12), k);
    let x = f64::from(card.left + STATS_PAD * k);
    for (i, line) in lines.iter().enumerate() {
        f.fonts.draw(
            c,
            &line.text,
            x,
            f64::from(card.top + STATS_PAD * k + stride * (i as f32 + 0.8)),
            W::Medium,
            size,
            role_tone(line.role),
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
        Some("WARN") => WARN_AMBER,
        Some("INFO") => theme::fg(1.0),
        _ => theme::fg(0.6),
    }
}

/// The log tail wrapped for one frame width, kept until the lines or the width change: a fade
/// redraws it at 30 Hz, and wrapping measures every word.
#[derive(Default)]
pub(super) struct LogRows {
    lines: Vec<String>,
    w: f32,
    rows: Vec<(f32, String, Color4f)>,
}

impl LogRows {
    fn update(&mut self, f: &Frame<'_>, lines: Vec<String>) {
        if self.w == f.w && self.lines == lines {
            return;
        }
        let k = f.k;
        let size = LOG_LINE * f64::from(k);
        let wrap_w = f.w - 2.0 * LOG_PAD * k - LOG_INDENT * k;
        self.rows.clear();
        for line in &lines {
            let tone = log_tone(line);
            let wrapped = wrap(f.fonts, line, W::Regular, size, f64::from(wrap_w));
            self.rows.extend(wrapped.into_iter().enumerate().map(|(i, text)| {
                let x = if i == 0 { 0.0 } else { LOG_INDENT * k };
                (x, text, tone)
            }));
        }
        self.lines = lines;
        self.w = f.w;
    }
}

pub(super) fn log(f: &Frame<'_>, cache: &mut LogRows, lines: Vec<String>) {
    cache.update(f, lines);
    let rows = &cache.rows;
    let k = f.k;
    let size = LOG_LINE * f64::from(k);
    let stride = line_h(size) as f32;
    let h = stride * rows.len().max(1) as f32 + 2.0 * LOG_PAD * k;
    let strip = Rect::from_xywh(0.0, f.h - h, f.w, h);
    let c = f.canvas;
    c.draw_rect(strip, &theme::fill(surface()));
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
}

pub(super) fn toast(f: &Frame<'_>, text: &str, alpha: f32) {
    pill(f, text, alpha, false);
}

/// The one-line exit hint at stream start: the toast's pill, bottom centre.
pub(super) fn exit_hint(f: &Frame<'_>, text: &str, alpha: f32) {
    pill(f, text, alpha, true);
}

/// A centred pill of one line, `TOAST_TOP` in from the top or the bottom edge.
fn pill(f: &Frame<'_>, text: &str, alpha: f32, bottom: bool) {
    let k = f.k;
    let size = TOAST_SIZE * f64::from(k);
    let w = f.fonts.measure(text, W::Medium, size) + 2.0 * TOAST_PAD_X * k;
    let h = line_h(size) as f32 + 2.0 * TOAST_PAD_Y * k;
    let y = if bottom { f.h - TOAST_TOP * k - h } else { TOAST_TOP * k };
    let pill = Rect::from_xywh((f.w - w) / 2.0, y, w, h);
    let c = f.canvas;
    alpha_layer(c, pill, alpha);
    c.draw_rrect(RRect::new_rect_xy(pill, h / 2.0, h / 2.0), &theme::fill(surface()));
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

// Decouple from stream's 2ms polling.
const DIALOG_FRAME_STEP: Duration = Duration::from_millis(16);

/// A two-button confirm dialog: a destructive action and Cancel, with an open/close fade.
pub(super) struct ConfirmDialog {
    title: &'static str,
    subtitle: &'static str,
    label: &'static str,
    focus: Option<usize>,
    fade: fade::ModalFade<usize>,
    focus_anim: Option<Instant>,
    /// The focused button's press dip, playing out over the close fade it starts.
    press: Press,
    hover_close: bool,
    last_draw: Option<Instant>,
    last_visual: Option<(usize, bool, bool, bool)>,
    /// Laid out on open: the subtitle and the display never change, and wrapping measures every
    /// word, which pointer motion would otherwise redo at ~100 Hz. Kept through the close fade.
    layout: Option<dialog::Layout>,
}

impl ConfirmDialog {
    pub(super) fn new(title: &'static str, subtitle: &'static str, label: &'static str) -> Self {
        Self {
            title,
            subtitle,
            label,
            focus: None,
            fade: fade::ModalFade::modal(),
            focus_anim: None,
            press: Press::default(),
            hover_close: false,
            last_draw: None,
            last_visual: None,
            layout: None,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.focus.is_some()
    }

    /// Opens on a `w`×`h` display with `focus` on.
    pub(super) fn open(&mut self, focus: usize, fonts: &Fonts, (w, h): (u32, u32)) {
        if self.layout.is_none() {
            self.layout = Some(dialog::layout(fonts, w as f32, h as f32, scale(h), self.subtitle));
        }
        self.last_visual = None;
        self.last_draw = None;
        self.focus = Some(focus);
        self.press = Press::default();
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
        let mut animating = self.fade.tick();
        animating |= self.press.armed() && !self.press.landed();
        if let Some(t) = self.focus_anim {
            if t.elapsed() >= FOCUS_POP {
                self.focus_anim = None;
            }
            animating = true;
        }
        animating
    }

    pub(super) fn redraw_due(&mut self, animating: bool) -> bool {
        let Some((focus, _, closing)) = self.frame() else {
            return false;
        };
        let visual = (focus, closing, self.hover_close, animating);
        if self.last_visual != Some(visual)
            || (animating && self.last_draw.is_none_or(|at| at.elapsed() >= DIALOG_FRAME_STEP))
        {
            self.last_visual = Some(visual);
            self.last_draw = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// Pointer and pad input while open. The layout is the same one [`Self::draw`] draws.
    ///
    /// `remote` is this event's key as the caller's
    /// [`RemoteKeys`](crate::platform::webos::input::RemoteKeys) resolved it; never re-derive it.
    pub(super) fn handle_event(
        &mut self,
        event: &sdl3::event::Event,
        remote: Option<crate::platform::webos::input::RemoteKey>,
    ) -> Option<ConfirmAction> {
        use sdl3::event::Event;
        let focus = self.focus?;
        // `(on the close mark, the button under the pointer)` for pointer events.
        let hit = match (event, &self.layout) {
            (Event::MouseMotion { x, y, .. } | Event::MouseButtonDown { x, y, .. }, Some(l)) => {
                (l.on_close(*x, *y), l.button_at(*x, *y))
            }
            _ => (false, None),
        };
        match *event {
            Event::MouseMotion { .. } => {
                let hover_close = hit.0;
                let hover_changed = self.hover_close != hover_close;
                self.hover_close = hover_close;
                return match hit.1 {
                    Some(i) if i != focus => {
                        self.set_focus(i);
                        Some(ConfirmAction::Navigated)
                    }
                    _ if hover_changed => Some(ConfirmAction::Navigated),
                    _ => None,
                };
            }
            Event::MouseButtonDown {
                mouse_btn: sdl3::mouse::MouseButton::Left,
                ..
            } => {
                if hit.0 {
                    self.dismiss();
                    return Some(ConfirmAction::Dismissed);
                }
                let i = hit.1?;
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
        let nav = if remote == Some(RemoteKey::Back) {
            Some(MenuEvent::Back)
        } else {
            match event {
                Event::KeyDown {
                    keycode: Some(k),
                    repeat: false,
                    ..
                } => crate::platform::webos::input::menu_event_for_key(*k),
                Event::GamepadButtonDown { which, button, .. } => {
                    crate::platform::webos::input::menu_event_for_button(*which, *button)
                }
                _ => None,
            }
        };
        match nav {
            Some(MenuEvent::Move(MenuDir::Left | MenuDir::Right)) => {
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
        let (Some((focus, alpha, closing)), Some(l)) = (self.frame(), &self.layout) else {
            return;
        };
        let motion = Motion {
            focus_anim: self.focus_anim,
            press: self.press,
            hover_close: self.hover_close && !closing,
        };
        dialog::draw(f, l, self.title, [self.label, "Cancel"], focus, &motion, alpha);
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
        match fade::hold_alpha(at, NOTIFICATION_HOLD, fade::OVERLAY_FADE) {
            Some(alpha) => Some((&self.text, alpha)),
            None => {
                self.shown_at = None;
                None
            }
        }
    }
}

#[cfg(test)]
mod dialog_tests {
    use super::*;

    #[test]
    fn close_hover_requests_redraw_only_when_changed() {
        let fonts = theme::build_fonts().unwrap();
        let mut dialog = ConfirmDialog::new("Stop?", "Close the stream", "Stop");
        dialog.open(0, &fonts, (1920, 1080));
        let l = dialog::layout(&fonts, 1920.0, 1080.0, scale(1080), "Close the stream");
        let motion = |x, y| sdl3::event::Event::MouseMotion {
            timestamp: 0,
            window_id: 0,
            which: 0,
            mousestate: sdl3::mouse::MouseState::from_sdl_state(0),
            x,
            y,
            xrel: 0.0,
            yrel: 0.0,
        };
        let enter = motion(l.close.center_x(), l.close.center_y());
        assert!(dialog.handle_event(&enter, None).is_some());
        assert!(dialog.hover_close);
        assert!(dialog.handle_event(&enter, None).is_none());
        assert!(dialog.handle_event(&motion(0.0, 0.0), None).is_some());
        assert!(!dialog.hover_close);
    }

    #[test]
    fn settled_dialog_stays_visible_without_requesting_frames() {
        let mut dialog = ConfirmDialog::new("Stop?", "Close the stream", "Stop");
        dialog.focus = Some(0);
        dialog.focus_anim = Some(Instant::now() - FOCUS_POP);
        assert!(dialog.tick());
        assert!(!dialog.tick());
        assert!(dialog.frame().is_some());
        dialog.dismiss();
        assert!(dialog.tick());
    }
}
