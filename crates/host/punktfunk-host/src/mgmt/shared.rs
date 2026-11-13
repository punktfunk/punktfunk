//! Shared management-API plumbing: [`ApiError`] envelope, [`api_error`], [`ApiJson`].
//!
//! Every non-2xx body is [`ApiError`]. [`ApiJson`] rewraps axum JSON rejections
//! so that contract holds. Handler modules `use super::shared::*` for the
//! axum/serde/utoipa prelude.

use axum::extract::Request;
use std::collections::BTreeMap;

pub(crate) use super::MgmtState;
pub(crate) use axum::extract::{Path, Query, State};
pub(crate) use axum::http::StatusCode;
pub(crate) use axum::response::{IntoResponse, Response};
pub(crate) use axum::Json;
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use std::sync::Arc;
pub(crate) use utoipa::ToSchema;

/// Envelope for every non-2xx body.
///
/// A sentence written for a person carries `code`: an app words it as the host message
/// `apierr_<code>` with `params`, and shows `error` for a code it does not know. Without
/// `code`, `error` is developer text and an app words the failure itself.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApiError {
    /// The sentence in English, or developer text when `code` is absent.
    pub(crate) error: String,
    /// The sentence's id: the message `apierr_<code>` in the host's messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<String>,
    /// The values of that message's placeholders, by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) params: Option<BTreeMap<String, String>>,
}

impl ApiError {
    /// A sentence for a person: `text` is the English of the message `apierr_<code>`.
    pub(crate) fn coded(code: &str, text: impl Into<String>) -> Self {
        Self {
            error: text.into(),
            code: Some(code.to_string()),
            params: None,
        }
    }

    /// Developer text, which an app does not word.
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            error: text.into(),
            code: None,
            params: None,
        }
    }

    /// Fills the placeholder `name`.
    pub(crate) fn param(mut self, name: &str, value: impl ToString) -> Self {
        self.params
            .get_or_insert_with(BTreeMap::new)
            .insert(name.to_string(), value.to_string());
        self
    }

    pub(crate) fn respond(self, status: StatusCode) -> Response {
        (status, Json(self)).into_response()
    }
}

/// A sentence for a person whose cause is an error chain: the chain goes to the log.
pub(crate) fn api_error_logged(
    status: StatusCode,
    code: &str,
    text: &str,
    e: &dyn std::fmt::Display,
) -> Response {
    tracing::warn!(code, error = %e, "management request failed");
    api_error_coded(status, code, text)
}

/// The generic 500 for a failure whose only text is an error chain: the chain goes to the log.
pub(crate) fn host_error(what: &str, e: &dyn std::fmt::Display) -> Response {
    tracing::warn!(error = %e, "{what}");
    api_error_coded(
        StatusCode::INTERNAL_SERVER_ERROR,
        "host_error",
        "The host hit an error",
    )
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

/// Developer text: a request the console never sends, a stale id, plumbing.
pub(crate) fn api_error(status: StatusCode, message: &str) -> Response {
    ApiError::plain(message).respond(status)
}

/// A sentence for a person, with no placeholders: `text` is the English of `apierr_<code>`.
pub(crate) fn api_error_coded(status: StatusCode, code: &str, text: &str) -> Response {
    ApiError::coded(code, text).respond(status)
}

/// Runs `f` off the async worker. A worker that never finishes is logged and answers 500
/// `apierr_worker_<worker>_stopped`; the panic text stays in the log.
pub(super) async fn blocking<T, F>(worker: &'static str, f: F) -> Result<T, Response>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| {
        tracing::error!(worker, error = %e, "management worker did not finish");
        worker_stopped(worker)
    })
}

/// The 500 for the management worker `worker` that never finished.
pub(super) fn worker_stopped(worker: &str) -> Response {
    let text = match worker {
        "action_check" => "The action check stopped responding.",
        "emulator_manager" => "The emulator manager stopped responding.",
        "install" => "The install stopped responding.",
        "monitor_list" => "The monitor list stopped responding.",
        "monitor_switch" => "The monitor switch stopped responding.",
        "plugin_access" => "The plugin access worker stopped responding.",
        "plugin_store" => "The plugin store stopped responding.",
        "power_check" => "The power check stopped responding.",
        "profile_store" => "The profile store stopped responding.",
        "seat" => "The seat stopped responding.",
        "seat_check" => "The seat check stopped responding.",
        "seat_ledger" => "The seat ledger stopped responding.",
        "seat_supervisor" => "The seat supervisor stopped responding.",
        "seats" => "The seats stopped responding.",
        "switch" => "The switch stopped responding.",
        "windows_accounts" => "The Windows accounts stopped responding.",
        _ => return host_error("management worker has no sentence", &worker),
    };
    let code = format!("worker_{worker}_stopped");
    api_error_coded(StatusCode::INTERNAL_SERVER_ERROR, &code, text)
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
