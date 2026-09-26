//! Presentation intents the run loop composes: store, latch clock, gate, source pacer.
//!
//! * [`FrameStore`] — newest-wins (`capacity == 0`) or smoothing FIFO with preroll
//!   (`capacity 1..=3`). Same contract as the Apple and Android presenters.
//! * [`LatchClock`] — panel latch grid from `VK_KHR_present_wait` on-glass stamps.
//!   A reported refresh is a mode claim; VRR makes it unusable. Without present-wait
//!   the grid is last submit plus the mode period.
//! * [`PresentGate`] — at most one undisplayed FIFO present. MAILBOX cannot queue.
//! * [`SourcePacer`] — smoothness plays on the source [`CadenceClock`], not arrival.
//!
//! Arithmetic is `CLOCK_REALTIME` ns (`pf_client_core::session::now_ns`, including
//! `DecodedFrame::decoded_ns`) so the cadence clock needs no domain conversion. The
//! run loop owns Vulkan and the clocks; this module is state plus arithmetic. Tests
//! pin the contracts; [`punktfunk_core::phase`] owns the shared grid and cadence math.

use punktfunk_core::phase::{CadenceClock, CadenceHealth, CadenceTuning};
use std::collections::VecDeque;

/// 100 ms: an occluded or wedged present is lost; the gate force-opens.
const STALE_REOPEN_NS: u64 = 100_000_000;

/// Slot-pick margin: 0.5 ms step, 2.5 ms cap. Starts at 0 — a fixed lead is display tax.
pub(crate) const MARGIN_STEP_NS: u64 = 500_000;
pub(crate) const MARGIN_MAX_NS: u64 = 2_500_000;

/// Newest-wins (`capacity == 0`: `submit` replaces, `take` clears) or smoothing FIFO
/// (`capacity 1..=3`: preroll-to-capacity, drop-oldest overflow). Underflow after
/// preroll re-arms; the previous frame stays on glass while headroom rebuilds.
pub(crate) struct FrameStore<T> {
    capacity: usize,
    frames: VecDeque<T>,
    prerolled: bool,
    /// Newest-wins displacements; not a fault.
    replaced: u32,
    overflow_drops: u32,
    underflows: u32,
}

impl<T> FrameStore<T> {
    pub(crate) fn new(capacity: usize) -> FrameStore<T> {
        FrameStore {
            capacity,
            frames: VecDeque::with_capacity(capacity.max(1) + 1),
            prerolled: false,
            replaced: 0,
            overflow_drops: 0,
            underflows: 0,
        }
    }

    pub(crate) fn is_smoothing(&self) -> bool {
        self.capacity > 0
    }

    pub(crate) fn submit(&mut self, f: T) {
        if self.capacity == 0 {
            if self.frames.pop_front().is_some() {
                self.replaced += 1;
            }
            self.frames.push_back(f);
        } else {
            self.frames.push_back(f);
            // Drop-oldest bounds latency; also trims a put_back that left capacity+1.
            while self.frames.len() > self.capacity {
                self.frames.pop_front();
                self.overflow_drops += 1;
            }
        }
    }

    /// FIFO: vend the front once `due` is true. Newest-wins never calls `due`: a frame
    /// is due the instant it exists, so the cadence clock stays off the latency path.
    pub(crate) fn take(&mut self, due: impl FnOnce(&T) -> bool) -> Option<T> {
        if self.capacity == 0 {
            return self.frames.pop_front();
        }
        if !self.prerolled {
            // Without preroll a steady stream drains on arrival and never builds jitter headroom.
            if self.frames.len() < self.capacity {
                return None;
            }
            self.prerolled = true;
        }
        let Some(f) = self.frames.front() else {
            self.underflows += 1;
            self.prerolled = false;
            return None;
        };
        // Held for its slot, not dry: no counter, preroll stays. An underflow
        // count here would re-arm preroll on every well-paced frame.
        if !due(f) {
            return None;
        }
        self.frames.pop_front()
    }

    /// Next `take` candidate. The run loop waits until this frame is due.
    pub(crate) fn front(&self) -> Option<&T> {
        self.frames.front()
    }

    /// Unpresented frame (gate closed / present failed). Newest-wins reinserts only
    /// into an empty slot; FIFO puts it at the front (it is the oldest).
    pub(crate) fn put_back(&mut self, f: T) {
        if self.capacity == 0 {
            if self.frames.is_empty() {
                self.frames.push_back(f);
            }
        } else {
            self.frames.push_front(f);
        }
    }

    /// Collapse to newest-wins. PyroWave plane-ring retirement assumes a depth-2
    /// newest-wins hand-off; all-intra frames make buffering pointless.
    #[cfg(feature = "pyrowave")]
    pub(crate) fn force_latency(&mut self) {
        if self.capacity == 0 {
            return;
        }
        self.capacity = 0;
        self.prerolled = false;
        while self.frames.len() > 1 {
            self.frames.pop_front();
        }
    }

    pub(crate) fn take_counters(&mut self) -> (u32, u32, u32) {
        let c = (self.replaced, self.overflow_drops, self.underflows);
        self.replaced = 0;
        self.overflow_drops = 0;
        self.underflows = 0;
        c
    }
}

/// Panel latch grid: last on-glass instant plus the learned period, for slot targeting.
///
/// Period is the shared [`punktfunk_core::phase::PanelGrid`], fed the median of each
/// [`GRID_OBSERVE_EVERY`] spacings, snapped onto the mode period when it lies within
/// [`grid_snap_ns`] of it. Present-wait stamps carry milliseconds of wake jitter; the
/// min of a run reads that as a faster panel and every slot it predicts is phantom.
/// A stream below panel rate lands at k×period and stays as measured: any multiple
/// is a real latch. Same grid the host-facing `LatchGrid` publish reads, so the
/// phase-lock report and the local scheduler cannot disagree.
pub(crate) struct LatchClock {
    anchor_ns: u64,
    /// Previous stamp, kept across calls. The run loop drains one present-wait sample
    /// per pass; spacings only within a batch (`windows(2)`) observe nothing.
    last_ns: u64,
    /// Spacings since the last grid handoff.
    pending: Vec<u64>,
    grid: punktfunk_core::phase::PanelGrid,
    fallback_period_ns: u64,
}

/// Spacings per [`PanelGrid`](punktfunk_core::phase::PanelGrid) handoff. 16 ≈ a
/// mode change within a second at 16+ fps.
const GRID_OBSERVE_EVERY: usize = 16;

/// 10 % of the mode period, at least 1 ms: a median of 16 jittered spacings sits well
/// inside; a wrong mode claim (120 for a 60 Hz panel) sits well outside.
fn grid_snap_ns(period_ns: u64) -> u64 {
    (period_ns / 10).max(1_000_000)
}

impl LatchClock {
    pub(crate) fn new(refresh_hz: u32) -> LatchClock {
        LatchClock {
            anchor_ns: 0,
            last_ns: 0,
            pending: Vec::with_capacity(GRID_OBSERVE_EVERY),
            grid: punktfunk_core::phase::PanelGrid::seeded(refresh_hz as i32),
            fallback_period_ns: 1_000_000_000 / u64::from(refresh_hz.max(1)),
        }
    }

    /// Fold on-glass stamps (ascending). Spacing is against the previous stamp,
    /// whatever the batch size, so a one-sample-per-pass drain still feeds the learner.
    ///
    /// `own_cadence`: the presents were scheduled on this clock's grid (the smoothing
    /// drain). Then a spacing of whole refreshes is the drain skipping slots, not a slower
    /// panel, and snaps to the mode — learning it fed back into a drain every second
    /// slot for good. Arrival-driven presents keep teaching a panel slower than its
    /// mode claim.
    pub(crate) fn note_batch(&mut self, stamps: &[u64], own_cadence: bool) {
        for &s in stamps {
            if self.last_ns != 0 && s > self.last_ns {
                let d = s - self.last_ns;
                // < 1 ms apart = a queued pair, not a grid step.
                if d > 1_000_000 {
                    self.pending.push(d);
                    if self.pending.len() >= GRID_OBSERVE_EVERY {
                        self.pending.sort_unstable();
                        let median = self.pending[self.pending.len() / 2];
                        let mode = self.fallback_period_ns;
                        let steps = if own_cadence {
                            ((median + mode / 2) / mode).max(1)
                        } else {
                            1
                        };
                        let spacing = if median.abs_diff(steps * mode) <= grid_snap_ns(mode) {
                            mode
                        } else {
                            median
                        };
                        self.grid.observe(spacing as i64);
                        self.pending.clear();
                    }
                }
            }
            self.last_ns = s;
        }
        if let Some(&last) = stamps.last() {
            self.anchor_ns = last;
        }
    }

    pub(crate) fn period_ns(&self) -> u64 {
        let learned = self.grid.period_ns();
        if learned > 0 {
            learned as u64
        } else {
            self.fallback_period_ns
        }
    }

    pub(crate) fn anchor_ns(&self) -> u64 {
        self.anchor_ns
    }

    /// First predicted latch strictly after `after_ns` (`anchor + k·period`). No
    /// anchor yet: one period out, so callers still get a usable deadline.
    pub(crate) fn next_slot_after(&self, after_ns: u64) -> u64 {
        let p = self.period_ns();
        if self.anchor_ns == 0 || after_ns < self.anchor_ns {
            return after_ns.saturating_add(p);
        }
        let k = (after_ns - self.anchor_ns) / p + 1;
        self.anchor_ns + k * p
    }
}

/// Panel refresh regime, measured from on-glass stamps. No portable query exists.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Cadence {
    /// Not enough evidence yet — say nothing rather than guess.
    #[default]
    Unknown,
    /// Stamps land on multiples of the panel period.
    Fixed,
    /// Stamps track our present spacing: variable refresh is live.
    Variable,
}

impl Cadence {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Cadence::Unknown => "",
            Cadence::Fixed => "no",
            Cadence::Variable => "yes",
        }
    }
}

/// Variable-refresh probe: measured, never queried. No portable query exists
/// (SDL, Wayland, and Vulkan omit adaptive-sync state), and a reported rate is a
/// mode claim, not the panel.
///
/// Discriminator is quantization. A fixed panel lands stamps on the vblank grid
/// (spacing ≈ k×period for whole k, including a slower stream). Live VRR refreshes
/// when we present, so spacing follows our cadence. Distance of each delta to the
/// nearest period multiple: tight ⇒ Fixed, consistently off ⇒ Variable. A stream
/// at exact panel rate is indistinguishable (delta ≈ period); VRR has nothing to do.
pub(crate) struct CadenceProbe {
    /// Off-grid distances as a fraction of the period, in thousandths.
    off_grid_milli: Vec<u32>,
    /// Previous stamp, kept across calls: the live drain hands one sample at a time.
    last_ns: u64,
    /// Last round's reading; a verdict publishes only after [`CADENCE_STABLE_ROUNDS`] agree.
    candidate: Cadence,
    agree_rounds: u8,
    verdict: Cadence,
}

/// Enough deltas to distinguish jitter from a real off-grid cadence.
const CADENCE_MIN_SAMPLES: usize = 24;
/// Consecutive agreeing rounds before a verdict is published. On-glass stamps
/// are the compositor's release; occlusion smears spacings like live VRR. Two
/// rounds, and no evidence from a distressed window ([`CadenceProbe::note`]'s
/// `healthy` flag).
const CADENCE_STABLE_ROUNDS: u8 = 2;
/// Median off-grid distance under this fraction of a period reads as grid-locked.
/// Stamps carry wait-then-read-clock jitter; 150‰ is loose — the two regimes
/// differ by far more.
const CADENCE_FIXED_MILLI: u32 = 150;

impl CadenceProbe {
    pub(crate) fn new() -> CadenceProbe {
        CadenceProbe {
            off_grid_milli: Vec::with_capacity(64),
            last_ns: 0,
            candidate: Cadence::Unknown,
            agree_rounds: 0,
            verdict: Cadence::Unknown,
        }
    }

    /// Fold on-glass stamps against the learned panel period. Spacing is against the
    /// previous stamp, whatever the batch size.
    ///
    /// `healthy` is "presents were flowing" (no stale force-opens). A distressed
    /// pipeline smears spacings for non-panel reasons; evidence is dropped but
    /// `last_ns` still advances so the timeline stays continuous.
    pub(crate) fn note(&mut self, stamps: &[u64], period_ns: u64, healthy: bool) {
        if period_ns == 0 || !healthy {
            self.last_ns = stamps.last().copied().unwrap_or(self.last_ns);
            return;
        }
        for &s in stamps {
            let prev = std::mem::replace(&mut self.last_ns, s);
            if prev == 0 || s <= prev {
                continue;
            }
            let delta = s - prev;
            let rem = delta % period_ns;
            // Distance to the nearest multiple: a delta just under k×period is on the
            // grid, not a whole period away from k-1.
            let off = rem.min(period_ns - rem);
            self.off_grid_milli
                .push((off.saturating_mul(1000) / period_ns) as u32);
            // A round closes on sample count, inside the loop — not once per call.
            // Per-call evaluation would make the verdict depend on how the caller batches.
            self.close_round_if_ready();
        }
    }

    fn close_round_if_ready(&mut self) {
        if self.off_grid_milli.len() >= CADENCE_MIN_SAMPLES {
            self.off_grid_milli.sort_unstable();
            let median = self.off_grid_milli[self.off_grid_milli.len() / 2];
            let round = if median <= CADENCE_FIXED_MILLI {
                Cadence::Fixed
            } else {
                Cadence::Variable
            };
            if round == self.candidate {
                self.agree_rounds = self.agree_rounds.saturating_add(1);
            } else {
                self.candidate = round;
                self.agree_rounds = 1;
            }
            if self.agree_rounds >= CADENCE_STABLE_ROUNDS {
                self.verdict = round;
            }
            self.off_grid_milli.clear();
        }
    }

    pub(crate) fn verdict(&self) -> Cadence {
        self.verdict
    }

    /// A mode switch or display change invalidates the evidence.
    pub(crate) fn reset(&mut self) {
        self.off_grid_milli.clear();
        self.last_ns = 0;
        self.candidate = Cadence::Unknown;
        self.agree_rounds = 0;
        self.verdict = Cadence::Unknown;
    }
}

/// Plays frames on the source cadence: a [`CadenceClock`] plus the two client
/// policies — which intent applies, and which cushion the measured refresh asks for.
///
/// The loop smooths the OFFSET, never the timestamps: genuine source-cadence
/// variation passes through to the due time. More-even due times than the source
/// would be a bug.
pub(crate) struct SourcePacer {
    clock: CadenceClock,
    /// Last [`follow`](Self::follow) verdict was [`Cadence::Variable`]: free-running tuning.
    free_running: bool,
}

impl SourcePacer {
    pub(crate) fn new() -> SourcePacer {
        SourcePacer {
            clock: CadenceClock::new(CadenceTuning::snapping()),
            free_running: false,
        }
    }

    /// Fold an arriving frame and return when it is due, in `ready_ns`'s clock domain.
    ///
    /// `None` under latency: arrival-driven, so the loop never carries an estimate
    /// built from samples it then ignored. Called at submit, not take: dropped frames
    /// are part of the arrival process; folding only survivors hides the jitter.
    pub(crate) fn due_ns(
        &mut self,
        smoothing: bool,
        src_pts_ns: u64,
        ready_ns: u64,
        frame_interval_ns: i64,
    ) -> Option<i64> {
        smoothing.then(|| {
            self.clock
                .due_ns(src_pts_ns, ready_ns as i64, frame_interval_ns)
        })
    }

    /// Follow the measured refresh verdict. Snapping onto the latch grid carries
    /// roughly half a refresh of slack; presenting at the due time carries none, so
    /// the cushions differ. Re-tuning re-anchors (tuning is fixed at construction),
    /// so this keys off the probe's published verdict, not a per-window reading.
    pub(crate) fn follow(&mut self, verdict: Cadence) {
        let free = verdict == Cadence::Variable;
        if free != self.free_running {
            self.free_running = free;
            self.clock = CadenceClock::new(if free {
                CadenceTuning::free_running()
            } else {
                CadenceTuning::snapping()
            });
        }
    }

    /// Present at the due time instead of snapping to the latch grid. True only
    /// where variable refresh is measured live — the panel refreshes when we present.
    pub(crate) fn free_running(&self) -> bool {
        self.free_running
    }

    /// Re-anchor on the next frame (display change, accepted mode switch). Measured
    /// jitter survives: it describes the link, not the stream.
    pub(crate) fn reset(&mut self) {
        self.clock.reset();
    }

    pub(crate) fn health(&self) -> CadenceHealth {
        self.clock.health()
    }
}

/// FIFO glass budget: at most one undisplayed present in flight, counted by the
/// present-wait waiter. MAILBOX/IMMEDIATE cannot queue; without present-wait there
/// is nothing to count and arrival pacing is unchanged.
#[derive(Default)]
pub(crate) struct PresentGate {
    /// Submit stamp of the newest tracked present; 0 = none yet.
    last_present_ns: u64,
    gated: u32,
    forced: u32,
}

impl PresentGate {
    /// Open when nothing undisplayed is in flight. A stale in-flight present
    /// force-opens after [`STALE_REOPEN_NS`] (occlusion, wedged compositor).
    pub(crate) fn open(&mut self, outstanding: usize, now_ns: u64) -> bool {
        if outstanding == 0 {
            return true;
        }
        if self.last_present_ns != 0
            && now_ns.saturating_sub(self.last_present_ns) > STALE_REOPEN_NS
        {
            self.forced += 1;
            return true;
        }
        self.gated += 1;
        false
    }

    pub(crate) fn note_present(&mut self, now_ns: u64) {
        self.last_present_ns = now_ns;
    }

    pub(crate) fn take_counters(&mut self) -> (u32, u32) {
        let c = (self.gated, self.forced);
        self.gated = 0;
        self.forced = 0;
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_wins_replaces_and_putback_never_clobbers() {
        let mut s: FrameStore<u32> = FrameStore::new(0);
        assert!(!s.is_smoothing());
        assert_eq!(s.take(|_| true), None);
        s.submit(1);
        s.submit(2);
        s.submit(3);
        assert_eq!(s.take(|_| true), Some(3), "only the newest survives");
        assert_eq!(s.take(|_| true), None);
        s.submit(4);
        let f = s.take(|_| true).unwrap();
        s.put_back(f);
        assert_eq!(s.take(|_| true), Some(4));
        let f = s.take(|_| true);
        assert_eq!(f, None);
        s.submit(5);
        let f = s.take(|_| true).unwrap();
        s.submit(6);
        s.put_back(f);
        assert_eq!(s.take(|_| true), Some(6));
        assert_eq!(
            s.take_counters(),
            (2, 0, 0),
            "two displacements, no fifo counters"
        );
    }

    #[test]
    fn fifo_prerolls_overflows_oldest_and_rearms_on_dry() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        assert!(s.is_smoothing());
        s.submit(1);
        assert_eq!(
            s.take(|_| true),
            None,
            "prerolling: below capacity, nothing vends"
        );
        s.submit(2);
        assert_eq!(s.take(|_| true), Some(1), "preroll reached — FIFO order");
        assert_eq!(
            s.take(|_| true),
            Some(2),
            "once prerolled the buffer drains normally"
        );
        assert_eq!(s.take(|_| true), None);
        s.submit(3);
        assert_eq!(s.take(|_| true), None, "re-armed preroll holds again");
        s.submit(4);
        assert_eq!(s.take(|_| true), Some(3));
        s.submit(5);
        s.submit(6);
        s.submit(7);
        assert_eq!(s.take(|_| true), Some(6));
        assert_eq!(s.take(|_| true), Some(7));
        let (replaced, drops, dry) = s.take_counters();
        assert_eq!(replaced, 0);
        assert_eq!(drops, 2, "6 evicted 4, 7 evicted 5");
        assert_eq!(dry, 1);
    }

    #[test]
    fn fifo_putback_restores_order() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        s.submit(1);
        s.submit(2);
        let f = s.take(|_| true).unwrap();
        s.put_back(f);
        assert_eq!(
            s.take(|_| true),
            Some(1),
            "the put-back frame is still first"
        );
    }

    /// Held-for-due is not dry: counting it as underflow re-arms preroll on every
    /// well-paced frame.
    #[test]
    fn a_frame_held_for_its_due_time_is_not_an_underflow() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        s.submit(10);
        s.submit(20);
        assert_eq!(s.take(|_| false), None, "prerolled, but nothing is due yet");
        assert_eq!(s.take(|&v| v >= 10), Some(10));
        assert_eq!(s.take(|&v| v >= 30), None, "20 is not due yet either");
        assert_eq!(s.take(|_| true), Some(20));
        assert_eq!(s.take(|_| true), None);
        s.submit(30);
        assert_eq!(s.take(|_| true), None, "re-armed preroll holds again");
        assert_eq!(
            s.take_counters(),
            (0, 0, 1),
            "one dry, nothing from the holds"
        );
    }

    /// Newest-wins never consults `due`, so a caller-supplied cadence clock cannot
    /// gate the latency intent.
    #[test]
    fn the_latency_store_vends_without_ever_asking_a_due_time() {
        let mut s: FrameStore<u32> = FrameStore::new(0);
        s.submit(7);
        let mut asked = false;
        let got = s.take(|_| {
            asked = true;
            false
        });
        assert_eq!(
            got,
            Some(7),
            "arrival-driven: the frame goes out regardless"
        );
        assert!(!asked, "…and the due time was never consulted");
    }

    #[cfg(feature = "pyrowave")]
    #[test]
    fn force_latency_collapses_to_one_slot() {
        let mut s: FrameStore<u32> = FrameStore::new(3);
        s.submit(1);
        s.submit(2);
        s.submit(3);
        s.force_latency();
        assert!(!s.is_smoothing());
        assert_eq!(
            s.take(|_| true),
            Some(3),
            "only the newest survives the collapse"
        );
        s.submit(4);
        s.submit(5);
        assert_eq!(s.take(|_| true), Some(5));
    }

    /// Wake jitter on the stamps must not read as a faster panel: the batch median
    /// snaps onto the mode grid. One queued-late pair in a batch cannot take it over.
    #[test]
    fn latch_clock_holds_the_mode_grid_through_jitter() {
        const P: u64 = 16_666_666;
        let mut c = LatchClock::new(60);
        let mut t = 1_000_000_000u64;
        for i in 0..(GRID_OBSERVE_EVERY * 8) {
            // ±1.5 ms of wake jitter, the kind a waiter thread stamps on Windows.
            let jitter: i64 = if i % 2 == 0 { 1_500_000 } else { -1_500_000 };
            t += (P as i64 + jitter) as u64;
            if i % GRID_OBSERVE_EVERY == 3 {
                // Two presents retiring together, 2.2 ms apart.
                c.note_batch(&[t, t + 2_200_000], false);
                t += 2_200_000;
            } else {
                c.note_batch(&[t], false);
            }
        }
        assert_eq!(c.period_ns(), P, "jitter is not a faster panel");

        // Arrival-driven presents every second refresh: a real 2×P latch, kept.
        let mut half = LatchClock::new(60);
        let mut t = 1_000_000_000u64;
        for _ in 0..(GRID_OBSERVE_EVERY * 9) {
            t += 2 * P;
            half.note_batch(&[t], false);
        }
        assert_eq!(half.period_ns(), 2 * P);

        // The smoothing drain presenting every second slot must not teach a 30 Hz panel:
        // that grid fed the drain back into every second slot for good.
        let mut drain = LatchClock::new(60);
        let mut t = 1_000_000_000u64;
        for _ in 0..(GRID_OBSERVE_EVERY * 9) {
            t += 2 * P;
            drain.note_batch(&[t], true);
        }
        assert_eq!(drain.period_ns(), P);
    }

    /// Learns the batch median, anchors on the newest stamp, extrapolates.
    /// Sub-ms pairs (queued double-present) never become the period.
    #[test]
    fn latch_clock_learns_and_extrapolates() {
        const P: u64 = 16_666_666; // 60 Hz
        let mut c = LatchClock::new(60);
        assert_eq!(c.period_ns(), P, "fallback = the mode refresh");
        assert_eq!(c.next_slot_after(1_000), 1_000 + P);

        c.note_batch(
            &[1_000_000_000, 1_000_000_000 + P, 1_000_000_000 + 2 * P],
            false,
        );
        assert_eq!(c.period_ns(), P);
        assert_eq!(c.anchor_ns(), 1_000_000_000 + 2 * P);
        let next = c.next_slot_after(c.anchor_ns());
        assert_eq!(next, 1_000_000_000 + 3 * P);
        assert_eq!(c.next_slot_after(next - 1), next);
        assert_eq!(c.next_slot_after(next), next + P);

        // A queued pair (< 1 ms apart) must not become the period.
        c.note_batch(&[2_000_000_000, 2_000_000_500], false);
        assert_eq!(c.period_ns(), P);
        assert_eq!(c.anchor_ns(), 2_000_000_500, "the anchor still advances");

        // 2×P is a slow stream, not a slower panel. PanelGrid needs a widen streak
        // before it grows; one window must not move the estimate.
        c.note_batch(&[3_000_000_000, 3_000_000_000 + 2 * P], false);
        assert_eq!(c.period_ns(), P, "one wide window is not a slower panel");

        c.note_batch(&[5_000_000_000], false);
        assert_eq!(c.anchor_ns(), 5_000_000_000);
        assert_eq!(c.period_ns(), P);

        let mut fast = LatchClock::new(120);
        fast.note_batch(&[1_000_000_000, 1_008_333_333], false);
        assert_eq!(fast.period_ns(), 8_333_333);
    }

    /// The live drain is one stamp per pass. Spacings only within a batch observe
    /// nothing and the learner stays on its seed.
    #[test]
    fn latch_clock_learns_from_one_sample_at_a_time() {
        const REAL: u64 = 16_666_666;
        let mut c = LatchClock::new(120); // seed too fast: refused mode switch
        let mut t = 1_000_000_000u64;
        for _ in 0..(GRID_OBSERVE_EVERY * 8 + 8) {
            t += REAL;
            c.note_batch(&[t], false);
        }
        assert_eq!(
            c.period_ns(),
            REAL,
            "single-stamp batches must still feed the grid learner"
        );
        assert_eq!(c.anchor_ns(), t);
    }

    /// The mode refresh is a claim. A refused switch or a compositor at its own rate
    /// seeds too fast; the learner climbs back once evidence is consistent.
    #[test]
    fn latch_clock_recovers_from_a_seed_faster_than_the_real_panel() {
        const REAL: u64 = 16_666_666; // 60 Hz panel
        let mut c = LatchClock::new(120); // mode claimed 120
        assert_eq!(c.period_ns(), 8_333_333, "seeded from the claim");

        // PanelGrid widens after 8 agreeing observations, each the min of
        // GRID_OBSERVE_EVERY spacings — one slow patch must not redefine the panel.
        let mut t = 1_000_000_000u64;
        for _ in 0..(GRID_OBSERVE_EVERY * 8 + GRID_OBSERVE_EVERY) {
            t += REAL;
            c.note_batch(&[t], false);
        }
        assert_eq!(
            c.period_ns(),
            REAL,
            "a sustained slower grid is adopted instead of aimed past forever"
        );
    }

    /// Fixed = on the vblank grid (including a slower stream at k×period). Variable =
    /// off that grid.
    #[test]
    fn cadence_probe_separates_grid_locked_from_variable() {
        const P: u64 = 8_333_333; // 120 Hz
                                  // CADENCE_STABLE_ROUNDS full rounds: a verdict publishes only after consecutive
                                  // rounds agree.
        const ROUNDS: u64 = (CADENCE_MIN_SAMPLES as u64) * (CADENCE_STABLE_ROUNDS as u64) + 4;

        let mut probe = CadenceProbe::new();
        assert_eq!(probe.verdict(), Cadence::Unknown, "no evidence yet");
        let stamps: Vec<u64> = (0..ROUNDS).map(|i| 1_000_000_000 + i * P).collect();
        probe.note(&stamps, P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed);

        // Half panel rate: 2×P is still grid-locked, not Variable.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..ROUNDS).map(|i| 1_000_000_000 + i * 2 * P).collect();
        probe.note(&stamps, P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Fixed,
            "a slower stream on a fixed panel picks a larger k, it does not leave the grid"
        );

        // ±0.5 ms jitter on an 8.3 ms period is not VRR.
        let mut probe = CadenceProbe::new();
        let jitter = [0i64, 300_000, -250_000, 120_000, -400_000, 80_000];
        let stamps: Vec<u64> = (0..ROUNDS as usize)
            .map(|i| (1_000_000_000 + i as i64 * P as i64 + jitter[i % jitter.len()]) as u64)
            .collect();
        probe.note(&stamps, P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed, "jitter is not VRR");

        // 100 fps on a 120 Hz-max panel: 10 ms is not a multiple of 8.33 ms.
        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..ROUNDS)
            .map(|i| 1_000_000_000 + i * 10_000_000)
            .collect();
        probe.note(&stamps, P, true);
        assert_eq!(probe.verdict(), Cadence::Variable);

        probe.reset();
        assert_eq!(probe.verdict(), Cadence::Unknown);

        let mut probe = CadenceProbe::new();
        probe.note(&[1_000_000_000, 1_010_000_000, 1_020_000_000], P, true);
        assert_eq!(probe.verdict(), Cadence::Unknown);

        // Live drain is one stamp per pass; spacings must still be measured.
        let mut probe = CadenceProbe::new();
        for i in 0..ROUNDS {
            probe.note(&[1_000_000_000 + i * 10_000_000], P, true); // 100 fps, off a 120 Hz grid
        }
        assert_eq!(
            probe.verdict(),
            Cadence::Variable,
            "one-sample batches must still yield spacings"
        );

        let mut probe = CadenceProbe::new();
        let stamps: Vec<u64> = (0..ROUNDS)
            .map(|i| 1_000_000_000 + i * 10_000_000)
            .collect();
        probe.note(&stamps, 0, true);
        assert_eq!(probe.verdict(), Cadence::Unknown);
    }

    /// Batching must not change the verdict: live drain is one stamp, tests hand over
    /// vectors.
    #[test]
    fn cadence_verdict_is_independent_of_batching() {
        const P: u64 = 8_333_333;
        let n = (CADENCE_MIN_SAMPLES as u64) * (CADENCE_STABLE_ROUNDS as u64) + 4;

        let stamps: Vec<u64> = (0..n).map(|i| 1_000_000_000 + i * P).collect();
        let mut bulk = CadenceProbe::new();
        bulk.note(&stamps, P, true);

        let mut drip = CadenceProbe::new();
        for s in &stamps {
            drip.note(&[*s], P, true);
        }

        assert_eq!(bulk.verdict(), Cadence::Fixed);
        assert_eq!(drip.verdict(), bulk.verdict(), "batching must not matter");
    }

    /// 120 Hz, source stamps in a different clock domain — the pacer is never told
    /// about the gap.
    const SRC_P: i64 = 8_333_333;
    const SRC_PTS0: u64 = 1_786_000_000_000_000_000;
    const SRC_READY0: u64 = 1_000_000_000;

    fn fold(p: &mut SourcePacer, smoothing: bool, n: u64) {
        for k in 0..n {
            p.due_ns(
                smoothing,
                SRC_PTS0 + k * SRC_P as u64,
                SRC_READY0 + k * SRC_P as u64,
                SRC_P,
            );
        }
    }

    /// Latency folds nothing: the estimate exists only where it is used, so a
    /// mid-stream collapse to latency (PyroWave) leaves no half-built loop.
    #[test]
    fn the_latency_intent_folds_no_frames_into_the_cadence_clock() {
        let mut p = SourcePacer::new();
        for k in 0..64u64 {
            assert_eq!(
                p.due_ns(
                    false,
                    SRC_PTS0 + k * SRC_P as u64,
                    SRC_READY0 + k * SRC_P as u64,
                    SRC_P
                ),
                None,
                "latency has no due time to answer with"
            );
        }
        let h = p.health();
        assert_eq!(h.frames, 0, "not one sample reached the loop");
        assert_eq!((h.offset_ns, h.skew_ns, h.jitter_ns), (0, 0, 0));
        assert!(p.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P).is_some());
        assert_eq!(p.health().frames, 1);
    }

    /// Due time is on the present clock (`decoded_ns` in, `now_ns` deadline out) with
    /// no conversion, however far the source stamps sit from it.
    #[test]
    fn a_due_time_comes_back_on_the_present_clocks_timeline() {
        let mut p = SourcePacer::new();
        fold(&mut p, true, 400);
        let k = 400u64;
        let due = p
            .due_ns(
                true,
                SRC_PTS0 + k * SRC_P as u64,
                SRC_READY0 + k * SRC_P as u64,
                SRC_P,
            )
            .unwrap();
        let ready = (SRC_READY0 + k * SRC_P as u64) as i64;
        assert!(
            (due - ready).abs() <= p.health().cushion_ns,
            "due {due} is not within a cushion of the present-clock ready {ready}"
        );
    }

    /// Measured VRR: no grid to snap to, so the due time is presented directly and
    /// the cushion covers the distribution. Re-tuning is a fresh loop, so this
    /// follows the published verdict.
    #[test]
    fn a_measured_vrr_verdict_switches_the_cushion_policy() {
        let mut p = SourcePacer::new();
        assert!(!p.free_running(), "snapping until the panel says otherwise");
        fold(&mut p, true, 200);
        p.follow(Cadence::Fixed);
        assert!(!p.free_running());
        assert_eq!(
            p.health().frames,
            200,
            "a verdict that changes nothing must not re-anchor"
        );
        p.follow(Cadence::Variable);
        assert!(p.free_running());
        assert_eq!(p.health().frames, 0, "re-tuning is a fresh loop");
        p.follow(Cadence::Unknown);
        assert!(
            !p.free_running(),
            "Unknown is the absence of a measurement, not a measurement of VRR"
        );

        // With no jitter yet, free-running already holds a frame back more than
        // snapping, which rides the half-refresh the snap-up gives it.
        let mut snap = SourcePacer::new();
        snap.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        let mut free = SourcePacer::new();
        free.follow(Cadence::Variable);
        free.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        assert!(
            free.health().cushion_ns > snap.health().cushion_ns,
            "free-running {} must cushion past snapping {}",
            free.health().cushion_ns,
            snap.health().cushion_ns
        );
    }

    #[test]
    fn gate_budgets_one_undisplayed_present() {
        let mut g = PresentGate::default();
        let t0 = 1_000_000_000u64;
        assert!(g.open(0, t0));
        g.note_present(t0);
        assert!(!g.open(1, t0 + 8_000_000), "one in flight — hold");
        assert!(
            g.open(1, t0 + STALE_REOPEN_NS + 1),
            "stale in-flight present force-opens"
        );
        let (gated, forced) = g.take_counters();
        assert_eq!((gated, forced), (1, 1));
        assert_eq!(g.take_counters(), (0, 0), "counters drain");
    }
}
