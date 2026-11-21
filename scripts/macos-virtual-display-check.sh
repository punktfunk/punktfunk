#!/bin/sh
# Live check of a macOS host's virtual displays over loopback.
# Usage: macos-virtual-display-check.sh <punktfunk-host> <punktfunk-probe> [port]
#
# Needs a logged-in desktop session, and Screen Recording for the process that runs the host
# (the terminal it inherits from). Run it on a test machine: the default-topology cases make
# every other display mirror the virtual one while they run. Sends no input.
# A bring-up slower than BRINGUP_LIMIT_MS (default 2500) fails as a stall, and a mode change
# slower than CHANGE_LIMIT_MS (default 1000) fails as one.
set -u

usage="usage: $0 <punktfunk-host> <punktfunk-probe> [port]"
abs() { case $1 in /*) echo "$1" ;; *) echo "$PWD/$1" ;; esac; }
HOST=$(abs "${1:?$usage}")
PROBE=$(abs "${2:?$usage}")
PORT=${3:-19877}
LIMIT=${BRINGUP_LIMIT_MS:-2500}
CHANGE=${CHANGE_LIMIT_MS:-1000}
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

# Wait until no Punktfunk display is listed, for at most 15 s.
wait_gone() {
    i=0
    while [ -n "$(our_displays)" ] && [ $i -lt 30 ]; do
        i=$((i + 1))
        sleep_s 0.5
    done
}

if client_streaming; then
    echo "a Punktfunk client is streaming to another machine: not running"
    exit 3
fi
if [ -n "$(our_displays)" ]; then
    echo "a Punktfunk display is already on this desktop: not running"
    exit 3
fi

# Start a host for `sessions` sessions of up to `secs` seconds each, logging under $S. `topo` is
# `extend`, or `default` for an unconfigured host (exclusive).
start_host() {
    S="$WORK/$1"
    TOPO=$4
    for side in host probe; do
        mkdir -p "$S/$side/config/punktfunk" "$S/$side/data" "$S/$side/state" \
            "$S/$side/cache" "$S/$side/tmp"
    done
    # Extend, twice over: the settings file, and the knob an unconfigured host reads.
    if [ "$TOPO" = extend ]; then
        printf '{"version":1,"preset":"custom","topology":"extend"}\n' \
            >"$S/host/config/punktfunk/display-settings.json"
    fi
    (
        cd "$S/host/tmp" || exit 2
        export PUNKTFUNK_CONFIG_DIR="$S/host/config/punktfunk"
        export XDG_CONFIG_HOME="$S/host/config" XDG_DATA_HOME="$S/host/data"
        export XDG_STATE_HOME="$S/host/state" XDG_CACHE_HOME="$S/host/cache" TMPDIR="$S/host/tmp"
        [ "$TOPO" = extend ] && export PUNKTFUNK_KWIN_VIRTUAL_PRIMARY=0
        export PUNKTFUNK_AUDIO_OUTPUT_MODE=host_and_client
        export RUST_LOG=info
        exec "$HOST" punktfunk1-host --source virtual --port "$PORT" --seconds "$3" \
            --max-sessions "$2" --allow-tofu --no-mdns
    ) >"$S/host.log" 2>&1 &
    hostpid=$!
    i=0
    while [ $i -lt 50 ] && ! grep -qE "listening|ready|bound" "$S/host.log" 2>/dev/null; do
        i=$((i + 1))
        sleep_s 0.1
    done
}

# Session `n` of the running host: the probe asks for `mode`, then the arguments after it.
# Records the displays macOS lists once the host has made the session's display.
session() {
    n=$1 mode=$2
    shift 2
    (
        cd "$S/probe/tmp" || exit 2
        export XDG_CONFIG_HOME="$S/probe/config" XDG_DATA_HOME="$S/probe/data"
        export XDG_STATE_HOME="$S/probe/state" XDG_CACHE_HOME="$S/probe/cache"
        export TMPDIR="$S/probe/tmp" RUST_LOG=info
        exec "$PROBE" --connect "127.0.0.1:$PORT" --mode "$mode" --codec hevc --quit "$@"
    ) >"$S/probe-$n.log" 2>&1 &
    probepid=$!
    i=0
    while [ "$(grep -c 'virtual display created' "$S/host.log")" -lt "$n" ] && [ $i -lt 100 ]; do
        i=$((i + 1))
        sleep_s 0.1
    done
    our_displays >"$S/during-$n.txt"
    wait "$probepid"
    plain "$S/probe-$n.log" >"$S/probe-$n.txt"
}

# Let the host finish, then wait for its displays to go before the next case.
finish_host() {
    i=0
    while kill -0 "$hostpid" 2>/dev/null && [ $i -lt 150 ]; do
        i=$((i + 1))
        sleep_s 0.1
    done
    kill "$hostpid" 2>/dev/null && wait "$hostpid" 2>/dev/null
    plain "$S/host.log" >"$S/host.txt"
    wait_gone
    our_displays >"$S/after.txt"
}

# The n-th trace of `kind` (`bringup`, `resize`): its total in milliseconds, or nothing.
trace_ms() {
    grep "session-transition trace kind=\"$2\"" "$S/host.txt" | sed -n "$1p" |
        sed -nE 's/.*total_ms=([0-9]+).*/\1/p'
}

# Session `n` was made at `size@hz` under the case's topology, listed, captured and encoded at
# `size` with no scaling, brought up within the limit, and streamed frames.
checks() {
    n=$1 size=$2 hz=$3 w=${2%x*} h=${2#*x}
    topo=Extend
    [ "$TOPO" = default ] && topo=Exclusive
    ms=$(trace_ms "$n" bringup)
    check "display $n created at ${size}@${hz} under $topo" \
        "grep 'virtual display created' '$S/host.txt' | sed -n '${n}p' |
         grep -q 'mode=${size}@${hz}.*topology=$topo'"
    check "macOS lists display $n at ${size} during the session" \
        "grep -q '^${size}@' '$S/during-$n.txt'"
    check "session $n brought up in ${ms:-?} ms (limit $LIMIT)" \
        "[ -n '$ms' ] && [ '${ms:-0}' -le $LIMIT ]"
    check "capture delivers ${size}" \
        "grep 'screencapturekit first frame' '$S/host.txt' | grep -q 'width=${w} height=${h}'"
    check "encoder opens at ${size}" \
        "grep 'VideoToolbox encoder open' '$S/host.txt' | grep -q 'width=${w} height=${h}'"
    check "frames arrive at client $n" \
        "grep 'stream complete' '$S/probe-$n.txt' | grep -qE 'frames=[1-9]'"
}

# Resizes `first` to `last` changed the display in place, each within the limit, and capture
# and encoder followed to `size`.
changed() {
    first=$1 last=$2 size=$3 w=${3%x*} h=${3#*x}
    count=$((last - first + 1))
    check "$count mode changes in place, none made again" \
        "[ \"\$(grep -c 'changed mode in place' '$S/host.txt')\" -eq $count ] &&
         ! grep -q 'making the virtual display again' '$S/host.txt'"
    r=$first
    while [ "$r" -le "$last" ]; do
        ms=$(trace_ms "$r" resize)
        check "mode change $r rebuilt in ${ms:-?} ms (limit $CHANGE)" \
            "[ -n '$ms' ] && [ '${ms:-0}' -le $CHANGE ]"
        r=$((r + 1))
    done
    check "capture and encoder follow to ${size}" \
        "grep 'screencapturekit first frame' '$S/host.txt' | grep -q 'width=${w} height=${h}' &&
         grep 'VideoToolbox encoder open' '$S/host.txt' | grep -q 'width=${w} height=${h}'"
}

# What every case ends on: no fallback, no letterbox, each display removed, none left.
ends() {
    count=$1
    check "no fallback to a mirror" \
        "! grep -qE 'virtual display not created|offers no virtual displays' '$S/host.txt'"
    check "no letterbox" "! grep -q letterboxed '$S/host.txt'"
    check "the host removed $count display(s)" \
        "[ \"\$(grep -c 'virtual display removed' '$S/host.txt')\" -eq $count ]"
    check "no Punktfunk display is left" "[ ! -s '$S/after.txt' ]"
}

# Five window resizes inside the first mode; the last is a common panel's own mode.
RESIZES="--remode 1476x972x60:3 --remode 1558x982x60:5 --remode 1114x984x60:7
    --remode 1600x900x60:9 --remode 1920x1080x60:11"

echo "1920x1080 at 60 Hz"
start_host hd 1 20 extend
session 1 1920x1080x60 --seconds 10
finish_host
checks 1 1920x1080 60
ends 1

echo "2560x1440 at 120 Hz"
start_host qhd 1 20 extend
session 1 2560x1440x120 --seconds 10
finish_host
checks 1 2560x1440 120
ends 1

for topo in default extend; do
    echo "1920x1080 at 60 Hz, then five resizes two seconds apart, $topo topology"
    start_host "changes-$topo" 1 25 "$topo"
    # shellcheck disable=SC2086 # RESIZES is a list of arguments
    session 1 1920x1080x60 --seconds 14 $RESIZES
    finish_host
    checks 1 1920x1080 60
    changed 1 5 1114x984
    ends 1
done

echo "1920x1080 at 60 Hz, then the larger 2560x1440 five seconds in"
start_host remode 1 25 extend
session 1 1920x1080x60 --seconds 12 --remode 2560x1440x60:5
finish_host
checks 1 1920x1080 60
ms=$(trace_ms 1 resize)
check "the display was made again at 2560x1440@60" \
    "grep 'virtual display created' '$S/host.txt' | sed -n '2p' | grep -q 'mode=2560x1440@60'"
check "the mode switch rebuilt in ${ms:-?} ms (limit $LIMIT)" "[ -n '$ms' ] && [ '${ms:-0}' -le $LIMIT ]"
check "capture and encoder follow to 2560x1440" \
    "grep 'screencapturekit first frame' '$S/host.txt' | grep -q 'width=2560 height=1440' &&
     grep 'VideoToolbox encoder open' '$S/host.txt' | grep -q 'width=2560 height=1440'"
ends 2

echo "two sessions in one host, default topology, each with a resize"
start_host reconnect 2 20 default
session 1 1920x1080x60 --seconds 8 --remode 1600x900x60:4
sleep_s 3
session 2 1920x1080x60 --seconds 8 --remode 1280x720x60:4
finish_host
checks 1 1920x1080 60
checks 2 1920x1080 60
changed 1 2 1280x720
ends 2

if [ -n "$(our_displays)" ]; then
    echo "WARNING: a Punktfunk display is still listed; quit the host to remove it"
fi
echo
echo "passed $PASSES, failed $FAILS; logs in $WORK"
[ "$FAILS" -eq 0 ]
