---
title: Sway / wlroots
description: Stream a sway or scroll session through xdg-desktop-portal-wlr, and fix portal routing and black streams.
---

Stream a sway session, or one on scroll, a sway fork. Install the host first: [Arch](/docs/arch),
[Ubuntu](/docs/ubuntu) or [Fedora](/docs/fedora). On Hyprland, see [Hyprland](/docs/hyprland).

## What works

- Each client gets its own headless output at its exact mode. The host captures it straight from
  the compositor on the GPU where it supports that, or through **xdg-desktop-portal-wlr** (xdpw).
  No chooser appears.
- Keyboard and mouse go in through the wlroots virtual pointer and keyboard protocols.
- Library launches open on their own workspace on the streamed output.
- A reconnect inside the [keep-alive](/docs/virtual-displays#keep-alive) window gets the same
  output back; `ctl display` lists it.

It needs sway 1.8 or newer, or scroll, and xdpw. The host drives the whole video side through
`swaymsg` (`scrollmsg` on scroll), so other wlroots compositors such as River or dwl can't stream.
It finds the running sway on every connect; you don't export `SWAYSOCK`.

If another portal backend (gtk, gnome) is installed too, route ScreenCast to xdpw in
`~/.config/xdg-desktop-portal/sway-portals.conf` (`scroll-portals.conf` on scroll), then run
`systemctl --user restart xdg-desktop-portal`:

```ini
[preferred]
default=gtk
org.freedesktop.impl.portal.ScreenCast=wlr
```

## host.env

Nothing is required: the host finds sway on every connect.

Don't set `PUNKTFUNK_COMPOSITOR`: a pin turns off detection and session following. Every other
option is in [Configuration](/docs/configuration).

The host writes its own chooser into `~/.config/xdg-desktop-portal-wlr/config` and restarts xdpw
when that changes.

## Start the host

From inside your sway session:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f    # watch it start and print its fingerprint
```

Open [the web console](/docs/web-console) to pair a [client](/docs/clients). To serve Moonlight
too, see [What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

## Headless session

There is no packaged headless sway session. Log in on the box, or autologin through your display
manager, and enable lingering so the host starts at boot:
`sudo loginctl enable-linger "$USER"`.

## Known limits

- sway gets far less testing than [KDE](/docs/kde) and [GNOME](/docs/gnome); scroll is untested on
  real hardware.
- SDR only: HDR on Linux needs [gamescope](/docs/gamescope#hdr-on-gamescope) — see
  [HDR](/docs/hdr#other-linux-desktops).

## Troubleshooting

### "swaymsg get_outputs (is the host inside the sway session env — SWAYSOCK?)"

The host found no sway to talk to: it runs outside the session, or the compositor has no sway IPC.
Start the host from inside a sway session.

### Black stream, and xdpw logs "unsupported cursor mode requested, cancelling"

A host that predates the cursor-mode check asked xdpw for a cursor mode it refuses. Update the
host, or switch the client to **game** [mouse mode](/docs/input#mouse-modes) meanwhile.

More fixes in [Troubleshooting](/docs/troubleshooting).
