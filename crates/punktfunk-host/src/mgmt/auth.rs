//! Auth gate for `/api/v1`: a paired device (from anywhere) or a bearer token (loopback peers
//! only).
//!
//! Three lanes:
//! - **paired device** — [`cert_may_access`] (status reads plus three writes: log upload,
//!   host-action invoke, ending its own games). Proven either by a client certificate over
//!   mTLS, or — for a browser, which has none — by a device token from
//!   [`super::device_auth`]. Same authority either way, because it is the same pairing.
//! - **plugin token** (bearer, loopback) — [`plugin_may_access`]: admin minus hooks, pairing
//!   admin, host logs, store, and update.
//! - **admin token** (bearer, loopback) — everything.

use super::shared::*;
use crate::gamestream::tls::PeerAddr;
use crate::gamestream::tls::PeerCertFingerprint;
use axum::extract::{FromRequestParts, Request};
use axum::http::header;
use axum::http::request::Parts;
use axum::http::Method;
use axum::middleware::Next;
use sha2::{Digest, Sha256};
use std::marker::PhantomData;

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

/// The `{id}` path segment of a plugin-owned resource, once the caller may write it.
///
/// One rule for every plugin-scoped write (`/plugins/{id}`, the library provider, scanner and
/// metadata routes): a plugin that proved which plugin it is may write only its own id (403).
/// The operator and the shared runner token may write any. `R` then checks the id's shape
/// (400). Both run before the body is read.
pub(crate) struct OwnedId<R>(pub String, pub PhantomData<R>);

/// The shape an [`OwnedId`] must have. `Err` is the 400's message.
pub(crate) trait IdRule {
    fn check(id: &str) -> Result<(), String>;
}

/// A library provider or metadata source ([`crate::library::validate_provider_name`]).
pub(crate) struct ProviderId;

impl IdRule for ProviderId {
    fn check(id: &str) -> Result<(), String> {
        crate::library::validate_provider_name(id)
    }
}

/// A plugin registration ([`crate::slug::plugin_id`]).
pub(crate) struct PluginId;

impl IdRule for PluginId {
    fn check(id: &str) -> Result<(), String> {
        crate::slug::plugin_id(id)
            .then_some(())
            .ok_or_else(|| "invalid plugin id (expected kebab-case `[a-z][a-z0-9-]*`, ≤64)".into())
    }
}

/// Any id: the route answers an unknown one itself.
pub(crate) struct AnyId;

impl IdRule for AnyId {
    fn check(_: &str) -> Result<(), String> {
        Ok(())
    }
}

impl<S: Send + Sync, R: IdRule> FromRequestParts<S> for OwnedId<R> {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Response> {
        let Path(id) = Path::<String>::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        if parts
            .extensions
            .get::<PluginIdentity>()
            .is_some_and(|who| who.0 != id)
        {
            return Err(api_error(
                StatusCode::FORBIDDEN,
                "a plugin may only write its own id",
            ));
        }
        R::check(&id).map_err(|e| api_error(StatusCode::BAD_REQUEST, &e))?;
        Ok(OwnedId(id, PhantomData))
    }
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
            .is_none_or(|a| a.0.ip().to_canonical().is_loopback());
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
                .is_some_and(|n| n.effective(fp, crate::clock::unix_secs()).is_some())
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
                    .is_some_and(|n| n.effective(&fp, crate::clock::unix_secs()).is_some())
            {
                return forward_device(req, next, fp).await;
            }
        }
    }
    // Full admin surface, so loopback only — the listener binds all interfaces so paired
    // clients can browse the library. No PeerAddr ⇒ unit test ⇒ treat as loopback. Canonical:
    // a dual-stack bind hands a local IPv4 caller over as ::ffff:127.0.0.1.
    let from_loopback = req
        .extensions()
        .get::<PeerAddr>()
        .is_none_or(|a| a.0.ip().to_canonical().is_loopback());
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
            // The file is the truth: `plugins add` mints and `plugins remove` revokes in their
            // own process, so a hit on memory alone keeps a removed plugin's token alive. A
            // small file, read per plugin request on the loopback lane.
            if let Some(fresh) = crate::mgmt_token::read_per_plugin(&st.config_dir) {
                *st.plugin_tokens.write().unwrap_or_else(|p| p.into_inner()) = fresh;
            }
            let who = find(&st.plugin_tokens.read().unwrap_or_else(|p| p.into_inner()));
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
        (&Method::GET, "/api/v1/library/page"),
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
        // Download progress for its own titles, mapped through the catalog the same way.
        (&Method::PUT, "/api/v1/library/provider/{}/downloads"),
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
        // What the emulators play and where their copies are, making one ready (firmware from the
        // plugin's own state folder), its firmware status and a game's add-ons. The save routes
        // also need the operator's save grant, checked in the handler.
        (&Method::GET, "/api/v1/emulators"),
        (&Method::GET, "/api/v1/emulators/catalog"),
        (&Method::POST, "/api/v1/emulators/{}/prepare"),
        (&Method::GET, "/api/v1/emulators/{}/firmware"),
        (&Method::POST, "/api/v1/emulators/{}/content"),
        (&Method::GET, "/api/v1/emulators/{}/saves"),
        (&Method::POST, "/api/v1/emulators/{}/saves/export"),
        (&Method::POST, "/api/v1/emulators/{}/saves/import"),
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
    // The handler scopes it to games this device launched, and refuses an expired device.
    if method == Method::POST && path == "/api/v1/game/end" {
        return true;
    }
    // Installing is what launching a missing title does anyway; the handler demands
    // `GRANT_LAUNCH`. Pause and remove demand `GRANT_MANAGE_GAMES`; cancel stays the operator's.
    if (method == Method::POST || method == Method::DELETE)
        && path_matches("/api/v1/library/install/{}", path)
    {
        return true;
    }
    if method == Method::POST && path_matches("/api/v1/library/install/{}/pause", path) {
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
                | "/api/v1/library/page"
        ) || path.starts_with("/api/v1/library/art/"))
}

/// Compare SHA-256 digests, not the strings — constant-time in the secret without a ct-eq
/// dependency.
pub(crate) fn token_eq(presented: &str, expected: &str) -> bool {
    Sha256::digest(presented.as_bytes()) == Sha256::digest(expected.as_bytes())
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
