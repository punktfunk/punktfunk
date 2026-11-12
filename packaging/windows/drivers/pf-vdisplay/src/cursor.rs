//! The IddCx hardware cursor: the control-plane verbs, each monitor's [`CursorState`] and the
//! query→publish worker.
//!
//! A delivered [`CursorShm`] section (`IOCTL_SET_CURSOR_CHANNEL`, [`set_cursor_channel`])
//! declares a hardware cursor (`IddCxMonitorSetupHardwareCursor`): DWM then excludes the pointer
//! from the frames it renders into the swap-chain and signals the monitor's data event on every
//! cursor change. The worker drains those signals and seqlock-publishes shape, position and
//! visibility into the section, which the host polls at its encode pace; the same publish lands
//! in the monitor's [`CursorCell`] for the encode pool's blend when the client draws no pointer.
//! [`set_cursor_forward`] flips the render model, [`resetup_cursor`] re-declares after a commit.
//!
//! Coordinates are published verbatim: the shape's top-left in this monitor's screen
//! coordinates, negative past its top-left edge. Shape pixels are the OS's 32-bpp rows at
//! `Pitch` (BGRA for alpha cursors, color+mask for masked-color ones), copied raw.
//!
//! Lock order: read the registry first, then take [`Monitor::cursor`]; never the reverse.

use core::sync::atomic::{AtomicU32, Ordering, fence};
use std::sync::Arc;

use pf_driver_proto::cursor::{
    CURSOR_MAGIC, CURSOR_SHAPE_BYTES, CURSOR_SHAPE_MAX, CURSOR_SHAPE_OFFSET, CURSOR_SHM_SIZE,
    CursorShm, shape_extent, shape_rgba,
};
use wdk_iddcx::nt_success;
use wdk_sys::iddcx;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Memory::{
    FILE_MAP_READ, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
};
use windows::Win32::System::Threading::WaitForMultipleObjects;

use crate::cursor_cell::{CursorCell, CursorImage};
use crate::monitor::Monitor;
use crate::registry::{self, lock};
use crate::worker::{OwnedHandle, OwnedView, Worker};

/// The host's `IOCTL_SET_CURSOR_CHANNEL` delivery: the [`CursorShm`] mapping handle VALUE,
/// already duplicated into this WUDFHost process. Owning a `CursorChannel` means owning the
/// handle; `Drop` closes it unless [`into_unowned`](Self::into_unowned) disarmed that — the
/// not-adopted reject path (the host reaps remotely), or [`setup_and_spawn`], which moves a
/// validated handle into an [`OwnedHandle`] that closes it instead.
pub struct CursorChannel {
    handle: u64,
    owned: bool,
}

impl CursorChannel {
    pub fn from_request(req: &pf_driver_proto::control::SetCursorChannelRequest) -> Option<Self> {
        if req.header_handle == 0 {
            return None;
        }
        Some(CursorChannel {
            handle: req.header_handle,
            owned: true,
        })
    }

    /// Disarm the Drop (delivery rejected — the handle stays for the host to reap remotely).
    pub fn into_unowned(mut self) {
        self.owned = false;
    }
}

impl Drop for CursorChannel {
    fn drop(&mut self) {
        if self.owned && self.handle != 0 {
            // SAFETY: we own this duplicated handle value; closing at most once (owned is our flag).
            unsafe {
                let _ = CloseHandle(HANDLE(self.handle as *mut core::ffi::c_void));
            }
        }
    }
}

/// One monitor's hardware-cursor state, behind [`Monitor::cursor`].
///
/// `data_event` is the OS cursor-data event the hardware cursor is declared against. It is owned
/// here, not by the worker thread: the declare paths copy its raw value out and call the setup
/// DDI after the guard drops, so it may close only after the worker's join — every path that
/// replaces the pair drops the worker first — and, since there is no un-declare, only once the
/// monitor has departed: it closes with the monitor. `forward_on == false` is the composite render
/// mode: the client draws no pointer, so the encode pool blends the worker's shape into every
/// frame DWM excludes it from (`cell.blend`, [`CursorState::set_blend`]).
pub(crate) struct CursorState {
    /// Before `data_event`: drop glue then joins the thread before closing the event it waits on.
    pub(crate) worker: Option<Worker>,
    data_event: Option<OwnedHandle>,
    pub(crate) forward_on: bool,
    /// Shared with the worker (writer) and the encode pool (reader); one per monitor life.
    pub(crate) cell: Arc<CursorCell>,
}

impl CursorState {
    /// No worker and no event; the forward render model until the arrival stamps the target's
    /// desired one.
    pub(crate) fn new() -> Self {
        Self {
            worker: None,
            data_event: None,
            forward_on: true,
            cell: Arc::default(),
        }
    }

    /// Blend iff the client draws nothing and a hardware cursor is declared on this adapter —
    /// the declare excludes the pointer from every frame for the WUDFHost's life, and the
    /// worker is the only shape source. Armed whenever a blend could turn on, so a clean plate
    /// exists when it does. `excluded` is [`registry::any_declared`], read before the caller
    /// took this monitor's lock.
    fn set_blend(&self, excluded: bool) {
        let armed = self.worker.is_some() && excluded;
        self.cell.set_armed(armed);
        self.cell.set_blend(!self.forward_on && armed);
    }
}

/// Adopt a hardware-cursor channel delivery (`IOCTL_SET_CURSOR_CHANNEL`): reuse or create the
/// monitor's data event, declare the hardware cursor, start the worker. `Err(ch)` when `owner`
/// has no arrived monitor with `target_id`, the event could not be made, or the section failed
/// to map or validate. A re-delivery replaces the worker and its section; the host re-sends
/// only after recreating the section. A monitor added without `hw_cursor` gets a channel only
/// because the adapter already excludes the pointer, so the channel exists for the pool's blend.
///
/// The monitor keeps one data event across deliveries: a re-declare after a swap-chain assign
/// copies its value out and must never find it closed. A replaced worker is joined before the
/// new one waits on it, and both the setup DDI and every join run with no lock held: the DDI can
/// re-enter the mode callbacks, and a join under a lock would head-block the control plane. A
/// delivery that fails leaves no worker, so the pool stops blending its last image.
pub fn set_cursor_channel(
    owner: u32,
    target_id: u32,
    ch: CursorChannel,
) -> Result<(), CursorChannel> {
    if target_id == 0 {
        return Err(ch);
    }
    let Some(m) = registry::find(|m| m.owner == owner && m.target_id() == target_id) else {
        return Err(ch);
    };
    let Some(object) = m.object() else {
        return Err(ch);
    };
    let Some(_ddi) = m.ddi() else {
        return Err(ch);
    };
    let excluded = registry::any_declared();
    let (declare, old_worker, old_event, cell) = {
        let mut c = lock(m.cursor());
        if !m.hw_cursor {
            c.forward_on = false;
        }
        (
            c.forward_on || excluded,
            c.worker.take(),
            c.data_event.take(),
            c.cell.clone(),
        )
    };
    drop(old_worker); // join a replaced worker BEFORE the new one waits on its event
    // Auto-reset: the OS signals it once per cursor update.
    let Some(data_event) = old_event.or_else(|| OwnedHandle::event(false)) else {
        dbglog!("[pf-vd] cursor: data event creation failed — keeping composited cursor");
        let excluded = registry::any_declared();
        lock(m.cursor()).set_blend(excluded);
        return Err(ch);
    };
    // `declare = false`: the composite render mode on an adapter that never declared — adopt
    // the channel and spawn the worker WITHOUT declaring, so DWM keeps compositing; a later
    // enable-flip declares against this event. Once anything declared, the pointer is gone
    // from every frame and the worker's shape is what the pool blends, so declare regardless.
    let spawned = setup_and_spawn(object, ch, declare, data_event.as_raw().0 as isize, cell);
    let worker = match spawned {
        Ok(worker) => worker,
        Err(unadopted) => {
            // An adopted channel is released already and the host sees no publish; one that
            // failed validation goes back for the host to reap. The event stays: an earlier
            // delivery may have declared it, and it closes with the monitor.
            let excluded = registry::any_declared();
            let mut c = lock(m.cursor());
            c.data_event.get_or_insert(data_event);
            c.set_blend(excluded);
            return unadopted.map_or(Ok(()), Err);
        }
    };
    if declare {
        // The worker only spawns after `IddCxMonitorSetupHardwareCursor` succeeded.
        registry::mark_declared(target_id);
    }
    let (displaced_worker, displaced_event) = install(&m, worker, data_event);
    let excluded = registry::any_declared();
    lock(m.cursor()).set_blend(excluded);
    drop(displaced_worker); // join outside every lock, then close the event it waited on
    drop(displaced_event);
    Ok(())
}

/// Install a fresh worker and the event it waits on, handing back what they displaced for the
/// caller to drop with no lock held, worker first. Once the monitor is torn down only the worker
/// comes back: a declare may name the event until the departure, so it stays and closes with
/// the monitor.
fn install(
    m: &Monitor,
    worker: Worker,
    data_event: OwnedHandle,
) -> (Option<Worker>, Option<OwnedHandle>) {
    let mut c = lock(m.cursor());
    if m.gone() {
        c.data_event.get_or_insert(data_event);
        return (Some(worker), None);
    }
    (c.worker.replace(worker), c.data_event.replace(data_event))
}

/// The mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`): `enable` declares the
/// hardware cursor again (DWM excludes the pointer; the per-commit re-declares resume);
/// disable stores the flag, which stops the per-commit re-declare on an adapter that never
/// declared and turns the pool's blend on where one did — there is no un-declare DDI.
/// `false` when `owner` has no monitor with `target_id`, or another owner has one.
///
/// The flip is state, not an edge on one monitor generation: the desired value persists per
/// target in the registry (a fresh entry inherits it at arrival) and is stamped on every live
/// entry matching the target, since duplicate generations coexist during re-arrival churn. The
/// DDI runs after every guard has dropped (it can re-enter the mode callbacks), against the
/// event value copied out of the entry, under the [`Monitor::ddi`] guard.
pub fn set_cursor_forward(owner: u32, target_id: u32, enable: bool) -> bool {
    // The desired state is keyed by OS target, so a target another owner holds is not this
    // owner's to flip.
    if registry::find(|m| m.owner != owner && m.target_id() == target_id).is_some() {
        return false;
    }
    registry::set_cursor_forward_desired(target_id, enable);
    let matching = registry::find_all(|m| m.owner == owner && m.target_id() == target_id);
    if matching.is_empty() {
        return false; // no monitor with this target at all
    }
    let excluded = registry::any_declared();
    // Only a present worker gets the immediate declare DDI call.
    let mut declare_on: Option<(Arc<Monitor>, Option<isize>)> = None;
    let (mut had_worker, mut blend_now) = (false, false);
    for m in &matching {
        let mut c = lock(m.cursor());
        c.forward_on = enable;
        c.set_blend(excluded);
        had_worker |= c.worker.is_some();
        blend_now |= c.cell.blends();
        if c.worker.is_some() {
            declare_on = Some((
                Arc::clone(m),
                c.data_event.as_ref().map(|h| h.as_raw().0 as isize),
            ));
        }
    }
    let (declare_m, worker_ev) = match declare_on {
        Some((m, ev)) => (Some(m), ev),
        None => (None, None),
    };
    match (enable, declare_m, worker_ev) {
        (true, Some(m), Some(ev)) => {
            // Enable declares immediately against the live worker's event (works any time),
            // under the DDI guard so a teardown cannot close that event underneath it.
            if let (Some(object), Some(_ddi)) = (m.object(), m.ddi()) {
                let st = setup_hardware_cursor(object, ev);
                dbglog!("[pf-vd] cursor: forward flip enable=1 (declare) -> {st:#x}");
                if wdk_iddcx::nt_success(st) {
                    registry::mark_declared(target_id);
                }
            }
        }
        (false, _, _) => {
            // Blend needs all three. Naming them is the difference between "the flip did not
            // arrive" and "it arrived and there was nothing to blend from".
            dbglog!(
                "[pf-vd] cursor: forward flip enable=0 — blend={} (forward_on=0 worker={} \
                 declared={}); a false worker/declared means nothing composites",
                u8::from(blend_now),
                u8::from(had_worker),
                u8::from(excluded)
            );
        }
        _ => dbglog!(
            "[pf-vd] cursor: forward flip enable=1 stored (worker={} declared={}) — applies at \
             the next channel delivery",
            u8::from(had_worker),
            u8::from(excluded)
        ),
    }
    true
}

/// Re-declare `m`'s hardware cursor after a swap-chain assign: a mode commit reverts the OS to
/// a software cursor, and without this `IddCxMonitorQueryHardwareCursor` fails
/// STATUS_NOT_SUPPORTED for good. No-op without a live worker, and in the composite render
/// mode on an adapter that never declared, whose software cursor is the point; once any
/// target declared, the pointer is excluded for good and the worker's shape is the only one
/// the pool can blend. The event value is copied out under the guard and the DDI runs after
/// it, since the DDI can re-enter the mode callbacks; the event closes only with the monitor,
/// and the [`Monitor::ddi`] guard holds the departure off until the call returns.
pub fn resetup_cursor(m: &Monitor) {
    let Some(object) = m.object() else {
        return;
    };
    let Some(_ddi) = m.ddi() else {
        return;
    };
    let excluded = registry::any_declared();
    let data_event = {
        let c = lock(m.cursor());
        c.data_event
            .as_ref()
            .filter(|_| (c.forward_on || excluded) && c.worker.is_some())
            .map(|h| h.as_raw().0 as isize)
    };
    if let Some(ev) = data_event {
        let st = setup_hardware_cursor(object, ev);
        dbglog!("[pf-vd] cursor: re-setup on swap-chain assign -> {st:#x}");
        if wdk_iddcx::nt_success(st) {
            registry::mark_declared(m.target_id());
        }
    }
}

/// Declare (or RE-declare) the hardware cursor for `monitor` against `data_event`, returning
/// the DDI status. Called at initial channel delivery AND on every swap-chain assignment, since
/// a mode commit reverts the path to a software cursor.
///
/// `data_event` is a BORROWED value: the monitor entry owns the event and closes it only after
/// joining the worker, so the handle this registers with the OS stays open for as long as the
/// OS may signal it. Must run OUTSIDE the monitors lock — the DDI may call back into the mode
/// callbacks, which take it.
pub fn setup_hardware_cursor(monitor: iddcx::IDDCX_MONITOR, data_event: isize) -> i32 {
    let caps = iddcx::IDDCX_CURSOR_CAPS {
        Size: core::mem::size_of::<iddcx::IDDCX_CURSOR_CAPS>() as u32,
        // On-glass finding (2026-07-22, .173): the hardware-cursor query delivers ONLY
        // `IDDCX_CURSOR_SHAPE_TYPE_ALPHA` shapes, in EVERY configuration — all three
        // ColorXorCursorSupport levels, event-driven AND ~30 Hz polled. Masked/monochrome
        // cursors (I-beam, resize, SIZEALL, app hand cursors) never arrive: NONE rejects the
        // query outright (STATUS_NOT_SUPPORTED); EMULATION software-composites them into the
        // frame (double cursor vs the client's stale local shape); FULL excludes them from the
        // frame but never delivers them (shape freezes on the last alpha cursor). FULL is the
        // right resting state: the frame stays cursor-free for ALL cursor types, and the
        // full-fidelity SHAPE comes from the session-side cursor source in the host, not from
        // this query (design/remote-desktop-sweep.md §8).
        ColorXorCursorSupport: iddcx::IDDCX_XOR_CURSOR_SUPPORT::IDDCX_XOR_CURSOR_SUPPORT_FULL,
        MaxX: CURSOR_SHAPE_MAX,
        MaxY: CURSOR_SHAPE_MAX,
        AlphaCursorSupport: 1,
    };
    let setup = iddcx::IDARG_IN_SETUP_HWCURSOR {
        CursorInfo: caps,
        hNewCursorDataAvailable: data_event as *mut core::ffi::c_void,
    };
    // SAFETY: `monitor` is a live IddCx monitor; `setup` outlives the call; `data_event` is a
    // live event handle owned by the monitor entry, which outlives every worker it declares.
    unsafe { wdk_iddcx::IddCxMonitorSetupHardwareCursor(monitor, &setup) }
}

// There is NO un-declare path: empty caps are rejected `STATUS_INVALID_PARAMETER`. The
// composite flip is a flag plus a mode re-commit: `set_cursor_forward(false)` stores
// the flag, the host forces a same-mode re-commit, and the OS's per-commit software-cursor
// default sticks because `resetup_cursor` skips the flagged monitor.

/// Map the delivered section and start the query→publish worker for `monitor`.
///
/// The handle value is untrusted until it maps and the view carries [`CURSOR_MAGIC`]: a value
/// that fails either check is never closed, since it could name an unrelated handle in this
/// process, and comes back as `Err(Some(ch))` for the host to reap its duplicate. A value that
/// passes is adopted into an [`OwnedView`] that unmaps and closes on EVERY later exit —
/// including a failed spawn, which drops the closure holding it — and then travels into the
/// thread. `data_event` is only borrowed: the monitor entry owns it and closes it after the join.
///
/// `Err(None)` when the adopted channel failed later (spawn, DDI) and is released already. On
/// any `Err` the caller keeps the composited cursor, which is also what the host falls back to
/// when no seqlock publish arrives.
pub fn setup_and_spawn(
    monitor: iddcx::IDDCX_MONITOR,
    ch: CursorChannel,
    declare: bool,
    data_event: isize,
    cell: Arc<CursorCell>,
) -> Result<Worker, Option<CursorChannel>> {
    let raw = HANDLE(ch.handle as *mut core::ffi::c_void);
    // SAFETY: MapViewOfFile validates `raw` itself and fails on a value that names no section;
    // size is the fixed contract size. READ|WRITE: we write the cursor state, the host reads it.
    let view = unsafe { MapViewOfFile(raw, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, CURSOR_SHM_SIZE) };
    if view.Value.is_null() {
        dbglog!("[pf-vd] cursor: MapViewOfFile failed — keeping composited cursor");
        return Err(Some(ch));
    }
    let shm = view.Value.cast::<CursorShm>();
    // SAFETY: the view spans CURSOR_SHM_SIZE >= size_of::<CursorShm>(); reading the host stamp.
    if unsafe { core::ptr::addr_of!((*shm).magic).read_volatile() } != CURSOR_MAGIC {
        dbglog!("[pf-vd] cursor: section magic mismatch — rejecting");
        // SAFETY: the view mapped just above, unmapped once here; the handle stays open.
        let _ = unsafe { UnmapViewOfFile(view) };
        return Err(Some(ch));
    }
    // SAFETY: the section carries our magic, so `raw` is the handle the host duplicated for us;
    // `into_unowned` disarms the channel's own close, leaving this value its sole closer.
    let mapping = unsafe { OwnedHandle::from_raw(raw) };
    ch.into_unowned();
    // SAFETY: `view` is the single mapping of `mapping` made above; from here one value owns
    // both, and every return below unmaps and closes them.
    let view = unsafe { OwnedView::from_raw(view.Value, mapping) };

    // Spawn BEFORE declaring: a declaration names `data_event`, which the caller closes when
    // this returns `Err`. `declare = false` (a delivery landing in COMPOSITE render mode)
    // already spawns undeclared, so a later enable-flip has an event to declare against.
    // The IddCx monitor handle travels as an integer; the view is `Send` itself.
    let monitor_v = monitor as usize;
    let worker = Worker::spawn("pf-vd-cursor", move |stop| {
        // The view unmaps when this thread returns.
        run_worker(monitor_v, view.base() as usize, data_event, stop, &cell);
    })
    .ok_or(None)?;

    if declare {
        let st = setup_hardware_cursor(monitor, data_event);
        if !nt_success(st) {
            dbglog!(
                "[pf-vd] cursor: IddCxMonitorSetupHardwareCursor failed 0x{:08x}",
                st as u32
            );
            // `worker` drops here → stops and joins the thread it just started.
            return Err(None);
        }
        dbglog!("[pf-vd] cursor: hardware cursor declared — worker started");
    } else {
        dbglog!("[pf-vd] cursor: channel adopted UNdeclared (composite mode) — worker started");
    }
    Ok(worker)
}

/// The wait→query→publish loop, exiting when `stop` signals.
///
/// This thread owns NOTHING it has to release. `stop` belongs to the [`Worker`] that spawned it
/// and `data_v` to the monitor entry, both of which close after the join; the mapping behind
/// `view_v` is unmapped by the [`OwnedView`] the spawning closure moved in here. Returning is
/// the whole cleanup.
fn run_worker(monitor_v: usize, view_v: usize, data_v: isize, stop: HANDLE, cell: &CursorCell) {
    let monitor = monitor_v as iddcx::IDDCX_MONITOR;
    let shm = view_v as *mut CursorShm;
    let shape_dst = (view_v + CURSOR_SHAPE_OFFSET) as *mut u8;
    let mut shape_buf = vec![0u8; CURSOR_SHAPE_BYTES];
    let mut last_shape_id: u32 = 0;
    // `last_shape_id` holds still while the pointer is hidden, so the OS reports that shape on
    // every poll. The log line keys on its own id.
    let mut logged_shape_id: u32 = 0;
    // Consecutive failed queries: ~100 ms at the poll rate before the blend drops the pointer.
    const QUERY_FAILS_HIDE: u32 = 3;
    let mut query_fails: u32 = 0;
    let mut published = false;
    // The pool's copy of the latest publish; its position outlives shape-less ticks.
    let mut image: Option<CursorImage> = None;
    let handles = [stop, HANDLE(data_v as *mut core::ffi::c_void)];
    // The predecessor's pointer is not this worker's to blend; see `CursorCell::reset`.
    cell.reset();
    loop {
        // Poll as well as wait: the OS signals the event only for hardware-plane cursors, so
        // a ~30 Hz timeout keeps position and visibility fresh across the ones it never
        // signals for. The query with the current `LastShapeId` is a no-op when nothing moved.
        const POLL_MS: u32 = 33;
        // SAFETY: both handles are live for the worker's lifetime (owner drops after join).
        let w = unsafe { WaitForMultipleObjects(&handles, false, POLL_MS) };
        if w == WAIT_OBJECT_0 {
            return; // stop
        }
        // Query on the data event OR the poll timeout; anything else is a wait failure.
        if w != WAIT_TIMEOUT && w.0 != WAIT_OBJECT_0.0 + 1 {
            dbglog!("[pf-vd] cursor: wait returned {:#x} — worker exiting", w.0);
            return; // wait failed — owner is tearing down
        }
        let in_args = iddcx::IDARG_IN_QUERY_HWCURSOR {
            LastShapeId: last_shape_id,
            ShapeBufferSizeInBytes: CURSOR_SHAPE_BYTES as u32,
            pShapeBuffer: shape_buf.as_mut_ptr(),
        };
        // Zeroed, the OS writes every field it reports. v3: the base query DDI slot is stubbed
        // to NOT_SUPPORTED on current IddCx.
        let mut out = iddcx::IDARG_OUT_QUERY_HWCURSOR3::default();
        // SAFETY: `monitor` is live (departure drops this worker FIRST), args outlive the call.
        let st =
            unsafe { wdk_iddcx::IddCxMonitorQueryHardwareCursor3(monitor, &in_args, &mut out) };
        if !nt_success(st) {
            query_fails += 1;
            if query_fails == 1 {
                dbglog!(
                    "[pf-vd] cursor: query failed 0x{:08x} (logged once)",
                    st as u32
                );
            }
            // A lost declare answers STATUS_NOT_SUPPORTED for good: past a few polls, stop the
            // pool blending a pointer frozen where it last was.
            if query_fails == QUERY_FAILS_HIDE {
                // SAFETY: `shm` points at the mapped CursorShm for the worker's lifetime.
                let hdr = unsafe { core::ptr::read_volatile(shm) };
                cell.publish(&mut image, &hdr, None, false);
            }
            continue;
        }
        query_fails = 0;
        if !published {
            published = true;
            // The raw position: monitor-relative per the IddCx docs; this line shows it.
            dbglog!(
                "[pf-vd] cursor: publishes live (x={} y={} posvalid={})",
                out.X,
                out.Y,
                out.PositionValid
            );
        }
        // One line per distinct shape: type (1 = masked color, 2 = alpha), size, visibility.
        // A masked shape is one the OS may still composite in software.
        if out.IsCursorShapeUpdated != 0 && out.CursorShapeInfo.ShapeId != logged_shape_id {
            logged_shape_id = out.CursorShapeInfo.ShapeId;
            dbglog!(
                "[pf-vd] cursor SHAPE id={} type={} {}x{} vis={} posvalid={}",
                out.CursorShapeInfo.ShapeId,
                out.CursorShapeInfo.CursorType,
                out.CursorShapeInfo.Width,
                out.CursorShapeInfo.Height,
                out.IsCursorVisible,
                out.PositionValid,
            );
        }
        // Seqlock publish: odd → write → even. The header alone changes on position moves;
        // shape bytes are only rewritten when the OS says the image changed, so a reader that
        // skips unchanged shape_ids never observes torn pixels.
        // SAFETY: `shm` points at the mapped CursorShm for the worker's lifetime.
        let seq = unsafe { &*core::ptr::addr_of!((*shm).seq).cast::<AtomicU32>() };
        let s = seq.load(Ordering::Relaxed);
        seq.store(s.wrapping_add(1), Ordering::Relaxed); // odd = mid-update
        fence(Ordering::Release);
        let visible = out.IsCursorVisible != 0;
        let mut shape = None;
        // SAFETY: exclusive writer (single worker per section); plain volatile field writes,
        // then one volatile read of the header for the host-stamped scale.
        let hdr = unsafe {
            core::ptr::addr_of_mut!((*shm).visible).write_volatile(u32::from(visible));
            // v3 `X`/`Y` are only meaningful when `PositionValid`; otherwise keep the prior
            // position (a position-invalid tick still carries a shape/visibility update).
            if out.PositionValid != 0 {
                core::ptr::addr_of_mut!((*shm).x).write_volatile(out.X);
                core::ptr::addr_of_mut!((*shm).y).write_volatile(out.Y);
            }
            if out.IsCursorShapeUpdated != 0 && visible {
                let info = &out.CursorShapeInfo;
                let stamp = CursorShm {
                    cursor_type: info.CursorType,
                    width: info.Width,
                    height: info.Height,
                    pitch: info.Pitch,
                    hot_x: info.XHot,
                    hot_y: info.YHot,
                    ..bytemuck::Zeroable::zeroed()
                };
                let (width, rows, pitch) = shape_extent(&stamp);
                core::ptr::copy_nonoverlapping(shape_buf.as_ptr(), shape_dst, rows * pitch);
                core::ptr::addr_of_mut!((*shm).cursor_type).write_volatile(stamp.cursor_type);
                core::ptr::addr_of_mut!((*shm).width).write_volatile(width as u32);
                core::ptr::addr_of_mut!((*shm).height).write_volatile(rows as u32);
                core::ptr::addr_of_mut!((*shm).pitch).write_volatile(info.Pitch);
                core::ptr::addr_of_mut!((*shm).hot_x).write_volatile(info.XHot);
                core::ptr::addr_of_mut!((*shm).hot_y).write_volatile(info.YHot);
                core::ptr::addr_of_mut!((*shm).shape_id).write_volatile(info.ShapeId);
                last_shape_id = info.ShapeId;
                shape = Some(shape_rgba(&stamp, &shape_buf));
            }
            core::ptr::read_volatile(shm)
        };
        fence(Ordering::Release);
        seq.store(s.wrapping_add(2), Ordering::Release); // even = consistent
        cell.publish(&mut image, &hdr, shape, visible);
    }
}
