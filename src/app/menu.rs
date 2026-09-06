//! The TV's own settings vocabulary: the locks this set puts on the shared rows, the host
//! power row's picks, and the log levels. The rows themselves, their labels and their steps
//! are the console's (`pf_console_ui::settings_rows`, read by `app::state::settingspage`).

use crate::core::caps::video_caps;
use crate::core::event::MenuEvent;
use crate::core::settings::TvSettings;
use crate::services::store::{AudioRoutePref, CodecPref, ExitAction, GamepadType, LogLevelOverride, Settings};
use crate::ui::focus::Dir;

/// This app's input vocabulary mapped onto `ui`'s spatial one. `ui` navigates by
/// direction; deciding that "Up" means a d-pad press is the app's business, and this is
/// where that translation happens once.
pub fn nav_dir(ev: MenuEvent) -> Option<Dir> {
    match ev {
        MenuEvent::Up => Some(Dir::Up),
        MenuEvent::Down => Some(Dir::Down),
        MenuEvent::Left => Some(Dir::Left),
        MenuEvent::Right => Some(Dir::Right),
        _ => None,
    }
}

/// The shared rows this set can lock — the ones [`row_lock`] answers for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SettingsRow {
    /// Locked where the backend has no HEVC — there is only one decodable codec then.
    Codec,
    /// HDR applies only to HEVC, so the row locks on an explicit H.264 pick.
    Hdr,
    /// Locked where the backend is capped at stereo — the only channel count then.
    Audio,
    /// Which controller the host presents to the game — see `store::GamepadType`.
    Gamepad,
    /// Whether the shared gamepad shell may front the app at all.
    GamepadUi,
    /// When it does. Directly below the switch that gates it.
    GamepadUiMode,
}

/// Whether this build links the shared shell at all. Every Linux target does (see Cargo.toml);
/// elsewhere the row is listed and locked rather than missing, which keeps the screen's row
/// indices the same everywhere.
pub(crate) const CONSOLE_UI_BUILT: bool = cfg!(target_os = "linux");

/// Why a row is shown but not editable, or `None` while it is. A lock can lift without a
/// restart (switching codec away from H.264, plugging in a pad). One predicate for the renderer
/// and the input path, so a greyed row and a rejected keypress can't disagree.
#[derive(Clone, Copy)]
pub(crate) enum RowLock {
    /// HDR under an explicit H.264 pick: the host never resolves HDR for such a session, so the
    /// toggle would be a no-op. `Automatic` leaves it editable — HEVC may still be resolved.
    HdrNeedsHevc,
    /// The active backend has no HDR at all (NDL v1) — nothing to toggle either way.
    NoHdr,
    /// One decodable codec, so `codec_prefs` collapses to a single entry.
    OneCodec,
    /// The active backend decodes one channel count (NDL v1), so no route could offer more.
    StereoOnly,
    /// The *selected audio processing* carries stereo only (`AudioRoutePref::max_channels`).
    RouteStereoOnly,
    /// Nothing is plugged into the TV, so there is no controller to describe to the host.
    NoGamepad,
    /// This build has no shared shell linked (see [`CONSOLE_UI_BUILT`]).
    NoShell,
    /// The console switch above is off, so its mode picks nothing.
    ConsoleOff,
}

/// What is known about this pairing's power rights on the host menu's host — the screen state
/// behind the exit-behaviour row, and the one predicate that both greys it and rejects a
/// keypress on it.
///
/// Only [`Self::Rights`] carries a host answer; everything else is a reason we don't have one,
/// which matters because a refused pairing needs widening on the host while an unreachable one
/// may simply be asleep — the same locked row for opposite reasons.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PowerAccess {
    /// No pairing with this host, so there is no access mask to carry a power grant.
    NotPaired,
    /// The probe hasn't answered yet.
    Unknown,
    /// The host never answered — asleep or off the network.
    Unreachable,
    /// The host answered, but has no power actions at all: it predates the route (a 404).
    Unsupported,
    /// What the host says this pairing may invoke. Empty means it answered and offers nothing.
    Rights(crate::services::power::PowerRights),
}

impl PowerAccess {
    /// Whether the row is usable at all — the host answered and offers something.
    pub(crate) fn unlocked(self) -> bool {
        matches!(self, Self::Rights(r) if r.any())
    }

    /// Whether one pick would actually be accepted. Unknown-yet and unreachable say yes: the
    /// row is locked in those states anyway, and claiming a stored pick is impossible on no
    /// evidence would be worse than saying nothing.
    pub(crate) fn allows(self, action: ExitAction) -> bool {
        match self {
            Self::Rights(r) => r.allows(action),
            Self::NotPaired | Self::Unknown | Self::Unreachable | Self::Unsupported => true,
        }
    }
}

/// Host power row indices.
pub const POWER_ROW_AUTO: usize = 0;
pub const POWER_ROW_EXIT: usize = 1;
pub const POWER_ROW_COUNT: usize = 2;

pub(crate) fn exit_action_label(action: ExitAction) -> &'static str {
    match action {
        ExitAction::None => "None",
        ExitAction::Sleep => "Sleep",
        ExitAction::Shutdown => "Shut down",
    }
}

/// What each pick actually does to the machine. Spelled out per value because the labels name
/// a power state and not its cost: one of these ends the host's uptime.
pub(crate) fn exit_action_caption(action: ExitAction) -> &'static str {
    match action {
        ExitAction::None => "The host keeps running after you leave",
        ExitAction::Sleep => "Suspend the host — Wake-on-LAN brings it back",
        ExitAction::Shutdown => "Power the host off completely",
    }
}

pub(crate) fn exit_action_current_index(action: ExitAction) -> usize {
    ExitAction::ALL.iter().position(|&a| a == action).unwrap_or(0)
}

/// Why a row is fixed, as the row's caption. `webos_major` is the OS major (`None` where it
/// couldn't be read).
pub(crate) fn lock_caption(lock: RowLock, webos_major: Option<u32>) -> String {
    let source = || match webos_major {
        Some(major) => format!("webOS {major}"),
        None => "this TV".to_string(),
    };
    match lock {
        RowLock::HdrNeedsHevc => "HDR is not supported by H.264".to_string(),
        RowLock::NoHdr => format!("HDR is not supported by {}", source()),
        RowLock::OneCodec => format!("H.264 is the only codec supported by {}", source()),
        RowLock::StereoOnly => format!("Stereo is the only layout supported by {}", source()),
        RowLock::RouteStereoOnly => format!(
            "{} audio processing decodes stereo only — change it under Audio",
            audio_route_label(AudioRoutePref::NdlOpus),
        ),
        RowLock::NoGamepad => "Connect a controller to your TV".to_string(),
        RowLock::NoShell => "This build has no controller UI".to_string(),
        RowLock::ConsoleOff => "Turn the controller UI on to choose when".to_string(),
    }
}

/// `detected` is the attached pad per `gamepad::detect_type` — `None` with nothing attached
/// (or an unrecognized pad), which is what locks the Gamepad row.
pub(crate) fn row_lock(row: SettingsRow, settings: &Settings, detected: Option<GamepadType>) -> Option<RowLock> {
    let caps = video_caps();
    match row {
        SettingsRow::Hdr if !caps.hdr => Some(RowLock::NoHdr),
        SettingsRow::Hdr if settings.codec_pref() == CodecPref::H264 => Some(RowLock::HdrNeedsHevc),
        SettingsRow::Codec if caps.codec_prefs().len() < 2 => Some(RowLock::OneCodec),
        // Device before route: where the client itself decodes stereo only, no audio-processing
        // pick could widen it, and naming one would send the user somewhere that cannot help.
        SettingsRow::Audio if channel_options_up_to(caps.max_channels).len() < 2 => Some(RowLock::StereoOnly),
        SettingsRow::Audio if audio_channel_options(settings).len() < 2 => Some(RowLock::RouteStereoOnly),
        SettingsRow::Gamepad if detected.is_none() => Some(RowLock::NoGamepad),
        SettingsRow::GamepadUi | SettingsRow::GamepadUiMode if !CONSOLE_UI_BUILT => Some(RowLock::NoShell),
        // The mode decides nothing while the switch above it is off. Greyed, not hidden: the
        // dependency is the point, and it sits directly under the row that lifts it.
        SettingsRow::GamepadUiMode if !settings.gamepad_ui() => Some(RowLock::ConsoleOff),
        _ => None,
    }
}

pub fn cycle_index(current: usize, len: usize, forward: bool) -> usize {
    if forward {
        (current + 1) % len
    } else {
        (current + len - 1) % len
    }
}

pub const LOG_LEVEL_OPTIONS: [LogLevelOverride; 4] = [
    LogLevelOverride::Debug,
    LogLevelOverride::Info,
    LogLevelOverride::Warn,
    LogLevelOverride::Error,
];

pub fn log_level_label(l: LogLevelOverride) -> &'static str {
    match l {
        LogLevelOverride::Debug => "Debug",
        LogLevelOverride::Info => "Info",
        LogLevelOverride::Warn => "Warn",
        LogLevelOverride::Error => "Error",
    }
}

pub fn log_level_dropdown_current_index(level: LogLevelOverride) -> usize {
    LOG_LEVEL_OPTIONS.iter().position(|&o| o == level).unwrap_or(0)
}

/// Every channel count this client can label; what is *offered* is [`audio_channel_options`].
const AUDIO_CHANNELS: [(u8, &str); 3] = [(2, "Stereo"), (6, "5.1 surround"), (8, "7.1 surround")];

/// The channel counts offered: what this client can decode, capped by what the selected route
/// can put on a speaker (`AudioRoutePref::max_channels`). Not filtered by the TV's current
/// Sound Out: that one changes under a running app and is applied per session instead.
pub fn audio_channel_options(settings: &Settings) -> &'static [(u8, &'static str)] {
    channel_options_up_to(settings.audio_route().max_channels(video_caps()))
}

/// The prefix of [`AUDIO_CHANNELS`] a `max`-channel ceiling leaves.
fn channel_options_up_to(max: u8) -> &'static [(u8, &'static str)] {
    let offered = AUDIO_CHANNELS.iter().take_while(|(c, _)| *c <= max).count();
    &AUDIO_CHANNELS[..offered]
}

/// A route's label. Names the decode step and the sink behind it — the pick is a hardware
/// path, and this row's audience is the one that wants the API named.
pub(crate) fn audio_route_label(route: AudioRoutePref) -> &'static str {
    match route {
        AudioRoutePref::Software => "Software (SDL)",
        AudioRoutePref::NdlOpus => "Offload (NDL)",
    }
}
