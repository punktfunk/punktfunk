//! `/plugin-access`: a plugin asks for a folder, the operator answers.
//!
//! The request lanes take a per-plugin token — the shared runner token carries no identity
//! and gets 403 — and only ever touch the caller's own rows. The overview and the decision
//! are admin-only, enforced by the lane allowlist rather than a check here. Every store
//! mutation emits `plugins.changed` so the console re-reads.

use super::shared::*;
use crate::events::{emit, EventKind};
use crate::plugins::access::{AskedPath, Decision, Mutation, PluginAccessSnapshot};
use axum::Extension;

/// How long a decision waits for a folder's ACL before it answers. A large Windows tree takes
/// minutes and the console's server gives a request two.
const DECISION_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Plugin-authored text: it lands on the console as a caption, so controls and bidi marks go,
/// as in a device name, and it is capped like a catalog string.
fn sanitize_reason(s: &str) -> Option<String> {
    let out: String = s
        .chars()
        .filter(|&c| !c.is_control() && !crate::native_pairing::is_spoofy_char(c))
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
    /// `true` asks for the folder and the files directly in it, nothing below. On Windows this
    /// lands at once where a whole folder rewrites every file in its tree. Ignored with `write`.
    #[serde(default)]
    pub files: bool,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct AccessRequest {
    #[serde(default)]
    pub paths: Vec<AccessPathRequest>,
    /// Catalog ids of emulators the plugin wants installed (`pcsx2`). Each answers with the
    /// folder the install lands in; the operator's yes installs it, then grants that folder.
    #[serde(default)]
    pub emulators: Vec<String>,
    /// libretro cores the plugin wants in RetroArch (`snes9x`). Each answers with RetroArch's
    /// cores folder; `refused:no_retroarch` without one on this host.
    #[serde(default)]
    pub cores: Vec<String>,
    /// Ask to read and restore emulator saves. One yes covers every emulator on this host; the
    /// answer names the folder the grant lands on.
    #[serde(default)]
    pub saves: bool,
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
    let paths: Vec<AskedPath> = req
        .paths
        .iter()
        .map(|p| AskedPath {
            path: p.path.clone(),
            write: p.write,
            files: p.files,
        })
        .collect();
    let mut changed = false;
    let mut outcomes = match st.access.request_paths(&id, &paths, reason.clone()) {
        Ok(m) => {
            changed |= m.changed;
            m.value
        }
        Err(e) => return store_err(e, "record the access request"),
    };
    for emulator in &req.emulators {
        let (path, _) = crate::emulators::grant_target(emulator);
        let refused = |rule: &str| crate::plugins::access::RequestOutcome {
            path: emulator.clone(),
            outcome: format!("refused:{rule}"),
        };
        let outcome = match crate::emulators::offered(emulator) {
            Ok(true) => match st
                .access
                .request_emulator(&id, emulator, &path, reason.clone())
            {
                Ok(m) => {
                    changed |= m.changed;
                    m.value
                }
                Err(e) => return store_err(e, "record the emulator request"),
            },
            Ok(false) => refused("not_offered"),
            Err(_) => refused("not_in_catalog"),
        };
        outcomes.push(outcome);
    }
    if !req.cores.is_empty() {
        let cores_dir = crate::emulators::cores_dir().ok().flatten();
        for core in &req.cores {
            let refused = |rule: &str| crate::plugins::access::RequestOutcome {
                path: core.clone(),
                outcome: format!("refused:{rule}"),
            };
            let outcome = if !crate::emulators::valid_core(core) {
                refused("bad_core")
            } else if let Some(dir) = cores_dir
                .as_ref()
                .filter(|d| crate::emulators::has_core(d, core))
            {
                // A sandbox that can't read the folder sees no cores; one already there needs
                // only the folder.
                let folder = [(dir.to_string_lossy().into_owned(), false)];
                match st.access.request(&id, &folder, reason.clone()) {
                    Ok(m) => {
                        changed |= m.changed;
                        m.value
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| refused("not_directory"))
                    }
                    Err(e) => return store_err(e, "record the core request"),
                }
            } else if let Some(dir) = &cores_dir {
                match st.access.request_core(&id, core, dir, reason.clone()) {
                    Ok(m) => {
                        changed |= m.changed;
                        m.value
                    }
                    Err(e) => return store_err(e, "record the core request"),
                }
            } else {
                refused("no_retroarch")
            };
            outcomes.push(outcome);
        }
    }
    if req.saves {
        let dir = crate::emulators::saves_grant();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return store_err(e, "make the saves folder");
        }
        match st.access.request_saves(&id, &dir, reason.clone()) {
            Ok(m) => {
                changed |= m.changed;
                outcomes.push(m.value);
            }
            Err(e) => return store_err(e, "record the saves request"),
        }
    }
    if changed {
        // A link the plugin asked through is a new root: the runner's ACE for it lands now, not
        // at the next minute's converge.
        tokio::task::spawn_blocking(crate::plugins::converge_after_decision);
        emit(EventKind::PluginsChanged { id });
    }
    Json(
        outcomes
            .into_iter()
            .map(|o| AccessPathOutcome {
                path: o.path,
                outcome: o.outcome,
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
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
/// A folder that takes longer answers 202 and lists its walk until the record lands.
#[utoipa::path(
    post,
    path = "/plugin-access/{plugin}/decide",
    tag = "plugin-access",
    operation_id = "decidePluginAccess",
    params(("plugin" = String, Path, description = "The plugin id")),
    request_body = DecideRequest,
    responses(
        (status = OK, description = "The plugin's access state after the decision", body = crate::plugins::access::PluginAccessSnapshot),
        (status = ACCEPTED, description = "The folder's access is still changing; `walks` lists it", body = crate::plugins::access::PluginAccessSnapshot),
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
    // A store call may change a folder's ACL, which walks its whole tree on Windows.
    let store = |f: Box<dyn FnOnce(&MgmtState) -> std::io::Result<_> + Send>| {
        let st = Arc::clone(&st);
        blocking("plugin access", move || f(&st))
    };
    let work = match (decision, req.write) {
        (Decision::Allow, Some(write)) => {
            let (plugin, path, form) = (plugin.clone(), req.path.clone(), req.form.clone());
            store(Box::new(move |st| {
                let path = std::path::Path::new(&path);
                let dir = match path.parent() {
                    Some(parent) if path.is_file() => parent,
                    _ => path,
                };
                match &form {
                    Some(form) => st.access.hand(&plugin, dir, write, form),
                    None => st.access.grant(&plugin, dir, write, "console"),
                }
                .and_then(|_| st.access.snapshot_for(&plugin))
                .map(|value| Mutation {
                    value,
                    changed: true,
                })
            }))
        }
        (Decision::Allow, None) => {
            // An emulator request: the install lands first, so the grant has a folder and a
            // failed download leaves the row for another try.
            if let Some(emulator) = st.access.pending_emulator(&plugin, &req.path) {
                let target = emulator.clone();
                match blocking("emulator manager", move || {
                    crate::emulators::install(&target)
                })
                .await
                {
                    Ok(Ok(_)) => emit(EventKind::EmulatorsChanged { id: emulator }),
                    Ok(Err(e)) => {
                        return super::emulators::hermir_err(&e, "The emulator didn't install");
                    }
                    Err(resp) => return resp,
                }
            }
            // Core rows share RetroArch's folder: one yes installs every core asked for there.
            let cores = st.access.pending_cores(&plugin, &req.path);
            if !cores.is_empty() {
                let batch = cores.clone();
                let done = blocking("emulator manager", move || {
                    batch
                        .iter()
                        .map(|c| crate::emulators::install_core(c).map(|_| ()))
                        .collect::<Result<Vec<()>, _>>()
                })
                .await;
                match done {
                    Ok(Ok(_)) => emit(EventKind::EmulatorsChanged {
                        id: "retroarch".into(),
                    }),
                    Ok(Err(e)) => {
                        return super::emulators::hermir_err(&e, "The core didn't install");
                    }
                    Err(resp) => return resp,
                }
            }
            let (plugin, path) = (plugin.clone(), req.path.clone());
            store(Box::new(move |st| {
                st.access.decide(&plugin, &path, Decision::Allow, "console")
            }))
        }
        (decision, _) => {
            let (plugin, path) = (plugin.clone(), req.path.clone());
            store(Box::new(move |st| {
                st.access.decide(&plugin, &path, decision, "console")
            }))
        }
    };
    settle(&st, plugin, "record the decision", work).await
}

/// Answer with `work`'s outcome, or with a 202 listing the walk once [`DECISION_WAIT`] is up.
/// `work` runs to its end either way, and what it changed reaches the runner and the console.
async fn settle<F>(st: &Arc<MgmtState>, plugin: String, what: &'static str, work: F) -> Response
where
    F: std::future::Future<
            Output = Result<std::io::Result<Mutation<PluginAccessSnapshot>>, Response>,
        > + Send
        + 'static,
{
    let (answer, answered) = tokio::sync::oneshot::channel();
    let id = plugin.clone();
    tokio::spawn(async move {
        let outcome = work.await;
        if matches!(&outcome, Ok(Ok(m)) if m.changed) {
            // Not awaited: a changed root restarts the runner, which this answer need not wait on.
            tokio::task::spawn_blocking(crate::plugins::converge_after_decision);
            emit(EventKind::PluginsChanged { id: id.clone() });
        }
        if let Err(Ok(Err(e))) = answer.send(outcome) {
            tracing::warn!(plugin = %id, error = %e, "folder decision not recorded");
        }
    });
    let Ok(outcome) = tokio::time::timeout(DECISION_WAIT, answered).await else {
        return match st.access.snapshot_for(&plugin) {
            Ok(snap) => (StatusCode::ACCEPTED, Json(snap)).into_response(),
            Err(e) => store_err(e, "read the access requests"),
        };
    };
    match outcome {
        Ok(Ok(Ok(m))) => Json(m.value).into_response(),
        Ok(Ok(Err(e))) => store_err(e, what),
        Ok(Err(resp)) => resp,
        Err(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The plugin access worker stopped responding.",
        ),
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
        (status = ACCEPTED, description = "The folder's access is still changing; `walks` lists it", body = crate::plugins::access::PluginAccessSnapshot),
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
    // A grant only forms held goes with them, and its ACL with it: off the async workers.
    let work = {
        let (st, plugin) = (Arc::clone(&st), plugin.clone());
        blocking("plugin access", move || {
            let changed = st.access.release(&plugin, &req.form, &req.keep)?.changed;
            let value = st.access.snapshot_for(&plugin)?;
            Ok(Mutation { value, changed })
        })
    };
    settle(&st, plugin, "record the release", work).await
}
