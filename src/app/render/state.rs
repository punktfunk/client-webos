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
    /// When the sidebar first drew the brand mark — the start of its one-shot entrance
    /// (`pf_console_ui::brand`). `None` until then.
    pub(crate) mark_shown_at: Option<std::time::Instant>,
    /// The running dot's breath (`app::draw::home::running_dot`).
    pub(crate) running_pulse: RunningPulse,
}

/// The running dot's pulse: its phase, how often it is worth redrawing, and the one value the
/// painter reads.
///
/// Stepped rather than continuous, because reporting `animating` is what keeps the menu loop
/// off `wait_for_event`: a smooth 60 Hz breath would hold this `SoC` at a full grid redraw per
/// frame for as long as a game is up, to move one dot on a near-two-second cycle. Eighteen
/// steps is ~10 Hz, past what anyone resolves in a slow fade and a sixth of the redraws.
///
/// One clock for both jobs, so the value drawn and the frame it is drawn on cannot drift: the
/// painter reads [`Self::breath`], and [`Self::tick`] is the only writer.
#[derive(Default)]
pub(crate) struct RunningPulse {
    /// Phase origin, stamped on the first live tick. `None` while nothing is running, which is
    /// what makes the next breath start at its peak rather than mid-fall.
    since: Option<std::time::Instant>,
    /// The step [`Self::breath`] was last computed for; `None` forces the first one.
    step: Option<u32>,
    /// 1.0 at the top of the breath, 0.0 at the bottom.
    pub(crate) breath: f32,
}

/// One full breath. Slow on purpose — a dot that blinks reads as an alarm, and this is only
/// saying "your host has this up".
const PULSE_SECS: f32 = 1.8;
/// Redraws per breath. See [`RunningPulse`].
const PULSE_STEPS: u32 = 18;

impl RunningPulse {
    /// Advances the breath and reports whether this frame owes a redraw for it. `live` is
    /// whether the dot is on screen at all — a false one parks the clock rather than freezing
    /// it mid-fade.
    pub(crate) fn tick(&mut self, now: std::time::Instant, live: bool) -> bool {
        if !live {
            self.since = None;
            self.step = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        let phase = now.duration_since(since).as_secs_f32() / PULSE_SECS;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let step = (phase.fract() * PULSE_STEPS as f32) as u32;
        if self.step == Some(step) {
            return false;
        }
        self.step = Some(step);
        // Off the step, not off `phase`: the value drawn is then exactly the one this frame was
        // woken for, and two cards in one frame cannot land on different points of the breath.
        let at = step as f32 / PULSE_STEPS as f32;
        self.breath = 0.5 + 0.5 * (at * std::f32::consts::TAU).cos();
        true
    }
}
