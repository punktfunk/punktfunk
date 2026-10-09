//! Client log bundles: paired devices POST uploads; the console lists, fetches, and deletes.
//! See `crate::client_logs` for why this is a file store.
//!
//! Upload is the one write a paired streaming cert may perform — write-only (the device
//! cannot read anything back, not even its own bundle), size-capped, quota-bounded per
//! device. List/fetch/delete stay on the loopback bearer: bundles can hold addresses and
//! host names, same lane as the host's own logs.

use super::auth::PairedDevice;
use super::shared::*;
use crate::client_logs::{ClientLogMeta, MAX_BUNDLE_BYTES};
use axum::body::Bytes;
use axum::Extension;

#[derive(Serialize, ToSchema)]
pub(crate) struct ClientLogUploaded {
    pub id: String,
}

/// Upload a client log bundle
///
/// A paired device posts plain text under its streaming cert — no bearer. Cap 1 MiB, newest
/// few per device kept. Write-only: uploading grants no read.
#[utoipa::path(
    post,
    path = "/client-logs",
    tag = "logs",
    operation_id = "clientLogsUpload",
    request_body(content = String, content_type = "text/plain", description = "The client's log text"),
    responses(
        (status = CREATED, description = "Bundle stored", body = ClientLogUploaded),
        (status = BAD_REQUEST, description = "No paired device behind the request", body = ApiError),
        (status = FORBIDDEN, description = "The device's access has expired (per-client access)", body = ApiError),
        (status = PAYLOAD_TOO_LARGE, description = "Bundle exceeds the size cap", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "Empty body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't store the log bundle", body = ApiError),
    )
)]
pub(crate) async fn client_logs_upload(
    State(st): State<Arc<MgmtState>>,
    device: Option<Extension<PairedDevice>>,
    body: Bytes,
) -> Response {
    // Auth admits this route for paired devices, by certificate or by device key, and for the
    // admin bearer. An upload with no device behind it has no owner to file it under.
    let Some(Extension(PairedDevice(fp))) = device else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "client log upload requires a paired device",
        );
    };
    if body.is_empty() {
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "empty log bundle");
    }
    if body.len() > MAX_BUNDLE_BYTES {
        return api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "log bundle exceeds the 1 MiB cap — send the tail",
        );
    }
    // The gate's `is_paired` is expiry-blind (right for roster GETs). This WRITE uses
    // `effective`: a lapsed guest must not keep writing to disk. No grant bit — a view-only
    // guest mid-session is who a debug bundle is wanted from.
    let now_unix = crate::clock::unix_secs();
    if st
        .native
        .as_ref()
        .and_then(|n| n.effective(&fp, now_unix))
        .is_none()
    {
        return api_error(
            StatusCode::FORBIDDEN,
            "this device's access has expired — ask the host's operator to approve it again",
        );
    }
    // Auth already proved membership; a race with unpair between the gate and here falls
    // back to the fingerprint prefix.
    let device_name = st
        .native
        .as_ref()
        .and_then(|n| {
            n.list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(&fp))
                .map(|c| c.name)
        })
        .unwrap_or_else(|| fp.chars().take(16).collect());
    match st.client_logs.save(&fp, &device_name, &body) {
        Ok(id) => {
            tracing::info!(
                device = %device_name,
                id = %id,
                bytes = body.len(),
                "client log bundle received — listed on the console's Logs page"
            );
            (StatusCode::CREATED, Json(ClientLogUploaded { id })).into_response()
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't store the log bundle — {e}"),
        ),
    }
}

/// List uploaded client log bundles
#[utoipa::path(
    get,
    path = "/client-logs",
    tag = "logs",
    operation_id = "clientLogsList",
    responses(
        (status = OK, description = "Stored bundles, newest first", body = [ClientLogMeta]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn client_logs_list(State(st): State<Arc<MgmtState>>) -> Json<Vec<ClientLogMeta>> {
    Json(st.client_logs.list())
}

/// Download a client log bundle
#[utoipa::path(
    get,
    path = "/client-logs/{id}",
    tag = "logs",
    operation_id = "clientLogsGet",
    params(("id" = String, Path, description = "The bundle id (its filename stem)")),
    responses(
        (status = OK, description = "The bundle body", body = String, content_type = "text/plain"),
        (status = NOT_FOUND, description = "No bundle with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The bundle file is unreadable", body = ApiError),
    )
)]
pub(crate) async fn client_logs_get(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    match st.client_logs.load(&id) {
        Ok(body) => (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            api_error(StatusCode::NOT_FOUND, "no bundle with that id")
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't read the log bundle — {e}"),
        ),
    }
}

/// Delete a client log bundle
///
/// `404` if there is no such bundle.
#[utoipa::path(
    delete,
    path = "/client-logs/{id}",
    tag = "logs",
    operation_id = "clientLogsDelete",
    params(("id" = String, Path, description = "The bundle id (its filename stem)")),
    responses(
        (status = NO_CONTENT, description = "Bundle deleted"),
        (status = NOT_FOUND, description = "No bundle with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't delete the log bundle", body = ApiError),
    )
)]
pub(crate) async fn client_logs_delete(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    match st.client_logs.delete(&id) {
        Ok(()) => {
            tracing::info!(id, "management API: client log bundle deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            api_error(StatusCode::NOT_FOUND, "no bundle with that id")
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't delete the log bundle — {e}"),
        ),
    }
}
