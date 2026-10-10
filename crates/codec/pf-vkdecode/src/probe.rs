//! Verbatim physical-device video-format probe behind `punktfunk-session --probe-decode`.
//!
//! For every profile the client can negotiate, asks the same question the
//! session caps query asks, in every usage the image pools would create with,
//! and records the answer — including failures as `VkResult`. Shares
//! [`crate::caps::query_formats_on`] with the real caps path; a private copy
//! would eventually disagree with the code it explains.
//!
//! A driver may ignore the requested `imageUsage` and still return an entry
//! whose envelope omits those bits. Empty list is the spec refusal
//! (`ERROR_FORMAT_NOT_SUPPORTED` / `ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR`).
//!
//! [`UsageProbe::image_format_support`] asks `vkGetPhysicalDeviceImageFormatProperties2`
//! with the same profile list. Reported, not authority: that entry point does
//! not fully honour the chained video profile list, so "creatable" is not
//! permission to sample a decode picture.
//! `vkGetPhysicalDeviceVideoFormatPropertiesKHR` is the authority; pin against
//! `vulkaninfo --show-video-props`.

use ash::vk;
use ash::vk::native as hh;

use crate::caps::query_formats_on;
use crate::caps::DecodeProfile;
use crate::caps::VideoFormat;
use crate::caps::COINCIDE_USAGE;
use crate::caps::DPB_USAGE;
use crate::caps::NV12;
use crate::caps::OUTPUT_USAGE;
use crate::caps::P010;
use crate::caps_av1::Av1ProfileKey;
use crate::caps_h265::H265ProfileKey;

/// Usage combinations, widest first. First three match [`crate::images`].
/// The rest split a refusal: decode roles vs sampling vs destination alone.
const USAGE_MATRIX: [(&str, vk::ImageUsageFlags); 6] = [
    ("coincide DPB|DST|SAMPLED", COINCIDE_USAGE),
    ("distinct DPB", DPB_USAGE),
    ("distinct DST|SAMPLED", OUTPUT_USAGE),
    ("DPB|DST (no SAMPLED)", DECODE_ONLY_USAGE),
    ("SAMPLED alone", vk::ImageUsageFlags::SAMPLED),
    ("DST alone", vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR),
];

/// Discriminator: decode roles work, sampling does not.
const DECODE_ONLY_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR.as_raw()
        | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR.as_raw(),
);

#[derive(Debug, Clone)]
pub struct ProfileProbe {
    pub profile: &'static str,
    /// Picture format derivation will look for.
    pub wanted: vk::Format,
    /// One row per [`USAGE_MATRIX`] entry, in that order.
    pub usages: Vec<UsageProbe>,
}

#[derive(Debug, Clone)]
pub struct UsageProbe {
    pub label: &'static str,
    pub usage: vk::ImageUsageFlags,
    /// Video-format query: entries, or the `VkResult`. Empty `Ok` is the spec
    /// refusal (`ERROR_FORMAT_NOT_SUPPORTED` / `ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR`).
    pub formats: Result<Vec<VideoFormat>, vk::Result>,
    /// Same profile list via `vkGetPhysicalDeviceImageFormatProperties2`.
    /// `Ok` is not permission to create a video image: this entry point does
    /// not fully honour the chained profile list.
    pub image_format_support: Result<(), vk::Result>,
}

impl UsageProbe {
    pub fn wanted_entry(&self, wanted: vk::Format) -> Option<VideoFormat> {
        self.formats
            .as_ref()
            .ok()?
            .iter()
            .copied()
            .find(|f| f.format == wanted)
    }
}

/// Negotiable rungs only, including 10-bit (a different Vulkan profile from 8-bit).
/// Exhaustive sweep is `vulkaninfo --show-video-props`.
fn probed_profiles() -> Vec<(&'static str, DecodeProfile, vk::Format)> {
    // Same `from_negotiated` as the decoders; a refused combination drops out.
    // 4:2:0 is chroma_format_idc 1; H.265 depth is `bit_depth_luma_minus8`, AV1 whole bits.
    let mut out: Vec<(&'static str, DecodeProfile, vk::Format)> = vec![(
        "H.264 High 4:2:0 8-bit",
        DecodeProfile::H264(hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH),
        NV12,
    )];
    if let Ok(key) = H265ProfileKey::from_negotiated(1, 0) {
        out.push(("H.265 Main 4:2:0 8-bit", DecodeProfile::H265(key), NV12));
    }
    if let Ok(key) = H265ProfileKey::from_negotiated(1, 2) {
        out.push(("H.265 Main 10 4:2:0 10-bit", DecodeProfile::H265(key), P010));
    }
    // `filmGrainSupport` is part of the Vulkan profile; hosts encode grain-less.
    if let Ok(key) = Av1ProfileKey::from_negotiated(1, 8, false) {
        out.push(("AV1 Main 4:2:0 8-bit", DecodeProfile::Av1(key), NV12));
    }
    if let Ok(key) = Av1ProfileKey::from_negotiated(1, 10, false) {
        out.push(("AV1 Main 4:2:0 10-bit", DecodeProfile::Av1(key), P010));
    }
    out
}

/// One physical device, every matrix question, every probed profile.
/// A refusal is a row, not an error return.
///
/// # Safety
///
/// `instance` must be a live `VkInstance` created through `entry`, and
/// `physical_device` one of its physical devices. Nothing here creates or destroys
/// anything; every call is a physical-device query.
pub unsafe fn probe_video_formats(
    entry: &ash::Entry,
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
) -> Vec<ProfileProbe> {
    let video_queue_instance = ash::khr::video_queue::Instance::new(entry, instance);
    probed_profiles()
        .into_iter()
        .map(|(profile, decode_profile, wanted)| {
            let usages = USAGE_MATRIX
                .iter()
                .map(|(label, usage)| {
                    // SAFETY: caller contract — live instance the video_queue table was
                    // loaded from, and one of its physical devices.
                    let formats = unsafe {
                        query_formats_on(
                            &video_queue_instance,
                            physical_device,
                            decode_profile,
                            *usage,
                        )
                    };
                    // SAFETY: as above.
                    let image_format_support = unsafe {
                        image_format_supported(
                            instance,
                            physical_device,
                            decode_profile,
                            wanted,
                            *usage,
                        )
                    };
                    UsageProbe {
                        label,
                        usage: *usage,
                        formats,
                        image_format_support,
                    }
                })
                .collect();
            ProfileProbe {
                profile,
                wanted,
                usages,
            }
        })
        .collect()
}

/// Image-format query for (`format`, `usage`) plus the profile list.
/// Distinct entry point from the video-format query; not authority (module docs).
///
/// # Safety
///
/// As [`probe_video_formats`].
unsafe fn image_format_supported(
    instance: &ash::Instance,
    physical_device: vk::PhysicalDevice,
    decode_profile: DecodeProfile,
    format: vk::Format,
    usage: vk::ImageUsageFlags,
) -> Result<(), vk::Result> {
    let mut chain = decode_profile.chain();
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(usage)
        .push_next(&mut profile_list);
    let mut props = vk::ImageFormatProperties2::default();
    // SAFETY: caller contract (live instance + one of its physical devices); `info`
    // roots a wired chain of locals that outlive the call, and `props` is a local the
    // driver fills.
    unsafe {
        instance.get_physical_device_image_format_properties2(physical_device, &info, &mut props)
    }
}

/// `NAME|NAME (0xHEX)`. Unnamed bits stay in the string; they must not vanish.
pub fn describe_usage(usage: vk::ImageUsageFlags) -> String {
    // Encode bits appear on decode pictures; leaving them unnamed looks like a stale tool.
    const BITS: [(vk::ImageUsageFlags, &str); 12] = [
        (vk::ImageUsageFlags::TRANSFER_SRC, "TRANSFER_SRC"),
        (vk::ImageUsageFlags::TRANSFER_DST, "TRANSFER_DST"),
        (vk::ImageUsageFlags::SAMPLED, "SAMPLED"),
        (vk::ImageUsageFlags::STORAGE, "STORAGE"),
        (vk::ImageUsageFlags::COLOR_ATTACHMENT, "COLOR_ATTACHMENT"),
        (vk::ImageUsageFlags::INPUT_ATTACHMENT, "INPUT_ATTACHMENT"),
        (vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR, "DECODE_DST"),
        (vk::ImageUsageFlags::VIDEO_DECODE_SRC_KHR, "DECODE_SRC"),
        (vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR, "DECODE_DPB"),
        (vk::ImageUsageFlags::VIDEO_ENCODE_DST_KHR, "ENCODE_DST"),
        (vk::ImageUsageFlags::VIDEO_ENCODE_SRC_KHR, "ENCODE_SRC"),
        (vk::ImageUsageFlags::VIDEO_ENCODE_DPB_KHR, "ENCODE_DPB"),
    ];
    describe_mask(usage.as_raw(), &BITS.map(|(f, n)| (f.as_raw(), n)))
}

/// `imageCreateFlags` as `NAME|NAME (0xHEX)`, same rule as [`describe_usage`].
pub fn describe_create_flags(flags: vk::ImageCreateFlags) -> String {
    const BITS: [(vk::ImageCreateFlags, &str); 5] = [
        (vk::ImageCreateFlags::MUTABLE_FORMAT, "MUTABLE_FORMAT"),
        (vk::ImageCreateFlags::EXTENDED_USAGE, "EXTENDED_USAGE"),
        (vk::ImageCreateFlags::ALIAS, "ALIAS"),
        (vk::ImageCreateFlags::DISJOINT, "DISJOINT"),
        (vk::ImageCreateFlags::PROTECTED, "PROTECTED"),
    ];
    describe_mask(flags.as_raw(), &BITS.map(|(f, n)| (f.as_raw(), n)))
}

/// Named bits `|`-joined, then the raw value; leftover bits stay as `unrecognised`.
fn describe_mask(raw: u32, bits: &[(u32, &str)]) -> String {
    if raw == 0 {
        return "(none) (0x0)".to_string();
    }
    let mut names: Vec<&str> = bits
        .iter()
        .filter(|(bit, _)| raw & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    let named: u32 = bits.iter().map(|(bit, _)| bit).sum();
    let leftover = raw & !named;
    let extra;
    if leftover != 0 {
        extra = format!("unrecognised 0x{leftover:X}");
        names.push(&extra);
    }
    format!("{} (0x{raw:X})", names.join("|"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_matrix_covers_every_usage_the_pools_create_with() {
        for real in [COINCIDE_USAGE, DPB_USAGE, OUTPUT_USAGE] {
            assert!(
                USAGE_MATRIX.iter().any(|(_, u)| *u == real),
                "{real:?} is created by the image pools but never probed"
            );
        }
        assert!(USAGE_MATRIX.iter().any(|(_, u)| *u == DECODE_ONLY_USAGE));
        assert!(USAGE_MATRIX
            .iter()
            .any(|(_, u)| *u == vk::ImageUsageFlags::SAMPLED));
    }

    #[test]
    fn every_probed_profile_names_the_format_derivation_wants() {
        let profiles = probed_profiles();
        assert!(
            profiles.len() >= 5,
            "expected H.264 + H.265 8/10-bit + AV1 8/10-bit, got {}",
            profiles.len()
        );
        for (name, _, wanted) in &profiles {
            assert!(
                crate::caps::OUTPUT_FORMATS.contains(wanted),
                "{name} wants {wanted:?}, which is outside this crate's output vocabulary"
            );
        }
        assert!(profiles.iter().any(|(_, _, w)| *w == P010), "no 10-bit leg");
    }

    #[test]
    fn mask_descriptions_account_for_every_bit_they_print() {
        assert_eq!(
            describe_usage(COINCIDE_USAGE),
            "SAMPLED|DECODE_DST|DECODE_DPB (0x1404)"
        );
        assert_eq!(describe_usage(vk::ImageUsageFlags::empty()), "(none) (0x0)");
        let intel = vk::ImageUsageFlags::TRANSFER_SRC
            | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
            | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
        assert_eq!(
            describe_usage(intel),
            "TRANSFER_SRC|DECODE_DST|DECODE_DPB (0x1401)"
        );
        let unknown = vk::ImageUsageFlags::from_raw(0x8000_0000);
        assert_eq!(
            describe_usage(unknown),
            "unrecognised 0x80000000 (0x80000000)"
        );
        assert_eq!(
            describe_create_flags(
                vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE
            ),
            "MUTABLE_FORMAT|EXTENDED_USAGE (0x108)"
        );
        assert_eq!(
            describe_create_flags(vk::ImageCreateFlags::empty()),
            "(none) (0x0)"
        );
    }

    #[test]
    fn the_wanted_entry_is_found_by_format_among_others() {
        let probe = UsageProbe {
            label: "x",
            usage: COINCIDE_USAGE,
            formats: Ok(vec![
                VideoFormat {
                    format: P010,
                    image_usage: COINCIDE_USAGE,
                    ..Default::default()
                },
                VideoFormat {
                    format: NV12,
                    image_usage: DPB_USAGE,
                    ..Default::default()
                },
            ]),
            image_format_support: Ok(()),
        };
        assert_eq!(probe.wanted_entry(NV12).unwrap().image_usage, DPB_USAGE);
        assert!(probe.wanted_entry(crate::caps::YUV444_8).is_none());
        // Failed query: no entry. Distinct from "returned nothing".
        let failed = UsageProbe {
            formats: Err(vk::Result::ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR),
            ..probe
        };
        assert!(failed.wanted_entry(NV12).is_none());
    }
}
