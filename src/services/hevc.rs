//! Synthetic HEVC Main10 pictures, for the HDR calibration patterns.
//!
//! Calibration has to put an *exact* PQ code value (see `core::pq`) on the panel, and the only way onto a webOS
//! video plane is an encoded bitstream. So this builds one — the smallest conformant encoder that
//! can express a flat image exactly.
//!
//! Every coding unit is a **PCM** CU: raw samples, no prediction, no transform, no residual, and
//! `pcm_loop_filter_disabled_flag` so the deblocker never touches them. What is written here is
//! what the panel is asked to show. The cost is size (~1 MB per frame), which is fine for
//! a still pattern re-fed a few times a second.
//!
//! The SPS pins `log2_min_luma_coding_block_size == log2_ctb_size == 32`, so `split_cu_flag` is
//! never coded and the whole arithmetic-coding surface is three bins per CTU: `part_mode`, then
//! `pcm_flag` and `end_of_slice_segment_flag`, both terminating bins. That is why the CABAC engine
//! below is ~60 lines rather than a few thousand.

/// Coded picture size — deliberately far below the panel's, and the video plane scales it up.
///
/// PCM is enormous (a 1080p frame is ~3.9 MB, ~310 Mb/s at the feed's cadence) and the patterns
/// are flat fields with hard edges, which survive scaling exactly where it matters: what is being
/// judged is a luminance, not a detail. Feeding a quarter of the samples is what keeps the
/// decoder's queue from backing up, and `NDL_DirectVideoPlay` blocks while it is full — a stall
/// there wedges the feed thread past its join deadline and poisons NDL for the next load.
///
/// 540 is not a multiple of the 32-sample minimum coding block, so the picture is coded 544 tall
/// and the extra 4 rows are cropped by the conformance window.
pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 540;
const CODED_HEIGHT: u32 = 544;

const CTB_LOG2: u32 = 5;
const CTB: u32 = 1 << CTB_LOG2;
const CTBS_X: u32 = WIDTH / CTB;
const CTBS_Y: u32 = CODED_HEIGHT / CTB;

/// Neutral chroma: the midpoint of the 10-bit range. Every pattern here is achromatic.
const NEUTRAL_CHROMA: u16 = 512;

/// A rectangle of constant luminance, placed in fractions of the picture (origin top-left). A
/// pattern is a background field plus a list of these, painted in order, so callers describe
/// what they want to see and never touch coded samples — the coded picture is this module's
/// business.
#[derive(Clone, Copy, Debug)]
pub struct Patch {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// 10-bit narrow-range luma code — see [`crate::core::pq::pq_code`].
    pub code: u16,
}

impl Patch {
    /// The patch in coded samples, the far edges exclusive. Clipped to the picture, and never
    /// smaller than one sample.
    fn rect(self) -> PatchRect {
        let scale = |v: f32, span: u32| (f64::from(v.clamp(0.0, 1.0)) * f64::from(span)).round() as u32;
        let x0 = scale(self.x, WIDTH).min(WIDTH - 1);
        let y0 = scale(self.y, HEIGHT).min(HEIGHT - 1);
        PatchRect {
            x0,
            y0,
            x1: (x0 + scale(self.w, WIDTH).max(1)).min(WIDTH),
            y1: (y0 + scale(self.h, HEIGHT).max(1)).min(HEIGHT),
            code: self.code,
        }
    }
}

/// Builds access units, reusing every buffer between frames.
///
/// A PCM frame is ~1 MB and the parameter sets never change, so an encoder is kept for
/// the life of the pattern feed: the parameter-set NALs are built once, and each frame re-fills
/// the same slice and output buffers instead of allocating ~2 MB per slider step.
pub struct Encoder {
    /// Start-code-prefixed VPS, SPS and PPS — identical for every frame.
    header: Vec<u8>,
    out: Vec<u8>,
    slice: BitWriter,
    pcm: PcmBlocks,
}

impl Encoder {
    #[must_use]
    pub fn new() -> Self {
        let mut header = Vec::new();
        nal(&mut header, 32, &vps());
        nal(&mut header, 33, &sps());
        nal(&mut header, 34, &pps());
        Self {
            header,
            out: Vec::with_capacity(1 << 20),
            // 1024 luma + 2x256 chroma samples at 10 bits is exactly 1920 bytes per CTU.
            slice: BitWriter::with_capacity((CTBS_X * CTBS_Y * 1920) as usize + 4096),
            pcm: PcmBlocks::new(),
        }
    }

    /// Encodes one complete Annex-B access unit — VPS, SPS, PPS and an IDR slice — showing
    /// `patches` over a field of `background`. Self-contained, so the decoder can be handed any
    /// frame at any time. Read it back with [`Encoder::frame`].
    pub fn encode(&mut self, background: u16, patches: &[Patch]) {
        let Self {
            header,
            out,
            slice,
            pcm,
        } = self;
        slice.reset();
        idr_slice(slice, pcm, background, patches);
        out.clear();
        out.extend_from_slice(header);
        nal(out, 20, slice.rbsp());
    }

    /// The access unit built by the last [`encode`](Self::encode), valid until the next one.
    #[must_use]
    pub fn frame(&self) -> &[u8] {
        &self.out
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

/// A patch in coded samples, with the luma code it carries.
#[derive(Clone, Copy)]
struct PatchRect {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
    code: u16,
}

impl PatchRect {
    /// The one code covering `[cx, cx + CTB) x [cy, cy + CTB)`, or `None` if this rect's edge
    /// crosses it. `background` is what the CTU shows if no rect covers it.
    fn cover(rects: &[Self], background: u16, cx: u32, cy: u32) -> Option<u16> {
        let mut code = background;
        for r in rects {
            let inside = cx >= r.x0 && cx + CTB <= r.x1 && cy >= r.y0 && cy + CTB <= r.y1;
            let outside = cx + CTB <= r.x0 || cx >= r.x1 || cy + CTB <= r.y0 || cy >= r.y1;
            if inside {
                // Later rects paint over earlier ones, exactly as a drawn plane would.
                code = r.code;
            } else if !outside {
                return None;
            }
        }
        Some(code)
    }
}

/// One CTU row of luma, written into `row` — only ever needed for the handful of CTUs a patch
/// edge crosses. The rows below [`HEIGHT`] are never covered by a patch, so they inherit the
/// background and the crop boundary cannot show.
fn ctu_row(rects: &[PatchRect], background: u16, cx: u32, y: u32, row: &mut [u16]) {
    row.fill(background);
    for r in rects {
        if y < r.y0 || y >= r.y1 {
            continue;
        }
        let (x0, x1) = (r.x0.max(cx), r.x1.min(cx + CTB));
        if x0 < x1 {
            row[(x0 - cx) as usize..(x1 - cx) as usize].fill(r.code);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Bit writing
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct BitWriter {
    buf: Vec<u8>,
    cur: u8,
    filled: u32,
}

impl BitWriter {
    fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            ..Self::default()
        }
    }

    fn bit(&mut self, b: u32) {
        self.cur = (self.cur << 1) | (b as u8 & 1);
        self.filled += 1;
        if self.filled == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.filled = 0;
        }
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    /// Unsigned Exp-Golomb, `ue(v)`.
    fn ue(&mut self, v: u32) {
        let code = v + 1;
        let len = 32 - code.leading_zeros();
        self.bits(0, len - 1);
        self.bits(code, len);
    }

    /// `se(0)` — the only signed Exp-Golomb value this encoder ever writes, which is one 1 bit.
    fn se_zero(&mut self) {
        self.bit(1);
    }

    fn aligned(&self) -> bool {
        self.filled == 0
    }

    fn align_zero(&mut self) {
        while !self.aligned() {
            self.bit(0);
        }
    }

    /// A one bit then zero padding — `rbsp_trailing_bits()` at the end of an RBSP, and the
    /// identical `byte_alignment()` that separates a slice header from its CABAC-coded data.
    fn stop_bit_and_align(&mut self) {
        self.bit(1);
        self.align_zero();
    }

    /// Raw bytes straight into the buffer. Only valid byte-aligned, which is what PCM data is.
    fn bytes(&mut self, b: &[u8]) {
        debug_assert!(self.aligned(), "raw bytes must start byte-aligned");
        self.buf.extend_from_slice(b);
    }

    fn reset(&mut self) {
        self.buf.clear();
        self.cur = 0;
        self.filled = 0;
    }

    /// The bytes written so far. Only valid once the RBSP is complete, i.e. byte-aligned.
    fn rbsp(&self) -> &[u8] {
        debug_assert!(self.aligned(), "RBSP must end byte-aligned");
        &self.buf
    }

    fn finish(self) -> Vec<u8> {
        debug_assert!(self.aligned(), "RBSP must end byte-aligned");
        self.buf
    }
}

/// Appends a start-code-prefixed NAL unit, inserting emulation-prevention bytes.
fn nal(out: &mut Vec<u8>, nal_type: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.push(nal_type << 1);
    out.push(1); // nuh_layer_id = 0, nuh_temporal_id_plus1 = 1

    // Copied in runs between insertions: the RBSP is ~1 MB and a byte at a time here costs as
    // much as producing it did.
    let mut zeros = 0u32;
    let mut start = 0;
    for (i, &b) in rbsp.iter().enumerate() {
        if zeros >= 2 && b <= 3 {
            out.extend_from_slice(&rbsp[start..i]);
            out.push(3);
            start = i;
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out.extend_from_slice(&rbsp[start..]);
}

// ---------------------------------------------------------------------------------------------
// Parameter sets
// ---------------------------------------------------------------------------------------------

/// `profile_tier_level(1, 0)` — Main10, High tier level 5.1, progressive frames only.
///
/// The feed runs ~78 Mb/s (~7.8 Mbit a frame at 10 fps), past every Main-tier level a TV decodes;
/// High tier 5.1 allows 160 Mb/s and is UHD Blu-ray's own profile, so any HDR TV takes it. PCM
/// still breaks the level's minimum compression ratio, which no level can fix and decoders do
/// not enforce.
fn profile_tier_level(w: &mut BitWriter) {
    w.bits(0, 2); // general_profile_space
    w.bit(1); // general_tier_flag: High
    w.bits(2, 5); // general_profile_idc: Main10
    for i in 0..32 {
        w.bit(u32::from(i == 2)); // general_profile_compatibility_flag
    }
    w.bit(1); // general_progressive_source_flag
    w.bit(0); // general_interlaced_source_flag
    w.bit(1); // general_non_packed_constraint_flag
    w.bit(1); // general_frame_only_constraint_flag
    w.bits(0, 32); // general_reserved_zero_43bits, first 32
    w.bits(0, 11); // ...and the remaining 11
    w.bit(0); // general_reserved_zero_bit
    w.bits(153, 8); // general_level_idc: level 5.1
}

fn vps() -> Vec<u8> {
    let mut w = BitWriter::with_capacity(32);
    w.bits(0, 4); // vps_video_parameter_set_id
    w.bit(1); // vps_base_layer_internal_flag
    w.bit(1); // vps_base_layer_available_flag
    w.bits(0, 6); // vps_max_layers_minus1
    w.bits(0, 3); // vps_max_sub_layers_minus1
    w.bit(1); // vps_temporal_id_nesting_flag
    w.bits(0xffff, 16); // vps_reserved_0xffff_16bits
    profile_tier_level(&mut w);
    w.bit(1); // vps_sub_layer_ordering_info_present_flag
    w.ue(0); // vps_max_dec_pic_buffering_minus1[0]
    w.ue(0); // vps_max_num_reorder_pics[0]
    w.ue(0); // vps_max_latency_increase_plus1[0]
    w.bits(0, 6); // vps_max_layer_id
    w.ue(0); // vps_num_layer_sets_minus1
    w.bit(0); // vps_timing_info_present_flag
    w.bit(0); // vps_extension_flag
    w.stop_bit_and_align();
    w.finish()
}

fn sps() -> Vec<u8> {
    let mut w = BitWriter::with_capacity(64);
    w.bits(0, 4); // sps_video_parameter_set_id
    w.bits(0, 3); // sps_max_sub_layers_minus1
    w.bit(1); // sps_temporal_id_nesting_flag
    profile_tier_level(&mut w);
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(1); // chroma_format_idc: 4:2:0
    w.ue(WIDTH);
    w.ue(CODED_HEIGHT);
    w.bit(1); // conformance_window_flag
    w.ue(0); // conf_win_left_offset
    w.ue(0); // conf_win_right_offset
    w.ue(0); // conf_win_top_offset
    w.ue((CODED_HEIGHT - HEIGHT) / 2); // conf_win_bottom_offset, in chroma units
    w.ue(2); // bit_depth_luma_minus8
    w.ue(2); // bit_depth_chroma_minus8
    w.ue(4); // log2_max_pic_order_cnt_lsb_minus4
    w.bit(1); // sps_sub_layer_ordering_info_present_flag
    w.ue(0); // sps_max_dec_pic_buffering_minus1[0]
    w.ue(0); // sps_max_num_reorder_pics[0]
    w.ue(0); // sps_max_latency_increase_plus1[0]

    // Minimum coding block == CTB == 32, so coding_quadtree never signals split_cu_flag.
    w.ue(CTB_LOG2 - 3); // log2_min_luma_coding_block_size_minus3
    w.ue(0); // log2_diff_max_min_luma_coding_block_size
    w.ue(0); // log2_min_luma_transform_block_size_minus2
    w.ue(3); // log2_diff_max_min_luma_transform_block_size
    w.ue(0); // max_transform_hierarchy_depth_inter
    w.ue(0); // max_transform_hierarchy_depth_intra
    w.bit(0); // scaling_list_enabled_flag
    w.bit(0); // amp_enabled_flag
    w.bit(0); // sample_adaptive_offset_enabled_flag
    w.bit(1); // pcm_enabled_flag
    w.bits(9, 4); // pcm_sample_bit_depth_luma_minus1
    w.bits(9, 4); // pcm_sample_bit_depth_chroma_minus1
    w.ue(CTB_LOG2 - 3); // log2_min_pcm_luma_coding_block_size_minus3
    w.ue(0); // log2_diff_max_min_pcm_luma_coding_block_size
    w.bit(1); // pcm_loop_filter_disabled_flag — nothing may filter a measured sample
    w.ue(0); // num_short_term_ref_pic_sets
    w.bit(0); // long_term_ref_pics_present_flag
    w.bit(0); // sps_temporal_mvp_enabled_flag
    w.bit(0); // strong_intra_smoothing_enabled_flag
    w.bit(1); // vui_parameters_present_flag
    w.bit(0); // aspect_ratio_info_present_flag
    w.bit(0); // overscan_info_present_flag
    w.bit(1); // video_signal_type_present_flag
    w.bits(5, 3); // video_format: unspecified
    w.bit(0); // video_full_range_flag: narrow range
    w.bit(1); // colour_description_present_flag
    w.bits(9, 8); // colour_primaries: BT.2020
    w.bits(16, 8); // transfer_characteristics: SMPTE ST 2084 (PQ)
    w.bits(9, 8); // matrix_coeffs: BT.2020 non-constant luminance
    w.bit(0); // chroma_loc_info_present_flag
    w.bit(0); // neutral_chroma_indication_flag
    w.bit(0); // field_seq_flag
    w.bit(0); // frame_field_info_present_flag
    w.bit(0); // default_display_window_flag
    w.bit(0); // vui_timing_info_present_flag
    w.bit(0); // bitstream_restriction_flag
    w.bit(0); // sps_extension_present_flag
    w.stop_bit_and_align();
    w.finish()
}

fn pps() -> Vec<u8> {
    let mut w = BitWriter::with_capacity(16);
    w.ue(0); // pps_pic_parameter_set_id
    w.ue(0); // pps_seq_parameter_set_id
    w.bit(0); // dependent_slice_segments_enabled_flag
    w.bit(0); // output_flag_present_flag
    w.bits(0, 3); // num_extra_slice_header_bits
    w.bit(0); // sign_data_hiding_enabled_flag
    w.bit(0); // cabac_init_present_flag
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.se_zero(); // init_qp_minus26 — SliceQpY 26, which the context initialisation depends on
    w.bit(0); // constrained_intra_pred_flag
    w.bit(0); // transform_skip_enabled_flag
    w.bit(0); // cu_qp_delta_enabled_flag
    w.se_zero(); // pps_cb_qp_offset
    w.se_zero(); // pps_cr_qp_offset
    w.bit(0); // pps_slice_chroma_qp_offsets_present_flag
    w.bit(0); // weighted_pred_flag
    w.bit(0); // weighted_bipred_flag
    w.bit(0); // transquant_bypass_enabled_flag
    w.bit(0); // tiles_enabled_flag
    w.bit(0); // entropy_coding_sync_enabled_flag
    w.bit(0); // pps_loop_filter_across_slices_enabled_flag
    w.bit(1); // deblocking_filter_control_present_flag
    w.bit(0); // deblocking_filter_override_enabled_flag
    w.bit(1); // pps_deblocking_filter_disabled_flag
    w.bit(0); // pps_scaling_list_data_present_flag
    w.bit(0); // lists_modification_present_flag
    w.ue(0); // log2_parallel_merge_level_minus2
    w.bit(0); // slice_segment_header_extension_present_flag
    w.bit(0); // pps_extension_present_flag
    w.stop_bit_and_align();
    w.finish()
}

// ---------------------------------------------------------------------------------------------
// CABAC
// ---------------------------------------------------------------------------------------------

/// Table 9-46, `rangeTabLps`.
#[rustfmt::skip]
const RANGE_TAB_LPS: [[u8; 4]; 64] = [
    [128, 176, 208, 240], [128, 167, 197, 227], [128, 158, 187, 216], [123, 150, 178, 205],
    [116, 142, 169, 195], [111, 135, 160, 185], [105, 128, 152, 175], [100, 122, 144, 166],
    [ 95, 116, 137, 158], [ 90, 110, 130, 150], [ 85, 104, 123, 142], [ 81,  99, 117, 135],
    [ 77,  94, 111, 128], [ 73,  89, 105, 122], [ 69,  85, 100, 116], [ 66,  80,  95, 110],
    [ 62,  76,  90, 104], [ 59,  72,  86,  99], [ 56,  69,  81,  94], [ 53,  65,  77,  89],
    [ 51,  62,  73,  85], [ 48,  59,  69,  80], [ 46,  56,  66,  76], [ 43,  53,  63,  72],
    [ 41,  50,  59,  69], [ 39,  48,  56,  65], [ 37,  45,  54,  62], [ 35,  43,  51,  59],
    [ 33,  41,  48,  56], [ 32,  39,  46,  53], [ 30,  37,  43,  50], [ 29,  35,  41,  48],
    [ 27,  33,  39,  45], [ 26,  31,  37,  43], [ 24,  30,  35,  41], [ 23,  28,  33,  39],
    [ 22,  27,  32,  37], [ 21,  26,  30,  35], [ 20,  24,  29,  33], [ 19,  23,  27,  31],
    [ 18,  22,  26,  30], [ 17,  21,  25,  28], [ 16,  20,  23,  27], [ 15,  19,  22,  25],
    [ 14,  18,  21,  24], [ 14,  17,  20,  23], [ 13,  16,  19,  22], [ 12,  15,  18,  21],
    [ 12,  14,  17,  20], [ 11,  14,  16,  19], [ 11,  13,  15,  18], [ 10,  12,  15,  17],
    [ 10,  12,  14,  16], [  9,  11,  13,  15], [  9,  11,  12,  14], [  8,  10,  12,  14],
    [  8,   9,  11,  13], [  7,   9,  11,  12], [  7,   9,  10,  12], [  7,   8,  10,  11],
    [  6,   8,   9,  11], [  6,   7,   9,  10], [  6,   7,   8,   9], [  2,   2,   2,   2],
];

/// Table 9-47, `transIdxLps`.
#[rustfmt::skip]
const TRANS_IDX_LPS: [u8; 64] = [
     0,  0,  1,  2,  2,  4,  4,  5,  6,  7,  8,  9,  9, 11, 11, 12,
    13, 13, 15, 15, 16, 16, 18, 18, 19, 19, 21, 21, 22, 22, 23, 24,
    24, 25, 26, 26, 27, 27, 28, 29, 29, 30, 30, 30, 31, 32, 32, 33,
    33, 33, 34, 34, 35, 35, 35, 36, 36, 36, 37, 37, 37, 38, 38, 63,
];

/// Table 9-47, `transIdxMps`.
#[rustfmt::skip]
const TRANS_IDX_MPS: [u8; 64] = [
     1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11, 12, 13, 14, 15, 16,
    17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32,
    33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
    49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 62, 63,
];

/// `part_mode` initValue for an I slice (Table 9-11, initType 0).
const PART_MODE_INIT_VALUE: u8 = 184;
const SLICE_QP_Y: i32 = 26;

/// One adaptive probability context.
#[derive(Clone, Copy)]
struct Ctx {
    state: u8,
    mps: u8,
}

impl Ctx {
    /// 9.3.2.2: derive the initial state from the table value and the slice QP.
    fn new(init_value: u8, qp: i32) -> Self {
        let m = i32::from(init_value >> 4) * 5 - 45;
        let n = (i32::from(init_value & 15) << 3) - 16;
        let pre = (((m * qp.clamp(0, 51)) >> 4) + n).clamp(1, 126);
        if pre <= 63 {
            Self {
                state: (63 - pre) as u8,
                mps: 0,
            }
        } else {
            Self {
                state: (pre - 64) as u8,
                mps: 1,
            }
        }
    }
}

/// The arithmetic encoder of 9.3.4, writing into a shared [`BitWriter`].
struct Cabac<'a> {
    w: &'a mut BitWriter,
    low: u32,
    range: u32,
    outstanding: u32,
    first_bit: bool,
}

impl<'a> Cabac<'a> {
    fn new(w: &'a mut BitWriter) -> Self {
        let mut cabac = Self {
            w,
            low: 0,
            range: 0,
            outstanding: 0,
            first_bit: true,
        };
        cabac.reset();
        cabac
    }

    /// 9.3.2.5: at the start of the slice, and re-applied after PCM samples. Context variables
    /// deliberately survive.
    fn reset(&mut self) {
        self.low = 0;
        self.range = 510;
        self.outstanding = 0;
        self.first_bit = true;
    }

    /// The first bit the engine would emit is always redundant, so it is dropped; every
    /// outstanding bit carries the complement of the bit that resolved it.
    fn put_bit(&mut self, b: u32) {
        if self.first_bit {
            self.first_bit = false;
        } else {
            self.w.bit(b);
        }
        while self.outstanding > 0 {
            self.w.bit(1 - b);
            self.outstanding -= 1;
        }
    }

    fn renorm(&mut self) {
        while self.range < 256 {
            if self.low < 256 {
                self.put_bit(0);
            } else if self.low >= 512 {
                self.low -= 512;
                self.put_bit(1);
            } else {
                self.low -= 256;
                self.outstanding += 1;
            }
            self.range <<= 1;
            self.low <<= 1;
        }
    }

    fn encode_bin(&mut self, ctx: &mut Ctx, bin: u8) {
        let q = ((self.range >> 6) & 3) as usize;
        let lps = u32::from(RANGE_TAB_LPS[ctx.state as usize][q]);
        self.range -= lps;
        if bin == ctx.mps {
            ctx.state = TRANS_IDX_MPS[ctx.state as usize];
        } else {
            self.low += self.range;
            self.range = lps;
            if ctx.state == 0 {
                ctx.mps = 1 - ctx.mps;
            }
            ctx.state = TRANS_IDX_LPS[ctx.state as usize];
        }
        self.renorm();
    }

    /// 9.3.4.3.5. A `1` flushes the engine; the final bit written is a one, which the decoder
    /// reads as `rbsp_stop_one_bit` at the end of a slice and as the last bit before
    /// `pcm_alignment_zero_bit` in a PCM CU.
    fn encode_terminate(&mut self, bin: u8) {
        self.range -= 2;
        if bin != 0 {
            self.low += self.range;
            self.range = 2;
            self.renorm();
            self.put_bit((self.low >> 9) & 1);
            let tail = ((self.low >> 7) & 3) | 1;
            self.w.bits(tail, 2);
        } else {
            self.renorm();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------------------------

fn idr_slice(w: &mut BitWriter, pcm: &mut PcmBlocks, background: u16, patches: &[Patch]) {
    let rects: Vec<PatchRect> = patches.iter().map(|p| p.rect()).collect();
    pcm.keep_only(background, &rects);

    w.bit(1); // first_slice_segment_in_pic_flag
    w.bit(0); // no_output_of_prior_pics_flag (present because this is an IRAP NAL type)
    w.ue(0); // slice_pic_parameter_set_id
    w.ue(2); // slice_type: I

    // IDR: no POC lsb, no reference picture set, no SAO flags, no reference list.
    w.se_zero(); // slice_qp_delta
    w.stop_bit_and_align();

    let mut part_mode = Ctx::new(PART_MODE_INIT_VALUE, SLICE_QP_Y);
    let mut c = Cabac::new(w);
    let mut block = [0u16; PCM_LUMA];
    let total = CTBS_X * CTBS_Y;
    for ctb in 0..total {
        let (cx, cy) = ((ctb % CTBS_X) * CTB, (ctb / CTBS_X) * CTB);
        // coding_quadtree omits split_cu_flag (CTB size == minimum CB size), so this is the
        // coding unit directly: PART_2Nx2N, then PCM.
        c.encode_bin(&mut part_mode, 1); // part_mode: PART_2Nx2N
        c.encode_terminate(1); // pcm_flag
        c.w.align_zero(); // pcm_alignment_zero_bit

        // The picture is a flat field with rects on it, so a CTU's content follows from
        // arithmetic: only the few a rect edge crosses are built sample by sample.
        match PatchRect::cover(&rects, background, cx, cy) {
            Some(code) => pcm.uniform(c.w, code),
            None => {
                for (i, row) in block.chunks_exact_mut(CTB as usize).enumerate() {
                    ctu_row(&rects, background, cx, cy + i as u32, row);
                }
                pcm.mixed(c.w, &block);
            }
        }
        c.reset();
        c.encode_terminate(u8::from(ctb + 1 == total)); // end_of_slice_segment_flag
    }
    w.align_zero();
}

/// Luma samples per PCM CU, and chroma samples (both planes at 4:2:0).
const PCM_LUMA: usize = (CTB * CTB) as usize;
const PCM_CHROMA: usize = ((CTB / 2) * (CTB / 2) * 2) as usize;

/// Packs 10-bit samples big-endian: four samples are exactly five bytes.
fn pack10(samples: &[u16], out: &mut Vec<u8>) {
    for q in samples.chunks_exact(4) {
        let (a, b, c, d) = (u32::from(q[0]), u32::from(q[1]), u32::from(q[2]), u32::from(q[3]));
        out.extend_from_slice(&[
            (a >> 2) as u8,
            ((a << 6) | (b >> 4)) as u8,
            ((b << 4) | (c >> 6)) as u8,
            ((c << 2) | (d >> 8)) as u8,
            d as u8,
        ]);
    }
}

/// The `pcm_sample()` payloads, as ready-made bytes.
///
/// PCM data is byte-aligned and a whole number of bytes per CU, so it is packed directly rather
/// than pushed through the bit writer — at ~0.8M samples a frame that loop was the entire cost of
/// building a pattern. Two more things never change: the chroma tail is the same 640 bytes for
/// every CU (every pattern here is achromatic), and every pattern is a flat field with one or two
/// rects on it, so nearly every CU is uniform and re-emits a luma block already built.
struct PcmBlocks {
    chroma: Vec<u8>,
    uniform: Vec<(u16, Vec<u8>)>,
    scratch: Vec<u8>,
}

impl PcmBlocks {
    fn new() -> Self {
        let mut chroma = Vec::with_capacity(PCM_CHROMA * 10 / 8);
        pack10(&[NEUTRAL_CHROMA; PCM_CHROMA], &mut chroma);
        Self {
            chroma,
            uniform: Vec::new(),
            scratch: Vec::new(),
        }
    }

    /// Drops the packed blocks of codes this picture does not use, so a slider swept end to end
    /// keeps the cache at a pattern's two or three codes rather than every code it passed.
    fn keep_only(&mut self, background: u16, rects: &[PatchRect]) {
        self.uniform
            .retain(|(code, _)| *code == background || rects.iter().any(|r| r.code == *code));
    }

    /// Writes a CU whose luma is all one `code`, then its chroma. The packed luma block is kept:
    /// nearly every CTU in a pattern is uniform, and a pattern uses two or three codes in all.
    fn uniform(&mut self, w: &mut BitWriter, code: u16) {
        let i = match self.uniform.iter().position(|(c, _)| *c == code) {
            Some(i) => i,
            None => {
                let mut block = Vec::with_capacity(PCM_LUMA * 10 / 8);
                pack10(&[code; PCM_LUMA], &mut block);
                self.uniform.push((code, block));
                self.uniform.len() - 1
            }
        };
        w.bytes(&self.uniform[i].1);
        w.bytes(&self.chroma);
    }

    /// Writes a CU from `luma` in raster order, then its chroma.
    fn mixed(&mut self, w: &mut BitWriter, luma: &[u16; PCM_LUMA]) {
        self.scratch.clear();
        pack10(luma, &mut self.scratch);
        w.bytes(&self.scratch);
        w.bytes(&self.chroma);
    }
}
