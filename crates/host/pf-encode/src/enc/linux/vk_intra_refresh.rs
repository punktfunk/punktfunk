//! Vendored `VK_KHR_video_encode_intra_refresh` (extension 553) bindings. Pinned `ash`
//! predates the extension; layouts are copied from the registry and chained via raw
//! `p_next`, same pattern as [`vk_valve_rgb`](super::vk_valve_rgb). The extension adds no
//! commands: five structs and one encode flag bit.
//!
//! Consumed by `vulkan_video.rs`. Evidence: `design/vulkan-intra-refresh.md`.
#![allow(dead_code)]

use ash::vk;
use std::ffi::{c_void, CStr};

pub const EXTENSION_NAME: &CStr = c"VK_KHR_video_encode_intra_refresh";

// VkStructureType — construct via `stype`.
pub const ST_CAPABILITIES: i32 = 1_000_552_000;
pub const ST_SESSION_CREATE_INFO: i32 = 1_000_552_001;
pub const ST_INFO: i32 = 1_000_552_002;
pub const ST_REFERENCE_INFO: i32 = 1_000_552_003;
pub const ST_PHYSICAL_DEVICE_FEATURES: i32 = 1_000_552_004;

// `VkVideoEncodeIntraRefreshModeFlagBitsKHR`
pub const MODE_PER_PICTURE_PARTITION: u32 = 0x01;
pub const MODE_BLOCK_BASED: u32 = 0x02;
pub const MODE_BLOCK_ROW_BASED: u32 = 0x04;
pub const MODE_BLOCK_COLUMN_BASED: u32 = 0x08;

/// `VK_VIDEO_ENCODE_INTRA_REFRESH_BIT_KHR`: bit 2 of `VkVideoEncodeFlagBitsKHR`.
pub const ENCODE_INTRA_REFRESH_BIT: u32 = 0x04;

/// `VkPhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR` — chain into
/// `VkPhysicalDeviceFeatures2` (query) / `VkDeviceCreateInfo` (enable).
#[repr(C)]
pub struct PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub video_encode_intra_refresh: vk::Bool32,
}

/// `VkVideoEncodeIntraRefreshCapabilitiesKHR` — chain into `VkVideoCapabilitiesKHR`.
#[repr(C)]
pub struct VideoEncodeIntraRefreshCapabilitiesKHR {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub intra_refresh_modes: u32,
    pub max_intra_refresh_cycle_duration: u32,
    pub max_intra_refresh_active_reference_pictures: u32,
    pub partition_independent_intra_refresh_regions: vk::Bool32,
    pub non_rectangular_intra_refresh_regions: vk::Bool32,
}

/// `VkVideoEncodeSessionIntraRefreshCreateInfoKHR` — chain into `VkVideoSessionCreateInfoKHR`.
/// The mode is fixed for the session; a frame opts in with [`ENCODE_INTRA_REFRESH_BIT`].
#[repr(C)]
pub struct VideoEncodeSessionIntraRefreshCreateInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub intra_refresh_mode: u32,
}

/// `VkVideoEncodeIntraRefreshInfoKHR` — chain into `VkVideoEncodeInfoKHR` on a frame carrying
/// [`ENCODE_INTRA_REFRESH_BIT`]. `index` runs `0..cycle`.
#[repr(C)]
pub struct VideoEncodeIntraRefreshInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub intra_refresh_cycle_duration: u32,
    pub intra_refresh_index: u32,
}

/// `VkVideoReferenceIntraRefreshInfoKHR` — chain into a `VkVideoReferenceSlotInfoKHR` of
/// `VkVideoEncodeInfoKHR::pReferenceSlots`. Must equal `cycle - index` of the current frame;
/// absent means 0, which a frame without the refresh bit requires.
#[repr(C)]
pub struct VideoReferenceIntraRefreshInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub dirty_intra_refresh_regions: u32,
}

#[inline]
pub fn stype(raw: i32) -> vk::StructureType {
    vk::StructureType::from_raw(raw)
}

// Const ABI checks (not `#[cfg(test)]`): a field edit is otherwise silent
// through raw `p_next`. Duplicated per vendor module on purpose.
macro_rules! assert_abi_layout {
    ($t:ty { size: $size:expr, align: $align:expr $(, $field:ident @ $off:expr)* $(,)? }) => {
        const _: () = {
            assert!(
                ::core::mem::size_of::<$t>() == $size,
                concat!(stringify!($t), ": size does not match the C ABI")
            );
            assert!(
                ::core::mem::align_of::<$t>() == $align,
                concat!(stringify!($t), ": alignment does not match the C ABI")
            );
            $(assert!(
                ::core::mem::offset_of!($t, $field) == $off,
                concat!(stringify!($t), ".", stringify!($field), ": offset does not match the C ABI")
            );)*
        };
    };
}

assert_abi_layout!(PhysicalDeviceVideoEncodeIntraRefreshFeaturesKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    video_encode_intra_refresh @ 16,
});

assert_abi_layout!(VideoEncodeIntraRefreshCapabilitiesKHR {
    size: 40, align: 8,
    s_type @ 0,
    p_next @ 8,
    intra_refresh_modes @ 16,
    max_intra_refresh_cycle_duration @ 20,
    max_intra_refresh_active_reference_pictures @ 24,
    partition_independent_intra_refresh_regions @ 28,
    non_rectangular_intra_refresh_regions @ 32,
});

assert_abi_layout!(VideoEncodeSessionIntraRefreshCreateInfoKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    intra_refresh_mode @ 16,
});

assert_abi_layout!(VideoEncodeIntraRefreshInfoKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    intra_refresh_cycle_duration @ 16,
    intra_refresh_index @ 20,
});

assert_abi_layout!(VideoReferenceIntraRefreshInfoKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    dirty_intra_refresh_regions @ 16,
});
