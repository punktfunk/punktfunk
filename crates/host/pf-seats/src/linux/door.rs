//! The door switch's decisions, for `door-helper`: who may switch the door now ([`admit`]) and
//! which of the owner's port lines the door takes ([`port_lines`]).
//!
//! Two one-shot root verbs of `punktfunk-seats` carry them, with no supervisor and no socket:
//! `check-owner`, which the helper asks under its lock, and `door-ports`, which reads the owner's
//! files on stdin. The helper keeps the hands: the lock and every read and write in a home or in
//! the box directory, each through `runuser` as that side, so root opens neither here.
//! `door_admission_table` and `door_port_lines_table` pin every answer.

use super::accounts::{Passwd, Switchable};
use super::err;
use crate::backend::BackendError;
use std::fmt;
use std::io::Read;
use std::path::Path;

/// The keys the door takes from the owner's files, in the order it writes them.
const PORT_KEYS: [&str; 3] = [
    "PUNKTFUNK_MGMT_BIND",
    "PUNKTFUNK_NATIVE_PORT",
    "PUNKTFUNK_UI_BIND",
];

/// The most `door-ports` reads: the owner's two env files, theirs to make any size.
const MAX_PORT_INPUT: u64 = 1 << 20;

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

/// The C locale's `[[:space:]]`, which counts VT where `u8::is_ascii_whitespace` doesn't.
fn space(b: &u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|b| !space(b)).unwrap_or(s.len());
    let end = s.iter().rposition(|b| !space(b)).map_or(start, |i| i + 1);
    &s[start..end]
}

/// The door's port lines from the owner's `host.env` then `web.env`: per [`PORT_KEYS`] key the
/// last line that sets it, one pair of matching quotes off, kept only when its value is
/// address characters. A line is `KEY=value` with blanks around both; any NUL drops all.
pub(super) fn port_lines(text: &[u8]) -> String {
    if text.contains(&0) {
        return String::new();
    }
    let mut last: [Option<&[u8]>; 3] = [None; 3];
    for line in text.split(|&b| b == b'\n') {
        let Some(eq) = line.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = trim(&line[..eq]);
        let Some(i) = PORT_KEYS.iter().position(|k| k.as_bytes() == key) else {
            continue;
        };
        let mut value = trim(&line[eq + 1..]);
        if let [quote @ (b'"' | b'\''), inner @ .., end] = value
            && end == quote
        {
            value = inner;
        }
        last[i] = Some(value);
    }
    let address = |b: &u8| b.is_ascii_alphanumeric() || b".:%_-[]".contains(b);
    PORT_KEYS
        .iter()
        .zip(last)
        .filter_map(|(key, value)| {
            let value = value.filter(|v| !v.is_empty() && v.iter().all(address))?;
            Some(format!("{key}={}\n", String::from_utf8_lossy(value)))
        })
        .collect()
}

/// `door-ports`: the [`port_lines`] of `input` into `to`, root's and 0644, replaced whole.
/// Input past [`MAX_PORT_INPUT`] is refused and the last copy stays.
pub fn ports(input: impl Read, to: &Path) -> Result<(), BackendError> {
    use std::os::unix::fs::PermissionsExt;
    let mut text = Vec::new();
    input
        .take(MAX_PORT_INPUT + 1)
        .read_to_end(&mut text)
        .map_err(|e| err("door_ports", format!("read the port lines: {e}")))?;
    if text.len() as u64 > MAX_PORT_INPUT {
        return Err(err("door_ports", "the port lines run past 1 MiB"));
    }
    let mut tmp = to.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);
    pf_paths::write_secret_file(tmp, port_lines(&text).as_bytes())
        .and_then(|()| std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o644)))
        .and_then(|()| std::fs::rename(tmp, to))
        .map_err(|e| err("door_ports", format!("write {}: {e}", to.display())))
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

    /// What the helper's `grep | awk` filter passed for each input, run under `LC_ALL=C`. The
    /// input is the owner's `host.env` then `web.env`, each read whole by `cat`.
    #[test]
    fn door_port_lines_table() {
        let rows: [(&str, &[u8], &str); 24] = [
            (
                "the three keys, in the door's order (9c97fefed)",
                b"PUNKTFUNK_UI_BIND=127.0.0.1:47990\nPUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_MGMT_BIND=0.0.0.0:48001\n",
                "PUNKTFUNK_MGMT_BIND=0.0.0.0:48001\nPUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
            ),
            (
                "no other key",
                b"PUNKTFUNK_LOG=debug\nRUST_LOG=trace\nPUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_UI_PASSWORD=x\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "the last line wins",
                b"PUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_NATIVE_PORT=49000\n",
                "PUNKTFUNK_NATIVE_PORT=49000\n",
            ),
            (
                "a dropped last line drops the key",
                b"PUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_NATIVE_PORT=48 000\n",
                "",
            ),
            (
                "an empty last line drops the key",
                b"PUNKTFUNK_NATIVE_PORT=48000\nPUNKTFUNK_NATIVE_PORT=\n",
                "",
            ),
            ("double quotes off", b"PUNKTFUNK_NATIVE_PORT=\"48000\"\n", "PUNKTFUNK_NATIVE_PORT=48000\n"),
            ("single quotes off", b"PUNKTFUNK_MGMT_BIND='[::1]:48001'\n", "PUNKTFUNK_MGMT_BIND=[::1]:48001\n"),
            ("one pair only", b"PUNKTFUNK_NATIVE_PORT=\"\"48000\"\"\n", ""),
            ("quotes that don't match stay", b"PUNKTFUNK_NATIVE_PORT=\"48000'\n", ""),
            ("a lone quote", b"PUNKTFUNK_NATIVE_PORT=\"\n", ""),
            ("empty quotes are an empty value", b"PUNKTFUNK_NATIVE_PORT=\"\"\n", ""),
            (
                "blanks around the key and the value, and a CR",
                b" \tPUNKTFUNK_NATIVE_PORT \t= 48000 \r\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "blanks outside the quotes go, inside they stay",
                b"PUNKTFUNK_NATIVE_PORT= \"48000\" \nPUNKTFUNK_UI_BIND=\" 127.0.0.1:47990\"\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "VT and FF are blanks",
                b"\x0bPUNKTFUNK_NATIVE_PORT\x0c=\x0b48000\x0c\n",
                "PUNKTFUNK_NATIVE_PORT=48000\n",
            ),
            (
                "address characters",
                b"PUNKTFUNK_MGMT_BIND=fe80::1%eth0\nPUNKTFUNK_UI_BIND=box-1.lan_x:47990\n",
                "PUNKTFUNK_MGMT_BIND=fe80::1%eth0\nPUNKTFUNK_UI_BIND=box-1.lan_x:47990\n",
            ),
            (
                "anything else drops the line",
                b"PUNKTFUNK_NATIVE_PORT=$(reboot)\nPUNKTFUNK_MGMT_BIND=0.0.0.0:48001 # lan\nPUNKTFUNK_UI_BIND=a=b\n",
                "",
            ),
            ("a byte past ASCII drops the line", "PUNKTFUNK_UI_BIND=b\u{fc}:1\n".as_bytes(), ""),
            (
                "only a line that is the key",
                b"export PUNKTFUNK_NATIVE_PORT=1\n#PUNKTFUNK_NATIVE_PORT=2\nPUNKTFUNK_NATIVE_PORTS=3\npunktfunk_native_port=4\nPUNKTFUNK NATIVE_PORT=5\n",
                "",
            ),
            (
                "web.env is read after host.env (9c97fefed)",
                b"PUNKTFUNK_UI_BIND=0.0.0.0:47990\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
                "PUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
            ),
            (
                "web.env without host.env",
                b"PUNKTFUNK_UI_PASSWORD_HASH=x\nPUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
                "PUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
            ),
            (
                "a host.env without its last newline joins web.env's first line",
                b"PUNKTFUNK_NATIVE_PORT=48000PUNKTFUNK_UI_BIND=127.0.0.1:47990\n",
                "",
            ),
            ("a last line without a newline", b"PUNKTFUNK_NATIVE_PORT=48000", "PUNKTFUNK_NATIVE_PORT=48000\n"),
            ("nothing, an empty file", b"", ""),
            ("a NUL anywhere: grep passes no line", b"PUNKTFUNK_NATIVE_PORT=48000\n\0\n", ""),
        ];
        for (row, input, want) in rows {
            assert_eq!(port_lines(input), want, "{row}");
        }
    }

    /// The copy is root's to replace whole and every account's to read; a flood leaves it be.
    #[test]
    fn the_door_s_port_file_is_replaced_whole() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("host.env");
        let kept = "PUNKTFUNK_NATIVE_PORT=48000\n";
        ports(kept.as_bytes(), &to).unwrap();
        assert_eq!(std::fs::read_to_string(&to).unwrap(), kept);
        let mode = std::fs::metadata(&to).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);
        let flood = std::io::repeat(b'#').take(MAX_PORT_INPUT + 1);
        assert_eq!(ports(flood, &to).unwrap_err().code, "door_ports");
        assert_eq!(std::fs::read_to_string(&to).unwrap(), kept);
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
