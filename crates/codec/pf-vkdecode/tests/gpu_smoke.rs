//! Vulkan Video decode smoke for H.264, H.265, and AV1.
//!
//! Each ignored GPU leg wraps a real device, derives session caps, and decodes
//! 48 AUs of the vendored 25fps vector through `vkCmdDecodeVideoKHR` while the
//! consumer holds [`CLIENT_HOLD`] frames. Every `RESULT_STATUS_ONLY` query must
//! read COMPLETE before release. A pool that ignores DPB depth or that hold
//! starves here. A missing codec extension is a skip, not a failure.
//!
//! Pixels, presenter interop (`value + 1` write-back), soak, and both vendors'
//! DPB layouts belong to `gpu_parity` and on-glass tests. Releases pass `false`
//! because no presenter runs. Device bring-up lives in `tests/common/mod.rs`.
//!
//! Prerequisite: Vulkan 1.3 with `VK_KHR_video_queue`, `VK_KHR_video_decode_queue`,
//! the codec decode extension, a `VIDEO_DECODE_KHR` queue, and `timelineSemaphore`
//! plus `synchronization2`. RADV needs `RADV_PERFTEST=video_decode`.
//!
//! ```text
//! cargo test -p pf-vkdecode -- --ignored
//! ```

mod common;

use ash::vk;
use pf_vkdecode::DecodeStatus;
use pf_vkdecode::DecodedVkFrame;
use pf_vkdecode::NoopQueueLock;
use pf_vkdecode::VkAv1Decoder;
use pf_vkdecode::VkCodec;
use pf_vkdecode::VkDecoder;
use pf_vkdecode::VkH264Decoder;
use pf_vkdecode::VkH265Decoder;

/// Past every vector's DPB (`max_dpb_frames = 7` H.264, eight AV1 slots), so
/// slots re-activate onto fresh pool images.
const AUS: usize = 48;
/// Client hold depth: channels + FrameStore preroll + in-flight present ≈ 4–7.
const CLIENT_HOLD: usize = 4;
/// 48 AUs may leave a few pictures in reorder; below this is a delivery failure.
const MIN_DELIVERED: usize = 40;

struct Geometry {
    /// Conformance-window size.
    display: (u32, u32),
    /// Exact allocated extent, if a driver has observed it.
    ///
    /// `pictureAccessGranularity` rounds the coded size up, so this is per-vector
    /// and per-driver, not a bitstream fact. `None` asserts only allocated ≥ display.
    exact_coded: Option<(u32, u32)>,
}

/// Decode [`AUS`] AUs holding [`CLIENT_HOLD`] frames; COMPLETE before each release.
///
/// Shared across codecs so the AV1 leg cannot prove less than the H.264 leg.
fn smoke(decoder: &mut VkDecoder<impl VkCodec>, aus: &[&[u8]], geometry: &Geometry) {
    // PF_VKD_TEST_READBACK adds TRANSFER_SRC to the picture pool. Production
    // never carries that usage; a leftover from the parity legs would pass
    // this test against a pool that is not the one we ship.
    assert!(
        std::env::var_os("PF_VKD_TEST_READBACK").is_none(),
        "PF_VKD_TEST_READBACK is set in the environment: it grows the picture pool \
         a usage flag production never carries, so this leg would no longer be \
         testing the production pool arrangement. Unset it for the smoke legs \
         (the parity legs set it themselves, under the same GPU lock)."
    );
    // No presenter: releases pass `presenter_signaled = false` (no `value + 1`).
    let mut held: std::collections::VecDeque<DecodedVkFrame> = std::collections::VecDeque::new();
    let mut delivered = 0usize;
    let mut geometry_checked = false;
    for (index, au) in aus.iter().enumerate().take(AUS) {
        let mut next = decoder.decode(au).unwrap_or_else(|e| {
            panic!(
                "AU {index}: decode failed: {e}\n  state: {}",
                decoder.debug_snapshot()
            )
        });
        while let Some(frame) = next {
            if !geometry_checked {
                assert_eq!(
                    (frame.crop.width, frame.crop.height),
                    geometry.display,
                    "the vector's display region"
                );
                assert!(
                    frame.coded_width >= frame.crop.width
                        && frame.coded_height >= frame.crop.height,
                    "the ALLOCATED extent ({}x{}) must cover the display region ({}x{})",
                    frame.coded_width,
                    frame.coded_height,
                    frame.crop.width,
                    frame.crop.height,
                );
                if let Some(exact) = geometry.exact_coded {
                    assert_eq!(
                        (frame.coded_width, frame.coded_height),
                        exact,
                        "ALLOCATED extent (this vector needs no granularity padding here)"
                    );
                }
                // 8-bit 4:2:0 vectors must land on NV12; a wrong pool format
                // still decodes, then renders with the wrong maths.
                assert_eq!(
                    frame.format,
                    pf_vkdecode::NV12,
                    "8-bit 4:2:0 vector must decode into an NV12 pool"
                );
                assert_ne!(frame.image, vk::Image::null());
                assert_ne!(frame.semaphore, vk::Semaphore::null());
                assert!(frame.value > 0);
                eprintln!(
                    "geometry: allocated {}x{} display {}x{} format {:?} layout {:?}",
                    frame.coded_width,
                    frame.coded_height,
                    frame.crop.width,
                    frame.crop.height,
                    frame.format,
                    frame.layout,
                );
                geometry_checked = true;
            }
            held.push_back(frame);
            delivered += 1;
            while held.len() > CLIENT_HOLD {
                let oldest = held.pop_front().expect("nonempty");
                assert_eq!(
                    decoder.wait_status(&oldest),
                    DecodeStatus::Ok,
                    "AU {index}: decode op not COMPLETE\n  state: {}",
                    decoder.debug_snapshot()
                );
                decoder
                    .release_frame(&oldest, false)
                    .unwrap_or_else(|e| panic!("AU {index}: release failed: {e}"));
            }
            next = decoder.take_ready();
        }
    }
    for frame in held.drain(..) {
        assert_eq!(decoder.wait_status(&frame), DecodeStatus::Ok);
        decoder
            .release_frame(&frame, false)
            .expect("tail frames release");
    }
    assert!(
        delivered >= MIN_DELIVERED,
        "expected at least {MIN_DELIVERED} delivered frames from {AUS} AUs, got {delivered}"
    );
    eprintln!(
        "final state: {} status_queries={}",
        decoder.debug_snapshot(),
        decoder.status_queries()
    );
}

#[test]
#[ignore = "needs a Vulkan Video H.264 decode device (fleet boxes; see module docs)"]
fn h264_decodes_48_aus_holding_four_frames_like_the_real_client() {
    // One codec at a time on the device (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    let setup = common::bring_up(&common::Request {
        codec: common::H264,
        // Decode-only is enough (nothing else submits) and exercises EXCLUSIVE
        // picture-pool sharing.
        graphics: common::Graphics::DecodeFamilyIsFine,
        report_families: false,
    });
    let handles = setup.handles();
    {
        // SAFETY: `setup` outlives this block (destroyed below, after the decoder
        // drops at the block's end), it was created with the H.264 decode
        // extensions + timeline/sync2 features, and its queue fields name the
        // families/queues it created.
        let mut decoder = unsafe { VkH264Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        smoke(
            &mut decoder,
            &common::split_h264_aus(common::TEST_25FPS_H264),
            &Geometry {
                display: (320, 240),
                exact_coded: Some((320, 240)),
            },
        );
    }
    // SAFETY: the decoder is gone (its Drop drained the queue and destroyed its
    // session/pools), and nothing else references the setup's handles.
    unsafe { setup.destroy() };
}

#[test]
#[ignore = "needs a Vulkan Video H.265 decode device (fleet boxes; see module docs)"]
fn h265_decodes_48_aus_holding_four_frames_like_the_real_client() {
    // One codec at a time on the device (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    let setup = common::bring_up(&common::Request {
        codec: common::H265,
        graphics: common::Graphics::DecodeFamilyIsFine,
        report_families: false,
    });
    let handles = setup.handles();
    {
        // SAFETY: as the H.264 leg — `setup` outlives this block and was created
        // with the H.265 decode extensions + timeline/sync2 features.
        let mut decoder = unsafe { VkH265Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // Shape gate (Main, 4:2:0, 8-bit → NV12) before the first AU so a device
        // that cannot host the combination refuses with a caps reason, not a
        // mid-stream decode failure.
        decoder
            .probe_stream_support(1, 0)
            .expect("the box must host H.265 Main 8-bit 4:2:0 (the vector's shape)");
        smoke(
            &mut decoder,
            &common::split_h265_aus(common::TEST_25FPS_H265),
            &Geometry {
                display: (320, 240),
                // HEVC `pictureAccessGranularity` is unpinned; print coded size
                // rather than guess.
                exact_coded: None,
            },
        );
    }
    // SAFETY: as the H.264 leg — the decoder is gone and nothing else references
    // the setup's handles.
    unsafe { setup.destroy() };
}

/// Same 48 AUs and hold depth as H.26x, but AV1 keeps hidden frames resident.
///
/// The first 48 temporal units decode more pictures than they show (the CPU
/// guard below asserts the counts); each hidden frame occupies a pool image as
/// a reference. A pool sized one-picture-per-AU starves here rather than in
/// `gpu_parity`, where the same miss looks like a generic decode failure.
#[test]
#[ignore = "needs a Vulkan Video AV1 decode device (fleet boxes; see module docs)"]
fn av1_decodes_48_aus_holding_four_frames_like_the_real_client() {
    // One codec at a time on the device (see `common::gpu_lock`).
    let _gpu = common::gpu_lock();

    let setup = common::bring_up(&common::Request {
        codec: common::AV1,
        graphics: common::Graphics::DecodeFamilyIsFine,
        report_families: false,
    });
    let handles = setup.handles();
    {
        // SAFETY: as the H.264 leg — `setup` outlives this block and was created
        // with the AV1 decode extension + timeline/sync2 features.
        let mut decoder = unsafe { VkAv1Decoder::new(&handles, Box::new(NoopQueueLock)) }
            .expect("wrap the device");
        // Shape gate (Main, 4:2:0, 8-bit, no film grain → NV12). Grain synthesis
        // is part of the Vulkan decode PROFILE, so a grain-only box refuses here
        // with a caps reason rather than at the first temporal unit.
        decoder.probe_stream_support(1, 8, false).expect(
            "the box must host AV1 Main 4:2:0 8-bit without film grain (the vector's shape)",
        );
        smoke(
            &mut decoder,
            &common::split_ivf(common::TEST_25FPS_AV1),
            &Geometry {
                display: (320, 240),
                // AV1 decode extent is the post-superres width and granularity is
                // unpinned; print coded size rather than guess.
                exact_coded: None,
            },
        );
    }
    // SAFETY: as the H.264 leg — the decoder is gone and nothing else references
    // the setup's handles.
    unsafe { setup.destroy() };
}

// CPU guards (not ignored): [`MIN_DELIVERED`] vs what each planner emits from
// the first [`AUS`] AUs. GPU legs do not run in CI; a deeper-reorder vector
// must fail here, not as pool starvation on hardware.

#[test]
fn the_delivery_floor_is_under_what_the_planners_emit_from_the_first_48_aus() {
    let h264 = {
        let mut planner = pf_bitstream::h264::H264Planner::new();
        common::split_h264_aus(common::TEST_25FPS_H264)
            .iter()
            .take(AUS)
            .enumerate()
            .map(|(index, au)| {
                planner
                    .plan_au(au)
                    .unwrap_or_else(|e| panic!("H.264 AU {index} must plan, got {e:?}"))
                    .dpb
                    .outputs
                    .len()
            })
            .sum::<usize>()
    };
    let h265 = {
        let mut planner = pf_bitstream::h265::H265Planner::new();
        common::split_h265_aus(common::TEST_25FPS_H265)
            .iter()
            .take(AUS)
            .enumerate()
            .map(|(index, au)| {
                planner
                    .plan_au(au)
                    .unwrap_or_else(|e| panic!("H.265 AU {index} must plan, got {e:?}"))
                    .dpb
                    .outputs
                    .len()
            })
            .sum::<usize>()
    };
    // One AV1 temporal unit can plan several frames; count every plan's outputs
    // or a reader under-counts by treating one plan per unit.
    let (av1, av1_frames) = {
        let mut planner = pf_bitstream::av1::Av1Planner::new();
        let mut outputs = 0usize;
        let mut frames = 0usize;
        for (index, au) in common::split_ivf(common::TEST_25FPS_AV1)
            .iter()
            .take(AUS)
            .enumerate()
        {
            let plans = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AV1 temporal unit {index} must plan, got {e:?}"));
            frames += plans.len();
            outputs += plans.iter().map(|p| p.dpb.outputs.len()).sum::<usize>();
        }
        (outputs, frames)
    };
    // No `flush` here on purpose: the smoke legs do not flush either, so the
    // planner's un-flushed output count is exactly the frame budget they have.
    assert!(
        h264 >= MIN_DELIVERED,
        "the H.264 leg asserts >= {MIN_DELIVERED} delivered but the planner only \
         outputs {h264} pictures from the first {AUS} AUs"
    );
    assert!(
        h265 >= MIN_DELIVERED,
        "the H.265 leg asserts >= {MIN_DELIVERED} delivered but the planner only \
         outputs {h265} pictures from the first {AUS} AUs"
    );
    assert!(
        av1 >= MIN_DELIVERED,
        "the AV1 leg asserts >= {MIN_DELIVERED} delivered but the planner only \
         outputs {av1} pictures from the first {AUS} temporal units"
    );
    // The floor above cannot see hidden frames the pool must still hold. Equal
    // counts would mean the vector no longer exercises multi-frame temporal units.
    assert!(
        av1_frames > av1,
        "the first {AUS} AV1 temporal units must decode MORE frames ({av1_frames}) \
         than they show ({av1}); equal counts mean the hidden-frame coverage is gone"
    );
}
