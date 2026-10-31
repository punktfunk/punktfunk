//! How root reads a file in a tree another uid can write: the box's, a seat's home, the owner's.
//!
//! The last step is always `O_NOFOLLOW | O_NONBLOCK`, so a link is refused and a FIFO or device
//! answers at once instead of stalling the supervisor. Only a regular file with one link and
//! within its size cap is read. A directory inside such a tree is opened the same way, one
//! component at a time ([`dir_at`], [`walk`]); a path is followed only up to a directory root
//! owns.

use rustix::fs::{Mode, OFlags};
use std::io::Read as _;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

/// A directory whose own path only root can change: `/var/lib/punktfunk`, a user's home.
pub(super) fn dir(path: &Path) -> std::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::open(path, flags, Mode::empty())?)
}

/// `name` in `at`, a directory and not a link. `None` when absent.
pub(super) fn dir_at(at: impl AsFd, name: &str) -> std::io::Result<Option<OwnedFd>> {
    open_at(at, name, OFlags::DIRECTORY)
}

fn open_at(at: impl AsFd, name: &str, extra: OFlags) -> std::io::Result<Option<OwnedFd>> {
    let flags = extra | OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    match rustix::fs::openat(at, name, flags, Mode::empty()) {
        Ok(fd) => Ok(Some(fd)),
        Err(e) if e == rustix::io::Errno::NOENT => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `name` in `at` as a regular single-link file of at most `max` bytes. `None` when absent.
pub(super) fn file_at(
    at: impl AsFd,
    name: &str,
    max: u64,
) -> std::io::Result<Option<(std::fs::File, std::fs::Metadata)>> {
    let Some(fd) = open_at(at, name, OFlags::NOCTTY)? else {
        return Ok(None);
    };
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() > 1 || meta.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{name} is not a plain single-link file within {max} bytes"),
        ));
    }
    Ok(Some((file, meta)))
}

/// The bytes of `name` in `at`, as [`file_at`] opens it. A file that grows past `max` while it
/// is read is refused.
pub(super) fn read_at(
    at: impl AsFd,
    name: &str,
    max: u64,
) -> std::io::Result<Option<(Vec<u8>, std::fs::Metadata)>> {
    let Some((file, meta)) = file_at(at, name, max)? else {
        return Ok(None);
    };
    let bytes = read_opened(file, &meta, name, max)?;
    Ok(Some((bytes, meta)))
}

/// The bytes of `file`, which [`file_at`] opened as `name`. A file that grows past `max` while it
/// is read is refused.
pub(super) fn read_opened(
    file: std::fs::File,
    meta: &std::fs::Metadata,
    name: &str,
    max: u64,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{name} grew beyond {max} bytes"),
        ));
    }
    Ok(bytes)
}

/// [`read_at`] for the last component of `path`, its directory opened by path with search
/// permission alone: the door reads the owner's record through a 0711 directory it can't list.
pub(super) fn read(path: &Path, max: u64) -> std::io::Result<Option<(Vec<u8>, std::fs::Metadata)>> {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} names no file", path.display()),
        ));
    };
    let flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    match rustix::fs::open(parent, flags, Mode::empty()) {
        Ok(at) => read_at(at, name, max),
        Err(e) if e == rustix::io::Errno::NOENT => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// `names` below `at`, one directory at a time, none of them a link. `None` when one is absent.
pub(super) fn walk(at: OwnedFd, names: &[&str]) -> std::io::Result<Option<OwnedFd>> {
    let mut at = at;
    for name in names {
        match dir_at(&at, name)? {
            Some(next) => at = next,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A FIFO at `path`: opening it to read blocks until a writer comes.
    pub(in crate::linux) fn fifo(path: &Path) {
        let made = std::process::Command::new("mkfifo").arg(path).status();
        assert!(made.unwrap().success(), "mkfifo {}", path.display());
    }

    /// What `f` returns, or `None` while it is still blocked after two seconds.
    pub(in crate::linux) fn within<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(2)).ok()
    }

    /// Only a plain file with one link within the cap is read; a link, a FIFO, a second link or
    /// an oversized file is refused at once, and an absent one is `None`.
    #[test]
    fn only_a_plain_file_within_the_cap_is_read() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        std::fs::write(dir.join("plain"), "ok").unwrap();
        std::os::unix::fs::symlink(dir.join("plain"), dir.join("link")).unwrap();
        std::fs::write(dir.join("twice"), "x").unwrap();
        std::fs::hard_link(dir.join("twice"), dir.join("twice-2")).unwrap();
        fifo(&dir.join("fifo"));
        let at = |name: &'static str, max: u64| {
            let dir = dir.clone();
            within(move || read(&dir.join(name), max).map(|f| f.map(|(b, _)| b))).expect("blocked")
        };
        assert_eq!(at("plain", 2).unwrap(), Some(b"ok".to_vec()));
        assert!(at("plain", 1).is_err());
        assert!(at("link", 2).is_err());
        assert!(at("twice", 2).is_err());
        assert!(at("fifo", 2).is_err());
        assert_eq!(at("absent", 2).unwrap(), None);
    }

    /// Search permission is enough, as the door has on the state directory. Root passes this
    /// whatever the flags.
    #[test]
    fn a_directory_that_cannot_be_listed_is_read_through() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("state");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("owner"), "deck\n").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o100)).unwrap();
        let bytes = read(&dir.join("owner"), 8).map(|f| f.map(|(b, _)| b));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(bytes.unwrap(), Some(b"deck\n".to_vec()));
    }
}
