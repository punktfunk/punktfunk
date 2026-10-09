//! The D3D11 frames the Windows wave smokes feed their encoders: the
//! [`crate::smoke_pattern`] texture as the capturer hands one over.

/// An NV12 D3D11 texture on `device` with `bind` flags, holding `nv12` (Y then UV, pitch `w`)
/// or left undefined.
pub fn nv12_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    w: u32,
    h: u32,
    nv12: Option<&[u8]>,
    bind: u32,
) -> windows::Win32::Graphics::Direct3D11::ID3D11Texture2D {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
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
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = nv12.map(|b| D3D11_SUBRESOURCE_DATA {
        pSysMem: b.as_ptr() as *const _,
        SysMemPitch: w,
        SysMemSlicePitch: 0,
    });
    let mut tex = None;
    // SAFETY: `init` points at `nv12`, alive across the call; the UV plane follows the Y plane at
    // the same pitch, the layout D3D11 reads NV12 initial data in. The out-param fills only on
    // success.
    unsafe { device.CreateTexture2D(&desc, init.as_ref().map(|i| i as *const _), Some(&mut tex)) }
        .expect("NV12 texture");
    tex.expect("NV12 texture")
}

/// A BGRA D3D11 texture on `device` with `bind` flags, holding `bgra` (pitch `w * 4`) or left
/// undefined: what the display composes, before any conversion.
pub fn bgra_texture(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    w: u32,
    h: u32,
    bgra: Option<&[u8]>,
    bind: u32,
) -> windows::Win32::Graphics::Direct3D11::ID3D11Texture2D {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = bgra.map(|b| D3D11_SUBRESOURCE_DATA {
        pSysMem: b.as_ptr() as *const _,
        SysMemPitch: w * 4,
        SysMemSlicePitch: 0,
    });
    let mut tex = None;
    // SAFETY: `init` points at `bgra`, `w * h * 4` bytes alive across the call, read at the
    // pitch given. The out-param fills only on success.
    unsafe { device.CreateTexture2D(&desc, init.as_ref().map(|i| i as *const _), Some(&mut tex)) }
        .expect("BGRA texture");
    tex.expect("BGRA texture")
}

/// An NV12 D3D11 frame of the pattern at `frame` frames of motion, as the capturer hands one
/// to a Windows encoder. `bind` as [`nv12_texture`].
pub fn nv12_scroll_frame(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    w: u32,
    h: u32,
    frame: usize,
    bind: u32,
) -> pf_frame::CapturedFrame {
    let nv12 = crate::smoke_pattern::scroll_pattern_nv12(w as usize, h as usize, frame);
    pf_frame::CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns: frame as u64,
        format: pf_frame::PixelFormat::Nv12,
        payload: pf_frame::FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
            texture: nv12_texture(device, w, h, Some(&nv12), bind),
            device: device.clone(),
            pyro: None,
        }),
        cursor: None,
    }
}
