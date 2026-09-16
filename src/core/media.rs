//! The media pipeline's vocabulary: what a decode backend must offer, and what a stage above it
//! is allowed to assume.
//!
//! Two backends decode video here (NDL v2, NDL v1) and two routes carry audio, and the
//! pipeline in `session` is written against these traits rather than against any of them. In
//! `core` for the layering reason every shared vocabulary is: `platform::webos` implements it and
//! `session` consumes it, so it can live in neither.
//!
//! **The traits describe capability, not policy.** A sink says what it can take
//! ([`VideoSinkCaps`]) and does it; anchoring, freeze-until-reanchor, backlog metering and
//! concealment are the stages' business, above this seam and identical on every backend.
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use punktfunk_core::quic;

/// Feed refused: pipeline loading. Distinct from decode error; flushing an unloaded decoder
/// kills the audio plane (see `session::stage`).
#[derive(Debug)]
pub struct NotReady;

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("decode pipeline not loaded yet — holding")
    }
}

impl std::error::Error for NotReady {}

/// Video backend capabilities. Every `false` disables a stage behaviour; avoids per-backend
/// matches scattered across the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoSinkCaps {
    /// Feed carries a presentation timestamp (`false` on NDL v1).
    pub pts: bool,
    /// Slice-progressive delivery. Requires repeatable PTS, implies [`Self::pts`].
    pub partial_au: bool,
    /// Decoder has a droppable render queue.
    pub flush: bool,
}

impl VideoSinkCaps {
    /// Narrowest backend: bare feed (NDL v1).
    pub const FEED_ONLY: Self = Self {
        pts: false,
        partial_au: false,
        flush: false,
    };
}

/// Monotonic clock for presentation pacing. NDL's is unrelated to host capture or wall-clock;
/// mapping is `session::timeline`'s job.
pub trait MediaClock: Send + Sync {
    /// Nanoseconds since the decoder loaded.
    fn now_ns(&self) -> u64;
}

/// One loaded video decoder.
pub trait VideoSink: Send {
    fn name(&self) -> &'static str;

    fn caps(&self) -> VideoSinkCaps;

    /// Feed one AU (or one piece). PTS is in this sink's clock; ignored if caps deny timestamps.
    fn feed(&self, au: &[u8], pts_ns: u64) -> Result<()>;

    /// Drop the render queue. No-op where [`VideoSinkCaps::flush`] is false.
    fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Queued frames, or `None` if backend can't report. `None` ≠ empty; stages treat differently.
    fn queue_depth(&self) -> Option<u32> {
        None
    }

    /// Negotiated colorimetry, plus HDR mastering metadata where the session applies it.
    fn set_color(&self, _meta: Option<&quic::HdrMeta>, _color: quic::ColorInfo) -> Result<()> {
        Ok(())
    }

    fn clock(&self) -> Option<&dyn MediaClock> {
        None
    }

    /// Audio plane for this load (also an [`AudioSink`]; route needs nothing else).
    fn audio_plane(&self) -> Option<Arc<dyn AudioPlane>> {
        None
    }

    /// Decoder failed unrecoverably (no re-anchor works). Ends session if true.
    fn is_dead(&self) -> bool {
        false
    }
}

/// Audio format a sink declares. Stage produces exactly this; no conversion after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    /// Opus the sink decodes: the wire's stereo as-is, or 5.1 re-encoded into NDL's layout.
    Opus { channels: u8 },
    /// Interleaved f32 in punktfunk's channel order (libopus output). No conversion needed.
    PcmF32 { channels: u8, sample_rate: u32 },
}

impl AudioFormat {
    /// Channels to speaker. Never folded (route is pre-negotiated to match). Mismatch is a bug.
    pub fn channels(self) -> u8 {
        match self {
            Self::Opus { channels } | Self::PcmF32 { channels, .. } => channels,
        }
    }
}

/// One packet on its way to a sink, in whatever shape that sink declared.
pub enum Samples<'a> {
    Opus(&'a [u8]),
    F32(&'a [f32]),
}

/// Somewhere a session's audio can go. Two exist here — the TV's SDL device and NDL's Opus plane
/// — and `session::audio`'s stage is written against this rather than against either of them, so
/// adding a third is one implementation and no pipeline change.
pub trait AudioSink: Send + Sync {
    fn name(&self) -> &'static str;

    fn format(&self) -> AudioFormat;

    /// Feed packet stamped in host capture clock. Timeline sinks map it; others ignore it.
    fn feed(&self, samples: Samples<'_>, host_pts_ns: u64) -> Result<()>;

    /// Queue depth in ms, or `None` if sink can't report.
    fn depth_ms(&self) -> Option<i64> {
        None
    }
}

/// Hardware audio plane for a video load (NDL, in practice).
///
/// The picture depends on it: NDL paces only when the plane exists and is primed
/// (docs/NOTES.md § "NDL's audio plane"). Does not need continuous feeding after prime.
pub trait AudioPlane: AudioSink {
    /// Lead of plane stamps over its clock (ms). Negative means plane is starved; on software
    /// route this is normal, not a fault.
    fn lead_ms(&self) -> i64;

    /// Run plane's thread until `stop`. Carry load prime until load confirms; keep checks off the
    /// feed path. Blocks; caller provides thread. `yields_to_real`: metronome yields to real stream.
    fn run_keepalive(&self, stop: &AtomicBool, yields_to_real: bool);

    /// Hold extra queue depth (ms) to pace picture against presentation cushion. FIXED per session;
    /// monotonic stamps can't give depth back once taken. No-op if plane has no lead to move.
    fn set_extra_lead_ms(&self, _ms: i64) {}

    /// Whether real audio may ride this plane (vs. keepalive only).
    ///
    /// Plane may exist unproven: backend accepts but confirms later or never. Feeding costs nothing
    /// if it fails, but routing real audio to an unproven plane silences the session. Conservative
    /// verdict: separate questions.
    fn accepts_stream(&self) -> bool {
        true
    }
}
