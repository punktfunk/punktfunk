---
title: Host events
description: Every event the host publishes, the fields each one carries, and the event stream that scripts and plugins read.
---

The host publishes an event each time something happens on it, such as a device connecting or a
game exiting: look one up here with the fields it carries, or read them live from a script. To
run a command or call a webhook on an event, see [Events & hooks](/docs/automation#hooks).

## The events

| Kind | Fires when | Carries |
|---|---|---|
| `client.connected` / `client.disconnected` | a device connects / goes away | name, fingerprint, plane (`native`, `gamestream`, or `web` for a browser), the `profile` it plays as (`id`, `display_name`; not on `gamestream`); disconnect adds `reason`: `quit`, `timeout` or `error` |
| `session.started` / `session.ended` | a session starts / ends | session id, client, fingerprint, plane, mode (`3840x2160@120`), HDR, `profile`. `session.ended` adds a `summary` (duration, codec, bitrate span, frames, bring-up time, and `ended`: `local`, `game_exited`, `host_ended`, `host_error`, `lost`, `stopped_by_operator` or `stolen`), the same shape as `GET /api/v1/session/last` |
| `stream.started` / `stream.stopped` | video starts / stops | mode, HDR, client, fingerprint, launched app, plane, `profile` |
| `game.launching` | the host is about to start a launched game; not when it picks up one still running | app id, title, store, client, fingerprint, plane, preset, `profile` |
| `game.running` | a launched game's own process runs (not just its launcher) | app id, title, store, client, fingerprint, plane, `profile` |
| `game.window` | the game's window reaches the screen, often 5–40 s after `game.running` | the same, plus the window's `title` and `app_id` |
| `game.exited` | a launched game is gone | the same, plus `reason`: `exited` (the player quit or it crashed) or `terminated` (the host closed it, per [these settings](/docs/when-a-game-ends)) |
| `pairing.pending` | an unpaired device knocks, once per device | name, fingerprint, plane (`native` for a browser too: it pairs into the same store) |
| `pairing.completed` / `pairing.denied` | a pairing is approved / denied | name, fingerprint, plane |
| `access.granted` / `access.changed` | you pick a device's access when pairing / edit it later | device, `grants` bits, `expires_unix` (absent: no expiry) |
| `access.expired` | a streaming device's access runs out | device |
| `display.created` / `display.released` | a virtual display is created / kept displays are released | backend and mode / count |
| `library.changed` | the game library changes | `source`: `manual` or the provider id |
| `downloads.changed` | a title's download starts, pauses or ends, or its files are removed | `app`, `title`, `state` (`queued`, `paused`, `done`, `failed`, `cancelled`, `removed`, …) |
| `emulators.changed` | a managed emulator is installed or removed | emulator `id` |
| `update.available` | a newer release is found, once per version | version, channel, install kind (`apt`, `windows-installer`, …) |
| `update.applied` | the updated host first starts | `from`, `to` |
| `action.invoked` | a [host power](/docs/host-power) action or **Next monitor** is accepted, or fails | `id` (`power.sleep`, `power.reboot`, `power.shutdown`, `host.restart`, `display.next`), device (absent for the console), `outcome` |
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
              "app": "steam:570", "plane": "native",
              "preset": { "id": "3f9a0c11e2b4", "name": "Docked" } } }
```

`preset` names the settings preset the device streamed with; it is absent when the device used
its plain settings, and on Moonlight. `profile` names who plays (`id`, `display_name`); it is
absent on Moonlight.

## Reacting to a game, not a stream

`stream.stopped` means the stream ended; `game.exited` means the game did. They differ on a
desktop stream with no game, or when the session outlives its game. Dim the lights on
`game.running`, when the game's process runs; send "ready" on `game.window`, when its window is on
screen, which can be half a minute later on a cold Proton prefix. `game.window` doesn't fire on
KWin without its window-list permission or on GNOME without the Punktfunk extension.

[Scripts](/docs/automation#commands) read `PF_EVENT_GAME_TITLE`, `PF_EVENT_GAME_APP` and, on
`game.exited`, `PF_EVENT_REASON`: `exited` when the player quit, `terminated` when the host closed
it.

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

- Each frame carries `id:` (the `seq`), `event:` (the kind) and `data:` (the event JSON).
- `?kinds=` filters by kind or `domain.*`, comma-separated.
- Reconnect with `Last-Event-ID` or `?since=<seq>` to replay what you missed from the last ~1024
  events. If they're gone, an `event: dropped` frame comes first: re-read `/status`, `/clients`
  and the rest.
- Without a cursor the stream replays what it holds, then sends `event: live`. Stay quiet until it
  arrives.
- At most 32 streams at once; more get 503.
