---
title: A profile's lock
description: Where a profile's lock, PIN and device keys are kept, which host checks them, and what a device that passes the lock may change.
---

`profile-locks.json` sits beside the box's pairing store. The box's own host, the plain host or
the door, is its one writer, `0600` on Linux and SYSTEM and Administrators on Windows; it is never
in `profiles.json`. Its `profiles` map each profile id to `locked`, an optional `pin`
(`iterations`, `salt`, `hash`: PBKDF2-HMAC-SHA256, 600,000 iterations, a 16-byte salt) and the
`devices` list of a person's row's `devices.json`, where a device the operator added is `stay` and
`added`. Removing a profile removes its entry; unlocking keeps the list and the PIN, and locking
replaces the list with the devices the lock names. One desktop has one lock: the owner is the
first profile whose `os_account` is `operator`, and any later one shares its desktop, has no
entry and opens with the owner's lock, list and PIN. A take-turns profile keeps its own.

## The check

A device opens a profile when the profile is unlocked, or its list admits the
device: `stay`, or present. A device the list holds a key for is asked for the key's proof or
the PIN. Any other device is refused, or asked for the PIN when the profile has one. Every host asks it after a connect resolves its profile and before the profile is
placed or played, the door and a seat host alike. A wake, the picker (`enumerate`), a
device's library read as a profile (`as=`, answered as an unknown id), the door's forwarding
of a device's own play, and a GameStream launch or resume (as the owner) ask it too.
A session, Punktfunk or GameStream, whose device stops being admitted ends within a second.
On a door a Moonlight device is paired with the owner's host: adding one to a list asks that
host's `GET /clients`.

## A file that doesn't read

Every profile is refused to every device, every edit is refused,
and the host logs it. A missing file locks nothing.

## The PIN

Only the box's own host checks it, sent as `ProfileUnlock` kind 1. A device waits
after wrong ones as after a wrong account password; 10 wrong for one profile within 15 minutes,
over all devices, close PIN entry for it until the oldest is 15 minutes old. Every further
ten wrong PINs double that window, up to a day (`pin_misses` in the file counts them); a right
PIN or a new PIN puts it back. A device's waits live in memory; a profile's count is its
`wrong_pins`, unix seconds, in the file, so a restart clears none. A try counts there before its hash runs and is taken back when right. A right PIN
makes the device present, or `stay` when the message asks. Each try writes one line under
`punktfunk::audit`, never the PIN.

## The key

A right PIN that brings a key to enrol, as a password does, stores it in the
device's entry's `keys`, under the profile id it was typed for; one the host can't verify with
is left out. Profiles that share a lock keep a key each, at most 8 an entry, and a profile's
removal drops its keys. An entry's `key` from before `keys` opens any profile of its lock until
a proof makes it that profile's. A device with a key is admitted only while present. Once it
left, `enumerate` says `pin_key` on a profile it holds a key for, and
`POST /profiles/{id}/unlock-challenge` answers `{challenge, profile}`: 32 bytes as 64 hex,
valid 120 seconds and once, the device's only live one for that id. The key signs
`"punktfunk-profile-unlock-v1"` ‖ challenge ‖ the profile id, zero bytes after it to 64 ‖ the
device fingerprint ‖ the fingerprint of the host the device dialled, 187 bytes, signed as the
seat form is, and verified with that profile's key alone;
`clients/shared/profile-unlock-vectors.json` holds both. A right proof makes the device
present. A failed one costs the device's limit alone and answers `close_lock_key_failed`, or
`close_lock_not_allowed` when the profile has no PIN. A device the console adds drops its keys
and stays until signed out; signing it out drops them too.

## A seat host

A seat host reads the box's file, read only, and never takes a PIN or a key proof. A Windows seat
reads it from `PUNKTFUNK_TRUST_DIR`. A Linux seat reads the
[trust copy](/docs/developers/multi-seat-contract/linux#the-trust-copy), which holds its
profile's lock with no PIN; a locked profile's seat gets only the pairing records its lock
admits, as a person's row does, and the owner's row follows the owner's lock. A lock file
there that doesn't read gives an empty pairing list.

## The person

A device a locked profile's list admits now, with nothing asked, is that
profile's person, unless the profile is the owner or on a person's own account. With
`?as=<id>` it may call `mgmt_routes::PERSON`: `PUT library/hidden/{id}`,
`PUT library/overrides/games/{id}`, `PUT library/overrides/sources/{source}`,
`PUT library/scanners/{id}`, and `PUT` and `DELETE session/settings`, which write that
profile's own layer alone. A hide or a game's entry it adds must name a title the profile's
library lists, and a source's entry a source with such a title; taking one away needs none. With
no `as` a device isn't let in. A device that isn't the person, an open profile, one that needs
its PIN and the owner's id answer 403 `profile_change_not_allowed`; an unknown id 404
`profile_unknown`. Art picks stay off: the host fetches a pick's URL itself. Asked on every
call. The box's own host decides, the door on a door: it passes the call to the owner's host,
which writes the library, carrying `X-Punktfunk-Person` beside `X-Punktfunk-Device`. That host
honours it only with its admin token, on these routes, for the profile `as` names and never
the owner, and needs no pairing record for the device. Any other seat host decides nothing.
