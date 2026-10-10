//! `punktfunk-seats fence`: udev's `IMPORT{program}` for a seat's pad
//! (`packaging/linux/65-punktfunk-seats.rules`). It reads the device's `phys` out of sysfs and
//! prints the seat user it names, so the rule can hand the node to that user alone. A device it
//! cannot map prints nothing, and the node keeps the distro's rules. A device under a vhci port
//! is the binary's to map, by the port the supervisor recorded.

use super::accounts;
use crate::ipc::{Diagnostic, DiagnosticLevel};
use std::path::{Path, PathBuf};

/// What the supervisor stamps on a seat's pad: `punktfunk-seat:<account>/<index>`.
const PHYS_PREFIX: &str = "punktfunk-seat:";
/// This module's udev rule, and where distros keep udev rules.
const RULE: &str = "65-punktfunk-seats.rules";
const RULE_DIRS: [&str; 5] = [
    "/etc/udev/rules.d",
    "/run/udev/rules.d",
    "/usr/lib/udev/rules.d",
    "/lib/udev/rules.d",
    "/usr/local/lib/udev/rules.d",
];

/// Whether udev has the fence rule. Without it a seat's pads keep the distro's permissions.
pub fn rule_installed() -> bool {
    RULE_DIRS
        .iter()
        .any(|dir| Path::new(dir).join(RULE).exists())
}

/// The doctor's rows for seat pads: the fence rule, and the broker's socket.
pub fn diagnostics() -> Vec<Diagnostic> {
    let row = |ok: bool, code: &str, good: &str, bad: &str| Diagnostic {
        level: if ok {
            DiagnosticLevel::Info
        } else {
            DiagnosticLevel::Warning
        },
        code: code.into(),
        message: if ok { good } else { bad }.into(),
        seat_id: None,
    };
    vec![
        row(
            rule_installed(),
            "pad_fence",
            "the seat pad fence rule is installed",
            "The seat pad fence rule isn't installed, so the box's own Steam can open a seat's \
             controllers. Reinstall the punktfunk-seats package.",
        ),
        row(
            Path::new(pf_paths::seat::PADS_SOCKET).exists(),
            "pad_broker",
            "the pad broker is listening",
            "The pad broker isn't listening, so a seat gets no controller. Restart \
             punktfunk-seats.",
        ),
    ]
}

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
    if let Some(account) = seat_of_sysfs(&dev) {
        print_for(&account);
    }
}

/// The rule's lines for `account`, when it names a user other than root.
pub fn print_for(account: &str) {
    let Ok(Some(user)) = accounts::lookup(account) else {
        return;
    };
    if user.uid != 0 {
        print!("{}", lines(account, user.uid));
    }
}

/// What the rule reads: the user, its uid and the seat name the node goes on.
pub fn lines(account: &str, uid: u32) -> String {
    format!("PF_SEAT_USER={account}\nPF_SEAT_UID={uid}\nPF_SEAT=seat-punktfunk-{account}\n")
}

/// The account named by the `phys` of `dev` or of its parent: an `event*` node's is the input
/// device's attribute, a `hidraw*` node's is `HID_PHYS` in the HID device's `uevent`.
fn seat_of_sysfs(dev: &Path) -> Option<String> {
    [dev.to_path_buf(), dev.join("device")]
        .iter()
        .find_map(|d: &PathBuf| {
            let phys = std::fs::read_to_string(d.join("phys"))
                .ok()
                .or_else(|| hid_phys(&std::fs::read_to_string(d.join("uevent")).ok()?))?;
            account_of_phys(phys.trim())
        })
}

/// `HID_PHYS=` out of a HID device's `uevent`.
fn hid_phys(uevent: &str) -> Option<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_PHYS="))
        .map(str::to_owned)
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
    fn a_hid_devices_uevent_names_its_phys() {
        let uevent = "DRIVER=playstation\nHID_ID=0003:0000054C:00000CE6\n\
                      HID_NAME=Punktfunk DualSense 0\nHID_PHYS=punktfunk-seat:pf-seat-1/dualsense/0\n\
                      HID_UNIQ=punktfunk-ds-0\nMODALIAS=hid:b0003g0001v0000054Cp00000CE6\n";
        assert_eq!(
            hid_phys(uevent).as_deref().and_then(account_of_phys),
            Some("pf-seat-1".into())
        );
        assert_eq!(hid_phys("DRIVER=x\n"), None);
    }

    #[test]
    fn the_rule_reads_three_lines() {
        assert_eq!(
            lines("pf-seat-1", 987),
            "PF_SEAT_USER=pf-seat-1\nPF_SEAT_UID=987\nPF_SEAT=seat-punktfunk-pf-seat-1\n"
        );
    }
}
