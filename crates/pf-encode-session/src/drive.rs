//! The encode thread's steady state ([`Drive`]): pool slot → `submit` → `poll` → heap + slot
//! table → `latest` + event, with back-pressure taken on pool slots and the wedge state word
//! kept for the host's classifier. The pool is whatever [`FrameSource`] the side runs.
//!
//! The loop is frame-driven, not timed: submit while there is room and a frame, publish while an
//! access unit is owed and ready, and otherwise park once on `{stop, pool, the backend's
//! completion event, a high-resolution timer}`. So an AU leaves the encoder as soon as it is
//! encoded rather than when the next frame is submitted, and a desktop that goes still still
//! publishes its last picture.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::offset_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pf_driver_proto::encode::FrameToken;
use pf_driver_proto::encode::au::{self, AuHeader, AuSlot, HeapRing};
use pf_encode_win::{AuChunk, Encoder};
use pf_frame::CapturedFrame;
use pf_frame::pace::{FrameCredit, GapChange, Restamp};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::Fail;
use crate::open::{hdr_meta, qpc_frequency, qpc_now, qpc_to_ns};
use crate::section::{Ctl, EncodeSession};

/// One queued frame: its slot, the compositor's present stamp in QPC ticks (zero when it has
/// none), and its source sequence.
pub type Slot = (usize, u64, u64);

/// Where a session's frames come from: the driver's pool under its swap chain, the capture
/// worker's under Windows Graphics Capture. Every method is called from the encode thread.
pub trait FrameSource: Sync {
    /// The encode thread starts (`true`) or stops (`false`) consuming.
    fn set_live(&self, live: bool);
    /// The oldest queued frame within `budget` frames, now the encoder's, and how many queued
    /// frames were shed to stay inside it.
    fn take_within(&self, budget: usize) -> (Option<Slot>, u64);
    /// The frame last taken, again: a recovery frame nothing composed for.
    fn republish(&self) -> Option<Slot>;
    /// A pointer this source blends moved, and its last frame can be re-encoded now.
    fn cursor_pending(&self) -> bool;
    /// That re-encode, with the pointer where it is now.
    fn cursor_republish(&self) -> Option<Slot>;
    /// A pointer-only frame was dropped: the move is still owed.
    fn cursor_changed(&self);
    /// `slot` as the frame `submit` takes, stamped `pts_ns`.
    fn frame(&self, slot: usize, pts_ns: u64) -> Result<CapturedFrame, Fail>;
    /// Hand `slot` back, whether its access unit was published or it was skipped.
    fn release(&self, slot: usize);
    /// Signalled once per queued frame and once per control wake.
    fn event(&self) -> RawHandle;
    /// Whether a frame is queued.
    fn has_full(&self) -> bool;
    /// Count one dropped frame; the new total, for the header.
    fn drop_one(&self) -> u64;
}

/// Submits allowed ahead of the oldest AU — the host's pipeline depth. Also what the pool
/// guarantees a backend that encodes an input texture where it lies: a slot handed to the
/// encoder sits in `encoding` and no drain pass can take it back until the AU is published.
pub const MAX_INFLIGHT: usize = 2;
/// Polls that return nothing while an AU is owed, before the state word says WEDGED.
const WEDGE_AFTER: Duration = Duration::from_secs(2);
/// How often the loop re-enters `poll` for a backend with no completion event
/// ([`Encoder::ready_event`]). Their poll is bounded-blocking or complete-at-submit, so this is
/// the cadence of a re-entry, not a spin.
const NO_EVENT_POLL: Duration = Duration::from_micros(200);
/// How long a produced chunk waits for heap space or a free slot before the AU is dropped and
/// a keyframe requested; longer would stall the encoder behind a host that stopped reading.
const SLOT_WAIT: Duration = Duration::from_millis(250);
/// Consecutive failed submits before the thread gives up on its backend.
const MAX_SUBMIT_FAILURES: u32 = 8;

/// Fault injection: park forever, holding the pool slot and the session, once `encoded` passes
/// `limit`. Nothing unparks this thread: the source has to keep producing against a thread
/// that is gone. `None`, which every shipping open passes, never parks.
fn block_if_armed(limit: Option<u64>, encoded: u64) {
    if limit.is_some_and(|n| encoded > n) {
        tracing::info!("encode: block-after armed — wedging at frame {encoded}");
        loop {
            std::thread::park();
        }
    }
}

fn stop_signalled(stop: HANDLE) -> bool {
    // SAFETY: `stop` is the worker's stop event, alive until the worker joins or leaks.
    unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
}

/// The stream's frame rate and the refresh the source composes at, both in Hz.
pub struct Rates {
    pub fps: u32,
    pub panel_hz: u32,
}

impl<'a> Drive<'a> {
    /// `stop` is the thread's stop event, alive for as long as this value. `block_after` is the
    /// fault-injection frame count ([`block_if_armed`]).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        enc: Box<dyn Encoder>,
        pool: &'a dyn FrameSource,
        session: &'a EncodeSession,
        stop: RawHandle,
        live: &'a AtomicBool,
        rates: Rates,
        opened_kbps: u32,
        block_after: Option<u64>,
    ) -> Self {
        let (heap_offset, heap_bytes) = session.section.heap();
        let fps = rates.fps.max(1);
        let qpc_hz = qpc_frequency();
        Self {
            enc,
            pool,
            session,
            ring: HeapRing::new(heap_offset, heap_bytes),
            wire_seq: session.wire_seq_base.load(Ordering::Acquire),
            publish_seq: 0,
            inflight: VecDeque::new(),
            mid_au: false,
            dropping_au: false,
            au_published: false,
            submit_failures: 0,
            want_republish: false,
            cursor_only: false,
            qpc_hz,
            frame_interval: Duration::from_micros(1_000_000 / u64::from(fps)),
            credit: FrameCredit::new(qpc_hz / u64::from(fps)),
            // Only a panel faster than the stream stamps on a grid finer than the frames.
            restamp: (rates.panel_hz > fps)
                .then(|| Restamp::new(qpc_hz / u64::from(rates.panel_hz))),
            last_frame: None,
            owed_since: None,
            ready_latch: false,
            timer: None,
            report: Report::new(qpc_hz, 1_000_000 / u64::from(fps)),
            applied_kbps: opened_kbps,
            state: au::ENCODER_OPEN,
            stop: HANDLE(stop),
            live,
            block_after,
            encoded: 0,
        }
    }

    /// A cursor-only frame when the pointer moved and a whole period passed with no frame of
    /// any kind, so the stream never exceeds the refresh — a 1 kHz mouse over a desktop that
    /// composes at refresh adds nothing, and between caps the mark coalesces to the latest
    /// position. `None` when no move is pending, the period has not elapsed, or the re-encode
    /// was pre-empted by a queued composed frame.
    fn cursor_frame(&mut self) -> Option<(usize, u64, u64)> {
        if !self.pool.cursor_pending() {
            return None;
        }
        if self
            .last_frame
            .is_some_and(|t| t.elapsed() < self.frame_interval)
        {
            return None;
        }
        self.pool.cursor_republish()
    }
}

/// A submitted frame whose access unit is still owed.
#[derive(Clone, Copy)]
struct Owed {
    slot: usize,
    /// The compositor's present stamp; zero, or the submit time, for a re-encode.
    qpc: u64,
    seq: u64,
    submitted: u64,
    /// The present stamp the access unit carries.
    qpc_pts: u64,
    /// A composed frame rather than a re-encode: the only kind whose stamps show cadence.
    composed: bool,
}

/// The steady state: pool slot → `submit` → `poll` → heap + slot table → `latest` + event.
///
/// Idle desktop: nothing is re-encoded at cadence — a pool with no new frame means no AU. A
/// keyframe request re-encodes the stash; the first frame is the pool's or the seed's.
pub struct Drive<'a> {
    enc: Box<dyn Encoder>,
    pool: &'a dyn FrameSource,
    session: &'a EncodeSession,
    ring: HeapRing,
    /// Next wire index to stamp; every published AU takes exactly one, a dropped AU none.
    wire_seq: u32,
    /// Publish-token sequence, per session thread.
    publish_seq: u32,
    /// Every frame whose AU is owed, in submit order.
    inflight: VecDeque<Owed>,
    /// A partially drained AU must finish through `poll_chunk`.
    mid_au: bool,
    /// The AU in progress could not be placed; its remaining chunks are dropped too.
    dropping_au: bool,
    /// A chunk of the AU in progress reached the section, so its wire index is spent even if
    /// the tail is dropped — the reader closes it as a gap, never splices the next AU on.
    au_published: bool,
    /// Consecutive `submit` failures; see [`MAX_SUBMIT_FAILURES`].
    submit_failures: u32,
    /// A keyframe or an RFI anchor is owed; if nothing composes, re-encode the stash instead
    /// of waiting.
    want_republish: bool,
    /// The frame last taken is a cursor-only re-encode.
    cursor_only: bool,
    qpc_hz: u64,
    /// The stream's frame period — the gap a cursor-only re-encode may fill.
    frame_interval: Duration,
    /// Composed frames are encoded at the stream rate however fast the panel composes.
    credit: FrameCredit,
    /// Present stamps moved onto the content's cadence, when the panel ticks faster than that.
    restamp: Option<Restamp>,
    /// When the last frame of any kind was taken; `None` before the first. A compose at
    /// refresh resets this every period, so the pointer rides the composed frames alone.
    last_frame: Option<Instant>,
    /// Since when an AU has been owed with nothing produced; cleared by every chunk. The wedge
    /// clock, kept across parks so a stream of composed frames cannot re-arm it forever.
    owed_since: Option<Instant>,
    /// The backend's completion event fired while the loop was parked on it. Auto-reset, so the
    /// park consumed it: without this latch the AU would wait out [`WEDGE_AFTER`].
    ready_latch: bool,
    /// The one timed wake ([`Drive::park`]); built on first use, `None` if the OS refused one.
    timer: Option<OwnedHandle>,
    report: Report,
    /// Rate the backend is encoding at, kbps. Seeded from the open and rewritten by every
    /// drained bitrate ctl, including a declined one — the host reads it back rather than
    /// assuming the ask landed.
    applied_kbps: u32,
    state: u32,
    stop: HANDLE,
    live: &'a AtomicBool,
    block_after: Option<u64>,
    /// Frames submitted, for [`block_if_armed`].
    encoded: u64,
}

impl Drive<'_> {
    fn stopped(&self) -> bool {
        !self.live.load(Ordering::Acquire) || stop_signalled(self.stop)
    }

    fn set_state(&mut self, state: u32) {
        if self.state != state && self.live.load(Ordering::Acquire) {
            self.state = state;
            self.session
                .section
                .store_u32(offset_of!(AuHeader, encoder_state), state);
        }
    }

    pub fn run(&mut self) {
        self.pool.set_live(true);
        loop {
            self.drain_ctl();
            if self.stopped() {
                break;
            }
            // Room and a frame: submit it. Publishing comes second so a burst keeps the encoder
            // fed, and the AU of the frame before it is retrieved on the very next turn.
            if self.inflight.len() < MAX_INFLIGHT
                && let Some(next) = self.take_next()
            {
                if !self.submit_one(next) {
                    break;
                }
                continue;
            }
            // An AU is owed and the encoder has it: publish and free the slot.
            if !self.inflight.is_empty() && self.au_ready() && self.publish_one() {
                continue;
            }
            self.park();
        }
        // No flush on the way out: a stopped session has nowhere to send the last AUs. A
        // detached thread owns nothing in the pool any more — its successor reclaimed it.
        if self.live.load(Ordering::Acquire) {
            for owed in self.inflight.drain(..) {
                self.pool.release(owed.slot);
            }
            self.pool.set_live(false);
        }
    }

    /// The `ENCODE_CTL` ops queued since the last frame, in order. An RFI the backend cannot
    /// honour becomes a keyframe request, the host's own fallback. Either way the recovery
    /// frame rides the next encode, and a still desktop composes none: the stash carries it.
    fn drain_ctl(&mut self) {
        for op in self.session.take_ctl() {
            match op {
                Ctl::RequestKeyframe => {
                    self.enc.request_keyframe();
                    self.want_republish = true;
                }
                Ctl::InvalidateRefFrames(first, last) => {
                    if !self
                        .enc
                        .invalidate_ref_frames(i64::from(first), i64::from(last))
                    {
                        self.enc.request_keyframe();
                    }
                    self.want_republish = true;
                }
                Ctl::DistrustReferences => self.enc.distrust_references(),
                Ctl::ReconfigureBitrate(kbps) => {
                    if self.enc.reconfigure_bitrate(u64::from(kbps) * 1000) {
                        // A backend that tracks its own clamp reports it; the rest took the ask.
                        self.applied_kbps = self
                            .enc
                            .applied_bitrate_bps()
                            .map_or(kbps, |bps| (bps / 1000) as u32);
                    } else {
                        tracing::info!("encode: backend declined bitrate {kbps} kbps in place");
                    }
                    // Declined or clamped, the host must see what is encoding: the ctl has no
                    // reply, so this stamp is the only thing that can contradict the ask.
                    self.session.section.store_u32(
                        offset_of!(AuHeader, applied_bitrate_kbps),
                        self.applied_kbps,
                    );
                }
                Ctl::SetHdrMeta(bytes) => self.enc.set_hdr_meta(hdr_meta(&bytes)),
                Ctl::Flush => {
                    if let Err(e) = self.enc.flush() {
                        tracing::info!("encode: flush failed: {e:#}");
                    }
                    self.drain_all();
                }
            }
        }
    }

    /// The next frame to submit: a composed one the stream-rate credit covers, else the stash for
    /// a recovery ask nothing composed for, else a cursor-only re-encode. A composed frame the
    /// credit does not cover waits for it, which also holds the other two back. The ask survives a
    /// turn that found the stash slot busy — the AU owed on it is about to free it. Every frame
    /// taken stamps `last_frame`: that is the clock the cursor-only cap runs on.
    fn take_next(&mut self) -> Option<(usize, u64, u64)> {
        let budget = self.credit.frames(qpc_now());
        let (composed, shed) = self.pool.take_within(budget);
        self.report.shed += shed;
        if composed.is_some() {
            self.credit.spend();
        }
        let mut next =
            composed.or_else(|| self.want_republish.then(|| self.pool.republish()).flatten());
        self.cursor_only = next.is_none();
        if self.cursor_only {
            next = self.cursor_frame();
        }
        if next.is_some() {
            self.want_republish = false;
            self.last_frame = Some(Instant::now());
        }
        next
    }

    fn drop_slot(&mut self, slot: usize) {
        self.release_if_live(slot);
        self.count_drop();
        if self.cursor_only {
            self.pool.cursor_changed();
        }
    }

    /// Hand a slot back unless this thread was detached: its successor reclaimed every slot
    /// (`reset`), and a release here would free one that successor is encoding.
    fn release_if_live(&self, slot: usize) {
        if self.live.load(Ordering::Acquire) {
            self.pool.release(slot);
        }
    }

    /// Submit one pool slot, or drop it where dropping is free. A dropped cursor-only frame
    /// marks the pointer changed again, or the gesture's last position is never sent. `false`
    /// means the backend has failed [`MAX_SUBMIT_FAILURES`] times running and the thread leaves.
    fn submit_one(&mut self, (slot, qpc, seq): (usize, u64, u64)) -> bool {
        // Back-pressure lands here: no free AU slot, no submit.
        if !self.section_has_free() {
            self.drop_slot(slot);
            return true;
        }
        // A cursor-only re-encode is stamped now, off the panel's grid: it keeps its stamp.
        let qpc_pts = match &mut self.restamp {
            Some(r) if !self.cursor_only => r.apply(qpc),
            _ => qpc,
        };
        let pts = qpc_to_ns(if qpc_pts == 0 { qpc_now() } else { qpc_pts }, self.qpc_hz);
        let frame = match self.pool.frame(slot, pts) {
            Ok(f) => f,
            Err(_) => {
                self.drop_slot(slot);
                return true;
            }
        };
        self.encoded += 1;
        block_if_armed(self.block_after, self.encoded);
        let index = self.wire_seq.wrapping_add(self.inflight.len() as u32);
        let submitted = qpc_now();
        if let Err(e) = self.enc.submit_indexed(&frame, index) {
            tracing::info!("encode: submit failed: {e:#}");
            self.release_if_live(slot);
            self.set_state(au::ENCODER_WEDGED);
            // A lazy backend re-runs its whole bring-up per submit; stop retrying at the compose
            // rate and leave the session threadless for the host's reset rung.
            self.submit_failures += 1;
            if self.submit_failures >= MAX_SUBMIT_FAILURES {
                tracing::info!("encode: {MAX_SUBMIT_FAILURES} submits failed in a row — leaving");
                return false;
            }
            return true;
        }
        self.submit_failures = 0;
        self.report.submits += 1;
        self.inflight.push_back(Owed {
            slot,
            qpc,
            seq,
            submitted,
            qpc_pts,
            composed: !self.cursor_only && qpc != 0,
        });
        true
    }

    /// The backend's completion signal for the oldest in-flight AU, in this crate's `windows`.
    fn ready_handle(&self) -> Option<HANDLE> {
        self.enc.ready_event().map(|h| HANDLE(h as *mut c_void))
    }

    /// Whether the oldest in-flight AU can be retrieved without blocking. An event backend
    /// answers from its completion event — consumed here, or latched when a park woke on it. A
    /// backend without one has no cheap answer, so its own bounded `poll` is the answer.
    fn au_ready(&mut self) -> bool {
        let Some(ev) = self.ready_handle() else {
            return true;
        };
        if self.ready_latch {
            return true;
        }
        // SAFETY: the backend's own completion event, alive while it holds the AU.
        self.ready_latch = unsafe { WaitForSingleObject(ev, 0) == WAIT_OBJECT_0 };
        self.ready_latch
    }

    /// One poll of the oldest in-flight AU. `false` means the backend produced nothing, so the
    /// caller parks; a poll failure counts as progress — the AU is gone either way.
    fn publish_one(&mut self) -> bool {
        let chunked = self.mid_au || self.enc.supports_chunked_poll();
        let next = if chunked {
            self.enc.poll_chunk()
        } else {
            self.enc.poll().map(|au| au.map(AuChunk::whole))
        };
        self.ready_latch = false;
        match next {
            Ok(Some(chunk)) => {
                self.owed_since = None;
                self.set_state(au::ENCODER_ENCODING);
                self.on_chunk(chunk);
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::info!("encode: poll failed: {e:#}");
                self.set_state(au::ENCODER_WEDGED);
                // A detached thread's slots were reclaimed by its successor; releasing one
                // here would free a slot that successor is encoding.
                if let Some(owed) = self.inflight.pop_front()
                    && self.live.load(Ordering::Acquire)
                {
                    self.pool.release(owed.slot);
                }
                self.mid_au = false;
                true
            }
        }
    }

    /// Every owed AU out — the `Ctl::Flush` tail, bounded by [`WEDGE_AFTER`] whether or not the
    /// backend signals completion.
    fn drain_all(&mut self) {
        let deadline = Instant::now() + WEDGE_AFTER;
        while !self.inflight.is_empty() && !self.stopped() && Instant::now() < deadline {
            if self.au_ready() && self.publish_one() {
                continue;
            }
            self.park();
        }
    }

    /// When the loop must wake without a signal: the credit a waiting composed frame needs, what
    /// is left of the period since the last frame when a cursor move is pending, or the re-entry
    /// cadence of a backend that owes an AU and signals nothing. `None` = park on the handles alone.
    fn timer_due(&self) -> Option<Duration> {
        let credit = (self.inflight.len() < MAX_INFLIGHT && self.pool.has_full())
            .then(|| Duration::from_nanos(qpc_to_ns(self.credit.wait(), self.qpc_hz)));
        let cursor = self.pool.cursor_pending().then(|| {
            self.last_frame
                .map(|t| self.frame_interval.saturating_sub(t.elapsed()))
                .unwrap_or_default()
        });
        let poll = (!self.inflight.is_empty() && self.enc.ready_event().is_none())
            .then_some(NO_EVENT_POLL);
        [credit, cursor, poll].into_iter().flatten().min()
    }

    /// Arm the timer for `due`. `false` if the OS refused one: the park then runs on its handles
    /// and the wedge timeout, which costs the cursor cap its precision, never a frame.
    fn arm_timer(&mut self, due: Duration) -> bool {
        if self.timer.is_none() {
            // SAFETY: plain unnamed timer creation, twice at most. The high-resolution flag needs
            // Windows 10 1803; an older host rejects it and the ordinary timer is the fallback.
            let made = unsafe {
                CreateWaitableTimerExW(
                    None,
                    PCWSTR::null(),
                    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                    TIMER_ALL_ACCESS.0,
                )
                .or_else(|_| CreateWaitableTimerExW(None, PCWSTR::null(), 0, TIMER_ALL_ACCESS.0))
            };
            // SAFETY: the handle was just created here and nothing else can close it.
            self.timer = made
                .ok()
                .map(|h| unsafe { OwnedHandle::from_raw_handle(h.0) });
        }
        let Some(timer) = &self.timer else {
            return false;
        };
        // Negative is relative, in 100 ns units; one unit minimum so a due-now timer still fires.
        let due_100ns = -((due.as_nanos() / 100).max(1).min(i64::MAX as u128) as i64);
        // SAFETY: our own timer handle; `due_100ns` is a valid local read during the call, and
        // there is no completion routine or resume.
        unsafe {
            let timer = HANDLE(timer.as_raw_handle());
            SetWaitableTimer(timer, &due_100ns, 0, None, None, false).is_ok()
        }
    }

    /// The loop's only wait: stop, a filled pool slot, the backend's completion event, and the
    /// high-resolution timer for the two things that need a deadline instead of a signal. A
    /// `WaitForMultipleObjects` timeout would quantise to WUDFHost's 15.6 ms tick, which caps
    /// pointer re-encodes near 64 Hz.
    ///
    /// The timeout is [`WEDGE_AFTER`] while an AU is owed — expiry there is the wedge the host's
    /// classifier reads — and infinite otherwise: every wake source signals a handle in this set.
    fn park(&mut self) {
        // Seeded with the stop event: only the first `n` entries are ever waited on.
        let mut handles = [self.stop; 4];
        let mut ready_at = usize::MAX;
        let mut n = 1;
        handles[n] = HANDLE(self.pool.event());
        n += 1;
        if let Some(ev) = self.ready_handle() {
            ready_at = n;
            handles[n] = ev;
            n += 1;
        }
        if let Some(due) = self.timer_due()
            && self.arm_timer(due)
            && let Some(timer) = &self.timer
        {
            handles[n] = HANDLE(timer.as_raw_handle());
            n += 1;
        }
        let ms = if self.inflight.is_empty() {
            // Nothing owed: the next AU starts its own wedge clock, not the last one's.
            self.owed_since = None;
            INFINITE
        } else {
            let since = *self.owed_since.get_or_insert_with(Instant::now);
            if since.elapsed() > WEDGE_AFTER {
                self.set_state(au::ENCODER_WEDGED);
            }
            WEDGE_AFTER.as_millis() as u32
        };
        self.report.parks += 1;
        // SAFETY: `stop` is the worker's stop event, alive until the worker joins or leaks; the
        // pool event lives as long as the pool the session's thread borrows; the completion event
        // belongs to the backend this loop owns; the timer is ours.
        let woke = unsafe { WaitForMultipleObjects(&handles[..n], false, ms) };
        // An auto-reset completion event this park consumed: without the latch its AU would sit
        // out the wedge timeout.
        if woke.0.wrapping_sub(WAIT_OBJECT_0.0) as usize == ready_at {
            self.ready_latch = true;
        }
    }

    fn states(&self) -> [u32; au::AU_SLOTS as usize] {
        core::array::from_fn(|i| self.session.section.slot_state(i).load(Ordering::Acquire))
    }

    fn section_has_free(&self) -> bool {
        self.states().contains(&au::FREE)
    }

    fn count_drop(&self) {
        if !self.live.load(Ordering::Acquire) {
            return;
        }
        let n = self.pool.drop_one();
        self.session
            .section
            .store_u64(offset_of!(AuHeader, dropped_total), n);
    }

    /// One chunk of the oldest in-flight AU: published, or dropped with the rest of its AU. A
    /// detached thread returning from its wedge touches neither the pool nor the section.
    fn on_chunk(&mut self, chunk: AuChunk) {
        let Some(&owed) = self.inflight.front() else {
            return;
        };
        if !self.live.load(Ordering::Acquire) {
            return;
        }
        self.mid_au = !chunk.last;
        if chunk.first {
            self.dropping_au = false;
            self.au_published = false;
        }
        if !self.dropping_au {
            if self.publish(&chunk, &owed) {
                self.au_published = true;
            } else {
                self.dropping_au = true;
                self.count_drop();
                self.enc.request_keyframe();
            }
        }
        if chunk.last {
            if self.au_published {
                self.wire_seq = self.wire_seq.wrapping_add(1);
                let n = self
                    .session
                    .section
                    .add_u64(offset_of!(AuHeader, published_total), 1);
                if n == 1 {
                    tracing::info!("encode: first AU published ({} B)", chunk.data.len());
                }
            }
            // Stamped before the release: that release is what ends a bypass hold.
            self.report.note_encoded(owed.submitted, qpc_now());
            if owed.composed {
                self.report.note_stamps(owed.qpc, owed.qpc_pts);
            }
            self.inflight.pop_front();
            self.pool.release(owed.slot);
            self.dropping_au = false;
        }
    }

    /// Heap bytes, slot record, `latest`, event — in that order. `false` when no placement
    /// came free within [`SLOT_WAIT`]. The record carries the frame's corrected present, submit
    /// and publish QPC, which is how the host splits its age; the age line here stays on the
    /// compositor's own stamp.
    fn publish(&mut self, chunk: &AuChunk, owed: &Owed) -> bool {
        let section = &self.session.section;
        let len = chunk.data.len() as u32;
        let deadline = Instant::now() + SLOT_WAIT;
        let (slot, offset) = loop {
            let states = self.states();
            if let Some(placed) = self.ring.take(len, &states) {
                break placed;
            }
            if Instant::now() > deadline || self.stopped() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        if !self.live.load(Ordering::Acquire) || !section.write_heap(offset, &chunk.data) {
            return false;
        }
        let flags = [
            (chunk.first, au::AU_FIRST),
            (chunk.last, au::AU_LAST),
            (chunk.keyframe, au::AU_KEYFRAME),
            (chunk.recovery_anchor, au::AU_RECOVERY_ANCHOR),
            (chunk.chunk_aligned, au::AU_CHUNK_ALIGNED),
            (chunk.recovery_point, au::AU_RECOVERY_POINT),
            (chunk.recovery_close, au::AU_RECOVERY_CLOSE),
        ]
        .into_iter()
        .fold(0, |acc, (on, bit)| if on { acc | bit } else { acc });
        let now = qpc_now();
        section.publish_slot(
            slot,
            &AuSlot {
                offset,
                len,
                wire_seq: self.wire_seq,
                source_seq: owed.seq as u32,
                qpc_pts: owed.qpc_pts,
                flags,
                state: au::PUBLISHED,
                qpc_submit: owed.submitted,
                qpc_published: now,
            },
        );
        self.publish_seq = self.publish_seq.wrapping_add(1);
        section.store_u64(offset_of!(AuHeader, last_au_qpc), now);
        self.report.note_publish(owed.qpc, now);
        section.publish_latest(FrameToken {
            generation: self.session.generation,
            seq: self.publish_seq,
            slot: slot as u8,
        });
        true
    }
}

/// The loop's own ten-second line: how many access units went out, how old each was when it did,
/// and how many submits and parks it took. AU age is `published_at − PresentDisplayQPCTime`, so
/// it carries the compose-to-publish span the design pays for a frame — encode time once the
/// loop stops fetching one AU on the next frame's submit.
///
/// `aged` is how many of `published` that span could be taken from, and reading it is not
/// optional: a cursor-only re-encode carries no present stamp, and a head whose stamp names the
/// vblank the frame is *for* rather than the one it came from puts it in the future, which is not
/// an age at all. `aged=0` means no measurement, not a zero one.
///
/// `enc_us` needs no present stamp: it is submit to the access unit's last chunk, the span a
/// bypass pool holds its next acquire for. `over` counts the ones longer than a frame period.
/// `shed` counts composed frames left out to hold the stream rate on a faster panel.
/// `gap_change_us` is how far each composed frame's gap moved from the one before it, on the
/// compositor's stamps (`raw`) and on the stamps the access units carry (`pts`): near zero is an
/// even cadence, a panel tick is jitter the stamps pass on.
struct Report {
    hz: u64,
    period_us: u64,
    since: u64,
    n: u64,
    aged: u64,
    sum_us: u64,
    max_us: u64,
    submits: u64,
    parks: u64,
    shed: u64,
    enc_n: u64,
    enc_sum_us: u64,
    enc_max_us: u64,
    enc_over: u64,
    gap_change: GapChange,
}

impl Report {
    const EVERY_MS: u64 = 10_000;

    fn new(hz: u64, period_us: u64) -> Self {
        Self {
            hz,
            period_us,
            since: 0,
            n: 0,
            aged: 0,
            sum_us: 0,
            max_us: 0,
            submits: 0,
            parks: 0,
            shed: 0,
            enc_n: 0,
            enc_sum_us: 0,
            enc_max_us: 0,
            enc_over: 0,
            gap_change: GapChange::default(),
        }
    }

    /// One composed frame's compositor stamp and the stamp its access unit carries.
    fn note_stamps(&mut self, raw: u64, pts: u64) {
        self.gap_change.note(raw, pts);
    }

    /// One whole access unit, submitted at `submitted` and out at `now`.
    fn note_encoded(&mut self, submitted: u64, now: u64) {
        let us = now.saturating_sub(submitted) * 1_000_000 / self.hz;
        self.enc_n += 1;
        self.enc_sum_us += us;
        self.enc_max_us = self.enc_max_us.max(us);
        self.enc_over += u64::from(us > self.period_us);
    }

    /// One published chunk, stamped `qpc` at compose and `now` at publish.
    fn note_publish(&mut self, qpc: u64, now: u64) {
        if self.since == 0 {
            self.since = now;
        }
        self.n += 1;
        if qpc != 0 && now > qpc {
            let us = (now - qpc) * 1_000_000 / self.hz;
            self.aged += 1;
            self.sum_us += us;
            self.max_us = self.max_us.max(us);
        }
        let window_ms = (now - self.since) * 1_000 / self.hz;
        if window_ms < Self::EVERY_MS {
            return;
        }
        let (raw, pts) = self.gap_change.take();
        tracing::info!(
            "drive: win_ms={window_ms} published={} submits={} parks={} shed={} aged={} au_age_us mean={} max={} enc_us mean={} max={} over={} gap_change_us raw={} pts={}",
            self.n,
            self.submits,
            self.parks,
            self.shed,
            self.aged,
            self.sum_us / self.aged.max(1),
            self.max_us,
            self.enc_sum_us / self.enc_n.max(1),
            self.enc_max_us,
            self.enc_over,
            raw * 1_000_000 / self.hz,
            pts * 1_000_000 / self.hz
        );
        // The stamps carry across windows; only the counts start over.
        let gap_change = core::mem::take(&mut self.gap_change);
        *self = Self::new(self.hz, self.period_us);
        self.gap_change = gap_change;
        self.since = now;
    }
}
