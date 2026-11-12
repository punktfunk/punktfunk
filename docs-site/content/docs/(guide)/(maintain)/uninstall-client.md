---
title: Uninstalling a Client
description: Remove the Punktfunk app from the device you stream to, and clear what each client leaves behind.
---

Remove the Punktfunk app from any device you stream to, and see what each one leaves behind.

## Clients

Removing a client doesn't unpair it: unpair the device in the host's
[web console](/docs/web-console) too.

| Client | Remove | Left behind |
|---|---|---|
| Linux Flatpak | `flatpak uninstall --user --delete-data io.unom.punktfunk` | A `~/.config/punktfunk` an older version used, if any; the Flatpak remote (`flatpak remotes --user`, then `flatpak remote-delete --user <name>`) |
| Linux packages | `sudo apt purge punktfunk-client`, `sudo dnf remove punktfunk-client` or `sudo pacman -Rns punktfunk-client`; drop the repo [as for hosts](/docs/uninstall#linux-hosts) | `~/.config/punktfunk` |
| Windows installer | **Punktfunk** in **Settings → Apps → Installed apps**, or `& "$env:LOCALAPPDATA\Programs\Punktfunk\unins000.exe" /VERYSILENT`. Portable zip: delete the folder | `%APPDATA%\punktfunk` |
| Windows MSIX | `Get-AppxPackage *unom.punktfunk \| Remove-AppxPackage` | `%APPDATA%\punktfunk`, if present |
| macOS | Quit it and drag it from **Applications** to the Trash | — |
| iPhone, iPad, Apple TV | Delete the app; if you joined the beta, stop testing in **TestFlight** | — |
| Android, Android TV | Uninstall from Google Play or **Settings → Apps**; leave the testing program on the Play listing if you joined canary | — |
| Steam Deck (Decky) | Uninstall **Punktfunk** from Decky's plugin list ([Steam Deck → Uninstalling](/docs/steam-deck#uninstalling)) | The Steam shortcuts, the Steam Input template, the client and its settings: `~/.var/app/io.unom.punktfunk` for the Flatpak, `~/.config/punktfunk` for a native client |
| LG webOS TV | Remove it from the TV's launcher | — |
