//! Arrivals in order, each with the fence its pixels wait on.
//!
//! The loop thread appends and never waits, so it stays free to hand buffers back. The
//! consumer waits on the oldest render and takes the newest frame whose render finished. A
//! producer renders behind whatever else fills the GPU queue, so several arrivals can be out
//! at once with none of them readable yet; a one-deep slot would lose all but the last.

use crate::linux::sync_timeline::SyncDevice;
use crate::CapturedFrame;
use pf_dmabuf::fence::WaitOutcome;
use std::collections::VecDeque;
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Past this a render is wedged, not slow, and the frame is taken as it is.
pub(in crate::linux) const RENDER_GUARD: Duration = Duration::from_millis(100);

/// Backstop on the queue's length. Holds bound it first: a pool lends no more than it has.
const MAX_PENDING: usize = 8;

/// What fences a frame's pixels. Owned, so the wait can run after `.process` returned.
pub(in crate::linux) enum RenderFence {
    /// Implicit sync: the dmabuf's pending writes at arrival.
    SyncFile(OwnedFd),
    /// Explicit sync: the producer's acquire point on the buffer's timeline.
    Acquire {
        dev: Arc<SyncDevice>,
        timeline: OwnedFd,
        point: u64,
    },
}

impl RenderFence {
    /// Block until the render finished or `timeout` passed.
    pub(super) fn wait(&self, timeout: Duration) -> std::io::Result<WaitOutcome> {
        match self {
            RenderFence::SyncFile(fd) => {
                let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
                pf_dmabuf::fence::wait_sync_file(fd.as_fd(), ms)
            }
            RenderFence::Acquire {
                dev,
                timeline,
                point,
            } => dev.wait(timeline.as_raw_fd(), *point, timeout),
        }
    }

    /// A failed wait reads as done: an unreadable fence must not hold the stream.
    fn done(&self) -> bool {
        !matches!(self.wait(Duration::ZERO), Ok(WaitOutcome::TimedOut))
    }
}

struct Pending {
    frame: CapturedFrame,
    fence: Option<Arc<RenderFence>>,
    arrived: Instant,
}

impl Pending {
    fn ready(&self, now: Instant) -> bool {
        match &self.fence {
            None => true,
            Some(f) => now.duration_since(self.arrived) >= RENDER_GUARD || f.done(),
        }
    }
}

/// How one taken frame's render went, for the `PUNKTFUNK_PERF` line.
pub(in crate::linux) struct Taken {
    pub(in crate::linux) frame: CapturedFrame,
    /// Arrival to take: the render, then the consumer's own delay.
    pub(in crate::linux) waited: Duration,
    pub(in crate::linux) outcome: WaitOutcome,
    /// Older frames this take let go.
    pub(in crate::linux) passed: usize,
}

/// What the consumer has to wait for.
enum Wait {
    /// Nothing queued: the next arrival.
    Arrival,
    Ready,
    /// The oldest render, no longer than until its guard.
    Render(Arc<RenderFence>, Instant),
}

#[derive(Default)]
pub(in crate::linux) struct Arrivals {
    q: VecDeque<Pending>,
}

pub(in crate::linux) type FrameQueue = Arc<Mutex<Arrivals>>;

impl Arrivals {
    /// Append an arrival, after letting go of every finished frame but the newest: a slow
    /// consumer takes that one alone, and the producer needs the buffers of the rest to
    /// send anything newer. Frames still rendering stay; one of them is the next to finish.
    pub(super) fn push(&mut self, frame: CapturedFrame, fence: Option<RenderFence>, now: Instant) {
        self.supersede(now);
        while self.q.len() >= MAX_PENDING {
            self.q.pop_front();
        }
        self.q.push_back(Pending {
            frame,
            fence: fence.map(Arc::new),
            arrived: now,
        });
    }

    /// Drop every frame older than the newest finished one. Returns how many went.
    fn supersede(&mut self, now: Instant) -> usize {
        let newest = self.q.iter().rposition(|p| p.ready(now)).unwrap_or(0);
        self.q.drain(..newest).count()
    }

    /// The newest frame whose render finished. It and every older frame leave the queue.
    pub(in crate::linux) fn take_ready(&mut self, now: Instant) -> Option<Taken> {
        let passed = self.supersede(now);
        // The newest finished frame is the front now, if any has finished.
        if !self.q.front()?.ready(now) {
            return None;
        }
        let p = self.q.pop_front()?;
        let waited = now.duration_since(p.arrived);
        Some(Taken {
            outcome: match &p.fence {
                None => WaitOutcome::NoFence,
                Some(_) if waited >= RENDER_GUARD => WaitOutcome::TimedOut,
                Some(_) => WaitOutcome::Signaled,
            },
            frame: p.frame,
            waited,
            passed,
        })
    }

    pub(in crate::linux) fn clear(&mut self) {
        self.q.clear();
    }

    fn wait(&self, now: Instant) -> Wait {
        if self.q.iter().any(|p| p.ready(now)) {
            return Wait::Ready;
        }
        match self.q.front() {
            Some(Pending {
                fence: Some(f),
                arrived,
                ..
            }) => Wait::Render(f.clone(), *arrived + RENDER_GUARD),
            _ => Wait::Arrival,
        }
    }
}

/// Block until [`Arrivals::take_ready`] has a frame, `deadline` passes, or the loop thread
/// is gone: `false` for the last. Never consumes.
pub(in crate::linux) fn wait_ready(
    queue: &FrameQueue,
    wake: &Receiver<()>,
    deadline: Instant,
) -> bool {
    use std::sync::mpsc::RecvTimeoutError;
    loop {
        let now = Instant::now();
        let Ok(wait) = queue.lock().map(|q| q.wait(now)) else {
            return true;
        };
        match wait {
            Wait::Ready => return true,
            Wait::Arrival => {
                let Some(left) = deadline.checked_duration_since(now) else {
                    return true;
                };
                match wake.recv_timeout(left) {
                    Ok(()) => {}
                    Err(RecvTimeoutError::Timeout) => return true,
                    Err(RecvTimeoutError::Disconnected) => return false,
                }
            }
            Wait::Render(fence, guard) => {
                let _ = fence.wait(deadline.min(guard).saturating_duration_since(now));
                if Instant::now() >= deadline {
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Arrivals, RenderFence, RENDER_GUARD};
    use crate::{CapturedFrame, FramePayload, PixelFormat};
    use pf_dmabuf::fence::WaitOutcome;
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::time::{Duration, Instant};

    fn frame(n: u64) -> CapturedFrame {
        CapturedFrame {
            provenance: Default::default(),
            width: 1,
            height: 1,
            pts_ns: n,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0; 4]),
            cursor: None,
        }
    }

    /// A pipe's read end is a fence that signals when the write end is written to.
    fn fence() -> (RenderFence, OwnedFd) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is the two-int array `pipe` fills.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both ends were just created and nothing else owns them.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        (RenderFence::SyncFile(r), w)
    }

    fn signal(w: &OwnedFd) {
        // SAFETY: one byte from a live array into an open pipe.
        let written = unsafe { libc::write(w.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        assert_eq!(written, 1);
    }

    fn frames(q: &Arrivals) -> Vec<u64> {
        q.q.iter().map(|p| p.frame.pts_ns).collect()
    }

    #[test]
    fn a_frame_still_rendering_is_not_taken() {
        let mut q = Arrivals::default();
        let now = Instant::now();
        let (f, w) = fence();
        q.push(frame(1), Some(f), now);
        assert!(q.take_ready(now).is_none());
        assert_eq!(frames(&q), [1], "and it stays queued");
        signal(&w);
        let t = q.take_ready(now).unwrap();
        assert_eq!(t.frame.pts_ns, 1);
        assert_eq!(t.outcome, WaitOutcome::Signaled);
    }

    /// Two renders done, a third not: the second is taken, the first let go, the third kept.
    #[test]
    fn the_newest_finished_render_wins() {
        let mut q = Arrivals::default();
        let now = Instant::now();
        let (f1, w1) = fence();
        let (f2, w2) = fence();
        let (f3, w3) = fence();
        q.push(frame(1), Some(f1), now);
        q.push(frame(2), Some(f2), now);
        q.push(frame(3), Some(f3), now);
        signal(&w1);
        signal(&w2);
        let t = q.take_ready(now).unwrap();
        assert_eq!((t.frame.pts_ns, t.passed), (2, 1));
        assert!(q.take_ready(now).is_none(), "the third still renders");
        signal(&w3);
        assert_eq!(q.take_ready(now).unwrap().frame.pts_ns, 3);
    }

    /// A consumer slower than the producer: the queue gives back what it will never take,
    /// or the producer has no buffer left to send the frame the consumer does want.
    #[test]
    fn an_arrival_lets_go_of_the_finished_frames_before_the_newest() {
        let mut q = Arrivals::default();
        let now = Instant::now();
        for n in 1..=3 {
            let (f, w) = fence();
            signal(&w);
            q.push(frame(n), Some(f), now);
        }
        assert_eq!(frames(&q), [2, 3], "the third arrival let the first go");
        let (f4, _w4) = fence();
        q.push(frame(4), Some(f4), now);
        assert_eq!(frames(&q), [3, 4], "renders still out stay");
        let t = q.take_ready(now).unwrap();
        assert_eq!((t.frame.pts_ns, t.passed), (3, 0));
    }

    /// Renders behind a full GPU queue: none finished yet, and every one is kept.
    #[test]
    fn frames_still_rendering_pile_up() {
        let mut q = Arrivals::default();
        let now = Instant::now();
        let mut writers = Vec::new();
        for n in 1..=3 {
            let (f, w) = fence();
            writers.push(w);
            q.push(frame(n), Some(f), now);
        }
        assert_eq!(frames(&q), [1, 2, 3]);
        signal(&writers[0]);
        assert_eq!(q.take_ready(now).unwrap().frame.pts_ns, 1);
        assert_eq!(frames(&q), [2, 3]);
    }

    #[test]
    fn a_frame_with_no_fence_is_ready_at_once() {
        let mut q = Arrivals::default();
        let now = Instant::now();
        let (f, _w) = fence();
        q.push(frame(1), Some(f), now);
        q.push(frame(2), None, now);
        let t = q.take_ready(now).unwrap();
        assert_eq!((t.frame.pts_ns, t.passed), (2, 1));
        assert_eq!(t.outcome, WaitOutcome::NoFence);
    }

    #[test]
    fn a_render_past_its_guard_is_taken_anyway() {
        let mut q = Arrivals::default();
        let now = Instant::now();
        let (f, _w) = fence();
        q.push(frame(1), Some(f), now);
        assert!(q.take_ready(now + RENDER_GUARD / 2).is_none());
        let t = q.take_ready(now + RENDER_GUARD).unwrap();
        assert_eq!(t.outcome, WaitOutcome::TimedOut);
    }

    #[test]
    fn the_wait_returns_when_the_oldest_render_finishes() {
        let queue = super::FrameQueue::default();
        let (_tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        let (f, w) = fence();
        queue
            .lock()
            .unwrap()
            .push(frame(1), Some(f), Instant::now());
        let t0 = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(20));
                signal(&w);
            });
            assert!(super::wait_ready(&queue, &rx, t0 + Duration::from_secs(2)));
        });
        assert!(t0.elapsed() < Duration::from_secs(1), "woke on the fence");
        assert!(queue.lock().unwrap().take_ready(Instant::now()).is_some());
    }

    #[test]
    fn the_wait_says_when_the_loop_thread_is_gone() {
        let queue = super::FrameQueue::default();
        let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
        drop(tx);
        assert!(!super::wait_ready(
            &queue,
            &rx,
            Instant::now() + Duration::from_secs(2)
        ));
    }
}
