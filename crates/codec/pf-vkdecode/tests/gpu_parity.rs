//! Vulkan Video pixel-parity for H.264, H.265, and AV1.
//!
//! Ignored GPU legs decode vendored and host-produced streams, read back each
//! display-region frame, and require SHA-256 plus display-order frame count
//! (including flush) to match the checked-in libavcodec goldens. H.264/H.265
//! also prove three- and four-byte Annex-B start codes produce identical pixels;
//! AV1 OBUs are length-delimited, so there is no prefix-width leg. The fixtures and
//! their CPU guards are `pf_bitstream::testing::parity`'s; the guards here cover this
//! file's four-byte rewrite and frame-0 blob. Files do not cover packetisation or loss.
//!
//! Needs a Vulkan Video device for the codec/profile under test. RADV also needs
//! `RADV_PERFTEST=video_decode`. Select a GPU with `PF_VKD_SMOKE_VENDOR=0x1002`
//! or `0x10de`.
//!
//! ```text
//! cargo test -p pf-vkdecode --test gpu_parity -- --ignored --nocapture
//! ```
//!
//! Inputs live under `tests/data`. The legs assert; they write no evidence files.

mod common;

use ash::vk;
use pf_bitstream::testing::parity;
use pf_bitstream::testing::parity::Fixture;
use pf_bitstream::testing::parity::Layout;
use pf_vkdecode::DecodeStatus;
use pf_vkdecode::DecodedVkFrame;
use pf_vkdecode::DeviceHandles;
use pf_vkdecode::NoopQueueLock;
use pf_vkdecode::VkAv1Decoder;
use pf_vkdecode::VkCodec;
use pf_vkdecode::VkDecoder;
use pf_vkdecode::VkH264Decoder;
use pf_vkdecode::VkH265Decoder;
use sha2::Digest;

/// The pool format a fixture's goldens hash. [`DecodedVkFrame::format`] fails a pool of
/// the other depth instead of hashing a different layout.
fn vk_format(layout: Layout) -> vk::Format {
    match layout {
        Layout::Nv12 => pf_vkdecode::NV12,
        Layout::P010 => pf_vkdecode::P010,
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    sha2::Sha256::digest(data)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Set `PF_VKD_TEST_READBACK` so pool images grow TRANSFER_SRC for the copy.
///
/// The GPU lock is taken by reference: `env::set_var` is unsound from a live
/// multithreaded process, so a borrow states "the caller holds the lock" in the
/// type. This is the file's only `set_var`.
fn arm_test_readback(_gpu: &std::sync::MutexGuard<'static, ()>) {
    // SAFETY: `_gpu` is the binary-wide GPU lock (`common::gpu_lock`). The parity
    // legs are this variable's only writers and readers, and they run one at a time.
    unsafe { std::env::set_var("PF_VKD_TEST_READBACK", "1") };
}

/// GPU→CPU readback: one mapped staging buffer and one command buffer on the
/// graphics queue. Each read waits the frame's timeline `value`, copies, restores
/// layout, signals `value + 1` in the same submit, then host-waits the fence.
///
/// Display size is a constructor argument: it sizes the staging buffer and is the
/// crop every read asserts.
struct Readback {
    device: ash::Device,
    queue: vk::Queue,
    cmd_pool: vk::CommandPool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    mapped: *const u8,
    display: (u32, u32),
    /// Pool format. Held here so buffer sizing and the per-frame assert share one
    /// source — an 8-bit buffer that accepted a 10-bit frame would hash half a picture.
    format: vk::Format,
    /// 1 for NV12, 2 for the `3PACK16` ten-bit family (10 bits in the high end of
    /// each 16-bit word — P010's layout).
    bytes_per_sample: u32,
    /// Tightly packed size: `w * h * 3 / 2 * bytes_per_sample`.
    frame_bytes: usize,
}

impl Readback {
    /// # Safety
    ///
    /// `instance`/`pd`/`device` are live; `graphics_qf` names a queue family a
    /// queue was created on (index 0) whose family supports TRANSFER (GRAPHICS
    /// implies it).
    unsafe fn new(
        instance: &ash::Instance,
        pd: vk::PhysicalDevice,
        device: &ash::Device,
        graphics_qf: u32,
        display: (u32, u32),
        format: vk::Format,
    ) -> Self {
        let (width, height) = display;
        // The two-plane copy halves both dimensions for chroma; an odd region
        // would silently drop a chroma row/column.
        assert_eq!(
            (width % 2, height % 2),
            (0, 0),
            "the display region must be chroma-aligned"
        );
        let bytes_per_sample = match format {
            f if f == pf_vkdecode::NV12 => 1,
            f if f == pf_vkdecode::P010 => 2,
            other => panic!("readback has no sample size for {other:?}"),
        };
        let frame_bytes = (width * height * 3 / 2 * bytes_per_sample) as usize;

        // SAFETY: fn contract — live device, queue 0 of this family exists.
        let queue = unsafe { device.get_device_queue(graphics_qf, 0) };
        let pool_ci = vk::CommandPoolCreateInfo::default()
            .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER)
            .queue_family_index(graphics_qf);
        // SAFETY: live device; destroyed in `destroy`.
        let cmd_pool = unsafe { device.create_command_pool(&pool_ci, None) }
            .expect("create the readback command pool");
        let alloc_ci = vk::CommandBufferAllocateInfo::default()
            .command_pool(cmd_pool)
            .level(vk::CommandBufferLevel::PRIMARY)
            .command_buffer_count(1);
        // SAFETY: the pool was just created on this device.
        let cmd = unsafe { device.allocate_command_buffers(&alloc_ci) }
            .expect("allocate the readback command buffer")[0];
        // SAFETY: live device; destroyed in `destroy`.
        let fence = unsafe { device.create_fence(&vk::FenceCreateInfo::default(), None) }
            .expect("create the readback fence");

        let buffer_ci = vk::BufferCreateInfo::default()
            .size(frame_bytes as u64)
            .usage(vk::BufferUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        // SAFETY: live device; destroyed in `destroy`.
        let buffer =
            unsafe { device.create_buffer(&buffer_ci, None) }.expect("create the staging buffer");
        // SAFETY: the buffer was just created on this device.
        let req = unsafe { device.get_buffer_memory_requirements(buffer) };
        // SAFETY: live instance + physical device (fn contract).
        let props = unsafe { instance.get_physical_device_memory_properties(pd) };
        let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let type_index = (0..props.memory_type_count)
            .find(|&i| {
                req.memory_type_bits & (1u32 << i) != 0
                    && props.memory_types[i as usize]
                        .property_flags
                        .contains(wanted)
            })
            .expect("a HOST_VISIBLE|HOST_COHERENT memory type for the staging buffer");
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(type_index);
        // SAFETY: live device, size from the requirements just queried; freed in
        // `destroy`.
        let memory =
            unsafe { device.allocate_memory(&alloc, None) }.expect("allocate staging memory");
        // SAFETY: fresh buffer bound to fresh memory of the required size.
        unsafe { device.bind_buffer_memory(buffer, memory, 0) }.expect("bind staging memory");
        // SAFETY: the memory is HOST_VISIBLE and not yet mapped; the mapping
        // lives until `destroy` frees the memory (implicit unmap).
        let mapped =
            unsafe { device.map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty()) }
                .expect("map the staging buffer")
                .cast_const()
                .cast::<u8>();

        Self {
            device: device.clone(),
            queue,
            cmd_pool,
            cmd,
            fence,
            buffer,
            memory,
            mapped,
            display,
            format,
            bytes_per_sample,
            frame_bytes,
        }
    }

    /// Copy `frame`'s cropped planes into the staging buffer, tightly packed
    /// (Y `w*h`, then interleaved UV `w*h/2`) — ffmpeg `-f rawvideo -pix_fmt nv12`.
    /// `bufferRowLength = 0` packs rows at the copy extent, so pitch/crop padding
    /// cannot leak into the hash.
    ///
    /// # Safety
    ///
    /// `frame` was delivered by a decoder on this device and is not yet
    /// released; its image carries TRANSFER_SRC (`PF_VKD_TEST_READBACK`); no other
    /// work uses the graphics queue or this image concurrently (test is serialized).
    unsafe fn read_nv12(&self, frame: &DecodedVkFrame) -> Vec<u8> {
        let (width, height) = self.display;
        assert_eq!(
            (frame.crop.width, frame.crop.height),
            self.display,
            "the vector's display size this readback was built for (the goldens \
             hash exactly this region)"
        );
        assert_eq!(
            (frame.crop.x % 2, frame.crop.y % 2),
            (0, 0),
            "chroma-aligned crop origin"
        );

        let begin = vk::CommandBufferBeginInfo::default()
            .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
        // SAFETY: RESET_COMMAND_BUFFER pool (begin implicitly resets); previous
        // submit was fence-waited.
        unsafe { self.device.begin_command_buffer(self.cmd, &begin) }
            .expect("begin the readback command buffer");

        let subresource = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: frame.layer,
            layer_count: 1,
        };
        // Timeline wait at submit carries decode visibility; no src access here.
        let to_transfer = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::COPY)
            .dst_access_mask(vk::AccessFlags2::TRANSFER_READ)
            .old_layout(frame.layout)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(frame.image)
            .subresource_range(subresource);
        let dep =
            vk::DependencyInfo::default().image_memory_barriers(std::slice::from_ref(&to_transfer));
        // SAFETY: recording state; the image is live until release (fn contract).
        unsafe { self.device.cmd_pipeline_barrier2(self.cmd, &dep) };

        // Crop at the source; plane-1 offsets/extents are in the chroma plane's
        // half-resolution coordinates. Rows pack at the copy extent.
        let layers = |aspect| vk::ImageSubresourceLayers {
            aspect_mask: aspect,
            mip_level: 0,
            base_array_layer: frame.layer,
            layer_count: 1,
        };
        let regions = [
            vk::BufferImageCopy {
                buffer_offset: 0,
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: layers(vk::ImageAspectFlags::PLANE_0),
                image_offset: vk::Offset3D {
                    x: frame.crop.x as i32,
                    y: frame.crop.y as i32,
                    z: 0,
                },
                image_extent: vk::Extent3D {
                    width,
                    height,
                    depth: 1,
                },
            },
            vk::BufferImageCopy {
                // Byte offset (extents above are texels): luma is `w * h * bytes_per_sample`.
                buffer_offset: u64::from(width * height * self.bytes_per_sample),
                buffer_row_length: 0,
                buffer_image_height: 0,
                image_subresource: layers(vk::ImageAspectFlags::PLANE_1),
                image_offset: vk::Offset3D {
                    x: (frame.crop.x / 2) as i32,
                    y: (frame.crop.y / 2) as i32,
                    z: 0,
                },
                image_extent: vk::Extent3D {
                    width: width / 2,
                    height: height / 2,
                    depth: 1,
                },
            },
        ];
        // SAFETY: the image is in TRANSFER_SRC_OPTIMAL via the barrier above and
        // carries TRANSFER_SRC usage (fn contract); the buffer's `frame_bytes`
        // exactly spans the two packed regions.
        unsafe {
            self.device.cmd_copy_image_to_buffer(
                self.cmd,
                frame.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                self.buffer,
                &regions,
            );
        }

        // Presenter contract: restore the delivered layout; HOST_READ the copy.
        let restore = vk::ImageMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::empty())
            .dst_stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)
            .dst_access_mask(vk::AccessFlags2::empty())
            .old_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .new_layout(frame.layout)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(frame.image)
            .subresource_range(subresource);
        let host_read = vk::BufferMemoryBarrier2::default()
            .src_stage_mask(vk::PipelineStageFlags2::COPY)
            .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
            .dst_stage_mask(vk::PipelineStageFlags2::HOST)
            .dst_access_mask(vk::AccessFlags2::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(self.buffer)
            .offset(0)
            .size(vk::WHOLE_SIZE);
        let dep = vk::DependencyInfo::default()
            .image_memory_barriers(std::slice::from_ref(&restore))
            .buffer_memory_barriers(std::slice::from_ref(&host_read));
        // SAFETY: recording state; own buffer, live image.
        unsafe { self.device.cmd_pipeline_barrier2(self.cmd, &dep) };
        // SAFETY: recording above is complete and valid.
        unsafe { self.device.end_command_buffer(self.cmd) }.expect("end the readback commands");

        // Wait `value`, signal `value + 1` — the `DecodedVkFrame` sync contract.
        let wait = vk::SemaphoreSubmitInfo::default()
            .semaphore(frame.semaphore)
            .value(frame.value)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let signal = vk::SemaphoreSubmitInfo::default()
            .semaphore(frame.semaphore)
            .value(frame.value + 1)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS);
        let cmd_info = vk::CommandBufferSubmitInfo::default().command_buffer(self.cmd);
        let submit = vk::SubmitInfo2::default()
            .wait_semaphore_infos(std::slice::from_ref(&wait))
            .command_buffer_infos(std::slice::from_ref(&cmd_info))
            .signal_semaphore_infos(std::slice::from_ref(&signal));
        // SAFETY: live queue/fence; semaphore is the frame's timeline (fn
        // contract); fence was reset after its last use.
        unsafe {
            self.device
                .queue_submit2(self.queue, std::slice::from_ref(&submit), self.fence)
        }
        .expect("submit the readback");
        // SAFETY: the fence was just submitted.
        unsafe {
            self.device
                .wait_for_fences(&[self.fence], true, 10_000_000_000)
        }
        .expect("readback completes within 10s");
        // SAFETY: the fence was observed signalled above.
        unsafe { self.device.reset_fences(&[self.fence]) }.expect("reset the readback fence");

        // SAFETY: `mapped` is `frame_bytes` of HOST_COHERENT memory; the fence
        // wait plus HOST_READ barrier ordered the device writes before this read.
        unsafe { std::slice::from_raw_parts(self.mapped, self.frame_bytes) }.to_vec()
    }

    /// # Safety
    ///
    /// No submission in flight (every `read_nv12` fence-waited) and nothing else
    /// references these handles.
    unsafe fn destroy(&self) {
        // SAFETY: own handles on the live device, idle per the fn contract;
        // freeing the memory implicitly unmaps it.
        unsafe {
            self.device.destroy_buffer(self.buffer, None);
            self.device.free_memory(self.memory, None);
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.cmd_pool, None);
        }
    }
}

/// Wait, read, release with the presenter write-back the readback enqueued.
fn consume_frame(
    decoder: &mut VkDecoder<impl VkCodec>,
    readback: &Readback,
    frame: &DecodedVkFrame,
    index: usize,
) -> Vec<u8> {
    assert_eq!(
        decoder.wait_status(frame),
        DecodeStatus::Ok,
        "frame {index}: decode op not COMPLETE\n  state: {}",
        decoder.debug_snapshot()
    );
    // Wrong pool format decodes, then hashes a different layout; refuse it here.
    assert_eq!(
        frame.format, readback.format,
        "frame {index}: the vector must decode into the pool format the readback \
         was built for"
    );
    // SAFETY: delivered and unreleased on this device; pool has TRANSFER_SRC
    // (`PF_VKD_TEST_READBACK` set before the first decode); test is serialized.
    let nv12 = unsafe { readback.read_nv12(frame) };
    decoder
        .release_frame(frame, true)
        .unwrap_or_else(|e| panic!("frame {index}: release failed: {e}"));
    nv12
}

/// Decode every AU and hash every delivered frame in display order, including
/// the `flush` tail. One body for all three codecs.
///
/// H.264/H.265 `flush` can release a reorder tail; AV1's planner has no flush — a
/// shown frame is output by the unit that decodes it — so `VkAv1Decoder::flush`
/// frees hidden pictures and returns nothing. The shared body catches a stranded
/// shown frame via the frame-count assert.
fn collect_hashes(
    decoder: &mut VkDecoder<impl VkCodec>,
    readback: &Readback,
    aus: &[&[u8]],
) -> Vec<String> {
    let mut hashes: Vec<String> = Vec::new();
    for (au_index, au) in aus.iter().enumerate() {
        let mut next = decoder.decode(au).unwrap_or_else(|e| {
            panic!(
                "AU {au_index}: decode failed: {e}\n  state: {}",
                decoder.debug_snapshot()
            )
        });
        while let Some(frame) = next {
            let planes = consume_frame(decoder, readback, &frame, hashes.len());
            hashes.push(sha256_hex(&planes));
            next = decoder.take_ready();
        }
    }
    decoder.flush();
    while let Some(frame) = decoder.take_ready() {
        let planes = consume_frame(decoder, readback, &frame, hashes.len());
        hashes.push(sha256_hex(&planes));
    }
    eprintln!(
        "final state: {} status_queries={}",
        decoder.debug_snapshot(),
        decoder.status_queries()
    );
    hashes
}

/// Verdict after teardown so a mismatch panic cannot leave the device alive.
fn assert_bit_identical(hashes: &[String], goldens: &[&str], codec: &str) {
    assert_eq!(
        hashes.len(),
        goldens.len(),
        "{codec}: frame count diverges from libavcodec ({} decoded vs {} golden)",
        hashes.len(),
        goldens.len()
    );
    let mut mismatches = 0usize;
    let mut first_divergence: Option<usize> = None;
    for (index, (got, want)) in hashes.iter().zip(goldens.iter()).enumerate() {
        if got.as_str() != *want {
            if mismatches < 10 {
                eprintln!("frame {index}: MISMATCH\n  ours:   {got}\n  golden: {want}");
            }
            first_divergence.get_or_insert(index);
            mismatches += 1;
        }
    }
    // First divergence localises the defect; later mismatches are often DPB
    // downstream of that one frame.
    assert!(
        first_divergence.is_none(),
        "{codec}: FIRST DIVERGENT FRAME = {} ({mismatches}/{} frames diverge from \
         libavcodec; up to 10 printed above). Frame 0 is intra-only — if IT is the \
         first, suspect readback geometry (pitch/crop), the picture format, or intra \
         decode / the per-frame parameter conversion; a first divergence LATER points \
         at inter prediction, per-reference info or DPB management, and the frames \
         after it are probably just downstream of it.",
        first_divergence.unwrap_or_default(),
        hashes.len()
    );
    eprintln!(
        "{codec}: {} frames bit-identical to libavcodec software decode",
        hashes.len()
    );
}

/// Decode `aus` with the decoder `open` builds on a fresh `codec` device, hash every
/// delivered frame at `f`'s display region and layout, and compare with `f`'s goldens.
/// One body for the three codecs; `open` refuses a box without the fixture's profile.
fn parity_run<C: VkCodec>(
    f: &Fixture,
    aus: &[&[u8]],
    label: &str,
    codec: common::Codec,
    open: impl FnOnce(&DeviceHandles) -> VkDecoder<C>,
) {
    // One codec at a time; `set_var` only under this lock (`common::gpu_lock`).
    let _gpu = common::gpu_lock();

    arm_test_readback(&_gpu);

    let goldens = f.goldens();

    let setup = common::bring_up(&common::Request {
        codec,
        // Readback records on graphics; a device without that family is skipped.
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let hashes = {
        let mut decoder = open(&handles);
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists; destroyed
        // at the end of this block after its last read.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                f.display,
                vk_format(f.layout),
            )
        };
        let hashes = collect_hashes(&mut decoder, &readback, aus);
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: decoder Drop drained the queue and destroyed session/pools;
    // readback handles are gone; nothing else references the setup.
    unsafe { setup.destroy() };

    assert_bit_identical(&hashes, &goldens, label);
}

/// The leg for one fixture: its own units and label.
fn fixture_parity_run(f: &Fixture) {
    codec_parity_run(f, &f.split(), f.label);
}

/// `aus` through the decoder for `f`'s codec, which refuses up front a box without the
/// fixture's profile: H.265 at the layout's depth, AV1 Main 4:2:0 8-bit without film grain
/// (grain is part of the Vulkan decode profile). The AUs are a parameter so the
/// three-byte and four-byte legs share it: prefix width carries no information.
fn codec_parity_run(f: &Fixture, aus: &[&[u8]], label: &str) {
    match f.codec {
        parity::Codec::H264 => parity_run(f, aus, label, common::H264, |handles| {
            // SAFETY: `parity_run` brought `handles` up with the H.264 decode extensions
            // and timeline/sync2, and drops the decoder before it destroys them.
            unsafe { VkH264Decoder::new(handles, Box::new(NoopQueueLock)) }
                .expect("wrap the device")
        }),
        parity::Codec::H265 => parity_run(f, aus, label, common::H265, |handles| {
            // SAFETY: `parity_run` brought `handles` up with the H.265 decode extensions
            // and timeline/sync2, and drops the decoder before it destroys them.
            let decoder = unsafe { VkH265Decoder::new(handles, Box::new(NoopQueueLock)) }
                .expect("wrap the device");
            let depth = f.layout.bit_depth() - 8;
            decoder
                .probe_stream_support(1, depth)
                .unwrap_or_else(|e| panic!("{label}: the box must host this H.265 shape — {e:?}"));
            decoder
        }),
        parity::Codec::Av1 => parity_run(f, aus, label, common::AV1, |handles| {
            // SAFETY: `parity_run` brought `handles` up with the AV1 decode extensions
            // and timeline/sync2, and drops the decoder before it destroys them.
            let decoder = unsafe { VkAv1Decoder::new(handles, Box::new(NoopQueueLock)) }
                .expect("wrap the device");
            decoder
                .probe_stream_support(1, 8, false)
                .unwrap_or_else(|e| {
                    panic!("{label}: the box must host AV1 Main 4:2:0 8-bit, no grain — {e:?}")
                });
            decoder
        }),
    }
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::H264);
}

/// Same 250 frames, four-byte start codes as the host emits.
///
/// A failure here where the three-byte leg passes means the extra prefix byte is
/// reaching the driver.
#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_four_byte_start_codes_decode_bit_identically() {
    let stream = common::h264_four_byte_start_codes(parity::H264.bytes);
    codec_parity_run(
        &parity::H264,
        &common::split_h264_aus(&stream),
        "H.264 (4-byte start codes)",
    );
}

/// Host low-delay H.264. The conformance vector never aliases setup and a
/// reference onto one DPB slot; this stream does ([`parity::H264_LOWDELAY`]).
/// DISTINCT hands the aliased reference the setup's array layer; COINCIDE drops it
/// from `pReferenceSlots`.
#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn low_delay_host_h264_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::H264_LOWDELAY);
}

/// AU boundaries of a `PUNKTFUNK_DUMP_VIDEO` capture: `.idx` sidecar when present
/// (`offset len flags complete` per line), H.265 splitter otherwise. Skip
/// `complete == 0` — the client is only ever fed complete AUs.
fn field_aus(stream: &[u8], idx_path: &std::path::Path) -> Vec<std::ops::Range<usize>> {
    let Ok(idx) = std::fs::read_to_string(idx_path) else {
        return common::split_h265_aus(stream)
            .iter()
            .map(|au| {
                let start = au.as_ptr() as usize - stream.as_ptr() as usize;
                start..start + au.len()
            })
            .collect();
    };
    idx.lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let offset: usize = parts.next()?.parse().ok()?;
            let len: usize = parts.next()?.parse().ok()?;
            let _flags = parts.next()?;
            let complete = parts.next()? != "0";
            (complete && offset + len <= stream.len()).then_some(offset..offset + len)
        })
        .collect()
}

/// The AV1 twin of the H.265 field hasher below: decode a capture that carries its `.idx`
/// sidecar (a `PUNKTFUNK_DUMP_VIDEO` dump, or a wave smoke's `write_capture`) and write one
/// SHA-256 per shown frame to `<stream>.pfhash`. A unit that fails prints, counts and writes
/// `-`, so a one-frame-per-unit capture keeps line N on unit N.
///
/// - `PF_VKD_FIELD_STREAM=/path/capture.obu` — the capture (required, `.idx` beside it).
#[test]
#[ignore = "field triage: set PF_VKD_FIELD_STREAM=/path/capture.obu (needs a Vulkan Video AV1 decode device)"]
fn field_av1_stream_writes_frame_hashes() {
    let stream_path = std::env::var("PF_VKD_FIELD_STREAM")
        .expect("PF_VKD_FIELD_STREAM must point at an AV1 capture with its .idx sidecar");
    let stream = std::fs::read(&stream_path).expect("read the capture");
    let idx_path = std::path::PathBuf::from(format!("{stream_path}.idx"));
    assert!(
        idx_path.exists(),
        "an AV1 capture needs its .idx sidecar: {}",
        idx_path.display()
    );
    let units = field_aus(&stream, &idx_path);
    assert!(!units.is_empty(), "no units in the capture");

    // Geometry and depth from the first unit that plans.
    let mut planner = pf_bitstream::av1::Av1Planner::new();
    let picture = units
        .iter()
        .find_map(|r| planner.plan_au(&stream[r.clone()]).ok()?.into_iter().next())
        .expect("no unit in the capture plans")
        .picture;
    let format = match picture.bit_depth {
        8 => pf_vkdecode::NV12,
        10 => pf_vkdecode::P010,
        other => panic!("unsupported field bit depth: {other}"),
    };
    let display = (picture.render_width, picture.render_height);

    // One codec at a time; `set_var` under the lock.
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);
    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let mut unit_errors = 0usize;
    let hashes = {
        // SAFETY: as in `codec_parity_run`.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(picture.chroma_format_idc, picture.bit_depth, false)
            .unwrap_or_else(|e| panic!("the box must host this AV1 shape — {e:?}"));
        // SAFETY: as in `parity_run`.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                display,
                format,
            )
        };
        let mut hashes: Vec<String> = Vec::new();
        for (index, range) in units.iter().enumerate() {
            match decoder.decode(&stream[range.clone()]) {
                Ok(mut next) => {
                    while let Some(frame) = next {
                        let planes = consume_frame(&mut decoder, &readback, &frame, hashes.len());
                        hashes.push(sha256_hex(&planes));
                        next = decoder.take_ready();
                    }
                }
                Err(e) => {
                    unit_errors += 1;
                    hashes.push("-".into());
                    eprintln!("unit {index}: decode failed ({e}) — continuing");
                }
            }
        }
        decoder.flush();
        while let Some(frame) = decoder.take_ready() {
            let planes = consume_frame(&mut decoder, &readback, &frame, hashes.len());
            hashes.push(sha256_hex(&planes));
        }
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing else
        // references its handles.
        unsafe { readback.destroy() };
        hashes
    };
    // SAFETY: decoder Drop drained the queue; readback handles are gone.
    unsafe { setup.destroy() };

    let out = format!("{stream_path}.pfhash");
    std::fs::write(&out, hashes.join("\n") + "\n").expect("write the hash file");
    eprintln!(
        "AV1 (field capture): {} frames hashed → {out} ({unit_errors} unit errors)",
        hashes.len()
    );
}

/// Decode a `PUNKTFUNK_DUMP_VIDEO` H.265 capture and write one SHA-256 per
/// display-order frame to `<stream>.pfhash`. `scripts/vkdecode-field-parity.sh`
/// diffs that against ffmpeg and names the first divergent frame.
///
/// - `PF_VKD_FIELD_STREAM=/path/au-*.h265` — the capture (required).
/// - `PF_VKD_FIELD_YUV=12,13` — write those frames' planes to
///   `<stream>.frame<N>.yuv` (optional, after the script names the divergence).
///
/// Per-AU errors (truncated tail, renegotiation): print, count, resume at the
/// next IRAP. A mid-capture resolution change ends the comparable stretch;
/// off-size crops are released unshown.
#[test]
#[ignore = "field triage: set PF_VKD_FIELD_STREAM=/path/capture.h265 (needs a Vulkan Video H.265 decode device; RADV additionally RADV_PERFTEST=video_decode)"]
fn field_h265_stream_writes_frame_hashes_for_ffmpeg_diff() {
    let stream_path = std::env::var("PF_VKD_FIELD_STREAM").expect(
        "PF_VKD_FIELD_STREAM must point at a PUNKTFUNK_DUMP_VIDEO .h265 capture \
         (see this test's docs)",
    );
    let stream = std::fs::read(&stream_path).expect("read the capture");
    let idx_path = std::path::PathBuf::from(format!("{stream_path}.idx"));
    let aus = field_aus(&stream, &idx_path);
    assert!(!aus.is_empty(), "no AUs in the capture");

    // Join at the first AU that plans: pre-IRAP AUs of a mid-session capture fail
    // with AwaitingIdr-shaped errors.
    let mut planner = pf_bitstream::h265::H265Planner::new();
    let mut first_planned = None;
    for (index, range) in aus.iter().enumerate() {
        if let Ok(plan) = planner.plan_au(&stream[range.clone()]) {
            first_planned = Some((index, plan.picture));
            break;
        }
    }
    let (start_index, picture) = first_planned.expect("no AU in the capture plans");
    let (format, label) = match picture.bit_depth_luma_minus8 {
        0 => (pf_vkdecode::NV12, "H.265 (field capture, 8-bit)"),
        2 => (pf_vkdecode::P010, "H.265 (field capture, 10-bit)"),
        other => panic!("unsupported field bit depth: {}", other + 8),
    };
    assert_eq!(
        picture.chroma_format_idc, 1,
        "the readback speaks NV12/P010 — a 4:4:4 capture needs its own leg"
    );
    let display = (picture.display_crop.width, picture.display_crop.height);
    let yuv_wanted: std::collections::BTreeSet<usize> = std::env::var("PF_VKD_FIELD_YUV")
        .unwrap_or_default()
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();

    // One codec at a time; `set_var` under the lock.
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let setup = common::bring_up(&common::Request {
        codec: common::H265,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let mut au_errors = 0usize;
    let mut off_size = 0usize;
    let mut concealed_aus: Vec<usize> = Vec::new();
    let hashes = {
        // SAFETY: `setup` outlives this block; created with H.265 decode
        // extensions + timeline/sync2.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(picture.chroma_format_idc, picture.bit_depth_luma_minus8)
            .unwrap_or_else(|e| panic!("{label}: the box must host this shape — {e:?}"));
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists; destroyed
        // at the end of this block.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                display,
                format,
            )
        };

        let mut hashes: Vec<String> = Vec::new();
        let sink = |decoder: &mut VkH265Decoder,
                    frame: DecodedVkFrame,
                    hashes: &mut Vec<String>,
                    off_size: &mut usize| {
            if (frame.crop.width, frame.crop.height) != display {
                // Renegotiated stretch — not comparable against this readback.
                *off_size += 1;
                decoder
                    .release_frame(&frame, false)
                    .expect("release an off-size frame unshown");
                return;
            }
            let index = hashes.len();
            let planes = consume_frame(decoder, &readback, &frame, index);
            if yuv_wanted.contains(&index) {
                let path = format!("{stream_path}.frame{index}.yuv");
                std::fs::write(&path, &planes).expect("write the requested .yuv");
                eprintln!("frame {index}: planes written to {path}");
            }
            hashes.push(sha256_hex(&planes));
        };
        for (au_index, range) in aus.iter().enumerate().skip(start_index) {
            match decoder.decode(&stream[range.clone()]) {
                Ok(mut next) => {
                    // Lossy captures hold concealed AUs; ffmpeg conceals them
                    // differently, so a divergence there is not this decoder.
                    let warnings = decoder.take_warnings();
                    if warnings
                        .iter()
                        .any(pf_vkdecode::H265PlanWarning::is_integrity)
                    {
                        if concealed_aus.len() < 10 {
                            eprintln!("AU {au_index}: planned with concealment {warnings:?}");
                        }
                        concealed_aus.push(au_index);
                    }
                    while let Some(frame) = next {
                        sink(&mut decoder, frame, &mut hashes, &mut off_size);
                        next = decoder.take_ready();
                    }
                }
                // Print, count, continue — recovery latch resumes at the next IRAP.
                Err(e) => {
                    au_errors += 1;
                    eprintln!("AU {au_index}: decode failed ({e}) — continuing");
                }
            }
        }
        decoder.flush();
        while let Some(frame) = decoder.take_ready() {
            sink(&mut decoder, frame, &mut hashes, &mut off_size);
        }
        eprintln!(
            "final state: {} status_queries={}",
            decoder.debug_snapshot(),
            decoder.status_queries()
        );
        // SAFETY: every readback was fence-waited inside `read_nv12`; nothing
        // else references its handles.
        unsafe { readback.destroy() };
        hashes
    };

    // SAFETY: decoder Drop drained the queue and destroyed session/pools;
    // readback handles are gone; nothing else references the setup.
    unsafe { setup.destroy() };

    let out = format!("{stream_path}.pfhash");
    std::fs::write(&out, hashes.join("\n") + "\n").expect("write the hash file");
    eprintln!(
        "{label}: {} frames hashed → {out} (skipped {start_index} pre-join AUs, \
         {au_errors} AU errors, {off_size} off-size frames); diff with \
         scripts/vkdecode-field-parity.sh",
        hashes.len()
    );
    // No concealed AU: every divergence the diff finds is this decoder's.
    if concealed_aus.is_empty() {
        eprintln!("integrity: no AU needed concealment — any divergence is OURS");
    } else {
        eprintln!(
            "integrity: {} of {} AUs planned with concealment (first: {:?}) — a \
             divergence AT or AFTER the first is likely ffmpeg concealing differently, \
             not this decoder",
            concealed_aus.len(),
            aus.len() - start_index,
            &concealed_aus[..concealed_aus.len().min(10)]
        );
    }
}

#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::H265);
}

/// Ten-bit path — the only non-8-bit leg here. Goldens are P010; the Vulkan pool
/// is `G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16` (10 bits in the high end of each
/// 16-bit word), the same layout, so one golden file serves both rungs.
#[test]
#[ignore = "needs a Vulkan Video H.265 Main 10 decode device (fleet boxes; see module docs)"]
fn main10_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::MAIN10);
}

/// Host low-delay HEVC. Five-picture DPB, four marked references, no reorder: the
/// `SlotMap` retires and reissues a slot on the same AU. The vendored vector
/// reorders and never does that. pf-bitstream's parity guard pins the planner side.
#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn low_delay_host_h265_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::H265_LOWDELAY);
}

/// See [`h264_four_byte_start_codes_decode_bit_identically`].
#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_four_byte_start_codes_decode_bit_identically() {
    let stream = common::h265_four_byte_start_codes(parity::H265.bytes);
    codec_parity_run(
        &parity::H265,
        &common::split_h265_aus(&stream),
        "H.265 (4-byte start codes)",
    );
}

/// 250 temporal units in, 250 displayed frames out (24 hidden frames are decoded
/// and referenced, never shown). Tightly packed NV12 over the render region.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::AV1);
}

/// Host AV1 at the only resolution that emits more than one tile.
///
/// The vendored vector is `tile_cols = tile_rows = 1`. This stream is
/// `tile_rows = 2`, `height_in_sbs_minus_1 = [16, 16]`, both tiles in one Tile
/// Group OBU. Readback is 12,441,600 bytes/frame, not 115,200.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn low_delay_host_av1_every_frame_hashes_bit_identical_to_libavcodec() {
    fixture_parity_run(&parity::AV1_LOWDELAY);
}

/// A host that reopens its encoder at a new size mid-session sends a new
/// sequence header and key frame into the same decoder: `ensure_state` must
/// retire the session and pools and rebuild at the new extent. The other legs
/// start at their final size. Here the 320×240 vector runs first, then the 4K
/// two-tile host stream with no flush between, and both must still hash
/// bit-identical to libavcodec.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_size_change_mid_session_rebuilds_and_stays_bit_identical() {
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let (small_f, large_f) = (parity::AV1, parity::AV1_LOWDELAY);
    let (small, large) = (small_f.split(), large_f.split());
    assert_eq!((small.len(), large.len()), (small_f.units, large_f.units));

    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let (small_hashes, large_hashes) = {
        // SAFETY: as in `codec_parity_run`.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(1, 8, false)
            .expect("AV1 Main 4:2:0 8-bit, no film grain");
        // SAFETY: as in `parity_run`; one readback per geometry.
        let readback_small = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                small_f.display,
                vk_format(small_f.layout),
            )
        };
        // SAFETY: as above.
        let readback_large = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                large_f.display,
                vk_format(large_f.layout),
            )
        };
        let mut small_hashes: Vec<String> = Vec::new();
        for (au_index, au) in small.iter().enumerate() {
            let mut next = decoder
                .decode(au)
                .unwrap_or_else(|e| panic!("small AU {au_index}: decode failed: {e}"));
            while let Some(frame) = next {
                let planes =
                    consume_frame(&mut decoder, &readback_small, &frame, small_hashes.len());
                small_hashes.push(sha256_hex(&planes));
                next = decoder.take_ready();
            }
        }
        // No flush: the next unit is the new sequence header + key frame.
        let mut large_hashes: Vec<String> = Vec::new();
        for (au_index, au) in large.iter().enumerate() {
            let mut next = decoder.decode(au).unwrap_or_else(|e| {
                panic!(
                    "4K AU {au_index} after the size change: decode failed: {e}\n  state: {}",
                    decoder.debug_snapshot()
                )
            });
            while let Some(frame) = next {
                let planes =
                    consume_frame(&mut decoder, &readback_large, &frame, large_hashes.len());
                large_hashes.push(sha256_hex(&planes));
                next = decoder.take_ready();
            }
        }
        decoder.flush();
        while let Some(frame) = decoder.take_ready() {
            let planes = consume_frame(&mut decoder, &readback_large, &frame, large_hashes.len());
            large_hashes.push(sha256_hex(&planes));
        }
        eprintln!("final state: {}", decoder.debug_snapshot());
        // SAFETY: every readback was fence-waited inside `read_nv12`.
        unsafe {
            readback_small.destroy();
            readback_large.destroy();
        }
        (small_hashes, large_hashes)
    };
    // SAFETY: decoder Drop drained the queue; readback handles are gone.
    unsafe { setup.destroy() };

    assert_bit_identical(
        &small_hashes,
        &small_f.goldens(),
        "AV1 (320x240, before the size change)",
    );
    assert_bit_identical(
        &large_hashes,
        &large_f.goldens(),
        "AV1 (4K two-tile, after the size change)",
    );
    eprintln!(
        "AV1: 240p then 4K two-tile through one decoder, every frame bit-identical to libavcodec"
    );
}

/// Frame 0 pixels vs libavcodec, byte for byte. The hash leg names no cause; the
/// printed [`parity::Divergence`] does (`PLANE_1` copy region for chroma-only, crop
/// origin or pool extent for a shift). Equality is last so a failure prints the
/// report above the panic.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_frame0_pixels_say_which_plane_and_how_badly() {
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let aus = parity::AV1.split();
    assert_eq!(aus.len(), parity::AV1.units, "the vector's temporal units");
    let ours = av1_first_frame(&aus);

    report_divergence(&ours, parity::AV1_FRAME0);
    assert_eq!(
        sha256_hex(&ours),
        parity::AV1.goldens()[0],
        "AV1 frame 0 is not libavcodec's — read the report above for the class"
    );
    eprintln!("AV1 frame 0 is byte-identical to libavcodec");
}

/// `loop_filter_level[2]` and `[3]` (U/V deblock) as bit offsets from the first AU.
///
/// Derived from syntax: temporal delimiter (2) + sequence header (2+11) +
/// `OBU_FRAME` (1 + 2-byte leb128) put the uncompressed header at byte 18.
/// `loop_filter_level[0]` starts at bit 35; four `f(6)` levels (5.9.11) put U at
/// bit 47 and V at bit 53. The GPU probe re-parses before decode.
const AV1_FRAME0_FILTER_LEVEL_U_BIT: usize = 18 * 8 + 47;
const AV1_FRAME0_FILTER_LEVEL_V_BIT: usize = 18 * 8 + 53;

/// Strongest AV1 deblock (`f(6)`). The probe rewrites both chroma levels to this.
const MAX_LOOP_FILTER_LEVEL: u8 = 63;

/// Driver reads chroma deblock levels.
///
/// Decode frame 0 twice — coded `[8, 12]` vs rewritten `[63, 63]` — and require
/// chroma to differ, luma identical. Identical chroma: audit the lifetime of every
/// block the submit points at (`pColorConfig` / Std sequence header) first. A
/// freed `pColorConfig` reads as `mono_chrome = 1`, and a monochrome frame skips
/// those levels (AV1 7.14).
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_frame0_probes_whether_the_driver_reads_the_chroma_deblocking_levels() {
    let _gpu = common::gpu_lock();
    arm_test_readback(&_gpu);

    let aus = parity::AV1.split();
    assert_eq!(aus.len(), parity::AV1.units);

    let mutated_au = av1_frame0_with_max_chroma_deblocking(aus[0]);
    let mut units: Vec<&[u8]> = aus.clone();
    units[0] = &mutated_au;

    let coded = av1_first_frame(&aus);
    let maxed = av1_first_frame(&units);

    let luma = (parity::AV1.display.0 * parity::AV1.display.1) as usize;
    eprintln!("  coded chroma levels [8, 12]  {}", sha256_hex(&coded));
    eprintln!("  chroma levels [63, 63]       {}", sha256_hex(&maxed));
    eprintln!(
        "  libavcodec's frame 0         {}",
        sha256_hex(parity::AV1_FRAME0)
    );
    assert_eq!(
        coded[..luma],
        maxed[..luma],
        "the chroma deblocking levels must not move a luma sample — if they did, \
         the mutation desynchronised the frame header and the chroma comparison \
         below means nothing"
    );
    assert_ne!(
        coded[luma..],
        maxed[luma..],
        "the driver produced the SAME chroma from loop_filter_level[2..3] = [8, 12] \
         and from [63, 63]. This happened once before and the driver was INNOCENT: \
         a monochrome-looking sequence header makes it skip both levels, and ours \
         looked monochrome because its `pColorConfig` block had been freed and \
         reused before the decode op was recorded (see this test's docs). So audit \
         the LIFETIME of everything the submission points at — the Std sequence \
         header behind the parameters object first — before blaming the vendor"
    );
    eprintln!("the driver reads the chroma deblocking levels — the two decodes differ");
}

/// First AU with both chroma deblock levels rewritten to [`MAX_LOOP_FILTER_LEVEL`].
/// Offsets are syntax-derived; neighbours are fixed-width, so a miss probes the
/// wrong parameter without desynchronising. The CPU test checks the mutation.
fn av1_frame0_with_max_chroma_deblocking(au: &[u8]) -> Vec<u8> {
    let mut mutated = au.to_vec();
    for bit in [AV1_FRAME0_FILTER_LEVEL_U_BIT, AV1_FRAME0_FILTER_LEVEL_V_BIT] {
        set_bits(&mut mutated, bit, 6, MAX_LOOP_FILTER_LEVEL);
    }

    let before = av1_first_header(au);
    let after = av1_first_header(&mutated);
    assert_eq!(
        before.loop_filter_params.loop_filter_level,
        [1, 7, 8, 12],
        "the vendored vector's frame 0 codes these levels, and the whole probe is \
         built around the last two of them"
    );
    assert_eq!(
        after.loop_filter_params.loop_filter_level,
        [1, 7, MAX_LOOP_FILTER_LEVEL, MAX_LOOP_FILTER_LEVEL],
        "the rewrite must land on the two CHROMA levels and leave the luma pair \
         alone — a luma change would make the probe's control meaningless"
    );
    // A shifted rewrite would corrupt a neighbouring block before it showed in pixels.
    assert_eq!(
        after.cdef_params, before.cdef_params,
        "the CDEF block follows the loop filter block and is what a shifted rewrite \
         would corrupt first"
    );
    assert_eq!(after.quantization_params, before.quantization_params);
    assert_eq!(after.tile_info, before.tile_info);
    assert_eq!(
        after.loop_restoration_params,
        before.loop_restoration_params
    );
    assert_eq!(after.segmentation_params, before.segmentation_params);
    assert_eq!(
        (
            after.loop_filter_params.loop_filter_sharpness,
            after.loop_filter_params.loop_filter_ref_deltas,
            after.loop_filter_params.loop_filter_mode_deltas,
        ),
        (
            before.loop_filter_params.loop_filter_sharpness,
            before.loop_filter_params.loop_filter_ref_deltas,
            before.loop_filter_params.loop_filter_mode_deltas,
        ),
        "the rest of the loop filter block rides after the levels and must survive"
    );
    assert_ne!(mutated, au, "the rewrite must actually change bytes");
    mutated
}

#[test]
fn the_av1_chroma_deblocking_mutation_changes_only_those_two_levels() {
    let aus = parity::AV1.split();
    let mutated = av1_frame0_with_max_chroma_deblocking(aus[0]);
    // U ends mid-byte, so two or three bytes change; a whole-unit diff means
    // `set_bits` walked off its field.
    let changed = aus[0].iter().zip(&mutated).filter(|(a, b)| a != b).count();
    assert!(
        (1..=3).contains(&changed),
        "twelve bits spanning at most three bytes, and {changed} bytes changed"
    );
}

/// Overwrite the `bits`-wide big-endian bitfield at `bit`.
///
/// AV1 `f(n)` is MSB-first from the OBU payload: same width, same position, so
/// nothing after the field shifts.
fn set_bits(data: &mut [u8], bit: usize, bits: usize, value: u8) {
    for i in 0..bits {
        let at = bit + i;
        let mask = 1u8 << (7 - (at % 8));
        let set = (value >> (bits - 1 - i)) & 1 == 1;
        if set {
            data[at / 8] |= mask;
        } else {
            data[at / 8] &= !mask;
        }
    }
}

fn av1_first_header(au: &[u8]) -> pf_bitstream::av1::ParsedFrameHeader {
    let mut planner = pf_bitstream::av1::Av1Planner::new();
    let plans = planner.plan_au(au).expect("the unit plans");
    let plan = plans.first().expect("the unit carries a frame");
    (*plan.header).clone()
}

/// Decode only as far as the first delivered frame; tightly packed NV12. Owns its
/// device so a test may call it twice; GPU lock and readback hook are the caller's.
fn av1_first_frame(aus: &[&[u8]]) -> Vec<u8> {
    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::Required,
        report_families: true,
    });
    let handles = setup.handles();

    let ours = {
        // SAFETY: `setup` outlives this block; created with the AV1 decode
        // extension + timeline/sync2.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        decoder
            .probe_stream_support(1, 8, false)
            .expect("the box must host AV1 Main 4:2:0 8-bit, no film grain");
        // SAFETY: live instance/device; queue 0 of `graphics_qf` exists.
        let readback = unsafe {
            Readback::new(
                &setup.instance,
                setup.pd,
                &setup.device,
                setup.graphics_qf,
                parity::AV1.display,
                vk_format(parity::AV1.layout),
            )
        };

        // First delivered frame only: the first unit is a shown key frame.
        let mut first: Option<Vec<u8>> = None;
        for (index, au) in aus.iter().enumerate() {
            let frame = decoder
                .decode(au)
                .unwrap_or_else(|e| panic!("AU {index}: decode failed: {e}"));
            if let Some(frame) = frame {
                assert_eq!(
                    decoder.wait_status(&frame),
                    DecodeStatus::Ok,
                    "frame 0: decode op not COMPLETE\n  state: {}",
                    decoder.debug_snapshot()
                );
                assert_eq!(frame.format, pf_vkdecode::NV12, "frame 0: pool format");
                // SAFETY: delivered and unreleased; pool has TRANSFER_SRC; serialized.
                first = Some(unsafe { readback.read_nv12(&frame) });
                decoder
                    .release_frame(&frame, true)
                    .expect("frame 0: release");
                // A unit may carry more than one frame; this leg stops at the first.
                // Spare frames go back with `false` — nothing signalled their timeline.
                while let Some(spare) = decoder.take_ready() {
                    decoder
                        .release_frame(&spare, false)
                        .expect("release an unread frame of the same temporal unit");
                }
                break;
            }
        }
        // SAFETY: every readback was fence-waited inside `read_nv12`.
        unsafe { readback.destroy() };
        first.expect("the vector's first temporal unit shows a frame")
    };

    // SAFETY: decoder and readback are gone.
    unsafe { setup.destroy() };
    ours
}

/// Print where AV1 frame 0 differs from libavcodec's. See
/// [`av1_frame0_pixels_say_which_plane_and_how_badly`].
fn report_divergence(ours: &[u8], want: &[u8]) {
    let (width, height) = parity::AV1.display;
    eprintln!(
        "--- AV1 frame 0: {width}x{height} NV12, {} bytes ---",
        ours.len()
    );
    eprintln!("  ours   {}", sha256_hex(ours));
    eprintln!("  golden {}", sha256_hex(want));
    eprintln!(
        "  {}",
        parity::localise(ours, want, parity::AV1.display, parity::AV1.layout)
    );
}

// CPU guards — not `#[ignore]`d. The fixture guards run in pf-bitstream's
// `testing::parity`; these cover what only this file builds: the frame-0 blob and the
// four-byte rewrite.

/// [`parity::AV1_FRAME0`] pixels must hash to the AV1 golden set's first line. A
/// stale blob after a golden regen names the wrong cause. Also pins layout: 320×240
/// packed NV12 is 115200 bytes, luma first.
#[test]
fn the_av1_frame0_reference_is_the_first_golden() {
    let frame0 = parity::AV1_FRAME0;
    let (width, height) = (
        parity::AV1.display.0 as usize,
        parity::AV1.display.1 as usize,
    );
    assert_eq!(
        frame0.len(),
        width * height * 3 / 2,
        "data/test-25fps-av1.frame0.nv12 must be one tightly packed NV12 frame of \
         the vector's render region"
    );
    let goldens = parity::AV1.goldens();
    assert_eq!(
        sha256_hex(frame0),
        goldens[0],
        "the vendored frame-0 pixels must hash to the AV1 golden set's FIRST entry — \
         if they no longer do, the blob is from a different decode than the goldens \
         and `av1_frame0_pixels_say_which_plane_and_how_badly` would attribute a \
         divergence to the wrong cause. Regenerate it alongside the goldens: decode \
         the vector with `-f rawvideo -pix_fmt nv12 -fps_mode passthrough` and take \
         the first 115200 bytes (the golden file's header carries the full command)"
    );
    // A repeated-byte frame would pass a length check and make per-plane stats vacuous.
    let luma = &frame0[..width * height];
    let chroma = &frame0[width * height..];
    assert!(
        luma.iter().any(|b| *b != luma[0]) && chroma.iter().any(|b| *b != chroma[0]),
        "both planes must carry real picture content"
    );
}

/// Annex-B start codes as `(total, three_byte)`. Emulation prevention means
/// `00 00 01` cannot occur inside a NAL; a hit not preceded by a zero is three-byte.
fn annexb_prefixes(stream: &[u8]) -> (usize, usize) {
    let mut total = 0;
    let mut three_byte = 0;
    for i in 0..stream.len().saturating_sub(2) {
        if stream[i..i + 3] == [0x00, 0x00, 0x01] {
            total += 1;
            if i == 0 || stream[i - 1] != 0x00 {
                three_byte += 1;
            }
        }
    }
    (total, three_byte)
}

// Four-byte GPU legs assert the rewrite matches the original goldens — true if
// the rewrite returned its input or dropped NALs. These catch that in CI.

#[test]
fn the_h264_four_byte_rewrite_changes_prefixes_and_nothing_else() {
    use pf_bitstream::h264::H264Planner;

    let original = common::TEST_25FPS_H264;
    let rewritten = common::h264_four_byte_start_codes(original);

    let (original_total, original_three) = annexb_prefixes(original);
    let (rewritten_total, rewritten_three) = annexb_prefixes(&rewritten);

    assert!(
        original_three > 0,
        "the vendored H.264 vector is supposed to carry THREE-byte start codes; \
         if it no longer does, `h264_four_byte_start_codes_decode_bit_identically` \
         is feeding the hardware the same bytes as the leg above it and proves \
         nothing"
    );
    assert_eq!(
        rewritten_three, 0,
        "every start code in the rewritten stream must be four-byte — {rewritten_three} \
         of {rewritten_total} are not"
    );
    assert_eq!(
        rewritten_total, original_total,
        "the rewrite must preserve the NAL count exactly ({original_total}), not \
         drop or invent units"
    );
    assert!(
        rewritten.len() > original.len(),
        "widening every prefix cannot shrink the stream"
    );

    // Same AUs, same planner verdict: framing changed, nothing the decoder acts on.
    let aus = common::split_h264_aus(&rewritten);
    assert_eq!(
        aus.len(),
        common::split_h264_aus(original).len(),
        "the rewritten stream must split into the same access units"
    );
    assert_eq!(aus.len(), 250, "…and there are 250 of them");

    let mut planner = H264Planner::new();
    let mut outputs = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("AU {index}: the four-byte rewrite must plan as the original does, got {e:?}")
        });
        outputs += plan.dpb.outputs.len();
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs, 250,
        "the rewritten vector must still output 250 pictures"
    );
}

#[test]
fn the_h265_four_byte_rewrite_changes_prefixes_and_nothing_else() {
    use pf_bitstream::h265::H265Planner;

    let original = common::TEST_25FPS_H265;
    let rewritten = common::h265_four_byte_start_codes(original);

    let (original_total, original_three) = annexb_prefixes(original);
    let (rewritten_total, rewritten_three) = annexb_prefixes(&rewritten);

    assert!(
        original_three > 0,
        "the vendored H.265 vector is supposed to carry THREE-byte start codes; \
         if it no longer does, `h265_four_byte_start_codes_decode_bit_identically` \
         proves nothing"
    );
    assert_eq!(
        rewritten_three, 0,
        "every start code in the rewritten stream must be four-byte — {rewritten_three} \
         of {rewritten_total} are not"
    );
    assert_eq!(
        rewritten_total, original_total,
        "the rewrite must preserve the NAL count exactly ({original_total})"
    );
    assert!(
        rewritten.len() > original.len(),
        "widening every prefix cannot shrink the stream"
    );

    let aus = common::split_h265_aus(&rewritten);
    assert_eq!(
        aus.len(),
        common::split_h265_aus(original).len(),
        "the rewritten stream must split into the same access units"
    );
    assert_eq!(aus.len(), 250, "…and there are 250 of them");

    let mut planner = H265Planner::new();
    let mut outputs = 0usize;
    for (index, au) in aus.iter().enumerate() {
        let plan = planner.plan_au(au).unwrap_or_else(|e| {
            panic!("AU {index}: the four-byte rewrite must plan as the original does, got {e:?}")
        });
        outputs += plan.dpb.outputs.len();
    }
    outputs += planner.flush().outputs.len();
    assert_eq!(
        outputs, 250,
        "the rewritten vector must still output 250 pictures"
    );
}
