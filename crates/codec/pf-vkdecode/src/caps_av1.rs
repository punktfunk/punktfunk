//! AV1 decode profile: the key and chain [`crate::caps`] queries and derives
//! against.
//!
//! Film grain is part of the DECODE PROFILE (`filmGrainSupport` beside
//! `stdProfile`). Vulkan identity is BY VALUE, so a grain stream is a different
//! profile from a grain-less one. Passing `VK_FALSE` would present pictures the
//! encoder never intended; the hardware synthesizes grain or the ladder demotes.
//!
//! Picture format follows the stream as in H.265. Monochrome, 4:2:2 and 12-bit
//! are refused before a session — [`crate::OUTPUT_FORMATS`] is the vocabulary.

use ash::vk;
use ash::vk::native as hh;

use crate::caps_h265::output_format_for;
use crate::params_av1::ParamsAv1Error;
use crate::params_av1::STD_PROFILE_HIGH;
use crate::params_av1::STD_PROFILE_MAIN;
use crate::params_av1::STD_PROFILE_PROFESSIONAL;

/// Each field is a `VkVideoProfileInfoKHR` / `VkVideoDecodeAV1ProfileInfoKHR`
/// value. Vulkan identity is BY VALUE, so consumers rebuild the chain from this
/// `Copy` key rather than sharing one wired struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Av1ProfileKey {
    pub std_profile: hh::StdVideoAV1Profile,
    pub chroma_subsampling: vk::VideoChromaSubsamplingFlagsKHR,
    pub luma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    pub chroma_bit_depth: vk::VideoComponentBitDepthFlagsKHR,
    /// Sequence `film_grain_params_present`, not a frame's `apply_grain`.
    /// A session is one profile for its lifetime; a frame cannot apply grain the
    /// sequence never declared (`crate::pic_av1`'s `pFilmGrain` gate requires both).
    pub film_grain: bool,
}

impl Av1ProfileKey {
    /// Bit depth is in bits (8/10/12), not minus8.
    ///
    /// Refused here, before any query: monochrome, 4:2:2, 4:4:0, 12-bit — none have
    /// a two-plane format in [`crate::OUTPUT_FORMATS`].
    pub fn from_stream(
        seq_profile: u8,
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<Self, ParamsAv1Error> {
        let std_profile = match seq_profile {
            0 => STD_PROFILE_MAIN,
            1 => STD_PROFILE_HIGH,
            2 => STD_PROFILE_PROFESSIONAL,
            other => return Err(ParamsAv1Error::UnsupportedProfile(other)),
        };
        let chroma_subsampling = match chroma_format_idc {
            1 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_420,
            3 => vk::VideoChromaSubsamplingFlagsKHR::TYPE_444,
            // Vulkan can express MONOCHROME; this crate still refuses it.
            // Every delivered picture format is two-plane and the presenter
            // samples both. Refused rather than half-supported.
            other => return Err(ParamsAv1Error::UnsupportedChromaFormat(other)),
        };
        let depth = match bit_depth {
            8 => vk::VideoComponentBitDepthFlagsKHR::TYPE_8,
            10 => vk::VideoComponentBitDepthFlagsKHR::TYPE_10,
            other => return Err(ParamsAv1Error::UnsupportedBitDepth(other)),
        };
        // AV1 codes one bit depth for the sequence; there is no chroma depth
        // that can disagree with luma (the H.265 extra clause has no counterpart).
        Ok(Self {
            std_profile,
            chroma_subsampling,
            luma_bit_depth: depth,
            chroma_bit_depth: depth,
            film_grain,
        })
    }

    /// Probe key when sampling/depth/grain are negotiated but the sequence header
    /// has not arrived ([`crate::VkAv1Decoder::probe_stream_support`]).
    ///
    /// Negotiation does not carry `seq_profile`, so it is derived: 4:2:0 → Main,
    /// 4:4:4 → High (a host encodes 4:4:4 as High). Anything else goes to
    /// Professional, which cannot rescue a combination [`Self::from_stream`] refuses.
    pub fn from_negotiated(
        chroma_format_idc: u8,
        bit_depth: u8,
        film_grain: bool,
    ) -> Result<Self, ParamsAv1Error> {
        let seq_profile = match chroma_format_idc {
            1 => 0,
            3 => 1,
            _ => 2,
        };
        Self::from_stream(seq_profile, chroma_format_idc, bit_depth, film_grain)
    }

    /// `None` outside the envelope (unreachable off [`Self::from_stream`]).
    pub fn output_format(&self) -> Option<vk::Format> {
        let ten_bit = self.luma_bit_depth == vk::VideoComponentBitDepthFlagsKHR::TYPE_10;
        let depth_minus8 = if ten_bit { 2 } else { 0 };
        if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_420 {
            output_format_for(1, depth_minus8)
        } else if self.chroma_subsampling == vk::VideoChromaSubsamplingFlagsKHR::TYPE_444 {
            output_format_for(3, depth_minus8)
        } else {
            None
        }
    }
}

/// AV1 decode profile chain — [`crate::caps_h265::H265ProfileChain`]'s twin.
///
/// [`Self::wire`] points `profile.p_next` at this struct's own `av1` field; do
/// not move the value between `wire()` and the last use of the returned reference.
pub(crate) struct Av1ProfileChain {
    av1: vk::VideoDecodeAV1ProfileInfoKHR<'static>,
    /// Decode usage hints between the profile and the codec struct. Optional by the
    /// spec, but Intel's Windows driver walks the chain expecting it and faults on
    /// the first parameters create without it; FFmpeg always chains it.
    usage: vk::VideoDecodeUsageInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl Av1ProfileChain {
    pub(crate) fn new(key: Av1ProfileKey) -> Self {
        Self {
            av1: vk::VideoDecodeAV1ProfileInfoKHR::default()
                .std_profile(key.std_profile)
                // From the SEQUENCE, never forced false to make a query pass:
                // a grain-less profile would decode a grain stream into pictures
                // the encoder never intended (module docs).
                .film_grain_support(key.film_grain),
            usage: vk::VideoDecodeUsageInfoKHR::default(),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_AV1)
                .chroma_subsampling(key.chroma_subsampling)
                .luma_bit_depth(key.luma_bit_depth)
                .chroma_bit_depth(key.chroma_bit_depth),
        }
    }

    /// The root, and any copy of it, borrows `self`; a raw pointer from it must
    /// not outlive `self` in place.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'_> {
        self.usage.p_next = (&self.av1 as *const vk::VideoDecodeAV1ProfileInfoKHR<'_>).cast();
        self.profile.p_next = (&self.usage as *const vk::VideoDecodeUsageInfoKHR<'_>).cast();
        &self.profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::DecodeProfile;
    use crate::caps::NV12;
    use crate::caps::P010;
    use crate::caps::YUV444_10;
    use crate::caps::YUV444_8;

    #[test]
    fn the_profile_is_built_from_the_sequences_sampling_depth_and_grain_flag() {
        let main = Av1ProfileKey::from_stream(0, 1, 8, false).unwrap();
        assert_eq!(main.std_profile, STD_PROFILE_MAIN);
        assert_eq!(
            main.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_420
        );
        assert_eq!(
            main.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_8
        );
        assert_eq!(main.chroma_bit_depth, main.luma_bit_depth);
        assert!(!main.film_grain);
        assert_eq!(main.output_format(), Some(NV12));

        let main10 = Av1ProfileKey::from_stream(0, 1, 10, false).unwrap();
        assert_eq!(
            main10.luma_bit_depth,
            vk::VideoComponentBitDepthFlagsKHR::TYPE_10
        );
        assert_eq!(main10.output_format(), Some(P010));

        let high8 = Av1ProfileKey::from_stream(1, 3, 8, false).unwrap();
        assert_eq!(high8.std_profile, STD_PROFILE_HIGH);
        assert_eq!(
            high8.chroma_subsampling,
            vk::VideoChromaSubsamplingFlagsKHR::TYPE_444
        );
        assert_eq!(high8.output_format(), Some(YUV444_8));
        assert_eq!(
            Av1ProfileKey::from_stream(1, 3, 10, false)
                .unwrap()
                .output_format(),
            Some(YUV444_10)
        );

        let grainy = Av1ProfileKey::from_stream(0, 1, 8, true).unwrap();
        assert_ne!(grainy, main);
        assert!(grainy.film_grain);
        assert_eq!(
            grainy.output_format(),
            main.output_format(),
            "grain changes the profile, never the picture format"
        );
    }

    #[test]
    fn sequence_facts_outside_the_envelope_are_refused_by_the_profile_builder() {
        assert_eq!(
            Av1ProfileKey::from_stream(3, 1, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedProfile(3),
            "there is no AV1 seq_profile 3"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(0, 0, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(0),
            "monochrome has no two-plane picture format here"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(2, 2, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(2),
            "4:2:2 is legal AV1 Professional with no punktfunk output plumbing"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(2, 4, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(4),
            "the planner's 4:4:0 sentinel is refused, not read as 4:4:4"
        );
        assert_eq!(
            Av1ProfileKey::from_stream(2, 1, 12, false).unwrap_err(),
            ParamsAv1Error::UnsupportedBitDepth(12)
        );
    }

    #[test]
    fn the_negotiated_shape_picks_the_profile_a_host_encodes_it_with() {
        assert_eq!(
            Av1ProfileKey::from_negotiated(1, 8, false).unwrap(),
            Av1ProfileKey::from_stream(0, 1, 8, false).unwrap()
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(1, 10, false).unwrap(),
            Av1ProfileKey::from_stream(0, 1, 10, false).unwrap()
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(3, 8, true).unwrap(),
            Av1ProfileKey::from_stream(1, 3, 8, true).unwrap()
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(0, 8, false).unwrap_err(),
            ParamsAv1Error::UnsupportedChromaFormat(0)
        );
        assert_eq!(
            Av1ProfileKey::from_negotiated(1, 12, false).unwrap_err(),
            ParamsAv1Error::UnsupportedBitDepth(12)
        );
    }

    #[test]
    fn the_av1_profile_chain_wires_the_codec_struct_behind_the_root_profile() {
        let key = Av1ProfileKey::from_stream(0, 1, 10, true).unwrap();
        let mut chain = Av1ProfileChain::new(key);
        let profile = chain.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1
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
        // SAFETY: wire() pointed the usage struct's p_next at chain's own av1 field,
        // which lives for this whole scope and is a valid VideoDecodeAV1ProfileInfoKHR.
        let av1 = unsafe { &*usage.p_next.cast::<vk::VideoDecodeAV1ProfileInfoKHR<'_>>() };
        assert_eq!(av1.std_profile, STD_PROFILE_MAIN);
        assert_eq!(
            av1.film_grain_support,
            vk::TRUE,
            "the query the device answers is the GRAIN-enabled one"
        );

        let plain = Av1ProfileKey::from_stream(0, 1, 10, false).unwrap();
        let mut chain = Av1ProfileChain::new(plain);
        let profile = chain.wire();
        // SAFETY: as above — usage struct first, then the chain's own AV1 struct.
        let av1 = unsafe {
            &*(*profile.p_next.cast::<vk::VideoDecodeUsageInfoKHR<'_>>())
                .p_next
                .cast::<vk::VideoDecodeAV1ProfileInfoKHR<'_>>()
        };
        assert_eq!(av1.film_grain_support, vk::FALSE);

        // DecodeProfile must build the same chain; a profile-idc mix-up would
        // show up here.
        let mut erased = DecodeProfile::Av1(key).chain();
        let profile = erased.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_AV1
        );
        // SAFETY: as above — the erased chain wires its own usage and AV1 structs.
        let av1 = unsafe {
            &*(*profile.p_next.cast::<vk::VideoDecodeUsageInfoKHR<'_>>())
                .p_next
                .cast::<vk::VideoDecodeAV1ProfileInfoKHR<'_>>()
        };
        assert_eq!(av1.film_grain_support, vk::TRUE);
    }
}
