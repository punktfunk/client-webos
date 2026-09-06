//! A list card on the kit: a glass card with a title, a subtitle, a close mark, and the rows
//! in the console's `MenuList` — which owns the cursor's ease, the scroll and the entrance.
//!
//! Screens keep describing their rows as `FocusRow`s; [`row_spec`] is the one translation
//! into the kit's vocabulary, so a toggle is a switch and a dropdown a stepped pick on every
//! screen alike. [`layout`] is what the pointer hit tests measure; the rows themselves are
//! hit through the list's own last-drawn geometry.

use pf_console_ui::icons::{by_name, draw_icon};
use pf_console_ui::theme::{self, Fonts, W};
use pf_console_ui::widgets::{MenuList, RowSpec, ROW_H};
use skia_safe::{Contains, Point, Rect};

use super::{glass_card, line_h, wrap, Frame};
use crate::ui::widgets::{FocusRow, RowKind};

/// Card width as a share of the screen; the rows centre inside it at the kit's own width.
const WIDTH_FRAC: f32 = 0.42;
/// Design units.
const PAD: f32 = 24.0;
const CORNER: f32 = 16.0;
const TITLE_SIZE: f64 = 22.0;
const SUB_SIZE: f64 = 14.5;
const TITLE_GAP: f32 = 6.0;
const ROWS_GAP: f32 = 18.0;
const CLOSE_BOX: f32 = 30.0;
const ICON_BOX: f32 = 18.0;
/// The kit's own row pitch, restated: `ROW_H` plus its gap, and a header's band.
const ROW_GAP: f32 = 6.0;
const HEADER_H: f32 = 34.0;
/// The card keeps this much of the screen clear above and below, so a long list scrolls.
const MARGIN: f32 = 40.0;

pub(crate) struct Layout {
    pub card: Rect,
    pub close: Rect,
    /// Where the `MenuList` draws.
    pub rows: Rect,
    pub title_baseline: f32,
    pub sub_top: f32,
    pub sub: Vec<String>,
}

impl Layout {
    /// The same card moved to sit `margin` above the frame's bottom edge: the HDR
    /// calibration card, which must leave the test pattern above it in view.
    pub fn at_bottom(mut self, fh: f32, margin: f32) -> Self {
        let dy = (fh - margin - self.card.bottom).max(-self.card.top);
        self.card.offset((0.0, dy));
        self.close.offset((0.0, dy));
        self.rows.offset((0.0, dy));
        self.title_baseline += dy;
        self.sub_top += dy;
        self
    }

    pub fn on_close(&self, x: i32, y: i32) -> bool {
        self.close.contains(Point::new(x as f32, y as f32))
    }
}

/// The card for `rows` rows (`headers` of them carrying a group header) under a title and an
/// optional subtitle, centred on a `fw`×`fh` frame.
pub(crate) fn layout(
    fonts: &Fonts,
    fw: f32,
    fh: f32,
    k: f32,
    subtitle: Option<&str>,
    rows: usize,
    headers: usize,
) -> Layout {
    let w = (fw * WIDTH_FRAC).round();
    let inner_w = w - 2.0 * PAD * k;
    let title_h = line_h(TITLE_SIZE * f64::from(k)) as f32;
    let sub = subtitle.map_or_else(Vec::new, |s| {
        wrap(fonts, s, W::Regular, SUB_SIZE * f64::from(k), f64::from(inner_w))
    });
    let sub_line = line_h(SUB_SIZE * f64::from(k)) as f32;
    let sub_h = if sub.is_empty() {
        0.0
    } else {
        sub_line * sub.len() as f32 + TITLE_GAP * k
    };
    let rows_h_full = (rows as f32 * (ROW_H as f32 + ROW_GAP) - ROW_GAP).max(0.0) * k + headers as f32 * HEADER_H * k;
    let head_h = PAD * k + title_h + sub_h + ROWS_GAP * k;
    let rows_h = rows_h_full
        .min(fh - 2.0 * MARGIN * k - head_h - PAD * k)
        .max(ROW_H as f32 * k);
    let h = head_h + rows_h + PAD * k;
    let card = Rect::from_xywh(((fw - w) / 2.0).round(), ((fh - h) / 2.0).round(), w, h.round());
    let title_baseline = card.top + PAD * k + title_h * 0.8;
    let sub_top = card.top + PAD * k + title_h + TITLE_GAP * k;
    // The list gets the card's full width: it centres its rows at the kit's `ROW_MAX_W`.
    let rows_rect = Rect::from_xywh(card.left, card.top + head_h, w, rows_h);
    let close = Rect::from_xywh(
        card.right - (PAD * 0.6 + CLOSE_BOX) * k,
        card.top + PAD * 0.6 * k,
        CLOSE_BOX * k,
        CLOSE_BOX * k,
    );
    Layout {
        card,
        close,
        rows: rows_rect,
        title_baseline,
        sub_top,
        sub,
    }
}

/// Draw the card and its rows at `alpha`, risen by `dy`. `dt` is the frame's step for the
/// list's own motion; `active` is false for a card on its way out.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw(
    f: &Frame<'_>,
    list: &mut MenuList,
    l: &Layout,
    title: &str,
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
    let sub_line = line_h(SUB_SIZE * f64::from(k)) as f32;
    for (i, line) in l.sub.iter().enumerate() {
        f.fonts.draw(
            c,
            line,
            f64::from(l.card.left + PAD * k),
            f64::from(l.sub_top + sub_line * (i as f32 + 0.8)),
            W::Regular,
            SUB_SIZE * f64::from(k),
            theme::fg(0.6),
        );
    }
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
    list.render(c, l.rows, rows, f.fonts, f64::from(k), dt, active);
    c.restore();
    c.restore();
}

/// One of this app's rows in the kit's vocabulary.
pub(crate) fn row_spec(row: &FocusRow) -> RowSpec {
    let mut spec = match row.kind {
        RowKind::Toggle => RowSpec::toggle(row.label.clone(), row.value == "On"),
        RowKind::Dropdown => RowSpec::choice(row.label.clone(), row.value.clone()),
        RowKind::Action if row.value.is_empty() => RowSpec::action(row.label.clone(), true),
        // An action with a hint keeps its label at the leading edge, the hint dim on the right.
        RowKind::Action => RowSpec {
            label: row.label.clone(),
            value: Some(row.value.clone()),
            value_dim: true,
            ..RowSpec::default()
        },
    };
    if row.locked {
        spec.enabled = false;
        spec.adjustable = false;
    }
    spec.danger = row.danger;
    spec.dot = row.marked;
    spec.icon = Some(row.icon);
    spec.note = row.subtext.as_ref().map(|s| s.text.clone());
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::view::icons;
    use pf_console_ui::widgets::Control;

    #[test]
    fn rows_translate_into_the_kits_controls() {
        let toggle = row_spec(&FocusRow::toggle(icons::ICON_POWER, "Wake automatically", true));
        assert_eq!(toggle.control, Control::Toggle(true));
        assert_eq!(toggle.icon, Some("power"));
        let pick = row_spec(&FocusRow::dropdown(icons::ICON_POWER, "App exit behaviour", "Sleep").locked(true));
        assert_eq!(pick.control, Control::Value);
        assert!(!pick.enabled && !pick.adjustable);
        assert_eq!(pick.value.as_deref(), Some("Sleep"));
        let forget = row_spec(&FocusRow::action(icons::ICON_DELETE, "Forget host").danger());
        assert!(forget.danger && forget.value.is_none());
        assert_eq!(forget.icon, Some("trash-2"));
    }

    /// The host-power card on a raster frame: rows land inside the card, the switch's knob
    /// is drawn, and `PF_WEBOS_DUMP` writes the PNG.
    #[test]
    fn host_power_card_renders_its_rows() {
        use crate::app::menu::PowerAccess;
        use crate::app::view::hostpower;
        use crate::services::store::ExitAction;
        use skia_safe::{AlphaType, Color4f, ColorType, ImageInfo};
        theme::set_ink(theme::Ink::of(pf_console_ui::library::palette("violet")));
        let fonts = theme::build_fonts().unwrap();
        let (w, h) = (1920u32, 1080u32);
        let k = super::super::scale(h);
        let rows: Vec<RowSpec> = hostpower::rows(
            true,
            ExitAction::Sleep,
            PowerAccess::Rights(crate::services::power::PowerRights {
                sleep: true,
                shutdown: true,
            }),
        )
        .iter()
        .map(row_spec)
        .collect();
        let l = layout(&fonts, w as f32, h as f32, k, Some(hostpower::SUBTITLE), rows.len(), 0);
        let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
        let mut list = MenuList::new();
        for _ in 0..90 {
            surface.canvas().clear(Color4f::new(0.075, 0.063, 0.16, 1.0));
            let f = Frame::new(surface.canvas(), &fonts, w, h);
            draw(
                &f,
                &mut list,
                &l,
                &hostpower::title("Gaming PC"),
                &rows,
                false,
                1.0,
                0.0,
                1.0 / 60.0,
                true,
            );
        }
        if let Ok(dir) = std::env::var("PF_WEBOS_DUMP") {
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/list-host-power.png"), png.as_bytes()).unwrap();
        }
        let info = ImageInfo::new((w as i32, h as i32), ColorType::RGBA8888, AlphaType::Unpremul, None);
        let mut out = vec![0u8; (w * h * 4) as usize];
        assert!(surface.read_pixels(&info, &mut out, (w * 4) as usize, (0, 0)));
        let px = |x: f32, y: f32| {
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            [out[i], out[i + 1], out[i + 2]]
        };
        // The card is lighter than the ground; the switch's knob (on → right end) is white.
        assert!(px(l.card.left + 10.0, l.card.center_y())[0] > px(10.0, 10.0)[0]);
        let row0_cy = l.rows.top + (ROW_H as f32 / 2.0) * k;
        // The kit narrows its rows to the rect: `min(ROW_MAX_W, width − 48)` design units.
        let row_w = (620.0 * k).min(l.rows.width() - 48.0 * k);
        let row_right = l.rows.center_x() + row_w / 2.0;
        let knob = px(row_right - 16.0 * k - 10.0 * k, row0_cy);
        assert!(
            knob.iter().all(|c| *c > 200),
            "knob at the right of an on switch: {knob:?}"
        );
        assert!(l.rows.bottom <= l.card.bottom && l.rows.top >= l.card.top);
    }

    #[test]
    fn a_long_list_caps_at_the_screen_and_a_short_one_hugs_its_rows() {
        let fonts = theme::build_fonts().unwrap();
        let k = super::super::scale(1080);
        let short = layout(&fonts, 1920.0, 1080.0, k, Some("192.168.1.9:47989 · paired"), 3, 0);
        let long = layout(&fonts, 1920.0, 1080.0, k, None, 40, 0);
        assert!(short.card.height() < long.card.height());
        assert!(long.card.top >= MARGIN * k - 1.0 && long.card.bottom <= 1080.0 - MARGIN * k + 1.0);
        assert!(short.rows.top > short.sub_top);
        assert!(short.on_close(short.close.center_x() as i32, short.close.center_y() as i32));
    }
}
