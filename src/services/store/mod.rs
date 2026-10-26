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

use anyhow::{Context, Result};

use crate::core::VERSION;

pub use crate::core::model::{
    seed_new_host_profiles, upsert_known_host, AudioRoutePref, CodecPref, ExitAction, GamepadType, Persisted,
};
pub use crate::core::settings::TvSettings;
pub use crate::services::paths::app_dir;
pub use identity::load_or_create_identity;
pub use pf_client_core::trust::Settings;
pub use writer::StateWriter;

fn path() -> PathBuf {
    app_dir().join("settings.json")
}

/// Loads the whole persisted document. Absent, unreadable and unparseable all answer with
/// defaults — a torn file must not take the app down (`services::atomic` is what prevents one).
pub fn load() -> Persisted {
    let mut state = read_document(&path());
    stamp_version(&mut state);
    // A document written on a more capable TV can hold HEVC, HDR and 7.1 on a device with none
    // of them — leaving a *set* value whose row the UI hides.
    state.settings.clamp_to_caps();
    state
}

/// The document at `path`, or defaults. No migration: a document this build cannot read is
/// renamed to `settings.json.bad` first, because the version stamp right after would write the
/// defaults over the only copy of every paired host.
fn read_document(path: &Path) -> Persisted {
    let why = match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Persisted>(&bytes) {
            Ok(state) => return state,
            Err(e) => e.to_string(),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Persisted::default(),
        Err(e) => e.to_string(),
    };
    let mut aside = path.as_os_str().to_os_string();
    aside.push(".bad");
    match std::fs::rename(path, &aside) {
        Ok(()) => tracing::warn!(%why, "settings.json unreadable — kept as settings.json.bad, starting from defaults"),
        Err(e) => tracing::warn!(%why, "settings.json unreadable and not set aside: {e}"),
    }
    Persisted::default()
}

/// Brings [`Persisted::version`] up to this build's.
///
/// Written synchronously so it happens once — `StateWriter`'s baseline is taken after this,
/// and an unstamped document that never gets saved would stamp again on every launch.
fn stamp_version(state: &mut Persisted) {
    if state.version.as_deref() == Some(VERSION) {
        return;
    }
    state.version = Some(VERSION.to_string());
    match save(state) {
        Ok(()) => tracing::info!("stamped settings.json with version {VERSION}"),
        Err(e) => tracing::warn!("could not stamp document version: {e:#}"),
    }
}

/// Writes the whole document: its `settings` object is the shared schema verbatim (see [`shared`]).
pub fn save(state: &Persisted) -> Result<()> {
    let json = serde_json::to_string_pretty(state).context("serialize app state")?;
    crate::services::atomic::write(&path(), &json, "settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A document this build cannot parse is kept beside the defaults, never overwritten by them.
    #[test]
    fn an_unreadable_document_is_set_aside() {
        let dir = std::env::temp_dir().join(format!("pf-webos-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let doc = dir.join("settings.json");
        assert_eq!(read_document(&doc), Persisted::default(), "absent reads as defaults");
        assert!(!dir.join("settings.json.bad").exists(), "nothing to set aside");

        std::fs::write(&doc, br#"{"known_hosts": 7}"#).unwrap();
        assert_eq!(read_document(&doc), Persisted::default());
        assert!(!doc.exists());
        assert_eq!(
            std::fs::read(dir.join("settings.json.bad")).unwrap(),
            br#"{"known_hosts": 7}"#
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
