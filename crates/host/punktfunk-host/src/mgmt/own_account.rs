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

/// What the person reads in their own session: emulators there come in a later version.
pub(super) const EMULATORS_YET: (&str, &str) = (
    "emulators_own_account_yet",
    "Emulators can't be managed here yet.",
);

/// What the person does with their own emulators: point at a copy of their own, and answer its
/// first-run questions. Installs stay the box's, and the save exchange stays off.
const THEIR_EMULATORS: &[&str] = &["/api/v1/emulators/{}/adopt", "/api/v1/emulators/{}/prepare"];

/// [`refusal`] on a seat host on a person's own Linux account, on every lane, that person's
/// included but for their folder grants and [`THEIR_EMULATORS`]; every other host passes.
pub(super) async fn gate(req: Request, next: Next) -> Response {
    let here = cfg!(target_os = "linux") && pf_paths::seat::own_account();
    let person =
        req.extensions().get::<super::auth::AuthLane>() == Some(&super::auth::AuthLane::Person);
    let path = req.uri().path();
    let grants = path.starts_with("/api/v1/plugin-access/");
    let matches = punktfunk_core::mgmt_routes::path_matches;
    let theirs = THEIR_EMULATORS.iter().any(|p| matches(p, path));
    match here
        .then(|| refusal(req.method(), req.uri().path()))
        .flatten()
    {
        Some(PLUGINS) if person && grants => next.run(req).await,
        Some(EMULATORS) if person && theirs => next.run(req).await,
        Some(EMULATORS) if person => {
            api_error_coded(StatusCode::FORBIDDEN, EMULATORS_YET.0, EMULATORS_YET.1)
        }
        Some((code, why)) => api_error_coded(StatusCode::FORBIDDEN, code, why),
        None => next.run(req).await,
    }
}
