---
title: Running as a Service
description: Start the host at boot — on a desktop you log into, or on a headless always-on machine.
---

Run the host as a service so it is always there: from login on a desktop, from boot on a headless
box.

## What the unit starts

The service runs `punktfunk-host serve`: the Punktfunk plane (`punktfunk/1`) and the management
API. Two more planes stay off until you turn them on under **Host → Settings** in the
[web console](/docs/web-console), or in `~/.config/punktfunk/host.env`:

| Plane | Setting | `host.env` | Firewall |
|---|---|---|---|
| Stock [Moonlight](/docs/moonlight) (GameStream) | **GameStream** | `PUNKTFUNK_GAMESTREAM=1` | open the `punktfunk-gamestream` service |
| Browser client (preview) | **Browser streaming** | `PUNKTFUNK_WEBTRANSPORT=1` | UDP 9778, in `punktfunk-native` (`PUNKTFUNK_WEBTRANSPORT_PORT` moves it) |

Both take effect after a host restart. GameStream pairs over plain HTTP with weaker encryption, so
turn it on only on a LAN you trust ([Security](/docs/security)).
`systemctl --user cat punktfunk-host` shows the unit in effect, drop-ins included.

### The browser client (preview)

A browser that connects runs a full session. If you leave the plane on, set **Browser origins**
(`PUNKTFUNK_WEBTRANSPORT_ORIGINS`) to the address you load the client from — left empty, any page
open in your browser can reach the port. `PUNKTFUNK_WEBTRANSPORT_BIND` keeps the plane on one
interface.

## A desktop you log into [#a-a-desktop-you-log-into]

The packages ship a systemd user unit. Enable it once and the host starts at every login:

```sh
systemctl --user daemon-reload             # needed on the Bazzite sysext, harmless elsewhere
systemctl --user enable --now punktfunk-host
systemctl --user status punktfunk-host
```

The host finds your live session by itself, so it needs no `host.env` and no
`systemctl --user import-environment`. For settings, use the console or copy a template to
`~/.config/punktfunk/host.env` — they are in `/usr/share/punktfunk/` (`/usr/share/punktfunk-host/`
on Ubuntu); every knob is in [Configuration](/docs/configuration). A line in `host.env` locks
that setting in the console until you delete it. Built from source?
[Build from source](/docs/developers/build-from-source) installs the unit.

### Restart the host with your desktop

Without this drop-in, restarting Plasma or GNOME (a crash, logging out and back in) leaves the host
answering but failing every session at capture. The drop-in makes a desktop restart a host restart:

```sh
mkdir -p ~/.config/systemd/user/punktfunk-host.service.d
# /usr/share/punktfunk-host/ on Ubuntu
cp /usr/share/punktfunk/punktfunk-host-desktop-session.conf \
   ~/.config/systemd/user/punktfunk-host.service.d/desktop-session.conf
systemctl --user daemon-reload
systemctl --user reenable punktfunk-host
systemctl --user restart punktfunk-host
```

On NixOS set `services.punktfunk.host.desktopSession = true;` instead.

Skip it on a headless box (below). Sway and Hyprland never reach systemd's
`graphical-session.target`, so the drop-in does nothing there: leave the unit disabled and start the
host from the compositor config —
`exec systemctl --user start punktfunk-host` (sway) or
`exec-once = systemctl --user start punktfunk-host` (Hyprland).

## A headless, always-on host

A box with no monitor and no login needs the host to start without a login, and a desktop session
that comes up at boot.

1. Let user services start at boot (the guided installer's `--linger` does the same):

   ```sh
   sudo loginctl enable-linger "$USER"
   ```

2. Bring up a session at boot:
   - GNOME: [GNOME → Headless session](/docs/gnome#headless-session).
   - KDE Plasma: [KDE → Headless session](/docs/kde#headless-session).
   - Steam / gamescope, Bazzite: nothing to set up — the host starts a session per client
     ([gamescope](/docs/gamescope)).
3. Enable the host unit as above, and reboot.

## Windows

The installer registers the host as the **`PunktfunkHost`** service: `LocalSystem`, started at
boot, so it captures the lock screen and UAC prompts with nobody logged in. Keep a host with that
much reach on a network you trust ([Security](/docs/security)).

Manage it from an elevated prompt:

```powershell
punktfunk-host service status
punktfunk-host service restart
```

Every `service` subcommand is in [Host CLI](/docs/host-cli#service-windows). The planes in
[What the unit starts](#what-the-unit-starts) work the same way; `host.env` is
`%ProgramData%\punktfunk\host.env`. The installer opens the firewall on Private and Domain networks
only; a LAN Windows calls Public needs [one more step](/docs/troubleshooting#windows-firewall).

## Verifying

After a reboot, from another machine:

```sh
punktfunk reachable 192.168.1.50   # exit 0 = the host answered, 2 = it didn't
punktfunk hosts list --probe       # every saved host, online or offline
```

`punktfunk` ships with the Linux client packages and the Windows client. Any client that lists the
host works too. No answer? Read `journalctl --user -u punktfunk-host` on the host, or
`punktfunk-host service status` on Windows.

## GPU scheduling priority

[PyroWave](/docs/pyrowave) encodes on the same GPU cores as your game, so the host asks the driver
to run the encode first. That request needs `CAP_SYS_NICE`, which every Linux install grants to a
small helper, `punktfunk-encode-worker` — never to `punktfunk-host`. The other codecs use the GPU's
video engine and don't need it.

**Never give `punktfunk-host` a capability** — not with `setcap`, a systemd `AmbientCapabilities=`
line or a NixOS `security.wrappers` entry. KWin can't identify a host that holds one, and every KDE
session fails at capture with:

```
KWin does not expose zkde_screencast_unstable_v1 to this client
```

Check both binaries:

```sh
getcap /usr/bin/punktfunk-host              # should print nothing
getcap /usr/bin/punktfunk-encode-worker     # /usr/bin/punktfunk-encode-worker cap_sys_nice=ep
sudo setcap -r /usr/bin/punktfunk-host      # clears it; then restart the host
```

On the Bazzite image `/usr` is read-only: run `sudo punktfunk-sysext update` instead. On NixOS the
module wraps the worker for you.

Without the grant the stream still runs, at normal priority; only frame pacing under a GPU-bound
game suffers. `PYROWAVE_QUEUE_PRIORITY=off` and `PUNKTFUNK_ENCODE_WORKER=off` are the opt-outs —
[Configuration](/docs/configuration#advanced-performance-tuning).

## Stopping and removing

```sh
systemctl --user stop punktfunk-host        # Linux, until next login
systemctl --user disable --now punktfunk-host
punktfunk-host service stop                 # Windows, elevated
```

`punktfunk-host service uninstall` removes the Windows service and its firewall rules; the rest of
the install goes through **Settings → Apps**. A Linux package update restarts the running service
for you — [Updating](/docs/updating#restart-after-a-linux-package-update). Your config
and pairings survive all of this; [Uninstall](/docs/uninstall) clears them.
