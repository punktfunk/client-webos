//! `Screen::PickProfile`: one list of the catalog's profiles, opened for four purposes (plan
//! WP5 / D5): a host's default profile, a one-off "Connect with", a title's binding, and the
//! host's pinned sidebar cards. What a pick does is the purpose's; the list is the same.
use crate::app::nav::ScreenKey;
use crate::app::view::icons;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use crate::ui::widgets::FocusRow;

/// Why the list is up, and what a pick writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProfilePick {
    /// The host's default profile (`KnownHost::profile_id`); "None" is the global document.
    HostDefault { host: String, port: u16 },
    /// Stream the host's desktop once with a profile, binding nothing.
    ConnectWith { host: String, port: u16 },
    /// A title's binding (`KnownHost::games[id].profile`); "None" clears it.
    BindGame { pin_id: String, title: String },
    /// Toggle which profiles the host shows as sidebar cards (`KnownHost::pinned_profiles`).
    Pin { host: String, port: u16 },
}

impl ProfilePick {
    /// Whether the list leads with a "None" row.
    fn has_none(&self) -> bool {
        matches!(self, Self::HostDefault { .. } | Self::BindGame { .. })
    }

    pub(crate) fn title(&self) -> &'static str {
        match self {
            Self::HostDefault { .. } => "Default profile",
            Self::ConnectWith { .. } => "Connect with",
            Self::BindGame { .. } => "Settings profile",
            Self::Pin { .. } => "Pin to sidebar",
        }
    }
}

impl App {
    /// Raises the list. With no profile in the catalog there is nothing to pick from, so the
    /// host and card menus hide the rows that lead here — this is the belt to that brace.
    pub(crate) fn open_pick_profile(&mut self, pick: ProfilePick) {
        if self.profiles.is_empty() && !pick.has_none() {
            return;
        }
        // The cursor lands on the current choice, where there is one.
        let cursor = self
            .pick_current(&pick)
            .and_then(|id| self.profiles.iter().position(|p| p.id == id))
            .map_or(0, |i| i + usize::from(pick.has_none()));
        self.screens.profile_pick = Some(pick);
        self.nav.enter(Screen::PickProfile, cursor);
    }

    /// The id the purpose currently points at, if any.
    fn pick_current(&self, pick: &ProfilePick) -> Option<String> {
        match pick {
            ProfilePick::HostDefault { host, port } => self.known_host(host, *port)?.profile_id.clone(),
            ProfilePick::BindGame { pin_id, .. } => {
                let (host, port) = self.library.selected_host.clone()?;
                self.known_host(&host, port)?.game_profile(pin_id).map(str::to_string)
            }
            ProfilePick::ConnectWith { .. } | ProfilePick::Pin { .. } => None,
        }
    }

    /// What the list is for, or nothing when the screen is not up.
    pub(crate) fn profile_pick(&self) -> Option<&ProfilePick> {
        self.screens.profile_pick.as_ref()
    }

    pub(crate) fn pick_profile_subtitle(&self) -> Option<String> {
        Some(match self.profile_pick()? {
            ProfilePick::HostDefault { host, port } | ProfilePick::ConnectWith { host, port } => {
                let name = self.known_host(host, *port).map_or(host.as_str(), |h| h.name.as_str());
                match self.profile_pick()? {
                    ProfilePick::ConnectWith { .. } => format!("Stream {name}'s desktop with these settings once."),
                    _ => format!("What {name} streams with when a title has no profile of its own."),
                }
            }
            ProfilePick::BindGame { title, .. } => format!("The settings {title} streams with."),
            ProfilePick::Pin { host, port } => {
                let name = self.known_host(host, *port).map_or(host.as_str(), |h| h.name.as_str());
                format!("Each pinned profile is a card under {name} in the host list.")
            }
        })
    }

    /// The rows: an optional "None", then every profile. The dot marks the current choice —
    /// one for a default or a binding, one per pinned profile.
    pub(crate) fn pick_profile_rows(&self) -> Vec<FocusRow> {
        let Some(pick) = self.profile_pick() else {
            return Vec::new();
        };
        let current = self.pick_current(pick);
        let pinned: Vec<String> = match pick {
            ProfilePick::Pin { host, port } => self
                .known_host(host, *port)
                .map(|h| h.pinned_profiles.clone())
                .unwrap_or_default(),
            _ => Vec::new(),
        };
        let mut rows = Vec::with_capacity(self.profiles.len() + 1);
        if pick.has_none() {
            let row = FocusRow::action(icons::ICON_CLOSE, "None");
            rows.push(if current.is_none() { row.marked() } else { row });
        }
        for p in &self.profiles {
            let row = FocusRow::action(icons::ICON_WRENCH, p.name.clone());
            let on = current.as_deref() == Some(p.id.as_str()) || pinned.contains(&p.id);
            rows.push(if on { row.marked() } else { row });
        }
        rows
    }

    pub(crate) fn handle_pick_profile_event(&mut self, ev: MenuEvent) {
        if self.list_nav_event(ev) {
            return;
        }
        match ev {
            MenuEvent::Confirm => self.confirm_pick_profile(),
            MenuEvent::Back | MenuEvent::Secondary => self.close_pick_profile(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {}
        }
    }

    fn close_pick_profile(&mut self) {
        let pick = self.screens.profile_pick.take();
        let back = match pick {
            Some(ProfilePick::BindGame { .. }) | None => Screen::Home,
            Some(_) => Screen::HostMenu,
        };
        if back == Screen::Home {
            self.close_card_menu();
        }
        self.nav.resume(back);
    }

    fn confirm_pick_profile(&mut self) {
        let Some(pick) = self.screens.profile_pick.clone() else {
            return;
        };
        let row = self.nav.cursor(ScreenKey::PickProfile);
        let none = pick.has_none() && row == 0;
        let chosen = (!none)
            .then(|| {
                self.profiles
                    .get(row - usize::from(pick.has_none()))
                    .map(|p| p.id.clone())
            })
            .flatten();
        if !none && chosen.is_none() {
            return;
        }
        match pick {
            ProfilePick::HostDefault { host, port } => {
                if let Some(h) = self.known_host_mut(&host, port) {
                    h.profile_id = chosen;
                }
                self.persist();
                self.close_pick_profile();
            }
            ProfilePick::BindGame { pin_id, .. } => {
                if let Some((host, port)) = self.library.selected_host.clone() {
                    if let Some(h) = self.known_host_mut(&host, port) {
                        h.bind_game_profile(&pin_id, chosen.as_deref());
                    }
                }
                self.persist();
                self.close_pick_profile();
            }
            ProfilePick::Pin { host, port } => {
                if let (Some(h), Some(id)) = (self.known_host_mut(&host, port), chosen) {
                    match h.pinned_profiles.iter().position(|p| *p == id) {
                        Some(i) => {
                            h.pinned_profiles.remove(i);
                        }
                        None => h.pinned_profiles.push(id),
                    }
                }
                self.persist();
                self.refresh_entries();
                // Stays up: pinning is a toggle, and the dots show the set.
            }
            ProfilePick::ConnectWith { host, port } => {
                self.screens.profile_pick = None;
                self.screens.host_menu_index = None;
                self.nav.screen = Screen::Home;
                self.connect_desktop_with(&host, port, chosen);
            }
        }
    }
}
