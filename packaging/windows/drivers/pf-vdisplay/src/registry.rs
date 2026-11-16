//! The live-monitor registry: the one strong owner of every [`Monitor`], plus the per-id and
//! per-target state that must outlive any single monitor generation.
//!
//! A process `static` by necessity: the IddCx monitor/mode DDIs receive only an IddCx handle,
//! never the WDFDEVICE or its context. With one devnode and `ProcessSharingDisabled` the host
//! process dies with the device, so this is device-scoped already; device removal releases the
//! monitors' resources explicitly ([`crate::monitor::cleanup_for_device_removal`]).
//!
//! The lock is held for lookups and insert/remove only. A removal hands the `Arc<Monitor>`
//! back; the caller tears it down and departs it with the lock released, so nothing whose
//! `Drop` joins a thread or closes a handle ever drops under it. Lock order is
//! `REGISTRY → Monitor.*`; workers only touch their own monitor and never take this lock.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pf_driver_proto::vdisplay;

use crate::monitor::{Mode, Monitor};

struct Registry {
    monitors: Vec<Arc<Monitor>>,
    /// The last advertised list of a departed monitor, per id, unioned into the next same-id
    /// create so a re-arrived monitor's arrival list already holds every mode its predecessor
    /// served. The OS pins the settable set at arrival, so this is what makes a return to a
    /// previously used size an in-place mode set instead of a hotplug. In-process only;
    /// bounded by 16 ids × [`vdisplay::MODE_LIST_CAP`] modes.
    mode_history: Vec<(u32, Vec<Mode>)>,
    /// Desired cursor-render state per OS target (`IOCTL_SET_CURSOR_FORWARD`): `false` =
    /// composite, do not declare the hardware cursor. Kept outside the entries because they
    /// churn (re-arrival resizes, sibling slot re-creates): a fresh entry inherits this at
    /// arrival, so no generation can resurrect a declare the session turned off. Absent =
    /// `true` (declare at delivery).
    cursor_forward: Vec<(u32, bool)>,
    /// OS targets on which `IddCxMonitorSetupHardwareCursor` succeeded. A declare is
    /// irrevocable (no un-declare DDI) and excludes the pointer for the whole adapter, not only
    /// the declaring target, so ADD replies report `cursor_excluded` from [`any_declared`] and
    /// a session without a cursor channel self-composites the pointer. Never cleared: the
    /// state's true scope is this WUDFHost's life, which is also this static's. Bounded: 16 ids.
    declared: Vec<u32>,
}

static REGISTRY: Mutex<Registry> = Mutex::new(Registry {
    monitors: Vec::new(),
    mode_history: Vec::new(),
    cursor_forward: Vec::new(),
    declared: Vec::new(),
});

/// Lock `m`, recovering the guard on poison. Unreachable under this workspace's
/// `panic = "abort"`; kept so every lock site is this one call.
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn registry() -> MutexGuard<'static, Registry> {
    lock(&REGISTRY)
}

/// The first live monitor matching `pred`.
pub fn find(pred: impl Fn(&Monitor) -> bool) -> Option<Arc<Monitor>> {
    registry().monitors.iter().find(|m| pred(m)).cloned()
}

/// Every live monitor matching `pred`.
pub fn find_all(pred: impl Fn(&Monitor) -> bool) -> Vec<Arc<Monitor>> {
    registry()
        .monitors
        .iter()
        .filter(|m| pred(m))
        .cloned()
        .collect()
}

/// Unlink every monitor matching `pred`, handing the `Arc`s to the caller for teardown and
/// departure with the lock released.
pub fn remove(pred: impl Fn(&Monitor) -> bool) -> Vec<Arc<Monitor>> {
    let mut reg = registry();
    let (gone, keep): (Vec<_>, Vec<_>) = reg.monitors.drain(..).partition(|m| pred(m));
    reg.monitors = keep;
    // A reap or CLEAR_ALL loses the id's history otherwise, and the next resize back to a
    // mode it once advertised is a re-arrival instead of an in-place update.
    for m in &gone {
        record_modes(&mut reg, m);
    }
    gone
}

/// Remember `monitor`'s advertised list as its id's mode history.
fn record_modes(reg: &mut Registry, monitor: &Monitor) {
    let modes = monitor.modes();
    if let Some(slot) = reg.mode_history.iter_mut().find(|(i, _)| *i == monitor.id) {
        slot.1 = modes;
    } else {
        reg.mode_history.push((monitor.id, modes));
    }
}

/// Unlink `owner`'s monitor for `session_id`, recording its advertised list as the id's mode
/// history under the same guard, so a same-id create racing this removal cannot miss it.
pub fn remove_session(owner: u32, session_id: u64) -> Option<Arc<Monitor>> {
    let mut reg = registry();
    let pos = reg
        .monitors
        .iter()
        .position(|m| m.owner == owner && m.session_id == session_id)?;
    let monitor = reg.monitors.remove(pos);
    record_modes(&mut reg, &monitor);
    Some(monitor)
}

/// Register `owner`'s pending monitor for `session_id`: allocate its id — the host's
/// `preferred_id` when valid and not live on the device, else the lowest free, since a bounded
/// reused id keeps IddCx reusing the same OS target slot instead of leaving a ghost node — union
/// in the id's mode history, and link it. Allocation and insert share one guard so two
/// concurrent ADDs cannot pick the same id.
pub fn insert(
    owner: u32,
    session_id: u64,
    hw_cursor: bool,
    preferred_id: u32,
    exact_mhz: u32,
    mut modes: Vec<Mode>,
) -> Arc<Monitor> {
    let mut reg = registry();
    let live: Vec<u32> = reg.monitors.iter().map(|m| m.id).collect();
    let id = vdisplay::resolve_id(&live, preferred_id);
    // The id is only known here, so this is where the monitor's own history joins its list.
    // `accumulate_modes` owns the seat rule.
    if let Some((_, prev)) = reg.mode_history.iter().find(|(i, _)| *i == id) {
        vdisplay::accumulate_modes(&mut modes, crate::adapter::is_seat_role(), prev);
    }
    let monitor = Arc::new(Monitor::pending(
        owner, id, session_id, hw_cursor, exact_mhz, modes,
    ));
    reg.monitors.push(monitor.clone());
    monitor
}

/// The desired cursor-forward state for `target_id` (default `true`).
pub fn cursor_forward_desired(target_id: u32) -> bool {
    registry()
        .cursor_forward
        .iter()
        .find(|(t, _)| *t == target_id)
        .is_none_or(|(_, on)| *on)
}

/// Persist the desired cursor-forward state for `target_id`.
pub fn set_cursor_forward_desired(target_id: u32, enable: bool) {
    let mut reg = registry();
    if let Some(slot) = reg.cursor_forward.iter_mut().find(|(t, _)| *t == target_id) {
        slot.1 = enable;
    } else {
        reg.cursor_forward.push((target_id, enable));
    }
}

/// Record a SUCCESSFUL hardware-cursor declare on `target_id`.
pub fn mark_declared(target_id: u32) {
    let mut reg = registry();
    if !reg.declared.contains(&target_id) {
        reg.declared.push(target_id);
        dbglog!("[pf-vd] cursor: target {target_id} marked hardware-cursor declared (irrevocable)");
    }
}

/// True if ANY target ever had a hardware cursor declared in this WUDFHost's life — the
/// adapter-wide exclusion reach.
pub fn any_declared() -> bool {
    !registry().declared.is_empty()
}
