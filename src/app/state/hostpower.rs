//! Per-host power management — logic. Rendering lives in `app::view::hostpower`.
use crate::app::menu;
use crate::app::menu::PowerAccess;
use crate::app::nav::ScreenKey;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use crate::services::store::{self, ExitAction};

/// Why a power-rights probe produced no rights. Narrower than the transport error it comes
/// from, because only this much reaches a caption.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeFailure {
    /// The host answered `404`: it predates `/api/v1/actions` entirely.
    Unsupported,
    /// No answer at all.
    Unreachable,
}

impl App {
    /// Open host power settings for host menu's current host.
    pub(crate) fn open_host_power(&mut self) {
        // Re-asked per visit, never persisted: the grant lives in the host's access mask for
        // this device and can be widened or revoked there between visits.
        self.screens.power_rights = None;
        // Dropped before the new probe rather than only overwritten by it: `start_power_probe`
        // can bail (no pairing to ask with), and a previous host's answer landing on this
        // screen would otherwise be read as this host's.
        self.jobs.power_access = None;
        self.start_power_probe();
        self.nav.enter(Screen::HostPower, 0);
    }

    /// Asks the host whether this pairing may drive its power, off-thread. Unlike the root
    /// probe this is a network round trip rather than CPU, so it starts on the open frame —
    /// what it would otherwise block on is the host answering, not this TV.
    ///
    /// Silently does nothing without a pairing to ask with: `power_access` reads that case
    /// straight off the record, so there is nothing for a probe to add.
    fn start_power_probe(&mut self) {
        // Any action builds the same target; `Sleep` just names one so `power_plan` has an
        // id to accept. Nothing is sent — the probe only reads the action list.
        let Some((host, port)) = self.host_power_target() else {
            return;
        };
        let Some(known) = self.host_power_host() else { return };
        let Some(plan) = self.power_plan(known, ExitAction::Sleep) else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        match std::thread::Builder::new().name("power-probe".into()).spawn(move || {
            let _ = tx.send(plan.probe_rights());
        }) {
            Ok(_) => self.jobs.power_access = Some(crate::app::jobs::PowerProbeJob { host, port, rx }),
            // Nothing will ever answer, so settle rather than leave the row on its checking
            // caption forever.
            Err(e) => {
                tracing::warn!("power probe thread: {e}");
                self.screens.power_rights = Some(Err(ProbeFailure::Unreachable));
            }
        }
    }

    /// Sends one power action to a host that is up, off-thread, and puts the outcome on the
    /// Home status line. The host menu's power row is the only caller.
    ///
    /// Fire-and-report rather than fire-and-forget (which is all the app-exit path can do):
    /// there is still a screen here, and "nothing happened" is indistinguishable from a
    /// refusal without one — most usefully when the pairing simply has no Host power grant.
    pub(crate) fn start_power_action(&mut self, host: &str, port: u16, action: ExitAction, name: &str) {
        let Some(known) = self.known_host(host, port) else {
            return;
        };
        let Some(plan) = self.power_plan(known, action) else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        match std::thread::Builder::new().name("power-action".into()).spawn(move || {
            let _ = tx.send(plan.send());
        }) {
            Ok(_) => {
                self.jobs.power_action = Some(crate::app::jobs::PowerActionJob {
                    host: host.to_string(),
                    port,
                    action,
                    rx,
                });
                self.leave_powered_down(host, port);
                self.set_home_status(Some(crate::core::errors::power_pending_message(action, name)), false);
            }
            Err(e) => {
                tracing::warn!("power action thread: {e}");
                self.set_home_status(Some(format!("Couldn't reach {name}")), true);
            }
        }
    }

    /// Lets go of a host the user has just told to power down: drops it as the selected host
    /// (which clears the grid, its art and any in-flight fetch) and marks it so nothing tries
    /// to bring it back.
    ///
    /// Both halves matter. Leaving it selected means the next library fetch fails against a
    /// machine that is deliberately off, which opens the Wake dialog and — with `wol_auto` on
    /// — magic-packets it straight back up, undoing the press. Marking it is what
    /// `start_wake` and `select_host` read to tell "it went away" from "the user put it away".
    fn leave_powered_down(&mut self, host: &str, port: u16) {
        if self.library.selected_host.as_ref() == Some(&(host.to_string(), port)) {
            self.clear_selected_host();
        }
        // Its dot goes out now rather than at the next sweep; the 202 is as much confirmation
        // as this side ever gets.
        self.note_reachable(host, port, false);
        self.hosts.powered_down = Some((host.to_string(), port));
    }

    /// Reports a finished power action on the Home status line. The success wording is
    /// deliberately "asked": the host replies `202` and only then ends sessions and acts, so
    /// there is nothing here that has watched it actually go down.
    pub(crate) fn drain_power_action(&mut self) -> bool {
        let Some(job) = &self.jobs.power_action else {
            return false;
        };
        let (action, host, port) = (job.action, job.host.clone(), job.port);
        let rx = &job.rx;
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(
                crate::services::library::LibraryError::Unreachable("power action thread died".into()),
            ),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
        };
        self.jobs.power_action = None;
        match result {
            Ok(()) => {
                // The dot went out at the press (`leave_powered_down`); nothing to revise.
                self.set_home_status(Some(crate::core::errors::power_accepted_message(action)), true);
            }
            Err(e) => {
                // The disconnect was optimistic — the press acts before the host has replied —
                // so a refusal has to give the host back. The selection is not restored (that
                // is one press on the sidebar, and the status line below says what happened),
                // but the suppression must go: nothing is powering down, so nothing should be
                // left holding off auto-wake.
                if self.hosts.powered_down.as_ref() == Some(&(host.clone(), port)) {
                    self.hosts.powered_down = None;
                }
                // A refusal is the host answering; only a transport failure is silence.
                let online = !crate::app::state::reach::api_error_is_offline(&e);
                self.note_reachable(&host, port, online);
                self.set_home_status(Some(crate::app::view::hostpower::refusal_message(&e)), true);
            }
        }
        true
    }

    /// Picks up the probe's verdict, unlocking the exit-behaviour row (or explaining why not).
    /// Reports whether anything changed, so the open screen redraws.
    pub(crate) fn drain_power_access(&mut self) -> bool {
        let Some(job) = &self.jobs.power_access else {
            return false;
        };
        let (host, port) = (job.host.clone(), job.port);
        let rights = match job.rx.try_recv() {
            Ok(rights) => rights.map_err(|e| match e {
                // A reply, just not one this build's route exists in — the host is up and
                // there is no grant anywhere to widen.
                crate::services::library::LibraryError::Http(404) => ProbeFailure::Unsupported,
                _ => ProbeFailure::Unreachable,
            }),
            // A probe thread that died without sending would otherwise leave the row checking
            // forever, so a dead channel settles like a failed spawn.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(ProbeFailure::Unreachable),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
        };
        self.jobs.power_access = None;
        // Anything but silence is the host answering, a 404 included — it is a reply, and the
        // sweep's question is only whether the box is there.
        if rights != Err(ProbeFailure::Unreachable) {
            self.note_reachable(&host, port, true);
        }
        // Only the host that was asked may claim the answer: the screen can have moved on to
        // another one while the probe was out.
        if self.host_power_target() == Some((host, port)) {
            self.screens.power_rights = Some(rights);
        }
        true
    }

    /// This host's stored exit behaviour, defaulting for a discovered-only host with no record
    /// (which is also what an unchangeable row shows).
    pub(crate) fn host_power_exit_action(&self) -> ExitAction {
        self.host_power_host().map_or(ExitAction::None, |h| h.exit_action)
    }

    /// The saved record for the host this screen is acting on. `None` for a host that has only
    /// ever been discovered, which is what makes both rows unchangeable on one.
    pub(crate) fn host_power_host(&self) -> Option<&store::KnownHost> {
        let entry = self.host_menu_entry()?;
        self.known_host(entry.host(), entry.port())
    }

    /// `(address, port)` of the host this screen is acting on — the key `note_reachable`
    /// records against.
    fn host_power_target(&self) -> Option<(String, u16)> {
        let entry = self.host_menu_entry()?;
        Some((entry.host().to_string(), entry.port()))
    }

    /// Everything the card draws, resolved from one record lookup. The shell key, the focus
    /// key and the modal itself each want all three, and each used to re-find the host per
    /// value — six scans of the known-host list per frame for one record's worth of state.
    pub(crate) fn host_power_view(&self) -> (bool, ExitAction, PowerAccess) {
        let known = self.host_power_host();
        (
            known.is_some_and(|h| h.wol_auto),
            known.map_or(ExitAction::None, |h| h.exit_action),
            self.power_access_for(known),
        )
    }

    /// What is known about this pairing's power rights — the one predicate behind both the
    /// greyed row and the rejected keypress.
    pub(crate) fn power_access(&self) -> PowerAccess {
        self.power_access_for(self.host_power_host())
    }

    /// [`power_access`](Self::power_access) against an already-resolved record.
    fn power_access_for(&self, known: Option<&store::KnownHost>) -> PowerAccess {
        // An unpaired host has no access mask for a power grant to sit in, and the management
        // lane would refuse the invoke on the certificate alone.
        if !known.is_some_and(crate::core::model::KnownHost::is_paired) {
            return PowerAccess::NotPaired;
        }
        match self.screens.power_rights {
            None => PowerAccess::Unknown,
            Some(Err(ProbeFailure::Unsupported)) => PowerAccess::Unsupported,
            Some(Err(ProbeFailure::Unreachable)) => PowerAccess::Unreachable,
            Some(Ok(rights)) => PowerAccess::Rights(rights),
        }
    }

    /// Wake automatically is a plain Left/Right/Confirm toggle; App exit behaviour opens the
    /// same dropdown picker every settings dropdown uses (its row index is disambiguated from
    /// the other screens' by `self.nav.screen`, see `dropdown_overlay_tile`'s docs). Back
    /// returns to host menu.
    pub(crate) fn handle_host_power_event(&mut self, ev: MenuEvent) {
        if self.list_nav_event(ev) {
            return;
        }
        match (self.nav.cursor(ScreenKey::HostPower), ev) {
            (menu::POWER_ROW_AUTO, MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm) => self.toggle_wol_auto(),
            // A locked row (see `power_access`) rejects the press — the greyed control
            // already says the value is fixed. A pick steps: Confirm and Right forward,
            // Left back, the way the console's own choice rows do.
            (menu::POWER_ROW_EXIT, MenuEvent::Left | MenuEvent::Right | MenuEvent::Confirm)
                if self.power_access().unlocked() =>
            {
                let current = menu::exit_action_current_index(self.host_power_exit_action());
                let next = menu::cycle_index(current, ExitAction::ALL.len(), ev != MenuEvent::Left);
                self.apply_exit_action(next);
            }
            (_, MenuEvent::Back) => {
                // The exit behaviour may just have changed, and it is what the host menu's
                // power row is named after.
                self.latch_host_menu_power();
                self.nav.screen = Screen::HostMenu;
            }
            _ => {}
        }
    }

    /// Flip auto-send flag and persist (discovered-only hosts have no record).
    fn toggle_wol_auto(&mut self) {
        let from = self.host_power_host().is_some_and(|h| h.wol_auto);
        if self.edit_wake_host(|known| known.wol_auto = !from) {
            self.arm_switch_anim(from);
        }
    }

    /// Store an exit-behaviour pick (discovered-only hosts have no record, same as the toggle).
    fn apply_exit_action(&mut self, choice_index: usize) {
        let Some(&action) = ExitAction::ALL.get(choice_index) else {
            return;
        };
        self.edit_wake_host(|known| known.exit_action = action);
    }

    /// Runs `f` against the saved record for the host menu's host and persists. Reports
    /// whether there was one — a host that has only ever been discovered has nothing to edit,
    /// which is why neither of this screen's rows can be changed on one.
    fn edit_wake_host(&mut self, f: impl FnOnce(&mut store::KnownHost)) -> bool {
        let Some((host, port)) = self.host_power_target() else {
            return false;
        };
        let Some(known) = self.known_host_mut(&host, port) else {
            return false;
        };
        f(known);
        self.persist();
        true
    }
}
