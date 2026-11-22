#!/usr/bin/env bash
# Write the macOS host UI's string tables and L10n.swift from l10n/messages into
# packaging/macos/HostUI/Sources/PunktfunkHostUI/Generated, which git ignores. Run it before
# `swift build` or `swift test` in packaging/macos/HostUI.
#
# The catalog is compiled here to .lproj tables, which every SwiftPM copies into the resource
# bundle; not every toolchain compiles a String Catalog. L10n.swift looks the bundle up through
# HostUIResources: SwiftPM's Bundle.module looks beside the app bundle, never in Resources/.
set -euo pipefail
cd "$(dirname "$0")/.."
out=packaging/macos/HostUI/Sources/PunktfunkHostUI/Generated
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cargo run -q -p pf-l10n -- apple --root "$PWD/l10n" --out "$tmp" --parts common,tools
rm -rf "$out"
mkdir -p "$out/Strings"
xcrun xcstringstool compile "$tmp/Localizable.xcstrings" --output-directory "$out/Strings"
sed -e 's/Bundle\.module\.bundleURL/HostUIResources.bundleURL/' \
    -e 's/^public enum L10n/enum L10n/' -e 's/^    public static /    static /' \
    "$tmp/L10n.swift" >"$out/L10n.swift"
if grep -q 'Bundle\.module' "$out/L10n.swift"; then
    echo "gen-l10n-hostui.sh: L10n.swift still names Bundle.module; update the sed above" >&2
    exit 1
fi
