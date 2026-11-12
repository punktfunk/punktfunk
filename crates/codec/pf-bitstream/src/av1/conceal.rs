//! Conceal a lost AV1 reference for a decoder that refuses a stale slot. An error-resilient
//! frame codes the order hint it expects in every slot (5.9.2). After a loss its reference slot
//! holds an older picture, `RefValid` drops to 0, and the frame breaks conformance: a decoder
//! that does not conceal errors or hangs on it. Such a frame inherits no CDFs, so the rewrite
//! codes the order hints the decoder holds and the frame predicts from the older picture in its
//! slot. Same bit lengths; the wave that starts on the frame sweeps that base clean.

use super::{AuPlan, Av1Planner, PlanError, PlanWarning, RefPic, RefState, NUM_REF_SLOTS};

pub use crate::h265::conceal::Concealment;

/// One elementary stream's concealer. Feed exactly what the decoder receives, in decode
/// order: the planner inside mirrors the decoder's slots.
#[derive(Default)]
pub struct Av1Concealer {
    planner: Av1Planner,
    /// The decoder lost an AU this concealer folded: nothing is fed until a key frame.
    awaiting_key: bool,
}

impl Av1Concealer {
    pub fn new() -> Self {
        Self::default()
    }

    /// The decoder lost an AU this concealer folded: every AU is
    /// [`Concealment::Unrecoverable`] until a key frame.
    pub fn flush(&mut self) {
        self.awaiting_key = true;
    }

    /// Whether `au` is an error-resilient inter frame, the one shape [`Self::conceal`] can
    /// rewrite. Plans on a copy; this concealer is left as it was.
    pub fn error_resilient(&self, au: &[u8]) -> bool {
        self.planner.clone().plan_au(au).is_ok_and(|plans| {
            plans
                .iter()
                .any(|p| p.header.error_resilient_mode && !p.header.frame_is_intra)
        })
    }

    /// Fold one access unit; see [`Concealment`].
    pub fn conceal(&mut self, au: &[u8]) -> Concealment {
        let before = self.planner.clone();
        let plans = match self.planner.plan_au(au) {
            Ok(plans) => plans,
            // Waiting for a key frame, or no sequence header yet: nothing is fed.
            Err(err) if self.awaiting_key || matches!(err, PlanError::NoSequenceHeader) => {
                self.planner = before;
                return Concealment::Unrecoverable;
            }
            // Outside what the planner follows: the decoder gets the AU untouched, as before.
            Err(_) => return Concealment::Intact,
        };
        if self.awaiting_key {
            let key = plans
                .iter()
                .any(|p| p.header.frame_type == super::FrameType::KeyFrame && p.header.show_frame);
            if !key {
                self.planner = before;
                return Concealment::Unrecoverable;
            }
            self.awaiting_key = false;
        }
        if !plans.iter().any(lost_reference) {
            return Concealment::Intact;
        }
        let mut out = au.to_vec();
        if !plans
            .iter()
            .filter(|p| lost_reference(p))
            .all(|p| rewrite_frame(&mut out, p))
        {
            self.planner = before;
            return Concealment::Unrecoverable;
        }
        // The decoder receives `out`: mirror exactly that, and it must plan clean.
        let mut mirror = before.clone();
        match mirror.plan_au(&out) {
            Ok(plans) if !plans.iter().any(lost_reference) => {
                self.planner = mirror;
                Concealment::Rewritten(out)
            }
            _ => {
                self.planner = before;
                Concealment::Unrecoverable
            }
        }
    }
}

fn lost_reference(plan: &AuPlan) -> bool {
    plan.warnings.iter().any(|w| {
        matches!(
            w,
            PlanWarning::MissingReference { .. } | PlanWarning::StaleReference { .. }
        )
    })
}

/// Code, in `plan`'s frame, the order hint the decoder holds in every slot, so no reference
/// reads as stale. `false` when the frame is not error-resilient, predicts from two references
/// at once (`skipModeAllowed` reads the hints, and with it the header after them), or names a
/// slot that holds nothing, a later picture, or one of another size.
fn rewrite_frame(out: &mut [u8], plan: &AuPlan) -> bool {
    let h = &plan.header;
    let seq = &plan.sequence;
    if !h.error_resilient_mode || h.frame_is_intra || !seq.enable_order_hint || h.reference_select {
        return false;
    }
    let Some(hint_bit) = h.ref_order_hint_bit else {
        return false;
    };
    let bits = seq.order_hint_bits as u32;
    let mut held: [Option<RefPic>; NUM_REF_SLOTS] = [None; NUM_REF_SLOTS];
    for r in &plan.dpb_refs {
        held[usize::from(r.slot)] = Some(*r);
    }
    let cur = RefState::of(h);
    let usable = |slot: u8| {
        held[usize::from(slot)].is_some_and(|r| {
            r.state.upscaled_width == cur.upscaled_width
                && r.state.frame_height == cur.frame_height
                && relative_dist(r.state.order_hint, h.order_hint, bits) < 0
        })
    };
    if !h.ref_frame_idx.iter().all(|&s| usable(s)) {
        return false;
    }
    let at = plan.header_at * 8 + hint_bit;
    for (i, r) in held.iter().enumerate() {
        if let Some(r) = r {
            write_bits(out, at + i * bits as usize, bits, r.state.order_hint);
        }
    }
    true
}

/// `get_relative_dist` (7.12.3): how far order hint `a` lies after `b`, wrapped to `bits`.
fn relative_dist(a: u32, b: u32, bits: u32) -> i64 {
    if bits == 0 {
        return 0;
    }
    let m = 1i64 << (bits - 1);
    let diff = i64::from(a) - i64::from(b);
    (diff & (m - 1)) - (diff & m)
}

/// Write `value` MSB first as `bits` bits starting at bit `at` of `out`.
fn write_bits(out: &mut [u8], at: usize, bits: u32, value: u32) {
    for k in 0..bits as usize {
        let bit = (value >> (bits as usize - 1 - k)) & 1;
        let pos = at + k;
        let mask = 0x80u8 >> (pos % 8);
        if bit == 1 {
            out[pos / 8] |= mask;
        } else {
            out[pos / 8] &= !mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::split_ivf;

    /// libaom shaped like the Vulkan host: a key frame, then every frame predicts from slot 0
    /// and refreshes it, no reference-frame motion vectors; frame 14 alone is error-resilient,
    /// as a wave start is. The unused names point at key-frame slots. 64x64, 24 frames.
    fn chain() -> Vec<&'static [u8]> {
        split_ivf(include_bytes!("../../tests/vectors/aom-one-slot-er14.ivf"))
    }

    fn lost_reference_in(planner: &mut Av1Planner, au: &[u8]) -> bool {
        planner
            .plan_au(au)
            .expect("plans")
            .iter()
            .any(lost_reference)
    }

    #[test]
    fn the_whole_stream_stays_intact() {
        let mut c = Av1Concealer::new();
        for au in chain() {
            assert_eq!(c.conceal(au), Concealment::Intact);
        }
    }

    /// The Formuler session: frames 0 to 3 decoded, 4 lost, 5 to 13 withheld, 14 the wave
    /// start. 14 codes order hint 13 for slot 0, which holds frame 3.
    #[test]
    fn a_stale_wave_start_codes_the_order_hint_the_decoder_holds() {
        let aus = chain();
        let mut c = Av1Concealer::new();
        for au in &aus[..4] {
            assert_eq!(c.conceal(au), Concealment::Intact);
        }
        assert!(c.error_resilient(aus[14]));
        assert!(!c.error_resilient(aus[5]));
        let Concealment::Rewritten(out) = c.conceal(aus[14]) else {
            panic!("the stale wave start is rewritten");
        };
        assert_eq!(out.len(), aus[14].len());
        let moved = (0..out.len() * 8)
            .filter(|&b| (out[b / 8] ^ aus[14][b / 8]) & (0x80 >> (b % 8)) != 0)
            .count();
        assert_eq!(
            moved,
            (13u32 ^ 3).count_ones() as usize,
            "only hint 13 → 3 moves"
        );
        let mut concealed: Vec<u8> = aus[..4].concat();
        concealed.extend_from_slice(&out);
        for au in &aus[15..] {
            assert_eq!(c.conceal(au), Concealment::Intact);
            concealed.extend_from_slice(au);
        }

        // A decoder that saw what the client fed finds every reference valid.
        let mut fresh = Av1Planner::new();
        for au in &aus[..4] {
            assert!(!lost_reference_in(&mut fresh, au));
        }
        assert!(!lost_reference_in(&mut fresh, &out));
        // As the host sent it, the wave start names a stale slot.
        let mut raw = Av1Planner::new();
        for au in &aus[..4] {
            raw.plan_au(au).expect("plans");
        }
        assert!(lost_reference_in(&mut raw, aus[14]));

        // For the dav1d replay: `dav1d --demuxer section5 -i <file>`.
        if let Ok(dir) = std::env::var("PF_CONCEAL_OUT") {
            let mut dropped: Vec<u8> = aus[..4].concat();
            for au in &aus[14..] {
                dropped.extend_from_slice(au);
            }
            std::fs::write(format!("{dir}/dropped.obu"), dropped).expect("write");
            std::fs::write(format!("{dir}/concealed.obu"), concealed).expect("write");
        }
    }

    /// Without coded order hints a lost inter frame is invisible: the receiver rule, not the
    /// concealer, keeps such a frame off the decoder.
    #[test]
    fn a_frame_that_codes_no_order_hints_is_left_to_the_receiver_rule() {
        let aus = chain();
        let mut c = Av1Concealer::new();
        for au in &aus[..4] {
            c.conceal(au);
        }
        assert_eq!(c.conceal(aus[5]), Concealment::Intact);
    }

    #[test]
    fn a_flush_holds_every_au_until_the_next_key_frame() {
        let aus = chain();
        let mut c = Av1Concealer::new();
        for au in &aus[..4] {
            c.conceal(au);
        }
        c.flush();
        assert_eq!(c.conceal(aus[4]), Concealment::Unrecoverable);
        assert_eq!(c.conceal(aus[14]), Concealment::Unrecoverable);
        assert_eq!(c.conceal(aus[0]), Concealment::Intact);
        assert_eq!(c.conceal(aus[1]), Concealment::Intact);
    }

    #[test]
    fn relative_dist_wraps_at_the_hint_width() {
        assert_eq!(relative_dist(3, 14, 7), -11);
        assert_eq!(relative_dist(14, 3, 7), 11);
        assert_eq!(relative_dist(1, 127, 7), 2);
        assert_eq!(relative_dist(127, 1, 7), -2);
    }
}
