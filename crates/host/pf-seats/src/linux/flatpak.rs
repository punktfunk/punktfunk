//! Box-wide Flatpak apps: one `--system` install that every account on the box runs. Each
//! account has its own `~/.var/app/<id>`, so every seat keeps its own emulator config and saves.
//!
//! The id is checked before it gets here ([`crate::ipc::valid_flatpak_id`]).

use super::run;
use crate::backend::BackendError;

const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";
const CODE: &str = "flatpak_failed";

/// Installs the app `id` from Flathub, or updates it when it is there, adding the remote first if
/// the box lacks it, and answers the version it reports. `--app` refuses an id that names a
/// runtime, as [`remove`] does.
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
            "--app",
            "--or-update",
            "-y",
            "--noninteractive",
            "flathub",
            id,
        ],
    )?;
    let info = run(CODE, "flatpak", &["info", "--system", id]);
    Ok(info.ok().as_deref().and_then(version))
}

/// Uninstalls the app `id`. `--app` leaves a runtime of that name installed: the owner's
/// account can ask for any id, and the other accounts' apps run on the runtimes.
/// `--delete-data` removes root's `~/.var/app/<id>` only: a seat's stays.
pub(super) fn remove(id: &str, purge: bool) -> Result<(), BackendError> {
    let mut args = vec!["uninstall", "--system", "--app", "-y", "--noninteractive"];
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
    use super::super::fake;

    #[test]
    fn a_removal_names_an_app_so_a_runtime_stays() {
        let _machine = fake::install(|_, _| (0, String::new()));
        super::remove("net.pcsx2.PCSX2", true).unwrap();
        assert_eq!(
            fake::calls(),
            ["flatpak uninstall --system --app -y --noninteractive --delete-data net.pcsx2.PCSX2"]
        );
    }

    #[test]
    fn an_install_names_an_app_so_a_runtime_is_refused() {
        let _machine = fake::install(|_, _| (0, String::new()));
        super::install("net.pcsx2.PCSX2").unwrap();
        assert_eq!(
            fake::calls()[1],
            "flatpak install --system --app --or-update -y --noninteractive flathub net.pcsx2.PCSX2"
        );
    }

    #[test]
    fn the_version_is_the_info_s_version_line() {
        let info = "\nPCSX2 - PlayStation 2 Emulator\n\n          ID: net.pcsx2.PCSX2\n         Ref: app/net.pcsx2.PCSX2/x86_64/stable\n     Version: v2.2.0\n      Branch: stable\n";
        assert_eq!(super::version(info).as_deref(), Some("v2.2.0"));
        assert_eq!(super::version("ID: a.b\nVersion: \n"), None);
        assert_eq!(super::version(""), None);
    }
}
