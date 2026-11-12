//! `punktfunk-seats fence`: udev's `IMPORT{program}` for a seat's pad
//! (`packaging/linux/65-punktfunk-seats.rules`). A node whose device carries a seat's stamp in
//! `phys` goes to the seat the supervisor's record names for that exact device ([`vouch`]). A
//! stamped node without one prints `PF_SEAT_REFUSED`, and the rule makes it root's alone. A node
//! without a stamp prints nothing and keeps the distro's rules. A device under a vhci port is the
//! binary's to map, by the port the supervisor recorded. A seat's uinput proxy carries its
//! account in its name.

pub mod vouch;

use super::{accounts, session};
use crate::ipc::{Diagnostic, DiagnosticLevel};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// What the supervisor stamps on a seat's pad: `punktfunk-seat:<mark>/…`.
const PHYS_PREFIX: &str = "punktfunk-seat:";
/// The rule's line for a stamped node no record vouches for.
const REFUSED: &str = "PF_SEAT_REFUSED=1\n";
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
/// its account in its own name; any other node is [`judge`]d, and a vouched device whose
/// account is no seat's goes to nobody.
pub fn run() {
    let Some(devpath) = std::env::var_os("DEVPATH") else {
        return;
    };
    let Some(devpath) = devpath.to_str() else {
        return;
    };
    if let Some(account) = account_of_proxy(devpath) {
        print_for(&account);
        return;
    }
    let lines = match judge(Path::new("/sys"), vouch::trusted(), devpath) {
        Verdict::Nobody => return,
        Verdict::Seat(account) => seat_lines(&account),
        Verdict::Refused => None,
    };
    print!("{}", lines.as_deref().unwrap_or(REFUSED));
}

/// What the fence decides for a node.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No seat's stamp: the distro's rules stand.
    Nobody,
    /// A device the supervisor vouched for, the account's.
    Seat(String),
    /// A seat's stamp no record vouches for: root's alone.
    Refused,
}

/// The fence's rule for the node at `devpath`, with sysfs at `sys` and the supervisor's records
/// in `records` (`None` when they can't be trusted). A stamped node is a seat's only when its
/// device is one a record names: a uinput device by its kernel name, a uhid pad by the token in
/// its stamp, bound to the first HID device that carries it. The stamp never names the seat.
pub fn judge(sys: &Path, records: Option<&Path>, devpath: &str) -> Verdict {
    if !stamped(&sys.join(devpath.trim_start_matches('/'))) {
        return Verdict::Nobody;
    }
    let account = records.and_then(|dir| match device_under(devpath, vouch::UINPUT_DEVICES) {
        Some(input) if vouch::is_input(input) => vouch::account(dir, &format!("@{input}")),
        Some(_) => None,
        None => uhid_account(sys, dir, device_under(devpath, vouch::UHID_DEVICES)?),
    });
    account.map_or(Verdict::Refused, Verdict::Seat)
}

/// The device directly under `under` that `devpath` lies in.
fn device_under<'a>(devpath: &'a str, under: &str) -> Option<&'a str> {
    let rest = devpath
        .strip_prefix('/')?
        .strip_prefix(under)?
        .strip_prefix('/')?;
    rest.split('/').next().filter(|name| !name.is_empty())
}

/// The account a uhid pad's token names, when the HID device `hid` holds the token's bind: once
/// bound, only that device does; until then, the first to ask that no lower-numbered HID device
/// with the token precedes. The kernel numbers a HID device as it adds it, and a copy can read
/// the token only once the real one is added.
fn uhid_account(sys: &Path, dir: &Path, hid: &str) -> Option<String> {
    let devices = sys.join(vouch::UHID_DEVICES);
    let number = vouch::hid_number(hid)?;
    let token = token_of(&devices.join(hid))?;
    let account = vouch::account(dir, &token)?;
    let ours = match vouch::bound(dir, &token).ok()? {
        Some(bound) => bound == hid,
        None => !lower_twin(&devices, number, &token)? && vouch::bind(dir, &token, hid).ok()?,
    };
    ours.then_some(account)
}

/// The token in the stamp of the HID device at `hid`.
fn token_of(hid: &Path) -> Option<String> {
    let stamp = hid_phys(&std::fs::read_to_string(hid.join("uevent")).ok()?)?;
    let mark = stamp.trim().strip_prefix(PHYS_PREFIX)?.split('/').next()?;
    vouch::is_token(mark).then(|| mark.to_owned())
}

/// Whether a HID device numbered below `number` carries `token`.
fn lower_twin(devices: &Path, number: u64, token: &str) -> Option<bool> {
    let entries = std::fs::read_dir(devices).ok()?;
    Some(entries.flatten().any(|entry| {
        let name = entry.file_name();
        name.to_str()
            .and_then(vouch::hid_number)
            .is_some_and(|n| n < number)
            && token_of(&entry.path()).as_deref() == Some(token)
    }))
}

/// Prints [`seat_lines`]: nothing for an account that is no seat's.
pub fn print_for(account: &str) {
    if let Some(lines) = seat_lines(account) {
        print!("{lines}");
    }
}

/// The rule's lines for `account`, when it is a seat's user or the owner's row's. Any other
/// account, root's included, maps to nobody.
fn seat_lines(account: &str) -> Option<String> {
    let user = accounts::lookup(account).ok()??;
    (user.uid != 0 && fenced(&user, &session::env_path(account))).then(|| lines(account, user.uid))
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

/// Whether `dev` or an ancestor carries a seat's stamp: an input device's `phys`, or a HID
/// device's `HID_PHYS` in its `uevent`. A uhid pad's driver can leave its own input devices'
/// `phys` empty, so the stamp sits on the HID device up to three levels above the node.
fn stamped(dev: &Path) -> bool {
    dev.ancestors().take(5).any(|d| {
        let phys = std::fs::read_to_string(d.join("phys")).ok();
        let hid = std::fs::read_to_string(d.join("uevent"))
            .ok()
            .and_then(|uevent| hid_phys(&uevent));
        [phys, hid]
            .into_iter()
            .flatten()
            .any(|stamp| stamp.trim().starts_with(PHYS_PREFIX))
    })
}

/// `HID_PHYS=` out of a HID device's `uevent`.
fn hid_phys(uevent: &str) -> Option<String> {
    uevent
        .lines()
        .find_map(|line| line.strip_prefix("HID_PHYS="))
        .map(str::to_owned)
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
            hid_phys(uevent).as_deref(),
            Some("punktfunk-seat:pf-seat-1/dualsense/0")
        );
        assert_eq!(hid_phys("DRIVER=x\n"), None);
    }

    /// A fake sysfs and record directory, built the way the kernel and the supervisor lay
    /// them out.
    struct Tree {
        _temp: tempfile::TempDir,
        sys: std::path::PathBuf,
        dir: std::path::PathBuf,
    }

    impl Tree {
        fn new() -> Tree {
            let temp = tempfile::tempdir().unwrap();
            let (sys, dir) = (temp.path().join("sys"), temp.path().join("vouched"));
            std::fs::create_dir_all(&dir).unwrap();
            Tree {
                _temp: temp,
                sys,
                dir,
            }
        }

        /// A uinput device with `phys`, and its `event` node: the node's devpath.
        fn uinput(&self, input: &str, phys: &str) -> String {
            let dev = format!("/{}/{input}", vouch::UINPUT_DEVICES);
            let at = self.sys.join(&dev[1..]);
            std::fs::create_dir_all(at.join("event9")).unwrap();
            std::fs::write(at.join("phys"), format!("{phys}\n")).unwrap();
            format!("{dev}/event9")
        }

        /// A uhid HID device with `phys`, its hidraw node and its driver's input node with an
        /// empty `phys`, as hid-playstation leaves it: the two nodes' devpaths.
        fn uhid(&self, hid: &str, phys: &str) -> [String; 2] {
            let dev = format!("/{}/{hid}", vouch::UHID_DEVICES);
            let at = self.sys.join(&dev[1..]);
            std::fs::create_dir_all(at.join("hidraw/hidraw3")).unwrap();
            std::fs::create_dir_all(at.join("input/input77/event5")).unwrap();
            std::fs::write(at.join("input/input77/phys"), "\n").unwrap();
            std::fs::write(at.join("uevent"), format!("HID_PHYS={phys}\n")).unwrap();
            [
                format!("{dev}/hidraw/hidraw3"),
                format!("{dev}/input/input77/event5"),
            ]
        }

        fn judge(&self, devpath: &str) -> Verdict {
            judge(&self.sys, Some(&self.dir), devpath)
        }
    }

    fn seat(account: &str) -> Verdict {
        Verdict::Seat(account.into())
    }

    /// A uinput device is a seat's when the supervisor recorded its kernel name, whatever its
    /// stamp says; a stamped one it didn't record is nobody's, and an unstamped one isn't the
    /// fence's.
    #[test]
    fn a_uinput_device_passes_by_the_name_the_supervisor_recorded() {
        let tree = Tree::new();
        let mut vouch = vouch::Vouch::new(&tree.dir, "pf-seat-1", false).unwrap();
        // Numbers no test box has: `made` refreshes the real device's nodes.
        vouch.made("input900042").unwrap();
        let real = tree.uinput("input900042", "punktfunk-seat:pf-seat-1/steam/1");
        assert_eq!(tree.judge(&real), seat("pf-seat-1"));
        let forged = tree.uinput("input900041", "punktfunk-seat:pf-seat-1/steam/1");
        assert_eq!(
            tree.judge(&forged),
            Verdict::Refused,
            "a copy numbered lower"
        );
        let other = tree.uinput("input900043", "punktfunk-seat:pf-seat-2/0");
        assert_eq!(tree.judge(&other), Verdict::Refused, "another seat's stamp");
        let desk = tree.uinput("input900044", "");
        assert_eq!(tree.judge(&desk), Verdict::Nobody, "the box's own pad");
        assert_eq!(
            judge(&tree.sys, None, &real),
            Verdict::Refused,
            "records that can't be trusted"
        );
        drop(vouch);
        assert_eq!(tree.judge(&real), Verdict::Refused, "a withdrawn record");
    }

    /// A uhid pad's token binds the first HID device that carries it, and only its nodes pass:
    /// a copy numbered higher is refused before the bind and after, and so is a device that
    /// carries the token once the bound one is gone.
    #[test]
    fn a_uhid_pad_passes_by_its_token_bound_to_the_lowest_device() {
        let tree = Tree::new();
        let vouch = vouch::Vouch::new(&tree.dir, "pf-seat-1", true).unwrap();
        let stamp = format!("punktfunk-seat:{}/dualsense/0", vouch.mark());
        let [raw, input] = tree.uhid("0003:054C:0CE6.0014", &stamp);
        let [copy_raw, copy_input] = tree.uhid("0003:054C:0CE6.0015", &stamp);
        assert_eq!(
            tree.judge(&copy_input),
            Verdict::Refused,
            "a lower one is there"
        );
        assert_eq!(tree.judge(&raw), seat("pf-seat-1"));
        assert_eq!(
            tree.judge(&input),
            seat("pf-seat-1"),
            "a child of the bound one"
        );
        assert_eq!(tree.judge(&copy_raw), Verdict::Refused, "bound to another");

        let gone = tree
            .sys
            .join(vouch::UHID_DEVICES)
            .join("0003:054C:0CE6.0014");
        std::fs::remove_dir_all(gone).unwrap();
        let [later, _] = tree.uhid("0003:054C:0CE6.0016", &stamp);
        assert_eq!(
            tree.judge(&later),
            Verdict::Refused,
            "after the bound one is gone"
        );
    }

    /// A stamp without a record, with a record of another token, or in the old form that named
    /// the account, is nobody's; an unstamped uhid device, such as a Bluetooth pad's, isn't the
    /// fence's.
    #[test]
    fn a_uhid_stamp_without_its_record_is_refused() {
        let tree = Tree::new();
        let vouch = vouch::Vouch::new(&tree.dir, "pf-seat-1", true).unwrap();
        let unrecorded = format!("punktfunk-seat:{}/dualsense/0", "A".repeat(22));
        let [raw, _] = tree.uhid("0003:054C:0CE6.0001", &unrecorded);
        assert_eq!(tree.judge(&raw), Verdict::Refused);
        let [old, _] = tree.uhid(
            "0003:054C:0CE6.0002",
            "punktfunk-seat:pf-seat-1/dualsense/0",
        );
        assert_eq!(tree.judge(&old), Verdict::Refused);
        let [bluetooth, _] = tree.uhid("0005:054C:0CE6.0003", "a0:b1:c2:d3:e4:f5");
        assert_eq!(tree.judge(&bluetooth), Verdict::Nobody);
        assert!(vouch::is_token(vouch.mark()));
    }

    /// A stamped node that neither uinput nor uhid made is nobody's.
    #[test]
    fn a_stamp_on_any_other_device_is_refused() {
        let tree = Tree::new();
        let dev = "/devices/pci0000:00/0000:00:14.0/usb1/1-2/1-2:1.0/input/input8";
        std::fs::create_dir_all(tree.sys.join(&dev[1..]).join("event2")).unwrap();
        std::fs::write(tree.sys.join(&dev[1..]).join("phys"), "punktfunk-seat:x/0").unwrap();
        assert_eq!(tree.judge(&format!("{dev}/event2")), Verdict::Refused);
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
