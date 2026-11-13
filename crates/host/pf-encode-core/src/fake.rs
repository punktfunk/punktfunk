//! A GPU-less [`Encoder`] for loop tests. It encodes nothing: an access unit is its wire index
//! as four bytes, and its flags come from [`rfi::plan_frame`] under the [`RecoveryPolicy`] it
//! was built with, so a loop sees the recovery order a real backend follows.
//!
//! Anchors come from the last [`SLOTS`] clean wire indexes, plus the two [`rfi::Kept`] holds
//! past them, through [`rfi::plan_slot_recovery`]. A loss resolves at the report, and one that
//! lands while a wave runs always spoils it. `fail_submits` scripts submit failures, terminal ones with
//! `fail_terminal`. Tests and `test-support` only.

use std::collections::VecDeque;

use anyhow::{anyhow, Result};
use pf_frame::CapturedFrame;

use crate::rfi::{self, FrameRef, Pending, Recovery, RecoveryPolicy};
use crate::{Acked, EncodedFrame, Encoder, EncoderCaps, TerminalEncoderError};

/// Clean pictures an anchor may reach back over.
const SLOTS: usize = 8;

pub struct FakeEncoder {
    policy: RecoveryPolicy,
    state: Recovery,
    force_kf: bool,
    anchor: bool,
    floor: Option<Acked>,
    confirmed: Option<Acked>,
    kept: rfi::Kept,
    /// Wire index of each trusted picture, oldest first.
    trusted: VecDeque<i64>,
    out: VecDeque<EncodedFrame>,
    next_wire: i64,
    /// Submits that fail before one succeeds.
    pub fail_submits: u32,
    /// Those failures carry [`TerminalEncoderError`].
    pub fail_terminal: bool,
}

impl FakeEncoder {
    /// A fresh session: the first frame is an IDR.
    pub fn new(policy: RecoveryPolicy) -> Self {
        Self {
            policy,
            state: Recovery::default(),
            force_kf: true,
            anchor: false,
            floor: None,
            confirmed: None,
            kept: rfi::Kept::default(),
            trusted: VecDeque::new(),
            out: VecDeque::new(),
            next_wire: 0,
            fail_submits: 0,
            fail_terminal: false,
        }
    }

    fn refs(&self) -> Vec<(usize, i64)> {
        self.trusted.iter().copied().enumerate().collect()
    }
}

impl Encoder for FakeEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        if self.fail_submits > 0 {
            self.fail_submits -= 1;
            return Err(if self.fail_terminal {
                anyhow::Error::new(TerminalEncoderError).context("scripted submit failure")
            } else {
                anyhow!("scripted submit failure")
            });
        }
        let pending = Pending {
            idr: self.force_kf,
            anchor: self.anchor,
            acked: self
                .floor
                .is_some_and(|a| rfi::pick_acked(&self.refs(), &a).is_some()),
            loss: false,
        };
        let plan = rfi::plan_frame(self.state, pending, &self.policy);
        (self.state, self.force_kf, self.anchor) = (plan.next, false, false);
        if plan.refs == FrameRef::Idr {
            self.trusted.clear();
        }
        let trusted = &self.trusted;
        self.kept
            .step(self.next_wire, !plan.dirty, self.confirmed.as_ref(), |w| {
                trusted.contains(&w)
            });
        if !plan.dirty {
            self.trusted.push_back(self.next_wire);
            if self.trusted.len() > SLOTS {
                let kept = self.kept;
                let oldest = self.trusted.iter().position(|&w| !kept.holds(w));
                self.trusted
                    .remove(oldest.expect("a ring larger than the two holds"));
            }
        }
        self.out.push_back(EncodedFrame {
            data: (self.next_wire as u32).to_le_bytes().to_vec(),
            pts_ns: frame.pts_ns,
            keyframe: plan.refs == FrameRef::Idr,
            recovery_anchor: matches!(plan.refs, FrameRef::Anchor | FrameRef::Acked),
            recovery_point: plan.mark.point(),
            recovery_close: plan.mark.close(),
            chunk_aligned: false,
        });
        self.next_wire += 1;
        Ok(())
    }

    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        self.next_wire = i64::from(wire_index);
        self.submit(frame)
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: true,
            ..Default::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if first < 0 || first > last {
            return false;
        }
        let plan = rfi::plan_slot_recovery(&self.refs(), first, self.floor.as_ref());
        self.trusted.retain(|&w| w < first);
        self.anchor = plan.anchor.is_some();
        if self.anchor {
            return true;
        }
        match self.state.lose(&self.policy, true) {
            Some(waving) => {
                self.state = waving;
                true
            }
            None => false,
        }
    }

    fn distrust_references(&mut self) {
        self.trusted.clear();
        self.anchor = false;
    }

    fn set_reference_floor(&mut self, acked: Option<Acked>) {
        self.floor = acked;
    }

    fn note_confirmed(&mut self, acked: Option<Acked>) {
        self.confirmed = acked;
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.out.pop_front())
    }

    /// Forfeits what is in flight; the next submit is an IDR. A scripted failure survives.
    fn reset(&mut self) -> bool {
        *self = Self {
            fail_submits: self.fail_submits,
            fail_terminal: self.fail_terminal,
            ..Self::new(self.policy)
        };
        true
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use pf_frame::{FramePayload, PixelFormat};

    use super::*;
    use crate::rfi::MidWave;

    /// An IDR first, an anchor for a loss the ring covers, a marked wave for one it does not,
    /// a terminal failure when scripted, and an IDR again after a reset.
    #[test]
    fn the_fake_follows_the_recovery_order() {
        let frame = CapturedFrame {
            width: 16,
            height: 16,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Pixelless,
            cursor: None,
            provenance: Default::default(),
        };
        let mut enc = FakeEncoder::new(RecoveryPolicy {
            cycle: Some(3),
            mid_wave: MidWave::Queue,
            abandons: true,
        });
        // (keyframe, recovery_anchor, recovery_point, recovery_close)
        let au = |enc: &mut FakeEncoder, wire: u32| {
            enc.submit_indexed(&frame, wire).unwrap();
            let au = enc.poll().unwrap().unwrap();
            assert_eq!(au.data, wire.to_le_bytes());
            (
                au.keyframe,
                au.recovery_anchor,
                au.recovery_point,
                au.recovery_close,
            )
        };
        assert_eq!(au(&mut enc, 10), (true, false, false, false));
        assert_eq!(au(&mut enc, 11), (false, false, false, false));
        assert!(enc.invalidate_ref_frames(11, 11), "10 anchors");
        assert_eq!(au(&mut enc, 12), (false, true, false, false));
        assert!(enc.invalidate_ref_frames(10, 12), "nothing older: a wave");
        assert_eq!(au(&mut enc, 13), (false, false, true, false));
        assert_eq!(au(&mut enc, 14), (false, false, false, false));
        assert_eq!(au(&mut enc, 15), (false, false, true, true));

        (enc.fail_submits, enc.fail_terminal) = (1, true);
        let refused = enc.submit_indexed(&frame, 16).unwrap_err();
        assert!(refused.downcast_ref::<TerminalEncoderError>().is_some());
        assert!(enc.reset());
        assert_eq!(au(&mut enc, 16), (true, false, false, false));
    }
}
