//! Presentation intents the run loop composes: store, latch clock, gate, source pacer.
//!
//! * [`FrameStore`] — newest-wins (`capacity == 0`) or smoothing FIFO held for the due
//!   time (`capacity 1..=3`). Same contract as the Apple presenter's cadence take.
//! * [`LatchClock`] — panel latch grid from `VK_KHR_present_wait` on-glass stamps.
//!   A reported refresh is a mode claim; VRR makes it unusable. Without present-wait
//!   there is no grid: the drain presents at the due time and the panel quantizes.
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
/// (`capacity 1..=3`: held until due, drop-oldest overflow). The due time's cushion is
/// the headroom, so an empty store is the steady state and starvation reads as
/// `CadenceHealth::late`.
pub(crate) struct FrameStore<T> {
    capacity: usize,
    frames: VecDeque<T>,
    /// Newest-wins holds its frame until due or until the next one arrives (the latency
    /// intent on a measured VRR panel).
    paced: bool,
    /// Newest-wins displacements; not a fault.
    replaced: u32,
    overflow_drops: u32,
}

impl<T> FrameStore<T> {
    pub(crate) fn new(capacity: usize) -> FrameStore<T> {
        FrameStore {
            capacity,
            frames: VecDeque::with_capacity(capacity.max(1) + 1),
            paced: false,
            replaced: 0,
            overflow_drops: 0,
        }
    }

    pub(crate) fn is_smoothing(&self) -> bool {
        self.capacity > 0
    }

    /// Newest-wins holds its frame for the due time. Off, a frame is due the instant it
    /// exists. FIFO stores always ask.
    pub(crate) fn set_paced(&mut self, paced: bool) {
        self.paced = paced;
    }

    pub(crate) fn submit(&mut self, f: T) {
        if self.capacity == 0 && self.paced {
            // A held frame is never overwritten: its successor queues behind it and
            // `take` releases the older one at once. Three deep bounds a stalled loop.
            self.frames.push_back(f);
            while self.frames.len() > 3 {
                self.frames.pop_front();
                self.replaced += 1;
            }
        } else if self.capacity == 0 {
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

    /// FIFO: vend the front once `due` is true. Newest-wins asks `due` only when paced,
    /// and only while nothing newer waits behind the frame: pacing may delay a frame
    /// until its successor arrives, never drop it. Unpaced, a frame is due the instant
    /// it exists and the cadence clock stays off the latency path.
    pub(crate) fn take(&mut self, due: impl FnOnce(&T) -> bool) -> Option<T> {
        if self.capacity == 0 {
            let alone = self.frames.len() == 1;
            if self.paced && alone && self.frames.front().is_some_and(|f| !due(f)) {
                return None;
            }
            return self.frames.pop_front();
        }
        // No preroll: a stream below the panel rate empties the store between frames,
        // and refilling to capacity each time shows its frames in bursts.
        if !due(self.frames.front()?) {
            return None;
        }
        self.frames.pop_front()
    }

    /// Next `take` candidate. The run loop waits until this frame is due.
    pub(crate) fn front(&self) -> Option<&T> {
        self.frames.front()
    }

    /// Unpresented frame (gate closed / present failed). Newest-wins reinserts only
    /// into an empty slot; FIFO and the paced slot put it at the front (it is the oldest).
    pub(crate) fn put_back(&mut self, f: T) {
        if self.capacity == 0 && !self.paced {
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
        while self.frames.len() > 1 {
            self.frames.pop_front();
        }
    }

    pub(crate) fn take_counters(&mut self) -> (u32, u32) {
        let c = (self.replaced, self.overflow_drops);
        self.replaced = 0;
        self.overflow_drops = 0;
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
/// is a real latch.
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
/// nearest period multiple: tight ⇒ Fixed, consistently off ⇒ Variable. A source
/// spacing that is itself a period multiple — the panel's own rate, or a whole
/// fraction of it — lands on the grid under either regime, so those deltas are no
/// evidence and the verdict stands until the cadence leaves the grid again.
pub(crate) struct CadenceProbe {
    /// Off-grid distances as a fraction of the period, in thousandths.
    off_grid_milli: Vec<u32>,
    /// Previous glass stamp and its source stamp, kept across calls: the live drain
    /// hands one sample at a time.
    last_ns: u64,
    last_pts_ns: u64,
    /// Last round's reading; a verdict publishes only after [`CADENCE_STABLE_ROUNDS`] agree.
    candidate: Cadence,
    agree_rounds: u8,
    verdict: Cadence,
    /// The output's own vblank spacing reads off the mode period, or on it; `None` where no
    /// waiter measures it. It outranks the stamps ([`CadenceProbe::note_refresh`]).
    refresh_variable: Option<bool>,
    /// The presentation engine says it runs variable refresh. It outranks everything.
    engine_variable: bool,
    /// The stream's own spacing, smoothed, from every shown frame's source stamp.
    src_spacing_ns: u64,
    src_last_pts_ns: u64,
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
/// differ by far more. A source spacing inside it proves nothing either way.
const CADENCE_FIXED_MILLI: u32 = 150;

/// How far `delta_ns` sits from the nearest whole number of `period_ns`.
pub(crate) fn off_grid_ns(delta_ns: u64, period_ns: u64) -> u64 {
    if period_ns == 0 {
        return 0;
    }
    let rem = delta_ns % period_ns;
    // A delta just under k×period is on the grid, not a whole period away from k-1.
    rem.min(period_ns - rem)
}

/// [`off_grid_ns`] as thousandths of the period.
fn off_grid_milli(delta_ns: u64, period_ns: u64) -> u32 {
    (off_grid_ns(delta_ns, period_ns).saturating_mul(1000) / period_ns.max(1)) as u32
}

impl CadenceProbe {
    pub(crate) fn new() -> CadenceProbe {
        CadenceProbe {
            off_grid_milli: Vec::with_capacity(64),
            last_ns: 0,
            last_pts_ns: 0,
            candidate: Cadence::Unknown,
            agree_rounds: 0,
            verdict: Cadence::Unknown,
            refresh_variable: None,
            engine_variable: false,
            src_spacing_ns: 0,
            src_last_pts_ns: 0,
        }
    }

    /// Every shown frame's source stamp, whatever its glass stamp is worth: the stream's
    /// own spacing tells a panel-rate stream from a slower one.
    pub(crate) fn note_source(&mut self, pts_ns: u64) {
        let prev = std::mem::replace(&mut self.src_last_pts_ns, pts_ns);
        if prev == 0 || pts_ns <= prev {
            return;
        }
        let d = pts_ns - prev;
        self.src_spacing_ns = if self.src_spacing_ns == 0 {
            d
        } else {
            (self.src_spacing_ns * 7 + d) / 8
        };
    }

    /// The engine's own word on variable refresh, where it gives one.
    pub(crate) fn note_engine_variable(&mut self, variable: bool) {
        self.engine_variable = variable;
    }

    /// The output's measured vblank spacing, where a waiter reads it. A fixed panel
    /// refreshes at its mode period whatever is presented, so a spacing well off it is
    /// variable refresh — also at a whole divisor of the mode rate (60 on 120), where
    /// present stamps alone read as grid-locked. At the mode period under a slower stream
    /// the panel is fixed; under a stream at that rate it proves nothing.
    pub(crate) fn note_refresh(&mut self, refresh_ns: u64, mode_period_ns: u64) {
        let at_mode = refresh_ns * 100 < mode_period_ns * 103;
        let stream_at_panel_rate = self.src_spacing_ns > 0
            && self.src_spacing_ns.abs_diff(mode_period_ns) * 10 <= mode_period_ns;
        if at_mode && stream_at_panel_rate {
            return;
        }
        // 10 % on, 3 % off: a stream hovering at the panel's top rate does not flap.
        if refresh_ns * 100 > mode_period_ns * 110 {
            self.refresh_variable = Some(true);
        } else if refresh_ns * 100 < mode_period_ns * 103 || self.refresh_variable.is_none() {
            self.refresh_variable = Some(false);
        }
    }

    /// Fold shown frames as `(displayed_ns, pts_ns)` against the display mode's period.
    /// Spacing is against the previous frame, whatever the batch size, and a delta
    /// counts only where the source spacing is off the grid: at the panel's rate the
    /// glass sits on it under either regime.
    ///
    /// `healthy` is "presents were flowing" (no stale force-opens). A distressed
    /// pipeline smears spacings for non-panel reasons; evidence is dropped but the
    /// previous stamps still advance so the timeline stays continuous.
    pub(crate) fn note(&mut self, frames: &[(u64, u64)], period_ns: u64, healthy: bool) {
        if period_ns == 0 || !healthy {
            if let Some(&(s, pts)) = frames.last() {
                self.last_ns = s;
                self.last_pts_ns = pts;
            }
            return;
        }
        for &(s, pts) in frames {
            let prev = std::mem::replace(&mut self.last_ns, s);
            let prev_pts = std::mem::replace(&mut self.last_pts_ns, pts);
            if prev == 0 || s <= prev || prev_pts == 0 || pts <= prev_pts {
                continue;
            }
            if off_grid_milli(pts - prev_pts, period_ns) <= CADENCE_FIXED_MILLI {
                continue;
            }
            self.off_grid_milli
                .push(off_grid_milli(s - prev, period_ns));
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
        if self.engine_variable {
            return Cadence::Variable;
        }
        match self.refresh_variable {
            Some(true) => Cadence::Variable,
            Some(false) => Cadence::Fixed,
            None => self.verdict,
        }
    }

    /// A mode switch or display change invalidates the evidence.
    pub(crate) fn reset(&mut self) {
        self.off_grid_milli.clear();
        self.last_ns = 0;
        self.last_pts_ns = 0;
        self.candidate = Cadence::Unknown;
        self.agree_rounds = 0;
        self.verdict = Cadence::Unknown;
        self.refresh_variable = None;
        self.engine_variable = false;
    }
}

/// How the pacer serves frames.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PaceMode {
    /// Smoothness on a fixed panel: due time snapped onto the latch grid.
    Snap,
    /// Smoothness at the due time: measured VRR, or no glass grid to snap to.
    Free,
    /// The latency intent on a measured VRR panel: the due time with a tight cushion.
    VrrLatency,
}

/// `PUNKTFUNK_VRR_PACE=0` keeps the latency intent arrival-driven on a VRR panel: the
/// A/B for the pacing, and the way out where a panel takes it badly.
fn vrr_latency_pacing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_client_core::env_on("PUNKTFUNK_VRR_PACE") != Some(false))
}

/// Plays frames on the source cadence: a [`CadenceClock`] plus the two client
/// policies — which intent applies, and which cushion the measured refresh asks for.
///
/// The loop smooths the OFFSET, never the timestamps: genuine source-cadence
/// variation passes through to the due time. More-even due times than the source
/// would be a bug.
pub(crate) struct SourcePacer {
    clock: CadenceClock,
    mode: PaceMode,
}

impl SourcePacer {
    pub(crate) fn new() -> SourcePacer {
        SourcePacer {
            clock: CadenceClock::new(CadenceTuning::snapping()),
            mode: PaceMode::Snap,
        }
    }

    /// Fold an arriving frame and return when it is due, in `ready_ns`'s clock domain.
    ///
    /// `None` under latency on a fixed panel: arrival-driven, so the loop never carries
    /// an estimate built from samples it then ignored. Called at submit, not take:
    /// dropped frames are part of the arrival process; folding only survivors hides
    /// the jitter.
    pub(crate) fn due_ns(
        &mut self,
        smoothing: bool,
        src_pts_ns: u64,
        ready_ns: u64,
        frame_interval_ns: i64,
    ) -> Option<i64> {
        (smoothing || self.paces_latency()).then(|| {
            self.clock
                .due_ns(src_pts_ns, ready_ns as i64, frame_interval_ns)
        })
    }

    /// Follow the measured refresh verdict for this intent. Snapping onto the latch grid
    /// carries roughly half a refresh of slack; presenting at the due time carries none,
    /// so the cushions differ. Re-tuning re-anchors and keeps the measured jitter, so
    /// this keys off the probe's published verdict, not a per-window reading.
    /// Smoothness with no glass grid (`grid_known` false: no present-wait) runs free — a
    /// grid anchored on submit instants learns the stream's own cadence as the panel.
    /// The latency intent paces only where VRR is measured: a VRR panel shows every
    /// millisecond of arrival jitter; a fixed one hides it under the quantization.
    pub(crate) fn follow(&mut self, verdict: Cadence, grid_known: bool, smoothing: bool) {
        let mode = match (verdict == Cadence::Variable, smoothing) {
            (true, true) => PaceMode::Free,
            (true, false) if vrr_latency_pacing() => PaceMode::VrrLatency,
            (false, true) if !grid_known => PaceMode::Free,
            _ => PaceMode::Snap,
        };
        if mode != self.mode {
            self.mode = mode;
            self.clock.retune(match mode {
                PaceMode::Snap => CadenceTuning::snapping(),
                PaceMode::Free => CadenceTuning::free_running(),
                PaceMode::VrrLatency => CadenceTuning::vrr_latency(),
            });
        }
    }

    /// Present at the due time instead of snapping to the latch grid: variable refresh
    /// measured live (the panel refreshes when we present), or no glass grid at all.
    pub(crate) fn free_running(&self) -> bool {
        self.mode != PaceMode::Snap
    }

    /// The latency intent is paced: measured VRR, newest-wins held for its due time.
    pub(crate) fn paces_latency(&self) -> bool {
        self.mode == PaceMode::VrrLatency
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
            (2, 0),
            "two displacements, no fifo counters"
        );
    }

    #[test]
    fn fifo_vends_in_order_and_overflows_oldest() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        assert!(s.is_smoothing());
        s.submit(1);
        assert_eq!(s.take(|_| true), Some(1), "a due frame goes out alone");
        assert_eq!(s.take(|_| true), None);
        // A stream below the panel rate: the store empties between frames and
        // each one still goes out at its own due time.
        s.submit(2);
        assert_eq!(
            s.take(|_| true),
            Some(2),
            "an empty store does not re-buffer"
        );
        s.submit(3);
        s.submit(4);
        assert_eq!(s.take(|_| true), Some(3), "FIFO order");
        s.submit(5);
        s.submit(6);
        s.submit(7);
        assert_eq!(s.take(|_| true), Some(6));
        assert_eq!(s.take(|_| true), Some(7));
        assert_eq!(s.take_counters(), (0, 2), "6 evicted 4, 7 evicted 5");
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

    #[test]
    fn a_fifo_frame_is_held_until_due() {
        let mut s: FrameStore<u32> = FrameStore::new(2);
        s.submit(10);
        s.submit(20);
        assert_eq!(s.take(|_| false), None, "nothing is due yet");
        assert_eq!(s.take(|&v| v <= 10), Some(10));
        assert_eq!(s.take(|&v| v <= 10), None, "20 is not due yet either");
        assert_eq!(s.take(|_| true), Some(20));
        assert_eq!(s.take_counters(), (0, 0), "a hold drops nothing");
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
        // Paced (a measured VRR panel): held for its due time, never overwritten.
        s.set_paced(true);
        s.submit(8);
        assert_eq!(s.take(|_| false), None, "paced: held until due");
        s.submit(9);
        assert_eq!(
            s.take(|_| false),
            Some(8),
            "its successor arrived: the held frame goes out, it is not replaced"
        );
        assert_eq!(
            s.take(|_| false),
            None,
            "the successor waits for its own due time"
        );
        assert_eq!(s.take(|_| true), Some(9));
        assert_eq!(s.take_counters(), (0, 0), "pacing dropped nothing");
        // A stalled loop is still bounded: the fourth arrival evicts the oldest.
        for v in 10..14 {
            s.submit(v);
        }
        assert_eq!(s.take(|_| false), Some(11));
        assert_eq!(s.take_counters(), (1, 0));
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

    /// `(displayed, pts)` for a source at `src` spacing on a fixed panel: each frame
    /// lands on the next grid line at or after it.
    fn fixed_panel(start: u64, n: usize, period: u64, src: u64) -> Vec<(u64, u64)> {
        (0..n as u64)
            .map(|i| {
                let pts = start + i * src;
                (pts.div_ceil(period) * period, pts)
            })
            .collect()
    }

    /// `(displayed, pts)` under live VRR: the glass follows the source.
    fn vrr_panel(start: u64, n: usize, src: u64) -> Vec<(u64, u64)> {
        (0..n as u64)
            .map(|i| (start + i * src, start + i * src))
            .collect()
    }

    /// Frames for CADENCE_STABLE_ROUNDS full rounds: a verdict publishes only after
    /// consecutive rounds agree.
    const ROUNDS: usize = CADENCE_MIN_SAMPLES * CADENCE_STABLE_ROUNDS as usize + 4;

    /// Fixed = on the vblank grid, whatever the source does. Variable = off that grid,
    /// following the source.
    #[test]
    fn cadence_probe_separates_grid_locked_from_variable() {
        const P: u64 = 8_333_333; // 120 Hz
        let src = P * 3 / 2; // 80 fps: well off the grid

        let mut probe = CadenceProbe::new();
        assert_eq!(probe.verdict(), Cadence::Unknown, "no evidence yet");
        probe.note(&fixed_panel(1_000_000_000, ROUNDS, P, src), P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed);

        // ±0.5 ms stamp jitter on an 8.3 ms grid is not VRR.
        let jitter = [0i64, 300_000, -250_000, 120_000, -400_000, 80_000];
        let shaky: Vec<(u64, u64)> = fixed_panel(1_000_000_000, ROUNDS, P, src)
            .iter()
            .enumerate()
            .map(|(i, &(s, pts))| ((s as i64 + jitter[i % jitter.len()]) as u64, pts))
            .collect();
        let mut probe = CadenceProbe::new();
        probe.note(&shaky, P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed, "jitter is not VRR");

        // 100 fps on a 120 Hz-max panel: 10 ms is not a multiple of 8.33 ms.
        let mut probe = CadenceProbe::new();
        probe.note(&vrr_panel(1_000_000_000, ROUNDS, 10_000_000), P, true);
        assert_eq!(probe.verdict(), Cadence::Variable);

        probe.reset();
        assert_eq!(probe.verdict(), Cadence::Unknown);

        let mut probe = CadenceProbe::new();
        probe.note(&vrr_panel(1_000_000_000, 3, 10_000_000), P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Unknown,
            "three frames are not a round"
        );

        // Live drain is one frame per pass; spacings must still be measured.
        let mut probe = CadenceProbe::new();
        for f in vrr_panel(1_000_000_000, ROUNDS, 10_000_000) {
            probe.note(&[f], P, true);
        }
        assert_eq!(
            probe.verdict(),
            Cadence::Variable,
            "one-frame batches must still yield spacings"
        );

        let mut probe = CadenceProbe::new();
        probe.note(&vrr_panel(1_000_000_000, ROUNDS, 10_000_000), 0, true);
        assert_eq!(probe.verdict(), Cadence::Unknown);
    }

    /// The panel's own rate, or a whole fraction of it, lands on the grid under either
    /// regime: those frames change nothing, the last verdict stands. Before any verdict
    /// they leave Unknown — an unproven "vrr no" would be a claim.
    #[test]
    fn a_stream_at_the_panel_rate_keeps_the_last_verdict() {
        const P: u64 = 10_309_278; // 97 Hz
        const IDLE: u64 = 15_873_015; // 63 fps host repeats: 1.54 periods
        let t = |n: usize| 1_000_000_000 + n as u64 * 20_000_000;

        let mut probe = CadenceProbe::new();
        probe.note(&vrr_panel(t(0), ROUNDS, IDLE), P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Variable,
            "idle repeats measure VRR"
        );
        probe.note(&vrr_panel(t(ROUNDS), 4 * ROUNDS, P), P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Variable,
            "motion at the panel rate is no evidence"
        );
        probe.note(&vrr_panel(t(5 * ROUNDS), 4 * ROUNDS, 2 * P), P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Variable,
            "half rate: two periods either way"
        );

        let mut probe = CadenceProbe::new();
        probe.note(&fixed_panel(t(0), 4 * ROUNDS, P, P), P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Unknown,
            "a fixed panel at its rate: unproven"
        );
        probe.note(&fixed_panel(t(4 * ROUNDS), ROUNDS, P, IDLE), P, true);
        assert_eq!(probe.verdict(), Cadence::Fixed);
        probe.note(&vrr_panel(t(5 * ROUNDS), 4 * ROUNDS, P), P, true);
        assert_eq!(
            probe.verdict(),
            Cadence::Fixed,
            "panel-rate frames keep the verdict"
        );
    }

    /// Half the mode rate on a VRR panel: the stamps sit on the mode grid, the output's
    /// own refresh does not. Where it is measured it decides, both ways.
    #[test]
    fn a_measured_refresh_outranks_the_stamps() {
        const P: u64 = 6_060_606;
        let mut p = CadenceProbe::new();
        p.note(&vrr_panel(1_000_000_000, 60, 2 * P), P, true);
        assert_eq!(
            p.verdict(),
            Cadence::Unknown,
            "stamps alone: on the grid, unproven"
        );
        p.note_refresh(2 * P, P);
        assert_eq!(p.verdict(), Cadence::Variable);
        p.note_refresh(P + P / 20, P);
        assert_eq!(
            p.verdict(),
            Cadence::Variable,
            "5 % off: inside the hysteresis"
        );
        p.note_refresh(P, P);
        assert_eq!(
            p.verdict(),
            Cadence::Fixed,
            "at the mode period: nothing to pace"
        );
        let mut fresh = CadenceProbe::new();
        fresh.note_refresh(P + P / 20, P);
        assert_eq!(
            fresh.verdict(),
            Cadence::Fixed,
            "first reading, inside the band"
        );
    }

    /// The output's spacing at the mode period proves nothing while the stream itself
    /// runs at that rate; under a slower stream it is a fixed panel.
    #[test]
    fn a_panel_rate_stream_at_the_mode_period_is_no_evidence() {
        const P: u64 = 6_944_444; // 144 Hz
        let mut p = CadenceProbe::new();
        for i in 1..=16u64 {
            p.note_source(1_000_000_000 + i * P);
        }
        p.note_refresh(P + P / 50, P);
        assert_eq!(p.verdict(), Cadence::Unknown, "at the panel rate: unproven");
        p.note_refresh(P * 3 / 2, P);
        assert_eq!(p.verdict(), Cadence::Variable);
        p.note_refresh(P, P);
        assert_eq!(p.verdict(), Cadence::Variable, "the last reading stands");

        let mut slow = CadenceProbe::new();
        for i in 1..=16u64 {
            slow.note_source(1_000_000_000 + i * 2 * P);
        }
        slow.note_refresh(P, P);
        assert_eq!(
            slow.verdict(),
            Cadence::Fixed,
            "a slower stream on a panel at its mode rate"
        );
    }

    /// Batching must not change the verdict: live drain is one frame, tests hand over
    /// vectors.
    #[test]
    fn cadence_verdict_is_independent_of_batching() {
        const P: u64 = 8_333_333;
        let frames = fixed_panel(1_000_000_000, ROUNDS, P, P * 3 / 2);
        let mut bulk = CadenceProbe::new();
        bulk.note(&frames, P, true);

        let mut drip = CadenceProbe::new();
        for f in &frames {
            drip.note(&[*f], P, true);
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
    /// the cushion covers the distribution. Re-tuning keeps the loop, so this
    /// follows the published verdict.
    #[test]
    fn a_measured_vrr_verdict_switches_the_cushion_policy() {
        let mut p = SourcePacer::new();
        assert!(!p.free_running(), "snapping until the panel says otherwise");
        fold(&mut p, true, 200);
        p.follow(Cadence::Fixed, true, true);
        assert!(!p.free_running());
        assert_eq!(
            p.health().frames,
            200,
            "a verdict that changes nothing must not re-anchor"
        );
        p.follow(Cadence::Variable, true, true);
        assert!(p.free_running());
        assert_eq!(p.health().frames, 200, "re-tuning keeps the loop's history");
        p.follow(Cadence::Unknown, true, true);
        assert!(
            !p.free_running(),
            "Unknown is the absence of a measurement, not a measurement of VRR"
        );
        // No on-glass stamps (no present-wait): nothing to snap to, so the due time
        // is the target here too, whatever the verdict.
        p.follow(Cadence::Unknown, false, true);
        assert!(p.free_running(), "without a glass grid the drain runs free");
        p.follow(Cadence::Fixed, false, true);
        assert!(p.free_running());
        p.follow(Cadence::Unknown, true, true);
        assert!(!p.free_running());

        // With no jitter yet, free-running already holds a frame back more than
        // snapping, which rides the half-refresh the snap-up gives it.
        let mut snap = SourcePacer::new();
        snap.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        let mut free = SourcePacer::new();
        free.follow(Cadence::Variable, true, true);
        free.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        assert!(
            free.health().cushion_ns > snap.health().cushion_ns,
            "free-running {} must cushion past snapping {}",
            free.health().cushion_ns,
            snap.health().cushion_ns
        );
    }

    /// The latency intent folds nothing on a fixed panel or with no grid at all, and
    /// paces on a measured VRR panel with a cushion below the smooth intent's.
    #[test]
    fn the_latency_intent_paces_only_on_a_measured_vrr_panel() {
        let mut p = SourcePacer::new();
        p.follow(Cadence::Unknown, false, false);
        assert!(
            !p.paces_latency() && !p.free_running(),
            "no grid is not VRR: latency stays arrival-driven"
        );
        assert_eq!(p.due_ns(false, SRC_PTS0, SRC_READY0, SRC_P), None);
        p.follow(Cadence::Variable, true, false);
        assert!(p.paces_latency() && p.free_running());
        assert!(p.due_ns(false, SRC_PTS0, SRC_READY0, SRC_P).is_some());
        let tight = p.health().cushion_ns;
        let mut smooth = SourcePacer::new();
        smooth.follow(Cadence::Variable, true, true);
        smooth.due_ns(true, SRC_PTS0, SRC_READY0, SRC_P);
        assert!(
            tight < smooth.health().cushion_ns,
            "latency under VRR ({tight}) must cushion less than smooth ({})",
            smooth.health().cushion_ns
        );
        p.follow(Cadence::Fixed, true, false);
        assert!(
            !p.paces_latency(),
            "a fixed verdict returns the latency intent to arrival"
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
