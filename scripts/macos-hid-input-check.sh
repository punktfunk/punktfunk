#!/bin/sh
# Live check of a macOS host's virtual HID mouse and keyboard (PUNKTFUNK_HID_INPUT=1).
# Usage: macos-hid-input-check.sh <Punktfunk Host.app> <mac_gc_reader>
#
# The app is packaging/macos/bundle.sh around the `mac_input_live` example instead of the host,
# so its signature carries the HID virtual device entitlement (profile and identity as in
# packaging/macos/README.md):
#   cargo build -p pf-inject --release --example mac_input_live
#   sh packaging/macos/bundle.sh -b target/release/examples/mac_input_live \
#     -p <profile> -i "Apple Development" -t <team id> -v 0.0.1 -o dist-hid
#   swiftc -O -o mac_gc_reader crates/host/pf-inject/examples/mac_gc_reader.swift
#   sh scripts/macos-hid-input-check.sh "dist-hid/Punktfunk Host.app" mac_gc_reader
# Run it on a test Mac, from a terminal with the Accessibility grant, with nobody at the mouse
# or keyboard: it starts after 4 s without input, then moves the pointer, clicks and types an
# "a" into the reader's window.
set -u

usage="usage: $0 <Punktfunk Host.app> <mac_gc_reader>"
abs() { case $1 in /*) echo "$1" ;; *) echo "$PWD/$1" ;; esac; }
APP=$(abs "${1:?$usage}")
READER=$(abs "${2:?$usage}")
BIN="$APP/Contents/MacOS/punktfunk-host"
[ "$(uname -s)" = Darwin ] || { echo "macOS only"; exit 2; }
[ -x "$BIN" ] && [ -x "$READER" ] || { echo "both binaries must be executable"; exit 2; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/pf-hid-check.XXXXXX") || exit 2
LOG="$WORK/run.log"
FAILS=0
PASSES=0
pass() { PASSES=$((PASSES + 1)); echo "  PASS $1"; }
fail() { FAILS=$((FAILS + 1)); echo "  FAIL $1"; }
check() { if eval "$2"; then pass "$1"; else fail "$1"; fi; }
count() { grep -c -- "$1" "$LOG"; }

# While the run lasts: whether Keyboard Setup Assistant opens, and the device's registry entry.
(
    while :; do
        pgrep -x KeyboardSetupAssistant > /dev/null && touch "$WORK/assistant"
        ioreg -r -c IOHIDUserDevice -l 2> /dev/null | grep -q '"Product" = "Punktfunk Virtual Mouse"' &&
            ioreg -r -c IOHIDUserDevice -l > "$WORK/ioreg" 2> /dev/null
        sleep 0.25
    done
) &
SAMPLER=$!
trap 'kill $SAMPLER 2> /dev/null' EXIT

env PUNKTFUNK_HID_INPUT=1 "$BIN" "$READER" hid > "$LOG" 2>&1
sleep 3
kill $SAMPLER 2> /dev/null

echo "run log: $LOG"
grep -E 'probe:|abort:|GC (mouse|keyboard) connected' "$LOG" | sed 's/^/  /'
if [ -f "$WORK/ioreg" ]; then
    echo "  device entry:"
    grep -E '"Product" = "Punktfunk|"Transport"|HIDVirtualDevice|HIDRMDeviceState' "$WORK/ioreg" |
        sort -u | sed 's/^ */    /'
fi

check "the entitlement lets the device be created" "grep -q 'probe: Ready' '$LOG'"
check "the run finished" "! grep -q 'abort:' '$LOG'"
check "GameController lists the virtual mouse" \
    "grep -q 'GC mouse connected vendor=Punktfunk Virtual Mouse' '$LOG'"
check "GameController mouse motion" \
    "[ \"\$(count 'GCMOUSE vendor=Punktfunk Virtual Mouse moved')\" -ge 10 ]"
check "GameController left button down and up" \
    "grep -q 'vendor=Punktfunk Virtual Mouse left pressed=true' '$LOG' &&
     grep -q 'vendor=Punktfunk Virtual Mouse left pressed=false' '$LOG'"
check "GameController A key down and up" \
    "grep -q 'GCKEY code=4 pressed=true' '$LOG' && grep -q 'GCKEY code=4 pressed=false' '$LOG'"
check "AppKit moves from the device" "[ \"\$(count 'from=other MOUSE t=5')\" -ge 10 ]"
check "AppKit click from the device" \
    "grep -q 'from=other MOUSE t=1 ' '$LOG' && grep -q 'from=other MOUSE t=2 ' '$LOG'"
check "AppKit A key from the device" "grep -q 'from=other KEY down code=0x00' '$LOG'"
check "no Keyboard Setup Assistant" "[ ! -f '$WORK/assistant' ]"

echo "$PASSES passed, $FAILS failed"
[ "$FAILS" -eq 0 ]
