---
title: Windows seats
description: The Windows half of the seat contract — the service, seat accounts, the connector reservation, turning seats on and off, what a seat host owns, and the two drivers a seat needs.
---

On Windows a seat is a seat account's loopback Remote Desktop session with a host of its own
inside it. Read here what the service does for it and what that host owns.

## The supervisor on Windows

On Windows the supervisor owns the seat accounts, the Windows sessions and RDP. The console host
talks only to a pipe that LocalSystem serves, since the name is free while the service is down.
Seat hosts run in jobs of the supervisor's own, so a console logon or a console host restart leaves
them running; stopping the service logs every seat account off. After a crash the next start does
the same for sessions a seat's games left behind.

### The keeper

`punktfunk-seat-keeper.exe`, beside `punktfunk-host.exe`, holds one seat's
loopback RDP session open. The supervisor hands it the seat's credentials on stdin, never on the
command line. Run `punktfunk-seat-keeper trust` from an elevated prompt once, before the first
seat: it records the RDP certificate the keeper will accept. Turning seats on does this too,
and replaces an earlier record. Each seat start compares the certificate TermService presents
now with the record; a change fails the seat with `rdp_pin_changed` until seats are turned
off and on. The keeper's stderr goes to the seat's `host.log`.

### The seats group

Every seat account is in the local group `punktfunk-seats`, rejoined at
each start. The host denies that group on `%ProgramData%\punktfunk\ingest`, so a seat can't
replace the owner's Playnite library. The doctor's `account` row reports the membership.

### Seats on and off

Without the reservation marker below the ledger is still listed, and
every start answers `seats_off`. [Turn seats on and off](#turn-seats-on-and-off) writes the marker.

## A seat on an existing account

A seat signs in to an account the supervisor made, `pf_seat1` to `pf_seat4`, or to a local
standard account the box already has. The console asks for the second with `seat: true`,
`seat_account` and `seat_password` on `POST /api/v1/profiles`. `PUT /api/v1/profiles/{id}` with
`seat_password` replaces its password.

### Commands

`adopt` carries `seat`, as `create` does, plus the account's `sid` and
`password`, and answers `created`. `credential` carries `id` and `password`, checks the password
with a sign-in, seals it in place of the old one and answers `resealed` with the row.

### Attempts

Two wrong passwords for one account, by `adopt` and `credential` together, reach
the sign-in in any 15 minutes. A third answers `unlock_locked_out` with the wait in its
sentence before any sign-in, so the supervisor never drives the account into Windows' lockout.

### The row

An adopted row has `adopted_sid`, the account's SID, and `rdp_granted`, `true` when
adopt put the account in Remote Desktop Users. A made account is the row's when its comment
holds the seat's marker; an adopted one when its name resolves to `adopted_sid`. That SID is a
local user's, `S-1-5-21-<n>-<n>-<n>-<rid>` with a RID of 1000 or more.

### Names

An adopted account's name follows the SAM rule: 1 to 20 characters, spaces and
non-ASCII letters allowed, no control characters, none of `"/\[]:;|=,+*?<>`. A made one stays
`pf_seat<n>`.

### Passwords

1 to 256 bytes of UTF-8, no control characters, sealed with DPAPI under SYSTEM
as a made account's is.

### Adopt

Adopt refuses an administrator, an account denied Remote Desktop sign-in, an account with a
session on this PC, and a password that doesn't sign in. Then it seals the password, adds the
account to Remote Desktop Users where it is missing and to `punktfunk-seats`, and grants it
`SeDenyInteractiveLogonRight`. It sets no account flag and writes no comment. A step that fails
undoes the ones before it.

### Release

Release is `delete` on an adopted row, by the row's SID. It removes the deny right, the
`punktfunk-seats` membership, Remote Desktop Users only when `rdp_granted`, the sealed
password, and the Steam launchers seats wrote into the account's hive: `SeatSteam`, our
`steam://` handler, and a `Run\Steam` line naming a seat's IPC name. It never deletes the
account or its profile.

### Picker

`GET /api/v1/profiles/windows-accounts` serves **Existing account** and take-turns
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
4. Blocks port 3390 from every address but loopback with two firewall rules, then adds the seat
   listener `punktfunk-tcp`: a copy of `RDP-Tcp` on port 3390 whose sessions load
   `pf_seat_audio.dll`. The rules go first because TermService opens the port as soon as the
   listener is written. When the rules can't be added the listener isn't written, a listener
   already in place is disabled, and seats use port 3389. The x64 build goes to
   `System32\punktfunk` and the x86 build to `SysWOW64\punktfunk`. Every seat start brings them
   up to date. Not on ARM64.
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

## A Windows seat host

### One connector, one lock

A seat host creates its virtual display on its own connector only
and holds the mutex `Global\punktfunk-vdisplay-manager-seat-<slot>`; the console host holds
`Global\punktfunk-vdisplay-manager`. Hosts on different connectors start independently. A second
host on the same connector can't open the display driver.

### Driver calls are per process

Every host sees, changes and clears only the monitors it
created. A crashed host's monitors depart when its handles close or it misses the driver
watchdog, so no host reaps a neighbour's.

### Launches stay in its session

Games, store launches and hook commands run as the signed-in
user of the host's own Windows session, not the console's.

### Its audio devices are its own

It mints `Punktfunk Speakers [seat <id>]` and
`Punktfunk Microphone [seat <id>]`, matches them by a marker derived from the seat id, and never
adopts a neighbour's. It captures its own speakers and never changes the box's default playback
or recording device.

### Its virtual mouse is its own

The resident HID mouse that makes Windows draw a cursor into
the stream is `pf_mouse_<slot>`, with mailbox `Global\pfmouse-boot-<slot>`. The console host uses
index `0`. Only the console host sends input through it: HID input reaches the console session, so
a seat host injects with `SendInput`.

### Its pads are its own

Every virtual device a host creates, its pads and its virtual mouse,
carries the host's session id (`DEVPKEY_Device_SessionId`), so another session can't list it,
open it or receive its input. A pad the host can't fence is not created. The box has 16 pad
indices; each host takes the lowest one that no other host's pad holds.

### The desk's controllers are the desk's

The service writes the console session into every
physical game controller present and into each one that arrives, so a seat's HID, XUSB and
XInput listings and its GameInput deliveries leave them alone; a device a Remote Desktop
session redirected in keeps its own session. The tag is wiped by the device's next start and
written again on its arrival; a new console session moves it, and the service's stop and the
uninstaller clear it. The service is the one writer: a process in a session can't locate a
devnode tagged for another session, and session 0 locates them all. What Windows still shows
a seat:
[Seats on Windows](/docs/windows-seats#a-controller-plugged-into-the-pc-appears-on-a-seat).

### Lent controllers

The operator lends a controller plugged into the box to a seat, as on
[Linux](/docs/developers/multi-seat-contract/linux#lent-controllers): the ledger row keeps it, the same commands and routes change hands.
The key is the USB device's ids and the serial or port id in its instance id. Every HID and
XUSB devnode under a lent device carries the seat's session while the seat runs and a
session nobody has while it doesn't, so nobody lists a parked controller. Its keyboard and
mouse collections are removed while it is lent and come back with its next start: a
controller's desktop mode would type and point at the console whatever the tag. The supervisor
tells the fence each seat's session as a cycle enters and leaves it. A change of holder
restarts the USB device, which ends the last holder's open handles; a parked controller
going live changes in place. The service lends a device only when every interface is HID,
audio, CDC-ACM, CDC data or vendor and a game controller's HID collection or an XUSB
interface lies under it. The tag hides and does not deny: the console's Raw Input, an open
of a known path and WGI's Xbox listing still reach a lent controller, and its audio
endpoints stay the console's.

### Its pads' audio is its own

A Remote Desktop session lists only what its listener's audio
endpoint enumerator returns. Through the seat listener, that is Remote Audio plus one
"Wireless Controller" render endpoint per DualSense the seat host serves, in that pad's container
and in that session alone. The seat host claims a pad's index by creating
`Global\punktfunk-seat-pad-<session>-<index>` and its ring `…-pcm`, before the pad appears; the
endpoint inside audiodg writes what games render there into the ring, and the seat host streams
it as pad audio. A seat on `RDP-Tcp` has no pad audio.

### Steam starts as the seat's copy

Once the copy exists, each seat start writes the seat
account's `Software\Punktfunk\SeatSteam` (`Exe`, `Ipc` = `pfseat<seat id>`), a `steam://`
handler in its classes hive (marker value `PunktfunkSeat`) and Steam's `Run` entry, all naming
the copy under its IPC name. SYSTEM owns both keys; the seat may only read them. The box's
public Desktop `Steam.lnk` and the shared Start menu's `Steam\Steam.lnk` start
`punktfunk-tray.exe --steam "<their steam.exe>" <their arguments>`, which starts the copy for
an account with `SeatSteam` and the box's Steam for any other. Their targets are kept in the
Seats key's `SteamLinks` and put back when seats go off.

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
