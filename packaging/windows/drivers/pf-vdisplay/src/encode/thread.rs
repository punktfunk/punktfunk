//! The encode thread (`pf-vd-encode`): opens a backend inside WUDFHost, reports the outcome to
//! the `SET_ENCODE` caller, then feeds the encoder from the pool and publishes into the AU
//! section. One per [`EncodeSession`]; a wedged one is detached, never joined without a bound.
//!
//! [`open_backend`] is one `open` per [`OpenSpec::backend`]; [`EncodeThread`] walks the
//! request's preference list and reports what took. PyroWave's private Vulkan instance goes
//! through the box's implicit layers unless [`disable_implicit_vulkan_layers`] ran first:
//! overlays hang in session 0, where there is no desktop to hook.

use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pf_driver_proto::encode::DRV_STATUS_OPENED;
use pf_driver_proto::encode::au::{self, AuHeader};
use pf_driver_proto::encode::{
    self as wire, EncoderCapsWire, SetEncodeReply, SetEncodeRequest, backend,
};
use pf_encode_win::{ChromaFormat, Codec, Encoder, EncoderCaps};
use pf_frame::HdrMeta;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use super::convert::{AdapterId, Fail, InputKind, bridge, pixel_format};
use super::drive::Drive;
use super::pool::Pool;
use super::section::{AuSection, EncodeSession};
use crate::direct_3d_device::Direct3DDevice;
use crate::monitor::Monitor;
use crate::worker::{Mmcss, Worker};

pub(crate) use pf_driver_proto::encode::backend::NAMES as BACKEND_NAMES;

/// A failed `SET_ENCODE` as the wire reply: `status` from the driver's domain, the stage tag
/// in `name`.
pub fn fail_reply(status: u32, (error, name): Fail) -> SetEncodeReply {
    let mut reply = SetEncodeReply {
        status,
        error,
        ..bytemuck::Zeroable::zeroed()
    };
    let n = name.len().min(32);
    reply.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    reply
}

/// `pf_frame::HdrMeta` from its 28 `repr(C)` bytes.
pub fn hdr_meta(bytes: &[u8; 28]) -> HdrMeta {
    // SAFETY: `HdrMeta` is `repr(C)`, 28 bytes of plain integers with no invalid bit pattern;
    // `read_unaligned` copies them out of the request's byte array.
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<HdrMeta>()) }
}

/// The request's `open` spec for backend `backend` of its list. The input comes from
/// [`InputKind::choose`], so a 4:4:4 session gets an input that carries it (or a backend whose
/// own caps already say 4:2:0) — never a P010 pick under a reply promising full chroma.
pub fn spec_for(req: &SetEncodeRequest, backend: u32) -> Result<OpenSpec, Fail> {
    let (hdr, chroma444) = (req.hdr == 1, req.chroma == 1);
    // 10-bit SDR (depth 10, HDR off) picks a BT.709 P010 input on AMF; `choose` ignores it elsewhere.
    let ten_bit = req.bit_depth >= 10;
    let kind = InputKind::choose(backend, hdr, ten_bit, chroma444);
    Ok(OpenSpec {
        backend,
        codec: codec_from_wire(req.codec).ok_or((-4, "codec"))?,
        kind,
        width: req.width,
        height: req.height,
        fps: req.fps.max(1),
        bitrate_bps: u64::from(req.bitrate_kbps) * 1000,
        bit_depth: if req.bit_depth >= 10 { 10 } else { 8 },
        chroma: if chroma444 {
            ChromaFormat::Yuv444
        } else {
            ChromaFormat::Yuv420
        },
    })
}

fn caps_wire(c: EncoderCaps) -> EncoderCapsWire {
    EncoderCapsWire {
        supports_rfi: u32::from(c.supports_rfi),
        chroma_444: u32::from(c.chroma_444),
        intra_refresh: u32::from(c.intra_refresh),
        intra_refresh_recovery: u32::from(c.intra_refresh_recovery),
        intra_refresh_period: c.intra_refresh_period,
        blends_cursor: u32::from(c.blends_cursor),
    }
}

/// Walk the request's backend list in order; the first that opens wins. `Err` is the last
/// failure as the wire reply — no silent fallback past the list. `open_backend` returns a
/// backend whose session already exists, so `reply.caps` describes the live encoder rather
/// than its defaults — the host reads those caps once and never asks again.
fn open_listed(
    req: &SetEncodeRequest,
    adapter: &AdapterId,
    device: &windows62::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Result<(Box<dyn Encoder>, OpenSpec, SetEncodeReply), SetEncodeReply> {
    let mut last: Fail = (-1, "nobackend");
    for &backend in req.backends.iter().take_while(|&&b| b != 0) {
        let spec = match spec_for(req, backend) {
            Ok(s) => s,
            Err(f) => {
                last = f;
                continue;
            }
        };
        match open_backend(&spec, adapter, device) {
            Ok(mut enc) => {
                if req.wire_chunk_bytes != 0 {
                    enc.set_wire_chunking(req.wire_chunk_bytes as usize);
                }
                if req.hdr == 1 {
                    enc.set_hdr_meta(Some(hdr_meta(&req.hdr_meta)));
                }
                let applied = enc.applied_bitrate_bps().unwrap_or(spec.bitrate_bps);
                let mut reply = fail_reply(
                    wire::SET_ENCODE_OK,
                    (0, BACKEND_NAMES[backend as usize - 1]),
                );
                reply.backend_opened = backend;
                reply.caps = caps_wire(enc.caps());
                reply.applied_bitrate_kbps = (applied / 1000) as u32;
                return Ok((enc, spec, reply));
            }
            Err(f) => last = f,
        }
    }
    Err(fail_reply(wire::SET_ENCODE_NO_BACKEND, last))
}

/// What the thread runs with. The `opened` channel carries exactly one reply: the open's.
/// `monitor` is used during set-up only — a detached thread must not pin its monitor.
pub struct ThreadCtx {
    pub session: Arc<EncodeSession>,
    pub monitor: Weak<Monitor>,
    pub device: Arc<Direct3DDevice>,
    pub opened: SyncSender<SetEncodeReply>,
}

/// The running encode thread of one session.
pub struct EncodeThread {
    worker: Option<Worker>,
    /// Cleared on detach: the thread touches neither pool nor section once this is false.
    live: Arc<AtomicBool>,
}

impl EncodeThread {
    /// How long a stop waits before the thread is detached. A healthy thread is between two
    /// backend calls within one frame; ~250 ms is ten of them at 60 Hz.
    pub const STOP_BOUND: Duration = Duration::from_millis(250);

    /// Start the thread. `None` when the OS refused a thread or event; the caller replies
    /// [`wire::SET_ENCODE_THREAD`].
    pub fn spawn(ctx: ThreadCtx) -> Option<Self> {
        let live = Arc::new(AtomicBool::new(true));
        let thread_live = live.clone();
        let worker = Worker::spawn("pf-vd-encode", move |stop| run(stop, ctx, thread_live))?;
        Some(Self {
            worker: Some(worker),
            live,
        })
    }

    /// Stop within [`Self::STOP_BOUND`]; a thread that does not return is detached and counted
    /// in the section's `detached` word — the host's `DriverCycle` threshold reads it.
    pub fn stop(mut self, section: &AuSection) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        if !worker.stop_within(Self::STOP_BOUND) {
            self.live.store(false, Ordering::Release);
            let n = section.add_u32(offset_of!(AuHeader, detached), 1);
            dbglog!("[pf-vd] encode: thread detached (total {n})");
        }
    }
}

/// The thread body: open, build or reuse the monitor's pool, report, then drive until stopped.
/// The pool is reused — retained slot included — when it already fits this session's device,
/// size and input kind; anything else is a fresh pool installed on the monitor. The open line
/// names the frame path (`pool` or S6's `bypass`), so a comparison run can prove which it got.
fn run(stop: HANDLE, ctx: ThreadCtx, live: Arc<AtomicBool>) {
    let _mmcss = Mmcss::distribution("encode");
    let section = &ctx.session.section;
    let fail = |status, f| {
        let _ = ctx.opened.send(fail_reply(status, f));
    };
    let Some(adapter) = AdapterId::of(&ctx.device) else {
        return fail(wire::SET_ENCODE_NO_DEVICE, (-5, "adapter"));
    };
    let Some(monitor) = ctx.monitor.upgrade() else {
        return fail(wire::SET_ENCODE_NO_MONITOR, (-6, "gone"));
    };
    let device = match bridge(&ctx.device.device) {
        Ok(d) => d,
        Err(f) => return fail(wire::SET_ENCODE_NO_DEVICE, f),
    };
    let (mut enc, spec, reply) = match open_listed(&ctx.session.request, &adapter, &device) {
        Ok(x) => x,
        Err(reply) => {
            let _ = ctx.opened.send(reply);
            return;
        }
    };
    let size = (spec.width, spec.height);
    let reused = monitor
        .pool()
        .filter(|p| p.matches(&ctx.device, spec.kind, size));
    let pool = match reused {
        Some(p) => p,
        None => match Pool::build(
            &ctx.device,
            spec.kind,
            size,
            monitor.source_seq.clone(),
            monitor.cursor_cell(),
        ) {
            Ok(p) => {
                monitor.set_pool(p.clone());
                p
            }
            Err(f) => return fail(wire::SET_ENCODE_POOL, f),
        },
    };
    drop(monitor);
    dbglog!(
        "[pf-vd] encode: backend {} open {}x{} {:?} mode={} (target {})",
        reply.backend_opened,
        spec.width,
        spec.height,
        spec.kind,
        if pool.bypass() { "bypass" } else { "pool" },
        ctx.session.request.target_id
    );
    // What the pool guarantees, so a backend that can encode an input texture where it lies skips
    // its own copy of every frame. A slot the encoder holds is in `encoding`, which no drain pass
    // takes back until the AU is published.
    enc.set_input_ring_depth(super::drive::MAX_INFLIGHT);
    section.store_u32(offset_of!(AuHeader, driver_status), DRV_STATUS_OPENED);
    section.store_u32(
        offset_of!(AuHeader, driver_status_detail),
        reply.backend_opened,
    );
    section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_OPEN);
    // The rate the backend opened at seeds the header stamp, so a retarget the backend
    // declines before it ever accepts one still reads back as the rate that is encoding.
    let opened_kbps = reply.applied_bitrate_kbps;
    section.store_u32(offset_of!(AuHeader, applied_bitrate_kbps), opened_kbps);
    if ctx.opened.send(reply).is_err() {
        // The caller gave up waiting: nothing will install this session.
        return;
    }
    Drive::new(enc, &pool, &ctx.session, stop, &live, spec.fps, opened_kbps).run();
    if live.load(Ordering::Acquire) {
        section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_CLOSED);
    }
}

/// Everything one backend `open` takes, in the backends' own vocabulary.
#[derive(Clone, Copy, Debug)]
pub struct OpenSpec {
    /// 1 NVENC, 2 AMF, 3 QSV, 4 PyroWave, 5 Media Foundation.
    pub backend: u32,
    pub codec: Codec,
    pub kind: InputKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub bit_depth: u8,
    pub chroma: ChromaFormat,
}

/// The wire codec numbering (1 H264, 2 HEVC, 3 AV1, 4 PyroWave).
pub fn codec_from_wire(codec: u32) -> Option<Codec> {
    Some(match codec {
        1 => Codec::H264,
        2 => Codec::H265,
        3 => Codec::Av1,
        4 => Codec::PyroWave,
        _ => return None,
    })
}

/// Implicit Vulkan layers (overlays, our pf-vkhdr-layer) hang in session 0, and the encoder's
/// private instance wants none of them. The loader-wide knob needs a 1.3.234+ loader; each
/// manifest's own `disable_environment` works on any.
///
/// Call from `driver_entry` only. Mutating the environment is unsound once other threads run,
/// and the encode thread is exactly the wrong place for it; at load there is no other thread.
pub fn disable_implicit_vulkan_layers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: called from `driver_entry`, before this driver has started a thread, so no
        // reader can race the write. WUDFHost is our own process (`ProcessSharingDisabled`), so
        // the variables reach nobody else.
        unsafe {
            for (k, v) in [
                ("VK_LOADER_LAYERS_DISABLE", "~implicit~"),
                ("DISABLE_RTSS_LAYER", "1"),
                ("DISABLE_PF_VKHDR", "1"),
                ("DISABLE_VK_LAYER_VALVE_steam_overlay_1", "1"),
                ("DISABLE_VK_LAYER_VALVE_steam_fossilize_1", "1"),
                ("EOS_OVERLAY_DISABLE_VULKAN_WIN64", "1"),
                ("DISABLE_GALAXY_OVERLAY", "1"),
            ] {
                std::env::set_var(k, v);
            }
        }
    });
}

/// One backend open on `adapter`. `Err` carries the stage tag; the backend's message is logged.
/// `device` is the one the pool's frames carry: NVENC opens its session against it here so the
/// caller's caps read describes the hardware.
pub fn open_backend(
    spec: &OpenSpec,
    adapter: &AdapterId,
    device: &windows62::Win32::Graphics::Direct3D11::ID3D11Device,
) -> Result<Box<dyn Encoder>, Fail> {
    // NVENC is the only arm that opens against the device, and it is x86-64 only.
    #[cfg(not(target_arch = "x86_64"))]
    let _ = device;
    let (w, h, fps, bps) = (spec.width, spec.height, spec.fps, spec.bitrate_bps);
    let (depth, chroma) = (spec.bit_depth, spec.chroma);
    let format = pixel_format(spec.kind);
    // P010 serves both HDR and 10-bit SDR; the kind is the only thing that tells them apart, so
    // the backend's colour signalling follows it, not the (identical) P010 pixel label.
    let hdr = matches!(spec.kind, InputKind::P010 | InputKind::Rgb10);
    let luid = Some(adapter.luid62());
    // NVENC, QSV and PyroWave are x86-64 only (see Cargo.toml); an ARM64 driver refuses their
    // ids here and the host falls through to Media Foundation.
    let opened: anyhow::Result<Box<dyn Encoder>> = match spec.backend {
        #[cfg(target_arch = "x86_64")]
        backend::NVENC => pf_encode_win::nvenc::NvencD3d11Encoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, 1, luid,
        )
        .and_then(|mut e| {
            // The drive loop parks on handles, so the session opens async and hands its
            // completion events out through `ready_event` — no retrieve thread, no sampling.
            // `PFVD_NVENC_EVENTS=0` (machine environment, read per open) falls back to the sync
            // session and the loop's bounded-poll arm: the A/B for a GPU whose async encode
            // retires slower than its sync one.
            e.use_completion_events(crate::log::knob("PFVD_NVENC_EVENTS").as_deref() != Some("0"));
            // All three of these defer their session to the first frame, and the host reads the
            // caps in our reply once per session: open it here or it caches the defaults.
            e.prepare_d3d11(device, format, w, h)?;
            Ok(Box::new(e) as Box<dyn Encoder>)
        }),
        backend::AMF => pf_encode_win::amf::AmfEncoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, hdr, luid,
        )
        .and_then(|mut e| {
            e.prepare(device)?;
            Ok(Box::new(e) as Box<dyn Encoder>)
        }),
        #[cfg(target_arch = "x86_64")]
        backend::QSV => pf_encode_win::qsv::QsvEncoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, luid,
        )
        .and_then(|mut e| {
            e.prepare(device)?;
            Ok(Box::new(e) as Box<dyn Encoder>)
        }),
        #[cfg(target_arch = "x86_64")]
        backend::PYROWAVE => {
            // Layers were disabled at `driver_entry`; doing it here would race the live threads.
            pf_encode_win::pyrowave::PyroWaveEncoder::open(
                w,
                h,
                fps,
                bps,
                chroma,
                depth,
                adapter.vendor_id,
                adapter.device_id,
            )
            .map(|e| Box::new(e) as Box<dyn Encoder>)
        }
        backend::MEDIA_FOUNDATION => pf_encode_win::mf::MfEncoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, luid,
        )
        .map(|e| Box::new(e) as Box<dyn Encoder>),
        _ => return Err((-1, "backend")),
    };
    opened.map_err(|e| {
        dbglog!(
            "[pf-vd] encode: backend {} open FAILED: {e:#}",
            spec.backend
        );
        (-1, "open")
    })
}

pub fn qpc_now() -> u64 {
    let mut qpc = 0i64;
    // SAFETY: plain FFI; `qpc` is a valid local out-param.
    let _ = unsafe { QueryPerformanceCounter(&mut qpc) };
    qpc as u64
}

pub fn qpc_frequency() -> u64 {
    let mut hz = 0i64;
    // SAFETY: plain FFI; `hz` is a valid local out-param.
    let _ = unsafe { QueryPerformanceFrequency(&mut hz) };
    (hz as u64).max(1)
}

pub fn qpc_to_ns(qpc: u64, hz: u64) -> u64 {
    (u128::from(qpc) * 1_000_000_000 / u128::from(hz)) as u64
}
