#!/usr/bin/env bash
# Writes an unsigned .ipa for sideloading from a TestFlight archive.
#
#   sideload-ipa.sh ARCHIVE OUT.ipa BUNDLE_ID=PROFILE...
#
# Re-exports with the upload's manual signing, one App Store profile per bundle, then strips our
# signature: the device refuses an App Store profile outside the store, and a re-signer that keeps
# our entitlements fails. The sideload tool signs the result with the player's Apple ID.
# Warns and exits 0 when ARCHIVE is absent. Reads TEAM_ID, XCODE_DEV_DIR and RUNNER_TEMP.
set -euo pipefail
shopt -s nullglob

ARCHIVE=$1
mkdir -p "$(dirname "$2")"
OUT="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
shift 2
rm -f "$OUT"
if [ ! -d "$ARCHIVE" ]; then
  echo "::warning::$ARCHIVE not found — skipping $(basename "$OUT")"
  exit 0
fi

WORK="$RUNNER_TEMP/sideload-$(basename "$ARCHIVE" .xcarchive)"
rm -rf "$WORK" && mkdir -p "$WORK/side"
PROFILES=""
for pair in "$@"; do PROFILES="$PROFILES<key>${pair%%=*}</key><string>${pair#*=}</string>"; done
# destination=export writes the .ipa to -exportPath. No ASC key needed — no network.
cat > "$WORK/export.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>export</string>
  <key>teamID</key><string>$TEAM_ID</string>
  <key>signingStyle</key><string>manual</string>
  <key>signingCertificate</key><string>Apple Distribution</string>
  <key>provisioningProfiles</key><dict>$PROFILES</dict>
</dict>
</plist>
EOF
DEVELOPER_DIR="$XCODE_DEV_DIR" xcodebuild -exportArchive \
  -archivePath "$ARCHIVE" \
  -exportOptionsPlist "$WORK/export.plist" \
  -exportPath "$WORK/export"
ipas=("$WORK"/export/*.ipa)
[ ${#ipas[@]} -gt 0 ] || { echo "::warning::no .ipa was produced by export"; exit 0; }

# Payload only: Symbols/ and Signatures/ belong to the store upload.
(cd "$WORK/side" && unzip -q "${ipas[0]}" 'Payload/*')
for b in "$WORK"/side/Payload/*.app/PlugIns/*.appex "$WORK"/side/Payload/*.app; do
  codesign --remove-signature "$b"
  rm -rf "$b/_CodeSignature" "$b/embedded.mobileprovision"
done
(cd "$WORK/side" && zip -qry "$OUT" Payload)
echo "exported $OUT"
