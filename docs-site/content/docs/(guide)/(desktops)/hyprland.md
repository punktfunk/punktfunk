---
title: Hyprland
description: Stream a Hyprland session through xdg-desktop-portal-hyprland, and fix portal routing, permissions and black streams.
---

Stream a Hyprland session. Install the host first: [Arch](/docs/arch), [Ubuntu](/docs/ubuntu) or
[Fedora](/docs/fedora); on Omarchy, follow [Omarchy](/docs/omarchy).

## What works

- Each client gets its own headless output at its exact mode. The host captures it straight from
  Hyprland on the GPU, or through **xdg-desktop-portal-hyprland** (xdph) when it can't. No share
  dialog appears.
- Keyboard and mouse go in through the virtual pointer and keyboard protocols.
- Library launches open on their own workspace on the streamed output.
- A reconnect inside the [keep-alive](/docs/virtual-displays#keep-alive) window gets the same
  output back; `ctl display` lists it. When the output goes, its windows move to a remaining
  monitor.
- A `hyprctl reload` mid-stream keeps the client's mode.

It needs a running Hyprland session and xdph. If another portal backend (gtk, wlr) is installed
too, route ScreenCast to xdph in `~/.config/xdg-desktop-portal/hyprland-portals.conf`, then run
`systemctl --user restart xdg-desktop-portal`:

```ini
[preferred]
default=gtk
org.freedesktop.impl.portal.ScreenCast=hyprland
```

## host.env

Nothing is required: the host finds Hyprland on every connect.

Don't set `PUNKTFUNK_COMPOSITOR`: a pin turns off detection and session following. Every other
option is in [Configuration](/docs/configuration).

The host adds its picker to `~/.config/hypr/xdph.conf` and restarts xdph when that changes. A
picker you configured there still runs for every share the host didn't ask for.

## Start the host

From inside your Hyprland session:

```sh
systemctl --user enable --now punktfunk-host
journalctl --user -u punktfunk-host -f    # watch it start and print its fingerprint
```

Open [the web console](/docs/web-console) to pair a [client](/docs/clients). To serve Moonlight
too, see [What the unit starts](/docs/running-as-a-service#what-the-unit-starts).

## Headless session

There is no packaged headless Hyprland session. Log in on the box, or autologin through your
display manager, and enable lingering so the host starts at boot:
`sudo loginctl enable-linger "$USER"`.

## Known limits

- The streamed output runs at scale 1.
- SDR only: HDR on Linux needs [gamescope](/docs/gamescope#hdr-on-gamescope) — see
  [HDR](/docs/hdr#other-linux-desktops).
- Under **exclusive** [topology](/docs/virtual-displays#topology) your monitors come back through
  `hyprctl reload` when the display is released. That reload also drops runtime `hyprctl keyword`
  changes and re-runs `exec =` lines. A reload during the stream lights them early.

## Troubleshooting

### "hyprctl not reachable"

The host can't find the Hyprland instance. Start it from inside the session, as above.

### Black stream, and the host says the output "stayed 0x0"

Hyprland couldn't allocate a buffer for the headless output; its log shows
`GBM: Failed to allocate a GBM buffer`. This is a GPU driver or nested-session problem. Run
Hyprland as a real session and check your driver's GBM support.

### Black stream, and xdph logs "unavailable cursor mode 4"

A host that predates the cursor-mode check asked xdph for a cursor mode it doesn't offer. Update
the host, or switch the client to **game** [mouse mode](/docs/input#mouse-modes) meanwhile. If the
pointer misbehaves on an xdph that does offer it, set `PUNKTFUNK_PORTAL_CURSOR_MODE=embedded`.

### Black frames with `enforce_permissions` on [#permission-system]

Hyprland's permission system (0.49+, off by default) denies screen capture silently: the stream
shows black frames. The host logs a warning at startup when it's on. Grant `screencopy` to the host
and to xdph, which the host falls back to, with the paths your packages installed. In
`hyprland.lua`:

```lua
hl.permission("/usr/bin/punktfunk-host", "screencopy", "allow")
hl.permission("/usr/(lib|libexec|lib64)/xdg-desktop-portal-hyprland", "screencopy", "allow")
```

or in `hyprland.conf`:

```ini
permission = /usr/bin/punktfunk-host, screencopy, allow
permission = /usr/(lib|libexec|lib64)/xdg-desktop-portal-hyprland, screencopy, allow
```

Restart Hyprland afterwards; it reads permissions only at startup. The virtual pointer needs no
grant. Keyboards are allowed by default; a `keyboard` rule that denies new keyboards also blocks
the host's.

More fixes in [Troubleshooting](/docs/troubleshooting).
