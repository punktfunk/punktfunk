#!/usr/bin/env bash
#
# Type-check and lint punktfunk's Windows-gated capture and display Rust from a dev box that is not
# Windows.
#
#   scripts/xcheck.sh                    # clippy -D warnings — the default
#   scripts/xcheck.sh check              # cargo check instead of clippy
#   scripts/xcheck.sh clippy --fix       # extra args are passed through to cargo
#
# (`scripts/wincheck.sh` is a compat shim for this script's former name; a leading `windows`
# argument is still accepted.)
#
# CI compiles `#[cfg(target_os = "windows")]` code only on Windows, so a Windows-side edit made on a
# Mac is otherwise unverifiable until that job runs. None of pf-frame, pf-win-display, pf-capture
# and pf-vdisplay takes punktfunk-core with its `quic` feature (pf-win-display and pf-vdisplay take
# it with default features off), so their Windows closure has no ring or opus build script that
# needs the MSVC toolchain, and cargo checks them in the real workspace.
#
# There is no Linux leg: pf-capture's Linux half depends on the pipewire crate, whose build
# script needs a Linux libpipewire. Check it on Linux.
#
# Note: `cargo fmt` needs none of this — rustfmt follows `mod`/`#[path]` without evaluating cfg, so
# it already reaches Windows-only files.
set -euo pipefail

REPO="$(cd "$(dirname "$(readlink -f "$0")")/.." && pwd)"
TARGET=x86_64-pc-windows-msvc
LINT=(-p pf-frame -p pf-win-display -p pf-capture -p pf-vdisplay)

case "${1:-}" in
  windows) shift ;;
  linux)
    echo "xcheck: pf-vdisplay's Linux half needs libpipewire; run cargo clippy -p pf-vdisplay on Linux" >&2
    exit 2
    ;;
esac

cmd="${1:-clippy}"
[ $# -gt 0 ] && shift
case "$cmd" in
  check | clippy) ;;
  *)
    echo "usage: $(basename "$0") [check|clippy] [extra cargo args...]" >&2
    exit 2
    ;;
esac

if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "xcheck: target $TARGET not installed for the active toolchain — run:" >&2
  echo "    rustup target add $TARGET" >&2
  exit 1
fi

cd "$REPO"
echo "=== $cmd ${LINT[*]} ($TARGET) ==="
if [ "$cmd" = clippy ]; then
  cargo clippy --target "$TARGET" "${LINT[@]}" --all-targets "$@" -- -D warnings
else
  cargo check --target "$TARGET" "${LINT[@]}" --all-targets "$@"
fi
