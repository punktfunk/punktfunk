---
title: Configuration
description: Host settings in the web console, the host.env file that pins them, and the environment variables for everything else.
---

Every host setting is on the web console's **Host → Settings** page; `host.env` pins those settings
and holds the environment-only variables below. The host detects the compositor, input backend and
encoder on its own, so most variables here are overrides you rarely need.

## host.env

| | Linux | Windows |
|---|---|---|
| File | `~/.config/punktfunk/host.env` | `%ProgramData%\punktfunk\host.env` |
| Apply an edit | `systemctl --user restart punktfunk-host` | `punktfunk-host service restart` (elevated) |

One `KEY=value` per line; `#` starts a comment and keys are case-sensitive. The host reads the file
when it starts, so an edit does nothing until you restart it. A console setting set here shows as
locked in the console until you remove the line.

Resolution and bitrate aren't host settings — the client picks them. See [Bitrate](#bitrate).

## Settings in the web console

Each row is on **Host → Settings**; **Show advanced** reveals the rest, and searching for the
`host.env` name finds a row. A `host.env` line or a command-line flag wins and locks the row: remove
it and restart to hand the setting back. **Restart Punktfunk** applies the rows marked *after a
restart*.

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
| PyroWave quality | `PUNKTFUNK_PYROWAVE_BPP` | 0.25–4 bits/pixel | `1.6` | next session |
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

The table shows the Linux values. On Windows, **Encoder** takes `auto` · `nvenc` · `amf` · `qsv` ·
`mf`, and **Default gamepad** takes `auto` · `xbox360` · `xboxone` · `xboxelite` · `dualsense` ·
`dualsenseedge` · `dualshock4` · `steamdeck` · `steamcontroller2`. Older names still work:
`PUNKTFUNK_GS_ENCRYPT`, `PUNKTFUNK_GS_ADAPT`, `PUNKTFUNK_HOST_AUDIO=1` (**Where audio plays** =
`host_and_client`) and `PUNKTFUNK_KEEP_DEFAULT=1` (= `follow_default`).

### What some settings do

| Setting | What to know |
|---|---|
| **GameStream encryption** | **Offer** offers video, audio and control encryption and lets Moonlight choose; **Video only** encrypts video; `off` offers none; **Require** refuses a client that can't encrypt. |
| **ChaCha20 cipher** | Used only by clients without AES hardware that ask for it (older ARM TVs); the rest stay on AES-GCM. Both are full strength. |
| **Encoder** | `software` is Linux-only, and `auto` never picks it. How `auto` chooses: [Support matrix](/docs/support-matrix#how-the-host-picks-a-backend). |
| **Full color 4:4:4** | A host-side allow; each client's **Full chroma** setting asks for it. Native clients only — Moonlight stays 4:2:0. |
| **Game frame limit** | gamescope only. Games also see the display as that many Hz; the stream still runs at the client's rate. |
| **Cursor capture** | A troubleshooting knob: which cursor mode the host asks the screen-cast portal for. `auto` settles on one the portal offers. |
| **Vulkan encoding** | AMD and Intel: HEVC and AV1 through Vulkan Video, which recovers from loss without a full keyframe. Off uses VAAPI. |
| **Direct capture** | wlroots and Hyprland: capture the output directly instead of through the portal, on GPU sessions. A failure falls back to the portal. |
| **On-demand capture** | GNOME 49+: the virtual monitor paints once per streamed frame instead of on a timer. |
| **KWin capture pacing** | KWin 6.7+: keep KWin's own recording throttle. |
| **PyroWave bitrate cap** | A ceiling on every [PyroWave](/docs/pyrowave) session's bitrate, over what the link measurement found. |
| **Where audio plays** | **Device only**: the host goes quiet while streaming. **Device and host**: the host's speakers play too. **Host's own output**: the host leaves its audio devices alone and captures what plays there — a client asks for this per session with **Keep host audio playing**. |
| **Audio quality** | `high` is stereo 256 kbps Opus, `standard` 128 kbps, `low` for tight links. |
| **Lossless audio** | Serves uncompressed audio (1.4–8.5 Mbps in stereo) to a client that asks, when it fits a quarter of the video bitrate. The desktop clients read the same variable to ask — see [Client-side](#client-side-native-clients). |
| **Voice chat** | **On the host** keeps voice-chat apps on the host's output, so friends who stream in don't hear themselves. Recognised: Discord, Vesktop, WebCord, ArmCord, Legcord, TeamSpeak, Mumble; **Voice chat apps** adds name fragments such as `firefox`. See [Friends over the internet](/docs/friends-over-the-internet#voice-chat-while-they-play). |
| **Audio redundancy** | `auto` sends audio twice only to clients that support it, and only while the link loses packets. |
| **Steam per seat** | Each paired device's Steam launch runs under its own home in `~/.local/share/punktfunk/seats/`, so two devices play at once and the desktop Steam keeps running. Each seat signs in to Steam once, and one Steam account plays on one seat at a time. Needs a native Steam; the seat reuses your library folders. |
| **Pads per seat** | Each seat's Steam sees only its own device's controllers. Needs **Steam per seat** and `bwrap`. The box's own Steam still sees every pad. |
| **Seats kept warm** | How many seats keep Big Picture running before their device connects, so a launch skips Steam's cold start. Only seats that launched a Steam title in the last 14 days. |
| **Disconnect timeout** | How long before a vanished client counts as gone and a kept display starts its linger. A deliberate quit is instant. |

## Session anchors

Leave these unset. As a `systemctl --user` service the host follows the active session (Game Mode ↔
Desktop) and rewrites them on every connect.

| Variable | Set it only when |
|---|---|
| `XDG_RUNTIME_DIR` | The host runs outside a user service (ssh, cron): `/run/user/<uid>`, with the uid from `id -u`. A wrong uid breaks audio, capture and connections. |
| `DBUS_SESSION_BUS_ADDRESS` | Same case: `unix:path=/run/user/<uid>/bus`. |
| `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP` | The [headless KDE appliance](/docs/kde#headless-session), whose `host.env.kde` sets them. |

## Core

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_COMPOSITOR` | `kwin` · `mutter` · `gamescope` · `wlroots` · `hyprland` (also `kde`, `plasma`, `gnome`, `sway`, `wlr`, `river`, `hypr`) | Pins the virtual-display backend and turns session following off, so a Game Mode ↔ Desktop switch ends the stream. For single-purpose appliances only. |
| `PUNKTFUNK_INPUT_BACKEND` | `libei` · `kwin` · `gamescope` · `wlr` | Pins input injection. Detected with the compositor; an unknown value is ignored. |
| `PUNKTFUNK_VIDEO_SOURCE` | `virtual` (default) · `portal` | Moonlight sessions only: `portal` streams an existing monitor, which the GNOME 50+ HDR mirror needs — see [HDR](/docs/hdr#linux--gnome). |
| `PUNKTFUNK_CAPTURE_MONITOR` | connector name, e.g. `DP-2` | Linux: stream this physical monitor instead of a virtual display, overriding the console. `punktfunk-host list-monitors` lists the names; one that matches nothing fails the session. See [Virtual displays](/docs/virtual-displays#stream-a-real-monitor-instead). |
| `PUNKTFUNK_ZEROCOPY` | `1` · `0` | Linux: zero-copy capture to encode, on by default with a CPU fallback. `0` forces the CPU path; `1` turns a failed zero-copy setup into an error. |
| `PUNKTFUNK_RENDER_NODE` | path, e.g. `/dev/dri/renderD129` | Linux: the GPU to capture and encode on (default `/dev/dri/renderD128`). A GPU picked under **Host → GPUs** wins. |
| `PUNKTFUNK_RENDER_ADAPTER` | name substring, e.g. `4090` | The GPU to encode on, by adapter name. A GPU picked under **Host → GPUs** wins. |
| `PUNKTFUNK_VAAPI_LOW_POWER` | `0` | Intel on Linux: skip the low-power (VDEnc) encoder. Unset uses it wherever it exists and does bitrate control. |

## gamescope (Linux)

Env-only additions to the **Game Mode** rows above. See [gamescope](/docs/gamescope).

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_GAMESCOPE_SESSION` | `steam` | Own a `gamescope-session-plus` session at the client's mode — for a headless appliance. |
| `PUNKTFUNK_GAMESCOPE_NODE` | `auto` · node id | Attach to a running gamescope's video node at its own mode. Outranks `PUNKTFUNK_GAMESCOPE_SESSION`. |
| `PUNKTFUNK_GAMESCOPE_APP` | command | What a bare gamescope session runs, e.g. `vkcube`. |
| `PUNKTFUNK_GAMESCOPE_BIN` | path | The gamescope binary for sessions the host starts. Unset: `punktfunk-gamescope` on `PATH`, then `gamescope`. |
| `PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR` | path | Where the Vulkan layer that gives nested games an HDR10 swapchain lives (default `/usr/lib/punktfunk/vulkan/implicit_layer.d`; the NixOS module sets it). |

## Compositor-specific (Linux)

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_KWIN_VIRTUAL_PRIMARY` | `1` · `0` | `1` makes the streamed output the only desktop, `0` extends it. **Topology** in [Virtual displays](/docs/virtual-displays) supersedes it. |
| `PUNKTFUNK_MUTTER_VIRTUAL_PRIMARY` | `1` · `0` | The same; either name works on any compositor, and the first one set wins. |

## Hooks

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_RECOVER_SESSION_CMD` | command | Linux: runs (at most once a minute) when a client connects while no graphical session is live, such as after a compositor crash — typically `sudo -n systemctl restart gdm` with a matching sudoers rule. |
| `PUNKTFUNK_ON_CONNECT_CMD` | command | Runs when a client connects, on either protocol, with the event JSON on stdin and `PF_EVENT_*` variables. Filters and webhooks: [Events & hooks](/docs/automation). |
| `PUNKTFUNK_ON_DISCONNECT_CMD` | command | The same on disconnect; `PF_EVENT_REASON` is `quit`, `timeout` or `error`. |

## Video

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_FEC_PCT` | `0`–`90` (percent) | Pins error correction and turns adaptive FEC off (normally 5–50 %, starting at 10 %). `0` disables it. On GameStream it sets the starting percent and floor instead. Leave it unset. |
| `PUNKTFUNK_OH264_THREADS` | number (default `2`) | Software encoder threads. |
| `PUNKTFUNK_OH264_GOP` | frames (default fps × 600) | Software encoder keyframe interval; `0` lets the encoder decide. |
| `PUNKTFUNK_VDISPLAY_HZ_MULT` | `1`–`4` (default `1`) | Runs the virtual display at a multiple of the stream rate, so a frame waits less for the compositor's next paint. Costs GPU time. |
| `PUNKTFUNK_NVENC_RAW` | `1` · `0` | NVIDIA on Linux: convert the captured buffer straight into NVENC's input, on by default. `0` uses the copy-and-blend path. |
| `PUNKTFUNK_VULKAN_DIRECT_PLANES` | `1` · `0` | Vulkan Video on Linux: write the converted picture straight into the encoder where the driver allows, on by default. `0` keeps the extra copy. |

## Gamepads

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_XBOX_BACKEND` | `hid` · `xusb` *(default: automatic)* | **(Windows)** Which virtual Xbox pad the host builds. `hid` is seen by Steam, SDL, DirectInput and Windows.Gaming.Input, and by XInput through Windows' `xinputhid` filter. `xusb` is an Xbox 360 pad for XInput and Windows.Gaming.Input. Unset, the host picks `hid` where `xinputhid` is installed and `xusb` where it is not, such as Windows Server. |
| `PUNKTFUNK_PAD_AUDIO_SLOTS` | `1`–`4` (Windows `1`, Linux `4`) | How many controllers get their own [speaker and haptics](/docs/controller-audio) audio at once. |
| `PUNKTFUNK_PAD_SINK_NAME` | template | Linux, debugging: `node.name` of each pad's mono speaker node; `{pad}` and `{mac}` expand. Each pad gets a real DualSense's three nodes: mono speaker, 4-channel speaker-haptic, and a hidden 4-channel parent. |
| `PUNKTFUNK_PAD_SINK_DESC` | template | Linux, debugging: every pad node's `node.description` (default `Wireless Controller`). |
| `PUNKTFUNK_PAD_SINK_SPLIT_NAME` | node name · `0` | Linux, debugging: the `api.alsa.split.name` GE-Proton opens for haptics (default the hidden parent, `alsa_output.hw_punktfunkpad<N>_0`). `0` drops it, which sends GE-Proton down its Pulse route. |
| `PUNKTFUNK_PAD_SINK_PARENT_CLASS` | media class (default `Audio/Sink/Internal`) | Linux, debugging: the hidden parent's `media.class`, for a session manager that refuses `Internal` nodes. |

## Audio / microphone

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_AUDIO_GAIN` | number (default `1.0`, max `8.0`) | Boosts captured desktop audio on both protocols; a soft limiter rounds off peaks. The only host-side level on Windows, where capture ignores the speaker volume. |
| `PUNKTFUNK_STREAM_SINK` | unset · `stream` · `0` | Linux: where desktop audio is captured. Unset, the host creates **Punktfunk Stream Speaker**, makes it the default while streaming and records it. `stream` uses a capture stream in its place; `0` records your current default output. |
| `PUNKTFUNK_MIC_DEVICE` | name substring | Windows: the device the client's microphone is routed to. |
| `PUNKTFUNK_MIC_LEGACY_BUFFER` | `1` | Fixed microphone buffering instead of the adaptive one. If the mic only sounds right with this set, report it. |
| `PUNKTFUNK_NO_MIC_INSTALL` | set | Windows: don't install the virtual microphone. |
| `PUNKTFUNK_NO_AUDIO_MINT` | set | Windows: don't create the host's own audio endpoints from Steam's streaming-audio driver; pick devices by name instead. |

## Windows host

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_MONITOR_LINGER_MS` | ms (default `10000`) | Keep a client's virtual display this long after it disconnects. **Keep alive** in [Virtual displays](/docs/virtual-displays) supersedes it. |
| `PUNKTFUNK_EXCLUSIVE_REASSERT_MS` | ms (default `2000`), `0` off | How often the host checks that exclusive topology held, re-applying it when Windows or a driver turns a physical monitor back on. |
| `PUNKTFUNK_STANDBY_SINK_KEEP` | any value but `0`/`off` | Keep a connected but inactive external display (a TV on standby, a capture card) powered while streaming; by default the host turns it off, because Windows keeps drawing for it. Also keeps enabled the monitors **Disable monitor devices (PnP)** would disable. |
| `PUNKTFUNK_NO_ISOLATE` | set | Extend the desktop onto the virtual display instead of making it the only one. **Topology** in [Virtual displays](/docs/virtual-displays) supersedes it. |
| `PUNKTFUNK_HOST_CMD` | `serve` | The command the service runs; every install writes `serve`. With no line the service runs `serve --gamestream`, which `punktfunk-host service install` rewrites to `serve` with **GameStream** kept on. |
| `PUNKTFUNK_WEB_CONSOLE` | `off` | Don't run the web console alongside the service. |

## Network & discovery

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_NATIVE_PORT` | port (default `9777`) | The native QUIC port, as `serve --native-port`. Clients learn it from discovery. A value that isn't a port stops the host at startup. |
| `PUNKTFUNK_DATA_PORT` | port | Pins video to one UDP port, for a firewall, a forward or a tunnel; default a random port per session. Same as `serve --data-port`. |
| `PUNKTFUNK_DSCP` | `1` · `0` | QoS marking on media packets. Default: on toward private-network clients, off toward internet addresses. `1` always, `0` never. |
| `PUNKTFUNK_JUMBO` | `1` | Jumbo frames (about 9000-byte packets) on a wired LAN — see below. |
| `PUNKTFUNK_WIRE_MTU` | on-wire MTU, e.g. `9000` or `1400` | Above 1500: jumbo frames at that size. Below 1500: smaller packets from the start, for a VPN or tunnel. Use the MTU your network adapter reports. |

Jumbo frames need every NIC and switch on the path set to MTU `9000` first. The host probes the path
and switches mid-session only once the client confirms; a path that can't carry them stays at
standard size. Native protocol only.

## Auth, API & paths

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_MGMT_TOKEN` | token | Normally unset: the host generates the management token into `~/.config/punktfunk/mgmt-token`. A value set here is saved to that file and removed from the host's environment. |
| `PUNKTFUNK_PLUGIN_TOKEN` | token | The plugin runner's narrower token, handled the same way (`~/.config/punktfunk/plugin-token`). |
| `PUNKTFUNK_UI_PASSWORD_HASH` | argon2id hash | What a console sign-in is checked against; the console writes it to `~/.config/punktfunk/web-password`. See [Forgot your Password?](/docs/forgot-password). |
| `PUNKTFUNK_UI_PASSWORD` | password | A console password in clear text; the next sign-in replaces it with the hash. This is how you reset it. |
| `PUNKTFUNK_MGMT_BIND` | `IP:PORT` (default `0.0.0.0:47990`) | Where the management API listens; `--mgmt-bind` overrides it. `127.0.0.1:47990` keeps it off the LAN (paired clients then can't browse your library); another port lets it share the machine with Sunshine or Apollo. The console, plugin runner and tray follow the port through the `mgmt-endpoint` file. See [another streaming host is installed](/docs/troubleshooting-connect#another-streaming-host-sunshine-apollo--is-installed). |
| `PUNKTFUNK_UI_BIND` | address (default `0.0.0.0`) | Where the web console listens: `0.0.0.0` for your network, `127.0.0.1` for this machine, or one address such as a VPN interface. It never answers the internet. The installers ask; on NixOS use `services.punktfunk.web.bind`. |
| `PUNKTFUNK_UI_PLUGIN_PORT` | port (default console port + 1) | The separate origin plugin pages load from. If the console log says it couldn't open, point it at a free port. |
| `PUNKTFUNK_CONFIG_DIR` | path | The config directory (default `~/.config/punktfunk`, `%ProgramData%\punktfunk` on Windows): pairing state, certificates, `apps.json`. The desktop clients read the same name for their own files — see [Client-side](#client-side-native-clients). |
| `PUNKTFUNK_PLUGIN_SANDBOX` | `off` | Runs [plugins](/docs/plugins) without their sandboxes, with your account's access — only for a kernel that refuses unprivileged user namespaces. The runner doesn't read `host.env`: set it with `systemctl --user edit punktfunk-scripting` as `Environment=PUNKTFUNK_PLUGIN_SANDBOX=off`. |
| `PUNKTFUNK_LIBRARY_ART_ROOTS` | directories, separated like `PATH` | Where the host may read local game art. Replaces the defaults (your home plus the system icon and Flatpak directories on Linux; `C:\Users` and your Steam and Playnite installs on Windows), so list every root. The host logs `dropped local art the proxy may not serve` when one is missing. |
| `PUNKTFUNK_LIBRARY_ART_CACHE` | path | Where downloaded covers are kept (default under `~/.cache` or `%LOCALAPPDATA%`). `punktfunk-host library art --clear` empties it. |
| `PUNKTFUNK_LIBRARY_ART_CACHE_MB` | MB (default `512`) | Size cap on that cache; the oldest covers go first. |

## Advanced performance tuning

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_FRAME_DRIVEN` | `1` · `0` | Encode when a frame arrives instead of on a fixed tick, on by default. `0` restores the tick, which adds about half a frame of latency. |
| `PUNKTFUNK_GSO` | `1` · `0` | UDP segmentation offload: less send CPU, but bursty on constrained links. On by default on Windows (needed past about 1 Gbps), off on Linux. |
| `PUNKTFUNK_SPLIT_ENCODE` | `0` · `1` · `2` · `3` | NVENC split encode for very high pixel rates. Unset splits on its own from about 4K120; `1` forces a split, `2` and `3` force two or three ways, `0` never splits. |
| `PUNKTFUNK_NVENC_SUBFRAME` | `0` · `1` | NVENC sub-frame readback for lower latency, on where the GPU supports it. `0` never, `1` always. |
| `PUNKTFUNK_NVENC_SPLIT_ARBITRATE` | `1` | Lets NVENC change its split decision mid-session as the pixel rate moves. |
| `PUNKTFUNK_PHASE_LOCK` | `0` | Stops timing frame submission to the client's display. Try it if frame pacing keeps cycling. |
| `PUNKTFUNK_GPU_PRIORITY_CLASS` | `realtime` (default) · `high` · `normal` · `off` | Windows: GPU priority of capture and encode under a heavy game; `realtime` preempts the game at some cost to its frame rate. Try `high` if NVENC freezes with HAGS on and VRAM nearly full. The display driver's own raise turns off with `setx /M PFVD_NO_RT_GPU 1` and a device restart. |
| `PUNKTFUNK_IDD_ADAPTIVE` | `1` · `0` | Windows: adapt the encode pipeline depth to overruns, on by default. `0` pins the full depth and turns off the encode-cadence detector. |
| `PYROWAVE_QUEUE_PRIORITY` | `realtime` (default) · `high` · `off` | [PyroWave](/docs/pyrowave): ask the GPU to run the encode ahead of the game. On Linux an NVIDIA GPU defaults to `high`, because its `realtime` queue slows every submit. On Linux only `punktfunk-encode-worker` may raise it, never the host — see [GPU scheduling priority](/docs/running-as-a-service#gpu-scheduling-priority). `off` if the desktop stutters while streaming. |
| `PUNKTFUNK_ENCODE_WORKER` | path · `off` | Where the host finds `punktfunk-encode-worker` (default beside the host, then `PATH`; the NixOS module sets it). `off` encodes PyroWave in the host process at normal priority. |
| `PUNKTFUNK_SCRIPTING` | path | Where the host finds the `punktfunk-scripting` plugin runner. Set it when `punktfunk-host plugins add` works but the console says the runner isn't installed — the service's `PATH` is shorter than your shell's. |

## Diagnostics

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_PERF` | `1` | Log per-stage timing (capture, encode, send). |
| `RUST_LOG` | `info` · `debug` · `trace` | Log detail. Windows logs go to `%ProgramData%\punktfunk\logs\`. |
| `PUNKTFUNK_VIDEO_DROP` | `1`–`90` (percent) | Drop that share of video packets to test error correction. Testing only. |

The host also reads debugging variables not listed here; they change between releases.

## Client-side (native clients)

Read by the Linux and Windows clients, the Decky plugin and the `punktfunk` CLI — not the host. `PUNKTFUNK_AUDIO_HIRES` and `PUNKTFUNK_CONFIG_DIR` are also read by the host, each for its own files.

| Variable | Values | What it does |
|---|---|---|
| `PUNKTFUNK_DECODER` | `native-vulkan` · `native-vaapi` (Linux) · `native-d3d11va` (Windows) · `software` | Pins the decoder; the order it replaces is in the [Support matrix](/docs/support-matrix#client-decode). The older `vulkan`, `vaapi` and `d3d11va` still work. |
| `PUNKTFUNK_VAAPI_DEVICE` | path, e.g. `/dev/dri/renderD129` | Linux: the render node VAAPI decodes on. Unset: the first node where VAAPI starts, the presenting GPU's vendor first. |
| `PUNKTFUNK_VK_ADAPTER` | name substring | The GPU the client presents on. Unset prefers a discrete GPU. |
| `PUNKTFUNK_PREFER_PYROWAVE` | `1` | Ask for [PyroWave](/docs/pyrowave) where the client's own setting isn't reachable, such as a headless launch. |
| `PUNKTFUNK_PAD_SPEAKER_PATH` · `PUNKTFUNK_PAD_SPEAKER_VOLUME` | byte (default `0x20` / `0x7F`) | Which DualSense output [controller audio](/docs/controller-audio) plays to, and how loud. Change them only if the pad's speaker stays silent. |
| `PUNKTFUNK_PAD_AUDIO_PROFILE` | `0` | Linux: don't switch a wired DualSense's sound card to **Pro Audio** while streaming controller audio. Without it the voice coils fold into the speaker pair. |
| `PUNKTFUNK_OSD_SCALE` | multiplier (default `1`) | Size of the in-stream overlay on top of display scaling, from 0.5 to 4. |
| `PUNKTFUNK_CONFIG_DIR` | path | Where this client keeps its identity, saved hosts and settings (default `~/.config/punktfunk`, `%APPDATA%\punktfunk` on Windows). Empty is ignored. Moving it moves that identity, so paired hosts need pairing again. The Flatpak only sees `~/.config/punktfunk`. The private key is readable only by the account that created it. |
| `PUNKTFUNK_AUDIO_HIRES` | `1` · `48000` · `96000` · `<rate>/<bits>` · `0` | Asks for lossless audio, over the client's audio-format setting: `1` is 96 kHz/24-bit, a bare rate is 24-bit, `48000/16` is the cheapest. `0` or empty forces Opus; anything else is ignored. The host's **Lossless audio** reads the same name as on/off. |
| `PUNKTFUNK_NO_AEC` | `1` | Turn microphone echo cancellation off for this run. |
| `PUNKTFUNK_PRESENT_MODE` | `mailbox` · `fifo` · `immediate` · `fifo_relaxed` | Vulkan present mode. With V-sync on the default is `mailbox`, else `fifo` (AMD's Windows driver has no mailbox); with V-sync off `immediate` comes first. |
| `PUNKTFUNK_PRESENTER` | `arrival` | Show frames the moment they decode, bypassing frame pacing. A diagnostic. |
| `PUNKTFUNK_VRR_FIFO` | `1` | Follow a variable-refresh screen on a driver too old for the modern mode; costs latency on a fixed-refresh screen. The Detailed [stats overlay](/docs/stats) shows `vrr yes` when it works. |
| `PUNKTFUNK_PRESENT_DEBUG` | `1` | Log the presenter's summary every second. |
| `PUNKTFUNK_ABR_PROBE_KBPS` | kbps | Upper limit for the startup link measurement. |
| `PUNKTFUNK_ABR_PROBE` | `0` | Skip the startup link measurement; Automatic then opens at the starting rate and climbs. |
| `PUNKTFUNK_ABR_MAX_MBPS` | Mbps | Cap on Automatic's ceiling, for a client whose decoder can't keep up with what the link carries. |

## Bitrate

The client picks the bitrate; there is no host setting. On the native protocol it is the whole wire
budget — video, error correction and audio fit inside it. Native clients default to **Automatic**,
which measures the link; their speed test suggests a fixed rate. In Moonlight, set it in Moonlight.

## Multiple devices at once

`serve` streams up to 4 sessions at once, each on its own virtual display at its own mode; further
clients wait until a slot frees. The limit isn't configurable.

## Settings documented elsewhere

- **Virtual-display policy** — keep-alive, topology, scaling: [Virtual displays](/docs/virtual-displays).
- **Which GPU** on a multi-GPU box: **Host → GPUs**. A pick there outranks `PUNKTFUNK_RENDER_NODE`
  and `PUNKTFUNK_RENDER_ADAPTER`.
- **Event hooks and webhooks** — `hooks.json`: [Events & hooks](/docs/automation).
- **Browser streaming** ports: [The browser client](/docs/running-as-a-service#the-browser-client-preview).
- **Encoder prerequisites** — drivers and firmware: [Requirements](/docs/requirements).
- **Client settings** — resolution, bitrate, codec, HDR: [Client settings](/docs/client-settings).
