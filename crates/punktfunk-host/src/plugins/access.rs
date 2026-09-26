//! The operator's folder-access records for plugins: grants and pending requests.
//!
//! `plugin-run/plugin-grants.json` holds what the operator allowed (`{id: {grants, denied}}`; the
//! v1 `{id: [path]}` shape still parses, each string a read-only grant).
//! `plugin-access-pending.json` holds requests a plugin posted that await a decision. Both are
//! written tmp+rename and re-read on every operation, so a CLI write is never stale here.
//!
//! The runner binds these roots (read-only unless `write`); a plugin can ask, never grant —
//! every request is validated against the real filesystem before it is stored. On Linux the
//! runner's unit must see a root too: [`AccessStore::runner_roots`] feeds its drop-in.

use super::manifest::PluginManifest;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use utoipa::ToSchema;

/// Pending rows kept per plugin; a plugin re-asks on every scan, so past this the extra
/// rows are noise, not backlog.
pub const MAX_PENDING: usize = 16;
/// Granted roots kept per plugin.
pub const MAX_GRANTS: usize = 64;

/// One root the operator granted, as recorded on disk. `at`/`by` are audit fields:
/// RFC3339 and `console`/`cli`/`legacy`/`form` (a v1 entry has neither).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Grant {
    pub path: String,
    pub write: bool,
    pub at: String,
    pub by: String,
    /// The plugin forms that handed this path over (`config`, `game:<entry id>`). A grant
    /// with `by: "form"` goes when the last of them lets go.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forms: Vec<String>,
}

/// `by` on a grant only plugin forms made.
const BY_FORM: &str = "form";

/// A path a plugin asked for that the operator has not answered yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PendingRequest {
    pub path: String,
    pub write: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: String,
}

/// One plugin's entry in `plugin-grants.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PluginAccess {
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub denied: Vec<String>,
}

/// What an API read returns for one plugin: its entry plus its pending rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct PluginAccessSnapshot {
    pub plugin: String,
    pub grants: Vec<Grant>,
    pub pending: Vec<PendingRequest>,
    pub denied: Vec<String>,
}

/// The operator's answer to a pending request. `Forget` removes a grant or denial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Forget,
}

/// Per-path answer to a `request` batch: `granted`, `pending`, `denied`, or
/// `refused:<rule>` naming the rule that refused it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestOutcome {
    pub path: String,
    pub outcome: String,
}

/// A real path the plugin runner must see for a sandbox to bind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerRoot {
    pub path: PathBuf,
    pub write: bool,
}

/// `changed` is what the caller emits `plugins.changed` on; `value` is the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutation<T> {
    pub value: T,
    pub changed: bool,
}

const GRANTS_FILE: &str = "plugin-grants.json";
const PENDING_FILE: &str = "plugin-access-pending.json";
const MAX_PATH: usize = 4096;

/// The v1 entry was a bare path list; those grants were read-write in the runner, so on
/// read they become read-only and carry `by: "legacy"` rather than a guessed stamp.
#[derive(Deserialize)]
#[serde(untagged)]
enum AccessEntry {
    V1(Vec<String>),
    V2(PluginAccess),
}

impl AccessEntry {
    fn into_access(self) -> PluginAccess {
        match self {
            AccessEntry::V1(paths) => PluginAccess {
                grants: paths
                    .into_iter()
                    .map(|path| Grant {
                        path,
                        write: false,
                        at: String::new(),
                        by: "legacy".into(),
                        forms: Vec::new(),
                    })
                    .collect(),
                denied: Vec::new(),
            },
            AccessEntry::V2(a) => a,
        }
    }
}

/// What a request is checked against. Injected in tests so no path under the real home or
/// config dir is ever touched.
#[derive(Clone)]
struct PathPolicy {
    home: PathBuf,
    config_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
}

impl PathPolicy {
    /// The rules compare canonical paths, so the roots they protect are canonical too: on Fedora
    /// Atomic `/home` is a link to `/var/home`, and `~/.ssh` is only ever seen as the latter.
    fn resolved(home: PathBuf, config_dir: PathBuf, runtime_dir: Option<PathBuf>) -> Self {
        let real = |p: PathBuf| p.canonicalize().unwrap_or(p);
        Self {
            home: if home.as_os_str().is_empty() {
                home
            } else {
                real(home)
            },
            config_dir: real(config_dir),
            runtime_dir: runtime_dir.map(real),
        }
    }
}

/// Filesystem facts gathered once so the rule itself is pure.
#[derive(Clone, Copy)]
struct PathFacts {
    is_dir: bool,
    owner_uid: Option<u32>,
}

fn path_facts(canonical: &Path) -> PathFacts {
    PathFacts {
        is_dir: canonical.is_dir(),
        #[cfg(unix)]
        owner_uid: {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(canonical).ok().map(|m| m.uid())
        },
        #[cfg(not(unix))]
        owner_uid: None,
    }
}

/// Windows path spelling without the extended prefix, folded for comparison.
#[cfg(windows)]
fn windows_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    text.strip_prefix(r"\\?\")
        .unwrap_or(&text)
        .replace('/', "\\")
        .to_ascii_lowercase()
}

/// Path comparisons are case-insensitive on Windows only; elsewhere bytes rule.
#[cfg(windows)]
fn path_eq(a: &Path, b: &Path) -> bool {
    windows_path(a) == windows_path(b)
}

#[cfg(not(windows))]
fn path_eq(a: &Path, b: &Path) -> bool {
    a == b
}

fn same_path(a: &str, b: &str) -> bool {
    path_eq(Path::new(a), Path::new(b))
}

/// Is `path` at or below `base`? On Windows the `\\?\` a canonical grant carries, the slash
/// direction, and the case do not count.
#[cfg(windows)]
pub(crate) fn within(path: &Path, base: &Path) -> bool {
    let (path, base) = (windows_path(path), windows_path(base));
    path == base || path.starts_with(&format!("{base}\\"))
}

#[cfg(not(windows))]
pub(crate) fn within(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

/// The part of `path` below `base`, with `within`'s case rules applied.
fn rel_after(path: &Path, base: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        let norm = |p: &Path| {
            let s = p.to_string_lossy();
            s.strip_prefix(r"\\?\").unwrap_or(&s).to_ascii_lowercase()
        };
        norm(path)
            .strip_prefix(&format!("{}\\", norm(base)))
            .map(str::to_string)
    }
    #[cfg(not(windows))]
    {
        path.strip_prefix(base)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    }
}

/// A grant or manifest path: `~/x` expands against the policy home, anything else is as stored.
fn home_path(p: &str, policy: &PathPolicy) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) if !policy.home.as_os_str().is_empty() => policy.home.join(rest),
        _ => PathBuf::from(p),
    }
}

/// Is `canonical` inside a declared manifest root or an existing grant that allows `write`?
/// Declared roots (`(path, writable)`) are canonicalized when they resolve so both sides speak
/// the same spelling. A read-only root never answers a write request.
fn covers(
    declared: &[(PathBuf, bool)],
    grants: &[Grant],
    canonical: &Path,
    write: bool,
    policy: &PathPolicy,
) -> bool {
    declared.iter().any(|(r, writable)| {
        let root = r.canonicalize().unwrap_or_else(|_| r.clone());
        (*writable || !write) && within(canonical, &root)
    }) || grants
        .iter()
        .any(|g| (g.write || !write) && within(canonical, &home_path(&g.path, policy)))
}

/// Windows-only refusals: `C:\Users`, the profile roots directly under it, `C:\Windows` and
/// everything inside, and the sensitive dirs inside any profile root. Drive roots are
/// already refused as filesystem roots.
#[cfg(windows)]
fn windows_protected(canonical: &Path) -> bool {
    let users = Path::new(r"C:\Users");
    if path_eq(canonical, users) || canonical.parent().is_some_and(|p| path_eq(p, users)) {
        return true;
    }
    if within(canonical, Path::new(r"C:\Windows")) {
        return true;
    }
    if let Some(rel) = rel_after(canonical, users) {
        let mut it = rel.split('\\');
        let _profile = it.next();
        if let Some(first) = it.next() {
            if first == ".ssh" || first == ".gnupg" {
                return true;
            }
            if first == ".config" && it.next().is_some_and(|n| n.starts_with("punktfunk")) {
                return true;
            }
        }
    }
    false
}

/// The safety refusals for a path, in order; `None` means the path is a directory the host
/// could in principle grant. `operator` is true for a grant the operator makes (CLI, console)
/// and false for a plugin's request or manifest. Coverage, sticky denial, and caps are the
/// caller's business and are checked after this.
fn refusal_rule(
    raw: &Path,
    canonical: &Path,
    write: bool,
    operator: bool,
    policy: &PathPolicy,
    facts: PathFacts,
) -> Option<&'static str> {
    if !raw.is_absolute() {
        return Some("not_absolute");
    }
    if raw.components().any(|c| matches!(c, Component::ParentDir)) {
        return Some("parent_traversal");
    }
    if raw.as_os_str().len() > MAX_PATH {
        return Some("too_long");
    }
    if !facts.is_dir {
        return Some("not_directory");
    }
    // Broad root: the filesystem root, the operator's home or anything above it, the punktfunk
    // config dir or anything above it. A plugin may ask for `~/Games`, never for `~`.
    if canonical.parent().is_none()
        || (!policy.home.as_os_str().is_empty() && within(&policy.home, canonical))
        || (!policy.config_dir.as_os_str().is_empty() && within(&policy.config_dir, canonical))
    {
        return Some("broad_root");
    }
    let home = &policy.home;
    let mut protected = (!policy.config_dir.as_os_str().is_empty()
        && within(canonical, &policy.config_dir))
        || policy
            .runtime_dir
            .as_ref()
            .is_some_and(|r| within(canonical, r));
    if !home.as_os_str().is_empty() {
        protected = protected
            || within(canonical, &home.join(".ssh"))
            || within(canonical, &home.join(".gnupg"))
            || punktfunk_dot_config(home, canonical);
    }
    #[cfg(unix)]
    {
        // udisks mounts second drives and SD cards below `/run/media`: volumes, like `/mnt/x`.
        let volume = canonical
            .parent()
            .is_some_and(|p| within(p, Path::new("/run/media")));
        protected = protected
            || (!volume
                && ["/proc", "/sys", "/dev", "/run"]
                    .iter()
                    .any(|r| within(canonical, Path::new(r))));
    }
    #[cfg(windows)]
    {
        protected = protected || windows_protected(canonical);
    }
    if protected {
        return Some("protected_path");
    }
    if write && !write_owned(canonical, operator, policy, facts) {
        return Some("write_not_owned");
    }
    None
}

/// `~/.config/<anything starting with punktfunk>` — the host's own dir and any sibling a
/// future component adds there stay ungrantable.
fn punktfunk_dot_config(home: &Path, canonical: &Path) -> bool {
    let cfg_root = home.join(".config");
    let Some(rel) = rel_after(canonical, &cfg_root) else {
        return false;
    };
    rel.split(std::path::MAIN_SEPARATOR)
        .next()
        .is_some_and(|n| n.starts_with("punktfunk"))
}

/// May a plugin hold WRITE on `canonical`? On Windows only the operator grants it, and never in
/// a tree Windows or installed software runs from: every plugin shares the LocalService ACE.
#[cfg(windows)]
fn write_owned(canonical: &Path, operator: bool, _policy: &PathPolicy, _facts: PathFacts) -> bool {
    operator
        && ![
            r"C:\Program Files",
            r"C:\Program Files (x86)",
            r"C:\ProgramData",
        ]
        .iter()
        .any(|root| within(canonical, Path::new(root)))
}

/// The operator's own tree, or a mount they own — either means no one else's files change.
#[cfg(unix)]
fn write_owned(canonical: &Path, _operator: bool, policy: &PathPolicy, facts: PathFacts) -> bool {
    if !policy.home.as_os_str().is_empty() && within(canonical, &policy.home) {
        return true;
    }
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    facts.owner_uid == Some(euid)
}

#[cfg(not(any(unix, windows)))]
fn write_owned(canonical: &Path, _operator: bool, policy: &PathPolicy, _facts: PathFacts) -> bool {
    !policy.home.as_os_str().is_empty() && within(canonical, &policy.home)
}

fn now_rfc3339() -> String {
    jiff::Timestamp::now().to_string()
}

fn to_io(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(e.to_string())
}

/// tmp-write + rename so a reader never sees half a file; the temp is 0600 on Unix before it
/// becomes the real name. One writer for both files.
fn write_json_atomic<T: Serialize>(dir: &Path, name: &str, value: &T) -> io::Result<()> {
    let tmp = dir.join(format!("{name}.tmp"));
    let body = serde_json::to_string_pretty(value).map_err(to_io)?;
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, dir.join(name))
}

/// The ACL half of a grant, mapped into `io::Error` for the store's signatures. Runs only
/// once a grant is certain to be recorded — a cap refusal never leaves an unrecorded ACE.
fn apply_acl(dir: &Path, write: bool) -> io::Result<()> {
    #[cfg(test)]
    ACL_CALLS.with(|calls| calls.set(calls.get() + 1));
    super::grant_acl(dir, write)
}

/// Match the ACL on `path` to the grants left after one came off. Every Windows plugin is the
/// same principal, so the ACE goes only when no plugin holds the folder, and a write ACE falls
/// back to read when only read grants remain. Best-effort: the record is already written.
fn settle_acl(access: &BTreeMap<String, PluginAccess>, path: &str) {
    let held: Vec<bool> = access
        .values()
        .flat_map(|a| a.grants.iter())
        .filter(|g| same_path(&g.path, path))
        .map(|g| g.write)
        .collect();
    let dir = Path::new(path);
    let result = if held.is_empty() {
        #[cfg(test)]
        ACL_REMOVES.with(|calls| calls.set(calls.get() + 1));
        super::revoke_acl(dir)
    } else {
        apply_acl(dir, held.contains(&true))
    };
    if let Err(e) = result {
        tracing::warn!(path, error = %e, "revoked folder keeps its runner ACE");
    }
}

#[cfg(test)]
thread_local! {
    static ACL_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ACL_REMOVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn acl_calls() -> usize {
    ACL_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn acl_removes() -> usize {
    ACL_REMOVES.with(std::cell::Cell::get)
}

/// Grants + denials in the runner-data directory, plus the pending queue under the config dir.
pub struct AccessStore {
    config_dir: PathBuf,
    runner_dir: PathBuf,
    policy: PathPolicy,
    lock: Mutex<()>,
}

impl AccessStore {
    /// The operator's real store: runner grants under `plugin-run`, pending requests under
    /// `config_dir`, and refusals checked against this host's real protected roots.
    pub fn open(config_dir: PathBuf) -> Self {
        let home = crate::plugins::manifest::home_dir().unwrap_or_default();
        #[cfg(unix)]
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|s| !s.is_empty())
            .map(PathBuf::from);
        #[cfg(not(unix))]
        let runtime_dir = None;
        Self::open_with(
            config_dir.clone(),
            // The policy checks containment against the dir this store actually serves: the
            // management API passes a dedicated access dir, and its contents must refuse.
            PathPolicy::resolved(home, config_dir, runtime_dir),
        )
    }

    fn open_with(config_dir: PathBuf, policy: PathPolicy) -> Self {
        let runner_dir = config_dir.join(super::RUNNER_DATA_DIR);
        if let Err(e) = migrate_grants(&config_dir, &runner_dir) {
            tracing::warn!(error = %e, "plugin grants were not migrated to the runner directory");
        }
        Self {
            config_dir,
            runner_dir,
            policy,
            lock: Mutex::new(()),
        }
    }

    /// Both records, fresh — a `plugins grant` from another process must be seen.
    fn load_access(&self) -> BTreeMap<String, PluginAccess> {
        let path = self.runner_dir.join(GRANTS_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return BTreeMap::new();
        };
        match serde_json::from_str::<BTreeMap<String, AccessEntry>>(&text) {
            Ok(map) => map
                .into_iter()
                .map(|(id, e)| (id, e.into_access()))
                .collect(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "plugin grants: unreadable");
                BTreeMap::new()
            }
        }
    }

    fn load_pending(&self) -> BTreeMap<String, Vec<PendingRequest>> {
        let path = self.config_dir.join(PENDING_FILE);
        let Ok(text) = std::fs::read_to_string(&path) else {
            return BTreeMap::new();
        };
        match serde_json::from_str(&text) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "plugin access requests: unreadable");
                BTreeMap::new()
            }
        }
    }

    fn write_access(&self, map: &BTreeMap<String, PluginAccess>) -> io::Result<()> {
        prepare_runner_dir(&self.runner_dir)?;
        write_json_atomic(&self.runner_dir, GRANTS_FILE, map)
    }

    fn write_pending(&self, map: &BTreeMap<String, Vec<PendingRequest>>) -> io::Result<()> {
        pf_paths::create_private_dir(&self.config_dir)?;
        write_json_atomic(&self.config_dir, PENDING_FILE, map)
    }

    /// Record a batch of requests from `id`. A path already reachable, already pending, or
    /// sticky-denied is answered without a new row; anything the host would never grant is
    /// `refused:<rule>` here rather than offered to the operator.
    pub fn request(
        &self,
        id: &str,
        paths: &[(String, bool)],
        reason: Option<String>,
    ) -> io::Result<Mutation<Vec<RequestOutcome>>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let access = self.load_access();
        let mut pending_all = self.load_pending();
        let declared: Vec<(PathBuf, bool)> = crate::plugins::manifest::for_provider(id)
            .map(|m| {
                let reads = m.reads.iter().map(|p| (home_path(p, &self.policy), false));
                reads
                    .chain(m.writes.iter().map(|p| (home_path(p, &self.policy), true)))
                    .collect()
            })
            .unwrap_or_default();
        let entry = access.get(id).cloned().unwrap_or_default();
        let mut pending = pending_all.remove(id).unwrap_or_default();
        let mut changed = false;
        let outcomes: Vec<RequestOutcome> = paths
            .iter()
            .map(|(raw, write)| {
                self.request_one(
                    raw,
                    *write,
                    &reason,
                    &declared,
                    &entry,
                    &mut pending,
                    &mut changed,
                )
            })
            .collect();
        // The console lists only pending rows; a refusal is visible nowhere else.
        for o in &outcomes {
            if let Some(rule) = o.outcome.strip_prefix("refused:") {
                tracing::info!(plugin = id, path = %o.path, rule, "plugin folder request refused");
            }
        }
        if changed {
            pending_all.insert(id.to_string(), pending);
            self.write_pending(&pending_all)?;
        }
        Ok(Mutation {
            value: outcomes,
            changed,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn request_one(
        &self,
        raw: &str,
        write: bool,
        reason: &Option<String>,
        declared: &[(PathBuf, bool)],
        entry: &PluginAccess,
        pending: &mut Vec<PendingRequest>,
        changed: &mut bool,
    ) -> RequestOutcome {
        let outcome = |path: String, outcome: &str| RequestOutcome {
            path,
            outcome: outcome.into(),
        };
        let raw_path = Path::new(raw);
        // Shape first: a malformed string is refused before it touches the filesystem.
        let canonical = match raw_path.canonicalize() {
            Ok(c) => c,
            Err(_) if raw_path.is_absolute() => {
                // The refusal rule still applies its string checks; a path that does not
                // resolve is simply not a directory.
                let facts = PathFacts {
                    is_dir: false,
                    owner_uid: None,
                };
                let reason_str =
                    refusal_rule(raw_path, raw_path, write, false, &self.policy, facts)
                        .unwrap_or("not_directory");
                return outcome(raw.to_string(), &format!("refused:{reason_str}"));
            }
            Err(_) => return outcome(raw.to_string(), "refused:not_absolute"),
        };
        let canon = canonical.to_string_lossy().into_owned();
        let facts = path_facts(&canonical);
        // A repost returns pending even at the cap — but a stale row under a path that is
        // refused today answers refused, never pending.
        if let Some(row) = pending.iter().find(|p| same_path(&p.path, &canon)) {
            return match refusal_rule(raw_path, &canonical, row.write, false, &self.policy, facts) {
                Some(rule) => outcome(canon, &format!("refused:{rule}")),
                None => outcome(canon, "pending"),
            };
        }
        if let Some(rule) = refusal_rule(raw_path, &canonical, write, false, &self.policy, facts) {
            return outcome(canon, &format!("refused:{rule}"));
        }
        if covers(declared, &entry.grants, &canonical, write, &self.policy) {
            return outcome(canon, "granted");
        }
        if entry.denied.iter().any(|d| same_path(d, &canon)) {
            return outcome(canon, "denied");
        }
        if entry.grants.len() >= MAX_GRANTS {
            return outcome(canon, "refused:grant_limit");
        }
        if pending.len() >= MAX_PENDING {
            return outcome(canon, "refused:pending_limit");
        }
        pending.push(PendingRequest {
            path: canon.clone(),
            write,
            reason: reason.clone(),
            at: now_rfc3339(),
        });
        *changed = true;
        outcome(canon, "pending")
    }

    /// The operator's answer. `allow` needs a matching pending row and applies the platform
    /// ACL before anything is stored, so a failed grant never lands in the file.
    pub fn decide(
        &self,
        id: &str,
        path: &str,
        decision: Decision,
        by: &str,
    ) -> io::Result<Mutation<PluginAccessSnapshot>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let stored_path = resolve_stored(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path must be absolute with no '..' component",
            )
        })?;
        let mut access = self.load_access();
        let mut pending_all = self.load_pending();
        let mut grants_changed = false;
        let mut pending_changed = false;
        let mut ungranted = false;
        match decision {
            Decision::Allow => {
                // The cap is checked before the ACL and before the pending row moves: a
                // refused allow leaves no ACE on disk and the request stays actionable.
                let row = pending_all.get(id).and_then(|rows| {
                    rows.iter()
                        .find(|p| same_path(&p.path, &stored_path))
                        .cloned()
                });
                let Some(row) = row else {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "no pending request for that path",
                    ));
                };
                let entry = access.entry(id.to_string()).or_default();
                if !entry.grants.iter().any(|g| same_path(&g.path, &row.path))
                    && entry.grants.len() >= MAX_GRANTS
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "the grant limit is reached",
                    ));
                }
                apply_acl(Path::new(&row.path), row.write)?;
                if let Some(rows) = pending_all.get_mut(id) {
                    rows.retain(|p| !same_path(&p.path, &stored_path));
                }
                pending_changed = true;
                if let Some(g) = entry
                    .grants
                    .iter_mut()
                    .find(|g| same_path(&g.path, &row.path))
                {
                    g.write = row.write;
                    g.at = now_rfc3339();
                    g.by = by.to_string();
                } else {
                    entry.grants.push(Grant {
                        path: row.path,
                        write: row.write,
                        at: now_rfc3339(),
                        by: by.to_string(),
                        forms: Vec::new(),
                    });
                }
                grants_changed = true;
            }
            Decision::Deny => {
                let found = pending_all.get_mut(id).is_some_and(|rows| {
                    match rows.iter().position(|p| same_path(&p.path, &stored_path)) {
                        Some(i) => {
                            rows.remove(i);
                            true
                        }
                        None => false,
                    }
                });
                if !found {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "no pending request for that path",
                    ));
                }
                pending_changed = true;
                let entry = access.entry(id.to_string()).or_default();
                if !entry.denied.iter().any(|d| same_path(d, &stored_path)) {
                    entry.denied.push(stored_path.clone());
                }
                grants_changed = true;
            }
            Decision::Forget => {
                if let Some(entry) = access.get_mut(id) {
                    let before = entry.grants.len() + entry.denied.len();
                    let granted = entry.grants.len();
                    entry.grants.retain(|g| !same_path(&g.path, &stored_path));
                    ungranted = entry.grants.len() != granted;
                    entry.denied.retain(|d| !same_path(d, &stored_path));
                    grants_changed = entry.grants.len() + entry.denied.len() != before;
                }
            }
        }
        if grants_changed {
            self.write_access(&access)?;
        }
        if ungranted {
            settle_acl(&access, &stored_path);
        }
        if pending_changed {
            self.write_pending(&pending_all)?;
        }
        Ok(Mutation {
            value: snapshot_of(id, &access, &pending_all),
            changed: grants_changed || pending_changed,
        })
    }

    /// Every plugin with a recorded entry or pending row.
    pub fn snapshot(&self) -> io::Result<Vec<PluginAccessSnapshot>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let access = self.load_access();
        let pending = self.load_pending();
        let mut ids: Vec<String> = access
            .keys()
            .chain(pending.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        ids.sort();
        Ok(ids
            .into_iter()
            .map(|id| snapshot_of(&id, &access, &pending))
            .collect())
    }

    /// One plugin's grants, pending rows, and denials.
    pub fn snapshot_for(&self, id: &str) -> io::Result<PluginAccessSnapshot> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(snapshot_of(id, &self.load_access(), &self.load_pending()))
    }

    /// The grants the runner binds for `id` — empty when the file is absent or unreadable.
    pub fn grants_for(&self, id: &str) -> Vec<Grant> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.load_access()
            .get(id)
            .map(|a| a.grants.clone())
            .unwrap_or_default()
    }

    /// Every real path some sandbox binds: manifest `reads`/`writes` plus all grants, each
    /// judged on its real path so a link cannot bring in `~/.ssh`. A path spelled through a
    /// link adds the real directory holding that link, which bwrap follows in the runner.
    /// A root inside a plugin-writable root is dropped: that plugin could swap it for a link.
    pub fn runner_roots(&self, manifests: &BTreeMap<String, PluginManifest>) -> Vec<RunnerRoot> {
        let access = {
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.load_access()
        };
        let declared = manifests.values().flat_map(|m| {
            let reads = m.reads.iter().map(|p| (p.as_str(), false, false));
            reads.chain(m.writes.iter().map(|p| (p.as_str(), true, false)))
        });
        let granted = access
            .values()
            .flat_map(|a| a.grants.iter().map(|g| (g.path.as_str(), g.write, true)));
        // A missing root is the drop-in's `-` prefix's business, and a root may be a file.
        let allowed = |spelled: &Path, real: &Path, write: bool, operator: bool| {
            let facts = PathFacts {
                is_dir: true,
                ..path_facts(real)
            };
            refusal_rule(spelled, real, write, operator, &self.policy, facts).is_none()
        };
        let mut roots = Vec::new();
        for (p, write, operator) in declared.chain(granted) {
            let spelled = home_path(p, &self.policy);
            let real = spelled.canonicalize().unwrap_or_else(|_| spelled.clone());
            if !allowed(&spelled, &real, write, operator) {
                continue;
            }
            if real != spelled {
                let holder = spelled
                    .ancestors()
                    .skip(1)
                    .find(|a| a.canonicalize().is_ok_and(|c| c == *a));
                if let Some(dir) = holder.filter(|d| allowed(d, d, false, operator)) {
                    roots.push(RunnerRoot {
                        path: dir.to_path_buf(),
                        write: false,
                    });
                }
            }
            roots.push(RunnerRoot { path: real, write });
        }
        // Writable first, so the dedup keeps the stronger of two entries for one path.
        roots.sort_by(|a, b| a.path.cmp(&b.path).then(b.write.cmp(&a.write)));
        roots.dedup_by(|later, kept| later.path == kept.path);
        let writable: Vec<PathBuf> = roots
            .iter()
            .filter(|r| r.write)
            .map(|r| r.path.clone())
            .collect();
        roots.retain(|r| !writable.iter().any(|w| *w != r.path && within(&r.path, w)));
        roots
    }

    /// The operator's direct grant (CLI, access page): an existing directory the host would
    /// grant on request, ACL applied before it is recorded. Re-granting updates it and makes it
    /// the operator's own, so no form's release takes it away. A pending request for the same
    /// path is answered by it.
    pub fn grant(&self, id: &str, dir: &Path, write: bool, by: &str) -> io::Result<Vec<Grant>> {
        self.record(id, dir, write, by, None)
    }

    /// A grant a plugin form hands over: `form` joins the grant's holders and never narrows
    /// what is there. A new grant is `by: "form"` and goes with its last holder.
    pub fn hand(&self, id: &str, dir: &Path, write: bool, form: &str) -> io::Result<Vec<Grant>> {
        self.record(id, dir, write, BY_FORM, Some(form))
    }

    /// `form` now hands over only `keep`; it lets go of every other grant it holds. A grant
    /// only forms made goes with its last holder, and the operator's own grant stays. A kept
    /// file keeps its folder's grant.
    pub fn release(
        &self,
        id: &str,
        form: &str,
        keep: &[String],
    ) -> io::Result<Mutation<Vec<Grant>>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let kept: Vec<String> = keep.iter().flat_map(|p| grant_keys(Path::new(p))).collect();
        let mut access = self.load_access();
        let Some(entry) = access.get_mut(id) else {
            return Ok(Mutation {
                value: Vec::new(),
                changed: false,
            });
        };
        let mut changed = false;
        let mut gone = Vec::new();
        entry.grants.retain_mut(|g| {
            if !g.forms.iter().any(|f| f == form) || kept.iter().any(|k| same_path(k, &g.path)) {
                return true;
            }
            g.forms.retain(|f| f != form);
            changed = true;
            let last = g.forms.is_empty() && g.by == BY_FORM;
            if last {
                gone.push(g.path.clone());
            }
            !last
        });
        let grants = entry.grants.clone();
        if changed {
            self.write_access(&access)?;
            for path in &gone {
                settle_acl(&access, path);
            }
        }
        Ok(Mutation {
            value: grants,
            changed,
        })
    }

    fn record(
        &self,
        id: &str,
        dir: &Path,
        write: bool,
        by: &str,
        form: Option<&str>,
    ) -> io::Result<Vec<Grant>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !dir.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{}' is not a directory", dir.display()),
            ));
        }
        let canonical = dir.canonicalize()?;
        let facts = path_facts(&canonical);
        if let Some(rule) = refusal_rule(&canonical, &canonical, write, true, &self.policy, facts) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("'{}' can't be granted ({rule})", canonical.display()),
            ));
        }
        let mut access = self.load_access();
        let entry = access.entry(id.to_string()).or_default();
        let path = canonical.to_string_lossy().into_owned();
        let existing = entry.grants.iter().position(|g| same_path(&g.path, &path));
        // Cap before ACL: a refused grant must not leave an ACE on the directory.
        if existing.is_none() && entry.grants.len() >= MAX_GRANTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the grant limit is reached",
            ));
        }
        let write = match (existing, form) {
            (Some(i), Some(_)) => write || entry.grants[i].write,
            _ => write,
        };
        apply_acl(&canonical, write)?;
        match (existing, form) {
            (Some(i), Some(form)) => {
                let g = &mut entry.grants[i];
                g.write = write;
                if !g.forms.iter().any(|f| f == form) {
                    g.forms.push(form.to_string());
                }
            }
            (Some(i), None) => {
                let g = &mut entry.grants[i];
                g.write = write;
                g.at = now_rfc3339();
                g.by = by.to_string();
            }
            (None, _) => entry.grants.push(Grant {
                path: path.clone(),
                write,
                at: now_rfc3339(),
                by: by.to_string(),
                forms: form.map(|f| vec![f.to_string()]).unwrap_or_default(),
            }),
        }
        let grants = entry.grants.clone();
        self.write_access(&access)?;
        let mut pending = self.load_pending();
        if let Some(rows) = pending.get_mut(id) {
            let before = rows.len();
            rows.retain(|p| !same_path(&p.path, &path));
            if rows.len() != before {
                self.write_pending(&pending)?;
            }
        }
        Ok(grants)
    }

    /// Remove a grant by its stored absolute spelling — no canonicalize, so an unplugged
    /// drive's grant still comes off. A no-op revoke writes nothing.
    pub fn revoke(&self, id: &str, path: &Path) -> io::Result<Vec<Grant>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let stored = resolve_stored(&path.to_string_lossy()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path must be absolute with no '..' component",
            )
        })?;
        let mut access = self.load_access();
        let mut changed = false;
        let grants = match access.get_mut(id) {
            Some(entry) => {
                let before = entry.grants.len();
                entry.grants.retain(|g| !same_path(&g.path, &stored));
                changed = entry.grants.len() != before;
                entry.grants.clone()
            }
            None => Vec::new(),
        };
        if changed {
            self.write_access(&access)?;
            settle_acl(&access, &stored);
        }
        Ok(grants)
    }
}

/// Seed the directory-bound grants file from the pre-directory location. The old copy stays as
/// rollback data. Two stats when there is nothing to move: `open` runs on every launch check.
fn migrate_grants(config_dir: &Path, runner_dir: &Path) -> io::Result<()> {
    let target = runner_dir.join(GRANTS_FILE);
    let legacy = config_dir.join(GRANTS_FILE);
    if target.exists() || !legacy.exists() {
        return Ok(());
    }
    prepare_runner_dir(runner_dir)?;
    let tmp = runner_dir.join(format!("{GRANTS_FILE}.tmp"));
    std::fs::copy(legacy, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, target)
}

/// Harden the runner directory, then hand the Windows runner its read ACE back.
fn prepare_runner_dir(dir: &Path) -> io::Result<()> {
    pf_paths::create_secret_dir(dir)?;
    crate::plugins::converge_runner_data_dir(dir).map_err(to_io)
}

/// `path` as the files record it: canonical when it resolves, else the literal absolute
/// spelling so a decision can still name an unplugged drive.
fn resolve_stored(path: &str) -> Option<String> {
    let p = Path::new(path);
    if let Ok(c) = p.canonicalize() {
        return Some(c.to_string_lossy().into_owned());
    }
    (p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir)))
        .then(|| path.to_string())
}

/// The grants a handed path may stand on: its own, and its folder's for a file. A path that no
/// longer resolves keeps both, so a deleted file never costs the folder it comes back to.
fn grant_keys(p: &Path) -> Vec<String> {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let own = real(p);
    let folder = (own.is_file() || !own.exists())
        .then(|| own.parent().map(real))
        .flatten();
    [Some(own), folder]
        .into_iter()
        .flatten()
        .map(|k| k.to_string_lossy().into_owned())
        .collect()
}

fn snapshot_of(
    id: &str,
    access: &BTreeMap<String, PluginAccess>,
    pending: &BTreeMap<String, Vec<PendingRequest>>,
) -> PluginAccessSnapshot {
    let entry = access.get(id).cloned().unwrap_or_default();
    PluginAccessSnapshot {
        plugin: id.to_string(),
        grants: entry.grants,
        pending: pending.get(id).cloned().unwrap_or_default(),
        denied: entry.denied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sandboxed view of the host: temp home/config/runtime so the real ones never match.
    struct Fixture {
        _tmp: tempfile::TempDir,
        store_dir: PathBuf,
        policy: PathPolicy,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize once: on macOS tempdir is a `/var` symlink to `/private/var`, and the
        // policy compares canonical paths.
        let root = tmp.path().canonicalize().unwrap();
        for d in ["home", "config", "runtime", "store", "home/.config"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        Fixture {
            store_dir: root.join("store"),
            policy: PathPolicy {
                home: root.join("home"),
                config_dir: root.join("config"),
                runtime_dir: Some(root.join("runtime")),
            },
            _tmp: tmp,
        }
    }

    impl Fixture {
        fn store(&self) -> AccessStore {
            AccessStore::open_with(self.store_dir.clone(), self.policy.clone())
        }
        fn dir(&self, rel: &str) -> PathBuf {
            let p = self
                .store_dir
                .parent()
                .unwrap()
                .join(rel)
                .canonicalize()
                .unwrap_or_else(|_| self.store_dir.parent().unwrap().join(rel));
            std::fs::create_dir_all(&p).unwrap();
            p.canonicalize().unwrap()
        }
        fn request(&self, raw: &str) -> String {
            self.request_w(raw, false)
        }
        fn request_w(&self, raw: &str, write: bool) -> String {
            self.store()
                .request("demo", &[(raw.to_string(), write)], Some("needs it".into()))
                .unwrap()
                .value
                .remove(0)
                .outcome
        }
    }

    #[test]
    fn shape_rules_refuse_before_the_filesystem() {
        let f = fixture();
        assert_eq!(f.request("relative/dir"), "refused:not_absolute");
        assert_eq!(f.request("/tmp/../etc"), "refused:parent_traversal");
        let long = format!("/{}", "x".repeat(MAX_PATH));
        assert_eq!(f.request(&long), "refused:too_long");
        assert_eq!(
            f.request("/definitely/missing/punktfunk-test"),
            "refused:not_directory"
        );
        let file = f._tmp.path().join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(f.request(file.to_str().unwrap()), "refused:not_directory");
    }

    /// Fedora Atomic spells `$HOME` under `/home`, a link to `/var/home`, while a request only
    /// ever arrives canonical. The rules must still see the home and its `.ssh`.
    #[cfg(unix)]
    #[test]
    fn a_home_behind_a_link_keeps_its_refusals() {
        let f = fixture();
        let root = f.policy.home.parent().unwrap().to_path_buf();
        let real = root.join("var/home/u");
        std::fs::create_dir_all(real.join(".ssh")).unwrap();
        std::fs::create_dir_all(real.join("Games")).unwrap();
        std::os::unix::fs::symlink(root.join("var/home"), root.join("linkhome")).unwrap();
        let policy =
            PathPolicy::resolved(root.join("linkhome/u"), f.policy.config_dir.clone(), None);
        let store = AccessStore::open_with(f.store_dir.clone(), policy);
        let ask = |p: &Path| {
            store
                .request("demo", &[(p.to_string_lossy().into_owned(), false)], None)
                .unwrap()
                .value
                .remove(0)
                .outcome
        };
        assert_eq!(ask(&real), "refused:broad_root");
        assert_eq!(ask(&real.join(".ssh")), "refused:protected_path");
        assert_eq!(ask(&real.join("Games")), "pending");
    }

    #[test]
    fn broad_and_protected_roots_are_refused() {
        let f = fixture();
        assert_eq!(f.request("/"), "refused:broad_root");
        let home = f.policy.home.to_str().unwrap().to_string();
        assert_eq!(f.request(&home), "refused:broad_root");
        // An ancestor of home is as broad as home itself.
        let ancestor = f
            .policy
            .home
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(f.request(&ancestor), "refused:broad_root");
        let cfg_parent = f
            .policy
            .config_dir
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(f.request(&cfg_parent), "refused:broad_root");
        // The config dir itself is broad_root; only inside it is protected.
        assert_eq!(
            f.request(f.policy.config_dir.to_str().unwrap()),
            "refused:broad_root"
        );
        let inside_cfg = f.dir("config/nested");
        assert_eq!(
            f.request(inside_cfg.to_str().unwrap()),
            "refused:protected_path"
        );
        for rel in [
            "home/.ssh",
            "home/.gnupg",
            "home/.config/punktfunk",
            "home/.config/punktfunk-extra",
            "runtime",
        ] {
            let p = f.dir(rel);
            assert_eq!(
                f.request(p.to_str().unwrap()),
                "refused:protected_path",
                "{rel}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_system_dirs_are_protected() {
        let f = fixture();
        for p in ["/proc", "/sys", "/dev", "/run"] {
            if Path::new(p).is_dir() {
                assert_eq!(f.request(p), "refused:protected_path", "{p}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn removable_volumes_under_run_media_are_grantable() {
        let f = fixture();
        let dir = PathFacts {
            is_dir: true,
            owner_uid: None,
        };
        let rule = |p: &str| refusal_rule(Path::new(p), Path::new(p), false, false, &f.policy, dir);
        assert_eq!(rule("/run/media/deck/SD/Emulation/roms"), None);
        assert_eq!(rule("/run/media/mmcblk0p1"), None);
        assert_eq!(rule("/run/media"), Some("protected_path"));
        assert_eq!(rule("/run/user/1000"), Some("protected_path"));
        assert_eq!(rule("/run/mediax/games"), Some("protected_path"));
    }

    fn manifest(reads: &[&str], writes: &[&str]) -> BTreeMap<String, PluginManifest> {
        let m = PluginManifest {
            schema: 1,
            id: "demo".into(),
            reads: reads.iter().map(|s| (*s).to_string()).collect(),
            writes: writes.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        };
        BTreeMap::from([("demo".to_string(), m)])
    }

    fn targets(roots: &[RunnerRoot]) -> Vec<(String, bool)> {
        let s = |p: &Path| p.to_string_lossy().into_owned();
        roots.iter().map(|r| (s(&r.path), r.write)).collect()
    }

    #[cfg(unix)]
    #[test]
    fn a_path_through_a_link_brings_the_links_directory() {
        let f = fixture();
        let lib = f.dir("home/.local/share/Steam");
        f.dir("home/.steam");
        std::os::unix::fs::symlink(&lib, f.policy.home.join(".steam/steam")).unwrap();
        std::os::unix::fs::symlink(&lib, f.policy.home.join("steam-at-home")).unwrap();
        let roots = f
            .store()
            .runner_roots(&manifest(&["~/.steam/steam", "~/steam-at-home"], &[]));
        let h = |rel: &str| f.policy.home.join(rel).to_string_lossy().into_owned();
        // The home itself never holds a link for the runner: the second link reaches only `lib`.
        assert_eq!(
            targets(&roots),
            vec![(h(".local/share/Steam"), false), (h(".steam"), false)]
        );
    }

    #[test]
    fn runner_roots_join_manifests_and_grants() {
        let f = fixture();
        let home = f.policy.home.clone();
        let rom = f.dir("home/Emu");
        let games = f.dir("data/games");
        f.store().grant("demo", &rom, false, "cli").unwrap();
        f.store().grant("demo", &games, false, "cli").unwrap();
        let roots = f
            .store()
            .runner_roots(&manifest(&["~/.config/retroarch", "relative"], &["~/rw"]));
        let h = |rel: &str| home.join(rel).to_string_lossy().into_owned();
        assert_eq!(
            targets(&roots),
            vec![
                (games.to_string_lossy().into_owned(), false),
                (h(".config/retroarch"), false),
                (h("Emu"), false),
                (h("rw"), true),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn runner_roots_refuse_protected_paths_and_links_to_them() {
        let f = fixture();
        f.dir("home/.ssh");
        std::os::unix::fs::symlink(f.policy.home.join(".ssh"), f.policy.home.join("keys")).unwrap();
        let roots = f.store().runner_roots(&manifest(
            &["~/.ssh", "~/keys", "~", "~/.config/punktfunk"],
            &[],
        ));
        assert_eq!(targets(&roots), vec![]);
    }

    #[cfg(unix)]
    #[test]
    fn runner_roots_drop_a_root_a_plugin_can_rewrite() {
        let f = fixture();
        f.dir("home/rw/inner");
        f.dir("home/ro/rw");
        let roots = f.store().runner_roots(&manifest(
            &["~/rw/inner", "~/ro", "~/dup"],
            &["~/rw", "~/ro/rw", "~/dup"],
        ));
        let h = |rel: &str| f.policy.home.join(rel).to_string_lossy().into_owned();
        // `~/rw/inner` sits in a writable root; a write root inside a read-only one stays.
        assert_eq!(
            targets(&roots),
            vec![
                (h("dup"), true),
                (h("ro"), false),
                (h("ro/rw"), true),
                (h("rw"), true),
            ]
        );
    }

    #[test]
    fn cli_grant_refuses_what_a_request_would() {
        let f = fixture();
        let ssh = f.dir("home/.ssh");
        let err = f.store().grant("demo", &ssh, false, "cli").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(f.store().grants_for("demo").is_empty());
    }

    #[test]
    fn covered_paths_answer_granted_without_a_row() {
        let f = fixture();
        let games = f.dir("data/games");
        let s = f.store();
        s.grant("demo", &games, false, "cli").unwrap();
        let sub = f.dir("data/games/snes");
        let m = s
            .request("demo", &[(sub.to_str().unwrap().to_string(), false)], None)
            .unwrap();
        assert_eq!(m.value[0].outcome, "granted");
        assert!(!m.changed, "an already-reachable path stores no row");
        // The pure rule knows the manifest-declared half of the same check, and its mode.
        let declared = vec![(f.policy.home.join("Games"), false)];
        let x = f.policy.home.join("Games/x");
        assert!(covers(&declared, &[], &x, false, &f.policy));
        assert!(!covers(&declared, &[], &x, true, &f.policy), "a read root");
        // A write request over a read grant is a new request, not "granted".
        let w = s
            .request("demo", &[(sub.to_str().unwrap().to_string(), true)], None)
            .unwrap();
        assert_eq!(w.value[0].outcome, "pending");
        // A v1 `~/x` grant covers through the policy home even though it is stored raw.
        let mut access = BTreeMap::new();
        access.insert(
            "demo".to_string(),
            PluginAccess {
                grants: vec![Grant {
                    path: "~/gcov".into(),
                    write: false,
                    at: String::new(),
                    by: "legacy".into(),
                    forms: Vec::new(),
                }],
                denied: Vec::new(),
            },
        );
        s.write_access(&access).unwrap();
        let inside = f.dir("home/gcov/sub");
        let m = s
            .request(
                "demo",
                &[(inside.to_str().unwrap().to_string(), false)],
                None,
            )
            .unwrap();
        assert_eq!(m.value[0].outcome, "granted");
    }

    #[test]
    fn a_request_pends_reposts_and_survives_reload() {
        let f = fixture();
        let dir = f.dir("data/roms");
        let raw = dir.to_str().unwrap().to_string();
        assert_eq!(f.request(&raw), "pending");
        let m = f
            .store()
            .request("demo", &[(raw.clone(), true)], Some("other reason".into()))
            .unwrap();
        assert_eq!(m.value[0].outcome, "pending");
        assert!(!m.changed, "a repost writes nothing");
        // Fresh store over the same dir: the row persists, original fields intact.
        let snap = f.store().snapshot_for("demo").unwrap();
        assert_eq!(snap.pending.len(), 1);
        assert!(
            !snap.pending[0].write,
            "a repost does not widen the request"
        );
        assert_eq!(snap.pending[0].reason.as_deref(), Some("needs it"));
    }

    #[test]
    fn denial_sticks_across_a_new_store() {
        let f = fixture();
        let dir = f.dir("data/roms");
        let raw = dir.to_str().unwrap().to_string();
        assert_eq!(f.request(&raw), "pending");
        let m = f
            .store()
            .decide("demo", &raw, Decision::Deny, "console")
            .unwrap();
        assert!(m.changed);
        // A new AccessStore over the same dir still says denied, with no pending row.
        let s2 = f.store();
        assert_eq!(
            s2.request("demo", &[(raw.clone(), false)], None)
                .unwrap()
                .value[0]
                .outcome,
            "denied"
        );
        let snap = s2.snapshot_for("demo").unwrap();
        assert!(snap.pending.is_empty());
        assert_eq!(snap.denied, vec![raw.clone()]);
    }

    #[test]
    fn legacy_grants_migrate_and_parse_as_read_only() {
        let f = fixture();
        std::fs::create_dir_all(&f.store_dir).unwrap();
        std::fs::write(
            f.store_dir.join(GRANTS_FILE),
            r#"{"demo":["/mnt/old","/mnt/older"]}"#,
        )
        .unwrap();
        let grants = f.store().grants_for("demo");
        assert!(f
            .store_dir
            .join(crate::plugins::RUNNER_DATA_DIR)
            .join(GRANTS_FILE)
            .is_file());
        assert_eq!(
            grants,
            vec![
                Grant {
                    path: "/mnt/old".into(),
                    write: false,
                    at: String::new(),
                    by: "legacy".into(),
                    forms: Vec::new(),
                },
                Grant {
                    path: "/mnt/older".into(),
                    write: false,
                    at: String::new(),
                    by: "legacy".into(),
                    forms: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn corrupt_files_read_empty() {
        let f = fixture();
        std::fs::create_dir_all(&f.store_dir).unwrap();
        std::fs::write(f.store_dir.join(GRANTS_FILE), "{not json").unwrap();
        std::fs::write(f.store_dir.join(PENDING_FILE), "[nope").unwrap();
        let s = f.store();
        assert!(s.grants_for("demo").is_empty());
        assert!(s.snapshot().unwrap().is_empty());
    }

    #[test]
    fn allow_turns_pending_into_a_grant() {
        let f = fixture();
        let dir = f.dir("data/roms");
        let raw = dir.to_str().unwrap().to_string();
        assert_eq!(f.request(&raw), "pending");
        let m = f
            .store()
            .decide("demo", &raw, Decision::Allow, "console")
            .unwrap();
        assert!(m.changed);
        assert_eq!(m.value.grants.len(), 1);
        assert_eq!(m.value.grants[0].by, "console");
        assert!(m.value.pending.is_empty());
        // What the runner binds: `granted_roots` maps these through `grants_for`.
        assert_eq!(f.store().grants_for("demo"), m.value.grants);
        assert!(
            crate::plugins::manifest::granted_roots_in("demo", f.store_dir.clone()).contains(&dir)
        );
        // Allow without a pending row is a 404-class error.
        let err = f
            .store()
            .decide("demo", &raw, Decision::Allow, "console")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_direct_grant_answers_the_pending_request() {
        let f = fixture();
        let dir = f.dir("home/Games/celeste");
        let raw = dir.to_str().unwrap().to_string();
        assert_eq!(f.request(&raw), "pending");
        let grants = f.store().grant("demo", &dir, true, "console").unwrap();
        assert!(grants[0].write);
        let snap = f.store().snapshot_for("demo").unwrap();
        assert!(snap.pending.is_empty());
        assert_eq!(snap.grants.len(), 1);
    }

    #[test]
    fn a_handed_grant_goes_with_its_last_form() {
        let f = fixture();
        let saves = f.dir("home/Saves");
        let ini = saves.join("game.ini");
        std::fs::write(&ini, "x").unwrap();
        let s = f.store();
        s.hand("demo", &saves, true, "game:a").unwrap();
        let grants = s.hand("demo", &saves, false, "game:b").unwrap();
        assert!(
            grants[0].write,
            "a read-only form never narrows a write grant"
        );
        assert_eq!(grants[0].by, "form");
        // Game a still holds a file in the folder: the folder's grant stays.
        let kept = [ini.to_string_lossy().into_owned()];
        assert!(!s.release("demo", "game:a", &kept).unwrap().changed);
        let m = s.release("demo", "game:a", &[]).unwrap();
        assert!(m.changed);
        assert_eq!(m.value[0].forms, vec!["game:b"]);
        let removes = acl_removes();
        assert!(s.release("demo", "game:b", &[]).unwrap().value.is_empty());
        assert_eq!(acl_removes(), removes + 1, "the last form takes the ACE");
    }

    #[test]
    fn the_operators_own_grant_outlives_every_form() {
        let f = fixture();
        let s = f.store();
        let games = f.dir("home/Games");
        s.grant("demo", &games, false, "console").unwrap();
        s.hand("demo", &games, true, "config").unwrap();
        let m = s.release("demo", "config", &[]).unwrap();
        assert!(m.changed);
        assert_eq!((m.value.len(), m.value[0].by.as_str()), (1, "console"));
        // Granted by hand after a form handed it: the operator's now.
        let other = f.dir("home/Other");
        s.hand("demo", &other, false, "config").unwrap();
        s.grant("demo", &other, false, "cli").unwrap();
        assert_eq!(s.release("demo", "config", &[]).unwrap().value.len(), 2);
    }

    #[test]
    fn forget_removes_grants_and_denials_even_unplugged() {
        let f = fixture();
        let gone = "/mnt/unplugged-pf-test";
        let mut access = BTreeMap::new();
        access.insert(
            "demo".to_string(),
            PluginAccess {
                grants: vec![Grant {
                    path: gone.into(),
                    write: false,
                    at: "x".into(),
                    by: "cli".into(),
                    forms: Vec::new(),
                }],
                denied: vec![gone.into()],
            },
        );
        f.store().write_access(&access).unwrap();
        let m = f
            .store()
            .decide("demo", gone, Decision::Forget, "console")
            .unwrap();
        assert!(m.changed);
        assert!(m.value.grants.is_empty() && m.value.denied.is_empty());
        let m = f
            .store()
            .decide("demo", gone, Decision::Forget, "console")
            .unwrap();
        assert!(!m.changed, "a second forget is a no-op");
    }

    #[test]
    fn a_shared_folder_keeps_its_ace_until_the_last_grant_goes() {
        let f = fixture();
        let games = f.dir("data/games");
        f.store().grant("demo", &games, true, "cli").unwrap();
        f.store().grant("other", &games, false, "cli").unwrap();
        let (calls, removes) = (acl_calls(), acl_removes());
        // One plugin still reads it: the ACE stays, down to read.
        f.store().revoke("demo", &games).unwrap();
        assert_eq!((acl_calls(), acl_removes()), (calls + 1, removes));
        f.store()
            .decide(
                "other",
                &games.to_string_lossy(),
                Decision::Forget,
                "console",
            )
            .unwrap();
        assert_eq!(
            acl_removes(),
            removes + 1,
            "the last grant takes the ACE with it"
        );
    }

    #[test]
    fn caps_refuse_new_rows_only() {
        let f = fixture();
        let s = f.store();
        let mk = |i: usize| {
            let d = f.dir(&format!("data/d{i}"));
            d.to_str().unwrap().to_string()
        };
        let paths: Vec<(String, bool)> = (0..MAX_PENDING).map(|i| (mk(i), false)).collect();
        let m = s.request("demo", &paths, None).unwrap();
        assert!(m.value.iter().all(|o| o.outcome == "pending"));
        // At the cap a new path is refused; a repost still answers pending.
        assert_eq!(f.request(&mk(MAX_PENDING)), "refused:pending_limit");
        assert_eq!(
            s.request("demo", &[paths[0].clone()], None).unwrap().value[0].outcome,
            "pending"
        );
        // Grant cap: fill via direct grants, then the next is refused — without an ACL call.
        for i in 0..MAX_GRANTS {
            s.grant("demo", &f.dir(&format!("grants/g{i}")), false, "cli")
                .unwrap();
        }
        let acl_before = acl_calls();
        let err = s
            .grant("demo", &f.dir("grants/one-more"), false, "cli")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            acl_calls(),
            acl_before,
            "an over-cap grant never touches the platform ACL"
        );
    }

    /// Rows written straight to the fixture files — no ACL, no policy — so a test can pose
    /// states the public API would refuse.
    fn write_state(f: &Fixture, access: PluginAccess, pending: Vec<PendingRequest>) -> AccessStore {
        let s = f.store();
        let mut map = BTreeMap::new();
        map.insert("demo".to_string(), access);
        s.write_access(&map).unwrap();
        let mut pm = BTreeMap::new();
        pm.insert("demo".to_string(), pending);
        s.write_pending(&pm).unwrap();
        s
    }

    fn filler_grant(i: usize) -> Grant {
        Grant {
            path: format!("/cap-filler/g{i}"),
            write: false,
            at: String::new(),
            by: "cli".into(),
            forms: Vec::new(),
        }
    }

    #[test]
    fn a_denial_outranks_a_full_house() {
        let f = fixture();
        let denied = f.dir("data/den");
        let raw = denied.to_str().unwrap().to_string();
        let entry = PluginAccess {
            grants: (0..MAX_GRANTS).map(filler_grant).collect(),
            denied: vec![raw.clone()],
        };
        let pending: Vec<PendingRequest> = (0..MAX_PENDING)
            .map(|i| PendingRequest {
                path: format!("/cap-filler/p{i}"),
                write: false,
                reason: None,
                at: "x".into(),
            })
            .collect();
        let s = write_state(&f, entry, pending);
        assert_eq!(
            s.request("demo", &[(raw, false)], None).unwrap().value[0].outcome,
            "denied",
            "a sticky denial is answered before either cap"
        );
    }

    #[test]
    fn an_over_cap_allow_leaves_the_pending_row_and_no_acl() {
        let f = fixture();
        let dir = f.dir("data/late");
        let raw = dir.to_str().unwrap().to_string();
        let entry = PluginAccess {
            grants: (0..MAX_GRANTS).map(filler_grant).collect(),
            denied: Vec::new(),
        };
        // The pending row is written directly: requesting it now would hit the grant cap.
        let s = write_state(
            &f,
            entry,
            vec![PendingRequest {
                path: raw.clone(),
                write: false,
                reason: None,
                at: "x".into(),
            }],
        );
        let acl_before = acl_calls();
        let err = s
            .decide("demo", &raw, Decision::Allow, "console")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            acl_calls(),
            acl_before,
            "an over-cap allow never reaches the platform ACL"
        );
        let snap = s.snapshot_for("demo").unwrap();
        assert_eq!(
            snap.pending.len(),
            1,
            "the request survives for a later answer"
        );
    }

    #[test]
    fn the_opened_dir_is_its_own_protected_root() {
        let f = fixture();
        // `AccessStore::open` — the production constructor — must judge paths against the
        // dir it was given, not the process config dir.
        let s = AccessStore::open(f.store_dir.clone());
        let out = s
            .request(
                "demo",
                &[(f.store_dir.to_string_lossy().into_owned(), false)],
                None,
            )
            .unwrap();
        assert_eq!(out.value[0].outcome, "refused:broad_root");
        let inside = f.store_dir.join("inner");
        std::fs::create_dir_all(&inside).unwrap();
        let out = s
            .request(
                "demo",
                &[(
                    inside
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    false,
                )],
                None,
            )
            .unwrap();
        assert_eq!(out.value[0].outcome, "refused:protected_path");
    }

    #[test]
    fn revoke_validates_and_a_no_op_writes_nothing() {
        let f = fixture();
        let s = f.store();
        let err = s.revoke("demo", Path::new("relative/dir")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // A grant present but the revoked path absent: the file stays byte-identical.
        let games = f.dir("data/games");
        s.grant("demo", &games, false, "cli").unwrap();
        let grants_file = f
            .store_dir
            .join(crate::plugins::RUNNER_DATA_DIR)
            .join(GRANTS_FILE);
        let bytes_before = std::fs::read(&grants_file).unwrap();
        let grants = s
            .revoke("demo", Path::new("/mnt/never-granted-pf-test"))
            .unwrap();
        assert_eq!(grants.len(), 1);
        assert_eq!(
            std::fs::read(grants_file).unwrap(),
            bytes_before,
            "a no-op revoke does not rewrite the file"
        );
    }

    /// Windows comparisons ignore case through the same helpers the rules use.
    #[cfg(windows)]
    #[test]
    fn windows_paths_compare_case_insensitively() {
        assert!(within(
            Path::new(r"C:\Users\Ann\.ssh"),
            Path::new(r"c:\users\ann")
        ));
        assert!(same_path(r"\\?\C:\Users\Ann\Games", r"c:\users\ann\games"));
        assert!(windows_protected(Path::new(r"c:\USERS")));
        assert!(windows_protected(Path::new(r"C:\users\ann")));
        assert!(windows_protected(Path::new(r"C:\Users\ann\.ssh\keys")));
        assert!(windows_protected(Path::new(
            r"C:\USERS\ann\.config\Punktfunk-x"
        )));
        assert!(windows_protected(Path::new(r"c:\windows\system32")));
        // Ordinary profile content is not protected — only the named roots are.
        assert!(!windows_protected(Path::new(r"C:\Users\ann\Games")));
        assert!(!windows_protected(Path::new(r"D:\data\games")));
    }

    #[test]
    fn write_requests_need_a_path_the_operator_owns() {
        let f = fixture();
        let inside = f.dir("home/Games");
        let inside_outcome = f.request_w(inside.to_str().unwrap(), true);
        #[cfg(not(windows))]
        assert_eq!(
            inside_outcome, "pending",
            "inside the operator's home may ask for write"
        );
        #[cfg(windows)]
        assert_eq!(inside_outcome, "refused:write_not_owned");
        let facts = PathFacts {
            is_dir: true,
            owner_uid: Some(9_999),
        };
        let outside = Path::new("/mnt/elsewhere");
        // A foreign uid outside home fails the pure rule; the host's own uid passes.
        let rule = refusal_rule(outside, outside, true, false, &f.policy, facts);
        #[cfg(unix)]
        assert_eq!(rule, Some("write_not_owned"));
        #[cfg(windows)]
        assert_eq!(rule, Some("write_not_owned"));
    }
}
