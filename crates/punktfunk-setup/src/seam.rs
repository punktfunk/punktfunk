//! The two injection seams every other module reads the world through.
//!
//! `BasePaths` owns every filesystem root a probe may touch; `CommandRunner` owns every
//! process spawn. Nothing else in the crate may call `std::process::Command::new` — the
//! crate-local `clippy.toml` denies it, and `SystemRunner` carries the single `#[allow]`.
//!
//! Swap both for the fake pair and the engine cannot reach the host. See
//! `design/installer-v2.md` D3.
//!
//! `PUNKTFUNK_INSTALL_OS_RELEASE` and `PUNKTFUNK_INSTALL_ETC` override the real
//! `/etc` paths so installer-smoke's fake boxes keep working against this binary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Every filesystem root a probe may read.
///
/// `etc_root` is a prefix, not `/etc` itself: the sh script joins `"$ETC/etc/apt/..."`,
/// so an empty override is the real `/etc`.
#[derive(Debug, Clone)]
pub struct BasePaths {
    pub os_release: PathBuf,
    pub etc_root: PathBuf,
    pub sys: PathBuf,
    pub run: PathBuf,
    pub config: PathBuf,
    pub home: PathBuf,
}

impl BasePaths {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/root"), PathBuf::from);
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let etc_root = std::env::var_os("PUNKTFUNK_INSTALL_ETC")
            .map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let os_release = std::env::var_os("PUNKTFUNK_INSTALL_OS_RELEASE")
            .map_or_else(|| PathBuf::from("/etc/os-release"), PathBuf::from);
        Self {
            os_release,
            etc_root,
            sys: "/sys".into(),
            run: "/run".into(),
            config,
            home,
        }
    }

    pub fn rooted(root: &Path) -> Self {
        Self {
            os_release: root.join("etc/os-release"),
            etc_root: root.to_path_buf(),
            sys: root.join("sys"),
            run: root.join("run"),
            config: root.join("config"),
            home: root.join("home"),
        }
    }

    pub fn etc(&self, rel: &str) -> PathBuf {
        self.etc_root.join("etc").join(rel)
    }

    /// Path is reported verbatim in dry-run output.
    pub fn host_env(&self) -> PathBuf {
        self.config.join("punktfunk/host.env")
    }

    /// The host's settings store; the web console edits the same file.
    pub fn host_settings(&self) -> PathBuf {
        self.config.join("punktfunk/host-settings.json")
    }

    pub fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

/// Process environment, snapshotted once. Probes must not read `std::env`:
/// `set_var` is unsafe in Rust 2024, so tests cannot stage `DISPLAY` in a parallel suite.
#[derive(Debug, Default, Clone)]
pub struct Env(pub HashMap<String, String>);

impl Env {
    pub fn from_env() -> Self {
        Self(std::env::vars().collect())
    }

    /// Staged pairs only; any other key reads as unset.
    pub fn of(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// Spawn stdin. `Tty` is the package manager's prompt; without a terminal use
/// `/dev/null`, never the script's own stdin under `curl | sh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stdin {
    Tty,
    Null,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Spawn failed. No message: the caller owns the die line, matching the sh installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunFailed;

pub trait CommandRunner {
    fn run_shell(&self, cmd: &str, stdin: Stdin) -> Result<(), RunFailed>;

    /// `run_shell` with the output captured instead of inherited. The progress view shows
    /// nothing while a step runs, so on failure the tail is what the user gets to act on.
    fn run_shell_quiet(&self, cmd: &str, stdin: Stdin) -> Result<(), Vec<String>> {
        self.run_shell(cmd, stdin).map_err(|RunFailed| Vec::new())
    }

    /// Spawn a probe and capture it. `None` when the program is not on `PATH`.
    fn probe(&self, program: &str, args: &[&str]) -> Option<Output>;

    fn which(&self, program: &str) -> bool;

    fn first_line(&self, program: &str, args: &[&str]) -> Option<String> {
        let out = self.probe(program, args)?;
        if !out.ok() {
            return None;
        }
        out.stdout.lines().next().map(|l| l.trim().to_string())
    }
}

/// The one implementation that touches the machine.
pub struct SystemRunner {
    /// Prepended to `PATH` for the root-without-sudo shim.
    pub path_prefix: Option<PathBuf>,
    /// Injected into every spawn. Plan commands carry a literal `"$USER"`, so it must
    /// be set even when the installer was invoked without one.
    pub exports: Vec<(String, String)>,
}

impl SystemRunner {
    pub fn new() -> Self {
        Self {
            path_prefix: None,
            exports: Vec::new(),
        }
    }

    /// `sh -ec <cmd>` with stdin wired the way the caller asked.
    fn sh(&self, cmd: &str, stdin: Stdin) -> std::process::Command {
        let source = match stdin {
            Stdin::Tty => std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .ok(),
            Stdin::Null => None,
        };
        let mut c = self.command("sh");
        c.arg("-ec").arg(cmd);
        match source {
            Some(tty) => c.stdin(std::process::Stdio::from(tty)),
            None => c.stdin(std::process::Stdio::null()),
        };
        c
    }

    // The single sanctioned `Command::new` in the crate; everything else routes through the
    // trait so demo mode and the tests cannot be bypassed by accident.
    #[allow(clippy::disallowed_methods)]
    fn command(&self, program: &str) -> std::process::Command {
        // Windows: a bare tool name becomes its System32 path. The engine runs elevated
        // from the download directory, which `CreateProcess` searches before `%PATH%`.
        #[cfg(windows)]
        let program = &if program.contains(['\\', '/']) {
            program.to_string()
        } else {
            let exe = if program.contains('.') {
                program.to_string()
            } else {
                format!("{program}.exe")
            };
            crate::platform::windows::sys::system32(&exe)
        };
        let mut c = std::process::Command::new(program);
        // CREATE_NO_WINDOW: a fresh hidden console per child. The one inherited from the
        // updater's host dies with the service this run stops, and a child born into a
        // dead console exits 0xC0000142.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut c, 0x0800_0000);
        for (key, value) in &self.exports {
            c.env(key, value);
        }
        if let Some(prefix) = &self.path_prefix {
            let existing = std::env::var_os("PATH").unwrap_or_default();
            let mut parts = vec![prefix.clone()];
            parts.extend(std::env::split_paths(&existing));
            if let Ok(joined) = std::env::join_paths(parts) {
                c.env("PATH", joined);
            }
        }
        c
    }
}

impl Default for SystemRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for SystemRunner {
    fn run_shell(&self, cmd: &str, stdin: Stdin) -> Result<(), RunFailed> {
        match self.sh(cmd, stdin).status() {
            Ok(s) if s.success() => Ok(()),
            _ => Err(RunFailed),
        }
    }

    /// `exec 2>&1` keeps stderr in order with stdout. sudo's password prompt talks to the
    /// terminal directly, so it still reaches the user.
    fn run_shell_quiet(&self, cmd: &str, stdin: Stdin) -> Result<(), Vec<String>> {
        match self.sh(&format!("exec 2>&1\n{cmd}"), stdin).output() {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_string)
                .collect()),
            Err(_) => Err(Vec::new()),
        }
    }

    fn probe(&self, program: &str, args: &[&str]) -> Option<Output> {
        let out = self
            .command(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .ok()?;
        Some(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn which(&self, program: &str) -> bool {
        self.probe(
            "sh",
            &["-c", &format!("command -v {program} >/dev/null 2>&1")],
        )
        .is_some_and(|o| o.ok())
    }
}

/// Scripted answers, no spawn. Keyed by the whole command line
/// (`"systemctl is-active sunshine.service"`). An unscripted probe returns `None`,
/// the same as a missing binary.
#[derive(Debug, Default, Clone)]
pub struct FakeRunner {
    pub answers: HashMap<String, Output>,
    pub on_path: Vec<String>,
    pub ran: std::cell::RefCell<Vec<String>>,
    /// Fail `run_shell` when the snippet contains this substring.
    pub fail_matching: Option<String>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn answer(mut self, line: &str, code: i32, stdout: &str) -> Self {
        self.answers.insert(
            line.to_string(),
            Output {
                code,
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        );
        self.on_path
            .push(line.split_whitespace().next().unwrap_or(line).to_string());
        self
    }

    pub fn with_path(mut self, program: &str) -> Self {
        self.on_path.push(program.to_string());
        self
    }
}

impl CommandRunner for FakeRunner {
    fn run_shell(&self, cmd: &str, _stdin: Stdin) -> Result<(), RunFailed> {
        self.ran.borrow_mut().push(cmd.to_string());
        match &self.fail_matching {
            Some(needle) if cmd.contains(needle.as_str()) => Err(RunFailed),
            _ => Ok(()),
        }
    }

    fn probe(&self, program: &str, args: &[&str]) -> Option<Output> {
        let mut line = String::from(program);
        for a in args {
            line.push(' ');
            line.push_str(a);
        }
        if let Some(out) = self.answers.get(&line) {
            return Some(out.clone());
        }
        // On PATH but unscripted: exit 1, not `None`. Missing and "says no" are different.
        if self.on_path.iter().any(|p| p == program) {
            return Some(Output {
                code: 1,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        None
    }

    fn which(&self, program: &str) -> bool {
        self.on_path.iter().any(|p| p == program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etc_root_is_a_prefix_not_the_directory() {
        let p = BasePaths::rooted(Path::new("/tmp/box"));
        // Compare as `Path`s, not strings: `join` writes `\` on Windows.
        assert_eq!(
            p.etc("apt/sources.list.d/punktfunk.list"),
            Path::new("/tmp/box/etc/apt/sources.list.d/punktfunk.list")
        );
    }

    #[test]
    fn an_unscripted_probe_of_a_missing_program_is_none() {
        let r = FakeRunner::new().with_path("systemctl");
        assert!(r.probe("nvidia-smi", &[]).is_none());
        assert_eq!(r.probe("systemctl", &["is-active", "x"]).unwrap().code, 1);
    }

    #[test]
    fn fake_runner_records_every_snippet_it_was_handed() {
        let r = FakeRunner::new();
        r.run_shell("sudo apt update", Stdin::Null).unwrap();
        assert_eq!(r.ran.borrow().as_slice(), ["sudo apt update"]);
    }
}
