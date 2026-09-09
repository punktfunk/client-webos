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
        self.nav.enter(Screen::EditHost, 0);
    }

    /// Left/Right stand in for backspace; Confirm commits with 4 octets.
    pub(crate) fn handle_edit_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.screens.add_host.backspace(),
            MenuEvent::Right => self.screens.add_host.advance_field(),
            MenuEvent::Confirm => self.confirm_edit_host(),
            MenuEvent::Back => self.close_edit_host(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Rewrite address in-place, keeping identity (fingerprint, `mgmt_port`, MAC). No-op if partial.
    pub(crate) fn confirm_edit_host(&mut self) {
        if !self.screens.add_host.is_complete() {
            return;
        }
        let Some(idx) = self.screens.host_menu_index else {
            return;
        };
        let Some(HostEntry::Known(old)) = self.hosts.entries.get(idx).cloned() else {
            return;
        };
        let (host, port) = self.screens.add_host.host_and_port();
        if host == old.addr && port == old.port {
            self.close_edit_host();
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

        if self.library.selected_host.as_ref() == Some(&(old.addr.clone(), old.port)) {
            self.library.selected_host = Some((host.clone(), port));
        }
        // `rebuild_entries` may have moved the row, and the host menu behind this dialog acts
        // on the index, so both focus and that index follow the host to its new position.
        if let Some(row) = self.hosts.entry_index(&host, port) {
            self.set_home_focus(HomeFocus::Sidebar(row));
            self.screens.host_menu_index = Some(row);
        }
        self.render.grid.dirty = true;
        self.close_edit_host();
    }

    /// Back to the host menu this dialog was opened from — `resume`, not `enter`, so the
    /// cursor stays on the Connect row whose pencil opened it. The latch is
    /// `handle_host_power_event`'s: the menu's subtitle carries the address just changed.
    fn close_edit_host(&mut self) {
        self.latch_host_menu_power();
        self.nav.resume(Screen::HostMenu);
    }
}
