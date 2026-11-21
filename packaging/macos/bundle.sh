#!/bin/sh
# Assemble and sign "Punktfunk Host.app" around a built punktfunk-host binary.
#
# A restricted entitlement and every privacy grant attach to a signed bundle, so the host
# cannot run as a bare binary on macOS. This script builds that bundle; it installs nothing.
#
# Usage: sh packaging/macos/bundle.sh -b <binary> -p <profile> -i <identity> -t <team id> \
#          -v <version> -o <output dir>
#
# Each option falls back to an environment variable: PF_BINARY, PF_PROFILE, PF_IDENTITY,
# PF_TEAM_ID, PF_VERSION, PF_OUT.
#   binary    the built punktfunk-host (arm64)
#   profile   a macOS .provisionprofile for io.unom.punktfunk.host that carries
#             com.apple.developer.hid.virtual.device
#   identity  anything codesign accepts: a name such as "Apple Development", or a certificate hash
#   team id   the ten-character Apple team id of the profile
#   version   dotted digits, such as 0.47.0; the binary's own version comes from its build
set -eu

here=$(cd "$(dirname "$0")" && pwd)
plistbuddy=/usr/libexec/PlistBuddy
bundle_id=io.unom.punktfunk.host

die() {
    echo "bundle.sh: $*" >&2
    exit 1
}

binary=${PF_BINARY:-} profile=${PF_PROFILE:-} identity=${PF_IDENTITY:-}
team=${PF_TEAM_ID:-} version=${PF_VERSION:-} out=${PF_OUT:-}
while getopts b:p:i:t:v:o: opt; do
    case $opt in
        b) binary=$OPTARG ;;
        p) profile=$OPTARG ;;
        i) identity=$OPTARG ;;
        t) team=$OPTARG ;;
        v) version=$OPTARG ;;
        o) out=$OPTARG ;;
        *) die "usage: bundle.sh -b binary -p profile -i identity -t team -v version -o outdir" ;;
    esac
done

[ -n "$binary" ] && [ -n "$profile" ] && [ -n "$identity" ] && [ -n "$team" ] &&
    [ -n "$version" ] && [ -n "$out" ] || die "binary, profile, identity, team id, version and output directory are all required"
[ -x "$binary" ] || die "$binary isn't an executable file"
[ -f "$profile" ] || die "$profile isn't a file"
case $team in
    [A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9]) ;;
    *) die "the team id is ten capitals and digits" ;;
esac
case $version in
    *[!0-9.]* | .* | *. | *..*) die "the version is dotted digits, such as 0.47.0" ;;
esac

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# A profile for another App ID makes macOS kill the app at launch with no message.
security cms -D -i "$profile" > "$work/profile.plist" || die "can't read $profile"
granted=$("$plistbuddy" -c "Print :Entitlements:com.apple.application-identifier" "$work/profile.plist") ||
    die "$profile carries no application identifier"
[ "$granted" = "$team.$bundle_id" ] || die "the profile is for $granted, not $team.$bundle_id"

app="$out/Punktfunk Host.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
cp "$binary" "$app/Contents/MacOS/punktfunk-host"
cp "$profile" "$app/Contents/embedded.provisionprofile"
cp "$here/io.unom.punktfunk.host.plist" "$app/Contents/Library/LaunchAgents/$bundle_id.plist"
cp "$here/Info.plist" "$app/Contents/Info.plist"
"$plistbuddy" -c "Set :CFBundleShortVersionString $version" -c "Set :CFBundleVersion $version" \
    "$app/Contents/Info.plist"

cp "$here/host.entitlements" "$work/host.entitlements"
"$plistbuddy" -c "Set :com.apple.application-identifier $team.$bundle_id" \
    -c "Set :com.apple.developer.team-identifier $team" "$work/host.entitlements"

codesign --force --options runtime --sign "$identity" --entitlements "$work/host.entitlements" "$app"
codesign --verify --strict --verbose=2 "$app"
echo "$app"
