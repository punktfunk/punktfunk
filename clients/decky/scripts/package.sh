#!/usr/bin/env bash
# Assemble the Decky plugin into the canonical store/sideload layout:
#
#   out/punktfunk-v<version>.zip   ->  punktfunk/{dist/index.js,main.py,py_modules/pf_decky/,
#                                                  plugin.json,package.json,decky.pyi,LICENSE,README.md}
#   out/punktfunk/                 (the same tree, unzipped — rsync this with scripts/deploy.sh)
#
# The single top-level dir is the plugin's ON-DISK folder name (Decky extracts the zip as-is,
# so the dir in the zip becomes ~/homebrew/plugins/<dir>). It is deliberately NOT read from
# plugin.json "name": that field is the user-visible label ("Punktfunk", brand-cased, shown in
# Decky's plugin list) and Decky locates an installed plugin by MATCHING it, never by the folder
# name. Keeping the folder lowercase means a rename of the label can't strand the old directory
# next to a new one (which would show up as two plugins).
# Run after `pnpm build` (or use `pnpm run package`). Host-agnostic: needs only bash, python3 and zip.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

[ -f dist/index.js ] || { echo "dist/index.js missing — run 'pnpm build' first" >&2; exit 1; }
[ -f LICENSE ]       || { echo "LICENSE missing (required by the Decky store)" >&2; exit 1; }

NAME=punktfunk   # the on-disk plugin dir (see the header) — NOT plugin.json "name"
VER="$(python3 -c 'import json;print(json.load(open("package.json"))["version"])')"

STAGE="$(mktemp -d)"
DEST="$STAGE/$NAME"
mkdir -p "$DEST/dist" "$DEST/bin" "$DEST/assets"
cp dist/index.js "$DEST/dist/index.js"          # ship the bundle only, not the sourcemap
cp main.py plugin.json package.json LICENSE "$DEST/"
# The backend's modules, sources only: Decky puts py_modules/ on sys.path, and a stray
# __pycache__ from a local test run must not ship.
mkdir -p "$DEST/py_modules/pf_decky"
cp py_modules/pf_decky/*.py "$DEST/py_modules/pf_decky/"
# The stream-launch wrapper (target of the Steam shortcut) — must stay executable.
cp bin/punktfunkrun.sh "$DEST/bin/punktfunkrun.sh"
chmod 0755 "$DEST/bin/punktfunkrun.sh"
# Steam-shortcut artwork (grid/gridwide/hero/logo/icon — committed under assets/).
cp assets/*.png "$DEST/assets/"
# The Steam Input controller layout (native touchscreen `ts_n` + gamepad passthrough) the
# backend installs (apply_controller_config → controller_base/templates + the shortcut config).
mkdir -p "$DEST/controller_config"
cp controller_config/punktfunk.vdf "$DEST/controller_config/punktfunk.vdf"
[ -f decky.pyi ]  && cp decky.pyi  "$DEST/"
[ -f README.md ]  && cp README.md  "$DEST/"

OUT="$HERE/out"
mkdir -p "$OUT"
ZIP="$OUT/${NAME}-v${VER}.zip"
rm -f "$ZIP"
( cd "$STAGE" && zip -r -X "$ZIP" "$NAME" >/dev/null )
# Leave an unzipped staging tree for the rsync/sudo deploy path (scripts/deploy.sh).
rm -rf "$OUT/$NAME" && cp -r "$DEST" "$OUT/$NAME"
rm -rf "$STAGE"

echo "built  $ZIP"
echo "staged $OUT/$NAME  (deploy with: DECK=deck@<ip> bash scripts/deploy.sh)"
