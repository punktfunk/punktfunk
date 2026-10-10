//! VideoProc, hand-declared: the ingest colour conversion and the dmabuf import
//! attributes.
//!
//! Capture hands the host packed RGB — a dmabuf or CPU bytes — and the encoder
//! takes NV12 (P010 at ten bits). Both radeonsi and iHD expose
//! `VAEntrypointVideoProc`, so the conversion is one pipeline buffer on the GPU and
//! `swscale` has no job here. Same rules as [`crate::enc_h264`]: `#[repr(C)]`
//! against `va_vpp.h`, every layout measured by `layout-probe.c` on the target and
//! pinned by a `const _` assert, and no libva.

use std::ffi::c_void;
use std::mem::offset_of;
use std::mem::size_of;

pub use crate::config::VA_RT_FORMAT_YUV420;
pub use crate::config::VA_RT_FORMAT_YUV420_10;
pub use crate::config::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;

pub const VA_PROFILE_NONE: i32 = -1;
pub const VA_ENTRYPOINT_VIDEO_PROC: i32 = 10;
pub const VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE: u32 = 41;

/// `VAConfigAttribRTFormat` bits for packed RGB, which only ingest uses.
pub const VA_RT_FORMAT_RGB32: u32 = 0x0002_0000;
pub const VA_RT_FORMAT_RGB32_10: u32 = 0x0020_0000;

/// The two surface attributes a dmabuf import passes to `vaCreateSurfaces`.
pub const VA_SURFACE_ATTRIB_MEMORY_TYPE: i32 = 6;
pub const VA_SURFACE_ATTRIB_EXTERNAL_BUFFER_DESCRIPTOR: i32 = 7;
pub const VA_GENERIC_VALUE_TYPE_POINTER: i32 = 3;

/// `VAProcColorStandardExplicit`: the colour is in the properties, both sides.
/// Mesa derives a *named* output standard from the input's, so a named BT.2020
/// output is quietly BT.709; explicit is the only shape both drivers honour.
pub const VA_PROC_COLOR_STANDARD_EXPLICIT: u32 = 13;
/// `VAProcColorProperties::color_range`.
pub const VA_SOURCE_RANGE_REDUCED: u8 = 1;
pub const VA_SOURCE_RANGE_FULL: u8 = 2;
/// `filter_flags`: the driver's best scaler (iHD's polyphase AVS) instead of its default.
pub const VA_FILTER_SCALING_HQ: u32 = 0x0000_0200;

/// libva fourccs name byte order; DRM fourccs name the bit layout of a
/// little-endian word. `XR24` (`DRM_FORMAT_XRGB8888`) is B, G, R, X in memory,
/// which libva calls `BGRX`. The ten-bit and planar codes coincide.
pub const VA_FOURCC_BGRX: u32 = 0x5852_4742;
pub const VA_FOURCC_RGBX: u32 = 0x5842_4752;
pub const VA_FOURCC_BGRA: u32 = 0x4152_4742;
pub const VA_FOURCC_RGBA: u32 = 0x4142_4752;
pub const VA_FOURCC_X2R10G10B10: u32 = 0x3033_5258;
pub const VA_FOURCC_X2B10G10R10: u32 = 0x3033_4258;
pub const VA_FOURCC_A2R10G10B10: u32 = 0x3033_5241;
pub const VA_FOURCC_A2B10G10R10: u32 = 0x3033_4241;

const fn drm(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}
pub const DRM_FORMAT_XRGB8888: u32 = drm(b"XR24");
pub const DRM_FORMAT_ARGB8888: u32 = drm(b"AR24");
pub const DRM_FORMAT_XBGR8888: u32 = drm(b"XB24");
pub const DRM_FORMAT_ABGR8888: u32 = drm(b"AB24");
pub const DRM_FORMAT_XRGB2101010: u32 = drm(b"XR30");
pub const DRM_FORMAT_XBGR2101010: u32 = drm(b"XB30");
pub const DRM_FORMAT_ARGB2101010: u32 = drm(b"AR30");
pub const DRM_FORMAT_ABGR2101010: u32 = drm(b"AB30");
pub const DRM_FORMAT_NV12: u32 = drm(b"NV12");
pub const DRM_FORMAT_P010: u32 = drm(b"P010");

/// What to import a dmabuf as: the libva fourcc, and the render-target format its
/// surface is created with. `None` is a format no ingest takes.
pub fn import_format(drm_fourcc: u32) -> Option<(u32, u32)> {
    let va = match drm_fourcc {
        DRM_FORMAT_XRGB8888 => VA_FOURCC_BGRX,
        DRM_FORMAT_ARGB8888 => VA_FOURCC_BGRA,
        DRM_FORMAT_XBGR8888 => VA_FOURCC_RGBX,
        DRM_FORMAT_ABGR8888 => VA_FOURCC_RGBA,
        DRM_FORMAT_XRGB2101010 => VA_FOURCC_X2R10G10B10,
        DRM_FORMAT_XBGR2101010 => VA_FOURCC_X2B10G10R10,
        DRM_FORMAT_ARGB2101010 => VA_FOURCC_A2R10G10B10,
        DRM_FORMAT_ABGR2101010 => VA_FOURCC_A2B10G10R10,
        DRM_FORMAT_NV12 => crate::drm::VA_FOURCC_NV12,
        DRM_FORMAT_P010 => crate::drm::VA_FOURCC_P010,
        _ => return None,
    };
    Some((va, rt_format_for(va)?))
}

/// The render-target format a surface of `va_fourcc` lives in.
pub fn rt_format_for(va_fourcc: u32) -> Option<u32> {
    Some(match va_fourcc {
        VA_FOURCC_BGRX | VA_FOURCC_BGRA | VA_FOURCC_RGBX | VA_FOURCC_RGBA => VA_RT_FORMAT_RGB32,
        VA_FOURCC_X2R10G10B10
        | VA_FOURCC_X2B10G10R10
        | VA_FOURCC_A2R10G10B10
        | VA_FOURCC_A2B10G10R10 => VA_RT_FORMAT_RGB32_10,
        crate::drm::VA_FOURCC_NV12 => VA_RT_FORMAT_YUV420,
        crate::drm::VA_FOURCC_P010 => VA_RT_FORMAT_YUV420_10,
        _ => return None,
    })
}

/// `VARectangle`: `short` origin, `unsigned short` size.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaRectangle {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

/// `VAProcColorProperties`: five bytes of H.273 facts and three reserved.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct VaProcColorProperties {
    pub chroma_sample_location: u8,
    pub color_range: u8,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub reserved: [u8; 3],
}

/// `VAProcPipelineParameterBuffer`: one source surface into the picture
/// `vaBeginPicture` named. Pointers are null here — no regions, no filters, no
/// reference surfaces — which libva reads as "the whole picture, as is".
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VaProcPipelineParameterBuffer {
    pub surface: u32,
    pub surface_region: *const VaRectangle,
    pub surface_color_standard: u32,
    pub output_region: *const VaRectangle,
    pub output_background_color: u32,
    pub output_color_standard: u32,
    pub pipeline_flags: u32,
    pub filter_flags: u32,
    pub filters: *mut u32,
    pub num_filters: u32,
    pub forward_references: *mut u32,
    pub num_forward_references: u32,
    pub backward_references: *mut u32,
    pub num_backward_references: u32,
    pub rotation_state: u32,
    pub blend_state: *const c_void,
    pub mirror_state: u32,
    pub additional_outputs: *mut u32,
    pub num_additional_outputs: u32,
    pub input_surface_flag: u32,
    pub output_surface_flag: u32,
    pub input_color_properties: VaProcColorProperties,
    pub output_color_properties: VaProcColorProperties,
    pub processing_mode: u32,
    pub output_hdr_metadata: *const c_void,
    pub va_reserved: [u32; 16],
}

impl VaProcPipelineParameterBuffer {
    /// Whole-picture conversion of `source`, with the colour facts `swscale` used to
    /// be told: RGB is full range in and limited range out, and the `colour` H.273 triple
    /// (BT.709 for SDR at either depth, BT.2020 PQ for HDR) on the YUV side — the same
    /// primaries and transfer on both sides, so the only arithmetic is the matrix. YUV in is copied.
    pub fn convert(source: u32, source_is_rgb: bool, colour: [u8; 3]) -> Self {
        let (primaries, transfer, matrix) = (colour[0], colour[1], colour[2]);
        let (in_range, in_matrix) = if source_is_rgb {
            (VA_SOURCE_RANGE_FULL, 0)
        } else {
            (VA_SOURCE_RANGE_REDUCED, matrix)
        };
        Self {
            surface: source,
            surface_region: std::ptr::null(),
            surface_color_standard: VA_PROC_COLOR_STANDARD_EXPLICIT,
            output_region: std::ptr::null(),
            output_background_color: 0xff00_0000,
            output_color_standard: VA_PROC_COLOR_STANDARD_EXPLICIT,
            pipeline_flags: 0,
            filter_flags: 0,
            filters: std::ptr::null_mut(),
            num_filters: 0,
            forward_references: std::ptr::null_mut(),
            num_forward_references: 0,
            backward_references: std::ptr::null_mut(),
            num_backward_references: 0,
            rotation_state: 0,
            blend_state: std::ptr::null(),
            mirror_state: 0,
            additional_outputs: std::ptr::null_mut(),
            num_additional_outputs: 0,
            input_surface_flag: 0,
            output_surface_flag: 0,
            input_color_properties: VaProcColorProperties {
                color_range: in_range,
                colour_primaries: primaries,
                transfer_characteristics: transfer,
                matrix_coefficients: in_matrix,
                ..Default::default()
            },
            output_color_properties: VaProcColorProperties {
                color_range: VA_SOURCE_RANGE_REDUCED,
                colour_primaries: primaries,
                transfer_characteristics: transfer,
                matrix_coefficients: matrix,
                ..Default::default()
            },
            processing_mode: 0,
            output_hdr_metadata: std::ptr::null(),
            va_reserved: [0; 16],
        }
    }
}

/// Measured by `layout-probe.c` against libva 2.22 on `.50`.
const _: () = {
    assert!(size_of::<VaRectangle>() == 8);
    assert!(offset_of!(VaRectangle, width) == 4);
    assert!(size_of::<VaProcColorProperties>() == 8);
    assert!(offset_of!(VaProcColorProperties, matrix_coefficients) == 4);
    assert!(size_of::<VaProcPipelineParameterBuffer>() == 224);
    assert!(offset_of!(VaProcPipelineParameterBuffer, surface_region) == 8);
    assert!(offset_of!(VaProcPipelineParameterBuffer, surface_color_standard) == 16);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_region) == 24);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_color_standard) == 36);
    assert!(offset_of!(VaProcPipelineParameterBuffer, filters) == 48);
    assert!(offset_of!(VaProcPipelineParameterBuffer, forward_references) == 64);
    assert!(offset_of!(VaProcPipelineParameterBuffer, backward_references) == 80);
    assert!(offset_of!(VaProcPipelineParameterBuffer, rotation_state) == 92);
    assert!(offset_of!(VaProcPipelineParameterBuffer, blend_state) == 96);
    assert!(offset_of!(VaProcPipelineParameterBuffer, additional_outputs) == 112);
    assert!(offset_of!(VaProcPipelineParameterBuffer, input_surface_flag) == 124);
    assert!(offset_of!(VaProcPipelineParameterBuffer, input_color_properties) == 132);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_color_properties) == 140);
    assert!(offset_of!(VaProcPipelineParameterBuffer, processing_mode) == 148);
    assert!(offset_of!(VaProcPipelineParameterBuffer, output_hdr_metadata) == 152);
    assert!(offset_of!(VaProcPipelineParameterBuffer, va_reserved) == 160);
};

#[cfg(test)]
mod tests {
    use super::*;

    /// The capture formats the libav path mapped, each to the surface it imports as.
    /// `XR24` is the everyday one: BGRX bytes, so the libva name reverses.
    #[test]
    fn every_capture_fourcc_imports_as_the_right_surface() {
        assert_eq!(
            import_format(DRM_FORMAT_XRGB8888),
            Some((VA_FOURCC_BGRX, VA_RT_FORMAT_RGB32))
        );
        assert_eq!(
            import_format(DRM_FORMAT_ABGR8888),
            Some((VA_FOURCC_RGBA, VA_RT_FORMAT_RGB32))
        );
        assert_eq!(
            import_format(DRM_FORMAT_XRGB2101010),
            Some((VA_FOURCC_X2R10G10B10, VA_RT_FORMAT_RGB32_10))
        );
        assert_eq!(
            import_format(DRM_FORMAT_NV12),
            Some((crate::drm::VA_FOURCC_NV12, VA_RT_FORMAT_YUV420))
        );
        assert_eq!(import_format(drm(b"YUYV")), None);
        // The ten-bit codes are the same four bytes in both namespaces.
        assert_eq!(DRM_FORMAT_XRGB2101010, VA_FOURCC_X2R10G10B10);
        assert_eq!(DRM_FORMAT_NV12, crate::drm::VA_FOURCC_NV12);
    }

    /// RGB in is full range; the encoder's NV12 is limited BT.709, P010 limited
    /// BT.2020 — stated explicitly on both sides. Getting a side wrong is a picture
    /// that decodes fine and is the wrong red.
    #[test]
    fn rgb_ingest_states_both_sides_explicitly() {
        let p = VaProcPipelineParameterBuffer::convert(7, true, crate::hevc::COLOUR_BT709);
        assert_eq!(p.surface, 7);
        assert_eq!(p.surface_color_standard, VA_PROC_COLOR_STANDARD_EXPLICIT);
        assert_eq!(p.output_color_standard, VA_PROC_COLOR_STANDARD_EXPLICIT);
        let (i, o) = (&p.input_color_properties, &p.output_color_properties);
        assert_eq!(i.color_range, VA_SOURCE_RANGE_FULL);
        assert_eq!(o.color_range, VA_SOURCE_RANGE_REDUCED);
        assert_eq!((i.colour_primaries, i.transfer_characteristics), (1, 1));
        assert_eq!(
            (
                o.colour_primaries,
                o.transfer_characteristics,
                o.matrix_coefficients
            ),
            (1, 1, 1)
        );
        assert_eq!(i.matrix_coefficients, 0, "RGB source: identity");
        assert!(p.surface_region.is_null() && p.filters.is_null());

        let p = VaProcPipelineParameterBuffer::convert(7, false, crate::hevc::COLOUR_BT2020_PQ);
        let (i, o) = (&p.input_color_properties, &p.output_color_properties);
        assert_eq!(i.color_range, VA_SOURCE_RANGE_REDUCED);
        assert_eq!(
            (
                i.colour_primaries,
                i.transfer_characteristics,
                i.matrix_coefficients
            ),
            (9, 16, 9)
        );
        assert_eq!(
            (
                o.colour_primaries,
                o.transfer_characteristics,
                o.matrix_coefficients
            ),
            (9, 16, 9)
        );
    }
}
