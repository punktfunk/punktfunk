//! How long to wait before starting a supervised child again when it keeps exiting.
//!
//! A [`Backoff`] counts failures in a row. [`Backoff::failed`] counts one more and returns the
//! wait before the next start, along the caller's [`Curve`]. The caller's [`Reset`] rule says
//! whether a run that lasted long enough clears the count first. [`Backoff::reset`] clears it
//! under either rule. Pure: the caller passes the instants.
//!
//! Three users each hold one, with a curve and a rule of their own: the service's console host,
//! the service's web console, and a seat's host.

use std::time::{Duration, Instant};

/// How the wait grows with each failure in a row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Curve {
    /// One `step` more per failure, up to `steps` of them.
    Linear { step: Duration, steps: u32 },
    /// `first`, then twice the last wait, up to `cap`.
    Doubling { first: Duration, cap: Duration },
}

/// Whether a run that lasted counts as a recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reset {
    /// No run clears the count, however long.
    Never,
    /// A run at least this long clears the count before its failure is counted.
    AfterRun(Duration),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    curve: Curve,
    reset: Reset,
    failures: u32,
    /// When the child came up, until that run fails.
    up_since: Option<Instant>,
}

impl Backoff {
    pub const fn new(curve: Curve, reset: Reset) -> Self {
        Self {
            curve,
            reset,
            failures: 0,
            up_since: None,
        }
    }

    /// The child is up as of `now`. One already up keeps its earlier instant.
    pub fn started(&mut self, now: Instant) {
        self.up_since.get_or_insert(now);
    }

    /// The child failed at `now`, or did not start. Answers the wait before the next start.
    pub fn failed(&mut self, now: Instant) -> Duration {
        let ran = self
            .up_since
            .take()
            .map(|since| now.saturating_duration_since(since));
        if let (Reset::AfterRun(lasted), Some(ran)) = (self.reset, ran)
            && ran >= lasted
        {
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        match self.curve {
            Curve::Linear { step, steps } => step.saturating_mul(self.failures.min(steps)),
            Curve::Doubling { first, cap } => first
                .saturating_mul(2_u32.saturating_pow(self.failures - 1))
                .min(cap),
        }
    }

    /// Failures in a row so far.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// Forget the failures and the run: the caller knows the next start is a fresh one.
    pub fn reset(&mut self) {
        *self = Self::new(self.curve, self.reset);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);
    /// The service's console host.
    const CONSOLE_HOST: Curve = Curve::Linear {
        step: Duration::from_millis(500),
        steps: 10,
    };
    /// The service's web console.
    const WEB_CONSOLE: Curve = Curve::Doubling {
        first: Duration::from_millis(500),
        cap: Duration::from_secs(60),
    };
    /// A seat's host, and a seat's cycle.
    const SEAT: Curve = Curve::Doubling {
        first: Duration::from_secs(1),
        cap: Duration::from_secs(30),
    };

    /// The waits of `count` failures in a row, in milliseconds.
    fn waits(backoff: &mut Backoff, count: usize) -> Vec<u128> {
        let now = Instant::now();
        (0..count)
            .map(|_| backoff.failed(now).as_millis())
            .collect()
    }

    #[test]
    fn the_three_curves_in_use_give_their_waits() {
        let mut host = Backoff::new(CONSOLE_HOST, Reset::Never);
        let linear = [
            500, 1000, 1500, 2000, 2500, 3000, 3500, 4000, 4500, 5000, 5000, 5000,
        ];
        assert_eq!(waits(&mut host, 12), linear);
        assert_eq!(host.failures(), 12);

        let mut web = Backoff::new(WEB_CONSOLE, Reset::AfterRun(Duration::from_secs(60)));
        let doubling = [500, 1000, 2000, 4000, 8000, 16000, 32000, 60000, 60000];
        assert_eq!(waits(&mut web, 9), doubling);

        let mut seat = Backoff::new(SEAT, Reset::Never);
        assert_eq!(
            waits(&mut seat, 7),
            [1000, 2000, 4000, 8000, 16000, 30000, 30000]
        );
    }

    #[test]
    fn a_count_past_the_curve_stays_at_the_cap() {
        for curve in [CONSOLE_HOST, WEB_CONSOLE, SEAT] {
            let mut backoff = Backoff::new(curve, Reset::Never);
            let all = waits(&mut backoff, 200);
            assert_eq!(all[199], all[100], "{curve:?}");
            backoff.failures = u32::MAX;
            assert_eq!(backoff.failed(Instant::now()).as_millis(), all[199]);
            assert_eq!(backoff.failures(), u32::MAX);
        }
    }

    #[test]
    fn a_run_clears_the_count_only_under_the_rule_that_says_so() {
        let minute = Duration::from_secs(60);
        // The rule, how long the child ran before its fourth failure, and the wait after it.
        let rows = [
            (Reset::Never, HOUR, 8000),
            (
                Reset::AfterRun(minute),
                minute - Duration::from_millis(1),
                8000,
            ),
            (Reset::AfterRun(minute), minute, 1000),
            (Reset::AfterRun(minute), HOUR, 1000),
        ];
        for (rule, ran, wait) in rows {
            let mut backoff = Backoff::new(SEAT, rule);
            let up = Instant::now();
            assert_eq!(waits(&mut backoff, 3), [1000, 2000, 4000]);
            backoff.started(up);
            assert_eq!(
                backoff.failed(up + ran).as_millis(),
                wait,
                "{rule:?} {ran:?}"
            );
        }
    }

    #[test]
    fn a_run_is_counted_from_the_first_time_the_child_was_up() {
        let minute = Duration::from_secs(60);
        let mut backoff = Backoff::new(SEAT, Reset::AfterRun(minute));
        let up = Instant::now();
        assert_eq!(waits(&mut backoff, 2), [1000, 2000]);
        backoff.started(up);
        backoff.started(up + minute - Duration::from_secs(1));
        assert_eq!(backoff.failed(up + minute).as_millis(), 1000);
    }

    #[test]
    fn a_child_that_did_not_start_never_clears_the_count() {
        let mut backoff = Backoff::new(SEAT, Reset::AfterRun(Duration::from_secs(60)));
        let up = Instant::now();
        backoff.started(up);
        assert_eq!(backoff.failed(up).as_millis(), 1000);
        // No start since: the hour that passed was not a run.
        assert_eq!(backoff.failed(up + HOUR).as_millis(), 2000);
    }

    #[test]
    fn reset_forgets_the_failures_under_either_rule() {
        for rule in [Reset::Never, Reset::AfterRun(HOUR)] {
            let mut backoff = Backoff::new(CONSOLE_HOST, rule);
            assert_eq!(waits(&mut backoff, 4), [500, 1000, 1500, 2000]);
            backoff.started(Instant::now());
            backoff.reset();
            assert_eq!(backoff, Backoff::new(CONSOLE_HOST, rule));
            assert_eq!(waits(&mut backoff, 1), [500], "{rule:?}");
        }
    }
}
