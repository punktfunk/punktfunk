//! D3D11 colour converters shared by capture and encode: FP16 scRGB → P010
//! ([`HdrP010Converter`]) and → packed 10-bit ([`HdrRgb10Converter`]), BGRA →
//! PyroWave planes ([`BgraToYuvPlanes`]), the video-engine CSC
//! ([`VideoConverter`]), and the cursor blend ([`CursorBlendPass`]). HLSL is
//! compiled at first use through [`compile_shader`]. Consumed by pf-capture's
//! IDD-push capturer today; the display driver next.

mod cursor_blend;
pub use cursor_blend::CursorBlendPass;

use anyhow::{bail, Context, Result};
use std::ffi::c_void;
use windows::core::{s, Interface, PCSTR};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11SamplerState, ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader,
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BUFFER_DESC, D3D11_COMPARISON_NEVER,
    D3D11_FILTER_MIN_MAG_MIP_POINT, D3D11_RENDER_TARGET_VIEW_DESC, D3D11_RENDER_TARGET_VIEW_DESC_0,
    D3D11_RTV_DIMENSION_TEXTURE2D, D3D11_SAMPLER_DESC, D3D11_SUBRESOURCE_DATA, D3D11_TEX2D_RTV,
    D3D11_TEXTURE_ADDRESS_CLAMP, D3D11_USAGE_IMMUTABLE, D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_R10G10B10A2_UNORM};

/// Compile one HLSL entry point to bytecode.
///
/// # Safety
/// `entry` and `target` must be valid NUL-terminated ASCII (`s!()` at every
/// call site); `src` is a live `&str` for the duration of the call.
pub unsafe fn compile_shader(src: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>> {
    // SAFETY: `D3DCompile` reads `src.as_ptr()` for exactly `src.len()` bytes (a live `&str` that
    // outlives the synchronous call) plus the two caller-supplied NUL-terminated `PCSTR`s (per the
    // contract above); `&mut blob` / `Some(&mut errs)` are live out-params. Both
    // `slice::from_raw_parts` calls pair a blob's OWN `GetBufferPointer` with its OWN
    // `GetBufferSize` while that blob is still alive, and the slice is copied
    // (`to_string` / `to_vec`) before it goes out of scope.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut errs: Option<ID3DBlob> = None;
        let r = D3DCompile(
            src.as_ptr() as *const c_void,
            src.len(),
            PCSTR::null(),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut blob,
            Some(&mut errs),
        );
        if r.is_err() {
            let msg = errs
                .as_ref()
                .map(|e| {
                    let p = e.GetBufferPointer() as *const u8;
                    String::from_utf8_lossy(std::slice::from_raw_parts(p, e.GetBufferSize()))
                        .to_string()
                })
                .unwrap_or_default();
            bail!("D3DCompile failed: {msg}");
        }
        let blob = blob.context("no shader blob")?;
        let p = blob.GetBufferPointer() as *const u8;
        Ok(std::slice::from_raw_parts(p, blob.GetBufferSize()).to_vec())
    }
}

/// Fullscreen-triangle vertex shader for the HDR conversion pass (3 verts, no input layout).
pub const HDR_VS: &str = r"
struct VOut { float4 pos : SV_POSITION; float2 uv : TEXCOORD0; };
VOut main(uint vid : SV_VertexID) {
    float2 uv = float2((vid << 1) & 2, vid & 2);
    VOut o;
    o.pos = float4(uv * float2(2.0, -2.0) + float2(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}
";

/// Shared scRGB FP16 → BT.2020 PQ math for the P010 luma and chroma passes.
/// scRGB 1.0 = 80 nits. Identical to the R10 HDR path until RGB→Y + studio-range.
const HDR_P010_COMMON: &str = r"
Texture2D<float4> tx : register(t0);
SamplerState sm : register(s0);
// Rec.709 → Rec.2020 (linear). Same matrix as the R10 converter.
static const float3x3 BT709_TO_BT2020 = {
    0.627403914, 0.329283038, 0.043313048,
    0.069097292, 0.919540405, 0.011362303,
    0.016391439, 0.088013308, 0.895595253
};
float3 pq_oetf(float3 L) {
    // L is 1.0 = 10000 nits (ST 2084).
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 Lp = pow(saturate(L), m1);
    return pow((c1 + c2 * Lp) / (1.0 + c3 * Lp), m2);
}
// PQ BT.2020 RGB in [0,1] — the same pixels the R10 path stores before quantize.
// Both P010 passes use this so they match HdrConverter and the Rust reference.
float3 scrgb_to_pq2020(float2 uv) {
    // Negative scRGB is wide-gamut colour: convert first, clamp after (pq_oetf saturates).
    float3 nits = tx.Sample(sm, uv).rgb * 80.0;      // scRGB 1.0 = 80 nits
    float3 lin2020 = mul(BT709_TO_BT2020, nits);
    return pq_oetf(lin2020 / 10000.0);               // normalize to 10k nits, encode PQ -> [0,1]
}
// BT.2020 non-constant-luminance, on the PQ-encoded (gamma) RGB. Kr/Kg/Kb per Rec.2020.
static const float KR = 0.2627;
static const float KG = 0.6780;
static const float KB = 0.0593;
// 10-bit studio (limited) range codes. Y'  -> [64, 940]; Cb/Cr -> [64, 960] (512 ± 448).
float studio_y_code(float3 rgb_pq) {
    float y = KR * rgb_pq.r + KG * rgb_pq.g + KB * rgb_pq.b;     // [0,1]
    float code = 64.0 + 876.0 * y;                              // [64, 940]
    return clamp(code, 64.0, 940.0);
}
float2 studio_cbcr_code(float3 rgb_pq) {
    float y = KR * rgb_pq.r + KG * rgb_pq.g + KB * rgb_pq.b;
    float cb = (rgb_pq.b - y) / 1.8814;                          // ~[-0.5, 0.5]
    float cr = (rgb_pq.r - y) / 1.4746;
    float cbc = 512.0 + 896.0 * cb;                             // [64, 960]
    float crc = 512.0 + 896.0 * cr;
    return float2(clamp(cbc, 64.0, 960.0), clamp(crc, 64.0, 960.0));
}
// P010 stores the 10-bit code in the high 10 bits (code10 << 6). As R16_UNORM
// the float that maps to that u16 is code10*64 / 65535.0; round first, a decoder truncates.
float code10_to_unorm(float code10) { return (round(code10) * 64.0) / 65535.0; }
";

/// P010 luma: full-res Y′ into plane 0 (`R16_UNORM`).
const HDR_P010_Y_PS: &str = r"
#include_common
float main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    float3 pq = scrgb_to_pq2020(uv);
    float yc = studio_y_code(pq);
    return code10_to_unorm(yc);
}
";

/// P010 chroma: half-res interleaved (Cb,Cr) on plane 1 (`R16G16_UNORM`).
/// Left-cosited (H.273 chroma_loc type 0, the unsignaled default). Average the
/// even luma column's two rows in scRGB-linear, then PQ + Cb/Cr. A 2×2 box is
/// centre-sited and shifts chroma by half a luma pixel against the decoder.
/// `inv_src` = (1/srcW, 1/srcH).
const HDR_P010_UV_PS: &str = r"
#include_common
cbuffer C : register(b0) { float2 inv_src; float2 pad; };
float2 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    // `uv` is the chroma texel centre (middle of the 2×2 luma block). Left-cosite
    // is the LEFT column: the two centres sit at uv + (-h.x, ±h.y).
    float2 h = inv_src * 0.5;
    float3 a = tx.Sample(sm, uv + float2(-h.x, -h.y)).rgb;
    float3 b = tx.Sample(sm, uv + float2(-h.x,  h.y)).rgb;
    float3 scrgb = (a + b) * 0.5;
    float3 nits = scrgb * 80.0;
    float3 lin2020 = mul(BT709_TO_BT2020, nits);
    float3 pq = pq_oetf(lin2020 / 10000.0);
    float2 cc = studio_cbcr_code(pq);
    return float2(code10_to_unorm(cc.x), code10_to_unorm(cc.y));
}
";

/// scRGB FP16 → `R10G10B10A2` (BT.2020 PQ, full-range RGB), one full-res pass.
/// NVENC takes packed 10-bit RGB as `NV_ENC_BUFFER_FORMAT_ABGR10` and CSC to
/// YUV 4:4:4 itself. Colour math is [`HDR_P010_COMMON`]'s `scrgb_to_pq2020`.
///
/// DXGI `R10G10B10A2_UNORM` stores R in the low 10 bits, which NVENC names
/// `ABGR10` (A2B10G10R10 from the MSB). Same as SDR `B8G8R8A8` vs `ARGB`.
/// The shader writes RGB; no swizzle.
pub struct HdrRgb10Converter {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
}

/// R10G10B10A2 pass: PQ BT.2020 RGB into the packed 10-bit target. `saturate` is
/// implicit in the UNORM RT; `scrgb_to_pq2020` already clamps.
const HDR_RGB10_PS: &str = r"
#include_common
float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    return float4(scrgb_to_pq2020(uv), 1.0);
}
";

/// 10-bit SDR pass: BGRA → packed 10-bit, same sRGB values. The UNORM roundtrip
/// is the 8→10 expand (255/255 → 1023/1023). Transfer stays sRGB/BT.709; extra
/// bits are encoder precision, not source depth.
const SDR_RGB10_PS: &str = r"
Texture2D<float4> tx : register(t0);
SamplerState sm : register(s0);
float4 main(float4 pos : SV_POSITION, float2 uv : TEXCOORD0) : SV_TARGET {
    return float4(tx.Sample(sm, uv).rgb, 1.0);
}
";

impl HdrRgb10Converter {
    /// FP16 scRGB in, PQ-encoded BT.2020 RGB out.
    pub fn new(device: &ID3D11Device) -> Result<Self> {
        Self::from_ps(
            device,
            HDR_RGB10_PS.replace("#include_common", HDR_P010_COMMON),
        )
    }

    /// 10-bit SDR pass: BGRA in, same sRGB out at 10-bit UNORM (see
    /// [`SDR_RGB10_PS`]). Same VS/sampler/draw as the HDR pass so they cannot drift.
    pub fn new_sdr_expand(device: &ID3D11Device) -> Result<Self> {
        Self::from_ps(device, SDR_RGB10_PS.to_string())
    }

    fn from_ps(device: &ID3D11Device, src: String) -> Result<Self> {
        // SAFETY: every call is a `?`-checked D3D11 method on the live `device` borrow, over
        // fully-initialized stack descriptors and live `Option` out-params; `compile_shader`
        // receives `s!()` literals (its contract). Each created COM interface owns its own
        // reference, and no raw pointer outlives the call that produced it.
        unsafe {
            let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
            let psb = compile_shader(&src, s!("main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(&psb, None, Some(&mut ps))?;
            // POINT: 1:1 full-res, each RT pixel is one source texel centre.
            // Filtering would only blur it.
            let sd = D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            Ok(Self {
                vs: vs.context("rgb10 vs")?,
                ps: ps.context("rgb10 ps")?,
                sampler: sampler.context("rgb10 sampler")?,
            })
        }
    }

    /// Non-planar RTV of the packed 10-bit output. Once per out-ring slot, never per frame.
    pub fn rtv(device: &ID3D11Device, dst: &ID3D11Texture2D) -> Result<ID3D11RenderTargetView> {
        // SAFETY: one `?`-checked `CreateRenderTargetView` on the live `device` borrow, with a
        // fully-initialized descriptor local whose address is taken only for the synchronous call,
        // plus a live `Option` out-param.
        unsafe {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: DXGI_FORMAT_R10G10B10A2_UNORM,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device
                .CreateRenderTargetView(
                    dst,
                    Some(&desc as *const D3D11_RENDER_TARGET_VIEW_DESC),
                    Some(&mut rtv),
                )
                .context("CreateRenderTargetView(R10G10B10A2 out slot)")?;
            rtv.context("rgb10 rtv null")
        }
    }

    pub fn convert(
        &self,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        rtv: &ID3D11RenderTargetView,
        w: u32,
        h: u32,
    ) -> Result<()> {
        // SAFETY: all D3D11 work runs on the caller's live `ctx` borrow (the owning capture
        // thread's immediate context) over borrowed slices of fully-initialized locals and clones
        // of the caller's live SRV/RTV. No raw pointers and no mapping on this path.
        unsafe {
            ctx.OMSetBlendState(None, None, 0xffff_ffff); // opaque overwrite
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            let vp = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp]));
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.PSSetShader(&self.ps, None);
            ctx.Draw(3, 0);
            // Unbind so the next frame can re-RTV and NVENC can read.
            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            Ok(())
        }
    }
}

/// scRGB FP16 → P010 (BT.2020 PQ, 10-bit studio range) in two shader passes
/// (full-res luma, half-res chroma). NVIDIA's D3D11 VideoProcessor cannot do
/// RGB→P010 (renders green). One per capture device; rebuilt on device recreate.
///
/// Plane writes are planar RTVs of one P010 texture: `R16_UNORM` = plane 0
/// luma, `R16G16_UNORM` = plane 1 chroma. Needs D3D11.3+; a rejected plane
/// format fails the session — there is no runtime fallback.
pub struct HdrP010Converter {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    /// Chroma-pass `inv_src` = (1/srcW, 1/srcH), 16 bytes. Immutable: source size
    /// is fixed for this converter, which is already rebuilt on mode change.
    cbuf: ID3D11Buffer,
}

impl HdrP010Converter {
    /// `w`/`h` are the source size baked into the immutable chroma constant buffer.
    /// Rebuild if they change — a session that re-opens at a new mode drops this converter.
    pub fn new(device: &ID3D11Device, w: u32, h: u32) -> Result<Self> {
        // SAFETY: every call is a `?`-checked D3D11 method on the live `device` borrow, over
        // fully-initialized stack descriptors and live `Option` out-params; `compile_shader` receives
        // `s!()` literals (its contract). Each created COM interface owns its own reference, and no
        // raw pointer outlives the call that produced it.
        unsafe {
            // D3DCompile has no include handler here; substitute `#include_common`.
            let y_src = HDR_P010_Y_PS.replace("#include_common", HDR_P010_COMMON);
            let uv_src = HDR_P010_UV_PS.replace("#include_common", HDR_P010_COMMON);
            let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
            let yb = compile_shader(&y_src, s!("main"), s!("ps_5_0"))?;
            let uvb = compile_shader(&uv_src, s!("main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps_y = None;
            device.CreatePixelShader(&yb, None, Some(&mut ps_y))?;
            let mut ps_uv = None;
            device.CreatePixelShader(&uvb, None, Some(&mut ps_uv))?;
            let sd = D3D11_SAMPLER_DESC {
                // POINT: Y samples one texel centre; UV takes two explicit left-column
                // taps and averages in the shader. Filtering would blur those taps.
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                MaxLOD: f32::MAX,
                ..Default::default()
            };
            let mut sampler = None;
            device.CreateSamplerState(&sd, Some(&mut sampler))?;
            // `inv_src` is fixed for this source size; IMMUTABLE so convert never Maps.
            let inv_src: [f32; 4] = [1.0 / w.max(1) as f32, 1.0 / h.max(1) as f32, 0.0, 0.0];
            let cbd = D3D11_BUFFER_DESC {
                ByteWidth: 16, // float2 inv_src + float2 pad
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: 0,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: inv_src.as_ptr().cast(),
                SysMemPitch: 0,
                SysMemSlicePitch: 0,
            };
            let mut cbuf = None;
            device.CreateBuffer(&cbd, Some(&init), Some(&mut cbuf))?;
            Ok(Self {
                vs: vs.context("p010 vs")?,
                ps_y: ps_y.context("p010 y ps")?,
                ps_uv: ps_uv.context("p010 uv ps")?,
                sampler: sampler.context("p010 sampler")?,
                cbuf: cbuf.context("p010 cbuf")?,
            })
        }
    }

    /// Per-plane RTV of P010 `dst`: `R16_UNORM` = plane 0 luma, `R16G16_UNORM` =
    /// plane 1 chroma (format selects the plane). Once per out-ring slot, not per
    /// frame. Fails if the driver rejects planar RTVs (D3D11.3+ required).
    pub fn plane_rtv(
        device: &ID3D11Device,
        dst: &ID3D11Texture2D,
        format: DXGI_FORMAT,
    ) -> Result<ID3D11RenderTargetView> {
        // SAFETY: one `?`-checked `CreateRenderTargetView` on the live `device` borrow, with a
        // fully-initialized `D3D11_RENDER_TARGET_VIEW_DESC` local whose address is taken only for the
        // duration of the synchronous call, plus a live `Option` out-param.
        unsafe {
            let desc = D3D11_RENDER_TARGET_VIEW_DESC {
                Format: format,
                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_RTV { MipSlice: 0 },
                },
            };
            let mut rtv: Option<ID3D11RenderTargetView> = None;
            device
            .CreateRenderTargetView(
                dst,
                Some(&desc as *const D3D11_RENDER_TARGET_VIEW_DESC),
                Some(&mut rtv),
            )
            .with_context(|| {
                format!("CreateRenderTargetView(P010 plane, format={format:?}) — driver may not support planar RTVs")
            })?;
            rtv.context("p010 plane rtv null")
        }
    }

    /// Convert `src_srv` (FP16 scRGB, WxH) into P010 via caller-built plane RTVs
    /// ([`Self::plane_rtv`]). `w`/`h` are full luma dims (even) and must match
    /// construction. Views live on the out-ring slot so this path never
    /// `CreateRenderTargetView`s inside the keyed-mutex hold.
    pub fn convert(
        &self,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        y_rtv: &ID3D11RenderTargetView,
        uv_rtv: &ID3D11RenderTargetView,
        w: u32,
        h: u32,
    ) -> Result<()> {
        // SAFETY: all D3D11 work runs on the caller's live `ctx` borrow (the owning capture thread's
        // immediate context) over borrowed slices of fully-initialized locals (the viewports) and
        // clones of the caller's live SRV/RTVs. No raw pointers and no mapping on this path.
        unsafe {
            ctx.OMSetBlendState(None, None, 0xffff_ffff); // opaque overwrite
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
            ctx.PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

            let vp_y = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp_y]));
            ctx.OMSetRenderTargets(Some(&[Some(y_rtv.clone())]), None);
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(3, 0);
            ctx.OMSetRenderTargets(Some(&[None]), None);

            let vp_uv = D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: (w / 2) as f32,
                Height: (h / 2) as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            };
            ctx.RSSetViewports(Some(&[vp_uv]));
            ctx.OMSetRenderTargets(Some(&[Some(uv_rtv.clone())]), None);
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cbuf.clone())]));
            ctx.Draw(3, 0);

            // Unbind so the next frame can re-RTV and NVENC can read.
            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            Ok(())
        }
    }
}

/// PyroWave luma: full-res Y′ into `R8_UNORM`. BT.709 limited from 8-bit sRGB
/// BGRA, byte-identical to Linux `rgb2yuv.comp` `lumaY`. `Load` (not Sample)
/// so RTV pixel (x,y) is source texel (x,y).
const PYRO_Y_PS: &str = r"
Texture2D<float4> tx : register(t0);
float main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 c = tx.Load(int3(int2(pos.xy), 0)).rgb;
    return 16.0/255.0 + 0.1826*c.r + 0.6142*c.g + 0.0620*c.b;
}
";

/// PyroWave chroma: half-res interleaved CbCr into `R8G8_UNORM`. Left-sited
/// (H.273 type 0) [1 2 1] over columns p.x-1..p.x+1, then BT.709 limited Cb/Cr —
/// the same as `rgb2yuv.comp`. Even dimensions keep p.x+1 in-bounds.
const PYRO_UV_PS: &str = r"
Texture2D<float4> tx : register(t0);
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    int2 p = int2(pos.xy) * 2;
    int2 l = int2(max(p.x - 1, 0), p.y);
    float3 c00 = tx.Load(int3(p,             0)).rgb;
    float3 c10 = tx.Load(int3(p + int2(1,0), 0)).rgb;
    float3 c01 = tx.Load(int3(p + int2(0,1), 0)).rgb;
    float3 c11 = tx.Load(int3(p + int2(1,1), 0)).rgb;
    float3 cl0 = tx.Load(int3(l,             0)).rgb;
    float3 cl1 = tx.Load(int3(l + int2(0,1), 0)).rgb;
    float3 a = (cl0 + 2.0 * c00 + c10 + cl1 + 2.0 * c01 + c11) * 0.125;
    float u = 128.0/255.0 - 0.1006*a.r - 0.3386*a.g + 0.4392*a.b;
    float v = 128.0/255.0 + 0.4392*a.r - 0.3989*a.g - 0.0403*a.b;
    return float2(u, v);
}
";

/// PyroWave 4:4:4 chroma: full-res, per-pixel. Windows twin of `rgb2yuv444.comp`.
const PYRO_UV444_PS: &str = r"
Texture2D<float4> tx : register(t0);
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 c = tx.Load(int3(int2(pos.xy), 0)).rgb;
    float u = 128.0/255.0 - 0.1006*c.r - 0.3386*c.g + 0.4392*c.b;
    float v = 128.0/255.0 + 0.4392*c.r - 0.3989*c.g - 0.0403*c.b;
    return float2(u, v);
}
";

/// Shared HDR PyroWave math: scRGB FP16 → PQ BT.2020 → 10-bit studio codes
/// packed into 16-bit UNORM. Same CSC as [`HDR_P010_COMMON`], over `Load`ed
/// texels so the passes stay texel-exact like the SDR twins.
const PYRO_HDR_COMMON: &str = r"
Texture2D<float4> tx : register(t0);
static const float3x3 BT709_TO_BT2020 = {
    0.627403914, 0.329283038, 0.043313048,
    0.069097292, 0.919540405, 0.011362303,
    0.016391439, 0.088013308, 0.895595253
};
float3 pq_oetf(float3 L) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float3 Lp = pow(saturate(L), m1);
    return pow((c1 + c2 * Lp) / (1.0 + c3 * Lp), m2);
}
float3 scrgb_to_pq2020_rgb(float3 scrgb) {
    // Negative scRGB is wide-gamut colour: convert first, pq_oetf clamps after.
    return pq_oetf(mul(BT709_TO_BT2020, scrgb * 80.0) / 10000.0);
}
static const float KR = 0.2627;
static const float KG = 0.6780;
static const float KB = 0.0593;
float y_unorm(float3 pq) {
    float y = KR * pq.r + KG * pq.g + KB * pq.b;
    float code = clamp(round(64.0 + 876.0 * y), 64.0, 940.0);
    return (code * 64.0) / 65535.0;
}
float2 cbcr_unorm(float3 pq) {
    float y = KR * pq.r + KG * pq.g + KB * pq.b;
    float cbc = clamp(round(512.0 + 896.0 * (pq.b - y) / 1.8814), 64.0, 960.0);
    float crc = clamp(round(512.0 + 896.0 * (pq.r - y) / 1.4746), 64.0, 960.0);
    return float2((cbc * 64.0) / 65535.0, (crc * 64.0) / 65535.0);
}
";

/// PyroWave HDR luma: full-res PQ Y′ studio codes into `R16_UNORM`.
const PYRO_HDR_Y_PS: &str = r"
#include_common
float main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 pq = scrgb_to_pq2020_rgb(tx.Load(int3(int2(pos.xy), 0)).rgb);
    return y_unorm(pq);
}
";

/// PyroWave HDR 4:2:0 chroma: half-res, left-sited [1 2 1] like the SDR pass, averaged
/// in scRGB-linear, then PQ + studio Cb/Cr.
const PYRO_HDR_UV_PS: &str = r"
#include_common
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    int2 p = int2(pos.xy) * 2;
    int2 l = int2(max(p.x - 1, 0), p.y);
    float3 a = tx.Load(int3(p,             0)).rgb;
    float3 b = tx.Load(int3(p + int2(1,0), 0)).rgb;
    float3 c = tx.Load(int3(p + int2(0,1), 0)).rgb;
    float3 d = tx.Load(int3(p + int2(1,1), 0)).rgb;
    float3 e = tx.Load(int3(l,             0)).rgb;
    float3 f = tx.Load(int3(l + int2(0,1), 0)).rgb;
    float3 pq = scrgb_to_pq2020_rgb((e + 2.0 * a + b + f + 2.0 * c + d) * 0.125);
    return cbcr_unorm(pq);
}
";

/// PyroWave HDR 4:4:4 chroma: full-res, per-pixel.
const PYRO_HDR_UV444_PS: &str = r"
#include_common
float2 main(float4 pos : SV_POSITION) : SV_TARGET {
    float3 pq = scrgb_to_pq2020_rgb(tx.Load(int3(int2(pos.xy), 0)).rgb);
    return cbcr_unorm(pq);
}
";

/// BGRA/scRGB → separate Y and interleaved CbCr textures for the PyroWave
/// wavelet encoder (`design/pyrowave-windows-host-zerocopy.md`,
/// `design/pyrowave-444-hdr.md`). SDR writes BT.709-limited 8-bit planes;
/// HDR writes P010-style 10-bit studio codes into 16-bit planes.
///
/// Two textures, not one planar NV12: NVIDIA's D3D11→Vulkan import of a
/// planar NV12 is unreliable at arbitrary sizes. Caller owns the textures
/// and RTVs (shareable, per out-ring slot).
pub struct BgraToYuvPlanes {
    vs: ID3D11VertexShader,
    ps_y: ID3D11PixelShader,
    ps_uv: ID3D11PixelShader,
    /// Full-res chroma (4:4:4): the chroma viewport skips the /2.
    chroma444: bool,
}

impl BgraToYuvPlanes {
    pub fn new(device: &ID3D11Device, hdr: bool, chroma444: bool) -> Result<Self> {
        // SAFETY: as `HdrP010Converter::new` — `?`-checked D3D11 shader creation on the live
        // `device` borrow, with `s!()` literals into `compile_shader` and live out-params.
        unsafe {
            let (y_src, uv_src) = match (hdr, chroma444) {
                (false, false) => (PYRO_Y_PS.to_string(), PYRO_UV_PS.to_string()),
                (false, true) => (PYRO_Y_PS.to_string(), PYRO_UV444_PS.to_string()),
                (true, false) => (
                    PYRO_HDR_Y_PS.replace("#include_common", PYRO_HDR_COMMON),
                    PYRO_HDR_UV_PS.replace("#include_common", PYRO_HDR_COMMON),
                ),
                (true, true) => (
                    PYRO_HDR_Y_PS.replace("#include_common", PYRO_HDR_COMMON),
                    PYRO_HDR_UV444_PS.replace("#include_common", PYRO_HDR_COMMON),
                ),
            };
            let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
            let yb = compile_shader(&y_src, s!("main"), s!("ps_5_0"))?;
            let uvb = compile_shader(&uv_src, s!("main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps_y = None;
            device.CreatePixelShader(&yb, None, Some(&mut ps_y))?;
            let mut ps_uv = None;
            device.CreatePixelShader(&uvb, None, Some(&mut ps_uv))?;
            Ok(Self {
                vs: vs.context("pyro vs")?,
                ps_y: ps_y.context("pyro y ps")?,
                ps_uv: ps_uv.context("pyro uv ps")?,
                chroma444,
            })
        }
    }

    /// `src_srv` (BGRA SDR / scRGB FP16 HDR) → `y_rtv` (full-res Y) + `cbcr_rtv`
    /// (half- or full-res CbCr). `w`/`h` are full luma dims (even for 4:2:0).
    #[allow(clippy::too_many_arguments)]
    pub fn convert(
        &self,
        ctx: &ID3D11DeviceContext,
        src_srv: &ID3D11ShaderResourceView,
        y_rtv: &ID3D11RenderTargetView,
        cbcr_rtv: &ID3D11RenderTargetView,
        w: u32,
        h: u32,
    ) -> Result<()> {
        // SAFETY: D3D11 state-setting plus two `Draw`s on the caller's live immediate-context
        // borrow, over borrowed slices of fully-initialized locals (the viewports) and clones of the
        // caller's live SRV/RTVs. No raw pointers and no mapping on this path.
        unsafe {
            ctx.OMSetBlendState(None, None, 0xffff_ffff); // opaque overwrite
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShaderResources(0, Some(&[Some(src_srv.clone())]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: w as f32,
                Height: h as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.OMSetRenderTargets(Some(&[Some(y_rtv.clone())]), None);
            ctx.PSSetShader(&self.ps_y, None);
            ctx.Draw(3, 0);
            ctx.OMSetRenderTargets(Some(&[None]), None);

            let (cw, ch) = if self.chroma444 {
                (w, h)
            } else {
                (w / 2, h / 2)
            };
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: cw as f32,
                Height: ch as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.OMSetRenderTargets(Some(&[Some(cbcr_rtv.clone())]), None);
            ctx.PSSetShader(&self.ps_uv, None);
            ctx.Draw(3, 0);

            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.PSSetShaderResources(0, Some(&[None]));
            Ok(())
        }
    }
}

use windows::Win32::Graphics::Direct3D11::{
    ID3D11VideoContext1, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV,
    D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_RATIONAL,
};

/// D3D11 Video Processor CSC on the dedicated video engine, not the 3D
/// engine, so RGB→YUV does not contend with a GPU-bound game. Output is
/// always NV12, BT.709 studio-range — a native NVENC YUV input.
///
/// Does not produce P010/BT.2020: `new` pins
/// `YCBCR_STUDIO_G22_LEFT_P709`, and NVIDIA's processor cannot RGB→P010
/// (renders green). `scrgb_input` tone-maps FP16 down to 8-bit BT.709;
/// `idd_push::ensure_converter` currently always passes `false`.
pub struct VideoConverter {
    vdev: ID3D11VideoDevice,
    vctx: ID3D11VideoContext1,
    enumr: ID3D11VideoProcessorEnumerator,
    vp: ID3D11VideoProcessor,
}

impl VideoConverter {
    /// BGRA/FP16-RGB → NV12 (BT.709 limited SDR) on the video engine.
    /// `scrgb_input`: `false` = 8-bit sRGB BGRA, `true` = FP16 scRGB linear.
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        scrgb_input: bool,
    ) -> Result<Self> {
        // SAFETY: the `cast()`s and the `?`-checked video-device factory calls run on the caller's
        // live `device`/`context` borrows; `&desc` is a fully-initialized stack
        // `D3D11_VIDEO_PROCESSOR_CONTENT_DESC` read only for the duration of the call, and the
        // colour-space/frame-format setters take the just-created processor by borrow plus plain
        // enum values.
        unsafe {
            let vdev: ID3D11VideoDevice = device.cast().context("device -> ID3D11VideoDevice")?;
            let vctx: ID3D11VideoContext1 =
                context.cast().context("context -> ID3D11VideoContext1")?;
            let rate = DXGI_RATIONAL {
                Numerator: 240,
                Denominator: 1,
            };
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: rate,
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumr = vdev
                .CreateVideoProcessorEnumerator(&desc)
                .context("CreateVideoProcessorEnumerator")?;
            let vp = vdev
                .CreateVideoProcessor(&enumr, 0)
                .context("CreateVideoProcessor")?;

            // Full-range RGB in → studio BT.709 NV12 out. G10 = FP16 scRGB ring,
            // G22 = 8-bit BGRA ring. Output is always BT.709 SDR (tone-maps scRGB).
            let in_cs = if scrgb_input {
                DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709
            } else {
                DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
            };
            let out_cs = DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709;
            vctx.VideoProcessorSetStreamColorSpace1(&vp, 0, in_cs);
            vctx.VideoProcessorSetOutputColorSpace1(&vp, out_cs);
            // Progressive: one frame in, one out — no deinterlace, no frame-rate convert.
            vctx.VideoProcessorSetStreamFrameFormat(&vp, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            // Default auto-processing is ENABLED: denoise / edge enhance would
            // rewrite desktop pixels on every Blt. Force it off.
            vctx.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);

            Ok(Self {
                vdev,
                vctx,
                enumr,
                vp,
            })
        }
    }

    /// `input` (BGRA, or scRGB FP16 if built with `scrgb_input`) → `output`
    /// (NV12, BT.709 studio — never P010). Views are per call so the input
    /// texture can vary frame to frame.
    pub fn convert(&self, input: &ID3D11Texture2D, output: &ID3D11Texture2D) -> Result<()> {
        // SAFETY: both view creations are `?`-checked calls on `self.vdev` with fully-initialized
        // stack descriptors and live out-params. `stream.pInputSurface` is a `ManuallyDrop` of the
        // input view just created: `VideoProcessorBlt` only BORROWS it (a COM in-param never transfers
        // ownership), and the explicit `into_inner` drop below releases that reference exactly once on
        // both the success and the failure path. `slice::from_ref(&stream)` borrows the live local.
        unsafe {
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
            self.vdev
                .CreateVideoProcessorInputView(input, &self.enumr, &in_desc, Some(&mut in_view))
                .context("CreateVideoProcessorInputView")?;

            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
            self.vdev
                .CreateVideoProcessorOutputView(output, &self.enumr, &out_desc, Some(&mut out_view))
                .context("CreateVideoProcessorOutputView")?;
            let out_view = out_view.context("null output view")?;

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(in_view),
                ..Default::default()
            };
            let blt =
                self.vctx
                    .VideoProcessorBlt(&self.vp, &out_view, 0, std::slice::from_ref(&stream));
            // Blt only borrows the input view; `ManuallyDrop` suppressed Drop.
            // Release once on both paths — skipping this leaked one view per frame.
            drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
            blt.context("VideoProcessorBlt")
        }
    }
}
