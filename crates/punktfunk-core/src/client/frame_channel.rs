//! Pre-decode FIFO from the data-plane pump to the embedder, plus the
//! jump-to-live detectors and the decode/encode latency accumulators for ABR.
//!
//! Video AUs are reference-chained under an infinite GOP, so this queue never
//! drops a middle frame. A standing backlog is a jump-to-live (`clear` +
//! keyframe), not newest-wins. All-intra (PyroWave) is the exception: `pop`
//! drains to the newest AU.
//!
//! Two detectors: clock-based (`FLUSH_LATENCY` / `FLUSH_AFTER`, disarmed when
//! `clock_offset_ns == 0`) and clock-free (`QUEUE_HIGH` / `STANDING_TIME`).
//! The host recovery-cadence detector compares against `FLUSH_COOLDOWN` and
//! `NO_VIDEO_RETRY` themselves — do not copy the numbers.
//!
//! Tests in this file; pump wiring in `client/pump/data.rs`.

use crate::session::Frame;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Clock-free standing-queue trip: depth at/above this is "not draining".
/// 6 ≈ 100 ms at 60 fps — above a jitter buffer, so only a genuine backlog trips.
pub(crate) const QUEUE_HIGH: usize = 6;

/// Depth at/below which the standing-queue run resets. A true standing queue
/// never returns here; a clump does within a few frames.
pub(crate) const QUEUE_LOW: usize = 2;

/// Wall-clock the queue must sit ≥ [`QUEUE_HIGH`] (never dropping to [`QUEUE_LOW`])
/// before jump-to-live. 250 ms sits above a clump's drain (a few frames, ≤ ~100 ms)
/// at any fps — a frame-count would scale the accepted backlog with rate.
pub(crate) const STANDING_TIME: Duration = Duration::from_millis(250);

/// Memory backstop. The standing-queue detector jumps long before this; a jump
/// already requested a keyframe, so dropping the oldest AU is safe — the pending
/// IDR re-anchors. Hits only a wedged consumer during the flush cooldown.
const FRAME_QUEUE_HARD_CAP: usize = 90;

/// AUs held for a decoder that has not popped yet: the opening IDR and its dependents.
/// 48 ≈ 200 ms at 240 fps, 800 ms at 60. A slower decoder gets none and one keyframe.
pub(crate) const PREROLL_AUS: usize = 48;

/// Clock-based jump: completed frames this far behind the skew-corrected capture
/// clock. 400 ms sits above handshake error (≈ RTT/2) plus delivery jitter so a
/// healthy stream cannot trip; `clock_offset_ns == 0` disarms this path (same-clock
/// / no-handshake sessions use [`QUEUE_HIGH`]/[`STANDING_TIME`] instead).
pub(crate) const FLUSH_LATENCY: Duration = Duration::from_millis(400);

/// Wall-clock frames must run continuously over [`FLUSH_LATENCY`] before the
/// clock-based jump. 250 ms: a genuine standing queue puts every frame over the
/// bound; a one-off (IDR, scan blip) clears in a frame or two and resets the run.
pub(crate) const FLUSH_AFTER: Duration = Duration::from_millis(250);

/// Minimum spacing between jump-to-live events. A bottleneck that rebuilds the
/// queue instantly then degrades to a periodic skip instead of a keyframe storm.
///
/// Public: the host recovery-cadence detector compares against this constant, not
/// a copy. Each jump asks for a keyframe at exactly this period forever; that
/// perfect 2 s cadence is this cooldown, not a physical display disturbance.
pub const FLUSH_COOLDOWN: Duration = Duration::from_secs(2);

/// Keyframe re-ask spacing while no video has arrived — the opposite of
/// [`FLUSH_COOLDOWN`] (nothing vs too much). Public and 2600 ms (not 2000) so
/// the host recovery-cadence detector can tell the two faults apart; embedders
/// own this timer and must use this constant. [`crate::quic::LossReport`]
/// delivery counts settle it for clients new enough to send one.
pub const NO_VIDEO_RETRY: Duration = Duration::from_millis(2600);

/// One adaptive-FEC / ABR report window. A window the client discards (probe
/// tail, host pipeline gap) sends no [`crate::quic::LossReport`], so the host
/// reads a report later than this by a window as a discard, not jitter.
pub const ADAPT_REPORT_INTERVAL: Duration = crate::abr::WINDOW;

/// A clock-triggered jump that discarded fewer datagrams than this (and no queued
/// AUs) found no local backlog. Flushing helps neither a wall-clock step (NTP
/// shifts every future frame over-bound) nor an upstream queue (OWD already
/// feeds ABR). At the 5 Mbps floor a genuine 400 ms backlog is ~170 datagrams,
/// so 64 separates empty from real. See [`NOOP_CLOCK_FLUSHES_TO_DISARM`].
pub(crate) const NOOP_FLUSH_DATAGRAMS: u64 = 64;

/// Consecutive no-op clock flushes (see [`NOOP_FLUSH_DATAGRAMS`]) before the
/// clock-based detector disarms. Clock-free stays armed — it measures the local
/// queue. An applied mid-stream re-sync re-arms; disarm is the backstop between
/// re-syncs.
pub(crate) const NOOP_CLOCK_FLUSHES_TO_DISARM: u32 = 2;

/// Real backlog sheds under a pinned rate before the client says it cannot
/// keep up. [`FLUSH_COOLDOWN`] spaces them, so three is ≥ 4 s of falling
/// behind — a condition, not a loading burst.
pub(crate) const PIN_SHEDS_TO_WARN: u32 = 3;

/// Periodic mid-stream clock re-sync ([`ClockResync`]): 60 s bounds slow drift
/// and picks up an NTP step within a minute (8 tiny control messages per batch).
/// The pump also fires one immediately after the first no-op clock flush.
pub(crate) const CLOCK_RESYNC_INTERVAL: Duration = Duration::from_secs(60);

/// How far above the session's own OWD-floor a report window's *minimum* must
/// sit to count as standing. Jump-to-live ignores anything below ~6 frames /
/// 400 ms, so a sub-frame backlog or a stale offset is carried forever. 10 ms
/// is above handshake error + LAN jitter and below one 60 fps frame, so a
/// one-frame plateau (~17 ms) trips while a healthy stream cannot.
pub(crate) const STANDING_LAT_THRESH_NS: i128 = 10_000_000;

/// Consecutive elevated report windows (~750 ms each) before escalation — ~4.5 s
/// of loss-free standing elevation. Any loss resets the run: congestion is
/// FEC/ABR's, not this detector's.
pub(crate) const STANDING_LAT_WINDOWS: u32 = 6;

/// Per-session cap on flush+keyframe bleeds. Surviving a re-sync *and* this
/// many local flushes means the path latency itself changed; disarm instead of
/// paying a recovery keyframe every few seconds.
pub(crate) const STANDING_LAT_MAX_BLEEDS: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StandingLatAction {
    None,
    /// Mid-stream clock re-sync first: a stale offset from a stepped wall clock
    /// produces this signature. An applied re-sync re-bases the floor via the
    /// pump's `clock_gen` watch.
    Resync {
        above_ms: i64,
    },
    /// Elevation survived a re-sync: flush + keyframe. The pump reports
    /// execution via [`StandingLatency::bled`]; an unexecuted action re-arms
    /// next window.
    Bleed {
        above_ms: i64,
    },
    Disarm {
        above_ms: i64,
    },
}

/// Small, constant, loss-free OWD elevation — the band jump-to-live deliberately
/// tolerates. Tracks the session OWD floor (min of window-mins since start /
/// last rebase) and escalates: re-sync, then bounded bleeds, then disarm.
/// No clocks, no I/O: the ladder is unit-testable.
pub(crate) struct StandingLatency {
    floor_ns: Option<i128>,
    window_min_ns: Option<i128>,
    run: u32,
    /// This elevation already got its re-sync; next escalation is a bleed.
    resync_tried: bool,
    bleeds: u32,
    disarmed: bool,
}

impl StandingLatency {
    pub(crate) fn new() -> Self {
        StandingLatency {
            floor_ns: None,
            window_min_ns: None,
            run: 0,
            resync_tried: false,
            bleeds: 0,
            disarmed: false,
        }
    }

    /// One frame's skew-corrected OWD (capture→reassembly-complete, ns). Caller
    /// gates on a live clock offset and 0 < owd < 10 s, like the ABR OWD signal.
    pub(crate) fn note_frame(&mut self, owd_ns: i128) {
        self.window_min_ns = Some(match self.window_min_ns {
            Some(m) => m.min(owd_ns),
            None => owd_ns,
        });
    }

    /// Close a report window. Loss resets the run: congestion is FEC/ABR's, and
    /// queues under loss are not standing.
    pub(crate) fn on_window(&mut self, loss_free: bool) -> StandingLatAction {
        let Some(wmin) = self.window_min_ns.take() else {
            return StandingLatAction::None; // no frames this window — no evidence either way
        };
        let floor = *self.floor_ns.get_or_insert(wmin);
        self.floor_ns = Some(floor.min(wmin));
        let above_ns = wmin - floor;
        if self.disarmed {
            return StandingLatAction::None;
        }
        if !loss_free || above_ns < STANDING_LAT_THRESH_NS {
            self.run = 0;
            if above_ns < STANDING_LAT_THRESH_NS {
                self.resync_tried = false; // elevation cleared — a future one re-syncs first again
            }
            return StandingLatAction::None;
        }
        self.run += 1;
        if self.run < STANDING_LAT_WINDOWS {
            return StandingLatAction::None;
        }
        self.run = 0; // each escalation gets a fresh observation run
        let above_ms = (above_ns / 1_000_000) as i64;
        if !self.resync_tried {
            self.resync_tried = true;
            StandingLatAction::Resync { above_ms }
        } else if self.bleeds < STANDING_LAT_MAX_BLEEDS {
            StandingLatAction::Bleed { above_ms }
        } else {
            self.disarmed = true;
            StandingLatAction::Disarm { above_ms }
        }
    }

    /// Pump executed a [`StandingLatAction::Bleed`]. Floor is kept: a successful
    /// bleed returns OWD to it; an unsuccessful one leaves the elevation so the
    /// ladder continues toward the cap.
    pub(crate) fn bled(&mut self) {
        self.bleeds += 1;
        self.window_min_ns = None;
    }

    /// Applied mid-stream re-sync (pump `clock_gen` watch): OWDs shifted, so
    /// discard floor and elevation. Bleed budget survives (it caps keyframes
    /// per session).
    pub(crate) fn rebase(&mut self) {
        self.floor_ns = None;
        self.window_min_ns = None;
        self.run = 0;
        self.resync_tried = false;
    }
}

/// Which jump-to-live detector tripped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Trip {
    /// Every AU sat over [`FLUSH_LATENCY`] for [`FLUSH_AFTER`].
    pub(crate) clock: bool,
    /// The queue sat at [`QUEUE_HIGH`] for [`STANDING_TIME`] and still does.
    pub(crate) queue: bool,
}

/// What a jump-to-live's flush found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Shed {
    /// A clock-only trip found no local backlog. `disarmed`: this one turned
    /// the clock detector off.
    Noop { disarmed: bool },
    /// A real backlog, the `sheds`-th this session.
    Real { sheds: u32 },
}

/// Jump-to-live. In-order consume never catches up and an infinite GOP can't
/// drop a frame, so a backlog that stops draining is flushed. Two detectors
/// share [`FLUSH_COOLDOWN`]: the clock run ([`FLUSH_LATENCY`] for
/// [`FLUSH_AFTER`], needs a skew handshake) and the queue run ([`QUEUE_HIGH`]
/// for [`STANDING_TIME`]). Wall-clock, not frame counts: fps must not scale it.
/// The caller passes `now`, so the pump and the ABR simulator run one rule.
pub(crate) struct JumpToLive {
    stale_since: Option<Instant>,
    standing_since: Option<Instant>,
    last_flush: Option<Instant>,
    /// Consecutive clock-only flushes that found no local backlog.
    noop_clock_flushes: u32,
    clock_armed: bool,
    /// Flushes that found a real backlog. Under a pinned rate nothing else
    /// lowers the load, so this is the "can't keep up" count.
    real_sheds: u32,
    /// A held opening GOP drains until the depth first falls to [`QUEUE_LOW`].
    preroll_draining: bool,
    resync_wanted: bool,
}

impl JumpToLive {
    pub(crate) fn new() -> Self {
        JumpToLive {
            stale_since: None,
            standing_since: None,
            last_flush: None,
            noop_clock_flushes: 0,
            clock_armed: true,
            real_sheds: 0,
            preroll_draining: true,
            resync_wanted: false,
        }
    }

    /// An applied clock re-sync voids the clock run and re-arms its detector.
    /// `true` = the detector had been disarmed.
    pub(crate) fn rebase(&mut self) -> bool {
        self.stale_since = None;
        self.noop_clock_flushes = 0;
        !std::mem::replace(&mut self.clock_armed, true)
    }

    /// A frame neither detector may judge (probe burst, no decoder yet).
    pub(crate) fn idle(&mut self) {
        self.stale_since = None;
        self.standing_since = None;
    }

    /// One polled frame. `lat_ns` is its skew-corrected delay, `0` without an
    /// offset or for a prefix part; `depth` is the channel's. `Some` = flush
    /// now: the runs are reset and the cooldown is already stamped.
    pub(crate) fn observe(
        &mut self,
        now: Instant,
        lat_ns: i128,
        is_au: bool,
        depth: usize,
    ) -> Option<Trip> {
        if self.clock_armed && lat_ns > FLUSH_LATENCY.as_nanos() as i128 {
            self.stale_since.get_or_insert(now);
        } else if is_au {
            self.stale_since = None;
        }
        self.preroll_draining &= depth > QUEUE_LOW;
        if depth >= QUEUE_HIGH && !self.preroll_draining {
            self.standing_since.get_or_insert(now);
        } else if depth <= QUEUE_LOW {
            self.standing_since = None;
        }
        let ran = |since: Option<Instant>, d| {
            since.is_some_and(|t| now.saturating_duration_since(t) >= d)
        };
        // Still high NOW: a run in the hysteresis band (a clump mid-drain)
        // must not fire on elapsed time alone.
        let trip = Trip {
            clock: ran(self.stale_since, FLUSH_AFTER),
            queue: depth >= QUEUE_HIGH && ran(self.standing_since, STANDING_TIME),
        };
        if !(trip.clock || trip.queue) || !self.claim_flush(now) {
            return None;
        }
        self.idle();
        Some(trip)
    }

    /// Stamp the shared cooldown unless the last flush is under
    /// [`FLUSH_COOLDOWN`] old. `false` = too soon, nothing stamped.
    pub(crate) fn claim_flush(&mut self, now: Instant) -> bool {
        if self
            .last_flush
            .is_some_and(|t| now.saturating_duration_since(t) < FLUSH_COOLDOWN)
        {
            return false;
        }
        self.last_flush = Some(now);
        true
    }

    /// What `trip`'s flush discarded. A clock-only flush under
    /// [`NOOP_FLUSH_DATAGRAMS`] with no queued AU is a false behind (clock step,
    /// upstream queue): the first asks for a re-sync, the
    /// [`NOOP_CLOCK_FLUSHES_TO_DISARM`]-th disarms the clock detector.
    pub(crate) fn after_flush(&mut self, trip: Trip, flushed: u64, dropped: usize) -> Shed {
        if trip.clock && !trip.queue && flushed < NOOP_FLUSH_DATAGRAMS && dropped == 0 {
            self.noop_clock_flushes += 1;
            self.resync_wanted |= self.noop_clock_flushes == 1;
            let disarm = self.noop_clock_flushes >= NOOP_CLOCK_FLUSHES_TO_DISARM;
            self.clock_armed &= !disarm;
            return Shed::Noop { disarmed: disarm };
        }
        self.noop_clock_flushes = 0;
        self.real_sheds += 1;
        Shed::Real {
            sheds: self.real_sheds,
        }
    }

    /// The re-sync the first no-op clock flush asked for, once.
    pub(crate) fn take_resync(&mut self) -> bool {
        std::mem::take(&mut self.resync_wanted)
    }
}

/// Client decode latency for ABR. Embedder samples via
/// [`NativeClient::report_decode_us`] (µs from [`NativeClient::next_frame`] to
/// decoded output); the pump drains a window mean into
/// [`crate::abr::Driver`]. Only signal that sees the
/// client's decoder — a fast-LAN HW decoder saturates before the link, where
/// loss/OWD never register. Sum+count (not a running mean) so the pump takes
/// an unweighted mean and resets. Always accumulated so it stays bounded
/// (~180 samples at 240 fps) even when Automatic is off.
#[derive(Default)]
pub(crate) struct DecodeLatAcc {
    pub(crate) sum_us: u64,
    pub(crate) count: u32,
}

/// Host encode latency — [`DecodeLatAcc`]'s mirror. Datagram task samples
/// `HostStages::encode_us` (submit → bitstream ready); the pump drains a window
/// mean into [`crate::abr::Driver`]. Own accumulator, not
/// the overlay `host_timing` channel: that is a lossy `try_send` the embedder
/// may never drain, and a fat-LAN Automatic session otherwise drives the
/// encoder past its compute knee with nothing to stop it.
#[derive(Default)]
pub(crate) struct EncodeLatAcc {
    pub(crate) sum_us: u64,
    pub(crate) count: u32,
}

/// Pre-decode video hand-off from the data-plane pump to the embedder.
///
/// Side planes drop newest on overflow; video AUs are reference-chained under
/// an infinite GOP, so dropping any mid-stream frame corrupts dependents until
/// the next IDR. Strict FIFO, never a middle drop. Persistent underrun is a
/// jump-to-live ([`Self::clear`] + keyframe), not newest-wins.
///
/// All-intra exception ([`Self::set_all_intra`], PyroWave): every AU decodes
/// independently, so [`Self::pop`] drains to the newest and caps a standing
/// queue at ~1 frame with no keyframe round-trip.
pub(crate) struct FrameChannel {
    inner: Mutex<FrameQueue>,
    ready: Condvar,
    /// Set by the first [`Self::pop`]. Until then no decoder exists (a console
    /// launch hold keeps it away for up to 15 s) and the pump only [`Self::preroll`]s.
    consumer_seen: AtomicBool,
}

struct FrameQueue {
    q: VecDeque<Frame>,
    /// Pump exited: a blocked [`FrameChannel::pop`] reports Closed, not Timeout.
    closed: bool,
    /// Every AU decodes independently (PyroWave): [`FrameChannel::pop`] drains to the newest.
    all_intra: bool,
    /// AUs skipped by the all-intra drain since last [`FrameChannel::take_skipped`].
    /// Not losses — the wire delivered them; the overlay counts them as skipped.
    skipped_total: u64,
    /// The same skips over the whole session.
    skipped_ever: u64,
}

/// [`FrameChannel::pop`] result. `next_frame` maps Timeout/Closed as-is.
pub(crate) enum FramePop {
    Frame(Frame),
    Timeout,
    Closed,
}

impl FrameChannel {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(FrameQueue {
                q: VecDeque::new(),
                closed: false,
                all_intra: false,
                skipped_total: 0,
                skipped_ever: 0,
            }),
            ready: Condvar::new(),
            consumer_seen: AtomicBool::new(false),
        }
    }

    /// Whether anything has ever popped. False = no decoder yet: the pump holds
    /// the opening GOP through [`Self::preroll`] instead of queueing.
    pub(crate) fn consumer_seen(&self) -> bool {
        self.consumer_seen.load(Ordering::Relaxed)
    }

    /// Hold `frame` for a decoder that has not popped yet, while fewer than [`PREROLL_AUS`]
    /// AUs are held. At the bound the held GOP goes whole, since its dependents are useless
    /// without its IDR. Returns the AUs dropped, `frame` included; 0 = held.
    pub(crate) fn preroll(&self, frame: Frame) -> usize {
        let mut st = self.inner.lock().unwrap();
        let held = st.q.iter().filter(|f| f.complete).count();
        if held >= PREROLL_AUS {
            st.q.clear();
            return held + usize::from(frame.complete);
        }
        st.q.push_back(frame);
        drop(st);
        self.ready.notify_one();
        0
    }

    pub(crate) fn set_all_intra(&self, all_intra: bool) {
        self.inner.lock().unwrap().all_intra = all_intra;
    }

    /// All-intra skips since last call; resets on read.
    pub(crate) fn take_skipped(&self) -> u64 {
        let mut st = self.inner.lock().unwrap();
        std::mem::take(&mut st.skipped_total)
    }

    /// All-intra skips this session. Monotonic.
    pub(crate) fn skipped_ever(&self) -> u64 {
        self.inner.lock().unwrap().skipped_ever
    }

    pub(crate) fn push(&self, frame: Frame) {
        let mut st = self.inner.lock().unwrap();
        st.q.push_back(frame);
        while st.q.len() > FRAME_QUEUE_HARD_CAP {
            st.q.pop_front();
        }
        drop(st);
        self.ready.notify_one();
    }

    /// Queued depth in completed AUs — the clock-free standing-queue signal.
    /// Slice-progressive parts of an open AU do not count: thresholds are in
    /// frames, and counting parts would trip at a fraction of the real backlog.
    pub(crate) fn depth(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .q
            .iter()
            .filter(|f| f.complete)
            .count()
    }

    pub(crate) fn clear(&self) -> usize {
        let mut st = self.inner.lock().unwrap();
        let n = st.q.len();
        st.q.clear();
        n
    }

    pub(crate) fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    /// Oldest AU, waiting up to `timeout`. All-intra ([`Self::set_all_intra`])
    /// drains a multi-deep queue to the newest — skipped AUs are superseded and
    /// decode independently.
    ///
    /// The drain counts queue *entries* and assumes one entry == one AU, which
    /// holds because PyroWave refuses slice-progressive delivery
    /// (`client/pump/handshake.rs`; [`crate::session::Session::set_deliver_frame_parts`]).
    /// Combining the two splits an AU: `len > 1` fires mid-AU, returns a suffix,
    /// and `clear()`s that AU's prefixes — a headerless frame. To compose them,
    /// skip whole superseded AUs (drop up to the newest `part.first`), give
    /// `FRAME_QUEUE_HARD_CAP` eviction the same rule, and count `skipped_total`
    /// in AUs. Streamed AUs ([`crate::quic::VIDEO_CAP_STREAMED_AU`]) still
    /// arrive as one `Frame`.
    pub(crate) fn pop(&self, timeout: Duration) -> FramePop {
        self.consumer_seen.store(true, Ordering::Relaxed);
        let mut st = self.inner.lock().unwrap();
        if st.q.is_empty() && !st.closed {
            st = self.ready.wait_timeout(st, timeout).unwrap().0;
        }
        if st.all_intra && st.q.len() > 1 {
            st.skipped_total += (st.q.len() - 1) as u64;
            st.skipped_ever += (st.q.len() - 1) as u64;
            let newest = st.q.pop_back().expect("len > 1");
            st.q.clear();
            return FramePop::Frame(newest);
        }
        if let Some(f) = st.q.pop_front() {
            FramePop::Frame(f)
        } else if st.closed {
            FramePop::Closed
        } else {
            FramePop::Timeout
        }
    }
}

#[cfg(test)]
mod frame_channel_tests {
    use super::{FrameChannel, FramePop, FRAME_QUEUE_HARD_CAP, PREROLL_AUS};
    use crate::session::Frame;
    use std::time::Duration;

    fn frame(i: u32) -> Frame {
        Frame {
            data: vec![i as u8],
            frame_index: i,
            pts_ns: i as u64,
            flags: 0,
            epoch: 0,
            complete: true,
            part: None,
            received_ns: 0,
        }
    }

    #[test]
    fn depth_counts_aus_not_parts() {
        let ch = FrameChannel::new();
        let mut p = frame(1);
        p.complete = false;
        p.part = Some(crate::session::FramePart {
            offset: 0,
            first: true,
            last: false,
        });
        ch.push(p);
        assert_eq!(
            ch.depth(),
            0,
            "an open AU's prefix part is not a queued frame"
        );
        ch.push(frame(2));
        assert_eq!(ch.depth(), 1);
    }

    fn popped(ch: &FrameChannel) -> Option<u32> {
        match ch.pop(Duration::from_millis(0)) {
            FramePop::Frame(f) => Some(f.frame_index),
            _ => None,
        }
    }

    #[test]
    fn preroll_holds_the_opening_gop_then_drops_it_whole() {
        let ch = FrameChannel::new();
        for i in 0..PREROLL_AUS as u32 {
            assert_eq!(ch.preroll(frame(i)), 0);
        }
        assert_eq!(ch.depth(), PREROLL_AUS);
        assert!(!ch.consumer_seen(), "holding is not consuming");
        assert_eq!(
            popped(&ch),
            Some(0),
            "the decoder opens on the first AU held"
        );

        let ch = FrameChannel::new();
        for i in 0..PREROLL_AUS as u32 {
            ch.preroll(frame(i));
        }
        assert_eq!(ch.preroll(frame(PREROLL_AUS as u32)), PREROLL_AUS + 1);
        assert_eq!(ch.depth(), 0, "a GOP without its IDR is not kept");
    }

    #[test]
    fn fifo_order_and_depth() {
        let ch = FrameChannel::new();
        assert_eq!(ch.depth(), 0);
        ch.push(frame(1));
        ch.push(frame(2));
        assert_eq!(ch.depth(), 2);
        assert_eq!(popped(&ch), Some(1));
        assert_eq!(popped(&ch), Some(2));
        assert_eq!(ch.depth(), 0);
    }

    #[test]
    fn all_intra_drains_to_newest_and_counts_skips() {
        let ch = FrameChannel::new();
        ch.set_all_intra(true);
        for i in 1..=3 {
            ch.push(frame(i));
        }
        assert_eq!(popped(&ch), Some(3));
        assert_eq!(ch.depth(), 0);
        assert_eq!(ch.take_skipped(), 2);
        assert_eq!(ch.take_skipped(), 0);
        ch.push(frame(4));
        assert_eq!(popped(&ch), Some(4));
        assert_eq!(ch.take_skipped(), 0);
        assert_eq!(ch.skipped_ever(), 2);
    }

    #[test]
    fn empty_pop_times_out_not_closed() {
        let ch = FrameChannel::new();
        assert!(matches!(
            ch.pop(Duration::from_millis(1)),
            FramePop::Timeout
        ));
    }

    #[test]
    fn clear_drops_backlog_and_reports_count() {
        let ch = FrameChannel::new();
        for i in 0..5 {
            ch.push(frame(i));
        }
        assert_eq!(ch.clear(), 5);
        assert_eq!(ch.depth(), 0);
        assert!(matches!(
            ch.pop(Duration::from_millis(1)),
            FramePop::Timeout
        ));
    }

    #[test]
    fn close_after_drain_reports_closed() {
        let ch = FrameChannel::new();
        ch.push(frame(7));
        ch.close();
        // Queued frames still drain BEFORE the Closed signal.
        assert_eq!(popped(&ch), Some(7));
        assert!(matches!(ch.pop(Duration::from_millis(1)), FramePop::Closed));
    }

    #[test]
    fn consumer_is_seen_on_first_pop_only() {
        let ch = FrameChannel::new();
        ch.push(frame(1));
        assert!(!ch.consumer_seen(), "a push is not a consumer");
        assert!(matches!(
            ch.pop(Duration::from_millis(0)),
            FramePop::Frame(_)
        ));
        assert!(ch.consumer_seen());
        // A timed-out pop counts too: the decoder is waiting, not absent.
        let ch = FrameChannel::new();
        assert!(matches!(
            ch.pop(Duration::from_millis(0)),
            FramePop::Timeout
        ));
        assert!(ch.consumer_seen());
    }

    #[test]
    fn hard_cap_drops_oldest() {
        let ch = FrameChannel::new();
        let total = FRAME_QUEUE_HARD_CAP as u32 + 10;
        for i in 0..total {
            ch.push(frame(i));
        }
        assert_eq!(ch.depth(), FRAME_QUEUE_HARD_CAP);
        assert_eq!(popped(&ch), Some(total - FRAME_QUEUE_HARD_CAP as u32));
    }
}

#[cfg(test)]
mod standing_latency_tests {
    use super::{
        StandingLatAction, StandingLatency, STANDING_LAT_MAX_BLEEDS, STANDING_LAT_THRESH_NS,
        STANDING_LAT_WINDOWS,
    };

    const FLOOR: i128 = 2_000_000; // a healthy 2 ms LAN OWD
    const ELEVATED: i128 = FLOOR + STANDING_LAT_THRESH_NS + 7_000_000; // ~one 60fps frame above

    fn run_windows(d: &mut StandingLatency, owd: i128, n: u32) -> StandingLatAction {
        for i in 0..n {
            d.note_frame(owd);
            let a = d.on_window(true);
            if i + 1 < n {
                assert_eq!(a, StandingLatAction::None, "window {i} escalated early");
            } else {
                return a;
            }
        }
        unreachable!("n > 0 by construction");
    }

    fn learned(d: &mut StandingLatency) {
        d.note_frame(FLOOR);
        assert_eq!(d.on_window(true), StandingLatAction::None);
    }

    #[test]
    fn healthy_stream_never_escalates() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        for _ in 0..(STANDING_LAT_WINDOWS * 4) {
            d.note_frame(FLOOR + STANDING_LAT_THRESH_NS - 1);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
    }

    #[test]
    fn escalation_ladder_resync_then_bleeds_then_disarm() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
        for _ in 0..STANDING_LAT_MAX_BLEEDS {
            assert!(matches!(
                run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
                StandingLatAction::Bleed { .. }
            ));
            d.bled();
        }
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Disarm { .. }
        ));
        d.note_frame(ELEVATED);
        assert_eq!(d.on_window(true), StandingLatAction::None);
    }

    #[test]
    fn loss_windows_reset_the_run() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        for _ in 0..(STANDING_LAT_WINDOWS - 1) {
            d.note_frame(ELEVATED);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
        d.note_frame(ELEVATED);
        assert_eq!(d.on_window(false), StandingLatAction::None);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
    }

    #[test]
    fn recovery_resets_the_ladder_to_resync_first() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
        d.note_frame(FLOOR);
        assert_eq!(d.on_window(true), StandingLatAction::None);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
    }

    #[test]
    fn applied_resync_rebases_and_clears_a_stale_offset_elevation() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        assert!(matches!(
            run_windows(&mut d, ELEVATED, STANDING_LAT_WINDOWS),
            StandingLatAction::Resync { .. }
        ));
        d.rebase();
        for _ in 0..(STANDING_LAT_WINDOWS * 2) {
            d.note_frame(FLOOR);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
    }

    #[test]
    fn empty_windows_are_no_evidence() {
        let mut d = StandingLatency::new();
        learned(&mut d);
        for _ in 0..(STANDING_LAT_WINDOWS - 1) {
            d.note_frame(ELEVATED);
            assert_eq!(d.on_window(true), StandingLatAction::None);
        }
        assert_eq!(d.on_window(true), StandingLatAction::None);
        d.note_frame(ELEVATED);
        assert!(matches!(
            d.on_window(true),
            StandingLatAction::Resync { .. }
        ));
    }
}

#[cfg(test)]
mod jump_to_live_tests {
    use super::*;

    const BEHIND: i128 = FLUSH_LATENCY.as_nanos() as i128 + 1;
    const MS: Duration = Duration::from_millis(1);

    /// A primed decoder: the opening GOP has drained.
    fn live() -> JumpToLive {
        let mut j = JumpToLive::new();
        assert_eq!(j.observe(Instant::now(), 0, true, 0), None);
        j
    }

    #[test]
    fn the_clock_run_trips_after_flush_after_then_cools_down() {
        let t0 = Instant::now();
        let mut j = live();
        assert_eq!(j.observe(t0, BEHIND, true, 0), None);
        assert_eq!(j.observe(t0 + FLUSH_AFTER - MS, BEHIND, true, 0), None);
        let trip = j.observe(t0 + FLUSH_AFTER, BEHIND, true, 0);
        assert_eq!(
            trip,
            Some(Trip {
                clock: true,
                queue: false
            })
        );
        // The run restarts; a second trip waits out the shared cooldown.
        let t1 = t0 + FLUSH_AFTER;
        assert_eq!(j.observe(t1 + FLUSH_AFTER, BEHIND, true, 0), None);
        assert_eq!(j.observe(t1 + FLUSH_AFTER * 2, BEHIND, true, 0), None);
        assert!(j.observe(t1 + FLUSH_COOLDOWN, BEHIND, true, 0).is_some());
    }

    #[test]
    fn an_on_time_au_resets_the_clock_run_and_a_prefix_part_does_not() {
        let t0 = Instant::now();
        let mut j = live();
        j.observe(t0, BEHIND, true, 0);
        j.observe(t0 + MS, 0, false, 0);
        assert!(j.observe(t0 + FLUSH_AFTER, BEHIND, true, 0).is_some());
        let t1 = t0 + FLUSH_AFTER + FLUSH_COOLDOWN;
        j.observe(t1, BEHIND, true, 0);
        j.observe(t1 + MS, 0, true, 0);
        assert_eq!(j.observe(t1 + FLUSH_AFTER, BEHIND, true, 0), None);
    }

    #[test]
    fn the_queue_run_trips_only_while_still_high() {
        let t0 = Instant::now();
        let mut j = live();
        j.observe(t0, 0, true, QUEUE_HIGH);
        // Mid-drain the run survives the hysteresis band but can't fire.
        assert_eq!(j.observe(t0 + STANDING_TIME, 0, true, QUEUE_HIGH - 1), None);
        let trip = j.observe(t0 + STANDING_TIME + MS, 0, true, QUEUE_HIGH);
        assert_eq!(
            trip,
            Some(Trip {
                clock: false,
                queue: true
            })
        );
        // Back at QUEUE_LOW the run is gone.
        let t1 = t0 + FLUSH_COOLDOWN * 2;
        j.observe(t1, 0, true, QUEUE_HIGH);
        j.observe(t1 + MS, 0, true, QUEUE_LOW);
        assert_eq!(j.observe(t1 + STANDING_TIME, 0, true, QUEUE_HIGH), None);
    }

    #[test]
    fn a_held_opening_gop_starts_no_queue_run_until_it_drains() {
        let t0 = Instant::now();
        let mut j = JumpToLive::new();
        j.observe(t0, 0, true, PREROLL_AUS);
        assert_eq!(j.observe(t0 + STANDING_TIME, 0, true, QUEUE_HIGH), None);
        j.observe(t0 + STANDING_TIME + MS, 0, true, QUEUE_LOW);
        let t1 = t0 + STANDING_TIME * 2;
        j.observe(t1, 0, true, QUEUE_HIGH);
        assert!(j.observe(t1 + STANDING_TIME, 0, true, QUEUE_HIGH).is_some());
    }

    #[test]
    fn no_op_clock_flushes_resync_once_then_disarm_until_a_rebase() {
        let clock = Trip {
            clock: true,
            queue: false,
        };
        let mut j = live();
        assert_eq!(j.after_flush(clock, 0, 0), Shed::Noop { disarmed: false });
        assert!(j.take_resync());
        assert!(!j.take_resync(), "one re-sync per no-op run");
        assert_eq!(j.after_flush(clock, 0, 0), Shed::Noop { disarmed: true });
        assert!(!j.take_resync());
        let t0 = Instant::now();
        j.observe(t0, BEHIND, true, 0);
        assert_eq!(j.observe(t0 + FLUSH_AFTER, BEHIND, true, 0), None);
        assert!(j.rebase(), "a re-sync re-arms a disarmed detector");
        assert!(!j.rebase());
        j.observe(t0, BEHIND, true, 0);
        assert!(j.observe(t0 + FLUSH_AFTER, BEHIND, true, 0).is_some());
    }

    #[test]
    fn a_flush_that_found_a_backlog_counts_as_a_real_shed() {
        let clock = Trip {
            clock: true,
            queue: false,
        };
        let mut j = live();
        assert_eq!(j.after_flush(clock, 0, 0), Shed::Noop { disarmed: false });
        assert_eq!(
            j.after_flush(clock, NOOP_FLUSH_DATAGRAMS, 0),
            Shed::Real { sheds: 1 }
        );
        assert_eq!(j.after_flush(clock, 0, 1), Shed::Real { sheds: 2 });
        let both = Trip {
            clock: true,
            queue: true,
        };
        assert_eq!(j.after_flush(both, 0, 0), Shed::Real { sheds: 3 });
        // The real shed broke the no-op run: the next no-op is a first again.
        assert_eq!(j.after_flush(clock, 0, 0), Shed::Noop { disarmed: false });
    }
}
