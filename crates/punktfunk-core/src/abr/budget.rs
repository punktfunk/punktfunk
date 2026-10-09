//! The wire budget: what an encoder rate costs, and what parity takes out of it.
//!
//! A session's bitrate is its *wire* budget — every byte the media plane puts
//! on the link, headers, seals and FEC parity included. The encoder gets what
//! is left after the audio reservation and the parity share, so adaptive FEC
//! reallocates inside the budget instead of widening it. The host derives the
//! encoder rate from it, the controller judges delivery against it, and the
//! simulator spends it: one arithmetic, one file.

/// What the budget prices around each shard: 18 B over a sealed packet's
/// [`WIRE_OVERHEAD`](crate::packet::WIRE_OVERHEAD). The simulator's scenarios and the
/// controller behaviour they pin are tuned at 64; moving it re-tunes them.
pub const SHARD_WIRE_OVERHEAD: u64 = 64;

/// Floor on an encoder rate. Below this the picture is not worth the packets.
pub const MIN_BITRATE_KBPS: u32 = 500;

/// Adaptive-FEC band. A clean link decays to [`FEC_MIN`]; loss ramps toward
/// [`FEC_MAX`]. 5 % is 4 parity shards on a ~110 KB frame (2 on a 30 KB one) —
/// the burst a clean link still drops. A 1 % floor left one, so the cleanest
/// link lost a frame to two packets. A session opens at
/// [`FEC_ADAPTIVE_START`], before any loss report has sized it.
pub const FEC_MIN: u8 = 5;
pub const FEC_MAX: u8 = 50;
pub const FEC_ADAPTIVE_START: u8 = 10;
/// Points over the measured level while frames die that parity might have caught.
pub const FEC_STEP: u8 = 3;
/// Report windows (~750 ms each) the step gets to prove itself.
pub const FEC_STEP_WINDOWS: u32 = 4;

/// Report windows the loss level is averaged over. One window carries a few
/// hundred packets and a handful of losses, so its own share is noise; over
/// this many the level is within a tenth of the link's and still forgets a
/// loss that stopped inside half a minute.
const LOSS_HORIZON_WINDOWS: u32 = 32;
/// How rarely parity may leave a frame unrepaired: one in this many seconds
/// at the session's refresh. A shorter budget is a glitch every few minutes;
/// a longer one spends picture on loss the link does not have.
const LOSS_BUDGET_SECS: u64 = 600;
/// How much more loss than the level shows must still not want a parity shard
/// back before the rule gives it up. Every move of the target re-derives the
/// encoder rate, and the level's own error over the horizon is under a tenth,
/// so a quarter is outside the noise and a link on a boundary holds one
/// percent instead of sawing across it.
const LOSS_RELEASE_PCT: u32 = 125;
/// The most the measured loss may ask for on its own. What random loss takes
/// from the picture stops at a sixth of the encoder rate; past that the frame
/// is too small for a percent to protect well, and the rate rather than the
/// parity is the session's problem. The band above belongs to [`adapt_fec`]
/// and [`FEC_STEP`], which answer congestion.
const LOSS_PCT_MAX: u8 = 25;

/// Wire budget → encoder rate.
///
/// ```text
/// wire  = video × (payload+64)/payload × (100+fec)/100 + audio
/// video = (wire − audio) × payload/(payload+64) × 100/(100+fec)
/// ```
///
/// More parity means a lower encoder rate, never a fatter wire. Floored at
/// [`MIN_BITRATE_KBPS`]. PyroWave bypasses this (bpp pin, Automatic off).
pub fn encoder_kbps_for_budget(
    budget_kbps: u32,
    audio_kbps: u32,
    fec_percent: u8,
    shard_payload: u16,
) -> u32 {
    let payload = shard_payload.max(1) as u64;
    let video_wire = budget_kbps.saturating_sub(audio_kbps) as u64;
    let video =
        video_wire * payload * 100 / ((payload + SHARD_WIRE_OVERHEAD) * (100 + fec_percent as u64));
    u32::try_from(video)
        .unwrap_or(u32::MAX)
        .max(MIN_BITRATE_KBPS)
}

/// Inverse: the wire spend of an encoder rate. A short apply reports this so
/// the client's climb base tracks wire truth. Rounds up where the derivation
/// rounds down, so a roundtrip never inflates the budget the client believes.
pub fn budget_kbps_for_encoder(
    encoder_kbps: u32,
    audio_kbps: u32,
    fec_percent: u8,
    shard_payload: u16,
) -> u32 {
    let payload = shard_payload.max(1) as u64;
    let wire = encoder_kbps as u64 * (payload + SHARD_WIRE_OVERHEAD) * (100 + fec_percent as u64)
        / (payload * 100);
    u32::try_from(wire.saturating_add(audio_kbps as u64)).unwrap_or(u32::MAX)
}

/// Loss ppm (a feedback window's `loss_ppm`) → recovery %. FEC must exceed the
/// loss it covers, so the target is `loss × 1.4 + 1`, clamped to the band.
/// Clean (≈0 ppm) lands on [`FEC_MIN`].
///
/// Integer: `ceil(ppm/10_000 × 1.4) + 1` is `(ppm × 14).div_ceil(100_000) + 1`.
pub fn adapt_fec(loss_ppm: u32) -> u8 {
    let target = (loss_ppm as u64 * 14).div_ceil(100_000) as u32 + 1;
    target.clamp(FEC_MIN as u32, FEC_MAX as u32) as u8
}

/// The link's loss share as the parity rule reads it: an average over
/// [`LOSS_HORIZON_WINDOWS`], because one window's share is noise.
///
/// A report counts only the shards parity repaired, so this never reads more
/// loss than the link has, and more parity only makes it truer. The level a
/// window sets can therefore not raise the parity that raised the level.
#[derive(Clone, Copy, Debug, Default)]
pub struct LossHorizon {
    /// The level times [`LOSS_HORIZON_WINDOWS`].
    acc: u32,
    /// The percent the rule is holding. `0` before the first window.
    sized: u8,
}

impl LossHorizon {
    /// Fold one report window in and read the level, ppm. A window a frame
    /// died in is a burst or an overload, which parity answers with
    /// [`FEC_STEP`] and never with a wider share: the level holds instead.
    pub fn note(&mut self, loss_ppm: u32, unrecovered: bool) -> u32 {
        if !unrecovered {
            self.acc = self
                .acc
                .saturating_sub(self.acc / LOSS_HORIZON_WINDOWS)
                .saturating_add(loss_ppm.min(1_000_000));
        }
        self.acc / LOSS_HORIZON_WINDOWS
    }

    /// The percent the rule holds for a frame of `shards`. It rises the window
    /// the parity it buys stops covering the frame, and comes down only when
    /// [`LOSS_RELEASE_PCT`] of the level would still not want it back. A
    /// target that moves re-derives the encoder rate, so a steady link has to
    /// see one percent, not the two either side of a boundary.
    fn hold(&mut self, shards: u32, level: u32, fps: u32) -> u8 {
        let held = self.sized.max(FEC_MIN);
        let need = m_needed(shards, level, fps);
        let want = fec_pct_for_loss(shards, level, fps);
        self.sized = if parity_for_pct(shards, held) < need {
            want.max(held)
        } else if want < held
            && m_needed(shards, level.saturating_mul(LOSS_RELEASE_PCT) / 100, fps)
                <= parity_for_pct(shards, want)
        {
            want
        } else {
            held
        };
        self.sized
    }
}

/// What the session puts on the wire, as the parity rule has to read it: the
/// budget it spends, the reservation that is not video, the shard it fills,
/// and the refresh it fills one frame per.
#[derive(Clone, Copy, Debug)]
pub struct FrameBudget {
    pub budget_kbps: u32,
    pub audio_kbps: u32,
    pub shard_payload: u16,
    pub fps: u32,
}

/// Wire shards one frame puts on the link: its whole per-frame spend of the
/// budget, parity included.
///
/// The budget and the refresh fix this, so the percent the rule answers with
/// cannot move what it was sized against — size against the data shards
/// instead and more parity is a smaller frame, which asks a higher percent,
/// which is a smaller frame again. It is also the ordinary frame and not a
/// keyframe: a percent gives a frame four times this size four times the
/// parity, and the same share of loss is a thinner tail on the larger one.
pub fn wire_shards_per_frame(frame: FrameBudget) -> u32 {
    let video_wire = u64::from(frame.budget_kbps.saturating_sub(frame.audio_kbps));
    let bytes = video_wire * 1_000 / 8 / u64::from(frame.fps.max(1));
    let shard = u64::from(frame.shard_payload.max(1)) + SHARD_WIRE_OVERHEAD;
    ((bytes / shard) as u32).max(1)
}

/// Smallest parity `m` that keeps a frame of `shards` wire shards inside the
/// failure budget when each shard is lost on its own with probability
/// `loss_ppm`: `P(X > m) ≤ 1 / (fps × LOSS_BUDGET_SECS)`.
///
/// `P(X ≥ j) ≤ C(n, j) pʲ`, the tail's first term — never under the exact
/// sum, and inside a percent of it wherever the answer is small. Parts per
/// billion in `u128`, stepped by `T(j) = T(j-1) × (n - j + 1) × p / j`, so
/// nothing here meets a float.
fn m_needed(shards: u32, loss_ppm: u32, fps: u32) -> u32 {
    if loss_ppm == 0 || shards == 0 {
        return 0;
    }
    let budget_ppb = 1_000_000_000u128 / (u128::from(fps.max(1)) * u128::from(LOSS_BUDGET_SECS));
    let p = u128::from(loss_ppm);
    let n = u128::from(shards);
    // Past FEC_MAX of the frame the answer is the top of the band whatever
    // this says, so the walk stops there.
    let most = (shards * u32::from(FEC_MAX) / 100 + 1).min(shards);
    let mut tail = 1_000_000_000u128;
    for j in 1..=most {
        tail = tail
            .saturating_mul(n - u128::from(j) + 1)
            .saturating_mul(p)
            .div_ceil(u128::from(j) * 1_000_000);
        if tail <= budget_ppb {
            return j - 1;
        }
    }
    most
}

/// Parity a percent actually buys on a frame of `shards` wire shards: the
/// encoder fills what the percent leaves it, so the data shards are
/// `shards × 100 / (100 + pct)` and the parity rounds up from those.
fn parity_for_pct(shards: u32, pct: u8) -> u32 {
    let data = (u64::from(shards) * 100 / (100 + u64::from(pct))) as u32;
    crate::config::recovery_shards(data as usize, pct) as u32
}

/// Recovery percent the link's loss asks of a frame of `shards` wire shards,
/// sized for one unrepaired frame per [`LOSS_BUDGET_SECS`] at `fps`, and
/// never over [`LOSS_PCT_MAX`].
///
/// Never under [`FEC_MIN`] either: the band and its two-shard floor already
/// cover a clean link, and a link this rule finds nothing to answer is
/// exactly that. A percent, so [`encoder_kbps_for_budget`] and every other
/// consumer sees the whole cost.
pub fn fec_pct_for_loss(shards: u32, loss_ppm: u32, fps: u32) -> u8 {
    let floor = parity_for_pct(shards, FEC_MIN);
    let need = m_needed(shards, loss_ppm, fps);
    if shards == 0 || need <= floor {
        return FEC_MIN;
    }
    let data = shards.saturating_sub(need).max(1);
    let pct = (u64::from(need) * 100)
        .div_ceil(u64::from(data))
        .clamp(u64::from(FEC_MIN), u64::from(LOSS_PCT_MAX)) as u8;
    // A frame small enough that [`LOSS_PCT_MAX`] still buys it no shard the
    // two-shard floor did not already give it: there is nothing here to buy,
    // and the encoder keeps its rate.
    if parity_for_pct(shards, pct) <= floor {
        return FEC_MIN;
    }
    pct
}

/// One window's FEC target. `unrecovered_run` is the consecutive report
/// windows a keyframe ask landed in — frames parity could not repair.
///
/// Three terms, the largest wins: the band [`adapt_fec`] reads off this
/// window, the parity the link's measured loss asks of a frame the size this
/// budget buys, and [`FEC_STEP`] for the first [`FEC_STEP_WINDOWS`] of a run.
/// A run past that is loss no per-frame parity bridges, so the step comes
/// off. Decays one point per window so a burst every few seconds does not
/// fall to the floor between hits.
pub fn fec_target(
    loss_ppm: u32,
    prev: u8,
    unrecovered_run: u32,
    frame: FrameBudget,
    horizon: &mut LossHorizon,
) -> u8 {
    let step = if (1..=FEC_STEP_WINDOWS).contains(&unrecovered_run) {
        FEC_STEP
    } else {
        0
    };
    let level = horizon.note(loss_ppm, unrecovered_run > 0);
    let sized = horizon.hold(wire_shards_per_frame(frame), level, frame.fps);
    adapt_fec(loss_ppm)
        .max(sized)
        .saturating_add(step)
        .min(FEC_MAX)
        .max(prev.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapt_fec_maps_loss_to_recovery_band() {
        // Clean window (0 loss) is FEC_MIN; loss under ~2.8 % clamps to it.
        assert_eq!(adapt_fec(0), FEC_MIN);
        assert_eq!(adapt_fec(1), FEC_MIN);
        // FEC exceeds the loss it covers (×1.4 + 1 pt).
        assert_eq!(adapt_fec(30_000), 6); // 3% → ceil(4.2)+1 = 6
        assert_eq!(adapt_fec(50_000), 8); // 5% → ceil(7)+1 = 8
        assert_eq!(adapt_fec(100_000), 15); // 10% → ceil(14)+1 = 15
        assert_eq!(adapt_fec(1_000_000), FEC_MAX); // 100% → clamped
        assert!(adapt_fec(u32::MAX) <= FEC_MAX);
    }

    /// The integer form is the `f64` one the host shipped, for every loss a
    /// report can carry. Floating point decided a parity percent for three
    /// releases; this pins that nothing moved when it stopped.
    #[test]
    fn the_integer_band_is_the_float_one_it_replaced() {
        let float_form = |loss_ppm: u32| -> u8 {
            let loss_pct = loss_ppm as f64 / 10_000.0;
            let target = (loss_pct * 1.4).ceil() as u32 + 1;
            target.clamp(FEC_MIN as u32, FEC_MAX as u32) as u8
        };
        for ppm in 0..=1_000_000u32 {
            assert_eq!(adapt_fec(ppm), float_form(ppm), "loss_ppm {ppm}");
        }
    }

    /// A 1080p30 tunnel at 8.5 Mbps: the frame the rig measures, 19–30 data
    /// shards. Every test below that names a frame means this one.
    fn tunnel() -> FrameBudget {
        FrameBudget {
            budget_kbps: 8_500,
            audio_kbps: 256,
            shard_payload: 1408,
            fps: 30,
        }
    }

    /// A frame dying every window under low measured loss is a bounded step over the
    /// measured level, never a 5 % reading: on for a few windows, off once it has not
    /// stopped the asks, and a clean window ends the run.
    #[test]
    fn fec_step_is_bounded_and_gives_up_when_frames_keep_dying() {
        // Clean: measured level, decaying one point per window.
        let mut h = LossHorizon::default();
        assert_eq!(fec_target(0, FEC_MIN, 0, tunnel(), &mut h), FEC_MIN);
        assert_eq!(fec_target(0, 12, 0, tunnel(), &mut h), 11);
        // A dropped frame at 0.3 % loss: +3 over the floor, held while the run
        // is young. A window a frame died in teaches the horizon nothing, so
        // the step is the whole of the answer.
        let mut fec = FEC_MIN;
        let mut seen = Vec::new();
        for run in 1..=FEC_STEP_WINDOWS + 2 {
            fec = fec_target(3_000, fec, run, tunnel(), &mut h);
            seen.push(fec);
        }
        assert_eq!(
            seen,
            [8, 8, 8, 8, 7, 6],
            "step, then the decay back to measured"
        );
        // Measured loss still carries its own level once the step is off.
        let mut h = LossHorizon::default();
        assert_eq!(
            fec_target(100_000, FEC_MIN, FEC_STEP_WINDOWS + 1, tunnel(), &mut h),
            15
        );
        // Never past the band.
        assert_eq!(fec_target(1_000_000, FEC_MAX, 1, tunnel(), &mut h), FEC_MAX);
    }

    /// A frame [`LOSS_PCT_MAX`] cannot give another shard to keeps its encoder
    /// rate. Ten wire shards at 1.5 % want three parity; a quarter of the
    /// eight data shards they leave is still the two the floor already gives,
    /// so the rule asks for nothing rather than charging a sixth of the
    /// picture for a shard it never gets.
    #[test]
    fn a_frame_too_small_to_buy_a_shard_is_never_charged_for_one() {
        for (n, ppm, fps) in [
            (6u32, 15_000u32, 30u32),
            (6, 15_000, 165),
            (4, 70_000, 60),
            (8, 15_000, 30),
            (10, 15_000, 165),
        ] {
            assert_eq!(
                fec_pct_for_loss(n, ppm, fps),
                FEC_MIN,
                "{n} shards at {ppm} ppm, {fps} fps"
            );
            assert_eq!(parity_for_pct(n, LOSS_PCT_MAX), parity_for_pct(n, FEC_MIN));
        }
        // Two shards bigger and the ceiling does buy the third, so it is
        // asked for and paid for.
        assert_eq!(fec_pct_for_loss(12, 15_000, 30), LOSS_PCT_MAX);
        assert_eq!(parity_for_pct(12, LOSS_PCT_MAX), 3);
    }

    /// A frame of `n` wire shards at `pct`, through the shipped derivation:
    /// the wire shards it really becomes.
    fn spend(n: u32, fps: u32, pct: u8) -> u32 {
        let payload = 1408u16;
        let wire = u64::from(payload) + SHARD_WIRE_OVERHEAD;
        let budget = (u64::from(n) * wire * 8 * u64::from(fps) / 1_000) as u32 + 256;
        let enc = encoder_kbps_for_budget(budget, 256, pct, payload);
        let k = (u64::from(enc) * 1_000 / 8 / u64::from(fps))
            .div_ceil(u64::from(payload))
            .max(1) as u32;
        k + crate::config::recovery_shards(k as usize, pct) as u32
    }

    /// What the rule asks for costs the wire at most the one shard a percent
    /// rounds up into, and where [`LOSS_PCT_MAX`] clamped it, no more than the
    /// session spends today: on a frame that small the two-shard floor is a
    /// bigger share of the budget than the quarter the ceiling allows, so the
    /// clamped cell buys its parity out of the same wire.
    #[test]
    fn what_the_rule_asks_for_never_outspends_a_shard_of_rounding() {
        let mut clamped = 0;
        for fps in [30u32, 60, 120, 165] {
            for n in 4..=60u32 {
                for ppm in [1_000u32, 3_000, 7_000, 15_000, 30_000] {
                    let pct = fec_pct_for_loss(n, ppm, fps);
                    let floor = spend(n, fps, FEC_MIN);
                    let asked = spend(n, fps, pct);
                    assert!(
                        asked <= floor + 1,
                        "{n} shards at {ppm} ppm, {fps} fps: {pct} % puts {asked} \
                         shards on the wire where the floor puts {floor}"
                    );
                    if pct == LOSS_PCT_MAX {
                        clamped += 1;
                        assert!(
                            asked <= floor,
                            "{n} shards at {ppm} ppm, {fps} fps: the ceiling puts \
                             {asked} shards on the wire, the floor {floor}"
                        );
                    }
                }
            }
        }
        assert!(clamped > 20, "only {clamped} cells reached the ceiling");
    }

    /// A link with nothing to answer is exactly today's session: the band's
    /// floor, whatever frame the budget buys.
    #[test]
    fn a_clean_link_asks_for_the_floor_and_nothing_more() {
        for shards in [1u32, 2, 9, 20, 40, 120, 4_000] {
            for fps in [30u32, 60, 120, 165] {
                assert_eq!(fec_pct_for_loss(shards, 0, fps), FEC_MIN);
                // And a loss the two-shard floor already covers reads the same.
                assert_eq!(fec_pct_for_loss(shards, 1, fps), FEC_MIN);
            }
        }
        let mut h = LossHorizon::default();
        for _ in 0..200 {
            assert_eq!(fec_target(0, FEC_MIN, 0, tunnel(), &mut h), FEC_MIN);
        }
    }

    /// The integer rule against the binomial itself, in `f64`: it never hands
    /// a frame less parity than the exact tail asks for, and never more than
    /// three shards past it. The slack is the tail's first term standing in
    /// for the sum, plus the shard a percent rounds up on.
    #[test]
    fn the_parity_rule_holds_the_binomial_it_is_sized_against() {
        let binom = |n: u32, k: u32| -> f64 {
            (0..k).fold(1.0f64, |r, i| r * f64::from(n - i) / f64::from(i + 1))
        };
        let tail = |n: u32, p: f64, j: u32| -> f64 {
            (j..=n)
                .map(|i| binom(n, i) * p.powi(i as i32) * (1.0 - p).powi((n - i) as i32))
                .sum()
        };
        for fps in [30u32, 60] {
            let budget = 1.0 / f64::from(fps) / LOSS_BUDGET_SECS as f64;
            for n in [3u32, 8, 15, 20, 25, 30, 40, 60, 80, 100, 120] {
                for ppm in [0u32, 300, 1_000, 3_000, 7_000, 10_000, 15_000] {
                    let p = f64::from(ppm) / 1_000_000.0;
                    let m = parity_for_pct(n, fec_pct_for_loss(n, ppm, fps));
                    let exact = (0..=n).find(|&j| tail(n, p, j + 1) <= budget).unwrap_or(n);
                    let want = exact
                        .min(parity_for_pct(n, LOSS_PCT_MAX))
                        .max(parity_for_pct(n, FEC_MIN));
                    assert!(
                        (want..=want + 3).contains(&m),
                        "{n} shards at {ppm} ppm, {fps} fps: {m} parity against {want}"
                    );
                }
            }
        }
    }

    /// The rig's tunnel: 19–30 wire shards a frame at 0.7 %, where two parity
    /// shards lose a frame every forty seconds. The rule asks for three or
    /// four, as a percent the budget spends out of the encoder's rate — and
    /// the percent it names buys exactly that, no shard over.
    #[test]
    fn the_tunnels_frame_asks_for_the_parity_its_loss_needs() {
        let asked: Vec<(u32, u8, u32)> = [19u32, 23, 27, 30]
            .into_iter()
            .map(|n| {
                let pct = fec_pct_for_loss(n, 7_000, 30);
                (n, pct, parity_for_pct(n, pct))
            })
            .collect();
        assert_eq!(
            asked,
            [(19, 19, 3), (23, 15, 3), (27, 13, 3), (30, 16, 4)],
            "wire shards, percent, parity"
        );
        // The rig's 8.5 Mbps tunnel is 23 of them, and what it costs at the
        // same wire is 8 % of the encoder's rate.
        assert_eq!(wire_shards_per_frame(tunnel()), 23);
        let floor = encoder_kbps_for_budget(8_500, 256, FEC_MIN, 1408);
        let sized = encoder_kbps_for_budget(8_500, 256, 15, 1408);
        assert_eq!((floor, sized), (7_510, 6_857));
    }

    /// A steady link has to see one percent. Every move of the target
    /// re-derives the encoder rate, the rule sits on one `m` for a whole band
    /// of frame sizes, and the decay walks a point per window under it — so
    /// the boundary and the decay both have to come to rest, at every loss
    /// the band covers and every frame the range buys.
    #[test]
    fn a_steady_level_holds_one_fec_target() {
        for (budget_kbps, fps) in [
            (6_972u32, 30u32),
            (8_500, 30),
            (12_500, 30),
            (20_000, 60),
            (245_000, 60),
        ] {
            for ppm in [3_000u32, 7_000, 15_000, 25_000, 70_000] {
                let frame = FrameBudget {
                    budget_kbps,
                    audio_kbps: 256,
                    shard_payload: 1408,
                    fps,
                };
                let mut h = LossHorizon::default();
                let mut fec = FEC_ADAPTIVE_START;
                let mut moves = 0;
                for _ in 0..300 {
                    let next = fec_target(ppm, fec, 0, frame, &mut h);
                    moves += u32::from(next != fec);
                    fec = next;
                }
                let settled = fec;
                for _ in 0..100 {
                    fec = fec_target(ppm, fec, 0, frame, &mut h);
                    assert_eq!(
                        fec, settled,
                        "{budget_kbps} kbps at {fps} fps, {ppm} ppm: settled at \
                         {settled}, moved to {fec}"
                    );
                }
                assert!(
                    moves <= 16,
                    "{budget_kbps} kbps at {fps} fps, {ppm} ppm: {moves} encoder \
                     re-derivations before it settled"
                );
            }
        }
    }

    /// A level drifting across an `m` boundary must not take the shard back
    /// and forth over it. The rule gives one up only once [`LOSS_RELEASE_PCT`]
    /// of the level would still not want it, which is outside what a horizon
    /// of a few hundred packets a window can resolve.
    #[test]
    fn a_level_on_a_boundary_keeps_the_parity_it_bought() {
        let (n, fps) = (23u32, 30u32);
        // Where the third parity shard starts being needed.
        let edge = (1..20_000u32)
            .find(|&ppm| m_needed(n, ppm, fps) >= 3)
            .expect("a boundary inside the band");
        let mut h = LossHorizon::default();
        for _ in 0..400 {
            h.note(2 * edge, false);
        }
        let held = h.hold(n, 2 * edge, fps);
        assert!(held > FEC_MIN, "the rule has to be holding a shard to give");
        // A hair under the boundary, and a seventh under it, are both inside
        // what the level can tell apart: hold.
        assert_eq!(h.hold(n, edge - 1, fps), held, "held at {edge} ppm");
        assert_eq!(h.hold(n, edge * 85 / 100, fps), held);
        // Half of it is not: the shard goes back to the picture.
        assert_eq!(h.hold(n, edge / 2, fps), FEC_MIN);
    }

    /// The level a window alone cannot read: it holds through a window that
    /// happened to lose nothing, it drops to the floor within a minute of the
    /// loss stopping, and a window a frame died in moves it not at all.
    #[test]
    fn the_loss_horizon_holds_a_steady_link_and_forgets_one_that_stops() {
        let mut h = LossHorizon::default();
        let mut level = 0;
        for _ in 0..200 {
            level = h.note(7_000, false);
        }
        assert!((6_700..=7_000).contains(&level), "settled at {level} ppm");
        // One window with nothing in it is not a clean link.
        let dip = h.note(0, false);
        assert!(dip > 6_000, "one empty window dropped it to {dip}");
        // Congestion teaches it nothing either way.
        let mut held = h;
        assert_eq!(held.note(500_000, true), dip);
        // And a link that stops losing is at the floor inside a minute: the
        // rule is back to FEC_MIN once the level cannot ask for a third shard.
        let mut windows = 0;
        while fec_pct_for_loss(23, h.note(0, false), 30) > FEC_MIN {
            windows += 1;
            assert!(windows < 80, "still asking for parity after {windows}");
        }
        assert!(windows <= 40, "{windows} windows to forget the loss");
    }

    #[test]
    fn wire_budget_derivation_never_overshoots() {
        // 20 Mbps budget, 300 kbps audio, 10 % FEC, 1408-byte shards → 17 130 kbps video.
        assert_eq!(encoder_kbps_for_budget(20_000, 300, 10, 1408), 17_130);
        // Wire spend rounds back under the budget, never over.
        assert_eq!(budget_kbps_for_encoder(17_130, 300, 10, 1408), 19_999);

        // Non-floored roundtrip spends within the budget.
        for budget in [2_000u32, 5_000, 20_000, 100_000, 1_000_000] {
            for fec in [1u8, 5, 10, 25, 50] {
                for audio in [0u32, 256, 512, 8_500] {
                    for payload in [1388u16, 1408, 8896] {
                        let e = encoder_kbps_for_budget(budget, audio, fec, payload);
                        if e > MIN_BITRATE_KBPS {
                            let back = budget_kbps_for_encoder(e, audio, fec, payload);
                            assert!(
                                back <= budget,
                                "budget {budget} fec {fec} audio {audio} payload {payload}: \
                                 derived {e} spends {back}"
                            );
                        }
                    }
                }
            }
        }

        // Budget too small for its audio: floor at MIN and overshoot honestly.
        assert_eq!(
            encoder_kbps_for_budget(500, 8_500, 50, 1408),
            MIN_BITRATE_KBPS
        );

        // More parity ⇒ lower video rate, same budget.
        let calm = encoder_kbps_for_budget(20_000, 300, 1, 1408);
        let burned = encoder_kbps_for_budget(20_000, 300, 5, 1408);
        let stormy = encoder_kbps_for_budget(20_000, 300, 50, 1408);
        assert!(calm > burned && burned > stormy);
    }
}
