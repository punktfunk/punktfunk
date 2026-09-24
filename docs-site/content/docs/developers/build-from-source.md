---
title: Build from source
description: Compile the Linux host when no package fits your release, and build every other part of Punktfunk from a clone.
---

Compile the Linux host yourself, or build a client, the web console, the docs site or the Windows
drivers from a clone.

For the host, the package repos are the supported path ([Install the host](/docs/install)). Build
from source when no package fits your release, or to work on it. A source build gets no packaged
service and no updates: you set up the service yourself.

Rust comes from [rustup](https://rustup.rs). `rust-toolchain.toml` pins the version, and rustup
installs it on the first build:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Linux host

Build with the `nvenc` and `vulkan-encode` features, as the packages do. Without `nvenc` an NVIDIA
box has no hardware encoder; without `vulkan-encode` AMD and Intel fall back to VAAPI, which needs a
keyframe to recover from every lost frame.

### Ubuntu / Debian

```sh
sudo apt install build-essential clang libclang-dev pkg-config cmake git curl \
  libpipewire-0.3-dev libopus-dev libwayland-dev libxkbcommon-dev libdrm-dev \
  libgl-dev libegl-dev libgbm-dev
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release --locked -p punktfunk-host -p punktfunk-encode-worker \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode
```

### Fedora

```sh
sudo dnf install gcc gcc-c++ clang clang-devel cmake nasm git pkgconf-pkg-config \
  pipewire-devel wayland-devel libxkbcommon-devel opus-devel \
  mesa-libGL-devel mesa-libgbm-devel
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk
cargo build --release --locked -p punktfunk-host -p punktfunk-encode-worker \
  --features punktfunk-host/nvenc,punktfunk-host/vulkan-encode
```

Without `mesa-libGL-devel` the link fails with `cannot find -lGL`. To build an RPM instead, use the
CI toolchain: `docker build --build-arg FEDORA_VERSION=NN -f ci/fedora-rpm.Dockerfile -t pf-rpm ci`,
then run `packaging/rpm/build-rpm.sh` inside it.

### Arch (PKGBUILD)

The split `PKGBUILD` in `packaging/arch/` builds `punktfunk-host` and `punktfunk-client`. Set
`PF_WITH_WEB=1` to also build `punktfunk-web` and `PF_WITH_SCRIPTING=1` for `punktfunk-scripting`
(both need `bun`):

```sh
git clone https://git.unom.io/unom/punktfunk.git && cd punktfunk/packaging/arch
PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver   # builds the working tree
sudo pacman -U punktfunk-host-*.pkg.tar.zst
```

NVENC and EGL come from `nvidia-utils`. On a builder without a GPU, link the CUDA stub first (the
`PKGBUILD` header says how). Packager notes:
[packaging/arch](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/arch/README.md). For
a SteamOS host use the [on-device installer](/docs/steamos-host) instead; it builds against the
running OS.

### Running what you built

The box also needs what [Requirements](/docs/requirements) lists for your GPU. Run the host from
inside your desktop session; it detects the compositor:

```sh
target/release/punktfunk-host serve              # native clients only
target/release/punktfunk-host serve --gamestream # plus Moonlight (trusted LAN only)
```

The host finds `punktfunk-encode-worker` beside itself and runs PyroWave there at raised GPU
priority. Grant it the capability the packages grant:
`sudo setcap cap_sys_nice=ep target/release/punktfunk-encode-worker`.

To run the host as a user service from a clone at `~/punktfunk`, copy
`scripts/punktfunk-host.service` to `~/.config/systemd/user/`, then:

```sh
systemctl --user daemon-reload && systemctl --user enable --now punktfunk-host
```

[Running as a service](/docs/running-as-a-service) covers the rest.

## Workspace prerequisites

The whole Cargo workspace, clients and tools included, builds on Ubuntu 26.04 with the packages CI
installs (`ci/rust-ci.Dockerfile`):

```sh
sudo apt install build-essential clang libclang-dev pkg-config cmake git curl \
  libpipewire-0.3-dev libopus-dev libwayland-dev libxkbcommon-dev \
  libgl-dev libegl-dev libgbm-dev libgtk-4-dev libadwaita-1-dev libsdl3-dev
```

The first build downloads a prebuilt Skia (about 19 MB) for the console UI.

## Linux client

```sh
cargo build --release -p punktfunk-client-linux -p punktfunk-client-session -p punktfunk-cli
target/release/punktfunk-client
```

It needs GTK 4.16+, libadwaita 1.5+ and SDL3. The shell starts `punktfunk-session` from beside
itself, so build both. More:
[clients/linux](https://git.unom.io/unom/punktfunk/src/branch/main/clients/linux/README.md).

## Windows host and client

Install the MSVC toolchain, CMake and LLVM (libclang). In PowerShell, keep the target path short
and let CMake 4 build the vendored libraries:

```powershell
$env:CARGO_TARGET_DIR = 'C:\t'; $env:CMAKE_POLICY_VERSION_MINIMUM = '3.5'
cargo build --release -p punktfunk-host --features nvenc,qsv
cargo build --release -p punktfunk-tray
cargo build --release -p punktfunk-client-windows -p punktfunk-client-session -p punktfunk-cli
```

`CARGO_HOME` must be an ASCII path; SDL3's build breaks otherwise. Pack the host installer with
`pwsh -File packaging\windows\pack-host-installer.ps1 -Version 0.0.0-dev -TargetDir C:\t\release`
(`-NoDriver` skips the driver build). More:
[packaging/windows](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/README.md),
[clients/windows](https://git.unom.io/unom/punktfunk/src/branch/main/clients/windows/README.md).

## Windows drivers

The drivers are a separate workspace that needs the WDK. `scripts/ci/ensure-windows-toolchain.ps1`
installs it. From an MSVC developer shell:

```powershell
cd packaging\windows\drivers
$env:Version_Number = '10.0.26100.0'
..\drivers-cargo.ps1 'build --release --locked'
```

`drivers-cargo.ps1` runs cargo through a short drive letter, because the CMake builds inside
overflow `MAX_PATH`. To install a build on a test box, run `deploy-dev.ps1 -Install` from an
elevated shell after stopping the host (`punktfunk-host service stop`). Signing and the dev loop:
[packaging/windows](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/README.md).

## Apple client

On a Mac with Xcode 26.5:

```sh
rustup target add aarch64-apple-darwin
bash scripts/build-xcframework.sh     # BUILD_IOS=1 / BUILD_TVOS=1 add those slices
cd clients/apple && open Punktfunk.xcodeproj
```

tvOS slices build on a pinned nightly with `rust-src`; the script names it. More:
[clients/apple](https://git.unom.io/unom/punktfunk/src/branch/main/clients/apple/README.md).

## Android client

Install the Android SDK with NDK r30 (`30.0.14904198`), `cmake;3.22.1`, JDK 21 and
[cargo-ndk](https://github.com/bbqsrc/cargo-ndk):

```sh
cargo install cargo-ndk
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cd clients/android && ./gradlew :app:assembleDebug
```

The APK lands in `app/build/outputs/apk/debug/`. More:
[clients/android](https://git.unom.io/unom/punktfunk/src/branch/main/clients/android/README.md).

## Steam Deck plugin

```sh
cd clients/decky
pnpm install && pnpm run package                # → out/punktfunk-v<ver>.zip
DECK=deck@<deck-ip> pnpm run deploy              # install on a Deck and restart the loader
```

More: [clients/decky](https://git.unom.io/unom/punktfunk/src/branch/main/clients/decky/README.md).

## Web console, runner and docs site

All three run on [Bun](https://bun.sh). Run `bun install` in the directory first.

| In | Run | You get |
|---|---|---|
| `web/` | `bun run dev` | The console on `http://localhost:47992`, talking to the host on `127.0.0.1:47990` |
| `web/` | `bun run build`, then `bun run start` | The production server from `.output/` |
| `sdk/` | `bun src/runner-cli.ts` | The runner with your scripts and installed plugins (`--list` shows them) |
| `docs-site/` | `bun run dev` | This site on `http://localhost:3001/docs` |

`PUNKTFUNK_MGMT_URL` points the console's dev server at another host. More:
[web](https://git.unom.io/unom/punktfunk/src/branch/main/web/README.md),
[sdk](https://git.unom.io/unom/punktfunk/src/branch/main/sdk/README.md),
[docs-site](https://git.unom.io/unom/punktfunk/src/branch/main/docs-site/README.md).

## Client builds on 32-bit ARM

A client that embeds `punktfunk-core` can add the `chacha-aws-lc-rs` feature to cut streaming CPU
on 32-bit ARM devices such as webOS TVs. The client still has to ask for ChaCha20 when it connects;
hosts need no change.
