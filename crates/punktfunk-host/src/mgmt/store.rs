//! HTTP surface for the plugin **store**: catalogs, install/uninstall, sources, runner
//! (design `plugin-store.md`). Domain logic is [`crate::store`].
//!
//! Auth is admin bearer + loopback, same as other mutations, and **denied to the plugin
//! token** ([`super::auth::plugin_may_access`]). The deny is an exclusion-list carve-out;
//! a test there pins it. A plugin that can install plugins can persist a helper outside
//! its own constraints.
//!
//! Handlers hop to `spawn_blocking`: catalog scans, fetches, and package-manager spawns
//! would stall the runtime.

use super::shared::*;
use crate::store::{self, index, jobs, manifest, sources};

async fn blocking<T, F>(f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!("plugin-store worker panicked: {e}");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The plugin store stopped responding",
        )
    })
}

// ---------------------------------------------------------------- wire shapes

/// The console greys out rows this host cannot install.
#[derive(Serialize, ToSchema)]
pub(crate) struct HostFacts {
    pub version: String,
    /// `linux` / `windows` / `macos`.
    pub platform: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SourceView {
    pub name: String,
    pub url: String,
    /// Built-in `unom`: not editable, not removable; only its entries may be `verified`.
    pub builtin: bool,
    /// Unsigned sources still serve; the console flags them.
    pub signed: bool,
    /// Last refresh missed; entries still install — the pin travelled with the entry.
    pub stale: bool,
    /// Unix seconds of the cached index, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub entry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CatalogEntry {
    pub id: String,
    pub pkg: String,
    pub title: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub author: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    /// The one installable version this entry pins.
    pub version: String,
    pub source: String,
    /// `verified` (built-in) or `external` (operator-added). Never `unverified`: those come from a
    /// raw spec and are not listed.
    pub tier: String,
    /// When unom reviewed this exact tarball (built-in source only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<String>,
    pub platforms: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_host: Option<String>,
    pub compatible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incompatible_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    pub update_available: bool,
    /// Revocation of the catalogued version; listed, never offered quietly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
    /// Browse filters; the Game-sources add-rail is exactly the `library` entries.
    pub categories: Vec<String>,
    /// Host-side existence probe for the launcher this plugin scans. `null` = no probe for this
    /// platform (unknown, not "not installed").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct CatalogResponse {
    pub host: HostFacts,
    pub sources: Vec<SourceView>,
    pub plugins: Vec<CatalogEntry>,
    pub busy: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct InstalledView {
    pub pkg: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Install-time tier; unverified stays unverified after the dialog is gone.
    pub tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    /// Key for `GET /plugins`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
    pub running: bool,
    /// Catalog version when newer than installed (a version string, not a bool).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_available: Option<String>,
    /// Revocation of the installed version. Reported, never auto-removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked: Option<String>,
}

/// Catalog `{source, id}` or a raw `spec` the operator owns.
#[derive(Deserialize, ToSchema)]
pub(crate) struct InstallRequest {
    /// With [`Self::id`]: catalogued install.
    #[serde(default)]
    pub source: Option<String>,
    /// With [`Self::source`]: catalogued install.
    #[serde(default)]
    pub id: Option<String>,
    /// Raw spec (`@scope/name`, `@scope/name@1.2.3`, https tarball or git+https). Nothing reviewed
    /// it and nothing pins it.
    #[serde(default)]
    pub spec: Option<String>,
    /// Required with [`Self::spec`]. Without it the API refuses; a caller cannot skip the
    /// unverified decision.
    #[serde(default)]
    pub accept_unverified: bool,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct UninstallRequest {
    pub pkg: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct JobRef {
    pub job: String,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SourceInput {
    pub url: String,
    /// `ed25519:<base64>`. Omitted ⇒ an unsigned source (accepted, flagged everywhere).
    #[serde(default)]
    pub public_key: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RuntimeView {
    pub installed: bool,
    pub enabled: bool,
    pub running: bool,
    /// systemd unit or scheduled-task name.
    pub unit: String,
    /// Windows: the account the task runs as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RuntimeRequest {
    pub enabled: bool,
}

// ---------------------------------------------------------------- assembly

fn runtime_view() -> RuntimeView {
    let st = crate::plugins::runtime_status();
    RuntimeView {
        installed: st.installed,
        enabled: st.enabled,
        running: st.running,
        unit: st.unit.to_string(),
        principal: st.principal,
        detail: (!st.detail.is_empty()).then_some(st.detail),
    }
}

/// Merged catalog, annotated with this host's install state. `force` refreshes past the TTL.
fn build_catalog(force: bool) -> CatalogResponse {
    let states = store::catalogs(force);
    let installed = store::installed_packages(&store::plugins_dir());
    let mut plugins = Vec::new();
    let mut source_views = Vec::new();

    for st in &states {
        let count = st.index.as_ref().map(|i| i.plugins.len()).unwrap_or(0);
        source_views.push(SourceView {
            name: st.source.name.clone(),
            url: st.source.url.clone(),
            builtin: st.source.is_official(),
            signed: st.source.is_signed(),
            stale: st.stale,
            fetched_at: st.fetched_at,
            error: st.error.clone(),
            entry_count: count as u32,
            public_key: st.source.public_key.clone(),
        });
        let Some(idx) = &st.index else { continue };
        let verified = st.source.is_official();
        for e in &idx.plugins {
            let installed_version = installed
                .iter()
                .find(|p| p.pkg == e.pkg)
                .and_then(|p| p.version.clone());
            let reason = e.incompatible_reason();
            plugins.push(CatalogEntry {
                id: e.id.clone(),
                pkg: e.pkg.clone(),
                title: e.title.clone(),
                description: e.description.clone(),
                icon: e.icon.clone(),
                author: e.author.clone(),
                homepage: e.homepage.clone(),
                license: e.license.clone(),
                version: e.version.clone(),
                source: st.source.name.clone(),
                // Verified is the built-in source only; a third-party curator cannot confer unom's review.
                tier: if verified { "verified" } else { "external" }.to_string(),
                reviewed_at: verified
                    .then(|| e.verification.as_ref().map(|v| v.reviewed_at.clone()))
                    .flatten(),
                platforms: e.platforms.clone(),
                min_host: e.min_host.clone(),
                compatible: reason.is_none(),
                incompatible_reason: reason,
                update_available: installed_version.is_some()
                    && newer(installed_version.as_deref(), &e.version),
                installed_version,
                blocked: store::advisory_for(&e.pkg, Some(&e.version)).map(|a| a.reason),
                categories: e.categories.clone(),
                detected: e.detected(),
            });
        }
    }
    // Title then source, so duplicates across sources don't jitter between polls.
    plugins.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.source.cmp(&b.source))
    });
    CatalogResponse {
        host: HostFacts {
            version: index::host_version().to_string(),
            platform: index::HOST_PLATFORM.to_string(),
        },
        sources: source_views,
        plugins,
        busy: jobs::busy(),
    }
}

fn build_installed(live: &[String]) -> Vec<InstalledView> {
    let dir = store::plugins_dir();
    let records = manifest::load(&dir);
    let catalogs = store::cached_catalogs();
    let mut out: Vec<InstalledView> = store::installed_packages(&dir)
        .into_iter()
        .map(|p| {
            let rec = records.get(&p.pkg);
            // Catalog row, if any source still lists it: update version and a title for CLI installs.
            let entry = catalogs.iter().find_map(|s| {
                s.index
                    .as_ref()?
                    .plugins
                    .iter()
                    .find(|e| e.pkg == p.pkg)
                    .map(|e| (e, s.source.name.clone()))
            });
            let plugin_id = entry
                .as_ref()
                .map(|(e, _)| e.id.clone())
                .or_else(|| store::plugin_id_for_pkg(&p.pkg));
            InstalledView {
                // No manifest record means the CLI put it there (`cli` tier).
                tier: rec
                    .map(|r| r.tier)
                    .unwrap_or(manifest::Tier::Cli)
                    .as_str()
                    .to_string(),
                source: rec
                    .and_then(|r| r.source.clone())
                    .or_else(|| entry.as_ref().map(|(_, s)| s.clone())),
                entry_id: rec
                    .and_then(|r| r.entry_id.clone())
                    .or_else(|| entry.as_ref().map(|(e, _)| e.id.clone())),
                title: entry.as_ref().map(|(e, _)| e.title.clone()),
                installed_at: rec.and_then(|r| r.installed_at.clone()),
                running: plugin_id
                    .as_deref()
                    .is_some_and(|id| live.iter().any(|l| l == id)),
                // Only a version this host can run: an incompatible one is refused at install.
                update_available: entry.as_ref().and_then(|(e, _)| {
                    (newer(p.version.as_deref(), &e.version) && e.incompatible_reason().is_none())
                        .then(|| e.version.clone())
                }),
                blocked: store::advisory_for(&p.pkg, p.version.as_deref()).map(|a| a.reason),
                plugin_id,
                version: p.version,
                pkg: p.pkg,
            }
        })
        .collect();
    out.sort_by(|a, b| a.pkg.cmp(&b.pkg));
    out
}

/// Is the catalog's `want` newer than the installed `have`? A CLI install can be ahead of the
/// catalog, and offering its catalog version would be a downgrade. Unparseable versions fall
/// back to "different".
fn newer(have: Option<&str>, want: &str) -> bool {
    match (
        have.and_then(|v| semver::Version::parse(v).ok()),
        semver::Version::parse(want).ok(),
    ) {
        (Some(have), Some(want)) => want > have,
        _ => have != Some(want),
    }
}

#[cfg(test)]
#[test]
fn an_update_is_only_ever_newer() {
    assert!(newer(Some("0.6.0"), "0.6.1"));
    assert!(
        !newer(Some("0.6.2"), "0.6.1"),
        "a CLI install ahead of the catalog"
    );
    assert!(!newer(Some("0.6.1"), "0.6.1"));
    assert!(newer(None, "0.6.1"));
}

// ---------------------------------------------------------------- handlers

/// Browse the plugin catalog
///
/// Merged view across sources, annotated with what this host has and can run. A source past its
/// freshness window is refreshed first; a miss keeps the last copy marked `stale` — the pin
/// travelled with the entry.
#[utoipa::path(
    get,
    path = "/store/catalog",
    tag = "store",
    operation_id = "getPluginCatalog",
    responses(
        (status = OK, description = "The merged catalog", body = CatalogResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn get_catalog() -> Response {
    match blocking(|| build_catalog(false)).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => e,
    }
}

/// Refresh every catalog now
///
/// Bypasses the freshness window, re-fetches all sources, returns the merged catalog.
#[utoipa::path(
    post,
    path = "/store/refresh",
    tag = "store",
    operation_id = "refreshPluginCatalog",
    responses(
        (status = OK, description = "The freshly-fetched catalog", body = CatalogResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn refresh_catalog() -> Response {
    match blocking(|| build_catalog(true)).await {
        Ok(c) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            Json(c).into_response()
        }
        Err(e) => e,
    }
}

/// List installed plugins
///
/// Plugins-dir packages, with provenance and live registration. No provenance record means CLI
/// install (`tier: "cli"`); absence is the answer, not a gap.
#[utoipa::path(
    get,
    path = "/store/installed",
    tag = "store",
    operation_id = "listInstalledPlugins",
    responses(
        (status = OK, description = "Installed plugin packages", body = [InstalledView]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn list_installed() -> Response {
    // Lease registry is in-memory; read it here, not inside `spawn_blocking`.
    let live: Vec<String> = super::plugins::live_plugin_ids();
    match blocking(move || build_installed(&live)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e,
    }
}

/// Install a plugin
///
/// `{source, id}` installs the catalog pin after re-checking integrity, or `{spec,
/// accept_unverified: true}` installs unreviewed code the operator owns. `202` + job id; watch at
/// `GET /store/jobs/{id}`.
///
/// One package operation at a time (`409` otherwise): `bun` shares a lockfile and `node_modules`.
#[utoipa::path(
    post,
    path = "/store/install",
    tag = "store",
    operation_id = "installPlugin",
    request_body = InstallRequest,
    responses(
        (status = ACCEPTED, description = "Install job started", body = JobRef),
        (status = BAD_REQUEST, description = "Unknown entry, bad spec, or missing acknowledgement", body = ApiError),
        (status = CONFLICT, description = "Another package operation is in flight", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn install_plugin(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<InstallRequest>,
) -> Response {
    let plan =
        match (
            req.source.as_deref(),
            req.id.as_deref(),
            req.spec.as_deref(),
        ) {
            (Some(source), Some(id), None) => {
                let found = match blocking({
                    let (source, id) = (source.to_string(), id.to_string());
                    move || store::find_entry(&source, &id)
                })
                .await
                {
                    Ok(f) => f,
                    Err(e) => return e,
                };
                let Some((entry, verified)) = found else {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "no such plugin in that source's catalog",
                    );
                };
                if let Some(reason) = entry.incompatible_reason() {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("This plugin can't run on this host — {reason}"),
                    );
                }
                match jobs::Plan::from_entry(&entry, source, verified) {
                    Ok(p) => p,
                    Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
                }
            }
            (None, None, Some(spec)) => {
                if !req.accept_unverified {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        "installing from a raw package spec runs unreviewed code with operator \
                     privileges — set `accept_unverified` to confirm",
                    );
                }
                match jobs::Plan::from_spec(spec) {
                    Ok(p) => p,
                    Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
                }
            }
            _ => return api_error(
                StatusCode::BAD_REQUEST,
                "provide either {source, id} for a catalogued plugin or {spec} for a raw package",
            ),
        };

    match jobs::spawn_install(plan, st.plugin_tokens.clone()) {
        Ok(job) => (StatusCode::ACCEPTED, Json(JobRef { job })).into_response(),
        Err(e) => api_error(StatusCode::CONFLICT, &format!("{e:#}")),
    }
}

/// Uninstall a plugin
///
/// Drops the package and its provenance, then restarts the runner. Only names the runner would
/// supervise: a shared dependency cannot be ripped out of the tree.
#[utoipa::path(
    post,
    path = "/store/uninstall",
    tag = "store",
    operation_id = "uninstallPlugin",
    request_body = UninstallRequest,
    responses(
        (status = ACCEPTED, description = "Uninstall job started", body = JobRef),
        (status = BAD_REQUEST, description = "Not a plugin package name", body = ApiError),
        (status = CONFLICT, description = "Another package operation is in flight", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn uninstall_plugin(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<UninstallRequest>,
) -> Response {
    if let Err(e) = store::valid_installed_pkg(&req.pkg) {
        return api_error(StatusCode::BAD_REQUEST, &format!("{e:#}"));
    }
    // `@scope/plugin-*` also matches `@punktfunk/plugin-kit`, a library plugins depend on.
    // Only a top-level install (`installed_packages`) may be removed.
    let pkg = req.pkg.clone();
    let known = match blocking(move || {
        store::installed_packages(&store::plugins_dir())
            .iter()
            .any(|p| p.pkg == pkg)
    })
    .await
    {
        Ok(k) => k,
        Err(e) => return e,
    };
    if !known {
        return api_error(
            StatusCode::BAD_REQUEST,
            "that package is not an installed plugin — it may be a dependency of one, or already \
             removed",
        );
    }
    match jobs::spawn_uninstall(req.pkg, st.plugin_tokens.clone()) {
        Ok(job) => (StatusCode::ACCEPTED, Json(JobRef { job })).into_response(),
        Err(e) => api_error(StatusCode::CONFLICT, &format!("{e:#}")),
    }
}

/// List recent package jobs
#[utoipa::path(
    get,
    path = "/store/jobs",
    tag = "store",
    operation_id = "listPluginJobs",
    responses(
        (status = OK, description = "Recent install/uninstall jobs, oldest first", body = [Job]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn list_jobs() -> Json<Vec<jobs::Job>> {
    Json(jobs::list())
}

/// Follow one package job
///
/// Poll this while `state` is `running`; `log` carries the tail of the package manager's output.
#[utoipa::path(
    get,
    path = "/store/jobs/{id}",
    tag = "store",
    operation_id = "getPluginJob",
    params(("id" = String, Path, description = "The job id returned by install/uninstall")),
    responses(
        (status = OK, description = "The job", body = Job),
        (status = NOT_FOUND, description = "No such job (they are kept for a bounded history)", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn get_job(Path(id): Path<String>) -> Response {
    match jobs::get(&id) {
        Some(j) => Json(j).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no such job"),
    }
}

/// List catalog sources
#[utoipa::path(
    get,
    path = "/store/sources",
    tag = "store",
    operation_id = "listPluginSources",
    responses(
        (status = OK, description = "Configured sources, built-in first", body = [SourceView]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn list_sources() -> Response {
    match blocking(|| {
        store::cached_catalogs()
            .into_iter()
            .map(|st| SourceView {
                name: st.source.name.clone(),
                url: st.source.url.clone(),
                builtin: st.source.is_official(),
                signed: st.source.is_signed(),
                stale: st.stale,
                fetched_at: st.fetched_at,
                error: st.error,
                entry_count: st.index.map(|i| i.plugins.len()).unwrap_or(0) as u32,
                public_key: st.source.public_key,
            })
            .collect::<Vec<_>>()
    })
    .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => e,
    }
}

/// Add or update a catalog source
///
/// Its entries become installable and attributed to it. They never carry `verified`; that badge is
/// the built-in source alone.
#[utoipa::path(
    put,
    path = "/store/sources/{name}",
    tag = "store",
    operation_id = "putPluginSource",
    params(("name" = String, Path, description = "Source slug (`[a-z][a-z0-9-]*`)")),
    request_body = SourceInput,
    responses(
        (status = NO_CONTENT, description = "Source saved"),
        (status = BAD_REQUEST, description = "Invalid name, url or key — or the reserved built-in name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn put_source(
    Path(name): Path<String>,
    ApiJson(input): ApiJson<SourceInput>,
) -> Response {
    let source = sources::Source {
        name: name.clone(),
        url: input.url,
        public_key: input.public_key.filter(|k| !k.trim().is_empty()),
    };
    let saved = blocking(move || {
        let r = sources::put(source);
        if r.is_ok() {
            // A redefined source must not keep serving the old cache.
            store::drop_source_cache(&name);
        }
        r
    })
    .await;
    match saved {
        Err(e) => e,
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Ok(Ok(())) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Remove a catalog source
#[utoipa::path(
    delete,
    path = "/store/sources/{name}",
    tag = "store",
    operation_id = "deletePluginSource",
    params(("name" = String, Path, description = "Source slug")),
    responses(
        (status = NO_CONTENT, description = "Removed (or already absent)"),
        (status = FORBIDDEN, description = "The built-in source cannot be removed, or the plugin token is not authorized", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn delete_source(Path(name): Path<String>) -> Response {
    if name == sources::OFFICIAL_NAME {
        return api_error(
            StatusCode::FORBIDDEN,
            "the built-in source cannot be removed",
        );
    }
    let removed = blocking(move || {
        let r = sources::remove(&name);
        if matches!(r, Ok(true)) {
            store::drop_source_cache(&name);
        }
        r
    })
    .await;
    match removed {
        Err(e) => e,
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Ok(Ok(_)) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            StatusCode::NO_CONTENT.into_response()
        }
    }
}

/// Plugin runner state
///
/// Plugins run only while the runner is on. The runner discovers units at startup, so a freshly
/// installed plugin may not have appeared yet.
#[utoipa::path(
    get,
    path = "/store/runtime",
    tag = "store",
    operation_id = "getPluginRuntime",
    responses(
        (status = OK, description = "Runner state", body = RuntimeView),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn get_runtime() -> Response {
    match blocking(runtime_view).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e,
    }
}

/// Turn the plugin runner on or off
#[utoipa::path(
    post,
    path = "/store/runtime",
    tag = "store",
    operation_id = "setPluginRuntime",
    request_body = RuntimeRequest,
    responses(
        (status = OK, description = "The resulting runner state", body = RuntimeView),
        (status = BAD_REQUEST, description = "The runner could not be switched", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not authorized for the plugin token", body = ApiError),
    )
)]
pub(crate) async fn set_runtime(ApiJson(req): ApiJson<RuntimeRequest>) -> Response {
    let switched =
        blocking(move || crate::plugins::set_runtime_enabled(req.enabled).map(|()| runtime_view()))
            .await;
    match switched {
        Err(e) => e,
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Ok(Ok(v)) => {
            crate::events::emit(crate::events::EventKind::StoreChanged);
            Json(v).into_response()
        }
    }
}

// Re-exported so `routes!` can name the response body types.
pub(crate) use crate::store::jobs::Job;
