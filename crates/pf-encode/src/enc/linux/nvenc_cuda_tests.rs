use super::*;
use crate::max_forced_split_mode;
use crate::nvenc_core::{cached_split_verdict, slice_offsets_len, BitstreamLock};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use pf_zerocopy::cuda::{DeviceBuffer, PlaneLayout};

#[test]
fn raw_source_hold_is_cloned_for_the_pending_encode() {
    let hold: pf_frame::FrameHold = std::sync::Arc::new(());
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: 64,
        height: 64,
        pts_ns: 0,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Dmabuf(pf_frame::DmabufFrame {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            fourcc: u32::from_le_bytes(*b"XR24"),
            modifier: 0,
            plane1: None,
            offset: 0,
            stride: 256,
            hold: Some(hold.clone()),
            health: pf_zerocopy::zero_copy_health(0x5001),
            rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        cursor: None,
    };
    let pending = source_hold(&frame).expect("raw frame hold");
    assert_eq!(std::sync::Arc::strong_count(&hold), 3);
    drop(pending);
    assert_eq!(std::sync::Arc::strong_count(&hold), 2);
}

/// Env helper for ignored hardware tests. Run `--test-threads=1` — they mutate process env.
fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: `--test-threads=1` hardware tests only — no concurrent env access.
    unsafe { std::env::set_var(key, val) };
}

/// Same single-threaded contract as [`set_env`].
fn remove_env(key: &str) {
    // SAFETY: as `set_env` — single-threaded, no concurrent env access.
    unsafe { std::env::remove_var(key) };
}

/// SDR-10 rides NVENC's 8→10, which takes packed RGB (`ARGB`) only. A planar 8-bit surface
/// (NV12/YUV444) stays 8-bit — feeding it a 10-bit session fails `register_resource`, the
/// real-capture regression this guards. Packed 10-bit input is HDR (BT.2020 PQ) regardless.
#[test]
fn depth_and_hdr_needs_packed_rgb_for_ten_bit() {
    use nv::NV_ENC_BUFFER_FORMAT as F;
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ARGB, 10, false),
        (10, false)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ARGB, 8, false),
        (8, false)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_NV12, 10, false),
        (8, false)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_YUV444, 10, false),
        (8, false)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ARGB10, 8, true),
        (10, true)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ABGR10, 10, true),
        (10, true)
    );
    // A 10-bit input carries the session's colour; an 8-bit stream is never HDR.
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ABGR10, 10, false),
        (10, false)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_YUV420_10BIT, 10, false),
        (10, false)
    );
    assert_eq!(
        depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_NV12, 10, true),
        (8, false)
    );
}

#[test]
fn ten_bit_rgb_maps_to_the_matching_nvenc_format_and_blend_mode() {
    use nv::NV_ENC_BUFFER_FORMAT as F;
    // `x:R:G:B` = ARGB10; `x:B:G:R` = ABGR10.
    assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB10));
    assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ABGR10));
    assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB));
    assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_NV12));
    assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_YUV444));
    // Blend mode must unpack this channel order; a swap tints the pointer.
    assert_eq!(
        slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ARGB10),
        SlotFormat::X2Rgb10
    );
    assert_eq!(
        slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ABGR10),
        SlotFormat::X2Bgr10
    );
    assert_eq!(slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ARGB), SlotFormat::Argb);
}

/// What `resolve_split_mode` actually reads (`query_caps` latch).
fn self_engines(enc: &NvencCudaEncoder) -> u32 {
    enc.s.encoder_engines
}

/// NV12 with real entropy. Driver-zeroed VRAM under CBR emits ~300 B/AU against an 833 KB
/// quota, so timings measure only pixel-proportional cost. `block=1` is incompressible
/// (RC overshoots); larger `block` is the only way to reach the low bits/frame end.
fn noise_nv12_frame(w: u32, h: u32, i: u32, block: usize) -> CapturedFrame {
    let buf = DeviceBuffer::alloc(PlaneLayout::Nv12, w, h).expect("alloc NV12 device buffer");
    let (uv_ptr, uv_pitch) = buf.uv().expect("NV12 buffer has a UV plane");
    let mut st = 0x2545_F491_4F6C_DD1Du64 ^ ((i as u64 + 1) << 32);
    let mut next = move || {
        st ^= st << 13;
        st ^= st >> 7;
        st ^= st << 17;
        st
    };
    let b = block.max(1);
    let mut plane = |pw: usize, ph: usize| -> Vec<u8> {
        let bw = pw.div_ceil(b);
        let cells: Vec<u8> = (0..(bw * ph.div_ceil(b)))
            .map(|_| (next() >> 24) as u8)
            .collect();
        let mut out = Vec::with_capacity(pw * ph);
        for y in 0..ph {
            let row = y / b * bw;
            for x in 0..pw {
                out.push(cells[row + x / b]);
            }
        }
        out
    };
    let y = plane(w as usize, h as usize);
    let uv = plane(w as usize, h as usize / 2);
    // SAFETY: `buf` is the live `w`×`h` NV12 allocation above, UV at `uv_pitch`; the caller
    // made the shared context current.
    unsafe {
        pf_zerocopy::cuda::write_plane_from_host(buf.ptr, buf.pitch, &y, w as usize, h as usize)
            .expect("upload Y plane");
        pf_zerocopy::cuda::write_plane_from_host(uv_ptr, uv_pitch, &uv, w as usize, h as usize / 2)
            .expect("upload UV plane");
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: i as u64 * 16_666_667,
        format: PixelFormat::Nv12,
        payload: FramePayload::Cuda(buf),
        cursor: None,
    }
}

fn nv12_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
    // Uninit VRAM: session/RFI machinery, not picture fidelity.
    let buf = DeviceBuffer::alloc(PlaneLayout::Nv12, w, h).expect("alloc NV12 device buffer");
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: i as u64 * 16_666_667,
        format: PixelFormat::Nv12,
        payload: FramePayload::Cuda(buf),
        cursor: None,
    }
}

/// Hardware: GUID probe. Every NVENC encodes H.264 — `h264 = false` means enumeration is
/// broken. Asserted on the uncached fn (the cache would make stability vacuous).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on an NVIDIA box"]
fn nvenc_codec_probe_reports_real_gpu_support() {
    let probed = probe_support_uncached();
    let caps = probed.codecs;
    eprintln!(
        "NVENC probe: h264={} h265={} av1={} hevc_444={}",
        caps.h264, caps.h265, caps.av1, probed.hevc_444
    );
    assert!(
        caps.h264,
        "every NVENC generation encodes H.264 — a false here means the GUID enumeration \
         failed, which would narrow the host's codec advertisement"
    );
    assert!(
        !probed.hevc_444 || caps.h265,
        "a 4:4:4-capable HEVC that is not in the GUID list is contradictory"
    );
    let again = probe_support_uncached();
    assert_eq!(
        (caps.h264, caps.h265, caps.av1, probed.hevc_444),
        (
            again.codecs.h264,
            again.codecs.h265,
            again.codecs.av1,
            again.hevc_444
        ),
        "the probe must be stable — it is cached once and drives every later negotiation"
    );
}

#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_smoke_rfi_anchor() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");

    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    // Warm-up: 8 frames, wire indices 0..7.
    let mut aus = 0usize;
    let mut first_key = false;
    for i in 0..8u32 {
        let frame = nv12_frame(W, H, i);
        enc.submit_indexed(&frame, i).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            if aus == 0 {
                first_key = au.keyframe;
            }
            aus += 1;
        }
    }
    assert!(aus > 0, "no AUs produced");
    assert!(
        first_key,
        "first AU must be a keyframe (session opening IDR)"
    );
    assert!(enc.caps().supports_rfi, "RTX NVENC must advertise RFI");

    // In-DPB range (RFI_DPB=5 ⇒ 3..=7 live). Must be real RFI, not an IDR fallback.
    assert!(
        enc.invalidate_ref_frames(5, 6),
        "invalidate_ref_frames should succeed for an in-DPB range"
    );

    // Re-anchor AU: `recovery_anchor`, not a forced IDR.
    let frame = nv12_frame(W, H, 8);
    enc.submit_indexed(&frame, 8).expect("submit post-RFI");
    let mut saw_anchor = false;
    let mut anchor_was_keyframe = false;
    while let Some(au) = enc.poll().expect("poll") {
        if au.recovery_anchor {
            saw_anchor = true;
            anchor_was_keyframe = au.keyframe;
        }
    }
    assert!(
        saw_anchor,
        "the post-RFI AU must carry recovery_anchor (the F2 fix)"
    );
    assert!(
        !anchor_was_keyframe,
        "RFI re-anchor must be a P-frame, not an IDR"
    );
    enc.flush().ok();
    println!("nvenc_cuda smoke: {aus} AUs, RFI succeeded, recovery-anchor tagged on the P-frame");
}

/// The wave soak ([`crate::smoke_pattern::WaveSoak`], its `PF_WAVE_*` knobs) on the CUDA
/// session, frames uploaded from host memory; 10-bit feeds XBGR2101010, the Windows soak's
/// R10G10B10A2 bytes.
///
/// `cargo test -p pf-encode --features nvenc --release nvenc_cuda_wave_soak -- --ignored --nocapture`
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on an NVIDIA Linux box"]
fn nvenc_cuda_wave_soak() {
    let soak = crate::smoke_pattern::WaveSoak::from_env();
    let (w, h, fps, ten_bit) = (soak.w, soak.h, soak.fps, soak.ten_bit);
    let format = if ten_bit {
        PixelFormat::X2Bgr10
    } else {
        PixelFormat::Bgra
    };
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        soak.codec,
        format,
        w,
        h,
        fps,
        soak.mbps * 1_000_000,
        true,
        if ten_bit { 10 } else { 8 },
        format.is_ten_bit(),
        ChromaFormat::Yuv420,
        false,
        1,
    )
    .expect("open NVENC CUDA session");
    soak.run(
        "nvenc-cuda",
        &mut enc,
        |e| &e.s,
        |i| {
            let (w, h) = (w as usize, h as usize);
            let px = if ten_bit {
                crate::smoke_pattern::scroll_pattern_rgb10(w, h, i)
            } else {
                crate::smoke_pattern::scroll_pattern(w, h, i)
            };
            CapturedFrame {
                provenance: Default::default(),
                width: w as u32,
                height: h as u32,
                pts_ns: i as u64 * 1_000_000_000 / u64::from(fps),
                format,
                payload: FramePayload::Cpu(px),
                cursor: None,
            }
        },
    );
}

/// Packed `X2Rgb10` (NVENC `ARGB10`, no host CSC). Uninit VRAM: session machinery, not
/// picture fidelity.
fn rgb10_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
    let buf =
        DeviceBuffer::alloc(PlaneLayout::Packed32, w, h).expect("alloc packed RGB device buffer");
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: i as u64 * 16_666_667,
        format: PixelFormat::X2Rgb10,
        payload: FramePayload::Cuda(buf),
        cursor: None,
    }
}

/// Hardware: packed 10-bit → `ARGB10`. Depth follows the input and the session's HDR
/// verdict rides on it — that pair selects Main10 / BT.2020 PQ.
#[test]
#[ignore = "requires an NVIDIA GPU + driver with 10-bit encode"]
fn nvenc_cuda_hdr10_packed_rgb() {
    for codec in [Codec::H265, Codec::Av1] {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            codec,
            PixelFormat::X2Rgb10,
            W,
            H,
            60,
            20_000_000,
            true,
            10,
            true,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let mut aus = 0usize;
        let mut first_key = false;
        let mut stream: Vec<u8> = Vec::new();
        for i in 0..4u32 {
            enc.submit_indexed(&rgb10_frame(W, H, i), i)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                if aus == 0 {
                    first_key = au.keyframe;
                }
                assert!(!au.data.is_empty(), "empty AU");
                stream.extend_from_slice(&au.data);
                aus += 1;
            }
        }
        enc.flush().ok();
        // Dump for out-of-band ffprobe. In-tree we only see the encoder's own config.
        if let Ok(home) = std::env::var("HOME") {
            let ext = if codec == Codec::Av1 { "obu" } else { "h265" };
            let path = format!("{home}/nvenc-hdr10.{ext}");
            if std::fs::write(&path, &stream).is_ok() {
                println!(
                    "nvenc_cuda HDR10 {codec:?}: wrote {path} ({} bytes)",
                    stream.len()
                );
            }
        }
        assert!(aus > 0, "{codec:?}: no AUs produced");
        assert!(first_key, "{codec:?}: first AU must be the session IDR");
        // Depth + HDR came from the input format.
        assert_eq!(enc.s.bit_depth, 10, "{codec:?}: must have derived 10-bit");
        assert!(
            enc.s.hdr,
            "{codec:?}: must have derived HDR from the PQ format"
        );
        assert_eq!(
            enc.s.buffer_fmt,
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10,
            "{codec:?}: X2Rgb10 must ingest as ARGB10"
        );
        println!("nvenc_cuda HDR10 {codec:?}: {aus} AUs, ARGB10 in, 10-bit derived");
    }
}

#[test]
fn a_shrunk_cursor_keeps_its_colour_at_a_soft_edge() {
    // 4x2: an opaque red half and a transparent half, halved to 2x1.
    let mut px = Vec::new();
    for x in 0..4 {
        px.extend(if x < 2 {
            [255, 0, 0, 255]
        } else {
            [0, 0, 0, 0]
        });
    }
    let row: Vec<u8> = px.iter().chain(px.iter()).copied().collect();
    let out = shrink_rgba(&row, 4, 2, 2, 1);
    assert_eq!(out, vec![255, 0, 0, 255, 0, 0, 0, 0]);
    // A half-covered block keeps full red at half alpha, not a darkened red.
    let out = shrink_rgba(&row, 4, 2, 1, 1);
    assert_eq!(out, vec![255, 0, 0, 127]);
}

/// Hardware: a reframed session encodes the crop at half size. The decoded picture's edges
/// hold the crop's own colours, none of the frame around it.
#[test]
#[ignore = "requires an NVIDIA GPU + driver and ffmpeg — run on the RTX box (.21)"]
fn nvenc_cuda_reframe_crops_and_scales() {
    const SW: u32 = 768;
    const SH: u32 = 432;
    const W: u32 = 256;
    const H: u32 = 144;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Bgrx,
        W,
        H,
        60,
        8_000_000,
        false,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");
    assert!(enc.caps().crops_input && enc.caps().downscales_input);
    enc.set_input_crop([128, 72, 512, 288]).expect("reframe");
    // BGRX: green rows outside the crop, blue columns beside it, red then white inside.
    let mut px = vec![0u8; (SW * SH * 4) as usize];
    for y in 0..SH {
        for x in 0..SW {
            let c: [u8; 4] = if !(72..360).contains(&y) {
                [40, 200, 40, 255]
            } else if !(128..640).contains(&x) {
                [200, 40, 40, 255]
            } else if x < 384 {
                [40, 40, 200, 255]
            } else {
                [235, 235, 235, 255]
            };
            let i = ((y * SW + x) * 4) as usize;
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    let mut stream = Vec::new();
    for i in 0..4u32 {
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: SW,
            height: SH,
            pts_ns: u64::from(i) * 16_666_667,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(px.clone()),
            cursor: None,
        };
        enc.submit_indexed(&frame, i).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        stream.extend_from_slice(&au.data);
    }
    assert_eq!(
        (enc.s.width, enc.s.height),
        (W, H),
        "the session runs at the reframed size"
    );
    let path = std::env::temp_dir().join("nvenc-reframe.h265");
    std::fs::write(&path, &stream).expect("write the stream");
    let Ok(out) = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
    else {
        println!("no ffmpeg — skipping the picture check");
        return;
    };
    assert_eq!(
        out.stdout.len(),
        (W * H * 3) as usize,
        "decoded size: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let at = |x: u32, y: u32| {
        let i = ((y * W + x) * 3) as usize;
        [out.stdout[i], out.stdout[i + 1], out.stdout[i + 2]]
    };
    let near = |got: [u8; 3], want: [u8; 3], what: &str| {
        assert!(
            got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 40),
            "{what}: got {got:?}, want {want:?}"
        );
    };
    near(at(3, H / 2), [200, 40, 40], "left edge is the crop's red");
    near(
        at(W - 4, H / 2),
        [235, 235, 235],
        "right edge is the crop's white",
    );
    near(at(W / 4, 2), [200, 40, 40], "top edge has no green");
    near(
        at(W * 3 / 4, H - 3),
        [235, 235, 235],
        "bottom edge has no green",
    );
}

/// Hardware: cursor blend on a 10-bit packed slot. An 8-bit fallback would tint the
/// pointer. Blend correctness is display-referred — not asserted here.
#[test]
#[ignore = "requires an NVIDIA GPU + driver with 10-bit encode"]
fn nvenc_cuda_hdr10_cursor_blend() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    if !stream_ordered_requested() || async_retrieve_requested() {
        println!("skipped: stream-ordered submit disabled by env");
        return;
    }
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::X2Rgb10,
        W,
        H,
        60,
        8_000_000,
        true,
        10,
        true,
        ChromaFormat::Yuv420,
        true, // Vulkan slot ring + 10-bit blend
        4,
    )
    .expect("open NVENC CUDA session");
    let cursor = |serial: u64, x: i32, y: i32| pf_frame::CursorOverlay {
        x,
        y,
        w: 32,
        h: 32,
        rgba: std::sync::Arc::new(vec![0xFF; 32 * 32 * 4]),
        serial,
        hot_x: 0,
        hot_y: 0,
        visible: true,
    };
    let mut aus = 0usize;
    for i in 0..6u32 {
        let mut frame = rgb10_frame(W, H, i);
        // Serial flip at frame 3 (upload quiesce); position moves every frame.
        frame.cursor = Some(cursor(
            if i < 3 { 1 } else { 2 },
            40 + i as i32 * 9,
            60 + i as i32 * 5,
        ));
        enc.submit_indexed(&frame, i).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            assert!(!au.data.is_empty(), "empty AU");
            aus += 1;
        }
    }
    enc.flush().ok();
    assert!(aus > 0, "no AUs produced");
    assert_eq!(enc.s.bit_depth, 10, "must be a 10-bit session");
    assert_eq!(
        slot_fmt_of(enc.s.buffer_fmt),
        SlotFormat::X2Rgb10,
        "the blend must target the 10-bit packed slot layout, not the 8-bit one"
    );
    assert!(
        enc.caps().blends_cursor,
        "the direct-SDK path must still report a cursor blend at 10-bit"
    );
    println!("nvenc_cuda HDR10 cursor blend: {aus} AUs, slot fmt X2Rgb10");
}

/// Hardware: a packed-RGB 8-bit capture under a 10-bit session encodes a 10-bit stream,
/// BT.709, no PQ (the `.obu`/`.h265` land in `PUNKTFUNK_SMOKE_DIR` for `ffprobe`: each must
/// read `yuv420p10le` bt709). The planar arms are the regression guard: real Linux capture
/// is NV12 (and 4:4:4 is planar YUV444), which NVENC refuses in a 10-bit session — the
/// encoder must degrade those to 8-bit rather than fail `register_resource`.
///
/// `cargo test -p pf-encode --features nvenc --lib nvenc_cuda_sdr10 -- --ignored --nocapture`
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_sdr10_from_eight_bit_capture() {
    const W: u32 = 1280;
    const H: u32 = 720;
    let dir = std::env::var("PUNKTFUNK_SMOKE_DIR").unwrap_or_else(|_| ".".into());
    let cpu_frame = |i: u32| CapturedFrame {
        provenance: Default::default(),
        width: W,
        height: H,
        pts_ns: u64::from(i) * 16_666_667,
        format: PixelFormat::Bgra,
        payload: FramePayload::Cpu(crate::smoke_pattern::scroll_pattern(
            W as usize, H as usize, i as usize,
        )),
        cursor: None,
    };
    for (codec, tag, ext) in [(Codec::Av1, "av1", "obu"), (Codec::H265, "hevc", "h265")] {
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            codec,
            PixelFormat::Bgra,
            W,
            H,
            60,
            40_000_000,
            true,
            10,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA SDR-10 session");
        let mut stream = Vec::new();
        for i in 0..12u32 {
            enc.submit_indexed(&cpu_frame(i), i).expect("submit SDR-10");
            while let Some(au) = enc.poll().expect("poll") {
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().ok();
        assert!(!stream.is_empty(), "{tag}: no AUs produced");
        assert_eq!(
            enc.s.bit_depth, 10,
            "{tag}: an 8-bit capture must still encode 10-bit"
        );
        assert!(
            !enc.s.hdr,
            "{tag}: 10-bit SDR must not claim HDR — that stamps a PQ VUI"
        );
        let path = format!("{dir}/nvenc-cuda-sdr10-{tag}.{ext}");
        std::fs::write(&path, &stream).expect("write");
        println!(
            "nvenc_cuda SDR-10 {tag}: {} bytes, depth={} hdr={} -> {path}",
            stream.len(),
            enc.s.bit_depth,
            enc.s.hdr
        );
    }

    // Regression guard: a planar 8-bit capture (real Linux default is NV12; 4:4:4 is
    // planar YUV444) under a 10-bit-negotiated session must degrade to 8-bit and encode,
    // never fail register_resource and end the video.
    for (label, fmt, chroma, layout) in [
        (
            "nv12",
            PixelFormat::Nv12,
            ChromaFormat::Yuv420,
            PlaneLayout::Nv12,
        ),
        (
            "yuv444",
            PixelFormat::Yuv444,
            ChromaFormat::Yuv444,
            PlaneLayout::Yuv444,
        ),
    ] {
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            fmt,
            W,
            H,
            60,
            40_000_000,
            true,
            10,
            fmt.is_ten_bit(),
            chroma,
            false,
            4,
        )
        .expect("open planar SDR-10 session");
        let mut aus = 0usize;
        for i in 0..4u32 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: u64::from(i) * 16_666_667,
                format: fmt,
                payload: FramePayload::Cuda(
                    DeviceBuffer::alloc(layout, W, H).expect("alloc planar device buffer"),
                ),
                cursor: None,
            };
            enc.submit_indexed(&frame, i)
                .unwrap_or_else(|e| panic!("{label}: planar submit must degrade, not fail: {e:#}"));
            while let Some(_au) = enc.poll().expect("poll") {
                aus += 1;
            }
        }
        enc.flush().ok();
        assert_eq!(
            enc.s.bit_depth, 8,
            "{label}: a planar 8-bit capture must degrade to 8-bit"
        );
        assert!(!enc.s.hdr, "{label}: SDR");
        assert!(aus > 0, "{label}: no AUs");
        println!("nvenc_cuda SDR-10 {label}: degraded to 8-bit, {aus} AUs (no crash)");
    }
}

/// Hardware: HEVC FREXT YUV444 (stacked-plane copy NV12 does not exercise).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_yuv444() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Yuv444,
        W,
        H,
        60,
        40_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv444,
        false,
        4,
    )
    .expect("open NVENC CUDA 4:4:4 session");

    let mut aus = 0usize;
    for i in 0..6u32 {
        let buf =
            DeviceBuffer::alloc(PlaneLayout::Yuv444, W, H).expect("alloc YUV444 device buffer");
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: W,
            height: H,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Yuv444,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        };
        enc.submit_indexed(&frame, i).expect("submit 444");
        while let Some(_au) = enc.poll().expect("poll") {
            aus += 1;
        }
    }
    assert!(aus > 0, "no 4:4:4 AUs produced");
    assert!(enc.caps().chroma_444, "RTX NVENC HEVC must report 4:4:4");
    println!("nvenc_cuda 4:4:4 smoke: {aus} AUs, caps.chroma_444=true");
}

/// Hardware: an in-place rate retarget up and down emits no IDR
/// ([`crate::smoke_pattern::reconfigure_no_idr`]).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_reconfigure_no_idr() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");
    crate::smoke_pattern::reconfigure_no_idr(
        &mut enc,
        |i| nv12_frame(W, H, i as u32),
        &[60_000_000, 10_000_000],
    );
}

/// Hardware: can `splitEncodeMode` move in place (`resetEncoder=0`) without an IDR?
/// Sub-frame off — HEVC forced-split + sub-frame is unsupported, which would reject for
/// the wrong reason. Reports the verdict; asserts only that the measurement is valid.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_split_reconfigure_in_place() {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    const W: u32 = 1920;
    const H: u32 = 1080;
    const BPS: u64 = 40_000_000;
    let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
    let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

    // Sub-frame off; open split-disabled so the switch is a real change.
    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
    set_env("PUNKTFUNK_SPLIT_ENCODE", "0");

    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        BPS,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
        let (mut aus, mut keyframes) = (0usize, 0usize);
        for i in range {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                keyframes += au.keyframe as usize;
            }
        }
        (aus, keyframes)
    };

    // Session is lazy; reconfigure while `!inited` short-circuits to `true`.
    let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
    assert!(aus > 0, "no AUs before the reconfigure");
    assert_eq!(kfs, 1, "exactly the opening IDR before the reconfigure");
    assert!(
        enc.s.inited(),
        "session must be live for the spike to mean anything"
    );
    assert_eq!(
        enc.s.split_mode, disable,
        "the spike needs to OPEN split-disabled so the switch is a real change"
    );

    // Forced-2 on a 1-engine GPU is rejected for the wrong reason.
    // SAFETY: live session (`inited`); `get_cap` returns 0 on driver error.
    let engines = unsafe {
        enc.s.get_cap(
            enc.s.handle(),
            nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
        )
    };
    println!(
        "S1: NV_ENC_CAPS_NUM_ENCODER_ENGINES = {engines} (query_caps latched \
         encoder_engines={})",
        self_engines(&enc)
    );
    // `resolve_split_mode` reads the latched field, not the live cap.
    assert_eq!(
        self_engines(&enc),
        engines.max(0) as u32,
        "query_caps must latch NUM_ENCODER_ENGINES — resolve_split_mode reads that field, \
         not the live cap"
    );
    assert!(
        engines >= 2,
        "this GPU reports {engines} NVENC engine(s) — S1 is not interpretable here, run it on \
         a 2-engine card"
    );

    // Change only `splitEncodeMode`.
    enc.s.split_mode = two;
    let accepted = enc.reconfigure_bitrate(BPS);
    println!("S1: reconfigure DISABLE→TWO_FORCED accepted = {accepted}");

    let verdict = if !accepted {
        // Live session is still split-disabled — keep the field truthful.
        enc.s.split_mode = disable;
        "FAIL — driver REJECTED the in-place splitEncodeMode change"
    } else {
        let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
        assert!(aus > 0, "no AUs after the accepted reconfigure");
        if kfs == 0 {
            "PASS — accepted with NO IDR: mid-stream split adaptation is free"
        } else {
            "FAIL — accepted but forced an IDR (silently), which is the same as a rejection"
        }
    };
    println!("S1 VERDICT: {verdict}");

    // Reverse only if the forward change was accepted.
    if accepted {
        enc.s.split_mode = disable;
        let back = enc.reconfigure_bitrate(BPS);
        let kfs = if back {
            submit_and_poll(&mut enc, 8..12).1
        } else {
            usize::MAX
        };
        println!("S1: reverse TWO_FORCED→DISABLE accepted = {back}, keyframes after = {kfs}");
    }

    enc.flush().ok();
    remove_env("PUNKTFUNK_SPLIT_ENCODE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
}

/// Hardware: did an accepted in-place split actually take effect? A = fresh DISABLE, B =
/// fresh TWO_FORCED, C = DISABLE→TWO in place. C ≈ B ⇒ real; C ≈ A ⇒ ignored.
/// Bytes/AU are load-bearing: uninit VRAM under CBR can collapse every leg.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_split_reconfigure_takes_effect() {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    use std::time::Instant;
    const W: u32 = 3840;
    const H: u32 = 2160;
    const BPS: u64 = 400_000_000;
    const WARMUP: u32 = 8;
    const MEASURED: u32 = 24;
    /// Post-switch discard. Split does not reach steady state on frame 0; 16 was enough
    /// for the switched leg to match a fresh TWO_FORCED session.
    const SETTLE: u32 = 16;
    let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");

    // Rotated buffers do not help: the driver still returns zeroed VRAM. This harness
    // measures pixel-proportional cost only.
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

    // (early p50 µs, late p50 µs, median B/AU).
    let run_leg = |open_split: &str, switch_to: Option<u32>| -> (u128, u128, usize) {
        set_env("PUNKTFUNK_SPLIT_ENCODE", open_split);
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        // Same measured length; a switched leg starts `SETTLE` frames later.
        let measure_from = if switch_to.is_some() {
            WARMUP + SETTLE
        } else {
            WARMUP
        };
        let (mut times, mut sizes) = (Vec::new(), Vec::new());
        for i in 0..(measure_from + MEASURED) {
            // In-place switch once, after warmup.
            if i == WARMUP {
                if let Some(target) = switch_to {
                    enc.s.split_mode = target;
                    assert!(
                        enc.reconfigure_bitrate(BPS),
                        "in-place split switch must be accepted (S1a proved it is)"
                    );
                    continue;
                }
            }
            let t0 = Instant::now();
            enc.submit_indexed(&frames[(i % 4) as usize], i)
                .expect("submit");
            let mut got = 0usize;
            while let Some(au) = enc.poll().expect("poll") {
                got = au.data.len();
            }
            let dt = t0.elapsed().as_micros();
            if i >= measure_from {
                times.push(dt);
                sizes.push(got);
            }
        }
        enc.flush().ok();
        // Early vs late: a whole-window median of a settling switch lands between the arms.
        let half = times.len() / 2;
        let med = |s: &[u128]| {
            let mut v = s.to_vec();
            v.sort_unstable();
            v[v.len() / 2]
        };
        let (early, late) = (med(&times[..half]), med(&times[half..]));
        sizes.sort_unstable();
        (early, late, sizes[sizes.len() / 2])
    };

    let (a_early, a_late, a_bytes) = run_leg("0", None);
    let (b_early, b_late, b_bytes) = run_leg("2", None);
    let (c_early, c_late, c_bytes) = run_leg("0", Some(two));
    let (a_us, b_us, c_us) = (a_late, b_late, c_late);

    println!("S1b @ {W}x{H}@60 HEVC 8-bit, {} Mbps CBR:", BPS / 1_000_000);
    println!("  (early = first half of the measured window, late = second half)");
    println!(
        "  A fresh DISABLE      : early {a_early:>6} late {a_late:>6} us/frame, {a_bytes:>8} B/AU"
    );
    println!(
        "  B fresh TWO_FORCED   : early {b_early:>6} late {b_late:>6} us/frame, {b_bytes:>8} B/AU"
    );
    println!(
        "  C DISABLE→TWO in situ: early {c_early:>6} late {c_late:>6} us/frame, {c_bytes:>8} B/AU"
    );
    if c_early > c_late + c_late / 8 {
        println!(
            "  ⇒ leg C SETTLES ({c_early} → {c_late} us): the in-place switch is not \
             instantaneous, so a whole-window median understates it."
        );
    }

    let want_bytes = (BPS / 60 / 8) as usize;
    if a_bytes * 4 < want_bytes {
        println!(
            "  ⚠ INCONCLUSIVE on content: {a_bytes} B/AU is far below the {want_bytes} B/AU \
             CBR quota — rate control ran out of things to code, so these legs are not the \
             high-bits/frame regime the field case is in."
        );
    }
    let (near_b, near_a) = (c_us.abs_diff(b_us), c_us.abs_diff(a_us));
    println!(
        "  ⇒ C is nearer {} (|C-B|={near_b} vs |C-A|={near_a}) — {}",
        if near_b < near_a { "B" } else { "A" },
        if near_b < near_a {
            "the in-place split switch TOOK EFFECT"
        } else {
            "the driver appears to have IGNORED the in-place split change"
        }
    );

    remove_env("PUNKTFUNK_SPLIT_ENCODE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    let _ = (a_bytes, b_bytes, c_bytes);
}

/// Hardware: can `(split, sub-frame)` move as a pair in place, IDR-free?
/// `reconfigure_bitrate` does not recompute `subframe_chunks` — a caller flipping
/// sub-frame must clear that latch or `poll_chunk` busy-polls.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_split_subframe_pair_reconfigure() {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    const W: u32 = 1920;
    const H: u32 = 1080;
    const BPS: u64 = 40_000_000;
    let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
    let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

    // Split-disabled; sub-frame at the caps-gated default.
    set_env("PUNKTFUNK_SPLIT_ENCODE", "0");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");

    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        BPS,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
        let (mut aus, mut keyframes) = (0usize, 0usize);
        for i in range {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                keyframes += au.keyframe as usize;
            }
        }
        (aus, keyframes)
    };

    let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
    assert!(aus > 0 && kfs == 1, "opening IDR then steady P-frames");
    println!(
        "S1c: opened split={} subframe_on={} subframe_chunks={} chunked_poll={}",
        enc.s.split_mode,
        enc.s.subframe_on,
        enc.s.subframe_chunks,
        enc.supports_chunked_poll()
    );
    if !enc.s.subframe_on {
        println!(
            "S1c SKIPPED: sub-frame is off at open on this GPU/driver, so there is no pair to \
             flip — the arbitration reduces to S1a's plain split switch here."
        );
        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        return;
    }

    // Clear the chunked-poll latch with the sub-frame flag, or `poll_chunk` outlives it.
    enc.s.split_mode = two;
    enc.s.subframe_on = false;
    enc.s.subframe_chunks = false;
    let accepted = enc.reconfigure_bitrate(BPS);
    println!("S1c: (DISABLE,sub-frame on) → (TWO_FORCED,sub-frame off) accepted = {accepted}");

    if accepted {
        let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
        assert!(aus > 0, "no AUs after the pair flip");
        assert!(
            !enc.supports_chunked_poll(),
            "chunked poll must be disarmed once sub-frame is off — a stale latch makes \
             poll_chunk busy-poll its whole budget every AU"
        );
        println!(
            "S1c VERDICT: {}",
            if kfs == 0 {
                "PASS — the split×sub-frame PAIR moves in place with NO IDR"
            } else {
                "FAIL — pair flip forced an IDR"
            }
        );

        // Reverse pair (de-escalation).
        enc.s.split_mode = disable;
        enc.s.subframe_on = true;
        enc.s.subframe_chunks = enc.s.slices >= 2 && !enc.s.retrieving();
        let back = enc.reconfigure_bitrate(BPS);
        let kfs_back = if back {
            submit_and_poll(&mut enc, 8..12).1
        } else {
            usize::MAX
        };
        println!("S1c: reverse pair flip accepted = {back}, keyframes after = {kfs_back}");
    } else {
        println!(
            "S1c VERDICT: FAIL — driver REJECTED the pair flip. Split can still move alone \
             (S1a), so a WP3 arbitration would have to keep sub-frame fixed for the session \
             and only arbitrate split within that."
        );
        enc.s.split_mode = disable;
        enc.s.subframe_on = true;
    }

    enc.flush().ok();
    remove_env("PUNKTFUNK_SPLIT_ENCODE");
}

/// Hardware: does plain AUTO + default-on sub-frame actually split? HEVC split is
/// unsupported with sub-frame, so AUTO may mean "never split". Time AUTO vs DISABLE vs
/// TWO_FORCED at 4K (pixel-proportional; VRAM is zeroed).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_auto_split_with_subframe() {
    use std::time::Instant;
    const W: u32 = 3840;
    const H: u32 = 2160;
    const BPS: u64 = 400_000_000;
    const WARMUP: u32 = 8;
    const MEASURED: u32 = 24;

    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

    // `split: None` = unset = plain AUTO. Env `1` is AUTO_FORCED, which disarms sub-frame.
    let run = |split: Option<&str>, subframe: Option<&str>| -> (u128, bool) {
        match split {
            Some(v) => set_env("PUNKTFUNK_SPLIT_ENCODE", v),
            None => remove_env("PUNKTFUNK_SPLIT_ENCODE"),
        }
        match subframe {
            Some(v) => set_env("PUNKTFUNK_NVENC_SUBFRAME", v),
            None => remove_env("PUNKTFUNK_NVENC_SUBFRAME"),
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        let mut times = Vec::new();
        for i in 0..(WARMUP + MEASURED) {
            let t0 = Instant::now();
            enc.submit_indexed(&frames[(i % 4) as usize], i)
                .expect("submit");
            while enc.poll().expect("poll").is_some() {}
            if i >= WARMUP {
                times.push(t0.elapsed().as_micros());
            }
        }
        let sub = enc.s.subframe_on;
        enc.flush().ok();
        times.sort_unstable();
        (times[times.len() / 2], sub)
    };

    // Unset env: 4K60 8-bit is below SPLIT_FORCE_PIXEL_RATE → plain AUTO. Sub-frame must
    // stay on or this is not the fleet shape.
    let (auto_us, auto_sub) = run(None, None);
    let (dis_us, dis_sub) = run(Some("0"), None);
    let (two_us, two_sub) = run(Some("2"), Some("0"));
    // AUTO with sub-frame off: retiring AUTO would also change that shape.
    let (auto_nosub_us, auto_nosub_sub) = run(None, Some("0"));

    println!("D5 confirm @ {W}x{H}@60 HEVC 8-bit:");
    println!("  AUTO (unset) + sub-frame({auto_sub}) : {auto_us:>6} us/frame");
    println!("  DISABLE      + sub-frame({dis_sub}) : {dis_us:>6} us/frame");
    println!("  TWO_FORCED,   no sub-frame({two_sub}): {two_us:>6} us/frame");
    println!("  AUTO (unset), no sub-frame({auto_nosub_sub}): {auto_nosub_us:>6} us/frame");
    println!(
        "  ⇒ with sub-frame OFF, AUTO is nearer {} — retiring the AUTO arm {}",
        if auto_nosub_us.abs_diff(two_us) < auto_nosub_us.abs_diff(dis_us) {
            "TWO_FORCED (it DOES split)"
        } else {
            "DISABLE (it does not split either way)"
        },
        if auto_nosub_us.abs_diff(two_us) < auto_nosub_us.abs_diff(dis_us) {
            "would LOSE a real split on sub-frame-off sessions"
        } else {
            "is behaviour-neutral"
        }
    );
    assert!(
        auto_sub,
        "the AUTO leg resolved sub-frame OFF — it is not testing D5's fleet shape"
    );
    let (near_dis, near_two) = (auto_us.abs_diff(dis_us), auto_us.abs_diff(two_us));
    println!(
        "  ⇒ AUTO sits nearer {} (|A-D|={near_dis} vs |A-T|={near_two}) — D5 {}",
        if near_dis < near_two {
            "DISABLE"
        } else {
            "TWO"
        },
        if near_dis < near_two {
            "CONFIRMED: AUTO + sub-frame does NOT split; the resolver's AUTO arm is dead"
        } else {
            "REFUTED: AUTO does engage the second engine even with sub-frame on"
        }
    );

    remove_env("PUNKTFUNK_SPLIT_ENCODE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
}

/// Hardware: split ceiling. A refused mode falls back to DISABLE, not an error. Timing
/// tells whether an accepted mode actually used more engines.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_split_hardware_max() {
    use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
    use std::time::Instant;
    const W: u32 = 3840;
    const H: u32 = 2160;
    const BPS: u64 = 400_000_000;
    const WARMUP: u32 = 8;
    const MEASURED: u32 = 24;

    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

    // (opened mode, p50 µs, engines).
    let run = |split: &str| -> (u32, u128, i32) {
        set_env("PUNKTFUNK_SPLIT_ENCODE", split);
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        let mut times = Vec::new();
        for i in 0..(WARMUP + MEASURED) {
            let t0 = Instant::now();
            enc.submit_indexed(&frames[(i % 4) as usize], i)
                .expect("submit");
            while enc.poll().expect("poll").is_some() {}
            if i >= WARMUP {
                times.push(t0.elapsed().as_micros());
            }
        }
        // SAFETY: live session; `get_cap` returns 0 on driver error.
        let engines = unsafe {
            enc.s.get_cap(
                enc.s.handle(),
                nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
            )
        };
        let opened = enc.s.split_mode;
        enc.flush().ok();
        times.sort_unstable();
        (opened, times[times.len() / 2], engines)
    };

    println!("split ceiling probe @ {W}x{H}@60 HEVC 8-bit:");
    let mut baseline = None;
    // Env `0` selects DISABLE (enum 15), not the integer 0.
    for (label, env, want) in [
        ("DISABLE     ", "0", M::NV_ENC_SPLIT_DISABLE_MODE as u32),
        ("AUTO_FORCED ", "1", M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32),
        ("TWO_FORCED  ", "2", M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32),
        (
            "THREE_FORCED",
            "3",
            M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
        ),
    ] {
        let (opened, us, engines) = run(env);
        let honoured = opened == want;
        let vs = match baseline {
            None => {
                baseline = Some(us);
                String::new()
            }
            Some(b) => format!("  ({:.2}× vs DISABLE)", b as f64 / us as f64),
        };
        println!(
            "  req {label} → opened_mode={opened:<2} {} {us:>6} us/frame{vs}  [engines={engines}]",
            if honoured { "HONOURED" } else { "FELL BACK" }
        );
    }
    println!(
        "  note: opened_mode 15 = DISABLE (the backend's rejection fallback); a mode that is \
         HONOURED but no faster than DISABLE was accepted and did nothing."
    );

    remove_env("PUNKTFUNK_SPLIT_ENCODE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
}

/// Hardware: live split arbitration at 4K. Sub-frame off so the no-trade gate arms.
/// Must settle with zero extra IDRs and cache a splitting arm.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_split_arbitration_converges() {
    const W: u32 = 3840;
    const H: u32 = 2160;
    let disable = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;

    set_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE", "1");
    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
    remove_env("PUNKTFUNK_SPLIT_ENCODE");

    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        400_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    let mut keyframes = 0usize;
    let mut aus = 0usize;
    // Measure + settle + measure, with slack.
    for i in 0..140u32 {
        enc.submit_indexed(&frames[(i % 4) as usize], i)
            .expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            aus += 1;
            keyframes += au.keyframe as usize;
        }
    }
    let final_mode = enc.s.split_mode;
    let still_arbitrating = enc.s.arbiter.is_some();
    let verdict = cached_split_verdict(&enc.s.split_key());
    let enc_engines = enc.s.encoder_engines;
    enc.flush().ok();

    println!(
        "arbitration: {aus} AUs, {keyframes} keyframes, final split_mode={final_mode}, \
         cached verdict={verdict:?}, still running={still_arbitrating}"
    );
    assert!(aus > 100, "not enough AUs to complete an arbitration");
    assert!(
        !still_arbitrating,
        "arbitration did not finish in 140 frames"
    );
    assert_eq!(
        keyframes, 1,
        "THE POINT OF THIS DESIGN: arbitration must cost ZERO extra IDRs — only the session's \
         opening one"
    );
    assert_eq!(
        verdict,
        Some(final_mode),
        "the winning arm must be cached so later sessions skip the experiment"
    );
    assert_ne!(
        final_mode, disable,
        "at 4K with two engines a splitting arm is ~2x faster, so single-engine must not win"
    );
    // 4K60 is under SPLIT_FORCE_PIXEL_RATE (AUTO vs widest). Single-engine must not win.
    println!(
        "  (incumbent was the static rule's choice; challenger was mode {})",
        max_forced_split_mode(enc_engines)
    );

    remove_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    // Process-global cache would steer later tests that open this config with split unset.
    super::super::nvenc_core::clear_split_verdicts();
}

/// Hardware: Main10 split A/B (packed RGB10). Sub-frame off. Reports; both outcomes
/// are legitimate. `PF_AB_MODE` can retarget the operating point.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 vs Blackwell .21)"]
fn nvenc_cuda_main10_split_ab() {
    use std::time::Instant;
    const BPS: u64 = 400_000_000;
    const WARMUP: u32 = 12;
    const MEASURED: u32 = 32;
    // `PF_AB_MODE=WxHxFPS` retargets; default 4K60.
    let (w, h, fps) = std::env::var("PF_AB_MODE")
        .ok()
        .and_then(|s| {
            let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
            (p.len() == 3).then(|| (p[0], p[1], p[2]))
        })
        .unwrap_or((3840, 2160, 60));

    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    // Packed 10-bit: `bit_depth`/`hdr` are derived from the input, not the args.
    let frames: Vec<CapturedFrame> = (0..4).map(|i| rgb10_frame(w, h, i)).collect();

    let run = |split: &str| -> (u128, u8, usize) {
        set_env("PUNKTFUNK_SPLIT_ENCODE", split);
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::X2Rgb10,
            w,
            h,
            fps,
            BPS,
            true,
            10,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        let (mut times, mut bytes) = (Vec::new(), Vec::new());
        for i in 0..(WARMUP + MEASURED) {
            let t0 = Instant::now();
            enc.submit_indexed(&frames[(i % 4) as usize], i)
                .expect("submit");
            let mut got = 0usize;
            while let Some(au) = enc.poll().expect("poll") {
                got = au.data.len();
            }
            if i >= WARMUP {
                times.push(t0.elapsed().as_micros());
                bytes.push(got);
            }
        }
        let depth = enc.s.bit_depth;
        let opened = enc.s.split_mode;
        enc.flush().ok();
        times.sort_unstable();
        bytes.sort_unstable();
        println!(
            "    (opened split_mode={opened}, derived bit_depth={depth}, \
             {} B/AU)",
            bytes[bytes.len() / 2]
        );
        (times[times.len() / 2], depth, bytes[bytes.len() / 2])
    };

    println!(
        "Main10 split A/B @ {w}x{h}@{fps} HEVC 10-bit, {} Mbps:",
        BPS / 1_000_000
    );
    let (single_us, d1, _) = run("0");
    println!("  single-engine : {single_us:>6} us/frame");
    let (split_us, d2, _) = run("2");
    println!("  forced 2-way  : {split_us:>6} us/frame");
    assert_eq!(d1, 10, "leg 1 did not derive a 10-bit session");
    assert_eq!(d2, 10, "leg 2 did not derive a 10-bit session");
    let ratio = single_us as f64 / split_us.max(1) as f64;
    println!(
        "  ⇒ split is {ratio:.2}× the single-engine rate — {}",
        if ratio > 1.15 {
            "split WINS for Main10 here; the 2.7x-slower datapoint does NOT generalise"
        } else if ratio < 0.87 {
            "split LOSES for Main10 — the veto was right and must come back, scoped"
        } else {
            "a wash; neither arm is clearly better for Main10 here"
        }
    );

    remove_env("PUNKTFUNK_SPLIT_ENCODE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
}

/// Hardware: bits/frame curve. Zeroed VRAM only measures pixel-proportional cost.
/// [`noise_nv12_frame`] supplies entropy. Print B/AU next to every timing.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 / Blackwell .21)"]
fn nvenc_cuda_bits_per_frame_curve() {
    use std::time::Instant;
    const WARMUP: u32 = 10;
    const MEASURED: u32 = 24;
    let (w, h, fps) = std::env::var("PF_AB_MODE")
        .ok()
        .and_then(|s| {
            let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
            (p.len() == 3).then(|| (p[0], p[1], p[2]))
        })
        .unwrap_or((3840, 2160, 60));

    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    // Sweep spatial detail, not nominal bitrate. Pure noise overshoots any low target;
    // the x-axis is bits/frame actually produced.
    let bps: u64 = 600_000_000;
    println!(
        "bits/frame curve @ {w}x{h}@{fps} HEVC 8-bit, REAL content, {} Mbps cap:",
        bps / 1_000_000
    );
    println!("  detail | ACTUAL bits/frame |   single |  split-2 | ratio");
    for block in [64usize, 32, 16, 8, 4, 1] {
        let frames: Vec<CapturedFrame> = (0..4).map(|i| noise_nv12_frame(w, h, i, block)).collect();
        let run = |split: &str| -> (u128, usize) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                w,
                h,
                fps,
                bps,
                true,
                8,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let (mut times, mut bytes) = (Vec::new(), Vec::new());
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                let mut got = 0usize;
                while let Some(au) = enc.poll().expect("poll") {
                    got = au.data.len();
                }
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                    bytes.push(got);
                }
            }
            enc.flush().ok();
            times.sort_unstable();
            bytes.sort_unstable();
            (times[times.len() / 2], bytes[bytes.len() / 2])
        };
        let (s_us, s_bytes) = run("0");
        let (p_us, _) = run("2");
        println!(
            "  {block:>5}px | {:>10.2} Mbit    | {s_us:>6}us | {p_us:>6}us | {:>4.2}×",
            s_bytes as f64 * 8.0 / 1e6,
            s_us as f64 / p_us.max(1) as f64
        );
    }

    remove_env("PUNKTFUNK_SPLIT_ENCODE");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
}

/// Pre-session / nonsense RFI declines. Skips if the NVENC `.so` is absent.
#[test]
fn rfi_declines_impossible_ranges() {
    let Ok(mut enc) = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        1920,
        1080,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    ) else {
        eprintln!("skipping rfi_declines_impossible_ranges: NVENC unavailable (no NVIDIA driver)");
        return;
    };
    // Lazy init: no session yet.
    assert!(!enc.invalidate_ref_frames(0, 0), "no session → decline");
    assert!(!enc.invalidate_ref_frames(10, 5), "first > last → decline");
    assert!(
        !enc.invalidate_ref_frames(-1, 3),
        "negative first → decline"
    );
}

fn open_h265() -> NvencCudaEncoder {
    NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        1280,
        720,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA encoder")
}

/// Hardware: cycle codecs in one process; every leg must open and encode.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_codec_switch_reopen() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    for (leg, codec) in [
        Codec::H265,
        Codec::Av1,
        Codec::H265,
        Codec::H264,
        Codec::H265,
    ]
    .into_iter()
    .enumerate()
    {
        let mut enc = NvencCudaEncoder::open(
            codec,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open");
        for f in 0..4u32 {
            let frame = nv12_frame(W, H, f);
            enc.submit_indexed(&frame, f)
                .unwrap_or_else(|e| panic!("leg {leg} {codec:?} submit failed: {e:#}"));
            while enc.poll().expect("poll").is_some() {}
        }
        drop(enc);
    }
    println!("nvenc_cuda codec-switch: 5 legs across H265/AV1/H264, all clean");
}

/// Hardware: the H.264 stream through the client's planner, one AU in, one picture
/// out. 1920x1200 is level 5, where an unstated reorder bound is 12 pictures.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_h264_shows_each_picture_in_its_own_au() {
    const W: u32 = 1920;
    const H: u32 = 1200;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H264,
        PixelFormat::Nv12,
        W,
        H,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    let mut planner = pf_vaapi::H264Planner::new();
    let mut stored = Vec::new();
    let mut lags = Vec::new();
    for i in 0..40u32 {
        let frame = nv12_frame(W, H, i);
        enc.submit_indexed(&frame, i).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            let plan = planner.plan_au(&au.data).expect("plan");
            if stored.is_empty() {
                let vui = &plan.sps.vui_parameters;
                println!(
                    "sps: level={:?} poc_type={} refs={} restriction={} reorder={} dpb={}",
                    plan.sps.level_idc,
                    plan.sps.pic_order_cnt_type,
                    plan.sps.max_num_ref_frames,
                    vui.bitstream_restriction_flag,
                    vui.max_num_reorder_frames,
                    vui.max_dec_frame_buffering,
                );
            }
            stored.push(plan.dpb.stored.expect("stored"));
            for shown in &plan.dpb.outputs {
                let decoded_at = stored.iter().position(|id| id == shown).expect("known");
                lags.push(stored.len() - 1 - decoded_at);
            }
        }
    }
    enc.flush().ok();
    println!("{} AUs planned, output lags {lags:?}", stored.len());
    assert!(stored.len() >= 30, "only {} AUs produced", stored.len());
    assert_eq!(lags, vec![0; stored.len()]);
}

/// Hardware: drop with encodes in flight, then a fresh session must still open.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_dirty_teardown_reopen() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    for round in 0..3 {
        let mut enc = open_h265();
        for f in 0..4u32 {
            let frame = nv12_frame(W, H, f);
            enc.submit_indexed(&frame, f)
                .unwrap_or_else(|e| panic!("round {round} submit {f} failed: {e:#}"));
        }
        drop(enc); // pending encodes still in flight
    }
    let mut enc = open_h265();
    let frame = nv12_frame(W, H, 0);
    enc.submit_indexed(&frame, 0)
        .expect("reopen after dirty teardowns");
    while enc.poll().expect("poll").is_some() {}
    println!("nvenc_cuda dirty-teardown: 3 dirty drops, reopen clean");
}

/// Hardware: exhaust the concurrent-session cap, assert open fails, free slots, rebuild
/// in place and produce an AU.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_open_failure_diagnosis_and_recovery() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    try_api().expect("nvenc api");
    let shared = cuda::context().expect("shared ctx");

    let open_raw = |device: *mut c_void| -> (nv::NVENCSTATUS, *mut c_void) {
        let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
            version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
            deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            device,
            apiVersion: nv::NVENCAPI_VERSION,
            ..Default::default()
        };
        let mut enc: *mut c_void = ptr::null_mut();
        // SAFETY: live params / out-param across the sync call.
        let st = unsafe { (api().open_encode_session_ex)(&mut params, &mut enc) };
        (st, enc)
    };

    // Hold sessions until open fails.
    let mut held = Vec::new();
    loop {
        let (st, enc) = open_raw(shared);
        if st != nv::NVENCSTATUS::NV_ENC_SUCCESS {
            if !enc.is_null() {
                // SAFETY: destroy failed-open residue (NVENC docs).
                unsafe {
                    let _ = (api().destroy_encoder)(enc);
                }
            }
            break;
        }
        held.push(enc);
    }
    assert!(!held.is_empty(), "expected a finite session cap");

    // Caps-probe open must fail while the cap is exhausted.
    let mut enc = open_h265();
    let frame = nv12_frame(W, H, 0);
    let err = enc
        .submit_indexed(&frame, 0)
        .expect_err("submit must fail while the cap is exhausted");
    println!("at-cap error (self-diagnosis logged alongside): {err:#}");

    // Slots freed → same encoder rebuilds in place.
    for e in held {
        // SAFETY: successful raw open; destroy once.
        unsafe {
            let _ = (api().destroy_encoder)(e);
        }
    }
    assert!(enc.reset(), "in-place reset must be available");
    let frame = nv12_frame(W, H, 1);
    enc.submit_indexed(&frame, 1)
        .expect("rebuild after the transient cleared");
    let mut got = false;
    while enc.poll().expect("poll").is_some() {
        got = true;
    }
    assert!(got, "recovered encoder must produce an AU");
    println!("nvenc_cuda open-failure recovery: cap hit → diagnosed → recovered in place");
}

/// Hardware: stream-ordered submit must arm on a default-env session. A silent fallback
/// still encodes — no other test would notice.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_stream_ordered_arms() {
    const W: u32 = 640;
    const H: u32 = 360;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    // Operator opt-out / two-thread mode: skip, don't fail.
    if !stream_ordered_requested() || async_retrieve_requested() {
        println!("skipped: stream-ordered submit disabled by env");
        return;
    }
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        8_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");
    let frame = nv12_frame(W, H, 0);
    enc.submit_indexed(&frame, 0).expect("submit");
    let au = enc.poll().expect("poll").expect("AU");
    assert!(au.keyframe, "opening AU must be the session IDR");
    assert!(
        enc.stream_ordered,
        "IO-stream binding must arm on a default-env session (NvEncSetIOCudaStreams rejected?)"
    );
    assert!(
        !enc.io_stream.is_null(),
        "the boxed CUstream must be held while armed"
    );
}

/// Queue a convert-timeline wait nothing will signal, as a worker that died mid-pass leaves.
fn wedge_copy_stream(enc: &mut NvencCudaEncoder) {
    const NEVER: u64 = 1_000;
    let mut worker = pf_zerocopy::Importer::new_for_capture().expect("in-process importer");
    let fd = worker.convert_timeline().expect("convert timeline fd");
    let sem = cuda::ExternalSemaphore::import_timeline_fd(fd).expect("import the timeline");
    sem.wait(NEVER).expect("queue the wait");
    enc.worker = Some(worker);
    enc.convert_sem = Some(sem);
    enc.convert_waited = NEVER;
    assert!(
        cuda::copy_stream_sync_deadline(std::time::Duration::from_millis(50)).is_err(),
        "the unsignalled wait must hold the copy stream"
    );
}

/// Hardware: a stuck fused-pass wait on the stream NVENC is bound to. Releasing the
/// semaphore retires the worker and frees the stream; the session keeps encoding, and a
/// teardown over a second stuck wait returns.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_stuck_fused_wait_releases() {
    const W: u32 = 640;
    const H: u32 = 360;
    set_env("PUNKTFUNK_ZEROCOPY_INPROC", "1");
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    if !stream_ordered_requested() || async_retrieve_requested() {
        println!("skipped: stream-ordered submit disabled by env");
        return;
    }
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        8_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");
    enc.submit_indexed(&nv12_frame(W, H, 0), 0).expect("submit");
    enc.poll().expect("poll").expect("AU");
    assert!(
        enc.stream_ordered,
        "IO streams must be bound to the copy stream"
    );

    wedge_copy_stream(&mut enc);
    let t = std::time::Instant::now();
    enc.release_convert_sem();
    assert!(
        t.elapsed() < std::time::Duration::from_secs(2),
        "release must not hang"
    );
    assert!(enc.convert_sem.is_none() && enc.worker.is_none());
    cuda::copy_stream_sync_deadline(std::time::Duration::from_millis(100))
        .expect("the copy stream drains once the wait is satisfied");
    for i in 1..5 {
        enc.submit_indexed(&nv12_frame(W, H, i), i)
            .expect("submit after release");
        enc.poll().expect("poll").expect("AU after release");
    }

    wedge_copy_stream(&mut enc);
    let t = std::time::Instant::now();
    drop(enc);
    assert!(
        t.elapsed() < std::time::Duration::from_secs(3),
        "teardown must not hang"
    );
    remove_env("PUNKTFUNK_ZEROCOPY_INPROC");
}

/// Hardware: cursor frames stay on the stream-ordered path (`blend_ref_ordered`, ticket
/// +2 per frame), including a bitmap change and per-frame moves.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_cursor_blend_stream_ordered() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    // Operator opt-out / two-thread mode: skip, don't fail.
    if !stream_ordered_requested() || async_retrieve_requested() {
        println!("skipped: stream-ordered submit disabled by env");
        return;
    }
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        8_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        true, // Vulkan slot ring + blend
        4,
    )
    .expect("open NVENC CUDA session");
    let cursor = |serial: u64, x: i32, y: i32| pf_frame::CursorOverlay {
        x,
        y,
        w: 32,
        h: 32,
        rgba: std::sync::Arc::new(vec![0xFF; 32 * 32 * 4]),
        serial,
        hot_x: 0,
        hot_y: 0,
        visible: true,
    };
    let mut aus = 0usize;
    for i in 0..6u32 {
        let mut frame = nv12_frame(W, H, i);
        // Serial flip at frame 3 (upload quiesce); position moves every frame.
        frame.cursor = Some(cursor(
            if i < 3 { 1 } else { 2 },
            40 + i as i32 * 9,
            60 + i as i32 * 5,
        ));
        enc.submit_indexed(&frame, i).expect("submit cursor frame");
        while enc.poll().expect("poll").is_some() {
            aus += 1;
        }
    }
    assert_eq!(aus, 6, "every cursor frame must deliver an AU");
    assert!(
        enc.stream_ordered,
        "IO-stream binding must arm on a default-env session"
    );
    let vk = enc
        .vk_blend
        .as_ref()
        .expect("Vulkan slot blend must come up on an RTX box");
    assert!(
        vk.ordered_ready(),
        "timeline semaphore must export to CUDA on this driver"
    );
    assert_eq!(
        vk.ordered_ticket(),
        12,
        "all 6 cursor blends must take the ordered path (2 timeline values each)"
    );
    println!(
        "nvenc_cuda cursor stream-ordered: 6 cursor AUs, ticket={}",
        vk.ordered_ticket()
    );
}

/// Hardware: `set_pipelined(true)` rebuilds without IO-stream binding, spawns the
/// retrieve thread, keeps delivering AUs. First post-escalation AU is the re-open IDR.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_pipelined_escalation() {
    const W: u32 = 1280;
    const H: u32 = 720;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    if async_retrieve_vetoed() {
        println!("skipped: PUNKTFUNK_NVENC_ASYNC=0 vetoes the escalation");
        return;
    }
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        8_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");
    for i in 0..3u32 {
        let frame = nv12_frame(W, H, i);
        enc.submit_indexed(&frame, i).expect("submit");
        enc.poll().expect("poll").expect("AU");
    }
    assert!(!enc.s.retrieving(), "session starts sync");
    assert!(enc.set_pipelined(true), "escalation must be accepted");
    let mut aus = 0usize;
    let mut first_key = false;
    for i in 3..13u32 {
        let frame = nv12_frame(W, H, i);
        enc.submit_indexed(&frame, i)
            .expect("submit post-escalation");
        while let Some(au) = enc.poll().expect("poll") {
            if aus == 0 {
                first_key = au.keyframe;
            }
            aus += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    // Bounded drain of the pipelined tail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while aus < 10 && std::time::Instant::now() < deadline {
        if enc.poll().expect("poll").is_some() {
            aus += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        enc.s.retrieving(),
        "retrieve thread must be live after escalation"
    );
    assert!(
        !enc.stream_ordered,
        "IO-stream binding must be gone in pipelined mode"
    );
    assert_eq!(aus, 10, "every post-escalation frame must deliver an AU");
    assert!(first_key, "first post-escalation AU is the re-open IDR");
}

/// Hardware: do slices become readable mid-encode? Prints a doNotWait timeline; asserts
/// only that 4 slices materialize. `--test-threads=1`.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_subframe_slice_probe() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            remove_env("PUNKTFUNK_NVENC_SLICES");
            remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        }
    }
    set_env("PUNKTFUNK_NVENC_SLICES", "4");
    set_env("PUNKTFUNK_NVENC_SUBFRAME", "1");
    let _guard = EnvGuard;

    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    let frame = nv12_frame(W, H, 0);
    enc.submit_indexed(&frame, 0).expect("submit opening frame");
    enc.poll().expect("poll").expect("opening AU");

    // Spin doNotWait against the in-flight bitstream before the blocking poll.
    let frame = nv12_frame(W, H, 1);
    enc.submit_indexed(&frame, 1).expect("submit probed frame");
    let bs = enc.s.pending().back().expect("in-flight entry").bs;
    let mut offsets = vec![0u32; slice_offsets_len(W, H)];
    let t0 = std::time::Instant::now();
    let mut timeline: Vec<(u64, nv::NVENCSTATUS, u32, u32)> = Vec::new();
    loop {
        // SAFETY: live session; `bs` is the just-submitted bitstream. `offsets` is sized for
        // this frame. The guard unlocks before the next iteration.
        let lock =
            unsafe { BitstreamLock::new(api(), enc.s.handle(), bs, true, Some(&mut offsets)) };
        let (status, n, bytes) = match lock {
            Ok(l) => (
                nv::NVENCSTATUS::NV_ENC_SUCCESS,
                l.info().numSlices,
                l.info().bitstreamSizeInBytes,
            ),
            Err(st) => (st, 0, 0),
        };
        let t_us = t0.elapsed().as_micros() as u64;
        timeline.push((t_us, status, n, bytes));
        // Complete = 4 slices. LOCK_BUSY = still encoding. 50 ms safety window.
        if (status == nv::NVENCSTATUS::NV_ENC_SUCCESS && n >= 4) || t_us > 50_000 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_micros(50));
    }
    println!("subframe probe timeline (t_us, status, numSlices, bytes):");
    for (t, st, n, b) in &timeline {
        println!("  {t:>7} us  {st:?}  slices={n}  bytes={b}");
    }
    // Normal poll: probe locks must not have corrupted the session.
    let au = enc.poll().expect("poll probed frame").expect("probed AU");
    assert!(!au.data.is_empty(), "probed AU must carry data");
    let last = timeline.last().expect("at least one sample");
    assert_eq!(
        last.2, 4,
        "4 slices must materialize (PUNKTFUNK_NVENC_SLICES=4 + subframe readback armed)"
    );
    // One more frame — session still healthy.
    let frame = nv12_frame(W, H, 2);
    enc.submit_indexed(&frame, 2).expect("submit follow-up");
    enc.poll().expect("poll").expect("follow-up AU");
}

/// Annex-B NAL start code.
fn starts_with_start_code(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])
}

/// Hardware: chunked poll at defaults (4 slices + sub-frame). First/last metadata, Annex-B
/// cuts, shadow reassembly. At least one multi-chunk frame. `--test-threads=1`.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_chunked_poll_end_to_end() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    // Defaults under test — no leaked knobs.
    remove_env("PUNKTFUNK_NVENC_SLICES");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");

    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        4,
    )
    .expect("open NVENC CUDA session");

    let mut multi_chunk_frames = 0usize;
    let mut total_chunks = 0usize;
    for i in 0..6u32 {
        let frame = nv12_frame(W, H, i);
        enc.submit_indexed(&frame, i).expect("submit");
        assert!(
            enc.supports_chunked_poll(),
            "4 slices + subframe on a sync session must arm chunked poll"
        );
        let mut au = Vec::new();
        let mut chunks = 0usize;
        loop {
            let c = enc
                .poll_chunk()
                .expect("poll_chunk")
                .expect("an AU is in flight — poll_chunk must block, never None");
            if chunks == 0 {
                assert!(c.first, "the first chunk must open the AU");
                assert_eq!(
                    c.keyframe,
                    i == 0,
                    "only the session-opening frame is an IDR"
                );
            }
            assert_eq!(c.pts_ns, i as u64 * 16_666_667, "pts rides every chunk");
            assert!(!c.recovery_anchor, "no RFI happened");
            if !c.data.is_empty() {
                assert!(
                    starts_with_start_code(&c.data),
                    "chunk cut must land on an Annex-B start code (frame {i}, chunk {chunks})"
                );
            }
            au.extend_from_slice(&c.data);
            chunks += 1;
            if c.last {
                break;
            }
        }
        assert!(!au.is_empty(), "frame {i} produced an empty AU");
        assert!(
            enc.s.chunk.is_none(),
            "chunk state must be cleared once the AU closes"
        );
        if chunks > 1 {
            multi_chunk_frames += 1;
        }
        total_chunks += chunks;
        println!("frame {i}: {chunks} chunks, {} bytes", au.len());
    }
    assert!(
        multi_chunk_frames >= 1,
        "sub-frame readback yielded no multi-chunk frame — incremental slice readback \
         regressed (the probe shows ~200 µs slice spacing on this GPU)"
    );
    println!(
        "nvenc_cuda chunked poll: {total_chunks} chunks over 6 frames, \
         {multi_chunk_frames} frames chunked"
    );

    // A drained chunked AU leaves `poll()` usable.
    let frame = nv12_frame(W, H, 6);
    enc.submit_indexed(&frame, 6)
        .expect("submit plain-poll frame");
    let au = enc.poll().expect("poll").expect("AU");
    assert!(!au.data.is_empty());
}

/// Hardware: client `max_slices=1` must encode single-slice with chunked poll disarmed.
/// No env knobs. `--test-threads=1`.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_single_slice_client_ceiling() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    // Negotiated ceiling, not the operator override.
    remove_env("PUNKTFUNK_NVENC_SLICES");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
    let mut enc = NvencCudaEncoder::open(
        Codec::H265,
        PixelFormat::Nv12,
        W,
        H,
        60,
        20_000_000,
        true,
        8,
        false,
        ChromaFormat::Yuv420,
        false,
        1, // client never advertised multi-slice
    )
    .expect("open NVENC CUDA session");
    for i in 0..4u32 {
        let frame = nv12_frame(W, H, i);
        enc.submit_indexed(&frame, i).expect("submit");
        assert_eq!(
            enc.s.slices, 1,
            "a 1-slice client ceiling must clamp the Phase-3 default"
        );
        assert!(
            !enc.supports_chunked_poll(),
            "single-slice sessions have no boundaries — chunked poll must stay disarmed"
        );
        let au = enc.poll().expect("poll").expect("one AU per sync frame");
        assert!(!au.data.is_empty(), "frame {i} produced an empty AU");
    }
}

/// Hardware: `PUNKTFUNK_NVENC_SLICES=1` disarms chunked poll; `poll_chunk` is one
/// self-closing whole-AU chunk. `--test-threads=1`.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
fn nvenc_cuda_chunked_poll_fallback_whole_au() {
    const W: u32 = 1280;
    const H: u32 = 720;
    struct EnvGuard;
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            remove_env("PUNKTFUNK_NVENC_SLICES");
            remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        }
    }
    let _guard = EnvGuard;
    pf_zerocopy::cuda::make_current().expect("shared CUDA context current");

    // Explicit single slice — no boundaries.
    set_env("PUNKTFUNK_NVENC_SLICES", "1");
    remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    let mut enc = open_h265();
    let frame = nv12_frame(W, H, 0);
    enc.submit_indexed(&frame, 0).expect("submit");
    assert!(
        !enc.supports_chunked_poll(),
        "PUNKTFUNK_NVENC_SLICES=1 → chunked poll must not arm"
    );
    let c = enc
        .poll_chunk()
        .expect("poll_chunk")
        .expect("whole-AU chunk");
    assert!(c.first && c.last, "fallback chunk must be self-closing");
    assert!(c.keyframe, "opening AU is the session IDR");
    assert!(!c.data.is_empty());
    assert!(
        enc.poll_chunk().expect("poll_chunk").is_none(),
        "nothing in flight → None"
    );
    drop(enc);

    // Sub-frame vetoed: slices stay, chunked poll disarms, plain `poll` carries.
    remove_env("PUNKTFUNK_NVENC_SLICES");
    set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
    let mut enc = open_h265();
    let frame = nv12_frame(W, H, 0);
    enc.submit_indexed(&frame, 0).expect("submit");
    assert!(
        !enc.supports_chunked_poll(),
        "PUNKTFUNK_NVENC_SUBFRAME=0 → chunked poll must not arm"
    );
    let au = enc.poll().expect("poll").expect("AU");
    assert!(au.keyframe && !au.data.is_empty());
}
