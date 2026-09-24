---
title: Virtual displays
description: Choose what happens to your screens when a device connects — presets, keep-alive, your monitors, per-device settings and game sessions.
---

When a device connects, Punktfunk creates a virtual display at that device's resolution and
refresh rate and streams it. The **Displays** page of the [web console](/docs/web-console) decides
what happens around it. A change applies to the next connection.

The page shows your monitors and streamed screens to scale, a sentence saying what the next device
gets, the presets, the **Devices** list and **Your monitors**. The settings live in
`display-settings.json` in the host's config directory.

## Pick a preset

Click one; hover a preset to preview it on the map.

| Preset | For | After a device disconnects | Your monitors | A second device |
|---|---|---|---|---|
| **Default** | Most setups | Kept 10 s | Automatic | Gets its own screen |
| **Shared desktop** | A PC you also use in person | Removed at once | Stay on | Gets its own screen |
| **Hot-desk** | One person, roaming between their devices | Kept 5 min | Turn off | Is told it's busy |
| **Workstation** | A multi-monitor daily driver, arranged by you | Kept 5 min | Turn off | Gets its own screen |
| **Headless box** | A machine with no monitor | Kept until released | Turn off | Takes over |

Hot-desk remembers display settings per device and resolution; the others per device.

**Save as preset…** stores the settings in force, including
[Dedicated game sessions](#dedicated-game-sessions), as a card beside the built-ins. Its icons
rename it, update it to the current settings, or delete it. The built-in presets leave Dedicated
game sessions as they are.

## Customise

**Customise** asks five questions. Each answer saves at once.

### After a device disconnects [#keep-alive]

- **Remove its screen**.
- **Keep for** a number of seconds (up to 24 hours). A reconnect inside the window drops straight
  back in.
- **Keep until released**. The screen stays until you click **Release** on it in **Displays**, or
  **Release all kept**.

Closing the client removes the screen at once. A dropped connection is noticed after the
**Disconnect timeout** (8 s, an advanced setting in **Host → Settings**), then the window starts.
On gamescope the game keeps running as long as its screen does.

> **Keeping a screen with Turn off keeps your monitors dark after you disconnect**, until the
> window ends or you release it. On a PC you also use in person, pick **Shared desktop**.

### Your monitors while streaming [#topology]

- **Stay on**: the stream is an extra screen beside your monitors.
- **Stay on, stream is main**: the stream becomes the main screen.
- **Turn off**: the stream is the only screen until streaming ends.
- **Automatic**: **Turn off**, or **Stay on** when `PUNKTFUNK_COMPOSITOR` pins a compositor.

| | KWin | GNOME | Sway · Hyprland | Windows |
|---|---|---|---|---|
| Stay on | ✅ | ✅ | ✅ | ✅ |
| Stay on, stream is main | ✅ | ✅ | acts as Stay on | ✅ |
| Turn off | ✅ | ✅ | ✅ | ✅ |

- **Sway and Hyprland** have no main screen. The host focuses the streamed screen at session start
  and before each library launch; a window that opens later follows whatever has focus then.
- **Hyprland** turns monitors back on with `hyprctl reload` when the screen is removed. That
  re-reads your config: changes made with `hyprctl keyword` are lost, and `exec =` lines run again.
- **KWin** can keep chosen monitors lit under **Turn off**: set each to **Stay on** in **Your
  monitors**.
- Punktfunk never turns off a screen it created, so a second device never goes dark.

### A second device connects

- **Gets its own screen**.
- **Takes over**: the first device's stream ends.
- **Shares the screen**: it joins the live screen at that screen's resolution. Both hear the same
  audio; mute one from the **Sessions** card on **Home**.
- **Is told it's busy**.

The same device reconnecting always resumes. A Moonlight host serves one session: **Shares the
screen** turns the second client away, and the other two hand the host over.

### Remember display settings

**Per device**, **Per device and resolution**, or **Shared**. Your desktop keys its scaling to the
identity each device gets: see [Persistent scaling](#persistent-scaling).

### Up to this many screens at once

1 to 16 (default 4). Several devices become monitors of one desktop, side by side. Drag a streamed
screen on the map to place it; the host then keeps each device where you put it.

## Per-device settings

Click **Display settings…** on a device in **Displays** or **Devices**. Each question offers
**Follow host** or an answer for this device only:

- the keep-alive, second-device and remember questions above;
- **Your monitors while streaming** (Linux hosts);
- **Largest screen this device gets**, such as `2560x1440@60`. The device is told the smaller mode
  when it asks for more;
- **Scale its screen at** (Linux hosts): the starting scale on GNOME, until you change the scale in
  the desktop.

**Follow the host in everything** removes the device's overrides.

## Stream a real monitor instead

**Linux hosts.** In **Your monitors**, click **Stream this monitor** on a row. Every device then
sees that monitor instead of its own virtual screen. **Give each device its own screen** switches
back.

- The monitor is never touched, so keep-alive, **Your monitors** and layout don't apply.
- The resolution is the monitor's; the client scales the picture.
- Absolute mouse and pen input lands on that monitor.
- Works on KWin, GNOME, Sway, Hyprland and gamescope Game Mode, with no chooser dialog. A nested or
  headless gamescope has no monitor to list.

Monitors are named by connector (`HDMI-A-1`, `DP-2`). `punktfunk-host list-monitors` lists them,
and `mirror-test` checks the capture without a client ([Host CLI](/docs/host-cli#list-monitors)).
`punktfunk-host anchor-test --monitor HDMI-A-1` checks where absolute input lands.

To pin it from the host instead, set `PUNKTFUNK_CAPTURE_MONITOR=HDMI-A-1` in
[`host.env`](/docs/configuration). It wins over the console, which then shows the choice as locked.

## Dedicated game sessions

**Linux hosts with gamescope installed.** Under **Dedicated game sessions** on the **Displays**
page:

- **Auto** (default): a library launch runs in the session the box is in: Steam Game Mode, a bare
  gamescope, or your desktop.
- **Dedicated**: each library launch gets its own headless gamescope at the device's resolution
  and refresh, with only the game inside. With keep-alive, the game keeps running when you
  disconnect.

On a box in Steam Game Mode, a dedicated Steam launch borrows Game Mode's Steam and hands it back
when the session ends. Moonlight launches follow the same choice.

## When a game ends, and when a session does

**Linux and Windows hosts.** Under **When a game or a session ends**, in the same section. These
act only on a game the host launched for the session, never one you started yourself.

| Setting | Options (default first) |
|---|---|
| **When the game exits** | **End the session**: the client returns to its library · **Keep streaming** |
| **When the session ends** | **Leave it running** · **Close it on Stop**: closing the client closes the game, a dropped connection doesn't · **Always close it**: a dropped connection closes it too, after the reconnect window |
| **When you launch another game** | **Leave it running** · **Close it first**: only this device's earlier launch |
| **Reconnect window** | 300 s (10 s to 24 h). Reconnecting cancels it; **Home** shows the countdown and **End now**. |

Closing asks the game to quit like its window's close button, and forces it after 10 seconds.
Unsaved progress is lost, which is why nothing closes by default.

Keep-alive is how long the *screen* outlives a disconnect; the reconnect window is how long the
*game* does.

### On gamescope, keep-alive decides

A game in its own gamescope (a dedicated session, or a Steam Deck or Bazzite couch box) lives
exactly as long as its screen:

| You disconnect by | The game |
|---|---|
| **Stop** on the client or in the console | ends with the screen at once, even on **Leave it running** |
| a dropped connection | ends when the keep-alive window runs out |
| a dropped connection, **Keep until released** | keeps running; **Always close it** still ends it after the reconnect window |

To leave a game running on a gamescope box, keep its screen until released. On a desktop session
(KWin, GNOME, Sway) the settings above apply as written.

Hooks can react to `game.running` and `game.exited`: see [Automation](/docs/automation).

## Advanced Windows options

Under **Advanced** on a Windows host. They act while your monitors are turned off for a stream,
and each is undone when streaming ends, or at the next host start after a crash.

### Power monitors off (DDC/CI)

Off by default. Tells your monitors to switch their panels off, and back on afterwards. It stops
the stutter some setups get while a dark monitor keeps probing its inputs. Monitors without DDC/CI
are skipped. A monitor that stays dark: press its power button, then turn this off.

### Disable monitor devices (PnP)

On by default. Disables your monitors' Windows devices while streaming, so a standby monitor or TV
can't wake up mid-stream. Turn it off if a monitor fails to come back. After a crash, the next
host start re-enables them, or do it in Device Manager.

### Hold monitor identity (EDID)

AMD graphics only; shown where the AMD driver is installed. Keeps each sleeping monitor present to
the driver, like a dummy plug, which stops a rhythmic stutter every few seconds. If a monitor
misbehaves afterwards, turn this off and replug it.

## Persistent scaling

Set scaling once while streaming and each device gets it back on the next connection.

| Host | How |
|---|---|
| **Windows** | Set scaling in Settings while streaming; Windows remembers it per device. |
| **KDE Plasma** | Set it in System Settings while streaming; KWin reapplies it per device. |
| **GNOME** | Set it in Settings while streaming; the host saves it per device and reapplies it. |
| **Sway** | Not supported; set the scale in your sway config. |

## Troubleshooting

### My monitors stayed off after I disconnected

The screen is kept with **Turn off**. Click **Release** in **Displays**, or pick **Shared
desktop**.

### The streamed screen shows only my wallpaper

Your monitors **Stay on**, so the stream is an empty extra screen. Pick **Stay on, stream is main**
or **Turn off**.

### KWin can't create the virtual output

"Could not find output" has a few causes, by KWin version: see
[KDE Plasma](/docs/kde#troubleshooting).

### Keep-alive, monitors or layout settings do nothing

A real monitor is being streamed, and those settings only apply to virtual screens. Click **Give
each device its own screen** in **Your monitors**.

### The console won't let me pick a monitor

`PUNKTFUNK_CAPTURE_MONITOR` is set in `host.env`. Remove it and restart the host.

### A session fails with "no monitor named …"

The monitor you picked isn't connected, was renamed, or the host runs in another session. Run
`punktfunk-host list-monitors` and pick a listed one.

### My couch box's TV stayed on the stream after I disconnected

**Headless box** keeps the screen until released. Click **Release** in **Displays**, return to
Game Mode on the box, or restart the host.

A few display knobs in `host.env` apply only while the console has never saved a display setting;
[Configuration](/docs/configuration) lists them.
