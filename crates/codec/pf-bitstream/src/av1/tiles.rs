//! Spec `tile_group_obu()` (AV1 5.11.1): a plan's tile OBUs cut into per-tile
//! payload ranges. The Vulkan, DXVA and VA-API backends all submit from this
//! walk; none re-derives the header or size-field arithmetic.

use std::ops::Range;

use cros_codecs::codec::av1::parser::FrameHeaderObu;

use super::TilePlan;

/// Spec `obu_type` for a standalone tile group.
const OBU_TILE_GROUP: u8 = 4;
/// Spec `obu_type` for a frame header plus its tile group.
const OBU_FRAME: u8 = 6;

/// Malformed tile OBUs. Feeding the whole OBU as payload would treat headers
/// and `tile_size_minus_1` as entropy data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Av1TileError {
    Truncated {
        obu: usize,
    },
    NotAnObu {
        obu: usize,
    },
    /// Only `OBU_TILE_GROUP` and `OBU_FRAME` carry tiles.
    UnexpectedObu {
        obu: usize,
        obu_type: u8,
    },
    NoTiles,
    /// `obu_size` disagrees with the plan range. Last-tile size is implicit
    /// (whatever remains), so this is the only independent end-of-payload check.
    SizeMismatch {
        obu: usize,
        declared_end: usize,
        ranged_end: usize,
    },
    Overflow,
    /// More tiles than the backend's submission arrays hold.
    TooManyTiles {
        tiles: usize,
    },
}

impl std::fmt::Display for Av1TileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Av1TileError::Truncated { obu } => {
                write!(f, "tile OBU {obu} runs past the access unit")
            }
            Av1TileError::NotAnObu { obu } => {
                write!(f, "tile OBU {obu} has obu_forbidden_bit set")
            }
            Av1TileError::UnexpectedObu { obu, obu_type } => {
                write!(
                    f,
                    "tile OBU {obu} has type {obu_type}, which carries no tiles"
                )
            }
            Av1TileError::NoTiles => write!(f, "the frame header codes no tiles"),
            Av1TileError::SizeMismatch {
                obu,
                declared_end,
                ranged_end,
            } => write!(
                f,
                "tile OBU {obu} declares its payload ending at {declared_end}, the \
                 plan's range ends at {ranged_end}"
            ),
            Av1TileError::Overflow => {
                write!(f, "a tile offset or size exceeds what a submission encodes")
            }
            Av1TileError::TooManyTiles { tiles } => {
                write!(f, "{tiles} tiles exceed what one submission carries")
            }
        }
    }
}

impl std::error::Error for Av1TileError {}

/// One frame's tile payload ranges in access-unit coordinates — these are the
/// uploaded bytes, so packed offsets are the concatenation with no rebase.
///
/// [`Self::groups`] is the `tile_data` region per tile-group OBU (size fields
/// included). Vulkan and VA-API submit [`Self::tiles`]; DXVA uploads the
/// groups and must not re-walk the same spec arithmetic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1Bitstream {
    pub tiles: Vec<Range<usize>>,
    /// Per tile-group OBU: first `tile_size_minus_1` through OBU payload end.
    /// Every [`Self::tiles`] range sits in exactly one of these.
    pub groups: Vec<Range<usize>>,
}

fn leb128(au: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    // Spec caps leb128() at 8 bytes; a ninth continuation is malformed.
    for i in 0..8 {
        let byte = *au.get(at + i)?;
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Walk the plan's tile OBUs into per-tile payload ranges.
///
/// `TilePlan::data` is a whole tile-group (or frame) OBU; backends want the
/// payloads only. `header_bytes` locates the tile-group start in an
/// `OBU_FRAME`; `TileInfo` supplies `NumTiles`, tg bit widths, and
/// `TileSizeBytes`.
///
/// Last-tile size is implicit, so a walk always ends flush and "sizes add up"
/// is not a check. [`Av1TileError::SizeMismatch`] is `obu_size` vs the plan
/// range; overshoot is [`Av1TileError::Truncated`]; undershoot shortens the
/// last tile with nothing in the bitstream to contradict it.
pub fn plan_bitstream(
    au: &[u8],
    plan_tiles: &[TilePlan],
    header: &FrameHeaderObu,
) -> Result<Av1Bitstream, Av1TileError> {
    let tile_info = &header.tile_info;
    let num_tiles = tile_info
        .tile_cols
        .checked_mul(tile_info.tile_rows)
        .unwrap_or(0);
    if num_tiles == 0 {
        return Err(Av1TileError::NoTiles);
    }

    let mut tiles: Vec<Range<usize>> = Vec::with_capacity(num_tiles as usize);
    let mut groups: Vec<Range<usize>> = Vec::with_capacity(plan_tiles.len());
    for (obu, tile_group) in plan_tiles.iter().enumerate() {
        let (obu_type, payload) = obu_payload(au, &tile_group.data, obu)?;
        let data = tile_data(au, obu_type, payload, header, num_tiles, obu)?;
        groups.push(data.clone());
        // Bound by NumTiles: a tg_end past NumTiles would walk off the payload.
        let count = tile_group
            .tg_end
            .checked_sub(tile_group.tg_start)
            .and_then(|span| span.checked_add(1))
            .filter(|count| *count <= num_tiles)
            .ok_or(Av1TileError::Truncated { obu })? as usize;
        let size_bytes = tile_info.tile_size_bytes as usize;
        split_tiles(au, data, count, size_bytes, obu, &mut tiles)?;
    }

    if tiles.is_empty() {
        return Err(Av1TileError::NoTiles);
    }
    Ok(Av1Bitstream { tiles, groups })
}

/// The OBU header of plan group `obu`: its `obu_type` and the payload range
/// after the header and size field.
fn obu_payload(
    au: &[u8],
    range: &Range<usize>,
    obu: usize,
) -> Result<(u8, Range<usize>), Av1TileError> {
    if range.end > au.len() || range.start >= range.end {
        return Err(Av1TileError::Truncated { obu });
    }
    let first = au[range.start];
    if first & 0x80 != 0 {
        return Err(Av1TileError::NotAnObu { obu });
    }
    let obu_type = (first >> 3) & 0x0f;
    let extension_flag = (first >> 2) & 1 == 1;
    let has_size_field = (first >> 1) & 1 == 1;
    let mut cursor = range
        .start
        .checked_add(1 + usize::from(extension_flag))
        .ok_or(Av1TileError::Truncated { obu })?;
    // Plan range is header + obu_size. Cross-check when the size field is
    // present (the only independent end); Annex-B omits it and the range stands.
    let payload_end = range.end;
    if has_size_field {
        let (size, len) = leb128(au, cursor).ok_or(Av1TileError::Truncated { obu })?;
        cursor += len;
        let declared_end = cursor
            .checked_add(usize::try_from(size).map_err(|_| Av1TileError::Overflow)?)
            .ok_or(Av1TileError::Overflow)?;
        if declared_end != payload_end {
            return Err(Av1TileError::SizeMismatch {
                obu,
                declared_end,
                ranged_end: payload_end,
            });
        }
    }
    if cursor >= payload_end {
        return Err(Av1TileError::Truncated { obu });
    }
    Ok((obu_type, cursor..payload_end))
}

/// The `tile_data` region of one OBU payload: past the frame header of an
/// `OBU_FRAME` and the tile-group start/end fields.
fn tile_data(
    au: &[u8],
    obu_type: u8,
    payload: Range<usize>,
    header: &FrameHeaderObu,
    num_tiles: u32,
    obu: usize,
) -> Result<Range<usize>, Av1TileError> {
    let mut cursor = payload.start;
    // OBU_FRAME: skip the frame header. The driver reads it from the picture
    // parameters; the bitstream buffer holds tile payloads only.
    match obu_type {
        OBU_FRAME => {
            cursor = cursor
                .checked_add(header.header_bytes)
                .ok_or(Av1TileError::Truncated { obu })?;
        }
        OBU_TILE_GROUP => {}
        other => {
            return Err(Av1TileError::UnexpectedObu {
                obu,
                obu_type: other,
            })
        }
    }
    if cursor >= payload.end {
        return Err(Av1TileError::Truncated { obu });
    }

    // Flag is coded only when NumTiles > 1. Read it; do not infer from the
    // plan's tg_start/tg_end — 0/NumTiles-1 has two spellings of different length.
    let tile_info = &header.tile_info;
    let mut header_bits = 0usize;
    if num_tiles > 1 {
        let present = au[cursor] & 0x80 != 0;
        header_bits += 1;
        if present {
            header_bits += 2 * (tile_info.tile_cols_log2 + tile_info.tile_rows_log2) as usize;
        }
    }
    cursor += header_bits.div_ceil(8);
    if cursor >= payload.end {
        return Err(Av1TileError::Truncated { obu });
    }
    // `tile_data` starts here (`AV1RawTileGroup::tile_data` / DXVA memcpy).
    Ok(cursor..payload.end)
}

/// Cut `count` tiles out of `data`: each but the last carries a
/// le(`size_bytes`) `tile_size_minus_1`; the last takes the remainder.
fn split_tiles(
    au: &[u8],
    data: Range<usize>,
    count: usize,
    size_bytes: usize,
    obu: usize,
    tiles: &mut Vec<Range<usize>>,
) -> Result<(), Av1TileError> {
    // `TileSizeBytes` is 1..=4 when NumTiles > 1; 5.9.15 does not code it
    // for a single-tile frame (parser leaves 0).
    if count > 1 && !(1..=4).contains(&size_bytes) {
        return Err(Av1TileError::Overflow);
    }
    let mut cursor = data.start;
    for tile in 0..count {
        let last = tile + 1 == count;
        let size = if last {
            data.end
                .checked_sub(cursor)
                .ok_or(Av1TileError::Truncated { obu })?
        } else {
            // le(TileSizeBytes) inside the OBU, not merely inside the AU.
            if cursor + size_bytes > data.end {
                return Err(Av1TileError::Truncated { obu });
            }
            let mut value = 0usize;
            for byte in 0..size_bytes {
                value |= usize::from(au[cursor + byte]) << (8 * byte);
            }
            cursor += size_bytes;
            value + 1
        };
        let end = cursor
            .checked_add(size)
            .ok_or(Av1TileError::Truncated { obu })?;
        if end > data.end {
            return Err(Av1TileError::Truncated { obu });
        }
        tiles.push(cursor..end);
        cursor = end;
    }
    debug_assert_eq!(
        cursor, data.end,
        "the last tile's size is the payload remainder by construction"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use cros_codecs::bitstream_utils::IvfIterator;
    use cros_codecs::codec::av1::parser::ObuAction;
    use cros_codecs::codec::av1::parser::ParsedObu;
    use cros_codecs::codec::av1::parser::Parser;

    use super::*;
    use crate::av1::Av1Planner;
    use crate::testing::AV1_25FPS;

    /// [`plan_bitstream`] vs the parser's independent `Tile::tile_offset` /
    /// `tile_size`. A whole-OBU or off-by-header split fails here.
    #[test]
    fn every_tile_of_the_vector_splits_to_the_parsers_own_offsets_and_sizes() {
        let mut planner = Av1Planner::new();
        // Second parser: `Cow::Borrowed` slices point into the packet, so offsets
        // are pointer differences — no re-walk of the same arithmetic.
        let mut reference = Parser::default();
        let (mut frames, mut tiles_checked) = (0u32, 0u32);
        let mut frame_obus = 0u32;

        for packet in IvfIterator::new(AV1_25FPS) {
            let mut expected: Vec<Range<usize>> = Vec::new();
            let mut consumed = 0usize;
            while consumed < packet.len() {
                let action = reference
                    .read_obu(&packet[consumed..])
                    .expect("the clean vector parses");
                let obu = match action {
                    ObuAction::Process(obu) => obu,
                    ObuAction::Drop(n) => {
                        consumed += n as usize;
                        continue;
                    }
                };
                consumed += obu.bytes_used;
                match reference.parse_obu(obu).expect("the clean vector parses") {
                    ParsedObu::Frame(frame) => {
                        frame_obus += 1;
                        let payload = frame.tile_group.obu.as_ref();
                        let base = payload.as_ptr() as usize - packet.as_ptr() as usize;
                        for tile in &frame.tile_group.tiles {
                            let start = base + tile.tile_offset as usize;
                            expected.push(start..start + tile.tile_size as usize);
                        }
                        // Advance parser ref state or later inter frames fail.
                        if !frame.header.show_existing_frame {
                            reference
                                .ref_frame_update(&frame.header)
                                .expect("the clean vector updates");
                        }
                    }
                    ParsedObu::TileGroup(tg) => {
                        let payload = tg.obu.as_ref();
                        let base = payload.as_ptr() as usize - packet.as_ptr() as usize;
                        for tile in &tg.tiles {
                            let start = base + tile.tile_offset as usize;
                            expected.push(start..start + tile.tile_size as usize);
                        }
                    }
                    ParsedObu::FrameHeader(fh) if !fh.show_existing_frame => {
                        reference
                            .ref_frame_update(&fh)
                            .expect("the clean vector updates");
                    }
                    _ => {}
                }
            }

            let mut produced: Vec<Range<usize>> = Vec::new();
            for plan in planner.plan_au(packet).expect("the clean vector plans") {
                if plan.dpb.stored.is_none() {
                    continue;
                }
                frames += 1;
                let bitstream = plan_bitstream(packet, &plan.tiles, &plan.header)
                    .expect("every tile group splits");
                produced.extend(bitstream.tiles);
            }
            tiles_checked += produced.len() as u32;
            assert_eq!(
                produced, expected,
                "the split disagrees with the parser's own tile offsets/sizes"
            );
        }

        assert_eq!(frames, 274, "every frame of the vector must split");
        assert_eq!(
            tiles_checked, 274,
            "this vector is one tile per frame; the count pins that the comparison \
             above actually compared something"
        );
        assert!(
            frame_obus > 0,
            "the vector must exercise the OBU_FRAME path — where the frame header \
             sits INSIDE the tile OBU and the split has to step over it"
        );
    }

    /// Hand-built two-tile group: the vendored vector is one tile per frame, so
    /// `tile_size_minus_1` / present-flag / header alignment need this.
    fn two_tile_group(flag_present: bool) -> (Vec<u8>, FrameHeaderObu, Vec<TilePlan>) {
        let mut header = FrameHeaderObu::default();
        header.tile_info.tile_cols = 2;
        header.tile_info.tile_rows = 1;
        header.tile_info.tile_cols_log2 = 1;
        header.tile_info.tile_rows_log2 = 0;
        header.tile_info.tile_size_bytes = 2;

        // NumTiles > 1 codes the present flag. Clear: one padded bit. Set:
        // flag + tg_start/tg_end at 1 bit each = 3 bits, still one byte.
        let tg_header: u8 = if flag_present {
            // flag=1, tg_start=0, tg_end=1 from the MSB.
            0b1010_0000
        } else {
            0b0000_0000
        };
        let tile0 = [0xA1u8, 0xA2, 0xA3];
        let tile1 = [0xB1u8, 0xB2];
        let mut payload = vec![tg_header];
        // le(TileSizeBytes=2) of tile_size_minus_1 for every tile but the last.
        payload.extend_from_slice(&[(tile0.len() as u8) - 1, 0]);
        payload.extend_from_slice(&tile0);
        payload.extend_from_slice(&tile1);

        // obu_header: OBU_TILE_GROUP, no extension, has_size_field.
        let mut au = vec![0x22u8, payload.len() as u8];
        let payload_start = au.len();
        au.extend_from_slice(&payload);
        let tiles = vec![TilePlan {
            data: 0..au.len(),
            tg_start: 0,
            tg_end: 1,
        }];
        assert_eq!(payload_start, 2);
        (au, header, tiles)
    }

    #[test]
    fn a_multi_tile_group_splits_at_the_coded_tile_sizes() {
        for flag_present in [false, true] {
            let (au, header, tiles) = two_tile_group(flag_present);
            let bitstream = plan_bitstream(&au, &tiles, &header).expect("splits");
            let ranges = bitstream.tiles;
            // 2 OBU header + 1 tile-group header + 2 size bytes = 5.
            assert_eq!(ranges, vec![5..8, 8..10], "flag_present={flag_present}");
            assert_eq!(&au[ranges[0].clone()], &[0xA1, 0xA2, 0xA3]);
            assert_eq!(&au[ranges[1].clone()], &[0xB1, 0xB2]);
        }

        // Overshoot is refused. Undershoot cannot be: the last tile absorbs it.
        let (mut au, header, tiles) = two_tile_group(false);
        au[3] = 0x40; // tile_size_minus_1 = 64 ⇒ 65 bytes in an 8-byte payload
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Truncated { obu: 0 })
        );

        // TileSizeBytes is coded only for multi-tile; width 0 would read nothing.
        let (au, mut header, tiles) = two_tile_group(false);
        header.tile_info.tile_size_bytes = 0;
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Overflow)
        );
        header.tile_info.tile_size_bytes = 9;
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Overflow),
            "a width past 4 would overflow the shift"
        );

        let (au, header, mut tiles) = two_tile_group(false);
        tiles[0].tg_end = 7;
        assert_eq!(
            plan_bitstream(&au, &tiles, &header),
            Err(Av1TileError::Truncated { obu: 0 })
        );
    }

    #[test]
    fn an_obu_whose_declared_size_disagrees_with_the_plans_range_is_refused() {
        let mut planner = Av1Planner::new();
        let packet = IvfIterator::new(AV1_25FPS).next().expect("a first packet");
        let plan = planner
            .plan_au(packet)
            .expect("plans")
            .into_iter()
            .next()
            .expect("a frame");
        assert!(plan_bitstream(packet, &plan.tiles, &plan.header).is_ok());

        // Plan range shortened by one; bitstream `obu_size` still names the old
        // end. Last-tile size is implicit, so this is the only end check.
        let mut damaged = plan.tiles.clone();
        damaged[0].data.end -= 1;
        assert!(
            matches!(
                plan_bitstream(packet, &damaged, &plan.header),
                Err(Av1TileError::SizeMismatch { .. })
            ),
            "a range disagreeing with obu_size must be refused"
        );

        let start = plan.tiles[0].data.start;
        let mut au = packet.to_vec();
        // OBU_METADATA (5) in the type field.
        au[start] = (au[start] & !0x78) | (5 << 3);
        assert_eq!(
            plan_bitstream(&au, &plan.tiles, &plan.header),
            Err(Av1TileError::UnexpectedObu {
                obu: 0,
                obu_type: 5
            })
        );

        let mut no_tiles = (*plan.header).clone();
        no_tiles.tile_info.tile_cols = 0;
        assert_eq!(
            plan_bitstream(packet, &plan.tiles, &no_tiles),
            Err(Av1TileError::NoTiles)
        );
    }

    #[test]
    fn a_leb128_without_a_terminator_is_refused_rather_than_read_forever() {
        // Spec caps leb128() at eight continuation bytes.
        let au = [0x80u8; 16];
        assert_eq!(leb128(&au, 0), None);
        let au = [0x81u8, 0x02];
        assert_eq!(leb128(&au, 0), Some((0x101, 2)));
        assert_eq!(leb128(&[0x80], 0), None);
        assert_eq!(leb128(&[], 0), None);
    }
}
