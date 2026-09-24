//! AMD **AMF** hardware encoder (Windows, D3D11 input). Direct-SDK analogue of [`super::nvenc`].
//!
//! Drives the AMF **C vtable ABI** (GPUOpen headers; FFmpeg's `amfenc.c` uses the same surface,
//! not the C++ classes). FFI is header **v1.4.36**; load accepts runtimes down to **v1.4.30**
//! ([`sys::AMF_MIN_VERSION`]). Newer encoder features are string-keyed properties that degrade
//! per driver, not vtable changes. Loads `amfrt64.dll` at runtime — no build feature. Missing or
//! old runtime fails [`AmfEncoder::open`] and the session.
//!
//! Input is a same-device D3D11 NV12/P010 texture ring: `CopySubresourceRegion` then
//! `CreateSurfaceFromDX11Native`. No readback: Bgra/Rgb10a2 or CPU frames fail open/submit.
//! VCN does not encode 4:4:4. Evidence: `design/native-amf-encoder.md`.

// `unsafe_op_in_unsafe_fn` is off here: the body is raw AMF vtable calls. Clearing it means
// deleting markers that carry no caller contract, not wrapping each call in `unsafe {}`.
#![allow(unsafe_op_in_unsafe_fn)]

use super::policy::{intra_refresh_period, intra_refresh_requested, ltr_test_force_at};
use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::retrieve::Ready;
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use windows::core::{w, Interface, PCWSTR};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
    D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

// FFI vtable mirror in `amf_sys.rs` (no policy). `#[path]` keeps the `sys::` name at call sites.
#[path = "amf_sys.rs"]
mod sys;

use sys::{result_name, AmfVariant};

fn amf_ok(r: sys::AmfResult, what: &str) -> Result<()> {
    if r == sys::AMF_OK {
        Ok(())
    } else {
        Err(anyhow!("{what}: {} ({r})", result_name(r)))
    }
}

/// `AMF_FULL_VERSION` as `major.minor.patch` — the build nibble is unused here.
fn amf_version_str(v: u64) -> String {
    format!(
        "{}.{}.{}",
        (v >> 48) & 0xffff,
        (v >> 32) & 0xffff,
        (v >> 16) & 0xffff
    )
}

/// Path + file-version of the loaded `amfrt64.dll`. File-version is the driver build, not the AMF
/// runtime version — a stale System32 copy can lag the display driver. Diagnostics only.
///
/// # Safety
/// `module` must be a live handle the caller owns (the never-unloaded `amfrt64.dll`).
unsafe fn loaded_dll_identity(module: HMODULE) -> (Option<String>, Option<String>) {
    let mut buf = [0u16; 512];
    let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
    // n == 0 failed; n >= len truncated (no guaranteed NUL). Otherwise `buf[n]` is the terminator.
    if n == 0 || n >= buf.len() {
        return (None, None);
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    (Some(path), dll_file_version(PCWSTR(buf.as_ptr())))
}

/// `VS_FIXEDFILEINFO` file version as `a.b.c.d`. `None` if the resource is missing.
///
/// # Safety
/// `path` is a valid NUL-terminated wide string to a readable file.
unsafe fn dll_file_version(path: PCWSTR) -> Option<String> {
    let size = GetFileVersionInfoSizeW(path, None);
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    GetFileVersionInfoW(path, None, size, block.as_mut_ptr() as *mut c_void).ok()?;
    let mut value: *mut c_void = ptr::null_mut();
    let mut len: u32 = 0;
    let ok = VerQueryValueW(
        block.as_ptr() as *const c_void,
        w!("\\"),
        &mut value,
        &mut len,
    );
    if !ok.as_bool() || value.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
    {
        return None;
    }
    // SAFETY: on success `VerQueryValueW` points `value` at a `VS_FIXEDFILEINFO` living inside
    // `block` and valid for `len` bytes (checked >= its size); `block` outlives this read.
    let ffi = &*(value as *const VS_FIXEDFILEINFO);
    let (ms, ls) = (ffi.dwFileVersionMS, ffi.dwFileVersionLS);
    Some(format!(
        "{}.{}.{}.{}",
        ms >> 16,
        ms & 0xffff,
        ls >> 16,
        ls & 0xffff
    ))
}

// Runtime loader: resolve `amfrt64.dll` once, gate on the ABI floor, keep the factory forever.

struct AmfLib {
    factory: *mut sys::AmfFactory,
    version: u64,
}
// SAFETY: `factory` is the process-global AMFInit singleton; AMF documents factory creation as
// thread-safe, the DLL is never unloaded, and this is only handed out as `&'static` from a
// `OnceLock` — no interior mutation on the Rust side.
unsafe impl Send for AmfLib {}
// SAFETY: shared refs only read the two plain fields; mutation is inside the thread-safe runtime.
unsafe impl Sync for AmfLib {}

/// Resolve the AMF runtime once per process. `Err` = no `amfrt64.dll` or older than
/// [`sys::AMF_MIN_VERSION`] — callers fail open with "update the AMD driver".
fn try_factory() -> std::result::Result<&'static AmfLib, &'static str> {
    static LIB: std::sync::OnceLock<std::result::Result<AmfLib, String>> =
        std::sync::OnceLock::new();
    LIB.get_or_init(|| {
        let lib = load_factory();
        if let Err(e) = &lib {
            tracing::warn!(error = %e, "native AMF runtime unavailable");
        }
        lib
    })
    .as_ref()
    .map_err(|e| e.as_str())
}

fn load_factory() -> std::result::Result<AmfLib, String> {
    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: `LoadLibraryExW`/`GetProcAddress` take static NUL-terminated names; SYSTEM32-only
    // search keeps a planted DLL out of the SYSTEM-service process. Transmutes match
    // `AMFQueryVersion_Fn`/`AMFInit_Fn` (core/Factory.h). `AMFQueryVersion` writes one u64;
    // `AMFInit` is passed min(header, runtime) and fills `factory` only on AMF_OK (null-checked).
    // The module is never freed, so factory and entry points live for the process.
    unsafe {
        let module = LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| {
                format!("amfrt64.dll not loadable (install/update the AMD driver): {e}")
            })?;
        let query_version = GetProcAddress(module, s!("AMFQueryVersion"))
            .ok_or("amfrt64.dll exports no AMFQueryVersion")?;
        let init = GetProcAddress(module, s!("AMFInit")).ok_or("amfrt64.dll exports no AMFInit")?;
        let query_version: sys::AmfQueryVersionFn = std::mem::transmute(query_version);
        let init: sys::AmfInitFn = std::mem::transmute(init);

        let mut version = 0u64;
        let r = query_version(&mut version);
        if r != sys::AMF_OK {
            return Err(format!("AMFQueryVersion failed: {} ({r})", result_name(r)));
        }
        // Path + file version of the System32 DLL actually loaded — a stale copy can lag the driver.
        let (dll_path, dll_file_ver) = loaded_dll_identity(module);
        let dll_desc = format!(
            "{}{}",
            dll_path.as_deref().unwrap_or("amfrt64.dll"),
            dll_file_ver
                .as_deref()
                .map(|v| format!(" (file version {v})"))
                .unwrap_or_default(),
        );
        // Below AMF_MIN_VERSION the mirrored vtable is not guaranteed — decline, never UB. Newer
        // encoder features are string properties (`set_prop(required=false)`), not vtable slots.
        if version < sys::AMF_MIN_VERSION {
            return Err(format!(
                "AMF runtime {amf} (loaded from {dll_desc}) is older than the minimum supported \
                 1.4.30 — update the AMD driver (Adrenalin 23.5.2+; 25.1.1+ for the \
                 fully-validated feature set). If the display driver already reports a newer \
                 version, this amfrt64.dll did not update — reboot, then DDU + reinstall so \
                 System32's copy is refreshed.",
                amf = amf_version_str(version),
            ));
        }
        // Never pass a version newer than the runtime: AMFInit can reject an otherwise-usable driver.
        let init_version = sys::AMF_HEADER_VERSION.min(version);
        let mut factory: *mut sys::AmfFactory = ptr::null_mut();
        let r = init(init_version, &mut factory);
        if r != sys::AMF_OK {
            return Err(format!("AMFInit failed: {} ({r})", result_name(r)));
        }
        if factory.is_null() {
            return Err("AMFInit returned a null factory".into());
        }
        if version >= sys::AMF_HEADER_VERSION {
            tracing::info!(
                amf_version = %amf_version_str(version),
                dll = %dll_desc,
                "AMF runtime loaded (meets the validated 1.4.36 baseline)"
            );
        } else {
            tracing::warn!(
                amf_version = %amf_version_str(version),
                dll = %dll_desc,
                "AMF runtime is older than the validated 1.4.36 baseline — accepted (the core \
                 encode ABI is stable), but advanced features (LTR / intra-refresh recovery, AV1 \
                 coded-size alignment, in-band HDR metadata) validated on 1.4.36 may be \
                 unavailable on this driver and will degrade individually (see the per-property \
                 logs below). Update to AMD Adrenalin 25.1.1+ for the fully-validated path \
                 (Polaris/Vega drivers stay on 1.4.31 and only get the core path)."
            );
        }
        Ok(AmfLib { factory, version })
    }
}

// Per-codec property names (v1.4.36 headers). Unknown names use `set_prop(required=false)`.
// Enum VALUES differ: CBR is 1 on AVC, 3 on HEVC/AV1; SPEED is 1 / 10 / 100; AV1 swaps
// ULTRA_LOW_LATENCY/LOW_LATENCY relative to AVC/HEVC.

/// `AMF_VIDEO_ENCODER_HEVC_HEADER_INSERTION_MODE_IDR_ALIGNED`.
const HEVC_HEADER_IDR_ALIGNED: i64 = 2;
/// `AMF_VIDEO_ENCODER_AV1_HEADER_INSERTION_MODE_KEY_FRAME_ALIGNED`.
const AV1_HEADER_KEY_ALIGNED: i64 = 2;
/// `AMF_VIDEO_ENCODER_HEVC_PROFILE_MAIN_10`.
const HEVC_PROFILE_MAIN_10: i64 = 2;
/// `AMF_COLOR_BIT_DEPTH_10` (components/ColorSpace.h).
const COLOR_BIT_DEPTH_10: i64 = 10;
/// `AMF_VIDEO_ENCODER_AV1_ALIGNMENT_MODE_NO_RESTRICTIONS` / `_64X16_1080P_CODED_1082`.
/// Driver default `64X16_ONLY` rejects heights that are not multiples of 16 (1080p).
const AV1_ALIGNMENT_NO_RESTRICTIONS: i64 = 3;
const AV1_ALIGNMENT_1080P_CODED_1082: i64 = 2;
/// `AMF_VIDEO_ENCODER_AV1_ENCODING_LATENCY_MODE_LOWEST_LATENCY`.
const AV1_LATENCY_LOWEST: i64 = 3;
// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_ENUM` (components/ColorSpace.h): studio-range 709 / 2020.
const COLOR_PROFILE_709: i64 = 1;
const COLOR_PROFILE_2020: i64 = 2;
// `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM` / `AMF_COLOR_PRIMARIES_ENUM` (CICP code points).
const TRANSFER_BT709: i64 = 1;
const TRANSFER_SMPTE2084: i64 = 16;
const PRIMARIES_BT709: i64 = 1;
const PRIMARIES_BT2020: i64 = 9;

struct CodecProps {
    /// `factory->CreateComponent` id.
    component: PCWSTR,
    usage: PCWSTR,
    rc_method: PCWSTR,
    /// `RATE_CONTROL_METHOD_CBR` — 1 on AVC, **3** on HEVC and AV1.
    rc_cbr: i64,
    target_bitrate: PCWSTR,
    peak_bitrate: PCWSTR,
    vbv_size: PCWSTR,
    enforce_hrd: PCWSTR,
    filler_data: PCWSTR,
    /// Rate-control frame skip; the latency usages default it on.
    skip_frame: PCWSTR,
    quality_preset: PCWSTR,
    /// `QUALITY_PRESET_SPEED` — 1 on AVC, **10** on HEVC, **100** on AV1.
    quality_speed: i64,
    /// AVC/HEVC: `L"LowLatencyInternal"` (bool). AV1: `Av1EncodingLatencyMode` (enum).
    lowlatency: PCWSTR,
    /// Bool `true` (AVC/HEVC) or the AV1 latency-mode enum value.
    lowlatency_value: AmfVariantKind,
    framerate: PCWSTR,
    /// AVC `IDRPeriod`, HEVC `HevcGOPSize`, AV1 `Av1GOPSize`. Value is `i32::MAX` (infinite GOP)
    /// except AV1, whose header defines **0** as "key frame at first frame only".
    idr_period: PCWSTR,
    idr_period_value: i64,
    /// Per-surface forced-keyframe: 2 = PICTURE_TYPE_IDR (AVC/HEVC), **1** = KEY (AV1).
    force_picture_type: PCWSTR,
    force_idr_value: i64,
    /// Output `*_OUTPUT_DATA_TYPE_*` / `Av1OutputFrameType`. Type ≤ `output_key_max` is a
    /// keyframe. AV1 INTRA_ONLY=1 does not reset references — not a join point.
    output_data_type: PCWSTR,
    output_key_max: i64,
    /// `QueryTimeout` (ms): how long `QueryOutput` may block. Codec-prefixed like the rest, and
    /// optional — an older runtime rejects it and the retrieve thread samples instead.
    query_timeout: PCWSTR,
    out_color_profile: PCWSTR,
    out_transfer: PCWSTR,
    out_primaries: PCWSTR,
    /// `*InHDRMetadata` (`AMFBuffer` of [`sys::AmfHdrMetadata`]). `None` on AVC — no HDR on the wire.
    hdr_metadata: Option<PCWSTR>,
    /// Intra-refresh: (units-per-slot, block edge px). AVC 16-px MBs, HEVC 64-px CTBs. `None` on
    /// AV1 (mode enum only, no slot-size control).
    intra_refresh: Option<(PCWSTR, u32)>,
    /// LTR-RFI property names, on every codec.
    ltr: Option<LtrProps>,
}

/// AMF LTR property names, codec-prefixed (AVC bare, HEVC `Hevc*`, AV1 `Av1*`). Two static at
/// open, two per-frame on the input surface.
struct LtrProps {
    /// `MaxOfLTRFrames` — user LTR slots (we request [`NUM_LTR_SLOTS`]).
    max_ltr_frames: PCWSTR,
    /// `MaxNumRefFrames` — reference-picture budget; must exceed 1 for LTR to engage.
    max_num_ref_frames: PCWSTR,
    /// `MarkCurrentWithLTRIndex` — tag this frame as long-term reference slot N.
    mark_ltr_index: PCWSTR,
    /// `ForceLTRReferenceBitfield` — reference only LTR slots in the bitfield (`1<<N`).
    force_ltr_bitfield: PCWSTR,
}

enum AmfVariantKind {
    Bool(bool),
    I64(i64),
}

impl AmfVariantKind {
    fn to_variant(&self) -> AmfVariant {
        match self {
            AmfVariantKind::Bool(b) => AmfVariant::from_bool(*b),
            AmfVariantKind::I64(v) => AmfVariant::from_i64(*v),
        }
    }
}

fn codec_props(codec: Codec) -> CodecProps {
    match codec {
        Codec::H264 => CodecProps {
            component: w!("AMFVideoEncoderVCE_AVC"),
            usage: w!("Usage"),
            rc_method: w!("RateControlMethod"),
            rc_cbr: 1,
            target_bitrate: w!("TargetBitrate"),
            peak_bitrate: w!("PeakBitrate"),
            vbv_size: w!("VBVBufferSize"),
            enforce_hrd: w!("EnforceHRD"),
            filler_data: w!("FillerDataEnable"),
            skip_frame: w!("RateControlSkipFrameEnable"),
            quality_preset: w!("QualityPreset"),
            quality_speed: 1,
            lowlatency: w!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: w!("FrameRate"),
            idr_period: w!("IDRPeriod"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: w!("ForcePictureType"),
            force_idr_value: 2,
            output_data_type: w!("OutputDataType"),
            query_timeout: w!("QueryTimeout"),
            output_key_max: 1,
            out_color_profile: w!("OutColorProfile"),
            out_transfer: w!("OutColorTransferChar"),
            out_primaries: w!("OutColorPrimaries"),
            hdr_metadata: None,
            intra_refresh: Some((w!("IntraRefreshMBsNumberPerSlot"), 16)),
            ltr: Some(LtrProps {
                max_ltr_frames: w!("MaxOfLTRFrames"),
                max_num_ref_frames: w!("MaxNumRefFrames"),
                mark_ltr_index: w!("MarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("ForceLTRReferenceBitfield"),
            }),
        },
        Codec::H265 => CodecProps {
            component: w!("AMFVideoEncoderHW_HEVC"),
            usage: w!("HevcUsage"),
            rc_method: w!("HevcRateControlMethod"),
            rc_cbr: 3,
            target_bitrate: w!("HevcTargetBitrate"),
            peak_bitrate: w!("HevcPeakBitrate"),
            vbv_size: w!("HevcVBVBufferSize"),
            enforce_hrd: w!("HevcEnforceHRD"),
            filler_data: w!("HevcFillerDataEnable"),
            skip_frame: w!("HevcRateControlSkipFrameEnable"),
            quality_preset: w!("HevcQualityPreset"),
            quality_speed: 10,
            lowlatency: w!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: w!("HevcFrameRate"),
            idr_period: w!("HevcGOPSize"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: w!("HevcForcePictureType"),
            force_idr_value: 2,
            output_data_type: w!("HevcOutputDataType"),
            query_timeout: w!("HevcQueryTimeout"),
            output_key_max: 1,
            out_color_profile: w!("HevcOutColorProfile"),
            out_transfer: w!("HevcOutColorTransferChar"),
            out_primaries: w!("HevcOutColorPrimaries"),
            hdr_metadata: Some(w!("HevcInHDRMetadata")),
            intra_refresh: Some((w!("HevcIntraRefreshCTBsNumberPerSlot"), 64)),
            ltr: Some(LtrProps {
                max_ltr_frames: w!("HevcMaxOfLTRFrames"),
                max_num_ref_frames: w!("HevcMaxNumRefFrames"),
                mark_ltr_index: w!("HevcMarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("HevcForceLTRReferenceBitfield"),
            }),
        },
        Codec::Av1 => CodecProps {
            component: w!("AMFVideoEncoderHW_AV1"),
            usage: w!("Av1Usage"),
            rc_method: w!("Av1RateControlMethod"),
            rc_cbr: 3,
            target_bitrate: w!("Av1TargetBitrate"),
            peak_bitrate: w!("Av1PeakBitrate"),
            vbv_size: w!("Av1VBVBufferSize"),
            enforce_hrd: w!("Av1EnforceHRD"),
            filler_data: w!("Av1FillerData"),
            skip_frame: w!("Av1RateControlSkipFrameEnable"),
            quality_preset: w!("Av1QualityPreset"),
            quality_speed: 100,
            lowlatency: w!("Av1EncodingLatencyMode"),
            lowlatency_value: AmfVariantKind::I64(AV1_LATENCY_LOWEST),
            framerate: w!("Av1FrameRate"),
            idr_period: w!("Av1GOPSize"),
            idr_period_value: 0,
            force_picture_type: w!("Av1ForceFrameType"),
            force_idr_value: 1,
            output_data_type: w!("Av1OutputFrameType"),
            query_timeout: w!("Av1QueryTimeout"),
            output_key_max: 0,
            out_color_profile: w!("Av1OutputColorProfile"),
            out_transfer: w!("Av1OutputColorTransferChar"),
            out_primaries: w!("Av1OutputColorPrimaries"),
            hdr_metadata: Some(w!("Av1InHDRMetadata")),
            intra_refresh: None,
            ltr: Some(LtrProps {
                max_ltr_frames: w!("Av1MaxNumLTRFrames"),
                max_num_ref_frames: w!("Av1MaxNumRefFrames"),
                mark_ltr_index: w!("Av1MarkCurrentWithLTRIndex"),
                force_ltr_bitfield: w!("Av1ForceLTRReferenceBitfield"),
            }),
        },
        Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
    }
}

/// `PUNKTFUNK_AMF_USAGE` (the knob's numbering) → `*_USAGE_ENUM`. AVC/HEVC share numbering;
/// **AV1 swaps ULTRA_LOW_LATENCY (2) and LOW_LATENCY (1)** (VideoEncoderAV1.h).
fn usage_from_knobs(codec: Codec) -> i64 {
    let av1 = codec == Codec::Av1;
    let ull = if av1 { 2 } else { 1 };
    match crate::knobs::get().amf_usage {
        1 => {
            if av1 {
                1
            } else {
                2
            }
        }
        2 => 5,
        3 => 0,
        4 => 4,
        _ => ull,
    }
}

/// User LTR slots. AMD exposes 2; rotating them keeps a pair so a loss can re-reference the newest
/// mark *before* the loss point.
const NUM_LTR_SLOTS: usize = 2;

/// LTR loss recovery is on unless `PUNKTFUNK_NO_AMF_LTR=1` or `PUNKTFUNK_INTRA_REFRESH` asked
/// for intra-refresh instead: AMF has no constrained-intra property, so the two exclude each
/// other and the operator's pick wins, as on QSV.
fn ltr_disabled() -> bool {
    crate::knobs::get().no_amf_ltr != 0
}

/// Frames between LTR marks. Default `fps/2` (~0.5 s); [`NUM_LTR_SLOTS`] then covers ~1 s of
/// recent references. `PUNKTFUNK_LTR_INTERVAL_FRAMES` overrides.
fn ltr_mark_interval(fps: u32) -> i64 {
    super::policy::ltr_interval().unwrap_or_else(|| (fps.max(2) / 2).max(1) as i64)
}

// Owned-pointer guards: Terminate before Release (amfenc.c teardown order).

/// Owned `AMFComponent*` — `Terminate` + `Release` on drop.
struct Component(*mut sys::AmfComponent);
impl Drop for Component {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null `CreateComponent` pointer this guard uniquely owns;
        // vtable calls run on the owning thread. Flush-then-Terminate-then-Release; drop once.
        unsafe {
            // Flush before Terminate: an unflushed session can occupy AMD's limited VCN slots so
            // the next Init returns AMF_OK but never emits an AU. Best-effort on a wedge.
            ((*(*self.0).vtbl).flush)(self.0);
            let tr = ((*(*self.0).vtbl).terminate)(self.0);
            if tr != sys::AMF_OK {
                tracing::debug!(
                    result = %format!("{} ({tr})", result_name(tr)),
                    "AMF component Terminate returned non-OK on drop"
                );
            }
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFContext*` — `Terminate` + `Release` on drop.
struct Ctx(*mut sys::AmfContext);
impl Drop for Ctx {
    fn drop(&mut self) {
        // SAFETY: `self.0` is the non-null `CreateContext` pointer this guard uniquely owns
        // (`Inner` declares `comp` before `ctx`, so components drop first). Drop once, owning thread.
        unsafe {
            let tr = ((*(*self.0).vtbl).terminate)(self.0);
            if tr != sys::AMF_OK {
                tracing::debug!(
                    result = %format!("{} ({tr})", result_name(tr)),
                    "AMF context Terminate returned non-OK on drop (D3D11 device unbind)"
                );
            }
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFData*` (surface or buffer viewed through the `AMFData` prefix) — `Release` on drop.
struct OwnedData(*mut sys::AmfData);
impl Drop for OwnedData {
    fn drop(&mut self) {
        // SAFETY: one owned/AddRef'd reference from CreateSurface / QueryOutput / QueryInterface.
        // `release` is slot 2 of every AMF vtable. Drop once.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Set one component property. Required: abort the open. Optional: log and continue (VCN/driver
/// variance). Returns whether it applied, so callers gate advertised caps on the driver's answer.
unsafe fn set_prop(
    comp: *mut sys::AmfComponent,
    name: PCWSTR,
    value: AmfVariant,
    required: bool,
) -> Result<bool> {
    let r = ((*(*comp).vtbl).set_property)(comp, name.0, value);
    if r == sys::AMF_OK {
        return Ok(true);
    }
    let name = String::from_utf16_lossy(name.as_wide());
    if required {
        Err(anyhow!(
            "AMF SetProperty({name}) failed: {} ({r})",
            result_name(r)
        ))
    } else {
        // INFO not debug: rejected optional props are the per-box capability matrix.
        tracing::info!(
            property = %name,
            result = result_name(r),
            amf_code = r,
            "optional AMF encoder property rejected (VCN generation/driver) — continuing"
        );
        Ok(false)
    }
}

/// `GetProperty` BOOL. `None` on decline or non-BOOL.
unsafe fn get_prop_bool(comp: *mut sys::AmfComponent, name: PCWSTR) -> Option<bool> {
    let mut v = AmfVariant::zeroed();
    let r = ((*(*comp).vtbl).get_property)(comp, name.0, &mut v);
    if r != sys::AMF_OK {
        return None;
    }
    v.as_bool()
}

/// `GetProperty` INT64 after any internal clamp. `None` on decline or non-INT64 — never treat as 0.
unsafe fn get_prop_i64(comp: *mut sys::AmfComponent, name: PCWSTR) -> Option<i64> {
    let mut v = AmfVariant::zeroed();
    let r = ((*(*comp).vtbl).get_property)(comp, name.0, &mut v);
    if r != sys::AMF_OK {
        return None;
    }
    v.as_i64()
}

/// Input texture ring depth. AMF keeps reading a slot until its AU is retrieved, so at most
/// `RING - 1` frames may be in flight. `submit` drains before reuse. Shallow enough that
/// back-pressure starts after a few frames, not after AMF's 16-deep input queue.
const RING: usize = 6;

/// Process-wide count of successful `Init`s. A climbing number with no following first-AU log
/// ([`Inner::note_first_au`]) is a silent VCN-session wedge.
static AMF_CONTEXTS_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long the retrieve thread lets `QueryOutput` block before it looks at the stop flag. The
/// only cost of a bigger number is teardown latency; the only cost of a smaller one is wake-ups
/// on an idle encoder.
const QUERY_TIMEOUT_MS: i64 = 50;

/// What the retrieve thread and the encode thread share. The component is deliberately not in
/// here: AMF documents `SubmitInput` and `QueryOutput` as a thread pair, so only the two queues
/// need a lock, and it is never held across a `QueryOutput`.
#[derive(Default)]
struct Out {
    /// `(pts_ns, forced-IDR, recovery-anchor)` in submit order — `submit` pushes, the retrieve
    /// thread pops. Its length is the surfaces AMF still holds, which is what back-pressure reads.
    pending: VecDeque<(u64, bool, bool)>,
    /// Finished AUs waiting for `poll`.
    ready: VecDeque<EncodedFrame>,
    /// First typed `QueryOutput` failure. `poll` surfaces it so the caller resets, exactly as it
    /// did when the call was on the encode thread.
    err: Option<String>,
}

/// The retrieve thread and its signal. It owns every `QueryOutput` on the component, so the
/// encode thread never waits on VCN — it takes finished AUs off a queue, and a caller that parks
/// on handles takes [`Ready`] instead.
///
/// Dropping this stops and joins, which must happen before the component is terminated under it:
/// [`Inner`] declares it first for exactly that reason, and [`AmfEncoder::reset`] stops it by
/// hand around the in-place re-Init.
struct Retrieve {
    out: Arc<Mutex<Out>>,
    have: Arc<Ready>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Retrieve {
    /// Start a thread draining `comp`. `blocking` says `QueryTimeout` took, so the loop parks in
    /// `QueryOutput` instead of sampling.
    fn start(comp: *mut sys::AmfComponent, props: &CodecProps, blocking: bool) -> Result<Self> {
        let out: Arc<Mutex<Out>> = Arc::default();
        let have = Arc::new(Ready::new().ok_or_else(|| anyhow!("AMF: no completion event"))?);
        let stop = Arc::new(AtomicBool::new(false));
        let (comp, odt, okm) = (
            comp as usize,
            props.output_data_type.0 as usize,
            props.output_key_max,
        );
        let (t_out, t_have, t_stop) = (out.clone(), have.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("punktfunk-amf-out".into())
            .spawn(move || retrieve_loop(comp, odt, okm, blocking, t_out, t_have, t_stop))
            .context("spawn AMF retrieve thread")?;
        Ok(Self {
            out,
            have,
            stop,
            join: Some(join),
        })
    }

    /// Surfaces AMF still holds — the back-pressure reading.
    fn in_flight(&self) -> usize {
        lock(&self.out).pending.len()
    }

    /// Retire the thread and wait for it to leave `QueryOutput`. Idempotent; the queues survive
    /// so a caller can inspect them, and [`Self::reset_queues`] is what empties them.
    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }

    /// Forfeit everything owed — a re-Init voids the reference chain, so the AUs behind it are
    /// no longer decodable against what the client holds.
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
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Block in `QueryOutput` and hand finished AUs to the encode thread. Pointers travel as `usize`
/// (process-global AMF handles); the thread is joined before the component is terminated, so
/// `comp` outlives every call here.
fn retrieve_loop(
    comp: usize,
    output_data_type: usize,
    output_key_max: i64,
    blocking: bool,
    out: Arc<Mutex<Out>>,
    have: Arc<Ready>,
    stop: Arc<AtomicBool>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    let comp = comp as *mut sys::AmfComponent;
    let odt = PCWSTR(output_data_type as *const u16);
    while !stop.load(Ordering::Acquire) {
        // SAFETY: `comp` is the live component this thread was started for and is joined before
        // anything terminates it; this thread makes every `QueryOutput` call on it.
        match unsafe { drain_one_output(comp, odt, output_key_max) } {
            Ok(DrainOutcome::Frame { data, key_prop }) => {
                let mut g = lock(&out);
                // An AU with no submit behind it would pair every later AU with the wrong
                // pts, keyframe flag and anchor; that is a reset, never a renumbering.
                let Some((pts_ns, forced, recovery_anchor)) = g.pending.pop_front() else {
                    g.err
                        .get_or_insert_with(|| "AMF produced an AU with no submit pending".into());
                    have.set();
                    return;
                };
                g.ready.push_back(EncodedFrame {
                    data,
                    pts_ns,
                    keyframe: key_prop || forced,
                    recovery_anchor,
                    recovery_point: false,
                    recovery_close: false,
                    chunk_aligned: false,
                });
                // Under the lock, so it cannot race the clear `poll` does when it empties.
                have.set();
            }
            // `flush` owns the queue across a drain; a clear here could land on a frame queued
            // behind the Drain. EOF repeats on every call while the component sits drained and
            // the loop only exits on `stop`, so pace it like NotReady.
            Ok(DrainOutcome::Eof) => {
                if !blocking {
                    std::thread::sleep(std::time::Duration::from_micros(250));
                }
            }
            Ok(DrainOutcome::NotReady) => {
                // Without `QueryTimeout` the call is a poll; keep the old sampling interval,
                // which now costs this thread rather than the encode thread.
                if !blocking {
                    std::thread::sleep(std::time::Duration::from_micros(250));
                }
            }
            Err(e) => {
                let mut g = lock(&out);
                g.err.get_or_insert_with(|| format!("{e:#}"));
                have.set();
                return;
            }
        }
    }
}

/// Ask the component to let `QueryOutput` block. `false` means the driver declined and the
/// retrieve thread samples instead — older AMF runtimes have no such property.
///
/// # Safety
/// `comp` is live and not yet initialized past `apply_static_props`.
unsafe fn set_query_timeout(comp: *mut sys::AmfComponent, name: PCWSTR) -> bool {
    set_prop(comp, name, AmfVariant::from_i64(QUERY_TIMEOUT_MS), false).unwrap_or(false)
}

/// Live AMF session. Field order: `retrieve` stops and joins first, then `comp` drops
/// (Flush+Terminate+Release), then `ctx`.
struct Inner {
    retrieve: Retrieve,
    comp: Component,
    ctx: Ctx,
    /// Capturer device — kept alive for the ring textures.
    _device: ID3D11Device,
    /// Immediate context for the ring copy (this encode thread only).
    dctx: ID3D11DeviceContext,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    /// A reference to every texture AMF may still be reading, newest last, capped at [`RING`].
    /// `CreateSurfaceFromDX11Native` wraps without owning, so nothing else keeps a caller's
    /// texture alive for the encode; in-flight is never more than `RING`, so the last `RING`
    /// entries always cover whatever the hardware is on. Encode thread only.
    held: VecDeque<ID3D11Texture2D>,
    /// Last `*InHDRMetadata` pushed to this component — re-push on change or rebuild.
    hdr_pushed: Option<pf_frame::HdrMeta>,
    /// Gates the one-shot first-AU log. Absence after a context-created line is a VCN wedge.
    first_au_logged: bool,
}

impl Inner {
    /// One-shot first-AU log. Pairs a context-created line with proof VCN actually encodes.
    fn note_first_au(&mut self, au: &EncodedFrame) {
        if !self.first_au_logged {
            self.first_au_logged = true;
            tracing::info!(
                bytes = au.data.len(),
                keyframe = au.keyframe,
                "AMF produced its first AU on this context"
            );
        }
    }

    /// The oldest finished AU, or the retrieve thread's failure. Clears the signal as the queue
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

    /// [`Self::pop_ready`], waiting up to `wait_ms` for the thread to produce one. The bounded
    /// wait every caller of `poll` already expected, now a handle wait rather than a sample loop.
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

/// The queue lock, poison-tolerant: a retrieve thread that panicked leaves the AUs it already
/// handed over readable, and its error field is what tells `poll` to reset.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

pub struct AmfEncoder {
    codec: Codec,
    props: CodecProps,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    /// BT.2020 PQ (HDR) vs BT.709 (SDR). Independent of `ten_bit`: 10-bit SDR is Main10 under
    /// BT.709. P010 is the ring for both, so the colour signalling follows this, not the format.
    hdr: bool,
    /// Lazy from the first frame's device; rebuilt on capturer-device change.
    inner: Option<Inner>,
    bound_device: isize,
    frame_idx: i64,
    force_kf: bool,
    /// Static HDR mastering metadata; pushed as `*InHDRMetadata` when it changes.
    hdr_meta: Option<pf_frame::HdrMeta>,
    /// Driver accepted intra-refresh — gates [`EncoderCaps::intra_refresh`].
    ir_active: bool,
    /// Driver accepted LTR at open. Mutually exclusive with intra-refresh; LTR wins.
    ltr_active: bool,
    /// Wire `frame_idx` in each LTR slot (`None` = never marked). Newest pre-loss slot is forced.
    ltr_slots: [Option<i64>; NUM_LTR_SLOTS],
    /// Next LTR mark slot (round-robin).
    next_ltr_slot: usize,
    ltr_mark_interval: i64,
    /// LTR slot the next submit must force-reference. Consumed on that submit.
    pending_force: Option<usize>,
    /// `PUNKTFUNK_LTR_FORCE_AT=N`: self-trigger [`Encoder::invalidate_ref_frames`] at that index.
    ltr_test_force_at: Option<i64>,
    /// Refuse this frame after the LTR decision, as a failed surface creation would.
    #[cfg(test)]
    fail_submit_at: Option<i64>,
    /// What the caller promised through [`Encoder::set_input_ring_depth`]: how many frames may be
    /// in flight before it reuses an input texture. `None` = never told, so the ring copy stays.
    /// See [`AmfEncoder::in_place`].
    input_ring_depth: Option<usize>,
    /// Resets with no AU since (cleared in `poll`). At 2, escalate past in-place re-Init: that
    /// reuses the same context and cannot clear a dead VCN session. Drop `inner` instead.
    resets_without_output: u32,
}

// SAFETY: raw AMF pointers and D3D11 COM handles are not auto-`Send`. The session moves the
// encoder onto one encode thread and drives it there; the immediate context is never shared.
unsafe impl Send for AmfEncoder {}

impl AmfEncoder {
    /// Open the native AMF encoder. Fails the session when the runtime is missing/too old or the
    /// capture format is not NV12/P010. AV1 is probed up front (RDNA3+; same [`probe_can_encode`]
    /// as the advertisement).
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
        // BT.2020 PQ vs BT.709. Independent of depth: 10-bit SDR is a P010 ring under BT.709. The
        // caller sets it — P010 is the input for both HDR and 10-bit SDR, so `format` cannot.
        hdr: bool,
        // Selected render adapter (`None` = OS default); the AV1 probe opens on it.
        adapter_luid: Option<LUID>,
    ) -> Result<Self> {
        let lib = try_factory().map_err(|e| anyhow!("native AMF unavailable: {e}"))?;
        tracing::debug!(
            version = %amf_version_str(lib.version),
            "opening AMF encoder"
        );
        let props = codec_props(codec);
        // AV1 is RDNA3+ — probe here so a pre-RDNA3 box fails at open, not at lazy Init.
        if codec == Codec::Av1 && !probe_can_encode(Codec::Av1, adapter_luid) {
            bail!("this GPU/driver declined AV1 encode (RDNA3+ required) — native AMF probe");
        }
        // Depth follows delivered pixels, not negotiated depth ([`crate::ten_bit_input`]).
        let ten_bit = crate::ten_bit_input(format, bit_depth);
        // Ring is NV12/P010 only. Any other capture format has no native input path.
        let expected = if ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        if format != expected {
            bail!(
                "native AMF needs the video-processor {expected:?} capture path; capturer \
                 delivered {format:?} (no readback path since Phase 3 — see the AMFVideoConverter \
                 note in §3.2)"
            );
        }
        if ten_bit && codec == Codec::H264 {
            bail!("native AMF: 10-bit is HEVC-only (H.264 High10 is not a VCN mode)");
        }
        // VCN does not encode 4:4:4. `can_encode_444` is already false; degrade, don't fail.
        if chroma.is_444() {
            tracing::warn!("AMF cannot encode 4:4:4 (VCN hardware limit) — encoding 4:2:0");
        }
        Ok(AmfEncoder {
            codec,
            props,
            width,
            height,
            fps,
            bitrate_bps,
            ten_bit,
            hdr,
            inner: None,
            bound_device: 0,
            frame_idx: 0,
            force_kf: false,
            hdr_meta: None,
            ir_active: false,
            ltr_active: false,
            ltr_slots: [None; NUM_LTR_SLOTS],
            next_ltr_slot: 0,
            ltr_mark_interval: ltr_mark_interval(fps),
            pending_force: None,
            ltr_test_force_at: ltr_test_force_at(),
            #[cfg(test)]
            fail_submit_at: None,
            input_ring_depth: None,
            resets_without_output: 0,
        })
    }

    /// Attempt LTR-RFI unless `PUNKTFUNK_NO_AMF_LTR` or the periodic wave asked otherwise. Driver
    /// accept is `ltr_active`.
    fn ltr_wanted(&self) -> bool {
        !ltr_disabled() && !super::policy::intra_refresh_requested()
    }

    /// VBV/HRD buffer (bits) at `bps`: ~1 frame interval, `PUNKTFUNK_VBV_FRAMES`-scaled.
    fn vbv_bits(&self, bps: u64) -> i64 {
        ((bps as f64 / self.fps.max(1) as f64) * crate::vbv_frames_env())
            .clamp(1.0, i32::MAX as f64) as i64
    }

    /// Static encoder config, before `Init` and again on `reset()` re-`Init` (Terminate does not
    /// keep properties on every driver). Returns `(ir_active, ltr_active)` as requested AND
    /// accepted. Mutually exclusive — see [`Self::ltr_wanted`].
    unsafe fn apply_static_props(&self, comp: *mut sys::AmfComponent) -> Result<(bool, bool)> {
        let p = &self.props;
        // Usage first: it fully configures the parameter set; everything after is an override.
        set_prop(
            comp,
            p.usage,
            AmfVariant::from_i64(usage_from_knobs(self.codec)),
            true,
        )?;
        set_prop(comp, p.rc_method, AmfVariant::from_i64(p.rc_cbr), true)?;
        let bps = self.bitrate_bps.min(i64::MAX as u64) as i64;
        set_prop(comp, p.target_bitrate, AmfVariant::from_i64(bps), true)?;
        set_prop(comp, p.peak_bitrate, AmfVariant::from_i64(bps), true)?;
        set_prop(
            comp,
            p.framerate,
            AmfVariant::from_rate(self.fps.max(1), 1),
            true,
        )?;
        set_prop(
            comp,
            p.vbv_size,
            AmfVariant::from_i64(self.vbv_bits(self.bitrate_bps)),
            false,
        )?;
        set_prop(comp, p.enforce_hrd, AmfVariant::from_bool(true), false)?;
        set_prop(comp, p.filler_data, AmfVariant::from_bool(false), false)?;
        // The latency usages default this on: a frame over the one-frame VBV is then skipped
        // and the reference stays stale, so the next frame is over budget too — a scene cut
        // freezes the picture until the content drifts back to it.
        let usage_default = get_prop_bool(comp, p.skip_frame);
        set_prop(comp, p.skip_frame, AmfVariant::from_bool(false), false)?;
        tracing::info!(?usage_default, "AMF rate-control frame skip disabled");
        // Latency-first quality; low-latency submit (optional on older VCN).
        set_prop(
            comp,
            p.quality_preset,
            AmfVariant::from_i64(p.quality_speed),
            false,
        )?;
        set_prop(comp, p.lowlatency, p.lowlatency_value.to_variant(), false)?;
        // No periodic IDR (`i32::MAX` AVC/HEVC; 0 on AV1 = first frame only). Forced type supplies IDRs.
        set_prop(
            comp,
            p.idr_period,
            AmfVariant::from_i64(p.idr_period_value),
            false,
        )?;
        // Intra-refresh: per-slot units = ceil(total blocks / period). Optional; gates `caps()`.
        let mut ir_active = false;
        let mut ltr_active = false;
        if let Some(ltr) = p.ltr.as_ref().filter(|_| self.ltr_wanted()) {
            // LTR needs >1 ref frames and is mutually exclusive with intra-refresh.
            let ref_ok = set_prop(
                comp,
                ltr.max_num_ref_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            let ltr_ok = set_prop(
                comp,
                ltr.max_ltr_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            ltr_active = ref_ok && ltr_ok;
            if ltr_active {
                tracing::info!(
                    slots = NUM_LTR_SLOTS,
                    mark_interval = self.ltr_mark_interval,
                    "AMF LTR-RFI recovery enabled (loss recovery re-references a known-good LTR, not a full IDR)"
                );
            } else {
                tracing::warn!(
                    ref_ok,
                    ltr_ok,
                    "this VCN/driver rejected an LTR property — loss recovery stays full-IDR"
                );
            }
        } else if let Some((name, block)) = p.intra_refresh {
            if intra_refresh_requested() {
                let period = intra_refresh_period(self.fps);
                let blocks = self.width.div_ceil(block) * self.height.div_ceil(block);
                let per_slot = blocks.div_ceil(period).max(1);
                ir_active = set_prop(comp, name, AmfVariant::from_i64(per_slot as i64), false)?;
                if ir_active {
                    tracing::info!(
                        period_frames = period,
                        units_per_slot = per_slot,
                        "AMF intra-refresh wave enabled (keyframe requests will be rate-limited)"
                    );
                } else {
                    tracing::warn!(
                        "PUNKTFUNK_INTRA_REFRESH requested but this VCN/driver rejected the \
                         intra-refresh property — loss recovery stays full-IDR"
                    );
                }
            }
        }
        match self.codec {
            Codec::H264 => {
                // Never B-frames: a full frame of latency each (RDNA3+ defaults > 0).
                set_prop(comp, w!("BPicturesPattern"), AmfVariant::from_i64(0), false)?;
                // Limited-range YUV (matches the video processor's NV12).
                set_prop(
                    comp,
                    w!("FullRangeColor"),
                    AmfVariant::from_bool(false),
                    false,
                )?;
            }
            Codec::H265 => {
                // In-band VPS/SPS/PPS on every IDR. Forced-IDR surfaces also set `HevcInsertHeader`.
                set_prop(
                    comp,
                    w!("HevcHeaderInsertionMode"),
                    AmfVariant::from_i64(HEVC_HEADER_IDR_ALIGNED),
                    false,
                )?;
                // Studio range, matching NV12/P010 video-processor output.
                set_prop(comp, w!("HevcNominalRange"), AmfVariant::from_i64(0), false)?;
                if self.ten_bit {
                    // Main10 + 10-bit surfaces: required — silent 8-bit HDR is worse than failing open.
                    set_prop(
                        comp,
                        w!("HevcProfile"),
                        AmfVariant::from_i64(HEVC_PROFILE_MAIN_10),
                        true,
                    )?;
                    set_prop(
                        comp,
                        w!("HevcColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
            Codec::Av1 => {
                // Never B-frames: VCN5 can grow them (H.264 already did on RDNA3+). A B-frame
                // adds a frame of latency and breaks FIFO on the codec with no LTR/IR. Pre-VCN5
                // rejects the names (no-op). HEVC has no B-frame property at all.
                set_prop(
                    comp,
                    w!("Av1BPicturesPattern"),
                    AmfVariant::from_i64(0),
                    false,
                )?;
                set_prop(
                    comp,
                    w!("Av1MaxConsecutiveBPictures"),
                    AmfVariant::from_i64(0),
                    false,
                )?;
                set_prop(
                    comp,
                    w!("Av1AdaptiveMiniGop"),
                    AmfVariant::from_bool(false),
                    false,
                )?;
                // Sequence header OBU on every key frame (self-contained join points).
                set_prop(
                    comp,
                    w!("Av1HeaderInsertionMode"),
                    AmfVariant::from_i64(AV1_HEADER_KEY_ALIGNED),
                    false,
                )?;
                // Default `64X16_ONLY` rejects non-16-multiple heights (1080p). Prefer unrestricted;
                // fall back to 1080p-coded-1082. If neither applies, Init fails.
                let unrestricted = set_prop(
                    comp,
                    w!("Av1AlignmentMode"),
                    AmfVariant::from_i64(AV1_ALIGNMENT_NO_RESTRICTIONS),
                    false,
                )?;
                if !unrestricted && self.height % 16 != 0 {
                    set_prop(
                        comp,
                        w!("Av1AlignmentMode"),
                        AmfVariant::from_i64(AV1_ALIGNMENT_1080P_CODED_1082),
                        false,
                    )?;
                }
                if self.ten_bit {
                    // 10-bit is AV1 Main — only the surface depth needs forcing.
                    set_prop(
                        comp,
                        w!("Av1ColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
            Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
        }
        // BT.709 limited (SDR, either depth) or BT.2020 PQ (HDR). Keyed on colour, not depth:
        // 10-bit SDR is Main10 under BT.709. Required for HDR — missing PQ washes out; for SDR the
        // set is best-effort (the 8-bit arm always was), so the fatal flag is `hdr`, not `ten_bit`.
        let (profile, transfer, primaries) = if self.hdr {
            (COLOR_PROFILE_2020, TRANSFER_SMPTE2084, PRIMARIES_BT2020)
        } else {
            (COLOR_PROFILE_709, TRANSFER_BT709, PRIMARIES_BT709)
        };
        set_prop(
            comp,
            p.out_color_profile,
            AmfVariant::from_i64(profile),
            self.hdr,
        )?;
        set_prop(
            comp,
            p.out_transfer,
            AmfVariant::from_i64(transfer),
            self.hdr,
        )?;
        set_prop(
            comp,
            p.out_primaries,
            AmfVariant::from_i64(primaries),
            self.hdr,
        )?;
        Ok((ir_active, ltr_active))
    }

    /// Build or rebuild the AMF context + component on the capturer's device, plus the input ring.
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
        let lib = try_factory().map_err(|e| anyhow!("native AMF unavailable: {e}"))?;
        // SAFETY: `lib.factory` is live (gated above). CreateContext/CreateComponent fill
        // out-pointers only on AMF_OK (null-checked); each object moves into a guard so early
        // `?` releases once. `InitDX11` borrows a live `ID3D11Device`; AMF AddRefs until Terminate.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            amf_ok(
                ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx),
                "AMF CreateContext",
            )?;
            if ctx.is_null() {
                bail!("AMF CreateContext returned null");
            }
            let ctx = Ctx(ctx);
            amf_ok(
                ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1),
                "AMF InitDX11 (capturer device)",
            )?;
            let mut comp: *mut sys::AmfComponent = ptr::null_mut();
            amf_ok(
                ((*(*lib.factory).vtbl).create_component)(
                    lib.factory,
                    ctx.0,
                    self.props.component.0,
                    &mut comp,
                ),
                "AMF CreateComponent",
            )?;
            if comp.is_null() {
                bail!("AMF CreateComponent returned null");
            }
            let comp = Component(comp);
            let (ir_active, ltr_active) = self.apply_static_props(comp.0)?;
            let fmt = if self.ten_bit {
                sys::AMF_SURFACE_P010
            } else {
                sys::AMF_SURFACE_NV12
            };
            amf_ok(
                ((*(*comp.0).vtbl).init)(comp.0, fmt, self.width as i32, self.height as i32),
                "AMF encoder Init",
            )?;
            self.ir_active = ir_active;
            // Rebuilt component has no reference history; drop prior LTR marks.
            self.ltr_active = ltr_active;
            if ltr_active {
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
            }

            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width,
                Height: self.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: if self.ten_bit {
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
            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                let mut t: Option<ID3D11Texture2D> = None;
                device
                    .CreateTexture2D(&desc, None, Some(&mut t))
                    .context("CreateTexture2D (AMF input ring)")?;
                ring.push(t.context("AMF input ring texture")?);
            }
            let dctx = device
                .GetImmediateContext()
                .context("ID3D11Device immediate context")?;
            // Bump after successful Init so a failed bring-up never counts.
            let context_no =
                AMF_CONTEXTS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            tracing::info!(
                codec = ?self.codec,
                context = context_no,
                device = %format_args!("{:#x}", device.as_raw() as usize),
                width = self.width,
                height = self.height,
                fps = self.fps,
                ring = if self.ten_bit { "P010" } else { "NV12" },
                ltr = ltr_active,
                intra_refresh = ir_active,
                runtime = %format_args!(
                    "{}.{}.{}",
                    (lib.version >> 48) & 0xffff,
                    (lib.version >> 32) & 0xffff,
                    (lib.version >> 16) & 0xffff
                ),
                "native AMF encode active (zero-copy D3D11)"
            );
            // The retrieve thread starts against the initialized component and is joined before
            // anything terminates it (`Inner` drops it first; `reset` stops it by hand).
            let blocking = set_query_timeout(comp.0, self.props.query_timeout);
            let retrieve = Retrieve::start(comp.0, &self.props, blocking)?;
            tracing::debug!(
                blocking,
                "AMF retrieve thread started ({})",
                if blocking {
                    "QueryOutput blocks on the driver's own timeout"
                } else {
                    "runtime declined QueryTimeout — the thread samples"
                }
            );
            self.inner = Some(Inner {
                retrieve,
                comp,
                ctx,
                _device: device.clone(),
                dctx,
                ring,
                next: 0,
                held: VecDeque::new(),
                hdr_pushed: None,
                first_au_logged: false,
            });
            Ok(())
        }
    }
}

/// Push HDR mastering metadata as `*InHDRMetadata` (dynamic). Units match [`HdrMeta`]; primary
/// order is the trap: ST.2086 wire is G,B,R → labeled R/G/B fields.
///
/// # Safety
/// `ctx` and `comp` are the live pair owned by the calling encoder, encode thread only.
unsafe fn push_hdr_metadata(
    ctx: *mut sys::AmfContext,
    comp: *mut sys::AmfComponent,
    name: PCWSTR,
    meta: &pf_frame::HdrMeta,
) -> Result<()> {
    let mut buf: *mut sys::AmfBuffer = ptr::null_mut();
    amf_ok(
        ((*(*ctx).vtbl).alloc_buffer)(
            ctx,
            sys::AMF_MEMORY_HOST,
            std::mem::size_of::<sys::AmfHdrMetadata>(),
            &mut buf,
        ),
        "AMF AllocBuffer(HDR metadata)",
    )?;
    if buf.is_null() {
        bail!("AMF AllocBuffer(HDR metadata) returned null");
    }
    // AMFData-prefix guard (slot 2 is Release). SetProperty AddRefs; our drop leaves the property.
    let guard = OwnedData(buf as *mut sys::AmfData);
    let native = ((*(*buf).vtbl).get_native)(buf) as *mut sys::AmfHdrMetadata;
    if native.is_null() {
        bail!("AMF HDR metadata buffer has no host pointer");
    }
    // Host AMFBuffer heap alignment is unknown — write unaligned.
    native.write_unaligned(sys::AmfHdrMetadata {
        red_primary: meta.display_primaries[2],
        green_primary: meta.display_primaries[0],
        blue_primary: meta.display_primaries[1],
        white_point: meta.white_point,
        max_mastering_luminance: meta.max_display_mastering_luminance,
        min_mastering_luminance: meta.min_display_mastering_luminance,
        max_content_light_level: meta.max_cll,
        max_frame_average_light_level: meta.max_fall,
    });
    let r = ((*(*comp).vtbl).set_property)(
        comp,
        name.0,
        AmfVariant::from_interface(guard.0 as *mut c_void),
    );
    amf_ok(r, "AMF SetProperty(InHDRMetadata)")
}

/// Can this GPU's AMF runtime `Init` a `codec` encoder on the selected render adapter?
/// Tears down before return. `false` on any failure, including no runtime.
pub fn probe_can_encode(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    let Some(device) = selected_adapter_device(adapter_luid) else {
        return false;
    };
    probe_can_encode_on(&device, codec)
}

/// [`probe_can_encode`] on an explicit device (live tests pin the AMD adapter on a hybrid box).
fn probe_can_encode_on(device: &ID3D11Device, codec: Codec) -> bool {
    probe_open_on(device, codec, false)
}

/// Can this GPU `Init` `codec` at 10-bit (Main10 / `*ColorBitDepth` 10, P010)? H.264 is always
/// false (High10 is not a VCN mode).
pub fn probe_can_encode_10bit(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    let Some(device) = selected_adapter_device(adapter_luid) else {
        return false;
    };
    probe_open_on(&device, codec, true)
}

/// Probe body: context + component + usage + optional 10-bit props + tiny `Init`. `false` on fail.
fn probe_open_on(device: &ID3D11Device, codec: Codec, ten_bit: bool) -> bool {
    if try_factory().is_err() {
        return false;
    }
    let props = codec_props(codec);
    // SAFETY: factory is live; each created object moves into a guard so early return releases
    // once. `InitDX11` borrows `device`; AMF AddRefs until Terminate. Usage must be set before
    // `Init` (header default is N/A).
    unsafe {
        let Ok(lib) = try_factory() else { return false };
        let mut ctx: *mut sys::AmfContext = ptr::null_mut();
        if ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx) != sys::AMF_OK
            || ctx.is_null()
        {
            return false;
        }
        let ctx = Ctx(ctx);
        if ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1) != sys::AMF_OK {
            return false;
        }
        let mut comp: *mut sys::AmfComponent = ptr::null_mut();
        if ((*(*lib.factory).vtbl).create_component)(
            lib.factory,
            ctx.0,
            props.component.0,
            &mut comp,
        ) != sys::AMF_OK
            || comp.is_null()
        {
            return false;
        }
        let comp = Component(comp);
        if ((*(*comp.0).vtbl).set_property)(
            comp.0,
            props.usage.0,
            AmfVariant::from_i64(usage_from_knobs(codec)),
        ) != sys::AMF_OK
        {
            return false;
        }
        if ten_bit {
            // Same required 10-bit props as a real session — reject here is the probe's answer.
            let depth_props: &[(PCWSTR, i64)] = match codec {
                Codec::H265 => &[
                    (w!("HevcProfile"), HEVC_PROFILE_MAIN_10),
                    (w!("HevcColorBitDepth"), COLOR_BIT_DEPTH_10),
                ],
                Codec::Av1 => &[(w!("Av1ColorBitDepth"), COLOR_BIT_DEPTH_10)],
                Codec::H264 | Codec::PyroWave => return false,
            };
            for (name, value) in depth_props {
                if ((*(*comp.0).vtbl).set_property)(comp.0, name.0, AmfVariant::from_i64(*value))
                    != sys::AMF_OK
                {
                    return false;
                }
            }
        }
        let surface = if ten_bit {
            sys::AMF_SURFACE_P010
        } else {
            sys::AMF_SURFACE_NV12
        };
        ((*(*comp.0).vtbl).init)(comp.0, surface, 640, 480) == sys::AMF_OK
    }
}

/// D3D11 device on the selected render adapter; OS default hardware adapter if unresolved.
fn selected_adapter_device(adapter_luid: Option<LUID>) -> Option<ID3D11Device> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0,
    };
    use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
    // SAFETY: probe owns every handle. Factory/adapter COM objects or err → default fallback.
    // `D3D11CreateDevice` fills `device` only on success. Everything drops with its COM wrapper.
    unsafe {
        let adapter: Option<IDXGIAdapter1> = adapter_luid.and_then(|luid| {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            factory.EnumAdapterByLuid(luid).ok()
        });
        let mut device: Option<ID3D11Device> = None;
        let created = match &adapter {
            Some(a) => D3D11CreateDevice(
                a,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
            None => D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            ),
        };
        if created.is_err() {
            return None;
        }
        device
    }
}

enum DrainOutcome {
    /// Finished AU bytes and the driver's own keyframe verdict. The caller pairs it with the
    /// oldest `pending` entry — which it does under the queue lock, never across `QueryOutput`.
    Frame { data: Vec<u8>, key_prop: bool },
    /// No output yet (AMF_OK / AMF_REPEAT / AMF_NEED_MORE_INPUT with null data).
    NotReady,
    /// End of stream after `Drain`/`Flush` (AMF_EOF).
    Eof,
}

/// One `QueryOutput`. Blocks up to the component's `QueryTimeout` when [`set_query_timeout`]
/// took, else returns [`DrainOutcome::NotReady`] at once.
///
/// # Safety
/// `comp` is live and only the retrieve thread calls this on it — AMF documents `SubmitInput`
/// and `QueryOutput` as a submit/retrieve thread pair, which is the whole reason this is a free
/// fn taking a raw pointer.
unsafe fn drain_one_output(
    comp: *mut sys::AmfComponent,
    output_data_type: PCWSTR,
    output_key_max: i64,
) -> Result<DrainOutcome> {
    // SAFETY: `QueryOutput` fills `data` with an owned ref only when it returns one; non-null
    // moves into `OwnedData`. `QueryInterface(IID_AMFBuffer)` AddRefs (slot-2 release). Host
    // memory is valid until buffer release: copy to `Vec` before the guards drop.
    let mut data: *mut sys::AmfData = ptr::null_mut();
    let r = ((*(*comp).vtbl).query_output)(comp, &mut data);
    if data.is_null() {
        return match r {
            sys::AMF_EOF => Ok(DrainOutcome::Eof),
            sys::AMF_OK | sys::AMF_REPEAT | sys::AMF_NEED_MORE_INPUT => Ok(DrainOutcome::NotReady),
            // Typed failure on this frame (device-lost, …) — caller resets in place.
            other => bail!("AMF QueryOutput failed: {} ({other})", result_name(other)),
        };
    }
    let data = OwnedData(data);
    // Keyframe from output type, OR the forced flag so a driver that skips the property still flags.
    let mut var = AmfVariant::zeroed();
    let key_prop = ((*(*data.0).vtbl).get_property)(data.0, output_data_type.0, &mut var)
        == sys::AMF_OK
        && var.as_i64().is_some_and(|t| t <= output_key_max);
    let mut buf: *mut c_void = ptr::null_mut();
    amf_ok(
        ((*(*data.0).vtbl).query_interface)(data.0, &sys::IID_AMF_BUFFER, &mut buf),
        "AMF QueryInterface(AMFBuffer)",
    )?;
    if buf.is_null() {
        bail!("AMF output is not an AMFBuffer");
    }
    // AMFData-prefix guard: slot 2 is Release on every vtable.
    let buf_guard = OwnedData(buf as *mut sys::AmfData);
    let buf = buf_guard.0 as *mut sys::AmfBuffer;
    let size = ((*(*buf).vtbl).get_size)(buf);
    let native = ((*(*buf).vtbl).get_native)(buf);
    if native.is_null() || size == 0 {
        bail!("AMF output buffer is empty");
    }
    let data = std::slice::from_raw_parts(native as *const u8, size).to_vec();
    Ok(DrainOutcome::Frame { data, key_prop })
}

/// How long `submit` drains for a free input slot before declaring a wedge. Above one frame's
/// encode time, far under the session watchdog's ~2 s floor.
const INPUT_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

impl AmfEncoder {
    /// Whether to hand AMF the caller's texture instead of copying it into [`Inner::ring`], and
    /// how many frames may then be in flight.
    ///
    /// Encoding in place is always *sound* while in-flight stays inside the caller's declared
    /// depth — that is what [`Encoder::set_input_ring_depth`] promises. It is only worth doing at
    /// a depth of 2 or more: the copy is what decouples AMF's pipeline from the caller's ring, so
    /// at depth 1 dropping it would serialise submit against the encode and cost more than the
    /// copy does. An undeclared depth keeps the copy — a caller that never promised anything may
    /// reuse its texture the moment `submit` returns.
    fn in_place(&self) -> Option<usize> {
        self.input_ring_depth
            .filter(|&d| d >= 2)
            .map(|d| d.min(RING))
    }
}

impl AmfEncoder {
    /// [`Encoder::submit`] without the failure rule: the LTR mirror and a queued force are
    /// committed before the component takes the frame, so a refusal leaves them one frame ahead
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
                bail!("native AMF is D3D11-only; got a CPU frame (video processor lost?)")
            }
        };
        // Mid-session format fallback: CopySubresourceRegion across format groups is UB. No readback.
        let expected = if self.ten_bit {
            PixelFormat::P010
        } else {
            PixelFormat::Nv12
        };
        anyhow::ensure!(
            captured.format == expected,
            "captured format {:?} != AMF input ring {:?} (capturer video-processor fallback \
             mid-session — native AMF has no readback path)",
            captured.format,
            expected
        );
        self.ensure_inner(&frame.device)?;
        let cur_idx = self.frame_idx;
        // First submit on a component must be a forced IDR. Use the ring counter, not
        // `frame_idx == 0`: `submit_indexed` pins wire indexes that are non-zero after a rebuild.
        let opening = self.inner.as_ref().is_none_or(|i| i.next == 0);
        let mut forced = std::mem::take(&mut self.force_kf) || opening;
        let pts_100ns = self.frame_idx * 10_000_000 / self.fps.max(1) as i64;
        self.frame_idx += 1;
        // LTR decisions before borrowing `inner`: the test hook re-enters `&mut self`, and
        // PCWSTR copies let the surface block set props without re-borrowing `self.props`.
        let ltr_names = self
            .props
            .ltr
            .as_ref()
            .map(|l| (l.mark_ltr_index, l.force_ltr_bitfield));
        let mut mark_slot: Option<usize> = None;
        let mut force_slot: Option<usize> = None;
        let mut recovery_anchor = false;
        if self.ltr_active {
            if forced {
                // IDR resets decoder refs — drop stale LTR slots and any force queued against them.
                self.ltr_slots = [None; NUM_LTR_SLOTS];
                self.next_ltr_slot = 0;
                self.pending_force = None;
            } else if self.ltr_test_force_at == Some(cur_idx) {
                // Spike hook: self-trigger the real invalidate path without a live client.
                let triggered = self.invalidate_ref_frames(cur_idx, cur_idx);
                tracing::info!(
                    frame = cur_idx,
                    triggered,
                    "AMF LTR test hook fired invalidate_ref_frames"
                );
            }
            // Apply a queued force to this frame. Skip if the taint sweep emptied the slot: the
            // hardware still holds the tainted mark, so forcing it would re-reference the loss.
            if let Some(slot) = self.pending_force.take() {
                if self.ltr_slots[slot].is_some() {
                    force_slot = Some(slot);
                    recovery_anchor = true;
                    // LTR_MODE_RESET_UNUSED, the default: referencing one slot discards the rest.
                    for (s, marked) in self.ltr_slots.iter_mut().enumerate() {
                        if s != slot {
                            *marked = None;
                        }
                    }
                }
            }
            // Mark on IDR and every interval, never on the recovery frame (would overwrite the force).
            if force_slot.is_none() && (forced || cur_idx % self.ltr_mark_interval == 0) {
                let trusted = self.ltr_slots.map(|m| m.is_some());
                let slot = super::rfi::mark_slot(&trusted, self.next_ltr_slot);
                self.ltr_slots[slot] = Some(cur_idx);
                self.next_ltr_slot = (slot + 1) % NUM_LTR_SLOTS;
                mark_slot = Some(slot);
            }
        }
        #[cfg(test)]
        if self.fail_submit_at == Some(cur_idx) {
            bail!("test hook: frame {cur_idx} refused after the LTR decision");
        }
        let in_place = self.in_place();
        let inner = self.inner.as_mut().expect("ensure_inner succeeded");
        // Re-push HDR metadata on change or rebuild. Best-effort: reject leaves the 0xCE datagram.
        if let Some(name) = self.props.hdr_metadata {
            if self.hdr && inner.hdr_pushed != self.hdr_meta {
                if let Some(m) = self.hdr_meta {
                    // SAFETY: live context/component pair, encode thread (`push_hdr_metadata`).
                    match unsafe { push_hdr_metadata(inner.ctx.0, inner.comp.0, name, &m) } {
                        Ok(()) => tracing::debug!(
                            "AMF HDR mastering metadata attached (in-band on keyframes)"
                        ),
                        Err(e) => tracing::warn!(
                            error = %format!("{e:#}"),
                            "AMF rejected the HDR mastering metadata — no in-band SEI/OBU"
                        ),
                    }
                }
                inner.hdr_pushed = self.hdr_meta;
            }
        }
        // Bound in-flight below RING before reuse: AMF keeps reading a slot until its AU is
        // retrieved. Drain finished AUs into `ready` rather than overwrite or treat INPUT_FULL as
        // a wedge. No progress for the whole budget is a genuine wedge.
        // In place, the caller's declared depth is the bound; copying, it is our own ring.
        let cap = in_place.unwrap_or(RING);
        if inner.retrieve.in_flight() >= cap {
            let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
            // The retrieve thread is what frees a slot now; this only waits for it, and a whole
            // budget with no progress is the same wedge it always was.
            while inner.retrieve.in_flight() >= cap {
                if let Some(e) = lock(&inner.retrieve.out).err.take() {
                    bail!("{e}");
                }
                if std::time::Instant::now() >= deadline {
                    bail!(
                        "AMF produced no output for {} ms with {} frame(s) in flight — \
                         wedged (escalating to reset)",
                        INPUT_DRAIN_BUDGET.as_millis(),
                        inner.retrieve.in_flight()
                    );
                }
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
        }
        let slot = inner.next % RING;
        inner.next += 1;
        // SAFETY: `src`/`dst` are same-format, same-size, same-device (ring rebuilt on device
        // change). `CopySubresourceRegion` on this thread's immediate context is a valid GPU copy.
        // `CreateSurfaceFromDX11Native` wraps without owning (null observer); the surface moves
        // into `OwnedData`. AMF AddRefs what it keeps, so our release does not free a buffer in flight.
        unsafe {
            // The texture the hardware will read: the caller's own when it declared a depth deep
            // enough to leave it alone, else our copy of it.
            let source = if in_place.is_some() {
                frame.texture.clone()
            } else {
                let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
                let dst: ID3D11Resource = inner.ring[slot].cast().context("ring -> resource")?;
                inner
                    .dctx
                    .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None);
                inner.ring[slot].clone()
            };
            // Nothing else keeps it alive for the encode (see `Inner::held`).
            inner.held.push_back(source.clone());
            while inner.held.len() > RING {
                inner.held.pop_front();
            }

            let mut surf: *mut sys::AmfData = ptr::null_mut();
            amf_ok(
                ((*(*inner.ctx.0).vtbl).create_surface_from_dx11_native)(
                    inner.ctx.0,
                    source.as_raw(),
                    &mut surf,
                    ptr::null_mut(),
                ),
                "AMF CreateSurfaceFromDX11Native",
            )?;
            if surf.is_null() {
                bail!("AMF CreateSurfaceFromDX11Native returned null");
            }
            let surf = OwnedData(surf);
            ((*(*surf.0).vtbl).set_pts)(surf.0, pts_100ns);
            if forced {
                // Forced IDR/KEY + in-band headers. Log-and-continue: reject still encodes.
                let r = ((*(*surf.0).vtbl).set_property)(
                    surf.0,
                    self.props.force_picture_type.0,
                    AmfVariant::from_i64(self.props.force_idr_value),
                );
                if r != sys::AMF_OK {
                    tracing::warn!(
                        result = result_name(r),
                        amf_code = r,
                        "AMF forced-keyframe picture type rejected"
                    );
                    // Only the component's first frame is an IDR without the property; flagging
                    // any other AU a keyframe hands the client a join point that is not one.
                    forced = opening;
                }
                match self.codec {
                    Codec::H264 => {
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("InsertSPS").0,
                            AmfVariant::from_bool(true),
                        );
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("InsertPPS").0,
                            AmfVariant::from_bool(true),
                        );
                    }
                    Codec::H265 => {
                        let _ = ((*(*surf.0).vtbl).set_property)(
                            surf.0,
                            w!("HevcInsertHeader").0,
                            AmfVariant::from_bool(true),
                        );
                    }
                    // KEY_FRAME_ALIGNED already puts a sequence header OBU on every key frame.
                    Codec::Av1 => {}
                    Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
                }
            }
            // LTR mark/force decided above. Best-effort: reject leaves the client on IDR fallback.
            if let Some((mark_name, force_name)) = ltr_names {
                if let Some(slot) = mark_slot {
                    let r = ((*(*surf.0).vtbl).set_property)(
                        surf.0,
                        mark_name.0,
                        AmfVariant::from_i64(slot as i64),
                    );
                    if r != sys::AMF_OK {
                        tracing::warn!(
                            slot,
                            result = result_name(r),
                            amf_code = r,
                            "AMF LTR mark rejected"
                        );
                        // The mirror must not claim a slot the hardware never marked.
                        self.ltr_slots[slot] = None;
                    }
                }
                if let Some(slot) = force_slot {
                    let r = ((*(*surf.0).vtbl).set_property)(
                        surf.0,
                        force_name.0,
                        AmfVariant::from_i64(1_i64 << slot),
                    );
                    if r == sys::AMF_OK {
                        tracing::info!(
                            slot,
                            frame = cur_idx,
                            "AMF LTR-RFI: re-referencing known-good LTR (clean recovery, no IDR)"
                        );
                    } else {
                        tracing::warn!(
                            slot,
                            result = result_name(r),
                            amf_code = r,
                            "AMF LTR force-reference rejected — forcing an IDR on the next frame"
                        );
                        // The host booked a recovery on this frame; make the next one real.
                        self.force_kf = true;
                    }
                }
            }
            // Queued before the component takes the frame: the retrieve thread can pop for it the
            // moment SubmitInput returns, so a push after that races an empty queue. A refusal
            // below takes the entry back off.
            lock(&inner.retrieve.out)
                .pending
                .push_back((captured.pts_ns, forced, recovery_anchor));
            let mut r = ((*(*inner.comp.0).vtbl).submit_input)(inner.comp.0, surf.0);
            // AMF_INPUT_FULL is "busy, drain and retry", not a wedge. Re-submit the same surface.
            if r == sys::AMF_INPUT_FULL {
                let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
                loop {
                    // The retrieve thread drains; this only re-offers the same surface until a
                    // slot opens, on the same budget the drain loop used to run on.
                    std::thread::sleep(std::time::Duration::from_micros(250));
                    r = ((*(*inner.comp.0).vtbl).submit_input)(inner.comp.0, surf.0);
                    if r != sys::AMF_INPUT_FULL || std::time::Instant::now() >= deadline {
                        break;
                    }
                }
            }
            // NEED_MORE_INPUT = accepted; no AU owed for this submit alone.
            if !matches!(r, sys::AMF_OK | sys::AMF_NEED_MORE_INPUT) {
                lock(&inner.retrieve.out).pending.pop_back();
                if r == sys::AMF_INPUT_FULL {
                    bail!("AMF SubmitInput stayed AMF_INPUT_FULL past the drain budget — wedged");
                }
                bail!("AMF SubmitInput failed: {} ({r})", result_name(r));
            }
        }
        Ok(())
    }
}

impl Encoder for AmfEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let submitted = self.try_submit(captured);
        // A frame the component never took: the next one is an IDR, which resets the LTR
        // mirror and any queued force to what the hardware holds.
        if submitted.is_err() {
            self.force_kf = true;
        }
        submitted
    }

    /// Pin `frame_idx` to the wire index so LTR slots compare against client frame numbers across
    /// rebuilds. An internal counter desyncs on the first bitrate rebuild and can force an LTR
    /// marked inside the lost range.
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

    /// Force the next submit to re-reference the newest LTR marked before `[first, last]`.
    /// `true` = usable pre-loss LTR (caller must not also IDR); `false` = fall back to keyframe.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        // No live LTR (driver declined, or AV1) or a nonsense range → caller IDRs.
        if !self.ltr_active || first < 0 || first > last {
            return false;
        }
        // Policy is `rfi::plan_slot_recovery`; mechanism is clear the mirror slot. Slots store
        // wire indexes (`submit_indexed`) so they compare against client `first` across rebuilds.
        let view: Vec<(usize, i64)> = self
            .ltr_slots
            .iter()
            .enumerate()
            .filter_map(|(s, m)| m.map(|w| (s, w)))
            .collect();
        let plan = super::rfi::plan_slot_recovery(&view, first);
        for (slot, marked) in self.ltr_slots.iter_mut().enumerate() {
            if plan.tainted & (1 << slot) != 0 {
                *marked = None;
            }
        }
        match plan.anchor {
            Some((slot, ltr_frame)) => {
                // Next submit force-references this slot and ships `recovery_anchor`.
                self.pending_force = Some(slot);
                tracing::info!(
                    first,
                    last,
                    slot,
                    ltr_frame,
                    "AMF LTR-RFI: forcing the next frame to re-reference a known-good LTR (no IDR)"
                );
                true
            }
            None => {
                // Sweep may have emptied a queued force's slot — don't force a tainted hardware slot.
                self.pending_force = None;
                tracing::info!(
                    first,
                    last,
                    "AMF LTR-RFI: no live LTR older than the loss — falling back to IDR recovery"
                );
                false
            }
        }
    }

    /// Clear every LTR mirror slot and any queued force (would otherwise re-reference the taint).
    fn distrust_references(&mut self) {
        let live = self.ltr_slots.iter().filter(|m| m.is_some()).count();
        if live == 0 && self.pending_force.is_none() {
            return;
        }
        self.ltr_slots = [None; NUM_LTR_SLOTS];
        self.pending_force = None;
        tracing::debug!(
            live,
            "AMF LTR-RFI: client reported unrepaired damage — withdrawing anchor trust from every \
             live LTR (the marking cadence re-marks a clean frame within ~1/4 s)"
        );
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            blends_cursor: false,
            supports_rfi: self.ltr_active,
            chroma_444: false,
            intra_refresh: self.ir_active,
            // AMF emits no recovery-point SEI; host keeps the IDR path.
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
            downscales_input: false,
            crops_input: false,
        }
    }

    /// Bounded-blocking poll: spin `QueryOutput` with ~250 µs sleeps up to
    /// `min(3/4 frame interval, 12 ms)`. Expiry is `Ok(None)` — watchdog arbitrates a real wedge.
    /// Hands out `submit`'s buffered AUs first.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        // Scope the inner borrow so a produced AU can clear `resets_without_output` on `self`.
        let au = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(None);
            };
            // The same bound as before, now spent on the retrieve thread's event rather than on
            // a sample loop, so nothing else on this thread waits behind it.
            let budget_ms = (750 / self.fps.max(1)).clamp(1, 12);
            let au = inner.take_ready(budget_ms)?;
            if let Some(au) = &au {
                inner.note_first_au(au);
            }
            au
        };
        // Any AU proves this context encodes — reset the no-output streak.
        if au.is_some() {
            self.resets_without_output = 0;
        }
        Ok(au)
    }

    /// The retrieve thread's signal, once a component exists. Before the lazy open there is
    /// nothing to wait on, which a caller reads as "no completion signal" and polls instead.
    fn ready_event(&self) -> Option<isize> {
        self.inner.as_ref().map(|i| i.retrieve.have.raw())
    }

    /// Take the caller's promise about its own texture ring. At 2 or more this skips the
    /// full-frame copy every submit makes and encodes the caller's texture where it lies
    /// ([`AmfEncoder::in_place`]); the value also becomes the in-flight bound, since past it the
    /// caller may write the picture the hardware is still reading.
    fn set_input_ring_depth(&mut self, depth: usize) {
        if self.input_ring_depth == Some(depth) {
            return;
        }
        self.input_ring_depth = Some(depth);
        tracing::debug!(
            depth,
            in_place = self.in_place().is_some(),
            "AMF input ring depth declared"
        );
    }

    /// Stall recovery: Flush + Terminate + re-Init on the same context. Fail → drop `inner` so
    /// the next submit rebuilds lazily. Owed AUs forfeited; next frame is a forced IDR.
    /// In-place re-Init cannot clear a dead VCN session; at 2 no-output resets, tear the context down.
    fn reset(&mut self) -> bool {
        self.force_kf = true;
        self.resets_without_output = self.resets_without_output.saturating_add(1);
        if self.inner.is_none() {
            return true; // next submit rebuilds lazily
        }
        // Second no-output reset: the fault is the context. Drop `inner` before borrowing it.
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "AMF stall persisted across in-place re-Init — full context teardown, reopening a \
                 fresh context (next submit)"
            );
            self.inner = None;
            self.bound_device = 0;
            self.ir_active = false;
            self.ltr_active = false;
            return true;
        }
        let inner = self
            .inner
            .as_mut()
            .expect("inner is Some — checked above and not cleared since");
        // Stop and join before Terminate: the retrieve thread is inside `QueryOutput` on this
        // very component, and a re-Init under it would run against a terminated one.
        inner.retrieve.stop_and_join();
        inner.retrieve.reset_queues(); // owed AUs forfeited; rebuilt stream restarts at IDR
        inner.held.clear(); // the joined thread proves nothing is reading them
        inner.next = 0; // the rebuilt component's first frame is `opening` again
        inner.hdr_pushed = None; // re-Init'd component needs HDR metadata again
                                 // SAFETY: live component, encode thread, no AMF call in flight. Flush/Terminate are
                                 // legal on a wedge (results ignored); apply_static_props + init rebuild it.
        let rebuilt = unsafe {
            let comp = inner.comp.0;
            ((*(*comp).vtbl).flush)(comp);
            ((*(*comp).vtbl).terminate)(comp);
            let fmt = if self.ten_bit {
                sys::AMF_SURFACE_P010
            } else {
                sys::AMF_SURFACE_NV12
            };
            match self.apply_static_props(comp) {
                Ok((ir, ltr)) => {
                    self.ir_active = ir;
                    // Re-Init voids reference history; drop prior LTR marks.
                    self.ltr_active = ltr;
                    self.ltr_slots = [None; NUM_LTR_SLOTS];
                    self.next_ltr_slot = 0;
                    self.pending_force = None;
                    ((*(*comp).vtbl).init)(comp, fmt, self.width as i32, self.height as i32)
                        == sys::AMF_OK
                }
                Err(_) => false,
            }
        };
        if rebuilt {
            // The component is live again, so it needs its retrieve thread back. Without one no
            // AU would ever be taken off it and the rebuild would read as a second wedge.
            let comp = self
                .inner
                .as_ref()
                .expect("inner is Some — checked above and not cleared since")
                .comp
                .0;
            // SAFETY: `comp` is the component just re-initialized on this thread, with its
            // retrieve thread joined, so nothing else is calling into it.
            let blocking = unsafe { set_query_timeout(comp, self.props.query_timeout) };
            match Retrieve::start(comp, &self.props, blocking) {
                Ok(r) => {
                    self.inner
                        .as_mut()
                        .expect("inner is Some — checked above and not cleared since")
                        .retrieve = r;
                    tracing::info!(
                        "AMF encoder rebuilt in place (Terminate + re-Init on the same context)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "AMF rebuilt but its retrieve thread would not start — reopening lazily"
                    );
                    self.inner = None;
                    self.bound_device = 0;
                }
            }
        } else {
            self.ir_active = false;
            self.ltr_active = false;
            tracing::warn!("AMF in-place re-Init failed — full context teardown, reopening lazily");
            self.inner = None;
            self.bound_device = 0;
        }
        true
    }

    /// `TargetBitrate` via `GetProperty`. `None` before lazy open or on decline — caller keeps
    /// the requested rate. Without this, ABR never learns `encoder_ceiling_kbps` on AMD.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        let inner = self.inner.as_ref()?;
        // SAFETY: live component, session thread, no AMF call in flight; out-param is a local.
        unsafe { get_prop_i64(inner.comp.0, self.props.target_bitrate) }
            .filter(|&b| b > 0)
            .map(|b| b as u64)
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let bps_i = bps.min(i64::MAX as u64) as i64;
        let vbv = self.vbv_bits(bps);
        let Some(inner) = self.inner.as_ref() else {
            // Lazy open applies the new rate via `apply_static_props`.
            self.bitrate_bps = bps;
            return true;
        };
        // Target/Peak/VBV are dynamic: SetProperty retargets without Terminate (no IDR).
        // SAFETY: live component, encode thread, no AMF call in flight.
        let applied = unsafe {
            let p = &self.props;
            let comp = inner.comp.0;
            let ok = set_prop(comp, p.target_bitrate, AmfVariant::from_i64(bps_i), false)
                .unwrap_or(false)
                && set_prop(comp, p.peak_bitrate, AmfVariant::from_i64(bps_i), false)
                    .unwrap_or(false);
            if ok {
                // Optional VBV rescale; decline keeps the old buffer (HRD absorbs the mismatch).
                let _ = set_prop(comp, p.vbv_size, AmfVariant::from_i64(vbv), false);
            }
            ok
        };
        if !applied {
            // Half-applied pair is fine: the rebuild fallback re-authors from scratch.
            tracing::warn!(
                mbps = bps / 1_000_000,
                "AMF declined the dynamic bitrate retarget — falling back to a rebuild"
            );
            return false;
        }
        self.bitrate_bps = bps; // reset()/re-Init re-apply the new rate
        true
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // SAFETY: live component, owning thread. Drain = EOS; remaining AUs surface until AMF_EOF.
        let r = unsafe { ((*(*inner.comp.0).vtbl).drain)(inner.comp.0) };
        if r != sys::AMF_OK {
            tracing::debug!(
                result = result_name(r),
                amf_code = r,
                "AMF Drain returned non-OK at flush"
            );
        }
        // The owed AUs surface on the retrieve thread; wait for the last of them here so no
        // frame submitted after this can be paired with one. Past the budget the component is
        // at end-of-stream, so what is still owed never comes: those entries are stale.
        let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
        while inner.retrieve.in_flight() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_micros(250));
        }
        let stale = std::mem::take(&mut lock(&inner.retrieve.out).pending).len();
        if stale > 0 {
            tracing::warn!(stale, "AMF drain left frames without an AU");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Layout of the FFI mirrors lives as `const _: ()` in `amf_sys.rs` (every build). This
    // checks little-endian union payload packing, which a size/align assert cannot express.
    #[test]
    fn variant_payload_packing_matches_c() {
        let v = AmfVariant::from_rate(60, 1);
        assert_eq!(v.payload[0], 60u64 | (1u64 << 32));
        assert_eq!(AmfVariant::from_i64(-1).payload[0], u64::MAX);
    }

    /// HDR10 grade for live tests: BT.2020, 1000-nit, ST.2086 wire order (primaries G, B, R).
    fn sample_hdr_meta() -> pf_frame::HdrMeta {
        pf_frame::HdrMeta {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 1000 * 10000,
            min_display_mastering_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        }
    }

    /// D3D11 device on the AMD adapter. `None` = no AMD GPU — caller skips.
    fn amd_d3d11_device() -> Option<ID3D11Device> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{D3D11CreateDevice, D3D11_SDK_VERSION};
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1};
        const VENDOR_AMD: u32 = 0x1002;
        // SAFETY: probe owns every handle. Factory/adapter COM or err; CreateDevice fills
        // `device` only on success. Everything drops with its COM wrapper.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            for i in 0.. {
                let adapter: IDXGIAdapter1 = factory.EnumAdapters1(i).ok()?;
                let desc = adapter.GetDesc1().ok()?;
                if desc.VendorId != VENDOR_AMD {
                    continue;
                }
                let mut device: Option<ID3D11Device> = None;
                D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    Default::default(),
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
                .ok()?;
                return device;
            }
            None
        }
    }

    /// DEFAULT-usage NV12 texture (uninit GPU memory; content is irrelevant).
    fn nv12_texture(device: &ID3D11Device, w: u32, h: u32) -> ID3D11Texture2D {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
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
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("NV12 texture");
        tex.expect("NV12 texture")
    }

    /// Live [`Encoder`] smoke per codec: submit/poll, native `reset()`, second batch, flush-drain.
    /// Asserts Annex-B (or AV1 OBU), IDR at start and after reset, FIFO pts. Skips without AMD.
    /// The driver answers SET_ENCODE — where the host latches these caps for the session — before
    /// any frame is submitted, so what `caps()` says at that moment must be what the encoder
    /// negotiated. LTR and intra-refresh are decided in `ensure_inner`, which used to run only at
    /// the first submit: the host therefore latched `supports_rfi: false` and never sent a
    /// reference-frame invalidation, costing a full IDR per lost frame.
    ///
    /// Hardware-independent: it compares the two reads rather than demanding LTR, so a GPU that
    /// genuinely declines still passes (and the printed values say which happened).
    /// A skipped frame repeats the reference; under the one-frame VBV that is the picture
    /// freezing on a scene cut, so the open must leave the switch off whatever the usage set.
    #[test]
    fn amf_frame_skip_is_off_after_open_live() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            640,
            480,
            60,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        enc.prepare(&device).expect("prepare");
        let comp = enc
            .inner
            .as_ref()
            .expect("prepare opened the component")
            .comp
            .0;
        // SAFETY: a live component on this thread; `get_property` only reads it.
        let skip = unsafe { get_prop_bool(comp, enc.props.skip_frame) };
        assert_eq!(
            skip,
            Some(false),
            "rate-control frame skip must be off after open"
        );
    }

    #[test]
    fn amf_caps_do_not_change_at_the_first_submit_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = match AmfEncoder::open(
            Codec::H264,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        enc.prepare(&device).expect("prepare");
        let at_open = enc.caps();
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 1,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let _ = enc.poll().expect("poll");
        let after = enc.caps();
        eprintln!(
            "AMF caps at open: rfi={} ir={} | after first submit: rfi={} ir={}",
            at_open.supports_rfi, at_open.intra_refresh, after.supports_rfi, after.intra_refresh
        );
        assert_eq!(
            (at_open.supports_rfi, at_open.intra_refresh),
            (after.supports_rfi, after.intra_refresh),
            "the host reads these once, before the first frame"
        );
    }

    /// `flush` drains the component, which leaves it at end-of-stream where it takes no more
    /// input. A session that flushed must encode again, and every AU on both sides of the
    /// flush must carry the pts of the frame it encodes.
    #[test]
    fn amf_encodes_again_after_a_flush_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = match AmfEncoder::open(
            Codec::H264,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        enc.prepare(&device).expect("prepare");
        let run = |enc: &mut AmfEncoder, base: u64| {
            let mut pts = Vec::new();
            for i in 0..8 {
                let frame = CapturedFrame {
                    provenance: Default::default(),
                    width: w,
                    height: h,
                    pts_ns: base + i,
                    format: PixelFormat::Nv12,
                    payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                        texture: tex.clone(),
                        device: device.clone(),
                        pyro: None,
                    }),
                    cursor: None,
                };
                enc.submit(&frame).expect("submit");
                if let Some(au) = enc.poll().expect("poll") {
                    pts.push(au.pts_ns);
                }
            }
            pts
        };
        let before = run(&mut enc, 1);
        assert!(
            !before.is_empty(),
            "the encoder produced nothing before the flush"
        );
        enc.flush().expect("flush");
        let after = run(&mut enc, 1000);
        eprintln!("AMF AUs before flush: {before:?}, after: {after:?}");
        let resumed: Vec<u64> = after.iter().copied().filter(|p| *p >= 1000).collect();
        assert!(
            !resumed.is_empty(),
            "the encoder accepted no input after a flush — it is still at end-of-stream"
        );
        assert!(
            after
                .iter()
                .all(|p| (1..=8).contains(p) || (1000..1008).contains(p)),
            "an AU after the flush carries a pts nobody submitted: {after:?}"
        );
        assert!(
            resumed.windows(2).all(|w| w[1] == w[0] + 1),
            "AUs after the flush are paired off by one: {resumed:?}"
        );
    }

    #[test]
    fn amf_encode_live_smoke() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);

        for codec in [Codec::H265, Codec::H264, Codec::Av1] {
            // AV1 is RDNA3+: probe THIS device (`open` may pick a different GPU on a hybrid box).
            if codec == Codec::Av1 && !probe_can_encode_on(&device, codec) {
                eprintln!("skipping Av1: this AMD GPU's native probe declined it (pre-RDNA3?)");
                continue;
            }
            let mut enc = match AmfEncoder::open(
                codec,
                PixelFormat::Nv12,
                w,
                h,
                fps,
                2_000_000,
                8,
                ChromaFormat::Yuv420,
                false,
                None,
            ) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("skipping {codec:?}: native AMF open declined ({e:#})");
                    continue;
                }
            };
            let batch = |enc: &mut AmfEncoder, base: u64, n: usize| -> Vec<EncodedFrame> {
                let mut aus = Vec::new();
                for i in 0..n {
                    let frame = CapturedFrame {
                        provenance: Default::default(),
                        width: w,
                        height: h,
                        pts_ns: base + i as u64,
                        format: PixelFormat::Nv12,
                        payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                            texture: tex.clone(),
                            device: device.clone(),
                            pyro: None,
                        }),
                        cursor: None,
                    };
                    enc.submit(&frame).expect("submit");
                    if let Some(au) = enc.poll().expect("poll") {
                        aus.push(au);
                    }
                }
                aus
            };
            let first_run = batch(&mut enc, 1, 6);
            assert!(enc.reset(), "native reset must report rebuilt");
            let mut second_run = batch(&mut enc, 100, 6);
            enc.flush().expect("flush");
            for _ in 0..50 {
                match enc.poll().expect("drain poll") {
                    Some(au) => second_run.push(au),
                    None => break,
                }
            }
            assert!(
                first_run.len() >= 3 && second_run.len() >= 3,
                "{codec:?}: expected most AUs out (got {} + {})",
                first_run.len(),
                second_run.len()
            );
            for run in [&first_run, &second_run] {
                let first = &run[0];
                assert!(
                    first.keyframe,
                    "{codec:?}: stream/reset start must be an IDR"
                );
                if codec == Codec::Av1 {
                    // AV1 is OBU, not Annex-B.
                    assert!(!first.data.is_empty(), "Av1: empty key AU");
                } else {
                    assert!(
                        first.data.starts_with(&[0, 0, 0, 1]) || first.data.starts_with(&[0, 0, 1]),
                        "{codec:?}: AU must be Annex-B (got {:02x?})",
                        &first.data[..first.data.len().min(8)]
                    );
                }
            }
            assert_eq!(first_run[0].pts_ns, 1, "FIFO pts pairing");
            // Bitstream FIFO: a declined B-frame pin would reorder AUs. Don't trust set_prop.
            for run in [&first_run, &second_run] {
                for pair in run.windows(2) {
                    assert!(
                        pair[1].pts_ns > pair[0].pts_ns,
                        "{codec:?}: AUs must leave in submit order (reordering ⇒ B-frames), \
                         got {} then {}",
                        pair[0].pts_ns,
                        pair[1].pts_ns
                    );
                }
            }
            assert_eq!(second_run[0].pts_ns, 100, "post-reset FIFO pts pairing");
            eprintln!(
                "live AMF {codec:?} encode: {} + {} AUs across a native reset, first IDR {} bytes",
                first_run.len(),
                second_run.len(),
                first_run[0].data.len()
            );
        }
    }

    /// A submit refused after the LTR decision — surface creation, a property set — must not
    /// leave the mirror claiming a mark the hardware never made, nor eat a queued force: the
    /// next frame is an IDR, which resets both. Refuses one mark frame and one recovery frame.
    /// Skips without AMD; the mirror checks skip when the driver declines LTR.
    #[test]
    fn amf_refused_submit_forces_idr_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h) = (640u32, 480u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = match AmfEncoder::open(
            Codec::H264,
            PixelFormat::Nv12,
            w,
            h,
            30,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        enc.prepare(&device).expect("prepare");
        let ltr = enc.caps().supports_rfi;
        let mark = enc.ltr_mark_interval as u32;
        assert!(
            mark >= 4,
            "mark interval {mark} leaves no room for a loss window"
        );
        let (refused_mark, refused_force) = (mark, 2 * mark);
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for i in 0..4 * mark {
            if i == refused_mark {
                enc.fail_submit_at = Some(i as i64);
            }
            if i == refused_force {
                if ltr {
                    // The IDR after the refused mark is the pre-loss anchor.
                    assert!(
                        enc.invalidate_ref_frames(i as i64 - 2, i as i64 - 1),
                        "no pre-loss LTR to force"
                    );
                }
                enc.fail_submit_at = Some(i as i64);
            }
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i as u64,
                format: PixelFormat::Nv12,
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
                aus.push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            aus.push(au);
        }
        let au = |i: u32| {
            aus.iter()
                .find(|a| a.pts_ns == i as u64)
                .unwrap_or_else(|| panic!("no AU for frame {i}"))
        };
        assert!(au(0).keyframe, "first AU must be a keyframe");
        for refused in [refused_mark, refused_force] {
            assert!(
                aus.iter().all(|a| a.pts_ns != refused as u64),
                "refused frame {refused} produced an AU"
            );
            assert!(
                au(refused + 1).keyframe,
                "frame {} after refused frame {refused} must be an IDR",
                refused + 1
            );
        }
        if ltr {
            assert!(
                !enc.ltr_slots.contains(&Some(refused_mark as i64)),
                "mirror claims a mark the hardware never made: {:?}",
                enc.ltr_slots
            );
        }
        eprintln!(
            "live AMF refused-submit: {} AUs, ltr={ltr}, mirror {:?}",
            aus.len(),
            enc.ltr_slots
        );
    }

    /// LTR anchors on hardware: the wave smokes' moving pattern, a loss every `PF_WAVE_GAP` frames
    /// (default 40) answered `PF_WAVE_LAG` frames later (default 2) through
    /// `invalidate_ref_frames`. The full stream and the view without the lost frames land in
    /// `PUNKTFUNK_SMOKE_DIR` with `.idx` sidecars, for `gpu_parity`'s field hashers. HEVC, or
    /// `PF_WAVE_CODEC=h264` or `av1` (an `.obu` for `field_av1`); shape `PF_WAVE_SMOKE=WxH:8:fps:mbps`, `PF_WAVE_SOAK` losses.
    ///
    /// `cargo test -p pf-encode-win --lib amf_ltr_anchor_soak -- --ignored --nocapture`
    #[test]
    #[ignore = "requires an AMD GPU with AMF — run manually on an AMD Windows box (.173)"]
    fn amf_ltr_anchor_soak() {
        use crate::smoke_pattern::{scroll_pattern_nv12, write_capture};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA,
        };
        try_factory().expect("AMF runtime");
        let device = amd_d3d11_device().expect("an AMD adapter");
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
        let (codec, ext) = match std::env::var("PF_WAVE_CODEC").as_deref() {
            Ok("h264") => (Codec::H264, "h264"),
            Ok("av1") => (Codec::Av1, "obu"),
            _ => (Codec::H265, "h265"),
        };
        let mut enc = AmfEncoder::open(
            codec,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            mbps * 1_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        )
        .expect("AMF open");
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
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
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
            "amf_ltr_anchor_soak: {w}x{h} {fps} fps {mbps} Mbps {codec:?} lag={lag} gap={gap} \
             interval={} lost={} anchors={} idrs={}",
            enc.ltr_mark_interval,
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
            write_capture(&format!("{dir}/amf-anchor.{ext}"), &full).expect("write");
            write_capture(&format!("{dir}/amf-anchor-dropS.{ext}"), &view).expect("write");
        }
    }

    /// Live `applied_bitrate_bps`: None before lazy open, open rate after submit, new rate after
    /// retarget. Skips without AMD.
    #[test]
    fn amf_applied_bitrate_readback_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        )
        .expect("native AMF open");
        assert_eq!(
            enc.applied_bitrate_bps(),
            None,
            "no readback before the lazy open — the caller must keep the requested rate"
        );
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 1,
            format: PixelFormat::Nv12,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: tex.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let opened = enc.applied_bitrate_bps();
        assert_eq!(
            opened,
            Some(2_000_000),
            "post-open readback must be the accepted open rate"
        );
        assert!(
            enc.reconfigure_bitrate(8_000_000),
            "dynamic retarget declined on live hardware"
        );
        let retargeted = enc.applied_bitrate_bps();
        assert_eq!(
            retargeted,
            Some(8_000_000),
            "post-retarget readback must be the accepted NEW rate"
        );
        eprintln!("live AMF applied-bitrate readback: open {opened:?} -> retarget {retargeted:?}");
    }

    /// Live probe: AVC and HEVC must be true on any VCN; AV1 is hardware truth (RDNA3+).
    #[test]
    fn amf_native_probe_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let h264 = probe_can_encode_on(&device, Codec::H264);
        let h265 = probe_can_encode_on(&device, Codec::H265);
        let av1 = probe_can_encode_on(&device, Codec::Av1);
        eprintln!("native AMF probe: h264={h264} h265={h265} av1={av1}");
        assert!(h264 && h265, "every VCN generation encodes AVC + HEVC");
    }

    /// Live HDR: P010 HEVC Main10 must encode. Mastering/CLL prefix SEI (payload 137/144) is
    /// soft-reported — VCN generations differ.
    #[test]
    fn amf_hdr_encode_live_smoke() {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("P010 texture");
        let tex = tex.expect("P010 texture");
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            w,
            h,
            fps,
            4_000_000,
            10,
            ChromaFormat::Yuv420,
            true,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF 10-bit open declined ({e:#})");
                return;
            }
        };
        enc.set_hdr_meta(Some(sample_hdr_meta()));
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for i in 0..6 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: 1 + i as u64,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit(&frame).expect("submit (P010)");
            if let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        assert!(!aus.is_empty(), "10-bit HDR encode produced no AUs");
        let idr = &aus[0];
        assert!(idr.keyframe, "first AU must be an IDR");
        // HEVC prefix-SEI (NUH 0x4E 0x01): payload 137 mastering / 144 CLL.
        let mut mastering = false;
        let mut cll = false;
        for i in 0..idr.data.len().saturating_sub(5) {
            let d = &idr.data[i..];
            let nal = if d.starts_with(&[0, 0, 1]) {
                &d[3..]
            } else if d.starts_with(&[0, 0, 0, 1]) {
                &d[4..]
            } else {
                continue;
            };
            if nal.len() >= 3 && nal[0] == 0x4E && nal[1] == 0x01 {
                match nal[2] {
                    137 => mastering = true,
                    144 => cll = true,
                    _ => {}
                }
            }
        }
        eprintln!(
            "live AMF HEVC Main10 HDR: {} AUs, IDR {} bytes, mastering SEI={mastering}, CLL SEI={cll}",
            aus.len(),
            idr.data.len()
        );
        if !mastering {
            eprintln!("note: no mastering-display SEI found on this VCN/driver — client falls back to the 0xCE datagram");
        }
    }

    /// Live 10-bit SDR: P010 HEVC Main10 under BT.709 (no HDR volume). Confirms the colour untie —
    /// the encoder must NOT emit mastering/CLL SEI, and (via `AMF_SDR10_DUMP=<path>` + ffprobe) the
    /// SPS VUI signals BT.709, not BT.2020 PQ. Same P010 ring as the HDR path; only the colour
    /// differs.
    #[test]
    fn amf_sdr10_encode_live_smoke() {
        use windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE;
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_P010,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: CreateTexture2D fills the out-param only on success; owned COM, this thread.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.expect("P010 texture");
        let tex = tex.expect("P010 texture");
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::P010,
            w,
            h,
            fps,
            4_000_000,
            10,
            ChromaFormat::Yuv420,
            false, // SDR: BT.709, not BT.2020 PQ
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF 10-bit SDR open declined ({e:#})");
                return;
            }
        };
        // No set_hdr_meta: a 10-bit SDR session carries no HDR volume.
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for i in 0..6 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: 1 + i as u64,
                format: PixelFormat::P010,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            enc.submit(&frame).expect("submit (P010 SDR)");
            if let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        assert!(!aus.is_empty(), "10-bit SDR encode produced no AUs");
        let idr = &aus[0];
        assert!(idr.keyframe, "first AU must be an IDR");
        // No mastering (137) / CLL (144) prefix SEI on an SDR stream.
        let mut hdr_sei = false;
        for i in 0..idr.data.len().saturating_sub(5) {
            let d = &idr.data[i..];
            let nal = if d.starts_with(&[0, 0, 1]) {
                &d[3..]
            } else if d.starts_with(&[0, 0, 0, 1]) {
                &d[4..]
            } else {
                continue;
            };
            if nal.len() >= 3 && nal[0] == 0x4E && nal[1] == 0x01 && matches!(nal[2], 137 | 144) {
                hdr_sei = true;
            }
        }
        assert!(
            !hdr_sei,
            "a 10-bit SDR stream must not carry HDR mastering/CLL SEI"
        );
        if let Ok(path) = std::env::var("AMF_SDR10_DUMP") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let _ = std::fs::write(&path, &full);
            eprintln!(
                "amf_sdr10: wrote {path} ({} bytes, {} AUs)",
                full.len(),
                aus.len()
            );
        }
        eprintln!(
            "live AMF HEVC Main10 SDR: {} AUs, IDR {} bytes, hdr_sei={hdr_sei}",
            aus.len(),
            idr.data.len()
        );
    }

    /// Live: the D3D11 video processor converts 8-bit BGRA to a P010 target under BT.709 studio on
    /// AMD. This is the SDR-10 converter half (`EncodeInput::P010Sdr`) — the "renders green" caveat
    /// on RGB→P010 is NVIDIA-only, so prove AMD writes plausible luma. Mid-grey in → studio Y near
    /// 504 (10-bit); a failed render is black (0) or clipped.
    #[test]
    fn videoconverter_bgra_to_p010_bt709_live() {
        use crate::convert::VideoConverter;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA, D3D11_USAGE_STAGING,
        };
        use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h) = (256u32, 256u32);
        // SAFETY: the device is live on this thread.
        let ctx = unsafe { device.GetImmediateContext() }.expect("immediate context");

        // BGRA filled mid-grey (128,128,128,255), one subresource upload.
        let pixels = vec![128u8; (w * h * 4) as usize];
        let bgra_desc = D3D11_TEXTURE2D_DESC {
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
            // Match the driver's captured-BGRA binds (`Targets::new` InputKind::Bgra); a
            // shader-resource-only texture is not a valid video-processor input surface on AMD.
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr() as *const _,
            SysMemPitch: w * 4,
            SysMemSlicePitch: 0,
        };
        let mut bgra: Option<ID3D11Texture2D> = None;
        // SAFETY: descriptor + init data are fully populated; out-param filled on success.
        unsafe { device.CreateTexture2D(&bgra_desc, Some(&init), Some(&mut bgra)) }
            .expect("BGRA texture");
        let bgra = bgra.expect("BGRA texture");

        let p010_desc = D3D11_TEXTURE2D_DESC {
            Format: DXGI_FORMAT_P010,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..bgra_desc
        };
        let mut p010: Option<ID3D11Texture2D> = None;
        // SAFETY: as above.
        unsafe { device.CreateTexture2D(&p010_desc, None, Some(&mut p010)) }.expect("P010 texture");
        let p010 = p010.expect("P010 texture");

        let conv = VideoConverter::new(&device, &ctx, w, h, false).expect("VideoConverter");
        // The load-bearing assertion: AMD's video processor accepts a P010 output view.
        conv.convert(&bgra, &p010)
            .expect("BGRA->P010 on the AMD video processor (a green render would still Ok here)");

        // Read back the Y plane's first sample through a staging copy.
        let stag_desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..p010_desc
        };
        let mut stag: Option<ID3D11Texture2D> = None;
        // SAFETY: as above.
        unsafe { device.CreateTexture2D(&stag_desc, None, Some(&mut stag)) }.expect("staging P010");
        let stag = stag.expect("staging P010");
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: `stag` and `p010` are live same-device textures; `ctx` is their immediate context.
        // `Map` fills `mapped`; `Unmap` releases it before the function returns.
        let y10 = unsafe {
            ctx.CopyResource(&stag, &p010);
            ctx.Map(&stag, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .expect("map staging");
            // P010 Y plane: row 0, first 16-bit sample; the 10-bit code sits in the high bits.
            let sample = *(mapped.pData as *const u16);
            ctx.Unmap(&stag, 0);
            sample >> 6
        };
        eprintln!("VideoConverter BGRA(128)->P010 on AMD: Y10={y10} (expect ~504 studio grey)");
        assert!(
            (400..=620).contains(&y10),
            "P010 luma {y10} off BT.709 studio grey — the AMD video processor mis-rendered P010"
        );
    }

    /// Live intra-refresh property on a scratch component (does not mutate process env).
    #[test]
    fn amf_intra_refresh_property_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let Ok(lib) = try_factory() else { return };
        // SAFETY: guards own every created object; `set_prop` on this thread.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            assert_eq!(
                ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx),
                sys::AMF_OK
            );
            let ctx = Ctx(ctx);
            assert_eq!(
                ((*(*ctx.0).vtbl).init_dx11)(ctx.0, device.as_raw(), sys::AMF_DX11_1),
                sys::AMF_OK
            );
            for codec in [Codec::H264, Codec::H265] {
                let props = codec_props(codec);
                let mut comp: *mut sys::AmfComponent = ptr::null_mut();
                if ((*(*lib.factory).vtbl).create_component)(
                    lib.factory,
                    ctx.0,
                    props.component.0,
                    &mut comp,
                ) != sys::AMF_OK
                    || comp.is_null()
                {
                    eprintln!("skipping {codec:?}: component unavailable");
                    continue;
                }
                let comp = Component(comp);
                let _ = set_prop(
                    comp.0,
                    props.usage,
                    AmfVariant::from_i64(usage_from_knobs(codec)),
                    true,
                );
                let (name, block) = props.intra_refresh.expect("AVC/HEVC define intra-refresh");
                let blocks = 640u32.div_ceil(block) * 480u32.div_ceil(block);
                let per_slot = blocks.div_ceil(30).max(1);
                let applied = set_prop(comp.0, name, AmfVariant::from_i64(per_slot as i64), false)
                    .expect("optional set_prop never errors");
                eprintln!(
                    "intra-refresh {codec:?}: {per_slot} units/slot accepted={applied} on this VCN"
                );
            }
        }
    }

    /// Burst faster than the encoder drains (no poll between submits). `submit` must drain into
    /// `ready` instead of erroring. Asserts IDR-first FIFO across the ready→pending boundary.
    #[test]
    fn amf_backpressure_burst_live() {
        if let Err(e) = try_factory() {
            eprintln!("skipping: AMF runtime unavailable ({e})");
            return;
        }
        let Some(device) = amd_d3d11_device() else {
            eprintln!("skipping: no AMD adapter on this box");
            return;
        };
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let tex = nv12_texture(&device, w, h);
        let mut enc = match AmfEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            w,
            h,
            fps,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            None,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("skipping: native AMF open declined ({e:#})");
                return;
            }
        };
        const BURST: u64 = 48; // >> RING, faster than the ASIC drains
        for i in 1..=BURST {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i,
                format: PixelFormat::Nv12,
                payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                    texture: tex.clone(),
                    device: device.clone(),
                    pyro: None,
                }),
                cursor: None,
            };
            // No poll between submits: the in-flight bound must drain, never error.
            enc.submit(&frame)
                .expect("burst submit must ride back-pressure, not error");
        }
        enc.flush().expect("flush");
        let mut aus: Vec<EncodedFrame> = Vec::new();
        for _ in 0..(BURST as usize + 100) {
            match enc.poll().expect("drain poll") {
                Some(au) => aus.push(au),
                None => break,
            }
        }
        assert!(
            aus.len() as u64 >= BURST - 2,
            "most AUs must survive the burst without a reset (got {} of {BURST})",
            aus.len()
        );
        assert!(aus[0].keyframe, "first AU must be the IDR");
        for pair in aus.windows(2) {
            assert!(
                pair[1].pts_ns > pair[0].pts_ns,
                "AUs must stay FIFO-monotonic across the ready→pending boundary: {} then {}",
                pair[0].pts_ns,
                pair[1].pts_ns
            );
        }
        eprintln!(
            "back-pressure burst: {} AUs, FIFO-monotonic, IDR-first — ring bound held, no reset",
            aus.len()
        );
    }

    /// FFI smoke: load, version-gate, CreateContext + HEVC CreateComponent. A layout error in
    /// the mirror crashes; pass/skip is the assertion.
    #[test]
    fn amf_factory_probe_smoke() {
        let lib = match try_factory() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: AMF runtime unavailable ({e})");
                return;
            }
        };
        assert!(lib.version >= sys::AMF_MIN_VERSION);
        // SAFETY: CreateContext fills `ctx` only on AMF_OK; InitDX11(null) is AMF's own device
        // (fail → skip). Guards release every created object once.
        unsafe {
            let mut ctx: *mut sys::AmfContext = ptr::null_mut();
            let r = ((*(*lib.factory).vtbl).create_context)(lib.factory, &mut ctx);
            assert_eq!(r, sys::AMF_OK, "CreateContext: {}", result_name(r));
            assert!(!ctx.is_null());
            let ctx = Ctx(ctx);
            let r = ((*(*ctx.0).vtbl).init_dx11)(ctx.0, ptr::null_mut(), sys::AMF_DX11_1);
            if r != sys::AMF_OK {
                eprintln!(
                    "skipping: InitDX11(default device) failed ({})",
                    result_name(r)
                );
                return;
            }
            let mut comp: *mut sys::AmfComponent = ptr::null_mut();
            let r = ((*(*lib.factory).vtbl).create_component)(
                lib.factory,
                ctx.0,
                w!("AMFVideoEncoderHW_HEVC").0,
                &mut comp,
            );
            if r != sys::AMF_OK || comp.is_null() {
                // Probe answer (no HEVC VCN), not a mirror failure.
                eprintln!("note: CreateComponent(HEVC) declined ({})", result_name(r));
                return;
            }
            let _comp = Component(comp);
        }
    }
}
