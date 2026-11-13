# Identifiers

A reverse-DNS identifier is `io.unom.punktfunk`, all lowercase. A second part is a lowercase
dotted suffix: `io.unom.punktfunk.host`. Linux and Windows compare these case-sensitively, so a
match that ignores case is no match. A name that follows a platform's own convention keeps that
convention instead.

## Reverse-DNS identifiers

| Platform | Identifiers | Defined in |
| --- | --- | --- |
| Apple | Bundle `io.unom.punktfunk`, widgets `io.unom.punktfunk.widgets`, App Group `group.io.unom.punktfunk` | `clients/apple/Punktfunk.xcodeproj` |
| Android | Application id and package `io.unom.punktfunk` | `clients/android/app/build.gradle.kts` |
| Linux client | Flatpak, AppStream, GTK and Wayland app id, icon `io.unom.punktfunk`; console window app id `io.unom.punktfunk.console`; launchers `io.unom.punktfunk.desktop` and `io.unom.punktfunk.console.desktop` | `packaging/flatpak/`, `packaging/linux/`, `crates/client/pf-presenter/` |
| Linux host | KWin grant `io.unom.punktfunk.host.desktop`, launchers `io.unom.punktfunk.starthost.desktop` and `io.unom.punktfunk.webconsole.desktop`, tray autostart `io.unom.punktfunk.tray.desktop`, D-Bus `io.unom.punktfunk.shell`, polkit `io.unom.punktfunk.dm-helper` | `packaging/linux/`, `crates/host/` |
| Windows | MSIX `io.unom.punktfunk`, AUMIDs `io.unom.punktfunk.client` and `io.unom.punktfunk.tray` | `clients/windows/`, `crates/install/` |

## Platform conventions

| Kind | Form | Example |
| --- | --- | --- |
| Packages, binaries, systemd units, sysext images | Lowercase, hyphenated | `punktfunk-host`, `punktfunk-client.raw` |
| Windows services, tasks, registry keys | CamelCase, no dots | `PunktfunkHost`, `HKLM\SOFTWARE\Punktfunk` |
| winget | Publisher.Package | `unom.PunktfunkHost` |
| URL scheme, DNS-SD service | Lowercase | `punktfunk://`, `_punktfunk._udp` |
| GNOME Shell extension | name@domain | `punktfunk-windows@unom.io` |

## Never rename

- **Apple bundle id and App Group.** A new bundle id is a new App Store app, and the keychain
  identity stays with the old one.
- **Android application id.** Play ties the listing to it.
- **Windows Add/Remove Programs GUIDs and the winget `ProductCode`.** Upgrades find the install by
  them.
- **Plugin AppContainers, `punktfunk.plugin.<id>`.** The name derives the SID that every folder
  grant names.
- **The host sysext image `punktfunk`.** systemd-sysext merges an image only when its
  `extension-release` name matches the file, and the feed's updater writes that file.

## Older names still read

Older releases shipped `io.unom.Punktfunk` on Linux and `unom.punktfunk.*` on Windows. The code
that moves those installs names the old form on purpose:

- The Flatpak remote keeps the old refs, end-of-life with a rebase onto the new id
  (`.gitea/workflows/flatpak.yml`).
- Decky, its launch wrapper and the installer drive an old Flatpak until it moves.
- The host renames the tray's old autostart entry, and the Windows installer deletes the old tray
  AUMID key.
- Native packages link the old icon name to the new one for desktop entries users already have.
