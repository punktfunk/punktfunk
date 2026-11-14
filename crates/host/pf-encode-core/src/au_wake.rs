//! How a backend that finishes access units off the session loop tells the loop: the shared
//! [`AuWake`], and [`DoneWaiter`], a thread that blocks on each armed encode's completion and
//! rings it.

use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};

/// The handle [`crate::Encoder::set_au_wake`] takes.
type Wake = Arc<dyn Fn() + Send + Sync>;

/// The loop's wake for a finished access unit ([`crate::Encoder::set_au_wake`]), shared by
/// clone with the threads that see the completion. Rings nothing until the loop hands one over.
#[derive(Clone, Default)]
pub struct AuWake(Arc<Mutex<Option<Wake>>>);

impl AuWake {
    /// Every clone rings `wake` from now on; `None` silences them.
    pub fn set(&self, wake: Option<Wake>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = wake;
    }

    /// Call the loop's wake, outside the lock.
    pub fn ring(&self) {
        let wake = self.0.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
}

/// The waits armed and not started, and the one in progress. One lock, so
/// [`DoneWaiter::settle`] drops a queued wait before its handle is reused.
struct Watch<H> {
    queued: Vec<H>,
    busy: Option<H>,
    /// The owner settled the busy handle, or is dropping the waiter: its wait ends at the
    /// slice it is in.
    give_up: bool,
}

impl<H: Copy + Eq> Watch<H> {
    /// The thread took `h` off the channel: whether it is still wanted, now in progress.
    fn start(&mut self, h: H) -> bool {
        let Some(at) = self.queued.iter().position(|&q| q == h) else {
            return false;
        };
        self.queued.swap_remove(at);
        self.busy = Some(h);
        self.give_up = false;
        true
    }
}

/// Waits each [`Self::arm`]ed handle on its own thread, then rings the [`AuWake`]. Its owner
/// calls [`Self::settle`] before it resets, reuses or destroys a handle, and drops this before
/// the device the handles live on: `Drop` joins the thread.
pub struct DoneWaiter<H: Copy + Eq + Send + 'static> {
    jobs: Option<mpsc::Sender<H>>,
    watch: Arc<(Mutex<Watch<H>>, Condvar)>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl<H: Copy + Eq + Send + 'static> DoneWaiter<H> {
    /// `wait` blocks on one completion for a bounded slice: `true` once it completed or failed,
    /// `false` when the slice ran out. `None` when the thread can't start; the owner's AUs
    /// then wait for the loop's next tick.
    pub fn spawn(
        name: &str,
        wait: impl Fn(H) -> bool + Send + 'static,
        wake: AuWake,
    ) -> Option<Self> {
        let (jobs, rx) = mpsc::channel::<H>();
        let watch = Arc::new((
            Mutex::new(Watch {
                queued: Vec::new(),
                busy: None,
                give_up: false,
            }),
            Condvar::new(),
        ));
        let thread_watch = watch.clone();
        let join = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                pf_frame::thread_qos::boost_thread_priority(false);
                let (lock, done) = &*thread_watch;
                while let Ok(h) = rx.recv() {
                    if !locked(lock).start(h) {
                        continue;
                    }
                    let completed = loop {
                        if wait(h) {
                            break true;
                        }
                        if locked(lock).give_up {
                            break false;
                        }
                    };
                    locked(lock).busy = None;
                    done.notify_all();
                    if completed {
                        wake.ring();
                    }
                }
            })
            .ok()?;
        Some(Self {
            jobs: Some(jobs),
            watch,
            join: Some(join),
        })
    }

    /// Ring once `h` completes. Call after the submission that signals it.
    pub fn arm(&self, h: H) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let lock = &self.watch.0;
        // Queued before the send: the thread waits only a job it finds here.
        locked(lock).queued.push(h);
        if jobs.send(h).is_err() {
            locked(lock).queued.retain(|&q| q != h);
        }
    }

    /// Drop any queued wait on `h` and block while one is in progress, which gives up at its
    /// slice. Call before `h` is reset, reused or destroyed.
    pub fn settle(&self, h: H) {
        let (lock, done) = &*self.watch;
        let mut watch = locked(lock);
        watch.queued.retain(|&q| q != h);
        if watch.busy == Some(h) {
            watch.give_up = true;
        }
        while watch.busy == Some(h) {
            watch = done.wait(watch).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl<H: Copy + Eq + Send + 'static> Drop for DoneWaiter<H> {
    /// Nothing queued starts, the wait in progress ends at its slice, and the thread is joined.
    fn drop(&mut self) {
        {
            let mut watch = locked(&self.watch.0);
            watch.queued.clear();
            watch.give_up = true;
        }
        drop(self.jobs.take());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// A wake that counts its rings.
    fn counting() -> (AuWake, Arc<AtomicU32>) {
        let rings = Arc::new(AtomicU32::new(0));
        let wake = AuWake::default();
        let r = rings.clone();
        wake.set(Some(Arc::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        })));
        (wake, rings)
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let t0 = Instant::now();
        while !what() {
            if t0.elapsed() > Duration::from_secs(2) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// Handles complete when the "GPU" (a shared counter) reaches them; each armed one rings
    /// once it does, in order, and a handle the owner settled first never rings.
    #[test]
    fn each_armed_completion_rings_and_a_settled_one_does_not() {
        let gpu = Arc::new(AtomicU32::new(0));
        let g = gpu.clone();
        let (wake, rings) = counting();
        let waiter = DoneWaiter::spawn(
            "pf-test-done",
            move |h: u32| {
                std::thread::sleep(Duration::from_millis(1));
                g.load(Ordering::SeqCst) >= h
            },
            wake,
        )
        .expect("a thread");
        waiter.arm(1);
        waiter.arm(2);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(rings.load(Ordering::SeqCst), 0, "nothing completed yet");
        gpu.store(1, Ordering::SeqCst);
        assert!(
            until(|| rings.load(Ordering::SeqCst) == 1),
            "the first rang"
        );
        waiter.settle(2);
        gpu.store(2, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            rings.load(Ordering::SeqCst),
            1,
            "a settled wait never rings"
        );
        waiter.arm(2);
        assert!(
            until(|| rings.load(Ordering::SeqCst) == 2),
            "a re-arm rings"
        );
    }

    /// `settle` on a handle that never completes returns within a slice, and so does `Drop`
    /// with a wait in progress: neither holds its owner on a wedged device.
    #[test]
    fn settle_and_drop_end_a_wait_that_never_completes() {
        let (wake, _rings) = counting();
        let waiter = DoneWaiter::spawn(
            "pf-test-wedged",
            |_h: u32| {
                std::thread::sleep(Duration::from_millis(5));
                false
            },
            wake,
        )
        .expect("a thread");
        waiter.arm(7);
        assert!(until(|| locked(&waiter.watch.0).busy == Some(7)));
        let t0 = Instant::now();
        waiter.settle(7);
        assert!(t0.elapsed() < Duration::from_millis(500), "settle returned");
        waiter.arm(8);
        assert!(until(|| locked(&waiter.watch.0).busy == Some(8)));
        let t0 = Instant::now();
        drop(waiter);
        assert!(t0.elapsed() < Duration::from_millis(500), "drop joined");
    }

    /// The wake a backend took before the loop handed one over rings the loop's once it has;
    /// clearing it silences every clone.
    #[test]
    fn a_wake_set_later_reaches_every_clone() {
        let held = AuWake::default();
        held.ring();
        let (wake, rings) = counting();
        let clone = held.clone();
        held.set(wake.0.lock().unwrap().clone());
        clone.ring();
        assert_eq!(rings.load(Ordering::SeqCst), 1);
        held.set(None);
        clone.ring();
        assert_eq!(rings.load(Ordering::SeqCst), 1);
    }
}
