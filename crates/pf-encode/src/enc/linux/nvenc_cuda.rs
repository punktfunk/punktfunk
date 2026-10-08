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
        // it down). Register takes the live session and versioned locals that outlive the sync
        // call. `set_io_cuda_streams` points at a boxed `CUstream` freed once: `teardown` after
        // destroy, or `Box::from_raw` on the rejection path. Encode thread only.
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

            // Ring: register once, map per submit. Prefer Vulkan-imported slots so the cursor
            // blend writes the bytes NVENC encodes; any failure falls back to pitched CUDA.
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
            let (width, height, buffer_fmt) = (self.s.width, self.s.height, self.s.buffer_fmt);
            let slot_fmt = slot_fmt_of(buffer_fmt);
            // Full Vulkan ring, else full CUDA. Never mixed (flickering cursor) or short.
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
            // Bind IO streams to this thread's copy stream. Same stream both ways so later
            // copies into a reused slot wait for the encode. Sync retrieve only: two-thread
            // mode may recycle the captured buffer after `submit` while the stream still
            // holds the copy.
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
mod tests {
    use super::*;
    use crate::max_forced_split_mode;
    use crate::nvenc_core::{cached_split_verdict, slice_offsets_len, BitstreamLock};
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
    use pf_zerocopy::cuda::{DeviceBuffer, PlaneLayout};

    #[test]
    fn raw_source_hold_is_cloned_for_the_pending_encode() {
        let hold: pf_frame::FrameHold = std::sync::Arc::new(());
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: 64,
            height: 64,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Dmabuf(pf_frame::DmabufFrame {
                fd: std::fs::File::open("/dev/null").unwrap().into(),
                fourcc: u32::from_le_bytes(*b"XR24"),
                modifier: 0,
                plane1: None,
                offset: 0,
                stride: 256,
                hold: Some(hold.clone()),
                health: pf_zerocopy::zero_copy_health(0x5001),
                rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            cursor: None,
        };
        let pending = source_hold(&frame).expect("raw frame hold");
        assert_eq!(std::sync::Arc::strong_count(&hold), 3);
        drop(pending);
        assert_eq!(std::sync::Arc::strong_count(&hold), 2);
    }

    /// Env helper for ignored hardware tests. Run `--test-threads=1` — they mutate process env.
    fn set_env(key: &str, val: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: `--test-threads=1` hardware tests only — no concurrent env access.
        unsafe { std::env::set_var(key, val) };
    }

    /// Same single-threaded contract as [`set_env`].
    fn remove_env(key: &str) {
        // SAFETY: as `set_env` — single-threaded, no concurrent env access.
        unsafe { std::env::remove_var(key) };
    }

    /// SDR-10 rides NVENC's 8→10, which takes packed RGB (`ARGB`) only. A planar 8-bit surface
    /// (NV12/YUV444) stays 8-bit — feeding it a 10-bit session fails `register_resource`, the
    /// real-capture regression this guards. Packed 10-bit input is HDR (BT.2020 PQ) regardless.
    #[test]
    fn depth_and_hdr_needs_packed_rgb_for_ten_bit() {
        use nv::NV_ENC_BUFFER_FORMAT as F;
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ARGB, 10, false),
            (10, false)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ARGB, 8, false),
            (8, false)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_NV12, 10, false),
            (8, false)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_YUV444, 10, false),
            (8, false)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ARGB10, 8, true),
            (10, true)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ABGR10, 10, true),
            (10, true)
        );
        // A 10-bit input carries the session's colour; an 8-bit stream is never HDR.
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_ABGR10, 10, false),
            (10, false)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_YUV420_10BIT, 10, false),
            (10, false)
        );
        assert_eq!(
            depth_and_hdr(F::NV_ENC_BUFFER_FORMAT_NV12, 10, true),
            (8, false)
        );
    }

    #[test]
    fn ten_bit_rgb_maps_to_the_matching_nvenc_format_and_blend_mode() {
        use nv::NV_ENC_BUFFER_FORMAT as F;
        // `x:R:G:B` = ARGB10; `x:B:G:R` = ABGR10.
        assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB10));
        assert!(is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ABGR10));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_ARGB));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_NV12));
        assert!(!is_ten_bit_input(F::NV_ENC_BUFFER_FORMAT_YUV444));
        // Blend mode must unpack this channel order; a swap tints the pointer.
        assert_eq!(
            slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ARGB10),
            SlotFormat::X2Rgb10
        );
        assert_eq!(
            slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ABGR10),
            SlotFormat::X2Bgr10
        );
        assert_eq!(slot_fmt_of(F::NV_ENC_BUFFER_FORMAT_ARGB), SlotFormat::Argb);
    }

    /// What `resolve_split_mode` actually reads (`query_caps` latch).
    fn self_engines(enc: &NvencCudaEncoder) -> u32 {
        enc.s.encoder_engines
    }

    /// NV12 with real entropy. Driver-zeroed VRAM under CBR emits ~300 B/AU against an 833 KB
    /// quota, so timings measure only pixel-proportional cost. `block=1` is incompressible
    /// (RC overshoots); larger `block` is the only way to reach the low bits/frame end.
    fn noise_nv12_frame(w: u32, h: u32, i: u32, block: usize) -> CapturedFrame {
        let buf = DeviceBuffer::alloc(PlaneLayout::Nv12, w, h).expect("alloc NV12 device buffer");
        let (uv_ptr, uv_pitch) = buf.uv().expect("NV12 buffer has a UV plane");
        let mut st = 0x2545_F491_4F6C_DD1Du64 ^ ((i as u64 + 1) << 32);
        let mut next = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        let b = block.max(1);
        let mut plane = |pw: usize, ph: usize| -> Vec<u8> {
            let bw = pw.div_ceil(b);
            let cells: Vec<u8> = (0..(bw * ph.div_ceil(b)))
                .map(|_| (next() >> 24) as u8)
                .collect();
            let mut out = Vec::with_capacity(pw * ph);
            for y in 0..ph {
                let row = y / b * bw;
                for x in 0..pw {
                    out.push(cells[row + x / b]);
                }
            }
            out
        };
        let y = plane(w as usize, h as usize);
        let uv = plane(w as usize, h as usize / 2);
        // SAFETY: `buf` is the live `w`×`h` NV12 allocation above, UV at `uv_pitch`; the caller
        // made the shared context current.
        unsafe {
            pf_zerocopy::cuda::write_plane_from_host(
                buf.ptr, buf.pitch, &y, w as usize, h as usize,
            )
            .expect("upload Y plane");
            pf_zerocopy::cuda::write_plane_from_host(
                uv_ptr,
                uv_pitch,
                &uv,
                w as usize,
                h as usize / 2,
            )
            .expect("upload UV plane");
        }
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Nv12,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    fn nv12_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        // Uninit VRAM: session/RFI machinery, not picture fidelity.
        let buf = DeviceBuffer::alloc(PlaneLayout::Nv12, w, h).expect("alloc NV12 device buffer");
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::Nv12,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    /// Hardware: GUID probe. Every NVENC encodes H.264 — `h264 = false` means enumeration is
    /// broken. Asserted on the uncached fn (the cache would make stability vacuous).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on an NVIDIA box"]
    fn nvenc_codec_probe_reports_real_gpu_support() {
        let probed = probe_support_uncached();
        let caps = probed.codecs;
        eprintln!(
            "NVENC probe: h264={} h265={} av1={} hevc_444={}",
            caps.h264, caps.h265, caps.av1, probed.hevc_444
        );
        assert!(
            caps.h264,
            "every NVENC generation encodes H.264 — a false here means the GUID enumeration \
             failed, which would narrow the host's codec advertisement"
        );
        assert!(
            !probed.hevc_444 || caps.h265,
            "a 4:4:4-capable HEVC that is not in the GUID list is contradictory"
        );
        let again = probe_support_uncached();
        assert_eq!(
            (caps.h264, caps.h265, caps.av1, probed.hevc_444),
            (
                again.codecs.h264,
                again.codecs.h265,
                again.codecs.av1,
                again.hevc_444
            ),
            "the probe must be stable — it is cached once and drives every later negotiation"
        );
    }

    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_smoke_rfi_anchor() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");

        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        // Warm-up: 8 frames, wire indices 0..7.
        let mut aus = 0usize;
        let mut first_key = false;
        for i in 0..8u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                if aus == 0 {
                    first_key = au.keyframe;
                }
                aus += 1;
            }
        }
        assert!(aus > 0, "no AUs produced");
        assert!(
            first_key,
            "first AU must be a keyframe (session opening IDR)"
        );
        assert!(enc.caps().supports_rfi, "RTX NVENC must advertise RFI");

        // In-DPB range (RFI_DPB=5 ⇒ 3..=7 live). Must be real RFI, not an IDR fallback.
        assert!(
            enc.invalidate_ref_frames(5, 6),
            "invalidate_ref_frames should succeed for an in-DPB range"
        );

        // Re-anchor AU: `recovery_anchor`, not a forced IDR.
        let frame = nv12_frame(W, H, 8);
        enc.submit_indexed(&frame, 8).expect("submit post-RFI");
        let mut saw_anchor = false;
        let mut anchor_was_keyframe = false;
        while let Some(au) = enc.poll().expect("poll") {
            if au.recovery_anchor {
                saw_anchor = true;
                anchor_was_keyframe = au.keyframe;
            }
        }
        assert!(
            saw_anchor,
            "the post-RFI AU must carry recovery_anchor (the F2 fix)"
        );
        assert!(
            !anchor_was_keyframe,
            "RFI re-anchor must be a P-frame, not an IDR"
        );
        enc.flush().ok();
        println!(
            "nvenc_cuda smoke: {aus} AUs, RFI succeeded, recovery-anchor tagged on the P-frame"
        );
    }

    /// The Windows `nvenc_wave_soak` on the CUDA session, HEVC only (AV1 never waves): many
    /// waves, each answering a frame lost two ahead of its start (`PUNKTFUNK_NVENC_IR_ALWAYS=1`
    /// makes every RFI a wave), the same `PF_WAVE_*` knobs, frames uploaded from host memory. The
    /// full stream and the view that lost those frames land in `PUNKTFUNK_SMOKE_DIR` with `.idx`
    /// sidecars, for `gpu_parity`'s field hashers.
    ///
    /// `cargo test -p pf-encode --features nvenc --release nvenc_cuda_wave_soak -- --ignored --nocapture`
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on an NVIDIA Linux box"]
    fn nvenc_cuda_wave_soak() {
        let shape = std::env::var("PF_WAVE_SMOKE").unwrap_or_else(|_| "256x256:8:120:1".into());
        let count = |k: &str, d: usize| {
            std::env::var(k)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d)
        };
        let on = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
        let (waves, gap) = (count("PF_WAVE_SOAK", 12), count("PF_WAVE_GAP", 12));
        let (spoil, idr) = (on("PF_WAVE_SPOIL"), on("PF_WAVE_IDR"));
        let (codec, ext) = (Codec::H265, "h265");
        assert!(
            on("PUNKTFUNK_NVENC_IR_ALWAYS"),
            "PUNKTFUNK_NVENC_IR_ALWAYS=1 makes every ask a wave"
        );
        let mut parts = shape.split(':');
        let (w, h) = parts
            .next()
            .and_then(|s| s.split_once('x'))
            .map(|(w, h)| (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap()))
            .expect("PF_WAVE_SMOKE=WxH[:bits[:fps[:mbps]]]");
        let ten_bit = parts.next().is_some_and(|b| b == "10");
        let fps: u32 = parts.next().map_or(60, |f| f.parse().unwrap());
        let mbps: u64 = parts
            .next()
            .map_or(if w >= 1920 { 86 } else { 10 }, |m| m.parse().unwrap());
        // 10-bit feeds XBGR2101010: the Windows soak's R10G10B10A2 bytes.
        let format = if ten_bit {
            PixelFormat::X2Bgr10
        } else {
            PixelFormat::Bgra
        };
        let frame_at = |i: usize| {
            let mut px = crate::smoke_pattern::scroll_pattern(w as usize, h as usize, i);
            if ten_bit {
                for p in px.chunks_exact_mut(4) {
                    let (b, g, r) = (u32::from(p[0]), u32::from(p[1]), u32::from(p[2]));
                    let v = (r << 2) | ((g << 2) << 10) | ((b << 2) << 20) | (3 << 30);
                    p.copy_from_slice(&v.to_le_bytes());
                }
            }
            CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i as u64 * 1_000_000_000 / u64::from(fps),
                format,
                payload: FramePayload::Cpu(px),
                cursor: None,
            }
        };
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            codec,
            format,
            w,
            h,
            fps,
            mbps * 1_000_000,
            true,
            if ten_bit { 10 } else { 8 },
            format.is_ten_bit(),
            ChromaFormat::Yuv420,
            false,
            1,
        )
        .expect("open NVENC CUDA session");
        let cycle = enc.s.wave_cycle() as usize;
        assert!(cycle >= 2, "the wave is on");
        assert!(
            cycle > 3 || !spoil,
            "the spoiling loss lands inside the sweep"
        );
        // Wave k starts at 3 + k * period; its lost frame is two before that. A spoiled
        // wave is followed by the queued one, so its period holds two cycles.
        let period = if spoil { 2 * cycle + gap } else { cycle + gap };
        let last = 3 + waves * period;
        let (mut lost, mut starts, mut closes, mut idrs) = (vec![], vec![], vec![], vec![]);
        let mut aus = Vec::new();
        for i in 0..=last {
            let offset = (i >= 3 && (i - 3) / period < waves).then(|| (i - 3) % period);
            match offset {
                Some(0) => {
                    let l = (i - 2) as i64;
                    assert!(enc.invalidate_ref_frames(l, l), "the always-wave answers");
                    assert_eq!(enc.s.wave.map(|w| w.index), Some(0), "a fresh wave");
                    lost.push(i - 2);
                    starts.push(i);
                    if !spoil && !idr {
                        closes.push(i + cycle - 1);
                    }
                }
                Some(2) if idr => {
                    enc.request_keyframe();
                    idrs.push(i);
                }
                Some(3) if spoil => {
                    let l = (i - 1) as i64;
                    assert!(enc.invalidate_ref_frames(l, l), "a loss inside the sweep");
                    assert!(
                        enc.s.wave_spoiled && enc.s.wave_queued,
                        "spoiled, one queued"
                    );
                    lost.push(i - 1);
                    starts.push(i - 3 + cycle);
                    closes.push(i - 3 + 2 * cycle - 1);
                }
                _ => {}
            }
            enc.submit_indexed(&frame_at(i), i as u32).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus.push(au);
            }
        }
        enc.flush().ok();
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), last + 1, "one AU per submitted frame");
        for (i, au) in aus.iter().enumerate() {
            assert_eq!(
                au.keyframe,
                i == 0 || idrs.contains(&i),
                "AU {i}: IDRs only where forced"
            );
            assert_eq!(
                au.recovery_point,
                starts.contains(&i) || closes.contains(&i),
                "AU {i}: marks on every start and unspoiled close"
            );
            assert_eq!(
                au.recovery_close,
                closes.contains(&i),
                "AU {i}: the close bit on every unspoiled close"
            );
        }
        let full: Vec<&[u8]> = aus.iter().map(|a| a.data.as_slice()).collect();
        let view: Vec<&[u8]> = aus
            .iter()
            .enumerate()
            .filter(|(i, _)| !lost.contains(i))
            .map(|(_, a)| a.data.as_slice())
            .collect();
        let dir = std::env::var("PUNKTFUNK_SMOKE_DIR").unwrap_or_else(|_| ".".into());
        let capture = crate::smoke_pattern::write_capture;
        capture(&format!("{dir}/nvenc-cuda-wave.{ext}"), &full).expect("write");
        capture(&format!("{dir}/nvenc-cuda-wave-dropS.{ext}"), &view).expect("write");
        let csv = |v: &[usize]| {
            v.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        println!(
            "nvenc_cuda_wave_soak: {w}x{h} {}-bit {fps} fps {mbps} Mbps cycle={cycle} gap={gap} \
             waves={waves} aus={} lost={} closes={} spoil={spoil} idrs={}",
            if ten_bit { 10 } else { 8 },
            aus.len(),
            csv(&lost),
            csv(&closes),
            csv(&idrs)
        );
    }

    /// Packed `X2Rgb10` (NVENC `ARGB10`, no host CSC). Uninit VRAM: session machinery, not
    /// picture fidelity.
    fn rgb10_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        let buf = DeviceBuffer::alloc(PlaneLayout::Packed32, w, h)
            .expect("alloc packed RGB device buffer");
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: i as u64 * 16_666_667,
            format: PixelFormat::X2Rgb10,
            payload: FramePayload::Cuda(buf),
            cursor: None,
        }
    }

    /// Hardware: packed 10-bit → `ARGB10`. Depth follows the input and the session's HDR
    /// verdict rides on it — that pair selects Main10 / BT.2020 PQ.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver with 10-bit encode"]
    fn nvenc_cuda_hdr10_packed_rgb() {
        for codec in [Codec::H265, Codec::Av1] {
            const W: u32 = 1280;
            const H: u32 = 720;
            pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
            let mut enc = NvencCudaEncoder::open(
                codec,
                PixelFormat::X2Rgb10,
                W,
                H,
                60,
                20_000_000,
                true,
                10,
                true,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");

            let mut aus = 0usize;
            let mut first_key = false;
            let mut stream: Vec<u8> = Vec::new();
            for i in 0..4u32 {
                enc.submit_indexed(&rgb10_frame(W, H, i), i)
                    .expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    if aus == 0 {
                        first_key = au.keyframe;
                    }
                    assert!(!au.data.is_empty(), "empty AU");
                    stream.extend_from_slice(&au.data);
                    aus += 1;
                }
            }
            enc.flush().ok();
            // Dump for out-of-band ffprobe. In-tree we only see the encoder's own config.
            if let Ok(home) = std::env::var("HOME") {
                let ext = if codec == Codec::Av1 { "obu" } else { "h265" };
                let path = format!("{home}/nvenc-hdr10.{ext}");
                if std::fs::write(&path, &stream).is_ok() {
                    println!(
                        "nvenc_cuda HDR10 {codec:?}: wrote {path} ({} bytes)",
                        stream.len()
                    );
                }
            }
            assert!(aus > 0, "{codec:?}: no AUs produced");
            assert!(first_key, "{codec:?}: first AU must be the session IDR");
            // Depth + HDR came from the input format.
            assert_eq!(enc.s.bit_depth, 10, "{codec:?}: must have derived 10-bit");
            assert!(
                enc.s.hdr,
                "{codec:?}: must have derived HDR from the PQ format"
            );
            assert_eq!(
                enc.s.buffer_fmt,
                nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB10,
                "{codec:?}: X2Rgb10 must ingest as ARGB10"
            );
            println!("nvenc_cuda HDR10 {codec:?}: {aus} AUs, ARGB10 in, 10-bit derived");
        }
    }

    #[test]
    fn a_shrunk_cursor_keeps_its_colour_at_a_soft_edge() {
        // 4x2: an opaque red half and a transparent half, halved to 2x1.
        let mut px = Vec::new();
        for x in 0..4 {
            px.extend(if x < 2 {
                [255, 0, 0, 255]
            } else {
                [0, 0, 0, 0]
            });
        }
        let row: Vec<u8> = px.iter().chain(px.iter()).copied().collect();
        let out = shrink_rgba(&row, 4, 2, 2, 1);
        assert_eq!(out, vec![255, 0, 0, 255, 0, 0, 0, 0]);
        // A half-covered block keeps full red at half alpha, not a darkened red.
        let out = shrink_rgba(&row, 4, 2, 1, 1);
        assert_eq!(out, vec![255, 0, 0, 127]);
    }

    /// Hardware: a reframed session encodes the crop at half size. The decoded picture's edges
    /// hold the crop's own colours, none of the frame around it.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver and ffmpeg — run on the RTX box (.21)"]
    fn nvenc_cuda_reframe_crops_and_scales() {
        const SW: u32 = 768;
        const SH: u32 = 432;
        const W: u32 = 256;
        const H: u32 = 144;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Bgrx,
            W,
            H,
            60,
            8_000_000,
            false,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        assert!(enc.caps().crops_input && enc.caps().downscales_input);
        enc.set_input_crop([128, 72, 512, 288]).expect("reframe");
        // BGRX: green rows outside the crop, blue columns beside it, red then white inside.
        let mut px = vec![0u8; (SW * SH * 4) as usize];
        for y in 0..SH {
            for x in 0..SW {
                let c: [u8; 4] = if !(72..360).contains(&y) {
                    [40, 200, 40, 255]
                } else if !(128..640).contains(&x) {
                    [200, 40, 40, 255]
                } else if x < 384 {
                    [40, 40, 200, 255]
                } else {
                    [235, 235, 235, 255]
                };
                let i = ((y * SW + x) * 4) as usize;
                px[i..i + 4].copy_from_slice(&c);
            }
        }
        let mut stream = Vec::new();
        for i in 0..4u32 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: SW,
                height: SH,
                pts_ns: u64::from(i) * 16_666_667,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(px.clone()),
                cursor: None,
            };
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                stream.extend_from_slice(&au.data);
            }
        }
        enc.flush().expect("flush");
        while let Some(au) = enc.poll().expect("poll") {
            stream.extend_from_slice(&au.data);
        }
        assert_eq!(
            (enc.s.width, enc.s.height),
            (W, H),
            "the session runs at the reframed size"
        );
        let path = std::env::temp_dir().join("nvenc-reframe.h265");
        std::fs::write(&path, &stream).expect("write the stream");
        let Ok(out) = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
            .output()
        else {
            println!("no ffmpeg — skipping the picture check");
            return;
        };
        assert_eq!(
            out.stdout.len(),
            (W * H * 3) as usize,
            "decoded size: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let at = |x: u32, y: u32| {
            let i = ((y * W + x) * 3) as usize;
            [out.stdout[i], out.stdout[i + 1], out.stdout[i + 2]]
        };
        let near = |got: [u8; 3], want: [u8; 3], what: &str| {
            assert!(
                got.iter().zip(want).all(|(g, w)| g.abs_diff(w) <= 40),
                "{what}: got {got:?}, want {want:?}"
            );
        };
        near(at(3, H / 2), [200, 40, 40], "left edge is the crop's red");
        near(
            at(W - 4, H / 2),
            [235, 235, 235],
            "right edge is the crop's white",
        );
        near(at(W / 4, 2), [200, 40, 40], "top edge has no green");
        near(
            at(W * 3 / 4, H - 3),
            [235, 235, 235],
            "bottom edge has no green",
        );
    }

    /// Hardware: cursor blend on a 10-bit packed slot. An 8-bit fallback would tint the
    /// pointer. Blend correctness is display-referred — not asserted here.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver with 10-bit encode"]
    fn nvenc_cuda_hdr10_cursor_blend() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::X2Rgb10,
            W,
            H,
            60,
            8_000_000,
            true,
            10,
            true,
            ChromaFormat::Yuv420,
            true, // Vulkan slot ring + 10-bit blend
            4,
        )
        .expect("open NVENC CUDA session");
        let cursor = |serial: u64, x: i32, y: i32| pf_frame::CursorOverlay {
            x,
            y,
            w: 32,
            h: 32,
            rgba: std::sync::Arc::new(vec![0xFF; 32 * 32 * 4]),
            serial,
            hot_x: 0,
            hot_y: 0,
            visible: true,
        };
        let mut aus = 0usize;
        for i in 0..6u32 {
            let mut frame = rgb10_frame(W, H, i);
            // Serial flip at frame 3 (upload quiesce); position moves every frame.
            frame.cursor = Some(cursor(
                if i < 3 { 1 } else { 2 },
                40 + i as i32 * 9,
                60 + i as i32 * 5,
            ));
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                assert!(!au.data.is_empty(), "empty AU");
                aus += 1;
            }
        }
        enc.flush().ok();
        assert!(aus > 0, "no AUs produced");
        assert_eq!(enc.s.bit_depth, 10, "must be a 10-bit session");
        assert_eq!(
            slot_fmt_of(enc.s.buffer_fmt),
            SlotFormat::X2Rgb10,
            "the blend must target the 10-bit packed slot layout, not the 8-bit one"
        );
        assert!(
            enc.caps().blends_cursor,
            "the direct-SDK path must still report a cursor blend at 10-bit"
        );
        println!("nvenc_cuda HDR10 cursor blend: {aus} AUs, slot fmt X2Rgb10");
    }

    /// Hardware: a packed-RGB 8-bit capture under a 10-bit session encodes a 10-bit stream,
    /// BT.709, no PQ (the `.obu`/`.h265` land in `PUNKTFUNK_SMOKE_DIR` for `ffprobe`: each must
    /// read `yuv420p10le` bt709). The planar arms are the regression guard: real Linux capture
    /// is NV12 (and 4:4:4 is planar YUV444), which NVENC refuses in a 10-bit session — the
    /// encoder must degrade those to 8-bit rather than fail `register_resource`.
    ///
    /// `cargo test -p pf-encode --features nvenc --lib nvenc_cuda_sdr10 -- --ignored --nocapture`
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_sdr10_from_eight_bit_capture() {
        const W: u32 = 1280;
        const H: u32 = 720;
        let dir = std::env::var("PUNKTFUNK_SMOKE_DIR").unwrap_or_else(|_| ".".into());
        let cpu_frame = |i: u32| CapturedFrame {
            provenance: Default::default(),
            width: W,
            height: H,
            pts_ns: u64::from(i) * 16_666_667,
            format: PixelFormat::Bgra,
            payload: FramePayload::Cpu(crate::smoke_pattern::scroll_pattern(
                W as usize, H as usize, i as usize,
            )),
            cursor: None,
        };
        for (codec, tag, ext) in [(Codec::Av1, "av1", "obu"), (Codec::H265, "hevc", "h265")] {
            pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
            let mut enc = NvencCudaEncoder::open(
                codec,
                PixelFormat::Bgra,
                W,
                H,
                60,
                40_000_000,
                true,
                10,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA SDR-10 session");
            let mut stream = Vec::new();
            for i in 0..12u32 {
                enc.submit_indexed(&cpu_frame(i), i).expect("submit SDR-10");
                while let Some(au) = enc.poll().expect("poll") {
                    stream.extend_from_slice(&au.data);
                }
            }
            enc.flush().ok();
            assert!(!stream.is_empty(), "{tag}: no AUs produced");
            assert_eq!(
                enc.s.bit_depth, 10,
                "{tag}: an 8-bit capture must still encode 10-bit"
            );
            assert!(
                !enc.s.hdr,
                "{tag}: 10-bit SDR must not claim HDR — that stamps a PQ VUI"
            );
            let path = format!("{dir}/nvenc-cuda-sdr10-{tag}.{ext}");
            std::fs::write(&path, &stream).expect("write");
            println!(
                "nvenc_cuda SDR-10 {tag}: {} bytes, depth={} hdr={} -> {path}",
                stream.len(),
                enc.s.bit_depth,
                enc.s.hdr
            );
        }

        // Regression guard: a planar 8-bit capture (real Linux default is NV12; 4:4:4 is
        // planar YUV444) under a 10-bit-negotiated session must degrade to 8-bit and encode,
        // never fail register_resource and end the video.
        for (label, fmt, chroma, layout) in [
            (
                "nv12",
                PixelFormat::Nv12,
                ChromaFormat::Yuv420,
                PlaneLayout::Nv12,
            ),
            (
                "yuv444",
                PixelFormat::Yuv444,
                ChromaFormat::Yuv444,
                PlaneLayout::Yuv444,
            ),
        ] {
            pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                fmt,
                W,
                H,
                60,
                40_000_000,
                true,
                10,
                fmt.is_ten_bit(),
                chroma,
                false,
                4,
            )
            .expect("open planar SDR-10 session");
            let mut aus = 0usize;
            for i in 0..4u32 {
                let frame = CapturedFrame {
                    provenance: Default::default(),
                    width: W,
                    height: H,
                    pts_ns: u64::from(i) * 16_666_667,
                    format: fmt,
                    payload: FramePayload::Cuda(
                        DeviceBuffer::alloc(layout, W, H).expect("alloc planar device buffer"),
                    ),
                    cursor: None,
                };
                enc.submit_indexed(&frame, i).unwrap_or_else(|e| {
                    panic!("{label}: planar submit must degrade, not fail: {e:#}")
                });
                while let Some(_au) = enc.poll().expect("poll") {
                    aus += 1;
                }
            }
            enc.flush().ok();
            assert_eq!(
                enc.s.bit_depth, 8,
                "{label}: a planar 8-bit capture must degrade to 8-bit"
            );
            assert!(!enc.s.hdr, "{label}: SDR");
            assert!(aus > 0, "{label}: no AUs");
            println!("nvenc_cuda SDR-10 {label}: degraded to 8-bit, {aus} AUs (no crash)");
        }
    }

    /// Hardware: HEVC FREXT YUV444 (stacked-plane copy NV12 does not exercise).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_yuv444() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Yuv444,
            W,
            H,
            60,
            40_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv444,
            false,
            4,
        )
        .expect("open NVENC CUDA 4:4:4 session");

        let mut aus = 0usize;
        for i in 0..6u32 {
            let buf =
                DeviceBuffer::alloc(PlaneLayout::Yuv444, W, H).expect("alloc YUV444 device buffer");
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: W,
                height: H,
                pts_ns: i as u64 * 16_666_667,
                format: PixelFormat::Yuv444,
                payload: FramePayload::Cuda(buf),
                cursor: None,
            };
            enc.submit_indexed(&frame, i).expect("submit 444");
            while let Some(_au) = enc.poll().expect("poll") {
                aus += 1;
            }
        }
        assert!(aus > 0, "no 4:4:4 AUs produced");
        assert!(enc.caps().chroma_444, "RTX NVENC HEVC must report 4:4:4");
        println!("nvenc_cuda 4:4:4 smoke: {aus} AUs, caps.chroma_444=true");
    }

    /// Hardware: in-place rate retarget up and down must not emit an IDR.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_reconfigure_no_idr() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
            let mut keyframes = 0usize;
            let mut aus = 0usize;
            for i in range {
                let frame = nv12_frame(W, H, i);
                enc.submit_indexed(&frame, i).expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    aus += 1;
                    keyframes += au.keyframe as usize;
                }
            }
            (aus, keyframes)
        };

        let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
        assert!(aus > 0, "no AUs before the reconfigure");
        assert_eq!(kfs, 1, "exactly the opening IDR before the reconfigure");

        assert!(
            enc.reconfigure_bitrate(60_000_000),
            "in-place reconfigure to 60 Mbps must succeed on RTX NVENC"
        );
        let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
        assert!(aus > 0, "no AUs after the up-reconfigure");
        assert_eq!(kfs, 0, "an in-place rate retarget must not emit an IDR");

        assert!(
            enc.reconfigure_bitrate(10_000_000),
            "in-place reconfigure down to 10 Mbps must succeed"
        );
        let (aus, kfs) = submit_and_poll(&mut enc, 8..12);
        assert!(aus > 0, "no AUs after the down-reconfigure");
        assert_eq!(kfs, 0, "an in-place rate retarget must not emit an IDR");

        enc.flush().ok();
        println!("nvenc_cuda reconfigure smoke: 20→60→10 Mbps in place, zero IDRs");
    }

    /// Hardware: can `splitEncodeMode` move in place (`resetEncoder=0`) without an IDR?
    /// Sub-frame off — HEVC forced-split + sub-frame is unsupported, which would reject for
    /// the wrong reason. Reports the verdict; asserts only that the measurement is valid.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_reconfigure_in_place() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // Sub-frame off; open split-disabled so the switch is a real change.
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        set_env("PUNKTFUNK_SPLIT_ENCODE", "0");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
            let (mut aus, mut keyframes) = (0usize, 0usize);
            for i in range {
                let frame = nv12_frame(W, H, i);
                enc.submit_indexed(&frame, i).expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    aus += 1;
                    keyframes += au.keyframe as usize;
                }
            }
            (aus, keyframes)
        };

        // Session is lazy; reconfigure while `!inited` short-circuits to `true`.
        let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
        assert!(aus > 0, "no AUs before the reconfigure");
        assert_eq!(kfs, 1, "exactly the opening IDR before the reconfigure");
        assert!(
            enc.s.inited(),
            "session must be live for the spike to mean anything"
        );
        assert_eq!(
            enc.s.split_mode, disable,
            "the spike needs to OPEN split-disabled so the switch is a real change"
        );

        // Forced-2 on a 1-engine GPU is rejected for the wrong reason.
        // SAFETY: live session (`inited`); `get_cap` returns 0 on driver error.
        let engines = unsafe {
            enc.s.get_cap(
                enc.s.handle(),
                nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
            )
        };
        println!(
            "S1: NV_ENC_CAPS_NUM_ENCODER_ENGINES = {engines} (query_caps latched \
             encoder_engines={})",
            self_engines(&enc)
        );
        // `resolve_split_mode` reads the latched field, not the live cap.
        assert_eq!(
            self_engines(&enc),
            engines.max(0) as u32,
            "query_caps must latch NUM_ENCODER_ENGINES — resolve_split_mode reads that field, \
             not the live cap"
        );
        assert!(
            engines >= 2,
            "this GPU reports {engines} NVENC engine(s) — S1 is not interpretable here, run it on \
             a 2-engine card"
        );

        // Change only `splitEncodeMode`.
        enc.s.split_mode = two;
        let accepted = enc.reconfigure_bitrate(BPS);
        println!("S1: reconfigure DISABLE→TWO_FORCED accepted = {accepted}");

        let verdict = if !accepted {
            // Live session is still split-disabled — keep the field truthful.
            enc.s.split_mode = disable;
            "FAIL — driver REJECTED the in-place splitEncodeMode change"
        } else {
            let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
            assert!(aus > 0, "no AUs after the accepted reconfigure");
            if kfs == 0 {
                "PASS — accepted with NO IDR: mid-stream split adaptation is free"
            } else {
                "FAIL — accepted but forced an IDR (silently), which is the same as a rejection"
            }
        };
        println!("S1 VERDICT: {verdict}");

        // Reverse only if the forward change was accepted.
        if accepted {
            enc.s.split_mode = disable;
            let back = enc.reconfigure_bitrate(BPS);
            let kfs = if back {
                submit_and_poll(&mut enc, 8..12).1
            } else {
                usize::MAX
            };
            println!("S1: reverse TWO_FORCED→DISABLE accepted = {back}, keyframes after = {kfs}");
        }

        enc.flush().ok();
        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// Hardware: did an accepted in-place split actually take effect? A = fresh DISABLE, B =
    /// fresh TWO_FORCED, C = DISABLE→TWO in place. C ≈ B ⇒ real; C ≈ A ⇒ ignored.
    /// Bytes/AU are load-bearing: uninit VRAM under CBR can collapse every leg.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_reconfigure_takes_effect() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        use std::time::Instant;
        const W: u32 = 3840;
        const H: u32 = 2160;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 8;
        const MEASURED: u32 = 24;
        /// Post-switch discard. Split does not reach steady state on frame 0; 16 was enough
        /// for the switched leg to match a fresh TWO_FORCED session.
        const SETTLE: u32 = 16;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");

        // Rotated buffers do not help: the driver still returns zeroed VRAM. This harness
        // measures pixel-proportional cost only.
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // (early p50 µs, late p50 µs, median B/AU).
        let run_leg = |open_split: &str, switch_to: Option<u32>| -> (u128, u128, usize) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", open_split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                W,
                H,
                60,
                BPS,
                true,
                8,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");

            // Same measured length; a switched leg starts `SETTLE` frames later.
            let measure_from = if switch_to.is_some() {
                WARMUP + SETTLE
            } else {
                WARMUP
            };
            let (mut times, mut sizes) = (Vec::new(), Vec::new());
            for i in 0..(measure_from + MEASURED) {
                // In-place switch once, after warmup.
                if i == WARMUP {
                    if let Some(target) = switch_to {
                        enc.s.split_mode = target;
                        assert!(
                            enc.reconfigure_bitrate(BPS),
                            "in-place split switch must be accepted (S1a proved it is)"
                        );
                        continue;
                    }
                }
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                let mut got = 0usize;
                while let Some(au) = enc.poll().expect("poll") {
                    got = au.data.len();
                }
                let dt = t0.elapsed().as_micros();
                if i >= measure_from {
                    times.push(dt);
                    sizes.push(got);
                }
            }
            enc.flush().ok();
            // Early vs late: a whole-window median of a settling switch lands between the arms.
            let half = times.len() / 2;
            let med = |s: &[u128]| {
                let mut v = s.to_vec();
                v.sort_unstable();
                v[v.len() / 2]
            };
            let (early, late) = (med(&times[..half]), med(&times[half..]));
            sizes.sort_unstable();
            (early, late, sizes[sizes.len() / 2])
        };

        let (a_early, a_late, a_bytes) = run_leg("0", None);
        let (b_early, b_late, b_bytes) = run_leg("2", None);
        let (c_early, c_late, c_bytes) = run_leg("0", Some(two));
        let (a_us, b_us, c_us) = (a_late, b_late, c_late);

        println!("S1b @ {W}x{H}@60 HEVC 8-bit, {} Mbps CBR:", BPS / 1_000_000);
        println!("  (early = first half of the measured window, late = second half)");
        println!(
            "  A fresh DISABLE      : early {a_early:>6} late {a_late:>6} us/frame, {a_bytes:>8} B/AU"
        );
        println!(
            "  B fresh TWO_FORCED   : early {b_early:>6} late {b_late:>6} us/frame, {b_bytes:>8} B/AU"
        );
        println!(
            "  C DISABLE→TWO in situ: early {c_early:>6} late {c_late:>6} us/frame, {c_bytes:>8} B/AU"
        );
        if c_early > c_late + c_late / 8 {
            println!(
                "  ⇒ leg C SETTLES ({c_early} → {c_late} us): the in-place switch is not \
                 instantaneous, so a whole-window median understates it."
            );
        }

        let want_bytes = (BPS / 60 / 8) as usize;
        if a_bytes * 4 < want_bytes {
            println!(
                "  ⚠ INCONCLUSIVE on content: {a_bytes} B/AU is far below the {want_bytes} B/AU \
                 CBR quota — rate control ran out of things to code, so these legs are not the \
                 high-bits/frame regime the field case is in."
            );
        }
        let (near_b, near_a) = (c_us.abs_diff(b_us), c_us.abs_diff(a_us));
        println!(
            "  ⇒ C is nearer {} (|C-B|={near_b} vs |C-A|={near_a}) — {}",
            if near_b < near_a { "B" } else { "A" },
            if near_b < near_a {
                "the in-place split switch TOOK EFFECT"
            } else {
                "the driver appears to have IGNORED the in-place split change"
            }
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        let _ = (a_bytes, b_bytes, c_bytes);
    }

    /// Hardware: can `(split, sub-frame)` move as a pair in place, IDR-free?
    /// `reconfigure_bitrate` does not recompute `subframe_chunks` — a caller flipping
    /// sub-frame must clear that latch or `poll_chunk` busy-polls.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_subframe_pair_reconfigure() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        const W: u32 = 1920;
        const H: u32 = 1080;
        const BPS: u64 = 40_000_000;
        let disable = M::NV_ENC_SPLIT_DISABLE_MODE as u32;
        let two = M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32;

        // Split-disabled; sub-frame at the caps-gated default.
        set_env("PUNKTFUNK_SPLIT_ENCODE", "0");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            BPS,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let submit_and_poll = |enc: &mut NvencCudaEncoder, range: std::ops::Range<u32>| {
            let (mut aus, mut keyframes) = (0usize, 0usize);
            for i in range {
                let frame = nv12_frame(W, H, i);
                enc.submit_indexed(&frame, i).expect("submit");
                while let Some(au) = enc.poll().expect("poll") {
                    aus += 1;
                    keyframes += au.keyframe as usize;
                }
            }
            (aus, keyframes)
        };

        let (aus, kfs) = submit_and_poll(&mut enc, 0..4);
        assert!(aus > 0 && kfs == 1, "opening IDR then steady P-frames");
        println!(
            "S1c: opened split={} subframe_on={} subframe_chunks={} chunked_poll={}",
            enc.s.split_mode,
            enc.s.subframe_on,
            enc.s.subframe_chunks,
            enc.supports_chunked_poll()
        );
        if !enc.s.subframe_on {
            println!(
                "S1c SKIPPED: sub-frame is off at open on this GPU/driver, so there is no pair to \
                 flip — the arbitration reduces to S1a's plain split switch here."
            );
            remove_env("PUNKTFUNK_SPLIT_ENCODE");
            return;
        }

        // Clear the chunked-poll latch with the sub-frame flag, or `poll_chunk` outlives it.
        enc.s.split_mode = two;
        enc.s.subframe_on = false;
        enc.s.subframe_chunks = false;
        let accepted = enc.reconfigure_bitrate(BPS);
        println!("S1c: (DISABLE,sub-frame on) → (TWO_FORCED,sub-frame off) accepted = {accepted}");

        if accepted {
            let (aus, kfs) = submit_and_poll(&mut enc, 4..8);
            assert!(aus > 0, "no AUs after the pair flip");
            assert!(
                !enc.supports_chunked_poll(),
                "chunked poll must be disarmed once sub-frame is off — a stale latch makes \
                 poll_chunk busy-poll its whole budget every AU"
            );
            println!(
                "S1c VERDICT: {}",
                if kfs == 0 {
                    "PASS — the split×sub-frame PAIR moves in place with NO IDR"
                } else {
                    "FAIL — pair flip forced an IDR"
                }
            );

            // Reverse pair (de-escalation).
            enc.s.split_mode = disable;
            enc.s.subframe_on = true;
            enc.s.subframe_chunks = enc.s.slices >= 2 && !enc.s.retrieving();
            let back = enc.reconfigure_bitrate(BPS);
            let kfs_back = if back {
                submit_and_poll(&mut enc, 8..12).1
            } else {
                usize::MAX
            };
            println!("S1c: reverse pair flip accepted = {back}, keyframes after = {kfs_back}");
        } else {
            println!(
                "S1c VERDICT: FAIL — driver REJECTED the pair flip. Split can still move alone \
                 (S1a), so a WP3 arbitration would have to keep sub-frame fixed for the session \
                 and only arbitrate split within that."
            );
            enc.s.split_mode = disable;
            enc.s.subframe_on = true;
        }

        enc.flush().ok();
        remove_env("PUNKTFUNK_SPLIT_ENCODE");
    }

    /// Hardware: does plain AUTO + default-on sub-frame actually split? HEVC split is
    /// unsupported with sub-frame, so AUTO may mean "never split". Time AUTO vs DISABLE vs
    /// TWO_FORCED at 4K (pixel-proportional; VRAM is zeroed).
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_auto_split_with_subframe() {
        use std::time::Instant;
        const W: u32 = 3840;
        const H: u32 = 2160;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 8;
        const MEASURED: u32 = 24;

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // `split: None` = unset = plain AUTO. Env `1` is AUTO_FORCED, which disarms sub-frame.
        let run = |split: Option<&str>, subframe: Option<&str>| -> (u128, bool) {
            match split {
                Some(v) => set_env("PUNKTFUNK_SPLIT_ENCODE", v),
                None => remove_env("PUNKTFUNK_SPLIT_ENCODE"),
            }
            match subframe {
                Some(v) => set_env("PUNKTFUNK_NVENC_SUBFRAME", v),
                None => remove_env("PUNKTFUNK_NVENC_SUBFRAME"),
            }
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                W,
                H,
                60,
                BPS,
                true,
                8,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let mut times = Vec::new();
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                while enc.poll().expect("poll").is_some() {}
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                }
            }
            let sub = enc.s.subframe_on;
            enc.flush().ok();
            times.sort_unstable();
            (times[times.len() / 2], sub)
        };

        // Unset env: 4K60 8-bit is below SPLIT_FORCE_PIXEL_RATE → plain AUTO. Sub-frame must
        // stay on or this is not the fleet shape.
        let (auto_us, auto_sub) = run(None, None);
        let (dis_us, dis_sub) = run(Some("0"), None);
        let (two_us, two_sub) = run(Some("2"), Some("0"));
        // AUTO with sub-frame off: retiring AUTO would also change that shape.
        let (auto_nosub_us, auto_nosub_sub) = run(None, Some("0"));

        println!("D5 confirm @ {W}x{H}@60 HEVC 8-bit:");
        println!("  AUTO (unset) + sub-frame({auto_sub}) : {auto_us:>6} us/frame");
        println!("  DISABLE      + sub-frame({dis_sub}) : {dis_us:>6} us/frame");
        println!("  TWO_FORCED,   no sub-frame({two_sub}): {two_us:>6} us/frame");
        println!("  AUTO (unset), no sub-frame({auto_nosub_sub}): {auto_nosub_us:>6} us/frame");
        println!(
            "  ⇒ with sub-frame OFF, AUTO is nearer {} — retiring the AUTO arm {}",
            if auto_nosub_us.abs_diff(two_us) < auto_nosub_us.abs_diff(dis_us) {
                "TWO_FORCED (it DOES split)"
            } else {
                "DISABLE (it does not split either way)"
            },
            if auto_nosub_us.abs_diff(two_us) < auto_nosub_us.abs_diff(dis_us) {
                "would LOSE a real split on sub-frame-off sessions"
            } else {
                "is behaviour-neutral"
            }
        );
        assert!(
            auto_sub,
            "the AUTO leg resolved sub-frame OFF — it is not testing D5's fleet shape"
        );
        let (near_dis, near_two) = (auto_us.abs_diff(dis_us), auto_us.abs_diff(two_us));
        println!(
            "  ⇒ AUTO sits nearer {} (|A-D|={near_dis} vs |A-T|={near_two}) — D5 {}",
            if near_dis < near_two {
                "DISABLE"
            } else {
                "TWO"
            },
            if near_dis < near_two {
                "CONFIRMED: AUTO + sub-frame does NOT split; the resolver's AUTO arm is dead"
            } else {
                "REFUTED: AUTO does engage the second engine even with sub-frame on"
            }
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// Hardware: split ceiling. A refused mode falls back to DISABLE, not an error. Timing
    /// tells whether an accepted mode actually used more engines.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_hardware_max() {
        use nv::NV_ENC_SPLIT_ENCODE_MODE as M;
        use std::time::Instant;
        const W: u32 = 3840;
        const H: u32 = 2160;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 8;
        const MEASURED: u32 = 24;

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();

        // (opened mode, p50 µs, engines).
        let run = |split: &str| -> (u32, u128, i32) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::Nv12,
                W,
                H,
                60,
                BPS,
                true,
                8,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let mut times = Vec::new();
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                while enc.poll().expect("poll").is_some() {}
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                }
            }
            // SAFETY: live session; `get_cap` returns 0 on driver error.
            let engines = unsafe {
                enc.s.get_cap(
                    enc.s.handle(),
                    nv::NV_ENC_CAPS::NV_ENC_CAPS_NUM_ENCODER_ENGINES,
                )
            };
            let opened = enc.s.split_mode;
            enc.flush().ok();
            times.sort_unstable();
            (opened, times[times.len() / 2], engines)
        };

        println!("split ceiling probe @ {W}x{H}@60 HEVC 8-bit:");
        let mut baseline = None;
        // Env `0` selects DISABLE (enum 15), not the integer 0.
        for (label, env, want) in [
            ("DISABLE     ", "0", M::NV_ENC_SPLIT_DISABLE_MODE as u32),
            ("AUTO_FORCED ", "1", M::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32),
            ("TWO_FORCED  ", "2", M::NV_ENC_SPLIT_TWO_FORCED_MODE as u32),
            (
                "THREE_FORCED",
                "3",
                M::NV_ENC_SPLIT_THREE_FORCED_MODE as u32,
            ),
        ] {
            let (opened, us, engines) = run(env);
            let honoured = opened == want;
            let vs = match baseline {
                None => {
                    baseline = Some(us);
                    String::new()
                }
                Some(b) => format!("  ({:.2}× vs DISABLE)", b as f64 / us as f64),
            };
            println!(
                "  req {label} → opened_mode={opened:<2} {} {us:>6} us/frame{vs}  [engines={engines}]",
                if honoured { "HONOURED" } else { "FELL BACK" }
            );
        }
        println!(
            "  note: opened_mode 15 = DISABLE (the backend's rejection fallback); a mode that is \
             HONOURED but no faster than DISABLE was accepted and did nothing."
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// Hardware: live split arbitration at 4K. Sub-frame off so the no-trade gate arms.
    /// Must settle with zero extra IDRs and cache a splitting arm.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_split_arbitration_converges() {
        const W: u32 = 3840;
        const H: u32 = 2160;
        let disable = nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_DISABLE_MODE as u32;

        set_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE", "1");
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        remove_env("PUNKTFUNK_SPLIT_ENCODE");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let frames: Vec<CapturedFrame> = (0..4).map(|i| nv12_frame(W, H, i)).collect();
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            400_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let mut keyframes = 0usize;
        let mut aus = 0usize;
        // Measure + settle + measure, with slack.
        for i in 0..140u32 {
            enc.submit_indexed(&frames[(i % 4) as usize], i)
                .expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                aus += 1;
                keyframes += au.keyframe as usize;
            }
        }
        let final_mode = enc.s.split_mode;
        let still_arbitrating = enc.s.arbiter.is_some();
        let verdict = cached_split_verdict(&enc.s.split_key());
        let enc_engines = enc.s.encoder_engines;
        enc.flush().ok();

        println!(
            "arbitration: {aus} AUs, {keyframes} keyframes, final split_mode={final_mode}, \
             cached verdict={verdict:?}, still running={still_arbitrating}"
        );
        assert!(aus > 100, "not enough AUs to complete an arbitration");
        assert!(
            !still_arbitrating,
            "arbitration did not finish in 140 frames"
        );
        assert_eq!(
            keyframes, 1,
            "THE POINT OF THIS DESIGN: arbitration must cost ZERO extra IDRs — only the session's \
             opening one"
        );
        assert_eq!(
            verdict,
            Some(final_mode),
            "the winning arm must be cached so later sessions skip the experiment"
        );
        assert_ne!(
            final_mode, disable,
            "at 4K with two engines a splitting arm is ~2x faster, so single-engine must not win"
        );
        // 4K60 is under SPLIT_FORCE_PIXEL_RATE (AUTO vs widest). Single-engine must not win.
        println!(
            "  (incumbent was the static rule's choice; challenger was mode {})",
            max_forced_split_mode(enc_engines)
        );

        remove_env("PUNKTFUNK_NVENC_SPLIT_ARBITRATE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        // Process-global cache would steer later tests that open this config with split unset.
        super::super::nvenc_core::clear_split_verdicts();
    }

    /// Hardware: Main10 split A/B (packed RGB10). Sub-frame off. Reports; both outcomes
    /// are legitimate. `PF_AB_MODE` can retarget the operating point.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 vs Blackwell .21)"]
    fn nvenc_cuda_main10_split_ab() {
        use std::time::Instant;
        const BPS: u64 = 400_000_000;
        const WARMUP: u32 = 12;
        const MEASURED: u32 = 32;
        // `PF_AB_MODE=WxHxFPS` retargets; default 4K60.
        let (w, h, fps) = std::env::var("PF_AB_MODE")
            .ok()
            .and_then(|s| {
                let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            })
            .unwrap_or((3840, 2160, 60));

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Packed 10-bit: `bit_depth`/`hdr` are derived from the input, not the args.
        let frames: Vec<CapturedFrame> = (0..4).map(|i| rgb10_frame(w, h, i)).collect();

        let run = |split: &str| -> (u128, u8, usize) {
            set_env("PUNKTFUNK_SPLIT_ENCODE", split);
            let mut enc = NvencCudaEncoder::open(
                Codec::H265,
                PixelFormat::X2Rgb10,
                w,
                h,
                fps,
                BPS,
                true,
                10,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open NVENC CUDA session");
            let (mut times, mut bytes) = (Vec::new(), Vec::new());
            for i in 0..(WARMUP + MEASURED) {
                let t0 = Instant::now();
                enc.submit_indexed(&frames[(i % 4) as usize], i)
                    .expect("submit");
                let mut got = 0usize;
                while let Some(au) = enc.poll().expect("poll") {
                    got = au.data.len();
                }
                if i >= WARMUP {
                    times.push(t0.elapsed().as_micros());
                    bytes.push(got);
                }
            }
            let depth = enc.s.bit_depth;
            let opened = enc.s.split_mode;
            enc.flush().ok();
            times.sort_unstable();
            bytes.sort_unstable();
            println!(
                "    (opened split_mode={opened}, derived bit_depth={depth}, \
                 {} B/AU)",
                bytes[bytes.len() / 2]
            );
            (times[times.len() / 2], depth, bytes[bytes.len() / 2])
        };

        println!(
            "Main10 split A/B @ {w}x{h}@{fps} HEVC 10-bit, {} Mbps:",
            BPS / 1_000_000
        );
        let (single_us, d1, _) = run("0");
        println!("  single-engine : {single_us:>6} us/frame");
        let (split_us, d2, _) = run("2");
        println!("  forced 2-way  : {split_us:>6} us/frame");
        assert_eq!(d1, 10, "leg 1 did not derive a 10-bit session");
        assert_eq!(d2, 10, "leg 2 did not derive a 10-bit session");
        let ratio = single_us as f64 / split_us.max(1) as f64;
        println!(
            "  ⇒ split is {ratio:.2}× the single-engine rate — {}",
            if ratio > 1.15 {
                "split WINS for Main10 here; the 2.7x-slower datapoint does NOT generalise"
            } else if ratio < 0.87 {
                "split LOSES for Main10 — the veto was right and must come back, scoped"
            } else {
                "a wash; neither arm is clearly better for Main10 here"
            }
        );

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// Hardware: bits/frame curve. Zeroed VRAM only measures pixel-proportional cost.
    /// [`noise_nv12_frame`] supplies entropy. Print B/AU next to every timing.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually (Ada .181 / Blackwell .21)"]
    fn nvenc_cuda_bits_per_frame_curve() {
        use std::time::Instant;
        const WARMUP: u32 = 10;
        const MEASURED: u32 = 24;
        let (w, h, fps) = std::env::var("PF_AB_MODE")
            .ok()
            .and_then(|s| {
                let p: Vec<u32> = s.split('x').filter_map(|v| v.parse().ok()).collect();
                (p.len() == 3).then(|| (p[0], p[1], p[2]))
            })
            .unwrap_or((3840, 2160, 60));

        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Sweep spatial detail, not nominal bitrate. Pure noise overshoots any low target;
        // the x-axis is bits/frame actually produced.
        let bps: u64 = 600_000_000;
        println!(
            "bits/frame curve @ {w}x{h}@{fps} HEVC 8-bit, REAL content, {} Mbps cap:",
            bps / 1_000_000
        );
        println!("  detail | ACTUAL bits/frame |   single |  split-2 | ratio");
        for block in [64usize, 32, 16, 8, 4, 1] {
            let frames: Vec<CapturedFrame> =
                (0..4).map(|i| noise_nv12_frame(w, h, i, block)).collect();
            let run = |split: &str| -> (u128, usize) {
                set_env("PUNKTFUNK_SPLIT_ENCODE", split);
                let mut enc = NvencCudaEncoder::open(
                    Codec::H265,
                    PixelFormat::Nv12,
                    w,
                    h,
                    fps,
                    bps,
                    true,
                    8,
                    false,
                    ChromaFormat::Yuv420,
                    false,
                    4,
                )
                .expect("open NVENC CUDA session");
                let (mut times, mut bytes) = (Vec::new(), Vec::new());
                for i in 0..(WARMUP + MEASURED) {
                    let t0 = Instant::now();
                    enc.submit_indexed(&frames[(i % 4) as usize], i)
                        .expect("submit");
                    let mut got = 0usize;
                    while let Some(au) = enc.poll().expect("poll") {
                        got = au.data.len();
                    }
                    if i >= WARMUP {
                        times.push(t0.elapsed().as_micros());
                        bytes.push(got);
                    }
                }
                enc.flush().ok();
                times.sort_unstable();
                bytes.sort_unstable();
                (times[times.len() / 2], bytes[bytes.len() / 2])
            };
            let (s_us, s_bytes) = run("0");
            let (p_us, _) = run("2");
            println!(
                "  {block:>5}px | {:>10.2} Mbit    | {s_us:>6}us | {p_us:>6}us | {:>4.2}×",
                s_bytes as f64 * 8.0 / 1e6,
                s_us as f64 / p_us.max(1) as f64
            );
        }

        remove_env("PUNKTFUNK_SPLIT_ENCODE");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
    }

    /// Pre-session / nonsense RFI declines. Skips if the NVENC `.so` is absent.
    #[test]
    fn rfi_declines_impossible_ranges() {
        let Ok(mut enc) = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            1920,
            1080,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        ) else {
            eprintln!(
                "skipping rfi_declines_impossible_ranges: NVENC unavailable (no NVIDIA driver)"
            );
            return;
        };
        // Lazy init: no session yet.
        assert!(!enc.invalidate_ref_frames(0, 0), "no session → decline");
        assert!(!enc.invalidate_ref_frames(10, 5), "first > last → decline");
        assert!(
            !enc.invalidate_ref_frames(-1, 3),
            "negative first → decline"
        );
    }

    fn open_h265() -> NvencCudaEncoder {
        NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            1280,
            720,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA encoder")
    }

    /// Hardware: cycle codecs in one process; every leg must open and encode.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_codec_switch_reopen() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        for (leg, codec) in [
            Codec::H265,
            Codec::Av1,
            Codec::H265,
            Codec::H264,
            Codec::H265,
        ]
        .into_iter()
        .enumerate()
        {
            let mut enc = NvencCudaEncoder::open(
                codec,
                PixelFormat::Nv12,
                W,
                H,
                60,
                20_000_000,
                true,
                8,
                false,
                ChromaFormat::Yuv420,
                false,
                4,
            )
            .expect("open");
            for f in 0..4u32 {
                let frame = nv12_frame(W, H, f);
                enc.submit_indexed(&frame, f)
                    .unwrap_or_else(|e| panic!("leg {leg} {codec:?} submit failed: {e:#}"));
                while enc.poll().expect("poll").is_some() {}
            }
            drop(enc);
        }
        println!("nvenc_cuda codec-switch: 5 legs across H265/AV1/H264, all clean");
    }

    /// Hardware: the H.264 stream through the client's planner, one AU in, one picture
    /// out. 1920x1200 is level 5, where an unstated reorder bound is 12 pictures.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_h264_shows_each_picture_in_its_own_au() {
        const W: u32 = 1920;
        const H: u32 = 1200;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H264,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let mut planner = pf_vaapi::H264Planner::new();
        let mut stored = Vec::new();
        let mut lags = Vec::new();
        for i in 0..40u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            while let Some(au) = enc.poll().expect("poll") {
                let plan = planner.plan_au(&au.data).expect("plan");
                if stored.is_empty() {
                    let vui = &plan.sps.vui_parameters;
                    println!(
                        "sps: level={:?} poc_type={} refs={} restriction={} reorder={} dpb={}",
                        plan.sps.level_idc,
                        plan.sps.pic_order_cnt_type,
                        plan.sps.max_num_ref_frames,
                        vui.bitstream_restriction_flag,
                        vui.max_num_reorder_frames,
                        vui.max_dec_frame_buffering,
                    );
                }
                stored.push(plan.dpb.stored.expect("stored"));
                for shown in &plan.dpb.outputs {
                    let decoded_at = stored.iter().position(|id| id == shown).expect("known");
                    lags.push(stored.len() - 1 - decoded_at);
                }
            }
        }
        enc.flush().ok();
        println!("{} AUs planned, output lags {lags:?}", stored.len());
        assert!(stored.len() >= 30, "only {} AUs produced", stored.len());
        assert_eq!(lags, vec![0; stored.len()]);
    }

    /// Hardware: drop with encodes in flight, then a fresh session must still open.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_dirty_teardown_reopen() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        for round in 0..3 {
            let mut enc = open_h265();
            for f in 0..4u32 {
                let frame = nv12_frame(W, H, f);
                enc.submit_indexed(&frame, f)
                    .unwrap_or_else(|e| panic!("round {round} submit {f} failed: {e:#}"));
            }
            drop(enc); // pending encodes still in flight
        }
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0)
            .expect("reopen after dirty teardowns");
        while enc.poll().expect("poll").is_some() {}
        println!("nvenc_cuda dirty-teardown: 3 dirty drops, reopen clean");
    }

    /// Hardware: exhaust the concurrent-session cap, assert open fails, free slots, rebuild
    /// in place and produce an AU.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_open_failure_diagnosis_and_recovery() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        try_api().expect("nvenc api");
        let shared = cuda::context().expect("shared ctx");

        let open_raw = |device: *mut c_void| -> (nv::NVENCSTATUS, *mut c_void) {
            let mut params = nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS {
                version: nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER,
                deviceType: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_CUDA,
                device,
                apiVersion: nv::NVENCAPI_VERSION,
                ..Default::default()
            };
            let mut enc: *mut c_void = ptr::null_mut();
            // SAFETY: live params / out-param across the sync call.
            let st = unsafe { (api().open_encode_session_ex)(&mut params, &mut enc) };
            (st, enc)
        };

        // Hold sessions until open fails.
        let mut held = Vec::new();
        loop {
            let (st, enc) = open_raw(shared);
            if st != nv::NVENCSTATUS::NV_ENC_SUCCESS {
                if !enc.is_null() {
                    // SAFETY: destroy failed-open residue (NVENC docs).
                    unsafe {
                        let _ = (api().destroy_encoder)(enc);
                    }
                }
                break;
            }
            held.push(enc);
        }
        assert!(!held.is_empty(), "expected a finite session cap");

        // Caps-probe open must fail while the cap is exhausted.
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        let err = enc
            .submit_indexed(&frame, 0)
            .expect_err("submit must fail while the cap is exhausted");
        println!("at-cap error (self-diagnosis logged alongside): {err:#}");

        // Slots freed → same encoder rebuilds in place.
        for e in held {
            // SAFETY: successful raw open; destroy once.
            unsafe {
                let _ = (api().destroy_encoder)(e);
            }
        }
        assert!(enc.reset(), "in-place reset must be available");
        let frame = nv12_frame(W, H, 1);
        enc.submit_indexed(&frame, 1)
            .expect("rebuild after the transient cleared");
        let mut got = false;
        while enc.poll().expect("poll").is_some() {
            got = true;
        }
        assert!(got, "recovered encoder must produce an AU");
        println!("nvenc_cuda open-failure recovery: cap hit → diagnosed → recovered in place");
    }

    /// Hardware: stream-ordered submit must arm on a default-env session. A silent fallback
    /// still encodes — no other test would notice.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_stream_ordered_arms() {
        const W: u32 = 640;
        const H: u32 = 360;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Operator opt-out / two-thread mode: skip, don't fail.
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(au.keyframe, "opening AU must be the session IDR");
        assert!(
            enc.stream_ordered,
            "IO-stream binding must arm on a default-env session (NvEncSetIOCudaStreams rejected?)"
        );
        assert!(
            !enc.io_stream.is_null(),
            "the boxed CUstream must be held while armed"
        );
    }

    /// Queue a convert-timeline wait nothing will signal, as a worker that died mid-pass leaves.
    fn wedge_copy_stream(enc: &mut NvencCudaEncoder) {
        const NEVER: u64 = 1_000;
        let mut worker = pf_zerocopy::Importer::new_for_capture().expect("in-process importer");
        let fd = worker.convert_timeline().expect("convert timeline fd");
        let sem = cuda::ExternalSemaphore::import_timeline_fd(fd).expect("import the timeline");
        sem.wait(NEVER).expect("queue the wait");
        enc.worker = Some(worker);
        enc.convert_sem = Some(sem);
        enc.convert_waited = NEVER;
        assert!(
            cuda::copy_stream_sync_deadline(std::time::Duration::from_millis(50)).is_err(),
            "the unsignalled wait must hold the copy stream"
        );
    }

    /// Hardware: a stuck fused-pass wait on the stream NVENC is bound to. Releasing the
    /// semaphore retires the worker and frees the stream; the session keeps encoding, and a
    /// teardown over a second stuck wait returns.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_stuck_fused_wait_releases() {
        const W: u32 = 640;
        const H: u32 = 360;
        set_env("PUNKTFUNK_ZEROCOPY_INPROC", "1");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        enc.submit_indexed(&nv12_frame(W, H, 0), 0).expect("submit");
        enc.poll().expect("poll").expect("AU");
        assert!(
            enc.stream_ordered,
            "IO streams must be bound to the copy stream"
        );

        wedge_copy_stream(&mut enc);
        let t = std::time::Instant::now();
        enc.release_convert_sem();
        assert!(
            t.elapsed() < std::time::Duration::from_secs(2),
            "release must not hang"
        );
        assert!(enc.convert_sem.is_none() && enc.worker.is_none());
        cuda::copy_stream_sync_deadline(std::time::Duration::from_millis(100))
            .expect("the copy stream drains once the wait is satisfied");
        for i in 1..5 {
            enc.submit_indexed(&nv12_frame(W, H, i), i)
                .expect("submit after release");
            enc.poll().expect("poll").expect("AU after release");
        }

        wedge_copy_stream(&mut enc);
        let t = std::time::Instant::now();
        drop(enc);
        assert!(
            t.elapsed() < std::time::Duration::from_secs(3),
            "teardown must not hang"
        );
        remove_env("PUNKTFUNK_ZEROCOPY_INPROC");
    }

    /// Hardware: cursor frames stay on the stream-ordered path (`blend_ref_ordered`, ticket
    /// +2 per frame), including a bitmap change and per-frame moves.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_cursor_blend_stream_ordered() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        // Operator opt-out / two-thread mode: skip, don't fail.
        if !stream_ordered_requested() || async_retrieve_requested() {
            println!("skipped: stream-ordered submit disabled by env");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            true, // Vulkan slot ring + blend
            4,
        )
        .expect("open NVENC CUDA session");
        let cursor = |serial: u64, x: i32, y: i32| pf_frame::CursorOverlay {
            x,
            y,
            w: 32,
            h: 32,
            rgba: std::sync::Arc::new(vec![0xFF; 32 * 32 * 4]),
            serial,
            hot_x: 0,
            hot_y: 0,
            visible: true,
        };
        let mut aus = 0usize;
        for i in 0..6u32 {
            let mut frame = nv12_frame(W, H, i);
            // Serial flip at frame 3 (upload quiesce); position moves every frame.
            frame.cursor = Some(cursor(
                if i < 3 { 1 } else { 2 },
                40 + i as i32 * 9,
                60 + i as i32 * 5,
            ));
            enc.submit_indexed(&frame, i).expect("submit cursor frame");
            while enc.poll().expect("poll").is_some() {
                aus += 1;
            }
        }
        assert_eq!(aus, 6, "every cursor frame must deliver an AU");
        assert!(
            enc.stream_ordered,
            "IO-stream binding must arm on a default-env session"
        );
        let vk = enc
            .vk_blend
            .as_ref()
            .expect("Vulkan slot blend must come up on an RTX box");
        assert!(
            vk.ordered_ready(),
            "timeline semaphore must export to CUDA on this driver"
        );
        assert_eq!(
            vk.ordered_ticket(),
            12,
            "all 6 cursor blends must take the ordered path (2 timeline values each)"
        );
        println!(
            "nvenc_cuda cursor stream-ordered: 6 cursor AUs, ticket={}",
            vk.ordered_ticket()
        );
    }

    /// Hardware: `set_pipelined(true)` rebuilds without IO-stream binding, spawns the
    /// retrieve thread, keeps delivering AUs. First post-escalation AU is the re-open IDR.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_pipelined_escalation() {
        const W: u32 = 1280;
        const H: u32 = 720;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        if async_retrieve_vetoed() {
            println!("skipped: PUNKTFUNK_NVENC_ASYNC=0 vetoes the escalation");
            return;
        }
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            8_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");
        for i in 0..3u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            enc.poll().expect("poll").expect("AU");
        }
        assert!(!enc.s.retrieving(), "session starts sync");
        assert!(enc.set_pipelined(true), "escalation must be accepted");
        let mut aus = 0usize;
        let mut first_key = false;
        for i in 3..13u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i)
                .expect("submit post-escalation");
            while let Some(au) = enc.poll().expect("poll") {
                if aus == 0 {
                    first_key = au.keyframe;
                }
                aus += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(3));
        }
        // Bounded drain of the pipelined tail.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        while aus < 10 && std::time::Instant::now() < deadline {
            if enc.poll().expect("poll").is_some() {
                aus += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            enc.s.retrieving(),
            "retrieve thread must be live after escalation"
        );
        assert!(
            !enc.stream_ordered,
            "IO-stream binding must be gone in pipelined mode"
        );
        assert_eq!(aus, 10, "every post-escalation frame must deliver an AU");
        assert!(first_key, "first post-escalation AU is the re-open IDR");
    }

    /// Hardware: do slices become readable mid-encode? Prints a doNotWait timeline; asserts
    /// only that 4 slices materialize. `--test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_subframe_slice_probe() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                remove_env("PUNKTFUNK_NVENC_SLICES");
                remove_env("PUNKTFUNK_NVENC_SUBFRAME");
            }
        }
        set_env("PUNKTFUNK_NVENC_SLICES", "4");
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "1");
        let _guard = EnvGuard;

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit opening frame");
        enc.poll().expect("poll").expect("opening AU");

        // Spin doNotWait against the in-flight bitstream before the blocking poll.
        let frame = nv12_frame(W, H, 1);
        enc.submit_indexed(&frame, 1).expect("submit probed frame");
        let bs = enc.s.pending().back().expect("in-flight entry").bs;
        let mut offsets = vec![0u32; slice_offsets_len(W, H)];
        let t0 = std::time::Instant::now();
        let mut timeline: Vec<(u64, nv::NVENCSTATUS, u32, u32)> = Vec::new();
        loop {
            // SAFETY: live session; `bs` is the just-submitted bitstream. `offsets` is sized for
            // this frame. The guard unlocks before the next iteration.
            let lock =
                unsafe { BitstreamLock::new(api(), enc.s.handle(), bs, true, Some(&mut offsets)) };
            let (status, n, bytes) = match lock {
                Ok(l) => (
                    nv::NVENCSTATUS::NV_ENC_SUCCESS,
                    l.info().numSlices,
                    l.info().bitstreamSizeInBytes,
                ),
                Err(st) => (st, 0, 0),
            };
            let t_us = t0.elapsed().as_micros() as u64;
            timeline.push((t_us, status, n, bytes));
            // Complete = 4 slices. LOCK_BUSY = still encoding. 50 ms safety window.
            if (status == nv::NVENCSTATUS::NV_ENC_SUCCESS && n >= 4) || t_us > 50_000 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_micros(50));
        }
        println!("subframe probe timeline (t_us, status, numSlices, bytes):");
        for (t, st, n, b) in &timeline {
            println!("  {t:>7} us  {st:?}  slices={n}  bytes={b}");
        }
        // Normal poll: probe locks must not have corrupted the session.
        let au = enc.poll().expect("poll probed frame").expect("probed AU");
        assert!(!au.data.is_empty(), "probed AU must carry data");
        let last = timeline.last().expect("at least one sample");
        assert_eq!(
            last.2, 4,
            "4 slices must materialize (PUNKTFUNK_NVENC_SLICES=4 + subframe readback armed)"
        );
        // One more frame — session still healthy.
        let frame = nv12_frame(W, H, 2);
        enc.submit_indexed(&frame, 2).expect("submit follow-up");
        enc.poll().expect("poll").expect("follow-up AU");
    }

    /// Annex-B NAL start code.
    fn starts_with_start_code(d: &[u8]) -> bool {
        d.starts_with(&[0, 0, 0, 1]) || d.starts_with(&[0, 0, 1])
    }

    /// Hardware: chunked poll at defaults (4 slices + sub-frame). First/last metadata, Annex-B
    /// cuts, shadow reassembly. At least one multi-chunk frame. `--test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_chunked_poll_end_to_end() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        // Defaults under test — no leaked knobs.
        remove_env("PUNKTFUNK_NVENC_SLICES");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");

        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            4,
        )
        .expect("open NVENC CUDA session");

        let mut multi_chunk_frames = 0usize;
        let mut total_chunks = 0usize;
        for i in 0..6u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            assert!(
                enc.supports_chunked_poll(),
                "4 slices + subframe on a sync session must arm chunked poll"
            );
            let mut au = Vec::new();
            let mut chunks = 0usize;
            loop {
                let c = enc
                    .poll_chunk()
                    .expect("poll_chunk")
                    .expect("an AU is in flight — poll_chunk must block, never None");
                if chunks == 0 {
                    assert!(c.first, "the first chunk must open the AU");
                    assert_eq!(
                        c.keyframe,
                        i == 0,
                        "only the session-opening frame is an IDR"
                    );
                }
                assert_eq!(c.pts_ns, i as u64 * 16_666_667, "pts rides every chunk");
                assert!(!c.recovery_anchor, "no RFI happened");
                if !c.data.is_empty() {
                    assert!(
                        starts_with_start_code(&c.data),
                        "chunk cut must land on an Annex-B start code (frame {i}, chunk {chunks})"
                    );
                }
                au.extend_from_slice(&c.data);
                chunks += 1;
                if c.last {
                    break;
                }
            }
            assert!(!au.is_empty(), "frame {i} produced an empty AU");
            assert!(
                enc.s.chunk.is_none(),
                "chunk state must be cleared once the AU closes"
            );
            if chunks > 1 {
                multi_chunk_frames += 1;
            }
            total_chunks += chunks;
            println!("frame {i}: {chunks} chunks, {} bytes", au.len());
        }
        assert!(
            multi_chunk_frames >= 1,
            "sub-frame readback yielded no multi-chunk frame — incremental slice readback \
             regressed (the probe shows ~200 µs slice spacing on this GPU)"
        );
        println!(
            "nvenc_cuda chunked poll: {total_chunks} chunks over 6 frames, \
             {multi_chunk_frames} frames chunked"
        );

        // A drained chunked AU leaves `poll()` usable.
        let frame = nv12_frame(W, H, 6);
        enc.submit_indexed(&frame, 6)
            .expect("submit plain-poll frame");
        let au = enc.poll().expect("poll").expect("AU");
        assert!(!au.data.is_empty());
    }

    /// Hardware: client `max_slices=1` must encode single-slice with chunked poll disarmed.
    /// No env knobs. `--test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_single_slice_client_ceiling() {
        const W: u32 = 1920;
        const H: u32 = 1080;
        // Negotiated ceiling, not the operator override.
        remove_env("PUNKTFUNK_NVENC_SLICES");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");
        let mut enc = NvencCudaEncoder::open(
            Codec::H265,
            PixelFormat::Nv12,
            W,
            H,
            60,
            20_000_000,
            true,
            8,
            false,
            ChromaFormat::Yuv420,
            false,
            1, // client never advertised multi-slice
        )
        .expect("open NVENC CUDA session");
        for i in 0..4u32 {
            let frame = nv12_frame(W, H, i);
            enc.submit_indexed(&frame, i).expect("submit");
            assert_eq!(
                enc.s.slices, 1,
                "a 1-slice client ceiling must clamp the Phase-3 default"
            );
            assert!(
                !enc.supports_chunked_poll(),
                "single-slice sessions have no boundaries — chunked poll must stay disarmed"
            );
            let au = enc.poll().expect("poll").expect("one AU per sync frame");
            assert!(!au.data.is_empty(), "frame {i} produced an empty AU");
        }
    }

    /// Hardware: `PUNKTFUNK_NVENC_SLICES=1` disarms chunked poll; `poll_chunk` is one
    /// self-closing whole-AU chunk. `--test-threads=1`.
    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run manually on the RTX box (.21)"]
    fn nvenc_cuda_chunked_poll_fallback_whole_au() {
        const W: u32 = 1280;
        const H: u32 = 720;
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                remove_env("PUNKTFUNK_NVENC_SLICES");
                remove_env("PUNKTFUNK_NVENC_SUBFRAME");
            }
        }
        let _guard = EnvGuard;
        pf_zerocopy::cuda::make_current().expect("shared CUDA context current");

        // Explicit single slice — no boundaries.
        set_env("PUNKTFUNK_NVENC_SLICES", "1");
        remove_env("PUNKTFUNK_NVENC_SUBFRAME");
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit");
        assert!(
            !enc.supports_chunked_poll(),
            "PUNKTFUNK_NVENC_SLICES=1 → chunked poll must not arm"
        );
        let c = enc
            .poll_chunk()
            .expect("poll_chunk")
            .expect("whole-AU chunk");
        assert!(c.first && c.last, "fallback chunk must be self-closing");
        assert!(c.keyframe, "opening AU is the session IDR");
        assert!(!c.data.is_empty());
        assert!(
            enc.poll_chunk().expect("poll_chunk").is_none(),
            "nothing in flight → None"
        );
        drop(enc);

        // Sub-frame vetoed: slices stay, chunked poll disarms, plain `poll` carries.
        remove_env("PUNKTFUNK_NVENC_SLICES");
        set_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        let mut enc = open_h265();
        let frame = nv12_frame(W, H, 0);
        enc.submit_indexed(&frame, 0).expect("submit");
        assert!(
            !enc.supports_chunked_poll(),
            "PUNKTFUNK_NVENC_SUBFRAME=0 → chunked poll must not arm"
        );
        let au = enc.poll().expect("poll").expect("AU");
        assert!(au.keyframe && !au.data.is_empty());
    }
}
