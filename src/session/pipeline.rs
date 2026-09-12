//! Assembling one session's media pipeline: which sinks this TV gets, which stages sit above
//! them, and the threads that drive the whole thing.
//!
//! Everything backend-specific about a session is decided here, once. `connect` runs the handshake
//! and hands the result over; the pumps and stages above are written against `core::media`'s
//! traits and never learn which decoder or which audio route they got.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use punktfunk_core::client::NativeClient;

use crate::core::media::{AudioPlane, AudioSink, VideoSink};
use crate::platform::webos::device::{self, NdlGeneration};
use crate::platform::webos::ndl::v1::NdlV1Video;
use crate::platform::webos::ndl::{NdlCodec, NdlVideo};
use crate::services::join::{join_with_timeout, SHUTDOWN_JOIN_TIMEOUT};
use crate::services::store::AudioRoutePref;
use crate::session::audio::AudioStage;
use crate::session::connect::ConnectParams;
use crate::session::pump::{spawn_audio_feed, video_pump};
use crate::session::stage::{SinkConfig, VideoStage};
use crate::session::StreamStats;

/// The threads driving one session's pipeline. Dropping this does NOT stop them — the session's
/// `stop` flag does, and [`Self::join`] waits them out.
pub struct MediaPipeline {
    video_thread: std::thread::JoinHandle<()>,
    /// The audio pump, on the routes that own their sink. `None` on the software route, where the
    /// SDL device belongs to whichever thread initialised SDL and the loop spawns it instead.
    audio_thread: Option<std::thread::JoinHandle<()>>,
    /// The audio plane's keep-alive, on every V2 load that got a plane. `None` only when the load
    /// has no plane at all (V1, or a rejected audio load).
    clock_thread: Option<std::thread::JoinHandle<()>>,
}

impl MediaPipeline {
    /// Load the decoder, pick the audio route, and start every thread the session needs.
    ///
    /// Unwinds itself on failure: a thread already started is stopped and joined before the error
    /// returns, because a detached thread still feeding NDL would outlive the error the caller
    /// sees and race the `ndl::quit()` that follows it.
    ///
    /// Returns the pipeline, the route it settled on, and whether HDR metadata is being applied.
    pub fn build(
        params: &ConnectParams,
        client: &Arc<NativeClient>,
        stop: &Arc<AtomicBool>,
        stats: &Arc<StreamStats>,
    ) -> Result<(Self, AudioRoutePref, bool)> {
        let (player, is_hdr) = load_player(client, params)?;
        // Re-checked against the plane the load actually produced: rejected audio-enabled loads
        // leave no plane to ride. Metronome rides any plane and paces unconfirmed fine; real stream
        // rides only proven ones because the route can't be re-picked once running — a non-working
        // plane means silent audio.
        let plane = player.audio_plane();
        let proven = plane.as_ref().is_some_and(|p| p.accepts_stream());
        let route = resolve_route(params.audio_route, proven);
        tracing::info!(
            "audio path: {} on {} (host resolved {} channel(s))",
            audio_path_label(params.audio_route, route, plane.is_some(), proven),
            player.name(),
            client.audio_channels,
        );
        let video_thread = spawn_video_thread(client, player, stop, stats, is_hdr)?;
        // Failing here after the video thread is already up would otherwise detach it.
        let (audio_thread, clock_thread) = match spawn_plane_threads(client, plane, stop, route) {
            Ok(handles) => handles,
            Err(e) => {
                stop.store(true, Ordering::Relaxed);
                join_with_timeout(
                    video_thread,
                    SHUTDOWN_JOIN_TIMEOUT,
                    "video",
                    crate::platform::webos::ndl::poison,
                );
                return Err(e);
            }
        };
        Ok((
            Self {
                video_thread,
                audio_thread,
                clock_thread,
            },
            route,
            is_hdr,
        ))
    }

    /// Wait the threads out, bounded. `false` means one is still running — it may still be inside
    /// an NDL call, so the caller must skip `ndl::quit()`, and these three are the threads that
    /// touch NDL, so a wedge also refuses new loads until it finishes.
    pub fn join(self) -> bool {
        use crate::platform::webos::ndl::poison;
        let mut clean = join_with_timeout(self.video_thread, SHUTDOWN_JOIN_TIMEOUT, "video", poison);
        if let Some(audio) = self.audio_thread {
            clean &= join_with_timeout(audio, SHUTDOWN_JOIN_TIMEOUT, "audio", poison);
        }
        if let Some(clock) = self.clock_thread {
            clean &= join_with_timeout(clock, SHUTDOWN_JOIN_TIMEOUT, "clock", poison);
        }
        clean
    }
}

/// Downgrades the requested route to what the load proved. Software is the default and only
/// route with known-good pacing: NDL paces against a fed plane, which inherits network jitter
/// (silence plane cures this). Plane routes are kept selectable for comparison but unproven.
/// `has_plane` is the proven plane, not the requested one — see the call site.
fn resolve_route(pref: AudioRoutePref, has_plane: bool) -> AudioRoutePref {
    // The plane was loaded at the session's own width (stereo or 5.1), so only proof decides.
    match pref {
        AudioRoutePref::NdlOpus if has_plane => AudioRoutePref::NdlOpus,
        AudioRoutePref::Software | AudioRoutePref::NdlOpus => AudioRoutePref::Software,
    }
}

/// How long the V2 load waits for LOADCOMPLETED before starting unconfirmed. Only offload needs
/// proof — metronome paces unconfirmed planes fine, but wrong audio route is silent forever. Issue
/// #188: some sets report the callback only after the first video frame; extra waiting during load
/// just adds black screen.
fn plane_budget(pref: AudioRoutePref) -> std::time::Duration {
    match pref {
        AudioRoutePref::NdlOpus => crate::platform::webos::ndl::AUDIO_PROVE_BUDGET,
        AudioRoutePref::Software => crate::platform::webos::ndl::AUDIO_PRIME_BUDGET,
    }
}

/// Opens the decoder for the negotiated stream and hands it the colorimetry.
///
/// Returns the player and whether HDR mastering metadata is being applied — the answer the
/// video pump needs to know whether to forward per-content metadata at all.
fn load_player(client: &NativeClient, params: &ConnectParams) -> Result<(Box<dyn VideoSink>, bool)> {
    let panel = params.display_hdr;
    let resolved_mode = client.mode();
    let fps = resolved_mode.refresh_hz.max(1);
    let codec =
        NdlCodec::from_wire(client.codec).with_context(|| format!("unsupported codec 0x{:02x}", client.codec))?;
    let app_id = crate::platform::webos::ndl::app_id();
    let (width, height) = (resolved_mode.width as i32, resolved_mode.height as i32);
    let player: Box<dyn VideoSink> = match device::ndl_generation() {
        NdlGeneration::V2 => Box::new(Arc::new(
            // V2 loads ask for a plane to enable NDL pacing (docs/NOTES.md). Metronome is happy
            // unconfirmed, but real audio needs proof or silence results. Only offload route pays
            // the longer budget; wrong plane proof costs the route before any frame is fed.
            NdlVideo::load(
                &app_id,
                width,
                height,
                codec,
                Some(plane_budget(params.audio_route)),
                // Offload loads the plane at the session's width; the metronome rides stereo.
                if params.audio_route == AudioRoutePref::NdlOpus {
                    client.audio_channels
                } else {
                    2
                },
            )
            .context("NDL load")?,
        )),
        NdlGeneration::V1 => Box::new(NdlV1Video::load(&app_id, width, height, codec).context("NDL v1 load")?),
    };
    tracing::info!(
        "{} loaded ({codec:?} {}x{}@{fps}fps)",
        player.name(),
        resolved_mode.width,
        resolved_mode.height,
    );

    // HDR mastering metadata is applied only when the *negotiated* codec is HEVC: the
    // `NdlHdrInfo`/`setHdrInfo` fields are HEVC SEI syntax, and no other codec carries
    // HDR on this platform.
    let host_hdr = client.color.is_hdr();
    let is_hdr = host_hdr && matches!(codec, NdlCodec::H265);
    // What the host signalled in `Welcome`, before the SDR colorimetry fix below acts on it.
    tracing::info!(
        "host colour info: hdr={host_hdr} apply_hdr={is_hdr} codec={codec:?} transfer={} primaries={} matrix={}",
        client.color.transfer,
        client.color.primaries,
        client.color.matrix,
    );
    // Colorimetry goes to the decoder with the mastering metadata, and on this backend that means
    // it reaches it only on an HDR stream: `NDL_DirectVideoSetHDRInfo` emits an HDR infoframe on
    // ANY call and ignores an SDR triplet, so `NdlVideo::set_color_info` refuses `meta: None`
    // outright (read its docs before changing this — forcing the panel into HDR for SDR content is
    // the worse outcome, and it cost a black 1440p120 stream on a CX). `client.color` is still
    // passed on every session: the SDR arm is a no-op only on NDL, and a backend that can take
    // colorimetry without the HDR side effect gets it.
    if let Err(e) = player.set_color(is_hdr.then_some(panel).as_ref(), client.color) {
        tracing::warn!("NDL colour metadata failed: {e:#}");
    }
    Ok((player, is_hdr))
}

/// Why this session's audio ended up on the route it did. "Software Opus" is correct on three
/// different failure modes, all looking identical — logging the reason is essential for debugging.
/// `pref` distinguishes a downgrade (which narrowed channels per `max_channels` clamp before plane
/// proof) from a never-wanted route; downgrade merits explicit mention.
fn audio_path_label(pref: AudioRoutePref, route: AudioRoutePref, has_plane: bool, proven: bool) -> &'static str {
    match (route, has_plane) {
        (AudioRoutePref::NdlOpus, _) => "NDL hardware Opus decode (+ clock plane standing by)",
        // The user asked for the plane and the load never confirmed it in its budget, so the
        // session plays through SDL at the width it already negotiated.
        (AudioRoutePref::Software, true) if pref == AudioRoutePref::NdlOpus && !proven => {
            "software Opus decode -> SDL2 + NDL clock plane (offload asked for, plane unconfirmed)"
        }
        // A plane the real stream is not using is the pacing metronome — see
        // `NdlVideo::run_clock_plane`.
        (AudioRoutePref::Software, true) => "software Opus decode -> SDL2 + NDL clock plane",
        // No plane means no pacing reference either: NDL v1 has none, or the audio-enabled load
        // was refused outright and fell back to video-only.
        (AudioRoutePref::Software, false) => "software Opus decode -> SDL2, no clock plane",
    }
}

fn spawn_video_thread(
    client: &Arc<NativeClient>,
    player: Box<dyn VideoSink>,
    stop: &Arc<AtomicBool>,
    stats: &Arc<StreamStats>,
    is_hdr: bool,
) -> Result<std::thread::JoinHandle<()>> {
    let cfg = SinkConfig {
        stream_hz: client.mode().refresh_hz,
        report_decode_latency: client.wants_decode_latency(),
    };
    let (client, stop, stats) = (client.clone(), stop.clone(), stats.clone());
    std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || {
            // Built here, not on the caller's thread: the sink queries the panel refresh
            // rate through SDL on construction, and that stayed on the video thread before.
            let stage = VideoStage::new(player, stats.clone(), cfg);
            video_pump(client, stage, stop, stats, is_hdr);
        })
        .context("spawn video thread")
}

/// `(audio pump, clock plane)`.
type PlaneThreads = (Option<std::thread::JoinHandle<()>>, Option<std::thread::JoinHandle<()>>);

/// Threads feeding and riding NDL's audio plane: the keep-alive loop and real stream's pump
/// (if the route rides it). NDL paces the picture against any fed plane regardless of audio routing;
/// the route determines which thread feeds it: Software uses the metronome (silence only),
/// `NdlOpus` yields to the real stream (hardware-stamped).
fn spawn_plane_threads(
    client: &Arc<NativeClient>,
    ndl_audio: Option<Arc<dyn AudioPlane>>,
    stop: &Arc<AtomicBool>,
    route: AudioRoutePref,
) -> Result<PlaneThreads> {
    let Some(ndl) = ndl_audio else {
        return Ok((None, None));
    };
    // What the stage feeds: the plane itself where the hardware stamps, and nothing at all on the
    // software route (the SDL device belongs to the runtime).
    let sink: Option<Arc<dyn AudioSink>> =
        (route != AudioRoutePref::Software).then(|| ndl.clone() as Arc<dyn AudioSink>);
    let clock_thread = crate::platform::webos::ndl::spawn_clock_plane(ndl, stop.clone(), route.on_ndl_plane())
        .context("spawn clock plane thread")?;
    let Some(sink) = sink else {
        return Ok((None, Some(clock_thread)));
    };
    // Folded into the spawn's own error type to keep ONE failure path: an early `?` here would
    // return before the clock thread above is joined, detaching a thread still feeding NDL.
    let audio_thread = match AudioStage::new(sink, client.audio_channels, client.audio_layout) {
        Ok(stage) => {
            tracing::info!(
                "audio stage: {} channel(s), layout {} into {}",
                client.audio_channels,
                client.audio_layout,
                stage.sink_name()
            );
            spawn_audio_feed(client.clone(), stage, stop.clone())
        }
        Err(e) => Err(anyhow::anyhow!("audio stage: {e:#}")),
    };
    match audio_thread {
        Ok(handle) => Ok((Some(handle), Some(clock_thread))),
        // Same reason `connect` unwinds its video thread on failure: a detached thread still
        // feeding NDL would outlive the error and race the `ndl::quit()` that follows it.
        Err(e) => {
            stop.store(true, Ordering::Relaxed);
            join_with_timeout(
                clock_thread,
                SHUTDOWN_JOIN_TIMEOUT,
                "clock",
                crate::platform::webos::ndl::poison,
            );
            Err(e).context("spawn audio pump thread")
        }
    }
}
