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

Re-sync procedure: fetch the AOSP tree, re-apply this trim, diff `codec/` +
`bitstream_utils.rs` (expect near-zero conflicts), update the commit pin above.
