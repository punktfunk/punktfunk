//! macOS facts for conflicting-host detection: libproc for running processes, launchd job
//! plists for autostart, and app bundles plus `PATH`, Homebrew and MacPorts binaries for
//! installs. Spawn-free; an unreadable dir or pid yields no evidence.

use super::{find_on_path, Evidence, Known};
use std::path::PathBuf;

/// Where Homebrew (Apple silicon, Intel) and MacPorts link binaries, read even when the
/// launchd `PATH` leaves them out.
const BIN_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"];

/// Lowercased short name of every process this user may see. libproc's name is the
/// executable's, up to 32 bytes; every host we match fits.
pub fn running_processes() -> Vec<String> {
    // SAFETY: a NULL buffer of size 0 asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for processes started between the two calls; a later one is missed.
    let mut pids: Vec<libc::c_int> = vec![0; count + 64];
    let bytes = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())).unwrap_or(0);
    // SAFETY: `pids` is `bytes` writable bytes of `c_int`; the call writes at most that many and
    // returns how many pids it wrote.
    let written = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(written).unwrap_or(0));
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(process_name)
        .collect()
}

fn process_name(pid: libc::c_int) -> Option<String> {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is `buf.len()` writable bytes; `proc_name` writes at most that many and
    // returns the name's length, 0 when the pid is gone or hidden.
    let len = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let name = buf.get(..usize::try_from(len).ok().filter(|&l| l > 0)?)?;
    Some(String::from_utf8_lossy(name).to_ascii_lowercase())
}

pub fn static_evidence(known: &Known) -> Vec<Evidence> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut ev = Vec::new();

    let mut job_dirs = vec![
        PathBuf::from("/Library/LaunchAgents"),
        PathBuf::from("/Library/LaunchDaemons"),
    ];
    job_dirs.extend(home.iter().map(|h| h.join("Library/LaunchAgents")));
    for dir in &job_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let file = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = file.strip_suffix(".plist")
                && names_a_process(stem, known.processes)
            {
                // launchd loads every job plist here at login. One turned off with
                // `launchctl disable` still reads as autostart: that state is launchd's own.
                ev.push(Evidence::Service {
                    name: file,
                    autostart: true,
                });
            }
        }
    }

    let bundle = format!("{}.app", known.product.label());
    let mut app_dirs = vec![PathBuf::from("/Applications")];
    app_dirs.extend(home.iter().map(|h| h.join("Applications")));
    for app in app_dirs.iter().map(|d| d.join(&bundle)) {
        if app.is_dir() {
            ev.push(Evidence::Installed {
                at: app.to_string_lossy().into_owned(),
            });
        }
    }

    let path = std::env::var_os("PATH");
    for bin in known.processes {
        if let Some(found) = find_on_path(bin, path.as_deref(), BIN_DIRS) {
            ev.push(Evidence::Installed { at: found });
        }
    }
    ev
}

/// A launchd label names a host when one of its dot-, dash- or underscore-separated words is
/// that host's process name: `homebrew.mxcl.sunshine`, not `com.example.apollographql`.
fn names_a_process(label: &str, processes: &[&str]) -> bool {
    label
        .split(['.', '-', '_'])
        .any(|word| processes.iter().any(|p| word.eq_ignore_ascii_case(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launchd_label_names_a_host_by_a_whole_word() {
        let sunshine = &["sunshine"];
        assert!(names_a_process("homebrew.mxcl.sunshine", sunshine));
        assert!(names_a_process("dev.lizardbyte.app.Sunshine", sunshine));
        assert!(!names_a_process("com.example.sunshineweather", sunshine));
        assert!(!names_a_process("com.apollographql.router", &["apollo"]));
    }

    /// The listing answers without a spawn, and includes this test process.
    #[test]
    fn running_processes_includes_this_one() {
        let procs = running_processes();
        let me = process_name(std::process::id() as libc::c_int).expect("own name");
        assert!(procs.contains(&me), "{me} not in {} processes", procs.len());
    }
}
