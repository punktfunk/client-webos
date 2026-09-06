pub mod caps;
pub mod errors;
pub mod event;
pub mod media;
pub mod model;
pub mod perf;
pub mod pq;
pub mod screen;
pub mod settings;

/// The packaged version (e.g. `0.4.0+git.abc12345`), threaded in at compile time via
/// `PKG_VERSION` by the Taskfile's build step. `Cargo.toml` deliberately stays a fixed
/// `0.0.1` (see CLAUDE.md), so a bare native `cargo build` falls back to
/// `CARGO_PKG_VERSION` rather than showing an empty marker.
///
/// One definition, because it names two user-visible things: the About screen's subtitle and
/// the log file (see `logger::sink::log_file_path`, which also has to match older names when
/// it prunes).
pub const VERSION: &str = match option_env!("PKG_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
