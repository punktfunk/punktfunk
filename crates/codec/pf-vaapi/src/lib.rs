//! Native VAAPI H.264/HEVC/AV1 decode for the Linux clients, counterpart of
//! `pf-vkdecode` and `pf-dxvadec`.
//!
//! CPU-testable half: everything between a [`pf_bitstream`] access-unit plan and
//! the buffers `vaRenderPicture` delivers. It links no libva, names no `VA*`
//! handle type, and compiles on macOS. The `cfg(target_os = "linux")` rung lives
//! in `pf-client-core`; this crate holds layouts, conversion, and export walk.
//!
//! [`va`] / [`va_h265`] / [`va_av1`] hand-declare the libva decode buffers;
//! sizes and offsets are measured by `layout-probe.c` and pinned as compile-time
//! assertions. [`config`] picks profile, render-target format, and surface count.
//! [`pic`] / [`pic_h265`] / [`pic_av1`] fill picture parameters, matrices, and
//! slice or tile records. [`drm`] is the one structure the DRIVER writes: the
//! PRIME export descriptor and the plane walk a dmabuf import consumes.
//!
//! [`SlotMap`] is re-exported from [`pf_bitstream`]. VAAPI's extra hop is that
//! `VAPictureH264::picture_id` is a `VASurfaceID`, not a slot index, so conversion
//! takes the caller's surface table and stays pure. Evidence: `layout-probe.c`
//! and the compile-time assertions in [`va`], [`va_h265`], [`va_av1`], and [`drm`].

// `forbid(unsafe_code)` is the CPU-testable contract: no libva, no raw deref of
// the hand-declared `repr(C)` mirrors. A pointer walk here would make this the
// GPU half, untestable off a Linux box.
#![forbid(unsafe_code)]

pub mod config;
pub mod drm;
/// H.264 encode parameter buffers — the encode mirror of [`va`].
pub mod enc_h264;
/// HEVC encode parameter buffers and the driver feature words.
pub mod enc_h265;
/// One description of the stream, feeding both the packed headers and the VA buffer.
pub mod enc_params;
/// HEVC parameter sets, slice headers and HDR10 SEI, written from one description.
pub mod hevc;
pub mod pic;
pub mod pic_av1;
pub mod pic_h265;
pub mod va;
pub mod va_av1;
pub mod va_h265;
/// VideoProc: the ingest colour conversion and the dmabuf import attributes.
pub mod vpp;

/// DPB slot ledger, re-exported from [`pf_bitstream`] (crate docs).
pub use pf_bitstream::slots::SlotError;
pub use pf_bitstream::slots::SlotMap;

/// AV1 planner. ⚠ `plan_au` returns a **`Vec`**: an AV1 access unit is a temporal
/// unit and may carry several frames, of which at most one displays.
pub use pf_bitstream::av1::AuPlan as AuPlanAv1;
pub use pf_bitstream::av1::Av1Planner;
pub use pf_bitstream::av1::DpbUpdate as DpbUpdateAv1;
pub use pf_bitstream::av1::FrameType as FrameTypeAv1;
pub use pf_bitstream::av1::ParsedFrameHeader as ParsedFrameHeaderAv1;
pub use pf_bitstream::av1::ParsedSequenceHeader as ParsedSequenceHeaderAv1;
pub use pf_bitstream::av1::PicId as PicIdAv1;
pub use pf_bitstream::av1::PicturePlan as PicturePlanAv1;
pub use pf_bitstream::av1::PlanError as PlanErrorAv1;
pub use pf_bitstream::av1::PlanWarning as PlanWarningAv1;
pub use pf_bitstream::av1::NUM_REF_SLOTS;
/// H.264/H.265 planners, re-exported so the Linux layer names them through
/// `pf_vaapi` and does not grow a pf-bitstream dependency of its own.
pub use pf_bitstream::h264::AuPlan;
pub use pf_bitstream::h264::ColourDescription;
pub use pf_bitstream::h264::DisplayCrop;
pub use pf_bitstream::h264::H264Planner;
pub use pf_bitstream::h264::PlanError;
pub use pf_bitstream::h264::PlanWarning;
pub use pf_bitstream::h265::AuPlan as AuPlanH265;
pub use pf_bitstream::h265::H265Planner;
pub use pf_bitstream::h265::PlanError as PlanErrorH265;
pub use pf_bitstream::h265::PlanWarning as PlanWarningH265;

pub use drm::flatten;
pub use drm::ExportError;
pub use drm::ExportedPlane;
pub use drm::ExportedSurface;
pub use drm::VaDrmPrimeSurfaceDescriptor;
pub use drm::VA_EXPORT_SURFACE_READ_ONLY;
pub use drm::VA_EXPORT_SURFACE_SEPARATE_LAYERS;
pub use drm::VA_FOURCC_NV12;
pub use drm::VA_FOURCC_P010;

pub use config::profile_for;
pub use config::rt_format;
pub use config::surface_count;
pub use config::Codec;
pub use config::ConfigError;
pub use config::VaProfile;
pub use config::AV1_MAX_DPB_FRAMES;
pub use config::VA_ENTRYPOINT_VLD;
pub use pic::plan_to_va;
pub use pic::DecodePlanVa;
pub use pic::PlanToVaError;
pub use pic_av1::plan_to_va_av1;
pub use pic_av1::DecodePlanVaAv1;
pub use pic_av1::PlanToVaAv1Error;
pub use pic_av1::TileGroupVa;
pub use pic_h265::plan_to_va_h265;
pub use pic_h265::DecodePlanVaH265;
pub use pic_h265::PlanToVaH265Error;
pub use va::PicFieldsH264;
pub use va::SeqFieldsH264;
pub use va::VaIqMatrixBufferH264;
pub use va::VaPictureH264;
pub use va::VaPictureParameterBufferH264;
pub use va::VaSliceParameterBufferH264;

// ⚠ TEST-ONLY surface readback. Production exports a PRIME dmabuf; the only
// caller is `video_vaapi_native::parity` under `#[cfg(test)]`. Declared here
// so the harness needs no `libva-dev` (`va` module docs).
pub use va::pack_two_plane;
pub use va::packed_len;
pub use va::ImageReadError;
pub use va::VaImage;
pub use va::VaImageFormat;
pub use va::VA_LSB_FIRST;
