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
    let id = format!("job-{}-{}", super::catalog::unix_now(), g.counter);
    let job = Job {
        id: id.clone(),
        kind: kind.to_string(),
        target: target.to_string(),
        state: State::Running,
        phase: "queued".into(),
        log: Vec::new(),
        error: None,
        started_at: super::catalog::unix_now(),
        finished_at: None,
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
        j.finished_at = Some(super::catalog::unix_now());
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
    lock().jobs.iter().find(|j| j.id == id).cloned()
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

pub(crate) fn spawn_install(
    plan: Plan,
    plugin_tokens: crate::mgmt::PluginTokens,
) -> Result<String> {
    let target = plan.pkg.clone().unwrap_or_else(|| plan.spec.clone());
    let id = begin("install", &target)?;
    let job_id = id.clone();
    std::thread::Builder::new()
        .name("pf-store-install".into())
        .spawn(move || {
            let result = run_install(&job_id, plan, &plugin_tokens);
            finish(&job_id, result);
            crate::events::emit(crate::events::EventKind::StoreChanged);
        })
        .context("spawn the plugin-install worker")?;
    Ok(id)
}

pub(crate) fn spawn_uninstall(
    pkg: String,
    plugin_tokens: crate::mgmt::PluginTokens,
) -> Result<String> {
    if super::valid_installed_pkg(&pkg).is_err() {
        bail!("not an installable plugin package name");
    }
    let id = begin("uninstall", &pkg)?;
    let job_id = id.clone();
    std::thread::Builder::new()
        .name("pf-store-uninstall".into())
        .spawn(move || {
            let result = run_uninstall(&job_id, &pkg, &plugin_tokens);
            finish(&job_id, result);
            crate::events::emit(crate::events::EventKind::StoreChanged);
        })
        .context("spawn the plugin-uninstall worker")?;
    Ok(id)
}

fn run_install(id: &str, plan: Plan, plugin_tokens: &crate::mgmt::PluginTokens) -> Result<()> {
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
                 install (bun installs into the nearest package.json ABOVE the working directory). \
                 Move or delete it, or add a package.json to the plugins dir.",
                dir.display(),
                p.display()
            ),
            None => format!("{pkg} is not present after install"),
        }
    })?;

    if let (Some(want), Some(got)) = (plan.version.as_deref(), installed.version.as_deref()) {
        if want != got {
            // Do not leave an unreviewed version under a verified badge.
            log_line(id, format!("rolling back: expected {want}, found {got}"));
            set_phase(id, "rolling back");
            let _ = run_runner(
                id,
                &[
                    "remove".to_string(),
                    pkg.clone(),
                    "--plugins".to_string(),
                    dir.to_string_lossy().into_owned(),
                ],
            );
            bail!("installed {pkg}@{got} but the catalog pinned {want} — rolled back");
        }
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

    refresh_plugin_tokens(id, plugin_tokens)?;
    restart_runner(id);
    Ok(())
}

fn run_uninstall(id: &str, pkg: &str, plugin_tokens: &crate::mgmt::PluginTokens) -> Result<()> {
    let dir = super::plugins_dir();
    // Its titles leave with it: nothing would launch an exec tile of a removed plugin.
    let provider = crate::plugins::manifest::id_of_package(pkg);
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
    if let Some(provider) = provider {
        match crate::library::delete_provider(&provider) {
            Ok(n) if n > 0 => log_line(id, format!("removed {n} library titles of {provider}")),
            Ok(_) => {}
            Err(e) => log_line(
                id,
                format!("remove the library titles of {provider}: {e:#}"),
            ),
        }
    }
    refresh_plugin_tokens(id, plugin_tokens)?;
    restart_runner(id);
    Ok(())
}

/// Persist the installed-id token set, then publish the same set to live authentication before
/// the runner starts. The file is the runner's handoff; the lock is the API's view, and swapping
/// it here also revokes an uninstalled plugin's token.
fn refresh_plugin_tokens(id: &str, plugin_tokens: &crate::mgmt::PluginTokens) -> Result<()> {
    set_phase(id, "refreshing credentials");
    let refreshed = crate::mgmt_token::load_or_generate_per_plugin()
        .context("refresh per-plugin credentials")?;
    *plugin_tokens
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = refreshed;
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
    let mut child = Command::new(&program)
        .args(&prefix)
        .args(args)
        // `bun add` must not block on a prompt inside a service.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
    let body = resp
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_vec()
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

#[cfg(test)]
mod tests {
    use super::*;

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
