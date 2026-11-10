//! GPU checks on a real adapter: [`HdrP010Converter`]'s colour (the `hdr-p010-selftest`
//! subcommand, against the f64 analogue of its HLSL), [`reframe_selftest_at`] for the scale
//! pass, and the f16 codec both upload and read back through.
//!
//! Does not run in a session. Pin with `punktfunk-host hdr-p010-selftest WxH [vendor]`
//! at a session size (1080 is not 16-aligned — encoder align16 pool seam). `vendor`
//! is a PCI id (`0x8086` Intel / `0x10de` NVIDIA / `0x1002` AMD); a PASS is only
//! for that GPU.
//!
//! Evidence: `f16_tests`, ignored `hdr_p010_selftest_intel_1080_live`,
//! `docs-site/content/docs/(guide)/(streaming)/hdr.md`.

use super::HdrP010Converter;
use anyhow::{bail, Context, Result};
use pf_frame::d3d11_map::MappedSubresource;
use std::ffi::c_void;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
    ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_P010, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_R16G16_UNORM,
    DXGI_FORMAT_R16_UNORM, DXGI_SAMPLE_DESC,
};

/// f64 analogue of the HLSL in `CURVE_PQ2020` + `P010_COMMON`.
/// One scRGB pixel in (linear Rec.709, 1.0 = 80 nits, HDR may exceed 1.0);
/// out is 10-bit full-range (Y, Cb, Cr) for a flat block.
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
    // BT.2020 non-constant-luminance, full-range 10-bit.
    let (kr, kg, kb) = (0.2627, 0.6780, 0.0593);
    let y = kr * pr + kg * pg + kb * pb;
    let cb = (pb - y) / 1.8814;
    let cr = (pr - y) / 1.4746;
    let yc = (1023.0 * y).clamp(0.0, 1023.0);
    let cbc = (512.0 + 1023.0 * cb).clamp(0.0, 1023.0);
    let crc = (512.0 + 1023.0 * cr).clamp(0.0, 1023.0);
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
        // D3D11 reads `h` rows of `pitch` bytes through `pSysMem`.
        let pitch = w * 8;
        assert!(size_of_val(fp16.as_slice()) >= pitch as usize * h as usize);
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: fp16.as_ptr() as *const c_void,
            SysMemPitch: pitch,
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

        let conv = HdrP010Converter::new(&device, w, h, true)?;
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

    let (device, context) = selftest_device(vendor)?;
    // SAFETY: this block's D3D11 device + immediate context (both non-null) own
    // every Create/convert/Copy/Map below; one device, this thread. `fp16` is
    // W×H R16G16B16A16 with SysMemPitch = W*8 and outlives the synchronous
    // CreateTexture2D. Mapped reads are proven at `read_u16`.
    unsafe {
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
        // D3D11 reads `H` rows of `pitch` bytes through `pSysMem`.
        let pitch = W * 8; // 4 channels * 2 bytes
        assert!(size_of_val(fp16.as_slice()) >= pitch as usize * H as usize);
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: fp16.as_ptr() as *const c_void,
            SysMemPitch: pitch,
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

        let conv = HdrP010Converter::new(&device, W, H, true)?;
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

        let map = MappedSubresource::texture(&context, &staging, D3D11_MAP_READ)
            .context("Map(P010 staging)")?;
        // Bytes per luma row. P010: plane 1 starts at RowPitch * luma height (same pitch, no
        // extra alignment on a STAGING texture). DepthPitch is the whole surface; drivers omit
        // the chroma offset, so RowPitch*H is the portable read.
        let row_pitch = map.row_pitch();
        tracing::info!(
            row_pitch,
            depth_pitch = map.depth_pitch(),
            expected_chroma_base = row_pitch * H as usize,
            expected_total = row_pitch * H as usize * 3 / 2,
            "hdr-p010-selftest: mapped P010 layout (verify chroma plane offset here if chroma is wrong)"
        );
        let read_u16 = |byte_off: usize| u16::from_le_bytes([map[byte_off], map[byte_off + 1]]);
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
        drop(map);

        let mut max_y_err = 0.0f64;
        for y in 0..H {
            for x in 0..W {
                let (r, g, b, _) = pixel_rgb(x, y);
                let (ry, _, _) = p010_reference(r as f64, g as f64, b as f64);
                let got = y_codes[(y * W + x) as usize] as f64;
                max_y_err = max_y_err.max((got - ry).abs());
            }
        }
        // Cb/Cr only where the [1 2 1] footprint (columns sx-1..=sx+1, clamped; both rows)
        // is one flat colour; a mixed footprint has no single reference.
        let mut max_u_err = 0.0f64;
        let mut max_v_err = 0.0f64;
        for cy in 0..ch {
            for cx in 0..cw {
                let (sx, sy) = (cx * 2, cy * 2);
                let at = pixel_rgb(sx, sy);
                let all_flat = (0..2).all(|dy| {
                    [sx.saturating_sub(1), sx, sx + 1]
                        .iter()
                        .all(|&x| flat[((sy + dy) * W + x) as usize] && pixel_rgb(x, sy + dy) == at)
                });
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

        println!("HDR P010 self-test ({W}x{H}, BT.2020 PQ, 10-bit full range)");
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

/// A hardware device for a self-test: the first adapter with PCI vendor `vendor`, else the
/// default one. Prints which adapter it got, since a PASS speaks only for that GPU.
#[cfg(target_os = "windows")]
fn selftest_device(vendor: Option<u32>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
    // SAFETY: `?`-checked DXGI and D3D11 calls with live out-params; the adapter description is
    // a local copy, read within its own bounds.
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
        .context("D3D11CreateDevice(hardware) for a self-test")?;
        let device = device.context("null device")?;
        let context = context.context("null context")?;
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
        Ok((device, context))
    }
}

/// GPU check for [`ReframePass`](super::ReframePass): `crop` of a `source` pattern scaled to
/// `out`, once from 8-bit BGRA and once from FP16 scRGB, against
/// [`reframe_reference`](super::reframe::reframe_reference). PASS within 2 codes of 255
/// (BGRA) and 1 % (FP16): the D3D11 spec lets `sin` miss by 8e-4. `vendor` as
/// [`hdr_p010_selftest_at`].
#[cfg(target_os = "windows")]
pub fn reframe_selftest_at(
    source: (u32, u32),
    crop: [u32; 4],
    out: (u32, u32),
    vendor: Option<u32>,
) -> Result<()> {
    use super::ReframePass;
    use windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;

    let (sw, sh) = source;
    let [cx, cy, cw, ch] = crop;
    let fits =
        cx.checked_add(cw).is_some_and(|r| r <= sw) && cy.checked_add(ch).is_some_and(|b| b <= sh);
    if !fits || cw == 0 || ch == 0 || out.0 == 0 || out.1 == 0 {
        bail!(
            "reframe-selftest: crop {crop:?} of {sw}x{sh} to {}x{} does not fit",
            out.0,
            out.1
        );
    }
    let (device, context) = selftest_device(vendor)?;
    // A sawtooth, a smooth ramp and 8x8 checks: edges the kernel rings on beside flats it must
    // keep. FP16 runs it to scRGB 4.0.
    let pattern = |x: u32, y: u32| -> [f32; 3] {
        [
            ((x * 7 + y * 3) % 256) as f32 / 255.0,
            x as f32 / sw as f32,
            ((x / 8 + y / 8) % 2) as f32,
        ]
    };
    for linear in [false, true] {
        let format = if linear {
            DXGI_FORMAT_R16G16B16A16_FLOAT
        } else {
            DXGI_FORMAT_B8G8R8A8_UNORM
        };
        let bpp = if linear { 8 } else { 4 };
        // What the GPU reads, quantized as it stores it, so the reference starts from the same.
        let mut bytes = Vec::with_capacity((sw * sh) as usize * bpp);
        let mut seen = Vec::with_capacity((sw * sh) as usize);
        for y in 0..sh {
            for x in 0..sw {
                let c = pattern(x, y);
                if linear {
                    let h = c.map(|v| f32_to_f16(v * 4.0));
                    for v in [h[0], h[1], h[2], f32_to_f16(1.0)] {
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                    seen.push(h.map(f16_to_f32));
                } else {
                    let b = c.map(|v| (v * 255.0).round() as u8);
                    bytes.extend_from_slice(&[b[2], b[1], b[0], 255]);
                    seen.push(b.map(|v| f32::from(v) / 255.0));
                }
            }
        }
        let want = super::reframe::reframe_reference(&seen, source, crop, out, linear);
        let texture = |w: u32, h: u32, bind: u32, init: Option<&[u8]>| -> Result<ID3D11Texture2D> {
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
                Usage: if bind == 0 {
                    D3D11_USAGE_STAGING
                } else {
                    D3D11_USAGE_DEFAULT
                },
                BindFlags: bind,
                CPUAccessFlags: if bind == 0 {
                    D3D11_CPU_ACCESS_READ.0 as u32
                } else {
                    0
                },
                ..Default::default()
            };
            let data = init.map(|b| D3D11_SUBRESOURCE_DATA {
                pSysMem: b.as_ptr() as *const c_void,
                SysMemPitch: w * bpp as u32,
                SysMemSlicePitch: 0,
            });
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: `desc` is a local; `data`, when present, points into `init`, which holds
            // `h` rows of `w * bpp` bytes and outlives this synchronous call.
            unsafe {
                device.CreateTexture2D(&desc, data.as_ref().map(|d| d as *const _), Some(&mut t))
            }
            .context("CreateTexture2D(reframe selftest)")?;
            t.context("null reframe selftest texture")
        };
        let src = texture(sw, sh, D3D11_BIND_SHADER_RESOURCE.0 as u32, Some(&bytes))?;
        let dst = texture(out.0, out.1, D3D11_BIND_RENDER_TARGET.0 as u32, None)?;
        let staging = texture(out.0, out.1, 0, None)?;
        let pass = ReframePass::new(&device, crop, out, linear)?;
        let (mut srv, mut rtv): (
            Option<ID3D11ShaderResourceView>,
            Option<ID3D11RenderTargetView>,
        ) = (None, None);
        // SAFETY: view creation on live textures of this device with live out-params, then a
        // copy between two same-size, same-format textures of it.
        unsafe {
            device.CreateShaderResourceView(&src, None, Some(&mut srv))?;
            device.CreateRenderTargetView(&dst, None, Some(&mut rtv))?;
            pass.scale(
                &context,
                srv.as_ref().context("null srv")?,
                rtv.as_ref().context("null rtv")?,
            );
            context.CopyResource(&staging, &dst);
        }
        let map = MappedSubresource::texture(&context, &staging, D3D11_MAP_READ)
            .context("Map(reframe staging)")?;
        let pitch = map.row_pitch();
        let (mut worst, mut worst_at) = (0.0f64, (0, 0));
        for y in 0..out.1 as usize {
            for x in 0..out.0 as usize {
                let at = y * pitch + x * bpp;
                let got: [f32; 3] = if linear {
                    std::array::from_fn(|k| {
                        f16_to_f32(u16::from_le_bytes([map[at + 2 * k], map[at + 2 * k + 1]]))
                    })
                } else {
                    [map[at + 2], map[at + 1], map[at]].map(|v| f32::from(v) / 255.0)
                };
                let w = want[y * out.0 as usize + x];
                for k in 0..3 {
                    let (g, e) = (f64::from(got[k]), f64::from(w[k]));
                    // BGRA in codes of 255, FP16 relative to the value, floored at 1.0.
                    let err = if linear {
                        (g - e).abs() / e.abs().max(1.0)
                    } else {
                        (g - e).abs() * 255.0
                    };
                    if err > worst {
                        (worst, worst_at) = (err, (x, y));
                    }
                }
            }
        }
        drop(map);
        let (name, limit) = if linear {
            ("FP16 scRGB", 0.01)
        } else {
            ("BGRA", 2.0)
        };
        let (ow, oh) = out;
        println!("reframe {name}: {crop:?} of {sw}x{sh} -> {ow}x{oh}");
        println!("  worst error {worst:.4} at {worst_at:?} (limit {limit})");
        if worst > limit {
            println!("FAIL");
            bail!("reframe {name} self-test out of tolerance: {worst:.4} at {worst_at:?}");
        }
    }
    println!("PASS");
    Ok(())
}

/// IEEE-754 binary16 bits for the FP16 upload. Round-to-nearest; normals and subnormals.
#[cfg(target_os = "windows")]
pub(super) fn f32_to_f16(v: f32) -> u16 {
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

/// Inverse of [`f32_to_f16`]: the value an FP16 texel holds.
#[cfg(target_os = "windows")]
pub(super) fn f16_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let exp = ((h >> 10) & 0x1f) as i32;
    let mant = (h & 0x3ff) as f32;
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        31 => sign * f32::INFINITY, // encoder never emits NaN
        e => sign * (1.0 + mant / 1024.0) * 2f32.powi(e - 15),
    }
}

#[cfg(test)]
mod f16_tests {
    use super::{f16_to_f32, f32_to_f16};

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
    /// `cargo test -p pf-encode-win -- --ignored hdr_p010 --nocapture`.
    #[test]
    #[ignore]
    fn hdr_p010_selftest_intel_1080_live() {
        super::hdr_p010_selftest_at(1920, 1080, Some(0x8086)).expect("hdr p010 selftest @1080");
    }

    /// The supersampled case (a 4K render to a 1080p view) and an off-centre crop, on the
    /// default adapter. Run with `cargo test -p pf-encode-win -- --ignored reframe --nocapture`.
    #[test]
    #[ignore]
    fn reframe_selftest_live() {
        super::reframe_selftest_at((3840, 2160), [0, 0, 3840, 2160], (1920, 1080), None)
            .expect("reframe 4K to 1080p");
        super::reframe_selftest_at((1920, 1080), [200, 100, 1280, 720], (853, 480), None)
            .expect("reframe crop");
    }
}
