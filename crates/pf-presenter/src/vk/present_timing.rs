//! On-glass present stamps via `VK_KHR_present_wait`.
//!
//! `vkQueuePresentKHR` return is CPU submit, not vblank. A waiter thread
//! blocks in `vkWaitForPresentKHR` until the image is visible and stamps that.
//! Given the submit's timeline value it first stamps when our own GPU work was
//! done, which splits the compositor's share from ours. Where the engine stamps
//! presents itself (`VK_EXT_present_timing`) its stamp replaces the wake time, and
//! a present it reports as never shown yields no sample.
//!
//! [`PresentTimer::drain`] before `vkDestroySwapchainKHR` and before any
//! `vkCreateSwapchainKHR` that names the live swapchain as `oldSwapchain` —
//! that create externally-synchronises the old handle and can retire it under
//! a parked waiter. 250 ms wait cap: ids complete in submission order (a
//! MAILBOX-replaced id completes with the present that replaced it); a wait
//! only outlives that cap when the pipeline is already wedged.
//!
//! `vkWaitForPresentKHR`, `vkAcquireNextImageKHR` and `vkQueuePresentKHR` each
//! externally synchronize the swapchain. Every such call holds
//! [`PresentTimer::swapchain_guard`]'s lock for one call of at most [`SLICE_NS`];
//! the waiter waits in slices and lets a waiting presenter go first.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use ash::vk;

use super::timing_ext::{Engine, Offset, Stamp};

/// Longest single wait on the swapchain. 1 ms bounds how long a present waits for the
/// waiter, and a driver with millisecond timeouts still blocks rather than spins.
pub(crate) const SLICE_NS: u64 = 1_000_000;

/// Host sync for the swapchain between the presenter thread and the waiter.
#[derive(Default)]
struct SwapchainSync {
    lock: Mutex<()>,
    /// The presenter is blocked on `lock`; the waiter backs off before its next slice.
    presenter_waiting: AtomicBool,
}

pub(crate) struct PresentedSample {
    /// Capture stamp (host clock) — the e2e latency anchor.
    pub pts_ns: u64,
    /// Decode-complete stamp (client clock) — the display-stage anchor.
    pub decoded_ns: u64,
    /// `vkQueuePresentKHR` return (client clock) — pace/latch split:
    /// submitted−decoded is pipeline, displayed−submitted is the vsync latch.
    pub submitted_ns: u64,
    /// Our GPU work for this present finished (client clock). 0 when not waited.
    pub gpu_done_ns: u64,
    /// The image is visible (client clock): the engine's stamp, or the instant the
    /// present wait completed.
    pub displayed_ns: u64,
    /// `displayed_ns` is the engine's own stamp, not a wake time.
    pub exact: bool,
}

/// One present handed to the waiter.
pub(crate) struct Job {
    pub(crate) swapchain: vk::SwapchainKHR,
    pub(crate) present_id: u64,
    /// The submit's timeline signal, waited before the present.
    pub(crate) done: Option<(vk::Semaphore, u64)>,
    /// The present asked the engine for a stamp.
    pub(crate) stamped: bool,
    pub(crate) pts_ns: u64,
    pub(crate) decoded_ns: u64,
    pub(crate) submitted_ns: u64,
}

/// Run-loop wake (SDL event push), shared with the waiter thread.
type WakeSlot = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

/// Upstream keeps one frame in flight, so queue depth stays ~1.
pub(crate) struct PresentTimer {
    tx: Option<mpsc::Sender<Job>>,
    /// Enqueued but unfinished — drain barrier and the glass gate's in-flight count.
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    /// After each wait. The run loop installs an SDL wake so a gate reopen
    /// never waits out the event-loop timeout.
    wake: WakeSlot,
    sync: Arc<SwapchainSync>,
    /// Presents the engine reported as never shown, since the last take.
    unshown: Arc<AtomicUsize>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// An engine stamp this far before the submit or after the wake is on another clock.
const PLAUSIBLE_NS: u64 = 50_000_000;
/// This many stamps in a row off the session clock, and the wake time stands in for good.
const DISTRUST_AFTER: u32 = 8;
/// Stamps kept for jobs still queued: a replaced present's wait completes with its
/// successor's, and one poll returns both.
const STASH: usize = 32;

/// Engine stamps read ahead of the jobs they belong to.
struct Stamps {
    engine: Arc<Engine>,
    offset: Offset,
    stash: VecDeque<Stamp>,
    /// Consecutive stamps that failed [`PLAUSIBLE_NS`].
    off_clock: u32,
}

impl Stamps {
    /// The engine's word on `job`: `Some(Some(ns))` shown then, `Some(None)` never shown,
    /// `None` no word (not asked, not reported yet, or the stamps are distrusted).
    fn of(&mut self, sync: &SwapchainSync, job: &Job, wake_ns: u64) -> Option<Option<u64>> {
        // Polled on every job while results are outstanding: a present without a request
        // of its own still drains the queue the earlier ones fill.
        let stashed = self.stash.iter().any(|s| s.present_id == job.present_id);
        if !stashed && self.engine.outstanding() > 0 {
            let _swapchain = sync.lock.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `job.swapchain` is live until this job leaves `pending`, and the
            // lock above is its host sync.
            let fresh = unsafe { self.engine.poll(job.swapchain, &mut self.offset) };
            self.stash.extend(fresh);
            while self.stash.len() > STASH {
                self.stash.pop_front();
            }
        }
        if !job.stamped {
            return None;
        }
        let at = self
            .stash
            .iter()
            .position(|s| s.present_id == job.present_id)?;
        let stamp = self.stash.drain(..=at).next_back()?;
        if self.off_clock >= DISTRUST_AFTER {
            return None;
        }
        let Some(ns) = stamp.displayed_ns else {
            return Some(None);
        };
        let plausible = ns + PLAUSIBLE_NS >= job.submitted_ns && ns <= wake_ns + PLAUSIBLE_NS;
        if !plausible {
            self.off_clock += 1;
            if self.off_clock == DISTRUST_AFTER {
                tracing::warn!(
                    stamp_ns = ns,
                    submitted_ns = job.submitted_ns,
                    wake_ns,
                    "engine display stamps are off the session clock; using wake times"
                );
            }
            return None;
        }
        self.off_clock = 0;
        Some(Some(ns))
    }
}

/// The driver call that blocks until a present is visible.
pub(crate) enum Waiter {
    /// `VK_KHR_present_wait`.
    V1(ash::khr::present_wait::Device),
    /// `VK_KHR_present_wait2`, its entry point loaded by name.
    V2 {
        device: vk::Device,
        wait: super::setup::present_wait2::WaitFn,
    },
}

impl Waiter {
    /// # Safety
    /// `swapchain` is live and host-synchronised by the caller for this call.
    unsafe fn wait(
        &self,
        swapchain: vk::SwapchainKHR,
        present_id: u64,
        timeout_ns: u64,
    ) -> ash::prelude::VkResult<()> {
        match self {
            // SAFETY: the caller's contract is this call's.
            Waiter::V1(d) => unsafe { d.wait_for_present(swapchain, present_id, timeout_ns) },
            Waiter::V2 { device, wait } => {
                let info = super::setup::present_wait2::WaitInfo::new(present_id, timeout_ns);
                // SAFETY: the caller's contract; `info` outlives the call, and `wait` is
                // this device's own `vkWaitForPresent2KHR`.
                unsafe { wait(*device, swapchain, &info) }.result()
            }
        }
    }
}

/// The present wait for up to 250 ms in [`SLICE_NS`] calls, each under the swapchain
/// lock. 250 ms: ids complete in order; longer means the pipeline is wedged.
fn wait_sliced(wait_d: &Waiter, sync: &SwapchainSync, job: &Job) -> ash::prelude::VkResult<()> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        // Sleep, not spin: a boosted waiter could starve the presenter on a shared core.
        while sync.presenter_waiting.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_micros(50));
        }
        let r = {
            let _swapchain = sync.lock.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `job.swapchain` stays live for this call — enqueue runs while the
            // swapchain exists, and `drain`/Drop wait it out first. The lock above is the
            // swapchain's host sync against the presenter's acquire and present.
            unsafe { wait_d.wait(job.swapchain, job.present_id, SLICE_NS) }
        };
        match r {
            Err(vk::Result::TIMEOUT) if Instant::now() < deadline => {}
            r => return r,
        }
    }
}

impl PresentTimer {
    /// `engine`: where presents carry `VK_EXT_present_timing` requests, its stamps replace
    /// the wake time.
    pub(crate) fn spawn(wait_d: Waiter, device: ash::Device, engine: Option<Arc<Engine>>) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let wake: WakeSlot = Arc::new(Mutex::new(None));
        let sync = Arc::new(SwapchainSync::default());
        let unshown = Arc::new(AtomicUsize::new(0));
        let (pending_t, results_t, wake_t, sync_t, unshown_t) = (
            pending.clone(),
            results.clone(),
            wake.clone(),
            sync.clone(),
            unshown.clone(),
        );
        let mut stamps = engine.map(|engine| Stamps {
            engine,
            offset: Offset::default(),
            stash: VecDeque::with_capacity(STASH),
            off_clock: 0,
        });
        let join = std::thread::Builder::new()
            .name("pf-present-wait".into())
            .spawn(move || {
                // The on-glass stamp is taken at wake; scheduler delay reads as latch.
                pf_client_core::audio_rt::boost_and_log("present-wait");
                while let Ok(job) = rx.recv() {
                    let mut gpu_done_ns = 0;
                    if let Some((sem, value)) = job.done {
                        let semaphores = [sem];
                        let values = [value];
                        let info = vk::SemaphoreWaitInfo::default()
                            .semaphores(&semaphores)
                            .values(&values);
                        // SAFETY: `sem` is the presenter's timeline semaphore, alive until
                        // teardown, which drains this thread first.
                        if unsafe { device.wait_semaphores(&info, 250_000_000) }.is_ok() {
                            gpu_done_ns = pf_client_core::session::now_ns();
                        }
                    }
                    let r = wait_sliced(&wait_d, &sync_t, &job);
                    if r.is_ok() {
                        let wake_ns = pf_client_core::session::now_ns();
                        let engine = stamps.as_mut().and_then(|s| s.of(&sync_t, &job, wake_ns));
                        if let Some(Some(ns)) = engine {
                            tracing::trace!(
                                wake_lag_us = (wake_ns as i64 - ns as i64) / 1000,
                                "present wait completed after the engine's stamp"
                            );
                        }
                        match engine {
                            Some(None) => {
                                unshown_t.fetch_add(1, Ordering::Relaxed);
                            }
                            shown => results_t.lock().unwrap().push(PresentedSample {
                                pts_ns: job.pts_ns,
                                decoded_ns: job.decoded_ns,
                                submitted_ns: job.submitted_ns,
                                gpu_done_ns,
                                displayed_ns: shown.flatten().unwrap_or(wake_ns),
                                exact: shown.is_some(),
                            }),
                        }
                    }
                    // Wait failed: no sample. The frame still showed, or the loop
                    // is about to find out — do not poison the stats window.
                    pending_t.fetch_sub(1, Ordering::AcqRel);
                    // Wake after the count dropped so the run loop sees the
                    // post-completion state. The callback is an SDL event push
                    // and must not reenter this type.
                    if let Some(cb) = wake_t.lock().unwrap().as_ref() {
                        cb();
                    }
                }
            })
            .expect("spawn pf-present-wait");
        PresentTimer {
            tx: Some(tx),
            pending,
            results,
            wake,
            sync,
            unshown,
            join: Some(join),
        }
    }

    /// Presents the engine reported as never shown, since the last call.
    pub(crate) fn take_unshown(&self) -> u32 {
        self.unshown.swap(0, Ordering::Relaxed) as u32
    }

    /// The swapchain's host sync on the presenter thread: hold it across one acquire or
    /// present call. Waits out at most one [`SLICE_NS`] wait of the waiter.
    pub(crate) fn swapchain_guard(&self) -> MutexGuard<'_, ()> {
        self.sync.presenter_waiting.store(true, Ordering::Release);
        let guard = self
            .sync
            .lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.sync.presenter_waiting.store(false, Ordering::Release);
        guard
    }

    pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(cb);
    }

    /// Undisplayed presents, including waits that will end SUBOPTIMAL/TIMEOUT.
    /// Those resolve within 250 ms, past the gate's 100 ms stale force-open.
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn enqueue(&self, job: Job) {
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::AcqRel);
            if tx.send(job).is_err() {
                self.pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Wait until no wait still names a swapchain. Required before
    /// `vkDestroySwapchainKHR` / `oldSwapchain` create. Capped at 250 ms.
    pub(crate) fn drain(&self) {
        while self.pending.load(Ordering::Acquire) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    pub(crate) fn take_samples(&self) -> Vec<PresentedSample> {
        std::mem::take(&mut *self.results.lock().unwrap())
    }
}

impl Drop for PresentTimer {
    fn drop(&mut self) {
        // Dropping `tx` ends recv; join waits out any in-flight 250 ms wait.
        self.tx.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
