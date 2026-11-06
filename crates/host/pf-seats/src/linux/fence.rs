//! `punktfunk-seats fence`: udev's `IMPORT{program}` for a seat's pad
//! (`packaging/linux/65-punktfunk-seats.rules`). It reads the device's `phys` out of sysfs and
//! prints the seat user it names, so the rule can hand the node to that user alone. A device it
//! cannot map prints nothing, and the node keeps the distro's rules. A device under a vhci port
//! is the binary's to map, by the port the supervisor recorded. A seat's uinput proxy carries
//! its account in its name.

use super::{accounts, session};
use crate::ipc::{Diagnostic, DiagnosticLevel};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

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

/// The doctor's rows for seat pads: the fence rule, the broker's socket, and `/dev/cuse`, which
/// a seat's uinput proxy is made through and which must be root's alone: whoever opens it can
/// serve a seat's node first.
pub fn diagnostics() -> Vec<Diagnostic> {
    let row = |ok: bool, code: &str, good: &str, bad: &str| Diagnostic {
        level: if ok {
            DiagnosticLevel::Info
        } else {
            DiagnosticLevel::Warning
        },
        ..Diagnostic::info(code, if ok { good } else { bad })
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
        match std::fs::metadata("/dev/cuse") {
            Err(_) => row(
                false,
                "uinput_proxy",
                "",
                "/dev/cuse is missing, so a seat's Steam can't make the controller games read. \
                 Load the cuse kernel module.",
            ),
            Ok(meta) => row(
                root_only(meta.uid(), meta.gid(), meta.mode()),
                "uinput_proxy",
                "/dev/cuse is present for the seats' uinput proxy, and root's alone",
                "/dev/cuse opens to other users, so one of them could serve a seat's \
                 controllers. Make it root's alone, mode 0600.",
            ),
        },
    ]
}

/// Whether a node with this owner, group and mode opens for root alone.
fn root_only(uid: u32, gid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o007 == 0 && (gid == 0 || mode & 0o070 == 0)
}

/// The udev program. Never fails: udev takes the lines it gets. A seat's uinput proxy names
/// its account in its own name.
pub fn run() {
    let Some(devpath) = std::env::var_os("DEVPATH") else {
        return;
    };
    if let Some(account) = devpath.to_str().and_then(account_of_proxy) {
        print_for(&account);
        return;
    }
    let dev = Path::new("/sys").join(
        Path::new(&devpath)
            .strip_prefix("/")
            .unwrap_or(Path::new(&devpath)),
    );
    if let Some(account) = seat_of_sysfs(&dev) {
        print_for(&account);
    }
}

/// The rule's lines for `account`, when it is a seat's user or the owner's row's. Any other
/// stamp, root's included, maps to nobody.
pub fn print_for(account: &str) {
    let Ok(Some(user)) = accounts::lookup(account) else {
        return;
    };
    if user.uid != 0 && fenced(&user, &session::env_path(account)) {
        print!("{}", lines(account, user.uid));
    }
}

/// Whether `user` has a ledger row: a seat's marker, or `unit_env`, the environment the
/// supervisor writes for a row and that only the owner's row has without the marker.
fn fenced(user: &accounts::Passwd, unit_env: &Path) -> bool {
    accounts::is_seat(user) || unit_env.exists()
}

/// What the rule reads: the user, its uid and the seat name the node goes on.
pub fn lines(account: &str, uid: u32) -> String {
    format!("PF_SEAT_USER={account}\nPF_SEAT_UID={uid}\nPF_SEAT=seat-punktfunk-{account}\n")
}

/// The account stamped on `dev` or its nearest stamped ancestor: an input device's `phys`, or a
/// HID device's `HID_PHYS` in its `uevent`. A uhid pad's driver can leave its own input devices'
/// `phys` empty, so the stamp sits on the HID device up to three levels above the node.
fn seat_of_sysfs(dev: &Path) -> Option<String> {
    dev.ancestors().take(5).find_map(|d| {
        let phys = std::fs::read_to_string(d.join("phys")).ok();
        let hid = std::fs::read_to_string(d.join("uevent"))
            .ok()
            .and_then(|uevent| hid_phys(&uevent));
        [phys, hid]
            .into_iter()
            .flatten()
            .find_map(|stamp| account_of_phys(stamp.trim()))
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
    plain(phys.strip_prefix(PHYS_PREFIX)?.split('/').next()?)
}

/// The account in a uinput proxy's devpath, `/devices/virtual/cuse/punktfunk!uinput-<account>`,
/// when it is a plain user name. Sysfs spells the devname's `/` as `!`.
fn account_of_proxy(devpath: &str) -> Option<String> {
    let name = devpath.strip_prefix("/devices/virtual/cuse/")?;
    let prefix = pf_paths::seat::uinput_devname("").replace('/', "!");
    plain(name.strip_prefix(&prefix)?)
}

fn plain(account: &str) -> Option<String> {
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
    fn a_uinput_proxy_names_its_seat() {
        let cuse = "/devices/virtual/cuse/";
        assert_eq!(
            account_of_proxy(&format!("{cuse}punktfunk!uinput-pf-seat-2")),
            Some("pf-seat-2".into())
        );
        assert_eq!(account_of_proxy(&format!("{cuse}punktfunk!uinput-")), None);
        assert_eq!(
            account_of_proxy(&format!("{cuse}punktfunk!uinput-a!b")),
            None
        );
        assert_eq!(account_of_proxy(&format!("{cuse}fuse-thing")), None);
        assert_eq!(
            account_of_proxy("/devices/virtual/misc/punktfunk!uinput-pf-seat-2"),
            None
        );
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

    /// hid-playstation's input devices leave `phys` empty: the node's stamp is the HID device's,
    /// three levels up. A node with no stamp anywhere above is nobody's.
    #[test]
    fn a_uhid_pads_input_node_finds_the_hid_devices_stamp() {
        let root = std::env::temp_dir().join(format!("pf-fence-{}", std::process::id()));
        let hid = root.join("uhid/0003:054C:0CE6.0014");
        let input = hid.join("input/input77");
        std::fs::create_dir_all(input.join("event5")).unwrap();
        std::fs::write(input.join("phys"), "\n").unwrap();
        std::fs::write(
            hid.join("uevent"),
            "HID_PHYS=punktfunk-seat:pf-seat-1/dualsense/0\n",
        )
        .unwrap();
        assert_eq!(
            seat_of_sysfs(&input.join("event5")),
            Some("pf-seat-1".into())
        );
        std::fs::write(hid.join("uevent"), "HID_PHYS=usb-0000:00:14.0-1/input0\n").unwrap();
        assert_eq!(seat_of_sysfs(&input.join("event5")), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A stamp maps only to a seat's account or the owner's row's; a person's account the
    /// supervisor wrote nothing for is nobody's.
    #[test]
    fn only_a_seat_or_the_owners_row_is_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let env = temp.path().join("bob.env");
        let user = |gecos: &str| {
            accounts::parse_passwd(&format!("bob:x:1000:1000:{gecos}:/home/bob:/bin/sh")).unwrap()
        };
        let seat = user("punktfunk-seat=0123456789abcdef0123456789abcdef");
        assert!(fenced(&seat, &env), "a seat's account");
        assert!(!fenced(&user("Bob"), &env), "a person with no row");
        std::fs::write(&env, "").unwrap();
        assert!(fenced(&user("Bob"), &env), "the owner's row");
    }

    #[test]
    fn only_a_root_only_cuse_node_passes_the_doctor() {
        let cases = [
            ((0, 0, 0o020600), true, "root 0600"),
            ((0, 0, 0o020660), true, "group root"),
            ((0, 5, 0o020600), true, "another group, no group bits"),
            ((0, 5, 0o020660), false, "another group may open it"),
            ((0, 0, 0o020666), false, "everyone may open it"),
            ((1000, 0, 0o020600), false, "a user's"),
        ];
        for ((uid, gid, mode), only, what) in cases {
            assert_eq!(root_only(uid, gid, mode), only, "{what}");
        }
    }

    #[test]
    fn the_rule_reads_three_lines() {
        assert_eq!(
            lines("pf-seat-1", 987),
            "PF_SEAT_USER=pf-seat-1\nPF_SEAT_UID=987\nPF_SEAT=seat-punktfunk-pf-seat-1\n"
        );
    }
}
