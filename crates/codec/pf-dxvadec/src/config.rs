//! Decoder-creation decisions, isolated from D3D11 so they unit-test on every host:
//! which DXVA profile a stream needs, which `D3D11_VIDEO_DECODER_CONFIG` to pick
//! from a driver's offers, how large the decode surfaces are, and how many.
//!
//! `video_d3d11_native.rs` enumerates and creates; this module chooses, against
//! values the driver already returned. Same split as `pf-vkdecode`'s
//! `caps`. Profile GUIDs match `video_d3d11.rs`.

/// GUID `ID3D11VideoDevice::GetVideoDecoderProfile` returns and
/// `D3D11_VIDEO_DECODER_DESC::Guid` takes.
///
/// Stored as `u128` so this crate builds without the Windows SDK; the host
/// converts with `GUID::from_u128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxvaProfile {
    pub name: &'static str,
    pub guid: u128,
    /// Surface format: `DXGI_FORMAT_NV12` (103) 8-bit, `DXGI_FORMAT_P010` (104) 10-bit.
    /// `i32` matches windows-rs's `DXGI_FORMAT` alias; a `u32` would need a cast.
    pub dxgi_format: i32,
}

pub const DXGI_FORMAT_NV12: i32 = 103;
pub const DXGI_FORMAT_P010: i32 = 104;

/// `D3D11_DECODER_PROFILE_H264_VLD_NOFGT`. Film-grain variants are unused here.
pub const H264_VLD_NOFGT: DxvaProfile = DxvaProfile {
    name: "H.264 VLD NoFGT",
    guid: 0x1b81be68_a0c7_11d3_b984_00c04f2e73c5,
    dxgi_format: DXGI_FORMAT_NV12,
};

/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN`.
pub const HEVC_VLD_MAIN: DxvaProfile = DxvaProfile {
    name: "HEVC Main",
    guid: 0x5b11d51b_2f4c_4452_bcc3_09f2a1160cc0,
    dxgi_format: DXGI_FORMAT_NV12,
};

/// `D3D11_DECODER_PROFILE_HEVC_VLD_MAIN10`.
pub const HEVC_VLD_MAIN10: DxvaProfile = DxvaProfile {
    name: "HEVC Main10",
    guid: 0x107af0e0_ef1a_4d19_aba8_67a163073d13,
    dxgi_format: DXGI_FORMAT_P010,
};

/// `D3D11_DECODER_PROFILE_AV1_VLD_PROFILE0`. AV1 profiles are chroma, not depth:
/// Profile 0 is 4:2:0 at 8 and 10 bits, so [`AV1_VLD_PROFILE0_10BIT`] reuses this
/// GUID with P010.
pub const AV1_VLD_PROFILE0: DxvaProfile = DxvaProfile {
    name: "AV1 Profile 0",
    guid: 0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a,
    dxgi_format: DXGI_FORMAT_NV12,
};

/// Same GUID as [`AV1_VLD_PROFILE0`], P010 surfaces. Separate because
/// `CheckVideoDecoderFormat` and the pool are per-format: a driver can accept
/// the GUID at NV12 and refuse it at P010.
pub const AV1_VLD_PROFILE0_10BIT: DxvaProfile = DxvaProfile {
    name: "AV1 Profile 0 (10-bit)",
    guid: 0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a,
    dxgi_format: DXGI_FORMAT_P010,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

/// `None` when the shape is outside this backend. Refused before a decoder
/// exists so the ladder can fall through without burning the opening IDR.
///
/// Only 4:2:0 (`chroma_format_idc == 1`): DXVA 4:4:4 RExt exists, but the
/// `VideoProcessorBlt` hand-off has no measured 4:4:4 input. H.264 High10 has
/// no mainstream GUID. HEVC and AV1 stop at 10-bit. AV1 `mono_chrome` arrives
/// as `chroma_format_idc` 0 and is refused with every other non-4:2:0 shape.
pub fn profile_for(codec: Codec, chroma_format_idc: u8, bit_depth: u8) -> Option<DxvaProfile> {
    if chroma_format_idc != 1 {
        return None;
    }
    match (codec, bit_depth) {
        (Codec::H264, 8) => Some(H264_VLD_NOFGT),
        (Codec::H265, 8) => Some(HEVC_VLD_MAIN),
        (Codec::H265, 10) => Some(HEVC_VLD_MAIN10),
        (Codec::Av1, 8) => Some(AV1_VLD_PROFILE0),
        (Codec::Av1, 10) => Some(AV1_VLD_PROFILE0_10BIT),
        _ => None,
    }
}

/// `ConfigBitstreamRaw` for short-format slice control. The specs number this
/// differently; it selects which slice-control struct the driver reads:
///
/// * H.264: `1` = long (`DXVA_Slice_H264_Long`), `2` = short (`DXVA_Slice_H264_Short`);
/// * HEVC: `1` = short (`DXVA_Slice_HEVC_Short`) — the only format the spec defines;
/// * AV1: `1` — one record (`DXVA_Tile_AV1`), no long form.
///
/// Long format also carries derived reference lists and prediction weights
/// already in the picture parameters. A device with no short-format config is
/// refused; the FFmpeg rung implements both. libavcodec's
/// `dxva_get_decoder_configuration` scores `ConfigBitstreamRaw == 1` for every
/// codec and accepts `2` only for H.264.
pub const fn short_slice_config(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 2,
        Codec::H265 | Codec::Av1 => 1,
    }
}

/// The three fields `pick_config` / `pool_size` read from a
/// `D3D11_VIDEO_DECODER_CONFIG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigFacts {
    /// `D3D11_VIDEO_DECODER_CONFIG::ConfigBitstreamRaw`.
    pub bitstream_raw: u32,
    /// Is `guidConfigBitstreamEncryption` the all-zero "no encryption" GUID?
    pub no_encryption: bool,
    /// `ConfigMinRenderTargetBuffCount`. [`pool_size`] honours it as a floor.
    pub min_render_target_buffers: u16,
}

/// First short-format config, preferring unencrypted.
///
/// Returns an index so the caller can pass the driver's
/// `D3D11_VIDEO_DECODER_CONFIG` through: synthesizing from these three fields
/// drops the other `Config*` members. `None` is a refusal, not a fallback —
/// short-format slices against a long-format config decode to garbage.
pub fn pick_config(codec: Codec, configs: &[ConfigFacts]) -> Option<usize> {
    let want = short_slice_config(codec);
    let mut best: Option<(usize, u8)> = None;
    for (index, cfg) in configs.iter().enumerate() {
        if cfg.bitstream_raw != want {
            continue;
        }
        // Strict `>` keeps the driver's order among equal scores.
        let score = u8::from(cfg.no_encryption);
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((index, score));
        }
    }
    best.map(|(index, _)| index)
}

/// Decode-surface alignment in luma samples. 16 is a macroblock; 128 is what
/// the DXVA HEVC spec and libavcodec's `ff_dxva2_common_frame_params` require.
/// A miss smears the bottom rows rather than failing validation.
///
/// AV1 is 128 from the same function (`codec_id == HEVC || codec_id == AV1`),
/// not by analogy with superblock size.
pub const fn surface_alignment(codec: Codec) -> u32 {
    match codec {
        Codec::H264 => 16,
        Codec::H265 | Codec::Av1 => 128,
    }
}

pub const fn align_surface(value: u32, codec: Codec) -> u32 {
    let align = surface_alignment(codec);
    value.div_ceil(align) * align
}

/// One surface per DPB slot, or `ConfigMinRenderTargetBuffCount` when larger.
///
/// `dpb_slots` is [`crate::SlotMap`] capacity (`max_dpb_frames + 1`). A DXVA
/// surface is the picture: `RefFrameList` indexes the slot, unlike Vulkan's
/// decoupled picture pool. No spare — a `dpb_slots + 1`th surface cannot be
/// named. `driver_min` is the only honest extra: some drivers refuse a smaller
/// pool.
pub fn pool_size(dpb_slots: usize, driver_min: u16) -> u32 {
    // `DXVA_PicEntry::Index7Bits` is seven bits; 127 is the ceiling.
    (dpb_slots.max(usize::from(driver_min)).min(127)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_profile_table_covers_exactly_the_shapes_this_backend_decodes() {
        assert_eq!(profile_for(Codec::H264, 1, 8), Some(H264_VLD_NOFGT));
        assert_eq!(profile_for(Codec::H265, 1, 8), Some(HEVC_VLD_MAIN));
        assert_eq!(profile_for(Codec::H265, 1, 10), Some(HEVC_VLD_MAIN10));
        assert_eq!(
            profile_for(Codec::H265, 1, 10).map(|p| p.dxgi_format),
            Some(DXGI_FORMAT_P010)
        );
        assert_eq!(
            profile_for(Codec::H265, 1, 8).map(|p| p.dxgi_format),
            Some(DXGI_FORMAT_NV12)
        );
        assert_eq!(profile_for(Codec::Av1, 1, 8), Some(AV1_VLD_PROFILE0));
        assert_eq!(profile_for(Codec::Av1, 1, 10), Some(AV1_VLD_PROFILE0_10BIT));
        assert_eq!(AV1_VLD_PROFILE0.guid, AV1_VLD_PROFILE0_10BIT.guid);
        assert_eq!(AV1_VLD_PROFILE0.dxgi_format, DXGI_FORMAT_NV12);
        assert_eq!(AV1_VLD_PROFILE0_10BIT.dxgi_format, DXGI_FORMAT_P010);
        // Same GUID `video_d3d11.rs` hands FFmpeg (`PROFILE_AV1_VLD_PROFILE0`).
        assert_eq!(
            AV1_VLD_PROFILE0.guid,
            0xb8be4ccb_cf53_46ba_8d59_d6b8a6da5d2a
        );
    }

    #[test]
    fn shapes_outside_the_envelope_are_refused_rather_than_approximated() {
        assert_eq!(profile_for(Codec::H264, 3, 8), None);
        assert_eq!(profile_for(Codec::H265, 3, 10), None);
        assert_eq!(profile_for(Codec::H265, 2, 8), None);
        assert_eq!(profile_for(Codec::H264, 1, 10), None);
        assert_eq!(profile_for(Codec::H265, 1, 12), None);
        // AV1 `mono_chrome` arrives as `chroma_format_idc` 0, not 4:2:0.
        assert_eq!(profile_for(Codec::Av1, 3, 8), None);
        assert_eq!(profile_for(Codec::Av1, 3, 10), None);
        assert_eq!(profile_for(Codec::Av1, 1, 12), None);
        assert_eq!(profile_for(Codec::Av1, 0, 8), None);
    }

    #[test]
    fn short_slice_control_is_2_for_h264_and_1_for_hevc_and_av1() {
        assert_eq!(short_slice_config(Codec::H264), 2);
        assert_eq!(short_slice_config(Codec::H265), 1);
        assert_eq!(short_slice_config(Codec::Av1), 1);
    }

    #[test]
    fn config_selection_takes_a_short_format_config_and_prefers_no_encryption() {
        let configs = [
            ConfigFacts {
                bitstream_raw: 1, // H.264 long format — unusable here
                no_encryption: true,
                min_render_target_buffers: 0,
            },
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: false,
                min_render_target_buffers: 0,
            },
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: true,
                min_render_target_buffers: 0,
            },
        ];
        assert_eq!(pick_config(Codec::H264, &configs), Some(2));
        // HEVC/AV1: `1` is short, so index 0 wins. H.264 treated that row as long.
        assert_eq!(pick_config(Codec::H265, &configs), Some(0));
        assert_eq!(pick_config(Codec::Av1, &configs), Some(0));
    }

    #[test]
    fn a_device_with_no_short_format_config_is_refused_not_downgraded() {
        let long_only = [ConfigFacts {
            bitstream_raw: 1,
            no_encryption: true,
            min_render_target_buffers: 0,
        }];
        assert_eq!(pick_config(Codec::H264, &long_only), None);
        assert_eq!(pick_config(Codec::H264, &[]), None);
    }

    #[test]
    fn among_equal_configs_the_drivers_own_order_wins() {
        let configs = [
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: true,
                min_render_target_buffers: 0,
            },
            ConfigFacts {
                bitstream_raw: 2,
                no_encryption: true,
                min_render_target_buffers: 0,
            },
        ];
        assert_eq!(pick_config(Codec::H264, &configs), Some(0));
    }

    #[test]
    fn surfaces_are_macroblock_aligned_for_h264_and_128_aligned_for_hevc() {
        assert_eq!(align_surface(1920, Codec::H264), 1920);
        assert_eq!(align_surface(1080, Codec::H264), 1088);
        assert_eq!(align_surface(1920, Codec::H265), 1920);
        assert_eq!(align_surface(1080, Codec::H265), 1152);
        assert_eq!(align_surface(3840, Codec::H265), 3840);
        assert_eq!(align_surface(2400, Codec::H265), 2432);
        assert_eq!(align_surface(2432, Codec::H265), 2432);
        assert_eq!(align_surface(320, Codec::Av1), 384);
        assert_eq!(align_surface(240, Codec::Av1), 256);
        assert_eq!(align_surface(1920, Codec::Av1), 1920);
        assert_eq!(align_surface(1080, Codec::Av1), 1152);
    }

    #[test]
    fn an_av1_pool_is_the_eight_reference_slots_plus_the_current_picture() {
        // AV1 `NUM_REF_FRAMES` is 8, not an SPS field; pool is 1+8. Driver min still wins.
        assert_eq!(pool_size(9, 0), 9);
        assert_eq!(pool_size(9, 16), 16);
    }

    #[test]
    fn the_pool_holds_exactly_one_surface_per_dpb_slot_and_no_unaddressable_spare() {
        assert_eq!(pool_size(17, 0), 17);
        assert_eq!(pool_size(2, 0), 2);
    }

    #[test]
    fn the_pool_honours_a_drivers_own_minimum_and_the_seven_bit_index_ceiling() {
        assert_eq!(pool_size(3, 12), 12);
        assert_eq!(pool_size(3, 2), 3);
        assert_eq!(pool_size(200, 0), 127);
        assert_eq!(pool_size(3, 4000), 127);
    }
}
