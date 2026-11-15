//! The door switch's decisions, for `door-helper`: who may switch the door now ([`admit`]).
//!
//! `punktfunk-seats check-owner` prints the answer as a one-shot root verb, with no supervisor
//! and no socket, and the helper asks it under its lock. The helper keeps the hands: the lock and
//! every read and write in a home or in the box directory, each through `runuser` as that side.
//! `door_admission_table` pins every answer.

use super::accounts::{Passwd, Switchable};
use super::err;
use crate::backend::BackendError;
use std::fmt;

/// Whom a switch is for, as `check-owner` prints it.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Admit {
    /// On for this account: the first time, or again.
    On(Passwd),
    /// On over `gone`, a recorded owner whose account no longer exists.
    Takeover { gone: String, to: Passwd },
    /// Off for the recorded owner.
    Off(Passwd),
    /// Off for the recorded owner, whose account no longer exists.
    Gone,
}

impl fmt::Display for Admit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Admit::On(p) => write!(f, "on {} {}", p.uid, p.home.display()),
            Admit::Takeover { gone, to } => {
                write!(f, "takeover {gone} {} {}", to.uid, to.home.display())
            }
            Admit::Off(p) => write!(f, "off {} {}", p.uid, p.home.display()),
            Admit::Gone => f.write_str("gone"),
        }
    }
}

/// Whether `user` may switch the door `on` or off while root's record names `recorded`. `look`
/// is [`super::accounts::switchable`]: `user` passes it first, then the record decides. On is
/// refused while the record names an account that exists or may exist; off runs only for the
/// account it names.
pub(super) fn admit(
    on: bool,
    user: &str,
    recorded: Option<&str>,
    look: impl Fn(&str, bool) -> Result<Switchable, BackendError>,
) -> Result<Admit, BackendError> {
    let found = look(user, on)?;
    match (on, found, recorded) {
        (true, Switchable::Account(to), Some(owner)) if owner != user => match look(owner, false) {
            Ok(Switchable::Gone) => Ok(Admit::Takeover {
                gone: owner.to_owned(),
                to,
            }),
            Ok(Switchable::Account(_)) => Err(err(
                "door_on_for_other",
                format!("the door is on for {owner}: turn it off from that account first"),
            )),
            Err(doubt) => Err(err(
                "door_on_for_other",
                format!("the door is on for {owner}: {}", doubt.message),
            )),
        },
        (true, Switchable::Account(to), _) => Ok(Admit::On(to)),
        (false, found, Some(owner)) if owner == user => Ok(match found {
            Switchable::Account(p) => Admit::Off(p),
            Switchable::Gone => Admit::Gone,
        }),
        (false, _, _) => Err(err(
            "door_not_on_for",
            format!("the door is not on for {user}"),
        )),
        // `switchable` answers gone only to an off.
        (true, Switchable::Gone, _) => Err(err(
            "account_missing",
            format!("account '{user}' doesn't exist"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn passwd(name: &str, uid: u32) -> Passwd {
        Passwd {
            name: name.into(),
            uid,
            gid: uid,
            gecos: String::new(),
            home: PathBuf::from(format!("/home/{name}")),
        }
    }

    /// `switchable` over a fixed box: alice and bob in `punktfunk-update`, carol who left it,
    /// dave gone from a box whose accounts are all local, erin behind a directory service.
    fn look(name: &str, on: bool) -> Result<Switchable, BackendError> {
        super::super::accounts::check_name(name)?;
        let member = |uid| Ok(Switchable::Account(passwd(name, uid)));
        match (name, on) {
            ("alice", _) => member(1000),
            ("bob", _) => member(1001),
            ("carol", false) => member(1002),
            ("carol", true) => Err(err("not_in_update_group", "not in the group")),
            ("dave", false) => Ok(Switchable::Gone),
            ("erin", false) => Err(err("account_unknown", "can't tell if 'erin' is gone")),
            _ => Err(err(
                "account_missing",
                format!("account '{name}' doesn't exist"),
            )),
        }
    }

    /// Every answer `check-owner` gives, by the fix that decided it.
    #[test]
    fn door_admission_table() {
        type Row = (
            &'static str,
            bool,
            &'static str,
            Option<&'static str>,
            Result<&'static str, &'static str>,
        );
        let rows: [Row; 13] = [
            (
                "fresh on (1cbdc6d68)",
                true,
                "alice",
                None,
                Ok("on 1000 /home/alice"),
            ),
            (
                "repeat on",
                true,
                "alice",
                Some("alice"),
                Ok("on 1000 /home/alice"),
            ),
            (
                "on while a present owner holds it (1cbdc6d68)",
                true,
                "bob",
                Some("alice"),
                Err("door_on_for_other"),
            ),
            (
                "on over a gone owner (cfe8ee003)",
                true,
                "bob",
                Some("dave"),
                Ok("takeover dave 1001 /home/bob"),
            ),
            (
                "on while the owner lookup is uncertain (1d57cabeb)",
                true,
                "bob",
                Some("erin"),
                Err("door_on_for_other"),
            ),
            (
                "on outside the group, before the record",
                true,
                "carol",
                Some("alice"),
                Err("not_in_update_group"),
            ),
            (
                "on for a name the seats refuse (3e1b3f6c7)",
                true,
                "Bob.S",
                Some("alice"),
                Err("account_invalid"),
            ),
            (
                "off for the recorded owner (eccea23f4)",
                false,
                "alice",
                Some("alice"),
                Ok("off 1000 /home/alice"),
            ),
            (
                "off after leaving the group (1cbdc6d68)",
                false,
                "carol",
                Some("carol"),
                Ok("off 1002 /home/carol"),
            ),
            (
                "off for a gone owner (cfe8ee003)",
                false,
                "dave",
                Some("dave"),
                Ok("gone"),
            ),
            (
                "off while the owner lookup is uncertain (1d57cabeb)",
                false,
                "erin",
                Some("erin"),
                Err("account_unknown"),
            ),
            (
                "off for someone else (1cbdc6d68)",
                false,
                "bob",
                Some("alice"),
                Err("door_not_on_for"),
            ),
            (
                "off with no record",
                false,
                "bob",
                None,
                Err("door_not_on_for"),
            ),
        ];
        for (row, on, user, recorded, want) in rows {
            let got = admit(on, user, recorded, look)
                .map(|a| a.to_string())
                .map_err(|e| e.code);
            assert_eq!(got.as_deref().map_err(String::as_str), want, "{row}");
        }
    }

    /// A doubt about the recorded owner refuses with its cause in the line.
    #[test]
    fn an_uncertain_owner_s_doubt_reaches_the_refusal() {
        let refused = admit(true, "bob", Some("erin"), look).unwrap_err();
        assert_eq!(
            refused.message,
            "the door is on for erin: can't tell if 'erin' is gone"
        );
    }
}
