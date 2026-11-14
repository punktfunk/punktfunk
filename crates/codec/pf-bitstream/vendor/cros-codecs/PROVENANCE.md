# Vendored: cros-codecs (parser layer only)

- **Upstream:** <https://android.googlesource.com/platform/system/cros-codecs/> (the
  authoritative AOSP tree). Snapshot taken from the read-only GitHub mirror
  <https://github.com/chromeos/cros-codecs>, branch `main`,
  commit **`5ff6d693ffae0b36935b8fc13092c733b4c2646f`**, fetched 2026-08-05.
- **License:** BSD-3-Clause (`LICENSE`, copied verbatim). Attribution headers retained
  in every source file.
- **Why vendored, not a crates.io dependency:** the GitHub repo is a read-only mirror
  and the crates.io release lags it; a pinned, reviewed snapshot is the supply-chain
  posture punktfunk already uses elsewhere (`clients/android/native/vendor/ndk`,
  `pf-inject/vendor/usbip-sim`). Decision of record:
  punktfunk-planning `design/client-native-decode.md` §8.1.

## What was taken

`src/codec/{h264,h265,av1,vp9}` (parsers, DPBs, picture types, NALU/OBU machinery,
their `test_data` vectors — they double as punktfunk's conformance corpus),
`src/bitstream_utils.rs`, `LICENSE`. Upstream designed the `codec` module for exactly
this extraction — its module doc: "There shall be no dependencies from other modules of
this crate to this module, so that it can be turned into a crate of its own if needed
in the future."

## What was left behind

- `decoder/`, `encoder/`, `backend/`, `c2_wrapper/`, `video_frame`, `image_processing`,
  `utils` — the Linux-only halves (libva/v4l2/gbm/nix). punktfunk's `pf-bitstream` +
  `pf-vkdecode` occupy that layer.
- `codec/vp8` — VP9 has no dependency on it (verified) and no punktfunk host will ever
  emit VP8.

## Deviations from pristine upstream

1. `src/lib.rs` — rewritten: keeps only the module decls and `Resolution` /
   `ResolutionRoundMode` (the sole root items `codec` references), both copied verbatim;
   adds crate-level `#![allow(clippy::all, mismatched_lifetime_syntaxes)]` — vendored
   code is not held to the workspace lint bar (CI's `-D warnings` legs would fail on
   upstream style otherwise).
2. `src/codec.rs` — one line removed (`pub mod vp8;`).
3. `Cargo.toml` — rewritten: `log` is the only dependency the vendored subset needs,
   plus `env_logger`/`serde_json` dev-dependencies for upstream's in-tree tests.
4. `cargo fmt` normalization under the workspace's rustfmt config (mechanical only).
5. **Zero-unsafe, enforced**: `#![forbid(unsafe_code)]` added to lib.rs. Upstream's codec
   module had exactly one production `unsafe` (h264/dpb.rs `build_ref_pic_lists`: ref→index
   via pointer `offset_from`) — replaced with a safe `position(ptr::eq)` over the ≤16-entry
   DPB — and three test-only `mem::zeroed()` asserts, replaced with `Default::default()`
   (`PredWeightTable` derives `Default`; all-integer struct, identical value). The layer
   facing untrusted bytes is now compiler-verified free of unsafe — the property that
   motivates replacing libavcodec's C parsers in the first place.
6. `src/codec/h264/picture.rs` — `PictureData::new_from_slice`: `display_resolution`
   computed as `visible_rect.max` instead of `max - min`. `Sps::visible_rectangle()`
   returns the crop offset in `min` and the visible *size* in `max` (see its
   definition: `max.x = width - crop_left - crop_right`); upstream's subtraction
   double-counts the left/top crop and, worse, panics on u32 underflow for a
   large-but-parser-valid `frame_crop_left_offset` (e.g. 100 crop units on a 320-wide
   SPS). Found by pf-bitstream's conformance-window tests; upstream never hits it
   because real encoders crop right/bottom only. **Reported upstream 2026-08-06:
   <https://github.com/chromeos/cros-codecs/issues/99>.**

7. `src/codec/h265/parser.rs` — `parse_slice_header`: reject
   `num_long_term_sps + num_long_term_pics > 16` before the long-term RPS loop.
   Upstream bounds the pair only by `MAX_LONG_TERM_REF_PIC_SETS` (32) combined, while
   every long-term array in `SliceHeader` (`poc_lsb_lt`, `used_by_curr_pic_lt`,
   `delta_poc_msb_present_flag`, `delta_poc_msb_cycle_lt`, `lt_idx_sps`) is `[_; 16]`
   — a hostile slice header with 17+ entries panics the parser with an
   index-out-of-bounds (bounds checks stay on in release). Found by pf-bitstream's
   H.265 planner review; regression-tested there
   (`a_hostile_long_term_count_is_a_parse_error_not_a_panic`). **Reported upstream
   2026-08-06: <https://github.com/chromeos/cros-codecs/issues/100>.**

8. `src/bitstream_utils.rs` — `BitReader::read_bits` accepts 32 bits, and the 31-bit
   limit moves to `read_bits_signed` where its reason lives. Upstream capped the unsigned
   read at 31 "because that would break the read_bits_signed() function" — true of the
   signed path's `i32` accumulator, but it denies the unsigned path a width AV1 requires
   in five places: `timing_info`'s `num_units_in_display_tick` and `time_scale`,
   `decoder_model_info`'s `num_units_in_decoding_tick` (all `f(32)`), and the
   variable-width buffer-delay and `buffer_removal_time` fields, whose lengths are read
   from the stream and reach 32. A sequence header with `timing_info_present_flag` set
   was therefore unparseable — a legal stream that AMD's AMF encoder emits and NVENC does
   not, so **every AV1 session on an AMD host failed**: `AV1 parse: more than 31 (32) bits
   were requested` on the first access unit, then `No sequence header parsed yet` for
   every one after. Upstream's own `BitWriter::write_f` already accepts 32, so the crate
   could emit a header it could not read back.

   Three edits, all required together — the guard alone is not the fix:
   - the trailing mask is `u32::MAX` at 32 (`1u32 << 32` overflows: a debug panic, and in
     release a mask of zero, i.e. a silent `0` return);
   - the byte cursor is advanced before the accumulation loop when it sits at zero
     remaining bits, which otherwise shifts by the full width and ORs the spent byte in.
     At ≤31 bits the mask discarded those bits, so it was invisible; at 32 it cannot;
   - `read_bits_signed` carries its own `> 31` guard, so widening the unsigned path does
     not silently widen the signed one into an overflow.

   Also fixed in the same function: the sign extension `-1 ^ ((1 << num_bits) - 1)`
   overflows at `num_bits == 31` (`1i32 << 31` is `i32::MIN`; subtracting one from it
   panics in debug) — a latent panic at a width the guard admits and upstream's comment
   considered safe. Rewritten as `-1i32 << num_bits`, equal for every accepted width.
   Regression-tested here (`read_thirty_two_bits_*`, `signed_reads_stop_at_thirty_one_bits`,
   `widths_below_thirty_two_are_unchanged_across_a_spent_byte`) and end-to-end as an AV1
   synthesize/parse round trip (`sequence_header_obu_round_trips_timing_info`), which
   reproduces the field error string exactly when the guard is reverted. **Report upstream
   — not yet filed.**

   Not changed: `read_bits_signed(0)` still underflows on `num_bits - 1`. Unreachable —
   AV1 has its own `read_su`, and the H.26x `se(v)` callers all pass positive widths — so
   it is left to upstream rather than widened into this deviation.

9. `src/codec/h265/parser.rs` — `parse_vps` and `parse_sps`: reject
   `{vps,sps}_max_sub_layers_minus1 > 6` immediately after the read. The element is
   `u(3)`, so 7 is representable, but 7.4.3.1 and 7.4.3.2 both bound it at 6 — and
   every array the parser then walks with it is sized for the spec, not for the field:
   `profile_tier_level()`'s `sub_layer_*` flags are `[_; 6]` and the sub-layer ordering
   arrays are `[_; 7]`. A ~20-byte VPS NALU with the field set to 7 panicked inside
   `parse_profile_tier_level` with an index-out-of-bounds before any picture was
   decoded, on every reconnect. Bound at the spec's 6, not at the arrays' 5/6, because
   the two agree there — a conformant stream is never refused. This is also what keeps
   punktfunk's own `sps.max_num_reorder_pics[max_sub_layers_minus1]` and
   `sps.max_dec_pic_buffering_minus1[max_sub_layers_minus1]` reads in bounds
   (`pf-bitstream` h265.rs, and the `pf-vaapi` / `pf-dxvadec` / `pf-vkdecode` picture
   builders downstream of it) — they all take their `Sps` from this parser, so the
   parse-time check is the single choke point and none of them needs its own guard.
   Regression-tested in `pf-bitstream`
   (`a_param_set_claiming_eight_sub_layers_is_a_parse_error_not_a_panic`).
   **Report upstream — not yet filed.**

10. `src/codec/h265/parser.rs` — `parse_scaling_list_data`: read
    `scaling_list_pred_matrix_id_delta` with `read_ue_max(matrixId / factor)` instead of
    an unbounded `read_ue`, which is exactly the range 7.4.5 permits (`0` to
    `matrixId / ( sizeId == 3 ? 3 : 1 )`). Equation 7-42 subtracts
    `delta * factor` from `matrixId` in `u32`: unbounded it underflows — a debug panic
    on the subtraction, and in release a `~4e9` index into the six-entry
    `scaling_list_{4x4,8x8,16x16,32x32}`. Reachable from a PPS with
    `pps_scaling_list_data_present_flag` set, or the equivalent SPS flag. `factor` moved
    a few lines up so the bound and equation 7-42 share one definition; the same bound
    also makes `delta * factor` unable to overflow. Regression-tested in `pf-bitstream`
    (`a_scaling_list_predicting_from_a_negative_matrix_is_a_parse_error_not_a_panic`).
    **Report upstream — not yet filed.**

11. `src/codec/h265/parser.rs` — the tile syntax, three edits, all one defect. `parse_pps`
    bounded `num_tile_{columns,rows}_minus1` by the picture only
    (`pic_{width,height}_in_ctbs_y - 1`, which reaches 2110 on a legal SPS) while using
    them to index `column_width_minus1` / `row_height_minus1`. A PPS on a 2048x2048 SPS
    asking for 26 tile columns panicked. Annex A is the real bound: A.4.1 requires
    `num_tile_columns_minus1 < MaxTileCols` and `num_tile_rows_minus1 < MaxTileRows`,
    and Table A.8 peaks at 20 and 22 (levels 6, 6.1, 6.2). So:

    - new `MAX_TILE_COLUMNS` / `MAX_TILE_ROWS` consts (20, 22), and the counts are read
      with `min(picture bound, const - 1)`;
    - the arrays grew from `[u32; 19]` / `[u32; 21]` to those consts. Upstream sized them
      one short of Table A.8 — the code stores the running remainder in
      `column_width_minus1[num_tile_columns_minus1]`, so the last tile needs a slot —
      which means bounding at the arrays would have refused a conformant 20-column
      stream. Growing by one entry each costs nothing and lets the guard be the spec's
      number rather than an implementation artefact. The upstream test asserting
      `[0; 19]` / `[0; 21]` follows the consts now.

    Raising the ceiling to the spec's exposed two further panics downstream, in
    `parse_slice_header`'s entry-point block (both reachable before this change too, at
    the arrays' old 19x21 ceiling):

    - the `num_entry_point_offsets` maximum computed
      `(num_tile_columns_minus1 + 1) * (num_tile_rows_minus1 + 1) - 1` in `u8`, which
      overflows above 256 tiles — 20x22 is 440. Widened to `u32`, matching the sibling
      branch two lines down;
    - `num_entry_point_offsets` was then bounded by that maximum while
      `entry_point_offset_minus1` is `[u32; 32]`, so a slice claiming 35 entry points
      indexed past it. Clamped to the array, the same way deviation 7 handles the
      long-term arrays. 7.4.7.1 puts no 32-entry cap on the element, so this refuses a
      conformant stream with more than 32 entry points — notably 4K wavefront
      (`entropy_coding_sync_enabled_flag`) streams, which carry one offset per CTB row.
      An error beats a panic, but the real fix is upstream sizing that array from the
      stream.

    Regression-tested in `pf-bitstream`
    (`a_pps_with_more_tiles_than_any_level_allows_is_a_parse_error_not_a_panic`,
    `a_slice_claiming_more_entry_points_than_the_header_holds_is_a_parse_error_not_a_panic`).
    **Report upstream — not yet filed.**

12. `src/codec/av1/parser.rs` — `parse_tile_info`: the two non-uniform tile loops
    (`uniform_tile_spacing_flag == 0`) run until `start_sb` reaches `sb_cols` / `sb_rows`
    while filling `width_in_sbs_minus_1` / `height_in_sbs_minus_1`, which are
    `MAX_TILE_COLS` / `MAX_TILE_ROWS` (64) deep. Each iteration advances `start_sb` by at
    least one superblock, so a frame wide or tall enough — 4096 mi columns is 256
    superblocks — walks 256 entries into a 64-entry array. The uniform branch already
    checks `tile_cols > MAX_TILE_COLS` after the fact and is genuinely bounded before it
    (`tile_cols_log2 <= max_log2_tile_cols <= 6`); the non-uniform branch had neither.
    Guarded at the top of each loop body, returning the same
    `"Invalid tile_{cols,rows} {n}"` the uniform branch does. 64 is the spec's own
    ceiling (`MAX_TILE_COLS` / `MAX_TILE_ROWS` in 3, and a conformance requirement on
    `TileCols` / `TileRows` in 5.11.1), so it is both the array bound and the legal one.
    Regression-tested in the file's own test module
    (`more_non_uniform_tiles_than_the_spec_allows_is_a_parse_error_not_a_panic`).
    **Report upstream — not yet filed.**

13. `src/codec/h264/parser.rs` — `parse_sps`: reject a picture whose macroblock count
    overflows `u32`, with the `checked_mul` idiom the frame-crop validation a few lines
    below already uses. `max_dpb_frames()` computes
    `max_dpb_mbs / (width_mb * height_mb)` (A.3.1); both dimensions are `ue(v)` read into
    `u16`, so each reaches 65536 macroblocks and their product reaches 2^33. In debug
    that is a multiply-overflow panic, in release it wraps — 65536 x 65536 wraps to
    exactly zero — and the division that follows panics on a zero divisor. `max_dpb_frames()`
    returns `usize`, not `Result`, and the DPB and `max_num_order_frames()` both call it,
    so the check belongs at the parse boundary where an `Err` is available. Bounded at
    the arithmetic limit rather than Table A-1's `MaxFS`: the level tables are the only
    range H.264 gives these elements, this parser enforces no other level conformance at
    parse time, and hardware decoders routinely accept a stream whose level_idc
    understates its resolution. Regression-tested in the file's own test module
    (`a_picture_whose_macroblock_count_overflows_is_a_parse_error_not_a_panic`).
    **Report upstream — not yet filed.**

14. `src/codec/av1/parser.rs` — `read_obu`: bound `obu_size` against the buffer before it is
    used to slice. `obu_size` is a leb128 read out of the stream (`read_leb128()? as usize`,
    so anything up to `u32::MAX`) and nothing ties it to the bytes actually present; the OBU
    was then built with an unchecked `&data[start_offset..start_offset + obu_size]`. Any
    access unit whose last OBU declares more payload than remains — a truncated AU, or simply
    an over-declared size — panicked with `range end index .. out of range for slice of length
    ..`. That is a bounds check, not arithmetic, so it panics in release too (the workspace
    leaves `overflow-checks` off, which is why the parser's other unchecked accumulations
    merely wrap), and it aborts whichever thread is decoding.

    Blast radius is every native AV1 rung: `pf-vkdecode`, `pf-dxvadec` and `pf-vaapi` are all
    re-exports of `pf_bitstream::av1::Av1Planner`, whose `plan_au` hands raw access-unit bytes
    straight to this function. Reachable from the project's own `PUNKTFUNK_AU_FAULT=truncate`
    injector — whose `FaultMode::Truncate` docs reason only about Annex-B, where a NALU carries
    no length, while AV1 OBUs do — and from any AU delivered short over the wire.

    This was a gap in an otherwise consistent posture rather than a missing idea: `plan_au`
    degrades every *other* malformation to `PlanWarning::TruncatedAu` or `PlanError::Parse`,
    and `pf-vkdecode` re-validates `obu.end > au.len()` one layer up. Guarded with
    `checked_add` plus a length compare, returning the same `String` error the rest of the
    parser uses; the computed end is reused for `bytes_used` so the slice and the advance can
    no longer disagree. Regression-tested in the file's own test module
    (`an_obu_declaring_more_bytes_than_are_present_is_a_parse_error_not_a_panic`, which
    reproduces the original panic exactly when the guard is reverted) and at the planner
    boundary in `pf-bitstream`
    (`av1::tests::a_truncated_access_unit_is_a_plan_error_not_a_panic`).
    **Not filed upstream.**

15. `src/codec/av1/parser.rs` — `frame_size_with_refs`: refuse a `found_ref` that names a
    slot holding no frame. An inter frame with `frame_size_override_flag` set copies its
    sizes from the named reference; a never-filled slot holds zeros, `compute_image_size`
    then derives `mi_cols = 0`, and `parse_tile_info` divides by the resulting
    `tile_cols`. Bounds checks and division-by-zero panic in release too, on the decode
    thread. Reachable from a mid-stream join on a third-party encoder that codes
    per-frame sizes (punktfunk hosts do not). Refused with the parser's usual `String`
    error; `RefValid` is deliberately not consulted, so an error-resilient frame whose
    `ref_order_hint` disagreed with a slot still parses and lets `pf-bitstream` report
    the stale reference. No regression test: the synthesizer does not write
    `frame_size_with_refs`, and authoring the syntax by hand is out of proportion for a
    two-line zero check. **Not filed upstream.**

16. `src/codec/av1/parser.rs` — `parse_obu_header` and `read_obu`: refuse a set
    `obu_reserved_1bit`, and a low-overhead OBU that clears `obu_has_size_field`, with the
    parser's usual `String` error instead of `assert!`. Both were bare asserts, so they fired
    in release, and `parse_obu_header` runs before any type dispatch — one bit in the first
    byte of any OBU aborted the decoding thread. `Av1Planner::plan_au` degrades every `Err`
    into a `PlanError`, which a panic walked straight past. Same reachability as deviation 14:
    a client decodes whatever its host sends, so this is a remote abort on the client and a
    crash on any corruption that survives FEC. The third assert in `read_obu`
    (`position() % 8 == 0`) is byte-aligned by construction and is now a `debug_assert!`.
    Regression test: `a_reserved_obu_header_bit_is_an_error_not_a_panic` in `pf-bitstream`.
    **Report upstream — not yet filed.**

23. `src/codec/h264/parser.rs` — new `SpsBuilder::bitstream_restriction`. The
    synthesizer already writes `bitstream_restriction_flag`, `max_num_reorder_frames` and
    `max_dec_frame_buffering` (`synthesizer.rs` VUI tail); the builder had no way to set them,
    so no SPS it authored could state a reorder bound. punktfunk's native VAAPI encoder packs
    its own SPS (both radeonsi and iHD require the app to), and stating the bound is what lets a
    client output on it instead of holding pictures until the DPB fills — several frames of
    latency on a stream that reorders nothing. `max_dec_frame_buffering` is floored at
    `max_num_ref_frames` per E.2.1, which is the livelock the sweep's S-77 refuses at parse.
    Regression test: `an_authored_sps_states_the_reorder_bound` in `pf-vaapi`.
    **Report upstream — not yet filed.**

24. `src/codec/h265/dpb.rs` — `find_lowest_poc_for_bumping` returns the lowest-POC entry
    still needed for output, not the first entry sharing its POC. Upstream found the
    lowest POC, then looked the picture up again by `pic_order_cnt_val`. With two
    pictures at one POC it could return the one already output, `bump` changed nothing,
    and `pf-bitstream`'s `bump_as_needed` looped forever on the decode thread. A
    loss-damaged stream stores such a pair: a loopback at 15 % wire loss from an AMD
    (AMF, long-term references) host wedged the client's decoder, and its capture spins
    the planner at the same frame. The H.264 DPB already returns the entry. Regression
    test: `two_pictures_at_one_poc_both_leave_the_output_queue`, in the file's own test
    module. **Report upstream — not yet filed.**

25. `src/codec/h264/dpb.rs` — two edits, one defect. New `Dpb::bump_past_reorder_bound`
    outputs the lowest POC while more pictures wait than `max_num_reorder_frames`, and
    `clear` keeps that bound as it keeps `max_num_pics`. Upstream outputs on a full DPB
    only (C.4.5.3), so a stream that states `max_num_reorder_frames = 0` is still shown
    `max_dec_frame_buffering` pictures late; `clear` zeroed the bound at every IDR, which
    nothing read as a bound until now. The H.265 DPB already outputs on
    `sps_max_num_reorder_pics` (C.5.2.3). Regression tests:
    `a_zero_reorder_stream_shows_each_picture_in_its_own_au` and
    `a_stream_that_states_no_bound_waits_for_a_full_dpb` in `pf-bitstream`.
    **Report upstream — not yet filed.**

Deviations 26 to 31 were found by the `bitstream_plan` fuzz target (`fuzz/`), which runs
this crate with overflow checks off, as it ships: 26, 28, 30 and 31 abort a release client.

26. `src/codec/av1/parser.rs` — `parse_tile_group_obu`: refuse a tile group while the parser
    counts no tiles, and one whose `tg_start..=tg_end` leaves the frame's tile count.
    `tile_rows * tile_cols` is zero until a frame header has run `parse_tile_info`, so a tile
    group ahead of any header computed `num_tiles - 1`, a debug underflow, and then divided
    `tile_num` by a zero `tile_cols`: a division panic in release, from a sequence header and
    a three-byte OBU. `tg_start` and `tg_end` are read `tile_cols_log2 + tile_rows_log2` bits
    wide, which reaches past the tile count whenever it is not a power of two, and the loop
    indexes `mi_row_starts` by the row it derives. 6.10.1 requires `tg_end >= tg_start` and
    ends a frame's last group at `NumTiles - 1`. Regression test:
    `a_tile_group_ahead_of_any_frame_header_is_an_error_not_a_panic` in `pf-bitstream`.
    **Report upstream — not yet filed.**

27. `src/codec/av1/parser.rs` — the tile loop of `parse_tile_group_obu`: `tile_size_minus_1 + 1`
    and the running `sz` use checked arithmetic. A tile larger than what is left of its group
    underflowed `sz`: a debug panic, and in release a wrap that the `skip_bits` behind it
    turned into the error this now returns directly. No release crash, so no regression
    test. **Report upstream — not yet filed.**

28. `src/codec/h265/parser.rs` — `parse_vps`, `parse_sps`, `parse_pps` and
    `parse_slice_header` take their payload with `get(hdr_len..)`. `NaluHeader::parse` reads
    its two bytes from the stream cursor, not from the unit, so a unit cut to one byte ahead
    of another start code still gets a header, its second byte out of that start code, and
    `&data[2..]` on the one-byte unit panicked. That is a bounds check, so it fires in release
    too, from nine bytes. `pf-bitstream`'s own SEI path already read `get(2..)`. Regression
    test: `a_one_byte_unit_is_a_parse_error_not_a_panic` in `pf-bitstream`.
    **Report upstream — not yet filed.**

29. `src/codec/h265/parser.rs` — `parse_pps_range_extension`: the SAO offset scale bound is
    `bit_depth_y.saturating_sub(10)`. Upstream wrote `max(0, bit_depth_y - 10)` on a `u8`,
    which underflows before `max` sees it for every depth under ten: a debug panic, and in
    release a bound of 254 or 255 on a field that must then be zero. No release crash, so no
    regression test. **Report upstream — not yet filed.**

30. `src/codec/h265/parser.rs` — `parse_sps_scc_extension`: the bound on
    `num_palette_predictor_initializer_minus1` is computed in `u32`, and initializers for an
    empty palette are refused. `palette_max_size + delta_palette_max_predictor_size - 1` in
    `u8` wraps to 255 when both are zero; the count then reads up to 255 and the loop fills
    `palette_predictor_initializer`, which is 128 deep: an index panic in release, from a
    60-byte SPS. The two reads above it bound their sum at 128, so the checked form keeps
    every index inside the array. Regression test:
    `an_sps_with_palette_initializers_for_an_empty_palette_is_a_parse_error_not_a_panic` in
    `pf-bitstream`. **Report upstream — not yet filed.**

31. `src/codec/av1/parser.rs` — `parse_frame_header_obu` sets `seen_frame_header` once the
    header has parsed, not before, and `parse_frame_obu` refuses a frame OBU shorter than
    `header_bytes`. Set first, a header that failed to parse left the flag up with
    `last_frame_header` still holding the last good frame's. Until a temporal delimiter
    cleared it, every frame OBU came back as a copy of that header, and `parse_frame_obu`
    sliced its own payload at the old header's length: a bounds panic in release when the new
    OBU is the shorter one. Regression test:
    `a_frame_obu_after_a_failed_header_is_parsed_on_its_own` in `pf-bitstream`.
    **Report upstream — not yet filed.**

32. `src/codec/h264/nalu.rs` — `find_start_code` uses `memchr::memmem::find` (a new
    `memchr = "2"` dependency, already in the workspace tree) instead of a scalar
    `windows(3).position`: the scan runs once per byte of every AU on every client.
    Not a defect; not for upstream.

33. `src/codec/av1/parser.rs` — `FrameHeaderObu::ref_order_hint_bit`: the bit offset in the
    OBU payload where an error-resilient frame codes `ref_order_hint[0]`, recorded by
    `parse_uncompressed_frame_header`. `pf_bitstream::av1::conceal` rewrites those hints in
    place. Not a defect; not for upstream.

34. `src/codec/h265/parser.rs` — `parse_vps`: `vps_num_hrd_parameters` is read with
    `read_ue_max(vps_num_layer_sets_minus1 + 1)`, the range 7.4.3.1 gives it. Unbounded, it
    was the loop count for pushes of an `HrdParams`, ~7.7 KB built from five input bits: a
    64 KB VPS asked for about 1 GB and an allocation failure aborts the process. Regression
    test: `a_vps_claiming_more_hrd_parameters_than_layer_sets_is_a_parse_error` in
    `pf-bitstream`. **Report upstream — not yet filed.**

35. `src/codec/h265/parser.rs` — `parse_short_term_ref_pic_set`: a set is refused when it,
    or the set it predicts from, holds more than `sps_max_dec_pic_buffering_minus1[
    sps_max_sub_layers_minus1 ]` pictures (7.4.8). A derived set holds up to one more entry
    than its source and nothing capped it, so a chain of inter-predicted sets grew by one each
    until `used_by_curr_pic_flag` / `use_delta_flag` (64 deep) indexed out of range at the
    50th, and `delta_poc_s0/s1` (65) one set later: an index panic in release, from a SPS of
    about 270 bytes. The bound is at most 16 (`read_ue_max(16)`), so every index stays
    inside the arrays. The explicit branch reads its counts against the same value.
    Regression test: `chained_inter_rps_sets_stop_at_the_dpb_size_not_at_the_arrays` in
    `pf-bitstream`. **Report upstream — not yet filed.**

36. `src/codec/h265/parser.rs` — `parse_hrd_parameters`: `fixed_pic_rate_within_cvs_flag` is 1
    when `fixed_pic_rate_general_flag` is 1 (E.3.2). Upstream read it only when the general
    flag was 0 and left it false otherwise, so the next element came out as
    `low_delay_hrd_flag` instead of `elemental_duration_in_tc_minus1`, and the rest of the HRD,
    the VUI's `bitstream_restriction` and the SPS extension flags were read a few bits out of
    step. Any encoder that signals a fixed picture rate in its VUI or VPS HRD hits it.
    Regression test: `an_hrd_with_a_fixed_picture_rate_leaves_the_rest_of_the_vui_aligned`
    in `pf-bitstream`. **Report upstream — not yet filed.**

37. `src/codec/h265/parser.rs` — `Slice::replace_header`: a dependent segment keeps its own
    `num_entry_point_offsets` with the offsets it already kept, and takes `curr_rps_idx`,
    `num_pic_total_curr` and `st_rps_bits` from the independent header. Upstream restored those
    three from the dependent parse, which leaves them at their defaults (0, 0 and the SPS's set
    count), so a dependent segment's header described a picture with no references: the list
    builder sized its temporary list from zero and a `list_entry_lX` past the active count
    found nothing. Regression test:
    `a_dependent_segment_keeps_its_entry_points_and_takes_the_pictures_rps_fields` in the
    file's own test module. **Report upstream — not yet filed.**

38. `src/codec/h264/parser.rs` — `Sps::max_dpb_frames`: a VUI `max_dec_frame_buffering` is
    floored at `max_num_ref_frames`. Upstream let the VUI value replace the floored level
    value, so an SPS stating less than E.2.1 allows (an LTR or RFI configuration that raises
    the reference count and not the VUI field) opened a DPB the references alone fill; the
    second reference picture then failed with "DPB is full" and so did every access unit
    after it, until a new SPS arrived. Deviation 23 floors the builder's value; this floors
    the parsed one. Regression test:
    `a_vui_dpb_depth_below_max_num_ref_frames_keeps_the_reference_floor` in `pf-bitstream`.
    **Report upstream — not yet filed.**

39. `src/codec/h264/parser.rs` — `parse_ref_pic_list_modification`: refuse more than
    `num_ref_idx_lX_active_minus1 + 1` operations (7.4.3.1). The loop pushed a 12-byte entry
    per two input bits until the terminating idc 3, so a slice NAL cost about 48 times its
    size in heap before `modify_ref_pic_list` rejected the extra operation. The planner
    rejected the same operation already, as a concealed list; it is now a parse error.
    Regression test: `more_list_modifications_than_active_references_is_a_parse_error` in
    the file's own test module. **Report upstream — not yet filed.**

40. `src/codec/av1/parser.rs` — `parse_global_motion_params`: with `primary_ref_frame` NONE,
    `PrevGmParams` is the identity for `LAST_FRAME..=ALTREF_FRAME` and all six parameters
    (7.20 `setup_past_independence`). Upstream stopped before AltRef and before parameter 5,
    so AltRef's rotation/zoom and any reference's affine parameter 5 were decoded against a
    zero reference instead of the identity: a wrong warp matrix into the hardware. Bit
    consumption is unaffected, so the stream stays in step. Regression test:
    `global_motion_without_a_primary_ref_frame_starts_from_the_identity_on_altref` in the
    file's own test module. **Report upstream — not yet filed.**

41. `src/codec/av1/parser.rs` — `read_obu`: `ObuAction::Drop` carries the whole OBU
    (`obu_end`), not the header and size field. The caller adds the value to its offset, so a
    dropped OBU's payload was then read as OBU headers: a parse error or garbage for any
    stream whose operating point 0 excludes a layer. The Annex B end-of-data return at the
    top of the function has no payload to skip and is unchanged. Regression test:
    `a_dropped_obu_advances_past_its_payload` in the file's own test module.
    **Report upstream — not yet filed.**

42. `src/bitstream_utils.rs` — `BitReader::skip_bits` moves whole bytes in one step on a
    byte boundary of a reader without emulation prevention. It read 31 bits at a time, four
    one-byte cursor reads each, about 6 ns per skipped byte, and AV1 skips every tile payload
    but the last this way on the decode thread. Position, remaining bits and the refusal past
    the end are unchanged. Regression test:
    `skip_bits_lands_where_reading_the_same_bits_would` in the file's own test module. Not a
    defect; not for upstream.

43. `src/codec/h264/nalu.rs` — `Nalu::next`: the `zero_byte` of a four-byte start code is
    taken only when it lies at or after the cursor (`start_code_offset > pos`, was `> 0`).
    An end-of-sequence or end-of-bitstream NAL is sized as its header alone, so a caller that
    steps the cursor to the NAL's end stands right behind its second header byte. When that
    byte is zero (`nuh_temporal_id_plus1 = 0`, which `NaluHeader::parse` accepts) and a start
    code follows at once, the next NAL's start reached back into the NAL just read:
    `00 00 01 48 00 00 00 01`. `pf-bitstream`'s `PocWidener::widen` slices between the two
    and panicked, in release too, on the Android client's decode thread. Regression test:
    `a_start_code_right_behind_an_end_nal_starts_after_it` in `pf-bitstream`
    (`h265/poc_widen.rs`). **Report upstream — not yet filed.**

44. `src/codec/h265/parser.rs` — `parse_pps`: the explicit tile sizes
    (`uniform_spacing_flag == 0`) read each `column_width_minus1` / `row_height_minus1` with
    a maximum of `remainder - 1`, where the running remainder sits in the last slot. A PPS
    whose earlier tiles use the picture up leaves a remainder of zero with tiles still to
    read: the subtraction panicked in debug and wrapped in release, so widths that run past
    the picture reached the Vulkan, D3D11VA and V4L2 picture builders. 6.5.1 gives every
    tile at least one CTB, so a zero remainder there is refused with `checked_sub`.
    Regression test: `a_pps_whose_tiles_run_past_the_picture_is_a_parse_error_not_a_panic`
    in `pf-bitstream`. **Report upstream — not yet filed.**

Re-sync procedure: fetch the AOSP tree, re-apply this trim, diff `codec/` +
`bitstream_utils.rs` (expect near-zero conflicts), update the commit pin above.
