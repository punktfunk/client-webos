#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Screen {
    Home,
    Pairing,
    AddHost,
    Wake,
    ForgetHost,
    HostMenu,
    EditHost,
    About,
    SpeedTest,
    HostPower,
    /// Measuring the panel's HDR volume against synthetic PQ patterns, reached from
    /// Experimental (see `app/state/hdrcalibration.rs`). Unlike every other list screen it draws
    /// over live video: the patterns play on the NDL plane underneath it.
    HdrCalibration,
    /// "Send logs to developer" confirmation (see `app/sendlogs.rs`).
    SendLogs,
    /// Which collection a held card belongs to (see `app/collections.rs`). A scrolling row
    /// list, since a host may have every one of `MAX_COLLECTIONS` plus Library.
    Collections,
    /// Naming a collection: a new one, or one being renamed (see `app/state/collections.rs`).
    /// One screen, because the two differ only in what they start from and what they commit
    /// to.
    RenameCollection,
    /// "Remove collection?" — its games return to Library (see `app/state/collections.rs`).
    RemoveCollection,
    /// "Clear HDR calibration?" — puts the panel volume back to the shipped default (see
    /// `app/state/hdrcalibration.rs`).
    ResetHdrCalibration,
    /// The desktop page map on the console's row engine (see `app/state/settingspage.rs`):
    /// General, Display, Input, Audio, Controllers, About, in one document scope at a time.
    SettingsPage,
    /// Naming the profile in scope — a text form like the collection's.
    RenameProfile,
    /// "Delete profile?" — warns what falls back to the default settings.
    DeleteProfile,
    /// One list of the catalog's profiles, for a host default, a one-off connect, a title's
    /// binding or the sidebar pins (`app::state::profilepick`).
    PickProfile,
}

/// Pairing modal's focused input: PIN row or "Request access" button.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum PairingFocus {
    #[default]
    Pin,
    RequestAccess,
}

/// Home screen focus location.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HomeFocus {
    Sidebar(usize),
    SidebarMenu(usize),
    Grid(usize),
}
