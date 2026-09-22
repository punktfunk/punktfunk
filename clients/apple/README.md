# punktfunk — Apple client (macOS · iOS · iPadOS · tvOS)

One SwiftUI codebase for Mac, iPhone, iPad and Apple TV. Networking and protocol — QUIC control
plane, UDP data plane, FEC, AES-GCM, Opus, cert pinning — are the shared Rust `punktfunk-core`,
statically linked as `PunktfunkCore.xcframework`; this package is the Swift half: decode, present,
input, UI.

`PunktfunkKit` is the reusable library (the C-ABI wrapper, the two presenters, input and gamepad
capture, discovery); `PunktfunkClient` is the app, including the gamepad UI a connected controller
swaps the whole home for. What the app can do for a user is
[the docs site](https://docs.punktfunk.unom.io/docs/install-client)'s job, not this file's.

## Build, run, test

Xcode 26.5 / Swift 6.3. Build the Rust core into an xcframework first:

```sh
rustup target add aarch64-apple-darwin
bash scripts/build-xcframework.sh     # → clients/apple/PunktfunkCore.xcframework
#   BUILD_IOS=1 / BUILD_TVOS=1 add those slices

cd clients/apple
open Punktfunk.xcodeproj              # the real app: ⌘R
swift run PunktfunkClient             # or the unbundled dev shell
swift build && swift test             # units + loopback/remote (self-skip without a host)
```

tvOS slices are tier-3 Rust targets built from source:
`rustup toolchain install nightly && rustup component add rust-src --toolchain nightly`.

Against a host:

```sh
bash test-loopback.sh                 # builds punktfunk-host synthetic-source and byte-verifies frames
PUNKTFUNK_REMOTE_HOST=<ip> swift test --filter RemoteFirstLightTests
PUNKTFUNK_AUTOCONNECT=<ip> PUNKTFUNK_MODE=1280x720x60 swift run PunktfunkClient
```

## Traps

- **Entitlements.** The macOS target is App-Sandboxed and needs `network.server` — the raw-UDP plane
  and quinn both `bind()`. iOS/tvOS share an entitlements file; keep `app-sandbox` out of it. Verify
  with `codesign -d --entitlements :- <built .app>`.
- **Decode flow.** Every stream opens with an IDR carrying VPS/SPS/PPS in-band, and recovery
  keyframes re-send them. Refresh the format description on every IDR — there is no out-of-band
  extradata, ever.
- **ABI threading.** One video pump thread per connection, plus optional audio and feedback drain
  threads. `send()` is enqueue-only and safe alongside all of them; the wrapper's per-plane locks
  make `close()` safe from anywhere.
- **DualSense motion scale** (`GamepadWire`) comes from hid-playstation's math and has not been
  live-verified. If gyro or accel feels wrong, fix the sign/scale there and `evtest` the host's
  virtual pad.
- **The gamepad backdrop is pure SwiftUI** on purpose: a `.metal` library only bundles reliably in
  one of the two build systems these sources compile under.
- **Screenshots are automated.** `tools/screenshots.sh all` renders the real UI at App Store pixel
  sizes through a DEBUG-only shot mode; CI captures the iOS set on every main push. `ShotMock` seeds
  the data so a capture never browses the real LAN — a stranger's hostname reached the live listing
  that way once.
