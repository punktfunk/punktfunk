//! The encode thread (`pf-vd-encode`): opens a backend inside WUDFHost, reports the outcome to
//! the `SET_ENCODE` caller, then runs the shared session loop over the monitor's pool, which
//! publishes into the AU section. One per [`EncodeSession`]; a wedged one is detached, never
//! joined without a bound.
//!
//! PyroWave's private Vulkan instance goes through the box's implicit layers unless
//! [`disable_implicit_vulkan_layers`] ran first: overlays hang in session 0, where there is no
//! desktop to hook.

use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Weak};
use std::time::Duration;

use pf_driver_proto::encode::DRV_STATUS_OPENED;
use pf_driver_proto::encode::au::{self, AuHeader};
use pf_driver_proto::encode::{self as wire, SetEncodeReply};
use pf_encode_session::drive::{Drive, MAX_INFLIGHT, Rates};
use pf_encode_session::open::open_listed;
use pf_encode_session::section::SessionThread;
use windows::Win32::Foundation::HANDLE;

use super::convert::{adapter_of, bridge};
use super::pool::Pool;
use super::section::{AuSection, EncodeSession};
use crate::direct_3d_device::Direct3DDevice;
use crate::monitor::Monitor;
use crate::worker::{Mmcss, Worker};

pub use pf_encode_session::open::{fail_reply, qpc_frequency, qpc_now};

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

impl SessionThread for EncodeThread {
    fn stop(self: Box<Self>, section: &AuSection) {
        EncodeThread::stop(*self, section);
    }
}

/// G3 fault injection: encode this many frames, then never return from the encode work
/// (`PFVD_ENCODE_BLOCK_AFTER`, a frame count; unset or unparsable disables it). Read once per
/// encoder open, so a `reset` reopens blocked again until the knob is cleared.
fn block_after() -> Option<u64> {
    #[cfg(feature = "encode-probe")]
    return crate::log::knob("PFVD_ENCODE_BLOCK_AFTER")?
        .trim()
        .parse()
        .ok();
    #[cfg(not(feature = "encode-probe"))]
    None
}

/// The thread body: open, build or reuse the monitor's pool, report, then drive until stopped.
/// The pool is reused — retained slot included — when it already fits this session's device,
/// size and input kind; anything else is a fresh pool installed on the monitor. The session
/// opens on the pool's newest frame or the monitor's seed, and its sequence is in the header
/// before the reply. The open line names the frame path (`pool` or `bypass`) and that frame.
fn run(stop: HANDLE, ctx: ThreadCtx, live: Arc<AtomicBool>) {
    let _mmcss = Mmcss::distribution("encode");
    let section = &ctx.session.section;
    let fail = |status, f| {
        let _ = ctx.opened.send(fail_reply(status, f));
    };
    let Some(adapter) = adapter_of(&ctx.device) else {
        return fail(wire::SET_ENCODE_NO_DEVICE, (-5, "adapter"));
    };
    let Some(monitor) = ctx.monitor.upgrade() else {
        return fail(wire::SET_ENCODE_NO_MONITOR, (-6, "gone"));
    };
    let device = match bridge(&ctx.device.device) {
        Ok(d) => d,
        Err(f) => return fail(wire::SET_ENCODE_NO_DEVICE, f),
    };
    let opened = open_listed(&ctx.session.request, &adapter, &device, &crate::log::knob);
    let (mut enc, spec, reply) = match opened {
        Ok(x) => x,
        Err(reply) => {
            let _ = ctx.opened.send(reply);
            return;
        }
    };
    let size = (spec.width, spec.height);
    // `PFVD_POOL_BYPASS` (machine environment, read per open): `0` copies every frame.
    let bypass = wire::zero_copy(
        spec.backend,
        spec.kind.composed(),
        crate::log::knob("PFVD_POOL_BYPASS").as_deref(),
    );
    let reused = monitor
        .pool()
        .filter(|p| p.matches(&ctx.device, spec.kind, size, bypass));
    let pool = match reused {
        Some(p) => p,
        None => match Pool::build(
            &ctx.device,
            spec.kind,
            size,
            monitor.source_seq.clone(),
            monitor.cursor_cell(),
            bypass,
        ) {
            Ok(p) => {
                monitor.set_pool(p.clone());
                p
            }
            Err(f) => return fail(wire::SET_ENCODE_POOL, f),
        },
    };
    let first = pool.first_frame(&monitor.seed());
    // The requested mode leads the list: the refresh the host created this panel at.
    let panel_hz = monitor
        .modes()
        .first()
        .and_then(|m| m.refresh_rates.first().copied())
        .unwrap_or(spec.fps);
    drop(monitor);
    dbglog!(
        "[pf-vd] encode: backend {} open {}x{} {:?} mode={} first_frame={} (target {})",
        reply.backend_opened,
        spec.width,
        spec.height,
        spec.kind,
        if pool.bypass() { "bypass" } else { "pool" },
        first.map_or("none".into(), |s| format!("seq {s}")),
        ctx.session.request.target_id
    );
    // What the pool guarantees, so a backend that can encode an input texture where it lies skips
    // its own copy of every frame. A slot the encoder holds is in `encoding`, which no drain pass
    // takes back until the AU is published.
    enc.set_input_ring_depth(MAX_INFLIGHT);
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
    // The host counts a first frame by this sequence moving, and a queued frame is one.
    if let Some(seq) = first {
        section.store_u64(offset_of!(AuHeader, source_seq), seq);
    }
    if ctx.opened.send(reply).is_err() {
        // The caller gave up waiting: nothing will install this session.
        return;
    }
    let rates = Rates {
        fps: spec.fps,
        panel_hz,
    };
    Drive::new(
        enc,
        &*pool,
        &ctx.session,
        stop.0,
        &live,
        rates,
        opened_kbps,
        block_after(),
    )
    .run();
    if live.load(Ordering::Acquire) {
        section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_CLOSED);
        // The encoder is closed, and D3D11 frees what it released only at a flush. The pooled
        // device outlives the session and is idle after it.
        // SAFETY: a single call on the pooled device's multithread-protected context.
        unsafe { ctx.device.device_context.Flush() };
    }
}

/// Implicit Vulkan layers (overlays, our pf-vkhdr-layer) hang in session 0, and the encoder's
/// private instance wants none of them. The loader-wide knob needs a 1.3.234+ loader; each
/// manifest's own `disable_environment` works on any.
///
/// Call from `driver_entry`, before the first encoder opens: the loader reads these when it
/// creates an instance. WUDFHost is our own process (`ProcessSharingDisabled`), so the variables
/// reach nobody else.
pub fn disable_implicit_vulkan_layers() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: std documents `set_var` as always safe on Windows: the process environment
        // sits behind the OS's own lock, so the framework threads WUDFHost already runs cannot
        // race the write.
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
