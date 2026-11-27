//! `install-emulator` and `remove-emulator`: one box-wide emulator change, which the supervisor
//! runs as the account that owns the copies ([`pf_seats::linux::emulators`]). hermir installs
//! only a channel's archive build, only a download it checks against a published sha256, and
//! prints the row it records. A Flatpak goes box-wide through `flatpak` itself.

use pf_seats::linux::emulators::{dir, for_every_account, INSTALL_VERB, REMOVE_VERB, USER};
use rustix::fs::Mode;
use std::process::ExitCode;

/// The exit of `verb` for `id`, or `None` when `verb` is not one of these.
pub fn run(verb: &str, id: &str) -> Option<ExitCode> {
    if verb != INSTALL_VERB && verb != REMOVE_VERB {
        return None;
    }
    // An AppImage unpacks by running itself: never as root.
    if rustix::process::geteuid().is_root() {
        eprintln!("{verb}: not as root, the supervisor runs it as {USER}");
        return Some(ExitCode::FAILURE);
    }
    // Every account reads and runs the copies; the account that installs them alone writes.
    rustix::process::umask(Mode::from_raw_mode(0o022));
    Some(match change(verb == INSTALL_VERB, id) {
        Ok(row) => {
            println!("{row}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    })
}

/// Installs or updates `id` and answers its row as JSON, or removes it and answers nothing. A
/// copy that is gone already counts as removed, so the host can drop its row.
fn change(install: bool, id: &str) -> Result<String, String> {
    let h = hermir::Hermir::open(hermir::Options {
        prefix: Some(dir()),
        require_verified: true,
        ..Default::default()
    })
    .map_err(|e| format!("open {}: {e}", dir().display()))?;
    let emulator = h.emulator(id).map_err(|e| e.to_string())?;
    let managed = emulator.managed().map_err(|e| e.to_string())?;
    if !install {
        if managed.is_some() {
            let removed = emulator.remove(false);
            removed.map_err(|e| format!("remove {id}: {e}"))?;
        }
        return Ok(String::new());
    }
    match emulator.entry().channels.get(hermir::Os::Linux) {
        Some(hermir::Channel::Github { .. } | hermir::Channel::Url { .. }) => {}
        _ => return Err(format!("{id} has no archive build for Linux")),
    }
    let quiet = hermir::progress::Quiet;
    let row = match managed {
        Some(row) => emulator.update(&quiet).map(|fresh| fresh.unwrap_or(row)),
        None => emulator.install(&quiet),
    };
    let row = row.map_err(|e| format!("install {id}: {e}"))?;
    let copy = dir().join(id);
    for_every_account(&copy).map_err(|e| format!("set modes in {}: {e}", copy.display()))?;
    serde_json::to_string(&row).map_err(|e| e.to_string())
}
