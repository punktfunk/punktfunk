/*
 * Layout probe for the hand-declared DXVA AV1 structures in `src/dxva_av1.rs`.
 *
 * `src/dxva.rs`'s module docs explain why these layouts are written by hand:
 * windows-rs generates nothing from `dxva.h`, so the structs are transcribed —
 * and that file is the most safety-critical in the backend, because nothing in it
 * is type-checked against Windows. A field at the wrong offset is not a compile
 * error, it is a driver reading a reference index where a quantiser should be.
 *
 * AV1 is the first codec here whose declaration we can measure against the
 * AUTHORITATIVE header rather than against libavcodec's mirror: `DXVA_PicParams_AV1`
 * ships in the Windows SDK's own `dxva.h` (present in 10.0.26100.0 and 10.0.28000.0
 * on the .173 box). That is the declaration the driver was compiled against, so it
 * outranks any second-hand copy.
 *
 * Run it on a Windows box with the SDK (no FFmpeg, no GPU work, no stream):
 *
 *   cmd /c ""C:\Program Files\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat" ^
 *     && cl /nologo /W3 layout-probe-av1.c /Fe:probe-av1.exe && probe-av1.exe"
 *
 * Every number it prints is pinned as a `const` assertion in `src/dxva_av1.rs`, so a
 * transcription mistake is a compile error. The bit-field section exists because C
 * bit-field allocation order is ABI-defined rather than standardised: it PROVES
 * MSVC's least-significant-bit-first order rather than assuming it.
 */
#include <stdio.h>
#include <stddef.h>
#include <windows.h>
#include <dxva.h>

#define S(t)        printf("size %-34s %zu align %zu\n", #t, sizeof(t), __alignof(t))
#define O(t, f)     printf("off  %-24s %-34s %zu\n", #t, #f, offsetof(t, f))

int main(void) {
    S(DXVA_PicEntry_AV1);
    S(DXVA_PicParams_AV1);
    S(DXVA_Tile_AV1);
    S(DXVA_Status_AV1);

    O(DXVA_PicParams_AV1, width);
    O(DXVA_PicParams_AV1, height);
    O(DXVA_PicParams_AV1, max_width);
    O(DXVA_PicParams_AV1, max_height);
    O(DXVA_PicParams_AV1, CurrPicTextureIndex);
    O(DXVA_PicParams_AV1, superres_denom);
    O(DXVA_PicParams_AV1, bitdepth);
    O(DXVA_PicParams_AV1, seq_profile);

    O(DXVA_PicParams_AV1, tiles);
    O(DXVA_PicParams_AV1, tiles.cols);
    O(DXVA_PicParams_AV1, tiles.rows);
    O(DXVA_PicParams_AV1, tiles.context_update_id);
    O(DXVA_PicParams_AV1, tiles.widths);
    O(DXVA_PicParams_AV1, tiles.heights);

    O(DXVA_PicParams_AV1, coding);
    O(DXVA_PicParams_AV1, format);
    O(DXVA_PicParams_AV1, primary_ref_frame);
    O(DXVA_PicParams_AV1, order_hint);
    O(DXVA_PicParams_AV1, order_hint_bits);
    O(DXVA_PicParams_AV1, frame_refs);
    O(DXVA_PicParams_AV1, RefFrameMapTextureIndex);

    O(DXVA_PicParams_AV1, loop_filter);
    O(DXVA_PicParams_AV1, loop_filter.filter_level);
    O(DXVA_PicParams_AV1, loop_filter.filter_level_u);
    O(DXVA_PicParams_AV1, loop_filter.filter_level_v);
    O(DXVA_PicParams_AV1, loop_filter.sharpness_level);
    O(DXVA_PicParams_AV1, loop_filter.ref_deltas);
    O(DXVA_PicParams_AV1, loop_filter.mode_deltas);
    O(DXVA_PicParams_AV1, loop_filter.delta_lf_res);
    O(DXVA_PicParams_AV1, loop_filter.frame_restoration_type);
    O(DXVA_PicParams_AV1, loop_filter.log2_restoration_unit_size);

    O(DXVA_PicParams_AV1, quantization);
    O(DXVA_PicParams_AV1, quantization.base_qindex);
    O(DXVA_PicParams_AV1, quantization.y_dc_delta_q);
    O(DXVA_PicParams_AV1, quantization.u_dc_delta_q);
    O(DXVA_PicParams_AV1, quantization.v_dc_delta_q);
    O(DXVA_PicParams_AV1, quantization.u_ac_delta_q);
    O(DXVA_PicParams_AV1, quantization.v_ac_delta_q);
    O(DXVA_PicParams_AV1, quantization.qm_y);
    O(DXVA_PicParams_AV1, quantization.qm_u);
    O(DXVA_PicParams_AV1, quantization.qm_v);

    O(DXVA_PicParams_AV1, cdef);
    O(DXVA_PicParams_AV1, cdef.y_strengths);
    O(DXVA_PicParams_AV1, cdef.uv_strengths);
    O(DXVA_PicParams_AV1, interp_filter);

    O(DXVA_PicParams_AV1, segmentation);
    O(DXVA_PicParams_AV1, segmentation.feature_mask);
    O(DXVA_PicParams_AV1, segmentation.feature_data);

    O(DXVA_PicParams_AV1, film_grain);
    O(DXVA_PicParams_AV1, film_grain.grain_seed);
    O(DXVA_PicParams_AV1, film_grain.scaling_points_y);
    O(DXVA_PicParams_AV1, film_grain.num_y_points);
    O(DXVA_PicParams_AV1, film_grain.scaling_points_cb);
    O(DXVA_PicParams_AV1, film_grain.num_cb_points);
    O(DXVA_PicParams_AV1, film_grain.scaling_points_cr);
    O(DXVA_PicParams_AV1, film_grain.num_cr_points);
    O(DXVA_PicParams_AV1, film_grain.ar_coeffs_y);
    O(DXVA_PicParams_AV1, film_grain.ar_coeffs_cb);
    O(DXVA_PicParams_AV1, film_grain.ar_coeffs_cr);
    O(DXVA_PicParams_AV1, film_grain.cb_mult);
    O(DXVA_PicParams_AV1, film_grain.cb_luma_mult);
    O(DXVA_PicParams_AV1, film_grain.cr_mult);
    O(DXVA_PicParams_AV1, film_grain.cr_luma_mult);
    O(DXVA_PicParams_AV1, film_grain.cb_offset);
    O(DXVA_PicParams_AV1, film_grain.cr_offset);

    O(DXVA_PicParams_AV1, Reserved32Bits);
    O(DXVA_PicParams_AV1, StatusReportFeedbackNumber);

    O(DXVA_Tile_AV1, DataOffset);
    O(DXVA_Tile_AV1, DataSize);
    O(DXVA_Tile_AV1, row);
    O(DXVA_Tile_AV1, column);
    O(DXVA_Tile_AV1, anchor_frame);

    /*
     * Bit-field allocation order. MSVC packs from the least significant bit of the
     * storage unit upward in declaration order — proved here rather than assumed,
     * because getting it backwards puts every tool flag in the wrong place and the
     * picture merely decodes wrong.
     */
    {
        DXVA_PicParams_AV1 p;

        memset(&p, 0, sizeof(p));
        p.coding.use_128x128_superblock = 1;
        printf("bits coding.use_128x128_superblock=1 -> 0x%08x\n", p.coding.CodingParamToolFlags);
        memset(&p, 0, sizeof(p));
        p.coding.tx_mode = 3;
        printf("bits coding.tx_mode=3                -> 0x%08x\n", p.coding.CodingParamToolFlags);
        memset(&p, 0, sizeof(p));
        p.coding.reference_frame_update = 1;
        printf("bits coding.reference_frame_update=1 -> 0x%08x\n", p.coding.CodingParamToolFlags);

        memset(&p, 0, sizeof(p));
        p.format.frame_type = 3;
        printf("bits format.frame_type=3             -> 0x%02x\n", p.format.FormatAndPictureInfoFlags);
        memset(&p, 0, sizeof(p));
        p.format.mono_chrome = 1;
        printf("bits format.mono_chrome=1            -> 0x%02x\n", p.format.FormatAndPictureInfoFlags);

        memset(&p, 0, sizeof(p));
        p.loop_filter.delta_lf_present = 1;
        printf("bits loop_filter.delta_lf_present=1  -> 0x%02x\n", p.loop_filter.ControlFlags);

        memset(&p, 0, sizeof(p));
        p.quantization.delta_q_res = 3;
        printf("bits quantization.delta_q_res=3      -> 0x%02x\n", p.quantization.ControlFlags);

        memset(&p, 0, sizeof(p));
        p.cdef.bits = 3;
        printf("bits cdef.bits=3                     -> 0x%02x\n", p.cdef.ControlFlags);
        memset(&p, 0, sizeof(p));
        p.cdef.y_strengths[0].secondary = 3;
        printf("bits cdef.y_strengths[0].secondary=3 -> 0x%02x\n", p.cdef.y_strengths[0].combined);

        memset(&p, 0, sizeof(p));
        p.segmentation.temporal_update = 1;
        printf("bits segmentation.temporal_update=1  -> 0x%02x\n", p.segmentation.ControlFlags);
        memset(&p, 0, sizeof(p));
        p.segmentation.feature_mask[0].globalmv = 1;
        printf("bits segmentation.feature_mask[0].globalmv=1 -> 0x%02x\n",
               p.segmentation.feature_mask[0].mask);

        memset(&p, 0, sizeof(p));
        p.film_grain.ar_coeff_shift_minus6 = 3;
        printf("bits film_grain.ar_coeff_shift_minus6=3 -> 0x%04x\n", p.film_grain.ControlFlags);
        memset(&p, 0, sizeof(p));
        p.film_grain.matrix_coeff_is_identity = 1;
        printf("bits film_grain.matrix_coeff_is_identity=1 -> 0x%04x\n", p.film_grain.ControlFlags);
    }
    return 0;
}
