#!/usr/bin/env bash
# Diff a PUNKTFUNK_DUMP_VIDEO H.265 capture's on-GPU decode against ffmpeg's
# software decode of the same bytes, and name the FIRST divergent frame.
#
# Two halves:
#   1) On the box with the GPU (Deck: prefix RADV_PERFTEST=video_decode):
#        PF_VKD_FIELD_STREAM=/path/au-XXXX.h265 \
#          cargo test -p pf-vkdecode --test gpu_parity --release -- \
#          --ignored field_h265 --nocapture
#      This writes /path/au-XXXX.h265.pfhash (one SHA-256 per frame, display order).
#   2) Anywhere with ffmpeg ≥ 6 (this script):
#        scripts/vkdecode-field-parity.sh /path/au-XXXX.h265
#
# A divergence at frame 0 points at intra decode, the parameter-set conversion or
# readback geometry; a LATER first divergence points at inter prediction,
# reference info or DPB management — and everything after it is probably just
# downstream of that one frame. Re-run half 1 with PF_VKD_FIELD_YUV=<n>,<n+1> to
# dump the divergent frames' planes for visual inspection.
set -euo pipefail

stream="${1:?usage: vkdecode-field-parity.sh <capture.h265> [<capture.h265.pfhash>]}"
pfhash="${2:-$stream.pfhash}"
[ -r "$stream" ] || { echo "no such capture: $stream" >&2; exit 1; }
[ -r "$pfhash" ] || {
  echo "no hash file: $pfhash — run the GPU half first (see the header)" >&2
  exit 1
}

# The GPU leg reads back tightly packed NV12 (8-bit) or P010-layout words
# (10-bit, samples in the high bits) — both exactly what ffmpeg's rawvideo
# packing of these pixel formats produces, so framehash compares 1:1.
pix_fmt=$(ffprobe -v error -select_streams v:0 -show_entries stream=pix_fmt \
  -of default=nw=1:nk=1 "$stream")
case "$pix_fmt" in
  *10*) raw_fmt=p010le ;;
  *) raw_fmt=nv12 ;;
esac

ffhash=$(mktemp)
trap 'rm -f "$ffhash"' EXIT
# framehash data lines are "0, <dts>, <pts>, <duration>, <size>, <bare hex>" — the digest is the
# LAST comma-separated field with no `hash=` prefix of any kind, and every header line starts with
# '#'. Matching on a prefix yields an EMPTY hash file and a "0 frames" report.
ffmpeg -v error -i "$stream" -pix_fmt "$raw_fmt" -f framehash -hash sha256 - \
  | awk -F, '/^[0-9]/ { gsub(/[[:space:]]/, "", $NF); print $NF }' > "$ffhash"

ours=$(wc -l < "$pfhash" | tr -d ' ')
theirs=$(wc -l < "$ffhash" | tr -d ' ')
echo "frames: ours=$ours ffmpeg=$theirs (source pix_fmt=$pix_fmt → $raw_fmt)"
# A count mismatch is expected when the capture starts mid-session (the GPU leg
# skips pre-join AUs and prints how many) — compare the overlapping tail-aligned
# prefix in that case by hand; the common case is equal counts.

first=$(paste -d' ' "$pfhash" "$ffhash" | awk '$1 != $2 { print NR - 1; exit }')
mismatches=$(paste -d' ' "$pfhash" "$ffhash" | awk '$1 != $2' | wc -l | tr -d ' ')
if [ -z "$first" ]; then
  echo "OK: every compared frame is bit-identical to ffmpeg's software decode."
  echo "    (If the smear reproduced during this capture, the defect is NOT in"
  echo "     pf-vkdecode/the driver for these bytes — look upstream of the dump.)"
  exit 0
fi
echo "FIRST DIVERGENT FRAME = $first ($mismatches diverging in total)"
echo "the divergent picture's coded shape, per ffprobe:"
ffprobe -v error -select_streams v:0 -show_frames -read_intervals "%+#$((first + 2))" \
  -show_entries frame=pict_type,key_frame,pts -of csv "$stream" | sed -n "$((first + 1))p"
echo "next: re-run the GPU half with PF_VKD_FIELD_YUV=$first to dump its planes,"
echo "and inspect the divergent AU's slice headers (RPS shape, slice count, LT refs)."
