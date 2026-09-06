//! Runtime device capability detection.
//!
//! This client ships one binary to an open-ended set of TVs — a 2020 CX on webOS 5.6 and a
//! 2025 G5 on webOS 10.3 are both targets, and neither is the last one. Anything that
//! differs per model therefore has to be decided **at runtime**, from what the device
//! actually reports, rather than baked in.
//!
//! One thing deliberately **not** here: CPU codegen. `-C target-cpu` is a compile-time
//! flag, so a single `.ipk` cannot vary it per device — the baseline stays at the oldest
//! supported model and that is simply the cost of one binary. What *can* vary is
//! behaviour, and that is what this module feeds.
//!
//! **Detection is preferred by attempt, not by lookup table.** A table of model names is
//! wrong the day a TV ships that isn't in it. Where a capability can be probed by trying
//! it and handling failure (see `ndl::NdlVideo::load`'s audio fallback), that is always
//! the better mechanism; the facts here are for the decisions that can't be probed
//! cheaply, and for the log line that makes a bug report from an unknown model useful.
//!
//! **NDL generation.** The same runtime library ships v2 on webOS 5+ and a PTS-less,
//! H.264-only v1 on 3.5-4.x. [`ndl_generation`] only encodes the known version ranges; whether
//! the symbols are there is decided by the `dlsym` probe in `ndl` (the second, decisive gate).
use std::sync::OnceLock;

use anyhow::bail;

use crate::core::caps::VideoCaps;

/// TV capabilities detected at runtime (best-effort; missing sources fall back safely).
#[derive(Clone, Debug)]
pub struct DeviceInfo {
    /// CPU cores (drives off-main-thread work before contention).
    pub cores: usize,
    /// Major webOS release (5, 6, … 10), when it can be determined.
    pub webos_major: Option<u32>,
    /// Marketing model string, e.g. `OLED65G58LW.DEUQLJP`. Diagnostics only — never
    /// branch on this, see the module docs.
    pub model: Option<String>,
    /// webOS TV SDK version (major, minor) — see [`sdk_version`]. The NDL generation
    /// choice is written against this field, not `webos_major`.
    pub sdk_version: Option<(u32, u32)>,
    /// `otaId` from `getSystemInfo`, e.g. `HE_DTV_W19H_...`. Display-only.
    pub ota_id: Option<String>,
    /// SoC/board codename from `/etc/prefs/properties/machineName`, e.g. `m16p`, `k5lp`.
    pub machine_name: Option<String>,
}

/// webOS publishes these as plain JSON. Readable from a Dev-Mode shell; whether the
/// jailed app can read them varies, so every read is optional.
const OS_INFO: &str = "/var/run/nyx/os_info.json";
const DEVICE_INFO: &str = "/var/run/nyx/device_info.json";
const MACHINE_NAME_FILE: &str = "/etc/prefs/properties/machineName";
/// Present only when the `k5lp`/`k3lp` jailer config is sane — see [`jail_config_broken`].
const RTKMEM_DEVICE: &std::ffi::CStr = c"/dev/rtkmem";

/// Extract JSON field without parser (avoids serde on filesystem source).
fn json_str_field(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let rest = text.split_once(&needle)?.1;
    let rest = rest.split_once(':')?.1;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
    let (value, _) = rest.split_once('"')?;
    Some(value.to_string())
}

/// Parses `"4.3.0"` / `"5.2.0"` into `(major, minor)`. Extra segments (patch) are ignored —
/// every version constraint compares major, and minor only where it appears here.
fn parse_sdk_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().and_then(|m| m.parse().ok()).unwrap_or(0);
    Some((major, minor))
}

/// One cached `os_info.json` text — it can't change under a running app.
fn os_info() -> &'static str {
    static TEXT: OnceLock<String> = OnceLock::new();
    TEXT.get_or_init(|| std::fs::read_to_string(OS_INFO).unwrap_or_default())
}

/// One cached `getSystemInfo` payload for every field read from it: the Luna round-trip is on
/// the startup path and both readers want the same reply.
fn system_info() -> &'static str {
    static PAYLOAD: OnceLock<String> = OnceLock::new();
    PAYLOAD.get_or_init(|| {
        if !super::luna::available() {
            return String::new();
        }
        super::luna::call_capture(
            "luna://com.webos.service.tv.systemproperty/getSystemInfo",
            r#"{"keys":["sdkVersion","otaId","UHD"]}"#,
            super::luna::CALL_TIMEOUT,
        )
        .unwrap_or_default()
    })
}

/// The webOS TV SDK version (`sdkVersion`, not the Open webOS release) — the field
/// [`ndl_generation`] is written against. Resolved once, in precedence
/// order: launch-param override, `os_info.json`'s `webos_release`, then Luna `getSystemInfo`.
///
/// `os_info.json` first because Luna costs a subprocess spawn plus up to `CALL_TIMEOUT` on a TV
/// that never answers, and consumers only read the major, which `webos_release` gives.
pub fn sdk_version() -> Option<(u32, u32)> {
    static SDK_VERSION: OnceLock<Option<(u32, u32)>> = OnceLock::new();
    *SDK_VERSION.get_or_init(|| {
        if let Some(over) = crate::logger::webos_sdk_override() {
            if let Some(v) = parse_sdk_version(over) {
                tracing::info!("webOS SDK version overridden at launch: {over}");
                return Some(v);
            }
            tracing::warn!("webos_sdk launch param {over:?} unparseable — ignoring");
        }
        // Not the same field: `webos_release` is the OS release. Majors agree in practice, which
        // is all any consumer reads — but log it, since `sdk=` is then a guess.
        if let Some((major, _)) = json_str_field(os_info(), "webos_release").and_then(|v| parse_sdk_version(&v)) {
            tracing::info!("assuming SDK major {major} from os_info webos_release");
            return Some((major, 0));
        }
        json_str_field(system_info(), "sdkVersion").and_then(|s| parse_sdk_version(&s))
    })
}

/// `otaId` from Luna's `getSystemInfo`, e.g. `HE_DTV_W19H_...`. Display-only.
fn ota_id() -> Option<String> {
    json_str_field(system_info(), "otaId")
}

/// The panel's own stream mode — what the shared settings document's `0` ("Native") and
/// `match_window` resolve to at connect, as the desktop clients resolve them to the monitor.
///
/// SDL cannot answer this: it reports the app plane as 1080p@60 on a 4K panel. The resolution
/// comes from `getSystemInfo`'s `UHD` flag; an unanswered query reads as 1080p, which streams
/// rather than failing the handshake with a 0×0 request. The rate is 60: the plane rate the
/// compositor reports, and the shipped default before the document moved. 120 stays a pick.
pub fn native_mode() -> punktfunk_core::config::Mode {
    let uhd = json_str_field(system_info(), "UHD");
    let (width, height) = if uhd.as_deref() == Some("true") {
        (3840, 2160)
    } else {
        (1920, 1080)
    };
    tracing::info!(
        "panel: UHD={} — native mode {width}x{height}@60",
        uhd.as_deref().unwrap_or("unknown")
    );
    punktfunk_core::config::Mode {
        width,
        height,
        refresh_hz: 60,
    }
}

/// SoC/board codename (`/etc/prefs/properties/machineName`, e.g. `m16p`, `k5lp`), read only for
/// per-board workarounds (see [`jail_config_broken`]). Never a lookup key for anything
/// beyond that narrow set of on-device-verified quirks — see the module docs.
pub fn machine_name() -> Option<String> {
    static MACHINE_NAME: OnceLock<Option<String>> = OnceLock::new();
    MACHINE_NAME
        .get_or_init(|| {
            std::fs::read_to_string(MACHINE_NAME_FILE)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .clone()
}

/// Whether this machine's jailer config is known-broken for direct-media access: on `k5lp`/`k3lp`,
/// a broken config leaves `/dev/rtkmem` unreadable and NDL v1 cannot reach the decoder.
pub fn jail_config_broken() -> bool {
    static BROKEN: OnceLock<bool> = OnceLock::new();
    *BROKEN.get_or_init(|| {
        let Some(name) = machine_name() else {
            return false;
        };
        if name != "k5lp" && name != "k3lp" {
            return false;
        }
        // SAFETY: a C string literal is valid and NUL-terminated for the duration of the call.
        unsafe { libc::access(RTKMEM_DEVICE.as_ptr(), libc::R_OK) != 0 }
    })
}

/// Refuse to open `backend` when the jailer config is known-broken — the failure is the same for
/// every direct-media backend, and it's clearer here than as a black screen later.
pub fn ensure_jail_ok(backend: &str) -> anyhow::Result<()> {
    if jail_config_broken() {
        bail!(
            "this TV's jailer config leaves /dev/rtkmem unreadable (machine {}), so {backend} \
             cannot reach the decoder",
            machine_name().unwrap_or_else(|| "unknown".into()),
        );
    }
    Ok(())
}

/// Which NDL `DirectMedia` generation to *try*, from the known version ranges (v2 at `>=5`,
/// v1 at `>=3.5,<5`). Unknown defaults to `V2` — today's behaviour,
/// kept for every working device, including ones where Luna is unavailable. No fallback between
/// the two; `ndl`'s module docs say why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NdlGeneration {
    /// webOS 3.5-4.x: `NDL_DirectVideoOpen/SetCallback/SetArea/PlayWithCallback/Close`.
    /// H.264-only, SDR, no PTS input.
    V1,
    /// webOS 5+: `NDL_DirectVideoPlay(buffer, size, pts)` and friends. Today's behaviour.
    V2,
}

pub fn ndl_generation() -> NdlGeneration {
    static GENERATION: OnceLock<NdlGeneration> = OnceLock::new();
    *GENERATION.get_or_init(|| match sdk_version() {
        Some((major, _)) if major < 5 => NdlGeneration::V1,
        _ => NdlGeneration::V2,
    })
}

/// What this device may offer, from [`ndl_generation`]. Published once through
/// `core::caps::install` so the wire, the UI and settings load can't disagree.
pub fn video_caps() -> VideoCaps {
    match ndl_generation() {
        // Decoder-wide, NOT clamped to the Opus plane's stereo: the plane is one of two audio
        // routes and the SDL one carries widths it has no mode for. The plane's own ceiling is a
        // per-ROUTE clamp applied at connect (`core::model::AudioRoutePref::max_channels`) — a
        // global one here silently took 5.1 away from a route that can play it.
        NdlGeneration::V2 => VideoCaps::FULL,
        NdlGeneration::V1 => VideoCaps::H264_SDR,
    }
}

impl DeviceInfo {
    pub fn detect() -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let webos_major = json_str_field(os_info(), "webos_release")
            .and_then(|v| parse_sdk_version(&v))
            .map(|(major, _)| major);
        let model = std::fs::read_to_string(DEVICE_INFO)
            .ok()
            .and_then(|t| json_str_field(&t, "product_id"));
        Self {
            cores,
            webos_major,
            model,
            sdk_version: sdk_version(),
            ota_id: ota_id(),
            machine_name: machine_name(),
        }
    }

    /// Log device details at startup — the primary triage artifact for a report from a model
    /// neither developer owns: which webOS, which `SoC`, which NDL generation, jail check ok.
    pub fn log(&self) {
        tracing::info!(
            "device: cores={} webos={} model={} sdk={} ota={} machine={} ndl_gen={:?} jail_broken={}",
            self.cores,
            self.webos_major
                .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
            self.model.as_deref().unwrap_or("unknown"),
            self.sdk_version
                .map_or_else(|| "unknown".to_string(), |(maj, min)| format!("{maj}.{min}")),
            self.ota_id.as_deref().unwrap_or("unknown"),
            self.machine_name.as_deref().unwrap_or("unknown"),
            ndl_generation(),
            jail_config_broken(),
        );
    }
}

/// Nice value every stream-carrying thread is boosted to. Reached at nice 0, a thread that
/// feeds the decoder or reads a 1 kHz mouse loses the CPU to the vendor's own boosted decode
/// threads for tens of milliseconds at a stretch on this 3-core `SoC`.
pub const HOT_THREAD_NICE: libc::c_int = -10;

/// Renices `tid` (0 = calling thread) to [`HOT_THREAD_NICE`]; `false` if the kernel refused.
///
/// Always best-effort: it needs `CAP_SYS_NICE` or a large enough `RLIMIT_NICE`, present on a
/// rooted install and absent under a plain Dev-Mode SAM jail — see [`unlock_nice_limit`].
pub fn renice(tid: i32) -> bool {
    unlock_nice_limit();
    // SAFETY: plain syscall — tid and priority value only, no pointers.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, tid as libc::id_t, HOT_THREAD_NICE) == 0 }
}

/// Raises `RLIMIT_NICE`'s soft limit to its hard limit, once. Raising the soft limit needs no
/// privilege, so a jail that grants the hard limit alone would otherwise refuse every renice
/// for nothing. Logged either way: the limit is what a session log needs to say why the boost
/// did or did not apply. The kernel reads the limit as `20 - rlim_cur`, so nice -10 needs 30.
fn unlock_nice_limit() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `lim` is a valid, writable rlimit for the duration of the call.
        if unsafe { libc::getrlimit(libc::RLIMIT_NICE, &mut lim) } != 0 {
            return;
        }
        if lim.rlim_cur < lim.rlim_max {
            lim.rlim_cur = lim.rlim_max;
            // SAFETY: `lim` is a valid rlimit this function owns for the duration of the call.
            if unsafe { libc::setrlimit(libc::RLIMIT_NICE, &lim) } != 0 {
                tracing::warn!(
                    "RLIMIT_NICE: raising the soft limit failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        }
        tracing::info!(
            "RLIMIT_NICE soft={} hard={} (nice floor is 20 - soft; the boost asks {HOT_THREAD_NICE})",
            lim.rlim_cur,
            lim.rlim_max,
        );
    });
}

/// Boosts the calling thread. See [`renice`].
pub fn boost_current_thread() {
    let _ = renice(0);
}

/// Process CPU time (user+sys clock ticks, see [`clock_ticks_per_sec`]) and resident
/// memory (bytes), for the stats overlay's CPU/RAM line. Plain `/proc/self` reads.
pub fn process_cpu_mem() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` (field 2) may contain spaces/parens, so split after the last ')'.
    let after_comm = stat.rsplit_once(')')?.1;
    let mut fields = after_comm.split_whitespace();
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;

    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let rss_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64; // SAFETY: no pointers

    Some((utime + stime, rss_pages * page_size))
}

/// Clock ticks per second, for converting [`process_cpu_mem`]'s ticks to seconds.
pub fn clock_ticks_per_sec() -> u64 {
    (unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64).max(1) // SAFETY: no pointers
}
