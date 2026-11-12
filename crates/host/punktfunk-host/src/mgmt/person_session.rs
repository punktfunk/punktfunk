//! A person on their own seat host, proven by being the account it runs as. `ctl console-url`
//! inside the seat signs a ticket with the person token only that account reads
//! (`crate::person_token`). This host redeems it once for a session token it keeps in memory,
//! and a call bearing that token from loopback is on the person lane ([`may_reach`]).
//!
//! The box takes a person session's word for a profile only after that profile's seat host says
//! the token is live and its own ([`vouched_by_seat`]). Then the session reaches `PERSON` and
//! the reads the person's pages need ([`box_may_reach`]), as that profile, never as the owner.

use super::owner::{Forwarded, Target};
use super::scope::{scope_of, Scope};
use super::shared::*;
use axum::http::{HeaderMap, Method};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The session token on a call the console sends the box for a person.
pub(crate) const SESSION_HEADER: &str = "x-punktfunk-person-session";
/// The profile that session is for.
pub(crate) const PROFILE_HEADER: &str = "x-punktfunk-person-profile";

/// The longest a session lives. It also ends with the host and on `DELETE`.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Live sessions one host keeps; past this the oldest ends.
const MAX_SESSIONS: usize = 16;
/// Refused redemptions in [`FAILURE_WINDOW`] after which every redemption waits it out.
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// How long a redeemed ticket is remembered: a ticket is fresh 60 s either side of its stamp.
const SPENT_FOR: Duration = Duration::from_secs(2 * crate::person_token::TICKET_TTL_SECS);

/// The host routes a person's pages read on their seat host: its name and its status.
pub(crate) const HOST_READS: &[(&str, &str)] =
    &[("GET", "/api/v1/host"), ("GET", "/api/v1/status")];

/// What the person's pages read of their profile on the box, beside the `PERSON` writes.
pub(crate) const BOX_READS: &[(&str, &str)] = &[
    ("GET", "/api/v1/library"),
    ("GET", "/api/v1/library/page"),
    ("GET", "/api/v1/library/art/{id}/{kind}"),
    ("GET", "/api/v1/library/scanners"),
    ("GET", "/api/v1/library/overrides"),
    ("GET", "/api/v1/session/settings"),
];

/// Whether the person lane reaches `method` `path` on its seat host: a profile's data or
/// execution, or one of [`HOST_READS`]. A route with no scope stays closed.
pub(crate) fn may_reach(method: &Method, path: &str) -> bool {
    match scope_of(method, path) {
        Some(Scope::ProfileData | Scope::ProfileExec) => true,
        Some(_) => listed(HOST_READS, method, path),
        None => false,
    }
}

/// Whether a person session reaches `method` `path` on the box: `PERSON`, and [`BOX_READS`].
pub(crate) fn box_may_reach(method: &Method, path: &str) -> bool {
    let person = super::person::person_route(method.as_str(), path);
    scope_of(method, path).is_some() && (person || listed(BOX_READS, method, path))
}

/// `method` `path` is a row of `rows`, `HEAD` as its `GET`.
fn listed(rows: &[(&str, &str)], method: &Method, path: &str) -> bool {
    let method = if *method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    let matches = punktfunk_core::mgmt_routes::path_matches;
    rows.iter().any(|(m, p)| *m == method && matches(p, path))
}

/// A seat host's person token and the sessions it opened.
pub(crate) struct PersonAuth {
    token: String,
    /// This host's own profile, asked at each redemption.
    profile: Box<dyn Fn() -> Option<String> + Send + Sync>,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// SHA-256 of each session token: the token itself is never kept.
    sessions: HashMap<[u8; 32], Session>,
    /// Each redeemed ticket and when.
    spent: HashMap<String, Instant>,
    failures: Vec<Instant>,
}

struct Session {
    profile: String,
    expires: Instant,
}

/// Why a ticket opened nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// Malformed, stale, spent, signed with another token, or for another profile.
    Ticket,
    /// Too many refused tickets just now.
    Wait,
}

/// A session a ticket opened.
#[derive(Serialize, ToSchema)]
pub(crate) struct PersonGrant {
    /// Present as `Authorization: Bearer <token>` to this seat host, or as
    /// `X-Punktfunk-Person-Session` to the box. 64 lowercase hex.
    pub(crate) token: String,
    /// Unix seconds; it ends sooner when this host stops or the session is closed.
    pub(crate) expires_at: i64,
    /// The profile it is for: this seat host's own.
    pub(crate) profile: String,
}

/// Never the token.
impl std::fmt::Debug for PersonGrant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PersonGrant")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl PersonAuth {
    /// `token` is this start's person token; `profile` answers this host's own profile.
    pub(crate) fn new(
        token: String,
        profile: impl Fn() -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            token,
            profile: Box::new(profile),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// A new session for `ticket`, at unix second `unix` and instant `now`: signed with this
    /// host's person token, fresh, never redeemed before, and for this host's own profile.
    pub(crate) fn redeem(
        &self,
        ticket: &str,
        unix: u64,
        now: Instant,
    ) -> Result<PersonGrant, Refused> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .failures
            .retain(|at| now.saturating_duration_since(*at) < FAILURE_WINDOW);
        if inner.failures.len() >= MAX_FAILURES {
            return Err(Refused::Wait);
        }
        inner
            .spent
            .retain(|_, at| now.saturating_duration_since(*at) < SPENT_FOR);
        let parsed = crate::person_token::parse(ticket);
        let own = (self.profile)();
        let good = parsed.as_ref().is_some_and(|t| {
            t.fresh(unix) && t.signed_by(&self.token) && own.as_deref() == Some(t.profile)
        });
        let (Some(t), true, false) = (parsed, good, inner.spent.contains_key(ticket)) else {
            inner.failures.push(now);
            return Err(Refused::Ticket);
        };
        inner.spent.insert(ticket.to_string(), now);
        inner.sessions.retain(|_, s| s.expires > now);
        while inner.sessions.len() >= MAX_SESSIONS {
            let oldest = inner.sessions.iter().min_by_key(|(_, s)| s.expires);
            let Some(key) = oldest.map(|(k, _)| *k) else {
                break;
            };
            inner.sessions.remove(&key);
        }
        let token = crate::mgmt_token::random_token();
        let session = Session {
            profile: t.profile.to_string(),
            expires: now + SESSION_TTL,
        };
        inner.sessions.insert(digest(&token), session);
        Ok(PersonGrant {
            token,
            expires_at: crate::clock::unix_secs() + SESSION_TTL.as_secs() as i64,
            profile: t.profile.to_string(),
        })
    }

    /// The profile of the live session `token` names at `now`.
    pub(crate) fn session(&self, token: &str, now: Instant) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let session = inner.sessions.get(&digest(token))?;
        (session.expires > now).then(|| session.profile.clone())
    }

    /// Ends the session `token` names, if there is one.
    pub(crate) fn revoke(&self, token: &str) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.sessions.remove(&digest(token)).is_some()
    }
}

/// The ticket `ctl console-url` wrote into the page it opened.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PersonTicket {
    /// `<unix seconds>.<nonce>.<profile id>.<HMAC-SHA256>`, as the page's `ticket` carries it.
    ticket: String,
}

/// A session token the box asks about.
#[derive(Deserialize, ToSchema)]
pub(crate) struct PersonSessionToken {
    token: String,
}

/// Whose a live session is.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct PersonOf {
    pub(crate) profile: String,
}

/// The refusal of a ticket, one for every reason.
fn ticket_refused() -> Response {
    api_error_coded(
        StatusCode::UNAUTHORIZED,
        "person_ticket_refused",
        "Your Punktfunk didn't open. Open it again from your desktop.",
    )
}

/// `body` as a `T`, else nothing: a body that does not parse is never echoed.
fn parsed<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<T> {
    serde_json::from_slice(body).ok()
}

/// Open a person session
///
/// On a seat host, from loopback: trade the one-time ticket `ctl console-url` wrote into the
/// page it opened for a session token. The ticket is signed with this host's person token, at
/// most 60 s old, used once, and for this host's own profile. Every other host refuses it. Ten
/// refused tickets in a minute make the next wait out that minute.
#[utoipa::path(
    post,
    path = "/auth/person/session",
    tag = "host",
    operation_id = "openPersonSession",
    security(()),
    request_body = PersonTicket,
    responses(
        (status = OK, description = "A session for the person lane of this seat host", body = PersonGrant),
        (status = UNAUTHORIZED, description = "Not a ticket this host takes, or not from loopback", body = ApiError),
        (status = TOO_MANY_REQUESTS, description = "Too many refused tickets in the last minute", body = ApiError),
    )
)]
pub(crate) async fn open_person_session(
    State(st): State<Arc<MgmtState>>,
    body: axum::body::Bytes,
) -> Response {
    let Some(person) = st.person.as_ref() else {
        return ticket_refused();
    };
    let Some(PersonTicket { ticket }) = parsed(&body) else {
        return ticket_refused();
    };
    match person.redeem(&ticket, crate::clock::unix_secs_u64(), Instant::now()) {
        Ok(grant) => {
            tracing::info!(profile = %grant.profile, "management API: person session opened");
            Json(grant).into_response()
        }
        Err(Refused::Wait) => api_error_coded(
            StatusCode::TOO_MANY_REQUESTS,
            "person_ticket_wait",
            "Your Punktfunk didn't open after too many tries. Wait a minute, then open it again \
             from your desktop.",
        ),
        Err(Refused::Ticket) => {
            tracing::info!("management API: person ticket refused");
            ticket_refused()
        }
    }
}

/// End a person session
///
/// From loopback, the session's own token as the bearer. Answers the same whether it was live.
#[utoipa::path(
    delete,
    path = "/auth/person/session",
    tag = "host",
    operation_id = "closePersonSession",
    security(()),
    responses(
        (status = NO_CONTENT, description = "The session, if it was one, is over"),
        (status = UNAUTHORIZED, description = "Not from loopback", body = ApiError),
    )
)]
pub(crate) async fn close_person_session(
    State(st): State<Arc<MgmtState>>,
    headers: HeaderMap,
) -> StatusCode {
    let token = super::auth::bearer_of(&headers);
    if let (Some(person), Some(token)) = (st.person.as_ref(), token) {
        if person.revoke(token) {
            tracing::info!("management API: person session closed");
        }
    }
    StatusCode::NO_CONTENT
}

/// Whose a person session is
///
/// Admin lane, on a seat host: whether `token` is a live person session here, and for which
/// profile. The box asks before it takes a person session's word for that profile.
#[utoipa::path(
    post,
    path = "/auth/person/verify",
    tag = "host",
    operation_id = "verifyPersonSession",
    request_body = PersonSessionToken,
    responses(
        (status = OK, description = "A live session, and its profile", body = PersonOf),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No live session with that token here", body = ApiError),
    )
)]
pub(crate) async fn verify_person_session(
    State(st): State<Arc<MgmtState>>,
    body: axum::body::Bytes,
) -> Response {
    let live = st.person.as_ref().zip(parsed::<PersonSessionToken>(&body));
    match live.and_then(|(person, req)| person.session(&req.token, Instant::now())) {
        Some(profile) => Json(PersonOf { profile }).into_response(),
        None => api_error(
            StatusCode::NOT_FOUND,
            "no live person session with that token here",
        ),
    }
}

/// The answer to a person session the box can't take: no live session for that profile.
pub(crate) fn session_ended() -> Response {
    api_error_coded(
        StatusCode::UNAUTHORIZED,
        "person_session_ended",
        "Your Punktfunk session ended. Open it again from your desktop.",
    )
}

/// The session token and profile a call to the box carries, when it carries both.
pub(crate) fn presented(headers: &HeaderMap) -> Option<(&str, &str)> {
    let get = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
    Some((get(SESSION_HEADER)?, get(PROFILE_HEADER)?))
}

/// `profile`, once its seat host answered that `token` is a live session there for it. Never
/// the owner, and never a profile with no seat of its own. Wakes no seat.
pub(crate) async fn vouched_by_seat(st: &MgmtState, profile: &str, token: &str) -> Option<String> {
    let profiles = st.app.profiles.get()?;
    let owner = profiles.owner_id();
    if !crate::library::is_profile_id(profile) || owner.as_deref() == Some(profile) {
        return None;
    }
    let crate::profiles::OsAccount::Seat { seat: Some(seat) } = profiles.get(profile)?.os_account
    else {
        return None;
    };
    let call = Forwarded {
        method: "POST".into(),
        target: "auth/person/verify".into(),
        content_type: Some("application/json".into()),
        body: serde_json::to_vec(&serde_json::json!({ "token": token })).ok()?,
        device: None,
        person: None,
        stream: false,
        start: false,
    };
    let answer = st.seat_call(Target::Seat(seat), call).await.ok_body()?;
    let PersonOf { profile: theirs } = parsed(&answer)?;
    (theirs == profile).then_some(theirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
    const OTHER: &str = "1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e";
    const MINE: &str = "a1b2c3d4e5f6";
    const NONCE: &str = "00112233445566778899aabbccddeeff";

    fn auth(token: &str) -> PersonAuth {
        PersonAuth::new(token.into(), || Some(MINE.to_string()))
    }

    fn ticket(token: &str, ts: u64, nonce: &str, profile: &str) -> String {
        crate::person_token::sign(token, ts, nonce, profile)
    }

    /// A ticket opens one session, once, within 60 s of its stamp, on the host whose token
    /// signed it and for that host's own profile.
    #[test]
    fn a_ticket_is_single_use_fresh_and_its_host_s_own() {
        let (host, now) = (auth(TOKEN), Instant::now());
        let good = ticket(TOKEN, 1_000, NONCE, MINE);
        let grant = host.redeem(&good, 1_030, now).unwrap();
        assert_eq!(grant.profile, MINE);
        assert_eq!(host.session(&grant.token, now).as_deref(), Some(MINE));
        assert_eq!(host.redeem(&good, 1_030, now).unwrap_err(), Refused::Ticket);

        let fresh = |nonce: &str| ticket(TOKEN, 1_000, nonce, MINE);
        let nonce = |n: u8| format!("{n:032x}");
        assert!(
            host.redeem(&fresh(&nonce(1)), 1_061, now).is_err(),
            "expired"
        );
        assert!(
            host.redeem(&fresh(&nonce(2)), 939, now).is_err(),
            "from the future"
        );
        let another = ticket(OTHER, 1_000, &nonce(3), MINE);
        assert!(host.redeem(&another, 1_000, now).is_err(), "another host's");
        let theirs = ticket(TOKEN, 1_000, &nonce(4), "f6e5d4c3b2a1");
        assert!(host.redeem(&theirs, 1_000, now).is_err(), "another profile");
        assert!(host.redeem(&fresh(&nonce(5)), 1_060, now).is_ok());
        let none = PersonAuth::new(TOKEN.into(), || None);
        assert!(
            none.redeem(&fresh(&nonce(6)), 1_000, now).is_err(),
            "no profile here"
        );
    }

    /// Ten refused tickets in a minute refuse the next one, good or not, until the minute passes.
    #[test]
    fn refused_tickets_wait_out_a_minute() {
        let (host, now) = (auth(TOKEN), Instant::now());
        for n in 0..MAX_FAILURES {
            let bad = ticket(OTHER, 1_000, &format!("{n:032x}"), MINE);
            assert_eq!(host.redeem(&bad, 1_000, now).unwrap_err(), Refused::Ticket);
        }
        let good = ticket(TOKEN, 1_000, NONCE, MINE);
        assert_eq!(host.redeem(&good, 1_000, now).unwrap_err(), Refused::Wait);
        let later = now + FAILURE_WINDOW;
        assert!(host.redeem(&good, 1_000, later).is_ok());
    }

    /// A session lives 12 hours at most, ends when it is revoked, and is no other's.
    #[test]
    fn a_session_ends_on_revocation_and_after_12_hours() {
        let (host, now) = (auth(TOKEN), Instant::now());
        let grant = host
            .redeem(&ticket(TOKEN, 1_000, NONCE, MINE), 1_000, now)
            .unwrap();
        assert!(host
            .session(&grant.token, now + SESSION_TTL - Duration::from_secs(1))
            .is_some());
        assert!(host.session(&grant.token, now + SESSION_TTL).is_none());
        assert!(host.session(OTHER, now).is_none());
        assert!(host.revoke(&grant.token));
        assert!(host.session(&grant.token, now).is_none());
        assert!(!host.revoke(&grant.token));
        // A host that starts again knows none of the sessions before.
        assert!(auth(TOKEN).session(&grant.token, now).is_none());
    }

    /// The token `ctl console-url` reads back from the file the host minted signs a ticket that
    /// host redeems.
    #[cfg(unix)]
    #[test]
    fn ctl_s_ticket_opens_its_host_s_session() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let minted = crate::person_token::mint_in(dir.path()).unwrap();
        let host = auth(&minted);
        let read = crate::person_token::read_in(dir.path()).unwrap();
        let unix = crate::clock::unix_secs_u64();
        let ticket = crate::person_token::sign(&read, unix, NONCE, MINE);
        assert!(host.redeem(&ticket, unix, Instant::now()).is_ok());
    }
}
