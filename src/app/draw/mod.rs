//! The pointer UI's screens on the console kit (planning `design/webos-pointer-ui-overhaul.md` WP3).
//!
//! Immediate mode: a screen in [`ported`] draws itself on the frame's canvas every tick, from
//! app state, with `pf_console_ui`'s `theme` and `widgets`. Its geometry is one `layout` per
//! screen that the pointer hit tests call too, so what is drawn is what is hit. Everything
//! else still arrives as tiles through `render::skia` until its turn.
//!
//! Sizes are the console's design units, scaled by [`Frame::k`] — the same rule
//! (`height / 800`) the shell applies, so a row here is a row there.

pub(crate) mod about;
pub(crate) mod dialog;
pub(crate) mod form;
pub(crate) mod home;
pub(crate) mod list;
pub(crate) mod settings;

use pf_console_ui::anim::approach;
use pf_console_ui::theme::{self, Fonts, PanelStroke, W};
use skia_safe::{Canvas, RRect, Rect};

use crate::app::App;
use crate::core::screen::Screen;
use crate::ui;

/// Which screens draw here rather than as tiles. Every prepare, compose and hit-test path
/// asks this, so a screen moves over by being added to this list and nowhere else.
pub(crate) const fn ported(screen: Screen) -> bool {
    matches!(
        screen,
        Screen::ForgetHost
            | Screen::SendLogs
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::HostMenu
            | Screen::HostPower
            | Screen::SettingsPage
            | Screen::DeleteProfile
            | Screen::About
            | Screen::AddHost
            | Screen::EditHost
            | Screen::RenameCollection
            | Screen::RenameProfile
            | Screen::Pairing
            | Screen::Wake
            | Screen::SpeedTest
            | Screen::Collections
            | Screen::HdrCalibration
            | Screen::PickProfile
    )
}

/// The ported screens that are a list card (`draw::list`) rather than a dialog.
pub(crate) const fn is_list(screen: Screen) -> bool {
    matches!(
        screen,
        Screen::HostMenu | Screen::HostPower | Screen::Collections | Screen::HdrCalibration | Screen::PickProfile
    )
}

/// What every draw fn takes.
pub(crate) struct Frame<'a> {
    pub canvas: &'a Canvas,
    pub fonts: &'a Fonts,
    /// Layout size, in the units pointer events arrive in.
    pub w: f32,
    pub h: f32,
    /// Pixels per design unit.
    pub k: f32,
}

impl<'a> Frame<'a> {
    pub fn new(canvas: &'a Canvas, fonts: &'a Fonts, w: u32, h: u32) -> Self {
        Self {
            canvas,
            fonts,
            w: w as f32,
            h: h as f32,
            k: scale(h),
        }
    }
}

/// The console's `Viewport` default: 800 design units tall, clamped between a Deck and a 4K
/// panel. 1.35 at 1080p.
pub(crate) fn scale(h: u32) -> f32 {
    (h as f32 / 800.0).clamp(0.75, 3.0)
}

/// Geist's line box at `size`: the ascent-to-descent span comes out near 1.25 em.
pub(crate) fn line_h(size: f64) -> f64 {
    size * 1.25
}

/// Greedy word wrap on the kit's single-line measure, in device pixels. A word wider than
/// `max_w` stands alone and overflows rather than being split mid-word.
pub(crate) fn wrap(fonts: &Fonts, text: &str, w: W, size: f64, max_w: f64) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || f64::from(fonts.measure(&candidate, w, size)) <= max_w {
            line = candidate;
        } else {
            lines.push(std::mem::replace(&mut line, word.to_string()));
        }
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// A raised card: an opaque face lifted off the ground toward the accent, the kit's panel
/// tint and hairline over it. Opaque on purpose: the TV's GL has no backdrop blur to make a
/// translucent card read as glass, and a plain translucent card over a grid of covers read
/// as a mistake.
pub(crate) fn glass_card(canvas: &Canvas, rect: Rect, corner: f32, k: f32) {
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    canvas.draw_rrect(rr, &theme::fill(surface()));
    theme::panel(canvas, rect, corner, None, PanelStroke::Gradient, k);
}

/// The face of a raised card.
pub(crate) fn surface() -> skia_safe::Color4f {
    theme::card_face(0.16)
}

/// The dim over whatever a modal covers, at the modal's alpha.
pub(crate) fn scrim(canvas: &Canvas, w: f32, h: f32, alpha: f32) {
    canvas.draw_rect(
        Rect::from_xywh(0.0, 0.0, w, h),
        &theme::fill(theme::shade(0.45 * alpha)),
    );
}

/// An `ui::render::Rect` as Skia's.
pub(crate) fn sk(r: ui::render::Rect) -> Rect {
    Rect::from_xywh(r.x() as f32, r.y() as f32, r.width() as f32, r.height() as f32)
}

/// The frame's ground: the shared palette's own, flat. The console draws an aurora field
/// over the same colour; a pointer UI with a grid of covers wants it quiet.
pub(crate) fn ground() -> skia_safe::Color4f {
    let (r, g, b) = pf_console_ui::library::palette(&current_palette()).ground;
    skia_safe::Color4f::new(r as f32, g as f32, b as f32, 1.0)
}

/// The sidebar's opaque panel: the ground lifted a step toward the accent, under [`surface`].
pub(crate) fn panel() -> skia_safe::Color4f {
    theme::card_face(0.10)
}

thread_local! {
    static PALETTE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// The palette id `App::apply_ink` last published, for [`ground`].
pub(crate) fn set_current_palette(id: &str) {
    PALETTE.with(|p| {
        let mut p = p.borrow_mut();
        if *p != id {
            *p = id.to_string();
        }
    });
}

fn current_palette() -> String {
    PALETTE.with(|p| p.borrow().clone())
}

/// A Skia rect as the app's integer one, for the hit tests.
pub(crate) fn ui_rect(r: Rect) -> ui::render::Rect {
    ui::render::Rect::new(
        r.left.round() as i32,
        r.top.round() as i32,
        r.width().round().max(0.0) as u32,
        r.height().round().max(0.0) as u32,
    )
}

/// One eased focus channel per item on the kit row's own curve (`approach`, τ 60 ms):
/// toward 1 on the focused item, toward 0 on the rest. Hover is focus, so the pointer
/// rides the same ease as the D-pad.
#[derive(Default)]
pub(crate) struct FocusEase {
    f: Vec<f64>,
    target: Option<usize>,
}

impl FocusEase {
    /// Advances one frame. A changed item count snaps: the items share no history.
    pub(crate) fn step(&mut self, len: usize, focused: Option<usize>, dt: f64) {
        if self.f.len() != len {
            self.f = (0..len).map(|i| f64::from(Some(i) == focused)).collect();
        }
        self.target = focused;
        for (i, f) in self.f.iter_mut().enumerate() {
            let target = f64::from(Some(i) == focused);
            *f = approach(*f, target, dt, 0.06);
            if (*f - target).abs() < 0.002 {
                *f = target;
            }
        }
    }

    pub(crate) fn at(&self, i: usize) -> f32 {
        self.f.get(i).copied().unwrap_or(0.0) as f32
    }

    /// Frames still owed: a channel short of its target.
    pub(crate) fn animating(&self) -> bool {
        self.f
            .iter()
            .enumerate()
            .any(|(i, f)| *f != f64::from(Some(i) == self.target))
    }
}

/// The kit row's focus face at `f` ∈ 0..=1 — tint and stroke rise with `f`, the specular
/// past half — so a settings tab and a sidebar row read like a focused field. `rest` is
/// the item's idle face (the open page, the selected host), fading as focus arrives.
pub(crate) fn focus_face(c: &Canvas, r: Rect, corner: f32, f: f32, rest: bool, k: f32) {
    if rest && f < 0.99 {
        let idle = 1.0 - f;
        theme::panel(
            c,
            r,
            corner,
            Some(theme::accent(0.14 * idle)),
            PanelStroke::Plain(0.10 * idle),
            k,
        );
    }
    if f > 0.01 {
        theme::panel(
            c,
            r,
            corner,
            Some(theme::accent(0.30 * f)),
            PanelStroke::Plain(0.28 * f),
            k,
        );
    }
    if f > 0.5 {
        theme::panel_highlight(c, r, corner, k);
    }
}

/// Paints scaled 0.98 → 1.0 about `r`'s centre by `f`: the kit row's arrival.
pub(crate) fn with_pop(c: &Canvas, r: Rect, f: f32, paint: impl FnOnce(&Canvas)) {
    let s = 0.98 + 0.02 * f;
    c.save();
    c.translate((r.center_x(), r.center_y()));
    c.scale((s, s));
    c.translate((-r.center_x(), -r.center_y()));
    paint(c);
    c.restore();
}

impl App {
    /// The ported modal layer, drawn after the tiles: the open card at its open alpha and
    /// rise, and the card being left at its closing alpha — the same cross-fade
    /// `render::compose` plays for tiles, from state instead of from a snapshot. `dt` is
    /// the frame's step, for the kit widgets' own motion.
    pub(crate) fn draw_modals(&mut self, f: &Frame<'_>, dt: f64) {
        let screen = self.nav.screen;
        let m = if matches!(screen, Screen::Home) {
            0.0
        } else {
            self.render.modal.fade.open_alpha()
        };
        if let Some((alpha, left)) = self.render.modal.fade.closing_frame_against(m) {
            if ported(left) {
                self.draw_modal_screen(f, left, alpha, false, dt);
            }
        }
        if ported(screen) && m > 0.0 {
            self.draw_modal_screen(f, screen, m, true, dt);
        }
    }

    /// One ported card. `live` is whether it is the screen the cursor is on: a card on its
    /// way out keeps its last focus but takes no pop and no press.
    fn draw_modal_screen(&mut self, f: &Frame<'_>, screen: Screen, alpha: f32, live: bool, dt: f64) {
        // Over live video the card is all there is; over the menu it sits on a dim.
        if !crate::app::screens::over_video(screen) {
            scrim(f.canvas, f.w, f.h, alpha);
        }
        let dy = ui::animation::modal_rise(alpha) as f32;
        let focus = self.nav.cursor(crate::app::nav::ScreenKey::of(screen));
        if matches!(
            screen,
            Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::RenameProfile
        ) {
            let Some(copy) = self.text_form() else {
                return;
            };
            let l = form::layout(
                f.fonts,
                f.w,
                f.h,
                f.k,
                &copy.subtitle,
                copy.hint.is_some(),
                self.keyboard_shown,
            );
            let hover_close = live && self.render.hover_close;
            form::draw(f, &l, copy.title, copy.typed, copy.hint, hover_close, alpha, dy);
            return;
        }
        if screen == Screen::Pairing {
            let l = form::pair_layout(f.fonts, f.w, f.h, f.k, self.screens.pairing_status.is_some());
            let state = form::PairState {
                digits: self.screens.pin_digits,
                digit_index: self.screens.pin_digit_index,
                focus: self.screens.pairing_focus,
                status: self.screens.pairing_status.as_deref(),
                busy: self.screens.pairing_busy,
            };
            let hover_close = live && self.render.hover_close;
            form::draw_pair(f, &l, &state, hover_close, alpha, dy);
            return;
        }
        if screen == Screen::About {
            let l = about::layout(f.w, f.h, f.k);
            self.ensure_about_wrapped(l.body.width(), f.k);
            let hover_close = live && self.render.hover_close;
            let lines = self
                .render
                .about_wrapped
                .as_ref()
                .map_or(&[][..], |(_, v)| v.as_slice());
            about::draw(
                f,
                &l,
                crate::app::view::about::TITLE,
                &crate::app::view::about::subtitle(),
                lines,
                self.screens.about_scroll,
                hover_close,
                alpha,
                dy,
            );
            return;
        }
        if screen == Screen::SettingsPage {
            let rows = self.settings_page_specs();
            let l = settings::layout(f.w, f.h, f.k);
            let hover_close = live && self.render.hover_close;
            let (page, column) = (self.screens.settings_page.page, self.screens.settings_page.column);
            self.kit_list(screen).cursor = focus;
            let render = &mut self.render;
            let list = &mut render.list.as_mut().expect("kit_list seats it").1;
            let tabs = &mut render.tab_focus;
            settings::draw(f, list, tabs, &l, page, column, &rows, hover_close, alpha, dy, dt, live);
            return;
        }
        if is_list(screen) {
            let Some(card) = self.list_card(screen) else {
                return;
            };
            let l = self.list_layout(screen, &card, f.w, f.h, f.k);
            let hover_close = live && self.render.hover_close;
            let list = self.kit_list(screen);
            // The App's cursor is the one the handlers read; the widget's follows it.
            list.cursor = focus;
            list::draw(f, list, &l, &card.title, &card.rows, hover_close, alpha, dy, dt, live);
            return;
        }
        let Some(title) = dialog::title_of(screen) else {
            return;
        };
        let Some(confirm) = self.confirm_for(screen) else {
            if let Some((title, body, tone)) = self.message_card(screen) {
                let hover_close = live && self.render.hover_close;
                dialog::draw_message(f, title, &body, tone, hover_close, alpha, dy);
            }
            return;
        };
        let motion = live.then_some(dialog::Motion {
            focus_anim: self.render.modal.focus_anim,
            press: self.press_dip(screen),
            hover_close: self.render.hover_close,
        });
        dialog::draw(f, title, &confirm, focus, motion.as_ref(), alpha, dy);
    }

    /// The kit list for `screen`, made fresh when the screen changes so a new card enters
    /// with its own rise and no scroll carried over from the last one.
    pub(crate) fn kit_list(&mut self, screen: Screen) -> &mut pf_console_ui::widgets::MenuList {
        let cursor = self.nav.cursor(crate::app::nav::ScreenKey::of(screen));
        let fresh = !matches!(&self.render.list, Some((s, _)) if *s == screen);
        if fresh {
            let mut list = pf_console_ui::widgets::MenuList::new();
            list.jump_to(cursor);
            self.render.list = Some((screen, list));
        }
        &mut self.render.list.as_mut().expect("just set").1
    }

    /// The card a ported list screen shows: its title, subtitle and rows in the kit's
    /// vocabulary. `None` on any other screen.
    pub(crate) fn list_card(&self, screen: Screen) -> Option<ListCard> {
        use crate::app::view;
        Some(match screen {
            Screen::HostMenu => ListCard {
                title: self.host_menu_title(),
                subtitle: Some(self.host_menu_subtitle()),
                rows: self.host_menu_rows().iter().map(list::row_spec).collect(),
            },
            Screen::HostPower => {
                let (auto_send, exit_action, access) = self.host_power_view();
                ListCard {
                    title: view::hostpower::title(self.host_menu_host_name().unwrap_or_default()),
                    subtitle: Some(view::hostpower::SUBTITLE.to_string()),
                    rows: view::hostpower::rows(auto_send, exit_action, access)
                        .iter()
                        .map(list::row_spec)
                        .collect(),
                }
            }
            Screen::Collections => {
                let host = self.selected_known_host()?;
                let cursor = self.nav.cursor(crate::app::nav::ScreenKey::Collections);
                let dragging = self.screens.collections.dragging;
                let lit = self.screens.row_button;
                let mut rows: Vec<_> = self.collections_rows()?.iter().map(list::row_spec).collect();
                // Every collection row can be picked up and carries rename (and remove, off
                // the dynamic Library row); the add row is a plain action.
                for (i, (spec, c)) in rows.iter_mut().zip(host.collections()).enumerate() {
                    let on_row = i == cursor;
                    let held = dragging == Some(i)
                        || (on_row && lit == Some(crate::app::screens::rowbuttons::RowButton::Leading));
                    *spec = std::mem::take(spec).with_handle(held).with_buttons(
                        view::collections::trailing_marks(c.dynamic),
                        on_row.then(|| lit?.trailing()).flatten(),
                    );
                    spec.icon = None;
                }
                ListCard {
                    title: view::collections::heading(self.collections_target_held()).to_string(),
                    subtitle: Some(self.collections_heading().to_string()),
                    rows,
                }
            }
            Screen::PickProfile => ListCard {
                title: self.profile_pick()?.title().to_string(),
                subtitle: self.pick_profile_subtitle(),
                rows: self.pick_profile_rows().iter().map(list::row_spec).collect(),
            },
            Screen::HdrCalibration => {
                let hdr = self.screens.hdr.as_ref()?;
                let stalled = hdr
                    .playback
                    .as_ref()
                    .is_none_or(crate::app::state::hdrcalibration::Playback::stalled);
                let lit = self
                    .screens
                    .row_button
                    .and_then(crate::app::screens::rowbuttons::RowButton::trailing);
                let row = pf_console_ui::widgets::RowSpec::slider(
                    "",
                    hdr.step.value_text(hdr.display),
                    hdr.step.fraction(hdr.display),
                )
                .with_buttons(view::hdrcalibration::ACTION_MARKS, lit);
                ListCard {
                    title: hdr.step.label().to_string(),
                    subtitle: Some(view::hdrcalibration::subtitle(hdr.step, stalled).to_string()),
                    rows: vec![row],
                }
            }
            _ => return None,
        })
    }

    /// A ported list card's geometry. The HDR card sits at the bottom, under the pattern.
    fn list_layout(&self, screen: Screen, card: &ListCard, fw: f32, fh: f32, k: f32) -> list::Layout {
        let headers = card.rows.iter().filter(|r| r.header.is_some()).count();
        let l = list::layout(
            &self.fonts,
            fw,
            fh,
            k,
            card.subtitle.as_deref(),
            card.rows.len(),
            headers,
        );
        if screen == Screen::HdrCalibration {
            l.at_bottom(fh, fh * crate::app::view::hdrcalibration::BOTTOM_MARGIN_FRAC)
        } else {
            l
        }
    }

    /// The trailing button of the ported list under `(x, y)`, as `(row, button)`.
    pub(crate) fn kit_list_button_at(&mut self, x: i32, y: i32) -> Option<(usize, usize)> {
        let screen = self.nav.screen;
        if !is_list(screen) {
            return None;
        }
        let p = pf_console_ui::pointer::Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind: pf_console_ui::pointer::PointerKind::Move,
        };
        self.kit_list(screen).button_at(p)
    }

    /// The kit widget's side of a menu event on a ported list screen: the recoil at an end,
    /// the confirm dip, the value slip. The App's handler is what the event *means*; this
    /// is only what it looks like.
    pub(crate) fn kit_list_visual(&mut self, ev: crate::core::event::MenuEvent) {
        use crate::core::event::MenuEvent as E;
        use pf_client_core::menu_nav::{MenuDir, MenuEvent as K};
        let screen = self.nav.screen;
        if !is_list(screen) && !(screen == Screen::SettingsPage && !self.screens.settings_page.column) {
            return;
        }
        let len = self.row_count();
        let cursor = self.nav.cursor(crate::app::nav::ScreenKey::of(screen));
        let kit = match ev {
            E::Up => K::Move(MenuDir::Up),
            E::Down => K::Move(MenuDir::Down),
            E::Left => K::Move(MenuDir::Left),
            E::Right => K::Move(MenuDir::Right),
            E::Confirm => K::Confirm,
            E::Back | E::Secondary => return,
        };
        let list = self.kit_list(screen);
        list.cursor = cursor;
        let _ = list.menu(kit, len);
    }

    /// The row of the ported list under `(x, y)` — the kit's own last-drawn geometry.
    pub(crate) fn kit_list_row_at(&mut self, x: i32, y: i32) -> Option<usize> {
        let screen = self.nav.screen;
        if !is_list(screen) && screen != Screen::SettingsPage {
            return None;
        }
        let len = self.row_count();
        let list = self.kit_list(screen);
        let before = list.cursor;
        // A hover only reports a row it *moves* to, so probe from a cursor no row can be.
        list.cursor = usize::MAX;
        let p = pf_console_ui::pointer::Pointer {
            x: f64::from(x),
            y: f64::from(y),
            kind: pf_console_ui::pointer::PointerKind::Move,
        };
        let _ = list.pointer(p, len);
        let hit = (list.cursor != usize::MAX).then_some(list.cursor);
        list.cursor = before;
        hit
    }

    /// The pairing card's geometry on a frame of the given size.
    pub(crate) fn pair_layout(&self, w: u32, h: u32) -> form::PairLayout {
        form::pair_layout(
            &self.fonts,
            w as f32,
            h as f32,
            scale(h),
            self.screens.pairing_status.is_some(),
        )
    }

    /// Where a ported screen's close mark is hit, if one is up.
    pub(crate) fn ported_close_hit(&self, x: i32, y: i32, w: u32, h: u32) -> Option<bool> {
        let screen = self.nav.screen;
        if screen == Screen::SettingsPage {
            return Some(settings::layout(w as f32, h as f32, scale(h)).on_close(x, y));
        }
        if screen == Screen::About {
            return Some(about::layout(w as f32, h as f32, scale(h)).on_close(x, y));
        }
        if screen == Screen::Pairing {
            return Some(self.pair_layout(w, h).on_close(x, y));
        }
        if let Some(copy) = self.text_form() {
            let l = form::layout(
                &self.fonts,
                w as f32,
                h as f32,
                scale(h),
                &copy.subtitle,
                copy.hint.is_some(),
                self.keyboard_shown,
            );
            return Some(l.on_close(x, y));
        }
        if is_list(screen) {
            let card = self.list_card(screen)?;
            let l = self.list_layout(screen, &card, w as f32, h as f32, scale(h));
            return Some(l.on_close(x, y));
        }
        self.dialog_layout(w, h).map(|l| l.on_close(x, y))
    }
}

/// What a ported list screen shows.
pub(crate) struct ListCard {
    pub title: String,
    pub subtitle: Option<String>,
    pub rows: Vec<pf_console_ui::widgets::RowSpec>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_follows_the_consoles_rule() {
        assert!((scale(1080) - 1.35).abs() < 1e-6);
        assert!((scale(2160) - 2.7).abs() < 1e-6);
        assert!((scale(400) - 0.75).abs() < 1e-6);
    }

    /// A focus move owes frames until every channel reaches its target, then none.
    #[test]
    fn the_focus_ease_settles_and_says_so() {
        let mut e = FocusEase::default();
        e.step(3, Some(1), 1.0 / 60.0);
        assert_eq!(e.at(1), 1.0, "a fresh list snaps");
        assert!(!e.animating());
        e.step(3, Some(2), 1.0 / 60.0);
        assert!(e.animating());
        assert!(e.at(2) > 0.0 && e.at(2) < 1.0);
        for _ in 0..120 {
            e.step(3, Some(2), 1.0 / 60.0);
        }
        assert!(!e.animating());
        assert_eq!((e.at(0), e.at(1), e.at(2)), (0.0, 0.0, 1.0));
    }

    #[test]
    fn wrap_breaks_on_words_and_never_loses_one() {
        let fonts = theme::build_fonts().unwrap();
        let text = "Alpha will be removed from this TV. You can pair with it again later.";
        let lines = wrap(&fonts, text, W::Regular, 16.0, 220.0);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(lines.join(" "), text);
        for l in &lines {
            assert!(
                f64::from(fonts.measure(l, W::Regular, 16.0)) <= 220.0 || !l.contains(' '),
                "{l}"
            );
        }
        assert_eq!(wrap(&fonts, "", W::Regular, 16.0, 100.0), vec![String::new()]);
    }
}
