//! Client-side loss asks (`RfiRecovery`: the decoder's gaps and the pump's short tails),
//! the recent-RFI count, and what the short frames an RFI names still lacked.

use std::time::{Duration, Instant};

/// One RFI ask per window so a burst of gaps cannot storm the control stream.
/// The host coalesces further.
const RFI_THROTTLE: Duration = Duration::from_millis(100);

/// Short tails remembered so the decoder's later gap over them asks nothing. The gap
/// arrives within the loss window (~120 ms), a few frames at any rate.
const ASKED_TAILS: usize = 32;

/// Loss asks behind one throttle: the decoder's gaps ([`NativeClient::note_frame_index`])
/// and the pump's short tails ([`Self::tail_short`]). Wrapping `frame_index` arithmetic
/// lives here so embedders do not each re-derive it.
#[derive(Default)]
pub(crate) struct RfiRecovery {
    next_expected: Option<u32>,
    last_req: Option<Instant>,
    /// Lost range the throttle swallowed, widened by later asks; sent by the first
    /// `observe`, `tail_short` or `flush` after the window opens. Otherwise a second gap
    /// inside the window (a lost recovery anchor) asks nothing until the 500 ms backstop.
    pending: Option<(u32, u32)>,
    /// Frames already asked for at their tail, newest last.
    asked: std::collections::VecDeque<u32>,
}

/// `a` is ahead of `b` in half-space wrap order.
fn ahead_of(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

/// Where one AU's `frame_index` falls in receive order, from
/// [`NativeClient::observe_frame_index`](super::NativeClient::observe_frame_index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameOrder {
    /// The first index seen, or the expected next one.
    InOrder,
    /// A forward jump over this many lost frames.
    Gap(u32),
    /// Behind a frame already seen: a reordered or duplicate AU.
    Straggler,
}

/// Recovery request for a forward gap. Keyframe when the span exceeds
/// [`crate::packet::RFI_MAX_RANGE`]: no encoder still holds that reference.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecoveryAsk {
    None,
    Rfi(u32, u32),
    Keyframe,
}

impl RfiRecovery {
    /// `order` and `ask` are independent: throttle can yield [`RecoveryAsk::None`]
    /// on a [`FrameOrder::Gap`], so can a gap whose frames were asked for at their tail,
    /// and an in-order frame can carry the ask a throttled gap deferred. Pass the gap
    /// width to [`crate::reanchor::ReanchorGate::arm_expecting_drops`] or the
    /// reassembler's later `frames_dropped` climb is counted as a second loss.
    pub(crate) fn observe(&mut self, frame_index: u32, now: Instant) -> (FrameOrder, RecoveryAsk) {
        let order = match self.next_expected {
            Some(exp) => {
                // Half-space wrap: wrapping_sub < u32::MAX/2 is a forward gap; top half is a straggler.
                let ahead = frame_index.wrapping_sub(exp);
                if ahead == 0 {
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    FrameOrder::InOrder
                } else if ahead < u32::MAX / 2 {
                    // Advance past this frame so the same gap cannot re-fire. The oldest
                    // unsent loss stays `first`: the host invalidates everything since it.
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    if let Some(lost) = self.unasked(exp, frame_index.wrapping_sub(1)) {
                        self.widen(lost);
                    }
                    FrameOrder::Gap(ahead)
                } else {
                    // Leave next_expected: a rewind would false-gap the next in-order frame.
                    FrameOrder::Straggler
                }
            }
            None => {
                self.next_expected = Some(frame_index.wrapping_add(1));
                FrameOrder::InOrder
            }
        };
        (order, self.flush(now))
    }

    /// The pump saw `frame_index`'s last shard arrive short of what its parity can
    /// rebuild. Asks now, a frame interval before the next frame shows the gap, so the
    /// host's next encode is the anchor. A frame the decoder already passed, or one
    /// asked for before, asks nothing new.
    pub(crate) fn tail_short(&mut self, frame_index: u32, now: Instant) -> RecoveryAsk {
        let passed = self
            .next_expected
            .is_some_and(|exp| ahead_of(exp, frame_index));
        if !passed && !self.asked.contains(&frame_index) {
            if self.asked.len() == ASKED_TAILS {
                self.asked.pop_front();
            }
            self.asked.push_back(frame_index);
            self.widen((frame_index, frame_index));
        }
        self.flush(now)
    }

    /// `[first, last]` less the frames asked for at their tail at either end; `None`
    /// when every one was.
    fn unasked(&self, mut first: u32, mut last: u32) -> Option<(u32, u32)> {
        while self.asked.contains(&first) {
            if first == last {
                return None;
            }
            first = first.wrapping_add(1);
        }
        while self.asked.contains(&last) {
            last = last.wrapping_sub(1);
        }
        Some((first, last))
    }

    /// Merge `lost` into the pending range: oldest first, newest last.
    fn widen(&mut self, (first, last): (u32, u32)) {
        self.pending = Some(match self.pending {
            Some((f, l)) => (
                if ahead_of(f, first) { first } else { f },
                if ahead_of(last, l) { last } else { l },
            ),
            None => (first, last),
        });
    }

    /// An IDR was asked for: it repairs every frame the pending range names. The RFI
    /// throttle keeps its own window; an IDR ask does not hold back the next RFI.
    pub(crate) fn keyframe_requested(&mut self) {
        self.pending = None;
    }

    /// The pending range as an ask once the throttle window is open, else `None`.
    pub(crate) fn flush(&mut self, now: Instant) -> RecoveryAsk {
        let throttled = self
            .last_req
            .is_some_and(|t| now.duration_since(t) < RFI_THROTTLE);
        let Some((first, last)) = self.pending.filter(|_| !throttled) else {
            return RecoveryAsk::None;
        };
        self.pending = None;
        self.last_req = Some(now);
        if last.wrapping_sub(first).wrapping_add(1) > crate::packet::RFI_MAX_RANGE {
            RecoveryAsk::Keyframe
        } else {
            RecoveryAsk::Rfi(first, last)
        }
    }
}

/// One minute, the span of the host's `link health … rfi=` line.
const RECENT_SPAN: Duration = Duration::from_secs(60);

/// When each RFI in the last [`RECENT_SPAN`] went out, for the overlay.
#[derive(Default)]
pub(crate) struct RecentRfis(std::collections::VecDeque<Instant>);

impl RecentRfis {
    pub(crate) fn note(&mut self, now: Instant) {
        self.count(now);
        self.0.push_back(now);
    }

    /// RFIs sent in the minute before `now`. Ages out the older ones.
    pub(crate) fn count(&mut self, now: Instant) -> u32 {
        while self
            .0
            .front()
            .is_some_and(|&t| now.duration_since(t) >= RECENT_SPAN)
        {
            self.0.pop_front();
        }
        self.0.len() as u32
    }
}

/// Gaps remembered for the RFI line. More than the frame queue holds before
/// jump-to-live, so a gap is still here when the decoder reaches it.
const SHORT_FRAMES: usize = 16;

/// Frames the pump saw short, each with `(missing, recovery)` from
/// [`crate::session::Session::missing_beyond_parity`]. The pump writes on a short tail
/// or a forward gap; [`NativeClient::request_rfi`] reads the first frame of its range.
/// Both are rare, so a lock is fine.
///
/// [`NativeClient::request_rfi`]: super::NativeClient::request_rfi
#[derive(Default)]
pub(crate) struct ShortFrames(std::collections::VecDeque<(u32, u32, u32)>);

impl ShortFrames {
    pub(crate) fn note(&mut self, frame_index: u32, missing: u32, recovery: u32) {
        if self.0.len() == SHORT_FRAMES {
            self.0.pop_front();
        }
        self.0.push_back((frame_index, missing, recovery));
    }

    /// `(missing, recovery)` for `frame_index`, if the pump saw it short.
    pub(crate) fn get(&self, frame_index: u32) -> Option<(u32, u32)> {
        self.0
            .iter()
            .rev()
            .find(|e| e.0 == frame_index)
            .map(|&(_, missing, recovery)| (missing, recovery))
    }
}

/// Moves `last` to `idx` when `idx` is newer and returns the first index the jump
/// skipped. A repeat (a later part of the same AU) and a straggler leave `last`.
pub(crate) fn first_skipped(last: &mut Option<u32>, idx: u32) -> Option<u32> {
    let prev = last.replace(idx)?;
    let ahead = idx.wrapping_sub(prev);
    if ahead == 0 || ahead >= u32::MAX / 2 {
        *last = Some(prev);
        return None;
    }
    (ahead > 1).then(|| prev.wrapping_add(1))
}

#[cfg(test)]
mod rfi_recovery_tests {
    use super::{
        first_skipped, FrameOrder::*, RecentRfis, RecoveryAsk, RfiRecovery, ShortFrames,
        RFI_THROTTLE, SHORT_FRAMES,
    };
    use std::time::{Duration, Instant};

    // Offsets from this Instant model the throttle window; do not sleep.
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_arms_without_a_gap() {
        let mut r = RfiRecovery::default();
        assert_eq!(r.observe(100, base()), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(101));
    }

    #[test]
    fn contiguous_frames_never_gap() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(101, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(102, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(103, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(104));
    }

    #[test]
    fn forward_gap_reports_the_exact_lost_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(105, t), (Gap(4), RecoveryAsk::Rfi(101, 104)));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn single_frame_drop_names_a_unit_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (Gap(1), RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn throttle_suppresses_bursts_then_re_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(105, t0), (Gap(4), RecoveryAsk::Rfi(101, 104)));
        assert_eq!(
            r.observe(110, t0 + Duration::from_millis(50)),
            (Gap(4), RecoveryAsk::None)
        );
        // The swallowed gap widens the next ask instead of vanishing.
        assert_eq!(
            r.observe(120, t0 + RFI_THROTTLE + Duration::from_millis(1)),
            (Gap(9), RecoveryAsk::Rfi(106, 119))
        );
    }

    #[test]
    fn a_swallowed_gap_is_sent_by_the_next_frame_after_the_window() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        // The recovery anchor itself is lost: a second gap inside the window.
        assert_eq!(
            r.observe(104, t0 + Duration::from_millis(30)),
            (Gap(1), RecoveryAsk::None)
        );
        assert_eq!(
            r.observe(105, t0 + Duration::from_millis(60)),
            (InOrder, RecoveryAsk::None)
        );
        assert_eq!(
            r.observe(106, t0 + RFI_THROTTLE),
            (InOrder, RecoveryAsk::Rfi(103, 103))
        );
        assert_eq!(
            r.observe(107, t0 + RFI_THROTTLE),
            (InOrder, RecoveryAsk::None)
        );
    }

    #[test]
    fn a_swallowed_gap_is_sent_by_a_tick_once_the_window_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        assert_eq!(
            r.observe(104, t0 + Duration::from_millis(30)),
            (Gap(1), RecoveryAsk::None)
        );
        // No frame arrives after the second gap; the pump's tick still sends it.
        assert_eq!(r.flush(t0 + Duration::from_millis(60)), RecoveryAsk::None);
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::Rfi(103, 103));
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::None);
    }

    /// An IDR ask drops the pending range but leaves the RFI window alone: a new
    /// gap right after an IDR ask still asks at once.
    #[test]
    fn a_keyframe_ask_clears_the_pending_range_but_not_the_rfi_window() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        r.keyframe_requested();
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        // Swallowed by the RFI window, then repaired by the IDR.
        assert_eq!(
            r.observe(104, t0 + Duration::from_millis(30)),
            (Gap(1), RecoveryAsk::None)
        );
        r.keyframe_requested();
        assert_eq!(
            r.observe(105, t0 + RFI_THROTTLE),
            (InOrder, RecoveryAsk::None)
        );
    }

    #[test]
    fn a_reordered_pair_is_one_gap_then_a_straggler() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        // The late 101 is a straggler: no gap, expectation untouched.
        assert_eq!(r.observe(101, t), (Straggler, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(103));
        assert_eq!(r.observe(103, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(104));
    }

    #[test]
    fn stragglers_behind_the_delivery_point_are_ignored() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        r.observe(105, t);
        assert_eq!(r.observe(103, t), (Straggler, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn wraparound_is_contiguous_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t);
        assert_eq!(r.observe(u32::MAX, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(0));
        assert_eq!(r.observe(0, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(1));
    }

    #[test]
    fn gap_range_wraps_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t);
        assert_eq!(r.observe(1, t), (Gap(2), RecoveryAsk::Rfi(u32::MAX, 0)));
        assert_eq!(r.next_expected, Some(2));
    }

    #[test]
    fn huge_gap_resyncs_via_keyframe_not_rfi() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        let jump = 100 + crate::packet::RFI_MAX_RANGE + 2;
        assert_eq!(r.observe(jump, t), (Gap(jump - 101), RecoveryAsk::Keyframe));
        assert_eq!(r.next_expected, Some(jump + 1));
        assert_eq!(r.observe(jump + 1, t), (InOrder, RecoveryAsk::None));
        // Keyframe stamps last_req too; an immediate follow-up gap stays quiet.
        assert_eq!(
            r.observe(jump + 10, t + Duration::from_millis(1)),
            (Gap(8), RecoveryAsk::None)
        );
    }

    #[test]
    fn a_short_tail_asks_before_the_gap_and_the_gap_asks_nothing() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.tail_short(101, t), RecoveryAsk::Rfi(101, 101));
        let later = t + RFI_THROTTLE;
        assert_eq!(r.observe(102, later), (Gap(1), RecoveryAsk::None));
        assert_eq!(r.flush(later), RecoveryAsk::None);
    }

    #[test]
    fn a_gap_wider_than_the_short_tails_asks_for_the_rest() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.tail_short(101, t), RecoveryAsk::Rfi(101, 101));
        let later = t + RFI_THROTTLE;
        assert_eq!(r.observe(103, later), (Gap(2), RecoveryAsk::Rfi(102, 102)));
        let mut r = RfiRecovery::default();
        r.observe(100, t);
        assert_eq!(r.tail_short(102, t), RecoveryAsk::Rfi(102, 102));
        assert_eq!(r.observe(103, later), (Gap(2), RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn a_throttled_short_tail_goes_out_once() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        let t1 = t0 + Duration::from_millis(30);
        assert_eq!(r.tail_short(105, t1), RecoveryAsk::None);
        assert_eq!(r.observe(103, t1), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(104, t1), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(106, t1), (Gap(1), RecoveryAsk::None));
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::Rfi(105, 105));
        assert_eq!(r.flush(t0 + RFI_THROTTLE * 2), RecoveryAsk::None);
    }

    #[test]
    fn a_short_tail_the_decoder_passed_or_already_asked_asks_nothing() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        let later = t + RFI_THROTTLE;
        assert_eq!(r.tail_short(101, later), RecoveryAsk::None, "passed");
        assert_eq!(r.tail_short(103, later), RecoveryAsk::Rfi(103, 103));
        let much_later = later + RFI_THROTTLE;
        assert_eq!(r.tail_short(103, much_later), RecoveryAsk::None, "asked");
    }

    #[test]
    fn short_tails_merge_across_the_wrap() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(u32::MAX - 3, t0);
        assert_eq!(
            r.tail_short(u32::MAX - 2, t0),
            RecoveryAsk::Rfi(u32::MAX - 2, u32::MAX - 2)
        );
        let t1 = t0 + Duration::from_millis(30);
        assert_eq!(r.tail_short(0, t1), RecoveryAsk::None);
        assert_eq!(r.tail_short(u32::MAX, t1), RecoveryAsk::None);
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::Rfi(u32::MAX, 0));
    }

    #[test]
    fn a_jump_names_its_first_skipped_frame_and_parts_or_stragglers_do_not() {
        let mut last = None;
        assert_eq!(first_skipped(&mut last, 100), None);
        assert_eq!(first_skipped(&mut last, 101), None);
        assert_eq!(first_skipped(&mut last, 101), None, "a later part");
        assert_eq!(first_skipped(&mut last, 104), Some(102));
        assert_eq!(first_skipped(&mut last, 102), None, "a straggler");
        assert_eq!(last, Some(104));
        let mut last = Some(u32::MAX);
        assert_eq!(first_skipped(&mut last, 1), Some(0));
    }

    #[test]
    fn short_frames_keep_the_newest_and_forget_the_oldest() {
        let mut s = ShortFrames::default();
        for i in 0..=SHORT_FRAMES as u32 {
            s.note(i, i + 1, 2);
        }
        assert_eq!(s.get(0), None, "evicted");
        assert_eq!(s.get(1), Some((2, 2)));
        assert_eq!(s.get(SHORT_FRAMES as u32 + 1), None, "never short");
    }

    #[test]
    fn recent_rfis_age_out_after_a_minute() {
        let t0 = base();
        let at = |s| t0 + Duration::from_secs(s);
        let mut r = RecentRfis::default();
        assert_eq!(r.count(t0), 0);
        r.note(t0);
        r.note(at(30));
        assert_eq!(r.count(at(59)), 2);
        assert_eq!(r.count(at(60)), 1);
        r.note(at(61));
        assert_eq!(r.count(at(89)), 2);
        assert_eq!(r.count(at(121)), 0);
    }
}
