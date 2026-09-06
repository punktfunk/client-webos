//! Pairing modal logic — the PIN entry and request-access ceremonies. Rendering lives in
//! `app::view::pairing`.
use crate::app::{App, PairingOutcome};
use crate::core::event::MenuEvent;
use crate::core::screen::{PairingFocus, Screen};
use crate::services::store::{self, KnownHost};
use std::time::Instant;

impl App {
    /// Open pairing modal and reset PIN state.
    pub(crate) fn open_pairing(&mut self, idx: usize) {
        self.screens.pairing_entry = idx;
        self.screens.pin_digits = [0; 4];
        self.screens.pin_digit_index = 0;
        self.screens.pairing_status = None;
        self.nav.screen = Screen::Pairing;
        // Request access is the default: it is the path that always works, whereas the PIN
        // additionally needs the host's pairing page open and armed.
        self.screens.pairing_focus = PairingFocus::RequestAccess;
    }

    /// Handle pairing events (PIN row or Request Access button).
    pub fn handle_pairing_event(&mut self, ev: MenuEvent) {
        if self.screens.pairing_busy {
            // Mid-ceremony, Back cancels (dropping the receiver orphans the
            // worker — its send fails and it exits); everything else is ignored.
            if ev == MenuEvent::Back {
                self.jobs.cancel_pairing();
                self.screens.pairing_busy = false;
                self.screens.pairing_status = None;
                self.nav.screen = Screen::Home;
            }
            return;
        }
        // Back always leaves the modal; Secondary is the "switch pairing method"
        // shortcut — both work from either focus zone.
        match ev {
            MenuEvent::Back => {
                self.nav.screen = Screen::Home;
                return;
            }
            MenuEvent::Secondary => {
                self.screens.pairing_focus = match self.screens.pairing_focus {
                    PairingFocus::Pin => PairingFocus::RequestAccess,
                    PairingFocus::RequestAccess => PairingFocus::Pin,
                };
                self.render.modal.focus_anim = Some(Instant::now());
                return;
            }
            _ => {}
        }
        match self.screens.pairing_focus {
            // The digits sit in a horizontal row: Left/Right move *between* them and
            // Up/Down spin the focused digit's *value* (odometer-style: Up = +1, Down =
            // −1, wrapping 0..=9). Tabbing Right off the last digit drops focus onto the
            // "Request access" button below; `Confirm` submits the PIN.
            PairingFocus::Pin => match ev {
                MenuEvent::Up => {
                    self.screens.pin_digits[self.screens.pin_digit_index] =
                        (self.screens.pin_digits[self.screens.pin_digit_index] + 1) % 10;
                }
                MenuEvent::Down => {
                    self.screens.pin_digits[self.screens.pin_digit_index] =
                        (self.screens.pin_digits[self.screens.pin_digit_index] + 9) % 10;
                }
                MenuEvent::Left => {
                    // Off the left-hand end goes back up to the primary button, so the two
                    // options are reachable from each other without the Secondary key.
                    if self.screens.pin_digit_index > 0 {
                        self.screens.pin_digit_index -= 1;
                    } else {
                        self.screens.pairing_focus = PairingFocus::RequestAccess;
                    }
                    self.render.modal.focus_anim = Some(Instant::now());
                }
                MenuEvent::Right => {
                    // Stops at the last digit — the button is *above* this row now, so
                    // tabbing off the right-hand end no longer corresponds to anything.
                    if self.screens.pin_digit_index + 1 < self.screens.pin_digits.len() {
                        self.screens.pin_digit_index += 1;
                        self.render.modal.focus_anim = Some(Instant::now());
                    }
                }
                MenuEvent::Confirm => self.try_pair(),
                MenuEvent::Back | MenuEvent::Secondary => {} // handled above
            },
            // Left tabs back onto the PIN row; Confirm sends the access request.
            // Down (and Right, which reads the same way on a d-pad here) drops to the PIN
            // row below the "or" rule.
            PairingFocus::RequestAccess => match ev {
                MenuEvent::Down | MenuEvent::Right => {
                    self.screens.pairing_focus = PairingFocus::Pin;
                    self.screens.pin_digit_index = 0;
                    self.render.modal.focus_anim = Some(Instant::now());
                }
                MenuEvent::Confirm => self.try_request_access(),
                MenuEvent::Up | MenuEvent::Left | MenuEvent::Back | MenuEvent::Secondary => {}
            },
        }
    }

    /// No-PIN path: request access (park), then pin fingerprint. 185s timeout.
    pub(crate) fn try_request_access(&mut self) {
        let entry = &self.hosts.entries[self.screens.pairing_entry];
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        let mgmt_port = entry.mgmt_port();
        let mac = entry.mac().to_vec();
        let os = entry.os().to_string();
        self.screens.pairing_busy = true;
        self.screens.pairing_status = Some("Requesting access — approve this TV on the host.".into());
        tracing::info!("requesting access to {host}:{port}");

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.jobs.pairing = Some(rx);
        std::thread::spawn(move || {
            let result =
                crate::session::probe::request_access(&host, port, identity, crate::services::budget::HOST_WAIT)
                    .map_err(|e| crate::core::errors::friendly(&e));
            let _ = tx.send(PairingOutcome {
                host,
                port,
                name,
                mgmt_port,
                mac,
                os,
                result,
            });
        });
    }

    /// Drain finished pairing; persist on success, show error on failure.
    pub fn drain_pairing(&mut self) -> bool {
        let Some(rx) = &self.jobs.pairing else { return false };
        let Ok(outcome) = rx.try_recv() else { return false };
        self.jobs.pairing = None;
        self.screens.pairing_busy = false;
        match outcome.result {
            Ok(fingerprint) => {
                tracing::info!("paired ok ({}:{})", outcome.host, outcome.port);
                // Only reaches a genuinely new host — `upsert_known_host` keeps an existing
                // record's pins and wol_auto.
                let mut record = KnownHost {
                    shared: pf_client_core::trust::KnownHost {
                        name: outcome.name,
                        addr: outcome.host.clone(),
                        port: outcome.port,
                        mgmt_port: outcome.mgmt_port,
                        mac: outcome.mac,
                        os: outcome.os,
                        ..Default::default()
                    },
                    ..KnownHost::default()
                };
                record.set_fingerprint(fingerprint);
                store::upsert_known_host(&mut self.hosts.known, record);
                self.persist();
                self.rebuild_entries();
                self.nav.screen = Screen::Home;
                self.select_host(outcome.host, outcome.port, outcome.mgmt_port);
            }
            Err(e) => {
                tracing::warn!("pairing/request failed: {e}");
                self.screens.pairing_status = Some(e);
            }
        }
        true
    }

    /// Number button entry; auto-advances like phone PIN pad.
    pub fn enter_pin_digit(&mut self, digit: u8) {
        if self.screens.pairing_busy {
            return;
        }
        // A typed digit is unambiguously PIN input — pull focus back off the
        // Request-access button so it lands in the digit row (and can't
        // accidentally auto-submit the no-PIN path instead).
        self.screens.pairing_focus = PairingFocus::Pin;
        self.screens.pin_digits[self.screens.pin_digit_index] = digit;
        if self.screens.pin_digit_index + 1 < self.screens.pin_digits.len() {
            self.screens.pin_digit_index += 1;
        } else {
            self.try_pair();
        }
    }

    /// Undoes the last typed digit: `enter_pin_digit` writes then advances, so erasing
    /// means stepping back onto the digit just written and clearing it. The row is a
    /// fixed four-box odometer, so "clear" is 0 rather than an empty box. `false` when
    /// there is nothing left to undo — an all-zero PIN, or a ceremony already in flight
    /// (whose only meaningful key is the Back that cancels it).
    pub(crate) fn erase_pin_digit(&mut self) -> bool {
        // Not `pin_digits == [0; 4]`: a typed leading 0 is still an erasable digit.
        if self.screens.pairing_busy || (self.screens.pin_digit_index == 0 && self.screens.pin_digits[0] == 0) {
            return false;
        }
        self.screens.pairing_focus = PairingFocus::Pin;
        if self.screens.pin_digits[self.screens.pin_digit_index] == 0 && self.screens.pin_digit_index > 0 {
            self.screens.pin_digit_index -= 1;
            self.render.modal.focus_anim = Some(Instant::now());
        }
        self.screens.pin_digits[self.screens.pin_digit_index] = 0;
        true
    }

    /// Start PIN pairing on background thread (30s timeout).
    pub(crate) fn try_pair(&mut self) {
        let entry = &self.hosts.entries[self.screens.pairing_entry];
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        let mgmt_port = entry.mgmt_port();
        let mac = entry.mac().to_vec();
        let os = entry.os().to_string();
        let pin: String = self
            .screens
            .pin_digits
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        self.screens.pairing_busy = true;
        self.screens.pairing_status = Some("Pairing — confirm the PIN on the host.".into());
        tracing::info!("pairing with {host}:{port} (pin len {})", pin.len());

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.jobs.pairing = Some(rx);
        std::thread::spawn(move || {
            let result = punktfunk_core::client::NativeClient::pair(
                &host,
                port,
                (&identity.0, &identity.1),
                &pin,
                "webOS TV",
                // A handshake gated on a human walking to their PC, so it gets the long
                // host-wait budget rather than the old 30 s.
                crate::services::budget::HOST_WAIT,
            )
            .map_err(|e| crate::core::errors::pair_message(&e));
            // Send failing just means the user backed out and the receiver is
            // gone — nothing to deliver to.
            let _ = tx.send(PairingOutcome {
                host,
                port,
                name,
                mgmt_port,
                mac,
                os,
                result,
            });
        });
    }
}
