---
title: Install & startup
description: Fixes for a host that won't install or start — the console package, pacman, NVIDIA EGL, host.env edits and the Windows service.
---

Fixes for a host that won't install or start. A Linux host service that fails and `nvidia-smi`
errors are on [Troubleshooting](/docs/troubleshooting#install--startup).

## Linux

### `systemctl --user status punktfunk-web`: unit not found

The console is its own package, and installing only the host can leave it out. Install it by name
and start it:

```sh
sudo apt install punktfunk-web      # Debian, Ubuntu
sudo dnf install punktfunk-web      # Fedora
sudo pacman -S punktfunk-web        # Arch
systemctl --user enable --now punktfunk-web
```

On Fedora, `No match for argument` means the COPR repo, which has no console. Use the repo from
[Fedora](/docs/fedora#2-install-the-host). Then [read the login password](/docs/forgot-password).

### pacman: error: could not register 'punktfunk' database (database already registered)

The `[punktfunk]` block is in `/etc/pacman.conf` twice. It's harmless; delete the second block.

### The desktop won't start, or "GPU … not supported by EGL"

The NVIDIA GL/EGL userspace package is missing. Install it and check its vendor file:
[GNOME → The GL/EGL userspace](/docs/gnome#the-glegl-userspace).

### The session fails right after editing host.env

A key is misspelled, or a line points the host at the wrong session. The host reads `host.env` only
when it starts, so restart after every edit: `systemctl --user restart punktfunk-host`.

- Keys are case-sensitive: `punktfunk_gamescope_attach=1` sets nothing.
- `XDG_RUNTIME_DIR=/run/user/1000` on a box where `id -u` isn't 1000 points the host at another
  user's session. Delete the [session anchors](/docs/configuration#session-anchors); the service
  doesn't need them.
- `PUNKTFUNK_COMPOSITOR` pins one backend and stops the host following Game Mode ↔ Desktop. Remove
  it on a box that switches sessions.

## Windows

### Windows: the host or the web console won't start

The `PunktfunkHost` service runs both and restarts either one that stops. Check it from an elevated
PowerShell:

```powershell
punktfunk-host service status
punktfunk-host service restart
```

- Two `punktfunk-host.exe` processes are normal: one supervises, one streams in your session. Don't
  end either.
- If the console page stays down, read its log from the elevated PowerShell:
  `Get-Content -Tail 50 $env:ProgramData\punktfunk\logs\web.log`.

### Windows: the status icon is missing after an update

An update closes the tray, and Windows starts it again only at sign-in. From a normal PowerShell,
not an elevated one:

```powershell
punktfunk-host tray start
```
