#!/bin/sh
# Build the drag-to-Applications disk image around a signed "Punktfunk Host.app".
#
# dmgbuild writes the window layout (.DS_Store) itself, so no Finder runs. It comes from
# dmg-requirements.txt, by hash, into a venv in the cache. The script mounts the finished image
# read-only to check it, then signs it when an identity is given.
#
# Usage: sh packaging/macos/dmg.sh -a <app> -v <version> -o <output dir> [-i <identity>]
#   identity  as for bundle.sh; - or none leaves the image unsigned
# Environment: PF_PYTHON, a Python 3.10 or later (default python3); PF_CACHE, the cache dir
# (default ~/Library/Caches/punktfunk-packaging).
# Output: <output dir>/Punktfunk-Host-<version>.dmg
set -eu

here=$(cd "$(dirname "$0")" && pwd)

die() {
    echo "dmg.sh: $*" >&2
    exit 1
}

app='' version='' out='' identity=''
while getopts a:v:o:i: opt; do
    case $opt in
        a) app=$OPTARG ;;
        v) version=$OPTARG ;;
        o) out=$OPTARG ;;
        i) identity=$OPTARG ;;
        *) die "usage: dmg.sh -a app -v version -o outdir [-i identity]" ;;
    esac
done
[ -n "$app" ] && [ -d "$app/Contents" ] || die "-a names no app bundle"
[ -n "$out" ] || die "the output directory is required"
case $version in
    '' | *[!0-9.]* | .* | *. | *..*) die "the version is dotted digits, such as 0.47.0" ;;
esac

py=${PF_PYTHON:-python3}
"$py" -c 'import sys; sys.exit(sys.version_info < (3, 10))' 2>/dev/null ||
    die "dmgbuild needs Python 3.10 or later: set PF_PYTHON"
venv=${PF_CACHE:-$HOME/Library/Caches/punktfunk-packaging}/dmgbuild-$(shasum -a 256 "$here/dmg-requirements.txt" | cut -c1-12)
if [ ! -x "$venv/bin/dmgbuild" ]; then
    rm -rf "$venv"
    "$py" -m venv "$venv"
    "$venv/bin/pip" install -q --disable-pip-version-check --require-hashes --no-deps \
        --only-binary=:all: -r "$here/dmg-requirements.txt" || {
        rm -rf "$venv"
        die "dmgbuild didn't install"
    }
fi

name=$(basename "$app")
icon=$app/Contents/Resources/AppIcon.icns
[ -f "$icon" ] || icon=
dmg=$out/Punktfunk-Host-$version.dmg
mkdir -p "$out"
rm -f "$dmg"
"$venv/bin/dmgbuild" -s "$here/dmg-settings.py" -D app="$app" -D icon="$icon" \
    -D background="$here/dmg-background.png" "Punktfunk Host" "$dmg" >&2

# Read the image back the way a Mac mounts it.
mnt=$(mktemp -d)
hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mnt" "$dmg" >/dev/null
ok=1
codesign --verify --strict "$mnt/$name" || ok=
[ "$(readlink "$mnt/Applications")" = /Applications ] && [ -f "$mnt/.DS_Store" ] &&
    [ -f "$mnt/.background.tiff" ] || ok=
[ -z "$icon" ] || [ -f "$mnt/.VolumeIcon.icns" ] || ok=
hdiutil detach "$mnt" -quiet
rmdir "$mnt"
[ -n "$ok" ] || die "$dmg lacks a valid app, the Applications link or the window layout"

if [ -n "$identity" ] && [ "$identity" != - ]; then
    codesign --force --timestamp --sign "$identity" "$dmg"
    codesign --verify --strict --verbose=2 "$dmg"
fi
echo "$dmg"
