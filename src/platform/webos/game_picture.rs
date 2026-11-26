//! Switches the TV's picture mode to "game" for the length of a stream and puts the user's mode
//! back afterwards. The previous mode is parked in `picture-mode.prev` in the app dir while game
//! mode is on, so a crash or kill is repaired by [`recover`] on the next launch.
//! The `webos.game_picture` setting turns the whole thing off.
use std::ffi::CString;
use std::path::PathBuf;
use std::time::Duration;

use super::ls2::Bus;
use crate::services::paths::app_dir;

const GET: &std::ffi::CStr = c"luna://com.webos.settingsservice/getSystemSettings";
const SET: &std::ffi::CStr = c"luna://com.webos.settingsservice/setSystemSettings";
const GAME: &str = "game";
const WAIT: Duration = Duration::from_millis(1500);

fn saved() -> PathBuf {
    app_dir().join("picture-mode.prev")
}

fn current(bus: &Bus) -> Option<String> {
    let reply = bus
        .call_text(GET, cr#"{"category":"picture","keys":["pictureMode"]}"#, WAIT)
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&reply).ok()?;
    v["settings"]["pictureMode"].as_str().map(str::to_owned)
}

fn set(bus: &Bus, mode: &str) -> bool {
    let payload = serde_json::json!({
        "category": "picture",
        "settings": {"pictureMode": mode},
    });
    let ok = CString::new(payload.to_string())
        .ok()
        .and_then(|p| bus.call_text(SET, &p, WAIT).ok())
        .is_some_and(|r| r.contains("\"returnValue\":true"));
    if !ok {
        tracing::warn!("game picture: could not set pictureMode {mode}");
    }
    ok
}

/// Puts back the parked mode, unless the user already moved off "game" themselves.
fn restore(bus: &Bus) {
    let Ok(prev) = std::fs::read_to_string(saved()) else {
        return;
    };
    let prev = prev.trim();
    if !prev.is_empty() && current(bus).as_deref() == Some(GAME) && !set(bus, prev) {
        return;
    }
    tracing::info!("game picture: restored {prev}");
    let _ = std::fs::remove_file(saved());
}

/// App start: repairs a previous run that died in game mode.
pub fn recover() {
    if saved().exists() {
        if let Ok(bus) = Bus::open() {
            restore(&bus);
        }
    }
}

/// Held for one stream; dropping it restores the picture mode.
pub struct Guard;

pub fn enter(enabled: bool) -> Option<Guard> {
    if !enabled {
        return None;
    }
    let bus = Bus::open().ok()?;
    let prev = current(&bus)?;
    if prev == GAME {
        return None;
    }
    std::fs::write(saved(), &prev).ok()?;
    if !set(&bus, GAME) {
        let _ = std::fs::remove_file(saved());
        return None;
    }
    tracing::info!("game picture: {prev} -> game");
    Some(Guard)
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(bus) = Bus::open() {
            restore(&bus);
        }
    }
}
