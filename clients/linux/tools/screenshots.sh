#!/usr/bin/env bash
# Capture host-free UI screenshots of the native Linux client. Mirrors the iOS harness
# (clients/apple/tools/screenshots.sh): one app launch per scene (PUNKTFUNK_SHOT_SCENE),
# the app renders a mock-populated REAL view and — when the binary supports it — CAPTURES
# ITSELF (PUNKTFUNK_SHOT_OUT: widget snapshot → gsk render → PNG) before printing
# `PF_SHOT_READY`. Self-capture needs no Xvfb/ImageMagick and runs under a live Wayland
# session too; the X11 root-grab path is kept as a fallback for old binaries. No host,
# GPU, or live stream — only the chrome scenes (the stream runs in the punktfunk-session
# binary).
#
#   cargo build --release -p punktfunk-client-linux
#   bash clients/linux/tools/screenshots.sh            # → clients/linux/screenshots/<scene>.png
#   bash clients/linux/tools/screenshots.sh hosts pair # a subset
#
# Env knobs: BIN (client binary), OUT (output dir), GEOMETRY (Xvfb WxHxDepth),
# SETTLE (extra seconds after PF_SHOT_READY, X11-fallback only), SHOT_DISPLAY (X display),
# GSK_RENDERER (gl|ngl|cairo), FORCE_XVFB=1 (ignore a live Wayland session and go through
# Xvfb anyway).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)" # clients/linux
BIN="${BIN:-$here/../../target/release/punktfunk-client}"
OUT="${OUT:-$here/screenshots}"
# X11 fallback only: the client window maps at its 1200x780 default; with no WM under
# Xvfb it lands at the top-left, so keep the root just larger so the full window (incl.
# its CSD shadow) is captured by a root grab with only a thin margin to crop.
GEOMETRY="${GEOMETRY:-1380x860x24}"
SETTLE="${SETTLE:-1.2}"
SHOT_DISPLAY="${SHOT_DISPLAY:-:99}"

if [ "$#" -gt 0 ]; then SCENES=("$@"); else SCENES=(hosts settings trust pair addhost shortcuts library); fi

[ -x "$BIN" ] || {
	echo "client binary not found: $BIN (build it first: cargo build --release -p punktfunk-client-linux)" >&2
	exit 1
}

# Isolated scratch HOME: the client generates its identity here on first run, and the
# saved-hosts grid is read from client-known-hosts.json, so seed mock hosts for the
# `hosts` scene (the dialogs/settings build their own mock state in-app). `last_used` on
# the first entry is what the `lastConnected` host sort orders by.
WORK="$(mktemp -d)"
export HOME="$WORK"
mkdir -p "$HOME/.config/punktfunk"
cat >"$HOME/.config/punktfunk/client-known-hosts.json" <<'JSON'
{
  "hosts": [
    { "name": "Living Room PC", "addr": "192.168.1.42", "port": 9777,
      "fp_hex": "9f8e7d6c5b4a39281706f5e4d3c2b1a0998877665544332211ffeeddccbbaa00",
      "paired": true, "last_used": 1780000000 },
    { "name": "Office", "addr": "192.168.1.50", "port": 9777,
      "fp_hex": "a1b2c3d4e5f60718293a4b5c6d7e8f90112233445566778899aabbccddeeff00",
      "paired": false }
  ]
}
JSON

XVFB_PID=""
cleanup() {
	if [ -n "$XVFB_PID" ]; then kill "$XVFB_PID" 2>/dev/null || true; fi
	rm -rf "$WORK"
}
trap cleanup EXIT

if [ -n "${WAYLAND_DISPLAY:-}" ] && [ -z "${FORCE_XVFB:-}" ]; then
	# Live Wayland session: self-capture only (there is no root grab on Wayland). The
	# window flashes up briefly per scene — this is a dev harness, not CI polish.
	MODE=wayland
	export GDK_BACKEND=wayland
else
	# Software-rendered X session — no GPU/Wayland needed. GL/llvmpipe runs the real NGL
	# renderer (cairo is documented-incomplete for 3D-transformed content / libadwaita
	# transitions).
	MODE=x11
	unset WAYLAND_DISPLAY
	export DISPLAY="$SHOT_DISPLAY"
	export GDK_BACKEND=x11
	export LIBGL_ALWAYS_SOFTWARE=1
	export GALLIUM_DRIVER="${GALLIUM_DRIVER:-llvmpipe}"
	export GSK_RENDERER="${GSK_RENDERER:-gl}"

	Xvfb "$SHOT_DISPLAY" -screen 0 "$GEOMETRY" -nolisten tcp >"$WORK/xvfb.log" 2>&1 &
	XVFB_PID=$!
	# Wait for the display to accept connections.
	for _ in $(seq 1 50); do
		if command -v xdpyinfo >/dev/null 2>&1; then
			xdpyinfo -display "$SHOT_DISPLAY" >/dev/null 2>&1 && break
		else
			[ -e "/tmp/.X11-unix/X${SHOT_DISPLAY#:}" ] && break
		fi
		sleep 0.1
	done
fi

# X11 root grab — the fallback for binaries without self-capture.
capture_x11() {
	local out="$1"
	if command -v import >/dev/null 2>&1; then
		import -silent -window root "$out"
	elif command -v scrot >/dev/null 2>&1; then
		scrot -o "$out"
	else
		echo "no screenshot tool — install imagemagick or scrot" >&2
		return 1
	fi
}

mkdir -p "$OUT"
rc=0
for scene in "${SCENES[@]}"; do
	: >"$WORK/log"
	rm -f "$OUT/$scene.png"
	PUNKTFUNK_SHOT_SCENE="$scene" PUNKTFUNK_SHOT_OUT="$OUT/$scene.png" \
		"$BIN" >"$WORK/log" 2>&1 &
	pid=$!
	ready=0
	# Up to ~60s: a warm scene signals in ~1-2s, but the FIRST launch pays a large one-time
	# cold-start under software rendering (llvmpipe GL-shader + fontconfig cache build, plus
	# first-run client-identity generation into the shared scratch HOME) that can run ~25s on a
	# loaded CI runner.
	for _ in $(seq 1 600); do
		if grep -q "PF_SHOT_READY" "$WORK/log"; then
			ready=1
			break
		fi
		if ! kill -0 "$pid" 2>/dev/null; then
			# Self-capture binaries exit(0) right after READY — check the log once more.
			grep -q "PF_SHOT_READY" "$WORK/log" && ready=1
			break
		fi
		sleep 0.1
	done
	if [ "$ready" = 1 ]; then
		if [ -f "$OUT/$scene.png" ]; then
			echo "✓ $scene → $OUT/$scene.png (self-capture)"
		elif [ "$MODE" = x11 ]; then
			# Old binary (no PUNKTFUNK_SHOT_OUT support) — grab the X root instead.
			sleep "$SETTLE"
			if capture_x11 "$OUT/$scene.png"; then
				echo "✓ $scene → $OUT/$scene.png (x11 grab)"
			else
				rc=1
			fi
		else
			echo "✗ $scene: no PNG (self-capture failed — see log)" >&2
			sed 's/^/    /' "$WORK/log" >&2 || true
			rc=1
		fi
	else
		echo "✗ $scene: client never signalled PF_SHOT_READY" >&2
		sed 's/^/    /' "$WORK/log" >&2 || true
		rc=1
	fi
	kill "$pid" 2>/dev/null || true
	wait "$pid" 2>/dev/null || true
done

exit "$rc"
