---
title: Events & hooks
description: Run a command or call a webhook when the host does something, or prepare the host for one game.
---

Make the host do something when a stream starts, a device knocks or a game exits: turn on
Do-Not-Disturb, send a phone notification, pause downloads, switch the TV's HDR. The host publishes
an **event** for each of these, and you react with:

- **Hooks**: run a command or call a webhook. Set them on the console's **Automation** page or in
  `hooks.json`.
- **Per-app prep/undo**: commands around one title's launch.
- **The event stream**: `GET /api/v1/events`, for scripts and plugins.

[Host events](/docs/host-events) lists every event, the fields it carries, and how to read the
event stream.

Hooks only observe. They can't veto or delay a connection, a stream or a pairing, and they never
run in the streaming path. The one exception is a launch: a hook with `hold` set on
`game.launching` runs before the game starts, and the game waits for it.

## Hooks

In the console, open **Automation** → **Add hook**, pick the event under **When** (a trailing `.*`
matches a whole domain) and **Run a command** or **Call a webhook** under **Then**. **Only for a
specific client or game** narrows it. **Save** asks for the console password.

The console writes `~/.config/punktfunk/hooks.json` (Windows:
`%ProgramData%\punktfunk\hooks.json`). You can edit that file by hand or `PUT` it to
`/api/v1/hooks`; changes apply from the next event.

A profile on an existing account runs no hooks, on Linux and Windows: its hooks would run as that
person. Its seat refuses a new hook, and skips any it already has.

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
| `on` | An [event kind](/docs/host-events#the-events) (`stream.started`) or a domain (`pairing.*`). Required, with `run`, `webhook` or both. |
| `run` | A shell command. |
| `webhook` | A URL the event JSON is POSTed to. |
| `filter` | Optional exact matches, all of which must hold: `fingerprint` (the device), `client` (its name), `plane` (`native`, `gamestream`, `web`), `app`, `preset` (its name or id), `profile` (its id). |
| `timeout_s` | Seconds before a command is killed with everything it started. 1–600, default 30. |
| `debounce_ms` | Minimum gap between firings of this hook. Default 0. |
| `hold` | Only with `on: game.launching`: the game waits for this hook, up to `timeout_s`. |
| `elevated` | Windows: runs `run` as administrator. See [Run as administrator](#run-as-administrator). |
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

- **Windows:** the host service runs the command as the signed-in user, never as SYSTEM, with the
  `PF_*` variables but no stdin. The event JSON arrives as a file path, appended as the last
  argument.
- **Linux:** every absolute path in the command, and each directory above it, must be owned by
  you or root and not group- or world-writable, or the host refuses the hook in its log. `~/…`
  paths and bare names like `makoctl` aren't checked.
- At most 8 hooks run at once; more are dropped and logged, never queued.

### Run as administrator

On Windows, `"elevated": true` (**Run as administrator** in the console) runs a command with the
signed-in user's full administrator rights, without a UAC prompt. Turn it on once per host, from
an administrator PowerShell:

```powershell
punktfunk-host hooks elevated on
```

Until then the console can't save an elevated hook, and the host skips one written by hand: the
console password alone never grants administrator rights. A standard user's session can't elevate
either; the host skips the hook and logs why. `hooks elevated off` turns it off again.

On Linux, give the script its own sudoers rule and run it with `sudo -n`, which fails at once
instead of waiting for a password. Keep the script owned by root: whoever can edit it runs as root.

```sh
# sudo visudo -f /etc/sudoers.d/punktfunk
me ALL=(root) NOPASSWD: /usr/local/bin/tv-hdr.sh
```

The hook's `run` is then `sudo -n /usr/local/bin/tv-hdr.sh`.

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

### Hold the launch

A hook on `game.launching` with `hold` set runs before the game starts, and the launch waits
for it: the command until it exits, the webhook until it answers. Each gets `timeout_s`; past
it, or on a failure, the game starts anyway and the log says why. The device shows its launch
screen meanwhile. `debounce_ms` doesn't apply, and a held hook never runs a second time on its
own.

```json
{ "on": "game.launching", "filter": { "preset": "Docked" }, "hold": true, "timeout_s": 20,
  "run": "/home/me/.config/punktfunk/scripts/tv-profile.sh" }
```

Plugins hold the same stage: see [Writing plugins](/docs/developers/writing-plugins#hold-a-launch).

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
order, even when it ends in an error. A `do` that fails is logged and its `undo` skipped. The
console's Library form keeps prep steps but can't edit them.

Every step gets the session's mode: `PF_STREAM_WIDTH`, `PF_STREAM_HEIGHT`, `PF_STREAM_REFRESH`,
`PF_STREAM_HDR` (`1` / `0`), plus `PF_APP_ID` (a Punktfunk client's launch) or `PF_APP_TITLE`
(Moonlight), and `PF_PRESET_ID` / `PF_PRESET_NAME` when the device streamed with a preset. `undo` sees the same values as its `do`, so one entry serves every device.
On Windows, `"elevated": true` on a step runs its `do` and `undo` as administrator, once it is
[turned on](#run-as-administrator).

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

On Hyprland, sway, niri and Umbriel, a library launch opens on an empty workspace of the streamed screen instead
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

## Scripts and plugins

Ready-made plugins sync ROMs or a Playnite library, or hand a USB device to the host: see
[Plugins](/docs/plugins) and the
[controller passthrough recipe](/docs/plugin-setup#recipe-full-controller-passthrough-virtualhere). To write your own, see
[Writing plugins](/docs/developers/writing-plugins) and the
[TypeScript client](/docs/developers/management-api#typescript-sdk). The full API is at
[`/api/docs`](/api) on your host. A plugin's token can't register hooks or approve pairings; a
script that does needs the admin token ([Credentials](/docs/developers/management-api#credentials)).
