use super::*;
use crate::pyrowave_ffi::oracle;
use crate::pyrowave_wire::unwindow;
use crate::test_frames::{cpu_frame, cpu_frame_24};
use pf_frame::PixelFormat;

#[test]
fn in_flight_frame_owns_the_raw_source_hold() {
    let hold: pf_frame::FrameHold = std::sync::Arc::new(());
    let frame = InFlight {
        slot: 0,
        pts_ns: 0,
        cap: 1,
        seq: 0,
        wire_chunk: None,
        t0: std::time::Instant::now(),
        cpu_ns: [0; 4],
        _src_hold: Some(hold.clone()),
        src_key: None,
    };
    assert_eq!(std::sync::Arc::strong_count(&hold), 2);
    drop(frame);
    assert_eq!(std::sync::Arc::strong_count(&hold), 1);
}

/// BT.709 full-range YCbCr of an 8-bit RGB fill — same math as `rgb2yuv.comp`.
fn bt709(fill: [u8; 4]) -> (f64, f64, f64) {
    let (b, g, r) = (fill[0] as f64, fill[1] as f64, fill[2] as f64); // BGRA
    (
        0.2126 * r + 0.7152 * g + 0.0722 * b,
        128.0 - 0.1146 * r - 0.3854 * g + 0.5000 * b,
        128.0 + 0.5000 * r - 0.4542 * g - 0.0458 * b,
    )
}

/// 10-bit full-range codes of an 8-bit BGRA fill, folded into the decoder's 8-bit
/// output domain (`code10 * 255/1024`). `hdr` picks the shader's matrix: BT.2020 NCL
/// (`rgb2yuv*10.comp`) or BT.709 (`rgb2yuv*10_709.comp`).
fn code10_as_u8(fill: [u8; 4], hdr: bool) -> (f64, f64, f64) {
    let (b, g, r) = (
        fill[0] as f64 / 255.0,
        fill[1] as f64 / 255.0,
        fill[2] as f64 / 255.0,
    );
    let s = 255.0 / 1024.0;
    if hdr {
        (
            (268.7421 * r + 693.5940 * g + 60.6639 * b) * s,
            (512.0 - 142.8415 * r - 368.6584 * g + 511.5 * b) * s,
            (512.0 + 511.5 * r - 470.3603 * g - 41.1392 * b) * s,
        )
    } else {
        (
            (217.4898 * r + 731.6496 * g + 73.8606 * b) * s,
            (512.0 - 117.2073 * r - 394.2928 * g + 511.5 * b) * s,
            (512.0 + 511.5 * r - 464.5984 * g - 46.9016 * b) * s,
        )
    }
}

/// Decode an AU with a standalone pyrowave decoder to planar YUV. Oracle for smoke
/// plane-means and the Apple Metal PSNR fixtures (`pyrowave_dump_golden`).
unsafe fn decode_planes_chroma(
    w: u32,
    h: u32,
    au: &[u8],
    chroma444: bool,
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    // SAFETY: same contract as the caller.
    unsafe { oracle::decode_planes(w, h, &[au], chroma444) }.remove(0)
}

unsafe fn decode_planes(w: u32, h: u32, au: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    // SAFETY: same contract as the caller.
    unsafe { decode_planes_chroma(w, h, au, false) }
}

unsafe fn decode_plane_means(w: u32, h: u32, au: &[u8], chroma444: bool) -> (f64, f64, f64) {
    // SAFETY: same contract as the caller.
    oracle::plane_means(&unsafe { decode_planes_chroma(w, h, au, chroma444) })
}

/// Open → CSC → GPU encode → packetize, then CPU-decode each AU and check plane
/// means against the CSC's BT.709 math. Needs a Vulkan 1.3 GPU.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_smoke() {
    let (w, h) = (256u32, 256u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        40_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    assert!(!enc.caps().supports_rfi);

    let colors = [
        [40u8, 40, 200, 255],
        [40, 200, 40, 255],
        [200, 40, 40, 255],
        [128, 128, 128, 255],
    ];
    for (i, c) in colors.iter().enumerate() {
        enc.submit(&cpu_frame(w, h, i as u64 * 16_666_667, *c))
            .expect("submit");
        let au = enc.poll().expect("poll").expect("one AU per frame");
        assert!(au.keyframe, "every pyrowave AU is a keyframe");
        assert!(!au.data.is_empty());
        assert!(
            au.data.len() <= enc.budget.bytes + BS_SLACK,
            "AU exceeds rate budget"
        );
        // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
        let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, false) };
        let (ye, cbe, cre) = bt709(*c);
        assert!(
            (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
            "frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) vs \
             expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
        );
    }

    // Datagram-aligned: AU is a whole number of framed windows. Walk + reassemble
    // must reproduce a decodable packet stream.
    enc.set_wire_chunking(1408);
    enc.submit(&cpu_frame(w, h, 500, [90, 60, 30, 255]))
        .expect("chunked submit");
    let au = enc.poll().expect("poll").expect("chunked AU");
    assert!(au.chunk_aligned);
    let stream = unwindow(&au.data, 1408).expect("well-formed windows");
    assert!(!stream.is_empty(), "chunked AU carries real packets");
    // SAFETY: test-only FFI with locally-owned buffers.
    unsafe { decode_planes(w, h, &stream) };
    enc.set_wire_chunking(0); // below the floor — ignored, stays chunked
    assert!(enc.reconfigure_bitrate(100_000_000));
    assert!(enc.reset());
    enc.submit(&cpu_frame(w, h, 999, [10, 20, 30, 255]))
        .expect("submit after reset");
    assert!(enc.poll().expect("poll").is_some());
}

/// 24-bpp CPU payloads expand 3→4, not refuse. Channel order is load-bearing: a
/// swapped R/B or misplaced pad moves chroma means by tens of codes.
/// A real encoder's windowed AU leads with its level-4 head: the sealer finds it short of the
/// whole AU, and the reordered windows still decode.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_au_leads_with_its_head() {
    let (w, h) = (1920u32, 1080u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        200_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    enc.set_wire_chunking(1408);
    enc.submit(&test_card(w, h, 3)).expect("submit");
    let au = enc.poll().expect("poll").expect("AU");
    assert!(au.chunk_aligned);
    let windows = au.data.len() / 1408;
    let head = punktfunk_core::pyrowave::head_shards(&au.data, 1408)
        .expect("window 0 opens with the sequence header");
    assert!(
        head > 0 && head < windows / 2,
        "{head} of {windows} windows"
    );
    let stream = unwindow(&au.data, 1408).expect("well-formed windows");
    // SAFETY: test-only FFI with locally-owned buffers.
    unsafe { decode_planes(w, h, &stream) };
}

#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_smoke_cpu_rgb24() {
    let (w, h) = (256u32, 256u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        40_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    let colors: [[u8; 3]; 3] = [[200, 40, 40], [40, 200, 40], [40, 40, 200]];
    for fmt in [PixelFormat::Rgb, PixelFormat::Bgr] {
        for (i, c) in colors.iter().enumerate() {
            enc.submit(&cpu_frame_24(w, h, i as u64 * 16_666_667, *c, fmt))
                .expect("submit 24-bpp");
            let au = enc.poll().expect("poll").expect("one AU per frame");
            // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
            let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, false) };
            let (ye, cbe, cre) = bt709([c[2], c[1], c[0], 255]);
            assert!(
                (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
                "{fmt:?} frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) \
                 vs expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
            );
        }
    }
}

/// Each slot owns only conversion images and cursor staging. The 512 MiB ceiling
/// catches a bitstream or import cache becoming per-slot at 4K.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn slot_vram_cost_stays_bounded() {
    for (w, h, chroma, name) in [
        (1920u32, 1080u32, crate::ChromaFormat::Yuv420, "1080p 4:2:0"),
        (3840, 2160, crate::ChromaFormat::Yuv420, "4K 4:2:0"),
        (3840, 2160, crate::ChromaFormat::Yuv444, "4K 4:4:4"),
    ] {
        let enc = PyroWaveEncoder::open(w, h, 60, 40_000_000, chroma, 8, false, &Knobs::from_env())
            .expect("open");
        // SAFETY: plain memory-requirement queries on images this encoder owns.
        let per_slot: u64 = unsafe {
            [
                enc.slots[0].y.image,
                enc.slots[0].uv.image,
                enc.slots[0].cursor.img.image,
            ]
            .iter()
            .map(|&i| enc.device.get_image_memory_requirements(i).size)
            .sum::<u64>()
                + enc
                    .device
                    .get_buffer_memory_requirements(enc.slots[0].cursor.stage.buffer)
                    .size
        };
        assert!(per_slot > 0, "{name}: a slot must own real memory");
        assert!(
            per_slot < 512 * 1024 * 1024,
            "{name}: {per_slot} bytes per slot exceeds the 512 MiB ceiling"
        );
    }
}

/// A frame that is not the session mode must be refused. PyroWave applies no
/// alignment; without this check CSC clamps and CPU uploads `min(len, need)`, so it
/// smears. Refusal is before record, so a correctly-sized frame after still encodes.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_refuses_a_frame_that_is_not_the_mode() {
    let (w, h) = (256u32, 256u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        40_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    for (fw, fh) in [(w - 2, h), (w, h - 2), (w + 2, h + 2)] {
        let err = enc
            .submit(&cpu_frame(fw, fh, 0, [200, 40, 40, 255]))
            .expect_err("a frame that is not the mode must be refused");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("session mode"),
            "the refusal must name the mismatch, got: {msg}"
        );
    }
    // Refusal must enqueue nothing. Check before the good frame: `pending` is a queue.
    assert!(
        enc.poll().expect("poll").is_none(),
        "a refused frame must not enqueue an AU"
    );
    enc.submit(&cpu_frame(w, h, 16_666_667, [40, 200, 40, 255]))
        .expect("a correctly-sized frame after a refusal must still encode");
    assert!(
        enc.poll().expect("poll").is_some(),
        "the session must survive the refusals"
    );
    assert!(
        enc.poll().expect("poll").is_none(),
        "exactly one AU for one accepted frame"
    );
}

/// A reframe halves a 512×288 crop of a 768×432 capture into 256×144. The decode is the crop's
/// red and white halves, with none of the blue beside it or the green above and below, and the
/// pointer, blended at source size, lands moved and shrunk with the crop.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_reframe_crops_and_scales() {
    let (w, h) = (256u32, 144u32);
    let (sw, sh) = (768u32, 432u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        40_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    assert!(enc.caps().crops_input && enc.caps().downscales_input);
    enc.set_input_crop([128, 72, 512, 288]).expect("reframe");
    let (red, white) = ([40u8, 40, 200, 255], [235, 235, 235, 255]);
    let (green, blue) = ([40u8, 200, 40, 255], [200, 40, 40, 255]);
    // BGRX: green rows outside the crop, blue columns beside it, red then white inside.
    let mut px = vec![0u8; (sw * sh * 4) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let c = if !(72..360).contains(&y) {
                green
            } else if !(128..640).contains(&x) {
                blue
            } else if x < 384 {
                red
            } else {
                white
            };
            let i = ((y * sw + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    // A 64² green pointer at source (160, 200): the crop puts it at (16, 64), 32² in size.
    let cursor = pf_frame::CursorOverlay {
        x: 160,
        y: 200,
        w: 64,
        h: 64,
        rgba: std::sync::Arc::new([40u8, 200, 40, 255].repeat(64 * 64)),
        serial: 1,
        hot_x: 0,
        hot_y: 0,
        visible: true,
    };
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: sw,
        height: sh,
        pts_ns: 0,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Cpu(px),
        cursor: Some(cursor),
    };
    enc.submit(&frame).expect("submit");
    let au = enc.poll().expect("poll").expect("one AU");
    // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
    let (luma, cb, cr) = unsafe { decode_planes(w, h, &au.data) };
    let near = |(x, y): (u32, u32), want: [u8; 4], what: &str| {
        let c = ((y / 2) * (w / 2) + x / 2) as usize;
        let got = [luma[(y * w + x) as usize], cb[c], cr[c]].map(f64::from);
        let (ye, cbe, cre) = bt709(want);
        assert!(
            got.iter()
                .zip([ye, cbe, cre])
                .all(|(g, e)| (g - e).abs() < 24.0),
            "{what} at ({x}, {y}): YCbCr {got:?}, want ({ye:.0}, {cbe:.0}, {cre:.0})"
        );
    };
    near((3, h / 2), red, "left edge is the crop's red");
    near((w - 4, h / 2), white, "right edge is the crop's white");
    near((w / 4, 2), red, "top edge has no green");
    near((w * 3 / 4, h - 3), white, "bottom edge has no green");
    near((32, 80), green, "the pointer, moved and halved");
    near((32, 110), red, "below the halved pointer");
    near((96, 116), red, "where an unmoved pointer would land");
}

/// A failed dmabuf import must leak neither the dup'd fd nor the VkImage. Drives
/// `import_rgb_dmabuf` directly so deliberate failures cannot trip the raw-dmabuf latch.
/// A garbage modifier is refused before `create_image`; LINEAR memfd fails at `allocate_memory`.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn import_failure_leaks_no_fds() {
    let enc = PyroWaveEncoder::open(
        64,
        64,
        60,
        5_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    let memfd_frame = |modifier: u64| {
        let fd = rustix::fs::memfd_create(c"pf-import-leak", rustix::fs::MemfdFlags::empty())
            .expect("memfd_create");
        // Real pages, for an mmap-happy driver.
        rustix::fs::ftruncate(&fd, 64 * 64 * 4).expect("size the memfd");
        pf_frame::DmabufFrame {
            fd,
            fourcc: 0x3432_5258, // XR24 — maps, so failure lands past the fourcc gate
            modifier,
            plane1: None,
            offset: 0,
            stride: 64 * 4,
            hold: None,
            health: pf_zerocopy::zero_copy_health(modifier),
            rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    };
    let import = |d: &pf_frame::DmabufFrame| {
        // SAFETY: live instance/device/ext_fd/mem_props owned by `enc`; the frame is locally owned.
        unsafe {
            import_rgb_dmabuf(
                &enc.instance,
                enc.pd,
                &enc.device,
                &enc.ext_fd,
                &enc.mem_props,
                d,
                64,
                64,
            )
        }
    };
    let fd_count = || std::fs::read_dir("/proc/self/fd").expect("procfs").count();
    // Warm lazily-opened driver/loader descriptors before the baseline.
    for modifier in [u64::MAX - 1, 0] {
        let _ = import(&memfd_frame(modifier));
    }
    let baseline = fd_count();
    for i in 0..32 {
        let modifier = if i % 2 == 0 { u64::MAX - 1 } else { 0 };
        let r = import(&memfd_frame(modifier));
        assert!(
            r.is_err(),
            "a memfd/garbage-modifier import must fail (iteration {i})"
        );
    }
    assert_eq!(
        fd_count(),
        baseline,
        "fd count drifted across 32 failed imports — the unwind leaks"
    );
}

/// 4:4:4 must stay within budget, decode, and be run-to-run deterministic
/// (`patches/0001-payload-data-444-sizing.patch`).
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_smoke_444() {
    let (w, h) = (256u32, 256u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        40_000_000,
        crate::ChromaFormat::Yuv444,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    let colors = [
        [40u8, 40, 200, 255],
        [40, 200, 40, 255],
        [200, 40, 40, 255],
        [128, 128, 128, 255],
    ];
    for (i, c) in colors.iter().enumerate() {
        enc.submit(&cpu_frame(w, h, i as u64 * 16_666_667, *c))
            .expect("submit");
        let au = enc.poll().expect("poll").expect("one AU per frame");
        assert!(au.keyframe);
        assert!(
            au.data.len() <= enc.budget.bytes + BS_SLACK,
            "AU exceeds rate budget"
        );
        // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
        let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, true) };
        let (ye, cbe, cre) = bt709(*c);
        assert!(
            (ym - ye).abs() < 3.0 && (cbm - cbe).abs() < 3.0 && (crm - cre).abs() < 3.0,
            "frame {i}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) vs \
             expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
        );
    }

    // Busy content at ~2.6 bpp — the regime that overran 4:2:0-sized payload staging.
    let budget_bps = w as u64 * h as u64 * 60 * 26 / 10;
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        budget_bps,
        crate::ChromaFormat::Yuv444,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    let mut sizes = Vec::new();
    for _ in 0..3 {
        enc.submit(&test_card(w, h, 7)).expect("busy submit");
        let au = enc.poll().expect("poll").expect("busy AU");
        assert!(
            au.data.len() <= enc.budget.bytes + BS_SLACK,
            "busy 4:4:4 AU exceeds rate budget ({} > {})",
            au.data.len(),
            enc.budget.bytes + BS_SLACK
        );
        // SAFETY: test-only FFI with locally-owned buffers.
        let _ = unsafe { decode_planes_chroma(w, h, &au.data, true) };
        sizes.push(au.data.len());
    }
    assert!(
        sizes.windows(2).all(|s| s[0] == s[1]),
        "identical input produced varying AU sizes (the Phase-0 overrun signature): {sizes:?}"
    );
}

/// The four 10-bit mode combinations: SDR and BT.2020 PQ, each at 4:2:0 and 4:4:4.
/// The AU's colour byte is the honest stamp (`stamp_color_bits`), and the decode
/// means must match the shader's own matrix — a wrong shader picks the wrong matrix.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_smoke_10bit() {
    let (w, h) = (256u32, 256u32);
    for (chroma, hdr, label) in [
        (crate::ChromaFormat::Yuv420, false, "10-bit SDR 4:2:0"),
        (crate::ChromaFormat::Yuv444, false, "10-bit SDR 4:4:4"),
        (crate::ChromaFormat::Yuv420, true, "BT.2020 PQ 4:2:0"),
        (crate::ChromaFormat::Yuv444, true, "BT.2020 PQ 4:4:4"),
    ] {
        let mut enc =
            PyroWaveEncoder::open(w, h, 60, 40_000_000, chroma, 10, hdr, &Knobs::from_env())
                .unwrap_or_else(|e| panic!("{label}: open: {e:#}"));
        let fill = [40u8, 40, 200, 255];
        enc.submit(&cpu_frame(w, h, 0, fill))
            .unwrap_or_else(|e| panic!("{label}: submit: {e:#}"));
        let au = enc
            .poll()
            .unwrap_or_else(|e| panic!("{label}: poll: {e:#}"))
            .unwrap_or_else(|| panic!("{label}: no AU"));
        assert!(au.keyframe, "{label}: AU is not a keyframe");
        // The sequence header's colour byte: FULL + LEFT always, the BT.2020
        // primaries/PQ/matrix bits only on an HDR session (pyrowave_wire.rs).
        let stamped = au.data[7];
        assert_eq!(
            stamped & 0xC0,
            0x80,
            "{label}: colour byte {stamped:#04x} misses FULL+LEFT"
        );
        assert_eq!(
            stamped & 0x38,
            if hdr { 0x38 } else { 0 },
            "{label}: colour byte {stamped:#04x} — BT.2020 bits mismatch the session"
        );
        // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
        let (ym, cbm, crm) = unsafe { decode_plane_means(w, h, &au.data, chroma.is_444()) };
        let (ye, cbe, cre) = code10_as_u8(fill, hdr);
        assert!(
            (ym - ye).abs() < 4.0 && (cbm - cbe).abs() < 4.0 && (crm - cre).abs() < 4.0,
            "{label}: decoded plane means (Y {ym:.1}, Cb {cbm:.1}, Cr {crm:.1}) vs \
             expected (Y {ye:.1}, Cb {cbe:.1}, Cr {cre:.1})"
        );
    }
}

/// Deterministic busy BGRA card (gradients + checker + LCG). Flat fills miss the
/// entropy decoder; this hits every subband.
fn test_card(w: u32, h: u32, seed: u32) -> CapturedFrame {
    let mut rng = seed | 1;
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
            let i = ((y * w + x) * 4) as usize;
            let checker = if (x / 16 + y / 16) % 2 == 0 { 48 } else { 0 };
            let noise = (rng >> 24) as u8 / 8;
            buf[i] = ((x * 255 / w) as u8).saturating_add(noise);
            buf[i + 1] = ((y * 255 / h) as u8).saturating_add(checker);
            buf[i + 2] = (((x + y) * 255 / (w + h)) as u8).saturating_add(noise);
            buf[i + 3] = 255;
        }
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: seed as u64 * 16_666_667,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}

/// Dump Apple Metal golden fixtures: host-encoded AUs (dense and chunk-aligned)
/// plus upstream decode as YUV420P. Float wavelet math is not bit-exact; the Swift
/// test PSNR-matches Metal against these. Set `PYROWAVE_GOLDEN_DIR` and copy into
/// `clients/apple/Tests/PunktfunkKitTests/PyroWaveFixtures/`.
#[test]
#[ignore = "fixture generator — needs a real Vulkan 1.3 compute device"]
fn pyrowave_dump_golden() {
    let dir = match std::env::var("PYROWAVE_GOLDEN_DIR") {
        Ok(d) => std::path::PathBuf::from(d),
        Err(_) => {
            eprintln!("PYROWAVE_GOLDEN_DIR not set — skipping dump");
            return;
        }
    };
    std::fs::create_dir_all(&dir).expect("create golden dir");

    // Odd-block geometry: 256 aligns clean, 144 → aligned 160 exercises overhang. ~1.6 bpp.
    let (w, h) = (256u32, 144u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        4_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");

    let dump = |name: &str, bytes: &[u8]| {
        std::fs::write(dir.join(name), bytes).expect("write fixture");
        eprintln!("wrote {name}: {} bytes", bytes.len());
    };

    enc.submit(&test_card(w, h, 7)).expect("submit");
    let au = enc.poll().expect("poll").expect("AU");
    assert!(!au.chunk_aligned);
    dump("au-dense.bin", &au.data);
    // SAFETY: test-only FFI with locally-owned buffers.
    let (y, cb, cr) = unsafe { decode_planes(w, h, &au.data) };
    dump("ref-dense-y.bin", &y);
    dump("ref-dense-cb.bin", &cb);
    dump("ref-dense-cr.bin", &cr);

    // Different frame: Swift window walk + FRAG reassembly must reproduce the stream.
    enc.set_wire_chunking(1408);
    enc.submit(&test_card(w, h, 11)).expect("chunked submit");
    let au = enc.poll().expect("poll").expect("chunked AU");
    assert!(au.chunk_aligned);
    assert_eq!(au.data.len() % 1408, 0);
    dump("au-chunked.bin", &au.data);
    let stream = unwindow(&au.data, 1408).expect("well-formed windows");
    // SAFETY: test-only FFI with locally-owned buffers.
    let (y, cb, cr) = unsafe { decode_planes(w, h, &stream) };
    dump("ref-chunked-y.bin", &y);
    dump("ref-chunked-cb.bin", &cb);
    dump("ref-chunked-cr.bin", &cr);

    // 4:4:4 dense AU + full-res chroma reference. Same odd-block geometry.
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        6_500_000,
        crate::ChromaFormat::Yuv444,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    enc.submit(&test_card(w, h, 13)).expect("444 submit");
    let au = enc.poll().expect("poll").expect("444 AU");
    assert!(!au.chunk_aligned);
    dump("au-dense444.bin", &au.data);
    // SAFETY: test-only FFI with locally-owned buffers.
    let (y, cb, cr) = unsafe { decode_planes_chroma(w, h, &au.data, true) };
    dump("ref-dense444-y.bin", &y);
    dump("ref-dense444-cb.bin", &cb);
    dump("ref-dense444-cr.bin", &cr);
}

/// Input for the client's `pyrowave_bench` example: a `PUNKTFUNK_DUMP_VIDEO`-shaped
/// capture at `PYROWAVE_BENCH_OUT`, shaped by `PYROWAVE_BENCH_MODE=WxH:fps:mbps:bits`
/// (default `3840x2160:120:1200:10`). `PF_WAVE_NOISE=1` fills the rate budget the way
/// game content does.
#[test]
#[ignore = "fixture generator — needs a real Vulkan 1.3 compute device"]
fn pyrowave_dump_bench_capture() {
    let Ok(out) = std::env::var("PYROWAVE_BENCH_OUT") else {
        eprintln!("PYROWAVE_BENCH_OUT not set — skipping dump");
        return;
    };
    let mode =
        std::env::var("PYROWAVE_BENCH_MODE").unwrap_or_else(|_| "3840x2160:120:1200:10".into());
    let f: Vec<u64> = mode
        .split([':', 'x'])
        .map(|v| v.parse().expect("PYROWAVE_BENCH_MODE=WxH:fps:mbps:bits"))
        .collect();
    let (w, h, bits) = (f[0] as u32, f[1] as u32, f[4] as u8);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        f[2] as u32,
        f[3] * 1_000_000,
        crate::ChromaFormat::Yuv420,
        bits,
        bits >= 10,
        &Knobs::from_env(),
    )
    .expect("open");
    enc.set_wire_chunking(1408);
    // The index a client dump writes: `offset len flags complete`, flags = chunk-aligned.
    let (mut data, mut idx) = (Vec::new(), String::new());
    for i in 0..24usize {
        let mut frame = cpu_frame(w, h, i as u64 * 8_333_333, [0; 4]);
        frame.payload = FramePayload::Cpu(crate::smoke_pattern::scroll_pattern(
            w as usize, h as usize, i,
        ));
        enc.submit(&frame).expect("submit");
        let au = enc.poll().expect("poll").expect("AU").data;
        idx.push_str(&format!("{} {} 0x40 1\n", data.len(), au.len()));
        data.extend_from_slice(&au);
    }
    eprintln!("{mode}: 24 AUs, mean {} bytes", data.len() / 24);
    std::fs::write(&out, &data).expect("write capture");
    std::fs::write(format!("{out}.idx"), idx).expect("write index");
}

/// The open plan spells its extension names itself; a typo enables an extension no driver has.
#[test]
fn the_open_plan_spells_extensions_as_ash_does() {
    use crate::pyrowave_open as open;
    for (ours, theirs) in [
        (open::EXT_MEMORY_FD, ash::khr::external_memory_fd::NAME),
        (open::EXT_DMA_BUF, ash::ext::external_memory_dma_buf::NAME),
        (
            open::EXT_DRM_MODIFIER,
            ash::ext::image_drm_format_modifier::NAME,
        ),
        (open::EXT_FOREIGN, ash::ext::queue_family_foreign::NAME),
        (open::EXT_PRIORITY_KHR, vk::KHR_GLOBAL_PRIORITY_NAME),
        (open::EXT_PRIORITY_EXT, vk::EXT_GLOBAL_PRIORITY_NAME),
        (
            open::EXT_CALIBRATED_KHR,
            ash::khr::calibrated_timestamps::NAME,
        ),
        (
            open::EXT_CALIBRATED_EXT,
            ash::ext::calibrated_timestamps::NAME,
        ),
    ] {
        assert_eq!(ours, theirs);
    }
}

/// Luma PSNR (dB) of decoded Y against BT.709 full-range luma of source BGRA. Luma
/// only: chroma is subsampled on 4:2:0, and luma is where wavelet quantisation shows.
fn luma_psnr(src_bgra: &[u8], decoded_y: &[u8]) -> f64 {
    assert_eq!(src_bgra.len(), decoded_y.len() * 4);
    let mut sse = 0.0f64;
    for (px, &got) in src_bgra.chunks_exact(4).zip(decoded_y) {
        let (b, g, r) = (px[0] as f64, px[1] as f64, px[2] as f64);
        let want = 0.2126 * r + 0.7152 * g + 0.0722 * b;
        let d = want - got as f64;
        sse += d * d;
    }
    let mse = sse / decoded_y.len() as f64;
    if mse <= f64::EPSILON {
        return f64::INFINITY;
    }
    10.0 * (255.0f64 * 255.0 / mse).log10()
}

/// With `PUNKTFUNK_PYROWAVE_STREAMED_AU=1`, a busy-card GPU encode must come out of
/// `poll_chunk` in several window-aligned pieces that concatenate to a decodable AU.
/// Flat fills reassemble even with missing subbands; the busy card puts energy in
/// every subband so a lost window collapses PSNR.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn pyrowave_streamed_chunks_reassemble_and_keep_the_picture() {
    const WINDOW: usize = 1408;
    // 1280×720 at 60 Mb/s ≈ 125 KB/AU — several chunks, many windows.
    let (w, h) = (1280u32, 720u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        200_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open pyrowave encoder");
    enc.set_wire_chunking(WINDOW);

    assert!(
        enc.supports_chunked_poll(),
        "PUNKTFUNK_PYROWAVE_STREAMED_AU=1 must be set in the ENVIRONMENT of this test binary \
         — without it PW6 is off by design and there is nothing to verify"
    );

    for seed in [7u32, 11, 13] {
        let frame = test_card(w, h, seed);
        let FramePayload::Cpu(ref src) = frame.payload else {
            panic!("test card is a CPU frame")
        };
        let src = src.clone();
        enc.submit(&frame).expect("submit");

        let mut au = Vec::new();
        let (mut chunks, mut firsts, mut lasts) = (0u32, 0u32, 0u32);
        loop {
            let c = enc
                .poll_chunk()
                .expect("poll_chunk")
                .expect("an AU is in flight");
            assert!(c.chunk_aligned, "wire chunking is on");
            assert!(c.keyframe, "every pyrowave AU is a keyframe");
            assert_eq!(
                c.data.len() % WINDOW,
                0,
                "every chunk is a whole number of windows — a cut inside a window would \
                 split the 4-byte framing prefix from its body"
            );
            chunks += 1;
            firsts += u32::from(c.first);
            lasts += u32::from(c.last);
            au.extend_from_slice(&c.data);
            if c.last {
                break;
            }
        }
        assert_eq!(firsts, 1, "exactly one opening chunk");
        assert_eq!(lasts, 1, "exactly one closing chunk");
        assert!(
            chunks > 1,
            "seed {seed}: the AU came out in ONE piece ({} B) — the cut never engaged, so \
             this run proves nothing about PW6",
            au.len()
        );
        assert_eq!(au.len() % WINDOW, 0, "the AU is a whole number of windows");

        assert!(
            enc.poll_chunk().expect("poll_chunk after last").is_none(),
            "no AU is in flight once `last` was handed out"
        );

        let stream = unwindow(&au, WINDOW).expect("well-formed windows");
        // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
        let (y, _cb, _cr) = unsafe { decode_planes(w, h, &stream) };
        let psnr = luma_psnr(&src, &y);
        eprintln!(
            "seed {seed}: {chunks} chunks, {} B AU ({} windows), luma PSNR {psnr:.2} dB",
            au.len(),
            au.len() / WINDOW
        );
        assert!(
            psnr > 30.0,
            "seed {seed}: luma PSNR {psnr:.2} dB — the streamed reassembly lost or reordered \
             picture data (a flat-fill test would NOT have caught this)"
        );
    }
}

/// Two `pyrowave_encoder` objects each keep a 3-bit `sequence_count`, so alternating
/// them emits 1,1,2,2… The decoder restarts only when the value changes, so a repeat
/// is swallowed. Patch 0007 stamps one counter. Over 20 frames (past the wrap at 8):
/// wire +1 mod 8 per AU; one persistent decoder reports ready every AU; consecutive
/// pictures differ (`test_card` reseeded; flat fills hide a swallowed frame).
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn wire_sequence_increments_across_alternating_handles() {
    const FRAMES: u32 = 20;
    let (w, h) = (256u32, 256u32);
    let mut enc = PyroWaveEncoder::open(
        w,
        h,
        60,
        40_000_000,
        crate::ChromaFormat::Yuv420,
        8,
        false,
        &Knobs::from_env(),
    )
    .expect("open");
    const { assert!(SLOTS >= 2) };

    let mut aus: Vec<Vec<u8>> = Vec::new();
    for i in 0..FRAMES {
        // Content moves every frame. Odd seeds only: `test_card` starts its LCG at
        // `seed | 1`, so 2 and 3 produce a byte-identical card.
        enc.submit(&test_card(w, h, 2 * i + 1)).expect("submit");
        let au = enc.poll().expect("poll").expect("one AU per frame");
        aus.push(au.data);
    }

    let seqs: Vec<u8> = aus
        .iter()
        .map(|au| crate::pyrowave_wire::wire_sequence(au, 0).expect("AU carries a block header"))
        .collect();
    for (i, pair) in seqs.windows(2).enumerate() {
        assert_eq!(
            pair[1],
            (pair[0] + 1) & 7,
            "frame {} -> {}: wire sequence went {} -> {} (all: {seqs:?}). Two encoder handles \
             each counting alone produce repeats, which the decoder reads as more blocks of \
             the same frame — check that patch 0007 is applied and set_next_sequence is called",
            i,
            i + 1,
            pair[0],
            pair[1]
        );
    }

    // One decoder for the whole run: a fresh decoder per AU resets `last_seq`. A frame
    // that never becomes decodable is still being accumulated into the previous one.
    // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
    let lumas = unsafe { decode_stream_luma(w, h, &aus) };
    for (i, pair) in lumas.windows(2).enumerate() {
        assert_ne!(
            pair[0],
            pair[1],
            "frame {} decoded to the SAME picture as frame {i} — a swallowed frame",
            i + 1
        );
    }
}

/// Decode a whole AU stream through one decoder (`last_seq` is not reset per frame)
/// and return each frame's luma plane.
///
/// # Safety
/// Test-only FFI into the vendored decoder with locally-owned buffers.
unsafe fn decode_stream_luma(w: u32, h: u32, aus: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let aus: Vec<&[u8]> = aus.iter().map(Vec::as_slice).collect();
    // SAFETY: same contract as the caller.
    let planes = unsafe { oracle::decode_planes(w, h, &aus, false) };
    planes.into_iter().map(|(y, _, _)| y).collect()
}

/// PSNR (dB) between two equal-sized 8-bit planes; `f64::INFINITY` when identical.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let mse = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum::<f64>()
        / a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// Two frames in flight at once must match the synchronous depth-1 pictures, in
/// order. Ground truth is this encoder's own depth-1 decode: raw AU bytes are not
/// run-to-run reproducible. Flat fills hide a torn frame. Does not cover capture:
/// `.process` requeues the SPA buffer while encode still holds a dup'd fd. Sets
/// `max_inflight` directly; the shipped value stays 1.
#[test]
#[ignore = "needs a real Vulkan 1.3 compute device (run on a GPU host, not the build box)"]
fn overlapping_two_frames_reproduces_the_synchronous_picture() {
    const FRAMES: u32 = 16;
    let (w, h) = (256u32, 256u32);
    // Odd seeds: `test_card` starts its LCG at `seed | 1`, so 2 and 3 build the same card.
    let cards: Vec<CapturedFrame> = (0..FRAMES).map(|i| test_card(w, h, 2 * i + 1)).collect();
    let open = || {
        PyroWaveEncoder::open(
            w,
            h,
            60,
            40_000_000,
            crate::ChromaFormat::Yuv420,
            8,
            false,
            &Knobs::from_env(),
        )
        .expect("open")
    };

    let mut enc = open();
    let sync: Vec<Vec<u8>> = cards
        .iter()
        .map(|c| {
            enc.submit(c).expect("sync submit");
            assert_eq!(
                enc.inflight.len(),
                1,
                "submit must leave exactly one in flight"
            );
            enc.poll()
                .expect("sync poll")
                .expect("one AU per frame")
                .data
        })
        .collect();
    drop(enc);

    let mut enc = open();
    enc.max_inflight = SLOTS;
    let mut overlapped: Vec<Vec<u8>> = Vec::new();
    let mut saw_two_in_flight = false;
    for c in &cards {
        enc.submit(c).expect("overlapped submit");
        saw_two_in_flight |= enc.inflight.len() == 2;
        if enc.inflight.len() >= SLOTS {
            overlapped.push(
                enc.poll()
                    .expect("overlapped poll")
                    .expect("an AU once the pipeline is full")
                    .data,
            );
        }
    }
    enc.flush().expect("flush drains the tail");
    while let Some(au) = enc.poll().expect("tail poll") {
        overlapped.push(au.data);
    }
    drop(enc);
    assert!(
        saw_two_in_flight,
        "two frames were never actually in flight — this test proved nothing"
    );
    assert_eq!(
        overlapped.len(),
        sync.len(),
        "the overlapped run emitted a different number of AUs — a frame was lost"
    );

    // SAFETY: test-only FFI into the vendored decoder with locally-owned buffers.
    let (sy, oy) = unsafe {
        (
            decode_stream_luma(w, h, &sync),
            decode_stream_luma(w, h, &overlapped),
        )
    };
    let mut worst = f64::INFINITY;
    for i in 0..sy.len() {
        let p = psnr(&sy[i], &oy[i]);
        worst = worst.min(p);
        // 45 dB is far above "looks the same"; a torn frame from two moving cards
        // lands in the teens. Wavelet RDO is not bit-reproducible.
        assert!(
            p > 45.0,
            "frame {i}: overlapped decode is {p:.1} dB from the synchronous one — the pipelined \
             path changed the picture"
        );
        // A frame one position off still scores well against a neighbour. Must match its own.
        if i > 0 {
            let prev = psnr(&sy[i - 1], &oy[i]);
            assert!(
                p > prev,
                "frame {i} matches the PREVIOUS reference better ({prev:.1} dB) than its own \
                 ({p:.1} dB) — the pipeline is off by one"
            );
        }
    }
    eprintln!(
        "depth-2 vs depth-1 over {} frames: worst-case PSNR {}",
        sy.len(),
        if worst.is_infinite() {
            "identical (inf)".to_string()
        } else {
            format!("{worst:.1} dB")
        }
    );
}
