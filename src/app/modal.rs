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
    /// This frame's fade facts, latched once by `App::advance_frame`. The fades are read off
    /// `Instant::elapsed`, so sampling them per caller let `modal_visible` (asked before the
    /// page snapshot) and `draw_modals` (a whole `draw_home` later) land on opposite sides of
    /// a fade's end: the page was dropped for a card that then drew, or kept for one that did
    /// not. Everything derived from a fade comes from the one sample.
    pub frames: ModalFrames,
}

/// One frame's view of the modal fades.
#[derive(Clone, Copy, Default)]
pub(crate) struct ModalFrames {
    /// The open card's alpha; `0.0` on Home.
    pub open: f32,
    /// The card being left, at its closing alpha. Already filtered to the ported screens.
    pub leaving: Option<(f32, Screen)>,
    /// Whether either fade is still in flight.
    pub fading: bool,
}

impl Default for ModalState {
    fn default() -> Self {
        Self {
            fade: ui::fade::ModalFade::modal(),
            focus_anim: None,
            switch_anim: None,
            frames: ModalFrames::default(),
        }
    }
}
