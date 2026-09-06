//! Editing a saved host's address (reuses add-host widget) — logic. Fingerprint survives
//! address changes unchanged since it identifies the certificate, not the network
//! location. Rendering lives in `app::view::edithost`.
use crate::app::hosts::HostEntry;
use crate::app::state::textfield::TextField;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::{HomeFocus, Screen};
use crate::services::store;

impl App {
    /// Open `EditHost` for sidebar row; pre-filled with current address. No-op for unsaved entries.
    pub(crate) fn open_edit_host(&mut self, idx: usize) {
        let Some(HostEntry::Known(h)) = self.hosts.entries.get(idx) else {
            return;
        };
        self.screens.add_host = TextField::from_host_port(&h.addr, h.port);
        self.screens.edit_host_index = Some(idx);
        self.screens.host_menu_index = None;
        self.nav.screen = Screen::EditHost;
    }

    /// Handle menu event. Left/Right stand in for backspace; Confirm commits with 4 octets.
    pub(crate) fn handle_edit_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.screens.add_host.backspace(),
            MenuEvent::Right => self.screens.add_host.advance_field(),
            MenuEvent::Confirm => self.confirm_edit_host(),
            MenuEvent::Back => {
                self.screens.edit_host_index = None;
                self.nav.screen = Screen::Home;
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Rewrite address in-place, keeping identity (fingerprint, `mgmt_port`, MAC). No-op if partial.
    pub(crate) fn confirm_edit_host(&mut self) {
        if !self.screens.add_host.is_complete() {
            return;
        }
        let Some(idx) = self.screens.edit_host_index else {
            return;
        };
        let Some(HostEntry::Known(old)) = self.hosts.entries.get(idx).cloned() else {
            return;
        };
        let (host, port) = self.screens.add_host.host_and_port();
        if host == old.addr && port == old.port {
            self.screens.edit_host_index = None;
            self.nav.screen = Screen::Home;
            return;
        }

        // Drop old record before upsert to avoid stale entry (upsert_known_host keys on (host, port))
        self.hosts.known.retain(|k| !(k.addr == old.addr && k.port == old.port));
        store::upsert_known_host(
            &mut self.hosts.known,
            store::KnownHost {
                shared: pf_client_core::trust::KnownHost {
                    addr: host.clone(),
                    port,
                    ..old.shared.clone()
                },
                ..old.clone()
            },
        );
        // The address is the cache key, so the old one's art is now orphaned.
        crate::services::art::reconcile_host_caches(&self.hosts.known);
        self.persist();
        self.rebuild_entries();

        // Keep selection updated to new address
        if self.library.selected_host.as_ref() == Some(&(old.addr.clone(), old.port)) {
            self.library.selected_host = Some((host.clone(), port));
        }
        self.set_home_focus(HomeFocus::Sidebar(
            self.hosts
                .entries
                .iter()
                .position(|e| e.host() == host && e.port() == port)
                .unwrap_or(0),
        ));
        self.screens.edit_host_index = None;
        self.render.grid.dirty = true;
        self.nav.screen = Screen::Home;
    }
}
