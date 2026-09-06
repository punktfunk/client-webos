//! Per-screen payloads: the state a single screen owns while it is up.
//!
//! Grouped so it is visible at a glance which state belongs to a screen rather than to the app —
//! and so a screen's own `open_*` is the only thing that has to remember what to reset. Fields
//! that are `Option` are `None` off-screen; the flat ones (pairing's PIN, the host-menu cursor
//! into the sidebar) are reset by the `open_*` that raises their screen.

use crate::app::state::collections::CollectionsState;
use crate::app::state::hdrcalibration::HdrCalibrationState;
use crate::app::state::hostpower::ProbeFailure;
use crate::app::state::settingspage::SettingsPage;
use crate::app::state::speedtest::SpeedTestState;
use crate::app::state::textfield::TextField;
use crate::app::WakeState;
use crate::core::screen::PairingFocus;

#[derive(Default)]
pub(crate) struct ScreenSlots {
    /// The sidebar row the host menu (and everything reached from it) is acting on, `None`
    /// otherwise.
    pub(crate) host_menu_index: Option<usize>,
    /// Which of the focused row's trailing buttons has focus, rather than the row body — the
    /// row-list counterpart of `HomeFocus::SidebarMenu`. Shared by every screen whose rows
    /// carry them (the host menu's ⋯, a collection's rename/remove), because focus is only
    /// ever on one row of one list at a time. Cleared by any vertical move.
    pub(crate) row_button: Option<super::rowbuttons::RowButton>,
    /// What the host menu's power row does, latched when that menu opens rather than derived
    /// per frame.
    ///
    /// Latched because it is read at two different times — once to draw the row, once when
    /// Confirm lands on it — and it is derived from reachability, which `note_reachable` can
    /// flip between them off an mDNS announce. Deriving it twice let a row drawn "Wake host"
    /// shut the machine down instead. Going stale is the safe direction: a host that comes up
    /// while the menu is open still offers Wake, and a magic packet to a running host is
    /// nothing.
    pub(crate) host_menu_power: Option<crate::services::store::ExitAction>,
    /// The sidebar row `Screen::EditHost` is editing, `None` otherwise.
    pub(crate) edit_host_index: Option<usize>,
    /// The HDR calibration in progress — its step, its scratch volume and the pattern feed on the
    /// video plane. `None` whenever that screen isn't open, which is also what stops the feed.
    pub(crate) hdr: Option<HdrCalibrationState>,
    /// The in-flight/finished speed test, `None` when that screen isn't open.
    pub(crate) speed_test: Option<SpeedTestState>,
    /// The host being measured, for the status line.
    pub(crate) speed_test_name: String,
    pub(crate) add_host: TextField,
    /// The settings page's page, scope and column focus (`Screen::SettingsPage`).
    pub(crate) settings_page: SettingsPage,
    /// The name being typed for the profile in scope (`Screen::RenameProfile`).
    pub(crate) profile_name: TextField,
    /// What `Screen::PickProfile` is picking for.
    pub(crate) profile_pick: Option<crate::app::state::profilepick::ProfilePick>,
    /// The About document's first visible line.
    pub(crate) about_scroll: usize,
    /// The card `Screen::Collections` is moving, and its title — see [`CollectionsState`].
    pub(crate) collections: CollectionsState,
    /// What the host said about this pairing's power rights, `None` while the probe is still
    /// out (or before the screen that asks has ever been opened). Never persisted — a grant
    /// can be revoked on the host between visits, exactly like the root probe's verdict.
    /// `Err` distinguishes a host too old for the actions route (`Unsupported`) from one that
    /// never answered — the captions for the two send the reader to different places.
    pub(crate) power_rights: Option<Result<crate::services::power::PowerRights, ProbeFailure>>,
    /// The active "host unreachable — wake it?" prompt/wait, if any — see `WakeState`.
    pub(crate) wake: Option<WakeState>,
    /// PIN entry: 4 digits, each 0-9, edited one at a time.
    pub(crate) pin_digits: [u8; 4],
    pub(crate) pin_digit_index: usize,
    /// Whether the pairing modal's input is on the PIN row or the Request-access button.
    pub(crate) pairing_focus: PairingFocus,
    pub(crate) pairing_status: Option<String>,
    pub(crate) pairing_busy: bool,
    /// Index into `hosts.entries` currently being paired — captured when entering
    /// `Screen::Pairing`.
    pub(crate) pairing_entry: usize,
}
