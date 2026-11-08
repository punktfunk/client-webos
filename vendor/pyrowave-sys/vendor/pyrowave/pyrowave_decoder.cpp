// Copyright (c) 2025 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT
#include "pyrowave_decoder.hpp"
#include "device.hpp"
#include "buffer.hpp"
#include "image.hpp"
#include "math.hpp"
#include "pyrowave_common.hpp"
#include <algorithm>
#include <chrono>
#ifdef __linux__
#include <sched.h>
#endif
#include <thread>
#include "cpudq.hpp"

namespace PyroWave
{
using namespace Granite;
using namespace Vulkan;

struct DequantizerPushData
{
	ivec2 resolution;
	int32_t output_layer;
	int32_t block_offset_32x32;
	int32_t block_stride_32x32;
};

struct Decoder::Impl final : public WaveletBuffers
{
	BufferHandle dequant_offset_buffer, payload_data;
	BufferViewHandle payload_u32_view, payload_u16_view, payload_u8_view;

	// Turbo-hacky path.
	DeviceAllocationOwnerHandle linear_memory;
	ImageHandle payload_r8_image, payload_r16_image, payload_r32_image;
	bool need_image_transition = true;

	std::vector<uint32_t> dequant_offset_buffer_cpu;
	std::vector<uint8_t> block_dirty;
	bool zero_once = getenv("PYROWAVE_ZERO_ONCE") && atoi(getenv("PYROWAVE_ZERO_ONCE")) != 0;
	bool skip_bands_enabled = getenv("PYROWAVE_SKIP_BANDS") && atoi(getenv("PYROWAVE_SKIP_BANDS")) != 0;
	bool half_output = getenv("PYROWAVE_HALF_OUTPUT") && atoi(getenv("PYROWAVE_HALF_OUTPUT")) != 0;
	uint8_t band_skip[NumComponents][DecompositionLevels] = {};
	bool skip_tiles_enabled = getenv("PYROWAVE_SKIP_TILES") && atoi(getenv("PYROWAVE_SKIP_TILES")) != 0;
	BufferHandle tile_flag_buffer;
	std::vector<uint32_t> tile_flags_cpu;
	uint32_t tile_flag_offset[NumComponents][DecompositionLevels] = {};
	uint32_t tile_flag_words[DecompositionLevels] = {};
	bool tile_use[NumComponents][DecompositionLevels] = {};
	bool any_tile_use = false;
	void update_tile_flags();
	bool wavelet_images_initialized = false;
	bool use_idwt_shuf() const;
	std::vector<uint32_t> payload_data_cpu;
	// PUNKTFUNK (patch 0011): see Decoder::set_payload_memory. A wrapped image has no
	// default view, so the views sit beside it.
	uint8_t *in_place_base = nullptr;
	VkDeviceSize in_place_size = 0;
	VkDeviceSize in_place_reserve = 0;
	bool pushing_in_place = false;
	ImageViewHandle linear_views[3];
	int decoded_blocks = 0;
	int total_blocks_in_sequence = 0;
	uint32_t last_seq = UINT32_MAX;
	bool decoded_frame_for_current_sequence = false;

	// PYROWAVE_CPU_DEQUANT=1: the CPU dequantizes into host-cached linear images (NEON, thread pool)
	// and the GPU runs only the iDWT. Each ring slot has its own images and dirty map.
	bool cpu_dq = getenv("PYROWAVE_CPU_DEQUANT") && atoi(getenv("PYROWAVE_CPU_DEQUANT")) != 0;
	static constexpr int CpuRing = 4;
	cpudq::Dequant hq;
	bool cpu_dq_ready = false;
	int cpu_slot = 0;
	bool cpu_lin_fresh[CpuRing] = { true, true, true, true };
	std::vector<uint8_t> cpu_dirty[CpuRing];
	ImageHandle cpu_lin[CpuRing][NumComponents][DecompositionLevels];
	ImageViewHandle cpu_lin_view[CpuRing][NumComponents][DecompositionLevels];
	ImageViewHandle cpu_lin_ll[CpuRing][NumComponents][DecompositionLevels];
	std::vector<uint32_t> cpu_offsets_scratch;
	uint32_t cpu_dq_us = 0;
	bool cpu_dq_setup();
	bool cpu_dq_fill(CommandBuffer &cmd);
	bool cpu_dq_into(int slot, const uint8_t *pay, bool listed);
	int cpu_free_slot(int avoid) const;
	// Early dequant: windows of frame `early_tag` dequantized into `early_slot` while it arrives.
	int gpu_slot[2] = { -1, -1 };
	int early_slot = -1;
	uint32_t early_tag = 0, claimed_tag = 0;
	bool early_use = false, any_claimed = false;
	std::vector<uint8_t> early_done;
	std::vector<uint32_t> early_offs, early_pay;
	std::vector<std::pair<uint16_t *, int>> early_ptrs; // per plane, the early slot's mapping
	uint32_t early_done_blocks = 0;
	int early_verify = getenv("PYROWAVE_EARLY_VERIFY") ? atoi(getenv("PYROWAVE_EARLY_VERIFY")) : 0;
	int early_verify_n = 0;
	int early_verify_bad = -1;
	uint32_t early_windows(uint32_t tag, const uint8_t *wins, size_t count, size_t wbytes);
	uint32_t early_claim(uint32_t tag);
	void early_check(int slot);
	~Impl()
	{
		hq.pool.stop();
	}

	bool push_packet(const void *data, size_t size);
	bool decode(CommandBuffer &cmd, const ViewBuffers &views);
	bool decode_is_ready(bool allow_partial_frame) const;
	bool decode_is_ready(bool allow_partial_frame, int pristine_bands, float received_ratio,
	                     const uint32_t *active_block_mask, size_t word_count) const;
	bool has_pristine_bands(int bands, const uint32_t *active_block_mask, size_t word_count) const;

	bool decode_packet(const BitstreamHeader *header);

	bool dequant(CommandBuffer &cmd);
	bool idwt(CommandBuffer &cmd, const ViewBuffers &views);
	bool is_nv12(const ViewBuffers &views) const;
	ImageHandle nv12_cb;
	bool idwt_fragment(CommandBuffer &cmd, const ViewBuffers &views);
	void init_block_meta() override;
	void clear();

	bool upload_payload(CommandBuffer &cmd);

	void check_linear_texture_support();
	bool set_payload_memory(uint64_t buffer, uint64_t size, uint64_t copy_reserve, void *host,
	                        const uint64_t linear[3], uint32_t rows);
};

Decoder::Decoder()
{
	impl.reset(new Impl);
}

Decoder::~Decoder()
{
}

bool Decoder::Impl::upload_payload(CommandBuffer &cmd)
{
	VkDeviceSize required_size = payload_data_cpu.size() * sizeof(uint32_t);

	// Avoid edge case OOB access without robustness on the payload buffer during dequant.
	VkDeviceSize required_size_padded = required_size + 16;

	// PUNKTFUNK (patch 0011): the caller's memory is never reallocated. In-place packets are
	// already in it; copies land in its reserved head and must fit.
	if (in_place_base)
	{
		if (required_size_padded > in_place_reserve)
		{
			LOGE("Copied payload (%llu bytes) overflows the reserved head.\n",
			     static_cast<unsigned long long>(required_size));
			return false;
		}
	}
	else if (!payload_data || required_size_padded > payload_data->get_create_info().size)
	{
		BufferCreateInfo bufinfo;
		bufinfo.size = std::max<VkDeviceSize>(64 * 1024, required_size_padded * 2);
		bufinfo.usage =
			VK_BUFFER_USAGE_TRANSFER_DST_BIT |
			VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
			VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT;
		bufinfo.domain = BufferDomain::Device;
		payload_data = device->create_buffer(bufinfo);
		device->set_name(*payload_data, "payload-data");

		if (use_readonly_texel_buffer)
		{
			BufferViewCreateInfo view_info = {};
			view_info.buffer = payload_data.get();
			view_info.range = VK_WHOLE_SIZE;

			view_info.format = VK_FORMAT_R8_UINT;
			payload_u8_view = device->create_buffer_view(view_info);
			view_info.format = VK_FORMAT_R16_UINT;
			payload_u16_view = device->create_buffer_view(view_info);
			view_info.format = VK_FORMAT_R32_UINT;
			payload_u32_view = device->create_buffer_view(view_info);
		}

		// This shouldn't happen to demote to texel buffers if we need to deal with massive gigantic payloads.
		payload_r8_image.reset();
		payload_r16_image.reset();
		payload_r32_image.reset();
	}

	if (need_image_transition)
	{
		cmd.begin_barrier_batch();
		const Image *imgs[] = { payload_r8_image.get(), payload_r16_image.get(), payload_r32_image.get() };
		for (auto *img : imgs)
		{
			if (img)
			{
				cmd.image_barrier(*img, VK_IMAGE_LAYOUT_PREINITIALIZED, VK_IMAGE_LAYOUT_GENERAL,
				                  0, 0, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
			}
		}
		cmd.end_barrier_batch();
		need_image_transition = false;
	}

	if (!payload_data_cpu.empty())
		memcpy(cmd.update_buffer(*payload_data, 0, required_size), payload_data_cpu.data(), required_size);
	return true;
}

bool Decoder::Impl::decode_packet(const BitstreamHeader *header)
{
	auto &offset = dequant_offset_buffer_cpu[header->block_index];
	if (offset == UINT32_MAX)
	{
		decoded_blocks++;
		// PUNKTFUNK (patch 0011): an in-place packet is addressed where it lies.
		offset = pushing_in_place ?
		         uint32_t((reinterpret_cast<const uint8_t *>(header) - in_place_base) / sizeof(uint32_t)) :
		         uint32_t(payload_data_cpu.size());
	}
	else
	{
		return true;
	}

	auto *payload_words = reinterpret_cast<const uint32_t *>(header);

	if (sizeof(*header) / sizeof(uint32_t) > header->payload_words)
	{
		LOGE("payload_words is not large enough.\n");
		return false;
	}

	if (pushing_in_place)
		return true;

	payload_data_cpu.insert(
			payload_data_cpu.end(),
			payload_words,
			payload_words + header->payload_words);
	return true;
}

bool Decoder::Impl::push_packet(const void *data_, size_t size)
{
	auto *data = static_cast<const uint8_t *>(data_);
	while (size >= sizeof(BitstreamHeader))
	{
		auto *header = reinterpret_cast<const BitstreamHeader *>(data);

		if (header->extended != 0)
		{
			auto *seq = reinterpret_cast<const BitstreamSequenceHeader *>(header);

			if (sizeof(*header) > size)
			{
				LOGE("Parsing sequence header, but only %zu bytes left to parse.\n", size);
				return false;
			}

			if (seq->chroma_resolution != int(chroma))
			{
				LOGE("Chroma resolution mismatch!\n");
				return false;
			}

			uint8_t diff = (header->sequence - last_seq) & SequenceCountMask;
			if (last_seq != UINT32_MAX && diff > (SequenceCountMask / 2))
			{
				return true;
			}

			if (last_seq == UINT32_MAX || diff != 0)
			{
				clear();
				last_seq = header->sequence;
			}

			if (seq->code == BITSTREAM_EXTENDED_CODE_START_OF_FRAME)
			{
				if (seq->width_minus_1 + 1 != width || seq->height_minus_1 + 1 != height)
				{
					LOGE("Dimension mismatch in seq packet, (%d, %d) != (%d, %d)\n",
					     seq->width_minus_1 + 1, seq->height_minus_1 + 1, width, height);
					return false;
				}

				total_blocks_in_sequence = int(seq->total_blocks);
			}
			else
			{
				LOGE("Unrecognized sequence header mode %u.\n", seq->code);
				return false;
			}

			data += sizeof(*header);
			size -= sizeof(*header);

			continue;
		}

		size_t packet_size = header->payload_words * sizeof(uint32_t);

		// PUNKTFUNK: a packet must be at least its own header, so this parse cursor ALWAYS advances.
		// payload_words is a 12-bit wire field that may be 0, and decode_packet's duplicate-block
		// early return fires BEFORE its own payload_words minimum check — so a duplicate block_index
		// with payload_words == 0 returns true and `data += 0; size -= 0;` spins the decode thread
		// forever. Reject it here, before decode_packet is consulted. security-review 2026-08-15 #9.
		if (header->payload_words < sizeof(*header) / sizeof(uint32_t))
		{
			LOGE("payload_words (%u) is smaller than the packet header — refusing.\n", header->payload_words);
			return false;
		}

		if (packet_size > size)
		{
			LOGE("Packet header states %zu bytes, but only %zu bytes left to parse.\n", packet_size, size);
			return false;
		}

		bool restart;

		if (last_seq == UINT32_MAX)
		{
			restart = true;
		}
		else
		{
			uint8_t diff = (header->sequence - last_seq) & SequenceCountMask;
			if (diff > (SequenceCountMask / 2))
			{
				return true;
			}
			restart = diff != 0;
		}

		if (restart)
		{
			clear();
			last_seq = header->sequence;
		}

		if (header->block_index >= uint32_t(block_count_32x32))
		{
			LOGE("block_index %u is out of bounds (>= %d).\n", header->block_index, block_count_32x32);
			return false;
		}

		if (!decode_packet(header))
			return false;

		data += packet_size;
		size -= packet_size;
	}

	if (size != 0)
	{
		LOGE("Did not consume packet completely.\n");
		return false;
	}

	return true;
}

void Decoder::Impl::init_block_meta()
{
	WaveletBuffers::init_block_meta();

	BufferCreateInfo info;
	info.domain = BufferDomain::Device;
	info.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT;

	info.size = block_count_32x32 * sizeof(uint32_t);
	dequant_offset_buffer = device->create_buffer(info);
	device->set_name(*dequant_offset_buffer, "meta-buffer");
	dequant_offset_buffer_cpu.resize(block_count_32x32);
	block_dirty.assign(block_count_32x32, 1);

	payload_data_cpu.reserve(1024 * 1024);
}

bool Decoder::Impl::dequant(CommandBuffer &cmd)
{
	DequantizerPushData push = {};

	cmd.set_specialization_constant_mask(0);
	cmd.enable_subgroup_size_control(true);

	if (device->supports_subgroup_size_log2(true, 4, 7))
	{
		cmd.set_subgroup_size_log2(true, 4, 7);
	}
	else if (device->supports_subgroup_size_log2(true, 2, 7))
	{
		cmd.set_subgroup_size_log2(true, 2, 7);
	}
	else
	{
		LOGE("No compatible subgroup size config.\n");
		return false;
	}

	if (payload_r8_image && payload_r16_image && payload_r32_image)
		cmd.set_program(shaders.wavelet_dequant[2]);
	else if (use_readonly_texel_buffer)
		cmd.set_program(shaders.wavelet_dequant[1]);
	else
		cmd.set_program(shaders.wavelet_dequant[0]);

	cmd.begin_region("DWT dequant");
	auto start_dequant = cmd.write_timestamp(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);

	cmd.image_barrier(*wavelet_img_high_res, wavelet_images_initialized ? VK_IMAGE_LAYOUT_GENERAL : VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL,
	                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0,
	                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
	                  VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);

	if (wavelet_img_low_res)
	{
		cmd.image_barrier(*wavelet_img_low_res, wavelet_images_initialized ? VK_IMAGE_LAYOUT_GENERAL : VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL,
		                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT | VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0,
		                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
		                  VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);
	}

	wavelet_images_initialized = true;

	// De-quantize
	for (int level = 0; level < DecompositionLevels; level++)
	{
		for (int component = 0; component < NumComponents; component++)
		{
			// Ignore top-level CbCr when doing 420 subsampling.
			if (level == 0 && component != 0 && chroma == ChromaSubsampling::Chroma420)
				continue;
			if (half_output && level == (component != 0 && chroma == ChromaSubsampling::Chroma420 ? 1 : 0))
				continue;

			char label[128];
			snprintf(label, sizeof(label), "level %d - component %d", level, component);
			cmd.begin_region(label);

			for (int band = (level == DecompositionLevels - 1 ? 0 : 1); band < 4; band++)
			{
				if (band != 0 && (band_skip[component][level] & (1u << (band - 1))))
					continue;

				push.resolution.x = wavelet_img_high_res->get_width(level);
				push.resolution.y = wavelet_img_high_res->get_height(level);
				push.output_layer = band;
				push.block_offset_32x32 = block_meta[component][level][band].block_offset_32x32;
				push.block_stride_32x32 = block_meta[component][level][band].block_stride_32x32;
				cmd.push_constants(&push, 0, sizeof(push));

				cmd.set_storage_texture(0, 0, *component_layer_views[component][level]);
				cmd.set_storage_buffer(0, 1, *dequant_offset_buffer);

				if (linear_views[0])
				{
					// PUNKTFUNK (patch 0011): the caller's linear images.
					cmd.set_texture(0, 2, *linear_views[2]);
					cmd.set_texture(0, 3, *linear_views[1]);
					cmd.set_texture(0, 4, *linear_views[0]);
				}
				else if (payload_r8_image && payload_r16_image && payload_r32_image)
				{
					cmd.set_texture(0, 2, payload_r32_image->get_view());
					cmd.set_texture(0, 3, payload_r16_image->get_view());
					cmd.set_texture(0, 4, payload_r8_image->get_view());
				}
				else if (use_readonly_texel_buffer)
				{
					cmd.set_buffer_view(0, 2, *payload_u32_view);
					cmd.set_buffer_view(0, 3, *payload_u16_view);
					cmd.set_buffer_view(0, 4, *payload_u8_view);
				}
				else
					cmd.set_storage_buffer(0, 2, *payload_data);

				cmd.dispatch((push.resolution.x + 31) / 32, (push.resolution.y + 31) / 32, 1);
			}

			cmd.end_region();
		}
	}

	cmd.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
	            fragment_path ? VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT : VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
	            VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);

	auto end_dequant = cmd.write_timestamp(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);
	cmd.end_region();
	cmd.enable_subgroup_size_control(false);
	device->register_time_interval("GPU", std::move(start_dequant), std::move(end_dequant), "Dequant");

	return true;
}

bool Decoder::Impl::cpu_dq_setup()
{
	if (chroma != ChromaSubsampling::Chroma420 || half_output || fragment_path || in_place_base)
		return false;
	hq.init(width, height);
	// Write-combined (HOST_COHERENT, uncached) images need no cache flush; PYROWAVE_CPU_CACHED=1 restores the host-cached path.
	const bool cached = getenv("PYROWAVE_CPU_CACHED") && atoi(getenv("PYROWAVE_CPU_CACHED")) != 0;
	hq.wc = !cached;
	for (auto &p : hq.planes)
		if (p.w != int(wavelet_img_high_res->get_width(p.level)) || p.h != int(wavelet_img_high_res->get_height(p.level)))
			return false;
	for (int i = 0; i < CpuRing; i++)
	{
		for (int c = 0; c < NumComponents; c++)
			for (int l = 0; l < DecompositionLevels; l++)
			{
				if (l == 0 && c != 0)
					continue;
				auto info = ImageCreateInfo::immutable_2d_image(wavelet_img_high_res->get_width(l),
				                                                wavelet_img_high_res->get_height(l),
				                                                VK_FORMAT_R16_SFLOAT);
				info.domain = cached ? ImageDomain::LinearHostCached : ImageDomain::LinearHost;
				info.layers = NumFrequencyBandsPerLevel;
				info.usage = VK_IMAGE_USAGE_SAMPLED_BIT | VK_IMAGE_USAGE_STORAGE_BIT;
				info.initial_layout = VK_IMAGE_LAYOUT_PREINITIALIZED;
				info.misc |= IMAGE_MISC_LINEAR_IMAGE_IGNORE_DEVICE_LOCAL_BIT;
				info.layout = ImageLayout::General;
				cpu_lin[i][c][l] = device->create_image(info);
				if (!cpu_lin[i][c][l])
					return false;
				ImageViewCreateInfo vi = {};
				vi.image = cpu_lin[i][c][l].get();
				vi.view_type = VK_IMAGE_VIEW_TYPE_2D_ARRAY;
				vi.layers = NumFrequencyBandsPerLevel;
				vi.levels = 1;
				cpu_lin_view[i][c][l] = device->create_image_view(vi);
				vi.view_type = VK_IMAGE_VIEW_TYPE_2D;
				vi.layers = 1;
				cpu_lin_ll[i][c][l] = device->create_image_view(vi);
			}
		cpu_dirty[i].assign(hq.nblocks, 1);
		cpu_lin_fresh[i] = true;
	}
	// The calling thread is one worker; leave one core for the rest of the client.
	long ncpu = long(std::thread::hardware_concurrency());
#ifdef __linux__
	cpu_set_t set;
	if (sched_getaffinity(0, sizeof(set), &set) == 0)
		ncpu = CPU_COUNT(&set);
#endif
	int threads = int(std::max(1L, std::min(3L, ncpu - 1)));
	if (const char *t = getenv("PYROWAVE_CPU_DEQUANT_THREADS"))
		threads = std::max(1, atoi(t));
	hq.pool.start(&hq, threads - 1);
	return true;
}

int Decoder::Impl::cpu_free_slot(int avoid) const
{
	for (int k = 1; k <= CpuRing; k++)
	{
		int s = (cpu_slot + k) % CpuRing;
		if (s != gpu_slot[0] && s != gpu_slot[1] && s != avoid)
			return s;
	}
	return (cpu_slot + 1) % CpuRing;
}

// Dequantize into ring slot `slot`: hq.pool.work as built (listed), else every block hq.run picks.
bool Decoder::Impl::cpu_dq_into(int slot, const uint8_t *pay, bool listed)
{
	hq.dirty.swap(cpu_dirty[slot]);
	uint8_t *bases[NumComponents][DecompositionLevels] = {};
	for (int c = 0; c < NumComponents; c++)
		for (int l = 0; l < DecompositionLevels; l++)
			if (cpu_lin[slot][c][l])
				bases[c][l] = static_cast<uint8_t *>(device->map_image_memory(*cpu_lin[slot][c][l], MEMORY_ACCESS_WRITE_BIT));
	bool ok = true;
	for (auto &p : hq.planes)
	{
		VkImageSubresource sub = { VK_IMAGE_ASPECT_COLOR_BIT, 0, uint32_t(p.band) };
		VkSubresourceLayout lay;
		device->get_device_table().vkGetImageSubresourceLayout(device->get_device(),
		                                                       cpu_lin[slot][p.comp][p.level]->get_image(), &sub, &lay);
		p.ptr = reinterpret_cast<uint16_t *>(bases[p.comp][p.level] + lay.offset);
		p.ostride = int(lay.rowPitch / 2);
		p.clip = true;
		ok = ok && lay.rowPitch % 2 == 0;
	}
	if (ok)
	{
		if (listed)
			hq.pool.run(&hq, pay);
		else
			hq.run(pay);
	}
	for (int c = 0; c < NumComponents; c++)
		for (int l = 0; l < DecompositionLevels; l++)
			if (cpu_lin[slot][c][l])
				device->unmap_image_memory(*cpu_lin[slot][c][l], MEMORY_ACCESS_WRITE_BIT);
	hq.dirty.swap(cpu_dirty[slot]);
	return ok;
}

uint32_t Decoder::Impl::early_windows(uint32_t tag, const uint8_t *wins, size_t count, size_t wbytes)
{
	if (!cpu_dq_ready || hq.use_runs || !hq.wc || wbytes < 8)
		return 0;
	// Already decoded, or older than the frame in progress.
	if (any_claimed && int32_t(tag - claimed_tag) <= 0)
		return 0;
	if (early_slot >= 0 && tag != early_tag)
	{
		if (int32_t(tag - early_tag) < 0)
			return 0;
		early_slot = -1; // abandoned: the slot's dirty map still describes what it holds
	}
	if (early_slot < 0)
	{
		early_slot = cpu_free_slot(-1);
		early_tag = tag;
		early_done.assign(hq.nblocks, 0);
		early_offs.assign(hq.nblocks, UINT32_MAX);
		early_pay.clear();
		early_done_blocks = 0;
		// Coherent memory (hq.wc): mapped once for the frame, no flush owed.
		early_ptrs.clear();
		for (auto &p : hq.planes)
		{
			auto *base = static_cast<uint8_t *>(device->map_image_memory(*cpu_lin[early_slot][p.comp][p.level], MEMORY_ACCESS_WRITE_BIT));
			VkImageSubresource sub = { VK_IMAGE_ASPECT_COLOR_BIT, 0, uint32_t(p.band) };
			VkSubresourceLayout lay;
			device->get_device_table().vkGetImageSubresourceLayout(device->get_device(),
			                                                       cpu_lin[early_slot][p.comp][p.level]->get_image(), &sub, &lay);
			if (lay.rowPitch % 2 != 0)
			{
				early_slot = -1;
				return 0;
			}
			early_ptrs.emplace_back(reinterpret_cast<uint16_t *>(base + lay.offset), int(lay.rowPitch / 2));
		}
	}
	auto &work = hq.pool.work;
	work.clear();
	for (size_t w = 0; w < count; w++)
	{
		const uint8_t *win = wins + w * wbytes;
		uint16_t used, kind;
		memcpy(&used, win, 2);
		memcpy(&kind, win + 2, 2);
		if (kind != 0 || used == 0 || size_t(used) + 4 > wbytes)
			continue;
		const uint8_t *p = win + 4;
		size_t size = used;
		while (size >= 8)
		{
			uint32_t w0, w1;
			memcpy(&w0, p, 4);
			memcpy(&w1, p + 4, 4);
			if (w0 >> 31)
			{
				p += 8;
				size -= 8;
				continue;
			}
			uint32_t pw = (w0 >> 16) & 0xfff;
			uint32_t bi = w1 >> 8;
			if (pw < 2 || pw * 4 > size || bi >= uint32_t(hq.nblocks))
				break;
			if (!early_done[bi])
			{
				early_done[bi] = 1;
				early_offs[bi] = uint32_t(early_pay.size());
				size_t at = early_pay.size();
				early_pay.resize(at + pw);
				memcpy(early_pay.data() + at, p, pw * 4);
				work.push_back(int(bi));
			}
			p += pw * 4;
			size -= pw * 4;
		}
	}
	if (work.empty())
		return 0;
	// The NEON reader may read a few bytes past the last block.
	const size_t words = early_pay.size();
	early_pay.resize(words + 4, 0);
	for (size_t i = 0; i < hq.planes.size(); i++)
	{
		hq.planes[i].ptr = early_ptrs[i].first;
		hq.planes[i].ostride = early_ptrs[i].second;
		hq.planes[i].clip = true;
	}
	hq.offs.swap(early_offs);
	hq.dirty.swap(cpu_dirty[early_slot]);
	hq.pool.run(&hq, reinterpret_cast<const uint8_t *>(early_pay.data()));
	hq.dirty.swap(cpu_dirty[early_slot]);
	hq.offs.swap(early_offs);
	early_pay.resize(words);
	early_done_blocks += uint32_t(work.size());
	return uint32_t(work.size());
}

uint32_t Decoder::Impl::early_claim(uint32_t tag)
{
	any_claimed = true;
	claimed_tag = tag;
	early_use = early_slot >= 0 && early_tag == tag;
	if (early_slot >= 0 && int32_t(early_tag - tag) < 0)
		early_slot = -1;
	return early_use ? early_done_blocks : 0;
}

// PYROWAVE_EARLY_VERIFY=N: every Nth early frame, dequantize the whole AU again on one thread into
// a heap buffer and compare it with the slot. early_verify_bad = mismatching halves of the last check.
void Decoder::Impl::early_check(int slot)
{
	cpudq::Dequant ref;
	ref.init(width, height);
	ref.wc = false;
	std::vector<uint16_t> buf(ref.total_halves + 64);
	ref.bind_contiguous(buf.data());
	ref.offs = dequant_offset_buffer_cpu;
	ref.run(reinterpret_cast<const uint8_t *>(payload_data_cpu.data()));
	int bad = 0;
	for (size_t i = 0; i < hq.planes.size(); i++)
	{
		auto &p = hq.planes[i];
		auto &r = ref.planes[i];
		auto *base = static_cast<uint8_t *>(device->map_image_memory(*cpu_lin[slot][p.comp][p.level], MEMORY_ACCESS_READ_BIT));
		VkImageSubresource sub = { VK_IMAGE_ASPECT_COLOR_BIT, 0, uint32_t(p.band) };
		VkSubresourceLayout lay;
		device->get_device_table().vkGetImageSubresourceLayout(device->get_device(),
		                                                       cpu_lin[slot][p.comp][p.level]->get_image(), &sub, &lay);
		auto *img = reinterpret_cast<const uint16_t *>(base + lay.offset);
		for (int y = 0; y < p.h; y++)
			for (int x = 0; x < p.w; x++)
				bad += img[size_t(y) * (lay.rowPitch / 2) + x] != r.ptr[size_t(y) * r.ostride + x];
		device->unmap_image_memory(*cpu_lin[slot][p.comp][p.level], MEMORY_ACCESS_READ_BIT);
	}
	early_verify_bad = bad;
	LOGI("early dequant verify: %d mismatching coefficients\n", bad);
}

bool Decoder::Impl::cpu_dq_fill(CommandBuffer &cmd)
{
	const auto now_us = [] {
		return std::chrono::duration_cast<std::chrono::microseconds>(
				std::chrono::steady_clock::now().time_since_epoch()).count();
	};
	const auto t0 = now_us();
	const bool early = early_use && early_slot >= 0;
	early_use = false;
	const int slot = early ? early_slot : cpu_free_slot(early_slot);
	if (early)
		early_slot = -1;

	// The NEON reader may read a few bytes past the last block.
	const size_t payload_words = payload_data_cpu.size();
	payload_data_cpu.resize(payload_words + 4, 0);
	hq.offs = dequant_offset_buffer_cpu;
	if (early)
		hq.skip.swap(early_done);
	bool ok = cpu_dq_into(slot, reinterpret_cast<const uint8_t *>(payload_data_cpu.data()), false);
	if (early)
		hq.skip.swap(early_done);
	cpu_dq_us = uint32_t(now_us() - t0);
	if (ok && early && early_verify > 0 && ++early_verify_n % early_verify == 0)
		early_check(slot);
	payload_data_cpu.resize(payload_words);
	if (!ok)
		return false;
	cpu_slot = slot;
	gpu_slot[1] = gpu_slot[0];
	gpu_slot[0] = slot;

	for (int c = 0; c < NumComponents; c++)
		for (int l = 0; l < DecompositionLevels; l++)
			if (cpu_lin[slot][c][l])
				cmd.image_barrier(*cpu_lin[slot][c][l],
				                  cpu_lin_fresh[slot] ? VK_IMAGE_LAYOUT_PREINITIALIZED : VK_IMAGE_LAYOUT_GENERAL,
				                  VK_IMAGE_LAYOUT_GENERAL,
				                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
				                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
				                  VK_ACCESS_2_SHADER_SAMPLED_READ_BIT | VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);
	cpu_lin_fresh[slot] = false;
	return true;
}

bool Decoder::Impl::idwt_fragment(CommandBuffer &cmd, const ViewBuffers &views)
{
	const auto add_discard = [&](const Vulkan::Image *img) {
		if (img)
		{
			cmd.image_barrier(*img, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_ATTACHMENT_OPTIMAL,
			                  VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0,
			                  VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT);
		}
	};

	const auto add_read_only = [&](const Vulkan::Image *img) {
		if (img)
		{
			cmd.image_barrier(*img, VK_IMAGE_LAYOUT_ATTACHMENT_OPTIMAL, VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL,
			                  VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
			                  VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
		}
	};

	cmd.begin_barrier_batch();
	for (auto &level : fragment.levels)
	{
		for (auto &vert : level.vert)
			for (auto &comp : vert)
				add_discard(comp.get());
		for (auto &comp : level.horiz)
			add_discard(comp.get());
	}
	cmd.end_barrier_batch();

	auto start_idwt = cmd.write_timestamp(VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT);

	struct Push
	{
		float u_offset;
		float v_offset;
		float half_texel_offset_u;
		float half_texel_offset_v;
		float vp_scale;
		uint32_t pivot_size;
	} push = {};

	for (int input_level = DecompositionLevels - 1; input_level >= 0; input_level--)
	{
		int output_level = input_level - 1;

		char label[128];
		if (output_level >= 0)
			snprintf(label, sizeof(label), "Fragment iDWT level %u", output_level);
		else
			snprintf(label, sizeof(label), "Fragment iDWT final");
		cmd.begin_region(label);

		Vulkan::RenderPassInfo rp_info = {};

		bool has_chroma_output = output_level >= 0 || chroma == ChromaSubsampling::Chroma444;

		Vulkan::Program *vert_prog;
		Vulkan::Program *horiz_prog;

		if (has_chroma_output)
		{
			rp_info.store_attachments = 0x3;
			rp_info.num_color_attachments = 2;
			vert_prog = device->request_program(shaders.idwt_vs, shaders.idwt_fs[1]);
			horiz_prog = device->request_program(shaders.idwt_vs, shaders.idwt_fs[2]);
		}
		else
		{
			rp_info.store_attachments = 0x1;
			rp_info.num_color_attachments = 1;
			vert_prog = device->request_program(shaders.idwt_vs, shaders.idwt_fs[0]);
			horiz_prog = vert_prog;
		}

		// Vertical passes.
		for (int vert_pass = 0; vert_pass < 2; vert_pass++)
		{
			rp_info.color_attachments[0] = &fragment.levels[input_level].vert[vert_pass][0]->get_view();
			if (has_chroma_output)
				rp_info.color_attachments[1] = &fragment.levels[input_level].vert[vert_pass][1]->get_view();

			cmd.begin_render_pass(rp_info);
			cmd.set_program(vert_prog);
			cmd.set_opaque_sprite_state();
			cmd.set_specialization_constant_mask(0x1 | 0x8);
			cmd.set_specialization_constant(0, true);

			cmd.set_texture(0, 0, *fragment.levels[input_level].decoded[0][vert_pass + 0]);
			cmd.set_texture(0, 1, *fragment.levels[input_level].decoded[0][vert_pass + 2]);
			cmd.set_sampler(0, 2, *mirror_repeat_sampler);

			if (has_chroma_output)
			{
				cmd.set_texture(0, 3, *fragment.levels[input_level].decoded[1][vert_pass + 0]);
				cmd.set_texture(0, 4, *fragment.levels[input_level].decoded[1][vert_pass + 2]);
				cmd.set_texture(0, 5, *fragment.levels[input_level].decoded[2][vert_pass + 0]);
				cmd.set_texture(0, 6, *fragment.levels[input_level].decoded[2][vert_pass + 2]);
			}

			uint32_t render_width = rp_info.color_attachments[0]->get_view_width();
			uint32_t render_height = rp_info.color_attachments[0]->get_view_height();

			// Set mirror point.
			// Work around broken Mali r38.1 compiler.
			// If it sees negative texture offsets it breaks the output for whatever reason (!?!?!?!).
			auto *input_view = fragment.levels[input_level].decoded[0][0].get();
			push.u_offset = 0.0f;
			push.v_offset = -2.0f / float(input_view->get_view_height());
			push.half_texel_offset_u = 0.5f / float(input_view->get_view_width());
			push.half_texel_offset_v = 0.5f / float(input_view->get_view_height());
			push.vp_scale = cmd.get_viewport().height;
			push.pivot_size = render_height;
			cmd.push_constants(&push, 0, sizeof(push));

			// Render top edge condition.
			cmd.set_specialization_constant(3, -1);
			cmd.set_scissor({{ 0, 0 }, { render_width, 8 }});
			cmd.draw(3);

			// Render normal path
			cmd.set_specialization_constant(3, 0);
			cmd.set_scissor({{ 0, 8 }, { render_width, render_height - 16 }});
			cmd.draw(3);

			// Render bottom edge condition
			cmd.set_specialization_constant(3, +1);
			cmd.set_scissor({{ 0, int(render_height) - 8 }, { render_width, 8 }});
			cmd.draw(3);

			cmd.end_render_pass();
		}

		cmd.begin_barrier_batch();
		for (auto &vert : fragment.levels[input_level].vert)
			for (auto &comp : vert)
				add_read_only(comp.get());
		cmd.end_barrier_batch();

		if (has_chroma_output)
		{
			rp_info.num_color_attachments = 3;
			rp_info.store_attachments = 0x7;
		}
		else
		{
			rp_info.num_color_attachments = 1;
			rp_info.store_attachments = 0x1;
		}

		for (uint32_t comp = 0; comp < rp_info.num_color_attachments; comp++)
		{
			if (output_level < 0 || (output_level == 0 && chroma == ChromaSubsampling::Chroma420 && comp != 0))
				rp_info.color_attachments[comp] = views.planes[comp];
			else
				rp_info.color_attachments[comp] = &fragment.levels[output_level].horiz[comp]->get_view();
		}

		cmd.begin_render_pass(rp_info);
		cmd.set_program(horiz_prog);
		cmd.set_opaque_sprite_state();
		cmd.set_specialization_constant_mask(0xf);
		cmd.set_specialization_constant(0, false);
		cmd.set_specialization_constant(1, output_level < 0);
		cmd.set_specialization_constant(2, output_level < 0 || (output_level == 0 && chroma == ChromaSubsampling::Chroma420));

		cmd.set_texture(0, 0, fragment.levels[input_level].vert[0][0]->get_view());
		cmd.set_texture(0, 1, fragment.levels[input_level].vert[1][0]->get_view());
		cmd.set_sampler(0, 2, *mirror_repeat_sampler);

		if (has_chroma_output)
		{
			cmd.set_texture(0, 3, fragment.levels[input_level].vert[0][1]->get_view());
			cmd.set_texture(0, 4, fragment.levels[input_level].vert[1][1]->get_view());
		}

		uint32_t aligned_render_width = aligned_width >> (output_level + 1);
		uint32_t aligned_render_height = aligned_height >> (output_level + 1);

		// Chroma output might be smaller than Y in output_level == 0 due to not using alignment.
		// This is reflected in the actual render area, which is equal to default viewport.
		auto render_width = uint32_t(cmd.get_viewport().width);
		auto render_height = uint32_t(cmd.get_viewport().height);

		// In case we're rendering to an output texture,
		// the render area might be smaller than we expect for purposes of alignment.
		// Use properly scaled viewport that we scissor away as needed.
		cmd.set_viewport({ 0, 0, float(aligned_render_width), float(aligned_render_height), 0, 1 });

		// Set mirror point.
		auto *input_view = &fragment.levels[input_level].vert[0][0]->get_view();
		push.u_offset = -2.0f / float(input_view->get_view_width());
		push.v_offset = 0.0f;
		push.half_texel_offset_u = 0.5f / float(input_view->get_view_width());
		push.half_texel_offset_v = 0.5f / float(input_view->get_view_height());
		push.vp_scale = cmd.get_viewport().width;
		push.pivot_size = aligned_render_width;
		cmd.push_constants(&push, 0, sizeof(push));

		// Render left edge condition.
		cmd.set_specialization_constant(3, -1);
		cmd.set_scissor({{ 0, 0 }, { 8, render_height }});
		cmd.draw(3);

		// Render normal condition
		cmd.set_specialization_constant(3, 0);
		cmd.set_scissor({{ 8, 0 }, { std::min<uint32_t>(render_width - 8, aligned_render_width - 16), render_height }});
		cmd.draw(3);

		uint32_t aligned_x = aligned_render_width - 8;
		if (aligned_x < render_width)
		{
			// Render right edge condition
			cmd.set_specialization_constant(3, +1);
			cmd.set_scissor({{ int(aligned_x), 0 }, { render_width - aligned_x, render_height }});
			cmd.draw(3);
		}

		cmd.end_render_pass();

		// If chroma is subsampled, we cannot render the fully padded region in one render pass due to
		// rules regarding renderArea. renderArea cannot exceed the smallest image in the render pass.
		// We cannot use subpasses either, so split the render pass, but that's mostly fine,
		// since renderArea is non-overlapping.
		if (output_level == 0 && chroma == ChromaSubsampling::Chroma420)
		{
			rp_info.num_color_attachments = 1;
			rp_info.store_attachments = 0x1;

			VkMemoryBarrier2 by_region = { VK_STRUCTURE_TYPE_MEMORY_BARRIER_2 };
			by_region.srcStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
			by_region.dstStageMask = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
			by_region.srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
			by_region.dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT;
			VkDependencyInfo dep = { VK_STRUCTURE_TYPE_DEPENDENCY_INFO };
			dep.memoryBarrierCount = 1;
			dep.pMemoryBarriers = &by_region;
			dep.dependencyFlags = VK_DEPENDENCY_BY_REGION_BIT;

			// Need vertical fixup (very common for 1080p).
			if (rp_info.color_attachments[1]->get_view_height() < rp_info.color_attachments[0]->get_view_height())
			{
				// Insert a simple by_region barrier to ensure we follow Vulkan rules for RW access.
				cmd.barrier(dep);

				rp_info.render_area.extent.width = rp_info.color_attachments[0]->get_view_width();
				rp_info.render_area.extent.height =
						rp_info.color_attachments[0]->get_view_height() -
						rp_info.color_attachments[1]->get_view_height();
				rp_info.render_area.offset.x = 0;
				rp_info.render_area.offset.y = rp_info.color_attachments[1]->get_view_height();

				cmd.begin_render_pass(rp_info);
				cmd.set_program(device->request_program(shaders.idwt_vs, shaders.idwt_fs[0]));
				cmd.set_opaque_sprite_state();
				cmd.set_texture(0, 0, fragment.levels[input_level].vert[0][0]->get_view());
				cmd.set_texture(0, 1, fragment.levels[input_level].vert[1][0]->get_view());
				cmd.set_sampler(0, 2, *mirror_repeat_sampler);
				cmd.set_viewport({ 0, 0, float(aligned_render_width), float(aligned_render_height), 0, 1 });
				cmd.set_specialization_constant_mask(0x8);
				cmd.push_constants(&push, 0, sizeof(push));
				cmd.set_specialization_constant(3, 1); // Always consider edge handling.
				cmd.draw(3);
				cmd.end_render_pass();
			}

			// Need horizontal fixup (very rare).
			if (rp_info.color_attachments[1]->get_view_width() < rp_info.color_attachments[0]->get_view_width())
			{
				cmd.barrier(dep);

				rp_info.render_area.extent.width =
						rp_info.color_attachments[0]->get_view_width() -
						rp_info.color_attachments[1]->get_view_width();
				rp_info.render_area.extent.height = rp_info.color_attachments[0]->get_view_height();
				rp_info.render_area.offset.x = rp_info.color_attachments[1]->get_view_width();
				rp_info.render_area.offset.y = 0;

				cmd.begin_render_pass(rp_info);
				cmd.set_program(device->request_program(shaders.idwt_vs, shaders.idwt_fs[0]));
				cmd.set_opaque_sprite_state();
				cmd.set_texture(0, 0, fragment.levels[input_level].vert[0][0]->get_view());
				cmd.set_texture(0, 1, fragment.levels[input_level].vert[1][0]->get_view());
				cmd.set_sampler(0, 2, *mirror_repeat_sampler);
				cmd.set_viewport({ 0, 0, float(aligned_render_width), float(aligned_render_height), 0, 1 });
				cmd.set_specialization_constant_mask(0x8);
				cmd.push_constants(&push, 0, sizeof(push));
				cmd.set_specialization_constant(3, 1); // Always consider edge handling.
				cmd.draw(3);
				cmd.end_render_pass();
			}
		}

		if (output_level >= 0)
		{
			cmd.begin_barrier_batch();
			for (auto &comp: fragment.levels[output_level].horiz)
				add_read_only(comp.get());
			cmd.end_barrier_batch();
		}

		cmd.end_region();
	}

	auto end_idwt = cmd.write_timestamp(VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT);
	device->register_time_interval("GPU", std::move(start_idwt), std::move(end_idwt), "iDWT fragment");

	cmd.set_specialization_constant_mask(0);

	// Avoid WAR hazard for dequantization.
	cmd.barrier(VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0);

	return true;
}

bool Decoder::Impl::use_idwt_shuf() const
{
	return shaders.idwt_shuf && Configuration::get().get_precision() == 0 &&
	       device->supports_subgroup_size_log2(true, 3, 3) && !getenv("PYROWAVE_IDWT_SHARED");
}

void Decoder::Impl::update_tile_flags()
{
	const int R = 16;
	if (!tile_flag_buffer)
	{
		uint32_t align = std::max<uint32_t>(
				1u, uint32_t(device->get_gpu_properties().limits.minStorageBufferOffsetAlignment / sizeof(uint32_t)));
		uint32_t total = 0;
		for (int level = 0; level < DecompositionLevels; level++)
		{
			int w = component_layer_views[0][level]->get_view_width();
			int h = component_layer_views[0][level]->get_view_height();
			tile_flag_words[level] = uint32_t(((h + R - 1) / R) * (((2 * w + 23) / 24 + 7) / 8));
			for (int c = 0; c < NumComponents; c++)
			{
				tile_flag_offset[c][level] = total;
				total += (tile_flag_words[level] + align - 1) / align * align;
			}
		}
		tile_flags_cpu.assign(total, ~0u);

		BufferCreateInfo info;
		info.domain = BufferDomain::Device;
		info.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_DST_BIT;
		info.size = total * sizeof(uint32_t);
		tile_flag_buffer = device->create_buffer(info);
		device->set_name(*tile_flag_buffer, "tile-flags");
	}

	static const float min_skip = getenv("PYROWAVE_TILE_MIN") ? float(atof(getenv("PYROWAVE_TILE_MIN"))) : 0.15f;
	any_tile_use = false;

	for (int level = 0; level < DecompositionLevels; level++)
	{
		int w = component_layer_views[0][level]->get_view_width();
		int h = component_layer_views[0][level]->get_view_height();
		int gx = (h + R - 1) / R;
		int gy = ((2 * w + 23) / 24 + 7) / 8;

		for (int c = 0; c < NumComponents; c++)
		{
			tile_use[c][level] = false;
			if (level == 0 && c != 0 && chroma == ChromaSubsampling::Chroma420)
				continue;

			uint32_t active = 7u & ~uint32_t(band_skip[c][level]);
			if (!active)
				continue;
			unsigned reads = 0, skipped = 0;
			uint32_t *out = &tile_flags_cpu[tile_flag_offset[c][level]];
			uint64_t rows[3][64];
			int block_rows = (h + 31) / 32;
			if ((w + 31) / 32 > 64 || block_rows > 64)
				continue;
			for (int band = 1; band < 4; band++)
			{
				auto &meta = block_meta[c][level][band];
				for (int by = 0; by < block_rows; by++)
				{
					uint64_t bits = 0;
					const uint32_t *offs = &dequant_offset_buffer_cpu[meta.block_offset_32x32 + by * meta.block_stride_32x32];
					for (int bx = 0; bx < meta.block_stride_32x32; bx++)
						bits |= uint64_t(offs[bx] != UINT32_MAX) << bx;
					rows[band - 1][by] = bits;
				}
			}
			for (int wx = 0; wx < gx; wx++)
			{
				int r0 = std::max(wx * R - 2, 0) >> 5;
				int r1 = std::min(wx * R + R + 1, h - 1) >> 5;
				uint64_t span[3] = {};
				for (int b = 0; b < 3; b++)
					for (int by = r0; by <= r1; by++)
						span[b] |= rows[b][by];
				for (int wy = 0; wy < gy; wy++)
				{
					uint32_t word = 0;
					for (int s = 0; s < 8; s++)
					{
						int tx = wy * 8 + s;
						int c0 = std::max(tx * 12 - 2, 0);
						int c1 = std::min(tx * 12 + 13, w - 1);
						if (c0 > c1)
							continue;
						c0 >>= 5;
						c1 >>= 5;
						uint64_t cols = ((uint64_t(2) << (c1 - c0)) - 1) << c0;
						for (int b = 0; b < 3; b++)
						{
							if (!(active & (1u << b)))
								continue;
							reads++;
							if (span[b] & cols)
								word |= 1u << (3 * s + b);
							else
								skipped++;
						}
					}
					out[wy * gx + wx] = word;
				}
			}
			tile_use[c][level] = reads && float(skipped) >= min_skip * float(reads);
			any_tile_use = any_tile_use || tile_use[c][level];
		}
	}

	static const bool tile_stats = getenv("PYROWAVE_TILE_STATS") != nullptr;
	static bool printed;
	if (tile_stats && !printed)
	{
		printed = true;
		for (int level = 0; level < DecompositionLevels; level++)
		{
			for (int c = 0; c < NumComponents; c++)
			{
				unsigned present[3] = {}, total = 0;
				for (uint32_t i = 0; i < tile_flag_words[level]; i++)
				{
					uint32_t word = tile_flags_cpu[tile_flag_offset[c][level] + i];
					for (int s = 0; s < 8; s++)
					{
						total++;
						for (int b = 0; b < 3; b++)
							present[b] += (word >> (3 * s + b)) & 1u;
					}
				}
				LOGI("tiles L%d c%d skipmask %x use %d: present %u/%u/%u of %u\n", level, c, band_skip[c][level],
				     int(tile_use[c][level]), present[0], present[1], present[2], total);
			}
		}
	}
}

bool Decoder::Impl::is_nv12(const ViewBuffers &views) const
{
	return !views.planes[2] && chroma == ChromaSubsampling::Chroma420 && use_idwt_shuf() &&
	       shaders.idwt_shuf_merge[0] && views.planes[1]->get_format() == VK_FORMAT_R32_UINT;
}

bool Decoder::Impl::idwt(CommandBuffer &cmd, const ViewBuffers &views)
{
	const bool shuf = use_idwt_shuf();
	const bool nv12 = is_nv12(views);
	if (nv12 && (!nv12_cb || nv12_cb->get_height() != views.planes[1]->get_view_height()))
	{
		auto info = ImageCreateInfo::immutable_2d_image(views.planes[1]->get_view_width() / 2,
		                                                views.planes[1]->get_view_height(), VK_FORMAT_R32_UINT);
		info.usage = VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_SAMPLED_BIT;
		info.initial_layout = VK_IMAGE_LAYOUT_UNDEFINED;
		info.layout = ImageLayout::General;
		nv12_cb = device->create_image(info);
	}
	if (nv12)
		cmd.image_barrier(*nv12_cb, VK_IMAGE_LAYOUT_UNDEFINED, VK_IMAGE_LAYOUT_GENERAL,
		                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, 0,
		                  VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);

	bool packed[NumComponents];
	for (int c = 0; c < NumComponents; c++)
	{
		packed[c] = nv12 || views.planes[c]->get_format() == VK_FORMAT_R32_UINT;
		if (packed[c] && !shuf)
			return false;
	}
	auto select_program = [&](int c, int level, bool final_out = false) {
		if (!shuf)
			return;
		const bool pk = final_out && packed[c];
		if (final_out && nv12 && c == 2)
		{
			cmd.set_texture(0, 3, nv12_cb->get_view(), StockSampler::NearestClamp);
			if (tile_use[c][level])
			{
				cmd.set_program(shaders.idwt_shuf_merge_tile[band_skip[c][level]]);
				cmd.set_storage_buffer(0, 2, *tile_flag_buffer, tile_flag_offset[c][level] * sizeof(uint32_t),
				                       tile_flag_words[level] * sizeof(uint32_t));
			}
			else
				cmd.set_program(shaders.idwt_shuf_merge[band_skip[c][level]]);
		}
		else if (tile_use[c][level])
		{
			cmd.set_program(pk ? shaders.idwt_shuf_packed_tile[band_skip[c][level]] : shaders.idwt_shuf_tile[band_skip[c][level]]);
			cmd.set_storage_buffer(0, 2, *tile_flag_buffer, tile_flag_offset[c][level] * sizeof(uint32_t),
			                       tile_flag_words[level] * sizeof(uint32_t));
		}
		else if (pk)
			cmd.set_program(shaders.idwt_shuf_packed[band_skip[c][level]]);
		else
			cmd.set_program(band_skip[c][level] ? shaders.idwt_shuf_skip[band_skip[c][level]] : shaders.idwt_shuf);
	};
	if (shuf)
	{
		cmd.set_program(shaders.idwt_shuf);
		cmd.enable_subgroup_size_control(true);
		cmd.set_subgroup_size_log2(true, 3, 3);
	}
	else
	{
		cmd.set_program(shaders.idwt[Configuration::get().get_precision()]);
		cmd.enable_subgroup_size_control(false);
	}
	auto groups_y = [shuf](int res_y) { return shuf ? ((2 * res_y + 23) / 24 + 7) / 8 : (res_y + 15) / 16; };

	auto start_idwt = cmd.write_timestamp(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);

	struct
	{
		ivec2 resolution;
		vec2 inv_resolution;
	} push = {};

	const int lowest_level = half_output ? 1 : 0;
	for (int input_level = DecompositionLevels - 1; input_level >= lowest_level; input_level--)
	{
		// Transposed.
		push.resolution.x = component_layer_views[0][input_level]->get_view_height();
		push.resolution.y = component_layer_views[0][input_level]->get_view_width();
		push.inv_resolution.x = 1.0f / float(push.resolution.x);
		push.inv_resolution.y = 1.0f / float(push.resolution.y);
		cmd.push_constants(&push, 0, sizeof(push));
		cmd.set_specialization_constant_mask(1);
		cmd.set_specialization_constant(0, false);

		if (input_level == 0)
		{
			cmd.set_specialization_constant(0, true);
			if (chroma == ChromaSubsampling::Chroma444)
			{
				for (int c = 0; c < NumComponents; c++)
				{
					char label[64];
					snprintf(label, sizeof(label), "iDWT final, component %u", c);
					cmd.begin_region(label);
					cmd.set_storage_texture(0, 1, *views.planes[c]);
					cmd.set_texture(0, 0, cpu_dq ? *cpu_lin_view[cpu_slot][c][input_level] : *component_layer_views[c][input_level], *mirror_repeat_sampler);
					select_program(c, input_level, true);
					cmd.dispatch((push.resolution.x + 15) / 16, groups_y(push.resolution.y), 1);
					cmd.end_region();
				}
			}
			else
			{
				cmd.set_storage_texture(0, 1, *views.planes[0]);
				cmd.begin_region("iDWT final");
				cmd.set_texture(0, 0, cpu_dq ? *cpu_lin_view[cpu_slot][0][input_level] : *component_layer_views[0][input_level], *mirror_repeat_sampler);
				select_program(0, input_level, true);
				cmd.dispatch((push.resolution.x + 15) / 16, groups_y(push.resolution.y), 1);
				cmd.end_region();
			}
		}
		else
		{
			for (int c = 0; c < NumComponents; c++)
			{
				const int final_level = (chroma == ChromaSubsampling::Chroma420 && c != 0 ? 1 : 0) + (half_output ? 1 : 0);
				if (input_level < final_level)
					continue;

				cmd.set_texture(0, 0, cpu_dq ? *cpu_lin_view[cpu_slot][c][input_level] : *component_layer_views[c][input_level], *mirror_repeat_sampler);

				if (input_level == final_level)
				{
					if (nv12 && c == 2)
						cmd.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
						            VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
					cmd.set_storage_texture(0, 1, nv12 && c == 1 ? nv12_cb->get_view() : *views.planes[nv12 ? 1 : c]);
					cmd.set_specialization_constant(0, true);
				}
				else
				{
					cmd.set_storage_texture(0, 1, cpu_dq ? *cpu_lin_ll[cpu_slot][c][input_level - 1] : *component_ll_views[c][input_level - 1]);
					cmd.set_specialization_constant(0, false);
				}

				char label[64];
				snprintf(label, sizeof(label), "iDWT level %u, component %u", input_level - 1, c);
				cmd.begin_region(label);
				select_program(c, input_level, input_level == final_level);
				cmd.dispatch((push.resolution.x + 15) / 16, groups_y(push.resolution.y), 1);
				cmd.end_region();
			}
		}

		cmd.set_specialization_constant_mask(0);
		cmd.barrier(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
		            VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
	}

	if (shuf)
		cmd.enable_subgroup_size_control(false);

	auto end_idwt = cmd.write_timestamp(VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT);
	device->register_time_interval("GPU", std::move(start_idwt), std::move(end_idwt), "iDWT");
	return true;
}

bool Decoder::Impl::has_pristine_bands(int bands, const uint32_t *active_block_mask, size_t word_count) const
{
	// Account for 4:2:0 where level0 will not have packets.
	// It's somewhat meaningless to ask for pristine bands all the way up to that point though.
	assert(bands < DecompositionLevels);

	// This analysis assumes that there are no "null" blocks present.
	// For the lowest frequency bands, that is vanishingly unlikely to happen,
	// and worst case we get a false positive rejection.
	// TODO: It's possible to improve this by sending the "active block mask"
	// as sideband data from encoder if need be.

	const auto block_is_missing = [&](uint32_t block_index)
	{
		if (dequant_offset_buffer_cpu[block_index] != UINT32_MAX)
			return false;

		uint32_t word_index = block_index / 32;

		if (!active_block_mask || word_index >= word_count)
			return true;

		// If the block wasn't expected to be active anyway, just pass it through.
		return ((active_block_mask[word_index] >> (block_index % 32)) & 1) != 0;
	};

	for (int band = 0; band < bands; band++)
	{
		for (auto &component : block_meta)
		{
			if (band == 0)
			{
				auto &meta = component[DecompositionLevels - 1][0];
				for (int i = 0; i < meta.block_count_32x32; i++)
					if (block_is_missing(meta.block_offset_32x32 + i))
						return false;
			}
			else
			{
				// If we can reconstruct the LH, HL, HH bands, we can generate the higher-resolution LL band.
				for (int high_freq_bands = 1; high_freq_bands < 4; high_freq_bands++)
				{
					auto &meta = component[DecompositionLevels - band][high_freq_bands];
					for (int i = 0; i < meta.block_count_32x32; i++)
						if (block_is_missing(meta.block_offset_32x32 + i))
							return false;
				}
			}
		}
	}

	return true;
}

bool Decoder::Impl::decode_is_ready(bool allow_partial_frame, int pristine_bands, float received_ratio,
                                    const uint32_t *active_block_mask, size_t word_count) const
{
	if (decoded_frame_for_current_sequence)
		return false;

	if (last_seq == UINT32_MAX)
		return false;

	if (decoded_blocks < total_blocks_in_sequence)
	{
		if (!allow_partial_frame)
			return false;

		if (!has_pristine_bands(pristine_bands, active_block_mask, word_count))
			return false;
		if (float(decoded_blocks) <= float(total_blocks_in_sequence) * received_ratio)
			return false;
	}

	return true;
}

bool Decoder::Impl::decode_is_ready(bool allow_partial_frame) const
{
	// At the very least, we want some LL bands to be received properly,
	// otherwise we get extreme artifacts.
	return decode_is_ready(allow_partial_frame, 2, 0.9f, nullptr, 0);
}

bool Decoder::Impl::decode(CommandBuffer &cmd, const ViewBuffers &views)
{
	if (cpu_dq && !cpu_dq_ready)
	{
		cpu_dq_ready = cpu_dq_setup();
		if (!cpu_dq_ready)
		{
			LOGE("CPU dequant unavailable, using the GPU path.\n");
			cpu_dq = false;
		}
	}

	cmd.begin_region("Decode uploads");
	{
		if (!cpu_dq && !upload_payload(cmd))
		{
			cmd.end_region();
			return false;
		}

		uint32_t *offsets;
		if (cpu_dq)
		{
			cpu_offsets_scratch.resize(dequant_offset_buffer_cpu.size());
			offsets = cpu_offsets_scratch.data();
		}
		else
			offsets = static_cast<uint32_t *>(cmd.update_buffer(*dequant_offset_buffer, 0,
			                         dequant_offset_buffer_cpu.size() * sizeof(dequant_offset_buffer_cpu.front())));
		const bool skip_bands = skip_bands_enabled && !fragment_path && use_idwt_shuf();
		for (int c = 0; c < NumComponents; c++)
		{
			for (int level = 0; level < DecompositionLevels; level++)
			{
				band_skip[c][level] = 0;
				for (int band = 0; band < 4; band++)
				{
					auto &meta = block_meta[c][level][band];
					int begin = meta.block_offset_32x32;
					int end = begin + meta.block_count_32x32;
					bool any = false;
					for (int i = begin; i < end && !any; i++)
						any = dequant_offset_buffer_cpu[i] != UINT32_MAX;
					bool skip = skip_bands && band != 0 && !any;
					if (skip)
						band_skip[c][level] |= uint8_t(1u << (band - 1));

					for (int i = begin; i < end; i++)
					{
						uint32_t offset = dequant_offset_buffer_cpu[i];
						bool empty = offset == UINT32_MAX;
						if (!skip)
						{
							if (empty && (block_dirty[i] || !zero_once))
								offset = UINT32_MAX - 1;
							block_dirty[i] = !empty;
						}
						offsets[i] = offset;
					}
				}
			}
		}
		static const bool skip_stats = getenv("PYROWAVE_SKIP_STATS") != nullptr;
		if (skip_stats)
		{
			static bool printed;
			if (!printed)
			{
				printed = true;
				for (int c = 0; c < NumComponents; c++)
					LOGI("band skip mask c%d: L0 %x L1 %x L2 %x L3 %x L4 %x\n", c, band_skip[c][0], band_skip[c][1],
					     band_skip[c][2], band_skip[c][3], band_skip[c][4]);
			}
		}

		if (skip_tiles_enabled && shaders.idwt_shuf_tile[0] && !fragment_path && use_idwt_shuf())
		{
			update_tile_flags();
			if (any_tile_use)
				memcpy(cmd.update_buffer(*tile_flag_buffer, 0, tile_flags_cpu.size() * sizeof(uint32_t)),
				       tile_flags_cpu.data(), tile_flags_cpu.size() * sizeof(uint32_t));
		}

		cmd.barrier(VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT,
		            VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT,
		            VK_ACCESS_2_SHADER_STORAGE_READ_BIT |
		            (use_readonly_texel_buffer ? VK_ACCESS_2_SHADER_SAMPLED_READ_BIT : 0));
	}
	cmd.end_region();

	static const int stages = getenv("PYROWAVE_STAGES") ? atoi(getenv("PYROWAVE_STAGES")) : 2;
	if (stages < 1)
		return true;

	if (cpu_dq)
	{
		if (!cpu_dq_fill(cmd))
			return false;
	}
	else if (!dequant(cmd))
		return false;

	if (stages < 2)
		return true;

	cmd.barrier(VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT, 0, VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_TRANSFER_WRITE_BIT);

	if (fragment_path)
	{
		if (!idwt_fragment(cmd, views))
			return false;
	}
	else
	{
		if (!idwt(cmd, views))
			return false;
	}

	decoded_frame_for_current_sequence = true;
	return true;
}

void Decoder::Impl::clear()
{
	std::fill(dequant_offset_buffer_cpu.begin(), dequant_offset_buffer_cpu.end(), UINT32_MAX);
	decoded_blocks = 0;
	last_seq = UINT32_MAX;
	decoded_frame_for_current_sequence = false;
	total_blocks_in_sequence = block_count_32x32;
	payload_data_cpu.clear();
}

bool Decoder::device_prefers_fragment_path(Vulkan::Device &device)
{
	switch (device.get_device_features().driver_id)
	{
	// QCOM hardware struggles with compute in general and prefers fragment.
	// Turnip seems to like compute path just fine though ...
	case VK_DRIVER_ID_QUALCOMM_PROPRIETARY:
		return true;

	// Mali heavily favors texture sampling over LS heavy content.
	case VK_DRIVER_ID_ARM_PROPRIETARY:
	case VK_DRIVER_ID_MESA_PANVK:
		return true;

	default:
		return false;
	}
}

void Decoder::Impl::check_linear_texture_support()
{
	if (!use_readonly_texel_buffer)
		return;
	if (const char *env = getenv("PYROWAVE_PAYLOAD"))
		if (strcmp(env, "linear") != 0)
			return;

	// Texel buffers hit LS path on at least Mali, and most likely they hit slow paths on most mobile IHVs.
	// Try to promote to linear 2D images instead if we can get away with it.
	// Texture sampling performance is what mobile IHVs tend to optimize for.
	const struct
	{
		VkFormat fmt;
		uint32_t width;
		ImageHandle *out_handle;
	} reqs[] = {
		{ VK_FORMAT_R8_UINT, 4096, &payload_r8_image },
		{ VK_FORMAT_R16_UINT, 2048, &payload_r16_image },
		{ VK_FORMAT_R32_UINT, 1024, &payload_r32_image }
	};

	// Just assume this works. Can't imagine any GPU where this wouldn't work tightly packed.

	BufferCreateInfo bufinfo;
	bufinfo.size = 4 * 1024 * 1024;
	bufinfo.usage =
			VK_BUFFER_USAGE_TRANSFER_DST_BIT |
			VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
			VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT;
	bufinfo.domain = BufferDomain::Device;
	bufinfo.allocation_requirements.alignment = 64 * 1024;
	bufinfo.allocation_requirements.size = 4 * 1024 * 1024;
	bufinfo.allocation_requirements.memoryTypeBits = UINT32_MAX;
	payload_data = device->create_buffer(bufinfo);
	device->set_name(*payload_data, "payload-data");

	const auto *alias = &payload_data->get_allocation();

	// Try to force all linear images to alias each other.
	for (auto &req : reqs)
	{
		VkImageFormatProperties2 props2 = { VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2 };
		if (device->get_image_format_properties(req.fmt, VK_IMAGE_TYPE_2D, VK_IMAGE_TILING_LINEAR,
		                                        VK_IMAGE_USAGE_SAMPLED_BIT, 0,
		                                        nullptr, &props2))
		{
			if (props2.imageFormatProperties.maxExtent.width >= req.width &&
			    props2.imageFormatProperties.maxExtent.height >= 1024)
			{
				auto info = ImageCreateInfo::immutable_2d_image(req.width, 1024, req.fmt);
				info.domain = ImageDomain::LinearHost;
				info.usage = VK_IMAGE_USAGE_SAMPLED_BIT;
				info.num_memory_aliases = 1;
				info.layout = ImageLayout::General;
				info.memory_aliases = &alias;
				*req.out_handle = device->create_image(info);
			}
		}
	}

	if (payload_r8_image && payload_r16_image && payload_r32_image)
		LOGI("Using linear textures instead of texel buffers.\n");
}

// PUNKTFUNK (patch 0011): see Decoder::set_payload_memory. On failure the decoder keeps no
// caller handle and rebuilds its own payload on the next decode.
bool Decoder::Impl::set_payload_memory(uint64_t buffer, uint64_t size, uint64_t copy_reserve, void *host,
                                       const uint64_t linear[3], uint32_t rows)
{
	payload_r8_image.reset();
	payload_r16_image.reset();
	payload_r32_image.reset();
	payload_u8_view.reset();
	payload_u16_view.reset();
	payload_u32_view.reset();
	for (auto &v : linear_views)
		v.reset();
	payload_data.reset();
	in_place_base = nullptr;
	need_image_transition = true;

	if (!buffer || !host || copy_reserve + 16 > size)
		return false;

	BufferCreateInfo info = {};
	info.size = size;
	info.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT |
	             VK_BUFFER_USAGE_STORAGE_BUFFER_BIT |
	             VK_BUFFER_USAGE_UNIFORM_TEXEL_BUFFER_BIT;
	info.domain = BufferDomain::Device;
	payload_data = device->wrap_buffer(info, (VkBuffer)(uintptr_t)buffer, false);
	if (!payload_data)
		return false;

	if (use_readonly_texel_buffer && linear[0] && linear[1] && linear[2])
	{
		const struct
		{
			VkFormat fmt;
			uint32_t width;
			ImageHandle *out;
		} reqs[] = {
			{ VK_FORMAT_R8_UINT, 4096, &payload_r8_image },
			{ VK_FORMAT_R16_UINT, 2048, &payload_r16_image },
			{ VK_FORMAT_R32_UINT, 1024, &payload_r32_image }
		};

		for (int i = 0; i < 3; i++)
		{
			auto image_info = ImageCreateInfo::immutable_2d_image(reqs[i].width, rows, reqs[i].fmt);
			image_info.domain = ImageDomain::LinearHost;
			image_info.usage = VK_IMAGE_USAGE_SAMPLED_BIT;
			image_info.layout = ImageLayout::General;
			*reqs[i].out = device->wrap_image(image_info, (VkImage)(uintptr_t)linear[i]);
			if (!*reqs[i].out)
				break;

			ImageViewCreateInfo view_info = {};
			view_info.image = reqs[i].out->get();
			view_info.format = reqs[i].fmt;
			view_info.view_type = VK_IMAGE_VIEW_TYPE_2D;
			view_info.layers = 1;
			view_info.levels = 1;
			view_info.aspect = VK_IMAGE_ASPECT_COLOR_BIT;
			linear_views[i] = device->create_image_view(view_info);
			if (!linear_views[i])
				break;
		}

		if (!linear_views[0] || !linear_views[1] || !linear_views[2])
		{
			payload_r8_image.reset();
			payload_r16_image.reset();
			payload_r32_image.reset();
			for (auto &v : linear_views)
				v.reset();
		}
	}

	if (use_readonly_texel_buffer && !linear_views[0])
	{
		BufferViewCreateInfo view_info = {};
		view_info.buffer = payload_data.get();
		view_info.range = VK_WHOLE_SIZE;
		view_info.format = VK_FORMAT_R8_UINT;
		payload_u8_view = device->create_buffer_view(view_info);
		view_info.format = VK_FORMAT_R16_UINT;
		payload_u16_view = device->create_buffer_view(view_info);
		view_info.format = VK_FORMAT_R32_UINT;
		payload_u32_view = device->create_buffer_view(view_info);
	}

	in_place_base = static_cast<uint8_t *>(host);
	in_place_size = size;
	in_place_reserve = copy_reserve;
	return true;
}

bool Decoder::init(Vulkan::Device *device, int width, int height, ChromaSubsampling chroma_, bool fragment_path_)
{
	auto ops = device->get_device_features().vk11_props.subgroupSupportedOperations;
	constexpr VkSubgroupFeatureFlags required_features =
			VK_SUBGROUP_FEATURE_VOTE_BIT |
			VK_SUBGROUP_FEATURE_BALLOT_BIT |
			VK_SUBGROUP_FEATURE_ARITHMETIC_BIT |
			VK_SUBGROUP_FEATURE_SHUFFLE_BIT |
			VK_SUBGROUP_FEATURE_SHUFFLE_RELATIVE_BIT |
			VK_SUBGROUP_FEATURE_BASIC_BIT;

	if ((ops & required_features) != required_features)
	{
		LOGE("There are missing subgroup features. Device supports #%x, but requires #%x.\n",
		     ops, required_features);
		return false;
	}

	// The decoder is more lenient.
	if (!device->supports_subgroup_size_log2(true, 2, 7))
	{
		LOGE("Device doesn't support basic subgroup size control.\n");
		return false;
	}

	if (!impl->init(device, width, height, chroma_, fragment_path_))
	{
		LOGE("Failed to initialize.\n");
		return false;
	}

	if (!device->get_device_features().vk12_features.storageBuffer8BitAccess &&
	    !impl->use_readonly_texel_buffer)
	{
		LOGE("Device doesn't support 8-bit storage or large texel buffers.\n");
		return false;
	}

	impl->check_linear_texture_support();

	clear();
	return true;
}

void Decoder::clear()
{
	impl->clear();
}

bool Decoder::push_packet(const void *data, size_t size)
{
	return impl->push_packet(data, size);
}

bool Decoder::set_payload_memory(uint64_t buffer, uint64_t size, uint64_t copy_reserve, void *host,
                                 const uint64_t linear[3], uint32_t rows)
{
	return impl->set_payload_memory(buffer, size, copy_reserve, host, linear, rows);
}

bool Decoder::push_packet_in_place(const void *data, size_t size)
{
	auto *p = static_cast<const uint8_t *>(data);
	auto *base = impl->in_place_base;
	if (!base || p < base + impl->in_place_reserve || size > impl->in_place_size ||
	    VkDeviceSize(p - base) > impl->in_place_size - size || ((p - base) & 3) != 0)
	{
		LOGE("In-place packet lies outside the payload memory.\n");
		return false;
	}
	impl->pushing_in_place = true;
	bool ret = impl->push_packet(data, size);
	impl->pushing_in_place = false;
	return ret;
}

bool Decoder::decode(Vulkan::CommandBuffer &cmd, const ViewBuffers &views)
{
	return impl->decode(cmd, views);
}

uint32_t Decoder::cpu_dequant_us() const
{
	return impl->cpu_dq_us;
}

uint32_t Decoder::early_windows(uint32_t tag, const void *windows, size_t count, size_t window_bytes)
{
	return impl->early_windows(tag, static_cast<const uint8_t *>(windows), count, window_bytes);
}

uint32_t Decoder::early_claim(uint32_t tag)
{
	return impl->early_claim(tag);
}

int Decoder::early_verify_result() const
{
	return impl->early_verify_bad;
}

bool Decoder::decode_is_ready(bool allow_partial_frame) const
{
	return impl->decode_is_ready(allow_partial_frame);
}

bool Decoder::decode_is_ready(bool allow_partial_frame, int num_pristine_bands, float minimum_packet_ratio,
                              const uint32_t *active_block_mask, size_t word_count) const
{
	return impl->decode_is_ready(allow_partial_frame, num_pristine_bands, minimum_packet_ratio, active_block_mask, word_count);
}
}
