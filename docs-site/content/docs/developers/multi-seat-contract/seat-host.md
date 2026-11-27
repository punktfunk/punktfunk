---
title: What a seat host does differently
description: How a host started as a seat departs from a plain host — its display, the Seat defaults, the box's trust and library, its own titles and each profile's layer.
---

Read what changes when `punktfunk-host serve` runs as a seat. It is the same binary as a plain
host; these rules are what its [environment](/docs/developers/multi-seat-contract/supervisor#the-environment-a-seat-host-is-started-with)
turns on. What a Windows seat host adds is on [Windows seats](/docs/developers/multi-seat-contract/windows#a-windows-seat-host).

## The seat contract, not a display policy

A seat that is not the owner's own reads no
`display-settings.json` of its own. Its desktop is one virtual screen at the occupant's mode: it
never mirrors, keeps lit, powers off or PnP-disables a real monitor, and never routes a game to
a session of its own. A second device shares the occupant's screen when the box would join it,
by its own `mode_conflict` or the box's, and is refused as busy otherwise, as the door refuses
it before a redirect. Of a device's display settings in the box console, the largest mode, the
scale and that rule follow it onto a seat. Linger, topology and identity are the unconfigured
host's. `GET /display/settings` lists nothing in `enforced` and only `max_mode`, `scale` and
`mode_conflict` per device; every display or session settings write answers 409.

## The Seat defaults

The box writes `seat-defaults.json` into its config directory at start and
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

## The box's trust, its own key

The seat never writes the pairing store or profiles. A grant
changed in the box console applies to the seat's next check, and within about three seconds to
its live sessions: a narrowed grant takes effect, and an unpair, an expiry or a removed profile
ends the session. It mints and keeps its own identity
in `PUNKTFUNK_CONFIG_DIR`, never the box's. The supervisor records its certificate's SHA-256 in
the ledger while it runs. The box's `Redirect` carries that pin (field 7) and `enumerate` lists
it as `seat.pin`, so a client pins the seat through the box it already trusts. A seat whose pin
the box doesn't hold yet is unavailable. Sleep, restart and shut down are refused: the box's own
host is the one that does them, and it ends the sessions on every seat first.

## The box's library, on Windows

A Windows seat runs none of the box's plugins. It lists and
launches the box's titles as its own user, and answers every change to the box's library with
409. Titles from sources
that belong to one account (Playnite, Game Bar, Amazon, itch, Hydra) are left out. It runs the
box's emulators from `%ProgramData%\punktfunk\emulators` with the firmware the box's plugins
staged. The box host tails each running seat's `game.*` into its own bus, with `seat` naming
the profile: its plugins hear them, its hooks don't. A seat's held launch (below) runs the box
host's plugins, which then release it. The box host's save and firmware routes take
`?profile=<id>` for a profile with a seat: they read and write that seat account's copies, in
its profile folder. A seat host answers 409.

## The box's library, on Linux

The box's library is the owner's. A seat other than the
owner's runs none of the box's plugins, and of the box's library lists and launches only what
it can start as its own user:
`steam_appid` with an id that fits 32 bits and `steam_ui`, with its own Steam, and `emulator`,
with a copy it can run and a game file it can read. The seat answers every change to the box's
library with 409. The owner's host installs a Flathub emulator box-wide through `install_flatpak`
and an archive one through `install_emulator`, so every seat runs it with its own config and saves
in its home; which emulators stay the owner's is in [Profiles](/docs/profiles#on-linux). At a launch the seat installs the RetroArch
core the entry names into its own RetroArch and places the firmware from its library copy.

## Its own runner's titles

What a plugin of the seat account's own runner
publishes with `PUT library/provider/{provider}` lands in the seat host's own titles (below),
under `own:` ids. On a made Linux seat that runner is the seat account's `punktfunk-scripting`
user unit, which reads the seat's config directory, where the package enables the unit for
every account. On a person's own Linux account it is
[their seat's own runner](/docs/developers/multi-seat-contract/own-account#their-runner), and on Windows
[the seat's runner](/docs/developers/multi-seat-contract/own-account#a-windows-seats-runner). A
`?store=` claim is dropped there, so no row shadows a title of the box's. The provider's
running and download reports read those rows, and the box learns their names and pictures as
it learns the others'. On a person's own account only their session and their plugins write
them, and a provider row neither wrote lists nothing.

## Its own runner's folders

A plugin installed in the seat's own `plugins/` reads
its manifest and grants from the seat's own config directory, never the box's for a plugin of
the same id; any other plugin reads the box's copy. The operator decides them on a made seat,
the seat's person on both kinds of seat, and a folder the seat's account can't list is refused
(`not_readable`). On a person's own account, and to a Windows seat's person, an emulator or
core install asked through a grant stays refused (`emulators_own_account_yet`). On Windows the
seat's account lists the folder and places its access itself, so a grant opens only a folder
whose permissions that account may change.

## One profile

The seat serves the profile whose seat it is. A connect that names any other
profile, the owner's included, is refused as an unknown profile; one that names none plays as
this seat's profile. The owner's row alone also serves the owner and the profiles that share the
owner's desktop.

## A profile's own layer

Each profile but the owner may keep a layer of its own over the box's
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

## A profile's own titles

A seat host keeps titles of its own, launch and `prep` included, in
`library.json` in its own config directory. The custom-entry routes write there on a seat host,
under `own:<12 hex>` ids the box never mints; a write naming any other id answers 409. On a
person's own account only [their own session](/docs/developers/multi-seat-contract/own-account#the-persons-own-session) writes one; any other
lane answers 403 `library_own_account_person`, or `library_own_account` where that lane is
closed. Its list carries them on top of its profile's view, marked `own`, and they launch, run
`prep` and serve art as the seat's user, past the rules for what a seat lists of the box's.
`GET library/own` answers each one's id, title, platform and `http(s)` art, never how it
starts. The box asks for that when a seat's host comes up and on each `library.changed` the
seat sends, and stores it through `PUT library/own?as=<profile id>` on the host that writes the
library: the owner's host behind a door, the box host on Windows. It keeps it as
`own-titles.json` in the profile's layer, under the profile the seat belongs to, dropping a row
with a bad id or title and a cover or platform it would not store; an answer that isn't a list,
lists more than 500 or repeats an id leaves the stored titles as they were. `?as=` and the door's answer to a device list those rows,
marked `own`, with no launch: the box refuses to start one with the sentence a title gets that
this desktop can't start.

## Not a host of its own to clients

A seat host never advertises over mDNS, whatever
`PUNKTFUNK_MDNS` says, and its `Welcome` carries management port `0`: its API answers on
loopback only, so a client keeps the box's port, where the library comes from.

## No status tray

A seat host doesn't start or supervise one; the supervisor is the control
surface.
