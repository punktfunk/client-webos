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
        // Metronome rides any plane; real stream only proven ones. Route is locked once running.
        let plane = player.audio_plane();
        let proven = plane.as_ref().is_some_and(|p| p.accepts_stream());
        let route = resolve_route(params.audio_route, proven);
        // Set before plane threads start. Only for streams riding the plane: metronome has no
        // sync to hold, and its depth is a measured figure that must not be disturbed.
        let extra_lead_ms = super::timeline::smooth_cushion_ms(client.mode().refresh_hz, params.present_priority);
        if route.on_ndl_plane() && extra_lead_ms > 0 {
            if let Some(p) = plane.as_ref() {
                tracing::info!("audio plane holds {extra_lead_ms}ms extra to match the smoothness buffer");
                p.set_extra_lead_ms(extra_lead_ms);
            }
        }
        tracing::info!(
            "audio path: {} on {} (host resolved {} channel(s))",
            audio_path_label(params.audio_route, route, plane.is_some(), proven),
            player.name(),
            client.audio_channels,
        );
        let video_thread = spawn_video_thread(client, player, stop, stats, is_hdr, params.present_priority, route)?;
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
            // V2 loads ask for a plane to enable NDL pacing (docs/NOTES.md).
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
    // Colorimetry with mastering only on HDR streams: `NDL_DirectVideoSetHDRInfo` emits infoframes
    // on any call, and forcing HDR for SDR caused black 1440p120 on CX.
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
        // User asked for offload, but plane never confirmed in budget.
        (AudioRoutePref::Software, true) if pref == AudioRoutePref::NdlOpus && !proven => {
            "software Opus decode -> SDL3 + NDL clock plane (offload asked for, plane unconfirmed)"
        }
        // Plane is the pacing metronome; see `NdlVideo::run_clock_plane`.
        (AudioRoutePref::Software, true) => "software Opus decode -> SDL3 + NDL clock plane",
        // No plane: NDL v1 has none, or load was refused.
        (AudioRoutePref::Software, false) => "software Opus decode -> SDL3, no clock plane",
    }
}

fn spawn_video_thread(
    client: &Arc<NativeClient>,
    player: Box<dyn VideoSink>,
    stop: &Arc<AtomicBool>,
    stats: &Arc<StreamStats>,
    is_hdr: bool,
    present_priority: pf_client_core::trust::PresentPriority,
    route: AudioRoutePref,
) -> Result<std::thread::JoinHandle<()>> {
    let cfg = SinkConfig {
        stream_hz: client.mode().refresh_hz,
        report_decode_latency: client.wants_decode_latency(),
        present_priority,
    };
    let audio_rides_plane = route.on_ndl_plane();
    let (client, stop, stats) = (client.clone(), stop.clone(), stats.clone());
    std::thread::Builder::new()
        .name("punktfunk-webos-video".into())
        .spawn(move || {
            // VideoStage queries the panel refresh rate through SDL on construction.
            let stage = VideoStage::new(player, stats.clone(), &cfg);
            video_pump(client, stage, stop, stats, is_hdr, audio_rides_plane);
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
    // Feed the plane on offload; software route's SDL device is managed elsewhere.
    let sink: Option<Arc<dyn AudioSink>> =
        (route != AudioRoutePref::Software).then(|| ndl.clone() as Arc<dyn AudioSink>);
    let clock_thread = crate::platform::webos::ndl::spawn_clock_plane(ndl, stop.clone(), route.on_ndl_plane())
        .context("spawn clock plane thread")?;
    let Some(sink) = sink else {
        return Ok((None, Some(clock_thread)));
    };
    // Handle errors after joining the clock thread to avoid detaching it.
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
        // Avoid detaching threads still feeding NDL.
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
