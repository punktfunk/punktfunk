//! Off-thread sampler for the virtual target's live HDR and wide-colour flags and active
//! resolution.
//!
//! Samples the display actor's cached snapshot (`pf_win_display::display_events::snapshot`,
//! immunity plan WP9) — no `QueryDisplayConfig` on this thread, so the session-global
//! display-config lock can no longer stall it; the actor coalesces the query and logs a slow one.
//! The capture loop's per-frame cost is one uncontended mutex read of the published descriptor.
//!
//! Last-known-good per field: a sample where the target is missing from the snapshot (briefly
//! absent during a topology re-probe) keeps the previous value. `seq` advances only when the
//! target was seen active, so the consumer's debounce counts observations, never misses.
//!
//! Pin: [`DescriptorPoller::snapshot`]. Consumer: `poll_display_hdr` in the IDD-push capturer.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct DisplayDescriptor {
    pub(super) hdr: bool,
    pub(super) wcg: bool,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) struct DescriptorPoller {
    snap: Arc<Mutex<(DisplayDescriptor, u64)>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DescriptorPoller {
    /// 250 ms. Two-strikes debounce in `poll_display_hdr` acts on a real flip in two samples (~½ s).
    const INTERVAL: Duration = Duration::from_millis(250);

    pub(super) fn spawn(
        ccd: pf_win_display::win_display::CcdTargetKey,
        initial: DisplayDescriptor,
    ) -> Self {
        let snap = Arc::new(Mutex::new((initial, 0u64)));
        let stop = Arc::new(AtomicBool::new(false));
        let (snap_t, stop_t) = (snap.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("pf-idd-desc-poll".into())
            .spawn(move || {
                let mut last = initial;
                let mut seq = 0u64;
                while !stop_t.load(Ordering::Relaxed) {
                    let seen = pf_win_display::display_events::snapshot()
                        .target(ccd)
                        .filter(|t| t.active)
                        .map(|t| (t.hdr, t.wcg, t.width, t.height));
                    if let Some((hdr, wcg, width, height)) = seen {
                        if let Some(hdr) = hdr {
                            last.hdr = hdr;
                        }
                        if let Some(wcg) = wcg {
                            last.wcg = wcg;
                        }
                        if width != 0 && height != 0 {
                            last.width = width;
                            last.height = height;
                        }
                        seq += 1;
                        *snap_t.lock().unwrap() = (last, seq);
                    }
                    // `park_timeout`, not `sleep`: `Drop` unparks so join does not wait out INTERVAL.
                    std::thread::park_timeout(Self::INTERVAL);
                }
            })
            .map_err(|e| {
                // Not fatal: `seq` stays 0, so the capture loop never follows a mid-session flip.
                tracing::warn!(error = %e, "IDD push: descriptor-poller thread did not spawn — mid-session HDR/mode changes won't be followed");
            })
            .ok();
        Self { snap, stop, thread }
    }

    pub(super) fn snapshot(&self) -> (DisplayDescriptor, u64) {
        *self.snap.lock().unwrap()
    }
}

impl Drop for DescriptorPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
