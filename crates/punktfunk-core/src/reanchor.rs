//! Post-loss recovery: [`ReanchorGate`] holds concealed output until a proven re-anchor, and
//! [`AuAdmission`] keeps AUs that name a lost picture off the decoder.
//!
//! Hardware decoders return Ok on a missing reference and conceal; presenting that is the
//! gray-plate artifact. Every client holds the last good picture instead, and lifts only on
//! a real IDR, an honoured [`USER_FLAG_RECOVERY_ANCHOR`], an intra-refresh wave's close
//! after a start seen since the loss ([`USER_FLAG_RECOVERY_CLOSE`]; the second
//! [`USER_FLAG_RECOVERY_POINT`] from a host without the close bit), or a local
//! recovery-point SEI ([`ReanchorGate::on_local_recovery`]).
//!
//! One shared state machine so embedders do not re-derive it. Time-driven but takes `now`
//! so tests need no clock; C ABI wrappers pass `Instant::now()`. Lanes without a bitstream
//! parser never call the local or corroborated paths; the wire contract is unchanged.
//!
//! [`USER_FLAG_RECOVERY_ANCHOR`] is a host claim about the client decoder. A parser that can
//! prove the named references were concealed passes [`AnchorEvidence::ReferencesDamaged`]
//! through [`ReanchorGate::on_decoded_corroborated`]: the freeze stays up, the backstop
//! fires on its original deadline, and the client escalates to an IDR. Silence is
//! [`AnchorEvidence::Unavailable`].
//!
//! [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
//! [`USER_FLAG_RECOVERY_CLOSE`]: crate::packet::USER_FLAG_RECOVERY_CLOSE
//! [`USER_FLAG_RECOVERY_ANCHOR`]: crate::packet::USER_FLAG_RECOVERY_ANCHOR

use crate::packet::{
    FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_CLOSE, USER_FLAG_RECOVERY_POINT,
};
use std::time::{Duration, Instant};

/// Consecutive no-output AUs that force a keyframe request. 3 ≈ 50 ms at 60 Hz: skip a one-frame
/// decoder hiccup, still recover a lost initial IDR before the picture stays dark.
pub const NO_OUTPUT_KEYFRAME_STREAK: u32 = 3;

/// Longest the freeze waits for a re-anchor before [`ReanchorGate::poll`] re-asks. The deadline
/// never presents the concealed picture: it re-asks and keeps holding. 500 ms is well above a
/// recovery-IDR round-trip on a live link and short enough that a stalled host still recovers.
pub const REANCHOR_FREEZE_MAX: Duration = Duration::from_millis(500);

/// Intra-refresh [`USER_FLAG_RECOVERY_POINT`]s since the latest loss before the freeze lifts
/// without an IDR, on a host that marks both ends of a wave alike. Two, not one: the first
/// boundary after a loss may be the close of a wave that began before it. A host that sets
/// [`USER_FLAG_RECOVERY_CLOSE`] on the close needs no count: the first close after a start
/// seen since the arm lifts. Every arm resets the count.
///
/// [`USER_FLAG_RECOVERY_POINT`]: crate::packet::USER_FLAG_RECOVERY_POINT
/// [`USER_FLAG_RECOVERY_CLOSE`]: crate::packet::USER_FLAG_RECOVERY_CLOSE
pub const REANCHOR_MARKS_TO_LIFT: u32 = 2;

/// Extra freeze time each live recovery mark buys. Must exceed one intra-refresh wave
/// (~0.5 s) with margin so a healing stream is not pre-empted by the IDR floor. When marks
/// stop, the deadline lapses and the recovery-IDR floor still fires.
pub const RECOVERY_MARK_PATIENCE: Duration = Duration::from_millis(1500);

/// How long a gap-arm's expected `frames_dropped` climb stays pre-credited in
/// [`ReanchorGate::poll`]. The reassembler books the same loss ~120 ms later; without the
/// credit a fast LTR-RFI lift between the two signals re-freezes a healed stream. 1 s covers
/// the 120 ms loss window plus jitter, and expires so leftover credit cannot mask a later climb.
pub const DROP_CREDIT_WINDOW: Duration = Duration::from_millis(1000);

/// Frames skipped when `got` is ahead of `expected`, else `None`. Indices wrap: wrapping
/// subtraction split at the half-space — small positive is a forward gap, top half is a
/// straggler already passed.
pub fn index_gap(expected: u32, got: u32) -> Option<u32> {
    let ahead = got.wrapping_sub(expected);
    (ahead != 0 && ahead < u32::MAX / 2).then_some(ahead)
}

/// Fold one decoded frame: IDR or honoured LTR-RFI anchor lifts immediately. With a host
/// that marks the close (`close_aware`), `marks` counts wave starts since the arm and the
/// first close after one lifts; otherwise [`REANCHOR_MARKS_TO_LIFT`] marks must accumulate.
/// Returns `(lift, new_marks)` with the count reset to 0 on a lift. The caller applies
/// [`AnchorEvidence`] before `has_anchor` so this stays a pure statement of the wire rules.
fn reanchor_after_frame(
    is_keyframe: bool,
    has_anchor: bool,
    has_mark: bool,
    has_close: bool,
    close_aware: bool,
    marks: u32,
) -> (bool, u32) {
    let (marks, swept) = if close_aware {
        if has_close {
            (marks, marks >= 1)
        } else if has_mark {
            (marks.saturating_add(1), false)
        } else {
            (marks, false)
        }
    } else {
        let marks = if has_mark {
            marks.saturating_add(1)
        } else {
            marks
        };
        (marks, marks >= REANCHOR_MARKS_TO_LIFT)
    };
    if is_keyframe || has_anchor || swept {
        (true, 0)
    } else {
        (false, marks)
    }
}

/// Local bitstream-parser view of intra-refresh recovery on one frame — the in-band counterpart
/// of [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT). Two facts, not one
/// verdict: a recovery-point SEI promises a correct picture N frames later for a decoder that
/// lost references *before* the SEI, and nothing for a loss *after* it. Only
/// [`ReanchorGate::on_local_recovery`] pairs the SEI against the arm, because only the gate
/// knows when the loss was. Lanes without a parser leave this [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LocalRecovery {
    /// This AU carried a recovery-point SEI: a heal *starts* here.
    pub sei_here: bool,
    /// This frame is the picture a previously-seen SEI named: the heal *completed*.
    pub is_recovery_point: bool,
}

impl LocalRecovery {
    /// Both flags false. What a client without a local parser passes on every frame.
    pub const NONE: LocalRecovery = LocalRecovery {
        sei_here: false,
        is_recovery_point: false,
    };
}

/// What a local parser can say about a [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR)
/// on this frame. The host tracks whether the client *received* a reference, not whether it
/// *decoded* it from a complete chain — concealment is the gap. Three states so a lane that
/// cannot answer is not folded into "nothing wrong". Only [`Self::ReferencesDamaged`] changes
/// behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnchorEvidence {
    /// No local parser: the host's claim stands. Default for MediaCodec, VideoToolbox, C ABI.
    #[default]
    Unavailable,
    /// Predicts only from pictures that decoded from a complete reference chain.
    ReferencesClean,
    /// This AU predicts from a concealed picture, so the anchor must not lift.
    ReferencesDamaged,
}

impl AnchorEvidence {
    /// Honour the wire anchor unless refuted. Silence is not refutation — a lane that cannot
    /// corroborate must not become stricter than it was.
    fn honours_anchor(self) -> bool {
        !matches!(self, AnchorEvidence::ReferencesDamaged)
    }
}

/// Whether this decoded frame may reach the presenter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Show it: not frozen, or this frame is the re-anchor that lifts.
    Present,
    /// Withhold: post-loss concealment; the presenter keeps the last good picture.
    Hold,
}

/// Shared post-loss freeze. A client feeds *arm* (loss), each decoded frame
/// ([`on_decoded`](Self::on_decoded)), each no-output AU ([`on_no_output`](Self::on_no_output)),
/// and a periodic [`poll`](Self::poll). The gate emits *intents* only: `true` means the client
/// should ask for a keyframe through its own ~100 ms throttle. The gate never touches the wire.
#[derive(Debug, Clone)]
pub struct ReanchorGate {
    /// Freeze is up: withhold concealed output until a lift. Armed by any loss; cleared only
    /// by a lift in [`on_decoded`](Self::on_decoded) / [`on_local_recovery`](Self::on_local_recovery).
    awaiting: bool,
    /// Recovery marks since the latest arm: wave starts once the host marks closes, else
    /// every mark ([`REANCHOR_MARKS_TO_LIFT`]). Zeroed on every arm.
    marks: u32,
    /// The host sets [`USER_FLAG_RECOVERY_CLOSE`] on a wave's close. Latched by the first
    /// one seen; a stream never loses the bit, so it is never cleared.
    close_marks: bool,
    /// When [`poll`](Self::poll) re-asks while still holding. Never presents concealment. `None`
    /// when not frozen.
    deadline: Option<Instant>,
    /// Consecutive AUs with no decoded frame — a wedged decoder with no reassembler drop.
    /// [`NO_OUTPUT_KEYFRAME_STREAK`] trips a fresh IDR.
    no_output_streak: u32,
    /// Last `frames_dropped` [`poll`](Self::poll) saw. A climb is an unrecoverable AU.
    last_dropped: u64,
    /// A recovery-point SEI observed *since* the latest arm. Zeroed on arm so a wave that
    /// started before the loss cannot lift the freeze that loss raised.
    local_sei_since_arm: bool,
    /// Times the freeze has armed — monotonic, never reset. A local parser stamps its
    /// decode-order watermark on each move: a DPB flush can return pictures decoded *before*
    /// the loss, and wall-clock pairing cannot tell. Other clients ignore it.
    arms: u64,
    /// The latest lift came from an intra-refresh heal (wire marks or a local recovery
    /// point). The wave heals the lifting picture by overwrite, which a reference-chain
    /// ledger cannot see, so damaged evidence is not held against that frame or the one
    /// after it; the client forgives the swept picture in between, and from then on a
    /// damaged verdict is real again.
    mark_lift: bool,
    /// Frames since the mark lift that still ride its suspension.
    mark_lift_grace: u8,
    /// `frames_dropped` climb still expected from gap-armed losses. [`poll`](Self::poll)
    /// consumes this before treating a climb as a new loss ([`DROP_CREDIT_WINDOW`]).
    drop_credit: u64,
    /// [`DROP_CREDIT_WINDOW`] after the latest credited arm; `None` when credit is 0.
    drop_credit_expiry: Option<Instant>,
}

impl ReanchorGate {
    /// Seed `frames_dropped` so the first [`poll`](Self::poll) does not treat the baseline as a loss.
    pub fn new(frames_dropped: u64) -> Self {
        ReanchorGate {
            awaiting: false,
            marks: 0,
            close_marks: false,
            deadline: None,
            no_output_streak: 0,
            last_dropped: frames_dropped,
            local_sei_since_arm: false,
            arms: 0,
            mark_lift: false,
            mark_lift_grace: 0,
            drop_credit: 0,
            drop_credit_expiry: None,
        }
    }

    /// Monotonic arm count. Every arm site moves it, including [`Self::on_no_output`] and
    /// [`Self::poll`]; the overdue backstop re-asks without re-arming and must not move it
    /// (re-stamping would discard a heal already in flight).
    pub fn arms(&self) -> u64 {
        self.arms
    }

    /// Arm the freeze. Zeroes the mark count and (re-)sets the backstop. Idempotent while
    /// frozen: a second loss mid-freeze re-zeroes marks and pushes the deadline.
    pub fn arm(&mut self, now: Instant) {
        self.awaiting = true;
        self.marks = 0;
        self.arms = self.arms.saturating_add(1);
        // A wave already in flight still references the picture just lost. Only an SEI seen
        // from here on may be trusted.
        self.local_sei_since_arm = false;
        self.mark_lift = false;
        self.mark_lift_grace = 0;
        self.deadline = Some(now + REANCHOR_FREEZE_MAX);
    }

    /// [`arm`](Self::arm) for a frame-index gap of known width. Pre-credits that many
    /// `frames_dropped` so the reassembler's ~120 ms later climb is not a second loss
    /// ([`DROP_CREDIT_WINDOW`]). Use plain [`arm`](Self::arm) for wedge/demotion: no climb.
    pub fn arm_expecting_drops(&mut self, now: Instant, expected_drops: u64) {
        self.arm(now);
        self.drop_credit = self.drop_credit.saturating_add(expected_drops);
        self.drop_credit_expiry = Some(now + DROP_CREDIT_WINDOW);
    }

    /// Fold a local recovery-point observation *before* [`on_decoded`](Self::on_decoded).
    /// Returns `true` when it lifted. Trustworthy only when the SEI arrived at or after the
    /// arm ([`LocalRecovery`]); a pre-arm SEI is ignored and the backstop still covers it.
    /// Lifts on the first trusted recovery point: the SEI names a wave that started after the
    /// loss, so that picture is fully swept — the same guarantee as an honoured
    /// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR), derived locally.
    /// On an unfrozen gate it only records the SEI.
    pub fn on_local_recovery(&mut self, obs: LocalRecovery) -> bool {
        if obs.sei_here {
            self.local_sei_since_arm = true;
        }
        if !(obs.is_recovery_point && self.local_sei_since_arm && self.awaiting) {
            return false;
        }
        self.awaiting = false;
        self.deadline = None;
        self.marks = 0;
        // A wave heal: the reference chain stays marked damaged, so suspend that rule.
        self.mark_lift = true;
        self.mark_lift_grace = 1;
        // Spent: the next heal needs its own SEI, or one wave would lift a later loss.
        self.local_sei_since_arm = false;
        true
    }

    /// Fold one decoded frame. [`FLAG_SOF`](crate::packet::FLAG_SOF) is the host's codec-agnostic
    /// IDR; `decoder_keyframe` is libavcodec's extra IDR bit — pass `false` where the decoder
    /// does not flag keys. A live mark while frozen pushes the backstop
    /// ([`RECOVERY_MARK_PATIENCE`]). Believes an anchor on sight; parsers call
    /// [`on_decoded_corroborated`](Self::on_decoded_corroborated) instead.
    pub fn on_decoded(
        &mut self,
        wire_flags: u32,
        decoder_keyframe: bool,
        now: Instant,
    ) -> GateVerdict {
        self.on_decoded_corroborated(
            wire_flags,
            decoder_keyframe,
            AnchorEvidence::Unavailable,
            now,
        )
    }

    /// [`on_decoded`](Self::on_decoded) when the client's own parser knows whether this
    /// picture predicts from a complete reference chain.
    /// [`AnchorEvidence::ReferencesDamaged`] does two things. It withholds an
    /// [`USER_FLAG_RECOVERY_ANCHOR`](crate::packet::USER_FLAG_RECOVERY_ANCHOR) lift, and it
    /// holds the frame itself, arming the freeze if nothing else had: a picture predicted
    /// from a concealed one is the artifact whatever the wire says, and it reaches an
    /// unfrozen gate whenever a lift landed before the damaged chain drained (a reordered
    /// straggler decoded after its successors, an encoder still referencing the corrupt
    /// window after its recovery frame). Without the arm nothing would ever ask for the IDR.
    /// A real IDR predicts from nothing, so its evidence is never damaged and it still lifts.
    /// A lift by [`USER_FLAG_RECOVERY_POINT`](crate::packet::USER_FLAG_RECOVERY_POINT) marks
    /// suspends the rule for that frame and the next: the wave healed the swept picture,
    /// which the chain cannot show until the client forgives it.
    pub fn on_decoded_corroborated(
        &mut self,
        wire_flags: u32,
        decoder_keyframe: bool,
        evidence: AnchorEvidence,
        now: Instant,
    ) -> GateVerdict {
        self.no_output_streak = 0;
        let grace = std::mem::take(&mut self.mark_lift_grace) > 0;
        let is_keyframe = decoder_keyframe || (wire_flags & FLAG_SOF as u32 != 0);
        // Refuted anchors are stripped here so `reanchor_after_frame` stays the pure wire rules.
        let has_anchor = wire_flags & USER_FLAG_RECOVERY_ANCHOR != 0 && evidence.honours_anchor();
        let has_close = wire_flags & USER_FLAG_RECOVERY_CLOSE != 0;
        let has_mark = wire_flags & USER_FLAG_RECOVERY_POINT != 0 || has_close;
        self.close_marks |= has_close;
        if has_mark && self.awaiting {
            self.deadline = Some(now + RECOVERY_MARK_PATIENCE);
        }
        let (lift, marks) = reanchor_after_frame(
            is_keyframe,
            has_anchor,
            has_mark,
            has_close,
            self.close_marks,
            self.marks,
        );
        self.marks = marks;
        if lift {
            self.awaiting = false;
            self.deadline = None;
            self.mark_lift = !is_keyframe && !has_anchor;
            self.mark_lift_grace = u8::from(self.mark_lift);
        }
        let suspended = (lift && self.mark_lift) || grace;
        if evidence == AnchorEvidence::ReferencesDamaged && !suspended {
            if !self.awaiting {
                self.arm(now);
            }
            return GateVerdict::Hold;
        }
        if self.awaiting {
            GateVerdict::Hold
        } else {
            GateVerdict::Present
        }
    }

    /// One received AU produced no decoded frame. `true` when the streak trips: arm the freeze
    /// and ask for a keyframe, even if the client's throttle drops this iteration's request.
    pub fn on_no_output(&mut self, now: Instant) -> bool {
        self.no_output_streak += 1;
        if self.no_output_streak >= NO_OUTPUT_KEYFRAME_STREAK {
            self.arm(now);
            self.no_output_streak = 0;
            true
        } else {
            false
        }
    }

    /// Fold `frames_dropped` and the overdue backstop. `true` means ask for a keyframe: a climb
    /// beyond gap-arm credit is a fresh loss (arm), or the freeze has held [`REANCHOR_FREEZE_MAX`]
    /// with no re-anchor (re-ask and keep holding — never present concealment). A credited climb
    /// is delayed bookkeeping of a loss already armed; treating it as new is the double-arm race.
    /// The gap-arm's original deadline still re-asks if recovery never arrives.
    pub fn poll(&mut self, frames_dropped: u64, now: Instant) -> bool {
        let mut want_keyframe = false;
        if frames_dropped > self.last_dropped {
            let climb = frames_dropped - self.last_dropped;
            self.last_dropped = frames_dropped;
            if self.drop_credit_expiry.is_some_and(|e| now >= e) {
                self.drop_credit = 0;
                self.drop_credit_expiry = None;
            }
            let credited = climb.min(self.drop_credit);
            self.drop_credit -= credited;
            if self.drop_credit == 0 {
                self.drop_credit_expiry = None;
            }
            if climb > credited {
                self.arm(now);
                want_keyframe = true;
            }
        }
        if self.awaiting && self.deadline.is_some_and(|d| now >= d) {
            self.deadline = Some(now + REANCHOR_FREEZE_MAX);
            want_keyframe = true;
        }
        want_keyframe
    }

    pub fn is_holding(&self) -> bool {
        self.awaiting
    }

    /// The latest lift came from intra refresh marks, not an IDR or an anchor. A client with
    /// a bitstream planner forgives the swept picture on the frame this turns true: the wave
    /// healed it by overwrite, which the chain cannot show.
    pub fn lifted_by_marks(&self) -> bool {
        !self.awaiting && self.mark_lift
    }
}

/// How a decoder treats an AU that references a picture it does not hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoderClass {
    /// Refuses every later inter frame, a clean anchor included, until a keyframe.
    Strict,
    /// Conceals and carries on; the gate hides the result.
    Lenient,
}

/// A codec concealer's verdict on one AU, on a lane that has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concealment {
    /// Every reference names a picture the decoder holds, as received or rewritten.
    Decodable,
    /// Nothing can stand in: withhold it and ask for a keyframe.
    Unrecoverable,
}

/// What ended an [`AuAdmission`] episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    Idr,
    Anchor,
    /// An intra-refresh wave's start, on a lenient decoder.
    WaveStart,
    /// The concealer's `Decodable`.
    Concealed,
}

/// One finished stretch of withheld AUs, for the caller's log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Episode {
    /// Index of the first withheld AU.
    pub first: u32,
    pub withheld: u32,
    pub by: Resume,
}

/// [`AuAdmission::note`]'s verdict on one AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Admission {
    /// Keep this AU off the decoder, every part of it.
    pub withhold: bool,
    /// Ask for a keyframe. The caller throttles.
    pub ask_keyframe: bool,
    /// This AU ended a stretch that withheld at least one AU.
    pub ended: Option<Episode>,
}

/// The receiver half of reactive recovery: an AU reaches the decoder only when every picture
/// it references was decoded. After an index gap, each AU until the host's IDR or anchor names
/// the lost picture. A wave start also ends the stretch on a lenient decoder; a strict one keeps
/// withholding and asks for a keyframe. One per elementary stream, fed every AU in receive
/// order. A withheld AU is never fed and never folded into [`ReanchorGate`]; the gate's
/// [`REANCHOR_FREEZE_MAX`] re-ask ends a stretch whose anchor was lost.
#[derive(Debug, Clone, Default)]
pub struct AuAdmission {
    withholding: bool,
    /// A wave start arrived while a strict decoder was withheld from: the wave builds on a
    /// chain that decoder lacks, so only a keyframe ends this.
    mark_seen: bool,
    first: u32,
    withheld: u32,
}

impl AuAdmission {
    pub fn is_withholding(&self) -> bool {
        self.withholding
    }

    /// Fold one AU: its `index`, the index `gap` ahead of it (0 for none), its wire `flags`,
    /// the decoder's `class`, and the concealer's `verdict` on a lane that has one.
    pub fn note(
        &mut self,
        index: u32,
        gap: u32,
        flags: u32,
        class: DecoderClass,
        verdict: Option<Concealment>,
    ) -> Admission {
        match verdict {
            Some(Concealment::Decodable) => {
                return Admission {
                    ended: self.resume(Resume::Concealed),
                    ..Admission::default()
                }
            }
            Some(Concealment::Unrecoverable) => {
                return Admission {
                    withhold: true,
                    ask_keyframe: true,
                    ended: None,
                }
            }
            None => {}
        }
        if gap > 0 {
            if !self.withholding {
                self.first = index;
                self.withheld = 0;
            }
            self.withholding = true;
            self.mark_seen = false;
        }
        if !self.withholding {
            return Admission::default();
        }
        let wave_start = flags & USER_FLAG_RECOVERY_POINT != 0;
        let by = if flags & FLAG_SOF as u32 != 0 {
            Some(Resume::Idr)
        } else if flags & USER_FLAG_RECOVERY_ANCHOR != 0 {
            Some(Resume::Anchor)
        } else if wave_start && class == DecoderClass::Lenient {
            Some(Resume::WaveStart)
        } else {
            None
        };
        if let Some(by) = by {
            return Admission {
                ended: self.resume(by),
                ..Admission::default()
            };
        }
        self.mark_seen |= wave_start;
        self.withheld += 1;
        Admission {
            withhold: true,
            ask_keyframe: self.mark_seen,
            ended: None,
        }
    }

    fn resume(&mut self, by: Resume) -> Option<Episode> {
        let was = std::mem::take(&mut self.withholding);
        self.mark_seen = false;
        (was && self.withheld > 0).then_some(Episode {
            first: self.first,
            withheld: self.withheld,
            by,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fold `(is_keyframe, has_mark)` through `reanchor_after_frame`; return the 0-based index
    // of the first lift, or `None`. Resetting the running count to 0 models a fresh arm.
    fn lift_at(frames: &[(bool, bool)]) -> Option<usize> {
        let mut marks = 0u32;
        for (i, &(is_kf, has_mark)) in frames.iter().enumerate() {
            // Intra-refresh-mark model: no LTR-RFI path here (`an_rfi_anchor_lifts_immediately`).
            let (lift, m) = reanchor_after_frame(is_kf, false, has_mark, false, false, marks);
            marks = m;
            if lift {
                return Some(i);
            }
        }
        None
    }

    #[test]
    fn a_single_recovery_mark_does_not_lift() {
        assert_eq!(REANCHOR_MARKS_TO_LIFT, 2);
        assert_eq!(lift_at(&[(false, true)]), None);
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false)]),
            None
        );
    }

    #[test]
    fn the_second_recovery_mark_lifts() {
        assert_eq!(lift_at(&[(false, true), (false, true)]), Some(1));
        assert_eq!(
            lift_at(&[(false, false), (false, true), (false, false), (false, true)]),
            Some(3)
        );
    }

    #[test]
    fn a_real_keyframe_lifts_immediately() {
        assert_eq!(lift_at(&[(true, false)]), Some(0));
        assert_eq!(lift_at(&[(false, true), (true, false)]), Some(1));
    }

    /// Only a lift by wave marks reports as one; an IDR lift and the next arm clear it.
    #[test]
    fn lifted_by_marks_names_a_wave_lift_only() {
        let t = Instant::now();
        let mut g = ReanchorGate::new(0);
        g.arm(t);
        g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t);
        assert!(!g.lifted_by_marks(), "still holding after one mark");
        g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t);
        assert!(g.lifted_by_marks());
        g.arm(t);
        assert!(!g.lifted_by_marks(), "a new arm forgets the wave lift");
        g.on_decoded(FLAG_SOF as u32, true, t);
        assert!(!g.lifted_by_marks(), "an IDR lift is not a wave lift");
    }

    /// A host that marks the close apart: a close whose wave began before the loss does not
    /// count, the next start is not a second mark, and that wave's close lifts.
    #[test]
    fn a_close_mark_lifts_only_after_a_start_since_the_arm() {
        const CLOSE: u32 = USER_FLAG_RECOVERY_POINT | USER_FLAG_RECOVERY_CLOSE;
        let t = Instant::now();
        let mut g = ReanchorGate::new(0);
        g.arm(t);
        assert_eq!(g.on_decoded(CLOSE, false, t), GateVerdict::Hold);
        assert_eq!(
            g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t),
            GateVerdict::Hold,
            "the next start would have been the second mark"
        );
        assert_eq!(g.on_decoded(0, false, t), GateVerdict::Hold);
        assert_eq!(g.on_decoded(CLOSE, false, t), GateVerdict::Present);
        assert!(g.lifted_by_marks());
        // A restarted wave: two starts, then the close.
        g.arm(t);
        g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t);
        g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t);
        assert!(g.is_holding(), "two starts are not a sweep");
        assert_eq!(g.on_decoded(CLOSE, false, t), GateVerdict::Present);
    }

    /// The bit latches on first sight: the wave that introduces it lifts on its close, and
    /// from then on a lone close is no longer a countable mark.
    #[test]
    fn the_close_bit_latches_on_first_sight() {
        const CLOSE: u32 = USER_FLAG_RECOVERY_POINT | USER_FLAG_RECOVERY_CLOSE;
        let t = Instant::now();
        let mut g = ReanchorGate::new(0);
        g.arm(t);
        g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t);
        assert_eq!(g.on_decoded(CLOSE, false, t), GateVerdict::Present);
        g.arm(t);
        g.on_decoded(CLOSE, false, t);
        g.on_decoded(USER_FLAG_RECOVERY_POINT, false, t);
        assert!(
            g.is_holding(),
            "close then start: one wave early under the old rule"
        );
    }

    #[test]
    fn a_fresh_gap_resets_the_mark_count() {
        let mut marks = 0u32;
        let (_, m) = reanchor_after_frame(false, false, true, false, false, marks);
        marks = m;
        assert_eq!(marks, 1);
        marks = 0;
        let (lift, m) = reanchor_after_frame(false, false, true, false, false, marks);
        assert!(!lift, "a single post-gap mark must not lift");
        assert_eq!(m, 1);
    }

    #[test]
    fn an_rfi_anchor_lifts_immediately() {
        let (lift, marks) = reanchor_after_frame(false, true, false, false, false, 0);
        assert!(lift, "an RFI anchor must lift the freeze immediately");
        assert_eq!(marks, 0, "a lift resets the running mark count");
        let (lift, _) = reanchor_after_frame(false, true, true, false, false, 1);
        assert!(lift, "an anchor lifts regardless of the pending mark count");
    }

    #[test]
    fn contiguous_indices_are_not_a_gap() {
        assert_eq!(index_gap(5, 5), None);
        assert_eq!(index_gap(0, 0), None);
    }

    #[test]
    fn a_forward_jump_reports_the_skip_count() {
        assert_eq!(index_gap(5, 6), Some(1));
        assert_eq!(index_gap(5, 9), Some(4));
    }

    #[test]
    fn a_straggler_behind_us_is_not_a_gap() {
        // Reassembler can emit a newer frame first; the late one must not re-arm.
        assert_eq!(index_gap(9, 5), None);
        assert_eq!(index_gap(1, 0), None);
    }

    #[test]
    fn the_index_counter_wraps_cleanly() {
        assert_eq!(index_gap(0, 0), None);
        // wrapping_sub half-space: MAX → 0 is one skipped frame, not a straggler.
        assert_eq!(index_gap(u32::MAX, 0), Some(1));
        assert_eq!(index_gap(u32::MAX, 2), Some(3));
        assert_eq!(index_gap(0, u32::MAX), None);
    }

    const SOF: u32 = FLAG_SOF as u32;
    const ANCHOR: u32 = USER_FLAG_RECOVERY_ANCHOR;
    const POINT: u32 = USER_FLAG_RECOVERY_POINT;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_clean_link_never_holds() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
        assert_eq!(g.on_decoded(SOF, true, now), GateVerdict::Present);
        assert!(!g.is_holding());
        assert!(!g.poll(0, now));
    }

    #[test]
    fn a_gap_holds_until_the_wire_keyframe_lifts() {
        // Android/Apple: no decoder keyframe flag — lift is FLAG_SOF alone.
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present);
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_gap_lifts_on_the_first_rfi_anchor() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(ANCHOR, false, now), GateVerdict::Present);
        assert!(!g.is_holding());
    }

    #[test]
    fn a_gap_lifts_on_the_second_recovery_mark() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_second_gap_mid_freeze_resets_the_marks() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold);
        g.arm(now);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Hold);
        assert_eq!(g.on_decoded(POINT, false, now), GateVerdict::Present);
    }

    #[test]
    fn the_dropped_climb_arms_and_asks() {
        let mut g = ReanchorGate::new(5);
        let now = t0();
        assert!(!g.poll(5, now), "no climb → no ask");
        assert!(g.poll(6, now), "a climb asks for a keyframe");
        assert!(g.is_holding(), "and arms the freeze");
        assert!(
            !g.poll(6, now),
            "same value → no repeat ask from the drop path"
        );
    }

    #[test]
    fn an_rfi_anchor_is_not_refrozen_by_the_same_losss_drop_climb() {
        // Gap at T+10, LTR-RFI lift at T+60, reassembler climb ~T+130: credit must absorb
        // that climb or a healed stream re-freezes.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t + Duration::from_millis(10), 1);
        assert_eq!(
            g.on_decoded(ANCHOR, false, t + Duration::from_millis(60)),
            GateVerdict::Present,
            "the anchor lifts"
        );
        assert!(
            !g.poll(1, t + Duration::from_millis(130)),
            "the credited climb must not ask again"
        );
        assert!(!g.is_holding(), "and must not re-freeze the healed stream");
        assert_eq!(
            g.on_decoded(0, false, t + Duration::from_millis(141)),
            GateVerdict::Present,
            "healthy P-frames keep presenting"
        );
    }

    #[test]
    fn a_climb_beyond_the_credit_is_a_fresh_loss_and_arms() {
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 2);
        g.on_decoded(ANCHOR, false, t + Duration::from_millis(50));
        assert!(
            g.poll(3, t + Duration::from_millis(130)),
            "one uncredited drop → ask"
        );
        assert!(g.is_holding(), "and re-arm for the uncredited part");
    }

    #[test]
    fn the_drop_credit_expires_so_a_late_climb_still_arms() {
        // A straggler can fill the gap so no climb ever comes; leftover credit must expire.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 1);
        g.on_decoded(ANCHOR, false, t + Duration::from_millis(50));
        let late = t + DROP_CREDIT_WINDOW + Duration::from_millis(1);
        assert!(
            g.poll(1, late),
            "an expired credit no longer absorbs climbs"
        );
        assert!(g.is_holding());
    }

    #[test]
    fn a_credited_climb_keeps_the_unhealed_freezes_original_deadline() {
        // Unhealed: consuming the credited climb must not silence the backstop, and must
        // not push the deadline out to a later re-arm.
        let mut g = ReanchorGate::new(0);
        let t = t0();
        g.arm_expecting_drops(t, 1);
        assert!(
            !g.poll(1, t + Duration::from_millis(130)),
            "credited climb: no early re-ask"
        );
        assert!(g.is_holding(), "still frozen — nothing healed it");
        assert!(
            g.poll(1, t + REANCHOR_FREEZE_MAX + Duration::from_millis(1)),
            "the overdue backstop still re-asks on the gap-arm's own deadline"
        );
        assert!(g.is_holding(), "and keeps holding, never resuming to gray");
    }

    #[test]
    fn the_no_output_streak_trips_at_three() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now), "third no-output trips the streak");
        assert!(g.is_holding());
        g.on_decoded(SOF, false, now);
        assert!(!g.on_no_output(now));
        assert!(!g.on_no_output(now));
        assert!(g.on_no_output(now));
    }

    #[test]
    fn an_overdue_freeze_re_asks_but_keeps_holding() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        assert!(!g.poll(0, start));
        assert!(g.is_holding());
        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(0, later), "overdue freeze re-asks for a keyframe");
        assert!(g.is_holding(), "but never resumes to the concealed picture");
    }

    fn local(sei_here: bool, is_recovery_point: bool) -> LocalRecovery {
        LocalRecovery {
            sei_here,
            is_recovery_point,
        }
    }

    #[test]
    fn a_local_recovery_point_lifts_the_freeze_without_the_backstop() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);

        assert!(!g.on_local_recovery(local(true, false)));
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);
        assert!(!g.on_local_recovery(local(false, false)));
        assert_eq!(g.on_decoded(0, false, start), GateVerdict::Hold);

        // Heal at 120 ms, well inside REANCHOR_FREEZE_MAX — the SEI path must not wait it out.
        let mid = start + Duration::from_millis(120);
        assert!(g.on_local_recovery(local(false, true)), "the heal lifts it");
        assert!(!g.is_holding());
        assert_eq!(g.on_decoded(0, false, mid), GateVerdict::Present);
        assert!(
            !g.poll(0, mid),
            "and no keyframe is ever asked for — no IDR spike on a stream that healed itself"
        );
    }

    #[test]
    fn a_recovery_point_from_a_wave_that_predates_the_loss_is_ignored() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_local_recovery(local(true, false)));
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "a pre-loss wave's recovery point must not lift"
        );
        assert!(g.is_holding());
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        assert!(!g.on_local_recovery(local(true, false)));
        assert!(g.on_local_recovery(local(false, true)));
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_spent_recovery_point_cannot_lift_the_next_loss() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        g.on_local_recovery(local(true, false));
        assert!(g.on_local_recovery(local(false, true)));
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "the previous wave's credit is gone"
        );
        assert!(g.is_holding());
    }

    /// Both facts on one AU (SEI count 0). The pairing is order, not "needs two frames".
    #[test]
    fn an_sei_that_is_its_own_recovery_point_lifts_on_that_frame() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert!(g.on_local_recovery(local(true, true)));
        assert_eq!(g.on_decoded(0, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_client_without_a_local_parser_sees_no_behaviour_change() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        for _ in 0..8 {
            assert!(!g.on_local_recovery(LocalRecovery::NONE));
            assert_eq!(g.on_decoded(0, false, now), GateVerdict::Hold);
        }
        assert!(
            g.is_holding(),
            "still frozen — only the wire can lift this one"
        );
        assert_eq!(g.on_decoded(SOF, false, now), GateVerdict::Present);
    }

    #[test]
    fn a_recovery_point_on_an_unfrozen_gate_is_not_banked() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        assert!(!g.on_local_recovery(local(true, true)));
        assert!(!g.is_holding());
        g.arm(now);
        assert!(
            !g.on_local_recovery(local(false, true)),
            "the pre-arm SEI was cleared by the arm"
        );
        assert!(g.is_holding());
    }

    #[test]
    fn every_arm_site_moves_the_arm_counter_and_the_backstop_does_not() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        assert_eq!(g.arms(), 0, "a fresh gate has never armed");

        g.arm(start);
        assert_eq!(g.arms(), 1);
        g.arm(start);
        assert_eq!(g.arms(), 2);

        assert!(g.poll(1, start));
        assert_eq!(g.arms(), 3);

        let later = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(g.poll(1, later));
        assert_eq!(g.arms(), 3, "the backstop re-asks, it does not re-arm");

        let mut g = ReanchorGate::new(0);
        assert!(!g.on_no_output(start));
        assert!(!g.on_no_output(start));
        assert_eq!(g.arms(), 0, "the streak has not tripped yet");
        assert!(g.on_no_output(start));
        assert_eq!(g.arms(), 1);
    }

    #[test]
    fn a_live_mark_stream_pushes_the_deadline_out() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        // Mark after the original deadline: patience must suppress the overdue ask.
        let t = start + REANCHOR_FREEZE_MAX + Duration::from_millis(10);
        assert_eq!(g.on_decoded(POINT, false, t), GateVerdict::Hold);
        assert!(!g.poll(0, t + Duration::from_millis(1)));
        assert!(g.is_holding());
    }

    use AnchorEvidence::{ReferencesClean, ReferencesDamaged, Unavailable};

    #[test]
    fn an_anchor_whose_references_the_decoder_concealed_does_not_lift() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "a refuted anchor is not a re-anchor"
        );
        assert!(g.is_holding(), "and the freeze stays up");
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert!(g.is_holding());
    }

    #[test]
    fn a_real_idr_lifts_even_while_the_evidence_refutes_anchors() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        // An IDR predicts from nothing, so the parser reports it clean.
        assert_eq!(
            g.on_decoded_corroborated(0, true, ReferencesClean, now),
            GateVerdict::Present,
            "the IDR re-anchors after any number of refused anchors"
        );
        assert!(!g.is_holding());

        // FLAG_SOF: lanes whose decoder does not flag IDRs.
        let mut g = ReanchorGate::new(0);
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(SOF, false, ReferencesClean, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    #[test]
    fn a_corroborated_anchor_lifts_on_the_first_occurrence() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesClean, now),
            GateVerdict::Hold,
            "an ordinary frame is still withheld"
        );
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesClean, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    #[test]
    fn an_uncorroborated_lane_behaves_exactly_as_it_always_has() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, Unavailable, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, Unavailable, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());

        let mut wire = ReanchorGate::new(0);
        let mut corroborated = ReanchorGate::new(0);
        wire.arm(now);
        corroborated.arm(now);
        for flags in [0, POINT, 0, ANCHOR, SOF, 0] {
            assert_eq!(
                wire.on_decoded(flags, false, now),
                corroborated.on_decoded_corroborated(flags, false, Unavailable, now),
                "flags {flags:#x} diverged between the two entry points"
            );
            assert_eq!(wire.is_holding(), corroborated.is_holding());
        }
    }

    #[test]
    fn a_refused_anchor_leaves_the_backstop_on_its_original_deadline() {
        let mut g = ReanchorGate::new(0);
        let start = t0();
        g.arm(start);
        for ms in [10, 100, 300, 490] {
            assert_eq!(
                g.on_decoded_corroborated(
                    ANCHOR,
                    false,
                    ReferencesDamaged,
                    start + Duration::from_millis(ms)
                ),
                GateVerdict::Hold
            );
            assert!(!g.poll(0, start + Duration::from_millis(ms)), "not yet due");
        }
        let overdue = start + REANCHOR_FREEZE_MAX + Duration::from_millis(1);
        assert!(
            g.poll(0, overdue),
            "the backstop fires on the arm's own deadline — the refusals did not extend it"
        );
        assert!(
            g.is_holding(),
            "and it keeps holding, never resuming to gray"
        );
    }

    #[test]
    fn refuted_anchors_do_not_disturb_the_two_mark_rule() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(POINT, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "mark #1 is still only half a re-anchor"
        );
        // Refused anchor between marks must not consume or reset the count.
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert_eq!(
            g.on_decoded_corroborated(POINT, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "mark #2 lifts exactly as it does on the wire path"
        );
        assert!(!g.is_holding());
        // The chain stays marked damaged after a wave heal; the mark lift stands until a
        // fresh loss arms again.
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Present
        );
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
    }

    /// A reordered straggler: its successors decoded against a hole and sit in the DPB
    /// damaged, then a real IDR lifts the freeze before they drain. The next damaged frame
    /// must hold and re-arm, or the grey plate stays up until an unrelated loss.
    #[test]
    fn a_damaged_frame_on_an_unfrozen_gate_holds_and_arms() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        let arms = g.arms();
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert!(g.is_holding());
        assert_eq!(
            g.arms(),
            arms + 1,
            "a fresh arm, so the backstop asks for the IDR"
        );
        // Still damaged while frozen: hold, no second arm (marks and SEI credit survive).
        g.on_decoded_corroborated(0, false, ReferencesDamaged, now);
        assert_eq!(g.arms(), arms + 1);
        assert!(g.poll(0, now + REANCHOR_FREEZE_MAX), "overdue: re-ask");
        // The IDR heals the chain and lifts; a clean successor presents.
        assert_eq!(
            g.on_decoded_corroborated(SOF, true, ReferencesClean, now),
            GateVerdict::Present
        );
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesClean, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    /// A recovery-point SEI lift is a wave heal too: the chain stays marked, the picture
    /// is clean, so damaged evidence must not re-freeze it.
    #[test]
    fn a_local_recovery_lift_suspends_the_damaged_rule() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert!(g.on_local_recovery(local(true, true)));
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Present
        );
        assert!(!g.is_holding());
    }

    /// A clean anchor lifts, then the encoder references the corrupt window again. The
    /// wire says nothing; the parser does.
    /// After a mark lift the client forgives the swept picture, so a damaged verdict two
    /// frames on is a picture that leans on a half-swept one: hold it and arm.
    #[test]
    fn the_mark_lift_suspension_ends_after_one_frame() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        g.on_decoded_corroborated(USER_FLAG_RECOVERY_POINT, false, ReferencesDamaged, now);
        assert_eq!(
            g.on_decoded_corroborated(USER_FLAG_RECOVERY_POINT, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "the close lifts whatever its chain says"
        );
        assert!(g.lifted_by_marks());
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Present,
            "the frame after the close still rides the lift"
        );
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Hold,
            "two frames on, a damaged chain is real"
        );
        assert!(g.is_holding());
    }

    #[test]
    fn a_damaged_frame_after_an_honoured_anchor_refreezes() {
        let mut g = ReanchorGate::new(0);
        let now = t0();
        g.arm(now);
        assert_eq!(
            g.on_decoded_corroborated(ANCHOR, false, ReferencesClean, now),
            GateVerdict::Present
        );
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesDamaged, now),
            GateVerdict::Hold
        );
        assert!(g.is_holding());
        assert_eq!(
            g.on_decoded_corroborated(0, false, ReferencesClean, now),
            GateVerdict::Hold,
            "a clean frame is not a re-anchor; only IDR, anchor or marks lift"
        );
    }

    use DecoderClass::{Lenient, Strict};
    const WAVE: u32 = USER_FLAG_RECOVERY_POINT;

    /// `(index, gap, flags)` through one rule with no concealer; the `(withhold, ask)` pairs.
    fn admit(
        a: &mut AuAdmission,
        class: DecoderClass,
        aus: &[(u32, u32, u32)],
    ) -> Vec<(bool, bool)> {
        aus.iter()
            .map(|&(i, gap, flags)| {
                let s = a.note(i, gap, flags, class, None);
                (s.withhold, s.ask_keyframe)
            })
            .collect()
    }

    const FEED: (bool, bool) = (false, false);
    const HOLD: (bool, bool) = (true, false);
    const HOLD_ASK: (bool, bool) = (true, true);

    #[test]
    fn a_clean_stream_feeds_every_au_whatever_its_marks() {
        for class in [Strict, Lenient] {
            let mut a = AuAdmission::default();
            let aus = [
                (0, 0, SOF),
                (1, 0, 0),
                (2, 0, WAVE),
                (3, 0, ANCHOR),
                (4, 0, 0),
            ];
            assert_eq!(admit(&mut a, class, &aus), [FEED; 5]);
        }
    }

    #[test]
    fn a_loss_withholds_deltas_until_the_anchor() {
        let mut a = AuAdmission::default();
        assert_eq!(admit(&mut a, Strict, &[(1, 0, SOF), (2, 0, 0)]), [FEED; 2]);
        // Frame 3 was lost: 4 and 5 name it. The RFI is in flight, so no keyframe ask.
        assert_eq!(admit(&mut a, Strict, &[(4, 1, 0), (5, 0, 0)]), [HOLD; 2]);
        let anchor = a.note(6, 0, ANCHOR, Strict, None);
        assert!(!anchor.withhold, "the anchor names a pre-loss picture");
        assert_eq!(
            anchor.ended,
            Some(Episode {
                first: 4,
                withheld: 2,
                by: Resume::Anchor
            })
        );
        assert_eq!(admit(&mut a, Strict, &[(7, 0, 0)]), [FEED]);
    }

    #[test]
    fn an_idr_ends_withholding_too() {
        let mut a = AuAdmission::default();
        assert_eq!(
            admit(
                &mut a,
                Strict,
                &[(1, 0, SOF), (3, 1, 0), (4, 0, SOF), (5, 0, 0)]
            ),
            [FEED, HOLD, FEED, FEED]
        );
    }

    #[test]
    fn an_anchor_right_after_the_gap_is_no_episode() {
        let mut a = AuAdmission::default();
        let s = a.note(3, 1, ANCHOR, Strict, None);
        assert!(!s.withhold);
        assert_eq!(
            s.ended, None,
            "nothing was withheld, so there is nothing to log"
        );
    }

    #[test]
    fn a_concealed_stream_withholds_nothing_after_a_loss() {
        let mut a = AuAdmission::default();
        for (i, gap, flags) in [(3, 1, 0), (4, 0, WAVE)] {
            let s = a.note(i, gap, flags, Strict, Some(Concealment::Decodable));
            assert_eq!((s.withhold, s.ask_keyframe), FEED);
        }
        assert!(!a.is_withholding());
    }

    #[test]
    fn a_decodable_verdict_ends_a_stretch() {
        let mut a = AuAdmission::default();
        assert_eq!(admit(&mut a, Strict, &[(3, 1, 0)]), [HOLD]);
        let s = a.note(4, 0, 0, Strict, Some(Concealment::Decodable));
        assert!(!s.withhold);
        assert_eq!(s.ended.map(|e| e.by), Some(Resume::Concealed));
    }

    #[test]
    fn an_unrecoverable_concealment_withholds_and_asks() {
        let mut a = AuAdmission::default();
        let s = a.note(3, 1, 0, Strict, Some(Concealment::Unrecoverable));
        assert_eq!((s.withhold, s.ask_keyframe), HOLD_ASK);
        // The concealer resumes at the IDR and says so per AU; the rule follows it.
        let s = a.note(4, 0, SOF, Strict, Some(Concealment::Decodable));
        assert!(!s.withhold);
    }

    /// The host declined the RFI and started a wave. It builds on a chain a strict decoder no
    /// longer holds, so only an IDR ends this: ask on the mark and on every AU after it.
    #[test]
    fn a_wave_start_while_withholding_asks_on_a_strict_decoder() {
        let mut a = AuAdmission::default();
        assert_eq!(
            admit(
                &mut a,
                Strict,
                &[
                    (3, 1, 0),
                    (4, 0, WAVE),
                    (5, 0, 0),
                    (6, 0, WAVE | USER_FLAG_RECOVERY_CLOSE)
                ]
            ),
            [HOLD, HOLD_ASK, HOLD_ASK, HOLD_ASK]
        );
        let idr = a.note(7, 0, SOF, Strict, None);
        assert!(!idr.withhold);
        assert_eq!(
            idr.ended.map(|e| (e.withheld, e.by)),
            Some((4, Resume::Idr))
        );
    }

    /// A lenient decoder takes the wave from its start and heals exactly as without the rule.
    #[test]
    fn a_wave_start_resumes_a_lenient_decoder() {
        let mut a = AuAdmission::default();
        assert_eq!(admit(&mut a, Lenient, &[(3, 1, 0), (4, 0, 0)]), [HOLD; 2]);
        let start = a.note(5, 0, WAVE, Lenient, None);
        assert_eq!((start.withhold, start.ask_keyframe), FEED);
        assert_eq!(start.ended.map(|e| e.by), Some(Resume::WaveStart));
        assert_eq!(
            admit(
                &mut a,
                Lenient,
                &[(6, 0, 0), (7, 0, WAVE | USER_FLAG_RECOVERY_CLOSE)]
            ),
            [FEED; 2]
        );
    }

    /// A second loss mid-stretch is the same episode. It clears the mark: the host answers the
    /// new ask, which may be an anchor.
    #[test]
    fn a_second_gap_while_withholding_restarts_the_mark() {
        let mut a = AuAdmission::default();
        assert_eq!(
            admit(
                &mut a,
                Strict,
                &[(3, 1, 0), (4, 0, WAVE), (6, 1, 0), (7, 0, 0)]
            ),
            [HOLD, HOLD_ASK, HOLD, HOLD]
        );
        let anchor = a.note(8, 0, ANCHOR, Strict, None);
        assert!(!anchor.withhold);
        assert_eq!(
            anchor.ended,
            Some(Episode {
                first: 3,
                withheld: 4,
                by: Resume::Anchor
            })
        );
    }

    /// The anchor itself is lost. The rule keeps withholding; the gate's backstop (or the
    /// feeder's RFI on the new gap) re-asks, and the next anchor ends the stretch.
    #[test]
    fn a_lost_anchor_withholds_until_the_re_asked_anchor() {
        let mut a = AuAdmission::default();
        // Frame 3 lost, anchor 6 lost too.
        let aus = [
            (4, 1, 0),
            (5, 0, 0),
            (7, 1, 0),
            (8, 0, 0),
            (9, 0, 0),
            (10, 0, 0),
            (11, 0, 0),
        ];
        assert_eq!(admit(&mut a, Strict, &aus), [HOLD; 7]);
        let anchor = a.note(12, 0, ANCHOR, Strict, None);
        assert!(!anchor.withhold);
        assert_eq!(anchor.ended.map(|e| (e.first, e.withheld)), Some((4, 7)));
        assert_eq!(admit(&mut a, Strict, &[(13, 0, 0)]), [FEED]);
    }

    /// A re-ask after the anchor landed is `Covered` on the host: the next frame carries the
    /// anchor flag again and references the received anchor. Fed, no episode.
    #[test]
    fn a_covered_re_ask_anchor_feeds() {
        let mut a = AuAdmission::default();
        assert_eq!(
            admit(&mut a, Strict, &[(4, 1, 0), (5, 0, ANCHOR), (6, 0, 0)]),
            [HOLD, FEED, FEED]
        );
        let covered = a.note(7, 0, ANCHOR, Strict, None);
        assert_eq!((covered.withhold, covered.ask_keyframe), FEED);
        assert_eq!(covered.ended, None);
        assert_eq!(admit(&mut a, Strict, &[(8, 0, 0)]), [FEED]);
    }

    /// `PF_AV1_DUMP=<soak capture>` with its `.idx`, `PF_AV1_LOST=1,23,…` and `PF_AV1_LAG=<n>`:
    /// the client's view (lost AUs absent, the host's anchor `lag` frames after each loss)
    /// through a strict rule. It must withhold exactly `lost+1 .. anchor-1`. `PF_AV1_OUT=<path>`
    /// writes the fed AUs with an `.idx` for dav1d and aomdec.
    #[test]
    #[ignore = "replay: set PF_AV1_DUMP, PF_AV1_LOST and PF_AV1_LAG"]
    fn a_soak_capture_loses_exactly_the_pre_ask_frames() {
        let path = std::env::var("PF_AV1_DUMP").expect("PF_AV1_DUMP=<capture>");
        let list = |k: &str| -> Vec<u32> {
            let v = std::env::var(k).unwrap_or_else(|_| panic!("{k} unset"));
            v.split(',')
                .map(|s| s.trim().parse().expect("an index"))
                .collect()
        };
        let lost = list("PF_AV1_LOST");
        let lag = list("PF_AV1_LAG")[0];
        let bytes = std::fs::read(&path).expect("read the capture");
        let idx = std::fs::read_to_string(format!("{path}.idx")).expect("read the .idx");
        let units: Vec<(usize, usize)> = idx
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace().map(|s| s.parse().ok());
                Some((f.next()??, f.next()??))
            })
            .collect();

        let mut a = AuAdmission::default();
        let (mut withheld, mut episodes, mut gap) = (Vec::new(), 0, 0);
        let (mut out, mut out_idx) = (Vec::new(), String::new());
        for (i, &(off, len)) in units.iter().enumerate() {
            let i = i as u32;
            if lost.contains(&i) {
                gap += 1;
                continue;
            }
            let anchor = i.checked_sub(lag).is_some_and(|l| lost.contains(&l));
            let flags = if anchor { ANCHOR } else { 0 };
            let s = a.note(i, std::mem::take(&mut gap), flags, Strict, None);
            episodes += u32::from(s.ended.is_some());
            if s.withhold {
                withheld.push(i);
                continue;
            }
            out_idx += &format!("{} {len} 0x0 1\n", out.len());
            out.extend_from_slice(&bytes[off..off + len]);
        }
        let expected: Vec<u32> = lost.iter().flat_map(|&l| l + 1..l + lag).collect();
        println!(
            "withheld {withheld:?} in {episodes} episodes; fed {}",
            units.len() - lost.len() - withheld.len()
        );
        assert_eq!(withheld, expected);
        if let Ok(o) = std::env::var("PF_AV1_OUT") {
            std::fs::write(&o, &out).expect("write the view");
            std::fs::write(format!("{o}.idx"), out_idx).expect("write its .idx");
        }
    }
}
