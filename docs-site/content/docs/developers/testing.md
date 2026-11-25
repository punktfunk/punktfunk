---
title: Run and test
description: Stream from a source build to a client, then run the checks that prove a change — the workspace suite and the commands for each area.
---

Stream from a source build, then run the checks that prove a change before you push it.

## Run a host from source

| Setup | Needs | Exercises |
|---|---|---|
| [Synthetic host and the probe](#without-a-desktop) | Any OS. No GPU, no desktop. | The protocol: handshake, encryption, error correction, reassembly |
| [A real host and a real client](#on-a-linux-desktop) | A Linux desktop session | The whole pipeline, on glass |
| [The probe against a live host](#one-plane-at-a-time) | A running host | One plane: input, microphone, touch |

### Without a desktop

The [first run](/docs/developers#first-run) is this setup: the synthetic host speaks `punktfunk/2`
with generated frames, and `punktfunk-probe` byte-checks them. It runs anywhere the host
compiles, macOS included. `--allow-tofu` lets an unpaired client connect; leave it out to test
pairing.

### On a Linux desktop

Start the host inside the session, then, in a second terminal, open a pairing window:

```sh
cargo run -rp punktfunk-host -- serve
cargo run -rp punktfunk-host -- ctl pair arm       # prints the PIN
```

On the client box, pair and stream with the CLI (it starts `punktfunk-session` from beside itself):

```sh
cargo build --release -p punktfunk-cli -p punktfunk-client-session
target/release/punktfunk pair <host-ip>            # asks for the PIN
target/release/punktfunk launch <host-ip>
```

Any installed client pairs the same way. For the web console against this host, run
`bun run dev` in `web/`.

### One plane at a time

`punktfunk-probe` exercises single planes against a live host: `--input-test`, `--mic-test`,
`--touch-test`, `--rich-input-test`. Its flags are in
[clients/probe](https://git.unom.io/unom/punktfunk/src/branch/main/clients/probe/README.md).

## The workspace suite

The core of CI's `rust` job, on Linux with the
[workspace packages](/docs/developers/build-from-source#workspace-prerequisites):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked     # unit, loopback, proptest, C ABI harness
```

Use `--locked` as CI does, or a silent `Cargo.lock` update passes locally and fails CI. To run one
crate, `cargo test -p <package>`; package names are on the
[Code map](/docs/developers/code-map). Tests that need a GPU, a compositor or a display skip
themselves or are `#[ignore]`d.

Two traps:

- `punktfunk-core` and `punktfunk-ffi` alone build without their `quic` feature, which drops the
  connection tests. Use `cargo test -p punktfunk-core -p punktfunk-ffi --features quic`.
- `punktfunk-host` has no library target: pass test filters after `--`, never `--lib`.

## Checks by area

| You touched | Run |
|---|---|
| FEC or loss handling | `cargo run -p loss-harness` (loss with no network) |
| The C ABI | `bash crates/client/punktfunk-ffi/tests/c/run.sh` (links from C) |
| Scroll, on the wire | `cargo test -p punktfunk-core --features quic scroll` |
| Scroll, host mapping (any OS) | `cargo test -p pf-inject --lib scroll` |
| Touch and scroll in the presenter (macOS too) | `cargo test -p pf-presenter --no-default-features` |
| The console UI (macOS too, CPU raster) | `cargo test -p pf-console-ui` |
| Messages in `l10n/` | `cargo run -p pf-l10n --locked -- check` |
| A Linux GPU encoder | `cargo test -p pf-encode -p pf-encode-core --locked --features nvenc,vulkan-encode,pyrowave` |
| Vulkan code, on a box with a device and the validation layer | `sh scripts/vk-validation-tests.sh` |
| The host without GameStream | `cargo clippy -p punktfunk-host --no-default-features --features pyrowave --all-targets -- -D warnings` |
| The guided installer | `cargo test -p punktfunk-setup` and `sh scripts/ci/check-installer-behavior.sh` |
| A host setting | `cargo test -p pf-host-config docs_table_is_current` |
| Windows code, from another OS | `scripts/xcheck.sh` |
| A docs page | `sh scripts/ci/check-docs-links.sh` and `sh scripts/ci/check-docs-drift.sh` |

`scripts/xcheck.sh` type-checks and lints the Windows code of the capture, display and frame crates.
It needs the target installed: `rustup target add x86_64-pc-windows-msvc`. Linux-only code needs a
Linux box or container, because pipewire's build script needs the Linux library. Regenerating a
checked-in file is under [Generated files](/docs/developers/contributing#generated-files).

## JavaScript packages

Run `bun install` in the directory first. Build before typecheck: the build generates code the
typecheck imports.

| Directory | Command |
|---|---|
| `web/` | `bun run check && bun run build && bun run lint && bun run test` |
| `docs-site/` | `bun run build && bun run lint` |
| `sdk/` | `bun run typecheck && bun run test` |
| `plugin-kit/` | `bun run check && bun run typecheck && bun run test` |
| `clients/decky/` | `pnpm install && pnpm run test`, then `python3 scripts/test-backend.py` (Python 3.10+) |

## Apple client

Build `PunktfunkCore.xcframework` first
([Build from source](/docs/developers/build-from-source#apple-client)), then from `clients/apple`:

```sh
swift test                  # macOS units; tests that need a host skip without one
bash test-loopback.sh       # synthetic hosts on 127.0.0.1, then the integration tests
swift test --filter ScrollCaptureTests
xcodebuild -project Punktfunk.xcodeproj -scheme Punktfunk-iOS \
  -destination 'generic/platform=iOS Simulator' CODE_SIGNING_ALLOWED=NO build
```

The iOS and tvOS builds need those slices in the xcframework (`BUILD_IOS=1`, `BUILD_TVOS=1`). The
tvOS scheme is the same with `-scheme Punktfunk-tvOS`, `'generic/platform=tvOS Simulator'` and
`-skipMacroValidation -skipPackagePluginValidation`. `ScrollCaptureTests` needs an xcframework built
from the same tree, whose header defines `PUNKTFUNK_FEATURE_QUIC`; `build-xcframework.sh` produces
that.

## Android client

From `clients/android`, with the toolchain from
[Build from source](/docs/developers/build-from-source#android-client):

```sh
./gradlew :kit:cargoNdkClippy
./gradlew :kit:testDebugUnitTest :app:testDebugUnitTest -PexcludeScreenshots
```

## Windows drivers

From `packaging/windows/drivers`, as in
[Build from source](/docs/developers/build-from-source#windows-drivers):

```powershell
..\drivers-cargo.ps1 'test --locked -p pf-umdf-util'
..\drivers-cargo.ps1 'test --locked -p pf-vdisplay --lib'
..\drivers-cargo.ps1 'clippy --locked -p pf-umdf-util -p pf-xusb -p pf-gamepad -p pf-mouse -p pf-touchpad -p wdk-iddcx -p pf-vdisplay --all-targets -- -D warnings'
```
