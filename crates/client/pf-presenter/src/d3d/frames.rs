//! The frames the CSC pass samples. The CPU I420 lane: three `R8_UNORM` textures sized by
//! `CpuPlanarFrame::plane_dims`, filled by `UpdateSubresource`. A D3D11VA pool slice:
//! sampled in place on the decoder's own device, after a GPU wait on its `written` fence,
//! through two plane views cached per pool generation; no copy, no `Map`, no CPU wait.

use crate::slice_model::{uv_scale, ViewCache};
use anyhow::{bail, Context as _, Result};
use pf_client_core::video::{ColorDesc, CpuPlanarFrame, PoolSlice, SliceGuard, SlotFormat};
use windows::Win32::d3d11::{
    ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11ShaderResourceView, ID3D11Texture2D,
    D3D11_BIND_SHADER_RESOURCE, D3D11_SHADER_RESOURCE_VIEW_DESC, D3D11_SHADER_RESOURCE_VIEW_DESC_0,
    D3D11_TEX2D_ARRAY_SRV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::d3dcommon::D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
use windows::Win32::dxgi::{
    DXGI_FORMAT, DXGI_FORMAT_R16G16_UNORM, DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R8G8_UNORM,
    DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC,
};

/// A pool slice's luma and CbCr views.
pub(super) type PlaneViews = [Option<ID3D11ShaderResourceView>; 2];

pub(super) struct CpuPlanes {
    /// Luma size; the chroma planes follow `CpuPlanarFrame::plane_dims`.
    size: (u32, u32),
    textures: [ID3D11Texture2D; 3],
    pub(super) views: [Option<ID3D11ShaderResourceView>; 3],
}

impl CpuPlanes {
    fn new(device: &ID3D11Device5, f: &CpuPlanarFrame) -> Result<CpuPlanes> {
        let plane = |i: usize| -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
            let (width, height) = f.plane_dims(i);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let (mut texture, mut view) = (None, None);
            // SAFETY: COM calls on the live device over the local descriptor; each out-param
            // is a local `Option` checked below.
            unsafe {
                device
                    .CreateTexture2D(&desc, None, Some(&mut texture))
                    .ok()
                    .context("create a CPU plane texture")?;
                let texture = texture.as_ref().context("CreateTexture2D returned none")?;
                device
                    .CreateShaderResourceView(texture, None, Some(&mut view))
                    .ok()
                    .context("create a CPU plane view")?;
            }
            Ok((
                texture.context("CreateTexture2D returned none")?,
                view.context("CreateShaderResourceView returned none")?,
            ))
        };
        let (y, cb, cr) = (plane(0)?, plane(1)?, plane(2)?);
        Ok(CpuPlanes {
            size: (f.width, f.height),
            textures: [y.0, cb.0, cr.0],
            views: [Some(y.1), Some(cb.1), Some(cr.1)],
        })
    }

    pub(super) fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Copy `f`'s planes into `slot`, rebuilding the textures when the frame size changes.
    pub(super) fn upload(
        slot: &mut Option<CpuPlanes>,
        device: &ID3D11Device5,
        context: &ID3D11DeviceContext4,
        f: &CpuPlanarFrame,
    ) -> Result<()> {
        if slot.as_ref().is_none_or(|p| p.size != (f.width, f.height)) {
            *slot = Some(CpuPlanes::new(device, f)?);
        }
        let planes = slot.as_ref().context("CPU planes")?;
        for (i, texture) in planes.textures.iter().enumerate() {
            let data = f.plane(i);
            let (width, _) = f.plane_dims(i);
            // SAFETY: a COM call on the live context; `data` is the tightly packed plane of
            // `width` × the texture's height bytes, read before the call returns.
            unsafe {
                context.UpdateSubresource(texture, 0, None, data.as_ptr().cast(), width, 0);
            }
        }
        Ok(())
    }
}

/// The pool slice last drawn. It stays pinned so a `Redraw` can sample it again, until the
/// next frame or `drop_video` drops it; the drop hands the slice back to the decoder with
/// the last `read` value a draw of it signalled.
pub(super) struct HeldSlice {
    pub(super) views: PlaneViews,
    /// Display size; the views span the coded slice, cropped by `uv_scale`.
    pub(super) size: (u32, u32),
    pub(super) uv_scale: [f32; 2],
    pub(super) color: ColorDesc,
    p010: bool,
    read_fence: Option<ID3D11Fence>,
    read: u64,
    guard: Option<SliceGuard>,
}

impl HeldSlice {
    /// Order `context` after the decoder's write of `s` and look up its plane views. Without
    /// a `written` fence the decoder's submit on this same context already came first.
    pub(super) fn take(
        device: &ID3D11Device5,
        context: &ID3D11DeviceContext4,
        cache: &mut ViewCache<PlaneViews>,
        s: PoolSlice,
    ) -> Result<HeldSlice> {
        if let Some(fence) = &s.written_fence {
            // SAFETY: a COM call on the live immediate context with the frame's live fence;
            // the wait is queued on the GPU, the CPU does not block.
            unsafe { context.Wait(fence, s.written) }
                .ok()
                .map_err(|e| super::frame_error(e, "wait the decoder's written fence"))?;
        }
        let views = cache
            .get_or_make(s.generation, s.slice, || {
                slice_views(device, &s.texture, s.slice, s.format)
            })?
            .clone();
        Ok(HeldSlice {
            views,
            size: (s.width, s.height),
            uv_scale: uv_scale((s.width, s.height), (s.coded_width, s.coded_height)),
            color: s.color,
            p010: s.format == SlotFormat::P010,
            read_fence: s.read_fence,
            read: 0,
            guard: Some(s.guard),
        })
    }

    /// The CSC rows: P010 holds 10-bit codes in the high bits of 16, as `record_d3d11`.
    pub(super) fn rows(&self) -> [[f32; 4]; 3] {
        let (depth, msb_packed) = if self.p010 { (10, true) } else { (8, false) };
        pf_client_core::video::csc_rows(self.color, depth, msb_packed)
    }

    /// Signal `read` to `value` on `context`, after the draw that sampled the slice; without a
    /// fence only the value is kept, for the guard's release.
    pub(super) fn signal(&mut self, context: &ID3D11DeviceContext4, value: u64) -> Result<()> {
        if let Some(fence) = &self.read_fence {
            // SAFETY: a COM call on the live immediate context with the frame's live fence.
            unsafe { context.Signal(fence, value) }
                .ok()
                .map_err(|e| super::frame_error(e, "signal the presenter's read fence"))?;
        }
        self.read = value;
        Ok(())
    }
}

impl Drop for HeldSlice {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            guard.release(self.read);
        }
    }
}

/// One-slice array views of a two-plane texture's planes, in the formats D3D11 maps to
/// each plane: R8 and R8G8 for NV12, R16 and R16G16 for P010.
fn slice_views(
    device: &ID3D11Device5,
    texture: &ID3D11Texture2D,
    slice: u32,
    format: SlotFormat,
) -> Result<PlaneViews> {
    let (luma, chroma) = match format {
        SlotFormat::Nv12 => (DXGI_FORMAT_R8_UNORM, DXGI_FORMAT_R8G8_UNORM),
        SlotFormat::P010 => (DXGI_FORMAT_R16_UNORM, DXGI_FORMAT_R16G16_UNORM),
        other => bail!("a pool slice in {other:?}"),
    };
    let view = |format: DXGI_FORMAT| -> Result<ID3D11ShaderResourceView> {
        let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: format,
            ViewDimension: D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 1,
                    FirstArraySlice: slice,
                    ArraySize: 1,
                },
            },
        };
        let mut view = None;
        // SAFETY: a COM call on the live device over the frame's live texture and the local
        // descriptor; the out-param is a local `Option` checked below.
        unsafe { device.CreateShaderResourceView(texture, Some(&desc), Some(&mut view)) }
            .ok()
            .map_err(|e| super::frame_error(e, "create a pool slice view"))?;
        view.context("CreateShaderResourceView returned none")
    };
    Ok([Some(view(luma)?), Some(view(chroma)?)])
}
