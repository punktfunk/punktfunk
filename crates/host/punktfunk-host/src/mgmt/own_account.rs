//! What a seat host on a person's own Linux account keeps out of their account: the console
//! manages no plugin and no emulator there. Their own lane installs and runs their plugins, and
//! those plugins write their own. Reads stay, so the console can show the profile.

use super::auth::AuthLane;
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

/// What any other lane reads where the person's lane is open.
const PLUGINS_BY_PERSON: (&str, &str) = (
    "plugins_own_account_person",
    "Plugins on a profile that uses an existing account are installed by that person, from \
     their own desktop.",
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

/// Whether `lane` may make the plugin write `path` on a person's own account: their own lane,
/// and their plugins on the routes the plugin lane reaches at all. Catalog sources stay closed:
/// only the built-in source installs there.
fn their_plugins(lane: Option<AuthLane>, path: &str) -> bool {
    matches!(lane, Some(AuthLane::Person | AuthLane::Plugin))
        && !path.starts_with("/api/v1/store/sources/")
}

/// A plugin page's credential, which opens its settings and sign-ins: on a person's own account
/// a read of their lane alone.
const THEIR_PAGES: &str = "/api/v1/plugins/{}/ui-credential";

/// [`refusal`] on a seat host on a person's own Linux account, on every lane but theirs for
/// [`their_plugins`], [`THEIR_PAGES`] and [`THEIR_EMULATORS`]; every other host passes. The
/// person is told a catalog source would bring unreviewed plugins; while their lane is open any
/// other lane is told the plugins are theirs.
pub(super) async fn gate(State(st): State<Arc<MgmtState>>, req: Request, next: Next) -> Response {
    let here = cfg!(target_os = "linux") && pf_paths::seat::own_account();
    let lane = req.extensions().get::<AuthLane>().copied();
    let person = lane == Some(AuthLane::Person);
    let path = req.uri().path();
    let matches = punktfunk_core::mgmt_routes::path_matches;
    let theirs = THEIR_EMULATORS.iter().any(|p| matches(p, path));
    let page = matches!(*req.method(), Method::GET | Method::HEAD) && matches(THEIR_PAGES, path);
    match here
        .then(|| refusal(req.method(), path).or(page.then_some(PLUGINS)))
        .flatten()
    {
        Some(PLUGINS) if their_plugins(lane, path) => next.run(req).await,
        Some(PLUGINS) if person => {
            let (code, why) = super::store::REVIEWED_ONLY;
            api_error_coded(StatusCode::FORBIDDEN, code, why)
        }
        Some(PLUGINS) if st.person.is_some() => api_error_coded(
            StatusCode::FORBIDDEN,
            PLUGINS_BY_PERSON.0,
            PLUGINS_BY_PERSON.1,
        ),
        Some(EMULATORS) if person && theirs => next.run(req).await,
        Some(EMULATORS) if person => {
            api_error_coded(StatusCode::FORBIDDEN, EMULATORS_YET.0, EMULATORS_YET.1)
        }
        Some((code, why)) => api_error_coded(StatusCode::FORBIDDEN, code, why),
        None => next.run(req).await,
    }
}
