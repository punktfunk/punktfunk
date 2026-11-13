//! The door as a router: what it answers itself, what it passes to the owner's host, and
//! what a device's call reaches as that device. The seats are a fake that records every call.

use super::*;
use crate::mgmt::owner::{Answer, Forwarded, Seats, Target};
use crate::seats::Snapshot;
use std::collections::BTreeMap;
use std::sync::Mutex;

const OWNER_SEAT: &str = "0123456789abcdef0123456789abcdef";
const KID_SEAT: &str = "fedcba9876543210fedcba9876543210";

/// A seat row as the supervisor lists it; `owner` marks the box owner's.
fn seat(id: &str, owner: bool, mgmt_port: u16) -> pf_seats::Seat {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": if owner { "Enrico" } else { "Kid" }, "account": "pf_seat",
        "display_slot": 12, "native_port": 9778, "mgmt_port": mgmt_port, "owner": owner,
        "runtime": { "state": "running" },
    }))
    .unwrap()
}

/// Seats that answer what they were told to and remember what was asked.
struct FakeSeats {
    snapshot: Arc<Snapshot>,
    /// Target path → status and JSON body.
    answers: BTreeMap<String, (u16, String)>,
    calls: Mutex<Vec<(Target, Forwarded)>>,
    /// The supervisor lists no owner row.
    no_owner: bool,
}

impl FakeSeats {
    fn new(snapshot: Snapshot) -> Arc<Self> {
        Arc::new(FakeSeats {
            snapshot: Arc::new(snapshot),
            answers: BTreeMap::new(),
            calls: Mutex::new(Vec::new()),
            no_owner: false,
        })
    }

    fn without_owner(mut self: Arc<Self>) -> Arc<Self> {
        Arc::get_mut(&mut self).unwrap().no_owner = true;
        self
    }

    fn answering(
        mut self: Arc<Self>,
        target: &str,
        status: u16,
        body: serde_json::Value,
    ) -> Arc<Self> {
        Arc::get_mut(&mut self)
            .unwrap()
            .answers
            .insert(target.into(), (status, body.to_string()));
        self
    }

    fn calls(&self) -> Vec<(Target, Forwarded)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Seats for FakeSeats {
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
        let stream = call.stream;
        let path = call.target.clone();
        self.calls.lock().unwrap().push((target.clone(), call));
        if self.no_owner && *target == Target::Owner {
            return Answer::NoSeat;
        }
        if stream {
            return Answer::Stream(
                200,
                Some("text/event-stream".into()),
                Body::from("event: pad.state\ndata: {}\n\n"),
            );
        }
        match self.answers.get(&path) {
            Some((status, body)) => Answer::Reply(
                *status,
                Some("application/json".into()),
                body.clone().into_bytes(),
            ),
            None => Answer::Reply(404, None, Vec::new()),
        }
    }
}

/// A box with the owner's row running and one seat of its own, `device` playing on the kid's.
fn box_snapshot(device: Option<&str>) -> Snapshot {
    let mut occupants = BTreeMap::new();
    let mut statuses = BTreeMap::new();
    if let Some(fp) = device {
        let row = serde_json::json!({
            "id": 3, "plane": "native", "phase": "streaming", "client": &fp[..12],
            "client_name": "Kid's Deck", "mode": "1280x800@60", "hdr": false, "join": false,
            "muted": false, "pads": [], "uptime_s": 10,
        });
        statuses.insert(
            KID_SEAT.to_string(),
            serde_json::json!({
                "video_streaming": true, "audio_streaming": true, "active_sessions": 1,
                "sessions": [row], "games": [{"session_id": 3, "client": &fp[..12], "title": "Hades", "state": "running"}],
                "downloads": [{"id": "steam:1145360", "title": "Hades"}],
                "session": {"width": 1280, "height": 800, "fps": 60},
            }),
        );
        occupants.insert(
            KID_SEAT.to_string(),
            vec![crate::seats::Occupant {
                client: fp[..12].to_string(),
                name: Some("Kid's Deck".into()),
            }],
        );
    }
    Snapshot {
        on: true,
        single_session: false,
        seats: vec![seat(OWNER_SEAT, true, 47991), seat(KID_SEAT, false, 47992)],
        occupants,
        statuses,
    }
}

/// A pairing store with `fps` paired, full control each.
fn paired(fps: &[&str]) -> Arc<crate::native_pairing::NativePairing> {
    let np = crate::native_pairing::NativePairing::load_with(
        Some(crate::test_support::scratch().join("pf-door.json")),
        None,
        false,
    )
    .unwrap();
    for (n, fp) in fps.iter().enumerate() {
        np.add(&format!("device {n}"), fp).unwrap();
    }
    Arc::new(np)
}

fn admin_door(seats: Arc<FakeSeats>) -> Router {
    test_app_door(test_state(), None, seats)
}

#[tokio::test]
async fn the_door_passes_an_admin_call_to_the_owner_s_host() {
    let seats = FakeSeats::new(box_snapshot(None)).answering(
        "gpus",
        200,
        serde_json::json!({"gpus": [{"id": "amd"}]}),
    );
    let app = admin_door(seats.clone());
    let (status, body) = send(&app, get_req("/api/v1/gpus")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["gpus"][0]["id"], "amd",
        "the owner's answer, as it came"
    );
    let calls = seats.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Target::Owner);
    assert_eq!(calls[0].1.method, "GET");
    assert_eq!(calls[0].1.target, "gpus");
    assert_eq!(calls[0].1.device, None);
    assert!(calls[0].1.start, "a console call wakes the owner's session");
}

/// With no owner row the owner's calls are refused, never answered from the door's own files.
#[tokio::test]
async fn without_an_owner_row_the_owner_s_calls_are_refused() {
    let seats = FakeSeats::new(box_snapshot(None)).without_owner();
    let app = admin_door(seats.clone());
    let (status, _) = send(&app, get_req("/api/v1/hooks")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let patch = json_req(
        Method::PATCH,
        "/api/v1/host/settings",
        serde_json::json!({"encoder": "auto"}),
    );
    let (status, _) = send(&app, patch).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "an owner's row");
}

#[tokio::test]
async fn the_door_answers_its_own_routes_itself() {
    let seats = FakeSeats::new(box_snapshot(None));
    let app = admin_door(seats.clone());
    for own in [
        "/api/v1/native/pending",
        "/api/v1/profiles",
        "/api/v1/actions",
    ] {
        let (status, _) = send(&app, get_req(own)).await;
        assert_ne!(status, StatusCode::BAD_GATEWAY, "{own} is the door's");
    }
    assert!(seats.calls().is_empty(), "nothing left the door");
}

/// A plugin's token holds on the door; its calls never carry to another host.
#[tokio::test]
async fn a_plugin_s_call_never_leaves_the_door() {
    let seats = FakeSeats::new(box_snapshot(None));
    let app = admin_door(seats.clone());
    let req = with_bearer(get_req("/api/v1/gpus"), "plugin-secret");
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(seats.calls().is_empty());
}

#[tokio::test]
async fn a_dot_segment_never_leaves_the_door() {
    let seats = FakeSeats::new(box_snapshot(None));
    let app = admin_door(seats.clone());
    let (status, _) = send(&app, get_req("/api/v1/plugins/x/ui/%2e%2e/native/clients")).await;
    assert_ne!(status, StatusCode::OK);
    assert!(seats.calls().is_empty());
}

/// The device plays on the kid's seat: its End game reaches that host, vouched as the device.
#[tokio::test]
async fn a_device_s_own_play_reaches_its_seat_as_that_device() {
    let fp = "aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233";
    let np = paired(&[fp]);
    let seats = FakeSeats::new(box_snapshot(Some(fp))).answering(
        "game/end",
        200,
        serde_json::json!({"ended": true}),
    );
    let app = test_app_door(test_state(), Some(np), seats.clone());
    let mut req = json_req(Method::POST, "/api/v1/game/end", serde_json::json!({}));
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let calls = seats.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Target::Seat(KID_SEAT.into()));
    assert_eq!(calls[0].1.device.as_deref(), Some(fp));
    assert_eq!(calls[0].1.target, "game/end");
}

/// Its screen picker reads the screens of the seat it plays on, as that device, and wakes no
/// seat: the door has no screens of its own.
#[tokio::test]
async fn a_device_s_screen_picker_reads_its_seat() {
    let fp = "aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233";
    let np = paired(&[fp]);
    let seats = FakeSeats::new(box_snapshot(Some(fp))).answering(
        "display/targets",
        200,
        serde_json::json!({"targets": []}),
    );
    let app = test_app_door(test_state(), Some(np), seats.clone());
    let mut req = get_req("/api/v1/display/targets");
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let calls = seats.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Target::Seat(KID_SEAT.into()));
    assert_eq!(calls[0].1.device.as_deref(), Some(fp));
    assert!(!calls[0].1.start, "a device's read wakes no seat");
}

/// A device's install, pause and removal reach the owner's host as that device, wherever it
/// plays: the owner's plugins download into the box's shared game files.
#[tokio::test]
async fn a_device_s_install_reaches_the_owner_s_host_as_that_device() {
    let fp = "aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233";
    let np = paired(&[fp]);
    for plays in [Some(fp), None] {
        let seats = FakeSeats::new(box_snapshot(plays)).answering(
            "library/install/steam:570",
            202,
            serde_json::json!({"id": "steam:570"}),
        );
        let app = test_app_door(test_state(), Some(np.clone()), seats.clone());
        let calls = [
            (Method::POST, "/api/v1/library/install/steam:570"),
            (Method::DELETE, "/api/v1/library/install/steam:570"),
            (Method::POST, "/api/v1/library/install/steam:570/pause"),
        ];
        for (method, path) in calls {
            let mut req = axum::http::Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(PeerCertFingerprint(Some(fp.to_string())));
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_ne!(resp.status(), StatusCode::CONFLICT, "{path}");
        }
        let calls = seats.calls();
        assert_eq!(calls.len(), 3);
        for (target, call) in &calls {
            assert_eq!(*target, Target::Owner, "{}", call.target);
            assert_eq!(call.device.as_deref(), Some(fp), "never as the operator");
        }
        assert_eq!(calls[0].1.method, "POST");
        assert_eq!(calls[2].1.target, "library/install/steam:570/pause");
    }
}

/// The owner's host answers a vouched install as the device: its grant decides, and a device
/// it holds no pairing for gets nothing. The admin token alone is the operator.
#[tokio::test]
async fn a_vouched_install_is_the_device_s_never_the_operator_s() {
    use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_LAUNCH};
    let (pad, deck) = (
        "1111111111111111111111111111111111111111111111111111111111111111",
        "2222222222222222222222222222222222222222222222222222222222222222",
    );
    let unpaired = "3333333333333333333333333333333333333333333333333333333333333333";
    let np = crate::native_pairing::NativePairing::load_with(
        Some(crate::test_support::scratch().join("pf-vouched.json")),
        None,
        false,
    )
    .unwrap();
    let access = |grants| {
        Some(crate::native_pairing::Access {
            grants,
            expires_unix: None,
            until_disconnect: false,
        })
    };
    np.add_with_access("pad", pad, access(GRANT_GAMEPAD))
        .unwrap();
    np.add_with_access("deck", deck, access(GRANT_GAMEPAD | GRANT_LAUNCH))
        .unwrap();
    let app = test_app_native(test_state(), Arc::new(np));
    let install = |device: Option<&str>| {
        let mut req = axum::http::Request::post("/api/v1/library/install/custom:no-such")
            .body(Body::empty())
            .unwrap();
        if let Some(fp) = device {
            let value = axum::http::HeaderValue::from_str(fp).unwrap();
            req.headers_mut()
                .insert(crate::mgmt::auth::VOUCH_HEADER, value);
        }
        req
    };
    let (status, body) = send(&app, install(Some(pad))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["code"], "device_not_allowed_to_start_games");
    let (status, _) = send(&app, install(Some(deck))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "past the grant");
    let (status, _) = send(&app, install(Some(unpaired))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(&app, install(None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the operator");
}

/// The door answers a profile's view of the library from its own copy, for the console and
/// for a device alike.
#[tokio::test]
async fn the_door_answers_a_profile_s_library_itself() {
    let fp = "aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233";
    let seats = FakeSeats::new(box_snapshot(None));
    let tmp = tempfile::tempdir().unwrap();
    let row = |id: &str| serde_json::json!({"id": id, "display_name": id, "os_account": {"kind": "operator"}});
    let profiles = serde_json::json!({"profiles": [row("000000000001"), row("a1a1a1a1a1a1")]});
    let state = super::profile_view::profiles_state(tmp.path(), profiles);
    let app = test_app_door(state, Some(paired(&[fp])), seats.clone());
    for path in [
        "/api/v1/library?as=a1a1a1a1a1a1",
        "/api/v1/library/page?as=a1a1a1a1a1a1",
    ] {
        let (status, _) = send(&app, get_req(path)).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        let mut req = get_req(path);
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}, a device");
    }
    assert!(seats.calls().is_empty(), "nothing left the door");
}

/// The seats' sessions, games and downloads are on the door's `/status`; a device sees the
/// others' rows without their identity.
#[tokio::test]
async fn status_carries_the_seats_sessions() {
    let fp = "aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233";
    let other = "0000000000000000000000000000000000000000000000000000000000000000";
    let np = paired(&[fp, other]);
    let seats = FakeSeats::new(box_snapshot(Some(fp)));
    let app = test_app_door(test_state(), Some(np), seats.clone());

    // Other tests register sessions in this process: look for the seat's rows, not at [0].
    let rows = |body: &serde_json::Value, key: &str| -> Vec<serde_json::Value> {
        body[key].as_array().cloned().unwrap_or_default()
    };
    let (status, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["active_sessions"].as_u64().unwrap() >= 1);
    assert!(rows(&body, "sessions")
        .iter()
        .any(|r| r["client_name"] == "Kid's Deck"));
    assert!(rows(&body, "games").iter().any(|r| r["title"] == "Hades"));
    assert!(rows(&body, "downloads")
        .iter()
        .any(|r| r["id"] == "steam:1145360"));
    assert_eq!(body["video_streaming"], true);
    assert!(
        !body["session"].is_null(),
        "a representative, as on one host"
    );
    assert!(
        seats.calls().is_empty(),
        "status is the door's, from its snapshot"
    );

    let cert = |fp: &str| {
        let mut req = get_req("/api/v1/status");
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        req
    };
    let resp = app.clone().oneshot(cert(fp)).await.unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        rows(&body, "games").iter().any(|r| r["title"] == "Hades"),
        "its own game, to resume"
    );
    let resp = app.clone().oneshot(cert(other)).await.unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(
        body["active_sessions"].as_u64().unwrap() >= 1,
        "the row counts"
    );
    let kid = rows(&body, "sessions")
        .into_iter()
        .find(|r| r["id"] == 3)
        .expect("the seat's row");
    assert_eq!(kid["client"], "", "without the other device's identity");
    assert!(kid.get("client_name").is_none());
}

#[tokio::test]
async fn diagnostics_merge_the_owner_s_report() {
    let seats = FakeSeats::new(box_snapshot(None)).answering(
        "diagnostics",
        200,
        serde_json::json!({"ran_at_unix": 5, "checks": [{
            "id": "plugin_sandbox", "status": "fail", "severity": "critical",
            "summary": "no bwrap", "impact": "no plugins", "params": {}, "source": "startup",
        }]}),
    );
    let app = admin_door(seats.clone());
    let (status, body) = send(&app, get_req("/api/v1/diagnostics")).await;
    assert_eq!(status, StatusCode::OK);
    let sandbox = body["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "plugin_sandbox")
        .expect("the owner's check");
    assert_eq!(sandbox["status"], "fail");
    assert!(!seats.calls()[0].1.start, "a health check wakes nobody");
}

#[tokio::test]
async fn host_settings_show_the_owner_s_rows_beside_the_door_s() {
    let seats = FakeSeats::new(box_snapshot(None)).answering(
        "host/settings",
        200,
        serde_json::json!({"settings": [{"id": "idle_timeout_ms", "title": "owner"}],
                           "restart_pending": [], "env_file": "/home/me/.config/punktfunk/host.env"}),
    );
    let app = admin_door(seats.clone());
    let (status, body) = send(&app, get_req("/api/v1/host/settings")).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["settings"].as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["id"] == "mdns" && r["title"] != "owner"),
        "the door's"
    );
    assert!(rows
        .iter()
        .any(|r| r["id"] == "idle_timeout_ms" && r["title"] == "owner"));
    assert_eq!(body["env_file"], "/home/me/.config/punktfunk/host.env");
}

/// A door row the door won't take refuses the whole patch before the owner's host stores any.
#[tokio::test]
async fn a_bad_door_row_stores_none_of_the_owner_s() {
    let seats = FakeSeats::new(box_snapshot(None)).answering(
        "host/settings",
        200,
        serde_json::json!({"settings": [], "restart_pending": []}),
    );
    let app = admin_door(seats.clone());
    let patch = json_req(
        Method::PATCH,
        "/api/v1/host/settings",
        serde_json::json!({"encoder": "nvenc", "mdns": "bogus"}),
    );
    let (status, _) = send(&app, patch).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(seats.calls().is_empty(), "the owner's host was not asked");
}

/// A tail from the owner's host reaches the console as it arrives, not as one buffered answer.
#[tokio::test]
async fn a_tail_streams_through_the_door() {
    let seats = FakeSeats::new(box_snapshot(None));
    let app = admin_door(seats.clone());
    let req = with_bearer(get_req("/api/v1/session/7/pads"), "test-secret");
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap(),
        "text/event-stream"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.starts_with(b"event: pad.state"));
    assert!(seats.calls()[0].1.stream);
}

/// A relayed event joins this host's bus; only the admin token may put one there.
#[tokio::test]
async fn a_relayed_event_joins_the_bus() {
    let app = test_app(test_state(), None);
    // The bus is the process's: other tests emit on it too, so look for this one event.
    let mut rx = crate::events::bus().subscribe_live();
    let ev = serde_json::json!({"event": {"seq": 1, "ts_ms": 1, "schema": 1,
        "kind": "pairing.denied",
        "device": {"name": "Deck", "fingerprint": "relayed-ab", "plane": "native"}}});
    let (status, _) = send(
        &app,
        json_req(Method::POST, "/api/v1/events/relay", ev.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut seen = false;
    while let Ok(got) = rx.try_recv() {
        if let crate::events::EventKind::PairingDenied { device, .. } = &got.kind {
            seen |= device.fingerprint == "relayed-ab";
        }
    }
    assert!(seen, "the relayed event is on the bus");

    // A seat's game keeps its seat, so this host's plugins know whose it is.
    let game = serde_json::json!({"event": {"seq": 1, "ts_ms": 1, "schema": 1,
        "seat": "9a3f1c2b7e40", "kind": "game.exited", "reason": "exited",
        "game": {"title": "relayed-game", "client": "deck", "plane": "native"}}});
    let (status, _) = send(&app, json_req(Method::POST, "/api/v1/events/relay", game)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut seat = None;
    while let Ok(got) = rx.try_recv() {
        if let crate::events::EventKind::GameExited { game, .. } = &got.kind
            && game.title == "relayed-game"
        {
            seat = got.seat.clone();
        }
    }
    assert_eq!(seat.as_deref(), Some("9a3f1c2b7e40"));

    let req = with_bearer(
        json_req(Method::POST, "/api/v1/events/relay", ev),
        "plugin-secret",
    );
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// On a door a Moonlight device is paired with the owner's host, so adding one to a profile's
/// lock asks that host's list, once, and takes its label; a fingerprint neither lists is refused.
#[tokio::test]
async fn a_door_adds_a_moonlight_device_the_owner_s_host_paired() {
    let moonlight = "ef".repeat(32);
    let seats = FakeSeats::new(box_snapshot(None)).answering(
        "clients",
        200,
        serde_json::json!([{"fingerprint": moonlight.to_uppercase(), "label": "Living room"}]),
    );
    let state = test_state();
    let path = crate::test_support::scratch().join("profiles.json");
    let profiles = crate::profiles::Profiles::load_with(Some(path), None);
    profiles.ensure_owner("box").unwrap();
    let owner = profiles.owner_id().unwrap();
    let _ = state.profiles.set(Arc::new(profiles));
    let app = test_app_door(state, None, seats.clone());
    let devices = format!("/api/v1/profiles/{owner}/devices");
    let add = |fp: &str| {
        json_req(
            Method::POST,
            &devices,
            serde_json::json!({"fingerprint": fp}),
        )
    };
    let (status, body) = send(&app, add(&moonlight)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["fingerprint"], moonlight.as_str());
    assert_eq!(body[0]["name"], "Living room");
    assert_eq!(
        body[0]["moonlight"], true,
        "the list tags it from the owner's"
    );
    let (status, body) = send(&app, add(&"cd".repeat(32))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "profile_device_not_paired");
    let asked: Vec<_> = seats
        .calls()
        .into_iter()
        .map(|(t, c)| (t, c.target))
        .collect();
    // The add, the list it answers, the refused add.
    let clients = (Target::Owner, "clients".to_string());
    assert_eq!(asked, [clients.clone(), clients.clone(), clients]);
}

/// The door decides who a locked profile's person is, from its own lock file, and passes the
/// person's call to the owner's host as that device, naming the profile, never as the operator.
/// A device that isn't the person, the owner's id, an unknown one, no `as`, and a profile on a
/// person's own account are answered on the door, and nothing leaves it.
#[tokio::test]
async fn a_person_s_call_reaches_the_owner_s_host_as_that_device() {
    const OWNER_P: &str = "0f0f0f0f0f0f";
    const KID: &str = "a1b2c3d4e5f6";
    let (listed, other) = ("a7".repeat(32), "b8".repeat(32));
    for adopted in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let row =
            |id, account| serde_json::json!({"id": id, "display_name": id, "os_account": account});
        let profiles = serde_json::json!({"profiles": [
            row(OWNER_P, serde_json::json!({"kind": "operator"})),
            row(KID, serde_json::json!({"kind": "seat", "seat": KID_SEAT})),
        ]});
        let state = super::profile_view::profiles_state(tmp.path(), profiles);
        let device = pf_seats::ipc::Device {
            fingerprint: listed.clone(),
            name: "Kid's tablet".into(),
        };
        let store = state.profiles.get().unwrap().clone();
        let locked = store.locks().edit(KID, |lock, now| {
            lock.locked = true;
            lock.add(&device, now)
        });
        assert!(locked.unwrap());
        let mut snapshot = box_snapshot(None);
        if adopted {
            snapshot.seats[1].adopted_uid = Some(1000);
        }
        let target = format!("library/hidden/steam:400?as={KID}");
        let seats = FakeSeats::new(snapshot).answering(
            &target,
            200,
            serde_json::json!({"id": "steam:400", "hidden": true}),
        );
        let np = paired(&[listed.as_str(), other.as_str()]);
        let app = test_app_door(state, Some(np), seats.clone());
        let hide = |fp: &str, query: &str| {
            let path = format!("/api/v1/library/hidden/steam:400{query}");
            let mut req = json_req(Method::PUT, &path, serde_json::json!({"hidden": true}));
            req.extensions_mut()
                .insert(PeerCertFingerprint(Some(fp.to_string())));
            req
        };
        for (fp, query, status) in [
            (other.as_str(), format!("?as={KID}"), StatusCode::FORBIDDEN),
            (
                listed.as_str(),
                format!("?as={OWNER_P}"),
                StatusCode::FORBIDDEN,
            ),
            (
                listed.as_str(),
                "?as=0123456789ab".into(),
                StatusCode::NOT_FOUND,
            ),
            (listed.as_str(), String::new(), StatusCode::UNAUTHORIZED),
        ] {
            let resp = app.clone().oneshot(hide(fp, &query)).await.unwrap();
            assert_eq!(resp.status(), status, "{query}");
        }
        assert!(seats.calls().is_empty(), "nothing left the door");
        let resp = app
            .clone()
            .oneshot(hide(&listed, &format!("?as={KID}")))
            .await;
        let status = resp.unwrap().status();
        if adopted {
            assert_eq!(status, StatusCode::FORBIDDEN, "a person's own account");
            assert!(seats.calls().is_empty());
            continue;
        }
        assert_eq!(status, StatusCode::OK);
        let calls = seats.calls();
        assert_eq!(calls.len(), 1);
        let (to, call) = &calls[0];
        assert_eq!(*to, Target::Owner);
        assert_eq!(call.target, target);
        assert_eq!(
            call.device.as_deref(),
            Some(listed.as_str()),
            "as the device"
        );
        assert_eq!(call.person.as_deref(), Some(KID));
    }
}
