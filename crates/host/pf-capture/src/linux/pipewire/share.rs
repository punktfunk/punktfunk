//! One PipeWire stream for every session on a shared gamescope screen.
//!
//! gamescope's stream is reliable, and PipeWire hands a reliable frame to one link only: a
//! second link gets none. A session that opens a node already captured here takes its frames
//! from that stream instead ([`find`]), each published once per viewer under one hold.
//!
//! - [`Viewers`]: who takes frames, and the fan-out the loop thread runs in `.process`.
//! - [`StreamCore`]: the stream's thread and facts, alive until its last capturer drops.

use super::plan::{ImportPolicy, NegotiationPlan};
use super::queue::{FrameQueue, RenderFence};
use super::thread::PwCtl;
use crate::linux::{xfixes_cursor, CursorSlot, ImporterCell, StreamShared, StreamStatus};
use crate::{CapturedFrame, FramePayload, ZeroCopyPolicy};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Instant;

/// Closes a stream's [`Viewers`] when its loop thread ends, unwinding included.
pub(super) struct CloseOnExit(pub(super) Arc<Viewers>);

impl Drop for CloseOnExit {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A capturer's place among a stream's viewers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seat {
    /// The capturer that opened the stream: the loop thread's own queue and wake.
    Opener,
    Joined(u64),
}

struct Joined {
    id: u64,
    active: bool,
    queue: FrameQueue,
    wake: SyncSender<()>,
}

struct Inner {
    /// The opener's [`crate::Feed::set_active`]; `None` once it left.
    opener: Option<bool>,
    joined: Vec<Joined>,
    next_id: u64,
    /// The loop thread ended: nobody joins, and every joined wake is gone.
    closed: bool,
}

/// The capturers one stream feeds.
pub(super) struct Viewers {
    /// A capturer joined or the opener left. Off, `.process` publishes without this lock.
    fanned: AtomicBool,
    inner: Mutex<Inner>,
}

impl Default for Viewers {
    fn default() -> Self {
        Viewers {
            fanned: AtomicBool::new(false),
            inner: Mutex::new(Inner {
                opener: Some(false),
                joined: Vec::new(),
                next_id: 0,
                closed: false,
            }),
        }
    }
}

impl Viewers {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn fanned(&self) -> bool {
        self.fanned.load(Ordering::Relaxed)
    }

    /// A new viewer's seat, queue and wake. `None` once the loop thread ended.
    fn join(&self) -> Option<(Seat, FrameQueue, Receiver<()>)> {
        let mut inner = self.lock();
        if inner.closed {
            return None;
        }
        let (queue, (wake, woken)) = (FrameQueue::default(), sync_channel(1));
        let id = inner.next_id;
        inner.next_id += 1;
        inner.joined.push(Joined {
            id,
            active: false,
            queue: queue.clone(),
            wake,
        });
        self.fanned.store(true, Ordering::Relaxed);
        Some((Seat::Joined(id), queue, woken))
    }

    /// Record `seat`'s [`crate::Feed::set_active`]; `gate` is on while any viewer is active.
    pub(super) fn set_active(&self, seat: Seat, active: bool, gate: &AtomicBool) {
        let mut inner = self.lock();
        match seat {
            Seat::Opener => inner.opener = inner.opener.map(|_| active),
            Seat::Joined(id) => {
                if let Some(j) = inner.joined.iter_mut().find(|j| j.id == id) {
                    j.active = active;
                }
            }
        }
        gate.store(inner.any_active(), Ordering::Relaxed);
    }

    /// `seat`'s capturer is gone; `gate` stays on while a viewer left is active.
    pub(super) fn leave(&self, seat: Seat, gate: &AtomicBool) {
        let mut inner = self.lock();
        match seat {
            Seat::Opener => {
                inner.opener = None;
                self.fanned.store(true, Ordering::Relaxed);
            }
            Seat::Joined(id) => inner.joined.retain(|j| j.id != id),
        }
        gate.store(inner.any_active(), Ordering::Relaxed);
    }

    /// The loop thread ended: drop every joined wake so its viewer sees the stream gone.
    pub(super) fn close(&self) {
        let mut inner = self.lock();
        inner.closed = true;
        inner.joined.clear();
    }

    /// A wake edge with no frame behind it, to every joined viewer.
    pub(super) fn wake_joined(&self) {
        for j in &self.lock().joined {
            let _ = j.wake.try_send(());
        }
    }

    /// Queue `frame` for every active viewer: the original to one, a [`second`] to each
    /// other. Runs in `.process`, so it never blocks on a consumer.
    pub(super) fn publish(
        &self,
        opener: (&FrameQueue, &SyncSender<()>),
        frame: CapturedFrame,
        fence: Option<RenderFence>,
    ) {
        let (fence, now) = (fence.map(Arc::new), Instant::now());
        let send = |queue: &FrameQueue, wake: &SyncSender<()>, frame: CapturedFrame| {
            if let Ok(mut q) = queue.lock() {
                q.push_shared(frame, fence.clone(), now);
            }
            let _ = wake.try_send(());
        };
        let inner = self.lock();
        let to_opener = inner.opener == Some(true);
        let mut original = Some(frame);
        let mut takers = inner.joined.iter().filter(|j| j.active).peekable();
        while let Some(j) = takers.next() {
            let last = takers.peek().is_none() && !to_opener;
            let copy = if last {
                original.take()
            } else {
                original.as_ref().and_then(second)
            };
            match copy {
                Some(frame) => send(&j.queue, &j.wake, frame),
                None => crate::linux::delivery::warn_once(
                    "a frame imported on the capture thread cannot be shared — the joined \
                     viewer of this screen gets no picture",
                ),
            }
        }
        if let Some(frame) = original.filter(|_| to_opener) {
            send(opener.0, opener.1, frame);
        }
    }
}

impl Inner {
    fn any_active(&self) -> bool {
        self.opener == Some(true) || self.joined.iter().any(|j| j.active)
    }
}

/// `frame` again for another viewer: the same dmabuf under the same hold, or the CPU pixels
/// copied. `None` for a frame already imported to the GPU, which has one owner.
fn second(frame: &CapturedFrame) -> Option<CapturedFrame> {
    let payload = match &frame.payload {
        FramePayload::Cpu(pixels) => FramePayload::Cpu(pixels.clone()),
        FramePayload::Dmabuf(d) => FramePayload::Dmabuf(pf_frame::DmabufFrame {
            fd: d.fd.try_clone().ok()?,
            fourcc: d.fourcc,
            modifier: d.modifier,
            plane1: d.plane1,
            offset: d.offset,
            stride: d.stride,
            hold: d.hold.clone(),
            health: d.health.clone(),
            rebuild: d.rebuild.clone(),
        }),
        _ => return None,
    };
    Some(CapturedFrame {
        width: frame.width,
        height: frame.height,
        pts_ns: frame.pts_ns,
        format: frame.format,
        payload,
        cursor: frame.cursor.clone(),
        provenance: frame.provenance,
    })
}

/// What two opens must agree on to share one stream: the frames one negotiated are the
/// frames the other's encoder takes. The wire rate is not in it; each viewer takes the
/// newest frame at its own tick.
#[derive(Clone, PartialEq)]
pub(super) struct ShareKey {
    pub(super) plan: NegotiationPlan,
    pub(super) policy: ZeroCopyPolicy,
    /// `want_444`, `want_hdr`, `ten_bit_sdr`, `sdr10_native`, `full_range`.
    pub(super) format: [bool; 5],
}

/// One stream's thread and the facts a capturer of it reads. The thread stops when the last
/// capturer drops this.
pub(super) struct StreamCore {
    pub(super) ctl: Option<::pipewire::channel::Sender<PwCtl>>,
    pub(super) join: Mutex<Option<JoinHandle<()>>>,
    pub(super) viewers: Arc<Viewers>,
    pub(super) shared: StreamShared,
    pub(super) cursor_slot: CursorSlot,
    pub(super) importer: ImporterCell,
    pub(super) vaapi_dmabuf: bool,
    pub(super) hdr_offer: bool,
    pub(super) import_policy: ImportPolicy,
    pub(super) raw_for_encoder: bool,
    /// `None` never shares.
    pub(super) key: Option<ShareKey>,
    /// Gamescope XFixes reader, which every viewer's pointer comes from.
    pub(super) gs_cursor: Mutex<Option<xfixes_cursor::XFixesCursorSource>>,
}

impl StreamCore {
    /// The loop thread has not exited.
    pub(super) fn running(&self) -> bool {
        let join = self.join.lock().unwrap_or_else(|e| e.into_inner());
        join.as_ref().is_some_and(|j| !j.is_finished())
    }

    /// Take a viewer's seat on this stream. `None` once its thread ended.
    pub(super) fn join(&self) -> Option<(Seat, FrameQueue, Receiver<()>)> {
        self.viewers.join()
    }
}

impl Drop for StreamCore {
    /// Quit the loop thread and join it: the stream disconnects and EGL/CUDA are released
    /// before the caller goes on. `send` errs only if the thread already exited.
    fn drop(&mut self) {
        if let Some(ctl) = self.ctl.take() {
            let _ = ctl.send(PwCtl::Quit);
        }
        let join = self.join.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Some(join) = join.take() {
            let _ = join.join();
        }
    }
}

/// The streams that may be shared, by node. Weak: a stream lives by its capturers alone.
static LIVE: Mutex<Vec<(u32, Weak<StreamCore>)>> = Mutex::new(Vec::new());

pub(super) fn register(node_id: u32, core: &Arc<StreamCore>) {
    let mut live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    live.retain(|(_, c)| c.strong_count() > 0);
    live.push((node_id, Arc::downgrade(core)));
}

/// The stream on `node_id` whose frames an open with `key` can take. One still negotiating
/// counts; one that broke or errored never does: its capturers are about to rebuild it.
pub(super) fn find(node_id: u32, key: &ShareKey) -> Option<Arc<StreamCore>> {
    let live = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let on_node = live.iter().filter(|(n, _)| *n == node_id);
    let dead = |s| matches!(s, StreamStatus::Broken | StreamStatus::Errored);
    let mut healthy = (on_node.filter_map(|(_, c)| c.upgrade()))
        .filter(|c| c.running() && !dead(c.shared.status()));
    let core = healthy.next()?;
    if core.key.as_ref() != Some(key) {
        tracing::warn!(
            node_id,
            "this screen is already captured in a format this session's encoder does not take \
             — opening a second stream, which a reliable producer leaves without frames"
        );
        return None;
    }
    Some(core)
}

#[cfg(test)]
mod tests {
    use super::{Seat, Viewers};
    use crate::linux::pipewire::queue::FrameQueue;
    use crate::{CapturedFrame, FramePayload, PixelFormat};
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::sync::mpsc::sync_channel;
    use std::time::Instant;

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

    fn taken(q: &FrameQueue) -> Option<u64> {
        let mut q = q.lock().unwrap();
        q.take_ready(Instant::now()).map(|t| t.frame.pts_ns)
    }

    #[test]
    fn every_active_viewer_gets_each_frame_and_an_idle_one_none() {
        let v = Viewers::default();
        let opener = FrameQueue::default();
        let (wake, _woken) = sync_channel(1);
        assert!(!v.fanned(), "a lone opener publishes on the old path");
        let (a, qa, _wa) = v.join().unwrap();
        let (_b, qb, _wb) = v.join().unwrap();
        assert!(v.fanned());
        let gate = AtomicBool::new(false);
        v.set_active(Seat::Opener, true, &gate);
        v.set_active(a, true, &gate);
        v.publish((&opener, &wake), frame(1), None);
        assert_eq!(taken(&opener), Some(1));
        assert_eq!(taken(&qa), Some(1));
        assert_eq!(taken(&qb), None, "b never went active");
    }

    #[test]
    fn a_joined_viewer_keeps_the_frames_after_the_opener_leaves() {
        let v = Viewers::default();
        let opener = FrameQueue::default();
        let (wake, _woken) = sync_channel(1);
        let (a, qa, _wa) = v.join().unwrap();
        let gate = AtomicBool::new(false);
        v.set_active(Seat::Opener, true, &gate);
        v.set_active(a, true, &gate);
        v.leave(Seat::Opener, &gate);
        assert!(gate.load(Relaxed), "a is still active");
        v.publish((&opener, &wake), frame(2), None);
        assert_eq!(taken(&qa), Some(2));
        assert_eq!(
            taken(&opener),
            None,
            "nothing piles up behind a gone opener"
        );
        v.leave(a, &gate);
        assert!(!gate.load(Relaxed));
    }

    #[test]
    fn a_closed_stream_takes_no_viewer_and_drops_their_wakes() {
        let v = Viewers::default();
        let (_a, _qa, woken) = v.join().unwrap();
        v.close();
        assert!(
            woken.recv().is_err(),
            "the joined wake went with the thread"
        );
        assert!(v.join().is_none());
    }
}
