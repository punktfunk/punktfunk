/*
 * Layout probe for the hand-declared libva structures in `src/va.rs`,
 * `src/va_h265.rs` and `src/va_av1.rs`.
 *
 * Those modules declare VAAPI's decode buffers as `#[repr(C)]` Rust structs, because
 * this crate must build on macOS and in the Linux container where libva headers need
 * not exist. This file is how those declarations were CHECKED rather than eyeballed,
 * and it is committed so the check is reproducible instead of a claim in a commit
 * message.
 *
 * Run it against real headers (no Linux box required):
 *
 *   docker run --rm --platform linux/amd64 -v "$PWD/crates/codec/pf-vaapi:/w" -w /w \
 *     pf-lxcheck2 bash -lc 'apt-get update -qq && apt-get install -y -qq libva-dev \
 *       && gcc -w -O0 layout-probe.c -o /tmp/probe && /tmp/probe'
 *
 * Every number it prints is pinned as a `const` assertion at the bottom of
 * `src/va.rs`, so a transcription mistake is a compile error. The bit-field section
 * exists because C bit-field allocation order is ABI-defined, not standardised:
 * it PROVES least-significant-bit-first on this ABI rather than assuming it.
 *
 * Or on a box with libva-dev (`.50`): `gcc -w -O0 layout-probe.c -o probe && ./probe`.
 *
 * Last run against libva 2.23.0-1ubuntu1, x86_64-linux-gnu; the VideoProc section
 * (`src/vpp.rs`) against libva 2.22 on `.50`.
 */
#include <stdio.h>
#include <stddef.h>
#include <va/va.h>
#include <va/va_dec_hevc.h>
#include <va/va_dec_av1.h>
#include <va/va_drmcommon.h>
#include <va/va_vpp.h>

#define S(t)        printf("size %-34s %zu align %zu\n", #t, sizeof(t), _Alignof(t))
#define O(t, f)     printf("off  %-20s %-28s %zu\n", #t, #f, offsetof(t, f))

int main(void) {
    S(VAPictureH264);
    O(VAPictureH264, picture_id);
    O(VAPictureH264, frame_idx);
    O(VAPictureH264, flags);
    O(VAPictureH264, TopFieldOrderCnt);
    O(VAPictureH264, BottomFieldOrderCnt);
    O(VAPictureH264, va_reserved);

    S(VAPictureParameterBufferH264);
    O(VAPictureParameterBufferH264, CurrPic);
    O(VAPictureParameterBufferH264, ReferenceFrames);
    O(VAPictureParameterBufferH264, picture_width_in_mbs_minus1);
    O(VAPictureParameterBufferH264, picture_height_in_mbs_minus1);
    O(VAPictureParameterBufferH264, bit_depth_luma_minus8);
    O(VAPictureParameterBufferH264, bit_depth_chroma_minus8);
    O(VAPictureParameterBufferH264, num_ref_frames);
    O(VAPictureParameterBufferH264, seq_fields);
    O(VAPictureParameterBufferH264, num_slice_groups_minus1);
    O(VAPictureParameterBufferH264, slice_group_map_type);
    O(VAPictureParameterBufferH264, slice_group_change_rate_minus1);
    O(VAPictureParameterBufferH264, pic_init_qp_minus26);
    O(VAPictureParameterBufferH264, pic_init_qs_minus26);
    O(VAPictureParameterBufferH264, chroma_qp_index_offset);
    O(VAPictureParameterBufferH264, second_chroma_qp_index_offset);
    O(VAPictureParameterBufferH264, pic_fields);
    O(VAPictureParameterBufferH264, frame_num);
    O(VAPictureParameterBufferH264, va_reserved);

    S(VAIQMatrixBufferH264);
    O(VAIQMatrixBufferH264, ScalingList4x4);
    O(VAIQMatrixBufferH264, ScalingList8x8);
    O(VAIQMatrixBufferH264, va_reserved);

    S(VASliceParameterBufferH264);
    O(VASliceParameterBufferH264, slice_data_size);
    O(VASliceParameterBufferH264, slice_data_offset);
    O(VASliceParameterBufferH264, slice_data_flag);
    O(VASliceParameterBufferH264, slice_data_bit_offset);
    O(VASliceParameterBufferH264, first_mb_in_slice);
    O(VASliceParameterBufferH264, slice_type);
    O(VASliceParameterBufferH264, direct_spatial_mv_pred_flag);
    O(VASliceParameterBufferH264, num_ref_idx_l0_active_minus1);
    O(VASliceParameterBufferH264, num_ref_idx_l1_active_minus1);
    O(VASliceParameterBufferH264, cabac_init_idc);
    O(VASliceParameterBufferH264, slice_qp_delta);
    O(VASliceParameterBufferH264, disable_deblocking_filter_idc);
    O(VASliceParameterBufferH264, slice_alpha_c0_offset_div2);
    O(VASliceParameterBufferH264, slice_beta_offset_div2);
    O(VASliceParameterBufferH264, RefPicList0);
    O(VASliceParameterBufferH264, RefPicList1);
    O(VASliceParameterBufferH264, luma_log2_weight_denom);
    O(VASliceParameterBufferH264, chroma_log2_weight_denom);
    O(VASliceParameterBufferH264, luma_weight_l0_flag);
    O(VASliceParameterBufferH264, luma_weight_l0);
    O(VASliceParameterBufferH264, luma_offset_l0);
    O(VASliceParameterBufferH264, chroma_weight_l0_flag);
    O(VASliceParameterBufferH264, chroma_weight_l0);
    O(VASliceParameterBufferH264, chroma_offset_l0);
    O(VASliceParameterBufferH264, luma_weight_l1_flag);
    O(VASliceParameterBufferH264, luma_weight_l1);
    O(VASliceParameterBufferH264, luma_offset_l1);
    O(VASliceParameterBufferH264, chroma_weight_l1_flag);
    O(VASliceParameterBufferH264, chroma_weight_l1);
    O(VASliceParameterBufferH264, chroma_offset_l1);
    O(VASliceParameterBufferH264, va_reserved);

    /* Bit-field allocation order: prove LSB-first rather than assume it. */
    {
        VAPictureParameterBufferH264 p;
        p.seq_fields.value = 0;
        p.seq_fields.bits.chroma_format_idc = 3;
        printf("bits seq_fields.chroma_format_idc=3 -> value 0x%08x\n", p.seq_fields.value);
        p.seq_fields.value = 0;
        p.seq_fields.bits.log2_max_frame_num_minus4 = 0xf;
        printf("bits seq_fields.log2_max_frame_num_minus4=0xf -> value 0x%08x\n", p.seq_fields.value);
        p.pic_fields.value = 0;
        p.pic_fields.bits.reference_pic_flag = 1;
        printf("bits pic_fields.reference_pic_flag=1 -> value 0x%08x\n", p.pic_fields.value);
        p.pic_fields.value = 0;
        p.pic_fields.bits.weighted_bipred_idc = 3;
        printf("bits pic_fields.weighted_bipred_idc=3 -> value 0x%08x\n", p.pic_fields.value);
    }

    /* ---- HEVC (va_dec_hevc.h) ---- */
    S(VAPictureHEVC);
    O(VAPictureHEVC, picture_id);
    O(VAPictureHEVC, pic_order_cnt);
    O(VAPictureHEVC, flags);
    O(VAPictureHEVC, va_reserved);

    S(VAPictureParameterBufferHEVC);
    O(VAPictureParameterBufferHEVC, CurrPic);
    O(VAPictureParameterBufferHEVC, ReferenceFrames);
    O(VAPictureParameterBufferHEVC, pic_width_in_luma_samples);
    O(VAPictureParameterBufferHEVC, pic_height_in_luma_samples);
    O(VAPictureParameterBufferHEVC, pic_fields);
    O(VAPictureParameterBufferHEVC, sps_max_dec_pic_buffering_minus1);
    O(VAPictureParameterBufferHEVC, bit_depth_luma_minus8);
    O(VAPictureParameterBufferHEVC, bit_depth_chroma_minus8);
    O(VAPictureParameterBufferHEVC, pcm_sample_bit_depth_luma_minus1);
    O(VAPictureParameterBufferHEVC, pcm_sample_bit_depth_chroma_minus1);
    O(VAPictureParameterBufferHEVC, log2_min_luma_coding_block_size_minus3);
    O(VAPictureParameterBufferHEVC, log2_diff_max_min_luma_coding_block_size);
    O(VAPictureParameterBufferHEVC, log2_min_transform_block_size_minus2);
    O(VAPictureParameterBufferHEVC, log2_diff_max_min_transform_block_size);
    O(VAPictureParameterBufferHEVC, log2_min_pcm_luma_coding_block_size_minus3);
    O(VAPictureParameterBufferHEVC, log2_diff_max_min_pcm_luma_coding_block_size);
    O(VAPictureParameterBufferHEVC, max_transform_hierarchy_depth_intra);
    O(VAPictureParameterBufferHEVC, max_transform_hierarchy_depth_inter);
    O(VAPictureParameterBufferHEVC, init_qp_minus26);
    O(VAPictureParameterBufferHEVC, diff_cu_qp_delta_depth);
    O(VAPictureParameterBufferHEVC, pps_cb_qp_offset);
    O(VAPictureParameterBufferHEVC, pps_cr_qp_offset);
    O(VAPictureParameterBufferHEVC, log2_parallel_merge_level_minus2);
    O(VAPictureParameterBufferHEVC, num_tile_columns_minus1);
    O(VAPictureParameterBufferHEVC, num_tile_rows_minus1);
    O(VAPictureParameterBufferHEVC, column_width_minus1);
    O(VAPictureParameterBufferHEVC, row_height_minus1);
    O(VAPictureParameterBufferHEVC, slice_parsing_fields);
    O(VAPictureParameterBufferHEVC, log2_max_pic_order_cnt_lsb_minus4);
    O(VAPictureParameterBufferHEVC, num_short_term_ref_pic_sets);
    O(VAPictureParameterBufferHEVC, num_long_term_ref_pic_sps);
    O(VAPictureParameterBufferHEVC, num_ref_idx_l0_default_active_minus1);
    O(VAPictureParameterBufferHEVC, num_ref_idx_l1_default_active_minus1);
    O(VAPictureParameterBufferHEVC, pps_beta_offset_div2);
    O(VAPictureParameterBufferHEVC, pps_tc_offset_div2);
    O(VAPictureParameterBufferHEVC, num_extra_slice_header_bits);
    O(VAPictureParameterBufferHEVC, st_rps_bits);
    O(VAPictureParameterBufferHEVC, va_reserved);

    S(VASliceParameterBufferHEVC);
    O(VASliceParameterBufferHEVC, slice_data_size);
    O(VASliceParameterBufferHEVC, slice_data_offset);
    O(VASliceParameterBufferHEVC, slice_data_flag);
    O(VASliceParameterBufferHEVC, slice_data_byte_offset);
    O(VASliceParameterBufferHEVC, slice_segment_address);
    O(VASliceParameterBufferHEVC, RefPicList);
    O(VASliceParameterBufferHEVC, LongSliceFlags);
    O(VASliceParameterBufferHEVC, collocated_ref_idx);
    O(VASliceParameterBufferHEVC, num_ref_idx_l0_active_minus1);
    O(VASliceParameterBufferHEVC, num_ref_idx_l1_active_minus1);
    O(VASliceParameterBufferHEVC, slice_qp_delta);
    O(VASliceParameterBufferHEVC, slice_cb_qp_offset);
    O(VASliceParameterBufferHEVC, slice_cr_qp_offset);
    O(VASliceParameterBufferHEVC, slice_beta_offset_div2);
    O(VASliceParameterBufferHEVC, slice_tc_offset_div2);
    O(VASliceParameterBufferHEVC, luma_log2_weight_denom);
    O(VASliceParameterBufferHEVC, delta_chroma_log2_weight_denom);
    O(VASliceParameterBufferHEVC, delta_luma_weight_l0);
    O(VASliceParameterBufferHEVC, luma_offset_l0);
    O(VASliceParameterBufferHEVC, delta_chroma_weight_l0);
    O(VASliceParameterBufferHEVC, ChromaOffsetL0);
    O(VASliceParameterBufferHEVC, delta_luma_weight_l1);
    O(VASliceParameterBufferHEVC, luma_offset_l1);
    O(VASliceParameterBufferHEVC, delta_chroma_weight_l1);
    O(VASliceParameterBufferHEVC, ChromaOffsetL1);
    O(VASliceParameterBufferHEVC, five_minus_max_num_merge_cand);
    O(VASliceParameterBufferHEVC, num_entry_point_offsets);
    O(VASliceParameterBufferHEVC, entry_offset_to_subset_array);
    O(VASliceParameterBufferHEVC, slice_data_num_emu_prevn_bytes);
    O(VASliceParameterBufferHEVC, va_reserved);

    S(VAIQMatrixBufferHEVC);
    O(VAIQMatrixBufferHEVC, ScalingList4x4);
    O(VAIQMatrixBufferHEVC, ScalingList8x8);
    O(VAIQMatrixBufferHEVC, ScalingList16x16);
    O(VAIQMatrixBufferHEVC, ScalingList32x32);
    O(VAIQMatrixBufferHEVC, ScalingListDC16x16);
    O(VAIQMatrixBufferHEVC, ScalingListDC32x32);
    O(VAIQMatrixBufferHEVC, va_reserved);

    {
        VAPictureParameterBufferHEVC h;
        h.pic_fields.value = 0;
        h.pic_fields.bits.chroma_format_idc = 3;
        printf("bits hevc pic_fields.chroma_format_idc=3 -> 0x%08x\n", h.pic_fields.value);
        h.pic_fields.value = 0;
        h.pic_fields.bits.NoBiPredFlag = 1;
        printf("bits hevc pic_fields.NoBiPredFlag=1 -> 0x%08x\n", h.pic_fields.value);
        h.slice_parsing_fields.value = 0;
        h.slice_parsing_fields.bits.IntraPicFlag = 1;
        printf("bits hevc slice_parsing_fields.IntraPicFlag=1 -> 0x%08x\n", h.slice_parsing_fields.value);
        VASliceParameterBufferHEVC s;
        s.LongSliceFlags.value = 0;
        s.LongSliceFlags.fields.slice_type = 3;
        printf("bits hevc LongSliceFlags.slice_type=3 -> 0x%08x\n", s.LongSliceFlags.value);
        s.LongSliceFlags.value = 0;
        s.LongSliceFlags.fields.slice_loop_filter_across_slices_enabled_flag = 1;
        printf("bits hevc LongSliceFlags.slice_loop_filter_across=1 -> 0x%08x\n", s.LongSliceFlags.value);
    }

    /* ---- AV1 (va_dec_av1.h) ----
     *
     * Three things make this codec's layout worth measuring rather than counting:
     * a POINTER member (`anchor_frames_list`) that forces eight-byte alignment and
     * therefore padding nothing in the field list suggests; two bit-field unions
     * that are NOT 32 bits wide (`loop_filter_info_fields` is a uint8_t,
     * `qmatrix_fields` and `loop_restoration_fields` are uint16_t), so a u32 `pack`
     * would write over the neighbouring field; and three nested structs whose own
     * VA_PADDING_LOW tails sit inside the picture-parameter buffer.
     */
    S(VASegmentationStructAV1);
    O(VASegmentationStructAV1, segment_info_fields);
    O(VASegmentationStructAV1, feature_data);
    O(VASegmentationStructAV1, feature_mask);
    O(VASegmentationStructAV1, va_reserved);

    S(VAFilmGrainStructAV1);
    O(VAFilmGrainStructAV1, film_grain_info_fields);
    O(VAFilmGrainStructAV1, grain_seed);
    O(VAFilmGrainStructAV1, num_y_points);
    O(VAFilmGrainStructAV1, point_y_value);
    O(VAFilmGrainStructAV1, point_y_scaling);
    O(VAFilmGrainStructAV1, num_cb_points);
    O(VAFilmGrainStructAV1, point_cb_value);
    O(VAFilmGrainStructAV1, point_cb_scaling);
    O(VAFilmGrainStructAV1, num_cr_points);
    O(VAFilmGrainStructAV1, point_cr_value);
    O(VAFilmGrainStructAV1, point_cr_scaling);
    O(VAFilmGrainStructAV1, ar_coeffs_y);
    O(VAFilmGrainStructAV1, ar_coeffs_cb);
    O(VAFilmGrainStructAV1, ar_coeffs_cr);
    O(VAFilmGrainStructAV1, cb_mult);
    O(VAFilmGrainStructAV1, cb_luma_mult);
    O(VAFilmGrainStructAV1, cb_offset);
    O(VAFilmGrainStructAV1, cr_mult);
    O(VAFilmGrainStructAV1, cr_luma_mult);
    O(VAFilmGrainStructAV1, cr_offset);
    O(VAFilmGrainStructAV1, va_reserved);

    S(VAWarpedMotionParamsAV1);
    O(VAWarpedMotionParamsAV1, wmtype);
    O(VAWarpedMotionParamsAV1, wmmat);
    O(VAWarpedMotionParamsAV1, invalid);
    O(VAWarpedMotionParamsAV1, va_reserved);

    S(VADecPictureParameterBufferAV1);
    O(VADecPictureParameterBufferAV1, profile);
    O(VADecPictureParameterBufferAV1, order_hint_bits_minus_1);
    O(VADecPictureParameterBufferAV1, bit_depth_idx);
    O(VADecPictureParameterBufferAV1, matrix_coefficients);
    O(VADecPictureParameterBufferAV1, seq_info_fields);
    O(VADecPictureParameterBufferAV1, current_frame);
    O(VADecPictureParameterBufferAV1, current_display_picture);
    O(VADecPictureParameterBufferAV1, anchor_frames_num);
    O(VADecPictureParameterBufferAV1, anchor_frames_list);
    O(VADecPictureParameterBufferAV1, frame_width_minus1);
    O(VADecPictureParameterBufferAV1, frame_height_minus1);
    O(VADecPictureParameterBufferAV1, output_frame_width_in_tiles_minus_1);
    O(VADecPictureParameterBufferAV1, output_frame_height_in_tiles_minus_1);
    O(VADecPictureParameterBufferAV1, ref_frame_map);
    O(VADecPictureParameterBufferAV1, ref_frame_idx);
    O(VADecPictureParameterBufferAV1, primary_ref_frame);
    O(VADecPictureParameterBufferAV1, order_hint);
    O(VADecPictureParameterBufferAV1, seg_info);
    O(VADecPictureParameterBufferAV1, film_grain_info);
    O(VADecPictureParameterBufferAV1, tile_cols);
    O(VADecPictureParameterBufferAV1, tile_rows);
    O(VADecPictureParameterBufferAV1, width_in_sbs_minus_1);
    O(VADecPictureParameterBufferAV1, height_in_sbs_minus_1);
    O(VADecPictureParameterBufferAV1, tile_count_minus_1);
    O(VADecPictureParameterBufferAV1, context_update_tile_id);
    O(VADecPictureParameterBufferAV1, pic_info_fields);
    O(VADecPictureParameterBufferAV1, superres_scale_denominator);
    O(VADecPictureParameterBufferAV1, interp_filter);
    O(VADecPictureParameterBufferAV1, filter_level);
    O(VADecPictureParameterBufferAV1, filter_level_u);
    O(VADecPictureParameterBufferAV1, filter_level_v);
    O(VADecPictureParameterBufferAV1, loop_filter_info_fields);
    O(VADecPictureParameterBufferAV1, ref_deltas);
    O(VADecPictureParameterBufferAV1, mode_deltas);
    O(VADecPictureParameterBufferAV1, base_qindex);
    O(VADecPictureParameterBufferAV1, y_dc_delta_q);
    O(VADecPictureParameterBufferAV1, u_dc_delta_q);
    O(VADecPictureParameterBufferAV1, u_ac_delta_q);
    O(VADecPictureParameterBufferAV1, v_dc_delta_q);
    O(VADecPictureParameterBufferAV1, v_ac_delta_q);
    O(VADecPictureParameterBufferAV1, qmatrix_fields);
    O(VADecPictureParameterBufferAV1, mode_control_fields);
    O(VADecPictureParameterBufferAV1, cdef_damping_minus_3);
    O(VADecPictureParameterBufferAV1, cdef_bits);
    O(VADecPictureParameterBufferAV1, cdef_y_strengths);
    O(VADecPictureParameterBufferAV1, cdef_uv_strengths);
    O(VADecPictureParameterBufferAV1, loop_restoration_fields);
    O(VADecPictureParameterBufferAV1, wm);
    O(VADecPictureParameterBufferAV1, va_reserved);
    printf("count VADecPictureParameterBufferAV1 wm      %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->wm) /
               sizeof(((VADecPictureParameterBufferAV1 *)0)->wm[0]));
    printf("size  union seq_info_fields                  %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->seq_info_fields));
    printf("size  union pic_info_fields                  %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->pic_info_fields));
    printf("size  union loop_filter_info_fields          %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->loop_filter_info_fields));
    printf("size  union qmatrix_fields                   %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->qmatrix_fields));
    printf("size  union mode_control_fields              %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->mode_control_fields));
    printf("size  union loop_restoration_fields          %zu\n",
           sizeof(((VADecPictureParameterBufferAV1 *)0)->loop_restoration_fields));

    S(VASliceParameterBufferAV1);
    O(VASliceParameterBufferAV1, slice_data_size);
    O(VASliceParameterBufferAV1, slice_data_offset);
    O(VASliceParameterBufferAV1, slice_data_flag);
    O(VASliceParameterBufferAV1, tile_row);
    O(VASliceParameterBufferAV1, tile_column);
    O(VASliceParameterBufferAV1, tg_start);
    O(VASliceParameterBufferAV1, tg_end);
    O(VASliceParameterBufferAV1, anchor_frame_idx);
    O(VASliceParameterBufferAV1, tile_idx_in_tile_list);
    O(VASliceParameterBufferAV1, va_reserved);

    /*
     * Bit-field allocation order for AV1's six unions — one field at a time, the
     * same proof the H.264 block makes, repeated here because three of these
     * unions are NARROWER than a word and a mistake there is invisible in a u32.
     */
    {
        VADecPictureParameterBufferAV1 a;
        a.seq_info_fields.value = 0;
        a.seq_info_fields.fields.still_picture = 1;
        printf("bits av1 seq_info.still_picture=1 -> 0x%08x\n", a.seq_info_fields.value);
        a.seq_info_fields.value = 0;
        a.seq_info_fields.fields.film_grain_params_present = 1;
        printf("bits av1 seq_info.film_grain_params_present=1 -> 0x%08x\n",
               a.seq_info_fields.value);
        a.seq_info_fields.value = 0;
        a.seq_info_fields.fields.mono_chrome = 1;
        printf("bits av1 seq_info.mono_chrome=1 -> 0x%08x\n", a.seq_info_fields.value);

        a.pic_info_fields.value = 0;
        a.pic_info_fields.bits.frame_type = 3;
        printf("bits av1 pic_info.frame_type=3 -> 0x%08x\n", a.pic_info_fields.value);
        a.pic_info_fields.value = 0;
        a.pic_info_fields.bits.large_scale_tile = 1;
        printf("bits av1 pic_info.large_scale_tile=1 -> 0x%08x\n", a.pic_info_fields.value);
        a.pic_info_fields.value = 0;
        a.pic_info_fields.bits.use_ref_frame_mvs = 1;
        printf("bits av1 pic_info.use_ref_frame_mvs=1 -> 0x%08x\n", a.pic_info_fields.value);

        a.loop_filter_info_fields.value = 0;
        a.loop_filter_info_fields.bits.sharpness_level = 7;
        printf("bits av1 loop_filter_info.sharpness_level=7 -> 0x%02x\n",
               a.loop_filter_info_fields.value);
        a.loop_filter_info_fields.value = 0;
        a.loop_filter_info_fields.bits.mode_ref_delta_update = 1;
        printf("bits av1 loop_filter_info.mode_ref_delta_update=1 -> 0x%02x\n",
               a.loop_filter_info_fields.value);

        a.qmatrix_fields.value = 0;
        a.qmatrix_fields.bits.using_qmatrix = 1;
        printf("bits av1 qmatrix.using_qmatrix=1 -> 0x%04x\n", a.qmatrix_fields.value);
        a.qmatrix_fields.value = 0;
        a.qmatrix_fields.bits.qm_v = 0xf;
        printf("bits av1 qmatrix.qm_v=0xf -> 0x%04x\n", a.qmatrix_fields.value);

        a.mode_control_fields.value = 0;
        a.mode_control_fields.bits.delta_q_present_flag = 1;
        printf("bits av1 mode_control.delta_q_present_flag=1 -> 0x%08x\n",
               a.mode_control_fields.value);
        a.mode_control_fields.value = 0;
        a.mode_control_fields.bits.skip_mode_present = 1;
        printf("bits av1 mode_control.skip_mode_present=1 -> 0x%08x\n",
               a.mode_control_fields.value);
        a.mode_control_fields.value = 0;
        a.mode_control_fields.bits.tx_mode = 3;
        printf("bits av1 mode_control.tx_mode=3 -> 0x%08x\n", a.mode_control_fields.value);

        a.loop_restoration_fields.value = 0;
        a.loop_restoration_fields.bits.yframe_restoration_type = 3;
        printf("bits av1 loop_restoration.yframe_restoration_type=3 -> 0x%04x\n",
               a.loop_restoration_fields.value);
        a.loop_restoration_fields.value = 0;
        a.loop_restoration_fields.bits.lr_uv_shift = 1;
        printf("bits av1 loop_restoration.lr_uv_shift=1 -> 0x%04x\n",
               a.loop_restoration_fields.value);

        VASegmentationStructAV1 s;
        s.segment_info_fields.value = 0;
        s.segment_info_fields.bits.enabled = 1;
        printf("bits av1 segment_info.enabled=1 -> 0x%08x\n", s.segment_info_fields.value);
        s.segment_info_fields.value = 0;
        s.segment_info_fields.bits.update_data = 1;
        printf("bits av1 segment_info.update_data=1 -> 0x%08x\n", s.segment_info_fields.value);

        VAFilmGrainStructAV1 g;
        g.film_grain_info_fields.value = 0;
        g.film_grain_info_fields.bits.apply_grain = 1;
        printf("bits av1 film_grain.apply_grain=1 -> 0x%08x\n", g.film_grain_info_fields.value);
        g.film_grain_info_fields.value = 0;
        g.film_grain_info_fields.bits.clip_to_restricted_range = 1;
        printf("bits av1 film_grain.clip_to_restricted_range=1 -> 0x%08x\n",
               g.film_grain_info_fields.value);
        g.film_grain_info_fields.value = 0;
        g.film_grain_info_fields.bits.grain_scale_shift = 3;
        printf("bits av1 film_grain.grain_scale_shift=3 -> 0x%08x\n",
               g.film_grain_info_fields.value);
    }

    printf("enum VAProfileAV1Profile0                   %d\n", VAProfileAV1Profile0);
    printf("enum VAProfileAV1Profile1                   %d\n", VAProfileAV1Profile1);
    printf("enum VAAV1TransformationIdentity            %d\n", VAAV1TransformationIdentity);
    printf("enum VAAV1TransformationTranslation         %d\n", VAAV1TransformationTranslation);
    printf("enum VAAV1TransformationRotzoom             %d\n", VAAV1TransformationRotzoom);
    printf("enum VAAV1TransformationAffine              %d\n", VAAV1TransformationAffine);
    printf("enum VA_RT_FORMAT_YUV420_10                 0x%08x\n", VA_RT_FORMAT_YUV420_10);
    printf("enum VA_RT_FORMAT_YUV420                    0x%08x\n", VA_RT_FORMAT_YUV420);

    printf("VA_PADDING_LOW=%d VA_PADDING_MEDIUM=%d\n", VA_PADDING_LOW, VA_PADDING_MEDIUM);

    /*
     * The export descriptor. This one is not a buffer we FILL — it is a struct the
     * driver WRITES, so a wrong layout is read as plausible garbage (an fd from the
     * middle of a pitch, a plane count from a modifier's high word) rather than
     * refused. It carries fixed-size arrays whose bounds the flattening walk trusts,
     * which is exactly the shape that turned into the green-screen bug once already.
     */
    S(VADRMPRIMESurfaceDescriptor);
    O(VADRMPRIMESurfaceDescriptor, fourcc);
    O(VADRMPRIMESurfaceDescriptor, width);
    O(VADRMPRIMESurfaceDescriptor, height);
    O(VADRMPRIMESurfaceDescriptor, num_objects);
    O(VADRMPRIMESurfaceDescriptor, objects);
    O(VADRMPRIMESurfaceDescriptor, num_layers);
    O(VADRMPRIMESurfaceDescriptor, layers);
    printf("count VADRMPRIMESurfaceDescriptor objects   %zu\n",
           sizeof(((VADRMPRIMESurfaceDescriptor *)0)->objects) /
               sizeof(((VADRMPRIMESurfaceDescriptor *)0)->objects[0]));
    printf("count VADRMPRIMESurfaceDescriptor layers    %zu\n",
           sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers) /
               sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers[0]));
    printf("count layer.object_index                    %zu\n",
           sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers[0].object_index) /
               sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers[0].object_index[0]));

    /*
     * The enumerators the runtime calls pass by value. Printed rather than
     * transcribed because two of them are the exact pair this program has already
     * been warned about: VASliceParameterBufferType and VASliceDataBufferType are
     * 4 and 5, not the 3 and 4 that counting the enum from the top suggests.
     */
    printf("enum VAEntrypointVLD                        %d\n", VAEntrypointVLD);
    printf("enum VAConfigAttribRTFormat                 %d\n", VAConfigAttribRTFormat);
    printf("enum VAPictureParameterBufferType           %d\n", VAPictureParameterBufferType);
    printf("enum VAIQMatrixBufferType                   %d\n", VAIQMatrixBufferType);
    printf("enum VASliceParameterBufferType             %d\n", VASliceParameterBufferType);
    printf("enum VASliceDataBufferType                  %d\n", VASliceDataBufferType);
    printf("enum VA_EXPORT_SURFACE_READ_ONLY            0x%04x\n", VA_EXPORT_SURFACE_READ_ONLY);
    printf("enum VA_EXPORT_SURFACE_SEPARATE_LAYERS      0x%04x\n",
           VA_EXPORT_SURFACE_SEPARATE_LAYERS);
    printf("enum VA_SURFACE_ATTRIB_SETTABLE             0x%04x\n", VA_SURFACE_ATTRIB_SETTABLE);
    printf("enum VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 0x%08x\n",
           VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2);
    printf("enum VASurfaceAttribPixelFormat             %d\n", VASurfaceAttribPixelFormat);
    printf("enum VAGenericValueTypeInteger              %d\n", VAGenericValueTypeInteger);
    printf("enum VA_STATUS_SUCCESS                      %d\n", VA_STATUS_SUCCESS);
    printf("enum VA_INVALID_ID                          0x%08x\n", VA_INVALID_ID);
    printf("enum VA_FOURCC_NV12                         0x%08x\n", VA_FOURCC_NV12);
    printf("enum VA_FOURCC_P010                         0x%08x\n", VA_FOURCC_P010);
    printf("enum VA_PROGRESSIVE                         0x%04x\n", VA_PROGRESSIVE);

    S(VASurfaceAttrib);
    O(VASurfaceAttrib, type);
    O(VASurfaceAttrib, flags);
    O(VASurfaceAttrib, value);
    S(VAGenericValue);
    O(VAGenericValue, type);
    O(VAGenericValue, value);
    S(VAConfigAttrib);
    O(VAConfigAttrib, type);
    O(VAConfigAttrib, value);

    /*
     * The IMAGE pair — `vaDeriveImage` / `vaCreateImage` + `vaGetImage` write these,
     * and they are the only way anything reads a decoded VAAPI surface back on the
     * CPU. `VAImage` is the awkward one of the whole file: `width` and `height` are
     * `unsigned short`, so the four-byte fields around them are NOT where counting
     * 32-bit words would put them, and `component_order` is four `char` rather than a
     * padded word. Both are measured here rather than reasoned about.
     *
     * ⚠ The readback these describe is TEST-ONLY (see `video_vaapi_native`'s `parity`
     * module). The production path exports a DRM-PRIME dmabuf and never maps a
     * surface; the structures are declared for the same reason every other structure
     * in this file is, so a parity harness can be written without a libva build
     * dependency.
     */
    S(VAImageFormat);
    O(VAImageFormat, fourcc);
    O(VAImageFormat, byte_order);
    O(VAImageFormat, bits_per_pixel);
    O(VAImageFormat, depth);
    O(VAImageFormat, red_mask);
    O(VAImageFormat, green_mask);
    O(VAImageFormat, blue_mask);
    O(VAImageFormat, alpha_mask);
    O(VAImageFormat, va_reserved);

    S(VAImage);
    O(VAImage, image_id);
    O(VAImage, format);
    O(VAImage, buf);
    O(VAImage, width);
    O(VAImage, height);
    O(VAImage, data_size);
    O(VAImage, num_planes);
    O(VAImage, pitches);
    O(VAImage, offsets);
    O(VAImage, num_palette_entries);
    O(VAImage, entry_bytes);
    O(VAImage, component_order);
    O(VAImage, va_reserved);
    printf("count VAImage pitches                       %zu\n",
           sizeof(((VAImage *)0)->pitches) / sizeof(((VAImage *)0)->pitches[0]));
    printf("count VAImage offsets                       %zu\n",
           sizeof(((VAImage *)0)->offsets) / sizeof(((VAImage *)0)->offsets[0]));
    printf("count VAImage component_order               %zu\n",
           sizeof(((VAImage *)0)->component_order));
    printf("enum VA_LSB_FIRST                           %d\n", VA_LSB_FIRST);
    printf("enum VA_MSB_FIRST                           %d\n", VA_MSB_FIRST);

    /* VideoProc: the ingest CSC (`src/vpp.rs`) and the dmabuf import attributes. */
    S(VARectangle);
    O(VARectangle, x);
    O(VARectangle, y);
    O(VARectangle, width);
    O(VARectangle, height);
    S(VAProcColorProperties);
    O(VAProcColorProperties, chroma_sample_location);
    O(VAProcColorProperties, color_range);
    O(VAProcColorProperties, colour_primaries);
    O(VAProcColorProperties, transfer_characteristics);
    O(VAProcColorProperties, matrix_coefficients);
    O(VAProcColorProperties, reserved);
    S(VAProcPipelineParameterBuffer);
    O(VAProcPipelineParameterBuffer, surface);
    O(VAProcPipelineParameterBuffer, surface_region);
    O(VAProcPipelineParameterBuffer, surface_color_standard);
    O(VAProcPipelineParameterBuffer, output_region);
    O(VAProcPipelineParameterBuffer, output_background_color);
    O(VAProcPipelineParameterBuffer, output_color_standard);
    O(VAProcPipelineParameterBuffer, pipeline_flags);
    O(VAProcPipelineParameterBuffer, filter_flags);
    O(VAProcPipelineParameterBuffer, filters);
    O(VAProcPipelineParameterBuffer, num_filters);
    O(VAProcPipelineParameterBuffer, forward_references);
    O(VAProcPipelineParameterBuffer, num_forward_references);
    O(VAProcPipelineParameterBuffer, backward_references);
    O(VAProcPipelineParameterBuffer, num_backward_references);
    O(VAProcPipelineParameterBuffer, rotation_state);
    O(VAProcPipelineParameterBuffer, blend_state);
    O(VAProcPipelineParameterBuffer, mirror_state);
    O(VAProcPipelineParameterBuffer, additional_outputs);
    O(VAProcPipelineParameterBuffer, num_additional_outputs);
    O(VAProcPipelineParameterBuffer, input_surface_flag);
    O(VAProcPipelineParameterBuffer, output_surface_flag);
    O(VAProcPipelineParameterBuffer, input_color_properties);
    O(VAProcPipelineParameterBuffer, output_color_properties);
    O(VAProcPipelineParameterBuffer, processing_mode);
    O(VAProcPipelineParameterBuffer, output_hdr_metadata);
    O(VAProcPipelineParameterBuffer, va_reserved);
    printf("count VAProcPipelineParameterBuffer va_reserved %zu\n",
           sizeof(((VAProcPipelineParameterBuffer *)0)->va_reserved) / sizeof(uint32_t));
    printf("enum VAProfileNone                          %d\n", VAProfileNone);
    printf("enum VAEntrypointVideoProc                  %d\n", VAEntrypointVideoProc);
    printf("enum VAProcPipelineParameterBufferType      %d\n", VAProcPipelineParameterBufferType);
    printf("enum VASurfaceAttribMemoryType              %d\n", VASurfaceAttribMemoryType);
    printf("enum VASurfaceAttribExternalBufferDescriptor %d\n", VASurfaceAttribExternalBufferDescriptor);
    printf("enum VAGenericValueTypePointer              %d\n", VAGenericValueTypePointer);
    printf("hex  VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 %#x\n", VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2);
    printf("hex  VA_RT_FORMAT_RGB32                     %#x\n", VA_RT_FORMAT_RGB32);
    printf("hex  VA_RT_FORMAT_RGB32_10                  %#x\n", VA_RT_FORMAT_RGB32_10);
    printf("hex  VA_RT_FORMAT_YUV420_10                 %#x\n", VA_RT_FORMAT_YUV420_10);
    printf("enum VAProcColorStandardNone                %d\n", VAProcColorStandardNone);
    printf("enum VAProcColorStandardBT709               %d\n", VAProcColorStandardBT709);
    printf("enum VAProcColorStandardBT2020              %d\n", VAProcColorStandardBT2020);
    printf("enum VAProcColorStandardSRGB                %d\n", VAProcColorStandardSRGB);
    printf("enum VAProcColorStandardExplicit            %d\n", VAProcColorStandardExplicit);
    printf("enum VA_SOURCE_RANGE_FULL                   %d\n", VA_SOURCE_RANGE_FULL);
    printf("enum VA_SOURCE_RANGE_REDUCED                %d\n", VA_SOURCE_RANGE_REDUCED);
    printf("hex  VA_FOURCC_BGRX %#x RGBX %#x BGRA %#x RGBA %#x\n", VA_FOURCC_BGRX, VA_FOURCC_RGBX, VA_FOURCC_BGRA, VA_FOURCC_RGBA);
    printf("hex  VA_FOURCC_XRGB %#x XBGR %#x ARGB %#x ABGR %#x\n", VA_FOURCC_XRGB, VA_FOURCC_XBGR, VA_FOURCC_ARGB, VA_FOURCC_ABGR);
    printf("hex  VA_FOURCC_X2R10G10B10 %#x X2B10G10R10 %#x A2R10G10B10 %#x A2B10G10R10 %#x\n",
           VA_FOURCC_X2R10G10B10, VA_FOURCC_X2B10G10R10, VA_FOURCC_A2R10G10B10, VA_FOURCC_A2B10G10R10);
    printf("hex  VA_FOURCC_NV12 %#x P010 %#x\n", VA_FOURCC_NV12, VA_FOURCC_P010);
    printf("hex  VA_EXPORT_SURFACE_READ_WRITE %#x WRITE_ONLY %#x\n", VA_EXPORT_SURFACE_READ_WRITE, VA_EXPORT_SURFACE_WRITE_ONLY);
    /* Rolling intra refresh (the on-demand wave): one misc buffer per picture. */
    S(VAEncMiscParameterRIR);
    O(VAEncMiscParameterRIR, intra_insertion_location);
    O(VAEncMiscParameterRIR, intra_insert_size);
    O(VAEncMiscParameterRIR, qp_delta_for_inserted_intra);
    O(VAEncMiscParameterRIR, va_reserved);
    printf("enum VAEncMiscParameterTypeRIR              %d\n", VAEncMiscParameterTypeRIR);
    {
        VAEncMiscParameterRIR r = {0};
        r.rir_flags.bits.enable_rir_row = 1;
        printf("bits rir_flags.enable_rir_row=1 -> value 0x%08x\n", r.rir_flags.value);
    }
    return 0;
}
