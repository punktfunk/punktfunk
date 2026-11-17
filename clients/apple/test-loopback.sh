#!/usr/bin/env bash
# Loopback integration: real punktfunk/1 hosts (synthetic source — pure protocol, runs fine on
# macOS) on 127.0.0.1, then the Swift integration tests against them through the xcframework.
# Three hosts: an OPEN one (--allow-tofu; the anonymous stream round trip — bare punktfunk1-host
# defaults to require-pairing), one armed with --require-pairing (the PIN ceremony + pairing
# gate — its random PIN is parsed out of its log), and a GUESS host whose one-shot arming window
# the wrong-PIN test deliberately burns (a pairing attempt — right or wrong — consumes the armed
# PIN, the SPAKE2 "one online guess", so the real ceremony needs a window of its own).
set -euo pipefail
cd "$(dirname "$0")/../.."

PORT="${PUNKTFUNK_LOOPBACK_PORT:-19778}"
PAIR_PORT="${PUNKTFUNK_PAIRING_PORT:-19779}"
GUESS_PORT="${PUNKTFUNK_GUESS_PORT:-19780}"

cargo build --release -p punktfunk-host

# Each host gets a throwaway config home: the pairing hosts persist a trust store
# (punktfunk1-paired.json, resolved from $HOME) and all mint an identity cert on first
# run — none of that belongs in the user's real ~/.config/punktfunk, and separate homes
# also keep the first runs from racing on the same cert.pem.
CFG="$(mktemp -d "${TMPDIR:-/tmp}/punktfunk-loopback.XXXXXX")"
PAIR_LOG="$CFG/pairing-host.log"
GUESS_LOG="$CFG/guess-host.log"
mkdir -p "$CFG/open" "$CFG/paired" "$CFG/guess"
trap 'kill "${HOST_PID:-}" "${PAIR_PID:-}" "${GUESS_PID:-}" 2>/dev/null || true' EXIT
# The open host also scripts a feedback burst (rumble + DualSense hidout) right after the
# handshake, so the Swift test can assert the host→client feedback planes end to end.
# The open host outlives the others on purpose: AudioDeviceSwitchTests connects to it and then
# spends tens of seconds moving the system's output device around, long after the 300 frames the
# round-trip test needs.
HOME="$CFG/open" XDG_CONFIG_HOME="$CFG/open/.config" PUNKTFUNK_TEST_FEEDBACK=1 \
    target/release/punktfunk-host punktfunk1-host --port "$PORT" --source synthetic --frames 12000 \
    --allow-tofu &
HOST_PID=$!
HOME="$CFG/paired" XDG_CONFIG_HOME="$CFG/paired/.config" \
    target/release/punktfunk-host punktfunk1-host --port "$PAIR_PORT" --source synthetic --frames 300 \
    --require-pairing >"$PAIR_LOG" 2>&1 &
PAIR_PID=$!
HOME="$CFG/guess" XDG_CONFIG_HOME="$CFG/guess/.config" \
    target/release/punktfunk-host punktfunk1-host --port "$GUESS_PORT" --source synthetic --frames 300 \
    --require-pairing >"$GUESS_LOG" 2>&1 &
GUESS_PID=$!
sleep 1

# Parse each pairing host's random arming PIN out of its startup log.
pin_from_log() {
    local log="$1" pin=""
    for _ in $(seq 50); do
        # The host prints "pairing PIN: 1234"; the pattern also matches a bare "pair: 1234".
        pin="$(grep -oE '(pairing PIN|pair): *[0-9]+' "$log" | head -1 | grep -oE '[0-9]+' || true)"
        [ -n "$pin" ] && break
        sleep 0.2
    done
    if [ -z "$pin" ]; then
        echo "no arming PIN in the pairing host's log ($log)" >&2
        exit 1
    fi
    echo "$pin"
}
PIN="$(pin_from_log "$PAIR_LOG")"
GUESS_PIN="$(pin_from_log "$GUESS_LOG")"

cd clients/apple
PUNKTFUNK_LOOPBACK_PORT="$PORT" PUNKTFUNK_PAIRING_PORT="$PAIR_PORT" PUNKTFUNK_PAIRING_PIN="$PIN" \
    PUNKTFUNK_GUESS_PORT="$GUESS_PORT" PUNKTFUNK_GUESS_PIN="$GUESS_PIN" \
    PUNKTFUNK_TEST_FEEDBACK=1 \
    swift test --filter 'LoopbackIntegrationTests|AudioDeviceSwitchTests|MicUplinkTests'
