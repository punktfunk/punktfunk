//! Read and replace `hooks.json`. Validated on write; the runner rereads the store per event
//! so a PUT applies without a restart.

use super::shared::*;

/// Get the hook configuration
///
/// Empty document when none is stored.
#[utoipa::path(
    get,
    path = "/hooks",
    tag = "hooks",
    operation_id = "getHooks",
    responses(
        (status = OK, description = "The stored hook configuration", body = crate::hooks::HooksConfig),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_hooks() -> Json<crate::hooks::HooksConfig> {
    Json(crate::hooks::store().get())
}

/// Replace the hook configuration
///
/// Whole-document PUT, not a patch. Applies from the next event. Commands run as the host
/// user (the interactive session on Windows).
#[utoipa::path(
    put,
    path = "/hooks",
    tag = "hooks",
    operation_id = "setHooks",
    request_body = crate::hooks::HooksConfig,
    responses(
        (status = OK, description = "Configuration stored; the new state", body = crate::hooks::HooksConfig),
        (status = BAD_REQUEST, description = "Structurally invalid configuration", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Configuration could not be persisted", body = ApiError),
    )
)]
pub(crate) async fn set_hooks(ApiJson(cfg): ApiJson<crate::hooks::HooksConfig>) -> Response {
    if let Err(e) = cfg.validate() {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    match crate::hooks::store().set(cfg) {
        Ok(()) => {
            tracing::info!("management API: hook configuration updated");
            Json(crate::hooks::store().get()).into_response()
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the hooks — {e:#}"),
        ),
    }
}
