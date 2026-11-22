#!/bin/sh
# Live check of HDR on a macOS host's virtual display, over loopback: test bars drawn above
# white on the display must reach the client at their own brightness, white at 203 nits.
# Usage: macos-hdr-check.sh <punktfunk-host> <punktfunk-probe> [port]
#        macos-hdr-check.sh --read <stream.h265>     (judge a stream a run left behind)
#
# Needs swiftc, ffmpeg and python3, a logged-in desktop session, and Screen Recording for the
# process that runs the host (the terminal it inherits from). A display named Punktfunk sits
# beside the others for about 20 s and shows the bars; the other displays stay as they are.
# Sends no input.
set -u

usage="usage: $0 <punktfunk-host> <punktfunk-probe> [port] | --read <stream.h265>"
abs() { case $1 in /*) echo "$1" ;; *) echo "$PWD/$1" ;; esac; }
plain() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
sleep_s() { perl -e "select(undef,undef,undef,$1)"; }
need() {
    for tool in "$@"; do
        command -v "$tool" >/dev/null || { echo "$tool is not installed"; exit 2; }
    done
}
[ "$(uname -s)" = Darwin ] || { echo "macOS only"; exit 2; }
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/pf-hdr-check.XXXXXX") || exit 2

# One HDR session on a virtual display that shows the bars; leaves `$WORK/stream.h265`.
session() {
    host=$1 probe=$2 port=$3
    swiftc -O -o "$WORK/bars" "$HERE/../crates/host/pf-capture/examples/mac_hdr_bars.swift" \
        2>"$WORK/swiftc.log" || { echo "the bar pattern did not build: $WORK/swiftc.log"; exit 2; }
    for side in host probe; do
        mkdir -p "$WORK/$side/config/punktfunk" "$WORK/$side/data" "$WORK/$side/state" \
            "$WORK/$side/cache" "$WORK/$side/tmp"
    done
    # Extend: the virtual display joins the desk and no other display changes.
    printf '{"version":1,"preset":"custom","topology":"extend"}\n' \
        >"$WORK/host/config/punktfunk/display-settings.json"
    (
        cd "$WORK/host/tmp" || exit 2
        export PUNKTFUNK_CONFIG_DIR="$WORK/host/config/punktfunk"
        export XDG_CONFIG_HOME="$WORK/host/config" XDG_DATA_HOME="$WORK/host/data"
        export XDG_STATE_HOME="$WORK/host/state" XDG_CACHE_HOME="$WORK/host/cache"
        export TMPDIR="$WORK/host/tmp" PUNKTFUNK_KWIN_VIRTUAL_PRIMARY=0
        export PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client RUST_LOG=info
        exec "$host" punktfunk1-host --source virtual --port "$port" --seconds 30 \
            --max-sessions 1 --allow-tofu --no-mdns
    ) >"$WORK/host.log" 2>&1 &
    hostpid=$!
    i=0
    while [ $i -lt 50 ] && ! grep -qE "listening|ready|bound" "$WORK/host.log" 2>/dev/null; do
        i=$((i + 1))
        sleep_s 0.1
    done
    "$WORK/bars" 18 >"$WORK/bars.log" 2>&1 &
    (
        cd "$WORK/probe/tmp" || exit 2
        export XDG_CONFIG_HOME="$WORK/probe/config" XDG_DATA_HOME="$WORK/probe/data"
        export XDG_STATE_HOME="$WORK/probe/state" XDG_CACHE_HOME="$WORK/probe/cache"
        export TMPDIR="$WORK/probe/tmp" RUST_LOG=info PUNKTFUNK_CLIENT_HDR=1
        exec "$probe" --connect "127.0.0.1:$port" --mode 1920x1080x60 --codec hevc --quit \
            --seconds 12 --out "$WORK/stream.h265"
    ) >"$WORK/probe.log" 2>&1
    kill "$hostpid" 2>/dev/null
    wait "$hostpid" 2>/dev/null
    plain "$WORK/host.log" >"$WORK/host.txt"
    cat "$WORK/bars.log"
    # `lift_us` on the closed line is what moving white took per frame.
    grep -E "virtual display created|screencapturekit capture closed" "$WORK/host.txt" |
        sed -E 's/^.*(virtual display created|screencapturekit capture closed)/  host: \1/' |
        cut -c1-160
}

if [ "${1:-}" = --read ]; then
    need ffmpeg ffprobe python3
    STREAM=$(abs "${2:?$usage}")
else
    need swiftc ffmpeg ffprobe python3
    HOST=$(abs "${1:?$usage}")
    PROBE=$(abs "${2:?$usage}")
    [ -x "$HOST" ] && [ -x "$PROBE" ] || { echo "both binaries must be executable"; exit 2; }
    session "$HOST" "$PROBE" "${3:-19878}"
    STREAM=$WORK/stream.h265
fi
[ -s "$STREAM" ] || { echo "FAIL the client received no stream; logs in $WORK"; exit 1; }
# A frame seven tenths in: the bars are up by then, and the stream has not ended.
frames=$(ffprobe -v error -count_frames -select_streams v:0 -show_entries stream=nb_read_frames \
    -of default=nw=1:nk=1 "$STREAM")
ffmpeg -v error -y -i "$STREAM" -vf "select=eq(n\,$((${frames:-0} * 7 / 10)))" -frames:v 1 \
    -f rawvideo -pix_fmt yuv420p10le "$WORK/frame.yuv" ||
    { echo "FAIL the stream did not decode"; exit 1; }
tags=$(ffprobe -v error -select_streams v:0 -show_entries \
    stream=pix_fmt,color_range,color_transfer,color_primaries -of default=nw=1:nk=1 "$STREAM" |
    tr '\n' ' ')
python3 -I - "$WORK/frame.yuv" "$tags" <<'PY'
import struct, sys
path, tags = sys.argv[1], sys.argv[2].split()
W, H = 1920, 1080
data = open(path, 'rb').read()
results = []
def check(name, ok):
    print(('  PASS ' if ok else '  FAIL ') + name)
    results.append(ok)
check(f'the stream is 10-bit BT.2020 PQ ({" ".join(tags)})',
      'yuv420p10le' in tags and 'smpte2084' in tags and 'bt2020' in tags)
if len(data) != W * H * 3:
    check(f'a {W}x{H} frame decoded', False)
    sys.exit(1)
def plane(off, w):
    return lambda x, y: struct.unpack_from('<H', data, off + 2 * (y * w + x))[0]
Y, U, V = plane(0, W), plane(2 * W * H, W // 2), plane(2 * W * H + W * H // 2, W // 2)
def mean(p, cx, cy, r=8):
    v = [p(x, y) for y in range(cy - r, cy + r) for x in range(cx - r, cx + r)]
    return sum(v) / len(v)
def nits(code):
    e = code / 1023 if 'pc' in tags else (code - 64) / 876
    m1, m2 = 2610 / 16384, 2523 / 4096 * 128
    c1, c2, c3 = 3424 / 4096, 2413 / 4096 * 32, 2392 / 4096 * 32
    p = max(e, 0.0) ** (1 / m2)
    return 10000 * (max(p - c1, 0) / (c2 - c3 * p)) ** (1 / m1)
names = ['black', 'grey 0.18', 'white', '2x white', '4x white', '8x white', 'sRGB red', 'P3 red']
bars = []
for i, name in enumerate(names):
    cx, cy = int((i + 0.5) * W / 8), int(H * 0.4)
    y, u, v = mean(Y, cx, cy), mean(U, cx // 2, cy // 2), mean(V, cx // 2, cy // 2)
    bars.append((nits(y), u, v))
    print(f'  {name:10s} {nits(y):7.1f} nits   Cb {u:5.1f}  Cr {v:5.1f}')
def near(got, want):
    return abs(got / want - 1) < 0.03
check('black is black', bars[0][0] < 1)
check('white sits at 203 nits', near(bars[2][0], 203))
check('18% grey sits at 18% of white', near(bars[1][0], 0.18 * 203))
check('2x white is 406 nits', near(bars[3][0], 406))
check('4x white is 812 nits', near(bars[4][0], 812))
check('8x white is brighter still', bars[5][0] > bars[4][0] * 1.5)
check('greys are neutral', all(abs(c - 512) <= 2 for b in bars[:6] for c in b[1:]))
check('red is red', bars[6][2] > 560 and bars[6][1] < 480)
check('Display P3 red is past sRGB red', bars[7][2] > bars[6][2] + 10)
print(f'passed {results.count(True)}, failed {results.count(False)}')
sys.exit(0 if all(results) else 1)
PY
status=$?
echo "logs in $WORK"
exit $status
