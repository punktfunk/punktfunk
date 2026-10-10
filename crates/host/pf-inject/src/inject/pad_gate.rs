//! Shared virtual-pad creation-retry policy for every backend manager
//! (Linux uinput/uhid, Windows XUSB/UMDF). See [`PadGate`].
//!
//! Create failures are systemic (device-node permissions, missing driver), so
//! the gate is manager-wide, not per slot. After a miss, creation is blocked
//! until the backoff elapses so the input path does not retry every frame. A
//! success resets to [`FIRST_BACKOFF`]. Consecutive misses double up to
//! [`MAX_BACKOFF`].
//!
//! Tests in this file pin allow, doubling, the cap, and reset.

use std::time::{Duration, Instant};

/// 1 s after the first miss. The input path runs 60–240 Hz; a retry every frame
/// would re-log on each one.
const FIRST_BACKOFF: Duration = Duration::from_secs(1);
/// 30 s cap. A broken host retries at most this often; a fix is picked up within
/// one window, with no host restart.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Manager-wide create-retry gate. Failures are systemic (permissions, missing
/// driver), so one timer covers every slot.
#[derive(Debug, Default)]
pub struct PadGate {
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl PadGate {
    pub fn new() -> PadGate {
        PadGate::default()
    }

    pub fn allow(&self, now: Instant) -> bool {
        match self.retry_at {
            None => true,
            Some(t) => now >= t,
        }
    }

    pub fn on_success(&mut self) {
        self.retry_at = None;
        self.backoff = Duration::ZERO;
    }

    pub fn on_failure(&mut self, now: Instant) {
        self.backoff = if self.backoff.is_zero() {
            FIRST_BACKOFF
        } else {
            (self.backoff * 2).min(MAX_BACKOFF)
        };
        self.retry_at = Some(now + self.backoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_gate_allows_creation() {
        assert!(PadGate::new().allow(Instant::now()));
    }

    #[test]
    fn failure_blocks_until_backoff_elapses_then_allows_one_retry() {
        let t0 = Instant::now();
        let mut g = PadGate::new();
        g.on_failure(t0);
        assert!(!g.allow(t0));
        assert!(!g.allow(t0 + FIRST_BACKOFF - Duration::from_millis(1)));
        assert!(g.allow(t0 + FIRST_BACKOFF));
    }

    #[test]
    fn consecutive_failures_double_the_backoff_up_to_the_cap() {
        let t0 = Instant::now();
        let mut g = PadGate::new();
        g.on_failure(t0);
        g.on_failure(t0);
        assert!(!g.allow(t0 + FIRST_BACKOFF));
        assert!(g.allow(t0 + 2 * FIRST_BACKOFF));
        for _ in 0..20 {
            g.on_failure(t0);
        }
        assert!(!g.allow(t0 + MAX_BACKOFF - Duration::from_millis(1)));
        assert!(g.allow(t0 + MAX_BACKOFF));
    }

    #[test]
    fn success_resets_the_backoff() {
        let t0 = Instant::now();
        let mut g = PadGate::new();
        g.on_failure(t0);
        g.on_failure(t0);
        g.on_success();
        assert!(g.allow(t0));
        g.on_failure(t0);
        assert!(!g.allow(t0 + FIRST_BACKOFF - Duration::from_millis(1)));
        assert!(g.allow(t0 + FIRST_BACKOFF));
    }
}
