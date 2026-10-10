//! Test support for the decoder crates: the vendored cros-codecs vectors, the
//! Annex-B and IVF access-unit splitters, authored H.264 fixtures, and the
//! libavcodec parity fixtures in [`parity`].
//!
//! Behind the `test-vectors` feature, which a crate turns on from its
//! `[dev-dependencies]`, so no shipped build carries the vectors.

use cros_codecs::bitstream_utils::IvfIterator;

pub mod parity;

pub const H264_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264");
/// Carries a B slice; the non-high `64x64-I-P-B-P.h264` is constrained
/// baseline, where x264 dropped it.
pub const H264_64X64_I_P_B_P_HIGH: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h264/test_data/64x64-I-P-B-P-high.h264");
pub const H265_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265");
pub const H265_64X64_I_P_B_P: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/64x64-I-P-B-P.h265");
pub const H265_BEAR: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/bear.h265");
pub const H265_BBB: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/h265/test_data/bbb.h265");
/// IVF: 250 temporal units, 274 coded frames.
pub const AV1_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1");
pub const VP9_25FPS: &[u8] =
    include_bytes!("../vendor/cros-codecs/src/codec/vp9/test_data/test-25fps.vp9");

/// Offsets of every Annex-B NAL header. Emulation prevention keeps `00 00 01`
/// out of payloads, so a byte scan finds exactly the start codes.
fn nal_headers(stream: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < stream.len() {
        if stream[i..i + 3] == [0x00, 0x00, 0x01] {
            out.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Split into access units given `(is_slice, first_in_picture)` per NAL
/// header. A new AU begins at a non-VCL NAL after slices, or at a
/// first-of-picture slice once the current AU has slices.
fn split_aus(stream: &[u8], classify: impl Fn(&[u8], usize) -> (bool, bool)) -> Vec<&[u8]> {
    let mut aus = Vec::new();
    let mut au_start = 0usize;
    let mut au_has_slice = false;
    for header in nal_headers(stream) {
        let (is_slice, first_in_picture) = classify(stream, header);
        // The start code owning this header, with the four-byte form's zero.
        let mut start = header - 3;
        if start > 0 && stream[start - 1] == 0x00 {
            start -= 1;
        }
        if au_has_slice && (!is_slice || first_in_picture) {
            aus.push(&stream[au_start..start]);
            au_start = start;
            au_has_slice = false;
        }
        au_has_slice |= is_slice;
    }
    aus.push(&stream[au_start..]);
    aus
}

/// H.264: a one-byte NAL header, slices are types 1 and 5, and
/// `first_mb_in_slice == 0` (ue(v) `1`) is the top bit of the next byte.
pub fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
    split_aus(stream, |s, h| {
        let is_slice = matches!(s[h] & 0x1f, 1 | 5);
        let first = is_slice && s.get(h + 1).is_some_and(|b| b & 0x80 != 0);
        (is_slice, first)
    })
}

/// H.265: a two-byte NAL header, slices are types below 32, and
/// `first_slice_segment_in_pic_flag` is the top bit at `+2`.
pub fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
    split_aus(stream, |s, h| {
        let is_slice = (s[h] >> 1) & 0x3f < 32;
        let first = is_slice && s.get(h + 2).is_some_and(|b| b & 0x80 != 0);
        (is_slice, first)
    })
}

/// AV1 temporal units: the IVF container's frames, since OBUs carry no
/// start codes to scan for.
pub fn split_ivf(stream: &[u8]) -> Vec<&[u8]> {
    IvfIterator::new(stream).collect()
}

/// Authored H.264 parameter sets and hand-written slice headers, for the
/// marking cases no vendored vector carries (MMCO, long-term, MMCO 5).
pub mod h264 {
    use std::rc::Rc;

    use cros_codecs::codec::h264::nalu_writer::NaluWriter;
    use cros_codecs::codec::h264::parser::Level;
    use cros_codecs::codec::h264::parser::NaluType;
    use cros_codecs::codec::h264::parser::Pps;
    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::Sps;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use cros_codecs::codec::h264::synthesizer::Synthesizer;

    /// Main, level 4, four reference frames, POC type 0. `frame_num` and
    /// `pic_order_cnt_lsb` are u(4): [`write_slice`] relies on both minus4
    /// fields being 0.
    pub fn base_sps() -> SpsBuilder {
        SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .log2_max_frame_num_minus4(0)
            .pic_order_cnt_type(0)
            .log2_max_pic_order_cnt_lsb_minus4(0)
    }

    /// [`base_sps`] at 64×64 with PPS 0.
    pub fn authored_sps_pps() -> (Rc<Sps>, Rc<Pps>) {
        let sps = base_sps().resolution(64, 64).build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        (sps, pps)
    }

    /// The SPS and PPS NALUs, the head of an IDR access unit.
    pub fn param_set_au(sps: &Sps, pps: &Pps) -> Vec<u8> {
        let mut au = Vec::new();
        Synthesizer::<'_, Sps, _>::synthesize(3, sps, &mut au, true).unwrap();
        Synthesizer::<'_, Pps, _>::synthesize(3, pps, &mut au, true).unwrap();
        au
    }

    /// One slice header. The planners read headers only, so no slice data
    /// follows the rbsp stop bit.
    #[derive(Debug, Clone, Copy)]
    pub struct SliceSpec<'a> {
        /// An IDR I slice; otherwise a P slice.
        pub idr: bool,
        pub ref_idc: u8,
        pub first_mb: u32,
        pub pps_id: u32,
        pub frame_num: u32,
        pub idr_pic_id: u32,
        pub poc_lsb: u32,
        /// The SPS is `pic_order_cnt_type` 2, which codes no `pic_order_cnt_lsb`.
        pub poc_type_2: bool,
        /// `delta_pic_order_cnt_bottom`: legal only when the PPS sets
        /// `bottom_field_pic_order_in_frame_present_flag`.
        pub bottom_delta: Option<i32>,
        pub num_ref_idx_l0_active: u32,
        /// `None` is sliding-window marking. `Some` is adaptive marking as raw
        /// ue(v) operations and arguments; the writer appends the closing 0.
        pub mmco: Option<&'a [u32]>,
    }

    impl Default for SliceSpec<'_> {
        fn default() -> Self {
            Self {
                idr: false,
                ref_idc: 1,
                first_mb: 0,
                pps_id: 0,
                frame_num: 0,
                idr_pic_id: 0,
                poc_lsb: 0,
                poc_type_2: false,
                bottom_delta: None,
                num_ref_idx_l0_active: 1,
                mmco: None,
            }
        }
    }

    pub fn write_slice(spec: &SliceSpec) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = NaluWriter::new(&mut buf, true);
            let nal_type = if spec.idr {
                NaluType::SliceIdr
            } else {
                NaluType::Slice
            };
            w.write_header(spec.ref_idc, nal_type as u8).unwrap();
            w.write_ue(spec.first_mb).unwrap();
            w.write_ue(if spec.idr { 2u32 } else { 0 }).unwrap(); // slice_type: I or P
            w.write_ue(spec.pps_id).unwrap();
            w.write_f(4, spec.frame_num).unwrap(); // frame_num, u(4)
            if spec.idr {
                w.write_ue(spec.idr_pic_id).unwrap();
            }
            if !spec.poc_type_2 {
                w.write_f(4, spec.poc_lsb).unwrap(); // pic_order_cnt_lsb, u(4)
            }
            if let Some(delta) = spec.bottom_delta {
                w.write_se(delta).unwrap();
            }
            if spec.idr {
                w.write_f(1, 0u32).unwrap(); // no_output_of_prior_pics_flag
                w.write_f(1, 0u32).unwrap(); // long_term_reference_flag
            } else {
                w.write_f(1, 1u32).unwrap(); // num_ref_idx_active_override_flag
                w.write_ue(spec.num_ref_idx_l0_active - 1).unwrap();
                w.write_f(1, 0u32).unwrap(); // ref_pic_list_modification_flag_l0
                if spec.ref_idc != 0 {
                    match spec.mmco {
                        None => w.write_f(1, 0u32).map(|_| ()).unwrap(),
                        Some(ops) => {
                            w.write_f(1, 1u32).unwrap(); // adaptive_ref_pic_marking_mode_flag
                            for value in ops {
                                w.write_ue(*value).unwrap();
                            }
                            w.write_ue(0u32).unwrap(); // end of the MMCO list
                        }
                    }
                }
            }
            w.write_se(0i32).unwrap(); // slice_qp_delta
            w.write_f(1, 1u32).unwrap(); // rbsp stop bit
            while !w.aligned() {
                w.write_f(1, 0u32).unwrap();
            }
        }
        buf
    }

    /// The IDR slice of [`authored_sps_pps`]'s stream: `frame_num`, POC and
    /// `idr_pic_id` 0.
    pub fn write_idr_slice() -> Vec<u8> {
        write_slice(&SliceSpec {
            idr: true,
            ref_idc: 3,
            ..Default::default()
        })
    }

    /// One P slice. `mmco_ops` pairs are `(operation, argument)`, for the
    /// one-argument operations 1, 2, 4 and 6.
    pub fn write_p_slice(
        frame_num: u32,
        poc_lsb: u32,
        ref_idc: u8,
        num_ref_idx_l0_active: u32,
        mmco_ops: Option<&[(u32, u32)]>,
    ) -> Vec<u8> {
        let flat: Option<Vec<u32>> =
            mmco_ops.map(|ops| ops.iter().flat_map(|&(op, arg)| [op, arg]).collect());
        write_slice(&SliceSpec {
            ref_idc,
            frame_num,
            poc_lsb,
            num_ref_idx_l0_active,
            mmco: flat.as_deref(),
            ..Default::default()
        })
    }
}
