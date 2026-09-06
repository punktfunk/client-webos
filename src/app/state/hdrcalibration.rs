//! HDR calibration logic. Rendering and the patterns live in `app::view::hdrcalibration`.
//!
//! Three measurements, one per luminance in the CTA-861.3 HDR static-metadata block. They are
//! made against synthetic PQ patterns played on the real video plane, because that is the only
//! signal path a stream will ever use — anything drawn on the graphics plane instead would be
//! measuring the compositor, not the panel.
//!
//! The edits are held in a scratch [`HdrDisplay`] and only written to `Settings` by *Save and
//! finish*. A half-walked calibration is worse than none: it would advertise a volume the user
//! never confirmed.
use crate::app::nav::ScreenKey;
use crate::app::view;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::model::HdrDisplay;
use crate::core::screen::Screen;
use crate::core::settings::TvSettings;
pub(crate) use crate::platform::webos::hdr_pattern::Playback;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum HdrStep {
    #[default]
    Peak,
    FrameAverage,
    Black,
}

impl HdrStep {
    /// The step after this one, `None` on the last — which is also what makes the action row
    /// read *Save and finish* rather than *Next*.
    pub(crate) fn next(self) -> Option<Self> {
        match self {
            Self::Peak => Some(Self::FrameAverage),
            Self::FrameAverage => Some(Self::Black),
            Self::Black => None,
        }
    }
}

/// What the calibration screen owns while it is up — see `screens::slots`.
pub(crate) struct HdrCalibrationState {
    pub(crate) step: HdrStep,
    /// The volume being measured. Committed to `Settings` only on finish.
    pub(crate) display: HdrDisplay,
    /// The pattern feed on the NDL plane. `None` once it has failed or been torn down; the
    /// screen stays usable either way, it just has nothing to show.
    pub(crate) playback: Option<Playback>,
    /// Last `(presenting, stalled)` the loop was told about — see [`App::tick_hdr_pattern`].
    seen: (bool, bool),
}

impl App {
    /// Opens the calibration screen and starts the pattern feed. Starts from whatever is already
    /// stored, so re-running it refines the previous answer instead of restarting from defaults.
    pub(crate) fn open_hdr_calibration(&mut self) {
        let step = HdrStep::default();
        let display = self.settings_ui.settings.hdr_display();
        let playback = match Playback::start(
            view::hdrcalibration::pattern_meta(step, display),
            view::hdrcalibration::pattern(step, display),
        ) {
            Ok(playback) => Some(playback),
            // Worth entering anyway: the sliders and their stored values still work, and the
            // screen's own error caption explains why nothing is on screen.
            Err(e) => {
                tracing::warn!("HDR calibration playback: {e:#}");
                None
            }
        };
        self.screens.hdr = Some(HdrCalibrationState {
            step,
            display,
            playback,
            seen: (false, false),
        });
        self.screens.row_button = None;
        self.nav.enter(Screen::HdrCalibration, 0);
    }

    /// The slider takes Left/Right; Confirm advances a step, or commits on the last. Back
    /// cancels, leaving `Settings` untouched.
    pub(crate) fn handle_hdr_calibration_event(&mut self, ev: MenuEvent) {
        if ev == MenuEvent::Back {
            self.close_hdr_calibration(false);
            return;
        }
        if self.list_nav_event(ev) {
            return;
        }
        // One row, so no cursor to consult. Left/Right belong to the slider — they are the
        // measurement — so the tick is reached with Down and left with Up, rather than with the
        // Right a collection row's buttons take.
        match ev {
            MenuEvent::Left => self.nudge_hdr_stop(-1),
            MenuEvent::Right => self.nudge_hdr_stop(1),
            MenuEvent::Down | MenuEvent::Up => {
                self.step_row_button(ev == MenuEvent::Down);
            }
            MenuEvent::Confirm => self.advance_hdr_step(),
            _ => {}
        }
    }

    /// Moves the current step's slider by `delta` stops.
    fn nudge_hdr_stop(&mut self, delta: i32) {
        let Some(hdr) = &self.screens.hdr else { return };
        let stop = hdr.step.lattice().index(u32::from(hdr.step.value(hdr.display))) as i32 + delta;
        self.set_hdr_stop(stop);
    }

    /// Sets the current step's slider from a 0..1 position along its track — the pointer path.
    pub(crate) fn set_hdr_fraction(&mut self, fraction: f32) {
        let Some(hdr) = &self.screens.hdr else { return };
        self.set_hdr_stop(hdr.step.lattice().stop_at(fraction));
    }

    /// The one writer of the scratch volume: clamps the stop into range, applies it to whichever
    /// field the step measures, and re-feeds the pattern.
    fn set_hdr_stop(&mut self, stop: i32) {
        let Some(hdr) = &mut self.screens.hdr else { return };
        let value = hdr.step.lattice().value(stop) as u16;
        let before = hdr.display;
        match hdr.step {
            HdrStep::Peak => {
                hdr.display.peak_nits = value;
                // A full field can never out-run a small window, so lowering the peak drags the
                // frame average down with it rather than leaving an impossible pair behind.
                hdr.display.frame_avg_nits = hdr.display.frame_avg_nits.min(value);
            }
            HdrStep::FrameAverage => hdr.display.frame_avg_nits = value.min(hdr.display.peak_nits),
            HdrStep::Black => hdr.display.black_code = value,
        }
        // A held key at the end of the track, or a pointer landing on the stop already selected,
        // would otherwise queue a full frame re-encode per event for an unchanged picture.
        if hdr.display != before {
            self.refresh_hdr_pattern();
        }
    }

    fn advance_hdr_step(&mut self) {
        let Some(hdr) = &mut self.screens.hdr else { return };
        match hdr.step.next() {
            Some(next) => {
                hdr.step = next;
                // Back to the slider: the next step's measurement is what the card is for, and
                // leaving focus on the tick would step past it on the first Confirm.
                self.screens.row_button = None;
                self.refresh_hdr_pattern();
            }
            None => self.close_hdr_calibration(true),
        }
    }

    /// Hands the feed the pattern for the current step.
    ///
    /// Both halves move with the scratch volume: the pattern sits at the top of the declared
    /// range and the declared range is what the slider drives, which is the only place the
    /// flattening this reading depends on can happen — see
    /// [`view::hdrcalibration::pattern_meta`].
    fn refresh_hdr_pattern(&mut self) {
        let Some(hdr) = &self.screens.hdr else { return };
        let Some(playback) = &hdr.playback else { return };
        playback.show(
            view::hdrcalibration::pattern_meta(hdr.step, hdr.display),
            view::hdrcalibration::pattern(hdr.step, hdr.display),
        );
    }

    /// Tears the feed down and leaves. `commit` writes the measured volume; cancelling does not.
    pub(crate) fn close_hdr_calibration(&mut self, commit: bool) {
        // Dropping the state stops the feed (see `Playback`'s `Drop`), which has to happen before
        // a stream can load its own player on the same plane.
        self.screens.row_button = None;
        if let Some(hdr) = self.screens.hdr.take() {
            if commit {
                self.settings_ui.settings.set_hdr_display(hdr.display, true);
                self.persist();
                tracing::info!(
                    peak_nits = hdr.display.peak_nits,
                    frame_avg_nits = hdr.display.frame_avg_nits,
                    black_code = hdr.display.black_code,
                    min_luminance = hdr.display.min_luminance_units(),
                    "HDR calibration saved"
                );
            }
        }
        self.nav.resume(Screen::SettingsPage);
    }

    /// Opens the "clear this calibration?" dialog from the Calibrate row's delete button.
    pub(crate) fn open_reset_hdr_calibration(&mut self) {
        self.nav.enter(Screen::ResetHdrCalibration, 1);
    }

    /// Handles one menu event on [`Screen::ResetHdrCalibration`].
    pub(crate) fn handle_reset_hdr_event(&mut self, ev: MenuEvent) {
        if self.confirm_nav_event(ev) {
            return;
        }
        match ev {
            MenuEvent::Confirm => {
                if self.nav.cursor(ScreenKey::ResetHdrCalibration) == 0 {
                    self.clear_hdr_calibration();
                }
                self.nav.resume(Screen::SettingsPage);
            }
            MenuEvent::Back | MenuEvent::Secondary => self.nav.resume(Screen::SettingsPage),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {}
        }
    }

    /// Puts the panel volume back to the shipped default. The numbers go with the flag: a set
    /// that reads "not calibrated" while still advertising a measured volume is the worst of
    /// both, since nothing on screen would say where those numbers came from.
    fn clear_hdr_calibration(&mut self) {
        let default = crate::core::settings::default_document();
        self.settings_ui.settings.set_hdr_display(default.hdr_display(), false);
        // The button that opened this dialog is gone with the measurement, so the focus it held
        // has to go back to the row itself.
        self.screens.row_button = None;
        self.persist();
        tracing::info!("HDR calibration cleared");
    }

    /// Whether the NDL plane is actually showing the pattern, which decides whether the frame is
    /// punched through to the video underlay — see [`App::frame_clear_color`].
    pub(crate) fn hdr_pattern_presenting(&self) -> bool {
        self.screens
            .hdr
            .as_ref()
            .and_then(|hdr| hdr.playback.as_ref())
            .is_some_and(Playback::presenting)
    }

    /// Picks up what the pattern feed did on its own: it started presenting (the background has
    /// to be punched through), or it gave up (`Playback::stalled` is a deadline elapsing rather
    /// than an event, and the card's copy changes with it). Neither reaches the loop any other
    /// way, so both are polled — see [`App::tick_screens`].
    pub(super) fn tick_hdr_pattern(&mut self) -> bool {
        let Some(hdr) = &mut self.screens.hdr else {
            return false;
        };
        let now = hdr
            .playback
            .as_ref()
            .map_or((false, false), |p| (p.presenting(), p.stalled()));
        let changed = now != hdr.seen;
        hdr.seen = now;
        changed
    }
}
