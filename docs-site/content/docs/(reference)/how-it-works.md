---
title: How It Works
description: The ideas behind Punktfunk — a virtual display per client, how frames reach the wire, the two protocols, and pairing.
---

What happens between your host and your device, in the words the rest of these docs use.

## A virtual display, sized to your device

When a client connects, the host creates a **virtual display** at the client's resolution and
refresh rate, streams it, and removes it when the client leaves. Apps and games treat it as a real
monitor, but no physical screen or dummy plug is involved. Two clients get two displays, each at its
own mode.

| Host | How the display is made |
|---|---|
| **GNOME** | A virtual monitor from Mutter's screen-cast API |
| **KDE Plasma** | A virtual output from KWin |
| **gamescope** (SteamOS, Bazzite) | A gamescope session at the client's mode |
| **[Hyprland](/docs/hyprland)** | A headless output added with `hyprctl` |
| **[sway, scroll](/docs/sway)** | A headless output added over the compositor's IPC |
| **Windows** | Punktfunk's own display driver, which also shows the secure desktop (UAC, lock screen) |

On Linux you can stream one of your real monitors instead — see
[Virtual displays](/docs/virtual-displays#stream-a-real-monitor-instead).

## From screen to wire

Captured frames stay on the GPU all the way into the encoder. Which encoder runs depends on the GPU:

| GPU | Linux | Windows |
|---|---|---|
| **NVIDIA** | NVENC | NVENC |
| **AMD** | Vulkan Video for HEVC and AV1, VAAPI for H.264 | AMF |
| **Intel** | Vulkan Video for HEVC and AV1, VAAPI for H.264 | QSV |
| **Other** | Software H.264, only if you set `PUNKTFUNK_ENCODER=software` | Media Foundation |

Client and host then agree on a codec: **HEVC** unless the client asks for another, **AV1** where
both sides support it, **H.264** on the software encoder, and **[PyroWave](/docs/pyrowave)** when
you pick it on a wired link. **[HDR](/docs/hdr)** works where the host's capture, its encoder and
your client all support it. The full breakdown is in the [Support matrix](/docs/support-matrix).

## Two protocols

- **punktfunk/1** — Punktfunk's own protocol: a QUIC control channel and an encrypted UDP media
  channel with forward error correction. The [native clients](/docs/clients) (Apple, Android, Linux,
  Windows) use it. It is always on.
- **GameStream** — the protocol [Moonlight](/docs/moonlight) speaks, so any Moonlight client
  connects. It is off until you turn on **GameStream** in **Host → Settings**, and it pairs over
  plain HTTP, so use it on a [trusted network](/docs/security#gamestream--moonlight-compatibility-is-the-weak-crypto-path)
  only.

Both run in one host process.

## Pairing and trust

The first time a device connects, you pair it: approve it in the web console, or type the PIN the
host shows. After that it reconnects on its own, on a pinned identity — no account, no cloud. See
[Pairing & Trust](/docs/pairing).

## Finding hosts

Hosts announce themselves on your local network, so the native clients and Moonlight list them
without an IP address.

## Several devices at once

A host streams to several clients at the same time, each on its own display at its own mode. See
[Multiple devices](/docs/configuration#multiple-devices-at-once).
