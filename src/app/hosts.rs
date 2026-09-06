//! The sidebar's host-list model: one entry per known or discovered host.
//!
//! [`HostsState`] holds the list itself plus everything learned about those hosts at runtime —
//! reachability and whether this TV is rooted. Per-host *library* state lives in
//! [`crate::app::library::Library`] instead; this is the part that outlives a host switch.
use crate::services::discovery::DiscoveredHost;
use crate::services::store::KnownHost;

/// One entry in the sidebar's host list — either a fully known/paired host or a
/// freshly discovered (not yet paired) one.
///
/// Not in `core`: it wraps a `services` type from each half of the list, and `core` is a
/// dependency leaf.
#[derive(Clone)]
pub enum HostEntry {
    Known(KnownHost),
    Discovered(DiscoveredHost),
    /// A pinned host+profile card (`KnownHost::pinned_profiles`): under its host, and OK
    /// streams the host's desktop with that profile.
    Pinned {
        host: KnownHost,
        profile_id: String,
        /// "Host · Profile", built once when the rows are.
        label: String,
    },
}

impl HostEntry {
    pub fn name(&self) -> &str {
        match self {
            Self::Known(h) => &h.name,
            Self::Discovered(h) => &h.name,
            Self::Pinned { label, .. } => label,
        }
    }
    /// Whether the row carries the ⋯ actions button: a host does, a pinned card does not.
    pub fn has_menu(&self) -> bool {
        !matches!(self, Self::Pinned { .. })
    }
    pub fn host(&self) -> &str {
        match self {
            Self::Known(h) | Self::Pinned { host: h, .. } => &h.addr,
            Self::Discovered(h) => &h.addr,
        }
    }
    pub fn port(&self) -> u16 {
        match self {
            Self::Known(h) | Self::Pinned { host: h, .. } => h.port,
            Self::Discovered(h) => h.port,
        }
    }
    pub fn is_paired(&self) -> bool {
        matches!(self, Self::Known(h) | Self::Pinned { host: h, .. } if h.is_paired())
    }
    pub fn mgmt_port(&self) -> Option<u16> {
        match self {
            Self::Known(h) | Self::Pinned { host: h, .. } => h.mgmt_port,
            Self::Discovered(h) => h.mgmt_port,
        }
    }
    /// Wake-on-LAN MAC(s) known for this entry so far — empty until it's been seen
    /// advertising its `mac` mDNS TXT at least once (see `discovery::DiscoveredHost::mac`).
    pub fn mac(&self) -> &[String] {
        match self {
            Self::Known(h) | Self::Pinned { host: h, .. } => &h.mac,
            Self::Discovered(h) => &h.mac,
        }
    }
    pub fn os(&self) -> &str {
        match self {
            Self::Known(h) | Self::Pinned { host: h, .. } => &h.os,
            Self::Discovered(h) => &h.os,
        }
    }
}

/// Every host the menu knows about, and what it has learned about them.
#[derive(Default)]
pub(crate) struct HostsState {
    pub(crate) known: Vec<KnownHost>,
    /// The sidebar's rows: known hosts first, then anything discovery has turned up since.
    pub(crate) entries: Vec<HostEntry>,
    /// Last known reachability per `(host, port)` — see `app::state::reach`.
    pub(crate) reachable: std::collections::HashMap<(String, u16), bool>,
    /// When the last reachability sweep ran, so `tick_reachability` can pace itself.
    pub(crate) reach_last: Option<std::time::Instant>,
    /// The host the user has just told to sleep or shut down, if any. While it is set, that
    /// host is left alone: no auto-wake, no library reload — putting a machine down and then
    /// having the client immediately magic-packet it back up is a loop, not a feature.
    ///
    /// Cleared the moment the user picks that host again, which is the explicit "I want it
    /// back" this waits for. Not persisted: a relaunch is that same intent.
    pub(crate) powered_down: Option<(String, u16)>,
    /// Whether this TV is webosbrew-rooted — `None` until `App::start_root_probe` answers.
    pub(crate) rooted: Option<bool>,
}
