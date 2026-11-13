//! A thread that waits each shipped picture's decode on the host and then runs a wake, so
//! the consumer can block for whichever comes first, its next AU or the pixels, instead of
//! polling the timeline.

use super::DecodedVkFrame;
use ash::vk;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};

static WAITER_BOOST: OnceLock<fn()> = OnceLock::new();

/// Install the call every [`DoneWaiter`] thread makes first, to raise its own priority: the
/// consumer wakes on it. First caller wins; later calls are ignored. Set it before the first
/// [`DoneWaiter::spawn`].
pub fn set_waiter_boost(boost: fn()) {
    let _ = WAITER_BOOST.set(boost);
}

/// Bounds one wait: a wedged decode must not pin the thread, and `Drop` joins it.
const WAIT_NS: u64 = 50_000_000;

/// The waits armed but not started, and the one in progress. One lock, so [`DoneWaiter::settle`]
/// can drop a queued wait before its semaphore goes.
#[derive(Default)]
struct Watch {
    queued: Vec<(vk::Semaphore, u64)>,
    busy: Option<vk::Semaphore>,
}

impl Watch {
    /// The thread took `job` off the channel: whether it is still wanted, now in progress.
    fn start(&mut self, job: (vk::Semaphore, u64)) -> bool {
        let Some(at) = self.queued.iter().position(|&q| q == job) else {
            return false;
        };
        self.queued.swap_remove(at);
        self.busy = Some(job.0);
        true
    }

    /// Drops every queued wait on `semaphore`.
    fn cancel(&mut self, semaphore: vk::Semaphore) {
        self.queued.retain(|&(s, _)| s != semaphore);
    }
}

/// Waits decodes for [`Self::arm`]ed frames and wakes after each. Declare it before the
/// decoder that owns the device: `Drop` joins the thread, so the wait never outlives the
/// device. [`Self::settle`] keeps a wait from outliving its semaphore.
pub struct DoneWaiter {
    jobs: Option<mpsc::Sender<(vk::Semaphore, u64)>>,
    watch: Arc<(Mutex<Watch>, Condvar)>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DoneWaiter {
    /// The thread runs the [`set_waiter_boost`] hook, then `wake` after each armed decode
    /// completes, or after 50 ms. `None` when the thread cannot start; the consumer then
    /// polls as before.
    pub fn spawn(device: ash::Device, wake: Arc<dyn Fn() + Send + Sync>) -> Option<Self> {
        let (jobs, rx) = mpsc::channel::<(vk::Semaphore, u64)>();
        let watch = Arc::new((Mutex::new(Watch::default()), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (watch_t, stop_t) = (watch.clone(), stop.clone());
        let join = std::thread::Builder::new()
            .name("pf-decode-done".into())
            .spawn(move || {
                if let Some(boost) = WAITER_BOOST.get() {
                    boost();
                }
                while let Ok(job) = rx.recv() {
                    if stop_t.load(Ordering::Acquire) {
                        break;
                    }
                    if !watch_t
                        .0
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .start(job)
                    {
                        continue;
                    }
                    let semaphores = [job.0];
                    let values = [job.1];
                    let info = vk::SemaphoreWaitInfo::default()
                        .semaphores(&semaphores)
                        .values(&values);
                    // SAFETY: `Drop` joins this thread before the owner drops the device.
                    // `settle` drops a queued wait on the semaphore and holds every release
                    // of it until this one returns, so the semaphore is live here.
                    let _ = unsafe { device.wait_semaphores(&info, WAIT_NS) };
                    watch_t.0.lock().unwrap_or_else(|e| e.into_inner()).busy = None;
                    watch_t.1.notify_all();
                    wake();
                }
            })
            .ok()?;
        Some(Self {
            jobs: Some(jobs),
            watch,
            stop,
            join: Some(join),
        })
    }

    /// Wake once `frame`'s decode is complete.
    pub fn arm(&self, frame: &DecodedVkFrame) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let job = (frame.semaphore, frame.value);
        // Queued before the send: the thread only waits a job it finds here.
        let lock = &self.watch.0;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued
            .push(job);
        if jobs.send(job).is_err() {
            let mut watch = lock.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(at) = watch.queued.iter().position(|&q| q == job) {
                watch.queued.swap_remove(at);
            }
        }
    }

    /// Drop any queued wait on `frame`'s semaphore and block while one is in progress. Call
    /// before anything that can destroy it, such as the release of a pool's last frame.
    pub fn settle(&self, frame: &DecodedVkFrame) {
        let (lock, done) = &*self.watch;
        let mut watch = lock.lock().unwrap_or_else(|e| e.into_inner());
        watch.cancel(frame.semaphore);
        while watch.busy == Some(frame.semaphore) {
            watch = done.wait(watch).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for DoneWaiter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        drop(self.jobs.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ash::vk::Handle;

    #[test]
    fn a_settled_wait_never_starts_and_a_newer_arm_still_does() {
        let s = vk::Semaphore::from_raw(7);
        let mut watch = Watch::default();
        watch.queued.push((s, 1));
        watch.cancel(s);
        watch.queued.push((s, 2));
        assert!(!watch.start((s, 1)), "the settled wait stays dropped");
        assert!(watch.start((s, 2)));
        assert_eq!(watch.busy, Some(s));
        assert!(watch.queued.is_empty());
    }

    static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Stands in for `vkWaitSemaphores`, so the waiter runs on a device that is not there.
    extern "system" fn fake_wait(
        _: vk::Device,
        _: *const vk::SemaphoreWaitInfo<'_>,
        _: u64,
    ) -> vk::Result {
        LOG.lock().unwrap().push("wait".into());
        vk::Result::SUCCESS
    }

    #[test]
    fn the_waiter_thread_boosts_before_its_first_wait() {
        set_waiter_boost(|| {
            let thread = std::thread::current();
            let line = format!("boost {}", thread.name().unwrap_or_default());
            LOG.lock().unwrap().push(line);
        });
        let load = |name: &std::ffi::CStr| match name.to_bytes() {
            b"vkWaitSemaphores" => fake_wait as *const std::ffi::c_void,
            _ => std::ptr::null(),
        };
        let device = ash::Device::from_parts_1_3(
            vk::Device::null(),
            ash::DeviceFnV1_0::load(load),
            ash::DeviceFnV1_1::load(load),
            ash::DeviceFnV1_2::load(load),
            ash::DeviceFnV1_3::load(load),
        );
        let (woke, woken) = mpsc::channel();
        let wake = Arc::new(move || {
            LOG.lock().unwrap().push("wake".into());
            let _ = woke.send(());
        });
        let waiter = DoneWaiter::spawn(device, wake).expect("the thread starts");
        let job = (vk::Semaphore::from_raw(7), 1);
        waiter.watch.0.lock().unwrap().queued.push(job);
        waiter.jobs.as_ref().unwrap().send(job).unwrap();
        woken
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the wake runs");
        drop(waiter);
        assert_eq!(
            *LOG.lock().unwrap(),
            ["boost pf-decode-done", "wait", "wake"]
        );
    }
}
