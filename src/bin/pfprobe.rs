//! Headless speed probe CLI (mirrors `session::probe::run_speed_probe`). Run on-device over SSH to test connection.
//! Usage: pfprobe <host> <port> <cert.pem> <key.pem> <pin-hex-64> [`target_kbps`] [`duration_ms`]

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    real::main()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("pfprobe targets webOS (armv7 linux) — build with the cross toolchain");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod real {
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use punktfunk_core::client::NativeClient;
    use punktfunk_core::config::Mode;
    use punktfunk_core::quic;

    /// Non-zero to avoid triggering core's startup probe (`bitrate_kbps` == 0 would).
    const PROBE_SESSION_BITRATE_KBPS: u32 = 20_000;
    const PROBE_REPORT_GRACE: Duration = Duration::from_secs(12);

    fn parse_pin(hex: &str) -> Result<[u8; 32]> {
        anyhow::ensure!(hex.len() == 64, "pin must be 64 hex chars");
        let mut pin = [0u8; 32];
        for (i, byte) in pin.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).with_context(|| format!("bad hex at byte {i}"))?;
        }
        Ok(pin)
    }

    pub fn main() -> Result<()> {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .with_target(false)
            .init();

        let args: Vec<String> = std::env::args().collect();
        anyhow::ensure!(
            args.len() >= 6,
            "usage: pfprobe <host> <port> <cert.pem> <key.pem> <pin-hex-64> [target_kbps] [duration_ms]"
        );
        let host = &args[1];
        let port: u16 = args[2].parse().context("port")?;
        let cert = std::fs::read_to_string(&args[3]).context("read cert")?;
        let key = std::fs::read_to_string(&args[4]).context("read key")?;
        let pin = parse_pin(&args[5])?;
        let target_kbps: u32 = args.get(6).map_or(Ok(320_000), |s| s.parse()).context("target_kbps")?;
        let duration_ms: u32 = args.get(7).map_or(Ok(3_000), |s| s.parse()).context("duration_ms")?;

        let mode = Mode {
            width: 1280,
            height: 720,
            refresh_hz: 60,
        };
        let client = NativeClient::connect(punktfunk_core::client::ConnectParams {
            bitrate_kbps: PROBE_SESSION_BITRATE_KBPS,
            video_caps: quic::VIDEO_CAP_CHACHA20,
            video_codecs: quic::CODEC_HEVC | quic::CODEC_H264,
            pin: Some(pin),
            identity: Some((cert, key)),
            ..punktfunk_core::client::ConnectParams::new(host, port, mode, Duration::from_secs(10))
        })
        .context("connect")?;
        println!(
            "connected: codec={} audio_ch={} resolved_bitrate_kbps={}",
            client.codec, client.audio_channels, client.resolved_bitrate_kbps
        );

        // Wait for the data plane to actually deliver before bursting: on this TV's Wi-Fi a NEW
        // host→client UDP flow is black-holed for ~10-14 s (AP/driver queue setup), and a burst
        // fired into that window measures the black hole, not the link. The first completed video
        // frame is the "plane is live" edge; print how long it took (that IS the black-hole span).
        let warm_start = Instant::now();
        let mut warmed = false;
        for _ in 0..300 {
            if client.next_frame(Duration::from_millis(100)).is_ok() {
                warmed = true;
                break;
            }
        }
        println!(
            "data-plane warmup: first_frame_after_ms={} warmed={warmed}",
            warm_start.elapsed().as_millis()
        );

        client
            .request_probe(target_kbps, duration_ms)
            .context("request_probe")?;
        let started = Instant::now();
        let deadline = started + Duration::from_millis(u64::from(duration_ms)) + PROBE_REPORT_GRACE;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let outcome = client.probe_result();
            if outcome.done {
                std::thread::sleep(Duration::from_millis(400));
                let o = client.probe_result();
                println!(
                    "result: recv_bytes={} recv_packets={} host_bytes={} host_packets={} \
                     elapsed_ms={} throughput_kbps={} loss_pct={:.2} host_drop_pct={:.2} \
                     wire_packets_sent={} send_dropped={} confirmed=true",
                    o.recv_bytes,
                    o.recv_packets,
                    o.host_bytes,
                    o.host_packets,
                    o.elapsed_ms,
                    o.throughput_kbps,
                    o.loss_pct,
                    o.host_drop_pct,
                    o.wire_packets_sent,
                    o.send_dropped,
                );
                client.disconnect_quit();
                return Ok(());
            }
            println!(
                "progress: t_ms={} recv_bytes={} recv_packets={}",
                started.elapsed().as_millis(),
                outcome.recv_bytes,
                outcome.recv_packets
            );
            if Instant::now() > deadline {
                let o = client.probe_result();
                client.disconnect_quit();
                let kbps = o.recv_bytes.saturating_mul(8) / u64::from(duration_ms);
                println!(
                    "result: recv_bytes={} recv_packets={} throughput_kbps={kbps} confirmed=false \
                     (host report never arrived; derived over the requested burst window)",
                    o.recv_bytes, o.recv_packets,
                );
                return Ok(());
            }
        }
    }
}
