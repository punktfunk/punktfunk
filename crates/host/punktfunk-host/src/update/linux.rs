//! Linux apply: start the `pf-update` root oneshot, read its result, and when the
//! on-disk binary changed, persist intent and restart. Boot reconciliation records
//! the outcome.
//!
//! This process is unprivileged. polkit authorizes `systemctl start
//! punktfunk-update.service` for the `punktfunk-update` group (packaged empty).
//! The request carries nothing: ExecStart is fixed and the helper reads
//! root-owned state. polkit uses NSS at request time, so a fresh `usermod -aG`
//! counts without re-login; [`opted_in`] uses the same NSS route.

#![cfg(target_os = "linux")]

use super::jobs::{self, IntentRecord};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// Root helper outcome. Same shape as `pf-update`'s `HelperResult`.
const HELPER_RESULT: &str = "/var/lib/punktfunk/update-result.json";

/// Unit the polkit rule scopes to. Presence means the helper is installed.
const UNIT_PATH: &str = "/usr/lib/systemd/system/punktfunk-update.service";

/// Pacman full-sysupgrade opt-in. Root-owned.
const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";

/// Cap on one helper run. `pacman -Syu` on a stale box is slow; a stuck manager must still error.
const HELPER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(serde::Deserialize)]
struct HelperResult {
    ok: bool,
    #[serde(default)]
    after_version: String,
    #[serde(default)]
    changed: bool,
    #[serde(default)]
    staged: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    finished_unix: u64,
}

pub(super) fn helper_installed() -> bool {
    Path::new(UNIT_PATH).exists()
}

pub(super) fn pacman_opted_in() -> bool {
    std::fs::read_to_string(PACMAN_OPTIN_CONF)
        .map(|c| c.lines().any(|l| l.trim() == "PACMAN_FULL_SYSUPGRADE=1"))
        .unwrap_or(false)
}

/// Group membership via NSS, matching polkit — not this process's (possibly stale)
/// credentials. Cached; the status endpoint polls this.
pub(super) fn opted_in() -> bool {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<Option<(Instant, bool)>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(None));
    {
        let cached = cache.lock().unwrap();
        if let Some((at, val)) = *cached {
            if at.elapsed() < Duration::from_secs(60) {
                return val;
            }
        }
    }
    let val = probe_group_membership().unwrap_or(false);
    *cache.lock().unwrap() = Some((Instant::now(), val));
    val
}

fn probe_group_membership() -> Option<bool> {
    let user = capture(Command::new("id").arg("-un"))?;
    let groups = capture(Command::new("id").args(["-nG", user.trim()]))?;
    Some(groups.split_whitespace().any(|g| g == "punktfunk-update"))
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The Deck's build tree, or `None` where there is no on-device source build.
fn source_tree() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let dir = Path::new(&home).join(pf_update_check::detect::SOURCE_CHECKOUT);
    dir.join(".git").is_dir().then_some(dir)
}

/// Commits the tracked upstream has and this checkout does not.
///
/// A source build carries no published artifact, so the signed manifest cannot answer
/// "is anything newer" for it — only the remote can. `None` is "git could not answer"
/// (no checkout, a detached tag, an unreachable forge), which keeps the last count
/// rather than reporting a Deck up to date on one bad network moment.
pub(super) fn source_behind() -> Option<u64> {
    let dir = source_tree()?;
    // lowSpeed is git's own timeout: an unreachable forge must not wedge the refresh
    // thread. GIT_TERMINAL_PROMPT stops a credential prompt waiting on a tty the host
    // does not have.
    let fetched = Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args([
            "-c",
            "http.lowSpeedLimit=1000",
            "-c",
            "http.lowSpeedTime=30",
            "fetch",
            "--quiet",
            "--no-tags",
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .ok()?
        .success();
    if !fetched {
        return None;
    }
    capture(Command::new("git").arg("-C").arg(&dir).args([
        "rev-list",
        "--count",
        "HEAD..@{upstream}",
    ]))?
    .trim()
    .parse()
    .ok()
}

/// Deck source rebuild via `systemd-run` so the script's host restart cannot
/// kill a child in our cgroup. Fail: we survive, the unit fails. Success: we
/// die mid-poll; the `source_build` intent at next boot is the signal.
pub(super) fn run_apply_steamos(
    target_version: &str,
    serial: u64,
    stage: &dyn Fn(&'static str),
) -> Result<(), (&'static str, String)> {
    let home = std::env::var("HOME").map_err(|_| ("applying", "no $HOME".to_string()))?;
    let script = std::path::Path::new(&home).join("punktfunk/scripts/steamdeck/update.sh");
    if !script.exists() {
        return Err((
            "applying",
            format!(
                "{} not found — is this the Deck on-device install?",
                script.display()
            ),
        ));
    }
    let log = pf_paths::config_dir()
        .join("logs")
        .join("update-steamos.log");
    if let Some(dir) = log.parent() {
        let _ = std::fs::create_dir_all(dir);
    }

    jobs::write_json_atomic(
        &jobs::intent_path(),
        &IntentRecord {
            from: crate::version::get().into(),
            to: target_version.into(),
            serial,
            started_unix: crate::clock::unix_secs_u64(),
            installer_sha256: String::new(),
            log_path: log.display().to_string(),
            source_build: true,
        },
    )
    .map_err(|e| ("applying", format!("write intent record: {e}")))?;

    const UNIT: &str = "pf-source-update";
    // `--collect` reaps the transient unit even on failure, so a retry can reuse the name.
    let launched = Command::new("systemd-run")
        .args(["--user", "--collect", "--unit", UNIT, "bash", "-c"])
        .arg(format!(
            "exec >> '{}' 2>&1; exec bash '{}' --pull",
            log.display(),
            script.display()
        ))
        .status();
    match launched {
        Ok(s) if s.success() => {}
        Ok(s) => {
            let _ = std::fs::remove_file(jobs::intent_path());
            return Err(("applying", format!("systemd-run exited {s}")));
        }
        Err(e) => {
            let _ = std::fs::remove_file(jobs::intent_path());
            return Err(("applying", format!("launch systemd-run: {e}")));
        }
    }
    stage("applying");

    // Success restarts us before the unit goes inactive. Surviving this loop is fail or still-building.
    let deadline = Instant::now() + Duration::from_secs(90 * 60);
    loop {
        std::thread::sleep(Duration::from_secs(5));
        let state = capture(Command::new("systemctl").args(["--user", "is-active", UNIT]))
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "failed".into());
        match state.as_str() {
            "active" | "activating" | "deactivating" | "reloading" => {
                if Instant::now() > deadline {
                    // Leave the build running. The intent stays for reconcile if it finishes.
                    return Err((
                        "applying",
                        format!(
                            "the source rebuild is still running after 90 min — following it \
                             ends here; see {}",
                            log.display()
                        ),
                    ));
                }
            }
            // Unit ended and we are still alive: the script never restarted us. An
            // up-to-date tree still rebuilds and restarts, so this is a failed build.
            _ => {
                let _ = std::fs::remove_file(jobs::intent_path());
                return Err((
                    "applying",
                    format!("the source rebuild failed — see {}", log.display()),
                ));
            }
        }
    }
}

/// Blocking; run on a blocking thread. An in-place binary change writes intent
/// and restarts us.
pub(super) fn run_apply(
    target_version: &str,
    serial: u64,
    stage: &dyn Fn(&'static str),
) -> Result<(), (&'static str, String)> {
    let started_unix = crate::clock::unix_secs_u64();

    let mut child = Command::new("systemctl")
        .args(["start", "punktfunk-update.service"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| ("applying", format!("launch systemctl: {e}")))?;

    let deadline = Instant::now() + HELPER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() > deadline => {
                let _ = child.kill();
                return Err((
                    "applying",
                    format!(
                        "the update helper is still running after {} min — see \
                         `journalctl -u punktfunk-update.service`",
                        HELPER_TIMEOUT.as_secs() / 60
                    ),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(500)),
            Err(e) => return Err(("applying", format!("wait for systemctl: {e}"))),
        }
    };

    if !status.success() {
        let mut err = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read as _;
            let _ = stderr.read_to_string(&mut err);
        }
        let denial = err.contains("interactive authentication")
            || err.contains("Access denied")
            || err.contains("Permission denied");
        return Err((
            "applying",
            if denial {
                format!(
                    "not authorized to start the update helper — enable web-triggered \
                     updates first: {}",
                    super::OPT_IN_HINT
                )
            } else {
                format!(
                    "update helper failed ({status}) — see \
                     `journalctl -u punktfunk-update.service`. {err}"
                )
            },
        ));
    }

    // Unit exit 0 is not enough: a leftover record from an old run must not count.
    let result: HelperResult = std::fs::read(HELPER_RESULT)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or((
            "applying",
            format!("the update helper wrote no readable result at {HELPER_RESULT}"),
        ))?;
    if result.finished_unix + 5 < started_unix {
        return Err((
            "applying",
            format!(
                "the update helper's result record predates this run \
                 ({} < {started_unix}) — it never wrote one",
                result.finished_unix
            ),
        ));
    }
    if !result.ok {
        return Err((
            "applying",
            result
                .error
                .unwrap_or_else(|| "the update helper reported failure without detail".into()),
        ));
    }

    let current = crate::version::get();
    if result.staged {
        // rpm-ostree: new deployment activates on reboot. Durable now; do not restart.
        let _ = jobs::write_json_atomic(
            &jobs::result_path(),
            &jobs::ResultRecord {
                ok: true,
                from: current.into(),
                to: target_version.into(),
                finished_unix: crate::clock::unix_secs_u64(),
                stage: None,
                error: None,
                log_path: None,
                staged: true,
            },
        );
        return Ok(());
    }
    if !result.changed {
        // Nothing newer on the channel (announce leads the mirrors). Not an error.
        let _ = jobs::write_json_atomic(
            &jobs::result_path(),
            &jobs::ResultRecord {
                ok: true,
                from: current.into(),
                to: current.into(),
                finished_unix: crate::clock::unix_secs_u64(),
                stage: None,
                error: None,
                log_path: None,
                staged: false,
            },
        );
        return Ok(());
    }

    // On-disk binary changed and the helper already proved it runs. Intent crosses the restart.
    let to = result
        .after_version
        .split_whitespace()
        .last()
        .unwrap_or(target_version)
        .to_string();
    jobs::write_json_atomic(
        &jobs::intent_path(),
        &IntentRecord {
            from: current.into(),
            to,
            serial,
            started_unix: crate::clock::unix_secs_u64(),
            installer_sha256: String::new(),
            log_path: "journalctl -u punktfunk-update.service".into(),
            source_build: false,
        },
    )
    .map_err(|e| ("restarting", format!("write intent record: {e}")))?;

    stage("restarting");
    // Web console and plugin runner first (own units), then us. `--no-block`: this process is
    // the one restarting. `try-restart` leaves an opted-out runner off.
    let _ = Command::new("systemctl")
        .args(["--user", "--no-block", "restart", "punktfunk-web.service"])
        .status();
    let _ = Command::new("systemctl")
        .args([
            "--user",
            "--no-block",
            "try-restart",
            "punktfunk-scripting.service",
        ])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "--no-block", "restart", "punktfunk-host.service"])
        .status();
    Ok(())
}
