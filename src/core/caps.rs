//! What the active video backend can present — one fact, read by everything that must agree
//! about it: `session::connect` (what's advertised — authoritative, the codec is negotiated
//! before any decoder opens), the shell's settings (what's offerable), `Settings::clamp_to_caps`
//! (normalising a document written on a more capable TV).
//!
//! In `core`, not `platform`, because `services` can't depend on `platform::webos`. Hence
//! the install-once global: the platform layer publishes at startup, every layer reads. **Unset
//! reads as [`VideoCaps::FULL`]** — today's webOS 5+ behaviour, so host builds, tests and any
//! pre-install path see exactly what shipped before this existed.
//!
//! One fact is stored: the NDL baseline, fixed for the run. Everything else — the active caps,
//! the codecs worth offering — derives from it.
use std::sync::OnceLock;

use crate::core::model::CodecPref;

/// Video capabilities of the backend this run will use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoCaps {
    pub h265: bool,
    /// HDR (10-bit + mastering metadata). Implies [`Self::h265`].
    pub hdr: bool,
    /// Highest audio channel count this client can decode and present through the SOFTWARE
    /// route — the decoder-wide ceiling.
    pub max_channels: u8,
    /// Whether an NDL audio plane exists at all here. Only NDL `DirectMedia` v2 has one: v1 has
    /// no audio type. False leaves
    /// `AudioRoutePref::Software` as the only route (`AudioRoutePref::available`).
    pub audio_plane: bool,
}

impl VideoCaps {
    /// NDL `DirectMedia` v2 on webOS 5+ — what every currently-working device gets, and the
    /// default until the platform installs something narrower.
    pub const FULL: Self = Self {
        h265: true,
        hdr: true,
        max_channels: 8,
        audio_plane: true,
    };

    /// NDL `DirectMedia` v1 on webOS 3.5-4.x. Stereo because its audio path is unused and
    /// 5.1/7.1 on a TV of that generation isn't worth offering.
    pub const H264_SDR: Self = Self {
        h265: false,
        hdr: false,
        max_channels: 2,
        audio_plane: false,
    };

    /// The codec preferences worth offering here, in display order — the one place the codec set
    /// is spelled, so the persisted-document clamp and the advertised wire set can't disagree.
    /// Without HEVC only one codec is decodable, so `Automatic` would resolve to it anyway and
    /// the list collapses to a single entry.
    pub fn codec_prefs(self) -> &'static [CodecPref] {
        if self.h265 {
            &[CodecPref::Auto, CodecPref::H264, CodecPref::Hevc]
        } else {
            &[CodecPref::H264]
        }
    }
}

/// What NDL can do on this TV — the platform's answer, fixed for the run. Unset reads as
/// [`VideoCaps::FULL`] (see the module docs).
static NDL_BASELINE: OnceLock<VideoCaps> = OnceLock::new();

/// Publish the detected NDL caps. Call once, before settings load or any UI is built; later calls
/// are ignored.
pub fn install(ndl_caps: VideoCaps) {
    if NDL_BASELINE.set(ndl_caps).is_err() {
        tracing::warn!("video caps already installed — ignoring {ndl_caps:?}");
    }
}

/// The active caps: the NDL baseline detected at startup.
pub fn video_caps() -> VideoCaps {
    NDL_BASELINE.get().copied().unwrap_or(VideoCaps::FULL)
}
