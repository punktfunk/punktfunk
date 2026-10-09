//! One report window as the controller sees it.
//!
//! The pump's [`WindowAccumulator`](super::window) fills a [`WindowSample`]
//! per 750 ms tick; [`BitrateController::on_window`](super::BitrateController::on_window)
//! consumes it. `None` on a latency field means nobody reports that signal —
//! absent, not clean.

/// One report window: the pump's tick, the controller's unit of time, and
/// the cadence the host reads a missing loss report against.
pub const WINDOW: std::time::Duration = std::time::Duration::from_micros(WINDOW_US as u64);
/// The same, in µs, where the arithmetic is integer.
pub(crate) const WINDOW_US: i64 = 750_000;

/// New-content evidence for one report window.
///
/// [`Active`](Self::Active)`(0)` is observed stillness (every arrived AU a
/// host-marked repeat) and counts toward idle re-arm. [`Empty`](Self::Empty)
/// is the same neutrality for climb and baselines, but no AU arrived, so it
/// does not count. [`Unmarked`](Self::Unmarked) is an older host that never
/// flags repeats: wall-clock arithmetic, never idle. The pump never maps an
/// empty receive window onto [`Unmarked`](Self::Unmarked).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowActivity {
    /// Host does not mark repeats. Wall-clock; never idle.
    Unmarked,
    /// No AU completed. Quiet like idle; does not count toward re-arm.
    Empty,
    /// New-content AU count. `0` = every arrived AU was a host-marked repeat.
    Active(u32),
}

impl WindowActivity {
    /// Every arrived AU was a host-marked repeat.
    pub fn idle(self) -> bool {
        matches!(self, Self::Active(0))
    }

    /// No new-content evidence: skip climb, baselines, re-probe.
    pub fn quiet(self) -> bool {
        matches!(self, Self::Empty | Self::Active(0))
    }
}

/// One-way delay as the window's first-shard samples saw it.
///
/// A shard arrives whether or not its frame ever completes, so this survives
/// the overload a completed-AU reading goes blind in. [`rise_us`](Self::rise_us)
/// is the signal the rate reads: a queue that is still filling against one
/// that is draining.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelayTrend {
    /// Frames that opened this window. One sample each.
    pub samples: u32,
    pub mean_us: i64,
    /// Least-squares fit from the window's first sample to its last, µs.
    /// Negative = the queue is draining.
    pub rise_us: i64,
    pub last_us: i64,
}

/// What one closed report window carries into the controller.
///
/// `actual_kbps` is the wire rate (headers, seals and FEC parity included,
/// probe filler netted out, audio reservation added) so it lives in the same
/// domain as the target. `dropped` counts unrecoverable frames, `flushed` is
/// a jump-to-live, `recovery_kf` the decode-recovery keyframe asks.
///
/// `owd_mean_us` is the completed-AU delay the rolling baseline is built from;
/// `delay` is the same quantity over arriving shards, which stays present when
/// no frame completes at all.
#[derive(Clone, Copy, Debug)]
pub struct WindowSample {
    pub now: std::time::Instant,
    pub dropped: u64,
    pub loss_ppm: u32,
    pub owd_mean_us: Option<i64>,
    /// `None` = no frame opened this window.
    pub delay: Option<DelayTrend>,
    pub decode_mean_us: Option<i64>,
    pub encode_mean_us: Option<i64>,
    pub actual_kbps: u32,
    pub flushed: bool,
    pub recovery_kf: u32,
    pub activity: WindowActivity,
    /// Shards that never arrived, by their place in the frame: the first twelve data
    /// shards, the middle, and the last twelve with all parity
    /// ([`Stats::loss_head`](crate::stats::Stats::loss_head)).
    pub head: u32,
    pub mid: u32,
    pub tail: u32,
    /// Packets this device dropped before the session read them: its kernel socket's and its
    /// demux queue's.
    pub sock_drops: u32,
}

impl WindowSample {
    /// An undamaged, signal-free window at `now`: the base a test fills in.
    /// The accumulator builds its samples whole.
    #[cfg(test)]
    pub(crate) fn at(now: std::time::Instant) -> Self {
        WindowSample {
            now,
            dropped: 0,
            loss_ppm: 0,
            owd_mean_us: None,
            delay: None,
            decode_mean_us: None,
            encode_mean_us: None,
            actual_kbps: 0,
            flushed: false,
            recovery_kf: 0,
            activity: WindowActivity::Unmarked,
            head: 0,
            mid: 0,
            tail: 0,
            sock_drops: 0,
        }
    }
}
