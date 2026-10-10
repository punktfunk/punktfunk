//! Vendored `VK_KHR_video_encode_av1` encode structs, `StdVideoEncodeAV1*` types, and
//! `VkStructureType` constants that pinned `ash 0.38.0+1.3.281` does not ship (the
//! extension landed in Vulkan 1.3.290). Copied from ash-master generated code and
//! chained into ash's generic video-encode-queue calls via raw `p_next`.
//!
//! Do not bump `ash` to git-master for these: it drops the lifetime on
//! `vk::AllocationCallbacks`, and `sdl3-sys`'s `ash` feature still generates
//! `AllocationCallbacks<'static>`, so the presenter's SDL/Vulkan surface path will
//! not compile. Common AV1 types (`StdVideoAV1*` sequence/tile/quant/…) already
//! live in `ash::vk::native` from decode. Layout is pinned by the `const` ABI
//! assertions below and `abi_tests` for bitfields. Delete this module once `ash`
//! publishes 1.4.x and `sdl3-sys` regenerates.
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use ash::vk;
use ash::vk::native::{
    StdVideoAV1FrameType, StdVideoAV1InterpolationFilter, StdVideoAV1Level, StdVideoAV1Profile,
    StdVideoAV1SequenceHeader, StdVideoAV1TxMode,
};
use std::ffi::{c_void, CStr};

pub const EXTENSION_NAME: &CStr = c"VK_KHR_video_encode_av1";

pub const ST_CAPABILITIES: i32 = 1_000_513_000;
pub const ST_SESSION_PARAMETERS_CREATE_INFO: i32 = 1_000_513_001;
pub const ST_PICTURE_INFO: i32 = 1_000_513_002;
pub const ST_DPB_SLOT_INFO: i32 = 1_000_513_003;
pub const ST_PHYSICAL_DEVICE_FEATURES: i32 = 1_000_513_004;
pub const ST_PROFILE_INFO: i32 = 1_000_513_005;
pub const ST_RATE_CONTROL_INFO: i32 = 1_000_513_006;
pub const ST_RATE_CONTROL_LAYER_INFO: i32 = 1_000_513_007;
pub const ST_SESSION_CREATE_INFO: i32 = 1_000_513_009;
pub const ST_GOP_REMAINING_FRAME_INFO: i32 = 1_000_513_010;

/// Bit 18 of `VkVideoCodecOperationFlagBitsKHR`.
pub const VIDEO_CODEC_OPERATION_ENCODE_AV1: u32 = 0x0004_0000;
/// LAST..ALTREF — the seven inter reference names.
pub const MAX_VIDEO_AV1_REFERENCES_PER_FRAME: usize = 7;
/// Sentinel past LAST..ALTREF: inherit no CDF/context from any reference.
pub const PRIMARY_REF_NONE: u8 = 7;
/// Bit 1 of the superblock-size flags.
pub const SUPERBLOCK_SIZE_128: u32 = 0x2;

/// Without this, the source's `codedExtent` MUST equal the sequence header's
/// `max_frame_{width,height}_minus_1 + 1` (`VUID-vkCmdEncodeVideoKHR-flags-10324`).
pub const CAPABILITY_FRAME_SIZE_OVERRIDE: u32 = 0x0000_0008;
/// Without this, EVERY reference slot's `codedExtent` MUST equal the source's
/// (`VUID-vkCmdEncodeVideoKHR-flags-10325`).
pub const CAPABILITY_MOTION_VECTOR_SCALING: u32 = 0x0000_0010;

pub const PREDICTION_MODE_INTRA_ONLY: i32 = 0;
pub const PREDICTION_MODE_SINGLE_REFERENCE: i32 = 1;
pub const RC_GROUP_INTRA: i32 = 0;
pub const RC_GROUP_PREDICTIVE: i32 = 1;
pub const RC_GROUP_BIPREDICTIVE: i32 = 2;

// Index into `reference_name_slot_indices` (0-based LAST..ALTREF). The STD enum is 1-based.
pub const REFERENCE_NAME_LAST_FRAME_IDX: usize = 0;

// Copied from ash-master `native.rs`; bit indices must stay bindgen's order.
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct __BindgenBitfieldUnit<Storage> {
    storage: Storage,
}
impl<Storage> __BindgenBitfieldUnit<Storage> {
    #[inline]
    pub const fn new(storage: Storage) -> Self {
        Self { storage }
    }
}
impl<Storage> __BindgenBitfieldUnit<Storage>
where
    Storage: AsRef<[u8]> + AsMut<[u8]>,
{
    #[inline]
    pub fn get_bit(&self, index: usize) -> bool {
        let byte_index = index / 8;
        let byte = self.storage.as_ref()[byte_index];
        let bit_index = if cfg!(target_endian = "big") {
            7 - (index % 8)
        } else {
            index % 8
        };
        byte & (1 << bit_index) == (1 << bit_index)
    }
    #[inline]
    pub fn set_bit(&mut self, index: usize, val: bool) {
        let byte_index = index / 8;
        let byte = &mut self.storage.as_mut()[byte_index];
        let bit_index = if cfg!(target_endian = "big") {
            7 - (index % 8)
        } else {
            index % 8
        };
        let mask = 1 << bit_index;
        if val {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
    #[inline]
    pub fn get(&self, bit_offset: usize, bit_width: u8) -> u64 {
        let mut val = 0;
        for i in 0..(bit_width as usize) {
            if self.get_bit(i + bit_offset) {
                let index = if cfg!(target_endian = "big") {
                    bit_width as usize - 1 - i
                } else {
                    i
                };
                val |= 1 << index;
            }
        }
        val
    }
    #[inline]
    pub fn set(&mut self, bit_offset: usize, bit_width: u8, val: u64) {
        for i in 0..(bit_width as usize) {
            let mask = 1 << i;
            let val_bit_is_set = val & mask == mask;
            let index = if cfg!(target_endian = "big") {
                bit_width as usize - 1 - i
            } else {
                i
            };
            self.set_bit(index + bit_offset, val_bit_is_set);
        }
    }
}

#[repr(C, align(4))]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1PictureInfoFlags {
    pub _bitfield_align_1: [u8; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
impl StdVideoEncodeAV1PictureInfoFlags {
    #[inline]
    pub fn set_error_resilient_mode(&mut self, val: u32) {
        self._bitfield_1.set(0, 1, val as u64)
    }
    #[inline]
    pub fn set_disable_cdf_update(&mut self, val: u32) {
        self._bitfield_1.set(1, 1, val as u64)
    }
    #[inline]
    pub fn set_use_superres(&mut self, val: u32) {
        self._bitfield_1.set(2, 1, val as u64)
    }
    #[inline]
    pub fn set_render_and_frame_size_different(&mut self, val: u32) {
        self._bitfield_1.set(3, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_screen_content_tools(&mut self, val: u32) {
        self._bitfield_1.set(4, 1, val as u64)
    }
    #[inline]
    pub fn set_is_filter_switchable(&mut self, val: u32) {
        self._bitfield_1.set(5, 1, val as u64)
    }
    #[inline]
    pub fn set_force_integer_mv(&mut self, val: u32) {
        self._bitfield_1.set(6, 1, val as u64)
    }
    #[inline]
    pub fn set_frame_size_override_flag(&mut self, val: u32) {
        self._bitfield_1.set(7, 1, val as u64)
    }
    #[inline]
    pub fn set_buffer_removal_time_present_flag(&mut self, val: u32) {
        self._bitfield_1.set(8, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_intrabc(&mut self, val: u32) {
        self._bitfield_1.set(9, 1, val as u64)
    }
    #[inline]
    pub fn set_frame_refs_short_signaling(&mut self, val: u32) {
        self._bitfield_1.set(10, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_high_precision_mv(&mut self, val: u32) {
        self._bitfield_1.set(11, 1, val as u64)
    }
    #[inline]
    pub fn set_is_motion_mode_switchable(&mut self, val: u32) {
        self._bitfield_1.set(12, 1, val as u64)
    }
    #[inline]
    pub fn set_use_ref_frame_mvs(&mut self, val: u32) {
        self._bitfield_1.set(13, 1, val as u64)
    }
    #[inline]
    pub fn set_disable_frame_end_update_cdf(&mut self, val: u32) {
        self._bitfield_1.set(14, 1, val as u64)
    }
    #[inline]
    pub fn set_allow_warped_motion(&mut self, val: u32) {
        self._bitfield_1.set(15, 1, val as u64)
    }
    #[inline]
    pub fn set_reduced_tx_set(&mut self, val: u32) {
        self._bitfield_1.set(16, 1, val as u64)
    }
    #[inline]
    pub fn set_skip_mode_present(&mut self, val: u32) {
        self._bitfield_1.set(17, 1, val as u64)
    }
    #[inline]
    pub fn set_delta_q_present(&mut self, val: u32) {
        self._bitfield_1.set(18, 1, val as u64)
    }
    #[inline]
    pub fn set_delta_lf_present(&mut self, val: u32) {
        self._bitfield_1.set(19, 1, val as u64)
    }
    #[inline]
    pub fn set_delta_lf_multi(&mut self, val: u32) {
        self._bitfield_1.set(20, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_enabled(&mut self, val: u32) {
        self._bitfield_1.set(21, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_update_map(&mut self, val: u32) {
        self._bitfield_1.set(22, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_temporal_update(&mut self, val: u32) {
        self._bitfield_1.set(23, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_update_data(&mut self, val: u32) {
        self._bitfield_1.set(24, 1, val as u64)
    }
    #[inline]
    pub fn set_UsesLr(&mut self, val: u32) {
        self._bitfield_1.set(25, 1, val as u64)
    }
    #[inline]
    pub fn set_usesChromaLr(&mut self, val: u32) {
        self._bitfield_1.set(26, 1, val as u64)
    }
    #[inline]
    pub fn set_show_frame(&mut self, val: u32) {
        self._bitfield_1.set(27, 1, val as u64)
    }
    #[inline]
    pub fn set_showable_frame(&mut self, val: u32) {
        self._bitfield_1.set(28, 1, val as u64)
    }
    #[inline]
    pub fn set_reserved(&mut self, val: u32) {
        self._bitfield_1.set(29, 3, val as u64)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1PictureInfo {
    pub flags: StdVideoEncodeAV1PictureInfoFlags,
    pub frame_type: StdVideoAV1FrameType,
    pub frame_presentation_time: u32,
    pub current_frame_id: u32,
    pub order_hint: u8,
    pub primary_ref_frame: u8,
    pub refresh_frame_flags: u8,
    pub coded_denom: u8,
    pub render_width_minus_1: u16,
    pub render_height_minus_1: u16,
    pub interpolation_filter: StdVideoAV1InterpolationFilter,
    pub TxMode: StdVideoAV1TxMode,
    pub delta_q_res: u8,
    pub delta_lf_res: u8,
    pub ref_order_hint: [u8; 8usize],
    pub ref_frame_idx: [i8; 7usize],
    pub reserved1: [u8; 3usize],
    pub delta_frame_id_minus_1: [u32; 7usize],
    pub pTileInfo: *const ash::vk::native::StdVideoAV1TileInfo,
    pub pQuantization: *const ash::vk::native::StdVideoAV1Quantization,
    pub pSegmentation: *const ash::vk::native::StdVideoAV1Segmentation,
    pub pLoopFilter: *const ash::vk::native::StdVideoAV1LoopFilter,
    pub pCDEF: *const ash::vk::native::StdVideoAV1CDEF,
    pub pLoopRestoration: *const ash::vk::native::StdVideoAV1LoopRestoration,
    pub pGlobalMotion: *const ash::vk::native::StdVideoAV1GlobalMotion,
    pub pExtensionHeader: *const StdVideoEncodeAV1ExtensionHeader,
    pub pBufferRemovalTimes: *const u32,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1ReferenceInfoFlags {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}
impl StdVideoEncodeAV1ReferenceInfoFlags {
    #[inline]
    pub fn set_disable_frame_end_update_cdf(&mut self, val: u32) {
        self._bitfield_1.set(0, 1, val as u64)
    }
    #[inline]
    pub fn set_segmentation_enabled(&mut self, val: u32) {
        self._bitfield_1.set(1, 1, val as u64)
    }
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1ReferenceInfo {
    pub flags: StdVideoEncodeAV1ReferenceInfoFlags,
    pub RefFrameId: u32,
    pub frame_type: StdVideoAV1FrameType,
    pub OrderHint: u8,
    pub reserved1: [u8; 3usize],
    pub pExtensionHeader: *const StdVideoEncodeAV1ExtensionHeader,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1ExtensionHeader {
    pub temporal_id: u8,
    pub spatial_id: u8,
}

// Layout-identical to the KHR structs with ash lifetimes/`PhantomData` dropped; chain via raw
// `p_next`. Flag/enum newtypes flattened to their `u32`/`i32` repr.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1ProfileInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub std_profile: StdVideoAV1Profile,
}

/// `videoEncodeAV1` must be enabled at device creation for any encode-AV1 use.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct PhysicalDeviceVideoEncodeAV1FeaturesKHR {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub video_encode_av1: vk::Bool32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1CapabilitiesKHR {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub flags: u32,
    pub max_level: StdVideoAV1Level,
    pub coded_picture_alignment: vk::Extent2D,
    pub max_tiles: vk::Extent2D,
    pub min_tile_size: vk::Extent2D,
    pub max_tile_size: vk::Extent2D,
    pub superblock_sizes: u32,
    pub max_single_reference_count: u32,
    pub single_reference_name_mask: u32,
    pub max_unidirectional_compound_reference_count: u32,
    pub max_unidirectional_compound_group1_reference_count: u32,
    pub unidirectional_compound_reference_name_mask: u32,
    pub max_bidirectional_compound_reference_count: u32,
    pub max_bidirectional_compound_group1_reference_count: u32,
    pub max_bidirectional_compound_group2_reference_count: u32,
    pub bidirectional_compound_reference_name_mask: u32,
    pub max_temporal_layer_count: u32,
    pub max_spatial_layer_count: u32,
    pub max_operating_points: u32,
    pub min_q_index: u32,
    pub max_q_index: u32,
    pub prefers_gop_remaining_frames: vk::Bool32,
    pub requires_gop_remaining_frames: vk::Bool32,
    pub std_syntax_flags: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1SessionParametersCreateInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub p_std_sequence_header: *const StdVideoAV1SequenceHeader,
    pub p_std_decoder_model_info: *const c_void,
    pub std_operating_point_count: u32,
    pub p_std_operating_points: *const c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1PictureInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub prediction_mode: i32,
    pub rate_control_group: i32,
    pub constant_q_index: u32,
    pub p_std_picture_info: *const StdVideoEncodeAV1PictureInfo,
    pub reference_name_slot_indices: [i32; MAX_VIDEO_AV1_REFERENCES_PER_FRAME],
    pub primary_reference_cdf_only: vk::Bool32,
    pub generate_obu_extension_header: vk::Bool32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1DpbSlotInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub p_std_reference_info: *const StdVideoEncodeAV1ReferenceInfo,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1RateControlInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub flags: u32,
    pub gop_frame_count: u32,
    pub key_frame_period: u32,
    pub consecutive_bipredictive_frame_count: u32,
    pub temporal_layer_count: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1QIndexKHR {
    pub intra_q_index: u32,
    pub predictive_q_index: u32,
    pub bipredictive_q_index: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1FrameSizeKHR {
    pub intra_frame_size: u32,
    pub predictive_frame_size: u32,
    pub bipredictive_frame_size: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1RateControlLayerInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub use_min_q_index: vk::Bool32,
    pub min_q_index: VideoEncodeAV1QIndexKHR,
    pub use_max_q_index: vk::Bool32,
    pub max_q_index: VideoEncodeAV1QIndexKHR,
    pub use_max_frame_size: vk::Bool32,
    pub max_frame_size: VideoEncodeAV1FrameSizeKHR,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1GopRemainingFrameInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub use_gop_remaining_frames: vk::Bool32,
    pub gop_remaining_intra: u32,
    pub gop_remaining_predictive: u32,
    pub gop_remaining_bipredictive: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct VideoEncodeAV1SessionCreateInfoKHR {
    pub s_type: vk::StructureType,
    pub p_next: *const c_void,
    pub use_max_level: vk::Bool32,
    pub max_level: StdVideoAV1Level,
}

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StdVideoEncodeAV1OperatingPointInfoFlags {
    pub _bitfield_align_1: [u32; 0],
    pub _bitfield_1: __BindgenBitfieldUnit<[u8; 4usize]>,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct StdVideoEncodeAV1OperatingPointInfo {
    pub flags: StdVideoEncodeAV1OperatingPointInfoFlags,
    pub operating_point_idc: u16,
    pub seq_level_idx: u8,
    pub seq_tier: u8,
    pub decoder_buffer_delay: u32,
    pub encoder_buffer_delay: u32,
    pub initial_display_delay_minus_1: u8,
}

#[inline]
pub fn stype(raw: i32) -> vk::StructureType {
    vk::StructureType::from_raw(raw)
}

// Hand-copied layouts go to the driver via raw `p_next`; a field insert/drop/widen is a silent
// offset bug. `const` (not `#[cfg(test)]`) so every build catches size/align/offset. Same-type
// field swaps are invisible here — order is pinned against the headers; bitfields by `abi_tests`.
// Duplicated in `vk_valve_rgb.rs`: both modules delete when ash ships these, so do not share.
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

// `*Flags` is a C `uint32_t` of bitfields: size/align only here; member order is `abi_tests`.
assert_abi_layout!(StdVideoEncodeAV1PictureInfoFlags { size: 4, align: 4 });

assert_abi_layout!(StdVideoEncodeAV1PictureInfo {
    size: 152, align: 8,
    flags @ 0,
    frame_type @ 4,
    frame_presentation_time @ 8,
    current_frame_id @ 12,
    order_hint @ 16,
    primary_ref_frame @ 17,
    refresh_frame_flags @ 18,
    coded_denom @ 19,
    render_width_minus_1 @ 20,
    render_height_minus_1 @ 22,
    interpolation_filter @ 24,
    TxMode @ 28,
    delta_q_res @ 32,
    delta_lf_res @ 33,
    ref_order_hint @ 34,
    ref_frame_idx @ 42,
    reserved1 @ 49,
    delta_frame_id_minus_1 @ 52,
    pTileInfo @ 80,
    pQuantization @ 88,
    pSegmentation @ 96,
    pLoopFilter @ 104,
    pCDEF @ 112,
    pLoopRestoration @ 120,
    pGlobalMotion @ 128,
    pExtensionHeader @ 136,
    pBufferRemovalTimes @ 144,
});

assert_abi_layout!(StdVideoEncodeAV1ReferenceInfoFlags { size: 4, align: 4 });

assert_abi_layout!(StdVideoEncodeAV1ReferenceInfo {
    size: 24, align: 8,
    flags @ 0,
    RefFrameId @ 4,
    frame_type @ 8,
    OrderHint @ 12,
    reserved1 @ 13,
    pExtensionHeader @ 16,
});

assert_abi_layout!(StdVideoEncodeAV1ExtensionHeader {
    size: 2, align: 1,
    temporal_id @ 0,
    spatial_id @ 1,
});

assert_abi_layout!(StdVideoEncodeAV1OperatingPointInfoFlags { size: 4, align: 4 });

assert_abi_layout!(StdVideoEncodeAV1OperatingPointInfo {
    size: 20, align: 4,
    flags @ 0,
    operating_point_idc @ 4,
    seq_level_idx @ 6,
    seq_tier @ 7,
    decoder_buffer_delay @ 8,
    encoder_buffer_delay @ 12,
    initial_display_delay_minus_1 @ 16,
});

assert_abi_layout!(VideoEncodeAV1ProfileInfoKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    std_profile @ 16,
});

assert_abi_layout!(PhysicalDeviceVideoEncodeAV1FeaturesKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    video_encode_av1 @ 16,
});

assert_abi_layout!(VideoEncodeAV1CapabilitiesKHR {
    size: 128, align: 8,
    s_type @ 0,
    p_next @ 8,
    flags @ 16,
    max_level @ 20,
    coded_picture_alignment @ 24,
    max_tiles @ 32,
    min_tile_size @ 40,
    max_tile_size @ 48,
    superblock_sizes @ 56,
    max_single_reference_count @ 60,
    single_reference_name_mask @ 64,
    max_unidirectional_compound_reference_count @ 68,
    max_unidirectional_compound_group1_reference_count @ 72,
    unidirectional_compound_reference_name_mask @ 76,
    max_bidirectional_compound_reference_count @ 80,
    max_bidirectional_compound_group1_reference_count @ 84,
    max_bidirectional_compound_group2_reference_count @ 88,
    bidirectional_compound_reference_name_mask @ 92,
    max_temporal_layer_count @ 96,
    max_spatial_layer_count @ 100,
    max_operating_points @ 104,
    min_q_index @ 108,
    max_q_index @ 112,
    prefers_gop_remaining_frames @ 116,
    requires_gop_remaining_frames @ 120,
    std_syntax_flags @ 124,
});

assert_abi_layout!(VideoEncodeAV1SessionParametersCreateInfoKHR {
    size: 48, align: 8,
    s_type @ 0,
    p_next @ 8,
    p_std_sequence_header @ 16,
    p_std_decoder_model_info @ 24,
    std_operating_point_count @ 32,
    p_std_operating_points @ 40,
});

assert_abi_layout!(VideoEncodeAV1PictureInfoKHR {
    size: 80, align: 8,
    s_type @ 0,
    p_next @ 8,
    prediction_mode @ 16,
    rate_control_group @ 20,
    constant_q_index @ 24,
    p_std_picture_info @ 32,
    reference_name_slot_indices @ 40,
    primary_reference_cdf_only @ 68,
    generate_obu_extension_header @ 72,
});

assert_abi_layout!(VideoEncodeAV1DpbSlotInfoKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    p_std_reference_info @ 16,
});

assert_abi_layout!(VideoEncodeAV1RateControlInfoKHR {
    size: 40, align: 8,
    s_type @ 0,
    p_next @ 8,
    flags @ 16,
    gop_frame_count @ 20,
    key_frame_period @ 24,
    consecutive_bipredictive_frame_count @ 28,
    temporal_layer_count @ 32,
});

assert_abi_layout!(VideoEncodeAV1QIndexKHR {
    size: 12, align: 4,
    intra_q_index @ 0,
    predictive_q_index @ 4,
    bipredictive_q_index @ 8,
});

assert_abi_layout!(VideoEncodeAV1FrameSizeKHR {
    size: 12, align: 4,
    intra_frame_size @ 0,
    predictive_frame_size @ 4,
    bipredictive_frame_size @ 8,
});

assert_abi_layout!(VideoEncodeAV1RateControlLayerInfoKHR {
    size: 64, align: 8,
    s_type @ 0,
    p_next @ 8,
    use_min_q_index @ 16,
    min_q_index @ 20,
    use_max_q_index @ 32,
    max_q_index @ 36,
    use_max_frame_size @ 48,
    max_frame_size @ 52,
});

assert_abi_layout!(VideoEncodeAV1GopRemainingFrameInfoKHR {
    size: 32, align: 8,
    s_type @ 0,
    p_next @ 8,
    use_gop_remaining_frames @ 16,
    gop_remaining_intra @ 20,
    gop_remaining_predictive @ 24,
    gop_remaining_bipredictive @ 28,
});

assert_abi_layout!(VideoEncodeAV1SessionCreateInfoKHR {
    size: 24, align: 8,
    s_type @ 0,
    p_next @ 8,
    use_max_level @ 16,
    max_level @ 20,
});

#[cfg(test)]
mod abi_tests {
    use super::*;

    fn sets_only_bit(
        storage: __BindgenBitfieldUnit<[u8; 4]>,
        name: &str,
        bit: usize,
        expected_width: usize,
    ) {
        for probe in 0..32 {
            let want = probe >= bit && probe < bit + expected_width;
            assert_eq!(
                storage.get_bit(probe),
                want,
                "`{name}` should occupy bit(s) {bit}..{}, but bit {probe} disagrees",
                bit + expected_width
            );
        }
    }

    /// C bitfields fill from bit 0 in declaration order. The array is
    /// `vulkan_video_codec_av1std_encode.h` member order; a silent index shift
    /// would light the wrong flag in the word the driver reads.
    #[test]
    fn picture_info_flag_setters_follow_the_c_declaration_order() {
        #[allow(clippy::type_complexity)]
        let members: [(&str, fn(&mut StdVideoEncodeAV1PictureInfoFlags)); 29] = [
            ("error_resilient_mode", |f| f.set_error_resilient_mode(1)),
            ("disable_cdf_update", |f| f.set_disable_cdf_update(1)),
            ("use_superres", |f| f.set_use_superres(1)),
            ("render_and_frame_size_different", |f| {
                f.set_render_and_frame_size_different(1)
            }),
            ("allow_screen_content_tools", |f| {
                f.set_allow_screen_content_tools(1)
            }),
            ("is_filter_switchable", |f| f.set_is_filter_switchable(1)),
            ("force_integer_mv", |f| f.set_force_integer_mv(1)),
            ("frame_size_override_flag", |f| {
                f.set_frame_size_override_flag(1)
            }),
            ("buffer_removal_time_present_flag", |f| {
                f.set_buffer_removal_time_present_flag(1)
            }),
            ("allow_intrabc", |f| f.set_allow_intrabc(1)),
            ("frame_refs_short_signaling", |f| {
                f.set_frame_refs_short_signaling(1)
            }),
            ("allow_high_precision_mv", |f| {
                f.set_allow_high_precision_mv(1)
            }),
            ("is_motion_mode_switchable", |f| {
                f.set_is_motion_mode_switchable(1)
            }),
            ("use_ref_frame_mvs", |f| f.set_use_ref_frame_mvs(1)),
            ("disable_frame_end_update_cdf", |f| {
                f.set_disable_frame_end_update_cdf(1)
            }),
            ("allow_warped_motion", |f| f.set_allow_warped_motion(1)),
            ("reduced_tx_set", |f| f.set_reduced_tx_set(1)),
            ("skip_mode_present", |f| f.set_skip_mode_present(1)),
            ("delta_q_present", |f| f.set_delta_q_present(1)),
            ("delta_lf_present", |f| f.set_delta_lf_present(1)),
            ("delta_lf_multi", |f| f.set_delta_lf_multi(1)),
            ("segmentation_enabled", |f| f.set_segmentation_enabled(1)),
            ("segmentation_update_map", |f| {
                f.set_segmentation_update_map(1)
            }),
            ("segmentation_temporal_update", |f| {
                f.set_segmentation_temporal_update(1)
            }),
            ("segmentation_update_data", |f| {
                f.set_segmentation_update_data(1)
            }),
            ("UsesLr", |f| f.set_UsesLr(1)),
            ("usesChromaLr", |f| f.set_usesChromaLr(1)),
            ("show_frame", |f| f.set_show_frame(1)),
            ("showable_frame", |f| f.set_showable_frame(1)),
        ];

        for (bit, (name, set)) in members.into_iter().enumerate() {
            let mut flags = StdVideoEncodeAV1PictureInfoFlags {
                _bitfield_align_1: [],
                _bitfield_1: __BindgenBitfieldUnit::new([0u8; 4]),
            };
            set(&mut flags);
            sets_only_bit(flags._bitfield_1, name, bit, 1);
        }
    }

    /// `reserved : 3` is bits 29..32. A dropped member above would shift it down.
    #[test]
    fn picture_info_reserved_occupies_the_top_three_bits() {
        let mut flags = StdVideoEncodeAV1PictureInfoFlags {
            _bitfield_align_1: [],
            _bitfield_1: __BindgenBitfieldUnit::new([0u8; 4]),
        };
        flags.set_reserved(0b111);
        sets_only_bit(flags._bitfield_1, "reserved", 29, 3);
    }

    #[test]
    fn reference_info_flag_setters_follow_the_c_declaration_order() {
        #[allow(clippy::type_complexity)]
        let members: [(&str, fn(&mut StdVideoEncodeAV1ReferenceInfoFlags)); 2] = [
            ("disable_frame_end_update_cdf", |f| {
                f.set_disable_frame_end_update_cdf(1)
            }),
            ("segmentation_enabled", |f| f.set_segmentation_enabled(1)),
        ];

        for (bit, (name, set)) in members.into_iter().enumerate() {
            let mut flags = StdVideoEncodeAV1ReferenceInfoFlags {
                _bitfield_align_1: [],
                _bitfield_1: __BindgenBitfieldUnit::new([0u8; 4]),
            };
            set(&mut flags);
            sets_only_bit(flags._bitfield_1, name, bit, 1);
        }
    }
}
