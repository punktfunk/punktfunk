//! A D3D11 `Map` held as a borrow: the mapped bytes are a slice and `Unmap` runs on drop.

use std::ops::Deref;
use std::ptr::NonNull;

use anyhow::{bail, Context, Result};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Buffer, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BUFFER_DESC,
    D3D11_MAP, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_FORMAT_P016,
};

/// Subresource 0 of a texture, or a whole buffer, mapped on its context. Derefs to the mapped
/// bytes; [`Self::bytes_mut`] writes them. No early return strands the mapping.
pub struct MappedSubresource<'a> {
    ctx: &'a ID3D11DeviceContext,
    res: &'a ID3D11Resource,
    ptr: NonNull<u8>,
    len: usize,
    row_pitch: usize,
    depth_pitch: usize,
    writable: bool,
}

impl<'a> MappedSubresource<'a> {
    /// Map a 2D texture: its rows, [`Self::row_pitch`] bytes apart. NV12, P010 and P016 carry
    /// their chroma rows after the luma rows at the same pitch.
    pub fn texture(
        ctx: &'a ID3D11DeviceContext,
        tex: &'a ID3D11Texture2D,
        how: D3D11_MAP,
    ) -> Result<Self> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `desc` is a live out-param.
        unsafe { tex.GetDesc(&mut desc) };
        let chroma = match desc.Format {
            DXGI_FORMAT_NV12 | DXGI_FORMAT_P010 | DXGI_FORMAT_P016 => desc.Height / 2,
            _ => 0,
        };
        let rows = (desc.Height + chroma) as usize;
        Self::map(ctx, tex, how, |pitch| pitch * rows)
    }

    /// Map a buffer: its `ByteWidth` bytes.
    pub fn buffer(
        ctx: &'a ID3D11DeviceContext,
        buf: &'a ID3D11Buffer,
        how: D3D11_MAP,
    ) -> Result<Self> {
        let mut desc = D3D11_BUFFER_DESC::default();
        // SAFETY: `desc` is a live out-param.
        unsafe { buf.GetDesc(&mut desc) };
        Self::map(ctx, buf, how, |_| desc.ByteWidth as usize)
    }

    /// `len` turns the mapped row pitch into the byte count D3D11 maps for this resource.
    fn map(
        ctx: &'a ID3D11DeviceContext,
        res: &'a ID3D11Resource,
        how: D3D11_MAP,
        len: impl FnOnce(usize) -> usize,
    ) -> Result<Self> {
        let mut m = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `res` is a live resource and `m` a live out-param.
        unsafe { ctx.Map(res, 0, how, 0, Some(&mut m)) }.context("map d3d11 subresource")?;
        let Some(ptr) = NonNull::new(m.pData.cast::<u8>()) else {
            // SAFETY: the map above succeeded on `ctx`.
            unsafe { ctx.Unmap(res, 0) };
            bail!("d3d11 map returned no pointer");
        };
        let row_pitch = m.RowPitch as usize;
        Ok(Self {
            ctx,
            res,
            ptr,
            len: len(row_pitch),
            row_pitch,
            depth_pitch: m.DepthPitch as usize,
            writable: how != D3D11_MAP_READ,
        })
    }

    /// Bytes from one row's start to the next.
    pub fn row_pitch(&self) -> usize {
        self.row_pitch
    }

    /// The mapped `DepthPitch`, for logs. D3D11 defines it only between depth slices and leaves
    /// it unused on a 2D texture, so the slice length never comes from it.
    pub fn depth_pitch(&self) -> usize {
        self.depth_pitch
    }

    /// The mapped bytes, for writing. Panics on a `D3D11_MAP_READ` mapping.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        assert!(self.writable, "subresource is mapped read-only");
        // SAFETY: as `deref`, and `&mut self` makes this the only view of the bytes.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Deref for MappedSubresource<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: D3D11 keeps a texture's rows, each `RowPitch` bytes, or a buffer's
        // `ByteWidth` bytes mapped at `ptr` until the `Unmap` in `drop`. It maps a subresource
        // once at a time, so no second guard writes them.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MappedSubresource<'_> {
    fn drop(&mut self) {
        // SAFETY: `self` holds this subresource's one mapping, taken on `ctx`.
        unsafe { self.ctx.Unmap(self.res, 0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_MAP_WRITE,
        D3D11_SDK_VERSION, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

    /// An NV12 staging texture on WARP: written through one guard, read back through the next.
    /// The second map succeeding is the proof the first guard unmapped.
    #[test]
    fn a_dropped_guard_unmaps_and_the_bytes_persist() {
        let (mut dev, mut ctx) = (None, None);
        // SAFETY: plain device creation; both out-params are live locals.
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
        }
        .expect("WARP device");
        let (dev, ctx) = (dev.expect("device"), ctx.expect("context"));
        let (w, h) = (64u32, 32u32);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
            ..Default::default()
        };
        let mut tex = None;
        // SAFETY: a complete description and a live out-param.
        unsafe { dev.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("staging texture");
        let tex = tex.expect("staging texture");

        let last_chroma_row = (h + h / 2 - 1) as usize;
        {
            let mut m = MappedSubresource::texture(&ctx, &tex, D3D11_MAP_WRITE).expect("map");
            let pitch = m.row_pitch();
            assert_eq!(m.len(), pitch * (h + h / 2) as usize);
            m.bytes_mut()[last_chroma_row * pitch..][..w as usize].fill(0xA5);
        }
        let m = MappedSubresource::texture(&ctx, &tex, D3D11_MAP_READ).expect("second map");
        let row = &m[last_chroma_row * m.row_pitch()..][..w as usize];
        assert!(row.iter().all(|&b| b == 0xA5));
    }
}
