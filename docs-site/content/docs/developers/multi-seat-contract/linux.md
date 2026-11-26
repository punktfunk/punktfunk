---
title: Linux seats
description: The Linux half of the seat contract — the root daemon, a seat's user, the trust and library copies, shared games, and how a seat gets its controllers.
---

On Linux the supervisor is the root daemon `punktfunk-seats` (`punktfunk-seats.service`), not part
of the box host. Its state is `/var/lib/punktfunk-seats`, root's alone; the paths below are under
it. The box host's directory, `/var/lib/punktfunk`, is the door's; the daemon reads it for the
trust copies. It starts one `punktfunk-seat@<user>.service` per seat: a system-range user,
`pf-seat-<n>`, with a logind session of its own, a headless compositor (`kwin_wayland`, else
gamescope) and a stock `punktfunk-host serve`, all run by `/usr/libexec/punktfunk/seat-session`.
`punktfunk-seats doctor` checks systemd, logind, the groups, a GPU render node, a compositor and
the installed unit.

## The package

`punktfunk-seats` is its own package. The host's rpm, deb and pacman packages
depend on it at their own version, and it installs the units without enabling them. NixOS turns
it on with `services.punktfunk.seats.enable`. The SteamOS on-device build lays the same files out
under `/opt/punktfunk` and `/etc` (`scripts/steamdeck/door.sh`).

## The CLI

`punktfunk-seats list`, `create <name>`, `adopt-owner <user>`,
`start|stop|delete <id>`, `doctor`, `seating`, `pads`, `lend <seat id> <key>` and
`take-back <key>` send the [socket's](/docs/developers/multi-seat-contract/supervisor) requests;
`release-owner` deletes the owner's row and `stop-all` stops every live seat.
`door on|off <user>` and `door ports` send none and need no running supervisor: they are the
[door switch](/docs/developers/multi-seat-contract/door#switching) itself, root's, which `door-helper` execs.

## Box-wide Flatpaks

`install_flatpak {id}` adds Flathub to the system installation when the
box lacks it and installs the app there, for every account; each user keeps its own data in
`~/.var/app/<id>`. `remove_flatpak {id, purge}` uninstalls it; `purge` deletes root's data of
it, never a seat's. Both name an app: an id that names a runtime is never installed or removed
through them. Both answer `flatpak {id, version}`. An id is reverse-DNS: letters, digits,
`.`, `_` and `-`, at least one dot, at most 255 bytes, no leading `-`; any other is refused
before `flatpak` runs. One change runs at a time: a second meanwhile answers `conflict`.

## The user

Its comment is `punktfunk-seat=<id>`, and the daemon touches or deletes only an
account whose comment matches exactly. The name is `pf-seat-1` to `pf-seat-4`; a create for
any other is refused. The home is `seats/<id>/home`, mode `0700`. It joins `render` and
`punktfunk-games`, the shared games folder's group, which the owner joins too. Never `punktfunk`,
which may power the box off, nor `input`, which reads every input device on the box.

## The environment

The [variables](/docs/developers/multi-seat-contract/supervisor#the-environment-a-seat-host-is-started-with) go into
`/run/punktfunk/seats/<user>.env` (root `0600`), which systemd reads as root. The seat's
`mgmt-token` is owned by the `punktfunk` user (root without one); every seat user shares
`punktfunk-games`, so no secret relies on group read.

## The trust copy

The seat reads `trust/<id>/`, not the box directory:
`punktfunk1-paired.json`, `profiles.json`, `display-settings.json` (from the library copy,
the owner's), `seat-defaults.json` and `profiles/`, `root:<seat user>` `0640`, and the seat's
[profile locks](/docs/developers/multi-seat-contract/profile-locks) when the box has any. The daemon recopies a file within 2
seconds of its change while the seat runs. The box's key never crosses. A seat other than the owner's also finds the box's library there, its
`PUNKTFUNK_LIBRARY_DIR`.

## The box's library

The owner's host writes the library; its plugins scan the owner's home.
From the owner's `~/.config/punktfunk` the daemon copies `library.json`, `library-hidden.json`,
`library-metadata.json`, `library-overrides.json`, `library-picks.json`, `library-scanners.json`
and `library-metadata/` to `library/`, `root:punktfunk-door` `0640`, within 2 seconds of a change,
and from there into each seat's trust copy. Each profile's layer, `library-profiles/<profile id>/`
with `library-hidden.json`, `library-overrides.json`, `library-picks.json`,
`library-scanners.json`, `own-titles.json` and `session-settings.json`, goes to `library/` for
every profile; a
seat's trust copy gets only the folder of the profile whose `os_account` names that seat in the
box's `profiles.json`, and loses one that stops being its own.
`plugin-run/plugin-grants.json` goes with them, and so
does the firmware plugins staged, `plugin-state/<provider>/firmware/<platform>/<file>` at exactly
that depth, up to 1 GiB a file. Nothing else of `plugin-run` or `plugin-state` is read: the
plugins' tokens and sign-ins live there. A link, a file with a second link or one over its cap is
not copied. Play stats stay with each host. A config folder it can't open, as in a home not
mounted yet, leaves the copy as it was. Deleting the owner's row deletes `library/`.

`sync_library` makes that copy at once and answers `synced` with whether it changed. The
supervisor also reads both under their first names, `level_library` and `leveled`. It waits
1.5 seconds for a change that holds the copies, such as a seat's removal, then answers the error `busy`.

## Games

`games/steamapps/` is one library every seat writes (group `punktfunk-games`, setgid,
default ACL). Each seat's own `compatdata`, `shadercache` and `downloading` under `seats/<id>/`
are bind-mounted over the shared ones, so prefixes never cross. A seat other than the owner's
sees an empty `/run/media`: Steam in Game Mode adopts every mounted drive with a `steamapps` as
a library, and a drive as mounted is the owner's alone.

## The owner's libraries

At each start the daemon shows every library the owner's Steam lists
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

## Granted roots

At each start every root the owner granted a plugin, as the library copy's
`plugin-grants.json` lists it, appears to the seat read-only at its own path, so a ROM the
library names opens on the seat: idmapped on btrfs, ext4, xfs and f2fs, so the seat reads the
owner's files as the owner, and a plain read-only bind elsewhere. A root in the owner's home is
reached through an empty folder over that home. A root granted later reaches the seat at its
next start.

The mount rules, and the roots and libraries each skips, are in
`crates/host/pf-seats/src/linux/libraries.rs`.

## Save exchange

Each seat has `seats/<id>/saves/out` and `saves/in`, the seat user's, mode
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

## Stopping

The daemon's stop ends the seat's user (`loginctl terminate-user`; a user still
running after 10 seconds fails the stop with `user_busy`) and takes back every pad relay and
USB/IP socket it handed the seat. A stop of the owner's row ends neither: that account is a
person's.

## Pads

A seat user can't open `/dev/uinput` or `/dev/uhid`: they are the console user's. A
seat host that can't write the node asks the daemon's pad broker, `/run/punktfunk/pads.sock`,
for a pad by kind (`pf_inject::pad_broker::Request`). The daemon builds it from its own tables,
keeps the kernel fd and hands the seat one end of a relay socket: a uinput pad's relay carries
the seat's `input_event` frames down and rumble back, and a uhid pad's carries the
`uhid_event`s the seat would write and read, minus `CREATE2` and `DESTROY`, which stay the
daemon's. The pad lives until the seat hangs up. The socket admits a running seat's user only
(the owner's row included), eight pads per seat. A box without the daemon gives a seat no pad.

## Who owns a pad's device nodes

The daemon stamps each pad's `phys` `punktfunk-seat:…` and records the device in
`/run/punktfunk/pads/vouched`, root's alone. `65-punktfunk-seats.rules` runs
`punktfunk-seats node-owner` on every stamped `input` and `hidraw` node: a recorded device's node
becomes that seat user's, `0600`, on a seat of its own, so neither the box's own Steam nor
another seat sees it, and any other stamped node is root's. While the daemon runs, every other
hidraw node keeps its group and seat0's ACL but loses `other` access.

An input node that udev classes as a keyboard or a mouse, and not as a joystick, is root's alone
whichever seat device it belongs to. An emulated Steam Deck carries a keyboard and a mouse
beside its controller, and whoever can write such a node types into the kernel's own keyboard
and pointer handlers, which serve the box's console.

## Lent controllers

The operator lends a controller plugged into the box to a seat: the seat's
ledger row keeps it, up to four, each key on one row. While a lend exists the daemon records it
under `/run/punktfunk/pads/lent/`, root's alone, one file per controller: the seat's user, or a
mark for parked. A record goes live before the seat's unit starts and parks when the seat
stops or fails; a deleted row gives the controller back to the desk. The key is the
controller's vendor, product and serial, found in any port, or its vendor, product and USB port
when it has no serial. A record also holds the port the lend found the controller in: while a
second device that carries the same key is plugged in, only the one in that port is the lent
one. While that directory exists, `65-punktfunk-seats.rules` runs
`punktfunk-seats node-owner` on every node of a real USB device. A lent controller's nodes become
the seat user's as udev creates them, `0600`, without `uaccess`, on a seat name of its own, or
root's alone while parked; its keyboard and mouse nodes are root's alone always, so a Steam
Controller's desktop mode reaches nobody, and so is the device's own node under
`/dev/bus/usb`, through which raw USB could take a keyboard interface from the kernel. On
every change of hands the daemon unbinds the USB device from its driver and binds it back:
its interfaces go and every open handle ends, a raw USB one too. Between the two it sends the
device a `change` event: the device itself stays, so only that runs the rules on its own node, and each new
node takes its seat from the device. A controller lent to the owner's row also opens for the
owner's session at the desk, the same uid. The seat host never serves it as a pad, so it is in no session's pad list and its input
is no presence.

## Lending

The daemon reads the hardware itself and lends only a device on a real USB bus
that is no hub, whose every interface is HID, audio, CDC-ACM, CDC data or vendor, and that is a
game controller by udev's joystick class or a known receiver; what the door says about a
device counts for nothing. `lend {id, key}` and `take_back {key}` change hands, `pads` lists
the controllers plugged in and every lent one, each answering `pads`. The management API's
admin lane, box scope, serves `GET /api/v1/profiles/pads` and
`PUT /api/v1/profiles/pads/{key}` with `{"profile": "<id>"}` or `{"profile": null}`. The `PUT`
answers 409 for a profile with no seat, a fifth controller, or one that isn't plugged in or
isn't a game controller; taking back an unplugged one works. Where the host can't lend, `GET`
answers `supported: false`.

## `/dev/uinput`

Before a seat's unit starts, the daemon serves it a CUSE device,
`/dev/punktfunk/uinput-<user>` (the user's, `0600`), and the unit binds it over `/dev/uinput`
for the seat's Steam Input. Each open is one real uinput device the daemon makes, 16 per seat.
Only gamepad capabilities pass: `EV_SYN`, `EV_KEY` for joystick and gamepad buttons,
`BTN_DPAD_*` and `BTN_TRIGGER_HAPPY*`, `EV_ABS` without pressure, tilt, tool or multitouch axes,
`EV_MSC`, `EV_FF` and the accelerometer property. Any other bit is refused with `EPERM`, so a
seat never makes a keyboard or a mouse. It needs `/dev/cuse`; `doctor` reports it.

## USB pads

A seat's USB/IP pad (the virtual Deck, the Steam Controllers, the DualSense, the
Switch 2) is served by the daemon, never by the seat. A seat that can't write vhci's `attach`
sends a `Serve` with the pad kind, and the client pad's identity for a Steam Controller, whose
serial passes as 1–16 letters and digits. The daemon starts a pad server,
`punktfunk-seats serve-usb`, which answers every descriptor and control transfer from
`pf-inject`'s tables, records the vhci port in `/run/punktfunk/pads/vhci/<port>` and writes
`attach`, four ports per seat. The seat gets one end of a relay and sends only input reports,
by interface. Closing the relay ends the pad server and the device.

## Game Mode

The seat host keeps the seat's mode, `desktop` or `game`, in `seat-session` in
its config directory, and starts in the one it ran last. It owns
`com.steampowered.SteamOSManager1` on the seat's session bus and serves `SessionManagement1`,
which `steamosctl` and Steam call; `steamos-session-select` and `steamos-desktop-return`, first
on the seat's `PATH`, call it too. A switch never reaches the box's login. On the owner's row
only the headless host plays the seat's mode; the owner's own host at the desk leaves Game Mode,
its bus name and its gamescope to the box. A seat never updates the box: the supervisor binds
`/run/punktfunk/seat-no-update` over each of `steamos-polkit-helpers/steamos-update`,
`steamos-polkit-helpers/jupiter-biosupdate` and `jupiter-initial-firmware-update` the box has.
