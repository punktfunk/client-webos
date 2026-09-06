//! The profile catalog's editor (planning `design/webos-pointer-ui-overhaul.md` D5): new, rename, duplicate,
//! delete, and the card menu's shortcut that opens a title's bound profile in profile scope,
//! creating it first. The catalog is the shared one every client reads; a title binds to a
//! profile by id on its host record.

use pf_client_core::profiles::StreamProfile;

use crate::app::state::settingspage::{Page, Scope};
use crate::app::state::textfield::TextField;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::model::MAX_COLLECTION_NAME;
use crate::core::screen::Screen;

/// A name that is not yet in the catalog: the title itself, or the title with a counter.
fn unique_name(catalog: &[StreamProfile], wanted: &str) -> String {
    let taken = |name: &str| catalog.iter().any(|p| p.name == name);
    if !taken(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|n| format!("{wanted} {n}"))
        .find(|name| !taken(name))
        .expect("an unbounded counter finds a free name")
}

impl App {
    /// Editing ▸ New profile…: an empty overlay under a placeholder name, opened for editing.
    pub(crate) fn new_profile(&mut self) {
        let profile = StreamProfile::new(unique_name(&self.profiles, "New profile"));
        let id = profile.id.clone();
        self.profiles.push(profile);
        self.persist();
        self.screens.settings_page.scope = Scope::Profile(id);
        self.open_rename_profile();
    }

    /// The card menu's "Game settings": the title's bound profile in profile scope, created
    /// under the title's own name if it has none yet — the branch's first-edit shortcut kept.
    pub(crate) fn open_game_profile(&mut self, pin_id: &str, title: &str) {
        let Some((host, port)) = self.library.selected_host.clone() else {
            return;
        };
        let bound = self
            .known_host(&host, port)
            .and_then(|h| h.game_profile(pin_id))
            .filter(|id| self.profiles.iter().any(|p| p.id == *id))
            .map(str::to_string);
        let id = match bound {
            Some(id) => id,
            None => {
                let profile = StreamProfile::new(unique_name(&self.profiles, title));
                let id = profile.id.clone();
                self.profiles.push(profile);
                if let Some(h) = self.known_host_mut(&host, port) {
                    h.bind_game_profile(pin_id, Some(&id));
                }
                self.persist();
                id
            }
        };
        self.screens.settings_page.scope = Scope::Profile(id);
        self.screens.settings_page.page = Page::Display;
        self.open_settings_page();
    }

    pub(crate) fn open_rename_profile(&mut self) {
        let Scope::Profile(id) = &self.screens.settings_page.scope else {
            return;
        };
        let name = self
            .profiles
            .iter()
            .find(|p| &p.id == id)
            .map_or(String::new(), |p| p.name.clone());
        self.screens.profile_name = TextField::name(MAX_COLLECTION_NAME, &name);
        self.nav.screen = Screen::RenameProfile;
    }

    pub(crate) fn handle_rename_profile_event(&mut self, ev: MenuEvent) {
        match ev {
            MenuEvent::Left => self.screens.profile_name.backspace(),
            MenuEvent::Confirm => self.confirm_rename_profile(),
            MenuEvent::Back | MenuEvent::Secondary => self.nav.resume(Screen::SettingsPage),
            MenuEvent::Right | MenuEvent::Up | MenuEvent::Down => {}
        }
    }

    pub(crate) fn enter_profile_name_char(&mut self, c: char) {
        self.screens.profile_name.enter_char(c);
    }

    /// Why the typed name is refused, `None` while it is fine.
    pub(crate) fn profile_name_hint(&self) -> Option<&'static str> {
        let name = self.screens.profile_name.text().trim();
        if name.is_empty() {
            return Some("A profile needs a name");
        }
        let Scope::Profile(id) = &self.screens.settings_page.scope else {
            return None;
        };
        self.profiles
            .iter()
            .any(|p| &p.id != id && p.name == name)
            .then_some("Another profile has that name")
    }

    fn confirm_rename_profile(&mut self) {
        if self.profile_name_hint().is_some() {
            return;
        }
        let name = self.screens.profile_name.text().trim().to_string();
        if let Scope::Profile(id) = self.screens.settings_page.scope.clone() {
            if let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) {
                p.name = name;
                self.persist();
            }
        }
        self.nav.resume(Screen::SettingsPage);
    }

    pub(crate) fn duplicate_profile(&mut self) {
        let Scope::Profile(id) = self.screens.settings_page.scope.clone() else {
            return;
        };
        let Some(source) = self.profiles.iter().find(|p| p.id == id).cloned() else {
            return;
        };
        let mut copy = StreamProfile::new(unique_name(&self.profiles, &format!("{} copy", source.name)));
        copy.overrides = source.overrides;
        let new_id = copy.id.clone();
        self.profiles.push(copy);
        self.persist();
        self.screens.settings_page.scope = Scope::Profile(new_id);
    }

    /// How many host defaults and title bindings name the profile in scope: what Delete warns
    /// will fall back to the default settings.
    pub(crate) fn profile_use_counts(&self) -> (usize, usize) {
        let Scope::Profile(id) = &self.screens.settings_page.scope else {
            return (0, 0);
        };
        let hosts = self
            .hosts
            .known
            .iter()
            .filter(|h| h.profile_id.as_deref() == Some(id))
            .count();
        let titles = self
            .hosts
            .known
            .iter()
            .flat_map(|h| h.game_profiles.values())
            .filter(|p| p.as_str() == id)
            .count();
        (hosts, titles)
    }

    pub(crate) fn open_delete_profile(&mut self) {
        if matches!(self.screens.settings_page.scope, Scope::Profile(_)) {
            self.nav.enter(Screen::DeleteProfile, 1);
        }
    }

    pub(crate) fn handle_delete_profile_event(&mut self, ev: MenuEvent) {
        if self.confirm_nav_event(ev) {
            return;
        }
        match ev {
            MenuEvent::Confirm if self.nav.cursor(crate::app::nav::ScreenKey::DeleteProfile) == 0 => {
                self.delete_profile();
                self.nav.resume(Screen::SettingsPage);
            }
            MenuEvent::Confirm | MenuEvent::Back | MenuEvent::Secondary => self.nav.resume(Screen::SettingsPage),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {}
        }
    }

    /// Drop the profile in scope and every binding that named it; the scope falls back to the
    /// global document.
    fn delete_profile(&mut self) {
        let Scope::Profile(id) = self.screens.settings_page.scope.clone() else {
            return;
        };
        self.profiles.retain(|p| p.id != id);
        for h in &mut self.hosts.known {
            if h.profile_id.as_deref() == Some(&id) {
                h.profile_id = None;
            }
            h.game_profiles.retain(|_, p| *p != id);
        }
        self.screens.settings_page.scope = Scope::Global;
        self.persist();
    }

    /// Whether `pin_id` on the selected host is bound to a profile that still exists — the
    /// card's dot.
    pub(crate) fn game_is_bound(&self, pin_id: &str) -> bool {
        self.selected_known_host()
            .and_then(|h| h.game_profile(pin_id))
            .is_some_and(|id| self.profiles.iter().any(|p| p.id == id))
    }

    /// The settings one launch runs with — `shared::launch_settings` over this App's document.
    pub(crate) fn launch_settings(
        &self,
        target: &crate::core::model::ConnectTarget,
    ) -> crate::services::store::Settings {
        let state = self.persisted();
        crate::services::store::shared::launch_settings(
            &state,
            &target.host,
            target.port,
            target.launch.as_deref(),
            target.profile.as_deref(),
        )
    }
}
