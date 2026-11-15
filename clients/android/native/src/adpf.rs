//! Android Adaptive Performance Framework (ADPF) — CPU performance hints for the decode thread.
//!
//! ADPF lets a latency-critical app tell the platform "these threads run a repeating workload with
//! this per-cycle deadline, and here's how long they *actually* took." The kernel's CPU governor
//! (on Qualcomm Snapdragon in particular — its ADPF backend is among the most responsive) then keeps
//! those threads on the fast cores at high clocks instead of migrating them to a little core or
//! down-clocking between frames. For a stream client the win is on the in-process hot path we
//! control — the `pf-decode` feed/drain/present loop — *not* the hardware codec itself (that decodes
//! in the mediacodec service, a separate process we can't hint); keeping our loop from being
//! scheduled late directly trims the jitter between "AU received" and "buffer released to the
//! Surface." It complements the codec-side `operating-rate`/`priority` hints, which push the codec's
//! own clocks.
//!
//! The `APerformanceHint_*` API arrived in NDK **API level 33**. minSdk is 31, so we CANNOT link the
//! symbols directly: a `libpunktfunk_android.so` carrying an unresolved
//! `APerformanceHint_createSession` import fails to load on API 31/32 devices
//! (`System.loadLibrary` throws) even if the code path is never taken. Instead we resolve the
//! entry points from `libandroid.so` with `dlsym` at runtime — absent on < 33 ⇒
//! [`HintSession::create`] returns `None` and the decode loop simply runs without hints.

use std::ffi::c_void;
use std::os::raw::c_int;

// `APerformanceHint_*` function-pointer types. The manager/session handles are opaque, so we treat
// them as `*mut c_void`.
type GetManagerFn = unsafe extern "C" fn() -> *mut c_void;
type CreateSessionFn = unsafe extern "C" fn(*mut c_void, *const i32, usize, i64) -> *mut c_void;
type ReportFn = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type UpdateTargetFn = unsafe extern "C" fn(*mut c_void, i64) -> c_int;
type CloseFn = unsafe extern "C" fn(*mut c_void);
type SetPreferPowerEfficiencyFn = unsafe extern "C" fn(*mut c_void, bool) -> c_int;
type WorkCreateFn = unsafe extern "C" fn() -> *mut c_void;
type WorkReleaseFn = unsafe extern "C" fn(*mut c_void);
type WorkSetFn = unsafe extern "C" fn(*mut c_void, i64);
type Report2Fn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

/// `AWorkDuration` and `reportActualWorkDuration2`, NDK **API 35**: a report that splits CPU
/// from GPU time, so the GPU clock can be raised as well as the cores.
struct WorkApi {
    create: WorkCreateFn,
    release: WorkReleaseFn,
    set_start: WorkSetFn,
    set_total: WorkSetFn,
    set_cpu: WorkSetFn,
    set_gpu: WorkSetFn,
    report2: Report2Fn,
}

/// The entry points we use, resolved once from `libandroid.so`, plus the process-wide manager.
struct Api {
    create_session: CreateSessionFn,
    report: ReportFn,
    update_target: UpdateTargetFn,
    close: CloseFn,
    /// `APerformanceHint_setPreferPowerEfficiency` — NDK **API 35**, so `Option`al even when the
    /// rest of ADPF resolved (a 33/34 device has the session API but not this one).
    set_prefer_power_efficiency: Option<SetPreferPowerEfficiencyFn>,
    /// API 35, as above.
    work: Option<WorkApi>,
    manager: *mut c_void,
}

/// Resolve the ADPF entry points + the process manager, or `None` on API < 33 (symbols absent) or if
/// the manager is unavailable.
fn resolve_api() -> Option<Api> {
    // SAFETY: `libandroid.so` is mapped into every app process, so `dlopen` only bumps its
    // refcount (never closed — a process-lifetime handle; null is checked). Each `sym` type is the
    // NDK header's signature for that name. A required symbol absent = API < 33 = no ADPF.
    unsafe {
        let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
        if lib.is_null() {
            return None;
        }
        let get_manager: GetManagerFn = crate::sym(lib, c"APerformanceHint_getManager")?;
        let create_session = crate::sym(lib, c"APerformanceHint_createSession")?;
        let report = crate::sym(lib, c"APerformanceHint_reportActualWorkDuration")?;
        let update_target = crate::sym(lib, c"APerformanceHint_updateTargetWorkDuration")?;
        let close = crate::sym(lib, c"APerformanceHint_closeSession")?;
        let manager = get_manager();
        if manager.is_null() {
            return None;
        }
        Some(Api {
            create_session,
            report,
            update_target,
            close,
            // API 35; the session works without it.
            set_prefer_power_efficiency: crate::sym(
                lib,
                c"APerformanceHint_setPreferPowerEfficiency",
            ),
            work: (|| {
                Some(WorkApi {
                    create: crate::sym(lib, c"AWorkDuration_create")?,
                    release: crate::sym(lib, c"AWorkDuration_release")?,
                    set_start: crate::sym(lib, c"AWorkDuration_setWorkPeriodStartTimestampNanos")?,
                    set_total: crate::sym(lib, c"AWorkDuration_setActualTotalDurationNanos")?,
                    set_cpu: crate::sym(lib, c"AWorkDuration_setActualCpuDurationNanos")?,
                    set_gpu: crate::sym(lib, c"AWorkDuration_setActualGpuDurationNanos")?,
                    report2: crate::sym(lib, c"APerformanceHint_reportActualWorkDuration2")?,
                })
            })(),
            manager,
        })
    }
}

/// A live ADPF hint session bound to a set of thread ids. Dropping it closes the session. Holds raw
/// handles, so it is `!Send`/`!Sync` — created and used only on the `pf-decode` thread.
pub struct HintSession {
    api: Api,
    session: *mut c_void,
    /// The one `AWorkDuration` [`HintSession::report_work`] refills; null below API 35.
    work: *mut c_void,
}

impl HintSession {
    /// Open a session hinting `tids` with an initial per-frame target of `target_ns` nanoseconds.
    /// `None` when ADPF is unavailable (device API < 33) or the platform declines — the caller then
    /// runs unhinted (a no-op, not an error). `prefer_performance` (the experimental low-latency
    /// mode) additionally biases the governor away from power efficiency (API 35+); off, the
    /// session runs with the platform default, as it did before the overhaul.
    pub fn create(target_ns: i64, tids: &[i32], prefer_performance: bool) -> Option<Self> {
        if target_ns <= 0 || tids.is_empty() {
            return None;
        }
        let api = resolve_api()?;
        // SAFETY: `api.manager` is the live process manager returned above; `tids` is a valid slice
        // of `len` i32s that `createSession` copies; it returns null on failure (checked).
        let session =
            unsafe { (api.create_session)(api.manager, tids.as_ptr(), tids.len(), target_ns) };
        if session.is_null() {
            return None;
        }
        // Tell the governor NOT to bias this session toward power efficiency (API 35+): our loop is
        // latency-critical, so we want it kept on fast cores at high clocks over battery savings.
        // Best-effort; absent below API 35.
        if prefer_performance {
            if let Some(f) = api.set_prefer_power_efficiency {
                // SAFETY: `session` is the live session just created; the fn takes it + a bool.
                unsafe { f(session, false) };
            }
        }
        let work = match &api.work {
            // SAFETY: the NDK constructor; null on failure, which `report_work` checks.
            Some(w) => unsafe { (w.create)() },
            None => std::ptr::null_mut(),
        };
        Some(Self { api, session, work })
    }

    /// Report one frame split into CPU and GPU time (API 35). `start_ns` is
    /// `CLOCK_MONOTONIC`. Below API 35 the total goes through [`HintSession::report_actual`].
    pub fn report_work(&self, start_ns: i64, total_ns: i64, cpu_ns: i64, gpu_ns: i64) {
        let Some(w) = self.api.work.as_ref().filter(|_| !self.work.is_null()) else {
            return self.report_actual(total_ns);
        };
        // The platform rejects the whole report on a zero start or total, or no CPU or GPU time.
        if start_ns <= 0 || total_ns <= 0 || cpu_ns.max(gpu_ns) <= 0 {
            return;
        }
        // SAFETY: `self.work` is the live `AWorkDuration` created with the session and
        // `self.session` the live session; every setter takes the handle and an i64.
        unsafe {
            (w.set_start)(self.work, start_ns);
            (w.set_total)(self.work, total_ns);
            (w.set_cpu)(self.work, cpu_ns.max(0));
            (w.set_gpu)(self.work, gpu_ns.max(0));
            (w.report2)(self.session, self.work);
        }
    }

    /// Report the wall-clock time the hinted thread spent producing the last displayed frame. When
    /// it exceeds the session target the governor boosts the cores running the thread; when it
    /// stays under, clocks may relax. No-op on a non-positive duration (the API rejects it).
    pub fn report_actual(&self, actual_ns: i64) {
        if actual_ns <= 0 {
            return;
        }
        // SAFETY: `self.session` is a live session for `self`'s lifetime.
        unsafe { (self.api.report)(self.session, actual_ns) };
    }

    /// Update the per-frame target (e.g. after a mid-session refresh-rate change). Unused today —
    /// the decode thread restarts on renegotiation — but kept for that path.
    #[allow(dead_code)]
    pub fn update_target(&self, target_ns: i64) {
        if target_ns <= 0 {
            return;
        }
        // SAFETY: `self.session` is a live session for `self`'s lifetime.
        unsafe { (self.api.update_target)(self.session, target_ns) };
    }
}

impl Drop for HintSession {
    fn drop(&mut self) {
        // SAFETY: `self.session` was created by `createSession` and is closed exactly once, here;
        // `self.work`, when not null, was created by `AWorkDuration_create` and released here.
        unsafe {
            (self.api.close)(self.session);
            if let (Some(w), false) = (&self.api.work, self.work.is_null()) {
                (w.release)(self.work);
            }
        }
    }
}
