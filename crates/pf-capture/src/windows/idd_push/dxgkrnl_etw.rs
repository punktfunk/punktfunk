//! Real-time ETW watch on `Microsoft-Windows-DxgKrnl` and `Microsoft-Windows-DXGI`.
//!
//! Event-id-filtered to the display-miniport DDI families whose servicing freezes
//! the present path, plus BltQueue enter/complete and DXGI Present starts, so a
//! stall report can name the DDI and duration instead of saying "below Windows".
//! [`EtwWatch::window_report`] is the compose-silence discriminator: DXGI presents
//! flowing while `BltQueueAddEntry` gaps = the OS dropped composed frames; both
//! silent = the content stopped presenting. Do not use DxgKrnl id 184 `Present`
//! (never fires on the redirected path) or `DWM_TIMING_INFO.cFrame` (refresh-
//! synthesized on Win11; advances without composes).
//!
//! Starting a real-time session needs admin / Performance Log Users — the packaged
//! host (service, SYSTEM) has it; a plain dev run degrades to `None` and reports
//! `etw=unavailable` instead of guessing. `FlushTimer` is 1 s, so a bracket from
//! the trailing second of a gap can land after that stall's report; the next
//! report still carries it.
//!
//! Design: `vdisplay-disturbance-immunity.md`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use pf_win_display::display_events::process_name;
use windows::core::{GUID, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, OpenTraceW, ProcessTrace, StartTraceW,
    CONTROLTRACE_HANDLE, ENABLE_TRACE_PARAMETERS, ENABLE_TRACE_PARAMETERS_VERSION_2,
    EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_FILTER_DESCRIPTOR, EVENT_FILTER_TYPE_EVENT_ID,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, PROCESSTRACE_HANDLE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, TRACE_LEVEL_INFORMATION,
    WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

/// `Microsoft-Windows-DxgKrnl`.
const DXGKRNL: GUID = GUID::from_u128(0x802EC45A_1E99_4B83_9920_87C98277BA9D);

/// `Microsoft-Windows-DXGI` — Present events fire in the presenting process.
const DXGI: GUID = GUID::from_u128(0xCA11C036_0102_4A2D_A6AD_F03CFED5D3C9);

/// QueryChildStatus 150/151, IndicateChildStatus 272, SetPowerState 154/155,
/// SetTimingsFromVidPn 430, DisplayDetectControl 1096/1097 (absent on older
/// builds; filtering them in is harmless), plus [`BLT_ADD_ID`] / [`BLT_COMPLETE_ID`].
const FILTER_IDS: [u16; 10] = [
    150,
    151,
    272,
    154,
    155,
    430,
    1096,
    1097,
    BLT_ADD_ID,
    BLT_COMPLETE_ID,
];

/// `BltQueueAddEntry`: one event per frame entering the virtual display's kernel present queue.
const BLT_ADD_ID: u16 = 1071;

/// `BltQueueCompleteIndirectPresent`: a frame completed from the queue to IddCx.
const BLT_COMPLETE_ID: u16 = 1068;

const DXGI_FILTER_IDS: [u16; 2] = [DXGI_PRESENT_ID, DXGI_PRESENT_MPO_ID];
/// DXGI `Present` start — one per app/DWM present call.
const DXGI_PRESENT_ID: u16 = 42;
/// DXGI `PresentMultiplaneOverlay` start.
const DXGI_PRESENT_MPO_ID: u16 = 55;

/// Machine-global real-time session name. Stopped-if-stale at start: a crashed host leaves it behind.
const SESSION: &str = "punktfunk-stallwatch-dxgkrnl";

/// Consumer callback destination, capped. 16384 ≈ 15–60 s at 90–240 Hz under a game + DWM,
/// more than any single stall window (a deeper hole's counts read as floors). DxgKrnl and DXGI
/// event-id spaces are disjoint, so one ring holds both without a provider tag.
static RING: Mutex<VecDeque<(i64, u16, u32)>> = Mutex::new(VecDeque::new());

const RING_CAP: usize = 16384;

fn qpc_now() -> i64 {
    let mut v = 0i64;
    // SAFETY: plain FFI; `v` is a valid local out-param.
    let _ = unsafe { QueryPerformanceCounter(&mut v) };
    v
}

fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        let mut f = 0i64;
        // SAFETY: plain FFI; `f` is a valid local out-param; QPC frequency is fixed at boot.
        let _ = unsafe { QueryPerformanceFrequency(&mut f) };
        f.max(1)
    })
}

/// Record id + timestamp + pid into the ring. `TimeStamp` is raw QPC only because both
/// halves of the clock contract hold: `ClientContext = 1` makes QPC the session clock, and
/// the consumer is opened with `PROCESS_TRACE_MODE_RAW_TIMESTAMP`. Without the flag,
/// ProcessTrace converts every timestamp to FILETIME regardless of session clock, and every
/// `ts <= to_q` comparison is against the wrong clock — a witness that silently reads empty.
unsafe extern "system" fn on_event(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    // SAFETY: `record` is the live event the consumer is delivering for the duration of this call.
    let (id, ts, pid) = unsafe {
        (
            (*record).EventHeader.EventDescriptor.Id,
            (*record).EventHeader.TimeStamp,
            (*record).EventHeader.ProcessId,
        )
    };
    // Poison-tolerant: this `extern "system"` callback runs on an OS thread, so a panic
    // here unwinds across FFI and aborts the host. Recovering the guard also makes poison
    // unreachable — nothing else under this lock can panic.
    let mut ring = RING.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() == RING_CAP {
        ring.pop_front();
    }
    ring.push_back((ts, id, pid));
}

/// Live session: controller handle stops the session on drop; consumer handle is the ProcessTrace pump.
pub(super) struct EtwWatch {
    session: CONTROLTRACE_HANDLE,
    consumer: PROCESSTRACE_HANDLE,
}

// SAFETY: both fields are kernel handle VALUES (u64 wrappers) owned by this watch.
// window_report reads the static ring; Drop stops/closes. The singleton hands out only `Arc<EtwWatch>`.
unsafe impl Send for EtwWatch {}
// SAFETY: as above — `&EtwWatch` exposes only `window_report` (static-ring reads).
unsafe impl Sync for EtwWatch {}

static WATCH: Mutex<Weak<EtwWatch>> = Mutex::new(Weak::new());

/// Process-wide watch, started on first use. `None` when the session cannot start (no admin
/// on a dev run) — callers report `etw=unavailable` rather than guessing.
pub(super) fn acquire() -> Option<Arc<EtwWatch>> {
    let mut g = WATCH.lock().unwrap();
    if let Some(w) = g.upgrade() {
        return Some(w);
    }
    let w = Arc::new(EtwWatch::start()?);
    *g = Arc::downgrade(&w);
    Some(w)
}

/// `EVENT_TRACE_PROPERTIES` plus trailing session-name space ETW writes into.
fn properties_buffer() -> (Vec<u8>, usize) {
    let base = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = base + (SESSION.len() + 1) * 2;
    (vec![0u8; total], base)
}

impl EtwWatch {
    fn start() -> Option<Self> {
        let name: Vec<u16> = SESSION.encode_utf16().chain([0]).collect();
        // A stale session from a crashed host blocks StartTrace with ERROR_ALREADY_EXISTS —
        // stop it by name first (fails benignly when there is none).
        let (mut stop_buf, _) = properties_buffer();
        // SAFETY: `stop_buf` is a live, zeroed, correctly-sized properties allocation; the name is
        // a live nul-terminated wide string; a session handle of 0 + name = control-by-name.
        unsafe {
            let props = stop_buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            (*props).Wnode.BufferSize = stop_buf.len() as u32;
            let _ = ControlTraceW(
                CONTROLTRACE_HANDLE::default(),
                PWSTR(name.as_ptr() as *mut _),
                props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }

        let (mut buf, base) = properties_buffer();
        let mut session = CONTROLTRACE_HANDLE::default();
        // SAFETY: `buf` is a live, zeroed allocation of base + name bytes; every write below is a
        // field of the properties struct at its head; `LoggerNameOffset = base` points at the
        // appended name space (ETW copies the name there itself).
        let rc = unsafe {
            let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            (*props).Wnode.BufferSize = buf.len() as u32;
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*props).Wnode.ClientContext = 1;
            (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*props).BufferSize = 64;
            (*props).MinimumBuffers = 2;
            (*props).MaximumBuffers = 4;
            (*props).FlushTimer = 1;
            (*props).LoggerNameOffset = base as u32;
            StartTraceW(&mut session, PWSTR(name.as_ptr() as *mut _), props)
        };
        if rc != ERROR_SUCCESS {
            tracing::debug!(
                rc = rc.0,
                "DxgKrnl ETW session unavailable (needs admin / Performance Log Users) — \
                 stall reports will say etw=unavailable"
            );
            return None;
        }
        // Fresh session, fresh ring: [`RING`] outlives any `EtwWatch`, so leftover events belong
        // to a dead session. Race-free here: the consumer thread that repopulates it is spawned below.
        RING.lock().unwrap().clear();

        // Kernel-side event-id filter: the provider's vblank/DPC keywords never reach us.
        // Fatal on failure — the DDI families + queue witnesses are this watch's reason to exist.
        if !enable_provider(session, &DXGKRNL, &FILTER_IDS) {
            tracing::debug!("DxgKrnl ETW enable failed — stopping the session");
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handle + valid properties allocation, stopped exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            }
            return None;
        }
        // DXGI present witness rides the same session. Degraded-not-fatal: a refusal only costs
        // per-process present counts — `window_report` then reports no present history, never a guess.
        if !enable_provider(session, &DXGI, &DXGI_FILTER_IDS) {
            tracing::debug!(
                "DXGI ETW enable failed — present-vs-queue discrimination unavailable this session"
            );
        }

        let mut log = EVENT_TRACE_LOGFILEW {
            LoggerName: PWSTR(name.as_ptr() as *mut _),
            ..Default::default()
        };
        // RAW_TIMESTAMP stops ProcessTrace converting `EVENT_HEADER.TimeStamp` to FILETIME, so
        // events arrive in the session clock (QPC, ClientContext above) — the clock window edges use.
        log.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
            | PROCESS_TRACE_MODE_EVENT_RECORD
            | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
        log.Anonymous2.EventRecordCallback = Some(on_event);
        // SAFETY: `log` is a fully-initialized local; `name` outlives the call (OpenTrace copies
        // what it needs before returning).
        let consumer = unsafe { OpenTraceW(&mut log) };
        if consumer.Value == u64::MAX {
            tracing::debug!("DxgKrnl ETW OpenTrace failed — stopping the session");
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handle + valid properties allocation, stopped exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            }
            return None;
        }
        // ProcessTrace blocks for the session's lifetime; Drop's STOP unblocks it.
        let consumer_value = consumer.Value;
        if let Err(e) = std::thread::Builder::new()
            .name("pf-etw-dxgkrnl".into())
            .spawn(move || {
                // SAFETY: `consumer_value` is the live consumer handle opened above; ProcessTrace
                // pumps it until the controller stops the session (Drop), then returns.
                let rc = unsafe {
                    ProcessTrace(
                        &[PROCESSTRACE_HANDLE {
                            Value: consumer_value,
                        }],
                        None,
                        None,
                    )
                };
                tracing::debug!(rc = rc.0, "DxgKrnl ETW consumer exited");
            })
        {
            tracing::debug!(error = %e, "DxgKrnl ETW consumer thread not spawned");
            let (mut buf, _) = properties_buffer();
            // SAFETY: live handles + valid properties allocation, released exactly once on this path.
            unsafe {
                let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
                (*props).Wnode.BufferSize = buf.len() as u32;
                let _ = ControlTraceW(session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
                let _ = CloseTrace(consumer);
            }
            return None;
        }
        tracing::debug!("DxgKrnl ETW stall-watch session live (event-id filtered)");
        Some(Self { session, consumer })
    }

    /// One stall window's ETW evidence, both halves from a single ring snapshot under a
    /// single `(Instant::now(), qpc_now())` anchor. Two reads would let events arriving
    /// between them make the prose and the verdict disagree.
    ///
    /// The summary covers `[hole_from - lead_in, hole_to]` — the disturbance that caused a
    /// hole lands just before DWM stops delivering. The counts cover `[hole_from, hole_to]`
    /// only: presents from the healthy flow inside the lead-in would falsely acquit the
    /// content. Brackets that merely span the summary window count too (a freeze-long
    /// `SetPowerState` has both edges outside the hole it caused). `"none"` when clean.
    pub(super) fn window_report(
        &self,
        hole_from: Instant,
        hole_to: Instant,
        lead_in: Duration,
    ) -> (String, EtwWindowCounts) {
        let (now_i, now_q, freq) = (Instant::now(), qpc_now(), qpc_freq());
        let to_q = now_q - duration_qpc(now_i.saturating_duration_since(hole_to), freq);
        let from_q = now_q - duration_qpc(now_i.saturating_duration_since(hole_from), freq);
        let summary_from_q = from_q - duration_qpc(lead_in, freq);
        // Snapshot then drop the lock: `process_name`'s OpenProcess calls run off the copy so
        // the consumer callback never queues behind a stall report.
        let events: Vec<(i64, u16, u32)> = {
            let ring = RING.lock().unwrap();
            ring.iter()
                .filter(|(ts, _, _)| *ts <= to_q)
                .copied()
                .collect()
        };
        let mut counts = count_window(&events, from_q, to_q, duration_qpc(LOOKBACK, freq));
        // Damage-idle discriminator: every lookback present from dwm.exe means the gated flow
        // was desktop composition (cursor/UI damage). A game anywhere in the lookback keeps
        // this false. Name resolution is a handful of OpenProcess calls, off the consumer thread.
        let lookback_pids = lookback_present_pids(&events, from_q, duration_qpc(LOOKBACK, freq));
        counts.flow_dwm_only = !lookback_pids.is_empty()
            && lookback_pids
                .iter()
                .all(|&pid| process_name(pid).is_some_and(|n| n.eq_ignore_ascii_case("dwm.exe")));
        let ms = |dq: i64| dq.max(0) * 1_000 / freq;
        let mut parts = Vec::new();
        for (start_id, stop_id, label) in [
            (150u16, 151u16, "QueryChildStatus"),
            (154, 155, "SetPowerState"),
            (1096, 1097, "DisplayDetectControl"),
        ] {
            let mut open: Option<i64> = None;
            let mut count = 0u32;
            let mut max_ms = 0i64;
            let mut still_open = false;
            for &(ts, id, _) in &events {
                if id == start_id {
                    open = Some(ts);
                } else if id == stop_id {
                    if let Some(s) = open.take() {
                        if s <= to_q && ts >= summary_from_q {
                            count += 1;
                            max_ms = max_ms.max(ms(ts - s));
                        }
                    }
                }
            }
            if let Some(s) = open {
                if s <= to_q {
                    count += 1;
                    max_ms = max_ms.max(ms(to_q - s));
                    still_open = true;
                }
            }
            if count > 0 {
                parts.push(format!(
                    "{label}×{count}(max {max_ms}ms{})",
                    if still_open { ", open" } else { "" }
                ));
            }
        }
        for (id, label) in [
            (272u16, "IndicateChildStatus"),
            (430, "SetTimingsFromVidPn"),
        ] {
            let count = events
                .iter()
                .filter(|(ts, i, _)| *i == id && *ts >= summary_from_q && *ts <= to_q)
                .count();
            if count > 0 {
                parts.push(format!("{label}×{count}"));
            }
        }
        // DXGI 42/55 + BltQueue: named top presenters split compose-silence into "content
        // stopped" vs "presents flowed and the path dropped them". Print `Present×0` only
        // when the witness was live before the hole ([`LOOKBACK`]) — silence is a finding;
        // a dead witness prints nothing rather than a fake zero.
        let mut per_pid: Vec<(u32, u32)> = Vec::new();
        let (mut adds, mut completes) = (0u32, 0u32);
        for &(ts, id, pid) in &events {
            if ts < summary_from_q || ts > to_q {
                continue;
            }
            match id {
                DXGI_PRESENT_ID | DXGI_PRESENT_MPO_ID => {
                    match per_pid.iter_mut().find(|(p, _)| *p == pid) {
                        Some((_, c)) => *c += 1,
                        None => per_pid.push((pid, 1)),
                    }
                }
                BLT_ADD_ID => adds += 1,
                BLT_COMPLETE_ID => completes += 1,
                _ => {}
            }
        }
        if !per_pid.is_empty() {
            per_pid.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
            let total: u32 = per_pid.iter().map(|(_, c)| c).sum();
            let top = per_pid
                .iter()
                .take(2)
                .map(|(pid, c)| {
                    let name = process_name(*pid).unwrap_or_else(|| format!("pid{pid}"));
                    format!("{name}:{c}")
                })
                .collect::<Vec<_>>()
                .join(",");
            parts.push(format!("Present×{total}({top})"));
        } else if counts.present_history {
            parts.push("Present×0".to_string());
        }
        if counts.queue_history || adds > 0 || completes > 0 {
            parts.push(format!("blt-queue add×{adds} complete×{completes}"));
        }
        let summary = if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" ")
        };
        (summary, counts)
    }
}

/// Witness-liveness lookback: history flags are true only when the stream produced
/// at least one event in the `LOOKBACK` window ending at the hole's start. An event
/// after the hole (resume burst) proves nothing about the witness during it; an event
/// that aged out of the ring would otherwise keep a stale known-working flag. 5 s is
/// far longer than any pre-stall active-flow gate, so a working witness cannot blink
/// false across a frame-time lull.
const LOOKBACK: Duration = Duration::from_secs(5);

/// Pure QPC-tick windowing (no ETW, no clock reads) so the ring→counts contract is
/// unit-testable without a session: presents (DXGI 42/55) and queue adds inside
/// `[from_q, to_q]`; liveness from `[from_q - lookback_q, from_q]`. Either
/// `BltQueueAddEntry` or `BltQueueCompleteIndirectPresent` satisfies `queue_history`.
fn count_window(
    events: &[(i64, u16, u32)],
    from_q: i64,
    to_q: i64,
    lookback_q: i64,
) -> EtwWindowCounts {
    let mut out = EtwWindowCounts::default();
    for &(ts, id, _) in events {
        let in_window = ts >= from_q && ts <= to_q;
        let in_lookback = ts >= from_q.saturating_sub(lookback_q) && ts <= from_q;
        match id {
            DXGI_PRESENT_ID | DXGI_PRESENT_MPO_ID => {
                out.present_history |= in_lookback;
                if in_window {
                    out.presents += 1;
                }
            }
            BLT_ADD_ID => {
                out.queue_history |= in_lookback;
                if in_window {
                    out.queue_adds += 1;
                }
            }
            BLT_COMPLETE_ID => out.queue_history |= in_lookback,
            _ => {}
        }
    }
    out
}

/// Structured half of [`EtwWatch::window_report`]: compose-silence discriminator evidence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct EtwWindowCounts {
    /// Swapchain presents inside the window; the game and dwm both count.
    pub(super) presents: u32,
    /// `BltQueueAddEntry` events inside the window (frames entering the kernel queue).
    pub(super) queue_adds: u32,
    /// Present stream demonstrated liveness inside [`LOOKBACK`] before the hole — a
    /// working witness whose in-window zero is a reading, not a dead one whose zero is noise.
    pub(super) present_history: bool,
    /// Queue-stream liveness inside [`LOOKBACK`] before the hole (`BltQueueAddEntry` or
    /// `BltQueueCompleteIndirectPresent` — either proves the witness works).
    pub(super) queue_history: bool,
    /// Every lookback present came from `dwm.exe` (and there was at least one): pre-hole
    /// flow was desktop composition, not a game. Set by [`EtwWatch::window_report`] (name
    /// resolution lives there); `stall::classify` requires it so a game's holes are never demoted.
    pub(super) flow_dwm_only: bool,
}

/// Distinct pids that presented (DXGI 42/55) in `[from_q - lookback_q, from_q]`.
/// Pure tick math, factored out of [`EtwWatch::window_report`] so windowing is unit-testable.
fn lookback_present_pids(events: &[(i64, u16, u32)], from_q: i64, lookback_q: i64) -> Vec<u32> {
    let mut pids = Vec::new();
    for &(ts, id, pid) in events {
        if matches!(id, DXGI_PRESENT_ID | DXGI_PRESENT_MPO_ID)
            && ts >= from_q.saturating_sub(lookback_q)
            && ts <= from_q
            && !pids.contains(&pid)
        {
            pids.push(pid);
        }
    }
    pids
}

/// Kernel-side event-id allowlist. `true` on success.
fn enable_provider(session: CONTROLTRACE_HANDLE, guid: &GUID, ids: &[u16]) -> bool {
    let mut filter = Vec::with_capacity(4 + ids.len() * 2);
    filter.extend_from_slice(&[1u8, 0u8]); // FilterIn = TRUE, Reserved
    filter.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    for id in ids {
        filter.extend_from_slice(&id.to_le_bytes());
    }
    let mut desc = EVENT_FILTER_DESCRIPTOR {
        Ptr: filter.as_ptr() as u64,
        Size: filter.len() as u32,
        Type: EVENT_FILTER_TYPE_EVENT_ID,
    };
    let params = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableProperty: 0,
        ControlFlags: 0,
        SourceId: GUID::zeroed(),
        EnableFilterDesc: &mut desc,
        FilterDescCount: 1,
    };
    // SAFETY: `session` is a live handle owned by the caller; `params`/`desc`/`filter` are live
    // locals for this synchronous call (the kernel copies the filter before returning).
    let rc = unsafe {
        EnableTraceEx2(
            session,
            guid,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION as u8,
            0,
            0,
            0,
            Some(&params),
        )
    };
    rc == ERROR_SUCCESS
}

/// `Duration` in QPC ticks. Saturating; diagnostic precision.
fn duration_qpc(d: Duration, freq: i64) -> i64 {
    (d.as_micros() as i64).saturating_mul(freq) / 1_000_000
}

impl Drop for EtwWatch {
    fn drop(&mut self) {
        let (mut buf, _) = properties_buffer();
        // SAFETY: `self.session`/`self.consumer` are the live handles this watch owns; the STOP
        // (with a valid properties allocation) ends the session and unblocks ProcessTrace, and
        // CloseTrace releases the consumer — each exactly once, here.
        unsafe {
            let props = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
            (*props).Wnode.BufferSize = buf.len() as u32;
            let _ = ControlTraceW(self.session, PWSTR::null(), props, EVENT_TRACE_CONTROL_STOP);
            let _ = CloseTrace(self.consumer);
        }
    }
}

// The module only compiles on Windows (lib.rs gates `mod windows`), so plain `cfg(test)` here
// already means "Windows tests" — and [`count_window`] itself is pure tick math, no session.
#[cfg(test)]
mod tests {
    use super::*;

    /// [`count_window`]: counts from `[from, to]`; liveness ONLY from the lookback window
    /// ending at the hole's start. An event after the hole or older than the lookback must
    /// not fly the known-working flag.
    #[test]
    fn count_window_liveness_and_windowing() {
        // Hole [1000, 2000], lookback 500 → liveness window [500, 1000]. Plain ticks.
        let (from, to, lb) = (1_000i64, 2_000i64, 500i64);
        let ev = |ts: i64, id: u16| (ts, id, 42u32);

        // Liveness before the hole, activity inside it.
        let events = [
            ev(600, DXGI_PRESENT_ID),       // lookback → present witness live
            ev(700, BLT_COMPLETE_ID),       // lookback → queue witness live (completes count)
            ev(1_100, DXGI_PRESENT_ID),     // in-window present
            ev(1_200, DXGI_PRESENT_MPO_ID), // in-window present (MPO path)
            ev(1_300, BLT_ADD_ID),          // in-window queue add
            ev(1_400, 430),                 // non-witness id: never counted here
        ];
        assert_eq!(
            count_window(&events, from, to, lb),
            EtwWindowCounts {
                presents: 2,
                queue_adds: 1,
                present_history: true,
                queue_history: true,
                // Set by `window_report` (needs name resolution), never by the tick math.
                flow_dwm_only: false,
            }
        );

        // In-window events count but do not confer liveness — the witness must have worked
        // before the hole for its zeros elsewhere to mean anything.
        let window_only = [ev(1_500, DXGI_PRESENT_ID), ev(1_600, BLT_ADD_ID)];
        let c = count_window(&window_only, from, to, lb);
        assert_eq!((c.presents, c.queue_adds), (1, 1));
        assert!(!c.present_history && !c.queue_history);

        // An event only after the hole proves nothing about the witness during it.
        let after_only = [ev(2_100, DXGI_PRESENT_ID), ev(2_200, BLT_ADD_ID)];
        assert_eq!(
            count_window(&after_only, from, to, lb),
            EtwWindowCounts::default()
        );

        // Events that aged past the lookback (a previous session's leftovers) don't either.
        let stale = [ev(499, DXGI_PRESENT_ID), ev(1, BLT_ADD_ID)];
        assert_eq!(
            count_window(&stale, from, to, lb),
            EtwWindowCounts::default()
        );

        // Both lookback edges are inclusive; the hole-start event is both liveness and count.
        let edges = [ev(500, DXGI_PRESENT_ID), ev(1_000, BLT_ADD_ID)];
        let c = count_window(&edges, from, to, lb);
        assert!(c.present_history && c.queue_history);
        assert_eq!((c.presents, c.queue_adds), (0, 1));

        // A lookback reaching below tick 0 saturates instead of wrapping.
        let c = count_window(&[ev(0, DXGI_PRESENT_ID)], 3, to, i64::MAX);
        assert!(c.present_history);
    }

    /// [`lookback_present_pids`]: presenters from the lookback window only (dedup'd),
    /// never from inside or after the hole.
    #[test]
    fn lookback_presenters_come_from_before_the_hole() {
        let (from, lb) = (1_000i64, 500i64);
        let events = [
            (600, DXGI_PRESENT_ID, 7u32),     // lookback, pid 7
            (700, DXGI_PRESENT_MPO_ID, 7u32), // lookback, pid 7 again (dedup)
            (800, DXGI_PRESENT_ID, 9u32),     // lookback, pid 9
            (900, BLT_ADD_ID, 11u32),         // lookback, but not a present
            (1_100, DXGI_PRESENT_ID, 13u32),  // inside the hole — excluded
        ];
        assert_eq!(lookback_present_pids(&events, from, lb), vec![7, 9]);
        assert!(lookback_present_pids(&events[4..], from, lb).is_empty());
    }
}
