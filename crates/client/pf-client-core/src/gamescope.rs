//! Whether a gamescope compositor is actually on the other end, and which socket reaches it.
//!
//! `GAMESCOPE_WAYLAND_DISPLAY` is not proof: the flatpak sets it on every launch so the
//! vendored gamescope WSI layer can negotiate HDR10 (`packaging/flatpak/io.unom.punktfunk.yml`).
//!
//! [`under_gamescope`] is the only check. Proof is `WAYLAND_DISPLAY` naming gamescope's
//! socket, by name or by inode (a flatpak may bind it as `wayland-0`); with no
//! `WAYLAND_DISPLAY`, the live socket, as in X11 Gaming Mode; or `XDG_CURRENT_DESKTOP`, which
//! gamescope sets to `gamescope` for every app it starts.
//!
//! gamescope also unsets `WAYLAND_DISPLAY` for its apps. A flatpak then binds `wayland-0`,
//! the desktop's socket where one runs, and `fallback-x11` drops `DISPLAY`. gamescope's WSI
//! layer carries HDR on X11 and Wayland surfaces alike, but reads `WAYLAND_DISPLAY` once and
//! stays off unless it names gamescope's socket. [`foreign`] is the name a launch needs.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Desktop,
    /// Inside gamescope; `WAYLAND_DISPLAY`, where set, reaches it.
    Gamescope,
    /// Inside gamescope, with `WAYLAND_DISPLAY` naming another compositor.
    Foreign,
}

pub fn under_gamescope() -> bool {
    place() != Place::Desktop
}

/// gamescope's socket name, as gamescope gave it, when this app runs inside gamescope but
/// `WAYLAND_DISPLAY` names another compositor. A process started with that name as its
/// `WAYLAND_DISPLAY` gets `None`.
pub fn foreign() -> Option<OsString> {
    if place() != Place::Foreign {
        return None;
    }
    std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY")
}

fn place() -> Place {
    decide(
        std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("XDG_CURRENT_DESKTOP").as_deref(),
        |name| runtime_path(name).is_some_and(|p| p.exists()),
        |a, b| {
            runtime_path(a)
                .zip(runtime_path(b))
                .is_some_and(|(a, b)| same_socket(&a, &b))
        },
    )
}

/// `name` under `XDG_RUNTIME_DIR`. `join` keeps an absolute name as it is.
pub fn runtime_path(name: &OsStr) -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|dir| Path::new(&dir).join(name))
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

/// Testable form of [`place`]. Does not read the process environment.
fn decide(
    gamescope: Option<&OsStr>,
    wayland: Option<&OsStr>,
    desktop: Option<&OsStr>,
    socket: impl Fn(&OsStr) -> bool,
    same: impl Fn(&OsStr, &OsStr) -> bool,
) -> Place {
    let started_us = desktop.is_some_and(|d| d.eq_ignore_ascii_case("gamescope"));
    let reaches = |w: &OsStr| gamescope.is_some_and(|g| w == g || same(g, w));
    match wayland {
        Some(w) if reaches(w) => Place::Gamescope,
        Some(_) if started_us => Place::Foreign,
        None if started_us || gamescope.is_some_and(&socket) => Place::Gamescope,
        _ => Place::Desktop,
    }
}

#[cfg(test)]
mod tests {
    use super::{decide, Place};
    use std::ffi::OsStr;

    #[test]
    fn only_a_real_gamescope_counts() {
        let gs = Some(OsStr::new("gamescope-0"));
        let wl0 = Some(OsStr::new("wayland-0"));
        let kde = Some(OsStr::new("KDE"));
        let present = |_: &OsStr| true;
        let absent = |_: &OsStr| false;
        let apart = |_: &OsStr, _: &OsStr| false;
        let aliased = |_: &OsStr, _: &OsStr| true;

        // Manifest sets gamescope-0; desktop compositor is wayland-0.
        assert_eq!(decide(gs, wl0, kde, absent, apart), Place::Desktop);
        // Socket existence is not enough while WAYLAND_DISPLAY names the desktop.
        assert_eq!(decide(gs, wl0, kde, present, apart), Place::Desktop);
        assert_eq!(decide(gs, gs, None, absent, apart), Place::Gamescope);
        // A flatpak nested under `--expose-wayland` sees gamescope's socket as wayland-0.
        assert_eq!(decide(gs, wl0, None, absent, aliased), Place::Gamescope);
        // Gaming Mode is X11: no WAYLAND_DISPLAY; the named socket is the proof.
        assert_eq!(decide(gs, None, None, present, apart), Place::Gamescope);
        assert_eq!(decide(gs, None, None, absent, apart), Place::Desktop);
        assert_eq!(decide(None, None, kde, present, aliased), Place::Desktop);
    }

    #[test]
    fn a_flatpak_inside_gamescope_is_handed_the_desktop_socket() {
        let gs = Some(OsStr::new("gamescope-0"));
        let wl0 = Some(OsStr::new("wayland-0"));
        let apart = |_: &OsStr, _: &OsStr| false;
        // `gamescope -- flatpak run`: flatpak binds the desktop's (or a dead) wayland-0.
        for desktop in ["gamescope", "Gamescope"] {
            let desktop = Some(OsStr::new(desktop));
            assert_eq!(decide(gs, wl0, desktop, |_| true, apart), Place::Foreign);
            // Restarted with gamescope's name, by name alone: no second restart.
            assert_eq!(decide(gs, gs, desktop, |_| false, apart), Place::Gamescope);
            assert_eq!(
                decide(gs, None, desktop, |_| false, apart),
                Place::Gamescope
            );
        }
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
