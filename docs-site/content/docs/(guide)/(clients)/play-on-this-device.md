---
title: Play on this device
description: Play a host's games on the PC it runs on, with no stream — what you need, what the host's card shows, and how Steam's Game Mode, controllers and emulators fit.
---

Start a game from the Punktfunk app on the PC that runs your host, and it plays on that PC's own
screen with no stream. It is for the box under the TV that already runs a host and should be the
console too.

There is nothing to switch on in the app. An app on the same machine as its paired host plays that host's
games there; every other host streams.

## What you need

- A [host](/docs/install) running on this machine, and the Linux app, the Windows app, the
  controller interface or the [`punktfunk` command](/docs/clients#scripting-the-punktfunk-cli) on
  the same machine, [paired](/docs/pairing) with it.
- **Launch games** in that app's [access](/docs/access-levels#individual-grants). **Full control**
  has it.
- A screen to play on:

| Host | The screen it needs |
|---|---|
| Windows | Someone signed in at the PC. The game starts for them. |
| Linux desktop | A running KDE Plasma, GNOME, Hyprland, sway or niri session. |
| Linux, your own gamescope | The app running inside it: [gamescope](/docs/gamescope#play-from-a-gamescope-of-your-own). |
| Linux, Steam's Game Mode | [Steam's Game Mode](#steams-game-mode) on screen. |

Games play as the owner profile. A [seat](/docs/profiles) has no screen of its own: its player
streams from another device. With [Reachable without logging in](/docs/profiles#on-linux) on, games
play at the box while you are signed in there.

## Start a game

1. Open the app on that PC. The host's card reads **This device**.
2. Select the card to open its library, then start a game. A game that downloads first shows its
   progress.
3. The app reads **Starting the game…**. When the game is up, the app steps aside: it draws
   nothing and lets go of your controllers, so the game has them.

Back from **Starting the game…** leaves that screen and cancels nothing. From a terminal,
`punktfunk launch <host> --game <id>` starts the game, and `--exec` waits until it ends.

## While it plays

Quit the game as you always do. When it ends, the app is back on that title. If the host can't
start it, the app says why: see [Troubleshooting](#troubleshooting).

Switch back to the controller interface's window while the game runs and it shows **Playing on
this device** with **End game**. It takes your controllers back to show that, and lets go again
when you switch away. The Linux and Windows apps keep their library, which marks the game as
running.

**End game** closes the game like any other: [Ending a game](/docs/game-library#ending-a-game).
The web console's **Home** lists it as **On this PC's screen**, with **End now**. A game at the
desk ends only when named: a call that ends every game nobody streams, such as
`punktfunk-host ctl end-game`, leaves it.

Launches and play time count in the title's [stats](/docs/game-library#play-stats) and for the
owner profile. The host runs the same game events and launch holds as for a stream, with the plane
`local`: see [Host events](/docs/host-events#a-game-at-the-desk). It runs no per-app prep steps. Of
the [session settings](/docs/when-a-game-ends#at-the-desk), only **When you launch another game**
acts.

## What the card offers

The card reads **This device** while the host runs, and **Playing** followed by the title while a
game does. Its library, launchers, installs, **End game**, hiding a game, host power, sending logs and
links stay.

Nothing that streams or wakes it is offered: no **Connect with…**, **Wake**, presets, screen
choice, clipboard or network speed test, and no **Desktop** tile.

## Another device streams

Punktfunk doesn't guard the screen against a stream from another device. The default **Your
monitors while streaming** switches the box's monitors off for the stream, and the game at the desk
loses them. To share the box, pick the **Desk PC** preset ([Virtual
displays](/docs/virtual-displays#pick-a-preset)) or give each person a
[profile](/docs/profiles).

## Steam's Game Mode

In Game Mode every game starts through Steam, so Steam shows it and Steam Input stays.

- **A Steam game**, your own non-Steam entries included, is started by the running Steam.
- **Any other game** needs an entry of its own in Steam's library. The Decky plugin adds them from
  **Games in Steam** ([Steam Deck](/docs/steam-deck#games-in-steam)). Without one the app says
  **This game isn't in Steam yet. Add its source to Steam in the Punktfunk panel.**

An entry runs in Steam's own tree under the game's name and art, with its own Steam Input layout.
It starts from the app, or from Steam's library with no Punktfunk on screen, and exiting it from
Steam ends it. The host still tracks it and credits its play time. The host must be running when
you start an entry from Steam.

Steam keeps one controller layout per name. Two games with the same name, such as one game on two
platforms, share one layout. A game named like one you stream shares that stream's Punktfunk
layout, touch included. An entry gets no Punktfunk layout of its own: Steam's default applies,
then yours.

## Controllers and emulators

The game reads your controllers directly. For an emulator, the host sets the players:

| Host | Emulator players |
|---|---|
| Linux | The controllers plugged into the box, in plug order: the first is player 1. Only for controllers the host can name the way the emulator does. With one it can't, such as a Joy-Con, none are set and the emulator keeps its own. |
| Windows | None set. The emulator keeps its own. |

The order of a mix, a DualSense beside an Xbox controller, isn't promised: on an emulator that goes
by index, set the players in the emulator. In a Steam entry with Steam Input on, the game sees
Steam's virtual controllers, which the host can't name: none are set and the emulator keeps its
own. With Steam Input off for the entry, the players are the physical controllers.

## Steam on a desktop

Steam running on a desktop applies its desktop layout to a controller while a game it didn't
start is in front, and the game can receive keys and clicks from it. A game the host starts is
such a game. Start it from Steam instead, or turn Steam's desktop layout off in its controller
settings.

## Troubleshooting

### The card doesn't read This device

The app found no host on this machine that answers as the one it is paired with. Start the host
and [pair](/docs/pairing) this app with it. A host reached only through a forwarded port is
streamed to, not played here.

### This device isn't allowed to start games on this host.

The app's access lacks **Launch games**, or the owner profile is
[locked](/docs/profiles#lock-a-profile) and this device isn't on its list. Change the access under
**Devices** → **Edit access** in the web console, or add the device to the profile's list.

### This host has no screen of its own to play on.

A seat has none. Neither has a box with **Reachable without logging in** on while nobody is signed
in at it. Sign in at the box, or stream from another device.

### Nobody is signed in on this PC. Sign in, then start the game again.

A Windows host starts games for the signed-in user. Sign in at the PC.

### No desktop is running on this host. Sign in, then start the game again.

A Linux host needs a running desktop session, or a gamescope. Sign in at the box.

### This game isn't in Steam yet. Add its source to Steam in the Punktfunk panel.

Game Mode starts Steam games and the entries the Decky plugin added. Switch the game's source on
under **Games in Steam**.

### Update Punktfunk on this device to play here.

The host on this device is older than the app. [Update](/docs/updating) it.

### This is this device's own host — there is nothing to stream.

The app was asked to stream to the host on its own machine. Start a game from its library instead.
