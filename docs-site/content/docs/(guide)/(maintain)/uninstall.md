---
title: Uninstalling
description: Remove the Punktfunk host or a client for each install method, and clear what the removal leaves behind.
---

Remove Punktfunk the way you installed it, then clear what stays behind if you want a clean slate.

- A Linux host from packages, the Bazzite sysext or NixOS: [Linux hosts](#linux-hosts), below.
- A host built on a Steam Deck or another SteamOS box:
  [Uninstalling a SteamOS Host](/docs/uninstall-steamos).
- The Windows host: [Uninstalling the Windows Host](/docs/uninstall-windows).
- A client app, on any device: [Uninstalling a Client](/docs/uninstall-client).
- Plugins, on any host: [Plugins and the script runner](#plugins-and-the-script-runner), before
  you remove the host.

Removal never deletes the config directory: `~/.config/punktfunk` on Linux, `%ProgramData%\punktfunk`
for the Windows host. It holds the host's identity, paired devices, console password, `host.env`,
library, logs and [plugins](/docs/plugins), so a reinstall picks up where you left off.

## Linux hosts

The [guided installer](/docs/install#guided-install-preview) does steps 1 and 2 for apt, dnf,
pacman and the Bazzite sysext: `sh install.sh --uninstall` (fetch it with
`curl -fsSLO https://punktfunk.unom.io/install.sh`).

With [Reachable without logging in](/docs/profiles#on-linux) or seat profiles set up, open
**Profiles** in the console before step 1: turn **Reachable without logging in** off and remove the
seat profiles, which deletes their accounts.

1. Stop the user services first. Package removal can't see the links `enable` wrote in your home,
   and the units then fail at every login:

   ```sh
   systemctl --user disable --now punktfunk-host punktfunk-web punktfunk-scripting
   ```

   Add `punktfunk-kde-session` if you set up the [headless KDE session](/docs/kde#headless-session).

2. Remove the packages and the repository. Name only the packages you installed:

   | How you installed | Remove |
   |---|---|
   | Ubuntu (apt) | `sudo apt purge punktfunk-host punktfunk-web punktfunk-scripting punktfunk-seats punktfunk-gamescope punktfunk-bun punktfunk-client`, then `sudo rm -f /etc/apt/sources.list.d/punktfunk.list /etc/apt/keyrings/punktfunk.asc && sudo apt update` |
   | Fedora (dnf) | `sudo dnf remove punktfunk punktfunk-web punktfunk-scripting punktfunk-client`, then `sudo rm -f /etc/yum.repos.d/punktfunk.repo` |
   | Bazzite / Fedora Atomic, layered RPMs | `sudo rpm-ostree uninstall punktfunk punktfunk-web` (what `rpm-ostree status` lists), remove `/etc/yum.repos.d/punktfunk.repo`, then reboot |
   | Bazzite sysext | [See below](#bazzite--fedora-atomic-systemd-sysext) |
   | Arch / CachyOS (pacman) | `sudo pacman -Rns punktfunk-host punktfunk-web punktfunk-scripting punktfunk-client`, then delete the `[punktfunk]` (or `[punktfunk-canary]`) section from `/etc/pacman.conf` |
   | Omarchy | `punktfunk-omarchy remove` first, then as Arch |
   | Steam Deck on-device build | [Uninstalling a SteamOS Host](/docs/uninstall-steamos) |
   | NixOS | [See below](#nixos) |

3. Clear what stays behind — the config directory, the tray's autostart entry, the three groups the
   packages create, lingering and the unit drop-ins:

   ```sh
   rm -rf ~/.config/punktfunk ~/.config/systemd/user/punktfunk-*.service.d
   rm -f ~/.config/autostart/io.unom.punktfunk.tray.desktop
   sudo groupdel punktfunk-update
   sudo groupdel punktfunk-door
   sudo gpasswd -d "$USER" punktfunk; sudo groupdel punktfunk
   sudo loginctl disable-linger "$USER"      # only if nothing else needs it
   ```

   Drop the `punktfunk` group rather than keeping it: it can attach emulated USB devices. If
   Reachable without logging in was on, run `sudo userdel punktfunk` first, or
   `groupdel punktfunk-door` refuses: the door's user holds that group. Seats leave a
   `punktfunk-games` group: `sudo groupdel punktfunk-games`.

4. Close the firewall ports you opened:

   | Firewall | Close |
   |---|---|
   | ufw | `sudo ufw delete allow punktfunk-native`, and `punktfunk-gamestream` / `punktfunk-web` if you allowed them |
   | firewalld | `sudo firewall-cmd --permanent --remove-service=punktfunk-native` (and the other two), then `sudo firewall-cmd --reload` |

   If the setup moved the management port next to Sunshine, also remove `47991/tcp`.

On Arch you can drop the repository key too:
`sudo pacman-key --delete E0CA04465C99C936E0B0C6510A317015A34DDD69`.

### Bazzite / Fedora Atomic (systemd-sysext)

Stop the services (step 1) before you remove the image, because the binaries leave with it:

```sh
sudo punktfunk-sysext remove
sudo rm -f /etc/modules-load.d/punktfunk.conf /etc/udev/rules.d/60-punktfunk.rules
```

`remove` deletes the image, `/etc/punktfunk-sysext.conf` and the gamescope session drop-in
unless you edited it. The two files above it leaves. On a Bazzite with Steam's
session manager it may also have added an SELinux module: `sudo semodule -r punktfunk-ds-inhibit`.
Then do steps 3 and 4.

### SteamOS / Steam Deck host (on-device build)

The on-device build has no uninstall script; remove it by hand with
[Uninstalling a SteamOS Host](/docs/uninstall-steamos).

### NixOS

Remove what you declared: the `services.punktfunk.*` options, `punktfunk.nixosModules.default` and
the `punktfunk` flake input. Then `sudo nixos-rebuild switch`. The unit, udev rules, tuning,
firewall ports and group memberships go with the generation; store paths stay until you
garbage-collect, and `~/.config/punktfunk` stays regardless.

## Plugins and the script runner

Plugins live in the host's config directory, so they survive host removal. Remove them first, or
delete the directories afterwards:

```sh
punktfunk-host plugins list             # what's installed
punktfunk-host plugins remove <name>    # uninstall one
punktfunk-host plugins disable          # stop and disable the runner
```

On Windows run these from an elevated PowerShell. `plugins remove` leaves each plugin's settings and
cache in `plugin-state/<plugin>/` (including API keys you entered) and the runner's `plugin-token`;
deleting the config directory removes them. The runner package (`punktfunk-scripting`) comes off
with the host packages; on Windows it goes with the host installer.

## Removing the pairing, not the software

- **On the host:** unpair the device in the [web console](/docs/web-console). It stops being
  trusted immediately.
- **On a Linux or Windows client:** `punktfunk hosts forget <host>` drops one saved host;
  `punktfunk reset` drops all of them and the stream settings but keeps the client's identity.
  The Linux desktop app also takes `punktfunk-client --forget-host <fingerprint|host[:port]>`.

See [Pairing](/docs/pairing) for how the two halves fit.
