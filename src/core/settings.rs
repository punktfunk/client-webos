//! This client's typed view of the shared settings document (`pf_client_core::trust::
//! Settings`), which is the one settings type it stores and edits (plan WP4). Shared fields
//! read straight; the ones this TV alone has live in the document's `extra` map under a
//! `webos.` prefix, and the two names the console shell shares stay unprefixed.

use pf_client_core::trust::Settings;
use punktfunk_core::config::GamepadPref;

use crate::core::model::{AudioRoutePref, CodecPref, GamepadType, GamepadUiMode, HdrDisplay, LogLevelOverride};
use crate::core::model::{HDR_BLACK, HDR_FRAME_AVG, HDR_PEAK};

/// Prefix for rows only this client has. Namespaced so a future shared field of the same name
/// cannot collide with what a TV persisted.
const P: &str = "webos.";
/// The console-vs-cursor pair, spelled as `pf_console_ui`'s own settings rows spell it.
const GAMEPAD_UI_KEY: &str = "gamepad_ui_enabled";
const GAMEPAD_UI_MODE_KEY: &str = "gamepad_ui_mode";

fn key(name: &str) -> String {
    format!("{P}{name}")
}

fn get<T: serde::de::DeserializeOwned>(t: &Settings, key: &str) -> Option<T> {
    t.extra.get(key).cloned().and_then(|v| serde_json::from_value(v).ok())
}

fn put<T: serde::Serialize>(t: &mut Settings, key: String, value: &T) {
    if let Ok(v) = serde_json::to_value(value) {
        t.extra.insert(key, v);
    }
}

/// This client's codec pick as punktfunk's wire name, and back.
fn shared_codec(c: CodecPref) -> &'static str {
    match c {
        CodecPref::Auto => "auto",
        CodecPref::H264 => "h264",
        CodecPref::Hevc => "hevc",
    }
}

fn local_codec(name: &str) -> CodecPref {
    match name {
        "h264" => CodecPref::H264,
        "hevc" => CodecPref::Hevc,
        // "av1" too: this client cannot decode it, so it reads as the host's choice.
        _ => CodecPref::Auto,
    }
}

/// This client's pad choice as punktfunk's. Also what the console shell's button-glyph legend
/// is picked by, which is why it is a value rather than only ever a string.
pub fn gamepad_pref(t: GamepadType) -> GamepadPref {
    match t {
        GamepadType::Auto => GamepadPref::Auto,
        GamepadType::XboxOne => GamepadPref::XboxOne,
        GamepadType::DualShock4 => GamepadPref::DualShock4,
        GamepadType::DualSense => GamepadPref::DualSense,
        GamepadType::DualSenseEdge => GamepadPref::DualSenseEdge,
        GamepadType::SwitchPro => GamepadPref::SwitchPro,
    }
}

/// The inverse. `None` for a kind this client has no row for (a Steam Deck's pad, say), so the
/// caller keeps its default rather than showing a control it cannot honour.
fn local_gamepad(name: &str) -> Option<GamepadType> {
    Some(match GamepadPref::from_name(name)? {
        GamepadPref::Auto => GamepadType::Auto,
        GamepadPref::XboxOne => GamepadType::XboxOne,
        GamepadPref::DualShock4 => GamepadType::DualShock4,
        GamepadPref::DualSense => GamepadType::DualSense,
        GamepadPref::DualSenseEdge => GamepadType::DualSenseEdge,
        GamepadPref::SwitchPro => GamepadType::SwitchPro,
        _ => return None,
    })
}

/// The typed reads and writes this client makes on the shared document.
pub trait TvSettings {
    /// Capture is a switch here and a two-name mode in the shared schema.
    fn cursor_capture(&self) -> bool;
    fn set_cursor_capture(&mut self, capture: bool);
    fn codec_pref(&self) -> CodecPref;
    fn set_codec_pref(&mut self, codec: CodecPref);
    fn gamepad_type(&self) -> GamepadType;
    fn set_gamepad_type(&mut self, kind: GamepadType);
    fn stats_overlay(&self) -> bool;
    /// Shared `pad_speaker` is a DESTINATION ("pad", "mix", "off"); this client offers
    /// pad-or-nothing.
    fn pad_speaker_on(&self) -> bool;
    fn hdr_peak_nits(&self) -> u16;
    fn hdr_frame_avg_nits(&self) -> u16;
    fn hdr_black_code(&self) -> u16;
    fn hdr_calibrated(&self) -> bool;
    fn log_level_override(&self) -> LogLevelOverride;
    fn set_log_level_override(&mut self, level: LogLevelOverride);
    fn show_logs(&self) -> bool;
    fn set_show_logs(&mut self, on: bool);
    fn game_mode(&self) -> bool;
    fn set_game_mode(&mut self, on: bool);
    fn audio_route(&self) -> AudioRoutePref;
    fn set_audio_route(&mut self, route: AudioRoutePref);
    fn cursor_gestures(&self) -> bool;
    fn gamepad_ui(&self) -> bool;
    fn set_gamepad_ui(&mut self, on: bool);
    fn gamepad_ui_mode(&self) -> GamepadUiMode;
    /// Whether the shared shell should be fronting the app right now. Android's rule minus its
    /// `tv` term: every webOS set is a TV, and the cursor UI is the one a remote wants.
    fn gamepad_ui_active(&self, pad_connected: bool) -> bool;
    /// The panel volume to advertise — see [`HdrDisplay`].
    fn hdr_display(&self) -> HdrDisplay;
    /// The one writer of the stored volume: the three measured fields move with the flag that
    /// says where they came from.
    fn set_hdr_display(&mut self, display: HdrDisplay, calibrated: bool);
    /// Normalise to what the active backend can present (`core::caps`), plus the one
    /// cross-field rule: HDR needs HEVC. Called on load and on every write.
    fn clamp_to_caps(&mut self);
}

fn tv_default() -> Settings {
    let mut s = Settings::default();
    s.set_hdr_display(
        HdrDisplay {
            peak_nits: 800,
            frame_avg_nits: 150,
            black_code: 68,
        },
        false,
    );
    s
}

impl TvSettings for Settings {
    fn cursor_capture(&self) -> bool {
        self.mouse_mode != "desktop"
    }

    fn set_cursor_capture(&mut self, capture: bool) {
        self.mouse_mode = if capture { "capture" } else { "desktop" }.to_string();
    }

    fn codec_pref(&self) -> CodecPref {
        local_codec(&self.codec)
    }

    fn set_codec_pref(&mut self, codec: CodecPref) {
        self.codec = shared_codec(codec).to_string();
    }

    fn gamepad_type(&self) -> GamepadType {
        local_gamepad(&self.gamepad).unwrap_or_default()
    }

    fn set_gamepad_type(&mut self, kind: GamepadType) {
        self.gamepad = gamepad_pref(kind).as_str().to_string();
    }

    fn stats_overlay(&self) -> bool {
        self.show_stats
    }

    fn pad_speaker_on(&self) -> bool {
        self.pad_speaker == "pad"
    }

    fn hdr_peak_nits(&self) -> u16 {
        get(self, &key("hdr_peak_nits")).unwrap_or(800)
    }

    fn hdr_frame_avg_nits(&self) -> u16 {
        get(self, &key("hdr_frame_avg_nits")).unwrap_or(150)
    }

    fn hdr_black_code(&self) -> u16 {
        get(self, &key("hdr_black_code")).unwrap_or(68)
    }

    fn hdr_calibrated(&self) -> bool {
        get(self, &key("hdr_calibrated")).unwrap_or(false)
    }

    fn log_level_override(&self) -> LogLevelOverride {
        get(self, &key("log_level_override")).unwrap_or(LogLevelOverride::Info)
    }

    fn set_log_level_override(&mut self, level: LogLevelOverride) {
        put(self, key("log_level_override"), &level);
    }

    fn show_logs(&self) -> bool {
        get(self, &key("show_logs")).unwrap_or(false)
    }

    fn set_show_logs(&mut self, on: bool) {
        put(self, key("show_logs"), &on);
    }

    fn game_mode(&self) -> bool {
        get(self, &key("game_mode")).unwrap_or(false)
    }

    fn set_game_mode(&mut self, on: bool) {
        put(self, key("game_mode"), &on);
    }

    fn audio_route(&self) -> AudioRoutePref {
        get(self, &key("audio_route")).unwrap_or_default()
    }

    fn set_audio_route(&mut self, route: AudioRoutePref) {
        put(self, key("audio_route"), &route);
    }

    fn cursor_gestures(&self) -> bool {
        get(self, &key("cursor_gestures")).unwrap_or(false)
    }

    fn gamepad_ui(&self) -> bool {
        // On, taking over only while a pad is attached — the cross-client default.
        get(self, GAMEPAD_UI_KEY).unwrap_or(true)
    }

    fn set_gamepad_ui(&mut self, on: bool) {
        put(self, GAMEPAD_UI_KEY.to_string(), &on);
    }

    fn gamepad_ui_mode(&self) -> GamepadUiMode {
        get(self, GAMEPAD_UI_MODE_KEY).unwrap_or_default()
    }

    fn gamepad_ui_active(&self, pad_connected: bool) -> bool {
        self.gamepad_ui() && (self.gamepad_ui_mode() == GamepadUiMode::Always || pad_connected)
    }

    fn hdr_display(&self) -> HdrDisplay {
        HdrDisplay {
            peak_nits: self.hdr_peak_nits(),
            frame_avg_nits: self.hdr_frame_avg_nits(),
            black_code: self.hdr_black_code(),
        }
    }

    fn set_hdr_display(&mut self, display: HdrDisplay, calibrated: bool) {
        put(self, key("hdr_peak_nits"), &display.peak_nits);
        put(self, key("hdr_frame_avg_nits"), &display.frame_avg_nits);
        put(self, key("hdr_black_code"), &display.black_code);
        put(self, key("hdr_calibrated"), &calibrated);
    }

    fn clamp_to_caps(&mut self) {
        let caps = crate::core::caps::video_caps();
        let codecs = caps.codec_prefs();
        let codec = self.codec_pref();
        if !codecs.contains(&codec) {
            tracing::info!(
                "settings: {codec:?} isn't offerable on this video backend — using {:?}",
                codecs[0]
            );
            self.set_codec_pref(codecs[0]);
        }
        if self.hdr_enabled && !caps.hdr {
            tracing::info!("settings: HDR isn't presentable on this video backend — turning it off");
            self.hdr_enabled = false;
        }
        if self.hdr_enabled && self.codec_pref() == CodecPref::H264 {
            // Mirrors `session::connect`'s own gate: a session pinned to H.264 never resolves HDR.
            tracing::info!("settings: HDR needs HEVC — an explicit H.264 pick turns it off");
            self.hdr_enabled = false;
        }
        let route = self.audio_route();
        if !AudioRoutePref::available(caps).contains(&route) {
            tracing::info!(
                "settings: {route:?} audio needs NDL's audio plane, which this backend has none of — using Software"
            );
            self.set_audio_route(AudioRoutePref::Software);
        }
        // The decoder-wide ceiling, and nothing else: `audio_channels` is a preference the
        // route's own limit and the TV's Sound Out narrow per session.
        if self.audio_channels > caps.max_channels {
            tracing::info!(
                "settings: {} audio channels is more than this client can decode ({}) — clamping",
                self.audio_channels,
                caps.max_channels,
            );
            self.audio_channels = caps.max_channels;
        }
        // Snapped rather than merely clamped: the sliders move on a lattice, and a value off it
        // would leave a thumb between two stops. A full field never out-runs a small window.
        let peak = HDR_PEAK.snap(u32::from(self.hdr_peak_nits())) as u16;
        let frame_avg = (HDR_FRAME_AVG.snap(u32::from(self.hdr_frame_avg_nits())) as u16).min(peak);
        let black = HDR_BLACK.snap(u32::from(self.hdr_black_code())) as u16;
        let calibrated = self.hdr_calibrated();
        self.set_hdr_display(
            HdrDisplay {
                peak_nits: peak,
                frame_avg_nits: frame_avg,
                black_code: black,
            },
            calibrated,
        );
    }
}

/// The document a fresh install starts from: the shared defaults with this TV's own rows.
pub fn default_document() -> Settings {
    tv_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tv_rows_round_trip_under_their_prefix() {
        let mut s = default_document();
        assert!(s.cursor_capture());
        s.set_cursor_capture(false);
        assert_eq!(s.mouse_mode, "desktop");
        s.set_game_mode(true);
        s.set_log_level_override(LogLevelOverride::Debug);
        s.set_codec_pref(CodecPref::Hevc);
        s.set_gamepad_type(GamepadType::DualSense);
        assert!(s.game_mode());
        assert_eq!(s.log_level_override(), LogLevelOverride::Debug);
        assert_eq!(s.codec_pref(), CodecPref::Hevc);
        assert_eq!(s.gamepad_type(), GamepadType::DualSense);
        assert!(s.extra.contains_key("webos.game_mode"));
        assert_eq!(s.hdr_display().peak_nits, 800);
        let json = serde_json::to_value(&s).unwrap();
        let back: Settings = serde_json::from_value(json).unwrap();
        assert!(back.game_mode() && back.gamepad_ui());
    }
}
