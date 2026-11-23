#!/bin/sh
# Notarize a Developer ID signed app or disk image, staple the ticket, check Gatekeeper takes it.
#
# An app goes to the notary service as a zip of itself, and the ticket is stapled to the app. A
# disk image goes as it is. On a rejection the script prints the notary log. Credentials, either:
#   NOTARY_PROFILE      a keychain profile from `xcrun notarytool store-credentials`
#   ASC_API_KEY_PATH, ASC_API_KEY_ID, ASC_API_ISSUER_ID
#                       an App Store Connect API key: the .p8 file, its id, its issuer
#
# Usage: sh packaging/macos/notarize.sh <app or dmg>
set -eu

die() {
    echo "notarize.sh: $*" >&2
    exit 1
}

target=${1:-}
[ -e "$target" ] || die "usage: notarize.sh <app or dmg>"
if [ -n "${NOTARY_PROFILE:-}" ]; then
    set -- --keychain-profile "$NOTARY_PROFILE"
elif [ -n "${ASC_API_KEY_PATH:-}" ]; then
    set -- --key "$ASC_API_KEY_PATH" --key-id "${ASC_API_KEY_ID:?}" --issuer "${ASC_API_ISSUER_ID:?}"
else
    die "no credentials: set NOTARY_PROFILE, or ASC_API_KEY_PATH, ASC_API_KEY_ID and ASC_API_ISSUER_ID"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
case $target in
    *.app | *.app/)
        upload=$work/app.zip
        ditto -c -k --keepParent "$target" "$upload"
        assess="-t exec"
        ;;
    *.dmg)
        upload=$target
        assess="-t open --context context:primary-signature"
        ;;
    *) die "$target is neither an .app nor a .dmg" ;;
esac

xcrun notarytool submit "$upload" --wait --output-format plist "$@" > "$work/result.plist"
id=$(plutil -extract id raw -o - "$work/result.plist")
status=$(plutil -extract status raw -o - "$work/result.plist")
if [ "$status" != Accepted ]; then
    xcrun notarytool log "$id" "$@" >&2 || true
    die "the notary service answered $status for $target (submission $id)"
fi
xcrun stapler staple "$target"
xcrun stapler validate "$target"
# shellcheck disable=SC2086 # $assess is a list of words
spctl -a -vv $assess "$target"
