//! Windows facts for conflicting-host detection: Toolhelp for running processes,
//! the SCM for registered services, `%ProgramFiles%` for on-disk installs.
//! Best-effort — privilege or API failure yields no evidence, never aborts startup.

use super::{Evidence, Known};
use windows_service::service::{ServiceAccess, ServiceStartType};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

/// Lowercased executable basenames (no `.exe`) from a Toolhelp snapshot.
pub fn running_processes() -> Vec<String> {
    crate::procscan::processes()
        .into_iter()
        .map(|(_, _, exe)| {
            let name = exe.to_ascii_lowercase();
            name.strip_suffix(".exe").unwrap_or(&name).to_string()
        })
        .collect()
}

pub fn static_evidence(known: &Known) -> Vec<Evidence> {
    let mut ev = Vec::new();
    for svc in known.win_services {
        if let Some(autostart) = service_start_type(svc) {
            ev.push(Evidence::Service {
                name: (*svc).to_string(),
                autostart,
            });
        }
    }
    for dir in known.win_dirs {
        if let Some(at) = program_files_dir(dir) {
            ev.push(Evidence::Installed { at });
        }
    }
    ev
}

/// `Some(autostart)` if the SCM has this service, `None` if it does not.
///
/// `autostart` is boot/system/auto only — those come up alone and can take the
/// GameStream ports. Missing `QUERY_CONFIG` reports dormant, not autostart:
/// a false alarm is worse than a miss, and a live host still hits the process scan.
fn service_start_type(name: &str) -> Option<bool> {
    let mgr = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let svc = mgr
        .open_service(
            name,
            ServiceAccess::QUERY_CONFIG | ServiceAccess::QUERY_STATUS,
        )
        // Status-only if `QUERY_CONFIG` is denied: still present (dormant), not absent.
        .or_else(|_| mgr.open_service(name, ServiceAccess::QUERY_STATUS))
        .ok()?;
    let autostart = svc.query_config().is_ok_and(|c| {
        matches!(
            c.start_type,
            ServiceStartType::AutoStart
                | ServiceStartType::BootStart
                | ServiceStartType::SystemStart
        )
    });
    Some(autostart)
}

/// Under `ProgramFiles`, `ProgramW6432`, or `ProgramFiles(x86)` — WOW64 vs 32-bit hosts differ.
fn program_files_dir(dir: &str) -> Option<String> {
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(base) = std::env::var_os(var) {
            let p = std::path::Path::new(&base).join(dir);
            if p.is_dir() {
                return Some(p.to_string_lossy().into_owned());
            }
        }
    }
    None
}
