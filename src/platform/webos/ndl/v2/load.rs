//! Loading an NDL v2 pipeline: the `NDL_DirectMediaLoad` attempt and its audio probe, the prime
//! that carries the plane through the load wait, the feed gate that ends it, and the HDR metadata
//! held back until the pipeline is ingesting.

use std::ffi::c_uint;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::core::media::NotReady;

use super::super::{
    arm_load, ensure_init, ensure_not_poisoned, ffi, lock_ffi, settle_before_retry, wait_load_completed, NdlCodec,
    AUDIO_PRIME_BUDGET, AUDIO_PROVE_BUDGET, LOAD_COMPLETED, LOAD_COMPLETE_TIMEOUT,
};
use super::plane::{plane_config, silence, PRIME_LEAD, PRIME_PACKET_MS, PRIME_RETRY};
use super::NdlVideo;

/// How long past the `NDL_DirectMediaLoad` CALL [`NdlVideo::ensure_loaded`] holds frames while
/// `LOADCOMPLETED` is missing.
///
/// Measured from `load_instant`, which is the load call itself, so this window OVERLAPS the load's
/// own wait rather than stacking on it: a unit whose callback never comes would otherwise eat the
/// load budget and then this one before the first frame.
///
/// **A backstop, not a live path** for a set that confirms during the load — `load()` has already
/// spent a longer budget by the time a frame gets here. It IS the live path on a set that reports
/// `LOADCOMPLETED` only once a frame has been fed, where holding forever is the deadlock: nothing
/// confirms until something feeds.
const FEED_ANYWAY_AFTER: Duration = Duration::from_millis(300);

// Const assert because debug assertions don't run on release (CI and TV use release builds).
// Every budget a load can be issued with, since the hold must overlap the wait rather than stack
// on it whichever one the session picked (`session::pipeline::plane_budget`).
const _: () = assert!(
    FEED_ANYWAY_AFTER.as_millis() < LOAD_COMPLETE_TIMEOUT.as_millis()
        && FEED_ANYWAY_AFTER.as_millis() < AUDIO_PRIME_BUDGET.as_millis()
        && FEED_ANYWAY_AFTER.as_millis() < AUDIO_PROVE_BUDGET.as_millis()
);

impl NdlVideo {
    /// Load NDL video stream. Calls `NDL_DirectMediaInit` on first use.
    ///
    /// `audio` is the plane's prime budget, or `None` for a video-only load; `plane_channels` is
    /// its width, 2 or 6. The budget is how long the load waits for `LOADCOMPLETED` before
    /// starting the stream unconfirmed — see [`super::super::AUDIO_PRIME_BUDGET`] and
    /// [`super::super::AUDIO_PROVE_BUDGET`]. The audio request itself is a probe: it fails silently on
    /// unsupported models, which retries video-only.
    pub fn load(
        app_id: &str,
        width: i32,
        height: i32,
        codec: NdlCodec,
        audio: Option<Duration>,
        plane_channels: u8,
    ) -> Result<Self> {
        ensure_not_poisoned()?;
        let fns = ffi::v2()?;
        ensure_init(app_id, true)?;
        let video = ffi::VideoInfo {
            width,
            height,
            kind: codec.ndl_type(),
            unknown1: 0,
        };
        if let Some(budget) = audio {
            // Request is accepted iff `ret == 0`; not judged until first frame fed (some models
            // don't report LOADCOMPLETED earlier, #188). So unconfirmed loads are normal; the
            // metronome rides the plane through the wait. Real audio only rides confirmed
            // ([`Self::plane_proven`]).
            //
            // Hard `Err` is still answered here: no handle to defer with. Unload before retry
            // (failed load may hold decoder resources); snapshot precedes attempt, so settle
            // can't wait out an already-spent UNLOADCOMPLETED.
            let unloads_before = super::super::unload_count();
            match Self::try_load(fns, video, Some(budget), plane_channels) {
                Ok(loaded) => return Ok(loaded),
                // WARN: video-only load streams unpaced (#188), every symptom misattributed.
                Err(e) => tracing::warn!(
                    "NDL audio-enabled load failed ({e:#}) — retrying video-only, the picture will not be paced"
                ),
            }
            fns.unload();
            // Rejected load's callbacks indistinguishable from retry's; let them land first.
            settle_before_retry(unloads_before);
        }
        Self::try_load(fns, video, None, plane_channels)
    }

    /// One `NDL_DirectMediaLoad` attempt, given its budget to report `LOADCOMPLETED` — priming
    /// the audio plane through the wait when the load asked for one (see [`Self::prime_audio`]).
    ///
    /// An audio-enabled attempt that does not confirm is still returned: unconfirmed is a normal
    /// state for one, not a failure (see [`Self::load`]).
    fn try_load(
        fns: &'static ffi::V2,
        video: ffi::VideoInfo,
        audio: Option<Duration>,
        plane_channels: u8,
    ) -> Result<Self> {
        let mut info = ffi::DataInfo {
            video,
            audio: if audio.is_some() {
                plane_config(plane_channels)
            } else {
                ffi::AudioUnion::SILENT
            },
        };
        arm_load();
        // Call time is origin of NDL's PTS domain; see [`Self::load_instant`].
        let load_instant = Instant::now();
        fns.load(&mut info, Some(super::super::on_load_state))?;
        // ret == 0 is "request accepted", not "pipeline ready". Audio-enabled loads wait for
        // plane confirmation and prime with silence; video-only waits for first feed.
        let (primed_pts_ms, confirmed) = match audio {
            Some(budget) => Self::prime_audio(fns, load_instant, budget, silence(plane_channels)),
            None => (0, wait_load_completed()),
        };
        // FATAL is a hard error, not "unconfirmed". Unconfirmed is the only allowed state for
        // audio-enabled loads (keeps the request a PROBE). Hard error here triggers retry
        // video-only in load().
        if audio.is_some() && super::super::fatal() {
            bail!("NDL reported a fatal state during the audio-enabled load");
        }
        Ok(Self {
            fns,
            load_instant,
            audio: audio.is_some(),
            refused: Default::default(),
            last_audio_pts_ms: AtomicI64::new(primed_pts_ms),
            extra_lead_ms: AtomicI64::new(0),
            feed_unblocked: AtomicBool::new(confirmed),
            plane_check_ms: AtomicI64::new(i64::MAX),
            plane_proven: audio.is_some() && confirmed,
            plane_channels,
            pending_hdr: Mutex::new(None),
            applied_hdr: Mutex::new(None),
        })
    }

    /// Feed silence packets until `LOADCOMPLETED`, bounded by budget. Not confirming is normal
    /// (some sets report callback only after first video frame). Unconfirmed loads continue via
    /// [`Self::run_clock_plane`].
    ///
    /// `load_instant` from caller: budget includes the `NDL_DirectMediaLoad` call itself, so
    /// sets blocking inside it don't spend twice. Stamps are in player-clock domain relative to
    /// `load_instant`.
    ///
    /// Feeds silence to break the black-picture-with-sound deadlock: plane won't report until
    /// it receives data, but feeders don't spawn until `session::connect` returns (after this
    /// wait). Bursts only: dropped silently if plane doesn't exist yet.
    ///
    /// Highest stamp → `last_audio_pts_ms` as floor (rewind would mute; see [`Self::play_audio`]).
    /// Ceiling locked at [`PRIME_LEAD`] packets above clock, same lead real audio targets.
    fn prime_audio(fns: &'static ffi::V2, load_instant: Instant, budget: Duration, silence: &[u8]) -> (i64, bool) {
        let mut pts_ms = 0;
        while !LOAD_COMPLETED.fired() {
            // Fatal is unchanging; budget spent only while load is plausibly coming.
            if super::super::fatal() {
                tracing::warn!("NDL load reported a fatal state after {pts_ms}ms of silence");
                return (pts_ms, false);
            }
            if load_instant.elapsed() >= budget {
                // INFO: normal on sets reporting callback against video ingest; metronome carries plane from here.
                tracing::info!(
                    "NDL load: no LOADCOMPLETED within {budget:?} of priming {pts_ms}ms of silence \
                     — starting the stream, the clock plane carries the prime until it lands"
                );
                return (pts_ms, false);
            }
            // Ceiling at PRIME_LEAD packets ahead of clock; real audio pinned to this floor.
            let target_ms = load_instant.elapsed().as_millis() as i64 + PRIME_LEAD * PRIME_PACKET_MS;
            {
                let _ffi = lock_ffi();
                while pts_ms < target_ms {
                    if let Err(e) = fns.audio_play(silence, pts_ms) {
                        tracing::warn!("NDL audio prime rejected at {pts_ms}ms: {e:#}");
                        return (pts_ms, LOAD_COMPLETED.fired());
                    }
                    pts_ms += PRIME_PACKET_MS;
                }
            }
            super::super::poll_until(PRIME_RETRY, || LOAD_COMPLETED.fired());
        }
        tracing::info!(
            "NDL audio prime: LOADCOMPLETED after {:?} ({pts_ms}ms of silence)",
            load_instant.elapsed()
        );
        (pts_ms, true)
    }
    /// `Err` until `LOADCOMPLETED`. Sink flushes, holds, and requests keyframe (right for late
    /// load). Bounded by [`FEED_ANYWAY_AFTER`] to unblock streams that never confirm.
    pub(super) fn ensure_loaded(&self) -> Result<()> {
        if self.feed_unblocked.load(Ordering::Relaxed) {
            return Ok(());
        }
        let elapsed = self.load_instant.elapsed();
        if LOAD_COMPLETED.fired() {
            tracing::info!("NDL LOADCOMPLETED landed {elapsed:?} after load");
        } else if elapsed >= FEED_ANYWAY_AFTER {
            // load() has already spent a load budget by now.
            tracing::warn!("NDL: still no LOADCOMPLETED {elapsed:?} after the load — feeding anyway");
        } else {
            return Err(NotReady.into());
        }
        self.feed_unblocked.store(true, Ordering::Relaxed);
        Ok(())
    }

    fn pending_hdr(&self) -> MutexGuard<'_, Option<ffi::HdrInfo>> {
        self.pending_hdr.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Apply metadata held back for the load, on the edge where the first frame was accepted.
    /// A colour failure only logs: the caller reads an `Err` out of [`Self::play`] as a decode
    /// error and answers it with a flush and a keyframe request.
    pub(super) fn replay_pending_hdr(&self) {
        // Lock held across the apply. Racing `set_color_info` must not drain a racing store,
        // and released lock lets newer value apply first, after which stale overwrites it.
        // `applied_hdr` won't dedupe a value that genuinely differs.
        let mut pending = self.pending_hdr();
        if let Some(info) = pending.take() {
            tracing::info!("NDL: applying HDR metadata held until the first accepted frame");
            if let Err(e) = self.apply_hdr_info(info) {
                tracing::warn!("NDL: applying held HDR metadata failed: {e:#}");
            }
        }
    }

    /// Apply metadata, unless it is what the panel is already on — see [`Self::applied_hdr`].
    /// The slot is held across the FFI call so two threads can't both decide the value is new.
    fn apply_hdr_info(&self, info: ffi::HdrInfo) -> Result<()> {
        let mut applied = self.applied_hdr.lock().unwrap_or_else(PoisonError::into_inner);
        if *applied == Some(info) {
            return Ok(());
        }
        self.set_hdr_info(info)?;
        *applied = Some(info);
        Ok(())
    }

    fn set_hdr_info(&self, info: ffi::HdrInfo) -> Result<()> {
        let _ffi = lock_ffi();
        self.fns.set_hdr_info(info)
    }
    /// Apply HDR mastering metadata (G/B/R order per ST.2086, no conversion needed).
    ///
    /// `meta: None` (SDR stream) is a **no-op**: `NDL_DirectVideoSetHDRInfo` flips panel into
    /// HDR mode on any call, ignoring SDR transfer/primaries. SDR colorimetry rides VUI.
    ///
    /// Deferred until first frame accepted — see [`Self::pending_hdr`].
    pub fn set_color_info(
        &self,
        meta: Option<&punktfunk_core::quic::HdrMeta>,
        color: punktfunk_core::quic::ColorInfo,
    ) -> Result<()> {
        let Some(m) = meta else {
            return Ok(());
        };
        let [g, b, r] = m.display_primaries;
        let info = ffi::HdrInfo {
            display_primaries_x0: c_uint::from(g[0]),
            display_primaries_y0: c_uint::from(g[1]),
            display_primaries_x1: c_uint::from(b[0]),
            display_primaries_y1: c_uint::from(b[1]),
            display_primaries_x2: c_uint::from(r[0]),
            display_primaries_y2: c_uint::from(r[1]),
            white_point_x: c_uint::from(m.white_point[0]),
            white_point_y: c_uint::from(m.white_point[1]),
            max_display_mastering_luminance: m.max_display_mastering_luminance as c_uint,
            min_display_mastering_luminance: m.min_display_mastering_luminance as c_uint,
            max_content_light_level: c_uint::from(m.max_cll),
            max_pic_average_light_level: c_uint::from(m.max_fall),
            transfer_characteristics: c_uint::from(color.transfer),
            color_primaries: c_uint::from(color.primaries),
            matrix_coeffs: c_uint::from(color.matrix),
            reserved: [0; 32],
        };
        // Defer until presenting (held against drain in replay_pending_hdr).
        let mut pending = self.pending_hdr();
        if !super::super::presenting() {
            *pending = Some(info);
            return Ok(());
        }
        // Lock released here; racing replay could apply stale value after this newer one.
        *pending = None;
        self.apply_hdr_info(info)
    }
}
