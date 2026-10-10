//! Gate probes for `design/intel-windows-zero-copy-conversion.md` §4, run by hand on an Intel
//! box: `enc-tests.exe --ignored gate_ --nocapture --test-threads 1`. They print what the
//! hardware did and write streams the rig decodes (`qsv-levels.py`). The two `qsv_live_` tests
//! hold what passed: profile, chroma, depth, signalling, and that the texture is read in place.
//!
//! The picture: eight bars in the top half, the same bars reversed below, a black band in the
//! last 16 rows. Bar 7 is a grey that steps per frame, so the decoded order shows; the seam
//! shows a vertical chroma shift, the band what the encoder made of rows past the texture.

use super::*;
use crate::convert::{HdrP010Converter, VideoConverter};
use std::path::Path;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

const FRAMES: u32 = 10;
const RT_SRV: u32 = (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32;

/// A device on the first Intel VPL adapter; `None` skips.
fn intel_device() -> Option<ID3D11Device> {
    let (_l, impls) = intel_loader().ok()?;
    let imp = impls.iter().find(|i| i.luid_valid)?;
    let luid = LUID {
        LowPart: u32::from_le_bytes(imp.luid[..4].try_into().ok()?),
        HighPart: i32::from_le_bytes(imp.luid[4..].try_into().ok()?),
    };
    // SAFETY: test-only COM on one thread; `EnumAdapterByLuid` gets the LUID the runtime
    // reported, and `D3D11CreateDevice` fills `device` only on success.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).ok()?;
        let mut device = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            Default::default(),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .ok()?;
        device
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("pf_encode=debug,pf_encode_win=debug")
        .with_test_writer()
        .try_init();
}

/// ST 2084 PQ of `nits`, 0..1.
fn pq(nits: f64) -> f64 {
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let y = (nits.max(0.0) / 10000.0).powf(m1);
    ((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)
}

/// SDR bars, 8-bit R'G'B'. `qsv-levels.py` holds the same tables.
fn bars_sdr(frame: u32) -> [[u32; 3]; 8] {
    let g = 16 + 24 * frame;
    [
        [255, 255, 255],
        [191, 191, 0],
        [0, 191, 191],
        [0, 191, 0],
        [191, 0, 191],
        [191, 0, 0],
        [0, 0, 191],
        [g, g, g],
    ]
}

/// HDR bars, 10-bit BT.2020 PQ R'G'B': greys at 20/80/320/1000 nits, then the primaries.
fn bars_hdr(frame: u32) -> [[u32; 3]; 8] {
    let c = |n: f64| (pq(n) * 1023.0).round() as u32;
    let g = 64 + 80 * frame;
    [
        [c(20.0); 3],
        [c(80.0); 3],
        [c(320.0); 3],
        [c(1000.0); 3],
        [600, 0, 0],
        [0, 600, 0],
        [0, 0, 600],
        [g, g, g],
    ]
}

/// FP16 scRGB bars (1.0 = 80 nits): the same greys, then the BT.709 primaries at 80 nits.
fn bars_scrgb(frame: u32) -> [[f32; 3]; 8] {
    let g = 0.125 * (frame + 1) as f32;
    [
        [0.25; 3],
        [1.0; 3],
        [4.0; 3],
        [12.5; 3],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
        [g; 3],
    ]
}

/// f32 → f16 bits for zero and positive normal values, which is all the bars hold.
fn f16_bits(v: f32) -> u16 {
    if v == 0.0 {
        return 0;
    }
    let b = v.to_bits();
    let exp = ((b >> 23) & 0xff) as i32 - 127 + 15;
    ((exp as u16) << 10) | ((b >> 13) & 0x3ff) as u16
}

/// BT.709 limited 8-bit Y'CbCr of 8-bit R'G'B'.
fn ycbcr709(rgb: [u32; 3]) -> [u32; 3] {
    let [r, g, b] = rgb.map(|c| c as f64 / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    [
        16.0 + 219.0 * y,
        128.0 + 224.0 * (b - y) / 1.8556,
        128.0 + 224.0 * (r - y) / 1.5748,
    ]
    .map(|v| v.round() as u32)
}

/// BT.2020 non-constant-luminance limited 10-bit Y'CbCr of 10-bit R'G'B'.
fn ycbcr2020(rgb: [u32; 3]) -> [u32; 3] {
    let [r, g, b] = rgb.map(|c| c as f64 / 1023.0);
    let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
    [
        64.0 + 876.0 * y,
        512.0 + 896.0 * (b - y) / 1.8814,
        512.0 + 896.0 * (r - y) / 1.4746,
    ]
    .map(|v| v.round() as u32)
}

/// Which bar covers `(x, y)`; `None` in the black band.
fn bar_at(x: u32, y: u32, (w, h): (u32, u32)) -> Option<usize> {
    if y >= h - 16 {
        return None;
    }
    let k = ((x * 8) / w).min(7) as usize;
    Some(if y < h / 2 { k } else { 7 - k })
}

fn texture(
    device: &ID3D11Device,
    format: DXGI_FORMAT,
    (w, h): (u32, u32),
    bind: u32,
    init: Option<(&[u8], u32)>,
) -> ID3D11Texture2D {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = init.map(|(bytes, pitch)| D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr().cast(),
        SysMemPitch: pitch,
        SysMemSlicePitch: 0,
    });
    let mut t = None;
    // SAFETY: a complete description; `data` points into `init`, which outlives the call.
    unsafe { device.CreateTexture2D(&desc, data.as_ref().map(|d| d as *const _), Some(&mut t)) }
        .expect("texture");
    t.expect("texture")
}

/// One frame of the picture in `format`.
fn picture(
    device: &ID3D11Device,
    format: PixelFormat,
    size: (u32, u32),
    frame: u32,
    bind: u32,
) -> ID3D11Texture2D {
    let (w, h) = size;
    let mut px: Vec<u8> = Vec::new();
    let (dxgi, pitch) = match format {
        PixelFormat::Bgra => {
            let b = bars_sdr(frame);
            for y in 0..h {
                for x in 0..w {
                    let [r, g, bl] = bar_at(x, y, size).map_or([0; 3], |k| b[k]);
                    px.extend([bl as u8, g as u8, r as u8, 255]);
                }
            }
            (DXGI_FORMAT_B8G8R8A8_UNORM, w * 4)
        }
        PixelFormat::RgbaF16 => {
            let b = bars_scrgb(frame);
            for y in 0..h {
                for x in 0..w {
                    let [r, g, bl] = bar_at(x, y, size).map_or([0.0; 3], |k| b[k]);
                    for c in [r, g, bl, 1.0] {
                        px.extend(f16_bits(c).to_le_bytes());
                    }
                }
            }
            (DXGI_FORMAT_R16G16B16A16_FLOAT, w * 8)
        }
        PixelFormat::Nv12 => {
            let b = bars_sdr(frame).map(ycbcr709);
            let black = ycbcr709([0; 3]);
            let at = |x, y| bar_at(x, y, size).map_or(black, |k| b[k]);
            for y in 0..h {
                for x in 0..w {
                    px.push(at(x, y)[0] as u8);
                }
            }
            for cy in 0..h / 2 {
                for cx in 0..w / 2 {
                    let [_, cb, cr] = at(cx * 2, cy * 2);
                    px.extend([cb as u8, cr as u8]);
                }
            }
            (DXGI_FORMAT_NV12, w)
        }
        PixelFormat::P010 => {
            let b = bars_hdr(frame).map(ycbcr2020);
            let black = ycbcr2020([0; 3]);
            let at = |x, y| bar_at(x, y, size).map_or(black, |k| b[k]);
            for y in 0..h {
                for x in 0..w {
                    px.extend(((at(x, y)[0] << 6) as u16).to_le_bytes());
                }
            }
            for cy in 0..h / 2 {
                for cx in 0..w / 2 {
                    let [_, cb, cr] = at(cx * 2, cy * 2);
                    px.extend(((cb << 6) as u16).to_le_bytes());
                    px.extend(((cr << 6) as u16).to_le_bytes());
                }
            }
            (DXGI_FORMAT_P010, w * 2)
        }
        _ => unreachable!("not a probed input"),
    };
    texture(device, dxgi, size, bind, Some((&px, pitch)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feed {
    /// Today's path: a copy into one of the runtime's surfaces.
    Runtime,
    /// Our allocator, a copy into our ring at the coded size.
    Copy,
    /// Our allocator, the caller's texture where it lies.
    InPlace,
    /// In place with shader-resource binds only, as a swap-chain surface carries.
    Swapchain,
}

/// What a survey stream starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Src {
    /// Frames in this format, copied into the ring.
    Plain(PixelFormat),
    /// BGRA through the video processor into P010 under BT.709 (`EncodeInput::P010Sdr`).
    VpSdr10,
}

/// Profile, chroma format idc, luma depth and colour from the first AU's SPS (H.264/HEVC).
fn sps(
    codec: Codec,
    aus: &[EncodedFrame],
) -> Option<(u8, u8, u8, pf_bitstream::h264::ColourDescription)> {
    let first = &aus.first()?.data;
    match codec {
        Codec::H264 => pf_bitstream::h264::H264Planner::new()
            .plan_au(first)
            .ok()
            .map(|p| {
                let p = p.picture;
                let depth = p.bit_depth_luma_minus8 + 8;
                (p.profile_idc, p.chroma_format_idc, depth, p.colour)
            }),
        Codec::H265 => pf_bitstream::h265::H265Planner::new()
            .plan_au(first)
            .ok()
            .map(|p| {
                let p = p.picture;
                let depth = p.bit_depth_luma_minus8 + 8;
                (p.general_profile_idc, p.chroma_format_idc, depth, p.colour)
            }),
        _ => None,
    }
}

/// What one encode left: its AUs, and (allocator, textures with a `MemId`, copy-ring length).
type Encoded = (Vec<EncodedFrame>, Option<(bool, usize, usize)>);

/// Ten frames through a three-texture ring, as the driver's pool feeds it. `Err` names the
/// step the encoder refused.
fn encode(
    device: &ID3D11Device,
    codec: Codec,
    src: Src,
    size: (u32, u32),
    feed: Feed,
) -> std::result::Result<Encoded, String> {
    let (format, depth, hdr) = match src {
        Src::Plain(PixelFormat::P010) => (PixelFormat::P010, 10, true),
        Src::Plain(f) => (f, 8, false),
        Src::VpSdr10 => (PixelFormat::P010, 10, false),
    };
    let mut enc = QsvEncoder::open(
        codec,
        format,
        size.0,
        size.1,
        30,
        8_000_000,
        depth,
        ChromaFormat::Yuv420,
        hdr,
        None,
    )
    .map_err(|e| format!("open refused: {e:#}"))?;
    enc.runtime_surfaces = feed == Feed::Runtime;
    if matches!(feed, Feed::InPlace | Feed::Swapchain) {
        enc.set_input_ring_depth(2);
    }
    if hdr {
        enc.set_hdr_meta(Some(pf_frame::HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 500,
            max_cll: 1000,
            max_fall: 400,
        }));
    }
    enc.prepare(device)
        .map_err(|e| format!("Init refused: {e:#}"))?;
    let bind = if feed == Feed::Swapchain {
        D3D11_BIND_SHADER_RESOURCE.0 as u32
    } else {
        RT_SRV
    };
    let ring: Vec<_> = (0..3)
        .map(|_| picture(device, format, size, 0, bind))
        .collect();
    // SAFETY: the device is live on this thread.
    let ctx = unsafe { device.GetImmediateContext() }.expect("context");
    let vp = (src == Src::VpSdr10)
        .then(|| VideoConverter::new(device, &ctx, size.0, size.1, false).expect("VP"));
    let mut out: Vec<EncodedFrame> = Vec::new();
    for i in 0..FRAMES {
        let slot = &ring[i as usize % ring.len()];
        // With depth 2 and three textures the slot's last frame is synced before it is written.
        match &vp {
            Some(vp) => {
                let bgra = picture(device, PixelFormat::Bgra, size, i, RT_SRV);
                vp.convert(&bgra, slot).expect("VP convert");
            }
            None => {
                let src = picture(device, format, size, i, 0);
                // SAFETY: same device, size and format.
                unsafe { ctx.CopyResource(slot, &src) };
            }
        }
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: size.0,
            height: size.1,
            pts_ns: i as u64 * 33_333_333,
            format,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: slot.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit_indexed(&frame, i)
            .map_err(|e| format!("submit {i} refused: {e:#}"))?;
        while let Some(au) = enc.poll().expect("poll") {
            out.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("drain") {
        out.push(au);
    }
    let own = enc
        .inner
        .as_ref()
        .map(|i| (i.alloc.is_some(), i.mem_ids.len(), i.ring.len()));
    Ok((out, own))
}

/// [`encode`], with the stream written to `dir` and a line printed either way.
fn survey_one(
    device: &ID3D11Device,
    dir: &Path,
    codec: Codec,
    src: Src,
    size: (u32, u32),
    feed: Feed,
) {
    let name = match src {
        Src::Plain(f) => format!("{f:?}-{}", if f == PixelFormat::P010 { 10 } else { 8 }),
        Src::VpSdr10 => "P010Sdr-10".into(),
    };
    let tag = format!("{codec:?}-{name}-{feed:?}-{}x{}", size.0, size.1);
    let started = std::time::Instant::now();
    let (aus, own) = match encode(device, codec, src, size, feed) {
        Ok(x) => x,
        Err(e) => return println!("gate {tag}: {e}"),
    };
    let ext = match codec {
        Codec::H264 => "h264",
        Codec::H265 => "h265",
        _ => "obu",
    };
    let bytes: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
    std::fs::write(dir.join(format!("{tag}.{ext}")), &bytes).expect("write stream");
    println!(
        "gate {tag}: {} AUs in {:?}, {} bytes, (allocator, textures, copy ring) {own:?}, \
         (profile, chroma, depth, colour) {:?}",
        aus.len(),
        started.elapsed(),
        bytes.len(),
        sps(codec, &aus)
    );
}

/// What G1 to G3 found, held: BGRA read in place at the display size (1080 rows, swap-chain
/// binds) encodes 8-bit 4:2:0 with BT.709 signalled, on H.264 and HEVC. A runtime that
/// answers RGB with a 4:4:4 stream, or copies instead of reading in place, fails here. The
/// decoded levels are the rig's (`qsv-levels.py`).
#[test]
fn qsv_live_bgra_reads_in_place_as_bt709_420() {
    let Some(device) = intel_device() else {
        eprintln!("skipping: no Intel adapter");
        return;
    };
    for (codec, profile) in [(Codec::H264, 100), (Codec::H265, 1)] {
        let (aus, own) = encode(
            &device,
            codec,
            Src::Plain(PixelFormat::Bgra),
            (1920, 1080),
            Feed::Swapchain,
        )
        .unwrap_or_else(|e| panic!("{codec:?}: {e}"));
        assert_eq!(aus.len(), FRAMES as usize, "{codec:?}: one AU per frame");
        assert_eq!(own, Some((true, 3, 0)), "{codec:?}: read in place, no copy");
        let (got_profile, chroma, depth, colour) = sps(codec, &aus).expect("SPS");
        assert_eq!((got_profile, chroma, depth), (profile, 1, 8), "{codec:?}");
        assert_eq!(
            (colour.colour_primaries, colour.matrix_coefficients),
            (1, 1),
            "{codec:?}: BT.709"
        );
    }
}

/// 10-bit SDR: the video processor's BT.709 P010 encodes HEVC Main10 signalled BT.709, not
/// the BT.2020 PQ an HDR P010 carries.
#[test]
fn qsv_live_p010_sdr_signals_bt709() {
    let Some(device) = intel_device() else {
        eprintln!("skipping: no Intel adapter");
        return;
    };
    if !probe_can_encode_10bit(Codec::H265, None) {
        eprintln!("skipping: this GPU declines 10-bit HEVC");
        return;
    }
    let (aus, own) = encode(
        &device,
        Codec::H265,
        Src::VpSdr10,
        (1920, 1080),
        Feed::InPlace,
    )
    .expect("encode");
    assert_eq!(aus.len(), FRAMES as usize);
    assert_eq!(own, Some((true, 3, 0)), "read in place, no copy");
    let (profile, chroma, depth, colour) = sps(Codec::H265, &aus).expect("SPS");
    assert_eq!((profile, chroma, depth), (2, 1, 10), "Main10 4:2:0");
    assert_eq!(
        (
            colour.colour_primaries,
            colour.transfer_characteristics,
            colour.matrix_coefficients
        ),
        (1, 1, 1),
        "BT.709"
    );
}

/// G1 and G2 at 640×480, G3 at 1080 and 1800 rows, and the video processor's BT.709 P010 for
/// 10-bit SDR. Streams land in `%TEMP%\pf_qsv_gates` for `qsv-levels.py`. A2RGB10 (G4) and
/// FP16 (G6) are out of `fourcc` since their first run; the plan holds those results.
#[test]
#[ignore = "gate probe: needs an Intel GPU, prints, writes streams"]
fn gate_input_survey_live() {
    use PixelFormat::{Bgra, Nv12, P010};
    init_tracing();
    let Some(device) = intel_device() else {
        eprintln!("skipping: no Intel adapter");
        return;
    };
    let dir = std::env::temp_dir().join("pf_qsv_gates");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("stream dir");
    let vga = (640, 480);
    for codec in [Codec::H264, Codec::H265, Codec::Av1] {
        for src in [
            Src::Plain(Nv12),
            Src::Plain(Bgra),
            Src::Plain(P010),
            Src::VpSdr10,
        ] {
            if codec == Codec::H264 && matches!(src, Src::Plain(P010) | Src::VpSdr10) {
                continue;
            }
            for feed in [Feed::Runtime, Feed::Copy, Feed::InPlace] {
                survey_one(&device, &dir, codec, src, vga, feed);
            }
        }
    }
    for size in [(1920, 1080), (2880, 1800)] {
        for src in [
            Src::Plain(Nv12),
            Src::Plain(Bgra),
            Src::Plain(P010),
            Src::VpSdr10,
        ] {
            for feed in [Feed::Copy, Feed::Swapchain] {
                // A video-processor output is a render target, never a swap-chain surface.
                if src == Src::VpSdr10 && feed == Feed::Swapchain {
                    continue;
                }
                survey_one(&device, &dir, Codec::H265, src, size, feed);
            }
        }
    }
}

/// A P010 texture's Y plane, then its interleaved CbCr rows, as 10-bit codes.
fn read_p010(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    tex: &ID3D11Texture2D,
    (w, h): (u32, u32),
) -> Vec<u16> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_P010,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut stage = None;
    // SAFETY: a complete staging description; the copy and map run on the device's own
    // immediate context, and every read stays inside the mapped rows (CbCr at RowPitch × h).
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut stage))
            .expect("staging");
        let stage: ID3D11Texture2D = stage.expect("staging");
        ctx.CopyResource(&stage, tex);
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&stage, 0, D3D11_MAP_READ, 0, Some(&mut m))
            .expect("map");
        let mut out = Vec::with_capacity((w * h * 3 / 2) as usize);
        for row in 0..h + h / 2 {
            let p = (m.pData as *const u8).add((row * m.RowPitch) as usize) as *const u16;
            out.extend(
                std::slice::from_raw_parts(p, w as usize)
                    .iter()
                    .map(|v| v >> 6),
            );
        }
        ctx.Unmap(&stage, 0);
        out
    }
}

/// G5: the video processor's FP16 scRGB → P010 PQ against the shader's, bar by bar. With
/// `PF_GATE_SECS` it then runs the VP convert at 4K60 that long, for the rig's engine sampler.
#[test]
#[ignore = "gate probe: needs a GPU, prints"]
fn gate_vp_hdr10_live() {
    init_tracing();
    let Some(device) = intel_device() else {
        eprintln!("skipping: no Intel adapter");
        return;
    };
    // SAFETY: the device is live on this thread.
    let ctx = unsafe { device.GetImmediateContext() }.expect("context");
    let size = (640, 480);
    let (w, h) = size;
    let src = picture(&device, PixelFormat::RgbaF16, size, 0, RT_SRV);
    let vp_out = texture(&device, DXGI_FORMAT_P010, size, RT_SRV, None);
    let sh_out = texture(&device, DXGI_FORMAT_P010, size, RT_SRV, None);
    let vp = match VideoConverter::new_hdr10(&device, &ctx, w, h) {
        Ok(c) => c,
        Err(e) => {
            println!("gate G5: {e:#}");
            return;
        }
    };
    println!("gate G5: CheckVideoProcessorFormatConversion says yes");
    vp.convert(&src, &vp_out).expect("VP convert");
    let shader = HdrP010Converter::new(&device, w, h).expect("shader");
    let y = HdrP010Converter::plane_rtv(&device, &sh_out, DXGI_FORMAT_R16_UNORM).expect("Y");
    let uv = HdrP010Converter::plane_rtv(&device, &sh_out, DXGI_FORMAT_R16G16_UNORM).expect("UV");
    let mut srv = None;
    // SAFETY: `src` is a live shader-resource texture on `device`; `srv` a valid out-param.
    unsafe { device.CreateShaderResourceView(&src, None, Some(&mut srv)) }.expect("srv");
    shader
        .convert(&ctx, &srv.expect("srv"), &y, &uv, w, h)
        .expect("shader convert");
    let (a, b) = (
        read_p010(&device, &ctx, &vp_out, size),
        read_p010(&device, &ctx, &sh_out, size),
    );
    let px = |p: &[u16], x: u32, y: u32| {
        let c = ((h + y / 2) * w + (x / 2) * 2) as usize;
        [p[(y * w + x) as usize], p[c], p[c + 1]]
    };
    let mut worst = 0;
    for k in 0..8 {
        let (x, y) = (k * w / 8 + w / 16, h / 4);
        let (va, vb) = (px(&a, x, y), px(&b, x, y));
        let d = (0..3).map(|i| va[i].abs_diff(vb[i])).max().unwrap_or(0);
        worst = worst.max(d);
        println!("gate G5 bar {k}: VP {va:?} shader {vb:?}");
    }
    println!("gate G5: worst difference {worst} code values of 1023");
    let secs: u64 = std::env::var("PF_GATE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if secs == 0 {
        return;
    }
    let big = (3840, 2160);
    let src = picture(&device, PixelFormat::RgbaF16, big, 0, RT_SRV);
    let out = texture(&device, DXGI_FORMAT_P010, big, RT_SRV, None);
    let vp = VideoConverter::new_hdr10(&device, &ctx, big.0, big.1).expect("VP at 4K");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut n = 0u32;
    while std::time::Instant::now() < end {
        vp.convert(&src, &out).expect("VP convert");
        // SAFETY: a single call on the live immediate context.
        unsafe { ctx.Flush() };
        n += 1;
        std::thread::sleep(std::time::Duration::from_micros(16_667));
    }
    println!("gate G5 cost: {n} VP converts at 4K");
}

/// Per-frame cost at 4K60 HEVC for one input and feed (`PF_GATE_FMT` nv12|p010|bgra,
/// `PF_GATE_FEED` runtime|copy|inplace, `PF_GATE_SECS`): submit-to-AU p50/p99 while the rig
/// samples each engine's load by process.
#[test]
#[ignore = "gate probe: needs an Intel GPU, prints"]
fn gate_cost_live() {
    init_tracing();
    let Some(device) = intel_device() else {
        eprintln!("skipping: no Intel adapter");
        return;
    };
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
    let (format, depth) = match var("PF_GATE_FMT", "nv12").as_str() {
        "p010" => (PixelFormat::P010, 10),
        "bgra" => (PixelFormat::Bgra, 8),
        _ => (PixelFormat::Nv12, 8),
    };
    let feed = match var("PF_GATE_FEED", "runtime").as_str() {
        "copy" => Feed::Copy,
        "inplace" => Feed::InPlace,
        _ => Feed::Runtime,
    };
    let secs: u64 = var("PF_GATE_SECS", "10").parse().unwrap_or(10);
    let size = (3840, 2160);
    let mut enc = QsvEncoder::open(
        Codec::H265,
        format,
        size.0,
        size.1,
        60,
        40_000_000,
        depth,
        ChromaFormat::Yuv420,
        depth == 10,
        None,
    )
    .expect("open");
    enc.runtime_surfaces = feed == Feed::Runtime;
    if feed == Feed::InPlace {
        enc.set_input_ring_depth(2);
    }
    enc.prepare(&device).expect("prepare");
    let ring: Vec<_> = (0..3)
        .map(|i| picture(&device, format, size, i, RT_SRV))
        .collect();
    let start = std::time::Instant::now();
    let mut sent = std::collections::HashMap::new();
    let mut lat = Vec::new();
    let mut i = 0u32;
    while start.elapsed().as_secs() < secs {
        let pts = i as u64 * 16_666_667;
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: size.0,
            height: size.1,
            pts_ns: pts,
            format,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: ring[i as usize % ring.len()].clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        sent.insert(pts, std::time::Instant::now());
        enc.submit_indexed(&frame, i).expect("submit");
        // Until this frame's own AU, so the time is submit to AU and not a poll budget.
        let give_up = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while sent.contains_key(&pts) && std::time::Instant::now() < give_up {
            let Some(au) = enc.poll().expect("poll") else {
                continue;
            };
            if let Some(t) = sent.remove(&au.pts_ns) {
                lat.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        i += 1;
        let next = start + std::time::Duration::from_nanos(i as u64 * 16_666_667);
        std::thread::sleep(next.saturating_duration_since(std::time::Instant::now()));
    }
    lat.sort_by(f64::total_cmp);
    let q = |p: f64| lat.get(((lat.len() as f64 - 1.0) * p) as usize).copied();
    println!(
        "gate cost {format:?} {feed:?}: {} frames, submit-to-AU p50 {:?} ms p99 {:?} ms",
        lat.len(),
        q(0.5),
        q(0.99)
    );
}
