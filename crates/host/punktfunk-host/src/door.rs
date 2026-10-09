//! **Reachable without logging in** on Linux: the box's host moves from the owner's session to a
//! system service, the door (`serve --door`), and back.
//!
//! This process is unprivileged either way. A root oneshot does the move
//! (`packaging/linux/door-helper`), and polkit lets members of `punktfunk-update` start it:
//! `punktfunk-door-on@<user>.service` from the owner's own host, `punktfunk-door-off@<user>.service`
//! from the door, which knows the owner from its seat ledger. The request returns once systemd has
//! the job; the console polls `GET /host` for `door`.

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

/// Turns the door on (`on`) or off. Blocking: it runs `id` and `systemctl`.
pub(crate) fn change(on: bool) -> Result<Outcome, Refusal> {
    #[cfg(target_os = "linux")]
    {
        linux::change(on)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = on;
        Err((
            StatusCode::CONFLICT,
            "Reachable without logging in is a Linux setting.".into(),
        ))
    }
}

/// The unit that switches the door for `user`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn unit(on: bool, user: &str) -> String {
    format!(
        "punktfunk-door-{}@{user}.service",
        if on { "on" } else { "off" }
    )
}

/// A user name `systemctl` takes as an instance and the helper accepts: lowercase letters,
/// digits, `_`, `-` and `.`, starting with a letter or `_`. Anything else is not started.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn instance_safe(user: &str) -> bool {
    let mut bytes = user.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'.')
        })
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The templates the helper units are installed as.
    const TEMPLATES: [&str; 2] = [
        "/usr/lib/systemd/system/punktfunk-door-on@.service",
        "/etc/systemd/system/punktfunk-door-on@.service",
    ];

    pub(super) fn change(on: bool) -> Result<Outcome, Refusal> {
        if on == pf_paths::seat::is_door() {
            return Ok(Outcome::Already);
        }
        if !TEMPLATES.iter().any(|p| Path::new(p).exists()) {
            return Err((
                StatusCode::CONFLICT,
                "This install doesn't include Reachable without logging in. Update Punktfunk first."
                    .into(),
            ));
        }
        let user = if on { own_user() } else { owner_account() }?;
        if !instance_safe(&user) {
            return Err((
                StatusCode::CONFLICT,
                "This account's name can't be switched automatically.".into(),
            ));
        }
        if on && !in_update_group(&user) {
            return Err((
                StatusCode::FORBIDDEN,
                format!(
                    "Add {user} to the punktfunk-update group first: sudo usermod -aG punktfunk-update {user}"
                ),
            ));
        }
        start(&unit(on, &user))?;
        Ok(Outcome::Started)
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

    /// The owner's account, from the seat ledger. The door has no other way to know whose
    /// files to hand back.
    fn owner_account() -> Result<String, Refusal> {
        crate::seats::snapshot()
            .owner()
            .map(|s| s.account.clone())
            .ok_or((
                StatusCode::CONFLICT,
                "This host has no owner to hand its files back to.".into(),
            ))
    }

    /// Group membership as polkit sees it, by name through NSS, so a fresh `usermod` counts.
    fn in_update_group(user: &str) -> bool {
        capture(Command::new("id").args(["-nG", user]))
            .is_some_and(|groups| groups.split_whitespace().any(|g| g == "punktfunk-update"))
    }

    fn capture(cmd: &mut Command) -> Option<String> {
        let out = cmd.output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `systemctl start --no-block`: polkit decides, and the job runs on after this returns.
    fn start(unit: &str) -> Result<(), Refusal> {
        let out = Command::new("systemctl")
            .args(["start", "--no-block", unit])
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
        let denied = stderr.contains("Access denied")
            || stderr.contains("interactive authentication")
            || stderr.contains("Permission denied");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_names_the_user_and_the_direction() {
        assert_eq!(unit(true, "enrico"), "punktfunk-door-on@enrico.service");
        assert_eq!(unit(false, "j.doe"), "punktfunk-door-off@j.doe.service");
    }

    /// The instance is data the helper re-checks, but nothing odd reaches `systemctl`: a user
    /// name with a slash, a space, an `@` or a leading dash is not started.
    #[test]
    fn only_a_plain_user_name_is_started() {
        for good in ["enrico", "_svc", "a1", "j.doe", "a-b_c"] {
            assert!(instance_safe(good), "{good}");
        }
        for bad in [
            "", "Enrico", "1abc", "-x", "a b", "a/b", "a@b", "a\u{e9}", ".x",
        ] {
            assert!(!instance_safe(bad), "{bad:?}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn off_linux_the_switch_is_a_plain_conflict() {
        let (status, _) = change(true).unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
