//! Pacing for a virtual display that ticks faster than its stream: the credit the encode thread
//! spends per frame, and the timestamp correction for the panel's tick. Plain arithmetic in any
//! clock's ticks: QPC in the Windows driver, nanoseconds in the Linux host.

/// A credit bucket at the stream rate. Each encoded frame spends one period; credit refills with
/// time up to two periods. A frame that lands one panel tick early still passes, and the long-run
/// rate never exceeds the stream's.
#[derive(Clone, Debug)]
pub struct FrameCredit {
    period: u64,
    credit: u64,
    last: Option<u64>,
}

impl FrameCredit {
    const DEPTH: u64 = 2;

    /// A full bucket for frames `period` ticks apart.
    #[must_use]
    pub fn new(period: u64) -> Self {
        let period = period.max(1);
        Self {
            period,
            credit: period * Self::DEPTH,
            last: None,
        }
    }

    /// Whole frames the credit covers at `now`.
    pub fn frames(&mut self, now: u64) -> usize {
        if let Some(last) = self.last {
            self.credit = self
                .credit
                .saturating_add(now.saturating_sub(last))
                .min(self.period * Self::DEPTH);
        }
        self.last = Some(now);
        (self.credit / self.period) as usize
    }

    /// One frame encoded.
    pub fn spend(&mut self) {
        self.credit = self.credit.saturating_sub(self.period);
    }

    /// Ticks until a whole frame is covered, as of the last [`Self::frames`].
    #[must_use]
    pub fn wait(&self) -> u64 {
        self.period.saturating_sub(self.credit)
    }
}

/// Timestamps for a panel that ticks every `tick` while frames come slower. The compositor stamps
/// a frame for the tick after it was ready, so its content time lies within one tick of the stamp.
/// Each stamp is moved inside `[stamp - tick, stamp]`, onto the cadence of the frames before it:
/// a capped game comes out evenly spaced, a variable one at its own spacing, and no frame moves by
/// more than a tick.
///
/// The phase moves only when a stamp's window forces it, so window noise never reaches the output.
/// The period learns from those moves and, slowly, from the window centres, which is what keeps a
/// game a hair off the stream rate from drifting across the window and jumping a tick.
#[derive(Clone, Debug)]
pub struct Restamp {
    tick: u64,
    last_raw: u64,
    last_out: u64,
    period: Option<f64>,
    warm_sum: u64,
    warm_n: u32,
}

impl Restamp {
    /// Gaps averaged into the first period estimate.
    const WARM: u32 = 4;
    /// How much of a window-forced move goes into the period.
    const FORCED_GAIN: f64 = 1.0 / 20.0;
    /// How much of the window centre's offset goes into the period each frame.
    const CENTRE_GAIN: f64 = 1.0 / 2048.0;

    #[must_use]
    pub fn new(tick: u64) -> Self {
        Self {
            tick,
            last_raw: 0,
            last_out: 0,
            period: None,
            warm_sum: 0,
            warm_n: 0,
        }
    }

    /// The corrected stamp for a frame stamped `raw`. Zero, no stamp at all, passes through.
    pub fn apply(&mut self, raw: u64) -> u64 {
        if raw == 0 {
            return 0;
        }
        if raw <= self.last_raw || self.last_raw == 0 {
            return self.restart(raw);
        }
        let gap = raw - self.last_raw;
        let Some(period) = self.period else {
            self.warm_sum += gap;
            self.warm_n += 1;
            if self.warm_n == Self::WARM {
                self.period = Some(self.warm_sum as f64 / f64::from(Self::WARM));
            }
            return self.centre(raw);
        };
        // Two and a half frames of nothing is a stall, not tick error.
        if gap as f64 > period * 2.5 {
            return self.restart(raw);
        }
        let (lo, hi) = (raw.saturating_sub(self.tick), raw);
        let predicted = self.last_out as f64 + period;
        let out = (predicted.clamp(lo as f64, hi as f64) as u64).max(self.last_out + 1);
        let centre = (lo + hi) as f64 / 2.0;
        self.period = Some(
            period
                + Self::FORCED_GAIN * (out as f64 - predicted)
                + Self::CENTRE_GAIN * (centre - predicted),
        );
        self.last_raw = raw;
        self.last_out = out;
        out
    }

    fn restart(&mut self, raw: u64) -> u64 {
        self.period = None;
        self.warm_sum = 0;
        self.warm_n = 0;
        self.centre(raw)
    }

    /// The middle of the window: the best guess before there is a cadence.
    fn centre(&mut self, raw: u64) -> u64 {
        let out = raw.saturating_sub(self.tick / 2).max(self.last_out + 1);
        self.last_raw = raw;
        self.last_out = out;
        out
    }
}

/// How far each frame's gap moved from the one before it, on two stamps of the same frames: the
/// producer's (`raw`) and the corrected one (`out`). Near zero is an even cadence; a panel tick
/// is jitter the stamps pass on.
#[derive(Clone, Debug, Default)]
pub struct GapChange {
    last: Option<(u64, u64)>,
    gaps: Option<(u64, u64)>,
    n: u64,
    raw: u64,
    out: u64,
}

impl GapChange {
    /// One frame's two stamps.
    pub fn note(&mut self, raw: u64, out: u64) {
        let gaps = match self.last.replace((raw, out)) {
            Some((r, o)) if raw > r && out > o => Some((raw - r, out - o)),
            _ => None,
        };
        if let (Some((rg, og)), Some((last_rg, last_og))) = (gaps, self.gaps) {
            self.n += 1;
            self.raw += rg.abs_diff(last_rg);
            self.out += og.abs_diff(last_og);
        }
        self.gaps = gaps;
    }

    /// The mean change per frame, `(raw, out)` in ticks, since the last take; the stamps carry on.
    pub fn take(&mut self) -> (u64, u64) {
        let n = core::mem::take(&mut self.n).max(1);
        let raw = core::mem::take(&mut self.raw);
        let out = core::mem::take(&mut self.out);
        (raw / n, out / n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream period and half of it as the panel tick, in microseconds.
    const PERIOD: u64 = 16_667;
    const TICK: u64 = 8_333;

    /// Deterministic jitter in `[-spread, spread]`.
    fn jitter(seed: &mut u64, spread: i64) -> i64 {
        *seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        ((*seed >> 33) as i64 % (2 * spread + 1)) - spread
    }

    /// The display stamp for content ready at `t`: the tick after it, plus one tick.
    fn stamp(t: u64) -> u64 {
        t.div_ceil(TICK) * TICK + TICK
    }

    #[test]
    fn credit_passes_the_stream_rate_untouched() {
        let mut c = FrameCredit::new(PERIOD);
        for k in 0..200 {
            assert!(c.frames(1_000_000 + k * PERIOD) >= 1, "frame {k}");
            c.spend();
        }
    }

    #[test]
    fn credit_halves_a_double_rate_source() {
        let mut c = FrameCredit::new(PERIOD);
        let mut taken = 0;
        for k in 0..200 {
            if c.frames(1_000_000 + k * TICK) >= 1 {
                c.spend();
                taken += 1;
            }
        }
        // The full bucket's head start, then one frame per period.
        assert!((100..=103).contains(&taken), "taken {taken}");
    }

    #[test]
    fn credit_passes_a_frame_one_tick_early() {
        let mut c = FrameCredit::new(PERIOD);
        let arrivals = [
            0,
            PERIOD,
            PERIOD + TICK,
            3 * PERIOD,
            4 * PERIOD,
            4 * PERIOD + TICK,
        ];
        for (i, t) in arrivals.iter().enumerate() {
            assert!(c.frames(1_000_000 + t) >= 1, "arrival {i}");
            c.spend();
        }
    }

    #[test]
    fn credit_wait_counts_down_to_the_next_frame() {
        let mut c = FrameCredit::new(PERIOD);
        c.frames(1_000_000);
        c.spend();
        c.spend();
        assert_eq!(c.frames(1_000_000 + 1_000), 0);
        assert_eq!(c.wait(), PERIOD - 1_000);
    }

    /// Mean spacing error against the jitter-free cadence, raw stamps and corrected ones, for
    /// content every `gap` us with `spread` us of jitter. Every corrected stamp stays in its window.
    fn spacing_error(gap: u64, spread: i64) -> (u64, u64) {
        let mut r = Restamp::new(TICK);
        let mut seed = 7;
        let (mut raw_err, mut out_err) = (0, 0);
        let (mut prev_raw, mut prev_out) = (0, 0);
        for k in 0..2_000u64 {
            // 40 us past a tick, so jitter flips frames across it.
            let ready = (1_000_040 + k * gap).wrapping_add_signed(jitter(&mut seed, spread));
            let raw = stamp(ready);
            let out = r.apply(raw);
            assert!(out <= raw && out + TICK >= raw, "frame {k} left its window");
            if k > 120 {
                raw_err += (raw - prev_raw).abs_diff(gap);
                out_err += (out - prev_out).abs_diff(gap);
            }
            (prev_raw, prev_out) = (raw, out);
        }
        (raw_err / 1_879, out_err / 1_879)
    }

    #[test]
    fn restamp_evens_out_a_capped_game() {
        let (raw, out) = spacing_error(PERIOD, 900);
        assert!(
            raw > 400 && out * 5 < raw,
            "raw {raw} us, corrected {out} us"
        );
    }

    #[test]
    fn restamp_follows_a_game_a_hair_off_the_stream_rate() {
        let (raw, out) = spacing_error(16_683, 900);
        assert!(out * 3 < raw, "raw {raw} us, corrected {out} us");
    }

    #[test]
    fn restamp_keeps_a_variable_rate_at_its_own_spacing() {
        for gap in [14_925, 20_000] {
            let (raw, out) = spacing_error(gap, 300);
            assert!(out * 10 < raw, "{gap}: raw {raw} us, corrected {out} us");
        }
    }

    #[test]
    fn gap_change_reads_zero_for_even_and_a_tick_for_flipped() {
        let mut g = GapChange::default();
        for k in 0..10 {
            // Even corrected stamps, raw ones alternating 1 and 3 ticks apart.
            let raw = k * 2 * TICK + if k % 2 == 1 { TICK } else { 0 };
            g.note(1_000_000 + raw, 1_000_000 + k * 2 * TICK);
        }
        assert_eq!(g.take(), (2 * TICK, 0));
        assert_eq!(g.take(), (0, 0), "a take starts the means over");
    }

    #[test]
    fn restamp_restarts_after_a_stall_and_passes_zero() {
        let mut r = Restamp::new(TICK);
        let mut t = 1_000_000;
        for _ in 0..10 {
            t += 2 * TICK;
            r.apply(t);
        }
        t += 200_000;
        assert_eq!(
            r.apply(t),
            t - TICK / 2,
            "a stall starts over at the window centre"
        );
        assert_eq!(r.apply(0), 0);
        assert!(r.apply(t - 5) > t - TICK / 2, "stamps never go backwards");
    }
}
