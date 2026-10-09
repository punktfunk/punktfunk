//! Linux facts for conflicting-host detection: `/proc` for running processes,
//! systemd unit dirs plus flatpak app dirs plus `PATH` for install markers.
//! Best-effort and spawn-free — a missing `/proc` or unreadable dir yields no evidence.

use super::{Evidence, Known};
use std::path::Path;

/// Lowercased `/proc/<pid>/comm` of every readable pid. `comm` is the kernel's
/// 15-char command name; every host we match fits, so no `/proc/<pid>/exe` readlink.
pub fn running_processes() -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_str()
            .map(|n| n.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false)
        {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            out.push(comm.trim().to_ascii_lowercase());
        }
    }
    out
}

pub fn static_evidence(known: &Known) -> Vec<Evidence> {
    let mut ev = Vec::new();

    // System + user unit dirs systemd actually walks (not just `/etc`).
    let home = std::env::var_os("HOME");
    let mut unit_dirs: Vec<String> = vec![
        "/etc/systemd/system".into(),
        "/run/systemd/system".into(),
        "/usr/lib/systemd/system".into(),
        "/lib/systemd/system".into(),
        "/etc/systemd/user".into(),
        "/usr/lib/systemd/user".into(),
    ];
    if let Some(h) = &home {
        unit_dirs.push(format!("{}/.config/systemd/user", h.to_string_lossy()));
    }
    for unit in known.linux_units {
        let file = format!("{unit}.service");
        if unit_dirs.iter().any(|d| Path::new(d).join(&file).exists()) {
            let autostart = unit_enabled(&file, home.as_deref());
            ev.push(Evidence::Service {
                name: file,
                autostart,
            });
        }
    }

    let mut flatpak_roots: Vec<String> = vec!["/var/lib/flatpak/app".into()];
    if let Some(h) = &home {
        flatpak_roots.push(format!("{}/.local/share/flatpak/app", h.to_string_lossy()));
    }
    for id in known.flatpaks {
        if flatpak_roots.iter().any(|r| Path::new(r).join(id).exists()) {
            ev.push(Evidence::Installed {
                at: format!("flatpak {id}"),
            });
        }
    }

    // PATH covers manual/package installs that have no unit or flatpak.
    let path = std::env::var_os("PATH");
    for bin in known.processes {
        if let Some(found) = find_on_path(bin, path.as_deref()) {
            ev.push(Evidence::Installed { at: found });
        }
    }

    ev
}

/// True when systemd will start `unit` (`<name>.service`) on its own.
///
/// Enablement is a symlink in a target's `.wants`/`.requires` under the drop-in
/// roots systemd reads — no `systemctl` (often no bus here). Do not hardcode
/// `multi-user.target`; any target counts. A unit file with no such link is inert.
fn unit_enabled(unit: &str, home: Option<&std::ffi::OsStr>) -> bool {
    let mut roots: Vec<String> = vec![
        "/etc/systemd/system".into(),
        "/run/systemd/system".into(),
        "/usr/lib/systemd/system".into(),
        "/lib/systemd/system".into(),
        "/etc/systemd/user".into(),
        "/usr/lib/systemd/user".into(),
    ];
    if let Some(h) = home {
        roots.push(format!("{}/.config/systemd/user", h.to_string_lossy()));
    }
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !(name.ends_with(".wants") || name.ends_with(".requires")) {
                continue;
            }
            // `symlink_metadata`: a dangling .wants link still means the operator enabled it.
            if std::fs::symlink_metadata(entry.path().join(unit)).is_ok() {
                return true;
            }
        }
    }
    false
}

fn find_on_path(bin: &str, path: Option<&std::ffi::OsStr>) -> Option<String> {
    let dirs = path.map(std::env::split_paths).into_iter().flatten();
    // Common bindirs even when PATH is unset or narrow (service context).
    let extra = ["/usr/bin", "/usr/local/bin", "/bin", "/usr/games"]
        .into_iter()
        .map(std::path::PathBuf::from);
    for dir in dirs.chain(extra) {
        let cand = dir.join(bin);
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}
