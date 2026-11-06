#!/bin/sh
# Build PunktfunkCore.xcframework on the Mac runner once per commit.
#
# apple.yml's swift, distribute and screenshots jobs need the same seven-slice bundle for the same
# commit. The first job builds into the persistent target dir and parks a copy under
# ~/ci/xcframework/<sha>; later jobs copy it.
# Parked copies older than two days are removed.
#
# Usage, from the repo root with GITHUB_SHA set: sh scripts/ci/mac-xcframework.sh
set -eu
sha=${GITHUB_SHA:?GITHUB_SHA is unset}
store="$HOME/ci/xcframework"
bundle=clients/apple/PunktfunkCore.xcframework
mkdir -p "$store"
find "$store" -mindepth 1 -maxdepth 1 -mtime +2 -exec rm -rf {} +
# The String Catalog is per checkout, not per bundle: a reused bundle still needs it.
bash scripts/gen-l10n-apple.sh

if [ -d "$store/$sha/PunktfunkCore.xcframework" ]; then
    rm -rf "$bundle"
    ditto "$store/$sha/PunktfunkCore.xcframework" "$bundle"
    echo "reused the xcframework built for $sha"
    exit 0
fi

CARGO_TARGET_DIR="$(sh scripts/ci/mac-cargo-target.sh apple)"
export CARGO_TARGET_DIR
# Unchanged crates stay fresh across checkouts (scripts/ci/src-mtimes.rs). Every build into
# this target dir goes through here, which is what the manifest needs.
tool="${RUNNER_TEMP:-/tmp}/src-mtimes"
rustc -O scripts/ci/src-mtimes.rs -o "$tool" && "$tool" "$CARGO_TARGET_DIR" || echo "src-mtimes skipped" >&2
BUILD_IOS=1 BUILD_TVOS=1 BUILD_VISIONOS=1 bash scripts/build-xcframework.sh

# Park it through a temp dir and a rename, so a half-written copy never looks complete.
tmp="$store/.$sha.$$"
mkdir -p "$tmp"
ditto "$bundle" "$tmp/PunktfunkCore.xcframework"
rm -rf "$store/$sha"
mv "$tmp" "$store/$sha"
echo "parked the xcframework for $sha"
