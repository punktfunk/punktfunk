---
title: Events & hooks
description: Run a command or call a webhook when the host does something, prepare the host for one game, or read the host's event stream from a script.
---

Make the host do something when a stream starts, a device knocks or a game exits: turn on
Do-Not-Disturb, send a phone notification, pause downloads, switch the TV's HDR. The host publishes
an **event** for each of these, and you react with:

- **Hooks**: run a command or call a webhook. Set them on the console's **Automation** page or in
  `hooks.json`.
- **Per-app prep/undo**: commands around one title's launch.
- **The event stream**: `GET /api/v1/events`, for scripts and plugins.

Hooks only observe. They can't veto or delay a connection, a stream or a pairing, and they never
run in the streaming path.

## The events

| Kind | Fires when | Carries |
|---|---|---|
| `client.connected` / `client.disconnected` | a device connects / goes away | name, fingerprint, plane (`native` / `gamestream`); disconnect adds `reason`: `quit`, `timeout` or `error` |
| `session.started` / `session.ended` | a session starts / ends | session id, client, fingerprint, plane, mode (`3840x2160@120`), HDR. `session.ended` adds a `summary` (duration, codec, bitrate span, frames, bring-up time, and `ended`: `local`, `game_exited`, `host_ended`, `host_error`, `lost` or `stopped_by_operator`), the same shape as `GET /api/v1/session/last` |
| `stream.started` / `stream.stopped` | video starts / stops | mode, HDR, client, fingerprint, launched app, plane |
| `game.running` | a launched game's own process runs (not just its launcher) | app id, title, store, client, fingerprint, plane |
| `game.window` | the game's window reaches the screen, often 5–40 s after `game.running` | the same, plus the window's `title` and `app_id` |
| `game.exited` | a launched game is gone | the same, plus `reason`: `exited` (the player quit) or `terminated` (the host closed it, per [these settings](/docs/virtual-displays#when-a-game-ends-and-when-a-session-does)) |
| `pairing.pending` | an unpaired device knocks, once per device | name, fingerprint, plane |
| `pairing.completed` / `pairing.denied` | a pairing is approved / denied | name, fingerprint, plane |
| `access.granted` / `access.changed` | you pick a device's access when pairing / edit it later | device, `grants` bits, `expires_unix` (absent: no expiry) |
| `access.expired` | a streaming device's access runs out | device |
| `display.created` / `display.released` | a virtual display is created / kept displays are released | backend and mode / count |
| `library.changed` | the game library changes | `source`: `manual` or the provider id |
| `update.available` | a newer release is found, once per version | version, channel, install kind (`apt`, `windows-installer`, …) |
| `update.applied` | the updated host first starts | `from`, `to` |
| `action.invoked` | a [host power](/docs/host-power) action is accepted, or fails | `id` (`power.sleep`, `power.reboot`, `power.shutdown`, `host.restart`), device (absent for the console), `outcome` |
| `plugins.changed` | a plugin registers, restarts or goes away | plugin id |
| `store.changed` | a plugin install or uninstall finished, or a catalog refreshed | nothing: re-read the store |
| `settings.changed` | host settings changed in the console | the setting ids |
| `host.started` / `host.stopping` | the host comes up / shuts down | version, whether GameStream is on |

Each event is one JSON object with a rising `seq`, a `ts_ms` timestamp and a `schema` version.
Fields are only ever added, never renamed.

```json
{ "seq": 42, "ts_ms": 1784227449526, "schema": 1,
  "kind": "stream.started",
  "stream": { "mode": "2560x1440@120", "hdr": true,
              "client": "Living Room TV", "fingerprint": "9f86d081…",
              "app": "steam:570", "plane": "native" } }
```

## Hooks

In the console, open **Automation** → **Add hook**, pick the event under **When** (a trailing `.*`
matches a whole domain) and **Run a command** or **Call a webhook** under **Then**. **Only for a
specific client or game** narrows it. **Save** asks for the console password.

The console writes `~/.config/punktfunk/hooks.json` (Windows:
`%ProgramData%\punktfunk\hooks.json`). You can edit that file by hand or `PUT` it to
`/api/v1/hooks`; changes apply from the next event.

```json
{
  "hooks": [
    { "on": "stream.started", "run": "/home/me/.config/punktfunk/scripts/on-stream.sh" },
    { "on": "client.connected",
      "filter": { "fingerprint": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" },
      "run": "kscreen-doctor output.HDMI-A-1.mode.3840x2160@60" },
    { "on": "pairing.pending",
      "webhook": "https://ha.local/api/webhook/punktfunk",
      "hmac_secret_file": "/home/me/.config/punktfunk/webhook-secret" }
  ]
}
```

| Field | Meaning |
|---|---|
| `on` | An event kind (`stream.started`) or a domain (`pairing.*`). Required, with `run`, `webhook` or both. |
| `run` | A shell command. |
| `webhook` | A URL the event JSON is POSTed to. |
| `filter` | Optional exact matches, all of which must hold: `fingerprint` (the device), `client` (its name), `plane` (`native` / `gamestream`), `app`. |
| `timeout_s` | Seconds before a command is killed with everything it started. 1–600, default 30. |
| `debounce_ms` | Minimum gap between firings of this hook. Default 0. |
| `hmac_secret_file` | Signs webhooks with `X-Punktfunk-Signature: sha256=<hex HMAC-SHA256 of the body>`. |

Filter on `fingerprint`, not `client`: names aren't unique and change on rename. The console's
device picker stores the fingerprint; `GET /api/v1/clients` lists both.

### Commands

A command runs as the host user (`sh -c` on Linux) with the event JSON on stdin and the event
flattened into `PF_EVENT_*` variables:

```sh
#!/bin/sh
# PF_EVENT_KIND=stream.started  PF_EVENT_STREAM_MODE=2560x1440@120  PF_EVENT_STREAM_HDR=true
# PF_EVENT_STREAM_CLIENT='Living Room TV'  PF_EVENT_STREAM_APP=steam:570  PF_EVENT_JSON='{…}'
[ "$PF_EVENT_KIND" = stream.started ] && makoctl mode -a do-not-disturb
```

- **Windows:** the host service runs the command as the signed-in user, never as SYSTEM, with no
  stdin and no `PF_*` variables. The event arrives as a JSON file path, appended as the last
  argument.
- **Linux:** every absolute path in the command, and each directory above it, must be owned by
  you or root and not group- or world-writable, or the host refuses the hook in its log. `~/…`
  paths and bare names like `makoctl` aren't checked.
- At most 8 hooks run at once; more are dropped and logged, never queued.

`PUNKTFUNK_ON_CONNECT_CMD` and `PUNKTFUNK_ON_DISCONNECT_CMD` in [`host.env`](/docs/configuration)
run a command on `client.connected` / `client.disconnected` without a `hooks.json`.

### Webhooks

The host POSTs the event JSON with verified TLS, follows no redirects and sends no Punktfunk
credentials. A webhook can't point at `localhost`, a loopback or a link-local address; use `run`
for a receiver on the host itself. LAN addresses and hostnames are fine.

With `hmac_secret_file` set, the host signs every POST, and skips it if it can't read the file.
`chmod 600` the file. Check the signature on the receiver:

```python
import hmac, hashlib
expected = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
ok = hmac.compare_digest(request.headers["X-Punktfunk-Signature"], expected)
```

### A hook that doesn't fire

One invalid entry disables every hook: the log says `hooks.json invalid — hooks disabled until
fixed` and names the entry. Read it on the console's **Troubleshooting** page or with
`journalctl --user -u punktfunk-host`. Log lines name a hook by its program or the webhook's
`scheme://host`, never its arguments or URL path. A `client` filter that never matches is logged
with the name the event carried.

## Per-app prep/undo

Attach `prep` steps to a [custom library entry](/docs/game-library#adding-a-game-by-hand) in
`library.json`, or to a GameStream `apps.json` entry. Each `do` runs before the title launches, and
the launch waits for it (up to 30 s per step). Each `undo` runs when the session ends, in reverse
order, even after a crash. A `do` that fails is logged and its `undo` skipped. The console's
Library form keeps prep steps but can't edit them.

Every step gets the session's mode: `PF_STREAM_WIDTH`, `PF_STREAM_HEIGHT`, `PF_STREAM_REFRESH`,
`PF_STREAM_HDR` (`1` / `0`), plus `PF_APP_ID` (a Punktfunk client's launch) or `PF_APP_TITLE`
(Moonlight). `undo` sees the same values as its `do`, so one entry serves every device. A
Windows host service can't pass these variables to its steps.

An `apps.json` entry; the `prep` array is the same in `library.json`:

```json
{ "id": 2, "title": "Cyberpunk 2077", "cmd": "steam -applaunch 1091500",
  "prep": [
    { "do":   "/home/me/.config/punktfunk/scripts/mode.sh do",
      "undo": "/home/me/.config/punktfunk/scripts/mode.sh undo" }
  ] }
```

```sh
#!/bin/sh
# mode.sh: cap the game at the device's refresh and light HDR only when negotiated.
set -eu   # fails loudly if a variable is missing, which also skips the undo
CONF="${XDG_CONFIG_HOME:-$HOME/.config}/MangoHud/MangoHud.conf"
case "${1:-}" in
do)   cp -f "$CONF" "$CONF.pf-bak"
      printf 'fps_limit=%s\n' "$PF_STREAM_REFRESH" >>"$CONF"
      [ "$PF_STREAM_HDR" = 1 ] && kscreen-doctor output.HDMI-A-1.hdr.enable || true ;;
undo) mv -f "$CONF.pf-bak" "$CONF"
      [ "$PF_STREAM_HDR" = 1 ] && kscreen-doctor output.HDMI-A-1.hdr.disable || true ;;
esac
```

### A launch on its own workspace

On Hyprland and sway, a library launch opens on an empty workspace of the streamed screen instead
of on top of your desk, and the host switches back when the game ends. A reconnect returns to that
game's workspace. KWin, GNOME, gamescope and Windows open it where the screen already is.

Set it per entry in `library.json`, beside `prep`. The keys act once, on the game's first window:

```json
{ "title": "Hades", "on_window": {
    "workspace": "own", "focus": true, "fullscreen": false, "move_to_stream_output": true
} }
```

| Key | Default | Does |
|---|---|---|
| `workspace` | `own` | `own` for an empty workspace, `current` for whatever the screen shows. The host-wide default is `launch_workspace` in `display-settings.json`. |
| `focus` | `true` | Raises the window. |
| `fullscreen` | `false` | Makes it full-screen. Most games set their own mode. |
| `move_to_stream_output` | `true` | Moves it to the streamed screen if it opened on one of your monitors. |

A compositor that refuses a step logs one line; the game still opens.

## Reacting to a game, not a stream

`stream.stopped` means the stream ended; `game.exited` means the game did. They differ on a
desktop stream with no game, or when the session outlives its game. Dim the lights on
`game.running`, when the game's process runs; send "ready" on `game.window`, when its window is on
screen, which can be half a minute later on a cold Proton prefix. `game.window` doesn't fire on
KWin without its window-list permission or on GNOME without the Punktfunk extension.

Scripts read `PF_EVENT_GAME_TITLE`, `PF_EVENT_GAME_APP` and, on `game.exited`, `PF_EVENT_REASON`:
`exited` when the player quit, `terminated` when the host closed it.

## The event stream

For a shell script, [`punktfunk-host ctl watch`](/docs/host-cli#ctl) prints one JSON object per
line, and resumes and reconnects by itself:

```sh
punktfunk-host ctl watch --kinds pairing.pending,stream.'*'
```

A `{"kind":"ctl.resync"}` line means events were missed: re-read the state you need.

To read Server-Sent Events directly, call the management API on the host with its token:

```sh
. ~/.config/punktfunk/mgmt-token   # sets PUNKTFUNK_MGMT_TOKEN
curl -Nk -H "Authorization: Bearer $PUNKTFUNK_MGMT_TOKEN" \
  "https://127.0.0.1:47990/api/v1/events?kinds=pairing.*,stream.*"
```

The token file is `PUNKTFUNK_MGMT_TOKEN=<token>`, so source it; `cat` into the header gets 401.

- Each frame carries `id:` (the `seq`), `event:` (the kind) and `data:` (the event JSON).
- `?kinds=` filters by kind or `domain.*`, comma-separated.
- Reconnect with `Last-Event-ID` or `?since=<seq>` to replay what you missed from the last ~1024
  events. If they're gone, an `event: dropped` frame comes first: re-read `/status`, `/clients`
  and the rest.
- Without a cursor the stream replays what it holds, then sends `event: live`. Stay quiet until it
  arrives.
- At most 32 streams at once; more get 503.

## Scripts and plugins

Ready-made plugins sync ROMs or a Playnite library, or hand a USB device to the host: see
[Plugins](/docs/plugins). To write your own, see
[Writing plugins](/docs/developers/writing-plugins) and the
[TypeScript client](/docs/developers/management-api#typescript-sdk). The full API is at
[`/api/docs`](/api) on your host.

A plugin under the runner gets a limited token. It can't register hooks, administer pairing, use
the plugin store or the update endpoints, or read the logs; those routes answer 403. A script that
should approve pairings, say by watching `pairing.pending` and calling
`POST /api/v1/native/pending/{id}/approve` when you tap yes on your phone, needs the full token:
set `PUNKTFUNK_MGMT_TOKEN` on its unit.

## Recipe: full controller passthrough (VirtualHere)

To give the host the real device (DualSense gyro and adaptive triggers, a racing wheel, a HOTAS),
hand it over USB with [VirtualHere](https://www.virtualhere.com/), a commercial product sold
separately.

1. Use the [VirtualHere plugin](/docs/plugins#virtualhere-usb-passthrough). It finds the device by
   name, hands it back if anything crashes, and tells you which side is misconfigured.
2. On the couch client, turn off **Forward controllers**
   ([Client settings](/docs/client-settings#input)). Otherwise games see two controllers, and on
   Linux and Windows VirtualHere can't take a pad the client holds.

Without the plugin, two hooks do the basic job. The address comes from `vhclientx86_64 -t LIST`;
it changes when the couch reboots, and an abnormal end leaves the device on the host:

```json
{ "hooks": [
    { "on": "stream.started", "run": "vhclientx86_64 -t \"USE,couch-deck.11\"" },
    { "on": "stream.stopped", "run": "vhclientx86_64 -t \"STOP USING,couch-deck.11\"" }
] }
```
