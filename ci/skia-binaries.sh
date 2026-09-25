#!/bin/sh
# Put the pinned prebuilt Skia where SKIA_BINARIES_URL=file:///opt/skia-binaries/… reads it.
#
# skia-bindings otherwise fetches ~19 MB from github.com in its build script on every cold
# compile, and a failed fetch falls through to a from-source build these images cannot run.
# The builder images run this at build time. A job runs it again because it may be on the
# previous image; with the archives present it only checks their digests.
#
# Bump the pins with skia-safe in crates/pf-console-ui/Cargo.toml. Same archives and sums as
# packaging/flatpak/io.unom.Punktfunk.yml.
#
# Usage: sh ci/skia-binaries.sh <target-triple>...
set -e

VERSION=0.99.0
KEY=a25a0fdb7d90429aa2d1
FEATURES=jpegd-jpege-pdf-textlayout-vulkan
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
    x86_64-unknown-linux-gnu) sha=158407a4b5ce8738431bb76498be3a44fda770e51d61aac18a0e0e97becdc1de ;;
    aarch64-unknown-linux-gnu) sha=cf5469d1d963f704cc997f9b3342d11c49b917002361947e5c7bf7dcc3f13534 ;;
    # rust-skia's wasm archive uses emscripten exceptions, which Rust's wasm std cannot link.
    # This one is built with -fwasm-exceptions by punktfunk/client-web's build.sh.
    wasm32-unknown-emscripten)
      sha=b60b81d31578f96ad13892d6b94510a7092e2928e9da6c45170f2ad18f44acdf
      features=gl-jpegd-jpege-pdf-textlayout
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
