//! NDL `DirectMedia` **v1** video (`NDL_DirectVideoOpen/SetCallback/SetArea/
//! PlayWithCallback/Close`) — the API webOS 3.5-4.x ships in `libNDL_directmedia.so.1`, where
//! the v2 surface [`super::v2`] uses does not exist.
//!
//! What it cannot do:
//! - **H.264 only, SDR, BT.709** — `NDL_DirectVideoOpen` rejects the rest. HEVC and HDR are
//!   refused *before* the handshake (`core::caps` → `session::connect`); [`NdlV1Video::load`]
//!   refuses again as defence in depth.
//! - **No PTS input** — `PlayWithCallback` takes `(data, size, userdata)` and frames present as
//!   fed, so `session`'s PTS-anchoring and A/V-offset machinery is inert here.
//! - **No render-buffer query, no flush, no HDR call.** The sink tolerates all three.
//! - **Fixed 1920x1080 display rect**, placed once via `SetArea` (see [`fit_video`]) in webOS's
//!   panel-independent app coordinate space, since v1 has no native punch-through sizing. Video
//!   is an underlay the UI composites over, so the app never repositions it.
//!
//! Decode resolution is *not* among the limits: real stream dimensions reach
//! `NDL_DirectVideoOpen` unclamped, so 1440p/4K decode as configured. The ceiling is the
//! silicon's, and v1 offers no way to ask what it is.
//!
//! Audio stays on software Opus → SDL: v1's `NDL_DirectAudio*` is PCM/AAC/AC3 with no Opus.
//!
//! **The M3/KADP patch is deliberately not adopted.** It `mprotect`s a vendor code page RWX and
//! NOPs two bytes out of a `MStar` codec-type whitelist so *non-H.264* types pass. We only ever
//! feed H.264 here, and unverifiable patching of vendor code is what `docs/NOTES.md` argues
//! against.
use std::ffi::c_ulonglong;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{bail, Result};

use crate::core::media::{VideoSink, VideoSinkCaps};

use super::{arm_load, ensure_init, ensure_not_poisoned, ffi, lock_ffi, mark_frame_fed_logged, NdlCodec, PLAYING};
use crate::platform::webos::device;

/// The v1 video plane's fixed size, regardless of panel resolution (see the module docs).
const PLANE_WIDTH: i32 = 1920;
const PLANE_HEIGHT: i32 = 1080;

/// Letterbox/pillarbox the stream's aspect into the fixed plane (once at open).
fn fit_video(fns: &ffi::V1, width: i32, height: i32) {
    let (x, y, w, h) = if width <= 0 || height <= 0 {
        (0, 0, PLANE_WIDTH, PLANE_HEIGHT)
    } else {
        let h = PLANE_WIDTH * height / width;
        if h <= PLANE_HEIGHT {
            (0, (PLANE_HEIGHT - h) / 2, PLANE_WIDTH, h)
        } else {
            let w = PLANE_WIDTH * PLANE_HEIGHT / h;
            ((PLANE_WIDTH - w) / 2, 0, w, PLANE_HEIGHT)
        }
    };
    // Only log on success; SetArea failure leaves the plane where open put it.
    match fns.video_set_area(x, y, w, h) {
        Ok(()) => tracing::info!("NDL v1 plane: {w}x{h}+{x}+{y} for a {width}x{height} stream"),
        Err(e) => tracing::warn!("NDL v1 SetArea({x},{y},{w},{h}): {e:#}"),
    }
}

/// Frame-done callback. Presence signal only; no PTS input to timestamp against.
extern "C" fn on_frame(userdata: c_ulonglong) {
    if PLAYING.bump_first() {
        tracing::info!("NDL v1 pipeline confirmed a frame through (frame {userdata})");
    }
}

/// One open v1 video decode session. Process-global like v2's — NDL has no context handle — so
/// dropping this closes *the* video plane.
pub struct NdlV1Video {
    fns: &'static ffi::V1,
    open_instant: Instant,
    /// Feed counter, handed to NDL as each frame's `userdata` and echoed back by [`on_frame`].
    frames_fed: AtomicU64,
}

impl NdlV1Video {
    /// Open the v1 video plane for a `width`x`height` H.264 stream.
    pub fn load(app_id: &str, width: i32, height: i32, codec: NdlCodec) -> Result<Self> {
        ensure_not_poisoned()?;
        // Guard against non-H.264: HEVC to v1 is black screen, not error.
        if codec != NdlCodec::H264 {
            bail!("NDL v1 decodes H.264 only (asked for {codec:?}) — see platform::webos::ndl::v1");
        }
        device::ensure_jail_ok("NDL v1")?;
        let fns = ffi::v1()?;
        ensure_init(app_id, false)?;
        let mut info = ffi::V1VideoInfo {
            width,
            height,
            source: 0,
        };
        // Re-arm before opening, so the reveal gate can't be satisfied by a previous session.
        arm_load();
        fns.video_open(&mut info)?;
        if let Err(e) = fns.video_set_callback(Some(on_frame)) {
            tracing::warn!("NDL v1 SetCallback failed ({e:#}) — presence signal unavailable");
        }
        fit_video(fns, width, height);
        Ok(Self {
            fns,
            open_instant: Instant::now(),
            frames_fed: AtomicU64::new(0),
        })
    }
}

impl Drop for NdlV1Video {
    fn drop(&mut self) {
        // Re-arm so the reveal gate stops reporting the session being torn down here.
        arm_load();
        let _ffi = lock_ffi();
        // Best-effort: `Drop` can't propagate a failure.
        if let Err(e) = self.fns.video_close() {
            tracing::warn!("{e:#}");
        }
    }
}

/// Feed-only: v1 has no timestamp input, no render-buffer query and no flush (see the module
/// docs), so every stage behaviour that depends on one switches off through [`VideoSinkCaps`].
impl VideoSink for NdlV1Video {
    fn name(&self) -> &'static str {
        "NDL v1"
    }

    fn caps(&self) -> VideoSinkCaps {
        VideoSinkCaps::FEED_ONLY
    }

    /// No PTS: v1 presents frames as they are fed (see the module docs), so the caller's
    /// timestamp has nowhere to go.
    fn feed(&self, au: &[u8], _pts_ns: u64) -> Result<()> {
        let frame = self.frames_fed.fetch_add(1, Ordering::Relaxed);
        let _ffi = lock_ffi();
        self.fns.video_play(au, frame)?;
        // Don't rely on on_frame alone; callback loss would block menu until reveal timeout.
        mark_frame_fed_logged("NDL v1", self.open_instant);
        Ok(())
    }
}
