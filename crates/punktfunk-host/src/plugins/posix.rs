//! The plugin runner off Windows: a `punktfunk-scripting` wrapper found by rung, driven as a
//! systemd USER unit on Linux. Other Unix hosts resolve the runner but cannot run it.

use super::*;

#[cfg(target_os = "linux")]
const UNIT: &str = "punktfunk-scripting";

/// Wrapper name every non-Windows package installs (`/usr/bin`, `~/.local/bin`, `$out/bin`).
const RUNNER_BIN: &str = "punktfunk-scripting";

/// The bun the deb and pacman packages share between the console and the runner (`punktfunk-bun`).
const PACKAGED_BUN: &str = "/usr/lib/punktfunk-bun/bun";

/// No elevation bar off Windows: the runner is the operator's own user unit.
pub(super) fn require_elevation(_what: &str) -> Result<()> {
    Ok(())
}

pub(super) fn usage_note() {}

pub(super) fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
    let exe = std::env::current_exe().ok();
    let path_var = std::env::var("PATH").ok();
    let home = std::env::var("HOME").ok();
    resolve_runner_in(
        std::env::var("PUNKTFUNK_SCRIPTING").ok().as_deref(),
        exe.as_deref().and_then(std::path::Path::parent),
        path_var.as_deref(),
        home.as_deref().map(std::path::Path::new),
        &|p| p.is_file(),
    )
    .ok_or_else(|| anyhow::anyhow!("{RUNNER_MISSING}"))
}

/// Shared with [`runtime_status`] so CLI and console say the same thing.
pub(super) const RUNNER_MISSING: &str =
    "the plugin runner isn't installed — install it first (Debian/Ubuntu: `sudo apt install \
     punktfunk-scripting`; SteamOS: re-run scripts/steamdeck/install.sh; NixOS: enable \
     `services.punktfunk.scripting`). If it is installed somewhere else, point PUNKTFUNK_SCRIPTING \
     at the punktfunk-scripting executable.";

/// Rungs: `PUNKTFUNK_SCRIPTING` → beside the host → `PATH` → `/usr` → `~/.local`.
/// Injected so tests do not mutate process env (races `getenv` in parallel).
///
/// `PATH` is the only rung a Nix install can land on: `punktfunk-scripting` is its
/// own derivation, neither beside the host nor under `/usr`.
fn resolve_runner_in(
    env: Option<&str>,
    exe_dir: Option<&std::path::Path>,
    path_var: Option<&str>,
    home: Option<&std::path::Path>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> Option<(std::path::PathBuf, Vec<String>)> {
    use std::path::{Path, PathBuf};

    // Two-file layout (private bun + runner bundle). A rung only when both exist.
    let pair = |bun: PathBuf, runner: PathBuf| -> Option<(PathBuf, Vec<String>)> {
        (exists(&bun) && exists(&runner))
            .then(|| (bun, vec![runner.to_string_lossy().into_owned()]))
    };

    // Operator override: not existence-checked, so a typo fails naming that path
    // instead of silently using some other installed runner.
    if let Some(v) = env.map(str::trim).filter(|v| !v.is_empty()) {
        return Some((PathBuf::from(v), Vec::new()));
    }
    if let Some(p) = exe_dir.map(|d| d.join(RUNNER_BIN)).filter(|p| exists(p)) {
        return Some((p, Vec::new()));
    }
    if let Some(p) = path_var
        .into_iter()
        .flat_map(|v| v.split(':'))
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(RUNNER_BIN))
        .find(|p| exists(p))
    {
        return Some((p, Vec::new()));
    }
    // Packaged `/usr` after `PATH`: a systemd unit PATH may omit `/usr/bin`.
    let wrapper = Path::new("/usr/bin").join(RUNNER_BIN);
    if exists(&wrapper) {
        return Some((wrapper, Vec::new()));
    }
    if let Some(cmd) = pair(
        PathBuf::from(PACKAGED_BUN),
        Path::new("/usr/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js"),
    ) {
        return Some(cmd);
    }
    // Immutable `/usr` (SteamOS): the same payload, user-scoped under `~/.local`.
    let home = home?;
    let wrapper = home.join(".local/bin").join(RUNNER_BIN);
    if exists(&wrapper) {
        return Some((wrapper, Vec::new()));
    }
    pair(
        home.join(".local/lib").join(RUNNER_BIN).join("bun"),
        home.join(".local/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js"),
    )
}

/// Nothing to grant off Windows: the runner is a systemd USER unit, so it already runs as the
/// operator and reads exactly what they can. The bind, not an ACL, is the boundary there.
pub(super) fn grant(_dir: &std::path::Path, _write: bool) -> Result<()> {
    Ok(())
}

pub(super) fn revoke(_dir: &std::path::Path) -> Result<()> {
    Ok(())
}

pub(super) fn converge_runner_acls(_status: &RuntimeStatus) {}

/// Lifts a mask left by [`disable`] first; a no-op when there is none.
#[cfg(target_os = "linux")]
pub(super) fn enable() -> Result<()> {
    run_systemctl(&["unmask", UNIT])?;
    run_systemctl(&["enable", "--now", UNIT])?;
    println!("Plugin runner enabled and started ({UNIT}).");
    Ok(())
}

/// The packages enable the unit in GLOBAL scope (`/etc/systemd/user`), which a user-scope
/// disable cannot undo: `is-enabled` keeps answering `enabled`. Mask is the per-user opt-out
/// there. The SteamOS install writes the unit into `~/.config/systemd/user`, where mask is
/// refused, so mask only when disable left it enabled.
#[cfg(target_os = "linux")]
pub(super) fn disable() -> Result<()> {
    run_systemctl(&["disable", "--now", UNIT])?;
    if systemctl_output(&["is-enabled", UNIT]).as_deref() == Some("enabled") {
        run_systemctl(&["mask", UNIT])?;
    }
    println!("Plugin runner stopped and disabled ({UNIT}).");
    Ok(())
}

/// Shown while systemd keeps restarting a runner that dies at start — "switched off" would send
/// the operator to a switch that is already on.
#[cfg(target_os = "linux")]
const RUNNER_FAILING: &str =
    "The plugin runner keeps failing to start. Troubleshooting → Plugins shows why.";

#[cfg(target_os = "linux")]
pub(super) fn runtime_status() -> RuntimeStatus {
    let enabled_raw = systemctl_output(&["is-enabled", UNIT]);
    let active = systemctl_output(&["is-active", UNIT]).unwrap_or_default();
    // `is-enabled` is `not-found` when the unit file is missing; the runner payload
    // is the other half of "can we install plugins".
    let unit_known = enabled_raw.as_deref().is_some_and(|s| s != "not-found");
    let installed = unit_known || runner_command().is_ok();
    let failing = active == "failed"
        || systemctl_output(&["show", UNIT, "-p", "SubState", "--value"]).as_deref()
            == Some("auto-restart");
    RuntimeStatus {
        installed,
        enabled: enabled_raw.as_deref() == Some("enabled"),
        running: active == "active",
        unit: UNIT,
        principal: None,
        detail: if !installed {
            RUNNER_MISSING.into()
        } else if failing {
            RUNNER_FAILING.into()
        } else {
            String::new()
        },
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) fn runtime_status() -> RuntimeStatus {
    RuntimeStatus {
        installed: false,
        enabled: false,
        running: false,
        unit: "punktfunk-scripting",
        principal: None,
        detail: "the plugin runner is only available on Linux and Windows hosts".into(),
    }
}

#[cfg(target_os = "linux")]
fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("run systemctl (is systemd available in this session?)")?;
    if !status.success() {
        bail!(
            "systemctl --user {} failed — is the punktfunk-scripting package installed?",
            args.join(" ")
        );
    }
    Ok(())
}

/// Trimmed `systemctl --user` stdout, or `None` if it could not run. Queries exit
/// non-zero for a normal "inactive"/"disabled", so the text is the answer.
#[cfg(target_os = "linux")]
fn systemctl_output(args: &[&str]) -> Option<String> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Is `PUNKTFUNK_PLUGIN_SANDBOX` off in the runner unit's own environment? The host's
/// environment says nothing about it: the runner reads only what its unit sets.
#[cfg(target_os = "linux")]
pub(super) fn runner_sandbox_off() -> bool {
    systemctl_output(&["show", UNIT, "-p", "Environment", "--value"]).is_some_and(|env| {
        env.split_whitespace().any(|kv| {
            kv.strip_prefix("PUNKTFUNK_PLUGIN_SANDBOX=")
                .is_some_and(|v| matches!(v.trim_matches('"'), "0" | "off" | "false"))
        })
    })
}

#[cfg(not(target_os = "linux"))]
pub(super) fn runner_sandbox_off() -> bool {
    false
}

#[cfg(target_os = "linux")]
pub(super) fn restart_runtime() -> Result<()> {
    run_systemctl(&["restart", UNIT])
}

/// The runner unit's drop-in naming every root a sandbox binds; its tmpfs home hides the rest.
#[cfg(target_os = "linux")]
const ROOTS_DROPIN: &str = "50-plugin-roots.conf";

/// Install the roots drop-in, then reload and restart the runner when it changed.
#[cfg(target_os = "linux")]
pub(super) fn converge_runner_roots(
    roots: &[access::RunnerRoot],
    home: &std::path::Path,
) -> Result<bool> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let dir = config.join(format!("systemd/user/{UNIT}.service.d"));
    let path = dir.join(ROOTS_DROPIN);
    let body = render_roots(roots, &hidden_roots(home));
    if std::fs::read_to_string(&path).is_ok_and(|old| old == body) {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    // Per process: a CLI grant and the serving host may converge at the same moment.
    let tmp = dir.join(format!("{ROOTS_DROPIN}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("replace {}", path.display()))?;
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["--no-block", "try-restart", UNIT])?;
    Ok(true)
}

/// What `ProtectHome` hides: `/home` and `/root` (not just this `home`), each also as it
/// resolves. Roots are canonical, and on Fedora Atomic `/home` is a link to `/var/home`.
#[cfg(any(test, target_os = "linux"))]
fn hidden_roots(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for p in [
        home,
        std::path::Path::new("/home"),
        std::path::Path::new("/root"),
    ] {
        out.push(p.to_path_buf());
        if let Ok(real) = p.canonicalize() {
            out.push(real);
        }
    }
    out
}

/// One self-bind per root the unit would otherwise hide or keep read-only; a read anywhere
/// outside `hidden` is visible already. A `src:dst` pair fails the unit's `+` ExecStartPre.
/// systemd drops a bind whose path holds a quote, and a control character would end the line:
/// left out.
#[cfg(any(test, target_os = "linux"))]
fn render_roots(roots: &[access::RunnerRoot], hidden: &[std::path::PathBuf]) -> String {
    let mut out = String::from(
        "# Written by punktfunk-host from plugin manifests and folder grants. Edits are replaced.\n[Service]\n",
    );
    for r in roots {
        let Some(path) = r.path.to_str() else {
            continue;
        };
        if path
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\'')
        {
            tracing::warn!(
                path,
                cause = "a quote or control character in the name",
                "plugin root left out of the runner unit"
            );
            continue;
        }
        let key = match (hidden.iter().any(|h| r.path.starts_with(h)), r.write) {
            (true, true) => "BindPaths",
            (true, false) => "BindReadOnlyPaths",
            (false, true) => "ReadWritePaths",
            (false, false) => continue,
        };
        let quoted = path.replace('\\', "\\\\").replace('%', "%%");
        out.push_str(&format!("{key}=\"-{quoted}\"\n"));
    }
    out
}

#[cfg(not(target_os = "linux"))]
pub(super) fn converge_runner_roots(
    _roots: &[access::RunnerRoot],
    _home: &std::path::Path,
) -> Result<bool> {
    Ok(false)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn enable() -> Result<()> {
    bail!("the plugin runner is only available on Linux and Windows hosts")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn disable() -> Result<()> {
    bail!("the plugin runner is only available on Linux and Windows hosts")
}

#[cfg(not(target_os = "linux"))]
pub(super) fn restart_runtime() -> Result<()> {
    bail!("the plugin runner is only available on Linux and Windows hosts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn present(ps: Vec<PathBuf>) -> impl Fn(&Path) -> bool {
        move |p: &Path| ps.iter().any(|q| q == p)
    }

    /// Layouts the resolver must serve. Nix lands on `PATH` and nowhere else.
    #[test]
    fn runner_resolution_table() {
        let beside = Path::new("/opt/punktfunk/bin");
        let nix = Path::new("/run/current-system/sw/bin");
        let home = Path::new("/home/deck");

        let exists = present(vec![nix.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(beside),
                Some(nix.to_str().unwrap()),
                None,
                &exists
            ),
            Some((nix.join(RUNNER_BIN), Vec::new()))
        );

        let exists = present(vec![beside.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                Some("/nix/store/abc/bin/punktfunk-scripting"),
                Some(beside),
                Some("/usr/bin"),
                Some(home),
                &exists
            ),
            Some(("/nix/store/abc/bin/punktfunk-scripting".into(), Vec::new()))
        );
        // Override is not existence-checked: a typo fails naming that path.
        assert_eq!(
            resolve_runner_in(Some("/nope/pf"), Some(beside), None, None, &exists),
            Some(("/nope/pf".into(), Vec::new()))
        );
        // Empty/whitespace is unset, not a path.
        assert_eq!(
            resolve_runner_in(Some("  "), Some(beside), None, None, &exists),
            Some((beside.join(RUNNER_BIN), Vec::new()))
        );
        let exists = present(vec![beside.join(RUNNER_BIN), nix.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(beside),
                Some(nix.to_str().unwrap()),
                None,
                &exists
            ),
            Some((beside.join(RUNNER_BIN), Vec::new()))
        );
        // PATH is walked entry by entry; empty entries skipped.
        let exists = present(vec![nix.join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(Path::new("/nowhere")),
                Some(":/nope:/run/current-system/sw/bin"),
                None,
                &exists
            ),
            Some((nix.join(RUNNER_BIN), Vec::new()))
        );

        // Packaged wrapper even when the unit PATH omits `/usr/bin`.
        let exists = present(vec![PathBuf::from("/usr/bin").join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(
                None,
                Some(Path::new("/nowhere")),
                Some("/nope"),
                None,
                &exists
            ),
            Some((PathBuf::from("/usr/bin").join(RUNNER_BIN), Vec::new()))
        );
        // Private two-file layout when the wrapper is absent.
        let bun = PathBuf::from(PACKAGED_BUN);
        let cli = PathBuf::from("/usr/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js");
        let exists = present(vec![bun.clone(), cli.clone()]);
        assert_eq!(
            resolve_runner_in(None, None, None, None, &exists),
            Some((bun, vec![cli.to_string_lossy().into_owned()]))
        );
        // Half of that layout is not a rung — do not spawn bun with no script.
        let exists = present(vec![PathBuf::from(PACKAGED_BUN)]);
        assert_eq!(resolve_runner_in(None, None, None, None, &exists), None);

        // SteamOS payload is user-scoped; reached only via HOME.
        let exists = present(vec![home.join(".local/bin").join(RUNNER_BIN)]);
        assert_eq!(
            resolve_runner_in(None, None, None, Some(home), &exists),
            Some((home.join(".local/bin").join(RUNNER_BIN), Vec::new()))
        );
        assert_eq!(resolve_runner_in(None, None, None, None, &exists), None);
        let bun = home.join(".local/lib").join(RUNNER_BIN).join("bun");
        let cli = home
            .join(".local/share")
            .join(RUNNER_BIN)
            .join("runner-cli.js");
        let exists = present(vec![bun.clone(), cli.clone()]);
        assert_eq!(
            resolve_runner_in(None, None, None, Some(home), &exists),
            Some((bun, vec![cli.to_string_lossy().into_owned()]))
        );

        let exists = present(vec![]);
        assert_eq!(
            resolve_runner_in(None, Some(beside), Some("/nope"), Some(home), &exists),
            None
        );
    }

    /// Miss text must name every install path, including NixOS (not only `apt`).
    #[test]
    fn the_missing_runner_error_names_every_platform_it_can_be_installed_on() {
        for hint in [
            "apt install",
            "steamdeck/install.sh",
            "NixOS",
            "PUNKTFUNK_SCRIPTING",
        ] {
            assert!(RUNNER_MISSING.contains(hint), "missing hint: {hint}");
        }
    }

    fn root(path: &str, write: bool) -> access::RunnerRoot {
        access::RunnerRoot {
            path: path.into(),
            write,
        }
    }

    /// systemd reads `"-path"`, `:`, `%%` and `\\` back as the paths written here.
    #[test]
    fn roots_render_as_the_unit_grammar() {
        let body = render_roots(
            &[
                root("/h/Emu", false),
                root("/h/My 100% Games:x\\y", false),
                root("/h/saves", true),
                root("/mnt/games", false),
                root("/mnt/out", true),
                root("/home/other/Games", false),
            ],
            &hidden_roots(Path::new("/h")),
        );
        let lines: Vec<&str> = body.lines().skip(2).collect();
        assert_eq!(
            lines,
            [
                r#"BindReadOnlyPaths="-/h/Emu""#,
                r#"BindReadOnlyPaths="-/h/My 100%% Games:x\\y""#,
                r#"BindPaths="-/h/saves""#,
                r#"ReadWritePaths="-/mnt/out""#,
                // ProtectHome hides every home, not only the operator's.
                r#"BindReadOnlyPaths="-/home/other/Games""#,
            ]
        );
        assert!(body.starts_with("# Written by punktfunk-host"));
        assert_eq!(body.lines().nth(1), Some("[Service]"));
    }

    #[test]
    fn a_root_that_could_break_the_unit_is_left_out() {
        let body = render_roots(
            &[
                root("/h/it's", false),
                root("/h/a\"b", false),
                root("/h/x\nExecStartPre=+/bin/sh", false),
            ],
            &hidden_roots(Path::new("/h")),
        );
        assert_eq!(body.lines().count(), 2, "{body}");
    }

    /// Fedora Atomic: `$HOME` is spelled under `/home`, a link to `/var/home`, and every root
    /// arrives canonical. Such a root is hidden all the same, so it gets its bind.
    #[test]
    fn a_home_behind_a_link_still_gets_its_binds() {
        let tmp = std::env::temp_dir().join(format!("pf-roots-link-{}", std::process::id()));
        let real_home = tmp.join("var/home/u");
        std::fs::create_dir_all(real_home.join(".local/share/Steam")).unwrap();
        std::os::unix::fs::symlink(tmp.join("var/home"), tmp.join("home")).unwrap();
        let steam = real_home.canonicalize().unwrap().join(".local/share/Steam");
        let body = render_roots(
            &[root(steam.to_str().unwrap(), false)],
            &hidden_roots(&tmp.join("home/u")),
        );
        std::fs::remove_dir_all(&tmp).unwrap();
        assert_eq!(
            body.lines().nth(2),
            Some(format!("BindReadOnlyPaths=\"-{}\"", steam.display()).as_str()),
            "{body}"
        );
    }
}
