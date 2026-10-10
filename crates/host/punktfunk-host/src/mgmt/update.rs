//! `/api/v1/update/*` — host update-check and apply.
//!
//! Admin lane only: denied to the plugin token (`auth::plugin_may_access`) and
//! absent from the paired-cert allowlist. An update trigger is operator business.
//!
//! Pin the wire types at [`UpdateStatus`]. Evidence:
//! `design/host-update-from-web-console.md`.

use super::shared::*;
use crate::update::{self, detect};

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateManifestInfo {
    pub version: String,
    /// Unix seconds; monotonic per channel.
    pub serial: u64,
    /// RFC-3339; display only, never compared.
    pub published_at: String,
    /// Forge-pinned by the manifest validator.
    pub notes_url: String,
    /// Last verified manifest older than 45 days.
    pub stale: bool,
}

/// In-process apply, or a leftover installer that has not resolved yet.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateJobInfo {
    pub target_version: String,
    /// `downloading` | `verifying` | `applying` | `restarting`.
    pub stage: String,
    pub received_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    pub started_unix: u64,
}

/// Last apply outcome. Survives the host's own restart.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateResultInfo {
    pub ok: bool,
    pub from: String,
    pub to: String,
    pub finished_unix: u64,
    /// Failed stage; absent on success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// Applied; activates on the next reboot (rpm-ostree).
    #[serde(default)]
    pub staged: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct UpdateStatus {
    /// `windows-installer` | `sysext` | `rpm-ostree` | `apt` | `dnf` | `pacman` |
    /// `steamos-source` | `nix` | `source`.
    pub install_kind: String,
    /// `stable` | `canary`.
    pub channel: String,
    pub current_version: String,
    /// `notify` (show the command) | `full` (one-click) | `staged` (apply + reboot).
    pub apply: String,
    /// Copy-paste update command for this install kind.
    pub channel_hint: String,
    /// `PUNKTFUNK_UPDATE_CHECK=0`.
    pub check_disabled: bool,
    /// Newer than `current_version` on this channel. Unparseable pairs never flag.
    pub available: bool,
    pub manifest: Option<UpdateManifestInfo>,
    /// Last successful check (unix seconds).
    pub last_checked_unix: Option<u64>,
    /// Last check failure, verbatim.
    pub last_error: Option<String>,
    /// Feed 404 for this channel: nothing published yet, not a check failure.
    /// Mutually exclusive with `last_error`. Never set once a manifest has been
    /// seen — a feed that then 404s is an error.
    pub not_published: bool,
    /// One-click apply is possible but not opted in — command to run
    /// (Linux: join `punktfunk-update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opt_in_hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<UpdateJobInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_result: Option<UpdateResultInfo>,
}

fn status_from(snap: update::Snapshot) -> UpdateStatus {
    let (kind, channel) = detect::detect();
    let current = crate::version::get();
    let stale = snap.stale();
    // Apply refuses without a verified manifest, so an offer needs one too.
    let available = snap.checked.as_ref().is_some_and(|c| {
        update::offers_update(kind, channel, current, &c.manifest, snap.source_behind)
    });
    // Leftover installer intent still shows as `restarting` so the console
    // poller does not see a gap after the previous host died.
    let job = snap
        .job
        .as_ref()
        .map(|j| UpdateJobInfo {
            target_version: j.target_version.clone(),
            stage: j.stage.into(),
            received_bytes: j.received_bytes,
            total_bytes: j.total_bytes,
            started_unix: j.started_unix,
        })
        .or_else(|| {
            snap.applying_from_intent().map(|i| UpdateJobInfo {
                target_version: i.to,
                stage: "restarting".into(),
                received_bytes: 0,
                total_bytes: None,
                started_unix: i.started_unix,
            })
        });
    UpdateStatus {
        install_kind: kind.as_str().into(),
        channel: channel.as_str().into(),
        current_version: current.into(),
        apply: update::apply_support().into(),
        channel_hint: detect::channel_hint(kind),
        check_disabled: update::check_disabled(),
        available,
        manifest: snap.checked.as_ref().map(|c| UpdateManifestInfo {
            version: c.manifest.version.clone(),
            serial: c.manifest.serial,
            published_at: c.manifest.published_at.clone(),
            notes_url: c.manifest.notes_url.clone(),
            stale,
        }),
        last_checked_unix: snap.checked.as_ref().map(|c| c.fetched_unix),
        last_error: snap.last_error,
        not_published: snap.not_published,
        opt_in_hint: update::opt_in_hint(),
        job,
        last_result: snap.last_result.as_ref().map(|r| UpdateResultInfo {
            ok: r.ok,
            from: r.from.clone(),
            to: r.to.clone(),
            finished_unix: r.finished_unix,
            stage: r.stage.clone(),
            error: r.error.clone(),
            log_path: r.log_path.clone(),
            staged: r.staged,
        }),
    }
}

/// Cached update-check state.
///
/// A cache older than 6 h may kick a background refresh. The response never
/// blocks on the network.
#[utoipa::path(
    get,
    path = "/update/status",
    tag = "update",
    operation_id = "getUpdateStatus",
    responses(
        (status = OK, description = "Current update-check state", body = UpdateStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_update_status() -> Json<UpdateStatus> {
    Json(status_from(update::snapshot_and_maybe_refresh()))
}

/// Force a manifest fetch and verification.
///
/// One forced check per 30 s. `last_error` is a failed check; `not_published`
/// is an empty channel, not a failure.
#[utoipa::path(
    post,
    path = "/update/check",
    tag = "update",
    operation_id = "forceUpdateCheck",
    responses(
        (status = OK, description = "Refreshed update-check state (`last_error` carries a failed check; `not_published` an empty channel, which is not one)", body = UpdateStatus),
        (status = CONFLICT, description = "Update checks are disabled on this host", body = ApiError),
        (status = TOO_MANY_REQUESTS, description = "A forced check ran less than 30 s ago", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn force_update_check() -> Response {
    match update::force_check().await {
        Ok(snap) => Json(status_from(snap)).into_response(),
        Err(update::ForceError::Disabled) => api_error(
            StatusCode::CONFLICT,
            "update checks are disabled on this host (PUNKTFUNK_UPDATE_CHECK=0)",
        ),
        Err(update::ForceError::TooSoon) => api_error(
            StatusCode::TOO_MANY_REQUESTS,
            "a forced update check ran less than 30 s ago — try again shortly",
        ),
    }
}

#[derive(Default, Deserialize, ToSchema)]
pub(crate) struct ApplyRequest {
    /// Apply even with a live stream (the stream drops when the host restarts).
    #[serde(default)]
    pub force: bool,
}

/// Start a one-click update
///
/// Only for install kinds that support it. No version or URL in the body — the host
/// installs the verified manifest. Poll `GET /update/status` (`job`); after restart,
/// the outcome is `last_result`.
#[utoipa::path(
    post,
    path = "/update/apply",
    tag = "update",
    operation_id = "applyUpdate",
    request_body = ApplyRequest,
    responses(
        (status = ACCEPTED, description = "Apply started — poll `GET /update/status`", body = UpdateStatus),
        (status = CONFLICT, description = "Refused: unsupported install kind, apply disabled (PUNKTFUNK_UPDATE_APPLY=0), a job already running, an active streaming session without `force`, or nothing newer to apply", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn apply_update(
    State(st): State<Arc<MgmtState>>,
    // Axum has no optional-body extractor; send `{}` for defaults.
    ApiJson(req): ApiJson<ApplyRequest>,
) -> Response {
    // Either streaming plane counts as an active session (`get_status` same).
    let session_active = st.app.streaming.load(std::sync::atomic::Ordering::SeqCst)
        || !crate::session_status::snapshot().is_empty();

    match update::start_apply(req.force, session_active) {
        Ok(()) => {
            let snap = update::snapshot_and_maybe_refresh();
            (StatusCode::ACCEPTED, Json(status_from(snap))).into_response()
        }
        Err(update::ApplyError::Unsupported) => api_error(
            StatusCode::CONFLICT,
            "this install kind has no one-click apply — use the update command shown in status",
        ),
        Err(update::ApplyError::Disabled) => api_error(
            StatusCode::CONFLICT,
            "one-click apply is disabled on this host (PUNKTFUNK_UPDATE_APPLY=0)",
        ),
        Err(update::ApplyError::JobRunning) => api_error(
            StatusCode::CONFLICT,
            "an update is already being applied — poll GET /update/status",
        ),
        Err(update::ApplyError::SessionActive) => api_error(
            StatusCode::CONFLICT,
            "a streaming session is active — pass {\"force\": true} to update anyway (the stream will drop)",
        ),
        Err(update::ApplyError::NothingToApply) => api_error(
            StatusCode::CONFLICT,
            "no newer release is known for this channel — run a check first",
        ),
    }
}
