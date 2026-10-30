//! What a suspend does to a keep-alive window, which counts real time.
//!
//! `Instant` stops while Linux sleeps, so a linger tick takes what it missed off every deadline
//! ([`Watch`]). A session the box slept under lost its client when the box went down, though the
//! host only learns that from the idle timeout after the wake ([`slept_since`]). A display lease
//! measures from its own start, so one rebuilt after the wake counts no sleep. The arithmetic is
//! [`crate::lifecycle::left_after_sleep`].

use std::time::{Duration, Instant};

/// A smaller gap is two clock reads apart, not a suspend.
const NOISE: Duration = Duration::from_secs(1);

/// Time since boot, with suspends and without.
#[cfg(target_os = "linux")]
fn clocks() -> (Duration, Duration) {
    fn read(clock: libc::clockid_t) -> Duration {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `clock_gettime` writes one `timespec` through the pointer and touches nothing
        // else; `ts` outlives the call.
        unsafe { libc::clock_gettime(clock, &mut ts) };
        Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
    }
    (read(libc::CLOCK_BOOTTIME), read(libc::CLOCK_MONOTONIC))
}

/// Time since boot, with suspends and without, from interrupt time in 100 ns ticks.
#[cfg(target_os = "windows")]
fn clocks() -> (Duration, Duration) {
    use windows::Win32::System::WindowsProgramming::{
        QueryInterruptTime, QueryUnbiasedInterruptTime,
    };
    let mut awake = 0u64;
    // SAFETY: `QueryInterruptTime` takes nothing; `QueryUnbiasedInterruptTime` writes one `u64`
    // through a pointer that outlives the call, and fails only on a null one.
    let real = unsafe {
        let _ = QueryUnbiasedInterruptTime(&mut awake);
        QueryInterruptTime()
    };
    let ticks = |t: u64| Duration::from_nanos(t.saturating_mul(100));
    (ticks(real), ticks(awake))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn clocks() -> (Duration, Duration) {
    (Duration::ZERO, Duration::ZERO)
}

fn without_noise(gap: Duration) -> Duration {
    if gap < NOISE {
        Duration::ZERO
    } else {
        gap
    }
}

/// Time this box has spent suspended since boot. Only the gap between two readings means
/// anything: take one when a session starts, and hand it to [`slept_since`] when it ends.
pub fn slept() -> Duration {
    let (real, awake) = clocks();
    real.saturating_sub(awake)
}

/// How long the box has slept since [`slept`] returned `mark`.
pub fn slept_since(mark: Duration) -> Duration {
    without_noise(slept().saturating_sub(mark))
}

/// One linger thread's view of the real time `Instant` did not count. Measured against `Instant`
/// itself, so it holds on a platform whose `Instant` runs through a suspend: nothing is missed.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "windows")),
    allow(dead_code, reason = "no linger thread")
)]
pub(crate) struct Watch {
    real: Duration,
    at: Instant,
}

#[cfg_attr(
    not(any(target_os = "linux", target_os = "windows")),
    allow(dead_code, reason = "no linger thread")
)]
impl Watch {
    pub(crate) fn new() -> Self {
        Watch {
            real: clocks().0,
            at: Instant::now(),
        }
    }

    /// Real time since the last call that `Instant` missed. Zero unless the box slept.
    pub(crate) fn missed(&mut self) -> Duration {
        let missed = self.step(clocks().0, Instant::now());
        if !missed.is_zero() {
            tracing::info!(
                secs = missed.as_secs(),
                "the box slept: counting that against the keep-alive windows"
            );
        }
        missed
    }

    fn step(&mut self, real: Duration, at: Instant) -> Duration {
        let passed = real.saturating_sub(self.real);
        let counted = at.saturating_duration_since(self.at);
        (self.real, self.at) = (real, at);
        without_noise(passed.saturating_sub(counted))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tick sees a suspend once, and clock jitter never.
    #[test]
    fn a_watch_reports_a_suspend_once() {
        let t0 = Instant::now();
        let mut watch = Watch {
            real: Duration::ZERO,
            at: t0,
        };
        let tick = |s: u64| t0 + Duration::from_secs(s);
        let secs = Duration::from_secs;
        let jitter = Duration::from_millis(5);
        assert_eq!(watch.step(secs(1), tick(1)), Duration::ZERO);
        assert_eq!(watch.step(secs(2) + jitter, tick(2)), Duration::ZERO);
        assert_eq!(watch.step(secs(3603) + jitter, tick(3)), secs(3600));
        assert_eq!(watch.step(secs(3604) + jitter, tick(4)), Duration::ZERO);
    }

    /// Where `Instant` runs through a suspend it counted everything, so nothing is taken twice.
    #[test]
    fn an_instant_that_counts_a_suspend_misses_nothing() {
        let t0 = Instant::now();
        let mut watch = Watch {
            real: Duration::ZERO,
            at: t0,
        };
        let later = Duration::from_secs(3600);
        assert_eq!(watch.step(later, t0 + later), Duration::ZERO);
    }

    /// The real clocks agree with each other on a box that is not sleeping.
    #[test]
    fn a_box_that_stays_awake_slept_nothing() {
        let mark = slept();
        assert_eq!(slept_since(mark), Duration::ZERO);
        assert_eq!(Watch::new().missed(), Duration::ZERO);
    }
}
