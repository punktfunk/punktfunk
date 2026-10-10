//! Auth lanes: the admin bearer, the plugin token, the cert allowlist and per-route grants.

use super::*;

/// Discovery reports `permitted` from the live grant mask; invoke 403s without Power and 404s an unknown id.
/// Stored pre-power "Full control" (`GRANT_ALL_PRE_POWER`) still carries Power.
/// Never drive a 202: that would actually suspend the box.
#[tokio::test]
async fn host_actions_follow_the_power_grant() {
    use punktfunk_core::quic::{GRANT_ALL_PRE_POWER, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-actions-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let guest_fp = "aaaa00000001";
    let owner_fp = "bbbb00000002";
    let legacy_fp = "cccc00000003";
    np.add_with_access(
        "guest",
        guest_fp,
        Some(crate::native_pairing::Access {
            grants: GRANT_GAMEPAD,
            expires_unix: None,
            until_disconnect: false,
        }),
    )
    .unwrap();
    np.add("owner", owner_fp).unwrap(); // absent grants = full control, including Power
    np.add_with_access(
        "legacy",
        legacy_fp,
        Some(crate::native_pairing::Access {
            grants: GRANT_ALL_PRE_POWER,
            expires_unix: None,
            until_disconnect: false,
        }),
    )
    .unwrap();
    let app = test_app_native(test_state(), np);

    let discover = |fp: &str| {
        let mut req = get_req("/api/v1/actions");
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        req
    };
    let (status, body) = send(&app, discover(guest_fp)).await;
    assert_eq!(status, StatusCode::OK);
    let rows = body["actions"].as_array().unwrap();
    assert_eq!(rows.len(), 5, "{body}");
    assert!(
        rows.iter().all(|a| a["permitted"] == false),
        "a controller-only guest must not be offered power: {body}"
    );
    for fp in [owner_fp, legacy_fp] {
        let (_, body) = send(&app, discover(fp)).await;
        assert!(
            body["actions"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| a["group"] != "display")
                .all(|a| a["permitted"] == true),
            "full control (current or legacy-stored) carries Power: {body}"
        );
    }
    // Admin bearer without a cert is the console/owner surface: everything permitted.
    let (_, body) = send(&app, get_req("/api/v1/actions")).await;
    assert!(body["actions"]
        .as_array()
        .unwrap()
        .iter()
        .all(|a| a["permitted"] == true));

    let post = |path: &str| axum::http::Request::post(path).body(Body::empty()).unwrap();
    // Typed 403 without the grant; unpaired never reaches this handler.
    assert_eq!(
        send_cert(&app, post("/api/v1/actions/power.sleep"), guest_fp).await,
        StatusCode::FORBIDDEN,
        "no Power bit ⇒ 403"
    );
    // Unknown id 404s before grant or platform checks.
    let (status, _) = send(&app, post("/api/v1/actions/no.such")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Pause and remove need the manage-games bit on the cert lane; a stored pre-manage Full
/// still carries it. `/status` tells each device its own grants and the operator none.
#[tokio::test]
async fn title_installs_follow_the_manage_grant() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_ALL_PRE_MANAGE, GRANT_GAMEPAD, GRANT_LAUNCH};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-installs-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let access = |grants| {
        Some(crate::native_pairing::Access {
            grants,
            expires_unix: None,
            until_disconnect: false,
        })
    };
    let guest_fp = "aaaa00000021";
    let legacy_fp = "bbbb00000022";
    np.add_with_access("guest", guest_fp, access(GRANT_GAMEPAD | GRANT_LAUNCH))
        .unwrap();
    np.add_with_access("legacy", legacy_fp, access(GRANT_ALL_PRE_MANAGE))
        .unwrap();
    let app = test_app_native(test_state(), np);

    let pause = || {
        axum::http::Request::post("/api/v1/library/install/custom:no-such/pause")
            .body(Body::empty())
            .unwrap()
    };
    let remove = || {
        axum::http::Request::delete("/api/v1/library/install/custom:no-such")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send_cert(&app, pause(), guest_fp).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send_cert(&app, remove(), guest_fp).await,
        StatusCode::FORBIDDEN
    );
    // Past the grant: the title is unknown.
    assert_eq!(
        send_cert(&app, pause(), legacy_fp).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send_cert(&app, remove(), legacy_fp).await,
        StatusCode::NOT_FOUND
    );

    let status = |fp: &str| {
        let mut req = get_req("/api/v1/status");
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        req
    };
    let (_, body) = send(&app, status(guest_fp)).await;
    assert_eq!(body["grants"], GRANT_GAMEPAD | GRANT_LAUNCH);
    let (_, body) = send(&app, status(legacy_fp)).await;
    assert_eq!(body["grants"], GRANT_ALL);
    let (_, body) = send(&app, get_req("/api/v1/status")).await;
    assert!(body.get("grants").is_none(), "{body}");
}

/// `display.next` follows the caller's own live session, never the Host power grant, and a
/// refusal ends nothing. Never invoked with a pass: `policy::prefs()` is the developer's own
/// `display-settings.json`, so a pinned two-head box would really switch.
#[tokio::test]
async fn display_next_follows_the_live_session_not_the_power_grant() {
    use punktfunk_core::quic::GRANT_GAMEPAD;
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-display-next-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let streaming_fp = "aaaa00000011"; // controller-only, streaming
    let idle_fp = "bbbb00000012"; // full control, nothing live
    np.add_with_access(
        "streaming",
        streaming_fp,
        Some(crate::native_pairing::Access {
            grants: GRANT_GAMEPAD,
            expires_unix: None,
            until_disconnect: false,
        }),
    )
    .unwrap();
    np.add("idle", idle_fp).unwrap();
    let app = test_app_native(test_state(), np);
    let (_live, stop, quit, _) = fake_session_with_flags(streaming_fp);

    let display_next = |body: &serde_json::Value| {
        body["actions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == "display.next")
            .cloned()
            .unwrap_or_else(|| panic!("display.next is always listed: {body}"))
    };
    let discover = |fp: Option<&str>| {
        let mut req = get_req("/api/v1/actions");
        if let Some(fp) = fp {
            req.extensions_mut()
                .insert(PeerCertFingerprint(Some(fp.to_string())));
        }
        req
    };
    let row = display_next(&send(&app, discover(Some(streaming_fp))).await.1);
    assert_eq!(
        row["permitted"], true,
        "its own session, no grant bit: {row}"
    );
    assert_eq!(row["group"], "display");
    assert_eq!(row["danger"], false);
    let row = display_next(&send(&app, discover(Some(idle_fp))).await.1);
    assert_eq!(
        row["permitted"], false,
        "Host power is not a session: {row}"
    );
    let row = display_next(&send(&app, discover(None)).await.1);
    assert_eq!(row["permitted"], true, "the console may always: {row}");

    // Down the power path this device has the grant and would meet the other live device's
    // 409 instead.
    let post = axum::http::Request::post("/api/v1/actions/display.next")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send_cert(&app, post, idle_fp).await, StatusCode::FORBIDDEN);
    assert!(
        !stop.load(Ordering::SeqCst) && !quit.load(Ordering::SeqCst),
        "a display action ends no session"
    );
}

/// A paired streaming cert reaches only the read-only allowlist; PIN and mutating routes need the operator bearer.
#[tokio::test]
async fn cert_auth_is_a_read_only_allowlist() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-cert-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("streaming-client", fp).unwrap();
    let app = test_app_native(test_state(), np);

    for p in [
        "/api/v1/host",
        "/api/v1/status",
        "/api/v1/compositors",
        "/api/v1/library",
    ] {
        assert_ne!(
            send_cert(&app, get_req(p), fp).await,
            StatusCode::UNAUTHORIZED,
            "a paired streaming cert should authorize GET {p}"
        );
    }
    // Roster GETs are token-only: one cert must not list every other device's name + fingerprint.
    for p in ["/api/v1/clients", "/api/v1/native/clients"] {
        assert_eq!(
            send_cert(&app, get_req(p), fp).await,
            StatusCode::UNAUTHORIZED,
            "the client roster {p} must require the bearer token, not just a paired cert"
        );
    }
    // Exact `/api/v1/library` cert match must not leak `/library/scanners`.
    assert_eq!(
        send_cert(&app, get_req("/api/v1/library/scanners"), fp).await,
        StatusCode::UNAUTHORIZED,
        "the scanner settings must require the bearer token, not just a paired cert"
    );
    for p in [
        "/api/v1/plugins",
        "/api/v1/plugins/rom-manager/ui-credential",
    ] {
        assert_eq!(
            send_cert(&app, get_req(p), fp).await,
            StatusCode::UNAUTHORIZED,
            "the plugin directory {p} must require the bearer token, not just a paired cert"
        );
    }
    assert_eq!(
        send_cert(&app, get_req("/api/v1/native/pair"), fp).await,
        StatusCode::UNAUTHORIZED,
        "GET /native/pair exposes the PIN → must require the bearer token"
    );
    assert_eq!(
        send_cert(
            &app,
            json_req(
                Method::POST,
                "/api/v1/native/pair/arm",
                serde_json::json!({"ttl_secs": 60})
            ),
            fp,
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "arming pairing must require the bearer token"
    );
    assert_eq!(
        send_cert(
            &app,
            axum::http::Request::delete("/api/v1/native/clients/deadbeefcafe")
                .body(Body::empty())
                .unwrap(),
            fp,
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "unpair (DELETE) must require the bearer token"
    );
    assert_eq!(
        send_cert(&app, get_req("/api/v1/status"), "not-paired").await,
        StatusCode::UNAUTHORIZED,
        "an unpaired cert must be rejected"
    );
}

/// Admin bearer is loopback-only so an all-interfaces bind never LAN-exposes the console token.
/// A paired cert still reaches the read-only allowlist from a LAN peer.
#[tokio::test]
async fn bearer_admin_is_loopback_only() {
    let lan: SocketAddr = "192.168.1.50:54321".parse().unwrap();
    let loopback: SocketAddr = "127.0.0.1:33333".parse().unwrap();
    let bearer = |peer: SocketAddr| {
        let mut req = get_req("/api/v1/stats/recordings"); // bearer-only admin route
        req.extensions_mut().insert(PeerAddr(peer));
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-secret"),
        );
        req
    };

    let app = test_app(test_state(), None);
    assert_eq!(
        app.clone()
            .oneshot(bearer(lan))
            .await
            .expect("infallible")
            .status(),
        StatusCode::UNAUTHORIZED,
        "a bearer token from a LAN peer must be rejected on the admin API"
    );
    assert_ne!(
        app.clone()
            .oneshot(bearer(loopback))
            .await
            .expect("infallible")
            .status(),
        StatusCode::UNAUTHORIZED,
        "the bearer token must be accepted from a loopback peer"
    );
    // A dual-stack bind hands local IPv4 over IPv4-mapped; a mapped LAN peer stays out.
    let mapped_loopback: SocketAddr = "[::ffff:127.0.0.1]:33333".parse().unwrap();
    let mapped_lan: SocketAddr = "[::ffff:192.168.1.50]:54321".parse().unwrap();
    assert_ne!(
        app.clone()
            .oneshot(bearer(mapped_loopback))
            .await
            .expect("infallible")
            .status(),
        StatusCode::UNAUTHORIZED,
        "a mapped loopback peer is loopback"
    );
    assert_eq!(
        app.clone()
            .oneshot(bearer(mapped_lan))
            .await
            .expect("infallible")
            .status(),
        StatusCode::UNAUTHORIZED,
        "a mapped LAN peer is not"
    );

    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-lanlib-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("lan-client", fp).unwrap();
    let app = test_app_native(test_state(), np);
    let mut req = get_req("/api/v1/library");
    req.extensions_mut().insert(PeerAddr(lan));
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    assert_ne!(
        app.clone().oneshot(req).await.expect("infallible").status(),
        StatusCode::UNAUTHORIZED,
        "a paired cert must reach the library from a LAN peer"
    );

    // Art proxy is a prefix match in `cert_may_access` (dynamic id/kind). Unknown kind 404s
    // before I/O, so this is the auth gate, not art resolution (`library::tests`).
    let mut req = get_req("/api/v1/library/art/steam:570/not-a-real-kind");
    req.extensions_mut().insert(PeerAddr(lan));
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    assert_eq!(
        app.clone().oneshot(req).await.expect("infallible").status(),
        StatusCode::NOT_FOUND,
        "a paired cert must reach the per-image library art proxy from a LAN peer \
         (and an unknown kind 404s, rather than ever being rejected as unauthorized)"
    );
}

#[tokio::test]
async fn health_is_open_and_versioned() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/health")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["abi_version"], punktfunk_core::ABI_VERSION);
}

#[tokio::test]
async fn bearer_token_is_enforced() {
    let app = test_app(test_state(), Some("sekrit"));

    let (status, body) = send(&app, get_req("/api/v1/status")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].as_str().unwrap().contains("bearer"));
    let wrong = axum::http::Request::get("/api/v1/status")
        .header("authorization", "Bearer nope")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED);

    let right = axum::http::Request::get("/api/v1/status")
        .header("authorization", "Bearer sekrit")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, right).await.0, StatusCode::OK);

    assert_eq!(
        send(&app, get_req("/api/v1/health")).await.0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, get_req("/api/v1/openapi.json")).await.0,
        StatusCode::OK
    );
    let docs = app.clone().oneshot(get_req("/api/docs")).await.unwrap();
    assert_eq!(docs.status(), StatusCode::OK);
    let html = docs.into_body().collect().await.unwrap().to_bytes();
    assert!(
        html.starts_with(b"<!doctype html>"),
        "Scalar UI should serve HTML"
    );
}

#[tokio::test]
async fn plugin_token_lane_is_scoped_and_loopback_only() {
    use axum::http::Method;
    let app = test_app(test_state(), None);

    let plugin_req = |method: Method, path: &str| {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", "Bearer plugin-secret")
            .body(Body::empty())
            .unwrap()
    };

    assert_eq!(
        send(&app, plugin_req(Method::GET, "/api/v1/status"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(&app, plugin_req(Method::GET, "/api/v1/plugins"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        send(
            &app,
            plugin_req(Method::DELETE, "/api/v1/plugins/no-such-plugin")
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );

    // The runner's only token (Windows LocalService cannot read the admin one). Pin ingest
    // here; do not rely on `plugin_may_access` continuing not to match `/plugins/logs`.
    let body = serde_json::json!({"entries": [{
        "ts_ms": 1_700_000_000_000u64,
        "level": "INFO",
        "source": "virtualhere",
        "msg": "hello from the runner",
    }]});
    let req = axum::http::Request::post("/api/v1/plugins/logs")
        .header("content-type", "application/json")
        .header("authorization", "Bearer plugin-secret")
        .body(Body::from(body.to_string()))
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::NO_CONTENT);

    // Carve-outs are 403 (authenticated but not authorized), not 401.
    #[cfg_attr(not(feature = "gamestream"), allow(unused_mut))]
    let mut carveouts = vec![
        (Method::GET, "/api/v1/hooks"),
        (Method::PUT, "/api/v1/hooks"),
        (Method::POST, "/api/v1/native/pair/arm"),
        (Method::GET, "/api/v1/native/pending"),
        (Method::DELETE, "/api/v1/clients/aabbcc"),
        (Method::GET, "/api/v1/plugins/x/ui-credential"),
        (Method::GET, "/api/v1/store/catalog"),
        (Method::POST, "/api/v1/store/install"),
        (Method::POST, "/api/v1/store/uninstall"),
        (Method::POST, "/api/v1/store/runtime"),
        (Method::PUT, "/api/v1/store/sources/evil"),
    ];
    // PIN route exists only in GameStream-featured builds.
    #[cfg(feature = "gamestream")]
    carveouts.push((Method::GET, "/api/v1/pair"));
    for (method, path) in carveouts {
        let (status, body) = send(&app, plugin_req(method.clone(), path)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(body["error"].as_str().unwrap().contains("plugin token"));
    }

    let wrong = axum::http::Request::get("/api/v1/status")
        .header("authorization", "Bearer plugin-wrong")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, wrong).await.0, StatusCode::UNAUTHORIZED);

    // LAN peer is refused before token compare, same as the admin token.
    let mut lan = plugin_req(Method::GET, "/api/v1/status");
    lan.extensions_mut()
        .insert(PeerAddr("192.168.1.50:40000".parse().unwrap()));
    assert_eq!(send(&app, lan).await.0, StatusCode::UNAUTHORIZED);
}

/// A blank token is no token: `run` refuses to start unauthenticated, even on loopback.
#[tokio::test]
async fn blank_token_rejected() {
    let opts = Options {
        bind: "127.0.0.1:0".parse().unwrap(),
        token: Some("   ".into()),
        plugin_token: None,
        ..Default::default()
    };
    let err = run(
        test_state(),
        opts,
        None,
        test_stats(),
        false,
        crate::identity::ephemeral().unwrap(),
        false,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("no token"), "{err}");
}
