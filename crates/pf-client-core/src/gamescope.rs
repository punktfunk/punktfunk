//! Whether a gamescope compositor is actually on the other end.
//!
//! `GAMESCOPE_WAYLAND_DISPLAY` is not proof: the flatpak sets it on every launch so the
//! vendored gamescope WSI layer can negotiate HDR10 (`packaging/flatpak/io.unom.Punktfunk.yml`).
//! Callers that treated the variable as Gaming Mode then fullscreened a GNOME/KDE session.
//!
//! [`under_gamescope`] is the only check. With `WAYLAND_DISPLAY` set, it must name gamescope's
//! socket, by name or by inode: a flatpak sees gamescope's socket bound as `wayland-0`. Gaming
//! Mode is X11 (`DISPLAY=:1`, no `WAYLAND_DISPLAY`); the live `xdg-run` socket named by the
//! variable is then the proof.

use std::ffi::OsStr;
use std::path::Path;

pub fn under_gamescope() -> bool {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR");
    // `join` keeps an absolute name as it is.
    let path = |name: &OsStr| runtime.as_ref().map(|dir| Path::new(dir).join(name));
    decide(
        std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        |name| path(name).is_some_and(|p| p.exists()),
        |a, b| {
            path(a)
                .zip(path(b))
                .is_some_and(|(a, b)| same_socket(&a, &b))
        },
    )
}

/// Both paths reach one socket, as a bind mount leaves it.
#[cfg(unix)]
fn same_socket(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => a.file_type().is_socket() && (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

#[cfg(not(unix))]
fn same_socket(_: &Path, _: &Path) -> bool {
    false
}

/// Testable form of [`under_gamescope`]. Does not read the process environment.
fn decide(
    gamescope: Option<&OsStr>,
    wayland: Option<&OsStr>,
    socket: impl Fn(&OsStr) -> bool,
    same: impl Fn(&OsStr, &OsStr) -> bool,
) -> bool {
    let Some(gamescope) = gamescope else {
        return false;
    };
    match wayland {
        Some(wayland) => wayland == gamescope || same(gamescope, wayland),
        None => socket(gamescope),
    }
}

#[cfg(test)]
mod tests {
    use super::decide;
    use std::ffi::OsStr;

    #[test]
    fn only_a_real_gamescope_counts() {
        let gs = Some(OsStr::new("gamescope-0"));
        let wl0 = Some(OsStr::new("wayland-0"));
        let present = |_: &OsStr| true;
        let absent = |_: &OsStr| false;
        let apart = |_: &OsStr, _: &OsStr| false;
        let aliased = |_: &OsStr, _: &OsStr| true;

        // Manifest sets gamescope-0; desktop compositor is wayland-0.
        assert!(!decide(gs, wl0, absent, apart));
        // Socket existence is not enough while WAYLAND_DISPLAY names the desktop.
        assert!(!decide(gs, wl0, present, apart));
        assert!(decide(gs, gs, absent, apart));
        // A flatpak nested under `--expose-wayland` sees gamescope's socket as wayland-0.
        assert!(decide(gs, wl0, absent, aliased));
        // Gaming Mode is X11: no WAYLAND_DISPLAY; the named socket is the proof.
        assert!(decide(gs, None, present, apart));
        assert!(!decide(gs, None, absent, apart));
        assert!(!decide(None, None, present, aliased));
    }

    #[cfg(unix)]
    #[test]
    fn a_bound_socket_is_the_same_socket() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("pf-gs-sock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (gs, other) = (dir.join("gamescope-0"), dir.join("wayland-0"));
        let _a = UnixListener::bind(&gs).unwrap();
        let _b = UnixListener::bind(&other).unwrap();
        assert!(super::same_socket(&gs, &gs));
        assert!(!super::same_socket(&gs, &other));
        assert!(!super::same_socket(&gs, &dir.join("missing")));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
