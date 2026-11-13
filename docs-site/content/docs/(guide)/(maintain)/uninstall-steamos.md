---
title: Uninstalling a SteamOS Host
description: Remove the host you built on a Steam Deck or another SteamOS box — the services, the build container, the files under your home and the root-owned tuning.
---

Remove a Punktfunk host you built on a Steam Deck or another SteamOS box, including the files that
would otherwise survive every SteamOS update.

## SteamOS / Steam Deck host (on-device build)

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
   rm -f  ~/.local/share/applications/io.unom.punktfunk.host.desktop
   rm -f  ~/.local/bin/punktfunk-tray ~/.config/autostart/io.unom.punktfunk.tray.desktop
   rm -f  ~/.local/share/applications/io.unom.punktfunk.starthost.desktop
   rm -f  ~/.local/share/icons/hicolor/*/apps/punktfunk-tray*.png
   rm -rf ~/punktfunk                        # the source checkout and its build
   ```

   The container shares your home, so `~/.cargo`, `~/.rustup` and `~/.bun` are there too. Delete
   them only if nothing else of yours uses Rust or bun.

3. If you set up [Reachable without logging in](/docs/profiles#on-linux), turn it off under
   **Profiles** first, then remove it. The last line deletes every seat's home and the shared
   games folder:

   ```sh
   sudo systemctl disable --now punktfunk-web-door punktfunk-door punktfunk-seats \
     var-lib-punktfunk.mount 'var-lib-punktfunk\x2dseats.mount'
   sudo rm -rf /opt/punktfunk /etc/systemd/system/punktfunk-seat@.service.d
   sudo rm -f /etc/systemd/system/punktfunk-{seats,seat@,door,web-door,door-on@,door-off@}.service \
     /etc/systemd/system/var-lib-punktfunk*.mount \
     /etc/systemd/system/user@.service.d/50-punktfunk-seats.conf \
     /etc/systemd/user/punktfunk-scripting.service /etc/pam.d/punktfunk-seat \
     /etc/polkit-1/rules.d/49-punktfunk-door.rules \
     /etc/udev/rules.d/65-punktfunk-seats.rules /etc/tmpfiles.d/punktfunk-seats.conf
   sudo rm -rf /home/.steamos/offload/var/lib/punktfunk /home/.steamos/offload/var/lib/punktfunk-seats
   ```

   Each seat's account stays: `getent passwd | grep punktfunk-seat=` lists them for
   `sudo userdel`, and `sudo userdel punktfunk` removes the door's. Then drop the groups it used:
   `sudo groupdel punktfunk-door; sudo groupdel punktfunk-update; sudo groupdel punktfunk-games`.

4. Remove the root-owned tuning. Don't skip it: the keep-list entry carries these files through
   every SteamOS update.

   ```sh
   sudo rm -f /etc/atomic-update.conf.d/punktfunk.conf \
              /etc/udev/rules.d/60-punktfunk.rules \
              /etc/modules-load.d/punktfunk.conf \
              /etc/sysctl.d/99-punktfunk-net.conf \
              /etc/NetworkManager/dispatcher.d/90-punktfunk-frame-adapter \
              /etc/systemd/system/user@.service.d/50-punktfunk-nice.conf
   sudo udevadm control --reload-rules
   sudo systemctl daemon-reload
   ```

5. Optional: `rm -rf ~/.config/punktfunk` for a clean slate, and
   `sudo gpasswd -d "$USER" punktfunk; sudo groupdel punktfunk`. If you had no KDE RemoteDesktop
   grant before, the installer seeded `~/.local/share/flatpak/db/kde-authorized`; delete it if
   nothing else relies on it. Your `input` group membership is harmless to keep.
