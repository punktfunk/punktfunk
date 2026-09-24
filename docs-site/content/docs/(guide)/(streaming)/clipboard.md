---
title: Shared clipboard
description: Copy on your device and paste on the host, and back. The three switches that must be on, what each client moves, and why nothing crosses.
---

Copy on your device and paste on the host, or the other way round, once three switches are on.

## Turn it on

1. **On the host:** in the web console, open **Host → Settings → Streaming** and set **Shared
   clipboard** to **Text** or **Text and files**. It applies from the next stream.
2. **For your device:** its [access level](/docs/access-levels) must include **Clipboard**.
   **Full control** does.
3. **In your client, for that host:** turn on the switch below. It is off by default and set per
   saved host.

| Client | Where | Switch |
|---|---|---|
| Mac, iPhone, iPad | Host card menu → **Host Details…** → **Connection** | **Share clipboard with this host** |
| Windows | Host tile menu → **Edit…** | **Share clipboard with this host** |
| Linux | Host card menu → **Edit…** | **Share clipboard** |
| Android | Host card menu → **Edit…** | **Shared clipboard** |
| Controller home (Android TV, Steam Deck) | The host's options | **Shared clipboard** |

The client reads its switch when a stream starts, so reconnect after changing it. On a Mac,
**Stream → Share Clipboard** (⌃⌥⇧C) turns it on mid-stream and **Stop Sharing Clipboard** turns
it off. The same keys work on an iPad with a keyboard while input is released.

A `PUNKTFUNK_CLIPBOARD` line in `host.env` overrides step 1 and locks it in the console: see
[Configuration](/docs/configuration).

## What crosses

| Client | Formats |
|---|---|
| Mac, iPhone, iPad | Text, rich text (RTF), HTML, PNG, JPEG, GIF |
| Windows | Text, PNG |
| Android | Text |
| Linux, Steam Deck | Nothing yet: the switch exists, but no clipboard is read or written |
| Apple TV | No clipboard |

- **Your copies cross when something pastes.** A copy sends only the list of formats; the bytes
  follow when an app on the other side pastes.
- **Host copies land on Windows and Android at once.** Windows skips a host copy over 4 MiB.
  Mac, iPhone and iPad fetch only when you paste.
- **Leaving the app on iPhone or iPad** ends the stream. A host copy you haven't pasted, up to
  8 MiB, is brought over first so you can paste it in another app.
- **One transfer is at most 64 MiB.**
- **Passwords stay put on Mac, iPhone, iPad and Windows.** A copy a password manager marks as
  secret is never sent. Android and the host don't filter, so a password copied on the host
  reaches your client.
- **No client sends files yet**, so **Text** and **Text and files** behave the same.
- **Moonlight has no clipboard.** It exists only in Punktfunk sessions.

A Linux host needs a desktop that offers a clipboard to it: `ext-data-control-v1` (KWin, Sway,
Hyprland and other wlroots compositors) or GNOME's remote-desktop clipboard. A
[gamescope](/docs/gamescope) session has none. A Windows host always has one.

## Why the toggle does nothing (or is greyed out)

### Nothing crosses in either direction

Work down this list:

| Cause | Fix |
|---|---|
| The host's **Shared clipboard** is **Off** | Step 1 in [Turn it on](#turn-it-on). |
| The console shows **Set by PUNKTFUNK_CLIPBOARD in host.env.** | Change or remove that line, then restart the host ([Configuration](/docs/configuration)). |
| Your client's switch is off for this host, or you changed it while streaming | Step 3, then reconnect. |
| Your device's access level lacks **Clipboard** | Step 2. |
| You're on Linux, a Steam Deck or an Apple TV | These clients don't move the clipboard yet. |
| The host's session has no clipboard (gamescope, or a desktop without the protocols above) | The host log says `clipboard backend unavailable`. Stream a desktop session instead. |

The host logs a `clipboard control` line for each stream with what it decided. See
[Troubleshooting](/docs/troubleshooting) for where the log is.

### **Share Clipboard** is greyed out on the Mac

You aren't streaming, the host's **Shared clipboard** is **Off**, or your device's access level
lacks **Clipboard**.

### A copied password doesn't arrive

Intended: the Mac, iPhone, iPad and Windows clients never send a copy a password manager marks as
secret.

### An image copied on Windows doesn't arrive on the host

The app put only a bitmap on the clipboard, and the Windows client sends images as PNG. Copy it
from an app that puts PNG on the clipboard.
