//! A pool buffer's life on this side, from `add_buffer` to `remove_buffer`: one state per
//! buffer, and one [`step`] over what the loop thread and the encoder do to it.
//!
//! A published buffer stays out of the producer's pool until encode lets go of it. The whole
//! pool may be out: the producer then has nothing to paint and skips. The encode thread only
//! parks a release and wakes the loop; only the loop thread hands a buffer back.

use super::UserData;
use crate::linux::sync_timeline::{
    undelivered, StreamBuffers, SyncDevice, SyncPoints, Syncobj, Undelivered,
};
use pipewire as pw;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// Where one pool buffer is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum BufferState {
    /// With the producer.
    #[default]
    Free,
    /// In this `.process`, neither held nor handed back yet.
    Dequeued,
    /// Withheld from the producer under the hold of `generation`.
    Held { generation: u64 },
    /// Its hold dropped, the stream's `nth` drop; withheld until the loop requeues it.
    Released { generation: u64, nth: u64 },
    /// Freed under us by `remove_buffer`.
    Purged,
}

/// What happens to a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    /// This `.process` takes the buffer, to publish it or to hand it back.
    Dequeued,
    /// A hold of this generation goes out with the buffer's frame.
    Published(u64),
    /// The hold of `generation` dropped, on any thread: the stream's `nth` drop.
    HoldDropped { generation: u64, nth: u64 },
    /// The loop requeues what was released.
    Drained,
    /// This `.process` is done with the buffer.
    Done,
    /// `remove_buffer`.
    Removed,
}

/// What the loop thread does with the buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Nothing,
    /// Hand it back to the producer.
    Requeue,
    /// The producer sent a held buffer again (PipeWire < 1.6). The stale hold is gone, so its
    /// release cannot requeue the buffer this `.process` publishes; one IDR covers the frame
    /// encoded from the earlier read.
    Resent,
}

/// One buffer's next state. A hold releases only under its own generation, so a stale hold
/// never requeues a re-held buffer or the next pool's tenant of its address.
fn step(state: BufferState, event: Event) -> (BufferState, Action) {
    use BufferState::*;
    match (state, event) {
        (Purged, _) | (_, Event::Removed) => (Purged, Action::Nothing),
        (Held { .. } | Released { .. }, Event::Dequeued) => (Dequeued, Action::Resent),
        (_, Event::Dequeued) => (Dequeued, Action::Nothing),
        (_, Event::Published(generation)) => (Held { generation }, Action::Nothing),
        (
            Held { generation },
            Event::HoldDropped {
                generation: dropped,
                nth,
            },
        ) if dropped == generation => (Released { generation, nth }, Action::Nothing),
        (Released { .. }, Event::Drained) => (Free, Action::Requeue),
        (Free | Dequeued, Event::Done) => (Free, Action::Requeue),
        (state, _) => (state, Action::Nothing),
    }
}

/// One pool buffer: where it is, and its explicit-sync timelines.
#[derive(Default)]
struct Entry {
    state: BufferState,
    /// Imported on first use, destroyed with the entry at `remove_buffer`.
    timelines: Option<Timelines>,
    /// The release point last signalled: a point is signalled once.
    released: Option<u64>,
    /// An unsignalled release point while this side does not hold the buffer, and when it
    /// was first seen there.
    waiting: Option<(u64, Instant)>,
}

/// A buffer's two timelines, by the fds its sync datas carry.
#[derive(Clone)]
pub(super) struct Timelines {
    fds: (RawFd, RawFd),
    /// Fences the producer's render.
    pub(super) acquire: Arc<Syncobj>,
    /// Signalled here on hand-back.
    release: Arc<Syncobj>,
}

/// Every buffer of the pool, in the order the producer announced them.
#[derive(Default)]
struct Book {
    bufs: Vec<(usize, Entry)>,
    /// Last issued hold generation, monotonic per stream.
    last_gen: u64,
    /// Holds dropped so far: the drain requeues in drop order.
    drops: u64,
}

impl Book {
    /// Step `buf` through `event`. A buffer the book does not list is `Free` to the loop's own
    /// events on a buffer in its hands, and `Purged` to the rest.
    fn apply(&mut self, buf: usize, event: Event) -> Action {
        let at = self.bufs.iter().position(|(b, _)| *b == buf);
        let state = match at {
            Some(i) => self.bufs[i].1.state,
            None if matches!(event, Event::Dequeued | Event::Published(_) | Event::Done) => {
                BufferState::Free
            }
            None => BufferState::Purged,
        };
        let (next, action) = step(state, event);
        match (at, next) {
            (Some(i), BufferState::Purged) => {
                self.bufs.remove(i);
            }
            (Some(i), next) => self.bufs[i].1.state = next,
            (None, BufferState::Purged) => {}
            (None, state) => self.bufs.push((
                buf,
                Entry {
                    state,
                    ..Entry::default()
                },
            )),
        }
        action
    }

    /// `add_buffer`.
    fn add(&mut self, buf: usize) {
        self.bufs.retain(|(b, _)| *b != buf);
        self.bufs.push((buf, Entry::default()));
    }

    /// `remove_buffer`: a later release of its hold is a no-op.
    fn remove(&mut self, buf: usize) {
        self.apply(buf, Event::Removed);
    }

    /// `buf` arrives in `.process`. `true`: the producer re-sent it while held.
    fn arrive(&mut self, buf: usize) -> bool {
        self.apply(buf, Event::Dequeued) == Action::Resent
    }

    /// Withhold `buf` under a new generation, unless the pool is too shallow to lend
    /// ([`SHALLOW_POOL`]) or every other buffer is out.
    fn hold(&mut self, buf: usize, pool_live: u32) -> Option<u64> {
        let cap = if pool_live > SHALLOW_POOL {
            pool_live as usize
        } else {
            0
        };
        let out = self
            .bufs
            .iter()
            .filter(|(b, e)| *b != buf && withheld(e.state));
        if out.count() >= cap {
            return None;
        }
        self.last_gen += 1;
        self.apply(buf, Event::Published(self.last_gen));
        Some(self.last_gen)
    }

    /// The hold of `generation` on `buf` dropped.
    fn drop_hold(&mut self, buf: usize, generation: u64) {
        self.drops += 1;
        let nth = self.drops;
        self.apply(buf, Event::HoldDropped { generation, nth });
    }

    /// Every released buffer, in drop order, each now `Free`: the caller requeues them.
    fn drain(&mut self) -> Vec<usize> {
        let mut released: Vec<(u64, usize)> = self
            .bufs
            .iter()
            .filter_map(|(b, e)| match e.state {
                BufferState::Released { nth, .. } => Some((nth, *b)),
                _ => None,
            })
            .collect();
        released.sort_unstable();
        for &(_, buf) in &released {
            self.apply(buf, Event::Drained);
        }
        released.into_iter().map(|(_, buf)| buf).collect()
    }

    /// `.process` is done with `buf`. `true`: hand it back now; `false`: a hold owns it.
    fn done(&mut self, buf: usize) -> bool {
        self.apply(buf, Event::Done) == Action::Requeue
    }

    /// Buffers withheld from the producer now.
    fn withheld_count(&self) -> u32 {
        self.bufs.iter().filter(|(_, e)| withheld(e.state)).count() as u32
    }

    fn entry(&mut self, buf: usize) -> Option<&mut Entry> {
        self.bufs
            .iter_mut()
            .find(|(b, _)| *b == buf)
            .map(|(_, e)| e)
    }
}

/// Withheld from the producer: a hold is out, or dropped and not yet requeued.
fn withheld(state: BufferState) -> bool {
    matches!(
        state,
        BufferState::Held { .. } | BufferState::Released { .. }
    )
}

impl UserData {
    /// Withhold this buffer from the producer until the returned hold drops.
    /// `None` requeues at `.process` return — the producer may then rewrite the dmabuf while
    /// encode still reads it, so no lane publishes a raw frame under `None`. The cap is the pool
    /// census: a shortage means the census counts fewer buffers than the pool has, and drops
    /// the arrival. A pool that can never hold (`holds_possible` false) takes the lane's own
    /// fallback.
    /// No queued frame is given up for an arrival: the oldest render out is the next to
    /// finish, and a producer behind a full GPU queue would never see one through.
    /// A buffer the ledger still withholds was re-sent by the producer: it is re-held under
    /// a new generation, and `resent` asks for the covering IDR. Every arrival raises the
    /// pool gauge's withheld peak to the count after its hold.
    pub(super) fn try_defer(
        &mut self,
        pw_buf: *mut pw::sys::pw_buffer,
        stream: &dyn StreamBuffers,
    ) -> Option<pf_frame::FrameHold> {
        if !zerocopy_hold_enabled() {
            return None;
        }
        let buf = pw_buf as usize;
        // PipeWire below 1.6 (no `node.reliable`) lets the producer reclaim a buffer before this
        // side marked it busy, then send it again. Its frame is fresh; the earlier hold's read may
        // be torn. The arrival ends the stale hold before the drain below, so its release cannot
        // hand back the buffer this callback publishes; the pool stays whole, and the loop
        // answers with one IDR.
        let resent = self.ledger.book().arrive(buf);
        if resent && self.shared.resent.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::warn!(
                "producer re-sent a buffer this capture still holds — re-holding it; one IDR \
                 covers the frame encoded from the earlier read (PipeWire < 1.6)"
            );
        }
        // A hold the encoder dropped during this `.process` (the fence wait above is where it
        // lands) still counts against the pool until the loop services the wake — after this
        // callback. Requeue it now, so the arrival spends a budget that is current.
        // SAFETY: `stream` is the stream whose `.process` is running on this loop thread.
        unsafe { self.ledger.drain(stream) };
        let pool_live = self.pool.live;
        let (held, withheld) = {
            let mut book = self.ledger.book();
            (book.hold(buf, pool_live), book.withheld_count())
        };
        self.shared
            .pool
            .withheld_peak
            .fetch_max(withheld, Ordering::Relaxed);
        let Some(generation) = held else {
            if !self.ledger.logged_shallow.swap(true, Ordering::Relaxed) {
                tracing::warn!(
                    pool_depth = pool_live,
                    holds_possible = holds_possible(true, pool_live),
                    "zero-copy: the producer's buffer pool cannot lend a buffer across the \
                     encode — while holds are possible at all this arrival is dropped \
                     (held_drops= on the provenance line); a pool that can never hold takes \
                     the CPU copy instead"
                );
            }
            return None;
        };
        if !self.ledger.logged_active.swap(true, Ordering::Relaxed) {
            tracing::info!(
                pool_depth = pool_live,
                "zero-copy: withholding each published buffer from the producer until the \
                 encoder releases it (deferred requeue — the producer can no longer rewrite a \
                 frame mid-encode); PUNKTFUNK_ZEROCOPY_HOLD=0 restores the immediate requeue"
            );
        }
        Some(Arc::new(BufferHold {
            ledger: self.ledger.clone(),
            buf,
            generation,
        }))
    }

    /// Requeue a buffer this `.process` publishes nothing from. One the ledger still withholds
    /// was re-sent while held (PipeWire < 1.6): the arrival ends its stale hold, so that
    /// release no-ops and the buffer rejoins once, and `resent` asks for the covering IDR.
    ///
    /// # Safety
    /// Loop thread; `buf` was dequeued from the live `stream` and is not yet requeued.
    pub(super) unsafe fn requeue_unpublished(
        &self,
        stream: &dyn StreamBuffers,
        buf: *mut pw::sys::pw_buffer,
    ) {
        if self.ledger.book().arrive(buf as usize) {
            self.shared.resent.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract is `done`'s; the arrival above leaves no hold that
        // would queue `buf` a second time.
        unsafe { self.ledger.done(stream, buf) };
    }
}

/// How many buffers the producer allocated for this stream.
///
/// `live` comes from `add_buffer`/`remove_buffer` on the loop thread. There is no "pool
/// complete" event, so the count is published from `.process` (first dequeue ⇒ allocation
/// finished). Depth is the budget a hold spends; a pool of ≤ [`SHALLOW_POOL`] cannot defer
/// and the producer may rewrite a buffer mid-encode.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct PoolCensus {
    pub(super) live: u32,
    /// Deepest `live` this session. A depth decision must not follow a renegotiation down to zero.
    pub(super) high_water: u32,
    /// Last logged `live`, so a stable pool logs once per distinct depth.
    logged: Option<u32>,
}

impl PoolCensus {
    pub(super) fn add(&mut self) {
        self.live += 1;
        self.high_water = self.high_water.max(self.live);
    }

    pub(super) fn remove(&mut self) {
        self.live = self.live.saturating_sub(1);
    }

    /// `Some(live)` the first time each distinct depth is seen.
    pub(super) fn note_frame(&mut self) -> Option<u32> {
        (self.logged != Some(self.live)).then(|| {
            self.logged = Some(self.live);
            self.live
        })
    }
}

/// Holds exist for this pool: its buffers may wait in the queue or under the consumer's
/// import. False means the arrival path imports itself.
pub(super) fn holds_possible(hold_enabled: bool, pool_live: u32) -> bool {
    hold_enabled && pool_live > SHALLOW_POOL
}

/// A pool this shallow lends nothing: the host's frame and one render out would be all of it,
/// and the producer could paint only once encode let go.
///
/// A deeper pool lends every buffer. One is the host's frame; a producer behind a full GPU
/// queue has two or three renders out at any frame rate, and a pool of 4 holds no more. With
/// the whole pool out the producer skips a frame, which costs what dropping one here would.
pub(super) const SHALLOW_POOL: u32 = 2;

/// Pool the raw lane asks for: four encoder holds, the host frame, the capture slot, and two
/// buffers reserved for the producer. A producer capped below this spends only the holds it
/// serves; [`UserData::release_unconsumed`] frees the slot's for the next arrival.
const RAW_LANE_POOL_MIN: i32 = 8;

/// Least pool depth this stream asks for: the producer's minimum, deepened to
/// [`RAW_LANE_POOL_MIN`] on the raw lane but never past `pool_max`. A minimum above what the
/// producer serves fails negotiation outright.
pub(super) fn pool_ask(pool_min: i32, pool_max: Option<i32>, raw_lane: bool) -> i32 {
    if !raw_lane {
        return pool_min;
    }
    let deep = pool_min.max(RAW_LANE_POOL_MIN);
    pool_max.map_or(deep, |max| deep.min(max).max(pool_min))
}

/// [`Knobs::zerocopy_hold`](crate::knobs::Knobs::zerocopy_hold): off, `try_defer` returns
/// `None` and the raw passthrough takes the CPU fallback.
pub(super) fn zerocopy_hold_enabled() -> bool {
    crate::knobs::Knobs::process().zerocopy_hold
}

/// One stream's pool. Shared by the loop thread, which steps every buffer, and the
/// [`BufferHold`] guards on the encode thread, which only park a release and wake the loop.
pub(super) struct PoolLedger {
    book: Mutex<Book>,
    /// Wakes the loop to requeue what was released. Send failure = the loop is gone.
    wake: pw::channel::Sender<()>,
    logged_active: AtomicBool,
    logged_shallow: AtomicBool,
    /// Signals a buffer's release point as it rejoins; `None` without explicit sync.
    sync: Option<Arc<SyncDevice>>,
    /// Buffers [`PoolLedger::recover_undelivered`] gave back to the producer.
    pub(super) undelivered: AtomicU64,
}

impl PoolLedger {
    pub(super) fn new(wake: pw::channel::Sender<()>, sync: Option<Arc<SyncDevice>>) -> Self {
        PoolLedger {
            book: Mutex::default(),
            wake,
            logged_active: AtomicBool::new(false),
            logged_shallow: AtomicBool::new(false),
            sync,
            undelivered: AtomicU64::new(0),
        }
    }

    /// Nothing panics under this lock, so a poisoned book is still whole.
    fn book(&self) -> MutexGuard<'_, Book> {
        self.book.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `add_buffer`.
    pub(super) fn add(&self, buf: usize) {
        self.book().add(buf);
    }

    /// `remove_buffer`: the buffer is freed under any hold, and that hold's later release is
    /// a no-op. Its timelines go with it, destroyed outside the book's lock.
    pub(super) fn remove(&self, buf: usize) {
        let timelines = {
            let mut book = self.book();
            let timelines = book.entry(buf).and_then(|e| e.timelines.take());
            book.remove(buf);
            timelines
        };
        drop(timelines);
    }

    /// `buf`'s timelines, imported on its first use. Loop thread: the only importer, so the
    /// entry gains none between the look and the store. Errors without explicit sync.
    pub(super) fn timelines(&self, buf: usize, p: &SyncPoints) -> std::io::Result<Timelines> {
        let Some(dev) = &self.sync else {
            return Err(std::io::ErrorKind::Unsupported.into());
        };
        let fds = (p.acquire_fd, p.release_fd);
        let cached = self.book().entry(buf).and_then(|e| e.timelines.clone());
        if let Some(t) = cached.filter(|t| t.fds == fds) {
            return Ok(t);
        }
        let t = Timelines {
            fds,
            acquire: Syncobj::import(dev, p.acquire_fd)?,
            release: Syncobj::import(dev, p.release_fd)?,
        };
        let replaced = self
            .book()
            .entry(buf)
            .and_then(|e| e.timelines.replace(t.clone()));
        drop(replaced);
        Ok(t)
    }

    /// Requeue every buffer whose hold dropped since the last drain. Returns how many rejoined.
    ///
    /// # Safety
    /// Loop thread only, and `stream` is the live stream whose buffers this ledger tracks.
    pub(super) unsafe fn drain(&self, stream: &dyn StreamBuffers) -> usize {
        let released = self.book().drain();
        for &buf in &released {
            // SAFETY: the book releases only a buffer it still withheld under the dropping
            // hold's generation, and `remove_buffer` takes a buffer out of the book: `buf` is
            // a live buffer of `stream` this side dequeued and never requeued.
            unsafe { self.hand_back(stream, buf as *mut pw::sys::pw_buffer) };
        }
        released.len()
    }

    /// `.process` is done with `buf`: hand it back unless a hold owns its requeue.
    ///
    /// # Safety
    /// Loop thread; `buf` was dequeued from the live `stream` and is not yet requeued. The
    /// caller touches it no further.
    pub(super) unsafe fn done(&self, stream: &dyn StreamBuffers, buf: *mut pw::sys::pw_buffer) {
        if self.book().done(buf as usize) {
            // SAFETY: the caller's contract; the book saw no hold that would requeue `buf`.
            unsafe { self.hand_back(stream, buf) };
        }
    }

    /// Hand `buf` back to the producer, its release point signalled first when the stream
    /// negotiated explicit sync: the producer waits on that point before it paints the buffer
    /// again, so a point never signalled parks one pool slot for good.
    ///
    /// # Safety
    /// Loop thread; `buf` is a live buffer of `stream` this side holds and touches no further.
    unsafe fn hand_back(&self, stream: &dyn StreamBuffers, buf: *mut pw::sys::pw_buffer) {
        // SAFETY: `buf` is held (caller), so its `spa_buffer` is readable until the queue below.
        let points = unsafe { SyncPoints::of((*buf).buffer) };
        if let (Some(_), Some(p)) = (&self.sync, points) {
            match self.release(buf as usize, &p) {
                // SAFETY: still held.
                Ok(()) => unsafe { p.promise_release() },
                Err(e) => {
                    static ONCE: AtomicBool = AtomicBool::new(true);
                    if ONCE.swap(false, Ordering::Relaxed) {
                        tracing::warn!(
                            error = %e,
                            "explicit sync: release point not signalled — the producer may never \
                             paint this buffer again"
                        );
                    }
                }
            }
        }
        // SAFETY: the caller's contract is `queue`'s.
        unsafe { stream.queue(buf) };
    }

    /// Signal `buf`'s release point, unless this side already did. Loop thread; the ioctls
    /// run outside the book's lock, which a dropping hold takes.
    fn release(&self, buf: usize, p: &SyncPoints) -> std::io::Result<()> {
        let signalled = self.book().entry(buf).and_then(|e| {
            e.waiting = None;
            e.released
        });
        if signalled == Some(p.release_point) {
            return Ok(());
        }
        self.timelines(buf, p)?.release.signal(p.release_point)?;
        if let Some(e) = self.book().entry(buf) {
            e.released = Some(p.release_point);
        }
        Ok(())
    }

    /// Signal the release point of each pool buffer that never reached this side.
    ///
    /// PipeWire recycles a buffer its consumer did not dequeue in time. A producer that leaves
    /// `UNSCHEDULED_RELEASE` clear cannot tell, waits on that release point for good, and its
    /// pool is one buffer short from then on. A point is signalled only behind its acquire
    /// point, so a buffer the producer is painting is left alone.
    ///
    /// # Safety
    /// Loop thread, outside `.process`: every buffer this side dequeued is then withheld or
    /// handed back.
    pub(super) unsafe fn recover_undelivered(&self) {
        if self.sync.is_none() {
            return;
        }
        let now = Instant::now();
        // Only the loop thread withholds a buffer, so none of these becomes withheld below.
        let idle: Vec<_> = self
            .book()
            .bufs
            .iter()
            .filter(|(_, e)| !withheld(e.state))
            .map(|(buf, e)| (*buf, e.released, e.waiting))
            .collect();
        let mut n = 0;
        for (buf, released, seen) in idle {
            // SAFETY: `buf` is a live `pw_buffer`: `remove_buffer` takes a buffer out of the
            // book. The producer may be writing the meta; `undelivered` takes a torn pair for
            // settled.
            let Some(p) = (unsafe { SyncPoints::of((*(buf as *mut pw::sys::pw_buffer)).buffer) })
            else {
                continue;
            };
            let points = (p.flags, p.acquire_point, p.release_point);
            match undelivered(points, released, seen, now) {
                Undelivered::Settled => {}
                Undelivered::Watch => {
                    if seen.is_none_or(|(point, _)| point != p.release_point) {
                        if let Some(e) = self.book().entry(buf) {
                            e.waiting = Some((p.release_point, now));
                        }
                    }
                }
                Undelivered::Release => {
                    let painted = self.timelines(buf, &p);
                    if painted.is_ok_and(|t| t.acquire.available(p.acquire_point))
                        && self.release(buf, &p).is_ok()
                    {
                        n += 1;
                    }
                }
            }
        }
        if n > 0 && self.undelivered.fetch_add(n, Ordering::Relaxed) == 0 {
            tracing::info!(
                buffers = n,
                "explicit sync: the producer sent buffers that never arrived and does not \
                 reclaim them itself — their release points are signalled here \
                 (undelivered= on the provenance line)"
            );
        }
    }
}

/// Releases its buffer to the producer when the last clone drops. Park-and-wake only from the
/// dropping thread; `pw_stream_queue_buffer` runs on the loop thread ([`PoolLedger::drain`]).
struct BufferHold {
    ledger: Arc<PoolLedger>,
    buf: usize,
    generation: u64,
}

impl Drop for BufferHold {
    fn drop(&mut self) {
        self.ledger.book().drop_hold(self.buf, self.generation);
        let _ = self.ledger.wake.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::{pool_ask, Book, PoolCensus, RAW_LANE_POOL_MIN, SHALLOW_POOL};

    #[derive(Clone, Copy, Debug)]
    enum Op {
        /// The census: how many buffers the pool has.
        Pool(u32),
        Add(usize),
        Remove(usize),
        Arrive(usize),
        Hold(usize),
        Drop(usize, u64),
        Drain,
        Done(usize),
    }

    #[derive(Debug, PartialEq)]
    enum Saw {
        /// The op answers nothing.
        Ok,
        Resent,
        Held(u64),
        Refused,
        Requeued(&'static [usize]),
        /// `.process` hands the buffer back.
        Back,
        /// A hold owns the requeue.
        Kept,
    }

    const A: usize = 0xa;
    const B: usize = 0xb;
    const C: usize = 0xc;
    const D: usize = 0xd;
    const E: usize = 0xe;

    struct Row {
        /// The fix this row pins.
        fix: &'static str,
        /// The tests it is taken from.
        from: &'static str,
        steps: &'static [(Op, Saw)],
    }

    use Op::*;
    use Saw::{Back, Held, Kept, Refused, Requeued, Resent};

    const KWIN: u32 = crate::KWIN_POOL_MAX as u32;

    const ROWS: &[Row] = &[
        Row {
            fix: "12e428be0 re-hold a re-sent buffer instead of rebuilding",
            from: "hold_book_rehold_spends_no_second_slot, \
                   a_resent_buffer_is_reheld_under_a_new_generation",
            steps: &[
                (Pool(SHALLOW_POOL + 1), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(B), Saw::Ok),
                (Hold(B), Held(2)),
                (Done(B), Kept),
                (Arrive(C), Saw::Ok),
                (Hold(C), Held(3)),
                (Done(C), Kept),
                (Arrive(D), Saw::Ok),
                (Hold(D), Refused),
                (Done(D), Back),
                (Arrive(A), Resent),
                (Hold(A), Held(4)),
                (Done(A), Kept),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(A, 4), Saw::Ok),
                (Drain, Requeued(&[A])),
            ],
        },
        Row {
            fix: "20c346a90 a hold shortage drops the arrival, never copies",
            from: "hold_book_lends_the_whole_pool_or_nothing, a_kwin_pool_lends_all_four, \
                   an_arrival_spends_only_a_hold_that_came_back",
            steps: &[
                (Pool(SHALLOW_POOL), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Refused),
                (Done(A), Back),
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Hold(B), Held(2)),
                (Hold(C), Held(3)),
                (Hold(D), Held(4)),
                (Arrive(E), Saw::Ok),
                (Hold(E), Refused),
                (Done(E), Back),
                (Drop(B, 2), Saw::Ok),
                (Drain, Requeued(&[B])),
                (Arrive(E), Saw::Ok),
                (Hold(E), Held(5)),
                (Drop(B, 2), Saw::Ok),
                (Drain, Requeued(&[])),
            ],
        },
        Row {
            fix: "ee709266d requeue a dropped hold before the arrival gives up",
            from: "an_arrival_drains_a_release_the_loop_has_not_serviced",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Hold(A), Held(1)),
                (Hold(B), Held(2)),
                (Hold(C), Held(3)),
                (Hold(D), Held(4)),
                (Drop(A, 1), Saw::Ok),
                (Arrive(E), Saw::Ok),
                (Hold(E), Refused),
                (Drain, Requeued(&[A])),
                (Hold(E), Held(5)),
                (Done(E), Kept),
                (Drain, Requeued(&[])),
            ],
        },
        Row {
            fix: "2b771fe20 purge held buffers before handing them back",
            from: "a_burst_hands_its_stale_buffers_back_once",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(A), Resent),
                (Done(A), Back),
                (Arrive(B), Saw::Ok),
                (Done(B), Back),
                (Arrive(C), Saw::Ok),
                (Drain, Requeued(&[])),
                (Hold(C), Held(2)),
                (Done(C), Kept),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(C, 2), Saw::Ok),
                (Drain, Requeued(&[C])),
            ],
        },
        Row {
            fix: "a re-sent buffer is re-held under the new frame",
            from: "a_resent_buffer_is_reheld_under_the_new_frame",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(A), Resent),
                (Drain, Requeued(&[])),
                (Hold(A), Held(2)),
                (Done(A), Kept),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(A, 2), Saw::Ok),
                (Drain, Requeued(&[A])),
            ],
        },
        Row {
            fix: "4f2bb5bbe purge a re-sent buffer before draining releases",
            from: "a_resent_buffer_is_not_handed_back_by_its_stale_release",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Drop(A, 1), Saw::Ok),
                (Arrive(A), Resent),
                (Drain, Requeued(&[])),
                (Hold(A), Held(2)),
                (Done(A), Kept),
                (Drop(A, 2), Saw::Ok),
                (Drain, Requeued(&[A])),
            ],
        },
        Row {
            fix: "one hold, one requeue",
            from: "a_held_buffer_rejoins_once_after_its_hold_drops, \
                   hold_book_releases_exactly_once",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Drain, Requeued(&[])),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[A])),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
            ],
        },
        Row {
            fix: "a renegotiated pool takes no release from the old one",
            from: "a_renegotiated_pool_takes_no_release_from_the_old_one, \
                   hold_book_generation_outlives_an_address_reuse",
            steps: &[
                (Pool(KWIN), Saw::Ok),
                (Add(A), Saw::Ok),
                (Add(B), Saw::Ok),
                (Arrive(A), Saw::Ok),
                (Hold(A), Held(1)),
                (Done(A), Kept),
                (Arrive(B), Saw::Ok),
                (Hold(B), Held(2)),
                (Done(B), Kept),
                (Remove(A), Saw::Ok),
                (Remove(B), Saw::Ok),
                (Drop(A, 1), Saw::Ok),
                (Drain, Requeued(&[])),
                (Add(A), Saw::Ok),
                (Add(B), Saw::Ok),
                (Arrive(B), Saw::Ok),
                (Hold(B), Held(3)),
                (Done(B), Kept),
                (Drop(B, 2), Saw::Ok),
                (Drain, Requeued(&[])),
                (Drop(B, 3), Saw::Ok),
                (Drain, Requeued(&[B])),
            ],
        },
    ];

    fn run(book: &mut Book, pool: &mut u32, op: Op) -> Saw {
        match op {
            Pool(n) => *pool = n,
            Add(buf) => book.add(buf),
            Remove(buf) => book.remove(buf),
            Arrive(buf) if book.arrive(buf) => return Resent,
            Arrive(_) => {}
            Hold(buf) => return book.hold(buf, *pool).map_or(Refused, Held),
            Drop(buf, generation) => book.drop_hold(buf, generation),
            Drain => return Requeued(book.drain().leak()),
            Done(buf) if book.done(buf) => return Back,
            Done(_) => return Kept,
        }
        Saw::Ok
    }

    /// Each row replays what a fix pinned, one buffer event at a time.
    #[test]
    fn a_pool_buffer_lives_as_each_fix_pinned_it() {
        for row in ROWS {
            let (mut book, mut pool) = (Book::default(), 0);
            for (i, (op, want)) in row.steps.iter().enumerate() {
                let saw = run(&mut book, &mut pool, *op);
                assert_eq!(&saw, want, "{} ({}), step {i}: {op:?}", row.fix, row.from);
            }
        }
    }

    /// Two holds dropped before the loop runs its wake rejoin in the order they dropped, and
    /// stay withheld until then.
    #[test]
    fn released_buffers_rejoin_in_drop_order() {
        let mut book = Book::default();
        for buf in [A, B, C] {
            book.hold(buf, 4).unwrap();
        }
        book.drop_hold(C, 3);
        book.drop_hold(A, 1);
        assert_eq!(book.withheld_count(), 3, "released, not yet requeued");
        assert_eq!(book.drain(), [C, A]);
        assert_eq!(book.withheld_count(), 1);
    }

    /// A stable pool logs one line, not one per frame. `.process` runs at capture rate.
    #[test]
    fn a_stable_pool_is_logged_once() {
        let mut p = PoolCensus::default();
        for _ in 0..8 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(8));
        for _ in 0..100 {
            assert_eq!(p.note_frame(), None, "the same depth must not re-log");
        }
    }

    /// A renegotiation frees the pool and re-allocates it. The LIVE count therefore dips (and the
    /// new depth is worth a second line), but `high_water` — the number a pipeline-depth decision
    /// keys on — must not follow the dip down.
    #[test]
    fn a_renegotiated_pool_relogs_but_the_high_water_holds() {
        let mut p = PoolCensus::default();
        for _ in 0..8 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(8));
        for _ in 0..8 {
            p.remove();
        }
        for _ in 0..4 {
            p.add();
        }
        assert_eq!(p.note_frame(), Some(4), "a changed depth is worth a line");
        assert_eq!(p.high_water, 8, "the deepest pool seen this session");
    }

    /// `remove_buffer` without a matching `add_buffer` must not wrap the count to `u32::MAX` —
    /// a depth gate reading that would happily pipeline against a pool of zero.
    #[test]
    fn unmatched_removes_saturate_at_zero() {
        let mut p = PoolCensus::default();
        p.remove();
        p.remove();
        assert_eq!(p.note_frame(), Some(0));
        assert_eq!(p.high_water, 0);
    }

    /// The raw lane deepens the ask to 6 unless the producer caps lower; KWin fails negotiation
    /// above its cap, so there the ask stays 4.
    #[test]
    fn the_raw_lane_pool_ask_stops_at_the_producers_cap() {
        assert_eq!(pool_ask(crate::POOL_MIN, None, false), crate::POOL_MIN);
        assert_eq!(pool_ask(crate::POOL_MIN, None, true), RAW_LANE_POOL_MIN);
        assert_eq!(
            pool_ask(crate::KWIN_POOL_MIN, Some(crate::KWIN_POOL_MAX), true),
            crate::KWIN_POOL_MAX
        );
        assert_eq!(
            pool_ask(crate::KWIN_POOL_MIN, Some(crate::KWIN_POOL_MAX), false),
            crate::KWIN_POOL_MIN
        );
        assert_eq!(
            pool_ask(4, Some(3), true),
            4,
            "never below the producer's own minimum"
        );
    }
}
