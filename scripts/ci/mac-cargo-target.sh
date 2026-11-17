#!/bin/sh
# Print a cargo target dir that outlives the job on the persistent macOS runner.
#
# The act host executor deletes a job's checkout, target/ included, when the job ends. A dir
# under $HOME survives it, so apple.yml's distribute and screenshots jobs reuse the swift job's
# xcframework build, and every job recompiles only what changed.
#
# The dir name carries the Xcode and rustc versions: cargo fingerprints the linker's path but not
# the ld behind it (a newer ld breaks host proc-macros), nor MACOSX_DEPLOYMENT_TARGET, so a
# toolchain change starts clean. Older dirs of the same name are removed, and one past 30 GB is
# emptied.
#
# Usage: CARGO_TARGET_DIR="$(sh scripts/ci/mac-cargo-target.sh <name>)"
set -eu
name=${1:?usage: mac-cargo-target.sh <name>}
root="$HOME/ci/cargo-target"
xcode=$(xcodebuild -version 2>/dev/null | head -1 | tr ' ' '-')
rustc=$(rustc --version 2>/dev/null | cut -d' ' -f2)
dir="$root/$name-${xcode:-noxcode}-${rustc:-norustc}"
mkdir -p "$root"
for old in "$root/$name-"*; do
    if [ -d "$old" ] && [ "$old" != "$dir" ]; then
        echo "removing the stale target dir $old" >&2
        rm -rf "$old"
    fi
done
if [ -d "$dir" ] && [ "$(du -sk "$dir" | cut -f1)" -gt 31457280 ]; then
    echo "emptying $dir (over 30 GB)" >&2
    rm -rf "$dir"
fi
mkdir -p "$dir"
echo "cargo target dir: $dir ($(du -sh "$dir" | cut -f1))" >&2
echo "$dir"
