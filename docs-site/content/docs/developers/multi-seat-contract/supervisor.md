---
title: The supervisor
description: The seat supervisor's control channel and commands, the environment it starts a seat host with, and the check that a seat can stream.
---

Talk to the seat supervisor, or read what it hands a seat host at start. The supervisor is
`crates/host/pf-seats`. A platform plugs in through `PlatformBackend` (`src/backend.rs`), whose
doc comments are the backend's half of the contract. The host reads its half in one place,
`pf_paths::seat` (`crates/host/pf-paths/src/seat.rs`).

## Where it runs and listens

| | Windows | Linux |
|---|---|---|
| Runs as | The `PunktfunkHost` service, LocalSystem in session 0, beside the console host | The root daemon `punktfunk-seats` (`punktfunk-seats.service`) |
| Endpoint | `\\.\pipe\punktfunk-seats` | `/run/punktfunk/seats.sock`, `0660 root:punktfunk-door` |
| Who may connect | SYSTEM and Administrators; remote clients are refused | root and the `punktfunk` user, by the peer's credentials. Once the owner's row is adopted, the owner's uid too, for `install_flatpak` and `remove_flatpak` only |
| Ledger | `%ProgramData%\punktfunk\seats\` | `/var/lib/punktfunk-seats/seats/` |
| The client waits | 60 s for `list`, 30 min for `enable` with `prepare`, 5 min for the rest | 30 min for a Flatpak change, 180 s for the rest, 30 s more for an `unlock` |

## Requests and answers

Each connection carries one request, and at most eight are served at once. A frame is a four-byte
big-endian length, then JSON, 64 KiB at most. A request is
`{"schema_version": 1, "command": {"type": "<command>", …}}`; the answer is
`{"status": "success", "schema_version": 1, "result": {"kind": "<result>", …}}`, or
`"status": "error"` with `error: {code, message}`. A refusal from the platform backend also
carries its own code, such as `credential_wrong`, as `error.refusal`, and `message` leads with
`<refusal>: `. A reader that finds no field takes the code from the start of `message`. The types are `Request`, `Command` and
`Response` in `crates/host/pf-seats/src/ipc.rs`. A request carries no credential of its own:
`adopt`, `adopt_user`, `credential` and `unlock` carry an account's password, which the supervisor
seals (Windows) or checks once (Linux) and forgets. On Linux a peer or command the socket refuses
is closed unanswered.

## Commands

| Commands | Where |
|---|---|
| `list`, `create`, `start`, `stop`, `delete`, `doctor`, `seating`, `enable`, `disable` | Both. On Linux `enable` answers like `seating`, and `disable` is refused: seats are on while the daemon runs. |
| `adopt`, `credential` | Windows: [a seat on an existing account](/docs/developers/multi-seat-contract/windows#a-seat-on-an-existing-account) |
| `adopt_owner`, `install_flatpak`, `remove_flatpak`, `install_emulator`, `remove_emulator` | Linux: [the owner's row](/docs/developers/multi-seat-contract/door), box-wide Flatpaks and emulators |
| `pads`, `lend`, `take_back` | Lent controllers, on [Linux](/docs/developers/multi-seat-contract/linux#lent-controllers) and on [Windows](/docs/developers/multi-seat-contract/windows#lent-controllers). Each answers `pads`. |
| `adopt_user`, `unlock`, `unlock_state`, `challenge`, `left`, `sign_out`, `devices` | Linux: [a seat on a person's own account](/docs/developers/multi-seat-contract/own-account) |

## The environment a seat host is started with

The supervisor starts an ordinary `punktfunk-host serve` with these. Don't set them by hand; set
them together. The host logs a warning at startup for each one that is set and ignored.

| Variable | Value | Rule |
|---|---|---|
| `PUNKTFUNK_SEAT_SESSION` | `1` | Marks a seat host. Unset or any other value is the console host. A seat host is never the door: `--door` is ignored on it. |
| `PUNKTFUNK_SEAT_ID` | 32 lowercase hexadecimal characters | Required with `PUNKTFUNK_SEAT_SESSION=1`. Missing or any other form: a seat host with `PUNKTFUNK_TRUST_DIR` doesn't start, and none mints audio devices. Ignored without `PUNKTFUNK_SEAT_SESSION=1`. |
| `PUNKTFUNK_SEAT_DISPLAY_SLOT` | Windows: `12`–`15` | The seat's connector. Not a number, out of range, or the marker absent: the host refuses every virtual-display session. |
| `PUNKTFUNK_TRUST_DIR` | Windows: the box's config directory. Linux: the seat's [trust copy](/docs/developers/multi-seat-contract/linux#the-trust-copy) | The seat reads the box's pairing store, `profiles.json`, per-device display overlays and `seat-defaults.json` from here, read only, and follows their changes. A relative path is ignored. |
| `PUNKTFUNK_PAIRING` | `refused` | Devices pair with the box. A knock is refused and no PIN window opens. A seat host refuses pairing with or without it. |
| `PUNKTFUNK_LIBRARY_DIR` | Windows: the box's config directory. Linux: the seat's trust copy, on every seat but the owner's | The seat reads the box's library from here, read only: `library*.json`, `library-metadata/`, the plugin grants and the firmware plugins staged; on Windows also the plugin manifests its entries launch through and the managed emulators. Play stats stay in the seat's own directory. A relative path is ignored. |
| `PUNKTFUNK_SEAT_OWN_ACCOUNT` | `1` on a seat on a person's own account, Windows and Linux | The host's one sign that it runs as a real person. Unset on a made seat and on the Linux owner's row. |
| `PUNKTFUNK_SEAT_OWNER` | Linux: `1` on the owner's row | The seat host is the owner's own, and also serves the owner and the profiles that share the owner's desktop. |
| `PUNKTFUNK_SEAT_HEADLESS` | Linux: `1` on the owner's row, set by the runner, not the environment file | The host runs in the row's headless session, not at the desk. |
| `PUNKTFUNK_SEAT_STEAM` | Windows: the seat's own `steam.exe`, when the box has Steam | Steam runs once per IPC name per machine, so a seat starts Steam from here only, always with `-master_ipc_name_override pfseat<seat id>`. The supervisor copies the box's client into the seat account's profile at the seat's first start; until the file exists, the seat's Steam titles don't start. Unset or relative: no Steam on the seat. |

It also sets these ordinary [host settings](/docs/configuration), so seats don't collide. A seat
presents its own certificate and honours the box's pairings and grants.

| Variable | Supervisor's value |
|---|---|
| `PUNKTFUNK_CONFIG_DIR` | A per-seat directory. Linux: the seat home's `.config/punktfunk`, `seats/<id>/config` on a person's row, unset on the owner's row |
| `PUNKTFUNK_MGMT_BIND` | `127.0.0.1:<per-seat port>` |
| `PUNKTFUNK_NATIVE_PORT` | a per-seat port |
| `PUNKTFUNK_HOST_NAME`, `PUNKTFUNK_GAMESTREAM` | the seat's name, and `0`. Unset on the Linux owner's row |
| `PUNKTFUNK_MDNS` | `0` |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | Windows: `follow_default` |
| `PUNKTFUNK_WEBTRANSPORT` | Linux: `1` while the box's Seat defaults stream to browsers (`browser`), else `0` |
| `PUNKTFUNK_WEBTRANSPORT_PORT`, `PUNKTFUNK_WEBTRANSPORT_ORIGINS` | Linux, with `PUNKTFUNK_WEBTRANSPORT=1`: the seat's browser port, UDP 9783–9786, and the box's `browser_origins` |
| `PUNKTFUNK_MGMT_TOKEN` | Linux: fresh at each start, and written to `seats/hosts/<id>/mgmt-token` so the box host reaches the seat's loopback API |

The supervisor must keep each seat's RDP session active. A seat host runs outside the console
session by design, and display activation fails while its session is inactive.

## Checking a seat can actually stream

Run this inside the seat's session:

```
punktfunk-host spike --source virtual --seconds 5
```

It creates a virtual display, captures it and encodes in the display driver. It exits non-zero on
any gap: no driver, no captured frame, no GPU encoder (a software encoder counts as a failure), or
no encoded output. The supervisor runs it before it reports a seat healthy. Add `--hdr` to also
require the 10-bit path.
