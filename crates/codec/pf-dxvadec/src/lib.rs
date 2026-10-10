//! Native D3D11VA (DXVA) H.264/HEVC/AV1 decode for the Windows clients, counterpart of `pf-vkdecode`.
//!
//! CPU-testable half: everything between a [`pf_bitstream`] access-unit plan and the
//! bytes `ID3D11VideoContext::SubmitDecoderBuffers` delivers. It never names a D3D11,
//! COM, or Windows type. The `cfg(windows)` rung lives in `pf-client-core`; this crate
//! holds the layouts, packing, picture-parameter conversion, and descriptor policy.
//!
//! [`dxva`] hand-declares the `dxva.h` buffers (windows-rs does not generate them);
//! compile-time size/offset proofs stand in for the header. [`pack`] and [`mod@pack_av1`]
//! write the bitstream buffer: H.264/HEVC normalise start codes and charge 128-byte
//! tail padding to the last slice record; AV1 concatenates tile-group `tile_data` and
//! charges padding to the buffer only. [`pic`] / [`pic_h265`] / [`pic_av1`] fill
//! picture parameters, matrices, and slice/tile control through a DPB [`SlotMap`].
//!
//! [`SlotMap`] is re-exported from [`pf_bitstream`]: `DXVA_PicEntry::Index7Bits` is a
//! decode-surface index with the same lifetime the map already models. Construction
//! is field-by-field from `const fn zeroed()`, never `mem::zeroed`. The crate has no
//! `unsafe`: [`dxva::as_bytes`] / [`dxva::slice_bytes`] are `bytemuck` casts over
//! `Pod` layouts. Evidence: `design/client-native-decode.md`.

pub mod config;
pub mod descriptors;
pub mod dxva;
pub mod dxva_av1;
pub mod pack;
pub mod pack_av1;
pub mod pic;
pub mod pic_av1;
pub mod pic_h265;

/// Spec-literal `tile_group_obu()` ranges (AV1 5.11.1). Shared with Vulkan;
/// [`Av1Bitstream::groups`] is the DXVA-only half — see [`mod@pack_av1`].
pub use pf_bitstream::av1::tiles::plan_bitstream;
pub use pf_bitstream::av1::tiles::Av1Bitstream;
pub use pf_bitstream::av1::tiles::Av1TileError;
pub use pf_bitstream::h265::RefRpsIdxError;
/// DPB slot ledger, re-exported from [`pf_bitstream`] (crate docs).
pub use pf_bitstream::slots::SlotError;
pub use pf_bitstream::slots::SlotMap;

// DXVA submit is synchronous (`BeginFrame`…`EndFrame`); there is no decoder
// object here. Re-exports let the Windows layer name planner types without a
// pf-bitstream dependency of its own.
/// AV1 planner. ⚠ `plan_au` returns a **`Vec`**: an AV1 access unit is a temporal
/// unit and may carry several frames, of which at most one displays.
pub use pf_bitstream::av1::AuPlan as AuPlanAv1;
pub use pf_bitstream::av1::Av1Planner;
pub use pf_bitstream::av1::FrameType as FrameTypeAv1;
pub use pf_bitstream::av1::PicId as PicIdAv1;
pub use pf_bitstream::av1::PlanError as PlanErrorAv1;
pub use pf_bitstream::av1::PlanWarning as PlanWarningAv1;
pub use pf_bitstream::av1::NUM_REF_SLOTS;
pub use pf_bitstream::h264::AuPlan;
pub use pf_bitstream::h264::ColourDescription;
pub use pf_bitstream::h264::DisplayCrop;
pub use pf_bitstream::h264::H264Planner;
pub use pf_bitstream::h264::PlanError;
pub use pf_bitstream::h264::PlanWarning;
/// H.265 planner. Separate types: its warning enum is not H.264's, and a
/// per-codec dispatch must name both.
pub use pf_bitstream::h265::AuPlan as AuPlanH265;
pub use pf_bitstream::h265::H265Planner;
pub use pf_bitstream::h265::PlanError as PlanErrorH265;
pub use pf_bitstream::h265::PlanWarning as PlanWarningH265;

pub use config::align_surface;
pub use config::pick_config;
pub use config::pool_size;
pub use config::profile_for;
pub use config::short_slice_config;
pub use config::surface_alignment;
pub use config::Codec;
pub use config::ConfigFacts;
pub use config::DxvaProfile;
pub use config::AV1_VLD_PROFILE0;
pub use config::AV1_VLD_PROFILE0_10BIT;
pub use config::DXGI_FORMAT_NV12;
pub use config::DXGI_FORMAT_P010;
pub use config::H264_VLD_NOFGT;
pub use config::HEVC_VLD_MAIN;
pub use config::HEVC_VLD_MAIN10;
pub use descriptors::descriptors_av1;
pub use descriptors::descriptors_h264;
pub use descriptors::descriptors_h265;
pub use descriptors::BufferDescriptor;
pub use descriptors::BUFFER_BITSTREAM;
pub use descriptors::BUFFER_INVERSE_QUANTIZATION_MATRIX;
pub use descriptors::BUFFER_PICTURE_PARAMETERS;
pub use descriptors::BUFFER_SLICE_CONTROL;
pub use dxva::as_bytes;
pub use dxva::slice_bytes;
pub use dxva::PicParamsH264;
pub use dxva::PicParamsHevc;
pub use dxva::QmatrixH264;
pub use dxva::QmatrixHevc;
pub use dxva::SliceH264Short;
pub use dxva::SliceHevcShort;
pub use dxva::BITSTREAM_ALIGN;
pub use dxva_av1::PicParamsAv1;
pub use dxva_av1::TileAv1;
pub use pack::pack;
pub use pack::packed_size;
pub use pack::PackError;
pub use pack::Packed;
pub use pack::SliceRecord;
pub use pack_av1::pack_av1;
pub use pack_av1::packed_size_av1;
pub use pack_av1::PackedAv1;
pub use pic::plan_to_dxva;
pub use pic::slice_control;
pub use pic::DecodePlanDxva;
pub use pic::DxvaRef;
pub use pic::PlanToDxvaError;
pub use pic_av1::plan_to_dxva_av1;
pub use pic_av1::DecodePlanDxvaAv1;
pub use pic_av1::PlanToDxvaAv1Error;
pub use pic_h265::plan_to_dxva_h265;
pub use pic_h265::slice_control_h265;
pub use pic_h265::DecodePlanDxvaH265;
pub use pic_h265::DxvaRefH265;
pub use pic_h265::PlanToDxvaH265Error;
