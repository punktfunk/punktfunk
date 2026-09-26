//! Hung-decoder detection for the MediaCodec decode loop.

use std::time::{Duration, Instant};

/// How long access units may wait while the codec frees no input slot before the decoder counts
/// as hung. A working codec frees a slot per decoded frame: 1 s is 60 missed slots at 60 Hz.
pub(crate) const INPUT_STALL_PATIENCE: Duration = Duration::from_secs(1);

/// Trips once access units have waited [`INPUT_STALL_PATIENCE`] with no input slot offered.
///
/// A hung hardware decoder raises no error. It stops taking input, so no keyframe can reach it,
/// and the loop's keyframe requests change nothing. A codec that takes no input shows no new
/// picture either, so the screen is already frozen whenever this trips.
#[derive(Default)]
pub(crate) struct InputStall {
    since: Option<Instant>,
}

impl InputStall {
    /// One loop pass. `offered`: the codec freed an input slot this pass. `waiting`: access units
    /// are still parked after feeding.
    pub(crate) fn poll(&mut self, offered: bool, waiting: bool, now: Instant) -> bool {
        if offered || !waiting {
            self.since = None;
            return false;
        }
        now.duration_since(*self.since.get_or_insert(now)) >= INPUT_STALL_PATIENCE
    }
}

#[cfg(test)]
mod tests {
    use super::{InputStall, INPUT_STALL_PATIENCE};
    use std::time::{Duration, Instant};

    #[test]
    fn only_a_codec_that_stops_taking_input_trips() {
        let mut s = InputStall::default();
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        // A working codec frees a slot each pass: never trips, however long the backlog.
        for i in 0..300 {
            assert!(!s.poll(true, true, t0 + ms(i * 10)));
        }
        // A still host parks nothing, so an idle stretch never counts against the codec.
        assert!(!s.poll(false, false, t0 + ms(8000)));
        // The first AU after the pause starts the clock instead of tripping on the pause.
        let t1 = t0 + ms(10_000);
        assert!(!s.poll(false, true, t1));
        assert!(!s.poll(false, true, t1 + INPUT_STALL_PATIENCE - ms(1)));
        assert!(s.poll(false, true, t1 + INPUT_STALL_PATIENCE));
        // One freed slot restarts the wait.
        assert!(!s.poll(true, true, t1 + INPUT_STALL_PATIENCE + ms(5)));
        assert!(!s.poll(false, true, t1 + INPUT_STALL_PATIENCE + ms(10)));
    }
}
