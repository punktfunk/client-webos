//! Carries NDL's POC across a recovery anchor.
//!
//! NDL derives each picture's POC MSB from the picture fed before it. A hold feeds nothing, so an
//! anchor more than half the lsb range past the last fed picture lands a wrap low: its long-term
//! reference resolves to nothing and the picture smears until the next refresh. AMF codes a 4-bit
//! lsb, which makes that nine frames.
//!
//! A bridge is the anchor with its POC and long-term MSB cycles rewritten to a point in between.
//! It names the same references and keeps the anchor's slice data, so it decodes to the anchor's
//! own picture, and each one walks NDL's POC half a range closer to the real anchor.

use std::io::Cursor;
use std::ops::Range;
use std::rc::Rc;

use cros_codecs::codec::h265::parser::{Nalu, NaluType, Parser, Pps, SliceHeader};

/// Bridges one anchor may take. Each costs a decode, and a keyframe is due long before this many.
const MAX_BRIDGES: i64 = 32;

const IDR_W_RADL: u8 = 19;
const IDR_N_LP: u8 = 20;
const SPS_NUT: u8 = 33;
const PPS_NUT: u8 = 34;

/// Enough of a slice NAL to reach `slice_pic_order_cnt_lsb` of an AU's first slice.
const SLICE_HEAD: usize = 16;

/// What NDL's POC derivation holds, mirrored from every AU it takes.
#[derive(Default)]
pub(super) struct PocBridge {
    parser: Parser,
    /// POC of the last picture NDL took. `None` until an IDR sets it.
    poc: Option<i64>,
    /// Host index of that picture.
    fed: u64,
}

impl PocBridge {
    /// Run on every AU NDL took. Reads no further than the AU's first slice header.
    pub(super) fn note_fed(&mut self, au: &[u8], index: u64) {
        self.fed = index;
        for (at, _) in au.windows(3).enumerate().filter(|(_, w)| *w == [0, 0, 1]) {
            let Some(nal) = au.get(at + 3..) else { return };
            match nal.first().map_or(0, |b| (b >> 1) & 0x3f) {
                IDR_W_RADL | IDR_N_LP => {
                    self.poc = Some(0);
                    return;
                }
                0..=31 => {
                    self.track(nal);
                    return;
                }
                SPS_NUT => {
                    if let Ok(nal) = Nalu::next(&mut Cursor::new(&au[at..])) {
                        let _ = self.parser.parse_sps(&nal);
                    }
                }
                PPS_NUT => {
                    if let Ok(nal) = Nalu::next(&mut Cursor::new(&au[at..])) {
                        let _ = self.parser.parse_pps(&nal);
                    }
                }
                _ => {}
            }
        }
    }

    fn track(&mut self, nal: &[u8]) {
        let rbsp = unescape(&nal[..nal.len().min(SLICE_HEAD)]);
        let Some(prev) = self.poc else { return };
        let Some((at, pps)) = lsb_position(&self.parser, &rbsp) else {
            return;
        };
        let bits = lsb_bits(&pps);
        let lsb = BitCursor { data: &rbsp, at }.bits(bits);
        self.poc = Some(derive_poc(prev, i64::from(lsb), 1 << bits));
    }

    /// The AUs to feed ahead of `anchor`, oldest first. Empty when it needs none, or when it has a
    /// shape this can't rewrite, and then it goes in as it is.
    pub(super) fn bridges(&mut self, anchor: &[u8], index: u64) -> Vec<Vec<u8>> {
        self.plan(anchor, index).unwrap_or_else(|why| {
            tracing::warn!("no POC bridge for frame {index}: {why}");
            Vec::new()
        })
    }

    fn plan(&mut self, anchor: &[u8], index: u64) -> Result<Vec<Vec<u8>>, &'static str> {
        let Some(prev) = self.poc else { return Ok(Vec::new()) };
        let mut units = Vec::new();
        let mut cursor = Cursor::new(anchor);
        while let Ok(nal) = Nalu::next(&mut cursor) {
            units.push(if (nal.header.type_ as u32) < 32 {
                let slice = self
                    .parser
                    .parse_slice_header(nal)
                    .map_err(|_| "unreadable slice header")?;
                (slice.nalu, Some(slice.header))
            } else {
                (nal, None)
            });
        }
        let Some((nal, Some(first))) = units.iter().find(|(_, header)| header.is_some()) else {
            return Ok(Vec::new());
        };
        // Only a TRAIL_R at temporal layer 0 moves the POC a later picture derives from.
        if nal.header.type_ != NaluType::TrailR || nal.header.nuh_temporal_id() != 0 {
            return Ok(Vec::new());
        }
        let pps = Rc::clone(self.parser.get_pps(first.pic_parameter_set_id).ok_or("no PPS")?);
        let sps = &pps.sps;
        let bits = lsb_bits(&pps);
        let half = 1i64 << (bits - 1);
        let frames = i64::try_from(index.saturating_sub(self.fed)).map_err(|_| "frame index jumped")?;
        let poc = derive_poc(prev + frames, i64::from(first.pic_order_cnt_lsb), half * 2);
        let gap = poc - prev;
        if gap <= half {
            return Ok(Vec::new());
        }
        let count = (gap - 1) / half;
        if count > MAX_BRIDGES {
            return Err("gap too long");
        }
        let rps = &first.short_term_ref_pic_set;
        if first.short_term_ref_pic_set_sps_flag || rps.num_negative_pics + rps.num_positive_pics > 0 {
            return Err("short-term references");
        }
        if !sps.long_term_ref_pics_present_flag || sps.num_long_term_ref_pics_sps > 0 {
            return Err("long-term references this can't name");
        }
        let bridges = (1..=count)
            .rev()
            .map(|k| {
                let mut au = Vec::with_capacity(anchor.len());
                for (nal, header) in &units {
                    au.extend_from_slice(&[0, 0, 0, 1]);
                    match header {
                        Some(header) if !header.dependent_slice_segment_flag => {
                            rebase(nal.as_ref(), header, &self.parser, poc, k * half, &mut au)?
                        }
                        _ => au.extend_from_slice(nal.as_ref()),
                    }
                }
                Ok(au)
            })
            .collect::<Result<Vec<_>, &'static str>>()?;
        tracing::info!("POC gap {gap} at frame {index}: {count} bridge(s) ahead of the anchor");
        self.poc = Some(poc - half);
        Ok(bridges)
    }
}

fn lsb_bits(pps: &Pps) -> u32 {
    u32::from(pps.sps.log2_max_pic_order_cnt_lsb_minus4) + 4
}

/// Equation 8-1: the POC a decoder gives `lsb` right after a picture at `prev`. Also the POC
/// carrying `lsb` nearest to `prev`.
fn derive_poc(prev: i64, lsb: i64, max: i64) -> i64 {
    let forward = (lsb - prev).rem_euclid(max);
    if forward <= max / 2 {
        prev + forward
    } else {
        prev + forward - max
    }
}

/// Bit offset of `slice_pic_order_cnt_lsb` in a slice's unescaped NAL, and the PPS it names.
fn lsb_position(parser: &Parser, rbsp: &[u8]) -> Option<(usize, Rc<Pps>)> {
    let irap = (16..=23).contains(&(rbsp.first()? >> 1 & 0x3f));
    let mut r = BitCursor { data: rbsp, at: 16 };
    let first_in_pic = r.bits(1) == 1;
    r.at += usize::from(irap);
    let pps = u8::try_from(r.ue()).ok()?;
    let pps = Rc::clone(parser.get_pps(pps)?);
    if !first_in_pic {
        r.at += usize::from(pps.dependent_slice_segments_enabled_flag);
        r.at += (u32::BITS - (pps.sps.pic_size_in_ctbs_y - 1).leading_zeros()) as usize;
    }
    r.at += usize::from(pps.num_extra_slice_header_bits);
    r.ue(); // slice_type
    r.at += usize::from(pps.output_flag_present_flag);
    r.at += 2 * usize::from(pps.sps.separate_colour_plane_flag);
    Some((r.at, pps))
}

/// Appends `nal` as a picture `back` POCs before the anchor at `poc`. Everything between the lsb
/// and the long-term block is copied: the caller vouched that the anchor has no short-term set.
fn rebase(
    nal: &[u8],
    header: &SliceHeader,
    parser: &Parser,
    poc: i64,
    back: i64,
    out: &mut Vec<u8>,
) -> Result<(), &'static str> {
    // The stop bit keeps the header's last byte non-zero, so slice data keeps its own escaping.
    let raw_end = header.header_bit_size as usize / 8 + header.n_emulation_prevention_bytes as usize;
    let rbsp = unescape(nal.get(..raw_end).ok_or("short slice")?);
    let (lsb_at, pps) = lsb_position(parser, &rbsp).ok_or("unreadable slice header")?;
    let bits = lsb_bits(&pps);
    let long_term_at = lsb_at + bits as usize + 1 + header.st_rps_bits as usize;
    let mut r = BitCursor {
        data: &rbsp,
        at: long_term_at,
    };

    // Wraps between this picture and the anchor, which every MSB cycle sheds.
    let wraps = (poc >> bits) - ((poc - back) >> bits);
    let mut entries = Vec::new();
    let (mut cycle, mut named) = (0, 0);
    for _ in 0..r.ue() {
        let lsb = r.bits(bits);
        let used = r.bits(1);
        if r.bits(1) == 0 {
            entries.push((lsb, used, None));
            continue;
        }
        cycle += i64::from(r.ue());
        match cycle - wraps {
            kept if kept >= 0 => {
                entries.push((lsb, used, Some(kept - named)));
                named = kept;
            }
            // Marked inside the gap: NDL never had it, and this picture comes before it.
            _ if used == 0 => {}
            _ => return Err("anchor predicts from a picture after the bridge"),
        }
    }
    let tail = r.at;
    let stop = (tail..rbsp.len() * 8)
        .rfind(|&at| (BitCursor { data: &rbsp, at }).bits(1) == 1)
        .ok_or("no stop bit")?;

    let mut w = BitSink::default();
    w.copy(&rbsp, 0..lsb_at);
    w.put(bits, ((poc - back) & ((1 << bits) - 1)) as u32);
    w.copy(&rbsp, lsb_at + bits as usize..long_term_at);
    w.ue(entries.len() as u32);
    for (lsb, used, delta) in entries {
        w.put(bits, lsb);
        w.put(1, used);
        w.put(1, u32::from(delta.is_some()));
        if let Some(delta) = delta {
            w.ue(delta as u32);
        }
    }
    w.copy(&rbsp, tail..stop);
    w.put(1, 1);
    w.put(((8 - w.len % 8) % 8) as u32, 0);
    escape(&w.bytes, out);
    out.extend_from_slice(&nal[raw_end..]);
    Ok(())
}

fn unescape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut zeros = 0;
    for &byte in raw {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
    out
}

fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if byte == 0 { zeros + 1 } else { 0 };
        out.push(byte);
    }
}

/// Reads past the end as zeros: the parser has already bounded every header this walks.
struct BitCursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl BitCursor<'_> {
    fn bits(&mut self, count: u32) -> u32 {
        (0..count).fold(0, |value, _| {
            let bit = self.data.get(self.at / 8).map_or(0, |b| b >> (7 - self.at % 8) & 1);
            self.at += 1;
            value << 1 | u32::from(bit)
        })
    }

    fn ue(&mut self) -> u32 {
        let mut zeros = 0;
        while zeros < 32 && self.bits(1) == 0 {
            zeros += 1;
        }
        ((1u64 << zeros) - 1 + u64::from(self.bits(zeros))) as u32
    }
}

#[derive(Default)]
struct BitSink {
    bytes: Vec<u8>,
    len: usize,
}

impl BitSink {
    fn put(&mut self, count: u32, value: u32) {
        for shift in (0..count).rev() {
            if self.len % 8 == 0 {
                self.bytes.push(0);
            }
            if let Some(last) = self.bytes.last_mut() {
                *last |= ((value >> shift & 1) as u8) << (7 - self.len % 8);
            }
            self.len += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let code = u64::from(value) + 1;
        let width = 64 - code.leading_zeros();
        self.put(width - 1, 0);
        for shift in (0..width).rev() {
            self.put(1, (code >> shift & 1) as u32);
        }
    }

    fn copy(&mut self, from: &[u8], range: Range<usize>) {
        for at in range {
            let bit = BitCursor { data: from, at }.bits(1);
            self.put(1, bit);
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    // An AMF 1080p stream's parameter sets, and the head of its recovery anchor at POC 120:
    // long-term references at 96 (used, cycle 1) and 64 (kept, cycle 3), no short-term set.
    const SPS: &[u8] = &[
        0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0xb0, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x9c,
        0xa0, 0x02, 0x80, 0x80, 0x32, 0x17, 0x13, 0x92, 0x42, 0x95, 0x92, 0xf9, 0xff, 0x80, 0x00, 0x80, 0x00, 0xb7,
        0x01, 0x01, 0x01, 0x04, 0x00, 0x00, 0x03, 0x00, 0x04, 0x00, 0x00, 0x03, 0x01, 0x03, 0x10,
    ];
    const PPS: &[u8] = &[0x44, 0x01, 0xe0, 0xf7, 0xc0, 0x4c, 0x90];
    const ANCHOR: &[u8] = &[
        0x02, 0x01, 0xd4, 0x1b, 0x0d, 0x02, 0xc2, 0x09, 0x60, 0x83, 0x84, 0x60, 0xff, 0xc5, 0x55, 0x51, 0x9a, 0xbc,
        0x71, 0x6d, 0x36, 0xeb, 0xb1, 0xd3, 0x6d, 0xd9, 0x19, 0x96, 0x61, 0x70, 0x2c, 0xcd, 0xca, 0xab, 0x7c, 0x21,
        0xa2, 0xe5, 0x07, 0xc1, 0x7b, 0xd2, 0x9e, 0x9e, 0x56, 0x11, 0xe0, 0xdb,
    ];
    const ANCHOR_POC: u64 = 120;

    fn annexb(nal: &[u8]) -> Vec<u8> {
        [&[0, 0, 0, 1], nal].concat()
    }

    pub(in crate::session::stage) fn anchor_au() -> Vec<u8> {
        annexb(ANCHOR)
    }

    /// A mirror whose last fed picture is at `poc`, with frame index equal to POC.
    pub(in crate::session::stage) fn bridge_at(poc: i64) -> PocBridge {
        let mut bridge = PocBridge::default();
        bridge.note_fed(&[annexb(SPS), annexb(PPS)].concat(), poc as u64);
        bridge.poc = Some(poc);
        bridge
    }

    fn header(au: &[u8]) -> SliceHeader {
        let mut parser = Parser::default();
        let sets = [annexb(SPS), annexb(PPS)].concat();
        let mut cursor = Cursor::new(&sets[..]);
        let sps = Nalu::next(&mut cursor).expect("SPS");
        parser.parse_sps(&sps).expect("SPS");
        let pps = Nalu::next(&mut cursor).expect("PPS");
        parser.parse_pps(&pps).expect("PPS");
        let nal = Nalu::next(&mut Cursor::new(au)).expect("slice");
        parser.parse_slice_header(nal).expect("header").header
    }

    /// `(POC lsb, (used, MSB cycle) per long-term entry)`, as a decoder reads this AU.
    fn read(au: &[u8]) -> (u16, Vec<(bool, u32)>) {
        let header = header(au);
        let entries = (0..usize::from(header.num_long_term_pics))
            .map(|i| (header.used_by_curr_pic_lt[i], header.delta_poc_msb_cycle_lt[i]))
            .collect();
        (header.pic_order_cnt_lsb, entries)
    }

    #[test]
    fn an_anchor_within_half_the_range_needs_no_bridge() {
        let mut bridge = bridge_at(112);
        assert!(bridge.bridges(&anchor_au(), ANCHOR_POC).is_empty());
        bridge.note_fed(&anchor_au(), ANCHOR_POC);
        assert_eq!(bridge.poc, Some(120));
    }

    /// Nine frames is the first gap the 4-bit lsb can't span.
    #[test]
    fn one_frame_past_half_the_range_takes_one_bridge() {
        let mut bridge = bridge_at(111);
        let bridges = bridge.bridges(&anchor_au(), ANCHOR_POC);
        assert_eq!(bridges.len(), 1);
        assert_eq!(read(&bridges[0]), (0, vec![(true, 1), (false, 3)]));
        bridge.note_fed(&anchor_au(), ANCHOR_POC);
        assert_eq!(bridge.poc, Some(120));
    }

    #[test]
    fn bridges_walk_the_poc_and_shed_a_cycle_per_wrap() {
        let mut bridge = bridge_at(97);
        let bridges = bridge.bridges(&anchor_au(), ANCHOR_POC);
        let read: Vec<_> = bridges.iter().map(|au| read(au)).collect();
        assert_eq!(
            read,
            [(8, vec![(true, 0), (false, 2)]), (0, vec![(true, 1), (false, 3)])],
            "POC 104, then POC 112"
        );
        bridge.note_fed(&anchor_au(), ANCHOR_POC);
        assert_eq!(bridge.poc, Some(120));
    }

    #[test]
    fn a_bridge_keeps_the_anchors_slice_data() {
        let bridges = bridge_at(97).bridges(&anchor_au(), ANCHOR_POC);
        let data = &ANCHOR[header(&anchor_au()).header_bit_size as usize / 8..];
        assert!(bridges.iter().all(|au| au.ends_with(data)));
    }

    /// A kept reference newer than the bridge never reached NDL, and the bridge can't name it.
    #[test]
    fn a_kept_reference_marked_in_the_gap_is_dropped() {
        let mut swapped = ANCHOR.to_vec();
        // The two long-term entries' used flags.
        for at in [36, 45] {
            swapped[at / 8] ^= 1 << (7 - at % 8);
        }
        let bridges = bridge_at(81).bridges(&annexb(&swapped), ANCHOR_POC);
        assert_eq!(bridges.len(), 4);
        assert_eq!(read(&bridges[0]), (8, vec![(true, 1)]), "POC 88 names only POC 64");
        assert_eq!(read(&bridges[1]), (0, vec![(false, 0), (true, 2)]), "POC 96");
    }

    /// The host named a reference NDL can't have. The anchor goes in as it is.
    #[test]
    fn an_anchor_used_reference_inside_the_gap_takes_no_bridge() {
        let mut bridge = bridge_at(81);
        assert!(bridge.bridges(&anchor_au(), ANCHOR_POC).is_empty());
        assert_eq!(bridge.poc, Some(81));
    }

    /// The mirror follows NDL into a wrap as well: an unbridged anchor lands a wrap low.
    #[test]
    fn the_mirror_derives_what_the_decoder_derives() {
        let mut bridge = bridge_at(111);
        bridge.note_fed(&anchor_au(), ANCHOR_POC);
        assert_eq!(bridge.poc, Some(104));
        assert_eq!(derive_poc(112, 8, 16), 120);
        assert_eq!(derive_poc(7, 0, 16), 0);
        assert_eq!(derive_poc(7, 15, 16), 15);
    }

    #[test]
    fn escaping_round_trips() {
        let rbsp = [0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7, 0, 0, 4];
        let mut raw = Vec::new();
        escape(&rbsp, &mut raw);
        assert!(!raw.windows(3).any(|w| w == [0, 0, 1] || w == [0, 0, 0]));
        assert_eq!(unescape(&raw), rbsp);
    }

    #[test]
    fn exp_golomb_round_trips() {
        let mut w = BitSink::default();
        let values = [0, 1, 2, 3, 6, 7, 255, 65_535];
        values.iter().for_each(|&v| w.ue(v));
        let mut r = BitCursor { data: &w.bytes, at: 0 };
        assert_eq!(values.map(|_| r.ue()), values);
    }
}
