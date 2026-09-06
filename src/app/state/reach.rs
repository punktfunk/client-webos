//! Ambient reachability polling for sidebar host rows. Pure logic — no view counterpart.
use crate::app::App;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// How often the whole host list is re-probed. Deliberately slow: this is ambient status, nobody
/// waits on it, and every round is a connection the host logs. The cost of a longer interval is
/// only that the dot lags a host coming up or going down by up to this long.
const REACH_INTERVAL: Duration = Duration::from_secs(180);

/// One host's probe result.
pub(crate) struct Reachability {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) online: bool,
}

/// Whether a management-API failure means the host never answered. Every other error is a
/// reply — `NotPaired` is a 401/403, `Http` carries a status, and `PinMismatch` is a
/// certificate the host presented — so only the transport ones count as the host being down.
/// Same split `handle_library_error` makes when it decides whether Wake-on-LAN would help.
pub(crate) fn api_error_is_offline(e: &crate::services::library::LibraryError) -> bool {
    use crate::services::library::LibraryError;
    matches!(e, LibraryError::Unreachable(_))
}

impl App {
    /// Kick off reachability sweep if one is due and none is in flight.
    pub(crate) fn tick_reachability(&mut self) {
        if self.jobs.reach.is_some() {
            return; // a sweep is still running
        }
        if self.hosts.reach_last.is_some_and(|t| t.elapsed() < REACH_INTERVAL) {
            return;
        }
        let targets: Vec<(String, u16)> = self
            .hosts
            .entries
            .iter()
            .map(|e| (e.host().to_string(), e.port()))
            .collect();
        // Stamped only once there is something to sweep: an empty first tick (the host list
        // still coming in over mDNS) used to arm the interval anyway, so the first real sweep
        // was `REACH_INTERVAL` away instead of immediate.
        if targets.is_empty() {
            return;
        }
        self.hosts.reach_last = Some(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel();
        self.jobs.reach = Some(rx);
        // One thread for the whole sweep, probing sequentially: the host count here is a
        // handful, and a thread per host would spike this SoC's 3 cores for a cosmetic
        // indicator. Each send failing (the receiver replaced by a newer sweep, or the app
        // gone) just ends the sweep early.
        std::thread::spawn(move || {
            for (host, port) in targets {
                let online = punktfunk_core::client::NativeClient::probe(&host, port, crate::services::budget::PROBE);
                if tx.send(Reachability { host, port, online }).is_err() {
                    return;
                }
            }
        });
    }

    /// The one place `hosts.reachable` is written.
    ///
    /// The sweep is the *fallback* source of this, not the only one: a library listing, a wake
    /// probe, an mDNS announce and the host-power probe each prove a host answered (or didn't)
    /// long before the next round comes due. Every one of them reports here, so the sidebar dot
    /// and the host menu's power row never sit behind evidence the app already has.
    ///
    /// Reports whether the recorded state actually moved, which is what a caller folds into its
    /// own "redraw owed".
    pub(crate) fn note_reachable(&mut self, host: &str, port: u16, online: bool) -> bool {
        let key = (host.to_string(), port);
        if self.hosts.reachable.get(&key) == Some(&online) {
            return false;
        }
        self.hosts.reachable.insert(key, online);
        true
    }

    /// Records a management-API outcome as reachability.
    pub(crate) fn note_api_result<T>(
        &mut self,
        host: &str,
        port: u16,
        result: &Result<T, crate::services::library::LibraryError>,
    ) -> bool {
        let online = result.as_ref().err().is_none_or(|e| !api_error_is_offline(e));
        self.note_reachable(host, port, online)
    }

    /// Drain finished probes. Returns true if sidebar changed.
    pub(crate) fn drain_reachability(&mut self) -> bool {
        let Some(rx) = &self.jobs.reach else { return false };
        // Collected before any are applied: `note_reachable` takes `&mut self`, which the
        // receiver borrow above rules out.
        let mut results = Vec::new();
        let mut finished = false;
        loop {
            match rx.try_recv() {
                Ok(r) => results.push(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            self.jobs.reach = None;
        }
        let mut changed = false;
        for r in results {
            changed |= self.note_reachable(&r.host, r.port, r.online);
        }
        changed
    }

    /// Last known reachability (None until first probe).
    pub(crate) fn entry_online(&self, entry: &crate::app::hosts::HostEntry) -> Option<bool> {
        self.hosts
            .reachable
            .get(&(entry.host().to_string(), entry.port()))
            .copied()
    }

    /// Last known reachability of a saved host, by record rather than by sidebar entry —
    /// the exit path has the `KnownHost` and no entry to go with it.
    pub(crate) fn known_host_online(&self, known: &crate::services::store::KnownHost) -> Option<bool> {
        self.hosts.reachable.get(&(known.addr.clone(), known.port)).copied()
    }

    pub(crate) fn new_reachability() -> HashMap<(String, u16), bool> {
        HashMap::new()
    }
}
