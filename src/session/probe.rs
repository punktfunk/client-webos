//! Handshake-only connections: the pairing trust step and the network speed test.
//!
//! Neither loads a video backend or spawns a pump — see the `connect` module for the streaming
//! path. Both block, and both are run on a worker thread by their callers.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::client::{NativeClient, ProbeOutcome};
use punktfunk_core::config::{CompositorPref, Mode};
use punktfunk_core::quic;

/// Opens a handshake-only session: no video backend loads, no pump thread spawns, nothing is
/// presented. Both callers below share it so the decisions that are the same either way — the
/// throwaway 720p mode, whole-AU delivery, no cursor, no HDR metadata, the host's own device
/// label — are stated once.
///
/// **`video_caps` always advertises `VIDEO_CAP_CHACHA20`, exactly as a real session does.**
/// `punktfunk-core` counts delivered bytes AFTER AEAD decrypt, so a probe that negotiated
/// AES-GCM would measure a ceiling this armv7 CPU can't reach with the cipher an actual stream
/// uses. See `docs/NOTES.md` on why `ChaCha20` exists on this client at all.
fn handshake_only(
    host: &str,
    port: u16,
    bitrate_kbps: u32,
    video_codecs: u8,
    pin: Option<[u8; 32]>,
    identity: (String, String),
    timeout: Duration,
) -> punktfunk_core::Result<NativeClient> {
    // The negotiated mode is irrelevant here (immediately dropped); a small 720p request keeps
    // the host from doing needless 4K/HEVC setup for a connection we close at once.
    let mode = Mode {
        width: 1280,
        height: 720,
        refresh_hz: 60,
    };
    NativeClient::connect(
        host,
        port,
        mode,
        CompositorPref::Auto,
        punktfunk_core::config::GamepadPref::Auto,
        bitrate_kbps,
        quic::VIDEO_CAP_CHACHA20,
        2, // stereo baseline
        video_codecs,
        0,     // no preferred codec
        None,  // no HDR display metadata: nothing presents
        0,     // client_caps: nothing renders a cursor
        false, // frame_parts: whole AUs (see `super::connect`)
        None,  // no launch
        None,  // name: keep the host's fingerprint-derived label (see `super::connect`)
        pin,
        Some(identity),
        timeout,
    )
}

/// The no-PIN "request access" trust step: connect presenting our identity, which a host
/// requiring pairing PARKS until its operator approves this device, then return the host's
/// fingerprint to pin and tear the connection straight back down. `pin` is an advertised
/// fingerprint to hold the host to; `None` trusts whoever answers.
///
/// Uses `handshake_only`, so the video plane is never touched — this needs the handshake to reach
/// `Welcome`, not a running stream. Blocks up to `timeout` (the operator-approval window).
pub fn request_access(
    host: &str,
    port: u16,
    pin: Option<[u8; 32]>,
    identity: (String, String),
    timeout: Duration,
) -> Result<[u8; 32]> {
    let client = handshake_only(
        host,
        port,
        1_000, // minimal bitrate — connection is closed as soon as trust is established
        quic::CODEC_H264,
        pin,
        identity,
        timeout,
    )
    .context("request access connect")?;
    let fingerprint = client.host_fingerprint;
    // Deliberate teardown — the host should drop the parked/approved session now, not
    // linger for a stream that isn't coming. (Runs on a background thread — see
    // `App::try_request_access` — so no log handle here; the caller logs the outcome.)
    client.disconnect_quit();
    Ok(fingerprint)
}

/// What the host is asked to burst during a speed test, and for how long.
///
/// Deliberately **not** the other clients' 3 Gbps / 5 s, and not the 1 Gbps this first
/// shipped with either. Measured on a real CX over Wi-Fi against a 0.19.2 host: a 1 Gbps
/// request was honoured exactly (375 MB pushed in 3 s) while the TV received 87 MB —
/// **~80 % loss** — and in half the attempts the host's end-of-burst `ProbeResult`, which
/// travels over the QUIC control stream *through that same saturated path*, never arrived
/// at all. Overshooting capacity is how a probe finds a ceiling, but overshooting it
/// fourfold mostly measures the access point's drop policy and costs the measurement its
/// own result message.
///
/// The target is chosen against what the answer can actually be *used* for: the bitrate
/// slider caps at `core::model::BITRATE_MAX_KBPS` (200 Mbps) and the recommendation is 70 % of
/// measured, so anything above ~285 Mbps already produces an identical clamped
/// recommendation. 320 Mbps stays above that — it can still detect any ceiling that
/// would change the advice — while keeping the overshoot bounded. Measured on a G5
/// (2026-07-24, warm data plane, sweep 260/280/320/400): delivered goodput is a flat
/// ~245 Mbps at every offered rate — the TV's own Wi-Fi radio (USB 2.0-attached) is the
/// ceiling, independently confirmed with a raw UDP flood — so a 400 Mbps burst just
/// raises the shed overshoot (51 % packet loss vs 38 % at 320) and with it the odds the
/// end-of-burst report is starved out, for zero extra information.
const PROBE_TARGET_KBPS: u32 = 320_000;
/// A pinned (non-zero) session rate for the probe connect — see the call site: its only
/// job is to keep `bitrate_kbps == 0` from arming core's own capacity probe against the
/// single shared `ProbeState`. Nothing decodes here, so the value itself is immaterial.
const PROBE_SESSION_BITRATE_KBPS: u32 = 20_000;
/// Below this many delivered bytes, a missing host report is a failure rather than
/// something to salvage — 1 MB over a 3 s burst is ~2.7 Mbps, far under anything worth
/// recommending a bitrate from.
const SALVAGE_MIN_BYTES: u64 = 1024 * 1024;
const PROBE_DURATION_MS: u32 = 3_000;
/// How long to wait for the data plane to prove itself live (first completed video frame)
/// before bursting. Observed on the G5 over Wi-Fi (2026-07-24): a NEW host→client UDP flow
/// is sometimes black-holed (AP/driver flow setup — even the session's own 20 Mbps video
/// is held, while QUIC control chats at ~1 ms RTT), then dumped all at once. Measured
/// holes ranged ~10-29 s, longer after longer idle. A burst fired into that window
/// measures the black hole, not the link. Waiting for the first delivered frame starts
/// every measurement on a proven-live plane; if nothing arrives within the cap, proceed
/// anyway — the burst then behaves exactly as before.
const PROBE_WARMUP_CAP: Duration = Duration::from_secs(35);
/// How long to keep polling for the host's end-of-burst report after the burst should
/// have finished before giving up. Generous: the report shares a link the burst has just
/// been hammering, so its first delivery attempt can well be lost and need a retransmit.
const PROBE_REPORT_GRACE: Duration = Duration::from_secs(12);

/// A finished speed test, and whether the host confirmed the figures.
pub struct SpeedProbeResult {
    pub outcome: ProbeOutcome,
    /// `true` when the host's end-of-burst report arrived. `false` means it never did and
    /// the throughput was derived from what this client actually received over the burst
    /// window it asked for — a real measurement, but with no host-side cross-check, so no
    /// loss figure and a conservative reading if the burst was cut short.
    pub confirmed: bool,
}

/// Runs one network speed test against `host` and returns the host's final measurement.
///
/// Like [`request_access`], this uses [`NativeClient`] directly rather than `session::connect`:
/// no video backend is loaded and no pump thread is spawned, so the punch-through plane
/// is never touched — the host builds a virtual output, but nothing is decoded or
/// presented. Blocks; run it on a worker thread.
///
/// `progress` is called with each partial poll so the UI can show the figure climbing.
pub fn run_speed_probe(
    host: &str,
    port: u16,
    identity: (String, String),
    pin: Option<[u8; 32]>,
    timeout: Duration,
    mut progress: impl FnMut(ProbeOutcome),
) -> Result<SpeedProbeResult> {
    let client = handshake_only(
        host,
        port,
        // NOT 0. `bitrate_kbps == 0` is what arms punktfunk-core's OWN startup
        // link-capacity probe (`client/pump/data.rs`: 2 Gbps for 800ms, ~2s after
        // connect) — and core has exactly one `ProbeState` slot with no correlation id,
        // which our `request_probe` below would be sharing with it. Core defers its
        // probe while ours is active, but the reverse race (its probe landing just as
        // ours finishes and resetting the state we're about to read) is real. Pinning a
        // rate disarms core's probe entirely; the value is irrelevant since nothing is
        // decoded here.
        PROBE_SESSION_BITRATE_KBPS,
        quic::CODEC_HEVC | quic::CODEC_H264,
        pin,
        identity,
        timeout,
    )
    .context("speed test connect")?;

    // The negotiated session, logged before the burst: if a measurement comes back
    // empty, this line is what says whether the connection itself was sane.
    tracing::info!(
        "speed test connected: codec={} audio_ch={} resolved_bitrate_kbps={} caps=0x{:02x}",
        client.codec,
        client.audio_channels,
        client.resolved_bitrate_kbps,
        quic::VIDEO_CAP_CHACHA20,
    );

    // Don't burst into a dead plane — see PROBE_WARMUP_CAP. `next_frame` drains the session's
    // decode-less video into the void; the first completed frame is the "plane is live" edge.
    let warmup = Instant::now();
    let mut warmed = false;
    while warmup.elapsed() < PROBE_WARMUP_CAP {
        if client.next_frame(Duration::from_millis(250)).is_ok() {
            warmed = true;
            break;
        }
    }
    tracing::info!(
        "speed test: data plane {} after {} ms",
        if warmed {
            "live"
        } else {
            "still silent (proceeding anyway)"
        },
        warmup.elapsed().as_millis(),
    );

    client
        .request_probe(PROBE_TARGET_KBPS, PROBE_DURATION_MS)
        .context("request_probe")?;
    // Flip the UI from "Connecting…" to "Measuring…" the moment the burst is requested —
    // with the warmup above, the first 250 ms poll is no longer the earliest signal.
    progress(client.probe_result());

    let deadline = Instant::now() + Duration::from_millis(u64::from(PROBE_DURATION_MS)) + PROBE_REPORT_GRACE;
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let outcome = client.probe_result();
        if outcome.done {
            // Let the last in-flight UDP shards land before tearing the connection
            // down, so the delivered-bytes figure isn't cut short by our own exit.
            std::thread::sleep(Duration::from_millis(400));
            let final_outcome = client.probe_result();
            // Both sides of the measurement, separately. This is the line that tells a
            // host-side problem from a client-side one: `host_bytes == 0` means the host
            // never put filler on the wire (it ignored or couldn't serve the request),
            // whereas `host_bytes > 0` with `recv_bytes == 0` means it sent and we
            // received nothing usable — a network path or a decrypt mismatch, since
            // punktfunk-core counts bytes only AFTER a successful AEAD open.
            tracing::info!(
                "speed test result: recv_bytes={} recv_packets={} host_bytes={} host_packets={} \
                 elapsed_ms={} throughput_kbps={} loss_pct={:.2} host_drop_pct={:.2} \
                 wire_packets_sent={} send_dropped={}",
                final_outcome.recv_bytes,
                final_outcome.recv_packets,
                final_outcome.host_bytes,
                final_outcome.host_packets,
                final_outcome.elapsed_ms,
                final_outcome.throughput_kbps,
                final_outcome.loss_pct,
                final_outcome.host_drop_pct,
                final_outcome.wire_packets_sent,
                final_outcome.send_dropped,
            );
            client.disconnect_quit();
            return Ok(SpeedProbeResult {
                outcome: final_outcome,
                confirmed: true,
            });
        }
        progress(outcome);
        if Instant::now() > deadline {
            // The report never came — but `recv_bytes` is live during the burst (core
            // computes it as `rx_now - base`), so if a real amount of filler arrived the
            // measurement is not lost: divide it by the burst window we asked for. The
            // host honours that duration exactly when it does report (confirmed
            // on-device: a 3,000 ms request came back as `elapsed_ms=3000`), so this is
            // the same denominator, just not host-attested. Only the loss figure is
            // genuinely unavailable, since that needs the host's sent-packet count.
            let mut salvaged = client.probe_result();
            client.disconnect_quit();
            if salvaged.recv_bytes >= SALVAGE_MIN_BYTES {
                salvaged.elapsed_ms = PROBE_DURATION_MS;
                salvaged.throughput_kbps =
                    (salvaged.recv_bytes.saturating_mul(8) / u64::from(PROBE_DURATION_MS)) as u32;
                tracing::warn!(
                    "speed test: no host report; salvaged from {} received bytes over the {} ms \
                     burst window -> {} kbps (unconfirmed)",
                    salvaged.recv_bytes,
                    PROBE_DURATION_MS,
                    salvaged.throughput_kbps,
                );
                return Ok(SpeedProbeResult {
                    outcome: salvaged,
                    confirmed: false,
                });
            }
            anyhow::bail!(
                "the host never sent its result, and almost nothing arrived. The test burst can \
                 saturate the link the result has to come back over — try again, or move the TV \
                 closer to the access point."
            );
        }
    }
}
