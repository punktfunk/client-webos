//! The per-host actions menu — logic. Rendering lives in `app::view::hostmenu`.
//!
//! Adding an action here is deliberately two edits and nothing else: a row in
//! [`App::host_menu_actions`] and an arm in [`App::confirm_host_menu_row`]. Everything
//! else — card geometry, the unfocused shell, the focused-row tile, the focus pop — is
//! `ui::widgets::ListModal`'s, shared with any future list screen.
use crate::app::hosts::HostEntry;
use crate::app::nav::ScreenKey;
use crate::app::state::profilepick::ProfilePick;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use crate::services::store::ExitAction;
use crate::ui::widgets::FocusRow;

/// Host action (enum instead of bare index so conditional rows don't silently shift indices).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HostAction {
    Connect,
    Pair,
    SpeedTest,
    /// The power row — wake, or this host's own exit behaviour applied on demand. Which one
    /// it is right now is [`App::host_menu_power_row`].
    Power,
    /// This host's power settings (`Screen::HostPower`): wake automatically, exit behaviour.
    PowerSettings,
    /// Stream the desktop once with a picked profile.
    ConnectWith,
    /// The profile a title with no binding streams with.
    DefaultProfile,
    /// Which profiles are cards under this host in the sidebar.
    Pin,
    Edit,
    Forget,
}

/// Every action the menu can offer — the capacity [`HostActions`] is sized to.
const MAX_HOST_ACTIONS: usize = 10;

/// The actions one host's menu offers, in display order.
///
/// A fixed array rather than a `Vec`: the row count is what the card's height is measured
/// from, so the pointer path asks for this on every Magic Remote `MouseMotion`, and an
/// allocation per motion event is what it cost when the labels came with it.
pub(crate) struct HostActions {
    items: [HostAction; MAX_HOST_ACTIONS],
    len: usize,
}

impl HostActions {
    fn push(&mut self, action: HostAction) {
        self.items[self.len] = action;
        self.len += 1;
    }
}

impl Default for HostActions {
    fn default() -> Self {
        Self {
            items: [HostAction::Connect; MAX_HOST_ACTIONS],
            len: 0,
        }
    }
}

impl std::ops::Deref for HostActions {
    type Target = [HostAction];

    fn deref(&self) -> &[HostAction] {
        &self.items[..self.len]
    }
}

/// One action's row. Split from [`App::host_menu_actions`] so the geometry path can have the
/// list's shape without its labels — every label here is an owned `String` in the `FocusRow`.
/// `paired` is the host's pairing state: the only thing a label varies on.
fn host_menu_row(action: HostAction, paired: bool, power: Option<ExitAction>) -> FocusRow {
    use crate::app::view::icons;
    match action {
        HostAction::Connect if paired => FocusRow::action(icons::ICON_TV, "Connect"),
        // The hint goes in the value column like every other Action row's, rather than
        // being parenthesised into the label.
        HostAction::Connect => FocusRow::action_with_value(icons::ICON_TV, "Connect", "pairs first"),
        HostAction::Pair => FocusRow::action(icons::ICON_LOCK, "Pair with PIN"),
        HostAction::SpeedTest => FocusRow::action(icons::ICON_SIGNAL, "Test network speed"),
        // Plain, never `danger()`: red is this menu's colour for destroying saved state
        // (Forget host), and powering a machine off destroys nothing here.
        HostAction::Power => FocusRow::action(icons::ICON_POWER, power_row_label(power)),
        HostAction::PowerSettings => FocusRow::action(icons::ICON_SETTINGS, "Power settings"),
        HostAction::ConnectWith => FocusRow::action(icons::ICON_PLAY, "Connect with\u{2026}"),
        HostAction::DefaultProfile => FocusRow::action(icons::ICON_WRENCH, "Default profile\u{2026}"),
        HostAction::Pin => FocusRow::action(icons::ICON_PIN, "Pin to sidebar\u{2026}"),
        HostAction::Edit => FocusRow::action(icons::ICON_EDIT, "Edit address"),
        HostAction::Forget => FocusRow::action(icons::ICON_DELETE, "Forget host").danger(),
    }
}

/// What the power row is called, given what it currently does — `None` being a host that is
/// down, which can only be woken.
fn power_row_label(power: Option<ExitAction>) -> &'static str {
    match power {
        None => "Wake host",
        Some(ExitAction::Sleep) => "Put to sleep",
        // `ExitAction::None` never reaches here: `host_menu_power_row` resolves it to
        // `Shutdown`, because a host that is already up has nothing to wake.
        Some(ExitAction::None | ExitAction::Shutdown) => "Shut down",
    }
}

impl App {
    /// The action rows, as the view paints them.
    pub(crate) fn host_menu_rows(&self) -> Vec<FocusRow> {
        let paired = self.host_menu_paired();
        let power = self.host_menu_power_row();
        self.host_menu_actions_with(power)
            .iter()
            .map(|&a| host_menu_row(a, paired, power))
            .collect()
    }

    /// The sidebar entry the host menu (and everything reached from it) is acting on. The one
    /// `host_menu_index` lookup — half a dozen call sites had spelled it out.
    pub(crate) fn host_menu_entry(&self) -> Option<&HostEntry> {
        self.screens.host_menu_index.and_then(|i| self.hosts.entries.get(i))
    }

    /// Whether the menu's host is paired — what half the actions are conditional on, and
    /// the one thing a row label varies on.
    pub(crate) fn host_menu_paired(&self) -> bool {
        self.host_menu_entry().is_some_and(HostEntry::is_paired)
    }

    /// What the power row currently offers: `None` to wake a host that is down, or the power
    /// action to send to one that is up.
    ///
    /// A host that is up has nothing to wake, so the row always offers a way to put it back
    /// down. Which way follows that host's own exit behaviour, with `None` reading as Shut
    /// down — the button has to mean *something*, and shutdown is what "no preference" gets
    /// on the way out of a session too.
    ///
    /// Reads the ambient reachability record (`App::entry_online`) rather than probing: nobody
    /// is waiting on this, and the row it picks is the one the same record already put a dot
    /// next to in the sidebar. A host whose reachability is still unknown counts as down — the
    /// wake row is the safe wrong answer, because waking a host that is already up costs a
    /// magic packet, while shutting one down that we only assumed was up does not undo.
    pub(crate) fn host_menu_power_row(&self) -> Option<ExitAction> {
        self.screens.host_menu_power
    }

    /// Derives what [`host_menu_power_row`](Self::host_menu_power_row) latches.
    fn resolve_power_row(&self) -> Option<ExitAction> {
        let entry = self.host_menu_entry()?;
        if self.entry_online(entry) != Some(true) {
            return None;
        }
        Some(match self.known_host(entry.host(), entry.port())?.exit_action {
            ExitAction::Sleep => ExitAction::Sleep,
            ExitAction::None | ExitAction::Shutdown => ExitAction::Shutdown,
        })
    }

    /// Opens host menu for sidebar row `idx` (⋯ button, pointer, or Right key).
    pub(crate) fn open_host_menu(&mut self, idx: usize) {
        self.screens.host_menu_index = Some(idx);
        self.screens.row_button = None;
        self.latch_host_menu_power();
        self.nav.enter(Screen::HostMenu, 0);
    }

    /// Re-resolves the power row and stores it. Called on every entry to the host menu — the
    /// sidebar opening it, and Back out of this host's power settings, where the exit
    /// behaviour the row names may just have been changed.
    pub(crate) fn latch_host_menu_power(&mut self) {
        self.screens.host_menu_power = self.resolve_power_row();
    }

    /// The actions offered; conditional on host state (saved/discovered, has MAC).
    pub(crate) fn host_menu_actions(&self) -> HostActions {
        self.host_menu_actions_with(self.host_menu_power_row())
    }

    /// [`host_menu_actions`](Self::host_menu_actions) with the power row already resolved —
    /// what the callers that also need the labels use, so the reachability lookup behind it is
    /// paid once per pass rather than once per table.
    pub(crate) fn host_menu_actions_with(&self, power: Option<ExitAction>) -> HostActions {
        let mut actions = HostActions::default();
        let Some(entry) = self.host_menu_entry() else {
            return actions;
        };
        let saved = matches!(entry, HostEntry::Known(_));
        let paired = entry.is_paired();
        actions.push(HostAction::Connect);
        actions.push(HostAction::Pair);
        // Both this and "Wake host" below need a paired host: the probe runs over the real
        // data plane (so it needs the host to accept this client's certificate), and waking a
        // host we can't then connect to is a dead end.
        if paired {
            actions.push(HostAction::SpeedTest);
        }
        // A MAC is what the wake half needs; the power half needs only the management lane, so
        // a host with no MAC still gets the row once it is up.
        if paired && (!entry.mac().is_empty() || power.is_some()) {
            actions.push(HostAction::Power);
        }
        if saved {
            actions.push(HostAction::PowerSettings);
        }
        // The profile rows need a paired, saved host and a catalog with something in it.
        if saved && paired && !self.profiles.is_empty() {
            actions.push(HostAction::ConnectWith);
            actions.push(HostAction::DefaultProfile);
            actions.push(HostAction::Pin);
        }
        if saved {
            actions.push(HostAction::Edit);
            actions.push(HostAction::Forget);
        }
        actions
    }

    /// The host's name — the menu's title.
    pub(crate) fn host_menu_title(&self) -> String {
        self.host_menu_entry()
            .map_or_else(String::new, |e| e.name().to_string())
    }

    /// `address:port` and the pairing state — the menu's subtitle.
    pub(crate) fn host_menu_subtitle(&self) -> String {
        self.host_menu_entry().map_or_else(String::new, |e| {
            let paired = if e.is_paired() { "paired" } else { "not paired" };
            format!("{}:{} · {paired}", e.host(), e.port())
        })
    }

    pub(crate) fn handle_host_menu_event(&mut self, ev: MenuEvent) {
        if self.list_nav_event(ev) {
            return;
        }
        match ev {
            MenuEvent::Confirm => self.confirm_host_menu_row(),
            MenuEvent::Back => {
                self.screens.host_menu_index = None;
                self.nav.screen = Screen::Home;
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right | MenuEvent::Secondary => {}
        }
    }

    /// The power row's action: a magic packet on a host that is down, or this host's own exit
    /// behaviour applied on demand to one that is up. Both close the menu and report progress
    /// on the Home status line, because both take longer than a frame.
    fn confirm_power_row(&mut self, idx: usize) {
        // The latched value, so the press does what the row the user was looking at said.
        let power = self.host_menu_power_row();
        let Some(entry) = self.hosts.entries.get(idx) else {
            return;
        };
        let (host, port) = (entry.host().to_string(), entry.port());
        let mac = entry.mac().to_vec();
        let name = entry.name().to_string();
        self.screens.host_menu_index = None;
        self.nav.screen = Screen::Home;
        let Some(action) = power else {
            self.start_wake(host, port, mac, format!("Waking {name}…"));
            return;
        };
        self.start_power_action(&host, port, action, &name);
    }

    /// Runs focused row's action; every arm navigates away or closes menu.
    pub(crate) fn confirm_host_menu_row(&mut self) {
        let actions = self.host_menu_actions();
        let Some(action) = actions.get(self.nav.cursor(ScreenKey::HostMenu)) else {
            return;
        };
        let Some(idx) = self.screens.host_menu_index else {
            return;
        };
        match action {
            HostAction::Connect => {
                self.screens.host_menu_index = None;
                self.nav.screen = Screen::Home;
                self.confirm_sidebar_host(idx);
            }
            // Straight to the PIN ceremony, even for an already-paired host: re-pairing
            // is the documented recovery when a host's certificate has changed.
            HostAction::Pair => {
                self.screens.host_menu_index = None;
                self.open_pairing(idx);
            }
            HostAction::SpeedTest => self.open_speed_test(idx),
            HostAction::Power => self.confirm_power_row(idx),
            HostAction::PowerSettings => self.open_host_power(),
            HostAction::ConnectWith | HostAction::DefaultProfile | HostAction::Pin => {
                let Some(entry) = self.hosts.entries.get(idx) else {
                    return;
                };
                let (host, port) = (entry.host().to_string(), entry.port());
                let pick = match action {
                    HostAction::ConnectWith => ProfilePick::ConnectWith { host, port },
                    HostAction::DefaultProfile => ProfilePick::HostDefault { host, port },
                    _ => ProfilePick::Pin { host, port },
                };
                self.open_pick_profile(pick);
            }
            HostAction::Edit => self.open_edit_host(idx),
            HostAction::Forget => self.open_forget_host(idx),
        }
    }
}
