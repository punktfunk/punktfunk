//! Direct-SDK NVENC encoder (Linux, CUDA input).
//!
//! Raw `nvEncodeAPI` so this host can do reference-frame invalidation, the recovery-anchor
//! tag, `reset()`, and HDR/Main10. CUDA frames go in zero-copy; CPU frames are uploaded.
//! The session itself is the shared [`NvSession`]; this file owns the `.so`, the CUDA
//! context, the input ring, the fused dmabuf convert and the cursor blend. Design:
//! `design/linux-direct-nvenc.md`; recovery: `encoder-recovery-hardening.md`.
//!
//! Loads `libnvidia-encode.so.1` at runtime (never a link-time import: AMD/Intel boxes still
//! start and fall through to VAAPI/software). Session is `NV_ENC_DEVICE_TYPE_CUDA` on the
//! shared process-wide `CUcontext`. Input is an encoder-owned ring of registered CUDA
//! surfaces; each `FramePayload::Cuda` is device→device copied into a slot. Stream-ordered
//! submit (default; `PUNKTFUNK_NVENC_STREAM_ORDERED=0` reverts) binds IO streams so copy +
//! blend enqueue without a CPU sync.
//!
//! Two-thread retrieve (`PUNKTFUNK_NVENC_ASYNC`: `1` always, `0` never, unset = adaptive via
//! [`Encoder::set_pipelined`]) keeps the session SYNC — Linux has no completion events — and
//! moves blocking `nvEncLockBitstream` off the encode thread. Sub-frame chunked poll (default
//! 4 slices + `SUBFRAME_READBACK`) is mutually exclusive with pipelined retrieve.
//!
//! Compiles GPU-less; [`try_api`] fails cleanly without a driver.

// `unsafe_op_in_unsafe_fn` off: this file is raw CUDA/`nvEncodeAPI` calls. Wrapping each one
// would add a SAFETY that only restates the prototype. Exit: delete the empty markers.
#![allow(unsafe_op_in_unsafe_fn)]

use super::nvenc_core::{CreateInstance, EncodeApi, GetMaxSupportedVersion, NvStatusExt};
use super::nvenc_session::{
    async_inflight_cap, async_retrieve_requested, async_retrieve_vetoed, lock_copy, open_session,
    EncodeInput, NvSession, OpenTarget, RetrieveDone, RetrieveJob, POOL,
};
use super::nvenc_status;
use super::{AuChunk, ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, ensure, Context, Result};
use pf_frame::{CapturedFrame, DmabufFrame, FramePayload};
use pf_zerocopy::cuda::{self, InputSurface};
use pf_zerocopy::vkslot::{SlotFormat, VkSlotBlend, VkSlotRef};
use std::collections::HashSet;
use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::ptr;
use std::sync::mpsc;

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

// Runtime-loaded NVENC entry table. Never a link-time import: `nvenc` is compiled in
// unconditionally, and a load-time `.so` would refuse to start on AMD/Intel-only boxes.

/// Resolve the table once per process. `Err` = no driver, no `.so`, or a driver older than
/// our headers. [`NvencCudaEncoder::open`] gates on it.
fn try_api() -> std::result::Result<&'static EncodeApi, &'static str> {
    static TABLE: std::sync::OnceLock<std::result::Result<EncodeApi, String>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            let table = load_api();
            if let Err(e) = &table {
                tracing::warn!(error = %e, "NVENC (Linux direct) API unavailable");
            }
            table
        })
        .as_ref()
        .map_err(|e| e.as_str())
}

/// Loaded table. Call only past a [`try_api`] gate; the mapping lives for the process.
fn api() -> &'static EncodeApi {
    try_api().expect("NVENC call before a successful try_api() gate")
}

/// Codec / 4:4:4 / 10-bit caps from one throwaway session. Per-field fail direction is on the
/// fields: codecs fail open, 4:4:4 and 10-bit fail closed.
#[derive(Clone, Copy)]
pub(crate) struct ProbedSupport {
    /// Encode GUIDs this chip lists. All-`false` = unanswered; [`crate::codec_support_wire_mask`]
    /// turns that into `None` so the caller keeps the static superset (fail open).
    pub codecs: crate::CodecSupport,
    /// HEVC 4:4:4 encode. `false` when unanswered (fail closed: a 4:2:0 session beats a dead
    /// one).
    pub hevc_444: bool,
    /// 10-bit encode per listed codec. `false` when unanswered (fail closed).
    pub ten_bit: crate::CodecSupport,
}

/// Cached [`probe_support_uncached`] — one throwaway session per process.
pub(crate) fn probe_support() -> ProbedSupport {
    static CACHE: std::sync::OnceLock<ProbedSupport> = std::sync::OnceLock::new();
    *CACHE.get_or_init(probe_support_uncached)
}

/// Ask this GPU's driver which codecs it encodes, plus HEVC 4:4:4 / 10-bit.
///
/// Same client and shared CUDA context as the live sessions: a second NVENC client in this
/// process wedges later opens (`NV_ENC_ERR_INVALID_VERSION`). Failures return "nothing
/// probed"; per-field fail direction is on [`ProbedSupport`].
fn probe_support_uncached() -> ProbedSupport {
    let unknown = ProbedSupport {
        codecs: crate::CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        },
        hevc_444: false,
        ten_bit: crate::CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        },
    };
    let Ok(api) = try_api() else {
        return unknown;
    };
    let cu_ctx = match cuda::context() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "NVENC codec probe: no CUDA context");
            return unknown;
        }
    };
    // SAFETY: `try_api()` Ok ⇒ every fn pointer is a live driver entry. `target` names the
    // shared CUDA context; `open_session` destroys a failed open's residue. `count`/`written`
    // outlive their sync calls and `guids` is sized to the reported count. The session is
    // destroyed once, before the listing is read.
    unsafe {
        let target = OpenTarget {
            device_type: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
            device: cu_ctx,
        };
        let enc = match open_session(api, target) {
            Ok(enc) => enc,
            Err(e) => {
                tracing::warn!(
                    error = %format!("{:#}", nvenc_status::call_err("open_encode_session_ex (codec probe)", e)),
                    "NVENC codec probe failed — keeping the static codec advertisement"
                );
                return unknown;
            }
        };
        let mut count = 0u32;
        let counted = (api.get_encode_guid_count)(enc, &mut count).nv_ok().is_ok();
        let mut guids = vec![nv::GUID::default(); count as usize];
        let mut written = 0u32;
        let listed = counted
            && count > 0
            && (api.get_encode_guids)(enc, guids.as_mut_ptr(), count, &mut written)
                .nv_ok()
                .is_ok();
        guids.truncate(written as usize);
        // Cap query for an absent codec is undefined — only against a listed HEVC GUID.
        let mut hevc_444 = false;
        if listed && guids.contains(&nv::NV_ENC_CODEC_HEVC_GUID) {
            let mut param = nv::NV_ENC_CAPS_PARAM {
                version: nv::NV_ENC_CAPS_PARAM_VER,
                capsToQuery: nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
                reserved: [0; 62],
            };
            let mut val: core::ffi::c_int = 0;
            hevc_444 = (api.get_encode_caps)(enc, nv::NV_ENC_CODEC_HEVC_GUID, &mut param, &mut val)
                .nv_ok()
                .is_ok()
                && val != 0;
        }
        // Same still-open session; skip unlisted GUIDs (cap query for an absent codec is undefined).
        let mut ten_bit = crate::CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        if listed {
            for (guid, slot) in [
                (nv::NV_ENC_CODEC_HEVC_GUID, &mut ten_bit.h265),
                (nv::NV_ENC_CODEC_AV1_GUID, &mut ten_bit.av1),
            ] {
                if !guids.contains(&guid) {
                    continue;
                }
                let mut param = nv::NV_ENC_CAPS_PARAM {
                    version: nv::NV_ENC_CAPS_PARAM_VER,
                    capsToQuery: nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
                    reserved: [0; 62],
                };
                let mut val: core::ffi::c_int = 0;
                *slot = (api.get_encode_caps)(enc, guid, &mut param, &mut val)
                    .nv_ok()
                    .is_ok()
                    && val != 0;
            }
        }
        let _ = (api.destroy_encoder)(enc);
        if !listed {
            tracing::warn!(
                "NVENC codec probe: driver listed no encode GUIDs — keeping the static advertisement"
            );
            return unknown;
        }
        ProbedSupport {
            codecs: crate::CodecSupport {
                h264: guids.contains(&nv::NV_ENC_CODEC_H264_GUID),
                h265: guids.contains(&nv::NV_ENC_CODEC_HEVC_GUID),
                av1: guids.contains(&nv::NV_ENC_CODEC_AV1_GUID),
            },
            hevc_444,
            ten_bit,
        }
    }
}

fn load_api() -> std::result::Result<EncodeApi, String> {
    // SAFETY: `Library::new` runs the trusted NVIDIA driver's initializers; absence is `Err`.
    // Each `lib.get::<T>` matches the `nvEncodeAPI.h` prototype. Fn pointers are copied out of
    // `Symbol`; `forget(lib)` leaks the mapping for the process, as the table needs.
    unsafe {
        let lib = libloading::Library::new("libnvidia-encode.so.1")
            .or_else(|_| libloading::Library::new("libnvidia-encode.so"))
            .map_err(|e| format!("libnvidia-encode.so.1 not loadable (no NVIDIA driver?): {e}"))?;
        let get_version: libloading::Symbol<GetMaxSupportedVersion> = lib
            .get(b"NvEncodeAPIGetMaxSupportedVersion\0")
            .map_err(|e| {
                format!("libnvidia-encode exports no NvEncodeAPIGetMaxSupportedVersion: {e}")
            })?;
        let create_instance: libloading::Symbol<CreateInstance> = lib
            .get(b"NvEncodeAPICreateInstance\0")
            .map_err(|e| format!("libnvidia-encode exports no NvEncodeAPICreateInstance: {e}"))?;
        let api = EncodeApi::from_exports(*get_version, *create_instance)?;
        std::mem::forget(lib); // mapping must outlive the copied fn pointers (process)
        Ok(api)
    }
}

/// Stream-ordered submit (default on; `PUNKTFUNK_NVENC_STREAM_ORDERED=0` = blocking copies).
/// Sync retrieve only, and only while `pending` is empty — see [`Encoder::submit`].
fn stream_ordered_requested() -> bool {
    std::env::var("PUNKTFUNK_NVENC_STREAM_ORDERED")
        .map(|v| v.trim() != "0")
        .unwrap_or(true)
}

/// A dead convert worker waits this long before the lane spawns another. A crash loop would
/// otherwise cost a process and a CUDA context on every frame; three deaths trip the raw lane's
/// degrade latch, and the next session negotiates the import path instead.
const WORKER_RESPAWN_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// The raw frame the fused pass last converted, and the ring slot holding the result.
/// `cursor` is what the pass baked in — see [`cursor_mark`].
#[derive(Clone, Copy)]
struct LastRaw {
    pts_ns: u64,
    fd: i32,
    slot: usize,
    cursor: Option<(u64, i32, i32)>,
}

/// The cursor state the fused pass burns into a slot: the bitmap's serial and where it landed.
/// `serial` bumps only when the bitmap changes, so a position-only move needs `x`/`y` too.
/// `None` when nothing is drawn — same gate the convert uses.
fn cursor_mark(captured: &CapturedFrame) -> Option<(u64, i32, i32)> {
    captured
        .cursor
        .as_ref()
        .filter(|ov| ov.visible && ov.w > 0 && ov.h > 0 && !ov.rgba.is_empty())
        .map(|ov| (ov.serial, ov.x, ov.y))
}

/// A producer NV12 or P010's chroma `(offset, stride)`: the reported plane, else the contiguous
/// one below the luma. `None` for packed RGB, which the worker converts instead of copying.
fn chroma_plane(captured: &CapturedFrame, d: &DmabufFrame) -> Option<(u32, u32)> {
    use pf_frame::PixelFormat::{Nv12, P010};
    matches!(captured.format, Nv12 | P010).then(|| {
        d.plane1
            .unwrap_or((d.offset + d.stride * captured.height, d.stride))
    })
}

/// Retrieve thread: blocking-lock, copy, unlock, send. Exits when the job channel closes —
/// teardown drops the sender and joins before destroy, so `enc`/`bs` outlive uses here.
fn retrieve_loop(
    enc: usize,
    work_rx: mpsc::Receiver<RetrieveJob>,
    done_tx: mpsc::Sender<RetrieveDone>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    // Shared process-wide CUDA context — same `cuCtxSetCurrent` the encode thread does.
    if let Err(e) = cuda::make_current() {
        tracing::warn!(error = %format!("{e:#}"), "pf-nvenc-out: cuCtxSetCurrent failed");
    }
    let mut jobs: u64 = 0;
    while let Ok(job) = work_rx.recv() {
        // Host `wait_us` wraps a non-blocking poll here, so the encode wait is sampled on this
        // thread (same `PUNKTFUNK_PERF` cadence as submit: every 120).
        let sample = pf_host_config::config().perf && jobs % 120 == 0;
        jobs += 1;
        let t0 = std::time::Instant::now();
        // SAFETY: `job.bs` is a pool bitstream a prior `encode_picture` targeted; teardown
        // joins this thread first. Secondary-thread lock/unlock while the encode thread
        // submits is the NVENC guide's threading model.
        let result = unsafe { lock_copy(api(), enc, job.bs) };
        if sample {
            if let Ok((data, _)) = &result {
                tracing::info!(
                    lock_us = t0.elapsed().as_micros() as u64,
                    au_kib = (data.len() / 1024) as u64,
                    "NVENC retrieve lock (sampled): blocking lock_bitstream + AU copy on \
                     pf-nvenc-out (the async-mode encode wait)"
                );
            }
        }
        if done_tx.send(RetrieveDone { bs: job.bs, result }).is_err() {
            break; // encode thread dropped the receiver (teardown joins us)
        }
    }
}

/// NVENC buffer format for a captured frame. NV12/YUV444 come from `DeviceBuffer` layout;
/// packed RGB is 4 bytes/px either way, so depth and channel order come from `fmt`, not `buf`.
/// Packed RGB lets NVENC do the CSC (BT.2020 NCL when HDR) — no host CSC, no depth loss.
fn buffer_format(buf: &cuda::DeviceBuffer, fmt: pf_frame::PixelFormat) -> nv::NV_ENC_BUFFER_FORMAT {
    if buf.is_yuv444() {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
    } else if buf.is_nv12() {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12
    } else {
        match fmt {
            // `x:R:G:B` 2:10:10:10 LE = NVENC `ARGB10` (B in the low 10 bits).
            pf_frame::PixelFormat::X2Rgb10 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10,
            // `x:B:G:R` 2:10:10:10 LE = NVENC `ABGR10` (R in the low 10 bits).
            pf_frame::PixelFormat::X2Bgr10 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10,
            // Packed BGRA (`copy_device_to_device` fallback); NVENC `ARGB` does the CSC.
            _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
        }
    }
}

/// Encode depth and HDR verdict for one capture format.
///
/// A 10-bit input pins depth 10 and carries the session's colour (`hdr_asked`): BT.2020 PQ, or
/// BT.709 when gamescope composited 10-bit SDR. An 8-bit capture reaches a 10-bit SDR stream
/// only through NVENC's 8→10, which takes PACKED RGB (`ARGB`); a planar 8-bit surface
/// (NV12/YUV444) fails `register_resource` in a 10-bit session, so it stays 8-bit. An 8-bit
/// stream is never HDR.
fn depth_and_hdr(fmt: nv::NV_ENC_BUFFER_FORMAT, depth_asked: u8, hdr_asked: bool) -> (u8, bool) {
    if is_ten_bit_input(fmt) {
        return (10, hdr_asked);
    }
    let packed_rgb8 = fmt == nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
    (
        if depth_asked >= 10 && packed_rgb8 {
            10
        } else {
            8
        },
        false,
    )
}

/// A PQ input: 10-bit in an HDR session. The cursor blends re-encoded as PQ there; sRGB bytes
/// would be read as PQ.
fn pq_input(hdr_asked: bool, fmt: pf_frame::PixelFormat) -> bool {
    hdr_asked && fmt.is_ten_bit()
}

/// 10-bit input: packed RGB, or a producer's P010. Depth follows the capture format; the
/// colour is the session's.
fn is_ten_bit_input(fmt: nv::NV_ENC_BUFFER_FORMAT) -> bool {
    matches!(
        fmt,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10
            | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
            | nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT
    )
}

/// Encoder-owned input surface + NVENC registration (once at init, unregistered at teardown).
/// What a submit carries: a CUDA buffer the encoder copies into a ring slot, or the held
/// dmabuf the zero-copy worker's fused pass writes into the slot directly.
#[derive(Clone, Copy)]
enum Source<'a> {
    Cuda(&'a cuda::DeviceBuffer),
    Dmabuf(&'a DmabufFrame),
}

/// Clone the producer hold only for a raw dmabuf submission.
fn source_hold(frame: &CapturedFrame) -> Option<pf_frame::FrameHold> {
    match &frame.payload {
        FramePayload::Dmabuf(d) => d.hold.clone(),
        _ => None,
    }
}

struct RingSlot {
    surface: SlotSurface,
    reg: nv::NV_ENC_REGISTERED_PTR,
}

/// Ring-slot backing: Vulkan-imported (cursor-blendable) or pitched CUDA (encode still works,
/// no cursor). Same `(ptr, pitch, height)` for NVENC registration.
enum SlotSurface {
    Cuda(InputSurface),
    /// Backing lives in [`VkSlotBlend`] (`free_slots`); the ref is Copy geometry.
    Vk(VkSlotRef),
}

/// Area-average a straight-alpha RGBA bitmap down to `tw × th`, weighting colour by alpha so
/// transparent pixels do not darken the edge. The pointer then keeps its size relative to a
/// reframed picture.
fn shrink_rgba(rgba: &[u8], w: u32, h: u32, tw: u32, th: u32) -> Vec<u8> {
    let (w, h, tw, th) = (w as usize, h as usize, tw as usize, th as usize);
    let mut out = vec![0u8; tw * th * 4];
    if rgba.len() < w * h * 4 {
        return out;
    }
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h).div_ceil(th).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w).div_ceil(tw).min(w));
            let (mut rgb, mut alpha, mut n) = ([0u64; 3], 0u64, 0u64);
            for y in y0..y1.max(y0 + 1) {
                for x in x0..x1.max(x0 + 1) {
                    let p = &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4];
                    let a = u64::from(p[3]);
                    for c in 0..3 {
                        rgb[c] += u64::from(p[c]) * a;
                    }
                    alpha += a;
                    n += 1;
                }
            }
            let o = (ty * tw + tx) * 4;
            for c in 0..3 {
                out[o + c] = rgb[c].checked_div(alpha).unwrap_or(0) as u8;
            }
            out[o + 3] = (alpha / n) as u8;
        }
    }
    out
}

fn slot_fmt_of(fmt: nv::NV_ENC_BUFFER_FORMAT) -> SlotFormat {
    match fmt {
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => SlotFormat::Yuv444,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => SlotFormat::Nv12,
        // Geometry matches `Argb` (4 B/px) but blend must unpack 10-bit channels, not bytes.
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10 => SlotFormat::X2Rgb10,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10 => SlotFormat::X2Bgr10,
        nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT => SlotFormat::P010,
        _ => SlotFormat::Argb,
    }
}

impl SlotSurface {
    fn ptr(&self) -> pf_zerocopy::cuda::CUdeviceptr {
        match self {
            SlotSurface::Cuda(s) => s.ptr,
            SlotSurface::Vk(r) => r.ptr,
        }
    }
    fn pitch(&self) -> usize {
        match self {
            SlotSurface::Cuda(s) => s.pitch,
            SlotSurface::Vk(r) => r.pitch,
        }
    }
    fn height(&self) -> u32 {
        match self {
            SlotSurface::Cuda(s) => s.height,
            SlotSurface::Vk(r) => r.height,
        }
    }
}

/// The host's crop and scale ([`Encoder::set_input_crop`]): each capture lands in a source-size
/// staging slot, and [`VkSlotBlend::reframe`] writes the NVENC slot from it.
struct NvReframe {
    /// `x, y, w, h` in source pixels.
    crop: [u32; 4],
    /// The session's picture size.
    out: (u32, u32),
    /// The capture size the staging ring was built for.
    src: (u32, u32),
    /// One source-size slot per ring slot, same layout. Freed with the ring.
    staging: Vec<VkSlotRef>,
    /// The cursor bitmap scaled by the reframe: `(serial, rgba, w, h)`.
    cursor: Option<(u64, Vec<u8>, u32, u32)>,
}

pub struct NvencCudaEncoder {
    /// The session state and per-frame logic shared with the Windows backend.
    s: NvSession,
    /// Process-wide `CUcontext` this session is bound to.
    cu_ctx: *mut c_void,
    /// Depth the session negotiated. Held because the session's bit depth follows the
    /// capture: packed 10-bit input pins 10 ([`is_ten_bit_input`]); an 8-bit capture takes
    /// this, which NVENC writes as a 10-bit stream from the 8-bit surface. The NV12/YUV444
    /// converts write 8-bit planes.
    depth_asked: u8,
    /// The session's colour for a 10-bit input: BT.2020 PQ, or BT.709 for gamescope's 10-bit
    /// SDR. An 8-bit input is SDR whatever this says.
    hdr_asked: bool,
    /// Device copy of the last CPU frame, reused while the size and layout hold.
    upload: Option<cuda::DeviceBuffer>,
    ring: Vec<RingSlot>,
    /// Lifetime submit count (never reset) — `PUNKTFUNK_PERF` sample cadence.
    frames: u64,
    /// Vulkan SPIR-V cursor blend (`vkslot.rs`). `None` = bring-up failed, ring is plain CUDA,
    /// no cursor. `cursor_tried` is one-shot; `cursor_serial` is the uploaded bitmap.
    vk_blend: Option<VkSlotBlend>,
    /// Crop and scale ahead of the encode. Needs `vk_blend`.
    reframe: Option<NvReframe>,
    /// Cursor overlays expected. Off = skip Vulkan bring-up; embedded-pointer sessions never
    /// carry an overlay.
    blend_wanted: bool,
    cursor_tried: bool,
    cursor_serial: u64,
    /// Blend-warn latch: once per failure streak. A warn on every cursor frame would evict
    /// the log ring.
    cursor_blend_warned: bool,
    /// The fused convert lane (`PUNKTFUNK_NVENC_RAW`): the zero-copy worker writes held
    /// dmabufs straight into the ring. `worker_slots` are the ring ids it has imported.
    raw_wanted: bool,
    worker: Option<pf_zerocopy::Importer>,
    worker_slots: HashSet<usize>,
    worker_cursor_serial: u64,
    /// The last raw frame the fused pass converted, and where it landed.
    last_raw: Option<LastRaw>,
    /// The worker's convert timeline in CUDA: each pass's value is waited on the copy stream
    /// before NVENC maps the slot.
    convert_sem: Option<cuda::ExternalSemaphore>,
    /// Highest value waited on `convert_sem`: what a stuck copy stream is queued behind.
    convert_waited: u64,
    /// Earliest the lane may spawn another convert worker ([`WORKER_RESPAWN_BACKOFF`]).
    worker_retry_at: Option<std::time::Instant>,
    /// One-shot [`diagnose_failed_open`](Self::diagnose_failed_open) — a reset burst logs once.
    diagnosed: bool,
    /// Pipelined-retrieve escalation. Sticky across rebuilds; switch is at the next drained
    /// point via [`maybe_engage_async`](Self::maybe_engage_async).
    want_async: bool,
    /// De-escalation waiting for a drained point. Distinct from `!want_async`: operator-forced
    /// async (`PUNKTFUNK_NVENC_ASYNC=1`) also has `want_async` false and must not be torn down.
    want_sync: bool,
    /// Heap `CUstream` the IO-stream binding points at (the API takes pointers, this struct
    /// moves). Null when off; freed in `teardown` *after* destroy.
    io_stream: *mut *mut c_void,
    /// Stream-ordered submit armed (sync retrieve). Per-frame gate also requires `pending` empty.
    stream_ordered: bool,
}

// SAFETY: the session's NVENC handles, `cu_ctx`, and the raw pointers in `ring` are `!Send`.
// The encoder is moved onto the host encode thread once; `submit`/`poll`/`RFI`/`Drop` run
// there. The retrieve thread (when armed) only lock/unlocks bitstreams and is joined in
// `teardown` before destroy. The ownership-transfer move has no NVENC/CUDA call in flight.
unsafe impl Send for NvencCudaEncoder {}

impl NvencCudaEncoder {
    /// Same signature as `super::NvencEncoder::open`. `format`/`cuda` are advisory: real input
    /// comes from the first captured frame, and its depth follows that frame — a 10-bit session
    /// whose capture is 8-bit must encode and label 8-bit. `hdr` is the session's colour, which
    /// a 10-bit input carries.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        _format: pf_frame::PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        _cuda: bool,
        bit_depth: u8,
        hdr: bool,
        chroma: ChromaFormat,
        cursor_blend: bool,
        max_slices: u32,
    ) -> Result<Self> {
        // Fail here, not as an opaque session error on the first frame.
        let api = try_api().map_err(|e| anyhow!("NVENC (Linux direct) unavailable: {e}"))?;
        let mut s = NvSession::new(api, codec, width, height, fps, bitrate_bps, max_slices);
        // Provisional until the first frame names the real input (`ensure_session` sets both).
        s.bit_depth = bit_depth;
        // HEVC-only; confirmed against frame layout + GPU at init.
        s.chroma_444 = chroma.is_444() && codec == Codec::H265;
        Ok(Self {
            s,
            cu_ctx: ptr::null_mut(),
            depth_asked: bit_depth,
            hdr_asked: hdr,
            upload: None,
            ring: Vec::new(),
            frames: 0,
            vk_blend: None,
            reframe: None,
            blend_wanted: cursor_blend,
            cursor_tried: false,
            cursor_serial: u64::MAX,
            cursor_blend_warned: false,
            // Capture's identity-scoped latch controls whether a dmabuf arrives; the encoder
            // only applies the raw-lane knob to that payload.
            raw_wanted: pf_zerocopy::nvenc_raw_enabled(),
            worker: None,
            worker_slots: HashSet::new(),
            worker_cursor_serial: u64::MAX,
            last_raw: None,
            convert_sem: None,
            convert_waited: 0,
            worker_retry_at: None,
            diagnosed: false,
            want_async: false,
            want_sync: false,
            io_stream: ptr::null_mut(),
            stream_ordered: false,
        })
    }

    /// Engage pipelined retrieve when `pending` is empty: rebuild *without* the IO-stream
    /// binding (its output-stream wait would serialize a pipelined session). Re-open starts
    /// with an IDR. No-op until [`want_async`](Self::want_async).
    fn maybe_engage_async(&mut self) {
        if !self.want_async || self.s.retrieving() || !self.s.pending().is_empty() {
            return;
        }
        if self.s.inited() {
            // SAFETY: encode thread, `pending` empty ⇒ nothing in flight. `teardown` handles
            // this live session; next submit lazily re-inits and spawns the retrieve thread.
            unsafe { self.teardown() };
            tracing::info!(
                "NVENC pipelined-retrieve escalation: rebuilding the session without the \
                 IO-stream binding (stream-ordered submit and two-thread retrieve are mutually \
                 exclusive); next frame opens with an IDR"
            );
        }
    }

    /// Inverse of [`maybe_engage_async`](Self::maybe_engage_async): drain, rebuild, lazy SYNC
    /// re-init restores IO-stream binding and sub-frame chunking. No-op until
    /// [`want_sync`](Self::want_sync).
    fn maybe_disengage_async(&mut self) {
        if !self.want_sync || !self.s.retrieving() || !self.s.pending().is_empty() {
            return;
        }
        self.want_sync = false;
        if self.s.inited() {
            // SAFETY: encode thread, `pending` empty ⇒ nothing in flight or queued. `teardown`
            // joins the retrieve thread; next submit lazily re-inits sync.
            unsafe { self.teardown() };
            tracing::info!(
                "NVENC pipelined-retrieve de-escalation: rebuilding the session with the sync \
                 retrieve (IO-stream binding and sub-frame chunking restored); next frame opens \
                 with an IDR"
            );
        }
    }

    /// Drop the fused-pass semaphore once no queued wait needs it. CUDA forbids destroying it
    /// under a wait, and a stalled or dead worker never signals: past a short drain the worker
    /// goes and the wait is satisfied here, so the copy stream (NVENC's input) runs on.
    fn release_convert_sem(&mut self) {
        let Some(sem) = self.convert_sem.take() else {
            return;
        };
        let waited = std::mem::take(&mut self.convert_waited);
        let drained = |budget| {
            cuda::make_current().is_ok() && cuda::copy_stream_sync_deadline(budget).is_ok()
        };
        if waited == 0 || drained(std::time::Duration::from_millis(100)) {
            return;
        }
        // The old worker must not signal after this: a timeline only moves forward.
        self.worker = None;
        if sem.signal_detached(waited).is_ok() && drained(std::time::Duration::from_secs(1)) {
            tracing::warn!(
                value = waited,
                "fused-pass wait satisfied for a retired worker"
            );
            return;
        }
        tracing::error!(
            value = waited,
            "copy stream stuck on the fused pass; semaphore leaked"
        );
        std::mem::forget(sem);
    }

    /// Stop retrieval, drain the copy stream, unmap pending inputs, then release source holds
    /// and session resources.
    unsafe fn teardown(&mut self) {
        if self.s.handle().is_null() {
            return;
        }
        // An encode queued behind a fused-pass wait still reads the inputs the session unmaps.
        self.release_convert_sem();
        // Join the retrieve thread, then unmap. An in-flight lock returns in ≤ a frame; after
        // join no other thread can touch the session destroyed below.
        self.s.stop();
        for slot in &self.ring {
            let _ = (api().unregister_resource)(self.s.handle(), slot.reg);
        }
        if let Some(w) = self.worker.as_mut() {
            w.forget_slots();
            // The ring is retired, so every dmabuf the worker cached against it is stale.
            // This releases the held fds and the Vulkan images they imported; without it a
            // renegotiation leaves the old pool's imports sitting under the new one's.
            w.clear_cache();
        }
        self.worker_slots.clear();
        // `clear_cache` dropped the worker's cursor bitmap; the next frame must upload it again.
        self.worker_cursor_serial = u64::MAX;
        self.last_raw = None;
        // A failed destroy can leak a slot toward the concurrent-session cap (per process;
        // only a restart clears it).
        self.s.close(|enc| {
            if let Err(e) = (api().destroy_encoder)(enc).nv_ok() {
                tracing::warn!(
                    status = ?e,
                    "NVENC destroy_encoder failed at teardown — the driver may have leaked this \
                     session's slot toward the concurrent-session cap"
                );
            }
        });
        // IO-stream pointee: free only after destroy. Null so a re-init cannot double-free.
        if !self.io_stream.is_null() {
            drop(Box::from_raw(self.io_stream));
            self.io_stream = ptr::null_mut();
        }
        self.stream_ordered = false;
        self.ring.clear(); // CUDA InputSurfaces; Vk slots freed just below
        if let Some(r) = &mut self.reframe {
            r.staging.clear();
        }
        if let Some(vk) = &mut self.vk_blend {
            // Slot memory + CUDA mapping. Device stays up (`cursor_tried` is one-shot).
            vk.free_slots();
        }
    }

    /// One-shot open-failure diagnosis. Retries on a fresh CUDA context:
    /// shared-ctx poison vs driver (skew / session-cap / GPU lost) vs CUDA itself.
    /// Log-only; latched so a reset burst logs once.
    fn diagnose_failed_open(&mut self) {
        if self.diagnosed {
            return;
        }
        self.diagnosed = true;
        let fresh = cuda::with_fresh_context(|ctx| {
            let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device: ctx,
                apiVersion: nv::NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut enc: *mut c_void = ptr::null_mut();
            // SAFETY: `params`/`enc` outlive the sync call; `ctx` is the fresh diagnostic
            // context. Destroy any session the probe opened, including a failed status.
            unsafe {
                let st = (api().open_encode_session_ex)(&mut params, &mut enc);
                if !enc.is_null() {
                    let _ = (api().destroy_encoder)(enc);
                }
                st
            }
        });
        match fresh {
            Ok(nv::NVENCSTATUS::NV_ENC_SUCCESS) => tracing::error!(
                "NVENC self-diagnosis: the session opens FINE on a fresh CUDA context — the \
                 host's shared CUDA context is in a bad state (host bug; please report this log)"
            ),
            Ok(st) => tracing::error!(
                fresh_ctx_status = ?st,
                "NVENC self-diagnosis: the open fails on a fresh CUDA context too — driver-level \
                 cause: {}",
                nvenc_status::explain(st)
            ),
            Err(e) => tracing::error!(
                error = %format!("{e:#}"),
                "NVENC self-diagnosis: no fresh CUDA context — CUDA itself is \
                 unhealthy in this process (GPU reset/fell off the bus, or a poisoned driver \
                 state); a host restart should clear it"
            ),
        }
    }

    /// Opens the lazy session and input ring after the first frame fixes their format.
    fn init_session(&mut self) -> Result<()> {
        // SAFETY: NVENC calls go through `api()` (gated in `open`). `target` names the shared
        // CUDA context, live for the process, and the session is closed (`ensure_session` tore
        // it down). `enc` is the session just opened, as `build_ring` and `bind_io_streams`
        // require. Encode thread only.
        unsafe {
            self.cu_ctx = cuda::context().context("shared CUDA context (Linux direct NVENC)")?;
            self.s.gpu = self.cu_ctx as u64;
            cuda::make_current().context("cuCtxSetCurrent (encode thread)")?;
            let target = OpenTarget {
                device_type: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device: self.cu_ctx,
            };
            if let Err(e) = self.s.query_caps(target) {
                // First open of any session — one-shot diagnosis before propagating.
                self.diagnose_failed_open();
                return Err(e);
            }
            let split_mode = self.s.resolve_shape();
            self.s.full_range = self.s.buffer_fmt
                == nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444
                && pf_zerocopy::egl::yuv444_full_range();
            self.s.open(target, split_mode)?;
            let enc = self.s.handle();
            self.s.create_bitstreams()?;

            // The Vulkan slot blend, tried once per encoder; without it the ring is plain CUDA.
            if !self.cursor_tried && (self.blend_wanted || self.raw_wanted) {
                self.cursor_tried = true;
                match VkSlotBlend::new() {
                    Ok(v) => self.vk_blend = Some(v),
                    Err(e) => tracing::warn!(
                        error = %format!("{e:#}"),
                        "NVENC (Linux): Vulkan slot-blend bring-up failed — plain CUDA input \
                         surfaces, cursor compositing unavailable"
                    ),
                }
            }
            let slot_fmt = slot_fmt_of(self.s.buffer_fmt);
            self.build_ring(enc, slot_fmt)?;
            if let (Some(r), Some(vk)) = (self.reframe.as_mut(), self.vk_blend.as_mut()) {
                for _ in 0..POOL {
                    r.staging.push(
                        vk.alloc_slot(slot_fmt, r.src.0, r.src.1)
                            .context("NVENC (Linux): reframe staging slot")?,
                    );
                }
            }

            // Retrieve thread against the live session. Teardown joins it before destroy.
            if async_retrieve_requested() || self.want_async {
                self.s.start_retrieve("pf-nvenc-out", retrieve_loop)?;
                tracing::info!(
                    depth = async_inflight_cap(),
                    escalated = self.want_async,
                    "NVENC two-thread retrieve enabled (submit thread + blocking-lock thread)"
                );
            }
            self.bind_io_streams(enc);
            let s = &self.s;
            tracing::info!(
                mode = %format_args!("{}x{}@{}", s.width, s.height, s.fps),
                bit_depth = s.bit_depth,
                mbps = s.bitrate_bps / 1_000_000,
                codec = ?s.codec_guid,
                fmt = ?s.buffer_fmt,
                // Final split (post-fallback) at INFO — journals run INFO+.
                split_mode = s.split_mode,
                // Engine count: the driver silently honours an over-wide request, so mode
                // alone cannot be trusted.
                engines = s.encoder_engines,
                slices = s.slices,
                subframe = s.subframe_on,
                "NVENC CUDA session ready"
            );
            // Chunked poll: multi-slice + sub-frame + sync retrieve. AV1 is 1 slice —
            // `resolve_split_subframe` disarms sub-frame there so the writer and reader agree.
            self.s.finish_open();
            Ok(())
        }
    }

    /// Fill the input ring: register once, map per submit. Vulkan-imported slots when the
    /// blend is up, so the cursor blend writes the bytes NVENC encodes; any Vulkan failure
    /// retires the whole ring and rebuilds it on pitched CUDA. Never mixed (a flickering cursor)
    /// or short.
    ///
    /// # Safety
    /// `enc` is this encoder's open session; every slot lands in `self.ring` once registered,
    /// which `teardown` unregisters.
    unsafe fn build_ring(&mut self, enc: *mut c_void, slot_fmt: SlotFormat) -> Result<()> {
        let (width, height, buffer_fmt) = (self.s.width, self.s.height, self.s.buffer_fmt);
        'ring: for use_vk in [self.vk_blend.is_some(), false] {
            if !use_vk && self.reframe.is_some() {
                bail!("NVENC (Linux): the reframe needs Vulkan input slots");
            }
            if !use_vk && self.vk_blend.is_some() {
                // Wholesale Vulkan retire before the CUDA retry.
                for s in self.ring.drain(..) {
                    let _ = (api().unregister_resource)(enc, s.reg);
                }
                if let Some(vk) = &mut self.vk_blend {
                    vk.free_slots();
                }
                self.vk_blend = None;
            }
            for _ in 0..POOL {
                let surface = if use_vk {
                    let vk = self.vk_blend.as_mut().expect("use_vk implies Some");
                    match vk.alloc_slot(slot_fmt, width, height) {
                        Ok(r) => SlotSurface::Vk(r),
                        Err(e) => {
                            tracing::warn!(
                                error = %format!("{e:#}"),
                                "NVENC (Linux): Vulkan slot alloc failed — rebuilding the \
                                 ring on plain CUDA surfaces (cursor compositing \
                                 unavailable)"
                            );
                            continue 'ring;
                        }
                    }
                } else {
                    // P010 is NV12's geometry at two bytes a sample.
                    use nv::NV_ENC_BUFFER_FORMAT as F;
                    let (layout, row_px) = match buffer_fmt {
                        F::NV_ENC_BUFFER_FORMAT_YUV444 => (cuda::PlaneLayout::Yuv444, width),
                        F::NV_ENC_BUFFER_FORMAT_NV12 => (cuda::PlaneLayout::Nv12, width),
                        F::NV_ENC_BUFFER_FORMAT_YUV420_10BIT => {
                            (cuda::PlaneLayout::Nv12, width * 2)
                        }
                        _ => (cuda::PlaneLayout::Packed32, width),
                    };
                    SlotSurface::Cuda(
                        InputSurface::alloc(layout, row_px, height)
                            .context("alloc NVENC input surface")?,
                    )
                };
                let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                    version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                    resourceType:
                        nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                    width,
                    height,
                    pitch: surface.pitch() as u32,
                    resourceToRegister: surface.ptr() as *mut c_void,
                    bufferFormat: buffer_fmt,
                    bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                    ..Default::default()
                };
                match (api().register_resource)(enc, &mut rr).nv_ok() {
                    Ok(()) => {}
                    Err(e) if use_vk => {
                        // Import refused — same wholesale CUDA fallback.
                        tracing::warn!(
                            error = ?e,
                            "NVENC (Linux): registering a Vulkan-imported slot failed — \
                             rebuilding the ring on plain CUDA surfaces"
                        );
                        continue 'ring;
                    }
                    Err(e) => {
                        return Err(nvenc_status::call_err(
                            "register_resource (CUDADEVICEPTR)",
                            e,
                        ));
                    }
                }
                self.ring.push(RingSlot {
                    surface,
                    reg: rr.registeredResource,
                });
            }
            break 'ring;
        }
        Ok(())
    }

    /// Bind the IO streams to this thread's copy stream, the same both ways so a later copy
    /// into a reused slot waits for the encode. Sync retrieve only: two-thread mode may recycle
    /// the captured buffer after `submit` while the stream still holds the copy.
    ///
    /// # Safety
    /// `enc` is this encoder's open session. The boxed `CUstream` is freed once: by `teardown`
    /// after destroy, or here when the driver rejects it.
    unsafe fn bind_io_streams(&mut self, enc: *mut c_void) {
        if !self.s.retrieving() && stream_ordered_requested() {
            let stream = cuda::copy_stream_handle();
            if !stream.is_null() {
                // Driver takes `CUstream` pointers — box; `teardown` frees after destroy.
                let holder = Box::into_raw(Box::new(stream));
                match (api().set_io_cuda_streams)(
                    enc,
                    holder as nv::NV_ENC_CUSTREAM_PTR,
                    holder as nv::NV_ENC_CUSTREAM_PTR,
                )
                .nv_ok()
                {
                    Ok(()) => {
                        self.io_stream = holder;
                        self.stream_ordered = true;
                        tracing::info!(
                            "NVENC stream-ordered submit armed (IO streams bound — no CPU \
                             sync in the submit path)"
                        );
                    }
                    Err(e) => {
                        drop(Box::from_raw(holder));
                        tracing::debug!(
                            status = ?e,
                            "NvEncSetIOCudaStreams rejected — keeping blocking copies"
                        );
                    }
                }
            }
        }
    }

    /// The slot layout a raw dmabuf session encodes: YUV444 for a 4:4:4 session, NVENC's
    /// packed 10-bit for an HDR capture, NV12 or P010 for a producer's own (copied, not
    /// converted), else NV12 (`PUNKTFUNK_NV12`) or packed ARGB. A 10-bit SDR session keeps
    /// packed ARGB: NVENC widens only packed RGB to 10 bits.
    fn raw_buffer_format(&self, fmt: pf_frame::PixelFormat) -> nv::NV_ENC_BUFFER_FORMAT {
        use nv::NV_ENC_BUFFER_FORMAT as F;
        if self.s.chroma_444 {
            return F::NV_ENC_BUFFER_FORMAT_YUV444;
        }
        match fmt {
            pf_frame::PixelFormat::X2Rgb10 => F::NV_ENC_BUFFER_FORMAT_ARGB10,
            pf_frame::PixelFormat::X2Bgr10 => F::NV_ENC_BUFFER_FORMAT_ABGR10,
            pf_frame::PixelFormat::Nv12 => F::NV_ENC_BUFFER_FORMAT_NV12,
            pf_frame::PixelFormat::P010 => F::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
            _ if pf_zerocopy::nv12_enabled() && self.depth_asked < 10 => {
                F::NV_ENC_BUFFER_FORMAT_NV12
            }
            _ => F::NV_ENC_BUFFER_FORMAT_ARGB,
        }
    }

    /// The fused convert: the worker writes `d`, cursor included, into ring slot `slot` — or
    /// into the reframe staging slot, which the Lanczos pass then scales into the ring. One
    /// GPU pass, no copy. A failure marks this capture for rebuild on its safe offer.
    /// The raw lane: the held dmabuf goes through the worker's fused pass into the slot (or
    /// the staging slot of a reframing session). `ordered`: the copy stream carries the
    /// hand-off to NVENC; otherwise the CPU waits it here.
    fn convert_raw(
        &mut self,
        captured: &CapturedFrame,
        d: &DmabufFrame,
        slot: usize,
        ordered: bool,
    ) -> Result<()> {
        let fmt = slot_fmt_of(self.s.buffer_fmt);
        let SlotSurface::Vk(dst) = &self.ring[slot].surface else {
            // A ring rebuilt on CUDA surfaces: the capture must fall back to the import path.
            super::vk_util::reject_dmabuf(d, "no Vulkan input slots");
            bail!("NVENC (Linux): a raw dmabuf submit needs Vulkan input slots");
        };
        let dst = *dst;
        let (target, src_size) = match &self.reframe {
            Some(r) => (r.staging[slot], r.src),
            None => (dst, (self.s.width, self.s.height)),
        };
        // A repeat of the frame just converted (the source produced nothing new) is cloned
        // from its slot: one copy, no second worker round trip. The pass bakes the pointer in,
        // so a cursor that moved over a still source is a different picture and must convert.
        // A producer NV12 is copied fresh instead: the host blends its pointer after the copy,
        // and a clone would carry the last blend into a second one.
        let mark = LastRaw {
            pts_ns: captured.pts_ns,
            fd: d.fd.as_raw_fd(),
            slot,
            cursor: cursor_mark(captured),
        };
        if let Some(last) = self
            .last_raw
            .filter(|_| chroma_plane(captured, d).is_none())
        {
            if last.pts_ns == mark.pts_ns
                && last.fd == mark.fd
                && last.cursor == mark.cursor
                && last.slot != slot
                && last.slot < self.ring.len()
            {
                let rows = fmt.rows(self.s.height) as usize;
                let (src, dst_s) = (&self.ring[last.slot].surface, &self.ring[slot].surface);
                // SAFETY: both are this session's ring slots, live with the encoder and laid out
                // for `fmt`; `submit_device` made the context current. An unsynced clone is
                // stream-ordered ahead of the encode that reads it.
                unsafe {
                    cuda::copy_surface_to_surface(
                        src.ptr(),
                        dst_s.ptr(),
                        dst_s.pitch(),
                        rows,
                        !ordered,
                    )
                }
                .context("NVENC (Linux): clone the repeat slot")?;
                self.last_raw = Some(mark);
                return Ok(());
            }
        }
        let value = match self.convert_raw_inner(captured, d, fmt, target, src_size) {
            Ok(v) => {
                d.health.note_raw_import_ok();
                v
            }
            Err(e) => {
                super::vk_util::reject_dmabuf(d, "nvenc convert");
                return Err(e).context("NVENC (Linux): fused convert");
            }
        };
        // A worker that died after replying may never signal the value we are about to wait on,
        // and a CUDA external-semaphore wait has no timeout — refuse the frame while the failure
        // is still an error rather than a hang.
        if self.worker.as_ref().is_none_or(|w| w.dead()) {
            super::vk_util::reject_dmabuf(d, "convert worker died mid-pass");
            bail!("NVENC (Linux): the convert worker died before its pass could be waited on");
        }
        // The pass lands on the GPU; its value gates the copy stream, which is NVENC's input
        // stream. An unordered submit, or a reframe reading the staging slot on another queue,
        // waits here instead.
        self.convert_sem
            .as_ref()
            .ok_or_else(|| anyhow!("convert timeline not imported"))?
            .wait(value)
            .context("NVENC (Linux): wait the fused pass")?;
        self.convert_waited = value;
        if !ordered || self.reframe.is_some() {
            // Bounded: the value came from another process, so a lost signal must cost this
            // frame, not the encode thread. Generous against a slow 4K pass under load. A
            // stall retires the worker and frees the stream for the next frame.
            if let Err(e) = cuda::copy_stream_sync_deadline(std::time::Duration::from_secs(2)) {
                super::vk_util::reject_dmabuf(d, "fused pass stalled");
                self.release_convert_sem();
                return Err(e).context("NVENC (Linux): sync the fused pass");
            }
        }
        if let Some(r) = &self.reframe {
            let (crop, out) = (r.crop, r.out);
            self.vk_blend
                .as_mut()
                .expect("a Vk ring implies the slot device")
                .reframe(&target, &dst, fmt, crop, out)
                .context("NVENC (Linux): reframe")?;
        }
        self.last_raw = Some(mark);
        Ok(())
    }

    fn convert_raw_inner(
        &mut self,
        captured: &CapturedFrame,
        d: &DmabufFrame,
        fmt: SlotFormat,
        target: VkSlotRef,
        src_size: (u32, u32),
    ) -> Result<u64> {
        if self.worker.as_ref().is_none_or(|w| w.dead()) {
            // A corpse, not a first spawn: its death is the lane's to count.
            if self.worker.take().is_some() {
                super::vk_util::reject_dmabuf(d, "convert worker died");
            }
            // The timeline and the slot registrations belonged to that process.
            self.worker_slots.clear();
            self.worker_cursor_serial = u64::MAX;
            self.release_convert_sem();
            let now = std::time::Instant::now();
            if self.worker_retry_at.is_some_and(|at| now < at) {
                bail!("NVENC (Linux): the convert worker is restarting");
            }
            self.worker_retry_at = Some(now + WORKER_RESPAWN_BACKOFF);
            self.worker =
                Some(pf_zerocopy::Importer::new_for_capture().context("spawn the convert worker")?);
        }
        let vk = self
            .vk_blend
            .as_mut()
            .ok_or_else(|| anyhow!("no Vulkan slot device"))?;
        let worker = self.worker.as_mut().expect("ensured above");
        if self.convert_sem.is_none() {
            let fd = worker
                .convert_timeline()
                .context("export the convert timeline")?;
            self.convert_sem = Some(
                cuda::ExternalSemaphore::import_timeline_fd(fd)
                    .context("import the convert timeline")?,
            );
        }
        if !self.worker_slots.contains(&target.id) {
            let (fd, size) = vk.slot_fd(target.id)?;
            worker.register_slot(target.id as u32, fd, size)?;
            self.worker_slots.insert(target.id);
        }
        // The worker copies a producer NV12 without a pass to blend in; the host blends after.
        let cursor = match &captured.cursor {
            _ if chroma_plane(captured, d).is_some() => None,
            Some(ov) if ov.visible && ov.w > 0 && ov.h > 0 && !ov.rgba.is_empty() => {
                if self.worker_cursor_serial != ov.serial {
                    // A PQ frame takes the cursor re-encoded as PQ; sRGB bytes would be read as PQ.
                    let rgba = if pq_input(self.hdr_asked, captured.format) {
                        ov.pq_rgba()
                    } else {
                        ov.rgba.clone()
                    };
                    worker.set_cursor(ov.serial, ov.w, ov.h, &rgba)?;
                    self.worker_cursor_serial = ov.serial;
                }
                Some(pf_zerocopy::CursorRect {
                    x: ov.x,
                    y: ov.y,
                    w: ov.w,
                    h: ov.h,
                })
            }
            _ => None,
        };
        let src = pf_zerocopy::ConvertSrc {
            fd: d.fd.as_raw_fd(),
            fourcc: d.fourcc,
            modifier: d.modifier,
            offset: d.offset,
            stride: d.stride,
            width: captured.width,
            height: captured.height,
            plane1: chroma_plane(captured, d),
        };
        let out = pf_zerocopy::ConvertOut {
            mode: fmt.mode(),
            width: src_size.0,
            height: src_size.1,
            pitch_w: (target.pitch / 4) as u32,
            plane_rows: target.height,
        };
        worker.convert(&src, target.id as u32, &out, cursor)
    }

    /// Device→device copy into the ring slot. `sync` blocks; `!sync` enqueues on the copy
    /// stream (stream-ordered submit — gate in [`Encoder::submit`]).
    fn copy_into_slot(&self, buf: &cuda::DeviceBuffer, slot: usize, sync: bool) -> Result<()> {
        let s = &self.ring[slot].surface;
        // SAFETY: the slot is this session's, laid out for `buffer_fmt` at `s.height()` rows.
        // `submit_device` made the context current; an unsynced copy runs only stream-ordered,
        // with the caller holding `buf` across the poll.
        unsafe { self.copy_into(buf, s.ptr(), s.pitch(), s.height() as u64, sync) }
    }

    /// [`copy_into_slot`](Self::copy_into_slot) into any surface of this session's layout:
    /// planes contiguous under one `pitch`, `hh` luma rows.
    ///
    /// # Safety
    /// The context is current, `base` is a live surface of this session's layout with `hh`
    /// luma rows at `pitch`, `buf` describes a live capture allocation, and with `!sync` `buf`
    /// stays valid until the encode that reads the copy completes.
    unsafe fn copy_into(
        &self,
        buf: &cuda::DeviceBuffer,
        base: cuda::CUdeviceptr,
        pitch: usize,
        hh: u64,
        sync: bool,
    ) -> Result<()> {
        match self.s.buffer_fmt {
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV444 => {
                if !buf.is_yuv444() {
                    bail!("4:4:4 session but the captured buffer is not planar YUV444");
                }
                let planes = [
                    (base, pitch),
                    (base + pitch as u64 * hh, pitch),
                    (base + 2 * pitch as u64 * hh, pitch),
                ];
                cuda::copy_yuv444_to_device(buf, planes, sync)
            }
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12 => {
                if !buf.is_nv12() {
                    bail!("NV12 session but the captured buffer has no chroma plane");
                }
                // NV12: UV at base + pitch*height, same pitch.
                cuda::copy_nv12_to_device(buf, base, pitch, base + pitch as u64 * hh, pitch, sync)
            }
            _ => cuda::copy_device_to_device(buf, base, pitch, sync),
        }
    }

    /// CPU pixels into a reused pitched device buffer: BGRA-order 32-bit as is, RGBA-order
    /// swizzled, 24-bit repacked, NV12 as two planes. Synchronous, so the buffer is free
    /// again on return.
    fn upload_cpu(
        &mut self,
        captured: &CapturedFrame,
        pixels: &[u8],
    ) -> Result<cuda::DeviceBuffer> {
        use pf_frame::PixelFormat as P;
        use std::borrow::Cow;
        let (w, h) = (captured.width, captured.height);
        cuda::make_current().context("cuCtxSetCurrent (CPU upload)")?;
        let nv12 = captured.format == P::Nv12;
        let buf = match self.upload.take() {
            Some(b) if b.width == w && b.height == h && b.is_nv12() == nv12 => b,
            _ if nv12 => cuda::DeviceBuffer::alloc(cuda::PlaneLayout::Nv12, w, h)?,
            _ => cuda::DeviceBuffer::alloc(cuda::PlaneLayout::Packed32, w, h)?,
        };
        let (w, h) = (w as usize, h as usize);
        if nv12 {
            let (uv_ptr, uv_pitch) = buf
                .uv()
                .context("NV12 device buffer without a chroma plane")?;
            // SAFETY: `buf` is a live NV12 allocation of `w`×`h` (checked or allocated above), Y
            // at `pitch` and UV at `uv_pitch` with `h/2` rows; the context is current (above).
            unsafe {
                cuda::write_plane_from_host(buf.ptr, buf.pitch, pixels, w, h)?;
                cuda::write_plane_from_host(uv_ptr, uv_pitch, &pixels[w * h..], w, h / 2)?;
            }
            return Ok(buf);
        }
        let packed: Cow<[u8]> = match captured.format {
            P::Bgra | P::Bgrx | P::X2Rgb10 | P::X2Bgr10 | P::Rgb10a2 => Cow::Borrowed(pixels),
            P::Rgba | P::Rgbx => Cow::Owned(
                pixels
                    .chunks_exact(4)
                    .flat_map(|px| [px[2], px[1], px[0], px[3]])
                    .collect(),
            ),
            P::Bgr => Cow::Owned(
                pixels
                    .chunks_exact(3)
                    .flat_map(|px| [px[0], px[1], px[2], 255])
                    .collect(),
            ),
            P::Rgb => Cow::Owned(
                pixels
                    .chunks_exact(3)
                    .flat_map(|px| [px[2], px[1], px[0], 255])
                    .collect(),
            ),
            other => bail!("Linux direct-NVENC cannot upload a {other:?} CPU frame"),
        };
        // SAFETY: `buf` is a live 4-byte allocation of `w`×`h` at `pitch` (checked or allocated
        // above); the context is current (above).
        unsafe { cuda::write_plane_from_host(buf.ptr, buf.pitch, &packed, w * 4, h)? };
        Ok(buf)
    }

    /// Prepare and submit one device frame: rebuild on a changed input, backpressure, fill the
    /// ring slot, blend the cursor, encode. The pending entry owns a raw source hold until
    /// retrieval completes, including stream-ordered fused conversion.
    fn submit_device(&mut self, captured: &CapturedFrame, src: Source<'_>) -> Result<()> {
        self.maybe_engage_async();
        self.maybe_disengage_async();
        self.ensure_session(captured, src)?;
        // Two-thread backpressure: block on the oldest completion so this slot is free before
        // reuse. Cap-deep instead of 1.
        self.s.wait_below(async_inflight_cap())?;
        let slot = self.s.slot();
        // `PUNKTFUNK_PERF` submit split (~1 line / 2 s at 60 fps). Host `submit_us` folds
        // copy/blend/map/pic; this splits them.
        let sample = pf_host_config::config().perf && self.frames % 120 == 0;
        self.frames += 1;
        let (ordered, cursor_ordered) = self.ordering(captured, slot);
        let t0 = std::time::Instant::now();
        let fused_cursor = self.fill_slot(captured, src, slot, ordered)?;
        let t_copy = t0.elapsed();
        // A fused convert already carries the cursor.
        if let (false, Some(ov)) = (fused_cursor, &captured.cursor) {
            self.blend_cursor(captured, ov, slot, cursor_ordered);
        }
        let t_blend = t0.elapsed() - t_copy;
        let input = EncodeInput {
            reg: self.ring[slot].reg,
            pitch: self.ring[slot].surface.pitch() as u32,
            event: ptr::null_mut(),
            pts_ns: captured.pts_ns,
            hold: source_hold(captured),
        };
        // SAFETY: the session is open (`ensure_session`) and `input.reg` is this slot's
        // registration. The slot was just filled (blocking copy, or IO-stream / timeline ordered
        // before this encode) and is not overwritten until POOL submits later, by which time
        // this encode was polled.
        let times = unsafe { self.s.encode(input) }?;
        if sample {
            tracing::info!(
                copy_us = t_copy.as_micros() as u64,
                blend_us = t_blend.as_micros() as u64,
                map_us = times.map.as_micros() as u64,
                pic_us = times.pic.as_micros() as u64,
                "NVENC submit split (sampled): copy=input D2D copy blend=cursor map=map_input \
                 pic=encode_picture launch"
            );
        }
        Ok(())
    }

    /// Open the session for this capture, first tearing down one whose size or format
    /// (NV12↔YUV444) no longer matches. Depth, HDR and 4:4:4 follow the capture format.
    fn ensure_session(&mut self, captured: &CapturedFrame, src: Source<'_>) -> Result<()> {
        let new_fmt = match src {
            Source::Cuda(b) => buffer_format(b, captured.format),
            Source::Dmabuf(_) => self.raw_buffer_format(captured.format),
        };
        let input = (captured.width, captured.height);
        let inited = self.s.inited();
        let size_changed = inited
            && match &self.reframe {
                Some(r) => r.src != input,
                None => (self.s.width, self.s.height) != input,
            };
        let fmt_changed = inited && self.s.buffer_fmt != new_fmt;
        if inited && (size_changed || fmt_changed) {
            tracing::info!(
                size_changed,
                fmt_changed,
                new = format!("{}x{}", captured.width, captured.height),
                "NVENC (Linux): capture size/format changed — re-initializing session"
            );
            // SAFETY: encode thread, the session open, previous frame already polled — nothing
            // mid-encode. Cached ring/bitstreams/pending belong to this session.
            unsafe { self.teardown() };
        }
        if self.s.inited() {
            return cuda::make_current().context("cuCtxSetCurrent (encode thread)");
        }
        if let (Some(_), Source::Dmabuf(d)) = (&self.reframe, src) {
            if new_fmt == nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT {
                // This latches the whole raw lane for the identity; a P010 reframe pass keeps it.
                super::vk_util::reject_dmabuf(d, "a reframing session has no P010 path");
                bail!("NVENC (Linux): a reframing session cannot take a producer's P010");
            }
        }
        (self.s.width, self.s.height) = input;
        if let Some(r) = &mut self.reframe {
            let [x, y, w, h] = r.crop;
            ensure!(
                x + w <= input.0 && y + h <= input.1,
                "NVENC (Linux): a {}x{} capture does not hold the {w}x{h} crop at {x},{y}",
                input.0,
                input.1
            );
            r.src = input;
            (self.s.width, self.s.height) = r.out;
        }
        self.s.buffer_fmt = new_fmt;
        // Depth from the capture format: a packed-RGB 8-bit surface reaches a 10-bit SDR
        // stream; a planar 8-bit one (NV12/YUV444) stays 8-bit rather than failing the
        // 10-bit session. Colour is the session's.
        let (depth, hdr) = depth_and_hdr(new_fmt, self.depth_asked, self.hdr_asked);
        if self.depth_asked >= 10 && depth < 10 {
            tracing::warn!(
                format = ?captured.format,
                "Linux direct-NVENC: 10-bit negotiated but the capture is a planar 8-bit \
                 surface NVENC can't feed a 10-bit session — encoding 8-bit SDR (the stream \
                 is labelled to match)"
            );
        }
        (self.s.bit_depth, self.s.hdr) = (depth, hdr);
        // FREXT only on genuine YUV444; NV12/RGB cannot reconstruct full chroma.
        self.s.chroma_444 = self.s.chroma_444
            && match src {
                Source::Cuda(b) => b.is_yuv444(),
                Source::Dmabuf(_) => true,
            };
        // `init_session` publishes the handle before later fallible steps. A failure leaves
        // a live session with `inited` false; the next submit would skip teardown and leak.
        // `teardown` keys off the handle, so it cleans this half-built state.
        if let Err(e) = self.init_session() {
            // SAFETY: encode thread owns the session; failed init left nothing mid-encode.
            unsafe { self.teardown() };
            return Err(e);
        }
        Ok(())
    }

    /// Whether this frame's copy and cursor blend may enqueue ahead of the encode without a CPU
    /// sync: `(ordered, cursor_ordered)`. Only while `pending` is empty — the blocking `poll`
    /// drained the prior encode, so the reused slot was fully read and the caller still holds
    /// this payload across `poll`. Pipelined / two-thread falls back to a blocking copy so an
    /// early-recycled source cannot be read late.
    fn ordering(&self, captured: &CapturedFrame, slot: usize) -> (bool, bool) {
        let base_ordered = self.stream_ordered
            && !self.s.retrieving()
            && self.s.pending().is_empty()
            && self.reframe.is_none();
        // Cursor stays stream-ordered when the blend can wait a CUDA-held timeline semaphore.
        // Otherwise the fence/CPU path sits between copy and encode.
        let cursor_ordered = base_ordered
            && captured.cursor.is_some()
            && matches!(self.ring[slot].surface, SlotSurface::Vk(_))
            && self.vk_blend.as_ref().is_some_and(|vk| vk.ordered_ready());
        let ordered = base_ordered && (captured.cursor.is_none() || cursor_ordered);
        (ordered, cursor_ordered)
    }

    /// Fill ring slot `slot` with this frame. A held RGB dmabuf goes through the worker's fused
    /// pass, cursor included (`true`). A producer NV12 is copied by the worker, and a CUDA buffer
    /// is copied in and reframed when the session scales; both leave the cursor to
    /// [`Self::blend_cursor`] (`false`).
    fn fill_slot(
        &mut self,
        captured: &CapturedFrame,
        src: Source<'_>,
        slot: usize,
        ordered: bool,
    ) -> Result<bool> {
        let buf = match src {
            Source::Dmabuf(d) => {
                self.convert_raw(captured, d, slot, ordered)?;
                return Ok(chroma_plane(captured, d).is_none());
            }
            Source::Cuda(buf) => buf,
        };
        match &self.reframe {
            // A reframe reads the staging slot on the Vulkan queue: the copy must have landed.
            Some(r) => {
                let (stage, fmt) = (r.staging[slot], slot_fmt_of(self.s.buffer_fmt));
                // SAFETY: the staging slot is this session's, sized for its layout; the
                // context is current (`ensure_session`), and the copy is synchronous.
                unsafe {
                    self.copy_into(buf, stage.ptr, stage.pitch, u64::from(stage.height), true)
                }?;
                let (crop, out) = (r.crop, r.out);
                let (vk, SlotSurface::Vk(dst)) = (self.vk_blend.as_mut(), &self.ring[slot].surface)
                else {
                    bail!("NVENC (Linux): a reframing session without Vulkan slots");
                };
                vk.expect("a Vk ring implies the slot device")
                    .reframe(&stage, dst, fmt, crop, out)
                    .context("NVENC (Linux): reframe")?;
            }
            None => self.copy_into_slot(buf, slot, !ordered)?,
        }
        Ok(false)
    }

    /// Blend the cursor into this slot's owned surface (cursor rect, never the compositor
    /// dmabuf). Ordered: copy/dispatch/encode on-device via timeline. Else CUDA copy then
    /// fence-waited dispatch, then encode. Failure drops the cursor, never the frame.
    fn blend_cursor(
        &mut self,
        captured: &CapturedFrame,
        ov: &pf_frame::CursorOverlay,
        slot: usize,
        cursor_ordered: bool,
    ) {
        // A reframed picture takes the pointer scaled and moved with it.
        let (cw, ch, cx, cy) = match &mut self.reframe {
            Some(r) => {
                let [x, y, w, _] = r.crop;
                let (ow, oh) = r.out;
                let scale = |v: i64, n: u32| (v * i64::from(n)).div_euclid(i64::from(w));
                let h = r.crop[3];
                let scale_y = |v: i64| (v * i64::from(oh)).div_euclid(i64::from(h));
                if r.cursor.as_ref().map(|c| c.0) != Some(ov.serial) {
                    let tw = (u64::from(ov.w) * u64::from(ow) / u64::from(w)).max(1) as u32;
                    let th = (u64::from(ov.h) * u64::from(oh) / u64::from(h)).max(1) as u32;
                    let src = if pq_input(self.hdr_asked, captured.format) {
                        ov.pq_rgba()
                    } else {
                        ov.rgba.clone()
                    };
                    let scaled = shrink_rgba(&src, ov.w, ov.h, tw, th);
                    r.cursor = Some((ov.serial, scaled, tw, th));
                }
                let (_, _, tw, th) = r.cursor.as_ref().expect("set above");
                (
                    *tw,
                    *th,
                    scale(i64::from(ov.x) - i64::from(x), ow) as i32,
                    scale_y(i64::from(ov.y) - i64::from(y)) as i32,
                )
            }
            None => (ov.w, ov.h, ov.x, ov.y),
        };
        let (Some(vk), SlotSurface::Vk(vref)) = (self.vk_blend.as_mut(), &self.ring[slot].surface)
        else {
            if !self.cursor_blend_warned {
                self.cursor_blend_warned = true;
                tracing::warn!(
                    blend_wanted = self.blend_wanted,
                    "NVENC (Linux): cursor overlay present but no Vulkan blend (bring-up failed, \
                     or a non-blend session unexpectedly carried an overlay) — cursor not \
                     composited"
                );
            }
            return;
        };
        if self.cursor_serial != ov.serial {
            // Quiesces in-flight ordered blends before touching staging.
            let pq = pq_input(self.hdr_asked, captured.format).then(|| ov.pq_rgba());
            let bitmap = match &self.reframe {
                Some(r) => r.cursor.as_ref().map_or(&[][..], |c| c.1.as_slice()),
                None => pq.as_deref().unwrap_or(&ov.rgba).as_slice(),
            };
            vk.upload_cursor(bitmap, cw, ch);
            self.cursor_serial = ov.serial;
        }
        // `surfW` = content width. Pixels past content land in cropped padding.
        let (fmt, width) = (slot_fmt_of(self.s.buffer_fmt), self.s.width);
        let r = if cursor_ordered {
            vk.blend_ref_ordered(vref, fmt, width, cw, ch, cx, cy)
        } else {
            vk.blend_ref(vref, fmt, width, cw, ch, cx, cy)
        };
        if let Err(e) = r {
            if !self.cursor_blend_warned {
                self.cursor_blend_warned = true;
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "NVENC (Linux): cursor blend dispatch failed — cursor not composited"
                );
            }
        } else {
            self.cursor_blend_warned = false;
        }
    }
}

impl Encoder for NvencCudaEncoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let uploaded = match &captured.payload {
            FramePayload::Cpu(pixels) => Some(self.upload_cpu(captured, pixels)?),
            _ => None,
        };
        let src = match (&captured.payload, &uploaded) {
            (FramePayload::Cuda(b), _) => Source::Cuda(b),
            (_, Some(b)) => Source::Cuda(b),
            (FramePayload::Dmabuf(d), _) if self.raw_wanted => Source::Dmabuf(d),
            (FramePayload::Dmabuf(d), _) => {
                super::vk_util::reject_dmabuf(d, "this session takes no raw dmabuf");
                bail!("Linux direct-NVENC needs a CUDA or CPU frame; got a dmabuf")
            }
            _ => bail!("Linux direct-NVENC needs a CUDA or CPU frame"),
        };
        let result = self.submit_device(captured, src);
        if let Some(b) = uploaded {
            self.upload = Some(b);
        }
        result
    }
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.s.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn request_keyframe(&mut self) {
        self.s.force_kf = true;
    }

    fn set_pipelined(&mut self, on: bool) -> bool {
        if !on {
            // Latch de-escalation; switch at the next drained point. Caller re-queries until
            // inactive.
            if async_retrieve_requested() {
                // Operator pinned async on — do not undo it.
                return self.want_async || self.s.retrieving();
            }
            if self.want_async || self.s.retrieving() {
                self.want_async = false;
                self.want_sync = true;
                self.maybe_disengage_async();
            }
            return self.want_async || self.s.retrieving();
        }
        if async_retrieve_vetoed() {
            return false; // `PUNKTFUNK_NVENC_ASYNC=0`
        }
        self.want_sync = false; // latest intent wins
        if !self.want_async && !self.s.retrieving() {
            self.want_async = true;
            self.maybe_engage_async();
        }
        true
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            blends_cursor: true,
            supports_rfi: self.s.rfi_supported,
            chroma_444: self.s.chroma_444,
            intra_refresh: false,
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
            // `set_input_crop` refuses when the Vulkan slot device will not come up.
            downscales_input: true,
            crops_input: true,
        }
    }

    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        ensure!(
            !self.s.inited(),
            "NVENC (Linux): the reframe is set before the first frame"
        );
        let [_, _, w, h] = rect;
        let out = (self.s.width, self.s.height);
        ensure!(
            w >= out.0 && h >= out.1,
            "NVENC (Linux): a {w}x{h} crop would upscale into the {}x{} session",
            out.0,
            out.1
        );
        if self.vk_blend.is_none() {
            cuda::make_current().context("cuCtxSetCurrent (reframe bring-up)")?;
            self.vk_blend = Some(VkSlotBlend::new().context("Vulkan slot device for the reframe")?);
            self.cursor_tried = true;
        }
        self.reframe = Some(NvReframe {
            crop: rect,
            out,
            src: (0, 0),
            staging: Vec::new(),
            cursor: None,
        });
        tracing::info!(crop = ?rect, ?out, "NVENC (Linux): cropping and scaling on the Vulkan slot queue");
        Ok(())
    }

    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.s.hdr_meta = meta;
    }

    fn distrust_references(&mut self) {
        self.s.distrusted = true;
    }

    fn set_reference_floor(&mut self, acked: Option<crate::Acked>) {
        self.s.reference_floor = acked;
    }

    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        self.s.invalidate_ref_frames(first, last)
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        self.s.poll()
    }

    fn supports_chunked_poll(&self) -> bool {
        self.s.supports_chunked_poll()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        self.s.poll_chunk()
    }

    fn reset(&mut self) -> bool {
        // SAFETY: encode thread, between submit/poll. `teardown` no-ops a closed session.
        unsafe { self.teardown() };
        self.s.force_kf = true;
        true
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        self.s.reconfigure_bitrate(bps)
    }

    fn set_send_spread_us(&mut self, us: u32) {
        self.s.send_spread_us = us;
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        // Post-clamp target: open search and reconfigure cache clamp both write it.
        Some(self.s.bitrate_bps)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + `frameIntervalP=1`: each submit yields its AU.
    }
}

impl Drop for NvencCudaEncoder {
    fn drop(&mut self) {
        // SAFETY: exclusive owner on the encode thread. `teardown` no-ops a null session;
        // otherwise cached resources belong to that live session. Once.
        unsafe { self.teardown() };
    }
}

#[cfg(test)]
#[path = "nvenc_cuda_tests.rs"]
mod tests;
