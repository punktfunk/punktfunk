//! Glass stamps without `VK_KHR_present_wait`: a waiter on the window's output vblank.
//!
//! A driver without `VK_KHR_present_wait` (AMD on Windows) leaves the panel unmeasured.
//! `IDXGIOutput::WaitForVBlank` returns once per refresh of the monitor under the
//! window; a FIFO present is taken to be on glass at the first vblank after it was
//! submitted with its GPU work done, one present per vblank. An estimate, labelled
//! `glass=est`: it feeds the ledger and the VRR verdict, never the latch grid, and the
//! glass gate only while the panel runs at its mode rate. Under variable refresh the
//! vblanks follow the presents; the waiter publishes their spacing, the one direct
//! reading of the panel's refresh this path has.
//!
//! Never touches the swapchain, so no drain before a swapchain teardown. It reads the
//! presenter's `done_sem` only, which outlives it: the presenter drops this waiter first.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ash::vk;
use windows::Win32::dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC};

use super::present_timing::PresentedSample;

enum Msg {
    Present(Job),
    /// The window moved to another monitor: follow it.
    Retarget(isize),
}

struct Job {
    done: Option<(vk::Semaphore, u64)>,
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
    queued: Instant,
}

/// A present whose GPU work never signals within this leaves the count without a
/// sample, like a present-wait past its cap. The gate force-opens at 100 ms anyway.
const STALE: Duration = Duration::from_millis(250);

type WakeSlot = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

pub(crate) struct VblankTimer {
    tx: Option<mpsc::Sender<Msg>>,
    /// Enqueued and not yet on glass: the glass gate's in-flight count.
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    /// Median spacing of the last [`REFRESH_SPANS`] vblanks; 0 until measured.
    refresh_ns: Arc<AtomicU64>,
    wake: WakeSlot,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

/// Vblank spacings per published median: a tenth of a second at 165 Hz.
const REFRESH_SPANS: usize = 16;

/// The DXGI output whose monitor is `monitor`, on any adapter: a hybrid box scans out on
/// one of them.
fn output_for(monitor: isize) -> Option<IDXGIOutput> {
    // SAFETY: plain factory creation; the interface is owned by this scope.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    for a in 0.. {
        // SAFETY: COM enumeration on the live factory; an error ends the walk.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else {
            break;
        };
        for o in 0.. {
            let mut output: Option<IDXGIOutput> = None;
            // SAFETY: COM enumeration on the adapter just returned, into a local out-param;
            // an error ends the walk.
            if unsafe { adapter.EnumOutputs(o, &mut output) }.is_err() {
                break;
            }
            let Some(output) = output else {
                break;
            };
            let mut desc = DXGI_OUTPUT_DESC::default();
            // SAFETY: a COM read into the local descriptor, checked before it is used.
            if unsafe { output.GetDesc(&mut desc) }.is_ok() && desc.Monitor.0 as isize == monitor {
                return Some(output);
            }
        }
    }
    None
}

fn gpu_done(device: &ash::Device, done: Option<(vk::Semaphore, u64)>) -> bool {
    let Some((sem, value)) = done else {
        return true;
    };
    // SAFETY: the presenter's timeline semaphore, alive until the presenter drops this
    // waiter, which joins the thread first.
    let reached = unsafe { device.get_semaphore_counter_value(sem) };
    reached.is_ok_and(|v| v >= value)
}

#[allow(clippy::too_many_arguments)]
fn run(
    device: ash::Device,
    monitor: isize,
    rx: mpsc::Receiver<Msg>,
    ready_tx: mpsc::Sender<bool>,
    pending: Arc<AtomicUsize>,
    results: Arc<Mutex<Vec<PresentedSample>>>,
    refresh_ns: Arc<AtomicU64>,
    wake: WakeSlot,
    stop: Arc<AtomicBool>,
) {
    // COM objects stay on this thread; the spawner only learns whether one was found.
    let mut output = output_for(monitor);
    let _ = ready_tx.send(output.is_some());
    if output.is_none() {
        return;
    }
    let mut queue: VecDeque<Job> = VecDeque::new();
    let mut last_vblank_ns = 0u64;
    let mut spans: Vec<u64> = Vec::with_capacity(REFRESH_SPANS);
    while !stop.load(Ordering::Acquire) {
        // SAFETY: a COM call on the live output; it blocks until the monitor's next vblank.
        let waited = output
            .as_ref()
            .is_some_and(|o| unsafe { o.WaitForVBlank() }.is_ok());
        if !waited {
            // Output gone (mode change, undock) until a retarget: keep the queue moving.
            output = None;
            std::thread::sleep(Duration::from_millis(16));
        }
        let vblank_ns = pf_client_core::session::now_ns();
        if !waited {
            spans.clear();
            refresh_ns.store(0, Ordering::Relaxed);
        } else if last_vblank_ns != 0 {
            spans.push(vblank_ns.saturating_sub(last_vblank_ns));
            if spans.len() == REFRESH_SPANS {
                spans.sort_unstable();
                refresh_ns.store(spans[REFRESH_SPANS / 2], Ordering::Relaxed);
                spans.clear();
            }
        }
        last_vblank_ns = if waited { vblank_ns } else { 0 };
        // Presents handed over during the wait are candidates for this vblank too.
        loop {
            match rx.try_recv() {
                Ok(Msg::Present(j)) => queue.push_back(j),
                Ok(Msg::Retarget(m)) => {
                    if let Some(o) = output_for(m) {
                        output = Some(o);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        // One present per vblank, in submission order: the first vblank after it was
        // submitted with its GPU work done. Work that finished inside the driver's flip
        // deadline reads one vblank early.
        let settled = match queue.front() {
            Some(j) if j.submitted_ns < vblank_ns && gpu_done(&device, j.done) => {
                let j = queue.pop_front().expect("front checked");
                results.lock().unwrap().push(PresentedSample {
                    pts_ns: j.pts_ns,
                    decoded_ns: j.decoded_ns,
                    submitted_ns: j.submitted_ns,
                    gpu_done_ns: 0,
                    displayed_ns: vblank_ns,
                    exact: false,
                });
                true
            }
            Some(j) if j.queued.elapsed() > STALE => {
                queue.pop_front();
                true
            }
            _ => false,
        };
        if settled {
            pending.fetch_sub(1, Ordering::AcqRel);
            if let Some(cb) = wake.lock().unwrap().as_ref() {
                cb();
            }
        }
    }
}

impl VblankTimer {
    /// `None` when no DXGI output holds the window's `monitor`: better no clock than a
    /// gate that never reopens.
    pub(crate) fn spawn(device: ash::Device, monitor: isize) -> Option<Self> {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let pending = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(Mutex::new(Vec::with_capacity(256)));
        let refresh_ns = Arc::new(AtomicU64::new(0));
        let wake: WakeSlot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (pending_t, results_t, wake_t, stop_t) =
            (pending.clone(), results.clone(), wake.clone(), stop.clone());
        let refresh_t = refresh_ns.clone();
        let join = std::thread::Builder::new()
            .name("pf-vblank".into())
            .spawn(move || {
                // The stamp is taken at wake; scheduler delay would read as latch.
                pf_client_core::audio_rt::boost_and_log("vblank");
                run(
                    device, monitor, rx, ready_tx, pending_t, results_t, refresh_t, wake_t, stop_t,
                );
            })
            .expect("spawn pf-vblank");
        let found = ready_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or(false);
        let timer = VblankTimer {
            tx: Some(tx),
            pending,
            results,
            refresh_ns,
            wake,
            stop,
            join: Some(join),
        };
        found.then_some(timer)
    }

    pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(cb);
    }

    /// The output's measured refresh spacing; 0 until a median exists.
    pub(crate) fn refresh_ns(&self) -> u64 {
        self.refresh_ns.load(Ordering::Relaxed)
    }

    /// Presents not yet on glass by the estimate, plus any that will drop out stale.
    pub(crate) fn outstanding(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub(crate) fn enqueue(
        &self,
        done: Option<(vk::Semaphore, u64)>,
        pts_ns: u64,
        decoded_ns: u64,
        submitted_ns: u64,
    ) {
        if let Some(tx) = &self.tx {
            self.pending.fetch_add(1, Ordering::AcqRel);
            let job = Job {
                done,
                pts_ns,
                decoded_ns,
                submitted_ns,
                queued: Instant::now(),
            };
            if tx.send(Msg::Present(job)).is_err() {
                self.pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Follow the window to `monitor` (a display change).
    pub(crate) fn retarget(&self, monitor: isize) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Msg::Retarget(monitor));
        }
    }

    pub(crate) fn take_samples(&self) -> Vec<PresentedSample> {
        std::mem::take(&mut *self.results.lock().unwrap())
    }
}

impl Drop for VblankTimer {
    fn drop(&mut self) {
        // The loop checks `stop` every vblank; the join waits out at most one refresh.
        self.stop.store(true, Ordering::Release);
        self.tx.take();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
