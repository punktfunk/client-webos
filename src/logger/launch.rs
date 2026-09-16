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
    /// Replaces the panel-size UI scale factor, tunable on glass without rebuild
    /// (`ares-launch … -p ui_scale=1.2`). Launch param only - see `app::draw::panel_k`.
    ///
    /// Untyped on purpose: `ares-launch` sends strings, SAM sends numbers. A field that rejects
    /// either shape fails the whole struct - silently loses telemetry (seen on device 2026-09-06).
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

/// Launch-time UI scale factor override; invalid values are ignored.
pub fn ui_scale_override() -> Option<f32> {
    let raw = launch_params().ui_scale.as_ref()?;
    let n = raw.as_f64().or_else(|| raw.as_str()?.parse().ok())? as f32;
    (n.is_finite() && n > 0.0).then_some(n)
}
