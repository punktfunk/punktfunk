#!/bin/sh
# Assemble and sign "Punktfunk Host.app": the host, its menu bar UI, bun and the bun payloads.
#
# A restricted entitlement and every privacy grant attach to a signed bundle, so the host
# cannot run as a bare binary on macOS. This script builds that bundle; it installs nothing.
#
# Usage: sh packaging/macos/bundle.sh -b <binary> -i <identity> [-p <profile> -t <team id>] \
#          -v <version> -o <output dir> [-u <bun> -w <payloads>] [-l <ui dylib> [-r <ui resources>]]
#
# Each option falls back to an environment variable: PF_BINARY, PF_IDENTITY, PF_PROFILE,
# PF_TEAM_ID, PF_VERSION, PF_OUT, PF_BUN, PF_PAYLOADS, PF_UI_LIB, PF_UI_RESOURCES.
#   binary    the built punktfunk-host (arm64)
#   identity  "Developer ID Application" (release), "Apple Development", a certificate hash,
#             or - for an ad hoc signature (no profile, no virtual gamepads)
#   profile   a .provisionprofile for io.unom.punktfunk.host that carries
#             com.apple.developer.hid.virtual.device, of the identity's kind
#   team id   the ten-character Apple team id of the profile
#   version   dotted digits, such as 0.47.0; the binary's own version comes from its build
#   bun       fetch-bun.sh's output; payloads, build-payloads.sh's
#   ui        a prebuilt libPunktfunkHostUI.dylib and its SwiftPM resource bundle; without
#             them the script builds packaging/macos/HostUI when that package exists
set -eu

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
plistbuddy=/usr/libexec/PlistBuddy
bundle_id=io.unom.punktfunk.host
ui_name=PunktfunkHostUI

die() {
    echo "bundle.sh: $*" >&2
    exit 1
}
note() {
    echo "bundle.sh: $*" >&2
}

binary=${PF_BINARY:-} profile=${PF_PROFILE:-} identity=${PF_IDENTITY:-}
team=${PF_TEAM_ID:-} version=${PF_VERSION:-} out=${PF_OUT:-}
bun=${PF_BUN:-} payloads=${PF_PAYLOADS:-} ui_lib=${PF_UI_LIB:-} ui_res=${PF_UI_RESOURCES:-}
while getopts b:p:i:t:v:o:u:w:l:r: opt; do
    case $opt in
        b) binary=$OPTARG ;;
        p) profile=$OPTARG ;;
        i) identity=$OPTARG ;;
        t) team=$OPTARG ;;
        v) version=$OPTARG ;;
        o) out=$OPTARG ;;
        u) bun=$OPTARG ;;
        w) payloads=$OPTARG ;;
        l) ui_lib=$OPTARG ;;
        r) ui_res=$OPTARG ;;
        *) die "usage: see the header of packaging/macos/bundle.sh" ;;
    esac
done

[ -n "$binary" ] && [ -n "$identity" ] && [ -n "$version" ] && [ -n "$out" ] ||
    die "binary, identity, version and output directory are required"
[ -x "$binary" ] || die "$binary isn't an executable file"
case $version in
    *[!0-9.]* | .* | *. | *..*) die "the version is dotted digits, such as 0.47.0" ;;
esac
[ -z "$bun" ] || [ -x "$bun" ] || die "$bun isn't an executable file"
if [ -n "$payloads" ]; then
    [ -n "$bun" ] || die "the payloads run on bun: pass -u"
    [ -f "$payloads/web/.output/server/index.mjs" ] && [ -f "$payloads/scripting/runner-cli.js" ] ||
        die "$payloads isn't build-payloads.sh output"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Three modes, by identity. A Developer ID or development profile decides the other two; the
# signing certificate must be one the profile names, or macOS kills the app at launch.
if [ "$identity" = - ]; then
    mode=adhoc
    note "ad hoc signature: no profile, so macOS refuses this build's virtual gamepads"
else
    [ -n "$profile" ] && [ -n "$team" ] || die "a profile and a team id are required unless the identity is -"
    [ -f "$profile" ] || die "$profile isn't a file"
    case $team in
        [A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9][A-Z0-9]) ;;
        *) die "the team id is ten capitals and digits" ;;
    esac
    security cms -D -i "$profile" > "$work/profile.plist" 2>/dev/null || die "can't read $profile"
    granted=$("$plistbuddy" -c "Print :Entitlements:com.apple.application-identifier" "$work/profile.plist") ||
        die "$profile carries no application identifier"
    [ "$granted" = "$team.$bundle_id" ] || die "the profile is for $granted, not $team.$bundle_id"
    "$plistbuddy" -c "Print :Entitlements:com.apple.developer.hid.virtual.device" "$work/profile.plist" \
        >/dev/null 2>&1 || die "$profile doesn't carry com.apple.developer.hid.virtual.device"
    if [ "$("$plistbuddy" -c "Print :ProvisionsAllDevices" "$work/profile.plist" 2>/dev/null)" = true ]; then
        mode=devid
    else
        mode=development
    fi
fi

# A prebuilt UI wins; else the HostUI package, when it exists. Its texts are generated first.
if [ -z "$ui_lib" ] && [ -f "$here/HostUI/Package.swift" ]; then
    bash "$root/scripts/gen-l10n-hostui.sh" >&2 || die "the HostUI texts didn't generate"
    swift build -c release --arch arm64 --package-path "$here/HostUI" >&2 || die "the HostUI build failed"
    ui_bin=$(swift build -c release --arch arm64 --package-path "$here/HostUI" --show-bin-path)
    ui_lib=$ui_bin/lib$ui_name.dylib
    ui_res=$ui_bin/${ui_name}_$ui_name.bundle
fi
[ -z "$ui_lib" ] || [ -f "$ui_lib" ] || die "$ui_lib isn't a file"

app="$out/Punktfunk Host.app"
contents=$app/Contents
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Library/LaunchAgents" "$contents/Resources/licenses"
cp "$binary" "$contents/MacOS/punktfunk-host"
cp "$here/io.unom.punktfunk.host.plist" "$contents/Library/LaunchAgents/$bundle_id.plist"
cp "$here/Info.plist" "$contents/Info.plist"
"$plistbuddy" -c "Set :CFBundleShortVersionString $version" -c "Set :CFBundleVersion $version" \
    "$contents/Info.plist"
cp "$root/LICENSE-MIT" "$root/LICENSE-APACHE" "$root/THIRD-PARTY-NOTICES.txt" "$contents/Resources/licenses/"

# The permission prompts are messages: the source language goes into Info.plist, every other
# shipped language into its lproj. CFBundleLocalizations is the shipped list.
locales=$root/l10n/locales.json
source_lang=$(plutil -extract source raw -o - "$locales")
"$plistbuddy" -c "Add :CFBundleLocalizations array" "$contents/Info.plist"
i=0
while lang=$(plutil -extract "shipped.$i" raw -o - "$locales" 2>/dev/null); do
    "$plistbuddy" -c "Add :CFBundleLocalizations:$i string $lang" "$contents/Info.plist"
    i=$((i + 1))
    for prompt in NSAudioCaptureUsageDescription:apple_usage_host_audio_capture \
        NSLocalNetworkUsageDescription:apple_usage_host_local_network; do
        msg=${prompt#*:}
        text=$(plutil -extract "$msg" raw -o - "$root/l10n/messages/apple/$lang.json" 2>/dev/null) || {
            [ "$lang" != "$source_lang" ] || die "l10n/messages/apple/$lang.json has no $msg"
            continue
        }
        case $text in *[{}\\]*) die "$msg ($lang) holds a placeholder or an escape" ;; esac
        if [ "$lang" = "$source_lang" ]; then
            strings=$contents/Info.plist
        else
            strings=$contents/Resources/$lang.lproj/InfoPlist.strings
            mkdir -p "${strings%/*}"
            [ -f "$strings" ] || plutil -create xml1 "$strings"
        fi
        plutil -insert "${prompt%%:*}" -string "$text" "$strings"
    done
done
# The Apple client's icon, compiled under the name Info.plist asks for. Assets.car carries the
# layered icon for macOS 26, AppIcon.icns the flat one for macOS 15.
if xcrun -f actool >/dev/null 2>&1; then
    cp -R "$root/clients/apple/App/punktfunk_Logo.icon" "$work/AppIcon.icon"
    xcrun actool --compile "$contents/Resources" --output-partial-info-plist "$work/icon.plist" \
        --app-icon AppIcon --include-all-app-icons --enable-on-demand-resources NO \
        --development-region en --target-device mac --minimum-deployment-target 15.0 \
        --platform macosx "$work/AppIcon.icon" >/dev/null || die "the icon didn't compile"
else
    note "no actool (Xcode): the bundle has no icon"
fi

if [ -n "$bun" ]; then
    mkdir -p "$contents/Helpers"
    cp "$bun" "$contents/Helpers/bun"
fi
if [ -n "$payloads" ]; then
    cp -R "$payloads/web" "$payloads/scripting" "$contents/Resources/"
else
    note "no payloads: the bundle has no web console and no plugin runner"
fi
if [ -n "$ui_lib" ]; then
    mkdir -p "$contents/Frameworks"
    cp "$ui_lib" "$contents/Frameworks/lib$ui_name.dylib"
    [ -z "$ui_res" ] || [ ! -d "$ui_res" ] || cp -R "$ui_res" "$contents/Resources/"
else
    note "no $here/HostUI package: the bundle has no menu bar UI"
fi

# Inside out, never --deep: bun, the UI, then the bundle and the host it names.
case $mode in
    devid) stamp=--timestamp ;;
    *) stamp=--timestamp=none ;;
esac
sign() {
    codesign --force --options runtime "$stamp" --sign "$identity" "$@"
}
[ -z "$bun" ] || sign --entitlements "$here/bun.entitlements" "$contents/Helpers/bun"
[ -z "$ui_lib" ] || sign "$contents/Frameworks/lib$ui_name.dylib"
if [ "$mode" = adhoc ]; then
    # No team id on either side, and the runtime's library validation wants them equal.
    "$plistbuddy" -c "Add :com.apple.security.cs.disable-library-validation bool true" "$work/adhoc.entitlements" >/dev/null
    sign --entitlements "$work/adhoc.entitlements" "$app"
else
    cp "$profile" "$contents/embedded.provisionprofile"
    cp "$here/host.entitlements" "$work/host.entitlements"
    "$plistbuddy" -c "Set :com.apple.application-identifier $team.$bundle_id" \
        -c "Set :com.apple.developer.team-identifier $team" "$work/host.entitlements"
    sign --entitlements "$work/host.entitlements" "$app"
    # The leaf certificate must be one of the profile's, byte for byte.
    codesign -d --extract-certificates="$work/cert" "$app" 2>/dev/null
    leaf=$(base64 < "$work/cert0" | tr -d '\n')
    i=0 found=''
    while cert=$(plutil -extract "DeveloperCertificates.$i" raw -o - "$work/profile.plist" 2>/dev/null); do
        [ "$cert" = "$leaf" ] && found=1
        i=$((i + 1))
    done
    [ -n "$found" ] || {
        rm -rf "$app"
        die "the $identity certificate isn't in $profile: a $mode profile takes a matching identity"
    }
fi
codesign --verify --strict --verbose=2 "$app"
spctl -a -vv -t exec "$app" || note "Gatekeeper passes only a notarized Developer ID build; this one is $mode"
echo "$app"
