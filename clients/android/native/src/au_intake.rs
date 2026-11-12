//! Which access units the MediaCodec loop feeds: the feeder's drop of what core refuses
//! ([`hand_on`]), the AV1 receiver rule ([`AuAdmission`]) and the wait for a keyframe on a
//! rebuilt decoder. No codec and no clock: the loop reports each delivery and its own drops,
//! and feeds what comes back. The freeze and every ask are core's.

use punktfunk_core::packet::FLAG_SOF;
use punktfunk_core::reanchor::{AuAdmission, Concealment, DecoderClass, Resume};

/// One delivery from the feeder: a whole AU, or one part of a sliced one.
#[derive(Clone, Copy)]
pub(crate) struct Delivery {
    pub(crate) index: u32,
    /// Not a later part of a sliced AU. The rule decides here and the later parts follow.
    pub(crate) first: bool,
    pub(crate) flags: u32,
}

/// What the loop does with one [`Delivery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) feed: bool,
    /// Ask for a keyframe. Core throttles.
    pub(crate) ask_keyframe: bool,
}

/// Whether the feeder hands one delivery to the loop. `admit` is core's verdict on a whole AU
/// or a first part, `None` on a later part: parts repeat their AU's index and follow the first
/// part. `refused` holds the last refused index.
pub(crate) fn hand_on(refused: &mut Option<u32>, index: u32, admit: Option<bool>) -> bool {
    match admit {
        Some(true) => true,
        Some(false) => {
            *refused = Some(index);
            false
        }
        None => *refused != Some(index),
    }
}

/// [`AuAdmission`] and what the loop keeps beside it.
struct Admit {
    rule: AuAdmission,
    class: DecoderClass,
    /// The AU whose parts are withheld.
    parts_of: Option<u32>,
    /// The loop dropped an admitted AU; the next first part is noted with a gap.
    local_gap: bool,
}

pub(crate) struct Intake {
    /// A rebuilt run drops every AU before the first keyframe.
    await_keyframe: bool,
    /// The receiver rule on AV1 sessions; `None` feeds every AU.
    admit: Option<Admit>,
    withheld: u64,
}

impl Intake {
    /// `class`: the receiver rule's decoder class, `None` for no rule.
    pub(crate) fn new(class: Option<DecoderClass>, await_keyframe: bool) -> Intake {
        Intake {
            await_keyframe,
            admit: class.map(|class| Admit {
                rule: AuAdmission::default(),
                class,
                parts_of: None,
                local_gap: false,
            }),
            withheld: 0,
        }
    }

    /// AUs the rule kept off the codec. They owe output like fed ones.
    pub(crate) fn withheld(&self) -> u64 {
        self.withheld
    }

    /// One delivery with the index gap ahead of it and the concealer's `verdict` on it. The
    /// rule sees every AU, the ones the keyframe wait then drops too, and only a keyframe's
    /// head ends that wait: its later parts carry the flag too, and without the head they
    /// decode nothing.
    pub(crate) fn note(&mut self, d: Delivery, gap: u32, verdict: Option<Concealment>) -> Verdict {
        let mut v = self.judge(d, gap, verdict);
        if v.feed && self.await_keyframe {
            if d.first && d.flags & u32::from(FLAG_SOF) != 0 {
                self.await_keyframe = false;
            } else {
                v.feed = false;
            }
        }
        v
    }

    /// The loop dropped an AU the rule had admitted, and the AUs behind it may name it: the
    /// next first part is noted with a gap, as for a network loss. `true` with a rule on;
    /// the parked AUs then go through [`Self::rejudge`].
    pub(crate) fn local_loss(&mut self) -> bool {
        match self.admit.as_mut() {
            Some(a) => {
                a.local_gap = true;
                true
            }
            None => false,
        }
    }

    /// A parked AU behind a local loss: the rule alone, it passed the keyframe wait before.
    pub(crate) fn rejudge(&mut self, d: Delivery, verdict: Option<Concealment>) -> Verdict {
        self.judge(d, 0, verdict)
    }

    fn judge(&mut self, d: Delivery, gap: u32, verdict: Option<Concealment>) -> Verdict {
        let Some(a) = self.admit.as_mut() else {
            return Verdict {
                feed: true,
                ask_keyframe: false,
            };
        };
        if !d.first {
            return Verdict {
                feed: a.parts_of != Some(d.index),
                ask_keyframe: false,
            };
        }
        let gap = if std::mem::take(&mut a.local_gap) {
            gap.max(1)
        } else {
            gap
        };
        let step = a.rule.note(d.index, gap, d.flags, a.class, verdict);
        a.parts_of = step.withhold.then_some(d.index);
        if let Some(e) = step.ended {
            log::info!(
                "decode: withheld {} AU(s) from frame {} after a loss, until {} at frame {}",
                e.withheld,
                e.first,
                match e.by {
                    Resume::Idr => "an IDR",
                    Resume::Anchor => "the anchor",
                    Resume::WaveStart => "a wave start",
                    Resume::Concealed => "the concealer",
                },
                d.index
            );
        }
        self.withheld += u64::from(step.withhold);
        Verdict {
            feed: !step.withhold,
            ask_keyframe: step.ask_keyframe,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{hand_on, Delivery, Intake, Verdict};
    use punktfunk_core::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_POINT};
    use punktfunk_core::reanchor::Concealment;
    use punktfunk_core::reanchor::DecoderClass::{self, Lenient, Strict};

    const SOF: u32 = FLAG_SOF as u32;
    const ANCHOR: u32 = USER_FLAG_RECOVERY_ANCHOR;
    const POINT: u32 = USER_FLAG_RECOVERY_POINT;

    #[derive(Clone, Copy)]
    enum Op {
        /// A whole AU or a first part: index, flags, gap ahead.
        Au(u32, u32, u32),
        /// A whole AU the concealer finds decodable: index, flags.
        Concealed(u32, u32),
        /// A later part of a sliced AU: index, flags.
        Part(u32, u32),
        /// The loop dropped an admitted AU; the bool is whether a rule is on.
        LocalLoss(bool),
        /// A parked AU judged again behind that drop.
        Parked(u32, u32),
    }
    use Op::{Au, Concealed, LocalLoss, Parked, Part};

    const FEED: Option<(bool, bool)> = Some((true, false));
    const DROP: Option<(bool, bool)> = Some((false, false));
    const DROP_ASK: Option<(bool, bool)> = Some((false, true));

    /// One script: the rule's class, whether the run waits for a keyframe, each step with
    /// `(feed, ask_keyframe)`, and the withheld count at the end.
    type Case = (
        &'static str,
        Option<DecoderClass>,
        bool,
        &'static [(Op, Option<(bool, bool)>)],
        u64,
    );

    const CASES: &[Case] = &[
        (
            "no rule feeds every AU, the one after a gap too",
            None,
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (Au(3, 0, 1), FEED),
                (LocalLoss(false), None),
                (Au(4, 0, 0), FEED),
            ],
            0,
        ),
        (
            "a rebuilt decoder is fed nothing before the keyframe",
            None,
            true,
            &[
                (Au(7, 0, 0), DROP),
                (Au(8, ANCHOR, 0), DROP),
                (Au(9, SOF, 0), FEED),
                (Part(9, SOF), FEED),
                (Au(10, 0, 0), FEED),
            ],
            0,
        ),
        (
            "a keyframe whose head was lost does not end the wait",
            None,
            true,
            &[
                (Part(9, SOF), DROP),
                (Au(10, 0, 0), DROP),
                (Au(11, SOF, 0), FEED),
                (Part(11, SOF), FEED),
            ],
            0,
        ),
        (
            "a loss withholds until the anchor, each part with its AU",
            Some(Lenient),
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (Au(3, 0, 1), DROP),
                (Part(3, 0), DROP),
                (Au(4, 0, 0), DROP),
                (Au(5, ANCHOR, 0), FEED),
                (Part(5, ANCHOR), FEED),
                (Au(6, 0, 0), FEED),
            ],
            2,
        ),
        (
            "a wave start resumes a lenient decoder",
            Some(Lenient),
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (Au(3, 0, 1), DROP),
                (Au(4, POINT, 0), FEED),
            ],
            1,
        ),
        (
            "a wave start the concealer can rewrite resumes a strict decoder",
            Some(Strict),
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (Au(3, 0, 1), DROP),
                (Concealed(4, POINT), FEED),
                (Au(5, 0, 0), FEED),
            ],
            1,
        ),
        (
            "a wave start asks for a keyframe on a strict decoder",
            Some(Strict),
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (Au(3, 0, 1), DROP),
                (Au(4, POINT, 0), DROP_ASK),
                (Au(5, 0, 0), DROP_ASK),
                (Au(6, SOF, 0), FEED),
            ],
            3,
        ),
        (
            "an anchor ends the rule's stretch, not the keyframe wait",
            Some(Lenient),
            true,
            &[
                (Au(1, 0, 1), DROP),
                (Au(2, ANCHOR, 0), DROP),
                (Au(3, 0, 0), DROP),
                (Au(4, SOF, 0), FEED),
            ],
            1,
        ),
        (
            "a local drop withholds the parked AUs behind it",
            Some(Lenient),
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (Au(2, 0, 0), FEED),
                (Au(3, 0, 0), FEED),
                (LocalLoss(true), None),
                (Parked(2, 0), DROP),
                (Parked(3, 0), DROP),
                (Au(4, 0, 0), DROP),
                (Au(5, SOF, 0), FEED),
            ],
            3,
        ),
        (
            "a local drop with nothing parked puts the gap on the next arrival",
            Some(Lenient),
            false,
            &[
                (Au(1, SOF, 0), FEED),
                (LocalLoss(true), None),
                (Au(2, 0, 0), DROP),
                (Au(3, ANCHOR, 0), FEED),
            ],
            1,
        ),
    ];

    #[test]
    fn the_intake_table() {
        for &(name, class, await_keyframe, steps, withheld) in CASES {
            let mut intake = Intake::new(class, await_keyframe);
            for (i, &(op, want)) in steps.iter().enumerate() {
                let got = match op {
                    Au(index, flags, gap) => Some(intake.note(
                        Delivery {
                            index,
                            first: true,
                            flags,
                        },
                        gap,
                        None,
                    )),
                    Concealed(index, flags) => Some(intake.note(
                        Delivery {
                            index,
                            first: true,
                            flags,
                        },
                        0,
                        Some(Concealment::Decodable),
                    )),
                    Part(index, flags) => Some(intake.note(
                        Delivery {
                            index,
                            first: false,
                            flags,
                        },
                        0,
                        None,
                    )),
                    LocalLoss(rule) => {
                        assert_eq!(intake.local_loss(), rule, "{name}: step {i}");
                        None
                    }
                    Parked(index, flags) => Some(intake.rejudge(
                        Delivery {
                            index,
                            first: true,
                            flags,
                        },
                        None,
                    )),
                };
                let want = want.map(|(feed, ask_keyframe)| Verdict { feed, ask_keyframe });
                assert_eq!(got, want, "{name}: step {i}");
            }
            assert_eq!(intake.withheld(), withheld, "{name}: withheld");
        }
    }

    #[test]
    fn the_feeder_drops_what_core_refuses() {
        // (index, core's verdict or `None` for a later part, handed on)
        const STEPS: &[(u32, Option<bool>, bool)] = &[
            (5, Some(true), true),
            (5, None, true),
            (3, Some(false), false),
            (3, None, false),
            (3, None, false),
            (6, Some(true), true),
            (3, None, false),
            (6, None, true),
            (7, None, true),
        ];
        let mut refused = None;
        for (i, &(index, admit, want)) in STEPS.iter().enumerate() {
            assert_eq!(hand_on(&mut refused, index, admit), want, "step {i}");
        }
    }
}
