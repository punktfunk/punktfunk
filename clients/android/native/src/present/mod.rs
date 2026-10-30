//! The present policy both sinks share, pure and host-tested: which held frame goes to glass,
//! when, aimed at what, and what is dropped and counted. Monotonic `i64` ns, frame handles and
//! counters only; the sinks in `decode::sink` own the codec, the reader, the layers and the logs.
//!
//!  * [`store`]: the held frames, a FIFO on source-cadence due times, two deep under latency.
//!  * [`budget`]: the SurfaceView timeline budget and the ASC in-flight budget.
//!  * [`slots`]: the ASC slot scheduler on the present-phased grid.
//!  * [`verdict`]: the down-rated panel rule and the once-a-second verdicts the sinks act on.
//!
//! The replay tests beside each module pin today's decisions. Design:
//! `design/android-presenter-overhaul-v3.md` §3.2 (planning repo).

pub(crate) mod budget;
pub(crate) mod slots;
pub(crate) mod store;
pub(crate) mod verdict;

use punktfunk_core::phase::{CadenceHealth, SlotIntervals};

/// The grid period while none is known, and the frame interval of an unnegotiated rate: one
/// 120 Hz frame.
pub(crate) const FALLBACK_PERIOD_NS: i64 = 8_333_333;

/// The user's presentation intent — the Apple client's `PresentPriority`, same resolution rules:
/// anything but an explicit "smooth" is latency; a smooth buffer outside 1..=3 becomes 2.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PresentPriority {
    /// Two frames deep on the source cadence, cushioned by one MAD of measured arrival jitter
    /// and the late tail; a frame due for a taken slot shares it. The default.
    Latency,
    /// A small holding store (1..=3 frames) drained on each frame's DUE time from the source's own
    /// timestamps: transport jitter absorbed in the cushion the cadence loop sizes from its own
    /// measured residual, at a latency the metrics show rather than hide.
    Smooth { buffer: usize },
}

impl PresentPriority {
    /// From the JNI ints (`presentPriority` 0 = latency / 1 = smooth; `smoothBuffer` 0 = auto).
    pub(crate) fn resolve(priority: i32, buffer: i32) -> PresentPriority {
        if priority != 1 {
            return PresentPriority::Latency;
        }
        let b = if (1..=3).contains(&buffer) {
            buffer as usize
        } else {
            2
        };
        PresentPriority::Smooth { buffer: b }
    }
}

/// The source's frame interval at the NEGOTIATED stream rate, not the panel's: the cadence
/// loop's cushion ceiling, since a cushion past one source frame buys smoothness the source
/// cannot supply. 0 (unnegotiated) takes [`FALLBACK_PERIOD_NS`], the tighter plausible answer:
/// too low costs smoothness, too high costs latency the user never asked for.
pub(crate) fn frame_interval_ns(source_hz: u32) -> i64 {
    match source_hz {
        0 => FALLBACK_PERIOD_NS,
        hz => 1_000_000_000 / i64::from(hz),
    }
}

/// The 1 Hz line's cadence-loop tail: `late‰` of all frames folded (a due time already past
/// when the frame became presentable — the direct signal the cushion is too small), the loop
/// residual's jitter, the cushion, and re-anchors. Counters are cumulative since the last
/// re-anchor, so `late` reads as a rate.
pub(crate) fn cadence_suffix(h: CadenceHealth) -> String {
    format!(
        " late={}‰ jitterMs={:.2} cushionMs={:.2} reanchors={}",
        h.late.saturating_mul(1000) / h.frames.max(1),
        h.jitter_ns as f64 / 1e6,
        h.cushion_ns as f64 / 1e6,
        h.reanchors,
    )
}

/// A sink's present counters for its 1 Hz `pf.present` line. The policy counts into it; each
/// sink formats its own line and resets the fields that line reports per window.
#[derive(Debug, Default)]
pub(crate) struct Counters {
    /// Presents handed to the display.
    pub(crate) released: u64,
    /// ASC: presents on glass, by fence or latch. The SurfaceView line reads its confirms instead.
    pub(crate) displays: u64,
    /// Frames the store evicted before they presented.
    pub(crate) skipped: u64,
    /// Budgets the stale backstop force-opened, and SurfaceView confirm ledgers written off.
    pub(crate) forced: u64,
    /// SurfaceView: budgets an `OnFrameRendered` confirm reopened before the predicted latch.
    pub(crate) confirmed: u64,
    /// SurfaceView: pumps that parked a frame on a closed budget.
    pub(crate) no_budget: u64,
    /// SurfaceView: vsync ticks that found the smoothing store empty — supply depth, not an alarm.
    pub(crate) dry: u64,
    /// SurfaceView: of `no_budget`, the pumps the unconfirmed-release rail held back.
    pub(crate) queue_waits: u64,
    /// ASC: completions without a latch — the transaction never reached glass as a frame.
    pub(crate) unlatched: u64,
    /// ASC: frames aimed one slot or more past the first reachable one.
    pub(crate) held: u64,
    /// ASC: frames SurfaceFlinger latched for a later vsync than they were aimed at.
    pub(crate) slot_miss: u64,
    /// ASC: completions that shared the previous one's latch — SurfaceFlinger showed one of two.
    pub(crate) coalesced: u64,
    /// ASC: present spacings in whole slots.
    pub(crate) intervals: SlotIntervals,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_explicit_smooth_is_smooth_and_its_buffer_stays_in_one_to_three() {
        let table: &[(i32, i32, PresentPriority)] = &[
            (0, 0, PresentPriority::Latency),
            (0, 3, PresentPriority::Latency),
            (2, 1, PresentPriority::Latency),
            (-1, 2, PresentPriority::Latency),
            (1, 0, PresentPriority::Smooth { buffer: 2 }),
            (1, 1, PresentPriority::Smooth { buffer: 1 }),
            (1, 3, PresentPriority::Smooth { buffer: 3 }),
            (1, 4, PresentPriority::Smooth { buffer: 2 }),
            (1, -1, PresentPriority::Smooth { buffer: 2 }),
        ];
        for &(priority, buffer, want) in table {
            assert_eq!(
                PresentPriority::resolve(priority, buffer),
                want,
                "{priority}/{buffer}"
            );
        }
    }

    #[test]
    fn an_unnegotiated_rate_takes_one_120_hz_frame() {
        assert_eq!(frame_interval_ns(0), FALLBACK_PERIOD_NS);
        assert_eq!(frame_interval_ns(60), 16_666_666);
        assert_eq!(frame_interval_ns(120), 8_333_333);
    }

    #[test]
    fn the_cadence_tail_is_byte_stable() {
        let h = CadenceHealth {
            frames: 2000,
            late: 3,
            reanchors: 2,
            jitter_ns: 412_000,
            cushion_ns: 1_250_000,
            ..CadenceHealth::default()
        };
        assert_eq!(
            cadence_suffix(h),
            " late=1‰ jitterMs=0.41 cushionMs=1.25 reanchors=2"
        );
    }
}
