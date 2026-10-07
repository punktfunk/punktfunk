//! Who is logged in, from `loginctl`: the supervisor's view of whether the box owner sits at the
//! machine. The owner's headless session never has a seat, so a session of theirs with one is a
//! physical login, and that login wins (`mod.rs`).

use super::run;
use crate::backend::BackendError;

/// One logind session, as `loginctl show-session` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LoginSession {
    pub id: String,
    pub user: String,
    /// Empty for a session with no seat: a headless one, or an SSH login.
    pub seat: String,
    pub class: String,
    pub remote: bool,
    pub state: String,
}

/// Parses the blank-line-separated `Key=value` blocks `loginctl show-session` prints for several
/// sessions. A block with no id is dropped.
pub(super) fn parse_sessions(text: &str) -> Vec<LoginSession> {
    text.split("\n\n")
        .filter_map(|block| {
            let mut s = LoginSession::default();
            for line in block.lines() {
                match line.split_once('=') {
                    Some(("Id", v)) => s.id = v.to_owned(),
                    Some(("Name", v)) => s.user = v.to_owned(),
                    Some(("Seat", v)) => s.seat = v.to_owned(),
                    Some(("Class", v)) => s.class = v.to_owned(),
                    Some(("Remote", v)) => s.remote = v == "yes",
                    Some(("State", v)) => s.state = v.to_owned(),
                    _ => {}
                }
            }
            (!s.id.is_empty()).then_some(s)
        })
        .collect()
}

/// Every session on the box.
pub(super) fn sessions() -> Result<Vec<LoginSession>, BackendError> {
    let listed = run(
        "loginctl_failed",
        "loginctl",
        &["list-sessions", "--no-legend"],
    )?;
    let ids: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = vec!["show-session", "-p", "Id", "-p", "Name", "-p", "Seat"];
    args.extend(["-p", "Class", "-p", "Remote", "-p", "State"]);
    args.extend(ids);
    run("loginctl_failed", "loginctl", &args).map(|text| parse_sessions(&text))
}

/// Whether `account` has a session at the machine: a seat, the `user` class, not remote, not on
/// its way out, and not `own`, the id of the session the owner's seat unit holds.
pub(super) fn at_the_desk(sessions: &[LoginSession], account: &str, own: Option<&str>) -> bool {
    sessions.iter().any(|s| {
        s.user == account
            && !s.seat.is_empty()
            && s.class == "user"
            && !s.remote
            && s.state != "closing"
            && Some(s.id.as_str()) != own
    })
}

/// The id the seat's runner recorded for its own session, when it has one.
pub(super) fn own_session(account: &str) -> Option<String> {
    let text =
        std::fs::read_to_string(super::session::runtime_dir(account).join("session")).ok()?;
    let id = text.trim();
    (!id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The owner's headless session (no seat), the same user's physical one, the greeter, an SSH
    /// login and a session that is closing, as `show-session` prints them.
    const LISTING: &str = "\
Id=9
Name=owner
Seat=
Class=user
Remote=no
State=active

Id=16
Name=owner
Seat=seat0
Class=user
Remote=no
State=active

Id=c1
Name=sddm
Seat=seat0
Class=greeter
Remote=no
State=active

Id=21
Name=owner
Seat=
Class=user
Remote=yes
State=active

Id=30
Name=ben
Seat=seat0
Class=user
Remote=no
State=closing
";

    #[test]
    fn show_session_blocks_parse_one_session_each() {
        let all = parse_sessions(LISTING);
        assert_eq!(all.len(), 5);
        assert_eq!(all[1].id, "16");
        assert_eq!(all[1].seat, "seat0");
        assert!(all[3].remote);
        assert!(parse_sessions("").is_empty());
        assert!(parse_sessions("Name=x\nSeat=\n").is_empty());
    }

    /// Only a seat, the `user` class, a local login and a live session count; the row's own
    /// session never does.
    #[test]
    fn the_owner_is_at_the_desk_only_for_a_physical_login() {
        let all = parse_sessions(LISTING);
        assert!(at_the_desk(&all, "owner", Some("9")));
        assert!(at_the_desk(&all, "owner", None));
        let headless_only: Vec<_> = all
            .iter()
            .filter(|s| s.id == "9" || s.id == "21" || s.id == "c1")
            .cloned()
            .collect();
        assert!(!at_the_desk(&headless_only, "owner", Some("9")));
        assert!(!at_the_desk(&all, "ben", None), "a closing session is gone");
        assert!(
            !at_the_desk(&all, "sddm", None),
            "the greeter is not a user class"
        );
        let physical_is_own: Vec<_> = all.iter().filter(|s| s.id == "16").cloned().collect();
        assert!(!at_the_desk(&physical_is_own, "owner", Some("16")));
    }
}
