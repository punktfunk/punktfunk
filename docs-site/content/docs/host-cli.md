---
title: Host CLI
description: The punktfunk-host commands and the flags you'll actually use — plus punktfunk, the command on the client machine.
---

The host is one binary, `punktfunk-host`. Most of the time you'll run a single command; the rest reads
its settings from [`host.env`](/docs/configuration). On the machine you stream *to*, there's a second
command — [`punktfunk`](#punktfunk-on-the-client-machine), which ships with the client.

| Command | What it does | Platform |
|---|---|---|
| [`serve`](#serve) | Run the host. | all |
| [`ctl`](#ctl) | Drive a running host: pairing, devices, sessions, events. | all |
| [`punktfunk1-host`](#punktfunk1-host) | Standalone native-only test host. | all |
| [`service`](#service-windows) | Register, start, stop and remove the Windows service. | Windows |
| [`tray`](#tray-windows) | Start, stop or query the status-tray icon. | Windows |
| [`driver`](#driver-windows) | Install or remove the bundled virtual-display / virtual-gamepad drivers. | Windows |
| [`web`](#web-windows) | Print the web-console login password. | Windows |
| [`plugins`](#plugins) | Install, remove and list plugins, and switch the runner on. | all |
| [`list-monitors`](#list-monitors) | List the physical monitors, by connector name. | Linux |
| `mirror-test` | Prove capture works from one of them — see [`list-monitors`](#list-monitors). | Linux |
| [`hdr-probe`](#hdr-probe-and-probe-compositor) | Report whether this box can deliver a 10-bit HDR stream, and what's missing. | Linux |
| [`hdr-p010-selftest`](#hdr-probe-and-probe-compositor) | Check the GPU's HDR capture colour conversion, with no display or session. | Windows |
| [`probe-compositor`](#hdr-probe-and-probe-compositor) | Exit 0 when the compositor is up and can create a virtual output. | Linux |
| [`detect-conflicts`](#detect-conflicts) | Report other Moonlight-compatible hosts on this machine. | all |
| `library` | Print [the resolved game library](/docs/game-library) as JSON — "does the host see my games?". | all |
| `openapi` | Print the management API's OpenAPI document. | all |
| `--version` | Print the host version. | all |

`punktfunk-host --help` prints the most-used of these. `plugins`, `service`, `driver` and `tray`
print their own usage when you run them with no arguments.

## `serve`

The normal way to run a host. By default `serve` starts the **secure native host**: the native
`punktfunk/1` server (QUIC, SPAKE2 PIN pairing, per-direction AEAD) plus the management API/web
console — all in one process. The native plane is **always on**; there is no flag to turn it off.

```sh
punktfunk-host serve
```

Add `--gamestream` (alias `--moonlight`) to **also** run the GameStream/Moonlight-compatible planes
(nvhttp pairing, RTSP, ENet control, `_nvstream` mDNS) — required for stock [Moonlight](/docs/moonlight)
clients. This is **opt-in** because GameStream carries inherent on-path weaknesses (pairing over plain
HTTP; its legacy control encryption can reuse GCM nonces), so enable it **only on a trusted LAN**. The
native plane is immune to those issues.

```sh
punktfunk-host serve --gamestream
```

| Flag | Meaning |
|---|---|
| `--gamestream` / `--moonlight` | Also run the GameStream/Moonlight-compat planes (for stock Moonlight clients). Opt-in, trusted-LAN only — see above. |
| `--native` | No-op. The native `punktfunk/1` server always runs in `serve`; kept only for backward compatibility. |
| `--native-port <PORT>` | Native QUIC port (default `9777`). |
| `--open` | Don't require pairing — serve any device on the network. Off by default; only for trusted single-user setups. |
| `--mgmt-bind <IP:PORT>` | Management API address (default `0.0.0.0:47990` — all interfaces, so paired clients can browse the game library over mTLS; pass `127.0.0.1:47990` to keep it loopback-only). |
| `--no-mdns` | Skip the mDNS adverts (native + GameStream) — for networks/containers where multicast doesn't work. Clients connect via a manually added host instead. Same as `PUNKTFUNK_MDNS=0`. |
| `--data-port <PORT>` | Pin the per-session video data plane to this fixed UDP port — one number to open in a firewall, forward on a router or share through a port proxy. Video still follows the client's hole-punch, so a NAT on the client's side that remaps ports works. Same as `PUNKTFUNK_DATA_PORT`; default is a fresh random port per session. |

These are the only flags `serve` accepts.

The management API is **always HTTPS**. It binds all interfaces by default so a **paired client** can
fetch the game library over its mTLS certificate — but off loopback that certificate reaches only the
read-only status + library endpoints. The **admin surface** (arming pairing, removing devices, session
control, library edits) needs a **bearer token** and is honored **from loopback only**. The token is
generated on first start and persisted to `~/.config/punktfunk/mgmt-token`, owner-only; the web
console, `ctl` and the tray read that same file. There is no flag for it, and the host drops
`PUNKTFUNK_MGMT_TOKEN` from its own environment at startup (persisting a value set there first), so
the games and hooks it launches cannot inherit it. Pass `--mgmt-bind 127.0.0.1:47990` to keep 47990
loopback-only. Every endpoint is in the interactive [**API Reference**](/api).

By default the host **requires pairing** — see [Pairing & Trust](/docs/pairing). On `serve` you
**arm pairing from the web console**; the host then displays a 4-digit PIN. `--open` serves any
device on the network (trusted single-user setups only). `punktfunk1-host` requires pairing too;
its `--allow-tofu` flag is the test-host equivalent of `--open`.

## `ctl`

Drive a **running** host from a terminal: approve a device, type a Moonlight PIN, rename or unpair,
stop a session, watch events. Everything the [web console](/docs/web-console) does day to day,
without a browser — and everything it does is the same management API the console talks to, over
loopback.

```sh
punktfunk-host ctl status
punktfunk-host ctl pending
punktfunk-host ctl approve 3
```

| Verb | What it does |
|---|---|
| `status` | Host state, live session count, paired-device counts. |
| `sessions` | The active session(s) and any launched game. |
| `summary` | One call for a status surface: host version, what is streaming, the streaming client's name, paired counts, and any conflicting host on this box. The **only** verb that names a connected device — `status` exposes no device names by design. |
| `pair status` | Is a pairing window open, and is a PIN waiting? |
| `pair arm` | Open a native pairing window and print the PIN. `--ttl <s>` how long the window stays open, `--expires-in <s>` how long the device's access lasts, `--preset <full\|controller\|view>`, `--fingerprint <fp>` to bind the window to **one** device. |
| `pair disarm` | Close it. |
| `pending` | Devices knocking, with their claimed name and fingerprint tail. |
| `approve <ID>` | Admit one, by id. `--name`, `--preset`, `--expires-in` as above. |
| `deny <ID>` | Refuse one. |
| `pin <PIN>` | Submit the PIN a Moonlight/GameStream client is showing. |
| `clients` | Paired devices on both planes, labelled. |
| `rename <FP> <NAME>` | Name a device. |
| `access <FP> <PRESET>` | `full`, `controller` or `view` — see [Access levels](/docs/access-levels). The full grant matrix stays in the console. |
| `unpair <FP>` | Remove one device. `unpair --all` removes every device on both planes (asks first; needs `--yes` with `--json`). |
| `stop-session` | Stop the active session. |
| `end-game` | End the launched game. |
| `display` | The virtual-display policy, every preset (built-in and saved), and the live displays. |
| `display preset <ID>` | Switch the policy to a preset. Reads the stored policy and edits it, so the axes a preset does not own — the streamed screen, the Windows monitor levers — survive the switch. |
| `display release [SLOT]` | Tear down **kept** displays now, so a physical-screen user gets their screen back without waiting out the linger. Omit `SLOT` for all. Never touches a display that is actively streaming. |
| `stats` | The live stream: mode, codec and the adaptive bitrate, plus frame timings and drops while a capture is recording. |
| `stats record start\|stop` | Arm or disarm the performance capture. `stop` writes the recording to disk. The capture is one host-wide slot the web console shares. |
| `watch` | Stream host events as line-JSON on stdout, one object per line. `--kinds stream.*,pairing.pending` filters; `--since <seq>` resumes. |

**`display` lists live and lingered heads on Hyprland.** A reconnect inside keep-alive recasts
the same named output, so the list is the truth. **Sway still reports an empty `displays` list** —
its capture arrives over a sandboxed portal handle the host cannot re-open per attach, so the
registry passes those displays through rather than owning them. Read Sway's empty list as *this
host does not track them*, not as "nothing is streaming". Omarchy is Hyprland, so `ctl display`
there lists the lingered heads.

### Reading `stats`

Two numbers named alike measure different things, and the gap between them is large:

- **Target** is the encoder's bitrate, where adaptive bitrate has settled. It is always available.
- **Sent** is what actually left the box. It only exists while a capture is recording.

A still desktop shows 300 Mbps of target against 11 Mbps sent, because capture is damage-driven:
nothing changed, so nothing was encoded. For the same reason `stats` prints **new** frames per
second beside **repeated** ones. A healthy 240 Hz stream of a motionless screen reads `0.0 fps new ·
157.2 fps repeated`; reading only the first number would say the stream was dead.

Add `--json` to any verb for machine-readable output: `{"v":1,"data":…}` on success,
`{"v":1,"error":{"code":…,"message":…}}` on failure, both on stdout. That envelope is the contract —
the tables above are for humans and are not stable.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | The host refused the request (the message carries its reason). |
| `2` | Usage error. |
| `3` | No host reachable — not running, or never run on this machine. |
| `4` | **Certificate pin mismatch.** Kept distinct on purpose: a script that treats it as "host down" and retries would be retrying into whatever is answering on that port. |

### Watching events

`watch` holds one long-lived connection and reconnects by itself, which makes it the right shape for
a status widget or a script. It starts at the live tail; `--since 0` replays the host's ring and
`--since <seq>` resumes after an event you saw:

```sh
punktfunk-host ctl watch --kinds pairing.pending,stream.'*' | while read -r line; do
  echo "$line"
done
```

Two synthetic lines are ours rather than the host's:

- `{"v":1,"kind":"ctl.resync"}` — the stream fell behind the host's catch-up ring, so anything you
  believe about pending devices or live sessions may be stale. Re-run `ctl status` / `ctl pending`
  instead of trusting your incremental state.
- `{"v":1,"kind":"ctl.disconnected","data":{"error":…}}` — the connection dropped; a reconnect is
  already in progress.

The host caps concurrent event streams (the console holds one); past the cap you get a `503` with
the host's own message and exit 1.

### How it authenticates

`ctl` reads two files from the host's config directory (`~/.config/punktfunk`, mode 0700) and
nothing else:

- `mgmt-token` — the operator token the host mints on first start, the same one the web console
  uses. `ctl` **consumes** it and never creates one: a missing token is an error, not a prompt.
- `native-cert.pem` (or `cert.pem` on older hosts) — the host's certificate, which `ctl` pins
  **before** sending the token. Anything else answering on the management port fails the TLS
  handshake and no credential is ever transmitted — that is exit code 4.

There is deliberately **no `--token` flag and no token environment variable** — a credential on a
command line or in an environment is readable by other processes on the box. The host persists its
token to that file on every start, so `ctl` always has one to read.

Everything runs over loopback — `ctl` adds no listener and no new way in.

## `punktfunk1-host`

A standalone native-only host, mainly for testing the `punktfunk/1` path without the GameStream server
or web console.

```sh
punktfunk-host punktfunk1-host --source virtual
```

| Flag | Meaning |
|---|---|
| `--port <N>` | QUIC listen port (default `9777`). |
| `--source synthetic` · `synthetic-abr` · `virtual` | `virtual` uses a real virtual display + NVENC; `synthetic` emits fixed test frames; `synthetic-abr` sizes every frame from the rate the session is running at, so Automatic can be measured on a box with no display and no GPU. |
| `--content <SCRIPT>` | What `synthetic-abr` encodes: `steady`, `idle-then-motion`, or `frame-driven:<fps>` for a source slower than the session (default `steady`). |
| `--fill <PCT>` | Share of each frame's bit allowance `synthetic-abr` fills, 1–100 (default 100). |
| `--recovery-ms <MS>` | How long `synthetic-abr` takes to answer a keyframe request. `0` (the default) answers on the next frame; a host that rebuilds its pipeline takes about a second. |
| `--keyframe-answer <KIND>` | What `synthetic-abr` answers a keyframe request with: `idr` (the default), or `wave:<n>` to answer only every n-th ask with one, as a host that prefers an intra-refresh wave does. |
| `--seconds <N>` / `--frames <N>` | Bound each session by wall-clock seconds or frame count. |
| `--max-concurrent <N>` | Stream at most N sessions at once (default 4); overflow waits in the queue. |
| `--max-sessions <N>` | Exit after N sessions (0 = serve forever). |
| `--allow-tofu` | Also accept **unpaired** clients (trust-on-first-use) and advertise pairing as optional. Pairing is required by default; trusted LANs only. (`--allow-pairing`/`--require-pairing` are the old names for the default behaviour and are accepted as no-ops.) |
| `--pairing-pin <PIN>` | Use a fixed pairing PIN instead of a fresh random one per ceremony. For test harnesses/CI only — a guessable PIN defeats the ceremony's rate limit. |
| `--data-port <PORT>` | Pin the video data plane to this fixed UDP port; video still follows the client's hole-punch. Same as `PUNKTFUNK_DATA_PORT`. |
| `--idle-timeout-ms <MS>` | Disconnect-detection latency — the QUIC control-connection idle timeout (default 8000). |
| `--no-mdns` | Skip the `_punktfunk._udp` advert; clients use `--connect HOST:PORT`. Same as `PUNKTFUNK_MDNS=0`. |

`--max-concurrent` and `--allow-tofu` are **`punktfunk1-host`-only** — `serve` does not accept them.
On `serve` you arm pairing from the web console instead (`--open` is its serve-any-device switch),
and concurrency is fixed at the built-in default (4 sessions) rather than settable from the command
line.

Both `serve` and `punktfunk1-host` advertise the host on the network so clients can discover it. The
graphical client browses the LAN for you, so it needs no command; from a terminal on the client
machine, [`punktfunk hosts list --probe`](/docs/clients#scripting-the-punktfunk-cli) re-checks the hosts you
have already saved by asking each one directly — which is how you confirm a routed or VPN host that mDNS never reaches.
(`punktfunk-probe --discover` also browses the LAN, but it is a developer tool built from the repo,
`cargo run -p punktfunk-probe -- --discover`, and no package installs it.)
Where multicast doesn't work (some Docker/VLAN setups), pass `--no-mdns` (or set
`PUNKTFUNK_MDNS=0`) and add the host in the client by address instead.

## `service` (Windows)

The Windows lifecycle surface. The installer runs `service install` for you, so you only need these
when you change something or when the service needs a nudge. Run them from an **Administrator**
prompt.

```powershell
punktfunk-host service install [--gamestream=on|off] [--allow-public-network] [--mgmt-bind=IP:PORT]
punktfunk-host service uninstall
punktfunk-host service start | stop | restart | status
```

| Subcommand | What it does |
|---|---|
| `install` | Registers the auto-start `PunktfunkHost` service, adds the firewall rules, and writes a default `%ProgramData%\punktfunk\host.env` if there isn't one. Safe to re-run: it's also how you change the options below. |
| `--gamestream=on\|off` | Sets the GameStream setting, the same toggle as **Host → Settings** in the web console. A `host.env` that still runs `serve --gamestream` is moved to `serve` with GameStream kept on, so the console can change it. A command line you edited by hand is left alone. |
| `--allow-public-network` | Also opens the ports on networks Windows classifies **Public**. By default only Private and Domain are opened. |
| `--mgmt-bind=IP:PORT` | Sets `PUNKTFUNK_MGMT_BIND` in `host.env` and opens the firewall for that port. The installer passes `0.0.0.0:47991` when Sunshine, Apollo or Vibeshine holds 47990. |
| `--web-bind=ADDR` | Sets `PUNKTFUNK_UI_BIND` in `host.env` — where the web console listens (`127.0.0.1`, `0.0.0.0`, or one address). The installer passes the answer from its Configure page; absent, `host.env` keeps what it says. |
| `uninstall` | Stops and deletes the service and removes its firewall rules. It does **not** remove the host itself — see [Uninstalling](/docs/uninstall) for that. |
| `start` / `stop` / `restart` | Service control. `restart` waits for the old process to exit first — this is what picks up a `host.env` edit. |
| `status` | Queries the service (the same thing `sc query PunktfunkHost` prints). |

See [Running as a Service](/docs/running-as-a-service) and [Windows Host](/docs/windows-host).

## `tray` (Windows)

The status tray is a per-user program started at sign-in, so an update or a crash otherwise leaves
you without the icon until the next logon. This is how you get it back without signing out:

```powershell
punktfunk-host tray start
punktfunk-host tray status
punktfunk-host tray stop
```

`start` reports the process it started, or says the tray is already running; `status` says whether
the tray is installed at all (it's an optional component at install time) and whether it's running.

## `driver` (Windows)

The installer installs and removes the bundled drivers, so you rarely touch this. It exists for
removing one without uninstalling the host:

```powershell
punktfunk-host driver uninstall            # the pf-vdisplay virtual display driver
punktfunk-host driver uninstall --gamepad  # the virtual-gamepad driver instead
```

`driver install --dir <stage> [--gamepad]` is the install half; it takes the staged driver files the
installer lays down, which is why it isn't something you run by hand.

## `web` (Windows)

Print the [web console](/docs/web-console) login password, from an **elevated** PowerShell — the
file it reads is ACL'd to Administrators and SYSTEM, so an ordinary prompt gets access denied:

```powershell
punktfunk-host web password
```

This is how you get the password after a **silent** install (winget, `/VERYSILENT`), which generates
one and displays nothing. The wizard shows it on its final page instead, so you only need this later.
`web setup` is the install half and takes the staged console payload, so it isn't run by hand.

## `plugins`

`punktfunk-host plugins add|remove|list|enable|disable|status` installs plugins and switches the
plugin/scripting runner on — the same thing the web console's **Plugins** page does. Plugins run
with the host's privileges, so read [Plugins](/docs/plugins) before installing one.

## `list-monitors`

`punktfunk-host list-monitors` prints the **physical** monitors this host's compositor has, by
connector name — which is how you name one for [Streamed
screen](/docs/virtual-displays#stream-a-real-monitor-instead) (in the console, or as
`PUNKTFUNK_CAPTURE_MONITOR`).

```sh
punktfunk-host list-monitors
```

```
Kwin:
  HDMI-A-1        1920x1080@60 at +0,+0    scale 1  Dell U2412M  [primary]
  DP-2            2560x1440@144 at +1920,+0  scale 1  ACME 27  [PINNED]
```

Tags flag what's worth knowing before you pick: `primary`, `disabled` (nothing to stream),
`punktfunk virtual display` (one of ours, not a real head), and `PINNED` for the one currently
selected. Linux
only — it reads the live compositor, so run it in (or with the environment of) the session you want
to stream.

`punktfunk-host mirror-test --monitor <CONNECTOR> [--seconds N] [--cpu]` then proves the whole path —
mirror, capture, frames — with no client involved. It reports the first frame, the frame count and
the negotiated size. Screen recording is damage-driven, so move the mouse on the host while it runs;
an idle desktop legitimately yields almost nothing.

## `hdr-probe` and `probe-compositor`

Two Linux readiness checks that need no client and no session of their own.

```sh
punktfunk-host hdr-probe
punktfunk-host probe-compositor
```

`hdr-probe` answers "why isn't my stream HDR?" — it reports, for both Linux HDR routes, whether the
box can deliver 10-bit PQ right now: is a monitor in HDR colour mode (the GNOME monitor-mirror
route), is the resolved gamescope the `punktfunk-gamescope` build with the knob on, and does the
encoder probe Main10 for HEVC/AV1. Run it with the same environment the host service has, or the
answers describe your shell rather than the host — [HDR → Check it](/docs/hdr#check-it) has that
one-liner and reads the output line by line. See
[HDR on gamescope](/docs/gamescope#hdr-on-gamescope) for the gamescope half.

`probe-compositor` exits **0** only when the compositor is up and can create a virtual output now —
what a session-bringup script should gate on instead of a blind `sleep`.

There is no `hdr-probe` on Windows. The Windows equivalent is a GPU colour self-test of the HDR
capture conversion — it needs no display and no session, and prints PASS or FAIL with the largest
error it saw:

```powershell
punktfunk-host hdr-p010-selftest 1920x1080 nvidia
```

Both arguments are optional: your real capture size (heights like 1080 aren't 16-aligned and take a
different driver path — the default is a token `64x64`) and, on a dual-GPU box, the vendor that
encodes: `intel`, `nvidia` or `amd`.

## `detect-conflicts`

`punktfunk-host detect-conflicts` reports other Moonlight-compatible hosts (Sunshine, Apollo, and
forks) installed or running on this machine. Running one alongside Punktfunk is **unsupported** —
they fight over the same ports and virtual-display driver. Prints what it found and exits **1** if
any conflict exists, **0** if clean (so installers and scripts can gate on it). The host also runs
this check at `serve` startup and reports it in the logs and in the management API's status
summary; on Windows the installer warns you before it installs. (The tray stays quiet about it on
purpose — an installed-but-idle Sunshine isn't a conflict until it runs.)
See [Troubleshooting → another streaming host is installed](/docs/troubleshooting#another-streaming-host-sunshine-apollo--is-installed).

## `punktfunk` on the client machine

The client half has its own command, `punktfunk` — the same core the graphical apps use, with no
window, so a script gets what a click gets, including waking a sleeping host and waiting for it.

Its verbs, where it ships, the `<host-ref>` grammar and the stable exit codes are on [Clients → the
`punktfunk` CLI](/docs/clients#scripting-the-punktfunk-cli); `punktfunk help <command>` prints one
verb's flags. `punktfunk wake` has its own exit codes, on [Wake on LAN → From the command
line](/docs/wake-on-lan#from-the-command-line).

## Environment

Most behaviour (compositor, video source, input backend, zero-copy) is set in
[`host.env`](/docs/configuration), not on the command line. When running as a
[service](/docs/running-as-a-service), the unit loads `host.env` for you.
