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

/// Fade strengths, 0..=1, for the top and bottom edge of a viewport whose content is
/// scrolled to centre `focused_center`. `fade` is the ramp length; every length is in one
/// unit of the caller's choosing. Zero means that edge has nothing behind it.
pub fn edge_fades(content_h: f32, view_h: f32, focused_center: f32, fade: f32) -> (f32, f32) {
    let hidden = content_h - view_h;
    if hidden <= 0.0 {
        return (0.0, 0.0);
    }
    let scroll = (focused_center - view_h / 2.0).clamp(0.0, hidden);
    let ramp = |len: f32| (len / fade).clamp(0.0, 1.0);
    (ramp(scroll), ramp(hidden - scroll))
}
