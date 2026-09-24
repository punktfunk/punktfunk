---
title: Host CLI
description: Every punktfunk-host command and flag you would run by hand, plus where the client's punktfunk command is documented.
---

Look up a `punktfunk-host` command or flag here. Most behaviour comes from
[`host.env`](/docs/configuration), not the command line; a flag wins over its `host.env` twin.

| Command | What it does | Platform |
|---|---|---|
| [`serve`](#serve) | Run the host. | all |
| [`ctl`](#ctl) | Drive a running host: pairing, devices, sessions, displays, stats, events. | all |
| [`plugins`](#plugins) | Install, remove and list plugins, grant them folders, switch the runner on. | all |
| [`library`](#library) | Print the resolved [game library](/docs/game-library) as JSON. | all |
| [`detect-conflicts`](#detect-conflicts) | Report other Moonlight-compatible hosts on this machine. | all |
| `openapi` | Print the management API's OpenAPI document. | all |
| [`punktfunk1-host`](#punktfunk1-host) | Standalone native-only test host. | all |
| `--version` | Print the host version. | all |
| [`list-monitors`](#list-monitors) | List the physical monitors by connector name. | Linux |
| [`mirror-test`](#list-monitors) | Capture one of them with no client, to prove the path works. | Linux |
| [`anchor-test`](#list-monitors) | Move the pointer across one of them, to check where absolute input lands. | Linux |
| [`hdr-probe`](#hdr-probe-and-probe-compositor) | Say whether this box can stream 10-bit HDR, and what is missing. | Linux |
| [`probe-compositor`](#hdr-probe-and-probe-compositor) | Exit 0 once the compositor can create a virtual output. | Linux |
| [`hdr-p010-selftest`](#hdr-probe-and-probe-compositor) | Check the GPU's HDR capture colour conversion. | Windows |
| [`service`](#service-windows) | Register, control and remove the Windows service. | Windows |
| [`tray`](#tray-windows) | Start, stop or query the status tray. | Windows |
| [`driver`](#driver-windows) | Check or remove the bundled drivers. | Windows |
| [`web`](#web-windows) | Print the web-console login password. | Windows |

`punktfunk-host --help` prints the common commands. `ctl`, `plugins`, `service`, `tray`, `driver`
and `web` print their usage when run with no arguments.

## `serve`

Runs the native `punktfunk/1` host and the management API in one process. The native plane always
runs.

```sh
punktfunk-host serve
```

| Flag | `host.env` twin | Meaning |
|---|---|---|
| `--gamestream` / `--moonlight` | `PUNKTFUNK_GAMESTREAM=1` | Also serve stock [Moonlight](/docs/moonlight) clients. Trusted LAN only: GameStream pairs over plain HTTP. |
| `--native-port <PORT>` | `PUNKTFUNK_NATIVE_PORT` | Native QUIC port (default `9777`). |
| `--mgmt-bind <IP:PORT>` | `PUNKTFUNK_MGMT_BIND` | Management API address (default `0.0.0.0:47990`). `127.0.0.1:47990` keeps it off the LAN, and paired clients can't browse your library. |
| `--data-port <PORT>` | `PUNKTFUNK_DATA_PORT` | Pin the video data plane to one UDP port, to open or forward. Default: a fresh port per session. |
| `--webtransport` | `PUNKTFUNK_WEBTRANSPORT=1` | Also accept the [browser client](/docs/running-as-a-service#the-browser-client-preview) (preview). |
| `--webtransport-port <PORT>` | `PUNKTFUNK_WEBTRANSPORT_PORT` | Its UDP port (default `9778`). |
| `--webtransport-bind <IP>` | `PUNKTFUNK_WEBTRANSPORT_BIND` | Its interface (default: all). |
| `--open` | — | Serve unpaired devices. Trusted single-user setups only. With the browser client on, it also needs `PUNKTFUNK_WEBTRANSPORT_ORIGINS`, or the host refuses to start. |
| `--no-mdns` | `PUNKTFUNK_MDNS=0` | Skip the mDNS adverts; add the host on the client by address. |
| `--native` | — | No-op. |

Pairing is required unless `--open`: arm it from the web console — see
[Pairing & Trust](/docs/pairing).

The management API is HTTPS and listens on every interface by default. Off loopback it serves
paired clients the read-only status and library. Every admin action needs the token in
`mgmt-token` and works from loopback only. The host writes that file on first start into its
config directory (`~/.config/punktfunk`, or `%ProgramData%\punktfunk` on Windows); the console,
`ctl` and the tray read it. Every endpoint is in the [API Reference](/api).

## `ctl`

Drives a running host over loopback: everything the [web console](/docs/web-console) does day to
day, from a terminal.

```sh
punktfunk-host ctl pending
punktfunk-host ctl approve 3
```

| Verb | What it does |
|---|---|
| `status` | Host state, live session count, paired-device counts. |
| `sessions` | The active sessions and any launched game. |
| `summary` | Host version, what is streaming, the streaming client's name, paired counts, conflicting hosts. The only verb that names a connected device. |
| `pair status` | Whether a pairing window is open and a PIN is waiting. |
| `pair arm` | Open a native pairing window and print the PIN. `--ttl <s>` window length, `--expires-in <s>` how long the device's access lasts, `--preset <full\|controller\|view>`, `--fingerprint <fp>` binds the window to one device. |
| `pair disarm` | Close the window. |
| `pending` | Devices waiting for approval, with claimed name and fingerprint tail. |
| `approve <ID>` | Admit one. Takes `--name`, `--preset`, `--expires-in`. |
| `deny <ID>` | Refuse one. |
| `pin <PIN> <UNIQUEID> <FP> <PEER_IP>` | Submit a Moonlight client's PIN, with the other three values exactly as `ctl pair status` shows them. |
| `clients` | Paired devices on both planes. |
| `rename <FP> <NAME>` | Name a device. |
| `access <FP> <PRESET>` | `full`, `controller` or `view` — see [Access levels](/docs/access-levels). |
| `unpair <FP>` | Remove one device. `unpair --all` removes every device on both planes; it asks first, or takes `--yes`. |
| `stop-session` | Stop the active session. |
| `end-game` | End the launched game. |
| `display` | The virtual-display policy, every preset, and the live and kept displays. |
| `display preset <ID>` | Switch to a preset. The streamed screen and other settings a preset doesn't own stay. |
| `display release [SLOT]` | Tear down kept displays now; omit `SLOT` for all. Never touches a streaming display. |
| `stats` | Mode, codec and bitrate of the live stream; frame timings and drops while a capture records. |
| `stats record start\|stop` | Arm or stop the performance capture. `stop` writes it to disk. The console shares the one slot. |
| `watch` | Host events as line-JSON. `--kinds pairing.pending,stream.*` filters; `--since <seq>` resumes. |
| `console-url` | Print a `file://` link that opens the console already logged in. Linux and macOS. |

In `stats`, **Target** is the encoder's bitrate; **Sent** is what left the box and exists only while
a capture records. A still screen sends little, so read **new** fps beside **repeated** fps.

Add `--json` to any verb: `{"v":1,"data":…}` on success, `{"v":1,"error":{"code":…,"message":…}}`
on failure, both on stdout. That envelope is stable; the human tables are not.

| Exit code | Meaning |
|---|---|
| `0` | Success. |
| `1` | The host refused the request; the message says why. |
| `2` | Usage error. |
| `3` | No host reachable, or it never ran on this machine. |
| `4` | Certificate pin mismatch — something else answers on the management port. Don't retry. |

`watch` reconnects by itself and starts at the live tail (`--since 0` replays the host's ring). It
adds two lines of its own: `{"kind":"ctl.resync"}` means it fell behind, so re-read `ctl status`
and `ctl pending`; `{"kind":"ctl.disconnected"}` means a reconnect is under way.

`ctl` reads the host's `mgmt-token` and pins its `native-cert.pem` before sending the token. It
never creates a token and takes none on the command line or from the environment.

## `plugins`

`punktfunk-host plugins add|remove|list|enable|disable|status|grant|access|revoke` does what
the console's **Plugins** page does. Plugins run with the host's privileges; read [Plugins](/docs/plugins)
first.

## `library`

`punktfunk-host library` prints the game library as JSON, the same as `GET /api/v1/library` — the
quick answer to "does the host see my games?". `punktfunk-host library art --clear` deletes the
covers the host cached for clients.

## `punktfunk1-host`

A standalone native-only host for testing the `punktfunk/1` path, with no GameStream and no console.
Pairing is required; it logs a PIN at startup.

```sh
punktfunk-host punktfunk1-host --source virtual
```

| Flag | Meaning |
|---|---|
| `--port <N>` | QUIC port (default `9777`). |
| `--source <SRC>` | `synthetic` (default, test frames), `synthetic-abr` (frames sized from the live bitrate; no display or GPU needed) or `virtual` (a real virtual display). |
| `--content <SCRIPT>` | What `synthetic-abr` encodes: `steady` (default), `idle-then-motion`, or `frame-driven:<fps>`. |
| `--fill <PCT>` | Share of each frame's bit budget `synthetic-abr` fills, 1–100 (default 100). |
| `--recovery-ms <MS>` | How long `synthetic-abr` takes to answer a keyframe request (default 0). |
| `--keyframe-answer <KIND>` | `idr` (default) or `wave:<n>`: answer only every n-th keyframe request with one. |
| `--idr-pct <PCT>` | A keyframe's size as a percent of an ordinary frame (default 1000). |
| `--bringup-ms <MS>` | How long `synthetic-abr` holds its first frame back (default 2500). |
| `--no-ramp` | Don't offer the client a link measurement before the first frame. |
| `--seconds <N>` | Session length for `virtual` and `synthetic-abr` (default 30). |
| `--frames <N>` | Session length for `synthetic` (default 300). |
| `--max-concurrent <N>` | Sessions streaming at once (default 4, `0` = unlimited); the rest queue. |
| `--max-sessions <N>` | Exit after N sessions (default `0`, serve forever). |
| `--allow-tofu` | Also accept unpaired clients. Trusted LANs only. |
| `--pairing-pin <PIN>` | Fixed pairing PIN, for test harnesses. A guessable PIN defeats the rate limit. |
| `--data-port <PORT>` | Pin the data plane to one UDP port. Fits one session; concurrent ones get a random port. |
| `--idle-timeout-ms <MS>` | How fast a dead client is detected (QUIC idle timeout, default 8000). |
| `--no-mdns` | Skip the mDNS advert. |

## `service` (Windows)

The installer runs `service install`; you need these only to change an option or nudge the
service. Run them from an **Administrator** prompt.

```powershell
punktfunk-host service install [--gamestream=on|off] [--allow-public-network[=on|off]]
                               [--mgmt-bind=IP:PORT] [--web-bind=ADDR]
punktfunk-host service start | stop | restart | status | uninstall
```

| Subcommand or option | What it does |
|---|---|
| `install` | Registers the auto-start `PunktfunkHost` service, adds the firewall rules, and writes `%ProgramData%\punktfunk\host.env` if missing. Re-run it to change an option. |
| `--gamestream=on\|off` | The GameStream setting, as in the console. |
| `--allow-public-network[=on\|off]` | Also open the ports on networks Windows calls **Public**. Default: Private and Domain only; left out, the previous choice stays. |
| `--mgmt-bind=IP:PORT` | Sets `PUNKTFUNK_MGMT_BIND` and opens that port. |
| `--web-bind=ADDR` | Sets `PUNKTFUNK_UI_BIND`, the console's address: `127.0.0.1`, `0.0.0.0`, or one of yours. |
| `uninstall` | Stops and deletes the service and its firewall rules. The host stays — see [Uninstalling](/docs/uninstall). |
| `start` / `stop` / `restart` | Service control. `restart` waits for the old process, so it picks up a `host.env` edit. |
| `status` | Same as `sc query PunktfunkHost`. |

See [Running as a Service](/docs/running-as-a-service) and [Windows Host](/docs/windows-host).

## `tray` (Windows)

`punktfunk-host tray start|stop|status` brings the per-user status tray back after an update or a
crash, without signing out. `status` says whether it is installed and running.

## `driver` (Windows)

The installer manages the drivers. These remove one without uninstalling the host:

```powershell
punktfunk-host driver check                # does the virtual display driver answer?
punktfunk-host driver uninstall            # the virtual display driver
punktfunk-host driver uninstall --gamepad  # the virtual gamepad driver
punktfunk-host driver uninstall --audio    # the host's virtual speaker and microphone devices
```

## `web` (Windows)

`punktfunk-host web password` prints the [web console](/docs/web-console) login password. Run it
elevated: the file is readable by Administrators and SYSTEM only. A silent install (winget,
`/VERYSILENT`) shows the password nowhere else.

## `list-monitors`

Prints the physical monitors by connector name, the name `PUNKTFUNK_CAPTURE_MONITOR` takes to
[stream a real monitor](/docs/virtual-displays#stream-a-real-monitor-instead). Run it inside the
session you want to stream.

```
$ punktfunk-host list-monitors
Kwin:
  HDMI-A-1        1920x1080@60 at +0,+0    scale 1  Dell U2412M  [primary]
  DP-2            2560x1440@144 at +1920,+0  scale 1  ACME 27  [PINNED]
```

Tags: `primary`, `disabled`, `punktfunk virtual display` (one of the host's own), `PINNED` (the
current pick).

`punktfunk-host mirror-test --monitor <CONNECTOR> [--seconds N] [--cpu]` captures that monitor with
no client and reports the first frame, the frame count and the size. Move the mouse while it runs:
a still screen produces almost no frames.

`punktfunk-host anchor-test --monitor <CONNECTOR> [--width W --height H]` walks the pointer to the
centre and four corners of that monitor, one second apart; `--none` walks it unanchored for
comparison. The log line `libei: absolute input maps into this output` names the monitor it hit.
It needs the libei input backend (GNOME, or `PUNKTFUNK_INPUT_BACKEND=libei`).

## `hdr-probe` and `probe-compositor`

Linux checks that need no client.

```sh
punktfunk-host hdr-probe
punktfunk-host probe-compositor
```

`hdr-probe` answers "why isn't my stream HDR?": a monitor in HDR mode (GNOME), a
`punktfunk-gamescope` with HDR on, 10-bit HEVC and AV1 encode, and the verdict for each plane. Run
it with the host service's environment: [HDR → Check it](/docs/hdr#check-it) has the command
and reads the output.

`probe-compositor` exits 0 once the compositor is ready, so a bring-up script can wait on it
instead of a `sleep`. It checks KWin's screencast grant and Hyprland's `hyprctl`; other compositors
pass once detected.

On Windows, `hdr-p010-selftest` checks the GPU's HDR capture conversion with no display or session,
and prints PASS or FAIL with the largest error:

```powershell
punktfunk-host hdr-p010-selftest 1920x1080 nvidia
```

Both arguments are optional: your capture size (default `64x64`; 1080 takes a different driver
path) and, on a dual-GPU box, the encoding vendor (`intel`, `nvidia` or `amd`).

## `detect-conflicts`

Lists Sunshine, Apollo and other Moonlight-compatible hosts installed on this machine. It exits
**1** when one runs or starts on its own, **0** otherwise, so scripts can gate on it. Running
one beside Punktfunk is unsupported: they fight over ports and the virtual display. `serve` runs
the same check at startup. See
[Troubleshooting](/docs/troubleshooting-connect#another-streaming-host-sunshine-apollo--is-installed).

## The client's `punktfunk` command

The machine you stream *to* has its own command, `punktfunk`. Its verbs and exit codes are on
[Clients → the `punktfunk` CLI](/docs/clients#scripting-the-punktfunk-cli); `punktfunk wake` is on
[Wake on LAN](/docs/wake-on-lan#from-the-command-line).
