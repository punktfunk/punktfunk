//! A Windows seat's own runner. The seat host (SYSTEM) starts `scripting-run.cmd` as the account
//! signed in to the seat's session, with no window, in a job only the host holds: the runner
//! ends with the host, at the seat's sign-out, and when it is switched off. No scheduled task
//! and no stored password. The switch is `plugin-runner-on` in the seat's config dir, as on
//! Linux. Each start gives that account read on what the runner reads there, and nothing else
//! of the seat's config.

use super::{icacls, runner_bundle_dir, runner_command};
use crate::plugins::{RuntimeStatus, RUNNER_DATA_DIR};
use crate::windows::interactive::{
    session_user_var, spawn_tree_as_current_session_user, SessionTree,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// The file in the seat's config dir that keeps its runner on.
const SWITCH: &str = "plugin-runner-on";

/// What the seat's config dir holds for its runner, and the seat account's ACE on each. Only
/// `plugin-state` is written; bun opens a unit file with WA, which changes no content.
const READS: [(&str, &str); 8] = [
    ("plugin-token", "(R)"),
    ("plugin-run/plugin-tokens.json", "(R)"),
    ("native-cert.pem", "(R)"),
    ("cert.pem", "(R)"),
    ("mgmt-endpoint", "(R)"),
    (RUNNER_DATA_DIR, "(OI)(CI)(RX)"),
    ("plugins", "(OI)(CI)(RX,WA)"),
    ("plugin-state", "(OI)(CI)(M)"),
];

/// The bundle's ACE: bun opens its entry script with WA too.
const BUNDLE: &str = "(OI)(CI)(RX,WA)";

/// How long a stop waits for the runner to let go of its log.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// The runner's name in the console.
const UNIT: &str = "punktfunk-scripting-seat";

static RUNNER: Mutex<Option<SessionTree>> = Mutex::new(None);

fn runner() -> MutexGuard<'static, Option<SessionTree>> {
    RUNNER.lock().unwrap_or_else(|poison| poison.into_inner())
}

fn on() -> bool {
    pf_paths::config_dir().join(SWITCH).exists()
}

/// The account signed in to this seat host's session as `S-1-…`. It stays the same while the
/// host runs: the seat's sign-out ends the host.
pub(super) fn account_sid() -> Option<String> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Some(sid.clone());
    }
    let sid = crate::windows::theme::session_sid()?;
    Some(SID.get_or_init(|| sid).clone())
}

fn account_name() -> Option<String> {
    static NAME: OnceLock<String> = OnceLock::new();
    if let Some(name) = NAME.get() {
        return Some(name.clone());
    }
    let name = session_user_var("USERNAME")?;
    Some(NAME.get_or_init(|| name).clone())
}

pub(super) fn status() -> RuntimeStatus {
    let installed = runner_command().is_ok();
    let running = runner().as_ref().is_some_and(|t| t.exit_code().is_none());
    RuntimeStatus {
        installed,
        enabled: on(),
        running,
        unit: UNIT,
        principal: account_name(),
        detail: if installed {
            String::new()
        } else {
            "reinstall punktfunk with the scripting component to get the plugin runner".into()
        },
    }
}

pub(super) fn enable() -> Result<()> {
    let switch = pf_paths::config_dir().join(SWITCH);
    pf_paths::replace_file(&switch, b"")
        .with_context(|| format!("replace {}", switch.display()))?;
    restart()
}

/// Off: the switch goes, the runner stops, and the seat's account loses what [`READS`] gave it.
pub(super) fn disable() -> Result<()> {
    let switch = pf_paths::config_dir().join(SWITCH);
    match std::fs::remove_file(&switch) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("remove {}", switch.display()));
        }
        _ => {}
    }
    if let Some(tree) = runner().take() {
        tree.end(STOP_WAIT);
    }
    if let (Some(account), Some(bundle)) = (account_sid(), runner_bundle_dir()) {
        for (path, _) in reads(&pf_paths::config_dir(), &bundle) {
            icacls(&path, &["/remove:g", &format!("*{account}")]);
        }
    }
    Ok(())
}

/// Stops the runner if it runs, then starts it: one at a time, whoever asks.
pub(super) fn restart() -> Result<()> {
    let mut runner = runner();
    if let Some(tree) = runner.take() {
        tree.end(STOP_WAIT);
    }
    *runner = Some(start().context("start the seat's plugin runner")?);
    Ok(())
}

/// The minute's check: a runner that is on and ended with an error, or never started, starts
/// again. One that ended cleanly stays down, as a refused sandbox does.
pub(super) fn restart_failed() {
    let failed = match runner().as_ref() {
        Some(tree) => tree.exit_code().is_some_and(|code| code != 0),
        None => true,
    };
    if !failed || !on() || runner_command().is_err() {
        return;
    }
    if let Err(e) = restart() {
        tracing::warn!(error = %format!("{e:#}"), "seat plugin runner not started");
    }
}

/// `scripting-run.cmd` as the seat's account, with the reads it needs. Its two folders are made
/// here: the runner makes neither.
fn start() -> Result<SessionTree> {
    runner_command()?;
    let bundle = runner_bundle_dir().context("resolve the runner bundle")?;
    let account = account_sid().context("nobody is signed in to this seat")?;
    let config = pf_paths::config_dir();
    for dir in [RUNNER_DATA_DIR, "plugin-state", "plugins"] {
        let dir = config.join(dir);
        if !dir.is_dir() {
            pf_paths::create_private_dir(&dir)
                .with_context(|| format!("create {}", dir.display()))?;
        }
    }
    for (path, rights) in reads(&config, &bundle) {
        if !path.exists() {
            continue;
        }
        if !icacls(&path, &["/grant:r", &format!("*{account}:{rights}")]) {
            tracing::warn!(path = %path.display(), "seat plugin runner read not granted");
        }
    }
    let script = bundle.join("scripting-run.cmd");
    let line = command_line(&crate::install::sys32("cmd.exe"), &script);
    spawn_tree_as_current_session_user(&line, &bundle, &env(crate::plugins::person_runner()))
}

/// Each path [`READS`] names under `config`, then the runner bundle.
fn reads(config: &Path, bundle: &Path) -> Vec<(PathBuf, &'static str)> {
    READS
        .iter()
        .map(|(name, rights)| (config.join(name), *rights))
        .chain([(bundle.to_path_buf(), BUNDLE)])
        .collect()
}

/// `cmd /d /c` running `script`: `/d` skips AutoRun, and the outer quotes are `/c`'s to strip.
fn command_line(cmd: &str, script: &Path) -> String {
    format!("\"{cmd}\" /d /c \"\"{}\"\"", script.display())
}

/// What the runner gets beside its host's own settings: on a person's own account, the word to
/// run their own plugins alone.
fn env(person: bool) -> Vec<(String, String)> {
    person
        .then(|| ("PUNKTFUNK_PERSON_PLUGINS".to_string(), "1".to_string()))
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner starts through `cmd`, which strips the outer quotes and keeps a path with
    /// spaces whole; only a person's own account narrows it to their plugins.
    #[test]
    fn the_runner_s_line_and_environment() {
        let script = Path::new(r"C:\Program Files\punktfunk\scripting\scripting-run.cmd");
        assert_eq!(
            command_line(r"C:\Windows\System32\cmd.exe", script),
            r#""C:\Windows\System32\cmd.exe" /d /c ""C:\Program Files\punktfunk\scripting\scripting-run.cmd"""#
        );
        assert_eq!(
            env(true),
            [("PUNKTFUNK_PERSON_PLUGINS".to_string(), "1".to_string())]
        );
        assert!(env(false).is_empty());
    }

    /// The seat's account reads its runner's inputs, writes its state alone, and gets nothing of
    /// the seat's own secrets.
    #[test]
    fn the_seat_s_account_reads_its_runner_s_inputs_alone() {
        let config = Path::new(r"C:\seat");
        let named: Vec<String> = reads(config, Path::new(r"C:\app\scripting"))
            .into_iter()
            .map(|(path, rights)| format!("{} {rights}", path.display()))
            .collect();
        assert!(named.iter().all(|n| !n.contains("mgmt-token")));
        assert!(named.iter().all(|n| !n.contains("person-token")));
        assert!(named.iter().all(|n| !n.contains("native-key")));
        let writes: Vec<&String> = named.iter().filter(|n| n.ends_with("(M)")).collect();
        assert_eq!(writes, [r"C:\seat\plugin-state (OI)(CI)(M)"]);
        assert_eq!(named.last().unwrap(), r"C:\app\scripting (OI)(CI)(RX,WA)");
    }
}
