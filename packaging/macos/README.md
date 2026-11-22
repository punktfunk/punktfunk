# macOS host bundle

`Punktfunk Host.app`: the `punktfunk-host` binary in a signed app bundle. macOS ties the restricted
HID entitlement and every privacy grant to a signed bundle, so the host does not run as a bare
binary here. Apple silicon, macOS 15 or later.

| File | Role |
|---|---|
| `Info.plist` | Bundle id `io.unom.punktfunk.host`, no Dock icon, the permission prompt texts |
| `host.entitlements` | The entitlements the signature carries; the team id is filled at build |
| `io.unom.punktfunk.host.plist` | The LaunchAgent: starts `punktfunk-host serve` at login, restarts it after a crash |
| `bundle.sh` | Assembles, signs and verifies the bundle |

## Build

```sh
cargo build -p punktfunk-host --release
sh packaging/macos/bundle.sh \
  -b target/release/punktfunk-host \
  -p path/to/Punktfunk_Host.provisionprofile \
  -i "Apple Development" \
  -t <team id> \
  -v 0.47.0 \
  -o dist
```

Each option also reads an environment variable: `PF_BINARY`, `PF_PROFILE`, `PF_IDENTITY`,
`PF_TEAM_ID`, `PF_VERSION`, `PF_OUT`. The profile must be for `<team id>.io.unom.punktfunk.host`
and carry `com.apple.developer.hid.virtual.device`; the script refuses another one. If more than
one certificate matches the identity name, pass its hash.

The script prints the path of `dist/Punktfunk Host.app`. The binary reports the version it was
built with (`PUNKTFUNK_BUILD_VERSION`), so pass the same number as `-v`.

The bundle carries the LaunchAgent at `Contents/Library/LaunchAgents/`, for
`SMAppService.agent(plistName:)`. The script does not register it.

## Permissions

| Ask | Why | Prompt text |
|---|---|---|
| Screen recording | Capture the display | macOS's own |
| System audio recording | Send the Mac's sound to the player | `NSAudioCaptureUsageDescription` |
| Accessibility | Press the player's keys and move its mouse | macOS's own |
| Local network | Announce the host to the LAN and take connections | `NSLocalNetworkUsageDescription` |
| `com.apple.developer.hid.virtual.device` | Present the player's gamepad to the Mac's games | entitlement, no prompt |

No microphone, camera or file-folder key: the host reads none of them.

## Hardened runtime

The bundle is signed with `--options runtime` and carries no `com.apple.security.cs.*` exception.
The binary links only system libraries, loads no plug-in into its own process (the plugin runner
is a separate executable) and maps no executable memory. An exception needs a stated need beside
it in `host.entitlements`.

## Not here

An installer, notarization, the updater, and translating the usage strings in `Info.plist`.
