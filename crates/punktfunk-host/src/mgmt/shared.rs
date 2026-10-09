//! Shared management-API plumbing: [`ApiError`] envelope, [`api_error`], [`ApiJson`].
//!
//! Every non-2xx body is [`ApiError`]. [`ApiJson`] rewraps axum JSON rejections
//! so that contract holds. Handler modules `use super::shared::*` for the
//! axum/serde/utoipa prelude.

use axum::extract::Request;

pub(crate) use super::MgmtState;
pub(crate) use axum::extract::{Path, Query, State};
pub(crate) use axum::http::StatusCode;
pub(crate) use axum::response::{IntoResponse, Response};
pub(crate) use axum::Json;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::sync::Arc;
pub(crate) use utoipa::ToSchema;

/// Envelope for every non-2xx body.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ApiError {
    error: String,
}

/// Bulk-unpair result, shared by `/clients` and `/native/clients`.
///
/// A count, not 204: unpair-everything is idempotent, and the operator still
/// needs to know whether that was three devices or none.
#[derive(Serialize, ToSchema)]
pub(crate) struct UnpairAllResult {
    #[schema(example = 3)]
    pub(crate) unpaired: u32,
}

pub(crate) fn api_error(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            error: message.to_string(),
        }),
    )
        .into_response()
}

/// Runs `f` off the async worker. A worker that never finishes is logged and answers 500
/// "The {what} stopped responding."; the panic text stays in the log.
pub(super) async fn blocking<T, F>(what: &'static str, f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!(worker = what, error = %e, "management worker did not finish");
        api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("The {what} stopped responding."),
        )
    })
}

/// `axum::Json` that rewraps rejections (400/422/415) in [`ApiError`].
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<S, T> axum::extract::FromRequest<S> for ApiJson<T>
where
    Json<T>: axum::extract::FromRequest<S, Rejection = axum::extract::rejection::JsonRejection>,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(ApiJson(value)),
            Err(rejection) => Err(api_error(rejection.status(), &rejection.body_text())),
        }
    }
}
