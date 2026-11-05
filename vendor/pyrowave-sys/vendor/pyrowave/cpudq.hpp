// CPU port of wavelet_dequant.comp: bitstream -> R16F coefficient planes.
#pragma once
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include <math.h>
#include <atomic>
#include <condition_variable>
#include <mutex>
#include <thread>
#include <vector>
#if defined(__linux__)
#include <sys/resource.h>
#include <sys/syscall.h>
#include <unistd.h>
#endif
#include <algorithm>
#if defined(__ARM_NEON)
#include <arm_neon.h>
#endif

namespace cpudq
{
static const int Levels = 5, Comps = 3;

struct Plane
{
	int comp = 0, level = 0, band = 0;
	int w = 0, h = 0;          // image size at this level
	int stride = 0, rows = 0;  // padded to 32
	size_t offset = 0;         // in halves
	int bx32 = 0, by32 = 0;
	int block0 = 0;            // first block index
	uint16_t *ptr = nullptr;   // output base
	int ostride = 0;           // output row stride in halves
	bool clip = false;         // output exactly w x h (edge blocks go through a tile)
};

struct Dequant
{
	int W = 0, H = 0;
	std::vector<Plane> planes;
	std::vector<uint32_t> blk_plane; // block -> plane idx
	int nblocks = 0;
	size_t total_halves = 0;
	uint16_t *base = nullptr;
	std::vector<uint8_t> dirty;
	std::vector<uint32_t> offs; // word offsets, UINT32_MAX = absent
	bool use_neon = true;
	bool wc = false; // write-combined destination: stage each block, then write full rows
	bool use_runs = false;

	void init(int width, int height)
	{
		W = width; H = height;
		int aw = (W + 31) & ~31, ah = (H + 31) & ~31;
		aw = aw < 128 ? 128 : aw; ah = ah < 128 ? 128 : ah;
		int hw = aw / 2, hh = ah / 2;
		size_t off = 0;
		int blk = 0;
		for (int level = Levels - 1; level >= 0; level--)
			for (int c = 0; c < Comps; c++)
			{
				if (level == 0 && c != 0) continue;
				for (int band = (level == Levels - 1 ? 0 : 1); band < 4; band++)
				{
					Plane p;
					p.comp = c; p.level = level; p.band = band;
					p.w = hw >> level; p.h = hh >> level;
					p.stride = (p.w + 31) & ~31; p.rows = (p.h + 31) & ~31;
					p.bx32 = p.stride / 32; p.by32 = p.rows / 32;
					p.offset = off;
					off += (size_t(p.stride) * p.rows + 31) & ~size_t(31);
					p.block0 = blk;
					blk += p.bx32 * p.by32;
					planes.push_back(p);
				}
			}
		nblocks = blk;
		total_halves = off;
		blk_plane.resize(nblocks);
		for (size_t i = 0; i < planes.size(); i++)
			for (int b = 0; b < planes[i].bx32 * planes[i].by32; b++)
				blk_plane[planes[i].block0 + b] = uint32_t(i);
		dirty.assign(nblocks, 1);
		offs.assign(nblocks, UINT32_MAX);
	}
	size_t bytes() const { return total_halves * 2; }
	void bind_contiguous(uint16_t *b)
	{
		base = b;
		for (auto &p : planes) { p.ptr = b + p.offset; p.ostride = p.stride; p.clip = false; }
	}

	// returns false on malformed input
	bool parse(const uint8_t *p, size_t size)
	{
		std::fill(offs.begin(), offs.end(), UINT32_MAX);
		const uint8_t *start = p;
		while (size >= 8)
		{
			uint32_t w0, w1;
			memcpy(&w0, p, 4); memcpy(&w1, p + 4, 4);
			if (w0 >> 31) { p += 8; size -= 8; continue; } // extended: 'extended' is bit 15 of the upper half
			uint32_t pw = (w0 >> 16) & 0xfff;
			if (pw < 2 || pw * 4 > size) return false;
			uint32_t bi = w1 >> 8;
			if (bi >= uint32_t(nblocks)) return false;
			if (offs[bi] == UINT32_MAX) offs[bi] = uint32_t((p - start) / 4);
			p += pw * 4; size -= pw * 4;
		}
		return size == 0;
	}

	static inline float decode_quant(uint32_t code)
	{
		int e = 4 - int(code >> 3);
		int m = code & 7;
		return (1.0f / (8.0f * 1024.0f * 1024.0f)) * (float(8 + m) * ldexpf(1.0f, 20 + e));
	}

	// scalar reference
	static void block_scalar(const uint8_t *pay, uint32_t off_words, uint16_t *dst, int stride)
	{
		const uint8_t *b = pay + size_t(off_words) * 4;
		uint32_t ballot = b[0] | (b[1] << 8);
		uint32_t qcode = b[4];
		int n = __builtin_popcount(ballot);
		const uint8_t *cws = b + 8;
		const uint8_t *cw2s = cws + 2 * n;
		const uint8_t *pp = cws + 3 * n;
		// total cost for sign start
		uint32_t total = 0;
		for (int i = 0; i < n; i++)
		{
			uint32_t c = cws[2 * i] | (cws[2 * i + 1] << 8);
			uint32_t qb = cw2s[i] & 15;
			for (int t = 0; t < 8; t++) total += qb + ((c >> (2 * t)) & 3);
		}
		uint64_t signpos = uint64_t(pp - pay + total) * 8;
		float q = decode_quant(qcode);
		int idx = 0;
		for (int s = 0; s < 16; s++)
		{
			uint16_t *d = dst + (s >> 2) * 8 * stride + (s & 3) * 8;
			if (!(ballot >> s & 1))
			{
				for (int y = 0; y < 8; y++) memset(d + y * stride, 0, 16);
				continue;
			}
			uint32_t c = cws[2 * idx] | (cws[2 * idx + 1] << 8);
			uint32_t qb = cw2s[idx] & 15;
			float scale = float(cw2s[idx] >> 4) / 8.0f + 0.25f;
			float inv = q * scale;
			idx++;
			for (int t = 0; t < 8; t++)
			{
				uint32_t f = (c >> (2 * t)) & 3;
				int iters = int(qb + f);
				uint32_t absv[8] = {};
				for (int k = 0; k < iters; k++)
				{
					uint32_t byte = pp[k];
					for (int bb = 0; bb < 8; bb++) absv[bb] |= ((byte >> bb) & 1u) << (iters - 1 - k);
				}
				pp += iters;
				int x0 = 4 * (t >> 2), y0 = 2 * (t & 3);
				if (c == 0) { for (int bb = 0; bb < 8; bb++) d[(y0 + (bb & 1)) * stride + x0 + (bb >> 1)] = 0; continue; }
				for (int bb = 0; bb < 8; bb++)
				{
					float v = 0;
					if (absv[bb])
					{
						v = (float(absv[bb]) + 0.5f) * inv;
						uint32_t sb = (b_bit(pay, signpos++));
						if (sb) v = -v;
					}
					d[(y0 + (bb & 1)) * stride + x0 + (bb >> 1)] = f2h(v);
				}
			}
		}
	}
	static inline uint32_t b_bit(const uint8_t *p, uint64_t pos) { return (p[pos >> 3] >> (pos & 7)) & 1; }
	static inline uint16_t f2h(float f)
	{
#if defined(__ARM_NEON)
		return vget_lane_u16(vreinterpret_u16_f16(vcvt_f16_f32(vdupq_n_f32(f))), 0);
#else
		uint32_t x; memcpy(&x, &f, 4);
		uint32_t s = (x >> 16) & 0x8000; int e = int((x >> 23) & 0xff) - 127 + 15; uint32_t m = x & 0x7fffff;
		if (e <= 0) return uint16_t(s); // flush (reference only)
		if (e >= 31) return uint16_t(s | 0x7c00);
		uint32_t r = (uint32_t(e) << 10) | (m >> 13);
		uint32_t rem = m & 0x1fff;
		if (rem > 0x1000 || (rem == 0x1000 && (r & 1))) r++;
		return uint16_t(s | r);
#endif
	}

#if defined(__ARM_NEON)

	static inline uint8x16_t bit_transpose(uint8x16_t v)
	{
		uint64x2_t x = vreinterpretq_u64_u8(v), t;
		t = vandq_u64(veorq_u64(x, vshlq_n_u64(x, 28)), vdupq_n_u64(0x0f0f0f0f00000000ULL));
		x = veorq_u64(x, veorq_u64(t, vshrq_n_u64(t, 28)));
		t = vandq_u64(veorq_u64(x, vshlq_n_u64(x, 14)), vdupq_n_u64(0x3333000033330000ULL));
		x = veorq_u64(x, veorq_u64(t, vshrq_n_u64(t, 14)));
		t = vandq_u64(veorq_u64(x, vshlq_n_u64(x, 7)), vdupq_n_u64(0x5500550055005500ULL));
		x = veorq_u64(x, veorq_u64(t, vshrq_n_u64(t, 7)));
		return vreinterpretq_u8_u64(x);
	}
	struct Tabs
	{
		uint8_t idx[256][8]; // sign-bit index for each set bit of the mask, 0xff elsewhere
		uint8_t pc[256];
		Tabs()
		{
			for (int m = 0; m < 256; m++)
			{
				int k = 0;
				for (int b = 0; b < 8; b++)
					idx[m][b] = (m >> b & 1) ? uint8_t(k++) : 0xff;
				pc[m] = uint8_t(k);
			}
		}
	};
	static const Tabs &tabs() { static const Tabs t; return t; }

	static inline uint16x8_t conv8(uint8x8_t mag, uint8x8_t neg, float inv2)
	{
		uint16x8_t v = vmovl_u8(mag);
		uint16x8_t w = vandq_u16(vorrq_u16(vshlq_n_u16(v, 1), vdupq_n_u16(1)), vtstq_u16(v, v));
		float32x4_t fl = vmulq_n_f32(vcvtq_f32_u32(vmovl_u16(vget_low_u16(w))), inv2);
		float32x4_t fh = vmulq_n_f32(vcvtq_f32_u32(vmovl_u16(vget_high_u16(w))), inv2);
		uint16x8_t h = vcombine_u16(vreinterpret_u16_f16(vcvt_f16_f32(fl)), vreinterpret_u16_f16(vcvt_f16_f32(fh)));
		return veorq_u16(h, vshlq_n_u16(vmovl_u8(neg), 15));
	}

	// Branch-light sub-block decode, valid when every group has <= 8 bit-planes.
	static inline void sub_fast(const uint8_t *pay, const uint8_t *&pp, uint64_t &signpos, uint32_t c, uint32_t qb,
	                            float inv2, uint16_t *d, int stride, const Tabs &T)
	{
		static const uint8_t mt[16] = {0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255};
		static const uint8_t bmv[8] = {1, 2, 4, 8, 16, 32, 64, 128};
		const uint8x8_t bm8 = vld1_u8(bmv);
		const uint8x16_t bm16 = vcombine_u8(bm8, bm8);
		uint8x8_t xs[8];
		const uint8_t *q = pp;
		for (int t = 0; t < 8; t++)
		{
			uint32_t n = qb + ((c >> (2 * t)) & 3);
			uint8x8_t x = vand_u8(vld1_u8(q + n - 8), vld1_u8(mt + n));
			xs[t] = vrev64_u8(x);
			q += n;
		}
		pp = q;
		uint8x16_t Y[4];
		uint32_t m[8];
		for (int p = 0; p < 4; p++)
		{
			Y[p] = bit_transpose(vcombine_u8(xs[p], xs[p + 4]));
			uint64x2_t s = vpaddlq_u32(vpaddlq_u16(vpaddlq_u8(vandq_u8(vtstq_u8(Y[p], Y[p]), bm16))));
			uint32x4_t s32 = vreinterpretq_u32_u64(s);
			m[p] = vgetq_lane_u32(s32, 0);
			m[p + 4] = vgetq_lane_u32(s32, 2);
		}
		uint8x8_t sel[8];
		for (int t = 0; t < 8; t++)
		{
			uint32_t w32;
			memcpy(&w32, pay + (signpos >> 3), 4);
			uint32_t sbits = w32 >> (signpos & 7);
			signpos += T.pc[m[t]];
			sel[t] = vtbl1_u8(vtst_u8(vdup_n_u8(uint8_t(sbits)), bm8), vld1_u8(T.idx[m[t]]));
		}
		for (int p = 0; p < 4; p++)
		{
			uint8x8x2_t yu = vuzp_u8(vget_low_u8(Y[p]), vget_high_u8(Y[p]));
			uint8x8x2_t su = vuzp_u8(sel[p], sel[p + 4]);
			vst1q_u16(d + 2 * p * stride, conv8(yu.val[0], su.val[0], inv2));
			vst1q_u16(d + (2 * p + 1) * stride, conv8(yu.val[1], su.val[1], inv2));
		}
	}

	static void block_neon2(const uint8_t *pay, uint32_t off_words, uint16_t *dst, int stride)
	{
		const Tabs &T = tabs();
		const uint8_t *b = pay + size_t(off_words) * 4;
		uint32_t ballot = b[0] | (b[1] << 8);
		uint32_t qcode = b[4];
		int n = __builtin_popcount(ballot);
		const uint8_t *cws = b + 8;
		const uint8_t *cw2s = cws + 2 * n;
		const uint8_t *pp = cws + 3 * n;
		uint32_t total = 0;
		for (int i = 0; i < n; i++)
		{
			uint32_t c = cws[2 * i] | (cws[2 * i + 1] << 8);
			uint32_t qb = cw2s[i] & 15;
			uint32_t s = (c & 0x3333) + ((c >> 2) & 0x3333);
			s = (s & 0x0f0f) + ((s >> 4) & 0x0f0f);
			s = (s & 0xff) + (s >> 8);
			total += qb * 8 + s;
		}
		uint64_t signpos = uint64_t(pp - pay + total) * 8;
		float q = decode_quant(qcode);
		int idx = 0;
		const uint16x8_t z = vdupq_n_u16(0);
		for (int s = 0; s < 16; s++)
		{
			uint16_t *d = dst + (s >> 2) * 8 * stride + (s & 3) * 8;
			if (!(ballot >> s & 1))
			{
				for (int y = 0; y < 8; y++) vst1q_u16(d + y * stride, z);
				continue;
			}
			uint32_t c = cws[2 * idx] | (cws[2 * idx + 1] << 8);
			uint32_t qb = cw2s[idx] & 15;
			float inv = q * (float(cw2s[idx] >> 4) / 8.0f + 0.25f);
			idx++;
			if (c == 0)
			{
				pp += 8 * qb;
				for (int y = 0; y < 8; y++) vst1q_u16(d + y * stride, z);
				continue;
			}
			bool fast = qb <= 5;
			if (!fast)
			{
				uint32_t mx = 0;
				for (int t = 0; t < 8; t++) mx = std::max(mx, (c >> (2 * t)) & 3);
				fast = qb + mx <= 8;
			}
			if (fast)
			{
				sub_fast(pay, pp, signpos, c, qb, inv * 0.5f, d, stride, T);
				continue;
			}
			// slow path (rare): per-group bit-plane loop
			static const uint8_t bitm[8] = {1, 2, 4, 8, 16, 32, 64, 128};
			const uint8x8_t bm8 = vld1_u8(bitm);
			const uint16x8_t bm16 = vmovl_u8(bm8);
			for (int t = 0; t < 8; t++)
			{
				int iters = int(qb) + int((c >> (2 * t)) & 3);
				uint16_t *o = d + 2 * (t & 3) * stride + 4 * (t >> 2);
				if (iters == 0)
				{
					vst1_u16(o, vdup_n_u16(0));
					vst1_u16(o + stride, vdup_n_u16(0));
					continue;
				}
				uint32_t orv = 0;
				uint32x4_t lo, hi;
				if (iters <= 16)
				{
					int16x8_t acc = vdupq_n_s16(0);
					for (int k = 0; k < iters; k++)
					{
						uint32_t by = pp[k];
						orv |= by;
						int16x8_t m = vreinterpretq_s16_s32(vdupq_n_s32(0)); (void)m;
						uint8x8_t tm = vtst_u8(vdup_n_u8(uint8_t(by)), bm8);
						int16x8_t mm = vmovl_s8(vreinterpret_s8_u8(tm));
						acc = vsubq_s16(vaddq_s16(acc, acc), mm);
					}
					uint16x8_t ua = vreinterpretq_u16_s16(acc);
					lo = vmovl_u16(vget_low_u16(ua));
					hi = vmovl_u16(vget_high_u16(ua));
				}
				else
				{
					uint32x4_t al = vdupq_n_u32(0), ah = vdupq_n_u32(0);
					for (int k = 0; k < iters; k++)
					{
						uint32_t by = pp[k];
						orv |= by;
						uint8x8_t tm = vtst_u8(vdup_n_u8(uint8_t(by)), bm8);
						int16x8_t mm = vmovl_s8(vreinterpret_s8_u8(tm));
						int32x4_t ml = vmovl_s16(vget_low_s16(mm)), mh = vmovl_s16(vget_high_s16(mm));
						al = vreinterpretq_u32_s32(vsubq_s32(vreinterpretq_s32_u32(vaddq_u32(al, al)), ml));
						ah = vreinterpretq_u32_s32(vsubq_s32(vreinterpretq_s32_u32(vaddq_u32(ah, ah)), mh));
					}
					lo = al; hi = ah;
				}
				pp += iters;
				const uint32x4_t halfbits = vdupq_n_u32(0x3f000000);
				float32x4_t fl = vcvtq_f32_u32(lo), fh = vcvtq_f32_u32(hi);
				fl = vaddq_f32(fl, vreinterpretq_f32_u32(vandq_u32(vtstq_u32(lo, lo), halfbits)));
				fh = vaddq_f32(fh, vreinterpretq_f32_u32(vandq_u32(vtstq_u32(hi, hi), halfbits)));
				fl = vmulq_n_f32(fl, inv);
				fh = vmulq_n_f32(fh, inv);
				uint16x8_t h = vcombine_u16(vreinterpret_u16_f16(vcvt_f16_f32(fl)), vreinterpret_u16_f16(vcvt_f16_f32(fh)));
				// signs
				int k = __builtin_popcount(orv);
				uint32_t sb = 0;
				{
					uint32_t w32;
					memcpy(&w32, pay + (signpos >> 3), 4);
					sb = (w32 >> (signpos & 7)) & ((1u << k) - 1);
					signpos += k;
				}
				uint32_t neg = 0;
				for (uint32_t m = orv; m; m &= m - 1)
				{
					neg |= (sb & 1u) << __builtin_ctz(m);
					sb >>= 1;
				}
				if (neg)
					h = veorq_u16(h, vandq_u16(vtstq_u16(vdupq_n_u16(uint16_t(neg)), bm16), vdupq_n_u16(0x8000)));
				uint16x4x2_t u = vuzp_u16(vget_low_u16(h), vget_high_u16(h));
				vst1_u16(o, u.val[0]);
				vst1_u16(o + stride, u.val[1]);
			}
		}
	}
#endif

	void do_block(int bi, const uint8_t *pay)
	{
		const Plane &pl = planes[blk_plane[bi]];
		int lb = bi - pl.block0;
		int bx = lb % pl.bx32, by = lb / pl.bx32;
		int os = pl.ostride;
		uint16_t *dst = pl.ptr + size_t(by) * 32 * os + bx * 32;
		uint32_t o = offs[bi];
		bool edge = pl.clip && ((bx + 1) * 32 > pl.w || (by + 1) * 32 > pl.h);
		int vw = edge ? std::min(32, pl.w - bx * 32) : 32, vh = edge ? std::min(32, pl.h - by * 32) : 32;
		if (o == UINT32_MAX)
		{
			if (dirty[bi])
			{
				for (int y = 0; y < vh; y++) memset(dst + size_t(y) * os, 0, vw * 2);
				dirty[bi] = 0;
			}
			return;
		}
		dirty[bi] = 1;
		alignas(16) uint16_t tile[32 * 32];
		const bool stage = edge || wc;
		uint16_t *d = stage ? tile : dst;
		int ds = stage ? 32 : os;
#if defined(__ARM_NEON)
		if (use_neon) block_neon2(pay, o, d, ds); else
#endif
		block_scalar(pay, o, d, ds);
		if (stage)
		{
#if defined(__ARM_NEON)
			if (vw == 32)
			{
				for (int y = 0; y < vh; y++)
				{
					uint16_t *r = dst + size_t(y) * os;
					const uint16_t *t = tile + y * 32;
					vst1q_u16(r, vld1q_u16(t)); vst1q_u16(r + 8, vld1q_u16(t + 8));
					vst1q_u16(r + 16, vld1q_u16(t + 16)); vst1q_u16(r + 24, vld1q_u16(t + 24));
				}
				return;
			}
#endif
			for (int y = 0; y < vh; y++) memcpy(dst + size_t(y) * os, tile + y * 32, vw * 2);
		}
	}

	static const int RunMax = 8;
#if defined(__ARM_NEON)
	void do_run(int bi, int n, const uint8_t *pay)
	{
		const Plane &pl = planes[blk_plane[bi]];
		int lb = bi - pl.block0;
		int bx = lb % pl.bx32, by = lb / pl.bx32;
		int os = pl.ostride;
		uint16_t *dst = pl.ptr + size_t(by) * 32 * os + bx * 32;
		alignas(16) uint16_t tile[32 * 32 * RunMax];
		const int ts = 32 * RunMax;
		for (int i = 0; i < n; i++)
		{
			uint32_t o = offs[bi + i];
			uint16_t *d = tile + i * 32;
			if (o == UINT32_MAX)
			{
				dirty[bi + i] = 0;
				uint16x8_t z = vdupq_n_u16(0);
				for (int y = 0; y < 32; y++)
				{
					vst1q_u16(d + y * ts, z); vst1q_u16(d + y * ts + 8, z);
					vst1q_u16(d + y * ts + 16, z); vst1q_u16(d + y * ts + 24, z);
				}
				continue;
			}
			dirty[bi + i] = 1;
			block_neon2(pay, o, d, ts);
		}
		int vw = n * 32, vh = 32;
		if (pl.clip)
		{
			vw = std::min(vw, pl.w - bx * 32);
			vh = std::min(32, pl.h - by * 32);
		}
		size_t bytes = size_t(vw) * 2;
		for (int y = 0; y < vh; y++)
		{
			const uint8_t *s = reinterpret_cast<const uint8_t *>(tile + y * ts);
			uint8_t *t = reinterpret_cast<uint8_t *>(dst + size_t(y) * os);
			memcpy(t, s, bytes);
		}
	}
#else
	void do_run(int, int, const uint8_t *) {}
#endif

	// Zero a horizontal run of dirty-absent blocks row by row so the stores stream.
#if defined(__ARM_NEON)
	void do_zero_run(int bi, int n)
	{
		const Plane &pl = planes[blk_plane[bi]];
		int lb = bi - pl.block0;
		int bx = lb % pl.bx32, by = lb / pl.bx32;
		int os = pl.ostride;
		int vw = n * 32, vh = 32;
		if (pl.clip)
		{
			vw = std::min(vw, pl.w - bx * 32);
			vh = std::min(32, pl.h - by * 32);
		}
		uint16_t *dst = pl.ptr + size_t(by) * 32 * os + bx * 32;
		const uint16x8_t z = vdupq_n_u16(0);
		for (int y = 0; y < vh; y++)
		{
			uint16_t *r = dst + size_t(y) * os;
			int x = 0;
			for (; x + 8 <= vw; x += 8) vst1q_u16(r + x, z);
			for (; x < vw; x++) r[x] = 0;
		}
		for (int i = 0; i < n; i++) dirty[bi + i] = 0;
	}
#else
	void do_zero_run(int, int) {}
#endif

	// persistent pool
	struct Pool
	{
		std::vector<std::thread> th;
		std::atomic<int> gen{0}, done{0}, next{0};
		std::atomic<bool> quit{false};
		std::vector<int> work;
		Dequant *dq = nullptr;
		const uint8_t *pay = nullptr;
		std::mutex mu;
		std::condition_variable cv;
		void loop()
		{
#if defined(__linux__)
			if (const char *n = getenv("PYROWAVE_CPU_DEQUANT_NICE"))
				setpriority(PRIO_PROCESS, id_t(syscall(SYS_gettid)), atoi(n));
#endif
			int seen = 0;
			for (;;)
			{
				for (int spins = 0; gen.load(std::memory_order_acquire) == seen; spins++)
				{
					if (quit.load())
						return;
					if (spins > 2000)
					{
						std::unique_lock<std::mutex> l(mu);
						cv.wait(l, [&] { return gen.load() != seen || quit.load(); });
						spins = 0;
					}
				}
				seen = gen.load();
				drain();
				done.fetch_add(1, std::memory_order_acq_rel);
			}
		}
		void drain()
		{
			for (;;)
			{
				int i = next.fetch_add(1, std::memory_order_relaxed);
				if (i >= int(work.size())) break;
				if (dq->use_runs) dq->do_run(work[i] & 0xffffff, work[i] >> 24, pay); else if (work[i] >> 24) dq->do_zero_run(work[i] & 0xffffff, work[i] >> 24); else dq->do_block(work[i], pay);
			}
		}
		void start(Dequant *d, int extra)
		{
			dq = d;
			for (int i = 0; i < extra; i++) th.emplace_back([this] { loop(); });
		}
		void stop()
		{
			quit = true;
			{ std::lock_guard<std::mutex> l(mu); }
			cv.notify_all();
			for (auto &t : th) t.join();
			th.clear();
		}
		void run(Dequant *d, const uint8_t *p)
		{
			dq = d; pay = p;
			next = 0; done = 0;
			gen.fetch_add(1, std::memory_order_release);
			{ std::lock_guard<std::mutex> l(mu); }
			cv.notify_all();
			drain();
			while (done.load(std::memory_order_acquire) < int(th.size())) {}
		}
	};
	Pool pool;

	// Build the work list (present blocks, largest first, plus dirty absent blocks) and decode.
	void run(const uint8_t *pay)
	{
		pool.work.clear();
		if (!use_runs)
		{
			int zs = -1, zn = 0;
			for (int i = 0; i < nblocks; i++)
			{
				const bool present = offs[i] != UINT32_MAX;
				const bool zero = !present && dirty[i];
				if (zn)
				{
					const Plane &pl = planes[blk_plane[i]];
					if (!zero || (i - pl.block0) % pl.bx32 == 0 || zn == 16) { pool.work.push_back(zs | (zn << 24)); zn = 0; }
				}
				if (zero) { if (!zn) zs = i; zn++; }
				else if (present) pool.work.push_back(i);
			}
			if (zn) pool.work.push_back(zs | (zn << 24));
		}
		else
		{
			int start = -1, n = 0, nb = 1;
			for (int i = 0; i < nblocks; i++)
			{
				const Plane &pl = planes[blk_plane[i]];
				bool newrow = (i - pl.block0) % pl.bx32 == 0;
				bool need = offs[i] != UINT32_MAX || dirty[i];
				if (n && (!need || newrow || n == RunMax)) { pool.work.push_back(start | (n << 24)); n = 0; }
				if (need) { if (!n) start = i; n++; }
			}
			if (n) pool.work.push_back(start | (n << 24));
		}
		pool.run(this, pay);
	}
};
} // namespace cpudq
