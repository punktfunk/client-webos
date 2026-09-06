//! Home screen logic: sidebar/grid navigation, host selection, game library fetch,
//! launching. Grid pixel geometry (rect helpers) lives in `app::view::home`.
use crate::app::hosts::HostEntry;
use crate::app::state::textfield::TextField;
use crate::app::view;
use crate::app::App;
use crate::app::ConnectTarget;
use crate::core::event::MenuEvent;
use crate::core::screen::{HomeFocus, Screen};
use crate::ui;
use std::time::Instant;

/// Home's two focus containers. Rows and their ⋯ buttons share one: they are laterally
/// paired, so Left off a ⋯ must reach *its own* row rather than the group's remembered
/// entry point. Column-stickiness when walking the ⋯ column upward falls out of the
/// rects instead — the button above is cross-axis aligned, the row body above isn't.
const GROUP_SIDEBAR: u8 = 0;
const GROUP_GRID: u8 = 1;

fn sidebar_row(focus: HomeFocus) -> Option<usize> {
    match focus {
        HomeFocus::Sidebar(index) | HomeFocus::SidebarMenu(index) => Some(index),
        HomeFocus::Grid(_) => None,
    }
}

impl App {
    /// Applies an interactive Home focus transition and its visual state.
    pub(crate) fn set_home_focus(&mut self, focus: HomeFocus) -> bool {
        let previous = self.home_focus;
        if previous == focus {
            return false;
        }
        self.home_focus = focus;
        if let HomeFocus::Grid(index) = focus {
            self.render.grid.focus_last = index;
        }
        if matches!(focus, HomeFocus::Grid(_)) || sidebar_row(previous) != sidebar_row(focus) {
            self.render.focus_anim = Some(Instant::now());
        }
        true
    }

    /// Total sidebar nav positions: host rows + "+ Add host" + "Settings".
    pub(crate) fn sidebar_len(&self) -> usize {
        self.hosts.entries.len() + 2
    }

    /// Read-only forwards to [`Library`](crate::app::library::Library)'s geometry, for the
    /// call sites that only need one answer. Anything that then mutates `self` must go
    /// through `self.library` directly: a `GridLayout` borrows the library alone, where one
    /// of these borrows the whole of `App`.
    ///
    /// Total grid nav positions — `0` (no cards at all) only when no host is
    /// selected yet, or one's selected but hasn't answered a library fetch yet.
    pub(crate) fn grid_len(&self, columns: usize) -> usize {
        if self.library.selected_host.is_none() {
            return 0;
        }
        self.library.grid_len(columns)
    }

    /// The card at grid index `idx`, or `None` for the padding after a partial
    /// section row, or out of range.
    pub(crate) fn grid_card_at(&self, idx: usize, columns: usize) -> Option<&crate::core::model::GameEntry> {
        self.library.card_at(idx, columns)
    }

    /// The pin id for whatever's at grid index `idx` — see `GridLayout::pin_id_at`.
    pub(crate) fn pin_id_at_grid_idx(&self, idx: usize, columns: usize) -> Option<&str> {
        self.library.pin_id_at(idx, columns)
    }

    /// Inverse of `pin_id_at_grid_idx`: grid index for a pin ID, keeping focus after reorder.
    pub(crate) fn grid_idx_for_pin_id(&self, id: &str, columns: usize) -> Option<usize> {
        self.library.idx_for_pin_id(id, columns)
    }

    /// Whether grid index `idx` is an actual card rather than empty padding
    /// after a partial section row.
    pub(crate) fn is_grid_card(&self, idx: usize, columns: usize) -> bool {
        self.grid_card_at(idx, columns).is_some()
    }

    pub(crate) fn sidebar_index_for_selected(&self) -> usize {
        self.sidebar_index_of_selected_host().unwrap_or(0)
    }

    /// Like `sidebar_index_for_selected`, but `None` both when nothing is selected
    /// and when the selected host has since dropped out of `entries` — a caller
    /// highlighting the active row must not fall back to row 0 in that case.
    pub(crate) fn sidebar_index_of_selected_host(&self) -> Option<usize> {
        let (h, p) = self.library.selected_host.as_ref()?;
        self.hosts.entries.iter().position(|e| e.host() == h && e.port() == *p)
    }
    /// Everything focusable on Home, as rects in screen space at the *target* scroll
    /// (not the eased `grid_scroll` — navigation should chase where the grid is going,
    /// not where it currently is). Rebuilt per d-pad press from the same geometry the
    /// draw path uses, so a layout hole simply has no item and no direction can land
    /// on it.
    fn home_focus_map(&self, columns: usize, screen_w: u32, screen_h: u32) -> ui::focus::FocusMap<HomeFocus> {
        let host_count = self.hosts.entries.len();
        let sidebar_len = self.sidebar_len();
        let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);

        let mut map = ui::focus::FocusMap::default();
        map.group(
            GROUP_SIDEBAR,
            ui::focus::Wrap::Vertical,
            HomeFocus::Sidebar(self.sidebar_index_for_selected()),
            HomeFocus::Sidebar(0),
        );
        map.group(
            GROUP_GRID,
            ui::focus::Wrap::None,
            HomeFocus::Grid(self.render.grid.focus_last),
            HomeFocus::Grid(0),
        );

        // One split for the whole sidebar — the same `Vec<Rect>` the painter and both hit
        // tests read, so focus can't disagree with what is on screen.
        for (index, row) in view::sidebar::nav_rows(sidebar_len, screen_h).into_iter().enumerate() {
            let has_menu = index < host_count && self.hosts.entries[index].has_menu();
            if has_menu {
                map.item(
                    HomeFocus::SidebarMenu(index),
                    ui::widgets::sidebar_menu_button_rect(row),
                    GROUP_SIDEBAR,
                );
            }
            map.item(
                HomeFocus::Sidebar(index),
                view::sidebar::row_body_rect(row, has_menu),
                GROUP_SIDEBAR,
            );
        }

        // One layout for the whole sweep: `is_grid_card`/`unscrolled_card_rect` would
        // each rebuild it for every card.
        let layout = self.library.layout(columns);
        // Only the cards a single move can actually reach (see `view::home::focus_window`) —
        // one item per card in the library made every keypress allocate and rescan the whole
        // list to answer a question about its immediate neighbours.
        let focus_window = view::home::focus_window(
            available_w,
            layout,
            self.render.grid.scroll_target,
            screen_h as i32,
            match self.home_focus {
                HomeFocus::Grid(i) => Some(i),
                HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => None,
            },
        );
        for idx in focus_window {
            if layout.card_at(&self.library.games, idx).is_none() {
                continue;
            }
            let r = view::home::unscrolled_card_rect(idx, ui::widgets::SIDEBAR_W as i32, available_w, layout);
            // Screen space, at the scroll the grid is easing *toward* — so that a move
            // crossing into the sidebar compares against its rows on equal terms.
            map.item(
                HomeFocus::Grid(idx),
                r.offset(0, -self.render.grid.scroll_target),
                GROUP_GRID,
            );
        }
        map
    }

    /// Moves Home's focus one step in `dir`: one spatial lookup, with the sidebar and
    /// grid containers deciding where a move that crosses between them lands (see
    /// `home_focus_map`). Layout holes, the pinned-row split and the row/⋯ split all
    /// fall out of the rects themselves.
    fn navigate_home(&mut self, dir: ui::focus::Dir, screen_w: u32, screen_h: u32) {
        let columns = view::home::grid_columns_for_screen(screen_w);
        let Some(next) = self
            .home_focus_map(columns, screen_w, screen_h)
            .navigate(self.home_focus, dir)
        else {
            return;
        };
        self.set_home_focus(next);
        if let HomeFocus::Grid(idx) = next {
            self.ensure_grid_visible(idx, columns, screen_w, screen_h);
        }
    }

    /// Handles one menu event on the Home screen (sidebar + grid). Returns a
    /// `ConnectTarget` when a grid card is confirmed.
    pub fn handle_home_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32) -> Option<ConnectTarget> {
        let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
        let columns = view::home::grid_columns(available_w);

        // The held card's submenu owns every key while it is up — it sits over one card,
        // so moving Home's focus underneath it would leave it pointing at another game.
        if self.handle_card_menu_event(ev, screen_w, screen_h) {
            return None;
        }
        if let Some(dir) = crate::app::menu::nav_dir(ev) {
            self.navigate_home(dir, screen_w, screen_h);
            return None;
        }
        match ev {
            MenuEvent::Confirm => match self.home_focus {
                HomeFocus::Sidebar(i) if i < self.hosts.entries.len() => {
                    self.confirm_sidebar_host(i);
                }
                HomeFocus::Sidebar(i) if i == self.hosts.entries.len() => {
                    self.screens.add_host = TextField::ipv4();
                    self.nav.screen = Screen::AddHost;
                }
                HomeFocus::Sidebar(_) => self.open_settings_page(),
                HomeFocus::SidebarMenu(i) => self.open_host_menu(i),
                HomeFocus::Grid(i) => self.confirm_grid_card(i, columns),
            },
            // Forgets the focused host (removes its persisted entry/fingerprint —
            // it'll reappear as "not paired" if still discoverable on the LAN).
            MenuEvent::Secondary => {
                if let HomeFocus::Sidebar(i) = self.home_focus {
                    if i < self.hosts.entries.len() {
                        self.forget_host(i);
                    }
                }
            }
            // Back on Home is owned by `App::back` (grid/⋯ → sidebar), reached via
            // `dispatch_menu_event` before this handler — never routed here.
            // The four directions returned above, through `menu::nav_dir`.
            MenuEvent::Back | MenuEvent::Up | MenuEvent::Down | MenuEvent::Left | MenuEvent::Right => {}
        }
        None
    }

    /// Pin ID of focused grid card, or `None` for sidebar/padding.
    pub(crate) fn focused_pin_id(&self, columns: usize) -> Option<&str> {
        match self.home_focus {
            HomeFocus::Grid(idx) => self.pin_id_at_grid_idx(idx, columns),
            HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_) => None,
        }
    }

    /// Moves the focused card into collection `to` (or back to Library with `None`),
    /// regroups the grid and carries focus with it. The one path every move takes — the card
    /// menu's Remove, and the Collections screen's confirm.
    pub(crate) fn move_focused_card(&mut self, to: Option<usize>, screen_w: u32, screen_h: u32) {
        let columns = view::home::grid_columns_for_screen(screen_w);
        let HomeFocus::Grid(idx) = self.home_focus else {
            return;
        };
        let Some(id) = self.pin_id_at_grid_idx(idx, columns).map(str::to_string) else {
            return;
        };
        self.move_card(&id, to, columns, screen_w, screen_h);
    }

    /// [`Self::move_focused_card`] for a card named by id — what the collections modal
    /// confirms with, since the card it targets need not be the focused one by then.
    /// `false` when the card already sits in `to` and nothing moved.
    pub(crate) fn move_card(
        &mut self,
        id: &str,
        to: Option<usize>,
        columns: usize,
        screen_w: u32,
        screen_h: u32,
    ) -> bool {
        let Some(known) = self.selected_known_host() else {
            return false;
        };
        if known.collection_of(id) == to {
            return false;
        }
        let Some(known) = self.selected_known_host_mut() else {
            return false;
        };
        known.move_to(id, to);
        self.persist();

        self.regroup_games();
        if let Some(new_idx) = self.grid_idx_for_pin_id(id, columns) {
            self.set_home_focus(HomeFocus::Grid(new_idx));
            self.ensure_grid_visible(new_idx, columns, screen_w, screen_h);
        }
        // Only the card that moved pops. Every other card slides one slot along — a shift the
        // eye already follows — and re-arming the whole reshuffled tail read as the grid
        // reloading rather than as one card changing places.
        self.render.grid.arm_card_pop(id, Instant::now());
        true
    }

    /// Re-lays the grid out over the selected host's collections — see
    /// [`Library::regroup`](crate::app::library::Library::regroup). Every path that changes a
    /// collection, the selected host or the library ends here.
    pub(crate) fn regroup_games(&mut self) {
        match self.selected_known_host_idx() {
            Some(idx) => {
                // Split the borrow: `regroup` needs the host record while it rewrites the
                // library, and both live on `self`.
                let host = std::mem::take(&mut self.hosts.known[idx]);
                self.library.set_desktop_icon(&host.os);
                self.library
                    .regroup(&host, self.recents.for_host(&host.addr, host.port));
                self.hosts.known[idx] = host;
            }
            None => self.library.clear_groups(),
        }
    }

    /// Drops per-game state (collection membership *and* settings overrides) for games the
    /// host no longer lists — otherwise a removed game keeps a slot in its collection and its
    /// overrides linger forever.
    ///
    /// Call only from the success arm of [`App::drain_games`]: `self.library.games` is empty
    /// whenever a fetch failed or a host is unreachable, and pruning against that would
    /// wipe everything the user configured the moment their host went to sleep.
    pub(crate) fn prune_stale_game_prefs(&mut self) {
        let Some(known_idx) = self.selected_known_host_idx() else {
            return;
        };
        let live: std::collections::HashSet<&str> = self.library.games.iter().map(|g| g.id.as_str()).collect();
        let (host, port) = (
            self.hosts.known[known_idx].addr.clone(),
            self.hosts.known[known_idx].port,
        );
        if self.hosts.known[known_idx].prune_games(|id| live.contains(id)) {
            self.persist();
        }
        // The play times go the same way, or `recents.json` keeps ids nothing can reach.
        if self.recents.prune(&host, port, |id| live.contains(id)) {
            self.recents.save();
        }
    }

    fn selected_known_host_idx(&self) -> Option<usize> {
        self.library
            .selected_host
            .as_ref()
            .and_then(|(h, p)| self.hosts.known.iter().position(|k| k.addr == *h && k.port == *p))
    }

    /// Eased 0..=1 progress of pin id `id`'s arrival (see `tile::CardSlot::pop`)
    /// — 1.0, full size, for anything not animating.
    pub(crate) fn card_pop_frac(&self, id: &str) -> f32 {
        crate::app::grid::Entrance::progress_of(self.render.grid.arrivals.pop(id), std::time::Instant::now()).0
    }

    /// The largest useful `grid_scroll` for the current library/layout — 0 when
    /// everything already fits on screen.
    pub(crate) fn max_grid_scroll(&self, columns: usize, available_w: u32, screen_h: u32) -> i32 {
        let viewport_h = screen_h as i32 - view::home::GRID_PAD - view::home::GRID_TOP_Y;
        let extra = self.library.layout(columns).total_extra();
        (view::home::grid_layer_height(self.grid_len(columns), columns, available_w) as i32 + extra
            - 2 * view::home::GRID_LAYER_PAD
            - viewport_h)
            .max(0)
    }

    /// Scrolls the grid (via `grid_scroll_target` — the rendered offset eases
    /// toward it, see `tick_animations`) just far enough that focused card `idx`,
    /// including its focus-ring halo, will be fully on screen. Clamped to the
    /// grid's real extent.
    pub(crate) fn ensure_grid_visible(&mut self, idx: usize, columns: usize, screen_w: u32, screen_h: u32) {
        /// Focus ring + `inflate` overhang around a focused card, plus a little
        /// breathing room.
        const FOCUS_MARGIN: i32 = 16;
        let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
        let card = self.unscrolled_card_rect(idx, columns, ui::widgets::SIDEBAR_W as i32, available_w);
        let viewport = (view::home::GRID_TOP_Y, screen_h as i32 - view::home::GRID_PAD);
        let max_scroll = self.max_grid_scroll(columns, available_w, screen_h);
        self.render.grid.scroll_target =
            ui::scroll::scroll_to_reveal(card, viewport, self.render.grid.scroll_target, FOCUS_MARGIN)
                .clamp(0, max_scroll);
    }

    /// Scrolls the grid by `dy_px` (positive = content moves up), clamped — the
    /// Magic Remote's scroll wheel on the Home screen. Returns whether the target
    /// actually moved (drives redraw; the eased offset follows in
    /// `tick_animations`).
    pub fn scroll_grid_by(&mut self, dy_px: i32, screen_w: u32, screen_h: u32) -> bool {
        if self.library.selected_host.is_none() {
            return false;
        }
        let available_w = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
        let columns = view::home::grid_columns(available_w);
        let max_scroll = self.max_grid_scroll(columns, available_w, screen_h);
        let next = (self.render.grid.scroll_target + dy_px).clamp(0, max_scroll);
        let changed = next != self.render.grid.scroll_target;
        self.render.grid.scroll_target = next;
        changed
    }
    pub(crate) fn confirm_sidebar_host(&mut self, idx: usize) {
        let entry = self.hosts.entries[idx].clone();
        match entry {
            HostEntry::Pinned { host, profile_id, .. } => {
                let (addr, port) = (host.addr.clone(), host.port);
                self.connect_desktop_with(&addr, port, Some(profile_id));
            }
            HostEntry::Known(h) if h.is_paired() => {
                let (host, port, mgmt_port) = (h.addr.clone(), h.port, h.mgmt_port);
                // Re-confirming the already-active host refreshes its library too — a
                // user clicking it is asking to see the current game list, e.g. after
                // installing something new on the host.
                self.select_host(host, port, mgmt_port);
            }
            _ => self.open_pairing(idx),
        }
    }

    /// Drops the selected host and everything drawn from its library — the grid, its art and any
    /// in-flight fetch. Whatever removed the host from the sidebar (Forget) must call this, or
    /// its grid stays on screen with no row to go back to.
    pub(crate) fn clear_selected_host(&mut self) {
        self.library.selected_host = None;
        self.library.clear();
        self.jobs.cancel_library();
        self.set_home_status(None, false);
        // Teardown establishes the next stable focus before Home is drawn again.
        self.home_focus = HomeFocus::Sidebar(0);
        self.render.grid.focus_last = 0;
        self.render.grid.dirty = true;
    }

    /// Selects host and kicks off async library fetch; avoids blocking the UI thread (used to freeze input).
    pub(crate) fn select_host(&mut self, host: String, port: u16, mgmt_port: Option<u16>) {
        // Picking a host is the "I want it back" that lifts a manual power-down — including
        // the auto-wake suppression, so the fetch below may legitimately wake it again.
        if self.hosts.powered_down.as_ref() == Some(&(host.clone(), port)) {
            self.hosts.powered_down = None;
        }
        self.library.selected_host = Some((host.clone(), port));
        self.persist();
        let name = self
            .hosts
            .known
            .iter()
            .find(|h| h.addr == host && h.port == port)
            .map_or_else(|| host.clone(), |h| h.name.clone());
        // Picking a host is the user's own action, so its progress replaces whatever the last
        // launch left on screen. The reload `App::new` starts is not this — it runs before
        // `home_status_sticky` is ever set. The line waits out `LIBRARY_STATUS_DELAY` (`tick_animations`
        // puts it up); the grid's spinner already says "working" in the meantime.
        self.set_home_status_delayed(format!("Loading library from {name}…"));
        self.library.clear();
        // Dropping the loader stops its worker (its request channel closes), so a host
        // switch abandons in-flight fetches for the previous library.
        self.jobs.art = None;
        // Focus stays on the sidebar until `drain_games` has cards to land on: `navigate`
        // can't move off a key with no rect, so an empty grid would kill the d-pad.
        self.render.grid.focus_last = 0;
        self.render.grid.dirty = true;
        self.render.grid.scroll = 0;
        self.render.grid.scroll_target = 0;

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let known = self.hosts.known.iter().find(|h| h.addr == host && h.port == port);
        let fingerprint = known.and_then(crate::core::model::KnownHost::fingerprint);
        let mgmt_port = mgmt_port.unwrap_or(crate::services::library::DEFAULT_MGMT_PORT);
        tracing::debug!("library: fetching from {host}:{mgmt_port}…");
        self.jobs.games = Some(crate::services::library::load_games_async(
            host,
            port,
            mgmt_port,
            identity,
            fingerprint,
            crate::services::budget::REQUEST,
        ));
    }

    /// Whether `select_host`'s library fetch is still out. Deliberately not `!games_loaded`:
    /// that stays `false` down the `Unreachable` path too, which would leave the grid's
    /// loading spinner running forever behind the Wake dialog.
    pub(crate) fn library_fetch_in_flight(&self) -> bool {
        self.jobs.games.is_some()
    }

    /// Whether a sent wake is being waited on with no modal up — the user pressed "Wake host"
    /// (or `wol_auto` sent silently), so the grid keeps its spinner turning until the host
    /// answers or `tick_wake` re-pops the dialog.
    pub(crate) fn wake_wait_in_flight(&self) -> bool {
        self.nav.screen != crate::app::Screen::Wake && self.screens.wake.as_ref().is_some_and(|w| w.sent && w.silent)
    }

    /// Drains `select_host`'s library fetch; switching hosts aborts old fetches safely.
    pub fn drain_games(&mut self) -> bool {
        let Some(rx) = &self.jobs.games else { return false };
        let Ok(loaded) = rx.try_recv() else { return false };
        self.jobs.games = None;
        let crate::services::library::GamesLoaded {
            host,
            port,
            mgmt_port,
            result,
        } = loaded;
        // The listing is the freshest word on whether this host is up, and it lands minutes
        // before the ambient sweep would ask again.
        self.note_api_result(&host, port, &result);
        match result {
            Ok(games) => {
                tracing::info!("library: {} games from {host}:{mgmt_port}", games.len());
                let identity = (self.identity.0.clone(), self.identity.1.clone());
                let known = self.hosts.known.iter().find(|h| h.addr == host && h.port == port);
                let fingerprint = known.and_then(crate::core::model::KnownHost::fingerprint);
                // Covers are requested per card as the grid window reaches them (see
                // `App::prepare_tiles`), not fetched for the whole library up front.
                self.jobs.art = Some(crate::services::art::ArtLoader::spawn(
                    host,
                    port,
                    mgmt_port,
                    identity,
                    fingerprint,
                    self.render.grid.card_size,
                ));
                self.library.load_games(games);
                // Hand the grid the focus `select_host` held back — only if the user hasn't
                // navigated off that row, so a late fetch can't yank them.
                if matches!(self.home_focus, HomeFocus::Sidebar(i) if Some(i) == self.sidebar_index_of_selected_host())
                {
                    self.set_home_focus(HomeFocus::Grid(0));
                }
                if !self.home_status_sticky {
                    self.set_home_status(None, false);
                }
                // Held until now rather than shown at startup: the hint points at the cards,
                // so it waits for a library to exist. Spent on sight, whatever happens next.
                if std::mem::take(&mut self.intro_hint_owed) {
                    self.set_home_status(Some(crate::app::state::cardmenu::INTRO_HINT.to_string()), false);
                }
                self.prune_stale_game_prefs();
                self.regroup_games();
            }
            Err(e) => {
                tracing::warn!("library fetch failed ({host}:{mgmt_port}): {e}");
                self.handle_library_error(host, port, &e);
            }
        }
        self.render.grid.dirty = true;
        true
    }

    /// Shared handling for a failed library fetch/reachability check, used by both
    /// `drain_games` and `drain_launch_check`. `Unreachable` opens the Wake dialog
    /// (even with no MAC on record — `start_wake`/`render_wake` just hide the send
    /// controls then); `NotPaired`/`PinMismatch`/`Http` mean the host answered, so
    /// Wake-on-LAN wouldn't help — those stay a plain status line.
    pub(crate) fn handle_library_error(&mut self, host: String, port: u16, e: &crate::services::library::LibraryError) {
        let reason = e.to_string();
        // A live problem with the host beats the previous launch's reason for bouncing.
        self.home_status_sticky = false;
        if matches!(e, crate::services::library::LibraryError::Unreachable(_)) {
            let mac = self
                .hosts
                .known
                .iter()
                .find(|h| h.addr == host && h.port == port)
                .map(|h| h.mac.clone())
                .unwrap_or_default();
            self.start_wake(host, port, mac, reason);
        } else {
            // The host answered — just not with a usable library — so Desktop is a
            // legitimate fallback here, unlike the `Unreachable` branch above.
            self.library.load_games(Vec::new());
            self.regroup_games();
            self.set_home_status(Some(reason), false);
        }
    }
    /// Confirms grid card, arming the launch straight away so the connect thread starts on the
    /// click and overlaps the zoom. No pre-flight reachability probe: it cost a whole mTLS
    /// round trip before the handshake, and connect reports an unreachable host itself (the
    /// host list probes on selection, which is where the Wake dialog comes from).
    pub(crate) fn confirm_grid_card(&mut self, idx: usize, columns: usize) {
        if self.launch_ready.is_some() || self.launch_anim.is_some() {
            return;
        }
        let Some((host, port)) = self.library.selected_host.clone() else {
            return;
        };
        let Some(known) = self.hosts.known.iter().find(|h| h.addr == host && h.port == port) else {
            return;
        };
        // The pin is also the pair state: no pin means the host was never paired, so there is
        // nothing to connect with.
        let Some(fingerprint) = known.fingerprint() else {
            return;
        };
        // Desktop is the host's own session rather than a game it lists, so it launches with
        // no id — the one thing left that tells it apart from any other card.
        let launch = match self.grid_card_at(idx, columns) {
            Some(game) if game.id == crate::core::model::DESKTOP_PIN_ID => None,
            Some(game) => Some(game.id.clone()),
            None => return,
        };
        // The loading screen's backdrop. Desktop has no art, so it keeps the plain fade to
        // black — as does a game with none, which hands straight over to the stream. Armed
        // before the art is handed over, so `Hero::accept` recognises a cache hit as belonging
        // to this launch even if the focus prefetch never ran.
        let game = launch
            .as_ref()
            .and_then(|id| self.library.games.iter().find(|g| &g.id == id));
        self.render.hero.arm(launch.clone());
        if let (Some(game), Some(loader)) = (game, &mut self.jobs.art) {
            // The disk is only touched when the focus prefetch hasn't already decoded this
            // hero — re-reading it would be several MB of nothing, and that is the common case.
            let in_hand = self.render.hero.image_for(&game.id).is_some()
                || loader
                    .cached_hero(&game.id)
                    .is_some_and(|image| self.render.hero.accept(game.id.clone(), image));
            if !in_hand && (game.art.hero.is_some() || game.art.header.is_some()) {
                // Asked for here as well as on focus, since a card can be confirmed before the
                // prefetch got round to it. The fetch overlaps the connect, and only *this*
                // case — art that isn't on this TV yet — is worth holding the hand-off for.
                loader.request_hero(game);
                self.render.hero.await_art();
            }
        }
        tracing::debug!("launch: connecting to {host}:{port} now, zoom runs in parallel");
        // The zoom and the fade to black say the launch started; a status line under them
        // only competes with that. Cleared so the last launch's failure doesn't sit under
        // this one either.
        self.set_home_status(None, false);
        self.launch_anim_idx = Some(idx);
        self.launch_ready = Some(ConnectTarget {
            host,
            port,
            fingerprint,
            launch,
            profile: None,
        });
        // Not `grid_dirty`: contents are unchanged, and dirtying rebuilds every card tile and
        // re-arms the loading spinner right as the zoom starts.
    }

    /// Streams `host`'s desktop with `profile` for this session only: "Connect with ▸" and the
    /// pinned sidebar cards. The host becomes the selected one too, so the library is what
    /// the stream returns to.
    pub(crate) fn connect_desktop_with(&mut self, host: &str, port: u16, profile: Option<String>) {
        if self.launch_ready.is_some() || self.launch_anim.is_some() {
            return;
        }
        let Some(known) = self.known_host(host, port) else {
            return;
        };
        let Some(fingerprint) = known.fingerprint() else {
            return;
        };
        let mgmt_port = known.mgmt_port;
        if self
            .library
            .selected_host
            .as_ref()
            .is_none_or(|(h, p)| h != host || *p != port)
        {
            self.select_host(host.to_string(), port, mgmt_port);
        }
        self.render.hero.arm(None);
        self.set_home_status(None, false);
        self.launch_anim_idx = None;
        self.launch_ready = Some(ConnectTarget {
            host: host.to_string(),
            port,
            fingerprint,
            launch: None,
            profile,
        });
    }

    /// Takes the `ConnectTarget` `confirm_grid_card` armed, if any — the runtime's tick loop
    /// calls this and breaks its event loop with it to actually start the stream.
    pub fn take_ready_launch(&mut self) -> Option<ConnectTarget> {
        self.launch_ready.take()
    }
    pub(crate) fn forget_host(&mut self, idx: usize) {
        let HostEntry::Known(h) = &self.hosts.entries[idx] else {
            return;
        };
        let (host, port) = (h.addr.clone(), h.port);
        self.hosts.known.retain(|k| !(k.addr == host && k.port == port));
        if self.recents.forget_host(&host, port) {
            self.recents.save();
        }
        crate::services::art::reconcile_host_caches(&self.hosts.known);
        self.rebuild_entries();
        if self.library.selected_host.as_ref() == Some(&(host, port)) {
            self.clear_selected_host();
        }
        // After the selection clear: persisting first would leave `selected_host` in the document
        // pointing at the host just forgotten.
        self.persist();
        let sidebar_len = self.sidebar_len();
        if let HomeFocus::Sidebar(i) = &mut self.home_focus {
            if *i >= sidebar_len {
                *i = sidebar_len - 1;
            }
        }
        self.render.grid.dirty = true;
    }
}
