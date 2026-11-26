//! A seat's trust copy, and the copy of a seat host's certificate.
//!
//! `trust/<id>/` is a read-only copy of the box's pairing store and profiles from the door's
//! directory. Root owns it where the filesystem has ACLs, and the seat's user reads what the
//! seat needs and nothing else of the door's ([`Reader`]). It never holds the box's identity: a
//! game on the seat could otherwise answer as the box.
//!
//! - A seat on a person's own account gets only the pairing records of the devices its row
//!   lets on. So does the seat of a locked profile, with the lock and without the PIN.
//! - The door owns the box's files and an account may own its copy, so both sides are read and
//!   written through directory handles ([`sync_file`], [`put_file`]) and no link is followed.
//! - [`copy_pin`] keeps root's copy of the certificate a seat host serves.
//!
//! The box library's files that a seat finds in its trust copy are copied by
//! [`super::library_copy`].

use super::acl::{reader_acl, set_acl, ACL_ACCESS, ACL_START, ACL_USER};
use super::layout::{box_library_dir, io_err, make_dir, seat_dir, trust_dir};
use super::library_copy::{lock_holder, open_source, sync_flat_dir, OWNER_POLICY};
use super::safe_open;
use crate::backend::BackendError;
use crate::model::SeatId;
use crate::profile_locks;
use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use std::collections::BTreeSet;
use std::io::{Read as _, Write as _};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

/// The box's pairing list: who a seat host admits. A person's seat gets it filtered.
const PAIRED: &str = "punktfunk1-paired.json";

/// Box files a seat host reads; any may not exist yet. Never the box's identity: a seat host
/// mints its own, and clients reach it through the pin the box's `Redirect` carries.
pub(super) const TRUST_FILES: [&str; 4] = [
    PAIRED,
    "profiles.json",
    "display-settings.json",
    // `pf_host_config::seat_defaults::FILE`: the Seat defaults a seat applies itself.
    "seat-defaults.json",
];

/// Profile avatars, one flat directory beside `profiles.json`.
pub(super) const AVATARS: &str = "profiles";

/// A trust file larger than this is not ours.
pub(super) const MAX_TRUST_BYTES: u64 = 8 * 1024 * 1024;

/// Whether a copy is stale: absent, or a different size or modification time. Copies keep the
/// source's mtime, so an unchanged file compares equal.
fn needs_copy(src: Stamp, dst: Option<Stamp>) -> bool {
    dst != Some(src)
}

/// Modification time in seconds and nanoseconds, then size.
type Stamp = (i64, i64, u64);

// The field types differ by target: a cast that is a no-op on one is needed on another.
#[allow(clippy::unnecessary_cast)]
fn stamp(st: &rustix::fs::Stat) -> Stamp {
    (
        st.st_mtime as i64,
        st.st_mtime_nsec as i64,
        st.st_size as u64,
    )
}

/// Where the supervisor keeps its copy of a seat host's certificate.
pub(super) fn pin_dir(state: &Path, id: &SeatId) -> PathBuf {
    seat_dir(state, id).join("pin")
}

/// Copies the certificate the host in `home` serves into `dst`, root's alone, or removes the copy
/// while the host has none. `home` is a seat user's or the owner's, so it goes through
/// [`safe_open`].
pub(super) fn copy_pin(home: &Path, dst: &Path) -> std::io::Result<()> {
    copy_cert(home, &[".config", "punktfunk"], dst)
}

/// [`copy_pin`] for a person's row, whose host keeps its identity in the row's own
/// [`super::layout::person_config_dir`], not in their home.
pub(super) fn copy_person_pin(state: &Path, id: &SeatId, dst: &Path) -> std::io::Result<()> {
    copy_cert(&seat_dir(state, id), &["config"], dst)
}

/// The certificate in `base`/`below`, as [`copy_pin`] copies it. The directories on the way are
/// the user's, so each opens without following a link.
fn copy_cert(base: &Path, below: &[&str], dst: &Path) -> std::io::Result<()> {
    const CERT: &str = "native-cert.pem";
    let config = safe_open::walk(safe_open::dir(base)?, below)?;
    let found = match config {
        Some(at) => safe_open::read_at(at, CERT, 64 * 1024)?,
        None => None,
    };
    let Some((bytes, _)) = found else {
        return match std::fs::remove_file(dst.join(CERT)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    };
    let text = String::from_utf8(bytes).map_err(|_| std::io::ErrorKind::InvalidData)?;
    make_dir(dst, 0o700)?;
    super::session::write_if_changed(&dst.join(CERT), &text, 0o600).map(drop)
}

/// Who reads a copy, and so who owns each of its files and folders.
///
/// - [`Reader::group`]: a made seat's user, through the seat's group. Root owns each file and
///   folder as `root:gid`, files `0640`, folders `0750`.
/// - [`Reader::person`]: the owner's account or a person's own account, whose primary group
///   others may share. The account owns each file and folder as `uid:gid`, files `0600`, folders
///   `0700`. Files root writes into a made seat's home take this reader too.
/// - [`open_copy`] turns a person reader into one that reads through an ACL entry for its uid.
///   Root owns each file and folder, and the group gets nothing, so the account can neither
///   create nor rename anything in its copy. A filesystem without ACLs keeps the person reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reader {
    uid: Option<u32>,
    /// The group of a file the account owns. It grants nothing while `acl` is set.
    gid: u32,
    /// Root owns the copy, and `uid` reads it through an ACL entry.
    acl: bool,
}

impl Reader {
    pub(super) fn group(gid: u32) -> Self {
        Self {
            uid: None,
            gid,
            acl: false,
        }
    }

    pub(super) fn person(uid: u32, gid: u32) -> Self {
        Self {
            uid: Some(uid),
            gid,
            acl: false,
        }
    }

    /// Whether the account owns the copy: a person reader without an ACL.
    fn owns(self) -> bool {
        self.uid.is_some() && !self.acl
    }

    /// The mode of a file in the copy. With an ACL it is the mode the ACL's entries give, so the
    /// `chmod` in [`Reader::hand`] leaves the ACL as it is.
    fn file_mode(self) -> u32 {
        if self.owns() {
            0o600
        } else {
            0o640
        }
    }

    /// The mode of a folder in the copy, picked as [`Reader::file_mode`] picks a file's.
    pub(super) fn dir_mode(self) -> u32 {
        if self.owns() {
            0o700
        } else {
            0o750
        }
    }

    /// Whether a file of the copy that `owner` owns is kept as it is. With an ACL only a file root
    /// owns is kept: a file the account owns is from an older copy, and is copied again.
    fn keeps(self, owner: u32) -> bool {
        !self.acl || owner == rustix::process::geteuid().as_raw()
    }

    /// Refuses the folder `fd`, found at `name` in a copy with an ACL, unless root or the account
    /// owns it. A folder the account owns is from an older copy. Any other folder was moved in
    /// from elsewhere, and root neither takes it over nor empties it.
    pub(super) fn claim(self, fd: &OwnedFd, name: &str) -> std::io::Result<()> {
        let owner = rustix::fs::fstat(fd)?.st_uid;
        if !self.acl || owner == rustix::process::geteuid().as_raw() || Some(owner) == self.uid {
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{name} belongs to uid {owner}, not to root or the copy's reader"),
        ))
    }

    /// Hands `fd`, a file or a folder (`dir`) of the copy, to this reader: the ACL first, then
    /// the mode, then the owner. A folder the account owns from an older copy stays readable to
    /// it at each step.
    pub(super) fn hand(self, fd: impl AsFd, dir: bool) -> std::io::Result<()> {
        let mut owner = (self.uid, Some(self.gid));
        if let (true, Some(uid)) = (self.acl, self.uid) {
            set_acl(&fd, &reader_acl(uid, dir))?;
            let root = rustix::process::geteuid().as_raw();
            owner = (Some(root), Some(rustix::process::getegid().as_raw()));
        }
        let mode = if dir {
            self.dir_mode()
        } else {
            self.file_mode()
        };
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode as _))?;
        std::os::unix::fs::fchown(fd, owner.0, owner.1)
    }
}

/// The reader `dst`, a person's trust copy, was made for: the account its ACL names while root
/// owns it, else the account that owns it.
fn copy_reader(dst: &OwnedFd) -> std::io::Result<Reader> {
    let stat = rustix::fs::fstat(dst)?;
    let owned = Reader::person(stat.st_uid, stat.st_gid);
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Ok(owned);
    }
    let mut buf = [0u8; ACL_START];
    let named = rustix::fs::fgetxattr(dst, ACL_ACCESS, &mut buf[..])
        .ok()
        .and_then(|len| {
            let user = (buf[..len].get(4..)?.chunks_exact(8))
                .find(|e| u16::from_le_bytes([e[0], e[1]]) == ACL_USER)?;
            Some(u32::from_le_bytes([user[4], user[5], user[6], user[7]]))
        });
    Ok(named.map_or(owned, |uid| Reader {
        uid: Some(uid),
        acl: true,
        ..owned
    }))
}

/// Removes `name` from `dir`; `true` when there was one.
pub(super) fn unlink(dir: &OwnedFd, name: &str) -> std::io::Result<bool> {
    match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) => Ok(true),
        Err(e) if e == rustix::io::Errno::NOENT => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Copies `name`, at most `max` bytes, from `src` into `dst` for `reader` when it differs or
/// `reader` doesn't keep the copy's owner ([`Reader::keeps`]), and removes it from `dst` when
/// `src` has none. Returns whether `dst` changed. Both are directory handles, so no link on
/// either side is followed, whoever owns the directory. An unchanged file is not read; a changed
/// one is streamed, never held whole.
pub(super) fn sync_file(
    src: Option<&OwnedFd>,
    dst: &OwnedFd,
    name: &str,
    reader: Reader,
    max: u64,
) -> std::io::Result<bool> {
    let found = match src {
        Some(src) => safe_open::file_at(src, name, max)?,
        None => None,
    };
    let Some((file, meta)) = found else {
        return unlink(dst, name);
    };
    let wanted = (meta.mtime(), meta.mtime_nsec(), meta.size());
    let current = rustix::fs::statat(dst, name, AtFlags::SYMLINK_NOFOLLOW)
        .ok()
        .filter(|st| reader.keeps(st.st_uid))
        .map(|st| stamp(&st));
    if !needs_copy(wanted, current) {
        return Ok(false);
    }
    let temp = format!("{name}.tmp");
    unlink(dst, &temp)?;
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mode = Mode::from_raw_mode(reader.file_mode() as _);
    let mut out = std::fs::File::from(rustix::fs::openat(dst, temp.as_str(), create, mode)?);
    if std::io::copy(&mut (&file).take(max + 1), &mut out)? > max {
        drop(out);
        unlink(dst, &temp)?;
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{name} grew beyond {max} bytes"),
        ));
    }
    reader.hand(&out, false)?;
    out.set_modified(meta.modified()?)?;
    drop(out);
    rustix::fs::renameat(dst, temp.as_str(), dst, name)?;
    Ok(true)
}

/// Brings a made seat's trust copy level with the box's files in `box_dir`, without the box's
/// identity: a game on the seat could otherwise answer as the box. Returns whether anything
/// changed. The door owns `box_dir` and an account may own its copy ([`Reader`]), so both are
/// read and written through directory handles. A locked profile's seat gets the pairing list its
/// lock admits and the lock without its PIN ([`lock_filter`]).
///
/// A file that can't be copied keeps its last good copy and the rest still follow; the first
/// failure is the answer. A box directory that isn't there, as in a reinstall, leaves every copy
/// as it was.
pub(super) fn refresh_trust(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    reader: Reader,
) -> Result<bool, BackendError> {
    sync_trust(box_dir, state, id, reader, Who::Seat { owner: false })
}

/// [`refresh_trust`] for the owner's row, which follows the owner's lock: every profile that
/// shares the owner's desktop opens with it.
pub(super) fn refresh_owner_trust(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    reader: Reader,
) -> Result<bool, BackendError> {
    sync_trust(box_dir, state, id, reader, Who::Seat { owner: true })
}

/// [`refresh_trust`] for a seat on a person's own account, whose pairing list holds only the
/// box's records of the devices in `admitted` ([`write_admitted`]).
pub(super) fn refresh_person_trust(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    reader: Reader,
    admitted: &BTreeSet<String>,
) -> Result<bool, BackendError> {
    sync_trust(box_dir, state, id, reader, Who::Person(admitted))
}

/// Whose trust copy [`sync_trust`] keeps: a person's own account's, admitting these devices, or
/// another seat's, the owner's row included, which its profile's lock decides.
#[derive(Clone, Copy)]
enum Who<'a> {
    Person(&'a BTreeSet<String>),
    Seat { owner: bool },
}

fn sync_trust(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    reader: Reader,
    who: Who<'_>,
) -> Result<bool, BackendError> {
    let io = |what: &str, e| io_err("trust_copy", what, e);
    let (dst, reader) =
        open_copy(&trust_dir(state, id), reader).map_err(|e| io("open the trust copy", e))?;
    let (mut changed, mut first) = (false, None);
    let mut note = |copied: std::io::Result<bool>, what: &str| match copied {
        Ok(did) => changed |= did,
        Err(e) => {
            first.get_or_insert_with(|| io(what, e));
        }
    };
    let admitted = match who {
        Who::Person(admitted) => Some(admitted.clone()),
        Who::Seat { owner } => {
            let (admitted, copy) = lock_filter(box_dir, id, owner, super::presence::unix_now());
            let written = match copy {
                Some(copy) => serde_json::to_vec_pretty(&copy)
                    .map_err(std::io::Error::other)
                    .and_then(|bytes| put_file(&dst, profile_locks::FILE, &bytes, reader)),
                None => unlink(&dst, profile_locks::FILE),
            };
            note(written, "write the profile locks");
            admitted
        }
    };
    let admitted = admitted.as_ref();
    if let Some(admitted) = admitted {
        note(
            write_admitted(box_dir, &dst, reader, admitted),
            "write the pairing list",
        );
    }
    let src = open_source(box_dir).map_err(|e| io("open the box directory", e))?;
    if let Some(src) = src {
        // The display policy is the owner's, by way of the library copy, not the door's.
        let owner =
            open_source(&box_library_dir(state)).map_err(|e| io("open the box library", e))?;
        for name in TRUST_FILES
            .into_iter()
            .filter(|&n| admitted.is_none() || n != PAIRED)
        {
            let from = if name == OWNER_POLICY {
                owner.as_ref()
            } else {
                Some(&src)
            };
            let copied = sync_file(from, &dst, name, reader, MAX_TRUST_BYTES);
            note(copied, &format!("copy {name}"));
        }
        let avatars = sync_flat_dir(Some(&src), &dst, AVATARS, reader, MAX_TRUST_BYTES);
        note(avatars, "copy the avatars");
    }
    first.map_or(Ok(changed), Err)
}

/// The pairing list of a person's seat in `dst`, for `reader`: each record of the box's
/// [`PAIRED`] whose fingerprint is in `admitted`, kept as it is. The box's file is the door's,
/// so one that is missing, refused by [`safe_open`], over [`MAX_TRUST_BYTES`] or not
/// `{"clients": [...]}` holds no record. Written only when it differs; `true` when it did.
fn write_admitted(
    box_dir: &Path,
    dst: &OwnedFd,
    reader: Reader,
    admitted: &BTreeSet<String>,
) -> std::io::Result<bool> {
    #[derive(serde::Deserialize)]
    struct Paired {
        clients: Vec<serde_json::Value>,
    }
    let admits = |record: &serde_json::Value| {
        let fingerprint = record.get("fingerprint").and_then(|f| f.as_str());
        fingerprint.is_some_and(|f| admitted.contains(&f.to_ascii_lowercase()))
    };
    let read = || -> Option<Vec<serde_json::Value>> {
        let (bytes, _) = safe_open::read(&box_dir.join(PAIRED), MAX_TRUST_BYTES).ok()??;
        let paired: Paired = serde_json::from_slice(&bytes).ok()?;
        Some(paired.clients.into_iter().filter(admits).collect())
    };
    let clients = if admitted.is_empty() {
        Vec::new()
    } else {
        read().unwrap_or_default()
    };
    let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({ "clients": clients }))?;
    bytes.push(b'\n');
    put_file(dst, PAIRED, &bytes, reader)
}

/// [`write_admitted`] into the trust copy `id` has, for the reader it was made for
/// ([`copy_reader`]). A seat that never started has none; its start writes one.
pub(super) fn rewrite_admitted(
    box_dir: &Path,
    state: &Path,
    id: &SeatId,
    admitted: &BTreeSet<String>,
) -> Result<bool, BackendError> {
    let io = |what: &str, e| io_err("trust_copy", what, e);
    let dst = match safe_open::dir(&trust_dir(state, id)) {
        Ok(dst) => dst,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io("open the trust copy", e)),
    };
    let reader = copy_reader(&dst).map_err(|e| io("read the trust copy's owner", e))?;
    write_admitted(box_dir, &dst, reader, admitted).map_err(|e| io("write the pairing list", e))
}

/// `bytes` as `name` in `dst` for `reader`, through a new file renamed over the old, when the
/// file holds anything else or `reader` doesn't keep its owner ([`Reader::keeps`]). `true` when
/// it changed.
pub(super) fn put_file(
    dst: &OwnedFd,
    name: &str,
    bytes: &[u8],
    reader: Reader,
) -> std::io::Result<bool> {
    let current = safe_open::read_at(dst, name, MAX_TRUST_BYTES)
        .ok()
        .flatten();
    if current.is_some_and(|(held, meta)| held == bytes && reader.keeps(meta.uid())) {
        return Ok(false);
    }
    let temp = format!("{name}.tmp");
    unlink(dst, &temp)?;
    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mode = Mode::from_raw_mode(reader.file_mode() as _);
    let mut out = std::fs::File::from(rustix::fs::openat(dst, temp.as_str(), create, mode)?);
    out.write_all(bytes)?;
    reader.hand(&out, false)?;
    drop(out);
    rustix::fs::renameat(dst, temp.as_str(), dst, name)?;
    Ok(true)
}

/// What the door's lock file ([`profile_locks::FILE`]) means for seat `id` at `now`. Two values:
///
/// - the devices that the seat's pairing list lets on, or `None` for every paired device
/// - the copy of the lock that the seat's host reads, without any PIN, or `None` for no lock
///
/// A lock file that exists and can't be read lets nobody on. So does a `profiles.json` that
/// can't be read while anything is locked.
fn lock_filter(
    box_dir: &Path,
    id: &SeatId,
    owner: bool,
    now: i64,
) -> (Option<BTreeSet<String>>, Option<profile_locks::Locks>) {
    let read = match safe_open::read(&box_dir.join(profile_locks::FILE), MAX_TRUST_BYTES) {
        Ok(None) => return (None, None),
        Ok(Some((bytes, _))) => profile_locks::Locks::parse(&bytes),
        Err(_) => None,
    };
    let closed = profile_locks::Locks {
        unreadable: true,
        ..profile_locks::Locks::default()
    };
    let Some(read) = read else {
        return (Some(BTreeSet::new()), Some(closed));
    };
    match lock_holder(box_dir, id, owner) {
        Some(holder) => {
            let served: Vec<String> = holder.into_iter().collect();
            (
                read.admitted_for(&served, now),
                Some(read.seat_copy(&served)),
            )
        }
        None if read.unreadable || read.profiles.values().any(|l| l.locked) => {
            (Some(BTreeSet::new()), Some(closed))
        }
        None => (None, None),
    }
}

/// `dst` made and opened, then handed to `reader`: a copy root writes for one reader. Returns the
/// reader each file and folder in it is made for, which for a person reader reads through an
/// ACL. A filesystem that refuses ACLs keeps the person reader, logged once per start.
pub(super) fn open_copy(dst: &Path, reader: Reader) -> std::io::Result<(OwnedFd, Reader)> {
    std::fs::create_dir_all(dst)?;
    let copy = safe_open::dir(dst)?;
    let acl = Reader {
        acl: reader.uid.is_some(),
        ..reader
    };
    let no_acl = |e: &std::io::Error| {
        let errno = Errno::from_io_error(e);
        errno.is_some_and(|e| [Errno::NOTSUP, Errno::OPNOTSUPP].contains(&e))
    };
    match acl.hand(&copy, true) {
        Ok(()) => return Ok((copy, acl)),
        Err(error) if acl.acl && no_acl(&error) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(copy = %dst.display(), %error, "trust copies owned by their accounts");
            });
        }
        Err(e) => return Err(e),
    }
    reader.hand(&copy, true)?;
    Ok((copy, reader))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::linux::acl::tests::REFUSE_ACL;
    use crate::linux::layout::ensure_layout;
    use crate::linux::library_copy::tests::{profiles_on, ANNA, OTHER_SEAT};
    use rustix::fs::XattrFlags;
    use std::os::unix::fs::PermissionsExt as _;

    /// Whether the filesystem under `dir` keeps an access ACL. Off one, the account owns a
    /// person's copy, as [`Reader::person`] says.
    pub(in crate::linux) fn keeps_acls(dir: &Path) -> bool {
        let probe = dir.join("acl-probe");
        std::fs::write(&probe, "").unwrap();
        let acl = reader_acl(rustix::process::geteuid().as_raw(), false);
        let kept = rustix::fs::setxattr(&probe, ACL_ACCESS, &acl, XattrFlags::empty()).is_ok();
        std::fs::remove_file(probe).unwrap();
        kept
    }

    /// The access ACL of `path`, empty when it has none.
    fn acl_of(path: &Path) -> Vec<u8> {
        let mut buf = [0u8; 256];
        let len = rustix::fs::getxattr(path, ACL_ACCESS, &mut buf[..]).unwrap_or(0);
        buf[..len].to_vec()
    }

    pub(in crate::linux) fn id() -> SeatId {
        SeatId::parse("0123456789abcdef0123456789abcdef").unwrap()
    }

    pub(in crate::linux) fn own_gid(dir: &Path) -> u32 {
        std::fs::metadata(dir).unwrap().gid()
    }

    pub(in crate::linux) fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn box_with_identity(dir: &Path) {
        std::fs::write(dir.join("native-cert.pem"), "cert").unwrap();
        std::fs::write(dir.join("native-key.pem"), "key").unwrap();
    }

    #[test]
    fn a_copy_is_made_only_for_a_missing_or_different_file() {
        assert!(needs_copy((10, 0, 5), None));
        assert!(needs_copy((10, 0, 5), Some((10, 0, 6))));
        assert!(needs_copy((10, 0, 5), Some((11, 0, 5))));
        assert!(needs_copy((10, 0, 5), Some((10, 1, 5))));
        assert!(!needs_copy((10, 0, 5), Some((10, 0, 5))));
    }

    #[test]
    fn the_trust_copy_follows_the_box_and_is_group_readable_only() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        let gid = own_gid(box_dir);
        box_with_identity(box_dir);
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        std::fs::create_dir(box_dir.join("profiles")).unwrap();
        std::fs::write(box_dir.join("profiles/p1.png"), "png").unwrap();
        std::fs::write(box_dir.join("mgmt-token"), "secret").unwrap();
        let trust = trust_dir(state, &id());

        assert!(refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap());
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles/p1.png")).unwrap(),
            "png"
        );
        assert_eq!(mode(&trust), 0o750);
        assert_eq!(mode(&trust.join("profiles.json")), 0o640);
        for kept in ["mgmt-token", "native-key.pem", "native-cert.pem"] {
            assert!(!trust.join(kept).exists(), "{kept} stays the box's");
        }
        assert!(
            !refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap(),
            "unchanged is a no-op"
        );

        // A changed file, a new avatar and a removed avatar all follow.
        std::fs::write(box_dir.join("profiles.json"), "{\"a\":1}").unwrap();
        std::fs::write(box_dir.join("profiles/p2.png"), "png2").unwrap();
        std::fs::remove_file(box_dir.join("profiles/p1.png")).unwrap();
        assert!(refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap());
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles.json")).unwrap(),
            "{\"a\":1}"
        );
        assert!(trust.join("profiles/p2.png").exists());
        assert!(!trust.join("profiles/p1.png").exists());

        // A file the box drops is dropped from the copy.
        std::fs::remove_file(box_dir.join("profiles.json")).unwrap();
        assert!(refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap());
        assert!(!trust.join("profiles.json").exists());
    }

    /// Root owns the owner's copy, and only the account reads it, through an ACL entry for its
    /// uid. Every group is closed out: a person's primary group may be one everybody on the box
    /// shares.
    #[test]
    fn root_owns_the_owner_s_trust_copy_and_the_account_alone_reads_it() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        if !keeps_acls(state) {
            eprintln!("no POSIX ACLs under {}: skipped", state.display());
            return;
        }
        box_with_identity(box_dir);
        std::fs::create_dir(box_dir.join("profiles")).unwrap();
        std::fs::write(box_dir.join("profiles/a.png"), "png").unwrap();
        let me = std::fs::metadata(box_dir).unwrap();
        let reader = Reader::person(me.uid(), me.gid());
        assert!(refresh_trust(box_dir, state, &id(), reader).is_ok());
        let trust = trust_dir(state, &id());
        for (path, dir) in [
            (trust.clone(), true),
            (trust.join("profiles"), true),
            (trust.join("profiles/a.png"), false),
        ] {
            assert_eq!(mode(&path), if dir { 0o750 } else { 0o640 });
            assert_eq!(
                acl_of(&path),
                reader_acl(me.uid(), dir),
                "{}",
                path.display()
            );
        }
    }

    /// On a filesystem that refuses ACLs, the account owns a person's copy: folders `0700`, files
    /// `0600`, no ACL.
    #[test]
    fn without_acls_the_account_owns_a_person_s_copy() {
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        std::fs::create_dir_all(box_dir.join("profiles")).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        std::fs::write(box_dir.join("profiles/a.png"), "png").unwrap();
        let me = std::fs::metadata(&box_dir).unwrap();
        let reader = Reader::person(me.uid(), me.gid());
        REFUSE_ACL.set(true);
        let copied = refresh_person_trust(&box_dir, &state, &id(), reader, &BTreeSet::new());
        REFUSE_ACL.set(false);
        copied.unwrap();
        let trust = trust_dir(&state, &id());
        for (path, wanted) in [
            (trust.clone(), 0o700),
            (trust.join("profiles"), 0o700),
            (trust.join("profiles.json"), 0o600),
            (trust.join(PAIRED), 0o600),
            (trust.join("profiles/a.png"), 0o600),
        ] {
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!((meta.mode() & 0o7777, meta.uid()), (wanted, me.uid()));
            assert!(acl_of(&path).is_empty(), "{}", path.display());
        }
    }

    /// A person's copy that the account owns, as one made without ACLs, passes to root at the
    /// next refresh with them: each folder and file, the account reading through its ACL entry.
    /// Checked as each uid through `setpriv`: the account reads its copy and can't create or
    /// rename anything in it, and another uid can't read it. A folder another uid moved in at a
    /// known name is refused, and keeps its owner, mode and files. Needs root to hand files to
    /// other uids, Linux for ACLs that mean anything, and `setpriv` from util-linux.
    #[test]
    #[ignore = "needs root on Linux: hands the copy to other uids and reads its ACLs back"]
    fn an_account_s_copy_passes_to_root_and_a_planted_folder_is_left_alone() {
        // Ids no account has, below 65536: a container's user namespace maps only those.
        const READER: u32 = 59_940;
        const STRANGER: u32 = 59_941;
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        std::fs::create_dir_all(box_dir.join("profiles")).unwrap();
        // Both uids walk to the copy: the temp root opens to them, and `ensure_layout` makes
        // `state` and `state/trust` 0711, as on a box.
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_layout(&state).unwrap();
        assert!(
            keeps_acls(&state),
            "no POSIX ACLs under {}",
            state.display()
        );
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        std::fs::write(box_dir.join("profiles/a.png"), "png").unwrap();
        let paired = r#"{"clients":[{"fingerprint":"aa"}]}"#;
        std::fs::write(box_dir.join(PAIRED), paired).unwrap();
        let reader = Reader::person(READER, READER);
        let refresh = || refresh_person_trust(&box_dir, &state, &id(), reader, &BTreeSet::new());
        let trust = trust_dir(&state, &id());

        REFUSE_ACL.set(true);
        refresh().unwrap();
        REFUSE_ACL.set(false);
        assert_eq!(
            std::fs::metadata(&trust).unwrap().uid(),
            READER,
            "an older copy"
        );
        refresh().unwrap();
        let admitted = BTreeSet::from(["aa".to_owned()]);
        assert!(rewrite_admitted(&box_dir, &state, &id(), &admitted).unwrap());
        for (path, dir) in [
            (trust.clone(), true),
            (trust.join("profiles"), true),
            (trust.join("profiles.json"), false),
            (trust.join(PAIRED), false),
            (trust.join("profiles/a.png"), false),
        ] {
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let wanted = if dir { 0o750 } else { 0o640 };
            let at = path.display();
            assert_eq!(
                (meta.uid(), meta.gid(), meta.mode() & 0o7777),
                (0, 0, wanted),
                "{at}"
            );
            assert_eq!(acl_of(&path), reader_acl(READER, dir), "{at}");
        }

        let as_uid = |uid: u32, program: &str, args: &[&Path]| {
            let status = std::process::Command::new("setpriv")
                .args([&format!("--reuid={uid}"), &format!("--regid={uid}")])
                .args(["--clear-groups", "--", program])
                .args(args)
                .stdout(std::process::Stdio::null())
                .status();
            let status =
                status.unwrap_or_else(|e| panic!("setpriv (util-linux) did not start: {e}"));
            status.success()
        };
        let (paired_copy, profiles) = (trust.join(PAIRED), trust.join("profiles"));
        assert!(as_uid(READER, "cat", &[&paired_copy]), "the account reads");
        assert!(
            as_uid(READER, "cat", &[&profiles.join("a.png")]),
            "the account reads"
        );
        assert!(as_uid(READER, "ls", &[&profiles]), "the account lists");
        let planted_file = trust.join("planted");
        assert!(
            !as_uid(READER, "touch", &[&planted_file]),
            "the account creates"
        );
        let moved = trust.join("moved");
        assert!(
            !as_uid(READER, "mv", &[&profiles, &moved]),
            "the account renames"
        );
        assert!(
            !as_uid(STRANGER, "cat", &[&paired_copy]),
            "a stranger reads"
        );
        assert!(!as_uid(STRANGER, "ls", &[&trust]), "a stranger lists");

        let planted = trust.join("profiles");
        std::fs::remove_dir_all(&planted).unwrap();
        std::fs::create_dir(&planted).unwrap();
        std::fs::write(planted.join("theirs.png"), "theirs").unwrap();
        std::os::unix::fs::chown(&planted, Some(STRANGER), Some(STRANGER)).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(refresh().is_err());
        let meta = std::fs::symlink_metadata(&planted).unwrap();
        assert_eq!(
            (meta.uid(), meta.gid(), meta.mode() & 0o7777),
            (STRANGER, STRANGER, 0o755)
        );
        assert!(planted.join("theirs.png").exists());
        assert!(!planted.join("a.png").exists());
    }

    /// A locked profile's seat admits only the devices its lock does and finds the lock beside
    /// its pairing list, without the PIN; an open profile's keeps the box's whole list. The
    /// owner's row follows the owner's lock, which every profile sharing that desktop opens
    /// with. A lock file that doesn't read admits nobody.
    #[test]
    fn a_locked_profile_s_seat_admits_only_its_devices() {
        use crate::profile_locks::{Lock, Locks, PinHash};
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        let reader = Reader::group(own_gid(box_dir));
        let (tv, phone) = ("aa".repeat(32), "bb".repeat(32));
        let paired = serde_json::json!({"clients": [
            {"name": "TV", "fingerprint": tv}, {"name": "Phone", "fingerprint": phone}]});
        std::fs::write(box_dir.join(PAIRED), paired.to_string()).unwrap();
        profiles_on(box_dir, id().as_str(), OTHER_SEAT);
        let owner = SeatId::parse("abcdefabcdefabcdefabcdefabcdefab").unwrap();
        let admitted = |seat: &SeatId| -> Vec<String> {
            let text = std::fs::read_to_string(trust_dir(state, seat).join(PAIRED)).unwrap();
            let file: serde_json::Value = serde_json::from_str(&text).unwrap();
            let clients = file["clients"].as_array().unwrap().iter();
            clients
                .map(|c| c["fingerprint"].as_str().unwrap().to_string())
                .collect()
        };
        let copy = |seat: &SeatId| {
            let bytes = std::fs::read(trust_dir(state, seat).join(profile_locks::FILE)).ok()?;
            Some(Locks::parse(&bytes).unwrap())
        };
        let lock = |fp: &str| {
            let mut lock = Lock {
                locked: true,
                pin: PinHash::new("2468"),
                ..Lock::default()
            };
            let device = crate::ipc::Device {
                fingerprint: fp.to_string(),
                name: "TV".into(),
            };
            assert!(lock.admit(&device, super::super::presence::unix_now(), true));
            lock
        };
        let write = |locks: &Locks| {
            let bytes = serde_json::to_vec(locks).unwrap();
            std::fs::write(box_dir.join(profile_locks::FILE), bytes).unwrap();
        };

        refresh_trust(box_dir, state, &id(), reader).unwrap();
        assert_eq!(admitted(&id()), [tv.clone(), phone.clone()]);
        assert!(copy(&id()).is_none(), "no lock file, no copy");

        let mut file = Locks::default();
        file.profiles.insert(ANNA.into(), lock(&tv));
        file.profiles.insert("c3c3c3c3c3c3".into(), lock(&phone));
        write(&file);
        refresh_trust(box_dir, state, &id(), reader).unwrap();
        assert_eq!(admitted(&id()), std::slice::from_ref(&tv));
        let anna = copy(&id()).unwrap();
        assert_eq!(anna.profiles.keys().collect::<Vec<_>>(), [ANNA]);
        assert!(anna.profiles[ANNA].locked && anna.profiles[ANNA].pin.is_none());
        assert_eq!(
            mode(&trust_dir(state, &id()).join(profile_locks::FILE)),
            0o640
        );
        refresh_owner_trust(box_dir, state, &owner, reader).unwrap();
        let own = admitted(&owner);
        assert_eq!(own, std::slice::from_ref(&phone), "the owner's own list");

        // A profile sharing the owner's desktop has no lock of its own: the owner's decides.
        let mut shared: serde_json::Value =
            serde_json::from_slice(&std::fs::read(box_dir.join("profiles.json")).unwrap()).unwrap();
        let sharer = serde_json::json!({"id": "d4d4d4d4d4d4", "display_name": "Guest",
            "os_account": {"kind": "operator"}});
        shared["profiles"].as_array_mut().unwrap().push(sharer);
        std::fs::write(box_dir.join("profiles.json"), shared.to_string()).unwrap();
        refresh_owner_trust(box_dir, state, &owner, reader).unwrap();
        assert_eq!(admitted(&owner), std::slice::from_ref(&phone));
        let keys = copy(&owner)
            .unwrap()
            .profiles
            .into_keys()
            .collect::<Vec<_>>();
        assert_eq!(keys, ["c3c3c3c3c3c3"], "the owner's lock alone");

        file.profiles.get_mut(ANNA).unwrap().locked = false;
        write(&file);
        refresh_trust(box_dir, state, &id(), reader).unwrap();
        assert_eq!(admitted(&id()), [tv.clone(), phone.clone()], "open again");

        std::fs::write(box_dir.join(profile_locks::FILE), "not a lock file").unwrap();
        for (seat, owner) in [(id(), false), (owner.clone(), true)] {
            sync_trust(box_dir, state, &seat, reader, Who::Seat { owner }).unwrap();
            assert!(admitted(&seat).is_empty(), "{seat}");
            assert!(copy(&seat).unwrap().unreadable);
        }
        std::fs::remove_file(box_dir.join(profile_locks::FILE)).unwrap();
        refresh_trust(box_dir, state, &id(), reader).unwrap();
        assert_eq!(admitted(&id()), [tv, phone]);
        assert!(copy(&id()).is_none());
    }

    /// A file the copy refuses leaves its last good copy and doesn't stop the others; a box
    /// directory that is gone leaves every copy.
    #[test]
    fn a_bad_trust_file_or_a_missing_box_keeps_the_last_copies() {
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        for dir in [&box_dir, &state] {
            std::fs::create_dir(dir).unwrap();
        }
        let gid = own_gid(&box_dir);
        let reader = Reader::group(gid);
        std::fs::write(box_dir.join("punktfunk1-paired.json"), "[1]").unwrap();
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        assert!(refresh_trust(&box_dir, &state, &id(), reader).unwrap());
        let trust = trust_dir(&state, &id());

        // A second link makes the paired list unreadable to the copy; profiles still follow.
        std::fs::hard_link(
            box_dir.join("punktfunk1-paired.json"),
            temp.path().join("backup"),
        )
        .unwrap();
        std::fs::write(box_dir.join("profiles.json"), "{\"a\":1}").unwrap();
        let error = refresh_trust(&box_dir, &state, &id(), reader).unwrap_err();
        assert_eq!(error.code, "trust_copy");
        assert_eq!(
            std::fs::read_to_string(trust.join("punktfunk1-paired.json")).unwrap(),
            "[1]"
        );
        assert_eq!(
            std::fs::read_to_string(trust.join("profiles.json")).unwrap(),
            "{\"a\":1}"
        );

        let gone = temp.path().join("gone");
        assert!(!refresh_trust(&gone, &state, &id(), reader).unwrap());
        assert!(trust.join("punktfunk1-paired.json").exists());
        assert!(trust.join("profiles.json").exists());
    }

    /// A link in the box directory is never followed into the copy.
    #[test]
    fn a_symlink_in_the_box_is_not_copied() {
        let temp = tempfile::tempdir().unwrap();
        let box_dir = temp.path();
        let state_temp = tempfile::tempdir().unwrap();
        let state = state_temp.path();
        box_with_identity(box_dir);
        let secret = box_dir.join("elsewhere");
        std::fs::write(&secret, "root only").unwrap();
        std::os::unix::fs::symlink(&secret, box_dir.join("profiles.json")).unwrap();
        let gid = own_gid(box_dir);
        let error = refresh_trust(box_dir, state, &id(), Reader::group(gid)).unwrap_err();
        assert_eq!(error.code, "trust_copy");
        assert!(!trust_dir(state, &id()).join("profiles.json").exists());
    }

    /// A link where the box's avatar folder should be is refused, and nothing behind it is
    /// copied into a seat's trust copy.
    #[test]
    fn a_linked_profiles_directory_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        let secret = temp.path().join("secret");
        for dir in [&box_dir, &state, &secret] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(secret.join("shadow.png"), "root only").unwrap();
        std::os::unix::fs::symlink(&secret, box_dir.join("profiles")).unwrap();
        let gid = own_gid(&box_dir);
        let error = refresh_trust(&box_dir, &state, &id(), Reader::group(gid)).unwrap_err();
        assert_eq!(error.code, "trust_copy");
        assert!(!trust_dir(&state, &id())
            .join("profiles/shadow.png")
            .exists());
    }

    /// A FIFO the door puts among its files doesn't stall the supervisor: one in the avatar
    /// folder is skipped, one at a file the copy reads fails the refresh at once.
    #[test]
    fn a_fifo_in_the_box_does_not_block_the_copy() {
        use super::super::safe_open::tests::{fifo, within};
        let temp = tempfile::tempdir().unwrap();
        let (box_dir, state) = (temp.path().join("box"), temp.path().join("state"));
        std::fs::create_dir_all(box_dir.join("profiles")).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::write(box_dir.join("profiles.json"), "{}").unwrap();
        fifo(&box_dir.join("profiles/p1.png"));
        let gid = own_gid(&box_dir);
        let refresh = || {
            let (b, s) = (box_dir.clone(), state.clone());
            within(move || refresh_trust(&b, &s, &id(), Reader::group(gid)).is_ok())
        };
        assert_eq!(refresh(), Some(true), "an avatar FIFO is skipped");
        fifo(&box_dir.join("seat-defaults.json"));
        assert_eq!(refresh(), Some(false), "a box file FIFO fails the refresh");
    }

    /// A FIFO for the seat host's certificate answers at once and leaves no copy; a real one is
    /// copied for root alone.
    #[test]
    fn a_fifo_for_the_host_certificate_does_not_block() {
        use super::super::safe_open::tests::{fifo, within};
        let temp = tempfile::tempdir().unwrap();
        let (home, pin) = (temp.path().join("home"), temp.path().join("pin"));
        let config = home.join(".config/punktfunk");
        std::fs::create_dir_all(&config).unwrap();
        fifo(&config.join("native-cert.pem"));
        let (h, p) = (home.clone(), pin.clone());
        let copied = within(move || copy_pin(&h, &p).is_ok()).expect("blocked on the FIFO");
        assert!(!copied);
        assert!(!pin.join("native-cert.pem").exists());
        std::fs::remove_file(config.join("native-cert.pem")).unwrap();
        std::fs::write(config.join("native-cert.pem"), "pem").unwrap();
        copy_pin(&home, &pin).unwrap();
        assert_eq!(
            std::fs::read_to_string(pin.join("native-cert.pem")).unwrap(),
            "pem"
        );
        assert_eq!(mode(&pin.join("native-cert.pem")), 0o600);
    }
}
