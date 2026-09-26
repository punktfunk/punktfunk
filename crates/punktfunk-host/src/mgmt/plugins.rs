//! In-memory, lease-based directory of running plugins and their loopback UI surfaces.
//!
//! Plugins register a port, per-boot secret, title, and icon. The console obtains credentials
//! server-side and proxies only to `127.0.0.1`. Registrations are never persisted or health-
//! checked and expire lazily after [`LEASE_TTL`].
//!
//! The shared `plugin-token` authenticates the runner, not an individual plugin. Any holder can
//! replace an id's registration, and [`crate::library::ask_plugin_launch`] treats that registration
//! as launch authority. Per-plugin ownership therefore requires runner process isolation rather
//! than an additional registry check.

use super::shared::*;
use crate::events::{emit, EventKind};
use axum::Extension;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

/// 90 s ≈ two missed 30 s SDK renewals before a plugin drops out of the listing.
const LEASE_TTL: Duration = Duration::from_secs(90);

/// The runner batches on a timer; past this the shipper drops its own backlog rather than let
/// one plugin fill the ring in a single request.
const MAX_LOG_BATCH: usize = 256;

/// Request body only — carries the secret. [`PluginUiPublic`] is the response view.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginUi {
    /// Loopback only — the host dials `127.0.0.1:<port>`; a registration cannot carry a hostname.
    pub port: u16,
    /// Per-boot; the console proxy presents it as `Authorization: Bearer`. Rotated on plugin restart.
    pub secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Serves a page the console opens and lists in the nav. Absent means yes, as before the flag.
    #[serde(default = "yes")]
    pub page: bool,
    /// Serves `GET/PUT /__config`, the plugin-wide settings form.
    #[serde(default)]
    pub config: bool,
    /// Serves `GET/PUT /__game?entry=<id>`, a tab on each library entry's page.
    #[serde(default)]
    pub game: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginRegistration {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Absent `ui` is a live listing with no nav entry — not an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<PluginUi>,
    /// Plugin kind, not a UI field. Console hides `library` from the nav (those plugins already have
    /// Game sources); omit the field to keep a nav page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Stages this plugin holds (`game.launching`). The host POSTs the event to `/__hold` on
    /// `ui.port` and waits for a 2xx. Needs `ui`.
    #[serde(default)]
    pub holds: Vec<String>,
    /// How long a hold may take, 1–120 000 ms. Absent means 30 000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold_timeout_ms: Option<u32>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginLogLine {
    /// Unix ms, kept verbatim — see [`crate::log_capture::LogRing::push_remote`].
    pub ts_ms: u64,
    /// Normalized by [`crate::log_capture::LogRing::push_remote`]; unknown becomes INFO.
    pub level: String,
    pub source: String,
    pub msg: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct PluginLogBatch {
    pub entries: Vec<PluginLogLine>,
}

/// Secret-free UI view for the listing. The secret never goes here.
#[derive(Serialize, ToSchema)]
pub(crate) struct PluginUiPublic {
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// See [`PluginUi::page`].
    pub page: bool,
    /// See [`PluginUi::config`].
    pub config: bool,
    /// See [`PluginUi::game`].
    pub game: bool,
}

/// Listing row. Never carries the secret — the browser reaches the UI only through the console proxy.
#[derive(Serialize, ToSchema)]
pub(crate) struct PluginSummary {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ui: Option<PluginUiPublic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

/// The only shape that returns a secret. The console BFF denylists this lookup from the browser.
#[derive(Serialize, ToSchema)]
pub(crate) struct UiCredential {
    pub port: u16,
    pub secret: String,
}

/// Internal, validated. No wire derives — must not leak the secret.
#[derive(Clone, PartialEq)]
struct StoredUi {
    port: u16,
    secret: String,
    icon: Option<String>,
    page: bool,
    config: bool,
    game: bool,
}

/// `expires_at` is monotonic [`Instant`] — a wall-clock jump must not expire a live lease.
struct Stored {
    title: String,
    version: Option<String>,
    ui: Option<StoredUi>,
    category: Option<String>,
    holds: Vec<String>,
    hold_timeout: Duration,
    expires_at: Instant,
}

impl Stored {
    /// Operator-visible fields only. A pure lease renewal matches and emits no event.
    fn public_eq(&self, v: &Valid) -> bool {
        self.title == v.title
            && self.version == v.version
            && self.ui == v.ui
            && self.category == v.category
    }
}

pub(crate) struct PluginRegistry {
    inner: RwLock<HashMap<String, Stored>>,
}

struct Valid {
    title: String,
    version: Option<String>,
    ui: Option<StoredUi>,
    category: Option<String>,
    holds: Vec<String>,
    hold_timeout: Duration,
}

impl PluginRegistry {
    fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// `true` iff an operator-visible field changed (the caller emits `plugins.changed`). A pure
    /// renewal is `false`.
    fn upsert(&self, id: &str, v: Valid) -> bool {
        let expires_at = Instant::now() + LEASE_TTL;
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let changed = match map.get(id) {
            // An expired prior entry counts as a change — it had stopped listing.
            Some(prev) => !prev.is_live() || !prev.public_eq(&v),
            None => true,
        };
        map.insert(
            id.to_string(),
            Stored {
                title: v.title,
                version: v.version,
                ui: v.ui,
                category: v.category,
                holds: v.holds,
                hold_timeout: v.hold_timeout,
                expires_at,
            },
        );
        changed
    }

    /// Also returns ids pruned this call (caller emits `plugins.changed`). Write lock so a stale
    /// entry is reaped exactly once.
    fn snapshot(&self) -> (Vec<PluginSummary>, Vec<String>) {
        let now = Instant::now();
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        let mut expired: Vec<String> = map
            .iter()
            .filter(|(_, s)| now >= s.expires_at)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            map.remove(id);
        }
        expired.sort();
        let mut live: Vec<PluginSummary> = map
            .iter()
            .map(|(id, s)| PluginSummary {
                id: id.clone(),
                title: s.title.clone(),
                version: s.version.clone(),
                ui: s.ui.as_ref().map(|u| PluginUiPublic {
                    port: u.port,
                    icon: u.icon.clone(),
                    page: u.page,
                    config: u.config,
                    game: u.game,
                }),
                category: s.category.clone(),
            })
            .collect();
        live.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        (live, expired)
    }

    /// Does not prune — a stale entry is reaped by the next [`snapshot`](Self::snapshot).
    fn credential(&self, id: &str) -> Option<UiCredential> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let s = map.get(id)?;
        if !s.is_live() {
            return None;
        }
        let ui = s.ui.as_ref()?;
        Some(UiCredential {
            port: ui.port,
            secret: ui.secret.clone(),
        })
    }

    /// Live plugins holding `stage`, with the credentials to call them. Read-only.
    fn holders(&self, stage: &str) -> Vec<Holder> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|(_, s)| s.is_live() && s.holds.iter().any(|h| h == stage))
            .filter_map(|(id, s)| {
                let ui = s.ui.as_ref()?;
                Some(Holder {
                    id: id.clone(),
                    port: ui.port,
                    secret: ui.secret.clone(),
                    timeout: s.hold_timeout,
                })
            })
            .collect()
    }

    /// Read-only; does not prune or emit.
    fn live_ids(&self) -> Vec<String> {
        let map = self.inner.read().unwrap_or_else(|e| e.into_inner());
        map.iter()
            .filter(|(_, s)| s.is_live())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// `true` only if a live entry existed. Expired or unknown is silent.
    fn remove(&self, id: &str) -> bool {
        let mut map = self.inner.write().unwrap_or_else(|e| e.into_inner());
        map.remove(id).is_some_and(|s| s.is_live())
    }
}

impl Stored {
    fn is_live(&self) -> bool {
        Instant::now() < self.expires_at
    }
}

/// Process-wide singleton, same shape as [`crate::events::bus`].
pub(crate) fn registry() -> &'static PluginRegistry {
    static REG: OnceLock<PluginRegistry> = OnceLock::new();
    REG.get_or_init(PluginRegistry::new)
}

/// Ids for the store's running column ([`super::store::list_installed`]). Not [`list_plugins`]:
/// that prunes and emits `plugins.changed`, which would fire on every store poll.
pub(crate) fn live_plugin_ids() -> Vec<String> {
    registry().live_ids()
}

/// A plugin that holds a stage, as [`crate::holds`] calls it. Carries the secret: never serialize.
pub(crate) struct Holder {
    pub id: String,
    pub port: u16,
    pub secret: String,
    pub timeout: Duration,
}

/// Live plugins registered to hold `stage`.
pub(crate) fn holders(stage: &str) -> Vec<Holder> {
    registry().holders(stage)
}

/// In-process `{port, secret}` for [`crate::library::ask_plugin_launch`]. Same lookup as
/// `GET /plugins/{id}/ui-credential`, without a management-API round trip to a port this process
/// already holds.
pub(crate) fn ui_credential(id: &str) -> Option<UiCredential> {
    registry().credential(id)
}

/// Bypass the HTTP router so [`crate::library::ask_plugin_launch`] tests can hit a stub server.
#[cfg(test)]
pub(crate) fn register_ui_for_test(id: &str, port: u16, secret: &str) {
    registry().upsert(
        id,
        Valid {
            title: id.to_string(),
            version: None,
            ui: Some(StoredUi {
                port,
                secret: secret.to_string(),
                icon: None,
                page: true,
                config: false,
                game: false,
            }),
            category: None,
            holds: Vec::new(),
            hold_timeout: Duration::from_secs(30),
        },
    );
}

/// Same kebab-case regex the SDK enforces, so the registration id matches the package name.
fn valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A title must not smuggle escapes or newlines into a log line or the nav.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Source is not a [`valid_plugin_id`] — the runner names a unit by `definePlugin` name, package
/// name, script stem, or `runner`. Sanitize, do not reject: controls go, length is capped, empty
/// becomes `runner` so a line is never attributed to nothing.
fn log_target(source: &str) -> String {
    let mut s = sanitize(source);
    if s.is_empty() {
        s = "runner".into();
    }
    // Cap on chars, not bytes — truncating a multi-byte name mid-sequence would panic.
    if s.chars().count() > 64 {
        s = s.chars().take(64).collect();
    }
    format!("plugin:{s}")
}

fn validate(reg: PluginRegistration) -> Result<Valid, String> {
    let title = sanitize(&reg.title);
    if title.is_empty() {
        return Err("title must not be empty".into());
    }
    if title.chars().count() > 64 {
        return Err("title must be at most 64 characters".into());
    }
    let version = match reg.version {
        Some(v) => {
            let v = sanitize(&v);
            if v.chars().count() > 32 {
                return Err("version must be at most 32 characters".into());
            }
            (!v.is_empty()).then_some(v)
        }
        None => None,
    };
    let ui = match reg.ui {
        Some(u) => Some(validate_ui(u)?),
        None => None,
    };
    // Closed charset, open vocabulary: an unknown category is stored and matches no console rule,
    // so a newer plugin still registers against an older host.
    let category = match reg.category {
        Some(c) => {
            let ok = (1..=32).contains(&c.len())
                && c.starts_with(|ch: char| ch.is_ascii_lowercase())
                && c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err(
                    "category must be 1–32 chars of [a-z0-9-], starting with a letter".into(),
                );
            }
            Some(c)
        }
        None => None,
    };
    if let Some(bad) = reg
        .holds
        .iter()
        .find(|h| !crate::holds::STAGES.contains(&h.as_str()))
    {
        return Err(format!(
            "holds: unknown stage `{}` — known: {}",
            sanitize(bad),
            crate::holds::STAGES.join(", ")
        ));
    }
    if !reg.holds.is_empty() && ui.is_none() {
        return Err("holds needs ui.port and ui.secret — the host calls /__hold there".into());
    }
    let Some(hold_timeout) = crate::holds::deadline(reg.hold_timeout_ms) else {
        return Err(format!(
            "hold_timeout_ms must be 1–{}",
            crate::holds::HOLD_MAX_MS
        ));
    };
    let mut holds = reg.holds;
    holds.sort();
    holds.dedup();
    Ok(Valid {
        title,
        version,
        ui,
        category,
        holds,
        hold_timeout,
    })
}

fn validate_ui(u: PluginUi) -> Result<StoredUi, String> {
    if u.port < 1024 {
        return Err("ui.port must be a non-privileged port (>= 1024)".into());
    }
    let n = u.secret.len();
    if !(16..=128).contains(&n) {
        return Err("ui.secret must be 16–128 characters".into());
    }
    if !u
        .secret
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("ui.secret must be [A-Za-z0-9_-]".into());
    }
    let icon = match u.icon {
        Some(icon) => {
            let ok = (1..=48).contains(&icon.len())
                && icon
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !ok {
                return Err("ui.icon must be a lucide name ([a-z0-9-], 1–48 chars)".into());
            }
            Some(icon)
        }
        None => None,
    };
    Ok(StoredUi {
        port: u.port,
        secret: u.secret,
        icon,
        page: u.page,
        config: u.config,
        game: u.game,
    })
}

/// Register or renew a plugin
///
/// Idempotent lease renew (~30 s). Emits `plugins.changed` only when an operator-visible field
/// changed; a pure renewal is silent.
#[utoipa::path(
    put,
    path = "/plugins/{id}",
    tag = "plugins",
    operation_id = "registerPlugin",
    params(("id" = String, Path, description = "The plugin id (its `definePlugin` name: `[a-z][a-z0-9-]*`)")),
    request_body = PluginRegistration,
    responses(
        (status = NO_CONTENT, description = "Registered / renewed"),
        (status = BAD_REQUEST, description = "Invalid id or registration", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn register_plugin(
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    Path(id): Path<String>,
    ApiJson(reg): ApiJson<PluginRegistration>,
) -> Response {
    if !crate::mgmt::auth::plugin_owns(who.as_ref().map(|e| &e.0), &id) {
        return api_error(StatusCode::FORBIDDEN, "a plugin may only write its own id");
    }
    if !valid_plugin_id(&id) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid plugin id (expected kebab-case `[a-z][a-z0-9-]*`, ≤64)",
        );
    }
    let valid = match validate(reg) {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &e),
    };
    if registry().upsert(&id, valid) {
        tracing::info!(plugin = %id, "plugin registered");
        emit(EventKind::PluginsChanged { id });
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Ingest runner log lines
///
/// Plugins are not host children — their output never hits the host `tracing` subscriber. Lines
/// share the host ring as `plugin:<source>` so `GET /logs` needs no second cursor.
#[utoipa::path(
    post,
    path = "/plugins/logs",
    tag = "plugins",
    operation_id = "ingestPluginLogs",
    request_body = PluginLogBatch,
    responses(
        (status = NO_CONTENT, description = "Lines ingested"),
        (status = BAD_REQUEST, description = "Batch too large", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn ingest_plugin_logs(ApiJson(batch): ApiJson<PluginLogBatch>) -> Response {
    if batch.entries.len() > MAX_LOG_BATCH {
        return api_error(
            StatusCode::BAD_REQUEST,
            &format!("at most {MAX_LOG_BATCH} entries per batch"),
        );
    }
    for line in batch.entries {
        crate::log_capture::ring().push_remote(
            &line.level,
            &log_target(&line.source),
            &sanitize_msg(&line.msg),
            line.ts_ms,
        );
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Strip controls from an ingested message, keep tabs.
///
/// A newline would let one line forge several in a downloaded log. Tabs stay — they are
/// load-bearing in stack traces and CLI output.
fn sanitize_msg(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\t' || !c.is_control() { c } else { ' ' })
        .collect::<String>()
        .trim_end()
        .to_string()
}

/// List registered plugins
///
/// Live, secret-free directory. The console fetches the secret separately, server-side.
#[utoipa::path(
    get,
    path = "/plugins",
    tag = "plugins",
    operation_id = "listPlugins",
    responses(
        (status = OK, description = "Live plugin registrations", body = [PluginSummary]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_plugins() -> Json<Vec<PluginSummary>> {
    let (plugins, expired) = registry().snapshot();
    // Lazy expiry: reaped here exactly once and announced, so the event stream sees a departure
    // with no DELETE.
    for id in expired {
        tracing::info!(plugin = %id, "plugin lease expired");
        emit(EventKind::PluginsChanged { id });
    }
    Json(plugins)
}

/// Fetch a plugin UI's proxy credential
///
/// Server-side `{port, secret}` for the console proxy. The console BFF denylists this from the
/// browser.
#[utoipa::path(
    get,
    path = "/plugins/{id}/ui-credential",
    tag = "plugins",
    operation_id = "getPluginUiCredential",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = OK, description = "The proxy credential", body = UiCredential),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No live plugin with that id, or it serves no UI", body = ApiError),
    )
)]
pub(crate) async fn get_ui_credential(Path(id): Path<String>) -> Response {
    match registry().credential(&id) {
        Some(cred) => Json(cred).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no live plugin UI with that id"),
    }
}

/// Deregister a plugin
///
/// Immediate remove (SDK finalizer on SIGTERM). Emits `plugins.changed` only for a live entry;
/// unknown/expired is a silent 204.
#[utoipa::path(
    delete,
    path = "/plugins/{id}",
    tag = "plugins",
    operation_id = "deregisterPlugin",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = NO_CONTENT, description = "Deregistered (or already absent)"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn delete_plugin(
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    Path(id): Path<String>,
) -> Response {
    if !crate::mgmt::auth::plugin_owns(who.as_ref().map(|e| &e.0), &id) {
        return api_error(StatusCode::FORBIDDEN, "a plugin may only write its own id");
    }
    if registry().remove(&id) {
        tracing::info!(plugin = %id, "plugin deregistered");
        emit(EventKind::PluginsChanged { id });
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(title: &str, port: u16, secret: &str) -> PluginRegistration {
        PluginRegistration {
            title: title.into(),
            version: None,
            ui: Some(PluginUi {
                port,
                secret: secret.into(),
                icon: Some("gamepad-2".into()),
                page: true,
                config: false,
                game: false,
            }),
            category: None,
            holds: Vec::new(),
            hold_timeout_ms: None,
        }
    }

    const SECRET: &str = "abcdefghijklmnop0123";

    #[test]
    fn id_validation() {
        assert!(valid_plugin_id("rom-manager"));
        assert!(valid_plugin_id("a"));
        assert!(valid_plugin_id("x9"));
        assert!(!valid_plugin_id(""));
        assert!(!valid_plugin_id("9lives"));
        assert!(!valid_plugin_id("-lead"));
        assert!(!valid_plugin_id("Rom"));
        assert!(!valid_plugin_id("rom_manager"));
        assert!(!valid_plugin_id(&"a".repeat(65)));
    }

    #[test]
    fn registration_validation() {
        assert!(validate(reg("ROM Manager", 49321, SECRET)).is_ok());
        let v = validate(PluginRegistration {
            title: "Ro\u{7}m\n".into(),
            version: None,
            ui: None,
            category: None,
            holds: Vec::new(),
            hold_timeout_ms: None,
        })
        .unwrap();
        assert_eq!(v.title, "Rom");
        let lib = |c: &str| PluginRegistration {
            title: "X".into(),
            version: None,
            ui: None,
            category: Some(c.into()),
            holds: Vec::new(),
            hold_timeout_ms: None,
        };
        assert_eq!(
            validate(lib("library")).unwrap().category.as_deref(),
            Some("library")
        );
        assert!(validate(lib("some-future-kind")).is_ok());
        assert!(validate(lib("")).is_err());
        assert!(validate(lib("Library")).is_err());
        assert!(validate(lib("9lives")).is_err());
        assert!(validate(lib("lib_rary")).is_err());
        assert!(validate(lib(&"a".repeat(33))).is_err());
        assert!(validate(reg("x", 80, SECRET)).is_err());
        assert!(validate(reg("x", 49321, "tooshort")).is_err());
        assert!(validate(reg("x", 49321, "bad secret with spaces!!")).is_err());
        assert!(validate(reg("   ", 49321, SECRET)).is_err());
    }

    #[test]
    fn ui_flags_default_to_a_page_only() {
        let old: PluginUi =
            serde_json::from_value(serde_json::json!({"port": 49321, "secret": SECRET})).unwrap();
        assert!(old.page && !old.config && !old.game);
        let tab: PluginUi = serde_json::from_value(
            serde_json::json!({"port": 49321, "secret": SECRET, "page": false, "game": true}),
        )
        .unwrap();
        assert!(!tab.page && !tab.config && tab.game);
        let r = PluginRegistry::new();
        r.upsert("p", validate(reg("T", 49321, SECRET)).unwrap());
        let mut flagged = reg("T", 49321, SECRET);
        flagged.ui.as_mut().unwrap().game = true;
        assert!(
            r.upsert("p", validate(flagged).unwrap()),
            "a flag change is visible"
        );
        let ui = r.snapshot().0.remove(0).ui.unwrap();
        assert!(ui.page && ui.game && !ui.config);
    }

    #[test]
    fn a_hold_needs_a_known_stage_a_ui_and_a_sane_deadline() {
        let held = |holds: &[&str], ms: Option<u32>| PluginRegistration {
            holds: holds.iter().map(|h| h.to_string()).collect(),
            hold_timeout_ms: ms,
            ..reg("Slots", 49321, SECRET)
        };
        let v = validate(held(&["game.launching", "game.launching"], Some(5_000))).unwrap();
        assert_eq!(v.holds, vec!["game.launching".to_string()]);
        assert_eq!(v.hold_timeout, Duration::from_secs(5));
        assert!(validate(held(&["game.exited"], None)).is_err());
        assert!(validate(held(&["game.launching"], Some(0))).is_err());
        assert!(validate(held(&["game.launching"], Some(120_001))).is_err());
        let headless = PluginRegistration {
            ui: None,
            ..held(&["game.launching"], None)
        };
        assert!(validate(headless).is_err());

        let r = PluginRegistry::new();
        r.upsert("slots", validate(held(&["game.launching"], None)).unwrap());
        r.upsert("other", validate(reg("Other", 49322, SECRET)).unwrap());
        let h = r.holders("game.launching");
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].id.as_str(), h[0].port), ("slots", 49321));
        assert_eq!(h[0].timeout, Duration::from_secs(30));
        assert!(r.holders("game.exited").is_empty());
    }

    #[test]
    fn upsert_reports_change_but_not_renewal() {
        let r = PluginRegistry::new();
        assert!(r.upsert("p", validate(reg("Title", 49321, SECRET)).unwrap()));
        assert!(!r.upsert("p", validate(reg("Title", 49321, SECRET)).unwrap()));
        assert!(r.upsert(
            "p",
            validate(reg("Title", 49321, "ZZZZZZZZZZZZZZZZ")).unwrap()
        ));
        assert!(r.upsert(
            "p",
            validate(reg("New Title", 49321, "ZZZZZZZZZZZZZZZZ")).unwrap()
        ));
    }

    #[test]
    fn snapshot_lists_secret_free_sorted() {
        let r = PluginRegistry::new();
        r.upsert("zeta", validate(reg("Zeta", 50000, SECRET)).unwrap());
        r.upsert("alpha", validate(reg("Alpha", 50001, SECRET)).unwrap());
        let (plugins, expired) = r.snapshot();
        assert!(expired.is_empty());
        assert_eq!(
            plugins.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(r.credential("alpha").unwrap().secret, SECRET);
        assert_eq!(r.credential("alpha").unwrap().port, 50001);
    }

    #[test]
    fn credential_absent_for_ui_less_and_unknown() {
        let r = PluginRegistry::new();
        r.upsert(
            "headless",
            validate(PluginRegistration {
                title: "Headless".into(),
                version: None,
                ui: None,
                category: None,
                holds: Vec::new(),
                hold_timeout_ms: None,
            })
            .unwrap(),
        );
        assert!(r.credential("headless").is_none());
        assert!(r.credential("nope").is_none());
    }

    #[test]
    fn expired_entries_drop_from_listing_and_credential() {
        let r = PluginRegistry::new();
        r.upsert("p", validate(reg("P", 49321, SECRET)).unwrap());
        {
            let mut map = r.inner.write().unwrap();
            map.get_mut("p").unwrap().expires_at =
                Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
        }
        assert!(r.credential("p").is_none());
        let (plugins, expired) = r.snapshot();
        assert!(plugins.is_empty());
        assert_eq!(expired, vec!["p".to_string()]);
        // Second snapshot must not re-announce.
        assert!(r.snapshot().1.is_empty());
    }

    #[test]
    fn remove_reports_live_only() {
        let r = PluginRegistry::new();
        r.upsert("p", validate(reg("P", 49321, SECRET)).unwrap());
        assert!(r.remove("p"));
        assert!(!r.remove("p"));
    }
}
