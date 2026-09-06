//! Shared scroll bookkeeping for modal content lists (uniform-stride rows or
//! wrapped text lines). Offset clamping, scroll-into-view, and fade logic
//! extracted so any modal can reuse it. Caller-agnostic to rendering/pixels.
//!
//! [`ScrollWindow`] is the row-index form, for lists whose scroll position picks *which
//! rows render at all*; [`scroll_to_reveal`] is the pixel form, for content that scrolls
//! continuously behind a viewport.
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
