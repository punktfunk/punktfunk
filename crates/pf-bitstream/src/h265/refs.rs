//! Reference facts every H.265 backend derives the same way from an [`AuPlan`].

use cros_codecs::codec::h265::parser::ScalingLists;

use super::AuPlan;
use super::PicId;
use super::RefPic;

/// Entries in each `RefPicSet*` index array Vulkan and DXVA take. H.265
/// allows more; beyond eight is [`RpsError::SetOverflow`].
pub const RPS_SET_LEN: usize = 8;

/// An index-array entry naming no picture.
pub const RPS_UNUSED: u8 = 0xFF;

/// Why the current RPS sets have no index-array form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpsError {
    /// A current set holds more than [`RPS_SET_LEN`] entries.
    SetOverflow { set: &'static str, len: usize },
    /// A slice list names a picture outside the current sets. 8.3.4 builds
    /// every list from those sets, so no index array could name it.
    OutsideRps(PicId),
}

/// The AU's current RPS sets as one binding list, before any slot lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentRefs {
    /// StCurrBefore, StCurrAfter, LtCurr in that order, first appearance
    /// first, each picture once. The flag is true when any occurrence is
    /// long-term.
    pub refs: Vec<(RefPic, bool)>,
    /// Per set, positions into [`Self::refs`]; [`RPS_UNUSED`] past its end.
    pub index: [[u8; RPS_SET_LEN]; 3],
    /// The other marked pictures (8.3.2 *Foll*), in `dpb_refs` order.
    pub foll: Vec<RefPic>,
}

/// `NumDeltaPocsOfRefRpsIdx` derivation failures. Each backend converts this
/// into its own conversion error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefRpsIdxError {
    /// The first slice's inline `st_ref_pic_set()` predicts from a missing SPS
    /// candidate — the count cannot be derived, and hardware would misparse the
    /// slice header.
    Invalid {
        curr_rps_idx: u8,
        delta_idx_minus1: u8,
    },
    /// Predicted-from candidate `NumDeltaPocs` exceeds `u8`. Impossible off a
    /// real parse (≤ 32); an error rather than a clamp, because a clamped count
    /// makes hardware misparse the slice header.
    NumDeltaPocsOverflow(u32),
}

impl std::fmt::Display for RefRpsIdxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefRpsIdxError::Invalid {
                curr_rps_idx,
                delta_idx_minus1,
            } => {
                write!(
                    f,
                    "inline st_ref_pic_set predicts from a nonexistent candidate \
                     (CurrRpsIdx {curr_rps_idx}, delta_idx_minus1 {delta_idx_minus1})"
                )
            }
            RefRpsIdxError::NumDeltaPocsOverflow(count) => {
                write!(f, "candidate NumDeltaPocs {count} exceeds u8")
            }
        }
    }
}

impl std::error::Error for RefRpsIdxError {}

impl AuPlan {
    /// The current RPS sets as positions into one deduplicated list, with the
    /// slice lists checked against them. Backends resolve slots and choose the
    /// marking; the set walk and its two refusals are shared.
    pub fn current_rps_refs(&self) -> Result<CurrentRefs, RpsError> {
        let mut refs: Vec<(RefPic, bool)> = Vec::new();
        let mut index = [[RPS_UNUSED; RPS_SET_LEN]; 3];
        let sets: [(&'static str, &[RefPic]); 3] = [
            ("RefPicSetStCurrBefore", &self.rps.st_curr_before),
            ("RefPicSetStCurrAfter", &self.rps.st_curr_after),
            ("RefPicSetLtCurr", &self.rps.lt_curr),
        ];
        for (array, (set, entries)) in index.iter_mut().zip(sets) {
            if entries.len() > RPS_SET_LEN {
                return Err(RpsError::SetOverflow {
                    set,
                    len: entries.len(),
                });
            }
            for (entry, rp) in array.iter_mut().zip(entries) {
                // Concealment can resolve two entries to one id: bind it once.
                let position = match refs.iter().position(|(r, _)| r.id == rp.id) {
                    Some(position) => {
                        refs[position].1 |= rp.is_long_term;
                        position
                    }
                    None => {
                        refs.push((*rp, rp.is_long_term));
                        refs.len() - 1
                    }
                };
                // At most 3 × RPS_SET_LEN pictures: well inside u8, never RPS_UNUSED.
                *entry = position as u8;
            }
        }

        for slice in &self.slices {
            for rp in slice.ref_list0.iter().chain(&slice.ref_list1) {
                if !refs.iter().any(|(r, _)| r.id == rp.id) {
                    return Err(RpsError::OutsideRps(rp.id));
                }
            }
        }

        let mut foll: Vec<RefPic> = Vec::new();
        for rp in &self.dpb_refs {
            let bound = refs.iter().any(|(r, _)| r.id == rp.id);
            if !bound && !foll.iter().any(|f| f.id == rp.id) {
                foll.push(*rp);
            }
        }
        Ok(CurrentRefs { refs, index, foll })
    }

    /// `NumDeltaPocsOfRefRpsIdx`: when the first slice's inline `st_ref_pic_set()`
    /// uses inter-RPS prediction, hardware re-parses those slice bits and needs
    /// `NumDeltaPocs[RefRpsIdx]` of the source candidate to size the
    /// `used_by_curr_pic_flag`/`use_delta_flag` loop (7.4.8); otherwise 0.
    ///
    /// Vulkan's `NumDeltaPocsOfRefRpsIdx` and DXVA's `ucNumDeltaPocsOfRefRpsIdx`
    /// both come from here. Panics on a plan with no slices; converters refuse
    /// those first.
    pub fn num_delta_pocs_of_ref_rps_idx(&self) -> Result<u8, RefRpsIdxError> {
        let hdr = &self
            .slices
            .first()
            .expect("caller validated the plan holds slices")
            .header;
        // Inline means CurrRpsIdx == num_short_term_ref_pic_sets (8.3.2 NOTE 2); an
        // SPS-indexed RPS re-parses nothing in the slice header.
        let inline = !hdr.short_term_ref_pic_set_sps_flag
            && hdr.curr_rps_idx == self.sps.num_short_term_ref_pic_sets;
        if !inline || !hdr.short_term_ref_pic_set.inter_ref_pic_set_prediction_flag {
            return Ok(0);
        }
        // RefRpsIdx = stRpsIdx - (delta_idx_minus1 + 1), stRpsIdx = CurrRpsIdx here
        // (equation 7-59). u16 so a hostile delta cannot wrap.
        let delta = hdr.short_term_ref_pic_set.delta_idx_minus1;
        let source = u16::from(hdr.curr_rps_idx)
            .checked_sub(u16::from(delta) + 1)
            .and_then(|idx| self.sps.short_term_ref_pic_set.get(usize::from(idx)))
            .ok_or(RefRpsIdxError::Invalid {
                curr_rps_idx: hdr.curr_rps_idx,
                delta_idx_minus1: delta,
            })?;
        // Real parses have NumDeltaPocs ≤ 32 (u8). A clamp would misparse the slice
        // header on hardware, so a constructed plan that exceeds it is an error.
        u8::try_from(source.num_delta_pocs)
            .map_err(|_| RefRpsIdxError::NumDeltaPocsOverflow(source.num_delta_pocs))
    }

    /// The scaling lists this picture dequantizes with, or `None` when the SPS
    /// disables them; a backend then submits no matrix at all.
    ///
    /// 7.4.5 takes the PPS's coded lists, else the SPS's coded lists, else the
    /// Table 7-5/7-6 defaults. The parser default-fills an uncoded PPS and
    /// leaves an uncoded SPS at zero, so the PPS copy carries the first and the
    /// last case, and the SPS's wins only when it alone coded data. Taking the
    /// SPS's whenever lists are enabled dequantizes every residual to nothing.
    ///
    /// VAAPI, DXVA and V4L2 take their one matrix from here. Vulkan passes the
    /// SPS and PPS lists separately.
    pub fn active_scaling_lists(&self) -> Option<&ScalingLists> {
        let (sps, pps) = (&self.sps, &self.pps);
        if !sps.scaling_list_enabled_flag {
            return None;
        }
        Some(
            if sps.scaling_list_data_present_flag && !pps.scaling_list_data_present_flag {
                &sps.scaling_list
            } else {
                &pps.scaling_list
            },
        )
    }
}
