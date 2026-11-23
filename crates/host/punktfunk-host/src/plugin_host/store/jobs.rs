//! Install and uninstall **jobs** for the plugin store (`design/plugin-store.md`).
//!
//! A package op takes tens of seconds, so the API returns a job id (HTTP 202) and
//! the console polls. `bun add` / `bun remove` share a lockfile and `node_modules`;
//! a request that arrives while a job runs is 409, not a queue.
//!
//! Pipeline: resolve → verify pin vs registry integrity → install → check on-disk
//! version (else roll back) → record provenance → restart the runner (it rediscovers
//! units only at startup).
//!
//! Verify is skipped for raw-spec installs: there is no pin.

use super::index::{scope_of, Entry};
use super::manifest::{self, Record, Tier};
use crate::plugin_host::PluginTokens;
use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 300s: `bun add` over a cold cache on a slow link is the worst case.
const JOB_TIMEOUT: Duration = Duration::from_secs(300);

const JOB_HISTORY: usize = 20;

const LOG_LINES: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum State {
    Running,
    Done,
    Failed,
}

/// Snake_case matches the management API; index/sources/manifest files use npm camelCase.
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct Job {
    pub id: String,
    pub kind: String,
    pub target: String,
    pub state: State,
    pub phase: String,
    pub log: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<u64>,
    /// The oldest folder whose access is changing while the job runs, whoever it is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk: Option<crate::plugins::walks::AccessWalk>,
}

struct Jobs {
    jobs: VecDeque<Job>,
    counter: u64,
}

fn jobs() -> &'static Mutex<Jobs> {
    static JOBS: std::sync::OnceLock<Mutex<Jobs>> = std::sync::OnceLock::new();
    JOBS.get_or_init(|| {
        Mutex::new(Jobs {
            jobs: VecDeque::new(),
            counter: 0,
        })
    })
}

fn lock() -> std::sync::MutexGuard<'static, Jobs> {
    jobs().lock().unwrap_or_else(|e| e.into_inner())
}

fn begin(kind: &str, target: &str) -> Result<String> {
    let mut g = lock();
    if let Some(active) = g.jobs.iter().find(|j| j.state == State::Running) {
        bail!(
            "another plugin operation is already running ({} {})",
            active.kind,
            active.target
        );
    }
    g.counter += 1;
    let id = format!("job-{}-{}", crate::clock::unix_secs_u64(), g.counter);
    let job = Job {
        id: id.clone(),
        kind: kind.to_string(),
        target: target.to_string(),
        state: State::Running,
        phase: "queued".into(),
        log: Vec::new(),
        error: None,
        started_at: crate::clock::unix_secs_u64(),
        finished_at: None,
        walk: None,
    };
    g.jobs.push_back(job);
    while g.jobs.len() > JOB_HISTORY {
        g.jobs.pop_front();
    }
    Ok(id)
}

fn update(id: &str, f: impl FnOnce(&mut Job)) {
    let mut g = lock();
    if let Some(job) = g.jobs.iter_mut().find(|j| j.id == id) {
        f(job);
    }
}

fn set_phase(id: &str, phase: &str) {
    tracing::info!(job = %id, phase, "plugin store job");
    update(id, |j| j.phase = phase.to_string());
}

fn log_line(id: &str, line: String) {
    update(id, |j| {
        if j.log.len() >= LOG_LINES {
            j.log.remove(0);
        }
        j.log.push(line);
    });
}

fn finish(id: &str, result: Result<()>) {
    update(id, |j| {
        j.finished_at = Some(crate::clock::unix_secs_u64());
        match &result {
            Ok(()) => {
                j.state = State::Done;
                j.phase = "done".into();
            }
            Err(e) => {
                j.state = State::Failed;
                j.error = Some(format!("{e:#}"));
            }
        }
    });
    match result {
        Ok(()) => tracing::info!(job = %id, "plugin store job finished"),
        Err(e) => tracing::warn!(job = %id, "plugin store job failed: {e:#}"),
    }
}

pub(crate) fn get(id: &str) -> Option<Job> {
    let mut job = lock().jobs.iter().find(|j| j.id == id).cloned()?;
    if job.state == State::Running {
        job.walk = crate::plugins::walks::current();
    }
    Some(job)
}

/// Newest last.
pub(crate) fn list() -> Vec<Job> {
    lock().jobs.iter().cloned().collect()
}

pub(crate) fn busy() -> bool {
    lock().jobs.iter().any(|j| j.state == State::Running)
}

/// Resolved install: spec, pins, and tier already decided.
pub(crate) struct Plan {
    /// Bare package name; `None` for URL/git specs.
    pub pkg: Option<String>,
    /// `bun add` operand: `pkg@version` for a catalog entry, else the raw spec.
    pub spec: String,
    pub version: Option<String>,
    /// `(scope, registry_url)` mapped into the plugins dir `bunfig.toml`.
    pub registry: Option<(String, String)>,
    pub integrity: Option<String>,
    pub tier: Tier,
    pub source: Option<String>,
    pub entry_id: Option<String>,
}

impl Plan {
    pub(crate) fn from_entry(entry: &Entry, source: &str, verified: bool) -> Result<Plan> {
        let scope = scope_of(&entry.pkg).context("catalog entry package must be scoped")?;
        // The scope's registry lands in bunfig for every later install and the SDK refresh, so
        // another source naming `@punktfunk` would redirect the official packages too.
        if scope == "@punktfunk" && source != super::sources::OFFICIAL_NAME {
            bail!("only the official source may install @punktfunk packages");
        }
        Ok(Plan {
            pkg: Some(entry.pkg.clone()),
            spec: format!("{}@{}", entry.pkg, entry.version),
            version: Some(entry.version.clone()),
            registry: Some((scope, entry.registry.clone())),
            integrity: Some(entry.integrity.clone()),
            tier: if verified {
                Tier::Verified
            } else {
                Tier::External
            },
            source: Some(source.to_string()),
            entry_id: Some(entry.id.clone()),
        })
    }

    /// Raw spec: no pin, no review, no catalog source.
    pub(crate) fn from_spec(spec: &str) -> Result<Plan> {
        let spec = validate_spec(spec)?;
        Ok(Plan {
            pkg: parse_spec_pkg(&spec),
            spec,
            version: None,
            registry: None,
            integrity: None,
            tier: Tier::Unverified,
            source: None,
            entry_id: None,
        })
    }
}

/// Specs the console may pass to `bun add`.
///
/// Not shell quoting (exec is an argv). A leading `-` is a flag; `file:` /
/// `link:` / `portal:` would install from the host filesystem. Those stay CLI-only.
fn validate_spec(spec: &str) -> Result<String> {
    let s = spec.trim();
    if s.is_empty() {
        bail!("empty package spec");
    }
    if s.len() > 400 {
        bail!("package spec is too long");
    }
    if s.starts_with('-') {
        bail!("a package spec cannot start with '-'");
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("a package spec cannot contain whitespace or control characters");
    }
    let lower = s.to_ascii_lowercase();
    for bad in ["file:", "link:", "portal:"] {
        if lower.starts_with(bad) {
            bail!("`{bad}` specs are not installable from the console — use the `punktfunk-host plugins add` CLI");
        }
    }
    // Only https and git+https. http:// and git:// are unauthenticated.
    if lower.contains("://")
        && !(lower.starts_with("https://") || lower.starts_with("git+https://"))
    {
        bail!("only https:// and git+https:// URLs are accepted");
    }
    Ok(s.to_string())
}

/// Bare name from an npm spec (`@scope/name`, `@scope/name@1.2.3`, `name@1`).
/// `None` for URL/git — those are identified by diffing `node_modules`.
fn parse_spec_pkg(spec: &str) -> Option<String> {
    if spec.contains("://") {
        return None;
    }
    if let Some(rest) = spec.strip_prefix('@') {
        // Version separator is the '@' after the scope slash.
        let (scope_and_name, _) = match rest.split_once('/') {
            Some((scope, tail)) => match tail.split_once('@') {
                Some((name, ver)) => (format!("@{scope}/{name}"), Some(ver)),
                None => (format!("@{scope}/{tail}"), None),
            },
            None => return None,
        };
        return Some(scope_and_name);
    }
    Some(
        spec.split_once('@')
            .map(|(n, _)| n)
            .unwrap_or(spec)
            .to_string(),
    )
}

/// Runs `work` for job `id` on a thread of its own and ends the job with its result. A refused
/// spawn or a panic ends it `Failed`, so the single-flight slot always frees.
fn spawn_worker(
    name: &str,
    id: &str,
    work: impl FnOnce(&str) -> Result<()> + Send + 'static,
) -> Result<()> {
    let job_id = id.to_string();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&job_id)))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("the plugin worker panicked")));
            finish(&job_id, result);
            crate::events::emit(crate::events::EventKind::StoreChanged);
        })
        .map(drop)
        .map_err(|e| {
            let e = anyhow::anyhow!("spawn the plugin worker: {e}");
            finish(id, Err(anyhow::anyhow!("{e:#}")));
            e
        })
}

pub(crate) fn spawn_install(plan: Plan, plugin_tokens: PluginTokens) -> Result<String> {
    let target = plan.pkg.clone().unwrap_or_else(|| plan.spec.clone());
    let id = begin("install", &target)?;
    spawn_worker("pf-store-install", &id, move |id| {
        run_install(id, plan, &plugin_tokens)
    })?;
    Ok(id)
}

pub(crate) fn spawn_uninstall(
    pkg: String,
    plugin_tokens: PluginTokens,
    delete_titles: crate::plugin_host::DeleteTitles,
) -> Result<String> {
    if super::valid_installed_pkg(&pkg).is_err() {
        bail!("not an installable plugin package name");
    }
    let id = begin("uninstall", &pkg)?;
    spawn_worker("pf-store-uninstall", &id, move |id| {
        run_uninstall(id, &pkg, &plugin_tokens, delete_titles)
    })?;
    Ok(id)
}

/// The installed version of a package an update is about to replace: its own files, and
/// `package.json` and `bun.lock` as bun left them, copied aside before `bun add` touches any.
/// A refused update puts them back. The copy goes when this drops.
struct Kept {
    version: String,
    manifest: Vec<u8>,
    lock: Option<Vec<u8>>,
    files: std::path::PathBuf,
}

impl Kept {
    fn take(dir: &std::path::Path, pkg: &str, version: &str) -> std::io::Result<Kept> {
        let files = dir.join(".kept-for-rollback");
        let _ = std::fs::remove_dir_all(&files);
        copy_tree(&dir.join("node_modules").join(pkg), &files)?;
        Ok(Kept {
            version: version.to_string(),
            manifest: std::fs::read(dir.join("package.json"))?,
            lock: std::fs::read(dir.join("bun.lock")).ok(),
            files,
        })
    }

    /// The package's files, `package.json` and `bun.lock` as they were when taken.
    fn put_back(&self, dir: &std::path::Path, pkg: &str) -> std::io::Result<()> {
        let at = dir.join("node_modules").join(pkg);
        let _ = std::fs::remove_dir_all(&at);
        copy_tree(&self.files, &at)?;
        std::fs::write(dir.join("package.json"), &self.manifest)?;
        match &self.lock {
            Some(lock) => std::fs::write(dir.join("bun.lock"), lock),
            None => {
                let _ = std::fs::remove_file(dir.join("bun.lock"));
                Ok(())
            }
        }
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.files);
    }
}

/// `from`'s folders and plain files at `to`. A link is left out.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let kind = std::fs::symlink_metadata(from)?.file_type();
    if kind.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else if kind.is_file() {
        std::fs::copy(from, to)?;
    }
    Ok(())
}

/// A refused update of `pkg` undone: the kept version's files and bun's record of them are
/// back, and bun has put the other packages back to that record where it could.
fn restore(id: &str, dir: &std::path::Path, pkg: &str, kept: &Kept, public: bool) -> Result<()> {
    kept.put_back(dir, pkg)
        .context("put the kept version back")?;
    let mut args = vec![
        "add".to_string(),
        format!("{pkg}@{}", kept.version),
        "--exact".to_string(),
    ];
    if public {
        args.push("--allow-public-registry".into());
    }
    args.push("--plugins".into());
    args.push(dir.to_string_lossy().into_owned());
    if let Err(e) = run_runner(id, &args) {
        log_line(
            id,
            format!("packages not put back to the kept record: {e:#}"),
        );
    }
    // Whatever bun fetched or rewrote for the package itself, the kept bytes win.
    kept.put_back(dir, pkg).context("put the kept version back")
}

fn run_install(id: &str, plan: Plan, plugin_tokens: &PluginTokens) -> Result<()> {
    let dir = super::plugins_dir();

    // Pin must still match what the registry serves.
    if let (Some(integrity), Some(pkg), Some(version), Some((_, registry))) = (
        plan.integrity.as_deref(),
        plan.pkg.as_deref(),
        plan.version.as_deref(),
        plan.registry.as_ref(),
    ) {
        set_phase(id, "verifying");
        let advertised = registry_integrity(registry, pkg, version)
            .with_context(|| format!("check {pkg}@{version} against {registry}"))?;
        if advertised != integrity {
            bail!(
                "integrity mismatch for {pkg}@{version}: the catalog pins {integrity} but the \
                 registry now serves {advertised}. This version was republished after it was \
                 reviewed — refusing to install."
            );
        }
        log_line(id, format!("integrity ok: {pkg}@{version}"));
    }

    set_phase(id, "installing");
    // Without a `package.json` here, `bun add` walks up and installs into the
    // nearest ancestor that has one — exit 0, wrong tree (`ensure_plugin_root`).
    super::ensure_plugin_root(&dir).with_context(|| format!("prepare {}", dir.display()))?;
    let before = super::installed_packages(&dir);
    // The version this replaces, kept so a refused update leaves it installed.
    let kept = plan.pkg.as_deref().and_then(|pkg| {
        let version = before.iter().find(|b| b.pkg == pkg)?.version.as_deref()?;
        Kept::take(&dir, pkg, version)
            .inspect_err(|e| log_line(id, format!("installed {pkg} not kept for a rollback: {e}")))
            .ok()
    });
    // Map the scope in bunfig ourselves. An older runner would treat an unknown
    // flag's value as a package name (`ensure_bunfig_scope`).
    if let Some((scope, url)) = &plan.registry {
        super::ensure_bunfig_scope(&dir, scope, url)
            .with_context(|| format!("map {scope} to {url}"))?;
    }
    let mut args = vec!["add".to_string(), plan.spec.clone()];
    if plan.version.is_some() {
        // Pin the range so a later `bun install` cannot drift. An old runner skips an
        // unknown `-` flag; we still install an exact version either way.
        args.push("--exact".into());
    }
    if !plan.spec.starts_with("@punktfunk/") {
        // The runner refuses non-Punktfunk registries unless this flag is set.
        // Catalog add or raw-spec install already made that choice.
        args.push("--allow-public-registry".into());
    }
    args.push("--plugins".into());
    args.push(dir.to_string_lossy().into_owned());

    run_runner(id, &args)?;

    set_phase(id, "checking");
    let after = super::installed_packages(&dir);
    let added: Vec<_> = after
        .iter()
        .filter(|p| !before.iter().any(|b| b.pkg == p.pkg))
        .collect();
    // Catalogued installs know the name; URL/git specs learn it from the diff.
    let pkg = plan
        .pkg
        .clone()
        .or_else(|| added.first().map(|p| p.pkg.clone()))
        .context(
            "the install finished but no new plugin package appeared — is this package a \
             punktfunk plugin? (it must be named `@scope/plugin-*` or `punktfunk-plugin-*`)",
        )?;
    let installed = after.iter().find(|p| p.pkg == pkg).with_context(|| {
        // Runner succeeded but the package is missing. Name the capturing ancestor
        // `package.json` if any (`ensure_plugin_root` seeds this dir).
        match super::capturing_ancestor(&dir) {
            Some(p) => format!(
                "the runner reported success but {pkg} is not in {} — `{}` is capturing the \
                 install (bun installs into the nearest package.json above the working directory). \
                 Move or delete it, or add a package.json to the plugins dir.",
                dir.display(),
                p.display()
            ),
            None => format!("{pkg} is not present after install"),
        }
    })?;

    // Do not leave an unreviewed version under a verified badge. The pin is checked again on
    // what bun fetched: the check above asked the registry separately, which can answer bun
    // with other bytes.
    let lock = std::fs::read_to_string(dir.join("bun.lock")).ok();
    let drift = pin_drift(
        &pkg,
        plan.version.as_deref(),
        plan.integrity.as_deref(),
        installed.version.as_deref(),
        lock.as_deref(),
    );
    if let Some(why) = drift {
        log_line(id, format!("rolling back: {why}"));
        set_phase(id, "rolling back");
        // An update goes back to the version that was installed; only a first install is removed.
        if let Some(kept) = &kept {
            let public = !plan.spec.starts_with("@punktfunk/");
            match restore(id, &dir, &pkg, kept, public) {
                Ok(()) => {
                    restart_runner(id);
                    bail!("{why} — kept {pkg}@{}", kept.version);
                }
                Err(e) => log_line(id, format!("{pkg}@{} not restored: {e:#}", kept.version)),
            }
        }
        let _ = run_runner(
            id,
            &[
                "remove".to_string(),
                pkg.clone(),
                "--plugins".to_string(),
                dir.to_string_lossy().into_owned(),
            ],
        );
        bail!("{why} — rolled back");
    }

    set_phase(id, "recording");
    manifest::record(
        &dir,
        &pkg,
        Record {
            tier: plan.tier,
            source: plan.source.clone(),
            entry_id: plan.entry_id.clone(),
            version: installed.version.clone().or(plan.version.clone()),
            spec: (plan.tier == Tier::Unverified).then(|| plan.spec.clone()),
            installed_at: Some(manifest::now_stamp()),
        },
    )
    .context("record install provenance")?;
    // On a person's own account the store answers their lane alone: what it installs is theirs.
    if pf_paths::seat::own_account() {
        crate::plugins::manifest::mark_person(&pf_paths::config_dir(), &pkg, true)
            .context("mark the plugin as the person's")?;
    }

    refresh_plugin_tokens(id, plugin_tokens)?;
    start_runner(id);
    Ok(())
}

/// Why `pkg`, installed at `got`, is not the catalog's pin, or `None` when it is. An integrity
/// pin holds only on the entry bun locked for that version: no lock, no entry or no installed
/// version is a mismatch.
fn pin_drift(
    pkg: &str,
    version: Option<&str>,
    integrity: Option<&str>,
    got: Option<&str>,
    lock: Option<&str>,
) -> Option<String> {
    if let (Some(want), Some(got)) = (version, got)
        && want != got
    {
        return Some(format!(
            "installed {pkg}@{got} but the catalog pinned {want}"
        ));
    }
    let want = integrity?;
    let Some(got) = got else {
        return Some(format!(
            "{pkg} has no version to check against the catalog pin"
        ));
    };
    match lock.and_then(|l| locked_integrity(l, pkg, got)) {
        Some(locked) if locked == want => None,
        Some(locked) => Some(format!(
            "bun installed {pkg}@{got} as {locked} but the catalog pins {want}"
        )),
        None => Some(format!(
            "no bun.lock entry for {pkg}@{got} to check against the catalog pin"
        )),
    }
}

/// [`restart_runner`], switching on a runner that is installed but off: an install asks for
/// the plugin to run, and a runner left off would keep it from ever starting.
fn start_runner(id: &str) {
    let st = crate::plugins::runtime_status();
    if !st.installed || st.enabled {
        return restart_runner(id);
    }
    set_phase(id, "starting runner");
    // Roots first, as a restart does: the new manifest may name paths the unit can't see yet.
    crate::plugins::converge_runner_roots();
    match crate::plugins::set_runtime_enabled(true) {
        Ok(()) => log_line(id, "plugin runner switched on to start this plugin".into()),
        Err(e) => log_line(id, format!("switch on the plugin runner: {e:#}")),
    }
}

fn run_uninstall(
    id: &str,
    pkg: &str,
    plugin_tokens: &PluginTokens,
    delete_titles: crate::plugin_host::DeleteTitles,
) -> Result<()> {
    let dir = super::plugins_dir();
    // Its titles leave with it: nothing would launch an exec tile of a removed plugin.
    let provider = crate::plugins::manifest::id_of_package(&dir, pkg);
    set_phase(id, "removing");
    run_runner(
        id,
        &[
            "remove".to_string(),
            pkg.to_string(),
            "--plugins".to_string(),
            dir.to_string_lossy().into_owned(),
        ],
    )?;
    set_phase(id, "recording");
    manifest::forget(&dir, pkg).context("update install provenance")?;
    if pf_paths::seat::own_account() {
        crate::plugins::manifest::mark_person(&pf_paths::config_dir(), pkg, false)
            .context("unmark the person's plugin")?;
    }
    if let Some(provider) = &provider {
        match delete_titles(provider) {
            Ok(n) if n > 0 => log_line(id, format!("removed {n} library titles of {provider}")),
            Ok(_) => {}
            Err(e) => log_line(
                id,
                format!("remove the library titles of {provider}: {e:#}"),
            ),
        }
    }
    refresh_plugin_tokens(id, plugin_tokens)?;
    // Its token is revoked now, so its own deregistration on the way out is refused.
    if let Some(provider) = provider
        && crate::plugin_host::registry::registry().remove(&provider)
    {
        crate::events::emit(crate::events::EventKind::PluginsChanged { id: provider });
    }
    restart_runner(id);
    Ok(())
}

/// Persist the installed-id token set, then publish the same set to live authentication before
/// the runner starts. The file is the runner's handoff; the lock is the API's view, and swapping
/// it here also revokes an uninstalled plugin's token.
fn refresh_plugin_tokens(id: &str, plugin_tokens: &PluginTokens) -> Result<()> {
    set_phase(id, "refreshing credentials");
    let refreshed = crate::mgmt_token::load_or_generate_per_plugin()
        .context("refresh per-plugin credentials")?;
    *plugin_tokens
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = refreshed;
    #[cfg(windows)]
    crate::plugin_host::TOKENS_CHANGED.notify_one();
    Ok(())
}

/// Best-effort restart so the runner rediscovers units. The package is already
/// installed: a restart failure is not an install failure.
fn restart_runner(id: &str) {
    set_phase(id, "restarting runner");
    match crate::plugins::restart_runtime() {
        Ok(true) => log_line(id, "plugin runner restarted".into()),
        Ok(false) => log_line(
            id,
            "the plugin runner is not enabled — enable it to start this plugin".into(),
        ),
        Err(e) => log_line(id, format!("restart the plugin runner: {e:#}")),
    }
}

fn run_runner(id: &str, args: &[String]) -> Result<()> {
    let (program, prefix) = crate::plugins::runner_command()?;
    tracing::info!(job = %id, program = %program.display(), ?args, "spawning the plugin runner");
    let mut cmd = Command::new(&program);
    cmd.args(&prefix)
        .args(args)
        // `bun add` must not block on a prompt inside a service.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Own process group, so a timeout also ends the bun the runner started.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("run the plugin runner ({})", program.display()))?;

    // Pump both streams: a full pipe on either blocks the child.
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        let job = id.to_string();
        pumps.push(std::thread::spawn(move || pump(&job, out)));
    }
    if let Some(err) = child.stderr.take() {
        let job = id.to_string();
        pumps.push(std::thread::spawn(move || pump(&job, err)));
    }

    let deadline = Instant::now() + JOB_TIMEOUT;
    let status = loop {
        match child.try_wait().context("wait for the plugin runner")? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                #[cfg(unix)]
                {
                    // SAFETY: kill(2) with a negative pid signals the process group created
                    // above; no memory is touched.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                }
                #[cfg(not(unix))]
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "the plugin runner timed out after {}s",
                    JOB_TIMEOUT.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(150)),
        }
    };
    for p in pumps {
        let _ = p.join();
    }
    if !status.success() {
        bail!(
            "the plugin runner exited with status {} — see the job log",
            status.code().unwrap_or(-1)
        );
    }
    Ok(())
}

fn pump(job: &str, stream: impl std::io::Read) {
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        let line = line.trim_end().to_string();
        if !line.is_empty() {
            log_line(job, line);
        }
    }
}

/// Integrity hash the registry advertises for `pkg@version`.
///
/// Clients already refuse a tarball that mismatches this hash. The remaining
/// attack is a republish (same version, new bytes). Compare against the review
/// pin before download.
fn registry_integrity(registry: &str, pkg: &str, version: &str) -> Result<String> {
    let base = registry.trim_end_matches('/');
    // Packument path percent-encodes the scope slash.
    let url = format!("{base}/{}", pkg.replace('/', "%2f"));
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .max_redirects(3)
        .user_agent(format!("punktfunk-host/{}", super::index::host_version()))
        .build()
        .into();
    let mut resp = agent
        .get(&url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => {
                anyhow::anyhow!("the registry does not know this package")
            }
            other => anyhow::anyhow!("registry request failed: {other}"),
        })?;
    let body = pf_update_check::feed::read_capped(&mut resp, 16 * 1024 * 1024)
        .context("read the registry response")?;
    let doc: serde_json::Value =
        serde_json::from_slice(&body).context("registry returned invalid JSON")?;
    let dist = doc
        .get("versions")
        .and_then(|v| v.get(version))
        .and_then(|v| v.get("dist"))
        .with_context(|| format!("the registry has no version {version} of this package"))?;
    dist.get("integrity")
        .and_then(|i| i.as_str())
        .map(str::to_string)
        .context("the registry did not advertise an integrity hash for this version")
}

/// The integrity `bun.lock` records for the top-level `pkg@version`. The text lockfile keeps
/// one package per line, its integrity the last string: `"pkg": ["pkg@1.0.0", "", {…}, "sha512-…"],`.
fn locked_integrity(lock: &str, pkg: &str, version: &str) -> Option<String> {
    let head = format!("\"{pkg}\": [\"{pkg}@{version}\",");
    let line = lock.lines().map(str::trim).find(|l| l.starts_with(&head))?;
    let tail = line.trim_end_matches(',').strip_suffix(']')?.trim_end();
    let tail = tail.strip_suffix('"')?;
    Some(tail[tail.rfind('"')? + 1..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locked_integrity_reads_the_top_level_entry() {
        let lock = r#"{
  "packages": {
    "@p/plugin-x": ["@p/plugin-x@1.2.0", "", { "dependencies": { "a": "^1" } }, "sha512-top=="],
    "@p/plugin-y/@p/plugin-x": ["@p/plugin-x@0.9.0", "", {}, "sha512-nested=="],
    "@p/plugin-z": ["@p/plugin-z@2.0.0", "https://r.example/", {}, "sha512-last=="]
  }
}"#;
        let got = |pkg, v| locked_integrity(lock, pkg, v);
        assert_eq!(got("@p/plugin-x", "1.2.0").as_deref(), Some("sha512-top=="));
        assert_eq!(
            got("@p/plugin-z", "2.0.0").as_deref(),
            Some("sha512-last==")
        );
        assert_eq!(got("@p/plugin-x", "0.9.0"), None);
        assert_eq!(got("@p/plugin-q", "1.0.0"), None);
    }

    #[test]
    fn pin_drift_refuses_what_the_lock_does_not_confirm() {
        let lock = r#"    "@p/plugin-x": ["@p/plugin-x@1.2.0", "", {}, "sha512-pinned=="],"#;
        let drift = |integrity: Option<&str>, got: Option<&str>, lock: Option<&str>| {
            pin_drift("@p/plugin-x", Some("1.2.0"), integrity, got, lock)
        };
        let pin = Some("sha512-pinned==");
        assert_eq!(drift(pin, Some("1.2.0"), Some(lock)), None);
        assert!(drift(Some("sha512-other=="), Some("1.2.0"), Some(lock)).is_some());
        assert!(drift(pin, Some("1.3.0"), Some(lock)).is_some());
        // What the lock can't confirm is refused, never passed.
        assert!(drift(pin, Some("1.2.0"), None).is_some());
        assert!(drift(pin, Some("1.2.0"), Some("{}")).is_some());
        assert!(drift(pin, None, Some(lock)).is_some());
        // An unpinned install has nothing to check.
        assert_eq!(drift(None, Some("1.2.0"), None), None);
    }

    /// What an update replaced comes back whole: the package's files, the manifest and the lock.
    #[test]
    fn a_kept_version_goes_back_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let pkg = "@p/plugin-x";
        let at = dir.join("node_modules").join(pkg);
        std::fs::create_dir_all(at.join("dist")).unwrap();
        let old: [(std::path::PathBuf, &str); 4] = [
            (at.join("package.json"), r#"{"version":"1.1.0"}"#),
            (at.join("dist/index.js"), "old"),
            (dir.join("package.json"), "old manifest"),
            (dir.join("bun.lock"), "old lock"),
        ];
        for (path, text) in &old {
            std::fs::write(path, text).unwrap();
        }
        let kept = Kept::take(dir, pkg, "1.1.0").unwrap();

        // What a refused `bun add` leaves: other files, a file more, and its own records.
        for (path, _) in &old {
            std::fs::write(path, "new").unwrap();
        }
        std::fs::write(at.join("dist/extra.js"), "new").unwrap();
        kept.put_back(dir, pkg).unwrap();
        for (path, text) in &old {
            assert_eq!(std::fs::read_to_string(path).unwrap(), *text, "{path:?}");
        }
        assert!(!at.join("dist/extra.js").exists());

        let copy = kept.files.clone();
        drop(kept);
        assert!(!copy.exists());
    }

    #[test]
    fn spec_validation_refuses_the_dangerous_shapes() {
        assert!(validate_spec("@scope/plugin-x").is_ok());
        assert!(validate_spec("@scope/plugin-x@1.2.3").is_ok());
        assert!(validate_spec("punktfunk-plugin-x").is_ok());
        assert!(validate_spec("https://e.org/p.tgz").is_ok());
        assert!(validate_spec("git+https://e.org/p.git").is_ok());

        assert!(validate_spec("").is_err());
        assert!(validate_spec("   ").is_err());
        // bun would parse a leading dash as a flag, not an operand
        assert!(validate_spec("--production").is_err());
        assert!(validate_spec("a b").is_err());
        // file:/link: would install from the host filesystem; that stays CLI-only
        assert!(validate_spec("file:/etc/passwd").is_err());
        assert!(validate_spec("link:../evil").is_err());
        // unauthenticated transports
        assert!(validate_spec("http://e.org/p.tgz").is_err());
        assert!(validate_spec("git://e.org/p.git").is_err());
    }

    #[test]
    fn spec_package_name_parsing() {
        assert_eq!(parse_spec_pkg("@a/b").as_deref(), Some("@a/b"));
        assert_eq!(parse_spec_pkg("@a/b@1.2.3").as_deref(), Some("@a/b"));
        assert_eq!(parse_spec_pkg("plain").as_deref(), Some("plain"));
        assert_eq!(parse_spec_pkg("plain@2").as_deref(), Some("plain"));
        // URL specs are identified by diffing node_modules after the install, not by parsing
        assert_eq!(parse_spec_pkg("https://e.org/p.tgz"), None);
    }

    #[test]
    fn plan_from_entry_pins_version_registry_and_integrity() {
        let idx = super::super::index::Index::parse(
            br#"{"schema":1,"plugins":[{"id":"rom-manager","pkg":"@punktfunk/plugin-rom-manager",
                "registry":"https://git.unom.io/api/packages/unom/npm/","title":"ROM",
                "version":"0.3.0","integrity":"sha512-AAAA"}]}"#,
        )
        .unwrap();
        let plan = Plan::from_entry(&idx.plugins[0], "unom", true).unwrap();
        assert_eq!(plan.spec, "@punktfunk/plugin-rom-manager@0.3.0");
        assert_eq!(plan.version.as_deref(), Some("0.3.0"));
        assert_eq!(plan.tier, Tier::Verified);
        assert_eq!(
            plan.registry.as_ref().unwrap().0,
            "@punktfunk",
            "the scope drives the bunfig registry mapping"
        );
        assert_eq!(plan.integrity.as_deref(), Some("sha512-AAAA"));

        // Another source may not claim the official scope: its registry would serve every
        // `@punktfunk` package from then on.
        assert!(Plan::from_entry(&idx.plugins[0], "retro-hub", false).is_err());
        let theirs = super::super::index::Index::parse(
            br#"{"schema":1,"plugins":[{"id":"hub","pkg":"@retro/plugin-hub",
                "registry":"https://example.org/npm/","title":"Hub",
                "version":"1.0.0","integrity":"sha512-BBBB"}]}"#,
        )
        .unwrap();
        let ext = Plan::from_entry(&theirs.plugins[0], "retro-hub", false).unwrap();
        assert_eq!(ext.tier, Tier::External);
        assert_eq!(ext.source.as_deref(), Some("retro-hub"));
    }

    #[test]
    fn plan_from_spec_is_unverified_and_unpinned() {
        let plan = Plan::from_spec("  @someone/punktfunk-plugin-x  ").unwrap();
        assert_eq!(plan.tier, Tier::Unverified);
        assert_eq!(plan.spec, "@someone/punktfunk-plugin-x");
        assert!(plan.version.is_none());
        assert!(
            plan.integrity.is_none(),
            "nothing to check a raw spec against"
        );
        assert!(plan.registry.is_none());
    }

    /// `begin` is process-global and single-flight; overlapping tests race.
    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        static M: Mutex<()> = Mutex::new(());
        M.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn single_flight_refuses_a_second_job() {
        let _g = exclusive();
        let first = begin("install", "@a/b").expect("first job starts");
        assert!(begin("install", "@c/d").is_err(), "second must be refused");
        assert!(busy());
        finish(&first, Ok(()));
        assert!(!busy());
        let second = begin("install", "@c/d").expect("a finished job frees the slot");
        finish(&second, Ok(()));
    }

    #[test]
    fn a_panicking_worker_ends_its_job_failed() {
        let _g = exclusive();
        let id = begin("install", "@panic/test").unwrap();
        spawn_worker("pf-store-test", &id, |_| panic!("worker panic")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while busy() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let job = get(&id).unwrap();
        assert_eq!(job.state, State::Failed);
        assert!(job.error.unwrap().contains("panicked"));
        assert!(!busy(), "the slot is free for the next job");
    }

    #[test]
    fn job_log_is_bounded_and_tails() {
        let _g = exclusive();
        let id = begin("install", "@log/test").unwrap();
        for i in 0..(LOG_LINES + 50) {
            log_line(&id, format!("line {i}"));
        }
        let job = get(&id).unwrap();
        assert_eq!(job.log.len(), LOG_LINES);
        assert_eq!(job.log.last().unwrap(), &format!("line {}", LOG_LINES + 49));
        finish(&id, Err(anyhow::anyhow!("boom")));
        let job = get(&id).unwrap();
        assert_eq!(job.state, State::Failed);
        assert!(job.error.unwrap().contains("boom"));
        assert!(job.finished_at.is_some());
    }
}
