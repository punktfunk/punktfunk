//! HEVC parameter sets and slice headers, written from one description.
//!
//! The counterpart of [`crate::enc_params`] for H.265. There is no synthesizer to
//! lean on, so the syntax is written here field by field — VPS, SPS with VUI, PPS,
//! the slice segment header with its inline reference picture set, and the two
//! HDR10 SEI messages — and read back by the client's own parser in the tests.
//!
//! The reference model is the RPS: every slice header lists the pictures the
//! decoder keeps, closest first, and marks the one this picture predicts from. A
//! recovery anchor is just a `used` mark on an older entry, and a distrusted
//! picture is simply not listed — the decoder drops it with no further syntax.

use cros_codecs::codec::h264::nalu_writer::NaluWriter;
use cros_codecs::codec::h264::nalu_writer::NaluWriterResult;

use crate::enc_h265::seq_fields;
use crate::enc_h265::vui_fields;
use crate::enc_h265::HevcFeatures;
use crate::enc_h265::VaEncSequenceParameterBufferHEVC;
use crate::enc_h265::NAL_IDR_W_RADL;
use crate::enc_h265::NAL_PPS;
use crate::enc_h265::NAL_PREFIX_SEI;
use crate::enc_h265::NAL_SPS;
use crate::enc_h265::NAL_TRAIL_R;
use crate::enc_h265::NAL_VPS;
use crate::enc_params::SessionParams;

/// `log2_max_pic_order_cnt_lsb_minus4`: eight bits of POC LSB, so a loss shorter
/// than 128 pictures keeps the decoder's MSB derivation on track.
const LOG2_MAX_POC_LSB_MINUS4: u8 = 4;

/// H.273 colour description for the VUI: primaries, transfer, matrix.
pub const COLOUR_BT709: [u8; 3] = [1, 1, 1];
pub const COLOUR_BT2020_PQ: [u8; 3] = [9, 16, 9];

/// What an HEVC session is: the shared facts, the depth, the colour tags, and what
/// the driver said it can do.
#[derive(Clone, Copy, Debug)]
pub struct HevcParams {
    pub common: SessionParams,
    /// Main10 with P010 surfaces; `false` is Main with NV12.
    pub ten_bit: bool,
    /// [`COLOUR_BT709`] or [`COLOUR_BT2020_PQ`]: what the VUI tells the decoder.
    pub colour: [u8; 3],
    pub features: HevcFeatures,
}

/// ST.2086 mastering display and CTA-861.3 content light level, as the two SEI
/// messages carry them. Chromaticities in 1/50000, luminance in 0.0001 cd/m².
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HdrStatic {
    /// G, B, R as (x, y).
    pub display_primaries: [[u16; 2]; 3],
    pub white_point: [u16; 2],
    pub max_display_mastering_luminance: u32,
    pub min_display_mastering_luminance: u32,
    pub max_cll: u16,
    pub max_fall: u16,
}

/// One picture's slice header facts.
#[derive(Clone, Copy, Debug)]
pub struct HevcSlice<'a> {
    pub is_idr: bool,
    /// Picture order count; 0 on an IDR, counting up from there.
    pub poc: i32,
    /// The pictures the decoder keeps after this one, closest first, each with
    /// whether this picture predicts from it. Empty on an IDR.
    pub rps: &'a [(i32, bool)],
}

impl HevcParams {
    /// Coded size: the picture padded to 16, which is a multiple of every minimum
    /// coding block both drivers offer, and what the surfaces are allocated at.
    pub fn coded_width(&self) -> u32 {
        self.common.width.div_ceil(16) * 16
    }

    pub fn coded_height(&self) -> u32 {
        self.common.height.div_ceil(16) * 16
    }

    /// Coding tree units in a picture: one slice covers them all.
    pub fn ctus_per_picture(&self) -> u32 {
        let ctb = self.features.ctb_size();
        self.coded_width().div_ceil(ctb) * self.coded_height().div_ceil(ctb)
    }

    /// `general_profile_idc`: 1 is Main, 2 is Main 10.
    pub fn profile_idc(&self) -> u8 {
        if self.ten_bit {
            2
        } else {
            1
        }
    }

    /// The smallest level (A.4) whose luma picture size and sample rate hold this
    /// stream: 4.1 for 1080p60, 5.1 for 4K60, 6.1 above.
    pub fn level_idc(&self) -> u8 {
        self.common.hevc_level_idc()
    }

    /// The VPS: one layer, one sub-layer, the profile and level, and the DPB size.
    pub fn vps(&self) -> Vec<u8> {
        nalu(NAL_VPS, |w| {
            w.write_f(4, 0u32)?; // vps_video_parameter_set_id
            w.write_f(1, 1u32)?; // vps_base_layer_internal_flag
            w.write_f(1, 1u32)?; // vps_base_layer_available_flag
            w.write_f(6, 0u32)?; // vps_max_layers_minus1
            w.write_f(3, 0u32)?; // vps_max_sub_layers_minus1
            w.write_f(1, 1u32)?; // vps_temporal_id_nesting_flag
            w.write_f(16, 0xffffu32)?; // vps_reserved_0xffff_16bits
            self.profile_tier_level(w)?;
            w.write_f(1, 0u32)?; // vps_sub_layer_ordering_info_present_flag
            self.ordering_info(w)?;
            w.write_f(6, 0u32)?; // vps_max_layer_id
            w.write_ue(0u32)?; // vps_num_layer_sets_minus1
            w.write_f(1, 0u32)?; // vps_timing_info_present_flag
            w.write_f(1, 0u32).map(|_| ()) // vps_extension_flag
        })
    }

    /// The SPS: the size and its conformance window, the depth, the block sizes the
    /// driver dictated, the tools it offers, and a VUI that names the colour, the
    /// frame rate and a zero reorder depth.
    pub fn sps(&self) -> Vec<u8> {
        nalu(NAL_SPS, |w| {
            let f = &self.features;
            w.write_f(4, 0u32)?; // sps_video_parameter_set_id
            w.write_f(3, 0u32)?; // sps_max_sub_layers_minus1
            w.write_f(1, 1u32)?; // sps_temporal_id_nesting_flag
            self.profile_tier_level(w)?;
            w.write_ue(0u32)?; // sps_seq_parameter_set_id
            w.write_ue(1u32)?; // chroma_format_idc: 4:2:0
            w.write_ue(self.coded_width())?; // pic_width_in_luma_samples
            w.write_ue(self.coded_height())?; // pic_height_in_luma_samples
            let (crop_right, crop_bottom) = (
                (self.coded_width() - self.common.width) / 2,
                (self.coded_height() - self.common.height) / 2,
            );
            let cropped = crop_right != 0 || crop_bottom != 0;
            w.write_f(1, u32::from(cropped))?; // conformance_window_flag
            if cropped {
                w.write_ue(0u32)?; // conf_win_left_offset
                w.write_ue(crop_right)?;
                w.write_ue(0u32)?; // conf_win_top_offset
                w.write_ue(crop_bottom)?;
            }
            let depth = u32::from(self.ten_bit) * 2;
            w.write_ue(depth)?; // bit_depth_luma_minus8
            w.write_ue(depth)?; // bit_depth_chroma_minus8
            w.write_ue(u32::from(LOG2_MAX_POC_LSB_MINUS4))?;
            w.write_f(1, 0u32)?; // sps_sub_layer_ordering_info_present_flag
            self.ordering_info(w)?;
            w.write_ue(u32::from(f.log2_min_cb_minus3))?;
            w.write_ue(u32::from(f.log2_ctb_minus3 - f.log2_min_cb_minus3))?;
            w.write_ue(u32::from(f.log2_min_tb_minus2))?;
            w.write_ue(u32::from(f.log2_max_tb_minus2 - f.log2_min_tb_minus2))?;
            w.write_ue(u32::from(f.max_transform_hierarchy_depth_inter))?;
            w.write_ue(u32::from(f.max_transform_hierarchy_depth_intra))?;
            w.write_f(1, 0u32)?; // scaling_list_enabled_flag
            w.write_f(1, u32::from(f.amp))?; // amp_enabled_flag
            w.write_f(1, u32::from(f.sao))?; // sample_adaptive_offset_enabled_flag
            w.write_f(1, 0u32)?; // pcm_enabled_flag
            w.write_ue(0u32)?; // num_short_term_ref_pic_sets: every slice carries its own
            w.write_f(1, 0u32)?; // long_term_ref_pics_present_flag
            w.write_f(1, u32::from(f.temporal_mvp))?; // sps_temporal_mvp_enabled_flag
            w.write_f(1, u32::from(f.strong_intra_smoothing))?;
            w.write_f(1, 1u32)?; // vui_parameters_present_flag
            self.vui(w)?;
            w.write_f(1, 0u32).map(|_| ()) // sps_extension_present_flag
        })
    }

    /// The PPS: the initial QP, per-CU QP for the rate controller where the driver
    /// has it, deblocking on with control present, and nothing the session never
    /// uses (tiles, WPP, weighted prediction, list modification).
    pub fn pps(&self) -> Vec<u8> {
        nalu(NAL_PPS, |w| {
            let f = &self.features;
            w.write_ue(0u32)?; // pps_pic_parameter_set_id
            w.write_ue(0u32)?; // pps_seq_parameter_set_id
            w.write_f(1, 0u32)?; // dependent_slice_segments_enabled_flag
            w.write_f(1, 0u32)?; // output_flag_present_flag
            w.write_f(3, 0u32)?; // num_extra_slice_header_bits
            w.write_f(1, 0u32)?; // sign_data_hiding_enabled_flag
            w.write_f(1, 0u32)?; // cabac_init_present_flag
            w.write_ue(0u32)?; // num_ref_idx_l0_default_active_minus1
            w.write_ue(0u32)?; // num_ref_idx_l1_default_active_minus1
            w.write_se(i32::from(self.common.initial_qp) - 26)?; // init_qp_minus26
            w.write_f(1, 0u32)?; // constrained_intra_pred_flag
            w.write_f(1, u32::from(f.transform_skip))?; // transform_skip_enabled_flag
            w.write_f(1, u32::from(f.cu_qp_delta))?; // cu_qp_delta_enabled_flag
            if f.cu_qp_delta {
                w.write_ue(u32::from(self.diff_cu_qp_delta_depth()))?;
            }
            w.write_se(0i32)?; // pps_cb_qp_offset
            w.write_se(0i32)?; // pps_cr_qp_offset
            w.write_f(1, 0u32)?; // pps_slice_chroma_qp_offsets_present_flag
            w.write_f(1, 0u32)?; // weighted_pred_flag
            w.write_f(1, 0u32)?; // weighted_bipred_flag
            w.write_f(1, 0u32)?; // transquant_bypass_enabled_flag
            w.write_f(1, 0u32)?; // tiles_enabled_flag
            w.write_f(1, 0u32)?; // entropy_coding_sync_enabled_flag
            w.write_f(1, 1u32)?; // pps_loop_filter_across_slices_enabled_flag
            w.write_f(1, 1u32)?; // deblocking_filter_control_present_flag
            w.write_f(1, 0u32)?; // deblocking_filter_override_enabled_flag
            w.write_f(1, 0u32)?; // pps_deblocking_filter_disabled_flag
            w.write_se(0i32)?; // pps_beta_offset_div2
            w.write_se(0i32)?; // pps_tc_offset_div2
            w.write_f(1, 0u32)?; // pps_scaling_list_data_present_flag
            w.write_f(1, 0u32)?; // lists_modification_present_flag
            w.write_ue(0u32)?; // log2_parallel_merge_level_minus2
            w.write_f(1, 0u32)?; // slice_segment_header_extension_present_flag
            w.write_f(1, 0u32).map(|_| ()) // pps_extension_present_flag
        })
    }

    /// `diff_cu_qp_delta_depth`: the whole CTB depth, so the rate controller may
    /// set a QP per coding block. Zero would make `cu_qp_delta` a no-op.
    pub fn diff_cu_qp_delta_depth(&self) -> u8 {
        self.features.log2_ctb_minus3 - self.features.log2_min_cb_minus3
    }

    /// One picture's slice segment header, whole picture, ending on the
    /// `byte_alignment()` the syntax itself requires — so its bit length is exactly
    /// its byte length times eight, for both drivers.
    pub fn slice_header(&self, slice: HevcSlice) -> Vec<u8> {
        let f = &self.features;
        let nal_type = if slice.is_idr {
            NAL_IDR_W_RADL
        } else {
            NAL_TRAIL_R
        };
        nalu(nal_type, |w| {
            w.write_f(1, 1u32)?; // first_slice_segment_in_pic_flag
            if slice.is_idr {
                w.write_f(1, 0u32)?; // no_output_of_prior_pics_flag
            }
            w.write_ue(0u32)?; // slice_pic_parameter_set_id
                               // slice_type: 2 = I, 1 = P, 0 = B — a generalised-B P picture is a B
                               // slice whose L1 is L0 again.
            let b_slice = !slice.is_idr && f.gpb;
            w.write_ue(if slice.is_idr {
                2u32
            } else if b_slice {
                0
            } else {
                1
            })?;
            if !slice.is_idr {
                let lsb_bits = usize::from(LOG2_MAX_POC_LSB_MINUS4) + 4;
                w.write_f(lsb_bits, (slice.poc as u32) & ((1 << lsb_bits) - 1))?;
                w.write_f(1, 0u32)?; // short_term_ref_pic_set_sps_flag: inline
                                     // st_ref_pic_set(0): no prediction from an SPS set, all negative.
                w.write_ue(slice.rps.len() as u32)?; // num_negative_pics
                w.write_ue(0u32)?; // num_positive_pics
                let mut prev = slice.poc;
                for &(poc, used) in slice.rps {
                    w.write_ue((prev - poc - 1) as u32)?; // delta_poc_s0_minus1
                    w.write_f(1, u32::from(used))?; // used_by_curr_pic_s0_flag
                    prev = poc;
                }
                if f.temporal_mvp {
                    w.write_f(1, 1u32)?; // slice_temporal_mvp_enabled_flag
                }
            }
            if f.sao {
                w.write_f(1, 1u32)?; // slice_sao_luma_flag
                w.write_f(1, 1u32)?; // slice_sao_chroma_flag
            }
            if !slice.is_idr {
                w.write_f(1, 1u32)?; // num_ref_idx_active_override_flag
                w.write_ue(0u32)?; // num_ref_idx_l0_active_minus1: one reference
                if b_slice {
                    w.write_ue(0u32)?; // num_ref_idx_l1_active_minus1: the same one
                    w.write_f(1, 0u32)?; // mvd_l1_zero_flag
                    if f.temporal_mvp {
                        w.write_f(1, 1u32)?; // collocated_from_l0_flag
                    }
                }
                w.write_ue(0u32)?; // five_minus_max_num_merge_cand
            }
            w.write_se(0i32)?; // slice_qp_delta
            w.write_f(1, 1u32).map(|_| ()) // slice_loop_filter_across_slices_enabled_flag
        })
    }

    /// The HDR10 static metadata as one prefix SEI NAL: mastering display colour
    /// volume (137) and content light level (144). Sent with every IDR.
    pub fn hdr_sei(&self, hdr: &HdrStatic) -> Vec<u8> {
        nalu(NAL_PREFIX_SEI, |w| {
            w.write_f(8, 137u32)?; // payloadType
            w.write_f(8, 24u32)?; // payloadSize
            for [x, y] in hdr.display_primaries {
                w.write_f(16, u32::from(x))?;
                w.write_f(16, u32::from(y))?;
            }
            w.write_f(16, u32::from(hdr.white_point[0]))?;
            w.write_f(16, u32::from(hdr.white_point[1]))?;
            w.write_f(32, hdr.max_display_mastering_luminance)?;
            w.write_f(32, hdr.min_display_mastering_luminance)?;
            w.write_f(8, 144u32)?; // payloadType
            w.write_f(8, 4u32)?; // payloadSize
            w.write_f(16, u32::from(hdr.max_cll))?;
            w.write_f(16, u32::from(hdr.max_fall)).map(|_| ())
        })
    }

    /// The same facts in the shape the driver reads.
    pub fn va_sequence(&self) -> VaEncSequenceParameterBufferHEVC {
        let f = &self.features;
        VaEncSequenceParameterBufferHEVC {
            general_profile_idc: self.profile_idc(),
            general_level_idc: self.level_idc(),
            general_tier_flag: 0,
            // Not 0: VDEnc's HuC rate control budgets a GOP from this and fails
            // the first picture on an empty one. Picture types stay ours — the
            // driver never inserts an IDR of its own.
            intra_period: u32::from(u16::MAX),
            intra_idr_period: u32::from(u16::MAX),
            ip_period: 1,
            bits_per_second: self.common.bitrate_bps,
            pic_width_in_luma_samples: self.coded_width() as u16,
            pic_height_in_luma_samples: self.coded_height() as u16,
            seq_fields: seq_fields(u8::from(self.ten_bit) * 2, f),
            log2_min_luma_coding_block_size_minus3: f.log2_min_cb_minus3,
            log2_diff_max_min_luma_coding_block_size: f.log2_ctb_minus3 - f.log2_min_cb_minus3,
            log2_min_transform_block_size_minus2: f.log2_min_tb_minus2,
            log2_diff_max_min_transform_block_size: f.log2_max_tb_minus2 - f.log2_min_tb_minus2,
            max_transform_hierarchy_depth_inter: f.max_transform_hierarchy_depth_inter,
            max_transform_hierarchy_depth_intra: f.max_transform_hierarchy_depth_intra,
            vui_parameters_present_flag: 1,
            vui_fields: vui_fields(),
            vui_num_units_in_tick: self.common.fps_den,
            vui_time_scale: self.common.fps_num,
            ..Default::default()
        }
    }

    fn profile_tier_level(&self, w: &mut Writer<'_>) -> NaluWriterResult<()> {
        w.write_f(2, 0u32)?; // general_profile_space
        w.write_f(1, 0u32)?; // general_tier_flag
        w.write_f(5, u32::from(self.profile_idc()))?;
        // general_profile_compatibility_flag[0..32], flag j at bit 31 - j. A Main
        // stream is also Main 10 compatible.
        let compatible: u32 = if self.ten_bit {
            1 << (31 - 2)
        } else {
            1 << (31 - 1) | 1 << (31 - 2)
        };
        w.write_f(32, compatible)?;
        w.write_f(1, 1u32)?; // general_progressive_source_flag
        w.write_f(1, 0u32)?; // general_interlaced_source_flag
        w.write_f(1, 0u32)?; // general_non_packed_constraint_flag
        w.write_f(1, 1u32)?; // general_frame_only_constraint_flag
        w.write_f(32, 0u32)?; // general_reserved_zero_43bits, first 32
        w.write_f(11, 0u32)?; // and the rest
        w.write_f(1, 0u32)?; // general_inbld_flag
        w.write_f(8, u32::from(self.level_idc())).map(|_| ())
    }

    /// `*_max_dec_pic_buffering_minus1`, `max_num_reorder_pics`,
    /// `max_latency_increase_plus1`: the slots plus this picture, no reordering.
    fn ordering_info(&self, w: &mut Writer<'_>) -> NaluWriterResult<()> {
        w.write_ue(u32::from(self.common.slots.max(1)))?;
        w.write_ue(0u32)?;
        w.write_ue(0u32)
    }

    fn vui(&self, w: &mut Writer<'_>) -> NaluWriterResult<()> {
        w.write_f(1, 0u32)?; // aspect_ratio_info_present_flag
        w.write_f(1, 0u32)?; // overscan_info_present_flag
        w.write_f(1, 1u32)?; // video_signal_type_present_flag
        w.write_f(3, 5u32)?; // video_format: unspecified
        w.write_f(1, 0u32)?; // video_full_range_flag
        w.write_f(1, 1u32)?; // colour_description_present_flag
        for tag in self.colour {
            w.write_f(8, u32::from(tag))?;
        }
        w.write_f(1, 0u32)?; // chroma_loc_info_present_flag
        w.write_f(1, 0u32)?; // neutral_chroma_indication_flag
        w.write_f(1, 0u32)?; // field_seq_flag
        w.write_f(1, 0u32)?; // frame_field_info_present_flag
        w.write_f(1, 0u32)?; // default_display_window_flag
        w.write_f(1, 1u32)?; // vui_timing_info_present_flag
        w.write_f(32, self.common.fps_den)?; // vui_num_units_in_tick
        w.write_f(32, self.common.fps_num)?; // vui_time_scale
        w.write_f(1, 0u32)?; // vui_poc_proportional_to_timing_flag
        w.write_f(1, 0u32)?; // vui_hrd_parameters_present_flag
        w.write_f(1, 1u32)?; // bitstream_restriction_flag
        w.write_f(1, 0u32)?; // tiles_fixed_structure_flag
        w.write_f(1, 1u32)?; // motion_vectors_over_pic_boundaries_flag
        w.write_f(1, 0u32)?; // restricted_ref_pic_lists_flag
        w.write_ue(0u32)?; // min_spatial_segmentation_idc
        w.write_ue(0u32)?; // max_bytes_per_pic_denom
        w.write_ue(0u32)?; // max_bits_per_min_cu_denom
        w.write_ue(15u32)?; // log2_max_mv_length_horizontal
        w.write_ue(15u32) // log2_max_mv_length_vertical
    }
}

type Writer<'a> = NaluWriter<&'a mut Vec<u8>>;

/// One annex-B NAL unit: start code, the two-byte header (layer 0, temporal id 0),
/// then `body` with emulation prevention, then `rbsp_trailing_bits`.
fn nalu(nal_type: u8, body: impl FnOnce(&mut Writer<'_>) -> NaluWriterResult<()>) -> Vec<u8> {
    let mut buf = vec![0, 0, 0, 1, nal_type << 1, 1];
    {
        let mut w = NaluWriter::new(&mut buf, true);
        body(&mut w).expect("writing to a Vec cannot fail, and every value fits its field");
        w.write_f(1, 1u32).expect("stop bit");
        while !w.aligned() {
            w.write_f(1, 0u32).expect("alignment");
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use cros_codecs::codec::h265::parser::Nalu;
    use cros_codecs::codec::h265::parser::Parser;
    use cros_codecs::codec::h265::parser::SliceHeader;
    use cros_codecs::codec::h265::parser::SliceType;

    use super::*;

    fn params(ten_bit: bool) -> HevcParams {
        HevcParams {
            common: SessionParams {
                width: 1920,
                height: 1080,
                fps_num: 60,
                fps_den: 1,
                bitrate_bps: 20_000_000,
                slots: 4,
                max_num_reorder_frames: 0,
                initial_qp: 30,
                vbv_frames: 1.0,
            },
            ten_bit,
            colour: if ten_bit {
                COLOUR_BT2020_PQ
            } else {
                COLOUR_BT709
            },
            // Intel's word: AMP, SAO, TMVP, depth 2, 64/8/4..32.
            features: HevcFeatures::from_attributes(0x0190_0464, 0x88c7),
        }
    }

    /// A parser with our parameter sets active, and the slice it read back.
    fn parse(p: &HevcParams, slice: &[u8]) -> SliceHeader {
        let mut parser = Parser::default();
        for bytes in [p.vps(), p.sps(), p.pps()] {
            let nalu = Nalu::next(&mut Cursor::new(&bytes[..])).unwrap();
            match bytes[4] >> 1 {
                NAL_VPS => {
                    parser.parse_vps(&nalu).unwrap();
                }
                NAL_SPS => {
                    parser.parse_sps(&nalu).unwrap();
                }
                _ => {
                    parser.parse_pps(&nalu).unwrap();
                }
            }
        }
        let nalu = Nalu::next(&mut Cursor::new(slice)).unwrap();
        parser.parse_slice_header(nalu).unwrap().header
    }

    /// The parameter sets, read back by the client's parser: the size survives the
    /// conformance window, the driver's block sizes and tools land, the VUI names
    /// the colour and the frame rate, and the DPB holds the slots plus one.
    #[test]
    fn the_parameter_sets_round_trip_through_the_parser() {
        let p = params(false);
        let mut parser = Parser::default();
        let vps = p.vps();
        parser
            .parse_vps(&Nalu::next(&mut Cursor::new(&vps[..])).unwrap())
            .unwrap();
        let sps_bytes = p.sps();
        let sps = parser
            .parse_sps(&Nalu::next(&mut Cursor::new(&sps_bytes[..])).unwrap())
            .unwrap()
            .clone();
        assert_eq!(sps.pic_width_in_luma_samples, 1920);
        assert_eq!(sps.pic_height_in_luma_samples, 1088);
        assert!(sps.conformance_window_flag);
        assert_eq!(sps.conf_win_bottom_offset, 4, "8 rows, in chroma units");
        assert_eq!(sps.bit_depth_luma_minus8, 0);
        assert_eq!(
            sps.log2_max_pic_order_cnt_lsb_minus4,
            LOG2_MAX_POC_LSB_MINUS4
        );
        assert_eq!(sps.max_dec_pic_buffering_minus1[0], 4);
        assert_eq!(sps.max_num_reorder_pics[0], 0);
        assert_eq!(sps.ctb_size_y, 64);
        assert_eq!(sps.min_cb_log2_size_y, 3);
        assert_eq!(sps.max_transform_hierarchy_depth_inter, 2);
        assert!(sps.amp_enabled_flag && sps.sample_adaptive_offset_enabled_flag);
        assert!(sps.temporal_mvp_enabled_flag);
        assert!(!sps.strong_intra_smoothing_enabled_flag);
        assert_eq!(sps.num_short_term_ref_pic_sets, 0);
        assert!(sps.vui_parameters_present_flag);
        let vui = &sps.vui_parameters;
        assert!(vui.video_signal_type_present_flag && vui.colour_description_present_flag);
        assert_eq!(
            (
                vui.colour_primaries,
                vui.transfer_characteristics,
                vui.matrix_coeffs
            ),
            (1, 1, 1)
        );
        let pps_bytes = p.pps();
        let pps = parser
            .parse_pps(&Nalu::next(&mut Cursor::new(&pps_bytes[..])).unwrap())
            .unwrap();
        assert_eq!(pps.init_qp_minus26, 4);
        assert!(pps.cu_qp_delta_enabled_flag && pps.transform_skip_enabled_flag);
        assert_eq!(pps.diff_cu_qp_delta_depth, 3);
        assert!(pps.deblocking_filter_control_present_flag);
        assert!(pps.loop_filter_across_slices_enabled_flag);
        assert_eq!(p.level_idc(), 123, "1080p60 is level 4.1");
        assert_eq!(p.ctus_per_picture(), 30 * 17);

        let ten = params(true);
        let sps_bytes = ten.sps();
        let mut parser = Parser::default();
        let vps = ten.vps();
        parser
            .parse_vps(&Nalu::next(&mut Cursor::new(&vps[..])).unwrap())
            .unwrap();
        let sps = parser
            .parse_sps(&Nalu::next(&mut Cursor::new(&sps_bytes[..])).unwrap())
            .unwrap();
        assert_eq!(sps.bit_depth_luma_minus8, 2);
        assert_eq!(sps.vui_parameters.transfer_characteristics, 16, "PQ");
    }

    /// The slice headers: an I slice for the IDR, and a P slice whose inline RPS keeps
    /// three pictures and predicts from the oldest — a recovery anchor's shape.
    #[test]
    fn slice_headers_carry_the_rps_the_decoder_keeps() {
        let p = params(false);
        let idr = p.slice_header(HevcSlice {
            is_idr: true,
            poc: 0,
            rps: &[],
        });
        assert_eq!(idr[4] >> 1, NAL_IDR_W_RADL);
        let header = parse(&p, &idr);
        assert!(header.first_slice_segment_in_pic_flag);
        assert_eq!(header.type_, SliceType::I);
        assert!(header.sao_luma_flag && header.sao_chroma_flag);

        let recovery = p.slice_header(HevcSlice {
            is_idr: false,
            poc: 10,
            rps: &[(7, true), (6, false), (5, false)],
        });
        assert_eq!(recovery[4] >> 1, NAL_TRAIL_R);
        let header = parse(&p, &recovery);
        assert_eq!(header.type_, SliceType::P);
        assert_eq!(header.pic_order_cnt_lsb, 10);
        assert!(!header.short_term_ref_pic_set_sps_flag);
        let rps = &header.short_term_ref_pic_set;
        assert_eq!(rps.num_negative_pics, 3);
        assert_eq!(rps.num_positive_pics, 0);
        assert_eq!(&rps.delta_poc_s0[..3], &[-3, -4, -5]);
        assert_eq!(&rps.used_by_curr_pic_s0[..3], &[true, false, false]);
        assert!(header.temporal_mvp_enabled_flag);
        assert!(header.num_ref_idx_active_override_flag);
        assert_eq!(header.num_ref_idx_l0_active_minus1, 0);
        assert_eq!(header.five_minus_max_num_merge_cand, 0);
        assert!(header.loop_filter_across_slices_enabled_flag);

        // Intel's shape: the same picture as a B slice whose L1 repeats L0.
        let mut gpb = p;
        gpb.features.gpb = true;
        let recovery = gpb.slice_header(HevcSlice {
            is_idr: false,
            poc: 10,
            rps: &[(7, true), (6, false), (5, false)],
        });
        let header = parse(&gpb, &recovery);
        assert_eq!(header.type_, SliceType::B);
        assert_eq!(header.num_ref_idx_l0_active_minus1, 0);
        assert_eq!(header.num_ref_idx_l1_active_minus1, 0);
        assert!(!header.mvd_l1_zero_flag);
        assert!(header.collocated_from_l0_flag);
        assert_eq!(
            &header.short_term_ref_pic_set.delta_poc_s0[..3],
            &[-3, -4, -5]
        );
    }

    /// Two SEI messages in one NAL, 24 and 4 bytes of payload. The luminance
    /// fields hold zero bytes, so emulation prevention is exercised and undone.
    #[test]
    fn the_hdr_sei_is_two_messages_in_one_nal() {
        let sei = params(true).hdr_sei(&HdrStatic {
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 50,
            max_cll: 1000,
            max_fall: 400,
        });
        assert_eq!(sei[4] >> 1, NAL_PREFIX_SEI);
        let mut rbsp = Vec::new();
        let mut zeros = 0;
        for &b in &sei[6..] {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            rbsp.push(b);
        }
        assert!(rbsp.len() < sei.len() - 6, "an emulation byte was needed");
        assert_eq!(&rbsp[..2], &[137, 24]);
        assert_eq!(&rbsp[2..6], &[0x21, 0x34, 0x9b, 0xaa], "G (8500, 39850)");
        assert_eq!(&rbsp[18..22], &[0, 0x98, 0x96, 0x80], "10 000 000");
        assert_eq!(&rbsp[22..26], &[0, 0, 0, 50]);
        assert_eq!(&rbsp[26..32], &[144, 4, 0x03, 0xe8, 0x01, 0x90]);
        assert_eq!(rbsp[32], 0x80, "trailing bits");
        assert_eq!(rbsp.len(), 33);
    }
}
