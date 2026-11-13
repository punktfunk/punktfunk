//! The person token: 32 random bytes a seat host writes into its own config folder at each
//! start, `0600`, so only the account it runs as can read it. A ticket signed with it proves the
//! caller is that account: `ctl console-url` signs one inside the seat, and the seat host
//! redeems it for a person session (`mgmt::person_session`). The supervisor and the door never
//! read it.

use hmac::{Hmac, KeyInit, Mac};
#[cfg(any(unix, test))]
use pf_paths::seat::{HostRole, SeatRole};
#[cfg(unix)]
use std::path::Path;

/// The token's file in the seat host's config folder.
#[cfg(unix)]
pub(crate) const FILE: &str = pf_paths::seat::PERSON_TOKEN;

/// How long a ticket may be redeemed, either side of its timestamp.
pub(crate) const TICKET_TTL_SECS: u64 = 60;

/// Whether a host in `role` mints one: a seat host of its own, made for its seat or on a
/// person's own account. Never the owner's row, the door or a plain host; a Windows seat host
/// runs as SYSTEM, where no file is the person's alone.
#[cfg(any(unix, test))]
pub(crate) fn mints(role: HostRole) -> bool {
    cfg!(target_os = "linux") && role == HostRole::Seat(SeatRole::Contract)
}

/// A fresh token in `dir`'s [`FILE`], replacing what was there. `Err` says why the person lane
/// stays closed: `dir` is not this account's alone, or the file did not come out `0600` and ours.
#[cfg(unix)]
pub(crate) fn mint_in(dir: &Path) -> Result<String, String> {
    private_folder(dir)?;
    let token = crate::mgmt_token::random_token();
    let path = dir.join(FILE);
    pf_paths::replace_secret_file(&path, format!("{token}\n").as_bytes())
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    match read_in(dir) {
        Ok(read) if read == token => Ok(token),
        Ok(_) => Err(format!("{} changed as it was written", path.display())),
        Err(why) => Err(why),
    }
}

/// The token in `dir`'s [`FILE`], when `dir` is this account's alone and the file is a `0600`
/// file of ours holding one.
#[cfg(unix)]
pub(crate) fn read_in(dir: &Path) -> Result<String, String> {
    use std::os::unix::fs::MetadataExt;
    private_folder(dir)?;
    let path = dir.join(FILE);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() || meta.uid() != crate::geteuid() || meta.mode() & 0o777 != 0o600 {
        return Err(format!(
            "{} is not a 0600 file of this account",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let token = text.trim();
    let hex = token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit());
    hex.then(|| token.to_string())
        .ok_or_else(|| format!("{} holds no token", path.display()))
}

/// `dir` is a folder, not a link, of this account, with nothing for its group or others.
#[cfg(unix)]
fn private_folder(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != crate::geteuid() || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} is not a folder of this account alone",
            dir.display()
        ));
    }
    Ok(())
}

/// One ticket, as [`parse`] read it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Ticket<'a> {
    pub(crate) ts: u64,
    pub(crate) nonce: &'a str,
    pub(crate) profile: &'a str,
    mac: &'a str,
}

/// The signed message. A cross-process contract: `ctl` signs it, the seat host checks it.
fn message(ts: u64, nonce: &str, profile: &str) -> String {
    format!("pf-person-session:v1:{ts}:{nonce}:{profile}")
}

fn mac_of(token: &str) -> Hmac<sha2::Sha256> {
    Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length")
}

/// A ticket for `profile`, issued at `ts` with `nonce` (lowercase hex): `<ts>.<nonce>.<profile>.<mac>`,
/// the MAC HMAC-SHA256 over [`message`] keyed by the person token. Every byte is `[0-9a-f.]`.
#[cfg(any(unix, test))]
pub(crate) fn sign(token: &str, ts: u64, nonce: &str, profile: &str) -> String {
    let mut mac = mac_of(token);
    mac.update(message(ts, nonce, profile).as_bytes());
    let mac = hex::encode(mac.finalize().into_bytes());
    format!("{ts}.{nonce}.{profile}.{mac}")
}

/// `ticket` split into its parts, when each has its shape: decimal seconds, a 32-hex nonce, a
/// profile id and a 64-hex MAC.
pub(crate) fn parse(ticket: &str) -> Option<Ticket<'_>> {
    let mut parts = ticket.split('.');
    let (ts, nonce, profile, mac) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
    let decimal = !ts.is_empty() && ts.len() <= 20 && ts.bytes().all(|b| b.is_ascii_digit());
    let lower = |s: &str| !s.bytes().any(|b| b.is_ascii_uppercase());
    let shaped = parts.next().is_none()
        && decimal
        && hex(nonce, 32)
        && hex(mac, 64)
        && lower(nonce)
        && lower(mac)
        && crate::library::is_profile_id(profile);
    shaped.then_some(Ticket {
        ts: ts.parse().ok()?,
        nonce,
        profile,
        mac,
    })
}

impl Ticket<'_> {
    /// Whether `token` signed this ticket. Constant-time in the MAC.
    pub(crate) fn signed_by(&self, token: &str) -> bool {
        let Ok(want) = hex::decode(self.mac) else {
            return false;
        };
        let mut mac = mac_of(token);
        mac.update(message(self.ts, self.nonce, self.profile).as_bytes());
        mac.verify_slice(&want).is_ok()
    }

    /// Whether it was issued within [`TICKET_TTL_SECS`] of `now`, either side.
    pub(crate) fn fresh(&self, now: u64) -> bool {
        now.abs_diff(self.ts) <= TICKET_TTL_SECS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
    const NONCE: &str = "00112233445566778899aabbccddeeff";

    /// Only the token that signed a ticket verifies it, and any changed part fails. The form is
    /// the cross-process contract `ctl` and the seat host share.
    #[test]
    fn a_ticket_verifies_with_its_token_alone() {
        let ticket = sign(TOKEN, 1_000, NONCE, "a1b2c3d4e5f6");
        let parsed = parse(&ticket).unwrap();
        assert_eq!(
            (parsed.ts, parsed.nonce, parsed.profile),
            (1_000, NONCE, "a1b2c3d4e5f6")
        );
        assert!(parsed.signed_by(TOKEN));
        assert!(!parsed.signed_by(&TOKEN.replace('0', "1")));
        for forged in [
            ticket.replacen("1000", "1001", 1),
            ticket.replace("a1b2c3d4e5f6", "a1b2c3d4e5f7"),
            ticket.replace(NONCE, &NONCE.replace('0', "9")),
        ] {
            assert!(!parse(&forged).unwrap().signed_by(TOKEN), "{forged}");
        }
        assert!(parsed.fresh(1_060) && parsed.fresh(940));
        assert!(!parsed.fresh(1_061) && !parsed.fresh(939));
    }

    #[test]
    fn a_ticket_out_of_shape_is_no_ticket() {
        let good = sign(TOKEN, 1_000, NONCE, "a1b2c3d4e5f6");
        assert!(parse(&good).is_some());
        assert!(good.bytes().all(|b| b.is_ascii_hexdigit() || b == b'.'));
        for bad in [
            String::new(),
            good.replace("a1b2c3d4e5f6", "A1B2C3D4E5F6"),
            good.replace("a1b2c3d4e5f6", "../../../etc"),
            good.to_uppercase(),
            format!("{good}.x"),
            good.replacen("1000", "-1", 1),
            good.replacen("1000", "", 1),
            good.replace(NONCE, "0011"),
        ] {
            assert!(parse(&bad).is_none(), "{bad}");
        }
    }

    /// The token lands `0600` in a `0700` folder of this account and reads back. A folder others
    /// can enter keeps the lane closed, and a planted file is replaced.
    #[cfg(unix)]
    #[test]
    fn the_token_is_this_account_s_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(mint_in(dir.path()).is_err(), "a group may enter");
        assert!(
            !dir.path().join(FILE).exists(),
            "written into an open folder"
        );
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let planted = dir.path().join(FILE);
        std::fs::write(&planted, format!("{TOKEN}\n")).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_in(dir.path()).is_err(), "a 0644 token is read");
        let token = mint_in(dir.path()).unwrap();
        assert_ne!(token, TOKEN, "the planted token was kept");
        assert_eq!(mode(&planted), 0o600);
        assert_eq!(read_in(dir.path()).unwrap(), token);
        assert_ne!(mint_in(dir.path()).unwrap(), token, "a start mints anew");
        let link = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(dir.path(), link.path().join("config")).unwrap();
        assert!(
            read_in(&link.path().join("config")).is_err(),
            "read through a link"
        );
    }

    #[test]
    fn only_a_seat_host_of_its_own_mints_one() {
        for role in HostRole::ALL {
            let want = cfg!(target_os = "linux") && role == HostRole::Seat(SeatRole::Contract);
            assert_eq!(mints(role), want, "{role:?}");
        }
    }
}
