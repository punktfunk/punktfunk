#!/bin/sh
# Post-link floor check for libpunktfunk_android.so: every undefined dynamic symbol must be
# defined in the NDK's minSdk-level stub libraries, for every built ABI.
#
# Why this exists: a Rust cdylib links fine with dangling undefined symbols (lld only rejects them
# under --no-undefined, which rustc does not pass), so a hard import of an above-floor NDK entry
# point sails through `cargo ndk --platform 28` and only explodes at `System.loadLibrary` on every
# device below the symbol's introduction level (AMediaCodec_setOnFrameRenderedCallback is API 33,
# so a hard import would stop the client loading on every pre-Android-13 device). Above-floor
# entry points must be dlsym-resolved at runtime instead (see decode::try_set_frame_rate /
# decode::install_render_callback / adpf). This script makes the violation a build failure.
# Wired into clients/android/kit/build.gradle.kts after each cargo-ndk build (local + CI).
#
# Usage: check-android-jni-imports.sh <ndk-dir> <jniLibs-dir> <api-level>
set -eu

NDK=$1
JNILIBS=$2
API=$3

# Host-agnostic prebuilt dir (linux-x86_64 on CI and the dev box, darwin-* on a Mac).
PREBUILT=$(echo "$NDK"/toolchains/llvm/prebuilt/*)
NM="$PREBUILT/bin/llvm-nm"
[ -x "$NM" ] || { echo "check-android-jni-imports: llvm-nm not found under $NDK" >&2; exit 1; }

triple_for_abi() {
    case "$1" in
        arm64-v8a) echo aarch64-linux-android ;;
        armeabi-v7a) echo arm-linux-androideabi ;;
        x86_64) echo x86_64-linux-android ;;
        x86) echo i686-linux-android ;;
        *) echo "" ;;
    esac
}

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

checked=0
fail=0
for so in "$JNILIBS"/*/libpunktfunk_android.so; do
    [ -f "$so" ] || continue
    abi=$(basename "$(dirname "$so")")
    triple=$(triple_for_abi "$abi")
    if [ -z "$triple" ]; then
        echo "check-android-jni-imports: unknown ABI dir '$abi' — extend triple_for_abi" >&2
        exit 1
    fi
    stubs="$PREBUILT/sysroot/usr/lib/$triple/$API"
    [ -d "$stubs" ] || { echo "check-android-jni-imports: no stub dir $stubs" >&2; exit 1; }

    # Strong undefined imports only ('U'; weak 'w'/'v' resolve to null and are fine), and the
    # stubs' exports, both with the @VERSION / @@VERSION suffix stripped.
    "$NM" -D --undefined-only "$so" | awk '$1 == "U" { print $2 }' | sed 's/@.*//' | sort -u \
        > "$TMP/undef"
    for stub in "$stubs"/*.so; do
        "$NM" -D --defined-only "$stub" 2>/dev/null | awk '{ print $NF }' | sed 's/@.*//'
    done | sort -u > "$TMP/defined"

    missing=$(comm -23 "$TMP/undef" "$TMP/defined")
    if [ -n "$missing" ]; then
        echo "check-android-jni-imports: $abi libpunktfunk_android.so imports symbols absent from" >&2
        echo "the API-$API sysroot — System.loadLibrary would fail on devices at the minSdk floor." >&2
        echo "dlsym-resolve these at runtime instead of hard-linking them:" >&2
        echo "$missing" | sed 's/^/    /' >&2
        fail=1
    fi
    checked=$((checked + 1))
done

[ "$checked" -gt 0 ] || { echo "check-android-jni-imports: no libpunktfunk_android.so under $JNILIBS" >&2; exit 1; }
[ "$fail" -eq 0 ] && echo "check-android-jni-imports: $checked ABI(s) clean at the API-$API floor"
exit "$fail"
