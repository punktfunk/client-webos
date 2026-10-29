//! Bringing up a streaming session: capability negotiation and the handshake, then handing the
//! result to `pipeline`, which builds everything that decodes it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::quic;

use crate::core::caps::video_caps;
use crate::services::join::{join_with_timeout, SHUTDOWN_JOIN_TIMEOUT};
use crate::services::store::{CodecPref, GamepadType};
use crate::session::pipeline::{MediaParams, MediaPipeline};
use crate::session::StreamStats;

/// How long a codec ask waits for the first frame in the host's next codec.
const CODEC_ASK_WAIT: Duration = Duration::from_secs(3);

#[derive(Default)]
pub struct ConnectAttempt(std::sync::Mutex<AttemptState>);

#[derive(Default)]
struct AttemptState {
    cancelled: bool,
    media_started: bool,
}

impl ConnectAttempt {
    pub fn cancel(&self) -> Option<crate::platform::webos::ndl::LoadGuard> {
        let mut state = self.0.lock().expect("connect attempt poisoned");
        state.cancelled = true;
        // Network-only attempts cannot enter media after cancellation. Once media starts,
        // retain exclusion until cleanup; a timer cannot safely revoke NDL ownership.
        state.media_started.then(crate::platform::webos::ndl::suspend_loads)
    }

    fn enter_media(&self) -> Result<()> {
        let mut state = self.0.lock().expect("connect attempt poisoned");
        anyhow::ensure!(!state.cancelled, "connection cancelled");
        state.media_started = true;
        Ok(())
    }
}

pub struct Connected {
    pub client: Arc<NativeClient>,
    pub stop: Arc<AtomicBool>,
    pub(crate) input_state: Arc<std::sync::Mutex<crate::core::input::HeldInputs>>,
    /// Live pump counters for stats overlay; see `StreamStats`.
    pub stats: Arc<StreamStats>,
    /// The decode pipeline, reloaded on each format the host moves the stream to.
    media: Mutex<Media>,
    /// Where this session's audio actually ended up — the preference, resolved against what the
    /// load produced.
    pub audio_route: crate::services::store::AudioRoutePref,
}

/// The session's decode pipeline and what its next load needs.
struct Media {
    /// Kept so `shutdown()` can join its threads, and so the QUIC close frame goes out before
    /// exit. `None` once a reload failed.
    pipeline: Option<MediaPipeline>,
    params: MediaParams,
    /// When this session asked the host for another codec, until a load for it is up.
    asked: Option<Instant>,
    /// `CODEC_*` bits NDL failed on, so an ask never sends the host back to one.
    excluded: u8,
}

impl Connected {
    /// Stop and join threads, then drop `NativeClient`. Call `disconnect_quit()` first for
    /// graceful shutdown. Returns `false` if any step didn't finish within
    /// `SHUTDOWN_JOIN_TIMEOUT` — the caller must then skip `ndl::quit()`, since the thread
    /// still running may still be inside an NDL FFI call that a concurrent unload would race. A
    /// wedged video/audio/clock join additionally refuses new loads until it finishes — those three
    /// are the threads that touch NDL.
    pub fn shutdown(self) -> bool {
        self.stop.store(true, Ordering::Relaxed);
        let media = self.media.into_inner().unwrap_or_else(PoisonError::into_inner);
        let mut clean = media.pipeline.is_none_or(MediaPipeline::join);
        // `NativeClient::drop` joins its own QUIC-close worker thread internally — bound
        // that the same way, on its own thread, rather than blocking here directly. Doesn't
        // touch NDL, so a wedge here doesn't refuse it, only skips `ndl::quit()` this run.
        let client = self.client;
        clean &= join_with_timeout(
            std::thread::spawn(move || drop(client)),
            SHUTDOWN_JOIN_TIMEOUT,
            "client-drop",
            || (),
        );
        clean
    }

    /// Keeps NDL decoding across the host's format changes; the stream loop calls it every tick.
    /// Reloads on a frame in a new format, and asks the host for another codec when the decoder
    /// dies. `false` once the decoder is gone with nothing left to ask for.
    pub fn tend_media(&self) -> bool {
        let mut media = self.media.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(reformat) = media.pipeline.as_ref().map(MediaPipeline::take_reformat) else {
            return false;
        };
        if let Some(first) = reformat {
            tracing::info!(format = ?first.new_format, "the host changed the stream format — reloading NDL");
            let old = media.pipeline.take().expect("the pipeline was just read");
            match old.rebuild(first, &media.params, &self.client, &self.stats) {
                Ok((pipeline, route)) => {
                    // ponytail: a plane that stops proving itself leaves offload audio silent
                    // until the next connect; the SDL route is picked once, at the stream start.
                    if route != self.audio_route {
                        tracing::warn!(?route, "NDL reload: the audio plane did not come back");
                    }
                    media.pipeline = Some(pipeline);
                    media.asked = None;
                    self.stats.decoder_dead.store(false, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::error!("NDL reload: {e:#}");
                    return false;
                }
            }
        }
        if !self.stats.decoder_dead.load(Ordering::Relaxed) {
            return true;
        }
        if let Some(asked) = media.asked {
            return self.client.codec_refused() == 0 && asked.elapsed() < CODEC_ASK_WAIT;
        }
        let codec = self.client.video_format().codec;
        media.excluded |= codec;
        if let Err(e) = self.client.request_codec_change(media.excluded) {
            tracing::debug!("no codec ask: {e:#}");
            return false;
        }
        tracing::warn!("NDL failed on codec 0x{codec:02x} — asking the host for another");
        media.asked = Some(Instant::now());
        true
    }

    /// [`Self::shutdown`], then the NDL unload unless a thread is wedged inside it.
    pub fn shutdown_and_quit(self) {
        if self.shutdown() {
            crate::platform::webos::ndl::quit();
        } else {
            tracing::warn!("session teardown timed out — skipping NDL unload for this run");
        }
    }
}

/// Everything [`connect`] needs from the chosen target and the user's settings.
///
/// A struct rather than a parameter list: every field comes from exactly those two places, and a
/// positional list of fifteen mostly-scalar arguments is one where a swapped pair still compiles.
pub struct ConnectParams {
    pub host: String,
    pub port: u16,
    /// Requested capture mode; the host clamps it and echoes the result in `client.mode()`.
    pub mode: Mode,
    pub bitrate_kbps: u32,
    pub hdr_enabled: bool,
    /// Main10 at BT.709 without HDR. Subsumed by `hdr_enabled`.
    pub ten_bit_sdr: bool,
    pub audio_channels: u8,
    /// This client's TLS identity, `(cert_pem, key_pem)`.
    pub identity: (String, String),
    /// Trusted host fingerprint from a prior pairing; `None` = trust-on-first-use.
    pub pin: Option<[u8; 32]>,
    /// Game/app handle for the host to launch once the session is up.
    pub launch: Option<String>,
    /// Handshake budget.
    pub timeout: Duration,
    pub codec: CodecPref,
    /// Advisory: the host falls back to its own default when this one is unavailable.
    pub compositor: CompositorPref,
    pub gamepad_type: GamepadType,
    pub cursor_capture: bool,
    /// Pad-audio render bits (`session::pad_audio::CAP_*`); non-zero advertises
    /// `CLIENT_CAP_PAD_AUDIO`. The per-pad declaration rides the arrival, not the handshake.
    pub pad_audio_caps: u8,
    pub audio_route: crate::services::store::AudioRoutePref,
    pub present_priority: pf_client_core::trust::PresentPriority,
    /// The panel volume advertised to the host and used until host metadata arrives.
    pub display_hdr: quic::HdrMeta,
    /// The host profile to play as; `None` lets the host choose.
    pub profile: Option<String>,
}

/// One `quic::CODEC_*` bit, or 0 where the preference names no single codec.
fn codec_bit(pref: CodecPref) -> u8 {
    match pref {
        CodecPref::Auto => 0,
        CodecPref::H264 => quic::CODEC_H264,
        CodecPref::Hevc => quic::CODEC_HEVC,
        CodecPref::Av1 => quic::CODEC_AV1,
    }
}

/// What this client advertises on the wire, clamped to what the TV can actually decode.
struct Negotiated {
    audio_channels: u8,
    /// `quic::VIDEO_CAP_*` bitfield.
    video_caps: u8,
    /// `quic::CODEC_*` bitfield: every codec this client can present.
    video_codecs: u8,
    /// A single `quic::CODEC_*` bit, or 0 for auto.
    preferred_codec: u8,
    display_hdr: Option<quic::HdrMeta>,
}

impl Negotiated {
    /// **The authoritative capability gate.** Codec, colour path and channel count are settled by
    /// the handshake, BEFORE any decoder opens, so a document carried over from a more capable TV
    /// must be clamped here and not merely hidden in the UI: HEVC negotiated onto an H.264-only
    /// decoder is a frozen black stream with no second chance once `Welcome` has resolved.
    fn clamp(params: &ConnectParams) -> Self {
        let caps = video_caps();
        // `params.audio_channels` is the user's PREFERENCE; this is where it becomes a width.
        // Only the static limits narrow it: what this client decodes and what the route carries.
        // Sound Out does not — webOS folds what its output can't pass (`ndl::log_audio_output`).
        let route_max = params.audio_route.max_channels(caps);
        let audio_channels = params.audio_channels.min(caps.max_channels).min(route_max);
        if audio_channels < params.audio_channels {
            // Names the limit that bound: "why is this stereo" is the question the log answers.
            let reason = if audio_channels == route_max {
                "the audio route carries no more"
            } else {
                "this client decodes no more"
            };
            tracing::info!(
                "audio: {} channel(s) requested, asking for {audio_channels} — {reason} \
                 (client {}, route {route_max})",
                params.audio_channels,
                caps.max_channels,
            );
        }
        let codecs = caps.codec_prefs();
        let codec_pref = if codecs.contains(&params.codec) {
            params.codec
        } else {
            codecs[0]
        };
        // Automatic still advertises HDR; `load_player` gates metadata on the negotiated codec.
        let hdr = params.hdr_enabled && caps.hdr && codec_pref.allows_ten_bit();
        // Same codec rule as HDR. NDL decodes Main10 from the SPS, and HDR metadata keys on the
        // host's colour, not the depth, so an SDR Main10 stream never flips the panel.
        let ten_bit_sdr = params.ten_bit_sdr && caps.h265 && codec_pref.allows_ten_bit();
        Self {
            audio_channels,
            // VIDEO_CAP_CHACHA20: unconditional — armv7 has no hardware AES, so ChaCha20 is
            // faster. A ≥0.17.2 host picks it up; older hosts ignore the unknown bit.
            video_caps: quic::VIDEO_CAP_CHACHA20
                | if hdr {
                    quic::VIDEO_CAP_10BIT | quic::VIDEO_CAP_HDR
                } else if ten_bit_sdr {
                    quic::VIDEO_CAP_10BIT
                } else {
                    0
                },
            // Advertised decode set folded from the one codec list (`codec_prefs`) so the host's
            // precedence ladder can never auto-pick a path this client can't present.
            video_codecs: codecs.iter().fold(0, |set, &pref| set | codec_bit(pref)),
            preferred_codec: codec_bit(codec_pref),
            // Core may replace this through `PUNKTFUNK_CLIENT_PEAK_NITS`; the host echoes that
            // effective volume on the metadata plane, so NDL converges after startup.
            display_hdr: hdr.then_some(params.display_hdr),
        }
    }
}

/// Runs the handshake. Everything wire-facing has already been clamped by [`Negotiated::clamp`].
fn dial(params: &ConnectParams, negotiated: &Negotiated) -> Result<NativeClient> {
    NativeClient::connect(punktfunk_core::client::ConnectParams {
        compositor: params.compositor,
        // Session-default pad kind; this client drives one pad (index 0), so it equals a
        // per-pad arrival and also reaches hosts without `HOST_CAP_GAMEPAD_STATE`.
        gamepad: params.gamepad_type.to_core(),
        bitrate_kbps: params.bitrate_kbps,
        video_caps: negotiated.video_caps,
        // Requested only: `AudioPlayer::new` is built from the RESOLVED `client.audio_channels`.
        audio_channels: negotiated.audio_channels,
        // libopus here decodes either coupling, and NDL's plane takes only this one. A host that
        // answers legacy is re-encoded (`session::audio`).
        audio_layout: punktfunk_core::audio::AudioLayout::Standard,
        video_codecs: negotiated.video_codecs,
        preferred_codec: negotiated.preferred_codec,
        display_hdr: negotiated.display_hdr,
        // See `store::Settings::cursor_capture` for the on/off split.
        client_caps: (if params.cursor_capture {
            0
        } else {
            quic::CLIENT_CAP_CURSOR
        }) | if params.pad_audio_caps != 0 {
            quic::CLIENT_CAP_PAD_AUDIO
        } else {
            0
        },
        // NDL takes complete AUs, so `frame_parts` stays off.
        // The stream loop reloads NDL on a frame in a new format (`Connected::tend_media`).
        rebuilds_decoder: true,
        launch: params.launch.clone(),
        pin: params.pin,
        identity: Some(params.identity.clone()),
        profile: params.profile.clone(),
        // LG's video path reads every stream as studio range, and NDL has no range to set.
        studio_range: true,
        studio_range_hdr: true,
        // Uncancelable: the connect has its own thread and the caller joins it.
        ..punktfunk_core::client::ConnectParams::new(&params.host, params.port, params.mode, params.timeout)
    })
    .context("connect")
}

/// The one line that says what the handshake actually settled on.
fn log_handshake(client: &NativeClient, negotiated: &Negotiated) {
    let fp_hex = client.host_fingerprint.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    tracing::info!(
        "connected: codec={} (offered=0x{:02x} preferred=0x{:02x}) \
         compositor={:?} audio_ch={} audio_layout={} color={:?} wire_budget_kbps={} \
         decode_latency={} caps=0x{:02x} fp={fp_hex}",
        client.codec,
        negotiated.video_codecs,
        negotiated.preferred_codec,
        client.resolved_compositor,
        client.audio_channels,
        client.audio_layout,
        client.color,
        client.resolved_bitrate_kbps,
        client.wants_decode_latency(),
        negotiated.video_caps,
    );
}

/// Connects to a punktfunk host and starts the video pump thread.
///
/// Blocks until the handshake completes or `params.timeout` elapses. NDL manages its own
/// punch-through area natively (see [`crate::platform::webos::ndl`]'s module docs), so no
/// display geometry is needed here.
pub fn connect(params: &ConnectParams, attempt: &ConnectAttempt) -> Result<Connected> {
    // Fails before touching the network: a full handshake would only end in `NdlVideo::load()`
    // rejecting the same gate, or hanging in it, while the host holds a session for us. The role
    // goes first: a stale role is what wedges NDL, and only its message names the fix.
    crate::platform::webos::ndl::ensure_media_role()?;
    crate::platform::webos::ndl::ensure_not_poisoned()?;
    let negotiated = Negotiated::clamp(params);
    let client = Arc::new(dial(params, &negotiated)?);
    log_handshake(&client, &negotiated);

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StreamStats::default());
    // Spawns the decode threads; fails atomically if any setup step fails.
    let media = attempt.enter_media().and_then(|()| {
        // Here, right before the load, not at the clamp: the query needs NDL initialised, and an
        // init with no load behind it — a dial that fails, a launch cancelled mid-handshake —
        // would leave NDL warm for the next session's load (docs/NOTES.md: init stays lazy).
        if negotiated.audio_channels > 2 {
            crate::platform::webos::ndl::log_audio_output();
        }
        MediaPipeline::build(&MediaParams::of(params), &client, &stats, None)
    });
    let (pipeline, route) = match media {
        Ok(built) => built,
        Err(e) => {
            // Past the handshake the host holds a session (and may have launched a game) for
            // us: end it deliberately rather than leave it lingering for a stream that isn't
            // coming — the same teardown `PendingConnect`'s drop gives a connect that finished.
            client.disconnect_quit();
            return Err(e);
        }
    };

    Ok(Connected {
        client,
        stop,
        input_state: Arc::default(),
        stats,
        // A reload asks for the route this load settled on: see `MediaPipeline::rebuild`.
        media: Mutex::new(Media {
            pipeline: Some(pipeline),
            params: MediaParams {
                audio_route: route,
                ..MediaParams::of(params)
            },
            asked: None,
            excluded: 0,
        }),
        audio_route: route,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::caps::VideoCaps;

    #[test]
    fn av1_is_offered_only_beside_hevc() {
        let set = |caps: VideoCaps| caps.codec_prefs().iter().fold(0, |s, &p| s | codec_bit(p));
        assert_eq!(
            set(VideoCaps::FULL),
            quic::CODEC_H264 | quic::CODEC_HEVC | quic::CODEC_AV1
        );
        assert_eq!(set(VideoCaps::H264_SDR), quic::CODEC_H264);
    }
}
