//! Controller updates reaching the host: the client → host half of the link.
//!
//! A snapshot client re-sends every live pad's whole state each 100 ms and stamps every send
//! with the next seq (`client/pump/input_task.rs`). So a live pad is a 10 Hz beacon: a hole in
//! it is the uplink going quiet, and a seq skip is a datagram the path lost. Across a hole,
//! `lost` near `silence_ms / 100` means the client kept sending and the path dropped it; `lost`
//! 0 means the client sent nothing.

use punktfunk_core::input::MAX_PADS;
use std::time::{Duration, Instant};

/// Four refreshes lost in a row. Random loss at 5 % does that about once every four hours.
const GAP_WARN: Duration = Duration::from_millis(500);
/// The window line's period: the host's `wire egress` line runs on the same one.
const WINDOW: Duration = Duration::from_secs(30);
const WARN_MIN_GAP: Duration = Duration::from_secs(1);

/// One hole in a pad's updates that just ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GapReport {
    pub pad: usize,
    pub silence_ms: u32,
    /// Seqs skipped across the hole: updates the client sent that never arrived.
    pub lost: u32,
}

/// Counts since the last window line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WindowReport {
    pub updates: u64,
    pub lost: u64,
    pub max_gap_ms: u32,
}

pub(super) struct PadUplink {
    /// Per wire pad: last arrival and the newest seq seen.
    last: [Option<(Instant, u8)>; MAX_PADS],
    window_start: Instant,
    updates: u64,
    lost: u64,
    max_gap: Duration,
    last_warn: Option<Instant>,
}

impl PadUplink {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            last: [None; MAX_PADS],
            window_start: now,
            updates: 0,
            lost: 0,
            max_gap: Duration::ZERO,
            last_warn: None,
        }
    }

    /// One snapshot as it arrived, before the apply gate. `Some` when it ended a hole worth a
    /// line. A reordered or repeated seq counts as an arrival and skips nothing.
    pub(super) fn note(&mut self, pad: usize, seq: u8, now: Instant) -> Option<GapReport> {
        let slot = self.last.get_mut(pad)?;
        self.updates += 1;
        let (at, prev) = slot.replace((now, seq))?;
        let ahead = seq.wrapping_sub(prev);
        let lost = if (1..128).contains(&ahead) {
            u32::from(ahead - 1)
        } else {
            // Keep the newer seq, or a late datagram would make the next one read as a skip.
            *slot = Some((now, prev));
            0
        };
        self.lost += u64::from(lost);
        let gap = now.saturating_duration_since(at);
        self.max_gap = self.max_gap.max(gap);
        if gap < GAP_WARN
            || self
                .last_warn
                .is_some_and(|t| now.saturating_duration_since(t) < WARN_MIN_GAP)
        {
            return None;
        }
        self.last_warn = Some(now);
        Some(GapReport {
            pad,
            silence_ms: gap.as_millis().min(u32::MAX as u128) as u32,
            lost,
        })
    }

    /// The pad was unplugged: its silence from here on is not the link.
    pub(super) fn forget(&mut self, pad: usize) {
        if let Some(slot) = self.last.get_mut(pad) {
            *slot = None;
        }
    }

    /// Once per [`WINDOW`], while any pad is live or was heard in it. A window with live pads and
    /// `updates` 0 is an uplink that stayed silent the whole time.
    pub(super) fn take_window(&mut self, now: Instant) -> Option<WindowReport> {
        if now.saturating_duration_since(self.window_start) < WINDOW {
            return None;
        }
        self.window_start = now;
        let report = WindowReport {
            updates: std::mem::take(&mut self.updates),
            lost: std::mem::take(&mut self.lost),
            max_gap_ms: std::mem::take(&mut self.max_gap)
                .as_millis()
                .min(u32::MAX as u128) as u32,
        };
        (report.updates > 0 || self.last.iter().any(Option::is_some)).then_some(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_path_that_drops_updates_reports_the_skipped_seqs() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        for i in 0..10u8 {
            assert!(u.note(0, i, t0 + ms(u64::from(i) * 100)).is_none());
        }
        // Seqs 10..=15 lost: 700 ms of silence, then 16 arrives.
        let r = u.note(0, 16, t0 + ms(1600)).expect("a 700 ms hole reports");
        assert_eq!(
            r,
            GapReport {
                pad: 0,
                silence_ms: 700,
                lost: 6
            }
        );
    }

    #[test]
    fn a_client_that_stopped_sending_skips_no_seq() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        u.note(2, 40, t0);
        let r = u.note(2, 41, t0 + ms(2000)).expect("a 2 s hole reports");
        assert_eq!((r.pad, r.silence_ms, r.lost), (2, 2000, 0));
    }

    #[test]
    fn a_reordered_update_neither_skips_nor_rolls_the_seq_back() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        u.note(0, 5, t0);
        u.note(0, 7, t0 + ms(100));
        u.note(0, 6, t0 + ms(110));
        u.note(0, 8, t0 + ms(200));
        let w = u.take_window(t0 + WINDOW).expect("a live pad reports");
        assert_eq!((w.updates, w.lost), (4, 1));
    }

    #[test]
    fn the_seq_wraps_without_a_skip() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        u.note(0, 255, t0);
        u.note(0, 0, t0 + ms(100));
        assert_eq!(u.take_window(t0 + WINDOW).map(|w| w.lost), Some(0));
    }

    #[test]
    fn warnings_are_at_most_one_per_second() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        u.note(0, 0, t0);
        u.note(1, 0, t0);
        assert!(u.note(0, 1, t0 + ms(600)).is_some());
        assert!(
            u.note(1, 1, t0 + ms(700)).is_none(),
            "folded into the first"
        );
        assert!(u.note(0, 2, t0 + ms(1700)).is_some());
    }

    #[test]
    fn an_unplugged_pad_goes_quiet_without_a_line() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        u.note(0, 0, t0);
        u.forget(0);
        assert!(u.note(0, 9, t0 + ms(5000)).is_none());
        assert!(u.take_window(t0 + WINDOW).is_some());
        assert!(
            u.take_window(t0 + WINDOW * 2).is_some(),
            "re-plugged pad is live"
        );
        u.forget(0);
        assert!(u.take_window(t0 + WINDOW * 3).is_none());
    }

    #[test]
    fn an_out_of_range_pad_is_ignored() {
        let t0 = Instant::now();
        let mut u = PadUplink::new(t0);
        assert!(u.note(MAX_PADS, 0, t0).is_none());
        assert!(u.take_window(t0 + WINDOW).is_none());
    }
}
