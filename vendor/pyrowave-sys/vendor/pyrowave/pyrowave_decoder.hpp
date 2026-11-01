// Copyright (c) 2025 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT
#pragma once

#include <memory>
#include <stddef.h>
#include <stdint.h>
#include "pyrowave_config.hpp"

namespace Vulkan
{
class Device;
class ImageView;
class CommandBuffer;
}

namespace PyroWave
{
class Decoder
{
public:
	Decoder();
	~Decoder();

	// Fragment path is optimized for typical mobile GPUs which have weak compute support.
	// iDWT is instead computed entirely in traditional render passes and fragment shaders.
	// This path is *not* recommended for desktop-class chips.
	bool init(Vulkan::Device *device, int width, int height,
	          ChromaSubsampling chroma, bool fragment_path = false);

	static bool device_prefers_fragment_path(Vulkan::Device &device);

	void clear();
	bool push_packet(const void *data, size_t size);

	// PUNKTFUNK (patch 0011): payload memory the caller owns and writes, so a decode copies
	// nothing. `buffer` is `size` bytes of host-coherent memory mapped at `host`; `linear`
	// are R8/R16/R32_UINT linear images of 4096-byte rows aliasing it from offset 0, all three
	// or none. push_packet keeps copying, into the first `copy_reserve` bytes;
	// push_packet_in_place takes packets that already lie past them, 4-byte aligned. Every
	// handle must outlive the decoder. Handles travel as their 64-bit values.
	bool set_payload_memory(uint64_t buffer, uint64_t size, uint64_t copy_reserve, void *host,
	                        const uint64_t linear[3], uint32_t rows);
	bool push_packet_in_place(const void *data, size_t size);

	// If fragment path is enabled, the command buffer must support graphics operations.
	// To synchronize, synchronize with COLOR_OUTPUT / COLOR_ATTACHMENT_WRITE / COLOR_ATTACHMENT_OPTIMAL.
	// Views must be created with VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT.
	bool decode(Vulkan::CommandBuffer &cmd, const ViewBuffers &views);

	bool decode_is_ready(bool allow_partial_frame) const;

	// A more refined version of decode_is_ready() that allows a bit more control.
	// The default is num_pristine_bands = 2 (the next-to-final LL band) and minimum_packet_ratio = 0.9.
	// active_block_mask is an optional pointer.
	// If bit i % 32 of active_block_mask[i / 32] is not set, then a missing block for that block index
	// is ignored. This is intended to cover advanced cases for error correction.
	// If the pointer is null, it is implied that all blocks are active for purposes of this call.
	bool decode_is_ready(bool allow_partial_frame, int num_pristine_bands, float minimum_packet_ratio,
	                     const uint32_t *active_block_mask, size_t word_count) const;

private:
	struct Impl;
	std::unique_ptr<Impl> impl;
};
}