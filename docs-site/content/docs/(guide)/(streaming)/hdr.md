---
title: HDR
description: What an HDR10 stream needs from the host, the codec and your client, and how to find the missing link when you get SDR.
---

You get a 10-bit HDR10 (BT.2020 PQ) stream when every link below holds; otherwise the session
streams 8-bit SDR. The host settles this before the first frame, so reconnect after changing
anything on this page.

## The chain

1. **The host allows it.** **Host → Settings → Video → 10-bit and HDR** (`PUNKTFUNK_10BIT`), on
   by default.
2. **The source delivers 10-bit PQ.** This is the link that fails most — see [Per host](#per-host).
3. **The codec has a 10-bit path:** HEVC, AV1 or [PyroWave](/docs/pyrowave). H.264 has none, so
   pinning H.264 on the client pins SDR.
4. **The host GPU encodes 10-bit** for that codec. The host test-opens an encoder once per GPU and
   codec and believes the answer.
5. **The client asks for HDR** with its HDR setting — see [Per client](#per-client).

## Per host

### Windows

Nothing to set. The host turns HDR on for the session's virtual display when the session is HDR,
and forces it off for an SDR session; Windows' own **Use HDR** switch doesn't matter.

- The encoder must be NVENC, AMF, QSV or PyroWave. A Windows on Arm host (Media Foundation) or
  a software-encoding host streams SDR.
- If Windows refuses to turn HDR on, the host logs an error and streams SDR although the client
  was told HDR. This is the one case where the client's `HDR` label is wrong.
- Vulkan games need the installer's **HDR Vulkan layer** (on by default); see
  [the troubleshooting entry](#a-vulkan-game-on-windows-says-hdr-isnt-supported).

### Linux + gamescope

The only Linux route to HDR for the Punktfunk apps.

1. Install **`punktfunk-gamescope`**; stock gamescope captures 8-bit.
   [HDR on gamescope](/docs/gamescope#hdr-on-gamescope) has the package for your distro.
2. Restart the host. It reads what the gamescope build can do once, at startup.
3. Let the host start the gamescope session, which is the default. A session it attaches to
   instead streams SDR: remove `PUNKTFUNK_GAMESCOPE_ATTACH=1` (**Attach mode**) from `host.env`
   if you have it.
4. For Steam's own HDR setting to be available, the build must be `+pfhdr14` or newer. Check with
   `punktfunk-gamescope --version`.

SDR content in an HDR session (the desktop, the Steam UI, SDR games) starts at **Host → Settings
→ Game Mode → SDR brightness** (advanced, `PUNKTFUNK_GAMESCOPE_SDR_NITS`): 203 nits, the level the
clients expect. Steam's SDR brightness setting moves it during the session. Builds older than
`+pfhdr16` ignore both and show SDR content oversaturated.

**Game Mode HDR** (`PUNKTFUNK_GAMESCOPE_HDR`) on the same page, turned off, keeps gamescope
sessions SDR.

### Linux + GNOME

GNOME HDR reaches [Moonlight](/docs/moonlight) only, by mirroring a real monitor (GNOME 50+).
A Punktfunk app connecting to a GNOME host gets SDR.

1. Add `PUNKTFUNK_VIDEO_SOURCE=portal` to [`host.env`](/docs/configuration) and restart the host.
2. Put the monitor in HDR mode in GNOME **Settings → Displays**.
3. Connect with HDR on in Moonlight.

With `PUNKTFUNK_CAPTURE_MONITOR=<connector>` set, only that monitor's HDR mode counts. If no
monitor is in HDR mode, the session streams SDR and says so in the host log.

### Other Linux desktops

SDR. KDE, GNOME's virtual displays and the wlroots-family compositors capture 8-bit, and so does
streaming a physical monitor to a Punktfunk app. No setting changes this.

## Per client

HDR is **on by default** in every client and can differ per [preset](/docs/presets-and-links).
The toggle sits with the other [video settings](/docs/client-settings#video).

| Client | Setting | Asks for HDR when |
|---|---|---|
| Linux, Steam Deck | **10-bit HDR** | the setting is on. Shows HDR10 where the display offers it, otherwise tone-maps to SDR |
| Windows | **HDR (10-bit, BT.2020 PQ)** | the same as Linux |
| macOS, iPhone, iPad | **10-bit HDR** | the setting is on **and** the display is HDR-capable |
| Apple TV | **10-bit HDR** | the setting is on **and** the TV is HDR-capable. The TV switches to HDR10 only with tvOS **Match Content** on; otherwise the stream is tone-mapped to SDR |
| Android, Android TV | **HDR** | the setting is on. It is greyed out on a panel without HDR10 |
| Moonlight | its own HDR toggle | the toggle appears when the host offers a 10-bit codec |

## HDR and 4:4:4

[Full chroma](/docs/client-settings#video) and HDR together:

| Host | HEVC | PyroWave |
|---|---|---|
| Windows | Both | Both |
| Linux | HDR, at 4:2:0 | Both |

AV1 is always 4:2:0.

## Check it

**On the client**, set the [stats overlay](/docs/stats) to **Detailed**. Its first line names the
codec and depth, then `HDR` for an HDR stream on an HDR display, or `HDR→SDR` when the Linux or
Windows client tone-maps it because the display offers no HDR10.

**On a Linux host**, one command checks every link:

```bash
punktfunk-host hdr-probe
```

It reports the monitor's HDR mode, whether the gamescope build offers 10-bit capture, the Game
Mode HDR setting, the encoder's 10-bit answer for HEVC and AV1, and whether the resolved compositor
and the GameStream side can do HDR. The service reads `host.env` and your shell doesn't, so load it
first, or the answers describe your shell:

```bash
set -a; . ~/.config/punktfunk/host.env; set +a
punktfunk-host hdr-probe
```

On Windows, [`hdr-p010-selftest`](/docs/host-cli#hdr-probe-and-probe-compositor) checks the GPU's
HDR colour conversion instead.

## Troubleshooting

### The stream is SDR

The host logs an `encode bit depth` line per session. The first `false` among
`host_wants_10bit`, `capture_supports_hdr`, `client_wants_hdr` and `gpu_can_10bit` names the
missing link of [the chain](#the-chain): the host setting, the source, the client, or the codec
and GPU.

### HDR stopped working on a Linux host

The log says `HDR capture negotiation failed`. After that the host offers SDR: on gamescope until
that display closes, on the GNOME monitor mirror until the host restarts. Fix the cause (usually
a gamescope that isn't the Punktfunk build, or a monitor out of HDR mode), then reconnect or
restart.

### A Vulkan game on Windows says HDR isn't supported

NVIDIA and AMD Vulkan drivers hide HDR from games on a virtual display. The installer's **HDR
Vulkan layer** option (`VK_LAYER_PUNKTFUNK_hdr_inject`) shows it to them again; re-run the
installer with it ticked. The layer does nothing in an SDR session and skips a built-in list of
anti-cheat titles. To turn it off for one game, set `DISABLE_PF_VKHDR=1` in its environment, or
list executables in `PF_VKHDR_EXCLUDE=foo.exe,bar.exe`. D3D11 and D3D12 games don't need it.

### An HDR stream is green on a Windows client with an AMD GPU

AMD drivers older than AMD Software 25.9.1 draw HDR wrong on the Vulkan decoder; the client warns
about it. Update the driver, or set **Video decoder** to **Hardware (Direct3D 11 / DXVA)**.
