//! Persistence: the `settings.json` document, plus the client identity PEMs beside it.
//!
//! - [`load`] / [`save`] read and write the whole [`Persisted`] document.
//! - [`StateWriter`] is what the app actually saves through: off-thread, coalescing.
//! - [`load_or_create_identity`] handles the PEM pair, which stays outside the document.
// The shell's `SettingsStore`. Gated with pf-console-ui itself (see Cargo.toml).
#[cfg(target_os = "linux")]
pub mod console;
mod identity;
pub mod shared;
mod writer;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::core::VERSION;

pub use crate::core::model::{AudioRoutePref, CodecPref, ExitAction, GamepadType, Persisted};
pub use crate::core::settings::TvSettings;
pub use crate::services::paths::app_dir;
pub use identity::load_or_create_identity;
pub use pf_client_core::trust::Settings;
pub use writer::StateWriter;

fn path() -> PathBuf {
    app_dir().join("settings.json")
}

/// Set by a [`load`] that found the document and could not read it: [`save`] then refuses,
/// so one bad read never replaces every pairing with defaults.
static UNREADABLE: AtomicBool = AtomicBool::new(false);

/// Loads the whole persisted document, migrated ([`shared::migrate_mouse_mode`]) and stamped.
/// Absent, unreadable and unparseable all answer with defaults: a bad file must not take the
/// app down.
pub fn load() -> Persisted {
    let (mut state, readable) = read_document(&path());
    UNREADABLE.store(!readable, Ordering::Relaxed);
    let migrated = shared::migrate_mouse_mode(&mut state);
    stamp_version(&mut state, migrated);
    // A document written on a more capable TV can hold HEVC, HDR and 7.1 on a device with none
    // of them — leaving a *set* value whose row the UI hides.
    state.settings.clamp_to_caps();
    state
}

/// The document at `path` or defaults, and whether a save may replace it. No migration: a
/// document that does not parse is renamed to `settings.json.bad` first, because the version
/// stamp right after would write the defaults over the only copy of every paired host. One
/// that exists but can't be read stays as it is, and nothing is saved over it.
fn read_document(path: &Path) -> (Persisted, bool) {
    let why = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Persisted>(&bytes) {
            Ok(state) => return (state, true),
            Err(e) => e.to_string(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Persisted::default(), true),
        Err(e) => {
            tracing::warn!(error = %e, "settings.json not read, so nothing is saved over it");
            return (Persisted::default(), false);
        }
    };
    let mut aside = path.as_os_str().to_os_string();
    aside.push(".bad");
    match std::fs::rename(path, &aside) {
        Ok(()) => {
            tracing::warn!(%why, "settings.json did not parse — kept as settings.json.bad, starting from defaults")
        }
        Err(e) => tracing::warn!(%why, "settings.json did not parse and was not set aside: {e}"),
    }
    (Persisted::default(), true)
}

/// Brings [`Persisted::version`] up to this build's, and saves when that or a migration
/// (`migrated`) changed the document.
///
/// Written synchronously so it happens once — `StateWriter`'s baseline is taken after this,
/// and an unstamped document that never gets saved would stamp again on every launch.
fn stamp_version(state: &mut Persisted, migrated: bool) {
    if !migrated && state.version.as_deref() == Some(VERSION) {
        return;
    }
    state.version = Some(VERSION.to_string());
    match save(state) {
        Ok(()) => tracing::info!("stamped settings.json with version {VERSION}"),
        Err(e) => tracing::warn!("could not stamp document version: {e:#}"),
    }
}

/// Writes the whole document: its `settings` object is the shared schema verbatim (see [`shared`]).
/// Refused while the document on disk could not be read.
pub fn save(state: &Persisted) -> Result<()> {
    anyhow::ensure!(
        !UNREADABLE.load(Ordering::Relaxed),
        "settings.json exists but can't be read"
    );
    let json = serde_json::to_string_pretty(state).context("serialize app state")?;
    crate::services::atomic::write(&path(), &json, "settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document this build cannot parse is kept beside the defaults, never overwritten by
    /// them. One that can't be read stays where it is, and saving is off.
    #[test]
    fn only_a_missing_or_broken_document_may_be_replaced() {
        let dir = std::env::temp_dir().join(format!("pf-webos-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("settings.json");
        assert_eq!(
            read_document(&doc),
            (Persisted::default(), true),
            "absent reads as defaults"
        );
        assert!(!dir.join("settings.json.bad").exists(), "nothing to set aside");

        std::fs::write(&doc, br#"{"known_hosts": 7}"#).unwrap();
        assert_eq!(read_document(&doc), (Persisted::default(), true));
        assert!(!doc.exists());
        assert_eq!(
            std::fs::read(dir.join("settings.json.bad")).unwrap(),
            br#"{"known_hosts": 7}"#
        );

        // A directory under the name: it exists, and no read succeeds.
        std::fs::create_dir_all(&doc).unwrap();
        assert_eq!(read_document(&doc), (Persisted::default(), false));
        assert!(doc.exists(), "left where it is");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
