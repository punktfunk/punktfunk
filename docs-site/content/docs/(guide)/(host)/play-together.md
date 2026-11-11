---
title: Play together
description: Let friends join the screen you stream — pick the preset, let them in, and what everyone shares.
---

Friends connect to your host from their own devices and share one screen: the same picture, the
same sound, and a controller each. Pick **Play together** for couch co-op, a party game, or
watching something together. To give each device a screen of its own, keep **Default**
([Virtual displays](/docs/virtual-displays#pick-a-preset)).

## Set it up

1. In the [web console](/docs/web-console), open **Displays** and click **Play together** under
   **When a device connects**. It applies from the next connection.
2. Let each friend in once. Their device's first connect waits in **Devices** under **Waiting for
   approval**. Click **Approve** and choose **Approve as guest**: Controller only, for 4 hours
   ([Pairing](/docs/pairing#choosing-access-when-you-admit-a-device)). To give more or less, see
   [Access levels](/docs/access-levels).
3. Connect your own device first, then your friends connect theirs.

A friend outside your home network needs a way in first, and a PIN instead of **Approve**: see
[Friends over the internet](/docs/friends-over-the-internet).

## What everyone gets

- **The screen.** The first device to connect sets its resolution and refresh rate. Every device
  after it sees that screen fitted to its own display.
- **The sound.** Everyone hears the same audio. To mute one device, use its row under **Now** on
  **Home**.
- **Controllers.** Every device can bring controllers. Each controller becomes its own player, in
  the order it is first used. The player picker on a device's row under **Now** changes its
  number ([Access levels](/docs/access-levels#couch-co-op-over-join)).
- **The pointer and keyboard.** Keyboard, mouse and touch from every device drive the one desktop
  pointer and keyboard, as far as each device's access allows.

## Things to know

- While more than one device shares the screen, its size stays put. A device that resizes its
  window or rotates rescales only its own picture. On a Linux host, a device left alone on the
  screen can resize it again; on a Windows host, reconnect that device.
- A device that joins is told at the start that it shares the screen.
- HDR follows the screen. If the screen is in HDR, a device that joins must have HDR on in its
  client, or it is told it can't join. If the screen is in SDR, everyone gets SDR, even on an HDR
  TV. See [HDR](/docs/hdr#hdr-on-a-shared-screen).
- On a gamescope or Game Mode host, only the screen's first device starts a game. A game picked on
  a joining device doesn't start. On other hosts it starts on the shared desktop.
- On a Windows host a joining device sees the pointer inside the picture. It doesn't see Windows
  permission prompts or the lock screen; the first device does.
- Two devices that connect at the same moment still share one screen.

## Troubleshooting

### A friend sees only the wallpaper, or a screen of their own

Their device's own setting outranks the preset. In **Devices**, open that device's **Display
settings…** and set **A second device connects** to **Follow host**.

### A friend is told the screen is in HDR

The screen is in HDR and their device didn't ask for it. Turn HDR on in their client
([HDR per client](/docs/hdr#per-client)). If their display can't show HDR, stop every stream, turn
HDR off on the device that connects first, and connect again.

### A friend's game didn't start

On a gamescope or Game Mode host, start the game from the first device; the friend sees it on the
shared screen. Elsewhere, a guest needs **Launch games** to start one
([Individual grants](/docs/access-levels#individual-grants)).
