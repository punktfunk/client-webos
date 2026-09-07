//! Scroll-into-view for content that scrolls continuously behind a viewport.
//! Caller-agnostic to rendering/pixels.
use crate::ui::render::Rect;

/// The pixel scroll offset that brings `target` (in unscrolled content space) fully
/// inside the vertical band `viewport`, starting from `current` and moving as little as
/// possible. `margin` is slack for whatever the focus treatment draws outside the rect
/// itself. Unclamped — the caller knows its own content extent.
pub fn scroll_to_reveal(target: Rect, viewport: (i32, i32), current: i32, margin: i32) -> i32 {
    let (top, bottom) = viewport;
    let above = target.y() - margin;
    let below = target.bottom() + margin;
    if above - current < top {
        above - top
    } else if below - current > bottom {
        below - bottom
    } else {
        current
    }
}
