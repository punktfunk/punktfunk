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
  replace the owner's Playnite library. The doctor's `account` row reports the membership.
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
| `GET /api/v1/profiles/seating` | Returns `enabled`, `platform` (`windows`, `linux` on a door, or `other`) and `checks`: Windows Server, the Remote Desktop Session Host role, licensing and a graphics card. A desktop edition whose TermService loads a wrapper DLL instead of `termsrv.dll` passes with a warning and skips the role and licensing checks. |
| `PUT /api/v1/profiles/seating` with `{"enabled": true, "allow_rdp_from_network": false}` | Turns seats on. |
| The same with `"prepare": true` | First installs `RDS-RD-Server` and sets licensing to Per User where missing, then turns seats on. A role that needs a restart answers a `restart` error and leaves seats off. |
| `PUT /api/v1/profiles/seating` with `{"enabled": false}` | Stops every seat and turns seats off. The accounts stay. |
| `GET /api/v1/profiles/doctor` | Returns the supervisor's report: one sentence per row, `seat_id` and `seat_name` on a seat's rows. |

A failed check changes nothing: the answer is 200 with `enabled: false` and the check as an
`error`. When every check passes, turning on:

1. Writes the reservation marker.
2. Installs `pf_vdisplay_seats.inf` from `staging\pfvdisplay\` beside `punktfunk-host.exe`.
3. Turns the Remote Desktop listener on if it was off, and limits the **Remote Desktop**
   firewall rules to `127.0.0.0/8` unless `allow_rdp_from_network` is `true`, with the two
   port 3389 rules the Session Host role adds. The service limits them again each time it
   starts.
4. Adds the seat listener `punktfunk-tcp`: a copy of `RDP-Tcp` on port 3390 whose sessions load
   `pf_seat_audio.dll`, with port 3390 blocked from every address but loopback. The x64 build goes
   to `System32\punktfunk` and the x86 build to `SysWOW64\punktfunk`. Every seat start brings
   them up to date. Not on ARM64.
5. Records the Remote Desktop certificate, as `punktfunk-seat-keeper trust` does.

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
| `PUNKTFUNK_SEAT_SESSION` | `1` | Marks a seat host. Unset or any other value is the console host. A seat host is never the door: `--door` and `PUNKTFUNK_DOOR` are ignored on it. |
| `PUNKTFUNK_SEAT_ID` | 32 lowercase hexadecimal characters | Required with `PUNKTFUNK_SEAT_SESSION=1`. Missing or any other form: the host mints no audio devices. Ignored without `PUNKTFUNK_SEAT_SESSION=1`. |
| `PUNKTFUNK_SEAT_DISPLAY_SLOT` | `12`–`15` | The seat's connector. Not a number, out of range, or the marker absent: the host refuses every virtual-display session. |
| `PUNKTFUNK_TRUST_DIR` | the box's config directory | The seat reads the box's pairing store, `profiles.json`, per-device display overlays and `seat-defaults.json` from here, read only, and follows their changes. A relative path is ignored. |
| `PUNKTFUNK_PAIRING` | `refused` | Devices pair with the box. A knock is refused and no PIN window opens. A seat host refuses pairing with or without it. |
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
| `PUNKTFUNK_MDNS` | `0` |
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
  it when it changes, so a change reaches the seat's next session. The box itself applies
  **Seats kept warm** and **Stop idle seats**. On Linux the file also carries `library_writable`,
  which the daemon reads from the box's copy when a seat starts (see the owner's libraries below),
  and `dualsense_usbip`, the box's own **DualSense over USB/IP** switch: a seat's DualSense is a
  USB/IP pad with a sound card exactly when the box's is.
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
- **Its pads are its own.** Every virtual device a host creates, its pads and its virtual mouse,
  carries the host's session id (`DEVPKEY_Device_SessionId`), so another session can't list it,
  open it or receive its input. A pad the host can't fence is not created. A Remote Desktop session
  into a single-seat box therefore no longer sees the console host's pads. The box has 16 pad
  indices; each host takes the lowest one that no other host's pad holds.
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
- **Games.** `games/steamapps/` is one library every seat writes (group `punktfunk-games`, setgid,
  default ACL). Each seat's own `compatdata`, `shadercache` and `downloading` under `seats/<id>/`
  are bind-mounted over the shared ones, so prefixes never cross. Everything that needs the
  binds must descend from the unit; a process started through `systemd-run --user` or a user
  service doesn't see them. The daemon repairs the ACL mask under `games/` after a seat stops and
  every 10 minutes, because a file created with mode `0644` is otherwise read-only to the other
  seats. A seat other than the owner's sees an empty `/run/media`: Steam in Game Mode adopts
  every mounted drive with a `steamapps` as a library, and a drive as mounted is the owner's
  alone. The owner's drives reach the seat as below.
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
  own entries and fields stay. The owner's Steam settings are never changed. Two Steam clients
  updating one game in a shared library at once take no lock.
  With **Seats update your games** off (`PUNKTFUNK_SEAT_LIBRARY_WRITABLE`, `library_writable` in
  the box's `seat-defaults.json`) the mount is read-only: a seat plays what is installed but can't
  update or install there, and its Steam can't rewrite the owner's appmanifests (it logs a failed
  app state write every few seconds while a game runs). What a launch writes gets a private copy
  under `seats/<id>/private/<key>`, found at each start: each `common/SteamLinuxRuntime_*/var`, each
  `common/SteamLinuxRuntime_*/*/files/.ref` and each `common/Proton*/dist.lock`. Without them a
  native game through the runtime or any Proton there fails with a read-only file system. A file
  that doesn't exist yet, such as the `dist.lock` of a Proton the owner never ran, stays read-only.
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
  daemon ends them until the seat restarts. The seat host's own pads still go through the
  broker, and a seat host offers no pen. It needs `/dev/cuse`, which the `cuse` module autoloads
  on first open; `doctor` reports it.
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
is the door: `punktfunk-host serve --door` (or `PUNKTFUNK_DOOR=1`), run as the `punktfunk` user by
`punktfunk-door.service` with `PUNKTFUNK_CONFIG_DIR=/var/lib/punktfunk`. It advertises, pairs,
serves the management API and places every connect, and opens no display, capture, input or
audio device. A connect is answered with a `Redirect` to a seat, or refused with
`SEAT_UNAVAILABLE`: the door never streams. `GET /api/v1/host` reports `door: true`. A host
started with `PUNKTFUNK_SEAT_SESSION=1` is a seat host and never the door, however the door is
asked for.

The box owner is a seat like any other. `adopt-owner <user>` adds a ledger row for an ordinary
account that exists (uid 1000 up, never a seat's), and only for the one `door-helper on` recorded
in `/var/lib/punktfunk-seats/owner`. Any other account, or any while nothing is recorded, answers
`owner_not_recorded`. Only root writes that file, so the door can't name another account.
Deleting the row never removes the account or its home. It takes a slot and ports from the same pools, and the door places the owner profile,
the profiles that share the owner's desktop and the light seats on it.

| Variable | The owner's row |
|---|---|
| `PUNKTFUNK_SEAT_OWNER` | `1`: the seat host is the owner's own, and also serves the owner and light-seat profiles. |
| `PUNKTFUNK_SEAT_HEADLESS` | `1`, set by the runner, not the file: the host runs in the row's headless session, not at the desk. |
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
