//! Geometry both render halves read: what scrolls, how far, and how a tile is cropped to
//! its viewport.
//!
//! Shared on purpose — `prepare`'s staleness checks and `compose`'s GPU-crop math have to
//! agree about a scrollable modal's extent, and deriving it twice is how they stop
//! agreeing.
use crate::app::nav::ScreenKey;
use crate::app::screens::scrolllist::scroll_list_width_frac;
use crate::app::screens::{is_confirm, is_list_modal, is_scroll_list};
use crate::app::{view, App, PairingFocus, Screen};
use crate::ui;
use crate::ui::render::Rect;
use crate::ui::Painter;

impl App {
    /// `(total units, visible units, card rect, content/viewport rect)` for whichever
    /// scrollable modal is open — `None` if `self.nav.screen` has no overflowing content.
    /// The one place this per-modal geometry lives, shared by `prepare_tiles`'s
    /// staleness checks and `draw_list`'s GPU-crop math so the two can't disagree.
    /// `About`'s `total` depends on `about_wrapped` already being fresh for this
    /// frame's body width — `prepare_tiles` ensures that before calling this;
    /// `draw_list` runs after `prepare_tiles` in the same frame, so it's already set.
    pub(crate) fn scroll_geometry(
        &self,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<(usize, usize, Rect, Rect)> {
        self.scroll_geometry_for(self.nav.screen, screen_w, screen_h, fonts)
    }

    /// Same as `scroll_geometry`, but for an explicit screen — `snapshot_closing_modal`
    /// needs the screen being *left*, which `self.nav.screen` has already moved off.
    pub(crate) fn scroll_geometry_for(
        &self,
        screen: Screen,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<(usize, usize, Rect, Rect)> {
        match screen {
            // The scope comes off the passed screen, not `self.nav.screen`: a closing Settings(Game)
            // is asked about after `self.nav.screen` has moved on, and reading the live scope there
            // measures the global list instead of the one being faded out.
            Screen::Settings(_) | Screen::Collections => {
                let total = self.scroll_list_row_count_for(screen);
                let (card, content) =
                    view::scrolllist::layout(total, screen_w, screen_h, scroll_list_width_frac(screen));
                Some((total, view::scrolllist::visible_rows(total, screen_h), card, content))
            }
            Screen::About => {
                let card = view::about::card_rect(screen_w, screen_h);
                let body = view::about::body_rect(card, fonts);
                let total = self.render.about_wrapped.as_ref().map_or(0, |(_, v)| v.len());
                let visible = view::about::visible_lines(body, fonts.raster, fonts.value);
                Some((total, visible, card, body))
            }
            _ => None,
        }
    }

    /// Clips a tile's destination to `clip`, returning `(source crop, clipped destination)`.
    ///
    /// The tile's full extent is assumed to map onto `dst`, so the crop is proportional —
    /// which keeps this correct even while `dst` is being zoom-animated. `None` when nothing
    /// of it remains inside.
    pub(crate) fn clip_tile(dst: Rect, clip: Rect, tile_w: u32, tile_h: u32) -> Option<(Rect, Rect)> {
        let visible = dst.intersection(clip)?;
        if visible == dst {
            return Some((Rect::new(0, 0, tile_w, tile_h), dst));
        }
        if dst.width() == 0 || dst.height() == 0 {
            return None;
        }
        let fx = |v: i32| (f64::from(v) / f64::from(dst.width())) * f64::from(tile_w);
        let fy = |v: i32| (f64::from(v) / f64::from(dst.height())) * f64::from(tile_h);
        let src = Rect::new(
            fx(visible.x() - dst.x()).round() as i32,
            fy(visible.y() - dst.y()).round() as i32,
            (fx(visible.width() as i32).round() as u32).max(1),
            (fy(visible.height() as i32).round() as u32).max(1),
        );
        Some((src, visible))
    }

    /// The furthest the viewport may be cropped down: the last unit sits flush with the
    /// viewport's bottom edge rather than scrolling past it.
    ///
    /// This is why the rendered offset is pixels and not units — `offset * stride` overshoots
    /// by exactly the peek strip at the end of the list, which would show a dead band below
    /// the final row (and is what the row-quantized version did).
    pub(crate) fn max_scroll_px(total: usize, stride: i32, viewport_h: u32) -> i32 {
        (total as i32 * stride - viewport_h as i32).max(0)
    }

    /// The animated scroll offset, held inside the range this list can actually travel.
    ///
    /// `modal.scroll_px` is the raw target the ease writes; every reader wants it clamped, and
    /// the clamp used to be spelled out at each of them.
    /// `screen`'s scrolling body as the compose and snapshot paths want it: `(row/line count,
    /// viewport, clamped pixel offset)`. One derivation, so a screen with a non-row stride
    /// cannot come out at two different offsets depending on who asked.
    pub(crate) fn scroll_view_for(
        &self,
        screen: Screen,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<(usize, Rect, i32)> {
        let (total, _, _, content) = self.scroll_geometry_for(screen, screen_w, screen_h, fonts)?;
        let stride = self.scroll_stride_for(screen, fonts);
        Some((total, content, self.clamped_scroll_px(total, stride, content.height())))
    }

    pub(crate) fn clamped_scroll_px(&self, total: usize, stride: i32, viewport_h: u32) -> i32 {
        self.render
            .modal
            .scroll_px
            .clamp(0, Self::max_scroll_px(total, stride, viewport_h))
    }

    /// Which slice of `screen`'s baked `tile::SCROLL_CONTENT` is showing, as `(src crop,
    /// dst rect)` — `None` for a screen whose body lives in its shell tile.
    ///
    /// The one place the window rebase lives: `compose_modal` draws the live modal with it,
    /// `snapshot_closing_modal` freezes the same crop for the fading one.
    pub(crate) fn scroll_src_rect(
        &self,
        screen: Screen,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<(Rect, Rect)> {
        let (total, _, _, content) = self.scroll_geometry_for(screen, screen_w, screen_h, fonts)?;
        // About uses a bounded window; for other screens, window_start is 0.
        let window_start = match screen {
            Screen::About => self.render.content_window.start,
            _ => 0,
        };
        let stride = self.scroll_stride_for(screen, fonts);
        // The animated offset (see `sync_modal_scroll`), in absolute content pixels,
        // rebased onto whatever slice is currently baked into the tile.
        let scroll_px = self.clamped_scroll_px(total, stride, content.height());
        let src = Rect::new(
            0,
            scroll_px - window_start as i32 * stride,
            content.width(),
            content.height(),
        );
        Some((src, content))
    }

    /// Re-derives `modal_scroll_target_px` from the integral offset, snapping rather than
    /// gliding when the scrolling modal changed. Called once per frame from `update_tiles`,
    /// which is where the geometry (and the fonts About's stride needs) is already in hand.
    ///
    /// Kept in absolute content pixels, *not* relative to the baked window: About re-bakes its
    /// window later in the same pass, and a window-relative target would jump by the whole
    /// window offset on the frame that happens — a full-document glide instead of a scroll.
    /// `draw_list` subtracts the window when it crops.
    pub(crate) fn sync_modal_scroll(
        &mut self,
        screen: Screen,
        total: usize,
        visible: usize,
        viewport_h: u32,
        stride: i32,
    ) {
        let offset = self.render.scroll.clamped(total, visible);
        // Biased back by one peek so the *top* edge also cuts mid-row: sitting on the row grid
        // would put nothing but the gap between rows under the top fade, which is invisible
        // (see `view::scrolllist::PEEK`). The clamps then pin the first and last positions
        // flush, where there is genuinely nothing beyond the edge to hint at. About scrolls
        // wrapped text, not rows, and has no peek.
        let bias = if is_scroll_list(screen) {
            view::scrolllist::PEEK as i32
        } else {
            0
        };
        let target = (offset as i32 * stride - bias)
            .min(Self::max_scroll_px(total, stride, viewport_h))
            .max(0);
        self.render.modal.scroll_target_px = target;
        if self.render.modal.scroll_screen != Some(screen) {
            self.render.modal.scroll_screen = Some(screen);
            self.render.modal.scroll_px = target;
        }
    }

    /// Pixel stride between two consecutive units of whichever modal is scrolling —
    /// Settings' fixed row height, or About's wrapped-line height. Only meaningful
    /// when `scroll_geometry` returns `Some`.
    pub(crate) fn scroll_stride(&self, fonts: &ui::text::Fonts) -> i32 {
        self.scroll_stride_for(self.nav.screen, fonts)
    }

    /// Same as `scroll_stride`, but for an explicit screen — see `scroll_geometry_for`.
    pub(crate) fn scroll_stride_for(&self, screen: Screen, fonts: &ui::text::Fonts) -> i32 {
        match screen {
            Screen::Settings(_) | Screen::Collections => ui::widgets::focus_row_stride() as i32,
            Screen::About => view::about::line_stride(fonts.raster, fonts.value),
            // Nothing else has a scrolling body. `1` rather than `0` because the stride is a
            // divisor in the scroll arithmetic, and this is only reached where
            // `scroll_geometry` already said there is nothing to scroll.
            Screen::Home
            | Screen::Pairing
            | Screen::AddHost
            | Screen::Wake
            | Screen::ForgetHost
            | Screen::HostMenu
            | Screen::EditHost
            | Screen::SpeedTest
            | Screen::HostPower
            | Screen::Diagnostics
            | Screen::Experimental
            | Screen::HdrCalibration
            | Screen::CursorSettings(_)
            | Screen::ControllerSettings(_)
            | Screen::SendLogs
            | Screen::RenameCollection
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::ResetGameSettings => 1,
        }
    }

    /// The open text form — the address form (Add host / Edit address) or the collection
    /// name dialog — built from whichever screen is up. `None` off them, so a screen that
    /// types into nothing cannot fall in here and render as a form.
    ///
    /// One value rather than a copy table plus a construction site: the geometry, the
    /// painter and `SDL_SetTextInputRect` all measure the same form.
    pub(crate) fn text_form(&self) -> Option<view::textform::Modal<'_>> {
        let (title, subtitle, typed, hint) = match self.nav.screen {
            Screen::EditHost => {
                let name = self
                    .screens
                    .edit_host_index
                    .and_then(|i| self.hosts.entries.get(i))
                    .map_or_else(String::new, |e| e.name().to_string());
                (
                    view::addhost::EDIT_TITLE,
                    view::addhost::edit_subtitle(&name),
                    self.screens.add_host.text(),
                    None,
                )
            }
            Screen::AddHost => (
                view::addhost::ADD_TITLE,
                view::addhost::ADD_SUBTITLE.to_string(),
                self.screens.add_host.text(),
                None,
            ),
            Screen::RenameCollection => {
                let renaming = self
                    .screens
                    .collections
                    .renaming
                    .and_then(|i| self.selected_known_host()?.collections().get(i))
                    .map(|c| c.name.clone());
                (
                    if renaming.is_some() {
                        view::collections::RENAME_TITLE
                    } else {
                        view::collections::ADD_TITLE
                    },
                    view::collections::name_subtitle(renaming.as_deref(), &self.screens.collections.title),
                    self.screens.collections.name.text(),
                    self.collection_name_hint(),
                )
            }
            _ => return None,
        };
        Some(view::textform::Modal {
            title,
            subtitle,
            typed,
            hint,
            keyboard_shown: self.keyboard_shown,
        })
    }

    /// The current screen's modal card rect, or `None` for a screen that draws no
    /// modal card (Home, or Wake before its payload is set). Measured off the same
    /// [`ui::ModalScreen`] value the renderer draws, so hover, click and the
    /// close-button hit-test can never drift from what is on screen.
    pub(crate) fn modal_card_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> Option<Rect> {
        self.with_modal_metrics(|s| s.card_rect(screen_w, screen_h, fonts))
    }

    /// The open modal's row-list viewport, or an empty rect on a screen without one —
    /// for the tile builders, which only reach here on a screen that has one.
    pub(crate) fn modal_list_content(&self, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> Rect {
        self.modal_list_geometry(screen_w, screen_h, fonts)
            .map_or_else(|| Rect::new(0, 0, 0, 0), |(_, content)| content)
    }

    /// The open modal's card and row-list viewport, when it has one. The pair every hit
    /// test and focused-row tile needs, derived once from the screen that draws them.
    pub(crate) fn modal_list_geometry(
        &self,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<(Rect, Rect)> {
        self.with_modal_metrics(|s| {
            let card = s.card_rect(screen_w, screen_h, fonts);
            s.content_rect(card, fonts).map(|content| (card, content))
        })?
    }

    /// The subtitle of the open two-button confirm modal — the string its card height and
    /// button-row rect are both measured from, so one value drives the whole dialog.
    ///
    /// `None` on any other screen, and on the two confirm screens whose buttons aren't up
    /// yet (see [`App::confirm_of`](crate::app::screens::confirm)).
    pub(crate) fn confirm_subtitle(&self) -> Option<String> {
        self.confirm_of().map(|c| c.subtitle)
    }

    /// Which of the open confirm dialog's two buttons has focus; `None` on a screen that
    /// isn't one, and on a `Wake` with no payload yet.
    ///
    /// Every screen's cursor is `nav`'s (see [`nav::Nav`](crate::app::nav::Nav)) — this only
    /// says which screens are confirm dialogs. Wake is the exception: its cursor rides in the
    /// payload that is `None` off-screen, so it has nowhere else to live.
    pub(crate) fn confirm_focused(&self) -> Option<usize> {
        match self.nav.screen {
            Screen::Wake => Some(self.screens.wake.as_ref()?.focused),
            screen if is_confirm(screen) => Some(self.nav.cursor(ScreenKey::of(screen))),
            _ => None,
        }
    }

    /// Moves the open confirm dialog's focus onto button `index`, reporting whether it
    /// actually moved — the hover/click contract every focus setter here follows.
    pub(crate) fn set_confirm_focused(&mut self, index: usize) -> bool {
        let Some(focused) = (match self.nav.screen {
            Screen::Wake => self.screens.wake.as_mut().map(|w| &mut w.focused),
            screen if is_confirm(screen) => Some(self.nav.cursor_mut(ScreenKey::of(screen))),
            _ => None,
        }) else {
            return false;
        };
        let changed = *focused != index;
        *focused = index;
        changed
    }

    /// Rect of the focused widget of whichever modal `screen` is — the setting row, the
    /// confirm button, the pairing digit, the list row. This is what `tile::MODAL_FOCUS`
    /// composites at, and what the press dip scales; `None` for the screens whose focus
    /// is baked into their shell (Home's grid has its own pop).
    pub(crate) fn modal_focus_rect(
        &self,
        screen: Screen,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Option<Rect> {
        match screen {
            // The scrolling row lists: one focused row, positioned the same way on both.
            Screen::Settings(_) | Screen::Collections => {
                let (total, _, _, content) = self.scroll_geometry_for(screen, screen_w, screen_h, fonts)?;
                // Positioned from the animated pixel offset, not the row index: the baked
                // list is cropped at that offset, and the focus tile *is* the focused row
                // re-rendered — so anchoring it to the quantized row would show that row's
                // content twice, in two places, for the length of every scroll.
                let stride = ui::widgets::focus_row_stride() as i32;
                let px = self.clamped_scroll_px(total, stride, content.height());
                Some(ui::widgets::focus_row_rect_at_px(
                    content,
                    self.nav.cursor(ScreenKey::of(screen)),
                    px,
                ))
            }
            // Every two-button confirm dialog: one subtitle drives the card, so one
            // button-row geometry serves all four.
            Screen::Wake
            | Screen::ForgetHost
            | Screen::SendLogs
            | Screen::SpeedTest
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::ResetGameSettings => self
                .confirm_subtitle()
                .zip(self.confirm_focused())
                .map(|(subtitle, i)| Self::confirm_focus_button_rect(screen_w, screen_h, fonts, &subtitle, i)),
            Screen::Pairing => {
                let card = view::pairing::card_rect(screen_w, screen_h, fonts);
                Some(match self.screens.pairing_focus {
                    PairingFocus::Pin => {
                        let digit_y = view::pairing::pin_row_y(card, fonts);
                        view::pairing::digit_rect(card, digit_y, self.screens.pin_digit_index)
                    }
                    PairingFocus::RequestAccess => view::pairing::request_button_rect(card, fonts),
                })
            }
            // Every plain list modal: one geometry, measured off the `ModalScreen`
            // the painter draws, indexed by that screen's own focus cursor.
            Screen::HostMenu
            | Screen::HostPower
            | Screen::Diagnostics
            | Screen::Experimental
            | Screen::HdrCalibration
            | Screen::CursorSettings(_)
            | Screen::ControllerSettings(_) => self.list_modal_focus_rect(screen_w, screen_h, fonts),
            // No single focused widget: a text form is one always-active field, and About is
            // a scrolling document.
            Screen::Home | Screen::AddHost | Screen::EditHost | Screen::RenameCollection | Screen::About => None,
        }
    }

    /// The open list modal's focused row index — the cursor `focus_row_rect` indexes with.
    /// `None` on a screen that has no plain row list (Settings scrolls, and owns its own
    /// geometry).
    ///
    /// One cursor per screen, so a nested menu keeps its place on the way back; which cursor
    /// is `nav`'s business, and this only says which screens are plain lists.
    pub(crate) fn list_modal_focused(&self) -> Option<usize> {
        is_list_modal(self.nav.screen).then(|| self.nav.cursor(ScreenKey::of(self.nav.screen)))
    }

    /// [`list_modal_focused`](Self::list_modal_focused)'s cursor itself, for the pointer's
    /// click-moves-focus rule — same predicate, so the two cannot name different rows.
    pub(crate) fn list_modal_focused_mut(&mut self) -> Option<&mut usize> {
        is_list_modal(self.nav.screen).then(|| self.nav.cursor_mut(ScreenKey::of(self.nav.screen)))
    }

    /// The open list modal's focused-row rect, positioned on screen. `None` unless the
    /// current screen is one of the plain list modals.
    pub(crate) fn list_modal_focus_rect(&self, screen_w: u32, screen_h: u32, fonts: &ui::text::Fonts) -> Option<Rect> {
        let (_, content) = self.modal_list_geometry(screen_w, screen_h, fonts)?;
        Some(ui::widgets::focus_row_rect(content, self.list_modal_focused()?))
    }

    /// Calls `f` with the open modal's *geometry* — [`ui::ModalMetrics`], the half of a modal
    /// screen that says where its card and rows are without saying what is written on them.
    ///
    /// Every screen but the host menu measures off values it already holds, so this is
    /// [`with_modal_screen`](Self::with_modal_screen) with one arm taken early: the host
    /// menu's rows are owned `String`s built per call, and this runs on every Magic Remote
    /// `MouseMotion`.
    pub(crate) fn with_modal_metrics<R>(&self, f: impl FnOnce(&dyn ui::ModalMetrics) -> R) -> Option<R> {
        if matches!(self.nav.screen, Screen::HostMenu) {
            return Some(f(&view::hostmenu::Metrics {
                subtitle: &self.host_menu_subtitle(),
                rows: self.host_menu_actions().len(),
            }));
        }
        self.with_modal_screen(|s| f(s))
    }

    /// By closure rather than by return value: the hit tests run this on every Magic Remote
    /// `MouseMotion`, and a returned `Box<dyn ModalScreen>` would put a heap allocation on
    /// that path for what is a geometry query.
    pub(crate) fn with_modal_screen<R>(&self, f: impl FnOnce(&dyn ui::ModalScreen) -> R) -> Option<R> {
        // The dialogs' labels and subtitle, from the one place that knows them. Bound here so
        // the borrowed `ConfirmButton`s below outlive the call.
        let confirm = self.confirm_of();
        Some(match self.nav.screen {
            Screen::Home => return None,
            // One screen, two scopes: the dim title suffix is the only thing the per-game
            // one adds, and it comes from the scratch copy that scope implies.
            Screen::Settings(set) => f(&view::settings::Modal {
                set,
                game: self.editing_game().map(|gs| gs.title.as_str()),
            }),
            Screen::Collections => f(&view::collections::Modal {
                rows: self.collections_row_count(),
                title: view::collections::heading(self.collections_target_held()),
                card: Some(self.collections_heading()),
            }),
            Screen::Pairing => f(&view::pairing::Modal {
                pin_digits: &self.screens.pin_digits,
                status: self.screens.pairing_status.as_ref(),
                busy: self.screens.pairing_busy,
            }),
            // Every one-field text form, described once by `text_form`.
            Screen::AddHost | Screen::EditHost | Screen::RenameCollection => f(&self.text_form()?),
            Screen::Wake => f(&view::confirm::Modal {
                title: view::wake::TITLE,
                confirm: confirm.as_ref()?,
            }),
            Screen::ForgetHost => f(&view::confirm::Modal {
                title: view::forget::TITLE,
                confirm: confirm.as_ref()?,
            }),
            Screen::HostMenu => f(&view::hostmenu::Modal {
                title: self.host_menu_host_name().unwrap_or_default(),
                subtitle: self.host_menu_subtitle(),
                rows: self.host_menu_rows(),
            }),
            Screen::HostPower => {
                let (auto_send, exit_action, access) = self.host_power_view();
                f(&view::hostpower::Modal {
                    host_name: self.host_menu_host_name().unwrap_or_default(),
                    auto_send,
                    exit_action,
                    access,
                })
            }
            Screen::About => f(&view::about::Modal),
            Screen::SpeedTest => f(&view::speedtest::Modal {
                state: self.screens.speed_test.as_ref(),
                host_name: &self.screens.speed_test_name,
                confirm: confirm.as_ref(),
            }),
            Screen::Diagnostics => f(&view::diagnostics::Modal {
                settings: &self.settings_ui.settings,
                to_host: self.send_logs_host_ready(),
            }),
            Screen::HdrCalibration => f(&self.hdr_calibration_view()?),
            Screen::Experimental => f(&view::experimental::Modal {
                settings: &self.settings_ui.settings,
                rooted: self.hosts.rooted,
            }),
            Screen::ControllerSettings(_) => f(&view::controllersettings::Modal {
                settings: self.settings_target(),
                detected_gamepad_type: self.detected_gamepad_type,
                dualsense_limited: self.dualsense_limited(),
                webos_major: self.webos_major(),
            }),
            Screen::CursorSettings(_) => f(&view::cursorsettings::Modal {
                settings: self.settings_target(),
                over: &self.editing_override(),
            }),
            Screen::SendLogs => f(&view::confirm::Modal {
                title: view::sendlogs::TITLE,
                confirm: confirm.as_ref()?,
            }),
            Screen::RemoveCollection => f(&view::confirm::Modal {
                title: view::collections::REMOVE_TITLE,
                confirm: confirm.as_ref()?,
            }),
            Screen::ResetHdrCalibration => f(&view::confirm::Modal {
                title: view::hdrcalibration::RESET_TITLE,
                confirm: confirm.as_ref()?,
            }),
            Screen::ResetGameSettings => f(&view::confirm::Modal {
                title: view::resetgame::TITLE,
                confirm: confirm.as_ref()?,
            }),
        })
    }

    /// A painter for the current screen's modal, sized and positioned to the card itself
    /// rather than to the whole screen. Nothing is drawn outside the card — the shadow is
    /// the compositor's nine-slice (`compose::push_card_shadow`) and the focus pop has its
    /// own tile — so the card needs no margin. Records the region in `modal_tile_region`,
    /// which is where
    /// `compose_modal` composites the tile. Falls back to full-screen on a screen with
    /// no card (shouldn't happen with one open).
    /// `recycled` is the tile's own previous surface, when it had one: a modal with no
    /// version key (`AddHost`) rebuilds on every keystroke, and its card is large enough that
    /// allocating a fresh pixmap per character was the cost of the rebuild. Reused only at
    /// the same size, and wiped first — the card is rounded, so its corners are never drawn
    /// over.
    pub(crate) fn modal_painter(
        &mut self,
        recycled: Option<Painter>,
        screen_w: u32,
        screen_h: u32,
        fonts: &ui::text::Fonts,
    ) -> Painter {
        let region = self
            .modal_card_rect(screen_w, screen_h, fonts)
            .unwrap_or_else(|| Rect::new(0, 0, screen_w, screen_h));
        self.render.modal.tile_region = region;
        let mut p = Painter::recycle(recycled, region.width(), region.height());
        p.set_origin(region.x(), region.y());
        p
    }
}
