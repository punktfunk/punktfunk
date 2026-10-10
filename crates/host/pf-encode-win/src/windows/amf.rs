//! AMD **AMF** hardware encoder (Windows, D3D11 input). Direct-SDK analogue of [`super::nvenc`].
//!
//! Drives the AMF **C vtable ABI** (GPUOpen headers; FFmpeg's `amfenc.c` uses the same surface,
//! not the C++ classes). FFI is header **v1.4.36**; load accepts runtimes down to **v1.4.30**
//! ([`sys::AMF_MIN_VERSION`]). Newer encoder features are string-keyed properties that degrade
//! per driver, not vtable changes. Loads `amfrt64.dll` at runtime — no build feature. Missing or
//! old runtime fails [`AmfEncoder::open`] and the session.
//!
//! Input is a same-device D3D11 texture in NV12, P010 or BGRA: the caller's own when it
//! declared a ring depth, else a `CopySubresourceRegion` into [`Inner::ring`], then
//! `CreateSurfaceFromDX11Native`. BGRA is converted by VCN, so no pass of ours runs on the 3D
//! engine. No readback: Rgb10a2 or CPU frames fail open/submit. VCN does not encode 4:4:4.
//! Evidence: `design/native-amf-encoder.md`.

use super::policy::{intra_refresh_period, intra_refresh_requested, ltr_test_force_at};
use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::ltr::{LtrMirror, LtrStep, NUM_LTR_SLOTS};
use crate::retrieve::{poll_budget_ms, AuQueue, RetrieveThread};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use windows::core::{h, w, Interface, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
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
    // SAFETY: `module` is live (caller contract); the call writes at most `buf.len()` units.
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    // n == 0 failed; n >= len truncated (no guaranteed NUL). Otherwise `buf[n]` is the terminator.
    if n == 0 || n >= buf.len() {
        return (None, None);
    }
    let path = String::from_utf16_lossy(&buf[..n]);
    // SAFETY: `buf[n]` is the terminator (checked above) and `buf` names the loaded DLL.
    let version = unsafe { dll_file_version(PCWSTR(buf.as_ptr())) };
    (Some(path), version)
}

/// `VS_FIXEDFILEINFO` file version as `a.b.c.d`. `None` if the resource is missing.
///
/// # Safety
/// `path` is a valid NUL-terminated wide string to a readable file.
unsafe fn dll_file_version(path: PCWSTR) -> Option<String> {
    // SAFETY: `path` is NUL-terminated (caller contract).
    let size = unsafe { GetFileVersionInfoSizeW(path, None) };
    if size == 0 {
        return None;
    }
    let mut block = vec![0u8; size as usize];
    // SAFETY: as above; `block` is writable for the `size` bytes passed.
    unsafe { GetFileVersionInfoW(path, None, size, block.as_mut_ptr() as *mut c_void) }.ok()?;
    let mut value: *mut c_void = ptr::null_mut();
    let mut len: u32 = 0;
    // SAFETY: `block` holds the version resource just read; the out-params are locals.
    let ok = unsafe {
        VerQueryValueW(
            block.as_ptr() as *const c_void,
            w!("\\"),
            &mut value,
            &mut len,
        )
    };
    if !ok.as_bool() || value.is_null() || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
    {
        return None;
    }
    // SAFETY: on success `VerQueryValueW` points `value` at a `VS_FIXEDFILEINFO` inside `block`,
    // valid for `len` bytes (checked >= its size). A `u8` buffer promises no alignment, so the
    // read is unaligned.
    let ffi = unsafe { (value as *const VS_FIXEDFILEINFO).read_unaligned() };
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
/// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_FULL_709`: full-range RGB, as a desktop composes it.
const COLOR_PROFILE_FULL_709: i64 = 7;
// `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM` / `AMF_COLOR_PRIMARIES_ENUM` (CICP code points).
const TRANSFER_BT709: i64 = 1;
const TRANSFER_LINEAR: i64 = 8;
const TRANSFER_SMPTE2084: i64 = 16;
const PRIMARIES_BT709: i64 = 1;
const PRIMARIES_BT2020: i64 = 9;

struct CodecProps {
    /// `factory->CreateComponent` id.
    component: &'static HSTRING,
    usage: &'static HSTRING,
    rc_method: &'static HSTRING,
    /// `RATE_CONTROL_METHOD_CBR` — 1 on AVC, **3** on HEVC and AV1.
    rc_cbr: i64,
    target_bitrate: &'static HSTRING,
    peak_bitrate: &'static HSTRING,
    vbv_size: &'static HSTRING,
    enforce_hrd: &'static HSTRING,
    filler_data: &'static HSTRING,
    /// Rate-control frame skip; the latency usages default it on.
    skip_frame: &'static HSTRING,
    quality_preset: &'static HSTRING,
    /// `QUALITY_PRESET_SPEED` — 1 on AVC, **10** on HEVC, **100** on AV1.
    quality_speed: i64,
    /// AVC/HEVC: `L"LowLatencyInternal"` (bool). AV1: `Av1EncodingLatencyMode` (enum).
    lowlatency: &'static HSTRING,
    /// Bool `true` (AVC/HEVC) or the AV1 latency-mode enum value.
    lowlatency_value: AmfVariantKind,
    framerate: &'static HSTRING,
    /// AVC `IDRPeriod`, HEVC `HevcGOPSize`, AV1 `Av1GOPSize`. Value is `i32::MAX` (infinite GOP)
    /// except AV1, whose header defines **0** as "key frame at first frame only".
    idr_period: &'static HSTRING,
    idr_period_value: i64,
    /// Per-surface forced-keyframe: 2 = PICTURE_TYPE_IDR (AVC/HEVC), **1** = KEY (AV1).
    force_picture_type: &'static HSTRING,
    force_idr_value: i64,
    /// Output `*_OUTPUT_DATA_TYPE_*` / `Av1OutputFrameType`. Type ≤ `output_key_max` is a
    /// keyframe. AV1 INTRA_ONLY=1 does not reset references — not a join point.
    output_data_type: &'static HSTRING,
    output_key_max: i64,
    /// `QueryTimeout` (ms): how long `QueryOutput` may block. Codec-prefixed like the rest, and
    /// optional — an older runtime rejects it and the retrieve thread samples instead.
    query_timeout: &'static HSTRING,
    out_color_profile: &'static HSTRING,
    out_transfer: &'static HSTRING,
    out_primaries: &'static HSTRING,
    /// Input colour, set for a BGRA input only: VCN converts it, and AMF's defaults for an
    /// RGB input do not describe a desktop.
    in_color_profile: &'static HSTRING,
    in_transfer: &'static HSTRING,
    in_primaries: &'static HSTRING,
    in_full_range: &'static HSTRING,
    /// `*InHDRMetadata` (`AMFBuffer` of [`sys::AmfHdrMetadata`]). `None` on AVC — no HDR on the wire.
    hdr_metadata: Option<&'static HSTRING>,
    /// Intra-refresh: (units-per-slot, block edge px). AVC 16-px MBs, HEVC 64-px CTBs. `None` on
    /// AV1 (mode enum only, no slot-size control).
    intra_refresh: Option<(&'static HSTRING, u32)>,
    /// LTR-RFI property names, on every codec.
    ltr: Option<LtrProps>,
}

/// AMF LTR property names, codec-prefixed (AVC bare, HEVC `Hevc*`, AV1 `Av1*`). Two static at
/// open, two per-frame on the input surface.
struct LtrProps {
    /// `MaxOfLTRFrames` — user LTR slots (we request [`NUM_LTR_SLOTS`]).
    max_ltr_frames: &'static HSTRING,
    /// `MaxNumRefFrames` — reference-picture budget; must exceed 1 for LTR to engage.
    max_num_ref_frames: &'static HSTRING,
    /// `MarkCurrentWithLTRIndex` — tag this frame as long-term reference slot N.
    mark_ltr_index: &'static HSTRING,
    /// `ForceLTRReferenceBitfield` — reference only LTR slots in the bitfield (`1<<N`).
    force_ltr_bitfield: &'static HSTRING,
    /// `LTRMode` — `1` keeps the slots a force leaves out; `0`, the default, empties them.
    ltr_mode: &'static HSTRING,
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
            component: h!("AMFVideoEncoderVCE_AVC"),
            usage: h!("Usage"),
            rc_method: h!("RateControlMethod"),
            rc_cbr: 1,
            target_bitrate: h!("TargetBitrate"),
            peak_bitrate: h!("PeakBitrate"),
            vbv_size: h!("VBVBufferSize"),
            enforce_hrd: h!("EnforceHRD"),
            filler_data: h!("FillerDataEnable"),
            skip_frame: h!("RateControlSkipFrameEnable"),
            quality_preset: h!("QualityPreset"),
            quality_speed: 1,
            lowlatency: h!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: h!("FrameRate"),
            idr_period: h!("IDRPeriod"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: h!("ForcePictureType"),
            force_idr_value: 2,
            output_data_type: h!("OutputDataType"),
            query_timeout: h!("QueryTimeout"),
            output_key_max: 1,
            out_color_profile: h!("OutColorProfile"),
            out_transfer: h!("OutColorTransferChar"),
            out_primaries: h!("OutColorPrimaries"),
            in_color_profile: h!("InColorProfile"),
            in_transfer: h!("InColorTransferChar"),
            in_primaries: h!("InColorPrimaries"),
            in_full_range: h!("InputFullRangeColor"),
            hdr_metadata: None,
            intra_refresh: Some((h!("IntraRefreshMBsNumberPerSlot"), 16)),
            ltr: Some(LtrProps {
                max_ltr_frames: h!("MaxOfLTRFrames"),
                max_num_ref_frames: h!("MaxNumRefFrames"),
                mark_ltr_index: h!("MarkCurrentWithLTRIndex"),
                force_ltr_bitfield: h!("ForceLTRReferenceBitfield"),
                ltr_mode: h!("LTRMode"),
            }),
        },
        Codec::H265 => CodecProps {
            component: h!("AMFVideoEncoderHW_HEVC"),
            usage: h!("HevcUsage"),
            rc_method: h!("HevcRateControlMethod"),
            rc_cbr: 3,
            target_bitrate: h!("HevcTargetBitrate"),
            peak_bitrate: h!("HevcPeakBitrate"),
            vbv_size: h!("HevcVBVBufferSize"),
            enforce_hrd: h!("HevcEnforceHRD"),
            filler_data: h!("HevcFillerDataEnable"),
            skip_frame: h!("HevcRateControlSkipFrameEnable"),
            quality_preset: h!("HevcQualityPreset"),
            quality_speed: 10,
            lowlatency: h!("LowLatencyInternal"),
            lowlatency_value: AmfVariantKind::Bool(true),
            framerate: h!("HevcFrameRate"),
            idr_period: h!("HevcGOPSize"),
            idr_period_value: i32::MAX as i64,
            force_picture_type: h!("HevcForcePictureType"),
            force_idr_value: 2,
            output_data_type: h!("HevcOutputDataType"),
            query_timeout: h!("HevcQueryTimeout"),
            output_key_max: 1,
            out_color_profile: h!("HevcOutColorProfile"),
            out_transfer: h!("HevcOutColorTransferChar"),
            out_primaries: h!("HevcOutColorPrimaries"),
            in_color_profile: h!("HevcInColorProfile"),
            in_transfer: h!("HevcInColorTransferChar"),
            in_primaries: h!("HevcInColorPrimaries"),
            in_full_range: h!("HevcInputFullRangeColor"),
            hdr_metadata: Some(h!("HevcInHDRMetadata")),
            intra_refresh: Some((h!("HevcIntraRefreshCTBsNumberPerSlot"), 64)),
            ltr: Some(LtrProps {
                max_ltr_frames: h!("HevcMaxOfLTRFrames"),
                max_num_ref_frames: h!("HevcMaxNumRefFrames"),
                mark_ltr_index: h!("HevcMarkCurrentWithLTRIndex"),
                force_ltr_bitfield: h!("HevcForceLTRReferenceBitfield"),
                ltr_mode: h!("HevcLTRMode"),
            }),
        },
        Codec::Av1 => CodecProps {
            component: h!("AMFVideoEncoderHW_AV1"),
            usage: h!("Av1Usage"),
            rc_method: h!("Av1RateControlMethod"),
            rc_cbr: 3,
            target_bitrate: h!("Av1TargetBitrate"),
            peak_bitrate: h!("Av1PeakBitrate"),
            vbv_size: h!("Av1VBVBufferSize"),
            enforce_hrd: h!("Av1EnforceHRD"),
            filler_data: h!("Av1FillerData"),
            skip_frame: h!("Av1RateControlSkipFrameEnable"),
            quality_preset: h!("Av1QualityPreset"),
            quality_speed: 100,
            lowlatency: h!("Av1EncodingLatencyMode"),
            lowlatency_value: AmfVariantKind::I64(AV1_LATENCY_LOWEST),
            framerate: h!("Av1FrameRate"),
            idr_period: h!("Av1GOPSize"),
            idr_period_value: 0,
            force_picture_type: h!("Av1ForceFrameType"),
            force_idr_value: 1,
            output_data_type: h!("Av1OutputFrameType"),
            query_timeout: h!("Av1QueryTimeout"),
            output_key_max: 0,
            out_color_profile: h!("Av1OutputColorProfile"),
            out_transfer: h!("Av1OutputColorTransferChar"),
            out_primaries: h!("Av1OutputColorPrimaries"),
            in_color_profile: h!("Av1InputColorProfile"),
            in_transfer: h!("Av1InputColorTransferChar"),
            in_primaries: h!("Av1InputColorPrimaries"),
            in_full_range: h!("Av1InputFullRangeColor"),
            hdr_metadata: Some(h!("Av1InHDRMetadata")),
            intra_refresh: None,
            ltr: Some(LtrProps {
                max_ltr_frames: h!("Av1MaxNumLTRFrames"),
                max_num_ref_frames: h!("Av1MaxNumRefFrames"),
                mark_ltr_index: h!("Av1MarkCurrentWithLTRIndex"),
                force_ltr_bitfield: h!("Av1ForceLTRReferenceBitfield"),
                ltr_mode: h!("Av1LTRMode"),
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

// Owned-pointer guards: Terminate before Release (amfenc.c teardown order). Each holds one
// owned reference to a non-null AMF object, and `amf_sys.rs` pins every vtable slot called
// here: that pair is the proof behind every `unsafe` call in the methods below. Names are
// `HSTRING`s, which are always NUL-terminated.

impl AmfLib {
    /// `CreateContext` → an owned [`Ctx`].
    fn create_context(&self) -> Result<Ctx> {
        let mut ctx: *mut sys::AmfContext = ptr::null_mut();
        // SAFETY: `factory` is the live process singleton (null-checked at load, DLL never
        // unloaded); on AMF_OK `ctx` holds one owned reference.
        let r = unsafe { ((*(*self.factory).vtbl).create_context)(self.factory, &mut ctx) };
        amf_ok(r, "AMF CreateContext")?;
        if ctx.is_null() {
            bail!("AMF CreateContext returned null");
        }
        Ok(Ctx(ctx))
    }

    /// `CreateComponent(id)` on `ctx` → an owned [`Component`].
    fn create_component(&self, ctx: &Ctx, id: &HSTRING) -> Result<Component> {
        let mut comp: *mut sys::AmfComponent = ptr::null_mut();
        // SAFETY: live factory as above and a live owned context; `id` is NUL-terminated. On
        // AMF_OK `comp` holds one owned reference.
        let r = unsafe {
            ((*(*self.factory).vtbl).create_component)(self.factory, ctx.0, id.as_ptr(), &mut comp)
        };
        amf_ok(r, "AMF CreateComponent")?;
        if comp.is_null() {
            bail!("AMF CreateComponent returned null");
        }
        Ok(Component(comp))
    }
}

/// Owned `AMFComponent*` — `Flush` + `Terminate` + `Release` on drop.
///
/// Shared with the retrieve thread through an `Arc`. `init`, `flush` and `terminate` take
/// `&mut self`, so they only run once that thread has been joined and dropped its clone.
struct Component(*mut sys::AmfComponent);

// SAFETY: AMF objects have no thread affinity; the last reference is released wherever the
// guard drops.
unsafe impl Send for Component {}
// SAFETY: shared the way AMF's submit/retrieve split runs a component: the retrieve thread calls
// only `query_output`, while SubmitInput, Drain and property calls stay on the encode thread.
// Init, Flush and Terminate need `&mut self`, which no thread gets while the other holds a clone.
unsafe impl Sync for Component {}

impl Component {
    /// `SetProperty`, raw result.
    fn set_property(&self, name: &HSTRING, value: AmfVariant) -> sys::AmfResult {
        // SAFETY: live component (guard proof above); `name` outlives the call. AMF copies the
        // variant in, AddRef'ing an interface payload.
        unsafe { ((*(*self.0).vtbl).set_property)(self.0, name.as_ptr(), value) }
    }

    /// `GetProperty`; `None` when the component declines.
    fn get_property(&self, name: &HSTRING) -> Option<AmfVariant> {
        let mut v = AmfVariant::zeroed();
        // SAFETY: live component; `v` is a local out-param AMF fills.
        let r = unsafe { ((*(*self.0).vtbl).get_property)(self.0, name.as_ptr(), &mut v) };
        (r == sys::AMF_OK).then_some(v)
    }

    /// Set one component property. Required: abort the open. Optional: log and continue
    /// (VCN/driver variance). Returns whether it applied, so callers gate advertised caps on the
    /// driver's answer.
    fn set_prop(&self, name: &HSTRING, value: AmfVariant, required: bool) -> Result<bool> {
        let r = self.set_property(name, value);
        if r == sys::AMF_OK {
            return Ok(true);
        }
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

    /// Set the rate property `name` to `ask`, or to the highest rate under it the encoder
    /// takes: a VCN refuses a rate over its ceiling where other encoders clamp. Bisected on
    /// the driver's own answer, to 1 Mbit/s, and kept in `ceiling` (0 = not known) so the
    /// next ask over it is one call. `None` when it takes no rate at all.
    fn set_rate(&self, name: &HSTRING, ask: i64, ceiling: &AtomicI64) -> Option<i64> {
        let set = |bps: i64| self.set_property(name, AmfVariant::from_i64(bps));
        let ask = match ceiling.load(Ordering::Relaxed) {
            0 => ask,
            known => ask.min(known),
        };
        match set(ask) {
            sys::AMF_OK => return Some(ask),
            sys::AMF_OUT_OF_RANGE => {}
            _ => return None,
        }
        let (mut taken, mut refused) = (0, ask);
        while refused - taken > 1_000_000 {
            let mid = taken + (refused - taken) / 2;
            if set(mid) == sys::AMF_OK {
                taken = mid;
            } else {
                refused = mid;
            }
        }
        // The last probe may be a refused one, so the rate returned is set once more.
        let fit = (taken > 0 && set(taken) == sys::AMF_OK).then_some(taken)?;
        ceiling.store(fit, Ordering::Relaxed);
        tracing::info!(
            property = %name,
            asked_mbps = ask / 1_000_000,
            fit_mbps = fit / 1_000_000,
            "AMF rate fitted under the encoder's ceiling"
        );
        Some(fit)
    }

    /// `GetProperty` BOOL. `None` on decline or non-BOOL.
    fn get_prop_bool(&self, name: &HSTRING) -> Option<bool> {
        self.get_property(name)?.as_bool()
    }

    /// `GetProperty` INT64 after any internal clamp. `None` on decline or non-INT64 — never
    /// treat as 0.
    fn get_prop_i64(&self, name: &HSTRING) -> Option<i64> {
        self.get_property(name)?.as_i64()
    }

    /// `Init` at `format` (`AMF_SURFACE_*`) and `width`×`height`.
    fn init(&mut self, format: i32, width: i32, height: i32) -> sys::AmfResult {
        // SAFETY: live component, and `&mut self` means no other thread is inside it.
        unsafe { ((*(*self.0).vtbl).init)(self.0, format, width, height) }
    }

    /// `Flush`: drop everything queued. Legal on a wedge.
    fn flush(&mut self) -> sys::AmfResult {
        // SAFETY: as `init`.
        unsafe { ((*(*self.0).vtbl).flush)(self.0) }
    }

    /// `Terminate`: releases every input surface; `init` may follow.
    fn terminate(&mut self) -> sys::AmfResult {
        // SAFETY: as `init`.
        unsafe { ((*(*self.0).vtbl).terminate)(self.0) }
    }

    /// `Drain`: end of stream; the owed AUs surface through `QueryOutput` until AMF_EOF.
    fn drain(&self) -> sys::AmfResult {
        // SAFETY: live component; Drain beside QueryOutput is AMF's end-of-stream pattern.
        unsafe { ((*(*self.0).vtbl).drain)(self.0) }
    }

    /// `SubmitInput`. The component takes its own reference to `surface`.
    fn submit_input(&self, surface: &OwnedData) -> sys::AmfResult {
        // SAFETY: live component and a live surface, both owned by their guards.
        unsafe { ((*(*self.0).vtbl).submit_input)(self.0, surface.0) }
    }

    /// `QueryOutput`: the result plus the output, owned, when there is one. The guard is
    /// built lazily: one around a null pointer would release through it when dropped.
    fn query_output(&self) -> (sys::AmfResult, Option<OwnedData>) {
        let mut data: *mut sys::AmfData = ptr::null_mut();
        // SAFETY: live component; `data` is a local out-param that holds one owned reference
        // whenever AMF fills it.
        let r = unsafe { ((*(*self.0).vtbl).query_output)(self.0, &mut data) };
        (r, (!data.is_null()).then(|| OwnedData(data)))
    }
}

impl Drop for Component {
    fn drop(&mut self) {
        // Flush before Terminate: an unflushed session can occupy AMD's limited VCN slots so
        // the next Init returns AMF_OK but never emits an AU. Best-effort on a wedge.
        self.flush();
        let tr = self.terminate();
        if tr != sys::AMF_OK {
            tracing::debug!(
                result = %format!("{} ({tr})", result_name(tr)),
                "AMF component Terminate returned non-OK on drop"
            );
        }
        // SAFETY: the one reference this guard owns, released once; `self.0` is not used after.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFContext*` — `Terminate` + `Release` on drop.
struct Ctx(*mut sys::AmfContext);

impl Ctx {
    /// `InitDX11` on `device`; `None` lets AMF create its own.
    ///
    /// # Safety
    /// `device` outlives this context.
    unsafe fn init_dx11(&self, device: Option<&ID3D11Device>) -> sys::AmfResult {
        let raw = device.map_or(ptr::null_mut(), |d| d.as_raw());
        // SAFETY: live context (guard proof above); `raw` is null or a device the caller keeps
        // alive for the context's life.
        unsafe { ((*(*self.0).vtbl).init_dx11)(self.0, raw, sys::AMF_DX11_1) }
    }

    /// `AllocBuffer` of `size` bytes of host memory.
    fn alloc_host_buffer(&self, size: usize) -> Result<Buffer> {
        let mut buf: *mut sys::AmfBuffer = ptr::null_mut();
        // SAFETY: live context; on AMF_OK `buf` holds one owned reference.
        let r = unsafe {
            ((*(*self.0).vtbl).alloc_buffer)(self.0, sys::AMF_MEMORY_HOST, size, &mut buf)
        };
        amf_ok(r, "AMF AllocBuffer")?;
        if buf.is_null() {
            bail!("AMF AllocBuffer returned null");
        }
        Ok(Buffer(buf))
    }

    /// Wrap `texture` as an `AMFSurface`, viewed through its `AMFData` prefix.
    ///
    /// # Safety
    /// `texture` lives on this context's device and stays alive until the component has let go
    /// of the surface: its AU is out, or Terminate has returned. The AMF docs do not say the
    /// surface holds a reference to it.
    unsafe fn create_surface_from_dx11(&self, texture: &ID3D11Texture2D) -> Result<OwnedData> {
        let mut surf: *mut sys::AmfData = ptr::null_mut();
        // SAFETY: live context; `texture` is live for as long as AMF may read it (caller
        // contract); no observer. On AMF_OK `surf` holds one owned reference.
        let r = unsafe {
            ((*(*self.0).vtbl).create_surface_from_dx11_native)(
                self.0,
                texture.as_raw(),
                &mut surf,
                ptr::null_mut(),
            )
        };
        amf_ok(r, "AMF CreateSurfaceFromDX11Native")?;
        if surf.is_null() {
            bail!("AMF CreateSurfaceFromDX11Native returned null");
        }
        Ok(OwnedData(surf))
    }
}

impl Drop for Ctx {
    fn drop(&mut self) {
        // SAFETY: the one reference this guard owns, terminated then released once (`Inner`
        // declares `comp` before `ctx`, so components drop first).
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

/// Owned `AMFData*` (a surface, or an encoder output) — `Release` on drop.
struct OwnedData(*mut sys::AmfData);

impl OwnedData {
    /// `SetPts` in 100 ns units.
    fn set_pts(&self, pts: i64) {
        // SAFETY: live data object (guard proof above).
        unsafe { ((*(*self.0).vtbl).set_pts)(self.0, pts) }
    }

    /// `SetProperty`, raw result.
    fn set_property(&self, name: &HSTRING, value: AmfVariant) -> sys::AmfResult {
        // SAFETY: live data object; `name` outlives the call; the variant is copied in.
        unsafe { ((*(*self.0).vtbl).set_property)(self.0, name.as_ptr(), value) }
    }

    /// `GetProperty`; `None` when the object has no such property.
    fn get_property(&self, name: &HSTRING) -> Option<AmfVariant> {
        let mut v = AmfVariant::zeroed();
        // SAFETY: live data object; `v` is a local out-param AMF fills.
        let r = unsafe { ((*(*self.0).vtbl).get_property)(self.0, name.as_ptr(), &mut v) };
        (r == sys::AMF_OK).then_some(v)
    }

    /// `QueryInterface(IID_AMFBuffer)`: the same object as an owned [`Buffer`].
    fn query_buffer(&self) -> Result<Buffer> {
        let mut buf: *mut c_void = ptr::null_mut();
        // SAFETY: live data object; the IID is a static. On AMF_OK `buf` holds one AddRef'd
        // reference to an `AMFBuffer`.
        let r =
            unsafe { ((*(*self.0).vtbl).query_interface)(self.0, &sys::IID_AMF_BUFFER, &mut buf) };
        amf_ok(r, "AMF QueryInterface(AMFBuffer)")?;
        if buf.is_null() {
            bail!("AMF output is not an AMFBuffer");
        }
        Ok(Buffer(buf.cast()))
    }
}

impl Drop for OwnedData {
    fn drop(&mut self) {
        // SAFETY: the one reference this guard owns, released once.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// Owned `AMFBuffer*` — `Release` on drop.
struct Buffer(*mut sys::AmfBuffer);

impl Buffer {
    /// Host pointer, valid while `self` lives. Null for a buffer without host memory.
    fn native(&self) -> *mut c_void {
        // SAFETY: live buffer (guard proof above); GetNative only reads it.
        unsafe { ((*(*self.0).vtbl).get_native)(self.0) }
    }

    /// Size in bytes.
    fn size(&self) -> usize {
        // SAFETY: live buffer; GetSize only reads it.
        unsafe { ((*(*self.0).vtbl).get_size)(self.0) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: the one reference this guard owns, released once.
        unsafe {
            ((*(*self.0).vtbl).release)(self.0);
        }
    }
}

/// The AMF surface format and the ring's texture format for an input this encoder takes.
fn input_formats(input: PixelFormat) -> Option<(i32, DXGI_FORMAT)> {
    match input {
        PixelFormat::Nv12 => Some((sys::AMF_SURFACE_NV12, DXGI_FORMAT_NV12)),
        PixelFormat::P010 => Some((sys::AMF_SURFACE_P010, DXGI_FORMAT_P010)),
        PixelFormat::Bgra => Some((sys::AMF_SURFACE_BGRA, DXGI_FORMAT_B8G8R8A8_UNORM)),
        PixelFormat::RgbaF16 => Some((sys::AMF_SURFACE_RGBA_F16, DXGI_FORMAT_R16G16B16A16_FLOAT)),
        _ => None,
    }
}

/// Input texture ring depth. AMF keeps reading a slot until its AU is retrieved, so at most
/// `RING - 1` frames may be in flight. `submit` drains before reuse. Shallow enough that
/// back-pressure starts after a few frames, not after AMF's 16-deep input queue.
const RING: usize = 6;

/// The [`RING`] copy targets for `input` at `width`×`height` on `device`.
fn input_ring(
    device: &ID3D11Device,
    input: PixelFormat,
    width: u32,
    height: u32,
) -> Result<Vec<ID3D11Texture2D>> {
    let (_, format) = input_formats(input).context("AMF input format")?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    (0..RING)
        .map(|_| {
            let mut t: Option<ID3D11Texture2D> = None;
            // SAFETY: a complete description, no initial data; `t` is a local out-param.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }
                .context("CreateTexture2D (AMF input ring)")?;
            t.context("AMF input ring texture")
        })
        .collect()
}

/// Process-wide count of successful `Init`s. A climbing number with no following first-AU log
/// ([`AuQueue::take_ready`]) is a silent VCN-session wedge.
static AMF_CONTEXTS_OPENED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long the retrieve thread lets `QueryOutput` block before it looks at the stop flag. The
/// only cost of a bigger number is teardown latency; the only cost of a smaller one is wake-ups
/// on an idle encoder.
const QUERY_TIMEOUT_MS: i64 = 50;

/// What the retrieve thread and the encode thread share: `(pts_ns, forced-IDR, recovery-anchor)`
/// per submitted frame. The component is deliberately not in here: AMF documents `SubmitInput` and
/// `QueryOutput` as a thread pair, so only the queue needs a lock, and it is never held across a
/// `QueryOutput`. The extra `bool` is "the component has answered EOF since the last Drain".
type OutQueue = AuQueue<(u64, bool, bool), bool>;

/// The retrieve thread and its queue. The thread owns every `QueryOutput` on the component, so
/// the encode thread never waits on VCN — it takes finished AUs off the queue, and a caller that
/// parks on handles takes the queue's signal instead.
///
/// Dropping this stops and joins, and the joined thread has dropped its `Arc` of the component.
/// [`Inner`] declares it first so the component's last reference, and its Terminate, stay on the
/// encode thread; [`AmfEncoder::reset`] stops it by hand to get `&mut` for the re-Init.
struct Retrieve {
    q: Arc<OutQueue>,
    thread: RetrieveThread,
}

impl Retrieve {
    /// Start a thread draining `comp`. `blocking` says `QueryTimeout` took, so the loop parks in
    /// `QueryOutput` instead of sampling.
    fn start(comp: Arc<Component>, props: &CodecProps, blocking: bool) -> Result<Self> {
        let q = Arc::new(OutQueue::new("AMF")?);
        let (odt, okm) = (props.output_data_type, props.output_key_max);
        let t_q = q.clone();
        let thread = RetrieveThread::spawn("punktfunk-amf-out", move |stop| {
            retrieve_loop(comp, odt, okm, blocking, &t_q, &stop)
        })?;
        Ok(Self { q, thread })
    }
}

/// Block in `QueryOutput` and hand finished AUs to the encode thread. `QueryOutput` is the only
/// call this thread makes on `comp`; its clone drops when the loop exits.
fn retrieve_loop(
    comp: Arc<Component>,
    output_data_type: &'static HSTRING,
    output_key_max: i64,
    blocking: bool,
    q: &OutQueue,
    stop: &AtomicBool,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    // An empty answer this fast was a poll, whatever `blocking` says: HEVC before its first
    // submit, or a runtime that took the timeout and ignores it. Unpaced, that spins a core.
    const POLL: std::time::Duration = std::time::Duration::from_millis(1);
    while !stop.load(Ordering::Acquire) {
        let asked = std::time::Instant::now();
        let pace = || {
            if !blocking || asked.elapsed() < POLL {
                std::thread::sleep(std::time::Duration::from_micros(250));
            }
        };
        match drain_one_output(&comp, output_data_type, output_key_max) {
            Ok(DrainOutcome::Frame { data, key_prop }) => {
                let mut g = q.lock();
                // An AU with no submit behind it would pair every later AU with the wrong
                // pts, keyframe flag and anchor; that is a reset, never a renumbering.
                let Some((pts_ns, forced, recovery_anchor)) = g.pending.pop_front() else {
                    q.fail(&mut g, || {
                        "AMF produced an AU with no submit pending".into()
                    });
                    return;
                };
                q.publish(
                    &mut g,
                    EncodedFrame {
                        data,
                        pts_ns,
                        keyframe: key_prop || forced,
                        recovery_anchor,
                        recovery_point: false,
                        recovery_close: false,
                        chunk_aligned: false,
                    },
                );
            }
            // `flush` owns the queue across a drain; a clear here could land on a frame queued
            // behind the Drain. EOF repeats on every call while the component sits drained and
            // the loop only exits on `stop`, so pace it like NotReady.
            Ok(DrainOutcome::Eof) => {
                q.lock().extra = true;
                pace();
            }
            // Without `QueryTimeout` the call is a poll, sampled at this interval.
            Ok(DrainOutcome::NotReady) => pace(),
            Err(e) => {
                q.fail(&mut q.lock(), || format!("{e:#}"));
                return;
            }
        }
    }
}

/// Ask the component to let `QueryOutput` block. Static: it takes hold at the next `Init`, and
/// set after one it is accepted and ignored. `false` means the driver declined and the
/// retrieve thread samples instead — older AMF runtimes have no such property.
fn set_query_timeout(comp: &Component, name: &HSTRING) -> bool {
    comp.set_prop(name, AmfVariant::from_i64(QUERY_TIMEOUT_MS), false)
        .unwrap_or(false)
}

/// Live AMF session. Field order: `retrieve` stops and joins first, then `comp` drops
/// (Flush+Terminate+Release), then `ctx`, then the device and textures they used.
struct Inner {
    retrieve: Retrieve,
    /// Shared with the retrieve thread, which holds the only other clone.
    comp: Arc<Component>,
    ctx: Ctx,
    /// Capturer device — kept alive for `ctx`, and where the ring textures are made.
    device: ID3D11Device,
    /// Immediate context for the ring copy. The AMF runtime uses it too, so it runs
    /// multithread-protected.
    dctx: ID3D11DeviceContext,
    /// The copy targets, made by the first submit that copies ([`input_ring`]): a caller
    /// encoding in place never pays for them. Empty until then.
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    /// A reference to every texture AMF may still be reading, newest last, capped at [`RING`].
    /// The AMF docs do not say whether `CreateSurfaceFromDX11Native` AddRefs the texture it
    /// wraps, so this keeps it alive; in-flight is never more than `RING`, so the last `RING`
    /// entries always cover whatever the hardware is on. Emptied only after Terminate.
    held: VecDeque<ID3D11Texture2D>,
    /// Last `*InHDRMetadata` pushed to this component — re-push on change or rebuild.
    hdr_pushed: Option<pf_frame::HdrMeta>,
}

pub struct AmfEncoder {
    codec: Codec,
    props: CodecProps,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    /// The highest target rate this encoder took after refusing one, 0 until it refuses.
    rate_ceiling: AtomicI64,
    /// What every submitted texture holds: NV12, P010, BGRA or FP16 ([`input_formats`]).
    input: PixelFormat,
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
    /// The driver keeps the slots a force leaves out (`LTRMode` 1): a mark awaiting the
    /// client's confirmation survives the forces before it.
    ltr_keep: bool,
    ltr: LtrMirror,
    ltr_mark_interval: i64,
    /// The newest frame the client confirmed, while the host holds confirmed references.
    reference_floor: Option<crate::Acked>,
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

// SAFETY: raw AMF pointers are not auto-`Send`. AMF objects have no thread affinity, and every
// call on this encoder runs on the one thread that owns it; only the retrieve thread shares the
// component (see `Retrieve`). The immediate context it shares with the AMF runtime is
// multithread-protected in `ensure_inner`.
unsafe impl Send for AmfEncoder {}

impl AmfEncoder {
    /// Open the native AMF encoder. Fails the session when the runtime is missing/too old or the
    /// capture format is none of [`input_formats`]. AV1 is probed up front (RDNA3+; same
    /// [`probe_can_encode`] as the advertisement).
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
        // Any other capture format has no native input path, and there is no readback.
        if input_formats(format).is_none() {
            bail!(
                "native AMF takes NV12, P010, BGRA or FP16 textures; capturer delivered {format:?}"
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
            rate_ceiling: AtomicI64::new(0),
            input: format,
            ten_bit,
            hdr,
            inner: None,
            bound_device: 0,
            frame_idx: 0,
            force_kf: false,
            hdr_meta: None,
            ir_active: false,
            ltr_active: false,
            ltr_keep: false,
            ltr: LtrMirror::default(),
            ltr_mark_interval: ltr_mark_interval(fps),
            reference_floor: None,
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
    /// keep properties on every driver). Returns `(ir_active, ltr_active, ltr_keep)` as
    /// requested AND accepted. The first two are mutually exclusive — see [`Self::ltr_wanted`].
    fn apply_static_props(&self, comp: &Component) -> Result<(bool, bool, bool)> {
        let p = &self.props;
        // Usage first: it fully configures the parameter set; everything after is an override.
        comp.set_prop(
            p.usage,
            AmfVariant::from_i64(usage_from_knobs(self.codec)),
            true,
        )?;
        comp.set_prop(p.rc_method, AmfVariant::from_i64(p.rc_cbr), true)?;
        let ask = self.bitrate_bps.min(i64::MAX as u64) as i64;
        let Some(bps) = comp.set_rate(p.target_bitrate, ask, &self.rate_ceiling) else {
            bail!(
                "AMF SetProperty({}) took no rate up to {ask}",
                p.target_bitrate
            );
        };
        comp.set_prop(p.peak_bitrate, AmfVariant::from_i64(bps), true)?;
        comp.set_prop(p.framerate, AmfVariant::from_rate(self.fps.max(1), 1), true)?;
        comp.set_prop(
            p.vbv_size,
            AmfVariant::from_i64(self.vbv_bits(bps as u64)),
            false,
        )?;
        comp.set_prop(p.enforce_hrd, AmfVariant::from_bool(true), false)?;
        comp.set_prop(p.filler_data, AmfVariant::from_bool(false), false)?;
        // The latency usages default this on: a frame over the one-frame VBV is then skipped
        // and the reference stays stale, so the next frame is over budget too — a scene cut
        // freezes the picture until the content drifts back to it.
        let usage_default = comp.get_prop_bool(p.skip_frame);
        comp.set_prop(p.skip_frame, AmfVariant::from_bool(false), false)?;
        tracing::info!(?usage_default, "AMF rate-control frame skip disabled");
        // Latency-first quality; low-latency submit (optional on older VCN).
        comp.set_prop(
            p.quality_preset,
            AmfVariant::from_i64(p.quality_speed),
            false,
        )?;
        comp.set_prop(p.lowlatency, p.lowlatency_value.to_variant(), false)?;
        // No periodic IDR (`i32::MAX` AVC/HEVC; 0 on AV1 = first frame only). Forced type supplies IDRs.
        comp.set_prop(
            p.idr_period,
            AmfVariant::from_i64(p.idr_period_value),
            false,
        )?;
        // Intra-refresh: per-slot units = ceil(total blocks / period). Optional; gates `caps()`.
        let mut ir_active = false;
        let mut ltr_active = false;
        let mut ltr_keep = false;
        if let Some(ltr) = p.ltr.as_ref().filter(|_| self.ltr_wanted()) {
            // LTR needs >1 ref frames and is mutually exclusive with intra-refresh.
            let ref_ok = comp.set_prop(
                ltr.max_num_ref_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            let ltr_ok = comp.set_prop(
                ltr.max_ltr_frames,
                AmfVariant::from_i64(NUM_LTR_SLOTS as i64),
                false,
            )?;
            ltr_active = ref_ok && ltr_ok;
            ltr_keep = ltr_active && comp.set_prop(ltr.ltr_mode, AmfVariant::from_i64(1), false)?;
            if ltr_active {
                tracing::info!(
                    slots = NUM_LTR_SLOTS,
                    keep_unforced = ltr_keep,
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
                ir_active = comp.set_prop(name, AmfVariant::from_i64(per_slot as i64), false)?;
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
                comp.set_prop(h!("BPicturesPattern"), AmfVariant::from_i64(0), false)?;
                // Limited-range YUV out, whichever input the ring holds.
                comp.set_prop(h!("FullRangeColor"), AmfVariant::from_bool(false), false)?;
            }
            Codec::H265 => {
                // In-band VPS/SPS/PPS on every IDR. Forced-IDR surfaces also set `HevcInsertHeader`.
                comp.set_prop(
                    h!("HevcHeaderInsertionMode"),
                    AmfVariant::from_i64(HEVC_HEADER_IDR_ALIGNED),
                    false,
                )?;
                // Studio range out, whichever input the ring holds.
                comp.set_prop(h!("HevcNominalRange"), AmfVariant::from_i64(0), false)?;
                if self.ten_bit {
                    // Main10 + 10-bit surfaces: required — silent 8-bit HDR is worse than failing open.
                    comp.set_prop(
                        h!("HevcProfile"),
                        AmfVariant::from_i64(HEVC_PROFILE_MAIN_10),
                        true,
                    )?;
                    comp.set_prop(
                        h!("HevcColorBitDepth"),
                        AmfVariant::from_i64(COLOR_BIT_DEPTH_10),
                        true,
                    )?;
                }
            }
            Codec::Av1 => {
                // Never B-frames: VCN5 can grow them (H.264 already did on RDNA3+). A B-frame
                // adds a frame of latency and breaks FIFO on the codec with no LTR/IR. Pre-VCN5
                // rejects the names (no-op). HEVC has no B-frame property at all.
                comp.set_prop(h!("Av1BPicturesPattern"), AmfVariant::from_i64(0), false)?;
                comp.set_prop(
                    h!("Av1MaxConsecutiveBPictures"),
                    AmfVariant::from_i64(0),
                    false,
                )?;
                comp.set_prop(
                    h!("Av1AdaptiveMiniGop"),
                    AmfVariant::from_bool(false),
                    false,
                )?;
                // Sequence header OBU on every key frame (self-contained join points).
                comp.set_prop(
                    h!("Av1HeaderInsertionMode"),
                    AmfVariant::from_i64(AV1_HEADER_KEY_ALIGNED),
                    false,
                )?;
                // Default `64X16_ONLY` rejects non-16-multiple heights (1080p). Prefer unrestricted;
                // fall back to 1080p-coded-1082. If neither applies, Init fails.
                let unrestricted = comp.set_prop(
                    h!("Av1AlignmentMode"),
                    AmfVariant::from_i64(AV1_ALIGNMENT_NO_RESTRICTIONS),
                    false,
                )?;
                if !unrestricted && self.height % 16 != 0 {
                    comp.set_prop(
                        h!("Av1AlignmentMode"),
                        AmfVariant::from_i64(AV1_ALIGNMENT_1080P_CODED_1082),
                        false,
                    )?;
                }
                if self.ten_bit {
                    // 10-bit is AV1 Main — only the surface depth needs forcing.
                    comp.set_prop(
                        h!("Av1ColorBitDepth"),
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
        comp.set_prop(p.out_color_profile, AmfVariant::from_i64(profile), self.hdr)?;
        comp.set_prop(p.out_transfer, AmfVariant::from_i64(transfer), self.hdr)?;
        comp.set_prop(p.out_primaries, AmfVariant::from_i64(primaries), self.hdr)?;
        // BGRA in: VCN converts to the studio-range output above. The profile is required —
        // it carries the range, and a runtime left guessing washes the picture out. A refusal
        // fails this open and the caller falls back to NV12.
        if self.input == PixelFormat::Bgra {
            let full_709 = AmfVariant::from_i64(COLOR_PROFILE_FULL_709);
            comp.set_prop(p.in_color_profile, full_709, true)?;
            comp.set_prop(p.in_transfer, AmfVariant::from_i64(TRANSFER_BT709), false)?;
            comp.set_prop(p.in_primaries, AmfVariant::from_i64(PRIMARIES_BT709), false)?;
            comp.set_prop(p.in_full_range, AmfVariant::from_bool(true), false)?;
        }
        // FP16 in is scRGB: linear light on BT.709 primaries. VCN converts it to the PQ
        // output above. A runtime that takes FP16 reads it this way unprompted, so a refused
        // property is not a refused input.
        if self.input == PixelFormat::RgbaF16 {
            comp.set_prop(p.in_transfer, AmfVariant::from_i64(TRANSFER_LINEAR), false)?;
            comp.set_prop(p.in_primaries, AmfVariant::from_i64(PRIMARIES_BT709), false)?;
        }
        Ok((ir_active, ltr_active, ltr_keep))
    }

    /// Build or rebuild the AMF context + component on the capturer's device.
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
        let ctx = lib.create_context()?;
        // SAFETY: plain accessor on a live device.
        let dctx =
            unsafe { device.GetImmediateContext() }.context("ID3D11Device immediate context")?;
        // The AMF runtime drives this immediate context from its own threads (the retrieve
        // thread's QueryOutput included) while this thread copies into the ring on it.
        if let Ok(mt) = dctx.cast::<ID3D11Multithread>() {
            // SAFETY: a device-wide flag on a live context, set before AMF is handed the device.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }
        // SAFETY: `device` outlives `ctx`: borrowed for this call, then kept in `Inner` behind
        // `ctx`, which drops first.
        let r = unsafe { ctx.init_dx11(Some(device)) };
        amf_ok(r, "AMF InitDX11 (capturer device)")?;
        let mut comp = lib.create_component(&ctx, self.props.component)?;
        let (ir_active, ltr_active, ltr_keep) = self.apply_static_props(&comp)?;
        let blocking = set_query_timeout(&comp, self.props.query_timeout);
        let (fmt, _) = input_formats(self.input).context("AMF input format")?;
        amf_ok(
            comp.init(fmt, self.width as i32, self.height as i32),
            "AMF encoder Init",
        )?;
        self.ir_active = ir_active;
        // Rebuilt component has no reference history; drop prior LTR marks.
        self.ltr_active = ltr_active;
        self.ltr_keep = ltr_keep;
        if ltr_active {
            self.ltr = LtrMirror::default();
        }

        // Bump after successful Init so a failed bring-up never counts.
        let context_no = AMF_CONTEXTS_OPENED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        tracing::info!(
            codec = ?self.codec,
            context = context_no,
            device = %format_args!("{:#x}", device.as_raw() as usize),
            width = self.width,
            height = self.height,
            fps = self.fps,
            ring = ?self.input,
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
        // The retrieve thread starts against the initialized component; `Inner` joins it
        // before its own reference drops, and `reset` stops it by hand.
        let comp = Arc::new(comp);
        let retrieve = Retrieve::start(Arc::clone(&comp), &self.props, blocking)?;
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
            device: device.clone(),
            dctx,
            ring: Vec::new(),
            next: 0,
            held: VecDeque::new(),
            hdr_pushed: None,
        });
        Ok(())
    }
}

/// Push HDR mastering metadata as `*InHDRMetadata` (dynamic). Units match [`HdrMeta`]; primary
/// order is the trap: ST.2086 wire is G,B,R → labeled R/G/B fields.
fn push_hdr_metadata(
    ctx: &Ctx,
    comp: &Component,
    name: &HSTRING,
    meta: &pf_frame::HdrMeta,
) -> Result<()> {
    let buf = ctx
        .alloc_host_buffer(std::mem::size_of::<sys::AmfHdrMetadata>())
        .context("HDR metadata")?;
    let native = buf.native().cast::<sys::AmfHdrMetadata>();
    if native.is_null() {
        bail!("AMF HDR metadata buffer has no host pointer");
    }
    // SAFETY: `native` addresses the `size_of::<AmfHdrMetadata>()` host bytes just allocated,
    // live while `buf` is. Host AMFBuffer alignment is unknown, hence the unaligned write.
    unsafe {
        native.write_unaligned(sys::AmfHdrMetadata {
            red_primary: meta.display_primaries[2],
            green_primary: meta.display_primaries[0],
            blue_primary: meta.display_primaries[1],
            white_point: meta.white_point,
            max_mastering_luminance: meta.max_display_mastering_luminance,
            min_mastering_luminance: meta.min_display_mastering_luminance,
            max_content_light_level: meta.max_cll,
            max_frame_average_light_level: meta.max_fall,
        })
    };
    // SetProperty AddRefs the buffer; dropping `buf` leaves the property's reference.
    let r = comp.set_property(name, AmfVariant::from_interface(buf.0.cast()));
    amf_ok(r, "AMF SetProperty(InHDRMetadata)")
}

/// Can this GPU's AMF runtime `Init` a `codec` encoder on the selected render adapter?
/// Tears down before return. `false` on any failure, including no runtime.
pub fn probe_can_encode(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    let Some(device) = pf_frame::dxgi::probe_device(adapter_luid, Default::default()) else {
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
    let Some(device) = pf_frame::dxgi::probe_device(adapter_luid, Default::default()) else {
        return false;
    };
    probe_open_on(&device, codec, true)
}

/// Probe body: context + component + usage + optional 10-bit props + tiny `Init`. `false` on fail.
fn probe_open_on(device: &ID3D11Device, codec: Codec, ten_bit: bool) -> bool {
    let Ok(lib) = try_factory() else { return false };
    let props = codec_props(codec);
    let Ok(ctx) = lib.create_context() else {
        return false;
    };
    // SAFETY: `device` is borrowed for this whole call, and `ctx` drops before it returns.
    if unsafe { ctx.init_dx11(Some(device)) } != sys::AMF_OK {
        return false;
    }
    let Ok(mut comp) = lib.create_component(&ctx, props.component) else {
        return false;
    };
    // Usage must be set before `Init` (header default is N/A).
    if comp.set_property(props.usage, AmfVariant::from_i64(usage_from_knobs(codec))) != sys::AMF_OK
    {
        return false;
    }
    if ten_bit {
        // Same required 10-bit props as a real session — reject here is the probe's answer.
        let depth_props: &[(&HSTRING, i64)] = match codec {
            Codec::H265 => &[
                (h!("HevcProfile"), HEVC_PROFILE_MAIN_10),
                (h!("HevcColorBitDepth"), COLOR_BIT_DEPTH_10),
            ],
            Codec::Av1 => &[(h!("Av1ColorBitDepth"), COLOR_BIT_DEPTH_10)],
            Codec::H264 | Codec::PyroWave => return false,
        };
        for (name, value) in depth_props {
            if comp.set_property(name, AmfVariant::from_i64(*value)) != sys::AMF_OK {
                return false;
            }
        }
    }
    let surface = if ten_bit {
        sys::AMF_SURFACE_P010
    } else {
        sys::AMF_SURFACE_NV12
    };
    comp.init(surface, 640, 480) == sys::AMF_OK
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
/// took, else returns [`DrainOutcome::NotReady`] at once. Only the retrieve thread calls this —
/// AMF documents `SubmitInput` and `QueryOutput` as a submit/retrieve thread pair.
fn drain_one_output(
    comp: &Component,
    output_data_type: &HSTRING,
    output_key_max: i64,
) -> Result<DrainOutcome> {
    let (r, data) = comp.query_output();
    let Some(data) = data else {
        return match r {
            sys::AMF_EOF => Ok(DrainOutcome::Eof),
            sys::AMF_OK | sys::AMF_REPEAT | sys::AMF_NEED_MORE_INPUT => Ok(DrainOutcome::NotReady),
            // Typed failure on this frame (device-lost, …) — caller resets in place.
            other => bail!("AMF QueryOutput failed: {} ({other})", result_name(other)),
        };
    };
    // Keyframe from output type, OR the forced flag so a driver that skips the property still flags.
    let key_prop = data
        .get_property(output_data_type)
        .and_then(|v| v.as_i64())
        .is_some_and(|t| t <= output_key_max);
    let buf = data.query_buffer()?;
    let size = buf.size();
    let native = buf.native();
    if native.is_null() || size == 0 {
        bail!("AMF output buffer is empty");
    }
    // SAFETY: an encoder output is host memory AMF has filled: `native` addresses `size`
    // initialized bytes until `buf` is released, and they are copied out before that.
    let data = unsafe { std::slice::from_raw_parts(native.cast::<u8>(), size) }.to_vec();
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
        anyhow::ensure!(
            captured.format == self.input,
            "captured format {:?} != AMF input ring {:?} (capturer video-processor fallback \
             mid-session — native AMF has no readback path)",
            captured.format,
            self.input
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
        // `&'static` name copies let the surface block set props without re-borrowing
        // `self.props`.
        let ltr_names = self
            .props
            .ltr
            .as_ref()
            .map(|l| (l.mark_ltr_index, l.force_ltr_bitfield));
        if self.ltr_active && !forced && self.ltr_test_force_at == Some(cur_idx) {
            // Spike hook: self-trigger the real invalidate path without a live client.
            let triggered = self.invalidate_ref_frames(cur_idx, cur_idx);
            tracing::info!(
                frame = cur_idx,
                triggered,
                "AMF LTR test hook fired invalidate_ref_frames"
            );
        }
        // An RFI force leaves only its own slot trusted, in either `LTRMode`.
        let LtrStep {
            mark_slot,
            force,
            acked,
        } = if self.ltr_active {
            let interval = self.ltr_mark_interval;
            let (floor, keep) = (self.reference_floor, self.ltr_keep);
            self.ltr.step(forced, cur_idx, interval, floor, keep, true)
        } else {
            LtrStep::default()
        };
        let force_slot = force.map(|(slot, _)| slot);
        let mut recovery_anchor = force_slot.is_some();
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
                    match push_hdr_metadata(&inner.ctx, &inner.comp, name, &m) {
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
        inner
            .retrieve
            .q
            .wait_until(INPUT_DRAIN_BUDGET, "AMF output", |o| o.pending.len() < cap)?;
        let slot = inner.next % RING;
        inner.next += 1;
        // The texture the hardware will read: the caller's own when it declared a depth deep
        // enough to leave it alone, else our copy of it.
        let source = if in_place.is_some() {
            frame.texture.clone()
        } else {
            if inner.ring.is_empty() {
                inner.ring = input_ring(&inner.device, self.input, self.width, self.height)?;
            }
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = inner.ring[slot].cast().context("ring -> resource")?;
            // SAFETY: `src`/`dst` are same-format, same-size textures on one device (the ring is
            // rebuilt on a device change); the immediate context is multithread-protected.
            unsafe {
                inner
                    .dctx
                    .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None)
            };
            inner.ring[slot].clone()
        };
        // Kept alive until its AU is out (see `Inner::held`).
        inner.held.push_back(source.clone());
        while inner.held.len() > RING {
            inner.held.pop_front();
        }
        // SAFETY: `source` is on the context's device (`ensure_inner` rebinds on a device
        // change) and `held` keeps it until RING newer submits or Terminate; in-flight never
        // exceeds RING.
        let surf = unsafe { inner.ctx.create_surface_from_dx11(&source) }?;
        surf.set_pts(pts_100ns);
        if forced {
            // Forced IDR/KEY + in-band headers. Log-and-continue: reject still encodes.
            let r = surf.set_property(
                self.props.force_picture_type,
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
                    let _ = surf.set_property(h!("InsertSPS"), AmfVariant::from_bool(true));
                    let _ = surf.set_property(h!("InsertPPS"), AmfVariant::from_bool(true));
                }
                Codec::H265 => {
                    let _ = surf.set_property(h!("HevcInsertHeader"), AmfVariant::from_bool(true));
                }
                // KEY_FRAME_ALIGNED already puts a sequence header OBU on every key frame.
                Codec::Av1 => {}
                Codec::PyroWave => unreachable!("PyroWave never opens the AMF backend"),
            }
        }
        // LTR mark/force decided above. Best-effort: reject leaves the client on IDR fallback.
        if let Some((mark_name, force_name)) = ltr_names {
            if let Some(slot) = mark_slot {
                let r = surf.set_property(mark_name, AmfVariant::from_i64(slot as i64));
                if r != sys::AMF_OK {
                    tracing::warn!(
                        slot,
                        result = result_name(r),
                        amf_code = r,
                        "AMF LTR mark rejected"
                    );
                    // The mirror must not claim a slot the hardware never marked.
                    self.ltr.slots[slot] = None;
                }
            }
            if let Some(slot) = force_slot {
                let r = surf.set_property(force_name, AmfVariant::from_i64(1_i64 << slot));
                if r == sys::AMF_OK {
                    if !acked {
                        tracing::info!(
                            slot,
                            frame = cur_idx,
                            "AMF LTR-RFI: re-referencing known-good LTR (clean recovery, no IDR)"
                        );
                    }
                } else {
                    tracing::warn!(
                        slot,
                        result = result_name(r),
                        amf_code = r,
                        "AMF LTR force-reference rejected — forcing an IDR on the next frame"
                    );
                    // The host booked a recovery on this frame; make the next one real. This
                    // one still predicts from the damage, so it lifts no freeze.
                    self.force_kf = true;
                    recovery_anchor = false;
                }
            }
        }
        // Queued before the component takes the frame: the retrieve thread can pop for it the
        // moment SubmitInput returns, so a push after that races an empty queue. A refusal
        // below takes the entry back off.
        inner
            .retrieve
            .q
            .lock()
            .pending
            .push_back((captured.pts_ns, forced, recovery_anchor));
        let mut r = inner.comp.submit_input(&surf);
        // AMF_INPUT_FULL is "busy, drain and retry", not a wedge. Re-submit the same surface.
        if r == sys::AMF_INPUT_FULL {
            let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
            loop {
                // The retrieve thread drains; this only re-offers the same surface until a
                // slot opens, on the same budget the drain loop used to run on.
                std::thread::sleep(std::time::Duration::from_micros(250));
                r = inner.comp.submit_input(&surf);
                if r != sys::AMF_INPUT_FULL || std::time::Instant::now() >= deadline {
                    break;
                }
            }
        }
        // NEED_MORE_INPUT = accepted; no AU owed for this submit alone.
        if !matches!(r, sys::AMF_OK | sys::AMF_NEED_MORE_INPUT) {
            inner.retrieve.q.lock().pending.pop_back();
            if r == sys::AMF_INPUT_FULL {
                bail!("AMF SubmitInput stayed AMF_INPUT_FULL past the drain budget — wedged");
            }
            bail!("AMF SubmitInput failed: {} ({r})", result_name(r));
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

    /// Force the next submit to re-reference the newest LTR marked before `[first, last]`
    /// ([`LtrMirror::invalidate`]); that AU ships `recovery_anchor`. `true` = usable pre-loss
    /// LTR (caller must not also IDR); `false` = fall back to keyframe.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        // No live LTR or a nonsense range → caller IDRs.
        if !self.ltr_active || first < 0 || first > last {
            return false;
        }
        match self.ltr.invalidate(first, self.reference_floor.as_ref()) {
            Some((slot, ltr_frame)) => {
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
                tracing::info!(
                    first,
                    last,
                    "AMF LTR-RFI: no live LTR older than the loss — falling back to IDR recovery"
                );
                false
            }
        }
    }

    fn set_reference_floor(&mut self, acked: Option<crate::Acked>) {
        self.reference_floor = acked;
    }

    /// Withdraw anchor trust from every live LTR and drop a queued force
    /// ([`LtrMirror::distrust`]); the trait docs carry the why.
    fn distrust_references(&mut self) {
        let live = self.ltr.distrust();
        if live > 0 {
            tracing::debug!(
                live,
                "AMF LTR-RFI: client reported unrepaired damage — withdrawing anchor trust from \
                 every live LTR (cleared by the next re-mark or IDR)"
            );
        }
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

    /// Wait up to [`poll_budget_ms`] for the retrieve thread's oldest AU. Expiry is `Ok(None)`;
    /// the watchdog arbitrates a real wedge. Any AU proves this context encodes, which ends
    /// the no-output streak.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let Some(inner) = self.inner.as_ref() else {
            return Ok(None);
        };
        let au = inner.retrieve.q.take_ready(poll_budget_ms(self.fps))?;
        if au.is_some() {
            self.resets_without_output = 0;
        }
        Ok(au)
    }

    /// The retrieve thread's signal, once a component exists. Before the lazy open there is
    /// nothing to wait on, which a caller reads as "no completion signal" and polls instead.
    fn ready_event(&self) -> Option<isize> {
        self.inner.as_ref().map(|i| i.retrieve.q.raw())
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
        // Taken so the rebuild can borrow `self` beside it; put back only once it is live again.
        let Some(mut inner) = self.inner.take() else {
            return true; // next submit rebuilds lazily
        };
        // Second no-output reset: the fault is the context.
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "AMF stall persisted across in-place re-Init — full context teardown, reopening a \
                 fresh context (next submit)"
            );
            drop(inner);
            self.bound_device = 0;
            self.ir_active = false;
            self.ltr_active = false;
            return true;
        }
        // Stop and join before Terminate: the retrieve thread is inside `QueryOutput` on this
        // very component, and a re-Init under it would run against a terminated one.
        inner.retrieve.thread.stop_and_join();
        inner.retrieve.q.reset(); // owed AUs forfeited; rebuilt stream restarts at IDR
        inner.next = 0; // the rebuilt component's first frame is `opening` again
        inner.hdr_pushed = None; // re-Init'd component needs HDR metadata again

        // The format the frames arrive in, as at the open: re-Init'd as NV12, a BGRA session
        // takes every surface and never returns an access unit.
        let fmt = input_formats(self.input).map(|(fmt, _)| fmt);
        let mut blocking = false;
        // The joined thread dropped its clone, so this is the only reference.
        let rebuilt = match Arc::get_mut(&mut inner.comp) {
            Some(comp) => {
                // Legal on a wedge; results ignored.
                comp.flush();
                comp.terminate();
                // VCN may read an input surface until Terminate returns; only then can they go.
                inner.held.clear();
                match (self.apply_static_props(comp), fmt) {
                    (Ok((ir, ltr, keep)), Some(fmt)) => {
                        self.ir_active = ir;
                        // Re-Init voids reference history; drop prior LTR marks.
                        self.ltr_active = ltr;
                        self.ltr_keep = keep;
                        self.ltr = LtrMirror::default();
                        blocking = set_query_timeout(comp, self.props.query_timeout);
                        comp.init(fmt, self.width as i32, self.height as i32) == sys::AMF_OK
                    }
                    _ => false,
                }
            }
            // Unreachable once joined; a failed rebuild tears the context down.
            None => false,
        };
        if rebuilt {
            // The component is live again, so it needs its retrieve thread back. Without one no
            // AU would ever be taken off it and the rebuild would read as a second wedge.
            match Retrieve::start(Arc::clone(&inner.comp), &self.props, blocking) {
                Ok(r) => {
                    inner.retrieve = r;
                    self.inner = Some(inner);
                    tracing::info!(
                        "AMF encoder rebuilt in place (Terminate + re-Init on the same context)"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "AMF rebuilt but its retrieve thread would not start — reopening lazily"
                    );
                    drop(inner);
                    self.bound_device = 0;
                }
            }
        } else {
            self.ir_active = false;
            self.ltr_active = false;
            tracing::warn!("AMF in-place re-Init failed — full context teardown, reopening lazily");
            drop(inner);
            self.bound_device = 0;
        }
        true
    }

    /// `TargetBitrate` via `GetProperty`. `None` before lazy open or on decline — caller keeps
    /// the requested rate. Without this, ABR never learns `encoder_ceiling_kbps` on AMD.
    fn applied_bitrate_bps(&self) -> Option<u64> {
        let inner = self.inner.as_ref()?;
        inner
            .comp
            .get_prop_i64(self.props.target_bitrate)
            .filter(|&b| b > 0)
            .map(|b| b as u64)
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let bps_i = bps.min(i64::MAX as u64) as i64;
        let Some(inner) = self.inner.as_ref() else {
            // Lazy open applies the new rate via `apply_static_props`.
            self.bitrate_bps = bps;
            return true;
        };
        // Target/Peak/VBV are dynamic: SetProperty retargets without Terminate (no IDR).
        let applied = {
            let p = &self.props;
            let comp = &inner.comp;
            let ceiling = &self.rate_ceiling;
            let fit = comp
                .set_rate(p.target_bitrate, bps_i, ceiling)
                .filter(|&fit| {
                    comp.set_prop(p.peak_bitrate, AmfVariant::from_i64(fit), false)
                        .unwrap_or(false)
                });
            if let Some(fit) = fit {
                // Optional VBV rescale; decline keeps the old buffer (HRD absorbs the mismatch).
                let vbv = self.vbv_bits(fit as u64);
                let _ = comp.set_prop(p.vbv_size, AmfVariant::from_i64(vbv), false);
            }
            fit
        };
        let Some(bps) = applied.map(|fit| fit as u64) else {
            // Half-applied pair is fine: the rebuild fallback re-authors from scratch.
            tracing::warn!(
                mbps = bps / 1_000_000,
                "AMF declined the dynamic bitrate retarget — falling back to a rebuild"
            );
            return false;
        };
        self.bitrate_bps = bps; // reset()/re-Init re-apply the new rate
        true
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // Drain = EOS; remaining AUs surface until AMF_EOF.
        inner.retrieve.q.lock().extra = false;
        let r = inner.comp.drain();
        if r != sys::AMF_OK {
            tracing::debug!(
                result = result_name(r),
                amf_code = r,
                "AMF Drain returned non-OK at flush"
            );
        }
        // The owed AUs surface on the retrieve thread; wait for the last of them here so no
        // frame submitted after this can be paired with one, and for the EOF behind them: the
        // component refuses input until `QueryOutput` has answered it. Past the budget what is
        // still owed never comes: those entries are stale.
        let deadline = std::time::Instant::now() + INPUT_DRAIN_BUDGET;
        let draining = |q: &OutQueue| {
            let g = q.lock();
            !g.pending.is_empty() || (r == sys::AMF_OK && !g.extra)
        };
        while draining(&inner.retrieve.q) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_micros(250));
        }
        let stale = std::mem::take(&mut inner.retrieve.q.lock().pending).len();
        if stale > 0 {
            tracing::warn!(stale, "AMF drain left frames without an AU");
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "amf_tests.rs"]
mod tests;
