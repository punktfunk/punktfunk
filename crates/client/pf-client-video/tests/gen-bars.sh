#!/bin/sh
# Regenerate the software rung's colour fixtures (`video_software.rs`'s M8 exit test).
#
# Three single-IDR H.264 streams of the same NINE colour bars, whose VUIs differ ONLY in
# matrix coefficients and range. That is the point: the pictures are DIFFERENT code points
# that must converge on the SAME RGB once the signalled matrix and range are honoured —
# which is what makes the test able to fail against a hardcoded matrix (the swscale BT.601
# default the old libav rung needed correction code for).
#
# ⚠ The NINTH bar (192,128,64) is load-bearing and must not be dropped for tidiness. The
# eight before it are fully saturated primaries/secondaries plus black and white, and on
# THOSE a limited↔full range mistake only pushes values outside [0,1] — where the shader
# clamps — so the whole fixture set decodes with max error 0 under the WRONG range and the
# range axis could not fail. Measured on this fixture: (192,128,64) gives max error 13
# under the wrong range, while a 50% grey gives only 3, which is inside the test's ±4
# tolerance. So it has to be a non-neutral mid-tone, not just a mid-tone.
#
# Not lossless: x264 refuses qp 0 outside High 4:4:4 Predictive, which openh264 cannot
# decode. qp 1 over flat bars is exact to within a code point or two at the bar centres
# the test samples, and the test's tolerance is ±4.
#
# Needs: ffmpeg with libx264. Run from this directory; overwrites the three fixtures.
set -e

python3 - <<'PY'
BARS = [(255,255,255),(255,255,0),(0,255,255),(0,255,0),(255,0,255),(255,0,0),(0,0,255),(0,0,0),
        (192,128,64)]
W, H = 32 * len(BARS), 64
row = bytearray()
for x in range(W):
    row += bytes(BARS[x // 32])
open('bars.rgb', 'wb').write(bytes(row) * H)
print(f'{W}x{H}')
PY

# 288x64: nine 32-px bars. Both dimensions stay macroblock-aligned (18x4), so there is no
# encoder padding for the crop to have to undo.
for spec in "601-limited bt470bg tv" "709-limited bt709 tv" "709-full bt709 pc"; do
  set -- $spec
  name=$1; mtx=$2; rng=$3
  ffmpeg -y -hide_banner -loglevel error -f rawvideo -pix_fmt rgb24 -s 288x64 -i bars.rgb \
    -vf "scale=in_range=full:out_color_matrix=$mtx:out_range=$rng,format=yuv420p" \
    -frames:v 1 -c:v libx264 -qp 1 -profile:v high \
    -x264-params "keyint=1:no-scenecut=1:colorprim=bt709:transfer=bt709:colormatrix=$mtx" \
    -color_primaries bt709 -color_trc bt709 -colorspace "$mtx" -color_range "$rng" \
    -f h264 "bars-$name.h264"
done
rm -f bars.rgb
ls -l bars-*.h264
