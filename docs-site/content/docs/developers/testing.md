---
title: Testing
description: Prove a change before you push it — the workspace suite, the per-area checks, and a live host from source against a client.
---

The commands that prove a change: the workspace suite, the checks for each area, and a host built
from source streaming to a client.

## The workspace suite

The core of CI's `rust` job, on Linux with the
[workspace packages](/docs/developers/build-from-source#workspace-prerequisites):

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked     # unit, loopback, proptest, C ABI harness
```

Use `--locked` as CI does, or a silent `Cargo.lock` update passes locally and fails CI. To run one
crate, `cargo test -p <package>`; package names are in
[Architecture](/docs/developers/architecture#where-the-code-lives). Tests that need a GPU, a
compositor or a display skip themselves or are `#[ignore]`d.

Two traps:

- `punktfunk-core` alone builds without its `quic` feature, which drops the C ABI tests. Use
  `cargo test -p punktfunk-core --features quic`.
- `punktfunk-host` has no library target: pass test filters after `--`, never `--lib`.

## Per-area checks

| Area | Command |
|---|---|
| FEC under loss, no network | `cargo run -p loss-harness` |
| C ABI links from C | `bash crates/punktfunk-core/tests/c/run.sh` |
| Scroll, wire and compatibility | `cargo test -p punktfunk-core --features quic scroll` |
| Scroll, host mapping (any OS) | `cargo test -p pf-inject --lib scroll` |
| Touch and scroll in the presenter (macOS too) | `cargo test -p pf-presenter --no-default-features` |
| Linux GPU encode backends | `cargo test -p pf-encode --locked --features nvenc,vulkan-encode,pyrowave` |
| Host without GameStream | `cargo clippy -p punktfunk-host --no-default-features --features pyrowave --all-targets -- -D warnings` |
| Guided installer | `cargo test -p punktfunk-setup` and `sh scripts/ci/check-installer-behavior.sh` |
| Settings table in the docs | `cargo test -p pf-host-config docs_table_is_current` |
| Windows or Linux code from another OS | `scripts/xcheck.sh windows` or `scripts/xcheck.sh linux` |

`scripts/xcheck.sh` type-checks and lints the `#[cfg(target_os = …)]` code of the capture, display
and frame crates in about a second. It needs the target installed:
`rustup target add x86_64-pc-windows-msvc` (or `x86_64-unknown-linux-gnu`).

A settings change regenerates the table in [Configuration](/docs/configuration) with
`UPDATE_SETTINGS_DOCS=1 cargo test -p pf-host-config docs_table_is_current`.

## JavaScript packages

Run `bun install` in the directory first. Build before typecheck: the build generates code the
typecheck imports.

| Directory | Command |
|---|---|
| `web/` | `bun run check && bun run build && bun run lint && bun run test` |
| `docs-site/` | `bun run build && bun run lint` |
| `sdk/` | `bun run typecheck && bun run test` |
| `plugin-kit/` | `bun run check && bun run typecheck && bun run test` |
| `clients/decky/` | `pnpm install && pnpm run typecheck`, then `python3.13 scripts/test-backend.py` |

For a docs change, also run `sh scripts/ci/check-docs-links.sh` and
`sh scripts/ci/check-docs-drift.sh` from the repo root.

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

The tvOS scheme is the same with `-scheme Punktfunk-tvOS` and `'generic/platform=tvOS Simulator'`.
`ScrollCaptureTests` needs an xcframework built from the same tree, whose header defines
`PUNKTFUNK_FEATURE_QUIC`; `build-xcframework.sh` produces that.

## Android client

From `clients/android`, with the toolchain from
[Build from source](/docs/developers/build-from-source#android-client):

```sh
./gradlew :kit:testDebugUnitTest :app:testDebugUnitTest -PexcludeScreenshots
```

## Windows drivers

From `packaging/windows/drivers`, as in
[Build from source](/docs/developers/build-from-source#windows-drivers):

```powershell
..\drivers-cargo.ps1 'test --locked -p pf-umdf-util'
..\drivers-cargo.ps1 'clippy --locked -p pf-umdf-util -p pf-xusb -p pf-gamepad -p pf-mouse -p wdk-iddcx -p pf-vdisplay --all-targets -- -D warnings'
```

## Run a host from source against a client

**Without a desktop.** The synthetic host speaks `punktfunk/1` with generated frames and runs
anywhere the host compiles, macOS included. In two terminals, start it and connect the probe,
which byte-checks the frames:

```sh
cargo run -rp punktfunk-host -- punktfunk1-host --source synthetic --allow-tofu
cargo run -rp punktfunk-probe -- --connect 127.0.0.1:9777
```

`--allow-tofu` lets an unpaired client connect; leave it out to test pairing.

**On a Linux desktop.** Start the host inside the session, then, in a second terminal, open a
pairing window:

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

Any installed client pairs the same way. `punktfunk-probe` exercises single planes against a live
host: `--input-test`, `--mic-test`, `--touch-test`, `--rich-input-test`. Its flags are in
[clients/probe](https://git.unom.io/unom/punktfunk/src/branch/main/clients/probe/README.md). For
the web console against this host, run `bun run dev` in `web/`.
