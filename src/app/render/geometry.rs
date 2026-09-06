//! What the screens say, read by the painters and the pointer path alike: the open text
//! form's copy, the confirm dialog's focus, the list screens' cursor.
use crate::app::nav::ScreenKey;
use crate::app::screens::{is_confirm, is_list_modal};
use crate::app::{view, App, Screen};

impl App {
    /// The open text form — the address form (Add host / Edit address) or the collection
    /// name dialog — built from whichever screen is up. `None` off them, so a screen that
    /// types into nothing cannot fall in here and render as a form.
    ///
    /// One value rather than a copy table plus a construction site: the geometry, the
    /// painter and `SDL_SetTextInputRect` all measure the same form.
    pub(crate) fn text_form(&self) -> Option<FormCopy<'_>> {
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
            Screen::RenameProfile => (
                view::profile::RENAME_TITLE,
                view::profile::RENAME_SUBTITLE.to_string(),
                self.screens.profile_name.text(),
                self.profile_name_hint(),
            ),
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
        Some(FormCopy {
            title,
            subtitle,
            typed,
            hint,
        })
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

    /// [`list_modal_focused`](Self::list_modal_focused)'s cursor itself, for the pointer's
    /// click-moves-focus rule — same predicate, so the two cannot name different rows.
    pub(crate) fn list_modal_focused_mut(&mut self) -> Option<&mut usize> {
        is_list_modal(self.nav.screen).then(|| self.nav.cursor_mut(ScreenKey::of(self.nav.screen)))
    }
}

/// What a text form says: the copy that tells Add host, Edit address and the two rename
/// dialogs apart, plus what is typed and why it is refused.
pub(crate) struct FormCopy<'a> {
    pub title: &'static str,
    pub subtitle: String,
    pub typed: &'a str,
    pub hint: Option<&'a str>,
}
