//! The Home screen's host-list sidebar: brand lockup, host rows, "+ Add host", and the
//! bottom-pinned Settings row, plus the hit tests that go with them. Built from the
//! nav-row widgets in `ui::widgets`.
use crate::ui;
use crate::ui::render::Rect;

/// Where the host rows start: under the mark — pad 24, mark 64, gap 36.
pub const TOP_Y: i32 = 124;

/// Every nav position's rect, in order: the host rows and "+ Add host" stacked from
/// [`TOP_Y`], then "Settings" pinned to the bottom of the panel — a spacer slot between
/// them is what "pinned" means, so it stays put regardless of how many hosts are known.
///
/// One split, read by the painter, hit tests and `home_focus_map` alike.
pub fn nav_rows(row_count: usize, screen_h: u32) -> Vec<Rect> {
    let column = Rect::new(
        ui::widgets::SIDEBAR_PAD,
        TOP_Y,
        ui::widgets::SIDEBAR_W - 2 * ui::widgets::SIDEBAR_PAD as u32,
        (screen_h as i32 - ui::widgets::SIDEBAR_PAD - TOP_Y).max(0) as u32,
    );
    let row_h = ui::widgets::SIDEBAR_ROW_H;
    let stride = row_h as i32 + ui::widgets::SIDEBAR_ROW_GAP;
    let mut rects: Vec<Rect> = (0..row_count.saturating_sub(1))
        .map(|i| Rect::new(column.x(), column.y() + i as i32 * stride, column.width(), row_h))
        .collect();
    if row_count > 0 {
        // The last position (Settings) is pinned to the panel's bottom.
        rects.push(Rect::new(
            column.x(),
            column.bottom() - row_h as i32,
            column.width(),
            row_h,
        ));
    }
    rects
}

/// The part of a nav row that is the row *itself* rather than its ⋯ button. Focus
/// navigation needs the two as disjoint targets (`ui::focus` never treats overlapping
/// rects as candidates for each other), so where the body ends is layout's business, not
/// navigation's.
pub fn row_body_rect(row: Rect, has_menu: bool) -> Rect {
    if !has_menu {
        return row;
    }
    let btn = ui::widgets::sidebar_menu_button_rect(row);
    Rect::new(row.x(), row.y(), (btn.x() - row.x()).max(0) as u32, row.height())
}

/// Whether `(x, y)` is on host row `index`'s ⋯ button. Checked *before*
/// [`hit_test_row`] by the click handler, since the button sits inside the row
/// it belongs to and would otherwise just read as a click on the row.
pub fn hit_test_menu_button(
    x: i32,
    y: i32,
    entries: &[crate::app::hosts::HostEntry],
    row_count: usize,
    screen_h: u32,
) -> Option<usize> {
    nav_rows(row_count, screen_h)
        .into_iter()
        .take(entries.len())
        .enumerate()
        .find(|(i, row)| entries[*i].has_menu() && ui::widgets::sidebar_menu_button_rect(*row).contains_point((x, y)))
        .map(|(i, _)| i)
}

/// `None` when `(x, y)` falls outside the sidebar's horizontal band at all — lets
/// mouse-motion handling distinguish "not hovering the sidebar" from "hovering the
/// sidebar but between rows." The last nav position (`row_count - 1`, "Settings")
/// is pinned to the bottom of the panel (see [`settings_row_rect`]) rather than
/// following on from the sequential rows above it.
pub fn hit_test_row(x: i32, y: i32, row_count: usize, screen_h: u32) -> Option<usize> {
    if x < 0 || x as u32 > ui::widgets::SIDEBAR_W || row_count == 0 {
        return None;
    }
    nav_rows(row_count, screen_h)
        .into_iter()
        .position(|row| row.contains_point((x, y)))
}
