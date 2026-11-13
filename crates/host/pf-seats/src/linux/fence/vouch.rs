//! The supervisor's record of each seat device it makes, in [`DIR`], root's alone. The fence
//! hands a stamped node to a seat only on a record here: anyone who can open `/dev/uinput` or
//! `/dev/uhid` can write the stamp itself.
//!
//! - `@<inputN>`: a uinput device the supervisor made, by the name its own fd gives. The kernel
//!   numbers input devices once, so the name never names another device.
//! - `<token>`: a uhid pad about to be made, whose stamp carries the token. The first HID device
//!   that carries it binds it, `<token>.bound`, a link to the device's name; only that device
//!   passes afterwards.
//!
//! Each file holds the seat's account. A [`Vouch`] removes its files when it drops, and
//! [`prune`] removes what a stopped supervisor left.

use std::io::{self, ErrorKind, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

pub const DIR: &str = "/run/punktfunk/pads/vouched";
/// Where uinput and uhid put the devices they make, under sysfs.
pub const UINPUT_DEVICES: &str = "devices/virtual/input";
pub const UHID_DEVICES: &str = "devices/virtual/misc/uhid";
/// 22 characters of 6 random bits: 132 bits, and short enough for uhid's 64-byte `phys`.
const TOKEN_LEN: usize = 22;
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// What the supervisor holds for one device it makes for a seat; dropping it withdraws the record.
pub struct Vouch {
    dir: PathBuf,
    account: String,
    token: Option<String>,
    instance: Option<String>,
}

impl Vouch {
    /// For a device `account` is about to get. A uhid pad (`token`) is recorded now, under a
    /// fresh token its stamp carries; a uinput device once [`made`](Self::made) names it.
    pub fn new(dir: &Path, account: &str, token: bool) -> io::Result<Vouch> {
        let mut vouch = Vouch {
            dir: dir.to_owned(),
            account: account.to_owned(),
            token: None,
            instance: None,
        };
        if token {
            let minted = mint();
            put(dir, &minted, account)?;
            vouch.token = Some(minted);
        }
        Ok(vouch)
    }

    /// What follows the stamp's prefix: the token, else the account.
    pub fn mark(&self) -> &str {
        self.token.as_deref().unwrap_or(&self.account)
    }

    /// Records the uinput device made as `instance`, the name its own fd gives, and has udev
    /// apply its rules to the device's nodes again: the fence may have run before the record.
    pub fn made(&mut self, instance: &str) -> io::Result<()> {
        if !is_input(instance) {
            return Err(io::Error::other(format!("{instance:?} is no input device")));
        }
        put(&self.dir, &format!("@{instance}"), &self.account)?;
        self.instance = Some(instance.to_owned());
        refresh(&Path::new("/sys").join(UINPUT_DEVICES).join(instance));
        Ok(())
    }
}

impl Drop for Vouch {
    fn drop(&mut self) {
        let token = self
            .token
            .iter()
            .flat_map(|t| [t.clone(), format!("{t}.bound")]);
        let instance = self.instance.iter().map(|i| format!("@{i}"));
        for name in token.chain(instance) {
            let _ = std::fs::remove_file(self.dir.join(name));
        }
    }
}

fn mint() -> String {
    use rand::Rng as _;
    let mut bytes = [0u8; TOKEN_LEN];
    rand::rng().fill_bytes(&mut bytes);
    bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(b & 63)]))
        .collect()
}

pub(super) fn is_token(text: &str) -> bool {
    text.len() == TOKEN_LEN && text.bytes().all(|b| ALPHABET.contains(&b))
}

/// `inputN`, the name the kernel gives an input device.
pub(super) fn is_input(name: &str) -> bool {
    name.strip_prefix("input")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The number the kernel gave a HID device, `.0014` in `0003:054C:0CE6.0014`.
pub(super) fn hid_number(name: &str) -> Option<u64> {
    let (ids, number) = name.rsplit_once('.')?;
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
    if ids.split(':').count() != 3 || !ids.split(':').all(hex) || !hex(number) {
        return None;
    }
    u64::from_str_radix(number, 16).ok()
}

/// Makes `dir`, root's `0700`, in a parent that stays as it is.
fn open_dir(dir: &Path) -> io::Result<()> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(error) = std::fs::DirBuilder::new().mode(0o700).create(dir)
        && error.kind() != ErrorKind::AlreadyExists
    {
        return Err(error);
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Writes `account` as `dir/name`, `0600`, whole: a temporary file renamed into place.
fn put(dir: &Path, name: &str, account: &str) -> io::Result<()> {
    open_dir(dir)?;
    let temp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(format!("{account}\n").as_bytes())?;
    std::fs::rename(&temp, dir.join(name))
}

/// The account `dir/name` holds.
pub(super) fn account(dir: &Path, name: &str) -> Option<String> {
    super::plain(std::fs::read_to_string(dir.join(name)).ok()?.trim())
}

/// The HID device `token` is bound to, `None` while none is.
pub(super) fn bound(dir: &Path, token: &str) -> io::Result<Option<String>> {
    match std::fs::read_link(dir.join(format!("{token}.bound"))) {
        Ok(target) => target
            .into_os_string()
            .into_string()
            .map(Some)
            .map_err(|_| ErrorKind::InvalidData.into()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Binds `token` to the HID device `hid`. When something bound it first: whether that is `hid`.
pub(super) fn bind(dir: &Path, token: &str, hid: &str) -> io::Result<bool> {
    match std::os::unix::fs::symlink(hid, dir.join(format!("{token}.bound"))) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            Ok(bound(dir, token)?.as_deref() == Some(hid))
        }
        Err(error) => Err(error),
    }
}

/// [`DIR`], when it is a directory root's alone: the fence trusts no record anywhere else.
pub fn trusted() -> Option<&'static Path> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::symlink_metadata(DIR).ok()?;
    (meta.is_dir() && meta.uid() == 0 && meta.mode() & 0o077 == 0).then(|| Path::new(DIR))
}

/// Removes every record no live device holds: one for a uinput device that is gone, a token
/// whose HID device is gone, and a token that never bound. At the supervisor's start only,
/// before it makes a uhid pad, whose token waits unbound for the fence.
pub fn prune(dir: &Path, sys: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let live_token = |token: &str| {
        is_token(token)
            && bound(dir, token).ok().flatten().is_some_and(|hid| {
                hid_number(&hid).is_some() && sys.join(UHID_DEVICES).join(hid).exists()
            })
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let keep = name
            .to_str()
            .is_some_and(|name| match name.strip_prefix('@') {
                Some(input) => is_input(input) && sys.join(UINPUT_DEVICES).join(input).exists(),
                None => live_token(name.strip_suffix(".bound").unwrap_or(name)),
            });
        if !keep {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Has udev apply its rules again to each `event*` and `js*` node of the input device at `dev`.
fn refresh(dev: &Path) {
    let Ok(entries) = std::fs::read_dir(dev) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let node = name.to_string_lossy();
        if (node.starts_with("event") || node.starts_with("js"))
            && let Err(error) = std::fs::write(entry.path().join("uevent"), "change")
        {
            tracing::debug!(%error, %node, "seat pad node not refreshed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt as _;

    /// A uhid pad's record stands before the pad exists, root's alone, under a token that fits
    /// its stamp; dropping the vouch takes it and its bind back.
    #[test]
    fn a_uhid_pads_record_is_written_first_and_withdrawn_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("pads/vouched");
        let vouch = Vouch::new(&dir, "pf-seat-1", true).unwrap();
        let token = vouch.mark().to_owned();
        assert!(is_token(&token), "{token}");
        let longest = format!("punktfunk-seat:{token}/dualsense-edge/255");
        assert!(longest.len() < 64, "uhid's phys holds {longest}");
        assert_eq!(account(&dir, &token).as_deref(), Some("pf-seat-1"));
        let mode = |path: &Path| std::fs::metadata(path).unwrap().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(&token)), 0o600);
        assert!(bind(&dir, &token, "0003:054C:0CE6.0014").unwrap());
        assert_ne!(Vouch::new(&dir, "pf-seat-1", true).unwrap().mark(), token);
        drop(vouch);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    /// A uinput device's record is its kernel name, written once it exists. Without a token
    /// the stamp keeps the account.
    #[test]
    fn a_uinput_devices_record_is_its_kernel_name() {
        let temp = tempfile::tempdir().unwrap();
        let mut vouch = Vouch::new(temp.path(), "pf-seat-2", false).unwrap();
        assert_eq!(vouch.mark(), "pf-seat-2");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
        assert!(vouch.made("event3").is_err());
        assert!(vouch.made("input../x").is_err());
        vouch.made("input987654").unwrap();
        assert_eq!(
            account(temp.path(), "@input987654").as_deref(),
            Some("pf-seat-2")
        );
        drop(vouch);
        assert_eq!(account(temp.path(), "@input987654"), None);
    }

    /// The first bind wins; a second asks whether it named the same device.
    #[test]
    fn a_token_binds_once() {
        let temp = tempfile::tempdir().unwrap();
        let token = mint();
        assert_eq!(bound(temp.path(), &token).unwrap(), None);
        assert!(bind(temp.path(), &token, "0003:054C:0CE6.0014").unwrap());
        assert!(bind(temp.path(), &token, "0003:054C:0CE6.0014").unwrap());
        assert!(!bind(temp.path(), &token, "0003:054C:0CE6.0015").unwrap());
        assert_eq!(
            bound(temp.path(), &token).unwrap().as_deref(),
            Some("0003:054C:0CE6.0014")
        );
    }

    #[test]
    fn device_names_parse_as_the_kernel_writes_them() {
        assert_eq!(hid_number("0003:054C:0CE6.0014"), Some(0x14));
        assert_eq!(hid_number("0003:28DE:1205.1000A"), Some(0x1000a));
        assert_eq!(hid_number("0003:054C.0014"), None);
        assert_eq!(hid_number("power"), None);
        assert_eq!(hid_number("0003:054C:0CE6."), None);
        assert!(is_input("input42"));
        assert!(!is_input("input") && !is_input("inputx") && !is_input("js0"));
    }

    /// At start, records of devices that are gone, and tokens that never bound, go; a live
    /// device's stay.
    #[test]
    fn prune_keeps_only_what_a_live_device_holds() {
        let temp = tempfile::tempdir().unwrap();
        let (dir, sys) = (temp.path().join("vouched"), temp.path().join("sys"));
        std::fs::create_dir_all(sys.join(UINPUT_DEVICES).join("input5")).unwrap();
        std::fs::create_dir_all(sys.join(UHID_DEVICES).join("0003:054C:0CE6.0002")).unwrap();
        put(&dir, "@input5", "pf-seat-1").unwrap();
        put(&dir, "@input6", "pf-seat-1").unwrap();
        let (live, gone, unbound) = (mint(), mint(), mint());
        for token in [&live, &gone, &unbound] {
            put(&dir, token, "pf-seat-1").unwrap();
        }
        bind(&dir, &live, "0003:054C:0CE6.0002").unwrap();
        bind(&dir, &gone, "0003:054C:0CE6.0001").unwrap();
        std::fs::write(dir.join("junk"), "x").unwrap();
        prune(&dir, &sys);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let mut want = vec!["@input5".to_owned(), live.clone(), format!("{live}.bound")];
        want.sort();
        assert_eq!(left, want);
    }
}
