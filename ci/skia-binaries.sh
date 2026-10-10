#!/bin/sh
# Put the pinned prebuilt Skia where SKIA_BINARIES_URL=file:///opt/skia-binaries/… reads it.
#
# skia-bindings otherwise fetches ~19 MB from github.com in its build script on every cold
# compile, and a failed fetch falls through to a from-source build these images cannot run.
# The builder images run this at build time. A job runs it again because it may be on the
# previous image; with the archives present it only checks their digests.
#
# Bump the pins with skia-safe in crates/client/pf-console-ui/Cargo.toml. Same archives and sums as
# packaging/flatpak/io.unom.Punktfunk.yml.
#
# Usage: sh ci/skia-binaries.sh <target-triple>...
set -e

VERSION=0.153.3
KEY=b7f043e0b1e2a850e702
FEATURES=ganesh-jpegd-jpege-pdf-textlayout-vulkan
DEST=/opt/skia-binaries

# A newer skia-bindings wants archives nobody pinned; stop before cargo falls back to a source build.
if [ -f Cargo.lock ] && ! grep -A1 '^name = "skia-bindings"$' Cargo.lock | grep -qx "version = \"$VERSION\""; then
  echo "::error::Cargo.lock no longer resolves skia-bindings $VERSION. Update the pins in ci/skia-binaries.sh."
  exit 1
fi

mkdir -p "$DEST"
for triple in "$@"; do
  features=$FEATURES
  url=https://github.com/rust-skia/skia-binaries/releases/download/$VERSION
  case $triple in
    x86_64-unknown-linux-gnu) sha=1620241c6f2247b6df21c6d201d5668693d2cf214f73dff06d66f40aa40f52b5 ;;
    aarch64-unknown-linux-gnu) sha=cae319203264a497291c34b66831e0af724acecaaa407e2f45b6635daeca6f64 ;;
    # rust-skia's wasm archive uses emscripten exceptions, which Rust's wasm std cannot link.
    # This one is built with -fwasm-exceptions by punktfunk/client-web's build.sh.
    wasm32-unknown-emscripten)
      sha=e7d8c13c829b041089ec35afecec611556ab9c79b68ce2ecf41a2a2611ec5832
      features=ganesh-gl-jpegd-jpege-pdf-textlayout
      url=https://git.unom.io/api/packages/unom/generic/skia-binaries/$VERSION ;;
    *) echo "no Skia pin for $triple" >&2; exit 1 ;;
  esac
  name=skia-binaries-$KEY-$triple-$features.tar.gz
  if echo "$sha  $DEST/$name" | sha256sum -c --quiet >/dev/null 2>&1; then
    echo "$name present"
    continue
  fi
  curl -fsSL --retry 10 --retry-delay 10 --retry-all-errors -o "$DEST/$name.part" \
    "$url/$name"
  echo "$sha  $DEST/$name.part" | sha256sum -c --quiet
  mv "$DEST/$name.part" "$DEST/$name"
  echo "$name fetched"
done
