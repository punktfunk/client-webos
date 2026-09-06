//! The screen *families* — the shapes more than one screen shares.
//!
//! A screen's own state and view stay in `app::state::<screen>` / `app::view::<screen>`; what
//! lives here is the description a family's one implementation reads, so four dialogs that
//! differ only in their labels are four values rather than four copies of the same match arm
//! (see `docs/APP-REWORK-PLAN.md` §1, P4).
pub(crate) mod confirm;
pub(crate) mod list;
pub(crate) mod rowbuttons;
pub(crate) mod slots;

use crate::core::screen::Screen;

/// Whether `screen` is one of the two-button confirm dialogs — the family that shares a card
/// (one subtitle sizes it), a button row and a focus cursor, differing only in its labels.
///
/// Exhaustive on purpose: a new screen has to say which family it joins rather than being
/// absorbed by a `_ =>` arm into the wrong geometry.
pub(crate) const fn is_confirm(screen: Screen) -> bool {
    match screen {
        Screen::Wake
        | Screen::ForgetHost
        | Screen::SendLogs
        | Screen::SpeedTest
        | Screen::RemoveCollection
        | Screen::ResetHdrCalibration
        | Screen::DeleteProfile => true,
        Screen::Home
        | Screen::Pairing
        | Screen::AddHost
        | Screen::HostMenu
        | Screen::EditHost
        | Screen::About
        | Screen::HostPower
        | Screen::HdrCalibration
        | Screen::Collections
        | Screen::RenameCollection
        | Screen::SettingsPage
        | Screen::PickProfile
        | Screen::RenameProfile => false,
    }
}

/// Whether `screen` draws over the video plane instead of over the menu.
///
/// The patterns the calibration screen measures play on the NDL plane *underneath* the graphics
/// plane, so everything the menu would normally composite behind a card — the sidebar, the grid,
/// the status block, the scrim, the frost pane — has to be left out rather than drawn and covered
/// (see `render::compose`, and `runtime::ui_flow` for the transparent clear that goes with it).
pub(crate) const fn over_video(screen: Screen) -> bool {
    matches!(screen, Screen::HdrCalibration)
}

/// Whether `screen` is a plain list modal: a card holding one `FocusRow` per line, baked into
/// one tile and hit-tested by row index. Same contract as [`is_confirm`] — and the reason it
/// stays exhaustive is that a screen silently missing from a table like this inherits the
/// wrong geometry in silence.
pub(crate) const fn is_list_modal(screen: Screen) -> bool {
    match screen {
        Screen::HostMenu
        | Screen::HostPower
        | Screen::PickProfile
        // A list modal like any other, even though it draws over the video plane rather than
        // over the menu — that difference is `compose_modal`'s, not this family's.
        | Screen::HdrCalibration
        => true,
        Screen::Home
        | Screen::Pairing
        // Settings is a list too, but a scrolling one — see `is_scroll_list`.
        | Screen::AddHost
        | Screen::Wake
        | Screen::ForgetHost
        | Screen::EditHost
        | Screen::About
        | Screen::SpeedTest
        | Screen::SendLogs
        // Collections is a scrolling list too, and its name dialog is a text form.
        | Screen::Collections
        | Screen::RenameCollection
        | Screen::RemoveCollection
        | Screen::ResetHdrCalibration
        | Screen::SettingsPage
        | Screen::RenameProfile
        | Screen::DeleteProfile => false,
    }
}
