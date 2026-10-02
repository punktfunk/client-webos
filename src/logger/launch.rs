//! Launch parameters SAM hands the app as argv[1].
use std::sync::OnceLock;

use serde::Deserialize;

use tracing::Level;

/// argv[1] shape from SAM; all fields optional (no error if missing).
#[derive(Deserialize, Default)]
struct LaunchParams {
    telemetry: Option<String>,
    telemetry_level: Option<String>,
    /// Forces `device::sdk_version`, so a modern TV can exercise the NDL v1 path
    /// (`task deploy WEBOS_SDK=...`).
    webos_sdk: Option<String>,
}

/// Cache launch params once; argv doesn't change over process lifetime.
fn launch_params() -> &'static LaunchParams {
    static PARAMS: OnceLock<LaunchParams> = OnceLock::new();
    PARAMS.get_or_init(|| {
        std::env::args()
            .nth(1)
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    })
}

/// Where to stream logs instead of writing them to disk; `None` means file.
pub(super) fn telemetry_addr() -> Option<&'static str> {
    launch_params().telemetry.as_deref().filter(|s| !s.is_empty())
}

/// Launch-time log level from the `TELEMETRY_LEVEL` env var; `None` keeps the default.
pub(super) fn launch_level() -> Option<Level> {
    match launch_params()
        .telemetry_level
        .as_deref()?
        .to_ascii_lowercase()
        .as_str()
    {
        "debug" => Some(Level::DEBUG),
        "info" => Some(Level::INFO),
        "warn" => Some(Level::WARN),
        "error" => Some(Level::ERROR),
        _ => None,
    }
}

/// Launch-time override for the detected webOS SDK version; `None` leaves detection untouched.
pub fn webos_sdk_override() -> Option<&'static str> {
    launch_params().webos_sdk.as_deref().filter(|s| !s.is_empty())
}
