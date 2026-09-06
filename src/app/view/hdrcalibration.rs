//! HDR calibration: the card, its two rows, and the test patterns the sliders drive.
//!
//! Unlike every other list modal this one draws over live video — the patterns play on the NDL
//! plane underneath — so the card is pinned to the bottom of the screen and kept short, clear of
//! the windows, which sit above centre (see `WINDOW_CENTER_Y`). Logic lives in `app::state::hdrcalibration`.
use crate::app::state::hdrcalibration::HdrStep;
use crate::core::model::{self, HdrDisplay};
use crate::core::pq;
use crate::platform::webos::hdr_pattern::Pattern;
use crate::services::hevc::Patch;

/// The "Clear HDR calibration?" dialog — the Calibrate row's delete button, the same one a
/// collection row carries.
pub const RESET_TITLE: &str = "Clear HDR calibration?";
pub const RESET_SUBTITLE: &str =
    "The panel volume goes back to the shipped default, and the host is told that instead.";

/// One row: the slider, carrying the step's measurement. Advancing is a button on it rather than
/// a row of its own — the card sits over the pattern being judged, and every row of height it
/// takes is picture the eye has to work around.
pub const ROW_COUNT: usize = 1;
pub const ROW_SLIDER: usize = 0;

/// Gap between the card's bottom edge and the bottom of the screen, as a fraction of height.
pub(crate) const BOTTOM_MARGIN_FRAC: f32 = 0.04;

/// Where the mosaic centres vertically, as a fraction of picture height — above centre, clear of
/// the card pinned to the bottom of the screen.
const WINDOW_CENTER_Y: f32 = 0.36;

/// Checkerboard tiles per side. Enough that the texture is
/// unmistakable across the room, few enough that each tile is a large flat area rather than a
/// detail the panel's own processing might soften.
const MOSAIC_TILES: usize = 6;

/// Where the dim half sits, as a fraction of the declared volume's ceiling. Far enough below it to
/// be plainly visible while the TV renders the volume as it is, close enough that a tone map's
/// shoulder takes both together.
const SHOULDER_RATIO: f32 = 0.9;

/// Window sizes, as a fraction of the picture.
///
/// Peak is measured on a small window: ABL is what limits a self-emissive panel, so the smaller
/// the window the closer the reading gets to the panel's real ceiling, and peak is the one
/// number that should not be dragged down by the rest of the screen. The floor fills the screen —
/// a near-black floor is easiest to judge with nothing else lit.
const PEAK_WINDOW_AREA: f32 = 0.024;
const FULL_SCREEN_AREA: f32 = 1.0;

/// The subtitle carries the instruction, so it changes per step and the card is re-measured
/// with it — see [`card_rect`].
#[must_use]
pub fn subtitle(step: HdrStep, stalled: bool) -> &'static str {
    if stalled {
        // The sliders and their stored values still work, so the screen stays open and says why
        // there is nothing to look at rather than pretending the panel measured as black.
        return "The video plane did not accept the test pattern — the values below are unchanged.";
    }
    match step {
        HdrStep::Peak => "Step 1 of 3. Raise until the edge between the two squares disappears, then Next.",
        HdrStep::FrameAverage => "Step 2 of 3. Raise until the checkerboard flattens to one tone, then Next.",
        HdrStep::Black => "Step 3 of 3. Lower until the tiles just disappear into the black, then save.",
    }
}

/// The button that advances a step, and commits on the last one — a trailing button like a
/// collection row's rename and remove, reached and lit the same way (`screens::rowbuttons`).
/// One tick throughout: the subtitle says which step this is, so the button only ever means
/// "this measurement is done".
/// The row's one trailing button: the tick that finishes a step.
pub const ACTION_MARKS: &[&str] = &["check"];

/// The mastering volume to declare while `step` is being measured.
///
/// The floor is declared at its minimum throughout, including while it is itself being measured:
/// a declared black that moved with the slider would change how the TV lifts the bottom of the
/// range at the same time as the patch it is being judged against.
#[must_use]
pub fn pattern_meta(step: HdrStep, display: HdrDisplay) -> punktfunk_core::quic::HdrMeta {
    // Every step declares a volume whose ceiling is the value that step measures, so its pattern
    // sits at the top of the declared range — the only place the flattening can happen. A static
    // tone map is driven by the mastering maximum and `MaxCLL`, never by `MaxFALL`, so a
    // full-field pattern left in the middle of a peak-sized volume is never touched by it.
    let ceiling = match step {
        HdrStep::Peak | HdrStep::Black => display.peak_nits,
        HdrStep::FrameAverage => display.frame_avg_nits,
    };
    HdrDisplay {
        peak_nits: ceiling,
        frame_avg_nits: display.frame_avg_nits.min(ceiling),
        black_code: pq::BLACK_CODE,
    }
    .hdr_meta()
}

/// The luma the video plane should be showing for `step` at `display` — a background field and
/// the patches on it.
///
/// **The pattern is defined against the declared volume, not in absolute nits**, because the
/// slider drives that volume (see `App::refresh_hdr_pattern`). Each bright step shows the volume's
/// own ceiling against [`SHOULDER_RATIO`] of it — the top of the range and just below it. While
/// the TV is not compressing, those two are a plain 10% apart and the boundary between them is
/// plain. Once the declared volume outruns the panel, the TV's tone map pulls the top of the range
/// down onto the panel's ceiling, the shoulder comes with it, and the boundary disappears. The
/// largest volume where it is still there is the volume this panel can actually render, which is
/// the number being measured.
///
/// Expect the overall brightness to barely move as the slider does: the top of the declared
/// volume renders at the panel's ceiling either way. What changes is whether the two tones are
/// still separable — that is the whole reading.
///
/// What differs per step is the area: a small window for peak (where ABL leaves the panel
/// free), and the whole screen for the floor (easiest to judge with nothing else lit).
#[must_use]
pub fn pattern(step: HdrStep, display: HdrDisplay) -> Pattern {
    match step {
        HdrStep::Peak => {
            let nits = f32::from(display.peak_nits);
            Pattern {
                background: pq::BLACK_CODE,
                patches: pair(PEAK_WINDOW_AREA, pq::pq_code(nits), pq::pq_code(nits * SHOULDER_RATIO)),
            }
        }
        // The same judgement over the whole screen, which is what makes it a full-field one: the
        // mosaic is the picture, so ABL sees the average it is being measured on.
        HdrStep::FrameAverage => {
            let nits = f32::from(display.frame_avg_nits);
            Pattern {
                background: pq::BLACK_CODE,
                patches: mosaic(FULL_SCREEN_AREA, pq::pq_code(nits * SHOULDER_RATIO), pq::pq_code(nits)),
            }
        }
        // The floor is the one step a tone map does not stand in the way of: near black the TV
        // has nothing to compress, so the codes go on the plane unconverted and the reading is
        // simply the dimmest tile still separable from black.
        HdrStep::Black => Pattern {
            background: pq::BLACK_CODE,
            patches: mosaic(FULL_SCREEN_AREA, pq::BLACK_CODE, display.black_code),
        },
    }
}

/// Two squares filling a window that covers `area`, edge to edge with no gap between them: `max`
/// on the left, `adjusted` on the right — see [`window_rect`] for where the window sits.
///
/// No gap on purpose. Two levels separated by black read as two objects and the eye compares them
/// one after the other; sharing an edge, the difference between them is a single boundary that
/// either exists or does not, which is a far finer judgement and the one this measurement wants.
fn pair(area: f32, max: u16, adjusted: u16) -> Vec<Patch> {
    let (x, y, side) = window_rect(area);
    let half = side / 2.0;
    vec![
        Patch {
            x,
            y,
            w: half,
            h: side,
            code: max,
        },
        Patch {
            x: x + half,
            y,
            w: half,
            h: side,
            code: adjusted,
        },
    ]
}

/// Where a window covering `area` sits: `(x, y, side)` in fractions of the picture. It keeps the
/// picture's aspect (so `area` is the fraction of the screen it covers, the way window patterns
/// are quoted) and, when it leaves room to, sits above centre — the card is pinned to the bottom
/// of the screen, and a window right on top of it would be judged against the card's own light.
fn window_rect(area: f32) -> (f32, f32, f32) {
    let side = area.clamp(0.0, 1.0).sqrt();
    // Kept whole inside the picture, so a window that fills the screen sits at the origin rather
    // than hanging off the top.
    let y = (WINDOW_CENTER_Y - side / 2.0).clamp(0.0, 1.0 - side);
    ((1.0 - side) / 2.0, y, side)
}

/// A checkerboard of `a` and `b` tiles filling a window that covers `area` of the picture — see
/// [`window_rect`] for where it sits.
fn mosaic(area: f32, a: u16, b: u16) -> Vec<Patch> {
    let (x, y, side) = window_rect(area);
    let (tw, th) = (side / MOSAIC_TILES as f32, side / MOSAIC_TILES as f32);
    // The `a` field first, then every second tile in `b` over it: half the rects of a full
    // checkerboard, and the seams between same-coloured tiles cannot show.
    let mut patches = vec![Patch {
        x,
        y,
        w: side,
        h: side,
        code: a,
    }];
    for row in 0..MOSAIC_TILES {
        for col in 0..MOSAIC_TILES {
            if (row + col) % 2 == 0 {
                continue;
            }
            patches.push(Patch {
                x: x + col as f32 * tw,
                y: y + row as f32 * th,
                w: tw,
                h: th,
                code: b,
            });
        }
    }
    patches
}

impl HdrStep {
    /// Also the card's title: the slider it heads is the only thing on the card, so labelling it
    /// twice would just narrow the track.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Peak => "Peak brightness",
            Self::FrameAverage => "Full-screen brightness",
            Self::Black => "Black level",
        }
    }

    #[must_use]
    pub fn value_text(self, display: HdrDisplay) -> String {
        match self {
            Self::Peak | Self::FrameAverage => format!("{} nits", self.value(display)),
            Self::Black => black_text(display.black_code),
        }
    }

    /// The lattice this step's slider walks.
    #[must_use]
    pub fn lattice(self) -> model::Lattice {
        match self {
            Self::Peak => model::HDR_PEAK,
            Self::FrameAverage => model::HDR_FRAME_AVG,
            Self::Black => model::HDR_BLACK,
        }
    }

    /// The value this step currently reads.
    #[must_use]
    pub fn value(self, display: HdrDisplay) -> u16 {
        match self {
            Self::Peak => display.peak_nits,
            Self::FrameAverage => display.frame_avg_nits,
            Self::Black => display.black_code,
        }
    }

    #[must_use]
    pub fn fraction(self, display: HdrDisplay) -> f32 {
        self.lattice().fraction(u32::from(self.value(display)))
    }
}

/// The floor's luminance, named to as many decimals as it actually has. The bottom stop is the
/// code for no light at all, which no number describes usefully.
fn black_text(code: u16) -> String {
    let nits = pq::pq_nits(code);
    if nits <= 0.0 {
        return "Black".to_string();
    }
    // Two significant figures wherever the floor lands: the bottom of the range is thousandths
    // of a nit and the top is tenths, and a fixed width would print either noise or nothing.
    let decimals = (1 - nits.log10().floor() as i32).clamp(2, 5) as usize;
    format!("{nits:.decimals$} nits")
}
