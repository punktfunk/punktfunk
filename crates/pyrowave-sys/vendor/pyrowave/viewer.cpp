// Copyright (c) 2025 Hans-Kristian Arntzen
// SPDX-License-Identifier: MIT

#include "application.hpp"
#include "command_buffer.hpp"
#include "device.hpp"
#include "os_filesystem.hpp"
#include "muglm/muglm_impl.hpp"
#include "pyrowave_encoder.hpp"
#include "pyrowave_decoder.hpp"
#include "yuv4mpeg.hpp"
#include "pyrowave_common.hpp"
#include "flat_renderer.hpp"
#include "ui_manager.hpp"
#include <string.h>
#include <stdexcept>
#include <random>

using namespace Granite;
using namespace Vulkan;

struct YCbCrImages
{
	Vulkan::ImageHandle images[3];
	PyroWave::ViewBuffers views;
};

static bool device_should_use_fragment_path(Device &device)
{
	return PyroWave::Decoder::device_prefers_fragment_path(device);
}

static YCbCrImages create_ycbcr_images(Device &device, int width, int height, VkFormat fmt, PyroWave::ChromaSubsampling chroma)
{
	YCbCrImages images;
	auto info = ImageCreateInfo::immutable_2d_image(width, height, fmt);
	info.usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_TRANSFER_DST_BIT |
	             VK_IMAGE_USAGE_SAMPLED_BIT;
	info.initial_layout = VK_IMAGE_LAYOUT_UNDEFINED;

	if (device_should_use_fragment_path(device))
		info.usage |= VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;
	else
		info.usage |= VK_IMAGE_USAGE_STORAGE_BIT;

	images.images[0] = device.create_image(info);
	device.set_name(*images.images[0], "Y");

	if (chroma == PyroWave::ChromaSubsampling::Chroma420)
	{
		info.width >>= 1;
		info.height >>= 1;
	}

	images.images[1] = device.create_image(info);
	device.set_name(*images.images[1], "Cb");

	images.images[2] = device.create_image(info);
	device.set_name(*images.images[2], "Cr");

	for (int i = 0; i < 3; i++)
		images.views.planes[i] = &images.images[i]->get_view();

	return images;
}

struct ViewerApplication : Granite::Application, Granite::EventHandler
{
	explicit ViewerApplication(const char *path_)
		: path(path_)
	{
		if (!file.open_read(path))
		{
			rawfile.reset(fopen(path, "rb"));
			if (!rawfile)
				throw std::runtime_error("Failed to open.");
			char magic[9] = {};
			if (fread(magic, 1, 8, rawfile.get()) != 8 || strcmp(magic, "PYROWAVE") != 0)
				throw std::runtime_error("Failed to open.");
			if (fread(&rawparam, sizeof(rawparam), 1, rawfile.get()) != 1)
				throw std::runtime_error("Failed to open.");
		}

		get_wsi().set_backbuffer_format(BackbufferFormat::UNORM);
		EVENT_MANAGER_REGISTER_LATCH(ViewerApplication, on_device_created, on_device_destroyed, DeviceCreatedEvent);
		EVENT_MANAGER_REGISTER(ViewerApplication, on_key_press, KeyboardEvent);
		EVENT_MANAGER_REGISTER(ViewerApplication, on_mouse, MouseMoveEvent);
		EVENT_MANAGER_REGISTER(ViewerApplication, on_mouse_event, MouseButtonEvent);

		x_slide = file.get_width() / 2;
	}

	bool is_mouse_active = false;
	bool paused = false;
	bool first_decode = true;

	enum class Mode
	{
		Slide,
		Flicker,
		Delta
	};
	Mode mode = Mode::Slide;
	bool lossy_simulation = false;
	int lossy_rate = 1;
	struct
	{
		uint64_t decode_success_with_mask;
		uint64_t decode_success_no_mask;
		uint64_t decode_success_perfect;
		uint64_t decode_failure;
	} simulated = {};
	std::mt19937 rnd;
	std::uniform_int_distribution<int> dist{0, 1000};

	bool on_mouse(const MouseMoveEvent &e)
	{
		if (is_mouse_active)
			x_slide = int(e.get_abs_x());
		return true;
	}

	bool on_mouse_event(const MouseButtonEvent &e)
	{
		is_mouse_active = e.get_pressed();
		return true;
	}

	bool on_key_press(const KeyboardEvent &e)
	{
		if (e.get_key_state() != KeyState::Released)
		{
			if (e.get_key() == Key::Up)
				bit_rate_mbit += 10;
			else if (e.get_key() == Key::Down && bit_rate_mbit > 20)
				bit_rate_mbit -= 10;
			else if (e.get_key() == Key::F)
				mode = Mode::Flicker;
			else if (e.get_key() == Key::D)
				mode = Mode::Delta;
			else if (e.get_key() == Key::S)
				mode = Mode::Slide;
			else if (e.get_key() == Key::L)
				lossy_simulation = !lossy_simulation;
			else if (e.get_key() == Key::Right)
			{
				lossy_rate = std::min<int>(lossy_rate + 1, 100);
				simulated = {};
			}
			else if (e.get_key() == Key::Left)
			{
				lossy_rate = std::max<int>(lossy_rate - 1, 1);
				simulated = {};
			}
			else if (e.get_key() == Key::P)
			{
				get_wsi().set_backbuffer_format(
						get_wsi().get_backbuffer_format() == BackbufferFormat::HDR10 ?
						BackbufferFormat::UNORM : BackbufferFormat::HDR10);
			}
		}

		if (e.get_key_state() == KeyState::Pressed && e.get_key() == Key::Space)
			paused = !paused;

		return true;
	}

	void on_device_created(const DeviceCreatedEvent &e)
	{
		if (rawfile)
		{
			auto format = YUV4MPEGFile::format_to_bytes_per_component(rawparam.format) == 2 ?
					VK_FORMAT_R16_UNORM : VK_FORMAT_R8_UNORM;
			auto chroma = YUV4MPEGFile::format_has_subsampling(rawparam.format) ?
			              PyroWave::ChromaSubsampling::Chroma420 :
			              PyroWave::ChromaSubsampling::Chroma444;

			out_images = create_ycbcr_images(e.get_device(), rawparam.width, rawparam.height, format, chroma);
			dec.init(&e.get_device(), rawparam.width, rawparam.height, chroma, device_should_use_fragment_path(e.get_device()));
		}
		else
		{
			auto format = YUV4MPEGFile::format_to_bytes_per_component(file.get_format()) == 2 ? VK_FORMAT_R16_UNORM
			                                                                                  : VK_FORMAT_R8_UNORM;
			auto chroma = YUV4MPEGFile::format_has_subsampling(file.get_format())
			              ? PyroWave::ChromaSubsampling::Chroma420 : PyroWave::ChromaSubsampling::Chroma444;
			in_images = create_ycbcr_images(e.get_device(), file.get_width(), file.get_height(), format, chroma);
			out_images = create_ycbcr_images(e.get_device(), file.get_width(), file.get_height(), format, chroma);
			enc.init(&e.get_device(), file.get_width(), file.get_height(), chroma);
			dec.init(&e.get_device(), file.get_width(), file.get_height(), chroma, device_should_use_fragment_path(e.get_device()));
		}

		first_decode = true;
	}

	void on_device_destroyed(const DeviceCreatedEvent &)
	{
		in_images = {};
		out_images = {};
	}

	std::vector<uint8_t> packetized_data;

	bool read_raw_payload()
	{
		uint32_t u32_size;

		for (;;)
		{
			if (fread(&u32_size, sizeof(u32_size), 1, rawfile.get()) != 1)
				return false;
			packetized_data.resize(u32_size);
			if (fread(packetized_data.data(), 1, u32_size, rawfile.get()) != u32_size)
				return false;

			if (!dec.push_packet(packetized_data.data(), packetized_data.size()))
				return false;

			if (dec.decode_is_ready(false))
				return true;
		}
	}

	void render_frame(double, double elapsed_time) override
	{
		auto &device = get_wsi().get_device();
		auto cmd = device.request_command_buffer();

		if (!paused)
		{
			if (rawfile)
			{
				if (!read_raw_payload())
				{
					fseek(rawfile.get(), strlen("PYROWAVE") + sizeof(rawparam), SEEK_SET);
					dec.clear();
					if (!read_raw_payload())
					{
						request_shutdown();
						return;
					}
				}
			}
			else if (!file.begin_frame())
			{
				file = {};
				if (!file.open_read(path) || !file.begin_frame())
				{
					request_shutdown();
					return;
				}
			}

			if (!rawfile)
			{
				for (int i = 0; i < 3; i++)
				{
					cmd->image_barrier(*in_images.images[i], VK_IMAGE_LAYOUT_UNDEFINED,
					                   VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
					                   0, 0,
					                   VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT);
				}

				for (int i = 0; i < 3; i++)
				{
					auto *y = cmd->update_image(*in_images.images[i]);
					if (!file.read(y, in_images.images[i]->get_width() * in_images.images[i]->get_height() *
					                  YUV4MPEGFile::format_to_bytes_per_component(file.get_format())))
					{
						LOGE("Failed to read plane.\n");
						device.submit_discard(cmd);
						request_shutdown();
						return;
					}
				}

				for (int i = 0; i < 3; i++)
				{
					if (device_should_use_fragment_path(device))
					{
						cmd->image_barrier(*in_images.images[i],
										   VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL,
						                   VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT,
						                   VK_PIPELINE_STAGE_2_FRAGMENT_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
					}
					else
					{
						cmd->image_barrier(*in_images.images[i],
						                   VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL,
						                   VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_2_TRANSFER_WRITE_BIT,
						                   VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
					}
				}
			}
		}

		unsigned bitstream_size = bit_rate_mbit * 1000000ull / (60 * 8);

		std::vector<uint32_t> active_mask_buffer;
		constexpr int PristineBands = 3;
		constexpr float MinimumPacketRatio = 0.9f;

		if (!rawfile)
		{
			bitstream_size &= ~3u;

			BufferCreateInfo buffer_info = {};
			buffer_info.usage = VK_BUFFER_USAGE_STORAGE_BUFFER_BIT | VK_BUFFER_USAGE_TRANSFER_SRC_BIT;

			buffer_info.size = enc.get_meta_required_size();
			buffer_info.domain = BufferDomain::Device;
			auto meta = device.create_buffer(buffer_info);
			buffer_info.domain = BufferDomain::CachedHost;
			auto meta_host = device.create_buffer(buffer_info);

			buffer_info.size = bitstream_size + 2 * enc.get_meta_required_size();
			buffer_info.domain = BufferDomain::Device;
			auto bitstream = device.create_buffer(buffer_info);
			buffer_info.domain = BufferDomain::CachedHost;
			auto bitstream_host = device.create_buffer(buffer_info);

			PyroWave::Encoder::BitstreamBuffers buffers = {};
			buffers.meta.buffer = meta.get();
			buffers.meta.size = meta->get_create_info().size;
			buffers.bitstream.buffer = bitstream.get();
			buffers.bitstream.size = bitstream->get_create_info().size;
			buffers.target_size = bitstream_size;

			enc.encode(*cmd, in_images.views, buffers);
			cmd->copy_buffer(*bitstream_host, *bitstream);
			cmd->copy_buffer(*meta_host, *meta);
			cmd->barrier(VK_PIPELINE_STAGE_2_COPY_BIT, VK_ACCESS_TRANSFER_WRITE_BIT,
			             VK_PIPELINE_STAGE_HOST_BIT, VK_ACCESS_HOST_READ_BIT);

			Fence fence;
			device.submit(cmd, &fence);
			fence->wait();

			auto *mapped_meta = static_cast<const PyroWave::BitstreamPacket *>(
					device.map_host_buffer(*meta_host, MEMORY_ACCESS_READ_BIT));
			auto *mapped_bits = static_cast<const uint32_t *>(
					device.map_host_buffer(*bitstream_host, MEMORY_ACCESS_READ_BIT));

			std::vector<uint8_t> reordered_packet_buffer(bitstream_size * 2);
			size_t num_packets = enc.compute_num_packets(mapped_meta, 8 * 1024);
			std::vector<PyroWave::Encoder::Packet> packets(num_packets);
			size_t out_packets = enc.packetize(packets.data(), 8 * 1024,
			                                   reordered_packet_buffer.data(),
			                                   reordered_packet_buffer.size(),
			                                   mapped_meta, mapped_bits);
			(void) out_packets;

			size_t encoded_size = 0;
			for (auto &p: packets)
				encoded_size += p.size;

			LOGI("Total encoded size: %zu\n", encoded_size);

			if (encoded_size > bitstream_size)
			{
				LOGE("Broken rate control\n");
				return;
			}

			for (int bands = 1; bands < 5; bands++)
			{
				size_t num_active_blocks = enc.get_num_active_blocks(bands);
				active_mask_buffer.resize((num_active_blocks + 31) / 32);
				enc.compute_block_active_words(bands, active_mask_buffer.data(), active_mask_buffer.size(),
				                               mapped_meta);

				uint32_t active_blocks = 0;
				for (auto &word : active_mask_buffer)
					active_blocks += Util::popcount32(word);

				LOGI("Bands %u -> residency ratio (%u / %zu) %.3f %%, critical packets %zu\n", bands, active_blocks,
				     num_active_blocks, 100.0f * float(active_blocks) / float(num_active_blocks),
				     enc.compute_num_critical_packets(bands, mapped_meta, 8 * 1024));
			}

			assert(out_packets == num_packets);

			std::vector<bool> lossy_successes(packets.size());

			for (auto &p: packets)
			{
				if (lossy_simulation)
					if (lossy_rate > dist(rnd))
						continue;

				lossy_successes[&p - packets.data()] = true;

				if (!dec.push_packet(reordered_packet_buffer.data() + p.offset, p.size))
					return;
			}

			size_t num_active_blocks = enc.get_num_active_blocks(PristineBands);
			active_mask_buffer.resize((num_active_blocks + 31) / 32);
			enc.compute_block_active_words(PristineBands, active_mask_buffer.data(), active_mask_buffer.size(),
										   mapped_meta);

			if (lossy_simulation)
			{
				size_t num_critical_packets = enc.compute_num_critical_packets(PristineBands, mapped_meta, 8 * 1024);

				// Simulate a simple XOR FEC model. If all but one packet is received, we can recover.
				size_t lossy_critical_successes = 0;
				for (size_t i = 0; i < num_critical_packets; i++)
					if (lossy_successes[i])
						lossy_critical_successes++;

				// If we're missing one critical packet, and we successfully got the XOR packet, we can reconstruct
				// the missing packet.
				if (lossy_critical_successes == num_critical_packets - 1 && lossy_rate <= dist(rnd))
				{
					LOGI("Simulating FEC success.\n");
					// Push all critical packets again.
					for (size_t i = 0; i < num_critical_packets; i++)
						dec.push_packet(reordered_packet_buffer.data() + packets[i].offset, packets[i].size);
				}
			}

			cmd = device.request_command_buffer();
		}

		for (int i = 0; i < 3; i++)
		{
			if (device_should_use_fragment_path(device))
			{
				cmd->image_barrier(*out_images.images[i],
				                   first_decode ? VK_IMAGE_LAYOUT_UNDEFINED : VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL,
				                   VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
				                   VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0,
				                   VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_ACCESS_2_COLOR_ATTACHMENT_WRITE_BIT);
			}
			else
			{
				cmd->image_barrier(*out_images.images[i],
				                   first_decode ? VK_IMAGE_LAYOUT_UNDEFINED : VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL, VK_IMAGE_LAYOUT_GENERAL,
				                   VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, 0,
				                   VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT);
			}
		}

		first_decode = false;

		if (dec.decode_is_ready(true, PristineBands, MinimumPacketRatio,
		                        active_mask_buffer.data(), active_mask_buffer.size()))
		{
			if (dec.decode_is_ready(true, PristineBands, MinimumPacketRatio, nullptr, 0))
				simulated.decode_success_no_mask++;
			if (dec.decode_is_ready(false))
				simulated.decode_success_perfect++;
			dec.decode(*cmd, out_images.views);
			simulated.decode_success_with_mask++;
		}
		else
		{
			LOGI("Dropped frame due to simulated packet loss.\n");
			simulated.decode_failure++;
		}

		for (int i = 0; i < 3; i++)
		{
			if (device_should_use_fragment_path(device))
			{
				cmd->image_barrier(*out_images.images[i],
				                   VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL,
				                   VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
				                   VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
			}
			else
			{
				cmd->image_barrier(*out_images.images[i],
				                   VK_IMAGE_LAYOUT_GENERAL, VK_IMAGE_LAYOUT_READ_ONLY_OPTIMAL,
				                   VK_PIPELINE_STAGE_2_COMPUTE_SHADER_BIT, VK_ACCESS_2_SHADER_STORAGE_WRITE_BIT,
				                   VK_PIPELINE_STAGE_FRAGMENT_SHADER_BIT, VK_ACCESS_2_SHADER_SAMPLED_READ_BIT);
			}
		}

		cmd->begin_render_pass(device.get_swapchain_render_pass(SwapchainRenderPass::ColorOnly));
		cmd->set_sampler(0, 3, StockSampler::LinearClamp);

		auto fmt = rawfile ? rawparam.format : file.get_format();

		cmd->set_specialization_constant_mask(3);
		cmd->set_specialization_constant(0, fmt == YUV4MPEGFile::Format::YUV420P16 || fmt == YUV4MPEGFile::Format::YUV444P16);
		cmd->set_specialization_constant(1, rawfile ? bool(rawparam.is_full_range) : file.is_full_range());

		CommandBufferUtil::setup_fullscreen_quad(*cmd, "builtin://shaders/quad.vert", "assets://yuv2rgb.frag",
		                                         {{ "DELTA", mode == Mode::Delta ? 1 : 0 }});

		x_slide = clamp(x_slide, 50, int(cmd->get_viewport().width) - 50);

		const float full_color = get_wsi().get_backbuffer_format() == BackbufferFormat::HDR10 ? 0.75f : 1.0f;

		double simulated_success_rate =
			simulated.decode_failure ?
			double(simulated.decode_success_with_mask) / double(simulated.decode_success_with_mask + simulated.decode_failure) : 1.0;

		double simulated_success_rate_no_mask =
			simulated.decode_failure ?
			double(simulated.decode_success_no_mask) / double(simulated.decode_success_with_mask + simulated.decode_failure) : 1.0;

		double simulated_success_rate_perfect =
			simulated.decode_failure ?
			double(simulated.decode_success_perfect) / double(simulated.decode_success_with_mask + simulated.decode_failure) : 1.0;

		if (mode == Mode::Flicker && !rawfile)
		{
			if (muglm::fract(elapsed_time * 10.0) < 0.5)
			{
				cmd->set_texture(0, 0, *in_images.views.planes[0]);
				cmd->set_texture(0, 1, *in_images.views.planes[1]);
				cmd->set_texture(0, 2, *in_images.views.planes[2]);
			}
			else
			{
				cmd->set_texture(0, 0, *out_images.views.planes[0]);
				cmd->set_texture(0, 1, *out_images.views.planes[1]);
				cmd->set_texture(0, 2, *out_images.views.planes[2]);
			}

			cmd->draw(3);
			flat_renderer.begin();
			char text[64];
			snprintf(text, sizeof(text), "FLICKER %u mbits | %.3f bpp @ 60 fps%s",
			         bit_rate_mbit,
					 double(bitstream_size * 8) / double(file.get_width() * file.get_height()),
			         paused ? " (paused)" : "");
			flat_renderer.render_text(GRANITE_UI_MANAGER()->get_font(UI::FontSize::Large),
			                          text, vec3(20, 20, 0), vec2(400, 200), vec4(full_color, full_color, 0.0f, 1.0f),
			                          Font::Alignment::TopLeft);
			flat_renderer.flush(*cmd, vec3(0), vec3(cmd->get_viewport().width, cmd->get_viewport().height, 1));
		}
		else if (mode == Mode::Slide || rawfile)
		{
			if (!rawfile)
			{
				cmd->set_texture(0, 0, *in_images.views.planes[0]);
				cmd->set_texture(0, 1, *in_images.views.planes[1]);
				cmd->set_texture(0, 2, *in_images.views.planes[2]);
				cmd->set_scissor({{ 0, 0 },
				                  { uint32_t(x_slide), uint32_t(cmd->get_viewport().height) }});
				cmd->draw(3);
			}

			cmd->set_texture(0, 0, *out_images.views.planes[0]);
			cmd->set_texture(0, 1, *out_images.views.planes[1]);
			cmd->set_texture(0, 2, *out_images.views.planes[2]);
			if (!rawfile)
			{
				cmd->set_scissor({{ int32_t(x_slide), 0 },
				                  { uint32_t(cmd->get_viewport().width), uint32_t(cmd->get_viewport().height) }});
			}
			cmd->draw(3);

			cmd->set_scissor({{ 0, 0 },
			                  { uint32_t(cmd->get_viewport().width), uint32_t(cmd->get_viewport().height) }});

			if (!rawfile)
			{
				flat_renderer.begin();
				char text[256];
				snprintf(text, sizeof(text), "%u mbits | %.3f bpp @ 60 fps%s | Loss rate %.1f %% | Success rate (with mask = %.3f %%, no mask = %.3f %%, perfect = %.3f %%)",
				         bit_rate_mbit,
				         double(bitstream_size * 8) / double(file.get_width() * file.get_height()),
				         paused ? " (paused)" : "", lossy_simulation ? lossy_rate * 0.1 : 0,
				         simulated_success_rate * 100.0, simulated_success_rate_no_mask * 100.0,
				         simulated_success_rate_perfect * 100.0);

				flat_renderer.render_text(GRANITE_UI_MANAGER()->get_font(UI::FontSize::Large),
				                          text, vec3(20, 20, 0), vec2(400, 200),
				                          vec4(full_color, full_color, 0.0f, 1.0f),
				                          Font::Alignment::TopLeft);
				flat_renderer.render_text(GRANITE_UI_MANAGER()->get_font(UI::FontSize::Large),
				                          text, vec3(18, 22, 0.5f), vec2(400, 200), vec4(0.0f, 0.0f, 0.0f, 1.0f),
				                          Font::Alignment::TopLeft);
				flat_renderer.render_quad(vec3(float(x_slide), 0.0f, 0.8f),
				                          vec2(2.0f, cmd->get_viewport().height),
				                          vec4(full_color, full_color, 0.0f, 1.0f));
				flat_renderer.flush(*cmd, vec3(0), vec3(cmd->get_viewport().width, cmd->get_viewport().height, 1));
			}
		}
		else
		{
			cmd->set_texture(0, 0, *in_images.views.planes[0]);
			cmd->set_texture(0, 1, *out_images.views.planes[0]);
			cmd->draw(3);

			flat_renderer.begin();
			char text[256];

			snprintf(text, sizeof(text),
			         "DELTA %u mbits | %.3f bpp @ 60 fps%s | Loss rate %.1f %% | Success rate (with mask = %.3f %%, no mask = %.3f %%, perfect = %.3f %%)",
			         bit_rate_mbit,
			         double(bitstream_size * 8) / double(file.get_width() * file.get_height()),
			         paused ? " (paused)" : "", lossy_simulation ? lossy_rate * 0.1 : 0.0,
			         simulated_success_rate * 100.0, simulated_success_rate_no_mask * 100.0,
			         simulated_success_rate_perfect * 100.0);

			flat_renderer.render_text(GRANITE_UI_MANAGER()->get_font(UI::FontSize::Large),
									  text, vec3(20, 20, 0), vec2(400, 200),
									  vec4(full_color, full_color, 0.0f, 1.0f),
									  Font::Alignment::TopLeft);
			flat_renderer.flush(*cmd, vec3(0), vec3(cmd->get_viewport().width, cmd->get_viewport().height, 1));
		}

		cmd->end_render_pass();

		device.submit(cmd);
	}

	unsigned get_default_width() override
	{
		return rawfile ? rawparam.width : file.get_width();
	}

	unsigned get_default_height() override
	{
		return rawfile ? rawparam.height : file.get_height();
	}

	PyroWave::Encoder enc;
	PyroWave::Decoder dec;
	YCbCrImages in_images;
	YCbCrImages out_images;
	YUV4MPEGFile file;
	const char *path;
	unsigned bit_rate_mbit = 200;
	FlatRenderer flat_renderer;

	struct RawParameters
	{
		int32_t width;
		int32_t height;
		YUV4MPEGFile::Format format;
		PyroWave::ChromaSubsampling chroma;
		uint32_t is_full_range;
		int32_t frame_rate_num;
		int32_t frame_rate_den;
		uint32_t chroma_siting;
	};
	static_assert(sizeof(RawParameters) == 32, "RawParameters size mismatch.");
	struct FILEDeleter { void operator()(FILE *f) { if (f) fclose(f); } };

	std::unique_ptr<FILE, FILEDeleter> rawfile;
	RawParameters rawparam = {};

	int x_slide = 100;
};

namespace Granite
{
Application *application_create(int argc, char **argv)
{
	GRANITE_APPLICATION_SETUP_FILESYSTEM();

#ifndef __ANDROID__
	if (argc != 2)
	{
		LOGE("Usage: pyrowave-viewer test.y4m\n");
		return nullptr;
	}
#endif

	const char *path = nullptr;
	if (argc >= 2)
		path = argv[1];

#ifdef __ANDROID__
	if (!path)
		path = "/data/local/tmp/test.wave";
#endif

	try
	{
		auto *app = new ViewerApplication(path);
		return app;
	}
	catch (const std::exception &e)
	{
		LOGE("application_create() threw exception: %s\n", e.what());
		return nullptr;
	}
}
}
