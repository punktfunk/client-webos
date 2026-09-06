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
use std::borrow::Cow;

/// What a button means, which is what colours it: the palette's error red for a loss, the
/// accent for the thing the dialog exists to do, and plain text for Cancel.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tone {
    Danger,
    Primary,
    Plain,
}

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
    pub tone: Tone,
}

/// An open confirm dialog, as data: what its card says, and what its two buttons are.
pub(crate) struct Confirm {
    pub subtitle: String,
    pub buttons: [Button; 2],
    /// The body reads as an error (a failed speed test).
    pub failed: bool,
}

impl Confirm {
    pub(crate) fn new(
        icon: Option<&'static str>,
        label: impl Into<Cow<'static, str>>,
        tone: Tone,
        cancel: &'static str,
        subtitle: String,
    ) -> Self {
        Self {
            subtitle,
            failed: false,
            buttons: [
                Button {
                    icon,
                    label: label.into(),
                    tone,
                },
                Button {
                    icon: None,
                    label: Cow::Borrowed(cancel),
                    tone: Tone::Plain,
                },
            ],
        }
    }
}

impl App {
    /// [`confirm_of`](Self::confirm_of) for any screen, not only the one the cursor is on:
    /// a card fading out is drawn from the same descriptor after the cursor has moved on.
    pub(crate) fn confirm_for(&self, screen: Screen) -> Option<Confirm> {
        Some(match screen {
            Screen::ForgetHost => Confirm::new(
                Some(view::icons::ICON_DELETE),
                "Forget",
                Tone::Danger,
                "Cancel",
                view::forget::subtitle(self.host_menu_host_name().unwrap_or_default()),
            ),
            Screen::RemoveCollection => {
                let (name, games) = self.removed_collection()?;
                Confirm::new(
                    Some(view::icons::ICON_DELETE),
                    "Remove",
                    Tone::Danger,
                    "Cancel",
                    view::collections::remove_subtitle(name, games),
                )
            }
            Screen::ResetHdrCalibration => Confirm::new(
                Some(view::icons::ICON_DELETE),
                "Clear",
                Tone::Danger,
                "Cancel",
                view::hdrcalibration::RESET_SUBTITLE.to_string(),
            ),
            Screen::DeleteProfile => {
                let (hosts, titles) = self.profile_use_counts();
                Confirm::new(
                    Some(view::icons::ICON_DELETE),
                    "Delete",
                    Tone::Danger,
                    "Cancel",
                    view::profile::delete_subtitle(hosts, titles),
                )
            }
            Screen::SendLogs => Confirm::new(
                Some(view::icons::ICON_SEND),
                "Send",
                // The same red as Forget: both are consequential.
                Tone::Danger,
                "Cancel",
                view::sendlogs::SUBTITLE.to_string(),
            ),
            Screen::Wake => Confirm::new(
                Some(view::icons::ICON_POWER),
                "Wake host",
                Tone::Primary,
                "Cancel",
                view::wake::status_text(self.screens.wake.as_ref().filter(|w| !w.mac.is_empty())?),
            ),
            Screen::SpeedTest => {
                let state = self.screens.speed_test.as_ref();
                if !view::speedtest::finished(state) {
                    return None;
                }
                Confirm {
                    failed: matches!(state, Some(crate::app::state::speedtest::SpeedTestState::Failed(_))),
                    ..Confirm::new(
                        Some(view::icons::ICON_SIGNAL),
                        view::speedtest::apply_label(view::speedtest::recommendation(state)),
                        Tone::Primary,
                        // "Close" rather than "Cancel": the test has already run, so there is
                        // nothing left to call off.
                        "Close",
                        view::speedtest::status(state, &self.screens.speed_test_name),
                    )
                }
            }
            _ => return None,
        })
    }
}
