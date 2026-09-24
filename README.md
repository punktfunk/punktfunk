<p align="center">
  <img src="assets/punktfunk-logo.svg" alt="Punktfunk" width="320" />
</p>

<p align="center"><b>Low-latency desktop and game streaming with first-class Linux and Windows hosts.</b></p>

Run the host on a Linux or Windows PC and stream your desktop or games to a Mac, PC, phone, tablet
or TV. Each client gets **its own virtual display at its own native resolution and refresh rate**,
so a laptop at 1080p60 and a TV at 4K can stream from one box at once without rearranging your real
monitors.

📖 **[docs.punktfunk.unom.io](https://docs.punktfunk.unom.io)** —
[How it works](https://docs.punktfunk.unom.io/docs/how-it-works) ·
[Quick start](https://docs.punktfunk.unom.io/docs/quickstart) ·
[Support matrix](https://docs.punktfunk.unom.io/docs/support-matrix) (what works where) ·
[Roadmap](https://docs.punktfunk.unom.io/docs/roadmap)

💬 [Discord](https://discord.gg/wzEGg9y45z) · [r/Punktfunk](https://www.reddit.com/r/Punktfunk/)

🔒 Vulnerabilities go privately to **security@punktfunk.com**, not to an issue — [SECURITY.md](SECURITY.md).

## What makes it different

- **Displays you configure, not just create.** Keep a game's display alive across disconnects so a
  reconnect drops straight back in; make the stream your sole desktop or extend alongside your
  monitors; turn several devices into monitors of one desktop. Presets in the console —
  [Virtual displays](https://docs.punktfunk.unom.io/docs/virtual-displays).
- **A real virtual display on Windows, too.** Linux uses per-compositor virtual outputs; Windows
  gets the same on-the-fly display from Punktfunk's own signed IddCx driver (`pf-vdisplay`) — no
  dummy HDMI plug, no Desktop Duplication or WGC screen-scraping, and it survives the secure desktop
  (UAC, lock screen).
- **GPU end to end.** Frames reach the encoder with no CPU copies (dmabuf → CUDA/Vulkan → NVENC on
  Linux; NVENC/AMF/QSV/Media Foundation on Windows). On a wired link,
  [PyroWave](https://docs.punktfunk.unom.io/docs/pyrowave) — an intra-only wavelet codec run as
  Vulkan compute — spends bandwidth to cut codec latency by an order of magnitude, and every frame
  is a keyframe, so loss costs one frame instead of a recovery round-trip.
- **Two protocols, one process.** `punktfunk/1` is the native plane: QUIC control, a GF(2¹⁶)
  Leopard-RS FEC + AES-GCM data plane that breaks the ~1 Gbps FEC wall, mid-stream mode
  renegotiation. `serve --gamestream` additionally serves any
  [Moonlight](https://moonlight-stream.org/) client — opt-in, trusted-LAN only, because GameStream
  has inherent on-path weaknesses.
- **A library that fills itself.** Steam and non-Steam titles appear as a grid on every client, and
  [plugins](https://docs.punktfunk.unom.io/docs/plugins) add their own sources — ROM Manager,
  Playnite, Ubisoft Connect, Battle.net, itch.io, Flatpak and more.
- **Secure by default.** One-time SPAKE2 **PIN pairing**, then pinned-identity reconnects. No
  accounts, no cloud. Hosts advertise over mDNS, so clients find them without typing an IP.

## Install the host

One command on Linux (preview) — it detects your distro, adds the repo, installs the host and
console, opens the firewall, and tells you how to pair:

```sh
curl -fsSL https://punktfunk.unom.io/install.sh | sh
```

On **Windows 11 22H2+** it's a signed installer (host + virtual-display and gamepad drivers):

```powershell
winget source add -n punktfunk https://winget.punktfunk.unom.io -t Microsoft.Rest
winget install unom.PunktfunkHost
```

Prefer to add the repo yourself? Every platform has a one-page guide — apt, dnf, pacman, the Bazzite
sysext, NixOS, SteamOS on-device build, and the Windows `setup.exe`:
**[/docs/install](https://docs.punktfunk.unom.io/docs/install)**. Updating, rolling back and
uninstalling are [/docs/updating](https://docs.punktfunk.unom.io/docs/updating) and
[/docs/uninstall](https://docs.punktfunk.unom.io/docs/uninstall).

## Connect a client

| Streaming to… | Use |
|---|---|
| Mac | The **Apple app** — notarized DMG, or TestFlight |
| iPhone, iPad, Apple TV | The **Apple app** on TestFlight |
| Linux desktop / laptop | **`punktfunk-client`** — Flatpak (any distro), or apt / rpm / pacman |
| Steam Deck | The **Decky plugin** in Gaming Mode; the Flatpak in Desktop Mode |
| Android phone or TV | The **Android app** on Google Play |
| Windows | Native **`punktfunk-client`** — signed installer (portable zip and MSIX too) |
| LG webOS TV | **`pf-webos`**, a community client in its own repo |
| Scripts and launchers | **`punktfunk`**, the headless CLI in the Linux client packages |
| Anything else | **Moonlight** over GameStream |

Every client discovers hosts automatically and does a one-time
[PIN pairing](https://docs.punktfunk.unom.io/docs/pairing). Per-device steps:
**[/docs/install-client](https://docs.punktfunk.unom.io/docs/install-client)**.

## Develop

Building, testing and contributing are in the
**[developer docs](https://docs.punktfunk.unom.io/docs/developers)**:
[Architecture](https://docs.punktfunk.unom.io/docs/developers/architecture) (crate map and design
invariants) ·
[Build from source](https://docs.punktfunk.unom.io/docs/developers/build-from-source) ·
[Testing](https://docs.punktfunk.unom.io/docs/developers/testing) ·
[Contributing](https://docs.punktfunk.unom.io/docs/developers/contributing).

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option — `SPDX-License-Identifier: MIT OR Apache-2.0`.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in
the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions. See [CONTRIBUTING.md](CONTRIBUTING.md).

### Third-party components

Punktfunk's own source is MIT/Apache-2.0. Shipped binaries additionally link third-party components
under their own (permissive) licenses — see [`THIRD-PARTY-NOTICES.txt`](THIRD-PARTY-NOTICES.txt)
(regenerate with `scripts/gen-third-party-notices.sh`). Nothing links FFmpeg any more: host encode
went native in 2026-09, and the clients decode natively.

### Trademarks

Punktfunk is an independent project and is **not affiliated with, endorsed by, or sponsored by**
NVIDIA, Microsoft, Sony, Valve, or the Moonlight project. "GameStream", "Moonlight", "Xbox",
"DualSense", "DualShock", and "PlayStation" are trademarks of their respective owners and are used
here only to describe interoperability.
