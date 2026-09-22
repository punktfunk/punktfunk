#!/bin/bash
# Build the two tvOS Skia archives rust-skia does not publish (device, arm64 simulator), for the
# skia-bindings version pf-console-ui pins. Its build script has no tvOS platform; the patch
# beside this adds one. Upload both archives to git.unom.io/unom/skia-binaries under the same
# tag, then update their SHA-256s in crates/pf-console-ui/Cargo.toml.
# usage: scripts/skia-tvos/build.sh <work-dir>   (about 6 GB; Skia builds in minutes)
set -euo pipefail
VER=0.99.0
HASH=a25a0fdb7d90429aa2d1
HERE=$(cd "$(dirname "$0")" && pwd)
NIGHTLY=$(sed -n 's/^NIGHTLY=//p' "$HERE/../build-xcframework.sh")
mkdir -p "$1"
WORK=$(cd "$1" && pwd)
SRC=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/skia-bindings-$VER 2>/dev/null | head -1)
if [[ -z "$SRC" ]]; then
    echo "skia-bindings $VER is not in the cargo registry: run cargo fetch first" >&2
    exit 1
fi

# The copy keeps .cargo_vcs_info.json, so the archive key keeps rust-skia's hash.
rm -rf "$WORK/skia-bindings"
cp -R "$SRC" "$WORK/skia-bindings"
patch -d "$WORK/skia-bindings" -p1 < "$HERE/skia-bindings-$VER.patch"

mkdir -p "$WORK/build/src"
cat > "$WORK/build/Cargo.toml" <<EOF
[package]
name = "skia-tvos-build"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
skia-safe = { version = "=$VER", features = ["metal", "textlayout"] }

[patch.crates-io]
skia-bindings = { path = "../skia-bindings" }

[workspace]
EOF
echo 'pub use skia_safe;' > "$WORK/build/src/lib.rs"

for T in aarch64-apple-tvos aarch64-apple-tvos-sim; do
    (cd "$WORK/build" && CARGO_TARGET_DIR="$WORK/target" TVOS_DEPLOYMENT_TARGET=17.0 \
        FORCE_SKIA_BUILD=1 cargo "+$NIGHTLY" build --release -Z build-std=std,panic_abort \
        --target "$T")
    LIBS=$(find "$WORK/target/$T/release/build" -path '*skia-bindings*' -name libskia.a)
    OUT=$(dirname "$(ls -t $LIBS | head -1)")
    KEY="$HASH-$T-jpegd-jpege-metal-pdf-textlayout"
    STAGE="$WORK/stage/$T/skia-binaries"
    rm -rf "$WORK/stage/$T"
    mkdir -p "$STAGE"
    for lib in skia skshaper skparagraph skunicode_core skunicode_icu skia-bindings; do
        cp "$OUT/lib$lib.a" "$STAGE/"
    done
    cp "$OUT/bindings.rs" "$STAGE/"
    cp "$WORK/skia-bindings/skia/LICENSE" "$STAGE/LICENSE_SKIA"
    printf '%s' "$VER" > "$STAGE/tag.txt"
    printf '%s' "$KEY" > "$STAGE/key.txt"
    tar -C "$WORK/stage/$T" -czf "$WORK/skia-binaries-$KEY.tar.gz" skia-binaries
    shasum -a 256 "$WORK/skia-binaries-$KEY.tar.gz"
done
