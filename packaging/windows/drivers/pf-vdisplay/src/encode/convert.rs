//! The `windows` 0.58 → 0.62 bridge and the backend input [`Targets`]: one set of textures per
//! pool slot, in whatever format the opened backend reads, plus the converter that fills them
//! from a BGRA or FP16 source. [`Targets::pass`] is the fused pass the drain worker runs in the
//! acquire window; [`Targets::frame`] wraps a filled slot as the `CapturedFrame` `submit` takes.
//!
//! A blended pointer costs the window nothing: the pass then only copies the source into a
//! slot-sized RGB scratch, and `frame` (the encode thread) draws the cursor quad and runs the
//! converter from there. NVENC's BGRA slot is its own scratch, so that kind copies once either
//! way; the converter kinds pay one extra full-frame copy while the client draws no pointer.
//!
//! Every COM object the backends see is a [`bridge`]d `QueryInterface` of the driver's own
//! 0.58 object, so the two crates never wrap each other's pointer.

use std::sync::{Arc, Mutex};

use pf_driver_proto::encode::EncodeInput;
use pf_encode_win::convert::{
    BgraToYuvPlanes, CursorBlendPass, HdrP010Converter, HdrRgb10Converter, VideoConverter,
};
use pf_frame::dxgi::{D3d11Frame, PyroFrameShare};
use pf_frame::{CapturedFrame, CursorOverlay, FramePayload, PixelFormat, Provenance};
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::core::Interface;
use windows62::Win32::Foundation::{CloseHandle, HANDLE};
use windows62::Win32::Graphics::Direct3D11 as d3d;
use windows62::Win32::Graphics::Dxgi::Common as dxgi;
use windows62::core::{Interface as _, PCWSTR};

use super::content_probe::ContentProbe;
use crate::cursor_cell::CursorImage;
use crate::direct_3d_device::Direct3DDevice;
use crate::registry::lock;

/// A driver-domain failure: a small code for the reply and a stage tag; the log has the rest.
pub type Fail = (i32, &'static str);

/// Every monitor on an adapter shares the pooled device's immediate context, and
/// `SetMultithreadProtected` serialises single calls, not the draw sequences a pass issues.
/// Held for each pass, always after the pool's state lock.
static CTX: Mutex<()> = Mutex::new(());

type Tex = d3d::ID3D11Texture2D;
type Rtv = d3d::ID3D11RenderTargetView;
type Srv = d3d::ID3D11ShaderResourceView;

/// The pooled device's adapter, as the backends want it named: the LUID for NVENC/AMF/QSV,
/// the PCI ids for PyroWave (LUIDs are invalid in session 0). Without PyroWave (ARM64) the
/// PCI ids have no reader outside the probe build.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub struct AdapterId {
    pub luid: LUID,
    pub vendor_id: u32,
    pub device_id: u32,
}

impl AdapterId {
    /// Read the adapter behind `device` once.
    pub fn of(device: &Direct3DDevice) -> Option<Self> {
        // SAFETY: plain queries on the live pooled device; each result is checked before use.
        let desc = unsafe {
            device
                .device
                .cast::<IDXGIDevice>()
                .ok()?
                .GetAdapter()
                .ok()?
                .GetDesc()
                .ok()?
        };
        Some(Self {
            luid: desc.AdapterLuid,
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
        })
    }

    /// The LUID in the backends' `windows` version.
    pub fn luid62(&self) -> windows62::Win32::Foundation::LUID {
        windows62::Win32::Foundation::LUID {
            LowPart: self.luid.LowPart,
            HighPart: self.luid.HighPart,
        }
    }
}

/// A `windows` 0.62 view of a 0.58 COM object: a real `QueryInterface`, so the result owns
/// its own reference and neither crate ever wraps the other's pointer.
pub fn bridge<T: windows62::core::Interface>(obj: &impl Interface) -> Result<T, Fail> {
    let raw = obj.as_raw();
    // SAFETY: `raw` is the live COM pointer `obj` owns for the duration of this call;
    // `from_raw_borrowed` takes no reference of its own and `cast` AddRefs through QI.
    let unk = unsafe { windows62::core::IUnknown::from_raw_borrowed(&raw) };
    unk.ok_or((-8, "bridge"))?.cast::<T>().map_err(|e| {
        dbglog!("[pf-vd] encode: 0.58→0.62 bridge QI failed: {e:?}");
        (-8, "bridge")
    })
}

/// One default-usage texture on the bridged device with the given bind and misc flags.
pub fn make_tex62(
    dev: &d3d::ID3D11Device,
    (w, h): (u32, u32),
    format: dxgi::DXGI_FORMAT,
    bind: u32,
    misc: u32,
) -> Result<Tex, Fail> {
    let desc = d3d::D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: dxgi::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: d3d::D3D11_USAGE_DEFAULT,
        BindFlags: bind,
        CPUAccessFlags: 0,
        MiscFlags: misc,
    };
    let mut t: Option<Tex> = None;
    // SAFETY: `desc` is a fully-initialized local; `t` a valid out-param checked below.
    let hr = unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t)) };
    match (hr, t) {
        (Ok(()), Some(t)) => Ok(t),
        (r, _) => {
            dbglog!("[pf-vd] encode: CreateTexture2D({format:?}) failed: {r:?}");
            Err((-2, "pool"))
        }
    }
}

fn rtv(dev: &d3d::ID3D11Device, t: &Tex) -> Result<Rtv, Fail> {
    let mut v = None;
    // SAFETY: `t` is a live render-target texture on `dev`; `v` a valid out-param.
    unsafe { dev.CreateRenderTargetView(t, None, Some(&mut v)) }.map_err(|e| {
        dbglog!("[pf-vd] encode: CreateRenderTargetView failed: {e:?}");
        (-2, "rtv")
    })?;
    v.ok_or((-2, "rtv"))
}

fn srv(dev: &d3d::ID3D11Device, t: &Tex) -> Result<Srv, Fail> {
    let mut v = None;
    // SAFETY: `t` is a live shader-resource texture on `dev`; `v` a valid out-param.
    unsafe { dev.CreateShaderResourceView(t, None, Some(&mut v)) }.map_err(|e| {
        dbglog!("[pf-vd] encode: CreateShaderResourceView failed: {e:?}");
        (-2, "srv")
    })?;
    v.ok_or((-2, "srv"))
}

/// A shared D3D11 fence, its NT handle and the context that signals it — PyroWave's
/// cross-device ordering, one per target set. The handle closes with this value; the encoder
/// holds its own duplicate.
pub struct SharedFence {
    pub fence: d3d::ID3D11Fence,
    pub ctx4: d3d::ID3D11DeviceContext4,
    pub handle: HANDLE,
}

// SAFETY: the NT handle is a process-wide token this value alone closes; the COM objects are
// agile. Every use is serialized by the pool's state mutex.
unsafe impl Send for SharedFence {}
// SAFETY: as above — a shared reference hands out only by-value copies of the handle.
unsafe impl Sync for SharedFence {}

impl SharedFence {
    pub fn new(dev: &d3d::ID3D11Device, ctx: &d3d::ID3D11DeviceContext) -> Result<Self, Fail> {
        let fail = |what: &str, e: windows62::core::Error| {
            dbglog!("[pf-vd] encode: {what} failed: {e:?}");
            (-2, "fence")
        };
        let dev5: d3d::ID3D11Device5 = dev.cast().map_err(|e| fail("ID3D11Device5", e))?;
        let ctx4: d3d::ID3D11DeviceContext4 =
            ctx.cast().map_err(|e| fail("ID3D11DeviceContext4", e))?;
        let mut fence: Option<d3d::ID3D11Fence> = None;
        // SAFETY: `?`-checked calls on live interfaces; `fence` is a valid out-param checked
        // below. GENERIC_ALL (0x1000_0000) is the access the host hands pyrowave's import.
        let handle = unsafe {
            dev5.CreateFence(0, d3d::D3D11_FENCE_FLAG_SHARED, &mut fence)
                .map_err(|e| fail("CreateFence", e))?;
            fence
                .as_ref()
                .ok_or((-2, "fence"))?
                .CreateSharedHandle(None, 0x1000_0000, PCWSTR::null())
                .map_err(|e| fail("Fence CreateSharedHandle", e))?
        };
        Ok(Self {
            fence: fence.ok_or((-2, "fence"))?,
            ctx4,
            handle,
        })
    }
}

impl Drop for SharedFence {
    fn drop(&mut self) {
        // SAFETY: the NT handle `new` minted; the encoder holds its own duplicate.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// What a backend reads per frame. The choice itself is [`EncodeInput::choose`] in the wire
/// crate, so the reply's `chroma_444` and these targets cannot disagree; this side owns only
/// the D3D formats behind each variant.
pub type InputKind = EncodeInput;

/// The `PixelFormat` label the frame carries into `submit`.
pub fn pixel_format(kind: InputKind) -> PixelFormat {
    match kind {
        InputKind::Bgra => PixelFormat::Bgra,
        InputKind::Nv12 | InputKind::Planar { hdr: false, .. } => PixelFormat::Nv12,
        // P010Sdr rides the same P010 label so the AMF submit check (`format == P010`) passes;
        // the encoder's `hdr` flag, not the label, decides BT.709 vs BT.2020.
        InputKind::P010 | InputKind::P010Sdr | InputKind::Planar { hdr: true, .. } => {
            PixelFormat::P010
        }
        InputKind::Rgb10 => PixelFormat::Rgb10a2,
    }
}

/// The source the pass reads: FP16 under advanced colour, BGRA otherwise.
pub fn source_format(kind: InputKind) -> dxgi::DXGI_FORMAT {
    match kind {
        InputKind::P010 | InputKind::Rgb10 | InputKind::Planar { hdr: true, .. } => {
            dxgi::DXGI_FORMAT_R16G16B16A16_FLOAT
        }
        _ => dxgi::DXGI_FORMAT_B8G8R8A8_UNORM,
    }
}

enum Planes {
    Bgra(Vec<Tex>),
    Nv12 {
        conv: VideoConverter,
        out: Vec<Tex>,
    },
    P010 {
        conv: HdrP010Converter,
        out: Vec<(Tex, Rtv, Rtv)>,
    },
    /// 10-bit SDR: the same video-engine BGRA→YUV as [`Self::Nv12`], but a P010 target — the
    /// VideoProcessor writes BT.709 studio at ten bits (`YCBCR_STUDIO_G22_LEFT_P709`).
    P010Sdr {
        conv: VideoConverter,
        out: Vec<Tex>,
    },
    Rgb10 {
        conv: HdrRgb10Converter,
        out: Vec<(Tex, Rtv)>,
    },
    Planar {
        conv: BgraToYuvPlanes,
        y: Vec<(Tex, Rtv)>,
        cbcr: Vec<(Tex, Rtv)>,
        fence: SharedFence,
        fence_value: u64,
    },
}

/// The per-slot input targets for one backend, built once from the slot count.
///
/// GPU cost per composed frame: in forward mode (the client draws the pointer) exactly one pass
/// and nothing cursor-related; in blend mode one save of at most 256², one quad, and — converter
/// kinds — the scratch copy the deferred pass already makes.
pub struct Targets {
    kind: InputKind,
    dev: d3d::ID3D11Device,
    ctx: d3d::ID3D11DeviceContext,
    width: u32,
    height: u32,
    planes: Planes,
    /// Per slot: the source copy a deferred pass left for [`Self::frame`], in the source
    /// format with render-target and shader-resource binds. Made on first use.
    rgb: Vec<Option<(Tex, Srv)>>,
    /// Per slot: `rgb` holds the frame and the converter has not run yet.
    deferred: Vec<bool>,
    /// Per slot: `rgb` holds this slot's picture. Unlike [`Self::deferred`] this survives the
    /// convert in [`Self::frame`], because the scratch keeps the pixels — which is what lets a
    /// cursor-only re-encode restore and re-blend without a compose.
    scratch_holds_frame: Vec<bool>,
    /// The cursor quad, built on first use; `None` after a build failure, logged once.
    blend: Option<CursorBlendPass>,
    blend_failed: bool,
    /// What the last cursor blend covered, before it drew: `CURSOR_SHAPE_MAX` square, source
    /// format, copies only. Made on first blend.
    patch: Option<Tex>,
    /// Which slot [`Self::patch`] belongs to and the clipped rectangle it holds. One patch, one
    /// owner: a blend on another slot replaces it, and only the newest blended slot is ever
    /// restored — it is the pool's stash.
    under: Option<(usize, (u32, u32, u32, u32))>,
    /// The video-engine kinds only: whether their output still moves with the source.
    probe: Option<ContentProbe>,
}

impl Targets {
    pub fn new(
        kind: InputKind,
        dev: &d3d::ID3D11Device,
        ctx: &d3d::ID3D11DeviceContext,
        (w, h): (u32, u32),
        slots: usize,
    ) -> Result<Self, Fail> {
        let convert_err = |e: anyhow::Error| {
            dbglog!("[pf-vd] encode: converter build failed: {e:#}");
            (-2, "convert")
        };
        let rt = d3d::D3D11_BIND_RENDER_TARGET.0 as u32;
        let planes = match kind {
            InputKind::Bgra => {
                let bind = rt | d3d::D3D11_BIND_SHADER_RESOURCE.0 as u32;
                let slots = (0..slots)
                    .map(|_| make_tex62(dev, (w, h), dxgi::DXGI_FORMAT_B8G8R8A8_UNORM, bind, 0))
                    .collect::<Result<_, _>>()?;
                Planes::Bgra(slots)
            }
            InputKind::Nv12 => {
                let conv = VideoConverter::new(dev, ctx, w, h, false).map_err(convert_err)?;
                let out = (0..slots)
                    .map(|_| make_tex62(dev, (w, h), dxgi::DXGI_FORMAT_NV12, rt, 0))
                    .collect::<Result<_, _>>()?;
                Planes::Nv12 { conv, out }
            }
            InputKind::P010Sdr => {
                // 8-bit BGRA in, P010 out: the video engine studio-swings BT.709 at ten bits.
                let conv = VideoConverter::new(dev, ctx, w, h, false).map_err(convert_err)?;
                let out = (0..slots)
                    .map(|_| make_tex62(dev, (w, h), dxgi::DXGI_FORMAT_P010, rt, 0))
                    .collect::<Result<_, _>>()?;
                Planes::P010Sdr { conv, out }
            }
            InputKind::P010 => {
                let conv = HdrP010Converter::new(dev, w, h).map_err(convert_err)?;
                let mut out = Vec::with_capacity(slots);
                for _ in 0..slots {
                    let t = make_tex62(dev, (w, h), dxgi::DXGI_FORMAT_P010, rt, 0)?;
                    let y = HdrP010Converter::plane_rtv(dev, &t, dxgi::DXGI_FORMAT_R16_UNORM)
                        .map_err(convert_err)?;
                    let uv = HdrP010Converter::plane_rtv(dev, &t, dxgi::DXGI_FORMAT_R16G16_UNORM)
                        .map_err(convert_err)?;
                    out.push((t, y, uv));
                }
                Planes::P010 { conv, out }
            }
            InputKind::Rgb10 => {
                let conv = HdrRgb10Converter::new(dev).map_err(convert_err)?;
                let mut out = Vec::with_capacity(slots);
                for _ in 0..slots {
                    let t = make_tex62(dev, (w, h), dxgi::DXGI_FORMAT_R10G10B10A2_UNORM, rt, 0)?;
                    let v = HdrRgb10Converter::rtv(dev, &t).map_err(convert_err)?;
                    out.push((t, v));
                }
                Planes::Rgb10 { conv, out }
            }
            InputKind::Planar { hdr, chroma444 } => {
                let conv = BgraToYuvPlanes::new(dev, hdr, chroma444).map_err(convert_err)?;
                let shared = (d3d::D3D11_RESOURCE_MISC_SHARED.0
                    | d3d::D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                    as u32;
                let (yf, cf) = if hdr {
                    (dxgi::DXGI_FORMAT_R16_UNORM, dxgi::DXGI_FORMAT_R16G16_UNORM)
                } else {
                    (dxgi::DXGI_FORMAT_R8_UNORM, dxgi::DXGI_FORMAT_R8G8_UNORM)
                };
                let chroma = if chroma444 { (w, h) } else { (w / 2, h / 2) };
                let mut y = Vec::with_capacity(slots);
                let mut cbcr = Vec::with_capacity(slots);
                for _ in 0..slots {
                    let yt = make_tex62(dev, (w, h), yf, rt, shared)?;
                    let ct = make_tex62(dev, chroma, cf, rt, shared)?;
                    y.push((yt.clone(), rtv(dev, &yt)?));
                    cbcr.push((ct.clone(), rtv(dev, &ct)?));
                }
                let fence = SharedFence::new(dev, ctx)?;
                Planes::Planar {
                    conv,
                    y,
                    cbcr,
                    fence,
                    fence_value: 0,
                }
            }
        };
        Ok(Self {
            kind,
            dev: dev.clone(),
            ctx: ctx.clone(),
            width: w,
            height: h,
            planes,
            rgb: (0..slots).map(|_| None).collect(),
            deferred: vec![false; slots],
            scratch_holds_frame: vec![false; slots],
            blend: None,
            blend_failed: false,
            patch: None,
            under: None,
            probe: match kind {
                InputKind::Nv12 => Some(ContentProbe::new("nv12")),
                InputKind::P010Sdr => Some(ContentProbe::new("p010sdr")),
                _ => None,
            },
        })
    }

    /// Hand slot `i`'s fresh conversion and the source it came from to the content probe.
    fn probe_after(&mut self, src: &Tex, i: usize) {
        let out = match &self.planes {
            Planes::Nv12 { out, .. } | Planes::P010Sdr { out, .. } => &out[i],
            _ => return,
        };
        if let Some(p) = self.probe.as_mut() {
            p.after_convert(&self.dev, &self.ctx, src, out);
        }
    }

    /// Undo the last cursor blend on slot `i`, so the next one draws the pointer onto a
    /// pointer-free picture instead of piling onto the old one. Nothing saved means nothing to
    /// put back — a slot no blend has touched is already clean.
    ///
    /// `Err` when a converter kind's slot has no frame in its RGB scratch: blending began after
    /// the pass that filled the slot, so there is nothing to re-blend until the next compose.
    /// The BGRA slot is its own scratch and is always restorable.
    pub fn restore_under(&mut self, i: usize) -> Result<(), Fail> {
        let converter = !matches!(self.planes, Planes::Bgra(_));
        if converter && !self.scratch_holds_frame[i] {
            return Err((-2, "scratch"));
        }
        let _ctx = lock(&CTX);
        if let Some((owner, (x, y, w, h))) = self.under
            && owner == i
        {
            let dst = match &self.planes {
                Planes::Bgra(slots) => slots[i].clone(),
                _ => self.rgb[i].as_ref().ok_or((-2, "scratch"))?.0.clone(),
            };
            let patch = self.patch.as_ref().ok_or((-2, "patch"))?;
            let region = d3d::D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            // SAFETY: `dst` is this slot's live RGB image and `patch` holds exactly `w`×`h` of
            // it, saved at `(x, y)` from the same texture in `save_under`. Same device, whose
            // immediate context is multithread-protected (`Direct3DDevice`).
            unsafe {
                self.ctx
                    .CopySubresourceRegion(&dst, 0, x, y, 0, patch, 0, Some(&region));
            }
            self.under = None;
        }
        // The converter ran over the blended scratch in `frame`; it has to run again over the
        // restored one, after the caller re-blends.
        if converter {
            self.deferred[i] = true;
        }
        Ok(())
    }

    /// One GPU pass from `src` (BGRA or FP16, the pool's size) into slot `i`: a copy for BGRA,
    /// the video engine for NV12, a draw for P010 and the planar pair. With `defer` the
    /// converter kinds copy into the slot's RGB scratch instead and convert in [`Self::frame`],
    /// after the cursor blend. `src` needs no bind flags beyond what the shader kinds read
    /// through an SRV created per call.
    pub fn pass(&mut self, src: &Tex, i: usize, defer: bool) -> Result<(), Fail> {
        let _ctx = lock(&CTX);
        self.deferred[i] = false;
        self.scratch_holds_frame[i] = false;
        // Fresh pixels: whatever the last blend on this slot covered is gone with them.
        if self.under.is_some_and(|(owner, _)| owner == i) {
            self.under = None;
        }
        if let Planes::Bgra(slots) = &self.planes {
            // SAFETY: `src` and the slot are live same-size textures on the same device, whose
            // immediate context is multithread-protected (`Direct3DDevice`).
            unsafe { self.ctx.CopyResource(&slots[i], src) };
            return Ok(());
        }
        if !defer {
            self.convert(src, None, i)?;
            self.probe_after(src, i);
            return Ok(());
        }
        if self.rgb[i].is_none() {
            let bind = (d3d::D3D11_BIND_RENDER_TARGET.0 | d3d::D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let t = make_tex62(
                &self.dev,
                (self.width, self.height),
                source_format(self.kind),
                bind,
                0,
            )?;
            let v = srv(&self.dev, &t)?;
            self.rgb[i] = Some((t, v));
        }
        let (scratch, _) = self.rgb[i].as_ref().ok_or((-2, "scratch"))?;
        // SAFETY: as the BGRA copy — same size and format by construction, same device.
        unsafe { self.ctx.CopyResource(scratch, src) };
        self.deferred[i] = true;
        self.scratch_holds_frame[i] = true;
        Ok(())
    }

    /// Keep what the cursor quad is about to cover, so a later pointer move can put the picture
    /// back without a full-frame copy. At most `CURSOR_SHAPE_MAX` square, clipped to the target
    /// ([`pf_driver_proto::cursor::clip_rect`]) — a pointer half off an edge saves only the half
    /// the blend will actually write. Failure leaves nothing recorded, so no wrong restore.
    fn save_under(&mut self, i: usize, dst: &Tex, cursor: &CursorImage) {
        use pf_driver_proto::cursor::{CURSOR_SHAPE_MAX, clip_rect};
        self.under = None;
        // The shape came through `shape_rgba`, which clamps to the declared max; the patch is
        // built for exactly that, so this is what keeps the copy inside it.
        let (w, h) = (
            cursor.w.min(CURSOR_SHAPE_MAX),
            cursor.h.min(CURSOR_SHAPE_MAX),
        );
        let Some(rect) = clip_rect(cursor.x, cursor.y, w, h, self.width, self.height) else {
            return;
        };
        if self.patch.is_none() {
            self.patch = make_tex62(
                &self.dev,
                (CURSOR_SHAPE_MAX, CURSOR_SHAPE_MAX),
                source_format(self.kind),
                0,
                0,
            )
            .ok();
        }
        let Some(patch) = self.patch.as_ref() else {
            return;
        };
        let (x, y, w, h) = rect;
        let region = d3d::D3D11_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + w,
            bottom: y + h,
            back: 1,
        };
        // SAFETY: `dst` is the slot's live RGB image at this target's size and format; `region`
        // is clipped to it and no wider than the patch. Same device, whose immediate context is
        // multithread-protected (`Direct3DDevice`).
        unsafe {
            self.ctx
                .CopySubresourceRegion(patch, 0, 0, 0, 0, dst, 0, Some(&region));
        }
        self.under = Some((i, rect));
    }

    /// The converter for slot `i` from `src`; `view` is its cached SRV, else one is made.
    fn convert(&self, src: &Tex, view: Option<&Srv>, i: usize) -> Result<(), Fail> {
        let convert_err = |e: anyhow::Error| {
            dbglog!("[pf-vd] encode: convert failed: {e:#}");
            (-2, "convert")
        };
        let view = || match view {
            Some(v) => Ok(v.clone()),
            None => srv(&self.dev, src),
        };
        match &self.planes {
            Planes::Bgra(_) => {}
            Planes::Nv12 { conv, out } => conv.convert(src, &out[i]).map_err(convert_err)?,
            Planes::P010Sdr { conv, out } => conv.convert(src, &out[i]).map_err(convert_err)?,
            Planes::P010 { conv, out } => conv
                .convert(
                    &self.ctx,
                    &view()?,
                    &out[i].1,
                    &out[i].2,
                    self.width,
                    self.height,
                )
                .map_err(convert_err)?,
            Planes::Rgb10 { conv, out } => conv
                .convert(&self.ctx, &view()?, &out[i].1, self.width, self.height)
                .map_err(convert_err)?,
            Planes::Planar { conv, y, cbcr, .. } => conv
                .convert(
                    &self.ctx,
                    &view()?,
                    &y[i].1,
                    &cbcr[i].1,
                    self.width,
                    self.height,
                )
                .map_err(convert_err)?,
        }
        Ok(())
    }

    /// Draw `cursor` over slot `i`'s RGB image — the BGRA slot itself, or the deferred scratch.
    /// A failure loses the pointer, never the frame, and is logged once.
    fn blend(&mut self, i: usize, cursor: &CursorImage, scale: f32) {
        let fp16 = source_format(self.kind) == dxgi::DXGI_FORMAT_R16G16B16A16_FLOAT;
        let dst = match &self.planes {
            Planes::Bgra(slots) => slots[i].clone(),
            _ if self.deferred[i] => match &self.rgb[i] {
                Some((t, _)) => t.clone(),
                None => return,
            },
            _ => return,
        };
        if self.blend.is_none() && !self.blend_failed {
            match CursorBlendPass::new(&self.dev) {
                Ok(p) => self.blend = Some(p),
                Err(e) => {
                    self.blend_failed = true;
                    dbglog!("[pf-vd] encode: cursor blend pass did not build: {e:#}");
                }
            }
        }
        if self.blend.is_none() {
            return;
        }
        // Before the draw: what the quad covers is only recoverable now.
        self.save_under(i, &dst, cursor);
        let pass = self.blend.as_mut().expect("checked just above");
        let overlay = CursorOverlay {
            x: cursor.x,
            y: cursor.y,
            w: cursor.w,
            h: cursor.h,
            rgba: Arc::clone(&cursor.rgba),
            serial: u64::from(cursor.serial),
            hot_x: cursor.hot_x,
            hot_y: cursor.hot_y,
            visible: true,
        };
        let linear_scale = if fp16 { scale } else { 0.0 };
        if let Err(e) = pass.blend(&self.dev, &self.ctx, &dst, &overlay, linear_scale)
            && !self.blend_failed
        {
            self.blend_failed = true;
            dbglog!("[pf-vd] encode: cursor blend draw failed: {e:#}");
        }
    }

    /// Spike S6: wrap the acquired surface itself as the frame `submit` takes — no pass, no
    /// slot, and no pointer, because there is no driver-owned image to draw one on. Only the
    /// BGRA kind reaches this; every other kind's converter has to run first.
    pub fn direct_frame(&self, src: &Tex, pts_ns: u64) -> CapturedFrame {
        CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns,
            format: pixel_format(self.kind),
            payload: FramePayload::D3d11(D3d11Frame {
                texture: src.clone(),
                device: self.dev.clone(),
                pyro: None,
            }),
            cursor: None,
            provenance: Provenance::UNTRACKED,
        }
    }

    /// Wrap filled slot `i` as the frame `submit` takes, after the cursor blend and the
    /// converter a deferred pass left for this thread. The planar pair signals its fence here,
    /// so the Vulkan wait orders after the pass however far apart the two threads ran.
    pub fn frame(
        &mut self,
        i: usize,
        pts_ns: u64,
        cursor: Option<(CursorImage, f32)>,
    ) -> Result<CapturedFrame, Fail> {
        let _ctx = lock(&CTX);
        if let Some((image, scale)) = cursor {
            self.blend(i, &image, scale);
        }
        if self.deferred[i] {
            self.deferred[i] = false;
            let (t, v) = self.rgb[i].clone().ok_or((-2, "scratch"))?;
            self.convert(&t, Some(&v), i)?;
            self.probe_after(&t, i);
        }
        let (texture, pyro) = match &mut self.planes {
            Planes::Bgra(slots) => (slots[i].clone(), None),
            Planes::Nv12 { out, .. } => (out[i].clone(), None),
            Planes::P010Sdr { out, .. } => (out[i].clone(), None),
            Planes::P010 { out, .. } => (out[i].0.clone(), None),
            Planes::Rgb10 { out, .. } => (out[i].0.clone(), None),
            Planes::Planar {
                y,
                cbcr,
                fence,
                fence_value,
                ..
            } => {
                *fence_value += 1;
                // SAFETY: `fence` is the live shared fence on this context's device; `Flush`
                // submits the queued convert + signal so the Vulkan wait can resolve.
                unsafe {
                    fence
                        .ctx4
                        .Signal(&fence.fence, *fence_value)
                        .map_err(|_| (-2, "fence"))?;
                    self.ctx.Flush();
                }
                let share = PyroFrameShare {
                    cbcr: cbcr[i].0.clone(),
                    fence_handle: Some(fence.handle.0 as isize),
                    fence_value: *fence_value,
                    ring_gen: 1,
                };
                (y[i].0.clone(), Some(share))
            }
        };
        Ok(CapturedFrame {
            width: self.width,
            height: self.height,
            pts_ns,
            format: pixel_format(self.kind),
            payload: FramePayload::D3d11(D3d11Frame {
                texture,
                device: self.dev.clone(),
                pyro,
            }),
            cursor: None,
            provenance: Provenance::UNTRACKED,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows62::Win32::Foundation::HMODULE;
    use windows62::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};

    /// A device for the pixel tests: the real adapter, else WARP, so a runner with no GPU still
    /// runs them. `None` means neither exists and the caller skips.
    fn device() -> Option<(d3d::ID3D11Device, d3d::ID3D11DeviceContext)> {
        for kind in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
            let (mut dev, mut ctx) = (None, None);
            // SAFETY: plain device creation; both out-params are valid locals checked below.
            let hr = unsafe {
                d3d::D3D11CreateDevice(
                    None,
                    kind,
                    HMODULE(core::ptr::null_mut()),
                    d3d::D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    d3d::D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                )
            };
            if let (Ok(()), Some(dev), Some(ctx)) = (hr, dev, ctx) {
                return Some((dev, ctx));
            }
        }
        None
    }

    /// Slot 0's pixels, through a staging copy.
    fn read_back(t: &Targets, ctx: &d3d::ID3D11DeviceContext, w: u32, h: u32) -> Vec<u8> {
        let Planes::Bgra(slots) = &t.planes else {
            panic!("BGRA targets only")
        };
        let desc = d3d::D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: dxgi::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: d3d::D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut stage: Option<Tex> = None;
        // SAFETY: `desc` is a fully-initialized local and `stage` a valid out-param; the copy and
        // the map below run on the same immediate context that owns both textures.
        unsafe {
            t.dev
                .CreateTexture2D(&desc, None, Some(&mut stage))
                .expect("staging texture");
            let stage = stage.expect("staging texture");
            ctx.CopyResource(&stage, &slots[0]);
            let mut m = d3d::D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&stage, 0, d3d::D3D11_MAP_READ, 0, Some(&mut m))
                .expect("map staging");
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for row in 0..h {
                let src = (m.pData as *const u8).add((row * m.RowPitch) as usize);
                out.extend_from_slice(core::slice::from_raw_parts(src, (w * 4) as usize));
            }
            ctx.Unmap(&stage, 0);
            out
        }
    }

    fn source(dev: &d3d::ID3D11Device, w: u32, h: u32) -> Tex {
        let pixels: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, (i % 253) as u8, (i % 241) as u8, 0xFF])
            .collect();
        let desc = d3d::D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: dxgi::DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: dxgi::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: d3d::D3D11_USAGE_DEFAULT,
            BindFlags: d3d::D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = d3d::D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut t: Option<Tex> = None;
        // SAFETY: `desc`/`init` are locals describing `pixels`, which outlives the call; `t` is a
        // valid out-param checked by `expect`.
        unsafe {
            dev.CreateTexture2D(&desc, Some(&init), Some(&mut t))
                .expect("source texture");
        }
        t.expect("source texture")
    }

    fn pointer(x: i32, y: i32) -> CursorImage {
        CursorImage {
            x,
            y,
            w: 16,
            h: 16,
            hot_x: 0,
            hot_y: 0,
            rgba: Arc::new(vec![0xFF; 16 * 16 * 4]),
            serial: 1,
            visible: true,
        }
    }

    /// The save-under contract: the blend changes the slot, and the restore puts back exactly
    /// what was there. A blend that never drew would leave the two reads equal and fail here
    /// first, so a build without the quad cannot pass this by doing nothing.
    #[test]
    fn a_restored_slot_is_the_frame_the_blend_covered() {
        let Some((dev, ctx)) = device() else {
            eprintln!("no D3D11 device (not even WARP) — skipping");
            return;
        };
        let (w, h) = (64u32, 64u32);
        let mut t = Targets::new(InputKind::Bgra, &dev, &ctx, (w, h), 1).expect("targets");
        let src = source(&dev, w, h);
        t.pass(&src, 0, true).expect("pass");
        let clean = read_back(&t, &ctx, w, h);

        t.blend(0, &pointer(8, 8), 0.0);
        let drawn = read_back(&t, &ctx, w, h);
        assert_ne!(clean, drawn, "the cursor quad drew nothing");

        t.restore_under(0).expect("restore");
        assert_eq!(read_back(&t, &ctx, w, h), clean, "restore left the pointer");
    }

    /// A pointer hanging off the edge saves only the part the blend can write, and the restore
    /// puts that part back — the clip is what keeps the copy inside both textures.
    #[test]
    fn a_pointer_off_the_edge_restores_the_part_that_landed() {
        let Some((dev, ctx)) = device() else {
            eprintln!("no D3D11 device (not even WARP) — skipping");
            return;
        };
        let (w, h) = (64u32, 64u32);
        let mut t = Targets::new(InputKind::Bgra, &dev, &ctx, (w, h), 1).expect("targets");
        let src = source(&dev, w, h);
        t.pass(&src, 0, true).expect("pass");
        let clean = read_back(&t, &ctx, w, h);

        t.blend(0, &pointer(-8, 56), 0.0);
        assert_ne!(
            clean,
            read_back(&t, &ctx, w, h),
            "the cursor quad drew nothing"
        );
        t.restore_under(0).expect("restore");
        assert_eq!(read_back(&t, &ctx, w, h), clean, "restore left the pointer");

        // Wholly outside: nothing saved, nothing to put back, and no stale rectangle kept.
        t.blend(0, &pointer(200, 200), 0.0);
        t.restore_under(0).expect("restore");
        assert_eq!(read_back(&t, &ctx, w, h), clean);
    }
}
