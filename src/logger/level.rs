//! The one level shared by both layers: fixed at startup, mirrored as an atomic ordinal for
//! the ring layer's cheap per-event compare.
use std::sync::atomic::{AtomicU8, Ordering};

use tracing::Level;
use tracing_subscriber::filter::LevelFilter;

/// Current level as an ordinal. Initially INFO.
static ORDINAL: AtomicU8 = AtomicU8::new(3);

/// Startup filter level: the `TELEMETRY_LEVEL` launch override (`task deploy TELEMETRY=...`)
/// when set, else INFO.
pub fn resolved_level() -> Level {
    super::launch::launch_level().unwrap_or(Level::INFO)
}

pub(super) fn install(level: Level) {
    ORDINAL.store(ordinal(level), Ordering::Relaxed);
}

/// Ordinal of the level currently in force.
pub(super) fn current_ordinal() -> u8 {
    ORDINAL.load(Ordering::Relaxed)
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
