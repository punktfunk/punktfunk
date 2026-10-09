//! Deferred requeue: a published buffer stays out of the producer's pool until encode lets
//! go of it. The whole pool may be out: the producer then has nothing to paint and skips.

use super::UserData;
use crate::linux::sync_timeline::{hand_back, release_undelivered, SyncDevice};
use pipewire as pw;
use std::sync::atomic::Ordering;

impl UserData {
    /// Withhold this buffer from the producer until the returned hold drops.
    /// `None` requeues at `.process` return — the producer may then rewrite the dmabuf while
    /// encode still reads it, so no lane publishes a raw frame under `None`: a transient
    /// shortage drops the arrival, a pool that can never hold (`holds_possible` false) takes
    /// the lane's own fallback.
    /// No queued frame is given up for an arrival: the oldest render out is the next to
    /// finish, and a producer behind a full GPU queue would never see one through.
    /// A buffer the book already lists was re-sent by the producer: no hold, and the capture is
    /// flagged for a rebuild.
    pub(super) fn try_defer(
        &mut self,
        pw_buf: *mut pw::sys::pw_buffer,
        stream: *mut pw::sys::pw_stream,
    ) -> Option<pf_frame::FrameHold> {
        if !zerocopy_hold_enabled() {
            return None;
        }
        let buf = pw_buf as usize;
        // A hold the encoder dropped during this `.process` (the fence wait above is where it
        // lands) is still in the book until the loop services the wake — after this callback.
        // Requeue it now, so the arrival spends a budget that is current.
        // SAFETY: `stream` is the stream whose `.process` is running on this loop thread.
        unsafe { self.defer.drain(stream) };
        // PipeWire below 1.6 (no `node.reliable`) lets the producer reclaim a buffer before this
        // side marked it busy, then send it again. Its frame is fresh; the earlier hold's read may
        // be torn. Re-held under a new generation, the stale hold's release is a no-op
        // (`HoldBook::complete`), the pool stays whole, and the loop answers with one IDR.
        if self.defer.book.lock().ok()?.contains(buf)
            && self.signals.resent.fetch_add(1, Ordering::Relaxed) == 0
        {
            tracing::warn!(
                "producer re-sent a buffer this capture still holds — re-holding it; one IDR \
                 covers the frame encoded from the earlier read (PipeWire < 1.6)"
            );
        }
        let pool_live = self.pool.live;
        let Some(generation) = self.defer.book.lock().ok()?.try_hold(buf, pool_live) else {
            if !self.defer.logged_shallow.swap(true, Ordering::Relaxed) {
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
        if !self.defer.logged_active.swap(true, Ordering::Relaxed) {
            tracing::info!(
                pool_depth = pool_live,
                "zero-copy: withholding each published buffer from the producer until the \
                 encoder releases it (deferred requeue — the producer can no longer rewrite a \
                 frame mid-encode); PUNKTFUNK_ZEROCOPY_HOLD=0 restores the immediate requeue"
            );
        }
        Some(std::sync::Arc::new(BufferHold {
            defer: self.defer.clone(),
            buf,
            generation,
        }))
    }

    /// Requeue a buffer this `.process` publishes nothing from. One the book still lists was
    /// re-sent while held (PipeWire < 1.6): its hold is purged first, so the stale release
    /// no-ops and the buffer rejoins once, and `resent` asks for the covering IDR.
    ///
    /// # Safety
    /// Loop thread; `buf` was dequeued from the live `stream` and is not yet requeued.
    pub(super) unsafe fn requeue_unpublished(
        &self,
        stream: *mut pw::sys::pw_stream,
        buf: *mut pw::sys::pw_buffer,
    ) {
        let held = self
            .defer
            .book
            .lock()
            .map(|mut b| {
                let held = b.contains(buf as usize);
                b.purge(buf as usize);
                held
            })
            .unwrap_or(false);
        if held {
            self.signals.resent.fetch_add(1, Ordering::Relaxed);
        }
        // SAFETY: the caller's contract is `hand_back`'s; the purge above leaves no hold that
        // would queue `buf` a second time.
        unsafe { hand_back(self.sync.as_deref(), stream, buf) };
    }
}

/// How many buffers the producer allocated for this stream.
///
/// `live` comes from `add_buffer`/`remove_buffer` on the loop thread. There is no "pool
/// complete" event, so the count is published from `.process` (first dequeue ⇒ allocation
/// finished). Depth is the budget [`HoldBook::try_hold`] spends; a pool of ≤ [`SHALLOW_POOL`]
/// cannot defer and the producer may rewrite a buffer mid-encode.
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

/// `PUNKTFUNK_ZEROCOPY_HOLD=0` restores immediate requeue (racy). On the raw passthrough it
/// never publishes an unheld dmabuf: `try_defer` returns `None` there and the frame takes the
/// safe CPU fallback instead. Use `env_on`; a bare `== "0"` is the trap `PUNKTFUNK_FORCE_SHM`
/// already hit.
pub(super) fn zerocopy_hold_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_ZEROCOPY_HOLD").unwrap_or(true))
}

/// Which buffers are withheld, each under a per-hold generation.
///
/// A pointer-value reuse across pool renegotiation must not satisfy a stale hold (`complete`).
/// Insert/remove run only on the loop thread; a dropping [`BufferHold`] only sends. Between
/// hold creation and the loop servicing release, `contains` is stable — what `.process` uses
/// to choose "requeue now" vs "the hold owns the requeue".
#[derive(Default)]
pub(super) struct HoldBook {
    /// `*mut pw_buffer` as usize → generation that owns it.
    out: std::collections::HashMap<usize, u64>,
    /// Last issued hold generation (monotonic per stream).
    last_gen: u64,
}

impl HoldBook {
    /// Withhold `buf` unless the pool is too shallow to lend ([`SHALLOW_POOL`]) or is all out.
    /// A buffer already out was re-sent by the producer: the new generation takes over its
    /// requeue, and the stale hold's [`complete`](Self::complete) no longer matches.
    fn try_hold(&mut self, buf: usize, pool_live: u32) -> Option<u64> {
        let cap = if pool_live > SHALLOW_POOL {
            pool_live as usize
        } else {
            0
        };
        if !self.out.contains_key(&buf) && self.out.len() >= cap {
            return None;
        }
        self.last_gen += 1;
        self.out.insert(buf, self.last_gen);
        Some(self.last_gen)
    }

    /// Release `buf` iff this generation still owns it. `true` ⇒ caller must requeue;
    /// `false` ⇒ purged (pool renegotiated — pointer may be a new buffer) so do not touch it.
    fn complete(&mut self, buf: usize, generation: u64) -> bool {
        match self.out.get(&buf) {
            Some(&g) if g == generation => {
                self.out.remove(&buf);
                true
            }
            _ => false,
        }
    }

    /// `remove_buffer`: the buffer is being freed under us. Later release of its hold is a no-op.
    pub(super) fn purge(&mut self, buf: usize) {
        self.out.remove(&buf);
    }

    pub(super) fn contains(&self, buf: usize) -> bool {
        self.out.contains_key(&buf)
    }
}

/// Shared by the loop thread ([`HoldBook`] ops) and [`BufferHold`] guards on the encode thread.
pub(super) struct DeferredRequeue {
    pub(super) book: std::sync::Mutex<HoldBook>,
    /// Releases dropped on any thread, `(buffer, generation)`, until the loop thread requeues
    /// them: the wake callback does, and so does `try_defer` before it gives an arrival up.
    /// The book alone would count a hold the encoder already let go until the loop got round
    /// to the wake, and the arrival that finds it still out is lost.
    pub(super) pending: std::sync::Mutex<Vec<(usize, u64)>>,
    /// Wakes the loop to drain `pending`. Send failure = the loop is gone.
    pub(super) wake: pw::channel::Sender<()>,
    pub(super) logged_active: std::sync::atomic::AtomicBool,
    pub(super) logged_shallow: std::sync::atomic::AtomicBool,
    /// Signals a buffer's release point as it rejoins; `None` without explicit sync.
    pub(super) sync: Option<std::sync::Arc<SyncDevice>>,
    /// Every buffer of the pool, from `add_buffer` to `remove_buffer`.
    pub(super) pool: std::sync::Mutex<Vec<usize>>,
    /// Buffers [`release_undelivered`] gave back to the producer.
    pub(super) undelivered: std::sync::atomic::AtomicU64,
}

impl DeferredRequeue {
    /// Give the producer back every buffer it sent that never arrived here.
    ///
    /// # Safety
    /// Loop thread, outside `.process`: every buffer this side dequeued is then in the book
    /// or handed back.
    pub(super) unsafe fn recover_undelivered(&self) {
        let Some(dev) = &self.sync else { return };
        let (Ok(pool), Ok(book)) = (self.pool.lock(), self.book.lock()) else {
            return;
        };
        // SAFETY: `pool` lists live buffers only (`remove_buffer` takes them out).
        let n = unsafe { release_undelivered(dev, &pool, |buf| book.contains(buf)) };
        if n > 0 && self.undelivered.fetch_add(n as u64, Ordering::Relaxed) == 0 {
            tracing::info!(
                buffers = n,
                "explicit sync: the producer sent buffers that never arrived and does not \
                 reclaim them itself — their release points are signalled here \
                 (undelivered= on the provenance line)"
            );
        }
    }

    /// Requeue every release parked since the last drain. Returns how many buffers rejoined.
    ///
    /// # Safety
    /// Loop thread only, and `stream` is the live stream whose buffers this book tracks.
    pub(super) unsafe fn drain(&self, stream: *mut pw::sys::pw_stream) -> usize {
        self.drain_with(|buf| {
            // SAFETY: `drain_with` hands over only buffers the book still listed under the
            // dropping hold's generation, and `remove_buffer` purges the book: `buf` is a
            // live buffer of `stream` this side dequeued and never requeued.
            unsafe { hand_back(self.sync.as_deref(), stream, buf as *mut pw::sys::pw_buffer) };
        })
    }

    /// Complete every parked release in the book; `requeue` gets each buffer that was still
    /// withheld under its hold's generation (a purged or re-held one is skipped).
    fn drain_with(&self, mut requeue: impl FnMut(usize)) -> usize {
        let pending = self
            .pending
            .lock()
            .map(|mut p| std::mem::take(&mut *p))
            .unwrap_or_default();
        let mut rejoined = 0;
        for (buf, generation) in pending {
            let owned = self
                .book
                .lock()
                .map(|mut b| b.complete(buf, generation))
                .unwrap_or(false);
            if owned {
                requeue(buf);
                rejoined += 1;
            }
        }
        rejoined
    }
}

/// Releases its buffer to the producer when the last clone drops. Park-and-wake only from the
/// dropping thread; `pw_stream_queue_buffer` runs on the loop thread ([`DeferredRequeue::drain`]).
struct BufferHold {
    defer: std::sync::Arc<DeferredRequeue>,
    buf: usize,
    generation: u64,
}

impl Drop for BufferHold {
    fn drop(&mut self) {
        if let Ok(mut p) = self.defer.pending.lock() {
            p.push((self.buf, self.generation));
        }
        let _ = self.defer.wake.send(());
    }
}

#[cfg(test)]
mod tests {
    use super::{HoldBook, PoolCensus, SHALLOW_POOL};

    /// A re-sent buffer is re-held: the stale generation no longer requeues, the new one does,
    /// and the pool stays whole.
    #[test]
    fn a_resent_buffer_is_reheld_under_a_new_generation() {
        let mut book = HoldBook::default();
        let old = book.try_hold(0x10, SHALLOW_POOL + 2).unwrap();
        let new = book.try_hold(0x10, SHALLOW_POOL + 2).unwrap();
        assert!(new > old);
        assert_eq!(book.out.len(), 1);
        assert!(!book.complete(0x10, old));
        assert!(book.contains(0x10));
        assert!(book.complete(0x10, new));
        assert!(!book.contains(0x10));
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

    /// A pool lends every buffer it has, and a shallow one none: those sessions fall back
    /// to the immediate requeue.
    #[test]
    fn hold_book_lends_the_whole_pool_or_nothing() {
        let mut b = HoldBook::default();
        for i in 0..8 {
            assert!(b.try_hold(0x1000 + i, 8).is_some(), "hold {i} of 8");
        }
        assert!(b.try_hold(0x2000, 8).is_none(), "the pool has no ninth");
        assert!(
            HoldBook::default().try_hold(0x1000, SHALLOW_POOL).is_none(),
            "a shallow pool lends nothing"
        );
        assert!(HoldBook::default()
            .try_hold(0x1000, SHALLOW_POOL + 1)
            .is_some());
    }

    /// KWin's pool of 4: the host's frame and three renders out. A release gives exactly one
    /// hold back, and its late duplicate must not requeue the buffer a second time.
    #[test]
    fn a_kwin_pool_lends_all_four() {
        let pool = crate::KWIN_POOL_MAX as u32;
        let mut b = HoldBook::default();
        assert!(b.try_hold(0x1000, pool).is_some(), "the host's frame");
        let oldest = b.try_hold(0x2000, pool).expect("a render still out");
        assert!(b.try_hold(0x3000, pool).is_some(), "a second");
        assert!(b.try_hold(0x4000, pool).is_some(), "a third");
        assert!(b.try_hold(0x5000, pool).is_none(), "the pool is all out");
        assert!(b.complete(0x2000, oldest));
        assert!(b.try_hold(0x5000, pool).is_some());
        assert!(!b.complete(0x2000, oldest), "the late release no-ops");
    }

    /// The raw lane deepens the ask to 6 unless the producer caps lower; KWin fails negotiation
    /// above its cap, so there the ask stays 4.
    #[test]
    fn the_raw_lane_pool_ask_stops_at_the_producers_cap() {
        use super::{pool_ask, RAW_LANE_POOL_MIN};
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

    /// One hold ⇒ one requeue: the first `complete` releases, a duplicate release (a bug shape,
    /// but also the benign stale-message case) must NOT requeue a second time — handing the
    /// producer the same buffer twice corrupts its pool.
    #[test]
    fn hold_book_releases_exactly_once() {
        let mut b = HoldBook::default();
        let g = b.try_hold(0x1000, 8).unwrap();
        assert!(b.complete(0x1000, g), "first release requeues");
        assert!(!b.complete(0x1000, g), "second release is a no-op");
        assert!(!b.contains(0x1000));
    }

    /// Pool replaced (`remove_buffer` purges), a new buffer lands on the same address, then the
    /// old hold's release arrives. Matching by pointer alone would requeue the new tenant while
    /// its own hold is still out.
    #[test]
    fn hold_book_generation_outlives_an_address_reuse() {
        let mut b = HoldBook::default();
        let old = b.try_hold(0x1000, 8).unwrap();
        b.purge(0x1000); // `remove_buffer`: pool renegotiated away under the hold
        assert!(!b.complete(0x1000, old), "purged hold releases nothing");
        let new = b.try_hold(0x1000, 8).unwrap(); // new pool, same address
        assert!(
            !b.complete(0x1000, old),
            "the OLD hold cannot release the NEW tenant"
        );
        assert!(b.contains(0x1000), "new tenant still withheld");
        assert!(b.complete(0x1000, new), "its own hold releases it");
    }

    /// A buffer already out keeps one requeue duty: the re-hold moves it to the new
    /// generation instead of adding a second entry, and it does not spend the cap.
    #[test]
    fn hold_book_rehold_spends_no_second_slot() {
        let mut b = HoldBook::default();
        let pool = SHALLOW_POOL + 1;
        for buf in 0..pool as usize {
            b.try_hold(0x1000 + buf, pool).unwrap();
        }
        assert!(b.try_hold(0x2000, pool).is_none(), "the pool is all out");
        assert!(b.try_hold(0x1000, pool).is_some(), "re-hold within cap");
        assert_eq!(b.out.len(), pool as usize);
    }

    /// KWin's pool of 4 with every hold out, one of them already dropped on the encode
    /// thread. Before the loop services that wake the book still counts it, and the arrival
    /// would be dropped. `try_defer` drains the parked release first, so the arrival holds —
    /// and the wake callback that runs later finds nothing left to requeue.
    #[test]
    fn an_arrival_drains_a_release_the_loop_has_not_serviced() {
        use super::{BufferHold, DeferredRequeue};
        let pool = crate::KWIN_POOL_MAX as u32;
        let (wake, _rx) = pipewire::channel::channel::<()>();
        let defer = std::sync::Arc::new(DeferredRequeue {
            book: std::sync::Mutex::new(HoldBook::default()),
            pending: std::sync::Mutex::new(Vec::new()),
            wake,
            logged_active: std::sync::atomic::AtomicBool::new(false),
            logged_shallow: std::sync::atomic::AtomicBool::new(false),
            sync: None,
            pool: Default::default(),
            undelivered: Default::default(),
        });
        let hold = |buf: usize| {
            let generation = defer.book.lock().unwrap().try_hold(buf, pool).unwrap();
            BufferHold {
                defer: defer.clone(),
                buf,
                generation,
            }
        };
        let replaced = hold(0x1000);
        let _current = hold(0x2000);
        let _queued = (hold(0x2800), hold(0x2900));
        drop(replaced); // the encode thread let it go; the loop has not run its wake yet
        assert!(
            defer.book.lock().unwrap().try_hold(0x3000, pool).is_none(),
            "the book still counts the dropped hold"
        );
        let mut requeued = Vec::new();
        assert_eq!(defer.drain_with(|buf| requeued.push(buf)), 1);
        assert_eq!(requeued, vec![0x1000], "the dropped hold's buffer rejoins");
        assert!(
            defer.book.lock().unwrap().try_hold(0x3000, pool).is_some(),
            "the arrival now holds"
        );
        assert_eq!(
            defer.drain_with(|_| panic!("nothing left to requeue")),
            0,
            "the late wake is a no-op"
        );
    }
}
