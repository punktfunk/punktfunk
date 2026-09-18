//! Intel **QSV** hardware encoder (Windows, D3D11 input). Native-VPL analogue of
//! [`super::amf`] and [`super::nvenc`].
//!
//! Drives the statically linked MIT VPL dispatcher (`libvpl-sys`): `MFXLoad` → hardware
//! filter → `MFXCreateSession` resolves the driver-store GPU runtime. Missing Intel
//! driver fails session creation and open falls through — same degrade as NVENC/AMF.
//! Feature `qsv` (cmake + libclang).
//!
//! Input is same-adapter D3D11 NV12/P010: `SetHandle` then GPU-side
//! `CopySubresourceRegion` into a runtime encode surface (`MFXMemory_GetSurfaceForEncode`).
//! No readback: Bgra/Rgb10a2 or CPU frames fail open/submit. HRD off so
//! `reconfigure_bitrate` is a no-IDR Reset. LTR-RFI is Query-gated per codec.
//! Evidence: `design/native-qsv-encoder.md`.

use super::policy::{intra_refresh_requested, ltr_test_force_at};
use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::retrieve::Ready;
use anyhow::{anyhow, bail, Context, Result};
use libvpl_sys as vpl;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::Interface;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;

fn sts_name(s: vpl::mfxStatus) -> &'static str {
    match s {
        vpl::MFX_ERR_NONE => "MFX_ERR_NONE",
        vpl::MFX_ERR_UNSUPPORTED => "MFX_ERR_UNSUPPORTED",
        vpl::MFX_ERR_MORE_DATA => "MFX_ERR_MORE_DATA",
        vpl::MFX_ERR_NOT_FOUND => "MFX_ERR_NOT_FOUND",
        vpl::MFX_ERR_DEVICE_LOST => "MFX_ERR_DEVICE_LOST",
        vpl::MFX_ERR_DEVICE_FAILED => "MFX_ERR_DEVICE_FAILED",
        vpl::MFX_ERR_GPU_HANG => "MFX_ERR_GPU_HANG",
        vpl::MFX_ERR_NOT_ENOUGH_BUFFER => "MFX_ERR_NOT_ENOUGH_BUFFER",
        vpl::MFX_ERR_MEMORY_ALLOC => "MFX_ERR_MEMORY_ALLOC",
        vpl::MFX_ERR_INCOMPATIBLE_VIDEO_PARAM => "MFX_ERR_INCOMPATIBLE_VIDEO_PARAM",
        vpl::MFX_ERR_INVALID_VIDEO_PARAM => "MFX_ERR_INVALID_VIDEO_PARAM",
        vpl::MFX_ERR_UNDEFINED_BEHAVIOR => "MFX_ERR_UNDEFINED_BEHAVIOR",
        vpl::MFX_ERR_NOT_INITIALIZED => "MFX_ERR_NOT_INITIALIZED",
        vpl::MFX_WRN_DEVICE_BUSY => "MFX_WRN_DEVICE_BUSY",
        vpl::MFX_WRN_IN_EXECUTION => "MFX_WRN_IN_EXECUTION",
        vpl::MFX_WRN_INCOMPATIBLE_VIDEO_PARAM => "MFX_WRN_INCOMPATIBLE_VIDEO_PARAM",
        vpl::MFX_WRN_PARTIAL_ACCELERATION => "MFX_WRN_PARTIAL_ACCELERATION",
        _ => "mfxStatus",
    }
}

fn vpl_ok(s: vpl::mfxStatus, what: &str) -> Result<()> {
    // VPL: positive is success-with-note (params corrected); only negatives fail.
    if s < vpl::MFX_ERR_NONE {
        bail!("{what} failed: {} ({s})", sts_name(s));
    }
    Ok(())
}

// Bindgen union accessors. Pin `__bindgen_anon_*` here so call sites read like C.

/// Encode-options view of `mfxInfoMFX` (the generated union is unreadable at call sites).
type EncOpts = vpl::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1;

fn mfx_of(par: &mut vpl::mfxVideoParam) -> &mut vpl::mfxInfoMFX {
    // SAFETY: `mfxVideoParam`'s anonymous union is `{ mfx, vpp }`; an encoder parameter block
    // only ever uses the `mfx` view, and all-zero bytes are a valid `mfxInfoMFX`.
    unsafe { &mut par.__bindgen_anon_1.mfx }
}

fn enc_of(mfx: &mut vpl::mfxInfoMFX) -> &mut EncOpts {
    // SAFETY: `mfxInfoMFX`'s anonymous union overlays encode/decode/JPEG option structs; an
    // encoder only ever uses the encode view, and all-zero bytes are valid for it.
    unsafe { &mut mfx.__bindgen_anon_1.__bindgen_anon_1 }
}

fn set_target_kbps(e: &mut EncOpts, kbps: u16) {
    e.__bindgen_anon_2 =
        vpl::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_2 { TargetKbps: kbps };
}

fn set_max_kbps(e: &mut EncOpts, kbps: u16) {
    e.__bindgen_anon_3 =
        vpl::mfxInfoMFX__bindgen_ty_1__bindgen_ty_1__bindgen_ty_3 { MaxKbps: kbps };
}

fn frame_wh(info: &mut vpl::mfxFrameInfo) -> &mut vpl::mfxFrameInfo__bindgen_ty_1__bindgen_ty_1 {
    // SAFETY: `mfxFrameInfo`'s anonymous union overlays the frame Width/Height/Crop view with
    // the buffer-size view; a video frame description uses the former, valid at all-zero.
    unsafe { &mut info.__bindgen_anon_1.__bindgen_anon_1 }
}

/// Split `bps` into `mfxU16` kbps × `BRCParamMultiplier` so rates above 65 Mbps still fit.
fn split_rate(bps: u64) -> (u16, u16) {
    let kbps = (bps / 1000).max(1);
    let mult = (kbps / (u16::MAX as u64) + 1) as u16;
    ((kbps / mult as u64) as u16, mult)
}

const fn align16(v: u32) -> u16 {
    (v.div_ceil(16) * 16) as u16
}

const NUM_LTR_SLOTS: usize = 2;

/// Defeat LTR-RFI (`PUNKTFUNK_NO_QSV_LTR`); loss recovery then always IDRs.
fn ltr_disabled() -> bool {
    crate::knobs::get().no_qsv_ltr != 0
}

/// Frames between LTR marks. Default ~1/4 s so a loss usually finds a recent slot.
fn ltr_mark_interval(fps: u32) -> i64 {
    super::policy::ltr_interval().unwrap_or_else(|| (fps as i64 / 4).max(1))
}

/// Intra-refresh wave period, clamped to the `mfxU16` field's useful 8..=240.
fn intra_refresh_period(fps: u32) -> u16 {
    super::policy::intra_refresh_period(fps).clamp(8, 240) as u16
}

struct Loader(vpl::mfxLoader);
impl Drop for Loader {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `MFXLoad` and is dropped exactly once.
            unsafe { vpl::MFXUnload(self.0) };
        }
    }
}

/// Owned `mfxSession`. `MFXClose` also tears down an encoder still open on it.
struct Session(vpl::mfxSession);
impl Drop for Session {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` came from a successful `MFXCreateSession` and is dropped exactly
            // once; MFXClose tears down any component still initialised on it.
            unsafe { vpl::MFXClose(self.0) };
        }
    }
}

/// Dispatcher hardware impl: `MFXCreateSession` index plus adapter LUID (zero if unknown).
struct VplImpl {
    index: u32,
    luid: [u8; 8],
    luid_valid: bool,
}

/// Loader filtered to Intel hardware. Dispatcher contract: one property per `mfxConfig`.
fn intel_loader() -> Result<(Loader, Vec<VplImpl>)> {
    // SAFETY: plain dispatcher C calls on handles owned by this function. Each config handle is
    // owned by the loader (released by MFXUnload); the `mfxVariant`s are by-value. The
    // enumeration loop releases every description handle it obtained via
    // `MFXDispReleaseImplDescription` before returning.
    unsafe {
        let loader = Loader(vpl::MFXLoad());
        if loader.0.is_null() {
            bail!("MFXLoad returned null (dispatcher out of memory?)");
        }
        for (name, value) in [
            (
                b"mfxImplDescription.Impl\0".as_slice(),
                vpl::MFX_IMPL_TYPE_HARDWARE as u32,
            ),
            (b"mfxImplDescription.VendorID\0".as_slice(), 0x8086u32),
        ] {
            let cfg = vpl::MFXCreateConfig(loader.0);
            if cfg.is_null() {
                bail!("MFXCreateConfig returned null");
            }
            let mut var: vpl::mfxVariant = std::mem::zeroed();
            var.Type = vpl::MFX_VARIANT_TYPE_U32;
            var.Data = vpl::mfxVariant_data { U32: value };
            vpl_ok(
                vpl::MFXSetConfigFilterProperty(cfg, name.as_ptr(), var),
                "MFXSetConfigFilterProperty",
            )?;
        }
        let mut impls = Vec::new();
        for i in 0u32.. {
            let mut hdl: vpl::mfxHDL = ptr::null_mut();
            let sts = vpl::MFXEnumImplementations(
                loader.0,
                i,
                vpl::MFX_IMPLCAPS_DEVICE_ID_EXTENDED,
                &mut hdl,
            );
            if sts != vpl::MFX_ERR_NONE || hdl.is_null() {
                break; // MFX_ERR_NOT_FOUND past the last impl
            }
            let dev = &*(hdl as *const vpl::mfxExtendedDeviceId);
            impls.push(VplImpl {
                index: i,
                luid: dev.DeviceLUID,
                luid_valid: dev.LUIDValid != 0,
            });
            let _ = vpl::MFXDispReleaseImplDescription(loader.0, hdl);
        }
        Ok((loader, impls))
    }
}

/// DXGI adapter LUID in the little-endian layout `mfxExtendedDeviceId::DeviceLUID` uses.
fn device_luid(device: &ID3D11Device) -> Option<[u8; 8]> {
    // SAFETY: standard COM navigation on a live device; every interface is an owned
    // windows-rs wrapper released on drop, and `GetDesc` fills a plain out-struct.
    unsafe {
        let dxgi: IDXGIDevice = device.cast().ok()?;
        let desc = dxgi.GetAdapter().ok()?.GetDesc().ok()?;
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
        b[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
        Some(b)
    }
}

/// Session on the capture adapter. `None` / no match picks the first Intel impl — only
/// correct when the box has exactly one.
fn create_session(target_luid: Option<[u8; 8]>) -> Result<(Loader, Session, (u16, u16))> {
    let (loader, impls) = intel_loader()?;
    if impls.is_empty() {
        bail!("no Intel hardware VPL implementation (no Intel GPU/driver on this box?)");
    }
    let chosen = target_luid
        .and_then(|want| impls.iter().find(|i| i.luid_valid && i.luid == want))
        .unwrap_or(&impls[0]);
    if let Some(want) = target_luid {
        if !(chosen.luid_valid && chosen.luid == want) {
            // Capture adapter is not Intel VPL. Terminal so the stream loop ends
            // instead of burning the reset budget on the same mismatch.
            return Err(anyhow::Error::new(super::TerminalEncoderError).context(
                "capture device's adapter is not an Intel VPL implementation (hybrid box? \
                     point PUNKTFUNK_RENDER_ADAPTER / the web-console GPU preference at the \
                     Intel adapter for a QSV session)",
            ));
        }
    }
    // SAFETY: `loader.0` is live; `MFXCreateSession` fills `session` only on success and the
    // guard closes it exactly once. `MFXQueryVersion` fills a plain out-struct on the live
    // session.
    unsafe {
        let mut session: vpl::mfxSession = ptr::null_mut();
        vpl_ok(
            vpl::MFXCreateSession(loader.0, chosen.index, &mut session),
            "MFXCreateSession",
        )?;
        let session = Session(session);
        let mut ver: vpl::mfxVersion = std::mem::zeroed();
        let _ = vpl::MFXQueryVersion(session.0, &mut ver);
        let api = (ver.__bindgen_anon_1.Major, ver.__bindgen_anon_1.Minor);
        Ok((loader, session, api))
    }
}

/// `NumRefFrame`: 2 short-term + the 2 LTR slots.
const NUM_REF_FRAMES: u16 = 4;

/// `mfxVideoParam` plus the ext-buffer storage `ExtParam` points into (must outlive the call).
struct ParamSet {
    par: vpl::mfxVideoParam,
    co: Box<vpl::mfxExtCodingOption>,
    co2: Option<Box<vpl::mfxExtCodingOption2>>,
    vsi: Option<Box<vpl::mfxExtVideoSignalInfo>>,
    mastering: Option<Box<vpl::mfxExtMasteringDisplayColourVolume>>,
    cll: Option<Box<vpl::mfxExtContentLightLevelInfo>>,
    ptrs: Vec<*mut vpl::mfxExtBuffer>,
}

impl ParamSet {
    /// Rebuild `ExtParam` from the live boxes. Call after any buffer is added or dropped.
    fn seal(&mut self) {
        self.ptrs.clear();
        self.ptrs
            .push(&mut self.co.Header as *mut vpl::mfxExtBuffer);
        if let Some(b) = self.co2.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        if let Some(b) = self.vsi.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        if let Some(b) = self.mastering.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        if let Some(b) = self.cll.as_mut() {
            self.ptrs.push(&mut b.Header as *mut vpl::mfxExtBuffer);
        }
        self.par.ExtParam = self.ptrs.as_mut_ptr();
        self.par.NumExtParam = self.ptrs.len() as u16;
    }
}

struct EncodeConfig {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// CO2 intra-refresh wave instead of LTR (mutually exclusive).
    intra_refresh: bool,
    hdr_meta: Option<pf_frame::HdrMeta>,
}

fn codec_id(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => vpl::MFX_CODEC_AVC as u32,
        Codec::H265 => vpl::MFX_CODEC_HEVC as u32,
        Codec::Av1 => vpl::MFX_CODEC_AV1 as u32,
        Codec::PyroWave => unreachable!("PyroWave never opens the QSV backend"),
    }
}

/// Low-latency block: `AsyncDepth=1`, no B-frames, CBR, HRD off (no-IDR Reset), infinite GOP.
fn build_params(cfg: &EncodeConfig) -> ParamSet {
    // SAFETY: all-zero is the documented initial state for every VPL parameter struct; fields
    // are then set through the typed accessors.
    let mut par: vpl::mfxVideoParam = unsafe { std::mem::zeroed() };
    par.AsyncDepth = 1;
    par.IOPattern = vpl::MFX_IOPATTERN_IN_VIDEO_MEMORY as u16;
    let mfx = mfx_of(&mut par);
    mfx.CodecId = codec_id(cfg.codec);
    mfx.LowPower = vpl::MFX_CODINGOPTION_ON as u16;
    mfx.CodecProfile = match (cfg.codec, cfg.ten_bit) {
        (Codec::H264, _) => vpl::MFX_PROFILE_AVC_HIGH as u16,
        (Codec::H265, false) => vpl::MFX_PROFILE_HEVC_MAIN as u16,
        (Codec::H265, true) => vpl::MFX_PROFILE_HEVC_MAIN10 as u16,
        (Codec::Av1, _) => vpl::MFX_PROFILE_AV1_MAIN as u16,
        (Codec::PyroWave, _) => unreachable!("PyroWave never opens the QSV backend"),
    };
    let (kbps, mult) = split_rate(cfg.bitrate_bps);
    mfx.BRCParamMultiplier = mult;
    {
        let e = enc_of(mfx);
        e.TargetUsage = vpl::MFX_TARGETUSAGE_BEST_SPEED as u16;
        e.GopPicSize = u16::MAX; // infinite GOP — IDR on demand only
        e.GopRefDist = 1; // no B-frames (latency + FIFO pairing)
        e.IdrInterval = 0;
        e.RateControlMethod = vpl::MFX_RATECONTROL_CBR as u16;
        e.NumRefFrame = NUM_REF_FRAMES;
        set_target_kbps(e, kbps);
        set_max_kbps(e, kbps);
    }
    let info = &mut mfx.FrameInfo;
    info.FourCC = if cfg.ten_bit {
        vpl::MFX_FOURCC_P010 as u32
    } else {
        vpl::MFX_FOURCC_NV12 as u32
    };
    if cfg.ten_bit {
        info.BitDepthLuma = 10;
        info.BitDepthChroma = 10;
        info.Shift = 1; // P010 is MSB-aligned
    }
    info.ChromaFormat = vpl::MFX_CHROMAFORMAT_YUV420 as u16;
    info.PicStruct = vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
    info.FrameRateExtN = cfg.fps.max(1);
    info.FrameRateExtD = 1;
    {
        let wh = frame_wh(info);
        wh.Width = align16(cfg.width);
        wh.Height = align16(cfg.height);
        wh.CropW = cfg.width as u16;
        wh.CropH = cfg.height as u16;
    }

    // HRD off: spec prerequisite for a bitrate Reset that does not emit a keyframe.
    // SAFETY: all-zero is valid for every ext buffer; the header is then stamped.
    let mut co: Box<vpl::mfxExtCodingOption> = Box::new(unsafe { std::mem::zeroed() });
    co.Header.BufferId = vpl::MFX_EXTBUFF_CODING_OPTION as u32;
    co.Header.BufferSz = std::mem::size_of::<vpl::mfxExtCodingOption>() as u32;
    co.NalHrdConformance = vpl::MFX_CODINGOPTION_OFF as u16;
    co.VuiNalHrdParameters = vpl::MFX_CODINGOPTION_OFF as u16;
    co.MaxDecFrameBuffering = NUM_REF_FRAMES;

    // Intra-refresh: AVC/HEVC only — AV1 has no IntRefType.
    let co2 = (cfg.intra_refresh && matches!(cfg.codec, Codec::H264 | Codec::H265)).then(|| {
        // SAFETY: all-zero is valid; header stamped below.
        let mut b: Box<vpl::mfxExtCodingOption2> = Box::new(unsafe { std::mem::zeroed() });
        b.Header.BufferId = vpl::MFX_EXTBUFF_CODING_OPTION2 as u32;
        b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtCodingOption2>() as u32;
        b.IntRefType = 1; // vertical wave
        b.IntRefCycleSize = intra_refresh_period(cfg.fps);
        b
    });

    // Colour signalling is unconditional: capture already CSC'd to BT.709 limited or
    // BT.2020 PQ. An "unspecified" stream lets decoders pick 601 at sub-HD.
    let hdr = cfg.ten_bit && cfg.codec != Codec::H264;
    let vsi = {
        // SAFETY: all-zero is valid; header stamped below.
        let mut b: Box<vpl::mfxExtVideoSignalInfo> = Box::new(unsafe { std::mem::zeroed() });
        b.Header.BufferId = vpl::MFX_EXTBUFF_VIDEO_SIGNAL_INFO as u32;
        b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtVideoSignalInfo>() as u32;
        b.VideoFormat = 5; // unspecified
        b.VideoFullRange = 0;
        b.ColourDescriptionPresent = 1;
        if hdr {
            b.ColourPrimaries = 9; // BT.2020
            b.TransferCharacteristics = 16; // SMPTE ST 2084 (PQ)
            b.MatrixCoefficients = 9; // BT.2020 non-constant
        } else {
            b.ColourPrimaries = 1; // BT.709
            b.TransferCharacteristics = 1; // BT.709
            b.MatrixCoefficients = 1; // BT.709
        }
        Some(b)
    };
    let mastering = cfg.hdr_meta.filter(|_| hdr).map(|m| {
        // SAFETY: all-zero is valid; header stamped below.
        let mut b: Box<vpl::mfxExtMasteringDisplayColourVolume> =
            Box::new(unsafe { std::mem::zeroed() });
        b.Header.BufferId = vpl::MFX_EXTBUFF_MASTERING_DISPLAY_COLOUR_VOLUME as u32;
        b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtMasteringDisplayColourVolume>() as u32;
        b.InsertPayloadToggle = vpl::MFX_PAYLOAD_IDR as u16;
        // Encode copies the fields into the bitstream in that codec's semantics: HEVC Annex D
        // (G,B,R, 1/50000, both luminances 0.0001 cd/m², as HdrMeta) or AV1 6.7.4.
        if cfg.codec == Codec::Av1 {
            let a = pf_frame::hdr::av1_mdcv(&m);
            for (i, [x, y]) in a.primaries.into_iter().enumerate() {
                b.DisplayPrimariesX[i] = x;
                b.DisplayPrimariesY[i] = y;
            }
            [b.WhitePointX, b.WhitePointY] = a.white_point;
            b.MaxDisplayMasteringLuminance = a.luminance_max;
            b.MinDisplayMasteringLuminance = a.luminance_min;
        } else {
            for (i, p) in m.display_primaries.iter().enumerate() {
                b.DisplayPrimariesX[i] = p[0];
                b.DisplayPrimariesY[i] = p[1];
            }
            b.WhitePointX = m.white_point[0];
            b.WhitePointY = m.white_point[1];
            b.MaxDisplayMasteringLuminance = m.max_display_mastering_luminance;
            b.MinDisplayMasteringLuminance = m.min_display_mastering_luminance;
        }
        b
    });
    let cll = cfg
        .hdr_meta
        .filter(|_| hdr)
        .filter(|m| m.max_cll != 0 || m.max_fall != 0)
        .map(|m| {
            // SAFETY: all-zero is valid; header stamped below.
            let mut b: Box<vpl::mfxExtContentLightLevelInfo> =
                Box::new(unsafe { std::mem::zeroed() });
            b.Header.BufferId = vpl::MFX_EXTBUFF_CONTENT_LIGHT_LEVEL_INFO as u32;
            b.Header.BufferSz = std::mem::size_of::<vpl::mfxExtContentLightLevelInfo>() as u32;
            b.InsertPayloadToggle = vpl::MFX_PAYLOAD_IDR as u16;
            b.MaxContentLightLevel = m.max_cll;
            b.MaxPicAverageLightLevel = m.max_fall;
            b
        });

    let mut set = ParamSet {
        par,
        co,
        co2,
        vsi,
        mastering,
        cll,
        ptrs: Vec::new(),
    };
    set.seal();
    set
}

/// Idle `mfxExtRefListCtrl`: every `FrameOrder` is `MFX_FRAMEORDER_UNKNOWN`.
fn empty_reflist() -> vpl::mfxExtRefListCtrl {
    // SAFETY: all-zero is a valid `mfxExtRefListCtrl`; the header + sentinel FrameOrders are
    // stamped before use.
    let mut r: vpl::mfxExtRefListCtrl = unsafe { std::mem::zeroed() };
    r.Header.BufferId = vpl::MFX_EXTBUFF_UNIVERSAL_REFLIST_CTRL as u32;
    r.Header.BufferSz = std::mem::size_of::<vpl::mfxExtRefListCtrl>() as u32;
    let unknown = vpl::MFX_FRAMEORDER_UNKNOWN as u32;
    for e in r.PreferredRefList.iter_mut() {
        e.FrameOrder = unknown;
    }
    for e in r.RejectedRefList.iter_mut() {
        e.FrameOrder = unknown;
    }
    for e in r.LongTermRefList.iter_mut() {
        e.FrameOrder = unknown;
    }
    r
}

/// Per-frame `mfxEncodeCtrl` plus ext buffers. EncodeFrameAsync copies the ctrl, not the
/// buffers — this box stays in the in-flight FIFO until the sync point completes.
struct FrameCtrl {
    ctrl: vpl::mfxEncodeCtrl,
    reflist: vpl::mfxExtRefListCtrl,
    ptrs: [*mut vpl::mfxExtBuffer; 1],
}

impl FrameCtrl {
    fn new() -> Box<Self> {
        // SAFETY: all-zero is valid for `mfxEncodeCtrl` (no ext buffers attached, no forced
        // type); the reflist starts as the sentinel idle state and the pointer array is wired
        // only when the reflist is actually used.
        let ctrl: vpl::mfxEncodeCtrl = unsafe { std::mem::zeroed() };
        let mut b = Box::new(FrameCtrl {
            ctrl,
            reflist: empty_reflist(),
            ptrs: [ptr::null_mut()],
        });
        b.ptrs[0] = &mut b.reflist.Header as *mut vpl::mfxExtBuffer;
        b
    }

    fn attach_reflist(&mut self) {
        self.ctrl.ExtParam = self.ptrs.as_mut_ptr();
        self.ctrl.NumExtParam = 1;
    }
}

/// In-flight frame. `_ctrl` keeps per-frame ext buffers alive until the sync point completes.
struct Pending {
    syncp: vpl::mfxSyncPoint,
    bs: Box<BsBuf>,
    pts_ns: u64,
    forced: bool,
    recovery_anchor: bool,
    _ctrl: Option<Box<FrameCtrl>>,
}

/// Output bitstream. Boxed so `Data` stays stable while the runtime writes asynchronously.
struct BsBuf {
    buf: Vec<u8>,
    mfx: vpl::mfxBitstream,
}

impl BsBuf {
    fn new(capacity: usize) -> Box<Self> {
        // SAFETY: all-zero is a valid `mfxBitstream`; Data/MaxLength are wired below.
        let mfx: vpl::mfxBitstream = unsafe { std::mem::zeroed() };
        let mut b = Box::new(BsBuf {
            buf: vec![0u8; capacity],
            mfx,
        });
        b.mfx.Data = b.buf.as_mut_ptr();
        b.mfx.MaxLength = capacity as u32;
        b
    }

    fn recycle(&mut self) {
        self.mfx.DataOffset = 0;
        self.mfx.DataLength = 0;
    }
}

/// Cap on in-flight frames. Steady state is 1 (`AsyncDepth=1`); this only bounds a stall.
const IN_FLIGHT_MAX: usize = 4;

/// Drain budget for `DEVICE_BUSY` / a full in-flight window. One frame's encode time, far
/// under the session watchdog's ~2 s floor.
const BUSY_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

/// What the retrieve thread and the encode thread share. The session is not in here: only this
/// thread calls `SyncOperation` on it, and the lock is never held across that call.
#[derive(Default)]
struct Out {
    /// In-flight FIFO. `GopRefDist=1` so AUs complete in submit order — `submit` pushes the back,
    /// the retrieve thread pops the front, and nothing else touches either end.
    pending: VecDeque<Pending>,
    ready: VecDeque<EncodedFrame>,
    /// Recycled bitstream boxes. `mfxBitstream` address must stay stable while in flight.
    #[allow(clippy::vec_box)]
    bs_pool: Vec<Box<BsBuf>>,
    /// First `SyncOperation` failure; `poll` surfaces it so the caller resets.
    err: Option<String>,
}

// SAFETY: `Pending` carries raw VPL allocations — a sync point, the boxed bitstream the runtime
// writes into, and the frame-control ext buffers. None is thread-affine (they are process-global
// runtime memory, like the session itself, which is why `QsvEncoder` is `Send` at all), and the
// mutex is what gives exactly one thread access at a time: `submit` pushes an entry and never
// looks at it again, and only the sync thread pops one.
unsafe impl Send for Out {}

/// The sync thread and its signal. It owns every `MFXVideoCORE_SyncOperation`, so the encode
/// thread never blocks on the fixed function — it takes finished AUs off a queue, and a caller
/// that parks on handles takes [`Ready`] instead.
///
/// Dropping this stops and joins, which must happen before the session is closed under it:
/// [`Inner`] declares it first, and [`QsvEncoder::reset`] stops it by hand around the re-Init.
struct Retrieve {
    out: Arc<Mutex<Out>>,
    have: Arc<Ready>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Retrieve {
    fn start(session: vpl::mfxSession) -> Result<Self> {
        let out: Arc<Mutex<Out>> = Arc::default();
        let have = Arc::new(Ready::new().ok_or_else(|| anyhow!("QSV: no completion event"))?);
        let stop = Arc::new(AtomicBool::new(false));
        let (s, t_out, t_have, t_stop) =
            (session as usize, out.clone(), have.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("punktfunk-qsv-out".into())
            .spawn(move || sync_loop(s, t_out, t_have, t_stop))
            .context("spawn QSV sync thread")?;
        Ok(Self {
            out,
            have,
            stop,
            join: Some(join),
        })
    }

    /// Frames the runtime still owes an AU for — the back-pressure reading.
    fn in_flight(&self) -> usize {
        lock(&self.out).pending.len()
    }

    /// Retire the thread and wait for it to leave `SyncOperation`. Idempotent.
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// Forfeit everything owed. Only legal after `Close`, which aborts the writes the runtime
    /// still has outstanding into these buffers.
    fn reset_queues(&self) {
        let mut g = lock(&self.out);
        g.pending.clear();
        g.ready.clear();
        g.err = None;
        self.have.clear();
    }
}

impl Drop for Retrieve {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// How long one `SyncOperation` may block before the loop looks at the stop flag.
const SYNC_WAIT_MS: u32 = 50;

/// Sync finished frames and hand their AUs to the encode thread. The session travels as `usize`;
/// the thread is joined before anything closes it.
fn sync_loop(session: usize, out: Arc<Mutex<Out>>, have: Arc<Ready>, stop: Arc<AtomicBool>) {
    pf_frame::thread_qos::boost_thread_priority(false);
    let session = session as vpl::mfxSession;
    while !stop.load(Ordering::Acquire) {
        // The front's sync point, copied out so the lock is not held across the wait. Only this
        // thread pops, so the entry it names is still the front when the lock comes back.
        let Some(syncp) = lock(&out).pending.front().map(|p| p.syncp) else {
            // Nothing owed: wait for `submit` rather than spin on an empty queue.
            std::thread::sleep(std::time::Duration::from_micros(250));
            continue;
        };
        // SAFETY: `session` is live until this thread is joined, and `syncp` belongs to an
        // operation submitted on it that has not been synced (entries leave `pending` once,
        // below, on this thread alone).
        let sts = unsafe { vpl::MFXVideoCORE_SyncOperation(session, syncp, SYNC_WAIT_MS) };
        if sts == vpl::MFX_WRN_IN_EXECUTION {
            continue;
        }
        let mut g = lock(&out);
        if sts < vpl::MFX_ERR_NONE {
            g.err.get_or_insert_with(|| {
                format!(
                    "MFXVideoCORE_SyncOperation failed: {} ({sts})",
                    sts_name(sts)
                )
            });
            have.set();
            return;
        }
        let done = match g.pending.pop_front() {
            Some(d) => d,
            None => continue,
        };
        match au_from(done) {
            Ok((au, bs)) => {
                g.bs_pool.push(bs);
                g.ready.push_back(au);
                // Under the lock, so it cannot race the clear `poll` does when it empties.
                have.set();
            }
            Err(e) => {
                g.err.get_or_insert_with(|| format!("{e:#}"));
                have.set();
                return;
            }
        }
    }
}

/// A synced [`Pending`] as its access unit plus the bitstream box to recycle.
fn au_from(done: Pending) -> Result<(EncodedFrame, Box<BsBuf>)> {
    let bs = &done.bs.mfx;
    let off = bs.DataOffset as usize;
    let len = bs.DataLength as usize;
    if off + len > done.bs.buf.len() {
        bail!(
            "QSV bitstream out of bounds: offset {off} + length {len} > buffer {}",
            done.bs.buf.len()
        );
    }
    let data = done.bs.buf[off..off + len].to_vec();
    let key_flag =
        bs.FrameType & (vpl::MFX_FRAMETYPE_IDR as u16 | vpl::MFX_FRAMETYPE_I as u16) != 0;
    let au = EncodedFrame {
        data,
        pts_ns: done.pts_ns,
        keyframe: key_flag || done.forced,
        recovery_anchor: done.recovery_anchor,
        recovery_point: false,
        recovery_close: false,
        chunk_aligned: false,
    };
    let mut bs_box = done.bs;
    bs_box.recycle();
    Ok((au, bs_box))
}

/// The queue lock, poison-tolerant: a sync thread that panicked leaves what it already handed
/// over readable, and its error field is what tells `poll` to reset.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Inner {
    /// Joined before `session` closes under it ([`Inner::drop`]).
    retrieve: Retrieve,
    /// Session must Close before the loader unloads the runtime (declaration drop order).
    session: Session,
    _loader: Loader,
    _device: ID3D11Device,
    dctx: ID3D11DeviceContext,
    bs_bytes: usize,
    frames_submitted: u64,
    first_au_logged: bool,
    /// Warn once if the runtime hands out array textures (subresource-0 copy would be wrong).
    array_warned: bool,
}

impl Drop for Inner {
    /// Join, then Close, then the fields. Every `pending` entry owns a bitstream the runtime
    /// may still be writing, and Close is what aborts those writes; field order alone freed
    /// the boxes under the runtime whenever a session ended with a frame in flight.
    fn drop(&mut self) {
        self.retrieve.stop_and_join();
        // SAFETY: the session is live and its sync thread joined; Close on an encoder in any
        // state is legal and its result carries nothing here.
        unsafe {
            let _ = vpl::MFXVideoENCODE_Close(self.session.0);
        }
    }
}

impl Inner {
    fn note_first_au(&mut self, au: &EncodedFrame) {
        if !self.first_au_logged {
            self.first_au_logged = true;
            tracing::info!(
                bytes = au.data.len(),
                keyframe = au.keyframe,
                "QSV produced its first AU on this session"
            );
        }
    }

    fn take_bs(&mut self) -> Box<BsBuf> {
        // A bitrate retarget can raise worst-case AU size; drop pooled buffers that are now short.
        let mut g = lock(&self.retrieve.out);
        while let Some(mut b) = g.bs_pool.pop() {
            if b.mfx.MaxLength as usize >= self.bs_bytes {
                b.recycle();
                return b;
            }
        }
        BsBuf::new(self.bs_bytes)
    }

    /// The oldest finished AU, or the sync thread's failure. Clears the signal as the queue
    /// empties — under the same lock the thread sets it under, so the two cannot cross.
    fn pop_ready(&mut self) -> Result<Option<EncodedFrame>> {
        let mut g = lock(&self.retrieve.out);
        if let Some(e) = g.err.take() {
            bail!("{e}");
        }
        let au = g.ready.pop_front();
        if g.ready.is_empty() {
            self.retrieve.have.clear();
        }
        Ok(au)
    }

    /// [`Self::pop_ready`], waiting up to `wait_ms` for the thread to produce one.
    fn take_ready(&mut self, wait_ms: u32) -> Result<Option<EncodedFrame>> {
        if let Some(au) = self.pop_ready()? {
            return Ok(Some(au));
        }
        if !self.retrieve.have.wait(wait_ms) {
            return Ok(None);
        }
        self.pop_ready()
    }
}

pub struct QsvEncoder {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// Lazy from the first frame's device; rebuilt on capturer-device change.
    inner: Option<Inner>,
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    hdr_meta: Option<pf_frame::HdrMeta>,
    /// HDR metadata baked at Init. A post-Init change re-Inits so in-band SEI/OBU refreshes.
    hdr_applied: Option<pf_frame::HdrMeta>,
    /// Driver accepted intra-refresh — gates [`EncoderCaps::intra_refresh`].
    ir_active: bool,
    /// `mfxExtRefListCtrl` passed the per-codec Query gate — gates [`EncoderCaps::supports_rfi`].
    ltr_active: bool,
    /// Wire index in each LTR slot (`None` = never marked). Mirrors the hardware DPB:
    /// clearing an entry issues no VPL call, so the encoder keeps the frame long-term.
    /// Distrust lives in `ltr_tainted` so RejectedRefList can still name the slot.
    ltr_slots: [Option<i64>; NUM_LTR_SLOTS],
    /// Mark is live in the DPB but sat inside the client's corrupt window — reject, don't force.
    /// Cleared on re-mark or IDR flush.
    ltr_tainted: [bool; NUM_LTR_SLOTS],
    next_ltr_slot: usize,
    ltr_mark_interval: i64,
    pending_force: Option<usize>,
    ltr_test_force_at: Option<i64>,
    /// Refuse this frame after the LTR decision, as a failed surface fetch would.
    #[cfg(test)]
    fail_submit_at: Option<i64>,
    /// Resets with no AU since. At 2, drop `inner` instead of Close+Init on a dead session.
    resets_without_output: u32,
}

// SAFETY: raw VPL and D3D11 handles are not auto-`Send`. The session moves the encoder onto
// one encode thread and drives it there; the immediate context is never shared.
unsafe impl Send for QsvEncoder {}

impl QsvEncoder {
    /// Open native QSV. Fails when there is no Intel VPL impl, the codec probe declines, or
    /// capture is not NV12/P010.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
        // Selected render adapter (`None` = first Intel VPL implementation); the AV1 probe
        // queries it. The session itself binds to the capture device's adapter at bring-up.
        adapter_luid: Option<LUID>,
    ) -> Result<Self> {
        if codec == Codec::PyroWave {
            bail!("PyroWave never opens the QSV backend");
        }
        // AV1 is DG2/Arc + MTL+ — probe here so an older box fails at open, not at lazy Init.
        if codec == Codec::Av1 && !probe_can_encode(Codec::Av1, adapter_luid) {
            bail!("this GPU/driver declined AV1 encode (DG2/Arc or MTL+ required) — QSV probe");
        }
        // Depth follows delivered pixels, not negotiated depth ([`crate::ten_bit_input`]).
        let ten_bit = crate::ten_bit_input(format, bit_depth);
        if ten_bit && codec == Codec::H264 {
            bail!("native QSV: 10-bit is HEVC/AV1-only (H.264 High10 is not negotiated)");
        }
        let expected = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        if format != expected {
            bail!(
                "native QSV needs the video-processor {expected:?} capture path; capturer \
                 delivered {format:?} (no readback path by design — zero-copy invariant)"
            );
        }
        if chroma.is_444() {
            tracing::warn!("QSV 4:4:4 is not probed/wired yet — encoding 4:2:0");
        }
        Ok(QsvEncoder {
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            ten_bit,
            inner: None,
            bound_device: 0,
            frame_idx: 0,
            force_kf: false,
            hdr_meta: None,
            hdr_applied: None,
            ir_active: false,
            ltr_active: false,
            ltr_slots: [None; NUM_LTR_SLOTS],
            ltr_tainted: [false; NUM_LTR_SLOTS],
            next_ltr_slot: 0,
            ltr_mark_interval: ltr_mark_interval(fps),
            pending_force: None,
            ltr_test_force_at: ltr_test_force_at(),
            #[cfg(test)]
            fail_submit_at: None,
            resets_without_output: 0,
        })
    }

    fn ltr_wanted(&self) -> bool {
        !ltr_disabled() && !intra_refresh_requested()
    }

    fn encode_config(&self) -> EncodeConfig {
        EncodeConfig {
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_bps: self.bitrate_bps,
            ten_bit: self.ten_bit,
            intra_refresh: intra_refresh_requested(),
            hdr_meta: self.hdr_meta,
        }
    }

    /// Query-gate LTR/IR, Init, then size the bitstream pool from `BufferSizeInKB`.
    fn init_encode(&self, session: vpl::mfxSession) -> Result<(bool, bool, usize)> {
        let cfg = self.encode_config();
        let mut set = build_params(&cfg);
        // Query-gate mfxExtRefListCtrl: AVC/HEVC are spec'd; AV1 is runtime-only and
        // spec-silent. Query mutates nothing on the live encoder.
        let ltr_active = self.ltr_wanted() && {
            let mut q_in = build_params(&cfg);
            let mut q_out = build_params(&cfg);
            let mut rl_in = Box::new(empty_reflist());
            let mut rl_out = Box::new(empty_reflist());
            q_in.ptrs.push(&mut rl_in.Header as *mut vpl::mfxExtBuffer);
            q_in.par.ExtParam = q_in.ptrs.as_mut_ptr();
            q_in.par.NumExtParam = q_in.ptrs.len() as u16;
            q_out
                .ptrs
                .push(&mut rl_out.Header as *mut vpl::mfxExtBuffer);
            q_out.par.ExtParam = q_out.ptrs.as_mut_ptr();
            q_out.par.NumExtParam = q_out.ptrs.len() as u16;
            // SAFETY: `session` is live; both param blocks and their ext chains outlive the
            // synchronous call (owned locals above).
            let sts = unsafe { vpl::MFXVideoENCODE_Query(session, &mut q_in.par, &mut q_out.par) };
            let ok = sts >= vpl::MFX_ERR_NONE;
            if !ok {
                tracing::info!(
                    codec = ?self.codec,
                    status = sts_name(sts),
                    "QSV declined mfxExtRefListCtrl — loss recovery falls back to IDR"
                );
            }
            ok
        };
        // Warnings (corrected params, partial acceleration) are logged but accepted.
        // SAFETY: `session` is live; `set` (params + ext chain) outlives the synchronous call.
        let sts = unsafe { vpl::MFXVideoENCODE_Init(session, &mut set.par) };
        vpl_ok(sts, "MFXVideoENCODE_Init")?;
        if sts > vpl::MFX_ERR_NONE {
            tracing::debug!(status = sts_name(sts), "QSV Init returned a warning");
        }
        // Asked-for, not installed — confirmed by GetVideoParam below.
        let ir_requested = cfg.intra_refresh && set.co2.is_some();
        // SAFETY: `session` is live; `got` and its (empty) ext chain outlive the call.
        let bs_bytes = unsafe {
            let mut got: vpl::mfxVideoParam = std::mem::zeroed();
            vpl_ok(
                vpl::MFXVideoENCODE_GetVideoParam(session, &mut got),
                "MFXVideoENCODE_GetVideoParam",
            )?;
            let m = &mut got.__bindgen_anon_1.mfx;
            let mult = m.BRCParamMultiplier.max(1) as usize;
            let kb = enc_of(m).BufferSizeInKB as usize;
            (kb * mult * 1000).max(256 * 1024)
        };
        // Query/Init can warn INCOMPATIBLE_VIDEO_PARAM and still drop the wave
        // (`IntRefType=0`). `ir_active` feeds `EncoderCaps::intra_refresh`; a false
        // true stops the client asking for IDRs. Separate GetVideoParam so a dislike
        // of CO2 cannot take down the BufferSizeInKB read.
        let ir_active = if ir_requested {
            // SAFETY: `session` is live on this thread; `got` and `co2_out` (with `ptrs` holding the
            // only reference to it) all outlive the synchronous call, and the runtime writes back
            // only into the buffer whose header we stamped.
            let confirmed = unsafe {
                let mut got: vpl::mfxVideoParam = std::mem::zeroed();
                let mut co2_out: vpl::mfxExtCodingOption2 = std::mem::zeroed();
                co2_out.Header.BufferId = vpl::MFX_EXTBUFF_CODING_OPTION2 as u32;
                co2_out.Header.BufferSz = std::mem::size_of::<vpl::mfxExtCodingOption2>() as u32;
                let mut ptrs: [*mut vpl::mfxExtBuffer; 1] = [&mut co2_out.Header as *mut _];
                got.ExtParam = ptrs.as_mut_ptr();
                got.NumExtParam = 1;
                let sts = vpl::MFXVideoENCODE_GetVideoParam(session, &mut got);
                if sts < vpl::MFX_ERR_NONE {
                    tracing::debug!(
                        status = sts_name(sts),
                        "QSV: CodingOption2 did not read back — trusting the intra-refresh request"
                    );
                    true
                } else {
                    co2_out.IntRefType != 0
                }
            };
            if !confirmed {
                tracing::warn!(
                    codec = ?cfg.codec,
                    "QSV silently dropped intra-refresh (GetVideoParam reports IntRefType=0) — \
                     advertising it OFF so the client keeps asking for IDRs on loss"
                );
            }
            confirmed
        } else {
            false
        };
        Ok((ltr_active, ir_active, bs_bytes))
    }

    /// Open the session now instead of at the first submit, so `caps()` reports the LTR and
    /// intra-refresh the encoder actually negotiated. The host latches those once per session
    /// and gates reference-frame invalidation on them — read early, every lost frame costs a
    /// full IDR for the whole session.
    pub fn prepare(&mut self, device: &ID3D11Device) -> Result<()> {
        self.ensure_inner(device)
    }

    fn ensure_inner(&mut self, device: &ID3D11Device) -> Result<()> {
        let dev_raw = device.as_raw() as isize;
        if self.inner.is_some() && self.bound_device == dev_raw {
            return Ok(());
        }
        self.inner = None;
        self.bound_device = dev_raw;
        let luid = device_luid(device);
        let (loader, session, api) = create_session(luid)?;
        // SAFETY: `session.0` is live; `device.as_raw()` is a borrowed live COM pointer for the
        // synchronous SetHandle (the runtime AddRefs what it keeps). The multithread-protect QI
        // is standard COM on the owned immediate context.
        let dctx = unsafe {
            let dctx = device
                .GetImmediateContext()
                .context("ID3D11Device immediate context")?;
            // Runtime threads touch the device: protection must be ON before SetHandle
            // or the runtime returns MFX_ERR_UNDEFINED_BEHAVIOR.
            if let Ok(mt) = dctx.cast::<ID3D11Multithread>() {
                let _ = mt.SetMultithreadProtected(true);
            }
            vpl_ok(
                vpl::MFXVideoCORE_SetHandle(
                    session.0,
                    vpl::MFX_HANDLE_D3D11_DEVICE,
                    device.as_raw(),
                ),
                "MFXVideoCORE_SetHandle(D3D11)",
            )?;
            dctx
        };
        let (ltr_active, ir_active, bs_bytes) = self.init_encode(session.0)?;
        self.ltr_active = ltr_active;
        self.ir_active = ir_active;
        self.ltr_slots = [None; NUM_LTR_SLOTS];
        self.ltr_tainted = [false; NUM_LTR_SLOTS];
        self.next_ltr_slot = 0;
        self.pending_force = None;
        self.hdr_applied = self.hdr_meta;
        tracing::info!(
            codec = ?self.codec,
            width = self.width,
            height = self.height,
            fps = self.fps,
            ten_bit = self.ten_bit,
            ltr = ltr_active,
            intra_refresh = ir_active,
            api = %format_args!("{}.{}", api.0, api.1),
            device = %format_args!("{:#x}", dev_raw as usize),
            "native QSV encode active (VPL, zero-copy D3D11)"
        );
        // The sync thread starts against the initialized session and is joined before anything
        // closes it (`Inner` drops it first; `reset` stops it by hand).
        let retrieve = Retrieve::start(session.0)?;
        self.inner = Some(Inner {
            retrieve,
            session,
            _loader: loader,
            _device: device.clone(),
            dctx,
            bs_bytes,
            frames_submitted: 0,
            first_au_logged: false,
            array_warned: false,
        });
        Ok(())
    }
}

impl QsvEncoder {
    /// [`Encoder::submit`] without the failure rule: the LTR mirror and a queued force are
    /// committed before the runtime takes the frame, so a refusal leaves them one frame ahead
    /// of the hardware. Only the wrapper's forced IDR resets both.
    fn try_submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        anyhow::ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured frame {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!("native QSV is D3D11-only; got a CPU frame (video processor lost?)")
            }
        };
        let expected = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        anyhow::ensure!(
            captured.format == expected,
            "captured format {:?} != QSV input {:?} (capturer video-processor fallback \
             mid-session — native QSV has no readback path)",
            captured.format,
            expected
        );
        self.ensure_inner(&frame.device)?;
        // Mid-stream HDR regrade re-Inits so the new mastering SEI/OBU rides the fresh IDR.
        if self.ten_bit && self.hdr_meta != self.hdr_applied && self.hdr_meta.is_some() {
            tracing::info!("QSV HDR metadata changed — re-initialising the encoder");
            self.inner = None;
            self.bound_device = 0;
            self.ensure_inner(&frame.device)?;
        }
        let cur_idx = self.frame_idx;
        let opening = self.inner.as_ref().is_none_or(|i| i.frames_submitted == 0);
        let forced = std::mem::take(&mut self.force_kf) || opening;
        self.frame_idx += 1;
        let mut mark_slot: Option<usize> = None;
        let mut force_ltr: Option<(usize, i64)> = None;
        let mut recovery_anchor = false;
        if self.ltr_active {
            if forced {
                // IDR voids decoder refs — drop stale slots and any queued force.
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.ltr_tainted = [false; NUM_LTR_SLOTS]; // IDR flushed the DPB
                self.next_ltr_slot = 0;
                self.pending_force = None;
            } else if self.ltr_test_force_at == Some(cur_idx) {
                let triggered = self.invalidate_ref_frames(cur_idx, cur_idx);
                tracing::info!(
                    frame = cur_idx,
                    triggered,
                    "QSV LTR test hook fired invalidate_ref_frames"
                );
            }
            if let Some(slot) = self.pending_force.take() {
                // Resolve now: taint may have landed since the force was queued.
                // Empty or tainted = ship a plain P, no `recovery_anchor` (that tag
                // lifts the client's post-loss freeze).
                if let Some(idx) = self.ltr_slots[slot].filter(|_| !self.ltr_tainted[slot]) {
                    force_ltr = Some((slot, idx));
                    recovery_anchor = true;
                }
            }
            if force_ltr.is_none() && (forced || cur_idx % self.ltr_mark_interval == 0) {
                let trusted: [bool; NUM_LTR_SLOTS] =
                    std::array::from_fn(|s| self.ltr_slots[s].is_some() && !self.ltr_tainted[s]);
                let slot = super::rfi::mark_slot(&trusted, self.next_ltr_slot);
                self.ltr_slots[slot] = Some(cur_idx);
                // Re-mark replaces LongTermIdx: the tainted frame leaves the DPB.
                self.ltr_tainted[slot] = false;
                self.next_ltr_slot = (slot + 1) % NUM_LTR_SLOTS;
                mark_slot = Some(slot);
            }
        }
        #[cfg(test)]
        if self.fail_submit_at == Some(cur_idx) {
            bail!("test hook: frame {cur_idx} refused after the LTR decision");
        }
        let ltr_slots = self.ltr_slots;
        let reject_ok = self.codec != Codec::Av1;
        let inner = self.inner.as_mut().expect("ensure_inner succeeded");
        // Wait for the sync thread to free a slot before submitting: the runtime keeps writing a
        // bitstream until its frame is synced, so the queue must not grow under overload.
        if inner.retrieve.in_flight() >= IN_FLIGHT_MAX {
            let deadline = std::time::Instant::now() + BUSY_BUDGET;
            while inner.retrieve.in_flight() >= IN_FLIGHT_MAX {
                if let Some(e) = lock(&inner.retrieve.out).err.take() {
                    bail!("{e}");
                }
                if std::time::Instant::now() >= deadline {
                    bail!(
                        "QSV produced no output for {} ms with {} frame(s) in flight — \
                         wedged (escalating to reset)",
                        BUSY_BUDGET.as_millis(),
                        inner.retrieve.in_flight()
                    );
                }
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
        }
        // SAFETY: the whole block runs on the single encode thread against the live session.
        // `MFXMemory_GetSurfaceForEncode` returns a runtime-owned surface we must Release
        // exactly once (every exit path below does). `GetNativeHandle` returns a borrowed
        // (non-AddRef'd) D3D11 texture the runtime keeps alive at least until the surface's
        // Release — the `CopySubresourceRegion` happens strictly before that. The manually
        // re-wrapped `ID3D11Texture2D::from_raw_borrowed` reference is never released by us.
        // `EncodeFrameAsync` copies `ctrl` internally; the attached ext buffers live on in the
        // `Pending` entry until the sync point completes, per the API contract.
        unsafe {
            let mut surf: *mut vpl::mfxFrameSurface1 = ptr::null_mut();
            vpl_ok(
                vpl::MFXMemory_GetSurfaceForEncode(inner.session.0, &mut surf),
                "MFXMemory_GetSurfaceForEncode",
            )?;
            if surf.is_null() {
                bail!("MFXMemory_GetSurfaceForEncode returned null");
            }
            let iface = (*surf).__bindgen_anon_1.FrameInterface;
            let release = (!iface.is_null())
                .then(|| (*iface).Release)
                .flatten()
                .ok_or_else(|| anyhow!("QSV surface has no FrameInterface.Release"))?;
            // Every failure path below must release the surface.
            let submit_result: Result<vpl::mfxSyncPoint> = (|| {
                let get_native = (*iface)
                    .GetNativeHandle
                    .ok_or_else(|| anyhow!("QSV surface has no GetNativeHandle"))?;
                let mut res: vpl::mfxHDL = ptr::null_mut();
                let mut res_type: vpl::mfxResourceType = 0;
                vpl_ok(
                    get_native(surf, &mut res, &mut res_type),
                    "FrameInterface.GetNativeHandle",
                )?;
                if res_type != vpl::MFX_RESOURCE_DX11_TEXTURE || res.is_null() {
                    bail!("QSV surface native handle is not a D3D11 texture (type {res_type})");
                }
                let dst = ID3D11Texture2D::from_raw_borrowed(&res)
                    .ok_or_else(|| anyhow!("QSV native handle is not ID3D11Texture2D"))?;
                if !inner.array_warned {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    dst.GetDesc(&mut desc);
                    if desc.ArraySize > 1 {
                        inner.array_warned = true;
                        tracing::warn!(
                            array_size = desc.ArraySize,
                            "QSV runtime handed out an ARRAY texture — subresource-0 copy may \
                             target the wrong slice (needs the on-glass check, design §3.4)"
                        );
                    }
                }
                let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
                let dst_res: ID3D11Resource = dst.cast().context("qsv texture -> resource")?;
                inner
                    .dctx
                    .CopySubresourceRegion(&dst_res, 0, 0, 0, 0, &src, 0, None);
                // mfxExtRefListCtrl keys on FrameOrder; `submit_indexed` keeps that = wire index.
                (*surf).Data.FrameOrder = cur_idx as u32;
                (*surf).Data.TimeStamp = captured.pts_ns.wrapping_mul(9) / 100_000; // 90 kHz
                let mut ctrl: Option<Box<FrameCtrl>> = None;
                if forced || mark_slot.is_some() || force_ltr.is_some() {
                    let mut c = FrameCtrl::new();
                    if forced {
                        c.ctrl.FrameType = (vpl::MFX_FRAMETYPE_IDR
                            | vpl::MFX_FRAMETYPE_I
                            | vpl::MFX_FRAMETYPE_REF)
                            as u16;
                    }
                    let mut use_reflist = false;
                    if let Some(slot) = mark_slot {
                        c.reflist.LongTermRefList[0].FrameOrder = cur_idx as u32;
                        c.reflist.LongTermRefList[0].PicStruct =
                            vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
                        c.reflist.LongTermRefList[0].LongTermIdx = slot as u16;
                        c.reflist.ApplyLongTermIdx = 1;
                        use_reflist = true;
                    }
                    if let Some((slot, ltr_frame)) = force_ltr {
                        // LongTermIdx stays 0 in PreferredRefList (AV1 rejects nonzero;
                        // AVC/HEVC key on FrameOrder).
                        c.reflist.PreferredRefList[0].FrameOrder = ltr_frame as u32;
                        c.reflist.PreferredRefList[0].PicStruct =
                            vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
                        // PreferredRefList is a reorder hint; the encoder may still
                        // predict from tainted short-term refs. Reject the other DPB
                        // candidates and cap L0 at 1. AVC/HEVC only — AV1 rejection
                        // is unvalidated; an unhonored hint still IDR-escalates.
                        if reject_ok {
                            let mut rej = 0;
                            let mut reject = |idx: i64| {
                                if idx >= 0 && idx != ltr_frame {
                                    c.reflist.RejectedRefList[rej].FrameOrder = idx as u32;
                                    c.reflist.RejectedRefList[rej].PicStruct =
                                        vpl::MFX_PICSTRUCT_PROGRESSIVE as u16;
                                    rej += 1;
                                }
                            };
                            reject(cur_idx - 1);
                            reject(cur_idx - 2);
                            for (s, marked) in ltr_slots.iter().enumerate() {
                                if s != slot {
                                    if let Some(idx) = *marked {
                                        reject(idx);
                                    }
                                }
                            }
                            c.reflist.NumRefIdxL0Active = 1;
                        }
                        use_reflist = true;
                        tracing::info!(
                            slot,
                            ltr_frame,
                            frame = cur_idx,
                            "QSV LTR-RFI: re-referencing known-good LTR (clean recovery, \
                             no IDR)"
                        );
                    }
                    if use_reflist {
                        c.attach_reflist();
                    }
                    ctrl = Some(c);
                }
                let mut bs = inner.take_bs();
                let mut syncp: vpl::mfxSyncPoint = ptr::null_mut();
                let ctrl_ptr = ctrl
                    .as_mut()
                    .map(|c| &mut c.ctrl as *mut vpl::mfxEncodeCtrl)
                    .unwrap_or(ptr::null_mut());
                let deadline = std::time::Instant::now() + BUSY_BUDGET;
                let sts = loop {
                    let sts = vpl::MFXVideoENCODE_EncodeFrameAsync(
                        inner.session.0,
                        ctrl_ptr,
                        surf,
                        &mut bs.mfx,
                        &mut syncp,
                    );
                    if sts != vpl::MFX_WRN_DEVICE_BUSY {
                        break sts;
                    }
                    // The sync thread is what frees the runtime; this only re-offers the frame.
                    if std::time::Instant::now() >= deadline {
                        break sts;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(250));
                };
                match sts {
                    s if s == vpl::MFX_WRN_DEVICE_BUSY => {
                        bail!("QSV EncodeFrameAsync stayed DEVICE_BUSY past the drain budget");
                    }
                    // GopRefDist=1 owes one AU per submit; MORE_DATA would desync the FIFO.
                    vpl::MFX_ERR_MORE_DATA => {
                        bail!("QSV EncodeFrameAsync returned MORE_DATA with GopRefDist=1");
                    }
                    s if s < vpl::MFX_ERR_NONE => {
                        bail!("QSV EncodeFrameAsync failed: {} ({s})", sts_name(s));
                    }
                    _ => {}
                }
                if syncp.is_null() {
                    bail!("QSV EncodeFrameAsync returned no sync point");
                }
                lock(&inner.retrieve.out).pending.push_back(Pending {
                    syncp,
                    bs,
                    pts_ns: captured.pts_ns,
                    forced,
                    recovery_anchor,
                    _ctrl: ctrl,
                });
                inner.frames_submitted += 1;
                Ok(syncp)
            })();
            // Runtime holds its own ref for the in-flight encode; ours drops now.
            let _ = release(surf);
            submit_result?;
        }
        Ok(())
    }
}

impl Encoder for QsvEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let submitted = self.try_submit(captured);
        // A frame the runtime never took: the next one is an IDR, which resets the LTR
        // mirror and any queued force to what the hardware holds.
        if submitted.is_err() {
            self.force_kf = true;
        }
        submitted
    }

    /// Pin `frame_idx` to the wire index so LTR slots and FrameOrder stay in the wire domain.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.hdr_meta = meta;
    }

    /// Force-reference the newest LTR marked before `[first, last]`. `false` → IDR fallback.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if !self.ltr_active || first < 0 || first > last {
            return false;
        }
        // Policy is `rfi::plan_slot_recovery`. Distrust is `ltr_tainted`, never a
        // cleared mirror: `ltr_slots` tracks the hardware DPB, and RejectedRefList
        // only names `Some` slots. Filter with `!ltr_tainted` so distrust survives
        // later losses.
        let view: Vec<(usize, i64)> = self
            .ltr_slots
            .iter()
            .enumerate()
            .filter(|&(slot, _)| !self.ltr_tainted[slot])
            .filter_map(|(s, m)| m.map(|w| (s, w)))
            .collect();
        let plan = super::rfi::plan_slot_recovery(&view, first);
        for (slot, tainted) in self.ltr_tainted.iter_mut().enumerate() {
            if plan.tainted & (1 << slot) != 0 {
                *tainted = true;
            }
        }
        match plan.anchor {
            Some((slot, ltr_frame)) => {
                self.pending_force = Some(slot);
                tracing::info!(
                    first,
                    last,
                    slot,
                    ltr_frame,
                    "QSV LTR-RFI: forcing the next frame to re-reference a known-good LTR (no \
                     IDR)"
                );
                true
            }
            None => {
                // Drop a queued force that now points at nothing clean.
                self.pending_force = None;
                tracing::info!(
                    first,
                    last,
                    "QSV LTR-RFI: no live LTR older than the loss — falling back to IDR recovery"
                );
                false
            }
        }
    }

    /// Withdraw anchor trust from every live LTR (trait docs carry the why).
    /// Distrust is `ltr_tainted`, never a cleared `ltr_slots` entry — the mirror
    /// tracks the hardware DPB and RejectedRefList only names `Some` slots.
    /// Taint clears on IDR flush or re-mark. Drop `pending_force` so an unconsumed
    /// force cannot re-reference a slot this call just distrusted.
    fn distrust_references(&mut self) {
        let live = self
            .ltr_slots
            .iter()
            .enumerate()
            .filter(|&(slot, m)| m.is_some() && !self.ltr_tainted[slot])
            .count();
        if live == 0 && self.pending_force.is_none() {
            return;
        }
        self.ltr_tainted = [true; NUM_LTR_SLOTS];
        self.pending_force = None;
        tracing::debug!(
            live,
            "QSV LTR-RFI: client reported unrepaired damage — withdrawing anchor trust from every \
             live LTR (cleared by the next re-mark or IDR flush)"
        );
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            // Capturer composites; this backend never reads `frame.cursor`.
            blends_cursor: false,
            supports_rfi: self.ltr_active,
            chroma_444: false,
            intra_refresh: self.ir_active,
            // Unvalidated — host keeps the IDR recovery path until then.
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
            downscales_input: false,
            crops_input: false,
        }
    }

    /// Wait up to `min(3/4 frame interval, 12 ms)` for the oldest AU. Expiry is `Ok(None)`.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let au = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(None);
            };
            // The same bound as before, now spent on the sync thread's event rather than inside
            // `SyncOperation`, so nothing else on this thread waits behind it.
            let budget_ms = (750 / self.fps.max(1)).clamp(1, 12);
            let au = inner.take_ready(budget_ms)?;
            if let Some(au) = &au {
                inner.note_first_au(au);
            }
            au
        };
        if au.is_some() {
            self.resets_without_output = 0;
        }
        Ok(au)
    }

    /// The sync thread's signal, once a session exists. Before the lazy open there is nothing to
    /// wait on, which a caller reads as "no completion signal" and polls instead.
    fn ready_event(&self) -> Option<isize> {
        self.inner.as_ref().map(|i| i.retrieve.have.raw())
    }

    /// Stall recovery: Close+Init in place. A second reset with no AU drops the whole session.
    fn reset(&mut self) -> bool {
        self.force_kf = true;
        self.resets_without_output = self.resets_without_output.saturating_add(1);
        if self.inner.is_none() {
            return true;
        }
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "QSV stall persisted across in-place re-Init — full session teardown, reopening \
                 lazily (next submit)"
            );
            self.inner = None;
            self.bound_device = 0;
            self.ir_active = false;
            self.ltr_active = false;
            return true;
        }
        let rebuilt = {
            let inner = self.inner.as_mut().expect("checked above");
            // Stop and join first: the sync thread is inside `SyncOperation` on this very
            // session, and Close under it would abort the operation it is waiting on.
            inner.retrieve.stop_and_join();
            // Close before dropping `pending`: each entry owns the BsBuf/FrameCtrl the runtime
            // still writes, and Close is what aborts those writes.

            // SAFETY: the session is live on this thread with its sync thread joined; Close on a
            // wedged encoder is legal (result deliberately ignored) and re-Init happens through
            // `init_encode`.
            unsafe {
                let _ = vpl::MFXVideoENCODE_Close(inner.session.0);
            }
            inner.retrieve.reset_queues();
            inner.frames_submitted = 0;
            inner.first_au_logged = false;
            inner.session.0
        };
        match self.init_encode(rebuilt) {
            Ok((ltr, ir, bs_bytes)) => {
                self.ltr_active = ltr;
                self.ir_active = ir;
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.ltr_tainted = [false; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
                let restarted = match self.inner.as_mut() {
                    Some(inner) => {
                        inner.bs_bytes = bs_bytes;
                        // BufferSizeInKB may have changed; a pooled buffer sized for the old rate
                        // would be short.
                        lock(&inner.retrieve.out).bs_pool.clear();
                        // The session encodes again, so it needs its sync thread back — without
                        // one nothing would ever take an AU off it.
                        match Retrieve::start(inner.session.0) {
                            Ok(r) => {
                                inner.retrieve = r;
                                true
                            }
                            Err(e) => {
                                tracing::warn!(
                                    error = %format!("{e:#}"),
                                    "QSV rebuilt but its sync thread would not start — reopening \
                                     lazily"
                                );
                                false
                            }
                        }
                    }
                    None => false,
                };
                if restarted {
                    tracing::info!("QSV encoder rebuilt in place (Close + re-Init on the session)");
                } else {
                    self.inner = None;
                    self.bound_device = 0;
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "QSV in-place re-Init failed — full session teardown, reopening lazily"
                );
                self.inner = None;
                self.bound_device = 0;
                self.ir_active = false;
                self.ltr_active = false;
            }
        }
        true
    }

    /// No-IDR ABR: `MFXVideoENCODE_Reset` + `StartNewSequence=OFF` (legal because HRD is off).
    /// Drain in-flight first — Reset requires completed syncs; a stuck drain falls back.
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        // Drain in its own scope so the `inner` borrow ends before the param rebuild.
        let session = {
            let Some(inner) = self.inner.as_mut() else {
                self.bitrate_bps = bps;
                return true;
            };
            // Reset needs every sync completed; the sync thread is what completes them, so this
            // waits for the queue to empty rather than draining it here.
            let deadline = std::time::Instant::now() + BUSY_BUDGET;
            while inner.retrieve.in_flight() > 0 {
                if let Some(e) = lock(&inner.retrieve.out).err.take() {
                    tracing::warn!(error = %e, "QSV retarget drain failed");
                    return false;
                }
                if std::time::Instant::now() >= deadline {
                    tracing::warn!(
                        "QSV bitrate retarget: in-flight frames didn't settle — falling back to a \
                         rebuild"
                    );
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
            inner.session.0
        };
        let old = self.bitrate_bps;
        self.bitrate_bps = bps;
        let cfg = self.encode_config();
        let mut set = build_params(&cfg);
        // SAFETY: all-zero valid; header stamped below; outlives the synchronous Reset call.
        let mut reset_opt: Box<vpl::mfxExtEncoderResetOption> =
            Box::new(unsafe { std::mem::zeroed() });
        reset_opt.Header.BufferId = vpl::MFX_EXTBUFF_ENCODER_RESET_OPTION as u32;
        reset_opt.Header.BufferSz = std::mem::size_of::<vpl::mfxExtEncoderResetOption>() as u32;
        reset_opt.StartNewSequence = vpl::MFX_CODINGOPTION_OFF as u16;
        set.ptrs
            .push(&mut reset_opt.Header as *mut vpl::mfxExtBuffer);
        set.par.ExtParam = set.ptrs.as_mut_ptr();
        set.par.NumExtParam = set.ptrs.len() as u16;
        // SAFETY: session live on this thread, no operation in flight (drained above); the
        // param block + ext chain outlive the synchronous call.
        let sts = unsafe { vpl::MFXVideoENCODE_Reset(session, &mut set.par) };
        if sts < vpl::MFX_ERR_NONE {
            tracing::warn!(
                mbps = bps / 1_000_000,
                status = sts_name(sts),
                "QSV declined the no-IDR bitrate retarget — falling back to a rebuild"
            );
            self.bitrate_bps = old;
            return false;
        }
        // Reset re-derives BufferSizeInKB; a step-up can outgrow pooled buffers.
        // take_bs drops any that are now too small.

        // SAFETY: `session` is live on this thread and drained above; `got` and its (empty) ext
        // chain outlive the synchronous call.
        let refreshed = unsafe {
            let mut got: vpl::mfxVideoParam = std::mem::zeroed();
            let sts = vpl::MFXVideoENCODE_GetVideoParam(session, &mut got);
            (sts >= vpl::MFX_ERR_NONE).then(|| {
                let m = &mut got.__bindgen_anon_1.mfx;
                let mult = m.BRCParamMultiplier.max(1) as usize;
                let kb = enc_of(m).BufferSizeInKB as usize;
                (kb * mult * 1000).max(256 * 1024)
            })
        };
        match (refreshed, self.inner.as_mut()) {
            (Some(bytes), Some(inner)) if bytes > inner.bs_bytes => {
                tracing::debug!(
                    old_bytes = inner.bs_bytes,
                    new_bytes = bytes,
                    mbps = bps / 1_000_000,
                    "QSV retarget raised the worst-case AU size — resizing the bitstream pool"
                );
                inner.bs_bytes = bytes;
            }
            (None, _) => tracing::warn!(
                "QSV retarget: GetVideoParam failed, keeping the previous AU buffer size"
            ),
            _ => {}
        }
        true
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // Null-surface EncodeFrameAsync is EOS; owed AUs then surface through `poll`.
        // SAFETY: session live on this thread; a null surface is the documented EOS marker;
        // each drained AU gets its own pooled bitstream + sync point, queued like a submit.
        unsafe {
            loop {
                let mut bs = inner.take_bs();
                let mut syncp: vpl::mfxSyncPoint = ptr::null_mut();
                let sts = vpl::MFXVideoENCODE_EncodeFrameAsync(
                    inner.session.0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut bs.mfx,
                    &mut syncp,
                );
                if sts < vpl::MFX_ERR_NONE || syncp.is_null() {
                    break; // MFX_ERR_MORE_DATA = drained
                }
                lock(&inner.retrieve.out).pending.push_back(Pending {
                    syncp,
                    bs,
                    pts_ns: 0,
                    forced: false,
                    recovery_anchor: false,
                    _ctrl: None,
                });
            }
        }
        Ok(())
    }
}

/// Can the selected Intel GPU encode `codec`? Query on a tiny block; no device handle.
pub fn probe_can_encode(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    probe_query(codec, false, adapter_luid)
}

/// 10-bit encode (HEVC Main10 / AV1, P010). H.264 is always false — High10 is never negotiated.
pub fn probe_can_encode_10bit(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    probe_query(codec, true, adapter_luid)
}

fn probe_query(codec: Codec, ten_bit: bool, adapter_luid: Option<LUID>) -> bool {
    if codec == Codec::PyroWave {
        return false;
    }
    let selected = adapter_luid.map(|l| {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&l.LowPart.to_le_bytes());
        b[4..].copy_from_slice(&l.HighPart.to_le_bytes());
        b
    });
    // Prefer the selected adapter; on a hybrid non-Intel pick, fall back to first Intel
    // impl. The probe answers "can Intel silicon do it"; open then enforces same-adapter.
    let opened = match selected {
        Some(want) => create_session(Some(want)).or_else(|_| create_session(None)),
        None => create_session(None),
    };
    let Ok((_loader, session, _api)) = opened else {
        return false;
    };
    let cfg = EncodeConfig {
        codec,
        width: 640,
        height: 480,
        fps: 30,
        bitrate_bps: 4_000_000,
        ten_bit,
        intra_refresh: false,
        hdr_meta: None,
    };
    let mut q_in = build_params(&cfg);
    let mut q_out = build_params(&cfg);
    // SAFETY: session is live; both param blocks + ext chains outlive the synchronous call.
    let sts = unsafe { vpl::MFXVideoENCODE_Query(session.0, &mut q_in.par, &mut q_out.par) };
    sts >= vpl::MFX_ERR_NONE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enumeration must not crash; no Intel hardware → empty list or a clean error.
    #[test]
    fn intel_enumeration_smoke() {
        match intel_loader() {
            Ok((_l, impls)) => {
                tracing::debug!(count = impls.len(), "Intel VPL implementations");
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "no Intel VPL loader (expected on non-Intel boxes)");
            }
        }
    }

    /// Probe answers are booleans (no panic) with or without Intel hardware.
    #[test]
    fn probe_smoke() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let can = probe_can_encode(codec, None);
            let can10 = probe_can_encode_10bit(codec, None);
            tracing::debug!(?codec, can, can10, "QSV probe");
            if can10 {
                assert!(can, "10-bit implies base codec support");
            }
        }
    }

    fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("pf_encode=debug")
            .with_test_writer()
            .try_init();
    }

    struct AuMeta {
        pts_ns: u64,
        keyframe: bool,
        recovery_anchor: bool,
        annexb_start: bool,
        len: usize,
    }

    fn test_hdr_meta() -> pf_frame::HdrMeta {
        pf_frame::HdrMeta {
            display_primaries: [[13250, 34500], [7500, 3000], [34000, 16000]], // G,B,R
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000, // 1000 nits @ 0.0001 cd/m²
            min_display_mastering_luminance: 500,        // 0.05 nits
            max_cll: 1000,
            max_fall: 400,
        }
    }

    /// Live encode on Intel silicon. `None` = skip. `on_frame` runs before each submit.
    fn drive_live(
        codec: Codec,
        ten_bit: bool,
        frames: u32,
        mut on_frame: impl FnMut(&mut QsvEncoder, u32),
    ) -> Option<Vec<AuMeta>> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_SDK_VERSION, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
        };
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

        init_tracing();
        let Ok((_l, impls)) = intel_loader() else {
            eprintln!("skipping: no VPL loader");
            return None;
        };
        let Some(imp) = impls.iter().find(|i| i.luid_valid) else {
            eprintln!("skipping: no Intel VPL implementation on this box");
            return None;
        };
        if !probe_can_encode(codec, None) {
            eprintln!("skipping: this GPU declines {codec:?} encode");
            return None;
        }
        if ten_bit && !probe_can_encode_10bit(codec, None) {
            eprintln!("skipping: this GPU declines 10-bit {codec:?}");
            return None;
        }
        // SAFETY: self-contained harness owning every COM handle it creates; `EnumAdapterByLuid`
        // gets the LUID the runtime itself reported; `D3D11CreateDevice` fills `device` only
        // on success; the NV12/P010 texture is created and used on that one device/thread.
        let (device, tex) = unsafe {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: u32::from_le_bytes(imp.luid[..4].try_into().unwrap()),
                HighPart: i32::from_le_bytes(imp.luid[4..].try_into().unwrap()),
            };
            let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("dxgi factory");
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).expect("intel adapter");
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("d3d11 device on intel adapter");
            let device: ID3D11Device = device.expect("device");
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 640,
                Height: 480,
                MipLevels: 1,
                ArraySize: 1,
                Format: if ten_bit {
                    DXGI_FORMAT_P010
                } else {
                    DXGI_FORMAT_NV12
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut t: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .expect("input texture");
            (device.clone(), t.expect("texture"))
        };
        let format = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        let mut enc = QsvEncoder::open(
            codec,
            format,
            640,
            480,
            30,
            2_000_000,
            if ten_bit { 10 } else { 8 },
            ChromaFormat::Yuv420,
            None,
        )
        .expect("open");
        // What the driver's encode thread does, so the harness reads the caps the host would:
        // the session is opened here rather than at the first submit.
        enc.prepare(&device).expect("prepare");
        if ten_bit {
            enc.set_hdr_meta(Some(test_hdr_meta()));
        }
        let mut aus = Vec::new();
        let mut push = |au: EncodedFrame| {
            aus.push(AuMeta {
                pts_ns: au.pts_ns,
                keyframe: au.keyframe,
                recovery_anchor: au.recovery_anchor,
                annexb_start: au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
                len: au.data.len(),
            });
        };
        for i in 0..frames {
            on_frame(&mut enc, i);
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: 640,
                height: 480,
                pts_ns: i as u64 * 33_333_333,
                format,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            match enc.submit_indexed(&frame, i) {
                Ok(()) => assert_ne!(enc.fail_submit_at, Some(i as i64), "frame {i} not refused"),
                Err(e) => assert_eq!(enc.fail_submit_at, Some(i as i64), "submit {i}: {e:#}"),
            }
            if let Some(au) = enc.poll().expect("poll") {
                push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            push(au);
        }
        Some(aus)
    }

    fn assert_stream_shape(aus: &[AuMeta], frames: u32, annexb: bool) {
        assert!(
            aus.len() >= frames as usize - 5,
            "expected ~{frames} AUs, got {}",
            aus.len()
        );
        assert!(aus[0].keyframe, "first AU must be a keyframe");
        assert!(aus[0].len > 0);
        if annexb {
            assert!(aus[0].annexb_start, "first AU is not Annex-B");
        }
    }

    /// LTR anchors on hardware, as `amf_ltr_anchor_soak`: the moving pattern, a loss every
    /// `PF_WAVE_GAP` frames (default 40) answered `PF_WAVE_LAG` frames later (default 2) through
    /// `invalidate_ref_frames`. The full stream and the view without the lost frames land in
    /// `PUNKTFUNK_SMOKE_DIR` with `.idx` sidecars, for `gpu_parity`'s field hashers. HEVC, or
    /// H.264 with `PF_WAVE_CODEC=h264`; shape `PF_WAVE_SMOKE=WxH:8:fps:mbps`, `PF_WAVE_SOAK` losses.
    #[test]
    #[ignore = "requires an Intel GPU with QSV — run manually on the Intel VM (9200)"]
    fn qsv_ltr_anchor_soak() {
        use crate::smoke_pattern::{scroll_pattern_nv12, write_capture};
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
            D3D11_BIND_SHADER_RESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
            D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
        init_tracing();
        let (_loader, impls) = intel_loader().expect("an Intel VPL loader");
        let imp = impls
            .iter()
            .find(|i| i.luid_valid)
            .expect("an Intel VPL implementation");
        let shape = std::env::var("PF_WAVE_SMOKE").unwrap_or_else(|_| "256x256:8:60:2".into());
        let mut parts = shape.split(':');
        let (w, h) = parts
            .next()
            .and_then(|s| s.split_once('x'))
            .map(|(w, h)| (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap()))
            .expect("PF_WAVE_SMOKE=WxH[:8[:fps[:mbps]]]");
        assert_ne!(parts.next(), Some("10"), "the soak feeds NV12");
        let fps: u32 = parts.next().map_or(60, |f| f.parse().unwrap());
        let mbps: u64 = parts.next().map_or(2, |m| m.parse().unwrap());
        let count = |k: &str, d: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let (losses, gap, lag) = (
            count("PF_WAVE_SOAK", 12),
            count("PF_WAVE_GAP", 40),
            count("PF_WAVE_LAG", 2),
        );
        assert!(lag >= 1 && lag < gap, "PF_WAVE_LAG=1..PF_WAVE_GAP");
        let h264 = std::env::var("PF_WAVE_CODEC").is_ok_and(|v| v == "h264");
        let (codec, ext) = if h264 {
            (Codec::H264, "h264")
        } else {
            (Codec::H265, "h265")
        };
        // SAFETY: test-only COM on one thread. `EnumAdapterByLuid` gets the LUID the runtime
        // reported; `D3D11CreateDevice` fills `device` only on success.
        let device: ID3D11Device = unsafe {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: u32::from_le_bytes(imp.luid[..4].try_into().unwrap()),
                HighPart: i32::from_le_bytes(imp.luid[4..].try_into().unwrap()),
            };
            let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("dxgi factory");
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).expect("intel adapter");
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("d3d11 device on intel adapter");
            device.expect("device")
        };
        let mut enc = QsvEncoder::open(
            codec,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            mbps * 1_000_000,
            8,
            ChromaFormat::Yuv420,
            None,
        )
        .expect("QSV open");
        enc.prepare(&device).expect("prepare");
        assert!(
            enc.caps().supports_rfi,
            "the driver declined LTR: nothing to soak"
        );
        let texture = |i: usize| {
            let (w, h) = (w as usize, h as usize);
            let nv12 = scroll_pattern_nv12(w, h, i);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: w as u32,
                Height: h as u32,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: nv12.as_ptr() as *const _,
                SysMemPitch: w as u32,
                SysMemSlicePitch: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            // SAFETY: `init` points at `nv12`, alive across the call; the UV plane follows the Y
            // plane at the same pitch, the layout D3D11 reads NV12 initial data in.
            unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
                .expect("NV12 frame texture");
            tex.expect("NV12 frame texture")
        };
        // Loss k is frame 1 + k * gap; its ask comes `lag` frames later, before that frame.
        let base = lag + 1;
        let last = base + losses * gap;
        let (mut lost, mut anchors, mut idrs) = (Vec::new(), Vec::new(), Vec::new());
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for i in 0..=last {
            if i >= base && (i - base) % gap == 0 && (i - base) / gap < losses {
                let l = (i - lag) as i64;
                lost.push(i - lag);
                if enc.invalidate_ref_frames(l, l) {
                    anchors.push(i);
                } else {
                    enc.request_keyframe();
                    idrs.push(i);
                }
            }
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i as u64,
                format: PixelFormat::Nv12,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: texture(i),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit_indexed(&frame, i as u32).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            aus.push(au);
        }
        aus.sort_by_key(|a| a.pts_ns);
        assert_eq!(aus.len(), last + 1, "one AU per frame");
        for (i, au) in aus.iter().enumerate() {
            assert_eq!(
                au.recovery_anchor,
                anchors.contains(&i),
                "AU {i}: anchors where answered"
            );
            assert!(
                !idrs.contains(&i) || au.keyframe,
                "AU {i}: a declined ask is an IDR"
            );
        }
        let csv = |v: &[usize]| {
            v.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        println!(
            "qsv_ltr_anchor_soak: {w}x{h} {fps} fps {mbps} Mbps {codec:?} lag={lag} gap={gap} \
             lost={} anchors={} idrs={}",
            csv(&lost),
            csv(&anchors),
            csv(&idrs)
        );
        if let Ok(dir) = std::env::var("PUNKTFUNK_SMOKE_DIR") {
            let full: Vec<&[u8]> = aus.iter().map(|a| a.data.as_slice()).collect();
            let view: Vec<&[u8]> = aus
                .iter()
                .enumerate()
                .filter(|(i, _)| !lost.contains(i))
                .map(|(_, a)| a.data.as_slice())
                .collect();
            write_capture(&format!("{dir}/qsv-anchor.{ext}"), &full).expect("write");
            write_capture(&format!("{dir}/qsv-anchor-dropS.{ext}"), &view).expect("write");
        }
    }

    #[test]
    fn qsv_encode_live_smoke() {
        let Some(aus) = drive_live(Codec::H264, false, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, true);
    }

    #[test]
    fn qsv_live_hevc() {
        let Some(aus) = drive_live(Codec::H265, false, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, true);
    }

    #[test]
    fn qsv_live_hevc10_hdr() {
        let Some(aus) = drive_live(Codec::H265, true, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, true);
    }

    /// AV1 output is an OBU stream, not Annex-B.
    #[test]
    fn qsv_live_av1_10bit() {
        let Some(aus) = drive_live(Codec::Av1, true, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30, false);
    }

    /// The driver answers SET_ENCODE — where the host latches these caps for the session —
    /// before any frame is submitted, so what `caps()` says then must be what the encoder
    /// negotiated. LTR and intra-refresh are decided in `ensure_inner`, which used to run only
    /// at the first submit: the host latched `supports_rfi: false` and never sent a
    /// reference-frame invalidation, costing a full IDR per lost frame.
    ///
    /// Compares the two reads rather than demanding LTR, so a GPU that genuinely declines still
    /// passes; the printed values say which happened.
    #[test]
    fn qsv_caps_do_not_change_at_the_first_submit_live() {
        let (mut at_open, mut later) = (None, None);
        let Some(_) = drive_live(Codec::H264, false, 4, |enc, i| {
            // `drive_live` prepares at open, as the driver does; i == 0 runs before any submit.
            if i == 0 {
                at_open = Some((enc.caps().supports_rfi, enc.caps().intra_refresh));
            }
            if i == 3 {
                later = Some((enc.caps().supports_rfi, enc.caps().intra_refresh));
            }
        }) else {
            return;
        };
        eprintln!("QSV caps at open: {at_open:?} | after submits: {later:?}");
        assert_eq!(
            at_open, later,
            "the host reads these once, before the first frame"
        );
    }

    /// Mid-stream invalidate must emit a `recovery_anchor` P-frame, not an IDR.
    #[test]
    fn qsv_live_ltr_rfi() {
        let mut rfi_answered = false;
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 30 && enc.caps().supports_rfi {
                rfi_answered = enc.invalidate_ref_frames(28, 29);
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60, true);
        if !rfi_answered {
            eprintln!("note: driver declined LTR (supports_rfi=false or no usable slot) — IDR fallback path");
            return;
        }
        let anchor = aus.iter().position(|a| a.recovery_anchor);
        assert!(
            anchor.is_some(),
            "RFI was answered but no recovery_anchor AU was emitted"
        );
        let a = &aus[anchor.unwrap()];
        assert!(
            !a.keyframe,
            "the recovery anchor must be a clean P-frame, not an IDR"
        );
        assert!(
            !aus[1..].iter().any(|x| x.keyframe),
            "an IDR appeared despite successful LTR-RFI recovery"
        );
    }

    /// A loss covering every live LTR mark must decline (IDR fallback); no recovery_anchor AU.
    #[test]
    fn qsv_live_ltr_rfi_taint_sweep_declines() {
        let mut rfi_answered = None;
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 30 && enc.caps().supports_rfi {
                // Frame 0 is the IDR — every later mark is at-or-after the loss.
                rfi_answered = Some(enc.invalidate_ref_frames(0, 2));
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60, true);
        let Some(answered) = rfi_answered else {
            eprintln!("note: driver declined LTR (supports_rfi=false) — sweep not exercised");
            return;
        };
        assert!(
            !answered,
            "a loss covering every live LTR mark must fall back to IDR recovery"
        );
        assert!(
            !aus.iter().any(|a| a.recovery_anchor),
            "no recovery_anchor AU may ship when the sweep left no clean LTR"
        );
    }

    /// A submit refused after the LTR decision — surface fetch, native handle — must not leave
    /// the mirror claiming a mark the hardware never made, nor eat a queued force: the next
    /// frame is an IDR, which resets both. Refuses one mark frame and one recovery frame; the
    /// mirror checks skip when the driver declines LTR.
    #[test]
    fn qsv_live_refused_submit_forces_idr() {
        let mut ltr = false;
        let mut refused = (0u32, 0u32);
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 0 {
                ltr = enc.caps().supports_rfi;
                let mark = enc.ltr_mark_interval as u32;
                assert!(
                    mark >= 4,
                    "mark interval {mark} leaves no room for a loss window"
                );
                refused = (mark, 2 * mark);
            }
            if i == refused.0 {
                enc.fail_submit_at = Some(i as i64);
            }
            if i == refused.1 {
                if ltr {
                    // The IDR after the refused mark is the pre-loss anchor.
                    assert!(
                        enc.invalidate_ref_frames(i as i64 - 2, i as i64 - 1),
                        "no pre-loss LTR to force"
                    );
                }
                enc.fail_submit_at = Some(i as i64);
            }
            if i == 59 && ltr {
                assert!(
                    !enc.ltr_slots.contains(&Some(refused.0 as i64)),
                    "mirror claims a mark the hardware never made: {:?}",
                    enc.ltr_slots
                );
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 58, true);
        let au = |i: u32| {
            aus.iter()
                .find(|a| a.pts_ns == i as u64 * 33_333_333)
                .unwrap_or_else(|| panic!("no AU for frame {i}"))
        };
        for r in [refused.0, refused.1] {
            assert!(
                aus.iter().all(|a| a.pts_ns != r as u64 * 33_333_333),
                "refused frame {r} produced an AU"
            );
            assert!(
                au(r + 1).keyframe,
                "frame {} after refused frame {r} must be an IDR",
                r + 1
            );
        }
        eprintln!("live QSV refused-submit: {} AUs, ltr={ltr}", aus.len());
    }

    /// Mid-stream `reconfigure_bitrate` must accept and must not emit a keyframe.
    #[test]
    fn qsv_live_bitrate_retarget() {
        let mut accepted = false;
        let Some(aus) = drive_live(Codec::H264, false, 60, |enc, i| {
            if i == 30 {
                accepted = enc.reconfigure_bitrate(6_000_000);
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60, true);
        assert!(accepted, "the no-IDR bitrate retarget was declined");
        assert!(
            !aus[1..].iter().any(|x| x.keyframe),
            "the bitrate retarget emitted a keyframe (StartNewSequence leak)"
        );
    }

    /// 1920×1080 P010 bars: height is not 16-aligned, so ingest copies into a
    /// 1920×1088 pool surface whose chroma plane sits at a different row offset
    /// than the source. Dumps `%TEMP%\pf_qsv_1080_bars.h265` for off-box decode.
    #[test]
    fn qsv_live_p010_1080_colorbars_dump() {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT,
        };
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

        // 10-bit limited YCbCr for the 8 sRGB bars at 80-nit PQ/BT.2020. MSB-aligned (`<<6`).
        const BARS: [(u16, u16, u16); 8] = [
            (490, 512, 512),
            (478, 423, 518),
            (464, 525, 473),
            (450, 432, 476),
            (350, 584, 585),
            (325, 448, 598),
            (226, 650, 535),
            (64, 512, 512),
        ];
        const W: u32 = 1920;
        const H: u32 = 1080;

        init_tracing();
        let Ok((_l, impls)) = intel_loader() else {
            eprintln!("skipping: no VPL loader");
            return;
        };
        let Some(imp) = impls.iter().find(|i| i.luid_valid) else {
            eprintln!("skipping: no Intel VPL implementation on this box");
            return;
        };
        if !probe_can_encode_10bit(Codec::H265, None) {
            eprintln!("skipping: this GPU declines 10-bit HEVC");
            return;
        }

        // P010: plane 0 = H×W luma; plane 1 = H/2 rows of interleaved Cb,Cr. Vertical bars.
        let bar_w = (W / 8) as usize;
        let mut init = vec![0u16; (W as usize) * (H as usize + H as usize / 2)];
        for y in 0..H as usize {
            for x in 0..W as usize {
                init[y * W as usize + x] = BARS[(x / bar_w).min(7)].0 << 6;
            }
        }
        let chroma_base = (W as usize) * (H as usize);
        for cy in 0..(H as usize / 2) {
            for cx in 0..(W as usize / 2) {
                let (_, cb, cr) = BARS[((cx * 2) / bar_w).min(7)];
                init[chroma_base + cy * W as usize + cx * 2] = cb << 6;
                init[chroma_base + cy * W as usize + cx * 2 + 1] = cr << 6;
            }
        }

        // SAFETY: self-contained harness on one thread/device (same contract as `drive_live`);
        // the initial-data pointer outlives the synchronous CreateTexture2D that reads it.
        let (device, tex) = unsafe {
            let luid = windows::Win32::Foundation::LUID {
                LowPart: u32::from_le_bytes(imp.luid[..4].try_into().unwrap()),
                HighPart: i32::from_le_bytes(imp.luid[4..].try_into().unwrap()),
            };
            let factory: IDXGIFactory4 = CreateDXGIFactory1().expect("dxgi factory");
            let adapter: IDXGIAdapter1 = factory.EnumAdapterByLuid(luid).expect("intel adapter");
            let mut device = None;
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                Default::default(),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("d3d11 device on intel adapter");
            let device: ID3D11Device = device.expect("device");
            let desc = D3D11_TEXTURE2D_DESC {
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
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let data = D3D11_SUBRESOURCE_DATA {
                pSysMem: init.as_ptr() as *const std::ffi::c_void,
                SysMemPitch: W * 2,
                SysMemSlicePitch: 0,
            };
            let mut t: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, Some(&data), Some(&mut t))
                .expect("bar texture");
            (device.clone(), t.expect("texture"))
        };

        let mut enc = QsvEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            W,
            H,
            30,
            10_000_000,
            10,
            ChromaFormat::Yuv420,
            None,
        )
        .expect("open");
        enc.set_hdr_meta(Some(test_hdr_meta()));
        let mut stream = Vec::new();
        let mut aus = 0usize;
        let mut keyframes = 0usize;
        for i in 0..12u32 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: i as u64 * 33_333_333,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit_indexed(&frame, i).expect("submit");
            if let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                keyframes += au.keyframe as usize;
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            aus += 1;
            keyframes += au.keyframe as usize;
            stream.extend_from_slice(&au.data);
        }
        assert!(aus >= 10, "expected ≥10 AUs, got {aus}");
        assert!(keyframes >= 1, "expected an IDR in the dump");
        let path = std::env::temp_dir().join("pf_qsv_1080_bars.h265");
        std::fs::write(&path, &stream).expect("write dump");
        println!(
            "wrote {} AUs ({} bytes, {keyframes} keyframes) to {}",
            aus,
            stream.len(),
            path.display()
        );
    }
}
