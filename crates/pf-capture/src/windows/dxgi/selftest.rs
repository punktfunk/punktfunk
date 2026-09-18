//! GPU colour check for [`HdrP010Converter`]: the `hdr-p010-selftest` subcommand,
//! the f64 analogue of its HLSL, and the f16 encoder that uploads the pattern.
//!
//! Does not run in a session. Pin with `punktfunk-host hdr-p010-selftest WxH [vendor]`
//! at a session size (1080 is not 16-aligned — encoder align16 pool seam). `vendor`
//! is a PCI id (`0x8086` Intel / `0x10de` NVIDIA / `0x1002` AMD); a PASS is only
//! for that GPU.
//!
//! [`hdr_p010_selftest_at`] and [`hdr_p010_convert_bars_on_luid`] are re-exported
//! by the parent so `crate::capture::dxgi::…` / `pf_capture::dxgi::…` keep resolving.
//! Evidence: `f16_tests`, ignored `hdr_p010_selftest_intel_1080_live`,
//! `docs-site/content/docs/hdr.md`.

use anyhow::{bail, Context, Result};
use pf_encode_win::convert::HdrP010Converter;
use std::ffi::c_void;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_P010, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_SAMPLE_DESC,
};

/// f64 analogue of the HLSL in `HDR_P010_COMMON`.
/// One scRGB pixel in (linear Rec.709, 1.0 = 80 nits, HDR may exceed 1.0);
/// out is 10-bit studio-range (Y, Cb, Cr) for a flat block.
#[cfg(target_os = "windows")]
fn p010_reference(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    fn pq_oetf(l: f64) -> f64 {
        let l = l.clamp(0.0, 1.0);
        let m1 = 0.1593017578125;
        let m2 = 78.84375;
        let c1 = 0.8359375;
        let c2 = 18.8515625;
        let c3 = 18.6875;
        let lp = l.powf(m1);
        ((c1 + c2 * lp) / (1.0 + c3 * lp)).powf(m2)
    }
    // scRGB -> nits -> BT.2020 linear (row-major matrix, mul(M, v)); pq_oetf clamps after.
    let (r, g, b) = (r * 80.0, g * 80.0, b * 80.0);
    let m = [
        [0.627403914, 0.329283038, 0.043313048],
        [0.069097292, 0.919540405, 0.011362303],
        [0.016391439, 0.088013308, 0.895595253],
    ];
    let lr = m[0][0] * r + m[0][1] * g + m[0][2] * b;
    let lg = m[1][0] * r + m[1][1] * g + m[1][2] * b;
    let lb = m[2][0] * r + m[2][1] * g + m[2][2] * b;
    // PQ encode (normalize to 10k nits).
    let pr = pq_oetf(lr / 10000.0);
    let pg = pq_oetf(lg / 10000.0);
    let pb = pq_oetf(lb / 10000.0);
    // BT.2020 non-constant-luminance, limited 10-bit.
    let (kr, kg, kb) = (0.2627, 0.6780, 0.0593);
    let y = kr * pr + kg * pg + kb * pb;
    let cb = (pb - y) / 1.8814;
    let cr = (pr - y) / 1.4746;
    let yc = (64.0 + 876.0 * y).clamp(64.0, 940.0);
    let cbc = (512.0 + 896.0 * cb).clamp(64.0, 960.0);
    let crc = (512.0 + 896.0 * cr).clamp(64.0, 960.0);
    (yc, cbc, crc)
}

/// Colour-bar P010 on `luid` with the IDD out-ring bind profile:
/// `BIND_RENDER_TARGET` only, `MiscFlags` 0. CPU-upload encoder tests cannot
/// use that profile; this is the RTV-written P010 → encoder ingest path.
/// Eight sRGB bars (1.0 = 80 nits). Expected 10-bit codes:
/// (490,512,512) (478,423,518) (464,525,473) (450,432,476) (350,584,585)
/// (325,448,598) (226,650,535) (64,512,512).
#[cfg(target_os = "windows")]
#[doc(hidden)]
pub fn hdr_p010_convert_bars_on_luid(
    luid: [u8; 8],
    w: u32,
    h: u32,
) -> Result<(ID3D11Device, ID3D11Texture2D)> {
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        bail!("bars pattern needs even non-zero dimensions, got {w}x{h}");
    }
    // EOTF(1)=1 and EOTF(0)=0, so 0/1 scRGB bars match the PQ/BT.2020 codes exactly.
    const BARS: [(f32, f32, f32); 8] = [
        (1.0, 1.0, 1.0),
        (1.0, 1.0, 0.0),
        (0.0, 1.0, 1.0),
        (0.0, 1.0, 0.0),
        (1.0, 0.0, 1.0),
        (1.0, 0.0, 0.0),
        (0.0, 0.0, 1.0),
        (0.0, 0.0, 0.0),
    ];
    let bar_w = (w / 8).max(1) as usize;
    let mut fp16 = vec![0u16; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let (r, g, b) = BARS[(x / bar_w).min(7)];
            let i = (y * w as usize + x) * 4;
            fp16[i] = f32_to_f16(r);
            fp16[i + 1] = f32_to_f16(g);
            fp16[i + 2] = f32_to_f16(b);
            fp16[i + 3] = f32_to_f16(1.0);
        }
    }
    // SAFETY: same single-device/single-thread contract as `hdr_p010_selftest_at`; the FP16
    // initial-data Vec outlives the synchronous CreateTexture2D; the returned COM handles own
    // their references.
    unsafe {
        let luid = windows::Win32::Foundation::LUID {
            LowPart: u32::from_le_bytes(luid[..4].try_into().unwrap()),
            HighPart: i32::from_le_bytes(luid[4..].try_into().unwrap()),
        };
        let factory: IDXGIFactory4 = CreateDXGIFactory1().context("dxgi factory")?;
        let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).context("adapter by luid")?;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice(luid) for bars convert")?;
        let device = device.context("null device")?;
        let context = context.context("null context")?;

        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: fp16.as_ptr() as *const c_void,
            SysMemPitch: w * 8,
            SysMemSlicePitch: 0,
        };
        let mut src_tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex))
            .context("CreateTexture2D(fp16 bars)")?;
        let src_tex = src_tex.context("null src tex")?;
        let mut src_srv: Option<ID3D11ShaderResourceView> = None;
        device
            .CreateShaderResourceView(&src_tex, None, Some(&mut src_srv))
            .context("CreateShaderResourceView(fp16 bars)")?;
        let src_srv = src_srv.context("null src srv")?;

        // The IDD out-ring's exact profile: P010, RENDER_TARGET only, MiscFlags 0.
        let p010_desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut p010: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&p010_desc, None, Some(&mut p010))
            .context("CreateTexture2D(P010 bars dst)")?;
        let p010 = p010.context("null p010 tex")?;

        let conv = HdrP010Converter::new(&device, w, h)?;
        let y_rtv = HdrP010Converter::plane_rtv(&device, &p010, DXGI_FORMAT_R16_UNORM)?;
        let uv_rtv = HdrP010Converter::plane_rtv(&device, &p010, DXGI_FORMAT_R16G16_UNORM)?;
        conv.convert(&context, &src_srv, &y_rtv, &uv_rtv, w, h)?;
        Ok((device, p010))
    }
}

/// Colour self-test for [`HdrP010Converter`] (`hdr-p010-selftest`).
/// PASS if max |err| vs [`p010_reference`] is Y ≤ 4, U/V ≤ 5 (rounding + chroma averaging).
///
/// `w`/`h` must be even and non-zero. Run at a session size: 1080 is not
/// 16-aligned (encoder align16 pool seam) and a driver may treat planar RTVs
/// differently at real sizes. `vendor` is a PCI id; a PASS is only for that GPU.
#[cfg(target_os = "windows")]
pub fn hdr_p010_selftest_at(w: u32, h: u32, vendor: Option<u32>) -> Result<()> {
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};

    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        bail!("hdr-p010-selftest needs even non-zero dimensions, got {w}x{h}");
    }
    // The eight 16x16 flats are laid out in a `w / 16` grid. Below one block wide that divisor
    // is zero, and too short to hold the rows the width implies puts the sample points past the
    // surface — a divide-by-zero or an out-of-range index rather than an error. Refuse instead.
    if w < 16 || h < 16 || 8u32.div_ceil(w / 16) * 16 > h {
        bail!("hdr-p010-selftest needs room for its 8 16x16 blocks, got {w}x{h}");
    }
    // 16×16 flats: each 2×2 chroma footprint is uniform, so Cb/Cr can match exactly.
    #[allow(non_snake_case)]
    let (W, H) = (w, h);
    const BLK: u32 = 16;
    // scRGB linear; 1.0 = 80 nits.
    let named: [(&str, f32, f32, f32); 8] = [
        ("red1.0", 1.0, 0.0, 0.0),
        ("green0.5", 0.0, 0.5, 0.0),
        ("blue4.0", 0.0, 0.0, 4.0),
        ("white1.0", 1.0, 1.0, 1.0),
        ("black", 0.0, 0.0, 0.0),
        ("gray0.5", 0.5, 0.5, 0.5),
        ("white4.0", 4.0, 4.0, 4.0),
        ("amber2.0", 2.0, 1.0, 0.0),
    ];

    let grid_cols = W / BLK;
    let pixel_rgb = |x: u32, y: u32| -> (f32, f32, f32, bool) {
        let idx = ((y / BLK) * grid_cols + (x / BLK)) as usize;
        if idx < named.len() {
            let (_, r, g, b) = named[idx];
            (r, g, b, true)
        } else {
            // Per-pixel gradient (Y-only compare — chroma would average neighbours).
            let r = (x as f32 / W as f32) * 3.0;
            let g = (y as f32 / H as f32) * 3.0;
            let b = ((x + y) as f32 / (W + H) as f32) * 3.0;
            (r, g, b, false)
        }
    };

    let mut fp16 = vec![0u16; (W * H * 4) as usize];
    let mut flat = vec![false; (W * H) as usize];
    for y in 0..H {
        for x in 0..W {
            let (r, g, b, is_flat) = pixel_rgb(x, y);
            let i = ((y * W + x) * 4) as usize;
            fp16[i] = f32_to_f16(r);
            fp16[i + 1] = f32_to_f16(g);
            fp16[i + 2] = f32_to_f16(b);
            fp16[i + 3] = f32_to_f16(1.0);
            flat[(y * W + x) as usize] = is_flat;
        }
    }

    // SAFETY: this block's D3D11 device + immediate context (both non-null) own
    // every Create/convert/Copy/Map below; one device, this thread. `fp16` is
    // W×H R16G16B16A16 with SysMemPitch = W*8 and outlives the synchronous
    // CreateTexture2D. Mapped reads are proven at `read_u16`.
    unsafe {
        let adapter: Option<IDXGIAdapter> = match vendor {
            None => None,
            Some(want) => {
                let factory: IDXGIFactory1 = CreateDXGIFactory1().context("dxgi factory")?;
                let mut found = None;
                for i in 0.. {
                    let Ok(a) = factory.EnumAdapters(i) else {
                        break;
                    };
                    let desc = a.GetDesc().context("adapter desc")?;
                    if desc.VendorId == want {
                        found = Some(a);
                        break;
                    }
                }
                Some(found.with_context(|| format!("no adapter with vendor id {want:#x}"))?)
            }
        };
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            adapter.as_ref(),
            if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice(hardware) for hdr-p010-selftest")?;
        let device = device.context("null device")?;
        let context = context.context("null context")?;
        {
            let dxgi: windows::Win32::Graphics::Dxgi::IDXGIDevice =
                device.cast().context("device -> IDXGIDevice")?;
            let desc = dxgi.GetAdapter().context("GetAdapter")?.GetDesc()?;
            let name = String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len())],
            );
            println!(
                "adapter: {name} (vendor {:#06x}, luid {:08x}:{:08x})",
                desc.VendorId, desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
            );
        }

        let src_desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: fp16.as_ptr() as *const c_void,
            SysMemPitch: W * 8, // 4 channels * 2 bytes
            SysMemSlicePitch: 0,
        };
        let mut src_tex: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&src_desc, Some(&init), Some(&mut src_tex))
            .context("CreateTexture2D(fp16 src)")?;
        let src_tex = src_tex.context("null src tex")?;
        let mut src_srv: Option<ID3D11ShaderResourceView> = None;
        device
            .CreateShaderResourceView(&src_tex, None, Some(&mut src_srv))
            .context("CreateShaderResourceView(fp16 src)")?;
        let src_srv = src_srv.context("null src srv")?;

        let p010_desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut p010: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&p010_desc, None, Some(&mut p010))
            .context("CreateTexture2D(P010 dst)")?;
        let p010 = p010.context("null p010 tex")?;

        let conv = HdrP010Converter::new(&device, W, H)?;
        let y_rtv = HdrP010Converter::plane_rtv(&device, &p010, DXGI_FORMAT_R16_UNORM)?;
        let uv_rtv = HdrP010Converter::plane_rtv(&device, &p010, DXGI_FORMAT_R16G16_UNORM)?;
        conv.convert(&context, &src_srv, &y_rtv, &uv_rtv, W, H)?;

        let stage_desc = D3D11_TEXTURE2D_DESC {
            Width: W,
            Height: H,
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
            ..Default::default()
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&stage_desc, None, Some(&mut staging))
            .context("CreateTexture2D(P010 staging)")?;
        let staging = staging.context("null staging")?;
        context.CopyResource(&staging, &p010);

        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
            .context("Map(P010 staging)")?;
        let row_pitch = map.RowPitch as usize; // bytes per luma row
        let base = map.pData as *const u8;
        // P010: plane 1 starts at RowPitch * luma height (same pitch, no extra
        // alignment on a STAGING texture). DepthPitch is the whole surface;
        // drivers omit the chroma offset, so RowPitch*H is the portable read.
        tracing::info!(
            row_pitch,
            depth_pitch = map.DepthPitch,
            expected_chroma_base = row_pitch * H as usize,
            expected_total = row_pitch * H as usize * 3 / 2,
            "hdr-p010-selftest: mapped P010 layout (verify chroma plane offset here if chroma is wrong)"
        );
        let read_u16 = |byte_off: usize| -> u16 {
            // SAFETY: `base` is the mapped staging pointer; all offsets are within the P010 surface
            // (luma H*RowPitch + chroma (H/2)*RowPitch ≤ DepthPitch). Already in the fn's unsafe scope.
            let p = base.add(byte_off) as *const u16;
            p.read_unaligned()
        };
        // Luma codes: stored u16 in the high 10 bits -> code10 = stored >> 6.
        let mut y_codes = vec![0u16; (W * H) as usize];
        for y in 0..H {
            for x in 0..W {
                let off = (y as usize) * row_pitch + (x as usize) * 2;
                y_codes[(y * W + x) as usize] = read_u16(off) >> 6;
            }
        }
        let cw = W / 2;
        let ch = H / 2;
        let chroma_base = row_pitch * H as usize;
        let mut cb_codes = vec![0u16; (cw * ch) as usize];
        let mut cr_codes = vec![0u16; (cw * ch) as usize];
        for cy in 0..ch {
            for cx in 0..cw {
                // Interleaved (Cb, Cr) per chroma sample → 2 u16 = 4 bytes per sample.
                let off = chroma_base + (cy as usize) * row_pitch + (cx as usize) * 4;
                cb_codes[(cy * cw + cx) as usize] = read_u16(off) >> 6;
                cr_codes[(cy * cw + cx) as usize] = read_u16(off + 2) >> 6;
            }
        }
        context.Unmap(&staging, 0);

        let mut max_y_err = 0.0f64;
        for y in 0..H {
            for x in 0..W {
                let (r, g, b, _) = pixel_rgb(x, y);
                let (ry, _, _) = p010_reference(r as f64, g as f64, b as f64);
                let got = y_codes[(y * W + x) as usize] as f64;
                max_y_err = max_y_err.max((got - ry).abs());
            }
        }
        // Cb/Cr only on flat 2×2 footprints; a mixed block has no single reference.
        let mut max_u_err = 0.0f64;
        let mut max_v_err = 0.0f64;
        for cy in 0..ch {
            for cx in 0..cw {
                let (sx, sy) = (cx * 2, cy * 2);
                let all_flat =
                    (0..2).all(|dy| (0..2).all(|dx| flat[((sy + dy) * W + (sx + dx)) as usize]));
                if !all_flat {
                    continue;
                }
                let (r, g, b, _) = pixel_rgb(sx, sy);
                let (_, rcb, rcr) = p010_reference(r as f64, g as f64, b as f64);
                let gu = cb_codes[(cy * cw + cx) as usize] as f64;
                let gv = cr_codes[(cy * cw + cx) as usize] as f64;
                max_u_err = max_u_err.max((gu - rcb).abs());
                max_v_err = max_v_err.max((gv - rcr).abs());
            }
        }

        println!("HDR P010 self-test ({W}x{H}, BT.2020 PQ, 10-bit limited range)");
        println!(
            "  {:<10} {:>14} {:>14} {:>14}",
            "color", "Y exp/got", "Cb exp/got", "Cr exp/got"
        );
        for (idx, (name, r, g, b)) in named.iter().enumerate() {
            let bx = (idx as u32 % grid_cols) * BLK + BLK / 2;
            let by = (idx as u32 / grid_cols) * BLK + BLK / 2;
            let (ey, ecb, ecr) = p010_reference(*r as f64, *g as f64, *b as f64);
            let gy = y_codes[(by * W + bx) as usize] as f64;
            let (ccx, ccy) = (bx / 2, by / 2);
            let gu = cb_codes[(ccy * cw + ccx) as usize] as f64;
            let gv = cr_codes[(ccy * cw + ccx) as usize] as f64;
            println!(
                "  {:<10} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0} {:>6.1}/{:<6.0}",
                name, ey, gy, ecb, gu, ecr, gv
            );
        }
        println!(
            "  max abs error:  Y={max_y_err:.2} (≤4)   Cb={max_u_err:.2} (≤5)   Cr={max_v_err:.2} (≤5)"
        );

        if max_y_err <= 4.0 && max_u_err <= 5.0 && max_v_err <= 5.0 {
            println!("PASS");
            Ok(())
        } else {
            println!("FAIL");
            bail!(
                "HDR P010 self-test out of tolerance (Y={max_y_err:.2} Cb={max_u_err:.2} Cr={max_v_err:.2})"
            );
        }
    }
}

/// IEEE-754 binary16 bits for the FP16 upload. Round-to-nearest; normals and subnormals.
#[cfg(target_os = "windows")]
fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x007f_ffff;
    if exp <= 0 {
        if exp < -10 {
            return sign; // too small → ±0
        }
        let mant = mant | 0x0080_0000; // implicit 1
        let shift = (14 - exp) as u32;
        let half_mant = (mant >> shift) as u16;
        let round = ((mant >> (shift - 1)) & 1) as u16;
        sign | (half_mant + round)
    } else if exp >= 0x1f {
        sign | 0x7c00 // Inf/NaN → Inf (our inputs never hit this)
    } else {
        let half_exp = (exp as u16) << 10;
        let half_mant = (mant >> 13) as u16;
        let round = ((mant >> 12) & 1) as u16;
        // Add, never OR: a mantissa carry must increment the exponent. OR into
        // bit 10 drops the carry on odd biased exponents (bit 10 already set).
        sign | (half_exp + half_mant + round)
    }
}

#[cfg(test)]
mod f16_tests {
    use super::f32_to_f16;

    /// Inverse of [`f32_to_f16`] for the round-trip oracle.
    fn f16_to_f32(h: u16) -> f32 {
        let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
        let exp = ((h >> 10) & 0x1f) as i32;
        let mant = (h & 0x3ff) as f32;
        match exp {
            0 => sign * mant * 2f32.powi(-24),
            31 => sign * f32::INFINITY, // encoder never emits NaN
            e => sign * (1.0 + mant / 1024.0) * 2f32.powi(e - 15),
        }
    }

    #[test]
    fn a_rounding_carry_increments_the_exponent() {
        // Biased exp 127 (2^0), mantissa rounds up out of 10 bits → 2.0 = 0x4000, not 1.0.
        assert_eq!(f32_to_f16(f32::from_bits((127 << 23) | 0x7FF000)), 0x4000);
        assert_eq!(
            f32_to_f16(1.9998779),
            0x4000,
            "1.9998779 must not read as 1.0"
        );
        assert_eq!(
            f32_to_f16(0.49996948),
            0x3800,
            "0.49996948 must not read as 0.25"
        );
        // Even biased exponent (bit 10 clear): carry is visible either way; must stay 4.0.
        assert_eq!(f32_to_f16(f32::from_bits((128 << 23) | 0x7FF000)), 0x4400); // → 4.0
    }

    #[test]
    fn the_constants_the_selftest_uploads_are_exact() {
        assert_eq!(f32_to_f16(0.0), 0x0000);
        assert_eq!(f32_to_f16(-0.0), 0x8000);
        assert_eq!(f32_to_f16(1.0), 0x3C00);
        assert_eq!(f32_to_f16(-1.0), 0xBC00);
        assert_eq!(f32_to_f16(0.5), 0x3800);
        assert_eq!(f32_to_f16(2.0), 0x4000);
        assert_eq!(f32_to_f16(4.0), 0x4400);
    }

    #[test]
    fn hdr_scrgb_values_round_trip_within_one_ulp() {
        for &v in &[
            0.0f32, 0.25, 0.5, 1.0, 1.5, 2.0, 3.0, 4.0, 0.1, 0.3, 0.7, 1.9998779, 0.49996948, 2.5,
            3.999, 0.001,
        ] {
            let back = f16_to_f32(f32_to_f16(v));
            // f16 has 11 significand bits: one ULP is |v|/1024 (subnormal floor 2^-24).
            let ulp = (v.abs() / 1024.0).max(2f32.powi(-24));
            assert!(
                (back - v).abs() <= ulp,
                "{v} round-tripped to {back} (ulp {ulp})"
            );
        }
    }

    #[test]
    fn out_of_range_magnitudes_saturate_rather_than_wrap() {
        // Above f16's max finite (65504) our encoder reports Inf; below its subnormal floor, ±0.
        assert_eq!(f32_to_f16(1.0e30), 0x7C00);
        assert_eq!(f32_to_f16(-1.0e30), 0xFC00);
        assert_eq!(f32_to_f16(1.0e-30), 0x0000);
        assert_eq!(f32_to_f16(-1.0e-30), 0x8000);
    }
}

#[cfg(test)]
mod hdr_selftests {
    /// GPU self-test at 1920×1080 (height not 16-aligned). Pinned to Intel
    /// `0x8086`; missing adapter fails cleanly. Run with
    /// `cargo test -p pf-capture -- --ignored hdr_p010 --nocapture`.
    #[test]
    #[ignore]
    fn hdr_p010_selftest_intel_1080_live() {
        super::hdr_p010_selftest_at(1920, 1080, Some(0x8086)).expect("hdr p010 selftest @1080");
    }
}
