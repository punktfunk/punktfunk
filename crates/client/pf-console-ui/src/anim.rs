//! Console-shell motion: springs for anything the user can retarget, timed
//! choreography for fire-and-forget arrivals.
//!
//! [`Spring`] wraps [`spring_advance`]. Velocity carries across a
//! retarget, so a Back mid-push turns the screen around where it is.
//! [`SpringSpec`] and [`springs`] name a feel instead of a `k`/`c` pair.
//!
//! [`Entrance`] plus the easing functions are a pure function of the clock.
//! A screen holds one and asks it per item; there is no per-item state.
//! Evidence: `spring_spec_matches_the_tray_constants`, `entrance_envelope`.

// Semi-implicit Euler, not eased: velocity carries across retargets.
/// Cursor chase: ζ ≈ 0.85 — settles in ~0.3 s with a whisker of overshoot.
pub const SPRING_K: f64 = 200.0;
pub const SPRING_C: f64 = 24.0;
/// Boundary recoil: soft and underdamped (ζ ≈ 0.4) — a rubbery bounce, two visible
/// wobbles, ~0.5 s to rest.
pub const BUMP_K: f64 = 260.0;
pub const BUMP_C: f64 = 13.0;

fn spring_step(pos: f64, vel: f64, target: f64, k: f64, c: f64, dt: f64) -> (f64, f64) {
    let vel = vel + (k * (target - pos) - c * vel) * dt;
    (pos + vel * dt, vel)
}

/// One frame of a damped spring, in ≤ 8 ms substeps so a stalled frame stays inside the integrator's stability bound.
pub fn spring_advance(
    mut pos: f64,
    mut vel: f64,
    target: f64,
    k: f64,
    c: f64,
    dt: f64,
) -> (f64, f64) {
    let n = (dt / 0.008).ceil().max(1.0) as usize;
    let h = dt / n as f64;
    for _ in 0..n {
        (pos, vel) = spring_step(pos, vel, target, k, c, h);
    }
    (pos, vel)
}

/// Refused-move recoil: the kick against the push, design units/s. A velocity, not a
/// displacement, so the list eases out and springs back rather than jumping.
pub const BUMP_V: f64 = 380.0;
/// Mount entrance ([`Entrance`]): arrival scale, rise (design units), yaw. Shared with the home carousel.
pub const ENTER_SCALE: f64 = 0.96;
pub const ENTER_RISE: f64 = 12.0;
pub const ENTER_TURN_DEG: f64 = 62.0;

pub fn ease_out_cubic(t: f64) -> f64 {
    let u = 1.0 - t.clamp(0.0, 1.0);
    1.0 - u * u * u
}

/// `tau` is seconds. The exponential is frame-rate independent and never overshoots.
pub fn approach(current: f64, target: f64, dt: f64, tau: f64) -> f64 {
    current + (target - current) * (1.0 - (-dt / tau).exp())
}

/// `response` is the undamped period in seconds; `damping` is ζ (1.0 lands dead).
/// [`SpringSpec::kc`] converts to the integrator's `k`/`c` the same way SwiftUI's
/// `.spring(response:dampingFraction:)` does.
#[derive(Clone, Copy)]
pub struct SpringSpec {
    pub response: f64,
    pub damping: f64,
}

impl SpringSpec {
    /// `k = ω²`, `c = 2ζω` with `ω = 2π/response`. Written with `ω` not `2ζ√k`:
    /// `√k` is `ω`, and `f64::sqrt` is not `const`.
    pub const fn kc(self) -> (f64, f64) {
        let w = std::f64::consts::TAU / self.response;
        (w * w, 2.0 * self.damping * w)
    }
}

/// Shell springs. Carousel pairs ([`SPRING_K`]/[`SPRING_C`], [`BUMP_K`]/[`BUMP_C`]) stay raw:
/// they are shared with the GTK launcher, and wrapping them as specs invites a
/// tidy-up that retunes coverflow on both surfaces.
pub mod springs {
    use super::SpringSpec;

    /// Screen push/pop. Damping 0.88 is just under critical: a full-screen bounce
    /// reads as broken. A spring, not a tween, so a mid-push Back retargets to 0
    /// and the screen turns around where it is.
    pub const NAV: SpringSpec = SpringSpec {
        response: 0.42,
        damping: 0.88,
    };
    /// Focus travel: the plate, and the scrolls that follow focus. Damping 0.78 leaves a
    /// slight overshoot on arrival; that is the pop.
    pub const FOCUS: SpringSpec = SpringSpec {
        response: 0.32,
        damping: 0.78,
    };
    /// Tab pill and keyboard tray: the [`TRAY_K`]/[`TRAY_C`] pair, pinned by
    /// `spring_spec_matches_the_tray_constants`.
    pub const INDICATOR: SpringSpec = SpringSpec {
        response: 0.32,
        damping: 0.86,
    };
    /// OK down: the pressed element dips to [`super::PRESS_SCALE`] and springs back. Damping
    /// 0.65 so a press reads as a press, not a fade.
    pub const PRESS: SpringSpec = SpringSpec {
        response: 0.16,
        damping: 0.65,
    };
    /// A toast or a modal arriving: the screen push's feel, no bounce.
    pub const MODAL: SpringSpec = SpringSpec {
        response: 0.42,
        damping: 0.88,
    };
    /// Quick-action ring. Looser than [`FOCUS`]: without a whisker past the seats
    /// the twist reads as stopping dead at the commit.
    pub const RING: SpringSpec = SpringSpec {
        response: 0.38,
        damping: 0.72,
    };
    /// The launch hold's cover leaving its shelf tile. Long and loose next to the
    /// rest: it crosses the whole screen and turns once on the way, and a tighter
    /// spring finishes both before the eye has followed either.
    pub const LAUNCH: SpringSpec = SpringSpec {
        response: 0.75,
        damping: 0.72,
    };
}

/// How far a press dips the pressed element.
pub const PRESS_SCALE: f64 = 0.97;

/// `k`/`c` live in [`crate::library`] and [`TRAY_K`]/[`TRAY_C`].
#[derive(Clone, Copy)]
pub struct Spring {
    pub pos: f64,
    pub vel: f64,
}

impl Default for Spring {
    /// At rest on zero.
    fn default() -> Spring {
        Spring::rest(0.0)
    }
}

impl Spring {
    pub fn rest(pos: f64) -> Spring {
        Spring { pos, vel: 0.0 }
    }

    pub fn step(&mut self, target: f64, k: f64, c: f64, dt: f64) {
        (self.pos, self.vel) = spring_advance(self.pos, self.vel, target, k, c, dt);
    }

    pub fn step_spec(&mut self, target: f64, spec: SpringSpec, dt: f64) {
        let (k, c) = spec.kc();
        self.step(target, k, c, dt);
    }

    /// Snap onto `target` once motion is imperceptible, so the frame loop stops dirtying.
    pub fn settle(&mut self, target: f64, eps_pos: f64, eps_vel: f64) {
        if (target - self.pos).abs() < eps_pos && self.vel.abs() < eps_vel {
            self.pos = target;
            self.vel = 0.0;
        }
    }
}

/// Tray slide: `.spring(response: 0.32, dampingFraction: 0.86)`, rounded
/// (exact is 385.53 / 33.77). Pinned by `spring_spec_matches_the_tray_constants`.
pub const TRAY_K: f64 = 385.0;
pub const TRAY_C: f64 = 33.7;

/// Overshoots 1.0 then settles, so a card reads as thrown. `C1` is 1.2, not the CSS
/// 1.70158: full strength reads as a bounce.
pub fn ease_out_back(t: f64) -> f64 {
    const C1: f64 = 1.2;
    const C3: f64 = C1 + 1.0;
    let u = t.clamp(0.0, 1.0) - 1.0;
    1.0 + C3 * u * u * u + C1 * u * u
}

/// Fraction of `window` spent fading. 0.34 so the card is solid before it stops moving.
const FADE_SHARE: f64 = 0.34;

#[derive(Clone, Copy)]
pub struct EntranceSpec {
    /// Seconds for one item to arrive.
    pub window: f64,
    /// Seconds of delay per step from the anchor.
    pub stagger: f64,
    /// Seconds, ceiling on delay. `cap / stagger` is how many steps ever fan;
    /// raise `stagger` alone and the far half of a shelf lands in one block.
    pub cap: f64,
}

pub mod entrances {
    use super::EntranceSpec;

    /// Cards, posters and coverflow: 40 ms apart, a ripple rather than a sequence. Six steps
    /// fan; the rest of a long row lands with the sixth.
    pub const CARDS: EntranceSpec = EntranceSpec {
        window: 0.5,
        stagger: 0.04,
        cap: 0.24,
    };
    /// A poster grid: the [`CARDS`] ripple along a row, with room for rows to follow one
    /// another. The grid counts a row as several steps, so a screenful cascades downward.
    pub const GRID: EntranceSpec = EntranceSpec {
        window: 0.5,
        stagger: 0.04,
        cap: 0.6,
    };
    /// Menu rows. Shorter than [`CARDS`], not zero: under about three frames
    /// apart the rows read as one arrival rather than a ripple.
    pub const ROWS: EntranceSpec = EntranceSpec {
        window: 0.42,
        stagger: 0.055,
        cap: 0.33,
    };
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct EntranceAt {
    /// Progress on [`ease_out_back`] (overshoots 1.0). The screen picks what moves.
    pub travel: f64,
    pub fade: f64,
}

impl EntranceAt {
    pub const SETTLED: EntranceAt = EntranceAt {
        travel: 1.0,
        fade: 1.0,
    };
}

/// Pure function of the shell clock. A screen holds one and asks it per item;
/// there is no per-card state. Arm once per mount, not per frame.
#[derive(Clone, Copy)]
pub struct Entrance {
    spec: EntranceSpec,
    anchor: usize,
    t0: f64,
    /// Snapshotted at arm time so one entrance plays one way if the setting moves.
    reduced: bool,
}

impl Entrance {
    /// Arm at `t0`. Callers pass the cursor as `anchor` so a restored selection
    /// assembles around the eye, not from a corner.
    pub fn new(spec: EntranceSpec, anchor: usize, t0: f64) -> Entrance {
        Entrance {
            spec,
            anchor,
            t0,
            reduced: crate::theme::reduce_motion(),
        }
    }

    pub fn at(&self, i: usize, t: f64) -> EntranceAt {
        let elapsed = t - self.t0;
        if self.reduced {
            return EntranceAt {
                travel: 1.0,
                fade: (elapsed / self.spec.window).clamp(0.0, 1.0),
            };
        }
        let delay = (self.anchor.abs_diff(i) as f64 * self.spec.stagger).min(self.spec.cap);
        let w = ((elapsed - delay) / self.spec.window).clamp(0.0, 1.0);
        EntranceAt {
            travel: ease_out_back(w),
            fade: ease_out_cubic(w / FADE_SHARE),
        }
    }

    /// Over at `cap + window` regardless of `len`, so callers can drop the entrance
    /// without counting items.
    pub fn done(&self, t: f64) -> bool {
        t - self.t0 >= self.spec.cap + self.spec.window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stay finite through a stalled frame (0.05 s).
    #[test]
    fn springs_converge() {
        let (mut pos, mut vel) = (0.0, 0.0);
        for _ in 0..120 {
            (pos, vel) = spring_advance(pos, vel, 3.0, SPRING_K, SPRING_C, 1.0 / 60.0);
        }
        assert!((pos - 3.0).abs() < 0.01, "{pos}");
        let (p, v) = spring_advance(0.0, 0.0, 1.0, BUMP_K, BUMP_C, 0.05);
        assert!(
            p.is_finite() && v.is_finite() && p > 0.0 && p < 2.0,
            "{p}/{v}"
        );
    }

    #[test]
    fn ease_out_cubic_shape() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        assert!(ease_out_cubic(0.5) > 0.5, "front-loaded");
        assert_eq!(ease_out_cubic(2.0), 1.0, "clamped");
    }

    #[test]
    fn approach_converges_and_never_overshoots() {
        let mut v = 0.0;
        for _ in 0..120 {
            v = approach(v, 1.0, 1.0 / 60.0, 0.06);
            assert!(v <= 1.0);
        }
        assert!((v - 1.0).abs() < 1e-6);
    }

    #[test]
    fn spring_settles() {
        let mut s = Spring::rest(0.0);
        for _ in 0..240 {
            s.step(1.0, 200.0, 24.0, 1.0 / 60.0);
            s.settle(1.0, 0.001, 0.01);
        }
        assert_eq!((s.pos, s.vel), (1.0, 0.0));
    }

    /// `kc` vs [`TRAY_K`]/[`TRAY_C`]. Those are rounded (385.0/33.7 vs 385.53/33.77),
    /// so the assert is 0.3 %, not exact.
    #[test]
    fn spring_spec_matches_the_tray_constants() {
        let (k, c) = springs::INDICATOR.kc();
        assert!(
            (k - TRAY_K).abs() / TRAY_K < 0.003,
            "k: spec says {k}, TRAY_K is {TRAY_K}"
        );
        assert!(
            (c - TRAY_C).abs() / TRAY_C < 0.003,
            "c: spec says {c}, TRAY_C is {TRAY_C}"
        );
    }

    /// Asserted on fade, not travel: ease-out-back overshoots, so a card at 80 % of
    /// its window can be further displaced than one that has already landed.
    #[test]
    fn entrance_envelope() {
        let e = Entrance::new(entrances::CARDS, 5, 0.0);

        for t in [0.05, 0.2, 0.4, 0.7, 1.1] {
            let anchor = e.at(5, t).fade;
            for i in 0..14 {
                assert!(
                    e.at(i, t).fade <= anchor + 1e-9,
                    "item {i} beat the anchor at t={t}"
                );
            }
            assert_eq!(e.at(3, t), e.at(7, t));
        }

        // Past `cap / stagger` steps, everything starts together. Derived so a
        // re-tune re-aims the probe.
        let beyond = (entrances::CARDS.cap / entrances::CARDS.stagger).ceil() as usize + 1;
        assert_eq!(e.at(5 + beyond, 0.3), e.at(5 + 250, 0.3));

        let mut last = 0.0;
        for step in 0..=120 {
            let at = e.at(9, f64::from(step) * 0.01);
            assert!(at.fade >= last - 1e-9, "fade went backwards at step {step}");
            assert!((0.0..=1.0).contains(&at.fade));
            last = at.fade;
        }

        let over = entrances::CARDS.cap + entrances::CARDS.window;
        assert!(e.done(over));
        assert!(!e.done(over - 0.01));
        for i in [5, 9, 400] {
            assert_eq!(e.at(i, over), EntranceAt::SETTLED, "item {i} never landed");
        }
    }

    /// `entrance_envelope` cannot see this: stagger 0 satisfies every shape property
    /// and arrives as one event. Derived from the specs so a re-tune that undoes the
    /// fan fails here.
    #[test]
    fn entrance_stagger_reads_as_a_sequence() {
        // Measured against the fade, not `window`: an item is perceptually done a
        // third of the way through the window, so `window` flatters the stagger.
        let separation = |spec: EntranceSpec| {
            let e = Entrance::new(spec, 5, 0.0);
            let t_mid = 0.5 * FADE_SHARE * spec.window;
            e.at(5, t_mid).fade - e.at(6, t_mid).fade
        };
        let cards = separation(entrances::CARDS);
        assert!(cards > 0.2, "CARDS neighbours arrive together: {cards}");
        let rows = separation(entrances::ROWS);
        assert!(rows > 0.5, "ROWS is quieter, not staggerless: {rows}");

        for (name, spec, budget) in [
            ("CARDS", entrances::CARDS, 1.25),
            ("GRID", entrances::GRID, 1.25),
            ("ROWS", entrances::ROWS, 0.8),
        ] {
            // `cap / stagger` is how many steps ever fan. Drop this and a wider
            // `stagger` buys more offset across fewer steps, which is worse.
            let steps = spec.cap / spec.stagger;
            assert!(steps >= 4.0, "{name} fans only {steps} steps");
            // Cursor delay is 0, so this budget is peripheral polish. Fail here
            // rather than treat "too long" as an opinion.
            let total = spec.cap + spec.window;
            assert!(total <= budget, "{name} takes {total} s to retire");
        }
    }

    #[test]
    fn ease_out_back_overshoots_then_lands() {
        // Tolerance, not exact: `1 − 2.2 + 1.2` is 0 in reals and −2.2e−16 in f64.
        assert!(ease_out_back(0.0).abs() < 1e-12);
        assert_eq!(ease_out_back(1.0), 1.0);
        assert_eq!(ease_out_back(2.0), 1.0, "clamped");
        let peak = (0..=100)
            .map(|i| ease_out_back(f64::from(i) / 100.0))
            .fold(f64::MIN, f64::max);
        assert!(peak > 1.0, "no overshoot at all: {peak}");
        assert!(peak < 1.12, "overshoot reads as a bounce: {peak}");
    }

    /// Snapshotted at arm time: the flag is flipped after `new` and the entrance
    /// must still play as a staggerless crossfade.
    #[test]
    fn entrance_under_reduced_motion_is_a_staggerless_crossfade() {
        crate::theme::set_reduce_motion(true);
        let e = Entrance::new(entrances::CARDS, 5, 0.0);
        crate::theme::set_reduce_motion(false);
        for t in [0.0, 0.1, 0.3, 0.6] {
            let a = e.at(5, t);
            assert_eq!(a.travel, 1.0, "nothing travels");
            assert_eq!(a, e.at(200, t), "and nothing staggers");
        }
        assert_eq!(e.at(0, 0.6), EntranceAt::SETTLED);
    }

    /// Peak of a unit step as a fraction over the target. 120 Hz so this is the
    /// spring's shape, not the sampling's.
    fn peak_overshoot(spec: SpringSpec) -> f64 {
        let mut s = Spring::rest(0.0);
        let mut peak: f64 = 0.0;
        for _ in 0..600 {
            s.step_spec(1.0, spec, 1.0 / 120.0);
            peak = peak.max(s.pos);
        }
        (peak - 1.0).max(0.0)
    }

    /// FOCUS must overshoot enough to read as a pop; INDICATOR and NAV must not
    /// wobble. A re-tune that breaks either fails here.
    #[test]
    fn spec_damping_choices_show_up_as_overshoot() {
        let focus = peak_overshoot(springs::FOCUS);
        assert!(focus > 0.005, "FOCUS must visibly overshoot, got {focus}");
        let indicator = peak_overshoot(springs::INDICATOR);
        assert!(
            indicator < 0.01,
            "INDICATOR must not visibly overshoot, got {indicator}"
        );
        let nav = peak_overshoot(springs::NAV);
        assert!(nav < 0.01, "NAV must not visibly overshoot, got {nav}");
        assert!(
            peak_overshoot(springs::PRESS) > focus,
            "PRESS is the loosest of the table"
        );
    }
}
