#!/usr/bin/env bash
# Wrap a built punktfunk pacman package into a systemd-sysext image — the update-survivable way to
# add it to an immutable Arch-derived distro (SteamOS 3): the .raw overlays /usr read-only from the
# writable /var/lib/extensions/, so it persists across A/B OS updates with no `steamos-readonly
# disable`. Works for either split package — on a Steam Deck you'd wrap the CLIENT. Needs
# `bsdtar`/`tar`, `squashfs-tools` (mksquashfs).
#
# Usage:  bash build-sysext.sh [--gamescope <punktfunk-gamescope-*.pkg.tar.zst>] \
#                              <punktfunk-{host,client}-*.pkg.tar.zst>
# Output: <pkgname>.raw   (e.g. punktfunk-client.raw)
#
# --gamescope folds the HDR-capable gamescope companion package (packaging/gamescope) into a HOST
# image as /usr/bin/punktfunk-gamescope — what lets the gamescope backend stream 10-bit BT.2020 PQ
# instead of 8-bit SDR (the host prefers that name on PATH and attempts HDR by default). Mirrors
# the Bazzite image's fold-in, including the honesty check: the binary is verified by executing
# its `+pfhdr` banner, never trusted by filename. Omit it and the image is exactly what it was —
# the host then stays SDR on that backend, by design.
#
# Capabilities in the image: NEVER on usr/bin/punktfunk-host, `cap_sys_nice=ep` on
# usr/bin/punktfunk-encode-worker (best-effort), and none on punktfunk-gamescope.
#
# ⚠ Capabilities are NOT lost on the way in — that was this comment's earlier claim and it is
# false: mksquashfs records security.capability, and the published Bazzite 0.26.0-1 image really
# did carry `cap_sys_nice=ep` on usr/bin/punktfunk-host. The host is left uncapped on purpose. A
# capability on the HOST binary makes it unidentifiable to KWin (which resolves a client's
# /proc/<pid>/exe to match it against a .desktop, and cannot read it for a capability-carrying
# process) and kills every Desktop-mode session.
#
# ⚠ And it is NOT enough to leave it out here: pacman scriptlets never run for a sysext, so the
# `setcap` in punktfunk-host.install cannot reach this image either way. The encode worker is
# therefore capped on the staging tree below — this is the only place a sysext can acquire it — and
# both halves of the matrix are asserted before mksquashfs by packaging/linux/sysext-lib.sh, which
# the Bazzite image shares. `punktfunk-gamescope` is a compositor, not a KWin client,
# so it is unaffected by the host rule and simply runs without a capability here, pacing slightly
# worse.
set -euo pipefail
. "$(cd "$(dirname "$0")" && pwd)/../linux/sysext-lib.sh"

GAMESCOPE=""
if [ "${1:-}" = "--gamescope" ]; then
  GAMESCOPE="${2:?--gamescope needs a punktfunk-gamescope package}"; shift 2
fi
# No braces in the message: a literal `}` inside ${1:?...} terminates the expansion early and
# corrupts $PKG (the tail of the message gets appended to the value — a real field bug).
PKG="${1:?usage: build-sysext.sh [--gamescope <pkg>] <punktfunk-host|client pkg.tar.zst>}"
[ -f "$PKG" ] || { echo "no such package: $PKG" >&2; exit 1; }
# Derive the package name from the file (pkgname is everything before the -<version>).
NAME="$(basename "$PKG" | sed -E 's/-[0-9].*//')"
[ -n "$NAME" ] || { echo "could not derive package name from $PKG" >&2; exit 1; }
if [ -n "$GAMESCOPE" ] && [ "$NAME" != "punktfunk-host" ]; then
  echo "--gamescope only makes sense for a punktfunk-host image (got: $NAME)" >&2; exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# A pacman package is a (zstd) tarball; a sysext only carries /usr (the host /etc, /var are the
# system's). Extract just usr/ from the payload.
if command -v bsdtar >/dev/null 2>&1; then
  bsdtar -C "$STAGE" -xf "$PKG" usr
else
  tar -C "$STAGE" -xf "$PKG" usr
fi

# The HDR gamescope companion (see --gamescope in the header), verified by its banner and its WSI
# layer rather than trusted by filename. Executing the staged binary needs a build box the binary
# runs on (the Arch CI container qualifies; it built it).
if [ -n "$GAMESCOPE" ]; then
  [ -f "$GAMESCOPE" ] || { echo "no such package: $GAMESCOPE" >&2; exit 1; }
  if command -v bsdtar >/dev/null 2>&1; then
    bsdtar -C "$STAGE" -xf "$GAMESCOPE" usr
  else
    tar -C "$STAGE" -xf "$GAMESCOPE" usr
  fi
  pf_verify_gamescope "$STAGE" "$GAMESCOPE"
  echo "folded in $("$STAGE/usr/bin/punktfunk-gamescope" --version 2>&1 | head -1) + its WSI layer"
fi

pf_stage_sysctl_unit "$STAGE"

# The marker systemd-sysext requires to merge the image. ID=_any merges onto ANY host os-release
# (SteamOS, Arch, Bazzite); ARCHITECTURE pins it to x86-64 so it's never merged on the wrong arch.
# EXTENSION_RELOAD_MANAGER makes systemd load the image's sysctl unit after the merge.
install -d "$STAGE/usr/lib/extension-release.d"
cat > "$STAGE/usr/lib/extension-release.d/extension-release.$NAME" <<EOF
ID=_any
ARCHITECTURE=x86-64
EXTENSION_RELOAD_MANAGER=1
EOF

# CAP_SYS_NICE on the encode worker, never the host, then both halves of the matrix (see the
# header). Needs CAP_SETFCAP, i.e. root or fakeroot; a plain-user build ships the worker uncapped,
# which is a pacing loss and nothing more.
pf_seal_caps "$STAGE" "A pacman payload carries no capabilities, so something else granted it."

OUT="$NAME.raw"
rm -f "$OUT"
mksquashfs "$STAGE" "$OUT" -all-root -noappend -quiet
echo "built $OUT"
echo "  install:  sudo cp $OUT /var/lib/extensions/ && sudo systemctl enable --now systemd-sysext"
if [ "$NAME" = "punktfunk-host" ]; then
  echo "  then:     systemctl --user enable --now punktfunk-host"
else
  echo "  then:     run 'punktfunk-client' (or let the Decky plugin launch it)"
fi
