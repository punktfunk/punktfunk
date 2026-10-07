//! Panel-grid learning and source-timestamp playout.
//!
//! Shared client math: every presenter computes the same numbers. Pure, no features.
//!
//! - [`PanelGrid`] — learned refresh period. Finer is adopted at once; coarser
//!   only after [`PANEL_WIDEN_STREAK`] agreeing observations.
//! - [`CadenceClock`] — type-2 loop. Due time is `src_pts + offset + cushion`;
//!   timestamps are never smoothed.
//! - [`SlotClock`] / [`pace_slot`] / [`SlotIntervals`] — present-phased vsync
//!   slots, the one-slot-apart pacing rule, and the judder ruler.
//!
//! Evidence: `design/presenter-cadence-rework.md`,
//! `design/android-asc-presenter-pacing-overhaul.md`.

/// ~24 Hz to ~500 Hz. Outside this is a clock glitch, not a display mode.
const PANEL_PERIOD_RANGE_NS: std::ops::RangeInclusive<i64> = 2_000_000..=42_000_000;

/// 200 µs of timeline jitter is still the same grid.
const PANEL_GRID_TOLERANCE_NS: i64 = 200_000;

/// One stray wider sample is a hiccup; eight (~66 ms at 120 Hz) is a panel
/// that really slowed down.
const PANEL_WIDEN_STREAK: u8 = 8;

/// Learned refresh period from vsync/frame-timeline spacing.
///
/// Presenters subdivide release targets onto this grid. An estimate finer than
/// the panel aims at instants that never arrive, so the estimate moves both ways:
/// narrow immediately, widen only after [`PANEL_WIDEN_STREAK`] agreeing samples,
/// taking the narrowest of them (a single wide sample is a missed callback).
/// Seed from the mode table (what the panel can do); correct from timeline
/// spacing (what it is doing) — a per-uid override is not the panel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PanelGrid {
    period_ns: i64,
    widen_streak: u8,
    /// Narrowest of the current widen streak — not the latest, not the widest.
    widen_candidate: i64,
}

impl PanelGrid {
    /// `hz == 0` leaves the estimate unknown; the first plausible sample sets it.
    pub fn seeded(hz: i32) -> PanelGrid {
        PanelGrid {
            period_ns: if hz > 0 { 1_000_000_000 / hz as i64 } else { 0 },
            widen_streak: 0,
            widen_candidate: 0,
        }
    }

    pub fn period_ns(&self) -> i64 {
        self.period_ns
    }

    /// `true` when [`period_ns`](Self::period_ns) changed.
    pub fn observe(&mut self, spacing_ns: i64) -> bool {
        if !PANEL_PERIOD_RANGE_NS.contains(&spacing_ns) {
            return false;
        }
        if self.period_ns == 0 {
            self.reset_streak();
            self.period_ns = spacing_ns;
            return true;
        }
        if spacing_ns < self.period_ns - PANEL_GRID_TOLERANCE_NS {
            self.reset_streak();
            self.period_ns = spacing_ns;
            return true;
        }
        if spacing_ns > self.period_ns + PANEL_GRID_TOLERANCE_NS {
            self.widen_streak = self.widen_streak.saturating_add(1);
            self.widen_candidate = if self.widen_candidate == 0 {
                spacing_ns
            } else {
                self.widen_candidate.min(spacing_ns)
            };
            if self.widen_streak >= PANEL_WIDEN_STREAK {
                self.period_ns = self.widen_candidate;
                self.reset_streak();
                return true;
            }
            return false;
        }
        self.reset_streak(); // agreed sample; the widen run is broken
        false
    }

    fn reset_streak(&mut self) {
        self.widen_streak = 0;
        self.widen_candidate = 0;
    }
}

/// Gains are shift counts (`1 >> n` per frame). Fixed-point i64 so every
/// client and the offline harness compute the same due times; no float on a
/// present path. Starting values: proportional time-constant of tens of frames,
/// integral an order slower, cushion a few MADs of residual.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CadenceTuning {
    /// Proportional: `1 >> offset_shift` of the residual per frame.
    pub offset_shift: u8,
    /// Integral on the per-frame rate (skew): `1 >> skew_shift`.
    pub skew_shift: u8,
    /// EMA of |residual|.
    pub jitter_shift: u8,
    /// One outlier must not yank the estimate.
    pub error_clamp_ns: i64,
    /// Cushion = `mad * cushion_num / cushion_den`, clamped to
    /// `[cushion_floor_ns, frame_interval_ns]`.
    pub cushion_num: u16,
    pub cushion_den: u16,
    pub cushion_floor_ns: i64,
    /// Source-timestamp gap beyond which the loop re-anchors instead of tracking.
    pub reanchor_gap_ns: i64,
}

impl CadenceTuning {
    /// Snap-up onto a display grid already carries ~½ refresh of slack, so the
    /// cushion can be small.
    pub const fn snapping() -> CadenceTuning {
        CadenceTuning {
            offset_shift: 5,
            skew_shift: 10,
            jitter_shift: 5,
            error_clamp_ns: 20_000_000,
            cushion_num: 2,
            cushion_den: 1,
            cushion_floor_ns: 500_000,
            reanchor_gap_ns: 500_000_000,
        }
    }

    /// Presenting at the due time directly (VRR, scanout): no implicit slack,
    /// so the cushion must cover more of the distribution on its own.
    pub const fn free_running() -> CadenceTuning {
        CadenceTuning {
            cushion_num: 3,
            cushion_floor_ns: 2_000_000,
            ..CadenceTuning::snapping()
        }
    }

    /// The latency intent on a measured VRR panel: the panel shows every millisecond of
    /// arrival jitter, so a small cushion buys evenness. One MAD, floor 0.5 ms.
    pub const fn vrr_latency() -> CadenceTuning {
        CadenceTuning {
            cushion_num: 1,
            ..CadenceTuning::snapping()
        }
    }
}

/// No residual percentiles: allocation-free, no histogram. Distributions live
/// on the client judder metric.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CadenceHealth {
    /// Since last [`CadenceClock::reset`].
    pub frames: u64,
    /// Due time already past when the frame became presentable — cushion too small.
    pub late: u64,
    /// Gave up tracking (gap, regression, or explicit reset).
    pub reanchors: u64,
    pub offset_ns: i64,
    pub skew_ns: i64,
    pub jitter_ns: i64,
    pub cushion_ns: i64,
}

/// Type-2 playout: due time is `src_pts + offset + cushion` on the source
/// cadence, not the arrival instant.
///
/// Tracks offset *and* per-frame rate because two free-running crystals produce
/// a ramp a proportional-only loop lags forever. Smooths the offset, never the
/// timestamps — genuine source variation passes through; only `ready − pts` is
/// filtered. Due times more even than the source is a bug
/// (`preserves_source_cadence`). A constant clock-domain offset is absorbed;
/// suspend/resume is a discontinuity — call [`reset`](Self::reset).
#[derive(Debug, Clone)]
pub struct CadenceClock {
    tuning: CadenceTuning,
    /// `ready − src_pts`, smoothed. Absorbs the clock-domain constant.
    offset_ns: i64,
    /// Per-frame drift of that offset — the integral term.
    skew_ns: i64,
    /// EMA of |residual|, the cushion's input.
    mad_ns: i64,
    /// `None` until the first sample anchors the loop.
    last_pts_ns: Option<u64>,
    /// Last interval seen, so [`cushion_ns`](Self::cushion_ns) can apply its ceiling.
    frame_interval_ns: i64,
    health: CadenceHealth,
}

impl CadenceClock {
    pub fn new(tuning: CadenceTuning) -> CadenceClock {
        CadenceClock {
            tuning,
            offset_ns: 0,
            skew_ns: 0,
            mad_ns: 0,
            last_pts_ns: None,
            frame_interval_ns: 0,
            health: CadenceHealth::default(),
        }
    }

    /// Re-anchor on the next sample. Call on known discontinuities: codec
    /// rebuild, surface recreate, jump-to-live, resume.
    pub fn reset(&mut self) {
        self.last_pts_ns = None;
        self.skew_ns = 0;
        // `mad_ns` survives: it describes the link, not the stream. Collapsing
        // the cushion to its floor on every rebuild presents late for hundreds
        // of frames — the failure the cushion exists to prevent.
    }

    /// Switch cushion policy mid-stream: re-anchors like [`reset`](Self::reset), and
    /// the measured jitter survives for the same reason.
    pub fn retune(&mut self, tuning: CadenceTuning) {
        self.tuning = tuning;
        self.reset();
    }

    /// Due time in the present-clock domain. May be earlier than `ready_ns`
    /// (late): present at the next opportunity; do not clamp to now — that
    /// would turn every late frame into a fresh anchor.
    ///
    /// `frame_interval_ns` is the nominal source interval and the cushion ceiling.
    pub fn due_ns(&mut self, src_pts_ns: u64, ready_ns: i64, frame_interval_ns: i64) -> i64 {
        self.frame_interval_ns = frame_interval_ns;
        self.health.frames += 1;
        let pts = src_pts_ns as i64;
        let raw = ready_ns - pts;

        let anchored = match self.last_pts_ns {
            // PTS went backwards, or the gap is too long to have tracked:
            // re-anchor rather than slew for seconds.
            Some(last)
                if src_pts_ns < last || src_pts_ns - last > self.tuning.reanchor_gap_ns as u64 =>
            {
                false
            }
            Some(_) => true,
            None => false,
        };
        if anchored {
            self.offset_ns = self.offset_ns.saturating_add(self.skew_ns);
            let err = (raw - self.offset_ns)
                .clamp(-self.tuning.error_clamp_ns, self.tuning.error_clamp_ns);
            self.offset_ns = self
                .offset_ns
                .saturating_add(shr_toward_zero(err, self.tuning.offset_shift));
            self.skew_ns = self
                .skew_ns
                .saturating_add(shr_toward_zero(err, self.tuning.skew_shift));
            let dev = err.abs() - self.mad_ns;
            self.mad_ns += shr_toward_zero(dev, self.tuning.jitter_shift);
        } else {
            self.offset_ns = raw;
            self.skew_ns = 0;
            self.health.reanchors += 1;
        }
        self.last_pts_ns = Some(src_pts_ns);

        let due = pts
            .saturating_add(self.offset_ns)
            .saturating_add(self.cushion_ns());
        if due < ready_ns {
            self.health.late += 1;
        }
        self.publish();
        due
    }

    /// Timestamp not on the source cadence (host re-anchor, plausibility "now").
    /// Folding it would drag the offset toward now while the stream is idle —
    /// when the estimate matters most. Returns a due time from the current
    /// estimate; offset, skew, and jitter stay put.
    pub fn note_off_cadence(&mut self, ready_ns: i64, frame_interval_ns: i64) -> i64 {
        self.frame_interval_ns = frame_interval_ns;
        ready_ns.saturating_add(self.cushion_ns())
    }

    pub fn jitter_ns(&self) -> i64 {
        self.mad_ns
    }

    /// Ceiling of one frame interval is an invariant, not a tunable: past a
    /// whole frame the source cannot supply that smoothness — deepen the buffer.
    pub fn cushion_ns(&self) -> i64 {
        let den = self.tuning.cushion_den.max(1) as i64;
        let want = self.mad_ns.saturating_mul(self.tuning.cushion_num as i64) / den;
        let ceiling = if self.frame_interval_ns > 0 {
            self.frame_interval_ns
        } else {
            i64::MAX
        };
        want.clamp(self.tuning.cushion_floor_ns.min(ceiling), ceiling)
    }

    pub fn health(&self) -> CadenceHealth {
        let mut h = self.health;
        h.offset_ns = self.offset_ns;
        h.skew_ns = self.skew_ns;
        h.jitter_ns = self.mad_ns;
        h.cushion_ns = self.cushion_ns();
        h
    }

    fn publish(&mut self) {
        self.health.offset_ns = self.offset_ns;
        self.health.skew_ns = self.skew_ns;
        self.health.jitter_ns = self.mad_ns;
    }
}

/// Shift toward zero. Plain `>>` rounds toward −∞ and would bias a loop that
/// lives within a few nanoseconds of zero error.
const fn shr_toward_zero(v: i64, shift: u8) -> i64 {
    if v < 0 {
        -((-v) >> shift)
    } else {
        v >> shift
    }
}

/// Lead samples beyond this are a stalled compositor, not a work duration.
const LEAD_MAX_NS: i64 = 50_000_000;

/// Present-phased vsync grid plus the lead the compositor needs ahead of it.
///
/// Phase is the last real present (a present fence), never a latch: the latch
/// is the compositor's wakeup, one work duration before the vsync, and that
/// duration differs per device. The period is the panel's as the compositor
/// states it ([`SlotClock::set_period`]); presents may narrow it, never widen
/// it — a source below the panel rate presents on every Nth vsync and must
/// not teach a slower panel. Slots are integer indices on the grid; a
/// presenter that targets `present_ns(slot) − period/2` is ready for `slot`
/// and not for `slot − 1` with half a period of tolerance either side.
/// `lead_ns` is a decaying maximum of present − latch, the apply-ahead a slot
/// needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotClock {
    period_ns: i64,
    phase_ns: i64,
    phase_slot: i64,
    lead_ns: i64,
}

impl SlotClock {
    /// `panel_hz` seeds the period; 0 leaves it to the first present spacing.
    pub fn seeded(panel_hz: i32) -> SlotClock {
        SlotClock {
            period_ns: if panel_hz > 0 {
                1_000_000_000 / i64::from(panel_hz)
            } else {
                0
            },
            phase_ns: 0,
            phase_slot: 0,
            lead_ns: 0,
        }
    }

    /// A present has been observed; slot arithmetic is meaningful.
    pub fn phased(&self) -> bool {
        self.phase_ns > 0 && self.period_ns > 0
    }

    pub fn period_ns(&self) -> i64 {
        self.period_ns
    }

    pub fn lead_ns(&self) -> i64 {
        self.lead_ns
    }

    /// The panel period as the compositor states it. Slots re-index from the
    /// last present; an implausible value is ignored.
    pub fn set_period(&mut self, period_ns: i64) {
        if PANEL_PERIOD_RANGE_NS.contains(&period_ns) {
            self.period_ns = period_ns;
        }
    }

    /// One real present and the latch that produced it. Returns the slot the
    /// present landed on; the grid re-phases onto it. A spacing under nine
    /// tenths of the period is the panel running faster than stated and
    /// narrows it; anything closer is fence jitter (250 µs on the A024).
    pub fn observe(&mut self, present_ns: i64, latch_ns: i64) -> i64 {
        let sample = present_ns - latch_ns;
        if (0..=LEAD_MAX_NS).contains(&sample) {
            if sample > self.lead_ns {
                self.lead_ns = sample;
            } else {
                self.lead_ns -= (self.lead_ns - sample) >> 4;
            }
        }
        let spacing = present_ns - self.phase_ns;
        if self.phase_ns > 0
            && PANEL_PERIOD_RANGE_NS.contains(&spacing)
            && (self.period_ns == 0 || spacing < self.period_ns / 10 * 9)
        {
            self.period_ns = spacing;
        }
        if !self.phased() {
            self.phase_ns = present_ns;
            self.phase_slot = 0;
            return 0;
        }
        let slot = self.slot_nearest(present_ns);
        self.phase_ns = present_ns;
        self.phase_slot = slot;
        slot
    }

    /// First slot strictly after `t_ns`.
    pub fn slot_after(&self, t_ns: i64) -> i64 {
        self.phase_slot + (t_ns - self.phase_ns).div_euclid(self.period_ns()) + 1
    }

    /// The slot nearest `t_ns`.
    pub fn slot_nearest(&self, t_ns: i64) -> i64 {
        let p = self.period_ns();
        self.phase_slot + (t_ns - self.phase_ns + p / 2).div_euclid(p)
    }

    pub fn present_ns(&self, slot: i64) -> i64 {
        self.phase_ns + (slot - self.phase_slot) * self.period_ns()
    }
}

/// The slot a frame presents on: never the previous frame's, never more than
/// `max_ahead` beyond the first reachable one. Beyond that bound the frame
/// re-targets the held slot and the compositor keeps the newer of the two.
pub fn pace_slot(reachable: i64, last_slot: Option<i64>, max_ahead: i64) -> i64 {
    let floor = last_slot.map_or(reachable, |l| l + 1);
    reachable.max(floor).min(reachable + max_ahead.max(0))
}

/// Present spacings in whole slots. The mode is the cadence ratio (2 for
/// 60-on-120); everything off the mode is judder. Bucket 0 is two frames on one
/// vsync — the compositor kept only one of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotIntervals {
    pub n0: u32,
    pub n1: u32,
    pub n2: u32,
    pub n3p: u32,
}

impl SlotIntervals {
    pub fn note(&mut self, slots: i64) {
        match slots {
            i64::MIN..=0 => self.n0 += 1,
            1 => self.n1 += 1,
            2 => self.n2 += 1,
            _ => self.n3p += 1,
        }
    }

    pub fn total(&self) -> u32 {
        self.n0 + self.n1 + self.n2 + self.n3p
    }

    /// The bucket with the most intervals, in slots (3 stands for 3+).
    pub fn mode(&self) -> i64 {
        let m = self.n1.max(self.n2).max(self.n3p);
        if m == 0 {
            return 0;
        }
        if self.n1 == m {
            1
        } else if self.n2 == m {
            2
        } else {
            3
        }
    }

    /// Off-mode intervals per thousand. Bucket 0 is never the mode.
    pub fn judder_permille(&self) -> u32 {
        let total = self.total();
        if total == 0 {
            return 0;
        }
        let on_mode = match self.mode() {
            1 => self.n1,
            2 => self.n2,
            3 => self.n3p,
            _ => 0,
        };
        ((total - on_mode) as u64 * 1000 / total as u64) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: i64 = 8_333_333; // 120 Hz in ns

    /// Realtime PTS vs monotonic present clock. The loop is not told the offset.
    const PTS0: u64 = 1_786_000_000_000_000_000;
    const DOMAIN: i64 = -1_785_000_000_000_000_000;
    /// Transport + decode: `ready − pts` once the domain is taken out.
    const DELAY: i64 = 12_000_000;

    /// Deterministic LCG in ±spread around zero — no OS randomness in tests.
    struct Lcg(u64);
    impl Lcg {
        fn noise(&mut self, spread_ns: i64) -> i64 {
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

    fn pts_at(k: i64) -> u64 {
        (PTS0 as i64 + k * P) as u64
    }

    fn settled(n: i64, spread_ns: i64) -> CadenceClock {
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let mut rng = Lcg(7);
        for k in 0..n {
            let ready = pts_at(k) as i64 + DOMAIN + DELAY + rng.noise(spread_ns);
            c.due_ns(pts_at(k), ready, P);
        }
        c
    }

    #[test]
    fn settles_from_cold() {
        let c = settled(400, 1_000_000);
        let err = c.health().offset_ns - (DOMAIN + DELAY);
        assert!(
            err.abs() < 500_000,
            "offset must converge on the true transport delay, off by {err} ns"
        );
        assert_eq!(c.health().reanchors, 1, "only the cold start anchors");
    }

    /// Type-2 vs its own type-1 twin: crystals produce a ramp; P-only lags forever.
    #[test]
    fn tracks_a_clock_ramp() {
        const RAMP: i64 = 400; // ns per frame ≈ 48 ppm, an ordinary crystal pair
        let run = |tuning: CadenceTuning| -> i64 {
            let mut c = CadenceClock::new(tuning);
            let mut last_err = 0;
            for k in 0..4_000i64 {
                let ready = pts_at(k) as i64 + DOMAIN + DELAY + k * RAMP;
                c.due_ns(pts_at(k), ready, P);
                last_err = (ready - pts_at(k) as i64) - c.health().offset_ns;
            }
            last_err.abs()
        };
        let type2 = run(CadenceTuning::snapping());
        // skew_shift 63 truncates every residual to 0 — proportional-only.
        let type1 = run(CadenceTuning {
            skew_shift: 63,
            ..CadenceTuning::snapping()
        });
        assert!(
            type2 * 4 < type1,
            "a rate term must beat proportional-only on a ramp: {type2} ns vs {type1} ns"
        );
        assert!(type2 < 3_000, "steady-state ramp error {type2} ns");
    }

    #[test]
    fn rejects_a_single_outlier() {
        let mut c = settled(400, 200_000);
        let before = c.health().offset_ns;
        // Half a second late: a stall, not a new operating point.
        let k = 400;
        c.due_ns(
            pts_at(k),
            pts_at(k) as i64 + DOMAIN + DELAY + 500_000_000,
            P,
        );
        let moved = (c.health().offset_ns - before).abs();
        // Clamp plus one frame of rate advance — the advance is the estimate
        // working, not the outlier.
        let t = CadenceTuning::snapping();
        let bound = (t.error_clamp_ns >> t.offset_shift) + c.health().skew_ns.abs();
        assert!(
            moved <= bound,
            "one outlier moved the estimate {moved} ns, past the clamp's {bound} ns"
        );
    }

    #[test]
    fn reanchors_on_a_gap() {
        let mut c = settled(400, 200_000);
        let anchors = c.health().reanchors;
        // Two-second pause: the estimate cannot have tracked across it.
        let far = pts_at(400) + 2_000_000_000;
        let ready = far as i64 + DOMAIN + DELAY + 4_000_000;
        c.due_ns(far, ready, P);
        assert_eq!(c.health().reanchors, anchors + 1);
        assert_eq!(
            c.health().offset_ns,
            ready - far as i64,
            "a re-anchor adopts the new sample outright rather than slewing to it"
        );
    }

    #[test]
    fn reanchors_on_regression() {
        let mut c = settled(400, 200_000);
        let anchors = c.health().reanchors;
        let back = pts_at(200); // source timestamps went backwards
        c.due_ns(back, back as i64 + DOMAIN + DELAY, P);
        assert_eq!(c.health().reanchors, anchors + 1);
    }

    /// Past due is returned as-is. Clamping to `ready_ns` would turn every late
    /// frame into a fresh anchor — arrival-driven presentation.
    #[test]
    fn late_frame_returns_past_due() {
        let mut c = settled(400, 200_000);
        let k = 400;
        let ready = pts_at(k) as i64 + DOMAIN + DELAY + 30_000_000; // 30 ms late
        let due = c.due_ns(pts_at(k), ready, P);
        assert!(
            due < ready,
            "a frame that arrived 30 ms late must read as already due"
        );
        assert_eq!(c.health().late, 1);
    }

    #[test]
    fn retune_keeps_the_measured_jitter() {
        let mut c = settled(400, 500_000);
        let before = c.health();
        c.retune(CadenceTuning::vrr_latency());
        let after = c.health();
        assert_eq!(after.jitter_ns, before.jitter_ns);
        assert_eq!(after.frames, before.frames);
    }

    #[test]
    fn off_cadence_does_not_move_the_loop() {
        let mut c = settled(400, 500_000);
        let before = c.health();
        let due = c.note_off_cadence(1_000_000, P);
        let after = c.health();
        assert_eq!(before.offset_ns, after.offset_ns);
        assert_eq!(before.skew_ns, after.skew_ns);
        assert_eq!(before.jitter_ns, after.jitter_ns);
        assert_eq!(
            before.frames, after.frames,
            "and it is not a cadence sample"
        );
        assert_eq!(due, 1_000_000 + c.cushion_ns());
    }

    /// Shift the present-side trace by a constant: every due time moves by
    /// exactly that constant. Callers feed one domain; no conversion in-path.
    #[test]
    fn domain_offset_is_absorbed() {
        const SHIFT: i64 = 987_654_321_000;
        let run = |extra: i64| -> Vec<i64> {
            let mut c = CadenceClock::new(CadenceTuning::snapping());
            let mut rng = Lcg(11);
            (0..300i64)
                .map(|k| {
                    let ready = pts_at(k) as i64 + DOMAIN + DELAY + extra + rng.noise(2_000_000);
                    c.due_ns(pts_at(k), ready, P)
                })
                .collect()
        };
        let a = run(0);
        let b = run(SHIFT);
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert_eq!(y - x, SHIFT, "frame {i} shifted by {} not {SHIFT}", y - x);
        }
    }

    /// Irregular source cadence is reproduced, not evened out. More-uniform due
    /// spacings than the source would be a bug.
    #[test]
    fn preserves_source_cadence() {
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let spacings: Vec<i64> = (0..300)
            .map(|k| if k % 2 == 0 { P / 2 } else { P * 3 / 2 })
            .collect();
        let mut pts = PTS0;
        let mut dues = Vec::new();
        let mut ptss = Vec::new();
        let mut rng = Lcg(13);
        for &s in &spacings {
            pts = (pts as i64 + s) as u64;
            let ready = pts as i64 + DOMAIN + DELAY + rng.noise(500_000);
            ptss.push(pts as i64);
            dues.push(c.due_ns(pts, ready, P));
        }
        // Compare the back half, once the loop has settled.
        for i in 200..dues.len() {
            let d_due = dues[i] - dues[i - 1];
            let d_pts = ptss[i] - ptss[i - 1];
            assert!(
                (d_due - d_pts).abs() < 200_000,
                "due spacing {d_due} must follow the source's {d_pts}"
            );
        }
    }

    #[test]
    fn cushion_respects_ceiling() {
        let mut c = CadenceClock::new(CadenceTuning::free_running());
        let mut rng = Lcg(17);
        // Jitter wider than a frame: cushion must still never exceed one interval.
        for k in 0..500i64 {
            let ready = pts_at(k) as i64 + DOMAIN + DELAY + rng.noise(40_000_000);
            c.due_ns(pts_at(k), ready, P);
            assert!(
                c.cushion_ns() <= P,
                "cushion {} ns exceeded the frame interval",
                c.cushion_ns()
            );
        }
        assert!(
            c.jitter_ns() > P,
            "the harness must actually have stressed it"
        );
    }

    // Import the real type — a paraphrase once "confirmed" a non-bug.

    /// Round consecutive present spacings to whole panel periods; ‰ that are
    /// not the modal count.
    fn judder_permille(presents: &[i64], panel_ns: i64) -> u32 {
        let counts: Vec<i64> = presents
            .windows(2)
            .map(|w| (w[1] - w[0] + panel_ns / 2) / panel_ns)
            .collect();
        if counts.is_empty() {
            return 0;
        }
        let mode = *counts
            .iter()
            .max_by_key(|c| counts.iter().filter(|x| x == c).count())
            .unwrap();
        let off = counts.iter().filter(|c| **c != mode).count();
        (off * 1000 / counts.len()) as u32
    }

    /// Map target instants onto the refreshes a frame actually presents on.
    ///
    /// Newest-wins: a claimed slot is replaced, not delayed. Delaying ratchets —
    /// one clamp puts the sequence permanently ahead and every later frame
    /// clamps too, scoring zero judder for both rules.
    fn present_slots(targets: &[i64], panel_ns: i64) -> Vec<i64> {
        let mut out: Vec<i64> = Vec::new();
        for &t in targets {
            let slot = (t + panel_ns - 1) / panel_ns * panel_ns;
            if out.last().is_none_or(|&last| slot > last) {
                out.push(slot);
            }
        }
        out
    }

    #[test]
    fn the_clock_beats_arrival_presentation_on_a_jittery_link() {
        // ±6 ms, comparable to an 8.33 ms period. Narrower jitter is what
        // snapping absorbs on its own, so it would not stress the clock.
        const JITTER: i64 = 6_000_000;
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let mut rng = Lcg(23);
        let (mut arrival, mut cadence) = (Vec::new(), Vec::new());
        for k in 0..1_200i64 {
            let ready = pts_at(k) as i64 + DOMAIN + DELAY + rng.noise(JITTER);
            let due = c.due_ns(pts_at(k), ready, P);
            if k > 200 {
                // Arrival: present when decoded. Clock: present at due, never before ready.
                arrival.push(ready);
                cadence.push(ready.max(due));
            }
        }
        let ja = judder_permille(&present_slots(&arrival, P), P);
        let jc = judder_permille(&present_slots(&cadence, P), P);
        println!(
            "sim: arrival {ja}‰ → cadence {jc}‰ (cushion {} ns)",
            c.cushion_ns()
        );
        assert!(
            jc < ja / 2,
            "source-timestamp playout must materially beat arrival: {jc}‰ vs {ja}‰"
        );
    }

    #[test]
    fn the_sim_does_not_reward_flattening_a_variable_source() {
        let mut c = CadenceClock::new(CadenceTuning::snapping());
        let mut rng = Lcg(29);
        let mut pts = PTS0;
        let (mut dues, mut ptss) = (Vec::new(), Vec::new());
        for k in 0..600i64 {
            // A renderer alternating 60 and 120 fps work — real, and not a defect.
            pts = (pts as i64 + if k % 3 == 0 { 2 * P } else { P }) as u64;
            let ready = pts as i64 + DOMAIN + DELAY + rng.noise(500_000);
            ptss.push(pts as i64);
            dues.push(c.due_ns(pts, ready, P));
        }
        let jd = judder_permille(&dues[300..], P);
        let jp = judder_permille(&ptss[300..], P);
        assert_eq!(
            jd, jp,
            "the due-time cadence must score exactly what the source's own does"
        );
    }
}

#[cfg(test)]
mod panel_grid_tests {
    use super::*;

    const P120: i64 = 8_333_333;
    const P60: i64 = 16_666_666;

    #[test]
    fn seeds_from_the_mode_and_reports_unknown_without_one() {
        assert_eq!(PanelGrid::seeded(120).period_ns(), 8_333_333);
        assert_eq!(PanelGrid::seeded(0).period_ns(), 0);
        let mut g = PanelGrid::seeded(0);
        assert!(
            g.observe(P120),
            "the first plausible sample sets an unseeded grid"
        );
        assert_eq!(g.period_ns(), P120);
    }

    #[test]
    fn narrows_immediately_when_the_panel_is_faster_than_the_mode_said() {
        // The down-rate case: the mode table read 60, the timelines run at 120.
        let mut g = PanelGrid::seeded(60);
        assert!(g.observe(P120));
        assert_eq!(g.period_ns(), P120, "a finer real grid is adopted at once");
    }

    /// Requested 120 Hz, panel still 60: must widen back. A finer-only learner
    /// would pace a 60 Hz panel on an 8.33 ms grid with no way back.
    #[test]
    fn widens_back_out_when_the_requested_mode_was_refused() {
        let mut g = PanelGrid::seeded(120);
        for i in 0..PANEL_WIDEN_STREAK - 1 {
            assert!(!g.observe(P60), "sample {i} must not widen on its own");
            assert_eq!(g.period_ns(), P120);
        }
        assert!(
            g.observe(P60),
            "a sustained run of wider spacings widens the grid"
        );
        assert_eq!(g.period_ns(), P60);
    }

    #[test]
    fn one_stray_wide_sample_never_widens() {
        let mut g = PanelGrid::seeded(120);
        for _ in 0..40 {
            assert!(!g.observe(P60));
            assert!(!g.observe(P120)); // an agreeing sample breaks the run
        }
        assert_eq!(
            g.period_ns(),
            P120,
            "alternating samples must not accumulate"
        );
    }

    #[test]
    fn widening_adopts_the_narrowest_of_the_run() {
        let mut g = PanelGrid::seeded(120);
        let run = [
            P60,
            33_000_000,
            P60 + 400_000,
            41_000_000,
            P60,
            P60,
            P60,
            P60,
        ];
        for s in run {
            g.observe(s);
        }
        assert_eq!(
            g.period_ns(),
            P60,
            "the estimate takes the narrowest of the run, never an outlier"
        );
    }

    #[test]
    fn implausible_spacings_are_ignored_entirely() {
        let mut g = PanelGrid::seeded(120);
        for _ in 0..100 {
            assert!(!g.observe(0));
            assert!(!g.observe(-1));
            assert!(!g.observe(1_000_000)); // 1000 Hz — below the range floor
            assert!(!g.observe(100_000_000)); // 10 Hz — above the ceiling
        }
        assert_eq!(g.period_ns(), P120);
    }

    #[test]
    fn a_transient_narrow_glitch_self_heals() {
        // Narrowing is immediate, so a glitch poisons the estimate; widening
        // must still recover.
        let mut g = PanelGrid::seeded(120);
        assert!(g.observe(2_100_000), "a glitch narrows the estimate");
        assert_eq!(g.period_ns(), 2_100_000);
        for _ in 0..PANEL_WIDEN_STREAK {
            g.observe(P120);
        }
        assert_eq!(g.period_ns(), P120, "and the real grid wins it back");
    }
}

#[cfg(test)]
mod slot_clock_tests {
    use super::*;

    const P: i64 = 8_333_333;
    const LEAD: i64 = 4_000_000;

    fn phased() -> SlotClock {
        let mut c = SlotClock::seeded(120);
        c.observe(1_000_000_000, 1_000_000_000 - LEAD);
        c
    }

    #[test]
    fn first_present_phases_the_grid_on_slot_zero() {
        let mut c = SlotClock::seeded(120);
        assert!(!c.phased());
        assert_eq!(c.observe(1_000_000_000, 1_000_000_000 - LEAD), 0);
        assert!(c.phased());
        assert_eq!(c.lead_ns(), LEAD);
        assert_eq!(c.present_ns(3), 1_000_000_000 + 3 * P);
    }

    #[test]
    fn slot_arithmetic_is_strict_after_and_nearest() {
        let c = phased();
        let v0 = 1_000_000_000;
        assert_eq!(c.slot_after(v0), 1, "the phase instant itself is not after");
        assert_eq!(c.slot_after(v0 + 1), 1);
        assert_eq!(c.slot_after(v0 + P - 1), 1);
        assert_eq!(c.slot_after(v0 + P), 2);
        assert_eq!(c.slot_after(v0 - 1), 0);
        assert_eq!(c.slot_nearest(v0 + P / 2 - 1), 0);
        assert_eq!(c.slot_nearest(v0 + P / 2 + 1), 1);
        assert_eq!(c.slot_nearest(v0 - P + 100), -1);
    }

    #[test]
    fn consecutive_presents_walk_the_slots_and_rephase() {
        let mut c = phased();
        let v0 = 1_000_000_000;
        assert_eq!(c.observe(v0 + P + 40_000, v0 + P + 40_000 - LEAD), 1);
        assert_eq!(
            c.observe(v0 + 2 * P - 30_000, v0 + 2 * P - 30_000 - LEAD),
            2
        );
        // Re-phased onto the last present: the next slot is one period from it.
        assert_eq!(c.present_ns(3), v0 + 3 * P - 30_000);
        assert_eq!(c.slot_after(v0 + 2 * P), 3);
    }

    #[test]
    fn a_half_rate_source_never_widens_the_grid() {
        let mut c = SlotClock::seeded(120);
        c.observe(1_000_000_000, 1_000_000_000 - LEAD);
        let mut t = 1_000_000_000;
        for i in 1..=240 {
            t += 2 * P;
            assert_eq!(c.observe(t, t - LEAD), 2 * i);
        }
        assert_eq!(c.period_ns(), P, "two seconds at 60 fps keep the 120 grid");
    }

    #[test]
    fn the_compositor_sets_the_period_and_a_faster_present_narrows_it() {
        let mut c = phased();
        let t = 1_000_000_000;
        c.set_period(2 * P);
        assert_eq!(c.period_ns(), 2 * P, "a 60 Hz panel is taken as stated");
        assert_eq!(c.slot_after(t), 1);
        assert_eq!(c.present_ns(1), t + 2 * P);
        c.set_period(1);
        assert_eq!(c.period_ns(), 2 * P, "an implausible period is ignored");
        assert_eq!(c.observe(t + P, t + P - LEAD), 1);
        assert_eq!(
            c.period_ns(),
            P,
            "one present a 120 Hz period on narrows the grid"
        );
    }

    #[test]
    fn lead_is_a_decaying_maximum() {
        let mut c = phased();
        let v = 1_000_000_000;
        c.observe(v + P, v + P - 6_000_000);
        assert_eq!(c.lead_ns(), 6_000_000, "a larger sample is adopted at once");
        for i in 2..40 {
            c.observe(v + i * P, v + i * P - LEAD);
        }
        assert!(c.lead_ns() < 4_500_000 && c.lead_ns() >= LEAD);
        c.observe(v + 40 * P, v + 40 * P - LEAD_MAX_NS - 1);
        assert!(c.lead_ns() < 4_500_000, "a stalled sample is ignored");
    }

    #[test]
    fn pace_slot_never_shares_a_slot_and_never_holds_past_the_bound() {
        assert_eq!(pace_slot(10, None, 1), 10);
        assert_eq!(
            pace_slot(10, Some(7), 1),
            10,
            "after a gap: as soon as possible"
        );
        assert_eq!(
            pace_slot(10, Some(10), 1),
            11,
            "an early frame takes the next slot"
        );
        assert_eq!(
            pace_slot(10, Some(11), 1),
            11,
            "past the bound it re-targets the held slot"
        );
        assert_eq!(
            pace_slot(10, Some(10), 0),
            10,
            "no pacing: today's collision"
        );
        assert_eq!(pace_slot(10, Some(12), 3), 13);
    }

    #[test]
    fn intervals_judder_is_the_off_mode_fraction() {
        let mut h = SlotIntervals::default();
        for _ in 0..117 {
            h.note(1);
        }
        h.note(0);
        h.note(2);
        h.note(3);
        assert_eq!(h.mode(), 1);
        assert_eq!(h.total(), 120);
        assert_eq!(h.judder_permille(), 25);
        let mut h = SlotIntervals::default();
        for _ in 0..60 {
            h.note(2);
        }
        assert_eq!(h.mode(), 2);
        assert_eq!(h.judder_permille(), 0, "60-on-120 at 2,2,2 is not judder");
        assert_eq!(SlotIntervals::default().judder_permille(), 0);
    }
}
