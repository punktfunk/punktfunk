# punktfunk client — Flatpak (Steam Deck / SteamOS, and any flatpak distro)

The native Linux **client** — the shell (crate `punktfunk-client-linux`, binary
`punktfunk-client`) plus the Vulkan session binary it execs for streaming (crate
`punktfunk-client-session`, binary `punktfunk-session`) — is
published two ways by CI (`.gitea/workflows/flatpak.yml`), on every push to `main` (a rolling
`<next-minor>-ciN.g<sha>` build, base derived from the latest stable tag by
`scripts/ci/pf-version.sh`) and on `v*` tags (a clean `X.Y.Z`):

1. **Hosted OSTree repo at `https://flatpak.unom.io`** (recommended) — a GPG-signed Flatpak
   remote served by a static Caddy container on unom-1, so users **install once and then
   `flatpak update`**. Shared unom-wide repo (remote name `unom`), reusable by other unom apps
   under the same signing key. See "Install (recommended)" below.
2. **Single-file `.flatpak` bundle** in **Gitea's generic package registry** (`unom` org) — the
   no-remote fallback the **Decky plugin** consumes (stable `latest/punktfunk-client.flatpak`
   URL) and the offline/manual path. On tags it's also attached to the Gitea release.

> The **host** is NOT a flatpak (it needs unsandboxed `/dev/uinput` + zero-copy NVENC — see
> [`../README.md`](../README.md) "Why not Flatpak"). Only the **client** is sandbox-friendly.

## Why flatpak for the Steam Deck

SteamOS `/usr` is read-only and image-based, and the system is **missing `libadwaita` and
`libSDL3`** — so a bare `punktfunk-client` binary dropped into `~/.local/bin` won't run. Flatpak
is the Deck's native, update-survivable app path (the user already runs Moonlight and chiaki-ng
as flatpaks), and the bundle carries libadwaita (from `org.gnome.Platform//50`) + a bundled SDL3.
It carries no FFmpeg: since M10 the client decodes on the user's own GPU drivers (Vulkan Video,
VAAPI) with openh264 + rav1d as the CPU floor, so the runtime's `codecs-extra` extension — and the
encumbered-codec question it answered — no longer enter into it.

App id: **`io.unom.Punktfunk`** (matches the Apple bundle id family and the Decky plugin's
flatpak fallback).

## Install

On the [docs site](https://docs.punktfunk.unom.io/docs/install-client#linux-desktop-flatpak) — the
hosted repo is the supported path, with a single-file bundle as the no-remote fallback for a Deck
that cannot reach it. Packager note: the bundle install has no remote, so it cannot self-update;
re-running the install with a newer bundle is the upgrade.

## Build locally / the CI fallback

CI builds this in a **`--privileged`** Fedora container, because `flatpak-builder` runs
`bubblewrap`, which needs user namespaces the default Docker executor denies. **If the Gitea
runner can't grant `--privileged`** (the job fails at `flatpak-builder` with
*"Creating new namespace failed: Operation not permitted"*), build it out-of-band and upload
by hand. The easiest place is **on the Deck itself** (it can run `org.flatpak.Builder`
user-scope, no root):

```sh
# On the Deck (or any flatpak box), one-time:
flatpak install --user -y flathub org.flatpak.Builder

# build-flatpak.sh auto-detects org.flatpak.Builder, generates cargo-sources.json (or reuses an
# existing one — see below), builds, and exports dist/punktfunk-client-<version>.flatpak:
bash packaging/flatpak/build-flatpak.sh

# Upload to the generic registry (PAT with write:package):
curl -fsS --user "enricobuehler:$REGISTRY_TOKEN" \
  --upload-file dist/punktfunk-client-*.flatpak \
  "https://git.unom.io/api/packages/unom/generic/punktfunk-client-flatpak/0.0.1-manual/punktfunk-client.flatpak"
```

> `cargo-sources.json` generation needs `python3` + `aiohttp` + `tomlkit`, which the Deck lacks.
> Generate it on a dev box (`build-flatpak.sh` does it, or run the upstream
> `flatpak-cargo-generator.py Cargo.lock -o packaging/flatpak/cargo-sources.json`), rsync it next
> to the manifest, and `build-flatpak.sh` reuses it (it only regenerates when the file is absent
> or `FORCE_GEN=1`).

> The Mac build host **cannot** build a Linux flatpak (no flatpak-builder for macOS), and
> home-worker-2 has no flatpak and no passwordless sudo to install it — so the Deck or the
> privileged CI container are the only two viable build sites.

### aarch64

The manifest builds for aarch64 as well as x86_64. Two things are architecture-specific, and both
are now expressed properly rather than hardcoded:

* **`PKG_CONFIG_PATH`** contains the runtime's multiarch directory. flatpak-builder does *not*
  shell-expand `env` values, so `${FLATPAK_ARCH}` would be taken literally — a `build-options.arch`
  override supplies the aarch64 string instead, inheriting everything else.
* **The prebuilt Skia archive** is per-target and pinned by sha256. There are now two `type: file`
  sources discriminated by `only-arches`, both landing on the same `dest-filename`, so
  `SKIA_BINARIES_URL` stays one literal path. Upstream publishes the aarch64 archive under the
  same skia commit hash and the same resolved-feature key (at 0.99:
  `jpegd-jpege-pdf-textlayout-vulkan`), so on a skia-safe bump update both URLs and both hashes
  together. The feature key is **not** stable across bumps — 0.87 was `pdf-textlayout-vulkan`;
  `jpeg` entering skia-safe's defaults at 0.99 renamed it.

```sh
ARCH=aarch64 bash packaging/flatpak/build-flatpak.sh
# -> dist/punktfunk-client-<version>-aarch64.flatpak
```

`ARCH` defaults to this machine's, and the bundle name now carries the architecture so an x86_64
and an aarch64 build can coexist in `dist/`. This is **not** a cross-compile: flatpak-builder runs
the build in a sandbox for the target arch, so building aarch64 anywhere but an arm64 machine
needs qemu binfmt and is very slow.

CI builds aarch64 as a second leg of `flatpak.yml`, on a runner labelled `linux-arm64`, in the
image `punktfunk-flatpak-ci-arm64` (a `docker.yml` `builders` row that runs on that runner). Both legs publish
into the same OSTree repo, so the `.flatpakref` installs the right arch on its own. The bundle is
`punktfunk-client-<version>-aarch64.flatpak`, and the alias is `<channel>/punktfunk-client-aarch64.flatpak`.
The leg is `continue-on-error` and never holds back x86_64.

The runner is a VM on the macOS CI Mac mini (unom/infra `runners/home-runner-arm-1`). It sits on
the LAN, so it pulls the builder from the LAN registry and uses ci-core's cache like the amd64 fleet.

## Manifest

[`io.unom.Punktfunk.yml`](io.unom.Punktfunk.yml). Runtime `org.gnome.Platform//50`
(GTK 4.20 + libadwaita 1.8 ≥ the crate floors of v4_16 / v1_5), built on freedesktop-sdk 25.08,
with two build-time SDK extensions: `org.freedesktop.Sdk.Extension.rust-stable` (→ //25.08,
**rustc 1.96** — the GTK4 dep chain, e.g. pango-sys 0.22, needs ≥ 1.92, which the EOL GNOME-48 /
24.08 rust-stable at 1.89 could not provide) and `org.freedesktop.Sdk.Extension.llvm20` (libclang,
needed by bindgen in sdl3-sys / pyrowave-sys). **No libavcodec at any layer** — the client links no
FFmpeg since M10, so neither the SDK's stripped build nor the runtime's `codecs-extra` shadow of it
is involved; HEVC decodes on the GPU's own driver, and there is deliberately no software HEVC
rung (see the manifest header). A bundled
**SDL3 3.4.10** module (pinned to match `sdl3-sys 0.6.6+SDL-3.4.10`), and finish-args for Wayland +
`--device=all` (GPU/VAAPI render node + evdev + the hidraw char-devices SDL3 needs for DualSense)
+ `--socket=pulseaudio` (PipeWire-pulse: playback + mic) + `--share=network`. Alongside it:
`io.unom.Punktfunk.desktop`, `io.unom.Punktfunk.metainfo.xml`, `io.unom.Punktfunk.svg` (all
installed by the manifest). No `vulkan-headers` module: it existed for `pf-ffvk`'s bindgen over
FFmpeg's `hwcontext_vulkan.h`, and ash generates its own bindings and dlopens the loader.
`cargo-sources.json` (the offline crate cache) is a pure function of
`Cargo.lock`; CI regenerates it each build and it is **gitignored** — generate it on any box with
network + `python3`/`aiohttp`/`tomlkit` (`build-flatpak.sh` does this automatically) and, for a
build host that lacks those (the Deck), rsync the generated file in alongside the manifest.

**Offline Skia:** the session binary's Skia console UI (`pf-console-ui` → `skia-safe`) normally
downloads prebuilt `libskia` binaries at build time, which is dead in the offline sandbox — so the
manifest pins a `skia-binaries-….tar.gz` source and points the build at it with
`SKIA_BINARIES_URL: file://…`. When bumping the `skia-safe`/`skia-bindings` crate version, update
that pinned tarball (URL + sha256) to the matching `skia-binaries` release **in the same commit**,
or the build breaks offline.

The failure is not a download error, because `file://` always succeeds — skia-bindings unpacks the
stale archive verbatim, *including the `bindings.rs` it was generated with*, so the new crate's
`src/defaults.rs` compiles against old bindings and the leg dies on missing associated consts
(`SkPathFillType::Default`, `SkPathDirection::Default` — they live in the generated `bindings.rs`,
so they travel with the archive, not the crate). Everything else in the offline chain is derived
from `Cargo.lock` and self-corrects; this tarball is the only hand-maintained pin, and the
0.87 → 0.99 bump (#193) left it behind.

## Hosting the repo (unom-1) + one-time setup

The OSTree repo flatpak-builder produces is GPG-signed in CI and rsynced to unom-1, where a tiny
static **Caddy container** (`server/compose.production.yml` + `server/Caddyfile`, port **3230**)
serves the `./site` tree (`repo/` + `unom.flatpakrepo` + `io.unom.Punktfunk.flatpakref` +
`index.html`). The edge Caddy on home-reverse-proxy-1 fronts it at `https://flatpak.unom.io`.
The CI deploy step **no-ops until the secret + infra exist**, so it won't redden builds mid-setup.

**Signing key:** dedicated RSA-4096 key `unom Flatpak Repo <flatpak@unom.io>`. Public key committed
at [`unom-flatpak.gpg`](unom-flatpak.gpg) (its base64 goes into the `.flatpakrepo`/`.flatpakref`
`GPGKey=`); private key (ASCII-armored, then base64) lives only in the CI secret.

One-time setup (mirrors any new unom DMZ service — see the deploy-infra notes):

1. **Secret** `FLATPAK_GPG_PRIVATE_KEY` on this repo = base64 of the armored private key
   (`gpg --armor --export-secret-keys <fpr> | base64 -w0`). `DEPLOY_*` + `REGISTRY_TOKEN` already exist.
   Also **`DEPLOY_KNOWN_HOSTS`** — unom-1's SSH host key, `ssh-keyscan -p "$DEPLOY_PORT"
   "$DEPLOY_HOST"` — which the deploy `ssh`es against with `StrictHostKeyChecking=yes`. Every run
   starts with an empty `known_hosts`, so `accept-new` made *every* run a first contact, handing the
   deploy key and the GPG-signed repo to whatever won the race for the address. It gates the step
   like the others: until it is set the deploy skips with a warning. Same secret and same host as
   the Nix cache publish (see `packaging/nix/README.md`), so configuring either satisfies both.
2. **Edge Caddy** on home-reverse-proxy-1 (`/home/caddy/caddy/Caddyfile`, apply by hand + `./reload.sh`):
   `flatpak.unom.io { reverse_proxy 192.168.50.50:3230 }`
3. **Port allowlist:** add `3230` to `caddy_target_ports` in `unom/infra` (proxmox/unom-1) + terraform apply.
4. **DNS:** ensure `flatpak.unom.io` resolves to the edge proxy.

Re-signing/rotation: regenerate the key, replace `unom-flatpak.gpg` + the secret; every client must
re-add the remote (the `GPGKey` changed), so rotate rarely.

## Alternatives considered

- **Hosted OSTree repo (chosen):** the only option that gives `flatpak update`. We self-host the
  static tree on unom-1 behind Caddy (Gitea has no flatpak/ostree registry); the build already
  produces the repo, so the marginal cost is GPG signing + an rsync + a 10-line static container.
- **Generic registry bundle (kept as fallback):** one curl to publish, one `flatpak install
  --bundle` to consume; mirrors the deb/rpm curl-upload pattern. No auto-update — this is what the
  **Decky plugin** pulls (stable `latest/punktfunk-client.flatpak`), plus the offline/manual path.
- **Release attachment:** also done on tags, good for a human-facing download page.
- **Flathub (blocked):** its [Generative AI policy](https://docs.flathub.org/docs/for-app-authors/requirements)
  bans AI-assisted manifests and AI-opened submissions, so a person writes both, not a copy of this
  manifest. Flathub also builds Skia from source (Neovide's manifest shows how), wants the newest
  GNOME runtime.
