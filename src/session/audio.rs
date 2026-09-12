//! The audio stage: transport packets → whatever the session's [`AudioSink`] takes.
//!
//! One implementation covers both routes. What differs between them is the sink's declared
//! [`AudioFormat`], and the stage produces exactly that: stereo Opus goes through untouched where
//! the TV decodes it, 5.1 is re-encoded into the one layout NDL decodes ([`OPUS_51_LAYOUT`]), and
//! libopus decodes here where the SDL device plays it — with concealment, written once into a
//! reused buffer.
//!
//! **Nothing is mixed down.** A layout the selected route cannot carry is not requested from the
//! host in the first place (`core::model::AudioRoutePref::max_channels`), so a width mismatch here
//! is a bug and is reported as one rather than folded away.
use std::sync::Arc;

use anyhow::{bail, Result};
use punktfunk_core::audio::{layout_for, AudioGapTracker};

use crate::core::media::{AudioFormat, AudioSink, Samples};
use crate::platform::webos::ndl::OPUS_51_LAYOUT;

/// 48 kHz, 5 ms frames — punktfunk's fixed audio framing (see punktfunk-core's audio.rs docs and
/// its `multistream_layout_roundtrips_with_channel_identity` test, the canonical reference for
/// both ends of this wire format).
pub const SAMPLE_RATE: u32 = 48_000;
const SAMPLES_PER_FRAME: usize = 240;
/// Duration of one packet, in ms — the framing above, as the concealment arithmetic needs it.
const FRAME_MS: i64 = 5;
/// Max channels punktfunk ever negotiates (7.1) — sizes the scratch decode buffer.
const MAX_CHANNELS: usize = 8;
/// Room for one re-encoded 5 ms packet: 320 bytes at [`OPUS_51_LAYOUT`]'s bitrate.
const MAX_PACKET: usize = 4000;

/// Decodes (or forwards) one session's audio into its sink.
pub struct AudioStage {
    sink: Arc<dyn AudioSink>,
    /// `None` where the TV takes the wire's stereo Opus as-is and this stage only forwards.
    decoder: Option<opus::MSDecoder>,
    /// 5.1 offload only: re-encodes every decoded frame into [`OPUS_51_LAYOUT`].
    reencoder: Option<opus::MSEncoder>,
    /// The re-encoder's output, one packet at a time.
    packet: Vec<u8>,
    /// Negotiated channel count — the decode width, and what libopus sizes a frame by.
    channels: usize,
    /// Detects packets lost on the wire so they can be concealed rather than skipped.
    gaps: AudioGapTracker,
    /// Reused across packets: concealment frames first, then the packet itself.
    f32: Vec<f32>,
    /// libopus's own output buffer, one frame at the widest layout. A field rather than a local:
    /// as a local it is a 7.7 KB stack array zeroed on every packet, i.e. 200 pointless memsets a
    /// second on a soft-float `SoC`. libopus overwrites what it uses, so the stale contents of the
    /// tail are never read.
    pcm: Box<[f32; SAMPLES_PER_FRAME * MAX_CHANNELS]>,
}

impl AudioStage {
    /// `channels` is host-resolved — the decoder MUST be built from what the handshake settled on,
    /// never from what was requested.
    pub fn new(sink: Arc<dyn AudioSink>, channels: u8) -> Result<Self> {
        let format = sink.format();
        if format.channels() != channels {
            // Not a fold: see the module docs. A route is only ever selected for a layout it
            // carries, so this is the caps wiring being wrong, and it must be loud.
            bail!(
                "{} takes {} channel(s), the session negotiated {channels}",
                sink.name(),
                format.channels(),
            );
        }
        let layout = layout_for(channels, false);
        let decoder = || {
            opus::MSDecoder::new(SAMPLE_RATE, layout.streams, layout.coupled, layout.mapping)
                .map_err(|e| anyhow::anyhow!("opus MSDecoder::new: {e}"))
        };
        let (decoder, reencoder) = match format {
            // The wire's stereo Opus is exactly what NDL's stereo plane decodes.
            AudioFormat::Opus { channels: 2 } => (None, None),
            // The wire couples (FC,LFE); NDL decodes only a layout that couples (RL,RR).
            AudioFormat::Opus { channels } if channels == OPUS_51_LAYOUT.channels => {
                (Some(decoder()?), Some(reencoder_51()?))
            }
            AudioFormat::Opus { channels } => bail!("NDL's Opus plane takes stereo or 5.1, not {channels} channels"),
            AudioFormat::PcmF32 { .. } => (Some(decoder()?), None),
        };
        Ok(Self {
            sink,
            decoder,
            reencoder,
            packet: vec![0; MAX_PACKET],
            channels: layout.channels as usize,
            gaps: AudioGapTracker::new(),
            // One packet plus the concealment burst that can precede it, so steady state never
            // reallocates.
            f32: Vec::with_capacity(SAMPLES_PER_FRAME * MAX_CHANNELS * 2),
            pcm: Box::new([0.0; SAMPLES_PER_FRAME * MAX_CHANNELS]),
        })
    }

    pub fn sink_name(&self) -> &'static str {
        self.sink.name()
    }

    /// One packet, concealment included, into the sink.
    ///
    /// Concealment sits in the hole BEFORE the packet, so its frames are stamped that many
    /// milliseconds earlier than the packet's own stamp. The SDL device takes them as one buffer;
    /// NDL takes one Opus packet per call, so a re-encoded frame is fed on its own.
    pub fn play(&mut self, seq: u32, pts_ns: u64, payload: &[u8]) -> Result<()> {
        // Destructured so the decode loop can hold `decoder`, `pcm` and `f32` at once.
        let Self {
            decoder,
            reencoder,
            packet,
            f32,
            pcm,
            channels,
            gaps,
            sink,
        } = self;
        let Some(decoder) = decoder.as_mut() else {
            // The TV decodes: concealment, layout and framing are all its business from here.
            return sink.feed(Samples::Opus(payload), pts_ns);
        };
        let channels = *channels;
        let missing = gaps.missing_before(seq);
        f32.clear();
        // A concealment frame gets one frame's worth of buffer, not the whole scratch: with no
        // packet to describe it libopus takes `out.len() / channels` as the frame size and
        // rejects an illegal one. 5.1 gives 1920/6 = 320.
        let cap = |i: u32| {
            if i < missing {
                SAMPLES_PER_FRAME * channels
            } else {
                SAMPLES_PER_FRAME * MAX_CHANNELS
            }
        };
        let stamp = |i: u32| pts_ns.saturating_sub((i64::from(missing - i) * FRAME_MS) as u64 * 1_000_000);
        // Concealment frames first (libopus PLC — decode with empty input interpolates a frame;
        // the alternative is a hard gap, i.e. a click), then the packet itself. `f32` is both what
        // libopus produces and what the SDL device takes, so there is no conversion pass there.
        for i in 0..=missing {
            let input: &[u8] = if i < missing { &[] } else { payload };
            let frames = decoder
                .decode_float(input, &mut pcm[..cap(i)], false)
                .map_err(|e| anyhow::anyhow!("opus decode: {e}"))?;
            let decoded = &pcm[..frames * channels];
            f32.extend_from_slice(decoded);
            if let Some(encoder) = reencoder.as_mut() {
                let n = encoder
                    .encode_float(decoded, packet)
                    .map_err(|e| anyhow::anyhow!("opus 5.1 re-encode: {e}"))?;
                sink.feed(Samples::Opus(&packet[..n]), stamp(i))?;
            }
        }
        if reencoder.is_some() {
            return Ok(());
        }
        sink.feed(Samples::F32(f32), stamp(0))
    }

    /// The sink's own queue depth in ms, where it knows one — NDL's plane lead, or the SDL ring's
    /// fill. The one figure that says which side of a late-audio report to look at.
    pub fn depth_ms(&self) -> Option<i64> {
        self.sink.depth_ms()
    }

    /// Peak sample of the last decoded buffer — a diagnostic that separates "the host is sending
    /// silence" from "the speaker is not working". `None` where stereo Opus is forwarded undecoded.
    pub fn peak(&self) -> Option<f32> {
        self.decoder
            .is_some()
            .then(|| self.f32.iter().fold(0f32, |m, &s| m.max(s.abs())))
    }
}

/// An encoder for [`OPUS_51_LAYOUT`]. `LowDelay` keeps it CELT-only, so the re-encode adds one
/// 5 ms frame and 2.5 ms of look-ahead.
fn reencoder_51() -> Result<opus::MSEncoder> {
    let l = OPUS_51_LAYOUT;
    let mut encoder = opus::MSEncoder::new(
        SAMPLE_RATE,
        l.streams,
        l.coupled,
        l.mapping,
        opus::Application::LowDelay,
    )
    .map_err(|e| anyhow::anyhow!("opus MSEncoder::new: {e}"))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(l.bitrate))
        .map_err(|e| anyhow::anyhow!("opus set_bitrate: {e}"))?;
    Ok(encoder)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Stands in for NDL's 5.1 plane: keeps every packet it is fed.
    struct Plane(Mutex<Vec<Vec<u8>>>);

    impl AudioSink for Plane {
        fn name(&self) -> &'static str {
            "test plane"
        }

        fn format(&self) -> AudioFormat {
            AudioFormat::Opus { channels: 6 }
        }

        fn feed(&self, samples: Samples<'_>, _host_pts_ns: u64) -> Result<()> {
            let Samples::Opus(packet) = samples else {
                bail!("the plane takes Opus only");
            };
            self.0.lock().unwrap().push(packet.to_vec());
            Ok(())
        }
    }

    /// A tone on the wire's rear-left comes out rear-left in NDL's layout: the re-encode moves
    /// streams, never channels.
    #[test]
    fn reencode_keeps_channel_identity() {
        const REAR_LEFT: usize = 4;
        let wire = layout_for(6, false);
        let mut wire_encoder = opus::MSEncoder::new(
            SAMPLE_RATE,
            wire.streams,
            wire.coupled,
            wire.mapping,
            opus::Application::LowDelay,
        )
        .unwrap();
        let plane = Arc::new(Plane(Mutex::new(Vec::new())));
        let mut stage = AudioStage::new(plane.clone(), 6).unwrap();

        let mut phase = 0f32;
        let mut packet = [0u8; MAX_PACKET];
        for seq in 0..40 {
            let mut frame = vec![0f32; SAMPLES_PER_FRAME * 6];
            for sample in frame.chunks_exact_mut(6) {
                phase += 0.05;
                sample[REAR_LEFT] = phase.sin() * 0.5;
            }
            let len = wire_encoder.encode_float(&frame, &mut packet).unwrap();
            stage.play(seq, 0, &packet[..len]).unwrap();
        }

        let l = OPUS_51_LAYOUT;
        let mut ndl = opus::MSDecoder::new(SAMPLE_RATE, l.streams, l.coupled, l.mapping).unwrap();
        let mut out = vec![0f32; SAMPLES_PER_FRAME * 6];
        let mut energy = [0f32; 6];
        for (i, packet) in plane.0.lock().unwrap().iter().enumerate() {
            let frames = ndl.decode_float(packet, &mut out, false).unwrap();
            // The first frames carry both codecs' warm-up.
            if i >= 10 {
                for sample in out[..frames * 6].chunks_exact(6) {
                    for (e, v) in energy.iter_mut().zip(sample) {
                        *e += v * v;
                    }
                }
            }
        }
        let loudest = (0..6).max_by(|&a, &b| energy[a].total_cmp(&energy[b])).unwrap();
        assert_eq!(loudest, REAR_LEFT, "energy per channel: {energy:?}");
    }
}
