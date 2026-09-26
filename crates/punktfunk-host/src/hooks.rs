//! Fire-and-forget operator commands and webhooks for host lifecycle events.
//!
//! `hooks.json` is operator-privileged (`/api/v1/hooks`). Commands get event JSON and
//! `PF_EVENT_*` env; Windows SYSTEM hosts run them as their WTS session user.
//! Webhooks use verified TLS, do not follow redirects or attach host credentials,
//! and may carry an HMAC signature.
//!
//! Debounce, timeout, process-group kill, and [`MAX_CONCURRENT_HOOKS`] bound work.
//! Absolute script paths must pass ownership/writability checks. Logs use sanitized
//! labels — command lines and webhook URLs may contain credentials.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use utoipa::ToSchema;

/// In-flight exec + webhook cap. Excess firings drop — hooks are observers, not a queue.
const MAX_CONCURRENT_HOOKS: usize = 8;

const DEFAULT_TIMEOUT_S: u32 = 30;
const MAX_TIMEOUT_S: u32 = 600;

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

fn default_timeout_s() -> u32 {
    DEFAULT_TIMEOUT_S
}

/// Operator hook config: `hooks.json` and the `/api/v1/hooks` body.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub hooks: Vec<HookEntry>,
}

/// One hook: `run` and/or `webhook` when `on` (+ `filter`) matches.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct HookEntry {
    /// Exact kind (`stream.started`) or `domain.*` prefix; same vocabulary as SSE `?kinds=`.
    pub on: String,
    /// Exact-match constraints; every present field must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<HookFilter>,
    /// Detached shell command: event JSON on stdin, `PF_EVENT_*` env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
    /// Exec timeout in seconds (1–600, default 30); the process group is killed on expiry.
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u32,
    /// Minimum interval between firings, in milliseconds. 0 = fire every time. A `hold` ignores it.
    #[serde(default)]
    pub debounce_ms: u64,
    /// The launch waits for this hook, up to `timeout_s`. Only with `on: game.launching`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hold: bool,
    /// HMAC secret file (`X-Punktfunk-Signature: sha256=<hex>`). Warns if world-readable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    pub hmac_secret_file: Option<PathBuf>,
}

/// Exact-match filters on event identity fields. Absent fields do not constrain;
/// a field set on a kind that does not carry it (e.g. `client` on `host.started`) never matches.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, Default)]
pub struct HookFilter {
    /// Client/device name (`session.*`: the Dashboard's short client label).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Certificate fingerprint (hex, case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plane: Option<crate::events::Plane>,
    /// Launched app id/title (`stream.*` events).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// The dialled settings preset, by id or name (`client.*`, `session.*`, `stream.*`, `game.*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

impl HookFilter {
    fn matches(&self, kind: &crate::events::EventKind) -> bool {
        if let Some(want) = &self.client {
            if kind.client_name() != Some(want.as_str()) {
                return false;
            }
        }
        if let Some(want) = &self.fingerprint {
            match kind.fingerprint() {
                Some(fp) if fp.eq_ignore_ascii_case(want) => {}
                _ => return false,
            }
        }
        if let Some(want) = self.plane {
            if kind.plane() != Some(want) {
                return false;
            }
        }
        if let Some(want) = &self.app {
            if kind.app() != Some(want.as_str()) {
                return false;
            }
        }
        if let Some(want) = &self.preset {
            match kind.preset() {
                Some(p) if p.id == *want || p.name.eq_ignore_ascii_case(want) => {}
                _ => return false,
            }
        }
        true
    }
}

impl HooksConfig {
    /// Structural errors fail the PUT; unknown kinds are accepted (additive catalog).
    pub fn validate(&self) -> Result<(), String> {
        for (i, h) in self.hooks.iter().enumerate() {
            let at = |msg: &str| format!("hooks[{i}]: {msg}");
            if h.on.trim().is_empty() {
                return Err(at("`on` must be an event kind or `domain.*` pattern"));
            }
            if h.run.as_deref().is_none_or(|r| r.trim().is_empty())
                && h.webhook.as_deref().is_none_or(|w| w.trim().is_empty())
            {
                return Err(at("needs `run` and/or `webhook`"));
            }
            if let Some(url) = h.webhook.as_deref().filter(|w| !w.trim().is_empty()) {
                if !url.starts_with("https://") && !url.starts_with("http://") {
                    return Err(at("`webhook` must be an http(s):// URL"));
                }
                if webhook_host_is_internal(url) {
                    return Err(at(
                        "`webhook` must not target a loopback/link-local/metadata host",
                    ));
                }
                // Warn, don't reject: an internal-only http:// receiver may be intentional.
                if h.hmac_secret_file.is_some() && url.starts_with("http://") {
                    tracing::warn!(
                        url = %webhook_origin(url),
                        "webhook has an hmac_secret_file but is http:// — the signed body is sent in cleartext; prefer https://"
                    );
                }
            }
            if let Some(p) = h.hmac_secret_file.as_deref() {
                if let Some(why) = secret_file_complaint(p) {
                    tracing::warn!(path = %p.display(),
                        "webhook hmac_secret_file is {why} — it should be operator-owned and chmod 600");
                }
            }
            if h.timeout_s == 0 || h.timeout_s > MAX_TIMEOUT_S {
                return Err(at(&format!("`timeout_s` must be 1–{MAX_TIMEOUT_S}")));
            }
            if h.hold && !crate::holds::STAGES.contains(&h.on.as_str()) {
                return Err(at(&format!(
                    "`hold` needs `on` set to one of: {}",
                    crate::holds::STAGES.join(", ")
                )));
            }
        }
        Ok(())
    }
}

/// Hygiene warning for [`HookEntry::hmac_secret_file`]. `None` if the file is fine or
/// absent — unreadability is [`post_webhook`]'s fail-closed case. Warn, do not refuse:
/// refusing here would silently drop a signing the operator asked for.
#[cfg(unix)]
fn secret_file_complaint(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid && meta.uid() != 0 {
        return Some(format!(
            "owned by uid {} (host runs as uid {euid})",
            meta.uid()
        ));
    }
    (meta.mode() & 0o077 != 0)
        .then(|| format!("group/world-accessible (mode {:o})", meta.mode() & 0o7777))
}

/// Windows: the SYSTEM/Admins-DACL'd config dir is the boundary (same as [`exec_path_check`]).
#[cfg(not(unix))]
fn secret_file_complaint(_path: &std::path::Path) -> Option<String> {
    None
}

// ------------------------------------------------------------------------- store

/// Persisted `hooks.json` (same recipe as [`crate::vdisplay::policy::DisplayPolicyStore`]):
/// private dir, temp-write + atomic rename; memory updates only after the write succeeds.
/// [`get`] re-stats mtime+length so a hand edit applies without a restart.
pub struct HooksStore {
    path: PathBuf,
    cur: Mutex<StoreState>,
}

struct StoreState {
    cfg: Option<HooksConfig>,
    /// mtime + length of the revision `cfg` was parsed from. `None` = the file did not exist.
    file_id: Option<(std::time::SystemTime, u64)>,
}

impl HooksStore {
    /// Missing or corrupt file ⇒ no hooks (warn on corrupt); never fail host startup.
    pub fn load_from(path: PathBuf) -> Self {
        let (cfg, file_id) = Self::read_disk(&path);
        HooksStore {
            path,
            cur: Mutex::new(StoreState { cfg, file_id }),
        }
    }

    /// On-disk identity. `None` if missing or unstatable — both mean no usable file.
    fn file_identity(path: &PathBuf) -> Option<(std::time::SystemTime, u64)> {
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    /// Same lenient contract as [`Self::load_from`]: missing, invalid, or planted by a
    /// non-admin before the first elevated run ⇒ no hooks. Hooks run commands.
    fn read_disk(path: &PathBuf) -> (Option<HooksConfig>, Option<(std::time::SystemTime, u64)>) {
        let file_id = Self::file_identity(path);
        if crate::planted::quarantine_planted_secret(path) {
            return (None, file_id);
        }
        let cfg = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<HooksConfig>(&bytes) {
                Ok(c) => {
                    if let Err(e) = c.validate() {
                        tracing::warn!(path = %path.display(),
                            "hooks.json invalid — hooks disabled until fixed: {e}");
                        None
                    } else {
                        Some(c)
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(),
                        "hooks.json unreadable — hooks disabled until fixed: {e}");
                    None
                }
            },
            Err(_) => None,
        };
        (cfg, file_id)
    }

    /// Empty when unconfigured. Reloads `hooks.json` if identity moved — hand edits apply
    /// on the next event (`docs/automation.md`).
    pub fn get(&self) -> HooksConfig {
        let mut st = self.cur.lock().unwrap();
        let now_id = Self::file_identity(&self.path);
        if now_id != st.file_id {
            let (cfg, file_id) = Self::read_disk(&self.path);
            tracing::info!(path = %self.path.display(), hooks = cfg.as_ref().map_or(0, |c| c.hooks.len()),
                "hooks.json changed on disk — reloaded");
            st.cfg = cfg;
            st.file_id = file_id;
        }
        st.cfg.clone().unwrap_or_default()
    }

    /// Persist then adopt (caller validates first). Memory updates only if the write succeeds.
    pub fn set(&self, cfg: HooksConfig) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            pf_paths::create_private_dir(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        pf_paths::write_secret_file(&tmp, &serde_json::to_vec_pretty(&cfg)?)?;
        std::fs::rename(&tmp, &self.path)?;
        let mut st = self.cur.lock().unwrap();
        st.file_id = Self::file_identity(&self.path);
        st.cfg = Some(cfg);
        Ok(())
    }
}

/// Process-wide store (`<config_dir>/hooks.json`); reloads on disk change ([`HooksStore::get`]).
pub fn store() -> &'static HooksStore {
    static STORE: OnceLock<HooksStore> = OnceLock::new();
    STORE.get_or_init(|| HooksStore::load_from(pf_paths::config_dir().join("hooks.json")))
}

// ------------------------------------------------------------------------- runner

/// Host-lifetime task: live event tail → matching hooks. Spawned by `serve()` before
/// `host.started`. Lag skips missed events — fire-and-forget, never an unbounded queue.
pub async fn runner() {
    let mut rx = crate::events::bus().subscribe_live();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HOOKS));
    let mut debounce: HashMap<u64, Instant> = HashMap::new();
    loop {
        match rx.recv().await {
            Ok(ev) => dispatch(&ev, &sem, &mut debounce),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "hook runner lagged — skipped events fire no hooks"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Debounce key: hash of the serialized entry, so an unchanged hook keeps its window across a PUT.
fn entry_key(h: &HookEntry) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(h)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// The kind matched but the name did not, so this hook stayed silent — say so once per
/// wanted/seen pair. A display name is neither unique nor fixed (renaming a device changes
/// it), which is why the console writes `fingerprint`; a hand-written name filter has only
/// this line to go on. Capped: one event carries one pair, and a host sees few devices.
fn note_client_mismatch(h: &HookEntry, ev: &crate::events::EventKind, kind: &str) {
    let Some(want) = h.filter.as_ref().and_then(|f| f.client.as_deref()) else {
        return;
    };
    let Some(got) = ev.client_name().filter(|got| *got != want) else {
        return;
    };
    static SEEN: OnceLock<Mutex<std::collections::HashSet<(String, String)>>> = OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock().unwrap();
    if seen.len() < 64 && seen.insert((want.to_string(), got.to_string())) {
        tracing::info!(
            on = %h.on, kind, want, got,
            "hook filtered on a client name the event does not carry — filter on the device's \
             fingerprint instead"
        );
    }
}

fn dispatch(
    ev: &crate::events::HostEvent,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
    debounce: &mut HashMap<u64, Instant>,
) {
    let kind = ev.kind.name();
    let cfg = store().get();
    for h in &cfg.hooks {
        // The launch stage runs a hold itself ([`hold`]); firing it here too would run it twice.
        if h.hold || !crate::events::kind_matches(&h.on, kind) {
            continue;
        }
        if !h
            .filter
            .as_ref()
            .unwrap_or(&HookFilter::default())
            .matches(&ev.kind)
        {
            note_client_mismatch(h, &ev.kind, kind);
            continue;
        }
        if h.debounce_ms > 0 {
            let key = entry_key(h);
            let now = Instant::now();
            if debounce
                .get(&key)
                .is_some_and(|t| now.duration_since(*t) < Duration::from_millis(h.debounce_ms))
            {
                tracing::debug!(on = %h.on, kind, "hook debounced");
                continue;
            }
            debounce.insert(key, now);
        }
        if let Some(cmd) = h.run.as_deref().filter(|c| !c.trim().is_empty()) {
            fire_exec(cmd.to_string(), ev, h.timeout_s, sem);
        }
        if let Some(url) = h.webhook.as_deref().filter(|u| !u.trim().is_empty()) {
            fire_webhook(url.to_string(), h.hmac_secret_file.clone(), ev, sem);
        }
    }
    // Env-var mirrors of `PUNKTFUNK_ON_CONNECT_CMD` / `PUNKTFUNK_ON_DISCONNECT_CMD`.
    let mirror = match kind {
        "client.connected" => pf_host_config::config().on_connect_cmd.clone(),
        "client.disconnected" => pf_host_config::config().on_disconnect_cmd.clone(),
        _ => None,
    };
    if let Some(cmd) = mirror {
        fire_exec(cmd, ev, DEFAULT_TIMEOUT_S, sem);
    }
}

/// The hooks that hold `ev`'s stage: `hold` set, kind and filter matching.
pub(crate) fn holding(ev: &crate::events::HostEvent) -> Vec<HookEntry> {
    let kind = ev.kind.name();
    store()
        .get()
        .hooks
        .into_iter()
        .filter(|h| h.hold && crate::events::kind_matches(&h.on, kind))
        .filter(|h| h.filter.as_ref().is_none_or(|f| f.matches(&ev.kind)))
        .collect()
}

/// Run one holding hook to completion, blocking: `run`, then `webhook`, each bounded by
/// `timeout_s`. Failures log; the caller proceeds either way.
pub(crate) fn hold(h: &HookEntry, ev: &crate::events::HostEvent) {
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let timeout = Duration::from_secs(u64::from(h.timeout_s));
    if let Some(cmd) = h.run.as_deref().filter(|c| !c.trim().is_empty()) {
        let label = cmd_label(cmd);
        match exec_path_check(cmd) {
            Err(e) => tracing::error!(cmd = %label, "REFUSING hook command — {e}"),
            Ok(()) => {
                tracing::info!(cmd = %label, kind = ev.kind.name(), "hook: holding the launch");
                run_hook_process(cmd, &json, &flatten_env(ev), timeout);
            }
        }
    }
    if let Some(url) = h.webhook.as_deref().filter(|u| !u.trim().is_empty()) {
        post_webhook(url, &json, h.hmac_secret_file.as_deref(), timeout);
    }
}

// ------------------------------------------------------------------------- exec action

/// Process-lifetime `#xxxxxxxx` of the secret-bearing part of a log line. Same id on every
/// line of one firing; different ids for two hooks that share a program or host.
fn short_id(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    format!("#{:08x}", hasher.finish() as u32)
}

/// Log `cmd`: program file name + [`short_id`]. Arguments are dropped — they carry tokens
/// and `GET /api/v1/logs` serves the tracing ring verbatim. Refusals still name the path
/// via [`exec_path_check`].
fn cmd_label(cmd: &str) -> String {
    let prog = cmd
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['"', '\'']);
    let file = prog.rsplit(['/', '\\']).next().unwrap_or(prog);
    format!("{file} {}", short_id(cmd))
}

fn fire_exec(
    cmd: String,
    ev: &crate::events::HostEvent,
    timeout_s: u32,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let label = cmd_label(&cmd);
    let Ok(permit) = sem.clone().try_acquire_owned() else {
        tracing::warn!(cmd = %label, "hook dropped — too many hook executions in flight");
        return;
    };
    if let Err(e) = exec_path_check(&cmd) {
        tracing::error!(cmd = %label, "REFUSING hook command — {e}");
        return;
    }
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let env = flatten_env(ev);
    let kind = ev.kind.name();
    let timeout = Duration::from_secs(u64::from(timeout_s));
    tracing::info!(cmd = %label, kind, "hook: running command");
    // Off-thread: streaming planes never wait on operator code. Permit frees on thread exit.
    std::thread::spawn(move || {
        run_hook_process(&cmd, &json, &env, timeout);
        drop(permit);
    });
}

/// Event as `PF_EVENT_*` env: scalar JSON leaves, path joined with `_` and uppercased
/// (`client.name` → `PF_EVENT_CLIENT_NAME`), plus `PF_EVENT_JSON`. Control chars stripped
/// so a device name cannot smuggle newlines into a shell consumer.
fn flatten_env(ev: &crate::events::HostEvent) -> Vec<(String, String)> {
    fn walk(prefix: &str, v: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map {
                    let key = k
                        .chars()
                        .map(|c| {
                            if c.is_ascii_alphanumeric() {
                                c.to_ascii_uppercase()
                            } else {
                                '_'
                            }
                        })
                        .collect::<String>();
                    walk(&format!("{prefix}_{key}"), val, out);
                }
            }
            serde_json::Value::Null => {}
            serde_json::Value::String(s) => {
                let clean: String = s.chars().filter(|c| !c.is_control()).collect();
                out.push((prefix.to_string(), clean));
            }
            other => out.push((prefix.to_string(), other.to_string())),
        }
    }
    let mut out = Vec::new();
    if let Ok(v) = serde_json::to_value(ev) {
        walk("PF_EVENT", &v, &mut out);
    }
    if let Ok(json) = serde_json::to_string(ev) {
        out.push(("PF_EVENT_JSON".to_string(), json));
    }
    out
}

/// Refuse a referenced script/binary that is group/world-writable or owned by neither the
/// host user nor root; walk every parent (a writable parent can swap the file). Bare names
/// are left to PATH. Every absolute-path token is checked, not just argv0 — otherwise
/// `bash /tmp/x` vets the interpreter and skips the script.
#[cfg(unix)]
fn exec_path_check(cmd: &str) -> Result<(), String> {
    let tokens = shell_tokens(cmd);
    if tokens.is_empty() {
        return Err("empty command".into());
    }
    // SAFETY: geteuid has no preconditions and touches no memory.
    let euid = unsafe { libc::geteuid() };
    for token in &tokens {
        if !token.starts_with('/') {
            continue;
        }
        let path = std::path::Path::new(token);
        if !std::fs::metadata(path).is_ok_and(|m| m.is_file()) {
            continue; // not an existing file — the shell reports it
        }
        for node in path.ancestors() {
            let Ok(meta) = std::fs::metadata(node) else {
                continue;
            };
            path_node_check(node, &meta, euid)?;
        }
    }
    Ok(())
}

/// Ownership/mode rule for the script and each parent. A sticky world-writable directory
/// (`/tmp`) passes: only the entry's owner can replace it, so the swap is already blocked.
#[cfg(unix)]
fn path_node_check(
    path: &std::path::Path,
    meta: &std::fs::Metadata,
    euid: u32,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != euid && meta.uid() != 0 {
        return Err(format!(
            "{} is owned by uid {} (host runs as uid {euid}) — a hook script and the directories \
             holding it must be owned by the operator or root",
            path.display(),
            meta.uid()
        ));
    }
    let sticky_dir = meta.is_dir() && meta.mode() & 0o1000 != 0;
    if meta.mode() & 0o022 != 0 && !sticky_dir {
        return Err(format!(
            "{} is group/world-writable (mode {:o}) — chmod go-w it first",
            path.display(),
            meta.mode() & 0o7777
        ));
    }
    Ok(())
}

/// Tokenize like `/bin/sh` for path finding: whitespace splits; quoted or backslash-escaped
/// runs stay one token. `split_whitespace` would miss `"/opt/my hooks/run.sh"`.
#[cfg(unix)]
fn shell_tokens(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = cmd.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            // Inside '' a backslash is literal; inside "" and unquoted it escapes the next char.
            Some('"') if c == '\\' => cur.extend(chars.next()),
            Some(_) => cur.push(c),
            None if c == '\\' => cur.extend(chars.next()),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// True if this process is `NT AUTHORITY\SYSTEM` (S-1-5-18) — the SCM service, or the `serve`
/// child it launches into the interactive session.
///
/// Fail closed: an unreadable token reads as SYSTEM. Both callers want the cautious answer —
/// a skipped hook beats a SYSTEM command, and no tray beats a SYSTEM-owned one.
#[cfg(windows)]
pub(crate) fn running_as_system() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser, WinLocalSystemSid, PSID,
        SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle from GetCurrentProcess; `token` is a live out-param.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return true; // fail closed
    }
    // TOKEN_USER is align-8; `[u8; 256]` is align-1 — a `&TOKEN_USER` into it is UB if
    // the slot is misaligned. Keep 256 BYTES: `[u64; 32]` would pass `len()`=32 to
    // GetTokenInformation and misclassify a 44-byte console TOKEN_USER as SYSTEM.
    #[repr(align(8))]
    struct TokenUserBuf([u8; 256]);
    let mut buf = TokenUserBuf([0u8; 256]);
    let mut len = 0u32;
    // SAFETY: `buf` is a writable local of the length passed; `len` is a live out-param.
    let got = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            Some(buf.0.as_mut_ptr().cast()),
            std::mem::size_of_val(&buf) as u32,
            &mut len,
        )
    };
    // SAFETY: the token handle came from OpenProcessToken and is not used after this.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(token);
    }
    if got.is_err() {
        return true; // fail closed
    }
    let mut system = [0u8; SECURITY_MAX_SID_SIZE as usize];
    let mut cb = system.len() as u32;
    // SAFETY: the buffer is SECURITY_MAX_SID_SIZE, the documented maximum SID size.
    if unsafe {
        CreateWellKnownSid(
            WinLocalSystemSid,
            None,
            Some(PSID(system.as_mut_ptr().cast())),
            &mut cb,
        )
    }
    .is_err()
    {
        return true; // fail closed
    }
    // SAFETY: `buf` holds a TOKEN_USER written by GetTokenInformation (align guaranteed by
    // TokenUserBuf); its `User.Sid` points into the same buffer, and both SIDs are valid for
    // this comparison.
    unsafe {
        let tu = &*(buf.0.as_ptr() as *const TOKEN_USER);
        // EqualSid maps BOOL(0) and a genuine failure both to Err; tell them apart via
        // GetLastError. Clear it first — a stale value would read as failure. Treating
        // EqualSid Err as "not SYSTEM" is fail-open.
        windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
        match EqualSid(tu.User.Sid, PSID(system.as_mut_ptr().cast())) {
            Ok(()) => true,                      // equal: we are SYSTEM
            Err(e) if e.code().is_ok() => false, // BOOL(0), last-error 0: genuinely not equal
            Err(_) => true,                      // EqualSid itself failed: fail closed
        }
    }
}

#[cfg(not(unix))]
fn exec_path_check(_cmd: &str) -> Result<(), String> {
    // Windows commands use the host session's user, while hooks.json has a SYSTEM/Admins DACL.
    // The Windows path therefore has no per-script ownership walk.
    Ok(())
}

/// Run one hook command to completion or timeout, blocking the reaper thread.
/// Returns exit-0 success; prep gates each step's `undo` on that.
#[cfg(unix)]
fn run_hook_process(
    cmd: &str,
    event_json: &str,
    env: &[(String, String)],
    timeout: Duration,
) -> bool {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let label = cmd_label(cmd);
    let mut c = std::process::Command::new("/bin/sh");
    c.arg("-c")
        .arg(cmd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        // Own process group so timeout can kill the whole tree the shell spawned.
        .process_group(0);
    c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => {
            tracing::error!(cmd = %label, error = %e, "hook command did not launch");
            return false;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(event_json.as_bytes());
        // stdin drops here; a hook that never reads it is unaffected.
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::warn!(cmd = %label, %status, "hook command exited non-zero");
                }
                return status.success();
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    tracing::warn!(cmd = %label, timeout_s = timeout.as_secs(),
                        "hook command timed out — killing its process group");
                    #[cfg(target_os = "linux")]
                    {
                        // SAFETY: kill(2) with a negative pid signals the process group we
                        // created via process_group(0); no memory is touched.
                        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    }
                    #[cfg(not(target_os = "linux"))]
                    let _ = child.kill();
                    let _ = child.wait(); // reap — never leave a zombie
                    return false;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                tracing::warn!(cmd = %label, error = %e, "hook command wait failed");
                return false;
            }
        }
    }
}

/// Runs a Windows hook as the signed-in user of the host's WTS session.
///
/// [`crate::interactive::spawn_as_current_session_user`] has no env or stdin,
/// so the event JSON path is the last argument. A non-SYSTEM host falls back to
/// its own token with the event environment and JSON on stdin. The call blocks
/// for the configured timeout while the detached user process runs.
#[cfg(windows)]
fn run_hook_process(
    cmd: &str,
    event_json: &str,
    env: &[(String, String)],
    timeout: Duration,
) -> bool {
    use std::io::Write;
    let label = cmd_label(cmd);
    let stamp = format!(
        "pf-hook-{}-{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let json_path = std::env::temp_dir().join(stamp);
    if std::fs::write(&json_path, event_json).is_err() {
        tracing::warn!(cmd = %label, "hook: event JSON temp file not written");
    }
    let cmdline = format!("{cmd} \"{}\"", json_path.display());
    match crate::interactive::spawn_as_current_session_user(&cmdline, None) {
        Ok(pid) => {
            tracing::debug!(cmd = %label, pid, "hook command launched as the current WTS session user");
            // The launch returns no child handle, so cleanup waits for the configured ceiling.
            std::thread::sleep(timeout);
            let _ = std::fs::remove_file(&json_path);
            // A detached user process has no observable status; successful launch arms prep undo.
            true
        }
        Err(e) if running_as_system() => {
            // A SYSTEM fallback would change the hook's principal, so a missing session user skips it.
            tracing::warn!(
                cmd = %label,
                error = %format!("{e:#}"),
                "hook SKIPPED: the host's WTS session has no user token, and this host is SYSTEM — \
                 hooks run as the session user and never fall back to SYSTEM"
            );
            let _ = std::fs::remove_file(&json_path);
            false
        }
        Err(e) => {
            // The caller's own token is the non-SYSTEM fallback principal.
            tracing::debug!(error = %format!("{e:#}"),
                "session-user spawn unavailable — running hook with the caller's token");
            let mut ok = false;
            let mut c = std::process::Command::new("cmd.exe");
            c.arg("/C")
                .arg(cmd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            c.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            match c.spawn() {
                Ok(mut child) => {
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(event_json.as_bytes());
                    }
                    let deadline = Instant::now() + timeout;
                    loop {
                        match child.try_wait().ok().flatten() {
                            Some(status) => {
                                ok = status.success();
                                break;
                            }
                            None if Instant::now() >= deadline => {
                                tracing::warn!(cmd = %label, "hook command timed out — killing it");
                                let _ = child.kill();
                                let _ = child.wait();
                                break;
                            }
                            None => std::thread::sleep(Duration::from_millis(100)),
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(cmd = %label, error = %e, "hook command did not launch")
                }
            }
            let _ = std::fs::remove_file(&json_path);
            ok
        }
    }
}

// ------------------------------------------------------------------------- webhook action

fn fire_webhook(
    url: String,
    secret_file: Option<PathBuf>,
    ev: &crate::events::HostEvent,
    sem: &std::sync::Arc<tokio::sync::Semaphore>,
) {
    let origin = webhook_origin(&url);
    let Ok(permit) = sem.clone().try_acquire_owned() else {
        tracing::warn!(url = %origin, "webhook dropped — too many hook executions in flight");
        return;
    };
    let json = serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string());
    let kind = ev.kind.name();
    tracing::info!(url = %origin, kind, "hook: posting webhook");
    std::thread::spawn(move || {
        post_webhook(&url, &json, secret_file.as_deref(), WEBHOOK_TIMEOUT);
        drop(permit);
    });
}

/// True if the host is loopback, link-local (incl. `169.254.169.254`), unspecified, or
/// `localhost`. Does not block RFC-1918 / ULA / `.local` — LAN webhooks are valid.
/// Textual + IP-literal only; no DNS, so not an anti-rebinding defense.
fn webhook_host_is_internal(url: &str) -> bool {
    let hostport = webhook_authority(url);
    let host = if let Some(rest) = hostport.strip_prefix('[') {
        rest.split(']').next().unwrap_or("") // [::1]:443 → ::1
    } else {
        hostport
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or(hostport)
    };
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() || host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(std::net::IpAddr::V6(v6)) => {
            // Loopback (::1), unspecified (::), or link-local fe80::/10.
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false, // a resolvable hostname — not statically classifiable here
    }
}

/// `scheme://[userinfo@]host[:port]/...` → the bare `host[:port]`. Textual, no DNS.
fn webhook_authority(url: &str) -> &str {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority)
}

/// Log `url`: `scheme://host[:port]` + [`short_id`]. Path, query, userinfo and control
/// characters dropped — the token is often a path segment, and `GET /api/v1/logs` serves the
/// ring verbatim. Artwork fetches log through this too.
pub(crate) fn webhook_origin(url: &str) -> String {
    let scheme = url
        .split_once("://")
        .map(|(s, _)| format!("{s}://"))
        .unwrap_or_default();
    format!("{scheme}{} {}", webhook_authority(url), short_id(url))
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

fn post_webhook(url: &str, json: &str, secret_file: Option<&std::path::Path>, timeout: Duration) {
    let origin = webhook_origin(url);
    // Verified TLS (ureq rustls roots). max_redirects(0): a compromised receiver cannot bounce the POST.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut req = agent.post(url).header("Content-Type", "application/json");
    if let Some(path) = secret_file {
        match std::fs::read(path) {
            Ok(secret) => {
                use hmac::{Hmac, KeyInit, Mac};
                let mut mac = match Hmac::<sha2::Sha256>::new_from_slice(&secret) {
                    Ok(m) => m,
                    Err(_) => {
                        tracing::error!(path = %path.display(), "webhook HMAC secret unusable");
                        return;
                    }
                };
                mac.update(json.as_bytes());
                let sig = hex::encode(mac.finalize().into_bytes());
                req = req.header("X-Punktfunk-Signature", &format!("sha256={sig}"));
            }
            Err(e) => {
                // Configured but unreadable: the operator wants signing. Do not POST unsigned.
                tracing::error!(path = %path.display(), error = %e,
                    "webhook HMAC secret unreadable — NOT posting unsigned");
                return;
            }
        }
    }
    match req.send(json) {
        Ok(resp) => {
            tracing::debug!(url = %origin, status = resp.status().as_u16(), "webhook delivered")
        }
        Err(ureq::Error::StatusCode(code)) => {
            tracing::warn!(url = %origin, status = code, "webhook rejected by receiver")
        }
        Err(e) => tracing::warn!(url = %origin, error = %e, "webhook delivery failed"),
    }
}

// ------------------------------------------------------------------------- per-app prep/undo

/// Per-app prep (Sunshine `prep-cmd` parity): `do` runs synchronously before launch;
/// `undo` runs at session end, reverse order, best-effort, including panic-unwind ([`PrepGuard`]).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq)]
pub struct PrepCmd {
    /// Command run before launch. Same recipe and ownership checks as hook `run`; stdin is `{}`.
    #[serde(rename = "do")]
    pub run: String,
    /// After session end. Skipped when its `do` failed (it never took effect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo: Option<String>,
}

/// Negotiated stream mode as `PF_STREAM_*` env for prep `do`/`undo` — same spelling as
/// [`crate::stream_marker`] (keys only ever added). Shared by both serving planes.
pub fn prep_mode_env(width: u32, height: u32, refresh_hz: u32, hdr: bool) -> [(String, String); 4] {
    [
        ("PF_STREAM_WIDTH".to_string(), width.to_string()),
        ("PF_STREAM_HEIGHT".to_string(), height.to_string()),
        ("PF_STREAM_REFRESH".to_string(), refresh_hz.to_string()),
        ("PF_STREAM_HDR".to_string(), u8::from(hdr).to_string()),
    ]
}

/// Armed `undo`s for one session's prep. Drop (end, error, panic-unwind) runs them in
/// reverse on a detached thread — teardown never blocks on operator code.
#[must_use = "dropping the guard immediately runs the undo commands"]
pub struct PrepGuard {
    undo: Vec<String>,
    env: Vec<(String, String)>,
}

/// Run prep steps synchronously, in order — the exception to fire-and-forget, because
/// prep must land before launch. Failed/refused `do` logs and continues; its `undo`
/// stays disarmed. Returns the guard that runs armed `undo`s at drop.
pub fn run_prep(cmds: &[PrepCmd], env: &[(String, String)]) -> PrepGuard {
    let timeout = Duration::from_secs(u64::from(DEFAULT_TIMEOUT_S));
    let mut undo = Vec::new();
    for c in cmds {
        let cmd = c.run.trim();
        if cmd.is_empty() {
            continue;
        }
        let label = cmd_label(cmd);
        if let Err(e) = exec_path_check(cmd) {
            tracing::error!(cmd = %label, "REFUSING prep command — {e}");
            continue;
        }
        tracing::info!(cmd = %label, "prep: running");
        if run_hook_process(cmd, "{}", env, timeout) {
            if let Some(u) = c.undo.as_deref().filter(|u| !u.trim().is_empty()) {
                undo.push(u.to_string());
            }
        } else if c.undo.is_some() {
            tracing::warn!(cmd = %label, "prep step failed — its undo is skipped");
        }
    }
    PrepGuard {
        undo,
        env: env.to_vec(),
    }
}

impl Drop for PrepGuard {
    fn drop(&mut self) {
        if self.undo.is_empty() {
            return;
        }
        let undo = std::mem::take(&mut self.undo);
        let env = std::mem::take(&mut self.env);
        let timeout = Duration::from_secs(u64::from(DEFAULT_TIMEOUT_S));
        // Detached: drop may be async or a panic-unwind; teardown must not block.
        // One thread keeps reverse-of-`do` order.
        std::thread::spawn(move || {
            for cmd in undo.iter().rev() {
                let label = cmd_label(cmd);
                if let Err(e) = exec_path_check(cmd) {
                    tracing::error!(cmd = %label, "REFUSING prep undo command — {e}");
                    continue;
                }
                tracing::info!(cmd = %label, "prep: running undo");
                run_hook_process(cmd, "{}", &env, timeout);
            }
        });
    }
}

// ------------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ClientRef, EventKind, HostEvent, Plane, StreamRef};

    fn sample_event() -> HostEvent {
        HostEvent {
            seq: 7,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::StreamStarted {
                stream: StreamRef {
                    mode: "2560x1440@120".into(),
                    hdr: true,
                    client: "Living Room TV".into(),
                    fingerprint: Some("9f86d081".into()),
                    app: Some("steam:570".into()),
                    plane: Plane::Native,
                    preset: None,
                },
            },
        }
    }

    #[test]
    fn validation_rejects_structural_errors() {
        let ok = HooksConfig {
            hooks: vec![HookEntry {
                on: "stream.*".into(),
                filter: None,
                run: Some("echo hi".into()),
                webhook: None,
                timeout_s: 30,
                debounce_ms: 0,
                hold: false,
                hmac_secret_file: None,
            }],
        };
        assert!(ok.validate().is_ok());

        let mut bad = ok.clone();
        bad.hooks[0].on = " ".into();
        assert!(bad.validate().is_err(), "empty `on`");

        let mut bad = ok.clone();
        bad.hooks[0].run = None;
        assert!(bad.validate().is_err(), "no action");

        let mut bad = ok.clone();
        bad.hooks[0].webhook = Some("ftp://nope".into());
        assert!(bad.validate().is_err(), "non-http webhook");

        let mut bad = ok.clone();
        bad.hooks[0].timeout_s = 0;
        assert!(bad.validate().is_err(), "zero timeout");
        bad.hooks[0].timeout_s = 601;
        assert!(bad.validate().is_err(), "over-ceiling timeout");

        let mut bad = ok.clone();
        bad.hooks[0].hold = true;
        assert!(bad.validate().is_err(), "hold on a kind nothing waits for");
        bad.hooks[0].on = "game.launching".into();
        assert!(bad.validate().is_ok(), "hold on the launch stage");
    }

    #[cfg(unix)]
    #[test]
    fn a_holding_hook_blocks_until_its_command_exits() {
        let h = HookEntry {
            on: "game.launching".into(),
            filter: None,
            run: Some("sleep 1".into()),
            webhook: None,
            timeout_s: 5,
            debounce_ms: 0,
            hold: true,
            hmac_secret_file: None,
        };
        let ev = crate::events::HostEvent {
            seq: 1,
            ts_ms: 0,
            schema: crate::events::SCHEMA_VERSION,
            kind: crate::events::EventKind::HostStopping,
        };
        let t = Instant::now();
        hold(&h, &ev);
        assert!(
            t.elapsed() >= Duration::from_millis(950),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn store_roundtrips_and_survives_corruption() {
        let path = std::env::temp_dir().join(format!(
            "pf-hooks-test-{}-{:p}.json",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&path);

        let store = HooksStore::load_from(path.clone());
        assert!(store.get().hooks.is_empty(), "unconfigured = no hooks");

        let cfg = HooksConfig {
            hooks: vec![HookEntry {
                on: "pairing.pending".into(),
                filter: Some(HookFilter {
                    plane: Some(Plane::Native),
                    ..Default::default()
                }),
                run: None,
                webhook: Some("https://ha.local/api/webhook/punktfunk".into()),
                timeout_s: 30,
                debounce_ms: 500,
                hold: false,
                hmac_secret_file: None,
            }],
        };
        store.set(cfg).unwrap();
        assert_eq!(store.get().hooks.len(), 1);

        let reload = HooksStore::load_from(path.clone());
        assert_eq!(reload.get().hooks.len(), 1);
        assert_eq!(reload.get().hooks[0].on, "pairing.pending");

        std::fs::write(&path, b"{ not json").unwrap();
        let corrupt = HooksStore::load_from(path.clone());
        assert!(corrupt.get().hooks.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hand_edited_file_reloads_without_restart() {
        let path = std::env::temp_dir().join(format!(
            "pf-hooks-reload-test-{}-{:p}.json",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&path);

        let store = HooksStore::load_from(path.clone());
        assert!(store.get().hooks.is_empty());

        std::fs::write(
            &path,
            br#"{"hooks":[{"on":"stream.started","run":"true"}]}"#,
        )
        .unwrap();
        assert_eq!(store.get().hooks.len(), 1, "hand edit applies on next read");
        assert_eq!(store.get().hooks[0].on, "stream.started");

        // Length differs so same-second mtime granularity cannot mask the second edit.
        std::fs::write(
            &path,
            br#"{"hooks":[{"on":"stream.started","run":"true"},{"on":"client.*","run":"true"}]}"#,
        )
        .unwrap();
        assert_eq!(store.get().hooks.len(), 2, "second hand edit applies too");

        std::fs::remove_file(&path).unwrap();
        assert!(store.get().hooks.is_empty(), "deleted file = no hooks");
    }

    #[test]
    fn filters_constrain_and_missing_fields_never_match() {
        let ev = sample_event();
        let f = HookFilter {
            client: Some("Living Room TV".into()),
            app: Some("steam:570".into()),
            plane: Some(Plane::Native),
            ..Default::default()
        };
        assert!(f.matches(&ev.kind));

        let f = HookFilter {
            client: Some("Bedroom".into()),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        // What the console writes: the device's certificate, which a rename leaves alone.
        // `stream.*` carries it, so the filter that survives a rename works on this kind too.
        let f = HookFilter {
            fingerprint: Some("9F86D081".into()),
            ..Default::default()
        };
        assert!(f.matches(&ev.kind), "fingerprint match ignores hex case");

        let f = HookFilter {
            fingerprint: Some("dead".into()),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        let f = HookFilter {
            plane: Some(Plane::Gamestream),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        // stream.* events carry no fingerprint — a fingerprint filter cannot match them.
        let f = HookFilter {
            fingerprint: Some("ab12".into()),
            ..Default::default()
        };
        assert!(!f.matches(&ev.kind));

        // Fingerprint matching is case-insensitive where the field exists.
        let connected = EventKind::ClientConnected {
            client: ClientRef {
                name: "Deck".into(),
                fingerprint: Some("AB12CD".into()),
                plane: Plane::Native,
                preset: None,
            },
        };
        let f = HookFilter {
            fingerprint: Some("ab12cd".into()),
            ..Default::default()
        };
        assert!(f.matches(&connected));
    }

    #[test]
    fn a_preset_filter_matches_by_id_or_name() {
        let docked = EventKind::ClientConnected {
            client: ClientRef {
                name: "Deck".into(),
                fingerprint: Some("ab12cd".into()),
                plane: Plane::Native,
                preset: Some(crate::events::PresetRef {
                    id: "3f9a0c11e2b4".into(),
                    name: "Docked".into(),
                }),
            },
        };
        let by = |want: &str| HookFilter {
            preset: Some(want.into()),
            ..Default::default()
        };
        assert!(by("3f9a0c11e2b4").matches(&docked));
        assert!(by("docked").matches(&docked), "a name matches in any case");
        assert!(!by("Handheld").matches(&docked));
        // No preset on the event: a preset filter never matches it.
        assert!(!by("Docked").matches(&sample_event().kind));
        // It rides the event JSON, so a hook's env names it.
        let json = serde_json::to_value(&docked).unwrap();
        assert_eq!(json["client"]["preset"]["name"], "Docked");
    }

    #[test]
    fn env_flattening_is_shell_safe_and_complete() {
        let ev = sample_event();
        let env = flatten_env(&ev);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PF_EVENT_KIND"), Some("stream.started"));
        assert_eq!(get("PF_EVENT_SEQ"), Some("7"));
        assert_eq!(get("PF_EVENT_STREAM_MODE"), Some("2560x1440@120"));
        assert_eq!(get("PF_EVENT_STREAM_HDR"), Some("true"));
        assert_eq!(get("PF_EVENT_STREAM_CLIENT"), Some("Living Room TV"));
        assert_eq!(get("PF_EVENT_STREAM_APP"), Some("steam:570"));
        assert_eq!(get("PF_EVENT_STREAM_PLANE"), Some("native"));
        assert!(get("PF_EVENT_JSON").unwrap().contains("\"seq\":7"));

        // Control chars in a client name must not reach env consumers.
        let mut evil = sample_event();
        if let EventKind::StreamStarted { stream } = &mut evil.kind {
            stream.client = "evil\nname\r\t".into();
        }
        let env = flatten_env(&evil);
        let v = env
            .iter()
            .find(|(k, _)| k == "PF_EVENT_STREAM_CLIENT")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(v, "evilname");
    }

    #[test]
    fn prep_mode_env_speaks_the_marker_vocabulary() {
        // Same names as the stream-marker file. HDR is 1/0 like the marker, not PF_EVENT_* true/false.
        let env = prep_mode_env(2560, 1440, 120, true);
        let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("PF_STREAM_WIDTH"), Some("2560"));
        assert_eq!(get("PF_STREAM_HEIGHT"), Some("1440"));
        assert_eq!(get("PF_STREAM_REFRESH"), Some("120"));
        assert_eq!(get("PF_STREAM_HDR"), Some("1"));
        assert_eq!(prep_mode_env(1, 1, 1, false)[3].1, "0");
    }

    #[cfg(unix)]
    #[test]
    fn exec_runs_with_stdin_and_env_and_timeout_kills() {
        let out = std::env::temp_dir().join(format!(
            "pf-hook-exec-{}-{:p}.txt",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&out);
        let ev = sample_event();
        let env = flatten_env(&ev);
        let json = serde_json::to_string(&ev).unwrap();
        run_hook_process(
            &format!(
                "printf '%s|' \"$PF_EVENT_KIND\" > {p}; cat >> {p}",
                p = out.display()
            ),
            &json,
            &env,
            Duration::from_secs(5),
        );
        let text = std::fs::read_to_string(&out).expect("hook wrote its file");
        assert!(text.starts_with("stream.started|"), "env delivered: {text}");
        assert!(text.contains("\"seq\":7"), "stdin delivered: {text}");
        let _ = std::fs::remove_file(&out);

        // Timeout must kill the process group, not wait out `sleep 30`.
        let started = Instant::now();
        run_hook_process("sleep 30", &json, &env, Duration::from_secs(1));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "timeout must kill the hook, not wait it out"
        );
    }

    #[cfg(unix)]
    #[test]
    fn prep_runs_do_in_order_and_undo_in_reverse() {
        let out = std::env::temp_dir().join(format!(
            "pf-prep-test-{}-{:p}.txt",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_file(&out);
        let step = |do_tag: &str, undo_tag: Option<&str>| PrepCmd {
            run: format!("echo {do_tag} >> {}", out.display()),
            undo: undo_tag.map(|t| format!("echo {t} >> {}", out.display())),
        };
        let cmds = vec![
            step("do-a", Some("undo-a")),
            step("do-b", Some("undo-b")),
            // `false` must not arm its undo.
            PrepCmd {
                run: "false".into(),
                undo: Some(format!("echo undo-never >> {}", out.display())),
            },
        ];
        let guard = run_prep(&cmds, &[]);
        let text = std::fs::read_to_string(&out).expect("prep steps ran synchronously");
        assert_eq!(text, "do-a\ndo-b\n", "dos run in order, before launch");

        drop(guard);
        // The undo thread is detached — poll for its completion.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&out).unwrap_or_default();
            if text.lines().count() >= 4 {
                assert_eq!(
                    text, "do-a\ndo-b\nundo-b\nundo-a\n",
                    "undos run in reverse; the failed step's undo is skipped"
                );
                break;
            }
            assert!(Instant::now() < deadline, "undo thread never ran: {text}");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Give the skipped-undo a beat to (wrongly) appear, then assert it didn't.
        std::thread::sleep(Duration::from_millis(200));
        assert!(!std::fs::read_to_string(&out)
            .unwrap()
            .contains("undo-never"));
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn prep_cmd_wire_shape() {
        // Wire spelling is `{ "do": …, "undo": … }`.
        let c: PrepCmd = serde_json::from_str(r#"{"do":"a","undo":"b"}"#).unwrap();
        assert_eq!(c.run, "a");
        assert_eq!(c.undo.as_deref(), Some("b"));
        let c: PrepCmd = serde_json::from_str(r#"{"do":"a"}"#).unwrap();
        assert!(c.undo.is_none());
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"do":"a"}"#);
    }

    #[cfg(unix)]
    #[test]
    fn ownership_check_refuses_world_writable_scripts() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "pf-hook-own-{}-{:p}.sh",
            std::process::id(),
            &0u8 as *const u8
        ));
        std::fs::write(&path, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(exec_path_check(&format!("{} arg", path.display())).is_ok());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            exec_path_check(&format!("{} arg", path.display())).is_err(),
            "world-writable script must be refused"
        );
        let _ = std::fs::remove_file(&path);

        // Bare command names are left to PATH; nonexistent paths are the shell's problem.
        assert!(exec_path_check("systemctl suspend").is_ok());
        assert!(exec_path_check("/nonexistent/definitely-not-here").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn ownership_check_sees_quoted_paths_and_writable_parents() {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::Permissions::from_mode;
        let dir = std::env::temp_dir().join(format!(
            "pf-hook-parent-{}-{:p}",
            std::process::id(),
            &0u8 as *const u8
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, mode(0o755)).unwrap();
        let script = dir.join("my hook.sh");
        std::fs::write(&script, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&script, mode(0o700)).unwrap();
        let quoted = format!("\"{}\" arg", script.display());

        assert!(exec_path_check(&quoted).is_ok(), "a sane quoted path runs");
        std::fs::set_permissions(&script, mode(0o777)).unwrap();
        assert!(
            exec_path_check(&quoted).is_err(),
            "world-writable script behind a quoted, space-bearing path must be refused"
        );
        assert!(
            exec_path_check(&format!("'{}'", script.display())).is_err(),
            "single quotes too"
        );
        assert!(
            exec_path_check(&script.display().to_string().replace(' ', "\\ ")).is_err(),
            "backslash-escaped spaces too"
        );

        // A writable parent can replace a well-owned script.
        std::fs::set_permissions(&script, mode(0o700)).unwrap();
        std::fs::set_permissions(&dir, mode(0o777)).unwrap();
        let err = exec_path_check(&quoted).expect_err("world-writable parent must be refused");
        assert!(err.contains(&dir.display().to_string()), "names it: {err}");
        std::fs::set_permissions(&dir, mode(0o755)).unwrap();
        assert!(exec_path_check(&quoted).is_ok(), "chmod go-w fixes it");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn secret_file_permissions_are_complained_about() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "pf-hook-secret-{}-{:p}.key",
            std::process::id(),
            &0u8 as *const u8
        ));
        std::fs::write(&path, b"s3cret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(secret_file_complaint(&path).is_none(), "0600 is the ask");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let why = secret_file_complaint(&path).expect("a world-readable secret is warned about");
        assert!(why.contains("644"), "the complaint names the mode: {why}");

        // Missing/unreadable is post_webhook's fail-closed case, not a permissions complaint.
        std::fs::remove_file(&path).unwrap();
        assert!(secret_file_complaint(&path).is_none());
    }

    /// `GET /api/v1/logs` serves the tracing ring verbatim: drop URL path and command args.
    #[test]
    fn log_labels_drop_the_credential_and_stay_identifiable() {
        let slack = "https://hooks.slack.com/services/T0000/B0000/XXXXsecretXXXX";
        let shown = webhook_origin(slack);
        assert!(
            shown.starts_with("https://hooks.slack.com "),
            "origin: {shown}"
        );
        assert!(!shown.contains("XXXXsecretXXXX"), "token dropped: {shown}");
        assert_ne!(
            shown,
            webhook_origin("https://hooks.slack.com/services/T1/B1/OTHER"),
            "two hooks to one host stay distinguishable"
        );

        // userinfo, path and query all go; the port stays (it names the receiver, not the secret).
        let creds = webhook_origin("https://user:pw@ha.local:8123/api/webhook/zzz?token=qqq");
        assert!(creds.starts_with("https://ha.local:8123 "), "{creds}");
        for secret in ["pw", "zzz", "qqq"] {
            assert!(!creds.contains(secret), "{secret} leaked: {creds}");
        }
        let forged = webhook_origin("https://cdn.example\r\nforged line/x.png");
        assert!(!forged.contains(['\r', '\n']), "{forged:?}");

        let cmd = "/usr/local/bin/notify.sh --token=SEKRIT-zz 'Living Room'";
        let label = cmd_label(cmd);
        assert!(
            label.starts_with("notify.sh #"),
            "says which script ran: {label}"
        );
        assert!(!label.contains("SEKRIT"), "arguments dropped: {label}");
        assert_eq!(
            label,
            cmd_label(cmd),
            "stable across one firing's log lines"
        );
        assert_ne!(
            label,
            cmd_label("/usr/local/bin/notify.sh --token=SEKRIT-yy")
        );
        assert!(cmd_label("curl -H \"Authorization: Bearer t\" https://x/y").starts_with("curl #"));
    }
}
