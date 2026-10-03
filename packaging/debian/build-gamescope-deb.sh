#!/usr/bin/env bash
# Package an already-built punktfunk-gamescope binary as a .deb, for the Gitea apt registry.
#
# Counterpart to ../gamescope/build-gamescope-rpm.sh, and the same argument: the binary is a
# ~10-minute meson build of an unrelated tree that CI does once and caches, so this repacks rather
# than rebuilds. The Arch package (../gamescope/PKGBUILD) is the one recipe that builds from source,
# because that is what makepkg is for.
#
# Installed as /usr/bin/punktfunk-gamescope — it does NOT replace the distro's gamescope, and does
# not Provide/Conflict with it. Only the sessions punktfunk-host starts itself resolve this binary
# (PUNKTFUNK_GAMESCOPE_BIN > punktfunk-gamescope > gamescope).
#
# `--stage` is the DESTDIR build-punktfunk-gamescope.sh wrote, not a single binary: that tree carries
# the compositor AND the Vulkan WSI layer built beside it, and a game nested under gamescope gets its
# HDR10 swapchain from that layer or from nowhere. Taking the whole tree is what stops the next file
# in the package needing a new flag in every packaging script.
#
# Usage:
#   VERSION=3.16.25.pfhdr4~ci42.gdeadbee bash packaging/debian/build-gamescope-deb.sh \
#     --stage gs-cache [--arch amd64]
# Output: dist/punktfunk-gamescope_<version>_<arch>.deb
set -euo pipefail

SRC_STAGE=""
DEB_ARCH=""
while [ $# -gt 0 ]; do
  case "$1" in
    --stage)  SRC_STAGE="${2:?--stage needs a path}"; shift 2 ;;
    --arch)   DEB_ARCH="${2:?--arch needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$SRC_STAGE" ] || { echo "ERROR: --stage is required" >&2; exit 2; }
# The layout build-punktfunk-gamescope.sh writes under its --destdir/--prefix.
BINARY="$SRC_STAGE/usr/bin/punktfunk-gamescope"
REAPER="$SRC_STAGE/usr/lib/punktfunk/gamescope/gamescopereaper"
LAYER_SO="$SRC_STAGE/usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
LAYER_JSON="$SRC_STAGE/usr/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
[ -x "$BINARY" ] || { echo "ERROR: $BINARY is not an executable file" >&2; exit 1; }
# gamescope starts every session's first child through its reaper.
[ -x "$REAPER" ] || { echo "ERROR: $REAPER missing from the stage — a session would run nothing" >&2; exit 1; }
# Hard, not best-effort: a package carrying the compositor without its layer looks perfectly healthy
# and then silently denies every game an HDR10 swapchain.
for f in "$LAYER_SO" "$LAYER_JSON"; do
  [ -f "$f" ] || { echo "ERROR: $f missing from the stage — no game HDR without it" >&2; exit 1; }
done

PKG="punktfunk-gamescope"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

DEB_ARCH="${DEB_ARCH:-$(dpkg --print-architecture)}"

# The marker is the host's whole capability probe: a binary that lost the patches installs fine and
# then silently streams SDR, cursorless, at a 60 Hz-advertising session. Refuse to package it.
BANNER="$("$BINARY" --version 2>&1 | head -1)"
case "$BANNER" in
  *'+pfhdr'*) ;;
  *) echo "ERROR: $BINARY has no +pfhdr marker — it is not a punktfunk gamescope build" >&2
     echo "       banner: $BANNER" >&2
     exit 1 ;;
esac

# Derive the version from the binary when the caller did not pass one — it is the only source that
# cannot drift from what is actually in the package.
if [ -z "${VERSION:-}" ]; then
  UPSTREAM="$(printf '%s\n' "$BANNER" | grep -o '[0-9]\+\.[0-9]\+\.[0-9]\+' | head -1)"
  PFHDR="$(printf '%s\n' "$BANNER" | grep -o '+pfhdr[0-9]\+' | head -1 | tr -d '+')"
  [ -n "$UPSTREAM" ] || { echo "ERROR: no X.Y.Z version in banner: $BANNER" >&2; exit 1; }
  # The Debian revision is the PKGBUILD's pkgrel, so a packaging-only change upgrades every channel.
  VERSION="${UPSTREAM}.${PFHDR}-$(sed -n 's/^pkgrel=//p' packaging/gamescope/PKGBUILD)"
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# mktemp gives 0700; the package root has to be world-readable or `dpkg-deb -c` shows the tree as
# root-only and some tooling refuses it.
chmod 0755 "$STAGE"
install -Dm0755 "$BINARY" "$STAGE/usr/bin/punktfunk-gamescope"
install -Dm0755 "$REAPER" "$STAGE/usr/lib/punktfunk/gamescope/gamescopereaper"
# /usr/lib/punktfunk, not a multiarch triplet dir: the layer manifest carries that absolute path
# baked in at build time, so the two have to agree. Nothing links the .so by soname — the Vulkan
# loader dlopens it by exactly that path — so multiarch has no say here.
install -Dm0755 "$LAYER_SO" "$STAGE/usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
install -Dm0644 "$LAYER_JSON" \
  "$STAGE/usr/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
# gamescope's notice as Debian's `copyright`, the bundled libraries' own beside it.
DOC_SRC="$SRC_STAGE/usr/share/doc/punktfunk-gamescope"
if [ -d "$DOC_SRC" ]; then
  mkdir -p "$STAGE/usr/share/doc/$PKG"
  cp -a "$DOC_SRC/." "$STAGE/usr/share/doc/$PKG/"
  cp "$DOC_SRC/LICENSE" "$STAGE/usr/share/doc/$PKG/copyright"
fi
mkdir -p "$STAGE/DEBIAN"

# Shared-library dependencies straight from the binary's own ELF NEEDED entries. That is what makes
# the package honest about the Ubuntu release it was compiled on: gamescope links a broad set
# (wlroots, SDL, libliftoff, vulkan, xwayland's libs), and hand-listing them would rot.
DEPS=""
if command -v dpkg-shlibdeps >/dev/null 2>&1; then
  # dpkg-shlibdeps insists on running from a package root with a debian/ dir. The layer counts too:
  # it alone links the system libwayland-client.
  mkdir -p "$STAGE/debian"
  : > "$STAGE/debian/control"
  ( cd "$STAGE" && dpkg-shlibdeps -O --ignore-missing-info usr/bin/punktfunk-gamescope \
      usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so 2>/dev/null ) \
    > "$STAGE/.shlibdeps" || true
  DEPS="$(sed -n 's/^shlibs:Depends=//p' "$STAGE/.shlibdeps" | head -1)"
  rm -rf "$STAGE/debian" "$STAGE/.shlibdeps"
fi
[ -n "$DEPS" ] || echo "WARNING: dpkg-shlibdeps produced no Depends — packaging without them" >&2

{
  echo "Package: $PKG"
  echo "Version: $VERSION"
  echo "Architecture: $DEB_ARCH"
  echo "Maintainer: unom <packages@unom.io>"
  echo "Section: utils"
  echo "Priority: optional"
  [ -n "$DEPS" ] && echo "Depends: $DEPS"
  # Not a hard dependency in either direction: the host works without this binary (SDR,
  # host-composited cursor), and someone may want the binary for their own capture consumer.
  echo "Recommends: punktfunk-host"
  echo "Homepage: https://git.unom.io/unom/punktfunk"
  echo "Description: gamescope with punktfunk's PipeWire capture patches"
  echo " gamescope built from the upstream revision punktfunk pins, plus the patches in"
  echo " packaging/gamescope/patches:"
  echo " ."
  echo "  * 10-bit BT.2020/PQ capture formats, so an HDR game reaches a capture consumer as HDR"
  echo "    instead of pre-tonemapped SDR."
  echo "  * --pipewire-composite-cursor: the pointer is painted into the capture stream, so a"
  echo "    consumer with no cursor of its own gets one and the host stops blending one in."
  echo "  * A headless session advertises its real mode and refresh rates (and"
  echo "    --custom-refresh-rates), so Steam and games see the resolution and refresh the stream"
  echo "    actually runs at instead of an unnamed 60 Hz panel."
  echo "  * --pipewire-composite-external-overlay: the mangoapp performance overlay is painted"
  echo "    into the capture stream, so the fps/stats readout is visible remotely."
  echo " ."
  echo " Installed as /usr/bin/punktfunk-gamescope, with its matching Vulkan WSI layer under"
  echo " /usr/lib/punktfunk. The layer has its own name and its own enable variable, so it sits"
  echo " beside your gamescope package's rather than replacing it; your system gamescope is"
  echo " untouched."
} > "$STAGE/DEBIAN/control"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT"
echo "==> wrote $OUT"
echo "    banner: $BANNER"
