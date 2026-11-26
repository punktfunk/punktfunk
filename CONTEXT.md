# Punktfunk

The words the code, the docs and the console use for who owns what.

## Scopes

**Box scope**: what belongs to the machine and exists once: identity, pairing and device grants,
GameStream, the browser plane, power, updates, seats on or off, the physical desk's displays. The
operator alone changes it.

**Host scope**: one host process's own report or switch: its status, logs, diagnostics, encoder
facts and live sessions. Every host has its own; the operator reaches a seat's through the box.

**Profile scope**: what a person would call theirs, on the host their profile plays on. Its
**data half** (library view, preferences, recents, saves) runs nothing. Its **execution half**
(hooks, plugins, emulator management, launch holds) runs code as the profile's own account.

## Who acts

**Operator**: whoever holds the console password, and through it the box's admin key. Changes
box scope and manages every profile.

**Person**: the human a profile stands for. The console password never runs code as them. On a
device, a locked profile's person is a device its list lets in now: it changes that profile's own
library view and game-exit choice, and nothing else. An open profile has no person. On a seat
on a person's own Linux account, the person is also whoever runs as that account in the seat's
desktop: its seat host trusts what its own token signed there, and only that writes the seat's
hooks and own titles.

**Device**: a paired client, proven by its certificate or a device token. It reaches the short
list of routes in `punktfunk_core::mgmt_routes` and nothing else.

## Seats

One box, several players at once, each on a desktop of their own. The map of who runs as whom
is `crates/host/pf-seats/README.md`.

**Box**: the machine.

**Seat**: one player's own desktop on the box: an OS account, a session and a host process of
its own. Not logind's seat; where the code means logind's, it says "logind seat".

**Seat host**: the `punktfunk-host serve` that runs for one seat.

**Supervisor**: the privileged process that creates, starts and stops seats. On Linux the root
daemon `punktfunk-seats`; on Windows a part of the `PunktfunkHost` service.

**Row**, **ledger**: the supervisor's saved record of one seat, and its file of all of them.
Code says "row" for the record and "seat" for the running thing.

**Made seat**: a seat on an account the supervisor created. The supervisor may delete that
account.

**Owner**, **the owner's row**: the account the box's install belongs to, and (Linux) the seat
on that account. The supervisor never changes the account itself.

**A person's row**, **own account**: a seat on someone else's existing login. It opens only
with that person's password or a key their device enrolled.

**The door** (Linux): the box's own host as a system service under the `punktfunk` user. It
faces the network, pairs devices and sends each player to their seat. It never streams.

**The desk**: the physical machine's own screen, keyboard and controllers. "At the desk" means
logged in there.

**Contract**: the environment a seat host is started with, which makes an ordinary host a seat
host (`pf_paths::seat`).

**Trust copy**: a seat's read-only copy of the box's pairing store and profiles.

**Box library**: root's copy of the owner's game library, for the door and for each seat.

**Pad**: a game controller. A **virtual pad** is one the supervisor makes for a seat's client.
A **lent pad** is a real controller plugged into the box and given to one seat; it is
**parked**, open to nobody, while that seat is down.

**Pad broker**: the supervisor's socket where a seat host asks for a virtual pad.

**Unlock**, **presence**: opening a person's seat with their password or device key. A device
that did so is present until it leaves or a time limit passes.

**Keeper** (Windows): the RDP client process that signs a seat's account in and keeps its
session up. A **cycle** is one keeper and its session.

**Display gate** (Windows): the check, once per seat start, that the seat's session can stream.
