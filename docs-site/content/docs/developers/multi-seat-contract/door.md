---
title: The owner's row and the door
description: How a Linux box stays reachable with nobody logged in — the door, which calls it answers itself, how events cross, and the seat the owner plays on.
---

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

## Keeping the box awake

A seat's seatless session may not take a logind block, so the door keeps the box awake: while
anyone plays on a running seat it holds a logind `sleep` block, which `49-punktfunk-door.rules`
grants it. The screen at the desk can still go dark.

## The library every client reads

Every client reads its library from the door, which has `PUNKTFUNK_LIBRARY_DIR` set to the box's
library, `/var/lib/punktfunk-seats/library`: all of it, for every profile, read only. A cover
there names a file in the owner's home, which the door can't read, so it asks the owner's host
for it over that host's loopback API. While that host is down the cover answers 404; asking for
one never starts the owner's session.

## Which calls the door answers

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
[person's](/docs/developers/multi-seat-contract/profile-locks#the-person) calls go to the owner's host too, once the door found the person. A
call starts a stopped seat and answers 503, except `host`, `status` and `diagnostics`, which
wake nobody; a host that doesn't answer is 502, and so is every call while the seat supervisor
doesn't answer. With no owner row the owner's calls answer 503, and so does a `host/settings`
change to any row but the door's own: the door's files never stand in for the owner's.
`events` and `session/*/pads` stream.

## Events

Events cross the same way. Each running seat's events join the door's bus with `seat` naming
the profile whose host raised them, so the console sees sessions, games, library and plugin
changes where they happen. The door's own (`pairing.*`, `access.*`, `host.*`, `action.*`,
`update.*`) go to the owner's host's `POST /events/relay`, whose hooks run them in the
owner's session; the door runs no hooks. The two sets are disjoint, so nothing comes back
around. A knock (`pairing.pending`) wakes a stopped owner's session and reaches its host once
that host answers, within two minutes, a supervisor that doesn't answer meanwhile included;
any other door event a stopped owner's host misses.

The door lists the box's copy of the library, so the owner's `library.changed` joins the bus
after the door asked the supervisor for `sync_library`: a client that re-reads on the event
reads the change. The door waits 2 seconds for that answer, then sends the event anyway.

A seat's `game.*` goes to the same route with its `seat` kept: the owner's plugins hear it, and
no host's hooks run on it. A seat's launch is held for the box's plugins, so its
`game.launching` carries a `hold` token, and the launch waits up to 130 seconds for
`POST /game/holds/{token}/release`. The door sends it once the owner's host has run the plugins
that hold the stage, whatever they answered, and at once when that host isn't running. An event
a seat host lists with a `seat` already is one a host relayed there, and is never taken back.

## The console handoff, idling and updates

The door's console also accepts a console handoff signed with the owner's host's token, the one
the owner's own `ctl console-url` holds: the door keeps it in `owner-handoff-key` in its own
directory, level with the owner's row, and never a contract seat's, whose user can read their
own. The owner's row never stops for being idle: it runs the plugins and keeps the library. A
seat host whose binary an update replaced restarts once nobody plays on it, and its runner
starts the new one.

## Display settings and hooks

`display-settings.json` and `hooks.json` are the owner's host's alone: `door-helper` copies
neither, the owner's host writes the per-device overlays too, the supervisor copies the policy
with the library (`library/`), the door reads it there for its second-device rule, and each
seat's trust copy takes it from there.
`host.restart` on the door restarts the owner's host, then the door itself; both stop and
their units start them over.

## The owner's row

The box owner is a seat like any other. `adopt-owner <user>` adds a ledger row for an ordinary
account that exists (uid 1000 up, outside 60001–65535 and below 2³¹, never a seat's), and only for
the one `door-helper on` recorded in `/var/lib/punktfunk-seats/owner`. Any other account, or any
while nothing is recorded, answers `owner_not_recorded`. Only root writes that file, so the door
can't name another account. The door reads it to switch off, so a supervisor that doesn't answer
never keeps the door on. Deleting the row never removes the account or its home. It takes a slot and
ports from the same pools, and the door places the owner profile and the profiles that share the
owner's desktop on it. Its [environment](/docs/developers/multi-seat-contract/supervisor#the-environment-a-seat-host-is-started-with) leaves out
`PUNKTFUNK_CONFIG_DIR`, `PUNKTFUNK_LIBRARY_DIR`, `PUNKTFUNK_HOST_NAME` and `PUNKTFUNK_GAMESTREAM`:
the owner keeps `~/.config/punktfunk`, the library their plugins write, and their own settings. Its
environment file has an ACL entry that lets the owner read it, so the owner's user `punktfunk-host`
moves to the row's ports at boot, logged in or not. The owner's own console,
`punktfunk-web.service`, doesn't start while that file exists: the door's console has the port. With
the owner's GameStream setting on, the owner's row is the box's Moonlight host, on the owner's own
pairings: the door can't redirect Moonlight, and a contract seat never serves it.

## Browsers

A browser that sets `PROFILES` is redirected to its seat's browser plane: the `Redirect` names
that port and carries the plane's certificate hash (field 8), which the browser pins and binds
its credential to. A running seat host follows the box's Seat defaults within seconds: its plane
comes up on its port, under the box's origins, and closes when the box stops streaming to
browsers. A seat host without Seat defaults follows its `PUNKTFUNK_WEBTRANSPORT*` variables.

## One owner host

The row's unit runs the owner's host in a `background` session, on a
private D-Bus, beside whatever the owner has on the monitor. The owner's `punktfunk-host` and
`punktfunk-kde-session` user units stand down while the unit's runtime directory exists.

## The monitor wins

The supervisor polls logind every 2 seconds. When the owner has a session
on a seat that isn't the row's own, it stops the row's unit and starts the owner's user host. The
owner's session is never ended.

## Switching

`PUT /api/v1/profiles/door` with `{"on": true}` or `{"on": false}` answers 202, or
204 when the box is already so. The owner's host starts `punktfunk-door-on@<user>.service`, the
door `punktfunk-door-off@<user>.service`; both run `door-helper`, which execs
`punktfunk-seats door on|off <user>`. It moves the box's files between the owner's
`~/.config/punktfunk` and `/var/lib/punktfunk`, reading and writing each as the owner or as the
door through `runuser`, and turns the units on or off.
The console's `web-session-key` and `web-session-epoch` move with them (on SteamOS, its
`PUNKTFUNK_UI_SECRET` line), so a signed-in console stays signed in. `on` also gives the door the
owner's `host-settings.json`, so the door's own rows start as the owner set them; before `off`,
the door sends the rows it stored to the owner's host, and answers 503 while that host starts. The
user must be in the `punktfunk-update` group, and a member switches the door for their own account
only. An on first sets `/var/lib/punktfunk-seats/door-hold`, which keeps the door's units from
starting, then writes that account to `/var/lib/punktfunk-seats/owner`. It lifts the hold once
the owner's files are in `/var/lib/punktfunk`, so an on that dies halfway leaves the door off
until the next on or off finishes the switch. Who may switch
is decided under the switch's lock, from that file: it refuses to switch on while the file names another account, and to switch off for
any but the one it names, which may have left the group since. A name whose account no longer
exists names no one: a member's switch-on takes the door over, replacing the box's files with
their own where they have the same file, and a switch-off for it leaves the box's files in
`/var/lib/punktfunk`. Only a `getent` miss on a box whose `passwd` sources are all local counts;
any other `getent` status, or a `passwd` source such as `sss` or `ldap`, refuses both. The
owner's host applies the same rules before it starts a switch, so the console names the account
the door is on for instead of asking. A switch it refuses or abandons sets
`door_switch_failed` on the starting host's `GET /host`, and the console stops waiting. A first
switch-on other than a takeover removes each box file the member lacks, avatars included. An
off whose files don't all arrive home still turns the door off and brings the owner's host and
console back, whose `GET /host` reports the failed off. A box file over 8 MiB, or an avatar
listing over 1 MiB, counts as one that didn't arrive. The off keeps the record and the box's files,
and holds the door's units with `/var/lib/punktfunk-seats/door-hold`: the next on keeps those
files, and the off after it hands them back.

The owner's console loads the password file as an environment file, and SteamOS sources it in a
shell. So an off hands back only the door's `PUNKTFUNK_UI_PASSWORD`, `PUNKTFUNK_UI_PASSWORD_HASH`
and `PUNKTFUNK_UI_SECRET` lines, and only with a value of plain characters
(letters, digits, `_.,:=+/@%^-`) or in single quotes with no quote inside, 255 characters at
most. A file with no such line leaves the owner the password file they had.

## The box's ports

The door binds the ports the owner's `host.env` names
(`PUNKTFUNK_MGMT_BIND`, `PUNKTFUNK_NATIVE_PORT`), and its console binds where the owner's
console did (`PUNKTFUNK_UI_BIND`, from `web.env` on SteamOS). Before each door start,
`door-helper ports` (`punktfunk-seats door ports`) reads both files as the owner and writes
those lines to `/var/lib/punktfunk-seats/host.env`, root's, so an update or a later
edit reaches the door: per key the last line, one pair of matching quotes off, and only a value
of address characters (letters, digits, `.:%_-[]`). On NixOS the door also takes the two port keys from
`services.punktfunk.host.settings`, and its console follows `services.punktfunk.web.bind`.
