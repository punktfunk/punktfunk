---
title: Presets and links
description: How settings presets override your client defaults per host or per connect, and how punktfunk:// links start a stream from a shortcut, a script or a browser.
---

A settings preset is a named set of stream settings you attach to a host, a game or one connect. A
`punktfunk://` link starts a stream you already set up, from a shortcut, a script or a browser.
Both live in the Punktfunk apps, not in the host's web console.

## What a preset is

A preset stores only the rows you change; every other row follows your default settings, including
later changes to them. Changing a row pins it, even to the value the default has. The row's
**Reset** makes it follow the default again.

Presets stay on the device that made them; nothing syncs them. Linux keeps them in
`~/.config/punktfunk/client-presets.json`, Windows in `%APPDATA%\punktfunk\client-presets.json`, the
Apple and Android apps in their own storage.

## Creating and editing one

1. Open **Settings** and pick **New preset** from the switcher at the top.
2. Name it and pick a colour. The colour tints the preset's chip on host cards.
3. Change the rows you want. A changed row shows a marker and **Reset**.

| App | Switcher | Rename, duplicate, delete |
|---|---|---|
| Linux, Windows, iPhone, iPad, Apple TV | **Editing** row | Linux: next to the switcher. Windows: **Edit preset…** in the switcher. Apple: the switcher's menu. |
| Mac | Menu at the top of the settings window | The same menu |
| Android | Row of chips | Tap the selected chip again |

Windows creates *Preset 1* and opens it; rename it there. Names are unique, ignoring case. The
console (the controller interface) can bind and pin presets but not create or edit them.

## What a preset can't change

Rows that describe this device rather than the stream don't show in a preset: the list is on
[Client settings](/docs/client-settings#settings-that-are-facts-about-your-device). **Share
clipboard** is a per-host switch in the host's edit sheet, not a setting: see
[Shared clipboard](/docs/clipboard).

## Using a preset

- **Bind it to a host.** Set **Preset** in the host's edit sheet, **Connect with** on the Apple host
  page, or **Default preset…** in the console's host options. A plain click on the host uses it.
- **Bind it to a game.** In the console, open a game's options (**X**) → **Settings preset…**.
  Launching that game uses it instead of the host's preset.
- **Use it once.** The host card's menu → **Connect with** (on Android, **Connect with: …**).
  **Default settings** there streams without the host's preset.
- **Pin it as a card.** A pinned preset gets its own card beside the host. Pin it in the host's edit
  sheet (Linux, Android), on the host page (Apple), with **Pin as card: …** (Android) or **Pin
  tiles** (Windows) in the card menu, or from the console's **Presets** tab. Unpinning changes
  neither the preset nor the host.

The active preset's name shows in the [stats overlay](/docs/stats).

Deleting a preset says how many hosts and pinned cards use it. Those fall back to your default
settings; nothing fails to connect.

## `punktfunk://` links

A link starts a stream on a host this device already trusts:

```text
punktfunk://connect/<host-ref>[?fp=<64-hex>][&host=<addr[:port]>][&launch=<id>][&preset=<ref>][&name=<label>]
```

`<host-ref>` is a saved host's record id, its name (ignoring case), or `addr[:port]`, tried in that
order. A name that matches two hosts is refused.

| Parameter | Means |
|---|---|
| `fp` | The host certificate fingerprint the link expects, 64 hex characters |
| `host` | `addr[:port]` to use when `<host-ref>` no longer matches; port defaults to `9777` |
| `launch` | A [library](/docs/game-library) id such as `steam:570`, launched on arrival |
| `preset` | A preset by id or unique name, for this connect only. `profile` is an alias. |
| `name` | A label shown in the confirmation, never trusted |

Scheme and route are case-insensitive, unknown parameters are ignored, and a repeated parameter's
first value wins. Limits: 2048 characters for the URL, 128 for `<host-ref>` and `launch`, 64 for
`preset` and `name`. `launch` must be printable ASCII without spaces, quotes, `\`, `$` or backticks.

`browse` takes the same form and opens the host's game library instead; only the Apple apps handle
it (their library widget and **Open Game Library** shortcut use it). Other apps show a notice.

```text
punktfunk://connect/Living%20Room%20PC
punktfunk://connect/Living%20Room%20PC?launch=steam:570
punktfunk://connect/Living%20Room%20PC?preset=Work
```

These name the host by label, so each asks first. **Copy link** writes the record id instead, which
opens in one click.

## What a link can and can't do

A link can do what clicking a card you already have does, and no more.

- It carries references, never values: no resolution, bitrate or codec parameters.
- It can't pair. `punktfunk://pair/…` is refused; [pairing](/docs/pairing) stays manual.
- Only the record id connects without asking. A name or an address, including `host=`, shows **Open
  this link?** first, naming the host and the game.
- A host you haven't saved is never connected. With an address, Linux, Windows and the Android app
  open the normal trust prompt, filled in with the address and any `fp`; the Apple apps and
  Android's controller interface show a notice. Without an address the link is refused.
- An `fp` that doesn't match the host's pinned fingerprint is refused.
- A `preset` that matches nothing, or two presets, is refused before anything connects. With no
  `preset`, the host's own binding applies.
- A link never ends a running session; the app asks you to end it first. On Apple and Android, a
  link to the host you're already streaming brings the app forward.

## Getting a link, and making a shortcut

**Copy link** (**Copy Link** on Apple) is in the host card's menu on Linux, Windows, Mac, iPhone,
iPad and Android, and in the console's host options (press **Up** on a host). On Linux, Windows,
Android and the console a game in the library has one too, with its `launch=`. A pinned card's link
carries its preset, except on Windows. A copied link carries the
record id, `host=` and, once pinned, `fp=`, so it survives an address change or a reinstall.

**Create shortcut…** (Linux, Windows) writes a launcher:

- **Linux**: a desktop entry in `~/.local/share/applications/`. Under Flatpak the app shows the URL
  for you to place yourself.
- **Windows**: a `.lnk` on your Desktop that runs `punktfunk-client.exe` with the URL.

From a script, use the [`punktfunk` command](/docs/clients#scripting-the-punktfunk-cli):

```bash
punktfunk presets list
punktfunk open --yes 'punktfunk://connect/Desk?preset=Work'
```

`--yes` answers the confirmation a name or address raises.
