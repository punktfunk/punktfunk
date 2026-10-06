//! Seat users: one system-range Linux account per seat, owned by an exact GECOS marker.
//!
//! The account is created with `useradd -r` so a display manager's greeter never lists it, with
//! its home under the box directory. The GECOS field carries `punktfunk-seat=<id>`; a user is
//! touched or deleted only when that field matches exactly, so a name that collides with a
//! person's account is refused instead of adopted.

use super::{err, run};
use crate::backend::BackendError;
use crate::model::{Seat, SeatId};
use std::path::{Path, PathBuf};

/// `=`, not `:`: the colon separates passwd fields and `useradd` refuses it in a comment.
const MARKER_PREFIX: &str = "punktfunk-seat=";

/// Groups a seat user joins when they exist: GPU and input device access, then the shared games
/// folder. `punktfunk` is required; the other two are reported by doctor when missing.
const GROUPS: [&str; 3] = ["render", "input", "punktfunk"];

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

/// A name `useradd` accepts everywhere: lowercase, starting with a letter or `_`. The ledger's own
/// rule is wider (uppercase, dots), which Debian's `useradd` refuses.
pub(super) fn valid_unix_name(account: &str) -> bool {
    let mut bytes = account.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
}

/// The passwd row for `name`, `None` when no such user exists. `getent` answers through NSS, so a
/// directory-backed user is found too.
pub(super) fn lookup(name: &str) -> Result<Option<Passwd>, BackendError> {
    Ok(getent("passwd", name)?.and_then(|line| parse_passwd(&line)))
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

fn getent(database: &str, key: &str) -> Result<Option<String>, BackendError> {
    let output = std::process::Command::new("getent")
        .args([database, key])
        .stdin(std::process::Stdio::null())
        .output()
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

pub(super) fn check_name(account: &str) -> Result<(), BackendError> {
    if valid_unix_name(account) {
        return Ok(());
    }
    Err(err(
        "account_invalid",
        format!("account '{account}' isn't a lowercase Linux user name"),
    ))
}

/// The seat's user, created on first use. An existing user is accepted only with the seat's
/// marker. The second value is whether this call created it.
pub(super) fn ensure(seat: &Seat, box_dir: &Path) -> Result<(Passwd, bool), BackendError> {
    if let Some(existing) = lookup(&seat.account)? {
        return if owned_by(&existing, &seat.id) {
            Ok((existing, false))
        } else {
            Err(not_owned(seat))
        };
    }
    let mut groups = Vec::new();
    for group in GROUPS {
        if group_gid(group)?.is_some() {
            groups.push(group);
        }
    }
    let seat_dir = super::shared::seat_dir(box_dir, &seat.id);
    super::shared::make_dir(&seat_dir, 0o711)
        .map_err(|e| err("seat_dirs", format!("create {}: {e}", seat_dir.display())))?;
    let home = seat_dir.join("home");
    let comment = marker(&seat.id);
    let groups = groups.join(",");
    let home_arg = home.to_string_lossy();
    run(
        "useradd_failed",
        "useradd",
        &[
            "-r",
            "-m",
            "-d",
            &home_arg,
            "-c",
            &comment,
            "-G",
            &groups,
            &seat.account,
        ],
    )?;
    let created = lookup(&seat.account)?
        .ok_or_else(|| err("useradd_failed", "useradd left no passwd entry"))?;
    Ok((created, true))
}

/// The seat's user, which must exist and carry the seat's marker.
pub(super) fn require(seat: &Seat) -> Result<Passwd, BackendError> {
    match lookup(&seat.account)? {
        Some(found) if owned_by(&found, &seat.id) => Ok(found),
        Some(_) => Err(not_owned(seat)),
        None => Err(err(
            "account_missing",
            format!("account '{}' doesn't exist", seat.account),
        )),
    }
}

/// Deletes the seat's user and home. A user without the marker is refused; one that is already
/// gone is not an error.
pub(super) fn delete(seat: &Seat) -> Result<(), BackendError> {
    let Some(found) = lookup(&seat.account)? else {
        return Ok(());
    };
    if !owned_by(&found, &seat.id) {
        return Err(not_owned(seat));
    }
    quiesce(&seat.account)?;
    let removed = run("userdel_failed", "userdel", &["-r", &seat.account]);
    // userdel exits 12 when it can't remove the home. The user is gone and the seat directory
    // is removed by path afterwards, so only a user that is still there is a failure.
    match (removed, lookup(&seat.account)?) {
        (Err(error), Some(_)) => Err(error),
        _ => Ok(()),
    }
}

/// Ends everything the user still runs. Its user manager outlives the seat's unit by seconds,
/// and a user deleted under it leaves live processes on a uid the next seat is handed.
fn quiesce(account: &str) -> Result<(), BackendError> {
    let present = || run("loginctl_failed", "loginctl", &["show-user", account]).is_ok();
    if present() {
        let _ = run("loginctl_failed", "loginctl", &["terminate-user", account]);
    }
    for attempt in 0..40 {
        if !present() {
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

fn not_owned(seat: &Seat) -> BackendError {
    err(
        "account_not_owned",
        format!(
            "account '{}' exists without the marker for seat {}",
            seat.account, seat.id
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
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

    #[test]
    fn the_account_name_must_be_one_every_useradd_takes() {
        for good in ["pf-seat-1", "_svc", "a", "pf_seat1"] {
            assert!(valid_unix_name(good), "{good}");
        }
        for bad in [
            "", "Pf-seat", "1seat", "-seat", "pf.seat", "pf seat", "pf-séat",
        ] {
            assert!(!valid_unix_name(bad), "{bad:?}");
        }
    }
}
