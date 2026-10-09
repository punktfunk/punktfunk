//! Live-session routes, `/status` and the tray's local summary.

use super::*;

fn fake_native_session(
    width: u32,
    height: u32,
    fps: u32,
) -> crate::session_status::LiveSessionGuard {
    let packed = ((width as u64) << 32) | ((height as u64) << 16) | fps as u64;
    // Desktop stream: no game row.
    crate::session_status::register(crate::session_status::Registration {
        mode: Arc::new(std::sync::atomic::AtomicU64::new(packed)),
        client_name: Some("studio-deck".into()),
        ..crate::session_status::Registration::fake("test-client")
    })
}

/// Every per-session route on an id nothing is streaming: a 404 in the ApiError envelope,
/// never a panic and never another session's teardown.
#[tokio::test]
async fn a_per_session_route_404s_an_unknown_id() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);
    let (_live, stop, _quit, idr) = fake_session_with_flags("aabbccddeeff");
    let ghost = u64::MAX;

    for req in [
        axum::http::Request::delete(format!("/api/v1/session/{ghost}"))
            .body(Body::empty())
            .unwrap(),
        axum::http::Request::post(format!("/api/v1/session/{ghost}/idr"))
            .body(Body::empty())
            .unwrap(),
        json_req(
            Method::PUT,
            &format!("/api/v1/session/{ghost}/audio"),
            serde_json::json!({ "muted": true }),
        ),
        json_req(
            Method::PUT,
            &format!("/api/v1/session/{ghost}/access"),
            serde_json::json!({ "level": "view" }),
        ),
    ] {
        let (status, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].is_string(), "ApiError envelope: {body}");
    }
    assert!(
        !stop.load(Ordering::SeqCst),
        "the live session is untouched"
    );
    assert!(!idr.load(Ordering::Relaxed));
}

/// `GET /session/last` answers a host that has streamed and a host that has not: a list,
/// never a 404 and never a 500. A stopped session arrives on it with the reason attached.
#[tokio::test]
async fn the_last_session_route_answers_before_and_after_a_session() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/session/last")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["sessions"].is_array(),
        "always a list, empty on a host that has not streamed: {body}"
    );

    let one = fake_session_with_flags("aabbccddeeff").0;
    let id = one.id;
    let del = axum::http::Request::delete(format!("/api/v1/session/{id}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    drop(one);

    let (status, body) = send(&app, get_req("/api/v1/session/last")).await;
    assert_eq!(status, StatusCode::OK);
    // By id: the ring is process-global and bounded, so a count is not a claim.
    let mine = body["sessions"]
        .as_array()
        .expect("a list")
        .iter()
        .find(|s| s["id"] == id)
        .unwrap_or_else(|| panic!("session {id} is on the list: {body}"));
    assert_eq!(mine["ended"], "stopped_by_operator");
    assert_eq!(mine["mode"], "1920x1080@60");
    assert_eq!(mine["codec"], "hevc");
    // No video loop ran, so it has no totals to claim — absent, not zero.
    assert!(mine.get("frames_sent").is_none(), "{mine}");
}

/// The point of the whole issue: with two clients on one host, stopping one must leave the
/// other streaming — and the id-less `DELETE /session` must still take both.
#[tokio::test]
async fn a_per_session_stop_drops_only_that_session() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);
    let (one, stop1, quit1, idr1) = fake_session_with_flags("aabbccddeeff");
    let (_two, stop2, quit2, idr2) = fake_session_with_flags("112233445566");

    let del = axum::http::Request::delete(format!("/api/v1/session/{}", one.id))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    assert!(stop1.load(Ordering::SeqCst) && quit1.load(Ordering::SeqCst));
    assert!(
        !stop2.load(Ordering::SeqCst),
        "the other client keeps streaming"
    );

    // Same for the keyframe: one id, one encoder.
    let post = axum::http::Request::post(format!("/api/v1/session/{}/idr", one.id))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, post).await.0, StatusCode::ACCEPTED);
    assert!(idr1.load(Ordering::Relaxed));
    assert!(!idr2.load(Ordering::Relaxed));

    // The id-less form is unchanged: every live session, as every existing caller expects.
    let all = axum::http::Request::delete("/api/v1/session")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, all).await.0, StatusCode::NO_CONTENT);
    assert!(stop2.load(Ordering::SeqCst) && quit2.load(Ordering::SeqCst));
}

/// Mute is per session: the other client on the same display keeps its audio, and the
/// state is on `/status` so the operator sees why one of them is silent.
///
/// The wire half — that a muted session stops sending datagrams — needs two real clients
/// on a real host; this covers the flag the audio thread reads.
#[tokio::test]
async fn muting_one_session_leaves_the_other_hearing() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);
    let (one, ..) = fake_session_with_flags("aabbccddeeff");
    let (two, ..) = fake_session_with_flags("112233445566");
    let muted = |id: u64| {
        crate::session_status::controls(id)
            .unwrap()
            .muted
            .load(Ordering::SeqCst)
    };

    let (status, _) = send(
        &app,
        json_req(
            Method::PUT,
            &format!("/api/v1/session/{}/audio", one.id),
            serde_json::json!({ "muted": true }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(muted(one.id));
    assert!(
        !muted(two.id),
        "the session sharing the sink still hears it"
    );

    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    let row = |id: u64| {
        body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(row(one.id)["muted"], true, "{body}");
    assert_eq!(row(two.id)["muted"], false);

    // Unmute puts it back without touching the sibling.
    let (status, _) = send(
        &app,
        json_req(
            Method::PUT,
            &format!("/api/v1/session/{}/audio", one.id),
            serde_json::json!({ "muted": false }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!muted(one.id));
}

/// The operator places one session's player: the pick lands on that row of `/status`
/// and nothing else, and it can be handed back.
///
/// Whether the pool actually hands that slot over is pinned in `pf_inject::pad_pool`
/// against a private pool — the process-wide one is shared with every other test here.
#[tokio::test]
async fn placing_a_player_touches_only_that_session() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);
    let (one, ..) = fake_session_with_flags("aabbccddeeff");
    let (two, ..) = fake_session_with_flags("112233445566");
    let pick = |id: u64, body: serde_json::Value| {
        json_req(Method::PUT, &format!("/api/v1/session/{id}/player"), body)
    };

    // Past the host's slot count: refused, so no console can store a slot no pad can take.
    let (status, _) = send(&app, pick(one.id, serde_json::json!({ "slot": 16 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // An id nothing is streaming is never another session's controllers.
    let ghost = one.id + two.id + 1000;
    let (status, _) = send(&app, pick(ghost, serde_json::json!({ "slot": 1 }))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = send(&app, pick(one.id, serde_json::json!({ "slot": 1 }))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["slot"], 1, "{body}");

    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    let row = |id: u64| {
        body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap()
    };
    assert_eq!(row(one.id)["preferred_pad_slot"], 1, "{body}");
    assert!(
        row(two.id)["preferred_pad_slot"].is_null(),
        "the other session keeps the first-free claim"
    );
    // Both rows carry the pads column whether or not anything is placed.
    assert_eq!(row(two.id)["pads"], serde_json::json!([]));

    // Handing it back leaves the row unplaced again.
    let (status, _) = send(&app, pick(one.id, serde_json::json!({ "slot": null }))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    assert!(body["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["preferred_pad_slot"].is_null()));
}

/// A live re-point moves the mask the input thread reads, and cannot widen past the
/// pairing: these sessions are paired controller-only, so `full` comes back clamped.
#[tokio::test]
async fn a_live_access_change_applies_to_the_running_session() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);
    let (one, ..) = fake_session_with_flags("aabbccddeeff");
    let (two, ..) = fake_session_with_flags("112233445566");
    let grants = |id: u64| {
        crate::session_status::controls(id)
            .unwrap()
            .grants
            .load(Ordering::Relaxed)
    };
    let before_other = grants(two.id);

    let (status, body) = send(
        &app,
        json_req(
            Method::PUT,
            &format!("/api/v1/session/{}/access", one.id),
            serde_json::json!({ "level": "view" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["level"], "view", "{body}");
    assert_eq!(
        grants(one.id),
        punktfunk_core::quic::GRANT_PRESET_VIEW_ONLY,
        "the live mask moved, with no reconnect"
    );
    assert_eq!(
        grants(two.id),
        before_other,
        "the other session is untouched"
    );

    // Handing the pad back: `full` is asked for, the controller-only pairing is what lands.
    let (status, body) = send(
        &app,
        json_req(
            Method::PUT,
            &format!("/api/v1/session/{}/access", one.id),
            serde_json::json!({ "level": "full" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["grants"].as_u64().unwrap() as u32,
        punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
        "clamped to the pairing's ceiling: {body}"
    );
    assert_eq!(
        grants(one.id),
        punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY
    );

    // Neither field, an unknown level, and a reserved bit are all 400s.
    for bad in [
        serde_json::json!({}),
        serde_json::json!({ "level": "admin" }),
        serde_json::json!({ "grants": punktfunk_core::quic::GRANT_RESERVED }),
    ] {
        let (status, body) = send(
            &app,
            json_req(
                Method::PUT,
                &format!("/api/v1/session/{}/access", one.id),
                bad,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());
    }
}

/// A native session must read as streaming in `/local/summary`. The GameStream `streaming` flag
/// stays false for the whole native stream, so the tray must not key off that flag alone.
#[tokio::test]
async fn local_summary_reports_a_native_session_as_streaming() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, summary_req()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);

    let session = fake_native_session(3840, 2160, 120);
    let (_, body) = send(&app, summary_req()).await;
    assert_eq!(body["video_streaming"], true, "native session: {body}");
    assert_eq!(body["audio_streaming"], true, "native session: {body}");
    assert_eq!(body["session"]["width"], 3840);
    assert_eq!(body["session"]["height"], 2160);
    assert_eq!(body["session"]["fps"], 120);
    // Live `client_name` is for the tray connect toast; idle-side is the next test.
    assert_eq!(body["client_name"], "studio-deck");

    drop(session);
    let (_, body) = send(&app, summary_req()).await;
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);
    assert_eq!(
        body["client_name"],
        serde_json::Value::Null,
        "no live session → no client name in the summary"
    );
}

/// `/local/summary` takes the tray token from loopback: position alone is nobody on a box
/// with several accounts, and a LAN peer is refused with the token. The body must not carry
/// PINs, fingerprints, or a paired-but-idle device's name. This test pairs a device and
/// registers no session so `client_name` stays absent.
#[tokio::test]
async fn local_summary_takes_the_tray_token_from_loopback_only_and_is_non_sensitive() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-summary-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    np.add("secret-device-name", "deadbeefcafe0123").unwrap();
    let app = test_app_native(test_state(), np);

    let (status, body) = send(&app, summary_req()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["native_paired_clients"], 1);
    assert_eq!(body["pending_approvals"], 0);
    assert!(body["version"].is_string());
    let raw = body.to_string();
    assert!(
        !raw.contains("deadbeefcafe0123") && !raw.contains("secret-device-name"),
        "summary must not leak fingerprints or device names: {raw}"
    );

    let (status, _) = send(&app, summary_req_from("192.168.1.50:40000")).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the local summary must be rejected for a LAN peer, token or not"
    );

    let (status, _) = send(&app, summary_req_from("[::1]:40000")).await;
    assert_eq!(status, StatusCode::OK, "::1 is a loopback peer");

    // Loopback with no bearer at all: a `send` without one is the bare tray of an older build.
    let mut bare = get_req("/api/v1/local/summary");
    bare.extensions_mut()
        .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
    bare.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer not-the-tray-token"),
    );
    let (status, _) = send(&app, bare).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "loopback is not a credential: a wrong bearer is refused"
    );

    // `ctl summary` presents the admin token and reaches it through its own lane.
    let mut admin = get_req("/api/v1/local/summary");
    admin
        .extensions_mut()
        .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
    let (status, _) = send(&app, admin).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the admin bearer still reads the summary"
    );
}

#[tokio::test]
async fn status_reflects_runtime_state() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let state = test_state();
    let app = test_app(state.clone(), None);

    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(body["video_streaming"], false);
    assert_eq!(body["session"], serde_json::Value::Null);

    *state.launch.lock().unwrap() = Some(LaunchSession {
        gcm_key: [0; 16],
        rikeyid: 1,
        width: 2560,
        height: 1440,
        fps: 120,
        appid: 1,
        host_audio: false,
        peer_ip: None,
        owner_fp: None,
    });
    state.streaming.store(true, Ordering::SeqCst);

    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(body["video_streaming"], true);
    assert_eq!(body["session"]["width"], 2560);
    assert_eq!(body["session"]["fps"], 120);
    assert!(!body.to_string().contains("gcm"));
}

#[tokio::test]
async fn stop_session_clears_runtime_state() {
    // This route quits every live native session, a sibling test's included.
    let _registry = crate::session_status::tests::REGISTRY.lock().await;
    let state = test_state();
    let app = test_app(state.clone(), None);
    state.streaming.store(true, Ordering::SeqCst);
    state.audio_streaming.store(true, Ordering::SeqCst);
    *state.launch.lock().unwrap() = Some(LaunchSession {
        gcm_key: [0; 16],
        rikeyid: 0,
        width: 1920,
        height: 1080,
        fps: 60,
        appid: 1,
        host_audio: false,
        peer_ip: None,
        owner_fp: None,
    });

    let del = axum::http::Request::delete("/api/v1/session")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    assert!(!state.streaming.load(Ordering::SeqCst));
    assert!(!state.audio_streaming.load(Ordering::SeqCst));
    assert!(state.launch.lock().unwrap().is_none());
}

#[tokio::test]
async fn idr_requires_an_active_stream() {
    // A sibling test's live native session would look like an active stream to this route.
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let state = test_state();
    let app = test_app(state.clone(), None);
    let post = || {
        axum::http::Request::post("/api/v1/session/idr")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send(&app, post()).await.0, StatusCode::CONFLICT);

    state.streaming.store(true, Ordering::SeqCst);
    assert_eq!(send(&app, post()).await.0, StatusCode::ACCEPTED);
    assert!(state.force_idr.load(Ordering::SeqCst));
}
