---
title: Release Channels
description: Stable follows releases, canary follows main — pick one per machine, subscribe to it, pin a version or switch.
---

Pick a release channel per machine, subscribe to it, and pin or switch versions when you need to.

- **Stable** moves on each `vX.Y.Z` release. Every install guide uses it. Pick it for anything you
  don't want to babysit.
- **Canary** is built from `main` whenever a merge touches that platform. It is fast and sometimes
  broken; pick it for a test machine.

The channels are separate repositories, so a stable box never picks up a canary build. Canary is
always one minor version ahead of stable, so going back to stable is a downgrade. The web console's
**Updates** card follows the channel you installed from ([Updating](/docs/updating)).

## Subscribe

`…` stands for `https://git.unom.io/api/packages/unom`.

| Platform | Canary | Stable |
|---|---|---|
| **apt** | `canary main` at the end of the line in `/etc/apt/sources.list.d/punktfunk.list` | `stable main` |
| **dnf / rpm-ostree** | baseurl `…/rpm/fedora-44-canary` (Bazzite: `…/rpm/bazzite-canary`) | `…/rpm/fedora-44` (`…/rpm/bazzite`) |
| **Bazzite sysext** | `sudo punktfunk-sysext install --channel canary` | `sudo punktfunk-sysext install` |
| **pacman** | a `[punktfunk-canary]` section in `/etc/pacman.conf` | `[punktfunk]` |
| **Flatpak client** | `https://flatpak.unom.io/io.unom.Punktfunk.Canary.flatpakref` | `…/io.unom.Punktfunk.flatpakref` |
| **Decky plugin** | `…/generic/punktfunk-decky/canary/punktfunk.zip` | `…/generic/punktfunk-decky/latest/punktfunk.zip` |
| **Windows host and client** | `canary/` in the download URL, e.g. `…/generic/punktfunk-host-windows/canary/punktfunk-host-setup.exe` | `latest/` in the URL, the releases page, or winget |
| **Android** | Google Play open testing, or `…/generic/punktfunk-android/canary/punktfunk-android.apk` | [Google Play](https://play.google.com/store/apps/details?id=io.unom.punktfunk), or `latest/` |
| **Apple** | TestFlight | TestFlight, and the `.dmg` on the releases page |

The [releases page](https://git.unom.io/unom/punktfunk/releases) and winget carry stable only.

## Pin a version, or roll back

| How you installed | Pin or roll back |
|---|---|
| **apt** | `apt-cache madison punktfunk-host` lists versions; `sudo apt install punktfunk-host=<version>`. `sudo apt-mark hold punktfunk-host` stays there, `apt-mark unhold` resumes. |
| **dnf** | `sudo dnf --showduplicates list punktfunk` lists versions; `sudo dnf install punktfunk-<version>` or `sudo dnf downgrade punktfunk`. |
| **pacman** | `sudo pacman -U /var/cache/pacman/pkg/punktfunk-host-<version>-x86_64.pkg.tar.zst`, then `IgnorePkg = punktfunk-host` in `/etc/pacman.conf` so `-Syu` leaves it. |
| **Bazzite sysext** | `punktfunk-sysext status` prints the feed URL; download `punktfunk-<version>-x86-64.raw` from it and `sudo punktfunk-sysext install --from-file punktfunk-<version>-x86-64.raw`. |
| **Windows installer** | Run the older `punktfunk-host-setup-<version>.exe` over the current install. |
| **winget** | `winget install unom.PunktfunkHost --version <x.y.z>` |
| **Decky plugin** | Install from URL: `…/generic/punktfunk-decky/<version>/punktfunk.zip`. |
| **SteamOS on-device build** | `git -C ~/punktfunk checkout v<x.y.z>`, then `bash ~/punktfunk/scripts/steamdeck/update.sh` (without `--pull`, which fetches `main` again). |
| **NixOS** | `sudo nixos-rebuild switch --rollback`, or pin the flake input to a `v<x.y.z>` tag and rebuild. |

Your config, console password and paired devices carry across in both directions.

## Switch an installed box between channels

On a Linux host the guided installer switches either way, and asks before it moves:

```sh
curl -fsSLO https://punktfunk.unom.io/install.sh
sh install.sh --channel canary    # or: --channel stable
```

Without `--channel` it stays on the box's current channel. By hand:

```sh
# apt
sudo sed -i 's/ stable main/ canary main/' /etc/apt/sources.list.d/punktfunk.list
sudo apt update && sudo apt upgrade

# dnf (Fedora)
sudo sed -i 's#/rpm/fedora-44#/rpm/fedora-44-canary#' /etc/yum.repos.d/punktfunk.repo
sudo dnf upgrade punktfunk punktfunk-web punktfunk-scripting

# rpm-ostree layer (Bazzite / Fedora Atomic)
sudo sed -i 's#/rpm/bazzite#/rpm/bazzite-canary#' /etc/yum.repos.d/punktfunk.repo   # or fedora-44
sudo /usr/share/punktfunk/update-punktfunk.sh

# pacman: rename the section, then -Sy and -S (-Syu never steps down)
sudo sed -i 's/^\[punktfunk\]$/[punktfunk-canary]/' /etc/pacman.conf
sudo pacman -Sy && sudo pacman -S punktfunk-host punktfunk-web punktfunk-scripting

# Bazzite sysext
sudo punktfunk-sysext install --channel canary

# Flatpak client
flatpak install --user https://flatpak.unom.io/io.unom.Punktfunk.Canary.flatpakref
```

Back to stable is the same edit reversed, plus a step down the package manager allows:
`sudo apt install --allow-downgrades punktfunk-host=<version>` (versions from `apt-cache madison`),
`sudo dnf distro-sync punktfunk punktfunk-web punktfunk-scripting`, the same `pacman -S`, or
`sudo punktfunk-sysext install --channel stable`.

Cutting a release is on [Releasing](/docs/developers/releasing).
