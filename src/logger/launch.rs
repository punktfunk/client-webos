//! Launch parameters SAM hands the app as argv[1].
use std::sync::OnceLock;

use serde::Deserialize;

use crate::services::store::LogLevelOverride;

/// argv[1] shape from SAM; all fields optional (no error if missing).
#[derive(Deserialize, Default)]
struct LaunchParams {
    telemetry: Option<String>,
    telemetry_level: Option<String>,
    /// Forces `device::sdk_version`, so a modern TV can exercise the NDL v1 path
    /// (`task deploy WEBOS_SDK=...`).
    webos_sdk: Option<String>,
    /// Replaces the panel-size UI scale factor, so a set neither developer owns can be tuned
    /// on glass without a rebuild (`ares-launch … -p ui_scale=1.2`). A launch param, never a
    /// settings row — see `app::draw::panel_k`.
    ///
    /// Untyped on purpose: `ares-launch` sends `-p` values as strings and SAM can send a
    /// number, and a field that rejects either shape fails the whole struct — which silently
    /// costs every other param, telemetry included (seen on device 2026-09-06).
    ui_scale: Option<serde_json::Value>,
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

/// Launch-time log level override from `TELEMETRY_LEVEL` env var.
/// Folded into settings so Diagnostics can display it. `None` leaves persisted level.
pub fn launch_level_override() -> Option<LogLevelOverride> {
    match launch_params()
        .telemetry_level
        .as_deref()?
        .to_ascii_lowercase()
        .as_str()
    {
        "debug" => Some(LogLevelOverride::Debug),
        "info" => Some(LogLevelOverride::Info),
        "warn" => Some(LogLevelOverride::Warn),
        "error" => Some(LogLevelOverride::Error),
        _ => None,
    }
}

/// Launch-time override for the detected webOS SDK version; `None` leaves detection untouched.
pub fn webos_sdk_override() -> Option<&'static str> {
    launch_params().webos_sdk.as_deref().filter(|s| !s.is_empty())
}

/// Launch-time replacement for the panel-derived UI scale factor; `None` leaves it alone.
/// Nonsense (zero, negative, unparseable) is ignored rather than shrinking the UI to nothing.
pub fn ui_scale_override() -> Option<f32> {
    let raw = launch_params().ui_scale.as_ref()?;
    let n = raw.as_f64().or_else(|| raw.as_str()?.parse().ok())? as f32;
    (n.is_finite() && n > 0.0).then_some(n)
}
