//! Bringing up a streaming session: capability negotiation and the handshake, then handing the
//! result to `pipeline`, which builds everything that decodes it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::quic;

use crate::core::caps::video_caps;
use crate::platform::webos::device::{self, NdlGeneration};
use crate::services::join::{join_with_timeout, SHUTDOWN_JOIN_TIMEOUT};
use crate::services::store::{CodecPref, GamepadType};
use crate::session::pipeline::MediaPipeline;
use crate::session::StreamStats;

pub struct Connected {
    pub client: Arc<NativeClient>,
    pub stop: Arc<AtomicBool>,
    /// Live pump counters for stats overlay; see `StreamStats`.
    pub stats: Arc<StreamStats>,
    /// The decode pipeline and the threads that drive it. Kept alive so `shutdown()` can join
    /// them, and so the QUIC close frame goes out before exit.
    pipeline: MediaPipeline,
    /// Where this session's audio actually ended up — the preference, resolved against what the
    /// load produced.
    pub audio_route: crate::services::store::AudioRoutePref,
    /// Whether HDR mastering metadata is being applied this session (negotiated codec is
    /// HEVC *and* the host signalled HDR). Drives which Game picture mode the runtime asks
    /// the TV for — `game` vs `hdrGame` (see `platform::webos::game_mode`).
    pub hdr: bool,
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
        let mut clean = self.pipeline.join();
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
    pub gamepad_type: GamepadType,
    pub cursor_capture: bool,
    /// Pad-audio render bits (`session::pad_audio::CAP_*`); non-zero advertises
    /// `CLIENT_CAP_PAD_AUDIO`. The per-pad declaration rides the arrival, not the handshake.
    pub pad_audio_caps: u8,
    pub audio_route: crate::services::store::AudioRoutePref,
    /// The panel volume advertised to the host and used until host metadata arrives.
    pub display_hdr: quic::HdrMeta,
}

/// One `quic::CODEC_*` bit, or 0 where the preference names no single codec.
fn codec_bit(pref: CodecPref) -> u8 {
    match pref {
        CodecPref::Auto => 0,
        CodecPref::H264 => quic::CODEC_H264,
        CodecPref::Hevc => quic::CODEC_HEVC,
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
    /// Whether to ask the host for slice-progressive AU prefixes — on wherever the backend can
    /// take them.
    frame_parts: bool,
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
        if audio_channels > 2 {
            crate::platform::webos::ndl::log_audio_output();
        }
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
        // HDR only ever applies to HEVC. An explicit H.264 pick disables it end to end
        // (the Settings toggle is hidden too — see `ui::settings`'s `row_shown`); on Automatic the
        // caps are still advertised and the host resolves the codec, with application gated
        // on the *negotiated* codec being HEVC in `load_player`.
        let hdr = params.hdr_enabled && caps.hdr && codec_pref != CodecPref::H264;
        Self {
            audio_channels,
            // VIDEO_CAP_CHACHA20: unconditional — armv7 has no hardware AES, so ChaCha20 is
            // faster. A ≥0.17.2 host picks it up; older hosts ignore the unknown bit.
            video_caps: quic::VIDEO_CAP_CHACHA20
                | if hdr {
                    quic::VIDEO_CAP_10BIT | quic::VIDEO_CAP_HDR
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
            frame_parts: device::ndl_generation() == NdlGeneration::V2,
        }
    }
}

/// Runs the handshake. Everything wire-facing has already been clamped by [`Negotiated::clamp`].
fn dial(params: &ConnectParams, negotiated: &Negotiated) -> Result<NativeClient> {
    NativeClient::connect_with_audio_format(
        &params.host,
        params.port,
        params.mode,
        CompositorPref::Auto,
        // Session-default pad kind. A per-pad `InputKind::GamepadArrival` could override this
        // for mixed setups, but this client drives one pad (index 0), for which the handshake
        // default is exactly equivalent — and it also reaches hosts too old to advertise
        // `HOST_CAP_GAMEPAD_STATE`.
        params.gamepad_type.to_core(),
        params.bitrate_kbps,
        negotiated.video_caps,
        // Requested only — the host clamps to what it can capture, and
        // `AudioPlayer::new` is built from the RESOLVED `client.audio_channels`,
        // never from this.
        negotiated.audio_channels,
        // Opus at 48 kHz/16-bit: this client has no lossless ask.
        0,
        0,
        // The standard coupling on every session: libopus here decodes either, and NDL's plane
        // takes only this one. A host that answers legacy is re-encoded (`session::audio`).
        punktfunk_core::audio::AudioLayout::Standard,
        negotiated.video_codecs,
        negotiated.preferred_codec,
        negotiated.display_hdr,
        // client_caps: see `store::Settings::cursor_capture` for the on/off split.
        (if params.cursor_capture {
            0
        } else {
            quic::CLIENT_CAP_CURSOR
        }) | if params.pad_audio_caps != 0 {
            quic::CLIENT_CAP_PAD_AUDIO
        } else {
            0
        },
        // Slice-progressive delivery: AU prefixes reach the decoder while the rest is still on the
        // wire, so a frame no longer waits for its own last datagram (`session::stage`'s `AuParts`).
        // On wherever it can be — NDL v2 only, per `Negotiated::clamp`: v1's feed has no timestamp
        // to repeat across pieces.
        negotiated.frame_parts,
        params.launch.clone(),
        // Device name for the host's pending-approval list. `None` keeps the host's
        // fingerprint-derived label ("device abcd1234"), i.e. exactly the behaviour before
        // core gained this parameter — sending a real TV name is a separate, user-visible
        // change and does not belong in a dependency bump.
        None,
        params.pin,
        Some(params.identity.clone()),
        params.timeout,
        // Uncancelable: the connect has its own thread and the caller joins it.
        None,
    )
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
pub fn connect(params: &ConnectParams) -> Result<Connected> {
    // Fails before touching the network: a full handshake would only end in `NdlVideo::load()`
    // rejecting the same gate, pointlessly holding the host's pending-session slot for `timeout`.
    crate::platform::webos::ndl::ensure_not_poisoned()?;
    let negotiated = Negotiated::clamp(params);
    let client = Arc::new(dial(params, &negotiated)?);
    log_handshake(&client, &negotiated);

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(StreamStats::default());
    // One call builds the whole decode path — sinks, stages and the threads that drive them — and
    // unwinds itself if any part of it fails. Nothing about which backend or which audio route it
    // settled on reaches back out here beyond the two figures the loop displays.
    let (pipeline, route, is_hdr) = MediaPipeline::build(params, &client, &stop, &stats)?;

    Ok(Connected {
        client,
        stop,
        stats,
        pipeline,
        audio_route: route,
        hdr: is_hdr,
    })
}
