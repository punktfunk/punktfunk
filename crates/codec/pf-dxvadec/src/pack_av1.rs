//! AV1 bitstream buffer and the tile-control records that address it.
//! Counterpart of [`mod@crate::pack`]: AV1 has no Annex-B start codes and no slices.
//!
//! The buffer is every tile-group (or frame) OBU's `tile_data` region, concatenated
//! in plan order: from the first tile's `tile_size_minus_1` through the end of the
//! OBU payload. Not the OBU header, not `obu_size`, not an `OBU_FRAME` frame header
//! — those live in `DXVA_PicParams_AV1`. Inter-tile `tile_size_minus_1` fields ride
//! along unread. [`Av1Bitstream::groups`] is that region.
//!
//! Vulkan uploads tile payloads alone (size fields stripped). Both are correct:
//! both APIs address tiles by an explicit (offset, size) pair. This path matches
//! libavcodec `dxva2_av1.c`.
//!
//! Two rules that differ from the H.264/HEVC packer:
//!
//! 1. **Padding is charged to nobody.** The 128-byte granule is added to the
//!    buffer's `DataSize` only. Copying H.264's last-record habit here would tell
//!    the driver the last tile is up to 128 bytes longer than it is. An AV1 tile's
//!    size is exact; its entropy decoder is not looking for a stop bit.
//! 2. **One record per TILE, not per tile group.** See [`TileAv1`].

use pf_bitstream::av1::tiles::Av1Bitstream;

use crate::dxva::BITSTREAM_ALIGN;
use crate::dxva_av1::TileAv1;
use crate::pack::PackError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedAv1 {
    /// Caller's rows, columns, and `anchor_frame`; `DataOffset`/`DataSize`
    /// rewritten to the packed buffer.
    pub tiles: Vec<TileAv1>,
    /// Bytes written including padding: the bitstream buffer's `DataSize`.
    pub data_size: u32,
}

/// Byte count [`pack_av1`] writes before padding: every tile-group region.
/// Separate so size cannot drift from pack.
pub fn packed_size_av1(bitstream: &Av1Bitstream) -> usize {
    bitstream.groups.iter().fold(0usize, |total, group| {
        total.saturating_add(group.end.saturating_sub(group.start))
    })
}

/// Pack one frame's tile data into `dst`, returning tile-control records.
///
/// `tiles` is the [`crate::plan_to_dxva_av1`] template: rows, columns, and
/// `anchor_frame` pass through; AU-relative `DataOffset`/`DataSize` are both
/// replaced so no record can come out half-rebased.
///
/// `dst` is the whole mapped bitstream buffer. Short of room for the 128-byte
/// tail, clamp (libavcodec does); the picture is complete either way.
pub fn pack_av1(
    au: &[u8],
    bitstream: &Av1Bitstream,
    tiles: &[TileAv1],
    dst: &mut [u8],
) -> Result<PackedAv1, PackError> {
    if bitstream.tiles.is_empty() || bitstream.groups.is_empty() {
        return Err(PackError::NoTiles);
    }
    if tiles.len() != bitstream.tiles.len() {
        return Err(PackError::TileCountMismatch {
            records: tiles.len(),
            tiles: bitstream.tiles.len(),
        });
    }
    let needed = packed_size_av1(bitstream);
    if needed > dst.len() {
        return Err(PackError::BufferTooSmall {
            needed,
            capacity: dst.len(),
        });
    }

    // Groups copied end to end. A tile's offset is its position inside its
    // group plus that group's base (`ctx_pic->bitstream_size + tile_offset`).
    let mut cursor = 0usize;
    let mut bases = Vec::with_capacity(bitstream.groups.len());
    for group in &bitstream.groups {
        let bytes = au.get(group.clone()).ok_or(PackError::RangeOutsideAu {
            start: group.start,
            end: group.end,
            au: au.len(),
        })?;
        dst[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        bases.push((group.clone(), cursor));
        cursor += bytes.len();
    }

    let mut records = Vec::with_capacity(tiles.len());
    for (tile, template) in bitstream.tiles.iter().zip(tiles) {
        // Containment, not re-derived per-group counts: a second split that
        // disagreed would rebase a tile against the wrong group's base.
        let (group, base) = bases
            .iter()
            .find(|(group, _)| group.start <= tile.start && tile.end <= group.end)
            .ok_or(PackError::TileOutsideGroup {
                start: tile.start,
                end: tile.end,
            })?;
        let offset = base + (tile.start - group.start);
        let size = tile.end - tile.start;
        records.push(TileAv1 {
            data_offset: u32::try_from(offset).map_err(|_| PackError::Overflow(offset))?,
            data_size: u32::try_from(size).map_err(|_| PackError::Overflow(size))?,
            ..*template
        });
    }

    // Pad to 128 bytes; already-aligned data still gets a FULL block.
    // Charged to the buffer `DataSize`, never to a tile record.
    let want = BITSTREAM_ALIGN - (cursor % BITSTREAM_ALIGN);
    let padding = want.min(dst.len() - cursor);
    dst[cursor..cursor + padding].fill(0);
    cursor += padding;

    Ok(PackedAv1 {
        tiles: records,
        data_size: u32::try_from(cursor).map_err(|_| PackError::Overflow(cursor))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxva_av1::UNUSED_INDEX;

    /// Ranges from `(start, end)` pairs. Not `vec![a..b]`: clippy reads a
    /// one-element `Vec<Range>` as a mistyped `vec![a; b]`.
    fn ranges<const N: usize>(pairs: [(usize, usize); N]) -> Vec<std::ops::Range<usize>> {
        pairs.into_iter().map(|(start, end)| start..end).collect()
    }

    /// Template with a known row/column; `0xDEAD_BEEF` offsets must not survive pack.
    fn template(row: u16, column: u16) -> TileAv1 {
        TileAv1 {
            data_offset: 0xDEAD_BEEF,
            data_size: 0xDEAD_BEEF,
            row,
            column,
            reserved16: 0,
            anchor_frame: UNUSED_INDEX,
            reserved8: 0,
        }
    }

    /// Two one-tile groups. Single-tile groups encode no size field, so each
    /// group's region is its tile.
    fn two_groups() -> (Vec<u8>, Av1Bitstream) {
        let mut au = vec![0u8; 64];
        for (i, byte) in au.iter_mut().enumerate() {
            *byte = i as u8;
        }
        (
            au,
            Av1Bitstream {
                tiles: ranges([(10, 20), (40, 55)]),
                groups: ranges([(10, 20), (40, 55)]),
            },
        )
    }

    #[test]
    fn the_tile_data_regions_are_concatenated_and_the_offsets_follow_them() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0xCCu8; 512];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(&dst[0..10], &au[10..20]);
        assert_eq!(&dst[10..25], &au[40..55]);
        assert_eq!(
            (packed.tiles[0].data_offset, packed.tiles[0].data_size),
            (0, 10)
        );
        assert_eq!(
            (packed.tiles[1].data_offset, packed.tiles[1].data_size),
            (10, 15),
            "the second group's tile is rebased onto the first group's length, \
             which is `ctx_pic->bitstream_size + tile_offset`"
        );
        assert_eq!((packed.tiles[1].row, packed.tiles[1].column), (0, 1));
        let anchor = packed.tiles[1].anchor_frame;
        assert_eq!(anchor, UNUSED_INDEX);
    }

    #[test]
    fn a_tile_inside_a_group_keeps_its_distance_from_the_group_start() {
        // Size fields (`tile_size_minus_1`) are copied and never addressed —
        // libavcodec's layout; a payload-only packer would strip them.
        let au: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let bitstream = Av1Bitstream {
            tiles: ranges([(102, 120), (122, 160)]),
            groups: ranges([(100, 160)]),
        };
        let mut dst = vec![0u8; 512];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(
            &dst[0..60],
            &au[100..160],
            "the WHOLE region, size fields and all"
        );
        assert_eq!(
            (packed.tiles[0].data_offset, packed.tiles[0].data_size),
            (2, 18)
        );
        assert_eq!(
            (packed.tiles[1].data_offset, packed.tiles[1].data_size),
            (22, 38)
        );
    }

    #[test]
    fn the_padding_is_charged_to_the_buffer_and_to_no_tile_record() {
        // 25 bytes pad to 128; each tile's `DataSize` stays exact. Trailing
        // zeros in an AV1 tile are not filler — they corrupt the entropy decode.
        let (au, bitstream) = two_groups();
        let mut dst = vec![0xCCu8; 512];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(packed.data_size, 128);
        assert_eq!(packed.tiles.iter().map(|t| t.data_size).sum::<u32>(), 25);
        assert!(
            dst[25..128].iter().all(|&b| b == 0),
            "padding must be zeros"
        );
        assert_eq!(
            dst[128], 0xCC,
            "past the data size the mapping is untouched"
        );
    }

    #[test]
    fn data_already_on_the_granule_still_gets_a_full_padding_block() {
        // `128 - (size & 127)` never yields zero, so 128 bytes of data report 256.
        let au: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        let bitstream = Av1Bitstream {
            tiles: ranges([(0, 128)]),
            groups: ranges([(0, 128)]),
        };
        let mut dst = vec![0u8; 512];
        let packed = pack_av1(&au, &bitstream, &[template(0, 0)], &mut dst).unwrap();
        assert_eq!(packed.data_size, 256);
        // `TileAv1` is `#[repr(packed)]`: copy the field before comparing it.
        let size = packed.tiles[0].data_size;
        assert_eq!(size, 128);
    }

    #[test]
    fn padding_is_clamped_to_what_the_mapping_can_hold() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0u8; 30];
        let packed =
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst).unwrap();
        assert_eq!(packed.data_size, 30);
    }

    #[test]
    fn an_au_larger_than_the_mapping_is_refused_rather_than_truncated() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0u8; 16];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst),
            Err(PackError::BufferTooSmall {
                needed: 25,
                capacity: 16,
            })
        );
        assert_eq!(packed_size_av1(&bitstream), 25);
    }

    #[test]
    fn a_region_outside_the_au_is_caught_before_it_indexes() {
        let au = vec![0u8; 32];
        let bitstream = Av1Bitstream {
            tiles: ranges([(10, 40)]),
            groups: ranges([(10, 40)]),
        };
        let mut dst = vec![0u8; 512];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0)], &mut dst),
            Err(PackError::RangeOutsideAu {
                start: 10,
                end: 40,
                au: 32,
            })
        );
    }

    #[test]
    fn a_tile_that_belongs_to_no_group_is_refused_rather_than_rebased_against_group_zero() {
        // Walk cannot produce this; the alternative is a record pointing at another tile.
        let au: Vec<u8> = (0..200u32).map(|i| i as u8).collect();
        let bitstream = Av1Bitstream {
            tiles: ranges([(10, 20), (150, 160)]),
            groups: ranges([(10, 20)]),
        };
        let mut dst = vec![0u8; 512];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0), template(0, 1)], &mut dst),
            Err(PackError::TileOutsideGroup {
                start: 150,
                end: 160,
            })
        );
    }

    #[test]
    fn a_record_count_that_disagrees_with_the_tile_count_is_refused() {
        let (au, bitstream) = two_groups();
        let mut dst = vec![0u8; 512];
        assert_eq!(
            pack_av1(&au, &bitstream, &[template(0, 0)], &mut dst),
            Err(PackError::TileCountMismatch {
                records: 1,
                tiles: 2,
            })
        );
    }

    #[test]
    fn an_empty_plan_is_refused() {
        let mut dst = vec![0u8; 512];
        let empty = Av1Bitstream {
            tiles: Vec::new(),
            groups: Vec::new(),
        };
        assert_eq!(
            pack_av1(&[], &empty, &[], &mut dst),
            Err(PackError::NoTiles)
        );
        assert_eq!(packed_size_av1(&empty), 0);
    }
}
