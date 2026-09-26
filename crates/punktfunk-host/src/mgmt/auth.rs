//! Auth gate for `/api/v1`: a paired device (from anywhere) or a bearer token (loopback peers
//! only).
//!
//! Three lanes:
//! - **paired device** — [`cert_may_access`] (status reads plus two writes: log upload and
//!   host-action invoke). Proven either by a client certificate over mTLS, or — for a browser,
//!   which has none — by a device token from [`super::device_auth`]. Same authority either way,
//!   because it is the same pairing.
//! - **plugin token** (bearer, loopback) — [`plugin_may_access`]: admin minus hooks, pairing
//!   admin, host logs, store, and update.
//! - **admin token** (bearer, loopback) — everything.

use super::shared::*;
use crate::gamestream::tls::PeerAddr;
use crate::gamestream::tls::PeerCertFingerprint;
use axum::extract::Request;
use axum::http::header;
use axum::http::Method;
use axum::middleware::Next;
use sha2::{Digest, Sha256};

/// Which credential authorized this request. [`require_auth`] stamps it on every forwarded
/// request; handlers extract `Extension<AuthLane>`. A missing extension is a 500, not a
/// default — a router that forgot the middleware must fail closed.
///
/// [`plugin_may_access`] answers route reachability; this answers field authority. Some
/// payloads carry operator-privileged fields (`prep`, `launch.kind == "command"`) on routes
/// a plugin may otherwise call. See [`crate::library::reject_privileged_fields`].
/// The paired device behind this request, stamped by [`require_auth`] on the `Cert` lane.
///
/// One extension for both proofs, because everything downstream — grant masks, expiry, the
/// power check in `mgmt::actions` — asks "which paired device is this", never "how did it
/// prove itself". Reading the TLS peer certificate directly would have answered only one of
/// the two.
#[derive(Clone, Debug)]
pub(crate) struct PairedDevice(pub String);

/// Which plugin a request came from, when it presented that plugin's own token rather than the
/// runner's shared one. Stamped like [`PairedDevice`], for the same reason: the routes that care
/// ask "whose is this", not "how did it prove it".
///
/// Absent on the shared runner token — a loose script has no plugin identity — and the id-scoped
/// routes then fall back to the older, unowned behaviour.
#[derive(Clone, Debug)]
pub(crate) struct PluginIdentity(pub String);

/// May a request carrying `identity` write the registration or provider named `id`?
///
/// One rule, in one place, for `PUT/DELETE /plugins/{id}` and the provider routes: a plugin that
/// proved which plugin it is may write only its own id.
pub(crate) fn plugin_owns(identity: Option<&PluginIdentity>, id: &str) -> bool {
    identity.is_none_or(|who| who.0 == id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthLane {
    /// Operator admin bearer (loopback): everything, including privileged fields.
    Admin,
    /// Scripting-runner bearer (loopback): [`plugin_may_access`] routes, never privileged fields.
    Plugin,
    /// A paired device: the [`cert_may_access`] set. Its fingerprint is stamped as
    /// [`PairedDevice`], whichever way it was proven.
    Cert,
    /// Open route (`/health`) or loopback-only tray summary — no credential.
    Public,
}

impl AuthLane {
    /// Fields that become command execution as the host user. Admin only. Still refused
    /// on other lanes if a write that carries `prep` / `launch.kind` is later allowlisted.
    pub(crate) fn may_set_privileged_fields(self) -> bool {
        matches!(self, AuthLane::Admin)
    }

    /// Operator's own lane (the console), as opposed to a paired client or a plugin.
    ///
    /// Same arm as [`Self::may_set_privileged_fields`] today, deliberately a separate
    /// question: command execution vs. "is this the person curating the library". A
    /// read-only operator-only view (hidden titles) is not a privilege escalation;
    /// collapsing the two would leave whichever changes first answering for the other.
    pub(crate) fn is_operator(self) -> bool {
        matches!(self, AuthLane::Admin)
    }
}

/// Paired client cert (mTLS, from anywhere) or a bearer token from a loopback peer.
/// `/health` is always open; `/local/summary` is loopback-only (the tray cannot read the
/// token file). Cert: [`cert_may_access`]. Bearer: full admin, confined to loopback because
/// the listener binds all interfaces by default.
pub(crate) async fn require_auth(
    State(st): State<Arc<MgmtState>>,
    req: Request,
    next: Next,
) -> Response {
    /// Stamp the lane so a handler can refuse privileged fields to a non-operator (see [`AuthLane`]).
    async fn forward(mut req: Request, next: Next, lane: AuthLane) -> Response {
        req.extensions_mut().insert(lane);
        next.run(req).await
    }

    /// The `Cert` lane, plus which device it is. Handlers re-read the pairing store from this.
    async fn forward_device(mut req: Request, next: Next, fp: String) -> Response {
        req.extensions_mut().insert(PairedDevice(fp));
        forward(req, next, AuthLane::Cert).await
    }

    /// The plugin lane: the route allowlist, and the caller's identity when it has one.
    async fn forward_plugin(
        mut req: Request,
        next: Next,
        identity: Option<PluginIdentity>,
    ) -> Response {
        if !plugin_may_access(req.method(), req.uri().path()) {
            return api_error(
                StatusCode::FORBIDDEN,
                "this route is not authorized for the plugin token — it requires the \
                 operator's admin token",
            );
        }
        if let Some(identity) = identity {
            req.extensions_mut().insert(identity);
        }
        forward(req, next, AuthLane::Plugin).await
    }

    if req.uri().path() == "/api/v1/health" {
        return forward(req, next, AuthLane::Public).await;
    }
    // The browser plane's certificate hash. Open because a browser that has never paired has no
    // client certificate to present, and the response authorises nothing — see
    // `mgmt::webtransport` for why PAKE, not this route, is what proves the peer.
    if req.uri().path() == "/api/v1/webtransport" {
        return forward(req, next, AuthLane::Public).await;
    }
    // The device exchange itself. Unauthenticated by necessity — this is what a client calls in
    // order to authenticate — and it hands out nothing but a random nonce until a signature by a
    // paired key arrives. See `mgmt::device_auth`.
    if matches!(
        req.uri().path(),
        "/api/v1/auth/device/challenge" | "/api/v1/auth/device/token"
    ) {
        return forward(req, next, AuthLane::Public).await;
    }
    // Tray status: unauthenticated, loopback only. On Windows the token file is
    // SYSTEM/Administrators-DACL'd, so the per-user tray cannot authenticate. Not on the
    // cert allowlist — LAN clients already have `/status`. No PeerAddr ⇒ test ⇒ loopback.
    if req.uri().path() == "/api/v1/local/summary" {
        let from_loopback = req
            .extensions()
            .get::<PeerAddr>()
            .is_none_or(|a| a.0.ip().is_loopback());
        return if from_loopback {
            forward(req, next, AuthLane::Public).await
        } else {
            api_error(
                StatusCode::UNAUTHORIZED,
                "the local summary is loopback-only",
            )
        };
    }
    // Fingerprint is attached by `serve_https` from the verified peer cert. Paired-to-stream
    // is not paired-to-administer: only [`cert_may_access`]; everything else needs the bearer.
    if let Some(PeerCertFingerprint(Some(fp))) = req.extensions().get::<PeerCertFingerprint>() {
        // `effective`, not `is_paired`. The expiry-blind verb answers "is this device listed"
        // (the roster). Authorizing on the listing would leave a lapsed guest on this lane
        // for as long as the record sits in the store.
        if cert_may_access(req.method(), req.uri().path())
            && st
                .native
                .as_ref()
                .is_some_and(|n| n.effective(fp, unix_now()).is_some())
        {
            let fp = fp.clone();
            return forward_device(req, next, fp).await;
        }
    }
    // A browser's device token. The same lane and the same route set as the certificate above:
    // it is the same pairing, proven with the key instead of with TLS. `effective` is re-read
    // here rather than trusted from the exchange, so unpairing a device revokes it at once
    // rather than when its token lapses.
    if let Some(token) = bearer(&req) {
        if let Some(fp) = st.device_auth.device_for(token) {
            if cert_may_access(req.method(), req.uri().path())
                && st
                    .native
                    .as_ref()
                    .is_some_and(|n| n.effective(&fp, unix_now()).is_some())
            {
                return forward_device(req, next, fp).await;
            }
        }
    }
    // Full admin surface, so loopback only — the listener binds all interfaces so paired
    // clients can browse the library. No PeerAddr ⇒ unit test ⇒ treat as loopback.
    let from_loopback = req
        .extensions()
        .get::<PeerAddr>()
        .is_none_or(|a| a.0.ip().is_loopback());
    if !from_loopback {
        return api_error(
            StatusCode::UNAUTHORIZED,
            "the admin API is loopback-only — a LAN client must present a paired client certificate",
        );
    }
    // `run` always passes a token; no-token is a misconfigured caller (a test building `app`
    // directly) — deny.
    let Some(expected) = st.token.as_deref() else {
        return api_error(StatusCode::UNAUTHORIZED, "authentication required");
    };
    let presented = bearer(&req);
    match presented {
        Some(token) if token_eq(token, expected) => forward(req, next, AuthLane::Admin).await,
        // Same loopback confinement. Checked AFTER the admin token so equal tokens
        // (operator misconfiguration) degrade to full access, never to a lockout.
        Some(token)
            if st
                .plugin_token
                .as_deref()
                .is_some_and(|pt| token_eq(token, pt)) =>
        {
            forward_plugin(req, next, None).await
        }
        // A plugin's OWN token: the same routes, plus the identity that makes them its own.
        Some(token) => {
            let find = |tokens: &std::collections::BTreeMap<String, String>| {
                tokens
                    .iter()
                    .find(|(_, pt)| token_eq(token, pt))
                    .map(|(id, _)| id.clone())
            };
            let mut who = find(&st.plugin_tokens.read().unwrap_or_else(|p| p.into_inner()));
            // `plugins add` mints in its own process: the file has the token before memory does.
            if who.is_none() {
                if let Some(fresh) = crate::mgmt_token::read_per_plugin(&st.config_dir) {
                    who = find(&fresh);
                    *st.plugin_tokens.write().unwrap_or_else(|p| p.into_inner()) = fresh;
                }
            }
            match who {
                Some(id) => forward_plugin(req, next, Some(PluginIdentity(id))).await,
                None => api_error(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid credentials (a paired device, or a bearer token)",
                ),
            }
        }
        None => api_error(
            StatusCode::UNAUTHORIZED,
            "missing or invalid credentials (a paired device, or a bearer token)",
        ),
    }
}

/// The `Authorization: Bearer` value, if there is one. Every lane that takes a token reads it
/// through here so they cannot disagree about the prefix.
fn bearer(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Allowlist of routes the plugin token may reach. A later route is denied until classified
/// (`every_route_is_classified_for_the_plugin_and_cert_lanes` in `mgmt::tests` fails the
/// build otherwise).
///
/// Out of the list: hooks (operator commands + webhook secrets), `GET /logs` (those secrets
/// unredacted), pairing admin, UI-proxy credentials, the plugin store, the update surface,
/// and the access overview/decision routes — a plugin may ask for a folder, never grant one.
/// Library writes are in because a provider reconciles its own entries; `prep` and
/// `launch.kind == "command"` are refused in the handlers via [`AuthLane`].
///
/// Route reachability is not plugin identity: the runner is one process on one token, so this
/// gate cannot tell which plugin is calling and any holder may write another's registration.
/// What that no longer buys is a command — an `exec` entry resolves against the manifest of the
/// package that declared the provider id (`plugins::manifest`), not against the caller.
pub(crate) fn plugin_may_access(method: &Method, path: &str) -> bool {
    // (method, path); `{}` is exactly one segment. Grouped as the route table is.
    const ALLOWED: &[(&Method, &str)] = &[
        (&Method::GET, "/api/v1/health"),
        (&Method::GET, "/api/v1/host"),
        (&Method::GET, "/api/v1/status"),
        (&Method::GET, "/api/v1/local/summary"),
        (&Method::GET, "/api/v1/compositors"),
        (&Method::GET, "/api/v1/events"),
        // Rosters, read-only. DELETE is pairing admin — not listed.
        (&Method::GET, "/api/v1/clients"),
        (&Method::GET, "/api/v1/native/clients"),
        (&Method::GET, "/api/v1/gpus"),
        (&Method::PUT, "/api/v1/gpus/preference"),
        (&Method::GET, "/api/v1/host/theme"),
        (&Method::GET, "/api/v1/display/settings"),
        (&Method::PUT, "/api/v1/display/settings"),
        (&Method::GET, "/api/v1/display/state"),
        (&Method::GET, "/api/v1/display/monitors"),
        (&Method::PUT, "/api/v1/display/layout"),
        (&Method::POST, "/api/v1/display/release"),
        (&Method::GET, "/api/v1/display/presets"),
        (&Method::POST, "/api/v1/display/presets"),
        (&Method::PUT, "/api/v1/display/presets/{}"),
        (&Method::DELETE, "/api/v1/display/presets/{}"),
        (&Method::GET, "/api/v1/display/clients/{}"),
        (&Method::PUT, "/api/v1/display/clients/{}"),
        (&Method::DELETE, "/api/v1/display/clients/{}"),
        (&Method::DELETE, "/api/v1/session"),
        (&Method::POST, "/api/v1/session/idr"),
        // Per-session stop/keyframe/mute ride the same lane as their host-wide forms.
        // `PUT /session/{}/access` does not: re-pointing a grant set is access
        // administration, like `PATCH /native/clients/{}`.
        (&Method::DELETE, "/api/v1/session/{}"),
        (&Method::POST, "/api/v1/session/{}/idr"),
        (&Method::PUT, "/api/v1/session/{}/audio"),
        (&Method::GET, "/api/v1/session/last"),
        (&Method::GET, "/api/v1/session/settings"),
        (&Method::PUT, "/api/v1/session/settings"),
        (&Method::POST, "/api/v1/game/end"),
        // Library reads + provider reconcile. Privileged fields refused via `AuthLane`.
        (&Method::GET, "/api/v1/library"),
        (&Method::GET, "/api/v1/library/art/{}/{}"),
        (&Method::GET, "/api/v1/library/scanners"),
        (&Method::PUT, "/api/v1/library/scanners/{}"),
        (&Method::POST, "/api/v1/library/custom"),
        (&Method::PUT, "/api/v1/library/custom/{}"),
        (&Method::DELETE, "/api/v1/library/custom/{}"),
        (&Method::PUT, "/api/v1/library/provider/{}"),
        (&Method::DELETE, "/api/v1/library/provider/{}"),
        // Provider liveness for its own titles — mapped through the catalog, no one else's session.
        (&Method::PUT, "/api/v1/library/provider/{}/running"),
        // An Art & Metadata source pushes its own result and reads its mode. Ordering, the
        // switches and picks are the operator's.
        (&Method::GET, "/api/v1/library/metadata"),
        (&Method::PUT, "/api/v1/library/metadata/{}"),
        (&Method::DELETE, "/api/v1/library/metadata/{}"),
        (&Method::POST, "/api/v1/stats/capture/start"),
        (&Method::POST, "/api/v1/stats/capture/stop"),
        (&Method::GET, "/api/v1/stats/capture/status"),
        (&Method::GET, "/api/v1/stats/capture/live"),
        (&Method::GET, "/api/v1/stats/recordings"),
        (&Method::GET, "/api/v1/stats/recordings/{}"),
        (&Method::DELETE, "/api/v1/stats/recordings/{}"),
        (&Method::GET, "/api/v1/plugins"),
        (&Method::POST, "/api/v1/plugins/logs"),
        // A plugin asks for a folder and reads its own rows; deciding is admin-only, so the
        // overview and `/plugin-access/{}/decide` are deliberately absent here.
        (&Method::GET, "/api/v1/plugin-access/requests"),
        (&Method::POST, "/api/v1/plugin-access/requests"),
        (&Method::PUT, "/api/v1/plugins/{}"),
        (&Method::DELETE, "/api/v1/plugins/{}"),
    ];
    ALLOWED
        .iter()
        .any(|(m, pat)| *m == method && path_matches(pat, path))
}

/// `{}` is exactly one segment. Never a prefix test: `/api/v1/plugins/{}` must not swallow
/// `/api/v1/plugins/x/ui-credential` the way `starts_with` would.
fn path_matches(pattern: &str, path: &str) -> bool {
    let (mut p, mut a) = (pattern.split('/'), path.split('/'));
    loop {
        match (p.next(), a.next()) {
            (None, None) => return true,
            (Some(pe), Some(ae)) if pe == "{}" || pe == ae => continue,
            _ => return false,
        }
    }
}

/// Allowlist a paired streaming cert may reach. Deny-by-default: pairing PIN, pending queue,
/// and every other mutation need the operator bearer. `/health` is always open, separately.
pub(crate) fn cert_may_access(method: &Method, path: &str) -> bool {
    // Write-only: the device gets an id back and can read nothing, not even its own upload.
    // Size- and quota-capped in the handler/store.
    if method == Method::POST && path == "/api/v1/client-logs" {
        return true;
    }
    // Id-only invoke. The handler re-reads `effective(fp, now)` and demands `GRANT_POWER`;
    // the route being reachable grants nothing by itself. `GET /actions` is on the read list.
    if method == Method::POST && path_matches("/api/v1/actions/{}", path) {
        return true;
    }
    method == Method::GET
        && (matches!(
            path,
            "/api/v1/host"
                | "/api/v1/compositors"
                | "/api/v1/status"
                | "/api/v1/actions"
                // Rosters are not on this lane: they name every other paired device. Library
                // GET is; POST/PUT/DELETE stay token-only via this exact-path match.
                | "/api/v1/library"
        ) || path.starts_with("/api/v1/library/art/"))
}

/// Compare SHA-256 digests, not the strings — constant-time in the secret without a ct-eq
/// dependency.
pub(crate) fn token_eq(presented: &str, expected: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(expected.as_bytes())
}

/// Host wall clock, unix seconds — the clock every stored access deadline is expressed in.
/// Sampled at each check, same as `mgmt::native`'s copy.
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GET /logs` is the `/hooks` carve-out's back door: webhook URLs (the bearer for those
    /// sinks) and spawned command lines, unredacted. A plugin only needs the other direction.
    #[test]
    fn the_plugin_lane_writes_logs_but_never_reads_them() {
        assert!(!plugin_may_access(&Method::GET, "/api/v1/logs"));
        assert!(plugin_may_access(&Method::POST, "/api/v1/plugins/logs"));
    }
}
