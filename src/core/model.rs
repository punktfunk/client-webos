//! Plain domain data. No I/O — persistence lives in `crate::services`.
use serde::{Deserialize, Serialize};

use pf_client_core::profiles::StreamProfile;

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
    /// A profile for this launch alone ("Connect with", a pinned card), over every binding.
    pub profile: Option<String>,
}

/// One saved host: the record every punktfunk client stores (`trust::KnownHost`, flattened
/// into the same object — plan D8) plus what only this TV keeps: its power behaviour and the
/// grid's collections. Reads of the shared fields go through `Deref`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct KnownHost {
    #[serde(flatten)]
    pub shared: pf_client_core::trust::KnownHost,
    /// Wake this host when it is picked and found asleep.
    pub wol_auto: bool,
    /// What to do to this host when the app exits (per-host, off by default; sits under
    /// `wol_auto` in Host power settings — the two are the same switch pointing opposite ways).
    pub exit_action: ExitAction,
    /// Grid section order, Library included as the [`Collection::dynamic`] entry. One vector
    /// carries everything: order, names and membership. Change it through the methods below,
    /// which keep a game in at most one collection and Library unremovable.
    #[serde(default = "new_host_collections")]
    pub(crate) collections: Vec<Collection>,
}

impl Default for KnownHost {
    fn default() -> Self {
        Self {
            shared: pf_client_core::trust::KnownHost::default(),
            wol_auto: false,
            exit_action: ExitAction::default(),
            collections: new_host_collections(),
        }
    }
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
/// serialized form is the one comparison that cannot miss a field.
impl PartialEq for KnownHost {
    fn eq(&self, other: &Self) -> bool {
        serde_json::to_value(self).ok() == serde_json::to_value(other).ok()
    }
}

/// 0.33.0), which need the pairing's Host power grant. Defaults to [`Self::None`]: a client
/// that quietly powered a machine down would be worse than one that never offered to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitAction {
    #[default]
    None,
    Sleep,
    Shutdown,
}

impl ExitAction {
    /// Every value, in the order the dropdown lists them.
    pub const ALL: [Self; 3] = [Self::None, Self::Sleep, Self::Shutdown];

    /// The host action id to invoke, or `None` when there is nothing to do.
    pub fn action_id(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Sleep => Some("power.sleep"),
            Self::Shutdown => Some("power.shutdown"),
        }
    }
}

/// One grid section: a named, ordered set of game ids. Exactly one per host is
/// [`Collection::dynamic`] (Library), whose members are computed rather than stored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct Collection {
    pub name: String,
    /// Member ids ([`GameEntry::id`] or [`DESKTOP_PIN_ID`]), in user order. Unbounded — there
    /// is no per-collection card limit.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub games: Vec<String>,
    /// Library: holds whatever is in no other collection, so `games` is always empty on disk.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dynamic: bool,
}

impl Collection {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            games: Vec::new(),
            dynamic: false,
        }
    }

    /// The one dynamic entry. Never removable, members never stored.
    pub fn library() -> Self {
        Self {
            name: LIBRARY_COLLECTION.to_string(),
            games: Vec::new(),
            dynamic: true,
        }
    }
}

/// Max *user* collections per host. The dynamic Library entry is not one of them.
pub const MAX_COLLECTIONS: usize = 20;

/// Max collection name length, in chars.
pub const MAX_COLLECTION_NAME: usize = 24;

/// The dynamic entry's name on a freshly migrated host — renameable like any other.
pub const LIBRARY_COLLECTION: &str = "Library";

/// The collection a migrated host's old pins land in, and the one a new host starts with.
pub const PINNED_COLLECTION: &str = "Pinned";

/// Pin ID for the "Desktop" card — a `games` key and a collection member like any other.
/// Never pruned, since no library listing contains it.
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

    /// The grid sections, in order. Never empty: every record carries at least Library.
    pub fn collections(&self) -> &[Collection] {
        &self.collections
    }

    fn collections_mut(&mut self) -> &mut Vec<Collection> {
        if self.collections.is_empty() {
            self.collections.push(Collection::library());
        }
        &mut self.collections
    }

    /// Index of the dynamic (Library) entry. Present on every migrated host; `None` only
    /// before migration, where there is nothing to draw anyway.
    pub fn library_index(&self) -> Option<usize> {
        self.collections().iter().position(|c| c.dynamic)
    }

    /// Which collection holds `id`, or `None` for Library — where every id that no
    /// collection names implicitly lives.
    pub fn collection_of(&self, id: &str) -> Option<usize> {
        self.collections()
            .iter()
            .position(|c| !c.dynamic && c.games.iter().any(|g| g == id))
    }

    /// Moves `id` into collection `to`, or back to Library with `None`. A game is in at
    /// most one collection, so this removes it from any other first. Appends: a just-moved
    /// card is looked for at the end of the block it joined.
    pub fn move_to(&mut self, id: &str, to: Option<usize>) {
        let collections = self.collections_mut();
        for entry in collections.iter_mut() {
            entry.games.retain(|g| g != id);
        }
        if let Some(entry) = to.and_then(|i| collections.get_mut(i)).filter(|c| !c.dynamic) {
            entry.games.push(id.to_string());
        }
    }

    /// This game's bound profile id, if it has one (the shared `game_profiles`).
    pub fn game_profile(&self, id: &str) -> Option<&str> {
        self.shared.profile_for_game(id)
    }

    /// Drops per-game state for ids the host no longer lists. `live` must come from a
    /// *successful* library fetch — an error or an offline host would otherwise wipe
    /// everything. [`DESKTOP_PIN_ID`] is always kept: it is never in a library listing.
    /// Returns whether anything was removed.
    pub fn prune_games(&mut self, live: impl Fn(&str) -> bool) -> bool {
        let before = self.game_profiles.len();
        self.shared
            .game_profiles
            .retain(|id, _| id == DESKTOP_PIN_ID || live(id));
        let mut dropped = self.game_profiles.len() != before;
        for entry in &mut self.collections {
            let before = entry.games.len();
            entry.games.retain(|id| id == DESKTOP_PIN_ID || live(id));
            dropped |= entry.games.len() != before;
        }
        dropped
    }
}

/// Editing a host's collections: what the collections modal and its dialogs drive. Split out
/// so the invariants have one home — a game in at most one collection, Library unremovable,
/// names trimmed and unique — and so no caller can reach past them into the vector itself.
///
impl KnownHost {
    /// The member `id` trades places with inside its own collection: its neighbour in
    /// *grid* order. [`DESKTOP_PIN_ID`] is an ordinary member here — it moves, and is moved
    /// past, like any card. `None` at either end of the block, and in Library, whose order
    /// is recency rather than the user's.
    pub fn collection_neighbour(&self, id: &str, forward: bool) -> Option<&str> {
        let at = self.collection_of(id)?;
        let games = &self.collections().get(at)?.games;
        let pos = games.iter().position(|g| g == id)?;
        if forward {
            games.get(pos + 1)
        } else {
            games[..pos].last()
        }
        .map(String::as_str)
    }

    /// Swaps `id` with [`Self::collection_neighbour`] — the in-collection card reorder.
    /// `false` when there is nowhere to go, which the caller shows as a reject nudge.
    pub fn swap_within_collection(&mut self, id: &str, forward: bool) -> bool {
        let Some(other) = self.collection_neighbour(id, forward).map(str::to_string) else {
            return false;
        };
        let Some(at) = self.collection_of(id) else {
            return false;
        };
        let Some(entry) = self.collections_mut().get_mut(at) else {
            return false;
        };
        let (Some(a), Some(b)) = (
            entry.games.iter().position(|g| g == id),
            entry.games.iter().position(|g| *g == other),
        ) else {
            return false;
        };
        entry.games.swap(a, b);
        true
    }

    /// Number of user collections — what [`MAX_COLLECTIONS`] bounds.
    pub fn user_collection_count(&self) -> usize {
        self.collections().iter().filter(|c| !c.dynamic).count()
    }

    pub fn can_add_collection(&self) -> bool {
        self.user_collection_count() < MAX_COLLECTIONS
    }

    /// Whether `name` may be used for collection `at` (`None` when adding): trimmed,
    /// non-empty, within [`MAX_COLLECTION_NAME`] and unique case-insensitively. What gates
    /// the add/rename confirm button.
    pub fn can_name(&self, at: Option<usize>, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_COLLECTION_NAME {
            return false;
        }
        !self
            .collections()
            .iter()
            .enumerate()
            .any(|(i, c)| Some(i) != at && c.name.eq_ignore_ascii_case(name))
    }

    /// Appends a user collection, returning its index. `None` when the name is refused or
    /// the host is at [`MAX_COLLECTIONS`].
    pub fn add_collection(&mut self, name: &str) -> Option<usize> {
        if !self.can_add_collection() || !self.can_name(None, name) {
            return None;
        }
        let collections = self.collections_mut();
        collections.push(Collection::new(name.trim()));
        Some(collections.len() - 1)
    }

    /// Renames any entry, Library included. `false` when the name is refused.
    pub fn rename_collection(&mut self, at: usize, name: &str) -> bool {
        if !self.can_name(Some(at), name) {
            return false;
        }
        let name = name.trim().to_string();
        match self.collections_mut().get_mut(at) {
            Some(entry) => {
                entry.name = name;
                true
            }
            None => false,
        }
    }

    /// Removes a user collection; its members fall back to Library. Refuses the dynamic
    /// entry — the missing Remove icon on that row is not this rule's only enforcement.
    pub fn remove_collection(&mut self, at: usize) -> bool {
        if self.collections().get(at).is_none_or(|c| c.dynamic) {
            return false;
        }
        self.collections_mut().remove(at);
        true
    }

    /// Moves an entry within the order, the dynamic one included: this vector *is* the grid
    /// section order.
    pub fn reorder_collection(&mut self, from: usize, to: usize) -> bool {
        let collections = self.collections_mut();
        if from >= collections.len() || to >= collections.len() || from == to {
            return false;
        }
        let entry = collections.remove(from);
        collections.insert(to, entry);
        true
    }
}

/// The collections a genuinely new host starts with: "Pinned" holding the Desktop card,
/// then Library — which is what a pre-collections install looked like after its first pair.
pub fn new_host_collections() -> Vec<Collection> {
    let mut pinned = Collection::new(PINNED_COLLECTION);
    pinned.games.push(DESKTOP_PIN_ID.to_string());
    vec![pinned, Collection::library()]
}

/// Upserts by `(addr, port)`, keeping the existing pin if the new record is unpaired (a fresh
/// mDNS discovery shouldn't clobber a paired host) — same reasoning for `mac` and `os`,
/// learned separately (see `App::drain_discovery`). The record's id, its profile bindings,
/// its pins, `wol_auto`, the exit action and `collections` are *always* kept from the existing
/// record: only their own screens change them, so no add/edit/re-pair flow may clobber any.
pub fn upsert_known_host(hosts: &mut Vec<KnownHost>, mut new: KnownHost) {
    let Some(existing) = hosts.iter_mut().find(|h| h.addr == new.addr && h.port == new.port) else {
        hosts.push(new);
        return;
    };
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
    new.profile_id.clone_from(&existing.profile_id);
    new.pinned_profiles.clone_from(&existing.pinned_profiles);
    new.game_profiles.clone_from(&existing.game_profiles);
    new.collections.clone_from(&existing.collections);
    new.wol_auto = existing.wol_auto;
    new.exit_action = existing.exit_action;
    *existing = new;
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
    /// the picture paced. The longest path, every layout, and the only one whose pacing is proven
    /// on hardware — so, the default.
    #[default]
    Software,
    /// The wire's Opus, decoded by the TV on its audio plane. No local decode at all. Stereo
    /// only — NDL's Opus struct has no multistream mapping field — and some sets accept the load
    /// and then play nothing, which no runtime probe detects.
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
            // SDL opens whatever the negotiated layout is; nothing folds.
            Self::Software => caps.max_channels,
            Self::NdlOpus => caps.max_channels.min(2),
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
    /// The wire preference sent in the handshake, which becomes the session-default pad kind.
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

    /// Whether a host pad of this kind can emit `DualSense` HID feedback (adaptive triggers,
    /// lightbar). The Edge is a `DualSense` plus extra buttons, so it carries the same effects.
    pub fn is_dualsense(self) -> bool {
        matches!(self, Self::DualSense | Self::DualSenseEdge)
    }
}

/// Override for the on-device log verbosity, settable live from the Diagnostics
/// screen — see `logger::set_level_override`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevelOverride {
    Debug,
    #[default]
    Info,
    Warn,
    Error,
}

/// Slider range, in 5 Mbps steps.
pub const BITRATE_MIN_KBPS: u32 = 10_000;
/// The one bitrate ceiling this client has. It bounds the manual slider AND, through `main`,
/// `punktfunk_core::abr`'s automatic wire-budget climb (`PUNKTFUNK_ABR_MAX_MBPS`) and the startup
/// link-capacity probe's burst target (`PUNKTFUNK_ABR_PROBE_KBPS`) — an Automatic session that
/// could climb past what the slider allows would just be a second, hidden setting.
pub const BITRATE_MAX_KBPS: u32 = 200_000;

/// A slider's discrete positions: a closed range walked in fixed steps.
///
/// One value type for every slider — the three HDR measurements and Bitrate — so the range, the
/// stop count, the value at a stop and the snap back onto the lattice all come from the same
/// numbers. They have to stay inverses of each other, and spelling each one out per slider is
/// how they stop being.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Lattice {
    pub lo: u32,
    pub hi: u32,
    pub step: u32,
}

impl Lattice {
    /// How many positions the slider has.
    #[must_use]
    pub fn stops(self) -> usize {
        ((self.hi - self.lo) / self.step) as usize + 1
    }

    /// Which position `value` sits at.
    #[must_use]
    pub fn index(self, value: u32) -> usize {
        ((value.clamp(self.lo, self.hi) - self.lo) / self.step) as usize
    }

    /// The value at `stop`, which is clamped into range first.
    #[must_use]
    pub fn value(self, stop: i32) -> u32 {
        let stop = stop.clamp(0, self.stops() as i32 - 1) as u32;
        (self.lo + stop * self.step).min(self.hi)
    }

    /// Where `value` sits along the track, as 0..1.
    #[must_use]
    pub fn fraction(self, value: u32) -> f32 {
        let last = self.stops().saturating_sub(1);
        if last == 0 {
            0.0
        } else {
            self.index(value) as f32 / last as f32
        }
    }

    /// The stop nearest a 0..1 position along the track — the inverse of [`Lattice::fraction`].
    #[must_use]
    pub fn stop_at(self, fraction: f32) -> i32 {
        let last = self.stops().saturating_sub(1) as f32;
        (fraction.clamp(0.0, 1.0) * last).round() as i32
    }

    /// Clamps into range, then rounds to the nearest stop.
    #[must_use]
    pub fn snap(self, value: u32) -> u32 {
        let offset = value.clamp(self.lo, self.hi) - self.lo;
        self.value(((offset + self.step / 2) / self.step) as i32)
    }
}

/// Peak-brightness slider, in nits — and, while the calibration screen is up, the mastering
/// maximum the pattern declares. The ceiling has to sit well above any panel the app runs on, or
/// the slider ends before the TV starts compressing and the reading cannot be taken: a CX
/// measures ~790, a G3 with MLA ~1300, a 2025 G5 ~2400.
pub const HDR_PEAK: Lattice = Lattice {
    lo: 300,
    hi: 4_000,
    step: 10,
};
/// Full-field (frame-average) slider, in nits, and the pattern's declared `MaxFALL` while it is
/// being measured. OLEDs hold ~140-180 once ABL settles; backlit LCDs hold far more, so this too
/// has to reach past any of them for the flattening point to land inside the slider.
pub const HDR_FRAME_AVG: Lattice = Lattice {
    lo: 100,
    hi: 1_000,
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

/// The panel's HDR colour volume, as measured by the calibration screen.
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
    /// The black floor in the 0.0001 cd/m² units the wire carries, never zero: ST.2086 reads a
    /// zero there as "unknown", and a self-emissive panel's real floor is better described by the
    /// smallest luminance the field can express than by no answer at all.
    #[must_use]
    pub fn min_luminance_units(self) -> u32 {
        ((crate::core::pq::pq_nits(self.black_code) * 10_000.0).round() as u32).max(1)
    }

    /// HDR10 mastering metadata describing this panel.
    ///
    /// It goes two places, and both matter. To NDL, where it is the volume the TV tone-maps the
    /// stream into: give it the panel's real numbers and that map becomes an identity. And to the
    /// host in `Hello::display_hdr`, which codes it into the virtual display's CTA-861.3 HDR block,
    /// so the game renders to this volume rather than to a placeholder someone else has to undo.
    /// One tone map, at the source — which is what `HGiG` asks for.
    ///
    /// The defaults are an LG CX's, which is what this client sent to every TV before the
    /// calibration screen existed.
    #[must_use]
    pub fn hdr_meta(self) -> punktfunk_core::quic::HdrMeta {
        punktfunk_core::quic::HdrMeta {
            // G, B, R order (ST.2086), 1/50000 chromaticity units — BT.2020 primaries.
            display_primaries: [[8_500, 39_850], [6_550, 2_300], [35_400, 14_600]],
            white_point: [15_635, 16_450], // D65
            max_display_mastering_luminance: u32::from(self.peak_nits) * 10_000,
            min_display_mastering_luminance: self.min_luminance_units(),
            max_cll: self.peak_nits,
            max_fall: self.frame_avg_nits,
        }
    }
}

/// When the shared shell takes the menus over. Mirrors Android's `gamepad_ui_mode`, and
/// serializes to the same two strings the shell's own row stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GamepadUiMode {
    /// Only while a real game pad is attached. A Magic Remote is not one — see
    /// `platform::webos::gamepad::any_pad_connected`.
    #[default]
    Connected,
    /// Whenever the switch is on, pad or no pad.
    Always,
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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    #[serde(default = "crate::core::settings::default_document")]
    pub settings: pf_client_core::trust::Settings,
    pub known_hosts: Vec<KnownHost>,
    /// The sidebar host row the user last had active — so relaunching lands back on its game
    /// grid instead of an unfocused sidebar. `(host, port)`, not an index: `known_hosts` order
    /// isn't stable across a forget/re-add.
    pub selected_host: Option<(String, u16)>,
    /// The app version that last wrote this document (`CARGO_PKG_VERSION`). `None` means it
    /// was written before versioning existed — the only signal a future migration gets about
    /// which shape it is reading. `store::load` stamps it on first sight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The settings-profile catalog: named bundles of sparse overrides, the model punktfunk's
    /// other clients keep in `client-profiles.json`. Here it rides the one document this client
    /// writes, for the reason everything else does — one file, one writer, no merge.
    ///
    /// A game's settings ARE a profile (the shared `game_profiles`), which is what lets the
    /// shared shell list them and bind one to a title. A TV with no desktop app beside it can still
    /// fill this: opening a game's settings and changing a row creates the profile.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<StreamProfile>,
}

/// Cover-art paths for a title (host-relative, fetched via mTLS). Cards prefer
/// `portrait` then `header` then `hero`; the hero backdrop prefers `hero`.
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
        h.bind_game_profile("doom", Some("p1"));
        let json = serde_json::to_value(&h).unwrap();
        assert_eq!(json["addr"], "10.0.0.2");
        assert_eq!(json["fp_hex"], "5a".repeat(32));
        assert_eq!(json["wol_auto"], true);
        assert_eq!(json["game_profiles"]["doom"], "p1");
        assert!(json.get("shared").is_none(), "flattened, not nested");
        let back: KnownHost = serde_json::from_value(json).unwrap();
        assert_eq!(back, h);
        assert!(back.is_paired());
        assert_eq!(back.fingerprint(), Some([0x5a; 32]));
        assert_eq!(back.collections().len(), 2, "a fresh record carries Pinned and Library");

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
