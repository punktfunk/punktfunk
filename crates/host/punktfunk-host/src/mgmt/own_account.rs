//! What a seat host on a person's own Linux account keeps out of their account: the console
//! manages no plugin and no emulator there. Reads stay, so the console can show the profile.

use super::scope::{row_of, Scope};
use super::shared::*;
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;

/// The code of each sentence's host message `apierr_<code>`, and its English.
const PLUGINS: (&str, &str) = (
    "plugins_own_account",
    "Plugins don't run on a profile that uses an existing Linux account yet.",
);
const EMULATORS: (&str, &str) = (
    "emulators_own_account",
    "The console doesn't manage emulators inside a profile that uses an existing Linux account.",
);

/// Why `method` on `path` is refused there, if it is: a write to a route that runs code as the
/// person ([`Scope::ProfileExec`]), read as the router reads the path. Hooks refuse themselves,
/// on every system. A catalog refresh only fetches the catalogs, so it stays with the reads.
pub(super) fn refusal(method: &Method, path: &str) -> Option<(&'static str, &'static str)> {
    if matches!(*method, Method::GET | Method::HEAD)
        || matches!(path, "/api/v1/store/refresh" | "/api/v1/hooks")
    {
        return None;
    }
    (row_of(method, path)? == Scope::ProfileExec).then(|| {
        if path.starts_with("/api/v1/emulators/") {
            EMULATORS
        } else {
            PLUGINS
        }
    })
}

/// [`refusal`] on a seat host on a person's own Linux account; every other host passes.
pub(super) async fn gate(req: Request, next: Next) -> Response {
    let here = cfg!(target_os = "linux") && pf_paths::seat::own_account();
    match here
        .then(|| refusal(req.method(), req.uri().path()))
        .flatten()
    {
        Some((code, why)) => api_error_coded(StatusCode::FORBIDDEN, code, why),
        None => next.run(req).await,
    }
}
