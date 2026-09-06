//! Pre-stream UI: Home screen (sidebar + game grid) with modals (Pairing/Settings/Add-host).
//! `ui.rs` owns drawing/input-mapping, `store.rs` owns persistence, `discovery.rs` owns mDNS.
//!
//! Per-screen `impl App` blocks are split by concern: `state` (event handling, transitions)
//! and `view` (geometry + draw-list building). Keeping them under `app` lets `ui`/`core`
//! stay dependency leaves — neither reaches back into `App`.
pub(crate) mod assets;
pub(crate) mod draw;
pub(crate) mod grid;
pub(crate) mod hero;
pub(crate) mod hosts;
pub(crate) mod jobs;
pub(crate) mod library;
pub(crate) mod menu;
pub(crate) mod modal;
pub(crate) mod nav;
pub(crate) mod pointer;
pub(crate) mod press;
pub(crate) mod render;
pub(crate) mod screens;
pub(crate) mod settingsui;
pub(crate) mod spinner;
pub(crate) mod state;
pub(crate) mod view;

use std::time::{Duration, Instant};

use crate::ui::render::Rect;
use anyhow::Result;

use crate::app::hosts::HostEntry;
use crate::core::event::MenuEvent;
use crate::core::model;
pub use crate::core::model::ConnectTarget;
pub use crate::core::screen::{HomeFocus, PairingFocus, Screen};
use crate::core::settings::TvSettings;
use crate::services::discovery::Discovery;
use crate::services::library::GameEntry;
use crate::services::store::{self, KnownHost};
use crate::ui;

/// How much a focused grid card grows. Bigger than the modal widgets' pop (they sit
/// in a fixed column where any spill reads as a layout shift); a card has the grid gap
/// around it to grow into.
pub(crate) const CARD_GROWTH: f32 = 0.045;
pub(crate) const LAUNCH_GROWTH: f32 = 3.5;
pub(crate) const CARD_POP: Duration = Duration::from_millis(300);
pub(crate) const CARD_POP_SHRINK: f32 = 0.14;
/// The grid's first appearance after the spinner: one diagonal wave from the top-left corner,
/// scale-free, so the whole screen reads as one surface arriving rather than as a field of
/// individually popping cards. The launch backdrop leaves on the same motion (`app::hero`).
pub(crate) const GRID_REVEAL_WAVE: ui::animation::Wave = ui::animation::Wave {
    span: Duration::from_millis(380),
    fade: Duration::from_millis(420),
};
/// How long a Home status line stays up at full opacity before it fades out. The fade
/// itself is [`OVERLAY_FADE`], the same curve the toast notification leaves on. Every line here is
/// ambient (a load result, a wake report, a launch error) and none of them stay true
/// forever, so the grid gets its bottom edge back instead of keeping stale text.
pub(crate) const HOME_STATUS_LIFETIME: Duration = Duration::from_secs(15);

/// How long a library fetch may run before its progress line is worth putting up — avoids
/// flashing "Loading library…" for one frame on a fast fetch.
pub(crate) const LIBRARY_STATUS_DELAY: Duration = Duration::from_secs(1);
/// Home status bar's vertical padding; box height is fixed at two text rows.
pub(crate) const STATUS_BG_PAD: i32 = 12;

/// WOL packet resend interval; silent-mode timeout before showing prompt.
pub(crate) const WAKE_RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// Reachability recheck interval (independent of WOL timers).
pub(crate) const WAKE_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// Wake-on-LAN flow state: both interactive prompt and silent background wait.
pub struct WakeState {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: String,
    pub(crate) mac: Vec<String>,
    /// Original library error, restored on back-out.
    pub(crate) reason: String,
    pub(crate) focused: usize,
    pub(crate) sent: bool,
    /// Packet count; shown so silent wait visibly progresses.
    pub(crate) attempts: u32,
    pub(crate) since: Option<Instant>,
    pub(crate) last_attempt: Option<Instant>,
    /// `true` while running silently (auto-send before prompt shown).
    pub(crate) silent: bool,
    pub(crate) last_probe: Option<Instant>,
    pub(crate) probe_rx: Option<std::sync::mpsc::Receiver<crate::services::library::GamesLoaded>>,
}

pub struct App {
    pub(crate) nav: nav::Nav,
    pub(crate) jobs: jobs::Jobs,
    pub(crate) library: library::Library,
    pub(crate) hosts: hosts::HostsState,
    pub(crate) settings_ui: settingsui::SettingsUi,
    pub(crate) screens: screens::slots::ScreenSlots,
    pub(crate) render: render::state::RenderState,
    pub(crate) home_focus: HomeFocus,
    pub(crate) home_status: Option<String>,
    /// Must survive library reload (cleared on success, else error disappears after 1s).
    pub(crate) home_status_sticky: bool,
    /// One-shot toast, outbox since App has no overlay handle.
    pub(crate) toast: Option<String>,
    home_status_shown_at: Option<Instant>,
    /// Status line waiting out `LIBRARY_STATUS_DELAY`.
    library_status_due: Option<(Instant, String)>,
    pub(crate) launch_ready: Option<ConnectTarget>,
    pub(crate) launch_anim: Option<Instant>,
    pub(crate) launch_anim_idx: Option<usize>,
    /// Submenu over held card's title strip.
    pub(crate) card_menu: Option<state::cardmenu::CardMenu>,
    /// Intro hint owed on first launch after version bump.
    pub(crate) intro_hint_owed: bool,
    /// Per-host launch history (orders Library section). Cached at startup.
    pub(crate) recents: crate::services::recents::Recents,
    /// Off-thread settings persist.
    pub(crate) state_writer: store::StateWriter,
    /// Detected pad type (meaningful only if `gamepad_type` is Auto).
    pub(crate) detected_gamepad_type: Option<store::GamepadType>,
    /// webOS on-screen keyboard up (moves address form from under panel).
    pub(crate) keyboard_shown: bool,
    pub(crate) identity: (String, String),
    /// The settings-profile catalog ([`store::Persisted::profiles`]). Held rather than re-read
    /// because [`App::persist`] rebuilds the whole document from these fields, so anything not
    /// here is dropped on the next save.
    pub(crate) profiles: Vec<pf_client_core::profiles::StreamProfile>,
    /// Last tick time (for real-time scroll easing, not frame-count based).
    last_tick: Option<Instant>,
    /// The console kit's Geist, for the screens drawn on it (`app::draw`). Owned here
    /// because the pointer hit tests measure with it too, not only the frame; `Rc` so the
    /// frame can borrow it beside a `&mut App`.
    pub(crate) fonts: std::rc::Rc<pf_console_ui::theme::Fonts>,
}

/// What a finished background pairing/request-access ceremony reports back —
/// everything needed to persist the host on success (captured going in, so the
/// worker doesn't need `App` access).
pub(crate) struct PairingOutcome {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: String,
    pub(crate) mgmt_port: Option<u16>,
    pub(crate) mac: Vec<String>,
    pub(crate) os: String,
    /// The host's pinned fingerprint, or a user-displayable error.
    pub(crate) result: Result<[u8; 32], String>,
}

/// The sidebar's saved-host rows.
/// The sidebar's rows from the saved hosts: each host, then one card per profile pinned
/// under it (a pin whose profile has left the catalog is skipped, not shown broken).
fn known_entries(
    known_hosts: &[store::KnownHost],
    profiles: &[pf_client_core::profiles::StreamProfile],
) -> Vec<HostEntry> {
    let mut entries = Vec::with_capacity(known_hosts.len());
    for h in known_hosts {
        entries.push(HostEntry::Known(h.clone()));
        for id in &h.pinned_profiles {
            if let Some(p) = profiles.iter().find(|p| p.id == *id) {
                entries.push(HostEntry::Pinned {
                    host: h.clone(),
                    profile_id: id.clone(),
                    label: format!("{} · {}", h.name, p.name),
                });
            }
        }
    }
    entries
}

impl App {
    // ------------------------------------------------------ what `runtime` may write --
    // The menu owns its own state; these are the four things only the outer loop can know.

    /// The attached pad's type per `gamepad::detect_type`, refreshed on hotplug.
    pub fn set_gamepad_type(&mut self, kind: Option<store::GamepadType>) {
        self.detected_gamepad_type = kind;
    }

    /// Whether webOS's on-screen keyboard is up, polled from `SDL_IsScreenKeyboardShown`.
    pub fn set_keyboard_shown(&mut self, shown: bool) {
        self.keyboard_shown = shown;
    }

    /// Show status line only if fetch still running after `LIBRARY_STATUS_DELAY`.
    pub(crate) fn set_home_status_delayed(&mut self, line: String) {
        self.set_home_status(None, false);
        self.library_status_due = Some((Instant::now(), line));
    }

    /// Set Home status (sticky survives reload, cleared on success). Drops delayed line.
    pub(crate) fn set_home_status(&mut self, status: Option<String>, sticky: bool) {
        self.library_status_due = None;
        self.home_status_shown_at = status.is_some().then(Instant::now);
        self.home_status = status;
        self.home_status_sticky = sticky;
    }

    /// Status line opacity (same clock as toast, so lines leave screen identically).
    pub(crate) fn home_status_alpha(&self) -> Option<f32> {
        ui::fade::hold_alpha(
            self.home_status_shown_at?,
            HOME_STATUS_LIFETIME,
            crate::ui::fade::OVERLAY_FADE,
        )
    }

    /// Queue transient toast (replaces waiting ones; second action before tick = first stale).
    pub(crate) fn toast(&mut self, message: impl Into<String>) {
        self.toast = Some(message.into());
    }

    /// Take queued toast for loop's overlay.
    pub fn take_toast(&mut self) -> Option<String> {
        self.toast.take()
    }

    /// Ends a bitrate-slider drag; the button can only come up on the loop that owns events.
    pub fn end_slider_drag(&mut self) {
        self.settings_ui.slider_drag = false;
    }

    pub fn new(identity: (String, String), fonts: std::rc::Rc<pf_console_ui::theme::Fonts>) -> Self {
        let store::Loaded {
            state: loaded,
            new_build,
        } = store::load();
        // The writer's baseline is the document as loaded, so an unchanged launch never writes.
        let state_writer = store::StateWriter::spawn(loaded.clone());
        let store::Persisted {
            settings,
            known_hosts,
            selected_host,
            version: _,
            profiles,
        } = loaded;
        let entries = known_entries(&known_hosts, &profiles);

        // Catches hosts that left the list while the app was closed (migration, torn document);
        // in-session removals reconcile at their own sites.
        crate::services::art::reconcile_host_caches(&known_hosts);
        let mut app = Self {
            nav: nav::Nav::default(),
            library: library::Library::default(),
            jobs: jobs::Jobs {
                discovery: crate::services::discovery::Discovery::start(),
                ..Default::default()
            },
            settings_ui: settingsui::SettingsUi::new(settings),
            screens: screens::slots::ScreenSlots::default(),
            render: render::state::RenderState::default(),
            hosts: hosts::HostsState {
                known: known_hosts,
                entries,
                reachable: Self::new_reachability(),
                ..Default::default()
            },
            home_focus: HomeFocus::Sidebar(0),
            home_status: None,
            home_status_sticky: false,
            toast: None,
            home_status_shown_at: None,
            library_status_due: None,
            launch_ready: None,
            launch_anim: None,
            launch_anim_idx: None,
            card_menu: None,
            intro_hint_owed: new_build,
            recents: crate::services::recents::Recents::load(),
            state_writer,
            detected_gamepad_type: None,
            keyboard_shown: false,
            identity,
            profiles,
            last_tick: None,
            fonts,
        };
        // Restore the last-active sidebar host (if it's still known and paired)
        // so relaunching the app lands back on its game grid.
        if let Some((host, port)) = selected_host {
            if let Some(h) = app
                .hosts
                .known
                .iter()
                .find(|h| h.addr == host && h.port == port && h.is_paired())
            {
                let (host, port, mgmt_port) = (h.addr.clone(), h.port, h.mgmt_port);
                app.select_host(host, port, mgmt_port);
            }
        }
        // Applies the persisted "Show logs" preference to the otherwise-ephemeral overlay.
        if app.settings_ui.settings.show_logs() {
            crate::runtime::set_log_overlay_enabled(true);
        }
        app
    }

    /// Publish the kit's palette for this frame from the shared document's `ui_palette` —
    /// the same row every gamepad surface reads, so the two UIs cannot differ in colour.
    pub(crate) fn apply_ink(&self) {
        let palette = pf_console_ui::library::palette(&self.settings_ui.settings.ui_palette);
        crate::app::draw::set_current_palette(palette.id);
        pf_console_ui::theme::set_ink(pf_console_ui::theme::Ink::of(palette));
    }

    /// Name of the host whichever host-scoped modal (Forget, Host power settings) is acting on.
    pub(crate) fn host_menu_host_name(&self) -> Option<&str> {
        self.screens
            .host_menu_index
            .and_then(|i| self.hosts.entries.get(i))
            .map(HostEntry::name)
    }

    /// Grid geometry bridges — `view::home` is pure geometry, so these supply the two
    /// pieces of live state (the section shape and the scroll offset) it takes.
    pub(crate) fn unscrolled_card_rect(&self, idx: usize, columns: usize, grid_x: i32, available_w: u32) -> Rect {
        view::home::unscrolled_card_rect(idx, grid_x, available_w, self.library.layout(columns))
    }

    pub(crate) fn scrolled_card_rect(&self, idx: usize, columns: usize, grid_x: i32, available_w: u32) -> Rect {
        view::home::scrolled_card_rect(
            idx,
            grid_x,
            available_w,
            self.library.layout(columns),
            self.render.grid.scroll,
        )
    }

    /// The grid card under the pointer, if any — see `view::home::card_at_point`, which
    /// honours the section headings and the gap that push the library block down on screen (a
    /// test against the bare grid rect picked the row above).
    pub(crate) fn hit_test_grid_card(&self, x: i32, y: i32, columns: usize, available_w: u32) -> Option<usize> {
        let grid_x = ui::widgets::SIDEBAR_W as i32;
        if x < grid_x {
            return None;
        }
        view::home::card_at_point(
            grid_x,
            available_w,
            self.library.layout(columns),
            (x, y + self.render.grid.scroll),
        )
    }

    /// Rebuilds the sidebar from `known_hosts`, dropping any discovered-but-unsaved rows. Every
    /// caller that mutates `known_hosts` goes through this rather than collecting the list
    /// itself, so no site has to remember to re-anchor focus.
    pub(crate) fn refresh_entries(&mut self) {
        self.rebuild_entries();
    }

    pub(crate) fn rebuild_entries(&mut self) {
        self.set_entries(known_entries(&self.hosts.known, &self.profiles));
    }

    /// The one place the sidebar row list is replaced: keeps focus on the row the user is on and
    /// marks the layer dirty, neither of which any caller should have to remember.
    fn set_entries(&mut self, entries: Vec<HostEntry>) {
        let before = self.hosts.entries.len();
        self.hosts.entries = entries;
        self.reanchor_sidebar_focus(before);
    }

    /// Keeps sidebar focus on the row the user is actually on after the host list changed
    /// length (`before` is what it was). Focus is a flat index over hosts + "Add host" +
    /// "Settings", and the two utility rows are identified purely by their index — see
    /// `compose_sidebar_focus`, which only draws the bottom-pinned highlight for
    /// `entries.len() + 1` — so leaving a stale index there puts "Settings" mid-list.
    fn reanchor_sidebar_focus(&mut self, before: usize) {
        let now = self.hosts.entries.len();
        let (HomeFocus::Sidebar(i) | HomeFocus::SidebarMenu(i)) = self.home_focus else {
            return; // grid focus doesn't index the sidebar
        };
        if now == before {
            return;
        }
        // A ⋯ belongs to a host row, so it survives only while that row does.
        if matches!(self.home_focus, HomeFocus::SidebarMenu(_)) && i < now {
            return;
        }
        // Past the hosts are the two utility rows, which move with the list's length; a host
        // index only needs clamping into what's left.
        let i = if i >= before { i - before + now } else { i.min(now) };
        // Content reanchoring preserves focus identity; it is not an interactive move.
        self.home_focus = HomeFocus::Sidebar(i);
    }

    /// Whether `addr:port` already has a sidebar row, saved or merely discovered.
    pub(crate) fn host_listed(&self, addr: &str, port: u16) -> bool {
        self.hosts.known.iter().any(|h| h.addr == addr && h.port == port)
            || self
                .hosts
                .entries
                .iter()
                .any(|e| matches!(e, HostEntry::Discovered(d) if d.addr == addr && d.port == port))
    }

    /// Merges freshly-discovered hosts into the entry list (known hosts keep their
    /// paired status; a discovered host not yet known gets appended), learns each
    /// known host's OS and Wake-on-LAN MACs from its live advert, and — if a wake is in
    /// flight (`self.screens.wake`) — notices when the
    /// waking host reappears on mDNS and reconnects. Returns whether the sidebar
    /// actually changed — `main.rs`'s render loop uses this to skip a redraw when a
    /// discovery tick found nothing new (see its dirty-flag docs).
    pub fn drain_discovery(&mut self) -> bool {
        let before = self.hosts.entries.len();
        let mut sidebar_changed = false;
        let mut host_metadata_changed = false;
        let mut grid_changed = false;
        let mut woke = None;
        // `found.addr` throughout this loop is deliberate, not a typo for a nonexistent
        // `found.host` — `DiscoveredHost` (discovery.rs) only has `addr`, `WakeState`/
        // `KnownHost` only have `host`; both hold the same kind of value (network address).
        let polled = self.jobs.discovery.as_mut().map(Discovery::poll).unwrap_or_default();
        let selected = self.library.selected_host.clone();
        for found in polled {
            // An announce is the host saying it is up, on its own initiative — the cheapest
            // liveness evidence there is, and previously the one the dot ignored.
            sidebar_changed |= self.note_reachable(&found.addr, found.port, true);
            #[allow(clippy::suspicious_operation_groupings)]
            if let Some(w) = &self.screens.wake {
                if found.addr == w.host && found.port == w.port {
                    woke = Some((found.addr.clone(), found.port, found.mgmt_port));
                }
            }
            #[allow(clippy::suspicious_operation_groupings)]
            let known = self
                .hosts
                .known
                .iter_mut()
                .find(|h| h.addr == found.addr && h.port == found.port);
            if let Some(known) = known {
                if !found.mac.is_empty() && known.mac != found.mac {
                    known.mac.clone_from(&found.mac);
                    host_metadata_changed = true;
                }
                if !found.os.is_empty() && known.os != found.os {
                    known.os.clone_from(&found.os);
                    host_metadata_changed = true;
                    // The Desktop card wears this host's OS mark, so a newly-learned OS
                    // restamps the entry and re-rasters the one card that shows it.
                    if selected
                        .as_ref()
                        .is_some_and(|(h, p)| h == &found.addr && *p == found.port)
                    {
                        self.library.set_desktop_icon(&found.os);
                        self.render.grid.cards_dirty.push(model::DESKTOP_PIN_ID.to_string());
                        grid_changed = true;
                    }
                }
            }
            if !self.host_listed(&found.addr, found.port) {
                self.hosts.entries.push(HostEntry::Discovered(found));
                sidebar_changed = true;
            }
        }
        if host_metadata_changed {
            self.persist();
        }
        if let Some((host, port, mgmt_port)) = woke {
            self.wake_succeeded(host, port, mgmt_port, "mDNS");
            sidebar_changed = true;
        }
        if sidebar_changed {
            // Rows were appended, so the utility rows have moved.
            self.reanchor_sidebar_focus(before);
        }
        sidebar_changed || grid_changed
    }

    /// Ends an in-flight wake because the host is actually back — whether that was
    /// noticed passively (`drain_discovery` seeing a fresh mDNS resolve) or actively
    /// (`tick_wake`'s reachability probe succeeding). `source` is just for the log line.
    pub(crate) fn wake_succeeded(&mut self, host: String, port: u16, mgmt_port: Option<u16>, source: &str) {
        tracing::info!("wake succeeded: {host}:{port} back ({source})");
        let name = self.screens.wake.take().map(|w| w.name);
        // A wake ends on its own timing, not a keypress, so it dismisses its own modal and
        // nothing else. The selection still moves — that is what the wait was for. Safe under
        // an open modal: the one that writes per-host state off the selection pins its target
        // at open time (`GameSettingsState::host`), and the rest key off `host_menu_index`,
        // which nothing in the background reorders (`drain_discovery` only appends).
        if matches!(self.nav.screen, Screen::Wake) {
            self.nav.screen = Screen::Home;
        }
        self.select_host(host, port, mgmt_port);
        // Overrides `select_host`'s plain "Loading library…": after a wait that may
        // have run for minutes with no modal up, the bar's job is to report that the
        // host came back, not just that a fetch started.
        if let Some(name) = name {
            self.set_home_status_delayed(format!("{name} is back online — loading its library…"));
        }
    }

    /// Drains any cover art that's finished decoding since the last tick — called
    /// alongside `drain_discovery`. Returns whether any new art actually arrived
    /// (see `drain_discovery`'s docs on why).
    pub fn drain_art(&mut self) -> bool {
        let Some(loader) = &self.jobs.art else { return false };
        let loaded = loader.drain();
        if loaded.is_empty() {
            return false;
        }
        for item in loaded {
            match item {
                crate::services::art::ArtLoaded::Card { game_id, art } => {
                    // Layout is unchanged by art arriving: only that card's cover is rebuilt.
                    self.render.grid.cards_dirty.push(game_id.clone());
                    self.library.art.insert(game_id, art);
                }
                crate::services::art::ArtLoaded::Hero { game_id, image } => {
                    // One that's no longer of use (focus moved on) is let go of in the
                    // loader too, so coming back to that card asks again — served from the
                    // disk cache by then, no round trip.
                    if !self.render.hero.accept(game_id.clone(), image) {
                        if let Some(loader) = &mut self.jobs.art {
                            loader.forget_hero(&game_id);
                        }
                    }
                }
            }
        }
        true
    }
    /// Erases one character from whichever screen is currently editing text, reporting
    /// whether it consumed the key. The counterpart to [`Self::back`]: one definition of
    /// "what an erase means here", so the loop that sees the Backspace doesn't have to
    /// know which screens edit what. `false` leaves the key to its normal `Back` meaning,
    /// which is what makes an erase on an already-empty field still close the modal.
    pub fn erase_text_entry(&mut self) -> bool {
        match self.nav.screen {
            Screen::AddHost | Screen::EditHost => {
                !self.screens.add_host.text().is_empty() && {
                    self.screens.add_host.backspace();
                    true
                }
            }
            Screen::RenameCollection => {
                !self.screens.collections.name.text().is_empty() && {
                    self.screens.collections.name.backspace();
                    true
                }
            }
            Screen::RenameProfile => {
                !self.screens.profile_name.text().is_empty() && {
                    self.screens.profile_name.backspace();
                    true
                }
            }
            Screen::Pairing => self.erase_pin_digit(),
            _ => false,
        }
    }

    /// Applies a `Back` to whichever screen is current — the single shared
    /// definition of "what Back means here" for every caller that needs it
    /// pre-emptively rather than through the normal per-screen `MenuEvent`
    /// dispatch: `main.rs`'s Back handling on Home (a no-op there, but routed
    /// through here so the policy lives in one place) and a modal's close (X)
    /// button click (`handle_mouse_click`'s `hover_close` branch below).
    pub fn back(&mut self, screen_w: u32, screen_h: u32) -> Option<ConnectTarget> {
        // Back steps focus out of the game grid (and the ⋯ column) back onto the
        // host sidebar first. Only a Back from the sidebar itself is a no-op here
        // — the menu loop turns that into the quit dialog.
        if matches!(self.nav.screen, Screen::Home) {
            // A held card's submenu is up: Back dismisses it rather than stepping focus
            // out from under it.
            if self.card_menu.is_some() {
                self.close_card_menu();
                return None;
            }
            match self.home_focus {
                HomeFocus::Grid(_) => {
                    self.set_home_focus(HomeFocus::Sidebar(self.sidebar_index_for_selected()));
                }
                HomeFocus::SidebarMenu(i) => {
                    self.set_home_focus(HomeFocus::Sidebar(i));
                }
                HomeFocus::Sidebar(_) => {}
            }
            return None;
        }
        // Every modal decides for itself where Back goes.
        self.handle_menu_event(MenuEvent::Back, screen_w, screen_h)
    }

    /// Advances every live animation one tick — the eased scroll, the focus pop,
    /// the modal fade — and reports whether anything is still moving (the main
    /// loop keeps rendering while true). Expired animations report one final
    /// `true` so their end state gets drawn.
    pub fn tick_animations(&mut self) -> bool {
        let now = Instant::now();
        let dt = self.last_tick.map_or(ui::animation::SCROLL_STEP_TICK, |t| now - t);
        self.last_tick = Some(now);
        let mut animating =
            ui::animation::ease_scroll(&mut self.render.grid.scroll, self.render.grid.scroll_target, dt);
        if let Some(t) = self.render.focus_anim {
            let duration = match self.home_focus {
                HomeFocus::Grid(_) => ui::animation::CARD_FOCUS_POP,
                HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => ui::animation::FOCUS_POP,
            };
            if t.elapsed() >= duration {
                self.render.focus_anim = None;
            }
            animating = true;
        }
        if self.render.modal.fade.tick() {
            animating = true;
        }
        // The hero loading screen keeps panning for as long as the launch is on screen,
        // which (unlike the fade) is however long the handshake takes.
        if self
            .launch_anim
            .is_some_and(|t| t.elapsed() < hero::LAUNCH_FADE || self.render.hero.showing())
        {
            animating = true;
        }
        if let Some(t) = self.render.modal.focus_anim {
            if t.elapsed() >= ui::animation::FOCUS_POP {
                self.render.modal.focus_anim = None;
            }
            animating = true;
        }
        // Disarmed by `poll_press` (the render loop retires the dip), not here.
        if self.render.press.armed() {
            animating = true;
        }
        if let Some((t, _, _)) = self.render.modal.switch_anim {
            if t.elapsed() >= ui::animation::FOCUS_POP {
                self.render.modal.switch_anim = None;
            }
            animating = true;
        }
        // The lifetime outranks `home_status_sticky`: sticky defends a line against the
        // library reload's clear, not against the clock.
        // Only the expiring frame reports `animating` — the idle branch's `wait_for_event`
        // still times out at `TICK_BUDGET`, so this runs on schedule without holding the
        // SoC at 60Hz for the whole 15s.
        if let Some((_, line)) = self
            .library_status_due
            .take_if(|(t, _)| t.elapsed() >= LIBRARY_STATUS_DELAY)
        {
            // A fetch that already landed needs no line — and must not overwrite whatever
            // `drain_games` put up instead. Only a line that actually goes up is a redraw.
            if self.library_fetch_in_flight() {
                self.set_home_status(Some(line), false);
                animating = true;
            }
        }
        // Every frame of the fade out is a redraw; the frame after it is the clear.
        if self
            .home_status_shown_at
            .is_some_and(|t| t.elapsed() >= HOME_STATUS_LIFETIME)
        {
            if self.home_status_alpha().is_none() {
                self.set_home_status(None, false);
            }
            animating = true;
        }
        // The held card's submenu: its rise, and the selection band's slide between rows.
        // Both run off clocks on `CardMenu`, not off `focus_anim` — without reporting them
        // here the loop parks in `wait_for_event` mid-rise (the auto-repeat KeyDowns the
        // hold swallows set no `dirty`), and the panel finishes only when OK is released.
        if self.card_menu.as_mut().is_some_and(state::cardmenu::CardMenu::tick) {
            animating = true;
        }
        if self.render.grid.card_pops_running() || self.render.grid.reveal.dissolving() {
            animating = true;
        }
        // The kit list and the two focus eases settle on their own clocks, past the pop.
        if self.render.list.as_ref().is_some_and(|(_, l)| l.animating())
            || self.render.sidebar_focus.animating()
            || self.render.tab_focus.animating()
        {
            animating = true;
        }
        // The mark's entrance, from the sidebar's first frame.
        if self
            .render
            .mark_shown_at
            .is_some_and(|t| t.elapsed().as_secs_f32() < pf_console_ui::brand::INTRO_SECS)
        {
            animating = true;
        }
        animating
    }

    /// Queues the whole document for the background writer. Every mutation of settings, hosts or
    /// selection comes through here rather than writing its own slice.
    pub(crate) fn persist(&self) {
        self.state_writer.save(self.persisted());
    }

    /// The document as this App holds it right now.
    pub(crate) fn persisted(&self) -> store::Persisted {
        store::Persisted {
            settings: self.settings_ui.settings.clone(),
            known_hosts: self.hosts.known.clone(),
            selected_host: self.library.selected_host.clone(),
            // Always this build's version: whatever wrote the document last is what a future
            // migration needs to know, and that is now us.
            version: Some(store::VERSION.to_string()),
            profiles: self.profiles.clone(),
        }
    }

    /// Whether the pad in play is a `DualSense` this webOS release only partly supports — the
    /// caution the Controller row carries. The *effective* kind, so `Auto` answers for
    /// whatever is actually attached rather than for the word itself.
    pub(crate) fn dualsense_limited(&self) -> bool {
        let settings = &self.settings_ui.settings;
        let effective = if settings.gamepad_type() == store::GamepadType::Auto {
            self.detected_gamepad_type.unwrap_or_default()
        } else {
            settings.gamepad_type()
        };
        effective.is_dualsense() && !crate::platform::webos::dualsense::hid_playstation_bound()
    }

    /// This set's webOS major, for the row captions that name it. `None` where
    /// `device::sdk_version` could not tell.
    pub(crate) fn webos_major(&self) -> Option<u32> {
        crate::platform::webos::device::sdk_version().map(|(major, _)| major)
    }

    /// The known-host record for an address — the one place `(host, port)` is matched.
    pub(crate) fn known_host(&self, host: &str, port: u16) -> Option<&KnownHost> {
        self.hosts.known.iter().find(|h| h.addr == host && h.port == port)
    }

    /// The `KnownHost` record backing `selected_host`, if any — shared by every lookup
    /// that needs the selected host's collections or per-game settings.
    pub(crate) fn selected_known_host(&self) -> Option<&KnownHost> {
        let (host, port) = self.library.selected_host.as_ref()?;
        self.known_host(host, *port)
    }

    /// The selected host when its last reachability check succeeded.
    pub(crate) fn reachable_selected_host(&self) -> Option<&KnownHost> {
        let known = self.selected_known_host()?;
        (self.known_host_online(known) == Some(true)).then_some(known)
    }

    /// What to do to the selected host on the way out, or `None` when it is set to "None",
    /// has never been paired (the management lane needs this device's cert on its list), or
    /// no host is selected at all.
    ///
    /// Built while `App` is alive so the exit paths can fire it after it is gone — see
    /// [`services::power::ExitPlan`](crate::services::power::ExitPlan).
    pub(crate) fn exit_plan(&self) -> Option<crate::services::power::ExitPlan> {
        // The SELECTED host and only it — the one the sidebar highlights as active, via the
        // same `library.selected_host` that `sidebar_index_of_selected_host` reads. Every
        // other known host is left alone whatever its own `exit_action` says: the setting is
        // per host, but quitting only ever ends the session you are in.
        let Some(known) = self.reachable_selected_host() else {
            tracing::debug!("exit action skipped: selected host was not reachable");
            return None;
        };
        self.power_plan(known, known.exit_action)
    }

    /// The management-lane target for one power action on `known`, or `None` when there is
    /// nothing to send: no action ([`ExitAction::None`]), or no pairing to send it under.
    ///
    /// The one place the mgmt-port default and the pin-is-required rule are stated — the exit
    /// path, the host menu's power row and the permission probe all build their target here.
    pub(crate) fn power_plan(
        &self,
        known: &KnownHost,
        action: crate::services::store::ExitAction,
    ) -> Option<crate::services::power::ExitPlan> {
        action.action_id()?;
        Some(crate::services::power::ExitPlan {
            addr: known.addr.clone(),
            mgmt_port: known.mgmt_port.unwrap_or(crate::services::library::DEFAULT_MGMT_PORT),
            identity: self.identity.clone(),
            // Required, not merely pinned-if-known: an unpaired host would refuse the invoke
            // anyway, and a power action is the last request to send to an unverified peer.
            pin: Some(known.fingerprint()?),
            action,
        })
    }

    /// Which of the selected host's collections holds `pin_id`, or `None` for Library.
    pub(crate) fn collection_of_card(&self, pin_id: &str) -> Option<usize> {
        self.selected_known_host()?.collection_of(pin_id)
    }

    /// Whether a collection holds `pin_id` — what the card menu's Remove row, its Add/Move
    /// wording and the collections modal's heading all turn on. Library *is* "in no
    /// collection", so a card there is not held.
    pub(crate) fn card_is_held(&self, pin_id: &str) -> bool {
        self.collection_of_card(pin_id).is_some()
    }

    pub(crate) fn known_host_mut(&mut self, host: &str, port: u16) -> Option<&mut KnownHost> {
        self.hosts.known.iter_mut().find(|h| h.addr == host && h.port == port)
    }

    pub(crate) fn selected_known_host_mut(&mut self) -> Option<&mut KnownHost> {
        let (host, port) = self.library.selected_host.clone()?;
        self.known_host_mut(&host, port)
    }

    /// The entry behind grid card `idx` (see `grid_card_at`). Callers must only pass an
    /// `idx` that `is_grid_card` (tile building already filters padding gaps out).
    pub(crate) fn grid_card_entry(&self, idx: usize, columns: usize) -> &GameEntry {
        match self.grid_card_at(idx, columns) {
            Some(game) => game,
            None => unreachable!("idx filtered to a real card before building"),
        }
    }

    /// Per-tick app-state advance that must run exactly once, *before* `prepare_tiles`
    /// composes the frame — kept out of `prepare_tiles` so that method only touches tiles.
    /// Derives `card_size` from the current width and advances the modal open/close fades on
    /// a screen transition. Ordering matters: fades must advance once per tick before compose,
    /// so the `ui_flow`/`stream` loops call this immediately ahead of `prepare_tiles`.
    /// Returns whether the screen changed this tick — `prepare_tiles` needs it to force a
    /// modal-tile rebuild on entry, but this method has already consumed the transition by
    /// advancing `last_screen`, so it hands the flag back rather than leaving it to recompute.
    pub fn advance_frame(&mut self, screen_w: u32) -> bool {
        let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
        let columns = view::home::grid_columns(available_w);
        self.render.grid.card_size = view::home::grid_card_size(available_w, columns);

        // Every screen transition triggers close-fade for the left screen and
        // open-fade for the entered screen, centralized here rather than at each
        // dispatch site. Every modal exit fades, modal-to-modal included: the leaving
        // card's pixels go to `tile::MODAL_PREV` (see `snapshot_closing_modal`), so the
        // entering screen taking over `tile::MODAL` no longer forces the close to be a cut.
        let screen_changed = self.nav.screen != self.nav.last_screen;
        if screen_changed {
            let left = self.nav.last_screen;
            self.nav.last_screen = self.nav.screen;
            // Modal-to-modal cross-fades: `ui::fade` makes the leaving card the entering
            // one's inverse. Anything involving Home is a plain open or close.
            if !matches!(left, Screen::Home) {
                if matches!(self.nav.screen, Screen::Home) {
                    self.render.modal.fade.close(left);
                } else {
                    self.render.modal.fade.close_cross(left);
                }
            }
            if !matches!(self.nav.screen, Screen::Home) {
                self.render.modal.fade.open();
                // Reopening the same screen before its close-fade finished — the new
                // open wins. A close-fade for a *different* screen is left alone.
                self.render.modal.fade.cancel_closing(self.nav.screen);
            }
        }
        screen_changed
    }
}
