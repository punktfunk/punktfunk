---
title: A seat on a person's own account
description: A seat that runs as a family member's own login — how the account is adopted, the person's own session and what it reaches, and which devices may open the seat.
---

A seat on the Linux door can run on an account the box already has: a family member's own login.
The console asks with `seat: true`, `seat_account`, a Linux user name, and `seat_password` on
`POST /api/v1/profiles`. It runs as the owner's row does, on a bus of its own, but in a session
of the unit's `user` class, which a shutdown at the machine counts as another user's session,
and it is a contract seat's host, not the owner's.

## Adopting the account

### Command

`adopt_user` carries `seat`, as `create` does, plus the account's `password`, and
answers `created`. The supervisor reads the account's uid itself. The password goes once through
PAM's `login` stack, the one `punktfunk-seat@.service` runs its unit under: `pam_authenticate`,
then `pam_acct_mgmt`, so a locked or expired account is refused. Nothing keeps it, and
`credential` is refused for the row. A refusal's code is `credential_wrong`,
`credential_must_change`, `account_locked`, `account_missing`, `account_not_ordinary`,
`account_is_owner`, `account_taken`, `unlock_locked_out` or `pam_unavailable`.

### The check

PAM runs in a child, `punktfunk-seats pam-check <account>`, with an empty
environment and the password on stdin; libpam never loads into the daemon. A child that hasn't
answered in 30 seconds is killed and the answer is `pam_unavailable`. Other commands don't
wait on it, and the account's uid is read again before the row lands.

### Attempts

After a failed check a device waits 2 s × 2^failures, and 15 minutes after the
fifth. Two failed checks for one account reach PAM in any 15 minutes, from every source together,
so a stack that locks after three, such as `pam_faillock`, is never driven there. That window
doubles with every ten failed checks since the account's last right password, up to 24 hours,
and a day without a failed check forgets them. A refusal
answers `unlock_locked_out` with the wait in its sentence. A check that gave no verdict counts as
failed.

### Which accounts

An ordinary user, as the owner's row takes: uid 1000 up, outside
60001–65535 and below 2³¹, never a seat's, named by the owner's name rule, so `user@domain` of
a directory service passes. Not the box owner, by name or by uid, and not an account another row
holds, by name or by uid.

### The row

It has `adopted_uid`, the uid read at adopt, and no `owner`. A start, the certificate
read and the doctor refuse a row whose name now resolves to another uid. A stop or a delete still
runs and reaches only the row's unit.

### A guest on the account

Nothing on the account changes: no `useradd`, `usermod` or
`userdel`, no group, no mode or owner of the home, and the daemon writes nothing inside it.
The row's files are under `/var/lib/punktfunk-seats` and `/run`: its environment file stays
root's, and the host keeps its identity in `seats/<id>/config`, the person's `0700`, not in
`~/.config/punktfunk`. A stop ends the row's unit and no other process of the user, their
login and SSH included. A delete removes what the row made and gives the account back.

### The monitor wins

While the person has a session on a seat, a start answers
`account_at_desk`, and a running unit ends within 2 seconds of their login. Nothing takes the
row over: their own `punktfunk-host` units are not started or stopped.

### What it does not take

The owner's `host.env` and settings, the box's library as its own
(it plays the box's, from its trust copy), the door, and the owner's user host. The trust copy
is readable by the person alone.

### Their runner

The seat's plugin runner is `punktfunk-scripting-seat@<seat id>.service`, a
user unit the scripting package ships, in the person's own user manager, which the seat's
login session starts. The seat's host starts it while `plugin-runner-on` is in its config
directory, at each of its own starts and when the person switches it on, and stops it when
they switch it off; `seat-session` stops it with the seat. Nothing enables it, and its roots
drop-in is in `$XDG_RUNTIME_DIR/systemd/user`, so nothing of it lands in their home. It reads
the seat's config directory, sees none of `/var/lib/punktfunk-seats` but what it binds and
none of `~/.config/punktfunk`, runs no loose script, and starts only the packages their
session installed (`PUNKTFUNK_PERSON_PLUGINS=1`). Where AppArmor restricts user namespaces it
runs under the profile `punktfunk-scripting-seat`. The seat's host never names their own
`punktfunk-scripting`.

## The person's own session

A Linux seat's own host, made or on a person's own account, runs as the seat's account, so what
it writes into its config folder only that account and root read. The console password never
reaches this lane.

### The token

At start the seat host writes 32 random bytes as hex to `person-token` in
`PUNKTFUNK_CONFIG_DIR`, mode `0600`, owned by its uid, in a folder it owns; it refuses a folder
that is a link, another uid's, or open to group or others, and the lane stays closed. No copy
list of the supervisor names the file. The owner's row, the door and a plain host write none
and take no ticket. A Windows seat keeps its token in memory ([below](#a-windows-seats-person)).

### The ticket

`punktfunk-host ctl console-url`, run as the person in the seat, signs
`<unix s>.<32 hex nonce>.<profile id>.<HMAC-SHA256 hex>` over
`pf-person-session:v1:<unix s>:<nonce>:<profile id>` with the token, and writes a page opening
`https://127.0.0.1:47992/_auth/person?ticket=<ticket>`.

### The session

`POST /api/v1/auth/person/session` with `{"ticket"}`, from loopback, answers
`{token, expires_at, profile}`. The host takes a ticket at most 60 s from its stamp, once, for
its own profile; otherwise 401 `person_ticket_refused`, and 429 `person_ticket_wait` for a
refused one after ten refusals in a minute. A good ticket opens whatever was refused before
it. A session lasts 12 hours, 16 at most, and ends with the host or on
`DELETE` with its token as the bearer. A call with an expired one answers 401
`person_session_ended`.

### On the seat host

`Authorization: Bearer <session>`, from loopback, reaches every route
scoped `profile-data` or `profile-exec` and `GET host` and `GET status`; anything else is 403.
An `as` naming another profile is 403. `GET auth/person/profile` answers the session's own
profile from the seat's trust copy, `{id, display_name, accent?, has_avatar}`, and
`GET auth/person/profile/avatar` its picture; any other lane gets 404.

### On the box

The console sends `X-Punktfunk-Person-Session` and `X-Punktfunk-Person-Profile`
with no bearer, and `?as=` naming that profile, to the door (on Windows the box host) from
loopback. The door asks the
profile's seat host `POST /api/v1/auth/person/verify` `{"token"}` with its admin token, and
takes the call only when the answer names that profile; any other call answers 401
`person_session_ended`. Refusals are not counted, so no other local caller can close a
profile to its own person. It reaches
`mgmt_routes::PERSON` and the reads `GET library`, `library/page`, `library/art/{id}/{kind}`,
`library/scanners`, `library/overrides` and `session/settings`, never for the owner.
`library/overrides` carries the box's shared sets as `box_sets`, which the person reads and
points at but never changes, and the box's entries as `box_games` and `box_sources`, only for
the titles and sources the profile's library lists. The owner's host takes the door's word
with `X-Punktfunk-Person` and its admin token, as for a device's person.

### What only it writes, on a person's own account

`PUT hooks` and the custom-entry writes.
The host marks each with `person: true`, and runs only hooks and lists only own titles so
marked: a file planted without the mark runs nothing. A `prep` step runs only when an own
title the person wrote names it. Any other lane is refused, with `hooks_own_account_person` or
`library_own_account_person`, reads no hooks there, and reads a marked title as 404.

### Their emulators

The person adopts a copy of their own (`POST emulators/{id}/adopt`) and
prepares their emulators (`POST emulators/{id}/prepare`, without a `firmware_dir`); every
other lane gets `emulators_own_account`. An install, a removal, content and the save exchange
stay refused for the person too, with `emulators_own_account_yet`. Hermir's adoption list is
in the account's own data folder, where no other lane writes.

### Their plugins

The person installs and removes plugins (`store/install` from the built-in
source alone, `store/uninstall`), switches [their runner](#their-runner)
(`store/runtime`), reads a plugin page's `ui-credential` and decides folder grants. Their
plugins, on their own tokens, register, publish their titles (`library/provider/*`), ship
logs and ask for folders. A store install puts the package on `plugin-run/person-plugins.json`,
which the host writes and the runner reads; a removal takes it off. The runner starts only
those, and the host reads manifests and mints credentials for those alone: a package dropped
into the seat's `plugins/` any other way runs nothing. A provider's rows on the seat's shelf
are marked `person: true`. Any other lane is refused with `plugins_own_account_person`
(`plugins_own_account` while the person lane is closed) and reads `store/runtime` as not
installed; a catalog source is refused to the person too, with
`plugin_own_account_unreviewed`. The `plugins` CLI there only reads.

## A Windows seat's person

A Windows seat host is SYSTEM, so no file is the seat account's alone. It keeps its person token
in memory and hands out tickets in the same form through a pipe. The session, the box's half and
the marks above are the same; on Windows the box host checks with the seat host itself.

### The pipe

`\\.\pipe\punktfunk-person-<WTS session id>`, owned by SYSTEM, open to SYSTEM
and to the account signed in to the seat's session, labelled medium integrity with no read or
write up, refusing remote clients. A caller writes
`ticket\n`. The host reads the caller's token and answers one ticket line only when its user is
that account, its session the seat's, its integrity medium or higher and it runs in no
AppContainer; anyone else is hung up on.

### The way in

`punktfunk-host ctl console-url` in the seat asks there, refuses a pipe SYSTEM
doesn't own, and prints `https://127.0.0.1:47992/_auth/person?ticket=<ticket>`; `--open` opens
it, or `/closed?why=refused` when no ticket came. The status tray's Open console runs that in
a session whose person pipe exists.

### What it reaches there

`GET host`, `GET status`, their profile and picture, their hooks,
their own titles (`library/custom` and its art), the store but its sources, `store/runtime`,
`GET plugins`, a plugin page's `ui-credential` and folder decisions (`plugin-access`). Every
other profile route is 403: that host would act as SYSTEM on what the call names. A hook the
person saves names no `hmac_secret_file` (`hooks_secret_file_person`). An install from outside
the built-in source is refused (`plugin_seat_unreviewed`): that store runs as SYSTEM, and it
writes into the seat's own `plugins/` alone. Emulators stay the box's.

## A Windows seat's runner

### Who runs it

The seat's host starts `scripting-run.cmd` as the account signed in to the
seat's session, with no window, in a job only the host holds. It does so at each of its own
starts and when the runner is switched on, while `plugin-runner-on` is in its config directory.
It ends when it is switched off, when the host ends, and at the seat's sign-out. No scheduled
task and no stored password. A run that ends with an error starts again within a minute.

### What that account reads

Each start gives it read on `plugin-token`,
`plugin-run/plugin-tokens.json`, the TLS pin, `mgmt-endpoint` and `plugin-run`, read and
`FILE_WRITE_ATTRIBUTES` on `plugins` and the runner bundle, and Modify on `plugin-state`, where
`runner.log` lands. Switching off takes them back. It gets no ACE on any other folder.

### Its sandbox

Each plugin runs in AppContainer `punktfunk.plugin.<id>.<first 8 of the seat
id>` and reaches the host on `\\.\pipe\punktfunk-seat-<seat id>-plugin-<id>`, open to the
seat's account and that container, so nothing of it is the box's plugin's of the same id. `~`
in a manifest is that account's profile. On a person's own account it runs the packages their
session installed alone (`PUNKTFUNK_PERSON_PLUGINS=1`).

### Who installs

On a made seat the operator and the person install, switch and decide
folders. On a person's own account the person alone does; any other lane gets
`plugins_own_account_person` and reads `store/runtime` as not installed.

## Who opens a person's seat

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

### The device list

`seats/<id>/devices.json`, root `0600`. A password or key unlock makes the
device present: no end while connected, 600 seconds after `left`, never more than 24 hours from
the unlock, and none once the session stops, from a stop, the person's login at the machine or a
unit found failed or down. An entry with neither `stay` nor a key goes with its presence. A file
the supervisor can't read, or one that holds anything it wouldn't write, is no device, and
`doctor` shows `device_list`.

### Who is admitted

A device that is present, or that stays signed in and has no key: a key
never lets a device on without a fresh proof, so a copied pairing key alone can't reach the seat.
The seat's `trust/<id>/punktfunk1-paired.json` holds only the box's records of admitted devices,
each as the box wrote it; the other trust files copy as on any seat. The box's file is the door's:
one that is missing, unreadable, over 8 MiB or not `{"clients": [...]}` gives an empty list. The
list is rewritten on each change of who is admitted and checked every 2 seconds while the seat
runs.

### The box pin

The first challenge or key proof pins the SHA-256 of the door's
`/var/lib/punktfunk/native-cert.pem` in `box-pin`. While the certificate differs from it, every
challenge and key proof is refused and `doctor` shows `box_pin` as an error; removing the file
takes a new pin.

### The audit line

Every `unlock` writes one journal line under the target `punktfunk::audit`:
`seat`, `account`, `uid`, `device`, `proof`, `result`, `device_failures` and
`account_failures`. Never the password.

The row's [environment](/docs/developers/multi-seat-contract/supervisor#the-environment-a-seat-host-is-started-with) is a contract seat's, with
`PUNKTFUNK_SEAT_OWN_ACCOUNT=1` and `PUNKTFUNK_CONFIG_DIR=seats/<id>/config`. From it the runner
keeps the session on a private bus, leaves the user's other desktop processes alone and writes no
seat-only setup into their home, as `PUNKTFUNK_SEAT_OWNER` does on the owner's row.
