//! The scale pass a framed session runs before its converter: one rectangle of each source
//! picture, scaled to the encode size in two separable Lanczos-3 passes and written in the
//! source's own format, so every converter after it runs unchanged. The kernel and the edge rule
//! are the Linux `reframe.comp`'s: the footprint widens by the downscale ratio, a 1:1 axis copies,
//! taps clamp inside the crop, and an FP16 picture sits between the passes. The pointer is not
//! this pass's: the caller blends it into the source picture first, so it scales with it.

use super::{compile_shader, HDR_VS};
use anyhow::{ensure, Context, Result};
use windows::core::s;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11Device, ID3D11DeviceContext, ID3D11PixelShader, ID3D11RenderTargetView,
    ID3D11ShaderResourceView, ID3D11Texture2D, ID3D11VertexShader, D3D11_BIND_CONSTANT_BUFFER,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BUFFER_DESC,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_IMMUTABLE,
    D3D11_VIEWPORT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC};

/// One axis per draw: x from the crop into the FP16 picture, then y into the target. `linear`
/// is an FP16 scRGB source, whose values run past 1.0: its ringing clamps to the taps' own range.
/// An 8-bit source clamps to [0, 1], as on Linux.
const REFRAME_PS: &str = r"
Texture2D<float4> tx : register(t0);
cbuffer C : register(b0) {
    int2 origin;   // source texel at the crop's top-left
    int2 extent;   // crop size in source texels
    float stepv;   // source texels per output pixel on the filtered axis
    int axis;      // 0 = filter x, 1 = filter y
    int scrgb;     // `linear` is an HLSL keyword
    int pad;
};
// Footprint cap: a 10x downscale still fits the tap loop.
static const float MAX_STRETCH = 10.0;
static const int MAX_TAPS = 64;
float lanczos3(float x) {
    x = abs(x);
    if (x < 1e-5) return 1.0;
    if (x >= 3.0) return 0.0;
    float px = 3.14159265358979 * x;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}
float4 main(float4 pos : SV_POSITION) : SV_TARGET {
    int2 o = int2(pos.xy);
    int2 dir = axis == 0 ? int2(1, 0) : int2(0, 1);
    int2 base = (origin + o) * (int2(1, 1) - dir);
    int lo = dot(origin, dir);
    int hi = lo + dot(extent, dir) - 1;
    // Source coordinate of this output pixel's centre; texel i covers [i, i+1).
    float s = float(lo) + (float(dot(o, dir)) + 0.5) * stepv;
    float3 acc;
    if (stepv == 1.0) {
        acc = tx.Load(int3(base + dir * clamp(int(floor(s)), lo, hi), 0)).rgb;
    } else {
        float stretch = clamp(stepv, 1.0, MAX_STRETCH);
        float support = 3.0 * stretch;
        int first = int(floor(s - support - 0.5)) + 1;
        int last = min(int(ceil(s + support - 0.5)) - 1, first + MAX_TAPS - 1);
        float3 mn = 65504.0;
        float3 mx = -65504.0;
        float total = 0.0;
        acc = 0.0;
        [loop] for (int i = first; i <= last; i++) {
            float w = lanczos3((float(i) + 0.5 - s) / stretch);
            float3 c = tx.Load(int3(base + dir * clamp(i, lo, hi), 0)).rgb;
            acc += w * c;
            total += w;
            mn = min(mn, c);
            mx = max(mx, c);
        }
        acc /= total;
        if (scrgb != 0) acc = clamp(acc, mn, mx);
    }
    return float4(scrgb != 0 ? acc : saturate(acc), 1.0);
}
";

/// The two draws that scale `crop` of a source picture to `out`. One per framing: the crop, the
/// size and the FP16 picture between the passes are fixed at construction.
pub struct ReframePass {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    /// `out.0 × crop[3]`, FP16: the x pass's target, the y pass's source.
    tmp_rtv: ID3D11RenderTargetView,
    tmp_srv: ID3D11ShaderResourceView,
    /// The x pass's constants, then the y pass's.
    cb: [ID3D11Buffer; 2],
    crop: [u32; 4],
    out: (u32, u32),
}

impl ReframePass {
    /// `crop` is `x, y, width, height` in source pixels. `linear` is an FP16 scRGB source.
    pub fn new(
        device: &ID3D11Device,
        crop: [u32; 4],
        out: (u32, u32),
        linear: bool,
    ) -> Result<Self> {
        let [cx, cy, cw, ch] = crop;
        let (ow, oh) = out;
        ensure!(
            cw > 0 && ch > 0 && ow > 0 && oh > 0,
            "reframe {crop:?} to {ow}x{oh} has an empty side"
        );
        let consts = |origin: [u32; 2], extent: [u32; 2], step: f32, axis: i32| -> [i32; 8] {
            [
                origin[0] as i32,
                origin[1] as i32,
                extent[0] as i32,
                extent[1] as i32,
                i32::from_ne_bytes(step.to_ne_bytes()),
                axis,
                i32::from(linear),
                0,
            ]
        };
        let x = consts([cx, cy], [cw, ch], cw as f32 / ow as f32, 0);
        let y = consts([0, 0], [ow, ch], ch as f32 / oh as f32, 1);
        // SAFETY: `?`-checked D3D11 creation on the live `device` borrow, over fully-initialized
        // stack descriptors; each `pSysMem` points at an 8-word local that outlives its
        // synchronous `CreateBuffer`. `compile_shader` receives `s!()` literals (its contract).
        unsafe {
            let vsb = compile_shader(HDR_VS, s!("main"), s!("vs_5_0"))?;
            let psb = compile_shader(REFRAME_PS, s!("main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vsb, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(&psb, None, Some(&mut ps))?;
            let mut cb = Vec::with_capacity(2);
            for words in [&x, &y] {
                let desc = D3D11_BUFFER_DESC {
                    ByteWidth: 32,
                    Usage: D3D11_USAGE_IMMUTABLE,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                };
                let init = D3D11_SUBRESOURCE_DATA {
                    pSysMem: words.as_ptr().cast(),
                    SysMemPitch: 0,
                    SysMemSlicePitch: 0,
                };
                let mut buf = None;
                device.CreateBuffer(&desc, Some(&init), Some(&mut buf))?;
                cb.push(buf.context("reframe constants")?);
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: ow,
                Height: ch,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                ..Default::default()
            };
            let mut tmp: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut tmp))
                .context("CreateTexture2D(reframe FP16 picture)")?;
            let tmp = tmp.context("reframe FP16 picture")?;
            let mut tmp_rtv = None;
            device.CreateRenderTargetView(&tmp, None, Some(&mut tmp_rtv))?;
            let mut tmp_srv = None;
            device.CreateShaderResourceView(&tmp, None, Some(&mut tmp_srv))?;
            let [cb_x, cb_y]: [ID3D11Buffer; 2] = cb
                .try_into()
                .map_err(|_| anyhow::anyhow!("reframe constants"))?;
            Ok(Self {
                vs: vs.context("reframe vs")?,
                ps: ps.context("reframe ps")?,
                tmp_rtv: tmp_rtv.context("reframe rtv")?,
                tmp_srv: tmp_srv.context("reframe srv")?,
                cb: [cb_x, cb_y],
                crop,
                out,
            })
        }
    }

    /// Both draws: `src`, a source picture that holds the crop, into `dst`, an `out`-sized
    /// target in the source's format.
    pub fn scale(
        &self,
        ctx: &ID3D11DeviceContext,
        src: &ID3D11ShaderResourceView,
        dst: &ID3D11RenderTargetView,
    ) {
        let (ow, oh) = self.out;
        let viewport = |w: u32, h: u32| D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: w as f32,
            Height: h as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        // SAFETY: state setting and two `Draw`s on the caller's live context, over borrowed
        // slices of initialized locals and clones of live views. The FP16 picture leaves the
        // output merger before the y pass reads it, and every binding is cleared after.
        unsafe {
            ctx.OMSetBlendState(None, None, 0xffff_ffff);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);

            ctx.OMSetRenderTargets(Some(&[Some(self.tmp_rtv.clone())]), None);
            ctx.PSSetShaderResources(0, Some(&[Some(src.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb[0].clone())]));
            ctx.RSSetViewports(Some(&[viewport(ow, self.crop[3])]));
            ctx.Draw(3, 0);

            ctx.OMSetRenderTargets(Some(&[Some(dst.clone())]), None);
            ctx.PSSetShaderResources(0, Some(&[Some(self.tmp_srv.clone())]));
            ctx.PSSetConstantBuffers(0, Some(&[Some(self.cb[1].clone())]));
            ctx.RSSetViewports(Some(&[viewport(ow, oh)]));
            ctx.Draw(3, 0);

            ctx.OMSetRenderTargets(Some(&[None]), None);
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
    }
}

/// The f64 model of [`REFRAME_PS`] over both passes, the FP16 picture between them rounded as
/// the GPU stores it. `src` is `source.0 × source.1` RGB.
pub(super) fn reframe_reference(
    src: &[[f32; 3]],
    source: (u32, u32),
    crop: [u32; 4],
    out: (u32, u32),
    linear: bool,
) -> Vec<[f32; 3]> {
    let [cx, cy, cw, ch] = crop.map(|v| v as usize);
    let (ow, oh) = (out.0 as usize, out.1 as usize);
    let sw = source.0 as usize;
    let tmp: Vec<[f32; 3]> = (0..ch * ow)
        .map(|i| {
            let (x, y) = (i % ow, i / ow);
            let px = axis_sample(x, cx, cw, cw as f64 / ow as f64, linear, |sx| {
                src[(cy + y) * sw + sx]
            });
            px.map(|c| super::selftest::f16_to_f32(super::selftest::f32_to_f16(c)))
        })
        .collect();
    (0..oh * ow)
        .map(|i| {
            let (x, y) = (i % ow, i / ow);
            axis_sample(y, 0, ch, ch as f64 / oh as f64, linear, |sy| {
                tmp[sy * ow + x]
            })
        })
        .collect()
}

/// One output pixel `o` of one axis, the shader's loop: `tap(i)` reads source position `i`
/// on that axis, already clamped inside `[lo, lo + len)`.
fn axis_sample(
    o: usize,
    lo: usize,
    len: usize,
    step: f64,
    linear: bool,
    tap: impl Fn(usize) -> [f32; 3],
) -> [f32; 3] {
    let lanczos3 = |x: f64| -> f64 {
        let x = x.abs();
        if x < 1e-5 {
            return 1.0;
        }
        if x >= 3.0 {
            return 0.0;
        }
        let px = std::f64::consts::PI * x;
        3.0 * px.sin() * (px / 3.0).sin() / (px * px)
    };
    let hi = (lo + len - 1) as i64;
    let at = |i: i64| tap(i.clamp(lo as i64, hi) as usize);
    let s = lo as f64 + (o as f64 + 0.5) * step;
    if step == 1.0 {
        return at(s.floor() as i64);
    }
    let stretch = step.clamp(1.0, 10.0);
    let support = 3.0 * stretch;
    let first = (s - support - 0.5).floor() as i64 + 1;
    let last = ((s + support - 0.5).ceil() as i64 - 1).min(first + 63);
    let (mut acc, mut total) = ([0.0f64; 3], 0.0f64);
    let (mut mn, mut mx) = ([f64::MAX; 3], [f64::MIN; 3]);
    for i in first..=last {
        let w = lanczos3((i as f64 + 0.5 - s) / stretch);
        let c = at(i);
        for k in 0..3 {
            let v = f64::from(c[k]);
            acc[k] += w * v;
            mn[k] = mn[k].min(v);
            mx[k] = mx[k].max(v);
        }
        total += w;
    }
    std::array::from_fn(|k| {
        let v = acc[k] / total;
        let v = if linear {
            v.clamp(mn[k], mx[k])
        } else {
            v.clamp(0.0, 1.0)
        };
        v as f32
    })
}
