//! Which management API routes a paired device may call, and which anyone may.
//!
//! One table for the host's auth gate and the browser client's page server, which relays only
//! these. Methods are uppercase HTTP method names. A route not listed is denied.

/// `(method, path)` of the routes anyone may call without a key: health, the browser plane's
/// certificate hash, and the device exchange a browser calls in order to authenticate.
pub const OPEN_ROUTES: [(&str, &str); 4] = [
    ("GET", "/api/v1/health"),
    ("GET", "/api/v1/webtransport"),
    ("POST", "/api/v1/auth/device/challenge"),
    ("POST", "/api/v1/auth/device/token"),
];

/// A route in [`OPEN_ROUTES`].
pub fn open_route(method: &str, path: &str) -> bool {
    OPEN_ROUTES.contains(&(method, path))
}

/// `{}` is exactly one segment. Never a prefix test: `/api/v1/plugins/{}` must not swallow
/// `/api/v1/plugins/x/ui-credential` the way `starts_with` would.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let (mut p, mut a) = (pattern.split('/'), path.split('/'));
    loop {
        match (p.next(), a.next()) {
            (None, None) => return true,
            (Some(pe), Some(ae)) if pe == "{}" || pe == ae => continue,
            _ => return false,
        }
    }
}

/// Allowlist a paired device may reach. Deny-by-default: pairing PIN, pending queue, and every
/// other mutation need the operator bearer. [`OPEN_ROUTES`] are not on it.
pub fn paired_may_access(method: &str, path: &str) -> bool {
    // Write-only: the device gets an id back and can read nothing, not even its own upload.
    // Size- and quota-capped in the handler/store.
    if method == "POST" && path == "/api/v1/client-logs" {
        return true;
    }
    // Id-only invoke. The handler re-reads `effective(fp, now)` and demands `GRANT_POWER`;
    // the route being reachable grants nothing by itself. `GET /actions` is on the read list.
    if method == "POST" && path_matches("/api/v1/actions/{}", path) {
        return true;
    }
    // The handler scopes it to games this device launched, and refuses an expired device.
    if method == "POST" && path == "/api/v1/game/end" {
        return true;
    }
    // Installing is what launching a missing title does anyway; the handler demands
    // `GRANT_LAUNCH`. Pause and remove demand `GRANT_MANAGE_GAMES`; cancel stays the operator's.
    if (method == "POST" || method == "DELETE") && path_matches("/api/v1/library/install/{}", path)
    {
        return true;
    }
    if method == "POST" && path_matches("/api/v1/library/install/{}/pause", path) {
        return true;
    }
    // A client's picker: the list, a picture, getting a seat ready, and the challenge of a key
    // unlock for this device, which the handler answers only for a key it enrolled. Editing
    // stays the console's.
    if method == "GET"
        && (path == "/api/v1/profiles/enumerate"
            || path_matches("/api/v1/profiles/{}/avatar", path))
    {
        return true;
    }
    if method == "POST"
        && (path_matches("/api/v1/profiles/{}/wake", path)
            || path_matches("/api/v1/profiles/{}/unlock-challenge", path))
    {
        return true;
    }
    method == "GET"
        && (matches!(
            path,
            "/api/v1/host"
                | "/api/v1/compositors"
                | "/api/v1/status"
                | "/api/v1/actions"
                // Rosters are not on this lane: they name every other paired device. Library
                // GET is; POST/PUT/DELETE stay token-only via this exact-path match.
                | "/api/v1/library"
                | "/api/v1/library/page"
        ) || path.starts_with("/api/v1/library/art/"))
}
