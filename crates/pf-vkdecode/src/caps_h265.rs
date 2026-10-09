//! H.265 decode profile: the key and chain [`crate::caps`] queries and derives
//! against.
//!
//! Picture format is the stream's, not a constant. SPS chroma and bit depth
//! (Main → NV12, Main 10 → P010, RExt 4:4:4 → the two-plane 4:4:4 formats) also
//! fill the `VkVideoProfileInfoKHR` every session object is created against.
//! Both come from one [`H265ProfileKey`]. A device that cannot host the
//! combination is refused before a session exists, so the ladder demotes with a
//! named reason rather than creating images the driver never advertised.

use ash::vk;
use ash::vk::native as hh;

use crate::caps::NV12;
use crate::caps::P010;
use crate::caps::YUV444_10;
use crate::caps::YUV444_8;
use crate::params_h265::profile_to_std;
use crate::params_h265::H265ParamsError;

/// Stream facts that fill `VkVideoProfileInfoKHR`.
///
/// Profile identity in Vulkan is by value across the caps query, the session,
/// every profile-listed image/buffer and the query pool. Each consumer rebuilds
/// a structurally identical chain from this `Copy` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H265ProfileKey {
    /// The four Vulkan expresses: Main (1), Main 10 (2), Main Still Picture (3), RExt (4).
    pub std_profile_idc: hh::StdVideoH265ProfileIdc,
    pub chroma_subsampling: vk::VideoChromaSubsamplingFlagsKHR,
    pub luma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    pub chroma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
}

impl H265ProfileKey {
    /// Build the key from one picture's SPS facts.
    ///
    /// The envelope is the same gate [`crate::params_h265`] applies — 4:2:0 or
    /// 4:4:4, no separate colour planes, 8 or 10 bits, luma depth == chroma depth
    /// — because the caps query needs a profile before any parameter-set
    /// conversion runs. A narrower copy here would drift and hand the driver a
    /// profile the SPS cannot match.
    pub fn from_stream(
        general_profile_idc: u8,
        chroma_format_idc: u8,
        separate_colour_plane_flag: bool,
        bit_depth_luma_minus8: u8,
        bit_depth_chroma_minus8: u8,
    ) -> Result<Self, H265ParamsError> {
        let std_profile_idc = profile_to_std(general_profile_idc)?;
        let chroma_subsampling = match chroma_format_idc {
            1 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
            3 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
            0 | 2 => {
                return Err(H265ParamsError::UnsupportedChromaFormat(chroma_format_idc));
            }
            other => return Err(H265ParamsError::InvalidChromaFormatIdc(other)),
        };
        // 4:4:4 with separate colour planes is ChromaArrayType 0: three
        // monochrome planes. `TYPE_444` would mis-state the bitstream.
        if chroma_format_idc == 3 && separate_colour_plane_flag {
            return Err(H265ParamsError::SeparateColourPlanes);
        }
        if bit_depth_luma_minus8 != bit_depth_chroma_minus8
            || !matches!(bit_depth_luma_minus8, 0 | 2)
        {
            return Err(H265ParamsError::UnsupportedBitDepth {
                luma_minus8: bit_depth_luma_minus8,
                chroma_minus8: bit_depth_chroma_minus8,
            });
        }
        let depth = if bit_depth_luma_minus8 == 0 {
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        } else {
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        };
        Ok(Self {
            std_profile_idc,
            chroma_subsampling,
            luma_bit_depth: depth,
            chroma_bit_depth: depth,
        })
    }

    /// Key for a stream whose chroma/depth the session already negotiated, before
    /// any SPS ([`crate::VkH265Decoder::probe_stream_support`]).
    ///
    /// Profile idc is not in that pair, so it is derived: 4:2:0 8-bit → Main,
    /// 4:2:0 10-bit → Main 10, 4:4:4 → RExt (4:4:4 is only RExt). Once an SPS
    /// arrives, [`Self::from_stream`] is the authority; this path never admits a
    /// combination that gate refuses.
    pub fn from_negotiated(
        chroma_format_idc: u8,
        bit_depth_luma_minus8: u8,
    ) -> Result<Self, H265ParamsError> {
        let general_profile_idc = match (chroma_format_idc, bit_depth_luma_minus8) {
            (1, 0) => 1,
            (1, 2) => 2,
            (3, _) => 4,
            // Outside the envelope: a profile idc that cannot rescue it, so
            // `from_stream` is the one gate that produces the error.
            _ => 4,
        };
        Self::from_stream(
            general_profile_idc,
            chroma_format_idc,
            false,
            bit_depth_luma_minus8,
            bit_depth_luma_minus8,
        )
    }

    /// `None` is outside the envelope [`Self::from_stream`] already refused.
    pub fn output_format(&self) -> Option<vk::Format> {
        let ten_bit = self.luma_bit_depth == vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
        if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_420 {
            Some(if ten_bit { P010 } else { NV12 })
        } else if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_444 {
            Some(if ten_bit { YUV444_10 } else { YUV444_8 })
        } else {
            None
        }
    }
}

/// Same mapping as [`H265ProfileKey::output_format`], without building a key.
pub fn output_format_for(chroma_format_idc: u8, bit_depth_luma_minus8: u8) -> Option<vk::Format> {
    match (chroma_format_idc, bit_depth_luma_minus8) {
        (1, 0) => Some(NV12),
        (1, 2) => Some(P010),
        (3, 0) => Some(YUV444_8),
        (3, 2) => Some(YUV444_10),
        _ => None,
    }
}

/// One H.265 decode profile chain. [`Self::wire`] points `profile.p_next` at this
/// struct's own `h265` field; do not move the value between `wire()` and the last
/// use of the returned reference.
pub(crate) struct H265ProfileChain {
    h265: vk::VideoDecodeH265ProfileInfoKHR<'static>,
    /// Decode usage hints between the profile and the codec struct. Optional by the
    /// spec, but Intel's Windows driver walks the chain expecting it and faults on
    /// the first parameters create without it; FFmpeg always chains it.
    usage: vk::VideoDecodeUsageInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl H265ProfileChain {
    pub(crate) fn new(key: H265ProfileKey) -> Self {
        Self {
            h265: vk::VideoDecodeH265ProfileInfoKHR::default().std_profile_idc(key.std_profile_idc),
            usage: vk::VideoDecodeUsageInfoKHR::default(),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H265)
                .chroma_subsampling(key.chroma_subsampling)
                .luma_bit_depth(key.luma_bit_depth)
                .chroma_bit_depth(key.chroma_bit_depth),
        }
    }

    /// Wire the internal `p_next` chain and hand out the profile root. The root, and
    /// any copy of it, borrows `self`; a raw pointer from it must not outlive `self`
    /// in place.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'_> {
        self.usage.p_next = (&self.h265 as *const vk::VideoDecodeH265ProfileInfoKHR<'_>).cast();
        self.profile.p_next = (&self.usage as *const vk::VideoDecodeUsageInfoKHR<'_>).cast();
        &self.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::DecodeProfile;

    #[test]
    fn the_profile_is_built_from_the_streams_chroma_format_and_bit_depth() {
        let main = H265ProfileKey::from_stream(1, 1, false, 0, 0).unwrap();
        assert_eq!(
            main.std_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN
        );
        assert_eq!(
            main.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
        );
        assert_eq!(
            main.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        );
        assert_eq!(main.output_format(), Some(NV12));

        let main10 = H265ProfileKey::from_stream(2, 1, false, 2, 2).unwrap();
        assert_eq!(
            main10.std_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );
        assert_eq!(
            main10.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert_eq!(main10.chroma_bit_depth, main10.luma_bit_depth);
        assert_eq!(main10.output_format(), Some(P010));

        let rext8 = H265ProfileKey::from_stream(4, 3, false, 0, 0).unwrap();
        assert_eq!(
            rext8.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
        );
        assert_eq!(rext8.output_format(), Some(YUV444_8));
        let rext10 = H265ProfileKey::from_stream(4, 3, false, 2, 2).unwrap();
        assert_eq!(rext10.output_format(), Some(YUV444_10));

        for (chroma, depth, format) in [
            (1u8, 0u8, NV12),
            (1, 2, P010),
            (3, 0, YUV444_8),
            (3, 2, YUV444_10),
        ] {
            assert_eq!(output_format_for(chroma, depth), Some(format));
        }
        assert_eq!(output_format_for(2, 0), None, "4:2:2 has no output format");
    }

    #[test]
    fn the_negotiated_pair_picks_the_profile_a_host_encodes_it_with() {
        let main = H265ProfileKey::from_negotiated(1, 0).unwrap();
        assert_eq!(
            main,
            H265ProfileKey::from_stream(1, 1, false, 0, 0).unwrap()
        );
        assert_eq!(main.output_format(), Some(NV12));

        let main10 = H265ProfileKey::from_negotiated(1, 2).unwrap();
        assert_eq!(
            main10,
            H265ProfileKey::from_stream(2, 1, false, 2, 2).unwrap()
        );
        assert_eq!(main10.output_format(), Some(P010));

        let rext8 = H265ProfileKey::from_negotiated(3, 0).unwrap();
        assert_eq!(
            rext8,
            H265ProfileKey::from_stream(4, 3, false, 0, 0).unwrap()
        );
        assert_eq!(rext8.output_format(), Some(YUV444_8));
        let rext10 = H265ProfileKey::from_negotiated(3, 2).unwrap();
        assert_eq!(rext10.output_format(), Some(YUV444_10));

        assert_eq!(
            H265ProfileKey::from_negotiated(2, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(2)
        );
        assert_eq!(
            H265ProfileKey::from_negotiated(0, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(0)
        );
        assert_eq!(
            H265ProfileKey::from_negotiated(1, 4).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 4,
                chroma_minus8: 4
            }
        );
    }

    #[test]
    fn stream_facts_outside_the_envelope_are_refused_by_the_profile_builder() {
        assert_eq!(
            H265ProfileKey::from_stream(9, 1, false, 0, 0).unwrap_err(),
            H265ParamsError::UnmappableProfileIdc(9),
            "High Throughput/SCC profiles have no Vulkan code point"
        );
        assert_eq!(
            H265ProfileKey::from_stream(1, 2, false, 0, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(2),
            "4:2:2 is legal H.265 with no punktfunk output plumbing"
        );
        assert_eq!(
            H265ProfileKey::from_stream(1, 0, false, 0, 0).unwrap_err(),
            H265ParamsError::UnsupportedChromaFormat(0)
        );
        assert_eq!(
            H265ProfileKey::from_stream(1, 4, false, 0, 0).unwrap_err(),
            H265ParamsError::InvalidChromaFormatIdc(4)
        );
        // Same envelope as `params_h265`: separate planes at 4:4:4 are
        // ChromaArrayType 0, not interleaved 4:4:4. This `pub` constructor is
        // reachable without the planner.
        assert_eq!(
            H265ProfileKey::from_stream(4, 3, true, 0, 0).unwrap_err(),
            H265ParamsError::SeparateColourPlanes
        );
        // The flag is only defined at 4:4:4 (7.4.3.2.1); it must not disturb 4:2:0.
        assert!(H265ProfileKey::from_stream(1, 1, true, 0, 0).is_ok());
        assert_eq!(
            H265ProfileKey::from_stream(4, 1, false, 4, 4).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 4,
                chroma_minus8: 4
            },
            "12-bit has no output format"
        );
        assert_eq!(
            H265ProfileKey::from_stream(4, 1, false, 0, 2).unwrap_err(),
            H265ParamsError::UnsupportedBitDepth {
                luma_minus8: 0,
                chroma_minus8: 2
            },
            "disagreeing luma/chroma depths have no output format"
        );
    }

    #[test]
    fn the_h265_profile_chain_wires_the_codec_struct_behind_the_root_profile() {
        let key = H265ProfileKey::from_stream(2, 1, false, 2, 2).unwrap();
        let mut chain = H265ProfileChain::new(key);
        let profile = chain.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_H265
        );
        assert_eq!(
            profile.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
        );
        assert_eq!(
            profile.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert!(!profile.p_next.is_null());
        // SAFETY: wire() pointed p_next at chain's own usage field, which lives for
        // this whole scope and is a valid VideoDecodeUsageInfoKHR.
        let usage = unsafe { &*profile.p_next.cast::<vk::VideoDecodeUsageInfoKHR<'_>>() };
        assert_eq!(usage.s_type, vk::StructureType::VIDEO_DECODE_USAGE_INFO_KHR);
        assert_eq!(
            usage.video_usage_hints,
            vk::VideoDecodeUsageFlagsKHR::DEFAULT
        );
        // SAFETY: wire() pointed the usage struct's p_next at chain's own h265 field,
        // which lives for this whole scope and is a valid VideoDecodeH265ProfileInfoKHR.
        let h265 = unsafe { &*usage.p_next.cast::<vk::VideoDecodeH265ProfileInfoKHR<'_>>() };
        assert_eq!(
            h265.std_profile_idc,
            hh::StdVideoH265ProfileIdc_STD_VIDEO_H265_PROFILE_IDC_MAIN_10
        );

        // Same chain via the type-erased dispatch — an H.264 idc cannot build this.
        let mut erased = DecodeProfile::H265(key).chain();
        let profile = erased.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_H265
        );
    }
}
