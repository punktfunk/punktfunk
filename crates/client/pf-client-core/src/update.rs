//! Linux client update check and apply routing.
//!
//! Counterpart to `punktfunk-host::update`. Trust — per-channel Ed25519-signed
//! manifest, validation, "is this newer?" — lives in [`pf_update_check`] so host
//! and client cannot disagree about who may announce a release.
//!
//! This crate is the engine. Decky, GTK About, and `punktfunk-client --check-update`
//! call it; the plugin cannot verify a signature (unprivileged Python, no crypto).
//!
//! Nothing here is privileged. Flatpak updates through flatpak. A packaged client
//! starts the parameterless `pf-update` oneshot (polkit, group `punktfunk-update`).
//! Everything else returns the command. The helper request carries no version, URL,
//! or package name — it derives those from root-owned state.

#![cfg(target_os = "linux")]

use pf_update_check::detect::{self, InstallKind, Product};
use pf_update_check::floor;
use pf_update_check::version::{is_newer, Channel};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The unit the polkit rule scopes to. Its presence is the "helper is installed" probe.
const HELPER_UNIT: &str = "punktfunk-client-update.service";
const HELPER_UNIT_PATH: &str = "/usr/lib/systemd/system/punktfunk-client-update.service";

/// Outcome of `apply-client`. Separate from the host's record so a dual-product box cannot
/// read the other run.
const HELPER_RESULT: &str = "/var/lib/punktfunk/client-update-result.json";

/// Pacman full-sysupgrade hatch (root-owned). Shared with the host: one box, one answer.
const PACMAN_OPTIN_CONF: &str = "/etc/punktfunk/update.conf";

/// Mirrors `packaging/linux/49-punktfunk-client-update.rules`.
const OPT_IN_GROUP: &str = "punktfunk-update";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Apply {
    Full,
    /// A reboot finishes it (rpm-ostree).
    Staged,
    /// Nothing here can install it — show [`Status::command`].
    Notify,
}

/// Who runs the apply. This process can drive the root helper; it cannot update its own
/// flatpak (no host `flatpak` inside the sandbox) — that belongs to whoever launched it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Applier {
    /// The caller runs `flatpak update --user io.unom.Punktfunk`.
    Flatpak,
    /// `punktfunk-client --apply-update` drives the packaged root helper.
    Helper,
    None,
}

/// Check answer as the CLI serialises it for Decky and GTK About.
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub kind: String,
    pub channel: String,
    pub current: String,
    /// Newest on the channel, or `current` when the check could not run.
    pub latest: String,
    pub update_available: bool,
    pub apply: Apply,
    pub applier: Applier,
    /// Copy-pastable install line; always populated.
    pub command: String,
    /// Set when one-click would work after the operator joins the group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opt_in_hint: Option<String>,
    /// Manifest notes URL (forge-validated), empty when unknown.
    pub notes_url: String,
    /// Why the check could not complete. `update_available` is false when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Feed answered, but this channel has no release. Not a malfunction.
    /// Unlike the host's `UpdateStatus`, `error` stays set and `--check-update` exits 1:
    /// an empty channel is not evidence this build is current, and a mistyped
    /// `PUNKTFUNK_UPDATE_FEED` is indistinguishable from here.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub not_published: bool,
}

/// Operator kill switch for checks. Same env name the host honours.
pub fn check_disabled() -> bool {
    crate::env_on("PUNKTFUNK_UPDATE_CHECK") == Some(false)
}

/// Operator kill switch for apply. Status still reports what is available and the hand command.
pub fn apply_disabled() -> bool {
    crate::env_on("PUNKTFUNK_UPDATE_APPLY") == Some(false)
}

/// Client install kind + channel (marker files are per-[`Product`], not shared with the host).
///
/// `current` is a parameter, not `env!`: only the binary is version-stamped
/// (`clients/linux/build.rs` / `PUNKTFUNK_BUILD_VERSION`, including canary `~ciN`).
/// A library `env!` would report the workspace version.
pub fn detect_install(current: &str) -> (InstallKind, Channel) {
    detect::classify(&detect::gather(Product::Client, current), Product::Client)
}

fn helper_installed() -> bool {
    Path::new(HELPER_UNIT_PATH).exists()
}

fn pacman_opted_in() -> bool {
    std::fs::read_to_string(PACMAN_OPTIN_CONF)
        .map(|c| c.lines().any(|l| l.trim() == "PACMAN_FULL_SYSUPGRADE=1"))
        .unwrap_or(false)
}

/// Group membership via NSS, matching polkit — not this process's (possibly stale) credentials,
/// so a fresh `usermod -aG` counts without re-login.
fn opted_in() -> bool {
    let Some(user) = capture(Command::new("id").arg("-un")) else {
        return false;
    };
    let Some(groups) = capture(Command::new("id").args(["-nG", user.trim()])) else {
        return false;
    };
    groups.split_whitespace().any(|g| g == OPT_IN_GROUP)
}

fn capture(cmd: &mut Command) -> Option<String> {
    let out = cmd.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn opt_in_hint() -> String {
    format!("sudo usermod -aG {OPT_IN_GROUP} $USER   # enables one-tap client updates on this box")
}

/// Box capabilities, probed once so [`apply_route`] is a pure function of (kind, caps).
/// Env and root-owned files are read here only — tests can exhaust the routes without a box.
#[derive(Debug, Clone, Copy)]
struct Caps {
    apply_disabled: bool,
    helper: bool,
    opted_in: bool,
    pacman_optin: bool,
}

impl Caps {
    fn probe() -> Self {
        let helper = helper_installed();
        Self {
            apply_disabled: apply_disabled(),
            helper,
            // Skip: both shell out / read root-owned config, and neither can change the answer
            // when no helper is installed.
            opted_in: helper && opted_in(),
            pacman_optin: helper && pacman_opted_in(),
        }
    }
}

fn apply_route(kind: InstallKind, c: Caps) -> (Apply, Applier) {
    if c.apply_disabled {
        return (Apply::Notify, Applier::None);
    }
    let helper_ready = c.helper && c.opted_in;
    match kind {
        // Per-user install the user already owns — no helper, no group.
        InstallKind::Flatpak => (Apply::Full, Applier::Flatpak),
        InstallKind::Apt | InstallKind::Dnf if helper_ready => (Apply::Full, Applier::Helper),
        InstallKind::RpmOstree if helper_ready => (Apply::Staged, Applier::Helper),
        InstallKind::Pacman if helper_ready && c.pacman_optin => (Apply::Full, Applier::Helper),
        // Client sysext has no feed (the signed image is the host). Nix, source, Deck tree: same.
        _ => (Apply::Notify, Applier::None),
    }
}

/// True when joining the group would change [`apply_route`] — drives [`Status::opt_in_hint`].
fn opt_in_would_help(kind: InstallKind, c: Caps) -> bool {
    !c.apply_disabled
        && c.helper
        && !c.opted_in
        && matches!(
            kind,
            InstallKind::Apt | InstallKind::Dnf | InstallKind::RpmOstree | InstallKind::Pacman
        )
}

fn state_path() -> Option<PathBuf> {
    crate::trust::config_dir()
        .ok()
        .map(|d| d.join("client-update-state.json"))
}

/// The floor's write: [`crate::trust::write_atomic`], whose in-place fallback still raises it
/// when rename cannot work.
fn write_floor(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    crate::trust::write_atomic(path, bytes)
}

/// Fetch and verify the channel manifest. Blocking. A failed check sets `error` and
/// `update_available: false` — "could not tell" must never render as "up to date".
pub fn check(current: &str) -> Status {
    let (kind, channel) = detect_install(current);
    let caps = Caps::probe();
    let current = current.to_string();
    let mut status = Status {
        kind: kind.as_str().to_string(),
        channel: channel.as_str().to_string(),
        current: current.clone(),
        latest: current.clone(),
        update_available: false,
        apply: Apply::Notify,
        applier: Applier::None,
        command: detect::update_command(kind, Product::Client),
        opt_in_hint: opt_in_would_help(kind, caps).then(opt_in_hint),
        notes_url: String::new(),
        error: None,
        not_published: false,
    };
    let (apply, applier) = apply_route(kind, caps);
    status.apply = apply;
    status.applier = applier;

    if check_disabled() {
        status.error = Some("update checks are disabled (PUNKTFUNK_UPDATE_CHECK=0)".into());
        return status;
    }

    let manifest = match pf_update_check::feed::fetch_manifest_blocking(
        &pf_update_check::feed::feed_base(),
        channel.as_str(),
        &pf_update_check::pinned_keys(),
        &format!("punktfunk-client/{current} (update-check)"),
    ) {
        Ok(m) => m,
        Err(e) => {
            status.not_published = e.is_not_published();
            status.error = Some(e.to_string());
            return status;
        }
    };

    // Anti-rollback: a validly signed older manifest is an error, not a silent downgrade.
    if let Some(path) = state_path() {
        if let Err(e) = floor::check(&path, channel.as_str(), manifest.serial) {
            status.error = Some(e);
            return status;
        }
        let _ = floor::raise(&path, channel.as_str(), manifest.serial, write_floor);
    }

    status.latest = manifest.version.clone();
    status.notes_url = manifest.notes_url.clone();
    status.update_available = is_newer(
        &manifest.version,
        manifest.ci_run,
        manifest.commit.as_deref(),
        &current,
        channel,
    );
    status
}

#[derive(Debug, Clone, Serialize)]
pub struct ApplyOutcome {
    pub ok: bool,
    /// The package set actually changed on disk.
    pub changed: bool,
    /// Installed, but a reboot activates it (rpm-ostree).
    pub staged: bool,
    pub before: String,
    pub after: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ApplyOutcome {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            changed: false,
            staged: false,
            before: String::new(),
            after: String::new(),
            error: Some(error.into()),
        }
    }
}

#[derive(serde::Deserialize)]
struct HelperResult {
    ok: bool,
    #[serde(default)]
    before_version: String,
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

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Drive the packaged root helper when that is this install's applier. Blocking.
///
/// Refuses every other kind: a flatpak updates only from outside the sandbox, and a client
/// sysext / source build has no feed. Callers read [`Status::applier`] first.
pub fn apply(current: &str) -> ApplyOutcome {
    let (kind, _) = detect_install(current);
    let caps = Caps::probe();
    let (_, applier) = apply_route(kind, caps);
    match applier {
        Applier::Helper => {}
        Applier::Flatpak => {
            return ApplyOutcome::failed(
                "a flatpak client updates from outside its sandbox — run `flatpak update --user \
                 io.unom.Punktfunk` (the Decky plugin does this for you)",
            )
        }
        Applier::None => {
            let hint = opt_in_would_help(kind, caps)
                .then(opt_in_hint)
                .unwrap_or_else(|| detect::update_command(kind, Product::Client));
            return ApplyOutcome::failed(format!(
                "no one-tap update for a `{}` install — {hint}",
                kind.as_str()
            ));
        }
    }

    let started = now_unix();
    let output = Command::new("systemctl")
        .args(["start", HELPER_UNIT])
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => return ApplyOutcome::failed(format!("launch systemctl: {e}")),
    };
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        let denied = err.contains("interactive authentication")
            || err.contains("Access denied")
            || err.contains("Permission denied");
        return ApplyOutcome::failed(if denied {
            format!(
                "not authorized to start the update helper — enable one-tap updates first: {}",
                opt_in_hint()
            )
        } else {
            format!(
                "update helper failed ({}) — see `journalctl -u {HELPER_UNIT}`. {}",
                output.status,
                err.trim()
            )
        });
    }

    // The unit succeeded; the record is ground truth. `finished_unix + 5 < started` means
    // the helper never wrote (5 s covers clock skew) — do not report a previous run.
    let result: HelperResult = match std::fs::read(HELPER_RESULT)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(r) => r,
        None => {
            return ApplyOutcome::failed(format!(
                "the update helper wrote no readable result at {HELPER_RESULT}"
            ))
        }
    };
    if result.finished_unix + 5 < started {
        return ApplyOutcome::failed(format!(
            "the update helper's result record predates this run ({} < {started}) — it never \
             wrote one",
            result.finished_unix
        ));
    }
    if !result.ok {
        return ApplyOutcome::failed(
            result
                .error
                .unwrap_or_else(|| "the update helper reported failure without detail".into()),
        );
    }
    ApplyOutcome {
        ok: true,
        changed: result.changed,
        staged: result.staged,
        before: result.before_version,
        after: result.after_version,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Caps {
        Caps {
            apply_disabled: false,
            helper: true,
            opted_in: true,
            pacman_optin: true,
        }
    }

    /// Kill switch beats every kind, including flatpak (needs no other permission).
    #[test]
    fn kill_switch_forces_notify() {
        let off = Caps {
            apply_disabled: true,
            ..ready()
        };
        for kind in [
            InstallKind::Flatpak,
            InstallKind::Apt,
            InstallKind::Dnf,
            InstallKind::RpmOstree,
            InstallKind::Pacman,
        ] {
            assert_eq!(
                apply_route(kind, off),
                (Apply::Notify, Applier::None),
                "{}",
                kind.as_str()
            );
        }
    }

    /// No feed ⇒ notify, however permissive the box. A button that cannot work is worse.
    #[test]
    fn feedless_kinds_are_notify_only() {
        for kind in [
            InstallKind::Sysext,
            InstallKind::Nix,
            InstallKind::Source,
            InstallKind::SteamosSource,
            InstallKind::WindowsInstaller,
        ] {
            assert_eq!(
                apply_route(kind, ready()),
                (Apply::Notify, Applier::None),
                "{}",
                kind.as_str()
            );
        }
    }

    #[test]
    fn helper_kinds_need_helper_and_opt_in() {
        for kind in [InstallKind::Apt, InstallKind::Dnf, InstallKind::RpmOstree] {
            for caps in [
                Caps {
                    helper: false,
                    ..ready()
                },
                Caps {
                    opted_in: false,
                    ..ready()
                },
            ] {
                assert_eq!(
                    apply_route(kind, caps),
                    (Apply::Notify, Applier::None),
                    "{}",
                    kind.as_str()
                );
            }
            let (tier, applier) = apply_route(kind, ready());
            assert_eq!(applier, Applier::Helper, "{}", kind.as_str());
            let want = if kind == InstallKind::RpmOstree {
                Apply::Staged
            } else {
                Apply::Full
            };
            assert_eq!(tier, want, "{}", kind.as_str());
        }
    }

    /// Pacman also needs the root-owned hatch: a full `-Syu` is a whole-system action.
    #[test]
    fn pacman_needs_its_own_opt_in() {
        assert_eq!(
            apply_route(
                InstallKind::Pacman,
                Caps {
                    pacman_optin: false,
                    ..ready()
                }
            ),
            (Apply::Notify, Applier::None)
        );
        assert_eq!(
            apply_route(InstallKind::Pacman, ready()),
            (Apply::Full, Applier::Helper)
        );
    }

    #[test]
    fn flatpak_needs_no_opt_in_and_is_applied_by_the_caller() {
        let bare = Caps {
            apply_disabled: false,
            helper: false,
            opted_in: false,
            pacman_optin: false,
        };
        assert_eq!(
            apply_route(InstallKind::Flatpak, bare),
            (Apply::Full, Applier::Flatpak)
        );
    }

    #[test]
    fn opt_in_hint_only_where_it_would_help() {
        let not_opted = Caps {
            opted_in: false,
            ..ready()
        };
        assert!(opt_in_would_help(InstallKind::Apt, not_opted));
        assert!(!opt_in_would_help(InstallKind::Sysext, not_opted));
        assert!(!opt_in_would_help(InstallKind::Flatpak, not_opted));
        assert!(!opt_in_would_help(
            InstallKind::Apt,
            Caps {
                helper: false,
                ..not_opted
            }
        ));
        assert!(!opt_in_would_help(InstallKind::Apt, ready()));
    }
}
