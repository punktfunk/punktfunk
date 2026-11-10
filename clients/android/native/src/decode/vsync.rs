//! The vsync clock behind the timeline presenter: an `AChoreographer` thread publishing the
//! panel's vsync grid + upcoming frame timelines, and pulsing the decode loop's event channel so
//! a frame parked on a closed glass budget gets its retry tick.
//!
//! On API 33+ the thread rides `AChoreographer_postVsyncCallback`, whose callback payload carries
//! the platform's FRAME TIMELINES — for each upcoming refresh, when SurfaceFlinger expects to
//! present and the deadline by which a frame must be submitted to make it. That pair is exactly
//! what `AMediaCodec_releaseOutputBufferAtTime` wants as its target. On 31/32 the older
//! `postFrameCallback64` supplies only the vsync instant; the presenter then releases ASAP
//! (identical to the legacy path) and uses the measured period purely to predict the latch for
//! its glass budget.
//!
//! Every `AChoreographer_*` symbol is dlsym-resolved from `libandroid.so` (mirrors
//! [`super::setup::try_set_frame_rate`]): several sit above the crate's API floor, and one hard
//! import of a too-new symbol fails `System.loadLibrary` on every older device.
//!
//! Started LAZILY on the first decoded frame (the Apple deadline presenter's bootstrap lesson:
//! an eagerly started clock ticks uselessly for the whole connect window), stopped + joined via
//! [`VsyncClock`]'s `Drop`.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sys::now_monotonic_ns;
use crate::vsync_grid::{onto_panel_grid, pick_timeline, smoothed_period, FrameTimeline};

/// State the choreographer thread publishes and the decode loop reads. All monotonic ns.
pub(super) struct VsyncShared {
    stop: AtomicBool,
    /// The latest vsync callback's frame time (0 = no callback yet).
    last_vsync_ns: AtomicI64,
    /// Estimated vsync period (EMA over callback deltas / timeline spacing; 0 = unmeasured).
    ///
    /// ⚠ This is the APP's render rate, not necessarily the panel's: Android down-rates a
    /// process's vsync stream (frame-rate categories / per-uid overrides), so a quiet UI can be
    /// served 60 Hz callbacks while the panel scans at 120 (observed on-glass, A024). Pacing
    /// video to THIS rate would cap the stream — hence `panel_period_ns` + the subdivision in
    /// [`Self::next_target`].
    period_ns: AtomicI64,
    /// The panel's own refresh period — the grid SurfaceFlinger actually latches on (0 = unknown).
    /// Seeded from the display mode Kotlin resolved at stream start and then corrected by
    /// measurement; the learner itself is [`punktfunk_core::phase::PanelGrid`], owned by the
    /// choreographer thread (see [`CallbackCtx::panel`]) and published here for the decode loop.
    panel_period_ns: AtomicI64,
    /// Callback count, for the one-shot cadence diagnostic log.
    ticks: std::sync::atomic::AtomicU32,
    /// The latest callback's upcoming timelines, soonest first. Empty on the 31/32 fallback.
    timelines: Mutex<Vec<FrameTimeline>>,
}

impl VsyncShared {
    /// The measured vsync period, or 0 while unmeasured.
    pub(super) fn period_ns(&self) -> i64 {
        self.period_ns.load(Ordering::Relaxed)
    }

    /// The panel's own refresh period (0 = unknown) — for the pf-present line's decomposition.
    pub(super) fn panel_period_ns(&self) -> i64 {
        self.panel_period_ns.load(Ordering::Relaxed)
    }

    /// The release target for a frame that must not be presented before `not_before_ns`:
    /// [`pick_timeline`] over the stored set, then [`onto_panel_grid`]. `None` on the 31/32
    /// fallback; the caller releases ASAP.
    pub(super) fn next_target(&self, not_before_ns: i64) -> Option<FrameTimeline> {
        let picked = {
            let g = self
                .timelines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pick_timeline(&g, self.period_ns(), not_before_ns)?
        };
        let panel = self.panel_period_ns.load(Ordering::Relaxed);
        Some(onto_panel_grid(picked, panel, not_before_ns))
    }
}

// ---- dlsym'd AChoreographer surface ----

type PostFrameCallback64 =
    unsafe extern "C" fn(*mut c_void, unsafe extern "C" fn(i64, *mut c_void), *mut c_void);
type PostVsyncCallback = unsafe extern "C" fn(
    *mut c_void,
    unsafe extern "C" fn(*const c_void, *mut c_void),
    *mut c_void,
);

struct ChoreoApi {
    get_instance: unsafe extern "C" fn() -> *mut c_void,
    /// API 33: vsync callback with frame-timeline payload. Preferred.
    post_vsync: Option<PostVsyncCallback>,
    /// API 29 fallback: frame callback with only the vsync instant.
    post_frame64: Option<PostFrameCallback64>,
    // AChoreographerFrameCallbackData accessors (API 33; present iff `post_vsync` is).
    fcd_frame_time: Option<unsafe extern "C" fn(*const c_void) -> i64>,
    fcd_timelines_len: Option<unsafe extern "C" fn(*const c_void) -> usize>,
    fcd_preferred_index: Option<unsafe extern "C" fn(*const c_void) -> usize>,
    fcd_expected_present: Option<unsafe extern "C" fn(*const c_void, usize) -> i64>,
    fcd_deadline: Option<unsafe extern "C" fn(*const c_void, usize) -> i64>,
}

impl ChoreoApi {
    /// Resolve from `libandroid.so`. `None` when even the baseline symbols are missing.
    fn resolve() -> Option<ChoreoApi> {
        // SAFETY: dlopen of the always-mapped libandroid.so (refcount bump, never closed; null is
        // checked). Each `sym` type is the NDK header's signature for that name.
        unsafe {
            let lib = libc::dlopen(c"libandroid.so".as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return None;
            }
            use crate::sym;
            let post_vsync = sym(lib, c"AChoreographer_postVsyncCallback");
            let post_frame64 = sym(lib, c"AChoreographer_postFrameCallback64");
            // Neither post entry point — no clock on this device.
            if post_vsync.is_none() && post_frame64.is_none() {
                return None;
            }
            Some(ChoreoApi {
                get_instance: sym(lib, c"AChoreographer_getInstance")?,
                post_vsync,
                post_frame64,
                fcd_frame_time: sym(lib, c"AChoreographerFrameCallbackData_getFrameTimeNanos"),
                fcd_timelines_len: sym(
                    lib,
                    c"AChoreographerFrameCallbackData_getFrameTimelinesLength",
                ),
                fcd_preferred_index: sym(
                    lib,
                    c"AChoreographerFrameCallbackData_getPreferredFrameTimelineIndex",
                ),
                fcd_expected_present: sym(
                    lib,
                    c"AChoreographerFrameCallbackData_getFrameTimelineExpectedPresentationTimeNanos",
                ),
                fcd_deadline: sym(
                    lib,
                    c"AChoreographerFrameCallbackData_getFrameTimelineDeadlineNanos",
                ),
            })
        }
    }
}

/// Everything a callback invocation needs. Owned by the choreographer thread's stack; callbacks
/// only ever fire inside that thread's looper poll, so the borrow can't outlive the thread.
struct CallbackCtx {
    api: ChoreoApi,
    choreographer: *mut c_void,
    shared: Arc<VsyncShared>,
    on_tick: Box<dyn Fn() + Send>,
    /// The panel-period learner. `Cell` rather than an atomic because it is touched from exactly
    /// one thread — callbacks only ever fire inside this thread's looper poll (see the struct
    /// doc) — and its streak state is nobody else's business; only the settled period is
    /// published, to `shared.panel_period_ns`.
    panel: std::cell::Cell<punktfunk_core::phase::PanelGrid>,
}

impl CallbackCtx {
    /// Common tail of both callback flavours: update the grid estimate, publish, pulse, re-arm.
    fn tick(&self, frame_time_ns: i64, timelines: Vec<FrameTimeline>) {
        let prev = self
            .shared
            .last_vsync_ns
            .swap(frame_time_ns, Ordering::Relaxed);
        // Panel-grid learner: timeline spacing is SurfaceFlinger's own grid, and therefore the
        // only honest witness to what the panel is doing — the configured mode is not (under a
        // per-uid frame-rate override `Display.getRefreshRate` REPORTS THE OVERRIDE, observed
        // on-glass: a 120 Hz panel read back as 60 while its timelines ran at 8.28 ms), and
        // neither is the mode Kotlin *requested* (`preferredDisplayModeId` is a hint the system
        // may refuse). Both directions matter and the asymmetry lives in `PanelGrid`.
        if timelines.len() >= 2 {
            let spacing = timelines[1].expected_present_ns - timelines[0].expected_present_ns;
            let mut grid = self.panel.get();
            if grid.observe(spacing) {
                self.shared
                    .panel_period_ns
                    .store(grid.period_ns(), Ordering::Relaxed);
                log::info!(
                    "vsync: panel grid now {:.2}ms",
                    grid.period_ns() as f64 / 1e6
                );
            }
            self.panel.set(grid);
        }
        // One-shot cadence diagnostic (3rd tick, once deltas exist): the callback cadence vs the
        // panel period is exactly the down-rating question, and this line answers it on-glass.
        if self.shared.ticks.fetch_add(1, Ordering::Relaxed) == 2 {
            let spacing = if timelines.len() >= 2 {
                timelines[1].expected_present_ns - timelines[0].expected_present_ns
            } else {
                0
            };
            log::info!(
                "vsync: cadence Δ={:.2}ms timelines={} spacing={:.2}ms panel={:.2}ms",
                if prev > 0 {
                    (frame_time_ns - prev) as f64 / 1e6
                } else {
                    0.0
                },
                timelines.len(),
                spacing as f64 / 1e6,
                self.shared.panel_period_ns.load(Ordering::Relaxed) as f64 / 1e6,
            );
        }
        let old = self.shared.period_ns.load(Ordering::Relaxed);
        if let Some(period) = smoothed_period(old, &timelines, frame_time_ns, prev) {
            self.shared.period_ns.store(period, Ordering::Relaxed);
        }
        if !timelines.is_empty() {
            let mut g = self
                .shared
                .timelines
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *g = timelines;
        }
        (self.on_tick)();
        if !self.shared.stop.load(Ordering::Relaxed) {
            self.repost();
        }
    }

    fn repost(&self) {
        // SAFETY: `choreographer` is this thread's instance; the ctx pointer stays valid for the
        // thread's life and callbacks only fire on this thread (see the struct doc).
        unsafe {
            let ud = self as *const CallbackCtx as *mut c_void;
            if let Some(post) = self.api.post_vsync {
                post(self.choreographer, on_vsync, ud);
            } else if let Some(post) = self.api.post_frame64 {
                post(self.choreographer, on_frame64, ud);
            }
        }
    }
}

/// API 33+ trampoline: harvest the frame timelines, then the common tick. Panic-free (an unwind
/// out of an `extern "C"` fn aborts).
unsafe extern "C" fn on_vsync(data: *const c_void, ud: *mut c_void) {
    // SAFETY: `ud` is the thread's `CallbackCtx`, alive for the whole poll loop (see struct doc).
    let ctx = unsafe { &*(ud as *const CallbackCtx) };
    let api = &ctx.api;
    let (mut frame_time, mut timelines) = (now_monotonic_ns(), Vec::new());
    // SAFETY: `data` is the platform's callback payload, valid for this invocation; the accessors
    // were resolved together with `post_vsync` (same API level) and are only called when present.
    unsafe {
        if let Some(f) = api.fcd_frame_time {
            frame_time = f(data);
        }
        if let (Some(len_f), Some(pref_f), Some(exp_f), Some(dl_f)) = (
            api.fcd_timelines_len,
            api.fcd_preferred_index,
            api.fcd_expected_present,
            api.fcd_deadline,
        ) {
            let len = len_f(data).min(8);
            // From the PREFERRED index on: earlier timelines are ones the platform already
            // considers missed for a frame starting now.
            let start = pref_f(data).min(len);
            timelines = (start..len)
                .map(|i| FrameTimeline {
                    expected_present_ns: exp_f(data, i),
                    deadline_ns: dl_f(data, i),
                })
                .collect();
        }
    }
    ctx.tick(frame_time, timelines);
}

/// API 29 fallback trampoline: vsync instant only.
unsafe extern "C" fn on_frame64(frame_time_ns: i64, ud: *mut c_void) {
    // SAFETY: `ud` is the thread's `CallbackCtx` (see `on_vsync`).
    let ctx = unsafe { &*(ud as *const CallbackCtx) };
    ctx.tick(frame_time_ns, Vec::new());
}

/// The clock: a dedicated looper thread the choreographer calls back on. Dropping stops + joins.
pub(super) struct VsyncClock {
    shared: Arc<VsyncShared>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl VsyncClock {
    /// Spawn the choreographer thread. `on_tick` fires once per vsync ON THAT THREAD — it must
    /// only do something cheap and `Send` (the decode loop passes an event-channel send).
    /// `panel_hz` SEEDS the panel-grid learner (0 = unknown) — the latch grid that
    /// [`VsyncShared::next_target`] subdivides onto. A seed, not a fact: it names the display
    /// mode Kotlin *requested*, and the observed timeline spacing is what settles it. `None` when the platform surface is missing
    /// (very old device) — the presenter then runs clock-less (ASAP targets, predicted-latch
    /// budget).
    pub(super) fn start(panel_hz: i32, on_tick: Box<dyn Fn() + Send>) -> Option<VsyncClock> {
        let api = ChoreoApi::resolve()?;
        let timelines_live = api.post_vsync.is_some();
        let shared = Arc::new(VsyncShared {
            stop: AtomicBool::new(false),
            last_vsync_ns: AtomicI64::new(0),
            period_ns: AtomicI64::new(0),
            panel_period_ns: AtomicI64::new(
                punktfunk_core::phase::PanelGrid::seeded(panel_hz).period_ns(),
            ),
            ticks: std::sync::atomic::AtomicU32::new(0),
            timelines: Mutex::new(Vec::new()),
        });
        let thread_shared = shared.clone();
        let join = std::thread::Builder::new()
            .name("pf-vsync".into())
            .spawn(move || {
                let looper = ndk::looper::ThreadLooper::prepare();
                // SAFETY: getInstance on a thread with a prepared looper returns this thread's
                // choreographer (never null once a looper exists).
                let choreographer = unsafe { (api.get_instance)() };
                if choreographer.is_null() {
                    log::warn!("vsync: AChoreographer_getInstance returned null — no clock");
                    return;
                }
                let ctx = CallbackCtx {
                    api,
                    choreographer,
                    shared: thread_shared,
                    on_tick,
                    panel: std::cell::Cell::new(punktfunk_core::phase::PanelGrid::seeded(panel_hz)),
                };
                ctx.repost();
                // The bounded poll doubles as the stop check: no cross-thread wake needed, worst
                // case teardown waits one timeout out. Callbacks fire inside poll_once_timeout.
                while !ctx.shared.stop.load(Ordering::Relaxed) {
                    let _ = looper.poll_once_timeout(Duration::from_millis(250));
                }
                // `ctx` drops here — after the loop, so no queued callback can outlive it (they
                // only ever fire inside this thread's poll).
            })
            .ok()?;
        log::info!(
            "vsync: choreographer clock started ({})",
            if timelines_live {
                "frame timelines"
            } else {
                "frame callback fallback"
            }
        );
        Some(VsyncClock {
            shared,
            join: Some(join),
        })
    }

    pub(super) fn shared(&self) -> &Arc<VsyncShared> {
        &self.shared
    }
}

impl Drop for VsyncClock {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
