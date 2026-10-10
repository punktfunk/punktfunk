//! Policy for the buffers passed to `ID3D11VideoContext::SubmitDecoderBuffers`.
//!
//! Order matches FFmpeg's DXVA path: picture parameters, optional inverse-
//! quantization matrix, bitstream, slice/tile control. Every `DataOffset` is
//! zero. Every `DataSize` is bytes written: full parameter structs, padded
//! bitstream, exact slice/tile records (H.264/HEVC short records are 10 bytes).
//! `NumMBsInBuffer` is H.264's macroblock count on bitstream and slice control
//! only; it is zero on parameter/matrix buffers and on every HEVC/AV1 buffer.
//! H.264 always submits a matrix, HEVC only when `qmatrix` is present, AV1
//! never — so AV1 submissions are always three buffers.
//! The Windows layer must preserve these values and this order.

use std::mem::size_of;

use crate::dxva::PicParamsH264;
use crate::dxva::PicParamsHevc;
use crate::dxva::QmatrixH264;
use crate::dxva::QmatrixHevc;
use crate::dxva::SliceH264Short;
use crate::dxva::SliceHevcShort;
use crate::dxva_av1::PicParamsAv1;
use crate::dxva_av1::TileAv1;
use crate::pack::Packed;
use crate::pack_av1::PackedAv1;
use crate::pic::DecodePlanDxva;
use crate::pic_h265::DecodePlanDxvaH265;

/// `D3D11_VIDEO_DECODER_BUFFER_PICTURE_PARAMETERS`.
pub const BUFFER_PICTURE_PARAMETERS: u32 = 0;
/// `D3D11_VIDEO_DECODER_BUFFER_INVERSE_QUANTIZATION_MATRIX`.
pub const BUFFER_INVERSE_QUANTIZATION_MATRIX: u32 = 4;
/// `D3D11_VIDEO_DECODER_BUFFER_SLICE_CONTROL`.
pub const BUFFER_SLICE_CONTROL: u32 = 5;
/// `D3D11_VIDEO_DECODER_BUFFER_BITSTREAM`.
pub const BUFFER_BITSTREAM: u32 = 6;

/// The four `D3D11_VIDEO_DECODER_BUFFER_DESC` fields this backend decides.
/// The other ten (motion-compensation, encryption, reserved) stay zero via
/// `..Default::default()` on the Windows side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferDescriptor {
    /// `BufferType` / DXVA `CompressedBufferType`. One of `BUFFER_*`.
    pub buffer_type: u32,
    pub data_offset: u32,
    /// Bytes written into the driver's mapping, not the mapping's capacity.
    pub data_size: u32,
    pub num_mbs_in_buffer: u32,
}

impl BufferDescriptor {
    const fn new(buffer_type: u32, data_size: u32, num_mbs_in_buffer: u32) -> BufferDescriptor {
        BufferDescriptor {
            buffer_type,
            data_offset: 0,
            data_size,
            num_mbs_in_buffer,
        }
    }
}

/// `n` short-format records, matching [`crate::dxva::slice_bytes`].
/// Saturating: a `u32` holds 429 million ten-byte records; the packer already
/// refused an AU that large.
fn slice_control_size(record_size: usize, records: usize) -> u32 {
    u32::try_from(record_size.saturating_mul(records)).unwrap_or(u32::MAX)
}

pub fn descriptors_h264(plan: &DecodePlanDxva, packed: &Packed) -> Vec<BufferDescriptor> {
    let mb_count = plan.mb_count;
    vec![
        BufferDescriptor::new(
            BUFFER_PICTURE_PARAMETERS,
            size_of::<PicParamsH264>() as u32,
            0,
        ),
        BufferDescriptor::new(
            BUFFER_INVERSE_QUANTIZATION_MATRIX,
            size_of::<QmatrixH264>() as u32,
            0,
        ),
        BufferDescriptor::new(BUFFER_BITSTREAM, packed.data_size, mb_count),
        BufferDescriptor::new(
            BUFFER_SLICE_CONTROL,
            slice_control_size(size_of::<SliceH264Short>(), packed.records.len()),
            mb_count,
        ),
    ]
}

pub fn descriptors_h265(plan: &DecodePlanDxvaH265, packed: &Packed) -> Vec<BufferDescriptor> {
    let mut out = Vec::with_capacity(4);
    out.push(BufferDescriptor::new(
        BUFFER_PICTURE_PARAMETERS,
        size_of::<PicParamsHevc>() as u32,
        0,
    ));
    if plan.qmatrix.is_some() {
        out.push(BufferDescriptor::new(
            BUFFER_INVERSE_QUANTIZATION_MATRIX,
            size_of::<QmatrixHevc>() as u32,
            0,
        ));
    }
    out.push(BufferDescriptor::new(BUFFER_BITSTREAM, packed.data_size, 0));
    out.push(BufferDescriptor::new(
        BUFFER_SLICE_CONTROL,
        slice_control_size(size_of::<SliceHevcShort>(), packed.records.len()),
        0,
    ));
    out
}

/// AV1: three buffers, never a matrix. Slice control is `DXVA_Tile_AV1`
/// (16 bytes per tile), not a 10-byte slice record. Bitstream `DataSize` is
/// the packer's padded size — the only place AV1 padding is accounted
/// ([`mod@crate::pack_av1`]).
pub fn descriptors_av1(packed: &PackedAv1) -> Vec<BufferDescriptor> {
    vec![
        BufferDescriptor::new(
            BUFFER_PICTURE_PARAMETERS,
            size_of::<PicParamsAv1>() as u32,
            0,
        ),
        BufferDescriptor::new(BUFFER_BITSTREAM, packed.data_size, 0),
        BufferDescriptor::new(
            BUFFER_SLICE_CONTROL,
            slice_control_size(size_of::<TileAv1>(), packed.tiles.len()),
            0,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxva::PicEntry;
    use crate::pack::SliceRecord;
    use crate::pic::DxvaRef;

    /// Stub plan: only `mb_count` is read. Whole-stream evidence lives in
    /// `tests/libav_picparams_parity.rs`.
    fn h264_plan(mb_count: u32) -> DecodePlanDxva {
        DecodePlanDxva {
            pic_params: PicParamsH264::zeroed(),
            qmatrix: QmatrixH264::zeroed(),
            slice_ranges: Vec::new(),
            setup_slot: 0,
            setup_id: 1,
            setup_is_reference: true,
            release_after_decode: Vec::new(),
            refs: Vec::<DxvaRef>::new(),
            mb_count,
        }
    }

    fn h265_plan(qmatrix: Option<QmatrixHevc>) -> DecodePlanDxvaH265 {
        DecodePlanDxvaH265 {
            pic_params: PicParamsHevc::zeroed(),
            qmatrix,
            slice_ranges: Vec::new(),
            setup_slot: 0,
            setup_id: 1,
            setup_is_reference: true,
            refs: Vec::new(),
        }
    }

    fn packed(slices: usize, data_size: u32) -> Packed {
        Packed {
            records: (0..slices)
                .map(|i| SliceRecord {
                    location: i as u32 * 64,
                    bytes: 64,
                })
                .collect(),
            data_size,
        }
    }

    #[test]
    fn the_buffer_type_code_points_are_the_ones_windows_rs_declares() {
        // windows-rs `D3D11_VIDEO_DECODER_BUFFER_*` code points; 1..=3 are unused
        // here. A swap hands the driver a bitstream where it expects slice control.
        assert_eq!(BUFFER_PICTURE_PARAMETERS, 0);
        assert_eq!(BUFFER_INVERSE_QUANTIZATION_MATRIX, 4);
        assert_eq!(BUFFER_SLICE_CONTROL, 5);
        assert_eq!(BUFFER_BITSTREAM, 6);
    }

    #[test]
    fn an_h264_submission_carries_four_buffers_in_libavcodecs_order() {
        let descs = descriptors_h264(&h264_plan(300), &packed(2, 512));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_INVERSE_QUANTIZATION_MATRIX,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ]
        );
        assert_eq!(descs[0].data_size, 1040);
        assert_eq!(descs[1].data_size, 224);
        assert_eq!(descs[2].data_size, 512);
        assert_eq!(descs[3].data_size, 2 * 10, "two ten-byte short records");
    }

    #[test]
    fn only_the_h264_bitstream_and_slice_control_buffers_carry_a_macroblock_count() {
        let descs = descriptors_h264(&h264_plan(300), &packed(1, 256));
        assert_eq!(descs[0].num_mbs_in_buffer, 0, "picture parameters");
        assert_eq!(descs[1].num_mbs_in_buffer, 0, "quantization matrices");
        assert_eq!(descs[2].num_mbs_in_buffer, 300, "bitstream");
        assert_eq!(descs[3].num_mbs_in_buffer, 300, "slice control");
    }

    #[test]
    fn an_hevc_submission_omits_the_quantization_matrix_buffer_when_there_is_none() {
        let descs = descriptors_h265(&h265_plan(None), &packed(1, 384));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ],
            "a submission with no matrix must not carry an empty matrix buffer"
        );
        assert!(descs
            .iter()
            .all(|d| d.buffer_type != BUFFER_INVERSE_QUANTIZATION_MATRIX));
    }

    #[test]
    fn an_hevc_submission_carries_the_quantization_matrix_buffer_when_there_is_one() {
        let descs = descriptors_h265(&h265_plan(Some(QmatrixHevc::zeroed())), &packed(3, 640));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_INVERSE_QUANTIZATION_MATRIX,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ]
        );
        assert_eq!(descs[0].data_size, 232);
        assert_eq!(descs[1].data_size, 1000);
        assert_eq!(descs[2].data_size, 640);
        assert_eq!(descs[3].data_size, 3 * 10, "three ten-byte short records");
    }

    #[test]
    fn the_hevc_descriptors_carry_no_macroblock_count_at_all() {
        // libavcodec writes 0 here; a CTB count would be the other-direction miss.
        for descs in [
            descriptors_h265(&h265_plan(None), &packed(1, 256)),
            descriptors_h265(&h265_plan(Some(QmatrixHevc::zeroed())), &packed(4, 1024)),
        ] {
            for desc in descs {
                assert_eq!(
                    desc.num_mbs_in_buffer, 0,
                    "buffer type {} carries a macroblock count",
                    desc.buffer_type
                );
            }
        }
    }

    #[test]
    fn every_descriptor_starts_at_byte_zero_of_its_own_buffer() {
        let h264 = descriptors_h264(&h264_plan(1), &packed(2, 256));
        let h265 = descriptors_h265(&h265_plan(Some(QmatrixHevc::zeroed())), &packed(2, 256));
        for desc in h264.into_iter().chain(h265) {
            assert_eq!(desc.data_offset, 0);
        }
    }

    #[test]
    fn the_slice_control_size_is_one_short_format_record_per_slice() {
        // Short-format packed size is 10, not 12 (`#[repr(C)] {u32,u32,u16}`).
        // Long format is an order of magnitude larger.
        assert_eq!(size_of::<SliceH264Short>(), 10);
        assert_eq!(size_of::<SliceHevcShort>(), 10);
        for slices in [1usize, 2, 5, 68] {
            let h264 = descriptors_h264(&h264_plan(1), &packed(slices, 4096));
            assert_eq!(h264[3].data_size, 10 * slices as u32);
            let h265 = descriptors_h265(&h265_plan(None), &packed(slices, 4096));
            assert_eq!(h265[2].data_size, 10 * slices as u32);
        }
    }

    fn packed_av1(tiles: usize, data_size: u32) -> PackedAv1 {
        PackedAv1 {
            tiles: (0..tiles)
                .map(|i| TileAv1 {
                    data_offset: i as u32 * 64,
                    data_size: 64,
                    row: 0,
                    column: i as u16,
                    ..Default::default()
                })
                .collect(),
            data_size,
        }
    }

    #[test]
    fn an_av1_submission_carries_three_buffers_and_never_a_quantization_matrix() {
        // `dxva2_av1_end_frame` passes `NULL, 0` for the matrix; a fourth buffer
        // would invent one AV1 does not transmit.
        let descs = descriptors_av1(&packed_av1(1, 384));
        assert_eq!(
            descs.iter().map(|d| d.buffer_type).collect::<Vec<_>>(),
            vec![
                BUFFER_PICTURE_PARAMETERS,
                BUFFER_BITSTREAM,
                BUFFER_SLICE_CONTROL,
            ]
        );
        assert_eq!(descs[0].data_size, 912, "DXVA_PicParams_AV1, measured");
        assert_eq!(descs[1].data_size, 384);
        assert_eq!(descs[2].data_size, 16, "one sixteen-byte DXVA_Tile_AV1");
    }

    #[test]
    fn the_av1_descriptors_carry_no_macroblock_count_at_all() {
        // Not a tile count: that is the symmetric-looking wrong value.
        for tiles in [1usize, 4, 64] {
            for desc in descriptors_av1(&packed_av1(tiles, 4096)) {
                assert_eq!(
                    desc.num_mbs_in_buffer, 0,
                    "buffer type {} carries a macroblock count",
                    desc.buffer_type
                );
                assert_eq!(desc.data_offset, 0);
            }
        }
    }

    #[test]
    fn the_av1_tile_buffer_is_sixteen_bytes_per_tile_not_ten() {
        // `DXVA_Tile_AV1` is 16 bytes (`dxva.h`); H.264/HEVC short records are 10.
        assert_eq!(size_of::<TileAv1>(), 16);
        for tiles in [1usize, 2, 8, 64] {
            let descs = descriptors_av1(&packed_av1(tiles, 4096));
            assert_eq!(descs[2].data_size, 16 * tiles as u32);
        }
    }

    #[test]
    fn a_reference_entry_in_the_plan_does_not_reach_the_descriptors() {
        let mut plan = h264_plan(300);
        plan.refs.push(DxvaRef {
            slot: 2,
            id: 7,
            is_long_term: true,
            top_field_order_cnt: 4,
            bottom_field_order_cnt: 4,
            frame_num_or_lt_idx: 1,
        });
        plan.pic_params.RefFrameList[0] = PicEntry::new(2, true);
        assert_eq!(
            descriptors_h264(&plan, &packed(1, 256)),
            descriptors_h264(&h264_plan(300), &packed(1, 256))
        );
    }
}
