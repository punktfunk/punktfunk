//! `punktfunk-seats node-owner`: tells udev which user owns a controller's device node.
//!
//! udev runs it for each device node, through `IMPORT{program}` in
//! `packaging/linux/65-punktfunk-seats.rules`. It prints `PF_SEAT_USER` lines, which give the node
//! to one seat's user, or `PF_SEAT_REFUSED`, which makes the node root-only, or nothing, which
//! leaves the distro's rules in charge. How it decides:
//!
//! - The supervisor stamps the `phys` of every pad it makes with `punktfunk-seat:`. A stamped
//!   node goes to a seat only when the supervisor recorded that exact device ([`made_devices`]).
//!   A stamped node with no record is refused, because anyone can write a stamp.
//! - A node with no stamp is left alone, unless it belongs to a desk pad lent to a seat
//!   ([`judge_lent`]).
//! - A device under a USB/IP port is decided in the daemon binary, by the port the supervisor
//!   recorded for the seat.
//! - A seat's `/dev/uinput` stand-in carries the seat's account in its device name.
//!
//! A node that udev classes as a keyboard or a mouse never goes to a seat ([`InputId::desk`]).

pub mod made_devices;

use super::{accounts, lent, session};
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

/// Whether udev has the udev rule for seat pads. Without it a seat's pads keep the distro's permissions.
pub fn rule_installed() -> bool {
    RULE_DIRS
        .iter()
        .any(|dir| Path::new(dir).join(RULE).exists())
}

/// Whether the udev rule for seat pads udev reads first runs the helper on a lent desk pad's nodes.
fn lends_installed() -> bool {
    let rule = RULE_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(RULE))
        .find(|rule| rule.exists());
    rule.and_then(|rule| std::fs::read_to_string(rule).ok())
        .is_some_and(|text| text.contains("pads/lent"))
}

/// The doctor's rows for seat pads: the udev rule for seat pads and its line for lent desk pads, the broker's
/// socket, and `/dev/cuse`, which a seat's uinput proxy is made through and which must be root's
/// alone: whoever opens it can serve a seat's node first.
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
        lending_row(lends_installed()),
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

/// The doctor's row for the rule line that hands a lent desk pad over, by whether it is there.
fn lending_row(installed: bool) -> Diagnostic {
    if installed {
        Diagnostic::info(
            "pad_lending",
            "The rule that gives a controller to a profile is installed",
        )
        .worded("seatcheck_pad_lending_ok")
    } else {
        Diagnostic::warning(
            "pad_lending",
            "The installed seat rule can't give a controller to a profile, so a controller \
             given away stays with this PC. Reinstall the punktfunk-seats package.",
        )
        .worded("seatcheck_pad_lending_missing")
    }
}

/// Whether a node with this owner, group and mode opens for root alone.
fn root_only(uid: u32, gid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o007 == 0 && (gid == 0 || mode & 0o070 == 0)
}

/// The udev program. Never fails: udev takes the lines it gets. A seat's uinput proxy names
/// its account in its own name; any other node is [`judge`]d, and one without a seat's stamp
/// is judged as a lent desk pad's ([`judge_lent`]). A node whose account is no seat's goes to
/// nobody.
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
    let sys = Path::new("/sys");
    let verdict = match judge(sys, made_devices::trusted(), devpath) {
        Verdict::Nobody => {
            let records = Path::new(lent::DIR);
            let trusted = lent::trusted(records).then_some(records);
            judge_lent(sys, trusted, devpath, InputId::from_env())
        }
        verdict => verdict,
    };
    let lines = match verdict {
        Verdict::Nobody => return,
        Verdict::Seat(account) => lines_for(&account, InputId::from_env()),
        Verdict::Refused => None,
    };
    print!("{}", lines.as_deref().unwrap_or(REFUSED));
}

/// What the udev helper decides for a node.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No seat's stamp and no lend: the distro's rules stand.
    Nobody,
    /// A device the supervisor vouched for, or a live lent pad's node: the account's.
    Seat(String),
    /// A seat's stamp no record vouches for, or a lent pad's node no seat may have: root's alone.
    Refused,
}

/// The input classes udev gave the node, from the properties it passes the program.
#[derive(Clone, Copy, Debug, Default)]
pub struct InputId {
    pub keyboard: bool,
    pub mouse: bool,
    pub joystick: bool,
}

impl InputId {
    fn from_env() -> InputId {
        let set = |name: &str| std::env::var(name).as_deref() == Ok("1");
        InputId {
            keyboard: set("ID_INPUT_KEYBOARD"),
            mouse: set("ID_INPUT_MOUSE"),
            joystick: set("ID_INPUT_JOYSTICK"),
        }
    }

    /// Whether udev classes the node as a keyboard or a mouse and not as a joystick. Such a node
    /// never goes to a seat, whichever record names its device: whoever can write it types into
    /// the kernel's own keyboard, SysRq and pointer handlers, which serve the box's console.
    pub fn desk(self) -> bool {
        (self.keyboard || self.mouse) && !self.joystick
    }
}

/// The lend records' rule for the node at `devpath`, with sysfs at `sys` and the records in
/// `records` (`None` when they can't be trusted). A node on a real USB bus whose device a
/// record names by either of its keys, and not a twin of it, goes to the live holder's account.
/// A parked pad's node,
/// and a keyboard or mouse node of any lent pad, is root's alone: a boot keyboard or mouse
/// interface, or input udev classes as keyboard or mouse and not as joystick. So is the
/// device's own node, which speaks raw USB: its holder could take a keyboard interface from the
/// kernel through it.
pub fn judge_lent(sys: &Path, records: Option<&Path>, devpath: &str, input: InputId) -> Verdict {
    let Some(records) = records else {
        return Verdict::Nobody;
    };
    let Some((device, interface)) = lent::node_device(sys, devpath) else {
        return Verdict::Nobody;
    };
    let Some(holder) = lent::holder_of(sys, records, &device) else {
        return Verdict::Nobody;
    };
    let Some(class) = interface else {
        return Verdict::Refused;
    };
    let desk_input = lent::boot_input(class) || input.desk();
    match plain(&holder) {
        Some(account) if holder != lent::PARKED && !desk_input => Verdict::Seat(account),
        _ => Verdict::Refused,
    }
}

/// The udev helper's rule for the node at `devpath`, with sysfs at `sys` and the supervisor's records
/// in `records` (`None` when they can't be trusted). A stamped node is a seat's only when its
/// device is one a record names: a uinput device by its kernel name, a uhid pad by the token in
/// its stamp, bound to the first HID device that carries it. The stamp never names the seat.
pub fn judge(sys: &Path, records: Option<&Path>, devpath: &str) -> Verdict {
    if !stamped(&sys.join(devpath.trim_start_matches('/'))) {
        return Verdict::Nobody;
    }
    let account =
        records.and_then(
            |dir| match device_under(devpath, made_devices::UINPUT_DEVICES) {
                Some(input) if made_devices::is_input(input) => {
                    made_devices::account(dir, &format!("@{input}"))
                }
                Some(_) => None,
                None => uhid_account(sys, dir, device_under(devpath, made_devices::UHID_DEVICES)?),
            },
        );
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
    let devices = sys.join(made_devices::UHID_DEVICES);
    let number = made_devices::hid_number(hid)?;
    let token = token_of(&devices.join(hid))?;
    let account = made_devices::account(dir, &token)?;
    let ours = match made_devices::bound(dir, &token).ok()? {
        Some(bound) => bound == hid,
        None => {
            !lower_twin(&devices, number, &token)? && made_devices::bind(dir, &token, hid).ok()?
        }
    };
    ours.then_some(account)
}

/// The token in the stamp of the HID device at `hid`.
fn token_of(hid: &Path) -> Option<String> {
    let stamp = hid_phys(&std::fs::read_to_string(hid.join("uevent")).ok()?)?;
    let mark = stamp.trim().strip_prefix(PHYS_PREFIX)?.split('/').next()?;
    made_devices::is_token(mark).then(|| mark.to_owned())
}

/// Whether a HID device numbered below `number` carries `token`.
fn lower_twin(devices: &Path, number: u64, token: &str) -> Option<bool> {
    let entries = std::fs::read_dir(devices).ok()?;
    Some(entries.flatten().any(|entry| {
        let name = entry.file_name();
        name.to_str()
            .and_then(made_devices::hid_number)
            .is_some_and(|n| n < number)
            && token_of(&entry.path()).as_deref() == Some(token)
    }))
}

/// Prints the rule's lines for a node the caller found to be `account`'s: a seat's uinput proxy,
/// or a node under a seat's USB port. Prints nothing for an account that is no seat's.
pub fn print_for(account: &str) {
    if let Some(lines) = lines_for(account, InputId::from_env()) {
        print!("{lines}");
    }
}

/// The rule's lines for a node of `account`'s device, which every seat verdict ends in. A
/// keyboard or mouse node is refused here ([`InputId::desk`]): an emulated Steam Deck carries
/// both beside its controller. `None` for an account that is no seat's.
fn lines_for(account: &str, input: InputId) -> Option<String> {
    if input.desk() {
        return Some(REFUSED.to_owned());
    }
    seat_lines(account)
}

/// The rule's lines for `account`, when it is a seat's user or the owner's row's. Any other
/// account, root's included, maps to nobody.
fn seat_lines(account: &str) -> Option<String> {
    let user = accounts::lookup(account).ok()??;
    (user.uid != 0 && has_row(&user, &session::env_path(account))).then(|| lines(account, user.uid))
}

/// Whether `user` has a ledger row: a seat's marker, or `unit_env`, the environment the
/// supervisor writes for a row and that only the owner's row has without the marker.
fn has_row(user: &accounts::Passwd, unit_env: &Path) -> bool {
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
            let dev = format!("/{}/{input}", made_devices::UINPUT_DEVICES);
            let at = self.sys.join(&dev[1..]);
            std::fs::create_dir_all(at.join("event9")).unwrap();
            std::fs::write(at.join("phys"), format!("{phys}\n")).unwrap();
            format!("{dev}/event9")
        }

        /// A uhid HID device with `phys`, its hidraw node and its driver's input node with an
        /// empty `phys`, as hid-playstation leaves it: the two nodes' devpaths.
        fn uhid(&self, hid: &str, phys: &str) -> [String; 2] {
            let dev = format!("/{}/{hid}", made_devices::UHID_DEVICES);
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
    /// stamp says; a stamped one it didn't record is nobody's, and an unstamped one is left to
    /// the distro's rules.
    #[test]
    fn a_uinput_device_passes_by_the_name_the_supervisor_recorded() {
        let tree = Tree::new();
        let mut record = made_devices::DeviceRecord::new(&tree.dir, "pf-seat-1", false).unwrap();
        // Numbers no test box has: `made` refreshes the real device's nodes.
        record.made("input900042").unwrap();
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
        drop(record);
        assert_eq!(tree.judge(&real), Verdict::Refused, "a withdrawn record");
    }

    /// A uhid pad's token binds the first HID device that carries it, and only its nodes pass:
    /// a copy numbered higher is refused before the bind and after, and so is a device that
    /// carries the token once the bound one is gone.
    #[test]
    fn a_uhid_pad_passes_by_its_token_bound_to_the_lowest_device() {
        let tree = Tree::new();
        let record = made_devices::DeviceRecord::new(&tree.dir, "pf-seat-1", true).unwrap();
        let stamp = format!("punktfunk-seat:{}/dualsense/0", record.mark());
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
            .join(made_devices::UHID_DEVICES)
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
    /// the account, is nobody's; an unstamped uhid device, such as a Bluetooth pad's, is left to
    /// the distro's rules.
    #[test]
    fn a_uhid_stamp_without_its_record_is_refused() {
        let tree = Tree::new();
        let record = made_devices::DeviceRecord::new(&tree.dir, "pf-seat-1", true).unwrap();
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
        assert!(made_devices::is_token(record.mark()));
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
    fn only_a_seat_or_the_owner_s_row_gets_nodes() {
        let temp = tempfile::tempdir().unwrap();
        let env = temp.path().join("bob.env");
        let user = |gecos: &str| {
            accounts::parse_passwd(&format!("bob:x:1000:1000:{gecos}:/home/bob:/bin/sh")).unwrap()
        };
        let seat = user("punktfunk-seat=0123456789abcdef0123456789abcdef");
        assert!(has_row(&seat, &env), "a seat's account");
        assert!(!has_row(&user("Bob"), &env), "a person with no row");
        std::fs::write(&env, "").unwrap();
        assert!(has_row(&user("Bob"), &env), "the owner's row");
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

    /// A keyboard or mouse node is refused for any account, before the account is even looked
    /// up. A joystick that also has keys, a touchpad and a hidraw node are not desk input.
    #[test]
    fn a_keyboard_or_mouse_node_never_goes_to_a_seat() {
        let input = |keyboard, mouse, joystick| InputId {
            keyboard,
            mouse,
            joystick,
        };
        for desk in [
            input(true, false, false),
            input(false, true, false),
            input(true, true, false),
        ] {
            assert_eq!(lines_for("pf-seat-1", desk).as_deref(), Some(REFUSED));
        }
        for pad in [
            input(false, false, true),
            input(true, false, true),
            input(false, true, true),
            InputId::default(),
        ] {
            assert!(!pad.desk(), "{pad:?}");
        }
    }

    /// A lent pad's node goes as its record says: to the live holder unless it is a keyboard or
    /// mouse node, to nobody while parked. A device no record names, a seat's USB/IP pad, a
    /// virtual device and records that can't be trusted change nothing.
    #[test]
    fn a_lent_pads_nodes_go_as_its_record_says() {
        let desk = lent::fake::Desk::new();
        let xhci = "pci0000:00/0000:00:14.0/usb3";
        let ids = (0x28de, 0x1102);
        let (keys, pointer, pad) = ([0x03, 0x01, 0x01], [0x03, 0x01, 0x02], [0x03, 0x00, 0x00]);
        let lizard = [(keys, false), (pointer, false), (pad, true)];
        let given = desk.plug(&format!("{xhci}/3-1"), ids, Some("SC1"), &lizard);
        let parked = desk.plug(&format!("{xhci}/3-2"), ids, None, &lizard);
        let keyboard = desk.plug(
            &format!("{xhci}/3-3"),
            (0x046d, 0xc31c),
            None,
            &[(keys, false)],
        );
        let usbip = desk.plug("platform/vhci_hcd.0/usb11/11-1", ids, Some("SC1"), &lizard);
        let records = desk.records();
        std::fs::create_dir_all(records).unwrap();
        std::fs::write(records.join("usb-28de-1102-s-SC1"), "pf-seat-1\n").unwrap();
        std::fs::write(records.join("usb-28de-1102-p-0000_00_14.0-2"), "-\n").unwrap();
        let none = InputId::default();
        let input = |keyboard, mouse, joystick| InputId {
            keyboard,
            mouse,
            joystick,
        };
        let cases = [
            (
                &given.hidraw[2],
                none,
                Some("pf-seat-1"),
                "its pad's hidraw",
            ),
            (
                &given.event[2],
                input(false, false, true),
                Some("pf-seat-1"),
                "its joystick",
            ),
            (
                &given.event[2],
                input(true, false, true),
                Some("pf-seat-1"),
                "a joystick with keys",
            ),
            (&given.dev, none, None, "the device's own raw USB node"),
            (&given.hidraw[0], none, None, "a boot keyboard's hidraw"),
            (
                &given.event[1],
                input(false, true, false),
                None,
                "the lizard pointer",
            ),
            (
                &given.event[2],
                input(true, false, false),
                None,
                "keys on a pad interface",
            ),
            (&parked.hidraw[2], none, None, "parked, named by its port"),
        ];
        for (devpath, input, want, what) in cases {
            let want = want.map_or(Verdict::Refused, seat);
            assert_eq!(
                judge_lent(&desk.sys(), Some(records), devpath, input),
                want,
                "{what}"
            );
        }
        let nobody = [
            (&keyboard.event[0], Some(records), "the desk's keyboard"),
            (&usbip.hidraw[2], Some(records), "a seat's USB/IP pad"),
            (
                &given.hidraw[2],
                lent::trusted(records).then_some(records),
                "untrusted records",
            ),
        ];
        for (devpath, records, what) in nobody {
            let verdict = judge_lent(&desk.sys(), records, devpath, none);
            assert_eq!(verdict, Verdict::Nobody, "{what}");
        }
        let uhid = format!(
            "/{}/0003:28DE:1102.0001/hidraw/hidraw1",
            made_devices::UHID_DEVICES
        );
        assert_eq!(
            judge_lent(&desk.sys(), Some(records), &uhid, none),
            Verdict::Nobody
        );
    }

    /// The shipped rule runs the helper on USB nodes only while lend records exist, and makes a
    /// refused node of every kind a seat's line hands over root's.
    #[test]
    fn the_shipped_rule_gates_on_the_lend_records() {
        let rules = include_str!("../../../../../packaging/linux/65-punktfunk-seats.rules");
        assert!(rules.contains(&format!("TEST==\"{}\"", lent::DIR)));
        assert!(rules
            .contains("SUBSYSTEM==\"input|hidraw|usb|sound|tty\", ENV{PF_SEAT_REFUSED}==\"1\""));
    }

    #[test]
    fn the_rule_reads_three_lines() {
        assert_eq!(
            lines("pf-seat-1", 987),
            "PF_SEAT_USER=pf-seat-1\nPF_SEAT_UID=987\nPF_SEAT=seat-punktfunk-pf-seat-1\n"
        );
    }
}
