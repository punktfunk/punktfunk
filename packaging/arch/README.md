# punktfunk on Arch Linux / SteamOS

Packaging for punktfunk on Arch and Arch-derived immutable distros. The `PKGBUILD` is a **split
package** producing **`punktfunk-host`** (the gaming-rig host) and **`punktfunk-client`** (the native
GTK4/libadwaita Linux client) — mirrors the rpm subpackages (`packaging/rpm/punktfunk.spec`) and the
deb build scripts. On a **Steam Deck used as a client you want `punktfunk-client`** (it's what the
[Decky plugin](../../clients/decky/) launches); on a gaming rig, `punktfunk-host`.

> **Steam Deck as a HOST:** don't use this PKGBUILD — SteamOS's read-only root makes `makepkg`/sysext
> awkward, and a prebuilt binary breaks on OS library bumps. Use the on-device build script instead:
> **[`scripts/steamdeck/install.sh`](../../scripts/steamdeck/)** (it builds in a Debian-trixie distrobox
> ABI-matched to SteamOS and uses **VAAPI** on the Deck's AMD GPU). The Deck host path is the one
> exception to "host encode is NVENC-only" below.

A third member, **`punktfunk-web`** (the browser management console — pairing + status), is
**opt-in**: build it by setting `PF_WITH_WEB=1`, which requires **`bun`** at build time (`bun-bin`
from the AUR if it isn't in your repos). bun is also the **runtime** — the console serves HTTPS
(HTTP/1.1 over TLS) via `Bun.serve`. Either `PF_WITH_WEB=1` or `PF_WITH_SCRIPTING=1` also builds
**`punktfunk-bun`**: the one vendored bun both packages run on, at `/usr/lib/punktfunk-bun/bun`,
never on PATH (no `nodejs` dependency). A default `makepkg` builds only host+client with no JS
tooling — mirroring the RPM spec's `%bcond_with web`.

> **Host encode: NVENC on NVIDIA, VAAPI on AMD/Intel** (`PUNKTFUNK_ENCODER=auto` picks one). The host
> now has a VAAPI encoder + zero-copy dmabuf path alongside NVENC/CUDA, so `punktfunk-host` works on
> Arch + NVIDIA **and** AMD/Intel (incl. the Steam Deck — see the on-device path above). The client
> decodes via VAAPI on AMD/Intel with a software fallback.

## Install

On the [docs site](https://docs.punktfunk.unom.io/docs/arch). CI (`arch.yml`) builds this PKGBUILD
in an `archlinux:base-devel` container on every push and publishes to the Arch package registry — a
plain pacman repo, so a box installs and updates with `pacman -Syu` like anything else. It is a
binary repo, so a partial upgrade is not supported: always a full `-Syu`.

### aarch64 (Arch Linux ARM) — the client

The PKGBUILD declares `arch=('x86_64' 'aarch64')`. On aarch64 it builds the **client only** —
`pkgname` drops `punktfunk-host`, so makepkg never enters the host's build or package path, and
`build()` skips the host/tray cargo invocations and their NVENC/Vulkan-encode features. The host
stays x86-only because its encode stack (NVENC/QSV/AMF) is.

Nothing else changes — run the same command on an Arch Linux ARM box:

```sh
cd packaging/arch
PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
# -> punktfunk-client-<ver>-<rel>-aarch64.pkg.tar.zst   (no punktfunk-host package)
```

There is no cross-compile path here: makepkg builds for `CARCH`, so this wants a real aarch64
Arch machine (or an emulated Arch Linux ARM container, which is slow). Unlike the deb, it has
**not** been verified end to end yet — there is no official arm64 Arch container to test in.
Then the standard first-run (printed by the install scriptlet):
```sh
sudo usermod -aG input "$USER"          # virtual gamepads; re-login after
systemctl --user enable --now punktfunk-host
# Settings: the console. A line in ~/.config/punktfunk/host.env locks that setting there.
# Web console (if you installed the punktfunk-web package): enable it + read the login password.
systemctl --user enable --now punktfunk-web
journalctl --user -u punktfunk-web-init | sed -n 's/.*password generated: //p'   # open https://<host-ip>:47992
```
NVENC/EGL come from the NVIDIA driver: `sudo pacman -S --needed nvidia-utils`.

### Runtime dependency map (Fedora/Debian → Arch)

| Need | Arch package |
|------|--------------|
| PipeWire + session mgr | `pipewire` `wireplumber` |
| PulseAudio-API audio for games | `pipewire-pulse` *(host optdepend — real `pulseaudio` also works; never a hard dep, it CONFLICTS with `pulseaudio`)* |
| Opus / input injection | `opus` `libei` |
| GL/EGL + gbm + xkb + wayland | `libglvnd` `mesa` `libxkbcommon` `wayland` |
| NVIDIA driver (NVENC/EGL/CUDA) | `nvidia-utils` *(optdepend — never a hard dep)* |
| Compositor backends | `gamescope` (≥3.16.22) / `kwin` / `mutter` / `sway` *(optdepends)* |

## Immutable Arch (SteamOS 3) — the systemd-sysext mechanism

SteamOS has a **read-only `/usr` on A/B partitions**, and every OS update reimages the rootfs —
so `steamos-readonly disable` + `pacman` is fragile for anything that must survive updates. The
SteamOS-blessed overlay mechanism is a **systemd-sysext**: an image merged read-only over `/usr`
at boot, living in the writable `/var/lib/extensions/`.

> **For a SteamOS HOST this is NOT the supported path** — that is
> [`scripts/steamdeck/install.sh`](../../scripts/steamdeck/) (the on-device distrobox build,
> which also builds the HDR gamescope). A host sysext carries a prebuilt binary that breaks on
> the next SteamOS soname bump, and `/var` — where sysexts live — is per-A/B-partition-set.
> The mechanism below is what the Deck **client** image uses (next section), and an option for
> operators on other immutable Arch derivatives who accept the prebuilt trade-off.

Build the package, then wrap its `/usr` payload into a sysext image:
```sh
# 1. build the pacman packages (needs an Arch environment / container)
cd packaging/arch && PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
( cd ../gamescope && makepkg -f -d --holdver )   # optional: the HDR gamescope companion
# 2. turn it into a sysext .raw (extracts the packages' /usr into an image + extension-release);
#    --gamescope folds the HDR build into a HOST image (verified by its +pfhdr banner)
bash build-sysext.sh --gamescope ../gamescope/punktfunk-gamescope-*.pkg.tar.zst punktfunk-host-*.pkg.tar.zst
# 3. on the box:
sudo cp punktfunk-host.raw /var/lib/extensions/
sudo systemctl enable --now systemd-sysext      # merges it
systemctl --user enable --now punktfunk-host     # the user unit is now under /usr/lib
```
The udev rule, sysctl, and systemd **user** unit all live under `/usr/lib`, so the merged sysext
exposes them. `systemd-sysext refresh` re-merges after a reboot. (One HDR nuance of the sysext
path: the image ships gamescope without `CAP_SYS_NICE`, so its frame pacing is marginally worse —
everything works. Capabilities inside the image: `punktfunk-host` carries **none**, on *either*
path, deliberately — one would make it unidentifiable to KWin and break desktop streaming;
`punktfunk-encode-worker` carries `cap_sys_nice=ep`, applied by `build-sysext.sh` because pacman
scriptlets never run for a sysext and a merged `/usr` is read-only. Both are asserted at build
time. See
[Running as a service](https://punktfunk.io/docs/running-as-a-service#gpu-scheduling-priority).)

## Steam Deck — the client (what the Decky plugin launches)

To stream *to* a Deck, you install **`punktfunk-client`** there — same sysext mechanism, but
wrapping the client package instead. The split `makepkg` produces both `.pkg.tar.zst` files; on the
Deck use the client one:
```sh
cd packaging/arch && PF_SRCDIR="$(git rev-parse --show-toplevel)" makepkg -f --holdver
bash build-sysext.sh punktfunk-client-*.pkg.tar.zst        # → punktfunk-client.raw
# on the Deck:
sudo cp punktfunk-client.raw /var/lib/extensions/
sudo systemctl enable --now systemd-sysext
sudo pacman -S --needed libva-mesa-driver                  # VAAPI hw decode on the Deck's AMD APU
```
Now `punktfunk-client` is on `PATH`, so the **[Decky plugin](../../clients/decky/)** finds and
launches it (`punktfunk-client --connect host:port`) — gamescope composites its video like a game.
The client needs no `/dev/uinput` or compositor-spawning rights (it captures input and decodes),
so it's a much lighter sysext than the host.

## Firewall

**Stock Arch ships no firewall**, so there is nothing to do — but spins that enable one do not get
their ports opened, because an Arch package never touches the admin's running firewall. **CachyOS is
the common case**: it ships `ufw` enabled, so the host is unreachable until you allow it. Some spins
(EndeavourOS) enable `firewalld` instead.

The package ships openers for both — a ufw application profile and firewalld service definitions,
neither auto-enabled. The commands are on the
[Arch docs page](https://docs.punktfunk.unom.io/docs/arch) and the port breakdown in
[`data/platforms.json`](../../data/platforms.json).

## Files
- `PKGBUILD` — split package: `punktfunk-host` + `punktfunk-client` + `punktfunk-seats` (builds the
  working tree via `PF_SRCDIR`, or a git tag for AUR). `punktfunk-seats` is the root supervisor
  behind profile seats: a host `optdepends`, installed under `/usr/libexec/punktfunk`, never enabled.
  It also carries the door's units, root helper and polkit rule.
- `punktfunk-host.install` / `punktfunk-client.install` — pacman scriptlets (udev reload + sysctl +
  first-run hint, incl. the ufw/firewalld enable command for whichever is present), mirror the RPM
  `%post` / deb postinst. `punktfunk-scripting.install` loads the runner's AppArmor profile.
  `punktfunk-seats.install` creates the `punktfunk` group and the seat directories.
- The firewall openers are shared across all Linux packaging and live in [`../linux/`](../linux/):
  the ufw application profile (`punktfunk.ufw` → `/etc/ufw/applications.d/punktfunk`) and the
  firewalld service definitions (`punktfunk-native.xml` / `punktfunk-gamestream.xml` /
  `punktfunk-web.xml` → `/usr/lib/firewalld/services/`). None auto-enabled; see Firewall above.
- `build-sysext.sh` — wraps either built `.pkg.tar.zst` into a `systemd-sysext` `.raw` for SteamOS
  (derives the name from the package, so it works for host or client).
