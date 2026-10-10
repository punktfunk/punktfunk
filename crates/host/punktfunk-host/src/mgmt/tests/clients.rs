//! Paired clients: list, label, unpair and the GameStream PIN.

use super::*;

// The env override must cover the whole body. `#[tokio::test]` is single-threaded, so
// nothing else needs the executor while we hold `CONFIG_DIR_TEST_LOCK`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn paired_clients_list_and_unpair() {
    // Unpair writes paired.json; the override keeps this off the real config dir.
    let tmp = ConfigDirOverride::new();

    let state = test_state();
    let app = test_app(state.clone(), None);

    // Native ephemeral identity (CN "punktfunk") so both build flavors share a stand-in cert.
    let stand_in = crate::identity::ephemeral().unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(stand_in.cert_pem.as_bytes()).unwrap();
    let der = pem.contents.clone();
    let fingerprint = hex::encode(Sha256::digest(&der));
    // `AppState::new` loads paired.json; clear before seeding so a real pairing never lands at [0].
    {
        let mut p = state.paired.lock().unwrap();
        p.clear();
        // Cloned, not moved: the unpair-all section at the end of this test re-seeds it.
        p.push(der.clone());
    }

    let (status, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["fingerprint"], fingerprint);
    assert_eq!(body[0]["subject"], "CN=punktfunk");

    let bad = axum::http::Request::delete("/api/v1/clients/zz")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, bad).await.0, StatusCode::BAD_REQUEST);

    // Unpair is revocation: it must end this client's live session, not only delist the cert.
    {
        use std::sync::atomic::Ordering;
        // owner_fp is the sha256 of the cert DER — the bytes `fingerprint` encodes.
        let mut owner = [0u8; 32];
        owner.copy_from_slice(&hex::decode(&fingerprint).unwrap());
        state.streaming.store(true, Ordering::SeqCst);
        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            host_audio: false,
            peer_ip: None,
            owner_fp: Some(owner),
        });
    }

    // Path is case-insensitive; uppercase hex must match too.
    let del = |fp: String| {
        axum::http::Request::delete(format!("/api/v1/clients/{fp}"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send(&app, del(fingerprint.to_uppercase())).await.0,
        StatusCode::NO_CONTENT
    );
    {
        use std::sync::atomic::Ordering;
        assert!(
            state.launch.lock().unwrap().is_none(),
            "unpair must end the revoked client's live session"
        );
        assert!(!state.streaming.load(Ordering::SeqCst));
        assert!(
            state.quit.load(Ordering::SeqCst),
            "the teardown is deliberate (quit), not a drop"
        );
    }
    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    assert_eq!(send(&app, del(fingerprint)).await.0, StatusCode::NOT_FOUND);

    // Restart must not resurrect the pairing (that re-opens the control port).
    // `PUNKTFUNK_CONFIG_DIR` is used verbatim — no `punktfunk` subdirectory.
    let disk = std::fs::read(tmp.path().join("paired.json")).expect("unpair persisted paired.json");
    assert_eq!(
        serde_json::from_slice::<Vec<Vec<u8>>>(&disk).unwrap(),
        Vec::<Vec<u8>>::new()
    );

    // Re-seed two clients and clear teardown flags so bulk-delete's session effect is not leftover.
    let second = crate::identity::ephemeral().unwrap();
    let (_, second_pem) = x509_parser::pem::parse_x509_pem(second.cert_pem.as_bytes()).unwrap();
    let second_der = second_pem.contents.clone();
    let second_fp = hex::encode(Sha256::digest(&second_der));
    {
        use std::sync::atomic::Ordering;
        let mut p = state.paired.lock().unwrap();
        p.clear();
        p.push(der.clone());
        p.push(second_der);
        state.quit.store(false, Ordering::SeqCst);
        state.streaming.store(true, Ordering::SeqCst);
        // Session owned by the second client — bulk delete must end whichever revoked cert owns it.
        let mut owner = [0u8; 32];
        owner.copy_from_slice(&hex::decode(&second_fp).unwrap());
        *state.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            host_audio: false,
            peer_ip: None,
            owner_fp: Some(owner),
        });
    }

    let del_all = || {
        axum::http::Request::delete("/api/v1/clients")
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 2, "both clients must be reported removed");

    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    {
        use std::sync::atomic::Ordering;
        assert!(
            state.launch.lock().unwrap().is_none(),
            "unpair-all must end the live session of any client it revokes"
        );
        assert!(state.quit.load(Ordering::SeqCst));
    }
    // Same persist check: a resurrected pairing would re-open the control port.
    let disk = std::fs::read(tmp.path().join("paired.json")).unwrap();
    assert_eq!(
        serde_json::from_slice::<Vec<Vec<u8>>>(&disk).unwrap(),
        Vec::<Vec<u8>>::new()
    );

    // Emptying an empty store is 200 with count 0, not the single-delete's 404.
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 0);
}

/// Moonlight certs share a subject; the label is the only distinction in the console, so
/// a silent no-op rename is indistinguishable from picking the other device.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn client_label_round_trips_scrubs_and_is_forgotten_on_unpair() {
    let tmp = ConfigDirOverride::new();

    let state = test_state();
    let app = test_app(state.clone(), None);
    let stand_in = crate::identity::ephemeral().unwrap();
    let (_, pem) = x509_parser::pem::parse_x509_pem(stand_in.cert_pem.as_bytes()).unwrap();
    let der = pem.contents.clone();
    let fingerprint = hex::encode(Sha256::digest(&der));
    {
        let mut p = state.paired.lock().unwrap();
        p.clear();
        p.push(der.clone());
    }

    let patch = |fp: String, body: serde_json::Value| {
        axum::http::Request::patch(format!("/api/v1/clients/{fp}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    // Unnamed: the field is absent, not an empty string.
    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert!(body[0]["label"].is_null());

    // Path is case-insensitive; uppercase fingerprint must match too.
    let (status, body) = send(
        &app,
        patch(
            fingerprint.to_uppercase(),
            serde_json::json!({ "label": "Living Room TV" }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["label"], "Living Room TV");
    let (_, body) = send(&app, get_req("/api/v1/clients")).await;
    assert_eq!(body[0]["label"], "Living Room TV");

    // Bidi override could impersonate another device in the unpair list; whitespace collapse keeps one line.
    // `\u{202E}` is RIGHT-TO-LEFT OVERRIDE.
    let (_, body) = send(
        &app,
        patch(
            fingerprint.clone(),
            serde_json::json!({ "label": "  Deck\u{202E}evil\n\nx  " }),
        ),
    )
    .await;
    assert_eq!(body["label"], "Deckevil x");

    // Whitespace-only must clear, not store "   " or the sanitizer's "device <fp8>" fallback.
    let (_, body) = send(
        &app,
        patch(fingerprint.clone(), serde_json::json!({ "label": "   " })),
    )
    .await;
    assert!(body["label"].is_null());

    send(
        &app,
        patch(
            fingerprint.clone(),
            serde_json::json!({ "label": "Bedroom" }),
        ),
    )
    .await;
    let (_, body) = send(
        &app,
        patch(fingerprint.clone(), serde_json::json!({ "label": null })),
    )
    .await;
    assert!(body["label"].is_null());

    // Malformed → 400; unknown-but-well-formed → 404 (must not write a label nothing can clean up).
    assert_eq!(
        send(
            &app,
            patch("zz".into(), serde_json::json!({ "label": "x" }))
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            &app,
            patch("aa".repeat(32), serde_json::json!({ "label": "x" }))
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );

    // Unpair must forget the label so a later re-pair of the same cert does not inherit it.
    send(
        &app,
        patch(
            fingerprint.clone(),
            serde_json::json!({ "label": "Living Room TV" }),
        ),
    )
    .await;
    let del = axum::http::Request::delete(format!("/api/v1/clients/{fingerprint}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let on_disk: std::collections::BTreeMap<String, String> =
        std::fs::read(tmp.path().join("client-labels.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
    assert!(
        !on_disk.contains_key(&fingerprint),
        "unpair must forget the device's label, got {on_disk:?}"
    );
}

#[cfg(feature = "gamestream")]
#[tokio::test]
async fn submit_pin_validates_and_requires_pending_pairing() {
    let app = test_app(test_state(), None);
    let post = |body: &str| {
        axum::http::Request::post("/api/v1/pair/pin")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let body = |pin: &str| {
        format!(
            r#"{{"pin":"{pin}","uniqueid":"dev","fingerprint":"{}","peer_ip":"127.0.0.1"}}"#,
            "aa".repeat(32)
        )
    };
    assert_eq!(send(&app, post(&body(""))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        send(&app, post(&body("12ab"))).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(&app, post(&body("1234"))).await.0,
        StatusCode::CONFLICT
    );

    // axum body rejections must still wear the ApiError envelope.
    let (status, body) = send(&app, post("{not json")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string(), "syntax error: {body}");
    let (status, body) = send(&app, post(r#"{"wrong":"shape"}"#)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].is_string(), "schema mismatch: {body}");
    let no_ct = axum::http::Request::post("/api/v1/pair/pin")
        .body(Body::from(r#"{"pin":"1234"}"#))
        .unwrap();
    let (status, body) = send(&app, no_ct).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(body["error"].is_string(), "media type: {body}");
}
