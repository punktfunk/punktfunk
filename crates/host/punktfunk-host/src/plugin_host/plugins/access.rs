//! The operator's folder-access records for plugins: grants and pending requests.
//!
//! `plugin-run/plugin-grants.json` holds what the operator allowed (`{id: {grants, denied}}`; the
//! v1 `{id: [path]}` shape still parses, each string a read-only grant).
//! `plugin-access-pending.json` holds requests a plugin posted that await a decision. Both are
//! written tmp+rename and re-read on every operation, so a CLI write is never stale here. A
//! grants file that can't be read is never replaced: every write and listing fails, naming it.
//! Each read-modify-write holds `plugin-access.lock`, so the service and the CLI lose no update.
//!
//! The runner binds these roots (read-only unless `write`); a plugin can ask, never grant —
//! every request is validated against the real filesystem before it is stored. On Linux the
//! runner's unit must see a root too: [`AccessStore::runner_roots`] feeds its drop-in.

use super::manifest::PluginManifest;
use super::walks::{self, AccessWalk};
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
    /// The folder and the files directly in it, nothing below. Its ACE lands at once however
    /// large the tree under it is, where a recursive one rewrites every file.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub files: bool,
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
    /// An emulator to install into `path` first: the operator's yes installs, then grants.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emulator: Option<String>,
    /// A libretro core to install into `path` (RetroArch's cores folder) first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<String>,
    /// The plugin asks to read and restore emulator saves; `path` is the folder that grant lands on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub saves: bool,
    /// The folder and the files directly in it, nothing below. See [`Grant::files`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub files: bool,
    /// How the plugin spelled `path` when that goes through a link. An allow records it in
    /// [`PluginAccess::links`]: on Windows the link itself needs the runner's ACE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spelled: Option<String>,
}

/// One plugin's entry in `plugin-grants.json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PluginAccess {
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub denied: Vec<String>,
    /// A path the plugin asked for through a link → the granted path it resolves to. The Linux
    /// sandbox binds only the latter, so the runner recreates each link inside; on Windows each
    /// link on the way gets its own ACE, since a DACL set by name lands on the target.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
}

/// What an API read returns for one plugin: its entry plus its pending rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct PluginAccessSnapshot {
    pub plugin: String,
    pub grants: Vec<Grant>,
    pub pending: Vec<PendingRequest>,
    pub denied: Vec<String>,
    /// Folders whose access is changing for this plugin right now.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub walks: Vec<AccessWalk>,
}

/// One folder a plugin asks for. `files` means nothing with `write`, which takes the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AskedPath {
    pub path: String,
    pub write: bool,
    pub files: bool,
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
    /// The folder and the files directly in it. See [`Grant::files`].
    #[cfg_attr(not(windows), allow(dead_code, reason = "a bind costs no walk"))]
    pub files: bool,
    /// A junction or symbolic link on the way to a root, as spelled: the link object alone,
    /// nothing it points at. Windows only; bwrap follows a link.
    #[cfg_attr(not(windows), allow(dead_code, reason = "bwrap follows a link"))]
    pub link: bool,
}

/// `changed` is what the caller emits `plugins.changed` on; `value` is the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutation<T> {
    pub value: T,
    pub changed: bool,
}

const GRANTS_FILE: &str = "plugin-grants.json";
const PENDING_FILE: &str = "plugin-access-pending.json";
/// Beside the pending file. Empty: only its lock matters ([`AccessStore::exclusive`]).
const LOCK_FILE: &str = "plugin-access.lock";
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
                        files: false,
                    })
                    .collect(),
                denied: Vec::new(),
                links: BTreeMap::new(),
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
    #[cfg_attr(windows, allow(dead_code, reason = "Windows has no owner uid"))]
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

/// A grant or manifest path: `~/x` expands against the policy home, a `%ProgramData%`-style
/// prefix against the machine's root it names, anything else is as stored.
fn home_path(p: &str, policy: &PathPolicy) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) if !policy.home.as_os_str().is_empty() => policy.home.join(rest),
        _ => super::manifest::expand_env_prefix(p),
    }
}

/// `raw` → `canon` when the plugin reached `canon` through a link: a plain absolute spelling
/// that differs from where it resolves. A drive is plain; a verbatim or UNC prefix is not.
fn link_of(raw: &str, canon: &str) -> Option<(String, String)> {
    let p = Path::new(raw);
    let plain = p.is_absolute()
        && p.components().all(|c| match c {
            Component::RootDir | Component::Normal(_) => true,
            Component::Prefix(prefix) => matches!(prefix.kind(), std::path::Prefix::Disk(_)),
            Component::CurDir | Component::ParentDir => false,
        });
    let spelled: PathBuf = p.components().collect();
    (plain && !path_eq(&spelled, Path::new(canon)))
        .then(|| (spelled.to_string_lossy().into_owned(), canon.to_string()))
}

/// The reparse points on the way to `spelled`, nearest the leaf first: the runner opens through
/// each, so each needs an ACE of its own. A drive root is never one. Spelled with one separator:
/// a manifest path keeps the `/` that followed its `%Variable%`.
#[cfg(windows)]
fn link_components(spelled: &Path) -> Vec<PathBuf> {
    spelled
        .ancestors()
        .filter(|a| a.parent().is_some())
        .filter(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
        .map(|a| a.components().collect())
        .collect()
}

/// A path with one `*` component: `base\*\tail` names each folder one level under `base` that
/// holds `tail`, which may be empty, and `base` itself, whose listing is what the pattern is
/// about. Always a files pattern, whatever flag came with it: a plugin on a kit that predates
/// `files` asks with a plain string. One whole `*`, at least one real component before it, no
/// other component carrying one.
struct Pattern {
    base: PathBuf,
    tail: PathBuf,
}

fn pattern_of(p: &Path) -> Option<Pattern> {
    let comps: Vec<Component> = p.components().collect();
    let star = comps
        .iter()
        .position(|c| matches!(c, Component::Normal(n) if *n == "*"))?;
    let (base, tail) = (&comps[..star], &comps[star + 1..]);
    let starry =
        |c: &Component| matches!(c, Component::Normal(n) if n.to_string_lossy().contains('*'));
    if base.iter().any(starry) || tail.iter().any(starry) {
        return None;
    }
    if !base.iter().any(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    if tail.iter().any(|c| !matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(Pattern {
        base: base.iter().collect(),
        tail: tail.iter().collect(),
    })
}

impl Pattern {
    /// The folders the pattern names today, sorted. A missing base names none.
    fn expand(&self) -> Vec<PathBuf> {
        let Ok(listed) = std::fs::read_dir(&self.base) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = listed
            .flatten()
            .map(|entry| {
                let dir = entry.path();
                if self.tail.as_os_str().is_empty() {
                    dir
                } else {
                    dir.join(&self.tail)
                }
            })
            .filter(|p| p.is_dir())
            .collect();
        out.sort();
        out
    }

    /// Is `canonical` one of the folders this pattern names: the canonical base, or one
    /// component under it and then the tail?
    fn names(&self, canonical: &Path) -> bool {
        let Ok(base) = self.base.canonicalize() else {
            return false;
        };
        if path_eq(canonical, &base) {
            return true;
        }
        let Some(rest) = rel_after(canonical, &base) else {
            return false;
        };
        let mut comps = Path::new(&rest).components();
        comps
            .next()
            .is_some_and(|c| matches!(c, Component::Normal(_)))
            && path_eq(&comps.collect::<PathBuf>(), &self.tail)
    }
}

/// `act` on `dir`, or on each folder its pattern names today, the base first.
fn each_named(dir: &Path, act: &mut dyn FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
    match pattern_of(dir) {
        Some(pattern) => {
            act(&pattern.base)?;
            pattern.expand().iter().try_for_each(|d| act(d))
        }
        None => act(dir),
    }
}

/// A root a request is checked against: `(path, writable, files)`.
type Declared = (PathBuf, bool, bool);

/// Does this root answer for `canonical`? A read-only root never answers a write. A files root
/// answers a files request, for itself alone or for each folder its pattern names; a tree root
/// answers both below it.
fn answers(
    root: &Path,
    writable: bool,
    own_files: bool,
    canonical: &Path,
    write: bool,
    files: bool,
) -> bool {
    (writable || !write)
        && if own_files {
            files
                && match pattern_of(root) {
                    Some(pattern) => pattern.names(canonical),
                    None => path_eq(canonical, root),
                }
        } else {
            within(canonical, root)
        }
}

/// What holds `canonical`: a tree root, declared or granted, outranks a files root that is it
/// or whose pattern names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cover {
    None,
    Files,
    Tree,
}

/// Declared roots are canonicalized when they resolve so both sides speak the same spelling.
fn covers(
    declared: &[Declared],
    grants: &[Grant],
    canonical: &Path,
    write: bool,
    files: bool,
    policy: &PathPolicy,
) -> Cover {
    let declared = declared.iter().map(|(r, writable, own_files)| {
        (
            r.canonicalize().unwrap_or_else(|_| r.clone()),
            *writable,
            *own_files,
        )
    });
    let granted = grants
        .iter()
        .map(|g| (home_path(&g.path, policy), g.write, g.files));
    let mut cover = Cover::None;
    for (root, writable, own_files) in declared.chain(granted) {
        if !answers(&root, writable, own_files, canonical, write, files) {
            continue;
        }
        if !own_files {
            return Cover::Tree;
        }
        cover = Cover::Files;
    }
    cover
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
    facts.owner_uid == Some(crate::geteuid())
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

fn write_json_atomic<T: Serialize>(dir: &Path, name: &str, value: &T) -> io::Result<()> {
    let body = serde_json::to_vec_pretty(value).map_err(to_io)?;
    replace(&dir.join(name), &body)
}

/// The one writer for both files: a per-process synced temp and a rename, so the CLI and the
/// service never publish each other's half-written temp. Owner-only on Unix. Windows keeps the
/// directory's inherited ACL: an owner-only DACL would strip the runner's read ACE.
fn replace(path: &Path, body: &[u8]) -> io::Result<()> {
    if cfg!(unix) {
        pf_paths::replace_secret_file(path, body)
    } else {
        pf_paths::replace_file(path, body)
    }
}

/// The ACL half of a grant, mapped into `io::Error` for the store's signatures: the runner's
/// ACE, and `plugin`'s capability ACE when named. Runs only once a grant is certain to be
/// recorded — a cap refusal never leaves an unrecorded ACE. Called with no lock held: a large
/// tree takes minutes, and every reader of this store would wait. A pattern lands on each
/// folder it names today; the converge places the ones that appear later.
fn apply_acl(plugin: Option<&str>, dir: &Path, write: bool, files: bool) -> io::Result<()> {
    #[cfg(test)]
    ACL_CALLS.with(|calls| calls.set(calls.get() + 1));
    each_named(dir, &mut |d| super::grant_acl(plugin, d, write, files))
}

/// Match the ACL on `path` to the grants left after `plugin`'s came off. Every Windows plugin
/// shares the runner's account, so its ACE goes, with `plugin`'s capability ACE, only when no
/// plugin holds the folder, and a write ACE falls back to read when only read grants remain.
/// `files` is the kind that came off. Best-effort: the record is already written.
fn settle_acl(access: &BTreeMap<String, PluginAccess>, plugin: &str, path: &str, files: bool) {
    let held: Vec<&Grant> = access
        .values()
        .flat_map(|a| a.grants.iter())
        .filter(|g| same_path(&g.path, path))
        .collect();
    let dir = Path::new(path);
    let result = if held.is_empty() {
        #[cfg(test)]
        ACL_REMOVES.with(|calls| calls.set(calls.get() + 1));
        each_named(dir, &mut |d| super::revoke_acl(plugin, d, files))
    } else {
        let write = held.iter().any(|g| g.write);
        apply_acl(None, dir, write, held.iter().all(|g| g.files))
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

    /// One read-modify-write, alone: this store's mutex, then an exclusive lock on a sidecar
    /// file, which every store on this directory takes — the service's, a CLI `plugins
    /// grant`'s. Held until the pair drops; the OS frees it when a holder dies.
    #[allow(
        clippy::incompatible_msrv,
        reason = "`File::lock` is 1.89; rust-toolchain.toml pins 1.96"
    )]
    fn exclusive(&self) -> io::Result<(std::sync::MutexGuard<'_, ()>, std::fs::File)> {
        let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.config_dir.join(LOCK_FILE);
        let open = || {
            std::fs::File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)
        };
        let file = match open() {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                pf_paths::create_private_dir(&self.config_dir)?;
                open()
            }
            opened => opened,
        };
        let file = file
            .and_then(|f| f.lock().map(|()| f))
            .map_err(|e| io::Error::other(format!("lock {}: {e}", path.display())))?;
        Ok((guard, file))
    }

    /// The grants file, fresh — a `plugins grant` from another process must be seen. No file
    /// is no grants. One that can't be read or parsed is an error naming it, so a caller that
    /// writes stops before it replaces the file.
    fn load_access(&self) -> io::Result<BTreeMap<String, PluginAccess>> {
        let path = self.runner_dir.join(GRANTS_FILE);
        let unreadable = |what: &str, e: &dyn std::fmt::Display| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{what} {}: {e}", path.display()),
            )
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(unreadable("read", &e)),
        };
        let map: BTreeMap<String, AccessEntry> =
            serde_json::from_str(&text).map_err(|e| unreadable("parse", &e))?;
        Ok(map
            .into_iter()
            .map(|(id, e)| (id, e.into_access()))
            .collect())
    }

    /// [`Self::load_access`] for a caller that binds or checks and never writes: an unreadable
    /// file grants nothing.
    fn granted(&self) -> BTreeMap<String, PluginAccess> {
        self.load_access().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "plugin grants unreadable, none apply");
            BTreeMap::new()
        })
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
    /// `refused:<rule>` here rather than offered to the operator. A granted path asked for
    /// through a link records that link for the runner.
    pub fn request_paths(
        &self,
        id: &str,
        paths: &[AskedPath],
        reason: Option<String>,
    ) -> io::Result<Mutation<Vec<RequestOutcome>>> {
        let _guard = self.exclusive()?;
        let mut access = self.load_access()?;
        let mut pending_all = self.load_pending();
        let declared: Vec<Declared> = crate::plugins::manifest::for_provider(id)
            .map(|m| {
                let root = |p: &String, w, f| (home_path(p, &self.policy), w, f);
                let reads = m.reads.iter().map(|p| root(p, false, false));
                let files = m.files.iter().map(|p| root(p, false, true));
                let writes = m.writes.iter().map(|p| root(p, true, false));
                reads.chain(files).chain(writes).collect()
            })
            .unwrap_or_default();
        let entry = access.get(id).cloned().unwrap_or_default();
        let mut pending = pending_all.remove(id).unwrap_or_default();
        let mut changed = false;
        let outcomes: Vec<RequestOutcome> = paths
            .iter()
            .map(|asked| {
                self.request_one(
                    id,
                    &asked.path,
                    asked.write,
                    asked.files && !asked.write,
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
        let links: Vec<(String, String)> = paths
            .iter()
            .zip(&outcomes)
            .filter(|(_, o)| o.outcome == "granted")
            .filter_map(|(asked, o)| link_of(&asked.path, &o.path))
            .filter(|(k, v)| entry.links.get(k) != Some(v))
            .collect();
        if !links.is_empty() {
            let e = access.entry(id.to_string()).or_default();
            for (raw, target) in links {
                if e.links.len() < MAX_GRANTS || e.links.contains_key(&raw) {
                    e.links.insert(raw, target);
                }
            }
            // Only links into a grant still held: a manifest root binds at its own spelling.
            e.links.retain(|_, target| {
                e.grants
                    .iter()
                    .any(|g| within(Path::new(target), &home_path(&g.path, &self.policy)))
            });
            // A new link is a new root for the runner, so the caller converges on it.
            if e.links != entry.links {
                self.write_access(&access)?;
                changed = true;
            }
        }
        Ok(Mutation {
            value: outcomes,
            changed,
        })
    }

    /// [`Self::request_paths`] for folders asked for whole.
    pub fn request(
        &self,
        id: &str,
        paths: &[(String, bool)],
        reason: Option<String>,
    ) -> io::Result<Mutation<Vec<RequestOutcome>>> {
        let asked: Vec<AskedPath> = paths
            .iter()
            .map(|(path, write)| AskedPath {
                path: path.clone(),
                write: *write,
                files: false,
            })
            .collect();
        self.request_paths(id, &asked, reason)
    }

    #[allow(clippy::too_many_arguments)]
    fn request_one(
        &self,
        id: &str,
        raw: &str,
        write: bool,
        files: bool,
        reason: &Option<String>,
        declared: &[Declared],
        entry: &PluginAccess,
        pending: &mut Vec<PendingRequest>,
        changed: &mut bool,
    ) -> RequestOutcome {
        let outcome = |path: String, outcome: &str| RequestOutcome {
            path,
            outcome: outcome.into(),
        };
        let raw_path = Path::new(raw);
        // Shape first: a malformed string is refused before it touches the filesystem. A pattern
        // is a files request however it was asked, judged by its base; the `*` and the tail stay
        // as written.
        let pattern = pattern_of(raw_path).filter(|_| !write);
        let files = files || pattern.is_some();
        let resolved = match &pattern {
            Some(pattern) => pattern.base.canonicalize(),
            None => raw_path.canonicalize(),
        };
        let canonical = match resolved {
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
        // The rules judge `canonical`; the row and the grant carry `stored`, a pattern's base
        // with its `*` and tail put back.
        let facts = path_facts(&canonical);
        let stored = match &pattern {
            Some(pattern) => canonical.join("*").join(&pattern.tail),
            None => canonical.clone(),
        };
        let canon = stored.to_string_lossy().into_owned();
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
        match covers(declared, &entry.grants, &stored, write, files, &self.policy) {
            Cover::None => {}
            Cover::Tree => return outcome(canon, "granted"),
            Cover::Files => {
                // A folder a pattern names, or one reached through a link, gets its ACE now, so
                // the scan this answer goes back to can read it.
                if pattern.is_none() {
                    if let Err(e) = apply_acl(Some(id), &stored, false, true) {
                        tracing::warn!(plugin = id, path = %canon, error = %e, "covered folder not opened");
                    }
                }
                return outcome(canon, "granted");
            }
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
            spelled: match pattern {
                Some(_) => None,
                None => link_of(raw, &canon).map(|(spelled, _)| spelled),
            },
            path: canon.clone(),
            write,
            reason: reason.clone(),
            at: now_rfc3339(),
            emulator: None,
            core: None,
            saves: false,
            files,
        });
        *changed = true;
        outcome(canon, "pending")
    }

    /// A plugin asks for an emulator. The row carries the folder the install lands in, so the
    /// operator's yes installs first and then grants it like any folder; the folder need not
    /// exist yet, which is why this skips the path rules a plain request goes through.
    pub fn request_emulator(
        &self,
        id: &str,
        emulator: &str,
        path: &Path,
        reason: Option<String>,
    ) -> io::Result<Mutation<RequestOutcome>> {
        self.request_managed(id, path, reason, Some(emulator), None, false)
    }

    /// A plugin asks for a libretro core: the row's folder is RetroArch's cores folder.
    pub fn request_core(
        &self,
        id: &str,
        core: &str,
        path: &Path,
        reason: Option<String>,
    ) -> io::Result<Mutation<RequestOutcome>> {
        self.request_managed(id, path, reason, None, Some(core), false)
    }

    /// A plugin asks to move emulator saves: one row on the folder the grant lands on.
    pub fn request_saves(
        &self,
        id: &str,
        path: &Path,
        reason: Option<String>,
    ) -> io::Result<Mutation<RequestOutcome>> {
        self.request_managed(id, path, reason, None, None, true)
    }

    fn request_managed(
        &self,
        id: &str,
        path: &Path,
        reason: Option<String>,
        emulator: Option<&str>,
        core: Option<&str>,
        saves: bool,
    ) -> io::Result<Mutation<RequestOutcome>> {
        let _guard = self.exclusive()?;
        let access = self.load_access()?;
        let mut pending_all = self.load_pending();
        let canon = path.to_string_lossy().into_owned();
        let entry = access.get(id).cloned().unwrap_or_default();
        let mut pending = pending_all.remove(id).unwrap_or_default();
        let outcome = |o: &str| RequestOutcome {
            path: canon.clone(),
            outcome: o.into(),
        };
        let answer = if entry.grants.iter().any(|g| same_path(&g.path, &canon)) {
            outcome("granted")
        } else if entry.denied.iter().any(|d| same_path(d, &canon)) {
            outcome("denied")
        } else if pending
            .iter()
            .any(|p| same_path(&p.path, &canon) && p.core.as_deref() == core)
        {
            outcome("pending")
        } else if entry.grants.len() >= MAX_GRANTS {
            outcome("refused:grant_limit")
        } else if pending.len() >= MAX_PENDING {
            outcome("refused:pending_limit")
        } else {
            pending.push(PendingRequest {
                path: canon.clone(),
                write: false,
                reason,
                at: now_rfc3339(),
                emulator: emulator.map(str::to_string),
                core: core.map(str::to_string),
                saves,
                files: false,
                spelled: None,
            });
            pending_all.insert(id.to_string(), pending);
            self.write_pending(&pending_all)?;
            return Ok(Mutation {
                value: outcome("pending"),
                changed: true,
            });
        };
        Ok(Mutation {
            value: answer,
            changed: false,
        })
    }

    /// Whether plugin `id` may write `path`: under a writable root its manifest declares or a
    /// write grant. A path that doesn't resolve is checked as given.
    pub fn may_write(&self, id: &str, path: &Path) -> bool {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let declared: Vec<Declared> = crate::plugins::manifest::for_provider(id)
            .map(|m| {
                m.writes
                    .iter()
                    .map(|p| (home_path(p, &self.policy), true, false))
                    .collect()
            })
            .unwrap_or_default();
        let grants = self
            .granted()
            .get(id)
            .map(|e| e.grants.clone())
            .unwrap_or_default();
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        covers(&declared, &grants, &canonical, true, false, &self.policy) != Cover::None
    }

    /// The emulator a pending row asks for, when it is one.
    pub fn pending_emulator(&self, id: &str, path: &str) -> Option<String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.load_pending()
            .get(id)?
            .iter()
            .find(|p| same_path(&p.path, path))
            .and_then(|p| p.emulator.clone())
    }

    /// The cores the pending rows for `path` ask for: several cores share one folder.
    pub fn pending_cores(&self, id: &str, path: &str) -> Vec<String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.load_pending()
            .get(id)
            .map(|rows| {
                rows.iter()
                    .filter(|p| same_path(&p.path, path))
                    .filter_map(|p| p.core.clone())
                    .collect()
            })
            .unwrap_or_default()
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
        let stored_path = resolve_stored(path).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path must be absolute with no '..' component",
            )
        })?;
        match decision {
            Decision::Allow => self.allow(id, &stored_path, by),
            Decision::Deny => self.deny(id, &stored_path),
            Decision::Forget => self.forget(id, &stored_path),
        }
    }

    fn allow(
        &self,
        id: &str,
        stored_path: &str,
        by: &str,
    ) -> io::Result<Mutation<PluginAccessSnapshot>> {
        let no_row = || io::Error::new(io::ErrorKind::NotFound, "no pending request for that path");
        // The cap is checked before the ACL and before the pending row moves: a refused allow
        // leaves no ACE on disk and the request stays actionable.
        let row = {
            let _guard = self.exclusive()?;
            let access = self.load_access()?;
            let row = self.load_pending().get(id).and_then(|rows| {
                rows.iter()
                    .find(|p| same_path(&p.path, stored_path))
                    .cloned()
            });
            let row = row.ok_or_else(no_row)?;
            let grants = access.get(id).map_or(&[][..], |e| &e.grants);
            if !grants.iter().any(|g| same_path(&g.path, &row.path)) && grants.len() >= MAX_GRANTS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the grant limit is reached",
                ));
            }
            row
        };
        apply_acl(Some(id), Path::new(&row.path), row.write, row.files)?;
        let guard = self.exclusive()?;
        let mut access = self.load_access()?;
        let mut pending_all = self.load_pending();
        let answered = pending_all.get_mut(id).is_some_and(|rows| {
            let before = rows.len();
            rows.retain(|p| !same_path(&p.path, stored_path));
            rows.len() != before
        });
        // A no that landed during the walk stands, and the ACE comes back off.
        if !answered {
            drop(guard);
            settle_acl(&access, id, &row.path, row.files);
            return Err(no_row());
        }
        let entry = access.entry(id.to_string()).or_default();
        if let Some(g) = entry
            .grants
            .iter_mut()
            .find(|g| same_path(&g.path, &row.path))
        {
            g.write = row.write;
            g.files = row.files;
            g.at = now_rfc3339();
            g.by = by.to_string();
        } else {
            entry.grants.push(Grant {
                path: row.path.clone(),
                write: row.write,
                at: now_rfc3339(),
                by: by.to_string(),
                forms: Vec::new(),
                files: row.files,
            });
        }
        if let Some(spelled) = row.spelled {
            if entry.links.len() < MAX_GRANTS || entry.links.contains_key(&spelled) {
                entry.links.insert(spelled, row.path);
            }
        }
        self.write_access(&access)?;
        self.write_pending(&pending_all)?;
        Ok(Mutation {
            value: snapshot_of(id, &access, &pending_all),
            changed: true,
        })
    }

    /// Every row on the folder goes, as an allow takes them all: RetroArch's cores share one.
    fn deny(&self, id: &str, stored_path: &str) -> io::Result<Mutation<PluginAccessSnapshot>> {
        let _guard = self.exclusive()?;
        let mut access = self.load_access()?;
        let mut pending_all = self.load_pending();
        let found = pending_all.get_mut(id).is_some_and(|rows| {
            let before = rows.len();
            rows.retain(|p| !same_path(&p.path, stored_path));
            rows.len() != before
        });
        if !found {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no pending request for that path",
            ));
        }
        let entry = access.entry(id.to_string()).or_default();
        if !entry.denied.iter().any(|d| same_path(d, stored_path)) {
            entry.denied.push(stored_path.to_string());
        }
        self.write_access(&access)?;
        self.write_pending(&pending_all)?;
        Ok(Mutation {
            value: snapshot_of(id, &access, &pending_all),
            changed: true,
        })
    }

    fn forget(&self, id: &str, stored_path: &str) -> io::Result<Mutation<PluginAccessSnapshot>> {
        let guard = self.exclusive()?;
        let mut access = self.load_access()?;
        let pending = self.load_pending();
        let (mut changed, mut ungranted, mut files) = (false, false, false);
        if let Some(entry) = access.get_mut(id) {
            let before = entry.grants.len() + entry.denied.len();
            let granted = entry.grants.len();
            files = entry
                .grants
                .iter()
                .any(|g| g.files && same_path(&g.path, stored_path));
            entry.grants.retain(|g| !same_path(&g.path, stored_path));
            ungranted = entry.grants.len() != granted;
            entry.denied.retain(|d| !same_path(d, stored_path));
            changed = entry.grants.len() + entry.denied.len() != before;
        }
        if changed {
            self.write_access(&access)?;
        }
        drop(guard);
        if ungranted {
            settle_acl(&access, id, stored_path, files);
        }
        Ok(Mutation {
            value: snapshot_of(id, &access, &pending),
            changed,
        })
    }

    /// Every plugin with a recorded entry or pending row.
    pub fn snapshot(&self) -> io::Result<Vec<PluginAccessSnapshot>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let access = self.load_access()?;
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
        Ok(snapshot_of(id, &self.load_access()?, &self.load_pending()))
    }

    /// The grants the runner binds for `id` — empty when the file is absent or unreadable.
    pub fn grants_for(&self, id: &str) -> Vec<Grant> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.granted()
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
            self.granted()
        };
        let declared = manifests.values().flat_map(declared_entries);
        let granted = access
            .values()
            .flat_map(|a| a.grants.iter().map(grant_entry));
        let links = access.values().flat_map(|a| a.links.keys());
        self.resolve_roots(declared.chain(granted), links.map(String::as_str))
    }

    /// [`Self::runner_roots`] for one plugin: what its manifest declares plus its own grants,
    /// which is what its own account or capability gets an ACE for.
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows grants per plugin"))]
    pub fn plugin_roots(&self, id: &str, manifest: &PluginManifest) -> Vec<RunnerRoot> {
        let access = {
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.granted()
        };
        let granted = access
            .get(id)
            .into_iter()
            .flat_map(|a| a.grants.iter().map(grant_entry));
        let links = access.get(id).into_iter().flat_map(|a| a.links.keys());
        self.resolve_roots(
            declared_entries(manifest).chain(granted),
            links.map(String::as_str),
        )
    }

    /// May the runner see `real`, asked for as `spelled`? A missing root is the drop-in's `-`
    /// prefix's business, and a root may be a file.
    fn may_bind(&self, spelled: &Path, real: &Path, write: bool, operator: bool) -> bool {
        let facts = PathFacts {
            is_dir: true,
            ..path_facts(real)
        };
        refusal_rule(spelled, real, write, operator, &self.policy, facts).is_none()
    }

    /// The links on the way to `spelled` when it resolves elsewhere, each a root of its own,
    /// judged as a folder would be.
    #[cfg(windows)]
    fn link_roots(&self, spelled: &Path, real: &Path, operator: bool) -> Vec<RunnerRoot> {
        if path_eq(real, spelled) {
            return Vec::new();
        }
        link_components(spelled)
            .into_iter()
            .filter(|link| self.may_bind(link, link, false, operator))
            .map(|path| RunnerRoot {
                path,
                write: false,
                files: false,
                link: true,
            })
            .collect()
    }

    /// One spelled root into `roots`: its real path, the links on the way to it on Windows, and
    /// on Linux the real directory holding a link, which bwrap follows in the runner.
    fn bind(
        &self,
        roots: &mut Vec<RunnerRoot>,
        spelled: PathBuf,
        write: bool,
        files: bool,
        operator: bool,
    ) {
        let real = spelled.canonicalize().unwrap_or_else(|_| spelled.clone());
        if !self.may_bind(&spelled, &real, write, operator) {
            return;
        }
        #[cfg(windows)]
        roots.extend(self.link_roots(&spelled, &real, operator));
        if real != spelled {
            let holder = spelled
                .ancestors()
                .skip(1)
                .find(|a| a.canonicalize().is_ok_and(|c| c == *a));
            if let Some(dir) = holder.filter(|d| self.may_bind(d, d, false, operator)) {
                roots.push(RunnerRoot {
                    path: dir.to_path_buf(),
                    write: false,
                    files: false,
                    link: false,
                });
            }
        }
        roots.push(RunnerRoot {
            path: real,
            write,
            files,
            link: false,
        });
    }

    /// `(spelled, write, files, operator)` entries into the real roots a runner may see, plus
    /// the links a plugin asked through, which Windows binds as roots of their own.
    fn resolve_roots<'a>(
        &self,
        entries: impl Iterator<Item = (&'a str, bool, bool, bool)>,
        links: impl Iterator<Item = &'a str>,
    ) -> Vec<RunnerRoot> {
        let mut roots = Vec::new();
        for (p, write, files, operator) in entries {
            let spelled = home_path(p, &self.policy);
            // A pattern binds its base and each folder it names today, all by their own files;
            // the next converge sees new ones.
            match pattern_of(&spelled).filter(|_| !write) {
                Some(pattern) => {
                    self.bind(&mut roots, pattern.base.clone(), false, true, operator);
                    for dir in pattern.expand() {
                        self.bind(&mut roots, dir, false, true, operator);
                    }
                }
                None => self.bind(&mut roots, spelled, write, files, operator),
            }
        }
        #[cfg(windows)]
        for spelled in links.map(PathBuf::from) {
            let real = spelled.canonicalize().unwrap_or_else(|_| spelled.clone());
            roots.extend(self.link_roots(&spelled, &real, true));
        }
        #[cfg(not(windows))]
        let _ = links;
        // Strongest first, so the dedup keeps it: a write, then a tree, then a folder's own
        // files, then a link.
        let strength = |r: &RunnerRoot| match (r.write, r.files, r.link) {
            (true, _, _) => 3,
            (false, _, true) => 0,
            (false, true, false) => 1,
            (false, false, false) => 2,
        };
        roots.sort_by(|a, b| a.path.cmp(&b.path).then(strength(b).cmp(&strength(a))));
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
        let guard = self.exclusive()?;
        let kept: Vec<String> = keep.iter().flat_map(|p| grant_keys(Path::new(p))).collect();
        let mut access = self.load_access()?;
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
                gone.push((g.path.clone(), g.files));
            }
            !last
        });
        let grants = entry.grants.clone();
        if changed {
            self.write_access(&access)?;
            drop(guard);
            for (path, files) in &gone {
                settle_acl(&access, id, path, *files);
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
        let path = canonical.to_string_lossy().into_owned();
        let write = {
            let _guard = self.exclusive()?;
            let access = self.load_access()?;
            let grants = access.get(id).map_or(&[][..], |e| &e.grants);
            let existing = grants.iter().find(|g| same_path(&g.path, &path));
            // Cap before ACL: a refused grant must not leave an ACE on the directory.
            if existing.is_none() && grants.len() >= MAX_GRANTS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "the grant limit is reached",
                ));
            }
            match (existing, form) {
                (Some(g), Some(_)) => write || g.write,
                _ => write,
            }
        };
        apply_acl(Some(id), &canonical, write, false)?;
        let _guard = self.exclusive()?;
        let mut access = self.load_access()?;
        let entry = access.entry(id.to_string()).or_default();
        let existing = entry.grants.iter().position(|g| same_path(&g.path, &path));
        match (existing, form) {
            (Some(i), Some(form)) => {
                let g = &mut entry.grants[i];
                g.write = write;
                g.files = false;
                if !g.forms.iter().any(|f| f == form) {
                    g.forms.push(form.to_string());
                }
            }
            (Some(i), None) => {
                let g = &mut entry.grants[i];
                g.write = write;
                g.files = false;
                g.at = now_rfc3339();
                g.by = by.to_string();
            }
            (None, _) => entry.grants.push(Grant {
                path: path.clone(),
                write,
                at: now_rfc3339(),
                by: by.to_string(),
                forms: form.map(|f| vec![f.to_string()]).unwrap_or_default(),
                files: false,
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
        let guard = self.exclusive()?;
        let stored = resolve_stored(&path.to_string_lossy()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "the path must be absolute with no '..' component",
            )
        })?;
        let mut access = self.load_access()?;
        let (mut changed, mut files) = (false, false);
        let grants = match access.get_mut(id) {
            Some(entry) => {
                let before = entry.grants.len();
                files = entry
                    .grants
                    .iter()
                    .any(|g| g.files && same_path(&g.path, &stored));
                entry.grants.retain(|g| !same_path(&g.path, &stored));
                changed = entry.grants.len() != before;
                entry.grants.clone()
            }
            None => Vec::new(),
        };
        if changed {
            self.write_access(&access)?;
            drop(guard);
            settle_acl(&access, id, &stored, files);
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
    replace(&target, &std::fs::read(legacy)?)
}

/// Harden the runner directory, then hand the Windows runner its read ACE back.
fn prepare_runner_dir(dir: &Path) -> io::Result<()> {
    pf_paths::create_private_dir(dir)?;
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

/// A manifest's roots as [`AccessStore::resolve_roots`] takes them.
fn declared_entries(m: &PluginManifest) -> impl Iterator<Item = (&str, bool, bool, bool)> {
    let reads = m.reads.iter().map(|p| (p.as_str(), false, false, false));
    let files = m.files.iter().map(|p| (p.as_str(), false, true, false));
    let writes = m.writes.iter().map(|p| (p.as_str(), true, false, false));
    reads.chain(files).chain(writes)
}

fn grant_entry(g: &Grant) -> (&str, bool, bool, bool) {
    (g.path.as_str(), g.write, g.files, true)
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
        walks: walks::of(id),
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

    /// The root rules the runner's sandbox twins (`refusedRoot` in sdk/src/sandbox.ts), on
    /// canonical absolute paths: the shape rules (`..`, length, not a directory) stay host-only.
    #[cfg(unix)]
    fn path_refusal_vectors() -> String {
        const HOME: &str = "/home/u";
        const USER_CONFIG: &str = "/home/u/.config/punktfunk";
        const SYSTEM_CONFIG: &str = "/var/lib/punktfunk";
        let rows: &[(&str, &[&str])] = &[
            (
                USER_CONFIG,
                &[
                    "/",
                    "/home",
                    "/home/u",
                    "/proc",
                    "/proc/1/root",
                    "/sys/kernel",
                    "/dev/shm",
                    "/run",
                    "/run/media",
                    "/run/user/1000",
                    "/run/mediax/games",
                    "/home/u/.ssh",
                    "/home/u/.gnupg/private-keys-v1.d",
                    "/home/u/.config",
                    "/home/u/.config/punktfunk",
                    "/home/u/.config/punktfunk/plugin-run",
                    "/home/u/.config/punktfunk-extra",
                    "/home/u/.local/share/Steam",
                    "/home/u/.config/retroarch",
                    "/home/u/Emu",
                    "/run/media/u/SD",
                    "/run/media/deck/SD/Emulation/roms",
                    "/mnt/games1",
                    "/tmp/vhclient_response",
                    "/usr/share/applications",
                ],
            ),
            (
                SYSTEM_CONFIG,
                &[
                    "/var",
                    "/var/lib",
                    "/var/lib/punktfunk",
                    "/var/lib/punktfunk/plugin-run",
                    "/var/lib/punktfunk-other",
                    "/var/lib/flatpak",
                    "/home/u",
                    "/home/u/.ssh",
                    "/home/u/.config/punktfunk",
                    "/home/u/.config/punktfunk-extra",
                    "/home/u/.config/retroarch",
                    "/home/u/Games",
                ],
            ),
        ];
        let facts = PathFacts {
            is_dir: true,
            owner_uid: None,
        };
        let mut lines = Vec::new();
        for (config_dir, paths) in rows {
            let policy = PathPolicy {
                home: HOME.into(),
                config_dir: (*config_dir).into(),
                runtime_dir: Some("/run/user/1000".into()),
            };
            for path in *paths {
                let p = Path::new(path);
                let rule = refusal_rule(p, p, false, false, &policy, facts);
                assert!(
                    matches!(rule, None | Some("broad_root" | "protected_path")),
                    "{path}: {rule:?}"
                );
                lines.push(format!(
                    "    {{\"path\": \"{path}\", \"home\": \"{HOME}\", \"config_dir\": \"{config_dir}\", \
                     \"refused\": {}}}",
                    rule.is_some()
                ));
            }
        }
        format!(
            "{{\n  \"$comment\": \"Generated from punktfunk-host plugin_host access::refusal_rule by \
             path_refusal_vectors_are_checked_in (UPDATE_VECTORS=1 rewrites it). The SDK's sandbox \
             replays it.\",\n  \"vectors\": [\n{}\n  ]\n}}\n",
            lines.join(",\n")
        )
    }

    /// The file at `rel`, under this crate, holds `fresh`. `UPDATE_VECTORS=1` rewrites it first.
    #[cfg(unix)]
    fn assert_checked_in(rel: &str, fresh: &str) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        if std::env::var_os("UPDATE_VECTORS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, fresh).unwrap();
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == fresh,
            "{} is stale: rerun with UPDATE_VECTORS=1",
            path.display()
        );
    }

    /// The `/proc`, `/sys`, `/dev` and `/run` rules are Unix-only, so the file is too.
    #[cfg(unix)]
    #[test]
    fn path_refusal_vectors_are_checked_in() {
        assert_checked_in(
            "testdata/path-refusal-vectors.json",
            &path_refusal_vectors(),
        );
    }

    /// The grants file as the store's own writer leaves it: one plugin with every kind of row,
    /// one with no grant.
    #[cfg(unix)]
    fn grants_file_sample() -> String {
        let f = fixture();
        let s = f.store();
        let grant = |path: &str, write: bool, by: &str, forms: &[&str]| Grant {
            path: path.into(),
            write,
            at: if by == "legacy" {
                String::new()
            } else {
                "2026-01-01T00:00:00Z".into()
            },
            by: by.into(),
            forms: forms.iter().map(|form| (*form).to_string()).collect(),
            files: false,
        };
        let demo = PluginAccess {
            grants: vec![
                grant("/mnt/roms", false, "console", &[]),
                grant("/mnt/saves", true, BY_FORM, &["config", "game:steam:440"]),
                grant("/mnt/old", false, "legacy", &[]),
            ],
            denied: vec!["/mnt/private".into()],
            links: BTreeMap::from([("/home/u/Games".into(), "/mnt/roms/games".into())]),
        };
        let quiet = PluginAccess {
            denied: vec!["/mnt/private".into()],
            ..Default::default()
        };
        let map = BTreeMap::from([("demo".to_string(), demo), ("quiet".to_string(), quiet)]);
        s.write_access(&map).unwrap();
        std::fs::read_to_string(s.runner_dir.join(GRANTS_FILE)).unwrap()
    }

    /// The runner's `grantedRoots` parses this file strictly and grants nothing on a row it
    /// can't read. The SDK's tests replay the checked-in copy.
    #[cfg(unix)]
    #[test]
    fn the_grants_file_the_runner_reads_is_checked_in() {
        let dir = crate::plugins::RUNNER_DATA_DIR;
        assert_checked_in(
            &format!("../../../sdk/test/host-config/{dir}/{GRANTS_FILE}"),
            &grants_file_sample(),
        );
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
        let declared = vec![(f.policy.home.join("Games"), false, false)];
        let x = f.policy.home.join("Games/x");
        assert_eq!(
            covers(&declared, &[], &x, false, false, &f.policy),
            Cover::Tree
        );
        assert_eq!(
            covers(&declared, &[], &x, true, false, &f.policy),
            Cover::None,
            "a read root"
        );
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
                    files: false,
                }],
                denied: Vec::new(),
                links: BTreeMap::new(),
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

    #[cfg(unix)]
    #[test]
    fn a_granted_path_asked_for_through_a_link_records_the_link() {
        let f = fixture();
        let games = f.dir("data/ssd/games");
        let other = f.dir("data/other");
        let s = f.store();
        s.grant("demo", &games, false, "cli").unwrap();
        let home_games = f.dir("home/Games");
        std::os::unix::fs::symlink(&games, home_games.join("SteamLibrary")).unwrap();
        std::os::unix::fs::symlink(&other, home_games.join("Other")).unwrap();
        let raw = |p: &Path| p.to_str().unwrap().to_string();
        let asked = [
            (raw(&home_games.join("SteamLibrary")), false),
            (raw(&games), false),
            (raw(&home_games.join("Other")), false),
        ];
        let m = s.request("demo", &asked, None).unwrap();
        let outcomes: Vec<&str> = m.value.iter().map(|o| o.outcome.as_str()).collect();
        assert_eq!(outcomes, ["granted", "granted", "pending"]);
        // Only the link into a grant; the canonical spelling needs none.
        assert_eq!(
            s.load_access().unwrap()["demo"].links,
            BTreeMap::from([(asked[0].0.clone(), raw(&games))])
        );
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
    fn one_no_on_the_cores_folder_takes_every_core_asked_there() {
        let f = fixture();
        let cores = f.dir("data/retroarch/cores");
        let s = f.store();
        for core in ["snes9x", "mgba"] {
            assert_eq!(
                s.request_core("demo", core, &cores, None)
                    .unwrap()
                    .value
                    .outcome,
                "pending"
            );
        }
        s.decide("demo", cores.to_str().unwrap(), Decision::Deny, "console")
            .unwrap();
        assert!(s.snapshot_for("demo").unwrap().pending.is_empty());
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
                    files: false,
                },
                Grant {
                    path: "/mnt/older".into(),
                    write: false,
                    at: String::new(),
                    by: "legacy".into(),
                    forms: Vec::new(),
                    files: false,
                },
            ]
        );
    }

    /// The pending queue is rebuilt by the plugins' next scan, so a damaged one reads empty.
    #[test]
    fn a_corrupt_pending_file_reads_empty() {
        let f = fixture();
        std::fs::write(f.store_dir.join(PENDING_FILE), "[nope").unwrap();
        assert!(f.store().snapshot().unwrap().is_empty());
    }

    /// A grants file that does not parse stays byte for byte: every call that would write it,
    /// and every listing, stops with an error naming it. What binds or checks sees no grant.
    #[test]
    fn an_unreadable_grants_file_is_kept_and_named() {
        let f = fixture();
        let (roms, saves) = (f.dir("data/roms"), f.dir("data/saves"));
        let (roms_s, saves_s) = (roms.to_str().unwrap(), saves.to_str().unwrap());
        let s = f.store();
        s.grant("demo", &roms, true, "cli").unwrap();
        assert_eq!(f.request(saves_s), "pending");
        assert!(s.may_write("demo", &roms));
        let file = s.runner_dir.join(GRANTS_FILE);
        let damaged = std::fs::read(&file).unwrap()[..20].to_vec();
        std::fs::write(&file, &damaged).unwrap();
        let acl = acl_calls();

        let ask = [(roms_s.to_string(), false)];
        let refused = [
            ("request", s.request("demo", &ask, None).map(drop)),
            (
                "request_core",
                s.request_core("demo", "snes9x", &roms, None).map(drop),
            ),
            (
                "allow",
                s.decide("demo", saves_s, Decision::Allow, "console")
                    .map(drop),
            ),
            (
                "deny",
                s.decide("demo", saves_s, Decision::Deny, "console")
                    .map(drop),
            ),
            (
                "forget",
                s.decide("demo", roms_s, Decision::Forget, "console")
                    .map(drop),
            ),
            ("grant", s.grant("demo", &saves, false, "cli").map(drop)),
            ("hand", s.hand("demo", &saves, true, "config").map(drop)),
            ("release", s.release("demo", "config", &[]).map(drop)),
            ("revoke", s.revoke("demo", &roms).map(drop)),
            ("snapshot", s.snapshot().map(drop)),
            ("snapshot_for", s.snapshot_for("demo").map(drop)),
        ];
        for (call, result) in refused {
            let err = result.expect_err(call);
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{call}");
            assert!(
                err.to_string().contains(file.to_str().unwrap()),
                "{call}: {err}"
            );
        }
        assert_eq!(std::fs::read(&file).unwrap(), damaged, "kept as found");
        assert_eq!(acl_calls(), acl, "no ACE for a grant that was not recorded");
        assert!(s.grants_for("demo").is_empty());
        assert!(!s.may_write("demo", &roms));
        assert!(s.runner_roots(&BTreeMap::new()).is_empty());
    }

    /// Each store has a mutex of its own, as each process does; the file lock is what they
    /// share. Eight granting at once into a directory none has made yet lose no grant.
    #[test]
    fn stores_on_one_directory_lose_no_grant() {
        let f = fixture();
        let root = f.store_dir.join("fresh");
        let dirs: Vec<PathBuf> = (0..8).map(|i| f.dir(&format!("grants/g{i}"))).collect();
        std::thread::scope(|scope| {
            for dir in &dirs {
                let store = AccessStore::open_with(root.clone(), f.policy.clone());
                scope.spawn(move || store.grant("demo", dir, false, "cli").map(drop).unwrap());
            }
        });
        let store = AccessStore::open_with(root.clone(), f.policy.clone());
        assert_eq!(store.grants_for("demo").len(), dirs.len());
        assert!(root.join(LOCK_FILE).is_file());
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
                    files: false,
                }],
                denied: vec![gone.into()],
                links: BTreeMap::new(),
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
            files: false,
        }
    }

    fn ask_files(s: &AccessStore, path: &Path) -> String {
        let asked = AskedPath {
            path: path.to_str().unwrap().to_string(),
            write: false,
            files: true,
        };
        s.request_paths("demo", &[asked], None).unwrap().value[0]
            .outcome
            .clone()
    }

    #[test]
    fn a_files_grant_answers_for_its_folder_alone() {
        let f = fixture();
        let lib = f.dir("data/lib");
        let below = f.dir("data/lib/common");
        let s = f.store();
        assert_eq!(ask_files(&s, &lib), "pending");
        let path = lib.to_str().unwrap();
        s.decide("demo", path, Decision::Allow, "console").unwrap();
        assert!(s.grants_for("demo")[0].files);
        assert_eq!(ask_files(&s, &lib), "granted");
        // What it does not reach is asked for, and the operator can widen it.
        assert_eq!(ask_files(&s, &below), "pending");
        assert_eq!(f.request(path), "pending");
        s.decide("demo", path, Decision::Allow, "console").unwrap();
        assert!(!s.grants_for("demo")[0].files);
        assert_eq!(ask_files(&s, &lib), "granted");
    }

    /// A `files` pattern: one prompt for a library folder answers each game folder under it,
    /// new ones included, and the runner binds what the pattern names today.
    #[test]
    fn a_files_pattern_answers_each_folder_it_names() {
        let f = fixture();
        let lib = f.dir("data/lib");
        let a = f.dir("data/lib/GameA/__Installer");
        f.dir("data/lib/GameB");
        std::fs::write(lib.join("notes.txt"), "x").unwrap();
        let s = f.store();
        let pattern = lib.join("*").join("__Installer");
        assert_eq!(ask_files(&s, &pattern), "pending");
        let row = s.snapshot_for("demo").unwrap().pending.remove(0);
        assert!(row.files && row.spelled.is_none());
        assert_eq!(row.path, pattern.to_string_lossy());
        s.decide("demo", &row.path, Decision::Allow, "console")
            .unwrap();
        let grant = &s.grants_for("demo")[0];
        assert!(grant.files && grant.path.contains('*'));
        assert_eq!(ask_files(&s, &a), "granted");
        assert_eq!(ask_files(&s, &lib), "granted", "the base lists its names");
        assert_eq!(
            ask_files(&s, &lib.join("GameB")),
            "pending",
            "the tail is part of the name"
        );
        assert_eq!(ask_files(&s, &pattern), "granted", "the same pattern again");
        let b = f.dir("data/lib/GameB/__Installer");
        assert_eq!(ask_files(&s, &b), "granted", "a folder that appeared later");
        let roots = s.runner_roots(&BTreeMap::new());
        let bound: Vec<&Path> = roots
            .iter()
            .filter(|r| r.files)
            .map(|r| r.path.as_path())
            .collect();
        assert_eq!(bound, vec![lib.as_path(), a.as_path(), b.as_path()]);
        assert!(!s.may_write("demo", &a));
    }

    /// A pattern is judged by the folder its base names, and needs exactly one whole `*`.
    #[test]
    fn a_pattern_needs_a_real_base_and_one_star() {
        let f = fixture();
        let s = f.store();
        let x = f.dir("data/x");
        assert_eq!(
            ask_files(&s, &f.policy.home.join("*")),
            "refused:broad_root"
        );
        assert_eq!(
            ask_files(&s, &x.join("*").join("*")),
            "refused:not_directory"
        );
        assert_eq!(
            ask_files(&s, &x.join("goggame-*.info")),
            "refused:not_directory"
        );
        // A kit that predates `files` asks with a plain string: a `*` is a files pattern anyway.
        let whole = s
            .request(
                "demo",
                &[(x.join("*").to_string_lossy().into_owned(), false)],
                None,
            )
            .unwrap();
        assert_eq!(whole.value[0].outcome, "pending");
        assert!(s.snapshot_for("demo").unwrap().pending[0].files);
    }

    #[test]
    fn a_whole_folder_answers_a_files_request_below_it() {
        let f = fixture();
        let lib = f.dir("data/lib");
        let below = f.dir("data/lib/steamapps");
        let s = f.store();
        s.grant("demo", &lib, false, "cli").unwrap();
        assert_eq!(ask_files(&s, &below), "granted");
    }

    #[test]
    fn a_write_asks_for_the_tree_not_the_files() {
        let f = fixture();
        let lib = f.dir("data/lib");
        let asked = AskedPath {
            path: lib.to_str().unwrap().to_string(),
            write: true,
            files: true,
        };
        let s = f.store();
        s.request_paths("demo", &[asked], None).unwrap();
        let row = s.snapshot_for("demo").unwrap().pending.remove(0);
        assert!(row.write && !row.files);
    }

    #[test]
    fn a_root_keeps_the_strongest_kind_any_plugin_asked() {
        let f = fixture();
        let lib = f.dir("data/lib");
        let spelled = lib.to_str().unwrap().to_string();
        let declared = |files: &[&str], reads: &[&str], writes: &[&str]| {
            let list = |v: &[&str]| v.iter().map(|p| (*p).to_string()).collect();
            [(
                "demo".to_string(),
                PluginManifest {
                    files: list(files),
                    reads: list(reads),
                    writes: list(writes),
                    ..Default::default()
                },
            )]
            .into()
        };
        let s = f.store();
        let kinds = |m: &BTreeMap<String, PluginManifest>| {
            let root = s.runner_roots(m).remove(0);
            (root.write, root.files)
        };
        let one = [spelled.as_str()];
        assert_eq!(kinds(&declared(&one, &[], &[])), (false, true));
        assert_eq!(kinds(&declared(&one, &one, &[])), (false, false));
        assert_eq!(kinds(&declared(&one, &[], &one)), (true, false));
    }

    #[test]
    fn a_denial_outranks_a_full_house() {
        let f = fixture();
        let denied = f.dir("data/den");
        let raw = denied.to_str().unwrap().to_string();
        let entry = PluginAccess {
            grants: (0..MAX_GRANTS).map(filler_grant).collect(),
            denied: vec![raw.clone()],
            links: BTreeMap::new(),
        };
        let pending: Vec<PendingRequest> = (0..MAX_PENDING)
            .map(|i| PendingRequest {
                path: format!("/cap-filler/p{i}"),
                write: false,
                reason: None,
                at: "x".into(),
                emulator: None,
                core: None,
                saves: false,
                files: false,
                spelled: None,
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
            links: BTreeMap::new(),
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
                emulator: None,
                core: None,
                saves: false,
                files: false,
                spelled: None,
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

    /// A junction is a link the runner opens through, so the ACE must land on it as well as on
    /// its target: a request keeps its spelling, an allow records the link, and the link is a
    /// root of its own. `mklink /J` needs no privilege.
    #[cfg(windows)]
    #[test]
    fn a_path_through_a_junction_keeps_its_spelling_and_binds_the_link() {
        assert_eq!(
            link_of(r"C:\Games\Lib", r"\\?\D:\Lib"),
            Some((r"C:\Games\Lib".to_string(), r"\\?\D:\Lib".to_string()))
        );
        assert_eq!(link_of(r"C:\Games\Lib", r"\\?\c:\games\lib"), None);
        assert_eq!(link_of(r"\\?\C:\Games\Lib", r"\\?\D:\Lib"), None);
        assert_eq!(link_of(r"\\srv\share\Lib", r"\\?\UNC\srv\share\Lib"), None);

        let f = fixture();
        let plain = |p: &Path| PathBuf::from(p.to_string_lossy().trim_start_matches(r"\\?\"));
        let target = plain(&f.dir("data/lib"));
        let link = plain(&f.dir("home/Games")).join("Lib");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let spelled = link.to_string_lossy().into_owned();
        let canon = link.canonicalize().unwrap().to_string_lossy().into_owned();
        assert_eq!(link_of(&spelled, &canon), Some((spelled.clone(), canon)));
        assert_eq!(link_components(&link.join("steamapps")), vec![link.clone()]);
        assert!(link_components(&target).is_empty());

        let s = f.store();
        assert_eq!(f.request(&spelled), "pending");
        let row = s.snapshot_for("demo").unwrap().pending.remove(0);
        assert_eq!(row.spelled.as_deref(), Some(spelled.as_str()));
        s.decide("demo", &row.path, Decision::Allow, "console")
            .unwrap();
        let entry = s.load_access().unwrap().remove("demo").unwrap();
        assert_eq!(entry.links.get(&spelled), Some(&row.path));
        let roots = s.runner_roots(&BTreeMap::new());
        assert!(roots.iter().any(|r| r.link && r.path == link), "{roots:?}");
        assert!(roots.iter().any(|r| !r.link && !r.files && !r.write));
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
