---
title: When a game ends
description: Choose what the host does when a launched game exits, when its session ends and when you launch another, and how keep-alive decides it on gamescope.
---

Choose whether a stream ends with its game, and whether a game closes when you disconnect.

## When a game ends, and when a session does

**Linux and Windows hosts.** In **Host → Session** in the web console. These act only on a game the
host launched for the session, never one you started yourself.

| Setting | Options (default first) |
|---|---|
| **When the game exits** | **End the session**: the client returns to its library · **Keep streaming** |
| **When the session ends** | **Leave it running** · **Close it on Stop**: closing the client closes the game, a dropped connection doesn't · **Always close it**: a dropped connection closes it too, after the reconnect window |
| **When you launch another game** | **Leave it running** · **Close it first**: only this device's earlier launch |
| **Reconnect window** | 300 s (10 s to 24 h). Reconnecting cancels it; **Home** shows the countdown and **End now**. |

Closing asks the game to quit like its window's close button, and forces it after 10 seconds.
Unsaved progress is lost, which is why nothing closes by default. A player can still close one:
see [Ending a game](/docs/game-library#ending-a-game).

[Keep-alive](/docs/virtual-displays#keep-alive) is how long the *screen* outlives a disconnect;
the reconnect window is how long the *game* does.

### On gamescope, keep-alive decides

A game in its own gamescope (a [dedicated session](/docs/virtual-displays#dedicated-game-sessions),
or a Steam Deck or Bazzite couch box) lives
exactly as long as its screen:

| You disconnect by | The game |
|---|---|
| **Stop** on the client or in the console | ends with the screen at once, even on **Leave it running** |
| a dropped connection | ends when the keep-alive window runs out |
| the host going to sleep | as a dropped connection, and the sleep counts against the window |

With **Keep until released** the screen outlives every disconnect, Stop included, so the game keeps
running; **When the session ends** can still close it. To leave a game running on a gamescope box,
keep its screen until released. On a desktop session (KWin, GNOME, Sway) the settings above apply
as written.

Hooks can react to `game.running` and `game.exited`: see [Automation](/docs/automation).
