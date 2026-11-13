//! The AV1 receiver rule on the desktop decoders, as Android and Apple run it: after a loss no
//! access unit reaches the decoder until an IDR, the host's anchor, or an error-resilient wave
//! start. Every frame between loads its entropy state from the lost picture and parses as
//! garbage; AMD's VCN hangs on such a stream. The concealer points a wave start that names the
//! lost picture at a held one, so the wave still heals.
//!
//! Strict: only the Vulkan host waves on AV1, and it codes each wave start error-resilient.
//! A wave start that is not asks for a keyframe.

use pf_bitstream::av1::conceal::{Av1Concealer, Concealment};
use punktfunk_core::packet::USER_FLAG_RECOVERY_POINT;
use punktfunk_core::reanchor::{self, AuAdmission, DecoderClass, Resume};

/// What the pump does with one AV1 access unit.
#[derive(Debug, PartialEq, Eq)]
pub enum Intake {
    Feed,
    /// Feed these bytes instead: the AU named a lost picture and now names a held one.
    Rewritten(Vec<u8>),
    /// Keep it off the decoder. `true`: ask the host for a keyframe.
    Withhold(bool),
}

/// One AV1 stream's rule and concealer. A new decoder format starts a new one.
#[derive(Default)]
pub struct Av1Intake {
    rule: AuAdmission,
    conceal: Av1Concealer,
    /// The concealer refused an AU the rule admitted: the next one follows a gap.
    refused: bool,
}

impl Av1Intake {
    /// Fold one AU in receive order: its wire `index`, the frames lost ahead of it, its
    /// wire `flags`.
    pub fn note(&mut self, index: u32, gap: u32, flags: u32, au: &[u8]) -> Intake {
        let wave_start = flags & USER_FLAG_RECOVERY_POINT != 0;
        let verdict = (wave_start && self.conceal.error_resilient(au))
            .then_some(reanchor::Concealment::Decodable);
        let gap = if std::mem::take(&mut self.refused) {
            gap.max(1)
        } else {
            gap
        };
        let step = self
            .rule
            .note(index, gap, flags, DecoderClass::Strict, verdict);
        if let Some(e) = step.ended {
            let by = match e.by {
                Resume::Idr => "an IDR",
                Resume::Anchor => "the anchor",
                Resume::WaveStart => "a wave start",
                Resume::Concealed => "a concealed wave start",
            };
            tracing::info!(
                first = e.first,
                withheld = e.withheld,
                index,
                "AV1 frames withheld after a loss, until {by}"
            );
        }
        if step.withhold {
            return Intake::Withhold(step.ask_keyframe);
        }
        match self.conceal.conceal(au) {
            Concealment::Intact => Intake::Feed,
            Concealment::Rewritten(out) => Intake::Rewritten(out),
            Concealment::Unrecoverable => {
                self.refused = true;
                Intake::Withhold(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Av1Intake, Intake};
    use pf_bitstream::testing::{split_ivf, AV1_ONE_SLOT_ER14};
    use punktfunk_core::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_POINT};

    const SOF: u32 = FLAG_SOF as u32;

    /// Frames 0 to 3 fed, 4 lost. Returns the intake and the vector's AUs.
    fn after_a_loss() -> (Av1Intake, Vec<&'static [u8]>) {
        let aus = split_ivf(AV1_ONE_SLOT_ER14);
        let mut intake = Av1Intake::default();
        assert_eq!(intake.note(0, 0, SOF, aus[0]), Intake::Feed);
        for i in 1..4 {
            assert_eq!(intake.note(i, 0, 0, aus[i as usize]), Intake::Feed);
        }
        (intake, aus)
    }

    #[test]
    fn a_loss_withholds_until_the_wave_start_names_a_held_picture() {
        let (mut intake, aus) = after_a_loss();
        assert_eq!(intake.note(5, 1, 0, aus[5]), Intake::Withhold(false));
        for i in 6..14 {
            assert_eq!(
                intake.note(i, 0, 0, aus[i as usize]),
                Intake::Withhold(false)
            );
        }
        let Intake::Rewritten(out) = intake.note(14, 0, USER_FLAG_RECOVERY_POINT, aus[14]) else {
            panic!("the stale wave start is fed rewritten");
        };
        assert_eq!(out.len(), aus[14].len());
        for i in 15..aus.len() as u32 {
            assert_eq!(intake.note(i, 0, 0, aus[i as usize]), Intake::Feed);
        }
    }

    #[test]
    fn the_hosts_anchor_ends_the_stretch() {
        let (mut intake, aus) = after_a_loss();
        assert_eq!(intake.note(5, 1, 0, aus[5]), Intake::Withhold(false));
        assert_eq!(
            intake.note(6, 0, USER_FLAG_RECOVERY_ANCHOR, aus[6]),
            Intake::Feed
        );
        assert_eq!(intake.note(7, 0, 0, aus[7]), Intake::Feed);
    }

    #[test]
    fn a_wave_start_the_concealer_cannot_move_waits_for_a_keyframe() {
        let (mut intake, aus) = after_a_loss();
        assert_eq!(
            intake.note(5, 1, USER_FLAG_RECOVERY_POINT, aus[5]),
            Intake::Withhold(true)
        );
        assert_eq!(intake.note(6, 0, 0, aus[6]), Intake::Withhold(true));
        assert_eq!(intake.note(7, 0, SOF, aus[0]), Intake::Feed);
    }
}
