---
title: Clients
description: The Punktfunk apps for Apple devices, Linux, Windows and Android, the community webOS app, Moonlight, and the punktfunk command — what each does and which to pick.
---

Pick the app for the device you stream *to*. Install steps for each are on
[Install a Client](/docs/install-client).

## Which should I use?

| You're streaming to… | Use |
|---|---|
| A Mac, iPhone, iPad or Apple TV | The [Apple app](#apple-app-mac-iphone-ipad-apple-tv) |
| A Linux desktop or laptop | The [Linux app](#linux-desktop-client-gtk4) |
| A Steam Deck | The [Decky plugin](/docs/steam-deck) in Gaming Mode, the Linux app in Desktop Mode |
| A Windows PC | The [Windows app](#windows-desktop-client) |
| An Android phone, tablet or TV | The [Android app](#android-app-phone--android-tv) |
| An LG webOS TV | The community [webOS app](#webos-lg-tv--community) |
| A browser or any other device | [Moonlight](#moonlight-anything-else) |
| Scripts and home automation | The [`punktfunk` command](#scripting-the-punktfunk-cli) |

Every Punktfunk app finds hosts on your network, [pairs](/docs/pairing) once and reconnects on its
own, browses the host's [game library](/docs/game-library), and has a controller interface for the
couch. They share [presets and `punktfunk://` links](/docs/presets-and-links), the
[in-stream keys](/docs/input#getting-your-input-back) and the settings on
[Client settings](/docs/client-settings).

## Apple app (Mac, iPhone, iPad, Apple TV)

One app for macOS, iOS, iPadOS and tvOS. It adds:

- A **Library** tab (a sidebar row on the Mac) with a chip for each host.
- A host page behind each card's ⓘ, with presets, pairing, power and a network speed test. On
  Apple TV, hold the card or press Play/Pause on it.
- Widgets for hosts and the game library, a Live Activity while you stream, and Shortcuts actions
  for Siri.
- DualSense rumble, adaptive triggers, lightbar, motion and touchpad.

## Linux desktop client (GTK4)

`punktfunk-client` is a GTK4 app with hardware decode (Vulkan Video or VAAPI), PipeWire audio and
SDL3 controllers. For the couch, open **Punktfunk Console**, the gamepad button in the header bar,
or run `punktfunk-client --browse --fullscreen`.

## Windows desktop client

A WinUI 3 app for x64 and Arm64 with hardware decode (Vulkan Video or D3D11VA), HDR, WASAPI audio
and SDL3 controllers. **Punktfunk Console** in the Start menu is the controller interface for a TV or
HTPC.

## Android app (phone + Android TV)

One app for phones, tablets and Android TV, with hardware decode, [HDR10](/docs/hdr#per-client), a
microphone uplink and D-pad navigation.

Plug a **DualSense**, **DualSense Edge** or **DualShock 4** in by **USB** and allow the USB prompt:
the host then gets rumble, adaptive triggers, the lightbar and gyro. Over Bluetooth the pad works as
an ordinary gamepad. **Settings → Controllers → DualSense / DualShock passthrough (USB)** turns
this off.

## webOS (LG TV) — community

[`pf-webos`](https://github.com/dyptan-io/pf-webos) is a native Punktfunk app for LG TVs, built by
the community. It uses the TV's hardware decoder and works with the Magic Remote. It isn't in the LG
Content Store: [sideload it](/docs/install-client#lg-webos-tv-community).

## Moonlight (anything else)

Any [Moonlight](https://moonlight-stream.org/) client connects over GameStream once the host turns
it on: see [Connect with Moonlight](/docs/moonlight). The stream is encrypted and adapts to a lossy
link; the native speed test and jumbo frames are Punktfunk-app only.

## Scripting: the `punktfunk` CLI

`punktfunk` is the client without a window. It ships with every Linux package and the Windows
installer. Under the Flatpak, run it as `flatpak run --command=punktfunk io.unom.Punktfunk`.

```sh
punktfunk discover                            # hosts advertising on this network
punktfunk pair <host>[:port] --pin -          # pair this device; PIN on stdin
punktfunk hosts list --probe                  # saved hosts, each checked live
punktfunk library <host-ref> --json           # the host's games
punktfunk launch <host-ref> --game <id>       # stream, waking the host first
punktfunk open 'punktfunk://connect/<host-ref>'
punktfunk speed-test <host-ref>               # measure the link, suggest a bitrate
```

A `<host-ref>` is a saved host's id, its name or its address. Also: `hosts add`, `hosts forget`,
`default-host`, [`wake`](/docs/wake-on-lan#from-the-command-line), `reachable`, `presets list` and
`reset`. `punktfunk help <command>` lists a command's flags.

- `pair` reads the PIN from stdin with `--pin -`, or asks for it on a terminal. A PIN on the command
  line is refused.
- `open` asks before connecting unless the link names the host by its id. `--yes` answers for you;
  with no terminal and no `--yes` it exits 6.
- Exit codes: **0** ok, **2** connect failed, **3** trust rejected (pair again), **4** the renderer
  didn't start, **5** nothing matched, **6** needs a person (pairing or an unknown host).

Older scripts can keep `punktfunk-client --connect <host>:9777`; on Windows, `--discover` and
`--headless --speed-test` too. They don't wake a sleeping host.
