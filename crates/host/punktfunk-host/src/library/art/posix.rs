//! POSIX art roots and the handle-resolved path check: `$HOME`, the system icon themes, and
//! the fd's link-resolved path.

use super::*;

/// `$HOME` is the POSIX analogue of the Windows users base. An empty list here would silently
/// serve no plugin art: POSIX absolute paths are classified as local.
///
/// The system icon dirs are world-readable package data, so a `.desktop` entry's icon can be
/// served. `/var/lib/flatpak` is whole because its exported icons are symlinks into `app/`,
/// and confinement judges the resolved path.
pub(super) fn extra_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = ["/usr/share/icons", "/usr/share/pixmaps", "/var/lib/flatpak"]
        .into_iter()
        .map(PathBuf::from)
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if !home.as_os_str().is_empty() {
            roots.push(home);
        }
    }
    roots
}

/// Link-resolved path of the object `f` is open on. Re-run confinement on that object, not on
/// a path that a rename after open could retarget.
#[cfg(target_os = "linux")]
pub(super) fn final_path_of(f: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd as _;
    std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).ok()
}

/// macOS twin (dev builds only — the shipped hosts are Linux and Windows): `F_GETPATH`.
#[cfg(not(target_os = "linux"))]
pub(super) fn final_path_of(f: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: the fd is a live open File and `buf` is the PATH_MAX-sized buffer F_GETPATH
    // requires; the kernel NUL-terminates what it writes.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return None;
    }
    let len = buf.iter().position(|&b| b == 0)?;
    Some(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(
        &buf[..len],
    )))
}
