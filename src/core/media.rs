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

/// A feed refused because the pipeline hasn't finished loading — distinct from a decode error
/// because the response differs: a decode error is answered with a flush, and flushing a decoder
/// that has not finished loading takes the session's audio out for good (see `session::stage`).
#[derive(Debug)]
pub struct NotReady;

impl std::fmt::Display for NotReady {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("decode pipeline not loaded yet — holding")
    }
}

impl std::error::Error for NotReady {}

/// What a video backend can be asked to do. Every `false` here is a stage behaviour that must
/// switch off, not a call that may fail — the alternative is per-backend `match`es scattered
/// across the pipeline, which is what this replaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoSinkCaps {
    /// The feed takes a presentation timestamp. `false` on NDL v1, which presents in feed order.
    pub pts: bool,
    /// Access units may be fed in pieces as they arrive (slice-progressive delivery). Needs a
    /// timestamp that can be repeated across the pieces, so it implies [`Self::pts`].
    pub partial_au: bool,
    /// The decoder holds a render queue that can be dropped on the floor.
    pub flush: bool,
}

impl VideoSinkCaps {
    /// Nothing beyond a feed: the narrowest backend shape (NDL v1).
    pub const FEED_ONLY: Self = Self {
        pts: false,
        partial_au: false,
        flush: false,
    };
}

/// A monotonic clock a sink presents against. NDL has its own, unrelated to the
/// host's capture clock and to wall-clock — mapping between them is `session::timeline`'s job, and
/// this is the only thing it needs from a backend.
pub trait MediaClock: Send + Sync {
    /// Nanoseconds since the decoder loaded.
    fn now_ns(&self) -> u64;
}

/// One loaded video decoder.
pub trait VideoSink: Send {
    /// For the log line and the stats overlay.
    fn name(&self) -> &'static str;

    fn caps(&self) -> VideoSinkCaps;

    /// Hand one access unit (or one piece of one) to the decoder. `pts_ns` is in this sink's own
    /// clock domain and ignored by a sink whose caps say it takes no timestamp.
    fn feed(&self, au: &[u8], pts_ns: u64) -> Result<()>;

    /// Drop whatever is queued. A no-op where [`VideoSinkCaps::flush`] is false.
    fn flush(&self) -> Result<()> {
        Ok(())
    }

    /// Frames queued for presentation, or `None` where the backend can't say — which is not the
    /// same answer as an empty queue, and the stages treat it differently.
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

    /// The audio plane this load produced, where the backend has one and the load was accepted.
    /// It is an [`AudioSink`] too, so a route that rides it needs nothing else.
    fn audio_plane(&self) -> Option<Arc<dyn AudioPlane>> {
        None
    }

    /// Whether this decoder has failed in a way no re-anchor can undo, so the stage should stop
    /// feeding it and the session should end rather than run on a picture that will never return.
    /// `false` on a backend with no such notion, which is then simply never fatal.
    fn is_dead(&self) -> bool {
        false
    }
}

/// What an audio sink takes. Declared by the sink; the stage above produces exactly this and
/// nothing converts afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    /// Opus the sink decodes: the wire's stereo as-is, or 5.1 re-encoded into NDL's layout.
    Opus { channels: u8 },
    /// Interleaved f32 in punktfunk's own channel order — what libopus decodes to, so this is the
    /// format that costs no conversion at all.
    PcmF32 { channels: u8, sample_rate: u32 },
}

impl AudioFormat {
    /// Channels this sink puts on a speaker. Nothing is ever folded into it: a session asks the
    /// host for a layout the selected route carries (`model::AudioRoutePref::max_channels`), so a
    /// mismatch here is a bug, not a case to mix down.
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
    /// For the log line and the stats overlay.
    fn name(&self) -> &'static str;

    /// What this sink takes. The stage produces it; nothing converts on the way in.
    fn format(&self) -> AudioFormat;

    /// Feed one packet, stamped in the host's capture clock. A sink that paces on a timeline of
    /// its own maps it; one that just plays what it is given ignores it.
    fn feed(&self, samples: Samples<'_>, host_pts_ns: u64) -> Result<()>;

    /// Queue depth in ms where the sink knows one, for the stats overlay.
    fn depth_ms(&self) -> Option<i64> {
        None
    }
}

/// A hardware audio plane belonging to a video load — NDL's, in practice.
///
/// It is part of the *video* sink's world because the picture depends on it: NDL paces the
/// picture against a fed plane, so a plane starved of packets is a video stutter, not an audio
/// fault (docs/NOTES.md § "NDL's audio plane").
pub trait AudioPlane: AudioSink {
    /// How far the plane's stamps run ahead of its clock, in ms — the depth NDL paces the
    /// PICTURE on, and so the figure a stutter report is read against. Sagging towards zero is
    /// the signature.
    fn lead_ms(&self) -> i64;

    /// Keep the plane fed until `stop`, so the picture stays paced. Blocks; the caller gives it a
    /// thread. `yields_to_real` leaves the plane to whatever pump is feeding it and fills in only
    /// once that stops.
    fn run_keepalive(&self, stop: &AtomicBool, yields_to_real: bool);

    /// Whether the session's REAL audio may ride this plane, as opposed to only a keepalive.
    ///
    /// A plane can exist without being proven: a backend may accept the request and confirm it
    /// later, or never. Keeping it fed costs nothing if it turns out not to be there, but routing
    /// the session's only audio onto it does — that is a silent session — so the two questions are
    /// asked separately and the route takes the conservative one.
    fn accepts_stream(&self) -> bool {
        true
    }
}
