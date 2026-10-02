//! HDR calibration: three measurements, one per luminance in the CTA-861.3 HDR static-metadata
//! block, made against synthetic PQ patterns played on the real video plane — the only signal
//! path a stream will ever use. Opened from the menu with the remote's Blue key.
//!
//! The edits are held in a scratch [`HdrDisplay`] and only written to the document by the last
//! step's OK. A half-walked calibration is worse than none: it would advertise a volume the user
//! never confirmed.

use pf_console_ui::theme::{self, W};
use skia_safe::{Color4f, RRect, Rect};

use super::overlay::{self, alpha_layer, line_h, opaque_card, wrap, Frame};
use super::*;
use crate::core::event::MenuEvent;
use crate::core::model::{self, HdrDisplay};
use crate::core::pq;
use crate::platform::webos::hdr_pattern::{Pattern, Playback};
use crate::platform::webos::input::{menu_event_for_button, menu_event_for_key, RemoteKey, RemoteKeys};
use crate::services::hevc::Patch;

/// Where the mosaic centres vertically, as a fraction of picture height — above centre, clear of
/// the card pinned to the bottom of the screen.
const WINDOW_CENTER_Y: f32 = 0.36;

/// Checkerboard tiles per side. Enough that the texture is unmistakable across the room, few
/// enough that each tile is a large flat area the panel's own processing cannot soften.
const MOSAIC_TILES: usize = 6;

/// Where the dim half sits, as a fraction of the declared volume's ceiling. Far enough below it to
/// be plainly visible while the TV renders the volume as it is, close enough that a tone map's
/// shoulder takes both together.
const SHOULDER_RATIO: f32 = 0.9;

/// Window sizes, as a fraction of the picture. Peak is measured on a small window, where ABL
/// leaves a self-emissive panel free; the floor fills the screen, easiest to judge with nothing
/// else lit.
const PEAK_WINDOW_AREA: f32 = 0.024;
const FULL_SCREEN_AREA: f32 = 1.0;

/// Design units.
const CARD_WIDTH_FRAC: f32 = 0.5;
const BOTTOM_MARGIN_FRAC: f32 = 0.04;
const PAD: f32 = 22.0;
const CORNER: f32 = 16.0;
const TITLE_SIZE: f64 = 20.0;
const BODY_SIZE: f64 = 14.0;
const HINT_SIZE: f64 = 12.0;
const GAP: f32 = 12.0;
const TRACK_H: f32 = 6.0;
const KNOB_R: f32 = 9.0;

const TICK: Duration = Duration::from_millis(16);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Step {
    #[default]
    Peak,
    FrameAverage,
    Black,
}

impl Step {
    /// The step after this one; `None` on the last, whose OK saves.
    fn next(self) -> Option<Self> {
        match self {
            Self::Peak => Some(Self::FrameAverage),
            Self::FrameAverage => Some(Self::Black),
            Self::Black => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Peak => "Peak brightness",
            Self::FrameAverage => "Full-screen brightness",
            Self::Black => "Black level",
        }
    }

    fn instruction(self) -> &'static str {
        match self {
            Self::Peak => "Step 1 of 3. Raise until the edge between the two squares disappears.",
            Self::FrameAverage => "Step 2 of 3. Raise until the checkerboard flattens to one tone.",
            Self::Black => "Step 3 of 3. Lower until the tiles just disappear into the black.",
        }
    }

    fn lattice(self) -> model::Lattice {
        match self {
            Self::Peak => model::HDR_PEAK,
            Self::FrameAverage => model::HDR_FRAME_AVG,
            Self::Black => model::HDR_BLACK,
        }
    }

    fn value(self, display: HdrDisplay) -> u16 {
        match self {
            Self::Peak => display.peak_nits,
            Self::FrameAverage => display.frame_avg_nits,
            Self::Black => display.black_code,
        }
    }

    fn value_text(self, display: HdrDisplay) -> String {
        match self {
            Self::Peak | Self::FrameAverage => format!("{} nits", self.value(display)),
            Self::Black => black_text(display.black_code),
        }
    }
}

/// The mastering volume to declare while `step` is measured. Its ceiling is the value that step
/// measures, so the pattern sits at the top of the declared range — the only place a static tone
/// map flattens anything. The floor is declared at its minimum throughout: a declared black that
/// moved with the slider would change how the TV lifts the range while it is being judged.
fn pattern_meta(step: Step, display: HdrDisplay) -> punktfunk_core::quic::HdrMeta {
    let ceiling = match step {
        Step::Peak | Step::Black => display.peak_nits,
        Step::FrameAverage => display.frame_avg_nits,
    };
    HdrDisplay {
        peak_nits: ceiling,
        frame_avg_nits: display.frame_avg_nits.min(ceiling),
        black_code: pq::BLACK_CODE,
    }
    .hdr_meta()
}

/// The picture for `step`, defined against the declared volume rather than in absolute nits.
/// Each bright step shows the volume's ceiling against [`SHOULDER_RATIO`] of it: plainly two tones
/// while the TV renders the volume as is, one tone once the volume outruns the panel and the tone
/// map pulls both onto its ceiling. The largest volume that still shows the boundary is the one
/// this panel can render.
fn pattern(step: Step, display: HdrDisplay) -> Pattern {
    let patches = match step {
        Step::Peak => {
            let nits = f32::from(display.peak_nits);
            pair(PEAK_WINDOW_AREA, pq::pq_code(nits), pq::pq_code(nits * SHOULDER_RATIO))
        }
        Step::FrameAverage => {
            let nits = f32::from(display.frame_avg_nits);
            mosaic(FULL_SCREEN_AREA, pq::pq_code(nits * SHOULDER_RATIO), pq::pq_code(nits))
        }
        // Near black the TV has nothing to compress, so the codes reach the panel unconverted.
        Step::Black => mosaic(FULL_SCREEN_AREA, pq::BLACK_CODE, display.black_code),
    };
    Pattern {
        background: pq::BLACK_CODE,
        patches,
    }
}

/// Two squares sharing an edge, `max` left and `adjusted` right. No gap: a shared edge is a single
/// boundary that either exists or does not, a finer judgement than comparing two objects.
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

/// `(x, y, side)` of a window covering `area` of the picture, kept whole inside it and above
/// centre when there is room — clear of the card's own light.
fn window_rect(area: f32) -> (f32, f32, f32) {
    let side = area.clamp(0.0, 1.0).sqrt();
    let y = (WINDOW_CENTER_Y - side / 2.0).clamp(0.0, 1.0 - side);
    ((1.0 - side) / 2.0, y, side)
}

/// A checkerboard of `a` and `b` tiles: the `a` field, then every second tile in `b` over it.
fn mosaic(area: f32, a: u16, b: u16) -> Vec<Patch> {
    let (x, y, side) = window_rect(area);
    let tile = side / MOSAIC_TILES as f32;
    let mut patches = vec![Patch {
        x,
        y,
        w: side,
        h: side,
        code: a,
    }];
    for row in 0..MOSAIC_TILES {
        for col in (0..MOSAIC_TILES).filter(|col| (row + col) % 2 == 1) {
            patches.push(Patch {
                x: x + col as f32 * tile,
                y: y + row as f32 * tile,
                w: tile,
                h: tile,
                code: b,
            });
        }
    }
    patches
}

/// The floor's luminance to two significant figures. The bottom stop is no light at all.
fn black_text(code: u16) -> String {
    let nits = pq::pq_nits(code);
    if nits <= 0.0 {
        return "Black".to_string();
    }
    let decimals = (1 - nits.log10().floor() as i32).clamp(2, 5) as usize;
    format!("{nits:.decimals$} nits")
}

/// What the screen holds while it is up.
struct Calibration {
    step: Step,
    display: HdrDisplay,
    /// `None` once the feed failed to start; the screen stays usable and says why.
    playback: Option<Playback>,
}

impl Calibration {
    /// The one writer of the scratch volume: clamps the stop, applies it to the step's field and
    /// re-feeds the pattern if anything changed.
    fn nudge(&mut self, delta: i32) {
        let lattice = self.step.lattice();
        let stop = lattice.index(u32::from(self.step.value(self.display))) as i32 + delta;
        let value = lattice.value(stop) as u16;
        let before = self.display;
        match self.step {
            Step::Peak => {
                self.display.peak_nits = value;
                // A full field can never out-run a small window.
                self.display.frame_avg_nits = self.display.frame_avg_nits.min(value);
            }
            Step::FrameAverage => self.display.frame_avg_nits = value.min(self.display.peak_nits),
            Step::Black => self.display.black_code = value,
        }
        if self.display != before {
            self.refresh();
        }
    }

    fn refresh(&self) {
        if let Some(playback) = &self.playback {
            playback.show(pattern_meta(self.step, self.display), pattern(self.step, self.display));
        }
    }

    fn presenting(&self) -> bool {
        self.playback.as_ref().is_some_and(Playback::presenting)
    }

    fn stalled(&self) -> bool {
        self.playback.as_ref().is_none_or(Playback::stalled)
    }
}

/// How the screen was left.
pub(super) enum Exit {
    /// Saved or cancelled; back to the menu.
    Menu,
    /// The app is closing.
    Quit,
}

/// Runs the calibration screen until it is saved, cancelled, or the app is asked to close.
pub(super) fn run(
    canvas: &sdl3::render::WindowCanvas,
    gl: &mut Option<console_flow::ConsoleGl>,
    events: &mut sdl3::EventPump,
    fonts: &pf_console_ui::theme::Fonts,
    display: (u32, u32),
) -> Result<Exit> {
    let stored = store::load().settings.hdr_display();
    let step = Step::default();
    let playback = Playback::start(pattern_meta(step, stored), pattern(step, stored))
        .inspect_err(|e| tracing::warn!("HDR calibration playback: {e:#}"))
        .ok();
    let mut cal = Calibration {
        step,
        display: stored,
        playback,
    };
    let mut remote_keys = RemoteKeys::default();
    let mut exit_held = true;
    // One line per streak of undrawable frames — see `overlay::drawn`.
    let mut overlay_warned = false;
    let exit = 'screen: loop {
        let started = Instant::now();
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            break 'screen Exit::Quit;
        }
        if exit_gesture_fired(&mut exit_held) {
            break 'screen Exit::Menu;
        }
        for event in events.poll_iter() {
            use sdl3::event::Event;
            let remote = remote_keys.press(&event);
            let ev = match event {
                Event::Quit { .. } => break 'screen Exit::Quit,
                _ if remote == Some(RemoteKey::Back) => Some(MenuEvent::Back),
                // A held OK must not walk every step and save values nobody judged.
                Event::KeyDown {
                    keycode: Some(k),
                    repeat,
                    ..
                } => menu_event_for_key(k).filter(|ev| !repeat || *ev != MenuEvent::Confirm),
                Event::GamepadButtonDown { which, button, .. } => menu_event_for_button(which, button),
                // With the Magic Remote's pointer up, OK arrives as a click rather than a key.
                Event::MouseButtonDown {
                    mouse_btn: sdl3::mouse::MouseButton::Left,
                    ..
                } => Some(MenuEvent::Confirm),
                _ => None,
            };
            match ev {
                Some(MenuEvent::Back) => {
                    tracing::info!("HDR calibration cancelled");
                    break 'screen Exit::Menu;
                }
                Some(MenuEvent::Left | MenuEvent::Down) => cal.nudge(-1),
                Some(MenuEvent::Right | MenuEvent::Up) => cal.nudge(1),
                Some(MenuEvent::Confirm) => match cal.step.next() {
                    Some(next) => {
                        cal.step = next;
                        cal.refresh();
                    }
                    None => {
                        save(cal.display);
                        break 'screen Exit::Menu;
                    }
                },
                _ => {}
            }
        }
        // Punched through once the plane shows the pattern; a plain ground until then.
        let clear = if cal.presenting() {
            overlay::TRANSPARENT
        } else {
            Color4f::new(0.0, 0.0, 0.0, 1.0)
        };
        // A TV panel over the pattern (picture settings, the natural thing to open here) fails
        // every GL call on this surface until it closes: the card freezes, the screen stays.
        let frame = overlay::frame(gl, canvas, fonts, display, clear, |f| draw(f, &cal));
        overlay::drawn(frame, &mut overlay_warned);
        let elapsed = started.elapsed();
        if elapsed < TICK {
            std::thread::sleep(TICK - elapsed);
        }
    };
    // Dropping the feed unloads NDL, which has to happen before a stream loads its own player.
    drop(cal);
    // The console redraws the whole surface on its first frame, so a wipe that could not draw
    // here costs nothing.
    overlay::drawn(overlay::wipe(gl, canvas, fonts), &mut overlay_warned);
    Ok(exit)
}

fn save(volume: HdrDisplay) {
    let mut state = store::load();
    state.settings.set_hdr_display(volume, true);
    match store::save(&state) {
        Ok(()) => tracing::info!(
            peak_nits = volume.peak_nits,
            frame_avg_nits = volume.frame_avg_nits,
            black_code = volume.black_code,
            "HDR calibration saved"
        ),
        Err(e) => tracing::error!("HDR calibration not saved: {e:#}"),
    }
}

/// The card, pinned to the bottom so the windows above centre stay clear: title, instruction,
/// the slider with its value, and the key hint.
fn draw(f: &Frame<'_>, cal: &Calibration) {
    let (c, k) = (f.canvas, f.k);
    let w = (f.w * CARD_WIDTH_FRAC).round();
    let inner = w - 2.0 * PAD * k;
    let body = if cal.stalled() && !cal.presenting() {
        "The video plane did not accept the test pattern. The values below are unchanged."
    } else {
        cal.step.instruction()
    };
    let lines = wrap(f.fonts, body, W::Regular, BODY_SIZE * f64::from(k), f64::from(inner));
    let title_h = line_h(TITLE_SIZE * f64::from(k)) as f32;
    let body_line = line_h(BODY_SIZE * f64::from(k)) as f32;
    let hint_h = line_h(HINT_SIZE * f64::from(k)) as f32;
    let slider_h = 2.0 * KNOB_R * k;
    let h = 2.0 * PAD * k + title_h + body_line * lines.len() as f32 + slider_h + hint_h + 3.0 * GAP * k;
    let card = Rect::from_xywh(
        ((f.w - w) / 2.0).round(),
        (f.h * (1.0 - BOTTOM_MARGIN_FRAC) - h).round(),
        w,
        h.round(),
    );
    alpha_layer(c, card, 1.0);
    opaque_card(f, card, CORNER);
    let left = card.left + PAD * k;
    let mut y = card.top + PAD * k;
    f.fonts.draw(
        c,
        cal.step.label(),
        f64::from(left),
        f64::from(y + title_h * 0.8),
        W::SemiBold,
        TITLE_SIZE * f64::from(k),
        theme::fg(1.0),
    );
    let value = cal.step.value_text(cal.display);
    let value_w = f.fonts.measure(&value, W::Medium, TITLE_SIZE * f64::from(k));
    f.fonts.draw(
        c,
        &value,
        f64::from(card.right - PAD * k - value_w),
        f64::from(y + title_h * 0.8),
        W::Medium,
        TITLE_SIZE * f64::from(k),
        theme::accent(1.0),
    );
    y += title_h + GAP * k;
    for line in &lines {
        f.fonts.draw(
            c,
            line,
            f64::from(left),
            f64::from(y + body_line * 0.8),
            W::Regular,
            BODY_SIZE * f64::from(k),
            theme::fg(0.72),
        );
        y += body_line;
    }
    y += GAP * k;
    let fraction = cal.step.lattice().fraction(u32::from(cal.step.value(cal.display)));
    let cy = y + KNOB_R * k;
    let track = Rect::from_xywh(left, cy - TRACK_H * k / 2.0, inner, TRACK_H * k);
    let radius = TRACK_H * k / 2.0;
    c.draw_rrect(RRect::new_rect_xy(track, radius, radius), &theme::fill(theme::fg(0.15)));
    let filled = Rect::from_xywh(track.left, track.top, inner * fraction, track.height());
    c.draw_rrect(
        RRect::new_rect_xy(filled, radius, radius),
        &theme::fill(theme::accent(1.0)),
    );
    let mut knob = theme::fill(theme::fg(1.0));
    knob.set_anti_alias(true);
    c.draw_circle((track.left + inner * fraction, cy), KNOB_R * k, &knob);
    y += slider_h + GAP * k;
    let hint = if cal.step.next().is_some() {
        "◀ ▶ Adjust   ·   OK Next   ·   Back Cancel"
    } else {
        "◀ ▶ Adjust   ·   OK Save   ·   Back Cancel"
    };
    f.fonts.draw(
        c,
        hint,
        f64::from(left),
        f64::from(y + hint_h * 0.8),
        W::Regular,
        HINT_SIZE * f64::from(k),
        theme::fg(0.5),
    );
    c.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full-screen mosaic is half `b` tiles over an `a` field, all inside the picture.
    #[test]
    fn mosaic_fills_the_picture_with_half_the_tiles() {
        let patches = mosaic(FULL_SCREEN_AREA, 1, 2);
        assert_eq!(patches.len(), 1 + MOSAIC_TILES * MOSAIC_TILES / 2);
        assert!(patches
            .iter()
            .all(|p| p.x >= 0.0 && p.y >= 0.0 && p.x + p.w <= 1.0 + 1e-6));
    }

    /// Lowering the peak below the frame average drags the average down with it.
    #[test]
    fn peak_never_falls_below_frame_average() {
        let mut cal = Calibration {
            step: Step::Peak,
            display: HdrDisplay {
                peak_nits: model::HDR_PEAK.lo as u16,
                frame_avg_nits: model::HDR_FRAME_AVG.hi as u16,
                black_code: pq::BLACK_CODE,
            },
            playback: None,
        };
        cal.nudge(0);
        assert!(cal.display.frame_avg_nits <= cal.display.peak_nits);
    }
}
