---
title: Events & hooks
description: React to what the host does — lifecycle events over SSE, hook commands and webhooks, per-app prep/undo — for notifications, DND toggles, Home Assistant, and more.
---

The host emits a **lifecycle event** for the things you'd want to react to: a client connects or
disconnects, a stream starts or stops, a pairing request arrives, a virtual display is created,
the library changes, the host starts or shuts down. Two ways to consume them:

- **Hooks** — zero-code: entries in `~/.config/punktfunk/hooks.json` run a **command** or POST a
  **webhook** when a matching event fires. Covers the common automation: Do-Not-Disturb during a
  stream, a phone notification on a pairing request, pausing downloads while playing.
- **The event stream** — code: `GET /api/v1/events` on the management API is a standard
  [Server-Sent Events](https://developer.mozilla.org/en-US/docs/Web/API/Server-sent_events)
  stream of the same events, for scripts and integrations that want to *decide* things (e.g.
  auto-approve pairing from a known subnet by calling the approve endpoint).

Hooks **observe** — they can never veto or delay a connection, a stream, or a pairing decision,
and nothing configured here runs anywhere near the streaming path.

## The events

| Kind | Fires when | Carries |
|---|---|---|
| `client.connected` / `client.disconnected` | a client session is admitted / goes away | device name, cert fingerprint, plane (`native`/`gamestream`); disconnect adds `reason`: `quit` (user stop), `timeout` (vanished), `error` |
| `session.started` / `session.ended` | an A/V session registers / ends | session id, client label, cert fingerprint, plane, mode (`3840x2160@120`), HDR. `session.ended` adds a `summary`: duration, codec / bit depth / chroma, the bitrate span (min/avg/max + how many times it moved), frames sent and dropped, input datagram counts, gyro cadence, audio egress totals, bring-up ms, path MTU, and `ended` — `local`, `game_exited`, `host_ended`, `host_error`, `lost` or `stopped_by_operator`. The same shape `GET /api/v1/session/last` returns for the last eight |
| `stream.started` / `stream.stopped` | video actually starts / stops | mode, HDR, client name, cert fingerprint, launched app id/title (when one was requested), plane |
| `game.running` | a launched game's own process is seen running (not merely its launcher) | app id, title, store, client, cert fingerprint, plane |
| `game.window` | the game's own window reaches the screen — often 5-40 s after `game.running`, while Proton builds a prefix or a splash sits on a black window | the same, plus `title` and `app_id` of that window |
| `game.exited` | a launched game is gone | the same, plus `reason`: `exited` (the player quit it) or `terminated` (the host closed it, per your [session⇄game settings](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does)) |
| `pairing.pending` | an unpaired device knocks — a native one once per device, not per retry; a Moonlight one when its PIN ceremony parks | device name, fingerprint, plane |
| `pairing.completed` / `pairing.denied` | a pairing is approved+stored / denied | device name, fingerprint, plane |
| `display.created` / `display.released` | a virtual display is minted / kept displays are released | backend + mode / count |
| `library.changed` | the game library is mutated | source: `manual`, or the provider id that reconciled (`PUT /api/v1/library/provider/{p}`) |
| `update.available` | a verified manifest announces a release newer than the running host — once per discovered version, not on every check | version, channel (`stable`/`canary`), and this host's install kind (`apt`, `windows-installer`, …) |
| `update.applied` | the new binary's first start after a successful update | `from`, `to` |
| `plugins.changed` | a plugin's registration changes (registered, restarted, deregistered, or its lease expired) | plugin id |
| `store.changed` | an install or uninstall finished, or a plugin catalog was refreshed | none — re-read `GET /api/v1/store/catalog` / `…/installed` |
| `settings.changed` | the operator changed host settings in the console | the setting ids — re-read `GET /api/v1/host/settings` |
| `host.started` / `host.stopping` | the serve planes come up / wind down | version, whether GameStream is enabled |

Every event is a small JSON document with a monotonic `seq`, a `ts_ms` timestamp, a `schema`
version (additive-only — fields get added, never renamed), and the fields above:

```json
{ "seq": 42, "ts_ms": 1784227449526, "schema": 1,
  "kind": "stream.started",
  "stream": { "mode": "2560x1440@120", "hdr": true,
              "client": "Living Room TV", "fingerprint": "9f86d081…",
              "app": "steam:570", "plane": "native" } }
```

## Hooks: `hooks.json`

Create `~/.config/punktfunk/hooks.json` (Windows: `%ProgramData%\punktfunk\hooks.json`), or PUT
the same document to `/api/v1/hooks` from a script — changes apply immediately, no restart:

```json
{
  "hooks": [
    { "on": "stream.started",  "run": "/home/me/.config/punktfunk/scripts/on-stream.sh" },
    { "on": "stream.stopped",  "run": "/home/me/.config/punktfunk/scripts/off-stream.sh" },
    { "on": "client.connected",
      "filter": { "fingerprint": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" },
      "run": "kscreen-doctor output.HDMI-A-1.mode.3840x2160@60" },
    { "on": "pairing.pending",
      "webhook": "https://ha.local/api/webhook/punktfunk",
      "hmac_secret_file": "/home/me/.config/punktfunk/webhook-secret" }
  ]
}
```

Each entry:

| Field | Meaning |
|---|---|
| `on` | Which events fire it: an exact kind (`stream.started`) or a `domain.*` prefix (`pairing.*`). |
| `run` | A shell command (`sh -c` on Linux). Gets the event JSON on **stdin** and flat **`PF_EVENT_*`** env vars. |
| `webhook` | A URL the event JSON is POSTed to. TLS-verified, redirects are never followed, no Punktfunk credentials attached. |
| `filter` | Optional exact-match constraints: `fingerprint` (the device's certificate — what the console writes when you pick a device), `client` (its display name), `plane` (`native`/`gamestream`), `app`. All present fields must match. |
| `timeout_s` | Command timeout (default 30, max 600) — on expiry the whole process group is killed. |
| `debounce_ms` | Minimum interval between firings of this hook (0 = every event). |
| `hmac_secret_file` | File with a secret; the webhook gains `X-Punktfunk-Signature: sha256=<hex HMAC-SHA256 of the body>` so your receiver can authenticate the host. |

Target a device by `fingerprint`, not by name: two devices can share a name, and renaming one
changes what a `client` filter matches. The console's device picker shows names and stores the
fingerprint; `GET /api/v1/clients` and `…/native/clients` list both. A `client` filter that goes
quiet is logged — the host names the event's own client so the mismatch is visible.

### What the host refuses

The document is validated as a whole, and **one bad entry disables every hook** — the host logs
`hooks.json invalid — hooks disabled until fixed` and runs none until you correct it. The rules:

- An entry needs a non-empty `on`, plus `run` and/or `webhook`.
- `webhook` must be an `http(s)://` URL, and must **not** point at loopback, `localhost` or a
  link-local address (which also blocks the cloud metadata endpoint). A receiver on this same
  machine is what a `run` command is for. Ordinary LAN addresses — `192.168.x.x`, a ULA, a
  hostname — are fine, so Home Assistant on another box on your network works as written.
- `timeout_s` must be 1–600.
- If `hmac_secret_file` is set but unreadable, the host **skips** that POST rather than sending it
  unsigned. It also *warns* (and still signs) when that file isn't owned by you or is readable by
  anyone else — `chmod 600` it.

Check the log after editing: `journalctl --user -u punktfunk-host` on Linux, or the console's
**Troubleshooting** page on either platform. Hook lines are identified by the webhook's
`scheme://host` or the command's program name plus a short id — never the URL path or arguments,
which is where tokens live.

A `run` command's shell one-liner vocabulary — the event flattened to env, values sanitized:

```sh
#!/bin/sh
# PF_EVENT_KIND=stream.started   PF_EVENT_SEQ=42
# PF_EVENT_STREAM_MODE=2560x1440@120   PF_EVENT_STREAM_HDR=true
# PF_EVENT_STREAM_CLIENT='Living Room TV'   PF_EVENT_STREAM_APP=steam:570
# PF_EVENT_STREAM_PLANE=native   PF_EVENT_JSON='{…the whole event…}'
[ "$PF_EVENT_KIND" = stream.started ] && makoctl mode -a do-not-disturb
```

Richer payloads (and the full document) are on stdin for `jq`. On Windows, a SYSTEM host runs the
command as the signed-in user of **that host's WTS session** (never as SYSTEM); that path can't
carry per-process env or stdin, so the event JSON's path is appended as the command's last argument
instead.

Verify a signed webhook (Python):

```python
import hmac, hashlib
expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
ok = hmac.compare_digest(request.headers["X-Punktfunk-Signature"], expected)
```

**Rules of the road:** hooks are fire-and-forget and bounded — at most 8 in flight (extras are
dropped with a log line, never queued), and a command that outlives its timeout is killed. Commands
run unprivileged (as the host user on Linux, its WTS session user on Windows). On Linux, a command
that names a script by **absolute path** is safety-checked: the file *and every directory above it*
must be owned by you (or root) and not group/world-writable, or the host refuses loudly in the log.
(`/tmp`-style sticky world-writable dirs pass.) Write the full path — `~/…` and bare PATH names like
`makoctl` are expanded by the shell afterwards and are never checked. On Windows the ACL on the
config directory is the boundary.

The two simplest cases also exist as plain [host.env](/docs/configuration) settings, no
`hooks.json` needed: `PUNKTFUNK_ON_CONNECT_CMD` and `PUNKTFUNK_ON_DISCONNECT_CMD`.

## Per-app prep/undo

For per-title setup (HDR toggle, MangoHud, a VRR tweak), attach `prep` steps to a GameStream
`apps.json` entry or to a [custom library entry](/docs/game-library#adding-a-game-by-hand) — each
`do` runs **before** the title launches (synchronously — the launch waits), each `undo` runs at
session end in **reverse order**, best-effort, even if the session crashed:

```json
{ "id": 2, "title": "Steam", "compositor": "gamescope", "cmd": "steam -gamepadui",
  "prep": [
    { "do": "~/bin/hdr on",  "undo": "~/bin/hdr off" },
    { "do": "pactl set-default-sink game_sink", "undo": "pactl set-default-sink desk_sink" }
  ] }
```

A `do` that fails logs, keeps going, and its own `undo` is skipped (it never took effect).

Every prep command (and its `undo`) runs with the session's negotiated mode in its environment:
`PF_STREAM_WIDTH`, `PF_STREAM_HEIGHT`, `PF_STREAM_REFRESH` and `PF_STREAM_HDR` (`1`/`0`), plus the
app identity — `PF_APP_ID` for a native client's launch, `PF_APP_TITLE` for a Moonlight one. So a
per-mode frame cap is one step for every device —
`{ "do": "rtss-cli property:set Global FramerateLimit $PF_STREAM_REFRESH" }` — instead of one
hard-coded entry per client.

### One entry, every client

The point of those four variables is that the *entry* stops describing a device. Attach one script
to the title and let the session tell it what it got — 60 Hz on the phone, 4K120 HDR on the TV, from
the same two lines:

```json
{ "id": 2, "title": "Cyberpunk 2077", "cmd": "steam -applaunch 1091500",
  "prep": [
    { "do":   "/home/me/.config/punktfunk/scripts/mode.sh do",
      "undo": "/home/me/.config/punktfunk/scripts/mode.sh undo" }
  ] }
```

```sh
#!/bin/sh
# ~/.config/punktfunk/scripts/mode.sh — run as `mode.sh do` before the title, `mode.sh undo`
# at session end. Nothing here names a client: the negotiated mode arrives in the environment.
set -eu

CONF="${XDG_CONFIG_HOME:-$HOME/.config}/MangoHud/MangoHud.conf"

case "${1:-}" in
do)
  # Cap the game at the refresh this client actually negotiated.
  cp -f "$CONF" "$CONF.pf-bak"
  printf 'fps_limit=%s\n' "$PF_STREAM_REFRESH" >>"$CONF"

  # Light the panel's HDR only when the session really negotiated it.
  if [ "$PF_STREAM_HDR" = 1 ]; then
    kscreen-doctor output.HDMI-A-1.hdr.enable
  fi

  # The raster, for anything that wants pixels — a launcher's window size, a per-mode
  # config profile, or just a line in the journal naming what launched.
  logger -t punktfunk \
    "prep ${PF_APP_ID:-${PF_APP_TITLE:-desktop}}: ${PF_STREAM_WIDTH}x${PF_STREAM_HEIGHT}@${PF_STREAM_REFRESH}"
  ;;
undo)
  mv -f "$CONF.pf-bak" "$CONF"
  if [ "$PF_STREAM_HDR" = 1 ]; then
    kscreen-doctor output.HDMI-A-1.hdr.disable
  fi
  ;;
esac
```

Four things that example is quietly relying on:

- **`undo` sees exactly what its `do` saw.** The values are captured once, at launch, and held for
  the session — so teardown can branch on `PF_STREAM_HDR` and reach the same answer however the
  stream ended.
- **`PF_STREAM_HDR` is `1`/`0`**, the stream-marker file's spelling, not the `true`/`false` that
  `PF_EVENT_*` uses. One script can be written against either.
- **The app identity depends on the plane**: `PF_APP_ID` on a native client's launch,
  `PF_APP_TITLE` from a Moonlight one. `${PF_APP_ID:-${PF_APP_TITLE:-desktop}}` reads whichever one
  is set, and `:-` also catches the empty string a launch with no title of its own leaves behind.
- **`set -u` is doing work.** An older host doesn't set these, and the step then fails loudly (and
  disarms its own `undo`) instead of silently capping the game at `fps_limit=`.

The same `prep` array works on a custom `library.json` entry, where the identity arrives as
`PF_APP_ID`. The console's Library form has no input for prep steps and **clears** them on save, so
edit that file directly.

### A launch on its own workspace

A launch lands on whatever the streamed head is showing — which on a shared desk is the operator's
browser, chat and terminal. Where the compositor can place windows, the host instead puts the
launch on an **empty workspace** on that head and switches back when the game is done. Set it per
entry in `library.json`, beside `prep`:

```json
{ "title": "Hades", "on_window": { "workspace": "own" } }
```

`own` (the default) or `current` to keep the old behaviour. The host-wide default is
`launch_workspace` in `display-settings.json`; the entry wins where both are set. A reconnect to a
running game goes back to *that* game's workspace — the workspace belongs to the launch, not to the
session.

Three more keys in the same block act on the game's first window, once, when it appears:

```json
{ "title": "Hades", "on_window": {
    "workspace": "own", "focus": true, "fullscreen": false, "move_to_stream_output": true
} }
```

- **`focus`** (default `true`) raises it. The player asked for this game, so it belongs in front —
  a launcher that grabs focus back leaves them looking at the desktop.
- **`fullscreen`** (default `false`) makes it full-screen. Off because most games set their own
  mode, and forcing it fights a title that deliberately opened windowed.
- **`move_to_stream_output`** (default `true`) carries it onto the streamed screen if it opened on
  one of your own monitors. A game the player cannot see is the failure this whole stage exists
  for, so leave it on unless you are deliberately playing on the host's panel.

| Compositor | A launch gets its own workspace |
| --- | --- |
| Hyprland | yes — an empty workspace on the streamed head, or a free one |
| sway / wlroots | yes — same, through `swaymsg` |
| KWin | not yet |
| GNOME / Mutter | no — no per-output workspace to aim a launch at |
| gamescope | no — the game already has the session to itself |
| Windows | no — no workspaces |

Placement never costs you a launch: a compositor that refuses the switch, or a missing `hyprctl`,
logs one line and the game opens where it would have before.

## Reacting to a game, not a stream

`stream.stopped` tells you the *stream* ended; `game.exited` tells you the *game* did. Often the
same moment, but not always — a desktop stream has no game at all, and a stream can outlive its
game if you turned off "end the session when the game exits". No polling needed:

```json
{ "hooks": [
    { "on": "game.running", "run": "/home/me/.config/punktfunk/scripts/game-up.sh" },
    { "on": "game.window",  "run": "/home/me/.config/punktfunk/scripts/game-on-screen.sh" },
    { "on": "game.exited",  "run": "/home/me/.config/punktfunk/scripts/game-down.sh" }
] }
```

`game.running` fires when the host sees the game's *process*; `game.window` fires when its window
is actually on screen, which on a cold Proton prefix or an emulator loading a ROM can be half a
minute later. Dim the lights on the first; drop a "ready" notification on the second. It is
best-effort: on a compositor that reports no windows — KWin, GNOME, gamescope, Windows — it never
fires at all, and nothing else about the launch changes.

Both carry the title in `PF_EVENT_GAME_TITLE` / `PF_EVENT_GAME_APP`, and `game.exited` adds
`PF_EVENT_REASON` so a script can tell "the player quit" (`exited`) from "the host closed it"
(`terminated`) — worth checking before you, say, power the TV off.

Ending the session when a game exits needs no script: it is the default, on the console's
**Virtual displays** page under
[When a game or a session ends](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does).

## The event stream (`GET /api/v1/events`)

For a shell script or a status widget, the easy way is
[`punktfunk-host ctl watch`](/docs/host-cli#ctl) — it does the SSE, the `Last-Event-ID` resume and
the reconnect for you, and prints **one JSON object per line**, so the credentials never leave the
host binary:

```sh
punktfunk-host ctl watch --kinds pairing.pending,stream.'*'
```

It also emits a synthetic `{"kind":"ctl.resync"}` line when the stream fell off the host's catch-up
ring, which is the signal to re-snapshot rather than trust what you have.

For code that wants the raw stream, subscribe to SSE on the management API directly (loopback +
bearer token — the same credentials as the rest of the admin surface):

```sh
. ~/.config/punktfunk/mgmt-token   # sets PUNKTFUNK_MGMT_TOKEN
curl -Nk -H "Authorization: Bearer $PUNKTFUNK_MGMT_TOKEN" \
  "https://127.0.0.1:47990/api/v1/events?kinds=pairing.*,stream.*"
```

The token file holds `PUNKTFUNK_MGMT_TOKEN=<token>`, not a bare token, so it can be sourced (or
handed to a systemd unit as an `EnvironmentFile`) — `cat` it straight into the header and every
request comes back 401. The runner's `plugin-token` file has the same shape.

- Frames carry `id:` (the event's `seq`), `event:` (the kind), `data:` (the event JSON).
- Reconnect with the standard `Last-Event-ID` header (or `?since=<seq>`) and the host replays
  what you missed from its in-memory ring (~1024 events); if you fell off the ring you get one
  `event: dropped` frame first — resync from the REST snapshots (`/status`, `/clients`, …).
- No cursor replays the whole ring. One `event: live` frame closes the replay: what follows it
  happened after you connected, so a notifier should stay quiet until it arrives.
- `?kinds=` filters server-side: exact kinds or `domain.*` prefixes, comma-separated.

## Scripts, plugins, and the runner

For anything beyond a `curl` one-liner there is **`@punktfunk/host`** — the TypeScript SDK
(`sdk/` in the repo): typed events with automatic reconnect/resume, the REST surface, and a
plugin convention (`punktfunk-plugin-*`). Its **runner** (`punktfunk-scripting`) supervises a
directory of scripts and installed plugins as one service: crash-restarts with backoff, and a
`systemctl stop` that interrupts plugins structurally so their cleanup runs. See the SDK README
for the five-line quickstart and unit templates.

For ready-made plugins — sync your ROM collection or your Playnite library into the game library, or
hand a USB device on the couch to the host — see [Plugins](/docs/plugins). Install one from the web
console's **Plugins** page (Browse → pick → confirm; the host installs it and restarts the runner),
or from a terminal with `punktfunk-host plugins add <name>` followed by
`punktfunk-host plugins enable`.

The canonical "decide, don't just observe" pattern — approve pairing from your phone: watch
`pairing.pending`, send yourself a notification, and call
`POST /api/v1/native/pending/{id}/approve` when you tap yes. The full API is documented at
[`/api/docs`](/api) on your host.

> A unit under the runner auto-connects with the host's **scoped plugin token**, which covers
> the everyday surface (status, library, sessions, events) but deliberately not **hook
> registration**, **pairing administration**, the **plugin store** (`/api/v1/store…`, reads
> included), the **update endpoints** (`/api/v1/update…`), or another plugin's UI credential — so a
> plugin defect can't admit new devices, install code, or trigger an update. Those routes answer
> 403 on the plugin token. A script that should administer pairing (like the approval pattern above)
> opts into the full-admin credential explicitly: set `PUNKTFUNK_MGMT_TOKEN` on the unit (e.g.
> a `systemctl --user edit punktfunk-scripting` drop-in) or pass `{ token }` to `connect()`.

## Recipe: full controller passthrough (VirtualHere)

To get a controller's *native* features on the host — DualSense gyro, touchpad, adaptive
triggers, USB rumble — or to use a device no emulation can stand in for (a racing wheel, a HOTAS),
hand the physical device from the couch to the host over
[VirtualHere](https://www.virtualhere.com/) (USB-over-IP) while you play.

**Use the plugin.** [VirtualHere passthrough](/docs/plugins#virtualhere-usb-passthrough) finds the
device by name (so it survives the couch rebooting), brackets it around the session, gives it back
if anything crashes, and tells you which half of the setup is broken. That is the supported route;
the rest of this section is for people who would rather not install a plugin.

**Turn off controller forwarding on the couch.** Whatever route you take, the client that hands the
device over should stop *also* forwarding it: Settings → **Forward controllers**, off
([Client settings](/docs/client-settings#input)). Otherwise the host gets two controllers for one
pair of hands and games read both. On Linux and Windows it matters twice over — while the client
has the pad open it has *claimed* the device node, and VirtualHere cannot bind a device somebody
else is holding.

**The two sides.** VirtualHere is a server/client pair, and you run both: the **server on the couch**
(where the device is plugged in) shares it, and the **client on the host** mounts it. The client's
`-t` flag is a one-shot IPC to the already-running client — `-t LIST` prints every visible device
with its address (`server.port`, e.g. `couch-deck.11`), `-t "USE,<addr>"` mounts it, and
`-t "STOP USING,<addr>"` hands it back.

### Zero-code: two hooks

Bracket it on the stream with two [hooks](#hooks-hooksjson):

```json
{
  "hooks": [
    { "on": "stream.started", "run": "vhclientx86_64 -t \"USE,couch-deck.11\"" },
    { "on": "stream.stopped", "run": "vhclientx86_64 -t \"STOP USING,couch-deck.11\"" }
  ]
}
```

`couch-deck.11` is the device's address from `vhclientx86_64 -t LIST`.

The trade-offs the plugin exists to fix: the address is hard-coded, so it breaks when the couch
reboots or the device moves port; and if the stream ends abnormally the `stream.stopped` hook never
fires, leaving the device stranded on the host until somebody notices. There is also a
[`virtualhere-dualsense.ts`](https://git.unom.io/unom/punktfunk/src/branch/main/sdk/examples/virtualhere-dualsense.ts)
SDK example to build your own script on.

> VirtualHere is a commercial product, sold separately by VirtualHere Pty. Ltd. — free for one
> shared device, licensed beyond that. Punktfunk is not affiliated with it.
