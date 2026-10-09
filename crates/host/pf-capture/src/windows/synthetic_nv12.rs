//! Headless synthetic NV12 D3D11 capture for Windows GPU encoder tests.
//!
//! AMF and the D3D11 zero-copy NVENC/QSV paths need an NV12 texture on the GPU.
//! [`crate::SyntheticCapturer`] is CPU Bgrx, and DXGI Desktop Duplication is
//! denied in session-0, so this source builds NV12 on the render adapter.
//! Each frame is a moving luma ramp so the encoder sees real motion.
//!
//! Drive it with `spike --source synthetic-nv12`.

use crate::dxgi::{make_device, D3d11Frame};
use crate::{CapturedFrame, Capturer, FramePayload, PixelFormat};
use anyhow::{Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_WRITE, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_WRITE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

/// Staging is filled on the CPU; DEFAULT is what the encoder is handed.
/// Spike is capture→submit→poll on one thread, so reusing one DEFAULT is safe:
/// the encoder copies out before the next fill.
pub struct SyntheticNv12Capturer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    default_tex: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
}

// SAFETY: `make_device` omits `SINGLETHREADED`; D3D11 COM refcounts are interlocked.
// The capturer is moved onto its owner thread and is `Send` not `Sync`, so the
// single-threaded immediate context is never used concurrently.
unsafe impl Send for SyntheticNv12Capturer {}

impl SyntheticNv12Capturer {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        // NV12 is 4:2:0 — both dimensions must be even (the chroma plane is width/2 × height/2).
        let width = (width & !1).max(2);
        let height = (height & !1).max(2);
        // SAFETY: every COM handle created here is owned by `Self` or dropped on `?`.
        unsafe {
            let adapter =
                resolve_render_adapter().context("resolve render adapter for NV12 source")?;
            let (device, context) = make_device(&adapter).context("create D3D11 device")?;
            let default_tex = create_nv12(
                &device,
                width,
                height,
                D3D11_USAGE_DEFAULT,
                0,
                D3D11_BIND_SHADER_RESOURCE.0 as u32,
            )
            .context("create NV12 default texture")?;
            let staging = create_nv12(
                &device,
                width,
                height,
                D3D11_USAGE_STAGING,
                D3D11_CPU_ACCESS_WRITE.0 as u32,
                0,
            )
            .context("create NV12 staging texture")?;
            Ok(SyntheticNv12Capturer {
                device,
                context,
                default_tex,
                staging,
                width,
                height,
                fps,
                frame_idx: 0,
            })
        }
    }
}

impl Capturer for SyntheticNv12Capturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let pts_ns = self.frame_idx * 1_000_000_000 / self.fps.max(1) as u64;
        // SAFETY: Map/Unmap/CopyResource on this capturer's own single-threaded immediate context;
        // all writes stay within the mapped NV12 surface (Y: H rows of RowPitch; UV: H/2 rows of
        // RowPitch beginning at RowPitch*H — the standard NV12 plane layout).
        unsafe {
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_WRITE, 0, Some(&mut map))
                .context("Map(NV12 staging)")?;
            let pitch = map.RowPitch as usize;
            let base = map.pData as *mut u8;
            // Diagonal luma ramp, +4 codes/frame — deterministic motion the encoder must see.
            let shift = (self.frame_idx as u32).wrapping_mul(4);
            for y in 0..self.height {
                let row = base.add(y as usize * pitch);
                for x in 0..self.width {
                    *row.add(x as usize) = x.wrapping_add(y).wrapping_add(shift) as u8;
                }
            }
            // Neutral chroma (128) at RowPitch*H: H/2 rows, `width` bytes (interleaved Cb,Cr).
            let uv = base.add(pitch * self.height as usize);
            for r in 0..(self.height / 2) {
                let row = uv.add(r as usize * pitch);
                for c in 0..self.width {
                    *row.add(c as usize) = 128;
                }
            }
            self.context.Unmap(&self.staging, 0);
            self.context.CopyResource(&self.default_tex, &self.staging);
        }
        self.frame_idx += 1;
        Ok(CapturedFrame {
            provenance: Default::default(),
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(D3d11Frame {
                texture: self.default_tex.clone(),
                device: self.device.clone(),
                pyro: None,
            }),
            cursor: None,
        })
    }
}

/// Same render adapter the encoder picks (`PUNKTFUNK_RENDER_ADAPTER` / preference /
/// max-VRAM LUID), else adapter 0.
fn resolve_render_adapter() -> Result<IDXGIAdapter1> {
    if let Some(a) = pf_frame::dxgi::adapter_by_luid(pf_gpu::resolve_render_adapter_luid()) {
        return Ok(a);
    }
    // SAFETY: DXGI enumeration over owned locals; factory and adapter own their COM refs.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        factory.EnumAdapters1(0).context("EnumAdapters1(0)")
    }
}

fn create_nv12(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    usage: D3D11_USAGE,
    cpu_access: u32,
    bind: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu_access,
        ..Default::default()
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    // SAFETY: one `?`-checked `CreateTexture2D` on the `&ID3D11Device` borrow, which the borrow
    // itself keeps live, with a fully-initialized stack descriptor and a live `Option` out-param.
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut tex))
            .context("CreateTexture2D(NV12)")?;
    }
    tex.context("CreateTexture2D returned a null NV12 texture")
}
