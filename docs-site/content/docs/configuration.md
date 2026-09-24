---
title: Configuration
description: Host settings in the web console, the host.env file that pins them, and the PUNKTFUNK_* environment variables that stay env-only — what each one does.
---

Most host settings are on the web console's **Host → Settings** page: the table under
[Settings in the web console](#settings-in-the-web-console) lists them, with the `host.env` name each
one also answers to. Change them there; there is no file to edit.

**`host.env`** is for the rest, and for pinning. It lives at **`~/.config/punktfunk/host.env`** on
Linux and **`%ProgramData%\punktfunk\host.env`** on Windows: a `KEY=value` file where `#` starts a
comment and keys are **case-sensitive**. A console setting set there is locked in the console until
the line is gone. The sections after the table are the env-only variables: backend pins, network and
port tuning, paths, and diagnostics. A few settings are documented on the page that owns their
feature instead — they're listed under [Settings documented
elsewhere](#settings-documented-elsewhere) at the end.

The file is read when the host starts, so **an edit does nothing until you restart the host.** On
Linux, where the file is loaded by the `punktfunk-host` user service:

```bash
systemctl --user restart punktfunk-host
```

On Windows, where it is loaded by the `PunktfunkHost` service — from an Administrator prompt:

```powershell
punktfunk-host service restart
```

> **You rarely need most of these.** The host **auto-detects** the compositor, input backend, and
> encoder from your live session — a box that flips between Steam Gaming Mode and a KDE/GNOME desktop
> is followed automatically. The `PUNKTFUNK_*` knobs below are mostly **optional overrides** for
> forcing a specific backend, tuning performance, or debugging. The starter `host.env` for your
> platform sets only the few you actually need.

Two things people come here for are **not** host settings: **resolution** and **bitrate** are chosen
by the client — see [Bitrate](#bitrate) near the end. The last sections are background: the
variables the **clients** read, several devices at once, and codecs.

## Settings in the web console

Each of these is on **Host → Settings**. Advanced ones show once **Show advanced** is ticked, and a
search for the `host.env` name finds the setting. A value in `host.env`, or a flag on the host's
command line, wins over the console, and the console shows that setting as locked. Remove the line
and restart the host to hand the setting back to the console. The rows in the sections below say
more about some of them.

| Setting | `host.env` | Values | Default | Applies |
|---|---|---|---|---|
| GameStream | `PUNKTFUNK_GAMESTREAM` | `on` · `off` | `off` | after a restart |
| Browser streaming | `PUNKTFUNK_WEBTRANSPORT` | `on` · `off` | `off` | after a restart |
| Shared clipboard (Linux, Windows) | `PUNKTFUNK_CLIPBOARD` | `off` · `text` · `files` | `off` | next session |
| Host name | `PUNKTFUNK_HOST_NAME` | text, up to 63 characters | — | after a restart |
| GameStream encryption | `PUNKTFUNK_GAMESTREAM_ENCRYPT` | `supported` · `video` · `off` · `required` | `supported` | after a restart |
| Moonlight adaptive bitrate | `PUNKTFUNK_GAMESTREAM_ADAPT` | `on` · `off` | `on` | after a restart |
| ChaCha20 cipher | `PUNKTFUNK_CHACHA20` | `on` · `off` | `on` | next session |
| Browser origins | `PUNKTFUNK_WEBTRANSPORT_ORIGINS` | comma list | — | after a restart |
| Encoder | `PUNKTFUNK_ENCODER` | `auto` · `nvenc` · `vaapi` · `vulkan` · `pyrowave` · `software` | `auto` | next session |
| 10-bit and HDR | `PUNKTFUNK_10BIT` | `on` · `off` | `on` | next session |
| Full color 4:4:4 | `PUNKTFUNK_444` | `on` · `off` | `on` | next session |
| Game frame limit (Linux) | `PUNKTFUNK_MAX_FPS` | 0–240 fps | `0` | next session |
| Cursor capture (Linux) | `PUNKTFUNK_PORTAL_CURSOR_MODE` | `auto` · `embedded` · `metadata` · `hidden` | `auto` | next session |
| Vulkan encoding (Linux) | `PUNKTFUNK_VULKAN_ENCODE` | `on` · `off` | `on` | next session |
| Direct capture (Linux) | `PUNKTFUNK_DIRECT_CAPTURE` | `on` · `off` | `on` | next session |
| On-demand capture (Linux) | `PUNKTFUNK_LAZY_CAPTURE` | `on` · `off` | `on` | next session |
| KWin capture pacing (Linux) | `PUNKTFUNK_KWIN_PACED` | `on` · `off` | `off` | next session |
| PyroWave bitrate cap | `PUNKTFUNK_PYROWAVE_MAX_MBPS` | 0–10000 Mbps | `0` | next session |
| Where audio plays (Linux, Windows) | `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `client_only` · `host_and_client` · `follow_default` | `client_only` | next session |
| Audio quality | `PUNKTFUNK_AUDIO_QUALITY` | `low` · `standard` · `high` | `high` | next session |
| Lossless audio | `PUNKTFUNK_AUDIO_HIRES` | `on` · `off` | `on` | next session |
| Voice chat (Linux, Windows) | `PUNKTFUNK_AUDIO_VOICE_CHAT` | `stream` · `host` | `stream` | next session |
| Voice chat apps (Linux, Windows) | `PUNKTFUNK_AUDIO_VOICE_APPS` | comma list | — | next session |
| Controller speaker (Linux, Windows) | `PUNKTFUNK_PAD_AUDIO` | `on` · `off` | `on` | next session |
| Audio redundancy | `PUNKTFUNK_AUDIO_REDUNDANCY` | `auto` · `on` · `off` | `auto` | next session |
| Default gamepad (Linux, Windows) | `PUNKTFUNK_GAMEPAD` | `auto` · `xbox360` · `xboxone` · `dualsense` · `dualsenseedge` · `dualshock4` · `steamdeck` · `steamcontroller` · `steamcontroller2` · `switchpro` | `auto` | next session |
| Pen input (Linux, Windows) | `PUNKTFUNK_PEN` | `on` · `off` | `on` | next session |
| Steam USB gadget (Linux) | `PUNKTFUNK_STEAM_GADGET` | `auto` · `on` · `off` | `auto` | next session |
| DualSense over USB/IP (Linux) | `PUNKTFUNK_DUALSENSE_USBIP` | `on` · `off` | `off` | next session |
| Attach mode (Linux) | `PUNKTFUNK_GAMESCOPE_ATTACH` | `on` · `off` | `off` | next session |
| Game Mode HDR (Linux) | `PUNKTFUNK_GAMESCOPE_HDR` | `on` · `off` | `on` | next session |
| Force managed mode (Linux) | `PUNKTFUNK_GAMESCOPE_MANAGED` | `on` · `off` | `off` | next session |
| Adaptive sync (Linux) | `PUNKTFUNK_GAMESCOPE_VRR` | `on` · `off` | `on` | next session |
| SDR brightness (Linux) | `PUNKTFUNK_GAMESCOPE_SDR_NITS` | 1–10000 nits | `203` | next session |
| Extra refresh rates (Linux) | `PUNKTFUNK_GAMESCOPE_REFRESH_RATES` | comma list | — | next session |
| Steam integration (Linux) | `PUNKTFUNK_GAMESCOPE_STEAM` | `on` · `off` | `off` | next session |
| Startup splash (Linux) | `PUNKTFUNK_GAMESCOPE_SPLASH` | `on` · `off` | `on` | next session |
| Per-session isolation (Linux) | `PUNKTFUNK_GAMESCOPE_ISOLATE` | `on` · `off` | `on` | next session |
| Grab the cursor (Linux) | `PUNKTFUNK_GAMESCOPE_GRAB_CURSOR` | `on` · `off` | `off` | next session |
| Steam per seat (Linux) | `PUNKTFUNK_STEAM_SEAT_HOME` | `on` · `off` | `off` | next session |
| Pads per seat (Linux) | `PUNKTFUNK_STEAM_SEAT_SANDBOX` | `on` · `off` | `off` | next session |
| Seats kept warm (Linux) | `PUNKTFUNK_STEAM_PREWARM` | 0–8 seats | `1` | after a restart |
| Bind patched gamescope (Linux) | `PUNKTFUNK_GAMESCOPE_BIND` | `auto` · `on` · `off` | `auto` | next session |
| Follow mode switches (Linux) | `PUNKTFUNK_SESSION_WATCH` | `auto` · `on` · `off` | `auto` | next session |
| Local discovery | `PUNKTFUNK_MDNS` | `on` · `off` | `on` | after a restart |
| Disconnect timeout | `PUNKTFUNK_IDLE_TIMEOUT_MS` | 1000–120000 ms | `8000` | after a restart |
| Check for updates | `PUNKTFUNK_UPDATE_CHECK` | `on` · `off` | `on` | at once |
| Console updates | `PUNKTFUNK_UPDATE_APPLY` | `on` · `off` | `on` | at once |

## Session anchors

**Leave these unset on a normal setup.** Running as a `systemctl --user` service the host inherits
the correct `XDG_RUNTIME_DIR` from systemd, derives the session bus from it, and **rewrites
`WAYLAND_DISPLAY` / `XDG_CURRENT_DESKTOP` / `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` on every
connect** to follow the active session (Gaming ↔ Desktop) — a value written here can only be
redundant or stale.

| Setting | When to set it |
|---|---|
| `XDG_RUNTIME_DIR` | Only when the host runs **outside** a user service (ssh, cron): `/run/user/<your uid>` — check `id -u`. A copy-pasted `1000` on a box where that isn't your uid points the host at another user's (nonexistent) PipeWire/D-Bus, and **everything** fails (audio `Creation failed`, no capture, clients report the host unreachable). |
| `DBUS_SESSION_BUS_ADDRESS` | Same cases only: `unix:path=/run/user/<your uid>/bus`. Otherwise derived automatically. |
| `WAYLAND_DISPLAY` | Only the dedicated [headless-KDE appliance](/docs/kde#headless-session) (`wayland-kde`, set by its shipped `host.env.kde`). |
| `XDG_CURRENT_DESKTOP` | Same — appliance-only. |

## Core

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_COMPOSITOR` | `kwin` · `mutter` · `gamescope` · `wlroots` · `hyprland` (aliases: `kde`/`plasma`, `gnome`, `sway`/`wlr`) | Which backend creates the virtual display. `wlroots` is sway/River; `hyprland` is its own backend. **Leave unset.** Setting it **pins** the backend and turns session-following **off** — per connect *and* mid-stream, so a Desktop ↔ Gaming switch kills the stream instead of being followed. For CI/tests and dedicated single-session appliances only. |
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` (default) · `portal` | **GameStream/Moonlight sessions only** — it has no effect on the native `punktfunk/1` plane. `virtual` creates a per-client display at the client's exact mode (the normal choice); `portal` captures an existing monitor instead, and is what the GNOME 50+ HDR monitor mirror needs — see [HDR](/docs/hdr#linux--gnome). To stream a physical monitor to a Punktfunk app, use `PUNKTFUNK_CAPTURE_MONITOR` below, or the console's **Streamed screen**. |
| `PUNKTFUNK_CAPTURE_MONITOR` | a connector name (`HDMI-A-1`, `DP-2`, …) | Stream a **physical** monitor this host already has instead of creating a virtual display — see [Streamed screen](/docs/virtual-displays#stream-a-real-monitor-instead). List the names with `punktfunk-host list-monitors`. Setting it here **outranks the web console's** choice, so an appliance stays aimed where its operator pointed it; leave it unset to steer from the console. A name that matches no monitor fails the session loudly rather than streaming a different screen. Linux only. |
| `PUNKTFUNK_ZEROCOPY` | `1` · `0` *(default on)* | GPU zero-copy capture→encode (dmabuf → CUDA → NVENC, or D3D11 on Windows). **On by default** — no need to set it; it falls back to a CPU path automatically. Set `0` to force the CPU path. One exception: Windows **Intel/QSV** keeps the CPU path by default until zero-copy is validated on Intel hardware — set `1` to try it there. |
| `PUNKTFUNK_INPUT_BACKEND` | `libei` · `kwin` · `gamescope` · `wlr` | How input is injected. `kwin` (KWin fake-input) for KDE — direct injection with no portal approval dialog, so it also works on a headless KDE box; `libei` (the RemoteDesktop portal) for GNOME; `gamescope` for Bazzite/gamescope; `wlr` for Sway/wlroots **and Hyprland**. Auto-detected with the compositor; a value that isn't one of the four is ignored and detection runs anyway. |
| `PUNKTFUNK_PEN` | `1` · `0` *(default on)* | Full-fidelity stylus input — pressure, tilt, hover, eraser, barrel buttons — for the clients that send it. **On by default**; `0` stops the host advertising pen at all and every client folds the stylus back into ordinary touch. The host also needs `/dev/uinput` on Linux (the same `input` group the virtual gamepads use) or Windows 10 1809+. See [Pen and stylus](/docs/input#pen-and-stylus). |
| `PUNKTFUNK_ENCODER` | `auto` · `nvenc` · `vaapi` · `vulkan` (Linux) · `amf` · `qsv` · `mf` (Windows) · `software` | Encoder backend. `auto` (default) detects the GPU vendor: NVIDIA→NVENC; on **Linux** AMD/Intel→**Vulkan Video** for HEVC and AV1 (falling back to VAAPI when the device or the codec can't take it — H.264 is always VAAPI), on **Windows** AMD→AMF and Intel→QSV. `mf` (alias `mediafoundation`) is Windows Media Foundation — any vendor's hardware encoder, tried automatically when the native backend cannot open, and 8-bit 4:2:0 only. `software` (aliases `sw`/`openh264`) is the GPU-less H.264 path on both platforms — on Windows `auto` falls back to it when no GPU is found; on Linux it is **explicit-only** (`auto` never picks it). On a multi-GPU Windows box a forced hardware backend whose vendor contradicts the selected GPU (web-console preference) is **overridden** — the adapter wins and the host logs a warning; remove the stale pin. |
| `PUNKTFUNK_VULKAN_ENCODE` | `1` · `0` *(default on)* | **(Linux, AMD/Intel)** Use the Vulkan Video encoder for HEVC/AV1 sessions. **On by default** — it recovers from packet loss without a full keyframe, which the VAAPI path can't express. `0` pins the VAAPI path; so does a device that can't encode the profile (the host falls back on its own). See [Requirements](/docs/requirements). |
| `PUNKTFUNK_VAAPI_LOW_POWER` | `1` · `0` | **(Linux, Intel)** Pin the VAAPI entrypoint. Modern Intel (Gen12/Tiger Lake and newer, incl. Arc) only offers the low-power (VDEnc) entrypoint and the host detects that by itself; set this only to force one way or the other. See [Requirements](/docs/requirements). |
| `PUNKTFUNK_RENDER_NODE` | path | Linux DRM render node for zero-copy (default `/dev/dri/renderD128`). Set on multi-GPU boxes to pick the right GPU. Superseded by a manual GPU preference in the console — see below. |

> **Picking a GPU** — on a multi-GPU box, choose the GPU in the **web console** (Host → *GPUs*),
> which writes `gpu-settings.json`. A **manual** preference there outranks both
> `PUNKTFUNK_RENDER_NODE` (Linux) and `PUNKTFUNK_RENDER_ADAPTER` (Windows); while the console is
> left on **Automatic** — or the preferred GPU isn't present — those two still decide. They stay
> useful on a headless/appliance box nobody opens the console on.

Resolution and refresh are **not** set here — **the client chooses them.** When a device connects,
the host creates a virtual display at that device's resolution and refresh rate. A 1080p60 laptop and
a 1440p120 desktop each get their own. (With Moonlight, set the mode in Moonlight; the native clients
let you pick a mode or default to the device's display.)

## gamescope / session following (Linux, Bazzite/SteamOS)

Two mutually-exclusive models for a Steam/gamescope box. See [Steam / gamescope](/docs/gamescope) for
the full picture (and [Bazzite](/docs/bazzite) for that distro's specifics).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMESCOPE_ATTACH` | `1` · `0` *(unset = auto)* | **Attach** model: the box owns its gamescope session on its own display (you switch Gaming ↔ Desktop with the Steam UI); the host just captures whatever's live and never tears it down. On a **headless** box the box-owned autologin session is restarted at the client's resolution on a mismatch; a box driving a physical display, and any foreign/bare gamescope, streams at its own mode — i.e. the client is served a **mirror**, not a display of its own. Setting this also outranks a dedicated game session. No template ships it set; `=0` is the same as leaving it out. |
| `PUNKTFUNK_GAMESCOPE_MANAGED` | `1` | **Managed** model (the default where session infra is detected): the host takes the box's gamescope over and relaunches it **headless** at the *client's* exact resolution — Game Mode on the virtual screen — restoring the box on idle. |
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | The host owns a `gamescope-session-plus` (Steam) session at the client's mode (headless appliance; no physical session running). |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Discover + capture a **running** gamescope's PipeWire node at a fixed mode. Do **not** combine with `SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | For an ad-hoc bare-gamescope session, the nested command to run (e.g. `vkcube`). |
| `PUNKTFUNK_GAMESCOPE_HDR` | `1` · `0` *(default on)* | Allow HDR (10-bit BT.2020 PQ) sessions on the gamescope backend. Needs the `punktfunk-gamescope` build — see [HDR on gamescope](/docs/gamescope#hdr-on-gamescope); without the build, sessions stream SDR. Set `0` to force SDR. |
| `PUNKTFUNK_GAMESCOPE_SDR_NITS` | e.g. `400` | On an HDR gamescope session, the luminance SDR content (desktop, Steam overlay, SDR games) starts at inside the PQ container. Unset = 203, BT.2408 reference white, which is what our clients decode against. Steam's SDR brightness setting replaces it during the session. Needs `punktfunk-gamescope` `+pfhdr16`. |
| `PUNKTFUNK_GAMESCOPE_BIN` | path | Force a specific gamescope binary for the sessions the host spawns. Unset = prefer `punktfunk-gamescope` on `PATH`, then `gamescope`. |
| `PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR` | path | Directory holding our Vulkan WSI layer's manifest — the layer that lets a game nested under gamescope get an HDR10 swapchain. Unset = `/usr/lib/punktfunk/vulkan/implicit_layer.d`, where every distro package installs it. The NixOS module sets this for you, since the layer lives inside the gamescope derivation there. If no manifest is found the host leaves the system's own layer alone and games stay SDR. |
| `PUNKTFUNK_SESSION_WATCH` | `1` · `0` | Follow a Gaming ↔ Desktop switch **mid-stream** (rebuild the backend in place, no reconnect). **On by default** on Bazzite/SteamOS; set `0` to disable. |
| `PUNKTFUNK_GAMESCOPE_GRAB_CURSOR` | `1` | Add `--force-grab-cursor` to a bare gamescope session the host spawns **to run an app or game** (never the empty keep-alive session), forcing relative-mouse capture so FPS mouselook works over the injected pointer. **Off by default** — relative mode breaks absolute-pointer titles and menus, so turn it on per host. |
| `PUNKTFUNK_GAMESCOPE_SPLASH` | `1` · `0` *(default on)* | Run the built-in splash client inside each bare gamescope session the host spawns. **Leave it on**: gamescope only produces capture buffers once something paints, and a Steam launch paints nothing for its whole bootstrap — without the splash a fresh session starves and times out. `0` is a debugging escape hatch. |
| `PUNKTFUNK_GAMESCOPE_ISOLATE` | `1` · `0` *(default on)* | Give each bare gamescope session the host spawns its own input, audio and mic plane — a per-session input relay, the nested apps' audio routed to that session's stream sink, and a per-session virtual mic — so concurrent sessions on one box never hear or drive each other. `0` restores the shared host-lifetime planes. Shared-desktop backends (kwin/mutter/wlroots) and the managed/attach gamescope routes always use shared planes. |
| `PUNKTFUNK_STEAM_SEAT_HOME` | `1` · `0` *(default off)* | Give each paired device's dedicated Steam launch its own `HOME` under `~/.local/share/punktfunk/seats/<device>/`, instead of the one Steam the box's desktop already runs. The desktop Steam keeps running (today the host asks it to shut down first, which costs 3–20 s per launch), and two devices can play at once. The seat's home is a reflink clone of your Steam install without its account or its library, so **each seat signs in to Steam once** — the first launch on a seat streams Big Picture's sign-in screen, which takes a QR code, and the client says so instead of waiting on the game. **One Steam account plays on one seat at a time**, so two people playing at once need an account each; Steam Families lets them play different games out of one library, and the same game on two seats needs two copies. Needs a native Steam (`~/.steam`) and a filesystem with reflink support (btrfs, XFS); without either the seat downloads Steam once by itself. Games are not downloaded twice: the seat inherits your library folders. |
| `PUNKTFUNK_STEAM_SEAT_SANDBOX` | `1` · `0` *(default off)* | Show each seat's Steam only the controllers that seat's own device is streaming. Without it every seat's Steam opens every seat's virtual controller, and Steam Input on one seat can claim another seat's pad. The seat's Steam runs under `bwrap` (install `bubblewrap`) with a controller directory of its own, and the host puts a link there for each pad it creates for that seat. Needs `PUNKTFUNK_STEAM_SEAT_HOME` on; without it, or without `bwrap`, the launch runs as it does today and the host says so in its log. **The box's own Steam is not covered** — the desktop Steam, and Game Mode on the TV, still see every seat's pad, because the host does not start them. |
| `PUNKTFUNK_STEAM_PREWARM` | `0`–`8` *(default `1`)* | How many seats the host keeps a Big Picture Steam running for before their devices connect, so a launch skips Steam's 13–30 s cold boot. Only seats with `PUNKTFUNK_STEAM_SEAT_HOME` on are ever warmed, and only devices that launched a Steam title in the last 14 days. One warm seat sitting at Steam's sign-in screen holds about 1.8 GB of RAM, 300 MB of video memory and a third of one CPU core — that is Steam's own processes, not the compositor, so a frame limit changes none of it. `0` keeps nothing warm. |
| `PUNKTFUNK_GAMESCOPE_STEAM` | `1` | Launch every bare gamescope session the host spawns in Steam integration mode (`--steam`). A Steam title turns that on by itself; this forces it for non-Steam launches too. Managed / `gamescope-session-plus` sessions own their own flags and ignore it. |


## Compositor-specific (Linux)

See your desktop page ([KDE](/docs/kde), [GNOME](/docs/gnome)) for when to set these.

> **Managing virtual displays** — keep-alive after disconnect, exclusive vs. extend, and (on
> Windows/KDE) persistent per-client scaling — now has its own settings surface in the web console
> and `display-settings.json`. See [Virtual displays](/docs/virtual-displays). The two
> `*_VIRTUAL_PRIMARY` knobs and `PUNKTFUNK_MONITOR_LINGER_MS` below still work but are superseded by
> it (a settings file wins over them).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_KWIN_VIRTUAL_PRIMARY` | `1` | Make the streamed per-session output the sole desktop so plasmashell + windows render on it (not on the headless bootstrap output). Set by the KDE appliance `host.env`. Superseded by the console's **Topology** setting. |
| `PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY` | `1` | GNOME/Mutter equivalent of the above. |
| `PUNKTFUNK_PORTAL_CURSOR_MODE` | `auto` *(default)* · `embedded` · `metadata` · `hidden` | **Hyprland / wlroots only, and a troubleshooting knob** — which ScreenCast cursor mode the host asks the portal for. Unset, the host asks for `metadata` when the client draws the pointer itself and `embedded` otherwise, then settles that against the modes your portal advertises; it never requests one your portal lacks. Set `embedded` if the pointer misbehaves on a portal that *claims* metadata support but implements it poorly — that is the one case the automatic negotiation cannot detect. A pin is still only a preference: it is checked against the advertised modes like any other. |

## Session recovery (Linux)

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_RECOVER_SESSION_CMD` | command | Operator hook fired (debounced) when a client connects while **no graphical session is live** for the host's user — the state a compositor crash leaves behind (gnome-shell SIGSEGV → GDM greeter, whose auto-login is once-per-boot). Typically `sudo -n systemctl restart gdm` with a matching NOPASSWD sudoers rule, or `systemctl restart display-manager` under a polkit rule; with auto-login enabled the restart brings the desktop back and the client's automatic retry lands in it. Unset/empty = disabled (the default). |
| `PUNKTFUNK_ON_CONNECT_CMD` | command | Fired (detached) when a client connects, on either plane — the event JSON on stdin plus `PF_EVENT_*` env vars. The zero-config little sibling of [hooks.json](/docs/automation), which adds filters, webhooks, and debounce. |
| `PUNKTFUNK_ON_DISCONNECT_CMD` | command | The `client.disconnected` counterpart of `PUNKTFUNK_ON_CONNECT_CMD` (its `PF_EVENT_REASON` is `quit`, `timeout`, or `error`). |

## Video quality

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_FEC_PCT` | `0`–`90` (percent) | **Pins** forward-error-correction redundancy and turns adaptive FEC **off**. Leave it unset on the native protocol: the host normally sizes recovery to the loss the client reports (a 1–50 % band, starting at 10 %; once a session has seen real loss the quiet-time floor is 5 % rather than 1 %, and a couple of clean minutes earn 1 % back), so pinning a number can leave a lossy link *worse* off than letting it adapt. Set it only when a fixed, known overhead matters — a measurement or a speed test; `0` disables FEC entirely. Under the wire-budget bitrate (see [Bitrate](#bitrate)) the pinned percent is still carved out of the budget, it just never moves. On the GameStream/Moonlight plane it sets that plane's STARTING percent and, like on the native protocol, pins it — leave it unset and the host adapts there too, from the loss Moonlight reports. |
| `PUNKTFUNK_10BIT` | `1` · `0` *(default on)* | Allow 10-bit (HEVC Main10 / AV1 10-bit) sessions at all; `0` forces every session to 8-bit SDR. Which hosts can actually deliver it, and the client half of the switch, are on [HDR](/docs/hdr). |
| `PUNKTFUNK_444` | `1` · `0` *(default on)* | Host **policy gate** for full chroma 4:4:4 — sharper text and thin lines, no chroma loss. **On by default**; `0` forces every session to 4:2:0. It only ever *allows*: the client's own 4:4:4 setting (default off) is the real per-session switch, and the codec, capture-path and GPU gates behind it are on [Client settings → Full chroma](/docs/client-settings#video). Which GPUs and which clients can actually do it is in the [support matrix](/docs/support-matrix#encoders); how it interacts with HDR is on [HDR](/docs/hdr). **punktfunk/1 native only** — Moonlight stays 4:2:0. |
| `PUNKTFUNK_CHACHA20` | `1` · `0` *(default on)* | ChaCha20-Poly1305 session encryption for clients without hardware AES (old ARM TVs, e.g. webOS), lifting their ~100 Mbps software-AES decrypt ceiling. **On by default** on the host; a session uses it only when the client requests it — everyone else stays on AES-GCM. Purely a performance choice (both ciphers are full-strength); set `0` to force AES-GCM for all sessions. |
| `PUNKTFUNK_PYROWAVE_MAX_MBPS` | `N` (Mbps) | Cap the [PyroWave](/docs/pyrowave) Automatic bitrate pin. Automatic sessions already fit the pin to the link once, during bring-up — this is the ceiling over that, for a pin the measurement could not have found (an older client, or a ceiling you want tighter than the link's wall). Unset = no cap. Applies to every PyroWave session — a client-requested bitrate is treated as Automatic under PyroWave, so nothing bypasses the ceiling. |
| `PUNKTFUNK_DSCP` | `1` · `0` | DSCP / `SO_PRIORITY` QoS tagging on the media sockets. Default: **on toward private-network peers** (a LAN/Wi-Fi client — access points map DSCP to WMM airtime priority), off toward routable addresses (some ISP paths bleach or reject marked packets). `1` forces it on everywhere, `0` turns it off entirely. No-op on the wire on Windows without a qWAVE policy. |
| `PUNKTFUNK_OH264_THREADS` / `PUNKTFUNK_OH264_GOP` | `N` | Software (openh264) encoder tuning: encode threads (default 2 — latency over throughput) and GOP length in frames (unset = about ten minutes' worth, `fps × 600`; set `0` for encoder-auto). Only relevant with `PUNKTFUNK_ENCODER=software`. |
| `PUNKTFUNK_MAX_FPS` | `N` (fps) *(default: no limit)* | **Frame limiter for the game** — how fast the compositor lets it render. It does *not* cap the stream: the client still negotiates and receives its full rate, because the encode loop re-encodes the held frame whenever the compositor produced no new one (an almost-empty P-frame). A 60-capped game on a 120 Hz session still sends 120 frames a second, and the GPU time the game gives up goes to capture and encode instead — and to heat and battery on a laptop or handheld. **gamescope only today**: it takes this as `--nested-refresh`, the rate it clamps the game to; that is the nested output's rate, so everything gamescope composites moves at it. Other compositors have no equivalent lever and ignore it. ⚠️ On gamescope that one number is also the refresh the session **reports**: Steam's in-session display settings and every game will read the display as `N` Hz, and a game that paces itself to the display will hold itself there. If you want a quieter box without games believing the panel changed, cap the client's requested refresh instead. |
| `PUNKTFUNK_GAMESCOPE_REFRESH_RATES` | e.g. `60,90,120` *(default: just the session's own rate)* | Extra refresh rates a gamescope session **offers** in its in-session display settings. A headless gamescope has no EDID, so it cannot work out what else the display could run at — without this it advertises exactly one rate and Steam's refresh menu has a single entry. The rate the session actually runs at is always included, so this can only add options. Needs the `punktfunk-gamescope` build (`+pfhdr3`); ignored on a stock gamescope, which has no flag to take it. |
| `PUNKTFUNK_GAMESCOPE_VRR` | `1` · `0` *(default `1` = on)* | Headless gamescope paints on game commits instead of a synthetic vblank. Requires `punktfunk-gamescope` `+pfhdr10` or newer, which keeps `--framerate-limit` active across compositor refresh updates; older builds keep fixed-tick capture. `0` opts out. |
| `PUNKTFUNK_VDISPLAY_HZ_MULT` | `1`–`4` *(default `1` = off)* | Run the **virtual display** at a multiple of the session's frame rate without sending a single extra frame. A compositor paints on its own vblank, so a frame finished just after the capture sampled waits nearly a whole interval to be picked up — the jittery part of the latency budget. At `2` that worst case halves. Costs the compositor and GPU the extra composites, so it's opt-in. If the backend won't give the multiplied rate it reports what it achieved and the stream paces to that. |
| `PUNKTFUNK_LAZY_CAPTURE` | `1` · `0` *(default `1` = on)* | **GNOME 49+**: drive the virtual monitor's frame clock from capture, so it paints once per wire frame instead of on a vblank timer — a still desktop paints nothing, and a source slower than the wire is painted when it commits. Applies only where the compositor announces `node.supports-request`; elsewhere the producer keeps the tick. `0` restores the producer-driven stream. |
| `PUNKTFUNK_KWIN_PACED` | `1` · `0` *(default `0` = off)* | **KWin 6.7+**: keep KWin's own record throttle instead of asking for an unpaced stream. The unpaced offer lets KWin record on its frame signal; KWin before 6.7 rejects it and falls back to a paced twin on its own. Set `1` to A/B a stutter against the throttle. |
| `PUNKTFUNK_DIRECT_CAPTURE` | `1` · `0` *(default `1` = on)* | **Linux, wlroots/Hyprland**: capture the compositor's output with `ext-image-copy-capture-v1` instead of going through the xdg ScreenCast portal. The portal is a second clock in the path — xdg-desktop-portal-hyprland re-requests each frame on a millisecond timer with a 6 ms floor, which halves the rate above ~140 Hz and adds ~3 ms to every frame's age. Used only for GPU zero-copy sessions (it delivers dmabufs); a software encoder keeps the portal's CPU pixels, and any failure falls back to the portal on its own. `0` keeps the portal. |
| `PUNKTFUNK_NVENC_RAW` | `1` · `0` *(default `1` = on)* | **Linux, NVIDIA**: the capture hands the compositor's dmabuf to the encoder as is, and the zero-copy worker converts it in one GPU pass straight into the NVENC input slot — cursor included, any tiling. `0` restores the older path (import, copy, blend), the A/B lever if a picture looks wrong. Falls back on its own when the driver refuses the import. |
| `PUNKTFUNK_VULKAN_DIRECT_PLANES` | `1` · `0` *(default `1` = on)* | **Linux, AMD/Intel, Vulkan Video**: the colour conversion writes its planes straight into the encode picture where the driver allows storage writes to it, saving a copy per frame. `0` keeps the staged copy, the A/B lever for an encoder that misreads a directly written picture. |

## Gamepads

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_GAMEPAD` | `xbox360` · `xboxone` · `dualsense` · `dualsenseedge` · `dualshock4` · `steamdeck` · `switchpro` · `steamcontroller` · `steamcontroller2` (aliases: `ps5`, `edge`, `ps4`, `deck`, `switch`, `sc2`, `ibex`, …) | The virtual pad the host creates. Usually **auto-resolved from the client's physical controller** — set this only to force a type. `xbox360` (XInput) is the universal fallback. `dualsenseedge` gives the client's back paddles native buttons; `switchpro` gives Nintendo-family pads correct glyphs/layout + gyro. `steamcontroller2` (the 2026 Steam Controller) is passed through **as-is** — the host presents a real SC2 (`28DE:1302`) that Steam Input drives directly, mirroring the physical pad's raw reports (Linux and Windows; the Puck dongle's native multi-pad identity is Linux-only — a Windows host folds Puck pads onto the wired identity, which Steam drives the same way). DualSense (Edge)/DualShock 4 work on Linux (UHID) and Windows (UMDF); the Steam Deck pad too (Windows via the promoted UMDF identity); Switch Pro and the classic Steam Controller need Linux UHID. Unsupported choices fold to Xbox 360. |
| `PUNKTFUNK_XBOX_BACKEND` | `hid` · `xusb` *(default: automatic)* | **(Windows)** Which virtual Xbox pad the host builds. `hid` is seen by Steam, SDL, DirectInput and Windows.Gaming.Input, and by XInput through Windows' `xinputhid` filter. `xusb` is an XInput-only Xbox 360 pad. Unset, the host picks `hid` where `xinputhid` is installed and `xusb` where it is not, such as Windows Server. |
| `PUNKTFUNK_STEAM_GADGET` | `1` · `0` | Force the raw USB-gadget virtual Steam Deck on/off. **On by default on SteamOS**, off elsewhere. Lets Steam promote the virtual Deck to full Steam Input. |
| `PUNKTFUNK_DUALSENSE_USBIP` | `1` · `0` *(default off)* | **(Linux, experimental)** Present the virtual DualSense as a **real USB device** over `vhci_hcd`, carrying its own USB Audio Class sound card, instead of as a UHID device. This is what lets a libScePad-style title pair the pad with its own speaker: wine derives a Windows ContainerId by walking sysfs to a `usb_device` parent, which a UHID pad does not have, so on the default path the pad and its speaker both register as `GUID_NULL` and the game never opens the haptic stream. It also gives GE-Proton the real ALSA card its raw-`snd_pcm_open` haptic path scans for. With this on, the pad's audio is captured from its isochronous endpoint and **no PipeWire sinks are minted** — PipeWire builds the real ones from the card. Needs `vhci_hcd` loaded and the `punktfunk` group's write on its sysfs `attach` (both shipped by packaging); degrades to UHID otherwise. |
| `PUNKTFUNK_PAD_AUDIO` | `1` · `0` *(default on)* | Controller audio: what a game plays through the DualSense's built-in speaker and voice-coil haptics is streamed to the client's physical pad as its own low-latency plane. On by default and free while idle — silence is never encoded or sent; `0` turns it off host-wide. On Windows the pad's audio device is a pre-provisioned virtual endpoint; on Linux it is a per-pad PipeWire sink minted with the DualSense identity games match on — see [Controller speaker and haptics](/docs/controller-audio). |
| `PUNKTFUNK_PAD_AUDIO_SLOTS` | `1`–`4` *(default: Windows `1`, Linux `4`)* | How many controllers can have their own audio at once. On Windows each slot is a pre-provisioned virtual endpoint, so the default stays at one; a Linux sink is minted lazily and costs nothing idle, so every slot is on. |
| `PUNKTFUNK_PAD_SINK_NAME` / `PUNKTFUNK_PAD_SINK_DESC` | templates | **(Linux, field debugging)** Override the minted pad sink's `node.name` / `node.description`. `{pad}` and `{mac}` expand per pad. Only for chasing a title whose device matcher wants different strings — the defaults carry every known match surface. |
| `PUNKTFUNK_PAD_SINK_SPLIT_NAME` | node name · `0` *(default: the sink's own name)* | **(Linux, field debugging)** The `api.alsa.split.name` the pad sink advertises. GE-Proton opens that node as `pipewire:NODE=…` with AUX channels for its preferred haptic path; on a real pad it names the hidden 4-channel parent behind the mono speaker split, and our sink has no split, so it names itself. `0` drops the key, which pushes GE onto its Pulse-routed leg instead. |

## Audio / microphone

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_AUDIO_QUALITY` | `low` · `standard` · `high` *(default `high`)* | Desktop-audio encode quality. `high` (stereo 256 kbps Opus, effectively transparent) costs about 1 % of a normal video bitrate, so there's rarely a reason to go lower. `standard` is exactly the pre-0.25 encoder (stereo 128 kbps) — handy for an A/B comparison; `low` is for genuinely constrained links (noticeably lossy on music, still fine for game audio and voice). A typo warns in the log and keeps `high` rather than silently downgrading. Host-side only — clients play whatever arrives, no client setting involved. |
| `PUNKTFUNK_AUDIO_REDUNDANCY` | `1` · `0` *(default: automatic)* | Send audio packets redundantly so a lossy link doesn't crackle. Leave it unset: the host turns redundancy on by itself, only toward clients that support it and only while the link is actually losing packets. `1` forces it on for the whole session, `0` never sends it. |
| `PUNKTFUNK_AUDIO_HIRES` | `0` · `1` *(default: allowed)* | Whether this host serves the **lossless** audio plane — uncompressed PCM (44.1–176.4 kHz, 16/24-bit, stereo through 7.1) instead of Opus. **You don't need to set this** — the host allows it and the *client's* audio-format setting is the opt-in. `0` refuses the plane whatever clients ask. The link is protected mechanically: the plane costs **1.4–8.5 Mbps** in stereo (up to 33.9 for 176.4 kHz/24-bit 7.1), rides outside the adaptive-bitrate loop, and a session gets it only if the cost fits a quarter of its video bitrate. On game content the win is **bit-exactness**, not audibility — 256 kbps Opus is already transparent. Any failed condition (client didn't ask, `0` here, the capture path can't deliver the rate, the link can't spare it) leaves the session on Opus and the host log names which one lost. ⚠️ **The desktop clients read a variable of this same name with a richer grammar** (see [Client-side](#client-side-native-clients)), so on a box that is both host and client one line configures both halves — `0` means *off* to each, and a client-style `96000/24` reads as *allow* here. |
| `PUNKTFUNK_AUDIO_GAIN` | float (default `1.0`) | Gain applied to captured desktop audio — bump it for a quiet source. Applies to **both** the native `punktfunk/1` and Moonlight/GameStream paths. Peaks are rounded off by a soft limiter rather than clipped, so a boost distorts gracefully instead of abruptly; values above `8.0` (+18 dB) are capped, and a non-positive value is ignored. Note this buys **headroom, not loudness** — it cannot make a desktop mix as loud as already-limited streaming-app audio, and pushing it hard to try will audibly squash the signal. On Windows this is the only host-side control that works at all: loopback capture is tapped upstream of the endpoint's master volume, so the speaker slider does not affect what a client receives. |
| `PUNKTFUNK_STREAM_SINK` | *(unset — a host-owned virtual output)* · `stream` · `0` | **(Linux)** Where desktop audio is captured from. **Leave it unset.** The host creates its own virtual output — "Punktfunk Stream Speaker" — makes it the default while a session runs, and records that, so capture never depends on your speakers existing or on HDMI audio surviving a mode change; and because the host declares that output's format, a game can render real 5.1/7.1 into it even when this box's hardware is stereo. The output is a **real** PipeWire node (a `null-audio-sink`), which is what lets the graph run on the host's own clock — a capture-stream stand-in has to borrow a clock from whatever sound card is running. `stream` restores that older arrangement; `0` records whatever your current default output is playing, which follows the default around and hiccups when it changes. While a session runs you'll see the output and a recording stream named `punktfunk-audio-…` in pavucontrol — that is the capture, not a leak. The host log names the live topology (`desktop audio capture topology mode=…`) and which node is clocking it. |
| `PUNKTFUNK_MIC_DEVICE` | name substring | **(Windows)** Target mic-uplink device by friendly-name substring (first match wins). |
| `PUNKTFUNK_MIC_LEGACY_BUFFER` | `1` | Restore the fixed pre-adaptive mic buffering (a ~48 ms prime and ~120 ms cap on Windows; a buffer scaled to the recording app's audio quantum on Linux) instead of the adaptive per-client jitter target. One-release escape hatch: if the microphone coming out of the host only sounds right *with* this set, that's a bug — please report it. |
| `PUNKTFUNK_NO_MIC_INSTALL` | set | **(Windows)** Skip installing the virtual-mic driver (e.g. when the host runs as SYSTEM). |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `client_only` *(default)* · `host_and_client` · `follow_default` | Where desktop audio is audible while a stream runs. `client_only`: the client only — Windows parks playback on a silent endpoint, Linux has apps play into the host's stream output; that's why the PC goes quiet when a stream starts, and everything is put back when it ends. `host_and_client`: the host's speakers keep playing too — Windows captures a real output device, or with `PUNKTFUNK_AUDIO_VOICE_CHAT=host` keeps the silent output and renders the mix to your speakers itself; Linux links the stream output to the output you were using before the session, so the host hears exactly what the clients hear (channels those speakers lack are dropped). `follow_default` never touches your default devices at all — the host just captures whatever your default playback device is (on Windows the mic uplink still picks a target device; you may have to select it yourself). A misspelled value warns in the log and uses `client_only`. The pre-0.25 flags `PUNKTFUNK_HOST_AUDIO=1` and `PUNKTFUNK_KEEP_DEFAULT=1` still work as aliases for the last two; `follow_default` wins if both are set. A client can also ask for the `follow_default` behaviour per session — its [**Keep host audio playing**](/docs/client-settings#audio) setting — without touching this host-wide mode. |
| `PUNKTFUNK_AUDIO_VOICE_CHAT` | `stream` *(default)* · `host` | Where voice-chat apps play while a stream runs. `stream` captures them like everything else — right when you stream your own PC to yourself. `host` keeps them on the output you were using before the session, out of the stream, so friends who stream in and talk on Discord never hear their own voices back: Linux moves the apps' PipeWire streams; Windows writes the per-app output that Sound settings' *App volume and device preferences* page uses, from your signed-in session, and needs the silent virtual output the host mints from Steam's streaming drivers. Both undo it when the session ends. Pair it with `host_and_client` above for the playing-with-friends setup — see [Friends over the internet](/docs/friends-over-the-internet#voice-chat-while-they-play). Recognised apps: Discord (every build), Vesktop, WebCord, ArmCord, Legcord, TeamSpeak, Mumble; `PUNKTFUNK_AUDIO_VOICE_APPS` adds more. |
| `PUNKTFUNK_AUDIO_VOICE_APPS` | comma list | Adds to the recognised voice-chat apps: lowercase fragments matched against an app's name, binary or exe file name — `firefox` for Discord in a browser tab. |
| `PUNKTFUNK_NO_AUDIO_MINT` | set | **(Windows)** Don't provision the host's own dedicated virtual audio endpoints at startup (they're minted from Steam's streaming-audio driver where it's installed, and give capture a stable target that renaming or unplugging hardware can't break). With this set — or whenever minting isn't possible — the host picks devices by name instead, exactly as before 0.25. |

## Clipboard

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_CLIPBOARD` | `off` *(default)* · `text` · `files` | Share the clipboard between client and host. `files` (also `on`/`1`) allows text, HTML/RTF and images **plus file transfer**; `text` (also `text-only`) allows the text and image formats but refuses files. |

This line is only half the switch — your client has a per-host toggle that also has to be on, and
the host needs a clipboard backend underneath. Both, and what a greyed-out toggle means, are on
[Shared clipboard](/docs/clipboard).

## Windows host

Capture of the **secure desktop** — UAC prompts, the lock screen, the login screen — is always on
and has no setting: the host reads the pf-vdisplay driver's ring directly, and those surfaces are in
it. If an older `host.env` on your machine still carries a `PUNKTFUNK_SECURE_DDA` line, nothing reads
it — leave it or delete it, it makes no difference.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_MONITOR_LINGER_MS` | ms (default `10000`) | Defer tearing a per-client virtual display down after disconnect. A reconnect inside the window preempts it and creates a fresh one (a reused IddCx swap-chain is dead); the stable per-client monitor id keeps Windows' saved display config applying either way. Superseded by the console's **Keep alive** setting — see [Virtual displays](/docs/virtual-displays). |
| `PUNKTFUNK_EXCLUSIVE_REASSERT_MS` | ms (default `2000`), `0` = off | How often the host re-checks that **exclusive** display topology actually held. Windows (or a GPU driver / display-poller tool) can quietly re-activate a physical panel moments after the host disabled it — seen on hybrid Intel+NVIDIA laptops — putting windows, the cursor, and the lock screen on a screen that isn't streamed. The host re-asserts and logs when that happens; `0` restores the old fire-and-forget behavior. |
| `PUNKTFUNK_STANDBY_SINK_KEEP` | set to any value but `0`/`off` | Leave a **connected-but-inactive external sink** powered while streaming. By default the host disables such a sink for the stream's duration, because a standby TV left on HDMI keeps Windows composing for a head nobody watches. On the lab box that default cut the median compose hole from 6.3 s to 0.7 s over 16 alternating runs; it is an improvement, not a cure — some holes survive it. Only externals that belong to no topology are picked here, and your laptop panel never is. It also keeps enabled the monitors the stream switched off, which **Disable monitor devices (PnP)** otherwise disables. Set this if the sink must stay awake, for example a capture card or an AVR passthrough. |
| `PUNKTFUNK_RENDER_ADAPTER` | description substring | Multi-GPU boxes only: force the NVENC/capture GPU by adapter Description substring (e.g. `4090`). Leave unset on single-GPU machines. Superseded by a manual GPU preference in the console — see the *Picking a GPU* note under [Core](/docs/configuration#core); it still decides while the console is on Automatic. |
| `PUNKTFUNK_NO_ISOLATE` | set | Legacy topology knob: leave the virtual display **extended** alongside your physical monitors instead of making it the sole desktop. Superseded by the console's **Topology** setting — see [Virtual displays](/docs/virtual-displays). |
| `PUNKTFUNK_HOST_CMD` | `serve` | The host subcommand the service launches. Every install writes **`serve`**. GameStream is the **GameStream** setting, not a flag here: `serve --gamestream` locks it on in the console. `punktfunk-host service install` moves a `serve --gamestream` line, or a missing one, to `serve` with GameStream kept on. |

## Network & discovery

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_HOST_NAME` | free text, e.g. `Living Room` | The name this host shows up under in Moonlight and in the Punktfunk clients. Default: the machine's own hostname — so a box called `bazzite-htpc` can present itself as `Living Room` without renaming the machine. Takes effect on host restart. Spaces and accents are fine; `.` becomes `-` (a dot would split the name in client lists) and it's capped at 63 characters. The machine's real hostname is still what the host answers to on the network. |
| `PUNKTFUNK_MDNS` | `1` · `0` *(default on)* | mDNS adverts (native + GameStream). `0` skips them (same as `--no-mdns`) — for networks/containers where multicast doesn't work; add the host by address in the client instead. |
| `PUNKTFUNK_NATIVE_PORT` | port *(default: `9777`)* | The native punktfunk/1 (QUIC) control port clients connect on — same as `serve --native-port`, which overrides it. Clients discover the port over mDNS, and a host you added by hand keeps whatever port you added it with, so moving this needs no change on the client. A value that isn't a port is a startup error rather than a silent fall back to 9777. |
| `PUNKTFUNK_DATA_PORT` | port | Pin the per-session video data plane to a fixed UDP port — one number to open in a firewall, forward on a router or share through a port proxy; video still follows the client's hole-punch. Same as `serve --data-port`; see [Troubleshooting](/docs/troubleshooting). Default: a fresh random port per session. |
| `PUNKTFUNK_IDLE_TIMEOUT_MS` | ms (default `8000`) | How long the host waits before declaring a client that vanished (cable pulled, Wi-Fi dropped) gone — which is when a kept virtual display starts its linger. Lower it (e.g. `3000`) to reclaim displays sooner; it's clamped to ≥1 s and the keep-alive scales with it, so a live session never false-disconnects. A deliberate quit is instant regardless. Same as `--idle-timeout-ms` on `punktfunk1-host`. |
| `PUNKTFUNK_JUMBO` | `1` | Stream in **jumbo frames** — ~9000-byte packets instead of the standard ~1500-byte ones, so a high-bitrate session spends less CPU and per-packet overhead on a wired LAN. Off by default, and safe to turn on: see the note below the table. |
| `PUNKTFUNK_WIRE_MTU` | on-wire IP MTU, e.g. `9000` | The pick-your-own-number version of the same switch — and also the escape hatch for **small**-MTU links. A value above 1500 enables jumbo frames with your number as the target (and outranks `PUNKTFUNK_JUMBO`); a value *below* 1500 shrinks every session's packets from the start, for a path that can't carry full-size ones (a VPN or tunnel — the host normally learns this by itself, but the override skips the one degraded first session). Use the on-wire IP MTU your NIC reports (`ip link` on Linux, `netsh interface ipv4 show subinterfaces` on Windows) — IP/UDP overheads are subtracted for you. |

> **Jumbo frames** need every hop to carry them, and the host verifies rather than trusts.
> Sessions still *start* on standard-size packets; with the opt-in set, the host probes the path,
> and only once the probe proves it — and the client acknowledges the switch — does the stream
> grow to the large packets, mid-session, with no reconnect. A path that can't take them, or an
> older client, simply stays at the standard size; the only cost of leaving the opt-in on is a few
> extra probe packets at connect. Two things it can't do for you: the host NIC, the client NIC and
> every switch in between must have jumbo frames enabled in *their* settings first (usually a
> field called MTU, set to `9000`) — and both ends need Punktfunk 0.25 or newer. Native
> `punktfunk/1` sessions only; Moonlight sessions always use standard packets.

## Auth, API & paths

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_MGMT_TOKEN` | token | Bearer token for the management API. Normally **not set**: the host generates one into `~/.config/punktfunk/mgmt-token` (owner-only), which the console, `ctl`, the tray and the plugin runner all read. A value set here is written to that file and then removed from the host's environment, so the games and hooks it launches never inherit it — which is also why it does not belong in `host.env` on a box you share. |
| `PUNKTFUNK_UI_PASSWORD_HASH` | argon2id hash | What a web-console login is checked against. The console writes it into `~/.config/punktfunk/web-password` itself — see [Forgot your Password?](/docs/forgot-password). |
| `PUNKTFUNK_UI_PASSWORD` | password | Web-console login password in clear. Generated on first start, and the reset path afterwards: the next sign-in replaces it with the hash above. |
| `PUNKTFUNK_PLUGIN_TOKEN` | token | The scoped token the [plugin/scripting runner](/docs/plugins) uses — a narrower credential than `PUNKTFUNK_MGMT_TOKEN`, never full admin. Same treatment: generated into `~/.config/punktfunk/plugin-token`, and a value set here is persisted and dropped from the environment. |
| `PUNKTFUNK_MGMT_BIND` | `IP:PORT` *(default: `0.0.0.0:47990`)* | Where the management API listens. The `--mgmt-bind` flag overrides it. Two reasons to set it: pin `127.0.0.1:47990` to keep the API off the LAN entirely (paired clients then can't browse your library), or **move the port to share the machine with Sunshine, Apollo or Vibeshine** — 47990 is their web UI as well as our management API, and it's the only port the two still share once GameStream compat is off. Everything downstream follows the port you pick: native clients learn it from discovery, and the web console, the plugin runner (and so every library plugin) and the status tray read it from `~/.config/punktfunk/mgmt-endpoint` (`%ProgramData%\punktfunk\mgmt-endpoint` on Windows), which the host writes on every start. See [another streaming host is installed](/docs/troubleshooting#another-streaming-host-sunshine-apollo--is-installed). |
| `PUNKTFUNK_UI_BIND` | address *(default: `0.0.0.0`)* | Where the web console listens — `0.0.0.0` for your network, `127.0.0.1` for this machine only, or one address such as a VPN interface. On any bind the console answers only peers on the local network or a VPN (private and link-local addresses, IPv6 unique-local, a Tailscale tailnet) and refuses the internet. The plugin-UI origin (`PUNKTFUNK_UI_PLUGIN_PORT`, below) always follows it, so 47993 is never wider than 47992. The port stays `PORT` / the console's own 47992. The guided installer asks for this and writes it here; `punktfunk-setup --web-bind=localhost` and, on Windows, `punktfunk-host service install --web-bind=127.0.0.1` set it without the question. On NixOS the equivalent is `services.punktfunk.web.bind`, and `host.env` is not read by the console. |
| `PUNKTFUNK_CONFIG_DIR` | path | Override the config directory (default `~/.config/punktfunk`) — pairing state, certs, apps.json, captures. |
| `PUNKTFUNK_PLUGIN_SANDBOX` | `off` | Run [plugins](/docs/plugins) in the runner's own process instead of one sandbox each. That gives every installed plugin your account's access — your files, your session, the host's credentials — so the console keeps saying so while it is set. Only reason to: a box whose kernel refuses unprivileged user namespaces, where the alternative is no plugins at all. The runner does not read `host.env`: set it with `systemctl --user edit punktfunk-scripting` as `Environment=PUNKTFUNK_PLUGIN_SANDBOX=off` under `[Service]`. |
| `PUNKTFUNK_UI_PLUGIN_PORT` | port *(default: console port + 1)* | The separate port [plugin](/docs/plugins) UIs are served from. They get their own origin on purpose — a plugin page can never act as *you* on the console. If the console log says this port couldn't be opened (plugin UIs then stay disabled rather than sharing the console's origin), point it at a free port and restart. |
| `PUNKTFUNK_LIBRARY_ART_ROOTS` | directories, separated like `PATH` (`;` on Windows, `:` on Linux/macOS) | Where the host is allowed to read game artwork from when serving your library. Defaults to sensible platform roots: your home directory on Linux/macOS, and on Windows the users base (`C:\Users`) plus your Steam and Playnite installs, wherever they are — including a portable Playnite on another drive, which keeps its covers next to the program. Set it when box art lives somewhere else again — a second drive, a network mount, or a launcher installed outside all of those. Setting it **replaces** the defaults, so list every root you need. The host log's "dropped local art the proxy may not serve" line is this knob's cue: those entries still appear in your library, but their covers stay blank until the root is allowed. The cover store below is always readable and does not need listing here. |
| `PUNKTFUNK_LIBRARY_ART_CACHE` | path | Where the host keeps covers it downloaded for your clients (default `~/.cache/punktfunk/art`, `%LOCALAPPDATA%\punktfunk\art` on Windows). Each cover is fetched once, on the first client that asks for it, and served from here afterwards — so a shelf still draws when the site the cover came from is down. `punktfunk-host library art --clear` empties it. |
| `PUNKTFUNK_LIBRARY_ART_CACHE_MB` | megabytes *(default: 512)* | Ceiling on that store. Over it, the host deletes the oldest files until it is under again, and those covers are re-fetched the next time a client asks. |

## Updates

The host checks for a newer release and, where the platform allows it, can install it from the web
console. Both halves have a kill switch — see [Updating the Host](/docs/updating).

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_UPDATE_CHECK` | `0` · `false` · `off` | Never contact the update feed. The console's Updates card then shows checks as disabled; everything else keeps working. |
| `PUNKTFUNK_UPDATE_APPLY` | `0` · `false` · `off` | Keep the check but remove the **Update now** button, so the console only ever tells you the command to run. |

## Advanced performance tuning

Leave these at their defaults unless you're chasing latency; see the [troubleshooting](/docs/troubleshooting)
notes for context.

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_FRAME_DRIVEN` | `1` *(default)* · `0` | Wake the encoder when the capture actually delivers a frame, instead of sampling on a fixed tick. On by default on both protocols (a capture backend without an arrival signal keeps the tick regardless); `0` restores the tick everywhere. The tick costs about half a frame interval of latency per frame, so leave this on unless you are bisecting a cadence problem. |
| `PUNKTFUNK_GAMESTREAM_ADAPT` (was `PUNKTFUNK_GS_ADAPT`) | `1` *(default)* · `0` | GameStream/Moonlight only: let the host act on the packet loss Moonlight reports — raising error correction as loss appears, winding it back when the link is clean, and easing the bitrate off under sustained loss (recovering as it settles). `0` pins error correction and bitrate at their configured values for the whole session. |
| `PUNKTFUNK_GAMESTREAM_ENCRYPT` (was `PUNKTFUNK_GS_ENCRYPT`) | `supported` *(default)* · `video` · `off` | GameStream/Moonlight only: offer per-packet video encryption (`SS_ENC_VIDEO`) and the V2 control-encryption scheme (`SS_ENC_CONTROL_V2`) to clients that support them. **On by default** — the host offers, the client decides; Moonlight generally accepts both, and error correction still recovers lost packets normally. V2 gives the control channel a per-direction nonce, which the older scheme lacks. `video` offers video encryption only, leaving the control channel on the older scheme; `0` turns both offers off (the plaintext video wire earlier versions sent). Audio and the control channel are encrypted either way. |
| `PUNKTFUNK_GSO` | `1` · `0` | UDP segmentation offload on the send path (coalesce a frame's packets into kernel super-buffers) — cuts send CPU ~30%, but its line-rate packet trains can cost delivered throughput on constrained links (measured on a 2.5GbE hop). The default differs by platform. **Windows: on by default** (Send Offload — the lever that gets past ~1 Gbps, since Windows otherwise does one send call per packet); set `0` if a constrained link shows lost throughput. It also latches itself off for the rest of the run the first time the OS/NIC/path rejects an offloaded send. **Linux: off by default** until send pacing spaces the super-buffers; set `1` to opt in (auto-falls back to `sendmmsg` on kernels/paths without support). |
| `PUNKTFUNK_SPLIT_ENCODE` | `0`/`disable` · `1`/`auto` · `2` · `3` | NVENC N-way split-encode for very high pixel rates (5K@240). `auto` picks automatically above ~1 Gpix/s. H.264 never splits (not applicable per the SDK); on HEVC a *forced* split disables sub-frame readback (mutually unsupported) — set `0` to choose sub-frame instead. |
| `PUNKTFUNK_NVENC_SUBFRAME` | `0` · `1` | NVENC sub-frame (slice-level) readback for lower latency on sync sessions. Default: on where the GPU supports it (Linux direct NVENC). `0` = never; `1` = force. On HEVC it yields to a forced split-encode (the SDK documents the pair unsupported). |
| `PUNKTFUNK_NVENC_SPLIT_ARBITRATE` | `1` | Opt-in: let the host change its split-encode decision **live**, mid-session, as the pixel rate moves, instead of only choosing once at session start. Currently wired on the Linux direct-NVENC path. Only interesting alongside `PUNKTFUNK_SPLIT_ENCODE=auto` at very high pixel rates. |
| `PUNKTFUNK_GPU_PRIORITY_CLASS` | `off` · `normal` · `high` · `realtime` | **(Windows)** GPU scheduling priority for capture/encode under a GPU-saturating game. Default **`realtime`** — the stream's capture and encode preempt the game instead of waiting behind it (the same lever Sunshine and OBS use), which costs the local game some fps by design. Set `high` if NVENC freezes on a HAGS setup with VRAM near-full, or as a last-resort A/B for the log's `METRONOMIC` capture-stall warning — on some AMD boxes that quiets the pattern, but it masks the underlying disturbance rather than fixing it. The vdisplay driver's own raise has the same shape: default realtime, `setx /M PFVD_NO_RT_GPU 1` + device restart disables it. Every capture session logs the resolved posture as `GPU-priority posture for this capture session`. |
| `PUNKTFUNK_IDD_ADAPTIVE` | `1` *(default)* · `0` | **(Windows)** The adaptive pipeline-depth machinery: the host walks the depth up under sustained encode overrun and back down when clean. `0` pins the full configured depth **and disables the whole encode-cadence detector with it** — including the "encode behind cadence" ABR climb refusal — so leave it on unless you are deliberately A/B-ing that machinery. |
| `PYROWAVE_QUEUE_PRIORITY` | `realtime` *(default)* · `high` · `off` | [PyroWave](/docs/pyrowave) sessions only — the *intent*, forwarded to whichever process does the encode. PyroWave encodes on the same GPU shader cores a game uses, so a demanding game can starve it and the frame rate drops. This asks the driver to schedule the encode ahead of the game. `realtime` tries the strongest class and falls back to `high`; `high` asks only for the middle one; `off` disables the request. A driver that refuses simply encodes at normal priority — it can never stop a session starting. Granting the request needs the `CAP_SYS_NICE` capability, which the Linux packages give to `punktfunk-encode-worker` and **never** to `punktfunk-host` — a host holding any capability cannot be identified by KWin and loses desktop streaming entirely. Do not `setcap` the host to "make this work"; see [Running as a service](/docs/running-as-a-service#gpu-scheduling-priority). Set `off` if you see the desktop stutter while streaming. |
| `PUNKTFUNK_ENCODE_WORKER` | path · `off` | Where the host looks for `punktfunk-encode-worker`, the small capability-carrying helper that owns the priority-elevated [PyroWave](/docs/pyrowave) encode (previous row). Unset, the host looks beside its own binary and then on `PATH`, which is right for every package — set it only when the worker lives somewhere unusual. **NixOS needs it and the module sets it for you:** a file capability cannot live on a read-only nix store path, so the worker is exposed through `security.wrappers` and this points the host at that wrapper. `off` forces the encode back into the host process at default priority — a debug escape hatch, not a tuning knob. Every failure short of that is already handled: a missing binary, a worker that will not start, or one that dies mid-session falls back to encoding in-process with one line in the log, and never drops the session. |
| `PUNKTFUNK_SCRIPTING` | path | Where the host looks for `punktfunk-scripting`, the runner that performs every [plugin](/docs/plugins) package op (`plugins add`/`remove`/`list`, and the console's store installs). Unset, the host looks beside its own binary, then on `PATH`, then in the packaged `/usr` and `~/.local` layouts — right for every package, so set it only when the runner lives somewhere unusual. Like the row above it is **not** existence-checked: a path you name is a path you get, so a typo fails naming itself instead of quietly running a different runner. Worth knowing: the console runs installs inside the host *service*, whose `PATH` is normally much shorter than your login shell's — if `punktfunk-host plugins add` works and the console says the runner isn't installed, that gap is why, and this is the fix. |

## Diagnostics

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_PERF` | `1` | Log per-stage timing (capture, encode, send) — handy when tuning latency. |
| `RUST_LOG` | `info` · `debug` · `trace` | Log verbosity. On Windows, logs land in `%ProgramData%\punktfunk\logs\` (size-capped: a file over 10 MB is rotated to `.old` at the next service/host start, one generation kept). |
| `PUNKTFUNK_VIDEO_DROP` | `N` (percent) | Deliberately drop N% of video packets to exercise FEC recovery. **Testing only.** |

## Client-side (native clients)

A few knobs are read by the native **clients**, not the host — with one exception noted in the
table, where client and host read the *same* variable name for their own half of one feature:

| Setting | Values | Meaning |
|---|---|---|
| `PUNKTFUNK_DECODER` | `native-vulkan` · `native-vaapi` (Linux) · `native-d3d11va` (Windows) · `software` | Force the decode path. Default auto-selects hardware per GPU vendor and falls back on its own: **Linux** — Vulkan Video first on NVIDIA and AMD, VAAPI first on Intel and anything else; **Windows** — Vulkan Video first on NVIDIA and AMD, D3D11VA first on Intel and anything else. Whichever isn't first is the next thing tried, with software last (OpenH264 for H.264, rav1d for AV1 — there is no software HEVC, so a client that lands there reconnects on a codec it can decode). The names are the ones the [stats overlay](/docs/stats) prints, so a pin and a reading match. The older spellings `vulkan`, `vaapi` and `d3d11va` named the FFmpeg-backed decoders the clients used before and still work — each migrates onto the native path for the same hardware, and the client says so in its log. |
| `PUNKTFUNK_VAAPI_DEVICE` | path, e.g. `/dev/dri/renderD129` | **(Linux)** Pin the DRM render node the `native-vaapi` decoder opens. Unset, the client tries the nodes in order and takes the first that can decode the stream — set this on a multi-GPU box when it lands on the wrong one. |
| `PUNKTFUNK_PREFER_PYROWAVE` | `1` | Ask for the [PyroWave](/docs/pyrowave) wavelet codec on a wired link, where the client's own setting isn't reachable (the gamepad console, a headless launch). |
| `PUNKTFUNK_PAD_SPEAKER_PATH` · `PUNKTFUNK_PAD_SPEAKER_VOLUME` | byte, hex or decimal *(default `0x20` / `0x7F`)* | Which output a DualSense sends [controller audio](/docs/controller-audio) to, and how loud. A controller's channel 1 is shared between its headphone jack and its built-in speaker, and it powers up pointing at the jack — so with no headphones plugged in the speaker stays silent however correctly the audio is routed. Punktfunk points it at the speaker when controller-speaker is on. Change these only if your pad's speaker stays quiet; a game that sets its own audio levels still overrides them. |
| `PUNKTFUNK_PAD_AUDIO_PROFILE` | `0` | **(Linux)** Stop the client from switching a wired DualSense's sound card to **Pro Audio** while it streams [controller audio](/docs/controller-audio) to it. The switch exists because a controller's voice coils are channels 3 and 4 of its sound card, and a controller almost never presents four channels on its own — on any other profile the haptics are folded into the speaker pair and felt as nothing. Punktfunk restores the card's profile when the session ends and never saves it. Set this if you'd rather select the card's profile yourself. |
| `PUNKTFUNK_OSD_SCALE` | multiplier, e.g. `1.5` *(default `1`)* | Size of the in-stream overlay — the stats OSD, the capture hint and the start banner. They already follow your display's scaling setting (200 % display → twice the pixels), so set this only to nudge that: bigger for a TV across the room, smaller if your compositor reports an aggressive scale. Clamped to 0.5×–4×, and a line that would run off the screen is shrunk to fit. |
| `PUNKTFUNK_AUDIO_HIRES` | `1`/`on`/`true`/`yes` · `48000` · `96000` · `<rate>/<bits>` · `0`/`off`/`false`/`no` *(unset: the client's stored audio-format choice decides)* | ⚠️ **The same name as the host's policy gate in Audio / microphone above, and a different grammar** — so one line on a box that is both host and client sets both halves. This is the **request** half, and it overrides the client's stored audio-format choice for the run. `1` asks for 96 kHz / 24-bit, the rung the plane earns its bandwidth at. A bare rate — `48000` or `96000` — asks for that rate at 24-bit. `<rate>/<bits>` names both, which is the only way to reach `48000/16`: the cheapest lossless rung (~1.5 Mbps), and one no menu offers, because at 16-bit there is nothing left to *hear* over the 256 kbps Opus it replaces — only bit-exactness. `0` forces Opus even when the stored choice asks for lossless. Anything else is a typo: the client warns and **ignores** it, so the stored choice still decides rather than being silently switched off. And asking is not getting — a capture path that genuinely delivers the rate, the link budget, a frame that fits a datagram at that channel count, and the host not having set its own half to `0` all still have to agree, and the client plays whatever the host answers. (The host's half no longer has to be turned *on*, as it did before 2026-08-17 — so this request is usually the only one that matters.) Linux and Windows clients. |
| `PUNKTFUNK_NO_AEC` | `1` | Turn the microphone's echo cancellation off for this run, whatever **Echo cancellation** says in [client settings](/docs/client-settings#audio). One-way: it can only switch the processing off, never back on, and the setting is the normal way to control it. Linux and Windows clients. |
| `PUNKTFUNK_PRESENT_MODE` | `mailbox` *(default)* · `fifo` · `immediate` · `fifo_relaxed` | How decoded frames meet the display (the Vulkan present mode). The default prefers MAILBOX — tear-free without queueing behind the vertical refresh — and falls back to FIFO (classic vsync) where the driver doesn't offer it. **AMD's Windows driver offers no MAILBOX**, so those clients run FIFO, which adds a standing frame-pacing wait (up to one refresh interval). `immediate` removes that wait but can tear; `fifo_relaxed` only tears when a frame is late. If your latency floor matters more than tearing, try `immediate` and judge by eye. |
| `PUNKTFUNK_PRESENTER` | `arrival` | Turn the frame-pacing engine off for this run: frames present the instant they decode, exactly as they did before the **Prioritize** setting existed. A diagnostic — if a pacing change is suspected of causing judder or added delay, this switches it off without reinstalling anything. Linux and Windows clients. |
| `PUNKTFUNK_VRR_FIFO` | `1` | Force the display mode used to follow a **variable-refresh (VRR / FreeSync / G-Sync)** screen, on graphics drivers too old to offer the modern one. You almost certainly don't need this: where the driver supports the modern mode — which is what **Follow variable refresh rate** in [client settings](/docs/client-settings#video) uses — following the panel is already automatic and costs almost nothing. On an older driver the only way to follow the panel is a mode that measured roughly 27 ms *worse* on a fixed-refresh screen, so it stays off unless you ask for it, and it's only worth asking if you genuinely have a VRR screen and play fullscreen. Check the Detailed [stats overlay](/docs/stats): `vrr yes` means the panel really is following the stream. Linux and Windows clients. |
| `PUNKTFUNK_PRESENT_DEBUG` | `1` | Log the presenter's own 1-second summary (display mode, buffer drops, pacing counters) every second, even when nothing is going wrong. Without it the line appears only when there is something to report. |
| `PUNKTFUNK_ABR_PROBE_KBPS` | kbps, e.g. `90000` | The highest rate the startup link measurement will ask for. Against a current host that measurement is a ramp of short steps, run before the first video frame, which stops as soon as the link refuses a step or the steps prove what your resolution, refresh rate and codec could plausibly use — so it is already bounded, and this only lowers it. Against an older host it is the single burst's target instead, which defaults to twice that same figure, capped at 2 Gbps. |
| `PUNKTFUNK_ABR_PROBE` | `0` | Skip the startup link measurement entirely. Nothing then knows what the link holds, so the adaptive bitrate opens at the negotiated starting rate and climbs toward what the stream could use, finding the limit by running into it. A blunt instrument; prefer `PUNKTFUNK_ABR_MAX_MBPS`. |
| `PUNKTFUNK_ABR_MAX_MBPS` | Mbps, e.g. `300` | Hard cap on the adaptive bitrate's climb ceiling, whatever the startup measurement found. The escape hatch when adaptive sessions keep climbing past what your client's **decoder** can sustain (periodic hitch + "receive backlog stopped draining" in the client log). An explicit bitrate setting still bypasses ABR entirely. |

## Bitrate

The client requests a bitrate; there's no host-side bitrate knob. On the native `punktfunk/1`
plane the number is a **total wire budget**: what actually leaves the host — video, error-correction
parity, packet framing, and the audio plane's share — fits inside it, and when adaptive FEC adds
parity in answer to loss, the video rate comes down so the wire doesn't go up. (Older pairs, and
the GameStream/Moonlight plane, keep the historical meaning: the number programs the encoder, and
overheads ride on top.) To find a good value:

- **Native clients (Apple, Linux, Windows, Android):** use the built-in **speed test**, from a
  host's menu, or its host page on Apple. It measures your link, suggests a bitrate, and applies it.
- **Moonlight:** set the bitrate in Moonlight's settings. Start moderate and raise it.

## Multiple devices at once

The native `punktfunk/1` host (`serve`) streams up to **4 sessions at once** by default (an encoder
bound); further clients wait in the accept queue until a slot frees up. Each session gets its own
virtual display at the client's exact resolution, sharing the host's input/audio/mic services. The
limit isn't settable from `serve`'s command line yet — `punktfunk1-host`, the standalone test host,
exposes it as `--max-concurrent N` (see the [Host CLI](/docs/host-cli) reference).

## Codec and FEC

- Client and host **negotiate the codec**: **HEVC (H.265)** by default, **AV1** for clients that
  support it, and **H.264** when the session runs on the GPU-less software encoder.
- Both protocols add forward error correction for lossy links, and both adapt it to the loss the client reports — see `PUNKTFUNK_FEC_PCT` above.

## Settings documented elsewhere

Not everything you can configure is a `host.env` line, and a few knobs are explained on the page
that owns their feature:

- **Virtual-display policy** — keep-alive, topology, per-client scaling — lives in the web console
  and `display-settings.json`: [Virtual displays](/docs/virtual-displays).
- **Which GPU to use** on a multi-GPU box is a console choice (`gpu-settings.json`) that outranks
  `PUNKTFUNK_RENDER_NODE` / `PUNKTFUNK_RENDER_ADAPTER` — see *Picking a GPU* above.
- **Event hooks and webhooks** are `hooks.json`, not environment variables:
  [Events & hooks](/docs/automation).
- **Updating**, including the one-click opt-in on Linux: [Updating the Host](/docs/updating).
- **Encoder prerequisites** (Mesa/VAAPI packages, Intel HuC firmware, NVIDIA driver bits):
  [Requirements](/docs/requirements).
- **Full chroma (4:4:4)** — which codecs, capture paths, GPUs and clients can carry it:
  [Client settings](/docs/client-settings#video).
- **Client settings** — resolution, bitrate, codec, decoder, HDR — are set in the client app, with
  what each one defaults to and which of them the host can overrule:
  [Client settings](/docs/client-settings).

The host also reads a number of debugging and development variables that aren't listed here; they
change between releases and are not meant for everyday use.
