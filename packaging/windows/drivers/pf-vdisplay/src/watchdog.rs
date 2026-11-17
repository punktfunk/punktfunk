//! Owner liveness: which host processes are alive, and reaping the monitors of one that is not.
//!
//! Every monitor belongs to the process whose `IOCTL_ADD` created it
//! ([`Monitor::owner`](crate::monitor::Monitor::owner)), and only that process reaches it
//! again. An owner ends two ways: its last control handle closes ([`evt_file_cleanup`] — a
//! crash closes every handle, so this is immediate), or it keeps a handle open and sends no
//! IOCTL for [`WATCHDOG_TIMEOUT_S`] (a hung host; [`evt_timer`]). Either way only that owner's
//! monitors are departed, and a second host on the box keeps streaming.
//!
//! The timer is a child of the device, so the framework deletes it with the device instead of a
//! thread outliving the monitors it reaps, and [`stop`] waits out a tick that is mid-reap before
//! cleanup walks the same monitor list.

use std::sync::Mutex;
use std::time::Duration;

use wdk_iddcx::nt_success;
use wdk_sys::{
    NTSTATUS, ULONG, WDF_OBJECT_ATTRIBUTES, WDF_TIMER_CONFIG, WDFDEVICE, WDFFILEOBJECT, WDFTIMER,
    call_unsafe_wdf_function_binding,
};

use crate::registry::lock;
use crate::worker::Sendable;

/// The host must send an IOCTL within this window (it PINGs on a `timeout/3` timer) or the
/// watchdog treats it as gone and reaps its monitors. Reported to the host via `IOCTL_GET_INFO`.
pub const WATCHDOG_TIMEOUT_S: u32 = 10;

/// Tick period — about the host's PING cadence (`timeout/3`, 3.3 s). The reap needs
/// [`TICKS_TO_REAP`] silent ticks, so one dropped PING never trips it.
const TICK_MS: u32 = 3000;
/// Silent ticks before a reap, rounded UP so the reap never fires short of the timeout.
const TICKS_TO_REAP: u32 = (1000 * WATCHDOG_TIMEOUT_S).div_ceil(TICK_MS);

/// One host process: its IOCTL count, the count the last tick sampled, and how many ticks in a
/// row the two matched. Only the tick writes `seen` and `silent`; only one timer is ever armed.
struct Owner {
    pid: u32,
    pings: u64,
    seen: u64,
    silent: u32,
}

static OWNERS: Mutex<Vec<Owner>> = Mutex::new(Vec::new());
/// Control handles that have sent an IOCTL, by file object, with the process behind each. The
/// cleanup of a process's last one is the owner-gone signal.
static HANDLES: Mutex<Vec<(usize, u32)>> = Mutex::new(Vec::new());

/// The timer handle, from [`create`] until [`stop`] hands the device back to the framework.
type SendTimer = Sendable<WDFTIMER>;
static TIMER: Mutex<Option<SendTimer>> = Mutex::new(None);

/// Create the watchdog timer, stopped, as a child of `device`. Called from `driver_add` so the
/// handle exists before any IOCTL can arrive; [`start`] arms it once the adapter is up. Parenting
/// is what bounds the timer's life to the device's: a re-init gets a fresh timer, and no tick can
/// run after the framework has deleted the device.
pub fn create(device: WDFDEVICE) -> NTSTATUS {
    // AutomaticSerialization stays FALSE (the zeroed default): the tick reaps monitors, which joins
    // the swap-chain workers, and serializing that against the device's callbacks would park that
    // join in front of them.
    let mut cfg = WDF_TIMER_CONFIG {
        Size: core::mem::size_of::<WDF_TIMER_CONFIG>() as ULONG,
        EvtTimerFunc: Some(evt_timer),
        Period: TICK_MS,
        ..Default::default()
    };
    let mut attr = WDF_OBJECT_ATTRIBUTES {
        Size: core::mem::size_of::<WDF_OBJECT_ATTRIBUTES>() as ULONG,
        ParentObject: device.cast(),
        // Zeroed leaves these at 0 (Invalid) → set them like WDF_OBJECT_ATTRIBUTES_INIT.
        ExecutionLevel: wdk_sys::_WDF_EXECUTION_LEVEL::WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            wdk_sys::_WDF_SYNCHRONIZATION_SCOPE::WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    };
    let mut timer: WDFTIMER = core::ptr::null_mut();
    // SAFETY: cfg + attr are fully initialised locals; `timer` receives the created handle.
    let status = unsafe {
        call_unsafe_wdf_function_binding!(WdfTimerCreate, &mut cfg, &mut attr, &mut timer)
    };
    dbglog!("[pf-vd] watchdog WdfTimerCreate -> {status:#x}");
    if nt_success(status) {
        *lock(&TIMER) = Some(Sendable(timer));
    }
    status
}

/// Arm the watchdog from `adapter_init_finished` — the point where monitors become reachable.
/// Idempotent across re-entrant adapter inits: re-arming a queued timer only re-bases its due
/// time, and [`rebase`] restarts every owner's silence, so a reap counts no tick from before it.
pub fn start() {
    let timer = lock(&TIMER).as_ref().map(|t| t.0);
    let Some(timer) = timer else { return };
    rebase(&mut lock(&OWNERS));
    // SAFETY: `timer` is the live device-parented handle from `create`; a negative due time is
    // relative (100 ns units), so the first tick lands one period out.
    let _armed = unsafe {
        call_unsafe_wdf_function_binding!(WdfTimerStart, timer, -(i64::from(TICK_MS) * 10_000))
    };
}

/// Disarm the watchdog from device cleanup: the device — and with it every monitor — is going away,
/// and a reap running into `cleanup_for_device_removal` would fight it over the same monitor list.
/// Takes the handle, so the slot never holds one the framework has since deleted. The owner and
/// handle rows go too: they belong to the leaving device.
pub fn stop() {
    let taken = lock(&TIMER).take();
    let Some(Sendable(timer)) = taken else {
        return;
    };
    // SAFETY: `timer` is still live — the device's EvtCleanup runs before the framework deletes its
    // children. `1` is Wait=TRUE, so a tick that is mid-reap has returned by the time this does.
    let _was_queued = unsafe { call_unsafe_wdf_function_binding!(WdfTimerStop, timer, 1) };
    lock(&OWNERS).clear();
    lock(&HANDLES).clear();
    dbglog!("[pf-vd] watchdog: device cleanup — timer stopped");
}

/// Record liveness for EVERY inbound IOCTL from `pid`, not just PING — an ADD/REMOVE/GET_INFO
/// proves the host alive just as well — and note the control handle it arrived on as one of
/// that process's, so its cleanup can tell whether the process still holds another.
pub fn ping(pid: u32, file_object: WDFFILEOBJECT) {
    count(&mut lock(&OWNERS), pid);
    note_handle(&mut lock(&HANDLES), file_object as usize, pid);
}

/// One IOCTL from `pid`. A pid seen for the first time becomes an owner whose first tick
/// samples the count instead of counting silence.
fn count(owners: &mut Vec<Owner>, pid: u32) {
    match owners.iter_mut().find(|o| o.pid == pid) {
        Some(o) => o.pings += 1,
        None => owners.push(Owner {
            pid,
            pings: 1,
            seen: 0,
            silent: 0,
        }),
    }
}

/// Note the control handle `key` as one of `pid`'s; a handle is noted once.
fn note_handle(handles: &mut Vec<(usize, u32)>, key: usize, pid: u32) {
    if !handles.iter().any(|(fo, _)| *fo == key) {
        handles.push((key, pid));
    }
}

/// Forget the control handle `key`. Names its pid when that was the pid's last handle; a handle
/// still held beside it, or one that never sent an IOCTL, names nothing.
fn close_handle(handles: &mut Vec<(usize, u32)>, key: usize) -> Option<u32> {
    let pos = handles.iter().position(|(fo, _)| *fo == key)?;
    let (_, pid) = handles.swap_remove(pos);
    (!handles.iter().any(|(_, p)| *p == pid)).then_some(pid)
}

/// Sample every owner's count now and clear its silence: a reap then needs [`TICKS_TO_REAP`]
/// silent ticks counted from here.
fn rebase(owners: &mut [Owner]) {
    for o in owners {
        o.seen = o.pings;
        o.silent = 0;
    }
}

/// One tick over the owner table: a count that moved is sampled and its silence cleared, one
/// that stood still adds a silent tick. Returns the owners silent for [`TICKS_TO_REAP`] ticks
/// and removes them from the table.
fn stale_owners(owners: &mut Vec<Owner>) -> Vec<u32> {
    let mut stale = Vec::new();
    for o in owners.iter_mut() {
        if o.seen != o.pings {
            o.seen = o.pings;
            o.silent = 0;
            continue;
        }
        o.silent += 1;
        if o.silent >= TICKS_TO_REAP {
            stale.push(o.pid);
        }
    }
    owners.retain(|o| !stale.contains(&o.pid));
    stale
}

/// `EvtFileCleanup` on the control device: a handle is closing. When it was the last one its
/// process held ([`close_handle`]), that process is gone and its monitors depart now — not
/// after [`WATCHDOG_TIMEOUT_S`] of silence — so a restarted host finds its connectors free. The
/// handle lock is released before the reap.
pub unsafe extern "C" fn evt_file_cleanup(file_object: WDFFILEOBJECT) {
    let gone = close_handle(&mut lock(&HANDLES), file_object as usize);
    if let Some(pid) = gone {
        lock(&OWNERS).retain(|o| o.pid != pid);
        let n = crate::monitor::reap_owner(pid, Duration::ZERO);
        dbglog!("[pf-vd] owner {pid}: last control handle closed — departed {n} monitor(s)");
    }
}

/// One tick: depart the monitors of every owner whose IOCTL count has stood still for
/// [`WATCHDOG_TIMEOUT_S`] ([`stale_owners`]). A live host PINGs every `timeout/3`, so a count
/// only stalls this long when the host is hung, or gone with a handle the cleanup never
/// reported. The reap runs here on a framework thread with no lock held and joins the
/// swap-chain workers, which is why cleanup stops the timer with Wait=TRUE. A reaped owner's
/// rows go with its monitors: it starts over as a fresh owner on its next IOCTL.
unsafe extern "C" fn evt_timer(_timer: WDFTIMER) {
    let stale = stale_owners(&mut lock(&OWNERS));
    for pid in stale {
        lock(&HANDLES).retain(|(_, p)| *p != pid);
        let n = crate::monitor::reap_owner(pid, Duration::from_secs(3));
        dbglog!(
            "[pf-vd] watchdog: no IOCTL from owner {pid} in {WATCHDOG_TIMEOUT_S}s — departed {n} monitor(s)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tick up to `limit` times: the first tick that reaps, counted from 1, with its pids.
    fn ticks_to_reap(owners: &mut Vec<Owner>, limit: u32) -> Option<(u32, Vec<u32>)> {
        (1..=limit).find_map(|tick| {
            let stale = stale_owners(owners);
            (!stale.is_empty()).then_some((tick, stale))
        })
    }

    /// Owner 7 after one IOCTL and the tick that sampled it.
    fn sampled() -> Vec<Owner> {
        let mut owners = Vec::new();
        count(&mut owners, 7);
        assert!(
            stale_owners(&mut owners).is_empty(),
            "a moved count is no silence"
        );
        owners
    }

    #[test]
    fn a_silent_owner_reaps_on_the_fourth_silent_tick() {
        let mut owners = sampled();
        count(&mut owners, 8);
        for tick in 1..=3 {
            assert!(stale_owners(&mut owners).is_empty(), "silent tick {tick}");
            count(&mut owners, 8);
        }
        assert_eq!(stale_owners(&mut owners), [7]);
        let left: Vec<u32> = owners.iter().map(|o| o.pid).collect();
        assert_eq!(left, [8], "an owner that keeps sending stays");
    }

    #[test]
    fn an_ioctl_between_ticks_restarts_the_silence() {
        let mut owners = sampled();
        assert_eq!(ticks_to_reap(&mut owners, 3), None);
        count(&mut owners, 7);
        assert!(
            stale_owners(&mut owners).is_empty(),
            "the tick that sees the IOCTL"
        );
        assert_eq!(ticks_to_reap(&mut owners, 4), Some((4, vec![7])));
    }

    #[test]
    fn a_re_arm_restarts_the_silence() {
        let mut owners = sampled();
        assert_eq!(ticks_to_reap(&mut owners, 3), None);
        rebase(&mut owners);
        assert_eq!(ticks_to_reap(&mut owners, 4), Some((4, vec![7])));
    }

    #[test]
    fn the_last_handle_to_close_names_its_pid() {
        let mut handles = Vec::new();
        note_handle(&mut handles, 0x10, 7);
        note_handle(&mut handles, 0x20, 8);
        assert_eq!(
            close_handle(&mut handles, 0x30),
            None,
            "never sent an IOCTL"
        );
        assert_eq!(close_handle(&mut handles, 0x10), Some(7));
        assert_eq!(handles, [(0x20, 8)]);
    }

    #[test]
    fn a_pid_on_two_handles_is_gone_when_both_closed() {
        let mut handles = Vec::new();
        for key in [0x10, 0x20, 0x10] {
            note_handle(&mut handles, key, 7);
        }
        assert_eq!(handles, [(0x10, 7), (0x20, 7)]);
        assert_eq!(close_handle(&mut handles, 0x10), None);
        assert_eq!(close_handle(&mut handles, 0x20), Some(7));
        assert!(handles.is_empty());
    }
}
