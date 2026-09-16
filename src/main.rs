//! Native webOS TV client for punktfunk (see `docs/NOTES.md` for architecture).
//! Platform-gated to `target_os` = "linux" (both webOS and Linux dev boxes).
//
// `app`/`platform`/`session`/`runtime` are cfg-gated out on non-Linux hosts, so the `ui`/`core`/
// `services` items (and the glob re-exports feeding them) that they consume look "never used" on
// the macOS host build. Silence that there only — the Linux CI build still surfaces real dead code.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
#[cfg(target_os = "linux")]
mod app;
// The shared gamepad shell and the drawing kit both UIs use. Every Linux target: the host
// build renders the pointer UI's screens in its tests (see Cargo.toml).
#[cfg(target_os = "linux")]
mod console;
mod core;
mod logger;
#[cfg(target_os = "linux")]
mod platform;
mod services;
#[cfg(target_os = "linux")]
mod session;
mod ui;

use crate::core::model::BITRATE_MAX_KBPS;

#[cfg(target_os = "linux")]
mod runtime;

#[cfg(not(target_os = "linux"))]
mod runtime {
    pub fn run() -> anyhow::Result<()> {
        anyhow::bail!(
            "punktfunk-webos only runs under target_os = \"linux\" (a native Linux box, \
             or the armv7-unknown-linux-gnueabi webOS cross target) — see Cargo.toml"
        );
    }
}

/// [`BITRATE_MAX_KBPS`] in the Mbps unit `PUNKTFUNK_ABR_MAX_MBPS` is spelled in.
const ABR_MAX_MBPS: u32 = BITRATE_MAX_KBPS / 1_000;
/// Same proven-safe target as the client's connection test. Core keeps 70% of delivered probe
/// throughput, so probing at the 200 Mbps policy ceiling can never prove that ceiling.
const ABR_PROBE_KBPS: u32 = 320_000;

/// Publishes the two automatic-bitrate knobs `punktfunk_core` reads from the environment.
///
/// `PUNKTFUNK_ABR_MAX_MBPS` clamps core's climb ceiling however it is learned;
/// `PUNKTFUNK_ABR_PROBE_KBPS` uses the connection test's proven-safe 320 Mbps target. This clears
/// core's 70% safety margin while avoiding its mode-derived 1.8 Gbps 4K120 burst. See
/// `docs/NOTES.md` § "ABR startup probe".
fn set_abr_env() {
    std::env::set_var("PUNKTFUNK_ABR_PROBE_KBPS", ABR_PROBE_KBPS.to_string());
    std::env::set_var("PUNKTFUNK_ABR_MAX_MBPS", ABR_MAX_MBPS.to_string());
}

fn main() -> anyhow::Result<()> {
    // Load-bearing, not belt-and-braces: `ureq` is built without a backend feature
    // (`rustls-no-provider` — see Cargo.toml for why), and its own provider resolution ends in a
    // `panic!` when no process default has been installed. The two agents built by
    // `Agent::new_with_defaults()` — external cover art and the log upload — go through exactly
    // that path, so without this call the first HTTPS request on either aborts the app. The
    // mTLS library agent is unaffected either way; it names its provider via
    // `builder_with_provider`. Must land before any thread that might issue a request.
    punktfunk_core::tls::install_default_provider();
    // Set before anything spawns a thread: `set_var` is not thread-safe, and core reads these
    // while building its data-plane pump and bitrate controller during `connect`. An older core
    // simply ignores them.
    set_abr_env();
    runtime::run()
}
