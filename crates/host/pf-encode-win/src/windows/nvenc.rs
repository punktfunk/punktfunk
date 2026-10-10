//! Direct-SDK NVENC encoder (Windows, D3D11 input): zero-copy capture→encode on the GPU.
//!
//! Raw `nvEncodeAPI` through a runtime-loaded [`EncodeApi`]. The crate's `ENCODE_API` /
//! safe `Encoder` stay unused: they are CUDA-only and their static entry points would
//! import `nvEncodeAPI64.dll` at load on the all-vendor binary. The session itself is the
//! shared [`NvSession`]; this file owns the DLL, the D3D11 device, texture registration, the
//! completion events, and the session budget. Design: `design/linux-direct-nvenc.md`;
//! recovery: `encoder-recovery-hardening.md`.
//!
//! The session binds the DXGI capturer's `ID3D11Device` and registers each input texture
//! once (cached by pointer); `encode_picture` uses it in place. That holds while the host
//! loop is capture → submit → poll. A pipelined loop must hand a texture ring.
//!
//! Two-thread retrieve (`PUNKTFUNK_NVENC_ASYNC=1`): the encode thread submits; a retrieve
//! thread waits on per-buffer events and `nvEncLockBitstream`. `submit` blocks on the
//! oldest completion when `POOL - 1` encodes are in flight. Register/map/unmap stay on
//! the encode thread. The DLL resolves at runtime, so an AMD/Intel box fails [`try_api`]
//! and AMF/QSV/software carry the session.

// `unsafe_op_in_unsafe_fn` off: this file is raw NVENC/D3D11 calls. Wrapping each one
// would add a SAFETY that only restates the prototype. Exit: delete the empty markers.
#![allow(unsafe_op_in_unsafe_fn)]

use super::nvenc_core::{
    codec_guid, CreateInstance, EncodeApi, GetMaxSupportedVersion, NvStatusExt,
};
use super::nvenc_session::{
    async_inflight_cap, async_retrieve_requested, full_chroma_input, lock_copy, open_session,
    EncodeInput, NvSession, OpenTarget, RetrieveDone, RetrieveJob, POOL,
};
use super::nvenc_status;
use super::{AuChunk, ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::mpsc;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;

// Runtime-loaded NVENC entry table. A link-time import of `nvEncodeAPI64.dll` would
// refuse to start on AMD/Intel before `main`. Only the two DLL exports resolve by
// name; `NvEncodeAPICreateInstance` fills the rest.

/// Resolve the table once per process. `Err` = no NVIDIA driver/DLL or a driver older than our
/// headers. [`NvencD3d11Encoder::open`] and [`probe_can_encode_444`] gate on it.
fn try_api() -> std::result::Result<&'static EncodeApi, &'static str> {
    static TABLE: std::sync::OnceLock<std::result::Result<EncodeApi, String>> =
        std::sync::OnceLock::new();
    TABLE
        .get_or_init(|| {
            let table = load_api();
            if let Err(e) = &table {
                // Misdetect, or `PUNKTFUNK_ENCODER=nvenc` without a driver.
                tracing::warn!(error = %e, "NVENC API unavailable");
            }
            table
        })
        .as_ref()
        .map_err(|e| e.as_str())
}

/// Loaded table for call sites past a [`try_api`] gate. Lives for the process lifetime.
fn api() -> &'static EncodeApi {
    try_api().expect("NVENC call before a successful try_api() gate")
}

fn load_api() -> std::result::Result<EncodeApi, String> {
    use windows::core::{s, w};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };
    // SAFETY: `LoadLibraryExW`/`GetProcAddress` take static NUL-terminated names;
    // `LOAD_LIBRARY_SEARCH_SYSTEM32` excludes a planted DLL. The transmutes are the
    // `nvEncodeAPI.h` prototypes. The module is never freed.
    unsafe {
        let module = LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
            .map_err(|e| format!("nvEncodeAPI64.dll not loadable (no NVIDIA driver?): {e}"))?;
        let get_version = GetProcAddress(module, s!("NvEncodeAPIGetMaxSupportedVersion"))
            .ok_or("nvEncodeAPI64.dll exports no NvEncodeAPIGetMaxSupportedVersion")?;
        let create_instance = GetProcAddress(module, s!("NvEncodeAPICreateInstance"))
            .ok_or("nvEncodeAPI64.dll exports no NvEncodeAPICreateInstance")?;
        let get_version: GetMaxSupportedVersion = std::mem::transmute(get_version);
        let create_instance: CreateInstance = std::mem::transmute(create_instance);
        EncodeApi::from_exports(get_version, create_instance)
    }
}

/// Live NVENC session units in this process (plain = 1; forced split = one per engine, 2–3).
/// Admission reads this same counter; other processes are invisible, so we fail closed on our own.
static LIVE_SESSION_UNITS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Concurrent-session budget (GeForce 8; pro cards unlimited).
/// `PUNKTFUNK_NVENC_MAX_SESSIONS` overrides.
fn session_cap() -> u32 {
    match crate::knobs::get().nvenc_max_sessions {
        0 => 8,
        n => u32::from(n),
    }
}

/// Whether one more plain (non-split) session fits. AMD/Intel never open NVENC so this passes.
pub fn can_open_another_session() -> bool {
    LIVE_SESSION_UNITS.load(std::sync::atomic::Ordering::Relaxed) < session_cap()
}

fn split_mode_units(split_mode: u32) -> u32 {
    match split_mode {
        m if m == nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_THREE_FORCED_MODE as u32 => 3,
        m if m == nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_TWO_FORCED_MODE as u32
            || m == nv::NV_ENC_SPLIT_ENCODE_MODE::NV_ENC_SPLIT_AUTO_FORCED_MODE as u32 =>
        {
            2
        }
        _ => 1,
    }
}

/// Serializes every `nvEncOpenEncodeSessionEx` against [`reap_parked_sessions`]. Overlapping a
/// reap could recycle a zombie's address onto a new session, and the reap would destroy the live
/// one. Held across `init_session` and the standalone probes; admission never takes it.
static DRIVER_SESSION_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Session whose `destroy_encoder` failed ambiguously ([`nvenc_status::destroy_proves_no_session`]).
/// Units stay charged in [`LIVE_SESSION_UNITS`] until [`reap_parked_sessions`] proves the slot free.
struct ParkedSession {
    enc: usize,
    units: u32,
    /// Pins the D3D11 device the session opened against. Teardown drops the texture refs; without
    /// this, a later reap `destroy_encoder` could touch a freed device.
    _device: Option<ID3D11Device>,
}

// SAFETY: the COM ref is the only non-Send field. Capture creates the D3D11 device without
// `D3D11_CREATE_DEVICE_SINGLETHREADED` (pf-frame/src/dxgi.rs), we never call methods on it, and
// Release of a free-threaded COM object from another thread is sound. `enc` is only passed to
// `destroy_encoder` under [`PARKED`] with [`DRIVER_SESSION_GATE`] held.
unsafe impl Send for ParkedSession {}

static PARKED: std::sync::Mutex<Vec<ParkedSession>> = std::sync::Mutex::new(Vec::new());

/// Park a session whose destroy failed ambiguously. Units stay charged until a reap
/// proves the slot free. Age out the oldest entry (and refund) if parked units would
/// exceed the session cap.
fn park_session(enc: usize, units: u32, device: Option<ID3D11Device>) {
    let mut parked = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    while !parked.is_empty() && parked.iter().map(|z| z.units).sum::<u32>() + units > session_cap()
    {
        let old = parked.remove(0);
        LIVE_SESSION_UNITS.fetch_sub(old.units, std::sync::atomic::Ordering::Relaxed);
        tracing::warn!(
            enc = old.enc,
            units = old.units,
            "NVENC parked-session graveyard full — aging out the oldest entry (units refunded)"
        );
    }
    parked.push(ParkedSession {
        enc,
        units,
        _device: device,
    });
}

/// Retry `destroy_encoder` on parked sessions and refund units that now succeed (or prove gone).
/// Runs from `init_session` on the encode thread, never from admission (a wedged driver would
/// block the admission lock).
///
/// # Safety
/// Caller holds [`DRIVER_SESSION_GATE`] and no live NVENC session exists (checked: live units ==
/// parked units), so a recycled handle cannot alias a live session mid-reap. Residual, unprovable
/// from the SDK: a failed destroy leaves the handle intact for retry; NVENC documents neither way.
unsafe fn reap_parked_sessions() {
    let mut parked = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    if parked.is_empty() {
        return;
    }
    let parked_units: u32 = parked.iter().map(|z| z.units).sum();
    if LIVE_SESSION_UNITS.load(std::sync::atomic::Ordering::Relaxed) != parked_units {
        return; // a live session exists — its address space is off limits
    }
    parked.retain(
        |z| match (api().destroy_encoder)(z.enc as *mut c_void).nv_ok() {
            Ok(()) => {
                tracing::info!(
                    enc = z.enc,
                    units = z.units,
                    "NVENC parked session reclaimed — retry-destroy succeeded, budget refunded"
                );
                LIVE_SESSION_UNITS.fetch_sub(z.units, std::sync::atomic::Ordering::Relaxed);
                false
            }
            Err(e) if nvenc_status::destroy_proves_no_session(e) => {
                tracing::info!(
                    enc = z.enc,
                    units = z.units,
                    status = ?e,
                    "NVENC parked session gone on the driver side — budget refunded"
                );
                LIVE_SESSION_UNITS.fetch_sub(z.units, std::sync::atomic::Ordering::Relaxed);
                false
            }
            Err(e) => {
                tracing::debug!(enc = z.enc, status = ?e,
                    "NVENC parked session still refuses destroy — units stay charged");
                true
            }
        },
    );
}

/// Retrieve thread: wait on each job's completion event, lock/copy/unlock, send back.
/// Exits when the job channel closes. Teardown drops the sender and joins before
/// destroying the session, so `enc`/`bs`/`event` outlive every use. Touches only wait + lock/unlock.
fn retrieve_loop(
    enc: usize,
    work_rx: mpsc::Receiver<RetrieveJob>,
    done_tx: mpsc::Sender<RetrieveDone>,
) {
    pf_frame::thread_qos::boost_thread_priority(false);
    // After one 5 s timeout, later jobs wait 250 ms. Teardown must drain every queued
    // job (abandoning would destroy/unmap while encoding); the first timeout is already
    // encoder-fatal, so this only shortens teardown. A successful wait resets the latch.
    const WEDGED_DRAIN_WAIT_MS: u32 = 250;
    let mut wedged = false;
    while let Ok(job) = work_rx.recv() {
        let wait_ms = if wedged { WEDGED_DRAIN_WAIT_MS } else { 5000 };
        // SAFETY: `job.event` is an auto-reset event `init_session` registered; `job.bs` is
        // a pool bitstream. Both stay valid until `teardown` joins this thread first.
        // On WAIT_OBJECT_0 the encode is done; `lock_copy` copies the bytes, then unlocks.
        // Secondary-thread lock/unlock while the encode thread submits is the NVENC model.
        let result = unsafe {
            if WaitForSingleObject(HANDLE(job.event as *mut c_void), wait_ms) != WAIT_OBJECT_0 {
                wedged = true;
                Err(format!(
                    "NVENC completion event timeout ({wait_ms} ms) — encoder wedged?"
                ))
            } else {
                wedged = false;
                lock_copy(api(), enc, job.bs)
            }
        };
        if done_tx.send(RetrieveDone { bs: job.bs, result }).is_err() {
            break; // encoder gone; teardown drains us via join
        }
    }
}

pub struct NvencD3d11Encoder {
    /// The session state and per-frame logic shared with the Linux backend.
    s: NvSession,
    /// Negotiated 4:4:4, before per-init downgrade. `s.chroma_444` is the effective value and
    /// clears on subsampled YUV; keeping the request lets a later RGB re-init recover 4:4:4.
    chroma_444_requested: bool,
    /// HDR the capture format asks for. `s.hdr` is effective and a no-10-bit GPU clears it;
    /// comparing against it would rebuild the session every P010 frame.
    hdr_requested: bool,
    /// Latched when the caps probe finds no 10-bit encode, so re-inits do not re-warn.
    hdr_unsupported: bool,
    /// Capturer textures registered with NVENC, cached by pointer (in-place encode). The cloned
    /// `ID3D11Texture2D` keeps each alive until unregister — the capturer may drop its copy first.
    regs: HashMap<isize, (nv::NV_ENC_REGISTERED_PTR, ID3D11Texture2D)>,
    /// Async: completion event per pool bitstream (`HANDLE` as `usize`); empty in sync. Closed in `teardown`.
    events: Vec<usize>,
    /// Caller asked for completion events without a retrieve thread
    /// ([`Self::use_completion_events`]). Survives a rebuild; the session honours it only when
    /// the GPU advertises async encode and nobody asked for the two-thread mode.
    want_events: bool,
    /// Capturer `pipeline_depth`. Encode is in-place, so this hard-caps async depth: the capturer
    /// rotates the ring regardless of encode completion. `None` = unknown, do not pipeline past the env cap.
    input_ring_depth: Option<usize>,
    async_supported: bool,
    /// D3D11 device this session opened against. Capturer recreates it on a desktop switch; a new
    /// device pointer tears down and re-inits.
    init_device: *mut c_void,
    /// COM ref pinning that device. `init_device` alone pins nothing, and `teardown` releases
    /// texture regs before destroy — a parked retry must not outlive the device. Moved into
    /// [`ParkedSession`] on that path.
    init_device_com: Option<ID3D11Device>,
    /// Units this encoder holds against [`LIVE_SESSION_UNITS`] (1 plain, 2–3 if split). `0` while closed.
    session_units: u32,
}

// SAFETY: the `!Send` fields are the session's NVENC handles and pointers, the device pointer,
// and `ID3D11Texture2D` COM refs. One thread owns the encoder (every method runs there). In
// async mode the retrieve thread only waits/lock/unlock — never registrations, mappings, or
// D3D11 — and `teardown` joins it first. The ownership move is sound because no NVENC/D3D11
// call is in flight during it.
unsafe impl Send for NvencD3d11Encoder {}

/// The NVENC input format for a captured [`PixelFormat`]. Shared by `open` and the first
/// `submit` so the format `caps()` answers from is the one the session will init with.
const fn buffer_format(format: PixelFormat) -> nv::NV_ENC_BUFFER_FORMAT {
    match format {
        PixelFormat::P010 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_YUV420_10BIT,
        // Same packed layout for both: `Rgb10a2Sdr` is sRGB, so its session takes BT.709 VUI.
        PixelFormat::Rgb10a2 | PixelFormat::Rgb10a2Sdr => {
            nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR10
        }
        PixelFormat::Nv12 => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_NV12,
        _ => nv::NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB,
    }
}

/// Render-adapter LUID as the caches' GPU identity; `0` when unresolved.
fn luid_key(luid: Option<LUID>) -> u64 {
    luid.map_or(0, |l| ((l.HighPart as u32 as u64) << 32) | l.LowPart as u64)
}

impl NvencD3d11Encoder {
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
        // Client-decoder slice ceiling (`VIDEO_CAP_MULTI_SLICE` / GameStream slices-per-frame).
        // 1 = single-slice — the safe shape toward decoders that never asked (some SoCs wedge).
        max_slices: u32,
        // Selected render adapter (`None` = OS default). Only the probe/cache keys read it;
        // the session device comes from the first submitted frame.
        adapter_luid: Option<LUID>,
    ) -> Result<Self> {
        // DLL load is the real availability gate: fail open with a reason instead of an opaque
        // first-frame session error. Later NVENC calls sit behind this, so `api()` is sound.
        let api = try_api().map_err(|e| anyhow!("NVENC unavailable: {e}"))?;
        // 4:4:4 is HEVC-only; the GPU-support gate is in the caps probe.
        let want_444 = chroma.is_444() && codec == Codec::H265;
        let mut s = NvSession::new(api, codec, width, height, fps, bitrate_bps, max_slices);
        s.buffer_fmt = buffer_format(format);
        s.bit_depth = bit_depth;
        // Effective from the open, not from the first frame: callers read `caps()` to
        // answer the client, and a subsampled input never becomes 4:4:4 later.
        s.chroma_444 = want_444 && full_chroma_input(s.buffer_fmt);
        s.gpu = luid_key(adapter_luid);
        Ok(Self {
            s,
            chroma_444_requested: want_444,
            hdr_requested: false,
            hdr_unsupported: false,
            regs: HashMap::new(),
            events: Vec::new(),
            want_events: false,
            input_ring_depth: None,
            async_supported: false,
            init_device: ptr::null_mut(),
            init_device_com: None,
            session_units: 0,
        })
    }

    /// Tear down the session and pooled resources. Reused on a capture-device change and at Drop.
    unsafe fn teardown(&mut self) {
        if self.s.handle().is_null() {
            return;
        }
        // Joins the retrieve thread (it finishes queued jobs against the still-live session)
        // and unmaps in-flight inputs; only then is unregister/destroy sound.
        self.s.stop();
        let enc = self.s.handle();
        for (reg, _tex) in self.regs.values() {
            let _ = (api().unregister_resource)(enc, *reg);
        }
        for &ev in &self.events {
            let mut ep = nv::NV_ENC_EVENT_PARAMS {
                version: nv::NV_ENC_EVENT_PARAMS_VER,
                completionEvent: ev as *mut c_void,
                ..Default::default()
            };
            let _ = (api().unregister_async_event)(enc, &mut ep);
            let _ = CloseHandle(HANDLE(ev as *mut c_void));
        }
        self.events.clear();
        // Refund units only when the driver proves the slot free (success or a gone-session
        // status). Ambiguous failures park the handle with units still charged;
        // `reap_parked_sessions` retries once nothing is live.
        let dev_pin = self.init_device_com.take();
        let units = self.session_units;
        self.s.close(|enc| match (api().destroy_encoder)(enc).nv_ok() {
            Ok(()) => {
                LIVE_SESSION_UNITS.fetch_sub(units, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) if nvenc_status::destroy_proves_no_session(e) => {
                tracing::warn!(
                    status = ?e,
                    "NVENC destroy_encoder failed, but the status proves the driver holds no \
                     session (device gone/reset) — budget refunded"
                );
                LIVE_SESSION_UNITS.fetch_sub(units, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                tracing::warn!(
                    status = ?e,
                    units,
                    "NVENC destroy_encoder failed ambiguously — the driver may still hold this \
                     session's slot; parking the handle (units stay charged until a reap-destroy \
                     proves the slot free)"
                );
                park_session(enc as usize, units, dev_pin);
            }
        });
        self.session_units = 0;
        self.regs.clear();
    }

    /// Lazily create the session on the first frame's D3D11 device so capture and encode share it.
    fn init_session(&mut self, device: &ID3D11Device) -> Result<()> {
        // Serialize this open (caps + clamp + charge) against other opens and the zombie reap.
        let _gate = DRIVER_SESSION_GATE
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // SAFETY: gate held (no open can be handed a recycled zombie address mid-reap) and the
        // reap itself re-checks that no live session exists before touching any parked handle.
        unsafe { reap_parked_sessions() };
        let target = OpenTarget {
            device_type: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
            device: device.as_raw(),
        };
        // SAFETY: `target` names the live `device` this call borrows, and the session is closed
        // (`prepare_d3d11` tore it down). `register_async_event` takes the live session and a
        // version-set local; each event is pushed to `self.events` before it registers.
        unsafe {
            let caps = self.s.query_caps(target)?;
            if self.s.bit_depth >= 10 && !caps.ten_bit {
                if !self.hdr_unsupported {
                    tracing::warn!(
                        "NVENC: this GPU can't 10-bit encode — falling back to 8-bit SDR"
                    );
                }
                // Latch so `submit` compares against `hdr_requested`, not this cleared value.
                self.hdr_unsupported = true;
                self.s.bit_depth = 8;
                self.s.hdr = false;
            }
            self.async_supported = caps.async_encode;
            let split_mode = self.s.resolve_shape();
            // Two async shapes share one init flag and one event per bitstream: the operator's
            // two-thread retrieve, and the caller's event-driven same-thread retrieve. The
            // retrieve thread is what separates them, and only the two-thread mode has one.
            let two_thread = self.async_supported && async_retrieve_requested();
            self.s.session_async = two_thread || (self.async_supported && self.want_events);
            self.s.open(target, split_mode)?;
            let enc = self.s.handle();
            // Pin the device for the session lifetime and a parked afterlife if destroy fails.
            self.init_device_com = Some(device.clone());
            // Charge what this open holds so admission can decline a parallel display. Weighted
            // by the final split mode (one hardware session per engine).
            self.session_units = split_mode_units(self.s.split_mode);
            LIVE_SESSION_UNITS.fetch_add(self.session_units, std::sync::atomic::Ordering::Relaxed);
            // No encoder-owned input pool: capturer textures are registered on demand in
            // `submit` and encoded in place.
            self.s.create_bitstreams()?;
            // One auto-reset completion event per pool bitstream. In the two-thread mode the
            // retrieve thread waits on them and only ever sees raw addresses (`teardown` joins
            // it before any of them die); in the event mode the caller parks on them itself
            // through [`Encoder::ready_event`] and this thread still does the lock/copy.
            if self.s.session_async {
                for _ in 0..POOL {
                    let ev = CreateEventW(None, false, false, PCWSTR::null())
                        .context("CreateEvent (NVENC completion)")?;
                    // Push before registering: teardown only closes handles already in
                    // `self.events`. Unregister of a never-registered event is harmless.
                    self.events.push(ev.0 as usize);
                    let mut ep = nv::NV_ENC_EVENT_PARAMS {
                        version: nv::NV_ENC_EVENT_PARAMS_VER,
                        completionEvent: ev.0,
                        ..Default::default()
                    };
                    (api().register_async_event)(enc, &mut ep)
                        .nv_ok()
                        .map_err(|e| nvenc_status::call_err("register_async_event", e))?;
                }
            }
            if two_thread {
                self.s
                    .start_retrieve("punktfunk-nvenc-out", retrieve_loop)?;
                tracing::info!(
                    pool = POOL,
                    "NVENC async retrieve active (two-thread encode: submit here, \
                     lock_bitstream on the retrieve thread)"
                );
            } else if self.s.session_async {
                tracing::info!(
                    pool = POOL,
                    "NVENC completion events active (single-thread: the caller parks on \
                     ready_event, poll locks here)"
                );
            }
            let s = &self.s;
            tracing::info!(
                // `split_mode` is the final mode (post-fallback) and `engines` the ceiling it was
                // chosen from — either alone is ambiguous. `subframe` because AUTO + sub-frame
                // is a single-engine combination that reads like a split in a log.
                split_mode = s.split_mode,
                engines = s.encoder_engines,
                subframe = s.subframe_on,
                "NVENC D3D11 session: {}x{}@{} {}-bit{} {} Mbps {:?}",
                s.width,
                s.height,
                s.fps,
                s.bit_depth,
                if s.hdr { " HDR(BT.2020 PQ)" } else { "" },
                s.bitrate_bps / 1_000_000,
                s.codec_guid
            );
            self.s.finish_open();
            Ok(())
        }
    }

    /// Build the encode session now, so [`caps`](Encoder::caps) describes the hardware.
    ///
    /// [`submit`](Encoder::submit) calls this on its own, but a caller that reads caps before
    /// the first frame would otherwise get the struct defaults — `supports_rfi: false` on a
    /// card that does support reference-picture invalidation, which costs every later loss a
    /// full IDR. `device` must be the one the frames will arrive on, or the first submit
    /// rebuilds the session.
    ///
    /// Open the session in async mode and expose its per-bitstream completion events through
    /// [`Encoder::ready_event`], with no retrieve thread: `poll` stays the same-thread lock/copy
    /// the sync path uses, immediate once the event has fired. For a caller whose loop already
    /// parks on handles (the Windows driver's encode thread) — a two-thread retrieve there would
    /// only add a queue. Ignored on a GPU without `NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT`, and by
    /// `PUNKTFUNK_NVENC_ASYNC=1`, which asks for the two-thread mode instead. Call before the
    /// session opens.
    pub fn use_completion_events(&mut self, on: bool) {
        self.want_events = on;
    }

    /// Idempotent: a repeat call with the same device, format and size keeps the session.
    pub fn prepare_d3d11(
        &mut self,
        device: &ID3D11Device,
        format: PixelFormat,
        width: u32,
        height: u32,
    ) -> Result<()> {
        // Capturer recreates its D3D11 device on a desktop switch and may return a different
        // resolution. Re-init on a different device or size. HDR (BT.2020 PQ) when the capturer
        // hands a 10-bit frame (Rgb10a2 or P010); 8-bit NV12/ARGB is SDR. Can flip mid-session.
        let hdr = matches!(format, PixelFormat::Rgb10a2 | PixelFormat::P010);
        let dev_raw = device.as_raw();
        let inited = self.s.inited();
        let size_changed = inited && (self.s.width != width || self.s.height != height);
        // Compare against last-init REQUEST, not effective `s.hdr`: on a no-10-bit GPU
        // the caps probe clears `s.hdr`, so a P010 capturer would rebuild every frame.
        let hdr_changed = inited && self.hdr_requested != hdr;
        if inited && (self.init_device != dev_raw || size_changed || hdr_changed) {
            tracing::info!(
                device_changed = self.init_device != dev_raw,
                size_changed,
                hdr_changed,
                hdr,
                new = format!("{width}x{height}"),
                "NVENC: capture device/size/HDR changed — re-initializing session"
            );
            // SAFETY: `teardown` needs the encode thread with no NVENC call in flight and a session
            // whose cached regs/bitstreams/pending belong to it. All hold: this is the encode
            // thread, the session is open, and the previous frame's encode has already been polled.
            unsafe { self.teardown() };
        }
        if !self.s.inited() {
            self.s.width = width;
            self.s.height = height;
            self.hdr_requested = hdr;
            // Effective until the caps probe clears it on a card without 10-bit.
            self.s.hdr = hdr;
            // Recompute effective 4:4:4 from the negotiation. Overwriting `chroma_444` on the
            // first subsampled-YUV frame would permanently demote the session.
            self.s.chroma_444 = self.chroma_444_requested;
            // YUV (NV12/P010): native encode, no RGB→YUV CSC. RGB is the shader path.
            // 10-bit forces Main10; NV12 pins 8 — `register_resource` rejects it in a
            // 10-bit session, unlike ARGB.
            self.s.buffer_fmt = buffer_format(format);
            match format {
                PixelFormat::P010 | PixelFormat::Rgb10a2 | PixelFormat::Rgb10a2Sdr => {
                    self.s.bit_depth = 10;
                }
                PixelFormat::Nv12 => self.s.bit_depth = 8,
                _ => {}
            }
            // Clear the effective flag so `caps().chroma_444` reports what the stream carries;
            // keep `chroma_444_requested` so a later RGB re-init recovers 4:4:4.
            if self.s.chroma_444 && !full_chroma_input(self.s.buffer_fmt) {
                tracing::warn!(
                    ?format,
                    "4:4:4 negotiated but the capturer delivered subsampled YUV — encoding 4:2:0"
                );
                self.s.chroma_444 = false;
            }
            // `init_session` publishes the handle (and charges units) before its last
            // fallible steps, so a failure leaves a live session with `inited` false.
            // Re-init guards key off `inited`; teardown here, keyed off the handle,
            // so the next submit does not overwrite a live handle.
            if let Err(e) = self.init_session(device) {
                // SAFETY: same contract as the teardown above — encode thread owns the session,
                // and a failed init leaves nothing mid-encode.
                unsafe { self.teardown() };
                return Err(e);
            }
            self.init_device = dev_raw;
        }
        Ok(())
    }
}

impl Encoder for NvencD3d11Encoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        let frame = match &captured.payload {
            FramePayload::D3d11(f) => f,
            FramePayload::Cpu(_) => {
                bail!(
                    "NVENC D3D11 encoder needs a GPU texture frame (use the software encoder for CPU frames)"
                )
            }
        };
        self.prepare_d3d11(
            &frame.device,
            captured.format,
            captured.width,
            captured.height,
        )?;
        // Never reuse an in-flight bitstream; keep depth within the capturer's texture ring.
        // Encode is in-place: exceeding `pipeline_depth` overwrites a live texture. An
        // unknown ring uses 2 — less pipelining, not corruption. At the cap, block on the oldest.
        const UNCONFIGURED_RING_DEPTH: usize = 2;
        let cap = match self.input_ring_depth {
            Some(d) => async_inflight_cap().min(d.max(1)),
            None => async_inflight_cap().min(UNCONFIGURED_RING_DEPTH),
        };
        self.s.wait_below(cap)?;
        let enc = self.s.handle();
        let key = frame.texture.as_raw() as isize;
        if !self.regs.contains_key(&key) {
            let mut rr = nv::NV_ENC_REGISTER_RESOURCE {
                version: nv::NV_ENC_REGISTER_RESOURCE_VER,
                resourceType: nv::NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX,
                width: self.s.width,
                height: self.s.height,
                pitch: 0,
                resourceToRegister: frame.texture.as_raw(),
                bufferFormat: self.s.buffer_fmt,
                bufferUsage: nv::NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE,
                ..Default::default()
            };
            // SAFETY: `enc` is the live session `prepare_d3d11` just ensured; `rr` (version set)
            // names `frame.texture` from the same device the session opened against, and the
            // clone cached in `regs` keeps it alive until unregister.
            unsafe { (api().register_resource)(enc, &mut rr) }
                .nv_ok()
                .map_err(|e| nvenc_status::call_err("register_resource", e))?;
            self.regs
                .insert(key, (rr.registeredResource, frame.texture.clone()));
        }
        let event = self
            .events
            .get(self.s.slot())
            .map_or(ptr::null_mut(), |&e| e as *mut c_void);
        let input = EncodeInput {
            reg: self.regs[&key].0,
            pitch: 0,
            event,
            pts_ns: captured.pts_ns,
            hold: None,
        };
        // SAFETY: the session is open, `input.reg` registers this frame's texture on it and the
        // capturer keeps that texture unwritten while the ring depth caps in-flight encodes
        // (above); `event` is the slot's registered completion event or null in sync.
        unsafe { self.s.encode(input) }?;
        Ok(())
    }

    /// Pin this submission's frame number (`inputTimeStamp`) to the wire index the AU will
    /// carry, so RFI timestamps stay 1:1 across rebuilds. A repeat after a reset lands on a
    /// fresh session (teardown cleared the DPB), so re-pinning is always sound.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.s.frame_idx = wire_index as i64;
        self.submit(frame)
    }

    fn set_input_ring_depth(&mut self, depth: usize) {
        // Encode is in-place, so the capturer's ring depth hard-caps async pipeline depth.
        self.input_ring_depth = Some(depth);
        tracing::debug!(
            depth,
            env_cap = async_inflight_cap(),
            "NVENC: capturer input-ring depth reported — async in-flight bounded by the smaller"
        );
    }

    fn request_keyframe(&mut self) {
        self.s.force_kf = true;
    }

    fn caps(&self) -> EncoderCaps {
        // RFI is probed once at open. In-band HDR SEI needs no cap: it rides HEVC/H.264 keyframes.
        EncoderCaps {
            // Windows capture composites the pointer; this backend never reads `frame.cursor`.
            blends_cursor: false,
            supports_rfi: self.s.rfi_supported,
            // What the session actually configured (cleared if the GPU lacks YUV444).
            chroma_444: self.s.chroma_444,
            // Direct-NVENC recovers via real RFI (or a forced IDR), never an intra-refresh wave.
            intra_refresh: false,
            intra_refresh_recovery: false,
            intra_refresh_period: 0,
            downscales_input: false,
            crops_input: false,
        }
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

    /// The completion event of the oldest in-flight encode, in the event mode only
    /// ([`Self::use_completion_events`]). The two-thread mode owns its events, and a sync
    /// session has none — both answer `None`, and their `poll` blocks as before.
    fn ready_event(&self) -> Option<isize> {
        if !self.s.session_async || self.s.retrieving() {
            return None;
        }
        let bs = self.s.pending().front()?.bs;
        let slot = self.s.bitstreams().iter().position(|&b| b == bs)?;
        self.events.get(slot).map(|&e| e as isize)
    }

    fn supports_chunked_poll(&self) -> bool {
        self.s.supports_chunked_poll()
    }

    fn poll_chunk(&mut self) -> Result<Option<AuChunk>> {
        self.s.poll_chunk()
    }

    /// Encode-stall recovery: tear the session down; the next `submit` rebuilds (fresh
    /// session, IDR). Sync retrieve blocks inside `lock_bitstream`, so a hung lock never
    /// returns; this covers async retrieve (5 s event timeouts) and submit-side failures.
    fn reset(&mut self) -> bool {
        // SAFETY: `teardown` needs the encode thread with no NVENC call in flight and a session
        // whose cached resources belong to it — all hold (called between submit/poll).
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
        // Post-clamp: both open and reconfigure write what the session targets.
        Some(self.s.bitrate_bps)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // P1/ULL + frameIntervalP=1: each submit yields its AU; no internal queue.
    }
}

impl Drop for NvencD3d11Encoder {
    fn drop(&mut self) {
        // SAFETY: `teardown` needs the owning thread with no NVENC call in flight and a session
        // whose cached resources belong to it. At Drop this encoder is owned exclusively on the
        // encode thread; `teardown` early-returns on a closed session.
        unsafe { self.teardown() };
    }
}

/// Probe HEVC 4:4:4 (`NV_ENC_CAPS_SUPPORT_YUV444_ENCODE`). Cached by `pf_encode::can_encode_444`
/// and read before Welcome so the host advertises the chroma it can really encode.
pub fn probe_can_encode_444(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    if codec != Codec::H265 {
        return false;
    }
    probe_encode_cap(
        codec,
        nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_YUV444_ENCODE,
        adapter_luid,
    )
}

/// Probe 10-bit encode (`NV_ENC_CAPS_SUPPORT_10BIT_ENCODE` on the codec GUID). Cached by
/// `pf_encode::can_encode_10bit` and read before Welcome so negotiated depth matches NVENC.
pub fn probe_can_encode_10bit(codec: Codec, adapter_luid: Option<LUID>) -> bool {
    if !codec.supports_10bit() {
        return false;
    }
    probe_encode_cap(
        codec,
        nv::NV_ENC_CAPS::NV_ENC_CAPS_SUPPORT_10BIT_ENCODE,
        adapter_luid,
    )
}

/// One NVENC cap for `codec` on a throwaway session. `false` on any failure — unconfirmed = no.
fn probe_encode_cap(codec: Codec, cap: nv::NV_ENC_CAPS, adapter_luid: Option<LUID>) -> bool {
    with_probe_session(adapter_luid, |enc| {
        let mut param = nv::NV_ENC_CAPS_PARAM {
            version: nv::NV_ENC_CAPS_PARAM_VER,
            capsToQuery: cap,
            reserved: [0; 62],
        };
        let mut val: i32 = 0;
        // SAFETY: `get_encode_caps` reads one scalar cap into `val` (live locals) for live
        // session `enc` via the loaded API table (`with_probe_session` sits past `try_api`).
        unsafe {
            (api().get_encode_caps)(enc, codec_guid(codec), &mut param, &mut val)
                .nv_ok()
                .is_ok()
                && val != 0
        }
    })
    .unwrap_or(false)
}

/// Codecs this GPU's NVENC can encode (`nvEncGetEncodeGUIDs`) on a throwaway session,
/// probing the selected render adapter. Failure returns "nothing probed", which
/// `pf_encode::codec_support_wire_mask` turns into `None` so a broken probe cannot
/// narrow an NVIDIA host to nothing. Cached per GPU by `pf_encode::windows_codec_support`.
pub fn probe_codec_support(adapter_luid: Option<LUID>) -> crate::CodecSupport {
    let unknown = crate::CodecSupport {
        h264: false,
        h265: false,
        av1: false,
    };
    with_probe_session(adapter_luid, |enc| {
        // SAFETY: all NVENC calls go through the loaded API table against live session `enc`;
        // `count`/`written` are live locals, and `guids` is sized to the count the driver just
        // reported, its pointer valid for that many `GUID`s.
        unsafe {
            let mut count = 0u32;
            let counted = (api().get_encode_guid_count)(enc, &mut count)
                .nv_ok()
                .is_ok();
            let mut guids = vec![nv::GUID::default(); count as usize];
            let mut written = 0u32;
            let listed = counted
                && count > 0
                && (api().get_encode_guids)(enc, guids.as_mut_ptr(), count, &mut written)
                    .nv_ok()
                    .is_ok();
            if !listed {
                tracing::warn!(
                    "NVENC codec probe: driver listed no encode GUIDs — keeping the static \
                     advertisement"
                );
                return unknown;
            }
            guids.truncate(written as usize);
            crate::CodecSupport {
                h264: guids.contains(&codec_guid(Codec::H264)),
                h265: guids.contains(&codec_guid(Codec::H265)),
                av1: guids.contains(&codec_guid(Codec::Av1)),
            }
        }
    })
    .unwrap_or(unknown)
}

/// Open a throwaway NVENC session on a fresh hardware D3D11 device, hand it to `f`, tear down.
/// `None` = no loadable NVENC / no device / failed open. Shared by [`probe_encode_cap`] and
/// [`probe_codec_support`].
fn with_probe_session<T>(
    adapter_luid: Option<LUID>,
    f: impl FnOnce(*mut c_void) -> T,
) -> Option<T> {
    // Same exclusion as `init_session`: a throwaway open must not overlap a zombie reap.
    let _gate = DRIVER_SESSION_GATE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    use windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT;
    // No loadable NVENC → nothing to confirm. Also the `api()` gate for every call below and in `f`.
    if try_api().is_err() {
        return None;
    }
    // Probe the selected render adapter — the GPU the session will encode on. The OS default
    // can be the other GPU on a hybrid box.
    let device = pf_frame::dxgi::probe_device(adapter_luid, D3D11_CREATE_DEVICE_BGRA_SUPPORT)?;
    let target = OpenTarget {
        device_type: nv::NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX,
        device: device.as_raw(),
    };
    // SAFETY: this probe owns every handle it creates. `open_session` opens against `device`
    // (held for this scope) and destroys any residue of a failed open; `destroy_encoder` runs
    // once after `f` returns. No handle escapes.
    unsafe {
        let enc = open_session(api(), target).ok()?;
        let out = f(enc);
        let _ = (api().destroy_encoder)(enc);
        Some(out)
    }
}

#[cfg(test)]
#[path = "nvenc_tests.rs"]
mod tests;
