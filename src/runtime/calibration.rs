//! HDR calibration: three measurements, one per luminance in the CTA-861.3 HDR static-metadata
//! block, made against synthetic PQ patterns played on the real video plane — the only signal
//! path a stream will ever use. Opened from the menu with the remote's Blue key.
//!
//! The steps and their order follow the Windows HDR Calibration app (HGIG's recommended tests).
//! Floor and peak show its window pane: a frame at the slider's luminance around four squares at
//! a fixed reference — no light for the floor, 10,000 nits for the peak. The squares vanish where
//! the panel can no longer tell the slider from the reference: where it crushes, or clips. The
//! full frame is a plain field, climbed until a step no longer brightens it.
//!
//! The edits are held in a scratch [`HdrDisplay`] and only written to the document by the last
//! step's OK. A half-walked calibration is worse than none: it would advertise a volume the user
//! never confirmed.

use pf_console_ui::theme::{self, W};
use skia_safe::{Color4f, RRect, Rect};

use super::overlay::{self, alpha_layer, line_h, opaque_card, wrap, Frame};
use super::*;
use crate::core::model::{self, HdrDisplay};
use crate::core::pq;
use crate::platform::webos::hdr_pattern::{Pattern, Playback};
use crate::platform::webos::input::{menu_event_for_button, menu_event_for_key, wait_for_event, RemoteKey, RemoteKeys};
use crate::services::hevc::{self, Patch};
use pf_client_core::menu_nav::{MenuDir, MenuEvent};
use punktfunk_core::quic::HdrMeta;

/// The window pane, measured off the Windows HDR Calibration app: a square covering 10% of the
/// picture (HGIG's peak window, small enough that ABL leaves a self-emissive panel free), centred
/// where Windows centres it — clear of the card pinned to the bottom of the screen.
const PANE_AREA: f32 = 0.10;
const PANE_CENTER_Y: f32 = 0.47;

/// Across the pane's side: a frame band, a square, a cross bar twice the band, a square, a band.
/// On a 432-pixel pane that is 18 + 180 + 36 + 180 + 18.
const PANE_BAND: f32 = 1.0 / 24.0;
const PANE_SQUARE: f32 = 5.0 / 12.0;
const PANE_BAR: f32 = 2.0 * PANE_BAND;

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

/// Windows' order: floor, then peak, then full frame, which the peak bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Step {
    #[default]
    Black,
    Peak,
    FrameAverage,
}

/// What a step shows and measures, apart from the value itself.
struct StepSpec {
    label: &'static str,
    instruction: &'static str,
}

impl Step {
    /// The step after this one; `None` on the last, whose OK saves.
    fn next(self) -> Option<Self> {
        match self {
            Self::Black => Some(Self::Peak),
            Self::Peak => Some(Self::FrameAverage),
            Self::FrameAverage => None,
        }
    }

    fn spec(self) -> &'static StepSpec {
        const BLACK: StepSpec = StepSpec {
            label: "Minimum luminance",
            instruction: "Step 1 of 3. Lower until the pattern is no longer visible.",
        };
        const PEAK: StepSpec = StepSpec {
            label: "Maximum luminance",
            instruction: "Step 2 of 3. With the TV's Tone Mapping on HGIG, raise until the pattern is no longer \
                          visible.",
        };
        const FRAME_AVERAGE: StepSpec = StepSpec {
            label: "Max full frame luminance",
            instruction: "Step 3 of 3. Raise one step at a time, pausing after each. Stop as soon as a step \
                          no longer makes the screen brighter.",
        };
        match self {
            Self::Black => &BLACK,
            Self::Peak => &PEAK,
            Self::FrameAverage => &FRAME_AVERAGE,
        }
    }

    /// The slider's stops. The full field's end at the peak measured before it.
    fn lattice(self, display: HdrDisplay) -> model::Lattice {
        match self {
            Self::Black => model::HDR_BLACK,
            Self::Peak => model::HDR_PEAK,
            Self::FrameAverage => display.frame_avg_range(),
        }
    }

    /// Where the slider starts on entering the step. The full field climbs from the bottom: its
    /// answer is the first step that stops brightening, which only shows from below it.
    fn start(self, display: HdrDisplay) -> HdrDisplay {
        match self {
            Self::FrameAverage => self.with_value(display, self.lattice(display).lo),
            Self::Black | Self::Peak => display,
        }
    }

    fn value(self, display: HdrDisplay) -> u16 {
        match self {
            Self::Black => display.black_code,
            Self::Peak => display.peak_nits,
            Self::FrameAverage => display.frame_avg_nits,
        }
    }

    /// `display` with this step's value replaced, back inside the volume's invariants.
    fn with_value(self, mut display: HdrDisplay, value: u16) -> HdrDisplay {
        *match self {
            Self::Black => &mut display.black_code,
            Self::Peak => &mut display.peak_nits,
            Self::FrameAverage => &mut display.frame_avg_nits,
        } = value;
        display.normalized()
    }

    fn value_text(self, display: HdrDisplay) -> String {
        match self {
            Self::Black => black_text(self.value(display)),
            Self::Peak | Self::FrameAverage => format!("{} nits", self.value(display)),
        }
    }
}

/// What the plane shows for `step`: the mastering volume to declare, and the picture.
///
/// The first two steps show the window pane on black: its frame at the slider's luminance, its
/// squares at the reference. Above the floor the reference is the top of PQ; declared above the
/// volume's ceiling, it clips to wherever the TV puts that ceiling. So the squares show while the
/// slider is below what the panel renders and go once both land on the same light. Near black
/// the TV has nothing to compress, so the codes reach the panel unconverted.
///
/// The full frame has no pattern: the whole picture at the slider's luminance. ABL dims a frame by
/// one gain, so nothing inside a frame can show it; what can is the step itself. Below the
/// sustained limit each step brightens the screen, and past it ABL holds the screen where it was.
///
/// The declared ceiling is the value the step measures, so the slider's luminance sits
/// at the top of the declared range. The floor is declared at its minimum throughout: a declared
/// black that moved with the slider would change how the TV lifts the range while it is judged.
fn frame(step: Step, display: HdrDisplay) -> (HdrMeta, Pattern) {
    let value = step.value(display);
    let (code, icon, ceiling) = match step {
        Step::Black => (value, pq::BLACK_CODE, display.peak_nits),
        Step::Peak | Step::FrameAverage => (pq::pq_code(f32::from(value)), pq::WHITE_CODE, value),
    };
    let meta = HdrDisplay {
        peak_nits: ceiling,
        frame_avg_nits: display.frame_avg_nits,
        black_code: pq::BLACK_CODE,
    }
    .hdr_meta();
    let pattern = match step {
        Step::Black | Step::Peak => Pattern {
            background: pq::BLACK_CODE,
            patches: window_pane(code, icon),
        },
        Step::FrameAverage => Pattern {
            background: code,
            patches: Vec::new(),
        },
    };
    (meta, pattern)
}

/// The Windows window pane: a square covering 10% of the picture in `field`, square on the panel
/// (the coded picture has the panel's aspect), its frame and cross left in `field` and four
/// squares in `icon`.
fn window_pane(field: u16, icon: u16) -> Vec<Patch> {
    let aspect = hevc::WIDTH as f32 / hevc::HEIGHT as f32;
    let h = (PANE_AREA * aspect).sqrt();
    let w = h / aspect;
    let frame = Patch {
        x: (1.0 - w) / 2.0,
        y: PANE_CENTER_Y - h / 2.0,
        w,
        h,
        code: field,
    };
    let (near, far) = (PANE_BAND, PANE_BAND + PANE_SQUARE + PANE_BAR);
    let squares = [(near, near), (far, near), (near, far), (far, far)].map(|(dx, dy)| Patch {
        x: frame.x + dx * frame.w,
        y: frame.y + dy * frame.h,
        w: PANE_SQUARE * frame.w,
        h: PANE_SQUARE * frame.h,
        code: icon,
    });
    std::iter::once(frame).chain(squares).collect()
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

/// Everything the card and the clear depend on: the card is redrawn only when this changes.
type Visual = (Step, HdrDisplay, bool, bool);

impl Calibration {
    /// The slider's writer of the scratch volume: clamps the stop, applies it to the step's field
    /// and re-feeds the pattern if anything changed. [`Step::start`] is the only other.
    fn nudge(&mut self, delta: i32) {
        let lattice = self.step.lattice(self.display);
        let value = lattice.value(lattice.index(self.step.value(self.display)) as i32 + delta);
        let display = self.step.with_value(self.display, value);
        if display != self.display {
            self.display = display;
            self.refresh();
        }
    }

    fn refresh(&self) {
        if let Some(playback) = &self.playback {
            let (meta, pattern) = frame(self.step, self.display);
            playback.show(meta, pattern);
        }
    }

    fn presented(&self) -> bool {
        self.playback.as_ref().is_some_and(Playback::presented)
    }

    fn stalled(&self) -> bool {
        self.playback.as_ref().is_none_or(Playback::stalled)
    }

    fn visual(&self) -> Visual {
        (self.step, self.display, self.presented(), self.stalled())
    }
}

/// Runs the calibration screen until it is saved, cancelled, or the app is asked to close.
pub(super) fn run(
    canvas: &sdl3::render::WindowCanvas,
    gl: &mut Option<console_flow::ConsoleGl>,
    events: &mut sdl3::EventPump,
    fonts: &pf_console_ui::theme::Fonts,
    display: (u32, u32),
) -> Result<StreamOutcome> {
    let stored = store::load().settings.hdr_display().normalized();
    let step = Step::default();
    let (meta, pattern) = frame(step, stored);
    let playback = Playback::start(meta, pattern)
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
    let mut drawn: Option<Visual> = None;
    let exit = 'screen: loop {
        let started = Instant::now();
        if QUIT_REQUESTED.load(Ordering::Relaxed) {
            break 'screen StreamOutcome::Quit;
        }
        if exit_gesture_fired(&mut exit_held) {
            break 'screen StreamOutcome::ReturnToMenu;
        }
        for event in events.poll_iter() {
            use sdl3::event::Event;
            let remote = remote_keys.press(&event);
            let ev = match event {
                Event::Quit { .. } => break 'screen StreamOutcome::Quit,
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
                    break 'screen StreamOutcome::ReturnToMenu;
                }
                Some(MenuEvent::Move(MenuDir::Left | MenuDir::Down)) => cal.nudge(-1),
                Some(MenuEvent::Move(MenuDir::Right | MenuDir::Up)) => cal.nudge(1),
                Some(MenuEvent::Confirm) => match cal.step.next() {
                    Some(next) => {
                        cal.step = next;
                        cal.display = next.start(cal.display);
                        cal.refresh();
                    }
                    None => {
                        save(cal.display);
                        break 'screen StreamOutcome::ReturnToMenu;
                    }
                },
                _ => {}
            }
        }
        let visual = cal.visual();
        if drawn != Some(visual) {
            // Punched through once the plane shows the pattern; a plain ground until then.
            let clear = if visual.2 {
                overlay::TRANSPARENT
            } else {
                Color4f::new(0.0, 0.0, 0.0, 1.0)
            };
            // A TV panel over the pattern (picture settings, the natural thing to open here)
            // fails every GL call on this surface until it closes: the card freezes, the screen
            // stays, and the frame is retried next tick.
            let frame = overlay::frame(gl, canvas, fonts, display, clear, |f| draw(f, &cal));
            if overlay::drawn(frame, &mut overlay_warned) {
                drawn = Some(visual);
            }
        }
        wait_for_event(console_flow::TICK_BUDGET.saturating_sub(started.elapsed()));
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
    let kf = f64::from(k);
    let spec = cal.step.spec();
    // Text whose line box starts at `top`, `line` tall.
    let text = |s: &str, x: f32, top: f32, line: f32, weight: W, size: f64, color| {
        f.fonts.draw(
            c,
            s,
            f64::from(x),
            f64::from(top + line * 0.8),
            weight,
            size * kf,
            color,
        );
    };
    let w = (f.w * CARD_WIDTH_FRAC).round();
    let inner = w - 2.0 * PAD * k;
    let body = if cal.stalled() {
        "The video plane did not accept the test pattern. The values below are unchanged."
    } else {
        spec.instruction
    };
    let lines = wrap(f.fonts, body, W::Regular, BODY_SIZE * kf, f64::from(inner));
    let title_h = line_h(TITLE_SIZE * kf) as f32;
    let body_line = line_h(BODY_SIZE * kf) as f32;
    let hint_h = line_h(HINT_SIZE * kf) as f32;
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
    text(spec.label, left, y, title_h, W::SemiBold, TITLE_SIZE, theme::fg(1.0));
    let value = cal.step.value_text(cal.display);
    let value_x = card.right - PAD * k - f.fonts.measure(&value, W::Medium, TITLE_SIZE * kf);
    text(&value, value_x, y, title_h, W::Medium, TITLE_SIZE, theme::accent(1.0));
    y += title_h + GAP * k;
    for line in &lines {
        text(line, left, y, body_line, W::Regular, BODY_SIZE, theme::fg(0.72));
        y += body_line;
    }
    y += GAP * k;
    let fraction = cal.step.lattice(cal.display).fraction(cal.step.value(cal.display));
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
    text(hint, left, y, hint_h, W::Regular, HINT_SIZE, theme::fg(0.5));
    c.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pane's four squares sit inside its frame.
    #[test]
    fn window_pane_stays_inside_its_box() {
        let patches = window_pane(1, 2);
        let b = patches[0];
        assert_eq!(patches.len(), 5);
        assert!(patches[1..]
            .iter()
            .all(|p| p.x >= b.x && p.y >= b.y && p.x + p.w <= b.x + b.w + 1e-6 && p.y + p.h <= b.y + b.h + 1e-6));
    }
}
