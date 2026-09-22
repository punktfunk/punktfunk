#!/usr/bin/env bash
# Build PunktfunkCore.xcframework for the Apple clients — run ON A MAC with Xcode + rustup.
#
#   rustup target add aarch64-apple-darwin   # + aarch64-apple-ios for iOS
#   bash scripts/build-xcframework.sh
#
# Output: clients/apple/PunktfunkCore.xcframework (consumed by clients/apple/Package.swift).
# The library is built WITH the `quic` feature (the punktfunk/1 connection API), so the bundled
# header gets PUNKTFUNK_FEATURE_QUIC pre-defined — Swift sees punktfunk_connect & co. unconditionally.
set -euo pipefail
cd "$(dirname "$0")/.."
# CI points CARGO_TARGET_DIR at a dir that outlives the job (scripts/ci/mac-cargo-target.sh).
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# Apple silicon only: Intel Macs are discontinued, so no slice is built for them.
TARGETS_MAC=(aarch64-apple-darwin)
BUILD_IOS="${BUILD_IOS:-0}" # BUILD_IOS=1 adds iOS device + simulator slices (rustup targets aarch64-apple-ios{,-sim})
BUILD_TVOS="${BUILD_TVOS:-0}" # BUILD_TVOS=1 adds tvOS slices — TIER-3 Rust targets, built with $NIGHTLY

# The one place the tvOS toolchain is named — .gitea/workflows/apple.yml reads this line, so keep
# the shape. Pinned: a floating `nightly` swaps the compiler under a TestFlight build with no
# commit. Install with: rustup toolchain install $NIGHTLY --profile minimal --component rust-src
NIGHTLY=nightly-2026-08-11

# Toolchain resolution. Cargo's HOST artifacts (proc-macros, build scripts) are loaded by
# the RUNNING OS, so a beta Xcode's ld must not link them. CLT ships no iOS/tvOS SDKs.
# Resolution: a NON-BETA full Xcode for everything; with only a beta installed, macOS
# slices build against CLT and iOS/tvOS slices are refused.
pick_nonbeta_xcode() {
    local app
    for app in /Applications/Xcode.app /Applications/Xcode*.app; do
        case "$app" in *[Bb]eta*) continue ;; esac
        [ -x "$app/Contents/Developer/usr/bin/xcodebuild" ] && { echo "$app/Contents/Developer"; return; }
    done
}
case "${DEVELOPER_DIR:-}" in *[Bb]eta*) unset DEVELOPER_DIR ;; esac # never let a beta in via env
if [[ -z "${DEVELOPER_DIR:-}" ]]; then
    DEFAULT_DIR="$(xcode-select -p 2>/dev/null || true)"
    case "$DEFAULT_DIR" in
    *[Bb]eta*|*CommandLineTools*|'')
        NONBETA="$(pick_nonbeta_xcode || true)"
        if [[ -n "$NONBETA" ]]; then
            export DEVELOPER_DIR="$NONBETA"
        elif [[ "$BUILD_IOS" == "1" || "$BUILD_TVOS" == "1" ]]; then
            echo "ERROR: iOS/tvOS slices need a full NON-BETA Xcode in /Applications" >&2
            echo "       (CLT has no iOS SDK; a beta's ld breaks host proc-macro dylibs)." >&2
            exit 1
        elif [[ "$DEFAULT_DIR" != *CommandLineTools* ]]; then
            echo "ERROR: xcode-select default is a beta (or missing) and no non-beta Xcode/CLT" >&2
            echo "       fallback exists — install CLT or a release Xcode." >&2
            exit 1
        fi
        # else: the default IS CLT — dyld-safe for the mac slices; deliberately leave the
        # env untouched (an EXPLICIT DEVELOPER_DIR=<CLT> export trips xcrun's Xcode
        # license check when a full Xcode is also installed).
        ;;
    esac # a non-beta xcode-select default is fine as-is
fi

# Proc-macros are dylibs the running OS loads. With chained fixups — on at any deployment
# target >= 12, which the mac slices set — Xcode 27's ld writes one macOS 27 refuses
# ("mis-aligned LINKEDIT string pool", reported by cargo as E0463), so host links go without.
# The linker file lives in the target dir and changes only on edit: cargo rebuilds when it does.
HOST_LINKER="$(mkdir -p "$TARGET_DIR" && cd "$TARGET_DIR" && pwd)/proc-macro-linker"
HOST_LINKER_SH='#!/bin/sh
exec cc -Wl,-no_fixup_chains "$@"'
if [[ "$(cat "$HOST_LINKER" 2>/dev/null)" != "$HOST_LINKER_SH" ]]; then
    printf '%s\n' "$HOST_LINKER_SH" > "$HOST_LINKER"
    chmod +x "$HOST_LINKER"
fi
# Both host triples. On the same-triple slice it also links the cdylib, which is not shipped.
export CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER="$HOST_LINKER"
export CARGO_TARGET_X86_64_APPLE_DARWIN_LINKER="$HOST_LINKER"

# Hermetic Opus: never let audiopus_sys link a Homebrew libopus via pkg-config. A brew lib
# is built for the RUNNING macOS (its objects carry that minos, tripping the version guard
# below) and only exists for the host arch — the other slice silently falls back to the
# vendored build, so the two slices ship different libopus builds. Force the vendored CMake
# build everywhere; the policy floor keeps modern CMake (≥4) accepting libopus's old
# `cmake_minimum_required`.
export OPUS_NO_PKG_CONFIG=1
export CMAKE_POLICY_VERSION_MINIMUM=3.5

# Deployment targets must match Package.swift's platforms, or every consumer link emits
# "object file was built for newer macOS version" warnings.
for t in "${TARGETS_MAC[@]}"; do
    MACOSX_DEPLOYMENT_TARGET=14.0 cargo build --release -p punktfunk-core --features quic --target "$t"
done
if [[ "$BUILD_IOS" == "1" ]]; then
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p punktfunk-core --features quic --target aarch64-apple-ios
    IPHONEOS_DEPLOYMENT_TARGET=17.0 cargo build --release -p punktfunk-core --features quic --target aarch64-apple-ios-sim
fi
if [[ "$BUILD_TVOS" == "1" ]]; then
    # Tier-3 targets: no prebuilt std — $NIGHTLY + -Zbuild-std compiles it from rust-src.
    TVOS_DEPLOYMENT_TARGET=17.0 cargo "+$NIGHTLY" build --release -p punktfunk-core --features quic \
        -Z build-std=std,panic_abort --target aarch64-apple-tvos
    TVOS_DEPLOYMENT_TARGET=17.0 cargo "+$NIGHTLY" build --release -p punktfunk-core --features quic \
        -Z build-std=std,panic_abort --target aarch64-apple-tvos-sim
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

mkdir -p "$STAGE/macos"
cp "$TARGET_DIR"/aarch64-apple-darwin/release/libpunktfunk_core.a "$STAGE/macos/"

# Headers dir: the generated C header (with the quic API force-enabled) + a modulemap so
# Swift can `import PunktfunkCore`.
mkdir -p "$STAGE/include"
{
    echo "#define PUNKTFUNK_FEATURE_QUIC 1"
    cat include/punktfunk_core.h
} > "$STAGE/include/punktfunk_core.h"
cat > "$STAGE/include/module.modulemap" <<'EOF'
module PunktfunkCore {
    header "punktfunk_core.h"
    export *
}
EOF

ARGS=(-library "$STAGE/macos/libpunktfunk_core.a" -headers "$STAGE/include")
if [[ "$BUILD_IOS" == "1" ]]; then
    ARGS+=(-library "$TARGET_DIR"/aarch64-apple-ios/release/libpunktfunk_core.a -headers "$STAGE/include")
    ARGS+=(-library "$TARGET_DIR"/aarch64-apple-ios-sim/release/libpunktfunk_core.a -headers "$STAGE/include")
fi
if [[ "$BUILD_TVOS" == "1" ]]; then
    ARGS+=(-library "$TARGET_DIR"/aarch64-apple-tvos/release/libpunktfunk_core.a -headers "$STAGE/include")
    ARGS+=(-library "$TARGET_DIR"/aarch64-apple-tvos-sim/release/libpunktfunk_core.a -headers "$STAGE/include")
fi

# Cargo does NOT fingerprint MACOSX_DEPLOYMENT_TARGET — units cached from a build without
# it keep their old minos forever. Refuse to ship anything newer than the package floor
# (objects BELOW it, e.g. rustup's precompiled std at 11.0, are fine and unavoidable).
obj="$STAGE/macos/libpunktfunk_core.a"
bad=$(otool -l "$obj" 2>/dev/null | awk '/minos/ {print $2}' | sort -uV | awk -F. '$1 > 14' | head -1)
if [[ -n "$bad" ]]; then
    echo "ERROR: $obj contains objects built for macOS $bad (> 14.0)." >&2
    echo "Stale cache — rm -rf $TARGET_DIR/aarch64-apple-darwin and rebuild." >&2
    exit 1
fi

# -create-xcframework needs a full Xcode (CLT has no xcodebuild) but does NO linking —
# it only copies the libs and writes the bundle plist, so a beta Xcode is safe here.
XCODEBUILD=(xcodebuild)
if ! xcodebuild -version >/dev/null 2>&1; then
    for app in /Applications/Xcode.app /Applications/Xcode*.app; do
        if DEVELOPER_DIR="$app/Contents/Developer" xcodebuild -version >/dev/null 2>&1; then
            XCODEBUILD=(env DEVELOPER_DIR="$app/Contents/Developer" xcodebuild)
            echo "==> using $app for -create-xcframework"
            break
        fi
    done
fi

rm -rf clients/apple/PunktfunkCore.xcframework
"${XCODEBUILD[@]}" -create-xcframework "${ARGS[@]}" -output clients/apple/PunktfunkCore.xcframework

# Xcode (unlike `swift build`) refuses to EMBED an unsigned xcframework: the app targets in
# Punktfunk.xcodeproj fail with "The framework 'PunktfunkCore.xcframework' is unsigned". So
# sign the bundle here. Identity: $CODESIGN_IDENTITY if set, else the first "Apple Development"
# identity in the keychain, else ad-hoc ("-") — ad-hoc satisfies `swift build` and most local
# Xcode runs; a real identity is needed for device/distribution. --timestamp=none keeps it
# offline (a secure timestamp only matters for notarized distribution, which re-signs anyway).
SIGN_ID="${CODESIGN_IDENTITY:-}"
if [[ -z "$SIGN_ID" ]]; then
    SIGN_ID=$(security find-identity -v -p codesigning 2>/dev/null \
        | awk -F'"' '/Apple Development/ {print $2; exit}')
fi
SIGN_ID="${SIGN_ID:--}" # ad-hoc fallback when no real identity is available
if codesign --force --timestamp=none --sign "$SIGN_ID" clients/apple/PunktfunkCore.xcframework; then
    echo "OK: clients/apple/PunktfunkCore.xcframework (signed: $SIGN_ID)"
else
    echo "WARN: clients/apple/PunktfunkCore.xcframework built but NOT signed — Xcode app" >&2
    echo "      builds will report it unsigned. Set CODESIGN_IDENTITY and re-run." >&2
    echo "OK: clients/apple/PunktfunkCore.xcframework (unsigned)"
fi
