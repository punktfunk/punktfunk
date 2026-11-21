#!/bin/sh
# Run the rustup-init pinned in ci/rustup.env, checked by SHA-256, with the arguments given.
#
# Never `curl https://sh.rustup.rs | sh`: that runs whatever the server sends, unchecked, in
# front of every toolchain the packages are built with.
#
# Usage: sh ci/install-rustup.sh -y --no-modify-path --profile minimal [more rustup-init args]
#        (needs curl, and sha256sum or shasum)
set -eu
. "$(dirname "$0")/rustup.env"

case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) target=x86_64-unknown-linux-gnu sha=$RUSTUP_SHA_X86_64_LINUX ;;
  Linux/aarch64) target=aarch64-unknown-linux-gnu sha=$RUSTUP_SHA_AARCH64_LINUX ;;
  Darwin/arm64) target=aarch64-apple-darwin sha=$RUSTUP_SHA_AARCH64_DARWIN ;;
  Darwin/x86_64) target=x86_64-apple-darwin sha=$RUSTUP_SHA_X86_64_DARWIN ;;
  *) echo "no pinned rustup-init for $(uname -s)/$(uname -m)" >&2; exit 1 ;;
esac

dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
curl --proto '=https' --tlsv1.2 -fsSL -o "$dir/rustup-init" \
  "https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/$target/rustup-init"
if command -v sha256sum >/dev/null 2>&1; then
  echo "$sha  $dir/rustup-init" | sha256sum -c -
else
  echo "$sha  $dir/rustup-init" | shasum -a 256 -c -
fi
chmod +x "$dir/rustup-init"
"$dir/rustup-init" "$@"
