//! The audio plane's open and recovery rules, split out of the device-only [`crate::audio`] so
//! their tables run on the host. No NDK type and no clock: the shell reads the device, passes
//! plain values in and does what comes back.

/// When the newest frame written plays, ns on the timestamp's clock: the timestamp's time plus
/// the frames written past its position, at `rate_hz`.
pub(crate) fn heard_at(at_ns: i64, position: i64, written: i64, rate_hz: u32) -> i64 {
    at_ns + (written - position) * 1_000_000_000 / i64::from(rate_hz.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heard_at_extrapolates_the_frames_past_the_timestamp() {
        // (row, at_ns, position, written, rate_hz, heard_at)
        let rows = [
            (
                "nothing past the stamp plays at it",
                5_000,
                960,
                960,
                48_000,
                5_000,
            ),
            (
                "480 frames at 48 kHz are 10 ms",
                7,
                100,
                580,
                48_000,
                10_000_007,
            ),
            ("44.1 kHz rounds toward zero", 0, 0, 1, 44_100, 22_675),
            ("a rate of 0 reads as 1 Hz", 0, 0, 1, 0, 1_000_000_000),
        ];
        for (row, at, position, written, rate, want) in rows {
            assert_eq!(heard_at(at, position, written, rate), want, "{row}");
        }
    }
}
