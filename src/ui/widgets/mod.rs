//! The widgets themselves: cards and posters, the focusable-row list and its controls,
//! modal chrome, list-modal screens, nav rows, toasts.
//!
//! Flat within the group — a caller reaching for one row widget reaches for its neighbours
//! in the same breath, and the file a widget lives in is an implementation detail.

mod rows;
mod sidebar;

pub use rows::*;
pub use sidebar::*;
