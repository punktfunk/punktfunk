#!/bin/sh
# Live check of a macOS host's virtual displays over loopback, extend topology only.
# Usage: macos-virtual-display-check.sh <punktfunk-host> <punktfunk-probe> [port]
#
# Needs a logged-in desktop session, and Screen Recording for the process that runs the host
# (the terminal it inherits from). Adds a display to the desktop for a few seconds per run;
# never makes it main, never mirrors, never touches a physical display. Sends no input.
set -u

HOST=${1:?usage: $0 <punktfunk-host> <punktfunk-probe> [port]}
PROBE=${2:?usage: $0 <punktfunk-host> <punktfunk-probe> [port]}
PORT=${3:-19877}
[ "$(uname -s)" = Darwin ] || { echo "macOS only"; exit 2; }
[ -x "$HOST" ] && [ -x "$PROBE" ] || { echo "both binaries must be executable"; exit 2; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/pf-vd-check.XXXXXX") || exit 2
FAILS=0
PASSES=0

pass() { PASSES=$((PASSES + 1)); echo "  PASS $1"; }
fail() { FAILS=$((FAILS + 1)); echo "  FAIL $1"; }
check() { if eval "$2"; then pass "$1"; else fail "$1"; fi; }
plain() { sed -E 's/\x1b\[[0-9;]*m//g' "$1"; }
sleep_s() { perl -e "select(undef,undef,undef,$1)"; }

# A Punktfunk client streaming to another machine: running now would change its desktop.
client_streaming() {
    for pid in $(pgrep -f "Punktfunk.app/Contents/MacOS/Punktfunk"); do
        if lsof -nP -i -a -p "$pid" 2>/dev/null | grep -- '->' |
            grep -vqE -- '->(127\.0\.0\.1|\[::1\])'; then
            return 0
        fi
    done
    return 1
}

# The mode of each online Punktfunk virtual display (`WxH@Hz`), as CoreGraphics lists it.
our_displays() {
    L="$WORK/list"
    mkdir -p "$L/config/punktfunk" "$L/data" "$L/state" "$L/cache"
    env PUNKTFUNK_CONFIG_DIR="$L/config/punktfunk" XDG_CONFIG_HOME="$L/config" \
        XDG_DATA_HOME="$L/data" XDG_STATE_HOME="$L/state" XDG_CACHE_HOME="$L/cache" \
        "$HOST" list-monitors 2>/dev/null | awk '/punktfunk virtual display/ { print $2 }'
}

if client_streaming; then
    echo "a Punktfunk client is streaming to another machine: not running"
    exit 3
fi
if [ -n "$(our_displays)" ]; then
    echo "a Punktfunk display is already on this desktop: not running"
    exit 3
fi

# One session: label, the mode asked (WxHxHz), then extra probe arguments.
session() {
    label=$1 mode=$2
    shift 2
    S="$WORK/$label"
    for side in host probe; do
        mkdir -p "$S/$side/config/punktfunk" "$S/$side/data" "$S/$side/state" \
            "$S/$side/cache" "$S/$side/tmp"
    done
    # Extend, twice over: the settings file, and the knob an unconfigured host reads.
    printf '{"version":1,"preset":"custom","topology":"extend"}\n' \
        >"$S/host/config/punktfunk/display-settings.json"
    (
        cd "$S/host/tmp" || exit 2
        export PUNKTFUNK_CONFIG_DIR="$S/host/config/punktfunk"
        export XDG_CONFIG_HOME="$S/host/config" XDG_DATA_HOME="$S/host/data"
        export XDG_STATE_HOME="$S/host/state" XDG_CACHE_HOME="$S/host/cache" TMPDIR="$S/host/tmp"
        export PUNKTFUNK_KWIN_VIRTUAL_PRIMARY=0
        export PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client
        export RUST_LOG=info
        exec "$HOST" punktfunk1-host --source virtual --port "$PORT" --seconds 14 \
            --max-sessions 1 --allow-tofu --no-mdns
    ) >"$S/host.log" 2>&1 &
    hostpid=$!
    i=0
    while [ $i -lt 50 ] && ! grep -qE "listening|ready|bound" "$S/host.log" 2>/dev/null; do
        i=$((i + 1))
        sleep_s 0.1
    done
    (
        cd "$S/probe/tmp" || exit 2
        export XDG_CONFIG_HOME="$S/probe/config" XDG_DATA_HOME="$S/probe/data"
        export XDG_STATE_HOME="$S/probe/state" XDG_CACHE_HOME="$S/probe/cache"
        export TMPDIR="$S/probe/tmp" RUST_LOG=info
        exec "$PROBE" --connect "127.0.0.1:$PORT" --mode "$mode" --codec hevc --quit "$@"
    ) >"$S/probe.log" 2>&1 &
    probepid=$!
    sleep_s 2
    our_displays >"$S/during.txt"
    wait "$probepid"
    i=0
    while kill -0 "$hostpid" 2>/dev/null && [ $i -lt 100 ]; do
        i=$((i + 1))
        sleep_s 0.1
    done
    kill "$hostpid" 2>/dev/null && wait "$hostpid" 2>/dev/null
    sleep_s 1
    our_displays >"$S/after.txt"
    plain "$S/host.log" >"$S/host.txt"
    plain "$S/probe.log" >"$S/probe.txt"
}

# The checks every session shares: made at `size@hz` under extend, captured and encoded at
# `size` with no scaling, frames arrived, and the display is gone.
common() {
    size=$1 hz=$2 w=${1%x*} h=${1#*x}
    check "virtual display created at ${size}@${hz} under extend" \
        "grep 'virtual display created' '$S/host.txt' | grep -q 'mode=${size}@${hz}.*topology=Extend'"
    check "no fallback to a mirror" \
        "! grep -qE 'virtual display not created|offers no virtual displays' '$S/host.txt'"
    check "macOS lists the display at ${size} during the session" \
        "grep -q '^${size}@' '$S/during.txt'"
    check "capture delivers ${size} with no letterbox" \
        "grep 'screencapturekit first frame' '$S/host.txt' | grep -q 'width=${w} height=${h}' &&
         ! grep -q letterboxed '$S/host.txt'"
    check "encoder opens at ${size}" \
        "grep 'VideoToolbox encoder open' '$S/host.txt' | grep -q 'width=${w} height=${h}'"
    check "frames arrive at the client" \
        "grep 'stream complete' '$S/probe.txt' | grep -qE 'frames=[1-9]'"
    check "the host removed the display" "grep -q 'virtual display removed' '$S/host.txt'"
    check "no Punktfunk display is left" "[ ! -s '$S/after.txt' ]"
}

echo "1920x1080 at 60 Hz"
session hd 1920x1080x60 --seconds 6
common 1920x1080 60

echo "2560x1440 at 120 Hz"
session qhd 2560x1440x120 --seconds 6
common 2560x1440 120

echo "1920x1080 at 60 Hz, then 2560x1440 four seconds in"
session remode 1920x1080x60 --seconds 9 --remode 2560x1440x60:4
common 1920x1080 60
check "the display took 2560x1440 in place" \
    "grep -q 'virtual display moved to a new mode in place' '$S/host.txt'"
check "capture and encoder follow to 2560x1440" \
    "grep 'screencapturekit first frame' '$S/host.txt' | grep -q 'width=2560 height=1440' &&
     grep 'VideoToolbox encoder open' '$S/host.txt' | grep -q 'width=2560 height=1440'"

if [ -n "$(our_displays)" ]; then
    echo "WARNING: a Punktfunk display is still listed; quit the host to remove it"
fi
echo
echo "passed $PASSES, failed $FAILS; logs in $WORK"
[ "$FAILS" -eq 0 ]
