---
title: Support matrix
description: What works where — host platforms, encoders, client decoders and per-client features, each cell checked against the code that decides it.
---

Find your host desktop, GPU and client app, and read across.

## Legend

| | Meaning |
|---|---|
| ✅ | Works. |
| ⚠️ | Works, with a caveat — see the numbered note under the table. |
| ❌ | Not supported. Nothing to configure. |
| ❓ | Can't be known from this repository. |

Most codec, 10-bit and 4:4:4 cells are probed at runtime: the backend supports it, but your GPU or
driver can still say no. Features that need both ends are listed under
[Things both ends have to agree on](#things-both-ends-have-to-agree-on).

## Host platforms

Punktfunk hosts on **Linux** and **Windows**. macOS, iOS, tvOS and Android are clients only.

### Display and capture

Each session gets its own display at the client's resolution and refresh rate — see
[Virtual displays](/docs/virtual-displays).

| Host | Own virtual display | Stream a real monitor | Headless box |
|---|---|---|---|
| Windows 11 22H2+ | ✅ ¹ | ❌ ² | ✅ |
| KDE Plasma (KWin) | ✅ | ✅ | ✅ |
| GNOME (Mutter) | ✅ | ✅ | ✅ |
| gamescope (SteamOS · Bazzite) | ✅ ³ | ⚠️ ⁴ | ✅ |
| sway · scroll | ✅ ⁵ | ✅ | ⚠️ ⁶ |
| Hyprland | ✅ | ✅ | ⚠️ ⁶ |
| Cinnamon (Mint · LMDE) | ❌ ⁷ | ❌ | ❌ |
| River, dwl, COSMIC, niri, others | ❌ ⁸ | ❌ | ❌ |

1. Punktfunk's own display driver. Needs Windows 11 22H2 (build 22621); the installer refuses
   anything older. Each paired client gets its own monitor identity carrying its screen's HDR
   brightness, so Windows keeps per-client display settings and apps tone-map to your screen.
2. Windows lists your monitors but can't stream one. Streaming a physical monitor is Linux-only.
3. The host starts a gamescope at the client's mode, relaunches the box's Game Mode session at it
   (SteamOS, Bazzite), or attaches to one already running. An attached session on a box with a
   screen connected keeps its own mode, so the client gets a mirror. See [gamescope](/docs/gamescope).
4. Only while gamescope drives a real screen (Game Mode on a TV or handheld).
5. Driven over the compositor's IPC (`swaymsg`, `scrollmsg`). scroll hasn't been run on real
   hardware yet. See [Sway](/docs/sway).
6. The host writes the portal's output-chooser config, so a box with nobody at it can answer
   "which output?".
7. Cinnamon's compositor has no virtual-output API. A Mint or LMDE box can still stream games
   through a headless gamescope — see [Requirements](/docs/requirements#cinnamon-linux-mint-and-lmde).
8. No host backend. River is detected and takes input, but has no video path.

### Input, cursor and HDR

| Host | Input backend | Approval dialog | Client-drawn cursor | HDR10 source |
|---|---|---|---|---|
| Windows | SendInput | none ¹ | ✅ ² | ✅ ³ |
| KDE Plasma (KWin) | KWin fake input | none ⁴ | ✅ | ❌ ⁵ |
| GNOME (Mutter) | libei, direct to Mutter | none | ✅ ⁶ | ⚠️ ⁷ |
| gamescope | libei (gamescope's own) | none | ⚠️ ⁸ | ⚠️ ⁹ |
| sway · scroll | wlroots virtual pointer + keyboard | none | ❌ ¹⁰ | ❌ ⁵ |
| Hyprland | wlroots virtual pointer + keyboard | none | ❌ ¹⁰ | ❌ ⁵ |

1. The host must run in the signed-in console session, not session 0; the installed service
   starts it there. Over SSH you land in session 0 — see [Windows host](/docs/windows-host).
2. Needs the display driver's hardware-cursor channel, which the host probes once. Otherwise the
   pointer is drawn into the video.
3. The host turns on advanced colour for the session's display, so HDR works from an SDR desktop.
   Needs a 10-bit encoder and a client that asks for HDR — see [HDR](/docs/hdr).
4. KWin grants its capture and input protocols to the host's installed `.desktop` entry, so a
   headless box needs nobody to click **Allow**.
5. Virtual outputs on these compositors are 8-bit.
6. Always on: GNOME can't reliably paint the pointer into a virtual stream, so the host carries it
   separately.
7. Virtual displays are SDR. Mirroring a real HDR monitor on GNOME 50+ carries HDR, on the
   Moonlight/GameStream plane only — see [HDR](/docs/hdr).
8. Needs the patched `punktfunk-gamescope` (patch 2 or newer) and a session this host started or
   manages. Otherwise the host draws the pointer into the video.
9. Needs the patched `punktfunk-gamescope` and a session this host started or manages; an attached
   session streams SDR. See [HDR on gamescope](/docs/gamescope#hdr-on-gamescope).
10. Their portals offer no separate cursor, so the pointer is always part of the video.

### Version floors worth knowing

- **Windows 11 22H2 (build 22621)** — the display driver; the installer refuses anything older. Pen
  and touch injection need Windows 10 1809.
- **KWin** — a normal Plasma session (DRM backend) creates virtual outputs at any version; KWin's
  headless virtual backend needs **6.5.6**. Refresh rates above 60 Hz on a virtual output need
  **6.6**.
- **sway 1.8** — removes headless outputs again on teardown.
- **Hyprland** — no version gate. With `ecosystem.enforce_permissions` on (0.49+, off by default),
  grant the host `screencopy` — see [Hyprland](/docs/hyprland).
- **gamescope 3.16.22** — below it, headless capture deadlocks against PipeWire 1.6 and newer.
  **3.16.23** — below it, the Steam overlay never reaches the stream. The host warns but doesn't
  refuse.

## Encoders

| Host GPU | Backend | Codecs ¹ | HDR | 10-bit SDR | 4:4:4 |
|---|---|---|---|---|---|
| Windows · NVIDIA | NVENC | H.264 · HEVC · AV1 | ✅ | ✅ | ⚠️ ² |
| Windows · AMD | AMF | H.264 · HEVC · AV1 | ✅ | ⚠️ ³ | ❌ ⁴ |
| Windows · Intel | QSV | H.264 · HEVC · AV1 | ✅ | ❌ | ❌ |
| Windows · other | Media Foundation ⁵ | H.264 · HEVC | ❌ | ❌ | ❌ |
| Windows · none | — ⁶ | — | — | — | — |
| Linux · NVIDIA | NVENC | H.264 · HEVC · AV1 | ✅ | ✅ | ⚠️ ² |
| Linux · AMD, Intel | Vulkan Video ⁷ | HEVC · AV1 | ✅ | ⚠️ ⁷ | ❌ |
| Linux · AMD, Intel | VAAPI | H.264 · HEVC | ✅ | ✅ | ❌ |
| Any GPU | PyroWave ⁸ | wavelet | ✅ | ⚠️ ⁸ | ✅ |
| Linux · none | Software ⁹ | H.264 | ❌ | ❌ | ❌ |

1. Intersected with what your GPU and driver report. AV1 encode needs NVIDIA Ada, AMD RDNA3, Intel
   Arc or newer. If the NVENC probe can't run, the host advertises all three codecs.
2. HEVC only, where the GPU supports 4:4:4. On Windows it combines with HDR; on Linux 4:4:4 is
   8-bit, so a session asking for both gets HDR at 4:2:0.
3. HEVC only.
4. AMD's encoder never produces 4:4:4.
5. Picked when the GPU isn't NVIDIA, AMD or Intel but has a hardware encoder (an Adreno, say), and
   as the fallback when a native encoder won't open on an 8-bit 4:2:0 H.264 or HEVC session.
   Windows on ARM64 has only this backend.
6. Windows has no software encoder: a box with no usable GPU encoder can't stream.
7. HEVC and AV1 where the device opens that profile; anything else goes to VAAPI. 10-bit SDR is
   AV1 only here — 10-bit SDR HEVC runs on VAAPI. **Vulkan encoding** in
   [Host → Settings](/docs/configuration#settings-in-the-web-console) turns this backend off.
8. [PyroWave](/docs/pyrowave) runs only when the client picks it. Full chroma on any vendor; modes
   around 8K fall back to 4:2:0. 10-bit SDR on Linux only. Not in Windows ARM64 builds.
9. Only with `PUNKTFUNK_ENCODER=software`; `auto` never picks it on Linux.

HEVC 4:4:4 means an NVIDIA host, or pick PyroWave. Asking for 4:4:4 is a client setting, off by
default, on the Linux, Windows and Apple clients; Android has none, and GameStream sessions are
always 4:2:0. See [Client settings](/docs/client-settings#video).

### How the host picks a backend

- **Linux** — a GPU picked in the web console decides by vendor. NVIDIA needs the proprietary
  driver (`/dev/nvidiactl` or `/dev/nvidia0`); a nouveau card takes the AMD/Intel path. With no
  pick, a CUDA capture or an NVIDIA device node means NVENC, and anything else takes Vulkan Video
  for HEVC and AV1, with VAAPI as the fallback and for H.264. `auto` never picks software.
- **Windows** — the selected GPU's vendor decides: NVIDIA → NVENC, AMD → AMF, Intel → QSV. An
  unrecognised GPU falls through to the first one in the box with a known vendor, then to Media
  Foundation; with neither, the session fails. An encoder pin that contradicts the selected GPU is
  ignored, and the console flags it.
- **Codec** — the client's preferred codec wins if the host can encode it; otherwise
  **HEVC → AV1 → H.264**. PyroWave only when the client asks for it.

### Zero-copy capture to encode

- **Windows** — the display driver encodes on the GPU and the host only packetises; no backend
  reads frames back to the CPU.
- **Linux** — VAAPI takes the captured buffer directly, NVIDIA imports it through an isolated
  worker process, and PyroWave imports it on any vendor. gamescope hands over tiled buffers only
  when it is the patched build this host started; any other gamescope offers linear ones. An import
  that keeps failing drops that capture to linear buffers, then to the CPU path.

### What is in the build you installed

- **Windows installer** — the encoders live in the display driver: NVENC, AMF, QSV, Media
  Foundation and PyroWave on x64; Media Foundation only on ARM64.
- **Linux packages** (Arch, RPM, deb, Nix) — NVENC, Vulkan Video, VAAPI, software and PyroWave.
- **A hand `cargo build` on Linux** — VAAPI, software and PyroWave. NVENC (`nvenc`) and Vulkan
  Video (`vulkan-encode`) are cargo features: without `nvenc`, an NVIDIA box takes the AMD/Intel
  path and fails there. See [Build from source](/docs/developers/build-from-source).

## Client decode

No Punktfunk host or client contains FFmpeg. Every client decodes with the platform's own
decoders, with openh264 and rav1d as the software floor on the desktop.

| Client | Decode path | Codecs | 10-bit / HDR | 4:4:4 |
|---|---|---|---|---|
| Linux desktop | Vulkan Video → VAAPI → software ¹ | H.264, HEVC, AV1 ² | ✅ ³ | ⚠️ ⁴ |
| Windows desktop (x64, ARM64) | Vulkan Video / D3D11VA → software ¹ | H.264, HEVC, AV1 ² | ✅ ³ | ⚠️ ⁴ |
| Steam Deck (Decky) | as Linux desktop ⁵ | H.264, HEVC, AV1 ² | ✅ | ⚠️ ⁴ |
| macOS · iOS · tvOS | VideoToolbox; Metal for PyroWave ⁶ | H.264, HEVC, AV1 ⁶ | ⚠️ ⁷ | ⚠️ ⁸ |
| Android · Android TV | MediaCodec; Vulkan for PyroWave ⁹ | H.264, HEVC, AV1 ¹⁰ | ⚠️ ⁷ | ❌ |
| Moonlight | your Moonlight app's | negotiated | ⚠️ ¹¹ | ❌ |
| LG webOS · browser | ❓ ¹² | ❓ | ❓ | ❓ |

1. Linux tries Vulkan Video first wherever it decodes the codec, then VAAPI (skipped on NVIDIA),
   then software. Windows tries Vulkan Video first on NVIDIA and AMD, D3D11VA first on Intel and
   others. Pin one in the client's decoder setting or with
   [`PUNKTFUNK_DECODER`](/docs/configuration#client-side-native-clients); a pinned decoder that
   fails still falls through. Mid-session, three decode errors spread over at least a second move
   to the next decoder, and the session log names the one in use.
2. The software floor covers H.264 (openh264) and AV1 (rav1d), 8-bit only, with no HEVC. AV1 is
   offered only where the GPU decodes it; Windows drops HEVC when the GPU has no HEVC decoder. If
   every HEVC decoder fails, the client reconnects on a codec with a software floor. PyroWave is
   offered when the GPU passes its probe.
3. HDR is on by default. Linux presents HDR10 where the desktop offers it and tone-maps otherwise;
   Windows asks for HDR only when the selected output is in HDR mode. The software decoder refuses
   10-bit. 10-bit SDR is a client setting.
4. Opt-in **Full chroma**, off by default. The client asks for 4:4:4 only when its GPU
   hardware-decodes 4:4:4 HEVC, in practice NVIDIA; PyroWave needs no probe. There is no software
   fallback. The Detailed [stats overlay](/docs/stats) shows `4:4:4→4:2:0` when the host declined.
5. The plugin launches the Linux client, so decoding is identical.
6. AV1 only where VideoToolbox hardware-decodes it (M3-class Macs, A17 Pro-class iPhones).
   PyroWave decodes on Metal on A13-class chips and newer, when you pick it.
7. Probed against the display: EDR headroom on Mac, iPhone and iPad, HDR eligibility on Apple TV,
   HDR capabilities on Android. An SDR screen advertises no HDR.
8. Opt-in, and offered only where VideoToolbox hardware-decodes 4:4:4. HEVC 4:4:4 needs an NVIDIA
   host.
9. A ranked list that prefers hardware, low-latency decoders and blocks known-bad ones; there is no
   software rung. PyroWave needs a 64-bit device with Vulkan 1.3, and you pick it.
10. H.264 and HEVC are assumed; AV1 is probed.
11. The host offers HDR to Moonlight only when it can deliver it. See [Moonlight](/docs/moonlight).
12. Separate repositories; ask those projects.

## Clients and features

Linux, Windows, the Decky plugin and the `punktfunk` CLI stream through the same session binary.
Apple is one app across macOS, iOS/iPadOS and tvOS. Android TV is the Android app.

### Before you connect

| Client | Presets | `punktfunk://` links | Game library | Speed test | Wake-on-LAN | Updates itself |
|---|---|---|---|---|---|---|
| Linux desktop | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ⚠️ ² |
| Windows desktop | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ❌ ³ |
| macOS | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ❌ ³ |
| iPhone · iPad | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ❌ ³ |
| Apple TV | ✅ ⁴ | ✅ | ✅ ¹ | ✅ | ✅ | ❌ ³ |
| Android · Android TV | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ❌ ³ |
| Decky (Steam Deck) | ⚠️ ⁵ | ❌ | ✅ ⁶ | ❌ | ✅ | ✅ ⁷ |
| `punktfunk` CLI | ✅ | ✅ ⁸ | ✅ | ✅ | ✅ | ❌ |
| Moonlight | ❌ | ❌ | ✅ ⁹ | ❓ | ❓ | ❓ |

1. On any paired host. See [Game library](/docs/game-library).
2. System packages update through a packaged helper; a Flatpak updates with `flatpak update` (the
   Decky plugin runs it for you); other installs show the command. See
   [Keeping a client up to date](/docs/install-client#keeping-a-client-up-to-date).
3. Through the store or installer you got it from.
4. Made in the Apple TV's **Settings**. Presets don't sync between devices, and tvOS leaves out the
   settings it has no input for.
5. Streams with the presets pinned to a host in a client on the same device; it has no preset
   editor of its own.
6. Paired hosts' libraries appear in Steam's **Play from** menu and in the client's console home.
7. Updates itself and, where the install allows, the client it launches.
8. Follows links; the graphical apps register the URL scheme.
9. As ordinary GameStream apps. Moonlight's own features depend on your Moonlight app.

### Input while you stream

| Client | Gamepads | Rumble | Gyro · touchpad · triggers | Pen | Touch modes | Mouse modes |
|---|---|---|---|---|---|---|
| Linux · Windows desktop | ✅ ¹ | ✅ | ✅ ² | ❌ ³ | ⚠️ ⁴ | ⚠️ ⁵ |
| macOS | ✅ ¹ | ✅ | ✅ ² | ❌ | ❌ | ⚠️ ⁵ |
| iPhone · iPad | ✅ ¹ | ✅ | ✅ ² | ✅ ⁶ | ✅ | ⚠️ ⁷ |
| Apple TV | ✅ ¹ | ✅ | ✅ ² | ❌ | ❌ | ⚠️ ⁸ |
| Android · Android TV | ✅ ¹ | ⚠️ ⁹ | ⚠️ ¹⁰ | ✅ ¹¹ | ✅ ¹² | ✅ |
| Moonlight | ✅ | ✅ | ❌ ¹³ | ⚠️ ¹⁴ | ⚠️ ¹⁵ | — |

1. Several controllers, each on a stable slot. The pad-type picker on Linux, Windows and the
   console home offers Automatic, Xbox 360, Xbox One, DualSense, DualShock 4, Steam Deck and Steam
   Controller 2; Apple and Android offer the first six, with Steam Controller 2 as its own switch.
2. DualSense and DualShock 4 touchpad and motion are forwarded, and adaptive triggers and the
   lightbar play back on a real DualSense. Any pad with a gyro sends motion. The host's virtual pad
   needs a gyro to use it: Xbox pads have none, and **Automatic** picks Xbox for pads it doesn't
   recognise. Pick a DualSense-class **Controller type** for motion; Apple and Android say so on
   screen. See [Client settings](/docs/client-settings#input).
3. The desktop clients send no pen input.
4. Touchscreens only, and not yet tried on a Windows touchscreen.
5. A gamescope host takes relative mouse input only, so the desktop mouse mode isn't available
   there. See [Mouse modes](/docs/input#mouse-modes).
6. Apple Pencil with pressure, tilt, azimuth and hover, plus barrel roll from Pencil Pro on iOS
   17.5+. Needs a host that injects pen — see [Pen and stylus](/docs/input#pen-and-stylus).
7. iPad captures the pointer only in a full-screen front window — not in Stage Manager, Slide Over
   or on iPhone. **Capture pointer for games** turns it off.
8. The Siri Remote is a relative trackpad.
9. Uses the controller's motor where the phone exposes it. A setting also plays player 1's rumble
   on the phone's own motor (the iPhone has the same with the Taptic Engine).
10. Full motion and touchpad need the pad on USB (claimed by default); over Bluetooth, Android 12+
    forwards the gyro only. Adaptive triggers are dropped — Android has no API for them.
11. Stylus with eraser, both barrel buttons and hover.
12. Not on Android TV.
13. The host's GameStream plane takes controller events, not the motion, touchpad or trigger
    extensions.
14. The host takes Moonlight's pen and touch extensions; whether your Moonlight app sends them is
    up to it.
15. Pressure and contact area are dropped.

Keyboard chords, and what each host can type, are in [Input](/docs/input).

### Picture and sound while you stream

| Client | HDR | 4:4:4 | Surround 5.1 / 7.1 | Microphone | Clipboard | Stats overlay |
|---|---|---|---|---|---|---|
| Linux desktop | ✅ ¹ | ⚠️ ² | ✅ ³ | ✅ | ❌ ⁴ | ✅ |
| Windows desktop | ✅ ¹ | ⚠️ ² | ✅ ³ | ✅ | ⚠️ ⁵ | ✅ |
| macOS | ⚠️ ⁶ | ⚠️ ⁷ | ✅ ³ | ✅ | ✅ ⁸ | ✅ |
| iPhone · iPad | ⚠️ ⁶ | ⚠️ ⁷ | ✅ ³ | ✅ | ✅ ⁸ | ✅ |
| Apple TV | ⚠️ ⁶ | ⚠️ ⁷ | ✅ ³ | ❌ ⁹ | ❌ ⁹ | ✅ |
| Android · Android TV | ⚠️ ⁶ | ❌ | ✅ ³ | ✅ | ⚠️ ¹⁰ | ✅ |
| Moonlight | ⚠️ ¹¹ | ❌ | ✅ ³ | ❌ | ❌ | ❓ ¹² |

1. On by default, where the presenter can show it; Windows also needs the output in HDR mode.
2. Opt-in **Full chroma**, offered where the GPU hardware-decodes 4:4:4 HEVC, or on any GPU for
   PyroWave. HEVC 4:4:4 needs an NVIDIA host. A Windows host combines it with HDR; a Linux host
   keeps HDR and drops to 4:2:0, except with PyroWave. The stats overlay shows what you got.
3. The host rounds the request to stereo, 5.1 or 7.1 and says so before the first frame.
4. The switch is there, but the Linux client's side of the clipboard isn't built: nothing crosses.
5. Text and PNG images; an incoming image over 4 MiB is skipped.
6. Probed against your display — see note 7 under [Client decode](#client-decode).
7. Opt-in and probed. HEVC 4:4:4 needs an NVIDIA host; PyroWave works on any.
8. Text, RTF, HTML and images, fetched on paste in both directions. On iOS an unpasted host copy
   (up to 8 MiB) is pulled across when the session ends.
9. tvOS gives apps no microphone and no pasteboard.
10. Text only.
11. Decided by the host.
12. Moonlight has its own overlay; [stats](/docs/stats) covers Punktfunk's.

No client offers file transfer through the clipboard yet. See [Shared clipboard](/docs/clipboard).

### Things both ends have to agree on

Either side can be the reason one of these didn't happen:

- **Shared clipboard** — off on the host until an operator turns it on, and a per-host switch on
  the client. Only the Apple app greys **Share Clipboard** out when the host hasn't offered it.
- **Pen input** — the host offers it only with a usable `/dev/uinput` (Linux) or Windows 10 1809+.
  Without it, clients turn pen into touch.
- **Committed text (IME)** — the Windows host and the wlroots backend (sway, scroll, Hyprland) type
  any text. KDE, GNOME and gamescope fall back to key presses, which can't express every character.
- **Client-drawn cursor** — needs the client in desktop mouse mode and a host that carries the
  cursor separately ([table](#input-cursor-and-hdr)). Then the host stops drawing the pointer into
  the video.
- **10-bit, HDR and 4:4:4** — see [HDR](/docs/hdr).

## How finished each part is

| Part | Where it stands |
|---|---|
| **Protocol core** — `punktfunk-core`, the C ABI, FEC and crypto | Stable; the wire format and the C ABI are versioned contracts (see [Mixing versions](#mixing-versions)). |
| **Linux host** | The most exercised host. What it can do depends on the desktop — see the tables above. |
| **Windows host** | An installer with its own display driver. Setup has a publicly trusted signature; the drivers carry Punktfunk's own, which setup trusts on install ([About the signatures](/docs/windows-host#about-the-signatures)). NVENC is well trodden; AMF and QSV see less field use. Runs in the signed-in console session, not session 0. |
| **GameStream / Moonlight plane** | Works, and is off until you turn on **GameStream**. Pairs over plain HTTP — trusted LAN only ([Security](/docs/security#gamestream--moonlight-compatibility-is-the-weak-crypto-path)). Presets, links, clipboard and microphone aren't on it. |
| **Linux and Windows desktop clients** | Packaged; one codebase with the Decky plugin and the CLI. Windows ships for x64 and ARM64. |
| **Apple client** (macOS · iOS · iPadOS · tvOS) | One universal build on TestFlight; the Mac also has a notarized DMG. No microphone or clipboard on tvOS. |
| **Android client** (phone · TV) | Google Play, with canary builds on its beta track, plus a sideloadable APK. |
| **Decky plugin** (Steam Deck) | Installed from a URL, not the Decky store; updates itself and the client it launches. It starts the Linux client rather than streaming itself. |
| **Web console** | Manages the host: dashboard, sessions, pairing, library, displays, plugins, logs, stats, settings and updates. No speed test or bitrate setting — the client apps have those. |
| **Plugins** | First-party launcher, ROM and USB plugins plus an SDK, installed from the console. See [Plugins](/docs/plugins). |
| **`pf-webos`** (LG TV) · browser client | Separate repositories; ask those projects. |

## Mixing versions

Update a host and its clients in any order; paired devices stay paired. A feature one end doesn't
know is not offered, and the session goes ahead without it — see
[Things both ends have to agree on](#things-both-ends-have-to-agree-on).

| Contract | Current | Rule |
|---|---|---|
| `punktfunk/1` wire version | **2** | Client and host must match. |
| C ABI version | **37** | Between an app and the core library it ships with. |
| Windows display-driver protocol | **9** | Host and driver must match; the installer updates both. |
| Windows virtual-gamepad channel | **3** | Host and pad driver must match; same installer. |

## Where it has been run

Development and testing run on Linux hosts under KWin, Mutter, gamescope, wlroots and Hyprland on
NVIDIA and AMD GPUs, and on Windows hosts on NVIDIA, AMD and Intel, with the macOS, Linux, Windows
and Android clients and stock Moonlight. HDR on gamescope has been run end to end on Bazzite and
SteamOS.

## What is not verified

- **scroll as a host** — detected and driven like sway, not yet run on real hardware.
- **gamescope headless capture on the proprietary NVIDIA driver** — plausible, not well trodden,
  and no probe would catch it failing.
- **Touch input from a Windows client** — the same code as Linux, not run on a Windows touchscreen.
- **Re-applying your resolution after an Omarchy theme switch** — unit-tested, not tried during a
  live stream. If a theme switch costs you your resolution, report it.
- **`pf-webos` and the browser client** — separate repositories.
- **Moonlight's client-side features** — Wake-on-LAN, overlays and updates differ per Moonlight app.

For what is planned rather than shipped, see the [Roadmap](/docs/roadmap).
