# punktfunk-host — Debian/Ubuntu package (apt)

> **Which distros the published packages install on** — measured by installing them, not inferred
> from the build image (`scripts/ci/deb-install-smoke.sh` asserts this on every run):
>
> | | Ubuntu 24.04 | Ubuntu 26.04 | Debian 13 | Debian 12 |
> |---|---|---|---|---|
> | `punktfunk-host` / `punktfunk-seats` | ✅ | ✅ | ✅ | ❌ glibc 2.36 < 2.39 |
> | `punktfunk-web` / `punktfunk-scripting` | ✅ | ✅ | ✅ | ✅ |
> | `punktfunk-gamescope` | ❌ wayland 1.22 | ✅ | ✅ | ❌ |
> | `punktfunk-client` | ❌ `libc6 >= 2.43` | ✅ | ❌ `libc6 >= 2.43` | ❌ |
>
> Debian 13 is a supported host target ([docs](https://docs.punktfunk.unom.io/docs/debian)); the
> client is the one gap, since it is built on 26.04 and floors at that release's glibc.

`punktfunk-host` is published as a `.deb` to **Gitea's Debian package registry** in the public
`unom` org, so the Ubuntu hosts update with plain `apt`. CI (`.gitea/workflows/deb.yml`) builds
and publishes on every push to `main` (a rolling `<next-minor>~ciN.g<sha>` build — the base is
derived from the latest stable tag by `scripts/ci/pf-version.sh` — to the **`canary`** apt
distribution) and on `vX.Y.Z` tags (a clean `X.Y.Z` to the **`stable`** distribution, plus attached
to the unified Gitea Release). The two are separate apt distributions, so a stable box never jumps
to a canary build — see [Release Channels](https://punktfunk.unom.io/docs/channels). The repo line
below subscribes to `stable`; swap `stable` → `canary` for the latest main builds.

The same workflow also publishes **`punktfunk-web`** (the browser management console — pairing +
status) and **`punktfunk-client`** (the native GTK4/libadwaita Linux client). `punktfunk-host` **Recommends**
`punktfunk-web`, so a default `apt install punktfunk-host` pulls the console too (alongside the
udev/sysctl bits) unless you've disabled weak deps; `punktfunk-client` is independent — install it
on the box you stream *to*. (`punktfunk-probe` is the headless reference/test tool, not packaged
here.) `punktfunk-web` and `punktfunk-scripting` both depend on **`punktfunk-bun`**, the one pinned
bun they run on, at `/usr/lib/punktfunk-bun/bun`, never on PATH.

**`punktfunk-seats`** (`build-seats-deb.sh`) is the root supervisor behind profile seats. The host
job builds it beside the host, and `punktfunk-host` Recommends it. It ships disabled: the console
turns seats on. It pins `punktfunk-host` to its own version.

Package layout mirrors the Fedora RPM (`../rpm/punktfunk.spec`): the host binary, the `/dev/uinput`
udev rule, the systemd **user** unit, headless session helpers, the example config, and the OpenAPI
doc. Runtime `Depends` are computed by `dpkg-shlibdeps` from the binary itself. The NVIDIA driver
(`libnvidia-encode` / `libEGL_nvidia` / `libcuda`) is **not** a dependency — it's installed out of
band, like on the RPM side.

## Ubuntu 24.04 LTS (and why it needs a special build)

A host `.deb` built on the same Ubuntu 26.04 image as the client (`ci/rust-ci.Dockerfile`) bakes in
a glibc-2.41 floor that 24.04's apt can't satisfy ("the required packages are too recent"). So the
host `.deb` is built on an **Ubuntu 24.04 image** (`ci/rust-ci-noble.Dockerfile`) instead. The
result is **one** host `.deb` that installs on **Ubuntu 24.04 LTS through 26.04** (glibc floor
2.39). The client/web/scripting `.deb`s still build
on 26.04 (the native client needs SDL3 / GTK4 ≥ 4.20, absent on 24.04) — install the client on the box
you stream *to*, which is independent of the host's distro.

## `punktfunk-gamescope` is built on Debian 13, not Ubuntu

The patched gamescope has its own job (`build-publish-gamescope`) in a **Debian 13** image
(`ci/gamescope-trixie.Dockerfile`), and that is not a preference — it is the only apt distro the
tree configures on. Built in the noble host image, as it was until 2026-08, it failed every single
run:

```
wlroots| Dependency wayland-server found: NO found 1.22.0 but need: '>=1.23.1'
subprojects/wlroots/meson.build:96:17: ERROR: Dependency 'wayland-server' is required but not found
```

Our pin vendors wlroots 0.19.3, which floors wayland-server at 1.23.1; noble ships 1.22.0 (and has
no `libxcb-errors-dev`, and only libdisplay-info 0.1.1). Because every rung of that path was a
`::warning::` returning 0, **v0.26.0 and v0.27.0 both shipped with no gamescope .deb** while the
release notes and docs-site said it was apt-installable. Debian 13 has wayland 1.23.1 exactly —
the oldest apt base that works.

Two things make the one package serve both Debian 13 and Ubuntu 26.04:

- **`--extra-fallback libdisplay-info`** (see `packaging/gamescope/build-punktfunk-gamescope.sh`).
  Linked against the distro copy, the package picks up `Depends: libdisplay-info2 (>= 0.2.0)` on
  trixie — and Ubuntu 26.04 carries libdisplay-info **3** (0.3.0), so apt refuses it there.
  gamescope vendors the library as a submodule, so the vendored build drops the dependency. Same
  reasoning the script already applies to wlroots: a binary we ship must not follow the build
  host's shared libraries.
- The **static C++ runtime** the build script already forces, so `libstdc++` never appears in
  `NEEDED`. The binary asks only for `GLIBC_2.38`.

**Ubuntu 24.04 gets no gamescope package** and cannot: the wayland floor is a runtime one too.

## Install, firewall, updates

All three live on the docs pages ([Debian](https://docs.punktfunk.unom.io/docs/debian) /
[Ubuntu](https://docs.punktfunk.unom.io/docs/ubuntu)), stated once so they cannot drift. Packager
notes: the registry is public, so there is no apt auth beyond the signing key, and `stable` and
`canary` are separate apt distributions, so a stable box never jumps to a canary build. The package
ships ufw profiles and firewalld service definitions, neither auto-enabled — Debian ships no
firewall and Ubuntu's `ufw` is installed-but-inactive, so out of the box there is nothing to open.

## Build a `.deb` locally

```sh
VERSION=0.0.1 bash packaging/debian/build-deb.sh   # -> dist/punktfunk-host_0.0.1_amd64.deb
```

Needs `dpkg-dev` (`dpkg-shlibdeps`, `dpkg-deb`). It builds the release binary first if missing.
Building on a GPU box is fine — the NVIDIA driver lib is filtered out either way.

That invocation inherits the build box's glibc floor. For the **universal** package CI ships
(installs on 24.04 LTS → 26.04), build it in the noble image:

```sh
docker build -f ci/rust-ci-noble.Dockerfile -t pf-noble ci
docker run --rm -v "$PWD:/src" -w /src pf-noble \
  bash -lc 'VERSION=0.0.1 bash packaging/debian/build-deb.sh'
```

### The arm64 client `.deb`

The **client** also ships for arm64 (`punktfunk-client_<version>_arm64.deb`, published to the same
apt distribution — the registry keys pool entries by architecture, so an arm64 box needs no extra
configuration). There is no arm64 **host** package: the Linux host encodes with NVENC/QSV/AMF, all
x86.

It is cross-compiled on an ordinary amd64 machine in `ci/rust-ci-arm64cross.Dockerfile` — the
rust-ci toolchain plus an Ubuntu ports arm64 multiarch sysroot. No arm64 runner is involved:

```sh
docker build -f ci/rust-ci-arm64cross.Dockerfile -t pf-arm64cross .   # repo-root context
docker run --rm -v "$PWD:/w" -w /w pf-arm64cross \
  bash -lc 'VERSION=0.0.1 ARCH=arm64 TARGET=aarch64-unknown-linux-gnu \
              bash packaging/debian/build-client-deb.sh'
```

`TARGET` moves the binaries to `target/<triple>/release`; `ARCH` sets the package's
`Architecture:` field. Set both — one without the other builds an amd64 binary into a package
labelled arm64, or vice versa. `dpkg-shlibdeps` reads the arm64 sonames straight out of the
multiarch sysroot, so `Depends:` comes out right with no manual list.
