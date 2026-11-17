#!/usr/bin/env bash
# Prove the capture->NVENC chain end to end WITHOUT writing any Rust, using wf-recorder
# (wlr-screencopy) piped into hevc_nvenc. Run from a shell where headless Sway is up and
# WAYLAND_DISPLAY + XDG_RUNTIME_DIR are exported (`source /tmp/punktfunk-sway-env.sh`, which
# prepare-session.sh writes).
#
# Usage: scripts/headless/capture-smoke-test.sh [output.mkv] [WxH-unused]
#   Ctrl-C to stop; then play/inspect the file (e.g. ffprobe out.mkv).
set -euo pipefail

OUT="${1:-/tmp/punktfunk-headless-test.mkv}"
: "${WAYLAND_DISPLAY:?set WAYLAND_DISPLAY (e.g. wayland-1) — is headless Sway running?}"
: "${XDG_RUNTIME_DIR:?set XDG_RUNTIME_DIR=/run/user/\$(id -u)}"

command -v wf-recorder >/dev/null || { echo "wf-recorder missing — run scripts/bootstrap-ubuntu.sh"; exit 1; }

echo "recording HEADLESS-1 -> $OUT  (wlr-screencopy -> hevc_nvenc, low-latency). Ctrl-C to stop."
# Low-latency NVENC params verified for FFmpeg 6.x/7.x: preset p1, tune ull, no B-frames.
exec wf-recorder -o HEADLESS-1 -c hevc_nvenc \
    -p preset=p1 -p tune=ull -p rc=cbr -p bf=0 -p delay=0 \
    -f "$OUT"
