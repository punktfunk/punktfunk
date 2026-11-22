#!/bin/sh
# Build the two payloads the host bundle runs on bun: the web console and the plugin runner.
#
# Both are built as the Linux packages build them (packaging/debian/build-web-deb.sh and
# build-scripting-deb.sh), into a staging dir that bundle.sh copies from.
#
# Usage: sh packaging/macos/build-payloads.sh [-u bun] -o <staging dir>
#   bun  the bun that builds; default PF_BUN, else the one on PATH. CI passes fetch-bun.sh's.
# Output: <dir>/web/.output/{server,public} and <dir>/scripting/runner-cli.js
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)

die() {
    echo "build-payloads.sh: $*" >&2
    exit 1
}

bun=${PF_BUN:-} out=${PF_PAYLOADS:-}
while getopts u:o: opt; do
    case $opt in
        u) bun=$OPTARG ;;
        o) out=$OPTARG ;;
        *) die "usage: build-payloads.sh [-u bun] -o staging-dir" ;;
    esac
done
[ -n "$out" ] || die "the staging directory is required"
bun=${bun:-$(command -v bun)} || die "no bun: pass -u or put one on PATH"
# Package scripts call `bun` by name, so the one building must be the first on PATH.
PATH=$(cd "$(dirname "$bun")" && pwd):$PATH
export PATH

rm -rf "$out/web" "$out/scripting"
mkdir -p "$out/web/.output" "$out/scripting"

# --ignore-scripts as everywhere in CI: web's postinstall only regenerates bun.nix.
(cd "$root/web" && bun install --frozen-lockfile --ignore-scripts && bun run build)
grep -q 'Bun\.serve' "$root/web/.output/server/index.mjs" ||
    die "web/.output has no Bun.serve: the build took the wrong Nitro preset"
cp -R "$root/web/.output/server" "$root/web/.output/public" "$out/web/.output/"

(cd "$root/sdk" && bun install --frozen-lockfile --ignore-scripts &&
    bun build src/runner-cli.ts --target=bun --outfile="$out/scripting/runner-cli.js")
grep -q 'attempt=' "$out/scripting/runner-cli.js" ||
    die "the runner bundle lacks the dynamic plugin import: wrong build"
echo "$out"
