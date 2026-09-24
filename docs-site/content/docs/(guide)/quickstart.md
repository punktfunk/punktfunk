---
title: Quick Start
description: From nothing to streaming in five steps — install a host, open its console, pair a device, play.
---

Five steps take you from a fresh PC to a stream on your other device. Keep the host on your home
network or a VPN ([Security](/docs/security)).

## 1. Install the host

On the PC you stream *from*, follow the page for its system:

| Linux | Windows |
|---|---|
| [Ubuntu](/docs/ubuntu) · [Debian](/docs/debian) · [Fedora](/docs/fedora) · [Arch / CachyOS](/docs/arch) · [Omarchy](/docs/omarchy) · [Bazzite](/docs/bazzite) · [SteamOS](/docs/steamos-host) · [NixOS](/docs/nixos) | [Windows 11](/docs/windows-host) |

Check [Requirements](/docs/requirements) first if your machine is older or unusual.

## 2. Start it

- **Windows, SteamOS and the guided installer:** already running, and it comes back on its own.
- **Linux by hand:** the **Start it** step of your install page starts the host and the console;
  they come back at every login.

The host detects your desktop and announces itself on your network. There is nothing to configure
for a first stream.

## 3. Open the web console

Open **`https://<host-ip>:47992`** in a browser. The certificate is the host's own, so the browser
warns once: continue. Sign in with the console password:

| Host | Password |
|---|---|
| Linux | The one you typed in the guided installer, or print the generated one: `sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password` |
| SteamOS | `sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web.env` |
| Windows | The one you set in the installer, or from an elevated PowerShell: `punktfunk-host web password` |

Read it before you sign in: the console then stores only a hash. Lost it?
[Reset it](/docs/forgot-password).

![The console sign-in card: one password field](/img/console-login.png)

## 4. Install a client and pair it

On the device you stream *to*, install the app from [Install a Client](/docs/install-client).
Moonlight works too once you [turn GameStream on](/docs/moonlight).

Open the app. Your host is already in the list.

![The client's host list: saved hosts with their pairing state, and unpaired hosts found on this network](/img/client-hosts.png)

Select it and connect. In the console, open **Devices** and click **Approve** next to the device.
To pair with a PIN instead, click **Pair a device** and type the code into the app. A device pairs
once and reconnects on its own after that. More: [Pairing & Trust](/docs/pairing).

![The console's Devices page: two devices waiting for approval, and an armed PIN](/img/console-pairing.png)

## 5. Stream

Select the host and start streaming. The host creates a display at your device's resolution and
refresh rate. A desktop client captures your mouse and keyboard: **Ctrl+Alt+Shift+Q** (⌃⌥⇧Q on a
Mac) releases them.

## Next

- Launch games from the stream: [Game library](/docs/game-library) and its
  [plugins](/docs/plugins).
- Tune resolution, bitrate, codec and HDR per device: [Client settings](/docs/client-settings).
  **Start in** there can open the app straight into this host.
- Stream with nobody logged in: [Running as a service](/docs/running-as-a-service).
- Coming from Sunshine or Apollo: [Switching from Sunshine](/docs/switching-from-sunshine).
- Something wrong: [Troubleshooting](/docs/troubleshooting).
