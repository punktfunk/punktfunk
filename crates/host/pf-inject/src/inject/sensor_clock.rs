//! The `sensor_timestamp` a virtual Sony pad stamps into every input report.
//!
//! Real hardware fills this from its IMU clock. It is the only time basis for the motion samples
//! in the same report: `hid-playstation` forwards it as `MSC_TIMESTAMP`, SDL reads it on Windows,
//! and gyro aim integrates angular velocity against the `dt` it implies. Wrong units look like
//! a controller whose sensitivity is off by that factor.
//!
//! Ticks are elapsed time from the pad's first report, not accumulated per publish, so a bursty
//! loop cannot drift the clock. The caller truncates to the field's width, which is how hardware
//! wraps and how every consumer's `prev > current` delta check already works.

use std::time::Instant;

/// Per-pad tick clock. Epoch is that pad's first report, so the field starts at 0 like a new device.
pub struct SensorClock {
    epoch: Option<Instant>,
    /// Ticks per microsecond as the exact fraction `ticks_num / ticks_den`.
    ticks_num: u64,
    ticks_den: u64,
}

impl SensorClock {
    /// DualSense u32 `sensor_timestamp`: **1/3 µs** ticks (`DIV_ROUND_CLOSEST(delta, 3)`). Wraps ~23.9 min.
    pub fn dualsense() -> SensorClock {
        SensorClock::new(3, 1)
    }

    /// DualShock 4 u16 `sensor_timestamp`: **16/3 µs** (≈5.33 µs) ticks
    /// (`DIV_ROUND_CLOSEST(delta * 16, 3)`). Wraps ~349 ms; that is expected.
    pub fn dualshock4() -> SensorClock {
        SensorClock::new(3, 16)
    }

    /// Microsecond ticks: the 8BitDo u32 IMU clock and the HORIPAD's u16 one.
    pub fn micros() -> SensorClock {
        SensorClock::new(1, 1)
    }

    fn new(ticks_num: u64, ticks_den: u64) -> SensorClock {
        SensorClock {
            epoch: None,
            ticks_num,
            ticks_den,
        }
    }

    /// Ticks since this pad's first report. `now` is a parameter so tests can drive the clock.
    pub fn ticks(&mut self, now: Instant) -> u64 {
        let epoch = *self.epoch.get_or_insert(now);
        let micros = now.saturating_duration_since(epoch).as_micros() as u64;
        micros * self.ticks_num / self.ticks_den
    }

    /// [`ticks`](Self::ticks) truncated to DualSense's wrapping u32.
    pub fn ds_ticks(&mut self, now: Instant) -> u32 {
        self.ticks(now) as u32
    }

    /// [`ticks`](Self::ticks) truncated to DualShock 4's wrapping u16.
    pub fn ds4_ticks(&mut self, now: Instant) -> u16 {
        self.ticks(now) as u16
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// One second of wall time must read as one second in each pad's tick units.
    #[test]
    fn a_second_reads_as_a_second() {
        let t0 = Instant::now();
        let after = t0 + Duration::from_secs(1);

        // DualSense: 1 s = 3_000_000 ticks of 1/3 µs.
        let mut ds = SensorClock::dualsense();
        assert_eq!(ds.ticks(t0), 0, "the first report is the epoch");
        assert_eq!(ds.ticks(after), 3_000_000);

        // DualShock 4: 1 s = 187_500 ticks of 16/3 µs.
        let mut ds4 = SensorClock::dualshock4();
        assert_eq!(ds4.ticks(t0), 0);
        assert_eq!(ds4.ticks(after), 187_500);
    }

    /// A 4 ms publish interval (typical DualShock 4 cadence).
    #[test]
    fn one_publish_interval() {
        let t0 = Instant::now();
        let mut ds = SensorClock::dualsense();
        let mut ds4 = SensorClock::dualshock4();
        ds.ticks(t0);
        ds4.ticks(t0);
        let after = t0 + Duration::from_millis(4);
        assert_eq!(ds.ticks(after), 12_000); // 4000 µs × 3
        assert_eq!(ds4.ticks(after), 750); // 4000 µs × 3 / 16
    }

    /// Anchored to the epoch, not accumulated — an irregular cadence cannot drift.
    #[test]
    fn jitter_does_not_drift() {
        let t0 = Instant::now();
        let mut ds4 = SensorClock::dualshock4();
        ds4.ticks(t0); // first `ticks()` call is the epoch, not `t0` itself
        let mut t = t0;
        for step in [1u64, 17, 3, 40, 9, 2] {
            t += Duration::from_millis(step);
            ds4.ticks(t);
        }
        // 72 ms since that first report, regardless of how it was walked.
        assert_eq!(ds4.ticks(t), 72_000 * 3 / 16);
    }

    /// Hardware wraps; consumers use `prev > current`, so truncation is the correct fill.
    #[test]
    fn fields_wrap_like_hardware() {
        let t0 = Instant::now();

        // u16 holds 65536 × 16/3 µs = 349_525.33 µs: 349_525 µs is the last tick, 349_526 wraps.
        let mut ds4 = SensorClock::dualshock4();
        ds4.ticks(t0);
        assert_eq!(ds4.ds4_ticks(t0 + Duration::from_micros(349_525)), 65_535);
        assert_eq!(ds4.ds4_ticks(t0 + Duration::from_micros(349_526)), 0);

        // 3 ticks/µs: the first microsecond past u32 wrap lands on 2, not 0.
        let mut ds = SensorClock::dualsense();
        ds.ticks(t0);
        let past_wrap = Duration::from_micros(u32::MAX as u64 / 3 + 1);
        assert_eq!(ds.ds_ticks(t0 + past_wrap), 2);
    }
}
