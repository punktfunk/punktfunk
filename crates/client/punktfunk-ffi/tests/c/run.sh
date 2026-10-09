#!/usr/bin/env bash
# Build punktfunk-ffi's staticlib, then compile + link + run the C ABI harness against it.
# Proves the C ABI links from C. Works on Linux and macOS (link flags come from rustc).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ws="$(cd "$here/../../../../.." && pwd)"   # tests/c -> punktfunk-ffi -> client -> crates -> ws
cd "$ws"

profile="${1:-debug}"
build_flag=""
[ "$profile" = "release" ] && build_flag="--release"

# PF_SAN=address instruments BOTH sides of the C boundary at once: the staticlib via
# -Zsanitizer (nightly + -Zbuild-std, so std itself is instrumented) and the harness via
# clang -fsanitize. LSAN rides along (detect_leaks=1) and is the only automated check on
# the Box::into_raw/from_raw leak contract in the ABI. Linux x86_64 only; -Zbuild-std
# defeats sccache, so this belongs on a cron/dispatch job, not the per-push leg.
san="${PF_SAN:-}"
toolchain=""
target_args=""
target_sub=""
if [ -n "$san" ]; then
    san_target="x86_64-unknown-linux-gnu"
    # -Zsanitizer/-Zbuild-std need a nightly; PF_SAN_TOOLCHAIN pins a dated one (CI does).
    toolchain="+${PF_SAN_TOOLCHAIN:-nightly}"
    target_args="-Z build-std --target $san_target"
    target_sub="$san_target/"
    export RUSTFLAGS="-Zsanitizer=$san${RUSTFLAGS:+ $RUSTFLAGS}"
fi

echo ">> building punktfunk-ffi staticlib ($profile${san:+, sanitizer=$san})"
cargo $toolchain build $target_args -p punktfunk-ffi $build_flag >/dev/null

staticlib="$ws/target/${target_sub}$profile/libpunktfunk_ffi.a"
header_dir="$ws/include"
[ -f "$staticlib" ] || { echo "missing $staticlib"; exit 1; }
[ -f "$header_dir/punktfunk_core.h" ] || { echo "missing generated header"; exit 1; }

# Ask rustc what native libs the staticlib needs to link into a C program.
native_libs="$(cargo $toolchain rustc $target_args -p punktfunk-ffi --lib --crate-type staticlib $build_flag -- \
    --print native-static-libs 2>&1 | sed -n 's/.*native-static-libs: //p' | tail -1)"
echo ">> native libs: ${native_libs:-<none>}"

# Not mktemp: a debug+ASAN static binary can exceed a tmpfs /tmp; target/ is real disk.
out="$ws/target/${target_sub}$profile/punktfunk_harness"
cc="${CC:-cc}"
cflags=""
if [ -n "$san" ]; then
    cc="${CC:-clang}"
    cflags="-fsanitize=$san -fno-omit-frame-pointer"
fi
echo ">> compiling + linking harness"
$cc -std=c11 -Wall -Wextra -O2 $cflags ${CFLAGS:-} -I "$header_dir" \
    "$here/harness.c" "$staticlib" $native_libs -o "$out"

echo ">> running"
if [ -n "$san" ]; then
    ASAN_OPTIONS="detect_leaks=1${ASAN_OPTIONS:+:$ASAN_OPTIONS}" "$out"
else
    "$out"
fi
