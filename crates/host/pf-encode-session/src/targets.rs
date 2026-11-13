//! The backend input [`Targets`]: one set of textures per pool slot, in whatever format the
//! opened backend reads, plus the converter that fills them from a BGRA or FP16 source.
//! [`Targets::pass`] is the fused pass the source's own thread runs on each new frame;
//! [`Targets::frame`] wraps a filled slot as the `CapturedFrame` `submit` takes.
//!
//! A blended pointer costs that pass nothing: it then only copies the source into a
//! source-sized RGB scratch, and `frame` (the encode thread) draws the cursor quad and runs the
//! converter from there. A composed kind's slot (BGRA, FP16) is its own scratch, so it copies
//! once either way; the converter kinds pay one extra full-frame copy while the client draws no
//! pointer.
//!
//! [`Targets::framed`] scales: the scale pass runs where the converter would, before it, so a
//! composed slot is no longer its own scratch and the pointer lands at source size first.

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex};

use pf_driver_proto::encode::{EncodeInput, Framing};
use pf_encode_win::convert::{
    BgraToYuvPlanes, CursorBlendPass, HdrP010Converter, HdrRgb10Converter, ReframePass,
    VideoConverter,
};
use pf_frame::dxgi::{D3d11Frame, PyroFrameShare};
use pf_frame::{CapturedFrame, CursorOverlay, FramePayload, PixelFormat, Provenance};
use windows::Win32::Graphics::Direct3D11 as d3d;
use windows::Win32::Graphics::Dxgi::Common as dxgi;
use windows::core::{Interface as _, PCWSTR};

use crate::{Fail, lock};

/// Sources on one adapter can share a device's immediate context, and
/// `SetMultithreadProtected` serialises single calls, not the draw sequences a pass issues.
/// Held for each pass, always after the source's own state lock.
static CTX: Mutex<()> = Mutex::new(());

/// The pointer a source blends: straight-alpha RGBA at its frame-relative top-left, negative
/// past the frame's own. `serial` is the OS shape id.
#[derive(Clone)]
pub struct CursorImage {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: Arc<Vec<u8>>,
    pub serial: u32,
    pub visible: bool,
}

type Tex = d3d::ID3D11Texture2D;
type Rtv = d3d::ID3D11RenderTargetView;
type Srv = d3d::ID3D11ShaderResourceView;

/// One default-usage texture on `dev` with the given bind and misc flags.
pub fn make_tex(
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
            tracing::info!("encode: CreateTexture2D({format:?}) failed: {r:?}");
            Err((-2, "pool"))
        }
    }
}

fn rtv(dev: &d3d::ID3D11Device, t: &Tex) -> Result<Rtv, Fail> {
    let mut v = None;
    // SAFETY: `t` is a live render-target texture on `dev`; `v` a valid out-param.
    unsafe { dev.CreateRenderTargetView(t, None, Some(&mut v)) }.map_err(|e| {
        tracing::info!("encode: CreateRenderTargetView failed: {e:?}");
        (-2, "rtv")
    })?;
    v.ok_or((-2, "rtv"))
}

fn srv(dev: &d3d::ID3D11Device, t: &Tex) -> Result<Srv, Fail> {
    let mut v = None;
    // SAFETY: `t` is a live shader-resource texture on `dev`; `v` a valid out-param.
    unsafe { dev.CreateShaderResourceView(t, None, Some(&mut v)) }.map_err(|e| {
        tracing::info!("encode: CreateShaderResourceView failed: {e:?}");
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
    pub handle: OwnedHandle,
}

impl SharedFence {
    pub fn new(dev: &d3d::ID3D11Device, ctx: &d3d::ID3D11DeviceContext) -> Result<Self, Fail> {
        let fail = |what: &str, e: windows::core::Error| {
            tracing::info!("encode: {what} failed: {e:?}");
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
        // SAFETY: the NT handle `CreateSharedHandle` just minted; nothing else closes it.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        Ok(Self {
            fence: fence.ok_or((-2, "fence"))?,
            ctx4,
            handle,
        })
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
        InputKind::Nv12
        | InputKind::Planar {
            hdr: false,
            wcg: false,
            ..
        } => PixelFormat::Nv12,
        // P010Sdr and P010Wcg ride the same P010 label so the AMF submit check
        // (`format == P010`) passes; the encoder's `hdr` flag, not the label, decides BT.709 vs
        // BT.2020.
        InputKind::P010 | InputKind::P010Sdr | InputKind::P010Wcg | InputKind::Planar { .. } => {
            PixelFormat::P010
        }
        InputKind::Rgb10 => PixelFormat::Rgb10a2,
        InputKind::Rgb10Wcg => PixelFormat::Rgb10a2Sdr,
        InputKind::Fp16 => PixelFormat::RgbaF16,
    }
}

/// The source the pass reads: FP16 under advanced colour (HDR or SDR wide colour), BGRA
/// otherwise.
pub fn source_format(kind: InputKind) -> dxgi::DXGI_FORMAT {
    match kind {
        InputKind::P010
        | InputKind::Rgb10
        | InputKind::Fp16
        | InputKind::Rgb10Wcg
        | InputKind::P010Wcg
        | InputKind::Planar { hdr: true, .. }
        | InputKind::Planar { wcg: true, .. } => dxgi::DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => dxgi::DXGI_FORMAT_B8G8R8A8_UNORM,
    }
}

enum Planes {
    /// BGRA or FP16: the slot holds the source's own format and the backend converts.
    Composed(Vec<Tex>),
    Nv12 {
        conv: VideoConverter,
        out: Vec<Tex>,
    },
    P010 {
        conv: HdrP010Converter,
        out: Vec<(Tex, Rtv, Rtv)>,
    },
    /// 10-bit SDR: the same video-engine BGRA→YUV as [`Self::Nv12`], but a P010 target — the
    /// VideoProcessor writes BT.709 full range at ten bits (`YCBCR_FULL_G22_LEFT_P709`).
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

/// A framed session's scale ([`Targets::framed`]): the source's size, the pass, and where it
/// writes.
struct Scale {
    /// The surface the source hands over: the scratch's size and the pointer's frame.
    source: (u32, u32),
    pass: ReframePass,
    target: ScaleTarget,
}

enum ScaleTarget {
    /// A composed kind's slots, which the pass writes directly.
    Slots(Vec<Rtv>),
    /// The encode-size picture, in the source format, that a converter kind's converter reads.
    Picture(Tex, Srv, Rtv),
}

/// The per-slot input targets for one backend, built once from the slot count.
///
/// GPU cost per composed frame: in forward mode (the client draws the pointer) exactly one pass
/// and nothing cursor-related; in blend mode one save of at most 256², one quad, and — converter
/// kinds — the scratch copy the deferred pass already makes. Framed, the scale pass runs before
/// the converter, and a composed kind in blend mode pays the converter kinds' scratch copy.
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
    /// Set by [`Self::framed`] when the source is not the encode picture.
    scale: Option<Scale>,
}

impl Targets {
    /// `full_range` is the range the session advertised, which the YUV converters write, HDR
    /// included. The PyroWave converters write full range, the only range the host advertises
    /// for them.
    pub fn new(
        kind: InputKind,
        dev: &d3d::ID3D11Device,
        ctx: &d3d::ID3D11DeviceContext,
        (w, h): (u32, u32),
        slots: usize,
        full_range: bool,
    ) -> Result<Self, Fail> {
        let convert_err = |e: anyhow::Error| {
            tracing::info!("encode: converter build failed: {e:#}");
            (-2, "convert")
        };
        let rt = d3d::D3D11_BIND_RENDER_TARGET.0 as u32;
        let planes = match kind {
            InputKind::Bgra | InputKind::Fp16 => {
                let bind = rt | d3d::D3D11_BIND_SHADER_RESOURCE.0 as u32;
                let slots = (0..slots)
                    .map(|_| make_tex(dev, (w, h), source_format(kind), bind, 0))
                    .collect::<Result<_, _>>()?;
                Planes::Composed(slots)
            }
            InputKind::Nv12 => {
                let conv =
                    VideoConverter::new(dev, ctx, w, h, false, full_range).map_err(convert_err)?;
                let out = (0..slots)
                    .map(|_| make_tex(dev, (w, h), dxgi::DXGI_FORMAT_NV12, rt, 0))
                    .collect::<Result<_, _>>()?;
                Planes::Nv12 { conv, out }
            }
            InputKind::P010Sdr => {
                // 8-bit BGRA in, P010 out: the video engine writes BT.709 at ten bits.
                let conv =
                    VideoConverter::new(dev, ctx, w, h, false, full_range).map_err(convert_err)?;
                let out = (0..slots)
                    .map(|_| make_tex(dev, (w, h), dxgi::DXGI_FORMAT_P010, rt, 0))
                    .collect::<Result<_, _>>()?;
                Planes::P010Sdr { conv, out }
            }
            InputKind::P010 | InputKind::P010Wcg => {
                let conv = if kind == InputKind::P010 {
                    HdrP010Converter::new(dev, w, h, full_range)
                } else {
                    HdrP010Converter::new_sdr_fp16(dev, w, h, full_range)
                }
                .map_err(convert_err)?;
                let mut out = Vec::with_capacity(slots);
                for _ in 0..slots {
                    let t = make_tex(dev, (w, h), dxgi::DXGI_FORMAT_P010, rt, 0)?;
                    let y = HdrP010Converter::plane_rtv(dev, &t, dxgi::DXGI_FORMAT_R16_UNORM)
                        .map_err(convert_err)?;
                    let uv = HdrP010Converter::plane_rtv(dev, &t, dxgi::DXGI_FORMAT_R16G16_UNORM)
                        .map_err(convert_err)?;
                    out.push((t, y, uv));
                }
                Planes::P010 { conv, out }
            }
            InputKind::Rgb10 | InputKind::Rgb10Wcg => {
                let conv = if kind == InputKind::Rgb10 {
                    HdrRgb10Converter::new(dev)
                } else {
                    HdrRgb10Converter::new_sdr_fp16(dev)
                }
                .map_err(convert_err)?;
                let mut out = Vec::with_capacity(slots);
                for _ in 0..slots {
                    let t = make_tex(dev, (w, h), dxgi::DXGI_FORMAT_R10G10B10A2_UNORM, rt, 0)?;
                    let v = HdrRgb10Converter::rtv(dev, &t).map_err(convert_err)?;
                    out.push((t, v));
                }
                Planes::Rgb10 { conv, out }
            }
            InputKind::Planar {
                hdr,
                wcg,
                chroma444,
            } => {
                let conv = BgraToYuvPlanes::new(dev, hdr, wcg, chroma444).map_err(convert_err)?;
                let shared = (d3d::D3D11_RESOURCE_MISC_SHARED.0
                    | d3d::D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0)
                    as u32;
                let (yf, cf) = if hdr || wcg {
                    (dxgi::DXGI_FORMAT_R16_UNORM, dxgi::DXGI_FORMAT_R16G16_UNORM)
                } else {
                    (dxgi::DXGI_FORMAT_R8_UNORM, dxgi::DXGI_FORMAT_R8G8_UNORM)
                };
                let chroma = if chroma444 { (w, h) } else { (w / 2, h / 2) };
                let mut y = Vec::with_capacity(slots);
                let mut cbcr = Vec::with_capacity(slots);
                for _ in 0..slots {
                    let yt = make_tex(dev, (w, h), yf, rt, shared)?;
                    let ct = make_tex(dev, chroma, cf, rt, shared)?;
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
            scale: None,
        })
    }

    /// These targets fed `framing`'s source surfaces: `crop` of each, scaled to the size they
    /// were built at. A framing that does not scale changes nothing, so its frames take exactly
    /// the unframed path.
    pub fn framed(mut self, framing: &Framing) -> Result<Self, Fail> {
        if !framing.scales() {
            return Ok(self);
        }
        let size = (self.width, self.height);
        if framing.encode != size {
            return Err((-2, "scale"));
        }
        let fail = |e: anyhow::Error| {
            tracing::info!("encode: scale pass build failed: {e:#}");
            (-2, "scale")
        };
        let fp16 = source_format(self.kind) == dxgi::DXGI_FORMAT_R16G16B16A16_FLOAT;
        let pass = ReframePass::new(&self.dev, framing.crop, size, fp16).map_err(fail)?;
        let target = match &self.planes {
            Planes::Composed(slots) => ScaleTarget::Slots(
                slots
                    .iter()
                    .map(|t| rtv(&self.dev, t))
                    .collect::<Result<_, _>>()?,
            ),
            _ => {
                let bind =
                    (d3d::D3D11_BIND_RENDER_TARGET.0 | d3d::D3D11_BIND_SHADER_RESOURCE.0) as u32;
                let t = make_tex(&self.dev, size, source_format(self.kind), bind, 0)?;
                let (v, r) = (srv(&self.dev, &t)?, rtv(&self.dev, &t)?);
                ScaleTarget::Picture(t, v, r)
            }
        };
        self.scale = Some(Scale {
            source: framing.source,
            pass,
            target,
        });
        Ok(self)
    }

    /// The source's size: the encode size unless [`Self::framed`] scales.
    fn source_size(&self) -> (u32, u32) {
        self.scale
            .as_ref()
            .map_or((self.width, self.height), |s| s.source)
    }

    /// An unframed composed slot: the source's own size and format, so it is its own scratch.
    fn slot_is_scratch(&self) -> bool {
        matches!(self.planes, Planes::Composed(_)) && self.scale.is_none()
    }

    /// The picture slot `i`'s pointer is drawn on: the slot when it is its own scratch, else
    /// its RGB scratch.
    fn blend_target(&self, i: usize) -> Option<Tex> {
        match &self.planes {
            Planes::Composed(slots) if self.scale.is_none() => Some(slots[i].clone()),
            _ => self.rgb[i].as_ref().map(|(t, _)| t.clone()),
        }
    }

    /// Undo the last cursor blend on slot `i`, so the next one draws the pointer onto a
    /// pointer-free picture instead of piling onto the old one. Nothing saved means nothing to
    /// put back — a slot no blend has touched is already clean.
    ///
    /// `Err` when a slot that is not its own scratch has no frame in its RGB scratch: blending
    /// began after the pass that filled the slot, so there is nothing to re-blend until the
    /// next compose. An unframed composed slot is its own scratch and is always restorable.
    pub fn restore_under(&mut self, i: usize) -> Result<(), Fail> {
        let converter = !self.slot_is_scratch();
        if converter && !self.scratch_holds_frame[i] {
            return Err((-2, "scratch"));
        }
        let _ctx = lock(&CTX);
        if let Some((owner, (x, y, w, h))) = self.under
            && owner == i
        {
            let dst = self.blend_target(i).ok_or((-2, "scratch"))?;
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
            // immediate context is multithread-protected by whoever made it.
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

    /// One GPU pass from `src` (BGRA or FP16, the source's size) into slot `i`: a copy for the
    /// composed kinds, the video engine for NV12, a draw for P010 and the planar pair, each
    /// after the scale pass when framed. With `defer` every slot that is not its own scratch
    /// copies into its RGB scratch instead and converts in [`Self::frame`], after the cursor
    /// blend. `src` needs no bind flags beyond what the shader passes read through an SRV
    /// created per call.
    pub fn pass(&mut self, src: &Tex, i: usize, defer: bool) -> Result<(), Fail> {
        let _ctx = lock(&CTX);
        self.deferred[i] = false;
        self.scratch_holds_frame[i] = false;
        // Fresh pixels: whatever the last blend on this slot covered is gone with them.
        if self.under.is_some_and(|(owner, _)| owner == i) {
            self.under = None;
        }
        if let (Planes::Composed(slots), None) = (&self.planes, &self.scale) {
            // SAFETY: `src` and the slot are live same-size textures on the same device, whose
            // immediate context is multithread-protected by whoever made it.
            unsafe { self.ctx.CopyResource(&slots[i], src) };
            return Ok(());
        }
        if !defer {
            return self.convert(src, None, i);
        }
        if self.rgb[i].is_none() {
            let bind = (d3d::D3D11_BIND_RENDER_TARGET.0 | d3d::D3D11_BIND_SHADER_RESOURCE.0) as u32;
            let t = make_tex(
                &self.dev,
                self.source_size(),
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
        let (sw, sh) = self.source_size();
        let Some(rect) = clip_rect(cursor.x, cursor.y, w, h, sw, sh) else {
            return;
        };
        if self.patch.is_none() {
            self.patch = make_tex(
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
        // multithread-protected by whoever made it.
        unsafe {
            self.ctx
                .CopySubresourceRegion(patch, 0, 0, 0, 0, dst, 0, Some(&region));
        }
        self.under = Some((i, rect));
    }

    /// The scale pass when framed, then the converter, for slot `i` from `src`; `view` is its
    /// cached SRV, else one is made. Unframed, this is [`Self::convert_from`] alone.
    fn convert(&self, src: &Tex, view: Option<&Srv>, i: usize) -> Result<(), Fail> {
        let Some(scale) = &self.scale else {
            return self.convert_from(src, view, i);
        };
        let view = match view {
            Some(v) => v.clone(),
            None => srv(&self.dev, src)?,
        };
        match &scale.target {
            ScaleTarget::Slots(rtvs) => scale.pass.scale(&self.ctx, &view, &rtvs[i]),
            ScaleTarget::Picture(t, v, r) => {
                scale.pass.scale(&self.ctx, &view, r);
                return self.convert_from(t, Some(v), i);
            }
        }
        Ok(())
    }

    /// The converter for slot `i` from `src`; `view` is its cached SRV, else one is made.
    fn convert_from(&self, src: &Tex, view: Option<&Srv>, i: usize) -> Result<(), Fail> {
        let convert_err = |e: anyhow::Error| {
            tracing::info!("encode: convert failed: {e:#}");
            (-2, "convert")
        };
        let view = || match view {
            Some(v) => Ok(v.clone()),
            None => srv(&self.dev, src),
        };
        match &self.planes {
            Planes::Composed(_) => {}
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

    /// Draw `cursor` over slot `i`'s RGB image at source size — the slot itself when it is its
    /// own scratch, else the deferred scratch. A failure loses the pointer, never the frame, and
    /// is logged once.
    fn blend(&mut self, i: usize, cursor: &CursorImage, scale: f32) {
        let fp16 = source_format(self.kind) == dxgi::DXGI_FORMAT_R16G16B16A16_FLOAT;
        if !self.slot_is_scratch() && !self.deferred[i] {
            return;
        }
        let Some(dst) = self.blend_target(i) else {
            return;
        };
        if self.blend.is_none() && !self.blend_failed {
            match CursorBlendPass::new(&self.dev) {
                Ok(p) => self.blend = Some(p),
                Err(e) => {
                    self.blend_failed = true;
                    tracing::info!("encode: cursor blend pass did not build: {e:#}");
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
            tracing::info!("encode: cursor blend draw failed: {e:#}");
        }
    }

    /// Wrap the source's own surface as the frame `submit` takes — no pass, no slot, and no
    /// pointer, because there is no image of ours to draw one on. Only an unframed composed kind
    /// reaches this; every other kind's converter or scale pass has to run first.
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
        }
        let (texture, pyro) = match &mut self.planes {
            Planes::Composed(slots) => (slots[i].clone(), None),
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
                    fence_handle: Some(fence.handle.as_raw_handle() as isize),
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

impl Drop for Targets {
    /// Give the slots back now. D3D11 frees a released texture at the next flush, and a
    /// device that outlives its sessions is idle after the last one.
    fn drop(&mut self) {
        self.planes = Planes::Composed(Vec::new());
        self.rgb.clear();
        self.patch = None;
        self.blend = None;
        self.scale = None;
        let _ctx = lock(&CTX);
        // SAFETY: a single call on the device's multithread-protected context.
        unsafe { self.ctx.Flush() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::d3d11_map::MappedSubresource;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};

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

    /// `tex`'s bytes through a staging copy, and the row pitch they sit at. NV12 and P010 carry
    /// their chroma rows after the luma rows.
    fn staged(
        dev: &d3d::ID3D11Device,
        ctx: &d3d::ID3D11DeviceContext,
        tex: &Tex,
        (w, h): (u32, u32),
        format: dxgi::DXGI_FORMAT,
    ) -> (Vec<u8>, usize) {
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
            Usage: d3d::D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: d3d::D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut stage: Option<Tex> = None;
        // SAFETY: `desc` is a fully-initialized local and `stage` a valid out-param.
        unsafe { dev.CreateTexture2D(&desc, None, Some(&mut stage)) }.expect("staging texture");
        let stage = stage.expect("staging texture");
        // SAFETY: both textures belong to the device whose immediate context `ctx` is.
        unsafe { ctx.CopyResource(&stage, tex) };
        let m = MappedSubresource::texture(ctx, &stage, d3d::D3D11_MAP_READ).expect("map staging");
        (m.to_vec(), m.row_pitch())
    }

    /// Slot 0's pixels, through a staging copy.
    fn read_back(t: &Targets, ctx: &d3d::ID3D11DeviceContext, w: u32, h: u32) -> Vec<u8> {
        let Planes::Composed(slots) = &t.planes else {
            panic!("BGRA targets only")
        };
        let format = dxgi::DXGI_FORMAT_B8G8R8A8_UNORM;
        let (bytes, pitch) = staged(&t.dev, ctx, &slots[0], (w, h), format);
        bytes
            .chunks(pitch)
            .flat_map(|row| &row[..(w * 4) as usize])
            .copied()
            .collect()
    }

    /// A shader-readable `format` texture holding `px`, its rows packed.
    fn upload(
        dev: &d3d::ID3D11Device,
        (w, h): (u32, u32),
        format: dxgi::DXGI_FORMAT,
        px: &[u8],
    ) -> Tex {
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
            BindFlags: d3d::D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = d3d::D3D11_SUBRESOURCE_DATA {
            pSysMem: px.as_ptr().cast(),
            SysMemPitch: px.len() as u32 / h,
            SysMemSlicePitch: 0,
        };
        let mut t: Option<Tex> = None;
        // SAFETY: `desc`/`init` are locals describing `px`, which outlives the call; `t` is a
        // valid out-param checked by `expect`.
        unsafe {
            dev.CreateTexture2D(&desc, Some(&init), Some(&mut t))
                .expect("source texture");
        }
        t.expect("source texture")
    }

    fn source(dev: &d3d::ID3D11Device, w: u32, h: u32) -> Tex {
        let pixels: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, (i % 253) as u8, (i % 241) as u8, 0xFF])
            .collect();
        upload(dev, (w, h), dxgi::DXGI_FORMAT_B8G8R8A8_UNORM, &pixels)
    }

    /// Black, white and blue thirds in `kind`'s source format: BGRA, or FP16 at 0.0 and 1.0.
    fn bands(dev: &d3d::ID3D11Device, kind: InputKind, (w, h): (u32, u32)) -> Tex {
        let fp16 = source_format(kind) == dxgi::DXGI_FORMAT_R16G16B16A16_FLOAT;
        // f16 1.0 is 0x3C00; HDR's 125.0 (0x57D0) is 10000 nits, PQ's top code.
        let one: u16 = if kind == InputKind::P010 {
            0x57D0
        } else {
            0x3C00
        };
        let px: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let [r, g, b] = [[0u8, 0, 0], [1, 1, 1], [0, 0, 1]][(i % w * 3 / w) as usize];
                if fp16 {
                    [r, g, b, 1]
                        .map(|c| if c == 0 { 0u16 } else { one })
                        .map(|c| c.to_le_bytes())
                        .concat()
                } else {
                    vec![b * 255, g * 255, r * 255, 255]
                }
            })
            .collect();
        upload(dev, (w, h), source_format(kind), &px)
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
        let mut t = Targets::new(InputKind::Bgra, &dev, &ctx, (w, h), 1, true).expect("targets");
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
        let mut t = Targets::new(InputKind::Bgra, &dev, &ctx, (w, h), 1, true).expect("targets");
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

    /// A framed slot holds the source scaled to the encode size, and a pointer blended at
    /// source size lands at half its place. Flats wider than the kernel come through exactly.
    #[test]
    fn a_framed_slot_holds_the_scaled_picture() {
        let Some((dev, ctx)) = device() else {
            eprintln!("no D3D11 device (not even WARP) — skipping");
            return;
        };
        let framing = Framing {
            source: (64, 64),
            crop: [0, 0, 64, 64],
            encode: (32, 32),
        };
        let mut t = Targets::new(InputKind::Bgra, &dev, &ctx, (32, 32), 1, true)
            .and_then(|t| t.framed(&framing))
            .expect("framed targets");
        let dark = [0x20u8, 0x40, 0x80, 0xFF];
        let src = upload(
            &dev,
            (64, 64),
            dxgi::DXGI_FORMAT_B8G8R8A8_UNORM,
            &dark.repeat(64 * 64),
        );
        t.pass(&src, 0, false).expect("pass");
        let flat = read_back(&t, &ctx, 32, 32);
        assert!(
            flat.chunks(4).all(|p| p == dark),
            "a flat source scaled unevenly"
        );

        // Deferred, with the pointer at source (32, 32): output (20, 20) reads source 35..=47,
        // all pointer; output (4, 4) reads 3..=15, all desktop.
        t.pass(&src, 0, true).expect("deferred pass");
        t.frame(0, 0, Some((pointer(32, 32), 0.0))).expect("frame");
        let got = read_back(&t, &ctx, 32, 32);
        let at = |x: usize, y: usize| &got[(y * 32 + x) * 4..][..4];
        assert_eq!(
            at(20, 20),
            [0xFF; 4],
            "the pointer did not scale with the picture"
        );
        assert_eq!(at(4, 4), dark);
    }

    /// Each YUV converter writes the range it was built for. In 10-bit codes, black's Y, white's
    /// Y and blue's Cb are 0, 1023, 1023 at full range and 64, 940, 960 at studio. HDR checks
    /// luma only: BT.709 blue is not BT.2020 blue. The video-engine kinds need a video device,
    /// which WARP lacks.
    #[test]
    fn the_converters_write_the_advertised_range() {
        let Some((dev, ctx)) = device() else {
            eprintln!("no D3D11 device (not even WARP) — skipping");
            return;
        };
        let (w, h) = (96u32, 32u32);
        let video = dev.cast::<d3d::ID3D11VideoDevice>().is_ok();
        for kind in [
            InputKind::Nv12,
            InputKind::P010Sdr,
            InputKind::P010Wcg,
            InputKind::P010,
        ] {
            if matches!(kind, InputKind::Nv12 | InputKind::P010Sdr) && !video {
                eprintln!("{kind:?}: no video device — skipping");
                continue;
            }
            for full_range in [true, false] {
                let mut t = Targets::new(kind, &dev, &ctx, (w, h), 1, full_range).expect("targets");
                t.pass(&bands(&dev, kind, (w, h)), 0, false).expect("pass");
                let (tex, format) = match &t.planes {
                    Planes::Nv12 { out, .. } => (&out[0], dxgi::DXGI_FORMAT_NV12),
                    Planes::P010Sdr { out, .. } => (&out[0], dxgi::DXGI_FORMAT_P010),
                    Planes::P010 { out, .. } => (&out[0].0, dxgi::DXGI_FORMAT_P010),
                    _ => unreachable!("{kind:?} is a YUV kind"),
                };
                let (bytes, pitch) = staged(&dev, &ctx, tex, (w, h), format);
                // The sample at `row` and `col` as a 10-bit code; NV12's 8-bit ones widen by 4.
                let code = |row: u32, col: u32| -> u16 {
                    let at = row as usize * pitch;
                    if format == dxgi::DXGI_FORMAT_P010 {
                        let i = at + 2 * col as usize;
                        u16::from_le_bytes([bytes[i], bytes[i + 1]]) >> 6
                    } else {
                        u16::from(bytes[at + col as usize]) << 2
                    }
                };
                // Luma at mid-height; Cb is the even sample of a chroma row's pair, and 5w/6 is even.
                let got = [
                    code(h / 2, w / 6),
                    code(h / 2, w / 2),
                    code(h + h / 4, 5 * w / 6),
                ];
                let want = if full_range {
                    [0, 1023, 1023]
                } else {
                    [64, 940, 960]
                };
                let checked = if kind == InputKind::P010 { 2 } else { 3 };
                assert!(
                    got.iter()
                        .zip(want)
                        .take(checked)
                        .all(|(g, e)| g.abs_diff(e) <= 4),
                    "{kind:?} full_range {full_range}: {got:?}, want {want:?}"
                );
            }
        }
    }
}
