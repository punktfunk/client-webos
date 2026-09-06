//! Sidebar panel metrics and the generic nav-row widgets built on them: an icon + label
//! row with optional selection tint, and the ⋯ actions button a row can carry.
//!
//! This app's own host list is `app::view::sidebar`, which composes these.
use crate::ui::render::Rect;

// Sized for a 10-foot TV viewing distance, not a desktop/phone screen.
pub const SIDEBAR_W: u32 = 460;
pub const SIDEBAR_PAD: i32 = 24;
pub const SIDEBAR_ROW_H: u32 = 76;
pub const SIDEBAR_ROW_GAP: i32 = 10;

/// Size of the "more actions" (⋯) hit target at the right end of a host row. Square,
/// and generous — this is a 10-foot UI driven by a wobbly pointer, so the touch target
/// is deliberately much larger than the glyph drawn inside it.
pub const SIDEBAR_MENU_BTN: u32 = 52;
/// A row's ⋯ button. The single-button case of a focus row's trailing buttons — one
/// geometry, so the sidebar's ⋯ and a list row's cannot drift apart.
pub fn sidebar_menu_button_rect(row_rect: Rect) -> Rect {
    Rect::new(
        row_rect.right() - SIDEBAR_MENU_BTN as i32 - 10,
        row_rect.y() + (row_rect.height() as i32 - SIDEBAR_MENU_BTN as i32) / 2,
        SIDEBAR_MENU_BTN,
        SIDEBAR_MENU_BTN,
    )
}
