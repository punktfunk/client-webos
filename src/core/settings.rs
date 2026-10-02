//! This client's typed view of the shared settings document (`pf_client_core::trust::
//! Settings`), which is the one settings type it stores and edits (plan WP4). Shared fields
//! read straight; the ones this TV alone has live in the document's `extra` map under a
//! `webos.` prefix.

use pf_client_core::trust::Settings;
use punktfunk_core::config::GamepadPref;

use crate::core::model::{AudioRoutePref, CodecPref, GamepadType, HdrDisplay};
use crate::core::model::{HDR_BLACK, HDR_FRAME_AVG, HDR_PEAK};

/// Prefix for rows only this client has. Namespaced so a future shared field of the same name
/// cannot collide with what a TV persisted.
const P: &str = "webos.";

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

/// The typed reads and writes this client makes on the shared document.
pub trait TvSettings {
    /// Capture is a switch here and a two-name mode in the shared schema.
    fn cursor_capture(&self) -> bool;
    fn set_cursor_capture(&mut self, capture: bool);
    fn codec_pref(&self) -> CodecPref;
    fn set_codec_pref(&mut self, codec: CodecPref);
    fn gamepad_type(&self) -> GamepadType;
    fn set_gamepad_type(&mut self, kind: GamepadType);
    /// Shared `pad_speaker` is a DESTINATION ("pad", "mix", "off"); this client offers
    /// pad-or-nothing.
    fn pad_speaker_on(&self) -> bool;
    fn hdr_peak_nits(&self) -> u16;
    fn hdr_frame_avg_nits(&self) -> u16;
    fn hdr_black_code(&self) -> u16;
    fn hdr_calibrated(&self) -> bool;
    fn audio_route(&self) -> AudioRoutePref;
    fn set_audio_route(&mut self, route: AudioRoutePref);
    /// The panel volume to advertise — see [`HdrDisplay`].
    fn hdr_display(&self) -> HdrDisplay;
    /// The one writer of the stored volume: the three measured fields move with the flag that
    /// says where they came from.
    fn set_hdr_display(&mut self, display: HdrDisplay, calibrated: bool);
    /// Normalise to what the active backend can present (`core::caps`), plus the one
    /// cross-field rule: HDR needs HEVC. Called on load (`store::load`) and on every launch's
    /// resolved settings (`store::shared::launch_settings`), so a profile override cannot carry
    /// a pick this TV cannot present onto the wire.
    fn clamp_to_caps(&mut self);
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
        // A kind this client has no row for (a Steam Deck's pad, say) reads as the default
        // rather than as a control it cannot honour.
        GamepadPref::from_name(&self.gamepad)
            .and_then(GamepadType::from_core)
            .unwrap_or_default()
    }

    fn set_gamepad_type(&mut self, kind: GamepadType) {
        self.gamepad = kind.to_core().as_str().to_string();
    }

    fn pad_speaker_on(&self) -> bool {
        self.pad_speaker == "pad"
    }

    fn hdr_peak_nits(&self) -> u16 {
        get(self, &key("hdr_peak_nits")).unwrap_or(HdrDisplay::DEFAULT.peak_nits)
    }

    fn hdr_frame_avg_nits(&self) -> u16 {
        get(self, &key("hdr_frame_avg_nits")).unwrap_or(HdrDisplay::DEFAULT.frame_avg_nits)
    }

    fn hdr_black_code(&self) -> u16 {
        get(self, &key("hdr_black_code")).unwrap_or(HdrDisplay::DEFAULT.black_code)
    }

    fn hdr_calibrated(&self) -> bool {
        get(self, &key("hdr_calibrated")).unwrap_or(false)
    }

    fn audio_route(&self) -> AudioRoutePref {
        get(self, &key("audio_route")).unwrap_or_default()
    }

    fn set_audio_route(&mut self, route: AudioRoutePref) {
        put(self, key("audio_route"), &route);
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
        // route's own limit narrows per session.
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
    let mut s = Settings::default();
    s.set_hdr_display(HdrDisplay::DEFAULT, false);
    s
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
        s.set_audio_route(AudioRoutePref::NdlOpus);
        s.set_codec_pref(CodecPref::Hevc);
        s.set_gamepad_type(GamepadType::DualSense);
        assert_eq!(s.audio_route(), AudioRoutePref::NdlOpus);
        assert_eq!(s.codec_pref(), CodecPref::Hevc);
        assert_eq!(s.gamepad_type(), GamepadType::DualSense);
        assert!(s.extra.contains_key("webos.audio_route"));
        assert_eq!(s.hdr_display(), HdrDisplay::DEFAULT);
        let json = serde_json::to_value(&s).unwrap();
        let back: Settings = serde_json::from_value(json).unwrap();
        assert_eq!(back.audio_route(), AudioRoutePref::NdlOpus);
    }
}
