---
title: Plugin setup
description: Finish setting up the plugins that need more than an install — ROM Manager, Playnite, VirtualHere, Game slots and RTSS Frame Cap.
---

Finish setting up a plugin you [installed](/docs/plugins#install-from-the-console) that needs more
than the install: ROM Manager, Playnite, VirtualHere, Game slots or RTSS Frame Cap.

## ROM Manager

Point it at your ROM folders or a [RomM](https://romm.app) server. Each game launches in the best
emulator this host has for its system, after first-run setup and its firmware. A missing emulator
is one **Ask to install** away; you allow it under **Library → Sources**. A RomM game downloads
on its first launch with its updates and DLC, and its saves go back to the server. Install
**Libretro** for box art and details, and **SteamGridDB** with an API key for the games Libretro
has no art for.

On a box with seats, each seat plays these games with the box's emulators and keeps its own saves;
[Profiles](/docs/profiles#on-linux) names the exceptions on Linux. On Windows, RPCS3, Snes9x and
Supermodel keep one set for everyone. So does an emulator installed while seats were off or by an
earlier version, until it is reinstalled under **Library** → **Sources** → **Emulators**.

The console doesn't install, prepare or remove emulators, or move saves, inside a profile that
uses an existing Linux account. On an existing Windows account it adds no emulator copy by hand.

Add folders and servers on the console's **ROM Manager** page, under **Sources**. A server signs
in with a code you approve in RomM. Without the console, write
`plugin-state/rom-manager/config.json` in the config directory (`~/.config/punktfunk` or
`%ProgramData%\punktfunk`):

```jsonc
{
  "roots": [
    { "dir": "/mnt/roms" },
    { "dir": "/mnt/more/ps1-games", "platform": "psx" }
  ]
}
```

A folder of system folders (`snes`, `psx`, `ps2`, …) needs nothing else; a folder of one system's
games names it. Firmware goes in `bios/<system>/` beside the system folders. On Windows, write
paths as `D:\\roms`. Options:
[the plugin's repository](https://git.unom.io/unom/punktfunk-plugin-rom-manager).

## Playnite

Mirrors your [Playnite](https://playnite.link) library; launching a title hands it back to
Playnite. It has two parts, both on the Windows host:

1. The plugin, from an elevated PowerShell: `punktfunk-host plugins add playnite`.
2. The **Punktfunk Sync** extension in Playnite: download `punktfunk-sync.pext` from the
   [plugin's builds](https://git.unom.io/unom/punktfunk-plugin-playnite/actions), double-click it
   and restart Playnite.

The console's **Playnite** page then shows "Exporter connected", and games sync within seconds of a
library change. Filters are on that page. Details:
[the plugin's repository](https://git.unom.io/unom/punktfunk-plugin-playnite).

## VirtualHere (USB passthrough)

Hands a physical USB device on your couch machine (a wheel, HOTAS, pedals, an arcade stick) to the
host while you play, so the game sees the real device. It drives
[VirtualHere](https://www.virtualhere.com/), a commercial product you buy and install yourself;
Punktfunk bundles none of it and isn't affiliated with it.

You need both halves of VirtualHere running first:

- **The USB Server on the couch**, sharing the device. One device is free; more, or running the
  client as a service, needs a licence. Servers exist for Windows, Linux, macOS and Android, not
  for iOS or tvOS.
- **The USB Client on the host**, ideally as a service.

```sh
punktfunk-host plugins add virtualhere
```

On the console's **VirtualHere** page, **Devices** lists what the couch shares; pick one to add a
rule that matches it by name. By default the device moves to the host when video starts and back
when it stops; you can widen that to the session or to the whole time a client is connected. If
nothing happens, the **Diagnostics** tab names the part to fix. Details:
[the plugin's repository](https://git.unom.io/unom/punktfunk-plugin-virtualhere).

### Recipe: full controller passthrough (VirtualHere)

To give the host the real device (DualSense gyro and adaptive triggers, a racing wheel, a HOTAS),
hand it over USB with [VirtualHere](https://www.virtualhere.com/), a commercial product sold
separately.

1. Set up the plugin as above. It finds the device by name, hands it back if anything crashes,
   and tells you which side is misconfigured.
2. On the couch client, turn off **Forward controllers**
   ([Client settings](/docs/client-settings#input)). Otherwise games see two controllers, and on
   Linux and Windows VirtualHere can't take a pad the client holds.

Without the plugin, two [hooks](/docs/automation#hooks) do the basic job. The address comes from `vhclientx86_64 -t LIST`;
it changes when the couch reboots, and an abnormal end leaves the device on the host:

```json
{ "hooks": [
    { "on": "stream.started", "run": "vhclientx86_64 -t \"USE,couch-deck.11\"" },
    { "on": "stream.stopped", "run": "vhclientx86_64 -t \"STOP USING,couch-deck.11\"" }
] }
```

## Game slots

Keeps a copy of a game's config or save files for each device that streams it and puts that copy
in place before the game starts. Play at the PC with the monitor's settings, on the Steam Deck with
the Deck's, on the TV with the TV's.

1. Install **Game slots** from **Plugins**.
2. Open the game in the **Library** and pick its **Game slots** tab.
3. Turn on **Keep a slot per device** and add the game's config or save folders. On Windows, write
   the full `C:\Users\<you>\…` path.
4. **Save**. The console gives the plugin write access to each folder you added.

**A slot per settings preset** gives one device a slot for each of its presets, like *Docked* and
*Handheld*. The launch waits while the plugin swaps the files, up to two minutes; the device shows
its launch screen meanwhile. When the game exits, the plugin saves the device's files into its slot
and puts the PC's own back. A device's first launch starts from the PC's files.

It only swaps games a client launches from its library, never one already running, and only for
paired devices. A folder that Steam Cloud syncs raises Steam's conflict dialog after a swap: track
the game's config instead, or turn Steam Cloud off for that game. Details:
[the plugin's repository](https://git.unom.io/unom/punktfunk-plugin-game-slots).

## RTSS Frame Cap

Caps the game at the refresh rate the client streams at, through
[RivaTuner Statistics Server](https://www.guru3d.com/download/rtss-rivatuner-statistics-server-download/),
and puts your own limit back when the stream ends. RTSS is a separate download; Punktfunk bundles
none of it.

1. Install RTSS and turn on **Start with Windows** in its main window. RTSS enforces the cap
   inside the game, so it must be running when the game starts.
2. Install **RTSS Frame Cap** from **Plugins**, or run `punktfunk-host plugins add rtss`.
3. Let the plugin write RTSS's profiles. Once, from an elevated PowerShell:

   ```powershell
   icacls "C:\Program Files (x86)\RivaTuner Statistics Server\Profiles" /grant "*S-1-5-19:(OI)(CI)M" "*S-1-15-2-1251237843-3372458582-2949526732-893382375-1396029850-1946794802-2851191480:(OI)(CI)M"
   ```

   The first SID is the runner's account, the second the plugin's own sandbox; a write needs both.
   The console can't do this step: it never gives a plugin write access inside Program Files.
4. Start the stream, then the game. A game that was already running keeps the limit it started
   with.

Nothing capped? Open **Diagnostics** → **Troubleshooting** and pick the `plugin:rtss` source: each missing step is a
`FAIL` line with the command that fixes it, and each stream start logs `rtss: 120 fps for …`. A
per-game profile in RTSS overrides the cap. Details and the `doctor` command:
[the plugin's repository](https://git.unom.io/unom/punktfunk-plugin-rtss).
