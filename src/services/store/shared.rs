//! The settings document, in punktfunk's shared shape.
//!
//! `settings.json` persists [`pf_client_core::trust::Settings`] — the same struct the desktop
//! shells, the session binary and the Android client write. This module is the only place that
//! knows how this client's in-memory [`Settings`] maps onto it, so there is ONE stored schema
//! and two presentations of it: the existing webOS UI, and the shared gamepad shell, which
//! speaks `trust::Settings` natively and therefore needs no conversion at all.
//!
//! Anything punktfunk has no field for rides in `trust::Settings::extra`, a `#[serde(flatten)]`
//! map that every writer round-trips untouched. That is what keeps a TV-only row — HDR
//! calibration, the LG game-mode toggle, the log level — from either being dropped by another
//! client or forced into the shared struct. Android's platform rows use the same mechanism.
//!
//! ⚠ Two in-memory views of one file can drift within a session. The shell re-reads through
//! `SettingsStore::load` before every mutation, and the flip must reload the other side when it
//! switches; nothing here can enforce that.

use pf_client_core::trust;

use pf_client_core::profiles;

use crate::core::model::{Persisted, DESKTOP_PIN_ID};
use crate::core::settings::TvSettings;
use pf_client_core::trust::Settings;

/// The shell's copy of a host record: the shared half, verbatim (plan D8).
pub fn to_shared_host(h: &crate::core::model::KnownHost) -> trust::KnownHost {
    h.shared.clone()
}

/// The shell's stable row key for a host: its pinned fingerprint, else `addr:port`
/// (`pf_console_ui::HostRow::key`, the desktop's rule — the two must agree or a link copied
/// on one client names nothing on the other).
///
/// Every host-scoped command the shell raises (Forget, Wake, Edit, the clipboard toggle)
/// carries this string and nothing else, so [`find_known`] has to invert it. That is the
/// whole reason both live here, ungated, where `task test` can actually run them.
pub fn host_key(fp_hex: &str, addr: &str, port: u16) -> String {
    if fp_hex.is_empty() {
        format!("{addr}:{port}")
    } else {
        fp_hex.to_string()
    }
}

/// [`host_key`] for a record this client holds.
pub fn known_host_key(h: &crate::core::model::KnownHost) -> String {
    host_key(&h.fp_hex, &h.addr, h.port)
}

/// The record a shell row key addresses, or `None` if it names no host this client knows.
///
/// A pinned-profile card's key is `<host key>\0<profile id>`. This client mints none (it has
/// no profile catalog — see `store::console::ConsoleStore::profiles`), but the key is the
/// shell's shape, not ours, so the suffix is trimmed rather than trusted to be absent.
pub fn find_known(hosts: &[crate::core::model::KnownHost], key: &str) -> Option<usize> {
    let key = key.split('\0').next().unwrap_or(key);
    hosts.iter().position(|h| known_host_key(h) == key)
}

/// The store a library id belongs to — the `steam` of `steam:570`.
///
/// `GameEntry::id` is store-qualified by contract (see its doc); the shell prints this as the
/// card's store line. An id with no prefix yields the whole id rather than an empty string,
/// which reads as "unknown store" instead of a blank.
pub fn store_of(id: &str) -> &str {
    id.split_once(':').map_or(id, |(store, _)| store)
}

/// A pinned fingerprint as the lowercase hex the shell's rows and links carry.
pub fn hex(bytes: [u8; 32]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The inverse: a fingerprint the shell handed back, or `None` for anything that is not
/// exactly 32 bytes of hex.
///
/// Strict on purpose. This is the pin a session is verified against, so a short, odd-length or
/// non-hex string has to fail rather than silently produce a shorter key that would either be
/// rejected on the wire or — worse — compared against the wrong thing.
pub fn parse_fp(fp_hex: &str) -> Option<[u8; 32]> {
    if fp_hex.len() != 64 {
        return None;
    }
    let bytes: Vec<u8> = (0..64)
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(fp_hex.get(i..i + 2)?, 16).ok())
        .collect();
    bytes.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host records reach the shell named and reachable, with `paired` following the fingerprint
    /// and the hex being the full 32 bytes.
    #[test]
    fn hosts_convert_for_the_shell() {
        let mut paired = crate::core::model::KnownHost {
            shared: trust::KnownHost {
                name: "desk".into(),
                addr: "192.168.1.5".into(),
                port: 47_989,
                mgmt_port: Some(47_990),
                ..Default::default()
            },
            ..Default::default()
        };
        paired.set_fingerprint([0xab; 32]);
        let h = to_shared_host(&paired);
        assert_eq!(
            (h.name.as_str(), h.addr.as_str(), h.port),
            ("desk", "192.168.1.5", 47_989)
        );
        assert_eq!(h.mgmt_port, Some(47_990));
        assert!(h.paired);
        assert_eq!(h.fp_hex, "ab".repeat(32), "32 bytes is 64 hex chars");
        assert_eq!(paired.fingerprint(), Some([0xab; 32]), "the pin decodes back");
        // The record carries a stable id now, minted once: the same answer every call.
        assert!(h.id.is_some(), "a saved record has an id");
        assert_eq!(to_shared_host(&paired).id, h.id, "same host, same answer");

        let unpaired = crate::core::model::KnownHost {
            shared: trust::KnownHost {
                fp_hex: String::new(),
                paired: false,
                ..paired.shared.clone()
            },
            ..paired
        };
        let h = to_shared_host(&unpaired);
        assert!(!h.paired, "no fingerprint means not paired");
        assert!(h.fp_hex.is_empty());
    }

    /// The shell addresses hosts by row key alone, so every key this client mints must lead
    /// back to the record it came from — a miss is a Forget or a Wake that silently does
    /// nothing. Both spellings, and the pinned-card suffix the shell may append.
    #[test]
    fn row_keys_invert_to_their_host() {
        let mut paired = crate::core::model::KnownHost {
            shared: trust::KnownHost {
                name: "desk".into(),
                addr: "192.168.1.5".into(),
                port: 47_989,
                ..Default::default()
            },
            ..Default::default()
        };
        paired.set_fingerprint([0xab; 32]);
        let unpaired = crate::core::model::KnownHost {
            shared: trust::KnownHost {
                name: "typed".into(),
                addr: "10.0.0.9".into(),
                port: 47_989,
                ..Default::default()
            },
            ..Default::default()
        };
        let hosts = vec![paired.clone(), unpaired.clone()];

        assert_eq!(known_host_key(&paired), "ab".repeat(32), "paired keys on its pin");
        assert_eq!(
            known_host_key(&unpaired),
            "10.0.0.9:47989",
            "unpaired keys on addr:port"
        );
        assert_eq!(find_known(&hosts, &known_host_key(&paired)), Some(0));
        assert_eq!(find_known(&hosts, &known_host_key(&unpaired)), Some(1));
        // A pinned profile card rides its host's key behind a NUL.
        let pinned = format!("{}\0work", known_host_key(&paired));
        assert_eq!(find_known(&hosts, &pinned), Some(0), "the card resolves to its host");
        assert_eq!(find_known(&hosts, "nothing"), None);
    }

    /// The pin the shell hands back has to survive the round trip exactly, and anything that
    /// is not a whole fingerprint has to be refused rather than truncated — this is the value
    /// a session is verified against.
    #[test]
    fn fingerprints_round_trip_and_refuse_junk() {
        let fp = [0xab; 32];
        assert_eq!(parse_fp(&hex(fp)), Some(fp));
        assert_eq!(parse_fp(""), None, "an unpaired row carries no pin");
        assert_eq!(parse_fp(&"ab".repeat(31)), None, "62 hex chars is not a fingerprint");
        assert_eq!(parse_fp(&"zz".repeat(32)), None, "not hex at all");
    }

    /// The card's store line comes off the id, and an id without a prefix must not read blank.
    #[test]
    fn store_comes_off_the_id() {
        assert_eq!(store_of("steam:570"), "steam");
        assert_eq!(store_of("custom:my-thing"), "custom");
        assert_eq!(store_of("bare"), "bare");
    }
}

/// The settings one launch runs with: the global document with the resolved profile applied
/// — one-off ?? per-title ?? host default, the shared resolver's own precedence — then the
/// Desktop card's standing rule that pointer capture is off there (unless the profile sets
/// the mouse mode), then this set's caps. The single merge point both menu loops call.
pub fn launch_settings(
    state: &Persisted,
    addr: &str,
    port: u16,
    launch: Option<&str>,
    one_off: Option<&str>,
) -> Settings {
    let id = launch.unwrap_or(DESKTOP_PIN_ID);
    let host = state.known_hosts.iter().find(|h| h.addr == addr && h.port == port);
    let per_title = host.and_then(|h| h.game_profile(id));
    let bound = host.and_then(|h| h.profile_id.as_deref());
    let catalog = profiles::ProfilesFile {
        version: profiles::PROFILES_VERSION,
        profiles: state.profiles.clone(),
    };
    let profile = trust::resolve_profile(&catalog, bound, per_title, one_off);
    let global = &state.settings;
    let mut settings = profile
        .as_ref()
        .map_or_else(|| global.clone(), |p| p.overrides.apply(global));
    if id == DESKTOP_PIN_ID && profile.as_ref().is_none_or(|p| p.overrides.mouse_mode.is_none()) {
        settings.set_cursor_capture(false);
    }
    settings.clamp_to_caps();
    settings
}

/// Point a host's default at a catalog profile, or clear it. Refuses an id the catalog does not
/// hold: the record must never name a profile nothing resolves. Reports whether it changed.
pub fn bind_host_profile(state: &mut Persisted, key: &str, profile_id: Option<String>) -> bool {
    if let Some(id) = &profile_id {
        if !state.profiles.iter().any(|p| p.id == *id) {
            tracing::warn!(%id, "bind to a profile this document does not hold");
            return false;
        }
    }
    let Some(i) = find_known(&state.known_hosts, key) else {
        tracing::warn!(%key, "profile bind for an unknown host");
        return false;
    };
    let host = &mut state.known_hosts[i];
    if host.profile_id == profile_id {
        return false;
    }
    host.profile_id = profile_id;
    true
}

/// Pin or unpin one profile card on a host. Appends in press order; idempotent, so a repeat
/// inside one refresh cannot double a card. Reports whether it changed.
pub fn set_pin(state: &mut Persisted, key: &str, profile_id: String, pin: bool) -> bool {
    let Some(i) = find_known(&state.known_hosts, key) else {
        tracing::warn!(%key, "pin toggle for an unknown host");
        return false;
    };
    let pins = &mut state.known_hosts[i].pinned_profiles;
    let had = pins.contains(&profile_id);
    if pin && !had {
        pins.push(profile_id);
    } else if !pin && had {
        pins.retain(|id| *id != profile_id);
    } else {
        return false;
    }
    true
}

#[cfg(test)]
mod launch_tests {
    use super::*;
    use pf_client_core::profiles::StreamProfile;

    fn state_with(profile: StreamProfile, bind: impl FnOnce(&mut crate::core::model::KnownHost)) -> Persisted {
        let mut host = crate::core::model::KnownHost {
            shared: trust::KnownHost {
                addr: "10.0.0.2".into(),
                port: 47989,
                ..Default::default()
            },
            ..Default::default()
        };
        bind(&mut host);
        let mut state = Persisted::default();
        state.known_hosts.push(host);
        state.profiles.push(profile);
        state
    }

    /// The gamepad shell's two profile writes: a pin toggles once per press and a card key
    /// (`host\0profile`) addresses the host; the default bind refuses an unknown profile.
    #[test]
    fn pins_and_host_bindings_write_the_record_once() {
        let profile = StreamProfile::new("Work");
        let pid = profile.id.clone();
        let mut state = state_with(profile, |_| {});
        let key = known_host_key(&state.known_hosts[0]);
        assert!(set_pin(&mut state, &key, pid.clone(), true));
        assert!(!set_pin(&mut state, &key, pid.clone(), true), "a repeat is a no-op");
        assert_eq!(state.known_hosts[0].pinned_profiles, vec![pid.clone()]);
        let card_key = format!("{key}\0{pid}");
        assert!(set_pin(&mut state, &card_key, pid.clone(), false));
        assert!(state.known_hosts[0].pinned_profiles.is_empty());
        assert!(!bind_host_profile(&mut state, &key, Some("nothing".into())));
        assert!(bind_host_profile(&mut state, &key, Some(pid.clone())));
        assert_eq!(state.known_hosts[0].profile_id.as_deref(), Some(pid.as_str()));
        assert!(bind_host_profile(&mut state, &key, None));
        assert!(!set_pin(&mut state, "10.9.9.9:1", pid, true), "unknown host");
    }

    /// A title binding beats the host's default; a dangling id falls through to it; the
    /// desktop card streams with capture off unless the profile pins the mouse mode.
    #[test]
    fn launch_follows_the_shared_precedence_and_the_desktop_rule() {
        let mut title_profile = StreamProfile::new("Doom");
        title_profile.overrides.bitrate_kbps = Some(12_000);
        let tid = title_profile.id.clone();
        let mut host_profile = StreamProfile::new("Work");
        host_profile.overrides.bitrate_kbps = Some(34_000);
        let hid = host_profile.id.clone();
        let mut state = state_with(title_profile, |h| {
            h.bind_game_profile("doom", Some(&tid));
        });
        state.profiles.push(host_profile);
        state.known_hosts[0].profile_id = Some(hid);
        state.settings.set_cursor_capture(true);

        let doom = launch_settings(&state, "10.0.0.2", 47989, Some("doom"), None);
        assert_eq!(doom.bitrate_kbps, 12_000);
        let other = launch_settings(&state, "10.0.0.2", 47989, Some("quake"), None);
        assert_eq!(other.bitrate_kbps, 34_000, "the host default covers an unbound title");
        let desktop = launch_settings(&state, "10.0.0.2", 47989, None, None);
        assert!(!desktop.cursor_capture(), "the desktop card streams with capture off");
        assert!(other.cursor_capture(), "a game keeps the global capture");

        state.known_hosts[0].bind_game_profile("doom", Some("gone"));
        let dangling = launch_settings(&state, "10.0.0.2", 47989, Some("doom"), None);
        assert_eq!(
            dangling.bitrate_kbps, 34_000,
            "a dangling title binding falls back to the host default"
        );
    }
}
