#!/usr/bin/env bash
# Package an already-built punktfunk-gamescope binary as an RPM, for the Gitea RPM registry.
#
# WHY this exists: a traditional Fedora-family box (Nobara, plain Fedora, Nobara-derived HTPCs) has
# no other packaged route to punktfunk-gamescope. The Bazzite/Atomic sysext, the Arch package, the
# SteamOS installer and the NixOS option serve other platforms; the alternative is building
# gamescope from source.
#
# Nothing is BUILT here; CI builds once per Fedora major and caches the staged tree
# (.gitea/workflows/rpm.yml). See punktfunk-gamescope.spec's header for why repacking beats
# rebuilding.
#
# `--stage` is the DESTDIR that build-punktfunk-gamescope.sh wrote, not a single binary: that tree
# carries the compositor AND the WSI layer built beside it, and a game gets an HDR10 swapchain from
# that layer or from nowhere. Taking the whole tree rather than a file per artifact is deliberate —
# it is what stops the next file added to the package needing a new flag in every packaging script.
#
# Usage:
#   bash packaging/gamescope/build-gamescope-rpm.sh \
#     --stage gs-cache \
#     [--version 3.16.25] [--release 1] [--outdir dist]
#
# Output: <outdir>/punktfunk-gamescope-<version>-<release>.<arch>.rpm
set -euo pipefail

STAGE=""
# Default the version to the upstream gamescope the pinned revision describes as, suffixed with the
# patch-set revision — same shape as the Arch package's `pkgver`, so the two channels read alike.
VERSION=""
RELEASE="1"
OUTDIR="dist"

while [ $# -gt 0 ]; do
  case "$1" in
    --stage)   STAGE="${2:?--stage needs a path}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a value}"; shift 2 ;;
    --release) RELEASE="${2:?--release needs a value}"; shift 2 ;;
    --outdir)  OUTDIR="${2:?--outdir needs a value}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$STAGE" ] || { echo "ERROR: --stage is required" >&2; exit 2; }
# The layout build-punktfunk-gamescope.sh writes under its --destdir/--prefix.
BINARY="$STAGE/usr/bin/punktfunk-gamescope"
REAPER="$STAGE/usr/lib/punktfunk/gamescope/gamescopereaper"
LAYER_SO="$STAGE/usr/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
LAYER_JSON="$STAGE/usr/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
[ -x "$BINARY" ] || { echo "ERROR: $BINARY is not an executable file" >&2; exit 1; }
# gamescope starts every session's first child through its reaper.
[ -x "$REAPER" ] || { echo "ERROR: $REAPER missing from the stage — a session would run nothing" >&2; exit 1; }
# Hard, not best-effort. A package that carries the compositor without its layer looks completely
# healthy and then silently denies every game an HDR10 swapchain.
for f in "$LAYER_SO" "$LAYER_JSON"; do
  [ -f "$f" ] || { echo "ERROR: $f missing from the stage — no game HDR without it" >&2; exit 1; }
done

ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"
# The PKGBUILD's pkgrel leads the release, so a packaging-only change upgrades every channel.
RELEASE="$(sed -n 's/^pkgrel=//p' packaging/gamescope/PKGBUILD).${RELEASE}"

# Derive the version from the binary itself when not told: it is the only source that cannot drift
# from what is actually being packaged. `gamescope version 3.16.25-1-g8c676c3+pfhdr4 (gcc …)` →
# `3.16.25` + the marker. RPM versions may not contain `-`, hence the trailing `.pfhdrN` form.
BANNER="$("$BINARY" --version 2>&1 | head -1)"
case "$BANNER" in
  *'+pfhdr'*) ;;
  *) echo "ERROR: $BINARY has no +pfhdr marker — it is not a punktfunk gamescope build" >&2
     echo "       banner: $BANNER" >&2
     exit 1 ;;
esac
PFHDR="$(printf '%s\n' "$BANNER" | grep -o '+pfhdr[0-9]\+' | head -1 | tr -d '+')"
if [ -z "$VERSION" ]; then
  UPSTREAM="$(printf '%s\n' "$BANNER" | grep -o '[0-9]\+\.[0-9]\+\.[0-9]\+' | head -1)"
  [ -n "$UPSTREAM" ] || { echo "ERROR: no X.Y.Z version in banner: $BANNER" >&2; exit 1; }
  VERSION="${UPSTREAM}.${PFHDR}"
fi

echo "==> packaging $BINARY as punktfunk-gamescope-${VERSION}-${RELEASE}"
echo "    banner: $BANNER"

TOP="$(mktemp -d)"
trap 'rm -rf "$TOP"' EXIT
mkdir -p "$TOP"/{SOURCES,SPECS,BUILD,BUILDROOT,RPMS,SRPMS}
install -m0755 "$BINARY" "$TOP/SOURCES/punktfunk-gamescope"
install -m0755 "$REAPER" "$TOP/SOURCES/gamescopereaper"
install -m0755 "$LAYER_SO" "$TOP/SOURCES/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
install -m0644 "$LAYER_JSON" "$TOP/SOURCES/punktfunk_gamescope_wsi.json"

mkdir -p "$OUTDIR"
rpmbuild \
  --define "_topdir $TOP" \
  --define "pf_version $VERSION" \
  --define "pf_release $RELEASE" \
  -bb packaging/gamescope/punktfunk-gamescope.spec

find "$TOP/RPMS" -name '*.rpm' -exec cp -v {} "$OUTDIR/" \;
echo "==> wrote $(find "$OUTDIR" -name 'punktfunk-gamescope-*.rpm' -newer "$TOP" -print -quit 2>/dev/null || echo "$OUTDIR"/punktfunk-gamescope-*.rpm)"
