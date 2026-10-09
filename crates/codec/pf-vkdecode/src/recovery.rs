//! Converts H.264/H.265 recovery-point SEI into per-picture [`RecoveryMark`]
//! values. Streams without such SEI produce no marks.
//!
//! `sei_here` and `is_recovery_point` are independent and may both be true.
//! Charge the outstanding target before installing a new SEI: the announcing
//! picture is excluded from a fresh H.264 count and may finish the prior wave.
//! H.264 (§D.2.8): count `frame_num` changes; charge a gap as one increment so
//! uncertain recovery is reported late, never early.
//! H.265 (§D.3.8): target `poc + recovery_poc_cnt`; mark the first picture at
//! or past it. A negative count marks the SEI picture itself.
//! IDR/IRAP clears any outstanding target: its numbering is no longer valid.
//! A repeated SEI starts a new wave only when its implied target advances;
//! shrinking or equal announcements do not set `sei_here`.
//! Independent of wire recovery flags. The consumer, which knows loss timing,
//! must pair an SEI at/after loss with its later recovery point before lifting
//! a post-loss freeze.

use pf_bitstream::h264::RecoveryPoint;
use pf_bitstream::h265::RecoveryPointHevc;

/// Two independent booleans, not an enum: one picture can carry a zero-count
/// SEI and be the recovery point, and the consumer's SEI-then-mark pairing
/// must see both facts on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryMark {
    /// Pair a later [`Self::is_recovery_point`] with this to tell a post-loss
    /// wave from a pre-loss one.
    pub sei_here: bool,
    /// Output from the outstanding SEI's AU through this picture is correct.
    pub is_recovery_point: bool,
}

impl RecoveryMark {
    pub const NONE: RecoveryMark = RecoveryMark {
        sei_here: false,
        is_recovery_point: false,
    };
}

/// Outstanding recovery point, in the codec's counting unit (`frame_num` vs POC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// H.264: `frame_num` increments still owed, and the last charged `frame_num`.
    /// An increment is "this picture's `frame_num` differs", not a gap size.
    FrameNumIncrements { owed: u32, last_frame_num: u16 },
    /// H.265: absolute `PicOrderCntVal` at or past which output is correct.
    Poc(i32),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryWatch {
    target: Option<Target>,
}

impl RecoveryWatch {
    pub fn new() -> RecoveryWatch {
        RecoveryWatch { target: None }
    }

    /// Diagnostics and tests. The decode path reads the per-picture [`RecoveryMark`].
    pub fn is_watching(&self) -> bool {
        self.target.is_some()
    }

    /// Charge this picture against the outstanding target, then maybe install a
    /// new SEI. A wave-start AU may also complete the prior wave. D.2.8 counts
    /// increments *from* the SEI picture, exclusive, so the announcer is not
    /// charged for the fresh target. See [`Self::starts_a_new_wave_h264`].
    pub fn note_h264(
        &mut self,
        frame_num: u16,
        is_idr: bool,
        sei: Option<RecoveryPoint>,
    ) -> RecoveryMark {
        if is_idr {
            // IDR re-anchors and resets `frame_num`; a pending count is invalid past it.
            self.target = None;
        }
        let mut mark = RecoveryMark::NONE;
        // Outstanding owed after charging this picture; a fresh SEI is compared to it.
        let mut owed_here: Option<u32> = None;
        if let Some(Target::FrameNumIncrements {
            owed,
            last_frame_num,
        }) = self.target
        {
            let owed = if frame_num != last_frame_num {
                owed.saturating_sub(1)
            } else {
                owed
            };
            owed_here = Some(owed);
            if owed == 0 {
                mark.is_recovery_point = true;
                self.target = None;
            } else {
                self.target = Some(Target::FrameNumIncrements {
                    owed,
                    last_frame_num: frame_num,
                });
            }
        }
        if let Some(rp) = sei {
            if Self::starts_a_new_wave_h264(owed_here, rp.recovery_frame_cnt) {
                mark.sei_here = true;
                if rp.recovery_frame_cnt == 0 {
                    mark.is_recovery_point = true;
                    self.target = None;
                } else {
                    self.target = Some(Target::FrameNumIncrements {
                        owed: rp.recovery_frame_cnt,
                        last_frame_num: frame_num,
                    });
                }
            }
            // Same or shorter target: keep the further outstanding one (late, never early).
        }
        mark
    }

    /// D.2.8 and x264 `--intra-refresh` re-emit the current wave with a decreasing
    /// `recovery_frame_cnt`. Those must not set `sei_here`: the wave began before
    /// this picture. After IDR, or with no watch, every SEI is new.
    fn starts_a_new_wave_h264(owed_here: Option<u32>, recovery_frame_cnt: u32) -> bool {
        match owed_here {
            Some(owed) => recovery_frame_cnt > owed,
            None => true,
        }
    }

    /// `is_irap`, not `is_idr`: CRA/BLA also re-base POC, so a prior target cannot
    /// be compared past them. A new wave is `poc + delta` strictly greater than
    /// the outstanding [`Target::Poc`] (same rule as [`Self::starts_a_new_wave_h264`]).
    pub fn note_h265(
        &mut self,
        pic_order_cnt: i32,
        is_irap: bool,
        sei: Option<RecoveryPointHevc>,
    ) -> RecoveryMark {
        if is_irap {
            self.target = None;
        }
        let mut mark = RecoveryMark::NONE;
        // Target as this picture arrived; captured before the charge below can clear it.
        let outstanding = match self.target {
            Some(Target::Poc(target)) => Some(target),
            _ => None,
        };
        if let Some(target) = outstanding {
            if pic_order_cnt >= target {
                mark.is_recovery_point = true;
                self.target = None;
            }
        }
        if let Some(rp) = sei {
            // D.3.8: target = this POC + signed delta. Zero/negative is already reached.
            let target = pic_order_cnt.saturating_add(rp.recovery_poc_cnt);
            if outstanding.is_none_or(|t| target > t) {
                mark.sei_here = true;
                if pic_order_cnt >= target {
                    mark.is_recovery_point = true;
                    self.target = None;
                } else {
                    self.target = Some(Target::Poc(target));
                }
            }
            // Same or earlier target: keep the further outstanding one.
        }
        mark
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h264_sei(recovery_frame_cnt: u32) -> Option<RecoveryPoint> {
        Some(RecoveryPoint {
            recovery_frame_cnt,
            // Rolling-wave default; the watch ignores this — see
            // `an_approximate_recovery_point_still_marks`.
            exact_match: false,
            broken_link: false,
        })
    }

    fn h265_sei(recovery_poc_cnt: i32) -> Option<RecoveryPointHevc> {
        Some(RecoveryPointHevc {
            recovery_poc_cnt,
            exact_match: false,
            broken_link: false,
        })
    }

    #[test]
    fn an_h264_wave_marks_the_picture_the_sei_counted_to() {
        let mut w = RecoveryWatch::new();
        let start = w.note_h264(10, false, h264_sei(3));
        assert!(start.sei_here, "the wave start is reported");
        assert!(
            !start.is_recovery_point,
            "a count of 3 is not reached on the picture that announced it"
        );
        assert!(w.is_watching());
        assert_eq!(w.note_h264(11, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h264(12, false, None), RecoveryMark::NONE);
        let healed = w.note_h264(13, false, None);
        assert!(healed.is_recovery_point, "the third increment is the heal");
        assert!(!healed.sei_here);
        assert!(!w.is_watching(), "and the watch is spent");
        assert_eq!(w.note_h264(14, false, None), RecoveryMark::NONE);
    }

    /// D.2.8 counts `frame_num` increments, not pictures. A repeat must not charge.
    #[test]
    fn a_repeated_frame_num_spends_no_increment() {
        let mut w = RecoveryWatch::new();
        w.note_h264(4, false, h264_sei(2));
        assert_eq!(w.note_h264(5, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h264(5, false, None), RecoveryMark::NONE);
        assert!(
            w.note_h264(6, false, None).is_recovery_point,
            "the second true increment completes the count"
        );
    }

    /// A `frame_num` gap is charged as one increment, so the mark can only land late.
    #[test]
    fn a_frame_num_gap_delays_the_mark_it_never_advances_it() {
        let mut w = RecoveryWatch::new();
        w.note_h264(100, false, h264_sei(2));
        // 101 → 104 is three real increments, charged as one.
        assert_eq!(w.note_h264(104, false, None), RecoveryMark::NONE);
        assert!(
            w.note_h264(105, false, None).is_recovery_point,
            "the count still has to be walked off — never short-circuited"
        );
    }

    #[test]
    fn a_zero_count_marks_the_pictures_that_carries_the_sei() {
        let mut w = RecoveryWatch::new();
        let m = w.note_h264(7, false, h264_sei(0));
        assert!(m.sei_here && m.is_recovery_point);
        assert!(!w.is_watching());
    }

    /// Count is measured from the new SEI's own picture, not the prior wave's.
    #[test]
    fn a_new_sei_that_reaches_further_replaces_an_outstanding_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h264(1, false, h264_sei(2));
        // 4 increments from here vs the 1 the first wave still owed.
        let restart = w.note_h264(3, false, h264_sei(4));
        assert!(restart.sei_here, "it reaches further — a fresh wave starts");
        assert!(!restart.is_recovery_point);
        for fnum in 4..7 {
            assert_eq!(w.note_h264(fnum, false, None), RecoveryMark::NONE);
        }
        assert!(
            w.note_h264(7, false, None).is_recovery_point,
            "the SECOND wave's count is what completes, not the first's"
        );
    }

    /// x264 `--intra-refresh` re-emits decreasing `recovery_frame_cnt` (legal D.2.8).
    /// Those are the same wave; `sei_here` would arm a freeze on a pre-loss wave.
    #[test]
    fn a_decreasing_re_announcement_is_the_same_wave_not_a_new_one() {
        let mut w = RecoveryWatch::new();
        let start = w.note_h264(10, false, h264_sei(4));
        assert!(start.sei_here, "the FIRST announcement is a wave start");
        for (fnum, cnt) in [(11, 3), (12, 2), (13, 1)] {
            let m = w.note_h264(fnum, false, h264_sei(cnt));
            assert!(
                !m.sei_here,
                "frame_num {fnum} re-announces the same wave — not a fresh start"
            );
            assert!(!m.is_recovery_point, "and the wave is not there yet");
        }
        // Trailing `recovery_frame_cnt == 0` is the same wave reaching zero, not a new start.
        let healed = w.note_h264(14, false, h264_sei(0));
        assert!(healed.is_recovery_point, "the wave really did complete");
        assert!(
            !healed.sei_here,
            "…but nothing NEW started here — the count only walked to zero"
        );
        assert!(!w.is_watching());
    }

    #[test]
    fn an_h265_re_announcement_of_the_same_target_is_not_a_new_wave() {
        let mut w = RecoveryWatch::new();
        assert!(w.note_h265(10, false, h265_sei(10)).sei_here);
        for poc in 11..20 {
            let m = w.note_h265(poc, false, h265_sei(20 - poc));
            assert!(!m.sei_here, "poc {poc} re-announces target 20");
            assert!(!m.is_recovery_point);
        }
        let healed = w.note_h265(20, false, h265_sei(0));
        assert!(healed.is_recovery_point);
        assert!(!healed.sei_here, "the target did not advance past 20");
        assert!(w.note_h265(21, false, h265_sei(8)).sei_here);
    }

    /// A shorter SEI is ignored: keep the further target (late, never early).
    /// A real new-but-shorter wave is therefore unreported.
    #[test]
    fn an_sei_that_reaches_short_of_the_outstanding_target_never_shortens_the_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h265(0, false, h265_sei(20)); // target 20
        let short = w.note_h265(1, false, h265_sei(2)); // would target 3
        assert!(!short.sei_here);
        assert_eq!(w.note_h265(3, false, None), RecoveryMark::NONE, "not at 20");
        assert!(w.note_h265(20, false, None).is_recovery_point);

        let mut w = RecoveryWatch::new();
        w.note_h264(0, false, h264_sei(6));
        assert!(!w.note_h264(1, false, h264_sei(2)).sei_here);
        for fnum in 2..6 {
            assert_eq!(w.note_h264(fnum, false, None), RecoveryMark::NONE);
        }
        assert!(w.note_h264(6, false, None).is_recovery_point);
    }

    /// After IDR/IRAP the next SEI is a wave start even if its count is small.
    #[test]
    fn a_keyframe_makes_the_next_sei_a_wave_start_again() {
        let mut w = RecoveryWatch::new();
        w.note_h264(10, false, h264_sei(9));
        w.note_h264(0, true, None);
        assert!(
            w.note_h264(1, false, h264_sei(1)).sei_here,
            "a one-increment wave after an IDR is still a fresh wave"
        );

        let mut w = RecoveryWatch::new();
        w.note_h265(100, false, h265_sei(50));
        w.note_h265(0, true, None);
        assert!(w.note_h265(1, false, h265_sei(2)).sei_here);
    }

    /// IDR resets `frame_num`; a target counted on the old numbering must not survive.
    #[test]
    fn an_idr_drops_a_pending_h264_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h264(200, false, h264_sei(2));
        assert!(w.is_watching());
        let idr = w.note_h264(0, true, None);
        assert_eq!(
            idr,
            RecoveryMark::NONE,
            "the IDR is not a recovery-point mark"
        );
        assert!(!w.is_watching());
        assert_eq!(w.note_h264(1, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h264(2, false, None), RecoveryMark::NONE);
    }

    #[test]
    fn an_h265_wave_marks_the_first_picture_at_or_past_the_target_poc() {
        let mut w = RecoveryWatch::new();
        let start = w.note_h265(10, false, h265_sei(4));
        assert!(start.sei_here && !start.is_recovery_point);
        assert_eq!(w.note_h265(11, false, None), RecoveryMark::NONE);
        assert_eq!(w.note_h265(13, false, None), RecoveryMark::NONE);
        assert!(w.note_h265(14, false, None).is_recovery_point);
        assert!(!w.is_watching());

        // Nothing guarantees the exact target POC is coded.
        let mut w = RecoveryWatch::new();
        w.note_h265(0, false, h265_sei(3));
        assert!(w.note_h265(8, false, None).is_recovery_point);
    }

    /// `recovery_poc_cnt` is se(v) and may be negative; that must not become an infinite watch.
    #[test]
    fn a_negative_or_zero_poc_count_marks_immediately() {
        for count in [0, -1, -7] {
            let mut w = RecoveryWatch::new();
            let m = w.note_h265(30, false, h265_sei(count));
            assert!(m.sei_here && m.is_recovery_point, "count {count}");
            assert!(!w.is_watching(), "count {count}");
        }
    }

    /// Any IRAP re-bases POC; a prior target cannot be compared past it.
    #[test]
    fn an_irap_drops_a_pending_h265_watch() {
        let mut w = RecoveryWatch::new();
        w.note_h265(500, false, h265_sei(3));
        assert!(w.is_watching());
        assert_eq!(w.note_h265(0, true, None), RecoveryMark::NONE);
        assert!(!w.is_watching());
        assert_eq!(w.note_h265(1, false, None), RecoveryMark::NONE);
    }

    /// `exact_match_flag == 0` is the rolling-wave default. The watch must not require it.
    #[test]
    fn an_approximate_recovery_point_still_marks() {
        let mut w = RecoveryWatch::new();
        assert!(
            w.note_h264(
                1,
                false,
                Some(RecoveryPoint {
                    recovery_frame_cnt: 0,
                    exact_match: false,
                    broken_link: true,
                })
            )
            .is_recovery_point,
            "neither exact_match nor broken_link may veto the mark"
        );
        let mut w = RecoveryWatch::new();
        assert!(
            w.note_h265(
                1,
                false,
                Some(RecoveryPointHevc {
                    recovery_poc_cnt: 0,
                    exact_match: false,
                    broken_link: true,
                })
            )
            .is_recovery_point
        );
    }

    #[test]
    fn a_stream_without_recovery_point_seis_never_marks() {
        let mut w = RecoveryWatch::new();
        for n in 0..64u16 {
            assert_eq!(w.note_h264(n, n == 0, None), RecoveryMark::NONE);
        }
        let mut w = RecoveryWatch::new();
        for n in 0..64i32 {
            assert_eq!(w.note_h265(n, n == 0, None), RecoveryMark::NONE);
        }
    }
}
