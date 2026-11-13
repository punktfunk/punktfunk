use super::{
    build_h265_rps_s0, intra_refresh_caps, setup_slot, trusted_refs, PhaseStats,
    VulkanVideoEncoder, PHASE_IMPORT, PHASE_REPORT_EVERY, PHASE_SPIKE, PHASE_SPIKE_EVERY,
    PHASE_WAIT,
};

/// Frame 300 is lost at 120 fps; confirmations trail by 12 frames and the report lands 20
/// frames later, when the 8-slot ring holds only frames after the loss. The kept pin is still
/// resident and older than the loss, so the loss heals by anchor, not a wave.
#[test]
fn a_late_loss_report_finds_the_kept_pin() {
    let (lost, lag, report) = (300i64, 12i64, 20i64);
    let run = |keep: bool| {
        let (mut slot_wire, mut kept, mut prev) = (vec![-1i64; 8], crate::rfi::Kept::default(), 0);
        for wire in 0..lost + report {
            let setup = if wire == 0 {
                0
            } else {
                setup_slot(wire as u64, prev, &slot_wire, &kept)
            };
            slot_wire[setup] = wire;
            let confirmed = crate::Acked {
                last: (wire - lag).min(lost - 1),
                mask: 0xffff,
            };
            if keep {
                let held = slot_wire.clone();
                kept.step(wire, true, Some(&confirmed), |w| held.contains(&w));
            }
            prev = setup;
        }
        crate::rfi::pick_anchor(&trusted_refs(&slot_wire), lost)
    };
    let anchor = run(true).expect("the kept pin answers the late report");
    assert!(
        anchor.1 < lost && lost - anchor.1 <= 2 * lag + 1,
        "{anchor:?}"
    );
    assert_eq!(
        run(false),
        None,
        "the bare ring holds nothing older than the loss"
    );
}
use crate::env_knobs::Knobs;
use crate::test_dmabuf::{assert_edge_is_straight, edge_frame, LinearDmabuf, ODD_H, ODD_W};
use crate::test_frames::{cpu_frame, cpu_frame_24};
use crate::{Codec, Encoder};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

/// VBR when offered and not refused, CBR's loose window otherwise, the driver's cap on bitrate.
#[test]
fn rc_plan_prefers_offered_vbr_and_clamps_to_the_driver() {
    use super::rc_plan;
    use ash::vk::VideoEncodeRateControlModeFlagsKHR as Rc;
    let both = Rc::CBR | Rc::VBR;
    let vbr = (Rc::VBR, crate::vbv_window_ms(60), 20_000_000);
    let cbr = (Rc::CBR, (1000, 500), 20_000_000);
    assert_eq!(rc_plan(both, None, 60, 20_000_000, u64::MAX), vbr);
    assert_eq!(rc_plan(both, Some("auto"), 60, 20_000_000, u64::MAX), vbr);
    assert_eq!(rc_plan(both, Some(" CBR "), 60, 20_000_000, u64::MAX), cbr);
    // `vbr` is honoured only when advertised; a driver with neither still gets CBR.
    assert_eq!(rc_plan(Rc::CBR, Some("vbr"), 60, 20_000_000, u64::MAX), cbr);
    assert_eq!(rc_plan(Rc::empty(), None, 60, 20_000_000, u64::MAX), cbr);
    assert_eq!(
        rc_plan(both, None, 60, 900_000_000, 400_000_000).2,
        400_000_000
    );
}

/// The largest CTB and transform the driver offers, the transform never above the CTB.
#[test]
fn h265_blocks_follow_the_driver_caps() {
    use super::h265_block_log2;
    use ash::vk::VideoEncodeH265CtbSizeFlagsKHR as Ctb;
    use ash::vk::VideoEncodeH265TransformBlockSizeFlagsKHR as Tb;
    let all_tb = Tb::TYPE_4 | Tb::TYPE_8 | Tb::TYPE_16 | Tb::TYPE_32;
    assert_eq!(h265_block_log2(Ctb::TYPE_64, all_tb), (6, 5)); // VCN
    assert_eq!(h265_block_log2(Ctb::TYPE_32, all_tb), (5, 5)); // NVIDIA
    assert_eq!(h265_block_log2(Ctb::TYPE_16 | Ctb::TYPE_32, all_tb), (5, 5));
    assert_eq!(h265_block_log2(Ctb::TYPE_16, all_tb), (4, 4));
    assert_eq!(
        h265_block_log2(Ctb::TYPE_64, Tb::TYPE_4 | Tb::TYPE_16),
        (6, 4)
    );
    assert_eq!(h265_block_log2(Ctb::empty(), Tb::empty()), (6, 5));
}

/// The profile chain's structure types, head first.
fn chain_types(p: &ash::vk::VideoProfileInfoKHR) -> Vec<ash::vk::StructureType> {
    let mut out = vec![p.s_type];
    let mut next = p.p_next as *const ash::vk::BaseInStructure;
    while !next.is_null() {
        // SAFETY: every link `ProfileStack::wire` makes is a Vulkan struct that opens with
        // `sType`/`pNext`, alive in the stack the caller still holds.
        let base = unsafe { &*next };
        out.push(base.s_type);
        next = base.p_next;
    }
    out
}

/// One constructor for the session and every image profile: the VALVE rgb link is the only
/// thing `rgb` changes, and it hangs off usage.
#[test]
fn profile_chain_adds_rgb_conversion_only_when_asked() {
    use super::{codec_op_for, ProfileStack};
    use crate::vk_valve_rgb as vrgb;
    for av1 in [false, true] {
        let mut plain = ProfileStack::new(codec_op_for(av1), false, false);
        let mut rgb = ProfileStack::new(codec_op_for(av1), false, true);
        let (plain, rgb) = (chain_types(plain.wire(av1)), chain_types(rgb.wire(av1)));
        assert_eq!(plain.len(), 3, "profile → codec → usage");
        assert_eq!(
            plain[2],
            ash::vk::StructureType::VIDEO_ENCODE_USAGE_INFO_KHR
        );
        assert_eq!(rgb[..3], plain[..]);
        assert_eq!(rgb[3..], [vrgb::stype(vrgb::ST_PROFILE_INFO)]);
    }
}

/// Native planar pairs: NV12 is the 8-bit source, P010 the 10-bit one; a crossed pair
/// names a picture the session cannot program.
#[test]
fn native_planar_format_matches_only_the_depth_pair() {
    use super::native_planar_format_matches;
    assert!(native_planar_format_matches(PixelFormat::Nv12, false));
    assert!(native_planar_format_matches(PixelFormat::P010, true));
    assert!(!native_planar_format_matches(PixelFormat::Nv12, true));
    assert!(!native_planar_format_matches(PixelFormat::P010, false));
}

/// A studio session clears the shaders' `FULL_RANGE` constant, which Vulkan drops silently if a
/// rebuilt blob no longer declares it: the pixels would stay full under a studio VUI.
#[test]
fn the_csc_blobs_declare_the_range_constant() {
    use super::{CSC10_709_SPV, CSC10_SPV, CSC_FULL_RANGE_SPEC_ID, CSC_SPV};
    // `OpDecorate %target SpecId <id>`: word count 4, opcode 71, decoration 1.
    let declares = |spv: &[u8]| {
        let words: Vec<u32> = spv
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        words
            .windows(4)
            .any(|w| w[0] == (4 << 16 | 71) && w[2] == 1 && w[3] == CSC_FULL_RANGE_SPEC_ID)
    };
    assert!(declares(CSC_SPV), "rgb2yuv.spv");
    assert!(declares(CSC10_709_SPV), "rgb2yuv10_709.spv");
    assert!(declares(CSC10_SPV), "rgb2yuv10.spv");
}

/// Imported EXCLUSIVE images are re-acquired from the producer family on every use —
/// a cached acquire is an ownership transfer out of GENERAL, never IGNORED families.
#[test]
fn imported_acquire_moves_ownership_every_time() {
    use super::imported_acquire_barrier;
    use ash::vk::{self, Handle};
    let img = vk::Image::from_raw(7);
    for fresh in [true, false] {
        let b = imported_acquire_barrier(
            img,
            fresh,
            9,
            3,
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_READ,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
        );
        assert_eq!(b.src_queue_family_index, 9);
        assert_eq!(b.dst_queue_family_index, 3);
        assert_eq!(
            b.old_layout,
            if fresh {
                vk::ImageLayout::UNDEFINED
            } else {
                vk::ImageLayout::GENERAL
            }
        );
    }
}

/// The release hands the image back to the producer family in GENERAL — the layout the
/// next cached acquire comes out of.
#[test]
fn imported_release_returns_ownership_in_general() {
    use super::imported_release_barrier;
    use ash::vk::{self, Handle};
    let b = imported_release_barrier(
        vk::Image::from_raw(7),
        vk::ImageLayout::VIDEO_ENCODE_SRC_KHR,
        3,
        9,
        vk::PipelineStageFlags2::VIDEO_ENCODE_KHR,
        vk::AccessFlags2::VIDEO_ENCODE_READ_KHR,
    );
    assert_eq!(b.src_queue_family_index, 3);
    assert_eq!(b.dst_queue_family_index, 9);
    assert_eq!(b.new_layout, vk::ImageLayout::GENERAL);
}

/// The latch needs every gate at once; the 780M's measured caps pass, each missing one fails.
#[test]
fn intra_refresh_latch_needs_every_gate() {
    use crate::vk_intra_refresh as vir;
    let good = vir::VideoEncodeIntraRefreshCapabilitiesKHR {
        s_type: vir::stype(vir::ST_CAPABILITIES),
        p_next: std::ptr::null_mut(),
        intra_refresh_modes: vir::MODE_BLOCK_BASED
            | vir::MODE_BLOCK_ROW_BASED
            | vir::MODE_BLOCK_COLUMN_BASED,
        max_intra_refresh_cycle_duration: 256,
        max_intra_refresh_active_reference_pictures: 1,
        partition_independent_intra_refresh_regions: ash::vk::TRUE,
        non_rectangular_intra_refresh_regions: ash::vk::FALSE,
    };
    assert_eq!(
        intra_refresh_caps(true, true, &good).map(|c| c.max_cycle),
        Ok(256)
    );
    assert!(intra_refresh_caps(false, true, &good).is_err());
    assert!(intra_refresh_caps(true, false, &good).is_err());
    let mut c = vir::VideoEncodeIntraRefreshCapabilitiesKHR { ..good };
    c.intra_refresh_modes = vir::MODE_BLOCK_COLUMN_BASED;
    assert!(intra_refresh_caps(true, true, &c).is_err());
    let mut c = vir::VideoEncodeIntraRefreshCapabilitiesKHR { ..good };
    c.partition_independent_intra_refresh_regions = ash::vk::FALSE;
    assert!(intra_refresh_caps(true, true, &c).is_err());
    let mut c = vir::VideoEncodeIntraRefreshCapabilitiesKHR { ..good };
    c.max_intra_refresh_cycle_duration = 1;
    assert!(intra_refresh_caps(true, true, &c).is_err());
}

/// Full-retention RPS: every resident listed, setup occupant excluded, `used_by_curr_pic`
/// marks only the real reference.
#[test]
fn h265_rps_retains_all_residents() {
    // Slots hold POCs 8..15, current 16, reconstructing over POC 8, referencing POC 15.
    let slot_poc = [8i32, 9, 10, 11, 12, 13, 14, 15];
    let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 0, 15, 16);
    assert_eq!(n, 7, "all residents except the dying setup occupant");
    // Newest-first cumulative deltas: POCs 15,14,...,9 → every step is 1.
    assert_eq!(&deltas[..7], &[0u16; 7], "delta_minus1 chain of 1-steps");
    assert_eq!(used, 1 << 0, "only the newest (POC 15) is actively used");

    // Recovery: reference an older picture (POC 12) while newer residents stay listed.
    let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 0, 12, 16);
    assert_eq!(n, 7);
    assert_eq!(used, 1 << 3, "POC 12 is 4th-newest → S0 index 3");
    assert_eq!(&deltas[..7], &[0u16; 7]);

    // Sparse DPB after IDR: only POCs 0..2 resident.
    let slot_poc = [0i32, 1, 2, -1, -1, -1, -1, -1];
    let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 3, 2, 3);
    assert_eq!(n, 3);
    assert_eq!(&deltas[..3], &[0, 0, 0]);
    assert_eq!(used, 1 << 0);

    // Non-adjacent POCs: current 10, residents {9, 6, 2} → deltas-minus1 {0, 2, 3}.
    let slot_poc = [2i32, -1, 6, -1, 9, -1, -1, -1];
    let (n, deltas, used) = build_h265_rps_s0(&slot_poc, 7, 6, 10);
    assert_eq!(n, 3);
    assert_eq!(&deltas[..3], &[0, 2, 3]);
    assert_eq!(used, 1 << 1, "POC 6 is the 2nd-newest → S0 index 1");
}

/// BGRX frame of the shared moving texture at `frame` frames of motion
/// (`pf_encode_core::smoke_pattern`): the encoder must reach for rows above to predict it.
fn cpu_frame_scroll(w: u32, h: u32, pts_ns: u64, frame: u32) -> CapturedFrame {
    let buf = crate::smoke_pattern::scroll_pattern(w as usize, h as usize, frame as usize);
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}

const SMOKE_LOST: usize = 4;
/// Recovery-anchor index. RFI fires just before this submission; one normal P (frame 5,
/// referencing lost frame 4) is encoded in between, so a conforming decoder processes that
/// RPS before the anchor arrives. Frame 3 survives only because every P-frame's RPS lists
/// all resident DPB pictures ([`build_h265_rps_s0`]).
const SMOKE_ANCHOR: usize = 6;

/// Full `open` → IDR → P-frames → RFI-recovery. [`SMOKE_LOST`] is dropped; one P still
/// references it; [`SMOKE_ANCHOR`] re-anchors on pre-loss frame 3 (no IDR).
fn run_smoke(codec: Codec) -> Vec<crate::EncodedFrame> {
    run_smoke_opts(codec, false).expect("smoke")
}

/// `run_smoke` with RGB-direct explicit. `None` = probe declined (soft-skip).
fn run_smoke_opts(codec: Codec, rgb: bool) -> Option<Vec<crate::EncodedFrame>> {
    let env_dim = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
    let mut enc = VulkanVideoEncoder::open_opts(codec, w, h, 60, 10_000_000, rgb).expect("open");
    if rgb && enc.rgb.is_none() {
        eprintln!("run_smoke_opts: RGB-direct unavailable on this driver — skipping");
        return None;
    }
    assert!(enc.caps().supports_rfi, "must advertise RFI");

    let colors = [
        [40u8, 40, 200, 255],
        [40, 200, 40, 255],
        [200, 40, 40, 255],
        [200, 200, 40, 255],
        [40, 200, 200, 255],
        [200, 40, 200, 255],
        [120, 200, 80, 255],
        [80, 120, 200, 255],
    ];
    let mut aus: Vec<crate::EncodedFrame> = Vec::new();
    for (i, c) in colors.iter().enumerate() {
        if i == SMOKE_ANCHOR {
            // Next frame must re-anchor on a resident pre-loss reference (newest older = 3).
            assert!(
                enc.invalidate_ref_frames(SMOKE_LOST as i64, SMOKE_LOST as i64),
                "RFI should find an older-than-loss slot"
            );
        }
        enc.submit_indexed(&cpu_frame(w, h, i as u64 * 16_666_667, *c), i as u32)
            .expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        aus.push(au);
    }
    assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");

    let (mut keyframes, mut anchors) = (0usize, 0usize);
    for (i, au) in aus.iter().enumerate() {
        assert!(!au.data.is_empty(), "AU {i} empty");
        keyframes += au.keyframe as usize;
        anchors += au.recovery_anchor as usize;
        if i == 0 {
            assert!(au.keyframe, "frame 0 must be IDR");
        }
        if i == SMOKE_ANCHOR {
            assert!(
                au.recovery_anchor && !au.keyframe,
                "frame {SMOKE_ANCHOR} must be a clean recovery P-frame, not IDR"
            );
        }
    }
    assert_eq!(keyframes, 1, "exactly one IDR (frame 0)");
    assert_eq!(
        anchors, 1,
        "exactly one recovery anchor (frame {SMOKE_ANCHOR})"
    );
    Some(aus)
}

/// Dump full stream + client-view with AU [`SMOKE_LOST`] removed. Full stream must decode
/// 0-error. Dropped dump: one missing-ref at frame 5, none at the anchor (a complaint about
/// frame 3 means retention regressed).
fn dump_smoke(aus: &[crate::EncodedFrame], ext: &str) {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
    let p1 = format!("{home}/vkenc-host-smoke.{ext}");
    let _ = std::fs::write(&p1, &full);
    eprintln!(
        "run_smoke: wrote {p1} ({} bytes, {} AUs)",
        full.len(),
        aus.len()
    );
    let dropped: Vec<u8> = aus
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != SMOKE_LOST)
        .flat_map(|(_, a)| a.data.iter().copied())
        .collect();
    let p2 = format!("{home}/vkenc-host-smoke-dropped.{ext}");
    let _ = std::fs::write(&p2, &dropped);
    eprintln!(
        "run_smoke: wrote {p2} (frame {SMOKE_LOST} dropped; frame 5 conceals, \
         recovery@{SMOKE_ANCHOR} anchors to frame 3 and must decode clean)"
    );
}

/// HEVC smoke. `#[ignore]`d: needs a real `VK_KHR_video_encode_h265` device.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
fn vulkan_smoke() {
    dump_smoke(&run_smoke(Codec::H265), "h265");
}

/// AV1 smoke. Dumps `.obu` (TD + seq-header prefixes ahead of each frame OBU).
#[test]
#[ignore = "needs a real VK_KHR_video_encode_av1 device (run on the RADV host, not the build box)"]
fn vulkan_smoke_av1() {
    dump_smoke(&run_smoke(Codec::Av1), "obu");
}

/// RGB-direct (EFC) smoke. Soft-skips where the extension/probe is unavailable.
#[test]
#[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
fn vulkan_smoke_rgb() {
    if let Some(aus) = run_smoke_opts(Codec::H265, true) {
        dump_smoke(&aus, "rgb.h265");
    }
}

/// RGB-direct AV1 twin of [`vulkan_smoke_rgb`].
#[test]
#[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
fn vulkan_smoke_rgb_av1() {
    if let Some(aus) = run_smoke_opts(Codec::Av1, true) {
        dump_smoke(&aus, "rgb.obu");
    }
}

/// Wave smoke frame plan, 256×256 (4 CTB rows → cycle 4): IDR + 2 P, then an RFI with
/// every reference tainted, which used to force an IDR and now starts a wave.
const WAVE_START: usize = 3;
const WAVE_CYCLE: usize = 4;

/// The wave replaces the IDR: start mark, plain wave frames, close mark, no IDR, no anchor.
/// A later loss of the plain P after the wave must re-anchor on the close (a fully swept
/// picture is trusted) while mid-wave pictures never are.
fn run_wave_smoke(codec: Codec, ext: &str) {
    let (w, h) = (256u32, 256u32);
    let mut enc = VulkanVideoEncoder::open_opts(codec, w, h, 60, 10_000_000, false).expect("open");
    if enc.intra_refresh.is_none() {
        eprintln!("run_wave_smoke: intra refresh unavailable on this driver — skipping");
        return;
    }
    // `PF_WAVE_RESTART=1`: two frames into the wave a frame inside its sweep is lost, so
    // it restarts there with a fresh start mark; only the restarted close may lift.
    let restart = std::env::var("PF_WAVE_RESTART").is_ok_and(|v| v == "1");
    let start2 = if restart { WAVE_START + 2 } else { WAVE_START };
    let close = start2 + WAVE_CYCLE - 1;
    let after_wave = close + 1; // the plain P after the close
    let anchor_p = after_wave + 1; // re-anchors on the close after `after_wave` is lost
    let mut aus: Vec<crate::EncodedFrame> = Vec::new();
    for i in 0..=anchor_p {
        if i == WAVE_START {
            assert!(
                enc.invalidate_ref_frames(0, WAVE_START as i64 - 1),
                "a wave-capable encoder answers an RFI with no anchor"
            );
            assert!(
                enc.wave.is_none(),
                "the wave starts at frame-build, not here"
            );
        }
        if restart && i == start2 {
            let lost = (i - 1) as i64;
            assert!(
                enc.invalidate_ref_frames(lost, lost),
                "a loss inside the sweep"
            );
        }
        if i == anchor_p {
            assert!(
                enc.invalidate_ref_frames(after_wave as i64, after_wave as i64),
                "the wave close is a trusted anchor"
            );
        }
        // Scrolling texture: vertical motion makes the encoder reach for rows above,
        // which is what the clean-region constraint has to refuse across a stripe.
        enc.submit_indexed(
            &cpu_frame_scroll(w, h, i as u64 * 16_666_667, i as u32),
            i as u32,
        )
        .expect("submit");
        if i == WAVE_START || i == start2 {
            assert_eq!(
                enc.wave,
                Some(crate::rfi::Wave {
                    cycle: WAVE_CYCLE as u32,
                    index: 1
                }),
                "frame {i} started a wave at frame-build"
            );
        }
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        aus.push(au);
    }
    assert_eq!(aus.len(), anchor_p + 1, "one AU per submitted frame");
    assert!(enc.wave.is_none(), "the wave closed");

    assert!(aus[0].keyframe, "frame 0 is the IDR");
    for (i, au) in aus.iter().enumerate().skip(1) {
        assert!(!au.data.is_empty(), "AU {i} empty");
        assert!(
            !au.keyframe,
            "AU {i}: no IDR after frame 0 — the wave replaced it"
        );
        let start = i == WAVE_START || i == start2;
        assert_eq!(
            au.recovery_point,
            start || i == close,
            "AU {i}: recovery_point marks every start and the close"
        );
        assert_eq!(au.recovery_close, i == close, "AU {i}: the close bit");
        assert_eq!(
            au.recovery_anchor,
            i == anchor_p,
            "AU {i}: the only anchor P answers the post-wave loss"
        );
    }
    // Full stream, and the client's view with the pre-wave P frames lost (and the
    // restart's frame): decoded side by side, the close must match the full decode.
    if let Ok(home) = std::env::var("HOME") {
        let full: Vec<&[u8]> = aus.iter().map(|a| a.data.as_slice()).collect();
        let p = format!("{home}/vkenc-wave-smoke.{ext}");
        let _ = crate::smoke_pattern::write_capture(&p, &full);
        let dropped: Vec<&[u8]> = aus
            .iter()
            .enumerate()
            .filter(|(i, _)| *i == 0 || (*i >= WAVE_START && *i != start2 - 1))
            .map(|(_, a)| a.data.as_slice())
            .collect();
        let p2 = format!("{home}/vkenc-wave-smoke-dropped.{ext}");
        let _ = crate::smoke_pattern::write_capture(&p2, &dropped);
        eprintln!(
            "run_wave_smoke: wrote {p} ({} bytes, {} AUs) and {p2} (frames 1..{} dropped; \
             the close at {close} must decode identical to the full stream)",
            full.iter().map(|a| a.len()).sum::<usize>(),
            aus.len(),
            WAVE_START,
        );
    }
}

/// HEVC wave smoke. Run it through `scripts/vk-validation-tests.sh`: the validation layer is
/// the only check of the dirty-region bookkeeping, which RADV ignores.
#[test]
#[ignore = "needs VK_KHR_video_encode_intra_refresh (RADV >= Mesa 25.3 on VCN hardware)"]
fn vulkan_wave_smoke() {
    run_wave_smoke(Codec::H265, "h265");
}

/// AV1 twin of [`vulkan_wave_smoke`]; the wave start is error-resilient.
#[test]
#[ignore = "needs VK_KHR_video_encode_intra_refresh (RADV >= Mesa 25.3 on VCN hardware)"]
fn vulkan_wave_smoke_av1() {
    run_wave_smoke(Codec::Av1, "obu");
}

/// The anchor soak ([`crate::smoke_pattern::Soak`], its `PF_WAVE_*` knobs). With
/// `PF_WAVE_CONFIRM` and a `PF_WAVE_LAG` past the DPB ring, every ask anchors on the kept
/// picture instead of starting a wave.
#[test]
#[ignore = "needs a real VK_KHR_video_encode device"]
fn vulkan_anchor_soak() {
    let soak = crate::smoke_pattern::Soak::from_env();
    let (w, h, fps) = (soak.w, soak.h, soak.fps);
    let mut enc =
        VulkanVideoEncoder::open_opts(soak.codec, w, h, fps, soak.mbps * 1_000_000, false)
            .expect("open");
    let pts = |i: usize| i as u64 * 1_000_000_000 / u64::from(fps);
    soak.run("vulkan", &mut enc, |i| {
        cpu_frame_scroll(w, h, pts(i), i as u32)
    });
}

/// Packed 2:10:10:10 (`xRGB_210LE` / `PixelFormat::X2Rgb10`) CPU frame. Channels are 10-bit
/// code values (PQ container units).
fn cpu_frame_rgb10(w: u32, h: u32, pts_ns: u64, rgb10: [u16; 3]) -> CapturedFrame {
    // x:R:G:B 2:10:10:10 LE — B in bits 0-9, G in 10-19, R in 20-29.
    let word = ((rgb10[0] as u32 & 0x3FF) << 20)
        | ((rgb10[1] as u32 & 0x3FF) << 10)
        | (rgb10[2] as u32 & 0x3FF);
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&word.to_le_bytes());
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns,
        format: PixelFormat::X2Rgb10,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}

/// 10-bit twin of [`run_smoke_opts`]: Main10 / AV1-at-10, `G10X6…3PACK16` picture + DPB.
/// Colour truth is out of band (`dump_smoke`); in-tree asserts encode + AU count + depth.
/// `None` = device declined the 10-bit profile (soft skip).
fn run_smoke_10bit(codec: Codec, rgb: bool) -> Option<Vec<crate::EncodedFrame>> {
    let env_dim = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
    let mut enc =
        match VulkanVideoEncoder::open_opts_depth(codec, w, h, 60, 10_000_000, rgb, true, true) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("run_smoke_10bit({codec:?}, rgb={rgb}): open declined — {e:#}");
                return None;
            }
        };
    if rgb && enc.rgb.is_none() {
        eprintln!("run_smoke_10bit: BT.2020 RGB-direct unavailable on this driver — skipping");
        return None;
    }
    assert!(enc.spec.ten_bit, "a 10-bit session must report 10-bit");

    // Span the range so a wrong shift shows up as wildly wrong luminance in the dump.
    let colors: [[u16; 3]; 8] = [
        [160, 160, 800],
        [160, 800, 160],
        [800, 160, 160],
        [800, 800, 160],
        [160, 800, 800],
        [800, 160, 800],
        [480, 800, 320],
        [320, 480, 800],
    ];
    let mut aus: Vec<crate::EncodedFrame> = Vec::new();
    for (i, c) in colors.iter().enumerate() {
        enc.submit_indexed(&cpu_frame_rgb10(w, h, i as u64 * 16_666_667, *c), i as u32)
            .expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        aus.push(au);
    }
    assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");
    assert!(aus[0].keyframe, "frame 0 must be IDR");
    for (i, au) in aus.iter().enumerate() {
        assert!(!au.data.is_empty(), "AU {i} empty");
    }
    Some(aus)
}

/// HEVC Main10 through the compute CSC.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device with a 10-bit profile"]
fn vulkan_smoke_10bit() {
    if let Some(aus) = run_smoke_10bit(Codec::H265, false) {
        dump_smoke(&aus, "10bit.h265");
    }
}

/// AV1 at 10 bits.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_av1 device with a 10-bit profile"]
fn vulkan_smoke_10bit_av1() {
    if let Some(aus) = run_smoke_10bit(Codec::Av1, false) {
        dump_smoke(&aus, "10bit.obu");
    }
}

/// HDR with no host CSC: EFC does BT.2020 conversion. Soft-skips without `MODEL_YCBCR_2020`.
#[test]
#[ignore = "needs VK_VALVE_video_encode_rgb_conversion with the BT.2020 model at 10-bit"]
fn vulkan_smoke_rgb_10bit() {
    if let Some(aus) = run_smoke_10bit(Codec::H265, true) {
        dump_smoke(&aus, "rgb.10bit.h265");
    }
}

/// 10-bit SDR twin of [`run_smoke_10bit`]: a Main10 / AV1-10 session fed an 8-bit BGRA capture
/// (`bit_depth == 10`, HDR off) — `rgb2yuv10_709.comp` widens 8→10 under BT.709. `None` = the
/// device declined the 10-bit profile.
fn run_smoke_10bit_sdr(codec: Codec) -> Option<Vec<crate::EncodedFrame>> {
    let env_dim = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
    // ten_bit + SDR: an 8-bit capture at depth 10. want_rgb=false forces the compute CSC (the
    // 709 10-bit shader), the path a composited-cursor desktop always takes.
    let mut enc = match VulkanVideoEncoder::open_opts_depth(
        codec, w, h, 60, 10_000_000, false, true, false,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("run_smoke_10bit_sdr({codec:?}): open declined — {e:#}");
            return None;
        }
    };
    assert!(enc.spec.ten_bit, "a 10-bit session must report 10-bit");
    let colors = [
        [40u8, 40, 200, 255],
        [40, 200, 40, 255],
        [200, 40, 40, 255],
        [200, 200, 40, 255],
        [40, 200, 200, 255],
        [200, 40, 200, 255],
        [120, 200, 80, 255],
        [80, 120, 200, 255],
    ];
    let mut aus: Vec<crate::EncodedFrame> = Vec::new();
    for (i, c) in colors.iter().enumerate() {
        enc.submit_indexed(&cpu_frame(w, h, i as u64 * 16_666_667, *c), i as u32)
            .expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        aus.push(au);
    }
    assert_eq!(aus.len(), colors.len(), "one AU per submitted frame");
    assert!(aus[0].keyframe, "frame 0 must be IDR");
    Some(aus)
}

/// HEVC Main10 SDR through the compute CSC (BT.709 at ten bits).
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device with a 10-bit profile"]
fn vulkan_smoke_10bit_sdr() {
    if let Some(aus) = run_smoke_10bit_sdr(Codec::H265) {
        dump_smoke(&aus, "10bit.sdr.h265");
    }
}

/// AV1 10-bit SDR — the AMD/Intel path VAAPI cannot serve.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_av1 device with a 10-bit profile"]
fn vulkan_smoke_10bit_sdr_av1() {
    if let Some(aus) = run_smoke_10bit_sdr(Codec::Av1) {
        dump_smoke(&aus, "10bit.sdr.obu");
    }
}

/// 24-bpp CPU session via `normalize_cpu_rgb`. Frames alternate Rgb/Bgr (staging re-key).
/// A cursor-blend session leaves the CSC for EFC once no cursor reaches it and returns on
/// the first visible one; every frame comes back, with an IDR at each move.
#[test]
#[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
fn vulkan_encode_cursor_switch_follows_the_cursor() {
    let (w, h) = (256, 256);
    let mut enc = VulkanVideoEncoder::open(
        Codec::Av1,
        PixelFormat::Bgrx,
        w,
        h,
        60,
        10_000_000,
        true,
        8,
        false,
        true,
        &Knobs::from_env(),
    )
    .expect("open");
    assert!(
        enc.rgb.is_none() && enc.cursor_switch,
        "a blend session opens on the CSC"
    );
    let cursor = |mut f: CapturedFrame| {
        f.cursor = Some(pf_frame::CursorOverlay {
            x: 10,
            y: 10,
            w: 8,
            h: 8,
            rgba: std::sync::Arc::new(vec![255; 8 * 8 * 4]),
            serial: 1,
            hot_x: 0,
            hot_y: 0,
            visible: true,
        });
        f
    };
    let mut aus = Vec::new();
    let drain = |enc: &mut VulkanVideoEncoder, aus: &mut Vec<crate::EncodedFrame>| {
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    };
    enc.submit(&cpu_frame(w, h, 0, [40, 40, 200, 255])).unwrap();
    enc.cursorless_since = Some(std::time::Instant::now() - super::EFC_AFTER_CURSORLESS);
    enc.submit(&cpu_frame(w, h, 1, [40, 200, 40, 255])).unwrap();
    if !enc.cursor_switch {
        eprintln!("cursor switch: RGB-direct unavailable on this driver — skipping");
        return;
    }
    assert!(
        enc.rgb.is_some(),
        "no cursor for the hysteresis window: EFC"
    );
    enc.submit(&cpu_frame(w, h, 2, [200, 40, 40, 255])).unwrap();
    drain(&mut enc, &mut aus);
    enc.submit(&cursor(cpu_frame(w, h, 3, [200, 200, 40, 255])))
        .unwrap();
    assert!(
        enc.rgb.is_none(),
        "a visible cursor moves the session to the CSC"
    );
    enc.submit(&cursor(cpu_frame(w, h, 4, [40, 200, 200, 255])))
        .unwrap();
    enc.flush().unwrap();
    drain(&mut enc, &mut aus);
    let kf: Vec<bool> = aus.iter().map(|a| a.keyframe).collect();
    assert_eq!(
        kf,
        [true, true, false, true, false],
        "an IDR opens each session"
    );
}

fn run_smoke_cpu24(rgb_direct: bool) -> Option<Vec<crate::EncodedFrame>> {
    let env_dim = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    let (w, h) = (env_dim("PF_SMOKE_W", 256), env_dim("PF_SMOKE_H", 256));
    let mut enc =
        VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, rgb_direct).expect("open");
    if rgb_direct && enc.rgb.is_none() {
        eprintln!("run_smoke_cpu24: RGB-direct unavailable on this driver — skipping");
        return None;
    }
    let colors: [[u8; 3]; 4] = [[200, 40, 40], [40, 200, 40], [40, 40, 200], [200, 200, 40]];
    let mut aus: Vec<crate::EncodedFrame> = Vec::new();
    for i in 0..8usize {
        let fmt = if i % 2 == 0 {
            PixelFormat::Rgb
        } else {
            PixelFormat::Bgr
        };
        let frame = cpu_frame_24(w, h, i as u64 * 16_666_667, colors[i / 2], fmt);
        enc.submit_indexed(&frame, i as u32).expect("submit 24-bpp");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        aus.push(au);
    }
    assert_eq!(aus.len(), 8, "one AU per 24-bpp frame");
    assert!(aus[0].keyframe, "frame 0 must be IDR");
    Some(aus)
}

/// 24-bpp CPU frames through the CSC path.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
fn vulkan_smoke_cpu_rgb24() {
    let aus = run_smoke_cpu24(false).expect("CSC mode never soft-skips");
    if let Ok(home) = std::env::var("HOME") {
        let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
        let p = format!("{home}/vkenc-host-smoke-cpu24.h265");
        let _ = std::fs::write(&p, &full);
        eprintln!("vulkan_smoke_cpu_rgb24: wrote {p} ({} bytes)", full.len());
    }
}

/// 24-bpp RGB-direct twin: expanded RGBA/BGRA is the encode source. Soft-skips without VALVE.
#[test]
#[ignore = "needs VK_VALVE_video_encode_rgb_conversion (RADV >= Mesa 26.0 on EFC hardware)"]
fn vulkan_smoke_rgb_cpu24() {
    if let Some(aus) = run_smoke_cpu24(true) {
        if let Ok(home) = std::env::var("HOME") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let p = format!("{home}/vkenc-host-smoke-rgb-cpu24.h265");
            let _ = std::fs::write(&p, &full);
            eprintln!("vulkan_smoke_rgb_cpu24: wrote {p} ({} bytes)", full.len());
        }
    }
}

/// CSC refuses a source that doesn't match the session mode (clamped texelFetch would
/// silently crop/pad). Pins refusal in both directions, and that a refused submit does not
/// wedge the session (bail is after frame-type bookkeeping).
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device; meaningful only under validation layers"]
fn vulkan_csc_refuses_a_mismatched_source() {
    let mut enc =
        VulkanVideoEncoder::open_opts(Codec::H265, 512, 512, 60, 10_000_000, false).expect("open");
    enc.submit_indexed(&cpu_frame(512, 512, 0, [40, 40, 200, 255]), 0)
        .expect("well-sized baseline");
    while enc.poll().expect("poll").is_some() {}
    // Guard is equality on render size, not a ceiling.
    let e = enc
        .submit_indexed(&cpu_frame(128, 128, 16_666_667, [200, 40, 40, 255]), 1)
        .expect_err("smaller source must refuse");
    assert!(e.to_string().contains("mismatched"), "{e:#}");
    let e = enc
        .submit_indexed(&cpu_frame(640, 640, 33_333_334, [200, 40, 40, 255]), 2)
        .expect_err("larger source must refuse");
    assert!(e.to_string().contains("mismatched"), "{e:#}");
    let mut got_au = false;
    for i in 3..11u64 {
        enc.submit_indexed(
            &cpu_frame(512, 512, i * 16_666_667, [40, 200, 40, 255]),
            i as u32,
        )
        .expect("well-sized after refusal");
        while let Ok(Some(_)) = enc.poll() {
            got_au = true;
        }
    }
    assert!(got_au, "no AU after the refused submits — session wedged");
    eprintln!("done — under validation layers this run must report ZERO VUID errors");
}

/// A joiner's reframe on an EFC-capable device: the session reopens on the CSC, and the
/// decoded picture is the crop's red and white halves with none of the blue beside it or
/// the green above and below.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device and ffmpeg"]
fn vulkan_reframe_crops_and_scales() {
    let (w, h) = (256u32, 144u32);
    let (sw, sh) = (768u32, 432u32);
    let mut enc =
        VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, true).expect("open");
    assert!(enc.caps().crops_input && enc.caps().downscales_input);
    enc.set_input_crop([128, 72, 512, 288]).expect("reframe");
    assert!(enc.rgb.is_none(), "the reframe runs on the CSC path");
    // BGRX: green rows outside the crop, blue columns beside it, red then white inside.
    let mut px = vec![0u8; (sw * sh * 4) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let c: [u8; 4] = if !(72..360).contains(&y) {
                [40, 200, 40, 255]
            } else if !(128..640).contains(&x) {
                [200, 40, 40, 255]
            } else if x < 384 {
                [40, 40, 200, 255]
            } else {
                [235, 235, 235, 255]
            };
            let i = ((y * sw + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    let mut stream = Vec::new();
    for i in 0..4u64 {
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: sw,
            height: sh,
            pts_ns: i * 16_666_667,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(px.clone()),
            cursor: None,
        };
        enc.submit_indexed(&frame, i as u32).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        stream.extend_from_slice(&au.data);
    }
    let path = std::env::temp_dir().join("vkenc-reframe.h265");
    std::fs::write(&path, &stream).expect("write the stream");
    let Ok(out) = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
    else {
        eprintln!("no ffmpeg — skipping the picture check");
        return;
    };
    assert_eq!(
        out.stdout.len(),
        (w * h * 3) as usize,
        "decoded size: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let at = |x: u32, y: u32| {
        let i = ((y * w + x) * 3) as usize;
        [out.stdout[i], out.stdout[i + 1], out.stdout[i + 2]]
    };
    let near = |got: [u8; 3], want: [u8; 3], what: &str| {
        assert!(
            got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 40),
            "{what}: got {got:?}, want {want:?}"
        );
    };
    near(at(3, h / 2), [200, 40, 40], "left edge is the crop's red");
    near(
        at(w - 4, h / 2),
        [235, 235, 235],
        "right edge is the crop's white",
    );
    near(at(w / 4, 2), [200, 40, 40], "top edge has no green");
    near(
        at(w * 3 / 4, h - 3),
        [235, 235, 235],
        "bottom edge has no green",
    );
}

/// 1080 rows sit off VCN4's 64×16 AV1 grid. A stream whose header disagrees with the coded
/// size desyncs in the bottom superblock row, so that row must match the source as well as
/// the top one does, on the CSC and on EFC.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_av1 device and ffmpeg"]
fn vulkan_av1_1080_bottom_row_decodes_clean() {
    let (w, h, frames) = (1920u32, 1080u32, 12u32);
    for rgb in [false, true] {
        let mut enc =
            VulkanVideoEncoder::open_opts(Codec::Av1, w, h, 60, 20_000_000, rgb).expect("open");
        if rgb && enc.rgb.is_none() {
            eprintln!("RGB-direct unavailable on this driver — CSC only");
            continue;
        }
        let mut stream = Vec::new();
        for i in 0..frames {
            let frame = cpu_frame_scroll(w, h, u64::from(i) * 16_666_667, i);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
        let path = std::env::temp_dir().join("vkenc-1080.obu");
        std::fs::write(&path, &stream).expect("write the stream");
        let Ok(out) = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "obu", "-i"])
            .arg(&path)
            .args(["-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
        else {
            eprintln!("no ffmpeg — skipping the picture check");
            return;
        };
        // ffmpeg keeps the coded rows (render_size becomes the aspect ratio).
        let rows = out.stdout.len() / (frames * w * 3) as usize;
        assert!(
            rows >= h as usize,
            "decoded {} bytes: {}",
            out.stdout.len(),
            String::from_utf8_lossy(&out.stderr)
        );
        let last = &out.stdout[(frames - 1) as usize * rows * w as usize * 3..];
        let src = crate::smoke_pattern::scroll_pattern(w as usize, h as usize, frames as usize - 1);
        let mae = |y0: u32, y1: u32| {
            let mut sum = 0u64;
            for y in y0..y1 {
                for x in 0..w {
                    let (d, s) = (((y * w + x) * 3) as usize, ((y * w + x) * 4) as usize);
                    for (c, k) in [(0, 2), (1, 1), (2, 0)] {
                        sum += u64::from(last[d + c].abs_diff(src[s + k]));
                    }
                }
            }
            sum as f64 / f64::from((y1 - y0) * w * 3)
        };
        let (top, bottom) = (mae(0, 64), mae(h - 56, h));
        eprintln!("rgb={rgb}: rows={rows} top MAE {top:.1}, bottom MAE {bottom:.1}");
        assert!(
            bottom <= 2.0 * top + 8.0,
            "rgb={rgb}: the bottom superblock row decodes as garbage \
             (MAE {bottom:.1} vs {top:.1} at the top)"
        );
    }
}

/// White decodes to the session's range on both converters: Y' 255 full, 235 studio. A blend
/// session takes the CSC; the other takes EFC where the probe passes.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device and ffmpeg"]
fn vulkan_white_lands_at_the_session_range() {
    let (w, h) = (256u32, 256u32);
    for (full_range, blend) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut enc = VulkanVideoEncoder::open(
            Codec::H265,
            PixelFormat::Bgrx,
            w,
            h,
            60,
            10_000_000,
            blend,
            8,
            false,
            full_range,
            &Knobs::from_env(),
        )
        .expect("open");
        let efc = enc.rgb.is_some();
        let mut stream = Vec::new();
        for i in 0..2u32 {
            let frame = cpu_frame(w, h, u64::from(i) * 16_666_667, [255, 255, 255, 255]);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
        let path = std::env::temp_dir().join("vkenc-range.h265");
        std::fs::write(&path, &stream).expect("write the stream");
        // Same pixel format out as in: ffmpeg copies the codes, no range conversion.
        let Ok(out) = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args([
                "-frames:v",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuv420p",
                "-",
            ])
            .output()
        else {
            eprintln!("no ffmpeg — skipping the picture check");
            return;
        };
        assert_eq!(out.stdout.len(), (w * h * 3 / 2) as usize);
        let y = out.stdout[(h / 2 * w + w / 2) as usize];
        let want = if full_range { 255 } else { 235 };
        assert!(
            y.abs_diff(want) <= 3,
            "full={full_range} efc={efc}: white Y' {y}, want {want}"
        );
    }
}

/// Mid-stream [`Encoder::reset`] must not change what `vkCmdBeginVideoCodingKHR` declares.
/// `reset()` re-arms `first_frame` without rebuilding the session, so CBR is still current
/// — declaration keys on `rc_installed`. Also stages `reconfigure_bitrate` before the reset
/// (pending rate survives; begin must still declare the old rate — VUID-...-08254).
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device; meaningful only under validation layers"]
fn vulkan_reset_keeps_the_declared_rate_control_state() {
    let (w, h) = (256u32, 256u32);
    let mut enc =
        VulkanVideoEncoder::open_opts(Codec::H265, w, h, 60, 10_000_000, false).expect("open");
    eprintln!("phase 1: 4 frames (installs CBR on frame 0)");
    for i in 0..4u64 {
        enc.submit_indexed(
            &cpu_frame(w, h, i * 16_666_667, [40, 40, 200, 255]),
            i as u32,
        )
        .expect("submit");
        while enc.poll().expect("poll").is_some() {}
    }
    eprintln!("phase 2: reconfigure_bitrate() then reset() — the 08254 coincidence");
    // Pending rate must survive reset; next begin must still declare the OLD rate.
    assert!(enc.reconfigure_bitrate(20_000_000), "retarget should stage");
    assert!(enc.reset(), "reset should succeed");
    eprintln!("phase 3: 4 more frames — first one re-declares the OLD rate, installs the NEW");
    for i in 4..8u64 {
        enc.submit_indexed(
            &cpu_frame(w, h, i * 16_666_667, [200, 40, 40, 255]),
            i as u32,
        )
        .expect("submit after reset");
        while enc.poll().expect("poll").is_some() {}
    }
    eprintln!("done — under validation layers this run must report ZERO VUID errors");
}

/// A stall warns once a second; the window reports its maxima, then starts over.
#[test]
fn phase_stats_warn_once_a_second_then_report_the_window() {
    use std::time::{Duration, Instant};
    let mut p = PhaseStats::default();
    let t = Instant::now();
    assert!(
        !p.note(PHASE_WAIT, Duration::from_millis(8), t),
        "a healthy phase never warns"
    );
    assert!(p.note(PHASE_WAIT, PHASE_SPIKE, t), "the first stall warns");
    assert!(
        !p.note(
            PHASE_IMPORT,
            Duration::from_millis(400),
            t + Duration::from_millis(500)
        ),
        "a stall inside the same second is folded in silently"
    );
    assert!(
        p.note(PHASE_WAIT, PHASE_SPIKE, t + PHASE_SPIKE_EVERY),
        "the next second warns again"
    );
    assert!(
        p.report(t + Duration::from_secs(1)).is_none(),
        "no report before the window ends"
    );
    let w = p
        .report(t + PHASE_REPORT_EVERY)
        .expect("the window reports");
    assert_eq!(
        (w.max_us[PHASE_WAIT], w.max_us[PHASE_IMPORT]),
        (100_000, 400_000)
    );
    assert_eq!(p.max_us, [0; 4], "the next window starts empty");
}

/// Hardware: a LINEAR packed-RGB dmabuf whose row is off any driver's pitch alignment goes
/// through the compute CSC unsheared. NVIDIA reads it as an image at a pitch rounded up to 32
/// bytes; there the CSC reads the buffer's words.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device, a dma-buf exporting driver and ffmpeg"]
fn vulkan_csc_reads_a_linear_source_at_its_stride() {
    let buf = LinearDmabuf::new(&edge_frame(ODD_W, ODD_H, ODD_W * 4));
    let mut enc = VulkanVideoEncoder::open_opts(Codec::H265, ODD_W, ODD_H, 60, 40_000_000, false)
        .expect("open");
    let mut stream = Vec::new();
    for i in 0..3 {
        let frame = buf.frame(PixelFormat::Bgrx, b"XR24", ODD_W, ODD_H, ODD_W * 4, i);
        enc.submit_indexed(&frame, i).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        stream.extend_from_slice(&au.data);
    }
    assert_edge_is_straight(&stream, "vulkan-csc-linear.h265", ODD_W, ODD_H);
}

/// Hardware: pipelined, a slot's fence rings the loop's AU wake and rings alone bring back
/// every AU still on the GPU; a sync submit never rings; a drop with fences armed joins the
/// waiter first. Under the validation layer this is the check that the waiter and the encode
/// thread never hold a fence together.
#[test]
#[ignore = "needs a real VK_KHR_video_encode_h265 device (run on the RADV host, not the build box)"]
fn vulkan_pipelined_fence_rings_the_au_wake() {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    const FRAMES: u32 = 240;
    const W: u32 = 1920;
    const H: u32 = 1080;
    fn drain(enc: &mut VulkanVideoEncoder) -> u32 {
        let mut aus = 0;
        while enc.poll().expect("poll").is_some() {
            aus += 1;
        }
        aus
    }
    let period = Duration::from_secs(1) / 120;
    let frame = |i: u32| cpu_frame_scroll(W, H, u64::from(i) * 8_333_333, i);
    let mut enc =
        VulkanVideoEncoder::open_opts(Codec::H265, W, H, 120, 20_000_000, false).expect("open");
    let (ring, rung) = mpsc::channel();
    enc.set_au_wake(Some(std::sync::Arc::new(move || {
        let _ = ring.send(());
    })));

    for i in 0..3 {
        enc.submit_indexed(&frame(i), i).expect("sync submit");
        assert_eq!(drain(&mut enc), 1, "sync: the AU follows its submit");
    }
    assert!(rung.try_recv().is_err(), "a sync submit rang the wake");

    assert!(enc.set_pipelined(true), "pipelined retrieve refused");
    let (mut aus, mut rings, mut by_ring) = (0u32, 0u32, 0u32);
    for i in 3..3 + FRAMES {
        let next = frame(i);
        let tick = Instant::now();
        enc.submit_indexed(&next, i).expect("pipelined submit");
        // The loop polls after its submit, then waits out the tick: a ring ends that early.
        aus += drain(&mut enc);
        while rung
            .recv_timeout(period.saturating_sub(tick.elapsed()))
            .is_ok()
        {
            rings += 1;
            let got = drain(&mut enc);
            by_ring += got;
            aus += got;
        }
    }
    // Nothing submits any more, so only a ring can bring an AU back.
    while aus < FRAMES {
        rung.recv_timeout(Duration::from_secs(2))
            .expect("a ring for an AU still on the GPU");
        rings += 1;
        let got = drain(&mut enc);
        by_ring += got;
        aus += got;
    }
    println!("vulkan pipelined wake: {FRAMES} AUs, {rings} rings, {by_ring} AUs taken on a ring");
    assert_eq!(aus, FRAMES, "one AU per pipelined frame");
    assert!(rings <= FRAMES, "{rings} rings for {FRAMES} fences");

    // Back to sync: nothing arms a fence, so nothing rings.
    assert!(!enc.set_pipelined(false));
    std::thread::sleep(Duration::from_millis(100));
    while rung.try_recv().is_ok() {}
    for i in 3 + FRAMES..6 + FRAMES {
        enc.submit_indexed(&frame(i), i).expect("sync submit");
        assert_eq!(drain(&mut enc), 1, "sync: the AU follows its submit");
    }
    assert!(
        rung.recv_timeout(Duration::from_millis(100)).is_err(),
        "a sync submit rang the wake"
    );

    assert!(enc.set_pipelined(true));
    for i in 6 + FRAMES..8 + FRAMES {
        enc.submit_indexed(&frame(i), i).expect("pipelined submit");
    }
    drop(enc);
}
