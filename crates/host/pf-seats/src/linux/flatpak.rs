//! Box-wide Flatpak apps: one `--system` install every account on the box runs, each with its
//! own `~/.var/app/<id>`, so every seat keeps its own emulator config and saves.
//!
//! The id is checked before it gets here ([`crate::ipc::valid_flatpak_id`]).

use super::run;
use crate::backend::BackendError;

const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";
const CODE: &str = "flatpak_failed";

/// Installs `id` from Flathub, adding the remote first if the box lacks it, and answers the
/// version it reports.
pub(super) fn install(id: &str) -> Result<Option<String>, BackendError> {
    run(
        CODE,
        "flatpak",
        &[
            "remote-add",
            "--system",
            "--if-not-exists",
            "flathub",
            FLATHUB,
        ],
    )?;
    run(
        CODE,
        "flatpak",
        &[
            "install",
            "--system",
            "-y",
            "--noninteractive",
            "flathub",
            id,
        ],
    )?;
    let info = run(CODE, "flatpak", &["info", "--system", id]);
    Ok(info.ok().as_deref().and_then(version))
}

/// Uninstalls `id`. `--delete-data` removes root's `~/.var/app/<id>` only: a seat's stays.
pub(super) fn remove(id: &str, purge: bool) -> Result<(), BackendError> {
    let mut args = vec!["uninstall", "--system", "-y", "--noninteractive"];
    if purge {
        args.push("--delete-data");
    }
    args.push(id);
    run(CODE, "flatpak", &args).map(drop)
}

/// The `Version:` line of `flatpak info`, when it has one.
fn version(info: &str) -> Option<String> {
    info.lines()
        .find_map(|line| line.trim().strip_prefix("Version:"))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_version_is_the_info_s_version_line() {
        let info = "\nPCSX2 - PlayStation 2 Emulator\n\n          ID: net.pcsx2.PCSX2\n         Ref: app/net.pcsx2.PCSX2/x86_64/stable\n     Version: v2.2.0\n      Branch: stable\n";
        assert_eq!(super::version(info).as_deref(), Some("v2.2.0"));
        assert_eq!(super::version("ID: a.b\nVersion: \n"), None);
        assert_eq!(super::version(""), None);
    }
}
