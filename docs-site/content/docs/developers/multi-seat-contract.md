---
title: Multi-seat contract
description: The rules between a Punktfunk host and its seat supervisor — the control channel, the environment a seat host starts with, the Windows connector reservation, and on Linux the door, the owner's row and a seat on a person's own account.
---

The rules between the host and its seat supervisor, which runs one host per seat so several people
can play on one box. A normal install has one host on the console session, and none of this applies
to it. Setup and what players see: [Seats on Windows](/docs/windows-seats),
[Profiles](/docs/profiles#on-linux) and [Someone's own account](/docs/own-account). Linux seats are
[further down](#linux-seats).

The supervisor is `crates/host/pf-seats`. A platform plugs in through `PlatformBackend`
(`src/backend.rs`), whose doc comments are the backend's half of the contract. The host reads its
half in one place, `pf_paths::seat` (`crates/host/pf-paths/src/seat.rs`).

## The supervisor

| | Windows | Linux |
|---|---|---|
| Runs as | The `PunktfunkHost` service, LocalSystem in session 0, beside the console host | The root daemon `punktfunk-seats` (`punktfunk-seats.service`) |
| Endpoint | `\\.\pipe\punktfunk-seats` | `/run/punktfunk/seats.sock`, `0660 root:punktfunk-door` |
| Who may connect | SYSTEM and Administrators; remote clients are refused | root and the `punktfunk` user, by the peer's credentials. Once the owner's row is adopted, the owner's uid too, for `install_flatpak` and `remove_flatpak` only |
| Ledger | `%ProgramData%\punktfunk\seats\` | `/var/lib/punktfunk-seats/seats/` |
| The client waits | 60 s for `list`, 30 min for `enable` with `prepare`, 5 min for the rest | 30 min for a Flatpak change, 180 s for the rest, 30 s more for an `unlock` |

Each connection carries one request, and at most eight are served at once. A frame is a four-byte
big-endian length, then JSON, 64 KiB at most. A request is
`{"schema_version": 1, "command": {"type": "<command>", …}}`; the answer is
`{"status": "success", "schema_version": 1, "result": {"kind": "<result>", …}}`, or
`"status": "error"` with `error: {code, message}`. The types are `Request`, `Command` and
`Response` in `crates/host/pf-seats/src/ipc.rs`. A request carries no credential of its own:
`adopt`, `adopt_user`, `credential` and `unlock` carry an account's password, which the supervisor
seals (Windows) or checks once (Linux) and forgets. On Linux a peer or command the socket refuses
is closed unanswered.

| Commands | Where |
|---|---|
| `list`, `create`, `start`, `stop`, `delete`, `doctor`, `seating`, `enable`, `disable` | Both. On Linux `enable` answers like `seating`, and `disable` is refused: seats are on while the daemon runs. |
| `adopt`, `credential` | Windows: [a seat on an existing account](#a-seat-on-an-existing-account) |
| `adopt_owner`, `install_flatpak`, `remove_flatpak` | Linux: [the owner's row](#the-owners-row-and-the-door), box-wide Flatpaks |
| `adopt_user`, `unlock`, `unlock_state`, `challenge`, `left`, `sign_out`, `devices` | Linux: [a seat on a person's own account](#a-seat-on-a-persons-own-account) |

On Windows the supervisor owns the seat accounts, the Windows sessions and RDP. The console host
talks only to a pipe that LocalSystem serves, since the name is free while the service is down.
Seat hosts run in jobs of the supervisor's own, so a console logon or a console host restart leaves
them running; stopping the service logs every seat account off. After a crash the next start does
the same for sessions a seat's games left behind.

- **The keeper.** `punktfunk-seat-keeper.exe`, beside `punktfunk-host.exe`, holds one seat's
  loopback RDP session open. The supervisor hands it the seat's credentials on stdin, never on the
  command line. Run `punktfunk-seat-keeper trust` from an elevated prompt once, before the first
  seat: it records the RDP certificate the keeper will accept. Turning seats on does this too,
  and replaces an earlier record. Each seat start compares the certificate TermService presents
  now with the record; a change fails the seat with `rdp_pin_changed` until seats are turned
  off and on. The keeper's stderr goes to the seat's `host.log`.
- **The seats group.** Every seat account is in the local group `punktfunk-seats`, rejoined at
  each start. The host denies that group on `%ProgramData%\punktfunk\ingest`, so a seat can't
  replace the owner's Playnite library. The doctor's `account` row reports the membership.
- **Seats on and off.** Without the reservation marker below the ledger is still listed, and
  every start answers `seats_off`. [Turn seats on and off](#turn-seats-on-and-off) writes the marker.

### A seat on an existing account

A seat signs in to an account the supervisor made, `pf_seat1` to `pf_seat4`, or to a local
standard account the box already has. The console asks for the second with `seat: true`,
`seat_account` and `seat_password` on `POST /api/v1/profiles`. `PUT /api/v1/profiles/{id}` with
`seat_password` replaces its password.

- **Commands.** `adopt` carries `seat`, as `create` does, plus the account's `sid` and
  `password`, and answers `created`. `credential` carries `id` and `password`, checks the password
  with a sign-in, seals it in place of the old one and answers `resealed` with the row.
- **Attempts.** Two wrong passwords for one account, by `adopt` and `credential` together, reach
  the sign-in in any 15 minutes. A third answers `unlock_locked_out` with the wait in its
  sentence before any sign-in, so the supervisor never drives the account into Windows' lockout.
- **The row.** An adopted row has `adopted_sid`, the account's SID, and `rdp_granted`, `true` when
  adopt put the account in Remote Desktop Users. A made account is the row's when its comment
  holds the seat's marker; an adopted one when its name resolves to `adopted_sid`. That SID is a
  local user's, `S-1-5-21-<n>-<n>-<n>-<rid>` with a RID of 1000 or more.
- **Names.** An adopted account's name follows the SAM rule: 1 to 20 characters, spaces and
  non-ASCII letters allowed, no control characters, none of `"/\[]:;|=,+*?<>`. A made one stays
  `pf_seat<n>`.
- **Passwords.** 1 to 256 bytes of UTF-8, no control characters, sealed with DPAPI under SYSTEM
  as a made account's is.
- **Adopt** refuses an administrator, an account denied Remote Desktop sign-in, an account with a
  session on this PC, and a password that doesn't sign in. Then it seals the password, adds the
  account to Remote Desktop Users where it is missing and to `punktfunk-seats`, and grants it
  `SeDenyInteractiveLogonRight`. It sets no account flag and writes no comment. A step that fails
  undoes the ones before it.
- **Release** is `delete` on an adopted row, by the row's SID. It removes the deny right, the
  `punktfunk-seats` membership, Remote Desktop Users only when `rdp_granted`, the sealed
  password, and the Steam launchers seats wrote into the account's hive: `SeatSteam`, our
  `steam://` handler, and a `Run\Steam` line naming a seat's IPC name. It never deletes the
  account or its profile.
- **Picker.** `GET /api/v1/profiles/windows-accounts` serves **Existing account** and take-turns
  profiles alike. An account is free when no ledger row names it, by name or by SID, no take-turns
  profile binds it, and its name isn't `pf_seat*`.

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
| `GET /api/v1/profiles/seating` | Returns `enabled`, `platform` (`windows`, `linux` on a door, or `other`) and `checks`: Windows Server, the Remote Desktop Session Host role, licensing and a graphics card. A desktop edition whose TermService loads a wrapper DLL instead of `termsrv.dll` passes with a warning and skips the role and licensing checks. |
| `PUT /api/v1/profiles/seating` with `{"enabled": true, "allow_rdp_from_network": false}` | Turns seats on. |
| The same with `"prepare": true` | First installs `RDS-RD-Server` and sets licensing to Per User where missing, then turns seats on. A role that needs a restart answers a `restart` error and leaves seats off. |
| `PUT /api/v1/profiles/seating` with `{"enabled": false}` | Stops every seat and turns seats off. The accounts stay. |
| `GET /api/v1/profiles/doctor` | Returns the supervisor's report: one sentence per row, `seat_id` and `seat_name` on a seat's rows. |

A failed check changes nothing: the answer is 200 with `enabled: false` and the check as an
`error`. When every check passes, turning on:

1. Writes the reservation marker with an `Enabling` value, which step 5 removes. A key that
   still holds it, after a crash, reads as seats off; turning on again finishes it.
2. Installs `pf_vdisplay_seats.inf` from `staging\pfvdisplay\` beside `punktfunk-host.exe`.
3. Turns the Remote Desktop listener on if it was off, and limits the **Remote Desktop**
   firewall rules to `127.0.0.0/8` unless `allow_rdp_from_network` is `true`, with the two
   port 3389 rules the Session Host role adds. Each rule's scope is noted first, and turning
   off writes it back. With `allow_rdp_from_network` the inbound rules on port 3389 are
   enabled if they were off; Shadow stays as it is. The service limits them again each time
   it starts.
4. Adds the seat listener `punktfunk-tcp`: a copy of `RDP-Tcp` on port 3390 whose sessions load
   `pf_seat_audio.dll`, with port 3390 blocked from every address but loopback. The x64 build goes
   to `System32\punktfunk` and the x86 build to `SysWOW64\punktfunk`. Every seat start brings
   them up to date. Not on ARM64.
5. Records the Remote Desktop certificate, as `punktfunk-seat-keeper trust --replace` does.

The keeper connects to 3390 when it listens, else to 3389. TermService opens and closes a listener
as its key appears and goes; nothing restarts.

Turning off puts the listener and the firewall scope back, removes the seat listener and its
files, points the box's Steam shortcuts back at Steam, and removes the marker. A file the audio
services still hold goes at the next restart. The seat display driver stays in the driver store.
`punktfunk-host seats off` does the same without the service; the uninstaller runs it. Off
Windows and off a door, `GET` answers `enabled: false` with `platform: other`, and `PUT` answers
409.

Every seat start compares the `DriverVer` of `staging\pfvdisplay\pf_vdisplay_seats.inf` with the
seats packages in the driver store and adds the staged one when none matches: an uninstall's
`driver uninstall` deletes the package, and an upgrade only stages the new one. A running seat
keeps its driver until its next session.

## The environment a seat host is started with

The supervisor starts an ordinary `punktfunk-host serve` with these. Don't set them by hand; set
them together. The host logs a warning at startup for each one that is set and ignored.

| Variable | Value | Rule |
|---|---|---|
| `PUNKTFUNK_SEAT_SESSION` | `1` | Marks a seat host. Unset or any other value is the console host. A seat host is never the door: `--door` is ignored on it. |
| `PUNKTFUNK_SEAT_ID` | 32 lowercase hexadecimal characters | Required with `PUNKTFUNK_SEAT_SESSION=1`. Missing or any other form: a seat host with `PUNKTFUNK_TRUST_DIR` doesn't start, and none mints audio devices. Ignored without `PUNKTFUNK_SEAT_SESSION=1`. |
| `PUNKTFUNK_SEAT_DISPLAY_SLOT` | Windows: `12`–`15` | The seat's connector. Not a number, out of range, or the marker absent: the host refuses every virtual-display session. |
| `PUNKTFUNK_TRUST_DIR` | Windows: the box's config directory. Linux: the seat's [trust copy](#linux-seats) | The seat reads the box's pairing store, `profiles.json`, per-device display overlays and `seat-defaults.json` from here, read only, and follows their changes. A relative path is ignored. |
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

## What the host does differently on a seat

- **The seat contract, not a display policy.** A seat that is not the owner's own reads no
  `display-settings.json` of its own. Its desktop is one virtual screen at the occupant's mode: it
  never mirrors, keeps lit, powers off or PnP-disables a real monitor, and never routes a game to
  a session of its own. A second device shares the occupant's screen when the box would join it,
  by its own `mode_conflict` or the box's, and is refused as busy otherwise, as the door refuses
  it before a redirect. Of a device's display settings in the box console, the largest mode, the
  scale and that rule follow it onto a seat. Linger, topology and identity are the unconfigured
  host's. `GET /display/settings` lists nothing in `enforced` and only `max_mode`, `scale` and
  `mode_conflict` per device; every display or session settings write answers 409.
- **The Seat defaults.** The box writes `seat-defaults.json` into its config directory at start and
  after each settings change, and the seat re-reads it when it changes, so a change reaches the
  seat's next session. Its keys: `end_on_game_exit` (a seat's stream ends when its game exits),
  `max_mode` (the largest mode a seat grants a device without a cap of its own),
  `library_writable` (Linux: seats update and install into the owner's Steam libraries; the
  daemon reads it from the box's copy when a seat starts), `dualsense_usbip` (the box's own
  **DualSense over USB/IP** switch: a seat's DualSense is a USB/IP pad with a sound card exactly
  when the box's is), `browser` and `browser_origins` (each seat runs a browser plane of its own,
  under those origins) and `background_input` (Windows: every app a seat starts reads GameInput
  in the background). Session settings the seat's profile holds in its own layer (below) come
  first; `end_on_game_exit` applies where it holds none. On a person's own Linux account a game's
  exit always returns to the desktop. The box itself applies **Seats kept warm** and **Stop idle
  seats**.
- **The box's trust, its own key.** The seat never writes the pairing store or profiles. A grant
  changed in the box console applies to the seat's next check, and within about three seconds to
  its live sessions: a narrowed grant takes effect, and an unpair, an expiry or a removed profile
  ends the session. It mints and keeps its own identity
  in `PUNKTFUNK_CONFIG_DIR`, never the box's. The supervisor records its certificate's SHA-256 in
  the ledger while it runs. The box's `Redirect` carries that pin (field 7) and `enumerate` lists
  it as `seat.pin`, so a client pins the seat through the box it already trusts. A seat whose pin
  the box doesn't hold yet is unavailable. Sleep, restart and shut down are refused: the box's own
  host is the one that does them, and it ends the sessions on every seat first.
- **The box's library, on Windows.** A Windows seat runs no plugins and answers a plugin install
  with 409. It lists and launches the box's titles as its own user, and answers every library
  change with 409. Titles from sources
  that belong to one account (Playnite, Game Bar, Amazon, itch, Hydra) are left out. It runs the
  box's emulators from `%ProgramData%\punktfunk\emulators` with the firmware the box's plugins
  staged. The box host tails each running seat's `game.*` into its own bus, with `seat` naming
  the profile: its plugins hear them, its hooks don't. A seat's held launch (below) runs the box
  host's plugins, which then release it. The box host's save and firmware routes take
  `?profile=<id>` for a profile with a seat: they read and write that seat account's copies, in
  its profile folder. A seat host answers 409.
- **The box's library, on Linux.** The box's library is the owner's. A seat other than the
  owner's runs no plugins and lists and launches only what it can start as its own user:
  `steam_appid` with an id that fits 32 bits and `steam_ui`, with its own Steam, and `emulator`,
  with a copy it can run and a game file it can read. The seat answers every library change with
  409. The owner's host installs a Flathub emulator box-wide through `install_flatpak`, so every
  seat runs it with its own config and saves in `~/.var/app/<id>`; which emulators stay the
  owner's is in [Profiles](/docs/profiles#on-linux). At a launch the seat installs the RetroArch
  core the entry names into its own RetroArch and places the firmware from its library copy.
- **One profile.** The seat serves the profile whose seat it is. A connect that names any other
  profile, the owner's included, is refused as an unknown profile; one that names none plays as
  this seat's profile. The owner's row alone also serves the owner and the profiles that share the
  owner's desktop.
- **A profile's own layer.** Each profile but the owner may keep a layer of its own over the box's
  library, in the library directory at `library-profiles/<profile id>/`: `library-hidden.json`,
  `library-scanners.json`, `library-overrides.json`, `library-picks.json` and
  `session-settings.json`, each the shape of the box's file. Only the host-minted id (12 lowercase
  hex) names a folder. Nothing in a layer runs: no custom entry, command, `prep` or hook. The
  host that writes the library writes the layers, and removing a profile removes its folder. A
  layer adds and narrows, never widens: a title is hidden, or a source off, when the box or the
  profile says so; the profile's game override (for the game or its source) and its art pick for
  a slot win over the box's. Session settings are the profile's file when it has one, and only a
  profile with a seat of its own has them. A seat reads its own profile's folder, found from the
  box's `profiles.json` in its trust directory, for its list, launches, art, game overrides and
  session settings; the owner's host applies the layer of the profile a session resolved to,
  sharing the owner's desktop or taking turns, at launch and for game overrides. On the box,
  `?as=<profile id>` on the library routes and `session/settings` reads and writes that layer, and
  a profile with a seat of its own lists there what its seat lists. `DELETE session/settings?as=`
  drops the profile's own session settings, so its seat takes the Seat defaults again.
- **Not a host of its own to clients.** A seat host never advertises over mDNS, whatever
  `PUNKTFUNK_MDNS` says, and its `Welcome` carries management port `0`: its API answers on
  loopback only, so a client keeps the box's port, where the library comes from.
- **No status tray.** A seat host doesn't start or supervise one; the supervisor is the control
  surface.

### A Windows seat host

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
- **Its pads are its own.** Every virtual device a host creates, its pads and its virtual mouse,
  carries the host's session id (`DEVPKEY_Device_SessionId`), so another session can't list it,
  open it or receive its input. A pad the host can't fence is not created. The box has 16 pad
  indices; each host takes the lowest one that no other host's pad holds.
- **The desk's controllers are the desk's.** The box's host, which runs in the console session,
  writes that session into every physical game controller present and into each one that
  arrives, so a seat's HID, XUSB and XInput listings and its GameInput deliveries leave them
  alone; a device a Remote Desktop session redirected in keeps its own session. The tag is wiped
  by the device's next start and written again on its arrival; the service's stop and the
  uninstaller clear it. What Windows still shows a seat:
  [Seats on Windows](/docs/windows-seats#a-controller-plugged-into-the-pc-appears-on-a-seat).
- **Its pads' audio is its own.** A Remote Desktop session lists only what its listener's audio
  endpoint enumerator returns. Through the seat listener, that is Remote Audio plus one
  "Wireless Controller" render endpoint per DualSense the seat host serves, in that pad's container
  and in that session alone. The seat host claims a pad's index by creating
  `Global\punktfunk-seat-pad-<session>-<index>` and its ring `…-pcm`, before the pad appears; the
  endpoint inside audiodg writes what games render there into the ring, and the seat host streams
  it as pad audio. A seat on `RDP-Tcp` has no pad audio.
- **Steam starts as the seat's copy.** Once the copy exists, each seat start writes the seat
  account's `Software\Punktfunk\SeatSteam` (`Exe`, `Ipc` = `pfseat<seat id>`), a `steam://`
  handler in its classes hive (marker value `PunktfunkSeat`) and Steam's `Run` entry, all naming
  the copy under its IPC name. SYSTEM owns both keys; the seat may only read them. The box's
  public Desktop `Steam.lnk` and the shared Start menu's `Steam\Steam.lnk` start
  `punktfunk-tray.exe --steam "<their steam.exe>" <their arguments>`, which starts the copy for
  an account with `SeatSteam` and the box's Steam for any other. Their targets are kept in the
  Seats key's `SteamLinks` and put back when seats go off.

## A profile's lock

`profile-locks.json` sits beside the box's pairing store. The box's own host, the plain host or
the door, is its one writer, `0600` on Linux and SYSTEM and Administrators on Windows; it is never
in `profiles.json`. Its `profiles` map each profile id to `locked`, an optional `pin`
(`iterations`, `salt`, `hash`: PBKDF2-HMAC-SHA256, 600,000 iterations, a 16-byte salt) and the
`devices` list of a person's row's `devices.json`. Removing a profile removes its entry;
unlocking keeps the list and the PIN. One desktop has one lock: the owner is the first profile
whose `os_account` is `operator`, and any later one shares its desktop, has no entry and opens
with the owner's lock, list and PIN. A take-turns profile keeps its own.

- **The check.** A device opens a profile when the profile is unlocked, or its list admits the
  device: `stay`, or present. Any other device is refused, or asked for the PIN when the profile
  has one. Every host asks it after a connect resolves its profile and before the profile is
  placed or played, the door and a seat host alike. A wake, the picker (`enumerate`), the door's
  forwarding of a device's own play, and a GameStream launch or resume (as the owner) ask it too.
  A Punktfunk session whose device stops being admitted ends within a second; a GameStream
  session runs on until it ends.
- **A file that doesn't read.** Every profile is refused to every device, every edit is refused,
  and the host logs it. A missing file locks nothing.
- **The PIN.** Only the box's own host checks it, sent as `ProfileUnlock` kind 1. A device waits
  after wrong ones as after a wrong account password; 10 wrong for one profile within 15 minutes,
  over all devices, close PIN entry for it until the oldest is 15 minutes old. Counts live in
  memory. A right PIN makes the device present, or `stay` when the message asks. Each try writes
  one line under `punktfunk::audit`, never the PIN.
- **A seat host** reads the box's file, read only, and never takes a PIN. A Windows seat reads it
  from `PUNKTFUNK_TRUST_DIR`. A Linux seat reads the [trust copy](#linux-seats), which holds its
  profile's lock with no PIN; a locked profile's seat gets only the pairing records its lock
  admits, as a person's row does, and the owner's row follows the owner's lock. A lock file
  there that doesn't read gives an empty pairing list.

## Checking a seat can actually stream

Run this inside the seat's session:

```
punktfunk-host spike --source virtual --seconds 5
```

It creates a virtual display, captures it and encodes in the display driver. It exits non-zero on
any gap: no driver, no captured frame, no GPU encoder (a software encoder counts as a failure), or
no encoded output. The supervisor runs it before it reports a seat healthy. Add `--hdr` to also
require the 10-bit path.

## The seat display driver is a second package

Windows' terminal-services stack starts a seat's display with whichever driver claims the hardware
id `RdpIdd_IndirectDisplay`. Claiming it **takes over every RDP session on the machine**, ordinary
Remote Desktop included, so the build produces two packages from the same `.inx` and signing key:

| Package | Claims | Installed by |
|---|---|---|
| `pf_vdisplay.{inf,cat}` | the console display device, `Root\pf_vdisplay` | every punktfunk install |
| `pf_vdisplay_seats.{inf,cat}` | `RdpIdd_IndirectDisplay` only | turning seats on, and a seat start when the store lacks the staged version |

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
`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\<x64|arm64>\`, where Steam's first start puts
them ([Prepare Windows Server](/docs/windows-seats#prepare-windows-server)). Without them a seat
streams video with no audio. Pad audio needs neither: it comes from the seat listener.

```
powershell -File check-seat-audio.ps1
```

[`check-seat-audio.ps1`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/check-seat-audio.ps1)
exits 1 when neither is present. A virtual cable is no substitute: the host never picks one for
desktop audio, `PUNKTFUNK_MIC_DEVICE` only pins the microphone, and a cable pinned with
`PUNKTFUNK_LOOPBACK_DEVICE` is shared by every seat.

## Linux seats

On Linux the supervisor is the root daemon `punktfunk-seats` (`punktfunk-seats.service`), not part
of the box host. Its state is `/var/lib/punktfunk-seats`, root's alone; the paths below are under
it. The box host's directory, `/var/lib/punktfunk`, is the door's; the daemon reads it for the
trust copies. It starts one `punktfunk-seat@<user>.service` per seat: a system-range user,
`pf-seat-<n>`, with a logind session of its own, a headless compositor (`kwin_wayland`, else
gamescope) and a stock `punktfunk-host serve`, all run by `/usr/libexec/punktfunk/seat-session`.
`punktfunk-seats doctor` checks systemd, logind, the groups, a GPU render node, a compositor and
the installed unit.

- **The package.** `punktfunk-seats` is its own package. The host's rpm, deb and pacman packages
  depend on it at their own version, and it installs the units without enabling them. NixOS turns
  it on with `services.punktfunk.seats.enable`. The SteamOS on-device build lays the same files out
  under `/opt/punktfunk` and `/etc` (`scripts/steamdeck/door.sh`).
- **The CLI.** `punktfunk-seats list`, `create <name>`, `adopt-owner <user>`,
  `start|stop|delete <id>`, `doctor` and `seating` send the [socket's](#the-supervisor) requests;
  `release-owner` deletes the owner's row and `stop-all` stops every live seat.
  `check-owner on|off <user>` sends none: it applies the switch rules below and prints
  `<uid> <home>` or `gone`.
- **Box-wide Flatpaks.** `install_flatpak {id}` adds Flathub to the system installation when the
  box lacks it and installs the app there, for every account; each user keeps its own data in
  `~/.var/app/<id>`. `remove_flatpak {id, purge}` uninstalls it; `purge` deletes root's data of
  it, never a seat's. Both answer `flatpak {id, version}`. An id is reverse-DNS: letters, digits,
  `.`, `_` and `-`, at least one dot, at most 255 bytes, no leading `-`; any other is refused
  before `flatpak` runs. One change runs at a time: a second meanwhile answers `conflict`.
- **The user.** Its comment is `punktfunk-seat=<id>`, and the daemon touches or deletes only an
  account whose comment matches exactly. The name is `pf-seat-1` to `pf-seat-4`; a create for
  any other is refused. The home is `seats/<id>/home`, mode `0700`. It joins `render` and
  `punktfunk-games`, the shared games folder's group, which the owner joins too. Never `punktfunk`,
  which may power the box off, nor `input`, which reads every input device on the box.
- **The environment.** The [variables](#the-environment-a-seat-host-is-started-with) go into
  `/run/punktfunk/seats/<user>.env` (root `0600`), which systemd reads as root. The seat's
  `mgmt-token` is owned by the `punktfunk` user (root without one); every seat user shares
  `punktfunk-games`, so no secret relies on group read.
- **The trust copy.** The seat reads `trust/<id>/`, not the box directory:
  `punktfunk1-paired.json`, `profiles.json`, `display-settings.json` (from the library copy,
  the owner's), `seat-defaults.json` and `profiles/`, `root:<seat user>` `0640`, and the seat's
  [profile locks](#a-profiles-lock) when the box has any. The daemon recopies a file within 2
  seconds of its change while the seat runs. The box's key never crosses. A seat other than the owner's also finds the box's library there, its
  `PUNKTFUNK_LIBRARY_DIR`.
- **The box's library.** The owner's host writes the library; its plugins scan the owner's home.
  From the owner's `~/.config/punktfunk` the daemon copies `library.json`, `library-hidden.json`,
  `library-metadata.json`, `library-overrides.json`, `library-picks.json`, `library-scanners.json`
  and `library-metadata/` to `library/`, `root:punktfunk-door` `0640`, within 2 seconds of a change,
  and from there into each seat's trust copy. Each profile's layer, `library-profiles/<profile id>/`
  with `library-hidden.json`, `library-overrides.json`, `library-picks.json`,
  `library-scanners.json` and `session-settings.json`, goes to `library/` for every profile; a
  seat's trust copy gets only the folder of the profile whose `os_account` names that seat in the
  box's `profiles.json`, and loses one that stops being its own.
  `plugin-run/plugin-grants.json` goes with them, and so
  does the firmware plugins staged, `plugin-state/<provider>/firmware/<platform>/<file>` at exactly
  that depth, up to 1 GiB a file. Nothing else of `plugin-run` or `plugin-state` is read: the
  plugins' tokens and sign-ins live there. A link, a file with a second link or one over its cap is
  not copied. Play stats stay with each host. A config folder it can't open, as in a home not
  mounted yet, leaves the copy as it was. Deleting the owner's row deletes `library/`.
- **Games.** `games/steamapps/` is one library every seat writes (group `punktfunk-games`, setgid,
  default ACL). Each seat's own `compatdata`, `shadercache` and `downloading` under `seats/<id>/`
  are bind-mounted over the shared ones, so prefixes never cross. A seat other than the owner's
  sees an empty `/run/media`: Steam in Game Mode adopts every mounted drive with a `steamapps` as
  a library, and a drive as mounted is the owner's alone.
- **The owner's libraries.** At each start the daemon shows every library the owner's Steam lists
  (`steamapps/libraryfolders.vdf`) at `seats/<id>/libraries/<key>`, `<key>` a hash of its path.
  Its `steamapps` is the owner's own, through an idmapped mount: the seat's Steam plays, updates
  and runs Proton from it as the seat user, and every file stays the owner's on disk. The seat's
  own `compatdata` and `shadercache` cover the owner's. A library is skipped, with a line in the
  daemon's log, when it is gone, its `steamapps` isn't the owner's, or its filesystem takes no
  idmapped mount (anything but btrfs, ext4, xfs and f2fs). With `library_writable` off
  (`PUNKTFUNK_SEAT_LIBRARY_WRITABLE`) the mount is read-only, and what a launch must write
  (`common/SteamLinuxRuntime_*/var`, `common/SteamLinuxRuntime_*/*/files/.ref`,
  `common/Proton*/dist.lock`) gets a private copy under `seats/<id>/private/<key>`. The owner's
  Steam settings are never changed.
- **Granted roots.** At each start every root the owner granted a plugin, as the library copy's
  `plugin-grants.json` lists it, appears to the seat read-only at its own path, so a ROM the
  library names opens on the seat: idmapped on btrfs, ext4, xfs and f2fs, so the seat reads the
  owner's files as the owner, and a plain read-only bind elsewhere. A root in the owner's home is
  reached through an empty folder over that home. A root granted later reaches the seat at its
  next start.

  The mount rules, and the roots and libraries each skips, are in
  `crates/host/pf-seats/src/linux/libraries.rs`.
- **Save exchange.** Each seat has `seats/<id>/saves/out` and `saves/in`, the seat user's, mode
  `0770`, with an ACL entry and a default one for the box owner's uid, so no other seat reaches
  them. The daemon makes them at each start, sets the owner's entries for the owner row it knows
  then, and keeps what is in them until the seat is deleted. At a launch of an `emulator` title,
  before its hold, the seat host exports every unit of that emulator and platform into `out/` and
  lists them, with the platform's firmware, in `out/units.json`. Once the hold is released it
  puts back each `in/<name>` beside an `in/<name>.import.json` for that title, then empties
  `in/`; when the game exits it exports what changed. The owner's host answers the save routes'
  `?profile=` from there: it lists `units.json`, copies a unit's file from `out/` into the plugin's
  folder, and leaves an import in `in/`. It reads only plain files the seat user owns and never
  writes through a link. A seat's ROM roots are read-only, so nothing saves beside a game there.
- **Stopping.** The daemon's stop ends the seat's user (`loginctl terminate-user`; a user still
  running after 10 seconds fails the stop with `user_busy`) and takes back every pad relay and
  USB/IP socket it handed the seat. A stop of the owner's row ends neither: that account is a
  person's.
- **Pads.** A seat user can't open `/dev/uinput` or `/dev/uhid`: they are the console user's. A
  seat host that can't write the node asks the daemon's pad broker, `/run/punktfunk/pads.sock`,
  for a pad by kind (`pf_inject::pad_broker::Request`). The daemon builds it from its own tables,
  keeps the kernel fd and hands the seat one end of a relay socket: a uinput pad's relay carries
  the seat's `input_event` frames down and rumble back, and a uhid pad's carries the
  `uhid_event`s the seat would write and read, minus `CREATE2` and `DESTROY`, which stay the
  daemon's. The pad lives until the seat hangs up. The socket admits a running seat's user only
  (the owner's row included), eight pads per seat. A box without the daemon gives a seat no pad.
- **The fence.** The daemon stamps each pad's `phys` `punktfunk-seat:…` and records the device in
  `/run/punktfunk/pads/vouched`, root's alone. `65-punktfunk-seats.rules` runs
  `punktfunk-seats fence` on every stamped `input` and `hidraw` node: a recorded device's node
  becomes that seat user's, `0600`, on a seat of its own, so neither the box's own Steam nor
  another seat sees it, and any other stamped node is root's. While the daemon runs, every other
  hidraw node keeps its group and seat0's ACL but loses `other` access.
- **`/dev/uinput`.** Before a seat's unit starts, the daemon serves it a CUSE device,
  `/dev/punktfunk/uinput-<user>` (the user's, `0600`), and the unit binds it over `/dev/uinput`
  for the seat's Steam Input. Each open is one real uinput device the daemon makes, 16 per seat.
  Only gamepad capabilities pass: `EV_SYN`, `EV_KEY` for joystick and gamepad buttons,
  `BTN_DPAD_*` and `BTN_TRIGGER_HAPPY*`, `EV_ABS` without pressure, tilt, tool or multitouch axes,
  `EV_MSC`, `EV_FF` and the accelerometer property. Any other bit is refused with `EPERM`, so a
  seat never makes a keyboard or a mouse. It needs `/dev/cuse`; `doctor` reports it.
- **USB pads.** A seat's USB/IP pad (the virtual Deck, the Steam Controllers, the DualSense, the
  Switch 2) is served by the daemon, never by the seat. A seat that can't write vhci's `attach`
  sends a `Serve` with the pad kind, and the client pad's identity for a Steam Controller, whose
  serial passes as 1–16 letters and digits. The daemon starts a pad server,
  `punktfunk-seats serve-usb`, which answers every descriptor and control transfer from
  `pf-inject`'s tables, records the vhci port in `/run/punktfunk/pads/vhci/<port>` and writes
  `attach`, four ports per seat. The seat gets one end of a relay and sends only input reports,
  by interface. Closing the relay ends the pad server and the device.
- **Game Mode.** The seat host keeps the seat's mode, `desktop` or `game`, in `seat-session` in
  its config directory, and starts in the one it ran last. It owns
  `com.steampowered.SteamOSManager1` on the seat's session bus and serves `SessionManagement1`,
  which `steamosctl` and Steam call; `steamos-session-select` and `steamos-desktop-return`, first
  on the seat's `PATH`, call it too. A switch never reaches the box's login. On the owner's row
  only the headless host plays the seat's mode; the owner's own host at the desk leaves Game Mode,
  its bus name and its gamescope to the box. A seat never updates the box: the supervisor binds
  `/run/punktfunk/seat-no-update` over each of `steamos-polkit-helpers/steamos-update`,
  `steamos-polkit-helpers/jupiter-biosupdate` and `jupiter-initial-firmware-update` the box has.

### The owner's row and the door

On a box with [**Reachable without logging in**](/docs/profiles#on-linux) on, the box's own host
is the door: `punktfunk-host serve --door`, run as the `punktfunk` user by
`punktfunk-door.service` with `PUNKTFUNK_CONFIG_DIR=/var/lib/punktfunk`. It advertises, pairs,
serves the management API and places every connect, and opens no display, capture, input or
audio device. Its primary group is `punktfunk-door`, its own: it is not in `punktfunk`, which owns
the usbip attach nodes and passes `pf-dm-helper`. The seat socket and the box's library copy are
that group's. `door-helper ports` and `on` move an account with any other primary group into it.
A connect is answered with a `Redirect` to a seat, or refused with
`SEAT_UNAVAILABLE`: the door never streams. `GET /api/v1/host` reports `door: true`. A host
started with `PUNKTFUNK_SEAT_SESSION=1` is a seat host and never the door, however the door is
asked for.

A seat's seatless session may not take a logind block, so the door keeps the box awake: while
anyone plays on a running seat it holds a logind `sleep` block, which `49-punktfunk-door.rules`
grants it. The screen at the desk can still go dark.

Every client reads its library from the door, which has `PUNKTFUNK_LIBRARY_DIR` set to the box's
library, `/var/lib/punktfunk-seats/library`: all of it, for every profile, read only. A cover
there names a file in the owner's home, which the door can't read, so it asks the owner's host
for it over that host's loopback API. While that host is down the cover answers 404; asking for
one never starts the owner's session.

The door answers only its own: the open routes, pairing (`native/*`), `profiles/*`, power
(`actions/*`), `update/*`, `client-logs/*`, its `logs`, `events` and `events/relay`, the library
copy's reads (`library`, `library/page`, `library/art/*`), and what it merges with the owner's host:
`host` (the owner's `gamestream`), `host/settings` (its own rows are `host_name`, `mdns`,
`webtransport`, `webtransport_origins`, the Seat defaults and `update_*`), `status` (every
seat's sessions, games and downloads, each row with `seat`, the profile whose host runs it, for
the operator) and `diagnostics` with `diagnostics/refresh`. Every other admin call goes to the
owner's host the way covers do, a dot segment never, and the table is checked against the API
document (`every_route_has_a_side_on_the_door`). The door releases a seat's launch hold on that
seat (`game/holds/{token}/release`, below). A device's `game/end`, `GET display/targets`,
`PUT display/clients/*` and `GET compositors` go to the seat it plays on, and its
`library/install/*` to the owner's host, whose plugins download into the shared game files, each
as that device: the door's call carries `X-Punktfunk-Device`, which a seat host honours only with
its admin token and only for a device paired there, and then answers on the device's grants. A
call starts a stopped seat and answers 503, except `host`, `status` and `diagnostics`, which
wake nobody; a host that doesn't answer is 502, and so is every call while the seat supervisor
doesn't answer. With no owner row the owner's calls answer 503, and so does a `host/settings`
change to any row but the door's own: the door's files never stand in for the owner's.
`events` and `session/*/pads` stream.

Events cross the same way. Each running seat's events join the door's bus with `seat` naming
the profile whose host raised them, so the console sees sessions, games, library and plugin
changes where they happen. The door's own (`pairing.*`, `access.*`, `host.*`, `action.*`,
`update.*`) go to the owner's host's `POST /events/relay`, whose hooks run them in the
owner's session; the door runs no hooks. The two sets are disjoint, so nothing comes back
around. A knock (`pairing.pending`) wakes a stopped owner's session and reaches its host once
that host answers, within two minutes, a supervisor that doesn't answer meanwhile included;
any other door event a stopped owner's host misses.

A seat's `game.*` goes to the same route with its `seat` kept: the owner's plugins hear it, and
no host's hooks run on it. A seat host runs no plugins, so its `game.launching` carries a `hold`
token, and the launch waits up to 130 seconds for `POST /game/holds/{token}/release`. The door
sends it once the owner's host has run the plugins that hold the stage, whatever they answered,
and at once when that host isn't running. An event a seat host lists with a `seat` already is
one a host relayed there, and is never taken back.

The door's console also accepts a console handoff signed with the owner's host's token, the one
the owner's own `ctl console-url` holds: the door keeps it in `owner-handoff-key` in its own
directory, level with the owner's row, and never a contract seat's, whose user can read their
own. The owner's row never stops for being idle: it runs the plugins and keeps the library. A
seat host whose binary an update replaced restarts once nobody plays on it, and its runner
starts the new one.

`display-settings.json` and `hooks.json` are the owner's host's alone: `door-helper` copies
neither, the owner's host writes the per-device overlays too, the supervisor copies the policy
with the library (`library/`), the door reads it there for its second-device rule, and each
seat's trust copy takes it from there.
`host.restart` on the door restarts the owner's host, then the door itself; both stop and
their units start them over.

The box owner is a seat like any other. `adopt-owner <user>` adds a ledger row for an ordinary
account that exists (uid 1000 up, outside 60001–65535 and below 2³¹, never a seat's), and only for
the one `door-helper on` recorded in `/var/lib/punktfunk-seats/owner`. Any other account, or any
while nothing is recorded, answers `owner_not_recorded`. Only root writes that file, so the door
can't name another account. The door reads it to switch off, so a supervisor that doesn't answer
never keeps the door on. Deleting the row never removes the account or its home. It takes a slot and
ports from the same pools, and the door places the owner profile and the profiles that share the
owner's desktop on it. Its [environment](#the-environment-a-seat-host-is-started-with) leaves out
`PUNKTFUNK_CONFIG_DIR`, `PUNKTFUNK_LIBRARY_DIR`, `PUNKTFUNK_HOST_NAME` and `PUNKTFUNK_GAMESTREAM`:
the owner keeps `~/.config/punktfunk`, the library their plugins write, and their own settings. Its
environment file has an ACL entry that lets the owner read it, so the owner's user `punktfunk-host`
moves to the row's ports at boot, logged in or not. The owner's own console,
`punktfunk-web.service`, doesn't start while that file exists: the door's console has the port. With
the owner's GameStream setting on, the owner's row is the box's Moonlight host, on the owner's own
pairings: the door can't redirect Moonlight, and a contract seat never serves it.

A browser that sets `PROFILES` is redirected to its seat's browser plane: the `Redirect` names
that port and carries the plane's certificate hash (field 8), which the browser pins and binds
its credential to. A running seat host follows the box's Seat defaults within seconds: its plane
comes up on its port, under the box's origins, and closes when the box stops streaming to
browsers. A seat host without Seat defaults follows its `PUNKTFUNK_WEBTRANSPORT*` variables.

- **One owner host.** The row's unit runs the owner's host in a `background` session, on a
  private D-Bus, beside whatever the owner has on the monitor. The owner's `punktfunk-host` and
  `punktfunk-kde-session` user units stand down while the unit's runtime directory exists.
- **The monitor wins.** The supervisor polls logind every 2 seconds. When the owner has a session
  on a seat that isn't the row's own, it stops the row's unit and starts the owner's user host. The
  owner's session is never ended.
- **Switching.** `PUT /api/v1/profiles/door` with `{"on": true}` or `{"on": false}` answers 202, or
  204 when the box is already so. The owner's host starts `punktfunk-door-on@<user>.service`, the
  door `punktfunk-door-off@<user>.service`; both run `door-helper`, which moves the box's files
  between the owner's `~/.config/punktfunk` and `/var/lib/punktfunk` and turns the units on or off.
  The console's `web-session-key` and `web-session-epoch` move with them (on SteamOS, its
  `PUNKTFUNK_UI_SECRET` line), so a signed-in console stays signed in. `on` also gives the door the
  owner's `host-settings.json`, so the door's own rows start as the owner set them; before `off`,
  the door sends the rows it stored to the owner's host, and answers 503 while that host starts. The
  user must be in the `punktfunk-update` group, and a member switches the door for their own account
  only. The helper writes that account to `/var/lib/punktfunk-seats/owner` first. It refuses to
  switch on while the file names another account, and to switch off for any but the one it names,
  which may have left the group since. A name whose account no longer exists names no one: a
  member's switch-on takes the door over, replacing the box's files with their own where they have
  the same file, and a switch-off for it leaves the box's files in `/var/lib/punktfunk`. Only a
  `getent` miss on a box whose `passwd` sources are all local counts; any other `getent` status, or
  a `passwd` source such as `sss` or `ldap`, refuses both. The helper and the owner's host ask
  `check-owner` for these rules. A switch the helper refuses or abandons sets `door_switch_failed`
  on the starting host's `GET /host`, and the console stops waiting.
- **The box's ports.** The door binds the ports the owner's `host.env` names
  (`PUNKTFUNK_MGMT_BIND`, `PUNKTFUNK_NATIVE_PORT`), and its console binds where the owner's
  console did (`PUNKTFUNK_UI_BIND`, from `web.env` on SteamOS). `door-helper ports` copies those
  lines to `/var/lib/punktfunk-seats/host.env`, root's, before each door start, so an update or
  a later edit reaches the door. On NixOS the door also takes the two port keys from
  `services.punktfunk.host.settings`, and its console follows `services.punktfunk.web.bind`.

### A seat on a person's own account

A seat on the Linux door can run on an account the box already has: a family member's own login.
The console asks with `seat: true`, `seat_account`, a Linux user name, and `seat_password` on
`POST /api/v1/profiles`. It runs as the owner's row does, on a bus of its own, but in a session
of the unit's `user` class, which a shutdown at the machine counts as another user's session,
and it is a contract seat's host, not the owner's.

- **Command.** `adopt_user` carries `seat`, as `create` does, plus the account's `password`, and
  answers `created`. The supervisor reads the account's uid itself. The password goes once through
  PAM's `login` stack, the one `punktfunk-seat@.service` runs its unit under: `pam_authenticate`,
  then `pam_acct_mgmt`, so a locked or expired account is refused. Nothing keeps it, and
  `credential` is refused for the row. A refusal's code is `credential_wrong`,
  `credential_must_change`, `account_locked`, `account_missing`, `account_not_ordinary`,
  `account_is_owner`, `account_taken`, `unlock_locked_out` or `pam_unavailable`.
- **The check.** PAM runs in a child, `punktfunk-seats pam-check <account>`, with an empty
  environment and the password on stdin; libpam never loads into the daemon. A child that hasn't
  answered in 30 seconds is killed and the answer is `pam_unavailable`. Other commands don't
  wait on it, and the account's uid is read again before the row lands.
- **Attempts.** After a failed check a device waits 2 s × 2^failures, and 15 minutes after the
  fifth. Two failed checks for one account reach PAM in any 15 minutes, from every source together,
  so a stack that locks after three, such as `pam_faillock`, is never driven there. A refusal
  answers `unlock_locked_out` with the wait in its sentence. A check that gave no verdict counts as
  failed.
- **Which accounts.** An ordinary user, as the owner's row takes: uid 1000 up, outside
  60001–65535 and below 2³¹, never a seat's, named by the owner's name rule, so `user@domain` of
  a directory service passes. Not the box owner, by name or by uid, and not an account another row
  holds, by name or by uid.
- **The row.** It has `adopted_uid`, the uid read at adopt, and no `owner`. A start, the certificate
  read and the doctor refuse a row whose name now resolves to another uid. A stop or a delete still
  runs and reaches only the row's unit.
- **A guest on the account.** Nothing on the account changes: no `useradd`, `usermod` or
  `userdel`, no group, no mode or owner of the home, and the daemon writes nothing inside it.
  The row's files are under `/var/lib/punktfunk-seats` and `/run`: its environment file stays
  root's, and the host keeps its identity in `seats/<id>/config`, the person's `0700`, not in
  `~/.config/punktfunk`. A stop ends the row's unit and no other process of the user, their
  login and SSH included. A delete removes what the row made and gives the account back.
- **The monitor wins.** While the person has a session on a seat, a start answers
  `account_at_desk`, and a running unit ends within 2 seconds of their login. Nothing takes the
  row over: their own `punktfunk-host` units are not started or stopped.
- **What it does not take.** The owner's `host.env` and settings, the box's library as its own
  (it plays the box's, from its trust copy), the door, and the owner's user host. The trust copy
  is readable by the person alone.

#### Who opens a person's seat

The supervisor, not the door, decides which device gets onto a person's row. The door relays and
names the device by the fingerprint its QUIC handshake authenticated. Every command below answers
`invalid_request` for any other row, `not_found` for a missing one, and `invalid_request` for a
fingerprint that isn't 64 lowercase hex or a device name that isn't 1 to 64 printable characters.
A refusal is `backend` with `<code>: <sentence>`, the sentence the person reads.

- **`unlock {id, device: {fingerprint, name}, proof}`** answers `unlocked {seat, started}` only
  while the session runs and the device is admitted; `started` when this unlock started it.
  `proof` is tagged by `kind`:
  - `none`: an admitted device rejoins a running session. Otherwise `unlock_required`.
  - `password {password, stay_signed_in, enroll}`: the attempt limits, then PAM (`login` stack,
    auth then account) in the `pam-check` child, outside the supervisor's lock. On success a
    stopped session is started through the same path as `start`, so `account_at_desk` and the other
    start refusals apply, then the device is present; `stay_signed_in` sets `stay`, and `enroll`
    (`{alg: "p256" | "rsa2048", public}`) stores its key. A key that isn't a P-256 point on the
    curve or a DER `RSAPublicKey` of exactly 2048 bits is `invalid_request`, before PAM. Refusals:
    `credential_wrong`, `account_locked`, `credential_must_change`, `pam_unavailable`,
    `unlock_locked_out`, or `capacity` when the row lists 64 devices.
  - `key {signature}`: the device's enrolled key signed the live challenge (below), which this uses
    up whatever the outcome; then as after a password, with no PAM call. A wrong signature, a
    challenge used up, expired or never asked for, and a box certificate that no longer matches
    its pin answer `proof_invalid`. A failure counts against the device's limit alone, never the
    account's.
- **`unlock_state {id, fingerprint}`** answers `unlock_state {needs}`: `none` while the session
  runs and the device is admitted, else `key` when it has a key, else `password`.
- **`challenge {id, fingerprint}`** answers `challenge {challenge, seat}`: 32 bytes from the OS as
  64 hex, valid 120 seconds and once, the device's only live one. A device with no key gets
  `unlock_required`. The key signs `"punktfunk-seat-unlock-v1"` ‖ challenge ‖ the seat id's 32 hex
  characters ‖ the device fingerprint ‖ the box fingerprint, 152 bytes, with SHA-256: ECDSA P-256
  as an ASN.1 DER signature, or RSA 2048 as a 256-byte signature, PKCS#1 v1.5 or PSS with a
  32-byte salt.
- **`left {id, fingerprint}`** answers `left`: the device disconnected, so its presence ends 600
  seconds later unless it rejoins.
- **`sign_out {id, fingerprint}`** answers `signed_out`: forgets that device, its stay and its key,
  or every device without a fingerprint.
- **`devices {id}`** answers `devices {devices}`, each `{fingerprint, name, kind, added_unix,
  last_unix}`, `kind` `key` with a key, else `stay`, else `for_now`.
- **`start`** of a person's row answers `unlock_required`, and autostart never starts one. The box
  host waits up to 210 seconds for an `unlock`.
- **The device list.** `seats/<id>/devices.json`, root `0600`. A password or key unlock makes the
  device present: no end while connected, 600 seconds after `left`, never more than 24 hours from
  the unlock, and none once the session stops, from a stop, the person's login at the machine or a
  unit found failed or down. An entry with neither `stay` nor a key goes with its presence. A file
  the supervisor can't read, or one that holds anything it wouldn't write, is no device, and
  `doctor` shows `device_list`.
- **Who is admitted.** A device that is present, or that stays signed in and has no key: a key
  never lets a device on without a fresh proof, so a copied pairing key alone can't reach the seat.
  The seat's `trust/<id>/punktfunk1-paired.json` holds only the box's records of admitted devices,
  each as the box wrote it; the other trust files copy as on any seat. The box's file is the door's:
  one that is missing, unreadable, over 8 MiB or not `{"clients": [...]}` gives an empty list. The
  list is rewritten on each change of who is admitted and checked every 2 seconds while the seat
  runs.
- **The box pin.** The first challenge or key proof pins the SHA-256 of the door's
  `/var/lib/punktfunk/native-cert.pem` in `box-pin`. While the certificate differs from it, every
  challenge and key proof is refused and `doctor` shows `box_pin` as an error; removing the file
  takes a new pin.
- **The audit line.** Every `unlock` writes one journal line under the target `punktfunk::audit`:
  `seat`, `account`, `uid`, `device`, `proof`, `result`, `device_failures` and
  `account_failures`. Never the password.

The row's [environment](#the-environment-a-seat-host-is-started-with) is a contract seat's, with
`PUNKTFUNK_SEAT_OWN_ACCOUNT=1` and `PUNKTFUNK_CONFIG_DIR=seats/<id>/config`. From it the runner
keeps the session on a private bus, leaves the user's other desktop processes alone and writes no
seat-only setup into their home, as `PUNKTFUNK_SEAT_OWNER` does on the owner's row.

