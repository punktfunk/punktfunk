//! D3D11 decode device and shareable hand-off ring for the Windows DXVA rung.
//! [`crate::video_d3d11_native`] fills the decode surfaces; this module hands each slice
//! to the presenter (`pf-presenter/src/d3d11.rs`, `VK_KHR_external_memory_win32`),
//! converted to RGBA by `ID3D11VideoProcessor`, or copied into a two-plane NV12/P010 slot
//! the presenter's CSC pass samples when it can import one ([`HandoffRing::set_planar`]).
//!
//! Auto's first choice on Intel — the driver advertises Vulkan Video, but that
//! decode path is not the shipping one. NVIDIA/AMD fall back here below Vulkan
//! Video, including mid-session demotion.
//!
//! Decode surfaces carry no share flags; slots carry `SHARED_NTHANDLE |
//! SHARED_KEYEDMUTEX`. Both sides acquire the keyed mutex with **key 0**; a dropped frame
//! is never acquired, which a ping-pong key would deadlock on. The device is
//! created on the presenter's adapter (Vulkan LUID) so shares stay on one GPU.
//! A frame keeps its slot's NT handle open ([`SlotHandle`]): a ring rebuilt
//! under queued frames never hands the presenter a closed handle.
//!
//! PQ streams pass through as RGB10A2 when the presenter has an HDR10 swapchain
//! ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`]); otherwise the processor
//! tone-maps to sRGB. [`HandoffRing`] is `pub(crate)` for [`crate::video_d3d11_native`].

use crate::video::ColorDesc;
use anyhow::{anyhow, Context as _, Result};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::core::Interface;
use windows::Win32::d3d11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device5, ID3D11DeviceContext, ID3D11Fence,
    ID3D11Multithread, ID3D11Query, ID3D11Texture2D, ID3D11VideoContext1, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorEnumerator1,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView, D3D11_ASYNC_GETDATA_DONOTFLUSH,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_SHARED,
    D3D11_QUERY_DATA_TIMESTAMP_DISJOINT, D3D11_QUERY_DESC, D3D11_QUERY_TIMESTAMP,
    D3D11_QUERY_TIMESTAMP_DISJOINT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::d3dcommon::{D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_TYPE,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
    DXGI_FORMAT_P010, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE,
};
use windows::Win32::windef::RECT;
use windows::Win32::winnt::{GENERIC_ALL, HANDLE};

/// Six slots: the pump holds 2 decoded frames and the presenter has one in flight, so 3 are
/// outstanding. Double that leaves margin without meaningful VRAM cost.
const RING_SLOTS: usize = 6;

/// Decode-side keyed-mutex acquire budget, milliseconds. The presenter holds a slot for one
/// submit; a multi-second wait means the render thread died — error (and demote) rather than
/// wedge the decode loop.
const ACQUIRE_TIMEOUT_MS: u32 = 2000;

/// An acquire that waited this long met the presenter still reading the slot.
const ACQUIRE_STALL: Duration = Duration::from_millis(1);

/// NT handle of one ring slot, closed on the last drop. The ring holds one reference and
/// every [`D3d11Frame`] handed off holds another, so a rebuild cannot close a handle the
/// presenter still imports or is about to.
pub struct SlotHandle(HANDLE);

// SAFETY: an NT handle is a process-wide kernel reference with no thread affinity; the only
// operation on it is one `CloseHandle` from `Drop`.
unsafe impl Send for SlotHandle {}
// SAFETY: `raw` reads a plain value; nothing mutates the handle before the single close.
unsafe impl Sync for SlotHandle {}

impl SlotHandle {
    /// Raw handle value for `VkImportMemoryWin32HandleInfoKHR`; valid while `self` lives.
    pub fn raw(&self) -> isize {
        let HANDLE(p) = &self.0;
        *p as isize
    }
}

impl Drop for SlotHandle {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the shared-texture NT handle this value owns; `Drop` runs once,
        // so it is closed exactly once and never used after.
        unsafe {
            let _ = windows::Win32::handleapi::CloseHandle(self.0);
        }
    }
}

/// Pixel format of a hand-off slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotFormat {
    /// Video-processor output, sRGB.
    Bgra8,
    /// Video-processor output, PQ pass-through.
    Rgb10a2,
    /// Two-plane copies of the decoded picture; the presenter's CSC pass converts them.
    Nv12,
    P010,
}

/// One decoded frame in a ring slot the presenter imports by NT handle. Exclusion and
/// visibility ride the slot's keyed mutex (key 0), not this struct.
pub struct D3d11Frame {
    pub width: u32,
    pub height: u32,
    /// Colour of the slot's contents: sRGB or PQ RGB after the video processor, the
    /// stream's own YCbCr signalling in a planar slot. The presenter keys SDR/HDR off this.
    pub color: ColorDesc,
    /// What the slot holds; the presenter's Vulkan import must match it.
    pub format: SlotFormat,
    /// Intra (IDR/I) — the pump's post-loss re-anchor. See [`crate::video::DecodedImage::is_keyframe`].
    pub keyframe: bool,
    /// Whole prediction chain was fully available. Corroborates a host
    /// `USER_FLAG_RECOVERY_ANCHOR`: see [`crate::video::DecodedImage::anchor_evidence`].
    pub references_clean: bool,
    /// Slot NT handle (`CreateSharedHandle`), shared with the ring so it outlives a rebuild
    /// while this frame is queued or imported.
    pub handle: Arc<SlotHandle>,
    /// Bumped when the ring is rebuilt (size or flavour change). The presenter's import
    /// cache keys on `(generation, handle)`, so a reused handle value cannot alias.
    pub generation: u32,
}

/// Decode device on the presenter's adapter. `luid` is the Vulkan `deviceLUID`
/// (little-endian LowPart‖HighPart); a match keeps shares on one GPU. `None` or no match
/// uses the first hardware adapter. A WARP-only box fails out.
pub(crate) fn create_device(luid: Option<[u8; 8]>) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // SAFETY: DXGI factory creation takes no pointer and returns an owned factory or an error,
    // checked by `?`.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut chosen: Option<IDXGIAdapter1> = None;
    let mut fallback: Option<IDXGIAdapter1> = None;
    for i in 0.. {
        // SAFETY: a COM call on the live factory; the `Ok` binding is what proves an adapter came
        // back.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: `DXGI_ADAPTER_DESC1` is plain-old-data, so all-zeroes is a valid value.
        let mut desc: DXGI_ADAPTER_DESC1 = unsafe { std::mem::zeroed() };
        // SAFETY: a COM call on the adapter just enumerated, filling the zeroed local descriptor
        // through the out-param; checked before the descriptor is read.
        if unsafe { adapter.GetDesc1(&mut desc) }.is_err() {
            continue;
        }
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE as u32 != 0 {
            continue; // WARP cannot hardware-decode
        }
        if fallback.is_none() {
            fallback = Some(adapter.clone());
        }
        if let Some(want) = luid {
            let mut have = [0u8; 8];
            have[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
            have[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
            if have == want {
                chosen = Some(adapter);
                break;
            }
        }
    }
    if chosen.is_none() && luid.is_some() && fallback.is_some() {
        tracing::warn!(
            "no DXGI adapter matches the Vulkan device LUID — using the first hardware adapter"
        );
    }
    let adapter = chosen
        .or(fallback)
        .ok_or_else(|| anyhow!("no hardware DXGI adapter"))?;
    let mut device = None;
    let mut context = None;
    // SAFETY: `adapter` is the live adapter chosen above; the two out-params are local `Option`s
    // the callee only writes, and both are checked before use.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            windows::Win32::d3dcommon::D3D_DRIVER_TYPE_UNKNOWN,
            windows::Win32::minwindef::HINSTANCE(std::ptr::null_mut()),
            (D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT) as u32,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION as u32,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .ok()
    .context("D3D11CreateDevice")?;
    let device = device.ok_or_else(|| anyhow!("D3D11CreateDevice returned no device"))?;
    let context = context.ok_or_else(|| anyhow!("D3D11CreateDevice returned no context"))?;
    // Decode and driver threads both touch this device; without multithread protection
    // those concurrent COM calls race.
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        // SAFETY: a COM call on the live `ID3D11Multithread` from a checked `cast`; it takes a
        // BOOL and returns the previous protection state, which we ignore.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((device, context))
}

/// Whether this adapter's video processor can convert a PQ decode surface to sRGB —
/// the tonemap [`HandoffRing::present`] uses when the presenter has no HDR10 swapchain
/// ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`] false).
///
/// Setting the colorspaces is not a negotiation: `VideoProcessorSetStream/OutputColorSpace1`
/// accept anything and `VideoProcessorBlt` succeeds either way. A driver that cannot
/// convert renders garbage. Probe the SDR-ring pair: P010 `YCBCR_STUDIO_G2084_LEFT_P2020`
/// in, BGRA8 `RGB_FULL_G22_NONE_P709` out. Only a definitive "no" answers `false`; an
/// API failure answers `true` — a box whose D3D11 fails the probe fails D3D11VA
/// construction too. Paid once per connect; [`crate::video::hdr_presentable`] skips it
/// elsewhere.
pub(crate) fn pq_tonemap_supported(luid: Option<[u8; 8]>) -> bool {
    fn probe(luid: Option<[u8; 8]>) -> Result<bool> {
        let (device, _context) = create_device(luid)?;
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("device lacks ID3D11VideoDevice (created without VIDEO_SUPPORT)")?;
        // The enumerator wants a content shape; conversion support is a format/colorspace
        // fact, so any plausible size asks the same question.
        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: 1920,
            InputHeight: 1080,
            OutputFrameRate: rate,
            OutputWidth: 1920,
            OutputHeight: 1080,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: COM calls on the live device/enumerator just created, over a borrowed
        // fully-initialized stack descriptor; the conversion query fills a BOOL by value.
        unsafe {
            let enumerator = video_device
                .CreateVideoProcessorEnumerator(&desc)
                .context("CreateVideoProcessorEnumerator")?;
            let enumerator1: ID3D11VideoProcessorEnumerator1 = enumerator
                .cast()
                .context("enumerator lacks ID3D11VideoProcessorEnumerator1 (pre-Win10?)")?;
            let ok = enumerator1
                .CheckVideoProcessorFormatConversion(
                    DXGI_FORMAT_P010,
                    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709,
                )
                .context("CheckVideoProcessorFormatConversion")?;
            Ok(ok.as_bool())
        }
    }
    match probe(luid) {
        Ok(supported) => {
            if !supported {
                tracing::warn!(
                    "video processor reports NO P010 PQ→sRGB conversion — a PQ stream on the \
                     D3D11VA rung would render garbage (green) instead of tone-mapping"
                );
            }
            supported
        }
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"),
                "PQ tonemap probe failed — assuming supported");
            true
        }
    }
}

/// Shareable RGBA slot. The NT handle closes with the last [`SlotHandle`] reference.
struct Slot {
    /// Shared texture the views below point at; kept so they stay valid.
    _tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: Arc<SlotHandle>,
    out_view: ID3D11VideoProcessorOutputView,
}

/// One shareable slot texture with its keyed mutex and NT handle.
fn shared_texture(
    device: &ID3D11Device,
    desc: &D3D11_TEXTURE2D_DESC,
) -> Result<(ID3D11Texture2D, IDXGIKeyedMutex, Arc<SlotHandle>)> {
    let mut tex = None;
    // SAFETY: a `?`-checked `CreateTexture2D` on the live device, over the caller's
    // fully-initialized descriptor and a live `Option` out-param.
    unsafe { device.CreateTexture2D(desc, None, Some(&mut tex)) }
        .ok()
        .context("create shared hand-off texture")?;
    let tex: ID3D11Texture2D = tex.expect("CreateTexture2D succeeded");
    let mutex: IDXGIKeyedMutex = tex.cast().context("shared texture lacks IDXGIKeyedMutex")?;
    let resource: IDXGIResource1 = tex.cast().context("shared texture lacks IDXGIResource1")?;
    // SAFETY: the shared-handle creation runs on the live texture just created; the returned
    // NT handle is owned by the `SlotHandle` built below, which closes it in `Drop`.
    let handle = unsafe {
        resource.CreateSharedHandle(
            None,
            DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE as u32,
            None,
        )
    }
    .context("CreateSharedHandle")?;
    Ok((tex, mutex, Arc::new(SlotHandle(handle))))
}

/// `AcquireSync` reports a timeout (`WAIT_TIMEOUT`) and an abandoned mutex as success
/// codes; only `S_OK` means this side owns the slot.
fn keyed_acquired(hr: windows::core::HRESULT) -> Result<()> {
    if hr.0 == 0 {
        Ok(())
    } else {
        Err(anyhow!(
            "keyed-mutex acquire (decode side) returned {:#010x}",
            hr.0
        ))
    }
}

/// A planar slot: the texture the copy writes, its keyed mutex, its NT handle.
struct PlanarSlot {
    tex: ID3D11Texture2D,
    mutex: IDXGIKeyedMutex,
    handle: Arc<SlotHandle>,
}

/// Two-plane slots in the decode pool's own format, filled by a copy. No video processor:
/// the presenter's CSC pass converts, PQ tone-map included.
struct PlanarRing {
    slots: Vec<PlanarSlot>,
    width: u32,
    height: u32,
    format: SlotFormat,
    next: usize,
    generation: u32,
}

impl PlanarRing {
    fn build(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: SlotFormat,
        generation: u32,
    ) -> Result<PlanarRing> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: if format == SlotFormat::P010 {
                DXGI_FORMAT_P010
            } else {
                DXGI_FORMAT_NV12
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX)
                as u32,
        };
        let slots = (0..RING_SLOTS)
            .map(|_| {
                shared_texture(device, &desc).map(|(tex, mutex, handle)| PlanarSlot {
                    tex,
                    mutex,
                    handle,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        tracing::info!(
            width,
            height,
            slots = RING_SLOTS,
            generation,
            ?format,
            "D3D11 shared hand-off ring built (copy → planar)"
        );
        Ok(PlanarRing {
            slots,
            width,
            height,
            format,
            next: 0,
            generation,
        })
    }
}

/// Video processor plus shareable slots, both sized to the stream. A mid-stream
/// `Reconfigure` rebuilds the whole bundle. Stream state that is fixed per ring (source
/// rect, output colour space) is set at build; the input colour space is set on change.
struct SharedRing {
    slots: Vec<Slot>,
    vp: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    width: u32,
    height: u32,
    next: usize,
    generation: u32,
    /// `true` = RGB10A2 PQ BT.2020 (colorspace only, both sides G2084). `false` = BGRA8 sRGB.
    pq_out: bool,
    /// One input view per decode-pool slice, built when a pool first meets this ring and
    /// again when the session swaps pools (`in_views_pool` is the texture's identity).
    in_views: Vec<ID3D11VideoProcessorInputView>,
    in_views_pool: usize,
    /// Last input colour space set on stream 0; the host flips PQ in-band.
    in_cs: Option<DXGI_COLOR_SPACE_TYPE>,
}

impl SharedRing {
    fn build(
        device: &ID3D11Device,
        video_device: &ID3D11VideoDevice,
        video_context1: &ID3D11VideoContext1,
        width: u32,
        height: u32,
        generation: u32,
        pq_out: bool,
    ) -> Result<SharedRing> {
        // 1:1, no scaling — Vulkan scales at composite. Frame rates are advisory.
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: width,
            InputHeight: height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };
        // SAFETY: COM calls on the live video device, with a borrowed local descriptor and a
        // checked out-param.
        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content) }
            .context("CreateVideoProcessorEnumerator")?;
        // SAFETY: same live device, borrowed enumerator from the line above.
        let vp = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;
        // DXVA-aligned surfaces are taller than the frame (HEVC/AV1 round to 128); without a
        // source rect the padding blits too (uninit NV12 shows green, picture squashed). The
        // output space follows the slot flavour. Both are fixed for the ring's life. Driver
        // auto-processing (on by default: denoise, edge, colour enhancement) is forced off.
        let source = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        // SAFETY: COM calls on the live video context over the processor just created and a
        // borrowed local rect.
        unsafe {
            video_context1.VideoProcessorSetStreamSourceRect(&vp, 0, true, Some(&source));
            video_context1.VideoProcessorSetStreamAutoProcessingMode(&vp, 0, false);
            video_context1.VideoProcessorSetOutputColorSpace1(
                &vp,
                if pq_out {
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                } else {
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
                },
            );
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            // Single-plane RGB: NV12 D3D11→Vulkan import TDRs on NVIDIA. RGB10A2 for
            // HDR pass-through, BGRA8 otherwise.
            Format: if pq_out {
                DXGI_FORMAT_R10G10B10A2_UNORM
            } else {
                DXGI_FORMAT_B8G8R8A8_UNORM
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX)
                as u32,
        };
        let mut slots = Vec::with_capacity(RING_SLOTS);
        for _ in 0..RING_SLOTS {
            let (tex, mutex, handle) = shared_texture(device, &desc)?;
            let ov_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                // Anonymous.Texture2D.MipSlice = 0 — the zeroed default.
                ..Default::default()
            };
            let mut out_view = None;
            // SAFETY: COM calls on the live video device with borrowed local descriptors and a
            // checked out-param.
            unsafe {
                video_device.CreateVideoProcessorOutputView(
                    &tex,
                    &enumerator,
                    &ov_desc,
                    Some(&mut out_view),
                )
            }
            .ok()
            .context("CreateVideoProcessorOutputView")?;
            let out_view = out_view.expect("output view created");
            slots.push(Slot {
                _tex: tex,
                mutex,
                handle,
                out_view,
            });
        }
        tracing::info!(
            width,
            height,
            slots = RING_SLOTS,
            generation,
            hdr = pq_out,
            "D3D11 shared hand-off ring built (VideoProcessor → RGB)"
        );
        Ok(SharedRing {
            slots,
            vp,
            enumerator,
            width,
            height,
            next: 0,
            generation,
            pq_out,
            in_views: Vec::new(),
            in_views_pool: 0,
            in_cs: None,
        })
    }

    /// Input views over every slice of `pool`, once per pool. `tex_*` in the log is the
    /// DXVA-aligned surface; the gap to the frame is the padding the source rect excludes.
    fn bind_pool(
        &mut self,
        video_device: &ID3D11VideoDevice,
        pool: &ID3D11Texture2D,
        decoder: &str,
    ) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a COM call on the caller's live texture filling a local descriptor.
        unsafe { pool.GetDesc(&mut desc) };
        let mut views = Vec::with_capacity(desc.ArraySize as usize);
        for slice in 0..desc.ArraySize {
            let mut iv_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0, // surface format speaks for itself
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                // Anonymous.Texture2D zeroed (MipSlice 0); ArraySlice set below.
                ..Default::default()
            };
            iv_desc.Anonymous.Texture2D.ArraySlice = slice;
            let mut view = None;
            // SAFETY: a COM call on the live video device over the caller's texture, this
            // ring's enumerator and a borrowed local descriptor; the out-param is checked.
            unsafe {
                video_device.CreateVideoProcessorInputView(
                    pool,
                    &self.enumerator,
                    &iv_desc,
                    Some(&mut view),
                )
            }
            .ok()
            .context("CreateVideoProcessorInputView")?;
            views.push(view.expect("input view created"));
        }
        tracing::info!(
            width = self.width,
            height = self.height,
            tex_w = desc.Width,
            tex_h = desc.Height,
            slices = desc.ArraySize,
            pq = self.pq_out,
            decoder,
            "D3D11VA decode pool bound to the hand-off ring"
        );
        self.in_views = views;
        self.in_views_pool = pool.as_raw() as usize;
        Ok(())
    }
}

/// GPU time of one `VideoProcessorBlt`: a disjoint/timestamp query triple per frame. Four
/// triples cycle; the one issued four frames ago is read without flushing before it is
/// re-armed, so a still-pending read costs a sample, never a stall.
struct BltTimer {
    sets: Vec<QueryTriple>,
    next: usize,
}

struct QueryTriple {
    disjoint: ID3D11Query,
    t0: ID3D11Query,
    t1: ID3D11Query,
    armed: bool,
}

impl BltTimer {
    /// `None` when the device refuses timestamp queries; the window then has no Blt figure.
    fn new(device: &ID3D11Device) -> Option<BltTimer> {
        let make = |query| {
            let mut q = None;
            // SAFETY: a COM call on the live device over a borrowed local descriptor; the
            // out-param is checked before use.
            unsafe {
                device.CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: query,
                        MiscFlags: 0,
                    },
                    Some(&mut q),
                )
            }
            .ok()
            .ok()?;
            q
        };
        let mut sets = Vec::with_capacity(4);
        for _ in 0..4 {
            sets.push(QueryTriple {
                disjoint: make(D3D11_QUERY_TIMESTAMP_DISJOINT)?,
                t0: make(D3D11_QUERY_TIMESTAMP)?,
                t1: make(D3D11_QUERY_TIMESTAMP)?,
                armed: false,
            });
        }
        Some(BltTimer { sets, next: 0 })
    }

    /// Read the oldest triple, then arm it around the Blt the caller issues next.
    /// Returns that triple's Blt time in microseconds when the GPU has it ready.
    fn begin(&mut self, context: &ID3D11DeviceContext) -> Option<u32> {
        let idx = self.next;
        self.next = (self.next + 1) % self.sets.len();
        let set = &mut self.sets[idx];
        let sample = if set.armed {
            read_blt_us(context, set)
        } else {
            None
        };
        set.armed = true;
        // SAFETY: COM calls on the live context over queries this timer owns.
        unsafe {
            context.Begin(&set.disjoint);
            context.End(&set.t0);
        }
        sample
    }

    /// Close the triple [`Self::begin`] armed.
    fn end(&self, context: &ID3D11DeviceContext) {
        let idx = (self.next + self.sets.len() - 1) % self.sets.len();
        let set = &self.sets[idx];
        // SAFETY: COM calls on the live context over queries this timer owns.
        unsafe {
            context.End(&set.t1);
            context.End(&set.disjoint);
        }
    }
}

/// `None` while any of the three results is pending, or when the clock was disjoint.
fn read_blt_us(context: &ID3D11DeviceContext, set: &QueryTriple) -> Option<u32> {
    let mut disjoint = D3D11_QUERY_DATA_TIMESTAMP_DISJOINT::default();
    let mut t0 = 0u64;
    let mut t1 = 0u64;
    let flags = D3D11_ASYNC_GETDATA_DONOTFLUSH as u32;
    // SAFETY: COM calls on the live context over the timer's queries; each out-pointer is a
    // local of exactly the size passed. `S_OK` (0) means the value was written.
    unsafe {
        let ready = |hr: windows::core::HRESULT| hr.0 == 0;
        if !ready(context.GetData(
            &set.disjoint,
            Some(&mut disjoint as *mut _ as *mut c_void),
            size_of::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>() as u32,
            flags,
        )) || !ready(context.GetData(
            &set.t0,
            Some(&mut t0 as *mut _ as *mut c_void),
            size_of::<u64>() as u32,
            flags,
        )) || !ready(context.GetData(
            &set.t1,
            Some(&mut t1 as *mut _ as *mut c_void),
            size_of::<u64>() as u32,
            flags,
        )) {
            return None;
        }
    }
    if disjoint.Disjoint.as_bool() || disjoint.Frequency == 0 || t1 < t0 {
        return None;
    }
    u32::try_from((t1 - t0) * 1_000_000 / disjoint.Frequency).ok()
}

/// Can this device create a shared fence and hand out its NT handle? A shared fence would
/// replace the keyed mutex; the Vulkan half of the answer is the presenter's import line.
fn shared_fence_supported(device: &ID3D11Device) -> Result<()> {
    let device5: ID3D11Device5 = device
        .cast()
        .context("device lacks ID3D11Device5 (pre-1703 Windows?)")?;
    let mut fence: Option<ID3D11Fence> = None;
    // SAFETY: a COM call on the live device writing a local `Option` out-param, checked below.
    unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence) }
        .context("CreateFence(SHARED)")?;
    let fence = fence.ok_or_else(|| anyhow!("CreateFence returned no fence"))?;
    // SAFETY: a COM call on the fence just created; the returned NT handle is closed below.
    let handle = unsafe { fence.CreateSharedHandle(None, GENERIC_ALL as u32, None) }
        .context("fence CreateSharedHandle")?;
    // SAFETY: `handle` was returned above and is closed exactly once here.
    unsafe {
        let _ = windows::Win32::handleapi::CloseHandle(handle);
    }
    Ok(())
}

/// One-second window of hand-off costs. `info` under `PUNKTFUNK_PRESENT_DEBUG=1` (the
/// presenter's window switch) or when a frame stalled; `debug` otherwise.
struct HandoffWindow {
    start: Instant,
    frames: u32,
    /// Keyed-mutex acquires that waited on the presenter (≥ [`ACQUIRE_STALL`]), and the
    /// summed wait of every acquire.
    acquire_stalls: u32,
    acquire_wait: Duration,
    /// `DecoderBeginFrame` busy retries and the time they cost.
    begin_retries: u32,
    begin_wait: Duration,
    blt_us: Vec<u32>,
    debug: bool,
}

impl HandoffWindow {
    fn new() -> HandoffWindow {
        HandoffWindow {
            start: Instant::now(),
            frames: 0,
            acquire_stalls: 0,
            acquire_wait: Duration::ZERO,
            begin_retries: 0,
            begin_wait: Duration::ZERO,
            blt_us: Vec::with_capacity(256),
            debug: std::env::var_os("PUNKTFUNK_PRESENT_DEBUG").is_some(),
        }
    }

    fn note_frame(&mut self, acquire_wait: Duration, blt_us: Option<u32>) {
        self.frames += 1;
        self.acquire_wait += acquire_wait;
        if acquire_wait >= ACQUIRE_STALL {
            self.acquire_stalls += 1;
        }
        if let Some(us) = blt_us {
            self.blt_us.push(us);
        }
    }

    fn flush_if_due(&mut self) {
        if self.frames == 0 || self.start.elapsed() < Duration::from_secs(1) {
            return;
        }
        let blt = punktfunk_core::hud::Summary::of(&mut self.blt_us);
        let (blt_p50_us, blt_max_us) = (blt.p50_us, blt.max_us);
        let stalled = self.begin_retries > 0 || self.acquire_stalls > 0;
        // Both arms carry the same fields: `tracing` levels are not runtime values.
        if self.debug || stalled {
            tracing::info!(
                frames = self.frames,
                blt_p50_us,
                blt_max_us,
                blt_samples = self.blt_us.len(),
                acquire_stalls = self.acquire_stalls,
                acquire_wait_us = self.acquire_wait.as_micros() as u64,
                begin_retries = self.begin_retries,
                begin_wait_us = self.begin_wait.as_micros() as u64,
                "D3D11VA hand-off window"
            );
        } else {
            tracing::debug!(
                frames = self.frames,
                blt_p50_us,
                blt_max_us,
                blt_samples = self.blt_us.len(),
                acquire_stalls = self.acquire_stalls,
                acquire_wait_us = self.acquire_wait.as_micros() as u64,
                begin_retries = self.begin_retries,
                begin_wait_us = self.begin_wait.as_micros() as u64,
                "D3D11VA hand-off window"
            );
        }
        self.start = Instant::now();
        self.frames = 0;
        self.acquire_stalls = 0;
        self.acquire_wait = Duration::ZERO;
        self.begin_retries = 0;
        self.begin_wait = Duration::ZERO;
        self.blt_us.clear();
    }
}

/// One decoded picture for [`HandoffRing::present`]. Named fields so a swapped
/// `width`/`height` or `array_slice` is a build error, not a wrong picture.
pub(crate) struct HandoffSource<'a> {
    /// Decode-pool texture array from `video_d3d11_native`.
    pub texture: &'a ID3D11Texture2D,
    /// Slice in that array: the decoder's DPB slot, which is the DXVA surface index.
    pub array_slice: u32,
    /// Frame size, not the DXVA-aligned surface. The blit source rect uses this so
    /// padding rows stay out of the picture.
    pub width: u32,
    pub height: u32,
    /// Per-frame colour signalling; never latched — the host flips PQ in-band with a new SPS.
    pub color: ColorDesc,
    /// Intra (IDR/I) — the pump's post-loss re-anchor.
    pub keyframe: bool,
    /// Whole prediction chain was fully available — see [`D3d11Frame::references_clean`].
    pub references_clean: bool,
    /// Decoder name for the pool-bound log line, so a demotion logs the new rung.
    pub decoder: &'a str,
}

/// The shareable hand-off rings (video-processor RGB, or planar copies) and the D3D11
/// objects they live on. Turns a decoded NV12/P010 surface into a [`D3d11Frame`] the
/// presenter can import.
pub(crate) struct HandoffRing {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    video_device: ID3D11VideoDevice,
    /// `1` for the DXGI colour-space setters (Win10 1703+). Init fails to software without it.
    video_context1: ID3D11VideoContext1,
    ring: Option<SharedRing>,
    /// Presenter can import RGB10A2 and has an HDR10 swapchain
    /// ([`crate::video::VulkanDecodeDevice::d3d11_hdr10`]). PQ then uses the pass-through ring.
    hdr10_out: bool,
    /// Planar copies for NV12 / P010 pools the presenter imports ([`Self::set_planar`]);
    /// cleared for a format whose planar hand-off failed.
    planar_nv12: bool,
    planar_p010: bool,
    planar_ring: Option<PlanarRing>,
    /// Next ring generation, shared by both rings so the presenter's import cache never
    /// sees one generation twice.
    generation: u32,
    /// Blt GPU timing; `None` when the device has no timestamp queries.
    timer: Option<BltTimer>,
    window: HandoffWindow,
}

impl HandoffRing {
    /// Bind the video interfaces now. Missing them must fail the rung before the opening IDR.
    pub(crate) fn new(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        hdr10_out: bool,
    ) -> Result<HandoffRing> {
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("device lacks ID3D11VideoDevice (created without VIDEO_SUPPORT)")?;
        let video_context1: ID3D11VideoContext1 = context
            .cast()
            .context("context lacks ID3D11VideoContext1 (pre-1703 Windows?)")?;
        match shared_fence_supported(&device) {
            Ok(()) => tracing::info!("D3D11 shared fence supported"),
            Err(e) => tracing::info!(error = %format!("{e:#}"), "D3D11 shared fence unsupported"),
        }
        let timer = BltTimer::new(&device);
        Ok(HandoffRing {
            device,
            context,
            video_device,
            video_context1,
            ring: None,
            hdr10_out,
            planar_nv12: false,
            planar_p010: false,
            planar_ring: None,
            generation: 0,
            timer,
            window: HandoffWindow::new(),
        })
    }

    /// Copy NV12 / P010 pools into planar slots from the next frame on: only for formats
    /// the presenter imports ([`crate::video::VulkanDecodeDevice::d3d11_nv12`]).
    pub(crate) fn set_planar(&mut self, nv12: bool, p010: bool) {
        self.planar_nv12 = nv12;
        self.planar_p010 = p010;
    }

    /// The planar slot format for `pool`, when planar is on for its format.
    fn planar_format(&self, pool: &ID3D11Texture2D) -> Option<SlotFormat> {
        if !self.planar_nv12 && !self.planar_p010 {
            return None;
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: a COM call on the caller's live texture filling a local descriptor.
        unsafe { pool.GetDesc(&mut desc) };
        match desc.Format {
            f if f == DXGI_FORMAT_NV12 && self.planar_nv12 => Some(SlotFormat::Nv12),
            f if f == DXGI_FORMAT_P010 && self.planar_p010 => Some(SlotFormat::P010),
            _ => None,
        }
    }

    /// Copy one decoded slice into the next planar slot under its keyed mutex. The
    /// presenter's CSC pass converts it, so the frame carries the stream's own colour.
    fn present_planar(
        &mut self,
        source: &HandoffSource<'_>,
        format: SlotFormat,
    ) -> Result<D3d11Frame> {
        // Two-plane 4:2:0 copies need even extents; the pool is aligned larger, so rounding
        // up stays inside the decoded surface.
        let width = source.width.next_multiple_of(2);
        let height = source.height.next_multiple_of(2);
        let rebuild = self
            .planar_ring
            .as_ref()
            .is_none_or(|r| r.width != width || r.height != height || r.format != format);
        if rebuild {
            let generation = self.generation;
            self.generation += 1;
            self.planar_ring = Some(PlanarRing::build(
                &self.device,
                width,
                height,
                format,
                generation,
            )?);
            self.ring = None;
        }
        let context = self.context.clone();
        let ring = self.planar_ring.as_mut().expect("ring built above");
        let slot_idx = ring.next;
        ring.next = (ring.next + 1) % ring.slots.len();
        let slot = &ring.slots[slot_idx];
        let generation = ring.generation;
        let src_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: width,
            bottom: height,
            back: 1,
        };
        let started = Instant::now();
        // SAFETY: a COM call on the slot's live keyed mutex.
        let acquired = unsafe { slot.mutex.AcquireSync(0, ACQUIRE_TIMEOUT_MS) };
        let acquire_wait = started.elapsed();
        keyed_acquired(acquired)?;
        let sample = self.timer.as_mut().and_then(|t| t.begin(&context));
        // SAFETY: COM calls on the live context. The slot texture and the caller's decode
        // pool are live, `array_slice` is one of the pool's slices, and `src_box` lies inside
        // both (the slot is exactly that size; the pool is aligned larger).
        unsafe {
            context.CopySubresourceRegion(
                &slot.tex,
                0,
                0,
                0,
                0,
                source.texture,
                source.array_slice,
                Some(&src_box),
            );
        }
        if let Some(t) = &self.timer {
            t.end(&context);
        }
        // SAFETY: releases the acquire above on the same live keyed mutex.
        unsafe { slot.mutex.ReleaseSync(0) }
            .ok()
            .context("keyed-mutex release")?;
        // SAFETY: a COM call on the live context; the presenter's acquire waits on this copy.
        unsafe { context.Flush() };
        let frame = D3d11Frame {
            width,
            height,
            color: source.color,
            format,
            keyframe: source.keyframe,
            references_clean: source.references_clean,
            handle: slot.handle.clone(),
            generation,
        };
        self.window.note_frame(acquire_wait, sample);
        self.window.flush_if_due();
        Ok(frame)
    }

    /// Same `ID3D11VideoDevice` the native rung enumerates decode profiles on.
    pub(crate) fn video_device(&self) -> &ID3D11VideoDevice {
        &self.video_device
    }

    /// One `DecoderBeginFrame`'s busy retries, for the window log.
    pub(crate) fn note_begin_frame(&mut self, retries: u32, waited: Duration) {
        self.window.begin_retries += retries;
        self.window.begin_wait += waited;
    }

    /// Hand one decoded surface to the next ring slot under its keyed mutex: a planar copy
    /// when the presenter imports the pool's format, a video-processor Blt to RGB otherwise.
    /// A failed planar hand-off turns planar off for that format and takes the RGB path.
    /// The acquire also back-pressures if the presenter is still reading this slot.
    pub(crate) fn present(&mut self, source: HandoffSource<'_>) -> Result<D3d11Frame> {
        if let Some(format) = self.planar_format(source.texture) {
            match self.present_planar(&source, format) {
                Ok(frame) => return Ok(frame),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), ?format,
                        "D3D11 planar hand-off failed — using the video processor for this format");
                    match format {
                        SlotFormat::P010 => self.planar_p010 = false,
                        _ => self.planar_nv12 = false,
                    }
                    self.planar_ring = None;
                }
            }
        }
        let HandoffSource {
            texture: src,
            array_slice,
            width,
            height,
            color,
            keyframe,
            references_clean,
            decoder,
        } = source;
        // AddRef'd locals so the mutable `ring` borrow below doesn't lock all of `self`.
        let video_device = self.video_device.clone();
        let video_context1 = self.video_context1.clone();
        let context = self.context.clone();
        // Rebuild on first use, size change, or SDR↔HDR flavour change (PQ flips in-band
        // and swaps the slot format). Bit depth alone does not: SDR 10-bit and 8-bit
        // share the same output flavour.
        let pq_out = self.hdr10_out && color.is_pq();
        let rebuild = self
            .ring
            .as_ref()
            .is_none_or(|r| r.width != width || r.height != height || r.pq_out != pq_out);
        if rebuild {
            let generation = self.generation;
            self.generation += 1;
            self.ring = Some(SharedRing::build(
                &self.device,
                &video_device,
                &video_context1,
                width,
                height,
                generation,
                pq_out,
            )?);
            self.planar_ring = None;
        }
        let ring = self.ring.as_mut().expect("ring built above");
        if ring.in_views_pool != src.as_raw() as usize {
            ring.bind_pool(&video_device, src, decoder)?;
        }
        let in_view = ring
            .in_views
            .get(array_slice as usize)
            .ok_or_else(|| {
                anyhow!(
                    "decode slice {array_slice} is outside the {}-slice pool",
                    ring.in_views.len()
                )
            })?
            .clone();
        // Per-frame CICP → DXGI (host flips PQ in-band). Matrix 5/6 is BT.601; mapping
        // it to P709 is a hue error. DXGI has no full-range G2084 YCbCr enum, so PQ is
        // studio regardless of range.
        let in_cs = match (color.transfer, color.matrix, color.full_range) {
            (16, _, _) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020,
            (_, 9 | 10, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P2020,
            (_, 9 | 10, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P2020,
            (_, 5 | 6, false) => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601,
            (_, 5 | 6, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P601,
            (_, _, true) => DXGI_COLOR_SPACE_YCBCR_FULL_G22_LEFT_P709,
            _ => DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709,
        };
        if ring.in_cs != Some(in_cs) {
            // SAFETY: a COM call on the live video context over this ring's processor.
            unsafe { video_context1.VideoProcessorSetStreamColorSpace1(&ring.vp, 0, in_cs) };
            ring.in_cs = Some(in_cs);
        }
        let slot_idx = ring.next;
        ring.next = (ring.next + 1) % ring.slots.len();
        let slot = &ring.slots[slot_idx];
        let generation = ring.generation;

        // SAFETY: every call below is a COM call on a live interface — the video context
        // AddRef'd above, the ring's processor and views built by `SharedRing::build` and
        // `bind_pool`, and the timer's queries. The `ManuallyDrop` refs the stream struct
        // carries are balanced explicitly below.
        let (acquire_wait, blt_sample) = unsafe {
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(in_view)),
                ppFutureSurfaces: ptr::null_mut(),
                ppPastSurfacesRight: ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: ptr::null_mut(),
            };
            let mut streams = [stream];
            let acquire_started = Instant::now();
            let acquired = slot.mutex.AcquireSync(0, ACQUIRE_TIMEOUT_MS);
            let acquire_wait = acquire_started.elapsed();
            // Balance the ManuallyDrop refs BEFORE any error exit.
            let blt = keyed_acquired(acquired).and_then(|()| {
                let sample = self.timer.as_mut().and_then(|t| t.begin(&context));
                let blt = video_context1.VideoProcessorBlt(&ring.vp, &slot.out_view, 0, &streams);
                if let Some(t) = &self.timer {
                    t.end(&context);
                }
                blt.ok().map(|()| sample).context("VideoProcessorBlt")
            });
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurfaceRight);
            let release = slot.mutex.ReleaseSync(0);
            let sample = blt?;
            release.ok().context("keyed-mutex release")?;
            // Flush now: the presenter's GPU acquire waits on this blit; an unflushed
            // deferred batch adds a driver-decided delay.
            context.Flush();
            (acquire_wait, sample)
        };
        self.window.note_frame(acquire_wait, blt_sample);
        self.window.flush_if_due();
        Ok(D3d11Frame {
            width,
            height,
            // Slot contents after the blit, not the source signalling.
            color: if pq_out {
                ColorDesc {
                    primaries: 9,
                    transfer: 16, // PQ / SMPTE ST.2084
                    matrix: 0,    // identity — RGB
                    full_range: true,
                }
            } else {
                ColorDesc {
                    primaries: 1,
                    transfer: 13, // sRGB (H.273)
                    matrix: 0,    // identity — RGB
                    full_range: true,
                }
            },
            format: if pq_out {
                SlotFormat::Rgb10a2
            } else {
                SlotFormat::Bgra8
            },
            keyframe,
            references_clean,
            handle: slot.handle.clone(),
            generation,
        })
    }
}

/// User-mode driver version of the adapter behind `luid`, as Device Manager shows it
/// ([`crate::video::umd_version_parts`]). `None` when no adapter matches or DXGI refuses.
pub fn adapter_driver_version(luid: [u8; 8]) -> Option<[u16; 4]> {
    use windows::Win32::dxgi::IDXGIDevice;
    // SAFETY: plain DXGI factory creation; the returned interface is owned by this scope.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    for i in 0.. {
        // SAFETY: read-only enumeration on the live factory; the adapter is owned here.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: `DXGI_ADAPTER_DESC1` is plain-old-data, so all-zeroes is a valid value.
        let mut desc: DXGI_ADAPTER_DESC1 = unsafe { std::mem::zeroed() };
        // SAFETY: fills the zeroed local through the out-param; checked before it is read.
        if unsafe { adapter.GetDesc1(&mut desc) }.is_err() {
            continue;
        }
        let mut have = [0u8; 8];
        have[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
        have[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
        if have != luid {
            continue;
        }
        // SAFETY: a query on the live adapter; the IID is a static the callee only reads.
        let raw = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }.ok()?;
        return Some(crate::video::umd_version_parts(raw));
    }
    None
}

/// This desktop's HDR volume (`IDXGIOutput6::GetDesc1`) for Hello `display_hdr`, so
/// the host EDID matches this panel. `pos` selects the output containing that point
/// (`--window-pos`); no `pos` or no match uses the output at the desktop origin.
/// `None` when advanced color is off — claiming HDR for an SDR desktop would steer
/// host tone-mapping wrong. `PUNKTFUNK_CLIENT_PEAK_NITS` still overrides; see
/// `punktfunk_core::client::display_hdr_env_override`.
pub fn display_hdr_volume(pos: Option<(i32, i32)>) -> Option<punktfunk_core::quic::HdrMeta> {
    use windows::Win32::dxgi::{IDXGIOutput6, DXGI_OUTPUT_DESC1};
    // SAFETY: plain DXGI factory creation — no arguments to get wrong; the returned
    // interface is owned by this scope and dropped with it.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let mut fallback: Option<DXGI_OUTPUT_DESC1> = None;
    for a in 0.. {
        // SAFETY: read-only enumeration on the live factory; the returned adapter is
        // owned by this scope.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0.. {
            // Out-pointer convention in this windows-rs rev (no retval annotation).
            let mut output: Option<windows::Win32::dxgi::IDXGIOutput> = None;
            // SAFETY: read-only enumeration on the live adapter, writing a local
            // out-pointer that outlives the call.
            if unsafe { adapter.EnumOutputs(o, &mut output) }.ok().is_err() {
                break;
            }
            let Some(output) = output else {
                break;
            };
            let Ok(out6) = output.cast::<IDXGIOutput6>() else {
                continue; // pre-1809 DXGI — no advanced-color facts to read
            };
            let mut desc = DXGI_OUTPUT_DESC1::default();
            // SAFETY: fills a local, correctly-sized DXGI_OUTPUT_DESC1 that outlives
            // the call; the interface is live (owned just above).
            if unsafe { out6.GetDesc1(&mut desc) }.ok().is_err() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let contains =
                |x: i32, y: i32| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
            if let Some((x, y)) = pos {
                if contains(x, y) {
                    return hdr_meta_from_output(&desc);
                }
            }
            if fallback.is_none() || contains(0, 0) {
                fallback = Some(desc);
            }
        }
    }
    hdr_meta_from_output(&fallback?)
}

/// The ST.2086 shape of one output's colour facts; `None` for an SDR colorspace.
fn hdr_meta_from_output(
    d: &windows::Win32::dxgi::DXGI_OUTPUT_DESC1,
) -> Option<punktfunk_core::quic::HdrMeta> {
    use windows::Win32::dxgi::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    if d.ColorSpace != DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020 {
        return None;
    }
    // Chromaticity → 1/50000 units; luminance → 0.0001 cd/m² units (the HdrMeta contract).
    let c = |v: [f32; 2]| {
        [
            (v[0] * 50_000.0).round().clamp(0.0, 65_535.0) as u16,
            (v[1] * 50_000.0).round().clamp(0.0, 65_535.0) as u16,
        ]
    };
    Some(punktfunk_core::quic::HdrMeta {
        // ST.2086 primary order is G, B, R (see the HdrMeta docs); DXGI reports R/G/B.
        display_primaries: [c(d.GreenPrimary), c(d.BluePrimary), c(d.RedPrimary)],
        white_point: c(d.WhitePoint),
        max_display_mastering_luminance: (f64::from(d.MaxLuminance) * 10_000.0) as u32,
        min_display_mastering_luminance: (f64::from(d.MinLuminance) * 10_000.0) as u32,
        max_cll: d.MaxLuminance.round().clamp(0.0, 65_535.0) as u16,
        max_fall: d.MaxFullFrameLuminance.round().clamp(0.0, 65_535.0) as u16,
    })
}
