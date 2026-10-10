//! `punktfunk-seats fence`: udev's `IMPORT{program}` for a seat's pad
//! (`packaging/linux/65-punktfunk-seats.rules`). It reads the device's `phys` out of sysfs and
//! prints the seat user it names, so the rule can hand the node to that user alone. A device it
//! cannot map prints nothing, and the node keeps the distro's rules.

use super::accounts;
use std::path::{Path, PathBuf};

/// What the supervisor stamps on a seat's pad: `punktfunk-seat:<account>/<index>`.
const PHYS_PREFIX: &str = "punktfunk-seat:";

/// The udev program. Never fails: udev takes the lines it gets.
pub fn run() {
    let Some(devpath) = std::env::var_os("DEVPATH") else {
        return;
    };
    let dev = Path::new("/sys").join(
        Path::new(&devpath)
            .strip_prefix("/")
            .unwrap_or(Path::new(&devpath)),
    );
    let Some(account) = seat_of_sysfs(&dev) else {
        return;
    };
    let Ok(Some(user)) = accounts::lookup(&account) else {
        return;
    };
    if user.uid == 0 {
        return;
    }
    print!("{}", lines(&account, user.uid));
}

/// What the rule reads: the user, its uid and the seat name the node goes on.
pub fn lines(account: &str, uid: u32) -> String {
    format!("PF_SEAT_USER={account}\nPF_SEAT_UID={uid}\nPF_SEAT=seat-punktfunk-{account}\n")
}

/// The account named by the `phys` of `dev`, or of its parent (an `event*` node's is the
/// input device's).
fn seat_of_sysfs(dev: &Path) -> Option<String> {
    [dev.to_path_buf(), dev.join("device")]
        .iter()
        .find_map(|d: &PathBuf| {
            let phys = std::fs::read_to_string(d.join("phys")).ok()?;
            account_of_phys(phys.trim())
        })
}

/// `punktfunk-seat:<account>/<index>` → the account, when it is a plain user name.
pub fn account_of_phys(phys: &str) -> Option<String> {
    let rest = phys.strip_prefix(PHYS_PREFIX)?;
    let account = rest.split('/').next()?;
    let plain = !account.is_empty()
        && account.len() <= 32
        && account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    plain.then(|| account.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_stamped_plain_account_is_fenced() {
        assert_eq!(
            account_of_phys("punktfunk-seat:pf-seat-1/0"),
            Some("pf-seat-1".into())
        );
        assert_eq!(
            account_of_phys("punktfunk-seat:bazzite/3"),
            Some("bazzite".into())
        );
        assert_eq!(account_of_phys("usb-0000:00:14.0-1/input0"), None);
        assert_eq!(account_of_phys("punktfunk-seat:/0"), None);
        assert_eq!(account_of_phys("punktfunk-seat:../root/0"), None);
        assert_eq!(account_of_phys("punktfunk-seat:a b/0"), None);
    }

    #[test]
    fn the_rule_reads_three_lines() {
        assert_eq!(
            lines("pf-seat-1", 987),
            "PF_SEAT_USER=pf-seat-1\nPF_SEAT_UID=987\nPF_SEAT=seat-punktfunk-pf-seat-1\n"
        );
    }
}
