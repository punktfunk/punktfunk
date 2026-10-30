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
