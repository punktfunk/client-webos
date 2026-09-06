//! The gamepad shell's window onto this client's one document.
//!
//! `pf_console_ui::SettingsStore` is how a non-desktop host lends the shared shell its
//! persistence. The desktop implementation reads `trust::Settings::{load, save}` directly, which
//! this client cannot reuse: that path is hardcoded to `client-gtk-settings.json`, and the TV's
//! settings belong in its app directory beside everything else it keeps. Per-client paths are
//! already the norm — Windows has its own — so what is shared here is the SCHEMA, not the file.
//!
//! 🛑 Writes go through the app's [`StateWriter`], never straight to disk. That writer carries
//! the whole document precisely so a host edit and a settings change cannot race into
//! disagreeing files, and it drops snapshots equal to the last. A second writer behind its back
//! would resurrect exactly the race its docs describe: the app's next save would carry its own
//! stale `settings` and silently undo whatever the shell had just written.
//!
//! Only one UI is live at a time (that is what the flip means), so this owns the document while
//! the console is up and [`ConsoleStore::snapshot`] hands it back when the console closes.

use std::sync::{Arc, Mutex};

use pf_client_core::trust;
use pf_console_ui::SettingsStore;

use super::shared;
use super::{Persisted, StateWriter};

/// Poisoning means a panic while holding the document; nothing here can recover from it.
const POISONED: &str = "console-store mutex poisoned";

pub struct ConsoleStore {
    state: Mutex<Persisted>,
    writer: Arc<StateWriter>,
}

impl ConsoleStore {
    /// `state` is the document as the app has it right now; `writer` is the app's own.
    pub fn new(state: Persisted, writer: Arc<StateWriter>) -> Self {
        Self {
            state: Mutex::new(state),
            writer,
        }
    }

    /// The document as the console leaves it — what the other UI adopts when the flip returns,
    /// so a setting changed in the shell is not stale in the old screens.
    pub fn snapshot(&self) -> Persisted {
        self.state.lock().expect(POISONED).clone()
    }

    /// Change the document and persist all of it. The host-list commands (save, edit, forget)
    /// go through here for the same reason [`SettingsStore::save`] does: one writer, whole
    /// document, so a host edit and a settings change cannot race into disagreeing files.
    ///
    /// Returns whether `edit` reported a change; a command naming a host this client does not
    /// know says `false` and nothing is written.
    pub fn edit(&self, edit: impl FnOnce(&mut Persisted) -> bool) -> bool {
        let snapshot = {
            let mut state = self.state.lock().expect(POISONED);
            if !edit(&mut state) {
                return false;
            }
            state.clone()
        };
        self.writer.save(snapshot);
        true
    }
}

impl SettingsStore for ConsoleStore {
    fn load(&self) -> trust::Settings {
        self.state.lock().expect(POISONED).settings.clone()
    }

    fn save(&self, settings: &trust::Settings) {
        let snapshot = {
            let mut state = self.state.lock().expect(POISONED);
            state.settings = settings.clone();
            state.clone()
        };
        // Whole document, one writer — see the module note.
        self.writer.save(snapshot);
    }

    /// The document's catalog, in display order. Non-empty as soon as one game has its own
    /// settings: giving a game an override IS creating a profile here (`shared::bind_game_
    /// overrides`), which is what lets a TV with no desktop app beside it fill this list at all.
    fn profiles(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .expect(POISONED)
            .profiles
            .iter()
            .map(|p| (p.id.clone(), p.name.clone()))
            .collect()
    }

    fn known_hosts(&self) -> trust::KnownHosts {
        let state = self.state.lock().expect(POISONED);
        trust::KnownHosts {
            hosts: state.known_hosts.iter().map(shared::to_shared_host).collect(),
        }
    }
}

// No tests here on purpose. This module is arm-gated with pf-console-ui, and `task test` builds
// the HOST target — an armv7 test binary cannot execute on a runner, so anything asserted here
// would type-check and never run. The conversions it delegates to live in `shared`, which is not
// gated and is tested there; what is left in this file is a mutex, a clone and a writer call.
