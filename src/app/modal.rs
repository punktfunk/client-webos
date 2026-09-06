//! [`ModalState`] — what exists only while a modal is up: its open/close fade, the focused
//! widget's pop clock, and a toggle's slide.
use std::time::Instant;

use crate::core::screen::Screen;
use crate::ui;

pub(crate) struct ModalState {
    pub fade: ui::fade::ModalFade<Screen>,
    /// Focus-pop clock for the focused widget (a dialog button).
    pub focus_anim: Option<Instant>,
    /// `(start, from_on, row)` of the toggle row that flipped, for its knob's slide.
    pub switch_anim: Option<(Instant, bool, usize)>,
}

impl Default for ModalState {
    fn default() -> Self {
        Self {
            fade: ui::fade::ModalFade::modal(),
            focus_anim: None,
            switch_anim: None,
        }
    }
}
