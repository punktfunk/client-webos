//! Add-host modal logic. Rendering lives in `app::view::addhost`.
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::{HomeFocus, Screen};
use crate::services::store::{self, KnownHost};

impl App {
    /// Handles menu event on add-host modal. Left/Right stand in for backspace and
    /// "next field" (no dot or colon key on the remote) — Right past the fourth octet
    /// opens the optional port. Confirm once the address is complete.
    pub fn handle_add_host_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.screens.add_host.backspace(),
            MenuEvent::Right => self.screens.add_host.advance_field(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
            MenuEvent::Confirm => self.confirm_add_host(),
            MenuEvent::Back => self.nav.screen = Screen::Home,
        }
    }

    /// Direct digit entry from Magic Remote number buttons.
    pub fn enter_add_host_digit(&mut self, digit: u8) {
        self.screens.add_host.enter_digit(digit);
    }

    /// No-op until all four octets typed; prevents truncated connections.
    pub(crate) fn confirm_add_host(&mut self) {
        if !self.screens.add_host.is_complete() {
            return;
        }
        let (host, port) = self.screens.add_host.host_and_port();
        // Non-default port in the name, so two ports on one address stay tellable apart.
        let name = if port == FIXED_HOST_PORT {
            host.clone()
        } else {
            format!("{host}:{port}")
        };
        store::upsert_known_host(
            &mut self.hosts.known,
            // Only reaches a genuinely new host: `upsert_known_host` keeps an existing record's
            // pins, wol_auto and fingerprint, so re-adding a paired host neither unpairs it
            // nor resets its preferences.
            KnownHost {
                shared: pf_client_core::trust::KnownHost {
                    name,
                    addr: host.clone(),
                    port,
                    ..Default::default()
                },
                ..KnownHost::default()
            },
        );
        self.persist();
        self.rebuild_entries();
        self.set_home_focus(HomeFocus::Sidebar(
            self.hosts
                .entries
                .iter()
                .position(|e| e.host() == host && e.port() == port)
                .unwrap_or(0),
        ));
        self.nav.screen = Screen::Home;
    }
    /// Shared by `AddHost` and `EditHost`.
    pub(crate) fn enter_host_address_char(&mut self, c: char) {
        self.screens.add_host.enter_char(c);
    }
}

/// punktfunk's conventional host port — what a bare address means, so the add-host
/// screen only *has* to ask for an IP; an explicit `:port` suffix overrides it.
pub const FIXED_HOST_PORT: u16 = 9777;
