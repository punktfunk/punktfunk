//! The encode thread of one session: open a backend on the capture's device, build the pool
//! for the input it chose, report to the `SET_ENCODE` caller, then run the shared loop until
//! stopped. The driver's `encode/thread.rs`, with the capture where the swap chain is.

use std::mem::offset_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use pf_driver_proto::encode::au::{self, AuHeader};
use pf_driver_proto::encode::{self as wire, SetEncodeReply};
use pf_encode_session::drive::{Drive, Rates, MAX_INFLIGHT};
use pf_encode_session::open::{fail_reply, open_listed};
use pf_encode_session::section::{AuSection, EncodeSession, SessionThread};
use pf_encode_session::targets::source_format;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    AvSetMmThreadCharacteristicsW, CreateEventW, SetEvent, WaitForSingleObject,
};

use crate::pool::Pool;
use crate::source::Source;

/// A development knob by name, from this process's environment.
fn knob(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// What the thread runs with. `opened` carries exactly one reply: the open's.
pub struct ThreadCtx {
    pub session: Arc<EncodeSession>,
    pub source: Arc<Source>,
    pub opened: SyncSender<SetEncodeReply>,
}

/// The running encode thread of one session.
pub struct EncodeThread {
    join: JoinHandle<()>,
    /// Manual-reset; set once, by [`SessionThread::stop`].
    stop: OwnedHandle,
}

impl EncodeThread {
    /// How long a stop waits. A healthy thread is between two backend calls within one frame;
    /// 250 ms is ten of them at 60 Hz.
    const STOP_BOUND: Duration = Duration::from_millis(250);

    /// Start the thread. `None` when the OS refused a thread or an event.
    pub fn spawn(ctx: ThreadCtx) -> Option<Self> {
        // SAFETY: plain unnamed manual-reset event creation; the result is checked.
        let stop = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.ok()?;
        // SAFETY: the event was just created here and nothing else can close it.
        let stop = unsafe { OwnedHandle::from_raw_handle(stop.0) };
        // The thread only waits on the event, and `stop` outlives it: a stop joins or exits.
        let raw = stop.as_raw_handle() as usize;
        let join = std::thread::Builder::new()
            .name("pf-cw-encode".into())
            .spawn(move || run(raw as RawHandle, ctx))
            .ok()?;
        Some(Self { join, stop })
    }
}

impl SessionThread for EncodeThread {
    /// A thread that will not stop is inside a backend call that will not return. This
    /// process is the unit the host restarts, so it ends here instead of running on with a
    /// thread it cannot account for.
    fn stop(self: Box<Self>, _section: &AuSection) {
        // SAFETY: both handles are this value's own and live for the calls.
        let stopped = unsafe {
            let _ = SetEvent(HANDLE(self.stop.as_raw_handle()));
            let bound = Self::STOP_BOUND.as_millis() as u32;
            WaitForSingleObject(HANDLE(self.join.as_raw_handle()), bound) == WAIT_OBJECT_0
        };
        if !stopped {
            tracing::error!("encode thread did not stop within {:?}", Self::STOP_BOUND);
            std::process::exit(crate::EXIT_WEDGED);
        }
        let _ = self.join.join();
    }
}

/// The thread body: open, build the pool, report, then drive until stopped.
fn run(stop: RawHandle, ctx: ThreadCtx) {
    // MMCSS, as the driver's encode thread: the session must keep its cadence under a game
    // that takes every core. Best-effort, and the task ends with the thread.
    let mut task = 0u32;
    // SAFETY: a static task name and a live local out-param.
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Distribution"), &mut task) };

    let (session, source) = (&ctx.session, &ctx.source);
    let section = &session.section;
    let fail = |status, f| {
        let _ = ctx.opened.send(fail_reply(status, f));
    };
    let opened = open_listed(&session.request, &source.adapter, &source.device, &knob);
    let (mut enc, spec, reply) = match opened {
        Ok(x) => x,
        Err(reply) => {
            let _ = ctx.opened.send(reply);
            return;
        }
    };
    // The capture's format was fixed when it opened; an input that reads the other one cannot
    // be fed from it.
    if source_format(spec.kind) != source.format {
        return fail(wire::SET_ENCODE_POOL, (-4, "fmt"));
    }
    let size = (spec.width, spec.height);
    if source.size() != size {
        return fail(wire::SET_ENCODE_POOL, (-4, "size"));
    }
    let pool = match Pool::build(&source.device, &source.context, spec.kind, size) {
        Ok(p) => Arc::new(p),
        Err(f) => return fail(wire::SET_ENCODE_POOL, f),
    };
    tracing::info!(
        backend = reply.backend_opened,
        width = spec.width,
        height = spec.height,
        kind = ?spec.kind,
        fps = spec.fps,
        "encoder open"
    );
    // A slot the encoder holds stays out of the arrival handler's reach until its access unit
    // is published, so a backend may encode an input texture where it lies.
    enc.set_input_ring_depth(MAX_INFLIGHT);
    section.store_u32(offset_of!(AuHeader, driver_status), wire::DRV_STATUS_OPENED);
    section.store_u32(
        offset_of!(AuHeader, driver_status_detail),
        reply.backend_opened,
    );
    section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_OPEN);
    let opened_kbps = reply.applied_bitrate_kbps;
    section.store_u32(offset_of!(AuHeader, applied_bitrate_kbps), opened_kbps);
    // From here the capture fills the pool, starting with the picture it already holds.
    source.attach(pool.clone(), session.clone());
    if ctx.opened.send(reply).is_err() {
        source.detach(&pool);
        return;
    }
    let rates = Rates {
        fps: spec.fps,
        panel_hz: if source.refresh_hz == 0 {
            spec.fps
        } else {
            source.refresh_hz
        },
    };
    // This thread is never detached: a stop that fails ends the process.
    let live = AtomicBool::new(true);
    Drive::new(enc, &*pool, session, stop, &live, rates, opened_kbps, None).run();
    source.detach(&pool);
    section.store_u32(offset_of!(AuHeader, encoder_state), au::ENCODER_CLOSED);
}
