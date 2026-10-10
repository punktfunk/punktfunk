---
title: Multi-seat contract
description: How the host runs seats — on Windows the supervisor in the service, its pipe, the seat keeper and the connector reservation; on Linux the seat daemon, its socket and unit, and the owner's row behind the door; and the variables a seat host starts with.
---

The rules between the host and its seat supervisor, which runs one host per seat so several people
can play on one box. A normal install has one host on the console session, and none of this applies
to it. Windows seats need Windows Server and a CAL per seat; see
[Windows host → Good to know](/docs/windows-host#good-to-know). Linux seats are
[further down](#linux-seats).

## The supervisor

The `PunktfunkHost` service runs the supervisor beside the console host, as LocalSystem in session
0. It owns the seat accounts, the Windows sessions, RDP and the seat ledger in
`%ProgramData%\punktfunk\seats\`. Seat hosts run in jobs of its own, so a console logon or a console
host restart leaves them running; stopping the service logs every seat account off.

- **The pipe.** `\\.\pipe\punktfunk-seats` admits SYSTEM and Administrators only and refuses
  remote clients. Each connection carries one request: a four-byte big-endian length, then JSON,
  64 KiB at most. Commands are `list`, `create`, `start`, `stop`, `delete`, `doctor`, `seating`,
  `enable` and `disable`.
- **The keeper.** `punktfunk-seat-keeper.exe`, beside `punktfunk-host.exe`, holds one seat's
  loopback RDP session open. The supervisor hands it the seat's credentials on stdin, never on the
  command line. Run `punktfunk-seat-keeper trust` from an elevated prompt once, before the first
  seat: it records the RDP certificate the keeper will accept. Turning seats on does this too.
- **The seats group.** Every seat account is in the local group `punktfunk-seats`, rejoined at
  each start. The host denies that group on `%ProgramData%\punktfunk\ingest`, so a seat can't
  replace the owner's Playnite library. The doctor's `account_policy` row reports the membership.
- **Seats on and off.** Without the reservation marker below the ledger is still listed, and
  every start answers `seats_off`. [Turn seats on and off](#turn-seats-on-and-off) writes the marker.

## The reservation marker

```
HKLM\SOFTWARE\Punktfunk\Seats
```

The key's existence is the signal; its values are ignored. It splits the virtual-display
connectors:

| Host | Without the key | With the key |
|---|---|---|
| Console host | 0–15 | 0–11 (0 serves clients without an identity) |
| Seat host | refuses every virtual-display session | exactly one of 12–15, from `PUNKTFUNK_SEAT_DISPLAY_SLOT` |

- Only an elevated operator writes the key. It lives under `HKLM` so a seat host, an ordinary
  process, can't reserve connectors from its own environment.
- When the driver places a monitor outside the host's range, the host removes it and refuses the
  session.

## Turn seats on and off

The management API turns seats on from the box's console. These routes take the management token,
never a paired device or a plugin.

| Request | Does |
|---|---|
| `GET /api/v1/profiles/seating` | Returns `enabled`, `platform` (`windows`, `linux` on a door, or `other`) and `checks`: Windows Server, the Remote Desktop Session Host role, licensing and a graphics card. |
| `PUT /api/v1/profiles/seating` with `{"enabled": true, "allow_rdp_from_network": false}` | Turns seats on. |
| `PUT /api/v1/profiles/seating` with `{"enabled": false}` | Stops every seat and turns seats off. The accounts stay. |
| `GET /api/v1/profiles/doctor` | Returns the supervisor's report. |

A failed check changes nothing: the answer is 200 with `enabled: false` and the check as an
`error`. When every check passes, turning on:

1. Writes the reservation marker.
2. Installs `pf_vdisplay_seats.inf` from `staging\pfvdisplay\` beside `punktfunk-host.exe`.
3. Turns the Remote Desktop listener on if it was off, and limits the **Remote Desktop**
   firewall rules to `127.0.0.0/8` unless `allow_rdp_from_network` is `true`.
4. Records the Remote Desktop certificate, as `punktfunk-seat-keeper trust` does.

Turning off puts the listener and the firewall scope back and removes the marker. The seat
display driver stays in the driver store. Off Windows and off a door, `GET` answers `enabled: false`
with `platform: other`, and `PUT` answers 409.

## The environment a seat host is started with

The supervisor starts an ordinary `punktfunk-host serve` with these. Don't set them by hand; set
them together.

| Variable | Value | Rule |
|---|---|---|
| `PUNKTFUNK_SEAT_SESSION` | `1` | Marks a seat host. Unset or any other value is the console host. |
| `PUNKTFUNK_SEAT_ID` | 32 lowercase hexadecimal characters | Required with `PUNKTFUNK_SEAT_SESSION=1`. Missing or any other form: the host mints no audio devices. |
| `PUNKTFUNK_SEAT_DISPLAY_SLOT` | `12`–`15` | The seat's connector. Not a number, out of range, or the marker absent: the host refuses every virtual-display session. |
| `PUNKTFUNK_TRUST_DIR` | the box's config directory | The seat reads the box's pairing store, `profiles.json`, per-device display overlays and `seat-defaults.json` from here, read only, and follows their changes. A relative path is ignored. |
| `PUNKTFUNK_PAIRING` | `refused` | Devices pair with the box. A knock is refused and no PIN window opens. |
| `PUNKTFUNK_LIBRARY_DIR` | the box's config directory (Windows only) | The seat reads the box's library from here, read only: `library*.json`, `library-metadata/` and the plugin manifests and grants its entries launch through. Play stats stay in the seat's own directory. A relative path is ignored. |
| `PUNKTFUNK_SEAT_STEAM` | the seat's own `steam.exe` (Windows only, when the box has Steam) | Steam runs once per IPC name per machine, so a seat starts Steam from here only, always with `-master_ipc_name_override pfseat<seat id>`. The supervisor copies the box's client into the seat account's profile at the seat's first start; until the file exists, the seat's Steam titles don't start. Unset or relative: no Steam on the seat. |

It also sets these ordinary [host settings](/docs/configuration), so seats don't collide. A seat
presents its own certificate and honours the box's pairings and grants.

| Variable | Supervisor's value |
|---|---|
| `PUNKTFUNK_CONFIG_DIR` | a per-seat directory |
| `PUNKTFUNK_MGMT_BIND` | `127.0.0.1:<per-seat port>` |
| `PUNKTFUNK_NATIVE_PORT` | a per-seat port |
| `PUNKTFUNK_HOST_NAME` | the seat's name |
| `PUNKTFUNK_GAMESTREAM` | `0` |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `follow_default` |

The supervisor must keep each seat's RDP session active. A seat host runs outside the console
session by design, and display activation fails while its session is inactive.

## What the host does differently on a seat

- **The seat contract, not a display policy.** A seat that is not the owner's own reads no
  `display-settings.json` of its own. Its desktop is one virtual screen at the occupant's mode: it
  never mirrors, keeps lit, powers off or PnP-disables a real monitor, never routes a game to a
  session of its own, and a second device shares the occupant's screen. Of a device's display
  settings in the box console only the largest mode and the scale follow it onto a seat. Linger,
  topology and identity are the unconfigured host's. `GET /display/settings` lists nothing in
  `enforced` and only `max_mode` and `scale` per device; every display or session settings write
  answers 409.
- **The Seat defaults.** The box writes `seat-defaults.json` into its config directory at start and
  after each settings change: `end_on_game_exit` (a seat's stream ends when its game exits) and
  `max_mode` (the largest mode a seat grants a device without a cap of its own). The seat re-reads
  it when it changes, so a change reaches the seat's next session. The box itself applies the
  other two: **Seats kept warm** and **Stop idle seats**.
- **The box's trust, its own key.** The seat never writes the pairing store or profiles. A grant
  changed in the box console applies to the seat's next check. It mints and keeps its own identity
  in `PUNKTFUNK_CONFIG_DIR`, never the box's. The supervisor records its certificate's SHA-256 in
  the ledger while it runs. The box's `Redirect` carries that pin (field 7) and `enumerate` lists
  it as `seat.pin`, so a client pins the seat through the box it already trusts. A seat whose pin
  the box doesn't hold yet is unavailable. Sleep, restart and shut down are refused: the box's own
  host is the one that does them.
- **The box's library, on Windows.** A Windows seat runs no plugins. It lists and launches the
  box's titles as its own user, and answers every library change with 409. Titles from sources
  that belong to one account (Playnite, Game Bar, Amazon, itch, Hydra) are left out.
- **One profile.** The seat serves the profile whose seat it is. A connect that names another seat
  profile is refused as an unknown profile; one that names none plays as this seat's profile.
- **One connector, one lock.** A seat host creates its virtual display on its own connector only
  and holds the mutex `Global\punktfunk-vdisplay-manager-seat-<slot>`; the console host holds
  `Global\punktfunk-vdisplay-manager`. Hosts on different connectors start independently. A second
  host on the same connector can't open the display driver.
- **Driver calls are per process.** Every host sees, changes and clears only the monitors it
  created. A crashed host's monitors depart when its handles close or it misses the driver
  watchdog, so no host reaps a neighbour's.
- **Launches stay in its session.** Games, store launches and hook commands run as the signed-in
  user of the host's own Windows session, not the console's.
- **Its audio devices are its own.** It mints `Punktfunk Speakers [seat <id>]` and
  `Punktfunk Microphone [seat <id>]`, matches them by a marker derived from the seat id, and never
  adopts a neighbour's. It captures its own speakers and never changes the box's default playback
  or recording device.
- **Its virtual mouse is its own.** The resident HID mouse that makes Windows draw a cursor into
  the stream is `pf_mouse_<slot>`, with mailbox `Global\pfmouse-boot-<slot>`. The console host uses
  index `0`. Only the console host sends input through it: HID input reaches the console session, so
  a seat host injects with `SendInput`.
- **Gamepad indices are shared.** The box has 16 pad indices; each host takes the lowest one that
  no other host's pad holds.
- **No status tray.** A seat host doesn't start or supervise one; the supervisor is the control
  surface.

## Checking a seat can actually stream

Run this inside the seat's session:

```
punktfunk-host spike --source virtual --seconds 5
```

It creates a virtual display, captures it and encodes in the display driver. It exits non-zero on
any gap: no driver, no captured frame, no GPU encoder (a software encoder counts as a failure), or
no encoded output. The supervisor runs it before it reports a seat healthy. Add `--hdr` to also
require the 10-bit path; a seat's session doesn't pass that yet, so seats stream SDR.

## The seat display driver is a second package

Windows' terminal-services stack starts a seat's display with whichever driver claims the hardware
id `RdpIdd_IndirectDisplay`. Claiming it **takes over every RDP session on the machine**, ordinary
Remote Desktop included, so the build produces two packages from the same `.inx` and signing key:

| Package | Claims | Installed by |
|---|---|---|
| `pf_vdisplay.{inf,cat}` | the console display device, `Root\pf_vdisplay` | every punktfunk install |
| `pf_vdisplay_seats.{inf,cat}` | `RdpIdd_IndirectDisplay` only | the console, when seats are turned on |

A machine that never turned seats on never claims the id.

**Losing the claim is silent.** Windows' own `rdpidd.inf` ranks the same as ours (`0x00FF0000`), so
the newer `DriverVer` date wins. A seat on Microsoft's adapter still logs in and never streams.
Check it with at least one seat connected, since seat devnodes exist only while a session does:

```
powershell -File check-seat-display.ps1
```

[`check-seat-display.ps1`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/check-seat-display.ps1)
reports the driver on each live seat display and exits 1 if any isn't ours or none is live. It
warns when our driver date isn't newer than `rdpidd.inf`'s, and on a client edition.

## Seat audio needs a driver on disk

A seat usually has no sound card, so the host mints a render endpoint per seat by binding Valve's
Remote Play streaming drivers. `SteamStreamingSpeakers.inf` and `SteamStreamingMicrophone.inf` must
be bound to a device already, or sit in
`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\<x64|arm64>\`. Steam never has to run. Without
them a seat streams video with no audio.

```
powershell -File check-seat-audio.ps1
```

[`check-seat-audio.ps1`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/check-seat-audio.ps1)
exits 1 when neither is present. A virtual cable is no substitute: the host never loopback-captures
a cable, and `PUNKTFUNK_MIC_DEVICE` only pins the microphone.

## Linux seats

On Linux the supervisor is the root daemon `punktfunk-seats` (`punktfunk-seats.service`), not part
of the box host. It keeps its ledger in `seats/` under the box host's config directory,
`/var/lib/punktfunk`, and starts one `punktfunk-seat@<user>.service` per seat. A seat is a
system-range user, `pf-seat-<n>`, with a logind session of its own, a headless compositor
(`kwin_wayland`, else gamescope) and a stock `punktfunk-host serve`, all run by
`/usr/libexec/punktfunk/seat-session`.

- **The package.** `punktfunk-seats` is its own package: an rpm and a deb the host recommends, a
  pacman package the host lists as an optional dependency, and `services.punktfunk.seats.enable` on
  NixOS. The rpm, deb and pacman packages install the units without enabling them.
- **The socket.** `/run/punktfunk/seats.sock` carries the same frames and commands as the Windows
  pipe. It answers root and the `punktfunk` user, judged by the peer's credentials; any other peer
  is closed unanswered. `enable` answers like `seating`, and `disable` is refused: seats are on while
  the daemon runs. `punktfunk-seats list`, `create <name>`, `adopt-owner <user>`,
  `start|stop|delete <id>` and `doctor` send the same requests.
- **The user.** Its comment is `punktfunk-seat=<id>`, and the daemon touches or deletes only an
  account whose comment matches exactly. The home is `seats/<id>/home`. It joins `render` and
  `punktfunk-games`, the shared games folder's group, which the owner joins too. Never `punktfunk`,
  which may power the box off, nor `input`, which reads every input device on the box.
- **The environment.** The variables in the tables above, minus the display slot,
  go into `/run/punktfunk/seats/<user>.env` (root `0600`), which systemd reads as root.
  `PUNKTFUNK_CONFIG_DIR` is `<home>/.config/punktfunk`. Each start adds a fresh
  `PUNKTFUNK_MGMT_TOKEN` and writes the same line to `seats/hosts/<id>/mgmt-token`, owned by the
  `punktfunk` user (root without one), so the box host reaches the seat's loopback API. Every seat
  user shares `punktfunk-games`, so no secret relies on group read.
- **The trust copy.** The seat reads `trust/<id>/`, not the box directory:
  `punktfunk1-paired.json`, `profiles.json`, `display-settings.json`, `seat-defaults.json` and
  `profiles/`,
  `root:<seat user>` `0640`. The daemon recopies a file within 2 seconds of its change while the
  seat runs. The box's key never crosses, and a copy left by an older install is removed.
- **Games.** `games/steamapps/` is one library every seat writes (group `punktfunk`, setgid,
  default ACL). Each seat's own `compatdata`, `shadercache` and `downloading` under `seats/<id>/`
  are bind-mounted over the shared ones, so prefixes never cross. Everything that needs the
  binds must descend from the unit; a process started through `systemd-run --user` or a user
  service doesn't see them. The daemon repairs the ACL mask under `games/` after a seat stops and
  every 10 minutes, because a file created with mode `0644` is otherwise read-only to the other
  seats. A seat other than the owner's sees an empty `/run/media`: Steam in Game Mode adopts
  every mounted drive with a `steamapps` as a library, and the owner's drives are read-only to it.
- **Stopping.** `pam_systemd` moves the runner into the session's scope, so stopping the unit
  can't reach it. The runner ends its children on `SIGTERM`, and `seat-reap` ends the session
  by the id the runner recorded.
- **Pads.** A seat user can't open `/dev/uinput`: it is the console user's. A seat host asks
  the daemon's pad broker, `/run/punktfunk/pads.sock`, for a pad by kind, and the daemon builds
  it from its own tables, keeps the kernel fd and hands the seat one end of a relay socket. The
  seat sends its `input_event` frames down it and reads rumble back; the pad lives until the seat
  hangs up. The socket admits a running seat's user only (the owner's row included), eight pads
  per seat. The daemon stamps each pad's `phys` as `punktfunk-seat:<account>/<index>`, and
  `65-punktfunk-seats.rules` runs `punktfunk-seats fence` on it: the node becomes that user's,
  `0600`, on a seat of its own, so neither the box's own Steam nor another seat sees it. A box
  without the daemon gives a seat no pad; the seat host says so.
- **Game Mode.** The seat host keeps the seat's mode, `desktop` or `game`, in `seat-session` in
  its config directory, and starts in the one it ran last. It owns
  `com.steampowered.SteamOSManager1` on the seat's session bus and serves `SessionManagement1`,
  which `steamosctl` and Steam call; `steamos-session-select` and `steamos-desktop-return`, first
  on the seat's `PATH`, call it too. A switch never reaches the box's login. The runner masks the
  seat user's own `steamos-manager.service` for the session.

  The desktop keeps running in Game Mode. A switch moves the seat's own live sessions in place
  between its KWin output and a gamescope the host spawns with `steam -gamepadui -steamos3`, so the
  stream continues; a device joined to the screen is ended and joins the new one on reconnect. The
  seat's sessions use shared input and audio, never an isolated gamescope. That gamescope and its
  Steam outlive a disconnect, whatever `keep_alive` says, and the next connect from any device
  takes them over; the switch to the desktop or a deliberate quit ends them.

  Steam's update helpers escalate through pkexec, which a seat is refused, and its first-run setup
  stops on that. A seat never updates the box, so the supervisor binds
  `/run/punktfunk/seat-no-update` over each of `steamos-polkit-helpers/steamos-update`,
  `steamos-polkit-helpers/jupiter-biosupdate` and `jupiter-initial-firmware-update` the box has:
  "nothing to do", 7 from `steamos-update` and 0 from the others.

`punktfunk-seats doctor` checks systemd, logind, the groups, a GPU render node, a compositor and
the installed unit.

### The owner's row and the door

On a box with [**Reachable without logging in**](/docs/profiles#on-linux) on, the box's own host
is the door: `punktfunk-host serve --door` (or `PUNKTFUNK_DOOR=1`), run as the `punktfunk` user by
`punktfunk-door.service` with `PUNKTFUNK_CONFIG_DIR=/var/lib/punktfunk`. It advertises, pairs,
serves the management API and places every connect, and opens no display, capture, input or
audio device. A connect is answered with a `Redirect` to a seat, or refused with
`SEAT_UNAVAILABLE`: the door never streams. `GET /api/v1/host` reports `door: true`.

The box owner is a seat like any other. `adopt-owner <user>` adds a ledger row for an ordinary
account that exists (uid 1000 up, never a seat's); deleting the row never removes the account or
its home. It takes a slot and ports from the same pools, and the door places the owner profile,
the profiles that share the owner's desktop and the light seats on it.

| Variable | The owner's row |
|---|---|
| `PUNKTFUNK_SEAT_OWNER` | `1`: the seat host is the owner's own, and also serves the owner and light-seat profiles. |
| `PUNKTFUNK_CONFIG_DIR`, `PUNKTFUNK_HOST_NAME`, `PUNKTFUNK_GAMESTREAM` | Not set: the owner keeps `~/.config/punktfunk` and their own settings. |

Every row sets `PUNKTFUNK_MDNS=0`, since only the door advertises. The row's environment file is
`/run/punktfunk/seats/<user>.env`, owned by the owner, and the owner's user `punktfunk-host` reads
it, so it moves to the row's ports at boot, logged in or not.

- **One owner host.** The row's unit runs the owner's host in a `background` session, on a
  private D-Bus, beside whatever the owner has on the monitor; the user unit stands down while the
  unit's runtime directory exists.
- **The monitor wins.** The supervisor polls logind every 2 seconds. When the owner has a session
  on a seat that isn't the row's own, it stops the row's unit and starts the owner's user host. The
  owner's session is never ended.
- **Switching.** `PUT /api/v1/profiles/door` with `{"on": true}` or `{"on": false}` answers 202.
  The owner's host starts `punktfunk-door-on@<user>.service`, the door
  `punktfunk-door-off@<user>.service`; both run `door-helper`, which moves the box's files between
  the owner's `~/.config/punktfunk` and `/var/lib/punktfunk` and turns the units on or off. The user
  must be in the `punktfunk-update` group.
