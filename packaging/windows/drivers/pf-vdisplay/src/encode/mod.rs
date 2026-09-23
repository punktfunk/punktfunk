//! In-driver encode (design/windows-video-plane-overhaul.md §2): the encoder backends opened
//! and driven inside WUDFHost — the driver's only video transport. [`convert`] bridges the driver's
//! `windows` 0.58 objects to the backends' 0.62 and owns the input targets a pool slot is
//! written in; [`section`] is the host's AU section and the session installed on a monitor;
//! [`thread`] opens a backend, reports, and publishes. The S5 probe (`encode_probe.rs`) is a
//! thin client of the same pieces. [`set_encode`] is the control-plane verb.

mod content_probe;
pub mod convert;
pub mod drive;
pub mod pool;
pub mod section;
pub mod thread;

use std::mem::offset_of;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::Duration;

use pf_driver_proto::encode::au::{self, AuHeader};
use pf_driver_proto::encode::{self as wire, EncodeCtlRequest, SetEncodeReply, SetEncodeRequest};
use wdk_sys::NTSTATUS;

use self::section::{AuSection, Ctl, EncodeSession};
use self::thread::{EncodeThread, ThreadCtx, fail_reply};
use crate::monitor::Monitor;
use crate::{STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND, STATUS_SUCCESS, registry};

const STATUS_UNSUCCESSFUL: NTSTATUS = 0xC000_0001u32 as NTSTATUS;

/// How long `SET_ENCODE` waits for the thread's open. NVENC opens in under a millisecond, AMF
/// in tens; a backend that takes seconds is stuck in a driver, and the host's watchdog window
/// (10 s) must still see the IOCTL return.
const OPEN_BOUND: Duration = Duration::from_secs(5);

/// How long `SET_ENCODE` waits for the OS to assign the monitor a swap chain. The host opens the
/// encoder once its CCD topology settle returns, and that settle says nothing about the swap
/// chain, so a monitor that has just arrived — a mid-stream resize re-arrives one — is routinely
/// still without it. 1.5 s matches the host's own settle bound.
const SWAP_BOUND: Duration = Duration::from_millis(1500);

/// The monitor's render adapter, waiting up to [`SWAP_BOUND`] for the assignment. `None` means the
/// monitor never got a swap chain (or went away), which is the one thing an encoder cannot open
/// without — failing immediately instead would reject an open that is merely early.
fn wait_render_luid(monitor: &Monitor) -> Option<windows::Win32::Foundation::LUID> {
    let deadline = std::time::Instant::now() + SWAP_BOUND;
    loop {
        if let Some(luid) = monitor.render_luid() {
            return Some(luid);
        }
        if monitor.gone() || std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Backend names in the order the addresses below pin them, for the load-time log.
pub fn backends_linked() -> &'static [&'static str] {
    #[cfg(target_arch = "x86_64")]
    return &["nvenc", "amf", "qsv", "pyrowave", "mf", "convert"];
    #[cfg(not(target_arch = "x86_64"))]
    return &["amf", "mf", "convert"];
}

/// `IOCTL_SET_ENCODE`: open an encoder for `owner`'s monitor with `req.target_id` on the AU
/// section the host delivered, replacing any session it already has.
///
/// `Err` is an NTSTATUS for a malformed request or a target `owner` does not hold, with nothing
/// adopted. `Ok` completes the IOCTL successfully whatever `status` says; from the map on, the
/// driver owns the handles (`AuSection`). The session already on the monitor stops first, with
/// no lock held: two encode threads on one pool would split its slots, and the old one's exit
/// clears the pool's `live` under its successor. The open runs on the new encode thread and
/// this call waits [`OPEN_BOUND`] for its reply.
pub fn set_encode(owner: u32, req: &SetEncodeRequest) -> Result<SetEncodeReply, NTSTATUS> {
    // `open_listed` indexes the name table by `backend - 1`, so the bound is that table's —
    // widening it in the proto widens this with no edit here.
    let listed = req.backends[0] != 0 && req.backends.iter().all(|&b| wire::backend::listed(b));
    let valid = req.target_id != 0
        && req.section != 0
        && req.event != 0
        && wire::codec::valid(req.codec)
        && req.width != 0
        && req.height != 0
        && listed;
    if !valid {
        return Err(STATUS_INVALID_PARAMETER);
    }
    // The host's `host.env` knobs, before any backend reads them. An old host sent none and
    // this is the defaults; the machine environment still overlays either (dev override).
    pf_encode_win::knobs::set(req.knobs);
    let Some(monitor) = registry::find(|m| m.owner == owner && m.target_id() == req.target_id)
    else {
        return Err(STATUS_NOT_FOUND);
    };
    let section = match AuSection::map(req.section, req.event, req.section_bytes) {
        Ok(s) => s,
        Err(_) => return Err(STATUS_INVALID_PARAMETER),
    };
    if let Some(old) = monitor.take_encode() {
        old.stop();
        // A thread that would not stop was detached still holding its slots.
        if let Some(pool) = monitor.pool() {
            pool.reclaim();
        }
    }
    // A render device that will not create is why no swap-chain took: send its HRESULT.
    let no_device = |stage: &'static str| {
        let fail =
            crate::direct_3d_device::last_init_error().map_or((-5, stage), |hr| (hr, "d3d11"));
        fail_reply(wire::SET_ENCODE_NO_DEVICE, fail)
    };
    let Some(luid) = wait_render_luid(&monitor) else {
        return Ok(no_device("noswap"));
    };
    let Some(device) = crate::direct_3d_device::pooled_device(luid) else {
        return Ok(no_device("device"));
    };
    let generation = monitor.next_encode_generation();
    let session = Arc::new(EncodeSession::new(*req, section, generation));
    let (tx, rx) = sync_channel(1);
    let Some(thread) = EncodeThread::spawn(ThreadCtx {
        session: session.clone(),
        monitor: Arc::downgrade(&monitor),
        device,
        opened: tx,
    }) else {
        return Ok(fail_reply(wire::SET_ENCODE_THREAD, (-7, "spawn")));
    };
    let reply = match rx.recv_timeout(OPEN_BOUND) {
        Ok(reply) => reply,
        Err(RecvTimeoutError::Timeout) => fail_reply(wire::SET_ENCODE_TIMEOUT, (-3, "open")),
        Err(RecvTimeoutError::Disconnected) => fail_reply(wire::SET_ENCODE_THREAD, (-7, "exit")),
    };
    if reply.status != wire::SET_ENCODE_OK {
        // The thread has exited (or is stuck in the open and gets detached); the session's
        // handles close with the last `Arc`.
        thread.stop(&session.section);
        return Ok(reply);
    }
    drop(session.set_thread(thread));
    match monitor.set_encode(session) {
        Ok(displaced) => {
            if let Some(old) = displaced {
                old.stop();
            }
            Ok(reply)
        }
        Err(session) => {
            // Torn down while opening: stop what was just started, the handles close with it.
            session.stop();
            Ok(fail_reply(wire::SET_ENCODE_NO_MONITOR, (-6, "gone")))
        }
    }
}

/// `IOCTL_ENCODE_CTL`: one op on the live encoder of `owner`'s monitor with `req.target_id`.
/// Everything but `reset` is queued for the encode thread and wakes it; `reset` is
/// [`reset`], on this thread.
pub fn encode_ctl(owner: u32, req: &EncodeCtlRequest) -> NTSTATUS {
    let Some(monitor) = registry::find(|m| m.owner == owner && m.target_id() == req.target_id)
    else {
        return STATUS_NOT_FOUND;
    };
    let Some(session) = monitor.encode() else {
        return STATUS_NOT_FOUND;
    };
    let op = match req.op {
        wire::ENCODE_CTL_REQUEST_KEYFRAME => Ctl::RequestKeyframe,
        wire::ENCODE_CTL_INVALIDATE_REF_FRAMES => Ctl::InvalidateRefFrames(req.arg0, req.arg1),
        wire::ENCODE_CTL_DISTRUST_REFERENCES => Ctl::DistrustReferences,
        wire::ENCODE_CTL_RECONFIGURE_BITRATE => Ctl::ReconfigureBitrate(req.arg0),
        wire::ENCODE_CTL_SET_HDR_META => Ctl::SetHdrMeta(req.payload),
        wire::ENCODE_CTL_FLUSH => Ctl::Flush,
        wire::ENCODE_CTL_RESET => return reset(&monitor, &session, req.arg0),
        wire::ENCODE_CTL_CLOSE => {
            // The host's proxy is gone; a stale proxy names an older generation and stops
            // nothing. The pool stays for the retained slot. Matching the LIVE session too is
            // what keeps a close from taking its own replacement: a mid-stream resize installs
            // the new session first, and this close arrives a moment later.
            if session.generation == req.arg0
                && let Some(live) = monitor.take_encode_if(session.generation)
            {
                live.stop();
                if let Some(pool) = monitor.pool() {
                    pool.reclaim();
                }
            }
            return STATUS_SUCCESS;
        }
        _ => return STATUS_INVALID_PARAMETER,
    };
    session.push_ctl(op);
    if let Some(pool) = monitor.pool() {
        pool.wake();
    }
    STATUS_SUCCESS
}

/// `ENCODE_CTL::reset` — the §2.5 first rung. The current thread is asked to stop within
/// [`EncodeThread::STOP_BOUND`]; one that will not return is detached (counted, logged) and
/// its pool slots reclaimed. A fresh encoder opens on a fresh thread, on the same session and
/// section, and the wire sequence restarts at `wire_seq_base`. Costs one open plus one IDR,
/// never a compose hitch. Fails `STATUS_UNSUCCESSFUL` when nothing would open — the session
/// then has no thread, which the host's `DriverCycle` rung answers.
fn reset(monitor: &Arc<Monitor>, session: &Arc<EncodeSession>, wire_seq_base: u32) -> NTSTATUS {
    if let Some(thread) = session.take_thread() {
        thread.stop(&session.section);
    }
    if let Some(pool) = monitor.pool() {
        pool.reclaim();
    }
    // The old thread's published slots: the new heap ring knows nothing of their bytes and
    // would place its IDR over them while the host still reads them, and both would carry the
    // new base. Freed here, where nothing writes; a slot the host holds READING stays its own.
    for i in 0..au::AU_SLOTS as usize {
        let _ = session.section.slot_state(i).compare_exchange(
            au::PUBLISHED,
            au::FREE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
    session
        .wire_seq_base
        .store(wire_seq_base, Ordering::Release);
    session.stale.store(false, Ordering::Release);
    session
        .section
        .store_u32(offset_of!(AuHeader, wire_seq_base), wire_seq_base);
    let Some(device) = monitor
        .render_luid()
        .and_then(crate::direct_3d_device::pooled_device)
    else {
        return STATUS_UNSUCCESSFUL;
    };
    let (tx, rx) = sync_channel(1);
    let Some(thread) = EncodeThread::spawn(ThreadCtx {
        session: session.clone(),
        monitor: Arc::downgrade(monitor),
        device,
        opened: tx,
    }) else {
        return STATUS_UNSUCCESSFUL;
    };
    let reply = rx.recv_timeout(OPEN_BOUND).ok();
    dbglog!(
        "[pf-vd] encode: reset -> wire_seq {wire_seq_base}, reopen status {:?}",
        reply.map(|r| r.status)
    );
    if reply.is_some_and(|r| r.status == wire::SET_ENCODE_OK) {
        drop(session.set_thread(thread));
        STATUS_SUCCESS
    } else {
        thread.stop(&session.section);
        STATUS_UNSUCCESSFUL
    }
}

// `nvidia-video-codec-sdk` (feature `ci-check`) links no import library, and the DLL still pulls
// its `EncodeAPI` object, which names these two entry points. The NVENC backend resolves both
// from `nvEncodeAPI64.dll` at runtime and never calls these; they only satisfy the linker.
// Not `pub`: internal linkage only, nothing is exported from the DLL.
#[unsafe(no_mangle)]
extern "C" fn NvEncodeAPICreateInstance(_list: *mut core::ffi::c_void) -> u32 {
    // NV_ENC_ERR_NO_ENCODE_DEVICE
    1
}

#[unsafe(no_mangle)]
extern "C" fn NvEncodeAPIGetMaxSupportedVersion(_version: *mut u32) -> u32 {
    // NV_ENC_ERR_NO_ENCODE_DEVICE
    1
}
