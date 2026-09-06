//! The "host unreachable — wake it?" flow's logic: the Wake-on-LAN prompt, its retry/probe
//! timers, and its send-side plumbing. The per-host auto-send setting the prompt obeys
//! lives in `app::state::hostpower`. Rendering lives in `app::view::wake`.
use crate::app::{App, Screen, WakeState};
use crate::core::event::MenuEvent;
use crate::services::store::KnownHost;
use std::time::Instant;

impl App {
    /// Enters the WOL flow. With `wol_auto` off, shows prompt immediately.
    /// With it on, fires packet silently, shows prompt only after `WAKE_RETRY_INTERVAL`.
    pub(crate) fn start_wake(&mut self, host: String, port: u16, mac: Vec<String>, reason: String) {
        let known = self.hosts.known.iter().find(|h| h.addr == host && h.port == port);
        let name = known.map_or_else(|| host.clone(), |h| h.name.clone());
        // WHY: without a MAC, don't auto-send — show interactive explanation instead. A host
        // the user just powered down never auto-sends either, whatever `wol_auto` says: it is
        // unreachable *because they asked for that*, and waking it would undo the press.
        let auto = known.is_some_and(|h| h.wol_auto)
            && !mac.is_empty()
            && self.hosts.powered_down.as_ref() != Some(&(host.clone(), port));
        let mut wake = WakeState {
            host,
            port,
            name,
            mac,
            reason,
            // Lands on the "Wake host" button — the reason the user is here.
            focused: 0,
            sent: false,
            attempts: 0,
            since: None,
            last_attempt: None,
            silent: auto,
            // Baseline for `WAKE_PROBE_INTERVAL` — the first active probe fires
            // `WAKE_PROBE_INTERVAL` from now, not immediately.
            last_probe: Some(Instant::now()),
            probe_rx: None,
        };
        if auto {
            Self::send_wake(&mut wake);
            // No modal is up in this branch, so the Home bar is the only place the
            // wait is visible at all — without this it would sit on `select_host`'s
            // stale "Loading library…" until the host came back (or didn't).
            self.set_home_status(Some(Self::wake_home_status(&wake)), false);
        } else {
            self.nav.screen = Screen::Wake;
        }
        self.screens.wake = Some(wake);
    }

    /// Sends (or resends) the WOL magic packet, bumping the resend timer.
    pub(crate) fn send_wake(wake: &mut WakeState) {
        // WHY: only mark sent=true if packet actually went out; wake_and_log fails on
        // unparseable MAC or no interface. Avoid showing "Waiting…" for no packet.
        let sent = crate::services::wol::wake_and_log(&wake.mac, wake.host.parse().ok(), &wake.name);
        let now = Instant::now();
        if sent {
            wake.sent = true;
            wake.attempts += 1;
            wake.since.get_or_insert(now);
        } else {
            wake.reason = "Couldn't send the wake signal — no usable MAC address or network interface.".into();
        }
        wake.last_attempt = Some(now);
    }

    /// Advances an in-flight wake: resends WOL every `WAKE_RETRY_INTERVAL`, shows
    /// silent auto-send after that, and probes reachability every `WAKE_PROBE_INTERVAL`.
    /// Runs whether modal is showing or not; `drain_discovery` can also end wake.
    pub fn tick_wake(&mut self) -> bool {
        if self.screens.wake.is_none() {
            return false;
        }
        let now = Instant::now();
        let mut changed = false;
        let mut new_status = None;

        // Taken out of the `wake` borrow before it is used: recording the result touches the
        // rest of `self`, and this is the same "the API answered" evidence `drain_games` folds
        // in — the wake flow just happens to be the one asking.
        let probed = self.screens.wake.as_mut().and_then(|w| {
            let loaded = w.probe_rx.as_ref()?.try_recv().ok()?;
            w.probe_rx = None;
            Some(loaded)
        });
        if let Some(loaded) = probed {
            changed = true;
            self.note_api_result(&loaded.host, loaded.port, &loaded.result);
            if loaded.result.is_ok() {
                let (host, port) = (loaded.host, loaded.port);
                let mgmt_port = self
                    .hosts
                    .known
                    .iter()
                    .find(|h| h.addr == host && h.port == port)
                    .and_then(|h| h.mgmt_port);
                self.wake_succeeded(host, port, mgmt_port, "reachability probe");
                return true;
            }
            if let Some(w) = self.screens.wake.as_mut() {
                w.last_probe = Some(now);
            }
        }
        let Some(wake) = &mut self.screens.wake else {
            return changed;
        };

        // WHY: resend only if wake.sent=true; else retry would fire on first tick
        // before user confirms. First send is start_wake's call (auto) or user's confirm.
        let retry_due = !wake.mac.is_empty()
            && wake.sent
            && wake
                .last_attempt
                .is_some_and(|t| now.duration_since(t) >= crate::app::WAKE_RETRY_INTERVAL);
        // After retry_due, reveal silent wait so user sees it. Only once — re-popping
        // every minute would be nagging.
        let reveal = retry_due && wake.silent;
        if retry_due {
            Self::send_wake(wake);
            wake.silent = false;
            new_status = Some(Self::wake_home_status(wake));
            changed = true;
        }

        if wake.probe_rx.is_none()
            && wake
                .last_probe
                .is_some_and(|t| now.duration_since(t) >= crate::app::WAKE_PROBE_INTERVAL)
        {
            let (host, port) = (wake.host.clone(), wake.port);
            wake.probe_rx = Some(Self::wake_probe(&self.hosts.known, &self.identity, &host, port));
            wake.last_probe = Some(now);
        }
        if reveal {
            self.nav.screen = Screen::Wake;
        }
        if let Some(status) = new_status {
            self.set_home_status(Some(status), false);
        }
        changed
    }

    /// Spawns a reachability probe for (host, port). Associated function (not &self)
    /// so it can run while `tick_wake` holds &mut self.screens.wake.
    pub(crate) fn wake_probe(
        known_hosts: &[KnownHost],
        identity: &(String, String),
        host: &str,
        port: u16,
    ) -> std::sync::mpsc::Receiver<crate::services::library::GamesLoaded> {
        let known = known_hosts.iter().find(|h| h.addr == host && h.port == port);
        let fingerprint = known.and_then(crate::core::model::KnownHost::fingerprint);
        let mgmt_port = known
            .and_then(|h| h.mgmt_port)
            .unwrap_or(crate::services::library::DEFAULT_MGMT_PORT);
        crate::services::library::load_games_async(
            host.to_string(),
            port,
            mgmt_port,
            identity.clone(),
            fingerprint,
            crate::services::budget::PROBE,
        )
    }

    /// Handles Wake modal events: direction moves between "Wake"/"Cancel" buttons.
    /// Confirm sends and closes the modal, or cancels. Back dismisses it (wake runs on in bg).
    pub fn handle_wake_event(&mut self, ev: MenuEvent) {
        let Some(wake) = self.screens.wake.as_mut() else { return };
        // WHY: no MAC = no send/automate possible. Every event but Back is no-op.
        if wake.mac.is_empty() && ev != MenuEvent::Back {
            return;
        }
        if ev == MenuEvent::Back {
            self.close_wake(false);
            return;
        }
        match ev {
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {
                wake.focused = usize::from(wake.focused == 0);
                self.render.modal.focus_anim = Some(Instant::now());
            }
            MenuEvent::Confirm if wake.focused == 0 => {
                Self::send_wake(wake);
                // Hand the wait to the grid's spinner; `tick_wake` re-pops the modal if the
                // host is still down after `WAKE_RETRY_INTERVAL`. A packet that never went
                // out keeps the modal up, since it carries the error text.
                if wake.sent {
                    self.close_wake(true);
                    self.render.grid.reveal.restart();
                }
            }
            MenuEvent::Confirm => self.close_wake(false),
            MenuEvent::Back | MenuEvent::Secondary => {}
        }
    }

    /// Closes Wake modal. Sent wakes keep running in background (timers bring host back).
    /// Unsent wakes drop entirely, leaving error text behind.
    ///
    /// `keep_waiting` sets `silent` so `tick_wake` may re-pop the prompt (pressing "Wake host"),
    /// vs clearing it so it never comes back on its own (dismissing).
    fn close_wake(&mut self, keep_waiting: bool) {
        self.nav.screen = Screen::Home;
        let status = match self.screens.wake.as_mut() {
            Some(wake) if wake.sent => {
                wake.silent = keep_waiting;
                Some(Self::wake_home_status(wake))
            }
            _ => self.screens.wake.take().map(|w| w.reason),
        };
        self.set_home_status(status, false);
    }

    /// Home status bar line for background wake (auto-send or dismissed modal).
    /// Must stand alone; modal version sits under "Waking host…" title.
    pub(crate) fn wake_home_status(wake: &WakeState) -> String {
        match wake.attempts {
            0 => wake.reason.clone(),
            1 => format!(
                "Wake signal sent to {} — waiting for it to come back online…",
                wake.name
            ),
            n => format!(
                "Wake signal re-sent to {} ({n} attempts) — still waiting for it to come back online…",
                wake.name
            ),
        }
    }
}
