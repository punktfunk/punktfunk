//! DRM FourCC codes the host's capture, import and encode paths speak, and the one
//! fourcc → `VkFormat` table. A code is its four-byte name read little-endian.

use ash::vk;

pub const XR24: u32 = u32::from_le_bytes(*b"XR24"); // DRM_FORMAT_XRGB8888
pub const AR24: u32 = u32::from_le_bytes(*b"AR24"); // DRM_FORMAT_ARGB8888
pub const XB24: u32 = u32::from_le_bytes(*b"XB24"); // DRM_FORMAT_XBGR8888
pub const AB24: u32 = u32::from_le_bytes(*b"AB24"); // DRM_FORMAT_ABGR8888
pub const XR30: u32 = u32::from_le_bytes(*b"XR30"); // DRM_FORMAT_XRGB2101010
pub const AR30: u32 = u32::from_le_bytes(*b"AR30"); // DRM_FORMAT_ARGB2101010
pub const XB30: u32 = u32::from_le_bytes(*b"XB30"); // DRM_FORMAT_XBGR2101010
pub const AB30: u32 = u32::from_le_bytes(*b"AB30"); // DRM_FORMAT_ABGR2101010
/// One buffer, Y then interleaved UV.
pub const NV12: u32 = u32::from_le_bytes(*b"NV12");
/// NV12 at 16 bits a sample, the 10-bit code high.
pub const P010: u32 = u32::from_le_bytes(*b"P010");

/// The VkFormat whose colour components match `fourcc`; Vulkan does the byte swizzle.
/// The 10-bit DRM word is Vulkan's PACK32 layout: XR30 has R in bits 20..29
/// (`A2R10G10B10`, optional in Vulkan: a device without it drops XR30 from the capture
/// offer), XB30 in bits 0..9 (`A2B10G10R10`). NV12 and P010 map to their two-plane
/// formats; a consumer that samples packed RGB only refuses them itself.
pub fn vk_format(fourcc: u32) -> Option<vk::Format> {
    Some(match fourcc {
        XR24 | AR24 => vk::Format::B8G8R8A8_UNORM,
        XB24 | AB24 => vk::Format::R8G8B8A8_UNORM,
        XR30 | AR30 => vk::Format::A2R10G10B10_UNORM_PACK32,
        XB30 | AB30 => vk::Format::A2B10G10R10_UNORM_PACK32,
        NV12 => vk::Format::G8_B8R8_2PLANE_420_UNORM,
        P010 => vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's `fourcc_code` values; a typo in a name would move them.
    #[test]
    fn codes_are_the_kernel_values() {
        assert_eq!(XR24, 0x3432_5258);
        assert_eq!(AR24, 0x3432_5241);
        assert_eq!(XR30, 0x3033_5258);
        assert_eq!(XB30, 0x3033_4258);
        assert_eq!(NV12, 0x3231_564e);
        assert_eq!(P010, 0x3031_3050);
    }

    #[test]
    fn vk_format_matches_the_channel_order() {
        assert_eq!(vk_format(XR24), Some(vk::Format::B8G8R8A8_UNORM));
        assert_eq!(vk_format(AB24), Some(vk::Format::R8G8B8A8_UNORM));
        assert_eq!(vk_format(AR30), Some(vk::Format::A2R10G10B10_UNORM_PACK32));
        assert_eq!(vk_format(XB30), Some(vk::Format::A2B10G10R10_UNORM_PACK32));
        assert_eq!(vk_format(NV12), Some(vk::Format::G8_B8R8_2PLANE_420_UNORM));
        assert_eq!(vk_format(0), None);
    }
}
