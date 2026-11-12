---
title: Virtual displays
description: Choose what happens to your screens when a device connects — presets, keep-alive, your monitors, per-device settings and game sessions.
---

When a device connects, Punktfunk creates a virtual display at that device's resolution and
refresh rate and streams it. The **Displays** page of the [web console](/docs/web-console) decides
what happens around it. A change applies to the next connection.

The page shows your monitors and streamed screens to scale, with a dashed **Next device** where the
next device's screen lands. Under the map, **Screens** lists every monitor and streamed screen;
**When a device connects** says what the next device gets and holds the presets. The settings live
in `display-settings.json` in the host's config directory.

## Pick a preset

Click one under **When a device connects**; hover a preset to preview it on the map.

| Preset | For | After a device disconnects | Your monitors | A second device |
|---|---|---|---|---|
| **Default** | Most setups | Kept 10 s | Automatic | Gets its own screen |
| **Play together** | Friends playing or watching one screen | Kept 10 s | Automatic | Shares the screen |
| **Desk PC** | A PC you also use in person | Removed at once | Stay on | Gets its own screen |
| **Hot-desk** | One person, roaming between their devices | Kept 5 min | Turn off | Is told it's busy |
| **Workstation** | A multi-monitor daily driver, arranged by you | Kept 5 min | Turn off | Gets its own screen |
| **Headless box** | A machine with no monitor | Kept until released | Turn off | Takes over |

Hot-desk remembers display settings per device and resolution; the others per device.

**Save as preset…** stores the settings in force, including
[Dedicated game sessions](#dedicated-game-sessions), beside the built-ins. The buttons beside it
rename it, update it to the current settings, or delete it. The built-in presets leave Dedicated game
sessions as they are.

## Customise

**Customise** asks the questions below that your host acts on, including
[Dedicated game sessions](#dedicated-game-sessions). Each answer saves at once.

### After a device disconnects [#keep-alive]

- **Remove its screen**. A host does this until you choose otherwise.
- **Keep for** a number of seconds (up to 24 hours). A reconnect inside the window drops straight
  back in.
- **Keep until released**. The screen stays until you click **Release** on it in **Displays**, or
  **Release all kept**.

Stopping the stream on the client skips the **Keep for** window and removes the screen at once. A
dropped connection is noticed after the **Disconnect timeout** (8 s, an advanced setting in
**Host**), then the window starts.

**Keep for** keeps counting while the host sleeps: a host that sleeps past the window removes the
screen a few seconds after it wakes, unless a device reconnects first. On gamescope the game keeps
running as long as its screen does.

> **Keeping a screen with Turn off keeps your monitors dark after you disconnect**, until the
> window ends or you release it. On a PC you also use in person, pick **Desk PC**.

### Your monitors while streaming [#topology]

- **Stay on**: the stream is an extra screen beside your monitors.
- **Stay on, stream is main**: the stream becomes the main screen.
- **Turn off**: the stream is the only screen until streaming ends.
- **Automatic**: **Turn off**, or **Stay on** when `PUNKTFUNK_COMPOSITOR` pins a compositor.

| | KWin | GNOME | Sway · Hyprland · niri · Umbriel | Windows |
|---|---|---|---|---|
| Stay on | ✅ | ✅ | ✅ | ✅ |
| Stay on, stream is main | ✅ | ✅ | acts as Stay on | ✅ |
| Turn off | ✅ | ✅ | ✅ | ✅ |

- **Sway, Hyprland, niri and Umbriel** have no main screen. The host focuses the streamed screen at session start
  and before each library launch; a window that opens later follows whatever has focus then.
- **Hyprland** turns monitors back on with `hyprctl reload` when the screen is removed. That
  re-reads your config: changes made with `hyprctl keyword` are lost, and `exec =` lines run again.
- **niri** switches monitors off and on over its IPC when the screen is created and removed. If the
  host dies first, its next start lights them again.
- **Umbriel** switches monitors off and on over its IPC. Saving an output change in its config
  lights them mid-stream: see [Umbriel](/docs/umbriel#config-save).
- **KWin, Sway, Hyprland, niri and Umbriel** can keep chosen monitors lit under **Turn off**: set each to
  **Stay on** on its row in **Screens**.
- Punktfunk never turns off a screen it created, so a second device never goes dark.

### A second device connects

- **Gets its own screen**.
- **Takes over**: the first device's stream ends.
- **Shares the screen**: it joins the live screen at the size, refresh rate and HDR the first
  device set. [Play together](/docs/play-together) covers what the devices share.
- **Is told it's busy**.

The same device reconnecting always resumes. A Moonlight device that shares the screen gets the
picture fitted into the resolution it asked for, and the app picked in Moonlight doesn't start.
Only one Moonlight device streams at a time: **Shares the screen** turns a second one away, and
the other two hand the host over.

### Remember display settings

**Per device**, **Per device and resolution**, or **Shared**. Your desktop keys its scaling to the
identity each device gets: see [Persistent scaling](#persistent-scaling).

### Up to this many screens at once

Under **Advanced**: 1 to 16 (default 4). Several devices become monitors of one desktop, side by
side. Drag a streamed screen on the map to place it; the host then keeps each device where you put
it. **Arrange automatically**, under **Advanced**, puts them back in a row.

## Per-device settings

Open **Display settings…** from a device's row in **Devices**. Each question offers **Follow
host** or an answer for this device only:

- **Screen it streams** (Linux and Windows hosts): **Own screen**, or one of your monitors. **Own
  screen** keeps this device off a monitor the whole host streams. A device can also pick its
  screen in its own app ([Screen](/docs/client-settings#screen)); the sheet then says **Chosen on
  the device.** The latest pick wins, from either side;
- the keep-alive (including **Keep for** a number of seconds), second-device and remember
  questions above;
- **Your monitors while streaming**. On a Windows host the first device to connect decides it for
  every device that joins;
- **Largest screen this device gets**, such as `2560x1440@60`. The device is told the smaller mode
  when it asks for more, in its own screen's shape: a 2560×1600 device under `1920x1080@60` gets
  1728×1080;
- **Scale its screen at** (GNOME and niri hosts): the starting scale, until you change the scale in the
  desktop. On KDE, set the scale in System Settings while the device streams; KDE keeps it for
  that device.

**Follow the host in everything** removes the device's overrides.

## Stream a real monitor instead

To show every device one of your real monitors instead of a virtual screen, see
[Stream a real monitor](/docs/stream-a-monitor).

## Dedicated game sessions

**Linux hosts with gamescope installed.** The last question in **Customise**:

- **Auto** (default): a library launch runs in the session the box is in: Steam Game Mode, a bare
  gamescope, or your desktop.
- **Dedicated**: each library launch gets its own headless gamescope at the device's resolution
  and refresh, with only the game inside. With keep-alive, the game keeps running when you
  disconnect.

On a box in Steam Game Mode, a dedicated Steam launch borrows Game Mode's Steam and hands it back
when the session ends. Moonlight launches follow the same choice.

What happens to a game when it or its session ends is under
[When a game ends](/docs/when-a-game-ends).

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
| **niri** | Pick **Scale its screen at** in the device's **Display settings…**; the screen starts at it. |
| **Sway** | Not supported; set the scale in your sway config. |
| **Umbriel** | Not supported; the streamed screen is always at scale 1. |

## Troubleshooting

### My monitors stayed off after I disconnected

The screen is kept with **Turn off**. Click **Release** on its row in **Displays** → **Screens**,
or pick **Desk PC**.

### The streamed screen shows only my wallpaper

Your monitors **Stay on**, so the stream is an empty extra screen. Pick **Stay on, stream is main**
or **Turn off**.

### KWin can't create the virtual output

"Could not find output" has a few causes, by KWin version: see
[KDE Plasma](/docs/kde#troubleshooting).

### Keep-alive, monitors or layout settings do nothing

A real monitor is being streamed, and those settings only apply to virtual screens. In
**Screens**, click **Streamed to devices** on the streamed monitor.

### My couch box's TV stayed on the stream after I disconnected

**Headless box** keeps the screen until released. Click **Release** on its row in **Displays**,
return to Game Mode on the box, or restart the host.

A few display knobs in `host.env` apply only while the console has never saved a display setting;
[Configuration](/docs/configuration) lists them.
