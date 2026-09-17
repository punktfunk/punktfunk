//! The encode thread's steady state ([`Drive`]): pool slot → `submit` → `poll` → heap + slot
//! table → `latest` + event, with back-pressure taken on pool slots and the wedge state word
//! kept for the host's classifier.
//!
//! The loop is frame-driven, not timed: submit while there is room and a frame, publish while an
//! access unit is owed and ready, and otherwise park once on `{stop, pool, the backend's
//! completion event, a high-resolution timer}`. So an AU leaves the encoder as soon as it is
//! encoded rather than when the next frame is submitted, and a desktop that goes still still
//! publishes its last picture.

use std::collections::VecDeque;
use std::ffi::c_void;
use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pf_driver_proto::encode::FrameToken;
use pf_driver_proto::encode::au::{self, AuHeader, AuSlot, HeapRing};
use pf_encode_win::{AuChunk, Encoder};
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, INFINITE, SetWaitableTimer,
    TIMER_ALL_ACCESS, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::core::PCWSTR;

use super::pool::{Offer, Pool};
use super::section::{Ctl, EncodeSession};
use super::thread::{hdr_meta, qpc_frequency, qpc_now, qpc_to_ns};
use crate::worker::OwnedHandle;

/// Submits allowed ahead of the oldest AU — the host's pipeline depth. Also what the pool
/// guarantees a backend that encodes an input texture where it lies: a slot handed to the
/// encoder sits in `encoding` and no drain pass can take it back until the AU is published.
pub(crate) const MAX_INFLIGHT: usize = 2;
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

/// G3 fault injection: encode this many frames, then never return from the encode work
/// (`PFVD_ENCODE_BLOCK_AFTER`, a frame count; unset or unparsable disables it). Read once per
/// encoder open, so a `reset` reopens blocked again until the knob is cleared.
#[cfg(feature = "encode-probe")]
fn block_after() -> Option<u64> {
    crate::log::knob("PFVD_ENCODE_BLOCK_AFTER")?
        .trim()
        .parse()
        .ok()
}

/// Park forever, holding the pool slot and the session, once `encoded` passes the knob. Nothing
/// unparks this thread: the drain worker has to keep composing against a thread that is gone.
#[cfg(feature = "encode-probe")]
fn block_if_armed(limit: Option<u64>, encoded: u64) {
    if limit.is_some_and(|n| encoded > n) {
        dbglog!("[pf-vd] encode: PFVD_ENCODE_BLOCK_AFTER — wedging at frame {encoded}");
        loop {
            std::thread::park();
        }
    }
}

fn stop_signalled(stop: HANDLE) -> bool {
    // SAFETY: `stop` is the worker's stop event, alive until the worker joins or leaks.
    unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
}

impl<'a> Drive<'a> {
    pub fn new(
        enc: Box<dyn Encoder>,
        pool: &'a Pool,
        session: &'a EncodeSession,
        stop: HANDLE,
        live: &'a AtomicBool,
        fps: u32,
        opened_kbps: u32,
    ) -> Self {
        let (heap_offset, heap_bytes) = session.section.heap();
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
            qpc_hz: qpc_frequency(),
            frame_interval: Duration::from_micros(1_000_000 / u64::from(fps.max(1))),
            last_frame: None,
            owed_since: None,
            ready_latch: false,
            timer: None,
            report: Report::new(qpc_frequency()),
            applied_kbps: opened_kbps,
            state: au::ENCODER_OPEN,
            stop,
            live,
            #[cfg(feature = "encode-probe")]
            block_after: block_after(),
            #[cfg(feature = "encode-probe")]
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

/// The steady state: pool slot → `submit` → `poll` → heap + slot table → `latest` + event.
///
/// Idle desktop: nothing is re-encoded at cadence — a pool with no new frame means no AU, and
/// the host, which reads `source_seq` standing still with `drain_heartbeat_qpc` moving, kicks
/// a compose when it wants one. The stash rule (§2.2) covers only the first frame.
pub struct Drive<'a> {
    enc: Box<dyn Encoder>,
    pool: &'a Pool,
    session: &'a EncodeSession,
    ring: HeapRing,
    /// Next wire index to stamp; every published AU takes exactly one, a dropped AU none.
    wire_seq: u32,
    /// Publish-token sequence, per session thread.
    publish_seq: u32,
    /// `(slot, qpc, source_seq, qpc_submit)` of every frame whose AU is owed, in submit order.
    inflight: VecDeque<(usize, u64, u64, u64)>,
    /// A partially drained AU must finish through `poll_chunk`.
    mid_au: bool,
    /// The AU in progress could not be placed; its remaining chunks are dropped too.
    dropping_au: bool,
    /// A chunk of the AU in progress reached the section, so its wire index is spent even if
    /// the tail is dropped — the reader closes it as a gap, never splices the next AU on.
    au_published: bool,
    /// Consecutive `submit` failures; see [`MAX_SUBMIT_FAILURES`].
    submit_failures: u32,
    /// A keyframe was asked for; if nothing composes, re-encode the stash instead of waiting.
    want_republish: bool,
    qpc_hz: u64,
    /// The display's frame period — the gap a cursor-only re-encode may fill.
    frame_interval: Duration,
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
    #[cfg(feature = "encode-probe")]
    block_after: Option<u64>,
    #[cfg(feature = "encode-probe")]
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
            for (slot, ..) in self.inflight.drain(..) {
                self.pool.release(slot);
            }
            self.pool.set_live(false);
        }
    }

    /// The `ENCODE_CTL` ops queued since the last frame, in order. An RFI the backend cannot
    /// honour becomes a keyframe request, the host's own fallback.
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
                        self.want_republish = true;
                    }
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
                        dbglog!("[pf-vd] encode: backend declined bitrate {kbps} kbps in place");
                    }
                    // Declined or clamped, the host must see what is encoding: the ctl has no
                    // reply, so this stamp is the only thing that can contradict the ask.
                    self.session.section.store_u32(
                        offset_of!(AuHeader, applied_bitrate_kbps),
                        self.applied_kbps,
                    );
                }
                Ctl::SetHdrMeta(bytes) => self.enc.set_hdr_meta(Some(hdr_meta(&bytes))),
                Ctl::Flush => {
                    if let Err(e) = self.enc.flush() {
                        dbglog!("[pf-vd] encode: flush failed: {e:#}");
                    }
                    self.drain_all();
                }
            }
        }
    }

    /// The next frame to submit: a composed one, else the stash for a keyframe request nothing
    /// composed for, else a cursor-only re-encode. The request survives a turn that found the
    /// stash slot busy — the AU owed on it is about to free it. Every frame taken stamps
    /// `last_frame`: that is the clock the cursor-only cap runs on.
    fn take_next(&mut self) -> Option<(usize, u64, u64)> {
        let next = self
            .pool
            .take_full()
            .or_else(|| self.want_republish.then(|| self.pool.republish()).flatten())
            .or_else(|| self.cursor_frame());
        if next.is_some() {
            self.want_republish = false;
            self.last_frame = Some(Instant::now());
        }
        next
    }

    /// Hand a slot back unless this thread was detached: its successor reclaimed every slot
    /// (`reset`), and a release here would free one that successor is encoding.
    fn release_if_live(&self, slot: usize) {
        if self.live.load(Ordering::Acquire) {
            self.pool.release(slot);
        }
    }

    /// Submit one pool slot, or drop it where dropping is free. `false` means the backend has
    /// failed [`MAX_SUBMIT_FAILURES`] times running and the thread leaves.
    fn submit_one(&mut self, (slot, qpc, seq): (usize, u64, u64)) -> bool {
        // Back-pressure lands here: no free AU slot, no submit.
        if !self.section_has_free() {
            self.release_if_live(slot);
            self.count_drop();
            return true;
        }
        let pts = qpc_to_ns(if qpc == 0 { qpc_now() } else { qpc }, self.qpc_hz);
        let frame = match self.pool.frame(slot, pts) {
            Ok(f) => f,
            Err(_) => {
                self.release_if_live(slot);
                self.count_drop();
                return true;
            }
        };
        #[cfg(feature = "encode-probe")]
        {
            self.encoded += 1;
            block_if_armed(self.block_after, self.encoded);
        }
        let index = self.wire_seq.wrapping_add(self.inflight.len() as u32);
        let submitted = qpc_now();
        if let Err(e) = self.enc.submit_indexed(&frame, index) {
            dbglog!("[pf-vd] encode: submit failed: {e:#}");
            self.release_if_live(slot);
            self.set_state(au::ENCODER_WEDGED);
            // A lazy backend re-runs its whole bring-up per submit; stop retrying at the compose
            // rate and leave the session threadless for the host's reset rung.
            self.submit_failures += 1;
            if self.submit_failures >= MAX_SUBMIT_FAILURES {
                dbglog!("[pf-vd] encode: {MAX_SUBMIT_FAILURES} submits failed in a row — leaving");
                return false;
            }
            return true;
        }
        self.submit_failures = 0;
        self.report.submits += 1;
        self.inflight.push_back((slot, qpc, seq, submitted));
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
                dbglog!("[pf-vd] encode: poll failed: {e:#}");
                self.set_state(au::ENCODER_WEDGED);
                // A detached thread's slots were reclaimed by its successor; releasing one
                // here would free a slot that successor is encoding.
                if let Some((slot, ..)) = self.inflight.pop_front()
                    && self.live.load(Ordering::Acquire)
                {
                    self.pool.release(slot);
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

    /// When the loop must wake without a signal: what is left of the period since the last
    /// frame when a cursor move is pending, or the re-entry cadence of a backend that owes an
    /// AU and signals nothing. `None` = park on the handles alone.
    fn timer_due(&self) -> Option<Duration> {
        let cursor = self.pool.cursor_pending().then(|| {
            self.last_frame
                .map(|t| self.frame_interval.saturating_sub(t.elapsed()))
                .unwrap_or_default()
        });
        let poll = (!self.inflight.is_empty() && self.enc.ready_event().is_none())
            .then_some(NO_EVENT_POLL);
        match (cursor, poll) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
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
            self.timer = made.ok().map(|h| unsafe { OwnedHandle::from_raw(h) });
        }
        let Some(timer) = &self.timer else {
            return false;
        };
        // Negative is relative, in 100 ns units; one unit minimum so a due-now timer still fires.
        let due_100ns = -((due.as_nanos() / 100).max(1).min(i64::MAX as u128) as i64);
        // SAFETY: our own timer handle; `due_100ns` is a valid local read during the call, and
        // there is no completion routine or resume.
        unsafe { SetWaitableTimer(timer.as_raw(), &due_100ns, 0, None, None, false).is_ok() }
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
        handles[n] = self.pool.event();
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
            handles[n] = timer.as_raw();
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
        if let Offer::Dropped(n) = self.pool.drop_one() {
            self.session
                .section
                .store_u64(offset_of!(AuHeader, dropped_total), n);
        }
    }

    /// One chunk of the oldest in-flight AU: published, or dropped with the rest of its AU. A
    /// detached thread returning from its wedge touches neither the pool nor the section.
    fn on_chunk(&mut self, chunk: AuChunk) {
        let Some(&(slot, qpc, seq, submitted)) = self.inflight.front() else {
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
            if self.publish(&chunk, qpc, seq, submitted) {
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
                    dbglog!(
                        "[pf-vd] encode: first AU published ({} B)",
                        chunk.data.len()
                    );
                }
            }
            self.inflight.pop_front();
            self.pool.release(slot);
            self.dropping_au = false;
        }
    }

    /// Heap bytes, slot record, `latest`, event — in that order. `false` when no placement
    /// came free within [`SLOT_WAIT`]. The record carries the frame's present, submit and
    /// publish QPC, which is how the host splits its age.
    fn publish(&mut self, chunk: &AuChunk, qpc: u64, seq: u64, submitted: u64) -> bool {
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
                source_seq: seq as u32,
                qpc_pts: qpc,
                flags,
                state: au::PUBLISHED,
                qpc_submit: submitted,
                qpc_published: now,
            },
        );
        self.publish_seq = self.publish_seq.wrapping_add(1);
        section.store_u64(offset_of!(AuHeader, last_au_qpc), now);
        self.report.note_publish(qpc, now);
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
struct Report {
    hz: u64,
    since: u64,
    n: u64,
    aged: u64,
    sum_us: u64,
    max_us: u64,
    submits: u64,
    parks: u64,
}

impl Report {
    const EVERY_MS: u64 = 10_000;

    fn new(hz: u64) -> Self {
        Self {
            hz,
            since: 0,
            n: 0,
            aged: 0,
            sum_us: 0,
            max_us: 0,
            submits: 0,
            parks: 0,
        }
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
        dbglog!(
            "[pf-vd] drive: win_ms={window_ms} published={} submits={} parks={} aged={} au_age_us mean={} max={}",
            self.n,
            self.submits,
            self.parks,
            self.aged,
            self.sum_us / self.aged.max(1),
            self.max_us
        );
        *self = Self::new(self.hz);
        self.since = now;
    }
}
