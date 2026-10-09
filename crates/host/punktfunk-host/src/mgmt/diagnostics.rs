//! HTTP handlers for [`crate::diagnostics`].
//!
//! `GET /diagnostics` returns the last report; `POST /diagnostics/refresh` re-runs
//! probes. Both are absent from `auth::plugin_may_access` and `cert_may_access`:
//! the body names users, groups, and device nodes. Pin: `mgmt::tests` lane matrix.

use super::shared::*;
use crate::diagnostics::{CheckSource, DiagnosticsReport};

/// Last cached health report
///
/// Probes run at startup and on `POST /diagnostics/refresh`.
#[utoipa::path(
    get,
    path = "/diagnostics",
    tag = "diagnostics",
    operation_id = "getDiagnostics",
    responses(
        (status = OK, description = "The current verdicts, worst-first", body = DiagnosticsReport),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_diagnostics() -> Json<DiagnosticsReport> {
    Json(crate::diagnostics::registry().report())
}

/// Re-run every probe
///
/// Returns the new report. Poll GET; membership and udev rules only change
/// after the operator changes them.
#[utoipa::path(
    post,
    path = "/diagnostics/refresh",
    tag = "diagnostics",
    operation_id = "refreshDiagnostics",
    responses(
        (status = OK, description = "The refreshed verdicts, worst-first", body = DiagnosticsReport),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn refresh_diagnostics() -> Json<DiagnosticsReport> {
    // Probes `stat` sysfs and shell out to `id`; NSS on a remote directory can block.
    let report = tokio::task::spawn_blocking(|| {
        let reg = crate::diagnostics::registry();
        reg.run_all(CheckSource::Refresh);
        reg.report()
    })
    .await
    .unwrap_or_else(|_| crate::diagnostics::registry().report());
    Json(report)
}
