---
title: Multi-seat contract
description: How the host runs seats — on Windows the supervisor in the service, its pipe, the seat keeper and the connector reservation; on Linux the seat daemon, its socket and unit, and the owner's row behind the door; and the variables a seat host starts with.
---

The rules between the host and its seat supervisor, which runs one host per seat so several people
can play on one box. A normal install has one host on the console session, and none of this applies
to it. Windows seats need Windows Server and a CAL per seat; see
[Seats on Windows](/docs/windows-seats). Linux seats are [further down](#linux-seats).

## The supervisor

The `PunktfunkHost` service runs the supervisor beside the console host, as LocalSystem in session
0. It owns the seat accounts, the Windows sessions, RDP and the seat ledger in
`%ProgramData%\punktfunk\seats\`. Seat hosts run in jobs of its own, so a console logon or a console
host restart leaves them running; stopping the service logs every seat account off. After a
crash the next start does the same for sessions a seat's games left behind.

- **The pipe.** `\\.\pipe\punktfunk-seats` admits SYSTEM and Administrators only and refuses
  remote clients. Each connection carries one request: a four-byte big-endian length, then JSON,
  64 KiB at most. Commands are `list`, `create`, `adopt`, `credential`, `start`, `stop`, `delete`,
  `doctor`, `seating`, `enable` and `disable`. A request carries no credential of its own:
  `adopt` and `credential` carry an account's password, which the supervisor seals and forgets.
  The console host talks only to a pipe that LocalSystem serves, since the
  name is free while the service is down. It gives up on an answer after 60 seconds for `list`, 30
  minutes for `enable` when it installs the role, and 5 minutes for the rest.
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
- **Ledger.** An adopted row has `adopted_sid`, the account's SID, and `rdp_granted`, `true` when
  adopt put the account in Remote Desktop Users. A made row has neither, and its JSON is unchanged.
- **Identity.** A made account is the row's when its comment holds the seat's marker. An adopted
  one has no comment marker: it is the row's when its name resolves to `adopted_sid`. That SID is a
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
| `PUNKTFUNK_SEAT_ID` | 32 lowercase hexadecimal characters | Required with `PUNKTFUNK_SEAT_SESSION=1`. Missing or any other form: the host mints no audio devices. Ignored without `PUNKTFUNK_SEAT_SESSION=1`. |
| `PUNKTFUNK_SEAT_DISPLAY_SLOT` | `12`–`15` | The seat's connector. Not a number, out of range, or the marker absent: the host refuses every virtual-display session. |
| `PUNKTFUNK_TRUST_DIR` | the box's config directory | The seat reads the box's pairing store, `profiles.json`, per-device display overlays and `seat-defaults.json` from here, read only, and follows their changes. A relative path is ignored. |
| `PUNKTFUNK_PAIRING` | `refused` | Devices pair with the box. A knock is refused and no PIN window opens. A seat host refuses pairing with or without it. |
| `PUNKTFUNK_LIBRARY_DIR` | Windows: the box's config directory. Linux: the seat's trust copy, on every seat but the owner's | The seat reads the box's library from here, read only: `library*.json`, `library-metadata/`, the plugin grants and the firmware plugins staged; on Windows also the plugin manifests its entries launch through and the managed emulators. Play stats stay in the seat's own directory. A relative path is ignored. |
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
| `PUNKTFUNK_MDNS` | `0` |
| `PUNKTFUNK_AUDIO_OUTPUT_MODE` | `follow_default` |

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
  `mode_conflict` per device; every display or session settings write
  answers 409.
- **The Seat defaults.** The box writes `seat-defaults.json` into its config directory at start and
  after each settings change: `end_on_game_exit` (a seat's stream ends when its game exits) and
  `max_mode` (the largest mode a seat grants a device without a cap of its own). The seat re-reads
  it when it changes, so a change reaches the seat's next session. The box itself applies
  **Seats kept warm** and **Stop idle seats**. On Linux the file also carries `library_writable`,
  which the daemon reads from the box's copy when a seat starts (see the owner's libraries below),
  and `dualsense_usbip`, the box's own **DualSense over USB/IP** switch: a seat's DualSense is a
  USB/IP pad with a sound card exactly when the box's is.
- **The box's trust, its own key.** The seat never writes the pairing store or profiles. A grant
  changed in the box console applies to the seat's next check, and within about three seconds to
  its live sessions: a narrowed grant takes effect, and an unpair, an expiry or a removed profile
  ends the session. It mints and keeps its own identity
  in `PUNKTFUNK_CONFIG_DIR`, never the box's. The supervisor records its certificate's SHA-256 in
  the ledger while it runs. The box's `Redirect` carries that pin (field 7) and `enumerate` lists
  it as `seat.pin`, so a client pins the seat through the box it already trusts. A seat whose pin
  the box doesn't hold yet is unavailable. Sleep, restart and shut down are refused: the box's own
  host is the one that does them, and it ends the sessions on every seat first.
- **The box's library, on Windows.** A Windows seat runs no plugins. It lists and launches the
  box's titles as its own user, and answers every library change with 409. Titles from sources
  that belong to one account (Playnite, Game Bar, Amazon, itch, Hydra) are left out. It runs the
  box's emulators from `%ProgramData%\punktfunk\emulators` with the firmware the box's plugins
  staged. With seats on, an emulator installed from **Emulators** keeps each account's config and
  saves in that account's profile; RPCS3, Snes9x and Supermodel keep theirs beside the program,
  one set for every account. One installed while seats were off, or by an earlier version, keeps
  one set for every account until it is reinstalled there.
- **The box's library, on Linux.** The box's library is the owner's (see the owner's libraries
  below). A seat other than the owner's runs no plugins and lists and launches only what it can
  start as its own user: `steam_appid` with an id that fits 32 bits and `steam_ui`, with its own
  Steam, and `emulator`, with a copy it can run and a game file it can read. The owner's host
  checked an `emulator` entry against its plugin at publish. The seat answers every library change
  with 409. Every other title starts from the owner's home, or needs a manifest only that host has.
  The owner's host installs a Flathub emulator box-wide through `install_flatpak`, so every seat
  runs it with its own config and saves in `~/.var/app/<id>`. One the catalog ships another way
  (RPCS3, shadPS4, Supermodel, Vita3K, Xenia Canary) stays the owner's, as does a Flatpak in the
  owner's own installation until it is reinstalled from **Emulators**. At a launch the seat installs
  the RetroArch core the entry names into its own RetroArch and places the firmware from its
  library copy.
- **One profile.** The seat serves the profile whose seat it is. A connect that names any other
  profile, the owner's included, is refused as an unknown profile; one that names none plays as
  this seat's profile. The owner's row alone also serves the owner and the profiles that share the
  owner's desktop.
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
  open it or receive its input. A pad the host can't fence is not created. A Remote Desktop session
  into a single-seat box therefore no longer sees the console host's pads. The box has 16 pad
  indices; each host takes the lowest one that no other host's pad holds.
- **The desk's controllers are the desk's.** The box's host, which runs in the console session,
  writes that session into every physical game controller present and into each one that
  arrives, so a seat's HID, XUSB and XInput listings and its GameInput deliveries leave them
  alone; a device a Remote Desktop session redirected in keeps its own session. The write lands
  after the device is visible, because only the bus that creates a device can give it a session
  earlier (`design/desk-pads-and-seats-spike/results.md` S7). Measured from inside a seat at
  10 ms, a replugged desk pad was never listed there (S8). Two things stay with Windows:
  Windows.Gaming.Input on a seat keeps listing a plugged-in Xbox pad without its input, and a
  seat's pad is XInput index 1 if a desk Xbox pad is plugged after the seat's pad appeared, until
  the seat's next pad. The tag is wiped by the device's next start and written again on its
  arrival; the service's stop and the uninstaller clear it.
- **Its pads' audio is its own.** A Remote Desktop session lists only what its listener's audio
  endpoint enumerator returns. Through the seat listener, that is Remote Audio plus one
  "Wireless Controller" render endpoint per DualSense the seat host serves, in that pad's container
  and in that session alone. The seat host claims a pad's index by creating
  `Global\punktfunk-seat-pad-<session>-<index>` and its ring `…-pcm`, before the pad appears; the
  endpoint inside audiodg writes what games render there into the ring, and the seat host streams
  it as pad audio. A seat on `RDP-Tcp` has no pad audio.
- **No status tray.** A seat host doesn't start or supervise one; the supervisor is the control
  surface.
- **Steam starts as the seat's copy.** Once the copy exists, each seat start writes the seat
  account's `Software\Punktfunk\SeatSteam` (`Exe`, `Ipc` = `pfseat<seat id>`), a `steam://`
  handler in its classes hive (marker value `PunktfunkSeat`) and Steam's `Run` entry, all naming
  the copy under its IPC name. SYSTEM owns both keys; the seat may only read them. The box's
  public Desktop `Steam.lnk` and the shared Start menu's `Steam\Steam.lnk` start
  `punktfunk-tray.exe --steam "<their steam.exe>" <their arguments>`, which starts the copy for
  an account with `SeatSteam` and the box's Steam for any other. Their targets are kept in the
  Seats key's `SteamLinks` and put back when seats go off. Starting the box's `steam.exe`
  directly on a seat still closes the other Steam.
- **Not a host of its own to clients.** A seat host never advertises over mDNS, whatever
  `PUNKTFUNK_MDNS` says, and its `Welcome` carries management port `0`: its API answers on
  loopback only, so a client keeps the box's port, where the library comes from.

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
`%CommonProgramFiles(x86)%\Steam\drivers\Windows10\<x64|arm64>\`. Steam's first start puts them
there; its installer alone doesn't, and after that start Steam never has to run. Without them a
seat streams video with no audio. Pad audio needs neither: it comes from the seat listener.

```
powershell -File check-seat-audio.ps1
```

[`check-seat-audio.ps1`](https://git.unom.io/unom/punktfunk/src/branch/main/packaging/windows/check-seat-audio.ps1)
exits 1 when neither is present. A virtual cable is no substitute: the host never picks one for
desktop audio, `PUNKTFUNK_MIC_DEVICE` only pins the microphone, and a cable pinned with
`PUNKTFUNK_LOOPBACK_DEVICE` is shared by every seat.

## Linux seats

On Linux the supervisor is the root daemon `punktfunk-seats` (`punktfunk-seats.service`), not part
of the box host. Its state is `/var/lib/punktfunk-seats`, root's alone: the ledger in `seats/`, each
seat's home and trust copy, and the shared games folder. The paths below are under it. The box
host's directory, `/var/lib/punktfunk`, is the door's; the daemon reads it for the trust copies.
It starts one `punktfunk-seat@<user>.service` per seat. A seat is a
system-range user, `pf-seat-<n>`, with a logind session of its own, a headless compositor
(`kwin_wayland`, else gamescope) and a stock `punktfunk-host serve`, all run by
`/usr/libexec/punktfunk/seat-session`.

- **The package.** `punktfunk-seats` is its own package. The host's rpm, deb and pacman packages
  depend on it at their own version, and it installs the units without enabling them. NixOS turns
  it on with `services.punktfunk.seats.enable`. The SteamOS on-device build lays the same files out
  under `/opt/punktfunk` and `/etc` (`scripts/steamdeck/door.sh`).
- **The socket.** `/run/punktfunk/seats.sock` (`0660 root:punktfunk-door`) carries the same frames
  and commands as the Windows pipe. The daemon makes the `punktfunk-door` group at start when no
  package has, so an update that restarts it first still opens the socket to the door. It answers
  root and the `punktfunk` user, judged by the peer's credentials. Once the owner's row is
  adopted, the owner's uid gets an ACL entry on the socket file and may send `install_flatpak`
  and `remove_flatpak`, nothing else. Any other peer or command is closed unanswered. `enable`
  answers like `seating`, and `disable` is refused: seats are on while the daemon runs.
- **Box-wide Flatpaks.** `install_flatpak {id}` adds Flathub to the system installation when the
  box lacks it and installs the app there, for every account; each user keeps its own data in
  `~/.var/app/<id>`. `remove_flatpak {id, purge}` uninstalls it; `purge` deletes root's data of
  it, never a seat's. Both answer `flatpak {id, version}`. An id is reverse-DNS: letters, digits,
  `.`, `_` and `-`, at least one dot, at most 255 bytes, no leading `-`; any other is refused
  before `flatpak` runs. A change holds no lock on the ledger, a client waits up to 30 minutes
  for it, and one runs at a time: a second meanwhile answers `conflict`. `punktfunk-seats list`, `create <name>`, `adopt-owner <user>`,
  `start|stop|delete <id>`, `doctor` and `seating` send the same requests; `release-owner`
  deletes the owner's row and `stop-all` stops every live seat. `check-owner on|off <user>`
  sends none: it applies the switch rules below and prints `<uid> <home>` or `gone`.
- **The user.** Its comment is `punktfunk-seat=<id>`, and the daemon touches or deletes only an
  account whose comment matches exactly. The name is `pf-seat-1` to `pf-seat-4`; a create for
  any other is refused. The home is `seats/<id>/home`, mode `0700`. At start the daemon removes a
  seat account that no ledger row names, whose home is its own seat home and whose seat never
  started: what a create that stopped half-way left. It joins `render` and
  `punktfunk-games`, the shared games folder's group, which the owner joins too. Never `punktfunk`,
  which may power the box off, nor `input`, which reads every input device on the box.
- **The environment.** The variables in the tables above, minus the display slot,
  go into `/run/punktfunk/seats/<user>.env` (root `0600`), which systemd reads as root.
  `PUNKTFUNK_CONFIG_DIR` is `<home>/.config/punktfunk`. Each start adds a fresh
  `PUNKTFUNK_MGMT_TOKEN` and writes the same line to `seats/hosts/<id>/mgmt-token`, owned by the
  `punktfunk` user (root without one), so the box host reaches the seat's loopback API. Every seat
  user shares `punktfunk-games`, so no secret relies on group read.
- **The trust copy.** The seat reads `trust/<id>/`, not the box directory:
  `punktfunk1-paired.json`, `profiles.json`, `display-settings.json` (from the library copy,
  the owner's), `seat-defaults.json` and
  `profiles/`,
  `root:<seat user>` `0640`. The daemon recopies a file within 2 seconds of its change while the
  seat runs. The box's key never crosses, and a copy left by an older install is removed. A seat
  other than the owner's also finds the box's library there, its `PUNKTFUNK_LIBRARY_DIR`.
- **The box's library.** The owner's host writes the library; its plugins scan the owner's home.
  From the owner's `~/.config/punktfunk` the daemon copies `library.json`,
  `library-hidden.json`, `library-metadata.json`, `library-picks.json`, `library-scanners.json`
  and `library-metadata/` to `library/`, `root:punktfunk` `0640`, within 2 seconds of a change,
  and from there into each seat's trust copy. `plugin-run/plugin-grants.json` goes with them, and
  so does the firmware plugins staged, `plugin-state/<provider>/firmware/<platform>/<file>` at
  exactly that depth, up to 1 GiB a file. Nothing else of `plugin-run` or `plugin-state` is read:
  the plugins' tokens and sign-ins live there. A link, a file with a second link or one over its
  cap is not copied. Play stats stay with each host. A config folder it can't open, as in a home
  not mounted yet, leaves the copy as it was. Deleting the owner's row deletes `library/`.
- **Games.** `games/steamapps/` is one library every seat writes (group `punktfunk-games`, setgid,
  default ACL). `games` is sticky and closed to other users, so only the group reads the tree and
  a seat can't replace `steamapps`. Each seat start checks the folders and replaces a link or
  file a seat left where a bind target belongs. Each seat's own `compatdata`, `shadercache` and
  `downloading` under `seats/<id>/` are bind-mounted over the shared ones, so prefixes never
  cross. Everything that needs the binds must descend from the unit; a process started through
  `systemd-run --user` or a user service doesn't see them. The daemon repairs the ACL mask under
  `games/` after a seat stops and every 10 minutes, because a file created with mode `0644` is
  otherwise read-only to the other seats. A mask a seat closed on purpose, as with `chmod 600`,
  stays closed. A seat other than the owner's sees an empty `/run/media`: Steam in Game Mode
  adopts every mounted drive with a `steamapps` as a library, and a drive as mounted is the
  owner's alone. The owner's drives reach the seat as below.
- **The owner's libraries.** At each start the daemon reads every library the box owner's Steam
  lists (`steamapps/libraryfolders.vdf` of the Steam a seat is cloned from) and shows each at
  `seats/<id>/libraries/<key>`, `<key>` a hash of its path. Its `steamapps` is the owner's own,
  through an idmapped mount: the seat's Steam reads and writes it as the seat user, plays, updates
  and runs Proton from it, and every file stays the owner's on disk. The seat's own `compatdata`,
  `shadercache`, `downloading` and `temp` under `seats/<id>/private/<key>` cover the owner's, and
  the library's own path is inaccessible to the unit, so the owner's prefixes never reach a seat.
  The owner's `steamapps/libraryfolders.vdf` there is read-only to the seat.
  The unit mounts each library at `/run/punktfunk/libraries/<id>/<key>` (root only) in PID 1's
  namespace before it starts and unmounts it after, so a drive a seat shows stays in use until
  the seat stops. A library is skipped, with a line in the daemon's log, when it is gone, its
  `steamapps` isn't the owner's, or its filesystem takes no idmapped mount (anything but btrfs,
  ext4, xfs and f2fs). The daemon lists each shown library in both of the seat's
  `libraryfolders.vdf` copies, and drops one no longer shown and any under `/run/media`; Steam's
  own entries and fields stay. A new seat's list names its own Steam first, then the shared games
  folder, and each start adds the shared games folder if the list lacks it. The owner's Steam
  settings are never changed. Two Steam clients updating one game in a shared library at once
  take no lock.
  With **Seats update your games** off (`PUNKTFUNK_SEAT_LIBRARY_WRITABLE`, `library_writable` in
  the box's `seat-defaults.json`) the mount is read-only: a seat plays what is installed but can't
  update or install there, and its Steam can't rewrite the owner's appmanifests (it logs a failed
  app state write every few seconds while a game runs). What a launch writes gets a private copy
  under `seats/<id>/private/<key>`, found at each start: each `common/SteamLinuxRuntime_*/var`, each
  `common/SteamLinuxRuntime_*/*/files/.ref` and each `common/Proton*/dist.lock`. Without them a
  native game through the runtime or any Proton there fails with a read-only file system. A file
  that doesn't exist yet, such as the `dist.lock` of a Proton the owner never ran, stays read-only.
- **Granted roots.** At each start every root the owner granted a plugin, read or write, as the
  library copy's `plugin-grants.json` lists it, appears to the seat read-only at its own path, so
  a ROM the library names opens on the seat. On btrfs, ext4, xfs and f2fs the unit mounts it
  idmapped at its stage, as it does a library, and binds the stage there: the seat reads the
  owner's files as the owner. Elsewhere (exFAT, NTFS, FUSE) it is a plain read-only bind, and
  the seat reads what the files' modes let anyone read. A root in the owner's home is reached
  through an empty folder over that home, which hides the rest of it from the seat. One under
  `/run/media` shows inside the seat's empty `/run/media`: systemd mounts that first. A root is
  skipped, with a line in the daemon's log, when it is gone or no folder, its path has a
  character a unit file can't carry (`:`, `"`, `\`, `%`, `$` or a control character), it holds
  the owner's home or `/var/lib/punktfunk-seats` or lies in the latter, it lies in a shown
  library's `steamapps`, or the owner can't read it as themselves. A root inside another shows
  with it. A root granted later reaches the seat at its next start.
- **Save exchange.** Each seat has `seats/<id>/saves/out` and `saves/in`, the seat user's with
  group `punktfunk-games`, mode `2770`. The daemon makes them at each start and keeps what is in
  them until the seat is deleted. A seat's saves cross to and from the owner's plugins there;
  nothing writes them yet.
- **Stopping.** `pam_systemd` moves the runner into the session's scope, so stopping the unit
  can't reach it. The runner ends its children on `SIGTERM`, and `seat-reap` ends the session
  by the id the runner recorded. The daemon's stop then ends the seat's user
  (`loginctl terminate-user`, `SIGKILL` after 3 seconds; a user still running after 10 fails the
  stop with `user_busy`) and takes back its pads and USB devices: it shuts down every relay and
  USB/IP socket it handed the seat, also after a restart of the daemon. A stop of the owner's row
  ends neither: that account is a person's.
- **Pads.** A seat user can't open `/dev/uinput` or `/dev/uhid`: they are the console user's.
  A seat host that can write the node opens it itself, as the owner's host at the desk does
  through seat0's ACL. One that can't asks the daemon's pad broker, `/run/punktfunk/pads.sock`,
  for a pad by kind, and the daemon builds it from its own tables, keeps the kernel fd and hands
  the seat one end of a relay socket. A uinput pad's relay carries the seat's `input_event` frames down and rumble
  back; a uhid pad's carries the same `uhid_event`s the seat would write and read, minus
  `CREATE2` and `DESTROY`, which stay the daemon's. The pad lives until the seat hangs up. The
  socket admits a running seat's user only (the owner's row included), eight pads per seat. The
  daemon stamps each pad's `phys` as `punktfunk-seat:<account>/…`, and
  `65-punktfunk-seats.rules` runs `punktfunk-seats fence` on every `input` and `hidraw` node
  that carries it, on each event but `remove`: the node becomes that user's, `0600`, on a seat
  of its own, so neither the box's own Steam nor another seat sees it. A box without the daemon
  gives a seat no pad; the seat host says so. The daemon parks each relayed pad's fds in
  systemd's fd store, so a restart of the daemon keeps a seat's pads; `punktfunk-seats list`
  shows what each seat holds. While the daemon runs, every other hidraw node is the box's: it
  keeps its group and seat0's ACL but loses `other` access, which some distros grant on every
  hidraw (Bazzite's `MODE="0666"`) and which would hand the box's pads to every seat's Steam.
- **`/dev/uinput`.** Games read Steam Input's virtual controller, which the seat's Steam makes
  through `/dev/uinput`. Before a seat's unit starts, the daemon serves it a CUSE device,
  `/dev/punktfunk/uinput-<user>` (the user's, `0600`; the fence rule keeps it so on every udev
  event), and the unit binds it over `/dev/uinput`. Each open of it is one real uinput device the
  daemon makes and replays the seat's requests onto, 16 per seat. Only gamepad capabilities
  pass: `EV_SYN`, `EV_KEY` for joystick and gamepad buttons, `BTN_DPAD_*` and
  `BTN_TRIGGER_HAPPY*`, `EV_ABS` without pressure, tilt, tool or multitouch axes, `EV_MSC`,
  `EV_FF` and the accelerometer property. Any other bit is refused with `EPERM`, and the daemon
  logs each refused code once, so a seat never makes a keyboard or a mouse, which would reach
  the console's SysRq and `/dev/input/mice`. The daemon stamps each device's `phys` as
  `punktfunk-seat:<account>/steam/<n>`, whatever the seat asked, so the fence takes it. A 32-bit
  Steam's events and FF uploads are translated. The CUSE fd and each device's fd wait in the fd
  store, so a restart of the daemon keeps the bound node and the devices on it; a stop of the
  daemon ends them until the seat restarts. Before it stops, the unit's `ExecStop=` has the
  daemon answer every read a seat's Steam waits on, so none stays stranded in the kernel. The
  seat host's own pads still go through the broker, and a seat host offers no pen. It needs
  `/dev/cuse`, which the `cuse` module autoloads on first open; `doctor` reports it. The daemon
  makes `/dev/cuse` root's alone before each seat's node, so no other user can register the name
  a seat binds.
- **USB pads.** A seat's USB/IP pad (the virtual Deck, the Steam Controllers, the DualSense, the
  Switch 2) is served by the daemon, never by the seat. A seat that can't write vhci's `attach`
  sends a `Serve` with the pad kind, and the client pad's identity for a Steam Controller. The
  daemon starts a pad server, `punktfunk-seats serve-usb`, in a transient scope of its own, so a
  restart of the daemon leaves it running. The pad server answers every descriptor and control
  transfer from `pf-inject`'s tables. The daemon picks the vhci port, records it in
  `/run/punktfunk/pads/vhci/<port>` and writes `attach`, four ports per seat. The seat gets one end
  of a relay and sends only input reports, by interface. A kind takes only the report ids its
  table lists: the Deck's mouse and keyboard interfaces and the SC2's mouse and keyboard reports
  bind but never fire, the uhid SC2 included. An identity's serial passes as 1–16 letters and
  digits. The seat closing its relay ends the pad server and the device. An `Attach` from an
  older seat host is refused, and that host falls back to uhid pads; a socket an older daemon
  attached for a seat is detached at the next start. The fence maps every node under the port
  to the seat. While the daemon runs, a new vhci interface starts unauthorized and the fence
  authorizes it on its `add`. A seat's binds only when its device is one the host emulates
  (`usbip::EMULATED` in `pf-inject`) and the interface, as the kernel parsed it, is HID, audio,
  CDC-ACM or vendor-specific; anyone else's always. The attached socket waits in the fd store
  too, so a restarted daemon still drops the port's row when the device goes.
- **Game Mode.** The seat host keeps the seat's mode, `desktop` or `game`, in `seat-session` in
  its config directory, and starts in the one it ran last. It owns
  `com.steampowered.SteamOSManager1` on the seat's session bus and serves `SessionManagement1`,
  which `steamosctl` and Steam call; `steamos-session-select` and `steamos-desktop-return`, first
  on the seat's `PATH`, call it too. A switch never reaches the box's login. The runner masks the
  seat user's own `steamos-manager.service` for the session. The runner starts the host with
  `PUNKTFUNK_SEAT_HEADLESS=1`; on the owner's row only that host plays the seat's mode, and the
  owner's own host at the desk leaves Game Mode, its bus name and its gamescope to the box.

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

A seat's input and its portals stop at its own desktop, and its seatless session may not take a
logind block, so the door keeps the box awake. While anyone plays on a running seat it holds a
logind `sleep` block, which `49-punktfunk-door.rules` grants it. The screen at the desk can still
go dark. The owner's row at the desk needs none: that host is the desktop whose idle timer runs.

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
document (`every_route_has_a_side_on_the_door`). A device's `game/end` and `library/install/*`
go to the seat it plays on, as that device: the door's call carries `X-Punktfunk-Device`,
which a seat host honours only with its admin token and only for a device paired there. A
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
the one `door-helper on` recorded in `/var/lib/punktfunk-seats/owner`. Any other account, or any while nothing is recorded, answers
`owner_not_recorded`. Only root writes that file, so the door can't name another account. The
door reads it to switch off, so a supervisor that doesn't answer never keeps the door on.
Deleting the row never removes the account or its home. It takes a slot and ports from the same pools, and the door places the owner profile
and the profiles that share the owner's desktop on it.

| Variable | The owner's row |
|---|---|
| `PUNKTFUNK_SEAT_OWNER` | `1`: the seat host is the owner's own, and also serves the owner and the profiles that share the owner's desktop. |
| `PUNKTFUNK_SEAT_HEADLESS` | `1`, set by the runner, not the file: the host runs in the row's headless session, not at the desk. |
| `PUNKTFUNK_CONFIG_DIR`, `PUNKTFUNK_LIBRARY_DIR`, `PUNKTFUNK_HOST_NAME`, `PUNKTFUNK_GAMESTREAM` | Not set: the owner keeps `~/.config/punktfunk`, the library their plugins write, and their own settings. |

Every row sets `PUNKTFUNK_MDNS=0`, since only the door advertises. Every row also sets
`PUNKTFUNK_WEBTRANSPORT`: `1`, with its own `PUNKTFUNK_WEBTRANSPORT_PORT` (UDP 9783–9786, beside
its native port) and the box's `PUNKTFUNK_WEBTRANSPORT_ORIGINS`, while the box's Seat defaults
say it streams to browsers (`browser`, `browser_origins`), else `0`. A browser that sets
`PROFILES` is redirected to its seat's browser plane: the `Redirect` names that port and carries
the plane's certificate hash (field 8), which the browser pins and binds its credential to. A
running seat host follows the box's Seat defaults within seconds: its plane comes up on that
port, under those origins, and closes when the box stops streaming to browsers. A seat host
without Seat defaults follows these variables. With the owner's GameStream
setting on, the owner's row is the box's Moonlight host, on the owner's own pairings: the door
can't redirect Moonlight, and a contract seat never serves it. The row's environment file is
`/run/punktfunk/seats/<user>.env`, owned by the owner, and the owner's user `punktfunk-host` reads
it, so it moves to the row's ports at boot, logged in or not. The owner's own console,
`punktfunk-web.service`, doesn't start while that file exists: the door's console has the port.

- **One owner host.** The row's unit runs the owner's host in a `background` session, on a
  private D-Bus, beside whatever the owner has on the monitor. The owner's `punktfunk-host` and
  `punktfunk-kde-session` user units stand down while the unit's runtime directory exists.
- **The monitor wins.** The supervisor polls logind every 2 seconds. When the owner has a session
  on a seat that isn't the row's own, it stops the row's unit and starts the owner's user host. The
  owner's session is never ended.
- **Switching.** `PUT /api/v1/profiles/door` with `{"on": true}` or `{"on": false}` answers 202,
  or 204 when the box is already so. The owner's host starts `punktfunk-door-on@<user>.service`,
  the door `punktfunk-door-off@<user>.service`; both run `door-helper`, which moves the box's files between
  the owner's `~/.config/punktfunk` and `/var/lib/punktfunk` and turns the units on or off. The
  console's `web-session-key` and `web-session-epoch` move with them (on SteamOS, its
  `PUNKTFUNK_UI_SECRET` line), so a signed-in console stays signed in. `on` also gives the door
  the owner's `host-settings.json`, so the door's own rows start as the owner set them; before
  `off`, the door sends the rows it stored to the owner's host, and answers 503 while that host
  starts. The user
  must be in the `punktfunk-update` group, and a member switches the door for their own account
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
  lines to `/var/lib/punktfunk/host.env` before each door start, so an update or a later edit
  reaches the door. On NixOS the door also takes the two port keys from
  `services.punktfunk.host.settings`, and its console follows `services.punktfunk.web.bind`.
