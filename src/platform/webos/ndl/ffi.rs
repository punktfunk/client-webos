//! The raw NDL surface: C structs, function-pointer tables, and the one `dlopen` behind them.
//! No policy — see [`super`] for why this is `dlopen`'d rather than linked.
use std::ffi::{c_char, c_int, c_longlong, c_uint, c_ulonglong, c_void, CStr};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};

const LIB_NAME: &CStr = c"libNDL_directmedia.so.1";

// --- C structs -------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct VideoInfo {
    pub(super) width: c_int,
    pub(super) height: c_int,
    /// `NDL_VIDEO_TYPE`: 1=H264, 2=H265, 3=VP9.
    pub(super) kind: c_int,
    pub(super) unknown1: c_int,
}

/// NDL audio union (8-byte aligned). Tag 0 = no audio (all-zero).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(super) struct AudioUnion {
    pub(super) bytes: [u8; 32],
}

impl AudioUnion {
    /// The "no audio" arm: a video-only load.
    pub(super) const SILENT: Self = Self { bytes: [0; 32] };
}

/// `NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T` (field-for-field match).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(super) struct AudioOpusInfo {
    /// `NDL_AUDIO_TYPE`: 3 = Opus.
    pub(super) kind: c_int,
    pub(super) unknown1: c_int,
    pub(super) channels: c_int,
    pub(super) unknown2: c_int,
    /// kHz, not Hz.
    pub(super) sample_rate: f64,
    /// Stream header (undocumented, passed null).
    pub(super) stream_header: *const c_char,
    /// The struct's trailing padding, made explicit so it is *initialized*.
    ///
    /// `f64` is 8-aligned on this ABI, so the fields end at 28 and `size_of` rounds to 32 — and
    /// [`Self::to_union`] copies all 32 bytes. Left implicit, those last four go to NDL as
    /// whatever was on the stack: the load is then rejected asynchronously (returning 0, with no
    /// `LOADCOMPLETED` ever) for some values and not others, which is why hardware Opus came up
    /// black only some of the time.
    pub(super) _padding: [u8; 4],
}

/// The union arm is 32 bytes (the header's own `char padding[32]`); the copy below assumes the
/// struct fills it exactly, with no implicit padding left uninitialized.
///
/// Asserted only for the 32-bit-pointer ABI this ships on (armv7 webOS). A 64-bit `stream_header`
/// makes the struct 40 bytes and `_padding` land in the wrong place — the layout would have to be
/// re-derived for such a target, so the assert would be false there for a real reason rather than
/// a portability nit; it is scoped instead of relaxed so a host `cargo check` still builds.
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<AudioOpusInfo>() == 32);

impl AudioOpusInfo {
    /// Pack into the union [`DataInfo`] takes.
    pub(super) fn to_union(self) -> AudioUnion {
        let mut bytes = [0u8; 32];
        // SAFETY: `Self` is `repr(C)` and exactly the union arm's 32 bytes on the shipping target
        // (asserted above), so this copy stays in bounds and every byte of it is initialized. The
        // clamp only matters on a host build, where the struct is wider and unusable anyway.
        unsafe {
            std::ptr::copy_nonoverlapping(
                std::ptr::from_ref(&self).cast::<u8>(),
                bytes.as_mut_ptr(),
                size_of::<Self>().min(bytes.len()),
            );
        }
        AudioUnion { bytes }
    }
}

#[repr(C)]
pub(super) struct DataInfo {
    pub(super) video: VideoInfo,
    pub(super) audio: AudioUnion,
}

/// Mirrors `NDL_DIRECTVIDEO_HDR_INFO_T` field-for-field — the field names are the
/// H.265 `mastering_display_colour_volume`/`content_light_level_info` SEI syntax
/// element names verbatim, so punktfunk's own `HdrMeta` (same SEI-derived fields,
/// same units) copies straight across with no unit conversion.
/// Fifteen `unsigned int` per `webosbrew/webos-userland@00a5f87`, which also dropped the
/// trailing `reserved[32]` an older header revision declared. The padding is kept anyway: this
/// struct is passed BY VALUE, so a set built against the older layout would read 32 bytes past
/// our argument copy. Carrying it is safe under both revisions; dropping it only under the new
/// one, and which revision a given TV's `libndl-directmedia` was built against isn't knowable
/// from here.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct HdrInfo {
    pub(super) display_primaries_x0: c_uint,
    pub(super) display_primaries_y0: c_uint,
    pub(super) display_primaries_x1: c_uint,
    pub(super) display_primaries_y1: c_uint,
    pub(super) display_primaries_x2: c_uint,
    pub(super) display_primaries_y2: c_uint,
    pub(super) white_point_x: c_uint,
    pub(super) white_point_y: c_uint,
    pub(super) max_display_mastering_luminance: c_uint,
    pub(super) min_display_mastering_luminance: c_uint,
    pub(super) max_content_light_level: c_uint,
    pub(super) max_pic_average_light_level: c_uint,
    pub(super) transfer_characteristics: c_uint,
    pub(super) color_primaries: c_uint,
    pub(super) matrix_coeffs: c_uint,
    pub(super) reserved: [u8; 32],
}

/// `NDL_DIRECTVIDEO_DATA_INFO_T` (v1). `source` (`NDL_DIRECTVIDEO_SRC_TYPE`) is always `NONE`
/// (0) but must be declared: `NDL_DirectVideoOpen` reads all 12 bytes, so omitting it hands the
/// library four bytes of stack garbage.
#[repr(C)]
pub(super) struct V1VideoInfo {
    pub(super) width: c_int,
    pub(super) height: c_int,
    pub(super) source: c_int,
}

// --- Callback and function types -------------------------------------------------------------

/// `NDL_DirectMediaInit`'s resource-released callback. Always `None` here.
pub(super) type ResourceReleased = Option<extern "C" fn(*const c_char)>;
/// v2's load-state callback: `(state, num, str)`.
pub(super) type LoadStateCallback = Option<extern "C" fn(c_int, c_longlong, *const c_char)>;
/// v1's frame-done callback: the `userdata` its feed was given, echoed back.
pub(super) type FrameCallback = Option<extern "C" fn(c_ulonglong)>;

/// `NDL_DirectMediaInit`'s two prototypes: API 1 takes the resource-released callback, API 2 the
/// app id alone (`webosbrew/webos-userland@00a5f87`). One symbol, resolved once, typed twice.
pub(super) type InitV1 = unsafe extern "C" fn(*const c_char, ResourceReleased) -> c_int;
pub(super) type InitV2 = unsafe extern "C" fn(*const c_char) -> c_int;

// --- The safe surface ------------------------------------------------------------------------
//
// Every `unsafe` in this module is below, and nowhere else in `ndl`: the tables' function pointers
// are private, so the only way to reach NDL is through these methods. They own the pointer/length
// derivation and the `0 = ok` convention; the modules above own the policy.
//
// **They do NOT take `FFI_LOCK`.** Serializing NDL is [`super::lock_ffi`]'s job and several callers
// deliberately hold that guard across a whole burst of calls, which a lock taken in here would
// deadlock on.

/// NDL's convention: `0` is success and anything else a failure whose detail sits in the library's
/// own last-error buffer. One place builds that message, so no call site repeats it.
fn check(call: &str, ret: c_int) -> Result<()> {
    if ret == 0 {
        return Ok(());
    }
    bail!("{call} failed: ret={ret} error={}", last_error())
}

/// The three calls both generations share.
pub(super) struct Common {
    get_error: unsafe extern "C" fn() -> *const c_char,
    init_v1: InitV1,
    init_v2: InitV2,
    quit: unsafe extern "C" fn() -> c_int,
}

impl Common {
    /// `NDL_DirectMediaInit`. One symbol, two prototypes: `api2` picks the app-id-only form v2
    /// declares, against v1's app id plus resource-released callback (always NULL here).
    pub(super) fn init(&self, app_id: &CStr, api2: bool) -> Result<()> {
        // SAFETY: `app_id` is NUL-terminated and valid for the duration of the call.
        let ret = unsafe {
            if api2 {
                (self.init_v2)(app_id.as_ptr())
            } else {
                (self.init_v1)(app_id.as_ptr(), None)
            }
        };
        check("NDL_DirectMediaInit", ret)
    }

    /// Process-wide teardown. Best-effort: there is no recovery from a failure on the way out.
    pub(super) fn quit(&self) {
        // SAFETY: no arguments.
        unsafe { (self.quit)() };
    }
}

/// webOS 5+ `DirectMedia` v2. Every symbol is required: a partial table means an unknown
/// flavour of the library, and guessing which calls are safe on it is worse than refusing.
pub(super) struct V2 {
    load: unsafe extern "C" fn(*mut DataInfo, LoadStateCallback) -> c_int,
    unload: unsafe extern "C" fn() -> c_int,
    video_play: unsafe extern "C" fn(*mut c_void, c_uint, c_longlong) -> c_int,
    flush_render_buffer: unsafe extern "C" fn() -> c_int,
    get_render_buffer_length: unsafe extern "C" fn(*mut c_int) -> c_int,
    audio_play: unsafe extern "C" fn(*mut c_void, c_uint, c_longlong) -> c_int,
    set_hdr_info: unsafe extern "C" fn(HdrInfo) -> c_int,
}

impl V2 {
    /// `NDL_DirectMediaLoad`. Success here is only "request accepted" — see [`super::v2`] for what
    /// still has to be waited out.
    pub(super) fn load(&self, info: &mut DataInfo, on_state: LoadStateCallback) -> Result<()> {
        // SAFETY: `info` is a live, fully initialized `DataInfo` for the duration of the call, and
        // `on_state` an `extern "C"` fn with no captured state.
        let ret = unsafe { (self.load)(info, on_state) };
        check("NDL_DirectMediaLoad", ret)
    }

    /// Best-effort teardown: every caller is either in `Drop` or already unwinding an error it
    /// cannot improve on.
    pub(super) fn unload(&self) {
        // SAFETY: no arguments.
        unsafe { (self.unload)() };
    }

    /// Feed one access unit (or one piece of one) at `pts_ms` in the decoder's own clock domain.
    pub(super) fn video_play(&self, au: &[u8], pts_ms: i64) -> Result<()> {
        check("NDL_DirectVideoPlay", self.play(self.video_play, au, pts_ms))
    }

    /// Feed one Opus packet to the audio plane at `pts_ms`.
    pub(super) fn audio_play(&self, packet: &[u8], pts_ms: i64) -> Result<()> {
        check("NDL_DirectAudioPlay", self.play(self.audio_play, packet, pts_ms))
    }

    /// The shape both feeds share: a borrowed buffer NDL reads synchronously, and a stamp.
    fn play(
        &self,
        feed: unsafe extern "C" fn(*mut c_void, c_uint, c_longlong) -> c_int,
        data: &[u8],
        pts_ms: i64,
    ) -> c_int {
        // The `*mut` in the prototype is NDL's declaration, not a licence to write: both feeds are
        // reads. `data.len()` is what bounds the read, so the cast cannot narrow it.
        let len = c_uint::try_from(data.len()).unwrap_or(c_uint::MAX);
        // SAFETY: NDL reads `len` bytes synchronously and does not retain the pointer, so the
        // borrow outlives the call.
        unsafe { feed(data.as_ptr().cast_mut().cast::<c_void>(), len, pts_ms as c_longlong) }
    }

    /// Drop whatever the decoder has queued for presentation.
    pub(super) fn flush_render_buffer(&self) -> Result<()> {
        // SAFETY: no arguments.
        let ret = unsafe { (self.flush_render_buffer)() };
        check("NDL_DirectVideoFlushRenderBuffer", ret)
    }

    /// Frames buffered but not yet displayed, or `None` if the query failed — which is not the same
    /// answer as an empty queue, and the stage above treats it differently.
    pub(super) fn render_buffer_length(&self) -> Option<c_int> {
        let mut length: c_int = 0;
        // SAFETY: `length` is a live, correctly typed out-parameter for the duration of the call.
        let ret = unsafe { (self.get_render_buffer_length)(&raw mut length) };
        (ret == 0).then_some(length)
    }

    /// Hand the panel HDR mastering metadata. Passed **by value**; see [`HdrInfo`] on why the
    /// trailing padding is carried.
    pub(super) fn set_hdr_info(&self, info: HdrInfo) -> Result<()> {
        // SAFETY: passed by value; no pointers and no aliasing.
        let ret = unsafe { (self.set_hdr_info)(info) };
        check("NDL_DirectVideoSetHDRInfo", ret)
    }
}

/// webOS 3.5-4.x `DirectMedia` v1 — see [`super::v1`].
pub(super) struct V1 {
    video_open: unsafe extern "C" fn(*mut V1VideoInfo) -> c_int,
    video_close: unsafe extern "C" fn() -> c_int,
    video_set_area: unsafe extern "C" fn(c_int, c_int, c_int, c_int) -> c_int,
    video_set_callback: unsafe extern "C" fn(FrameCallback) -> c_int,
    video_play_with_callback: unsafe extern "C" fn(*mut c_void, usize, c_ulonglong) -> c_int,
}

impl V1 {
    /// Open the video plane for a stream of `info`'s dimensions.
    pub(super) fn video_open(&self, info: &mut V1VideoInfo) -> Result<()> {
        // SAFETY: `info` is a live, fully initialized `V1VideoInfo`; NDL copies what it needs.
        let ret = unsafe { (self.video_open)(info) };
        check("NDL_DirectVideoOpen", ret)
    }

    /// Best-effort teardown — the only caller is `Drop`, which cannot propagate a failure.
    pub(super) fn video_close(&self) -> Result<()> {
        // SAFETY: no arguments.
        let ret = unsafe { (self.video_close)() };
        check("NDL_DirectVideoClose", ret)
    }

    /// Place the fixed display rect (see [`super::v1`]'s `fit_video`).
    pub(super) fn video_set_area(&self, x: c_int, y: c_int, w: c_int, h: c_int) -> Result<()> {
        // SAFETY: plain integer arguments.
        let ret = unsafe { (self.video_set_area)(x, y, w, h) };
        check("NDL_DirectVideoSetArea", ret)
    }

    /// Register the frame-done callback.
    pub(super) fn video_set_callback(&self, cb: FrameCallback) -> Result<()> {
        // SAFETY: `cb` is an `extern "C"` fn with no captured state.
        let ret = unsafe { (self.video_set_callback)(cb) };
        check("NDL_DirectVideoSetCallback", ret)
    }

    /// Feed one access unit. No timestamp: v1 presents frames as they are fed. `userdata` is
    /// echoed back by the frame-done callback.
    pub(super) fn video_play(&self, au: &[u8], userdata: u64) -> Result<()> {
        // SAFETY: NDL reads `au.len()` bytes synchronously and does not retain the pointer. The
        // `*mut` is its declaration, not a write: this feed is a read.
        let ret = unsafe {
            (self.video_play_with_callback)(
                au.as_ptr().cast_mut().cast::<c_void>(),
                au.len(),
                userdata as c_ulonglong,
            )
        };
        check("NDL_DirectVideoPlayWithCallback", ret)
    }
}

// --- Resolution ------------------------------------------------------------------------------

/// An open handle to [`LIB_NAME`]. Never closed — the resolved function pointers outlive it by
/// design, as a `DT_NEEDED` load would have.
struct Lib(*mut c_void);

impl Lib {
    /// `RTLD_GLOBAL` matches what a `DT_NEEDED` load would have given the process.
    fn open() -> Result<Self> {
        // SAFETY: `LIB_NAME` is a NUL-terminated literal.
        let handle = unsafe { libc::dlopen(LIB_NAME.as_ptr(), libc::RTLD_LAZY | libc::RTLD_GLOBAL) };
        if handle.is_null() {
            bail!("dlopen({LIB_NAME:?}) failed — NDL DirectMedia is not available on this device");
        }
        Ok(Self(handle))
    }

    /// One symbol, or an error naming it — *which* symbol is missing is what says which
    /// generation this device has. `T` must be a function-pointer type.
    fn sym<T: Sized>(&self, name: &CStr) -> Result<T> {
        // SAFETY: `self.0` is a live `dlopen` handle and `name` NUL-terminated.
        let ptr = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        if ptr.is_null() {
            bail!("{LIB_NAME:?} is missing symbol {name:?}");
        }
        debug_assert_eq!(size_of::<T>(), size_of::<*mut c_void>(), "T must be a function pointer");
        // SAFETY: `T` is a function-pointer type and `ptr` is non-null and dlsym-verified.
        Ok(unsafe { std::mem::transmute_copy(&ptr) })
    }
}

/// Resolve a table once, caching the outcome **including the failure**: a symbol missing from
/// this device's library won't appear on a retry. Text rather than `anyhow::Error` because
/// errors aren't `Clone` and callers only print it.
fn cached<T: 'static>(
    cache: &'static OnceLock<std::result::Result<T, String>>,
    build: impl FnOnce(&Lib) -> Result<T>,
) -> Result<&'static T> {
    cache
        .get_or_init(|| Lib::open().and_then(|lib| build(&lib)).map_err(|e| format!("{e:#}")))
        .as_ref()
        .map_err(|e| anyhow::Error::msg(e.clone()))
        .context("NDL DirectMedia")
}

pub(super) fn common() -> Result<&'static Common> {
    static CACHE: OnceLock<std::result::Result<Common, String>> = OnceLock::new();
    cached(&CACHE, |lib| {
        let init_v1: InitV1 = lib.sym(c"NDL_DirectMediaInit")?;
        Ok(Common {
            get_error: lib.sym(c"NDL_DirectMediaGetError")?,
            init_v1,
            // SAFETY: the same address, retyped to the prototype API 2 declares for it.
            init_v2: unsafe { std::mem::transmute::<InitV1, InitV2>(init_v1) },
            quit: lib.sym(c"NDL_DirectMediaQuit")?,
        })
    })
}

pub(super) fn v2() -> Result<&'static V2> {
    static CACHE: OnceLock<std::result::Result<V2, String>> = OnceLock::new();
    cached(&CACHE, |lib| {
        Ok(V2 {
            load: lib.sym(c"NDL_DirectMediaLoad")?,
            unload: lib.sym(c"NDL_DirectMediaUnload")?,
            video_play: lib.sym(c"NDL_DirectVideoPlay")?,
            flush_render_buffer: lib.sym(c"NDL_DirectVideoFlushRenderBuffer")?,
            get_render_buffer_length: lib.sym(c"NDL_DirectVideoGetRenderBufferLength")?,
            audio_play: lib.sym(c"NDL_DirectAudioPlay")?,
            set_hdr_info: lib.sym(c"NDL_DirectVideoSetHDRInfo")?,
        })
    })
}

pub(super) fn v1() -> Result<&'static V1> {
    static CACHE: OnceLock<std::result::Result<V1, String>> = OnceLock::new();
    cached(&CACHE, |lib| {
        Ok(V1 {
            video_open: lib.sym(c"NDL_DirectVideoOpen")?,
            video_close: lib.sym(c"NDL_DirectVideoClose")?,
            video_set_area: lib.sym(c"NDL_DirectVideoSetArea")?,
            video_set_callback: lib.sym(c"NDL_DirectVideoSetCallback")?,
            video_play_with_callback: lib.sym(c"NDL_DirectVideoPlayWithCallback")?,
        })
    })
}

/// What `NDL_DirectAudioSupportMultiChannel` says about where the sound is currently going.
///
/// Two conditions, not one, which is why it is a four-way answer: the output path has to be
/// capable AND the TV's Sound Out has to be configured to use it. TV speakers are a 2.0/2.2 array
/// and ARC/optical carry 2-channel PCM only, so those report [`Self::OutputNotPassthrough`] at
/// best — 5.1 fed there is folded down inside the TV.
///
/// A transient setting, and the query is meaningful only once `NDL_DirectMediaInit` has run. It
/// describes NDL's own PCM path, so it is logged per session and gates nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MultiChannelPcm {
    /// No multi-channel support here.
    Unsupported,
    /// Supported, but nothing connected takes multi-channel PCM.
    NoDeviceConnected,
    /// The connected device takes it, but Digital Sound Out is not passthrough.
    OutputNotPassthrough,
    /// Current settings do support it: real 5.1 reaches the sink.
    Supported,
    /// The symbol is absent, the call failed, or it answered with a code the header doesn't
    /// document.
    Unknown,
}

/// Queries where multi-channel PCM currently goes.
///
/// ⚠ `int NDL_DirectAudioSupportMultiChannel(int *isSupported)` — the support code is an OUT
/// PARAMETER and the return is 0/-1. The `NDLMultiChannelPCMCallback` codes the header documents
/// alongside it are the same ladder shifted down by one; do not read one as the other.
pub(super) fn multichannel_pcm_status() -> MultiChannelPcm {
    type Query = unsafe extern "C" fn(*mut c_int) -> c_int;
    let Some(ptr) = optional_sym(c"NDL_DirectAudioSupportMultiChannel") else {
        return MultiChannelPcm::Unknown;
    };
    // SAFETY: `ptr` is a dlsym-verified address for the prototype above.
    let query: Query = unsafe { std::mem::transmute_copy(&ptr) };
    let mut raw: c_int = -1;
    // SAFETY: `raw` is a live, correctly typed out-parameter for the duration of the call.
    if unsafe { query(&raw mut raw) } != 0 {
        return MultiChannelPcm::Unknown;
    }
    match raw {
        0 => MultiChannelPcm::Unsupported,
        1 => MultiChannelPcm::NoDeviceConnected,
        2 => MultiChannelPcm::OutputNotPassthrough,
        3 => MultiChannelPcm::Supported,
        _ => MultiChannelPcm::Unknown,
    }
}

/// One symbol that only some firmware has, or `None`.
///
/// Resolved through `RTLD_DEFAULT` (the library is opened `RTLD_GLOBAL`) — but only after
/// [`common`] has forced that `dlopen`. Without it every optional symbol reads as absent on any
/// path that runs before the first real NDL call, which is exactly where the capability probes
/// run.
fn optional_sym(name: &CStr) -> Option<*mut c_void> {
    common().ok()?;
    // SAFETY: `name` is a NUL-terminated literal; the result is checked for null.
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    (!ptr.is_null()).then_some(ptr)
}

/// NDL's last error string (set on the most recent failing call). Messages only, so an
/// unresolved library answers with a placeholder rather than an error of its own.
pub(super) fn last_error() -> String {
    let Ok(fns) = common() else {
        return "(NDL not loaded)".to_string();
    };
    // SAFETY: returns a pointer to NDL's internal buffer; only borrowed here.
    let p = unsafe { (fns.get_error)() };
    if p.is_null() {
        return "(no NDL error message)".to_string();
    }
    // SAFETY: non-null NUL-terminated string owned by NDL; copied out before returning.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
