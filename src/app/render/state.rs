//! What the menu is drawing with: the tile-backed animation state, the scroll windows, and the
//! dirty flags that decide what gets re-rastered.
//!
//! Split off `App` so a `&mut self.render` pass can run beside a `&self` read of the library or
//! the host list. Nothing here is domain state — it is all recoverable from a redraw.

use std::time::Instant;

use crate::app::{grid, hero, modal};
use crate::ui;

#[derive(Default)]
pub(crate) struct RenderState {
    /// The connecting screen's backdrop, and every clock it runs on.
    pub(crate) hero: hero::Hero,
    /// The About document's source lines, built once on first open. ~10,000 static string
    /// slices; cheap to hold, wasteful to rebuild per frame.
    pub(crate) about_lines: Vec<&'static str>,
    /// `about_lines` wrapped to a body width, flattened into one list of visual lines (see
    /// `draw::about::wrap_document`) — the unit About scrolls over,
    /// since a source line's wrapped length varies and only the flattened list has a uniform
    /// per-unit stride. Keyed by the body width it was wrapped for, rebuilt if that width
    /// changes.
    pub(crate) about_wrapped: Option<(u32, Vec<String>)>,
    /// Whether the Magic Remote's pointer is currently hovering a modal's close (X) button.
    pub(crate) hover_close: bool,
    /// The grid's cover images by game id (see `app::draw::home`).
    pub(crate) covers: crate::app::draw::home::Covers,
    /// The launch backdrop as a Skia image, built when `hero` says its art is in hand.
    pub(crate) hero_image: Option<skia_safe::Image>,
    pub(crate) modal: modal::ModalState,
    pub(crate) grid: grid::GridState,
    pub(crate) focus_anim: Option<Instant>,
    pub(crate) press: ui::animation::Press,
    /// The kit list widget of the open ported list screen (`app::draw::list`), with the
    /// screen it was made for — a different screen gets a fresh one.
    pub(crate) list: Option<(crate::core::screen::Screen, pf_console_ui::widgets::MenuList)>,
    /// The sidebar rows' and the settings tabs' eased focus (`app::draw::FocusEase`).
    pub(crate) sidebar_focus: crate::app::draw::FocusEase,
    pub(crate) tab_focus: crate::app::draw::FocusEase,
}
