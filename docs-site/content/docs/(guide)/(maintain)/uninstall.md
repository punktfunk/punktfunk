---
title: Uninstalling
description: Remove the Punktfunk host or a client for each install method, and clear what the removal leaves behind.
---

Remove Punktfunk the way you installed it, then clear what stays behind if you want a clean slate.

Removal never deletes the config directory: `~/.config/punktfunk` on Linux, `%ProgramData%\punktfunk`
for the Windows host. It holds the host's identity, paired devices, console password, `host.env`,
library, logs and [plugins](/docs/plugins), so a reinstall picks up where you left off.

## Linux hosts

The [guided installer](/docs/install#guided-install-preview) does steps 1 and 2 for apt, dnf,
pacman and the Bazzite sysext: `sh install.sh --uninstall` (fetch it with
`curl -fsSLO https://punktfunk.unom.io/install.sh`).

1. Stop the user services first. Package removal can't see the links `enable` wrote in your home,
   and the units then fail at every login:

   ```sh
   systemctl --user disable --now punktfunk-host punktfunk-web punktfunk-scripting
   ```

   Add `punktfunk-kde-session` if you set up the [headless KDE session](/docs/kde#headless-session).

2. Remove the packages and the repository. Name only the packages you installed:

   | How you installed | Remove |
   |---|---|
   | Ubuntu (apt) | `sudo apt purge punktfunk-host punktfunk-web punktfunk-scripting punktfunk-client`, then `sudo rm -f /etc/apt/sources.list.d/punktfunk.list /etc/apt/keyrings/punktfunk.asc && sudo apt update` |
   | Fedora (dnf) | `sudo dnf remove punktfunk punktfunk-web punktfunk-scripting punktfunk-client`, then `sudo rm -f /etc/yum.repos.d/punktfunk.repo` |
   | Bazzite / Fedora Atomic, layered RPMs | `sudo rpm-ostree uninstall punktfunk punktfunk-web` (what `rpm-ostree status` lists), remove `/etc/yum.repos.d/punktfunk.repo`, then reboot |
   | Bazzite sysext | [See below](#bazzite--fedora-atomic-systemd-sysext) |
   | Arch / CachyOS (pacman) | `sudo pacman -Rns punktfunk-host punktfunk-web punktfunk-scripting punktfunk-client`, then delete the `[punktfunk]` (or `[punktfunk-canary]`) section from `/etc/pacman.conf` |
   | Omarchy | `punktfunk-omarchy remove` first, then as Arch |
   | Steam Deck on-device build | [See below](#steamos--steam-deck-host-on-device-build) |
   | NixOS | [See below](#nixos) |

3. Clear what stays behind — the config directory, the two groups the packages create, lingering
   and the unit drop-ins:

   ```sh
   rm -rf ~/.config/punktfunk ~/.config/systemd/user/punktfunk-*.service.d
   sudo groupdel punktfunk-update
   sudo gpasswd -d "$USER" punktfunk; sudo groupdel punktfunk
   sudo loginctl disable-linger "$USER"      # only if nothing else needs it
   ```

   Drop the `punktfunk` group rather than keeping it: it can attach emulated USB devices.

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

`remove` deletes the image, `/etc/punktfunk-sysext.conf`, the tray autostart and the gamescope
session drop-in unless you edited it. The two files above it leaves. On a Bazzite with Steam's
session manager it may also have added an SELinux module: `sudo semodule -r punktfunk-ds-inhibit`.
Then do steps 3 and 4.

### SteamOS / Steam Deck host (on-device build)

There is no uninstall script. `sh install.sh --uninstall` only stops the services; do the rest by
hand, in this order.

1. Stop and remove the user services:

   ```sh
   systemctl --user disable --now punktfunk-host punktfunk-web \
     punktfunk-scripting punktfunk-rebuild-check
   rm -f ~/.config/systemd/user/punktfunk-*.service
   rm -rf ~/.config/systemd/user/punktfunk-*.service.d
   systemctl --user daemon-reload
   sudo loginctl disable-linger "$USER"      # only if nothing else on the Deck needs it
   ```

2. Remove the build container and the files under your home:

   ```sh
   distrobox rm -f pf2                       # the build container (~1 GB)
   rm -f  ~/.local/bin/punktfunk-scripting ~/.local/bin/punktfunk-gamescope
   rm -rf ~/.local/lib/punktfunk-scripting ~/.local/share/punktfunk-scripting
   rm -f  ~/.local/share/punktfunk/gamescope.stamp
   rm -f  ~/.local/share/applications/io.unom.Punktfunk.Host.desktop
   rm -rf ~/punktfunk                        # the source checkout and its build
   ```

   The container shares your home, so `~/.cargo`, `~/.rustup` and `~/.bun` are there too. Delete
   them only if nothing else of yours uses Rust or bun.

3. Remove the root-owned tuning. Don't skip it: the keep-list entry carries these files through
   every SteamOS update.

   ```sh
   sudo rm -f /etc/atomic-update.conf.d/punktfunk.conf \
              /etc/udev/rules.d/60-punktfunk.rules \
              /etc/modules-load.d/punktfunk.conf \
              /etc/sysctl.d/99-punktfunk-net.conf \
              /etc/systemd/system/user@.service.d/50-punktfunk-nice.conf
   sudo udevadm control --reload-rules
   sudo systemctl daemon-reload
   ```

4. Optional: `rm -rf ~/.config/punktfunk` for a clean slate, and
   `sudo gpasswd -d "$USER" punktfunk; sudo groupdel punktfunk`. If you had no KDE RemoteDesktop
   grant before, the installer seeded `~/.local/share/flatpak/db/kde-authorized`; delete it if
   nothing else relies on it. Your `input` group membership is harmless to keep.

### NixOS

Remove what you declared: the `services.punktfunk.*` options, `punktfunk.nixosModules.default` and
the `punktfunk` flake input. Then `sudo nixos-rebuild switch`. The unit, udev rules, tuning,
firewall ports and group memberships go with the generation; store paths stay until you
garbage-collect, and `~/.config/punktfunk` stays regardless.

## Windows host

Uninstall **Punktfunk Host** from **Settings → Apps → Installed apps**, or run
`winget uninstall unom.PunktfunkHost`. Both remove the service, the drivers, the scheduled tasks and
the firewall rules ([full list](/docs/windows-host#uninstalling)). Left behind on purpose:

- **`%ProgramData%\punktfunk`**, your config and pairings:
  `Remove-Item -Recurse -Force "$env:ProgramData\punktfunk"`.
- **The winget source**, if you added it: `winget source remove -n punktfunk` (admin PowerShell).
- **VB-CABLE**, if an older Punktfunk installed it. Other apps may use it; remove it with its own
  uninstaller (**VB-Audio Virtual Cable** in Installed apps).
- **The old unom certificate**, if you imported it by hand for release 0.28.1 or earlier. Current
  releases don't need it: in `certlm.msc`, delete the certificate issued to **unom** (thumbprint
  `CD1EFDEEEC9743AFC38F56C5AF30C5A3009BE941`) from **Trusted Publishers** and **Trusted Root
  Certification Authorities**.

### A Punktfunk display or gamepad stays in Device Manager

An older build could leave one behind. While the host is still installed, run from an elevated
prompt:

```powershell
punktfunk-host driver uninstall
punktfunk-host driver uninstall --gamepad
punktfunk-host driver uninstall --audio
```

Already uninstalled? Install the current version and uninstall it again; its uninstaller runs all
three.

## Clients

Removing a client doesn't unpair it: unpair the device in the host's
[web console](/docs/web-console) too.

| Client | Remove | Left behind |
|---|---|---|
| Linux Flatpak | `flatpak uninstall --user --delete-data io.unom.Punktfunk` | `~/.config/punktfunk`, shared with the native client and Decky; the Flatpak remote (`flatpak remotes --user`, then `flatpak remote-delete --user <name>`) |
| Linux packages | `sudo apt purge punktfunk-client`, `sudo dnf remove punktfunk-client` or `sudo pacman -Rns punktfunk-client`; drop the repo as for hosts | `~/.config/punktfunk` |
| Windows installer | **Punktfunk** in **Settings → Apps → Installed apps**, or `& "$env:LOCALAPPDATA\Programs\Punktfunk\unins000.exe" /VERYSILENT`. Portable zip: delete the folder | `%APPDATA%\punktfunk` |
| Windows MSIX | `Get-AppxPackage unom.Punktfunk \| Remove-AppxPackage` | `%APPDATA%\punktfunk`, if present |
| macOS | Quit it and drag it from **Applications** to the Trash | — |
| iPhone, iPad, Apple TV | Delete the app; to leave the beta, stop testing in **TestFlight** | — |
| Android, Android TV | Uninstall from Google Play or **Settings → Apps**; leave the testing program on the Play listing if you joined canary | — |
| Steam Deck (Decky) | Uninstall **Punktfunk** from Decky's plugin list ([Steam Deck → Uninstalling](/docs/steam-deck#uninstalling)) | The Steam shortcuts, the Steam Input template, the client and `~/.config/punktfunk` |
| LG webOS TV | Remove it from the TV's launcher; the community [`pf-webos`](https://github.com/dyptan-io/pf-webos) project handles the rest | — |

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
