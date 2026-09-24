//! HTTP handlers for the game catalog, source toggles, provider reconcile, and box-art proxy.
//!
//! Domain logic lives in [`crate::library`] and [`crate::runstate`]; this is the `/api/v1/library`
//! surface. Lanes split on [`super::auth::AuthLane`]:
//! - **admin** — custom CRUD, hide/unhide, scanner toggles, and operator-only launch fields
//!   (`command`, `prep`).
//! - **plugin** — reconcile and liveness report for the store it publishes; privileged fields
//!   403, unservable local art is stripped rather than refusing the set.
//! - **paired cert** — `GET /library` (hidden titles filtered, `command` values cleared) and
//!   the art proxy.
//!
//! Pin: `mgmt::tests` lane matrix and `crate::library` art/privilege tests.

use super::auth::AuthLane;
use super::shared::*;
use axum::http::header;
use axum::Extension;
use sha2::{Digest, Sha256};

/// Refuse a custom-entry write that carries an operator-privileged field on a lane that may
/// not set one, or local art the proxy would not serve.
///
/// Single-entry writes only. Provider reconcile uses [`check_privileged_fields`] and
/// [`crate::library::sanitize_art_paths`]: one bad cover must not drop the whole store.
/// Route reachability is a different question — `PUT /library/provider/{p}` is a plugin
/// route, but `prep` / a non-host-resolved `launch.kind` are operator-only.
///
/// Returns `Some((reason, response))` to send, `None` to proceed. Not `Result`: the
/// "error" is the response, and a 128-byte `Response` in `Err` trips
/// `clippy::result_large_err`. `reason` is the log line so a 403 (privileged field) and a
/// 400 (unservable art) are not both logged as an auth failure.
fn check_entry_fields(
    lane: AuthLane,
    art: &crate::library::Artwork,
    launch: Option<&crate::library::LaunchSpec>,
    prep: &[crate::hooks::PrepCmd],
    icon: Option<&str>,
) -> Option<(String, Response)> {
    check_privileged_fields(lane, launch, prep, icon).or_else(|| {
        crate::library::validate_art_paths(art)
            .err()
            .map(|e| (e.clone(), api_error(StatusCode::BAD_REQUEST, &e)))
    })
}

/// Authority half of [`check_entry_fields`]: privileged field this lane may not set (403),
/// or an unrepresentable icon token (400).
///
/// Provider reconcile uses this and sanitizes unservable covers instead of 400-ing the
/// payload. See [`crate::library::sanitize_art_paths`].
fn check_privileged_fields(
    lane: AuthLane,
    launch: Option<&crate::library::LaunchSpec>,
    prep: &[crate::hooks::PrepCmd],
    icon: Option<&str>,
) -> Option<(String, Response)> {
    if !lane.may_set_privileged_fields() {
        if let Some(field) = crate::library::privileged_field(launch, prep) {
            return Some((
                format!("payload carries `{field}`, which this lane may not set"),
                api_error(
                    StatusCode::FORBIDDEN,
                    &format!(
                        "`{field}` can become a command the host runs as the host user, so it may \
                         only be set with the operator's admin token — a plugin may publish entries \
                         with any host-resolved launch kind (steam_appid, steam_ui, launcher_ui, \
                         epic, gog, aumid, xbox, lutris_id, heroic, playnite, uplay, amazon, \
                         battlenet) or `plugin` instead"
                    ),
                ),
            ));
        }
    }
    // Shape, every lane: an icon token names no host resource, so an operator token cannot
    // unlock it. Clients interpolate the value; refuse unrepresentable slugs here
    // (`crate::library::validate_icon`).
    if let Err(e) = crate::library::validate_icon(icon) {
        return Some((e.clone(), api_error(StatusCode::BAD_REQUEST, &e)));
    }
    None
}

#[derive(Deserialize)]
pub(crate) struct LibraryQuery {
    provider: Option<String>,
    platform: Option<String>,
}

/// List every title this host knows, sorted by title.
///
/// Plugin-synced entries plus custom ones. Art the host can serve is rewritten to this API's
/// art proxy, local paths and remote URLs alike; a URL the proxy already refused passes
/// through. `?provider=` / `?platform=` (case-insensitive) narrow.
///
/// The operator lane sees hidden titles (`hidden: true`) so the console can un-hide them.
/// Every other lane is filtered upstream and cannot tell they exist.
#[utoipa::path(
    get,
    path = "/library",
    tag = "library",
    operation_id = "getLibrary",
    params(
        ("provider" = Option<String>, Query, description = "Only entries owned by this external provider"),
        ("platform" = Option<String>, Query, description = "Only entries on this platform (case-insensitive, e.g. `PS2`)"),
    ),
    responses(
        (status = OK, description = "Unified library across all stores (the operator's lane also gets hidden entries, flagged)", body = [crate::library::OperatorGameEntry]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_library(
    Extension(lane): Extension<AuthLane>,
    Query(q): Query<LibraryQuery>,
) -> Response {
    // Operator list is a different type, not a flag, so a hidden title cannot reach a paired
    // client by forgetting a filter. Skip redaction here: this arm is the operator's token,
    // and the command line being shown is the one they typed.
    if lane.is_operator() {
        let mut rows = crate::library::all_games_for_operator();
        rows.retain(|r| matches_query(&r.entry, &q));
        for r in &mut rows {
            crate::library::proxy_art(&r.entry.id, &mut r.entry.art);
        }
        return Json(rows).into_response();
    }
    let mut games = crate::library::all_games();
    games.retain(|g| matches_query(g, &q));
    // Rewrite to the art proxy: an on-host path a client cannot reach, and a CDN URL every
    // client would otherwise fetch over the WAN for itself.
    for g in &mut games {
        crate::library::proxy_art(&g.id, &mut g.art);
    }
    // `cert_may_access` allows GET /library, so paired clients see this body. For a custom
    // entry `launch.value` is the operator's shell command; clear it. `kind` stays so the
    // client can still render launchability. Unconditional: the operator arm returned above.
    for g in &mut games {
        if let Some(l) = g.launch.as_mut() {
            if l.kind == "command" {
                l.value.clear();
            }
        }
    }
    Json(games).into_response()
}

/// Shared by both `get_library` arms so the filters cannot drift.
fn matches_query(g: &crate::library::GameEntry, q: &LibraryQuery) -> bool {
    if let Some(provider) = q.provider.as_deref().filter(|p| !p.is_empty()) {
        if g.provider.as_deref() != Some(provider) {
            return false;
        }
    }
    if let Some(platform) = q.platform.as_deref().filter(|p| !p.is_empty()) {
        if !g
            .meta
            .platform
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(platform))
        {
            return false;
        }
    }
    true
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct HiddenToggle {
    hidden: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct HiddenState {
    id: String,
    hidden: bool,
}

/// Hide or un-hide one library title.
///
/// Curation, not access control: the title leaves every play surface and launch resolution
/// but is not deleted. The operator console still lists it (`hidden: true`) so it can return.
/// The id is not required to exist now — a plugin mid-sync or an unmounted drive would
/// otherwise refuse a choice that should stick. Emits `library.changed` only on a real change.
#[utoipa::path(
    put,
    path = "/library/hidden/{id}",
    tag = "library",
    operation_id = "setLibraryEntryHidden",
    params(("id" = String, Path, description = "The library entry id (e.g. `steam:70`)")),
    request_body = HiddenToggle,
    responses(
        (status = OK, description = "Stored; the entry's visibility after the call", body = HiddenState),
        (status = BAD_REQUEST, description = "Empty entry id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the settings", body = ApiError),
    )
)]
pub(crate) async fn set_library_entry_hidden(
    Path(id): Path<String>,
    ApiJson(toggle): ApiJson<HiddenToggle>,
) -> Response {
    if id.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "entry id must not be empty");
    }
    match crate::library::set_entry_hidden(&id, toggle.hidden) {
        Ok(hidden) => {
            tracing::info!(entry = %id, hidden, "management API: library entry visibility set");
            Json(HiddenState { id, hidden }).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ScannerToggle {
    enabled: bool,
}

/// List library sources
///
/// One row per installed library plugin, with its enable state. Sources default to
/// enabled; disabling hides titles from the next read. The custom store is not a
/// source and is always on. Every row is
/// `origin: "plugin"`.
#[utoipa::path(
    get,
    path = "/library/scanners",
    tag = "library",
    operation_id = "listLibraryScanners",
    responses(
        (status = OK, description = "This host's scanners with their enable state", body = [crate::library::ScannerInfo]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_library_scanners() -> Json<Vec<crate::library::ScannerInfo>> {
    Json(crate::library::list_scanners())
}

/// Enable or disable a library source.
///
/// Takes effect on the next library read. Disabling hides titles; the plugin may keep
/// reconciling while off. Nothing is deleted. Emits `library.changed` when the state changes.
#[utoipa::path(
    put,
    path = "/library/scanners/{id}",
    tag = "library",
    operation_id = "setLibraryScanner",
    params(("id" = String, Path, description = "The scanner id (e.g. `steam`)")),
    request_body = ScannerToggle,
    responses(
        (status = OK, description = "Toggle stored; the full scanner list", body = [crate::library::ScannerInfo]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "A plugin toggled another plugin's source", body = ApiError),
        (status = NOT_FOUND, description = "No such scanner on this platform", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the settings", body = ApiError),
    )
)]
pub(crate) async fn set_library_scanner(
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    Path(id): Path<String>,
    ApiJson(toggle): ApiJson<ScannerToggle>,
) -> Response {
    if !crate::mgmt::auth::plugin_owns(who.as_ref().map(|e| &e.0), &id) {
        return api_error(
            StatusCode::FORBIDDEN,
            "a plugin may only toggle its own source",
        );
    }
    match crate::library::set_scanner_enabled(&id, toggle.enabled) {
        Ok(Some(scanners)) => {
            tracing::info!(
                scanner = %id,
                enabled = toggle.enabled,
                "management API: library scanner toggled"
            );
            Json(scanners).into_response()
        }
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no such scanner on this platform"),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Create a custom title
///
/// A user-curated entry. The host assigns a stable id, returned in the body.
#[utoipa::path(
    post,
    path = "/library/custom",
    tag = "library",
    operation_id = "createCustomGame",
    request_body = crate::library::CustomInput,
    responses(
        (status = CREATED, description = "Entry created", body = crate::library::CustomEntry),
        (status = BAD_REQUEST, description = "Empty title", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn create_custom_game(
    Extension(lane): Extension<AuthLane>,
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    if let Some((_, denied)) = check_entry_fields(
        lane,
        &input.art,
        input.launch.as_ref(),
        input.prep.as_deref().unwrap_or(&[]),
        input.icon.as_deref(),
    ) {
        return denied;
    }
    match crate::library::add_custom(input) {
        Ok(entry) => (StatusCode::CREATED, Json(entry)).into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// One custom entry as stored, `detect` and `prep` included
///
/// Operator lane only: the hint names host paths, which is why the catalog read
/// model leaves it out. The paired-cert allowlist names `GET /library` exactly.
#[utoipa::path(
    get,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "getCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    responses(
        (status = OK, description = "The stored entry", body = crate::library::CustomEntry),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
    )
)]
pub(crate) async fn get_custom_game(Path(id): Path<String>) -> Response {
    match crate::library::get_custom(&id) {
        Some(entry) => Json(entry).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no custom entry with that id"),
    }
}

#[utoipa::path(
    put,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "updateCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    request_body = crate::library::CustomInput,
    responses(
        (status = OK, description = "Entry updated", body = crate::library::CustomEntry),
        (status = BAD_REQUEST, description = "Empty title", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn update_custom_game(
    Extension(lane): Extension<AuthLane>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<crate::library::CustomInput>,
) -> Response {
    if input.title.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "title must not be empty");
    }
    if let Some((_, denied)) = check_entry_fields(
        lane,
        &input.art,
        input.launch.as_ref(),
        input.prep.as_deref().unwrap_or(&[]),
        input.icon.as_deref(),
    ) {
        return denied;
    }
    use crate::library::MutateOutcome;
    match crate::library::update_custom(&id, input) {
        Ok(MutateOutcome::Done(entry)) => Json(entry).into_response(),
        Ok(MutateOutcome::NotFound) => {
            api_error(StatusCode::NOT_FOUND, "no custom entry with that id")
        }
        Ok(MutateOutcome::ProviderOwned(p)) => api_error(
            StatusCode::CONFLICT,
            &format!("entry is owned by provider `{p}` — update it through its reconcile"),
        ),
        // Manual CRUD never requests a store claim; this arm is a programming error.
        Ok(MutateOutcome::StoreClaimed { .. }) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The entry wasn't updated — the host hit an unexpected state",
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[utoipa::path(
    delete,
    path = "/library/custom/{id}",
    tag = "library",
    operation_id = "deleteCustomGame",
    params(("id" = String, Path, description = "The custom entry id (without the `custom:` prefix)")),
    responses(
        (status = NO_CONTENT, description = "Entry deleted"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom entry with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn delete_custom_game(Path(id): Path<String>) -> Response {
    use crate::library::MutateOutcome;
    match crate::library::delete_custom(&id) {
        Ok(MutateOutcome::Done(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(MutateOutcome::NotFound) => {
            api_error(StatusCode::NOT_FOUND, "no custom entry with that id")
        }
        Ok(MutateOutcome::ProviderOwned(p)) => api_error(
            StatusCode::CONFLICT,
            &format!(
                "entry is owned by provider `{p}` — remove it there, or DELETE the provider set"
            ),
        ),
        // Manual CRUD never requests a store claim; this arm is a programming error.
        Ok(MutateOutcome::StoreClaimed { .. }) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The entry wasn't deleted — the host hit an unexpected state",
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProviderRemoved {
    removed: usize,
}

#[derive(Deserialize)]
pub(crate) struct ReconcileQuery {
    /// Claim this store so entries take `<store>:<external_id>` instead of `custom:<id>`.
    store: Option<String>,
}

/// Replace a provider's entries
///
/// The payload is the desired set, keyed by `external_id`. The host diffs, keeps surviving
/// host ids stable, and drops orphans. Empty array removes everything this provider owns.
/// Emits `library.changed` with the provider as `source`.
///
/// `?store=` claims that store: entries surface as `<store>:<external_id>` with the store
/// badge. One provider per store; a second claimant gets 409. Release the claim with
/// `DELETE`, not an empty reconcile — a store can have zero installed titles.
#[utoipa::path(
    put,
    path = "/library/provider/{provider}",
    tag = "library",
    operation_id = "reconcileProviderEntries",
    params(
        ("provider" = String, Path, description = "The provider id ([a-z0-9._-], `manual` reserved)"),
        ("store" = Option<String>, Query, description = "Claim this store for the provider ([a-z0-9_-], `custom`/`manual` reserved)"),
    ),
    request_body = Vec<crate::library::ProviderEntryInput>,
    responses(
        (status = OK, description = "The provider's resulting entries (host ids assigned/kept)", body = [crate::library::CustomEntry]),
        (status = BAD_REQUEST, description = "Invalid provider id, store id, or payload", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "That store is already claimed by another provider", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn reconcile_provider_entries(
    Extension(lane): Extension<AuthLane>,
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    Path(provider): Path<String>,
    Query(q): Query<ReconcileQuery>,
    ApiJson(mut inputs): ApiJson<Vec<crate::library::ProviderEntryInput>>,
) -> Response {
    if !crate::mgmt::auth::plugin_owns(who.as_ref().map(|e| &e.0), &provider) {
        return api_error(StatusCode::FORBIDDEN, "a plugin may only write its own id");
    }
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    let store = q.store.filter(|s| !s.is_empty());
    if let Some(store) = &store {
        if let Err(e) = crate::library::validate_store_claim(store) {
            return api_error(StatusCode::BAD_REQUEST, &e);
        }
    }
    match crate::library::validate_provider_payload(&provider, &mut inputs) {
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &e),
        // One warn per reconcile: a template that misses its roots misses every entry.
        Ok(dropped) => {
            if let Some((id, reason)) = dropped.first() {
                tracing::warn!(
                    provider,
                    dropped = dropped.len(),
                    first = %id,
                    reason = %reason,
                    "library reconcile dropped entries this host would not launch"
                );
            }
        }
    }
    // Check every entry: one privileged field anywhere is one command the host would run.
    // Art is not in this refusal — an unservable cover is stripped below so one bad
    // thumbnail cannot drop the store. Privileged fields still 403 the write.
    for (i, e) in inputs.iter().enumerate() {
        if let Some((reason, denied)) =
            check_privileged_fields(lane, e.launch.as_ref(), &e.prep, e.icon.as_deref())
        {
            tracing::warn!(
                provider,
                index = i,
                title = %e.title,
                reason = %reason,
                "library reconcile refused"
            );
            return denied;
        }
    }
    // A launcher this host cannot open is a fact about the box; drop that tile, not the set.
    for (title, value) in crate::library::sanitize_launcher_entries(&mut inputs) {
        tracing::warn!(
            provider,
            launcher = %value,
            title = %title,
            "library reconcile: dropped a launcher tile this host cannot open — the rest of the \
             payload still syncs. Install the launcher, or turn the tile off in the plugin's config"
        );
    }
    // One warn for the batch: a root mismatch misses every cover; per-entry would flood the log.
    let mut dropped_art = 0usize;
    let mut first_dropped: Option<(String, &'static str, String)> = None;
    for e in inputs.iter_mut() {
        for (field, value) in crate::library::sanitize_art_paths(&mut e.art) {
            dropped_art += 1;
            first_dropped.get_or_insert_with(|| (e.title.clone(), field, value));
        }
    }
    if let Some((title, field, path)) = first_dropped {
        tracing::warn!(
            provider,
            dropped = dropped_art,
            example_title = %title,
            example_field = field,
            example_path = %path,
            "library reconcile: dropped local art the proxy may not serve — these entries still \
             sync, but their covers will be blank. The path must be an image file (jpg/png/webp/\
             gif/bmp/ico/tga) inside an allowed art root; set PUNKTFUNK_LIBRARY_ART_ROOTS if this \
             library's art lives outside the defaults"
        );
    }
    match crate::library::reconcile_provider(&provider, store.as_deref(), inputs) {
        Ok(crate::library::MutateOutcome::Done(entries)) => {
            tracing::info!(
                provider,
                store = store.as_deref().unwrap_or("-"),
                count = entries.len(),
                "library provider reconciled"
            );
            Json(entries).into_response()
        }
        Ok(crate::library::MutateOutcome::StoreClaimed { store, provider }) => api_error(
            StatusCode::CONFLICT,
            &format!("store `{store}` is already claimed by provider `{provider}`"),
        ),
        Ok(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The library wasn't updated — the host hit an unexpected state",
        ),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Delete a provider's entries
///
/// Everything owned by `{provider}`, for plugin uninstall. Emits `library.changed`
/// when anything was removed.
#[utoipa::path(
    delete,
    path = "/library/provider/{provider}",
    tag = "library",
    operation_id = "deleteProviderEntries",
    params(("provider" = String, Path, description = "The provider id")),
    responses(
        (status = OK, description = "How many entries were removed", body = ProviderRemoved),
        (status = BAD_REQUEST, description = "Invalid provider id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn delete_provider_entries(
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    Path(provider): Path<String>,
) -> Response {
    if !crate::mgmt::auth::plugin_owns(who.as_ref().map(|e| &e.0), &provider) {
        return api_error(StatusCode::FORBIDDEN, "a plugin may only write its own id");
    }
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    match crate::library::delete_provider(&provider) {
        Ok(removed) => {
            if removed > 0 {
                tracing::info!(provider, removed, "library provider entries removed");
            }
            // Entries are gone; drop the liveness lease so a missing provider cannot hold it open.
            crate::runstate::forget(&provider);
            Json(ProviderRemoved { removed }).into_response()
        }
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct RunningTitle {
    /// Same key the reconcile payload uses.
    pub external_id: String,
    /// Pid the provider started, when it knows one. Re-resolved and pinned to start time
    /// before any signal; a stale or recycled pid contributes nothing.
    #[serde(default)]
    pub pid: Option<u32>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ProviderRunningInput {
    /// Complete running set, not a delta: anything absent is reported as stopped.
    #[serde(default)]
    pub running: Vec<RunningTitle>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ProviderRunningAccepted {
    matched: usize,
    /// Ignored because no such entry exists (report raced a reconcile).
    unknown: usize,
    /// Seconds this report stays authoritative without being restated.
    ttl_s: u64,
}

/// Report which of a provider's titles are running.
///
/// Live counterpart to reconcile `detect` hints: the body is the complete running set
/// ([`crate::runstate`]). Missed events and plugin restarts self-correct on the next PUT.
///
/// Authoritative for [`crate::runstate::REPORT_TTL`] unless restated; a dead plugin then
/// falls back to process scanning. Re-report on change and on a timer inside the window.
/// Titles the provider does not currently publish count as `unknown`, not an error — a
/// report may race its own reconcile.
#[utoipa::path(
    put,
    path = "/library/provider/{provider}/running",
    tag = "library",
    operation_id = "reportProviderRunning",
    params(("provider" = String, Path, description = "The provider id ([a-z0-9._-], `manual` reserved)")),
    request_body = ProviderRunningInput,
    responses(
        (status = OK, description = "The report was accepted", body = ProviderRunningAccepted),
        (status = BAD_REQUEST, description = "Invalid provider id or payload", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "A plugin reported another plugin's titles", body = ApiError),
    )
)]
pub(crate) async fn report_provider_running(
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    Path(provider): Path<String>,
    ApiJson(input): ApiJson<ProviderRunningInput>,
) -> Response {
    // Another plugin's report would end or prolong that provider's game lease.
    if !crate::mgmt::auth::plugin_owns(who.as_ref().map(|e| &e.0), &provider) {
        return api_error(
            StatusCode::FORBIDDEN,
            "a plugin may only report its own titles",
        );
    }
    if let Err(e) = crate::library::validate_provider_name(&provider) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    // Map the provider's `external_id`s to catalog library ids. Only published entries
    // resolve, so a report cannot name a title it does not own.
    let mine: Vec<(String, String)> = crate::library::load_custom()
        .into_iter()
        .filter(|e| e.provider.as_deref() == Some(provider.as_str()))
        .filter_map(|e| {
            let external = e.external_id.clone()?;
            Some((external, crate::library::library_id_for(&e)))
        })
        .collect();
    let owned: std::collections::HashSet<String> = mine.iter().map(|(_, id)| id.clone()).collect();

    let mut running = std::collections::HashMap::new();
    let mut unknown = 0usize;
    for t in &input.running {
        match mine.iter().find(|(external, _)| *external == t.external_id) {
            Some((_, id)) => {
                running.insert(id.clone(), t.pid);
            }
            None => unknown += 1,
        }
    }
    let matched = running.len();
    tracing::debug!(
        provider,
        owned = owned.len(),
        matched,
        unknown,
        "provider liveness report"
    );
    crate::runstate::report(&provider, owned, running);
    Json(ProviderRunningAccepted {
        matched,
        unknown,
        ttl_s: crate::runstate::REPORT_TTL.as_secs(),
    })
    .into_response()
}

/// Stream one cover-art image for a library entry.
///
/// Resolves `kind` (`portrait` | `hero` | `logo` | `header`) for a catalog id and returns the
/// bytes: a launcher's cover cache on the host disk, or a remote URL the host fetches once on
/// the first miss and then serves from its own store. Unknown id or kind is 404 so the client
/// can try the next candidate, and so is a URL the fetch refused — `GET /library` advertises
/// that one verbatim again. The response carries `Cache-Control` and an `ETag` of the bytes; a
/// request whose `If-None-Match` names that tag gets 304 with no body.
#[utoipa::path(
    get,
    path = "/library/art/{id}/{kind}",
    tag = "library",
    operation_id = "getLibraryArt",
    params(
        ("id" = String, Path, description = "The store-qualified library id, e.g. `steam:570`"),
        ("kind" = String, Path, description = "`portrait` | `hero` | `logo` | `header`"),
        ("If-None-Match" = Option<String>, Header, description = "An `ETag` from an earlier response"),
    ),
    responses(
        (status = OK, description = "Image bytes", content_type = "image/jpeg"),
        (status = NOT_MODIFIED, description = "The tag in `If-None-Match` is current"),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = NOT_FOUND, description = "No art of that kind for that id", body = ApiError),
    )
)]
pub(crate) async fn get_library_art(
    Path((id, kind)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(kind) = crate::library::ArtKind::parse(&kind) else {
        return api_error(StatusCode::NOT_FOUND, "unknown art kind");
    };
    // Any catalog id (manual, provider, claimed-store) serves its local file from here.
    // The proxy does not know which store the id belongs to.
    let stored = {
        let id = id.clone();
        tokio::task::spawn_blocking(move || crate::library::library_art_bytes(&id, kind)).await
    };
    if let Ok(Some((bytes, ctype))) = stored {
        // The tag is the bytes' hash: a replaced cover misses, an unchanged one revalidates
        // for free. A day of freshness keeps a shelf revisit off the network entirely.
        let etag = format!("\"{}\"", hex::encode(&Sha256::digest(&bytes)[..16]));
        let cache = [
            (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
            (header::ETAG, etag.clone()),
        ];
        let sent = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok());
        if not_modified(sent, &etag) {
            return (StatusCode::NOT_MODIFIED, cache).into_response();
        }
        let [cc, et] = cache;
        return ([cc, et, (header::CONTENT_TYPE, ctype)], bytes).into_response();
    }
    api_error(StatusCode::NOT_FOUND, "no art of that kind for this title")
}

/// Whether an `If-None-Match` value names `etag`: a list, `*`, or a weak `W/` form of it.
fn not_modified(if_none_match: Option<&str>, etag: &str) -> bool {
    if_none_match.is_some_and(|v| {
        v.split(',')
            .map(|t| t.trim().trim_start_matches("W/"))
            .any(|t| t == "*" || t == etag)
    })
}

#[cfg(test)]
mod tests {
    use super::not_modified;

    #[test]
    fn if_none_match_forms() {
        let tag = "\"abc\"";
        assert!(not_modified(Some("\"abc\""), tag));
        assert!(not_modified(Some("\"x\", \"abc\""), tag));
        assert!(not_modified(Some("W/\"abc\""), tag));
        assert!(not_modified(Some("*"), tag));
        assert!(!not_modified(Some("\"abd\""), tag));
        assert!(
            !not_modified(Some("abc"), tag),
            "an unquoted tag is not ours"
        );
        assert!(!not_modified(None, tag));
    }
}
