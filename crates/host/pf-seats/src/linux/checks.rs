//! What the doctor and the prerequisites read: the checks that seats can run on this machine.
//!
//! [`LinuxBackend::prerequisites`] answers each check with one `Diagnostic`: systemd is the init
//! system, logind runs, the door's group and the `render` group exist, a GPU render node is
//! present, a seat compositor is installed, the account and ACL tools are installed, the seat
//! unit and its scripts are installed, and the box has a host identity.
//!
//! [`LinuxBackend::recheck`] runs them now and keeps the answers. `Seating` reuses the last
//! answers while they are younger than [`CHECKS_FRESH`] ([`cached`]), so a frequent caller doesn't
//! fork `systemctl` and walk `PATH` each time. The doctor and enabling run them afresh.

use super::{accounts, socket};
use super::{run, LinuxBackend, CHECKS_FRESH};
use crate::ipc::Diagnostic;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// `slot`'s checks when they ran less than [`CHECKS_FRESH`] before `now`, else what `run`
/// answers, kept in `slot`.
pub(super) fn cached(
    slot: &mut Option<(Instant, Vec<Diagnostic>)>,
    now: Instant,
    run: impl FnOnce() -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    if let Some((at, checks)) = slot.as_ref()
        && now.duration_since(*at) < CHECKS_FRESH
    {
        return checks.clone();
    }
    let checks = run();
    *slot = Some((now, checks.clone()));
    checks
}

/// `name` on `PATH`, or in the system directories a service's `PATH` may omit.
pub(super) fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/bin", "/sbin", "/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// The programs in `systemctl show`'s `Exec*` lines: each `{ path=<program> ; argv[]=… }`.
fn exec_paths(show: &str) -> Vec<PathBuf> {
    show.split("path=")
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next())
        .map(PathBuf::from)
        .collect()
}

impl LinuxBackend {
    /// Runs the prerequisite checks now and keeps them for `Seating` to reuse.
    pub(super) fn recheck(&self) -> Vec<Diagnostic> {
        let mut slot = self.inner.checks.lock().unwrap_or_else(|e| e.into_inner());
        let checks = self.prerequisites();
        *slot = Some((Instant::now(), checks.clone()));
        checks
    }

    pub(super) fn prerequisites(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        // `ids` are the message ids of `good` and `bad`.
        let mut check = |ok: bool, code: &str, ids: [&str; 2], good: &str, bad: &str| {
            out.push(if ok {
                Diagnostic::info(code, good).worded(ids[0])
            } else {
                Diagnostic::error(code, bad).worded(ids[1])
            });
        };
        check(
            Path::new("/run/systemd/system").is_dir(),
            "systemd",
            ["seatcheck_systemd_ok", "seatcheck_systemd_missing"],
            "systemd is the init system",
            "This machine doesn't run systemd, which seats need.",
        );
        let logind = std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "systemd-logind"])
            .status()
            .is_ok_and(|s| s.success());
        check(
            logind,
            "logind",
            ["seatcheck_logind_ok", "seatcheck_logind_missing"],
            "systemd-logind is running",
            "systemd-logind isn't running, so a seat can't get a session.",
        );
        let group = |name: &str| accounts::group_gid(name).ok().flatten().is_some();
        check(
            group(socket::DOOR_GROUP),
            "door_group",
            ["seatcheck_door_group_ok", "seatcheck_door_group_missing"],
            "The punktfunk-door group exists",
            "The punktfunk-door group doesn't exist; reinstall the package that creates it.",
        );
        check(
            group("render"),
            "render_group",
            [
                "seatcheck_render_group_ok",
                "seatcheck_render_group_missing",
            ],
            "The render group exists",
            "The render group doesn't exist, so a seat can't use the graphics card.",
        );
        let render_node = std::fs::read_dir("/dev/dri").is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        });
        check(
            render_node,
            "render_node",
            ["seatcheck_render_node_ok", "seatcheck_render_node_missing"],
            "A GPU render node is present",
            "No graphics card render node was found, so seats can't stream.",
        );
        check(
            which("kwin_wayland").is_some() || which("gamescope").is_some(),
            "compositor",
            ["seatcheck_compositor_ok", "seatcheck_compositor_missing"],
            "A seat compositor is installed",
            "Neither kwin_wayland nor gamescope is installed.",
        );
        let missing: Vec<&str> = [
            "useradd",
            "userdel",
            "usermod",
            "setfacl",
            "loginctl",
            "systemctl",
            "setpriv",
            "cp",
        ]
        .into_iter()
        .filter(|tool| which(tool).is_none())
        .collect();
        check(
            missing.is_empty(),
            "tools",
            ["seatcheck_tools_ok", "seatcheck_tools_missing"],
            "The account and ACL tools are installed",
            &format!("These tools are missing: {}.", missing.join(", ")),
        );
        // What systemd would run, wherever the package put it.
        let installed = run(
            "seat_unit",
            "systemctl",
            &[
                "show",
                "punktfunk-seat@doctor.service",
                "-p",
                "LoadState",
                "-p",
                "ExecStart",
                "-p",
                "ExecStopPost",
            ],
        )
        .is_ok_and(|show| {
            let programs = exec_paths(&show);
            show.lines().any(|l| l == "LoadState=loaded")
                && programs.len() >= 2
                && programs.iter().all(|p| p.is_file())
        });
        check(
            installed,
            "seat_unit",
            ["seatcheck_seat_unit_ok", "seatcheck_seat_unit_missing"],
            "The seat unit and its scripts are installed",
            "The seat unit or its scripts are missing; reinstall the host package.",
        );
        let box_dir = &self.inner.box_dir;
        check(
            box_dir.join("native-cert.pem").is_file() && box_dir.join("native-key.pem").is_file(),
            "box_identity",
            [
                "seatcheck_box_identity_ok",
                "seatcheck_box_identity_missing",
            ],
            "The box has a host identity",
            "The box has no host identity yet; start its host once.",
        );
        if let Some(row) = out.iter_mut().find(|row| row.code == "tools") {
            row.params.insert("tools".into(), missing.join(", "));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_reads_the_programs_systemd_would_run() {
        let show = "LoadState=loaded\n\
            ExecStart={ path=/nix/store/abc-seats/libexec/punktfunk/seat-session ; argv[]=/nix/store/abc-seats/libexec/punktfunk/seat-session ; ignore_errors=no }\n\
            ExecStopPost={ path=/nix/store/abc-seats/libexec/punktfunk/seat-reap ; argv[]=x %i ; ignore_errors=no }\n";
        assert_eq!(
            super::exec_paths(show),
            [
                std::path::PathBuf::from("/nix/store/abc-seats/libexec/punktfunk/seat-session"),
                std::path::PathBuf::from("/nix/store/abc-seats/libexec/punktfunk/seat-reap"),
            ]
        );
        assert!(super::exec_paths("LoadState=not-found\nExecStart=\n").is_empty());
    }

    /// The checks run once, then are reused until they are `CHECKS_FRESH` old.
    #[test]
    fn the_seating_checks_are_reused_while_they_are_fresh() {
        let (mut slot, start) = (None, Instant::now());
        let mut runs = 0;
        let mut ask = |slot: &mut _, at: Instant| {
            cached(slot, at, || {
                runs += 1;
                vec![Diagnostic::info("run", runs.to_string())]
            })
        };
        assert_eq!(ask(&mut slot, start)[0].message, "1");
        assert_eq!(ask(&mut slot, start + CHECKS_FRESH / 2)[0].message, "1");
        assert_eq!(ask(&mut slot, start + CHECKS_FRESH)[0].message, "2");
        assert_eq!(ask(&mut slot, start + CHECKS_FRESH * 3 / 2)[0].message, "2");
    }

    #[test]
    fn a_tool_is_found_on_the_path_and_a_missing_one_is_not() {
        assert!(which("sh").is_some());
        assert!(which("no-such-tool-punktfunk").is_none());
    }
}
