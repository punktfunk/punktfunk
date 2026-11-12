//! Seat users: one system-range Linux account per seat, owned by an exact GECOS marker.
//!
//! The account is created with `useradd -r` so a display manager's greeter never lists it, with
//! its home under the supervisor's state directory. The GECOS field carries `punktfunk-seat=<id>`; a user is
//! touched or deleted only when that field matches exactly, so a name that collides with a
//! person's account is refused instead of adopted.

use super::{err, run};
use crate::backend::BackendError;
use crate::model::{
    ordinary_uid, valid_owner_account, Ledger, Seat, SeatId, MAX_OWNER_ACCOUNT_LEN, MAX_SEATS,
};
use std::path::{Path, PathBuf};

/// `=`, not `:`: the colon separates passwd fields and `useradd` refuses it in a comment.
const MARKER_PREFIX: &str = "punktfunk-seat=";

/// Groups a seat user joins when they exist: the GPU, then the shared games folder. Never
/// `punktfunk`, which may power the box off and stop its display manager, nor `input`, which
/// reads every input device on the box, the owner's keyboard too.
const GROUPS: [&str; 2] = ["render", GAMES_GROUP];

/// The shared games folder's group: every seat user and the owner.
pub(super) const GAMES_GROUP: &str = "punktfunk-games";

/// The GECOS text that marks `id`'s account as ours.
pub(super) fn marker(id: &SeatId) -> String {
    format!("{MARKER_PREFIX}{id}")
}

/// One `/etc/passwd` row, as `getent passwd` prints it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Passwd {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub home: PathBuf,
}

/// Parses `name:x:uid:gid:gecos:home:shell`. GECOS is the whole fifth field, commas included, so
/// a `chfn` that appended room numbers makes the marker stop matching.
pub(super) fn parse_passwd(line: &str) -> Option<Passwd> {
    let mut fields = line.trim_end_matches('\n').splitn(7, ':');
    let name = fields.next().filter(|n| !n.is_empty())?;
    let _password = fields.next()?;
    let uid = fields.next()?.parse().ok()?;
    let gid = fields.next()?.parse().ok()?;
    let gecos = fields.next()?;
    let home = fields.next()?;
    let _shell = fields.next()?;
    Some(Passwd {
        name: name.to_owned(),
        uid,
        gid,
        gecos: gecos.to_owned(),
        home: PathBuf::from(home),
    })
}

/// Whether `passwd` is the account this ledger created for `id`.
pub(super) fn owned_by(passwd: &Passwd, id: &SeatId) -> bool {
    passwd.gecos == marker(id)
}

/// The passwd row for `name`, `None` when no such user exists. `getent` answers through NSS, so a
/// directory-backed user is found too.
pub(super) fn lookup(name: &str) -> Result<Option<Passwd>, BackendError> {
    Ok(getent("passwd", name)?.and_then(|line| parse_passwd(&line)))
}

/// `name`'s login shell, the passwd row's last field; `None` when no such user exists.
pub(super) fn shell(name: &str) -> Result<Option<String>, BackendError> {
    let row = getent("passwd", name)?;
    Ok(row.and_then(|line| Some(line.trim_end().rsplit_once(':')?.1.to_owned())))
}

/// Every passwd row on the machine.
pub(super) fn all() -> Result<Vec<Passwd>, BackendError> {
    Ok(run("getent", "getent", &["passwd"])?
        .lines()
        .filter_map(parse_passwd)
        .collect())
}

/// The gid of group `name`, `None` when it doesn't exist.
pub(super) fn group_gid(name: &str) -> Result<Option<u32>, BackendError> {
    Ok(getent("group", name)?.and_then(|line| line.split(':').nth(2)?.parse().ok()))
}

/// Fedora Atomic keeps system groups such as `render` in `/usr/lib/group`, which `getent` reads
/// and `useradd -G` doesn't. Such a group's line is copied into `/etc/group`, as Bazzite's own
/// `ujust add-user-to-input-group` does; one appended line, so no rewrite races shadow's.
fn materialize_group(name: &str) -> Result<(), BackendError> {
    use std::io::Write;
    let etc = std::fs::read_to_string("/etc/group")
        .map_err(|e| err("group_file", format!("read /etc/group: {e}")))?;
    let lib = std::fs::read_to_string("/usr/lib/group").unwrap_or_default();
    let Some(line) = altfiles_line(&etc, &lib, name) else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open("/etc/group")
        .map_err(|e| err("group_file", format!("open /etc/group: {e}")))?;
    let text = if etc.is_empty() || etc.ends_with('\n') {
        format!("{line}\n")
    } else {
        format!("\n{line}\n")
    };
    file.write_all(text.as_bytes())
        .map_err(|e| err("group_file", format!("add {name} to /etc/group: {e}")))
}

/// `name`'s `/usr/lib/group` line when `/etc/group` lacks the group.
fn altfiles_line(etc: &str, lib: &str, name: &str) -> Option<String> {
    let named = |line: &&str| line.split(':').next() == Some(name);
    if etc.lines().any(|line| named(&line)) {
        return None;
    }
    lib.lines().find(named).map(str::to_string)
}

/// `name`'s gid, creating it as a system group first when it is missing.
pub(super) fn ensure_group(name: &str) -> Result<u32, BackendError> {
    if let Some(gid) = group_gid(name)? {
        return Ok(gid);
    }
    super::run("groupadd_failed", "groupadd", &["--system", name])?;
    group_gid(name)?
        .ok_or_else(|| super::err("group_missing", format!("group {name} after groupadd")))
}

fn getent(database: &str, key: &str) -> Result<Option<String>, BackendError> {
    let output = super::output("getent", &[database, key])
        .map_err(|error| err("getent", format!("run getent: {error}")))?;
    match output.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&output.stdout).into_owned())),
        // 2: the key is not in the database.
        Some(2) => Ok(None),
        other => Err(err(
            "getent",
            format!("getent {database} {key} exited with {other:?}"),
        )),
    }
}

/// Refuses an owner name outside [`valid_owner_account`] before any lookup runs.
pub(super) fn check_name(account: &str) -> Result<(), BackendError> {
    if valid_owner_account(account) {
        return Ok(());
    }
    Err(err(
        "account_invalid",
        format!("account '{account}' isn't a lowercase Linux user name"),
    ))
}

/// The names the door and the CLI give a seat's user: `pf-seat-1` up to the last seat. Whoever
/// reaches the socket creates a system user, so the name is not theirs to choose.
pub(super) fn check_seat_name(account: &str) -> Result<(), BackendError> {
    if (1..=MAX_SEATS).any(|n| account == format!("pf-seat-{n}")) {
        return Ok(());
    }
    Err(err(
        "account_invalid",
        format!("account '{account}' isn't a seat account, pf-seat-1 to pf-seat-{MAX_SEATS}"),
    ))
}

/// The seat's user, created on first use with a group of its own, never a shared one such as
/// NixOS's `users`. An empty group of its name, which `userdel` may leave, is taken over. An
/// existing user is accepted only with the seat's marker.
pub(super) fn ensure(seat: &Seat, state: &Path) -> Result<Passwd, BackendError> {
    if let Some(existing) = lookup(&seat.account)? {
        return if owned_by(&existing, &seat.id) {
            Ok(existing)
        } else {
            Err(not_owned(&seat.account, &seat.id))
        };
    }
    let groups = present_groups()?;
    let seat_dir = super::shared::seat_dir(state, &seat.id);
    super::shared::make_dir(&seat_dir, 0o711)
        .map_err(|e| err("seat_dirs", format!("create {}: {e}", seat_dir.display())))?;
    let home = seat_home(state, &seat.id);
    let comment = marker(&seat.id);
    let groups = groups.join(",");
    let home_arg = home.to_string_lossy();
    let leftover = getent("group", &seat.account)?.is_some_and(|line| members(&line).is_empty());
    let own: &[&str] = if leftover {
        &["-g", &seat.account]
    } else {
        &["-U"]
    };
    let mut args = vec!["-r", "-m", "-d", &home_arg, "-c", &comment, "-G", &groups];
    args.extend_from_slice(own);
    args.push(&seat.account);
    run("useradd_failed", "useradd", &args)?;
    lookup(&seat.account)?.ok_or_else(|| err("useradd_failed", "useradd left no passwd entry"))
}

/// Moves the seat user `passwd` onto a group of its own, named after it, when its primary group
/// is another. The seat's group-readable files are then its alone. Waits for a start with no
/// process of the user, whose old group a lingering manager would hand on. Refused while the
/// group of that name has members. `true` when the group changed.
pub(super) fn own_group(passwd: &Passwd) -> Result<bool, BackendError> {
    let primary = getent("group", &passwd.gid.to_string())?;
    if primary.is_some_and(|line| line.split(':').next() == Some(passwd.name.as_str()))
        || runs_as(Path::new("/proc"), passwd.uid)
    {
        return Ok(false);
    }
    match getent("group", &passwd.name)? {
        Some(line) if !members(&line).is_empty() => {
            return Err(err(
                "group_taken",
                format!("group {} has members", passwd.name),
            ))
        }
        Some(_) => {}
        None => run("groupadd_failed", "groupadd", &["--system", &passwd.name]).map(drop)?,
    }
    run(
        "usermod_failed",
        "usermod",
        &["-g", &passwd.name, &passwd.name],
    )?;
    Ok(true)
}

/// The [`GROUPS`] this box has, each in `/etc/group`.
fn present_groups() -> Result<Vec<&'static str>, BackendError> {
    let mut groups = Vec::new();
    for group in GROUPS {
        if group_gid(group)?.is_some() {
            materialize_group(group)?;
            groups.push(group);
        }
    }
    Ok(groups)
}

/// Adds the seat user `account` to each of [`GROUPS`] it is missing. Run at each start: a group
/// made after the account, or made again, has no members, and the games folder admits only them.
pub(super) fn join_groups(account: &str) -> Result<(), BackendError> {
    let mut missing = Vec::new();
    for group in present_groups()? {
        let line = getent("group", group)?.unwrap_or_default();
        if !is_member(&line, account) {
            missing.push(group);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let groups = missing.join(",");
    run("usermod_failed", "usermod", &["-a", "-G", &groups, account]).map(drop)
}

/// The members `line`, a group as `getent group` prints it, lists.
fn members(line: &str) -> Vec<&str> {
    let list = line.trim_end().splitn(4, ':').nth(3).unwrap_or("");
    list.split(',')
        .filter(|member| !member.is_empty())
        .collect()
}

fn is_member(line: &str, account: &str) -> bool {
    members(line).contains(&account)
}

/// The home `useradd` is given for `id`'s user.
fn seat_home(state: &Path, id: &SeatId) -> PathBuf {
    super::shared::seat_dir(state, id).join("home")
}

/// The seat id in `passwd`'s marker, whichever seat that is.
fn marked_id(passwd: &Passwd) -> Option<SeatId> {
    SeatId::parse(passwd.gecos.strip_prefix(MARKER_PREFIX)?).ok()
}

/// The seat accounts in `users` that no row of `ledger` names, with their seat ids. A user can
/// write a marker into their own comment, so only an account in the home `useradd` gave that seat
/// counts: a person's account never lives there.
pub(super) fn orphans<'a>(
    users: &'a [Passwd],
    ledger: &Ledger,
    state: &Path,
) -> Vec<(&'a Passwd, SeatId)> {
    users
        .iter()
        .filter_map(|user| Some((user, marked_id(user)?)))
        .filter(|(user, id)| ledger.seat(id).is_none() && user.home == seat_home(state, id))
        .collect()
}

/// Closes a seat's home to every user but its own. `useradd` makes it as open as `login.defs`
/// says, `0755` on Debian, and the seat directory above it is `0711` for every seat user. An
/// absent home is left as it is; a link is refused.
pub(super) fn close_home(home: &Path) -> Result<(), BackendError> {
    let io = |e: std::io::Error| {
        err(
            "seat_dirs",
            format!("close the home {}: {e}", home.display()),
        )
    };
    let (Some(parent), Some(name)) = (home.parent(), home.file_name().and_then(|n| n.to_str()))
    else {
        return Ok(());
    };
    let above = match super::safe_open::dir(parent) {
        Ok(above) => above,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(io(e)),
    };
    let Some(dir) = super::safe_open::dir_at(&above, name).map_err(io)? else {
        return Ok(());
    };
    let closed = rustix::fs::fchmod(&dir, rustix::fs::Mode::from_raw_mode(0o700 as _));
    closed.map_err(|e| io(e.into()))
}

/// Whether `passwd` carries a seat's marker, for any seat.
pub(super) fn is_seat(passwd: &Passwd) -> bool {
    passwd.gecos.starts_with(MARKER_PREFIX)
}

/// Whether `passwd` is a person's own account: an [`ordinary_uid`] and not a seat's.
pub(super) fn is_ordinary(passwd: &Passwd) -> bool {
    ordinary_uid(passwd.uid) && !is_seat(passwd)
}

/// The owner's account for `account`: a Unix name, an existing user and an ordinary one. Never
/// created here, and never deleted.
pub(super) fn require_owner(account: &str) -> Result<Passwd, BackendError> {
    check_name(account)?;
    match lookup(account)? {
        Some(found) if is_ordinary(&found) => Ok(found),
        Some(_) => Err(err(
            "account_not_ordinary",
            format!("account '{account}' is a system or seat account, not a person's"),
        )),
        None => Err(err(
            "account_missing",
            format!("account '{account}' doesn't exist"),
        )),
    }
}

/// The person a new row may adopt as `account`: an existing ordinary user whom no row holds and
/// who is not the box owner, the one `door-helper` recorded in `state` or the one the owner's
/// row names. The uid is read here, from the system, and no caller's.
pub(super) fn adoptable(
    account: &str,
    ledger: &Ledger,
    state: &Path,
) -> Result<Passwd, BackendError> {
    let found = require_ordinary(account)?;
    let mut owners = Vec::new();
    let named = ledger.owner().map(|owner| owner.account.clone());
    for name in named.into_iter().chain(recorded_owner(state)) {
        owners.extend(lookup(&name)?.map(|owner| (name, owner)));
    }
    check_adoptable(&found, ledger, &owners)?;
    Ok(found)
}

/// [`adoptable`] without the lookups: `owners` are the owner's accounts as named, resolved.
fn check_adoptable(
    found: &Passwd,
    ledger: &Ledger,
    owners: &[(String, Passwd)],
) -> Result<(), BackendError> {
    let name = &found.name;
    if owners
        .iter()
        .any(|(named, owner)| named == name || owner.uid == found.uid)
    {
        return Err(err(
            "account_is_owner",
            format!("{name} is this machine's owner, who has a profile of their own."),
        )
        .said("account_is_owner")
        .param("account", name));
    }
    let held = ledger
        .seats
        .iter()
        .any(|seat| seat.adopted_uid == Some(found.uid) || seat.account.eq_ignore_ascii_case(name));
    if held {
        return Err(err(
            "account_taken",
            format!("Another profile already uses {name}."),
        )
        .said("account_taken")
        .param("account", name));
    }
    Ok(())
}

/// The user `account`: it exists and is a person's. A refusal's message is the console's
/// sentence.
fn require_ordinary(account: &str) -> Result<Passwd, BackendError> {
    check_name(account)?;
    match lookup(account)? {
        Some(found) if is_ordinary(&found) => Ok(found),
        Some(_) => Err(err(
            "account_not_ordinary",
            format!("{account} is a system account, not a person's."),
        )
        .said("account_not_ordinary")
        .param("account", account)),
        None => Err(err(
            "account_missing",
            format!("There is no account called {account} on this machine."),
        )
        .said("account_missing")
        .param("account", account)),
    }
}

/// The user a person's row adopted: its name must still resolve to the uid the row recorded. A
/// name given to another user since, or to none, is refused; nothing runs as a stranger.
pub(super) fn require_person(seat: &Seat) -> Result<Passwd, BackendError> {
    let account = &seat.account;
    match (lookup(account)?, seat.adopted_uid) {
        (Some(found), Some(uid)) if found.uid == uid => Ok(found),
        (Some(_), _) => Err(err(
            "account_not_owned",
            format!("account '{account}' is now another user than the one this profile adopted"),
        )),
        (None, _) => Err(err(
            "account_missing",
            format!("account '{account}' doesn't exist"),
        )),
    }
}

/// The group whose members may switch the door for their own account.
const UPDATE_GROUP: &str = "punktfunk-update";

/// An account `door-helper` may switch the door for.
pub(super) enum Switchable {
    Account(Passwd),
    /// Switching off for a recorded owner whose account no longer exists.
    Gone,
}

/// Whether `door-helper` may switch the door for `account`: [`require_owner`]'s person, and a
/// member of [`UPDATE_GROUP`] to switch on. Switching off, a missing account is [`Switchable::Gone`]
/// only where every passwd source sits on the box: a directory service that isn't answering
/// misses too, and nothing is switched off on a guess.
pub(super) fn switchable(account: &str, on: bool) -> Result<Switchable, BackendError> {
    check_name(account)?;
    if !on && lookup(account)?.is_none() {
        let nsswitch = std::fs::read_to_string("/etc/nsswitch.conf").unwrap_or_default();
        if passwd_sources_local(&nsswitch) {
            return Ok(Switchable::Gone);
        }
        return Err(err(
            "account_unknown",
            format!(
                "can't tell if '{account}' is gone: its accounts may come from a directory service"
            ),
        ));
    }
    let passwd = require_owner(account)?;
    if on
        && !run("id_failed", "id", &["-nG", account])?
            .split_whitespace()
            .any(|g| g == UPDATE_GROUP)
    {
        return Err(err(
            "not_in_update_group",
            format!("'{account}' is not in the {UPDATE_GROUP} group"),
        ));
    }
    Ok(Switchable::Account(passwd))
}

/// Whether `nsswitch.conf` reads passwd from the box alone: files and the sources that keep their
/// rows beside them, with any `[STATUS=action]` between.
fn passwd_sources_local(nsswitch: &str) -> bool {
    let mut seen = false;
    for line in nsswitch.lines() {
        let mut words = line.split('#').next().unwrap_or("").split_whitespace();
        if words.next() != Some("passwd:") {
            continue;
        }
        seen = true;
        for word in words {
            let local = matches!(
                word,
                "files" | "compat" | "systemd" | "altfiles" | "extrausers" | "mymachines"
            );
            let action = word
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim_start_matches('!')
                .split_once('=')
                .is_some_and(|(status, act)| {
                    [status, act]
                        .iter()
                        .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_alphabetic()))
                });
            if !local && !action {
                return false;
            }
        }
    }
    seen
}

/// Where `door-helper` writes the box owner's account, under the state directory.
const OWNER_FILE: &str = "owner";

/// The account `door-helper` recorded in `state`, a directory only root writes.
pub(super) fn recorded_owner(state: &Path) -> Option<String> {
    // The name and `door-helper`'s newline.
    let cap = MAX_OWNER_ACCOUNT_LEN as u64 + 1;
    super::safe_open::read(&state.join(OWNER_FILE), cap)
        .ok()
        .flatten()
        .map(|(bytes, _)| String::from_utf8_lossy(&bytes).trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Refuses `account` as the owner's row unless it is the one `door-helper` recorded in `state`.
/// The door asks for the row, so it can't name another account.
pub(super) fn require_recorded(state: &Path, account: &str) -> Result<(), BackendError> {
    if recorded_owner(state).as_deref() == Some(account) {
        return Ok(());
    }
    Err(err(
        "owner_not_recorded",
        format!("account '{account}' isn't the owner door-helper recorded"),
    ))
}

/// The seat's user, which must exist and carry the seat's marker.
pub(super) fn require(seat: &Seat) -> Result<Passwd, BackendError> {
    match lookup(&seat.account)? {
        Some(found) if owned_by(&found, &seat.id) => Ok(found),
        Some(_) => Err(not_owned(&seat.account, &seat.id)),
        None => Err(err(
            "account_missing",
            format!("account '{}' doesn't exist", seat.account),
        )),
    }
}

/// Refuses a user named `account` that isn't `id`'s, before anything is done to it. An absent
/// user is fine.
pub(super) fn refuse_foreign(account: &str, id: &SeatId) -> Result<(), BackendError> {
    match lookup(account)? {
        Some(found) if !owned_by(&found, id) => Err(not_owned(account, id)),
        _ => Ok(()),
    }
}

/// Deletes the user `account` and its home when it carries `id`'s marker. A user without the
/// marker is refused; one that is already gone is not an error.
pub(super) fn delete(account: &str, id: &SeatId) -> Result<(), BackendError> {
    let Some(found) = lookup(account)? else {
        return Ok(());
    };
    if !owned_by(&found, id) {
        return Err(not_owned(account, id));
    }
    quiesce(account)?;
    let removed = run("userdel_failed", "userdel", &["-r", account]);
    // userdel exits 12 when it can't remove the home. The user is gone and the seat directory
    // is removed by path afterwards, so only a user that is still there is a failure.
    match (removed, lookup(account)?) {
        (Err(error), Some(_)) => Err(error),
        _ => Ok(()),
    }
}

/// Ends everything the user still runs: at its seat's stop, and before its account is deleted or
/// rehomed. Its user manager outlives the seat's unit by seconds, and `userdel` or `usermod`
/// refuse a busy user. Done when no process runs as the user: logind keeps an empty user's
/// record for `UserStopDelaySec` (10 s by default), as long as this whole wait. A linger the user
/// set goes first: it would start their manager at boot, before the seat's drop-in exists.
pub(super) fn quiesce(account: &str) -> Result<(), BackendError> {
    let Some(user) = lookup(account)? else {
        return Ok(());
    };
    let _ = run("loginctl_failed", "loginctl", &["disable-linger", account]);
    if run("loginctl_failed", "loginctl", &["show-user", account]).is_ok() {
        let _ = run("loginctl_failed", "loginctl", &["terminate-user", account]);
    }
    for attempt in 0..40 {
        if !runs_as(Path::new("/proc"), user.uid) {
            return Ok(());
        }
        if attempt == 12 {
            let kill = ["kill-user", account, "--signal=SIGKILL"];
            let _ = run("loginctl_failed", "loginctl", &kill);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    Err(err(
        "user_busy",
        format!("processes of '{account}' are still running"),
    ))
}

/// Whether a process in `proc` has `uid` as its real, effective or saved user, the test `userdel`
/// makes. An unreadable `proc` counts as busy.
fn runs_as(proc: &Path, uid: u32) -> bool {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return true;
    };
    entries.flatten().any(|entry| {
        std::fs::read_to_string(entry.path().join("status")).is_ok_and(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .is_some_and(|ids| {
                    ids.split_whitespace()
                        .take(3)
                        .any(|id| id.parse::<u32>().ok() == Some(uid))
                })
        })
    })
}

fn not_owned(account: &str, id: &SeatId) -> BackendError {
    err(
        "account_not_owned",
        format!("account '{account}' exists without the marker for seat {id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_s_members_are_its_fourth_field() {
        let line = "punktfunk-games:x:947:gamestream,pf-seat-1\n";
        assert!(is_member(line, "pf-seat-1"));
        assert!(is_member(line, "gamestream"));
        assert!(!is_member(line, "pf-seat-10"), "a prefix is not a member");
        assert!(
            !is_member("punktfunk-games:x:947:\n", "pf-seat-1"),
            "a remade group is empty"
        );
        assert!(
            !is_member("pf-seat-1:x:946:", "pf-seat-1"),
            "a group's name is not a member"
        );
        assert!(
            members("pf-seat-1:x:946:\n").is_empty(),
            "a leftover group is empty"
        );
    }

    #[test]
    fn a_user_is_busy_while_a_process_has_it_as_real_effective_or_saved_user() {
        let proc = tempfile::tempdir().unwrap();
        let status = |pid: &str, uids: &str| {
            std::fs::create_dir(proc.path().join(pid)).unwrap();
            let text = format!("Name:\tx\nUid:\t{uids}\nGid:\t0\t0\t0\t0\n");
            std::fs::write(proc.path().join(pid).join("status"), text).unwrap();
        };
        status("1", "0\t0\t0\t0");
        status("70", "0\t0\t0\t1001");
        std::fs::create_dir(proc.path().join("sys")).unwrap();
        assert!(
            !runs_as(proc.path(), 1001),
            "the filesystem uid alone is not the user's"
        );
        assert!(!runs_as(proc.path(), 1002));
        status("71", "0\t0\t1001\t1001");
        assert!(runs_as(proc.path(), 1001), "a saved uid is the user's");
        assert!(
            runs_as(&proc.path().join("missing"), 1002),
            "an unreadable proc is busy"
        );
    }

    fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    /// A group only `/usr/lib/group` holds is copied once; a name that is only a prefix isn't it.
    #[test]
    fn a_group_only_altfiles_holds_is_copied_into_etc() {
        let lib = "renderx:x:9:\nrender:x:105:\n";
        assert_eq!(
            altfiles_line("wheel:x:10:\n", lib, "render").as_deref(),
            Some("render:x:105:")
        );
        assert_eq!(altfiles_line("render:x:105:\n", lib, "render"), None);
        assert_eq!(altfiles_line("wheel:x:10:\n", "", "render"), None);
    }

    #[test]
    fn a_passwd_row_parses_and_keeps_the_whole_gecos_field() {
        let row = "pf-seat-1:x:975:972:punktfunk-seat=0123456789abcdef0123456789abcdef:/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/home:/bin/bash\n";
        let parsed = parse_passwd(row).unwrap();
        assert_eq!(
            (parsed.name.as_str(), parsed.uid, parsed.gid),
            ("pf-seat-1", 975, 972)
        );
        assert_eq!(parsed.gecos, marker(&id()));
        assert_eq!(
            parsed.home,
            Path::new("/var/lib/punktfunk/seats/0123456789abcdef0123456789abcdef/home")
        );
        assert!(parse_passwd("short:x:1").is_none());
        assert!(parse_passwd("n:x:notanumber:1:g:/h:/s").is_none());
        assert!(parse_passwd(":x:1:1:g:/h:/s").is_none());
    }

    /// The marker is exact: another seat's id, a `chfn`-style suffix, a prefix and a person's
    /// real name each leave the account unowned.
    #[test]
    fn only_the_exact_marker_owns_an_account() {
        let row = |gecos: &str| format!("u:x:1000:1000:{gecos}:/home/u:/bin/bash");
        let owned = |gecos: &str| owned_by(&parse_passwd(&row(gecos)).unwrap(), &id());
        assert!(owned(&marker(&id())));
        assert!(!owned("punktfunk-seat=ffffffffffffffffffffffffffffffff"));
        assert!(!owned(&format!("{},,,", marker(&id()))));
        assert!(!owned("punktfunk-seat=0123456789abcdef"));
        assert!(!owned("Ada Lovelace"));
        assert!(!owned(""));
    }

    /// A home `useradd` left at the distro's mode closes to its user; a link in its place is
    /// refused, and the directory it points at keeps its mode.
    #[test]
    fn a_seat_home_closes_to_its_user_and_never_through_a_link() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let seat = tempfile::tempdir().unwrap();
        let home = seat.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
        close_home(&home).unwrap();
        assert_eq!(mode(&home), 0o700);

        close_home(&seat.path().join("absent")).unwrap();
        close_home(&seat.path().join("no-seat-dir/home")).unwrap();

        let target = seat.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let linked = seat.path().join("linked");
        std::os::unix::fs::symlink(&target, &linked).unwrap();
        assert!(close_home(&linked).is_err());
        assert_eq!(mode(&target), 0o755);
    }

    /// A seat's marker names its id, whichever seat that is; anything else names none.
    #[test]
    fn a_marker_names_the_seat_whose_account_it_is() {
        let row = |gecos: &str| {
            parse_passwd(&format!("u:x:975:972:{gecos}:/var/lib/h:/bin/bash")).unwrap()
        };
        assert_eq!(marked_id(&row(&marker(&id()))), Some(id()));
        for gecos in [
            "",
            "Ada Lovelace",
            "punktfunk-seat=",
            "punktfunk-seat=0123456789abcdef",
            "punktfunk-seat=0123456789ABCDEF0123456789ABCDEF",
        ] {
            assert_eq!(marked_id(&row(gecos)), None, "{gecos:?}");
        }
    }

    /// An orphan is a seat account in its own seat home that no ledger row names. A marker in a
    /// person's comment, or in an account elsewhere, is not one.
    #[test]
    fn only_a_seat_account_in_its_seat_home_is_an_orphan() {
        use crate::model::CreateSeat;
        let state = Path::new("/var/lib/punktfunk-seats");
        let mut ledger = Ledger::new();
        let kept = ledger
            .allocate(CreateSeat {
                name: "Kept".into(),
                account: "pf-seat-1".into(),
                autostart: false,
            })
            .unwrap();
        let (orphan, forged) = (SeatId::random(), SeatId::random());
        let row = |name: &str, id: &SeatId, home: &Path| {
            let gecos = marker(id);
            parse_passwd(&format!(
                "{name}:x:975:972:{gecos}:{}:/bin/bash",
                home.display()
            ))
            .unwrap()
        };
        let users = [
            row("pf-seat-1", &kept.id, &seat_home(state, &kept.id)),
            row("pf-seat-2", &orphan, &seat_home(state, &orphan)),
            row("alice", &forged, Path::new("/home/alice")),
            row("pf-seat-3", &forged, &seat_home(state, &orphan)),
            parse_passwd("bob:x:1001:1001:Bob:/home/bob:/bin/bash").unwrap(),
        ];
        let found: Vec<_> = orphans(&users, &ledger, state)
            .into_iter()
            .map(|(user, id)| (user.name.as_str(), id))
            .collect();
        assert_eq!(found, [("pf-seat-2", orphan)]);
    }

    /// A user that isn't the seat's is refused before its sessions are touched; an absent one is
    /// not. `getent` answers for the machine's own users, so only Linux runs it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_foreign_user_is_refused_and_an_absent_one_is_not() {
        assert_eq!(
            refuse_foreign("root", &id()).unwrap_err().code,
            "account_not_owned"
        );
        assert!(refuse_foreign("pf-seat-no-such-user", &id()).is_ok());
    }

    /// The owner is a person: a local or a directory service's uid, never a seat's marked
    /// account, and never a system, homed or dynamic user.
    #[test]
    fn only_an_ordinary_account_can_be_the_owner() {
        let row = |uid: u32, gecos: &str| {
            parse_passwd(&format!("u:x:{uid}:{uid}:{gecos}:/home/u:/bin/bash")).unwrap()
        };
        assert!(is_ordinary(&row(1000, "Enrico")));
        assert!(is_ordinary(&row(60000, "")));
        assert!(is_ordinary(&row(65536, "")));
        assert!(is_ordinary(&row(1_950_400_009, "")), "FreeIPA");
        assert!(!is_ordinary(&row(999, "")), "a system user");
        assert!(!is_ordinary(&row(60001, "")), "homed");
        assert!(!is_ordinary(&row(65534, "")), "nobody");
        assert!(!is_ordinary(&row(1 << 31, "")));
        assert!(!is_ordinary(&row(u32::MAX, "")));
        assert!(!is_ordinary(&row(0, "root")));
        assert!(!is_ordinary(&row(1001, &marker(&id()))), "a seat's account");
    }

    /// Only the account `door-helper` recorded can become the owner's row; with no record, none.
    #[test]
    fn only_the_recorded_owner_is_adopted() {
        let state = tempfile::tempdir().unwrap();
        let refused = |account: &str| require_recorded(state.path(), account).is_err();
        assert!(refused("alice"), "nothing recorded");
        std::fs::write(state.path().join(OWNER_FILE), "alice\n").unwrap();
        assert!(!refused("alice"));
        assert!(refused("bob"));
        assert!(refused(""));
        let longest = format!("{}@corp.example.com", "j".repeat(64 - 17));
        assert_eq!(longest.len(), MAX_OWNER_ACCOUNT_LEN);
        std::fs::write(state.path().join(OWNER_FILE), format!("{longest}\n")).unwrap();
        assert!(!refused(&longest), "the longest name the seats accept");
    }

    /// A new row adopts neither the owner, by name or by uid, nor an account another row holds,
    /// whatever name it goes by.
    #[test]
    fn the_owner_and_a_held_account_are_not_adoptable() {
        use crate::model::CreateSeat;
        let user = |name: &str, uid: u32| {
            parse_passwd(&format!("{name}:x:{uid}:{uid}::/home/{name}:/bin/bash")).unwrap()
        };
        let named = |account: &str| CreateSeat {
            name: account.into(),
            account: account.into(),
            autostart: false,
        };
        let mut ledger = Ledger::new();
        ledger.allocate_adopted_user(named("carol"), 1200).unwrap();
        let owners = [("enrico".to_owned(), user("enrico", 1000))];
        let code = |found: &Passwd| check_adoptable(found, &ledger, &owners).unwrap_err().code;

        assert!(check_adoptable(&user("dave", 1300), &ledger, &owners).is_ok());
        assert_eq!(code(&user("enrico", 1000)), "account_is_owner");
        assert_eq!(
            code(&user("enrico@corp", 1000)),
            "account_is_owner",
            "an alias"
        );
        assert_eq!(code(&user("carol", 1200)), "account_taken");
        assert_eq!(code(&user("carol@corp", 1200)), "account_taken", "an alias");
        assert!(check_adoptable(&user("dave", 1300), &Ledger::new(), &[]).is_ok());
    }

    /// A person's row runs only while its name resolves to the uid it recorded.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_person_row_refuses_a_name_that_is_another_user() {
        use crate::model::CreateSeat;
        let seat = |account: &str, uid| {
            let request = CreateSeat {
                name: "Root".into(),
                account: account.into(),
                autostart: false,
            };
            let mut ledger = Ledger::new();
            let mut seat = ledger.allocate_adopted_user(request, 1500).unwrap();
            seat.adopted_uid = Some(uid);
            seat
        };
        assert_eq!(
            require_person(&seat("root", 1500)).unwrap_err().code,
            "account_not_owned"
        );
        assert_eq!(
            require_person(&seat("pf-no-such-user", 1500))
                .unwrap_err()
                .code,
            "account_missing"
        );
        assert_eq!(require_person(&seat("root", 0)).unwrap().uid, 0);
    }

    /// A miss means gone only where no passwd source could have been silent.
    #[test]
    fn only_local_passwd_sources_let_a_miss_mean_gone() {
        for local in [
            "passwd: files systemd\n",
            "passwd:   files altfiles [!UNAVAIL=return] systemd # comment sss\n",
            "group: sss\npasswd: compat\n",
        ] {
            assert!(passwd_sources_local(local), "{local:?}");
        }
        for remote in [
            "",
            "group: files\n",
            "passwd: files sss\n",
            "passwd: files ldap [NOTFOUND=return]\n",
            "passwd: files\npasswd: winbind\n",
            "passwd: files [=]\n",
        ] {
            assert!(!passwd_sources_local(remote), "{remote:?}");
        }
    }

    /// An owner name `getent` could read as an option is refused before anything runs.
    #[test]
    fn an_owner_name_is_checked_before_any_lookup() {
        assert_eq!(require_owner("-s").unwrap_err().code, "account_invalid");
    }

    /// Creating a seat takes the names the door and the CLI send and no other, even a valid one.
    #[test]
    fn a_seat_account_is_named_pf_seat_and_its_number() {
        for good in ["pf-seat-1", "pf-seat-2", "pf-seat-3", "pf-seat-4"] {
            assert!(check_seat_name(good).is_ok(), "{good}");
        }
        for bad in [
            "pf-seat-0",
            "pf-seat-5",
            "pf-seat-01",
            "pf-seat-+1",
            "pf-seat-",
            "pf-seat",
            "pf_seat1",
            "www-data",
            "_svc",
            "xpf-seat-1",
        ] {
            assert_eq!(
                check_seat_name(bad).unwrap_err().code,
                "account_invalid",
                "{bad}"
            );
        }
    }
}
