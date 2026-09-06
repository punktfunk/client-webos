//! The settings page: the desktop shells' page map on the console's row engine
//! (planning `design/webos-pointer-ui-overhaul.md` WP4/WP5).
//!
//! Pages and groups are the `WinUI` order; a row the TV cannot honour is left off a page's
//! positive list rather than greyed. Every shared row is the console's own — its label,
//! options, gate and step come from `pf_console_ui::settings_rows`, so a value means one thing
//! on both fronts. The TV-only rows (`webos.*`) are built here. Writes rebase on the App's
//! document and go through its one writer, never a second store.
//!
//! The scope switcher edits either the global document or one profile's overlay; a profile
//! row that differs from the global wears the dot, and Secondary on it clears the override.

use pf_client_core::profiles::{SettingsOverlay, StreamProfile};
use pf_client_core::trust;
use pf_console_ui::settings_rows::{self as engine, Ctx, RowId};
use pf_console_ui::widgets::RowSpec;

use crate::app::nav::ScreenKey;
use crate::app::{menu, App};
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use crate::core::settings::TvSettings;
use crate::services::store;

/// The page map. Labels are the desktop shells'; marks are the six the desktop's nav uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Page {
    #[default]
    General,
    Display,
    Input,
    Audio,
    Controllers,
    About,
}

impl Page {
    pub const ALL: [Self; 6] = [
        Self::General,
        Self::Display,
        Self::Input,
        Self::Audio,
        Self::Controllers,
        Self::About,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Display => "Display",
            Self::Input => "Input",
            Self::Audio => "Audio",
            Self::Controllers => "Controllers",
            Self::About => "About",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            Self::General => "settings",
            Self::Display => "maximize",
            Self::Input => "keyboard",
            Self::Audio => "volume-2",
            Self::Controllers => "gamepad-2",
            Self::About => "circle-help",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// Which document the page edits.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub(crate) enum Scope {
    #[default]
    Global,
    /// One profile's overlay, by catalog id.
    Profile(String),
}

/// One row on a page: a shared row from the console's engine, or one only the TV has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Row {
    Kit(RowId),
    /// The scope switcher at the top of General.
    Editing,
    /// `webos.game_mode`, rooted TVs only.
    GameMode,
    /// The three-step calibration screen.
    CalibrateHdr,
    /// "Clear HDR calibration?", once one has been saved.
    ResetHdr,
    /// One detected controller, or the "none" placeholder.
    Pad,
    Version,
    LogLevel,
    ShowLogs,
    SendLogs,
    Licences,
    Rename,
    Duplicate,
    Delete,
}

/// The page's own state, beside the row cursor `nav` keeps.
#[derive(Default)]
pub(crate) struct SettingsPage {
    pub page: Page,
    pub scope: Scope,
    /// Focus is on the page column, not the rows (Left from the first column of rows).
    pub column: bool,
}

/// A group header carried by the first row of the group, as the kit draws it.
type Rows = Vec<(Row, Option<&'static str>)>;

/// The rows a page lists in `scope`. The positive list of plan §4; the console's own platform
/// gate and applicability filter run over the shared rows afterwards.
fn page_rows(page: Page, scope: &Scope) -> Rows {
    use RowId as K;
    let profile = matches!(scope, Scope::Profile(_));
    let mut rows: Rows = Vec::new();
    let mut push = |row: Row, header: Option<&'static str>| rows.push((row, header));
    match page {
        Page::General => {
            push(Row::Editing, Some("Editing"));
            if profile {
                push(Row::Rename, Some("Profile"));
                push(Row::Duplicate, None);
                push(Row::Delete, None);
            } else {
                push(Row::Kit(K::AutoWake), Some("Session"));
                push(Row::Kit(K::Stats), Some("Statistics"));
                push(Row::Kit(K::Palette), Some("Interface"));
                push(Row::Kit(K::GamepadUi), None);
                push(Row::Kit(K::GamepadUiMode), None);
            }
        }
        Page::Display => {
            push(Row::Kit(K::Resolution), Some("Resolution"));
            push(Row::Kit(K::Refresh), None);
            push(Row::Kit(K::Bitrate), Some("Quality"));
            push(Row::Kit(K::Codec), None);
            push(Row::Kit(K::Hdr), None);
            if !profile {
                push(Row::CalibrateHdr, None);
                push(Row::ResetHdr, None);
                push(Row::GameMode, Some("TV"));
            }
        }
        Page::Input => {
            push(Row::Kit(K::Mouse), Some("Keyboard & mouse"));
            if !profile {
                push(Row::Kit(K::CursorGestures), None);
            }
            push(Row::Kit(K::InvertScroll), None);
        }
        Page::Audio => {
            push(Row::Kit(K::Audio), None);
            if !profile {
                push(Row::Kit(K::AudioRoute), None);
            }
        }
        Page::Controllers => {
            if !profile {
                push(Row::Pad, Some("Detected controllers"));
            }
            push(Row::Kit(K::PadType), Some("Gamepad"));
            if !profile {
                push(Row::Kit(K::PadHaptics), None);
                push(Row::Kit(K::PadSpeaker), None);
            }
        }
        Page::About => {
            push(Row::Version, None);
            push(Row::LogLevel, Some("Diagnostics"));
            push(Row::ShowLogs, None);
            push(Row::SendLogs, None);
            push(Row::Licences, Some("Legal"));
        }
    }
    rows
}

/// The overlay field a shared row pins in profile scope — `None` for a global-only row.
fn overlay_field(id: RowId) -> Option<&'static str> {
    Some(match id {
        RowId::Resolution => "resolution",
        RowId::Refresh => "refresh_hz",
        RowId::Bitrate => "bitrate_kbps",
        RowId::Codec => "codec",
        RowId::Hdr => "hdr_enabled",
        RowId::Audio => "audio_channels",
        RowId::Mouse => "mouse_mode",
        RowId::InvertScroll => "invert_scroll",
        RowId::PadType => "gamepad",
        RowId::Stats => "stats_verbosity",
        _ => return None,
    })
}

/// Whether `o` pins the field behind `id`.
fn overridden(o: &SettingsOverlay, id: RowId) -> bool {
    match id {
        RowId::Resolution => o.width.is_some() || o.height.is_some() || o.match_window.is_some(),
        RowId::Refresh => o.refresh_hz.is_some(),
        RowId::Bitrate => o.bitrate_kbps.is_some(),
        RowId::Codec => o.codec.is_some(),
        RowId::Hdr => o.hdr_enabled.is_some(),
        RowId::Audio => o.audio_channels.is_some(),
        RowId::Mouse => o.mouse_mode.is_some(),
        RowId::InvertScroll => o.invert_scroll.is_some(),
        RowId::PadType => o.gamepad.is_some(),
        RowId::Stats => o.stats_verbosity.is_some(),
        _ => false,
    }
}

/// A store the engine can be handed without a second writer behind the App's back: it
/// answers with what the page already holds and persists nothing — the page does that
/// itself, on the App's document, after a step.
struct PageStore {
    settings: trust::Settings,
    profiles: Vec<(String, String)>,
}

impl pf_console_ui::SettingsStore for PageStore {
    fn load(&self) -> trust::Settings {
        self.settings.clone()
    }

    fn save(&self, _settings: &trust::Settings) {}

    fn profiles(&self) -> Vec<(String, String)> {
        self.profiles.clone()
    }

    fn known_hosts(&self) -> trust::KnownHosts {
        trust::KnownHosts::default()
    }
}

/// What one remote event does on the settings page, by where focus is. The page column is
/// the way in (OK or Right) and out (Back); on the rows Left and Right step the value, and
/// Back climbs to the column.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NavStep {
    /// Move to the previous (−1) or next (+1) page.
    Page(i32),
    EnterRows,
    ToColumn,
    Leave,
    Row(RowStep),
    None,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RowStep {
    Step(i32),
    Activate,
    Clear,
}

pub(crate) fn settings_nav(column: bool, ev: MenuEvent) -> NavStep {
    if column {
        return match ev {
            MenuEvent::Up => NavStep::Page(-1),
            MenuEvent::Down => NavStep::Page(1),
            MenuEvent::Right | MenuEvent::Confirm => NavStep::EnterRows,
            MenuEvent::Back => NavStep::Leave,
            MenuEvent::Left | MenuEvent::Secondary => NavStep::None,
        };
    }
    match ev {
        MenuEvent::Left => NavStep::Row(RowStep::Step(-1)),
        MenuEvent::Right => NavStep::Row(RowStep::Step(1)),
        MenuEvent::Confirm => NavStep::Row(RowStep::Activate),
        MenuEvent::Secondary => NavStep::Row(RowStep::Clear),
        MenuEvent::Back => NavStep::ToColumn,
        // Up and Down move the row cursor; the row itself does nothing with them.
        MenuEvent::Up | MenuEvent::Down => NavStep::Row(RowStep::Step(0)),
    }
}

impl App {
    pub(crate) fn open_settings_page(&mut self) {
        // The page column takes focus first; OK or Right on a page moves into its rows.
        self.screens.settings_page.column = true;
        self.nav.enter(Screen::SettingsPage, 0);
        if self.screens.settings_page.page == Page::Display {
            self.jobs.root_probe_owed = self.hosts.rooted.is_none() && self.jobs.rooted.is_none();
        }
    }

    /// The document this scope edits: the global one, or the profile's overlay applied to it.
    fn scope_settings(&self) -> trust::Settings {
        let global = self.settings_ui.settings.clone();
        match &self.screens.settings_page.scope {
            Scope::Global => global,
            Scope::Profile(id) => self
                .profiles
                .iter()
                .find(|p| &p.id == id)
                .map_or_else(|| global.clone(), |p| p.overrides.apply(&global)),
        }
    }

    fn scope_profile(&self) -> Option<&StreamProfile> {
        match &self.screens.settings_page.scope {
            Scope::Global => None,
            Scope::Profile(id) => self.profiles.iter().find(|p| &p.id == id),
        }
    }

    /// Run `f` with the console's screen context over `settings`.
    fn with_engine<R>(&self, settings: &mut trust::Settings, f: impl FnOnce(&mut Ctx<'_>) -> R) -> R {
        let store = PageStore {
            settings: settings.clone(),
            profiles: self.profiles.iter().map(|p| (p.id.clone(), p.name.clone())).collect(),
        };
        let library = pf_console_ui::LibraryShared::default();
        let pads = self.kit_pads();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings,
            store: &store,
            platform: pf_console_ui::Platform::WebOS,
            pads: &pads,
            deck: false,
            fallback_ui: true,
            device_name: "webOS TV",
            t: 0.0,
        };
        f(&mut ctx)
    }

    /// The attached pad as the engine's `PadInfo`, so the pad-type row applies.
    fn kit_pads(&self) -> Vec<pf_client_core::menu_nav::PadInfo> {
        self.detected_gamepad_type
            .map(|kind| pf_client_core::menu_nav::PadInfo {
                name: format!("{kind:?}"),
                key: "0".into(),
                pref: crate::core::settings::gamepad_pref(kind),
                steam_virtual: false,
                battery: None,
                detail: String::new(),
                forwarded: false,
                rumble: false,
            })
            .into_iter()
            .collect()
    }

    /// The rows of the open page, gated: the plan's positive list, the console's platform
    /// gate, then its applicability filter.
    pub(crate) fn settings_page_rows(&self) -> Rows {
        let sp = &self.screens.settings_page;
        let mut settings = self.scope_settings();
        let rows = page_rows(sp.page, &sp.scope);
        self.with_engine(&mut settings, |ctx| {
            let mut out: Rows = Vec::new();
            let mut pending_header: Option<&'static str> = None;
            for (row, header) in rows {
                if header.is_some() {
                    pending_header = header;
                }
                let shown = match row {
                    Row::Kit(id) => engine::row_on(id, pf_console_ui::Platform::WebOS) && engine::row_applies(id, ctx),
                    Row::ResetHdr => self.settings_ui.settings.hdr_calibrated(),
                    _ => true,
                };
                if shown {
                    out.push((row, pending_header.take()));
                }
            }
            out
        })
    }

    /// The open page's rows as the kit draws them.
    pub(crate) fn settings_page_specs(&self) -> Vec<RowSpec> {
        let mut settings = self.scope_settings();
        let overlay = self.scope_profile().map(|p| p.overrides.clone());
        let rows = self.settings_page_rows();
        let profiles: Vec<(String, String)> = self.profiles.iter().map(|p| (p.id.clone(), p.name.clone())).collect();
        let core = self.settings_ui.settings.clone();
        self.with_engine(&mut settings, |ctx| {
            rows.iter()
                .map(|&(row, header)| {
                    let mut spec = match row {
                        Row::Kit(id) => {
                            let mut spec = engine::row_spec(id, ctx, &profiles);
                            if let Some(lock) = self.tv_lock(id, &core) {
                                spec = spec.locked(lock);
                            } else if id == RowId::PadType && self.dualsense_limited() {
                                spec = spec.with_note("DualSense is only partly supported on this webOS release");
                            }
                            spec.dot = overlay.as_ref().is_some_and(|o| overridden(o, id));
                            spec
                        }
                        Row::Editing => RowSpec::choice("Editing", self.scope_label()),
                        Row::GameMode => {
                            let spec = RowSpec::toggle("Game mode", core.game_mode())
                                .with_note("Asks the TV to switch its picture mode for the stream");
                            match self.hosts.rooted {
                                Some(true) => spec,
                                Some(false) => spec.locked("Needs a rooted TV"),
                                None => spec.locked("Checking whether this TV is rooted…"),
                            }
                        }
                        Row::CalibrateHdr => {
                            let spec = RowSpec::action("Calibrate HDR…", true);
                            if core.hdr_enabled && crate::core::caps::video_caps().hdr {
                                spec
                            } else {
                                spec.locked("Turn HDR on to calibrate")
                            }
                        }
                        Row::ResetHdr => RowSpec {
                            danger: true,
                            ..RowSpec::action("Clear HDR calibration…", true)
                        },
                        Row::Pad => match self.detected_gamepad_type {
                            Some(kind) => RowSpec::field(format!("{kind:?}"), "Connected".into(), ""),
                            None => RowSpec::field("No controller detected", String::new(), "Connect one to your TV"),
                        },
                        Row::Version => RowSpec::field("Punktfunk", store::VERSION.to_string(), ""),
                        Row::LogLevel => RowSpec::choice(
                            "Log level",
                            menu::log_level_label(crate::logger::current_level_override()),
                        ),
                        Row::ShowLogs => RowSpec::toggle("Show logs", core.show_logs()),
                        Row::SendLogs => RowSpec::action(
                            if self.send_logs_host_ready() {
                                "Send logs to the host"
                            } else {
                                "Send logs to developer…"
                            },
                            true,
                        ),
                        Row::Licences => RowSpec::action("Open-source licences", true),
                        Row::Rename => RowSpec::action("Rename…", true),
                        Row::Duplicate => RowSpec::action("Duplicate", true),
                        Row::Delete => RowSpec::action("Delete…", true),
                    };
                    spec.header = header;
                    spec
                })
                .collect()
        })
    }

    /// What the Editing row reads.
    fn scope_label(&self) -> String {
        match self.scope_profile() {
            Some(p) => p.name.clone(),
            None => "Default settings".into(),
        }
    }

    /// The TV's own reason a shared row is fixed here — caps the handshake narrows, a pad that
    /// is not in the room. The console's engine gates on platform; this gates on the set.
    fn tv_lock(&self, id: RowId, core: &trust::Settings) -> Option<String> {
        let row = match id {
            RowId::Hdr => menu::SettingsRow::Hdr,
            RowId::Codec => menu::SettingsRow::Codec,
            RowId::Audio => menu::SettingsRow::Audio,
            RowId::PadType => menu::SettingsRow::Gamepad,
            RowId::GamepadUi => menu::SettingsRow::GamepadUi,
            RowId::GamepadUiMode => menu::SettingsRow::GamepadUiMode,
            _ => return None,
        };
        menu::row_lock(row, core, self.detected_gamepad_type).map(|lock| menu::lock_caption(lock, self.webos_major()))
    }

    /// One menu event on the page. Left/Right on the column switch pages; on a row they step
    /// it. Confirm activates. Secondary clears an override in profile scope.
    pub(crate) fn handle_settings_page_event(&mut self, ev: MenuEvent) {
        let sp = &self.screens.settings_page;
        match settings_nav(sp.column, ev) {
            NavStep::Page(delta) => {
                let i = sp.page.index();
                let next = if delta < 0 {
                    i.saturating_sub(1)
                } else {
                    (i + 1).min(Page::ALL.len() - 1)
                };
                self.show_page(Page::ALL[next]);
            }
            NavStep::EnterRows => self.screens.settings_page.column = false,
            NavStep::ToColumn => self.screens.settings_page.column = true,
            NavStep::Leave => self.leave_settings_page(),
            NavStep::Row(step) => {
                if self.list_nav_event(ev) {
                    return;
                }
                let rows = self.settings_page_rows();
                let cursor = self
                    .nav
                    .cursor(ScreenKey::SettingsPage)
                    .min(rows.len().saturating_sub(1));
                let Some(&(row, _)) = rows.get(cursor) else {
                    return;
                };
                match step {
                    RowStep::Step(delta) => self.step_row(row, delta, false),
                    RowStep::Activate => self.activate_row(row),
                    RowStep::Clear => self.clear_override(row),
                }
            }
            NavStep::None => {}
        }
    }

    pub(crate) fn show_page(&mut self, page: Page) {
        if self.screens.settings_page.page != page {
            self.screens.settings_page.page = page;
            self.nav.set_cursor(ScreenKey::SettingsPage, 0);
            self.render.modal.focus_anim = Some(std::time::Instant::now());
        }
        // The Game mode row wants the root verdict; the probe runs once the page has settled.
        if page == Page::Display {
            self.jobs.root_probe_owed = self.hosts.rooted.is_none() && self.jobs.rooted.is_none();
        }
    }

    fn leave_settings_page(&mut self) {
        self.nav.screen = Screen::Home;
    }

    fn activate_row(&mut self, row: Row) {
        match row {
            Row::Kit(_) | Row::Editing | Row::LogLevel => self.step_row(row, 1, true),
            Row::GameMode => self.toggle_game_mode_row(),
            Row::CalibrateHdr => {
                if self.settings_ui.settings.hdr_enabled && crate::core::caps::video_caps().hdr {
                    self.open_hdr_calibration();
                }
            }
            Row::ShowLogs => {
                let on = !self.settings_ui.settings.show_logs();
                self.settings_ui.settings.set_show_logs(on);
                crate::runtime::set_log_overlay_enabled(on);
                self.persist();
            }
            Row::SendLogs => self.send_logs_action(),
            Row::ResetHdr => self.open_reset_hdr_calibration(),
            Row::Licences => self.open_about(),
            Row::Rename => self.open_rename_profile(),
            Row::Duplicate => self.duplicate_profile(),
            Row::Delete => self.open_delete_profile(),
            Row::Pad | Row::Version => {}
        }
    }

    /// Step `row` by `delta` (`wrap` for Confirm's forward cycle), then write the change back
    /// to whichever document the scope names.
    fn step_row(&mut self, row: Row, delta: i32, wrap: bool) {
        match row {
            Row::Editing => self.step_scope(delta),
            Row::LogLevel => {
                let cur = menu::log_level_dropdown_current_index(crate::logger::current_level_override());
                let next = menu::cycle_index(cur, menu::LOG_LEVEL_OPTIONS.len(), delta >= 0);
                self.settings_ui
                    .settings
                    .set_log_level_override(menu::LOG_LEVEL_OPTIONS[next]);
                crate::logger::set_level_override(self.settings_ui.settings.log_level_override());
                self.persist();
            }
            Row::Kit(id) => {
                let before = self.scope_settings();
                let mut after = before.clone();
                let changed = self.with_engine(&mut after, |ctx| engine::adjust(id, delta, wrap, ctx));
                if changed {
                    self.write_scope(&before, &after);
                }
            }
            _ => {}
        }
    }

    /// Persist an edited document: the global one, clamped to the TV's caps and projected onto
    /// this client's own struct; or the profile's overlay, absorbing what changed.
    fn write_scope(&mut self, before: &trust::Settings, after: &trust::Settings) {
        match self.screens.settings_page.scope.clone() {
            Scope::Global => {
                let mut document = after.clone();
                document.clamp_to_caps();
                self.settings_ui.settings = document;
                self.persist();
            }
            Scope::Profile(id) => {
                if let Some(p) = self.profiles.iter_mut().find(|p| p.id == id) {
                    p.overrides.absorb(before, after);
                    self.persist();
                }
            }
        }
    }

    /// Secondary on an overridden row in profile scope: back to the global value.
    fn clear_override(&mut self, row: Row) {
        let Row::Kit(id) = row else {
            return;
        };
        let Some(field) = overlay_field(id) else {
            return;
        };
        if let Scope::Profile(pid) = self.screens.settings_page.scope.clone() {
            if let Some(p) = self.profiles.iter_mut().find(|p| p.id == pid) {
                if p.overrides.clear(field) {
                    self.persist();
                }
            }
        }
    }

    /// Editing: Default settings → each profile → New profile… → back around.
    fn step_scope(&mut self, delta: i32) {
        let n = self.profiles.len() + 2;
        let cur = match &self.screens.settings_page.scope {
            Scope::Global => 0,
            Scope::Profile(id) => self.profiles.iter().position(|p| &p.id == id).map_or(0, |i| i + 1),
        };
        let next = menu::cycle_index(cur, n, delta >= 0);
        if next == 0 {
            self.screens.settings_page.scope = Scope::Global;
        } else if next == n - 1 {
            self.new_profile();
        } else {
            self.screens.settings_page.scope = Scope::Profile(self.profiles[next - 1].id.clone());
        }
    }

    fn toggle_game_mode_row(&mut self) {
        if self.hosts.rooted != Some(true) {
            return;
        }
        self.settings_ui
            .settings
            .set_game_mode(!self.settings_ui.settings.game_mode());
        self.persist();
    }

    /// Probes root access for the Game mode row, once per launch — rooting can come and go
    /// between boots, so it is never persisted, and no screen but this one needs the answer.
    ///
    /// Off-thread, and deliberately not on the frame the modal opens: the probe forks
    /// `luna-send-pub`, which in turn launches the Homebrew Channel's service on demand, and
    /// that costs enough CPU on this hardware to show as a stutter in the open animation
    /// running beside it. [`App::tick_root_probe`] starts it once that animation is over.
    fn start_root_probe(&mut self) {
        if self.hosts.rooted.is_some() || self.jobs.rooted.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        match std::thread::Builder::new().name("root-probe".into()).spawn(move || {
            let _ = tx.send(crate::platform::webos::game_mode::probe_rooted());
        }) {
            Ok(_) => self.jobs.rooted = Some(rx),
            // Nothing will ever answer, so settle on "not rooted" rather than leaving the row
            // stuck on its checking caption.
            Err(e) => {
                tracing::warn!("root probe thread: {e}");
                self.settle_rooted(false);
            }
        }
    }

    /// Records the probe's verdict. A `game_mode` left on from when this TV *was* rooted has to
    /// go with it: the row is locked once the verdict is in, so nothing could switch it off
    /// again, and every stream start would keep paying for luna calls that can only fail.
    fn settle_rooted(&mut self, rooted: bool) {
        self.hosts.rooted = Some(rooted);
        if !rooted && self.settings_ui.settings.game_mode() {
            self.settings_ui.settings.set_game_mode(false);
            self.persist();
        }
    }

    /// Starts an owed root probe once the modal that wants it has finished opening. Called
    /// each tick alongside the `drain_*`s.
    pub(crate) fn tick_root_probe(&mut self) {
        // Still on the Display page: leaving before the animation settles defers the probe to
        // the next visit rather than paying for it behind a screen that no longer asks.
        if !self.jobs.root_probe_owed
            || self.nav.screen != Screen::SettingsPage
            || self.screens.settings_page.page != Page::Display
            || self.render.modal.fade.is_animating()
        {
            return;
        }
        self.jobs.root_probe_owed = false;
        self.start_root_probe();
    }

    /// Picks up the probe's verdict, unlocking the Game mode row (or explaining why not).
    /// Reports whether anything changed, so the open screen redraws.
    pub(crate) fn drain_rooted(&mut self) -> bool {
        let Some(rx) = &self.jobs.rooted else { return false };
        let rooted = match rx.try_recv() {
            Ok(rooted) => rooted,
            // A probe thread that died without sending would otherwise leave the row on its
            // checking caption forever, so a dead channel settles like a failed spawn.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => false,
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
        };
        self.jobs.rooted = None;
        self.settle_rooted(rooted);
        true
    }
}

#[cfg(test)]
mod nav_tests {
    use super::*;

    /// Column first: OK or Right enters the rows, Back leaves. On the rows Left and Right
    /// step, Back climbs to the column, never out.
    #[test]
    fn back_climbs_one_level_and_only_the_column_leaves() {
        assert_eq!(settings_nav(true, MenuEvent::Back), NavStep::Leave);
        assert_eq!(settings_nav(true, MenuEvent::Confirm), NavStep::EnterRows);
        assert_eq!(settings_nav(true, MenuEvent::Right), NavStep::EnterRows);
        assert_eq!(settings_nav(true, MenuEvent::Left), NavStep::None);
        assert_eq!(settings_nav(true, MenuEvent::Up), NavStep::Page(-1));
        assert_eq!(settings_nav(false, MenuEvent::Back), NavStep::ToColumn);
        assert_eq!(settings_nav(false, MenuEvent::Left), NavStep::Row(RowStep::Step(-1)));
        assert_eq!(settings_nav(false, MenuEvent::Right), NavStep::Row(RowStep::Step(1)));
        assert_eq!(settings_nav(false, MenuEvent::Confirm), NavStep::Row(RowStep::Activate));
    }
}
