---
title: Stream a real monitor
description: Show every device one of your real monitors instead of a virtual display — pick it in the console or pin it in host.env.
---

Pick one of your monitors on the **Displays** page of the [web console](/docs/web-console), and
every device sees that monitor instead of a [virtual display](/docs/virtual-displays) of its own.

## Stream a real monitor instead

**Linux and Windows hosts.** In **Screens**, click **Stream this monitor** on a monitor's row. Every
device then sees that monitor instead of its own virtual screen, and the button reads **Streamed to
devices**. Click it again to give each device its own screen.

A device's own pick outranks this, whether made in its app or in its
[display settings](/docs/virtual-displays#per-device-settings). One device streams a monitor at a time: a device set to
a monitor another device is streaming gets its own screen for that session.

- The monitor is never touched, so keep-alive, **Your monitors while streaming** and layout don't
  apply.
- The resolution is the monitor's; the client scales the picture.
- Absolute mouse and pen input lands on that monitor.
- Works on KWin, GNOME, Sway, Hyprland, niri (stock niri too), Umbriel and gamescope Game Mode, with no
  chooser dialog. A nested or
  headless gamescope has no monitor to list.
- On Windows the pointer is part of the picture, and Windows permission prompts and the lock
  screen are not: the stream keeps showing the desktop behind a prompt, while your input goes to
  the prompt. A device that connects while the PC is locked or signed out gets a virtual screen
  instead, so you can sign in; the monitor is streamed again from the next connection.

On Linux, monitors are named by connector (`HDMI-A-1`, `DP-2`); `punktfunk-host list-monitors`
lists them ([Host CLI](/docs/host-cli#list-monitors)). On Windows they carry the Windows display
name (`\\.\DISPLAY1`), which the console shows.

To pin it from the host instead, set `PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1` in
[`host.env`](/docs/configuration). It wins over the console, which then shows the choice as locked.

## Troubleshooting

### The console won't let me pick a monitor

`PUNKTFUNK_CAPTURE_MONITOR` is set in `host.env`. Remove it and restart the host.

### A device streams its own screen instead of the monitor I picked

The monitor isn't connected, was renamed or switched off, another device is streaming it, or the
host runs in another session. The host log names the reason. Run `punktfunk-host list-monitors` and
pick a listed one, or wait for the other device to finish.
