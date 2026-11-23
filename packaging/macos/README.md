# macOS host bundle

`Punktfunk Host.app` and its disk image. macOS ties the restricted HID entitlement and every
privacy grant to a signed bundle, so the host does not run as a bare binary here. Apple silicon,
macOS 15 or later.

| File | Role |
|---|---|
| `bundle.sh` | Assembles, signs and verifies the bundle |
| `fetch-bun.sh` | Downloads the bun pinned in `ci/bun.env` and checks its hash |
| `build-payloads.sh` | Builds the web console and the plugin runner for the bundle |
| `dmg.sh`, `dmg-settings.py`, `dmg-requirements.txt` | Builds, checks and signs the disk image with dmgbuild |
| `dmg-background.svg` | The image's window background; the PNGs beside it are its 1x and 2x renders |
| `notarize.sh` | Notarizes an app or an image, staples it, asks Gatekeeper |
| `Info.plist` | Bundle id `io.unom.punktfunk.host`, no Dock icon, Bonjour types |
| `host.entitlements` | The host's entitlements; the team id is filled at build |
| `bun.entitlements` | bun's: the two a JIT runtime needs |
| `io.unom.punktfunk.host.plist` | The LaunchAgent: starts `punktfunk-host serve` at login, restarts it after a crash |
| `AppIcon.icon` | The app icon (Icon Composer); `actool` compiles it at build |
| `HostUI/` | The menu bar item and permission setup: a SwiftPM library the host loads; `bundle.sh` builds it |

## Menu bar UI

`HostUI` builds `libPunktfunkHostUI.dylib`, which the host loads from `Contents/Frameworks/` and
runs on its main thread (C ABI in `HostUI/Sources/CPunktfunkHostUI/include/pf_host_ui.h`), and
its resource bundle, which goes to `Contents/Resources/`. `bundle.sh` generates its texts,
builds it and signs it before the app: under the hardened runtime the host loads only its own
team's code.

```sh
scripts/gen-l10n-hostui.sh
swift build -c release --package-path packaging/macos/HostUI
swift test --package-path packaging/macos/HostUI
```

`HostUIPreview --render <dir>` writes every screen as PNG, light and dark, English and German,
without showing anything. Run without `--render` it puts the menu bar item on screen.

## Build

```sh
cargo build -p punktfunk-host --release
bun=$(sh packaging/macos/fetch-bun.sh)
sh packaging/macos/build-payloads.sh -u "$bun" -o dist/payloads
sh packaging/macos/bundle.sh -b target/release/punktfunk-host -i - -v 0.48.0 -o dist \
  -u "$bun" -w dist/payloads
sh packaging/macos/dmg.sh -a "dist/Punktfunk Host.app" -v 0.48.0 -o dist
```

`bundle.sh` prints the app's path, `dmg.sh` the image's: `dist/Punktfunk-Host-0.48.0.dmg`. Each
`bundle.sh` option also reads an environment variable; the script's header lists them. Without
`-u`/`-w` the bundle has no web console and no plugin runner. Without `HostUI/` it has no menu
bar UI, and without Xcode no icon; the script says which. `dmg.sh` needs Python 3.10 or later
(`PF_PYTHON`) and keeps dmgbuild in `~/Library/Caches/punktfunk-packaging` (`PF_CACHE`).

The binary reports the version it was built with (`PUNKTFUNK_BUILD_VERSION`), so pass the same
number as `-v`. The LaunchAgent sits at `Contents/Library/LaunchAgents/`, for
`SMAppService.agent(plistName:)`; nothing registers it at build.

## Signing modes

| `-i` | Profile | For | Virtual gamepads |
|---|---|---|---|
| `"Developer ID Application"` | the host's Developer ID profile | releases; notarized | yes |
| `"Apple Development"` | the host's development profile | this team's own Macs | yes |
| `-` | none | VMs, contributors | refused by macOS |

The profile must be for `<team id>.io.unom.punktfunk.host` and carry
`com.apple.developer.hid.virtual.device`. `bundle.sh` refuses a signing certificate the profile
does not list: a Developer ID identity with a development profile is killed at launch. If two
certificates match the identity name, pass its hash. Only a notarized Developer ID build passes
Gatekeeper; `bundle.sh` reports `spctl` and goes on.

## Release order

1. `bundle.sh` with the Developer ID identity and profile.
2. `notarize.sh "Punktfunk Host.app"`: it uploads a zip of the app and staples the app.
3. `dmg.sh -i "Developer ID Application"`: builds the image around the stapled app and signs it.
4. `notarize.sh Punktfunk-Host-<version>.dmg`: staples the image.

`notarize.sh` takes `NOTARY_PROFILE` (from `xcrun notarytool store-credentials`), or
`ASC_API_KEY_PATH`, `ASC_API_KEY_ID` and `ASC_API_ISSUER_ID`. `.gitea/workflows/macos-host.yml`
runs these four steps on a `v*` tag, attaches the image to the release and publishes it to the
`punktfunk-host-macos` generic package (`<version>/`, `latest/`; a dispatch with `dmg`:
`canary/`). Any other push builds an ad hoc image.

## What the maintainer provides

- A Developer ID provisioning profile for the App ID `io.unom.punktfunk.host` with the HID
  virtual device capability (developer portal, Profiles, Developer ID). CI reads it from the
  `MACOS_HOST_PROFILE` secret, base64: `base64 -i <profile>`.
- The notary credentials: the `ASC_API_KEY_P8`, `ASC_API_KEY_ID` and `ASC_API_ISSUER_ID`
  secrets the Apple client's release uses.
- The Developer ID Application identity in the runner user's login keychain, as for
  `apple.yml`.

## Permissions

| Ask | Why | Prompt text |
|---|---|---|
| Screen recording | Capture the display | macOS's own |
| System audio recording | Send the Mac's sound to the player | `NSAudioCaptureUsageDescription` |
| Accessibility (macOS 27: Device Control and Data Access) | Press the player's keys and move its mouse | macOS's own |
| Local network | Announce the host to the LAN and take connections | `NSLocalNetworkUsageDescription` |
| `com.apple.developer.hid.virtual.device` | Present the player's gamepad to the Mac's games | entitlement, no prompt |

No microphone, camera or file-folder key: the host reads none of them. The two prompt texts are
the messages `apple_usage_host_audio_capture` and `apple_usage_host_local_network`
(`l10n/messages/apple`); `bundle.sh` writes the English into `Info.plist` and each shipped
translation into `<lang>.lproj/InfoPlist.strings`.

## Hardened runtime

Every executable is signed with `--options runtime`. The host carries no
`com.apple.security.cs.*` exception: it links only system libraries, maps no executable memory
and loads one library of ours, the UI, signed by the same team. bun, the one bundled JIT
runtime, carries `allow-jit` and `allow-unsigned-executable-memory` and keeps library
validation, so a plugin's native addon that this team did not sign does not load. An ad hoc
build lifts library validation for the host, since neither side has a team id.

## Not here

An updater.
