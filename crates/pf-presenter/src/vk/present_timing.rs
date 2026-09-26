//! On-glass present stamps via `VK_KHR_present_wait`.
//!
//! `vkQueuePresentKHR` return is CPU submit, not vblank. A waiter thread
//! blocks in `vkWaitForPresentKHR` until the image is visible and stamps that.
//!
//! [`PresentTimer::drain`] before `vkDestroySwapchainKHR` and before any
//! `vkCreateSwapchainKHR` that names the live swapchain as `oldSwapchain` —
//! that create externally-synchronises the old handle and can retire it under
//! a parked waiter. 250 ms wait cap: ids complete in submission order (a
//! MAILBOX-replaced id completes with the present that replaced it); a wait
//! only outlives that cap when the pipeline is already wedged.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use ash::vk;

pub(crate) struct PresentedSample {
    /// Capture stamp (host clock) — the e2e latency anchor.
    pub pts_ns: u64,
    /// Decode-complete stamp (client clock) — the display-stage anchor.
    pub decoded_ns: u64,
    /// `vkQueuePresentKHR` return (client clock) — pace/latch split:
    /// submitted−decoded is pipeline, displayed−submitted is the vsync latch.
    pub submitted_ns: u64,
    /// `vkWaitForPresentKHR` completion: the image is visible (client clock).
    pub displayed_ns: u64,
}

struct Job {
    swapchain: vk::SwapchainKHR,
    present_id: u64,
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
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
    join: Option<std::thread::JoinHandle<()>>,
}

impl PresentTimer {
    pub(crate) fn spawn(wait_d: ash::khr::present_wait::Device) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let wake: WakeSlot = Arc::new(Mutex::new(None));
        let (pending_t, results_t, wake_t) = (pending.clone(), results.clone(), wake.clone());
        let join = std::thread::Builder::new()
            .name("pf-present-wait".into())
            .spawn(move || {
                // The on-glass stamp is taken at wake; scheduler delay reads as latch.
                pf_client_core::audio_rt::boost_and_log("present-wait");
                while let Ok(job) = rx.recv() {
                    // 250 ms: ids complete in order; longer means the pipeline is wedged.
                    // SAFETY: `job.swapchain` stays live for this call — enqueue runs
                    // while the swapchain exists, and `drain`/Drop wait it out first.
                    let r = unsafe {
                        wait_d.wait_for_present(job.swapchain, job.present_id, 250_000_000)
                    };
                    if r.is_ok() {
                        let displayed_ns = pf_client_core::session::now_ns();
                        results_t.lock().unwrap().push(PresentedSample {
                            pts_ns: job.pts_ns,
                            decoded_ns: job.decoded_ns,
                            submitted_ns: job.submitted_ns,
                            displayed_ns,
                        });
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
            join: Some(join),
        }
    }

    pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(cb);
    }

    /// Undisplayed presents, including waits that will end SUBOPTIMAL/TIMEOUT.
    /// Those resolve within 250 ms, past the gate's 100 ms stale force-open.
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn enqueue(
        &self,
        swapchain: vk::SwapchainKHR,
        present_id: u64,
        pts_ns: u64,
        decoded_ns: u64,
        submitted_ns: u64,
    ) {
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::AcqRel);
            if tx
                .send(Job {
                    swapchain,
                    present_id,
                    pts_ns,
                    decoded_ns,
                    submitted_ns,
                })
                .is_err()
            {
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
