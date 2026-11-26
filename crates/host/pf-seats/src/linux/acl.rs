//! POSIX ACLs as the kernel stores them, and the walk that keeps the games folder writable.
//!
//! An ACL is read and written as bytes in the `system.posix_acl_access` and
//! `system.posix_acl_default` extended attributes: a 4-byte version 2, then 8-byte entries,
//! little-endian. [`acl_bytes`] builds them, and [`reader_acl`] makes the one for a file only
//! root writes and one uid may read.
//!
//! [`fix_acl`] repairs the mask below the games folder. A file one seat creates with mode 0644
//! or 0755 gets a mask that cuts the group entry, so the other seats can't write it. Every seat
//! writes in the tree, so the walk goes through handles, never following a link, and runs on a
//! thread of its own.

use super::layout::io_err;
use super::safe_open;
use crate::backend::BackendError;
use rustix::fs::{AtFlags, Mode, OFlags, XattrFlags};
use rustix::io::Errno;
use std::ffi::CStr;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::sync::Mutex;

/// The access ACL as the kernel stores it: a 4-byte version 2, then 8-byte entries, little-endian.
pub(super) const ACL_ACCESS: &str = "system.posix_acl_access";
/// The ACL a folder's new files and folders start with, stored the same way.
pub(super) const ACL_DEFAULT: &str = "system.posix_acl_default";
/// An ACL entry's tags: the file's user, a named user, the file's group, the mask, others.
pub(super) const ACL_USER_OBJ: u16 = 0x01;
pub(super) const ACL_USER: u16 = 0x02;
pub(super) const ACL_GROUP_OBJ: u16 = 0x04;
pub(super) const ACL_MASK: u16 = 0x10;
pub(super) const ACL_OTHER: u16 = 0x20;
/// The id of an entry that names nobody.
pub(super) const ACL_NO_ID: u32 = u32::MAX;
/// Folders deeper than this below `games` keep their mask: the walk holds a handle per level.
const ACL_DEPTH: usize = 64;
/// The buffer an ACL is read into first, and the most it grows to: the kernel's xattr limit.
pub(super) const ACL_START: usize = 1024;
const ACL_MAX: usize = 64 * 1024;

/// Re-applies `m::rwX` below the games folder, as `setfacl -R` would, to a mask of `r--` or
/// `r-x`. A file one seat creates with mode 0644 or 0755 gets such a mask, which cuts the group
/// entry, so the other seats can't write it. Any other mask is a seat's choice and stays. Every
/// seat writes in the tree, so it is walked through handles: each entry opened in its folder's
/// handle without following a link, and changed through its own.
///
/// Walks the whole tree each call: cheap for a normal library, slow past millions of files,
/// where an inotify watch on `steamapps` would fix only what changed. So it walks on a thread of
/// its own and returns at once, and the caller's trust copy never waits on it. A call while a walk
/// runs asks for one more walk after it. A walk that fails is logged there.
pub(super) fn fix_acl(state: &Path) -> Result<(), BackendError> {
    {
        let mut walk = ACL_WALK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(again) = walk.as_mut() {
            *again = true;
            return Ok(());
        }
        *walk = Some(false);
    }
    let state = state.to_path_buf();
    let spawned = std::thread::Builder::new()
        .name("seats-acl".into())
        .spawn(move || loop {
            if let Err(error) = repair_games(&state) {
                tracing::warn!(%error, "games ACL mask not repaired");
            }
            let mut walk = ACL_WALK.lock().unwrap_or_else(|e| e.into_inner());
            if *walk == Some(true) {
                *walk = Some(false);
            } else {
                *walk = None;
                return;
            }
        });
    spawned.map(drop).map_err(|e| {
        *ACL_WALK.lock().unwrap_or_else(|e| e.into_inner()) = None;
        io_err("games_acl", "start the walk", e)
    })
}

/// [`fix_acl`]'s walk: `None` while idle, `Some(asked_again)` while one runs.
static ACL_WALK: Mutex<Option<bool>> = Mutex::new(None);

fn repair_games(state: &Path) -> Result<(), BackendError> {
    let io = |what: &str, e| io_err("games_acl", what, e);
    let top = safe_open::dir(state).map_err(|e| io("open the state directory", e))?;
    let Some(games) = safe_open::dir_at(&top, "games").map_err(|e| io("open games", e))? else {
        return Ok(());
    };
    mask_tree(games).map_err(|e| io("repair the games ACL mask", e))
}

/// [`fix_acl`]'s walk below `top`, depth first. An entry that goes, or turns into something else
/// mid-walk, is skipped; the first other error ends the walk's answer, not the walk.
fn mask_tree(top: OwnedFd) -> std::io::Result<()> {
    set_mask(&top, true)?;
    let mut first = None;
    let mut stack = vec![rustix::fs::Dir::new(top)?];
    while let Some(dir) = stack.last_mut() {
        let Some(entry) = dir.read() else {
            stack.pop();
            continue;
        };
        let child = entry
            .map_err(std::io::Error::from)
            .and_then(|entry| mask_entry(dir.fd()?, entry.file_name()))
            .and_then(|sub| {
                sub.map(rustix::fs::Dir::new)
                    .transpose()
                    .map_err(Into::into)
            });
        match child {
            Ok(Some(sub)) if stack.len() < ACL_DEPTH => stack.push(sub),
            Ok(_) => {}
            Err(error) => {
                first.get_or_insert(error);
            }
        }
    }
    first.map_or(Ok(()), Err)
}

/// Repairs the mask of `name` in `at`; a folder comes back to be walked. Only a folder or a
/// regular file is opened, never through a link; one swapped for a link or another kind since
/// it was listed is skipped. A FIFO, socket or link has no mask to repair.
fn mask_entry(at: BorrowedFd<'_>, name: &CStr) -> std::io::Result<Option<OwnedFd>> {
    use rustix::fs::FileType;
    if name == c"." || name == c".." {
        return Ok(None);
    }
    let Ok(listed) = rustix::fs::statat(at, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return Ok(None);
    };
    let kind = FileType::from_raw_mode(listed.st_mode);
    let flags = match kind {
        FileType::Directory => OFlags::DIRECTORY,
        FileType::RegularFile => OFlags::NOCTTY,
        _ => return Ok(None),
    };
    let flags = flags | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::openat(at, name, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(e) if [Errno::NOENT, Errno::LOOP, Errno::NOTDIR, Errno::NXIO].contains(&e) => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };
    let mode = rustix::fs::fstat(&fd)?.st_mode;
    if FileType::from_raw_mode(mode) != kind {
        return Ok(None);
    }
    let dir = kind == FileType::Directory;
    set_mask(&fd, dir || mode & 0o111 != 0)?;
    Ok(dir.then_some(fd))
}

/// Sets the mask of `fd`'s access ACL to `rw`, plus `x` when `exec`, if it is `r--` or `r-x`. A
/// file with no ACL has no named entry for a mask to cut, and stays as it is.
fn set_mask(fd: impl AsFd, exec: bool) -> std::io::Result<()> {
    let mut buf = vec![0u8; ACL_START];
    let len = loop {
        match rustix::fs::fgetxattr(&fd, ACL_ACCESS, &mut buf[..]) {
            Ok(len) => break len,
            Err(Errno::NODATA) => return Ok(()),
            Err(Errno::RANGE) if buf.len() < ACL_MAX => buf.resize(buf.len() * 4, 0),
            Err(e) => return Err(e.into()),
        }
    };
    if let Some(acl) = with_mask(&buf[..len], if exec { 7 } else { 6 }) {
        rustix::fs::fsetxattr(&fd, ACL_ACCESS, &acl, XattrFlags::REPLACE)?;
    }
    Ok(())
}

/// `acl` with its mask set to `perm`; `None` when it is no version-2 ACL, has no mask, or its
/// mask is not `r--` or `r-x`. Those are the group bits of a file created 0644 or 0755, and all
/// this widens: a file a seat closed with `chmod 600` has `---` and stays closed.
fn with_mask(acl: &[u8], perm: u16) -> Option<Vec<u8>> {
    let (version, entries) = acl.split_first_chunk::<4>()?;
    if u32::from_le_bytes(*version) != 2 || !entries.len().is_multiple_of(8) {
        return None;
    }
    let mask = entries
        .chunks_exact(8)
        .position(|e| u16::from_le_bytes([e[0], e[1]]) == ACL_MASK)?;
    let at = 4 + mask * 8 + 2;
    if !matches!(u16::from_le_bytes([acl[at], acl[at + 1]]), 4 | 5) {
        return None;
    }
    let mut out = acl.to_vec();
    out[at..at + 2].copy_from_slice(&perm.to_le_bytes());
    Some(out)
}

/// `entries` as the kernel stores an ACL: version 2, then `(tag, perm, id)` each, little-endian,
/// sorted by tag and then id. The owner, group, mask and other entries take [`ACL_NO_ID`].
pub(super) fn acl_bytes(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in entries {
        acl.extend(tag.to_le_bytes());
        acl.extend(perm.to_le_bytes());
        acl.extend(id.to_le_bytes());
    }
    acl
}

/// The access ACL of a file only root writes and `uid` may read: `rw-` for root, `r--` for `uid`
/// and the mask, nothing for the group or others. A folder (`dir`) adds `x` to all three.
pub(super) fn reader_acl(uid: u32, dir: bool) -> Vec<u8> {
    let x = u16::from(dir);
    acl_bytes(&[
        (ACL_USER_OBJ, 6 | x, ACL_NO_ID),
        (ACL_USER, 4 | x, uid),
        (ACL_GROUP_OBJ, 0, ACL_NO_ID),
        (ACL_MASK, 4 | x, ACL_NO_ID),
        (ACL_OTHER, 0, ACL_NO_ID),
    ])
}

/// Sets `acl` as the access ACL of `fd`. On a test's thread that asks, it fails as a filesystem
/// without ACLs does.
pub(super) fn set_acl(fd: impl AsFd, acl: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if tests::REFUSE_ACL.get() {
        return Err(Errno::NOTSUP.into());
    }
    Ok(rustix::fs::fsetxattr(
        fd,
        ACL_ACCESS,
        acl,
        XattrFlags::empty(),
    )?)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    thread_local! {
        /// Set, [`set_acl`] on this thread fails as a filesystem without ACLs does.
        pub(in crate::linux) static REFUSE_ACL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// An access ACL: owner `rw`, group `r`, a named group `rw`, the mask, other `r`.
    fn acl(mask: u16) -> Vec<u8> {
        acl_with_groups(mask, 1)
    }

    /// [`acl`] with `groups` named groups.
    fn acl_with_groups(mask: u16, groups: u32) -> Vec<u8> {
        let entry = |tag: u16, perm: u16, id: u32| {
            [
                &tag.to_le_bytes()[..],
                &perm.to_le_bytes(),
                &id.to_le_bytes(),
            ]
            .concat()
        };
        let none = u32::MAX;
        [
            2u32.to_le_bytes().to_vec(),
            entry(0x01, 6, none),
            entry(0x04, 4, none),
            (0..groups).flat_map(|n| entry(0x08, 6, 4242 + n)).collect(),
            entry(ACL_MASK, mask, none),
            entry(0x20, 4, none),
        ]
        .concat()
    }

    /// The mask of `path`'s access ACL.
    #[cfg(target_os = "linux")]
    fn mask_of(path: &Path) -> u16 {
        let mut buf = vec![0u8; 8192];
        let len = rustix::fs::getxattr(path, ACL_ACCESS, &mut buf[..]).unwrap();
        let mut entries = buf[4..len].chunks_exact(8);
        let mask = entries.find(|e| u16::from_le_bytes([e[0], e[1]]) == ACL_MASK);
        mask.map(|e| u16::from_le_bytes([e[2], e[3]])).unwrap()
    }

    /// `m::rwX` as `setfacl` reads it for the group bits of a file made 0644 or 0755, and nothing
    /// done to an ACL it can't read or to a mask a seat narrowed.
    #[test]
    fn the_acl_mask_becomes_rw_with_x_where_asked() {
        assert_eq!(with_mask(&acl(4), 6), Some(acl(6)));
        assert_eq!(with_mask(&acl(5), 7), Some(acl(7)));
        assert_eq!(with_mask(&acl(6), 6), None, "already rw");
        assert_eq!(with_mask(&acl(7), 7), None, "already rwx");
        for closed in 0..4 {
            assert_eq!(
                with_mask(&acl(closed), 7),
                None,
                "mask {closed} is a seat's"
            );
        }
        assert_eq!(with_mask(&acl(4)[..20], 6), None, "torn");
        let mut version_1 = acl(4);
        version_1[0] = 1;
        assert_eq!(with_mask(&version_1, 6), None, "not version 2");
    }

    /// The games walk repairs what is inside and never what a link leads to, also when a folder
    /// it listed is swapped for a link before it opens it. POSIX ACLs are Linux's.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_swapped_in_mid_walk_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let (games, outside) = (temp.path().join("games"), temp.path().join("outside"));
        std::fs::create_dir_all(games.join("sub")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(games.join("sub/a"), "x").unwrap();
        std::fs::write(outside.join("b"), "x").unwrap();
        for path in [games.join("sub"), games.join("sub/a"), outside.join("b")] {
            let set = rustix::fs::setxattr(&path, ACL_ACCESS, &acl(4), XattrFlags::empty());
            if set.is_err() {
                eprintln!("no POSIX ACLs under {}: skipped", temp.path().display());
                return;
            }
        }
        rustix::fs::setxattr(&outside, ACL_ACCESS, &acl(4), XattrFlags::empty()).unwrap();
        let handle = safe_open::dir(&games).unwrap();
        let mut listing = rustix::fs::Dir::read_from(&handle).unwrap();
        let listed = std::iter::from_fn(|| listing.read())
            .map(Result::unwrap)
            .find(|e| e.file_name() == c"sub")
            .unwrap();
        assert_eq!(listed.file_type(), rustix::fs::FileType::Directory);
        std::fs::rename(games.join("sub"), games.join("sub.old")).unwrap();
        std::os::unix::fs::symlink(&outside, games.join("sub")).unwrap();
        let opened = mask_entry(handle.as_fd(), listed.file_name()).unwrap();
        assert!(opened.is_none(), "the link is not opened");
        mask_tree(safe_open::dir(&games).unwrap()).unwrap();
        assert_eq!(mask_of(&outside), 4, "the link's target");
        assert_eq!(mask_of(&outside.join("b")), 4);
        assert_eq!(mask_of(&games.join("sub.old")), 7, "a folder takes x");
        assert_eq!(mask_of(&games.join("sub.old/a")), 6);
    }

    /// A file a seat closed with `chmod 600` stays closed through the walk; one left at `r--`
    /// opens to the group.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_walk_leaves_a_closed_file_closed() {
        let temp = tempfile::tempdir().unwrap();
        let games = temp.path().join("games");
        std::fs::create_dir(&games).unwrap();
        for (name, mask) in [("open", 4), ("closed", 0)] {
            let path = games.join(name);
            std::fs::write(&path, "x").unwrap();
            if rustix::fs::setxattr(&path, ACL_ACCESS, &acl(mask), XattrFlags::empty()).is_err() {
                eprintln!("no POSIX ACLs under {}: skipped", temp.path().display());
                return;
            }
        }
        mask_tree(safe_open::dir(&games).unwrap()).unwrap();
        assert_eq!(mask_of(&games.join("open")), 6);
        assert_eq!(mask_of(&games.join("closed")), 0);
    }

    /// An ACL longer than the first read buffer is repaired too, not skipped with its subtree.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_long_acl_is_repaired() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        std::fs::write(&path, "x").unwrap();
        let long = acl_with_groups(4, 300);
        assert!(long.len() > ACL_START);
        if rustix::fs::setxattr(&path, ACL_ACCESS, &long, XattrFlags::empty()).is_err() {
            eprintln!("no POSIX ACLs under {}: skipped", temp.path().display());
            return;
        }
        set_mask(std::fs::File::open(&path).unwrap(), false).unwrap();
        let mut buf = vec![0u8; 8192];
        let len = rustix::fs::getxattr(&path, ACL_ACCESS, &mut buf[..]).unwrap();
        assert_eq!(&buf[..len], acl_with_groups(6, 300).as_slice());
    }

    /// `fix_acl` hands the walk to a thread and is idle again once it ends.
    #[test]
    fn the_acl_walk_runs_apart_and_ends() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("games")).unwrap();
        fix_acl(temp.path()).unwrap();
        fix_acl(temp.path()).unwrap();
        let idle = || ACL_WALK.lock().unwrap().is_none();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !idle() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(idle());
    }
}
