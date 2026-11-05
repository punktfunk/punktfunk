//! What a seat host on a person's own Linux account keeps out of their account: the console
//! manages no plugin and no emulator there. Reads stay, so the console can show the profile.

use super::shared::*;
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;

const PLUGINS: &str = "Plugins don't run on a profile that uses an existing Linux account yet.";
const EMULATORS: &str =
    "The console doesn't manage emulators inside a profile that uses an existing Linux account.";

/// The route groups refused, each with the sentence the console shows.
const REFUSED: [(&str, &str); 3] = [
    ("/api/v1/store", PLUGINS),
    ("/api/v1/plugin-access", PLUGINS),
    ("/api/v1/emulators", EMULATORS),
];

/// Why `method` on `path` is refused there, if it is: every write in a [`REFUSED`] group. A
/// catalog refresh only fetches the catalogs, so it stays with the reads.
pub(super) fn refusal(method: &Method, path: &str) -> Option<&'static str> {
    if matches!(*method, Method::GET | Method::HEAD) || path == "/api/v1/store/refresh" {
        return None;
    }
    let within = |group: &str| {
        path.strip_prefix(group)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    };
    REFUSED
        .iter()
        .find(|(group, _)| within(group))
        .map(|(_, why)| *why)
}

/// [`refusal`] on a seat host on a person's own Linux account; every other host passes.
pub(super) async fn gate(req: Request, next: Next) -> Response {
    let here = cfg!(target_os = "linux") && pf_paths::seat::own_account();
    match here
        .then(|| refusal(req.method(), req.uri().path()))
        .flatten()
    {
        Some(why) => api_error(StatusCode::FORBIDDEN, why),
        None => next.run(req).await,
    }
}
