//! The phase-lock PLL: the client's [`PhaseReport`] steering when this host submits.
//!
//! [`PhaseCtl`] is the control-task → encode-loop bridge (latest-wins, drained ~1 Hz);
//! [`PhaseController`] is the loop that turns a report into a submit offset. Lifted out of the
//! frame loop so the controller reads on its own terms — it is a feedback loop, and a feedback
//! loop buried in the frame loop is one nobody re-derives.

use super::*;

/// `PUNKTFUNK_PHASE_LOCK=0` disarms the controller. Armed, it still waits for a [`PhaseReport`].
pub(super) fn phase_lock_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_PHASE_LOCK").unwrap_or(true))
}

/// Control-task → encode-loop bridge: latest-wins [`PhaseReport`], drained ~1 Hz, published as
/// the 0xCF ACK hold.
pub(crate) struct PhaseCtl {
    report: std::sync::Mutex<Option<punktfunk_core::quic::PhaseReport>>,
    applied_ns: std::sync::atomic::AtomicI64,
}

impl PhaseCtl {
    pub(crate) fn new() -> PhaseCtl {
        PhaseCtl {
            report: std::sync::Mutex::new(None),
            applied_ns: std::sync::atomic::AtomicI64::new(0),
        }
    }

    pub(crate) fn store(&self, r: punktfunk_core::quic::PhaseReport) {
        *self
            .report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r);
    }

    pub(super) fn take(&self) -> Option<punktfunk_core::quic::PhaseReport> {
        self.report
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(super) fn set_applied(&self, ns: i64) {
        self.applied_ns.store(ns, Ordering::Relaxed);
    }

    pub(crate) fn applied_ns(&self) -> i64 {
        self.applied_ns.load(Ordering::Relaxed)
    }
}

/// Submit lock onto an absolute grid `epoch + k×period + offset` (`design/phase-locked-capture.md`).
///
/// A per-frame additive hold on an arrival-slaved loop saturates once `hold + work ≥ interval`
/// and free-runs; the commanded phase never arrives. A periodic grid cannot free-run: occupancy
/// is one frame per period, so actuation is linear. Loop-local so it survives in-loop rebuilds;
/// a new session starts disengaged (no grid sleeps).
///
/// Failure = DISENGAGE, never park a hold. Near-antipode errors (within 1 ms of ±period/2) flip
/// sign on sampling noise — half-step until the error commits. Engagement needs sustained
/// coherence; each incoherent cycle backs off longer; a host that cannot hold coherence fuses
/// for the session. Evidence: `design/host-source-stutter-fixes.md`.
pub(super) struct PhaseController {
    /// Grid offset, ns ∈ [0, period). Meaningful only while engaged.
    pub(super) offset_ns: i64,
    /// Grid epoch; `None` = disengaged. Stamped at engage, cleared at disengage — the lock's age.
    pub(super) epoch: Option<std::time::Instant>,
    pub(super) last_adjust: std::time::Instant,
    /// Signed steps summed since engage — the chase detector. A chase moves one way; corrections
    /// around a held phase (window jitter wider than the deadband) cancel.
    pub(super) cum_travel_ns: i64,
    /// Consecutive incoherent reports; 3 disengage.
    pub(super) incoherent_streak: u32,
    /// Consecutive coherent reports — the engage gate.
    pub(super) coherent_streak: u32,
    /// Incoherent disengages this session. Forgiven by a lock that holds [`LOCK_STABLE`].
    pub(super) incoherent_cycles: u32,
    pub(super) fused: bool,
    pub(super) reengage_backoff: u32,
}

impl PhaseController {
    /// 2 ms/s of reports: wire cadence stays visually still; a half-period error converges in ~2–3 s.
    const MAX_STEP_NS: i64 = 2_000_000;
    const DEADBAND_NS: i64 = 300_000;
    /// SurfaceFlinger-class compositors need the frame ~2.5 ms before latch; `uncertainty_ns` widens this.
    const TARGET_LEAD_FLOOR_NS: i64 = 2_500_000;
    /// Below this circular coherence (‰) the arrival phase is smeared. `u16::MAX` bypasses the gate.
    const COHERENCE_FLOOR_MILLI: u16 = 300;
    /// Within this of ±period/2, sampling noise flips the sign — damp until the error commits.
    const ANTIPODE_GUARD_NS: i64 = 1_000_000;
    const REENGAGE_BACKOFF: u32 = 10;
    /// ~5 s of a lockable phase at the ~1 Hz report cadence. One report re-engages a hovering host.
    const ENGAGE_COHERENT_REPORTS: u32 = 5;
    /// Permanently disengaged (zero added latency) beats another cycle of timing steps.
    const INCOHERENT_FUSE: u32 = 8;
    /// `REENGAGE_BACKOFF << 5` = 320 ticks ≈ 5 min.
    const MAX_BACKOFF_SHIFT: u32 = 5;
    /// A transient bad patch must not fuse a host that is otherwise lockable.
    const LOCK_STABLE: std::time::Duration = std::time::Duration::from_secs(60);

    pub(super) fn new() -> PhaseController {
        PhaseController {
            offset_ns: 0,
            epoch: None,
            last_adjust: std::time::Instant::now(),
            cum_travel_ns: 0,
            incoherent_streak: 0,
            coherent_streak: 0,
            incoherent_cycles: 0,
            fused: false,
            reengage_backoff: 0,
        }
    }

    fn engaged(&self) -> bool {
        self.epoch.is_some()
    }

    /// `coherence_milli` is the number that says whether the host is marginal or hopeless.
    fn disengage(&mut self, reason: &'static str, backoff: u32, coherence_milli: u16) {
        if self.engaged() {
            tracing::info!(
                offset_ms = self.offset_ns as f64 / 1e6,
                coherence_milli,
                reason,
                "phase lock: disengaging the submit grid"
            );
        }
        self.epoch = None;
        self.offset_ns = 0;
        self.cum_travel_ns = 0;
        self.incoherent_streak = 0;
        self.coherent_streak = 0;
        self.reengage_backoff = backoff;
    }

    /// Positive (shortest-way) error = frames arrive early → grow the offset; negative → earlier.
    pub(super) fn adjust(&mut self, r: &punktfunk_core::quic::PhaseReport, period_ns: i64) {
        if period_ns <= 0 || self.fused {
            return;
        }
        self.last_adjust = std::time::Instant::now();
        if self.reengage_backoff > 0 {
            self.reengage_backoff -= 1;
            return;
        }
        let coherent =
            r.coherence_milli == u16::MAX || r.coherence_milli >= Self::COHERENCE_FLOOR_MILLI;
        if !coherent {
            self.coherent_streak = 0;
            self.incoherent_streak += 1;
            if self.incoherent_streak >= 3 {
                // Count only an engaged tear-down. Pre-lock incoherent arrival must not blow the fuse.
                if self.engaged() {
                    self.incoherent_cycles += 1;
                    if self.incoherent_cycles >= Self::INCOHERENT_FUSE {
                        self.fused = true;
                        tracing::info!(
                            cycles = self.incoherent_cycles,
                            coherence_milli = r.coherence_milli,
                            "phase lock: arrival phase incoherent on this host — parked for the \
                             session"
                        );
                    }
                }
                let backoff = Self::REENGAGE_BACKOFF
                    << self
                        .incoherent_cycles
                        .saturating_sub(1)
                        .min(Self::MAX_BACKOFF_SHIFT);
                self.disengage("incoherent arrival phase", backoff, r.coherence_milli);
            }
            return;
        }
        self.incoherent_streak = 0;
        self.coherent_streak = self.coherent_streak.saturating_add(1);
        // Forgive while the lock is good, not only when it is lost.
        if self.epoch.is_some_and(|e| e.elapsed() >= Self::LOCK_STABLE) {
            self.incoherent_cycles = 0;
        }
        let target = Self::TARGET_LEAD_FLOOR_NS.max(r.uncertainty_ns as i64 + 1_000_000);
        let raw = (r.arrival_lead_ns as i64 - target).rem_euclid(period_ns);
        let error = if raw > period_ns / 2 {
            raw - period_ns
        } else {
            raw
        };
        if error.abs() < Self::DEADBAND_NS {
            self.cum_travel_ns = 0;
            return;
        }
        if !self.engaged() {
            if self.coherent_streak < Self::ENGAGE_COHERENT_REPORTS {
                return;
            }
            self.epoch = Some(std::time::Instant::now());
            tracing::info!(
                coherence_milli = r.coherence_milli,
                "phase lock: engaging the submit grid"
            );
        }
        let mut step = error.clamp(-Self::MAX_STEP_NS, Self::MAX_STEP_NS);
        if error.abs() > period_ns / 2 - Self::ANTIPODE_GUARD_NS {
            step /= 2;
        }
        self.offset_ns = (self.offset_ns + step).rem_euclid(period_ns);
        self.cum_travel_ns += step;
        if self.cum_travel_ns.abs() > period_ns + period_ns / 4 {
            tracing::info!("phase lock: travel budget exhausted without convergence — disengaging");
            self.disengage("travel budget", Self::REENGAGE_BACKOFF, r.coherence_milli);
        }
    }

    /// Next grid instant at or after `now`. Newest-wins keeps content fresh across the wait.
    pub(super) fn next_submit_target(
        &self,
        now: std::time::Instant,
        period_ns: i64,
    ) -> Option<std::time::Instant> {
        let epoch = self.epoch?;
        if period_ns <= 0 {
            return None;
        }
        let elapsed = now.duration_since(epoch).as_nanos() as i64;
        let k = (elapsed - self.offset_ns).div_euclid(period_ns) + 1;
        let target_ns = k * period_ns + self.offset_ns;
        let target = epoch + std::time::Duration::from_nanos(target_ns.max(0) as u64);
        if target.duration_since(now).as_nanos() as i64 > period_ns {
            return Some(now);
        }
        Some(target)
    }

    pub(super) fn applied_readout(&self) -> i64 {
        if self.engaged() {
            self.offset_ns
        } else {
            0
        }
    }

    /// The grid a request-driven producer should paint on so its frame reaches the submit
    /// grid with `lead_ns` to spare: the engaged grid, `lead_ns` earlier. `None` disengaged.
    pub(super) fn paint_grid(&self, period_ns: i64, lead_ns: i64) -> Option<pf_capture::PaintGrid> {
        let epoch = self.epoch?;
        (period_ns > 0).then(|| pf_capture::PaintGrid {
            anchor_ns: pf_capture::mono_ns(epoch) + self.offset_ns - lead_ns,
            period_ns,
        })
    }

    pub(super) fn due(&self) -> bool {
        self.last_adjust.elapsed() >= std::time::Duration::from_secs(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIM_P: i64 = 8_333_333;
    const SIM_TARGET: i64 = 2_500_000;

    struct Lcg(u64);
    impl Lcg {
        fn next_noise(&mut self, spread_ns: i64) -> i64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            if spread_ns == 0 {
                return 0;
            }
            ((self.0 >> 33) as i64 % (2 * spread_ns)) - spread_ns
        }
    }

    fn report_from_lead(
        base_lead_ns: i64,
        noise_spread_ns: i64,
        rng: &mut Lcg,
    ) -> punktfunk_core::quic::PhaseReport {
        let samples_us: Vec<u64> = (0..120)
            .map(|_| {
                let lead = (base_lead_ns + rng.next_noise(noise_spread_ns)).rem_euclid(SIM_P);
                (lead / 1000) as u64
            })
            .collect();
        let (mean_ns, coherence) =
            punktfunk_core::phase::circular_latch(&samples_us, SIM_P).expect("120 samples");
        punktfunk_core::quic::PhaseReport {
            next_latch_host_ns: 0,
            latch_period_ns: SIM_P as u32,
            uncertainty_ns: 1_000_000,
            arrival_lead_ns: mean_ns as u32,
            coherence_milli: coherence,
        }
    }

    fn grid_lead(base_lead_ns: i64, c: &PhaseController) -> i64 {
        (base_lead_ns - c.applied_readout()).rem_euclid(SIM_P)
    }

    #[test]
    fn grid_plant_tight_jitter_locks_and_stays() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(7);
        for _ in 0..12 {
            let r = report_from_lead(grid_lead(7_500_000, &c), 500_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        let err = grid_lead(7_500_000, &c) - SIM_TARGET;
        assert!(c.engaged(), "a coherent linear plant must engage");
        assert!(
            err.abs() < 1_000_000,
            "tight jitter must converge near the target lead, residual {err} ns"
        );
        let before = c.offset_ns;
        for _ in 0..10 {
            let r = report_from_lead(grid_lead(7_500_000, &c), 500_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        assert!(
            (c.offset_ns - before).abs() <= 2 * PhaseController::MAX_STEP_NS,
            "a locked loop must not wander"
        );
    }

    /// Encode time that swings with content moves each report's mean ±1.5 ms around a held
    /// phase. Corrections of both signs cancel; they must not read as a chase.
    #[test]
    fn window_jitter_around_a_held_phase_stays_engaged() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(23);
        let mut locked = false;
        for i in 0..600 {
            let wobble = rng.next_noise(1_500_000);
            let r = report_from_lead(grid_lead(7_500_000, &c) + wobble, 300_000, &mut rng);
            c.adjust(&r, SIM_P);
            locked |= c.engaged();
            assert!(
                !locked || c.engaged(),
                "report {i}: disengaged under zero-mean window jitter"
            );
        }
        assert!(locked, "a coherent linear plant must engage");
        let err = grid_lead(7_500_000, &c) - SIM_TARGET;
        assert!(
            err.abs() < 2_000_000,
            "held near the target lead, residual {err} ns"
        );
    }

    #[test]
    fn grid_plant_antipode_start_converges_without_chatter() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(11);
        let base = (SIM_TARGET + SIM_P / 2).rem_euclid(SIM_P);
        for _ in 0..25 {
            let r = report_from_lead(grid_lead(base, &c), 400_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        let err = grid_lead(base, &c) - SIM_TARGET;
        assert!(
            err.abs() < 1_000_000,
            "an antipode start must still converge, residual {err} ns"
        );
        assert!(
            c.cum_travel_ns.abs() <= SIM_P,
            "damped antipode stepping spent {} ns of travel — it chattered",
            c.cum_travel_ns
        );
    }

    #[test]
    fn decoupled_plant_disengages_and_holds_nothing() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(13);
        let mut engaged_at_some_point = false;
        for _ in 0..40 {
            let r = report_from_lead(7_500_000, 300_000, &mut rng);
            c.adjust(&r, SIM_P);
            engaged_at_some_point |= c.engaged();
        }
        assert!(
            engaged_at_some_point,
            "the chase must have started before the budget tripped"
        );
        assert!(
            !c.engaged(),
            "a decoupled plant must end DISENGAGED, not parked"
        );
        assert_eq!(
            c.applied_readout(),
            0,
            "disengaged means zero applied offset"
        );
    }

    #[test]
    fn incoherent_phase_never_engages() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(17);
        for _ in 0..20 {
            let r = report_from_lead(7_500_000, SIM_P, &mut rng);
            c.adjust(&r, SIM_P);
        }
        assert!(
            !c.engaged(),
            "an incoherent phase must never engage the grid"
        );
    }

    #[test]
    fn regime_change_reengages_after_backoff() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(19);
        for _ in 0..40 {
            let r = report_from_lead(7_500_000, 300_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        assert!(!c.engaged());
        for _ in 0..30 {
            let r = report_from_lead(grid_lead(7_500_000, &c), 400_000, &mut rng);
            c.adjust(&r, SIM_P);
        }
        let err = grid_lead(7_500_000, &c) - SIM_TARGET;
        assert!(
            c.engaged(),
            "a linearized plant after backoff must re-engage"
        );
        assert!(err.abs() < 1_000_000, "…and lock, residual {err} ns");
    }

    #[test]
    fn submit_grid_is_periodic_and_offset_shifted() {
        let mut c = PhaseController::new();
        c.epoch = Some(std::time::Instant::now() - std::time::Duration::from_millis(50));
        c.offset_ns = 1_000_000;
        let now = std::time::Instant::now();
        let t1 = c.next_submit_target(now, SIM_P).unwrap();
        let t2 = c
            .next_submit_target(t1 + std::time::Duration::from_nanos(1), SIM_P)
            .unwrap();
        let dt = t2.duration_since(t1).as_nanos() as i64;
        assert!(
            (dt - SIM_P).abs() < 1_000,
            "grid ticks must advance by exactly one period, got {dt}"
        );
        c.offset_ns = 3_000_000;
        let t1b = c.next_submit_target(now, SIM_P).unwrap();
        let shift =
            t1b.duration_since(now).as_nanos() as i64 - t1.duration_since(now).as_nanos() as i64;
        assert!(
            (shift - 2_000_000).rem_euclid(SIM_P) < 1_000
                || (shift - 2_000_000).rem_euclid(SIM_P) > SIM_P - 1_000,
            "a +2 ms offset must shift the next target by +2 ms mod P, got {shift}"
        );
    }

    const COHERENT: u16 = PhaseController::COHERENCE_FLOOR_MILLI + 40;
    const INCOHERENT: u16 = PhaseController::COHERENCE_FLOOR_MILLI - 40;
    const ACTIONABLE_LEAD: i64 = 7_500_000;

    fn report_at(coherence_milli: u16, lead_ns: i64) -> punktfunk_core::quic::PhaseReport {
        punktfunk_core::quic::PhaseReport {
            next_latch_host_ns: 0,
            latch_period_ns: SIM_P as u32,
            uncertainty_ns: 1_000_000,
            arrival_lead_ns: lead_ns.rem_euclid(SIM_P) as u32,
            coherence_milli,
        }
    }

    fn one_incoherent_cycle(c: &mut PhaseController) -> u32 {
        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(c.engaged(), "the cycle must engage before it tears down");
        for _ in 0..3 {
            c.adjust(&report_at(INCOHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        let asked = c.reengage_backoff;
        while !c.fused && c.reengage_backoff > 0 {
            c.adjust(&report_at(INCOHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        asked
    }

    #[test]
    fn engage_requires_sustained_coherence() {
        let mut c = PhaseController::new();
        for i in 1..PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
            assert!(!c.engaged(), "engaged on only {i} coherent report(s)");
        }
        c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        assert!(
            c.engaged(),
            "sustained coherence must still engage the grid"
        );

        let mut c = PhaseController::new();
        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS - 1 {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        c.adjust(&report_at(INCOHERENT, ACTIONABLE_LEAD), SIM_P);
        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS - 1 {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(
            !c.engaged(),
            "a broken streak must not count toward engaging"
        );
    }

    #[test]
    fn incoherent_disengage_backoff_escalates() {
        let mut c = PhaseController::new();
        let asked: Vec<u32> = (0..4).map(|_| one_incoherent_cycle(&mut c)).collect();
        assert_eq!(
            asked,
            vec![10, 20, 40, 80],
            "each cycle must wait longer than the last (v3 asked for zero, every time)"
        );
    }

    #[test]
    fn fuse_after_repeated_cycles() {
        let mut c = PhaseController::new();
        for _ in 0..PhaseController::INCOHERENT_FUSE {
            one_incoherent_cycle(&mut c);
        }
        assert!(
            c.fused,
            "a host that never holds a lock must park for the session"
        );
        for _ in 0..50 {
            c.adjust(&report_at(u16::MAX, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(!c.engaged(), "a fused controller must stay disengaged");
    }

    #[test]
    fn stable_lock_resets_escalation() {
        let mut c = PhaseController::new();
        one_incoherent_cycle(&mut c);
        one_incoherent_cycle(&mut c);
        assert_eq!(c.incoherent_cycles, 2, "two cycles should have escalated");

        for _ in 0..PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(c.engaged());
        c.epoch = Some(
            std::time::Instant::now()
                - PhaseController::LOCK_STABLE
                - std::time::Duration::from_secs(1),
        );
        c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        assert_eq!(
            c.incoherent_cycles, 0,
            "a lock that held past LOCK_STABLE must forgive the escalation"
        );
    }

    #[test]
    fn flap_replay_stops_the_engage_churn() {
        let mut c = PhaseController::new();
        let mut rng = Lcg(23);
        let (mut engagements, mut reports, mut coherent_side) = (0u32, 0u32, true);
        while reports < 24 * 60 {
            let run = 2 + rng.next_noise(3).rem_euclid(3) as u32;
            for _ in 0..run {
                let was = c.engaged();
                let side = if coherent_side { COHERENT } else { INCOHERENT };
                c.adjust(&report_at(side, ACTIONABLE_LEAD), SIM_P);
                engagements += u32::from(!was && c.engaged());
                reports += 1;
            }
            coherent_side = !coherent_side;
        }
        assert!(
            engagements <= 2,
            "24 min of gate-hovering must not churn the grid: {engagements} engagements"
        );
        assert!(
            !c.fused,
            "flapping that never engaged must not blow the fuse"
        );
        for _ in 0..PhaseController::REENGAGE_BACKOFF + PhaseController::ENGAGE_COHERENT_REPORTS {
            c.adjust(&report_at(COHERENT, ACTIONABLE_LEAD), SIM_P);
        }
        assert!(
            c.engaged(),
            "a phase that finally holds must still get the grid"
        );
    }
}
