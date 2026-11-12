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

**Person**: the human a profile stands for. The console password never runs code as them.

**Device**: a paired client, proven by its certificate or a device token. It reaches the short
list of routes in `punktfunk_core::mgmt_routes` and nothing else.
