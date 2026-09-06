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

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::Value;

pub use crate::core::model::{
    upsert_known_host, AudioRoutePref, CodecPref, ExitAction, GamepadType, KnownHost, LogLevelOverride, Persisted,
    DESKTOP_PIN_ID,
};
pub use crate::core::settings::TvSettings;
pub use crate::services::paths::app_dir;
pub use identity::load_or_create_identity;
pub use pf_client_core::trust::Settings;
pub use writer::StateWriter;

fn path() -> PathBuf {
    app_dir().join("settings.json")
}

fn read_document() -> Option<Value> {
    let text = std::fs::read_to_string(path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// Loads the whole persisted document. Absent, unreadable and unparseable all answer with
/// defaults — a torn file must not take the app down (`services::atomic` is what prevents one).
///
/// Migrates the pre-consolidation layout in place, so callers never see the old shape.
/// [`load`]'s result: the document, plus whether this build is the first to write its version
/// into it — the one-shot signal the UI uses to introduce what the release added.
pub struct Loaded {
    pub state: Persisted,
    pub new_build: bool,
}

pub fn load() -> Loaded {
    // A nested `settings` key means the current shape. Otherwise the fields sit at the top level
    // — or the file is missing entirely, which still needs migrating, since pairing a host wrote
    // `known-hosts.json` without ever writing settings.
    let mut state = read_document().map_or_else(Persisted::default, from_document);
    let new_build = stamp_version(&mut state);
    // A document written on a more capable TV can hold HEVC, HDR and 7.1 on a device with none
    // of them — leaving a *set* value whose row the UI hides.
    state.settings.clamp_to_caps();
    Loaded { state, new_build }
}

/// The version this build writes into the document.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Brings [`Persisted::version`] up to this build's, and returns whether it had to — which is
/// how the UI knows this release is running here for the first time.
///
/// Written synchronously so it happens once — `StateWriter`'s baseline is taken after this,
/// and an unstamped document that never gets saved would stamp again on every launch.
fn stamp_version(state: &mut Persisted) -> bool {
    if state.version.as_deref() == Some(VERSION) {
        return false;
    }
    state.version = Some(VERSION.to_string());
    if let Err(e) = save(state) {
        tracing::warn!("could not stamp document version: {e:#}");
        return true;
    }
    tracing::info!("stamped settings.json with version {VERSION}");
    true
}

/// The document, with its `settings` object read out of the shared schema into this client's
/// own shape (see [`shared`]). A pre-shared object is converted here once; the next [`save`]
/// writes it back in the shared schema.
fn from_document(doc: Value) -> Persisted {
    serde_json::from_value(doc).unwrap_or_default()
}

pub fn save(state: &Persisted) -> Result<()> {
    let doc = serde_json::to_value(state).context("serialize app state")?;
    let json = serde_json::to_string_pretty(&doc).context("render app state")?;
    crate::services::atomic::write(&path(), &json, "settings.json")
}

/// The level `logger` starts at: the `TELEMETRY_LEVEL` launch override (`task deploy
/// TELEMETRY=...`) when set, else the persisted one. The override lives in the logger only —
/// Diagnostics reads `logger::current_level_override` — so it never reaches the document. Not
/// [`load`]: this runs before the subscriber exists. Reads either document shape.
pub fn persisted_log_level() -> LogLevelOverride {
    if let Some(level) = crate::logger::launch_level_override() {
        return level;
    }
    let Some(doc) = read_document() else {
        return LogLevelOverride::default();
    };
    let settings = doc.get("settings").unwrap_or(&doc);
    settings
        .get("webos.log_level_override")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}
