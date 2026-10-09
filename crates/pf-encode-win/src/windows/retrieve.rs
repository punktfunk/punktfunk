//! The finished-AU queue AMF, QSV and Media Foundation share with their completion source (a
//! retrieve thread, or MF's event callback), and the signal a caller that parks on handles waits
//! on ([`crate::Encoder::ready_event`]).
//!
//! [`Ready`] is manual-reset, not auto: an access unit stays announced until the queue it
//! announces is empty, so two AUs never need two waits and a caller that probes with a zero
//! timeout gets the same answer twice. Set and clear both happen under the queue's own mutex,
//! which is what orders them against each other.

use crate::EncodedFrame;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent, WaitForSingleObject};

/// A manual-reset event this value alone closes.
pub struct Ready(HANDLE);

// SAFETY: a Win32 event handle is a process-wide token, not thread-affine. This value is its sole
// closer and hands out only borrowed copies, so the retrieve thread and the encode thread may
// both hold it.
unsafe impl Send for Ready {}
// SAFETY: as above — every operation is a `&self` call the OS serializes internally.
unsafe impl Sync for Ready {}

impl Ready {
    /// `None` if the OS refused the handle; the caller then runs without a completion signal.
    pub fn new() -> Option<Self> {
        // SAFETY: plain event creation — manual-reset, unsignalled, unnamed, no descriptor.
        let h = unsafe { CreateEventW(None, true, false, None) }.ok()?;
        Some(Self(h))
    }

    /// The raw handle for [`crate::Encoder::ready_event`]; valid while `self` lives.
    pub fn raw(&self) -> isize {
        self.0 .0 as isize
    }

    /// An access unit is waiting.
    pub fn set(&self) {
        // SAFETY: our own event, alive as long as `self`.
        unsafe {
            let _ = SetEvent(self.0);
        }
    }

    /// The queue is empty again.
    pub fn clear(&self) {
        // SAFETY: our own event, alive as long as `self`.
        unsafe {
            let _ = ResetEvent(self.0);
        }
    }

    /// Wait up to `ms` for [`Self::set`]; `true` if it was signalled. Manual-reset, so this
    /// leaves the event alone — only [`Self::clear`] takes the announcement back.
    pub fn wait(&self, ms: u32) -> bool {
        // SAFETY: our own event, alive as long as `self`.
        unsafe { WaitForSingleObject(self.0, ms) == WAIT_OBJECT_0 }
    }
}

impl Drop for Ready {
    fn drop(&mut self) {
        // SAFETY: we created this handle and hand out only borrowed copies, so this is its sole
        // close; every thread that borrowed it was joined first.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// What the completion source and the encode thread share, under one lock.
pub struct Out<P, X = ()> {
    /// One entry per submitted frame still owed an AU, in submit order: `submit` pushes the
    /// back, the source pops the front. Its length is the back-pressure reading.
    pub pending: VecDeque<P>,
    /// Finished AUs waiting for `poll`.
    pub ready: VecDeque<EncodedFrame>,
    /// The source's first failure. `poll` surfaces it so the caller resets.
    pub err: Option<String>,
    /// Backend state that must change under the same lock.
    pub extra: X,
}

/// How long `poll` waits for an AU: three quarters of a frame interval, 1 to 12 ms.
pub fn poll_budget_ms(fps: u32) -> u32 {
    (750 / fps.max(1)).clamp(1, 12)
}

/// The finished-AU queue and the [`Ready`] that announces it.
pub struct AuQueue<P, X = ()> {
    out: Mutex<Out<P, X>>,
    have: Ready,
    backend: &'static str,
    /// The first AU since the open or the last [`Self::reset`] is logged. A context line with
    /// no first-AU line after it is a silent hardware wedge.
    first_au_logged: AtomicBool,
}

impl<P, X: Default> AuQueue<P, X> {
    /// `backend` names the encoder in this queue's errors and its first-AU line.
    pub fn new(backend: &'static str) -> Result<Self> {
        let have = Ready::new().ok_or_else(|| anyhow!("{backend}: no completion event"))?;
        Ok(Self {
            out: Mutex::new(Out {
                pending: VecDeque::new(),
                ready: VecDeque::new(),
                err: None,
                extra: X::default(),
            }),
            have,
            backend,
            first_au_logged: AtomicBool::new(false),
        })
    }
}

impl<P, X> AuQueue<P, X> {
    /// Poison-tolerant: a source that panicked leaves the AUs it already handed over readable,
    /// and its `err` is what tells `poll` to reset.
    pub fn lock(&self) -> MutexGuard<'_, Out<P, X>> {
        self.out.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The raw handle for [`crate::Encoder::ready_event`]; valid while `self` lives.
    pub fn raw(&self) -> isize {
        self.have.raw()
    }

    /// Hand `au` to the encode thread. `g` is this queue's guard, so the signal cannot race the
    /// clear [`Self::pop_ready`] does when the queue empties.
    pub fn publish(&self, g: &mut Out<P, X>, au: EncodedFrame) {
        g.ready.push_back(au);
        self.have.set();
    }

    /// Record the source's failure (the first one wins) and wake the encode thread to see it.
    /// `g` is this queue's guard.
    pub fn fail(&self, g: &mut Out<P, X>, msg: impl FnOnce() -> String) {
        g.err.get_or_insert_with(msg);
        self.have.set();
    }

    /// Forfeit everything owed: a restart voids the reference chain, so the AUs behind it no
    /// longer decode against what the client holds. The restarted encoder's first AU is logged
    /// again. `extra` is the backend's to reset.
    pub fn reset(&self) {
        let mut g = self.lock();
        g.pending.clear();
        g.ready.clear();
        g.err = None;
        self.have.clear();
        self.first_au_logged.store(false, Ordering::Relaxed);
    }

    /// The oldest finished AU, or the source's failure. Clears the signal as the queue empties.
    pub fn pop_ready(&self) -> Result<Option<EncodedFrame>> {
        let mut g = self.lock();
        if let Some(e) = g.err.take() {
            bail!("{e}");
        }
        let au = g.ready.pop_front();
        if g.ready.is_empty() {
            self.have.clear();
        }
        Ok(au)
    }

    /// [`Self::pop_ready`], waiting up to `wait_ms` on the signal for the source to produce one.
    /// Logs the first AU since the open or the last [`Self::reset`].
    pub fn take_ready(&self, wait_ms: u32) -> Result<Option<EncodedFrame>> {
        let au = match self.pop_ready()? {
            None if self.have.wait(wait_ms) => self.pop_ready()?,
            au => au,
        };
        if let Some(au) = &au {
            if !self.first_au_logged.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    bytes = au.data.len(),
                    keyframe = au.keyframe,
                    "{} produced its first AU on this session",
                    self.backend
                );
            }
        }
        Ok(au)
    }

    /// Wait until `ready` holds, the source fails, or `budget` passes. The source makes the
    /// progress; this only watches for it. `what` names the stall, e.g. `"AMF output"`.
    pub fn wait_until(
        &self,
        budget: Duration,
        what: &str,
        ready: impl Fn(&Out<P, X>) -> bool,
    ) -> Result<()> {
        let deadline = Instant::now() + budget;
        loop {
            {
                let mut g = self.lock();
                if ready(&g) {
                    return Ok(());
                }
                if let Some(e) = g.err.take() {
                    bail!("{e}");
                }
                if Instant::now() >= deadline {
                    bail!(
                        "{what} stalled for {} ms with {} frame(s) in flight — wedged \
                         (escalating to reset)",
                        budget.as_millis(),
                        g.pending.len()
                    );
                }
            }
            std::thread::sleep(Duration::from_micros(250));
        }
    }
}

/// A named completion thread and its stop flag. Dropping it stops and joins, so it must drop
/// before whatever the thread calls into.
pub struct RetrieveThread {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RetrieveThread {
    /// Run `body` on a new thread; `body` returns once the flag it gets reads `true`.
    pub fn spawn(name: &str, body: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let t_stop = stop.clone();
        let join = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || body(t_stop))
            .with_context(|| format!("spawn {name}"))?;
        Ok(Self {
            stop,
            join: Some(join),
        })
    }

    /// Retire the thread and wait for it to leave the call it is blocked in. Idempotent; the
    /// queue survives, and [`AuQueue::reset`] is what empties it.
    pub fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for RetrieveThread {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au() -> EncodedFrame {
        EncodedFrame {
            data: vec![1],
            pts_ns: 0,
            keyframe: false,
            recovery_anchor: false,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: false,
        }
    }

    #[test]
    fn the_signal_holds_until_the_queue_empties() {
        let q: AuQueue<u32> = AuQueue::new("test").unwrap();
        assert!(!q.have.wait(0));
        q.publish(&mut q.lock(), au());
        q.publish(&mut q.lock(), au());
        assert!(q.take_ready(0).unwrap().is_some());
        assert!(q.have.wait(0), "one AU still queued");
        assert!(q.take_ready(0).unwrap().is_some());
        assert!(!q.have.wait(0));
        assert!(q.take_ready(0).unwrap().is_none());
    }

    #[test]
    fn a_wait_that_is_already_met_leaves_the_failure_for_poll() {
        let q: AuQueue<u32> = AuQueue::new("test").unwrap();
        q.fail(&mut q.lock(), || "boom".into());
        q.wait_until(Duration::ZERO, "test", |o| o.pending.is_empty())
            .unwrap();
        q.lock().pending.push_back(1);
        let e = q.wait_until(Duration::ZERO, "test", |o| o.pending.is_empty());
        assert_eq!(e.unwrap_err().to_string(), "boom");
        let e = q.wait_until(Duration::ZERO, "test", |o| o.pending.is_empty());
        assert!(e
            .unwrap_err()
            .to_string()
            .starts_with("test stalled for 0 ms with 1 frame(s)"));
    }

    #[test]
    fn a_reset_rearms_the_first_au_log() {
        let q: AuQueue<u32> = AuQueue::new("test").unwrap();
        let logged = || q.first_au_logged.load(Ordering::Relaxed);
        assert!(q.take_ready(0).unwrap().is_none());
        assert!(!logged(), "no AU yet");
        q.publish(&mut q.lock(), au());
        assert!(q.take_ready(0).unwrap().is_some());
        assert!(logged());
        q.reset();
        assert!(!logged());
    }

    #[test]
    fn the_poll_budget_is_three_quarters_of_a_frame_within_1_to_12_ms() {
        let budgets = [0, 30, 60, 120, 240, 1000].map(poll_budget_ms);
        assert_eq!(budgets, [12, 12, 12, 6, 3, 1]);
    }
}
