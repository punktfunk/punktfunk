//! The display ACTOR (vdisplay immunity plan WP8, decision D6) and the OS display-event listener.
//!
//! One thread owns the hidden window that receives the three user-mode display signals Windows
//! exposes, and it is the only place that reads the CCD inventory for the hot paths:
//!
//! - `WM_DEVICECHANGE` + `RegisterDeviceNotificationW(GUID_DEVINTERFACE_MONITOR)`: monitor
//!   interface arrival/removal, including devnode churn that leaves topology unchanged.
//! - `DBT_DEVNODES_CHANGED`: PnP-tree churn with no payload.
//! - `WM_DISPLAYCHANGE`: a mode/topology commit reached the desktop. Absence is a signal: a
//!   probe with no mode delta does not fire this.
//! - `EVENT_SYSTEM_DESKTOPSWITCH` (WinEvent): the input desktop moved (UAC / lock / logon and
//!   back). Not logged; it refreshes [`crate::secure_desktop`] for the capturer's cursor guard.
//! - `EVENT_SYSTEM_FOREGROUND` (WinEvent): another window took the foreground. Logged with its
//!   process and class: a game that loses focus stops drawing while the stream keeps running.
//!
//! A driver-internal probe (EDID/DDC, DP retrain) emits none of these. Pair that silence with
//! metronomic stalls to tell a KMD-below-OS sink from a Windows re-enumeration.
//!
//! Every event schedules ONE coalesced refresh ([`snapshot::COALESCE`]) rather than querying
//! inside the window procedure; a slow safety timer covers a missed broadcast; a failed query
//! keeps the last-known-good [`DisplaySnapshot`] labelled with its age and backs off. Readers take
//! [`snapshot`] (an `Arc`, never the display-config lock) or wait on [`wait_for_change`]; a
//! caller that just mutated the topology asks for [`refresh_and_wait`].

use std::collections::VecDeque;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Devices::Display::GUID_DEVINTERFACE_MONITOR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClassNameW, GetMessageW,
    GetWindowThreadProcessId, KillTimer, PostMessageW, RegisterClassW, RegisterDeviceNotificationW,
    SetTimer, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVNODES_CHANGED,
    DBT_DEVTYP_DEVICEINTERFACE, DEVICE_NOTIFY_WINDOW_HANDLE, DEV_BROADCAST_DEVICEINTERFACE_W,
    DEV_BROADCAST_HDR, EVENT_SYSTEM_DESKTOPSWITCH, EVENT_SYSTEM_FOREGROUND, MSG, WINDOW_EX_STYLE,
    WINEVENT_OUTOFCONTEXT, WM_APP, WM_DEVICECHANGE, WM_DISPLAYCHANGE, WM_TIMER, WNDCLASSW,
    WS_OVERLAPPED,
};

use crate::snapshot::{self, DisplaySnapshot, SnapshotCache};

#[derive(Clone)]
pub struct DisplayEvent {
    pub at: Instant,
    pub kind: DisplayEventKind,
    /// Monitor instance id on arrival/removal; `None` otherwise.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DisplayEventKind {
    MonitorArrival,
    MonitorRemoval,
    /// PnP tree churn (broadcast, no payload).
    DevNodesChanged,
    DisplayChange,
}

impl DisplayEventKind {
    fn label(self) -> &'static str {
        match self {
            Self::MonitorArrival => "monitor-arrival",
            Self::MonitorRemoval => "monitor-removal",
            Self::DevNodesChanged => "devnodes-changed",
            Self::DisplayChange => "display-change",
        }
    }
}

struct State {
    events: VecDeque<DisplayEvent>,
    cache: SnapshotCache,
}

/// 128 ≈ 1 min at a 2 s probe cycle with ≤4 events each. The correlator only reads the last gap.
const RING_CAP: usize = 128;

/// The slow safety refresh (timer 1) and the coalescing one-shot (timer 2) — see `snapshot`.
const TIMER_SAFETY: usize = 1;
const TIMER_COALESCE: usize = 2;
/// Posted by [`request_refresh`] from any thread; the pump coalesces it like an OS event.
const WM_PF_REFRESH: u32 = WM_APP + 0x50;

/// The pump's window, once created (0 until then / if bring-up failed).
static HWND_CELL: AtomicIsize = AtomicIsize::new(0);

fn state() -> &'static (Mutex<State>, Condvar) {
    static STATE: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
    STATE.get_or_init(|| {
        (
            Mutex::new(State {
                events: VecDeque::with_capacity(RING_CAP),
                cache: SnapshotCache::new(Instant::now()),
            }),
            Condvar::new(),
        )
    })
}

fn lock() -> std::sync::MutexGuard<'static, State> {
    state().0.lock().unwrap_or_else(|e| e.into_inner())
}

/// Window or registration failure leaves the ring empty and the snapshot at generation 0;
/// streaming continues on the legacy direct readers.
pub fn spawn_once() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("pf-display-events".into())
            .spawn(pump);
        if let Err(e) = spawned {
            tracing::warn!(
                error = %e,
                "display-event listener thread not spawned — stall logs won't carry OS event attribution"
            );
        }
    });
}

/// The current snapshot — the hot read. Generation 0 (empty) until the actor's first query lands;
/// check [`DisplaySnapshot::is_fresh`] / [`DisplaySnapshot::age`] before treating it as a
/// verification.
pub fn snapshot() -> Arc<DisplaySnapshot> {
    lock().cache.current()
}

/// Ask the actor for a refresh (coalesced with any pending event). Fire-and-forget; pair with
/// [`wait_for_change`] or use [`refresh_and_wait`] to observe the result. No-op before the
/// window exists.
pub fn request_refresh() {
    let hwnd = HWND_CELL.load(Ordering::Acquire);
    if hwnd == 0 {
        return;
    }
    // SAFETY: `hwnd` is the pump's live window (stored after creation, never destroyed while the
    // process runs); PostMessageW only queues a message for that thread.
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut core::ffi::c_void)),
            WM_PF_REFRESH,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

/// Block until a snapshot with a generation above `seen` is published, or `timeout` elapses.
/// Returns the newest snapshot either way (`None` only if the actor never started).
pub fn wait_for_change(seen: u64, timeout: Duration) -> Option<Arc<DisplaySnapshot>> {
    if HWND_CELL.load(Ordering::Acquire) == 0 {
        return None;
    }
    let (m, cv) = state();
    let guard = m.lock().unwrap_or_else(|e| e.into_inner());
    let (guard, _) = cv
        .wait_timeout_while(guard, timeout, |st| st.cache.generation() <= seen)
        .unwrap_or_else(|e| e.into_inner());
    Some(guard.cache.current())
}

/// Refresh now and return the resulting snapshot (or the newest one if `timeout` elapses first).
/// The call a topology MUTATOR makes after its `SetDisplayConfig`, so its verification reads the
/// post-commit state without touching the display-config lock itself.
pub fn refresh_and_wait(timeout: Duration) -> Option<Arc<DisplaySnapshot>> {
    let seen = lock().cache.generation();
    request_refresh();
    wait_for_change(seen, timeout)
}

/// The snapshot, or — before the actor has published its first one (generation 0: not spawned
/// yet, or bring-up failed) — a one-off inventory read on the CALLER's thread, unpublished. For
/// the cold, non-hot readers (management listing, pre-mutation baselines, probe retargeting)
/// that must answer even when no session ever started the actor.
pub fn snapshot_or_query() -> Arc<DisplaySnapshot> {
    let snap = snapshot();
    if snap.generation > 0 {
        return snap;
    }
    let now = Instant::now();
    match crate::win_display::target_inventory_checked() {
        Ok(targets) => Arc::new(DisplaySnapshot {
            generation: 0,
            taken_at: now,
            failures: 0,
            targets: Arc::from(targets),
        }),
        Err(_) => snap,
    }
}

pub fn events_between(from: Instant, to: Instant) -> Vec<DisplayEvent> {
    let st = lock();
    st.events
        .iter()
        .filter(|e| e.at >= from && e.at <= to)
        .cloned()
        .collect()
}

/// One log field: `"monitor-removal x2 (DISPLAY\\…), devnodes-changed x1"`; `"none"` if empty.
pub fn summarize(events: &[DisplayEvent]) -> String {
    if events.is_empty() {
        return "none".into();
    }
    let mut out: Vec<String> = Vec::new();
    for kind in [
        DisplayEventKind::MonitorArrival,
        DisplayEventKind::MonitorRemoval,
        DisplayEventKind::DevNodesChanged,
        DisplayEventKind::DisplayChange,
    ] {
        let hits: Vec<&DisplayEvent> = events.iter().filter(|e| e.kind == kind).collect();
        if hits.is_empty() {
            continue;
        }
        let detail = hits
            .iter()
            .rev()
            .find_map(|e| e.detail.as_deref())
            .map(|d| format!(" ({d})"))
            .unwrap_or_default();
        out.push(format!("{} x{}{}", kind.label(), hits.len(), detail));
    }
    out.join(", ")
}

/// External standby sinks and the laptop panel Exclusive isolate deactivated, from the cached
/// snapshot. Never takes the CCD lock.
pub fn connected_inactive_physicals() -> Vec<String> {
    snapshot().connected_inactive_physicals()
}

fn push_event(kind: DisplayEventKind, detail: Option<String>) {
    let mut st = lock();
    if st.events.len() >= RING_CAP {
        st.events.pop_front();
    }
    st.events.push_back(DisplayEvent {
        at: Instant::now(),
        kind,
        detail,
    });
}

/// The one live inventory read. Publishes a fresh snapshot on success (an empty topology
/// included); on failure keeps the last-known-good, stamps the failure, and re-arms the coalesce
/// timer with the backoff delay so the retry never storms a display-config lock that is busy.
fn refresh_inventory(hwnd: HWND) {
    let started = Instant::now();
    let result = crate::win_display::target_inventory_checked();
    let now = Instant::now();
    // The display-config lock diagnostic the descriptor poller used to carry: a healthy inventory
    // read is sub-millisecond; tens of milliseconds means something holds the lock (topology
    // churn, display-poller software). Rate-limited to one line per 10 s.
    static LAST_SLOW: Mutex<Option<Instant>> = Mutex::new(None);
    let took = now.saturating_duration_since(started);
    if took >= Duration::from_millis(50) {
        let mut last = LAST_SLOW.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
            *last = Some(now);
            tracing::warn!(
                took_ms = took.as_millis() as u64,
                "slow display-descriptor poll — something is holding the Windows display-config \
                 lock (topology churn / display-poller software); on a host with periodic stream \
                 hitches, correlate this cadence"
            );
        }
    }
    let (m, cv) = state();
    let mut st = m.lock().unwrap_or_else(|e| e.into_inner());
    match result {
        Ok(targets) => {
            st.cache.publish(targets, now);
        }
        Err(e) => {
            let retry = st.cache.fail();
            tracing::debug!(
                ?e,
                retry_ms = retry.as_millis() as u64,
                "display snapshot: CCD query failed — keeping last-known-good"
            );
            // SAFETY: `hwnd` is the pump's live window; a repeated SetTimer on the same id resets it.
            unsafe {
                SetTimer(Some(hwnd), TIMER_COALESCE, retry.as_millis() as u32, None);
            }
        }
    }
    drop(st);
    cv.notify_all();
}

/// Coalesce: (re)arm the one-shot so a burst of broadcasts costs one query after it settles.
fn schedule_refresh(hwnd: HWND) {
    // SAFETY: `hwnd` is the pump's live window; SetTimer with an existing id resets its period.
    unsafe {
        SetTimer(
            Some(hwnd),
            TIMER_COALESCE,
            snapshot::COALESCE.as_millis() as u32,
            None,
        );
    }
}

/// `DBT_DEVNODES_CHANGED` arrives as `wParam` on `WM_DEVICECHANGE` without a registration; interface
/// arrival/removal needs `RegisterDeviceNotificationW` below.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DISPLAYCHANGE => {
            // lParam is the new primary resolution. Distinguishes a real mode change from a same-mode re-commit.
            let (w, h) = (
                (lparam.0 & 0xffff) as u32,
                ((lparam.0 >> 16) & 0xffff) as u32,
            );
            push_event(DisplayEventKind::DisplayChange, Some(format!("{w}x{h}")));
            schedule_refresh(hwnd);
            LRESULT(0)
        }
        WM_DEVICECHANGE => {
            let event = wparam.0 as u32;
            if event == DBT_DEVNODES_CHANGED {
                push_event(DisplayEventKind::DevNodesChanged, None);
                schedule_refresh(hwnd);
            } else if event == DBT_DEVICEARRIVAL || event == DBT_DEVICEREMOVECOMPLETE {
                let kind = if event == DBT_DEVICEARRIVAL {
                    DisplayEventKind::MonitorArrival
                } else {
                    DisplayEventKind::MonitorRemoval
                };
                // SAFETY: for these two events lParam is a DEV_BROADCAST_HDR or 0 (checked). Header
                // fields only, then DEV_BROADCAST_DEVICEINTERFACE_W after dbch_devicetype matches,
                // reading at most dbch_size bytes — the size the sender declared.
                let detail = unsafe {
                    let hdr = lparam.0 as *const DEV_BROADCAST_HDR;
                    if hdr.is_null() || (*hdr).dbch_devicetype != DBT_DEVTYP_DEVICEINTERFACE {
                        None
                    } else {
                        let di = hdr as *const DEV_BROADCAST_DEVICEINTERFACE_W;
                        let head = std::mem::offset_of!(DEV_BROADCAST_DEVICEINTERFACE_W, dbcc_name);
                        let bytes = ((*hdr).dbch_size as usize).saturating_sub(head);
                        let name = std::slice::from_raw_parts(
                            std::ptr::addr_of!((*di).dbcc_name).cast::<u16>(),
                            (bytes / 2).min(512),
                        );
                        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
                        crate::monitor_devnode::instance_id_from_interface_path(
                            &String::from_utf16_lossy(&name[..end]),
                        )
                    }
                };
                push_event(kind, detail);
                schedule_refresh(hwnd);
            }
            LRESULT(0)
        }
        WM_PF_REFRESH => {
            schedule_refresh(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == TIMER_COALESCE {
                // One-shot: kill before the query so a query slower than the period cannot
                // re-enter us, then refresh (a failure re-arms with its backoff).
                // SAFETY: `hwnd` is the pump's live window and the id is ours.
                unsafe {
                    let _ = KillTimer(Some(hwnd), TIMER_COALESCE);
                }
            }
            refresh_inventory(hwnd);
            LRESULT(0)
        }
        // SAFETY: default handling for everything else — the standard wndproc tail call.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Message-only windows receive neither `WM_DISPLAYCHANGE` nor broadcast `WM_DEVICECHANGE`.
fn pump() {
    let class: Vec<u16> = "pf-display-events\0".encode_utf16().collect();
    // SAFETY: Win32 window bring-up on this thread. `class` outlives every pointer use (lives to
    // fn end; the pump loops forever). Handles are the preceding calls' returns; any failure
    // returns from the thread (degraded, see `spawn_once`). The filter is a fully initialised
    // local that RegisterDeviceNotificationW reads synchronously.
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            tracing::warn!(
                "display-event listener: GetModuleHandleW failed — no OS event attribution"
            );
            return;
        };
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            tracing::warn!(
                "display-event listener: RegisterClassW failed — no OS event attribution"
            );
            return;
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WS_OVERLAPPED, // hidden; exists only to receive broadcasts
            0,
            0,
            0,
            0,
            None,
            None,
            Some(wc.hInstance),
            None,
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "display-event listener: CreateWindowExW failed — no OS event attribution");
                return;
            }
        };
        let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: std::mem::size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>() as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE.0,
            dbcc_classguid: GUID_DEVINTERFACE_MONITOR,
            ..Default::default()
        };
        if let Err(e) = RegisterDeviceNotificationW(
            HANDLE(hwnd.0),
            std::ptr::from_ref(&filter).cast(),
            DEVICE_NOTIFY_WINDOW_HANDLE,
        ) {
            // DBT_DEVNODES_CHANGED and WM_DISPLAYCHANGE still arrive — partial attribution.
            tracing::warn!(error = %e, "display-event listener: monitor-interface registration failed — arrival/removal detail unavailable");
        }
        SetTimer(
            Some(hwnd),
            TIMER_SAFETY,
            snapshot::SAFETY_REFRESH.as_millis() as u32,
            None,
        );
        // The first snapshot, on the actor thread (never in a caller's), before anyone can post.
        HWND_CELL.store(hwnd.0 as isize, Ordering::Release);
        refresh_inventory(hwnd);
        // Out-of-context WinEvents are delivered through this thread's message loop, so the
        // hook lives here and for the process lifetime (never unhooked). Failure leaves the
        // cursor poller's 250 ms refresh as the only secure-desktop signal.
        let hook = SetWinEventHook(
            EVENT_SYSTEM_DESKTOPSWITCH,
            EVENT_SYSTEM_DESKTOPSWITCH,
            None,
            Some(on_desktop_switch),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.0.is_null() {
            tracing::warn!(
                "display-event listener: desktop-switch hook failed — secure-desktop detection \
                 stays on the cursor poller's cadence"
            );
        }
        let hook = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        if hook.0.is_null() {
            tracing::warn!(
                "display-event listener: foreground hook failed — focus changes go unlogged"
            );
        }
        crate::input_desktop::refresh_secure_desktop();
        tracing::debug!(
            "display actor running (cached snapshot + monitor hot-plug / display-change attribution)"
        );
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

/// `EVENT_SYSTEM_DESKTOPSWITCH` carries no direction; classify the input desktop instead.
unsafe extern "system" fn on_desktop_switch(
    _hook: HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    crate::input_desktop::refresh_secure_desktop();
}

/// Names the process and window class that took the foreground. The title stays out: it can
/// carry a document or page name into a log the operator shares.
unsafe extern "system" fn on_foreground(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.is_invalid() {
        return;
    }
    let mut pid = 0u32;
    let mut class = [0u16; 128];
    // SAFETY: `hwnd` came from the WinEvent and may already be gone; both calls then fail
    // (pid 0, length 0) rather than fault. `pid` and `class` are live locals.
    let len = unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        GetClassNameW(hwnd, &mut class)
    };
    let class = String::from_utf16_lossy(&class[..len.max(0) as usize]);
    let exe = process_name(pid).unwrap_or_else(|| "?".into());
    tracing::debug!(pid, exe = %exe, class = %class, "foreground window changed");
}

/// Image base name for `pid`; `None` when the process is gone or protected.
pub fn process_name(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    // SAFETY: plain FFI; a refused open returns Err (checked via `ok()?`), and the returned
    // handle is closed exactly once below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: `process` is the live handle just opened with QUERY_LIMITED; `buf`/`len` are a
    // valid out-buffer and its capacity; `len` is the written UTF-16 length on success.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: `process` is the handle opened above, closed exactly once here.
    unsafe {
        let _ = CloseHandle(process);
    }
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string())
}
