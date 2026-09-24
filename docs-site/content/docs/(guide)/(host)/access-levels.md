---
title: Access levels
description: Choose what each paired device may do and for how long — presets, single grants, temporary access, couch co-op, and what access doesn't cover.
---

Give each paired device only what it needs: a friend's phone as a second controller for the
evening, a TV that plays but never types, a spectator who only watches. The host enforces it, and
nothing a client sends can widen its own access. A friend outside your LAN also needs a way in:
see [Friends over the internet](/docs/friends-over-the-internet).

Pick the access when you
[approve a device or arm a PIN](/docs/pairing#choosing-access-when-you-admit-a-device). Change it
later in the console under **Devices** → **Paired devices** → **Edit access**; the **Access**
column shows each device's level and time left.

## The three presets

| Access level | The device can |
|---|---|
| **Full control** | Do everything: keyboard, mouse, controllers, clipboard, microphone, launching games, host power. A plain **Approve** gives this. |
| **Controller only** | Send controller input. Its pads appear as extra controllers, with rumble and pad audio. |
| **View only** | Watch and listen, and send nothing. |

A hand-picked mix shows as **Custom**.

## Individual grants

Under **Advanced** in the access dialog:

| Grant | Covers |
|---|---|
| **Controller** | Buttons, sticks and motion, the host's virtual pads, rumble and pad audio. |
| **Mouse, touch & pen** | Mouse, scroll, touch and pen input. |
| **Keyboard** | Key presses. |
| **Clipboard** | The [shared clipboard](/docs/clipboard), when the host's own clipboard setting allows it too. Without it the client's clipboard control is greyed out. |
| **Microphone** | Sending the client's microphone to the host. |
| **Launch games** | Starting a title from the [library](/docs/game-library) when connecting. Without it, such a connect is refused with a message; the library stays visible. |
| **Host power** | Sleep, restart and shut down the host from the client ([Host power](/docs/host-power)). Full control includes it, since keyboard access reaches the desktop's power menu anyway. |

**Controller only** leaves out **Launch games**, so you choose what runs. Tick it to let a guest
pick games.

## Temporary access

**Access expires** takes **Never**, **In 1 hour**, **In 4 hours**, **In 8 hours**, **Until they
disconnect**, or **Custom…** hours. **Approve as guest** is Controller only for 4 hours.

- Times run on the host's clock.
- A streaming device gets warnings 5 minutes and 1 minute before, then its sessions end with "Your
  access to this host has expired." Other devices keep streaming.
- **Until they disconnect** removes the pairing a minute after the device's last session ends. A
  reconnect inside that minute is free.
- An expired device stays listed as **Expired**. Its next connect waits under **Waiting for
  approval**; approve it again with one click.
- A new expiry, **Expire now**, a changed level or **Unpair** reaches live sessions within
  moments.

Access belongs to the device: two sessions from one device share it.

## Couch co-op over JOIN

To add a second player to the game you're streaming:

1. Pair the guest's device **Controller only**.
2. In its **Display settings…**, set **A second device connects** to **Shares the screen**
   ([Per-device settings](/docs/virtual-displays#per-device-settings)).
3. The guest connects. The **Sessions** card on **Home** lists them as **Joined another session**,
   and their pads arrive as extra controllers on your desktop.

The **Sessions** card picks **Player 1–4** for each session, and the device keeps that player on
its next connect. A pad that is already plugged in keeps its slot until it reconnects. A slot
another live session asked for first stays theirs. Clients show *player 2* in the
[stats overlay](/docs/stats).

## Changing a live session

The **Sessions** card on **Home** also has an access picker per session: hand a view-only friend
the controller, take it back when your turn comes. It changes that session only, never beyond
what the device is paired for, and ends with the session. Resolution, bitrate and keyframe
requests are never restricted; they shape only that device's own stream.

## What access doesn't cover

> **A view-only guest still sees your whole desktop.** Access governs what a device sends in, not
> what it sees. A guest on a shared desktop watches and hears everything, notifications included.

- **Moonlight devices** always have full control. The console shows **Full (ungoverned)**.
- **Older Punktfunk clients** are enforced the same way but can't explain it: no access chip, no
  expiry warnings, and an ungranted keyboard does nothing. If a guest says their keyboard is dead,
  check their access, then their client version.

Current clients stop capturing what can't land, hide ungranted controls, show the access and time
left in the [stats overlay](/docs/stats), and show expiry warnings whatever the overlay is set to.

## Where enforcement happens

The host checks every input event against the session's grants before injecting it, and never
sets up an ungranted plane: no controller grant, no virtual pads; no microphone grant, no mic.
Pairing a device again keeps its access. Only the console, behind its login, can widen a grant.
Refused input is logged once per session and kind. See [Security & Safe Use](/docs/security).
