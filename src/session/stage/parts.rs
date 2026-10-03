//! Slice-progressive reassembly: which pieces of an access unit may reach the decoder, and when
//! one has died mid-flight. See [`AuParts`] for the contract this enforces.

use super::WireFrame;

/// What [`AuParts`] decided about one delivery.
pub(super) enum PartStep {
    /// Hand the bytes to the sink. `partial` = this is not the AU's last piece; `lost_parts` = an
    /// earlier AU died mid-flight, so decoding cannot continue from where it stopped.
    Feed { partial: bool, lost_parts: bool },
    /// Nothing usable — a piece of an AU already abandoned. The decoder must not see it.
    Discard,
}

/// Slice-progressive reassembly bookkeeping (`punktfunk_core::session::FramePart`).
///
/// On a backend that takes them (`VideoSinkCaps::partial_au`), AU prefixes arrive while the rest is
/// still on the wire and the decoder gets a frame's first bytes without waiting for its last
/// datagram — a real slice of a frame period at high bitrate, and pure latency: none of that wait
/// is decode work. No NDL generation takes them (both report `partial_au: false`, and `connect`
/// keeps `frame_parts` off), so every delivery carries `part: None` and this is a pass-through.
///
/// The contract enforced here is core's: parts arrive in order with no gaps, BUT the pre-decode
/// hand-off may drop entries (memory pressure, a jump-to-live clear), so an `offset` that isn't the
/// open AU's next expected byte means that AU is gone. There is no abort marker — a `first` part for
/// a new index while one is still open is how a death is signalled. Both cases abandon the AU and
/// report loss, which puts the sink into freeze-until-reanchor and asks the host for a keyframe:
/// the decoder is holding a truncated input, and nothing short of a fresh anchor clears it.
#[derive(Default)]
pub(super) struct AuParts {
    /// `(frame_index, next expected byte offset)` of the AU currently being fed.
    open: Option<(u32, u32)>,
    /// An AU was abandoned and nothing has restarted decoding since — so whatever comes next is
    /// resuming against a decoder that still holds a truncated frame.
    abandoned: bool,
}

impl AuParts {
    pub(super) fn step(&mut self, frame: &WireFrame<'_>, takes_parts: bool) -> PartStep {
        let Some(part) = frame.part.filter(|_| takes_parts) else {
            // Whole-AU delivery: parts weren't negotiated, this backend doesn't take them, or
            // this is an aged-out chunk-aligned partial — core hands all three over as one buffer.
            self.open = None;
            return PartStep::Feed {
                partial: false,
                lost_parts: std::mem::take(&mut self.abandoned),
            };
        };
        let len = frame.data.len() as u32;
        if part.first {
            let lost_parts = self.open.take().is_some() | std::mem::take(&mut self.abandoned);
            if lost_parts {
                tracing::warn!("frame parts: AU {} starts over an unfinished one", frame.index);
            }
            self.open = (!part.last).then_some((frame.index, len));
            return PartStep::Feed {
                partial: !part.last,
                lost_parts,
            };
        }
        match self.open {
            Some((index, next)) if index == frame.index && next == part.offset => {
                self.open = (!part.last).then_some((index, next + len));
                PartStep::Feed {
                    partial: !part.last,
                    lost_parts: false,
                }
            }
            // Either nothing is open (the AU was abandoned, or this part arrived without its head)
            // or the offset skipped — both mean the AU can never be completed.
            open => {
                if open.is_some() {
                    tracing::warn!(
                        "frame parts: AU {} broke at offset {} — abandoning",
                        frame.index,
                        part.offset,
                    );
                }
                self.drop_open();
                PartStep::Discard
            }
        }
    }

    /// Forget the AU in flight: its remaining parts are no longer feedable, and whatever restarts
    /// decoding has to be told the decoder holds a truncated input.
    pub(super) fn drop_open(&mut self) {
        self.abandoned |= self.open.take().is_some();
    }
}
