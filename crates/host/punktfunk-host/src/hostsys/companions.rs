//! The web console and the plugin runner as children of a serving macOS host: one app, one thing
//! to quit. Each runs on the bundle's `Contents/Helpers/bun` from `Contents/Resources`; a payload
//! the bundle lacks is not supervised.
//!
//! A child that exits starts again on the Windows console slot's backoff. Its output goes to
//! `<config_dir>/logs/<name>.log` and its pid to `<config_dir>/<name>.pid`, so a start after a
//! crash ends what the last host left. The runner is off while `plugin-runner-off` exists, and a
//! `plugin-runner-restart` file, from this process or the `plugins` CLI, asks for a fresh start.
//! The console's environment is `scripts/punktfunk-web.service`'s; `first_run` is
//! `scripts/web-init.sh`.

use pf_seats::backoff::{Backoff, Curve, Reset};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The console's port, which the tray probes.
const WEB_PORT: &str = "47992";
/// The runner's `SANDBOX_REFUSED_EXIT` (sdk/src/runner.ts): a restart would probe the same box.
const SANDBOX_REFUSED: i32 = 78;
/// Present, the runner is switched off.
const RUNNER_OFF: &str = "plugin-runner-off";
/// One fresh start of the runner, asked from any process. The supervisor removes it.
const RUNNER_RESTART: &str = "plugin-runner-restart";
/// How often the supervisor looks at its children and the runner's two files.
const TICK: Duration = Duration::from_millis(500);
/// One older generation of each log, rotated at a start. A single long run grows it until the next.
const LOG_CAP: u64 = 10 * 1024 * 1024;

const BACKOFF: Backoff = Backoff::new(
    Curve::Doubling {
        first: Duration::from_millis(500),
        cap: Duration::from_secs(60),
    },
    Reset::AfterRun(Duration::from_secs(60)),
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Web,
    Runner,
}

impl Kind {
    /// Its log and pid file are named after this.
    fn name(self) -> &'static str {
        match self {
            Kind::Web => "web",
            Kind::Runner => "runner",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Kind::Web => "web console",
            Kind::Runner => "plugin runner",
        }
    }

    /// SIGTERM to SIGKILL. The runner's plugins run their finalizers first, in the 30 s its
    /// Linux unit allows.
    fn grace(self) -> Duration {
        match self {
            Kind::Web => Duration::from_secs(5),
            Kind::Runner => Duration::from_secs(30),
        }
    }
}

/// bun and the script it runs.
#[derive(Clone, Debug, PartialEq)]
struct Payload {
    bun: PathBuf,
    script: PathBuf,
}

impl Payload {
    /// `kind`'s payload in the bundle around `exe`, which is `Contents/MacOS/<name>`.
    fn in_bundle(exe: &Path, kind: Kind) -> Option<Payload> {
        let contents = exe.parent()?.parent()?;
        let resources = contents.join("Resources");
        Some(Payload {
            bun: contents.join("Helpers").join("bun"),
            script: match kind {
                Kind::Web => resources.join("web/.output/server/index.mjs"),
                Kind::Runner => resources.join("scripting/runner-cli.js"),
            },
        })
    }

    fn of(kind: Kind) -> Option<Payload> {
        Payload::in_bundle(&std::env::current_exe().ok()?, kind)
    }

    fn present(&self) -> bool {
        self.bun.is_file() && self.script.is_file()
    }

    /// The bun a pid file's process must run to be ours: as the kernel names it.
    fn real_bun(&self) -> PathBuf {
        std::fs::canonicalize(&self.bun).unwrap_or_else(|_| self.bun.clone())
    }
}

struct Running {
    child: Child,
    /// When this host asked it to end.
    ending: Option<Instant>,
}

struct Slot {
    kind: Kind,
    payload: Payload,
    child: Option<Running>,
    backoff: Backoff,
    next_spawn: Instant,
    /// The runner refused to run without its sandbox: down until a restart is asked for.
    held: bool,
    /// A restart was asked for: a running child ends first.
    restart: bool,
    /// One log line per wait, not one per tick.
    said_wait: bool,
}

struct Supervisor {
    config: PathBuf,
    stopping: bool,
    slots: Vec<Slot>,
}

static SUPERVISOR: Mutex<Option<Supervisor>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<Supervisor>> {
    SUPERVISOR
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Supervise both children from now on. The serving host calls this once, as its management
/// API comes up: the token, the endpoint and the identity cert are on disk by then.
pub(crate) fn start() {
    let config = pf_paths::config_dir();
    let host_env = std::fs::read_to_string(config.join("host.env")).unwrap_or_default();
    let mut slots = Vec::new();
    for kind in [Kind::Web, Kind::Runner] {
        let Some(payload) = Payload::of(kind).filter(Payload::present) else {
            tracing::info!(
                "{} not supervised: the app bundle carries none",
                kind.label()
            );
            continue;
        };
        if kind == Kind::Web && is_off(setting("PUNKTFUNK_WEB_CONSOLE", &host_env)) {
            tracing::info!("web console not supervised: PUNKTFUNK_WEB_CONSOLE turns it off");
            continue;
        }
        slots.push(Slot {
            kind,
            payload,
            child: None,
            backoff: BACKOFF,
            next_spawn: Instant::now(),
            held: false,
            restart: false,
            said_wait: false,
        });
    }
    if slots.is_empty() {
        return;
    }
    if slots.iter().any(|s| s.kind == Kind::Web) {
        first_run(&config);
    }
    // This start is a fresh one already.
    let _ = std::fs::remove_file(config.join(RUNNER_RESTART));
    *lock() = Some(Supervisor {
        config,
        stopping: false,
        slots,
    });
    if let Err(e) = std::thread::Builder::new()
        .name("companions".into())
        .spawn(supervise)
    {
        tracing::error!(error = %e, "companion supervisor did not start");
    }
}

/// End both children and supervise no more: SIGTERM, then SIGKILL past each one's grace.
/// Blocks until they are gone.
pub(crate) fn stop() {
    let mut pid_files = Vec::new();
    let mut ending = Vec::new();
    {
        let mut guard = lock();
        let Some(sup) = guard.as_mut() else { return };
        sup.stopping = true;
        for slot in &mut sup.slots {
            pid_files.push(pid_file(&sup.config, slot.kind));
            let Some(run) = slot.child.take() else {
                continue;
            };
            ending.push(Ending {
                pid: run.child.id(),
                signalled: run.ending.is_some(),
                deadline: run.ending.unwrap_or_else(Instant::now) + slot.kind.grace(),
                bun: slot.payload.real_bun(),
                child: Some(run.child),
            });
        }
    }
    end(ending);
    for file in pid_files {
        let _ = std::fs::remove_file(file);
    }
}

/// The bundle's runner, which the `plugins` package commands run too.
pub(crate) fn runner_payload() -> Option<(PathBuf, PathBuf)> {
    Payload::of(Kind::Runner).map(|p| (p.bun, p.script))
}

pub(crate) fn runner_installed() -> bool {
    Payload::of(Kind::Runner).is_some_and(|p| p.present())
}

pub(crate) fn runner_enabled() -> bool {
    !pf_paths::config_dir().join(RUNNER_OFF).exists()
}

/// Whether the runner a serving host started runs now. Any process may ask.
pub(crate) fn runner_running() -> bool {
    let Some(payload) = Payload::of(Kind::Runner) else {
        return false;
    };
    std::fs::read_to_string(pid_file(&pf_paths::config_dir(), Kind::Runner))
        .is_ok_and(|text| ours(&text, &payload.real_bun(), exe_of).is_some())
}

/// Switch the runner on or off, now and for every later start of the serving host. On also
/// starts a runner that isn't running, one that refused its sandbox included.
pub(crate) fn set_runner_enabled(on: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    let off = pf_paths::config_dir().join(RUNNER_OFF);
    if !on {
        return pf_paths::replace_file(&off, b"")
            .with_context(|| format!("replace {}", off.display()));
    }
    match std::fs::remove_file(&off) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", off.display()))
        }
        _ if runner_running() => Ok(()),
        _ => restart_runner(),
    }
}

/// Ask the serving host for a fresh start of its runner, which follows within a tick.
pub(crate) fn restart_runner() -> anyhow::Result<()> {
    use anyhow::Context;
    let file = pf_paths::config_dir().join(RUNNER_RESTART);
    pf_paths::replace_file(&file, b"").with_context(|| format!("replace {}", file.display()))
}

/// Whether host.env, else this process's environment, turns the per-plugin sandbox off: what
/// the runner's own environment says at its start ([`child_env`]).
pub(crate) fn sandbox_off() -> bool {
    let host_env =
        std::fs::read_to_string(pf_paths::config_dir().join("host.env")).unwrap_or_default();
    is_off(setting(SANDBOX_KEY, &host_env))
}

fn supervise() {
    let Some(config) = lock().as_ref().map(|sup| sup.config.clone()) else {
        return;
    };
    // Both kinds, supervised or not: a console turned off since still holds its port.
    end([Kind::Web, Kind::Runner]
        .into_iter()
        .filter_map(|kind| left_over(&config, kind, &Payload::of(kind)?))
        .collect());
    loop {
        {
            let mut guard = lock();
            let Some(sup) = guard.as_mut() else { return };
            if sup.stopping {
                return;
            }
            for slot in &mut sup.slots {
                slot.tick(&config, Instant::now());
            }
        }
        std::thread::sleep(TICK);
    }
}

impl Slot {
    fn tick(&mut self, config: &Path, now: Instant) {
        if self.kind == Kind::Runner && take(&config.join(RUNNER_RESTART)) {
            self.held = false;
            self.restart = true;
            self.backoff.reset();
            self.next_spawn = now;
        }
        let wanted = self.wanted(config);
        if let Some(run) = &mut self.child {
            let pid = run.child.id();
            match run.child.try_wait() {
                Ok(None) => {
                    if run.ending.is_some_and(|at| now >= at + self.kind.grace()) {
                        kill_group(pid);
                    } else if run.ending.is_none() && (self.restart || !wanted) {
                        term(pid);
                        run.ending = Some(now);
                        self.restart = false;
                    }
                    return;
                }
                Ok(Some(status)) => {
                    // What it started and left behind. A group's id is not reused while it lives.
                    kill_group(pid);
                    let asked = run.ending.is_some();
                    self.child = None;
                    self.exited(config, asked, status, now);
                }
                Err(e) => {
                    tracing::warn!(error = %e, pid, "{} not waited for", self.kind.label());
                    return;
                }
            }
        }
        self.restart = false;
        if self.held || !wanted || now < self.next_spawn {
            return;
        }
        let label = self.kind.label();
        if !self.payload.present() {
            self.say_wait(|| tracing::warn!("{label} payload missing from the app bundle"));
            return;
        }
        if self.kind == Kind::Web && !host_ready(config) {
            self.say_wait(|| {
                tracing::info!("web console waits for the host's mgmt token and identity cert")
            });
            return;
        }
        match spawn(self.kind, &self.payload, config) {
            Ok(child) => {
                let pid = child.id();
                if let Err(e) = pf_paths::replace_file(
                    &pid_file(config, self.kind),
                    format!("{pid}\n").as_bytes(),
                ) {
                    tracing::warn!(error = %e, "{} pid not recorded", self.kind.label());
                }
                tracing::info!(pid, "{} started", self.kind.label());
                self.backoff.started(now);
                self.said_wait = false;
                self.child = Some(Running {
                    child,
                    ending: None,
                });
            }
            Err(e) => {
                tracing::error!(error = %e, "{} did not start", self.kind.label());
                self.next_spawn = now + self.backoff.failed(now);
            }
        }
    }

    /// Whether this child should be up now. The console always is.
    fn wanted(&self, config: &Path) -> bool {
        self.kind == Kind::Web || !config.join(RUNNER_OFF).exists()
    }

    fn say_wait(&mut self, say: impl FnOnce()) {
        if !self.said_wait {
            self.said_wait = true;
            say();
        }
    }

    fn exited(&mut self, config: &Path, asked: bool, status: ExitStatus, now: Instant) {
        let _ = std::fs::remove_file(pid_file(config, self.kind));
        match after_exit(self.kind, asked, status.code()) {
            Next::Now => {
                tracing::info!("{} stopped", self.kind.label());
                self.backoff.reset();
                self.next_spawn = now;
            }
            Next::Hold => {
                tracing::warn!(
                    "plugin runner stopped: it can't sandbox its plugins on this Mac — \
                     Troubleshooting → Plugins says why"
                );
                self.held = true;
            }
            Next::Later => {
                let wait = self.backoff.failed(now);
                self.next_spawn = now + wait;
                tracing::warn!(
                    %status,
                    retry_in_ms = wait.as_millis() as u64,
                    "{} exited — restarting",
                    self.kind.label()
                );
            }
        }
    }
}

/// What follows a child's exit.
#[derive(Debug, PartialEq)]
enum Next {
    /// It ended because this host asked: start it again now, if it is still wanted.
    Now,
    /// The runner refused to run without its sandbox: stay down until a restart is asked for.
    Hold,
    /// Anything else: start it again after the backoff.
    Later,
}

/// The runner never ends on its own but for its sandbox refusal, so any other exit, a signal's
/// or a clean one, counts as a failure.
fn after_exit(kind: Kind, asked: bool, code: Option<i32>) -> Next {
    if asked {
        Next::Now
    } else if kind == Kind::Runner && code == Some(SANDBOX_REFUSED) {
        Next::Hold
    } else {
        Next::Later
    }
}

fn spawn(kind: Kind, payload: &Payload, config: &Path) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let host_env = std::fs::read_to_string(config.join("host.env")).unwrap_or_default();
    let log = open_log(config, kind)?;
    let mut cmd = Command::new(&payload.bun);
    cmd.arg(&payload.script)
        .envs(child_env(kind, config, &host_env))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Its own group, so a SIGKILL to the group also ends what it started.
        .process_group(0);
    if let Some(dir) = payload.script.parent() {
        cmd.current_dir(dir);
    }
    cmd.spawn()
}

/// What `kind` gets on top of this process's environment, which `main` has emptied of
/// credentials. The console reads the four files itself (`web/nitro-entry/env-files.mjs`), a later
/// one winning, as the Linux unit's `EnvironmentFile=` lines do.
fn child_env(kind: Kind, config: &Path, host_env: &str) -> Vec<(&'static str, String)> {
    let at = |name: &str| config.join(name).to_string_lossy().into_owned();
    let mut env = vec![(
        "PUNKTFUNK_CONFIG_DIR",
        config.to_string_lossy().into_owned(),
    )];
    match kind {
        Kind::Web => env.extend([
            ("PORT", WEB_PORT.to_string()),
            // The legacy pair: the console serves the native one beside it (tls-paths.mjs).
            ("PUNKTFUNK_UI_TLS_CERT", at("cert.pem")),
            ("PUNKTFUNK_UI_TLS_KEY", at("key.pem")),
            ("PUNKTFUNK_UI_SECURE", "1".into()),
            ("PUNKTFUNK_UI_PASSWORD_FILE", at("web-password")),
            (
                "PUNKTFUNK_ENV_FILES",
                ["host.env", "mgmt-token", "web-password", "mgmt-endpoint"]
                    .map(at)
                    .join(":"),
            ),
        ]),
        // The runner reads no host.env, so its one setting there travels in its environment.
        Kind::Runner => env.extend(
            pf_paths::env_file::get(host_env, SANDBOX_KEY).map(|v| (SANDBOX_KEY, v.to_string())),
        ),
    }
    env
}

const SANDBOX_KEY: &str = "PUNKTFUNK_PLUGIN_SANDBOX";

/// `key` as host.env sets it, else as this process's environment does.
fn setting(key: &str, host_env: &str) -> Option<String> {
    pf_paths::env_file::get(host_env, key)
        .map(str::to_string)
        .or_else(|| std::env::var(key).ok())
}

fn is_off(value: Option<String>) -> bool {
    value.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// What the console can't start without (`web-init.sh`'s `host_ready`): the mgmt token and one
/// cert/key pair from one directory. Non-empty, since the host writes them in place.
fn host_ready(config: &Path) -> bool {
    let full = |name: &str| std::fs::metadata(config.join(name)).is_ok_and(|m| m.len() > 0);
    full("mgmt-token")
        && ((full("native-cert.pem") && full("native-key.pem"))
            || (full("cert.pem") && full("key.pem")))
}

/// The login password once and the console's bind in host.env once. The password is never logged.
fn first_run(config: &Path) {
    let pw = config.join("web-password");
    if !std::fs::metadata(&pw).is_ok_and(|m| m.len() > 0) {
        let line = format!(
            "PUNKTFUNK_UI_PASSWORD={}\n",
            crate::mgmt_token::random_password()
        );
        match pf_paths::replace_secret_file(&pw, line.as_bytes()) {
            Ok(()) => tracing::info!(
                file = %pw.display(),
                "web console login password generated — read it there before the first sign-in"
            ),
            Err(e) => tracing::warn!(error = %e, "web console login password not written"),
        }
    }
    let path = config.join("host.env");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(error = %e, "host.env not read, so the console's bind is not named");
            return;
        }
    };
    if let Some(next) = with_bind_named(&text) {
        match pf_paths::replace_secret_file(&path, next.as_bytes()) {
            Ok(()) => tracing::info!(
                "host.env: the web console answers on your local network (PUNKTFUNK_UI_BIND=0.0.0.0)"
            ),
            Err(e) => tracing::warn!(error = %e, "console bind not named in host.env"),
        }
    }
}

/// `text` with the console's bind named, or `None` when a line names it already.
fn with_bind_named(text: &str) -> Option<String> {
    if pf_paths::env_file::get(text, "PUNKTFUNK_UI_BIND").is_some() {
        return None;
    }
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    Some(format!(
        "{text}{sep}\n# Where the web console listens: 0.0.0.0 (your network, never the internet), \
         127.0.0.1\n# (this machine only), or one address, e.g. a VPN interface.\n\
         PUNKTFUNK_UI_BIND=0.0.0.0\n"
    ))
}

fn open_log(config: &Path, kind: Kind) -> std::io::Result<std::fs::File> {
    let dir = config.join("logs");
    pf_paths::create_private_dir(&dir)?;
    let path = dir.join(format!("{}.log", kind.name()));
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= LOG_CAP) {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
}

fn pid_file(config: &Path, kind: Kind) -> PathBuf {
    config.join(format!("{}.pid", kind.name()))
}

/// Whether `path` existed, removing it.
fn take(path: &Path) -> bool {
    std::fs::remove_file(path).is_ok()
}

/// A child on its way out: this host's own, or one an earlier host left.
struct Ending {
    pid: u32,
    child: Option<Child>,
    /// The bun it runs, as the kernel names it.
    bun: PathBuf,
    deadline: Instant,
    /// Already sent SIGTERM: a second one ends the runner without its finalizers.
    signalled: bool,
}

impl Ending {
    fn gone(&mut self) -> bool {
        match &mut self.child {
            Some(child) => !matches!(child.try_wait(), Ok(None)),
            None => exe_of(self.pid).as_deref() != Some(self.bun.as_path()),
        }
    }
}

/// SIGTERM to each, then SIGKILL to its group once it has gone or its grace ran out: what it
/// started goes with it.
fn end(mut procs: Vec<Ending>) {
    for p in procs.iter().filter(|p| !p.signalled) {
        term(p.pid);
    }
    while !procs.is_empty() {
        let now = Instant::now();
        procs.retain_mut(|p| {
            if !p.gone() && now < p.deadline {
                return true;
            }
            kill_group(p.pid);
            if let Some(child) = &mut p.child {
                let _ = child.wait();
            }
            false
        });
        if !procs.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// What an earlier host left of `kind`: the pid its file names, while that process still runs
/// our bun.
fn left_over(config: &Path, kind: Kind, payload: &Payload) -> Option<Ending> {
    let file = pid_file(config, kind);
    let text = std::fs::read_to_string(&file).ok()?;
    let _ = std::fs::remove_file(&file);
    let bun = payload.real_bun();
    let pid = ours(&text, &bun, exe_of)?;
    tracing::info!(pid, "{} left by an earlier host — ending it", kind.label());
    Some(Ending {
        pid,
        child: None,
        bun,
        deadline: Instant::now() + kind.grace(),
        signalled: false,
    })
}

/// The pid `pid_text` names while `exe_of` says it runs `bun`. Never this process, 0 or 1: a
/// signal to those would reach the wrong group.
fn ours(pid_text: &str, bun: &Path, exe_of: impl Fn(u32) -> Option<PathBuf>) -> Option<u32> {
    let pid: u32 = pid_text.trim().parse().ok()?;
    if pid <= 1 || pid == std::process::id() || i32::try_from(pid).is_err() {
        return None;
    }
    (exe_of(pid).as_deref() == Some(bun)).then_some(pid)
}

/// The executable `pid` runs, or `None` when no such process is there.
fn exe_of(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let pid = i32::try_from(pid).ok()?;
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `buf` is a live local of the length passed; the call writes at most that many bytes
    // and returns how many it wrote.
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    let n = usize::try_from(n).ok().filter(|&n| n > 0)?.min(buf.len());
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..n])))
}

/// SIGTERM to the child alone: the runner ends its own plugins.
fn term(pid: u32) {
    signal(pid, false, libc::SIGTERM);
}

/// SIGKILL to the group the child leads.
fn kill_group(pid: u32) {
    signal(pid, true, libc::SIGKILL);
}

fn signal(pid: u32, group: bool, sig: libc::c_int) {
    let Ok(pid) = i32::try_from(pid) else { return };
    if pid <= 1 {
        return;
    }
    let target = if group { -pid } else { pid };
    // SAFETY: kill(2) takes two integers and touches no memory of this process. `target` is never
    // 0, 1 or -1, which would reach this process's own group or every process.
    unsafe { libc::kill(target, sig) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payloads_sit_beside_the_executable_in_the_bundle() {
        let exe = Path::new("/Applications/Punktfunk Host.app/Contents/MacOS/punktfunk-host");
        let app = Path::new("/Applications/Punktfunk Host.app/Contents");
        let web = Payload::in_bundle(exe, Kind::Web).unwrap();
        assert_eq!(web.bun, app.join("Helpers/bun"));
        assert_eq!(
            web.script,
            app.join("Resources/web/.output/server/index.mjs")
        );
        let runner = Payload::in_bundle(exe, Kind::Runner).unwrap();
        assert_eq!(runner.bun, web.bun);
        assert_eq!(runner.script, app.join("Resources/scripting/runner-cli.js"));
    }

    /// The console's lines are the Linux unit's; the runner gets only host.env's sandbox switch.
    #[test]
    fn each_child_gets_its_launchers_environment() {
        let cfg = Path::new("/u/.config/punktfunk");
        let web = child_env(Kind::Web, cfg, "PUNKTFUNK_PLUGIN_SANDBOX=off\n");
        let get = |k: &str| web.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PORT"), Some("47992"));
        assert_eq!(get("PUNKTFUNK_CONFIG_DIR"), Some("/u/.config/punktfunk"));
        assert_eq!(
            get("PUNKTFUNK_UI_TLS_CERT"),
            Some("/u/.config/punktfunk/cert.pem")
        );
        assert_eq!(get("PUNKTFUNK_UI_SECURE"), Some("1"));
        assert_eq!(
            get("PUNKTFUNK_ENV_FILES"),
            Some(
                "/u/.config/punktfunk/host.env:/u/.config/punktfunk/mgmt-token:\
                 /u/.config/punktfunk/web-password:/u/.config/punktfunk/mgmt-endpoint"
            )
        );
        assert_eq!(get("PUNKTFUNK_PLUGIN_SANDBOX"), None);
        // The secrets stay in their files; the console reads them there.
        for secret in ["PUNKTFUNK_MGMT_TOKEN", "PUNKTFUNK_UI_PASSWORD"] {
            assert_eq!(get(secret), None, "{secret}");
        }

        let runner = child_env(Kind::Runner, cfg, "# x\nPUNKTFUNK_PLUGIN_SANDBOX=off\n");
        assert_eq!(
            runner,
            [
                ("PUNKTFUNK_CONFIG_DIR", "/u/.config/punktfunk".to_string()),
                ("PUNKTFUNK_PLUGIN_SANDBOX", "off".to_string()),
            ]
        );
        assert_eq!(child_env(Kind::Runner, cfg, "").len(), 1);
    }

    #[test]
    fn only_the_runners_sandbox_refusal_holds_it_down() {
        assert_eq!(after_exit(Kind::Runner, true, Some(0)), Next::Now);
        assert_eq!(after_exit(Kind::Runner, true, Some(78)), Next::Now);
        assert_eq!(after_exit(Kind::Runner, false, Some(78)), Next::Hold);
        assert_eq!(after_exit(Kind::Runner, false, Some(0)), Next::Later);
        assert_eq!(after_exit(Kind::Runner, false, None), Next::Later);
        assert_eq!(after_exit(Kind::Web, false, Some(78)), Next::Later);
        assert_eq!(after_exit(Kind::Web, true, None), Next::Now);
    }

    /// A recorded pid is ended only while it still runs our bun.
    #[test]
    fn a_left_over_pid_is_ours_only_while_it_runs_our_bun() {
        let bun = Path::new("/A.app/Contents/Helpers/bun");
        let runs = |want: u32, exe: &'static str| {
            move |pid: u32| (pid == want).then(|| PathBuf::from(exe))
        };
        assert_eq!(
            ours("4242\n", bun, runs(4242, "/A.app/Contents/Helpers/bun")),
            Some(4242)
        );
        assert_eq!(ours("4242", bun, runs(4242, "/usr/local/bin/bun")), None);
        assert_eq!(
            ours("4242", bun, runs(7, "/A.app/Contents/Helpers/bun")),
            None
        );
        for junk in ["", "abc", "-5", "0", "1", "4294967295"] {
            assert_eq!(
                ours(junk, bun, |_| Some(bun.to_path_buf())),
                None,
                "{junk:?}"
            );
        }
        let me = std::process::id().to_string();
        assert_eq!(ours(&me, bun, |_| Some(bun.to_path_buf())), None);
    }

    #[test]
    fn the_running_test_process_has_an_executable() {
        assert_eq!(
            exe_of(std::process::id()),
            Some(std::env::current_exe().unwrap().canonicalize().unwrap())
        );
        assert_eq!(exe_of(u32::MAX), None);
    }

    #[test]
    fn the_console_waits_for_a_token_and_one_whole_pair() {
        let dir = tempfile::tempdir().unwrap();
        let put = |name: &str, body: &str| std::fs::write(dir.path().join(name), body).unwrap();
        assert!(!host_ready(dir.path()));
        put("mgmt-token", "PUNKTFUNK_MGMT_TOKEN=x\n");
        put("native-cert.pem", "c");
        assert!(!host_ready(dir.path()));
        put("native-key.pem", "");
        assert!(!host_ready(dir.path()), "an empty key is mid-write");
        put("native-key.pem", "k");
        assert!(host_ready(dir.path()));
        std::fs::remove_file(dir.path().join("native-key.pem")).unwrap();
        put("cert.pem", "c");
        put("key.pem", "k");
        assert!(host_ready(dir.path()));
    }

    #[test]
    fn the_bind_is_named_once() {
        let named = with_bind_named("").unwrap();
        assert_eq!(
            pf_paths::env_file::get(&named, "PUNKTFUNK_UI_BIND"),
            Some("0.0.0.0")
        );
        assert_eq!(with_bind_named(&named), None);
        assert_eq!(with_bind_named("  PUNKTFUNK_UI_BIND=127.0.0.1\n"), None);
        let kept = with_bind_named("RUST_LOG=info").unwrap();
        assert!(kept.starts_with("RUST_LOG=info\n\n# Where"), "{kept}");
        // A commented line names nothing.
        assert!(with_bind_named("# PUNKTFUNK_UI_BIND=127.0.0.1\n").is_some());
    }

    #[test]
    fn a_setting_is_off_only_when_it_says_so() {
        assert!(is_off(setting(
            "PUNKTFUNK_WEB_CONSOLE",
            "PUNKTFUNK_WEB_CONSOLE=Off\n"
        )));
        assert!(!is_off(setting(
            "PUNKTFUNK_WEB_CONSOLE",
            "PUNKTFUNK_WEB_CONSOLE=on\n"
        )));
        assert!(!is_off(Some("yes".into())));
        assert!(!is_off(None));
    }
}
