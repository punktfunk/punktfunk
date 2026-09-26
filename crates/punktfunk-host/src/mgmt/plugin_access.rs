//! `/plugin-access`: a plugin asks for a folder, the operator answers.
//!
//! The request lanes take a per-plugin token — the shared runner token carries no identity
//! and gets 403 — and only ever touch the caller's own rows. The overview and the decision
//! are admin-only, enforced by the lane allowlist rather than a check here. Every store
//! mutation emits `plugins.changed` so the console re-reads.

use super::shared::*;
use crate::events::{emit, EventKind};
use crate::plugins::access::Decision;
use axum::Extension;

/// Plugin-authored text: it lands on the console as a caption, so controls go and it is
/// capped like a catalog string.
fn sanitize_reason(s: &str) -> Option<String> {
    let out: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect::<String>()
        .trim()
        .to_string();
    (!out.is_empty()).then_some(out)
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AccessPathRequest {
    /// The directory the plugin wants, absolute on the host.
    pub path: String,
    /// `true` asks for write access too; a grant is read-only unless the operator allows it.
    #[serde(default)]
    pub write: bool,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AccessRequest {
    pub paths: Vec<AccessPathRequest>,
    /// Why the plugin wants it, in the plugin's words. Optional, sanitized, ≤120 chars.
    #[serde(default)]
    pub reason: Option<String>,
}

/// One path's answer: `granted`, `pending`, `denied`, or `refused:<rule>`.
#[derive(Serialize, ToSchema)]
pub(crate) struct AccessPathOutcome {
    pub path: String,
    pub outcome: String,
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DecisionInput {
    Allow,
    Deny,
    Forget,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct DecideRequest {
    /// The path as it appears in the request or grant list.
    pub path: String,
    pub decision: DecisionInput,
    /// With `allow`: grant the path directly, pending request or not, read-write when true.
    /// A file grants its folder. The same refusals apply as to a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<bool>,
    /// With `allow` and `write`: the plugin form handing the path over (`config`,
    /// `game:<entry id>`). Its grant goes once every form that handed it lets go.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ReleaseRequest {
    /// The plugin form letting go, as named when it handed the paths over.
    pub form: String,
    /// Every path the form still hands over, as saved.
    pub keep: Vec<String>,
}

/// A form name is a key the console makes up, never a path; the grants file stores it.
fn valid_form(form: &str) -> bool {
    !form.is_empty() && form.len() <= 300 && !form.chars().any(char::is_control)
}

const FORM_INVALID: &str =
    "the form name must be 1 to 300 characters, none of them control characters";

fn store_err(e: std::io::Error, what: &str) -> Response {
    match e.kind() {
        std::io::ErrorKind::NotFound => api_error(
            StatusCode::NOT_FOUND,
            "there is no pending request for that path",
        ),
        std::io::ErrorKind::InvalidInput => api_error(StatusCode::BAD_REQUEST, &e.to_string()),
        _ => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't {what} — {e}"),
        ),
    }
}

/// The caller's plugin id; `None` is the shared runner token, which carries no identity.
fn caller_id(who: Option<Extension<crate::mgmt::auth::PluginIdentity>>) -> Option<String> {
    who.map(|Extension(w)| w.0)
}

fn no_identity() -> Response {
    api_error(
        StatusCode::FORBIDDEN,
        "folder access requests need a plugin's own token — the shared runner token carries no identity",
    )
}

/// Ask for folders this plugin cannot reach
///
/// Each path is validated against the real filesystem before it becomes a row the operator
/// sees: `granted` (already reachable), `pending`, `denied` (a sticky no), or `refused:<rule>`.
#[utoipa::path(
    post,
    path = "/plugin-access/requests",
    tag = "plugin-access",
    operation_id = "requestPluginAccess",
    request_body = AccessRequest,
    responses(
        (status = OK, description = "Per-path outcomes", body = [AccessPathOutcome]),
        (status = FORBIDDEN, description = "The shared runner token has no plugin identity", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The request could not be recorded", body = ApiError),
    )
)]
pub(crate) async fn request_plugin_access(
    State(st): State<Arc<MgmtState>>,
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
    ApiJson(req): ApiJson<AccessRequest>,
) -> Response {
    let Some(id) = caller_id(who) else {
        return no_identity();
    };
    let reason = req.reason.as_deref().and_then(sanitize_reason);
    let paths: Vec<(String, bool)> = req
        .paths
        .iter()
        .map(|p| (p.path.clone(), p.write))
        .collect();
    match st.access.request(&id, &paths, reason) {
        Ok(m) => {
            if m.changed {
                emit(EventKind::PluginsChanged { id });
            }
            Json(
                m.value
                    .into_iter()
                    .map(|o| AccessPathOutcome {
                        path: o.path,
                        outcome: o.outcome,
                    })
                    .collect::<Vec<_>>(),
            )
            .into_response()
        }
        Err(e) => store_err(e, "record the access request"),
    }
}

/// List this plugin's own access rows
///
/// Grants, pending requests, and denials for the calling plugin only.
#[utoipa::path(
    get,
    path = "/plugin-access/requests",
    tag = "plugin-access",
    operation_id = "getPluginAccessRequests",
    responses(
        (status = OK, description = "The calling plugin's access state", body = crate::plugins::access::PluginAccessSnapshot),
        (status = FORBIDDEN, description = "The shared runner token has no plugin identity", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_plugin_access_requests(
    State(st): State<Arc<MgmtState>>,
    who: Option<Extension<crate::mgmt::auth::PluginIdentity>>,
) -> Response {
    let Some(id) = caller_id(who) else {
        return no_identity();
    };
    match st.access.snapshot_for(&id) {
        Ok(snap) => Json(snap).into_response(),
        Err(e) => store_err(e, "read the access requests"),
    }
}

/// List every plugin's folder access
///
/// Admin lane only: grants, pending requests, and denials across all plugins.
#[utoipa::path(
    get,
    path = "/plugin-access",
    tag = "plugin-access",
    operation_id = "getPluginAccess",
    responses(
        (status = OK, description = "All plugins' access state", body = [crate::plugins::access::PluginAccessSnapshot]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not reachable on the plugin or device lane", body = ApiError),
    )
)]
pub(crate) async fn get_plugin_access(State(st): State<Arc<MgmtState>>) -> Response {
    match st.access.snapshot() {
        Ok(snaps) => Json(snaps).into_response(),
        Err(e) => store_err(e, "read plugin access"),
    }
}

/// Grant, deny, or forget an access request
///
/// `allow` turns a pending request into a grant (the platform ACL lands first, so a failed
/// grant stores nothing), or with `write` set grants a path the operator handed over, on
/// behalf of `form` when named; `deny` remembers the no; `forget` removes a grant or denial.
#[utoipa::path(
    post,
    path = "/plugin-access/{plugin}/decide",
    tag = "plugin-access",
    operation_id = "decidePluginAccess",
    params(("plugin" = String, Path, description = "The plugin id")),
    request_body = DecideRequest,
    responses(
        (status = OK, description = "The plugin's access state after the decision", body = crate::plugins::access::PluginAccessSnapshot),
        (status = BAD_REQUEST, description = "Invalid path or a limit reached", body = ApiError),
        (status = NOT_FOUND, description = "No pending request for that path", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not reachable on the plugin or device lane", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The decision could not be recorded", body = ApiError),
    )
)]
pub(crate) async fn decide_plugin_access(
    State(st): State<Arc<MgmtState>>,
    Path(plugin): Path<String>,
    ApiJson(req): ApiJson<DecideRequest>,
) -> Response {
    let decision = match req.decision {
        DecisionInput::Allow => Decision::Allow,
        DecisionInput::Deny => Decision::Deny,
        DecisionInput::Forget => Decision::Forget,
    };
    if req.form.as_deref().is_some_and(|f| !valid_form(f)) {
        return api_error(StatusCode::BAD_REQUEST, FORM_INVALID);
    }
    let outcome = match (decision, req.write) {
        (Decision::Allow, Some(write)) => {
            let path = std::path::Path::new(&req.path);
            let dir = match path.parent() {
                Some(parent) if path.is_file() => parent,
                _ => path,
            };
            match &req.form {
                Some(form) => st.access.hand(&plugin, dir, write, form),
                None => st.access.grant(&plugin, dir, write, "console"),
            }
            .and_then(|_| st.access.snapshot_for(&plugin))
            .map(|value| crate::plugins::access::Mutation {
                value,
                changed: true,
            })
        }
        (decision, _) => st.access.decide(&plugin, &req.path, decision, "console"),
    };
    match outcome {
        Ok(m) => {
            // Not awaited: a changed root restarts the runner, which this answer need not wait on.
            tokio::task::spawn_blocking(crate::plugins::converge_runner_roots);
            emit(EventKind::PluginsChanged { id: plugin });
            Json(m.value).into_response()
        }
        Err(e) => store_err(e, "record the decision"),
    }
}

/// Let a plugin form go of the paths it no longer hands over
///
/// `keep` is every path the form still holds. Each grant the form holds outside it loses the
/// form, and a grant only forms made goes with its last one. The operator's own grants stay.
#[utoipa::path(
    post,
    path = "/plugin-access/{plugin}/release",
    tag = "plugin-access",
    operation_id = "releasePluginAccess",
    params(("plugin" = String, Path, description = "The plugin id")),
    request_body = ReleaseRequest,
    responses(
        (status = OK, description = "The plugin's access state after the release", body = crate::plugins::access::PluginAccessSnapshot),
        (status = BAD_REQUEST, description = "Invalid form name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not reachable on the plugin or device lane", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The release could not be recorded", body = ApiError),
    )
)]
pub(crate) async fn release_plugin_access(
    State(st): State<Arc<MgmtState>>,
    Path(plugin): Path<String>,
    ApiJson(req): ApiJson<ReleaseRequest>,
) -> Response {
    if !valid_form(&req.form) {
        return api_error(StatusCode::BAD_REQUEST, FORM_INVALID);
    }
    match st
        .access
        .release(&plugin, &req.form, &req.keep)
        .and_then(|m| Ok((m.changed, st.access.snapshot_for(&plugin)?)))
    {
        Ok((changed, snap)) => {
            if changed {
                tokio::task::spawn_blocking(crate::plugins::converge_runner_roots);
                emit(EventKind::PluginsChanged { id: plugin });
            }
            Json(snap).into_response()
        }
        Err(e) => store_err(e, "record the release"),
    }
}
