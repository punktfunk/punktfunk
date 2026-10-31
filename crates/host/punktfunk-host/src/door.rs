//! **Reachable without logging in** on Linux: the box's host moves from the owner's session to a
//! system service, the door (`serve --door`), and back.
//!
//! This process is unprivileged either way. A root oneshot does the move
//! (`packaging/linux/door-helper`), and polkit lets members of `punktfunk-update` start it:
//! `punktfunk-door-on@<user>.service` from the owner's own host, `punktfunk-door-off@<user>.service`
//! from the door, which reads the owner from the helper's record. The request returns once systemd
//! has the job; the console polls `GET /host` for `door` and `door_switch_failed`, and reads
//! `door_unavailable` before it asks.

use crate::update::detect::InstallKind;
use axum::http::StatusCode;

/// A refusal: the status and its sentence for the operator.
pub(crate) type Refusal = (StatusCode, String);

/// What a request to switch the door found.
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
pub(crate) enum Outcome {
    /// The door was already as asked.
    Already,
    /// The switch is under way.
    Started,
}

#[cfg(not(target_os = "linux"))]
const LINUX_ONLY: &str = "Reachable without logging in is a Linux setting.";

/// Turns the door on (`on`) or off. Blocking: it runs `id` and `systemctl`.
pub(crate) fn change(on: bool) -> Result<Outcome, Refusal> {
    #[cfg(target_os = "linux")]
    {
        linux::change(on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on;
        Err((StatusCode::CONFLICT, LINUX_ONLY.into()))
    }
}

/// Why turning the door on would be refused right now, as the operator's next move. `None` when
/// it would start, and on the door itself. Blocking: it runs `id`.
pub(crate) fn unavailable() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux::unavailable()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Some(LINUX_ONLY.into())
    }
}

/// Whether the switch this host last started ran and failed: `door-helper` refused, or stopped
/// halfway and put the box back. The helper's reason is in the host's log. Blocking: it runs
/// `systemctl`.
pub(crate) fn switch_failed() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::switch_failed()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// What to do on an install whose door files are missing. Each channel gets them its own way.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn missing(kind: InstallKind) -> String {
    let package = |command: &str| {
        format!("This install is missing the punktfunk-seats package. Install it: {command}")
    };
    match kind {
        InstallKind::Apt => package("sudo apt install punktfunk-seats"),
        InstallKind::Dnf => package("sudo dnf install punktfunk-seats"),
        InstallKind::Pacman => package("sudo pacman -S punktfunk-seats"),
        InstallKind::RpmOstree => "This install is missing the punktfunk-seats package. Update \
                                   Punktfunk, then restart: sudo /usr/share/punktfunk/update-punktfunk.sh"
            .into(),
        InstallKind::Sysext => {
            "This image doesn't include it yet. Update Punktfunk: sudo punktfunk-sysext update".into()
        }
        InstallKind::Nix => "Turn it on in your NixOS configuration with \
                             services.punktfunk.seats.enable = true, then rebuild."
            .into(),
        InstallKind::SteamosSource => "This Deck hasn't set it up yet. Run bash \
                                       ~/punktfunk/scripts/steamdeck/update.sh in Konsole and \
                                       enter your password."
            .into(),
        InstallKind::Source | InstallKind::Flatpak | InstallKind::WindowsInstaller => {
            "This build doesn't include it. Install Punktfunk from a package to use it.".into()
        }
    }
}

/// Whether a Deck's system copy, by its version file, isn't the build this host runs. A console
/// update moves only this host, and the door would come up on the older copy. A checkout-less
/// build records an empty version, which proves nothing.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn copy_is_stale(recorded: Option<&str>, running: &str) -> bool {
    recorded
        .map(str::trim)
        .is_some_and(|v| !v.is_empty() && v != running)
}

/// The unit that switches the door for `user`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn unit(on: bool, user: &str) -> String {
    format!(
        "punktfunk-door-{}@{user}.service",
        if on { "on" } else { "off" }
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The templates the helper units are installed as: a package's, or an `/etc` copy (NixOS,
    /// the SteamOS on-device build).
    const TEMPLATES: [&str; 2] = [
        "/usr/lib/systemd/system/punktfunk-door-on@.service",
        "/etc/systemd/system/punktfunk-door-on@.service",
    ];

    pub(super) fn change(on: bool) -> Result<Outcome, Refusal> {
        if on == pf_paths::seat::role().is_door() {
            return Ok(Outcome::Already);
        }
        let user = if on { own_user() } else { owner_account() }?;
        if let Some(refusal) = refusal(on, &user) {
            return Err(refusal);
        }
        let unit = unit(on, &user);
        let before = invocation(&unit).unwrap_or_default();
        start(&unit)?;
        *STARTED.lock().unwrap_or_else(|e| e.into_inner()) = Some((unit, before));
        Ok(Outcome::Started)
    }

    /// The switch unit this host last started, and its invocation before then: a newer one is
    /// that start's run.
    static STARTED: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);

    pub(super) fn switch_failed() -> bool {
        let Some((unit, before)) = STARTED.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return false;
        };
        let Some(shown) = capture(Command::new("systemctl").args([
            "show",
            "-p",
            "InvocationID",
            "-p",
            "Result",
            &unit,
        ])) else {
            return false;
        };
        ran_and_failed(&shown, &before)
    }

    /// `systemctl show -p InvocationID -p Result`'s `before`'s successor, ended without success.
    pub(super) fn ran_and_failed(shown: &str, before: &str) -> bool {
        let field = |key: &str| {
            shown
                .lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .unwrap_or_default()
        };
        let run = field("InvocationID");
        !run.is_empty() && run != before && field("Result") != "success"
    }

    /// `unit`'s current invocation id, empty before its first run.
    fn invocation(unit: &str) -> Option<String> {
        let shown = capture(Command::new("systemctl").args(["show", "-p", "InvocationID", unit]))?;
        Some(shown.trim().strip_prefix("InvocationID=")?.to_string())
    }

    pub(super) fn unavailable() -> Option<String> {
        if pf_paths::seat::role().is_door() {
            return None;
        }
        match own_user() {
            Ok(user) => refusal(true, &user),
            Err(refusal) => Some(refusal),
        }
        .map(|(_, sentence)| sentence)
    }

    /// What stops switching the door for `user`, checked in the order the operator fixes it.
    fn refusal(on: bool, user: &str) -> Option<Refusal> {
        if !TEMPLATES.iter().any(|p| Path::new(p).exists()) {
            return Some((
                StatusCode::CONFLICT,
                missing(crate::update::detect::detect().0),
            ));
        }
        let copy =
            Path::new(pf_update_check::detect::STEAMOS_SYSTEM_COPY).with_file_name("version");
        let recorded = std::fs::read_to_string(copy).ok();
        if on && copy_is_stale(recorded.as_deref(), crate::version::get()) {
            return Some((
                StatusCode::CONFLICT,
                "This Deck's seats are on an older build than its host. Run bash \
                 ~/punktfunk/scripts/steamdeck/update.sh in Konsole and enter your password, \
                 then turn this on."
                    .into(),
            ));
        }
        // The helper's own rule, so the console says so before it asks.
        if let Err(refused) = pf_seats::linux::check_owner(user, on) {
            return Some(match refused.code.as_str() {
                "account_invalid" => (
                    StatusCode::CONFLICT,
                    "This account's name can't be switched automatically.".into(),
                ),
                "not_in_update_group" => (
                    StatusCode::FORBIDDEN,
                    format!(
                        "Add {user} to the punktfunk-update group first: sudo usermod -aG punktfunk-update {user}"
                    ),
                ),
                _ => (
                    StatusCode::CONFLICT,
                    "This account can't be switched automatically.".into(),
                ),
            });
        }
        if on && !in_update_group(Command::new("id").arg("-nG")) {
            return Some((
                StatusCode::FORBIDDEN,
                format!(
                    "Restart the computer to finish adding {user} to the punktfunk-update group. The host started before it."
                ),
            ));
        }
        None
    }

    /// The user this host runs as.
    fn own_user() -> Result<String, Refusal> {
        capture(Command::new("id").arg("-un"))
            .map(|s| s.trim().to_string())
            .ok_or_else(|| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Couldn't tell which account this host runs as.".into(),
                )
            })
    }

    /// The owner's account as `door-helper` recorded it, the one account it switches off for.
    /// Read from root's record, so a supervisor that isn't answering doesn't keep the door on.
    fn owner_account() -> Result<String, Refusal> {
        pf_seats::linux::recorded_owner().ok_or((
            StatusCode::CONFLICT,
            "This host has no owner to hand its files back to.".into(),
        ))
    }

    /// `punktfunk-update` among this process's groups. polkit judges the process, whose groups
    /// are fixed when the user manager starts, so a fresh `usermod` counts only after a restart.
    fn in_update_group(id: &mut Command) -> bool {
        capture(id).is_some_and(|groups| groups.split_whitespace().any(|g| g == "punktfunk-update"))
    }

    fn capture(cmd: &mut Command) -> Option<String> {
        let out = cmd.output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `systemctl start --no-block`: polkit decides, and the job runs on after this returns.
    /// `--no-ask-password`: a refusal answers at once instead of a password prompt on the box's
    /// own desktop that nobody streaming sees.
    fn start(unit: &str) -> Result<(), Refusal> {
        let out = Command::new("systemctl")
            .args(["start", "--no-block", "--no-ask-password", unit])
            .output()
            .map_err(|e| {
                tracing::warn!(%unit, error = %e, "door switch did not launch");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Couldn't start the switch. Check that systemd is running.".to_string(),
                )
            })?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        tracing::warn!(%unit, %stderr, "door switch refused");
        let said = stderr.to_ascii_lowercase();
        let denied = said.contains("access denied")
            || said.contains("interactive authentication")
            || said.contains("permission denied");
        Err(if denied {
            (
                StatusCode::FORBIDDEN,
                "This account isn't allowed to switch it. Add it to the punktfunk-update group, then try again."
                    .to_string(),
            )
        } else {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Couldn't start the switch. Check the host log.".to_string(),
            )
        })
    }
}

/// The key the door's console also accepts a console handoff under: the owner's host's token,
/// which the owner's own `ctl console-url` signs with. Never a contract seat's, whose user can
/// read their own. Kept level with the owner's row for the door's lifetime; absent without one.
#[cfg(target_os = "linux")]
pub(crate) const OWNER_KEY_FILE: &str = "owner-handoff-key";

/// Keeps [`OWNER_KEY_FILE`] in the door's directory level with the owner's row's token. A tick
/// the supervisor doesn't answer leaves the key as it is.
#[cfg(target_os = "linux")]
pub(crate) fn publish_owner_key() {
    let spawned = std::thread::Builder::new()
        .name("door-owner-key".into())
        .spawn(|| {
            let path = pf_paths::config_dir().join(OWNER_KEY_FILE);
            let mut last: Option<String> = None;
            loop {
                let ledger = crate::seats::ledger();
                let token = match ledger.owner() {
                    Ok(owner) => owner.and_then(crate::seats::token_of),
                    Err(_) => last.clone(),
                };
                if token != last {
                    let written = match &token {
                        Some(token) => pf_paths::replace_secret_file(&path, token.as_bytes()),
                        None => std::fs::remove_file(&path).or_else(|e| {
                            if e.kind() == std::io::ErrorKind::NotFound {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        }),
                    };
                    match written {
                        Ok(()) => last = token,
                        Err(error) => tracing::warn!(%error, "owner's handoff key not written"),
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(5));
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "owner's handoff key not kept");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_names_the_user_and_the_direction() {
        assert_eq!(unit(true, "enrico"), "punktfunk-door-on@enrico.service");
        assert_eq!(unit(false, "j_doe"), "punktfunk-door-off@j_doe.service");
        assert_eq!(
            unit(true, "jdoe@corp.example.com"),
            "punktfunk-door-on@jdoe@corp.example.com.service"
        );
    }

    /// A missing door names the fix for the channel the box came from, never "update" where an
    /// update would not bring it.
    #[test]
    fn a_missing_door_names_the_fix_for_its_channel() {
        for (kind, fix) in [
            (InstallKind::Apt, "sudo apt install punktfunk-seats"),
            (InstallKind::Dnf, "sudo dnf install punktfunk-seats"),
            (InstallKind::Pacman, "sudo pacman -S punktfunk-seats"),
            (InstallKind::RpmOstree, "update-punktfunk.sh"),
            (InstallKind::Sysext, "sudo punktfunk-sysext update"),
            (InstallKind::Nix, "services.punktfunk.seats.enable = true"),
            (InstallKind::SteamosSource, "scripts/steamdeck/update.sh"),
            (InstallKind::Source, "from a package"),
        ] {
            let sentence = missing(kind);
            assert!(sentence.contains(fix), "{kind:?}: {sentence}");
            assert!(!sentence.contains("  "), "{kind:?}: {sentence}");
        }
    }

    /// The copy turning the door on would run: behind or ahead of this host is stale, and no
    /// file or an empty one, from a build outside a checkout, proves nothing.
    #[test]
    fn only_a_copy_of_another_build_is_stale() {
        let host = "0.46.0+g8d15756b1";
        assert!(copy_is_stale(Some("0.46.0+gc06e61a0b\n"), host));
        assert!(!copy_is_stale(Some("0.46.0+g8d15756b1\n"), host));
        assert!(!copy_is_stale(Some("\n"), "0.45.0"));
        assert!(!copy_is_stale(None, host));
    }

    /// A run after the start that ended badly is a failure; the run before it, or one still
    /// going, is not.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_later_run_that_ended_badly_is_a_failed_switch() {
        use super::linux::ran_and_failed;
        let shown = |id: &str, result: &str| format!("InvocationID={id}\nResult={result}\n");
        assert!(ran_and_failed(&shown("b2", "exit-code"), "a1"));
        assert!(
            !ran_and_failed(&shown("a1", "exit-code"), "a1"),
            "the old run"
        );
        assert!(!ran_and_failed(&shown("b2", "success"), "a1"));
        assert!(!ran_and_failed(&shown("", "success"), ""), "not run yet");
        assert!(ran_and_failed(&shown("b2", "exit-code"), ""), "a first run");
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_the_switch_is_a_plain_conflict() {
        let (status, _) = change(true).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(unavailable().as_deref(), Some(LINUX_ONLY));
    }
}
