//! Activating the focused widget: the press dip, and the one table that routes a
//! `MenuEvent` to whichever screen is up.
//!
//! The dip is kept only for a press that stays on its screen — "request access", the
//! speed test's retry, picking a host. Anything that opens or closes a modal drops it:
//! the modal fade (75ms, full-screen scrim) darkens the pressed row and replaces the
//! tile that was dipping, so the 5px dip underneath is never seen. One motion per press.

use crate::app::{App, ConnectTarget, HomeFocus, PairingFocus, Screen};
use crate::core::event::MenuEvent;
use crate::ui;

impl App {
    /// Routes one `MenuEvent` to the open screen — the single dispatch table, which
    /// `press`, `back` and the runtime's event pump all come through. `Some` only when
    /// the event launched a stream.
    pub(crate) fn handle_menu_event(&mut self, ev: MenuEvent, screen_w: u32, screen_h: u32) -> Option<ConnectTarget> {
        // Anything but a confirm moves focus or closes the screen, so a dip still running
        // from an earlier press belongs to a widget that is no longer under the cursor.
        if ev != MenuEvent::Confirm {
            self.render.press.take();
        }
        self.kit_list_visual(ev);
        match self.nav.screen {
            Screen::Home => return self.handle_home_event(ev, screen_w, screen_h),
            Screen::Pairing => self.handle_pairing_event(ev),
            Screen::AddHost => self.handle_add_host_event(ev),
            Screen::Wake => self.handle_wake_event(ev),
            Screen::ForgetHost => self.handle_forget_host_event(ev),
            Screen::HostMenu => self.handle_host_menu_event(ev),
            Screen::HostPower => self.handle_host_power_event(ev),
            Screen::PickProfile => self.handle_pick_profile_event(ev),
            Screen::SpeedTest => self.handle_speed_test_event(ev),
            Screen::EditHost => self.handle_edit_host_event(ev),
            Screen::About => self.handle_about_event(ev, screen_w, screen_h),
            Screen::HdrCalibration => self.handle_hdr_calibration_event(ev),
            Screen::SendLogs => self.handle_send_logs_event(ev),
            Screen::Collections => self.handle_collections_event(ev, screen_w, screen_h),
            Screen::RenameCollection => self.handle_name_collection_event(ev, screen_w, screen_h),
            Screen::RemoveCollection => self.handle_remove_collection_event(ev),
            Screen::ResetHdrCalibration => self.handle_reset_hdr_event(ev),
            Screen::SettingsPage => self.handle_settings_page_event(ev),
            Screen::RenameProfile => self.handle_rename_profile_event(ev),
            Screen::DeleteProfile => self.handle_delete_profile_event(ev),
        }
        None
    }

    /// Confirms the focused widget, dipping it if it is a button whose action stays put.
    ///
    /// The action runs immediately; the dip is retired afterwards rather than gated
    /// beforehand, because whether a button opens anything is the screen handler's
    /// business and a list of which ones do would be one more thing to keep in step.
    pub(crate) fn press(&mut self, screen_w: u32, screen_h: u32) -> Option<ConnectTarget> {
        let before = self.nav.screen;
        if self.pressable() {
            self.render.press.arm();
        }
        let launched = self.handle_menu_event(MenuEvent::Confirm, screen_w, screen_h);
        if self.nav.screen != before || launched.is_some() {
            self.render.press.take();
        }
        launched
    }

    /// The dip, but only for the screen that armed it — which, since a press leaving its
    /// screen drops the dip, is simply the one that is up. Every focus tile on screen
    /// composites through `App::press`, so without this an open modal's confirm would push
    /// the sidebar row behind its card in alongside the button actually pressed.
    pub(crate) fn press_dip(&self, owner: Screen) -> ui::animation::Press {
        if self.nav.screen == owner {
            self.render.press
        } else {
            ui::animation::Press::default()
        }
    }

    /// Whether the focused widget is a button — the only thing that dips. A row carries a
    /// value the press changes in place, and pushing a full-width row in for that reads as
    /// the list lurching; a button *is* its action.
    fn pressable(&self) -> bool {
        match self.nav.screen {
            // Sidebar rows are buttons: pick a host, add one, open Settings. A grid card
            // isn't — launching one is already an animation of its own.
            Screen::Home => matches!(self.home_focus, HomeFocus::Sidebar(_) | HomeFocus::SidebarMenu(_)),
            // The button only; the PIN digits above it are a field.
            Screen::Pairing => matches!(self.screens.pairing_focus, PairingFocus::RequestAccess),
            Screen::Wake
            | Screen::ForgetHost
            | Screen::SpeedTest
            | Screen::SendLogs
            | Screen::RemoveCollection
            | Screen::ResetHdrCalibration
            | Screen::DeleteProfile => true,
            // Rows, not buttons.
            Screen::SettingsPage
            | Screen::RenameProfile
            | Screen::AddHost
            | Screen::EditHost
            | Screen::About
            | Screen::HostMenu
            | Screen::HostPower
            | Screen::PickProfile
            | Screen::HdrCalibration
            | Screen::Collections
            | Screen::RenameCollection => false,
        }
    }

    /// Retires a dip that has played out; called every frame. `true` means the tile moved
    /// back, so the frame is dirty.
    pub(crate) fn poll_press(&mut self) -> bool {
        self.render.press.landed() && self.render.press.take()
    }
}
