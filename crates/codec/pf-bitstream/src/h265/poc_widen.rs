//! Recode an HEVC stream for Amlogic's decoder: a 16-bit picture order count, and one
//! current reference per picture. Both leave the decode bit-exact.
//!
//! The decoder resolves a long-term reference whose picture lies behind a POC lsb wrap one
//! cycle off, misses it, and shows nothing until the next IDR. AMF codes a 4-bit lsb, so that
//! is every 16 frames. With 16 bits a long-term entry sits in the current cycle, the form
//! the decoder resolves. Every picture keeps its `PicOrderCntVal`.
//!
//! NVENC marks every long-term picture it holds as current in each slice, though the slice
//! predicts from one picture. The decoder takes the third picture after an IDR for an error
//! frame, and every picture after it. A P slice with one active reference gets its set with
//! that one picture current ([`one_current`]); the others stay in the set, unused.
//!
//! A picture after a gap of half a cycle or more gets the POC its long-term entries need:
//! the widener knows which pictures the decoder holds.

use std::io::Cursor;
use std::ops::Range;

use cros_codecs::codec::h265::parser::{Nalu, Parser, Pps, ShortTermRefPicSet, SliceHeader, Sps};

use super::conceal::{ceil_log2, escape, read_bit, unescape, Bits};

/// The widened `log2_max_pic_order_cnt_lsb`, the largest the syntax allows.
const WIDE_BITS: u32 = 16;
/// From here a keyframe is due: an IDR restarts the count before the 16-bit wrap.
const REFRESH_AT: i32 = 0xE000;
/// Cycles a gap may have swallowed. 8 × 16 frames is two seconds at 60 Hz.
const GAP_CYCLES: i32 = 8;

/// One elementary stream's widener. Feed every access unit the decoder gets, in order.
#[derive(Default)]
pub struct PocWidener {
    parser: Parser,
    /// `PicOrderCntVal` of the picture in progress and of the last TemporalId 0 reference.
    cur: i32,
    prev_tid0: i32,
    /// POCs the decoder holds: the last picture and what its RPS kept.
    held: Vec<i32>,
    refresh_asked: bool,
}

impl PocWidener {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` once per keyframe interval when the count nears its 16-bit wrap: ask the host
    /// for a keyframe.
    pub fn refresh_due(&mut self) -> bool {
        let due = self.cur >= REFRESH_AT && !self.refresh_asked;
        self.refresh_asked |= due;
        due
    }

    /// The access unit recoded. `None` for a stream this cannot recode (long-term candidates
    /// in the SPS, a header that does not parse): drop the widener, feed the stream as it
    /// came and ask for a keyframe.
    pub fn widen(&mut self, au: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(au.len() + 16);
        // Every slice of a picture codes the same set and has its own reference list: only
        // a picture in one slice gets its set rewritten.
        let single = vcl_nals(au) == 1;
        let mut cursor = Cursor::new(au);
        let mut end = 0usize;
        while let Ok(nalu) = Nalu::next(&mut cursor) {
            let at = cursor.position() as usize;
            let (start, stop) = (at - nalu.offset, at + nalu.size);
            cursor.set_position(stop as u64);
            out.extend_from_slice(au.get(end..start)?);
            end = stop;
            let (sc, nal) = au[start..stop].split_at(nalu.offset);
            out.extend_from_slice(sc);
            let nal_type = (nal.first()? >> 1) & 0x3f;
            let tid = (nal.get(1)? & 7).saturating_sub(1);
            match nal_type {
                32 => {
                    self.parser.parse_vps(&nalu).ok()?;
                    out.extend_from_slice(nal);
                }
                33 => {
                    let sps = self.parser.parse_sps(&nalu).ok()?;
                    if sps.num_long_term_ref_pics_sps != 0 {
                        return None;
                    }
                    out.extend(widen_sps(nal)?);
                }
                34 => {
                    self.parser.parse_pps(&nalu).ok()?;
                    out.extend_from_slice(nal);
                }
                0..=9 | 16..=21 => {
                    let slice = self.parser.parse_slice_header(nalu).ok()?;
                    let hdr = &slice.header;
                    let pps = self.parser.get_pps(hdr.pic_parameter_set_id)?.clone();
                    let sps = &pps.sps;
                    let bits = u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4;
                    let idr = nal_type == 19 || nal_type == 20;
                    if hdr.first_slice_segment_in_pic_flag {
                        self.begin_picture(hdr, nal_type, tid, bits);
                    }
                    if idr || hdr.dependent_slice_segment_flag || bits == WIDE_BITS {
                        out.extend_from_slice(nal);
                    } else {
                        let lt = long_term_pocs(hdr, self.cur, bits, &self.held);
                        let cur = self.cur;
                        out.extend(widen_slice(nal, nal_type, hdr, &pps, cur, &lt, single)?);
                    }
                    if hdr.first_slice_segment_in_pic_flag {
                        self.end_picture(hdr, sps, bits);
                    }
                }
                _ => out.extend_from_slice(nal),
            }
        }
        out.extend_from_slice(&au[end..]);
        Some(out)
    }

    /// 8.3.1, then the gap rule: the smallest later cycle whose used long-term entries all
    /// name held pictures.
    fn begin_picture(&mut self, hdr: &SliceHeader, nal_type: u8, tid: u8, bits: u32) {
        let max = 1i32 << bits;
        let lsb = i32::from(hdr.pic_order_cnt_lsb);
        if (16..=20).contains(&nal_type) {
            // IDR and BLA restart the count.
            self.cur = if nal_type >= 19 { 0 } else { lsb };
            self.refresh_asked = false;
        } else {
            let prev_lsb = self.prev_tid0 & (max - 1);
            let msb = self.prev_tid0 - prev_lsb;
            self.cur = lsb
                + if lsb < prev_lsb && prev_lsb - lsb >= max / 2 {
                    msb + max
                } else if lsb > prev_lsb && lsb - prev_lsb > max / 2 {
                    msb - max
                } else {
                    msb
                };
            let used = |cur: i32| {
                let n = usize::from(hdr.num_long_term_pics);
                long_term_pocs(hdr, cur, bits, &[])
                    .into_iter()
                    .zip(hdr.used_by_curr_pic_lt)
                    .zip(hdr.delta_poc_msb_present_flag)
                    .take(n)
                    .filter(|&((_, used), msb)| used && msb)
                    .map(|((poc, _), _)| poc)
                    .collect::<Vec<_>>()
            };
            let fits = |cur: i32| used(cur).iter().all(|p| self.held.contains(p));
            if !used(self.cur).is_empty() && !fits(self.cur) {
                if let Some(k) = (1..=GAP_CYCLES).find(|k| fits(self.cur + k * max)) {
                    self.cur += k * max;
                }
            }
        }
        // RASL, RADL and sub-layer non-reference pictures never anchor the next POC.
        let sub_layer_non_ref = nal_type < 16 && nal_type % 2 == 0;
        if tid == 0 && !(6..=9).contains(&nal_type) && !sub_layer_non_ref {
            self.prev_tid0 = self.cur;
        }
    }

    /// What the decoder holds for the next picture: this one and every RPS entry.
    fn end_picture(&mut self, hdr: &SliceHeader, sps: &Sps, bits: u32) {
        let mut held = vec![self.cur];
        if let Some(rps) = short_term_set(hdr, sps) {
            let s0 = &rps.delta_poc_s0[..usize::from(rps.num_negative_pics).min(16)];
            let s1 = &rps.delta_poc_s1[..usize::from(rps.num_positive_pics).min(16)];
            held.extend(s0.iter().chain(s1).map(|d| self.cur + d));
        }
        held.extend(long_term_pocs(hdr, self.cur, bits, &self.held));
        self.held = held;
    }
}

/// `PicOrderCntVal` of each slice long-term entry (8.3.2). An entry without its msb names
/// the held picture with that lsb, else the nearest earlier one.
fn long_term_pocs(hdr: &SliceHeader, cur: i32, bits: u32, held: &[i32]) -> Vec<i32> {
    let max = 1i32 << bits;
    let lsb = cur & (max - 1);
    (0..usize::from(hdr.num_long_term_pics).min(16))
        .map(|i| {
            let lt_lsb = hdr.poc_lsb_lt[i] as i32;
            if hdr.delta_poc_msb_present_flag[i] {
                // The parser holds DeltaPocMsbCycleLt, already summed (7-52).
                cur - hdr.delta_poc_msb_cycle_lt[i] as i32 * max - (lsb - lt_lsb)
            } else {
                let nearest = cur - (lsb - lt_lsb).rem_euclid(max);
                *held
                    .iter()
                    .find(|&&p| p & (max - 1) == lt_lsb)
                    .unwrap_or(&nearest)
            }
        })
        .collect()
}

/// The SPS NAL with `log2_max_pic_order_cnt_lsb_minus4` = 12. Unchanged when it already is.
fn widen_sps(nal: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(nal);
    let mut b = Bits {
        data: &rbsp,
        pos: 16,
    };
    b.u(4)?; // sps_video_parameter_set_id
    let sub_layers = b.u(3)?;
    b.u(1)?; // sps_temporal_id_nesting_flag
    b.pos += 96; // general profile, tier and level
    let present: Vec<(bool, bool)> = (0..sub_layers)
        .map(|_| Some((b.flag()?, b.flag()?)))
        .collect::<Option<_>>()?;
    if sub_layers > 0 {
        b.pos += 2 * (8 - sub_layers as usize);
    }
    for (profile, level) in present {
        b.pos += usize::from(profile) * 88 + usize::from(level) * 8;
    }
    b.ue()?; // sps_seq_parameter_set_id
    if b.ue()? == 3 {
        b.flag()?; // separate_colour_plane_flag
    }
    b.ue()?; // pic_width_in_luma_samples
    b.ue()?; // pic_height_in_luma_samples
    if b.flag()? {
        for _ in 0..4 {
            b.ue()?; // conformance window
        }
    }
    b.ue()?; // bit_depth_luma_minus8
    b.ue()?; // bit_depth_chroma_minus8
    let field = b.pos;
    if b.ue()? + 4 == WIDE_BITS {
        return Some(nal.to_vec());
    }
    let stop = last_set_bit(&rbsp)?;
    let mut w = BitWriter::default();
    w.copy(&rbsp, 0..field);
    w.ue(WIDE_BITS - 4);
    w.copy(&rbsp, b.pos..stop);
    Some(escape(&w.finish()))
}

/// The slice's short-term set: the SPS's by index, or its own.
fn short_term_set<'a>(hdr: &'a SliceHeader, sps: &'a Sps) -> Option<&'a ShortTermRefPicSet> {
    if hdr.short_term_ref_pic_set_sps_flag {
        sps.short_term_ref_pic_set
            .get(usize::from(hdr.short_term_ref_pic_set_idx))
    } else {
        Some(&hdr.short_term_ref_pic_set)
    }
}

/// Slice segment NALs in the access unit. An intra-refresh wave from NVENC codes its
/// stripe as a slice of its own, so each of its pictures has two or three.
pub fn vcl_nals(au: &[u8]) -> usize {
    let mut cursor = Cursor::new(au);
    let mut n = 0;
    while let Ok(nalu) = Nalu::next(&mut cursor) {
        let at = cursor.position() as usize;
        n += usize::from(au.get(at).is_some_and(|b| (b >> 1) & 0x3f < 32));
        cursor.set_position((at + nalu.size) as u64);
    }
    n
}

/// The one picture a slice predicts from, and the header bits around the list modification
/// that named it.
struct OneRef {
    /// Its entry among the short-term ones, negatives then positives, or the long-term ones.
    st: Option<usize>,
    lt: Option<usize>,
    /// From behind the long-term entries to the list modification.
    kept: Range<usize>,
    /// Behind the list modification.
    rest: usize,
}

/// `Some` for a P slice with one active reference whose set marks more than one picture
/// current: the picture `RefPicList0[0]` names (8.3.4). `b` stands behind the long-term
/// entries. `None` leaves the set as it came.
fn one_current(mut b: Bits, slice_type: u32, hdr: &SliceHeader, pps: &Pps) -> Option<OneRef> {
    let (sps, total) = (&pps.sps, hdr.num_pic_total_curr);
    // A picture that references itself counts in `total` and sits in neither set.
    if slice_type != 1 || total < 2 || pps.scc_extension.curr_pic_ref_enabled_flag {
        return None;
    }
    let from = b.pos;
    if sps.temporal_mvp_enabled_flag {
        b.flag()?; // slice_temporal_mvp_enabled_flag
    }
    if sps.sample_adaptive_offset_enabled_flag {
        b.flag()?; // slice_sao_luma_flag
        if sps.chroma_format_idc != 0 && !sps.separate_colour_plane_flag {
            b.flag()?; // slice_sao_chroma_flag
        }
    }
    // num_ref_idx_active_override_flag, num_ref_idx_l0_active_minus1
    let active = match b.flag()? {
        true => b.ue()?,
        false => u32::from(pps.num_ref_idx_l0_default_active_minus1),
    };
    if active != 0 {
        return None;
    }
    let kept = from..b.pos;
    // ref_pic_list_modification_flag_l0, list_entry_l0[0]
    let entry = match pps.lists_modification_present_flag && b.flag()? {
        true => b.u(ceil_log2(total))? as usize,
        false => 0,
    };
    // RefPicListTemp0: the current short-term pictures before, then after, then long-term.
    let set = short_term_set(hdr, sps)?;
    let neg = usize::from(set.num_negative_pics);
    let after = (0..usize::from(set.num_positive_pics))
        .filter_map(|i| set.used_by_curr_pic_s1[i].then_some(neg + i));
    let st: Vec<usize> = (0..neg)
        .filter(|&i| set.used_by_curr_pic_s0[i])
        .chain(after)
        .collect();
    let lt: Vec<usize> = (0..usize::from(hdr.num_long_term_pics).min(16))
        .filter(|&i| hdr.used_by_curr_pic_lt[i])
        .collect();
    let (st, lt) = match st.get(entry) {
        Some(&i) => (Some(i), None),
        None => (None, Some(*lt.get(entry - st.len())?)),
    };
    Some(OneRef {
        st,
        lt,
        kept,
        rest: b.pos,
    })
}

/// `set` as the slice's own, with only entry `current` used by the picture.
fn write_short_term(
    w: &mut BitWriter,
    set: &ShortTermRefPicSet,
    sps_sets: u32,
    current: Option<usize>,
) -> Option<()> {
    w.u(0, 1); // short_term_ref_pic_set_sps_flag
    if sps_sets != 0 {
        w.u(0, 1); // inter_ref_pic_set_prediction_flag
    }
    let neg = usize::from(set.num_negative_pics);
    let pos = usize::from(set.num_positive_pics);
    w.ue(neg as u32);
    w.ue(pos as u32);
    let mut prev = 0;
    for i in 0..neg {
        w.ue(u32::try_from(prev - set.delta_poc_s0[i] - 1).ok()?);
        w.u(u32::from(current == Some(i)), 1);
        prev = set.delta_poc_s0[i];
    }
    let mut prev = 0;
    for i in 0..pos {
        w.ue(u32::try_from(set.delta_poc_s1[i] - prev - 1).ok()?);
        w.u(u32::from(current == Some(neg + i)), 1);
        prev = set.delta_poc_s1[i];
    }
    Some(())
}

/// One slice segment NAL with the picture's and its long-term entries' full POC in 16 bits
/// and, in a `single` slice picture, one current reference ([`one_current`]). `lt` holds
/// each long-term entry's `PicOrderCntVal`.
fn widen_slice(
    nal: &[u8],
    nal_type: u8,
    hdr: &SliceHeader,
    pps: &Pps,
    cur: i32,
    lt: &[i32],
    single: bool,
) -> Option<Vec<u8>> {
    let sps = &pps.sps;
    let old_bits = u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4;
    let rbsp = unescape(nal);
    // The header ends byte-aligned behind its stop bit; slice data follows untouched.
    let data_at = (hdr.header_bit_size / 8) as usize;
    let stop = last_set_bit(rbsp.get(..data_at)?)?;

    let mut b = Bits {
        data: &rbsp,
        pos: 16,
    };
    let first = b.flag()?;
    if (16..=23).contains(&nal_type) {
        b.flag()?; // no_output_of_prior_pics_flag
    }
    b.ue()?; // slice_pic_parameter_set_id
    if !first {
        if pps.dependent_slice_segments_enabled_flag {
            b.flag()?;
        }
        b.u(ceil_log2(sps.pic_size_in_ctbs_y))?; // slice_segment_address
    }
    b.u(u32::from(pps.num_extra_slice_header_bits))?;
    let slice_type = b.ue()?;
    if pps.output_flag_present_flag {
        b.flag()?;
    }
    if sps.separate_colour_plane_flag {
        b.u(2)?;
    }
    let poc_at = b.pos;
    if b.u(old_bits)? != u32::from(hdr.pic_order_cnt_lsb) {
        return None; // the walk left the parser's path
    }
    let sets = u32::from(sps.num_short_term_ref_pic_sets);
    if b.flag()? {
        if sets > 1 {
            b.u(ceil_log2(sets))?; // short_term_ref_pic_set_idx
        }
    } else if sets != 0 && b.flag()? {
        // inter_ref_pic_set_prediction_flag: a flag per entry of the SPS set it predicts
        // from and one for the delta itself, each unused one followed by use_delta_flag.
        let back = b.ue()? + 1; // delta_idx_minus1
        b.flag()?; // delta_rps_sign
        b.ue()?; // abs_delta_rps_minus1
        let from = sps
            .short_term_ref_pic_set
            .get(sets.checked_sub(back)? as usize)?;
        for _ in 0..=from.num_delta_pocs {
            if !b.flag()? {
                b.flag()?;
            }
        }
    } else {
        let (neg, pos) = (b.ue()?, b.ue()?);
        for _ in 0..(neg + pos).min(32) {
            b.ue()?; // delta_poc_sX_minus1
            b.flag()?; // used_by_curr_pic_sX_flag
        }
    }
    let lt_at = b.pos;
    let n = lt.len();
    if sps.long_term_ref_pics_present_flag {
        if b.ue()? as usize != n {
            return None;
        }
        for _ in 0..n {
            b.u(old_bits)?; // poc_lsb_lt
            b.flag()?; // used_by_curr_pic_lt_flag
            if b.flag()? {
                b.ue()?; // delta_poc_msb_cycle_lt
            }
        }
    }
    if b.pos > stop {
        return None;
    }
    let after_lt = Bits {
        data: &rbsp,
        pos: b.pos,
    };
    let one = single
        .then(|| one_current(after_lt, slice_type, hdr, pps))
        .flatten()
        .filter(|one| one.rest <= stop);

    let wide = |poc: i32| (poc & 0xffff) as u32;
    let mut w = BitWriter::default();
    w.copy(&rbsp, 0..poc_at);
    w.u(wide(cur), WIDE_BITS);
    match &one {
        Some(one) => write_short_term(&mut w, short_term_set(hdr, sps)?, sets, one.st)?,
        None => w.copy(&rbsp, poc_at + old_bits as usize..lt_at),
    }
    if sps.long_term_ref_pics_present_flag {
        w.ue(n as u32);
        let mut cycles = 0i32;
        for (i, &poc) in lt.iter().enumerate() {
            w.u(wide(poc), WIDE_BITS);
            let used = one
                .as_ref()
                .map_or(hdr.used_by_curr_pic_lt[i], |one| one.lt == Some(i));
            w.u(u32::from(used), 1);
            // Whole 16-bit cycles between the two; 0 until the count wraps.
            let cycle = (cur - poc - (wide(cur) as i32 - wide(poc) as i32)) >> WIDE_BITS;
            let step = cycle - cycles;
            // An entry coded without its msb stays so; a step back cannot be coded.
            if hdr.delta_poc_msb_present_flag[i] && step >= 0 {
                w.u(1, 1);
                w.ue(step as u32);
                cycles = cycle;
            } else {
                w.u(0, 1);
            }
        }
    }
    match one {
        // One current picture: the slice codes no list modification.
        Some(one) => {
            w.copy(&rbsp, one.kept);
            w.copy(&rbsp, one.rest..stop);
        }
        None => w.copy(&rbsp, b.pos..stop),
    }
    let mut out = w.finish();
    out.extend_from_slice(&rbsp[data_at..]);
    Some(escape(&out))
}

/// The RBSP stop bit: the last set bit.
fn last_set_bit(rbsp: &[u8]) -> Option<usize> {
    let byte = rbsp.iter().rposition(|&b| b != 0)?;
    Some(byte * 8 + 7 - rbsp[byte].trailing_zeros() as usize)
}

#[derive(Default)]
struct BitWriter {
    out: Vec<u8>,
    bits: usize,
}

impl BitWriter {
    fn u(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            if self.bits % 8 == 0 {
                self.out.push(0);
            }
            if value >> i & 1 == 1 {
                *self.out.last_mut().expect("pushed above") |= 0x80 >> (self.bits % 8);
            }
            self.bits += 1;
        }
    }

    fn ue(&mut self, value: u32) {
        let len = 32 - (value + 1).leading_zeros();
        self.u(0, len - 1);
        self.u(value + 1, len);
    }

    fn copy(&mut self, from: &[u8], bits: std::ops::Range<usize>) {
        for bit in bits {
            self.u(u32::from(read_bit(from, bit)), 1);
        }
    }

    /// Stop bit, then zeros to the byte boundary.
    fn finish(mut self) -> Vec<u8> {
        self.u(1, 1);
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        idr_slice, param_sets, param_sets_lists, synth_slice, SliceOpts, SpsOpts,
    };
    use super::*;

    /// A long-term stream's opening AU and a P picture at `poc_lsb` with one used entry.
    fn opening() -> Vec<u8> {
        let mut sps = SpsOpts::default();
        sps.long_term = true;
        let mut au = param_sets(&sps);
        au.extend(idr_slice());
        au
    }

    fn p(poc_lsb: u32, lt_lsb: u32, cycle: u32) -> Vec<u8> {
        synth_slice(&SliceOpts {
            poc_lsb,
            lt: vec![(lt_lsb, true, Some(cycle))],
            sps_long_term: true,
            num_ref_idx_l0: 1,
            ..Default::default()
        })
    }

    /// The last slice header of each AU as a decoder reads the widened stream.
    fn headers(aus: &[Vec<u8>]) -> Vec<SliceHeader> {
        let mut parser = Parser::default();
        let mut widener = PocWidener::new();
        aus.iter()
            .map(|au| {
                let wide = widener.widen(au).expect("recodes");
                let mut cursor = Cursor::new(wide.as_slice());
                let mut last = None;
                while let Ok(nalu) = Nalu::next(&mut cursor) {
                    let end = cursor.position() as usize + nalu.size;
                    let kind = (wide[cursor.position() as usize] >> 1) & 0x3f;
                    match kind {
                        33 => drop(parser.parse_sps(&nalu).unwrap()),
                        34 => drop(parser.parse_pps(&nalu).unwrap()),
                        0..=21 => last = Some(parser.parse_slice_header(nalu).unwrap().header),
                        _ => {}
                    }
                    cursor.set_position(end as u64);
                }
                last.expect("a slice")
            })
            .collect()
    }

    #[test]
    fn a_long_term_entry_behind_the_wrap_stays_in_the_current_cycle() {
        // 4-bit lsb: picture 16 codes lsb 0 and reaches the IDR one cycle back.
        let mut aus = vec![opening()];
        aus.extend((1..16).map(|i| p(i, 0, 0)));
        aus.push(p(0, 0, 1));
        let hdr = headers(&aus).pop().unwrap();
        assert_eq!(hdr.pic_order_cnt_lsb, 16);
        assert_eq!(hdr.poc_lsb_lt[0], 0);
        assert!(hdr.delta_poc_msb_present_flag[0]);
        assert_eq!(hdr.delta_poc_msb_cycle_lt[0], 0);
        assert!(hdr.used_by_curr_pic_lt[0]);
    }

    #[test]
    fn a_picture_after_a_long_gap_takes_the_poc_its_reference_needs() {
        // Pictures 2..=24 are lost. Picture 25 codes lsb 9: 8.3.1 alone reads POC 9.
        let aus = vec![opening(), p(1, 0, 0), p(9, 0, 1)];
        let hdr = headers(&aus).pop().unwrap();
        assert_eq!(hdr.pic_order_cnt_lsb, 25);
        assert_eq!(hdr.poc_lsb_lt[0], 0);
    }

    /// A slice whose short-term set is predicted from the SPS's, as QSV codes the pictures
    /// behind a recovery frame, recodes and keeps its set and its long-term entry.
    #[test]
    fn a_slice_set_predicted_from_the_sps_recodes() {
        let mut sps = SpsOpts::default();
        sps.long_term = true;
        sps.chained_inter_rps = Some((2, 0));
        let mut opening = param_sets(&sps);
        opening.extend(idr_slice());
        // The SPS set {-1, -2} one picture earlier: -2 and -3 dropped, the delta -1 used.
        let predicted = |poc_lsb, lt| {
            synth_slice(&SliceOpts {
                poc_lsb,
                inter_rps: Some(vec![(false, false), (false, false), (true, true)]),
                lt,
                sps_long_term: true,
                num_ref_idx_l0: 1,
                ..Default::default()
            })
        };
        let aus = vec![
            opening,
            predicted(1, vec![]),
            predicted(2, vec![(0, false, None)]),
        ];
        let hdr = headers(&aus).pop().unwrap();
        assert_eq!(hdr.pic_order_cnt_lsb, 2);
        let set = &hdr.short_term_ref_pic_set;
        assert!(set.inter_ref_pic_set_prediction_flag);
        assert_eq!((set.num_negative_pics, set.delta_poc_s0[0]), (1, -1));
        assert_eq!((hdr.num_long_term_pics, hdr.poc_lsb_lt[0]), (1, 0));
        assert!(!hdr.used_by_curr_pic_lt[0]);
    }

    /// NVENC's shape: the long-term picture marked current beside the short-term one, one
    /// active reference. The recode leaves current the picture the slice predicts from, by
    /// list order or by the list modification, and a picture in two slices keeps its set.
    #[test]
    fn a_slice_keeps_only_the_picture_it_predicts_from_current() {
        let mut sps = SpsOpts::default();
        sps.long_term = true;
        let mut opening = param_sets_lists(&sps);
        opening.extend(idr_slice());
        let p = |poc_lsb, neg: &[(u32, bool)], lt: &[(u32, bool, Option<u32>)], entry| {
            synth_slice(&SliceOpts {
                poc_lsb,
                neg: neg.to_vec(),
                lt: lt.to_vec(),
                sps_long_term: true,
                num_ref_idx_l0: 1,
                pps_lists_modification: true,
                list_entry_l0: entry,
                ..Default::default()
            })
        };
        let idr = [(0, true, None)];
        let mut two_slices = p(4, &[(0, true)], &idr, None);
        two_slices.extend(p(4, &[(0, true)], &idr, None));
        let aus = vec![
            opening,
            p(1, &[(0, true)], &[], None),
            p(2, &[(0, true)], &idr, None),
            p(3, &[(0, true), (0, false)], &idr, Some(1)),
            two_slices,
        ];
        // (short-term entries current, the long-term entry current, NumPicTotalCurr)
        let current = |h: &SliceHeader| {
            let set = &h.short_term_ref_pic_set;
            let st = set.used_by_curr_pic_s0[..usize::from(set.num_negative_pics)].to_vec();
            (st, h.used_by_curr_pic_lt[0], h.num_pic_total_curr)
        };
        let h = headers(&aus);
        assert_eq!(current(&h[1]), (vec![true], false, 1), "one already");
        assert_eq!(
            current(&h[2]),
            (vec![true], false, 1),
            "first in list order"
        );
        assert_eq!(
            current(&h[3]),
            (vec![false, false], true, 1),
            "the list's entry"
        );
        assert_eq!(current(&h[4]), (vec![true], true, 2), "two slices");
        assert_eq!((h[3].pic_order_cnt_lsb, h[3].poc_lsb_lt[0]), (3, 0));
    }

    /// Parameter sets around a picture are no slices; each slice NAL of a wave picture is.
    #[test]
    fn a_picture_counts_its_slice_nals() {
        assert_eq!(vcl_nals(&opening()), 1);
        let wave: Vec<u8> = (0..3).flat_map(|_| p(1, 0, 0)).collect();
        assert_eq!(vcl_nals(&wave), 3);
    }

    /// An end-of-sequence or end-of-bitstream NAL is its two header bytes. One whose second
    /// byte is zero, with a start code right behind it, passes through byte for byte: the
    /// next NAL starts after it, never inside it.
    #[test]
    fn a_start_code_right_behind_an_end_nal_starts_after_it() {
        for end_nal in [0x48u8, 0x4A] {
            let mut au = opening();
            au.extend([0, 0, 1, end_nal, 0x00]);
            au.extend([0, 0, 1, 0x4A, 0x01]);
            let wide = PocWidener::new().widen(&au).expect("recodes");
            assert!(wide.ends_with(&[0, 0, 1, end_nal, 0x00, 0, 0, 1, 0x4A, 0x01]));
        }
    }

    /// Replays a host AU dump (`PF_HEVC_DUMP`, `[u32 len][u64 pts][u8 key][bytes]` chunks)
    /// and writes the widened Annex B stream to `PF_HEVC_OUT`.
    #[test]
    #[ignore = "needs PF_HEVC_DUMP and PF_HEVC_OUT"]
    fn a_host_dump_recodes_whole() {
        let dump = std::fs::read(std::env::var("PF_HEVC_DUMP").unwrap()).unwrap();
        let mut widener = PocWidener::new();
        let (mut out, mut at, mut aus) = (Vec::new(), 0usize, 0u32);
        while at + 13 <= dump.len() {
            let len = u32::from_le_bytes(dump[at..at + 4].try_into().unwrap()) as usize;
            let au = &dump[at + 13..at + 13 + len];
            out.extend(
                widener
                    .widen(au)
                    .unwrap_or_else(|| panic!("AU {aus} recodes")),
            );
            at += 13 + len;
            aus += 1;
        }
        std::fs::write(std::env::var("PF_HEVC_OUT").unwrap(), out).unwrap();
        println!("{aus} access units");
    }
}
