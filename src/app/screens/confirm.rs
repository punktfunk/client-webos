//! The app's two-button confirmation dialogs.
//!
//! Each is one card whose height its subtitle decides, one button row under it, and a
//! cursor picking between the two buttons. That used to be six `match self.screen` tables —
//! the subtitle, the focus, the focus setter, the button rect, and two inside `prepare_modal`
//! — each of which had to list every confirmation screen. Here the screen answers once, with a
//! value, and everything else reads that value.
use crate::app::nav::ScreenKey;
use crate::app::view;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use crate::ui::render::Color;
use crate::ui::theme::palette;
use crate::ui::widgets::ConfirmButton;
use std::borrow::Cow;

impl App {
    /// The nav half of a confirm dialog's event handling, the counterpart to
    /// `list_nav_event`: Left/Right trade the two buttons and arm the focus pop, reporting
    /// whether the event was spent doing so. Every one of these handlers starts here.
    pub(crate) fn confirm_nav_event(&mut self, ev: MenuEvent) -> bool {
        if !matches!(ev, MenuEvent::Left | MenuEvent::Right) {
            return false;
        }
        let key = ScreenKey::of(self.nav.screen);
        self.nav.set_cursor(key, 1 - self.nav.cursor(key));
        self.render.modal.focus_anim = Some(std::time::Instant::now());
        true
    }
}

/// One button of a [`Confirm`]. `Cow` because a speed test's primary button names the bitrate
/// it would apply, which is derived — every other label is static, and this dialog is rebuilt
/// per frame and per pointer motion for its geometry alone (see `App::confirm_of`).
pub(crate) struct Button {
    pub icon: Option<&'static str>,
    pub label: Cow<'static, str>,
    pub color: Color,
}

/// An open confirm dialog, as data: what its card says, and what its two buttons are.
pub(crate) struct Confirm {
    pub subtitle: String,
    pub buttons: [Button; 2],
}

impl Confirm {
    fn new(
        icon: Option<&'static str>,
        label: impl Into<Cow<'static, str>>,
        color: Color,
        cancel: &'static str,
        subtitle: String,
    ) -> Self {
        Self {
            subtitle,
            buttons: [
                Button {
                    icon,
                    label: label.into(),
                    color,
                },
                Button {
                    icon: None,
                    label: Cow::Borrowed(cancel),
                    color: palette().text,
                },
            ],
        }
    }

    /// The widget-facing view of [`Self::buttons`] — borrowed, so nothing is copied per frame.
    pub fn widgets(&self) -> [ConfirmButton<'_>; 2] {
        std::array::from_fn(|i| ConfirmButton {
            icon: self.buttons[i].icon,
            label: &self.buttons[i].label,
            color: self.buttons[i].color,
        })
    }
}

impl App {
    /// The open confirm dialog, or `None` — on a screen that isn't one, and on the one whose
    /// buttons aren't up yet: a speed test still running has nothing to apply.
    ///
    /// That `None` is load-bearing beyond the geometry: it is what says the dialog is not
    /// showing buttons, so a caller holding a `Some` has already proved the arm it is in is
    /// reachable — which is what four `expect`/`unreachable!` in `prepare_modal` used to
    /// assert by hand.
    pub(crate) fn confirm_of(&self) -> Option<Confirm> {
        Some(match self.nav.screen {
            Screen::ForgetHost => Confirm::new(
                Some(view::icons::ICON_DELETE),
                "Forget",
                palette().error,
                "Cancel",
                view::forget::subtitle(self.host_menu_host_name().unwrap_or_default()),
            ),
            Screen::RemoveCollection => {
                let (name, games) = self.removed_collection()?;
                Confirm::new(
                    Some(view::icons::ICON_DELETE),
                    "Remove",
                    palette().error,
                    "Cancel",
                    view::collections::remove_subtitle(name, games),
                )
            }
            Screen::ResetHdrCalibration => Confirm::new(
                Some(view::icons::ICON_DELETE),
                "Clear",
                palette().error,
                "Cancel",
                view::hdrcalibration::RESET_SUBTITLE.to_string(),
            ),
            Screen::ResetGameSettings => Confirm::new(
                Some(view::icons::ICON_DELETE),
                "Reset",
                palette().error,
                "Cancel",
                view::resetgame::subtitle(&self.settings_ui.game_settings.as_ref()?.title),
            ),
            Screen::SendLogs => Confirm::new(
                Some(view::icons::ICON_SEND),
                "Send",
                // The same red as Forget: both are consequential.
                palette().error,
                "Cancel",
                view::sendlogs::SUBTITLE.to_string(),
            ),
            Screen::Wake => Confirm::new(
                Some(view::icons::ICON_POWER),
                "Wake host",
                palette().accent_bright,
                "Cancel",
                view::wake::status_text(self.screens.wake.as_ref()?),
            ),
            Screen::SpeedTest => {
                let state = self.screens.speed_test.as_ref();
                if !view::speedtest::finished(state) {
                    return None;
                }
                Confirm::new(
                    Some(view::icons::ICON_SIGNAL),
                    view::speedtest::apply_label(view::speedtest::recommendation(state)),
                    palette().accent_bright,
                    // "Close" rather than "Cancel": the test has already run, so there is
                    // nothing left to call off.
                    "Close",
                    view::speedtest::status(state, &self.screens.speed_test_name),
                )
            }
            _ => return None,
        })
    }
}
