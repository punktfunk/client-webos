//! Rasterization: which tiles are stale this frame, and what to draw into each.
//!
//! The CPU half of the render path — the only place `tiny_skia` runs. Split out of
//! `app/mod.rs`, which held the state machine and both render halves in one 3.4k-line file.
//! Each `prepare_*` covers one family (sidebar, grid, hero, modal, dropdown, scroll) and
//! reports the tiles it rebuilt so `runtime` can re-upload their textures.
use std::time::Instant;

use anyhow::{Context, Result};

use crate::app::hosts::HostEntry;
use crate::app::nav::ScreenKey;
use crate::app::render::ctx::RenderCtx;
use crate::app::render::key::{ModalFocusKey, ModalShellKey, ScrollContentKey};
use crate::app::render::tile;
use crate::app::render::SnapshotBody;
use crate::app::screens::is_scroll_list;
use crate::app::screens::rowbuttons::RowButton;
use crate::app::{
    menu, view, App, HomeFocus, PairingFocus, Screen, ABOUT_WINDOW_BUDGET, ABOUT_WINDOW_MARGIN, SCROLL_INDICATOR_TILE_W,
};
use crate::ui;
use crate::ui::cache;
use crate::ui::render::{Rect, TileId};
use crate::ui::Painter;

impl App {
    /// Sidebar family: the focus-free strip (rebuilt on content change) plus the
    /// single focused-row overlay tile. Pushes any rebuilt tiles onto `updated`.
    /// Extracted from `prepare_tiles` as a self-contained family (A2 staging).
    fn prepare_sidebar(&mut self, ctx: &mut RenderCtx<'_>) -> Result<()> {
        let RenderCtx {
            tiles,
            text: text_cache,
            fonts,
            screen,
            updated,
            ..
        } = ctx;
        let screen_h = screen.h;
        // Kept on the `sidebar_dirty` flag rather than a content version: the strip is
        // built from every entry plus its reachability, and hashing all of that once a
        // frame would cost more than the flag the event side already maintains.
        let styled_at = ui::theme::epoch();
        if self.render.sidebar_dirty || self.render.sidebar_styled_at != styled_at || !tiles.contains(tile::SIDEBAR) {
            let selected = self.sidebar_index_of_selected_host();
            let entries = &self.hosts.entries;
            let reach = self.reachability_list();
            // Reuses the existing full-height strip as its own scratch surface — several MB
            // that would otherwise be reallocated on every host list change.
            // The outer condition has already decided this is stale, so the version just
            // has to differ from the last one — `ensure_in_place` would otherwise see an
            // unchanged `static_version` and skip the rebuild it was called to do.
            self.render.sidebar_gen = self.render.sidebar_gen.wrapping_add(1);
            tiles.ensure_in_place(
                tile::SIDEBAR,
                self.render.sidebar_gen,
                || Painter::new(ui::widgets::SIDEBAR_W, screen_h),
                |layer| {
                    view::sidebar::draw(
                        &mut ui::Canvas {
                            painter: layer,
                            text_cache,
                            fonts,
                            // The sidebar's own strip is the full panel height and a fixed width.
                            screen_w: ui::widgets::SIDEBAR_W,
                            screen_h,
                        },
                        entries,
                        None,
                        selected,
                        &reach,
                    )
                },
            )?;
            self.render.sidebar_dirty = false;
            self.render.sidebar_styled_at = styled_at;
            tiles.remove(tile::FOCUS_ROW); // row content may have changed under it
            updated.push(tile::SIDEBAR);
        }
        // One tile serves both sidebar focus states (see `render_focused_row_tile`).
        let sidebar_focus = match self.home_focus {
            HomeFocus::Sidebar(i) => Some((i, false)),
            HomeFocus::SidebarMenu(i) => Some((i, true)),
            HomeFocus::Grid(_) => None,
        };
        if let Some(key) = sidebar_focus {
            let online = self.hosts.entries.get(key.0).and_then(|e| self.entry_online(e));
            if tiles.ensure(tile::FOCUS_ROW, cache::version(&key), || {
                ui::rasterize(
                    view::sidebar::FocusedRowTile {
                        entries: &self.hosts.entries,
                        index: key.0,
                        menu_focused: key.1,
                        online,
                    },
                    text_cache,
                    fonts,
                )
            })? {
                updated.push(tile::FOCUS_ROW);
            }
        }
        Ok(())
    }

    /// Uploads the launching game's hero art as `tile::HERO`, once, and starts its
    /// fade-in clock. Gated on the launch having actually started: at ~1600px wide this
    /// is a multi-MB texture, and putting one on the GPU for every card the user merely
    /// scrolls past would undo the whole point of the windowed card cache.
    fn prepare_hero(&mut self, ctx: &mut RenderCtx<'_>) {
        let updated = &mut ctx.updated;
        if self.launch_anim.is_none() {
            return;
        }
        let Some(id) = self.render.hero.pending_upload() else {
            return;
        };
        // One hero slot: the upload replaces whatever it held (`Compositor::upload_raw`),
        // reusing the texture when the two images happen to share a size.
        self.render.hero.mark_uploaded(id);
        updated.push(tile::HERO);
    }

    /// Copies the leaving modal's pixels aside so it can fade out while the entering one
    /// rebuilds `tile::MODAL` — on the frame `advance_frame` started a close fade, only.
    /// Cloned rather than re-rendered: the left screen's state may already be torn down
    /// (a cleared `host_menu_index`), and these pixels are what was on screen last frame.
    fn snapshot_closing_modal(&mut self, ctx: &mut RenderCtx<'_>) {
        let RenderCtx {
            tiles,
            fonts,
            screen: size,
            screen_changed,
            updated,
            ..
        } = ctx;
        let screen_changed = *screen_changed;
        let (screen_w, screen_h) = (size.w, size.h);
        // Presence only — this decides whether a snapshot is still wanted, not what it
        // is drawn at, so it needs neither the entering alpha nor the exact duration.
        let closing = self.render.modal.fade.closing_frame();
        if closing.is_none() {
            // Fade over (or cancelled by reopening the same screen) — drop the copies
            // rather than keep two card-sized textures alive for nothing.
            if self.render.modal.prev.take().is_some() {
                tiles.remove(tile::MODAL_PREV);
                tiles.remove(tile::MODAL_PREV_CONTENT);
                self.render.evicted_tiles.push(tile::MODAL_PREV);
                self.render.evicted_tiles.push(tile::MODAL_PREV_CONTENT);
            }
            return;
        }
        let Some((_, left)) = closing.filter(|_| screen_changed) else {
            return;
        };
        let Some(shell) = tiles.get(tile::MODAL).cloned() else {
            return;
        };
        // Still the left card's — `modal_painter` moves it on later in this same call.
        let region = self.render.modal.tile_region;
        // What `compose_modal` was drawing for this screen last frame, frozen. Settings keeps
        // its rows where they are and redraws from them; stitching them into one full-height
        // painter here is what made leaving Settings slower than leaving any other modal.
        let content = match left {
            screen if is_scroll_list(screen) => self
                .scroll_view_for(left, screen_w, screen_h, fonts)
                .map(|(total, content, scroll_px)| SnapshotBody::Rows(total, content, scroll_px)),
            // `src_rect` first: a screen whose body lives in its shell tile has no crop, and
            // cloning the body pixmap before finding that out copies it for nothing.
            _ => self
                .scroll_src_rect(left, screen_w, screen_h, fonts)
                .and_then(|(src, dst)| Some((src, dst, tiles.get(tile::SCROLL_CONTENT).cloned()?)))
                .map(|(src, dst, body)| {
                    tiles.put(tile::MODAL_PREV_CONTENT, cache::static_version(), body);
                    updated.push(tile::MODAL_PREV_CONTENT);
                    SnapshotBody::Cropped(src, dst)
                }),
        };
        tiles.put(tile::MODAL_PREV, cache::static_version(), shell);
        updated.push(tile::MODAL_PREV);
        self.render.modal.prev = Some(crate::app::render::ModalSnapshot { region, content });
    }

    /// The version [`tile::MODAL`] is valid at — a hash of everything the open screen's
    /// chrome reads, plus the close-button hover every shell shares. `None` for a screen with
    /// no shell key of its own (`AddHost` and friends redraw on any `content_dirty` tick
    /// instead; see [`ModalShellKey`]).
    ///
    /// The key never leaves this function, which is what lets it borrow labels straight out of
    /// `App`: only the hash is kept, so a shell that is re-entered with different content
    /// differs by version rather than by a stored clone of every string it draws.
    fn modal_shell_version(
        &self,
        host_menu_actions: &[crate::app::state::hostmenu::HostAction],
        host_menu_power: Option<crate::services::store::ExitAction>,
    ) -> Option<u64> {
        // The derived strings the key borrows — bound here so they outlive it, and built only
        // on the screen that actually reads each one.
        let host_menu_title = matches!(self.nav.screen, Screen::HostMenu)
            .then(|| self.host_menu_title())
            .unwrap_or_default();
        let host_menu_subtitle = matches!(self.nav.screen, Screen::HostMenu)
            .then(|| self.host_menu_subtitle())
            .unwrap_or_default();
        let host_power_title = matches!(self.nav.screen, Screen::HostPower)
            .then(|| view::hostpower::title(&self.host_menu_title()))
            .unwrap_or_default();
        let speed_test_status = matches!(self.nav.screen, Screen::SpeedTest)
            .then(|| view::speedtest::status(self.screens.speed_test.as_ref(), &self.screens.speed_test_name))
            .unwrap_or_default();
        let key = match self.nav.screen {
            Screen::Settings(_) => Some(ModalShellKey::Settings {
                game: self.editing_game().map(|gs| gs.title.as_str()),
            }),
            Screen::Collections => Some(ModalShellKey::Collections {
                held: self.collections_target_held(),
                card: self.collections_heading(),
                rows: self.collections_row_count(),
            }),
            Screen::Wake => self.screens.wake.as_ref().map(|w| ModalShellKey::Wake {
                name: &w.name,
                mac_empty: w.mac.is_empty(),
            }),
            Screen::Pairing => Some(ModalShellKey::Pairing {
                digits: self.screens.pin_digits,
                status: self.screens.pairing_status.as_deref(),
                busy: self.screens.pairing_busy,
            }),
            Screen::ForgetHost => Some(ModalShellKey::ForgetHost {
                name: self
                    .screens
                    .host_menu_index
                    .and_then(|i| self.hosts.entries.get(i))
                    .map(HostEntry::name),
            }),
            Screen::HostMenu => Some(ModalShellKey::HostMenu {
                name: &host_menu_title,
                subtitle: &host_menu_subtitle,
                rows: host_menu_actions.len(),
                power: host_menu_power,
            }),
            Screen::HostPower => {
                let (auto, exit, access) = self.host_power_view();
                Some(ModalShellKey::HostPower {
                    title: &host_power_title,
                    auto,
                    exit,
                    access,
                })
            }
            Screen::About => Some(ModalShellKey::About),
            // The whole shell is derived from the status sentence, which already encodes
            // the phase and the latest measurement.
            Screen::SpeedTest => Some(ModalShellKey::SpeedTest {
                status: &speed_test_status,
            }),
            Screen::Diagnostics => Some(ModalShellKey::Diagnostics {
                log_level: self.settings_ui.settings.log_level_override,
                stats_overlay: self.settings_ui.settings.stats_overlay,
                show_logs: self.settings_ui.settings.show_logs,
                send_to_host: self.send_logs_host_ready(),
            }),
            Screen::Experimental => Some(ModalShellKey::Experimental {
                game_mode: self.settings_ui.settings.game_mode,
                audio_route: self.settings_ui.settings.audio_route,
                hdr_enabled: self.settings_ui.settings.hdr_enabled,
                hdr: self.calibrated_hdr_display(),
                rooted: self.hosts.rooted,
            }),
            Screen::HdrCalibration => self.hdr_calibration_view().map(|m| ModalShellKey::HdrCalibration {
                step: m.step,
                display: m.display,
                stalled: m.stalled,
            }),
            Screen::ControllerSettings(_) => Some(ModalShellKey::ControllerSettings {
                gamepad_type: self.settings_target().gamepad_type,
                pad_haptics: self.settings_target().pad_haptics,
                pad_speaker: self.settings_target().pad_speaker,
                gamepad_ui: self.settings_target().gamepad_ui,
                gamepad_ui_mode: self.settings_target().gamepad_ui_mode,
                detected: self.detected_gamepad_type,
                dualsense_limited: self.dualsense_limited(),
            }),
            Screen::CursorSettings(_) => Some(ModalShellKey::CursorSettings {
                cursor_capture: self.settings_target().cursor_capture,
                cursor_gestures: self.settings_target().cursor_gestures,
                over: self.editing_override(),
            }),
            Screen::SendLogs => Some(ModalShellKey::SendLogs),
            Screen::ResetHdrCalibration => Some(ModalShellKey::ResetHdrCalibration),
            Screen::RemoveCollection => self
                .removed_collection()
                .map(|(name, games)| ModalShellKey::RemoveCollection { name, games }),
            Screen::ResetGameSettings => self
                .settings_ui
                .game_settings
                .as_ref()
                .map(|gs| ModalShellKey::ResetGameSettings { game: &gs.title }),
            // `EditHost` joins `AddHost` in having no shell key: its typed-digit
            // display has no separate focus tile to protect, so it just redraws on
            // any `content_dirty` tick.
            Screen::Home | Screen::AddHost | Screen::EditHost | Screen::RenameCollection => None,
        };
        // Hashed with the key rather than carried inside it: the close-button hover changes
        // every shell alike (see `ModalShellKey`).
        key.as_ref().map(|k| cache::version(&(self.render.hover_close, k)))
    }

    /// The version [`tile::MODAL_FOCUS`] is valid at — a hash of the open modal's focused
    /// widget *and its value*, so a value change invalidates the tile just as a focus move
    /// does. `None` for a screen with no single focused widget. Borrowed like
    /// [`modal_shell_version`](Self::modal_shell_version), and for the same reason.
    fn modal_focus_version(
        &self,
        host_menu_actions: &[crate::app::state::hostmenu::HostAction],
        host_menu_power: Option<crate::services::store::ExitAction>,
    ) -> Option<u64> {
        // Borrowed by the key below, so it outlives it.
        let speed_test_label = matches!(self.nav.screen, Screen::SpeedTest)
            .then(|| view::speedtest::apply_label(view::speedtest::recommendation(self.screens.speed_test.as_ref())))
            .unwrap_or_default();
        let key = match self.nav.screen {
            Screen::Settings(_) => Some(ModalFocusKey::SettingsRow(
                self.nav.cursor(ScreenKey::Settings),
                *self.settings_target(),
                self.editing_override(),
                self.detected_gamepad_type,
            )),
            Screen::Collections => {
                let row = self.nav.cursor(ScreenKey::Collections);
                let host = self.selected_known_host();
                let holding = host
                    .zip(self.screens.collections.target.as_deref())
                    .is_some_and(|(h, target)| h.collection_of(target).or_else(|| h.library_index()) == Some(row));
                let name = host
                    .and_then(|h| h.collections().get(row))
                    .map_or("", |c| c.name.as_str());
                Some(ModalFocusKey::CollectionRow(
                    row,
                    name,
                    holding,
                    self.screens.row_button,
                    self.screens.collections.dragging.is_some(),
                ))
            }
            Screen::Wake => self.screens.wake.as_ref().map(|w| ModalFocusKey::WakeButton(w.focused)),
            Screen::Pairing => Some(match self.screens.pairing_focus {
                PairingFocus::Pin => ModalFocusKey::PairingDigit(
                    self.screens.pin_digit_index,
                    self.screens.pin_digits[self.screens.pin_digit_index],
                ),
                PairingFocus::RequestAccess => ModalFocusKey::PairingButton,
            }),
            Screen::ForgetHost => Some(ModalFocusKey::ForgetButton(self.nav.cursor(ScreenKey::ForgetHost))),
            Screen::HostMenu => host_menu_actions
                .get(self.nav.cursor(ScreenKey::HostMenu))
                .map(|&action| {
                    ModalFocusKey::MenuRow(
                        self.nav.cursor(ScreenKey::HostMenu),
                        action,
                        self.host_menu_paired(),
                        self.screens.row_button,
                        host_menu_power,
                    )
                }),
            Screen::HostPower => {
                let (auto, exit, access) = self.host_power_view();
                Some(ModalFocusKey::HostPowerRow {
                    row: self.nav.cursor(ScreenKey::HostPower),
                    auto,
                    exit,
                    access,
                })
            }
            // Only once there are buttons to focus — while measuring there is nothing
            // on the card but text.
            Screen::SpeedTest => view::speedtest::finished(self.screens.speed_test.as_ref())
                .then(|| ModalFocusKey::SpeedTestButton(self.nav.cursor(ScreenKey::SpeedTest), &speed_test_label)),
            Screen::Diagnostics => Some(ModalFocusKey::DiagnosticsRow(
                self.nav.cursor(ScreenKey::Diagnostics),
                self.settings_ui.settings.log_level_override,
                self.settings_ui.settings.stats_overlay,
                self.settings_ui.settings.show_logs,
            )),
            Screen::Experimental => Some(ModalFocusKey::ExperimentalRow {
                row: self.nav.cursor(ScreenKey::Experimental),
                button: self.screens.row_button,
                game_mode: self.settings_ui.settings.game_mode,
                audio_route: self.settings_ui.settings.audio_route,
                hdr_enabled: self.settings_ui.settings.hdr_enabled,
                hdr: self.calibrated_hdr_display(),
                rooted: self.hosts.rooted,
            }),
            Screen::HdrCalibration => self.hdr_calibration_view().map(|m| {
                ModalFocusKey::HdrCalibrationRow(
                    self.nav.cursor(ScreenKey::HdrCalibration),
                    m.step,
                    m.display,
                    m.stalled,
                    self.screens.row_button,
                )
            }),
            Screen::ControllerSettings(_) => Some(ModalFocusKey::ControllerSettingsRow(
                self.nav.cursor(ScreenKey::ControllerSettings),
                self.settings_target().gamepad_type,
                self.settings_target().pad_haptics,
                self.settings_target().pad_speaker,
                self.settings_target().gamepad_ui,
                self.settings_target().gamepad_ui_mode,
                self.detected_gamepad_type,
                self.dualsense_limited(),
            )),
            Screen::CursorSettings(_) => Some(ModalFocusKey::CursorSettingsRow(
                self.nav.cursor(ScreenKey::CursorSettings),
                self.settings_target().cursor_capture,
                self.settings_target().cursor_gestures,
                self.editing_override(),
            )),
            Screen::SendLogs => Some(ModalFocusKey::SendLogsButton(self.nav.cursor(ScreenKey::SendLogs))),
            Screen::RemoveCollection => Some(ModalFocusKey::RemoveCollectionButton(
                self.nav.cursor(ScreenKey::RemoveCollection),
            )),
            Screen::ResetHdrCalibration => Some(ModalFocusKey::ResetHdrButton(
                self.nav.cursor(ScreenKey::ResetHdrCalibration),
            )),
            Screen::ResetGameSettings => Some(ModalFocusKey::ResetGameSettingsButton(
                self.nav.cursor(ScreenKey::ResetGameSettings),
            )),
            // None has a single focused widget: the address form is one always-active
            // field, and About is a scrolling document.
            Screen::Home | Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::About => None,
        };
        key.as_ref().map(cache::version)
    }

    /// Modal family: the open modal's full-screen shell tile (rebuilt only on content
    /// change, keyed by `ModalShellKey`) and its single focused-widget tile (keyed by
    /// `ModalFocusKey`). Extracted from `prepare_tiles` (A2 staging).
    fn prepare_modal(&mut self, ctx: &mut RenderCtx<'_>) -> Result<()> {
        self.snapshot_closing_modal(ctx);
        // The panel shadows, once per style epoch: small atlases the compositor stretches over
        // whatever card or popup is open (see `ui::painter::shadow_atlas`). Built here rather
        // than lazily at the first open so the open itself pays nothing for them.
        for (id, radius) in [
            (tile::MODAL_SHADOW, ui::widgets::MODAL_RADIUS),
            (tile::PANEL_SHADOW, ui::widgets::CARD_RADIUS),
        ] {
            if ctx.tiles.ensure_static(id, || {
                let atlas = ui::painter::shadow_atlas(radius).context("shadow atlas")?;
                Ok(Painter::from_pixmap(atlas))
            })? {
                ctx.updated.push(id);
            }
        }
        let (content_dirty, screen_changed) = (ctx.content_dirty, ctx.screen_changed);
        let RenderCtx {
            tiles,
            text: text_cache,
            fonts,
            screen: size,
            updated,
            scroll_list_rows,
            ..
        } = ctx;
        let (screen_w, screen_h) = (size.w, size.h);
        let modal_open = !matches!(self.nav.screen, Screen::Home);
        // Every modal's shell only reacts to *content* changes — not to
        // `content_dirty`, which is also `true` on plain focus movement (see
        // `ModalShellKey`'s docs). `AddHost` has no `ModalShellKey` variant
        // (no split focus tile to protect) and just redraws on any
        // `content_dirty` tick, same as every modal did before this split.
        // Built once for both version fns rather than per call. The power row goes with them:
        // the actions table, the shell key and the focus key all read it, and each lookup is a
        // scan of the known-host list.
        let on_host_menu = matches!(self.nav.screen, Screen::HostMenu);
        let host_menu_power = on_host_menu.then(|| self.host_menu_power_row()).flatten();
        let host_menu_actions = if on_host_menu {
            self.host_menu_actions_with(host_menu_power)
        } else {
            crate::app::state::hostmenu::HostActions::default()
        };
        let shell_version = self.modal_shell_version(&host_menu_actions, host_menu_power);
        let modal_stale = match shell_version {
            Some(_) => !tiles.contains(tile::MODAL) || self.render.modal.shell_version != shell_version,
            None => content_dirty || !tiles.contains(tile::MODAL),
        };
        // A list modal bakes its rows into the shell, and its shell key carries their values —
        // so flipping a toggle invalidates the whole card. Re-rastering it (glass blur included)
        // costs more than the 140ms slide, which then never gets a frame: the knob is already at
        // the far end by the time the card comes back. The focused row is drawn by
        // `tile::MODAL_FOCUS` on top of the shell anyway, so the stale shell underneath shows
        // nothing wrong; the rebuild lands the tick after `switch_anim` retires. Settings needs
        // none of this — its shell key holds no values (its rows are tiles of their own).
        let defer_shell = self.render.modal.switch_anim.is_some() && tiles.contains(tile::MODAL) && !screen_changed;
        if !defer_shell {
            self.render.modal.shell_version = shell_version;
        }
        if modal_open && !defer_shell && (screen_changed || modal_stale) {
            // Sized to the card's bounding box, not the whole screen: the render fns
            // below draw at absolute, screen-centered coordinates, and the painter's
            // origin shift (see `Painter::set_origin`) maps that geometry into the
            // smaller buffer.
            // Taken, not left to be dropped when the rebuilt tile replaces it: the modal is
            // re-rasterized at the same size every time, so its pixmap is reusable.
            let recycled = tiles.take(tile::MODAL);
            let mut p = self.modal_painter(recycled, screen_w, screen_h, fonts);
            let c = &mut ui::Canvas::new(&mut p, text_cache, fonts, screen_w, screen_h);
            let hover_close = self.render.hover_close;
            self.with_modal_screen(|s| s.render(c, hover_close)).transpose()?;
            // Staleness was already decided above, against `modal.shell_version` rather than
            // against the store — the keyless screens have no version to compare, so they turn
            // on `content_dirty` instead. Hence `static_version` here: the store is told to keep this
            // until something removes it, not to arbitrate.
            tiles.put(tile::MODAL, cache::static_version(), p);
            updated.push(tile::MODAL);
        }
        // Whichever modal is open has at most one focused, zoom-animated widget
        // (`ModalFocusKey`'s docs) — `None` for screens with no such widget
        // (Home, AddHost) or while a speed test is still measuring.
        if let Some(version) = self.modal_focus_version(&host_menu_actions, host_menu_power) {
            // Also stale on every tick of an in-flight `switch_anim`: the knob's
            // position depends on elapsed time, not on the key, which doesn't
            // change mid-flip.
            let stale = self.render.modal.switch_anim.is_some() || !tiles.is_fresh(tile::MODAL_FOCUS, version);
            if stale {
                // `None` where the screen turns out to have no focused widget after all —
                // the descriptor that proves the arm reachable is the same value it draws
                // from, so an arm cannot assert its way past a `None` any more.
                let tile = match self.nav.screen {
                    // The scrolling row lists: one focused row re-rendered on its own tile,
                    // over the cropped strip the rest of the list is baked into.
                    screen @ (Screen::Settings(_) | Screen::Collections) => {
                        let index = self.nav.cursor(ScreenKey::of(screen));
                        match (
                            self.scroll_list_layout(screen, screen_w, screen_h),
                            scroll_list_rows.get_or_insert_with(|| self.scroll_list_rows().unwrap_or_default()),
                        ) {
                            (Some((_, content)), rows) => {
                                let dropdown_open =
                                    self.settings_ui.dropdown.as_ref().is_some_and(|dd| dd.row == index);
                                let target_on = rows.get(index).is_some_and(|r| r.value == "On");
                                Some(ui::rasterize(
                                    ui::widgets::FocusRowTile {
                                        rows,
                                        content_width: content.width(),
                                        index,
                                        dropdown_open,
                                        switch_frac: self.toggle_frac(target_on, index),
                                        trailing_focused: self.screens.row_button.and_then(RowButton::trailing),
                                        leading_focused: self.screens.row_button == Some(RowButton::Leading),
                                        // The handle of a held row, lit for as long as it
                                        // is held: a mode must look different from a focus.
                                        leading_active: self.dragged_handle(screen),
                                    },
                                    text_cache,
                                    fonts,
                                )?)
                            }
                            (None, _) => None,
                        }
                    }
                    // Every two-button confirm dialog shares the button geometry (one subtitle
                    // sizes the card, so one button row falls out of it) and describes its own
                    // labels — one value, not a match arm per screen.
                    Screen::Wake
                    | Screen::ForgetHost
                    | Screen::SendLogs
                    | Screen::SpeedTest
                    | Screen::RemoveCollection
                    | Screen::ResetHdrCalibration
                    | Screen::ResetGameSettings => match (self.confirm_of(), self.confirm_focused()) {
                        (Some(confirm), Some(i)) => {
                            let rect = Self::confirm_focus_button_rect(screen_w, screen_h, fonts, &confirm.subtitle, i);
                            Some(ui::rasterize(
                                ui::widgets::ConfirmButtonTile {
                                    button: &confirm.widgets()[i],
                                    w: rect.width(),
                                    h: rect.height(),
                                },
                                text_cache,
                                fonts,
                            )?)
                        }
                        _ => None,
                    },
                    Screen::Pairing => Some(match self.screens.pairing_focus {
                        PairingFocus::Pin => {
                            let digit = self.screens.pin_digits[self.screens.pin_digit_index].to_string();
                            ui::rasterize(
                                ui::widgets::CardTextTile {
                                    font: fonts.title,
                                    text: &digit,
                                    w: view::pairing::DIGIT_W,
                                    h: view::pairing::DIGIT_H,
                                },
                                text_cache,
                                fonts,
                            )?
                        }
                        PairingFocus::RequestAccess => {
                            let card = view::pairing::card_rect(screen_w, screen_h, fonts);
                            let btn = view::pairing::request_button_rect(card, fonts);
                            ui::rasterize(
                                view::pairing::RequestButtonTile {
                                    w: btn.width(),
                                    h: btn.height(),
                                },
                                text_cache,
                                fonts,
                            )?
                        }
                    }),
                    // Every plain list modal: same tile, same geometry, built from whichever
                    // rows the screen lists. Only Diagnostics and Experimental can have a
                    // dropdown open, and only the host menu has a ⋯ to light.
                    Screen::HostMenu
                    | Screen::HostPower
                    | Screen::Diagnostics
                    | Screen::Experimental
                    | Screen::HdrCalibration
                    | Screen::CursorSettings(_)
                    | Screen::ControllerSettings(_) => {
                        let rows = self.list_modal_rows().unwrap_or_default();
                        let content = self.modal_list_content(screen_w, screen_h, fonts);
                        self.list_modal_focused()
                            .map(|focused| {
                                let dropdown_open =
                                    self.settings_ui.dropdown.as_ref().is_some_and(|dd| dd.row == focused);
                                let target_on = rows.get(focused).is_some_and(|r| r.value == "On");
                                ui::rasterize(
                                    ui::widgets::FocusRowTile {
                                        rows: &rows,
                                        content_width: content.width(),
                                        index: focused,
                                        dropdown_open,
                                        switch_frac: self.toggle_frac(target_on, focused),
                                        trailing_focused: self.screens.row_button.and_then(RowButton::trailing),
                                        leading_focused: self.screens.row_button == Some(RowButton::Leading),
                                        leading_active: false,
                                    },
                                    text_cache,
                                    fonts,
                                )
                            })
                            .transpose()?
                    }
                    // No single focused widget to draw — `modal_focus_version` is `None` on
                    // these, so this is the arm that never runs rather than one that panics.
                    Screen::Home | Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::About => {
                        None
                    }
                };
                if let Some(tile) = tile {
                    tiles.put(tile::MODAL_FOCUS, version, tile);
                    updated.push(tile::MODAL_FOCUS);
                }
            }
        } else {
            tiles.remove(tile::MODAL_FOCUS);
        }
        Ok(())
    }

    /// Dropdown family: the overlay panel + focused-option tile for an open Settings/Diagnostics dropdown; cleared when closed (unless a close-fade still needs them). Extracted from `prepare_tiles`.
    fn prepare_dropdown(&mut self, ctx: &mut RenderCtx<'_>) -> Result<()> {
        let RenderCtx {
            tiles,
            text: text_cache,
            fonts,
            screen: size,
            updated,
            ..
        } = ctx;
        let (screen_w, screen_h) = (size.w, size.h);
        if let Some(dd) = &self.settings_ui.dropdown {
            let options = self.dropdown_options(dd.row);
            // The overlay hangs inside whichever viewport its list is drawn in.
            let content_w = match self.nav.screen {
                s if crate::app::screens::is_list_modal(s) => {
                    self.modal_list_content(screen_w, screen_h, fonts).width()
                }
                _ => view::settings::layout(self.settings_scope(), screen_w, screen_h)
                    .1
                    .width(),
            };

            // Keyed by screen as well as row: row 0 means a different setting on Settings
            // than it does on Diagnostics.
            let overlay = cache::version(&(self.nav.screen, dd.row));
            if tiles.ensure(tile::DROPDOWN_OVERLAY, overlay, || {
                let overlay_h = options.len() as u32 * ui::widgets::DROPDOWN_OPTION_H;
                let mut p = Painter::new(content_w, overlay_h.max(1));
                let rect = Rect::new(0, 0, content_w, overlay_h);
                ui::Canvas::tile(&mut p, text_cache, fonts)
                    .render(ui::widgets::DropdownOverlay::new(&options), rect)?;
                Ok(p)
            })? {
                updated.push(tile::DROPDOWN_OVERLAY);
            }

            let focused = cache::version(&(self.nav.screen, dd.row, dd.focused));
            if tiles.ensure(tile::DROPDOWN_FOCUS, focused, || {
                let option = options.get(dd.focused).map_or("", AsRef::as_ref);
                ui::rasterize(
                    ui::widgets::DropdownOptionTile {
                        option,
                        width: content_w,
                    },
                    text_cache,
                    fonts,
                )
            })? {
                updated.push(tile::DROPDOWN_FOCUS);
            }
        } else if !self.settings_ui.dropdown_fade.is_closing() {
            // Keep the tiles cached while a close-fade is in flight — `draw_list`
            // still composites them at falling alpha.
            tiles.remove(tile::DROPDOWN_OVERLAY);
            tiles.remove(tile::DROPDOWN_FOCUS);
        }
        Ok(())
    }

    /// Releases settings-row tiles from `first` on: the tail of a list that just got
    /// shorter, or the whole band once the settings screens are left.
    fn evict_list_rows_from(&mut self, first: usize, tiles: &mut ui::cache::TileStore) {
        for i in first..tile::LIST_ROW_SLOTS {
            let Some(id) = tile::list_row(i) else { break };
            if tiles.remove(id) {
                self.render.evicted_tiles.push(id);
            }
        }
    }

    /// Scroll family: the indicator, edge-fade ramps, and windowed content tile for whichever modal overflows (Settings rows / About document). Extracted from `prepare_tiles`.
    fn prepare_scroll(&mut self, ctx: &mut RenderCtx<'_>) -> Result<()> {
        let RenderCtx {
            tiles,
            text: text_cache,
            fonts,
            screen: size,
            updated,
            scroll_list_rows,
            ..
        } = ctx;
        let (screen_w, screen_h) = (size.w, size.h);
        // The settings-row band belongs to the settings screens alone; leaving them releases
        // it rather than holding a list's worth of textures behind whatever is on screen now.
        // Stepping into a sub-page is not leaving them: the fade draws from that band, and
        // keeping it makes the return a shell rebuild rather than a whole list of rows.
        let keeps_rows = crate::app::nav::over_scroll_list(self.nav.screen)
            || self.render.modal.prev.is_some_and(|p| p.holds_list_rows());
        if !keeps_rows {
            self.evict_list_rows_from(0, tiles);
        }
        // Whichever modal's content overflows its viewport (Settings' rows, About's
        // document) gets its scroll indicator and content tile refreshed here — see
        // `scroll_geometry`'s docs for why this one block covers every such modal
        // instead of being hand-copied per screen.
        if matches!(self.nav.screen, Screen::About) {
            // Mutates `about_wrapped` only — must happen before `scroll_geometry`
            // (a `&self` read) can report a non-zero total for this frame.
            let card = view::about::card_rect(screen_w, screen_h);
            let body = view::about::body_rect(card, fonts);
            self.ensure_about_wrapped(fonts, body.width());
        }
        if let Some((total, visible, _, content)) = self.scroll_geometry(screen_w, screen_h, fonts) {
            let scroll = self.render.scroll.clamped(total, visible);
            let ind = cache::version(&(self.nav.screen, total, visible, scroll, content.height()));
            if tiles.ensure(tile::SCROLL_INDICATOR, ind, || {
                ui::rasterize(
                    ui::widgets::ListScrollbarTile {
                        w: SCROLL_INDICATOR_TILE_W,
                        h: content.height(),
                        total,
                        visible,
                        scroll,
                    },
                    text_cache,
                    fonts,
                )
            })? {
                updated.push(tile::SCROLL_INDICATOR);
            }
            let stride = self.scroll_stride(fonts);
            self.sync_modal_scroll(self.nav.screen, total, visible, content.height(), stride);

            match self.nav.screen {
                screen if is_scroll_list(screen) => {
                    let dropdown_row = self.settings_ui.dropdown.as_ref().map(|dd| dd.row);
                    let row_count = self.scroll_list_row_count();
                    let rows_version = self.scroll_list_rows_version(content.width());
                    let cached = self.render.modal.scroll_list_rows_version_cached == Some(rows_version)
                        && (0..row_count).all(|i| tile::list_row(i).is_some_and(|id| tiles.contains(id)));
                    if !cached {
                        let rows = scroll_list_rows.get_or_insert_with(|| self.scroll_list_rows().unwrap_or_default());
                        // One tile per row, each keyed on that row's own content. Rebuilding the
                        // whole list as one strip cost 25-60ms on armv7 every time a single value
                        // moved; this pays for the row that actually changed and reads the rest
                        // straight out of the cache.
                        for (i, row) in rows.iter().enumerate() {
                            let Some(id) = tile::list_row(i) else { break };
                            let key = cache::version(&(self.nav.screen, i, row.key(), dropdown_row == Some(i)));
                            if tiles.is_fresh(id, key) {
                                continue;
                            }
                            let tile = ui::rasterize(
                                ui::widgets::RowTile {
                                    row,
                                    width: content.width(),
                                    dropdown_open: dropdown_row == Some(i),
                                },
                                text_cache,
                                fonts,
                            )?;
                            tiles.put(id, key, tile);
                            updated.push(id);
                        }
                        // Slots past the end of a list that just got shorter (a sub-page is a
                        // shorter list on the same screen) would otherwise keep drawing.
                        self.evict_list_rows_from(rows.len(), tiles);
                        self.render.modal.scroll_list_rows_version_cached = Some(rows_version);
                    }
                    // Every row is baked, so the window is the whole list — the crop
                    // rebase in `scroll_src_rect` has nothing to shift.
                    self.render.content_window = ui::scroll::ContentWindow {
                        start: 0,
                        len: row_count,
                    };
                }
                Screen::About => {
                    if let Some(new_start) = self.render.content_window.recenter_if_needed(
                        scroll,
                        visible,
                        total,
                        ABOUT_WINDOW_BUDGET,
                        ABOUT_WINDOW_MARGIN,
                    ) {
                        let len = ABOUT_WINDOW_BUDGET.min(total.saturating_sub(new_start));
                        if let Some((_, wrapped)) = &self.render.about_wrapped {
                            let stride = self.scroll_stride(fonts) as u32;
                            let mut p = Painter::new(content.width().max(1), (len as u32 * stride).max(1));
                            let mut c = ui::Canvas::tile(&mut p, text_cache, fonts);
                            view::about::draw_window(&mut c, fonts.value, wrapped, new_start, len)?;
                            self.render.content_window = ui::scroll::ContentWindow { start: new_start, len };
                            let key = cache::version(&(Screen::About, ScrollContentKey::About(new_start)));
                            tiles.put(tile::SCROLL_CONTENT, key, p);
                            updated.push(tile::SCROLL_CONTENT);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Warms the caches a modal's first open would otherwise fill cold, off the
    /// open path — call once on an idle Home frame. The first real open pays a
    /// burst of `SDL2_ttf` glyph renders plus the card's shadow blur; on the very
    /// first open of a session nothing is cached, which is the hitch the user
    /// sees. Rendering the Settings shell + rows into throwaway painters here has
    /// three surviving side effects: `text_cache` (per-line pixmaps), the
    /// thread-local shadow/glow caches, and `SDL2_ttf`'s own freetype glyph cache.
    /// The last is font-wide, so it speeds up every modal's cold renders — not
    /// just Settings — even where the exact strings differ (e.g. the host menu).
    pub fn prewarm_modal_caches(
        &self,
        text_cache: &mut crate::ui::text::TextCache,
        fonts: &ui::text::Fonts,
        screen_w: u32,
        screen_h: u32,
    ) -> Result<()> {
        let mut scratch = Painter::new(screen_w, screen_h);
        view::settings::render(
            &mut ui::Canvas::new(&mut scratch, text_cache, fonts, screen_w, screen_h),
            menu::SettingsScope::Global,
            None,
            self.render.hover_close,
        )?;
        let (_, content) = view::settings::layout(menu::SettingsScope::Global, screen_w, screen_h);
        let rows = self.settings_rows();
        // One row is enough to warm the glyph cache the whole list draws from — the rest are
        // the same fonts at the same sizes.
        if let Some(row) = rows.first() {
            let _ = ui::rasterize(
                ui::widgets::RowTile {
                    row,
                    width: content.width(),
                    dropdown_open: false,
                },
                text_cache,
                fonts,
            )?;
        }
        let _ = ui::rasterize(
            ui::widgets::FocusRowTile {
                rows: &rows,
                content_width: content.width(),
                index: 0,
                dropdown_open: false,
                switch_frac: 0.0,
                trailing_focused: None,
                leading_focused: false,
                leading_active: false,
            },
            text_cache,
            fonts,
        )?;
        Ok(())
    }

    /// Rasterizes every stale tile (tiny-skia, CPU — the only place rasterization
    /// happens) and returns which tiles need their GPU texture re-uploaded.
    /// `content_dirty` is the main loop's "an event/drain changed something this
    /// tick" flag — it forces the open modal's tile to re-rasterize, since modal
    /// content has no finer dirty tracking of its own. Pure animation frames pass
    /// `false` and rasterize nothing at all. Call `advance_frame` first.
    pub fn prepare_tiles(&mut self, ctx: &mut RenderCtx<'_>) -> Result<Vec<TileId>> {
        // The transition frame's cost is what decides whether the open fade is visible.
        let started = ctx.screen_changed.then(Instant::now);
        let screen_w = ctx.screen.w;

        self.prepare_sidebar(ctx)?;
        self.prepare_grid(ctx)?;
        self.prepare_hero(ctx);

        // Status line block — built whenever `home_status` is set, independent of
        // whether a host is selected (the "Send logs" result shows here too).
        match &self.home_status {
            Some(s) => {
                if ctx.tiles.ensure(tile::STATUS, cache::version(s), || {
                    let avail = screen_w.saturating_sub(ui::widgets::SIDEBAR_W);
                    let max_w = avail.saturating_sub(2 * view::home::GRID_PAD as u32);
                    ui::rasterize(
                        ui::tiles::WrappedTextTile {
                            font: ctx.fonts.label,
                            text: s,
                            max_w,
                            color: ui::theme::palette().muted,
                            line_gap: 6,
                        },
                        ctx.text,
                        ctx.fonts,
                    )
                })? {
                    ctx.updated.push(tile::STATUS);
                }
            }
            None => {
                ctx.tiles.remove(tile::STATUS);
            }
        }

        self.prepare_modal(ctx)?;
        self.prepare_dropdown(ctx)?;
        self.prepare_scroll(ctx)?;

        // Entering a modal rasterizes it — shell, rows, and (Settings/About) the full
        // content strip: tens of ms on this SoC, more than the fade itself. `advance_frame`
        // started the clocks before all that, so re-stamp them here and the fade is
        // measured from the first frame that can show it.
        if let Some(started) = started {
            tracing::debug!(
                "entered {:?}: {} tiles rasterized in {:?}",
                self.nav.screen,
                ctx.updated.len(),
                started.elapsed()
            );
            self.render.modal.fade.restart();
        }
        Ok(std::mem::take(&mut ctx.updated))
    }
}
