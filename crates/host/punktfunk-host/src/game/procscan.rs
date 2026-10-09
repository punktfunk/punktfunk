//! Find a launched game's processes from its store signals ([`crate::library::DetectSpec`]).
//!
//! Read-only: enumerate processes and read metadata already visible. No ptrace, no
//! injection, no handles held open. Linux sees only its own uid; Windows runs as
//! SYSTEM and can see everything, which is why the two rules are load-bearing:
//!
//! 1. **Never adopt a process that predates the launch.** Filter by start time
//!    against [`launch_stamp`], taken before anything spawns.
//! 2. **Never trust a bare pid.** Every remembered process carries its start time
//!    and is re-verified ([`Scanner::alive`]) before it is counted running or signalled.
//!
//! `/proc` on Linux, Toolhelp on Windows; same [`Scanner`] surface.
//! [`crate::gamelease`] is platform-neutral.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Scanner;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Scanner;
#[cfg(windows)]
pub(crate) use windows::{process_image, processes};

/// Adopted process: pid plus a start stamp that pins that pid to *this* process.
///
/// `start` is compared only for equality against a later read of the same pid.
/// Units differ: clock ticks since boot on Linux, a creation `FILETIME` on Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcRef {
    pub pid: u32,
    pub start: u64,
}

/// Slack on the "started after the launch" test, in seconds.
///
/// Start times are quantized (~10 ms on Linux) and a launcher can race the host,
/// so an exact comparison would reject the real game. Two seconds is far below
/// any launcher's bring-up, so a pre-existing instance still fails the filter.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
pub const START_SLACK_SECS: f64 = 2.0;

/// Reference instant for adopting a launch's processes, in seconds on the
/// platform process-start timeline (since boot on Linux, Windows epoch on
/// Windows). Compared only to a process start time on the same platform; never
/// a wall clock and never persisted.
///
/// Call **before** anything spawns ([`crate::gamelease::LeaseRequest::launch_stamp`]).
/// `None` (no matcher, or unread clock) disables the start-time filter.
pub fn launch_stamp() -> Option<f64> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Scanner::system().now_stamp()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

/// Pin a pid the host just spawned (rule 2). See [`Scanner::resolve`].
/// Platform-neutral so [`crate::gamelease`] stays free of `cfg`s. `None` with
/// no matcher, or if the pid is gone or unqueryable.
pub fn resolve(pid: u32) -> Option<ProcRef> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Scanner::system().resolve(pid)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Re-verify remembered processes. Empty with no matcher. Platform-neutral
/// so [`crate::gamelease`] stays free of `cfg`s.
pub fn alive(procs: &[ProcRef]) -> Vec<ProcRef> {
    #[cfg(any(target_os = "linux", windows))]
    {
        Scanner::system().alive(procs)
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = procs;
        Vec::new()
    }
}

/// Diagnostics only: short names in `procs` order. Not part of [`ProcRef`],
/// which is compared for equality.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
pub fn names(procs: &[ProcRef]) -> Vec<String> {
    #[cfg(any(target_os = "linux", windows))]
    {
        let scanner = Scanner::system();
        procs.iter().map(|p| scanner.name_of(*p)).collect()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = procs;
        Vec::new()
    }
}

/// Out-of-band opinion on whether a spec's game is still running.
///
/// Used **only to veto** declaring it gone — never as a primary signal.
/// `Some(true)` hold off; `Some(false)` agrees it is gone; `None` no opinion.
///
/// Linux has none: Steam's launch reaper is already a process the scan sees.
/// Windows has no reaper, which is why a second opinion exists.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
pub fn running_hint(spec: &crate::library::DetectSpec) -> Option<bool> {
    #[cfg(windows)]
    {
        spec.steam_appid.and_then(windows::steam_running_hint)
    }
    #[cfg(not(windows))]
    {
        let _ = spec;
        None
    }
}

/// `roots` and every live process descended from them. A launch command's shell is the root the
/// host holds; the game, and its window, belong to a child of it.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
pub fn with_descendants(roots: &[u32]) -> Vec<u32> {
    #[cfg(any(target_os = "linux", windows))]
    {
        descend(roots, &Scanner::system().parents())
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        roots.to_vec()
    }
}

/// `procs` narrowed to `root` and its descendants.
///
/// A nested lease's whole tree descends from its own gamescope. Two seats can run the same
/// title, and Steam's `SteamLaunch AppId=` reaper looks identical in both, so without this a
/// seat adopts — and its term ladder kills — the other seat's game.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
pub fn under(procs: &[ProcRef], root: u32) -> Vec<ProcRef> {
    under_tree(procs, &with_descendants(&[root]))
}

#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
fn under_tree(procs: &[ProcRef], tree: &[u32]) -> Vec<ProcRef> {
    procs
        .iter()
        .copied()
        .filter(|p| tree.contains(&p.pid))
        .collect()
}

/// `roots` first, then each descendant once, from `(pid, parent)` rows.
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "no scan on macOS"))]
fn descend(roots: &[u32], parents: &[(u32, u32)]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for &r in roots {
        if !out.contains(&r) {
            out.push(r);
        }
    }
    let mut i = 0;
    while i < out.len() {
        let parent = out[i];
        for &(pid, ppid) in parents {
            // Windows' idle entry is pid 0 and parents itself; never follow it.
            if ppid == parent && pid != 0 && !out.contains(&pid) {
                out.push(pid);
            }
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod descend_tests {
    use super::{descend, under_tree, ProcRef};

    /// Two seats, one appid: the scan hits both reapers, and a seat may keep only its own.
    #[test]
    fn a_scoped_scan_drops_the_other_seats_copy_of_the_same_title() {
        let rows = [(11, 10), (12, 11), (21, 20), (22, 21)];
        let p = |pid| ProcRef { pid, start: 1 };
        let hits = [p(12), p(22)];
        let mine = descend(&[10], &rows);
        assert_eq!(under_tree(&hits, &mine), [p(12)]);
        assert_eq!(under_tree(&hits, &descend(&[20], &rows)), [p(22)]);
        assert!(
            under_tree(&hits, &descend(&[99], &rows)).is_empty(),
            "a gone gamescope owns nothing"
        );
    }

    #[test]
    fn a_shells_game_and_its_helpers_are_found_and_strangers_are_not() {
        // sh 10 → wrapper 11 → game 12 → helper 13; 20 is unrelated.
        let rows = [(11, 10), (12, 11), (13, 12), (20, 1), (0, 0)];
        assert_eq!(descend(&[10], &rows), [10, 11, 12, 13]);
        assert_eq!(descend(&[12, 12], &rows), [12, 13]);
        assert_eq!(descend(&[99], &rows), [99], "a gone root is still itself");
    }
}
