---
title: HDR
description: How an HDR10 stream is decided end to end — the four things that must all be true, what each host and client can actually do, and how to check which link is missing.
---

An HDR session carries a **10-bit BT.2020 PQ (HDR10)** picture from the host's display to your
screen. It is on by default wherever it works; otherwise the session streams 8-bit BT.709 SDR. The
host decides **before the first frame** — it resolves every gate below and tells the client what it
will really send — so nothing on this page takes effect mid-stream; reconnect after changing any of
it.

## The chain

Four things must all be true. If you got SDR when you expected HDR, one of these is why.

1. **The source.** What the host captures must hand it 10-bit PQ pixels. This link fails most often
   and is entirely host-side — see [Per host](#per-host).
2. **The encoder.** The host GPU must encode 10-bit for the session's codec. The host probes by
   opening a tiny real encoder once per GPU and codec, and believes the answer. (PyroWave skips the
   probe — its own rule is below.)
3. **The codec.** Only HEVC, AV1 and PyroWave have a 10-bit path — see [Codec rules](#codec-rules).
4. **The client.** It must advertise 10-bit and HDR (its **HDR** setting) and be able to present or
   tone-map PQ.

The host allows 10-bit by default (`PUNKTFUNK_10BIT`). It only ever *allows* — the client's setting
is the real per-session switch.

## Per host

### Windows

The Windows host **turns HDR on for the session's virtual display itself** when 10-bit was
negotiated — you don't have to enable "Use HDR" in Windows Settings. It enables advanced colour at
capture open, waits for it to settle, then composes in FP16 and encodes P010 BT.2020 PQ. The
reverse holds too: an **SDR** session forces advanced colour **off** on that display, so a client
that asked for 8-bit is never handed PQ. If enabling it fails, the host logs a loud error and encodes
8-bit anyway — the client was already told HDR, so that is the one place a Punktfunk label can
outrun the picture.

- **HDR and 4:4:4 compose on Windows, not on Linux.** A **Windows** host carries both: the capture
  path writes full-resolution 10-bit chroma and NVENC encodes HEVC Main 4:4:4 10, so
  [full chroma](/docs/client-settings) costs nothing on an HDR desktop; [PyroWave](/docs/pyrowave)
  does the same there, in 16-bit planes. On **Linux** the 4:4:4 route is 8-bit, so a session that
  asks for both keeps HDR and drops to 4:2:0 — HDR wins, since games can only offer HDR on an HDR
  display. AV1 never carries 4:4:4 anywhere: Range Extensions are HEVC-only.
- **Vulkan games need the bundled layer.** NVIDIA and AMD Vulkan drivers refuse to advertise any HDR
  colour space for a surface on an indirect (virtual) display, so Vulkan games decide the device
  "does not support HDR" — though the driver happily presents an HDR swapchain there. The host
  installer ships an implicit Vulkan layer, `VK_LAYER_PUNKTFUNK_hdr_inject`, that adds those formats
  back (installer task **Install the HDR Vulkan layer**, ticked by default). It self-gates on the
  monitor's live advanced-colour state, so it does nothing on an SDR session, and it skips a built-in
  list of kernel-anti-cheat titles. `DISABLE_PF_VKHDR=1` in a game's environment switches it off for
  that process; `PF_VKHDR_EXCLUDE=foo.exe,bar.exe` skips further executables by name. D3D11/D3D12
  games need none of this.

### Linux + gamescope

Stock gamescope tone-maps its composite down to 8 bits before handing it over, so its capture output
is SDR whatever the game rendered. Real HDR needs **`punktfunk-gamescope`**, a build carrying a patch
that adds the 10-bit PQ formats to its PipeWire node. It installs beside your system gamescope rather
than replacing it; [HDR on gamescope](/docs/gamescope#hdr-on-gamescope) has the package for each
distro.

Steam's own HDR setting needs that build at `+pfhdr14` or newer (`punktfunk-gamescope --version`).
On an older one the stream is HDR, but Steam shows HDR as unavailable, and games that follow
Steam's setting can't turn it on.

Before spawning anything the host settles two facts: the gamescope binary it will run carries the
patch (its `--version` banner contains `+pfhdr`), and this host is the one **starting** the session
rather than attaching to a node someone else started.

**Attach mode is the trap.** The patched build only reaches sessions the host spawns itself —
managed, `PUNKTFUNK_GAMESCOPE_SESSION`, or a bare spawn. A session started by your display manager
runs the distro's own gamescope, which offers neither the 10-bit formats nor the in-node cursor, and
the host cannot tell that from the outside unless you pinned `PUNKTFUNK_GAMESCOPE_NODE`. With
`PUNKTFUNK_GAMESCOPE_ATTACH=1` and the patched build installed it reads the binary, believes HDR is
available, and offers it; the attached session can't answer that negotiation, so the connect fails
with no picture, the host latches an SDR downgrade for the rest of its life, and the next connect
streams — in SDR.

That bites on [Bazzite](/docs/bazzite), where the sysext installs `punktfunk-gamescope` alongside a
stock session gamescope. No template pins attach any more, so the managed default gets you HDR and
the compositor-drawn cursor — but an older template did, and an upgrade never rewrites a `host.env`
you already have: check yours for `PUNKTFUNK_GAMESCOPE_ATTACH=1` and delete the line. If you
deliberately stay on attach, set `PUNKTFUNK_GAMESCOPE_HDR=0` so the failed attempt never happens.
Attach also leaves the stream with no cursor; [HDR on gamescope](/docs/gamescope#hdr-on-gamescope)
has the fix for that half.

SDR content — the desktop, the Steam overlay, an SDR game — rides the same PQ container at
`PUNKTFUNK_GAMESCOPE_SDR_NITS`, default **203 nits**. That is BT.2408 reference white and the level
our clients decode against, so the two ends agree out of the box. Steam's SDR brightness setting
moves it during the session; a higher value makes SDR brighter next to HDR content.

That needs `punktfunk-gamescope` at `+pfhdr16` or newer. Older builds ignore both the variable and
Steam's setting, and stretch SDR colours to the BT.2020 gamut, so the Steam UI and SDR games look
oversaturated in an HDR stream.

### Linux + GNOME

A host serves [two protocols](/docs/how-it-works#two-protocols): its own `punktfunk/1` (the Linux,
Windows, Apple and Android apps) and GameStream ([Moonlight](/docs/moonlight)). GNOME HDR is
available on the GameStream side only.

GNOME 50 added HDR screencast for **real monitors** only, so this route mirrors a monitor instead of
creating a virtual display: set `PUNKTFUNK_VIDEO_SOURCE=portal`, put the monitor in HDR mode in
**Settings → Displays**, and connect an HDR-capable client. `PUNKTFUNK_CAPTURE_MONITOR=<connector>`
pins which head; when it is set the host checks *that* monitor's colour mode rather than asking
whether any monitor is in HDR. If none is, the session degrades to 8-bit SDR and says so in the log.

A Punktfunk app connecting to a GNOME host over `punktfunk/1` gets SDR. On that protocol the only
Linux HDR source is the gamescope virtual output.

### Linux virtual displays on KWin, Mutter and wlroots

**These are SDR.** Mutter's `RecordVirtual` streams and the KWin and wlroots virtual outputs are
8-bit upstream, so there is nothing to capture in 10 bits — no setting changes this. Streaming a
*physical* monitor with the [Streamed screen](/docs/virtual-displays) setting is SDR to a Punktfunk
app too, HDR panel or not; the GNOME/GameStream route above is the only Linux monitor mirror that can
be HDR.

## Per client

| Client | HDR10 present | Advertises HDR when |
|---|---|---|
| **Linux** (GTK) · **Windows** (WinUI 3) | Yes — the Vulkan presenter switches to an HDR10 swapchain when the surface offers one | the setting is on. It does **not** check your display first |
| **macOS · iPhone · iPad** | Yes — Metal, `rgba16Float` in BT.2100 PQ with EDR | the setting is on **and** the display reports HDR capability |
| **Apple TV** | Yes — PQ passthrough once the session's display-mode switch lands; before that, tone-mapped to SDR in-shader | the setting is on **and** the TV is HDR-capable |
| **Android** (phone + TV) | Yes — HDR10 via the Surface dataspace plus static metadata | the setting is on **and** the panel reports HDR10 or HDR10+. On an SDR panel the toggle is disabled |
| **Moonlight** | Its own HDR toggle, which appears only when the host advertises a 10-bit codec | — |

The Linux and Windows clients are deliberately looser: they advertise HDR whenever the setting is on
and let the presenter sort out the display — HDR10 swapchain where the compositor offers one,
tone-mapped to SDR where it doesn't. The stats overlay says which happened: `HDR` versus `HDR→SDR`.

One exception: frames from **software decode** never take the HDR10 swapchain, whatever the surface
offers, so a client with no hardware HEVC decode presents an HDR stream on the SDR swapchain without
a tone-map — washed out. Turn the client's HDR setting off there. The
[Steam Deck plugin](/docs/steam-deck) streams through this same client.

## Codec rules

- **HEVC** — Main10. The usual HDR codec.
- **AV1** — 10-bit, where the GPU encodes it. Advertised separately from HEVC, so a box that does
  one and not the other tells the truth about each.
- **H.264** — never. High10 is not an encode mode on the hardware Punktfunk targets, so negotiation
  never asks. Pinning H.264 in your client settings pins the session to SDR.
- **[PyroWave](/docs/pyrowave)** — carries HDR in 16-bit planes, but **only from a Windows host**.
  The Linux PyroWave capture path has no HDR colour conversion, so a Linux-hosted PyroWave session
  is SDR. Use HEVC or AV1 for HDR from Linux.

With full chroma: a **Linux** host encodes 4:4:4 at 8 bits, so a session that negotiates both
resolves back down to SDR before the stream starts — on Linux, 4:4:4 wins. A **Windows** host
carries HDR and full chroma at once. Full chroma is off until you turn it on, so this only bites if
you did.

## Check it

On a **Linux** host, one subcommand answers every link in the chain:

```bash
punktfunk-host hdr-probe
```

It prints, line by line: whether a monitor is in BT.2100 colour mode, whether the resolved gamescope
offers 10-bit PQ capture, the state of `PUNKTFUNK_GAMESCOPE_HDR`, whether gamescope will paint the
cursor into the capture node, the encoder's Main10 answer for HEVC and for AV1, whether the resolved
compositor can do HDR on Punktfunk's own plane, and the combined GameStream capability.

**Run it with the environment the host service has.** The service loads `host.env` itself; your shell
does not, and `PATH` decides which gamescope binary gets probed:

```bash
set -a; . ~/.config/punktfunk/host.env; set +a
punktfunk-host hdr-probe
```

There is no `hdr-probe` on Windows. Windows has instead a GPU colour self-test for the capture
conversion, which needs no display or session:

```powershell
punktfunk-host hdr-p010-selftest 1920x1080 nvidia
```

Pass your real capture size (heights like 1080 are not 16-aligned and take a different driver path)
and, on a dual-GPU box, the vendor that encodes — `intel`, `nvidia` or `amd`.

From the client side, set the [stats overlay](/docs/stats) to **Detailed**. On Linux and Windows it
adds an `HDR` tag — or `HDR→SDR` when a PQ stream is arriving but the local surface can't present
it. Android prints the negotiated depth and colour outright at the same tier
(`HEVC · 10-bit · HDR (BT.2020 PQ) · 4:2:0`).

## The switches

Host, in [`host.env`](/docs/configuration):

| Setting | Default | Effect |
|---|---|---|
| `PUNKTFUNK_10BIT` | **on** | Allow 10-bit (HEVC Main10 / AV1 10-bit) at all. `0`, `false`, `off` or `no` forces every session to 8-bit SDR. |
| `PUNKTFUNK_GAMESCOPE_HDR` | **on** | Allow HDR on the gamescope backend. It only decides whether HDR is *attempted* — a host without `punktfunk-gamescope` stays SDR either way. `0` is the escape hatch that puts the gamescope backend back on the old SDR path, spawn flags included. |
| `PUNKTFUNK_GAMESCOPE_SDR_NITS` | **203** | How bright SDR content starts inside the PQ container of an HDR gamescope session. Steam's SDR brightness setting replaces it. |
| `PUNKTFUNK_VIDEO_SOURCE=portal` | unset | Required for the GNOME 50+ monitor-mirror route. GameStream/Moonlight only — no effect on `punktfunk/1` sessions. |

Client: one toggle, in Settings under **Quality** with
[the rest of the video settings](/docs/client-settings#video) — **10-bit HDR** on the Linux, macOS,
iOS, iPadOS and tvOS apps, **HDR (10-bit, BT.2020 PQ)** on Windows, **HDR** on Android. It is **on
by default** on all of them. Off means "never send me 10-bit", and the host then never upgrades the
session. Like the other video settings it can be set per [preset](/docs/presets-and-links), so a
Work preset can prefer 4:4:4 while a Couch preset prefers HDR.
