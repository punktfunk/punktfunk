#!/bin/sh
# Download the Apple silicon bun pinned in ci/bun.env into a cache and print its path.
#
# The zip is checked against BUN_SHA_DARWIN_AARCH64 on every run and unpacked fresh, so the
# printed binary is always bytes the pin vouches for: the host bundle signs it as its own.
# bundle.sh takes the path and stays offline.
#
# Usage: sh packaging/macos/fetch-bun.sh [cache dir]
#   cache dir  default ~/Library/Caches/punktfunk-packaging
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
. "$root/ci/bun.env"
cache=${1:-$HOME/Library/Caches/punktfunk-packaging}/bun-$BUN_VERSION-darwin-aarch64
zip=$cache/bun-darwin-aarch64.zip

mkdir -p "$cache"
if ! echo "$BUN_SHA_DARWIN_AARCH64  $zip" | shasum -a 256 -c - >/dev/null 2>&1; then
    curl -fsSL -o "$zip" \
        "https://github.com/oven-sh/bun/releases/download/bun-v$BUN_VERSION/bun-darwin-aarch64.zip"
    echo "$BUN_SHA_DARWIN_AARCH64  $zip" | shasum -a 256 -c - >&2 || {
        rm -f "$zip"
        echo "fetch-bun.sh: the download doesn't match the pin in ci/bun.env" >&2
        exit 1
    }
fi
rm -rf "$cache/bun-darwin-aarch64"
unzip -q "$zip" -d "$cache"
echo "$cache/bun-darwin-aarch64/bun"
