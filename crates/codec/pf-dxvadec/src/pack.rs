//! Bitstream-buffer packing: slice NALUs into the mapping
//! `GetDecoderBuffer(D3D11_VIDEO_DECODER_BUFFER_BITSTREAM)` returns, plus the
//! byte locations the slice-control records point at.
//!
//! Three DXVA rules (libavcodec `dxva2_h264.c` / `dxva2_hevc.c`):
//!
//! 1. **Every slice is preceded by a three-byte start code** (`00 00 01`).
//!    `BSNALunitDataLocation` points at that start code, not the NALU header.
//!    The ranges are the plan's `nal` ranges, start code dropped, so the packer
//!    writes three bytes whatever prefix the encoder emitted.
//! 2. **`SliceBytesInBuffer` counts the start code**, so it is `3 + NALU bytes`.
//! 3. **Pad the buffer to a 128-byte multiple with zeros**, charged to the last
//!    slice's `SliceBytesInBuffer`. Trailing zeros after `rbsp_trailing_bits`
//!    are legal filler; a driver that reads to the stated length must see the
//!    padding inside the last record, not past it.
//!
//! Non-VCL NALUs never enter: `BSNALunitDataLocation` is defined as a *slice*
//! NALU offset.

use std::ops::Range;

use crate::dxva::BITSTREAM_ALIGN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceRecord {
    /// `BSNALunitDataLocation`: offset of this slice's start code in the buffer.
    pub location: u32,
    /// `SliceBytesInBuffer`: start code + NALU; last slice also includes tail padding.
    pub bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packed {
    pub records: Vec<SliceRecord>,
    /// Bytes written including padding: the bitstream buffer's `DataSize`.
    pub data_size: u32,
}

/// Why packing refused an AU. The caller drops the picture; none of these panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackError {
    NoSlices,
    /// Slice range outside the AU: a plan paired with the wrong buffer. Checked
    /// because the alternative is an out-of-bounds index.
    RangeOutsideAu {
        start: usize,
        end: usize,
        au: usize,
    },
    /// Mapping cannot hold the packed AU. Refuse rather than truncate a picture.
    BufferTooSmall {
        needed: usize,
        capacity: usize,
    },
    /// Offset or length exceeded `u32` (DXVA record width).
    Overflow(usize),
    /// AV1 ([`mod@crate::pack_av1`]): frame carried no tile data.
    NoTiles,
    /// AV1: tile payload is in no tile-group region from the same walk.
    /// Unreachable via [`crate::plan_bitstream`]; the alternative is a
    /// record addressing another tile's bytes.
    TileOutsideGroup {
        start: usize,
        end: usize,
    },
    /// AV1: template record count disagrees with the walk's tile list.
    TileCountMismatch {
        records: usize,
        tiles: usize,
    },
}

impl std::fmt::Display for PackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PackError::NoSlices => write!(f, "the plan holds no slices"),
            PackError::RangeOutsideAu { start, end, au } => {
                write!(
                    f,
                    "slice range {start}..{end} falls outside the {au}-byte AU"
                )
            }
            PackError::BufferTooSmall { needed, capacity } => write!(
                f,
                "the AU needs {needed} bitstream bytes; the driver's buffer holds {capacity}"
            ),
            PackError::Overflow(value) => write!(f, "byte value {value} exceeds u32"),
            PackError::NoTiles => write!(f, "the frame carried no tile data"),
            PackError::TileOutsideGroup { start, end } => write!(
                f,
                "tile payload {start}..{end} lies inside no tile-group region"
            ),
            PackError::TileCountMismatch { records, tiles } => write!(
                f,
                "{records} tile records against {tiles} tiles in the bitstream"
            ),
        }
    }
}

impl std::error::Error for PackError {}

/// Byte count [`pack`] writes before padding: `3 + NAL` per slice.
/// Separate so a caller can size a mapping, and so size cannot drift from pack.
pub fn packed_size(au: &[u8], slices: &[Range<usize>]) -> Result<usize, PackError> {
    if slices.is_empty() {
        return Err(PackError::NoSlices);
    }
    let mut total = 0usize;
    for range in slices {
        let nal = au.get(range.clone()).ok_or(PackError::RangeOutsideAu {
            start: range.start,
            end: range.end,
            au: au.len(),
        })?;
        total = total.saturating_add(3 + nal.len());
    }
    Ok(total)
}

/// Pack the NAL ranges `slices` of `au` into `dst`, each behind `00 00 01`,
/// returning slice-control locations.
///
/// `dst` is the whole mapped bitstream buffer, not a sub-slice: padding needs
/// the real capacity. Short of room for the 128-byte tail, clamp (libavcodec
/// does); the picture is complete either way.
pub fn pack(au: &[u8], slices: &[Range<usize>], dst: &mut [u8]) -> Result<Packed, PackError> {
    let needed = packed_size(au, slices)?;
    if needed > dst.len() {
        return Err(PackError::BufferTooSmall {
            needed,
            capacity: dst.len(),
        });
    }

    let mut records = Vec::with_capacity(slices.len());
    let mut cursor = 0usize;
    for range in slices {
        // `packed_size` already proved it; re-deriving keeps this index in bounds.
        let payload = au.get(range.clone()).ok_or(PackError::RangeOutsideAu {
            start: range.start,
            end: range.end,
            au: au.len(),
        })?;
        let location = u32::try_from(cursor).map_err(|_| PackError::Overflow(cursor))?;
        dst[cursor..cursor + 3].copy_from_slice(&[0, 0, 1]);
        dst[cursor + 3..cursor + 3 + payload.len()].copy_from_slice(payload);
        cursor += 3 + payload.len();
        let bytes =
            u32::try_from(3 + payload.len()).map_err(|_| PackError::Overflow(payload.len()))?;
        records.push(SliceRecord { location, bytes });
    }

    // Pad to the 128-byte granule. `128 - (cursor % 128)` yields a FULL block
    // when data already lands on the granule (libavcodec: never zero padding).
    // Some drivers require at least one padding byte.
    let want = BITSTREAM_ALIGN - (cursor % BITSTREAM_ALIGN);
    let padding = want.min(dst.len() - cursor);
    dst[cursor..cursor + padding].fill(0);
    cursor += padding;
    if let Some(last) = records.last_mut() {
        // Last record so `SliceBytesInBuffer` never walks past `DataSize`.
        last.bytes = last
            .bytes
            .saturating_add(u32::try_from(padding).unwrap_or(u32::MAX));
    }

    Ok(Packed {
        records,
        data_size: u32::try_from(cursor).map_err(|_| PackError::Overflow(cursor))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An AU of `count` NALs behind alternating three- and four-byte start
    /// codes, with the plan's `nal` ranges.
    fn synth_au(count: usize, payload: usize) -> (Vec<u8>, Vec<Range<usize>>) {
        let mut au = Vec::new();
        let mut ranges = Vec::new();
        for i in 0..count {
            if i % 2 == 0 {
                au.extend_from_slice(&[0, 0, 1]);
            } else {
                au.extend_from_slice(&[0, 0, 0, 1]);
            }
            let start = au.len();
            au.extend(std::iter::repeat_n(0xA0 + i as u8, payload));
            ranges.push(start..au.len());
        }
        (au, ranges)
    }

    #[test]
    fn each_slice_is_written_with_a_three_byte_start_code_whatever_the_au_carried() {
        let (au, ranges) = synth_au(2, 5);
        let mut dst = vec![0xCCu8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        assert_eq!(packed.records.len(), 2);
        assert_eq!(packed.records[0].location, 0);
        assert_eq!(&dst[0..3], &[0, 0, 1]);
        assert_eq!(&dst[3..8], &[0xA0; 5]);
        assert_eq!(packed.records[0].bytes, 8);
        assert_eq!(packed.records[1].location, 8);
        assert_eq!(&dst[8..11], &[0, 0, 1]);
        assert_eq!(&dst[11..16], &[0xA1; 5]);
    }

    #[test]
    fn the_buffer_is_padded_to_a_128_byte_granule_and_the_padding_is_charged_to_the_last_slice() {
        let (au, ranges) = synth_au(1, 5);
        let mut dst = vec![0xCCu8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        // 3 + 5 = 8 slice bytes, padded to the 128-byte granule.
        assert_eq!(packed.data_size, 128);
        assert_eq!(packed.records[0].bytes, 128);
        assert!(dst[8..128].iter().all(|&b| b == 0), "padding must be zeros");
        // Past `data_size` the mapping is untouched; the driver never looks there.
        assert_eq!(dst[128], 0xCC);
    }

    #[test]
    fn data_already_on_the_granule_still_gets_a_full_padding_block() {
        // 125-byte payload + 3-byte start code = 128 exactly, so padding is a full block.
        let (au, ranges) = synth_au(1, 125);
        let mut dst = vec![0u8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        assert_eq!(packed.data_size, 256);
        assert_eq!(packed.records[0].bytes, 256);
    }

    #[test]
    fn padding_is_clamped_to_what_the_mapping_can_hold() {
        let (au, ranges) = synth_au(1, 5);
        // 3 + 5 slice bytes and ten spare: clamp padding to the mapping.
        let mut dst = vec![0u8; 18];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        assert_eq!(packed.data_size, 18);
        assert_eq!(packed.records[0].bytes, 18);
    }

    #[test]
    fn an_au_larger_than_the_mapping_is_refused_rather_than_truncated() {
        let (au, ranges) = synth_au(2, 100);
        let mut dst = vec![0u8; 64];
        assert_eq!(
            pack(&au, &ranges, &mut dst),
            Err(PackError::BufferTooSmall {
                needed: 206,
                capacity: 64,
            })
        );
    }

    #[test]
    fn locations_are_contiguous_across_a_multi_slice_au() {
        let (au, ranges) = synth_au(4, 30);
        let mut dst = vec![0u8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        let mut expected = 0u32;
        for (i, record) in packed.records.iter().enumerate() {
            assert_eq!(record.location, expected);
            // Only the last record's `bytes` includes padding; earlier ones stay 33.
            if i + 1 < packed.records.len() {
                assert_eq!(record.bytes, 33);
            }
            expected += 33;
        }
        assert_eq!(packed.data_size, 4 * 33 + (128 - (4 * 33) % 128));
    }

    #[test]
    fn a_range_outside_the_au_is_caught_before_it_indexes() {
        let (au, _) = synth_au(1, 5);
        let mut dst = vec![0u8; 512];
        let au_len = au.len();
        let range = 0usize..au_len + 1;
        assert_eq!(
            pack(&au, std::slice::from_ref(&range), &mut dst),
            Err(PackError::RangeOutsideAu {
                start: 0,
                end: au_len + 1,
                au: au_len,
            })
        );
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let mut dst = vec![0u8; 512];
        assert_eq!(pack(&[], &[], &mut dst), Err(PackError::NoSlices));
        assert_eq!(packed_size(&[], &[]), Err(PackError::NoSlices));
    }

    #[test]
    fn packed_size_agrees_with_what_pack_writes_before_padding() {
        let (au, ranges) = synth_au(3, 17);
        assert_eq!(packed_size(&au, &ranges).unwrap(), 3 * 20);
        let mut dst = vec![0u8; 4096];
        let packed = pack(&au, &ranges, &mut dst).unwrap();
        let unpadded: u32 =
            packed.records.iter().map(|r| r.bytes).sum::<u32>() - (packed.data_size - 3 * 20u32);
        assert_eq!(unpadded, 60);
    }
}
