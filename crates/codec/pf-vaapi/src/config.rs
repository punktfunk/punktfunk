//! Which `VAProfile`, render-target format, and surface count a stream needs.
//!
//! Pure functions of stream shape, split out like `pf-dxvadec::config` so ordinary
//! gates run them — not `cfg(target_os = "linux")` FFI that only a box compiles.
//!
//! Constants are libva 2.23.0 enumerators, each declared once here at one integer
//! type; [`crate::enc_h265`] and [`crate::vpp`] re-export the ones they share.

/// `VAEntrypointVLD`. Full bitstream decode; the only entry point this rung uses.
pub const VA_ENTRYPOINT_VLD: i32 = 1;

pub const VA_PROFILE_H264_MAIN: i32 = 6;
pub const VA_PROFILE_H264_HIGH: i32 = 7;
pub const VA_PROFILE_H264_CONSTRAINED_BASELINE: i32 = 13;
pub const VA_PROFILE_HEVC_MAIN: i32 = 17;
pub const VA_PROFILE_HEVC_MAIN10: i32 = 18;
/// Not a count from the top of the enum: ten VP9/HEVC values sit between HEVC and AV1.
pub const VA_PROFILE_AV1_PROFILE0: i32 = 32;
pub const VA_PROFILE_AV1_PROFILE1: i32 = 33;

/// `VAConfigAttribRTFormat` bits: what a surface pool holds.
pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_RT_FORMAT_YUV444: u32 = 0x0000_0004;
pub const VA_RT_FORMAT_YUV420_10: u32 = 0x0000_0100;

/// `VAConfigAttribRTFormat`. Measured; 0 is a real enumerator, not "left unset".
pub const VA_CONFIG_ATTRIB_RT_FORMAT: u32 = 0;

/// Yields [`crate::VaDrmPrimeSurfaceDescriptor`], on export and on dmabuf import.
/// The older `DRM_PRIME` (0x2000_0000) is a different, smaller structure.
pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;

/// Mirrors `pf-dxvadec::Codec` locally; this crate must not depend on the Windows one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
    Av1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VaProfile {
    pub value: i32,
    pub name: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// No profile for this (chroma, depth) — 4:4:4, or a depth outside 8/10.
    UnsupportedShape { chroma_format_idc: u8, depth: u8 },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::UnsupportedShape {
                chroma_format_idc,
                depth,
            } => write!(
                f,
                "no VAAPI decode profile for chroma_format_idc {chroma_format_idc} at {depth} bits"
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

/// H.264 8-bit 4:2:0 is always High, not the SPS `profile_idc`. Main would
/// fail mid-stream on 8×8 transforms; High is a superset. Narrower values
/// exist for the capability probe (what the device offers).
///
/// 4:4:4 is refused: the header has High444, drivers here do not, Vulkan does.
/// AV1 Profile 0 is 8 and 10 bit under one enumerator — `rt_format` must differ
/// or the surface pool is wrong. Profile 1/2 and monochrome (`chroma_format_idc` 0)
/// are refused: `va_dec_av1.h` is 8/10-bit 4:2:0 only.
pub fn profile_for(
    codec: Codec,
    chroma_format_idc: u8,
    depth: u8,
) -> Result<VaProfile, ConfigError> {
    match (codec, chroma_format_idc, depth) {
        (Codec::H264, 1, 8) => Ok(VaProfile {
            value: VA_PROFILE_H264_HIGH,
            name: "H.264 High",
        }),
        (Codec::Av1, 1, 8) => Ok(VaProfile {
            value: VA_PROFILE_AV1_PROFILE0,
            name: "AV1 Profile 0",
        }),
        (Codec::Av1, 1, 10) => Ok(VaProfile {
            value: VA_PROFILE_AV1_PROFILE0,
            name: "AV1 Profile 0 (10-bit)",
        }),
        (Codec::H265, 1, 8) => Ok(VaProfile {
            value: VA_PROFILE_HEVC_MAIN,
            name: "HEVC Main",
        }),
        (Codec::H265, 1, 10) => Ok(VaProfile {
            value: VA_PROFILE_HEVC_MAIN10,
            name: "HEVC Main 10",
        }),
        _ => Err(ConfigError::UnsupportedShape {
            chroma_format_idc,
            depth,
        }),
    }
}

pub fn rt_format(chroma_format_idc: u8, depth: u8) -> Result<u32, ConfigError> {
    match (chroma_format_idc, depth) {
        (1, 8) => Ok(VA_RT_FORMAT_YUV420),
        (1, 10) => Ok(VA_RT_FORMAT_YUV420_10),
        (3, 8) => Ok(VA_RT_FORMAT_YUV444),
        _ => Err(ConfigError::UnsupportedShape {
            chroma_format_idc,
            depth,
        }),
    }
}

/// Zero-copy: a presented surface cannot be decoded into. Size the pool to DPB
/// and the decoder stalls behind display. The Vulkan rung's measured headroom.
/// Do not copy FFmpeg's `extra_hw_frames = 4`: `av_hwframe_get_buffer` blocks;
/// this pool does not.
pub const PRESENTER_HEADROOM: usize = pf_bitstream::slots::HOLD_HEADROOM as usize;

/// VAAPI has no driver minimum. AV1 passes [`AV1_MAX_DPB_FRAMES`] (codec constant, not a sequence header).
pub fn surface_count(max_dpb_frames: usize) -> usize {
    max_dpb_frames + 1 + PRESENTER_HEADROOM
}

/// AV1 DPB depth (`NUM_REF_FRAMES`). Codec constant, not a sequence-header field.
/// Ledger is this plus the picture being decoded (libavcodec `num_surfaces = 1 + 8`).
pub const AV1_MAX_DPB_FRAMES: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_8bit_420_is_high() {
        let p = profile_for(Codec::H264, 1, 8).expect("the envelope's only H.264 shape");
        assert_eq!(p.value, VA_PROFILE_H264_HIGH);
    }

    #[test]
    fn hevc_picks_main_or_main10_by_depth() {
        assert_eq!(
            profile_for(Codec::H265, 1, 8).unwrap().value,
            VA_PROFILE_HEVC_MAIN
        );
        assert_eq!(
            profile_for(Codec::H265, 1, 10).unwrap().value,
            VA_PROFILE_HEVC_MAIN10
        );
    }

    /// Profile 0 is one enumerator for 8 and 10 bit; depth reaches the pool via `rt_format`.
    #[test]
    fn av1_profile0_covers_both_depths_and_the_format_is_what_differs() {
        assert_eq!(
            profile_for(Codec::Av1, 1, 8).unwrap().value,
            VA_PROFILE_AV1_PROFILE0
        );
        assert_eq!(
            profile_for(Codec::Av1, 1, 10).unwrap().value,
            VA_PROFILE_AV1_PROFILE0
        );
        assert_ne!(
            profile_for(Codec::Av1, 1, 8).unwrap().name,
            profile_for(Codec::Av1, 1, 10).unwrap().name,
            "the log must still say which depth the session was built for"
        );
        assert_eq!(rt_format(1, 8).unwrap(), VA_RT_FORMAT_YUV420);
        assert_eq!(rt_format(1, 10).unwrap(), VA_RT_FORMAT_YUV420_10);
    }

    #[test]
    fn shapes_outside_the_envelope_are_refused_not_guessed() {
        // High10 and 4:4:4 exist in the header; mapping them to 8-bit High decodes garbage.
        assert!(profile_for(Codec::H264, 1, 10).is_err());
        assert!(profile_for(Codec::H264, 3, 8).is_err());
        assert!(profile_for(Codec::H265, 3, 10).is_err());
        assert!(rt_format(1, 12).is_err());
        // AV1 API is 8/10-bit 4:2:0 only. chroma_format_idc 0 is monochrome, not 4:2:0.
        assert!(profile_for(Codec::Av1, 3, 8).is_err());
        assert!(profile_for(Codec::Av1, 3, 10).is_err());
        assert!(profile_for(Codec::Av1, 1, 12).is_err());
        assert!(profile_for(Codec::Av1, 0, 8).is_err());
        assert!(profile_for(Codec::Av1, 2, 8).is_err());
    }

    /// Pool from the codec constant; ledger is nine slots, as [`crate::pic_av1::plan_to_va_av1`].
    #[test]
    fn the_av1_pool_is_the_codecs_eight_slots_plus_the_current_picture() {
        assert_eq!(AV1_MAX_DPB_FRAMES, pf_bitstream::av1::NUM_REF_SLOTS);
        assert_eq!(
            surface_count(AV1_MAX_DPB_FRAMES),
            8 + 1 + PRESENTER_HEADROOM
        );
    }

    #[test]
    fn rt_format_tracks_depth() {
        assert_eq!(rt_format(1, 8).unwrap(), VA_RT_FORMAT_YUV420);
        assert_eq!(rt_format(1, 10).unwrap(), VA_RT_FORMAT_YUV420_10);
    }

    #[test]
    fn the_surface_pool_covers_dpb_plus_current_plus_headroom() {
        assert_eq!(surface_count(4), 4 + 1 + PRESENTER_HEADROOM);
        assert_eq!(surface_count(16), 16 + 1 + PRESENTER_HEADROOM);
    }
}
