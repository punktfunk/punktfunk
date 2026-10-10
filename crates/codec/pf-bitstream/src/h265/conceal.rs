//! Conceal a missing HEVC reference for a decoder that binds references from the bitstream
//! alone. A stateless backend takes the stand-in [`H265Planner`] chose; VideoToolbox reads
//! the RPS itself, and one current entry it does not hold puts its session into an error
//! state that refuses every later non-IDR picture, the recovery anchor included. So the
//! slice header is rewritten before submit: `used_by_curr_pic` cleared on the absent entries
//! and set on the nearest present one. Same header length; a retained (Foll) entry the
//! decoder lacks is tolerated; an intra-refresh wave then sweeps the garbage base clean.

use std::ops::Range;

use cros_codecs::codec::h265::parser::{Pps, SliceHeader, Sps};
use tracing::debug;

use super::{H265Planner, PlanError, PlanWarning};

/// What the pump does with the access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Concealment {
    /// Every current reference is present: decode the AU as it came.
    Intact,
    /// Decode these bytes instead: every current reference names a picture the decoder holds.
    Rewritten(Vec<u8>),
    /// No present entry to stand in. Keep this and every following delta off the decoder and
    /// ask for an IDR; the concealer resumes at the next IRAP.
    Unrecoverable,
}

/// One elementary stream's concealer. Feed exactly what the decoder receives, in decode
/// order: the planner inside mirrors the decoder's DPB, and a picture fed here but never
/// decoded would be "present" for the next rewrite.
pub struct H265Concealer {
    planner: H265Planner,
}

impl Default for H265Concealer {
    fn default() -> Self {
        Self::new()
    }
}

impl H265Concealer {
    pub fn new() -> Self {
        Self {
            planner: H265Planner::new(),
        }
    }

    /// Fold one access unit; see [`Concealment`].
    pub fn conceal(&mut self, au: &[u8]) -> Concealment {
        let plan = match self.planner.plan_au(au) {
            Ok(plan) => plan,
            Err(PlanError::AwaitingIdr) => return Concealment::Unrecoverable,
            // Outside what the planner follows: the decoder gets the AU untouched, as before.
            Err(err) => {
                debug!(%err, "h265 conceal: AU not planned, passing it through");
                return Concealment::Intact;
            }
        };
        let missing = plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::MissingReference { .. }));
        if !missing {
            return Concealment::Intact;
        }
        let present: Vec<i32> = plan.dpb_refs.iter().map(|r| r.pic_order_cnt).collect();
        let mut patches: Vec<(Range<usize>, Vec<u8>)> = Vec::new();
        for slice in &plan.slices {
            // A dependent segment inherits the RPS; only the independent headers carry the bits.
            if slice.header.dependent_slice_segment_flag {
                continue;
            }
            let nal = &au[slice.data.clone()];
            match rewrite_slice(
                nal,
                &slice.header,
                &plan.sps,
                &plan.pps,
                plan.picture.pic_order_cnt,
                &present,
            ) {
                Some(bytes) => patches.push((slice.data.clone(), bytes)),
                None => {
                    // The decoder never sees this picture, so the mirror must forget it too.
                    self.planner.flush();
                    return Concealment::Unrecoverable;
                }
            }
        }
        let mut out = Vec::with_capacity(au.len() + 8);
        let mut cursor = 0;
        for (range, bytes) in patches {
            out.extend_from_slice(&au[cursor..range.start]);
            out.extend_from_slice(&bytes);
            cursor = range.end;
        }
        out.extend_from_slice(&au[cursor..]);
        Concealment::Rewritten(out)
    }
}

/// Rewrite one slice NALU (start code included) so its current short-term references are
/// present pictures. `None` when the RPS cannot be moved: it lives in the SPS or is
/// inter-predicted, no present entry exists, or the miss is a long-term entry.
fn rewrite_slice(
    nal: &[u8],
    hdr: &SliceHeader,
    sps: &Sps,
    pps: &Pps,
    cur_poc: i32,
    present: &[i32],
) -> Option<Vec<u8>> {
    let rps = &hdr.short_term_ref_pic_set;
    if hdr.short_term_ref_pic_set_sps_flag || rps.inter_ref_pic_set_prediction_flag {
        return None;
    }
    let header_at = nal.windows(3).position(|w| w == [0, 0, 1])? + 3;
    let nal_type = (nal.get(header_at)? >> 1) & 0x3f;
    let rbsp = unescape(nal.get(header_at + 2..)?);
    let bits = locate_used_flags(&rbsp, nal_type, sps, pps)?;

    let mut entries: Vec<(usize, i32, bool)> = Vec::new(); // (flag bit, POC, used)
    for (i, &bit) in bits.s0.iter().enumerate() {
        entries.push((
            bit,
            cur_poc.saturating_add(rps.delta_poc_s0[i]),
            rps.used_by_curr_pic_s0[i],
        ));
    }
    for (i, &bit) in bits.s1.iter().enumerate() {
        entries.push((
            bit,
            cur_poc.saturating_add(rps.delta_poc_s1[i]),
            rps.used_by_curr_pic_s1[i],
        ));
    }
    // The walk must land where the parser did, or the flips hit slice data.
    if bits.s0.len() != usize::from(rps.num_negative_pics)
        || bits.s1.len() != usize::from(rps.num_positive_pics)
        || entries
            .iter()
            .any(|&(bit, _, used)| read_bit(&rbsp, bit) != used)
    {
        return None;
    }
    let is_present = |poc: i32| present.contains(&poc);
    let mut flips: Vec<(usize, bool)> = entries
        .iter()
        .filter(|&&(_, poc, used)| used && !is_present(poc))
        .map(|&(bit, _, _)| (bit, false))
        .collect();
    if flips.is_empty() {
        return None;
    }
    if !entries
        .iter()
        .any(|&(_, poc, used)| used && is_present(poc))
    {
        // Nearest first: S0 runs backwards from the current picture, then S1 forwards.
        let &(bit, _, _) = entries.iter().find(|&&(_, poc, _)| is_present(poc))?;
        flips.push((bit, true));
    }
    let mut rbsp = rbsp;
    for (bit, value) in flips {
        let mask = 0x80 >> (bit % 8);
        if value {
            rbsp[bit / 8] |= mask;
        } else {
            rbsp[bit / 8] &= !mask;
        }
    }
    let mut out = nal[..header_at + 2].to_vec();
    out.extend(escape(&rbsp));
    Some(out)
}

/// Bit positions (RBSP-relative) of every `used_by_curr_pic_s0/s1_flag` in an inline RPS.
struct UsedFlagBits {
    s0: Vec<usize>,
    s1: Vec<usize>,
}

/// Walk 7.3.6.1 up to and through `st_ref_pic_set()`. `None` for a dependent segment, an
/// IDR, an SPS-indexed or inter-predicted RPS, or a truncated header.
fn locate_used_flags(rbsp: &[u8], nal_type: u8, sps: &Sps, pps: &Pps) -> Option<UsedFlagBits> {
    let mut b = Bits { data: rbsp, pos: 0 };
    let first_slice_segment_in_pic = b.flag()?;
    if (16..=23).contains(&nal_type) {
        b.flag()?; // no_output_of_prior_pics_flag
    }
    b.ue()?; // slice_pic_parameter_set_id
    if !first_slice_segment_in_pic {
        if pps.dependent_slice_segments_enabled_flag && b.flag()? {
            return None;
        }
        b.u(ceil_log2(sps.pic_size_in_ctbs_y))?; // slice_segment_address
    }
    b.u(u32::from(pps.num_extra_slice_header_bits))?;
    b.ue()?; // slice_type
    if pps.output_flag_present_flag {
        b.flag()?;
    }
    if sps.separate_colour_plane_flag {
        b.u(2)?;
    }
    if nal_type == 19 || nal_type == 20 {
        return None; // IDR: no RPS
    }
    b.u(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4)?; // slice_pic_order_cnt_lsb
    if b.flag()? {
        return None; // short_term_ref_pic_set_sps_flag
    }
    if sps.num_short_term_ref_pic_sets != 0 && b.flag()? {
        return None; // inter_ref_pic_set_prediction_flag
    }
    let num_negative = b.ue()?;
    let num_positive = b.ue()?;
    let mut s0 = Vec::new();
    for _ in 0..num_negative.min(16) {
        b.ue()?; // delta_poc_s0_minus1
        s0.push(b.pos);
        b.flag()?;
    }
    let mut s1 = Vec::new();
    for _ in 0..num_positive.min(16) {
        b.ue()?; // delta_poc_s1_minus1
        s1.push(b.pos);
        b.flag()?;
    }
    Some(UsedFlagBits { s0, s1 })
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn flag(&mut self) -> Option<bool> {
        Some(self.u(1)? == 1)
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            if self.pos / 8 >= self.data.len() {
                return None;
            }
            v = (v << 1) | u32::from(read_bit(self.data, self.pos));
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }
}

fn read_bit(data: &[u8], bit: usize) -> bool {
    data[bit / 8] & (0x80 >> (bit % 8)) != 0
}

fn ceil_log2(n: u32) -> u32 {
    if n <= 1 {
        0
    } else {
        32 - (n - 1).leading_zeros()
    }
}

/// Strip `emulation_prevention_three_byte`s (7.3.1.1).
fn unescape(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut zeros = 0usize;
    for &byte in payload {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Re-insert them. Deterministic over the RBSP, so the untouched slice data re-escapes to
/// its original bytes.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 8);
    let mut zeros = 0usize;
    for &byte in rbsp {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::{opening_idr_au, trail_p, SpsOpts};
    use super::super::H265Planner;
    use super::*;
    use crate::testing::split_h265_aus;

    fn idr() -> Vec<u8> {
        opening_idr_au(&SpsOpts::default())
    }

    fn has_missing(planner: &mut H265Planner, au: &[u8]) -> bool {
        planner
            .plan_au(au)
            .expect("plans")
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::MissingReference { .. }))
    }

    #[test]
    fn a_present_reference_leaves_the_au_intact() {
        let mut c = H265Concealer::new();
        assert_eq!(c.conceal(&idr()), Concealment::Intact);
        assert_eq!(c.conceal(&trail_p(1, &[(0, true)], 1)), Concealment::Intact);
    }

    #[test]
    fn a_missing_current_reference_moves_to_the_nearest_present_entry() {
        let mut c = H265Concealer::new();
        c.conceal(&idr());
        // POC 1 never arrived; POC 2 names it current and retains the IDR.
        let au = trail_p(2, &[(0, true), (0, false)], 1);
        let Concealment::Rewritten(out) = c.conceal(&au) else {
            panic!("a missing current reference is rewritten");
        };
        assert_eq!(out.len(), au.len());
        let moved: Vec<usize> = (0..au.len() * 8)
            .filter(|&b| read_bit(&au, b) != read_bit(&out, b))
            .collect();
        assert_eq!(
            moved.len(),
            2,
            "exactly the two used_by_curr_pic flags move"
        );
        // A decoder that saw only the IDR now plans it with no missing reference.
        let mut fresh = H265Planner::new();
        assert!(!has_missing(&mut fresh, &idr()));
        let plan = fresh.plan_au(&out).expect("plans");
        assert!(!plan
            .warnings
            .iter()
            .any(|w| matches!(w, PlanWarning::MissingReference { .. })));
        let current: Vec<i32> = plan
            .rps
            .st_curr_before
            .iter()
            .map(|r| r.pic_order_cnt)
            .collect();
        assert_eq!(current, vec![0]);
    }

    #[test]
    fn no_present_entry_in_the_rps_is_unrecoverable_until_an_idr() {
        let mut c = H265Concealer::new();
        c.conceal(&idr());
        // POC 2 names only the missing POC 1: nothing in its RPS can stand in.
        assert_eq!(
            c.conceal(&trail_p(2, &[(0, true)], 1)),
            Concealment::Unrecoverable
        );
        assert_eq!(
            c.conceal(&trail_p(3, &[(0, true)], 1)),
            Concealment::Unrecoverable
        );
        assert_eq!(c.conceal(&idr()), Concealment::Intact);
        assert_eq!(c.conceal(&trail_p(1, &[(0, true)], 1)), Concealment::Intact);
    }

    /// The Vulkan encoder's intra-refresh smoke (IDR, P1, P2, wave 3–6, P7, anchor 8) with
    /// frames 1–2 removed. Only the wave start names a missing picture; measured on
    /// VideoToolbox, the rewritten stream decodes end to end with the close bit-exact.
    #[test]
    fn the_vulkan_wave_dump_conceals_only_its_first_post_loss_picture() {
        let dropped = include_bytes!("../../tests/vectors/vkenc-wave-smoke-dropped.h265");
        let aus = split_h265_aus(dropped);
        assert_eq!(aus.len(), 7);
        let mut c = H265Concealer::new();
        assert_eq!(c.conceal(aus[0]), Concealment::Intact);
        let Concealment::Rewritten(out) = c.conceal(aus[1]) else {
            panic!("the wave start names the lost POC 2 as current");
        };
        // used_by_curr_pic_s0_flag[0] (POC 2) clears, [2] (POC 0) sets: NAL bits 37 and 41.
        let diff: Vec<(usize, u8)> = aus[1]
            .iter()
            .zip(&out)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, (a, b))| (i, a ^ b))
            .collect();
        assert_eq!(diff.len(), 2);
        assert_eq!((diff[0].1, diff[1].1), (0x04, 0x40));
        assert_eq!(diff[1].0, diff[0].0 + 1);
        let mut concealed = [aus[0].to_vec(), out].concat();
        for au in &aus[2..] {
            assert_eq!(c.conceal(au), Concealment::Intact);
            concealed.extend_from_slice(au);
        }
        // The full stream never needs it.
        let full = include_bytes!("../../tests/vectors/vkenc-wave-smoke.h265");
        let mut c = H265Concealer::new();
        for au in split_h265_aus(full) {
            assert_eq!(c.conceal(au), Concealment::Intact);
        }
        // For the VideoToolbox replay harness.
        if let Ok(path) = std::env::var("PF_CONCEAL_OUT") {
            std::fs::write(path, concealed).expect("write");
        }
    }
}
