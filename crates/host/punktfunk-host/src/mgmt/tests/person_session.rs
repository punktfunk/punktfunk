//! A person's session: opened on their own seat host from a ticket only their account could
//! sign, kept to that host, its loopback and its profile; and on the box, taken for that
//! profile only after its seat host said so.

use super::profile_view::{profiles_state, write_fixture, OWNER, SEAT_P};
use super::*;
use crate::mgmt::owner::{Answer, Forwarded, Seats, Target};
use crate::mgmt::person_session::{PROFILE_HEADER, SESSION_HEADER};
use crate::seats::Snapshot;
use std::sync::Mutex;

/// The seat host's person token.
const TOKEN: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
/// Another seat host's.
const THEIRS: &str = "6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b6b";

/// A ticket for [`SEAT_P`] signed with `token` now, `n` telling its nonce apart.
fn ticket(token: &str, n: u8) -> String {
    let now = crate::clock::unix_secs_u64();
    crate::person_token::sign(token, now, &format!("{n:032x}"), SEAT_P)
}

/// `req` as it comes, with no bearer added.
async fn bare(app: &Router, req: axum::http::Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// `req` from `peer`.
fn from(mut req: axum::http::Request<Body>, peer: &str) -> axum::http::Request<Body> {
    req.extensions_mut().insert(PeerAddr(peer.parse().unwrap()));
    req
}

fn open_req(ticket: &str) -> axum::http::Request<Body> {
    json_req(
        Method::POST,
        "/api/v1/auth/person/session",
        serde_json::json!({ "ticket": ticket }),
    )
}

/// A session on `app` for `ticket`: its token.
async fn open(app: &Router, ticket: &str) -> String {
    let (status, grant) = bare(app, open_req(ticket)).await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    assert_eq!(grant["profile"], SEAT_P);
    grant["token"].as_str().unwrap().to_string()
}

/// A ticket opens one session on its own seat host, from loopback, once. Another host's token,
/// a LAN peer and a second redemption get the one refusal.
#[tokio::test]
async fn a_ticket_opens_one_session_on_its_own_host() {
    let app = test_app_seat(test_state(), TOKEN, SEAT_P);
    let other = test_app_seat(test_state(), THEIRS, SEAT_P);
    let good = ticket(TOKEN, 1);
    let (status, refused) = bare(&other, open_req(&good)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "another seat host's ticket"
    );
    assert_eq!(refused["code"], "person_ticket_refused");
    let (status, _) = bare(&app, from(open_req(&good), "192.168.1.44:5000")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "from the LAN");
    open(&app, &good).await;
    let (status, again) = bare(&app, open_req(&good)).await;
    assert_eq!((status, again), (StatusCode::UNAUTHORIZED, refused));
    // A host with no person token opens nothing.
    let plain = test_app(test_state(), None);
    assert_eq!(
        bare(&plain, open_req(&ticket(TOKEN, 2))).await.0,
        StatusCode::UNAUTHORIZED
    );
}

/// A session token works from loopback on the host that opened it, for its own profile, until
/// it is closed or the host starts again.
#[tokio::test]
async fn a_session_is_its_host_s_its_loopback_s_and_its_profile_s() {
    let state = test_state();
    let app = test_app_seat(state.clone(), TOKEN, SEAT_P);
    let token = open(&app, &ticket(TOKEN, 1)).await;
    let status = || with_bearer(get_req("/api/v1/status"), &token);
    assert_eq!(bare(&app, status()).await.0, StatusCode::OK);
    let lan = from(status(), "192.168.1.44:5000");
    assert_eq!(bare(&app, lan).await.0, StatusCode::UNAUTHORIZED, "LAN");
    let other = test_app_seat(test_state(), TOKEN, SEAT_P);
    assert_eq!(
        bare(&other, status()).await.0,
        StatusCode::UNAUTHORIZED,
        "another host"
    );
    let theirs = with_bearer(get_req(&format!("/api/v1/library?as={OWNER}")), &token);
    assert_eq!(
        bare(&app, theirs).await.0,
        StatusCode::FORBIDDEN,
        "another profile"
    );
    // A start of the host is a new person auth with the same file: the session is gone.
    let restarted = test_app_seat(state, TOKEN, SEAT_P);
    assert_eq!(
        bare(&restarted, status()).await.0,
        StatusCode::UNAUTHORIZED,
        "restart"
    );

    let close = axum::http::Request::delete("/api/v1/auth/person/session")
        .body(Body::empty())
        .unwrap();
    let (code, _) = bare(&app, with_bearer(close, &token)).await;
    assert_eq!(code, StatusCode::NO_CONTENT);
    assert_eq!(
        bare(&app, status()).await.0,
        StatusCode::UNAUTHORIZED,
        "revoked"
    );
}

/// Every route the person lane reaches on its seat host, by the API document: a profile's data
/// and execution, and the host's name and status. A new route is closed until it has a scope,
/// and one with a profile scope changes this list.
#[test]
fn the_person_lane_reaches_what_is_pinned() {
    use crate::mgmt::person_session::{box_may_reach, may_reach};
    use std::collections::BTreeSet;
    const SEAT_REACH: &[(&str, &str)] = &[
        ("GET", "/api/v1/host"),
        ("GET", "/api/v1/status"),
        ("GET", "/api/v1/display/clients/{fingerprint}"),
        ("PUT", "/api/v1/display/clients/{fingerprint}"),
        ("DELETE", "/api/v1/display/clients/{fingerprint}"),
        ("GET", "/api/v1/display/targets"),
        ("GET", "/api/v1/downloads"),
        ("GET", "/api/v1/emulators"),
        ("GET", "/api/v1/emulators/catalog"),
        ("POST", "/api/v1/emulators/{id}/adopt"),
        ("POST", "/api/v1/emulators/{id}/content"),
        ("GET", "/api/v1/emulators/{id}/firmware"),
        ("POST", "/api/v1/emulators/{id}/install"),
        ("POST", "/api/v1/emulators/{id}/prepare"),
        ("POST", "/api/v1/emulators/{id}/remove"),
        ("GET", "/api/v1/emulators/{id}/saves"),
        ("POST", "/api/v1/emulators/{id}/saves/export"),
        ("POST", "/api/v1/emulators/{id}/saves/import"),
        ("POST", "/api/v1/game/end"),
        ("POST", "/api/v1/game/holds/{token}/release"),
        ("GET", "/api/v1/hooks"),
        ("PUT", "/api/v1/hooks"),
        ("GET", "/api/v1/library"),
        ("GET", "/api/v1/library/art/{id}/{kind}"),
        ("POST", "/api/v1/library/custom"),
        ("GET", "/api/v1/library/custom/{id}"),
        ("PUT", "/api/v1/library/custom/{id}"),
        ("DELETE", "/api/v1/library/custom/{id}"),
        ("GET", "/api/v1/library/own"),
        ("PUT", "/api/v1/library/hidden/{id}"),
        ("POST", "/api/v1/library/install/{id}"),
        ("DELETE", "/api/v1/library/install/{id}"),
        ("POST", "/api/v1/library/install/{id}/cancel"),
        ("POST", "/api/v1/library/install/{id}/pause"),
        ("GET", "/api/v1/library/metadata"),
        ("PUT", "/api/v1/library/metadata"),
        ("PUT", "/api/v1/library/metadata/{source}"),
        ("DELETE", "/api/v1/library/metadata/{source}"),
        ("GET", "/api/v1/library/overrides"),
        ("PUT", "/api/v1/library/overrides/games/{id}"),
        ("POST", "/api/v1/library/overrides/sets"),
        ("PUT", "/api/v1/library/overrides/sets/{id}"),
        ("DELETE", "/api/v1/library/overrides/sets/{id}"),
        ("PUT", "/api/v1/library/overrides/sources/{source}"),
        ("GET", "/api/v1/library/page"),
        ("PUT", "/api/v1/library/picks/{id}"),
        ("PUT", "/api/v1/library/provider/{provider}"),
        ("DELETE", "/api/v1/library/provider/{provider}"),
        ("PUT", "/api/v1/library/provider/{provider}/downloads"),
        ("PUT", "/api/v1/library/provider/{provider}/running"),
        ("GET", "/api/v1/library/scanners"),
        ("PUT", "/api/v1/library/scanners/{id}"),
        ("GET", "/api/v1/plugin-access"),
        ("GET", "/api/v1/plugin-access/requests"),
        ("POST", "/api/v1/plugin-access/requests"),
        ("POST", "/api/v1/plugin-access/{plugin}/decide"),
        ("POST", "/api/v1/plugin-access/{plugin}/release"),
        ("GET", "/api/v1/plugins"),
        ("POST", "/api/v1/plugins/logs"),
        ("PUT", "/api/v1/plugins/{id}"),
        ("DELETE", "/api/v1/plugins/{id}"),
        ("GET", "/api/v1/plugins/{id}/ui-credential"),
        ("GET", "/api/v1/plugins/{id}/ui/attach"),
        ("DELETE", "/api/v1/session/settings"),
        ("GET", "/api/v1/session/last"),
        ("GET", "/api/v1/session/settings"),
        ("PUT", "/api/v1/session/settings"),
        ("GET", "/api/v1/store/catalog"),
        ("POST", "/api/v1/store/install"),
        ("GET", "/api/v1/store/installed"),
        ("GET", "/api/v1/store/jobs"),
        ("GET", "/api/v1/store/jobs/{id}"),
        ("POST", "/api/v1/store/refresh"),
        ("GET", "/api/v1/store/runtime"),
        ("POST", "/api/v1/store/runtime"),
        ("GET", "/api/v1/store/sources"),
        ("PUT", "/api/v1/store/sources/{name}"),
        ("DELETE", "/api/v1/store/sources/{name}"),
        ("POST", "/api/v1/store/uninstall"),
    ];
    const BOX_REACH: &[(&str, &str)] = &[
        ("GET", "/api/v1/library"),
        ("GET", "/api/v1/library/page"),
        ("GET", "/api/v1/library/art/{id}/{kind}"),
        ("GET", "/api/v1/library/scanners"),
        ("GET", "/api/v1/library/overrides"),
        ("GET", "/api/v1/session/settings"),
        ("PUT", "/api/v1/library/hidden/{id}"),
        ("PUT", "/api/v1/library/overrides/games/{id}"),
        ("PUT", "/api/v1/library/scanners/{id}"),
        ("PUT", "/api/v1/session/settings"),
        ("DELETE", "/api/v1/session/settings"),
    ];
    let doc: serde_json::Value = serde_json::from_str(&openapi_json()).unwrap();
    let (mut seat, mut boxed) = (BTreeSet::new(), BTreeSet::new());
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for method in ops.as_object().unwrap().keys() {
            let m = Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let concrete = path
                .split('/')
                .map(|s| if s.starts_with('{') { "sample" } else { s })
                .collect::<Vec<_>>()
                .join("/");
            let row = (m.to_string(), path.clone());
            if may_reach(&m, &concrete) {
                seat.insert(row.clone());
            }
            if box_may_reach(&m, &concrete) {
                boxed.insert(row);
            }
        }
    }
    let rows = |r: &[(&str, &str)]| -> BTreeSet<_> {
        r.iter()
            .map(|(m, p)| (m.to_string(), p.to_string()))
            .collect()
    };
    assert_eq!(seat, rows(SEAT_REACH), "the seat host's person lane");
    assert_eq!(boxed, rows(BOX_REACH), "a person session on the box");
    assert!(!may_reach(&Method::GET, "/api/v1/library/../profiles"));
    assert!(!may_reach(&Method::GET, "/api/v1/profiles/x/proxy/hooks"));
    assert!(!may_reach(&Method::GET, "/api/v1/plugins/x/ui/index.html"));
}

/// Through the router, a session's call to a route off its lane is refused and runs nothing.
#[tokio::test]
async fn a_route_off_the_lane_answers_403() {
    let app = test_app_seat(test_state(), TOKEN, SEAT_P);
    let token = open(&app, &ticket(TOKEN, 1)).await;
    let verify = serde_json::json!({ "token": &token });
    for req in [
        get_req("/api/v1/profiles"),
        get_req("/api/v1/logs"),
        get_req("/api/v1/events"),
        get_req("/api/v1/native/clients"),
        json_req(
            Method::PATCH,
            "/api/v1/host/settings",
            serde_json::json!({}),
        ),
        json_req(Method::POST, "/api/v1/auth/person/verify", verify),
        get_req("/api/v1/profiles/a1b2c3d4e5f6/proxy/hooks"),
    ] {
        let path = req.uri().to_string();
        let (status, _) = bare(&app, with_bearer(req, &token)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
}

/// The box asks the seat host whose a session is, on the admin lane alone.
#[tokio::test]
async fn the_seat_host_answers_the_box_whose_a_session_is() {
    let app = test_app_seat(test_state(), TOKEN, SEAT_P);
    let token = open(&app, &ticket(TOKEN, 1)).await;
    let ask = |t: &str| {
        json_req(
            Method::POST,
            "/api/v1/auth/person/verify",
            serde_json::json!({ "token": t }),
        )
    };
    let (status, body) = send(&app, ask(&token)).await;
    assert_eq!(
        (status, &body["profile"]),
        (StatusCode::OK, &serde_json::json!(SEAT_P))
    );
    assert_eq!(send(&app, ask(THEIRS)).await.0, StatusCode::NOT_FOUND);
    assert_eq!(bare(&app, ask(&token)).await.0, StatusCode::UNAUTHORIZED);
    let plugin = with_bearer(ask(&token), "plugin-secret");
    assert_eq!(send(&app, plugin).await.0, StatusCode::FORBIDDEN);
}

/// A seat row of a profile, as the supervisor lists it.
fn row(id: &str, owner: bool) -> pf_seats::Seat {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": id, "account": "pf_seat", "display_slot": 12, "native_port": 9778,
        "mgmt_port": 47992, "owner": owner, "runtime": { "state": "running" },
    }))
    .unwrap()
}

/// [`SEAT_P`]'s seat, and one of another profile's.
const P_SEAT: &str = "0123456789abcdef0123456789abcdef";
const Q_SEAT: &str = "fedcba9876543210fedcba9876543210";
const Q: &str = "b1b2b3b4b5b6";
const OWNER_SEAT: &str = "00000000000000000000000000000000";

/// Seats where each seat host answers `verify` with what `live` says of its own sessions, and
/// the owner's host answers every call 200.
struct SessionSeats {
    snapshot: Arc<Snapshot>,
    /// Seat id → the session token live there and its profile.
    live: Vec<(&'static str, String, &'static str)>,
    calls: Mutex<Vec<(Target, Forwarded)>>,
}

impl Seats for SessionSeats {
    fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.snapshot)
    }

    fn ledger(&self) -> Snapshot {
        (*self.snapshot).clone()
    }

    fn ask(
        &self,
        _: pf_seats::ipc::Command,
    ) -> Result<pf_seats::ipc::CommandResult, pf_seats::ipc::ApiError> {
        Err(pf_seats::ipc::ApiError::new(
            pf_seats::ipc::ErrorCode::Transport,
            "no supervisor in this test",
        ))
    }

    fn call(&self, target: &Target, call: Forwarded) -> Answer {
        self.calls
            .lock()
            .unwrap()
            .push((target.clone(), call.clone()));
        let json = Some("application/json".to_string());
        let Target::Seat(seat) = target else {
            return Answer::Reply(200, json, b"{}".to_vec());
        };
        let asked: serde_json::Value = serde_json::from_slice(&call.body).unwrap_or_default();
        let hit = self.live.iter().find(|(s, token, _)| {
            s == seat && call.target == "auth/person/verify" && asked["token"] == *token
        });
        match hit {
            Some((.., profile)) => {
                let body = serde_json::json!({ "profile": profile }).to_string();
                Answer::Reply(200, json, body.into_bytes())
            }
            None => Answer::Reply(404, json, b"{}".to_vec()),
        }
    }
}

/// A door whose box has the owner, [`SEAT_P`] and `Q` with seats of their own; `live` as
/// [`SessionSeats::live`].
fn person_door(
    dir: &std::path::Path,
    live: Vec<(&'static str, String, &'static str)>,
) -> (Router, Arc<SessionSeats>) {
    write_fixture(dir);
    let p = |id: &str, account: serde_json::Value| serde_json::json!({"id": id, "display_name": id, "os_account": account});
    let profiles = serde_json::json!({"profiles": [
        p(OWNER, serde_json::json!({"kind": "operator"})),
        p(SEAT_P, serde_json::json!({"kind": "seat", "seat": P_SEAT})),
        p(Q, serde_json::json!({"kind": "seat", "seat": Q_SEAT})),
    ]});
    let state = profiles_state(dir, profiles);
    let snapshot = Snapshot {
        on: true,
        seats: vec![
            row(OWNER_SEAT, true),
            row(P_SEAT, false),
            row(Q_SEAT, false),
        ],
        ..Default::default()
    };
    let seats = Arc::new(SessionSeats {
        snapshot: Arc::new(snapshot),
        live,
        calls: Mutex::new(Vec::new()),
    });
    (test_app_door(state, None, seats.clone()), seats)
}

/// `req` as the console sends it for a person session: the token and profile, no bearer.
fn as_session(
    mut req: axum::http::Request<Body>,
    token: &str,
    profile: &str,
) -> axum::http::Request<Body> {
    let value = |v: &str| axum::http::HeaderValue::from_str(v).unwrap();
    req.headers_mut().insert(SESSION_HEADER, value(token));
    req.headers_mut().insert(PROFILE_HEADER, value(profile));
    req
}

/// The box takes "this is P's person" from P's seat host alone: it asks that seat whether the
/// token is live there and for P, every call, and only then passes the write to the owner's host
/// as P, with no device and no operator lane. Another seat's session, a dead one and the owner's
/// profile change nothing.
#[tokio::test]
async fn the_box_takes_a_person_session_only_from_its_seat_host() {
    let tmp = ConfigDirOverride::new();
    let mine = "1".repeat(64);
    let qs = "2".repeat(64);
    let live = vec![(P_SEAT, mine.clone(), SEAT_P), (Q_SEAT, qs.clone(), Q)];
    let (app, seats) = person_door(tmp.path(), live);
    let hide = |profile: &str| {
        json_req(
            Method::PUT,
            &format!("/api/v1/library/hidden/steam:400?as={profile}"),
            serde_json::json!({"hidden": true}),
        )
    };
    let (status, _) = bare(&app, as_session(hide(SEAT_P), &mine, SEAT_P)).await;
    assert_eq!(status, StatusCode::OK);
    let calls = seats.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].0, Target::Seat(P_SEAT.into()));
    assert_eq!(calls[0].1.target, "auth/person/verify");
    assert!(!calls[0].1.start, "a check wakes no seat");
    let (to, passed) = &calls[1];
    assert_eq!(*to, Target::Owner);
    assert_eq!(
        passed.target,
        format!("library/hidden/steam:400?as={SEAT_P}")
    );
    assert_eq!(
        (passed.device.as_deref(), passed.person.as_deref()),
        (None, Some(SEAT_P))
    );

    seats.calls.lock().unwrap().clear();
    let refused = [
        // Q's live session, presented as P's: P's seat knows no such token.
        as_session(hide(SEAT_P), &qs, SEAT_P),
        // A token no seat holds.
        as_session(hide(SEAT_P), &"3".repeat(64), SEAT_P),
    ];
    for req in refused {
        let (status, body) = bare(&app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["code"], "person_session_ended");
    }
    let asked = seats.calls.lock().unwrap().clone();
    assert!(
        asked.iter().all(|(t, _)| *t == Target::Seat(P_SEAT.into())),
        "{asked:?}"
    );
    seats.calls.lock().unwrap().clear();
    // The owner's profile has no person session, and no seat is asked about it.
    let (status, _) = bare(&app, as_session(hide(OWNER), &mine, OWNER)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(seats.calls.lock().unwrap().is_empty());
}

/// A person session names its profile in `as`, carries no bearer, comes from loopback and
/// reaches `PERSON` and the reads its pages need. Anything else is refused before any seat is
/// asked. Its library reads are the door's own.
#[tokio::test]
async fn a_person_session_on_the_box_reaches_its_view_alone() {
    let tmp = ConfigDirOverride::new();
    let mine = "1".repeat(64);
    let (app, seats) = person_door(tmp.path(), vec![(P_SEAT, mine.clone(), SEAT_P)]);
    let at = |path: &str| as_session(get_req(path), &mine, SEAT_P);
    for req in [
        at(&format!("/api/v1/library?as={Q}")),
        at("/api/v1/library"),
        at(&format!("/api/v1/profiles?as={SEAT_P}")),
        at(&format!("/api/v1/hooks?as={SEAT_P}")),
        with_bearer(at(&format!("/api/v1/library?as={SEAT_P}")), "test-secret"),
        as_session(
            json_req(
                Method::PUT,
                &format!("/api/v1/library/picks/steam:400?as={SEAT_P}"),
                serde_json::json!({"kind": "logo", "url": "https://p.example/l.png"}),
            ),
            &mine,
            SEAT_P,
        ),
    ] {
        let path = req.uri().to_string();
        assert_eq!(bare(&app, req).await.0, StatusCode::FORBIDDEN, "{path}");
    }
    let lan = from(
        at(&format!("/api/v1/library?as={SEAT_P}")),
        "192.168.1.44:5000",
    );
    assert_eq!(bare(&app, lan).await.0, StatusCode::UNAUTHORIZED);
    assert!(seats.calls.lock().unwrap().is_empty(), "a seat was asked");

    for path in ["/api/v1/library", "/api/v1/library/page"] {
        let (status, _) = bare(&app, at(&format!("{path}?as={SEAT_P}"))).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let calls = seats.calls.lock().unwrap().clone();
    assert!(calls.iter().all(|(t, _)| *t != Target::Owner), "{calls:?}");
    let (status, _) = bare(&app, at(&format!("/api/v1/library/scanners?as={SEAT_P}"))).await;
    assert_eq!(status, StatusCode::OK);
    let last = seats.calls.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.0, Target::Owner);
    assert_eq!(
        (last.1.person.as_deref(), last.1.start),
        (Some(SEAT_P), false)
    );
}

/// The owner's host takes the door's word for a person session with its own token alone: that
/// profile's view on the session's routes, never the owner's, never the operator's lane.
#[tokio::test]
async fn the_owner_s_host_takes_a_vouched_session_as_that_profile() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let trust = tempfile::tempdir().unwrap();
    let copy = trust.path().join("profiles.json");
    std::fs::write(&copy, super::profile_view::profiles().to_string()).unwrap();
    let state = test_state();
    let store = crate::profiles::Profiles::load_box(copy, Some("0".repeat(32)));
    let _ = state.profiles.set(Arc::new(store));
    let app = test_app(state, None);
    let vouch = |req: axum::http::Request<Body>, person: &str| {
        let mut req = req;
        let value = axum::http::HeaderValue::from_str(person).unwrap();
        req.headers_mut()
            .insert(crate::mgmt::person::VOUCH_HEADER, value);
        req
    };
    let hide = |profile: &str| {
        json_req(
            Method::PUT,
            &format!("/api/v1/library/hidden/steam:400?as={profile}"),
            serde_json::json!({"hidden": true}),
        )
    };
    let before = super::person::files(tmp.path());
    for (req, person) in [
        (hide(OWNER), OWNER),
        (hide(super::profile_view::SHARER), SEAT_P),
        (get_req("/api/v1/profiles"), SEAT_P),
        (get_req("/api/v1/hooks"), SEAT_P),
        (get_req("/api/v1/library/scanners"), SEAT_P),
    ] {
        let path = req.uri().to_string();
        let (status, _) = send(&app, vouch(req, person)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} as {person}");
    }
    let plugin = with_bearer(vouch(hide(SEAT_P), SEAT_P), "plugin-secret");
    assert_eq!(send(&app, plugin).await.0, StatusCode::FORBIDDEN);
    assert_eq!(super::person::files(tmp.path()), before, "nothing changed");
    let (status, j) = send(&app, vouch(hide(SEAT_P), SEAT_P)).await;
    assert_eq!(status, StatusCode::OK, "{j}");
    let after = super::person::files(tmp.path());
    let outside = super::person::outside;
    assert_eq!(outside(&after, SEAT_P), outside(&before, SEAT_P));
    assert_ne!(after, before);
    // What it adds stays bounded by the profile's library, as a device's does.
    let ghost = json_req(
        Method::PUT,
        &format!("/api/v1/library/hidden/steam:999999?as={SEAT_P}"),
        serde_json::json!({"hidden": true}),
    );
    assert_eq!(
        send(&app, vouch(ghost, SEAT_P)).await.0,
        StatusCode::NOT_FOUND
    );
}
