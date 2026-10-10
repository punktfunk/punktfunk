use super::*;
use pf_frame::{dxgi::D3d11Frame, CapturedFrame, FramePayload};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};

#[test]
#[ignore = "requires an RTX GPU with HEVC/AV1 encode and the default synchronous retrieve mode"]
fn nvenc_prepare_publishes_caps_before_submit() {
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010};

    // SAFETY: DXGI factory creation borrows nothing and has no preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    let adapter = (0..)
        .map_while(|i| {
            // SAFETY: `factory` outlives this closure and the call takes no lasting alias.
            unsafe { factory.EnumAdapters1(i) }.ok()
        })
        .find(|a| {
            // SAFETY: `a` is a live adapter the enumeration above just returned.
            unsafe { a.GetDesc1() }.is_ok_and(|d| d.VendorId == 0x10de)
        })
        .expect("NVIDIA adapter");
    // SAFETY: `adapter` is a live enumeration result held by this scope for the whole call,
    // and `make_device` takes no lasting alias to it.
    let (device, _) = unsafe { pf_frame::dxgi::make_device(&adapter) }.expect("make_device");
    const W: u32 = 1280;
    const H: u32 = 720;
    const BPS: u64 = 20_000_000;
    for codec in [Codec::H265, Codec::Av1] {
        for (format, dxgi, depth, hdr) in [
            (PixelFormat::Nv12, DXGI_FORMAT_NV12, 8, false),
            (PixelFormat::P010, DXGI_FORMAT_P010, 10, true),
            (
                PixelFormat::Rgb10a2Sdr,
                DXGI_FORMAT_R10G10B10A2_UNORM,
                10,
                false,
            ),
        ] {
            let mut enc = NvencD3d11Encoder::open(
                codec,
                format,
                W,
                H,
                60,
                BPS,
                10,
                ChromaFormat::Yuv444,
                1,
                None,
            )
            .expect("NVENC open");
            enc.prepare_d3d11(&device, format, W, H).expect("prepare");
            assert!(enc.s.inited());
            assert_eq!(enc.init_device, device.as_raw());
            assert_eq!((enc.s.bit_depth, enc.hdr_requested), (depth, hdr));
            assert!(enc.s.opening());
            assert_eq!(enc.s.frame_idx, 0);
            assert!(enc.s.pending().is_empty());
            assert!(enc.regs.is_empty());
            assert!(enc.poll().expect("poll before submit").is_none());
            // SAFETY: `enc.s.handle()` is the open session this test built above, and a cap
            // query neither retains the handle nor mutates session state.
            let rfi = unsafe {
                enc.s.get_cap(
                    enc.s.handle(),
                    nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION,
                )
            } != 0;
            // SAFETY: as above — same live session, same read-only query.
            let yuv444 = unsafe {
                enc.s.get_cap(
                    enc.s.handle(),
                    nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
                )
            } != 0;
            let caps = enc.caps();
            assert_eq!(caps.supports_rfi, rfi);
            assert_eq!(
                caps.chroma_444,
                codec == Codec::H265 && full_chroma_input(buffer_format(format)) && yuv444,
            );
            assert_eq!(enc.applied_bitrate_bps(), Some(BPS));
            let session = enc.s.handle();
            let bitstreams = enc.s.bitstreams().to_vec();
            enc.prepare_d3d11(&device, format, W, H)
                .expect("prepare again");
            assert_eq!(enc.s.handle(), session);
            assert_eq!(enc.s.bitstreams(), bitstreams);
            assert_eq!(enc.caps(), caps);
            assert!(!enc.invalidate_ref_frames(-1, -1));
            assert!(!enc.invalidate_ref_frames(100, 100));

            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: dxgi,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut texture = None;
            // SAFETY: `device` is live for this scope and `desc` is a fully initialised
            // D3D11_TEXTURE2D_DESC; the out-parameter is a local the call writes once.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .expect("input texture");
            let mut frame = CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: 0,
                format,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: texture.expect("input texture"),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            for i in 100..104 {
                frame.pts_ns = u64::from(i) * 16_666_667;
                enc.submit_indexed(&frame, i).expect("submit");
                let au = enc.poll().expect("poll").expect("AU");
                assert_eq!(au.keyframe, i == 100);
                assert_eq!(enc.s.handle(), session);
                assert_eq!(enc.s.bitstreams(), bitstreams);
                assert_eq!(enc.caps(), caps);
                assert_eq!(enc.applied_bitrate_bps(), Some(BPS));
            }
            enc.s.rfi_supported = false;
            assert!(!enc.invalidate_ref_frames(103, 103));
            assert!(!enc.s.pending_anchor);
            enc.s.rfi_supported = rfi;
            enc.distrust_references();
            assert!(!enc.invalidate_ref_frames(103, 103));
            enc.s.distrusted = false;
            let recovered = enc.invalidate_ref_frames(103, 103);
            if !recovered {
                enc.request_keyframe();
            }
            frame.pts_ns += 16_666_667;
            enc.submit_indexed(&frame, 104).expect("recovery submit");
            let au = enc.poll().expect("recovery poll").expect("recovery AU");
            assert_eq!(au.recovery_anchor, recovered);
            assert_eq!(au.keyframe, !recovered);
        }
    }
}

/// Saturated primaries separate BT.601 from BT.709 by tens of code points (pure-green luma 145 vs 173).
const BARS: [(u8, u8, u8); 8] = [
    (255, 255, 255),
    (255, 255, 0),
    (0, 255, 255),
    (0, 255, 0),
    (255, 0, 255),
    (255, 0, 0),
    (0, 0, 255),
    (0, 0, 0),
];

/// Left half: colour bars (matrix measurement). Right half: 1-px red/blue columns (true 4:4:4
/// keeps adjacent chroma distinct; a subsampled encode blends them).
fn probe_pattern(w: usize, h: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 4];
    let bar_w = (w / 2) / BARS.len();
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = if x < w / 2 {
                BARS[(x / bar_w).min(BARS.len() - 1)]
            } else if x % 2 == 0 {
                (255, 0, 0)
            } else {
                (0, 0, 255)
            };
            let o = (y * w + x) * 4;
            px[o] = b;
            px[o + 1] = g;
            px[o + 2] = r;
            px[o + 3] = 255;
        }
    }
    px
}

use crate::smoke_pattern::scroll_pattern;

/// The wave on NVENC, one HEVC stream: a loss with no anchor starts wave A (marks on
/// its start and close, no IDR); a loss of the plain P after it anchors on the close; a
/// loss whose anchor would be a dirty picture of A waves again (B); a loss mid-B spoils
/// it (no close mark) and queues C on the frame after B closes. Dumps the stream and
/// four client views for the decode check: `-dropA` loses frames 1–2 ahead of A,
/// `-dropL` the frame the anchor P answers, `-dropP` the anchor P ahead of B, `-dropC`
/// the two frames ahead of the mid-B loss; the anchor P, B's close and C's close must
/// decode identical. `PF_WAVE_SMOKE=WxH[:bits[:fps[:mbps]]]` runs a production shape
/// (`3840x2160:10:120:100`); 10-bit feeds R10G10B10A2 textures.
///
/// `cargo test -p pf-encode-win --features nvenc nvenc_wave_smoke -- --ignored --nocapture`
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
fn nvenc_wave_smoke() {
    let shape = std::env::var("PF_WAVE_SMOKE").unwrap_or_else(|_| "256x256:8:60".into());
    let mut parts = shape.split(':');
    let (w, h) = parts
        .next()
        .and_then(|s| s.split_once('x'))
        .map(|(w, h)| (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap()))
        .expect("PF_WAVE_SMOKE=WxH[:bits[:fps]]");
    let ten_bit = parts.next().is_some_and(|b| b == "10");
    let fps: u32 = parts.next().map_or(60, |f| f.parse().unwrap());
    let mbps: u64 = parts
        .next()
        .map_or(if w >= 1920 { 86 } else { 10 }, |m| m.parse().unwrap());
    #[allow(non_snake_case)]
    let (W, H) = (w, h);
    let (format, dxgi) = if ten_bit {
        (PixelFormat::Rgb10a2Sdr, DXGI_FORMAT_R10G10B10A2_UNORM)
    } else {
        (PixelFormat::Bgra, DXGI_FORMAT_B8G8R8A8_UNORM)
    };
    // SAFETY: test-only D3D11/DXGI COM calls on one thread; every out-pointer is checked
    // before use; every texture outlives the encoder call that reads it.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
        let adapter = (0..)
            .map_while(|i| factory.EnumAdapters1(i).ok())
            .find(|a| a.GetDesc1().is_ok_and(|d| d.VendorId == 0x10de))
            .expect("NVIDIA adapter");
        let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");
        let texture = |i: usize| {
            let mut bytes = scroll_pattern(W as usize, H as usize, i);
            if ten_bit {
                // BGRA8 -> R10G10B10A2 in place: each channel to its 10-bit lane.
                for px in bytes.chunks_exact_mut(4) {
                    let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
                    let v = (r << 2) | ((g << 2) << 10) | ((b << 2) << 20) | (3 << 30);
                    px.copy_from_slice(&v.to_le_bytes());
                }
            }
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: bytes.as_ptr() as *const _,
                SysMemPitch: W * 4,
                SysMemSlicePitch: 0,
            };
            let desc = D3D11_TEXTURE2D_DESC {
                Width: W,
                Height: H,
                MipLevels: 1,
                ArraySize: 1,
                Format: dxgi,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .expect("frame texture");
            tex.expect("null frame texture")
        };
        let mut enc = NvencD3d11Encoder::open(
            Codec::H265,
            format,
            W,
            H,
            fps,
            mbps * 1_000_000,
            if ten_bit { 10 } else { 8 },
            ChromaFormat::Yuv420,
            1,
            None,
        )
        .expect("NVENC open");
        // Caps, RFI included, exist only once the session is prepared on a device.
        enc.prepare_d3d11(&device, format, W, H).expect("prepare");
        assert!(
            enc.caps().supports_rfi,
            "the RTX box invalidates references"
        );
        let cycle = enc.s.wave_cycle() as usize;
        assert!(cycle >= 2, "the wave is on");
        println!(
            "nvenc_wave_smoke: {W}x{H} {}-bit {fps} fps {mbps} Mbps, cycle {cycle} frames",
            if ten_bit { 10 } else { 8 }
        );
        // Wave A at 3 (loss with no anchor), anchor P after it, wave B off a dirty
        // anchor, a loss three frames into B that queues C behind it, a plain P after C.
        let a_start = 3;
        let a_close = a_start + cycle - 1;
        let anchor_p = a_close + 2;
        let b_start = anchor_p + 1;
        let b_close = b_start + cycle - 1;
        let spoil_at = b_start + 3;
        let c_start = b_close + 1;
        let c_close = c_start + cycle - 1;
        // Plain P frames after C's close: `PF_WAVE_TAIL=<n>` lengthens the window that
        // shows whether a residual left at the close drifts.
        let tail: usize = std::env::var("PF_WAVE_TAIL")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(1);
        let last = c_close + tail;
        let mut aus = Vec::new();
        for i in 0..=last {
            if i == a_start {
                assert!(
                    enc.invalidate_ref_frames(0, 2),
                    "no anchor: the wave answers"
                );
                assert_eq!(enc.s.wave.map(|w| w.index), Some(0));
            }
            if i == anchor_p {
                let lost = (anchor_p - 1) as i64;
                assert!(enc.invalidate_ref_frames(lost, lost), "the close anchors");
                assert!(enc.s.wave.is_none(), "a clean anchor, no wave");
            }
            if i == b_start {
                // The anchor for this loss would be a picture of A before its close.
                let lost = (a_close - 1) as i64;
                assert!(enc.invalidate_ref_frames(lost, lost));
                assert_eq!(enc.s.wave.map(|w| w.index), Some(0), "dirty anchor: wave B");
            }
            if i == b_start + 1 {
                // Asked while B runs, for a frame before its start: no invalidation
                // (the driver would drop the sweep), no anchor, B's close still lifts.
                let lost = anchor_p as i64;
                assert!(enc.invalidate_ref_frames(lost, lost));
                assert_eq!(enc.s.wave.map(|w| w.index), Some(1), "B runs on");
                assert!(
                    !enc.s.wave_spoiled && enc.s.wave_queued,
                    "B unspoiled, C queued"
                );
            }
            if i == spoil_at {
                let lost = (spoil_at - 1) as i64;
                assert!(enc.invalidate_ref_frames(lost, lost));
                assert_eq!(enc.s.wave.map(|w| w.index), Some(3), "B runs on");
                assert!(enc.s.wave_spoiled && enc.s.wave_queued, "C queued behind B");
            }
            if i == c_start {
                assert_eq!(enc.s.wave.map(|w| w.index), Some(0), "C starts as B closes");
            }
            let tex = texture(i);
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: i as u64 * 1_000_000_000 / u64::from(fps),
                format,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: tex,
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit_indexed(&frame, i as u32).expect("submit");
            let au = enc.poll().expect("poll").expect("an AU per submit (sync)");
            aus.push(au);
        }
        enc.flush().ok();
        assert_eq!(aus.len(), last + 1);
        assert!(enc.s.wave.is_none(), "wave C closed");
        for (i, au) in aus.iter().enumerate() {
            assert_eq!(au.keyframe, i == 0, "AU {i}: the only IDR is frame 0");
            let marks = [a_start, a_close, b_start, c_start, c_close];
            assert_eq!(
                au.recovery_point,
                marks.contains(&i),
                "AU {i}: marks on every start and close but the spoiled close {b_close}"
            );
            assert_eq!(au.recovery_anchor, i == anchor_p, "AU {i}: one anchor P");
            assert_eq!(
                au.recovery_close,
                i == a_close || i == c_close,
                "AU {i}: the close bit on every unspoiled close"
            );
        }
        let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
        let view = |lost: std::ops::Range<usize>| -> Vec<u8> {
            aus.iter()
                .enumerate()
                .filter(|(i, _)| !lost.contains(i))
                .flat_map(|(_, a)| a.data.iter().copied())
                .collect()
        };
        let dir = std::env::var("PUNKTFUNK_SMOKE_DIR").unwrap_or_else(|_| ".".into());
        std::fs::write(format!("{dir}/nvenc-wave.h265"), &full).expect("write");
        std::fs::write(format!("{dir}/nvenc-wave-dropA.h265"), view(1..3)).expect("write");
        std::fs::write(
            format!("{dir}/nvenc-wave-dropL.h265"),
            view(anchor_p - 1..anchor_p),
        )
        .expect("write");
        std::fs::write(
            format!("{dir}/nvenc-wave-dropP.h265"),
            view(anchor_p..anchor_p + 1),
        )
        .expect("write");
        std::fs::write(
            format!("{dir}/nvenc-wave-dropC.h265"),
            view(spoil_at - 2..spoil_at),
        )
        .expect("write");
        println!(
            "nvenc_wave_smoke: {} AUs, {} bytes; A {a_start}..={a_close}, anchor {anchor_p}, \
             B {b_start}..={b_close} spoiled at {spoil_at}, C {c_start}..={c_close}; wrote \
             {dir}/nvenc-wave{{,-dropA,-dropL,-dropP,-dropC}}.h265",
            aus.len(),
            full.len()
        );
    }
}

/// Losses on hardware, shaped and dumped by [`crate::smoke_pattern::Soak`]: RFI anchors,
/// or with `PF_WAVE_ACKED=1` the long-term references the client's confirmations drive.
///
/// `cargo test -p pf-encode-win --features nvenc --lib nvenc_ltr_soak -- --ignored --nocapture`
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
fn nvenc_ltr_soak() {
    use crate::{smoke_d3d11::nv12_scroll_frame, smoke_pattern::Soak};
    // SAFETY: DXGI factory creation borrows nothing and has no preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.expect("DXGI factory");
    let adapter = (0..)
        // SAFETY: `factory` outlives this closure and the call takes no lasting alias.
        .map_while(|i| unsafe { factory.EnumAdapters1(i) }.ok())
        // SAFETY: `a` is a live adapter the enumeration above just returned.
        .find(|a| unsafe { a.GetDesc1() }.is_ok_and(|d| d.VendorId == 0x10de))
        .expect("NVIDIA adapter");
    // SAFETY: `adapter` is live for the call, and `make_device` keeps no alias to it.
    let (device, _) = unsafe { pf_frame::dxgi::make_device(&adapter) }.expect("make_device");
    let soak = Soak::from_env();
    let mut enc = NvencD3d11Encoder::open(
        soak.codec,
        PixelFormat::Nv12,
        soak.w,
        soak.h,
        soak.fps,
        soak.mbps * 1_000_000,
        8,
        ChromaFormat::Yuv420,
        1,
        None,
    )
    .expect("NVENC open");
    enc.prepare_d3d11(&device, PixelFormat::Nv12, soak.w, soak.h)
        .expect("prepare");
    assert!(
        enc.caps().supports_rfi,
        "the RTX box invalidates references"
    );
    println!("nvenc_ltr_soak: {} long-term slots", enc.s.ltr_frames);
    let (w, h) = (soak.w, soak.h);
    let bind = D3D11_BIND_RENDER_TARGET.0 as u32;
    soak.run("nvenc", &mut enc, |i| {
        nv12_scroll_frame(&device, w, h, i, bind)
    });
}

/// The wave soak ([`crate::smoke_pattern::WaveSoak`], its `PF_WAVE_*` knobs) on the D3D11
/// session: a fresh texture per frame, 10-bit as R10G10B10A2. `wave-soak.ps1` maps each
/// close and the drift after it from the dumped view.
///
/// `cargo test -p pf-encode-win --features nvenc nvenc_wave_soak -- --ignored --nocapture`
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
fn nvenc_wave_soak() {
    use crate::smoke_pattern::scroll_pattern_rgb10;
    let soak = crate::smoke_pattern::WaveSoak::from_env();
    let (w, h, fps, ten_bit) = (soak.w, soak.h, soak.fps, soak.ten_bit);
    let (format, dxgi) = if ten_bit {
        (PixelFormat::Rgb10a2Sdr, DXGI_FORMAT_R10G10B10A2_UNORM)
    } else {
        (PixelFormat::Bgra, DXGI_FORMAT_B8G8R8A8_UNORM)
    };
    // SAFETY: as `nvenc_wave_smoke`: test-only COM calls on one thread, out-pointers
    // checked, every texture outlives the encoder call that reads it.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
        let adapter = (0..)
            .map_while(|i| factory.EnumAdapters1(i).ok())
            .find(|a| a.GetDesc1().is_ok_and(|d| d.VendorId == 0x10de))
            .expect("NVIDIA adapter");
        let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");
        let texture = |i: usize| {
            let bytes = if ten_bit {
                scroll_pattern_rgb10(w as usize, h as usize, i)
            } else {
                scroll_pattern(w as usize, h as usize, i)
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: bytes.as_ptr() as *const _,
                SysMemPitch: w * 4,
                SysMemSlicePitch: 0,
            };
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: dxgi,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut tex = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
                .expect("frame texture");
            tex.expect("null frame texture")
        };
        let mut enc = NvencD3d11Encoder::open(
            soak.codec,
            format,
            w,
            h,
            fps,
            soak.mbps * 1_000_000,
            if ten_bit { 10 } else { 8 },
            ChromaFormat::Yuv420,
            1,
            None,
        )
        .expect("NVENC open");
        enc.prepare_d3d11(&device, format, w, h).expect("prepare");
        soak.run(
            "nvenc",
            &mut enc,
            |e| &e.s,
            |i| CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i as u64 * 1_000_000_000 / u64::from(fps),
                format,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: texture(i),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            },
        );
    }
}

/// Encode 30 static pattern frames through a real NVENC session (ARGB, production config).
fn encode_pattern(chroma: ChromaFormat, path: &str) {
    const W: u32 = 1280;
    const H: u32 = 720;
    // SAFETY: test-only D3D11/DXGI COM calls on one thread; every out-pointer is checked
    // before use; the texture/device outlive the encoder.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
        let mut adapter = None;
        for i in 0.. {
            let Ok(a) = factory.EnumAdapters1(i) else {
                break;
            };
            let desc = a.GetDesc1().expect("adapter desc");
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                adapter = Some(a);
                break;
            }
        }
        let adapter = adapter.expect("no hardware DXGI adapter");
        let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");

        let bytes = probe_pattern(W as usize, H as usize);
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr() as *const _,
            SysMemPitch: W * 4,
            SysMemSlicePitch: 0,
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // NVENC registration requires RENDER_TARGET on D3D11 input textures.
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
            .expect("pattern texture");
        let tex = tex.expect("null pattern texture");

        let mut enc = NvencD3d11Encoder::open(
            Codec::H265,
            PixelFormat::Bgra,
            W,
            H,
            60,
            100_000_000, // high rate: the 1-px stripes must survive quantization
            8,
            chroma,
            1,
            None,
        )
        .expect("NVENC open");
        let mut out = Vec::new();
        for i in 0..30u64 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: i * 16_666_667,
                format: PixelFormat::Bgra,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit(&frame).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                out.extend_from_slice(&au.data);
            }
        }
        enc.flush().ok();
        while let Ok(Some(au)) = enc.poll() {
            out.extend_from_slice(&au.data);
        }
        assert!(!out.is_empty(), "no AUs produced");
        let caps444 = enc.caps().chroma_444;
        std::fs::write(path, &out).expect("write bitstream");
        println!(
            "wrote {path}: {} bytes, requested {chroma:?}, caps.chroma_444={caps444}",
            out.len()
        );
    }
}

/// An in-place rate retarget up and down emits no IDR
/// ([`crate::smoke_pattern::reconfigure_no_idr`]).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
fn nvenc_reconfigure_no_idr() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    const W: u32 = 1280;
    const H: u32 = 720;
    // SAFETY: test-only, same D3D11/DXGI setup as `encode_pattern`.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
        let mut adapter = None;
        for i in 0.. {
            let Ok(a) = factory.EnumAdapters1(i) else {
                break;
            };
            let desc = a.GetDesc1().expect("adapter desc");
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                adapter = Some(a);
                break;
            }
        }
        let adapter = adapter.expect("no hardware DXGI adapter");
        let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");

        let bytes = probe_pattern(W as usize, H as usize);
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr() as *const _,
            SysMemPitch: W * 4,
            SysMemSlicePitch: 0,
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
            .expect("pattern texture");
        let tex = tex.expect("null pattern texture");

        let mut enc = NvencD3d11Encoder::open(
            Codec::H265,
            PixelFormat::Bgra,
            W,
            H,
            60,
            20_000_000,
            8,
            ChromaFormat::Yuv420,
            1,
            None,
        )
        .expect("NVENC open");
        crate::smoke_pattern::reconfigure_no_idr(
            &mut enc,
            |i| CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: i as u64 * 16_666_667,
                format: PixelFormat::Bgra,
                payload: FramePayload::D3d11(D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            },
            &[60_000_000, 10_000_000],
        );
    }
}

/// Check that `nvEncReconfigureEncoder` accepts a changed `splitEncodeMode` with
/// `resetEncoder=0` and emits no IDR on D3D11. Also checks `query_caps` latches
/// `NUM_ENCODER_ENGINES` and that an over-ask (3-way split on a 2-engine card)
/// is why the clamp exists. Reports rather than asserts: both outcomes are findings.
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX Windows box"]
fn nvenc_split_reconfigure_in_place() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    const W: u32 = 1920;
    const H: u32 = 1080;
    const BPS: u64 = 40_000_000;
    let disable = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;
    let two = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

    // SAFETY: this ignored hardware test is run alone; no other thread touches the env.
    unsafe {
        std::env::set_var("PUNKTFUNK_NVENC_SUBFRAME", "0");
        std::env::set_var("PUNKTFUNK_SPLIT_ENCODE", "0");
    }

    // SAFETY: test-only, same D3D11/DXGI setup as `nvenc_reconfigure_no_idr`.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().expect("DXGI factory");
        let mut adapter = None;
        for i in 0.. {
            let Ok(a) = factory.EnumAdapters1(i) else {
                break;
            };
            if a.GetDesc1().expect("adapter desc").Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0
            {
                adapter = Some(a);
                break;
            }
        }
        let adapter = adapter.expect("no hardware DXGI adapter");
        let (device, _ctx) = pf_frame::dxgi::make_device(&adapter).expect("make_device");
        let bytes = probe_pattern(W as usize, H as usize);
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: bytes.as_ptr() as *const _,
            SysMemPitch: W * 4,
            SysMemSlicePitch: 0,
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex = None;
        device
            .CreateTexture2D(&desc, Some(&init), Some(&mut tex))
            .expect("pattern texture");
        let tex = tex.expect("null pattern texture");

        let mut enc = NvencD3d11Encoder::open(
            Codec::H265,
            PixelFormat::Bgra,
            W,
            H,
            60,
            BPS,
            8,
            ChromaFormat::Yuv420,
            1,
            None,
        )
        .expect("NVENC open");

        let submit_and_poll = |enc: &mut NvencD3d11Encoder, range: std::ops::Range<u64>| {
            let (mut aus, mut keyframes) = (0usize, 0usize);
            for i in range {
                let frame = CapturedFrame {
                    provenance: Default::default(),
                    width: W,
                    height: H,
                    pts_ns: i * 16_666_667,
                    format: PixelFormat::Bgra,
                    payload: FramePayload::D3d11(D3d11Frame {
                        texture: tex.clone(),
                        device: device.clone(),
                        pyro: None,
                    }),
                    cursor: None,
                };
                enc.submit_indexed(&frame, i as u32).expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    aus += 1;
                    keyframes += au.keyframe as usize;
                }
            }
            (aus, keyframes)
        };

        let (aus, kfs) = submit_and_poll(&mut enc, 0..6);
        assert!(aus > 0 && kfs == 1, "opening IDR then steady P-frames");
        println!(
            "S1(win): engines={} (latched by query_caps), opened split_mode={}",
            enc.s.encoder_engines, enc.s.split_mode
        );
        assert!(
            enc.s.encoder_engines >= 2,
            "this GPU reports {} NVENC engine(s) — S1 is not interpretable here",
            enc.s.encoder_engines
        );
        assert_eq!(enc.s.split_mode, disable, "must open split-disabled");

        // Change only splitEncodeMode, in place, same bitrate.
        enc.s.split_mode = two;
        let accepted = enc.reconfigure_bitrate(BPS);
        println!("S1(win): reconfigure DISABLE→TWO_FORCED accepted = {accepted}");
        if accepted {
            let (aus, kfs) = submit_and_poll(&mut enc, 6..12);
            assert!(aus > 0, "no AUs after the accepted reconfigure");
            println!(
                "S1(win) VERDICT: {}",
                if kfs == 0 {
                    "PASS — accepted with NO IDR on D3D11: Windows arbitration is buildable"
                } else {
                    "FAIL — accepted but forced an IDR, which is the same as a rejection"
                }
            );
            enc.s.split_mode = disable;
            let back = enc.reconfigure_bitrate(BPS);
            println!("S1(win): reverse accepted = {back}");
        } else {
            enc.s.split_mode = disable;
            println!(
                "S1(win) VERDICT: FAIL — the D3D11 path REFUSES an in-place split change. \
                 Windows arbitration is not buildable; the Linux result does not transfer."
            );
        }
        enc.flush().ok();
    }

    // SAFETY: single-threaded manual test; no concurrent env access.
    unsafe {
        std::env::remove_var("PUNKTFUNK_SPLIT_ENCODE");
        std::env::remove_var("PUNKTFUNK_NVENC_SUBFRAME");
    }
}

/// Encode the probe pattern as FREXT 4:4:4 and as 4:2:0 so offline analysis can tell whether
/// the FREXT stream is full-chroma and which matrix the RGB→YUV CSC used (BT.601 vs BT.709).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box"]
fn nvenc_444_on_glass_probe() {
    encode_pattern(
        ChromaFormat::Yuv444,
        "C:\\Users\\Public\\nvenc444_probe.h265",
    );
    encode_pattern(
        ChromaFormat::Yuv420,
        "C:\\Users\\Public\\nvenc420_probe.h265",
    );
}

/// Codec-advertisement probe against the real driver. Every NVENC GPU encodes H.264
/// (`false` means enumeration is broken). Must be stable: one cached answer drives
/// every negotiation. `--release` is required on Windows: debug `/OPT:NOREF` keeps
/// the sdk crate's unused lazy loader and its NvEncodeAPI imports (LNK2019).
#[test]
#[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.173)"]
fn nvenc_codec_probe_reports_real_gpu_support() {
    let caps = probe_codec_support(None);
    eprintln!(
        "NVENC (Windows) probe: h264={} h265={} av1={}",
        caps.h264, caps.h265, caps.av1
    );
    assert!(
        caps.h264,
        "every NVENC generation encodes H.264 — a false here means the GUID enumeration \
         failed, which would narrow the host's codec advertisement"
    );
    let again = probe_codec_support(None);
    assert_eq!(
        (caps.h264, caps.h265, caps.av1),
        (again.h264, again.h265, again.av1),
        "the probe must be stable — it is cached once and drives every later negotiation"
    );
}
