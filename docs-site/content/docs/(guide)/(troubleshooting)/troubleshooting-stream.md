---
title: Picture & session
description: Fixes for a stream that connects with the wrong picture — black screen, rhythmic freezes, stutter, Game Mode and games on the wrong monitor.
---

Fixes for a stream that connects but shows the wrong picture.

## Linux

### Black screen / no picture, but the client connects

The host can't capture this session. Check:

- You're logged into a Wayland session, not X11. Pick it on the login screen.
- The compositor meets its floor: [Requirements → Desktop](/docs/requirements#desktop).
- `host.env` has no `PUNKTFUNK_COMPOSITOR` line. The host finds the live compositor itself; a pin
  aims it at one backend even when another session runs.
- Hyprland with `ecosystem.enforce_permissions` on: grant the host
  ([Hyprland → Permission system](/docs/hyprland#permission-system)).

### Capture fails: "Session creation inhibited" (GNOME)

A locked GNOME session blocks capture. Turn the lock off:
[GNOME → Do not lock the session](/docs/gnome#do-not-lock-the-session).

### Games from my library open on a physical monitor, not on the stream (Hyprland / sway)

A new window opens on the focused monitor, and something took focus from the stream: a click on a
physical monitor, or a launcher (Steam Big Picture, Heroic) that opens the game window later.

- Leave the host's keyboard and mouse alone until the game is up.
- Set **Displays** → **Customise** → **Your monitors while streaming** → **Turn off**. Every window
  then lands on the stream.
- Or give each launch its own session: **Displays** → **Dedicated game sessions** → **Dedicated**
  (needs `gamescope`; see [Dedicated game sessions](/docs/virtual-displays#dedicated-game-sessions)).

### Game Mode: black screen on connect, or the stream is stuck at the box's resolution

The host mirrored the box's own screen instead of taking the session over. The checks and the fix:
[gamescope → Troubleshooting](/docs/gamescope#the-stream-is-black-on-connect-or-stuck-at-the-boxs-resolution).

## Windows

### Black screen with sound (Windows)

The virtual display driver isn't answering, isn't installed, or doesn't match the host. The console's
**Troubleshooting** page says which under **Virtual display driver**.

- Not answering: run `punktfunk-host service restart` from an elevated PowerShell. RivaTuner
  Statistics Server (MSI Afterburner) is a known cause; close it before you connect again.
- Missing or out of date: reinstall the host. Setup brings the matching driver.

### The picture freezes for a moment, over and over (Windows)

A freeze on a steady rhythm is a display or driver disturbance, not bandwidth. While it happens,
**Home** → **Live status** → **Capture health** reads `stalled`.

Search the log on the console's **Troubleshooting** page for `METRONOMIC`. The line names the cause
and its cures:

- **…coincide with Windows monitor hot-plug/re-enumeration events**: a display, cable, switch or
  AVR re-probes its link. Turn off the display's auto input scan (on a TV also instant-on and CEC),
  unplug it at the GPU, or fit a dummy plug.
  [Disable monitor devices (PnP)](/docs/virtual-displays#disable-monitor-devices-pnp) stops
  Windows reacting.
- **…with NO coinciding OS display event**: the cause is below Windows, such as a sleeping screen the
  GPU driver services, display-poller software (SteelSeries GG, SignalRGB) or the refresh rate. Try
  another refresh rate. A laptop panel settles with **Displays** → **Customise** → **Your monitors
  while streaming** → **Stay on, stream is main**.

A freeze with no steady rhythm logs `REPEATING without a stable period` with the same fields.

## Any host

### Stutter, drops, or high latency

The link can't carry the stream's bitrate.

- Set **Bitrate** to **Automatic** so it adapts, or pick a lower fixed rate. The host menu's speed
  test suggests one. In Moonlight, lower it in Moonlight's settings.
- Use a wired connection or 5 GHz Wi-Fi.
- Several devices streaming at once share one encoder.

The [stats overlay](/docs/stats) shows which stage is late.

### The stream doesn't use the codec or HDR I picked

The host declined part of the request and told your client.
[When the client and the host disagree](/docs/client-settings#when-the-client-and-the-host-disagree)
lists what it does with each setting.
