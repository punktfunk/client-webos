//! Plain domain data. No I/O — persistence lives in `crate::services`.
use serde::{Deserialize, Serialize};

use pf_client_core::presets::StreamPreset;

use crate::core::caps::VideoCaps;

/// Stream connection target.
#[derive(Clone)]
pub struct ConnectTarget {
    pub host: String,
    pub port: u16,
    /// The pinned host fingerprint. Always present: the pin *is* the pair state, and
    /// every launch path bails on an unpaired host before building one of these.
    pub fingerprint: [u8; 32],
    /// Library entry id to launch, or `None` for desktop.
    pub launch: Option<String>,
    /// The delivery profile a network check left on the host's record (`1` capped, `2` smooth).
    pub delivery: Option<u8>,
}

/// One saved host: the record every punktfunk client stores (`trust::KnownHost`, flattened
/// into the same object — plan D8) plus what only this TV keeps: its power behaviour. Reads of the shared fields go through `Deref`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KnownHost {
    #[serde(flatten)]
    pub shared: pf_client_core::trust::KnownHost,
    /// Wake this host when it is picked and found asleep.
    pub wol_auto: bool,
    /// What to do to this host when the app exits (per-host, off by default; sits under
    /// `wol_auto` in Host power settings — the two are the same switch pointing opposite ways).
    pub exit_action: ExitAction,
}

impl std::ops::Deref for KnownHost {
    type Target = pf_client_core::trust::KnownHost;
    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

impl std::ops::DerefMut for KnownHost {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared
    }
}

/// The shared record derives no `PartialEq`; the store's writer compares documents, and the
/// serialized form is the one comparison that cannot miss a field. Bytes, not a `Value` tree: the
/// record's only map is a `BTreeMap`, so its encoding is deterministic.
impl PartialEq for KnownHost {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_vec(self).ok() == serde_json::to_vec(other).ok()
    }
}

/// What to do to a host when the app exits: nothing, or one of the host's power actions
/// (`services::power`, host-side from `punktfunk-core` 0.33.0), which need the pairing's Host
/// power grant. Defaults to [`Self::None`]: a client that quietly powered a machine down would be
/// worse than one that never offered to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitAction {
    #[default]
    None,
    Sleep,
    Shutdown,
}

impl ExitAction {
    /// The host action id to invoke, or `None` when there is nothing to do.
    pub fn action_id(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Sleep => Some("power.sleep"),
            Self::Shutdown => Some("power.shutdown"),
        }
    }
}

/// Pin ID for the "Desktop" card — a `game_presets` key like any game id.
pub const DESKTOP_PIN_ID: &str = "__desktop__";

impl KnownHost {
    /// Paired means a pinned certificate: the fingerprint IS the pair state.
    pub fn is_paired(&self) -> bool {
        !self.fp_hex.is_empty()
    }

    /// The pinned certificate fingerprint, decoded from the stored hex.
    pub fn fingerprint(&self) -> Option<[u8; 32]> {
        pf_client_core::trust::parse_hex32(&self.fp_hex)
    }

    /// Pins `fp` as the host's certificate: the record is paired from here on.
    pub fn set_fingerprint(&mut self, fp: [u8; 32]) {
        use std::fmt::Write;
        self.fp_hex = fp.iter().fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        });
        self.paired = true;
    }

    /// This game's bound profile id, if it has one (the shared `game_presets`).
    pub fn game_profile(&self, id: &str) -> Option<&str> {
        self.shared.preset_for_game(id)
    }
}

/// Upserts by `(addr, port)`, keeping the existing pin if the new record is unpaired (a fresh
/// mDNS discovery shouldn't clobber a paired host) — same reasoning for `mac` and `os`,
/// learned separately. The record's id, its profile bindings, its pins, `wol_auto` and the
/// exit action are *always* kept from the existing record: only their own screens change them, so no add/edit/re-pair flow may clobber any.
///
/// Returns the inserted record when it was genuinely new, so the caller can seed what a first
/// sighting gets ([`seed_new_host_profiles`]) without looking it up again. `None` for a merge:
/// re-adding or re-pairing a host must not re-seed anything.
pub fn upsert_known_host(hosts: &mut Vec<KnownHost>, mut new: KnownHost) -> Option<&mut KnownHost> {
    let Some(at) = hosts.iter().position(|h| h.addr == new.addr && h.port == new.port) else {
        hosts.push(new);
        return hosts.last_mut();
    };
    let existing = &mut hosts[at];
    if !new.is_paired() {
        new.fp_hex.clone_from(&existing.fp_hex);
        new.paired = existing.paired;
    }
    if new.mac.is_empty() {
        new.mac.clone_from(&existing.mac);
    }
    if new.os.is_empty() {
        new.os.clone_from(&existing.os);
    }
    new.id.clone_from(&existing.id);
    new.preset_id.clone_from(&existing.preset_id);
    new.pinned_presets.clone_from(&existing.pinned_presets);
    new.game_presets.clone_from(&existing.game_presets);
    new.wol_auto = existing.wol_auto;
    new.exit_action = existing.exit_action;
    *existing = new;
    None
}

fn unique_profile_name(catalog: &[StreamPreset], wanted: &str) -> String {
    let taken = |name: &str| catalog.iter().any(|p| p.name == name);
    if !taken(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|n| format!("{wanted} {n}"))
        .find(|name| !taken(name))
        .expect("an unbounded counter finds a free name")
}

const DESKTOP_PROFILE_NAME: &str = "Desktop";

/// Seeds a new host with a Desktop profile pinning absolute pointer mode (while the global
/// default stays capture). Unlike a resolver rule, this pin is visible in Settings and mutable,
/// so users see what's overriding the default and can adjust it.
///
/// Takes the record [`upsert_known_host`] reports as new, so existing installs keep their state.
/// Idempotent: a host with an existing Desktop binding is left alone.
pub fn seed_new_host_profiles(host: &mut KnownHost, profiles: &mut Vec<StreamPreset>) {
    if host.game_profile(DESKTOP_PIN_ID).is_some() {
        return;
    }
    let mut profile = StreamPreset::new(unique_profile_name(profiles, DESKTOP_PROFILE_NAME));
    profile.overrides.mouse_mode = Some(pf_client_core::trust::MouseMode::Desktop.as_name().to_string());
    let id = profile.id.clone();
    profiles.push(profile);
    host.bind_game_preset(DESKTOP_PIN_ID, Some(&id));
}

/// Codec preference selectable in Settings — a *preference*, not a demand. The host
/// resolves the session codec from the client's advertised set via its own precedence
/// ladder (HEVC > H.264), honouring the preference only when its encoder can
/// actually produce it — so "H264" on a host that can't encode H.264 still gets HEVC
/// rather than no session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodecPref {
    /// No preference — the host's precedence ladder decides (HEVC in practice).
    #[default]
    Auto,
    H264,
    Hevc,
}

/// Where a session's audio is decoded and played — the two routes this client can build,
/// selectable in Settings and swappable without touching the pipeline.
///
/// The routes trade the same way: everything below [`Self::Software`] is a shorter path onto the
/// panel's own clock, and gives up what this client can steer in exchange. Which is actually
/// smoother is hardware-specific (`docs/NOTES.md` § "NDL's audio plane"), hence a setting rather
/// than a fixed choice.
///
/// Not every route carries every layout, and nothing is ever folded down: what the selected route
/// can put on a speaker is what the handshake asks the host to encode (`AudioRoutePref::max_channels`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioRoutePref {
    /// Software Opus decode → the TV's SDL audio device, with NDL's silent clock plane keeping
    /// the picture paced. The longest path and the only one whose pacing is proven on hardware —
    /// so, the default. `PulseAudio`'s hardware sink is stereo, so surround folds there.
    #[default]
    Software,
    /// Opus decoded by the TV on its audio plane, the wire untouched: every host is asked for the
    /// one 5.1 coupling NDL decodes (`ndl::OPUS_51_LAYOUT`), and an older host's is re-encoded
    /// here. The only route that keeps 5.1 discrete. Some sets accept the load and then play
    /// nothing, which no runtime probe detects.
    NdlOpus,
}

impl AudioRoutePref {
    /// Whether the real stream rides NDL's audio plane — i.e. no SDL device is opened, and the
    /// clock plane is a standing filler rather than the only feed.
    pub fn on_ndl_plane(self) -> bool {
        self != Self::Software
    }

    /// How the stats overlay names this route. Which decoder ran leads the line: the paths fail
    /// differently, and reading the numbers without knowing which produced them has already cost
    /// real debugging time.
    pub fn overlay_tag(self) -> &'static str {
        match self {
            Self::Software => "Opus SW",
            Self::NdlOpus => "Opus HW",
        }
    }

    /// Widest layout this route can put on a speaker, and therefore the ceiling on what the
    /// handshake asks the host to encode. Per route, never global: NDL's plane and the SDL device
    /// have different ceilings, and clamping every session by the narrower one cost users 5.1 on
    /// a route that plays it.
    pub fn max_channels(self, caps: VideoCaps) -> u8 {
        match self {
            // SDL opens the negotiated layout; `PulseAudio` folds what its stereo sink can't carry.
            Self::Software => caps.max_channels,
            // NDL decodes stereo Opus and one 5.1 layout, nothing wider.
            Self::NdlOpus => caps.max_channels.min(6),
        }
    }

    /// The routes this device can actually build, in display order — software first, it being
    /// the default and the only one that needs no plane. The offload route needs NDL v2's audio
    /// type (`VideoCaps::audio_plane`); on webOS 4 and below there is no plane to ride and
    /// software is the whole list.
    pub fn available(caps: VideoCaps) -> &'static [Self] {
        if caps.audio_plane {
            &[Self::Software, Self::NdlOpus]
        } else {
            &[Self::Software]
        }
    }
}

/// Which controller the host should present to the game, selectable in Settings.
///
/// This is the *virtual* pad the host builds, not what the user is holding — the host
/// translates. It matters beyond glyphs: a game only emits adaptive-trigger effects when it
/// sees a `DualSense`, so [`GamepadType::DualSense`] is what makes `crate::platform::webos::dualsense` have
/// anything to replay. The host resolves the choice against what its platform can actually
/// build (the `PlayStation` and Switch backends need Linux UHID) and falls back on its own if
/// not, so an unbuildable pick degrades to a working session rather than none.
///
/// A deliberate subset of `punktfunk_core::config::GamepadPref`'s eleven variants: the Steam
/// Controller/Deck backends exist for clients running *on* that hardware, which a TV is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GamepadType {
    /// Mirror whichever controller is attached, falling back to the host's own choice (an
    /// Xbox pad) when the pad isn't one this client recognizes — see
    /// `gamepad::detect_type`. Resolved per session, so the stored preference stays "match my
    /// pad" rather than freezing to whatever was plugged in once.
    #[default]
    Auto,
    /// The host's Xbox pad. The alias keeps a settings file written when this client also
    /// offered a separate Xbox 360 pick loadable: the two differed only in the virtual pad's
    /// name.
    #[serde(alias = "xbox360")]
    XboxOne,
    DualShock4,
    /// Adaptive triggers, lightbar, touchpad, motion — see [`crate::platform::webos::dualsense`].
    DualSense,
    /// `DualSense` plus the two back buttons and two Fn buttons.
    DualSenseEdge,
    SwitchPro,
}

impl GamepadType {
    /// The wire preference sent in the handshake, which becomes the session-default pad kind —
    /// and the name the shared settings document stores, and what the console shell picks its
    /// button-glyph legend by.
    pub fn to_core(self) -> punktfunk_core::config::GamepadPref {
        use punktfunk_core::config::GamepadPref as P;
        match self {
            Self::Auto => P::Auto,
            Self::XboxOne => P::XboxOne,
            Self::DualShock4 => P::DualShock4,
            Self::DualSense => P::DualSense,
            Self::DualSenseEdge => P::DualSenseEdge,
            Self::SwitchPro => P::SwitchPro,
        }
    }

    /// The inverse of [`Self::to_core`]. `None` for a kind this client has no row for (a Steam
    /// Deck's pad, say), so the caller keeps its default rather than showing a control it
    /// cannot honour.
    pub fn from_core(pref: punktfunk_core::config::GamepadPref) -> Option<Self> {
        use punktfunk_core::config::GamepadPref as P;
        Some(match pref {
            P::Auto => Self::Auto,
            P::XboxOne => Self::XboxOne,
            P::DualShock4 => Self::DualShock4,
            P::DualSense => Self::DualSense,
            P::DualSenseEdge => Self::DualSenseEdge,
            P::SwitchPro => Self::SwitchPro,
            _ => return None,
        })
    }

    /// Whether a host pad of this kind can emit `DualSense` HID feedback (adaptive triggers,
    /// lightbar). The Edge is a `DualSense` plus extra buttons, so it carries the same effects.
    pub fn is_dualsense(self) -> bool {
        matches!(self, Self::DualSense | Self::DualSenseEdge)
    }
}

/// The one bitrate ceiling this client has. It bounds the manual bitrate AND, through `main`,
/// `punktfunk_core::abr`'s automatic wire-budget climb (`PUNKTFUNK_ABR_MAX_MBPS`) and the startup
/// link-capacity probe's burst target (`PUNKTFUNK_ABR_PROBE_KBPS`) — an Automatic session that
/// could climb past what the manual pick allows would just be a second, hidden setting.
pub const BITRATE_MAX_KBPS: u32 = 200_000;

/// 70% of measured goodput (headroom for FEC, loss).
/// Integer arithmetic order matters (`/ 10 * 7`, not `* 0.7`) — all clients must agree exactly.
pub fn recommended_bitrate_kbps(throughput_kbps: u32) -> u32 {
    throughput_kbps / 10 * 7
}

/// A slider's discrete positions: a closed range walked in fixed steps.
///
/// One value type for the three HDR measurement sliders, so the range, the
/// stop count, the value at a stop and the snap back onto the lattice all come from the same
/// numbers. They have to stay inverses of each other, and spelling each one out per slider is
/// how they stop being.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lattice {
    pub lo: u16,
    pub hi: u16,
    pub step: u16,
}

impl Lattice {
    /// How many positions the slider has.
    #[must_use]
    pub fn stops(self) -> usize {
        usize::from((self.hi - self.lo) / self.step) + 1
    }

    /// Which position `value` sits at.
    #[must_use]
    pub fn index(self, value: u16) -> usize {
        usize::from((value.clamp(self.lo, self.hi) - self.lo) / self.step)
    }

    /// The value at `stop`, which is clamped into range first.
    #[must_use]
    pub fn value(self, stop: i32) -> u16 {
        let stop = stop.clamp(0, self.stops() as i32 - 1) as u16;
        (self.lo + stop * self.step).min(self.hi)
    }

    /// Where `value` sits along the track, as 0..1.
    #[must_use]
    pub fn fraction(self, value: u16) -> f32 {
        let last = self.stops().saturating_sub(1);
        if last == 0 {
            0.0
        } else {
            self.index(value) as f32 / last as f32
        }
    }

    /// Clamps into range, then rounds to the nearest stop.
    #[must_use]
    pub fn snap(self, value: u16) -> u16 {
        let offset = value.clamp(self.lo, self.hi) - self.lo;
        self.value(i32::from((offset + self.step / 2) / self.step))
    }
}

/// Peak-brightness slider, in nits — and, while calibration is up, the mastering maximum the
/// pattern declares. The ceiling sits well above any panel the app runs on, or the slider ends
/// before the TV starts compressing: a CX measures ~790, a G3 with MLA ~1300, a 2025 G5 ~2400.
pub const HDR_PEAK: Lattice = Lattice {
    lo: 300,
    hi: 4_000,
    step: 10,
};
/// Full-field (frame-average) slider, in nits, and the pattern's declared `MaxFALL` while it is
/// being measured: climbed until a step no longer brightens a full field, where ABL holds it.
/// In use it ends at the measured peak ([`HdrDisplay::frame_avg_range`]).
pub const HDR_FRAME_AVG: Lattice = Lattice {
    lo: 100,
    hi: HDR_PEAK.hi,
    step: 10,
};
/// Black-floor slider, as 10-bit narrow-range PQ luma codes — 64 (zero light) up to 160
/// (about 0.4 nits, a poor edge-lit panel).
///
/// In codes rather than in nits because PQ is perceptually uniform and nits are not: the first
/// few codes above black span several decades of luminance, so a slider stepping through nits
/// would sit on one code for most of its travel while the picture never changed. One step here
/// is always one visible step.
pub const HDR_BLACK: Lattice = Lattice {
    lo: 64,
    hi: 160,
    step: 4,
};

/// The panel's HDR colour volume, as measured by HDR calibration (`runtime::calibration`).
///
/// These are the three luminances of the CTA-861.3 HDR static-metadata block. They travel to the
/// TV (so its tone map onto this panel becomes an identity) and to the host in
/// `Hello::display_hdr`, where punktfunk codes them into the virtual display's EDID — so the game
/// renders to this volume in the first place and nothing has to remap it later. That single
/// source-side tone map is what `HGiG` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HdrDisplay {
    /// Small-window peak (`MaxCLL`, and the mastering display's maximum).
    pub peak_nits: u16,
    /// Sustained full-field maximum (`MaxFALL`).
    pub frame_avg_nits: u16,
    /// Black floor, as a 10-bit narrow-range PQ luma code — see [`HDR_BLACK`].
    pub black_code: u16,
}

impl HdrDisplay {
    /// An LG CX's volume: what a TV advertises until HDR calibration has measured its own.
    pub const DEFAULT: Self = Self {
        peak_nits: 800,
        frame_avg_nits: 150,
        black_code: 68,
    };

    /// Every value on its slider's lattice, and the full field never above the small window.
    /// The one place these hold: the stored document, and every calibration step, go through it.
    #[must_use]
    pub fn normalized(self) -> Self {
        let peak = Self {
            peak_nits: HDR_PEAK.snap(self.peak_nits),
            ..self
        };
        Self {
            frame_avg_nits: peak.frame_avg_range().snap(self.frame_avg_nits),
            black_code: HDR_BLACK.snap(self.black_code),
            ..peak
        }
    }

    /// The full-field lattice for this peak: a full field never out-runs a small window.
    #[must_use]
    pub fn frame_avg_range(self) -> Lattice {
        Lattice {
            hi: self.peak_nits,
            ..HDR_FRAME_AVG
        }
    }

    /// The black floor in the 0.0001 cd/m² units the wire carries, never zero: ST.2086 reads a
    /// zero there as "unknown", and a self-emissive panel's real floor is better described by the
    /// smallest luminance the field can express than by no answer at all.
    fn min_luminance_units(self) -> u32 {
        ((crate::core::pq::pq_nits(self.black_code) * 10_000.0).round() as u32).max(1)
    }

    /// HDR10 mastering metadata describing this panel, for NDL and the host alike (see the type).
    ///
    /// The primaries are fixed (P3-D65); only the luminances are this panel's — an LG CX's
    /// until calibrated ([`Self::DEFAULT`]).
    #[must_use]
    pub fn hdr_meta(self) -> punktfunk_core::quic::HdrMeta {
        punktfunk_core::quic::HdrMeta {
            // G, B, R order (ST.2086), 1/50000 chromaticity units — P3-D65, the gamut LG panels
            // cover. BT.2020 would claim colours the panel cannot show, so nothing maps into it.
            display_primaries: [[13_250, 34_500], [7_500, 3_000], [34_000, 16_000]],
            white_point: [15_635, 16_450], // D65
            max_display_mastering_luminance: u32::from(self.peak_nits) * 10_000,
            min_display_mastering_luminance: self.min_luminance_units(),
            max_cll: self.peak_nits,
            max_fall: self.frame_avg_nits,
        }
    }
}

/// Everything this app persists, as one document — `settings.json`, written by
/// `services::store::StateWriter`.
///
/// One file rather than one per concern: separate files can disagree after a mid-write power
/// cut, which on a TV is the normal way to turn it off. Only the credentials
/// (`client-{cert,key}.pem`) stay outside, so a settings document that fails to parse can fall
/// back to defaults without silently discarding the identity every host has pinned.
///
/// `PartialEq` is load-bearing: `services::store::StateWriter` skips writing an unchanged
/// snapshot.
///
/// `Default` is spelled out rather than derived: a fresh install starts from this TV's document
/// ([`crate::core::settings::default_document`]), not the bare shared one, and `#[serde(default)]`
/// fills a missing field from it too — one source of defaults for both paths.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub settings: pf_client_core::trust::Settings,
    pub known_hosts: Vec<KnownHost>,
    /// The host the user last had active — so relaunching lands back on its library.
    /// `(host, port)`, not an index: `known_hosts` order isn't stable across a forget/re-add.
    pub selected_host: Option<(String, u16)>,
    /// The app version that last wrote this document (`core::VERSION`). `None` means it
    /// was written before versioning existed — the only signal a future migration gets about
    /// which shape it is reading. `store::load` stamps it on first sight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The settings-profile catalog: named bundles of sparse overrides, the model punktfunk's
    /// other clients keep in `client-profiles.json`. Here it rides the one document this client
    /// writes, for the reason everything else does — one file, one writer, no merge.
    ///
    /// A game's settings ARE a profile (the shared `game_presets`), which is what lets the
    /// shared shell list them and bind one to a title. A TV with no desktop app beside it can still
    /// fill this: opening a game's settings and changing a row creates the profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<StreamPreset>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            settings: crate::core::settings::default_document(),
            known_hosts: Vec::new(),
            selected_host: None,
            version: None,
            profiles: Vec::new(),
        }
    }
}

/// Cover-art paths for a title (host-relative, fetched via mTLS). Covers prefer
/// `portrait` then `header` then `hero`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Artwork {
    pub portrait: Option<String>,
    pub hero: Option<String>,
    pub header: Option<String>,
}

/// One title in the host's unified library. `id` is store-qualified (`steam:<appid>`,
/// `custom:<id>`) and doubles as the launch handle `session::connect`'s `launch`
/// parameter takes — the host resolves the actual launch spec itself from `id`.
#[derive(Clone, Debug, Deserialize)]
pub struct GameEntry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    /// Packaged brand mark token, such as `steam`, for art-less launcher cards.
    #[serde(default)]
    pub icon: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A full field above the peak comes down to it, and every value lands on its lattice.
    #[test]
    fn normalized_caps_full_field_at_peak() {
        let display = HdrDisplay {
            peak_nits: 805,
            frame_avg_nits: 2_000,
            black_code: 67,
        }
        .normalized();
        assert_eq!(
            (display.peak_nits, display.frame_avg_nits, display.black_code),
            (810, 810, 68)
        );
    }

    /// The saved record is the shared one plus this TV's fields, in one flat object (plan D8):
    /// what the shell reads is what is stored, and the pin round-trips through its hex.
    #[test]
    fn host_record_is_the_shared_record_flattened() {
        let mut h = KnownHost {
            shared: pf_client_core::trust::KnownHost {
                name: "desk".into(),
                addr: "10.0.0.2".into(),
                port: 47_989,
                ..Default::default()
            },
            wol_auto: true,
            ..Default::default()
        };
        h.set_fingerprint([0x5a; 32]);
        h.bind_game_preset("doom", Some("p1"));
        let json = serde_json::to_value(&h).unwrap();
        assert_eq!(json["addr"], "10.0.0.2");
        assert_eq!(json["fp_hex"], "5a".repeat(32));
        assert_eq!(json["wol_auto"], true);
        assert_eq!(json["game_presets"]["doom"], "p1");
        assert!(json.get("shared").is_none(), "flattened, not nested");
        let back: KnownHost = serde_json::from_value(json).unwrap();
        assert_eq!(back, h);
        assert!(back.is_paired());
        assert_eq!(back.fingerprint(), Some([0x5a; 32]));

        // Re-adding the host unpaired keeps the pin, the id and the bindings.
        let mut hosts = vec![h.clone()];
        upsert_known_host(
            &mut hosts,
            KnownHost {
                shared: pf_client_core::trust::KnownHost {
                    name: "desk".into(),
                    addr: "10.0.0.2".into(),
                    port: 47_989,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert_eq!(hosts.len(), 1);
        assert!(hosts[0].is_paired() && hosts[0].wol_auto);
        assert_eq!(hosts[0].id, h.id);
        assert_eq!(hosts[0].game_profile("doom"), Some("p1"));
    }
}
