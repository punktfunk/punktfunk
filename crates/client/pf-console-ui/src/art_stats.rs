//! What filling a shelf with cover art has cost this run.
//!
//! Exists for one question the shell cannot answer from a frame time alone: when a slow panel
//! stutters while a library loads, is that poster DECODING or something else in the frame? The
//! counters here separate the two, so a host can report both and a change can be measured
//! rather than guessed at.
//!
//! Process-wide and free-running: decoding happens on whatever thread draws, there is one
//! shelf being filled at a time, and a host reads deltas between two snapshots rather than
//! asking for a reset. Relaxed ordering throughout — these are counters for a human, not a
//! synchronisation edge, and no decision is made on them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static DECODED: AtomicU64 = AtomicU64::new(0);
static TOTAL_US: AtomicU64 = AtomicU64::new(0);
static MAX_US: AtomicU64 = AtomicU64::new(0);
static NATIVE_SCALED: AtomicU64 = AtomicU64::new(0);

/// One finished poster decode. `native_scaled` is whether the codec produced it at (or near)
/// the cached size itself rather than the caller resampling a full-size decode — the
/// difference the fast path exists to make, and the thing to check first if a device is slower
/// than it should be.
pub(crate) fn record(took: Duration, native_scaled: bool) {
    let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
    DECODED.fetch_add(1, Ordering::Relaxed);
    TOTAL_US.fetch_add(us, Ordering::Relaxed);
    MAX_US.fetch_max(us, Ordering::Relaxed);
    if native_scaled {
        NATIVE_SCALED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Poster decoding so far this run. Fields are cumulative; two snapshots subtract.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ArtStats {
    pub decoded: u64,
    pub total_us: u64,
    /// The single worst decode. A shelf's stutter is usually one of these, not the mean.
    pub max_us: u64,
    /// How many took the codec's own downscale. Well short of `decoded` means most art is
    /// arriving in a format or a size the fast path cannot help with.
    pub native_scaled: u64,
}

impl ArtStats {
    /// Mean microseconds per decode, or 0 with nothing decoded.
    #[must_use]
    pub fn mean_us(&self) -> u64 {
        self.total_us.checked_div(self.decoded).unwrap_or(0)
    }

    /// What happened between an earlier snapshot and this one. Saturating: a host that
    /// snapshots in the wrong order gets zeroes rather than a wrapped count.
    #[must_use]
    pub fn since(&self, earlier: ArtStats) -> ArtStats {
        ArtStats {
            decoded: self.decoded.saturating_sub(earlier.decoded),
            total_us: self.total_us.saturating_sub(earlier.total_us),
            // Not a difference: the max is a high-water mark, so the later one already is it.
            max_us: self.max_us,
            native_scaled: self.native_scaled.saturating_sub(earlier.native_scaled),
        }
    }
}

/// Read the counters. Not a consistent snapshot across all four — a decode landing mid-read
/// can leave the count one ahead of the total, which rounds the mean and nothing else.
#[must_use]
pub fn art_stats() -> ArtStats {
    ArtStats {
        decoded: DECODED.load(Ordering::Relaxed),
        total_us: TOTAL_US.load(Ordering::Relaxed),
        max_us: MAX_US.load(Ordering::Relaxed),
        native_scaled: NATIVE_SCALED.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deltas a host reports are the point of the type; the max deliberately is not one.
    #[test]
    fn a_delta_subtracts_the_counts_and_keeps_the_high_water_mark() {
        let before = ArtStats {
            decoded: 10,
            total_us: 1_000,
            max_us: 400,
            native_scaled: 8,
        };
        let now = ArtStats {
            decoded: 14,
            total_us: 1_600,
            max_us: 900,
            native_scaled: 11,
        };
        let d = now.since(before);
        assert_eq!(d.decoded, 4);
        assert_eq!(d.total_us, 600);
        assert_eq!(d.native_scaled, 3);
        assert_eq!(
            d.max_us, 900,
            "the worst decode is a mark, not a difference"
        );
        assert_eq!(d.mean_us(), 150);
    }

    /// Snapshots taken out of order say nothing happened rather than a wrapped count.
    #[test]
    fn an_out_of_order_delta_is_empty_not_enormous() {
        let later = ArtStats {
            decoded: 9,
            total_us: 900,
            max_us: 100,
            native_scaled: 9,
        };
        let d = later.since(ArtStats {
            decoded: 20,
            total_us: 2_000,
            max_us: 100,
            native_scaled: 20,
        });
        assert_eq!(d.decoded, 0);
        assert_eq!(d.total_us, 0);
        assert_eq!(
            d.mean_us(),
            0,
            "no decodes means no mean, not a division trap"
        );
    }
}
