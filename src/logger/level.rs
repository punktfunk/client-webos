//! The one live level shared by both layers: a `reload` handle drives the `fmt`
//! layer's filter, an atomic ordinal mirrors it for the ring layer (`reload::Layer`
//! isn't `Clone`, so it can't be attached to both).
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

use tracing::Level;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::{reload, Registry};

use crate::services::store::LogLevelOverride;

/// Live reload handle for dynamic level changes (Diagnostics screen).
static HANDLE: OnceLock<reload::Handle<LevelFilter, Registry>> = OnceLock::new();

/// Current level as an ordinal, for the ring layer's cheap per-event compare.
/// Initially INFO.
static ORDINAL: AtomicU8 = AtomicU8::new(3);

/// Startup filter level, mapped from persisted/launch-override settings.
pub fn resolved_level() -> Level {
    to_level(crate::services::store::persisted_log_level())
}

/// Applies immediately from Diagnostics screen; no-op before `init_subscriber`.
pub fn set_level_override(level: LogLevelOverride) {
    apply(to_level(level));
}

/// The level in force, as the settings enum: what Diagnostics shows. A launch override shows
/// here without ever reaching the document.
pub fn current_level_override() -> LogLevelOverride {
    match current_ordinal() {
        1 => LogLevelOverride::Error,
        2 => LogLevelOverride::Warn,
        3 => LogLevelOverride::Info,
        _ => LogLevelOverride::Debug,
    }
}

pub(super) fn install_handle(handle: reload::Handle<LevelFilter, Registry>, level: Level) {
    let _ = HANDLE.set(handle);
    ORDINAL.store(ordinal(level), Ordering::Relaxed);
}

/// Ordinal of the level currently in force.
pub(super) fn current_ordinal() -> u8 {
    ORDINAL.load(Ordering::Relaxed)
}

fn apply(level: Level) {
    ORDINAL.store(ordinal(level), Ordering::Relaxed);
    if let Some(handle) = HANDLE.get() {
        let _ = handle.modify(|filter| *filter = LevelFilter::from_level(level));
    }
}

fn to_level(o: LogLevelOverride) -> Level {
    match o {
        LogLevelOverride::Debug => Level::DEBUG,
        LogLevelOverride::Info => Level::INFO,
        LogLevelOverride::Warn => Level::WARN,
        LogLevelOverride::Error => Level::ERROR,
    }
}

/// Ascending by verbosity, so a `<=` compare answers "does this event pass?".
pub(super) fn ordinal(level: Level) -> u8 {
    match level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        Level::TRACE => 5,
    }
}

/// Inverse of `ordinal`.
pub(super) fn ordinal_to_filter(ordinal: u8) -> LevelFilter {
    match ordinal {
        1 => LevelFilter::ERROR,
        2 => LevelFilter::WARN,
        3 => LevelFilter::INFO,
        4 => LevelFilter::DEBUG,
        _ => LevelFilter::TRACE,
    }
}
