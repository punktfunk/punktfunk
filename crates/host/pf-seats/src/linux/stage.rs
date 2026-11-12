//! `punktfunk-seats stage <check> <mount> <options> <source> <stage>`: one of the owner's folders
//! mounted at a seat's stage, checked where it is mounted. The seat unit runs it as root in PID
//! 1's mount namespace before each start (`ExecStartPre=`, [`super::libraries::unit_lines`]).
//!
//! The supervisor checks each folder by path when it writes the unit; a link or a rename on that
//! path afterwards would choose what the seat gets. So the helper opens `source` without
//! following a link on any part of its path, judges the folder it holds ([`refusal`]), clones
//! that folder to a scratch beside the stage, which only root can name, and runs
//! `mount --bind -o <options>` from there, the idmap among them.
//!
//! `owner=<uid>`: root opens the folder and it must be that uid's. `reader=<account>`: the account
//! opens it with its own groups, so it must be able to walk to it, read it and enter it.

use super::libraries::IDMAP_FS;

/// The verb of `punktfunk-seats` that runs the helper.
pub const VERB: &str = "stage";

/// Whom the folder answers to.
#[derive(Debug, PartialEq, Eq)]
enum Check {
    /// `owner=<uid>`: root opens it; it is that uid's.
    Owner(u32),
    /// `reader=<account>`: that account opens it.
    Reader(String),
}

fn parse(check: &str) -> Option<Check> {
    match check.split_once('=')? {
        ("owner", uid) => uid.parse().ok().map(Check::Owner),
        ("reader", account) if !account.is_empty() => Some(Check::Reader(account.to_owned())),
        _ => None,
    }
}

/// Why the folder the helper opened, owned by `uid` on a filesystem of `fstype`, may not be
/// mounted with `options` for `check`; `None` when it may.
fn refusal(check: &Check, options: &str, uid: u32, fstype: Option<&str>) -> Option<&'static str> {
    if matches!(check, Check::Owner(owner) if *owner != uid) {
        return Some("it isn't the owner's");
    }
    let idmapped = options.contains("X-mount.idmap=");
    if idmapped && !fstype.is_some_and(|t| IDMAP_FS.contains(&t)) {
        return Some("its filesystem can't take an idmapped mount");
    }
    None
}

/// The helper. The unit ignores its exit code: a folder not mounted leaves the seat an empty one.
#[cfg(target_os = "linux")]
pub fn run(check: &str, mount: &str, options: &str, source: &str, stage: &str) -> u8 {
    match linux::stage(check, mount, options, source, stage) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("seat stage: {source} not mounted: {error}");
            1
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::super::{accounts, libraries};
    use super::{parse, refusal, Check};
    use rustix::mount::UnmountFlags;
    use rustix::process::{Gid, Uid};
    use std::os::fd::OwnedFd;
    use std::path::Path;

    /// The folder at `source` mounted at `stage`, or why not. The scratch goes either way.
    pub(super) fn stage(
        check: &str,
        mount: &str,
        options: &str,
        source: &str,
        stage: &str,
    ) -> Result<(), String> {
        let check = parse(check).ok_or_else(|| format!("unknown check {check}"))?;
        if !Path::new(stage).starts_with(libraries::STAGE_ROOT) {
            return Err(format!("the stage lies outside {}", libraries::STAGE_ROOT));
        }
        let folder = match &check {
            Check::Owner(_) => open_dir(Path::new(source)),
            Check::Reader(account) => open_as(&reader(account)?, Path::new(source)),
        }
        .map_err(|e| format!("open: {e}"))?;
        let uid = rustix::fs::fstat(&folder)
            .map_err(|e| format!("read its owner: {e}"))?
            .st_uid;
        let scratch = format!("{stage}.src");
        let mounted = clone_to(&folder, &scratch)
            .map_err(|e| format!("clone it: {e}"))
            .and_then(|()| {
                let fstype = libraries::fstype(Path::new(&scratch));
                if let Some(reason) = refusal(&check, options, uid, fstype.as_deref()) {
                    return Err(reason.to_owned());
                }
                let args = ["--bind", "-o", options, &scratch, stage];
                super::super::run("mount", mount, &args)
                    .map(drop)
                    .map_err(|e| e.message)
            });
        let _ = rustix::mount::unmount(scratch.as_str(), UnmountFlags::DETACH);
        let _ = std::fs::remove_dir(&scratch);
        mounted
    }

    /// The folder at `path`, refused when any part of the path is a link.
    fn open_dir(path: &Path) -> std::io::Result<OwnedFd> {
        use rustix::fs::{Mode, OFlags, ResolveFlags, CWD};
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let resolve = ResolveFlags::NO_SYMLINKS;
        Ok(rustix::fs::openat2(
            CWD,
            path,
            flags,
            Mode::empty(),
            resolve,
        )?)
    }

    /// An account's ids, with the groups its login gets.
    struct Reader {
        uid: Uid,
        gid: Gid,
        groups: Vec<Gid>,
    }

    fn reader(account: &str) -> Result<Reader, String> {
        let found = accounts::lookup(account)
            .map_err(|e| e.message)?
            .ok_or_else(|| format!("no account {account}"))?;
        let ids =
            super::super::run("id_failed", "id", &["-G", "--", account]).map_err(|e| e.message)?;
        let groups = ids
            .split_whitespace()
            .map(|gid| gid.parse().map(Gid::from_raw))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("read the groups of {account}: {e}"))?;
        Ok(Reader {
            uid: Uid::from_raw(found.uid),
            gid: Gid::from_raw(found.gid),
            groups,
        })
    }

    /// [`open_dir`] as `reader`, who must search every folder on the way and read and enter the
    /// last at the instant the handle is taken. Only this thread changes ids, and its saved uid
    /// stays root's, so it takes root back after.
    fn open_as(reader: &Reader, path: &Path) -> std::io::Result<OwnedFd> {
        use rustix::fs::{Access, AtFlags};
        use rustix::thread::{set_thread_groups, set_thread_res_gid, set_thread_res_uid};
        let (gid, groups) = (rustix::process::getgid(), rustix::process::getgroups()?);
        set_thread_groups(&reader.groups)?;
        set_thread_res_gid(reader.gid, reader.gid, None::<Gid>)?;
        set_thread_res_uid(reader.uid, reader.uid, None::<Uid>)?;
        let opened = open_dir(path).and_then(|dir| {
            rustix::fs::accessat(&dir, ".", Access::EXEC_OK, AtFlags::empty())?;
            Ok(dir)
        });
        set_thread_res_uid(Uid::ROOT, Uid::ROOT, None::<Uid>)?;
        set_thread_res_gid(gid, gid, None::<Gid>)?;
        set_thread_groups(&groups)?;
        opened
    }

    /// A clone of `folder`'s mount, rooted at `folder`, attached at `scratch`. A scratch a killed
    /// run left mounted is detached first.
    fn clone_to(folder: &OwnedFd, scratch: &str) -> std::io::Result<()> {
        use rustix::mount::{move_mount, open_tree, MoveMountFlags, OpenTreeFlags};
        let _ = rustix::mount::unmount(scratch, UnmountFlags::DETACH);
        if let Err(e) = std::fs::create_dir(scratch)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(e);
        }
        let flags = OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_EMPTY_PATH;
        let tree = open_tree(folder, "", flags)?;
        let attach = MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH;
        Ok(move_mount(&tree, "", rustix::fs::CWD, scratch, attach)?)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        /// A link anywhere on the path refuses it, the last part too; the folder it names opens.
        #[test]
        fn a_link_on_the_way_is_refused() {
            let temp = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(temp.path()).unwrap();
            std::fs::create_dir_all(base.join("real/roms")).unwrap();
            symlink(base.join("real"), base.join("link")).unwrap();
            symlink(base.join("real/roms"), base.join("last")).unwrap();
            assert!(open_dir(&base.join("real/roms")).is_ok());
            for linked in ["link/roms", "last"] {
                let refused = open_dir(&base.join(linked)).unwrap_err();
                assert_eq!(refused.raw_os_error(), Some(libc::ELOOP), "{linked}");
            }
        }

        /// A folder swapped in on the path is judged as it is when opened: a reader is refused
        /// where a closed folder now lies on the way, and root sees whose the folder now is. The
        /// thread is root's again after each open.
        #[test]
        fn a_swapped_folder_is_judged_as_it_is_when_opened() {
            if !rustix::process::geteuid().is_root() {
                return;
            }
            let temp = tempfile::tempdir().unwrap();
            let base = std::fs::canonicalize(temp.path()).unwrap();
            chmod(&base, 0o755);
            for dir in ["open/roms", "closed/roms"] {
                std::fs::create_dir_all(base.join(dir)).unwrap();
                chmod(&base.join(dir), 0o755);
            }
            chmod(&base.join("open"), 0o755);
            chmod(&base.join("closed"), 0o700);
            let nobody = Reader {
                uid: Uid::from_raw(65534),
                gid: Gid::from_raw(65534),
                groups: vec![Gid::from_raw(65534)],
            };
            let roms = base.join("open/roms");
            assert!(open_as(&nobody, &roms).is_ok());
            assert!(rustix::process::geteuid().is_root());

            std::fs::rename(base.join("open"), base.join("was-open")).unwrap();
            std::fs::rename(base.join("closed"), base.join("open")).unwrap();
            let refused = open_as(&nobody, &roms).unwrap_err();
            assert_eq!(refused.raw_os_error(), Some(libc::EACCES));
            assert!(rustix::process::geteuid().is_root());
            assert_eq!(rustix::process::getegid(), rustix::process::getgid());

            let uid = rustix::fs::fstat(open_dir(&roms).unwrap()).unwrap().st_uid;
            let owner = Check::Owner(65534);
            assert_eq!(refusal(&owner, "", uid, None), Some("it isn't the owner's"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_names_an_owner_uid_or_a_reader_account() {
        assert_eq!(parse("owner=1000"), Some(Check::Owner(1000)));
        assert_eq!(parse("reader=enrico"), Some(Check::Reader("enrico".into())));
        for bad in ["owner=", "owner=me", "reader=", "owner", "writer=1000", ""] {
            assert_eq!(parse(bad), None, "{bad:?}");
        }
    }

    /// A folder the owner check names must be that uid's; an idmap needs a filesystem that
    /// takes one, whoever opened it; a plain bind needs neither.
    #[test]
    fn a_folder_is_refused_for_the_owner_or_the_filesystem() {
        let idmap = "ro,nosuid,nodev,X-mount.idmap=u:1000:987:1 g:1000:956:1";
        let (owner, reader) = (Check::Owner(1000), Check::Reader("enrico".into()));
        let not_owner = Some("it isn't the owner's");
        let no_idmap = Some("its filesystem can't take an idmapped mount");
        assert_eq!(refusal(&owner, idmap, 1000, Some("btrfs")), None);
        assert_eq!(refusal(&owner, idmap, 1000, Some("ext4")), None);
        assert_eq!(refusal(&owner, idmap, 1001, Some("btrfs")), not_owner);
        assert_eq!(refusal(&owner, idmap, 0, Some("btrfs")), not_owner);
        assert_eq!(refusal(&owner, idmap, 1000, Some("exfat")), no_idmap);
        assert_eq!(refusal(&owner, idmap, 1000, None), no_idmap);
        assert_eq!(refusal(&reader, idmap, 0, Some("xfs")), None);
        assert_eq!(refusal(&reader, idmap, 0, Some("fuseblk")), no_idmap);
        let plain = "ro,nosuid,nodev";
        assert_eq!(refusal(&reader, plain, 0, Some("exfat")), None);
    }
}
