//! **Media Foundation** hardware encoder (Windows, D3D11 NV12 input). The vendor-agnostic
//! rung between the native SDKs ([`super::nvenc`]/[`super::amf`]/[`super::qsv`]) and
//! software: every x64 vendor ships an H.264/HEVC MFT, so a missing `amfrt64.dll` no
//! longer ends the session — and on Adreno it is the only hardware encode path there is.
//!
//! Drives an **async hardware MFT** (`MFTEnum2`, filtered to the capture adapter's LUID)
//! through its event generator, pumped non-blocking (`MF_EVENT_FLAG_NO_WAIT`) from
//! `submit`/`poll`. No second thread: the driver's encode thread stays the only one
//! touching the MFT, and a wedged MFT costs a bounded wait, not a join.
//!
//! Input is a same-device NV12 texture ring — `CopySubresourceRegion` then
//! `MFCreateDXGISurfaceBuffer`. 8-bit 4:2:0 only: no P010, no 4:4:4, and no AV1 (every
//! field report on the Qualcomm AV1 MFT is a defect). `mfplat.dll` is an OS component,
//! so this backend carries **no cargo feature** — it is in every Windows build.
//! Evidence: `design/media-foundation-encoder.md`.

use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::retrieve::Ready;
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::core::{implement, Interface, GUID};
use windows::Win32::Foundation::{E_NOTIMPL, LUID, S_OK};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_High, eAVScenarioInfo_DisplayRemoting,
    CODECAPI_AVEncCommonBufferSize, CODECAPI_AVEncCommonMaxBitRate,
    CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncH264CABACEnable, CODECAPI_AVEncMPVDefaultBPictureCount, CODECAPI_AVEncMPVGOPSize,
    CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, CODECAPI_AVScenarioInfo,
    ICodecAPI, IMF2DBuffer, IMFActivate, IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult,
    IMFAttributes, IMFDXGIDeviceManager, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFShutdown, IMFTransform, METransformHaveOutput, METransformNeedInput, MFCreateAttributes,
    MFCreateDXGIDeviceManager, MFCreateDXGISurfaceBuffer, MFCreateMediaType, MFCreateSample,
    MFMediaType_Video, MFNominalRange_16_235, MFSampleExtension_CleanPoint, MFStartup, MFTEnum2,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_HEVC, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoPrimaries_BT709, MFVideoTransFunc_709,
    MFVideoTransferMatrix_BT709, MFSTARTUP_LITE, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_ADAPTER_LUID,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_MESSAGE_COMMAND_DRAIN,
    MFT_MESSAGE_COMMAND_FLUSH, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_MESSAGE_NOTIFY_END_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_SA_D3D11_AWARE, MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

/// Input texture ring depth. The MFT holds a ref on the sample (and so the slot) until it
/// has consumed it; `submit` back-pressures at [`IN_FLIGHT_MAX`], well under this, so a
/// slot is never rewritten while the encoder still reads it.
const RING: usize = 6;

/// Frames the MFT may hold before `submit` blocks. `AVLowLatencyMode` promises one-in
/// one-out, so steady state is 1; this only bounds a stall.
const IN_FLIGHT_MAX: usize = 4;

/// Drain budget for a starved event pump. One frame's encode time over, far under the
/// session watchdog's ~2 s floor.
const BUSY_BUDGET: Duration = Duration::from_millis(200);

/// 100-ns ticks per second — MF's sample-time unit.
const HNS_PER_SEC: i64 = 10_000_000;

/// Output subtype. AV1 is refused: every field report on the Qualcomm AV1 MFT is a defect
/// (dropped frames, blockiness at low CBR), and no x64 vendor needs MF for AV1.
fn subtype(codec: Codec) -> Result<GUID> {
    match codec {
        Codec::H264 => Ok(MFVideoFormat_H264),
        Codec::H265 => Ok(MFVideoFormat_HEVC),
        Codec::Av1 => bail!(
            "Media Foundation AV1 is not enabled — the only hardware AV1 MFT in the field \
             (Qualcomm) is reported broken; use a native SDK backend for AV1"
        ),
        Codec::PyroWave => bail!("PyroWave never opens the Media Foundation backend"),
    }
}

/// Process-wide `MFStartup`. Never shut down: MF is refcounted per process and another
/// session's encoder may still hold it, so the teardown would be the bug, not the leak.
fn mf_startup() -> Result<()> {
    static ONCE: std::sync::OnceLock<std::result::Result<(), String>> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        // SAFETY: plain FFI with the version constant the headers define; `MFSTARTUP_LITE`
        // skips the socket/network stack this process never uses.
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).map_err(|e| format!("MFStartup: {e}")) }
    })
    .clone()
    .map_err(|e| anyhow!(e))
}

/// Join the MTA on this thread. A hardware MFT must be created in a multi-threaded
/// apartment; `RPC_E_CHANGED_MODE` means the thread already picked one and is not ours to
/// change. Never paired with `CoUninitialize` — the encode thread outlives the encoder.
fn com_init_mta() {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    DONE.with(|d| {
        if !d.replace(true) {
            // SAFETY: plain FFI. The returned HRESULT is deliberately dropped: a thread
            // already in an apartment keeps it, and the MFT open below reports the real
            // consequence if that apartment is the wrong one.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        }
    });
}

/// Hardware video-encoder MFTs on `adapter_luid` for `codec`, best first. Empty is the
/// honest answer for "this adapter has no MFT" — only a broken enumeration is an `Err`.
fn enumerate(codec: Codec, adapter_luid: Option<LUID>) -> Result<Vec<IMFActivate>> {
    let out_subtype = subtype(codec)?;
    mf_startup()?;
    com_init_mta();
    // SAFETY: every handle below is owned by this function. `MFTEnum2` writes a
    // CoTaskMemAlloc'd array of AddRef'd activates; each element is read out (taking that
    // reference) exactly once and the array is freed before return, on both paths.
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).context("MFCreateAttributes")?;
        let attrs = attrs.ok_or_else(|| anyhow!("MFCreateAttributes returned no attributes"))?;
        if let Some(luid) = adapter_luid {
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
            bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
            attrs
                .SetBlob(&MFT_ENUM_ADAPTER_LUID, &bytes)
                .context("MFT_ENUM_ADAPTER_LUID")?;
        }
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: MFVideoFormat_NV12,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: out_subtype,
        };
        let mut array: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFTEnum2(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &attrs,
            &mut array,
            &mut count,
        )
        .context("MFTEnum2")?;
        let mut found = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            if let Some(a) = ptr::read(array.add(i)) {
                found.push(a);
            }
        }
        CoTaskMemFree(Some(array as *const std::ffi::c_void));
        Ok(found)
    }
}

/// `MFT_FRIENDLY_NAME_Attribute`, for the one line that says which vendor's MFT opened.
fn friendly_name(activate: &IMFActivate) -> String {
    // SAFETY: standard MF attribute read on a live activate; the buffer is sized from the
    // length the same object just reported and only the written prefix is decoded.
    unsafe {
        let Ok(len) = activate.GetStringLength(&MFT_FRIENDLY_NAME_Attribute) else {
            return "unknown MFT".into();
        };
        let mut buf = vec![0u16; len as usize + 1];
        match activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None) {
            Ok(()) => String::from_utf16_lossy(&buf[..len as usize]),
            Err(_) => "unknown MFT".into(),
        }
    }
}

/// Everything one bring-up needs from the encoder's negotiated parameters.
#[derive(Clone, Copy)]
struct EncodeConfig {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
}

impl EncodeConfig {
    /// `MF_MT_AVG_BITRATE` and the codec-API rate properties are `u32` bits/s.
    fn bitrate_u32(&self) -> u32 {
        self.bitrate_bps.min(u64::from(u32::MAX)) as u32
    }
}

/// `ICodecAPI::SetValue`, advisory. An MFT that declines a property still encodes — every
/// vendor's optional set differs — so a failure is logged, never fatal. `value` must carry the
/// VARIANT type the property documents, and an integer literal is `i32` (`VT_I4`) where nearly
/// every codec property wants `VT_UI4` — write `1u32`. NVIDIA's MFT refuses a mistyped
/// B-picture count outright and ignores a mistyped force-keyframe silently.
fn set_advisory<T>(api: &ICodecAPI, key: &GUID, name: &str, value: T) -> bool
where
    T: Into<VARIANT> + Copy + std::fmt::Debug,
{
    let v = value.into();
    // SAFETY: `api` is live, `key` is a static GUID, and `v` is a by-value scalar VARIANT
    // that outlives the synchronous call (no allocation to release).
    let r = unsafe { api.SetValue(key, &v) };
    if let Err(e) = &r {
        tracing::debug!(property = name, value = ?value, error = %e, "MF encoder declined a codec-API property");
    }
    r.is_ok()
}

/// `ICodecAPI::IsSupported`, strictly. `S_FALSE` names a property the MFT knows and will not
/// honour, and windows-rs folds that into `Ok`; Microsoft's contract is to test for `S_OK`.
fn is_supported(api: &ICodecAPI, key: &GUID) -> bool {
    // SAFETY: `api` is live and `key` is a static GUID. The raw vtable call is the only way
    // to see `S_FALSE` — the wrapper's `Result` has already discarded it.
    let hr = unsafe { (Interface::vtable(api).IsSupported)(Interface::as_raw(api), key) };
    hr == S_OK
}

/// Both rate-control knobs in one place: the open path and `reconfigure_bitrate` must not
/// drift. `false` = the MFT refused the mean rate, so the caller rebuilds instead.
fn apply_bitrate(api: &ICodecAPI, cfg: &EncodeConfig) -> bool {
    let bps = cfg.bitrate_u32();
    let mean = set_advisory(api, &CODECAPI_AVEncCommonMeanBitRate, "MeanBitRate", bps);
    set_advisory(api, &CODECAPI_AVEncCommonMaxBitRate, "MaxBitRate", bps);
    // One frame of VBV: the tree's low-latency contract, same as the native backends.
    let vbv = (cfg.bitrate_bps / u64::from(cfg.fps.max(1))).min(u64::from(u32::MAX)) as u32;
    set_advisory(api, &CODECAPI_AVEncCommonBufferSize, "BufferSize", vbv);
    mean
}

/// The static property block, set before the media types (MS documents rate-control mode
/// and low latency as pre-type). Colour attributes are deliberately absent: no MFT writes
/// VUI from them and setting them crashes AMD's (Chromium `4c2d1fdf61`).
fn apply_static_properties(api: &ICodecAPI, cfg: &EncodeConfig, force_idr_ok: bool) {
    set_advisory(
        api,
        &CODECAPI_AVEncCommonRateControlMode,
        "RateControlMode",
        eAVEncCommonRateControlMode_CBR.0 as u32,
    );
    // VARIANT_BOOL on encoders — only the H.264 *decoder* takes VT_UI4 for low latency.
    // An MFT that checks the type declines a mistyped one and stays deeply pipelined.
    set_advisory(api, &CODECAPI_AVLowLatencyMode, "LowLatencyMode", true);
    // B-frames break FIFO pairing and add a frame of latency. Chromium forces 0 on
    // Qualcomm, where the MFT default was 1 and buggy.
    set_advisory(
        api,
        &CODECAPI_AVEncMPVDefaultBPictureCount,
        "BPictureCount",
        0u32,
    );
    set_advisory(
        api,
        &CODECAPI_AVScenarioInfo,
        "ScenarioInfo",
        eAVScenarioInfo_DisplayRemoting.0 as u32,
    );
    if cfg.codec == Codec::H264 {
        set_advisory(api, &CODECAPI_AVEncH264CABACEnable, "CABAC", true);
    }
    // Infinite GOP is the tree's contract: IDR on demand only. An MFT that cannot force an
    // IDR gets a periodic one instead, or a lost frame freezes the client forever.
    if force_idr_ok {
        if !set_advisory(api, &CODECAPI_AVEncMPVGOPSize, "GOPSize", u32::MAX) {
            set_advisory(
                api,
                &CODECAPI_AVEncMPVGOPSize,
                "GOPSize",
                cfg.fps.saturating_mul(60).max(60),
            );
        }
    } else {
        set_advisory(
            api,
            &CODECAPI_AVEncMPVGOPSize,
            "GOPSize",
            cfg.fps.saturating_mul(2).max(30),
        );
    }
}

/// `MF_MT_FRAME_SIZE` / `MF_MT_FRAME_RATE` / `MF_MT_PIXEL_ASPECT_RATIO` pack two `u32`s
/// into one `u64`, high word first. `MFSetAttributeSize` is a C++ inline, not an export.
const fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

/// The capture video processor's NV12 is BT.709 limited range. Tagged on both types so the
/// MFT writes it into the SPS colour description.
fn tag_bt709_limited(t: &IMFMediaType) -> Result<()> {
    // SAFETY: plain attribute writes with static GUID keys on a live media type.
    unsafe {
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

/// Output (bitstream) media type. Set before the input type — the MFT derives what input
/// it will accept from it.
fn output_type(cfg: &EncodeConfig) -> Result<IMFMediaType> {
    // SAFETY: `MFCreateMediaType` hands back an owned empty type; every setter below is a
    // plain attribute write on it with static GUID keys.
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType (output)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype(cfg.codec)?)?;
        t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate_u32())?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps.max(1), 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        if cfg.codec == Codec::H264 {
            t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
        }
        tag_bt709_limited(&t)?;
        Ok(t)
    }
}

/// Input (NV12) media type. 8-bit 4:2:0 is the whole input surface of this backend.
fn input_type(cfg: &EncodeConfig) -> Result<IMFMediaType> {
    // SAFETY: as `output_type` — owned empty media type, plain attribute writes.
    unsafe {
        let t = MFCreateMediaType().context("MFCreateMediaType (input)")?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps.max(1), 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        tag_bt709_limited(&t)?;
        Ok(t)
    }
}

/// H.264 SPS/PPS or HEVC VPS/SPS/PPS.
fn is_parameter_set(codec: Codec, nal: u8) -> bool {
    match codec {
        Codec::H264 => matches!(nal, 7 | 8),
        _ => matches!(nal, 32..=34),
    }
}

/// The AU's VPS/SPS/PPS run, or `None` when it carries none. An access-unit delimiter or
/// SEI ahead of the run is skipped rather than treated as its absence — that is the shape
/// Qualcomm's MFT emits, and it publishes the sequence header late and HEVC in-band only,
/// so an IDR without one is undecodable unless we prepend the cached run ourselves.
fn parameter_set_prefix(codec: Codec, au: &[u8]) -> Option<&[u8]> {
    let mut run: Option<usize> = None;
    let mut i = 2;
    while i + 1 < au.len() {
        if au[i - 2..i + 1] != [0, 0, 1] {
            i += 1;
            continue;
        }
        // `i` indexes the start code's last byte; the 4-byte form owns the zero before it.
        let header = i + 1;
        let code_start = if i >= 3 && au[i - 3] == 0 {
            i - 3
        } else {
            i - 2
        };
        let nal = match codec {
            Codec::H264 => au[header] & 0x1f,
            _ => (au[header] >> 1) & 0x3f,
        };
        match (is_parameter_set(codec, nal), run) {
            (true, None) => run = Some(code_start),
            (false, Some(start)) => return Some(&au[start..code_start]),
            _ => {}
        }
        i = header + 1;
    }
    run.map(|start| &au[start..])
}

/// One submitted frame, awaiting its AU. The MFT emits in submit order (B-frames are off),
/// so this pairs by position — MF puts no wire index on the output sample.
struct PendingMeta {
    pts_ns: u64,
}

/// Live MFT session. Field order is drop order: the transform releases before the device
/// manager and the ring textures it was reading.
/// What the MFT's event callback and the encode thread share.
///
/// An async MFT announces input credit and finished output through its event generator, and
/// `IMFMediaEventGenerator` refuses to mix `GetEvent` with `BeginGetEvent` — so once the callback
/// is armed it is the only reader, and everything it touches lives behind this lock.
#[derive(Default)]
struct Out {
    /// `METransformNeedInput` events not yet spent on a `ProcessInput`.
    need_input: u32,
    pending: VecDeque<PendingMeta>,
    ready: VecDeque<EncodedFrame>,
    /// VPS/SPS/PPS from the first IDR that carried them, prepended to any later IDR that
    /// does not. Empty until such an IDR is seen.
    param_sets: Vec<u8>,
    headers_warned: bool,
    /// First failure the callback hit; `poll` surfaces it so the caller resets.
    err: Option<String>,
}

/// What the callback keeps alive on its own. It outlives [`Inner`] on purpose: an `Invoke` can
/// still be running on an MF worker thread while the encode thread tears the session down, and
/// it must find live objects rather than freed ones. A shut-down MFT simply fails its calls,
/// which is what stops the re-arm.
struct Shared {
    mft: IMFTransform,
    events: IMFMediaEventGenerator,
    codec: Codec,
    out: Mutex<Out>,
    have: Ready,
}

// SAFETY: the MFT and its event generator are the free-threaded objects the async MFT model
// requires — that model is precisely "ProcessInput on the caller's thread, ProcessOutput from the
// event callback". Everything mutable is behind `out`.
unsafe impl Send for Shared {}
// SAFETY: as above — every field is either immutable or behind the mutex.
unsafe impl Sync for Shared {}

/// The event sink an async MFT calls back on. Re-arms itself until the generator says the MFT is
/// gone, which is what ends the chain at teardown.
#[implement(IMFAsyncCallback)]
struct EventSink(Arc<Shared>);

impl IMFAsyncCallback_Impl for EventSink_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> windows::core::Result<()> {
        // "Use the defaults" — the documented answer for a callback with no queue preference.
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: windows::core::Ref<IMFAsyncResult>) -> windows::core::Result<()> {
        let shared = &self.0;
        let Some(result) = result.as_ref() else {
            return Ok(());
        };
        // SAFETY: `result` is the generator's own completion for the `BeginGetEvent` below, and
        // `EndGetEvent` is the only legal way to take its event.
        let event = match unsafe { shared.events.EndGetEvent(result) } {
            Ok(e) => e,
            // The MFT shut down under us: stop, and do not re-arm.
            Err(_) => return Ok(()),
        };
        // SAFETY: plain accessor on the owned event.
        let kind = unsafe { event.GetType() }.unwrap_or(0) as i32;
        if kind == METransformNeedInput.0 {
            lock(&shared.out).need_input += 1;
        } else if kind == METransformHaveOutput.0 {
            match process_output(shared) {
                Ok(au) => {
                    let mut g = lock(&shared.out);
                    g.ready.push_back(au);
                    // Under the lock, so it cannot race the clear `poll` does when it empties.
                    shared.have.set();
                }
                Err(e) => {
                    let mut g = lock(&shared.out);
                    g.err.get_or_insert_with(|| format!("{e:#}"));
                    shared.have.set();
                }
            }
        }
        // SAFETY: re-arming with the same callback is the documented loop; a shut-down generator
        // refuses, which is how the chain ends.
        unsafe {
            let _ = shared
                .events
                .BeginGetEvent(&IMFAsyncCallback::from(EventSink(shared.clone())), None);
        }
        Ok(())
    }
}

/// The shared-state lock, poison-tolerant: a callback that panicked leaves what it already
/// produced readable, and its error field is what tells `poll` to reset.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Inner {
    /// Everything the callback touches. Dropping `Inner` shuts the MFT down, after which a late
    /// `Invoke` finds live-but-shut-down objects here rather than freed memory.
    shared: Arc<Shared>,
    /// Our own reference to the armed sink. The generator holds one too; this keeps the object
    /// alive across the gap between two `BeginGetEvent`s.
    _sink: IMFAsyncCallback,
    mft: IMFTransform,
    /// The activation object that created `mft`: it owns the shutdown, so it outlives it.
    activate: IMFActivate,
    /// `None` when the MFT exposes no `ICodecAPI` — then bitrate and GOP are whatever the
    /// output media type carried, and `reconfigure_bitrate` declines.
    codec_api: Option<ICodecAPI>,
    /// Held for the MFT's D3D binding; the transform AddRefs it, this keeps our own claim.
    _manager: Option<IMFDXGIDeviceManager>,
    _device: ID3D11Device,
    dctx: ID3D11DeviceContext,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    frames_submitted: u64,
    first_au_logged: bool,
}

impl Drop for Inner {
    /// An async MFT must be shut down before its last release, and an activation object
    /// shuts down what it created. Releasing without either leaks the vendor MFT's worker
    /// threads and GPU allocations for the life of the driver process.
    fn drop(&mut self) {
        // SAFETY: teardown runs on the encode thread that drove the MFT. Each call is
        // synchronous and takes no arguments, and this is the documented last use of both.
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            if let Ok(shutdown) = self.mft.cast::<IMFShutdown>() {
                let _ = shutdown.Shutdown();
            }
            let _ = self.activate.ShutdownObject();
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
                "Media Foundation produced its first AU on this session"
            );
        }
    }

    /// The oldest finished AU, or the callback's failure. Clears the signal as the queue empties
    /// — under the same lock the callback sets it under, so the two cannot cross.
    fn pop_ready(&mut self) -> Result<Option<EncodedFrame>> {
        let mut g = lock(&self.shared.out);
        if let Some(e) = g.err.take() {
            bail!("{e}");
        }
        let au = g.ready.pop_front();
        if g.ready.is_empty() {
            self.shared.have.clear();
        }
        Ok(au)
    }

    /// [`Self::pop_ready`], waiting up to `wait_ms` for the callback to produce one.
    fn take_ready(&mut self, wait_ms: u32) -> Result<Option<EncodedFrame>> {
        if let Some(au) = self.pop_ready()? {
            return Ok(Some(au));
        }
        if !self.shared.have.wait(wait_ms) {
            return Ok(None);
        }
        self.pop_ready()
    }
}

/// Drain one `METransformHaveOutput`: `ProcessOutput`, copy the bitstream out, pair it with
/// the oldest submitted frame.
fn process_output(shared: &Shared) -> Result<EncodedFrame> {
    // The MFT has handed this frame over, so its entry is spent whatever the payload turns
    // out to be — a failed ProcessOutput included. Popping only on the success path would pair
    // every later AU with the wrong frame's timestamp for the rest of the session.
    let meta = lock(&shared.out).pending.pop_front();
    // SAFETY: the MFT is live on this thread and owes exactly one output per HaveOutput
    // event. `MFT_OUTPUT_DATA_BUFFER`'s `ManuallyDrop` members are reclaimed with
    // `ManuallyDrop::take` on every path, so the sample and the event collection the MFT
    // allocated are released exactly once.
    let sample = unsafe {
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(None),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let call = shared.mft.ProcessOutput(0, &mut out, &mut status);
        let sample: Option<IMFSample> = ManuallyDrop::take(&mut out[0].pSample);
        let _events = ManuallyDrop::take(&mut out[0].pEvents);
        call.context("IMFTransform::ProcessOutput")?;
        sample.ok_or_else(|| anyhow!("MFT signalled HaveOutput with no sample"))?
    };
    // SAFETY: `sample` is owned here. The `Lock`ed pointer is read only for the length the
    // same call reported, and the buffer is unlocked before it drops.
    let (data, keyframe) = unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .context("IMFSample::ConvertToContiguousBuffer")?;
        let mut p: *mut u8 = ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut p, None, Some(&mut len))
            .context("IMFMediaBuffer::Lock")?;
        let data = if p.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(p, len as usize).to_vec()
        };
        let _ = buffer.Unlock();
        let keyframe = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        (data, keyframe)
    };
    if data.is_empty() {
        bail!("Media Foundation returned an empty access unit");
    }
    let data = if keyframe {
        repeat_parameter_sets(shared, data)
    } else {
        data
    };
    Ok(EncodedFrame {
        data,
        pts_ns: meta.map_or(0, |m| m.pts_ns),
        keyframe,
        recovery_anchor: false,
        recovery_point: false,
        recovery_close: false,
        chunk_aligned: false,
    })
}

/// Cache the first IDR's parameter-set run and re-attach it to any later IDR that arrives
/// without one. A no-op on every MFT that already repeats them (all three x64 vendors).
fn repeat_parameter_sets(shared: &Shared, au: Vec<u8>) -> Vec<u8> {
    let mut g = lock(&shared.out);
    if let Some(prefix) = parameter_set_prefix(shared.codec, &au) {
        if g.param_sets != prefix {
            g.param_sets = prefix.to_vec();
        }
        return au;
    }
    if g.param_sets.is_empty() {
        return au;
    }
    if !g.headers_warned {
        g.headers_warned = true;
        tracing::warn!(
            "this MFT does not repeat VPS/SPS/PPS on every IDR — prepending the cached \
             sequence header so a client that joins late can decode"
        );
    }
    let mut out = Vec::with_capacity(g.param_sets.len() + au.len());
    out.extend_from_slice(&g.param_sets);
    out.extend_from_slice(&au);
    out
}

/// Wait until the callback has made `ready` true, or the budget expires. The one wait in this
/// backend — both back-pressure and input credit go through it, so neither can grow its own
/// timeout. The callback is what makes progress now; this only watches for it.
fn wait_until(inner: &Inner, what: &str, ready: impl Fn(&Out) -> bool) -> Result<()> {
    let deadline = Instant::now() + BUSY_BUDGET;
    loop {
        {
            let mut g = lock(&inner.shared.out);
            if let Some(e) = g.err.take() {
                bail!("{e}");
            }
            if ready(&g) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "Media Foundation {what} stalled for {} ms with {} frame(s) in flight — \
                     wedged (escalating to reset)",
                    BUSY_BUDGET.as_millis(),
                    g.pending.len()
                );
            }
        }
        std::thread::sleep(Duration::from_micros(250));
    }
}

pub struct MfEncoder {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    /// Adapter the host picked. The session still binds to the first frame's device.
    adapter_luid: Option<LUID>,
    /// Lazy from the first frame's device; rebuilt on a capturer-device change.
    inner: Option<Inner>,
    bound_device: isize,
    force_kf: bool,
    /// The MFT answered `IsSupported(AVEncVideoForceKeyFrame)`. `false` buys a periodic
    /// GOP instead, because a stream with neither cannot recover from loss at all.
    force_idr_ok: bool,
    /// Resets with no AU since. At 2, drop `inner` instead of re-messaging a dead MFT.
    resets_without_output: u32,
}

// SAFETY: COM interfaces and D3D11 handles are not auto-`Send`. The session moves the
// encoder onto one encode thread and drives it there; the immediate context, the MFT, and
// its event generator are never touched from another thread.
unsafe impl Send for MfEncoder {}

impl MfEncoder {
    /// Open the MF backend. Fails when the adapter has no hardware MFT for `codec`, or
    /// when capture is not 8-bit NV12.
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
        adapter_luid: Option<LUID>,
    ) -> Result<Self> {
        subtype(codec)?;
        // Depth follows delivered pixels, not negotiated depth ([`crate::ten_bit_input`]),
        // so a 10-bit-negotiated session over 8-bit capture encodes 8-bit rather than bailing.
        if crate::ten_bit_input(format, bit_depth) {
            bail!(
                "Media Foundation encode is 8-bit 4:2:0 only — no vendor's MFT accepts P010 \
                 (capturer delivered {format:?})"
            );
        }
        if format != PixelFormat::Nv12 {
            bail!(
                "Media Foundation needs the video-processor NV12 capture path; capturer \
                 delivered {format:?} (no readback path by design — zero-copy invariant)"
            );
        }
        if chroma.is_444() {
            tracing::warn!("no Media Foundation MFT encodes 4:4:4 — encoding 4:2:0");
        }
        if enumerate(codec, adapter_luid)?.is_empty() {
            bail!("no hardware Media Foundation encoder for {codec:?} on the selected adapter");
        }
        Ok(MfEncoder {
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            adapter_luid,
            inner: None,
            bound_device: 0,
            force_kf: false,
            force_idr_ok: false,
            resets_without_output: 0,
        })
    }

    fn encode_config(&self) -> EncodeConfig {
        EncodeConfig {
            codec: self.codec,
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_bps: self.bitrate_bps,
        }
    }

    /// Bring the MFT up on the capturer's device: unlock async, bind D3D, negotiate types,
    /// apply the property block, start streaming.
    fn ensure_inner(&mut self, device: &ID3D11Device) -> Result<()> {
        let dev_raw = device.as_raw() as isize;
        if self.inner.is_some() && self.bound_device == dev_raw {
            return Ok(());
        }
        self.inner = None;
        self.bound_device = dev_raw;
        let cfg = self.encode_config();
        let activate = enumerate(self.codec, self.adapter_luid)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                anyhow!(
                    "no hardware Media Foundation encoder for {:?} on the selected adapter",
                    self.codec
                )
            })?;
        let name = friendly_name(&activate);
        let brought = || -> Result<_> {
            // SAFETY: the whole bring-up runs on this encode thread. Every interface is an
            // owned windows-rs wrapper released on drop; `device.as_raw()` and the device
            // manager's raw pointer are borrowed for the duration of the synchronous calls
            // that consume them (the MFT AddRefs the manager it is handed), and the ring
            // textures are created on and used from this one device.
            unsafe {
                let mft: IMFTransform = activate
                    .ActivateObject()
                    .context("IMFActivate::ActivateObject(IMFTransform)")?;
                let attrs = mft.GetAttributes().context("IMFTransform::GetAttributes")?;
                if attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 1 {
                    bail!("{name} is not an async MFT — this backend drives the event model only");
                }
                attrs
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .context("MF_TRANSFORM_ASYNC_UNLOCK")?;
                let dctx = device
                    .GetImmediateContext()
                    .context("ID3D11Device immediate context")?;
                // MFT worker threads touch the device: protection must be on before the
                // manager reset, or the reset takes a device nobody else may use (QSV's
                // on-glass lesson, `qsv.rs`).
                if let Ok(mt) = dctx.cast::<ID3D11Multithread>() {
                    let _ = mt.SetMultithreadProtected(true);
                }
                let manager = if attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0 {
                    let mut token = 0u32;
                    let mut mgr: Option<IMFDXGIDeviceManager> = None;
                    MFCreateDXGIDeviceManager(&mut token, &mut mgr)
                        .context("MFCreateDXGIDeviceManager")?;
                    let mgr =
                        mgr.ok_or_else(|| anyhow!("MFCreateDXGIDeviceManager returned none"))?;
                    mgr.ResetDevice(device, token)
                        .context("IMFDXGIDeviceManager::ResetDevice")?;
                    mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
                        .context("MFT_MESSAGE_SET_D3D_MANAGER")?;
                    Some(mgr)
                } else {
                    // Without D3D awareness the MFT would want system-memory input, which the
                    // zero-copy contract has no path to produce.
                    bail!("{name} is not MF_SA_D3D11_AWARE — no D3D11 texture input path");
                };
                let codec_api: Option<ICodecAPI> = mft.cast().ok();
                let force_idr_ok = codec_api
                    .as_ref()
                    .is_some_and(|api| is_supported(api, &CODECAPI_AVEncVideoForceKeyFrame));
                if let Some(api) = codec_api.as_ref() {
                    apply_static_properties(api, &cfg, force_idr_ok);
                }
                mft.SetOutputType(0, &output_type(&cfg)?, 0)
                    .context("IMFTransform::SetOutputType")?;
                mft.SetInputType(0, &input_type(&cfg)?, 0)
                    .context("IMFTransform::SetInputType")?;
                // `process_output` always asks the MFT for its own sample. One that wants the
                // caller to allocate would fail every ProcessOutput instead, so refuse here and
                // let the driver's preference list move on to the next backend.
                let out_info = mft
                    .GetOutputStreamInfo(0)
                    .context("IMFTransform::GetOutputStreamInfo")?;
                let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32;
                if out_info.dwFlags & provides == 0 {
                    bail!("{name} wants the caller to allocate output samples — unsupported here");
                }
                if let Some(api) = codec_api.as_ref() {
                    // Rate is dynamic: re-apply after the types so the MFT's own derivation
                    // from MF_MT_AVG_BITRATE cannot win.
                    apply_bitrate(api, &cfg);
                }
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
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
                let mut ring = Vec::with_capacity(RING);
                for _ in 0..RING {
                    let mut t: Option<ID3D11Texture2D> = None;
                    device
                        .CreateTexture2D(&desc, None, Some(&mut t))
                        .context("CreateTexture2D (MF input ring)")?;
                    ring.push(t.context("MF input ring texture")?);
                }
                let events: IMFMediaEventGenerator = mft
                    .cast()
                    .context("MFT exposes no IMFMediaEventGenerator")?;
                mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                    .context("MFT_MESSAGE_NOTIFY_BEGIN_STREAMING")?;
                mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                    .context("MFT_MESSAGE_NOTIFY_START_OF_STREAM")?;
                Ok((mft, events, codec_api, manager, dctx, ring, force_idr_ok))
            }
        };
        let (mft, events, codec_api, manager, dctx, ring, force_idr_ok) = match brought() {
            Ok(t) => t,
            Err(e) => {
                // An activated MFT released without its shutdown leaks the vendor's worker
                // threads and GPU allocations for the process's life (see `Inner::drop`).
                // SAFETY: the activation object is live; this is its last use on this path.
                unsafe {
                    let _ = activate.ShutdownObject();
                }
                return Err(e);
            }
        };
        self.force_idr_ok = force_idr_ok;
        if !force_idr_ok {
            tracing::warn!(
                mft = %name,
                "this MFT declines on-demand IDR (AVEncVideoForceKeyFrame) — falling back to a \
                 periodic GOP, so loss recovery waits for the next scheduled keyframe"
            );
        }
        tracing::info!(
            mft = %name,
            codec = ?self.codec,
            width = self.width,
            height = self.height,
            fps = self.fps,
            force_idr = force_idr_ok,
            device = %format_args!("{:#x}", dev_raw as usize),
            "Media Foundation encode active (async MFT, zero-copy D3D11 NV12)"
        );
        // Everything from here to the `Inner` below can still fail, and until that value exists
        // `Inner::drop` — the only thing that shuts the MFT down — is not armed. Bind the
        // prelude so a failure gets the same `ShutdownObject` the activation arm above does;
        // releasing an activated MFT without it leaks the vendor's worker threads and GPU
        // allocations for the life of the process.
        let armed = (|| -> Result<(Arc<Shared>, IMFAsyncCallback)> {
            let shared = Arc::new(Shared {
                mft: mft.clone(),
                events,
                codec: self.codec,
                out: Mutex::new(Out::default()),
                have: Ready::new().context("Media Foundation: no completion event")?,
            });
            // Arm the event sink. From here the callback is the only reader of the generator, so
            // every `NeedInput` and `HaveOutput` lands in `shared.out` rather than in a pump.
            let sink = IMFAsyncCallback::from(EventSink(shared.clone()));
            // SAFETY: `shared.events` is the live MFT's generator and `sink` is a callback that
            // outlives the request (both this `Inner` and the generator hold a reference).
            unsafe { shared.events.BeginGetEvent(&sink, None) }
                .context("IMFMediaEventGenerator::BeginGetEvent")?;
            Ok((shared, sink))
        })();
        let (shared, sink) = match armed {
            Ok(pair) => pair,
            Err(e) => {
                // SAFETY: the activation object is live; this is its last use on this path.
                unsafe {
                    let _ = activate.ShutdownObject();
                }
                return Err(e);
            }
        };
        self.inner = Some(Inner {
            shared,
            _sink: sink,
            mft,
            activate,
            codec_api,
            _manager: manager,
            _device: device.clone(),
            dctx,
            ring,
            next: 0,
            frames_submitted: 0,
            first_au_logged: false,
        });
        Ok(())
    }
}

impl Encoder for MfEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
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
                bail!("Media Foundation is D3D11-only; got a CPU frame (video processor lost?)")
            }
        };
        anyhow::ensure!(
            captured.format == PixelFormat::Nv12,
            "captured format {:?} != NV12 (capturer video-processor fallback mid-session — this \
             backend has no readback path)",
            captured.format
        );
        self.ensure_inner(&frame.device)?;
        let opening = self.inner.as_ref().is_none_or(|i| i.frames_submitted == 0);
        let forced = std::mem::take(&mut self.force_kf) || opening;
        let fps = self.fps.max(1);
        let inner = self.inner.as_mut().expect("ensure_inner succeeded");
        // Back-pressure before input credit: the AU the callback takes here frees the ring slot.
        wait_until(inner, "output", |o| o.pending.len() < IN_FLIGHT_MAX)
            .inspect_err(|_| self.force_kf = true)?;
        wait_until(inner, "input credit", |o| o.need_input > 0)
            .inspect_err(|_| self.force_kf = true)?;
        let slot = inner.next;
        inner.next = (inner.next + 1) % RING;
        // SAFETY: single encode thread against the live MFT. The ring texture is owned
        // here and only rewritten after back-pressure released it; the DXGI surface buffer
        // and sample are owned wrappers the MFT AddRefs for as long as it reads them, and
        // `ProcessInput` is only reached with a `METransformNeedInput` credit in hand.
        let submitted = unsafe {
            let src: ID3D11Resource = frame.texture.cast().context("texture -> resource")?;
            let dst: ID3D11Resource = inner.ring[slot].cast().context("ring -> resource")?;
            inner
                .dctx
                .CopySubresourceRegion(&dst, 0, 0, 0, 0, &src, 0, None);
            let buffer =
                MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, &inner.ring[slot], 0, false)
                    .context("MFCreateDXGISurfaceBuffer")?;
            // The DXGI buffer opens with length 0; MFTs that read it as a byte count
            // otherwise see an empty frame.
            if let Ok(len) = buffer
                .cast::<IMF2DBuffer>()
                .and_then(|b| b.GetContiguousLength())
            {
                let _ = buffer.SetCurrentLength(len);
            }
            let sample = MFCreateSample().context("MFCreateSample")?;
            sample.AddBuffer(&buffer).context("IMFSample::AddBuffer")?;
            sample.SetSampleTime((captured.pts_ns / 100) as i64)?;
            sample.SetSampleDuration(HNS_PER_SEC / i64::from(fps))?;
            if forced {
                if let Some(api) = inner.codec_api.as_ref() {
                    set_advisory(
                        api,
                        &CODECAPI_AVEncVideoForceKeyFrame,
                        "ForceKeyFrame",
                        1u32,
                    );
                }
            }
            // The entry goes in BEFORE the MFT takes the frame: the event callback runs on the
            // MFT's own thread and can pop for this frame the moment ProcessInput returns, so
            // pushing after it races an empty queue and skews every later AU's pts by one frame.
            // Both under one lock: the credit this submit spends, and the entry the callback
            // pairs its next output with.
            {
                let mut g = lock(&inner.shared.out);
                g.need_input = g.need_input.saturating_sub(1);
                g.pending.push_back(PendingMeta {
                    pts_ns: captured.pts_ns,
                });
            }
            inner.mft.ProcessInput(0, &sample, 0)
        };
        if let Err(e) = submitted {
            // The MFT never took it, so take the entry back off.
            lock(&inner.shared.out).pending.pop_back();
            self.force_kf = true;
            bail!("IMFTransform::ProcessInput: {e}");
        }
        inner.frames_submitted += 1;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            // The capturer composites; this backend never reads `frame.cursor`.
            blends_cursor: false,
            // No open-source client uses the LTR ICodecAPI set on any vendor's MFT, so the
            // slot planner has nothing to drive. Loss recovery is IDR.
            supports_rfi: false,
            chroma_444: false,
            intra_refresh: false,
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
            downscales_input: false,
            crops_input: false,
        }
    }

    /// Wait up to `min(3/4 frame interval, 12 ms)` for the oldest AU. Expiry is `Ok(None)`.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let budget_ms = (750 / self.fps.max(1)).clamp(1, 12);
        let au = {
            let Some(inner) = self.inner.as_mut() else {
                return Ok(None);
            };
            // The same bound as before, now spent on the callback's event rather than on a
            // sample loop, so nothing else on this thread waits behind it.
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

    /// The event sink's signal, once an MFT exists. Before the lazy open there is nothing to
    /// wait on, which a caller reads as "no completion signal" and polls instead.
    fn ready_event(&self) -> Option<isize> {
        self.inner.as_ref().map(|i| i.shared.have.raw())
    }

    /// Stall recovery: flush and restart streaming in place. A second reset with no AU
    /// since drops the MFT so the next submit activates a fresh one.
    fn reset(&mut self) -> bool {
        self.force_kf = true;
        self.resets_without_output = self.resets_without_output.saturating_add(1);
        let Some(inner) = self.inner.as_mut() else {
            return true;
        };
        if self.resets_without_output >= 2 {
            tracing::warn!(
                resets = self.resets_without_output,
                "Media Foundation stall persisted across an in-place restart — dropping the MFT, \
                 reopening lazily (next submit)"
            );
            self.inner = None;
            self.bound_device = 0;
            return true;
        }
        // SAFETY: the MFT is live on this thread; flush + stream restart is the documented
        // recovery order, and each message is a synchronous no-argument call.
        let stopped = unsafe {
            inner
                .mft
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)
                .and_then(|()| {
                    inner
                        .mft
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0)
                })
        };
        // The flush voided every in-flight frame and the queued events that named them. The
        // callback stays armed across it — the generator is the MFT's, not the stream's — so
        // this clear belongs BETWEEN the stop and the restart: re-arming issues fresh
        // METransformNeedInput on the MFT's thread, and clearing afterwards discarded the
        // input credit the restart had just granted.
        {
            let mut g = lock(&inner.shared.out);
            g.pending.clear();
            g.ready.clear();
            g.need_input = 0;
            g.err = None;
        }
        inner.shared.have.clear();
        // SAFETY: as above — synchronous no-argument messages on a live MFT.
        let restarted = stopped.and_then(|()| unsafe {
            inner
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|()| {
                    inner
                        .mft
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                })
        });
        inner.frames_submitted = 0;
        inner.first_au_logged = false;
        if let Err(e) = restarted {
            tracing::warn!(error = %e, "Media Foundation in-place restart failed — dropping the MFT");
            self.inner = None;
            self.bound_device = 0;
        } else {
            tracing::info!("Media Foundation encoder restarted in place (flush + begin streaming)");
        }
        true
    }

    /// No-IDR ABR: re-set the mean/max rate on the live `ICodecAPI`. Every vendor
    /// documents these as dynamic; `false` sends the caller to a full rebuild.
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let old = self.bitrate_bps;
        self.bitrate_bps = bps;
        let cfg = self.encode_config();
        let Some(inner) = self.inner.as_ref() else {
            return true; // Not open yet: the next bring-up carries the new rate.
        };
        let Some(api) = inner.codec_api.as_ref() else {
            self.bitrate_bps = old;
            return false;
        };
        if apply_bitrate(api, &cfg) {
            return true;
        }
        tracing::warn!(
            mbps = bps / 1_000_000,
            "the MFT declined the in-place bitrate retarget — falling back to a rebuild"
        );
        self.bitrate_bps = old;
        false
    }

    fn flush(&mut self) -> Result<()> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(());
        };
        // SAFETY: the MFT is live on this thread; end-of-stream then drain is the
        // documented order, and both are synchronous no-argument messages.
        unsafe {
            inner
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .and_then(|()| inner.mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0))
                .context("Media Foundation drain")?;
        }
        // Owed AUs arrive as HaveOutput events; surface them through `poll`.
        let _ = wait_until(inner, "drain", |o| o.pending.is_empty());
        // End-of-stream zeroed the MFT's input credit and it issues no more until a fresh
        // start-of-stream, so without this a later submit stalls out its whole budget.
        // Drop the stale count FIRST: the restart issues fresh METransformNeedInput on the
        // MFT's thread, and zeroing afterwards threw away the credit it had just granted.
        lock(&inner.shared.out).need_input = 0;
        // SAFETY: the MFT is live on this thread; a synchronous no-argument message.
        unsafe {
            let _ = inner
                .mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Ok(())
    }
}

/// Can this adapter encode `codec` in hardware through Media Foundation? Enumeration is
/// the probe: an MFT that lists the pair opens it.
pub fn probe_can_encode(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    enumerate(codec, adapter_luid).is_ok_and(|m| !m.is_empty())
}

/// No MFT encodes 10-bit for us: the whole backend is 8-bit 4:2:0 by design.
pub fn probe_can_encode_10bit(_codec: Codec, _adapter_luid: Option<LUID>) -> bool {
    false
}

/// Does this adapter have **any** hardware encoder MFT? The resolution policy's question:
/// an unknown-vendor GPU (Adreno) with an MFT is a GPU backend, not the software rung.
pub fn probe_has_hardware_encoder(adapter_luid: Option<LUID>) -> bool {
    probe_can_encode(Codec::H264, adapter_luid) || probe_can_encode(Codec::H265, adapter_luid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Enumeration must not crash; no MFT → empty list or a clean error.
    #[test]
    fn mf_enumeration_smoke() {
        for codec in [Codec::H264, Codec::H265] {
            match enumerate(codec, None) {
                Ok(found) => {
                    let names: Vec<_> = found.iter().map(friendly_name).collect();
                    tracing::debug!(?codec, ?names, "Media Foundation encoder MFTs");
                }
                Err(e) => tracing::debug!(?codec, error = %format!("{e:#}"), "no MFT enumeration"),
            }
        }
    }

    /// Probe answers are booleans (no panic) with or without a hardware MFT.
    #[test]
    fn probe_smoke() {
        for codec in [Codec::H264, Codec::H265, Codec::Av1] {
            let can = probe_can_encode(codec, None);
            assert!(!probe_can_encode_10bit(codec, None));
            tracing::debug!(?codec, can, "MF probe");
        }
        // AV1 is refused at the subtype gate, whatever the box has.
        assert!(!probe_can_encode(Codec::Av1, None));
    }

    #[test]
    fn nal_classification_matches_the_two_codecs() {
        assert!(is_parameter_set(Codec::H264, 8));
        assert!(!is_parameter_set(Codec::H264, 5));
        assert!(is_parameter_set(Codec::H265, 34));
        assert!(!is_parameter_set(Codec::H265, 19));
    }

    /// The cached prefix must end exactly at the first slice NAL, start-code included.
    #[test]
    fn parameter_set_prefix_cuts_at_the_first_slice() {
        // SPS (0x67) + PPS (0x68) + IDR slice (0x65).
        let au = [
            0, 0, 0, 1, 0x67, 0xAA, //
            0, 0, 0, 1, 0x68, 0xBB, //
            0, 0, 0, 1, 0x65, 0xCC,
        ];
        let prefix = parameter_set_prefix(Codec::H264, &au).expect("SPS-led AU has a prefix");
        assert_eq!(prefix, &au[..12]);
        // An AU that opens with the slice has no prefix to cache.
        assert!(parameter_set_prefix(Codec::H264, &au[12..]).is_none());
        // Parameter sets with no slice after them are all prefix.
        assert_eq!(
            parameter_set_prefix(Codec::H264, &au[..12]).expect("all parameter sets"),
            &au[..12]
        );
    }

    /// An MFT that opens the AU with a delimiter still gets its header cached. That is
    /// the Qualcomm shape, and requiring the run to come first defeated it entirely.
    #[test]
    fn parameter_set_prefix_skips_a_leading_delimiter() {
        // AUD (0x09) + SPS (0x67) + PPS (0x68) + IDR slice (0x65).
        let au = [
            0, 0, 0, 1, 0x09, 0x10, //
            0, 0, 0, 1, 0x67, 0xAA, //
            0, 0, 0, 1, 0x68, 0xBB, //
            0, 0, 0, 1, 0x65, 0xCC,
        ];
        let prefix =
            parameter_set_prefix(Codec::H264, &au).expect("delimiter-led AU still has a prefix");
        assert_eq!(prefix, &au[6..18]);
        // SEI-led, parameter sets running to the end of the AU.
        assert_eq!(
            parameter_set_prefix(Codec::H264, &au[..18]).expect("delimiter then parameter sets"),
            &au[6..18]
        );
    }

    /// The adapter the device sits on, as `MFT_ENUM_ADAPTER_LUID` wants it.
    fn adapter_luid_of(device: &ID3D11Device) -> Option<LUID> {
        use windows::Win32::Graphics::Dxgi::IDXGIDevice;
        // SAFETY: standard COM navigation on a live device; every interface is an owned
        // windows-rs wrapper released on drop, and `GetDesc` fills a plain out-struct.
        unsafe {
            let dxgi: IDXGIDevice = device.cast().ok()?;
            Some(dxgi.GetAdapter().ok()?.GetDesc().ok()?.AdapterLuid)
        }
    }

    fn init_tracing() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("pf_encode_win=debug")
            .with_test_writer()
            .try_init();
    }

    struct AuMeta {
        keyframe: bool,
        annexb_start: bool,
        data: Vec<u8>,
    }

    /// Live encode on whatever MFT this box has. `None` = skip. `on_frame` runs before
    /// each submit, so a test can retarget or force mid-stream.
    fn drive_live(
        codec: Codec,
        frames: u32,
        mut on_frame: impl FnMut(&mut MfEncoder, u32),
    ) -> Option<Vec<AuMeta>> {
        use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
        };

        init_tracing();
        // SAFETY: self-contained harness owning every COM handle it creates;
        // `D3D11CreateDevice` fills `device` only on success, and the NV12 texture is
        // created on and used from that one device and thread.
        let (device, tex) = unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                windows::Win32::Foundation::HMODULE::default(),
                // The device manager refuses a device without this and the MFT then fails
                // SET_D3D_MANAGER with a bare E_FAIL — the driver's pooled device carries it
                // for the same reason.
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
            .expect("d3d11 device");
            let device: ID3D11Device = device.expect("device");
            let desc = D3D11_TEXTURE2D_DESC {
                Width: 640,
                Height: 480,
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
            let mut t: Option<ID3D11Texture2D> = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut t))
                .expect("input texture");
            (device.clone(), t.expect("texture"))
        };
        // Bind the MFT to the device's OWN adapter, as a session does. A box with two vendors'
        // encoders hands `None` whichever the sort ranks first, and an MFT from one adapter
        // refuses a device from another with a bare E_FAIL at SET_D3D_MANAGER.
        let luid = adapter_luid_of(&device).expect("device LUID");
        if !probe_can_encode(codec, Some(luid)) {
            eprintln!("skipping: no hardware {codec:?} MFT on this box's render adapter");
            return None;
        }
        let mut enc = MfEncoder::open(
            codec,
            PixelFormat::Nv12,
            640,
            480,
            30,
            2_000_000,
            8,
            ChromaFormat::Yuv420,
            Some(luid),
        )
        .expect("open");
        let mut aus = Vec::new();
        let mut push = |au: EncodedFrame| {
            aus.push(AuMeta {
                keyframe: au.keyframe,
                annexb_start: au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
                data: au.data,
            });
        };
        for i in 0..frames {
            on_frame(&mut enc, i);
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: 640,
                height: 480,
                pts_ns: u64::from(i) * 33_333_333,
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
                push(au);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("drain") {
            push(au);
        }
        Some(aus)
    }

    fn assert_stream_shape(aus: &[AuMeta], frames: u32) {
        assert!(
            aus.len() >= frames as usize - 5,
            "expected ~{frames} AUs, got {}",
            aus.len()
        );
        assert!(aus[0].keyframe, "first AU must be a keyframe");
        assert!(!aus[0].data.is_empty());
        assert!(aus[0].annexb_start, "first AU is not Annex-B");
    }

    #[test]
    fn mf_encode_live_smoke() {
        let Some(aus) = drive_live(Codec::H264, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30);
    }

    /// The first IDR's SPS carries BT.709 limited range, the capture's own colour.
    #[test]
    fn mf_live_colour_description() {
        use pf_bitstream::h264::ColourDescription;
        let bt709 = ColourDescription {
            colour_primaries: 1,
            transfer_characteristics: 1,
            matrix_coefficients: 1,
            video_full_range: false,
        };
        for codec in [Codec::H264, Codec::H265] {
            let Some(aus) = drive_live(codec, 5, |_, _| {}) else {
                continue;
            };
            let colour = match codec {
                Codec::H264 => pf_bitstream::h264::H264Planner::new()
                    .plan_au(&aus[0].data)
                    .ok()
                    .map(|p| p.picture.colour),
                _ => pf_bitstream::h265::H265Planner::new()
                    .plan_au(&aus[0].data)
                    .ok()
                    .map(|p| p.picture.colour),
            }
            .expect("first AU parses");
            assert_eq!(colour, bt709, "{codec:?} colour description");
        }
    }

    #[test]
    fn mf_live_hevc() {
        let Some(aus) = drive_live(Codec::H265, 30, |_, _| {}) else {
            return;
        };
        assert_stream_shape(&aus, 30);
    }

    /// Mid-stream `reconfigure_bitrate` must accept and must not emit a keyframe.
    #[test]
    fn mf_live_bitrate_retarget() {
        let mut accepted = false;
        let Some(aus) = drive_live(Codec::H264, 60, |enc, i| {
            if i == 30 {
                accepted = enc.reconfigure_bitrate(6_000_000);
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60);
        assert!(accepted, "the in-place bitrate retarget was declined");
        assert!(
            !aus[1..].iter().any(|x| x.keyframe),
            "the bitrate retarget emitted a keyframe"
        );
    }

    /// `request_keyframe` must produce an IDR where it was asked for — the one device
    /// question that decides whether loss recovery works at all (design §6, Q1).
    #[test]
    fn mf_live_force_idr() {
        let Some(aus) = drive_live(Codec::H264, 60, |enc, i| {
            if i == 30 {
                enc.request_keyframe();
            }
        }) else {
            return;
        };
        assert_stream_shape(&aus, 60);
        let forced: Vec<usize> = aus
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, a)| a.keyframe)
            .map(|(i, _)| i)
            .collect();
        assert!(
            forced.iter().any(|&i| (28..=34).contains(&i)),
            "no IDR near the forced frame (keyframes at {forced:?}) — this MFT ignores \
             AVEncVideoForceKeyFrame; loss recovery must fall back to a periodic GOP"
        );
    }
}
