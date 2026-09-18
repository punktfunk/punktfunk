---
title: Support matrix
description: What actually works where — host platforms, encoders, client decoders and per-client features, each cell taken from the code that decides it rather than from a feature name.
---

Find your row — your host desktop, your GPU, your client app — and read across. Every cell is
taken from the code path that makes the decision, not from a feature's name. Support is
non-uniform by design: each compositor and GPU vendor gets its own backend, so the honest answer
to "does Punktfunk do X?" is usually "on which host, with which GPU, from which client".

## Legend

| | Meaning |
|---|---|
| ✅ | Works. |
| ⚠️ | Works, with a caveat — the numbered note under the table says which. |
| ❌ | Not supported. Nothing to configure. |
| ❓ | Untested or unverifiable from this repository. The note says what would settle it. |

## How to read a cell

A cell's "yes" can be decided four different ways — which tells you *when* it can turn into a "no"
on your machine:

- **Compiled in** — decided at build time. No setting changes it: a hand-built host has no NVENC.
- **Probed at runtime** — the host or client asks your driver, GPU or compositor and believes the
  answer. The backend supports it; **your device may still refuse it**. Most codec, 10-bit and
  4:4:4 cells are this kind.
- **Negotiated** — both ends must advertise it; the clipboard, pen input, 4:4:4 and client-drawn
  cursors all die if either side says no. The client's stats overlay prints `4:4:4→4:2:0` rather
  than letting you assume you got what you asked for.
- **Default on / opt-in / operator-gated** — HDR and 10-bit are attempted by default; lossless
  audio waits for a client to ask; the shared clipboard is off until an operator turns it on.

A few capabilities also **latch off for the rest of the host process** after a failure — a lost HDR
negotiation, a repeatedly dying zero-copy import worker. If a capability worked yesterday and not
today, restart the host before believing the table is wrong.

## Host platforms

Punktfunk hosts on **Linux** and **Windows** only. macOS, iOS, tvOS and Android are client-only
platforms — every host entry point on them fails at compile time, so there is no configuration that
turns a Mac into a host.

### Display and capture

By default every session gets its own display at your client's exact resolution and refresh rate —
how that is built differs per compositor, and [Virtual displays](/docs/virtual-displays) covers the
mechanics. The one exception is a gamescope the host only *attaches* to, which keeps its own mode
(note 4).

| Host | Own virtual display | Mirror a real monitor | Headless box |
|---|---|---|---|
| Windows 11 22H2+ | ✅ ¹ | ❌ ² | ✅ |
| KDE Plasma (KWin) | ✅ | ✅ | ✅ |
| GNOME (Mutter) | ✅ ³ | ✅ | ✅ |
| gamescope (SteamOS · Bazzite) | ✅ ⁴ | ⚠️ ⁵ | ✅ ⁶ |
| sway ⁹ | ✅ | ✅ | ⚠️ ⁷ |
| Hyprland | ✅ ⁸ | ✅ | ⚠️ ⁷ |
| Cinnamon (Mint · LMDE) | ❌ ¹⁰ | ❌ ¹⁰ | ❌ ¹⁰ |
| macOS / anything else | ❌ | ❌ | ❌ |

1. Punktfunk's own IddCx display driver. It requires **Windows 11 22H2 (build 22621) or newer** —
   the driver declares IddCx 1.10 and the installer enforces the floor. This is also the only
   backend that can give each client a **stable display identity**: it writes an EDID serial per
   paired client and codes your client panel's HDR luminance into it, so host apps tone-map to the
   screen the stream actually lands on.
2. Enumeration and capture are separate capabilities, and Windows only has the first. The host
   lists your real monitors (they explain the topology), but there is no capturer that can point at
   one, so the console renders the picker read-only and a saved pin is dropped rather than stored.
   Streaming a specific physical monitor is a **Linux-only** feature.
3. Sizing works the other way round here: Mutter sizes the virtual monitor from the video format
   negotiation rather than from the create call. Also, all virtual-monitor changes are serialized
   host-wide on purpose — concurrent ones have crashed gnome-shell.
4. gamescope is not a protocol the host talks to, and it resolves one of three sub-modes. It
   **spawns** a bare nested gamescope at your client's exact size and refresh (the plain-distro
   default); it **manages** a `gamescope-session-plus` / SteamOS session, relaunching it at your
   client's mode (Bazzite and SteamOS, which have that infrastructure); or it **attaches** to a
   gamescope someone else started. An attach is the only one that is not sized for you — a box
   driving a physical panel is mirrored at *its* mode, because re-moding it would flip the screen
   someone is sitting in front of. In every mode the host captures gamescope's built-in video node.
   See [gamescope](/docs/gamescope).
5. Only when gamescope is driving a real connector (a Game Mode session on a TV or handheld).
   A nested or headless gamescope reports no heads and the picker is legitimately empty.
6. The host injects a small splash client on purpose: a fresh gamescope only produces frames once
   something paints, and a game's own bootstrap paints nothing for several seconds.
7. Yes, but it needs managed configuration rather than being dialog-free by nature. The host
   writes and merges the portal's chooser configuration so a headless box can answer the
   "which output?" question that otherwise requires a GUI.
8. Contracts are verified against Hyprland 0.55.4 with xdph 1.3.x, and the virtual output was
   exercised end to end on a real Omarchy 4.x box for 0.33.0 — create-output through live stream.
   One piece of that box is still untested on glass; see
   [What is not verified](#what-is-not-verified).
9. Sway specifically, not the wlroots family. Creating the headless output, setting its mode and
   listing your monitors all go through sway's IPC (`swaymsg`), so a wlroots compositor without it
   (River, dwl, …) cannot host — the session fails at `swaymsg get_outputs`. Their input would work
   (they do have the wlroots virtual pointer and keyboard protocols), but with no video there is no
   stream. See [Sway / wlroots](/docs/sway).
10. Cinnamon's compositor **Muffin** exposes no virtual-output API and no monitor-capture route we
    can reach: it forked from Mutter 3.36, so `org.cinnamon.Muffin.ScreenCast` has only
    `RecordMonitor` / `RecordWindow` and never Mutter 42's `RecordVirtual`, and its portal backend
    (`xdg-desktop-portal-xapp`) implements no ScreenCast at all. Nothing in Punktfunk can change
    this. A Mint or LMDE box can still stream **games** through a headless gamescope, which needs no
    desktop compositor — see [Requirements → Cinnamon](/docs/requirements#cinnamon-linux-mint-and-lmde).

### Input, cursor and HDR

| Host | Input backend | Approval dialog | Cursor | HDR10 source |
|---|---|---|---|---|
| Windows | SendInput | none ¹ | ✅ ² | ✅ ³ |
| KDE Plasma (KWin) | `fake_input` | none ⁴ | ✅ | ❌ ⁵ |
| GNOME (Mutter) | libei (direct) | none ¹¹ | ⚠️ ⁶ | ⚠️ ⁷ |
| gamescope | libei (gamescope's own) | none | ⚠️ ⁸ | ⚠️ ⁹ |
| sway / wlroots | virtual pointer + keyboard | none | ⚠️ ¹⁰ | ❌ ⁵ |
| Hyprland | virtual pointer + keyboard | none | ⚠️ ¹⁰ | ❌ ⁵ |

1. But the host process must run as SYSTEM **in the interactive console session**, not in session 0.
   A plain session-0 service cannot capture or inject — the installed service is a supervisor that
   launches the real host into the console session for exactly this reason. Over SSH you land in
   session 0; see [Windows host](/docs/windows-host).
2. Two modes, chosen per session. By default the desktop compositor draws the pointer into the
   frame (which is why the host also installs a resident virtual mouse — a headless Windows box
   otherwise thinks no mouse is present and draws nothing). If your client asks to draw the cursor
   itself, the display driver must speak the hardware-cursor channel; the host probes for that once.
3. The host turns advanced colour on for your session's virtual display itself, so PQ flows even
   from an SDR desktop. It still needs a 10-bit-capable encoder and a client that asked for HDR —
   see [HDR](/docs/hdr).
4. Authorization comes from the host's installed `.desktop` entry (`X-KDE-Wayland-Interfaces`),
   which is how KWin grants the restricted `zkde_screencast` protocol without a pop-up. A headless
   box needs nobody to click "Allow".
5. Virtual outputs on these compositors are 8-bit. This is a compositor limitation, not a setting.
6. Cursor metadata is used for effectively every session because GNOME's "compositor draws it"
   mode is not real on a virtual stream from Mutter 48 onwards — recorded frames come out without
   a pointer, and pointer-only movement schedules no new frame at all.
7. There is a second, separate Linux HDR route: mirroring a real HDR monitor on **GNOME 50+**. It
   is available on the Moonlight/GameStream plane only, it must be asked for explicitly, and it is
   re-checked live against the monitor's current colour mode. See [HDR](/docs/hdr).
8. gamescope puts the pointer on a hardware plane, so no cursor arrives in the captured video. The
   host reconstructs and draws it, and only skips that blend when three things hold at once: the
   resolved binary is the patched `punktfunk-gamescope` build **at patch revision 2 or newer**
   (revision 1 carries HDR only), **this host spawned or manages the session** — an attach to a
   gamescope someone else started can promise nothing — and no earlier spawn was seen coming up
   without the flags we asked for. Note 9 gates HDR on the same two terms.
9. The only Linux virtual output that can be 10-bit, and it needs three things to be true at once:
   HDR attempts are on (they are, by default), the resolved gamescope binary is the patched
   `punktfunk-gamescope` build, and **this host spawned or manages the session**. Attaching to a
   gamescope someone else started can never be HDR. See
   [HDR on gamescope](/docs/gamescope#hdr-on-gamescope).
10. Cursor metadata is wired up but has **never been validated on real hardware** on these two
    backends. It should work; nobody has confirmed it. The compositor-draws-it mode is the tested
    path.
11. Not a desktop entry — on GNOME the host talks to Mutter's own `org.gnome.Mutter.RemoteDesktop`
    D-Bus EIS directly rather than the xdg RemoteDesktop portal. That interface asks for no
    interactive approval at all, which is the point: the portal's `Start()` would simply time out
    on a headless box with nobody there to answer it.

### Version floors worth knowing

- **Windows 11 22H2 / build 22621** — the virtual display driver. Windows 10 1809+ is enough for
  pen and touch injection, but the installer will not proceed below 22621.
- **KWin** — the compositor must implement `createVirtualOutput`. The **DRM backend**, which is
  what an ordinary Plasma session runs, does at **any** version; KWin's headless **virtual
  backend** (`kwin_wayland --virtual`, what the headless test path uses) only since **6.5.6** —
  below that the request fails with "Could not find output". **6.6** adds custom modes on virtual
  outputs, which is how the host reaches refresh rates above 60 Hz. KWin **6.7+** changed its
  output model again; that is handled.
- **sway 1.8** — headless outputs can be removed again on teardown.
- **Hyprland** — no version gate in the host: the `hyprctl` mode-rule path is version-independent
  and the running version is read for the log only. Contracts are verified against **0.55.4 with
  xdph 1.3.x**. On **0.49+**, if you have turned `ecosystem.enforce_permissions` on (it is off by
  default), grant the host screencopy and virtual pointer/keyboard — a denial shows up as silent
  black frames and dropped input, not as an error.
- **gamescope 3.16.22** — below it, headless capture deadlocks against PipeWire 1.6 and newer.
  **3.16.23** — below it, the Steam overlay never reaches the stream.

## Encoders

Every codec cell below is a **runtime probe**: the host opens a tiny real encoder, or asks the
driver's capability list, once per GPU and codec, and believes the answer. Your silicon may decline
what the backend supports — AV1 encode in particular is narrow (Ada and newer on NVIDIA, RDNA3 and
newer on AMD, Arc and newer on Intel).

| Host GPU | Backend | Codecs | 10-bit / HDR | 4:4:4 |
|---|---|---|---|---|
| Windows · NVIDIA | NVENC (direct SDK) | probed ¹ | ✅ probed | ⚠️ ² |
| Windows · AMD | AMF (native) | probed | ✅ probed | ❌ ³ |
| Windows · Intel | QSV (native) | probed | ⚠️ ⁴ | ❌ |
| Windows · any | PyroWave | wavelet ⁵ | ✅ | ✅ |
| Windows · none | no encode path | — | ❌ | ❌ |
| Linux · NVIDIA | NVENC (direct SDK) | probed ¹ | ✅ ⁶ | ⚠️ ² |
| Linux · AMD, Intel | Vulkan Video | HEVC, AV1 ⁷ | ⚠️ probed | ❌ |
| Linux · AMD, Intel | VAAPI | probed | ⚠️ probed | ❌ ⁹ |
| Linux · any | PyroWave | wavelet ⁵ | ❌ ⁸ | ✅ |
| Linux · none | software H.264 ¹⁰ | H.264 only | ❌ | ❌ |

1. H.264, HEVC and AV1, intersected with what the driver reports. If the probe cannot run (no
   driver, or a build without NVENC), the host advertises the full set rather than nothing — so an
   advertised codec is not always a *confirmed* codec.
2. HEVC only, and only when the GPU's 4:4:4 capability bit says yes. **HDR and 4:4:4 together
   depend on the platform.** On Windows they compose: the IDD-push capturer converts the FP16
   desktop to packed 10-bit BT.2020 PQ RGB and NVENC encodes HEVC Main 4:4:4 10, so a session gets
   both. On Linux 4:4:4 rides an 8-bit `YUV444P` route, so a session that negotiates both resolves
   the bit depth back to 8 — full chroma wins and the stream is SDR. Either way the answer is
   settled before the Welcome, so the client is never told one thing and sent another.
3. A hardware limitation of AMD's encode block, not a gap in Punktfunk. VCN never encodes 4:4:4,
   so there is nothing to probe.
4. Only in a build that includes the native QSV backend — which the shipped installer does. In a
   hand build without it, 10-bit is honestly reported as unavailable rather than guessed.
5. [PyroWave](/docs/pyrowave) is a wavelet codec, not H.26x. It is never picked automatically: your
   client has to ask for it by name in its codec setting. Its 4:4:4 is the one that needs no GPU
   encode probe — it does its own full-chroma colour conversion, so it resolves on any vendor —
   with a single ceiling: the vendored rate controller packs its block index into 16 bits, which an
   ≈8K-class 4:4:4 mode overflows, so those modes are downgraded to 4:2:0 before the Welcome.
6. Requires the direct-SDK NVENC path (which every shipped Linux package builds). Without it the
   frame takes a slower CPU route to reach 10-bit.
7. H.264 never uses this backend, and it exists only in a build carrying the Vulkan-encode feature
   (every shipped Linux package does). Even then the device is asked, per codec and per bit depth,
   whether it can open that encode profile; a "no" routes to VAAPI before a session is burned. A
   Vulkan open that fails falls back to the native VAAPI session, which takes a producer's
   NV12 as it is (no conversion pass), so a gamescope session that negotiated NV12 keeps
   streaming either way.
8. PyroWave is 8-bit on Linux. The 10-bit path exists only on Windows.
9. Not a hardware limit — the VAAPI backend simply has no 4:4:4 path yet, so the probe declines
   unconditionally and the session is negotiated as 4:2:0.
10. Explicit-only on Linux. `auto` never resolves here — a box with no usable GPU driver fails the
    session at encoder open rather than quietly encoding on the CPU, so set
    `PUNKTFUNK_ENCODER=software` deliberately if that is what you want. Windows has no software
    encoder at all — the pf-vdisplay driver encodes, so a host without a usable render adapter
    fails at encoder open.

**4:4:4 across the whole project:** only HEVC and PyroWave can carry it, and only NVENC and
PyroWave can produce it — so on the HEVC side full chroma means an NVIDIA host. Asking for it is a
client setting, off by default, and the **Linux, Windows and Apple** clients all have a working one;
**Android does not implement 4:4:4 at all**, and GameStream sessions are always 4:2:0. See
[Client settings](/docs/client-settings).

### How the host picks a backend

It is **not** a ladder, and the two platforms differ:

- **Linux** — a two-way decision, and the web console's GPU preference outranks the probe. If the
  console names a specific adapter, that adapter's vendor decides: AMD or Intel → the AMD/Intel
  opener, even on a box that also has an NVIDIA card; NVIDIA → NVENC, but still only if the
  proprietary driver's `/dev/nvidiactl` or `/dev/nvidia0` is there, so a nouveau card falls to the
  AMD/Intel opener. With no preference set, a live CUDA capture or an NVIDIA device node means
  NVENC; otherwise the AMD/Intel opener, which tries Vulkan Video first for HEVC and AV1 and falls
  back to VAAPI. Linux `auto` **never** lands on the software encoder — you have to ask for it,
  deliberately, so that a box with a broken driver fails loudly instead of quietly encoding on the
  CPU.
- **Windows** — the host reads the vendor of the selected render adapter: NVIDIA → NVENC,
  AMD → AMF, Intel → QSV. An unrecognised adapter falls
  through to the first adapter in the box that *does* have a recognised vendor, and only if none
  does → software H.264. If you pin an encoder whose vendor contradicts the selected adapter, the
  adapter wins and the log says so, because honouring the pin could only fail.

Codec negotiation itself is the same everywhere: your client's list is intersected with the host's,
a single explicit client preference wins if the host can also produce it, and otherwise the order is
**HEVC → AV1 → H.264**. PyroWave is outside that order entirely — preference only.

### Zero-copy capture to encode

This is a **GPU and encoder** question, not a compositor one, which is why it is not a column above.

- **Windows** — frames never leave the GPU, and since the encoder moved into the display driver
  they never leave that process either: the driver encodes what the compositor composed, on the
  pooled device its swap chain was assigned to, and publishes a bitstream the host only packetises.
  All three native backends (NVENC, AMF, QSV) take that surface directly and have no readback path
  at all, so no GPU → CPU → GPU round-trip is left on this platform.
- **Linux** — VAAPI takes the captured buffer directly; NVIDIA imports it through an isolated
  worker process; PyroWave imports it on any vendor. Two compositor-shaped edges matter: gamescope
  offers only linear buffers, and GNOME on NVIDIA allocates tiled-only — which PyroWave can still
  import, so that combination keeps zero-copy where H.26x would not. Repeated import failures latch
  the host onto the CPU path for the rest of its life.

### What is actually in the build you installed

- **Windows installer** — NVENC, AMF and QSV in one executable, plus PyroWave.
- **Linux packages** (Arch, RPM, deb, Nix) — NVENC and Vulkan Video, plus PyroWave. VAAPI and the
  software encoder are always compiled in.
- **A hand `cargo build` of the host** compiles in only what needs no cargo feature: VAAPI and the
  software encoder on Linux; native AMF and the software encoder on Windows; plus PyroWave, which
  is a *default* feature. Direct-SDK NVENC, Vulkan Video and native QSV are opt-in. On either
  platform an NVIDIA or Intel box then **fails the session at encoder open** with a "rebuild with
  `--features …`" error rather than quietly degrading, while an AMD box is fully served — by
  VAAPI on Linux, native AMF on Windows. If you built from source and your GPU seems unused,
  check this first.

## Client decode

**Nothing punktfunk ships contains FFmpeg** — host or client. Every decoder below is the platform's
own — Vulkan Video, DXVA, VAAPI, VideoToolbox, MediaCodec — driven directly from punktfunk's own
bitstream parser, with openh264 + rav1d as the CPU floor on the desktop. There is no libav* in any
package, and nothing to install.

| Client | Decode path (in order) | Codecs | 10-bit / HDR | 4:4:4 |
|---|---|---|---|---|
| Linux desktop | Vulkan Video → VAAPI → software ¹ | H.264, HEVC, AV1 ² | ✅ ³ | ⚠️ ⁴ |
| Windows desktop | Vulkan Video → D3D11VA → software ¹ | H.264, HEVC, AV1 ² | ✅ ³ | ⚠️ ⁴ |
| Steam Deck (via Decky) | as Linux desktop ⁵ | H.264, HEVC, AV1 ² | ✅ | ⚠️ ⁴ |
| macOS · iOS · tvOS | VideoToolbox only | H.264, HEVC, AV1 ⁶ | ⚠️ ⁷ | ⚠️ ⁸ |
| Android · Android TV | MediaCodec ⁹, or Vulkan compute for PyroWave | H.264, HEVC, AV1 ¹⁰ | ⚠️ ⁷ | ❌ |
| Moonlight | your Moonlight app's | negotiated | ⚠️ ¹¹ | ❌ |
| LG webOS (`pf-webos`) | ❓ | ❓ | ❓ | ❓ ¹² |

1. **The order depends on your GPU vendor.** NVIDIA and AMD get Vulkan Video first; Intel and
   unknown vendors get the platform decoder first (VAAPI on Linux, D3D11VA on Windows), because
   Vulkan decode on Intel Arc was field-broken even though the driver advertises it. Pick one
   explicitly in Preferences or with `PUNKTFUNK_DECODER` (`native-vulkan`, `native-vaapi`,
   `native-d3d11va`, `software`). A pin skips the vendor order — that is what it is for —
   but a pinned rung that cannot open still falls through rather than ending the session,
   and says so in the log. Settings saved before M10 (`vulkan`, `vaapi`, `d3d11va`) name
   the same hardware and are migrated onto the native rung automatically.
   Mid-session demotion is laddered, and each rung needs both three
   consecutive decode errors *and* a full second of them: Vulkan Video first demotes to VAAPI on
   Linux or D3D11VA on Windows, and only that backend demotes to software — so a startup burst no
   longer strands you. The session log names the rung it landed on, and warns when that rung/codec
   pair has no hardware run recorded behind it.
2. A statement about the decoders punktfunk BUILT, not about a codec registry: the hardware rungs
   cover all three, the CPU floor covers H.264 (openh264) and AV1 (rav1d) and has no HEVC at all.
   AV1 is advertised only where the GPU can really decode it — a CPU AV1 rung exists but a 4K AV1
   stream is not survivable on it, and the codec is fixed for the whole session once negotiated.
   HEVC is the one codec advertised without a CPU floor underneath: if every hardware rung for it
   fails, the session reconnects on a codec that has one rather than dying. PyroWave is added when
   the GPU passes its compute probe and you pick it.
3. On by default. It is presented on a real HDR10 surface where your desktop offers one (KDE HDR,
   gamescope), and tone-mapped in-shader otherwise. Software-decoded frames never take the HDR
   surface — the CPU rung is 8-bit by contract and refuses a 10-bit stream rather than mis-scaling
   it.
4. Opt-in (Settings ▸ **Full chroma**), off by default, and advertised whenever you ask — unlike
   Apple, **no client-side probe gates it**. Full chroma is a *hardware* path here: the Vulkan
   presenter samples the 2-plane 4:4:4 pool formats where the driver decodes HEVC Range Extensions
   in hardware, which today means NVIDIA. There is no software safety net under it — the CPU floor
   is 4:2:0 8-bit by contract and has no HEVC at all (note 2), so it refuses a 4:4:4 stream rather
   than converting it, and a box whose hardware 4:4:4 decode fails lands on note 2's codec
   reconnect instead of a downgraded picture. None of that is silent: the Detailed
   [stats overlay](/docs/stats) prints the resolved chroma (`4:4:4→4:2:0` when the host declined)
   and the rung frames really took. Whether you get it at all is then the host's half — HEVC 4:4:4
   means an NVIDIA host, or pick PyroWave, which decodes on its own GPU compute path and so carries
   full chroma on any vendor.
5. The Decky plugin does not decode anything — it launches the Linux client, so the decode path is
   identical, including the Mesa `RADV_PERFTEST=video_decode` opt-in the session binary sets before
   any Vulkan call (without it RADV exposes no decode queue and the Deck silently falls back to
   VAAPI, which fringes chroma on that GPU). What differs on the Deck is the hardware, not the code.
6. AV1 only where `VTIsHardwareDecodeSupported` says yes (M3-class Macs, A17 Pro-class iPhones) —
   VideoToolbox has no software AV1 decoder, so it is never advertised as a guess. PyroWave is
   decoded by a hand-written Metal kernel on A13-class and newer, and only when you pick it.
7. Runtime-probed against the actual display: EDR headroom on iPhone/iPad, HDR eligibility on Mac
   and Apple TV, HDR capabilities on Android. On an SDR panel the client advertises no HDR at all
   so the host sends a correct 8-bit picture instead of PQ your screen would mangle.
8. Opt-in, and the only client that **probes before asking**: it advertises 4:4:4 only where
   VideoToolbox really hardware-decodes it (both 8-bit and 10-bit when HDR is also on), because
   VideoToolbox's software 4:4:4 decode is far too slow for a real-time stream — validated on M3.
   The desktop clients need no such probe (note 4). For HEVC it only ever resolves against an
   NVIDIA host.
9. Chosen by name from a ranked device list that prefers hardware, real SoC vendors and low-latency
   decoders, and blocks the known-bad software ones. There is no software rung. PyroWave is the
   one exception on this row: it is not a MediaCodec at all but GPU compute presenting through its
   own swapchain, offered only on a 64-bit device whose GPU passes the probe (Vulkan 1.3 plus the
   codec's compute feature set) and only when you pick it.
10. H.264 and HEVC are assumed universal on Android hardware; AV1 is probed.
11. Whether HDR is offered is decided by the host and layered into what Moonlight is told, so an
    HDR toggle only appears in Moonlight when the host could really do it. See
    [Moonlight](/docs/moonlight).
12. `pf-webos` is a community client in a separate repository. Nothing in this codebase can
    establish its capabilities, so it is honestly blank rather than optimistically filled in.

**Multi-slice frames** are advertised by the Linux and Windows desktop clients only. Apple and
Android deliberately do not, because some TV-box decoders wedge the whole device on them, so those
sessions always receive single-slice frames. This is decoder truth, not a tuning knob.

## Clients and features

There are fewer client codebases than there are client apps. **Linux, Windows, the Decky plugin and
the `punktfunk` CLI all stream through the same session binary** — the front-ends differ, the stream
does not (with three exceptions: the clipboard, the decode order, and the audio endpoint pickers —
choosing a specific speaker or microphone is Linux-only; on Windows the session follows the system
default). Apple is one codebase across
macOS, iOS/iPadOS and tvOS. Android is one app, with Android TV being the same app in leanback mode.

### Before you connect

| Client | Presets | `punktfunk://` links | Game library | Speed test | Wake-on-LAN | Updates itself |
|---|---|---|---|---|---|---|
| Linux desktop | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ⚠️ ² |
| Windows desktop | ✅ | ✅ | ✅ ¹ | ✅ | ✅ | ❌ ³ |
| macOS | ✅ | ✅ | ✅ ⁴ | ✅ | ✅ | ❌ ³ |
| iPhone · iPad | ✅ | ✅ | ✅ ⁴ | ✅ | ✅ | ❌ ³ |
| Apple TV | ✅ ⁵ | ✅ | ✅ ⁴ | ✅ | ✅ | ❌ ³ |
| Android · Android TV | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ ³ |
| Decky (Steam Deck) | ⚠️ ⁶ | ❌ | ⚠️ ⁷ | ❌ | ✅ | ✅ ⁸ |
| `punktfunk` CLI | ✅ | ✅ ⁹ | ✅ | ✅ | ✅ | ❌ |
| Moonlight | ❌ ¹⁰ | ❌ ¹⁰ | ✅ ¹¹ | ❓ ¹² | ❓ ¹² | ❓ ¹² |

1. **Any paired host.** "Browse library…" sits on a saved host's card with nothing to turn on first.
   It stays hidden on a host that is only trusted, not yet paired: the fetch authenticates with the
   pairing identity and would come back refused. See [Game library](/docs/game-library).
2. One of two clients with a self-updater (the Decky plugin is the other), and what `--apply-update`
   can do depends on how you
   installed it: Flatpak updates itself, system packages go through a packaged helper, image-based
   systems only stage the update, and anything else just prints the command to run. See
   [Updating](/docs/updating).
3. Updates arrive through the store or installer you got the app from.
4. On by default on Apple.
5. Made and edited in the Apple TV's **Settings**, from the **Editing** row. The catalog is
   per-device and does not sync, so a preset made on a phone is not on the TV. Of the settings a
   preset can carry, tvOS drops the ones the platform has no input for: inverted scroll, modifier
   layout, variable refresh rate, mouse mode and touch mode.
6. The panel *shows* the presets a host has pinned, as nested one-tap cards, and streams with
   them; it has no preset surface of its own. Pins are made in a client's own UI — including the
   console home **Open Punktfunk** opens — and are shared, so every client shows the same cards.
   Creating and editing a preset stays a desktop-app job.
7. Not in the panel: **Open Punktfunk** opens the client's console home, and a paired host's
   library is one button from there.
8. Both the plugin itself and, where the install kind allows it, the client it launches.
9. The CLI parses and follows links; it does not register the URL scheme — the graphical apps do.
10. [Presets and links](/docs/presets-and-links) are Punktfunk-app concepts and do not exist on
    the GameStream plane.
11. Moonlight sees the host's titles as ordinary GameStream apps.
12. These live in whichever Moonlight app you use, not in this project. The host does publish the
    information a Wake-on-LAN-capable client needs. See [Wake-on-LAN](/docs/wake-on-lan).

### Input while you stream

| Client | Gamepads | Rumble | Gyro · touchpad · triggers | Pen | Touch modes | Mouse modes |
|---|---|---|---|---|---|---|
| Linux · Windows desktop | ✅ ¹ | ✅ | ✅ ² | ❌ ³ | ⚠️ ⁴ | ⚠️ ⁵ |
| macOS | ✅ ¹ | ✅ | ✅ ² | ❌ | ❌ | ⚠️ ⁵ |
| iPhone · iPad | ✅ ¹ | ✅ | ✅ ² | ✅ ⁶ | ✅ | ⚠️ ⁷ |
| Apple TV | ✅ ¹ | ✅ | ✅ ² | ❌ | ❌ | ⚠️ ⁸ |
| Android · Android TV | ✅ ¹ | ⚠️ ⁹ | ⚠️ ¹⁰ | ✅ ¹¹ | ✅ ¹² | ✅ |
| Moonlight | ✅ | ✅ | ❌ ¹³ | ⚠️ ¹⁴ | ⚠️ ¹⁵ | — |

1. Multiple controllers, each on its own stable slot, arriving and leaving independently. The pad
   **type** the host emulates is picked per pad; the pickers are not identical across apps — Linux,
   Android and the console home offer six presets including Steam Deck, Windows and Apple offer
   five.
2. DualSense and DualShock 4 touchpad and motion are forwarded, and the host's adaptive-trigger and
   lightbar effects are replayed on a real DualSense. On the desktop clients any controller SDL
   exposes a gyro on forwards motion — a Switch Pro or the Steam Deck's own pad included — and the
   host injects it into the matching virtual pad; the Deck's trackpads ride the same touchpad
   surface. On the Apple clients rich capture is gated to the DualSense/DualShock 4 family, so
   other pads there really do get rumble only.

   **But motion only lands if the virtual pad the host builds has somewhere to put it.** The Xbox
   360 and Xbox One backends have no gyro in their HID contract, so a session that resolves to one
   parses every motion sample and discards it. That is what *Automatic* does for any controller it
   doesn't recognise as Sony or Valve — an 8BitDo with a perfectly good gyro included — and it is
   also where a Switch Pro lands on a Windows host, which has no `hid-nintendo` backend to fold it
   into. Set **Controller type** to a DualSense-class preset to get motion in those cases; the
   clients now say so on-screen when they detect it, rather than leaving you to guess why tilting
   does nothing. See [Gamepad type](/docs/client-settings#gamepad-type).
3. No desktop client sends pen input, even though the desktop hosts can inject it.
4. All three touch modes exist in the shared code and the picker is there, but nobody has confirmed
   them on a Windows 2-in-1. Only meaningful on a touchscreen anyway.
5. The desktop (absolute) mouse model is unavailable against a **gamescope** host, which grants
   relative input only. The switch chord silently does nothing there; the session stays captured.
6. Apple Pencil, with pressure, tilt, azimuth and hover. Needs a host that can inject pen — see
   [Input](/docs/input). Barrel roll is not something Apple Pencil reports.
7. Pointer capture is on by default on iPad, but the system only grants it to a full-screen
   frontmost window; in Stage Manager, Slide Over or on iPhone it degrades to absolute pointing.
   It is not a setting you can force.
8. The Siri Remote acts as a relative trackpad. There is no absolute pointer mode on tvOS.
9. Uses the controller's own vibration motor where the phone kernel exposes it — many do not. There
   is an opt-in setting that **also** plays player 1's rumble on the phone's own motor, for clip-on
   pads with no motors of their own; the iPhone has the same toggle (Taptic Engine).
10. Only when the pad is claimed over **USB** (on by default). Over Bluetooth, Android's normal
    controller path carries no motion or touchpad, and adaptive-trigger effects are parsed and
    dropped because Android has no public API for them.
11. Full stylus support including eraser, both barrel buttons and hover. Android exposes no barrel
    roll.
12. Not applicable on Android TV — no touchscreen.
13. Punktfunk's GameStream server decodes only the classic multi-controller event and Sunshine's
    `CONTROLLER_ARRIVAL`; the extension packets that would carry pad motion, touchpad contacts and
    trigger effects are not implemented, so none of it reaches the host on this plane.
14. The host understands Moonlight's pen and touch extensions and feeds them through the same
    injection path, so Moonlight on an iPad with a Pencil really does draw. Whether your Moonlight
    app sends them is up to that app.
15. Forwarded, but pressure and contact area are dropped on the way in.

Punktfunk keyboard chords, and what each host backend can and cannot inject (including committed
text from an IME), are covered in [Input](/docs/input).

### Picture and sound while you stream

| Client | HDR | 4:4:4 | Surround 5.1 / 7.1 | Microphone | Clipboard | Stats overlay |
|---|---|---|---|---|---|---|
| Linux desktop | ✅ ¹ | ⚠️ ¹³ | ✅ ² | ✅ | ❌ ³ | ✅ |
| Windows desktop | ✅ ¹ | ⚠️ ¹³ | ✅ ² | ✅ | ⚠️ ⁴ | ✅ |
| macOS | ⚠️ ⁵ | ⚠️ ⁶ | ✅ ² | ✅ | ✅ ⁷ | ✅ |
| iPhone · iPad | ⚠️ ⁵ | ⚠️ ⁶ | ✅ ² | ✅ | ✅ ⁷ | ✅ |
| Apple TV | ⚠️ ⁵ | ⚠️ ⁶ | ✅ ² | ❌ ⁹ | ❌ ⁸ | ✅ |
| Android · Android TV | ⚠️ ⁵ | ❌ | ✅ ² | ✅ | ⚠️ ¹⁰ | ✅ |
| Moonlight | ⚠️ ¹¹ | ❌ | ✅ ² | ❌ | ❌ | ❓ ¹² |

1. Advertised whenever the HDR setting is on, which it is by default. No display probe — these
   clients ask, and the host decides.
2. Requested by the client and **resolved by the host**: it clamps to what it can actually capture
   (2, 6 or 8 channels) and tells you the real answer before the first frame.
3. There is a "Share clipboard" switch, but the Linux side of the bridge is a stub — nothing is
   ever offered or applied. Treat the clipboard as unavailable on Linux clients until this lands.
4. Text and PNG images. Applications that only publish the older bitmap format are not covered, and
   an inbound image over 4 MiB is skipped.
5. Runtime-probed against your actual display — see note 7 under [Client decode](#client-decode).
6. Opt-in, hardware-probed, and for **HEVC** it only ever resolves against an NVIDIA host — VAAPI,
   AMF and QSV all decline 4:4:4 outright. The **PyroWave** codec is the exception: it does its own
   full-chroma colour conversion, so 4:4:4 needs no encoder probe and resolves on any vendor. See
   [Client settings](/docs/client-settings).
7. The richest implementation: text, RTF, HTML and images, fetched lazily in both directions.
   Concealed and transient pasteboard items are skipped. On iOS one thing is not lazy: because
   backgrounding the app ends the session, an unpasted host offer is pulled across (up to 8 MiB)
   as the session ends, so it survives to be pasted in another app.
8. tvOS has no pasteboard, so there is nothing to share.
9. tvOS gives apps no microphone input at all.
10. Text only, by design — and unlike every other client, Android's per-host **Shared clipboard**
    switch starts **on** (the desktop clients and the Apple app default it off). Nothing crosses
    until the host also enables its clipboard, but the client-side consent is pre-granted.
11. Decided entirely by the host and layered into what Moonlight is offered.
12. Moonlight has its own overlay; [stats](/docs/stats) here describes Punktfunk's.
13. Opt-in — the per-preset **Full chroma (4:4:4)** switch, off by default — and advertised with
    no client-side probe. Both halves earn the ⚠️. On the **client** it is a hardware path with no
    software floor under it (note 4 under [Client decode](#client-decode)). On the **host** it
    needs HEVC on an **NVIDIA** host, or the PyroWave codec, which carries it on any vendor. On
    Windows it composes with HDR; on a Linux host 4:4:4 is 8-bit, so asking for both gives you full
    chroma in SDR. The stats overlay tells you which you got.

**File transfer through the clipboard does not exist yet** on any client. The wire format and the
host-side policy for it are in place, but no client offers files, so a copied file never crosses.
See [Clipboard](/docs/clipboard).

### Things both ends have to agree on

These are negotiated, and either side can be the reason it did not happen:

- **Shared clipboard** — off on the host until an operator enables it, and unavailable on a Linux
  host without the clipboard protocol (a gamescope session, for instance). The per-host "Share
  clipboard" switch is edited before you connect, so it is never greyed out — on Linux, Windows,
  Android and the Apple add-host sheet it stays settable against a host that will refuse, and just
  does nothing. Only the Apple app reflects the refusal live: the Stream ▸ Share Clipboard item is
  disabled when the connected host has not advertised the capability.
- **Pen input** — the host advertises it only if it can really inject: a usable `/dev/uinput` on
  Linux, or synthetic pointer devices on Windows 10 1809+. Without it, clients fold pen into touch.
- **Committed text (IME)** — only the Windows host and the sway/wlroots Linux backend can type
  arbitrary text. On KDE, GNOME and gamescope, sessions fall back to key synthesis, which cannot
  express every character.
- **Client-drawn cursor** — needs the client to be in desktop mouse mode *and* a host capture path
  that carries the cursor separately. gamescope never can. When both agree, the host stops drawing
  the pointer into the video.
- **10-bit, HDR and 4:4:4** — see [HDR](/docs/hdr) for the full four-link chain.

## How finished each part is

The tables above say what a part *can do*. This one says how settled it is — maturity, not
capability.

| Part | Where it stands |
|---|---|
| **Protocol core** — `punktfunk-core`, the C ABI, FEC and crypto | Stable. Both the wire format and the embeddable C surface are versioned contracts (see below) and are changed reluctantly. |
| **Linux host** | The oldest and most exercised surface. What differs is not the host but the desktop under it — each compositor gets its own capture, virtual-display and input backend, and they are not equally capable. |
| **Windows host** | Newest large surface, shipping as an installer with its own virtual-display driver — both signed with Punktfunk's own certificates rather than a publicly trusted one, so Windows warns on install (see [Windows host](/docs/windows-host#about-the-signatures)). NVENC is well trodden; the AMD (AMF) path was validated on real hardware in mid-2026 (Ryzen 7000 iGPU, 1080p120 HDR P010) and the Intel (QSV) path on Arc, but both see far less field time than NVENC — several of QSV's newer arms are still marked unvalidated in the code. One structural constraint that catches people: it must run in the interactive console session, not session 0. |
| **GameStream / Moonlight plane** | Works, and is **off** until you ask. Every install route is native-only: Linux packages run `serve` and take `PUNKTFUNK_GAMESTREAM=1` in `host.env`; SteamOS takes `--gamestream`; NixOS takes `services.punktfunk.host.gamestream = true`; Windows leaves the installer checkbox unticked. A bare `punktfunk-host serve` is off. It pairs over plain HTTP with weaker legacy encryption — trusted LAN only (see [Security](/docs/security#gamestream--moonlight-compatibility-is-the-weak-crypto-path)). It is a compatibility surface, so Punktfunk-only features (presets, links, clipboard, microphone) are not on it. |
| **Linux and Windows desktop clients** | Packaged and current. They are one codebase: the same session binary streams for both, and for the Decky plugin and the `punktfunk` CLI. |
| **Apple client** (macOS · iOS · iPadOS · tvOS) | One universal build, distributed as a **TestFlight beta**; the Mac also has a notarized DMG. Feature-complete apart from the platform gaps named above (no microphone or clipboard on tvOS). |
| **Android client** (phone · TV) | Published on **Google Play** as a public listing for releases, with an invite-only Internal testing track for canary, plus a sideloadable APK. The same app in leanback mode is the TV client. |
| **Decky plugin** (Steam Deck) | Ships through install-from-URL rather than the Decky store, and keeps itself and the client it launches up to date. It is a launcher, not a second client: it starts the Linux client rather than streaming itself, and holds no settings, no library and no host editor of its own — its **Open Punktfunk** button hands all of that to the client's console home. |
| **Web console** | The full management surface — dashboard and sessions, pairing, library, displays, plugins and the plugin store, logs, stats, settings, and host updates. It cannot yet run a speed test or set a bitrate; the client apps can. |
| **Plugins** | Three first-party ones (ROM Manager, Playnite, VirtualHere) plus the SDK, installed from the console. See [Plugins](/docs/plugins). |
| **`pf-webos`** (LG TV) | A community client in a separate repository. Nothing here can establish its state; ask that project. |

## Mixing versions

Update a host and its clients whenever it suits you — they interoperate across versions, and
anything already paired stays paired. Four separate contracts make that true, each versioned on its
own so that adding a client-side feature never locks a client out of a deployed host:

| Contract | Current | What it governs |
|---|---|---|
| `punktfunk/1` wire version | **2** | The `Hello`/`Welcome` handshake and the session planes. Hosts equality-check it, so this is the one that must match. |
| C ABI version | **17** | The embeddable C surface a client links against. It grows far more often than the wire does. |
| Virtual-display driver protocol (Windows) | **6** (accepts **3** and up) | Between the Windows host and its display driver, so an older driver keeps working after a host update. |
| Windows virtual-gamepad channel | **3** | Between the host and its pad driver. |

Everything newer than a peer is **negotiated**: a capability the other end never heard of is simply
not offered, and the session proceeds without it rather than failing. That is why a feature can be
present in both apps and still not appear — see [Things both ends have to agree
on](#things-both-ends-have-to-agree-on).

## Where it has been run

Day-to-day development and validation happen on Linux hosts across the KWin, Mutter, gamescope and
wlroots backends, on NVIDIA and AMD GPUs, and on Windows hosts on NVIDIA, AMD and Intel — with the native
macOS, Linux, Windows and Android clients, plus stock Moonlight over the GameStream path. HDR on
gamescope is verified end to end on Bazzite and on SteamOS.

Cross-machine latency figures are trustworthy because a wall-clock handshake removes the clock
offset between the two machines before anything is measured, so a capture-to-receipt number is valid
across the LAN rather than only on one box. (The remaining term — receipt to actually on screen — is
what the [roadmap](/docs/roadmap)'s glass-to-glass work is about.) See
[Understanding the stats overlay](/docs/stats).

## What is not verified

The honest counterpart to the list above. Some of these are ❓ cells; more of them are ⚠️ cells
whose caveat *is* "nobody has run this on real hardware" — a wrong ✅ is worse than either.

- **Client-drawn cursor on sway/wlroots and Hyprland.** Wired, never confirmed on glass. A single
  cursor-forwarding session on each — checking the pointer arrives and is not drawn twice — settles
  it.
- **gamescope headless capture on the proprietary NVIDIA driver.** Plausible by architecture, not a
  well-trodden path, and there is no probe that would catch it failing. One spawn-and-capture run on
  an NVIDIA box settles it.
- **Touch input from a Windows client.** Same shared code as Linux, no on-glass run.
- **Re-applying your resolution after an Omarchy theme switch.** The rest of the Omarchy
  integration was run end to end on a real 4.x box for 0.33.0 — install, `punktfunk-omarchy setup`,
  pair, stream — and five wrong assumptions were fixed there. This one piece was not: a theme
  switch reloads Hyprland's config and discards the host's display settings, and the re-apply is
  unit-tested with the trigger confirmed firing on that machine, but testing the re-application
  itself meant interrupting the only live stream we had. If a theme switch still costs you your
  resolution, say so.
- **The `pf-webos` LG TV client.** A community project in another repository. Its codecs, HDR
  behaviour and feature set cannot be established from here.
- **Everything client-side about Moonlight.** Wake-on-LAN, overlays, updates and which extensions
  it sends differ per Moonlight flavour. The honest answer is "ask your Moonlight app".
- **HDR when mirroring a real monitor on KDE, sway or Hyprland.** By construction it is SDR — only
  gamescope's virtual output and the GNOME portal mirror carry HDR — but nothing states that as a
  deliberate decision rather than a gap.

Where this page and another page disagree, this one is the one that was checked against the code.
For what is planned rather than shipped, see the [Roadmap](/docs/roadmap).
