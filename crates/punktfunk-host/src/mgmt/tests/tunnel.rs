//! The plaintext router, tunnelled requests, device keys and the CORS lane.

use super::*;

/// Plain HTTP on the management port answers the plane's route and nothing else, with the
/// cross-origin headers a page served over `http://` needs to read it.
#[tokio::test]
async fn the_plaintext_router_carries_one_route() {
    let app = crate::mgmt::bootstrap_app();
    // No plane runs in a test, so the route answers for itself: 404 with its own sentence.
    let (status, body) = send(&app, get_req("/api/v1/webtransport")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        body["error"].as_str().unwrap().contains("not enabled"),
        "{body}"
    );
    for path in [
        "/api/v1/health",
        "/api/v1/local/summary",
        "/api/v1/openapi.json",
        "/api/docs",
    ] {
        let (status, body) = send(&app, get_req(path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(
            body["error"].as_str().unwrap().contains("plain HTTP"),
            "{path}: {body}"
        );
    }
    let mut req = get_req("/api/v1/webtransport");
    req.headers_mut().insert(
        axum::http::header::ORIGIN,
        axum::http::HeaderValue::from_static("http://127.0.0.1:5173"),
    );
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        res.headers()
            .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .unwrap(),
        "http://127.0.0.1:5173",
        "a page reads the bootstrap cross-origin"
    );
}

/// The tunnel's one catastrophic mistake would be a request with no `PeerAddr`: the gate reads
/// that as loopback, and loopback is admin. Pinned from both sides — the same request straight
/// into the router, with nothing stamped, IS admin.
#[tokio::test]
async fn a_tunnelled_request_is_the_lan_peer_it_came_from() {
    use crate::webtransport::mgmt::{dispatch, Head};
    let app = test_app(test_state(), None);
    let lan: std::net::SocketAddr = "192.168.1.44:52000".parse().unwrap();
    let plane: std::net::SocketAddr = "0.0.0.0:9778".parse().unwrap();
    let head = |m: &str, p: &str, h: &[(&str, &str)]| Head {
        m: m.into(),
        p: p.into(),
        h: h.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
    };
    let admin = [("authorization", "Bearer test-secret")];
    let via = |head: Head| {
        let app = app.clone();
        async move { dispatch(&app, lan, plane, head, Vec::new()).await.status() }
    };

    assert_eq!(
        via(head("GET", "/api/v1/clients", &admin)).await,
        StatusCode::UNAUTHORIZED,
        "the admin bearer is loopback-only, and a tunnel is never loopback"
    );
    assert_eq!(
        send(&app, get_req("/api/v1/clients")).await.0,
        StatusCode::OK,
        "unstamped, the very same request is admin: that is the hole the stamp closes"
    );
    assert_eq!(
        via(head("GET", "/api/v1/health", &[])).await,
        StatusCode::OK
    );
    assert_eq!(
        via(head("GET", "/api/v1/local/summary", &[])).await,
        StatusCode::FORBIDDEN,
        "the position-authenticated lane is refused by the tunnel itself"
    );
    assert_eq!(
        via(head("DELETE", "/api/v1/clients", &admin)).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        via(head("GET", "/api/docs", &[])).await,
        StatusCode::FORBIDDEN,
        "nothing outside the versioned API"
    );
}

/// A device token earned through the tunnel buys the paired-device lane through the tunnel, and
/// no more — the roster that names every other device stays admin.
#[tokio::test]
async fn a_tunnelled_device_token_reads_the_library_and_no_more() {
    use crate::webtransport::mgmt::{dispatch, Head};
    use base64::Engine as _;
    use rcgen::{KeyPair, PublicKeyData as _, SigningKey as _, PKCS_ECDSA_P256_SHA256};

    let b64 = base64::engine::general_purpose::STANDARD;
    let path = std::env::temp_dir().join(format!("pf-mgmt-tunnel-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let np =
        Arc::new(crate::native_pairing::NativePairing::load_with(Some(path), None, false).unwrap());
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let spki = key.subject_public_key_info();
    let fp = hex::encode(crate::webtransport::sha256(&spki));
    np.add("Samsung TV", &fp).unwrap();
    let app = test_app_native(test_state(), np);
    let lan: std::net::SocketAddr = "192.168.5.63:40100".parse().unwrap();
    let plane: std::net::SocketAddr = "0.0.0.0:9778".parse().unwrap();
    let call = |m: &str, p: &str, h: Vec<(String, String)>, body: Vec<u8>| {
        let app = app.clone();
        let head = Head {
            m: m.into(),
            p: p.into(),
            h,
        };
        async move {
            let res = dispatch(&app, lan, plane, head, body).await;
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            let json = serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap_or(serde_json::Value::Null);
            (status, json)
        }
    };

    let (status, challenge) = call("POST", "/api/v1/auth/device/challenge", vec![], vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let nonce = challenge["nonce"].as_str().unwrap().to_string();
    let raw: Vec<u8> = (0..64)
        .step_by(2)
        .map(|i| u8::from_str_radix(&nonce[i..i + 2], 16).unwrap())
        .collect();
    let mut n = [0u8; 32];
    n.copy_from_slice(&raw);
    // `[0x5a; 32]` is the binding `test_app_native` installs as the host identity.
    let signature = b64.encode(
        key.sign(&punktfunk_core::quic::auth_signed_message(&[0x5a; 32], &n))
            .unwrap(),
    );
    let body = serde_json::json!({
        "device_key": b64.encode(&spki),
        "nonce": nonce,
        "signature": signature,
    })
    .to_string()
    .into_bytes();
    let json = vec![("content-type".to_string(), "application/json".to_string())];
    let (status, grant) = call("POST", "/api/v1/auth/device/token", json, body).await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    let bearer = vec![(
        "authorization".to_string(),
        format!("Bearer {}", grant["token"].as_str().unwrap()),
    )];

    let (status, _) = call(
        "GET",
        "/api/v1/library/page?limit=50",
        bearer.clone(),
        vec![],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the library, on the paired-device lane"
    );
    let (status, _) = call("GET", "/api/v1/status", bearer.clone(), vec![]).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call("GET", "/api/v1/native/clients", bearer.clone(), vec![]).await;
    assert_ne!(
        status,
        StatusCode::OK,
        "a device token must not reach the admin lane"
    );
    let (status, _) = call("GET", "/api/v1/clients", bearer, vec![]).await;
    assert_ne!(status, StatusCode::OK);
}

/// The browser's whole route into the management API: challenge, sign, exchange, then use the
/// token on the paired-device lane — and be refused everywhere that lane does not reach.
///
/// The signature is made with a real P-256 key over `auth_signed_message`, the same function the
/// control stream uses, so a divergence between the two shows up here rather than against a live
/// browser.
#[tokio::test]
async fn a_paired_device_key_buys_the_cert_lane_and_no_more() {
    use base64::Engine as _;
    use rcgen::{KeyPair, PublicKeyData as _, SigningKey as _, PKCS_ECDSA_P256_SHA256};

    let b64 = base64::engine::general_purpose::STANDARD;
    let path = std::env::temp_dir().join(format!("pf-mgmt-device-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let np =
        Arc::new(crate::native_pairing::NativePairing::load_with(Some(path), None, false).unwrap());
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let spki = key.subject_public_key_info();
    let fp = hex::encode(crate::webtransport::sha256(&spki));
    let app = test_app_native(test_state(), np.clone());

    // The exchange, as the page runs it.
    let exchange = |body: serde_json::Value| {
        let app = app.clone();
        async move {
            let req = axum::http::Request::post("/api/v1/auth/device/token")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap();
            // No bearer: this route is reached without a credential, which is the point.
            let res = app.oneshot(req).await.unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (
                status,
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
            )
        }
    };
    let challenge = || {
        let app = app.clone();
        async move {
            let req = axum::http::Request::post("/api/v1/auth/device/challenge")
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            v["nonce"].as_str().unwrap().to_string()
        }
    };
    let signed = |nonce: &str| {
        let raw: Vec<u8> = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&nonce[i..i + 2], 16).unwrap())
            .collect();
        let mut n = [0u8; 32];
        n.copy_from_slice(&raw);
        // `[0x5a; 32]` is the binding `test_app_native` installs as the host identity.
        b64.encode(
            key.sign(&punktfunk_core::quic::auth_signed_message(&[0x5a; 32], &n))
                .unwrap(),
        )
    };

    // Unpaired: a perfect signature buys nothing.
    let nonce = challenge().await;
    let (status, _) = exchange(serde_json::json!({
        "device_key": b64.encode(&spki),
        "nonce": nonce,
        "signature": signed(&nonce),
    }))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "not paired yet");

    np.add("Browser", &fp).unwrap();

    // A stale nonce is refused, and the refusal is indistinguishable from any other.
    let (status, _) = exchange(serde_json::json!({
        "device_key": b64.encode(&spki),
        "nonce": "ff".repeat(32),
        "signature": signed(&"ff".repeat(32)),
    }))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a nonce we never issued");

    // A live nonce with the wrong signature burns that nonce anyway.
    let nonce = challenge().await;
    let (status, _) = exchange(serde_json::json!({
        "device_key": b64.encode(&spki),
        "nonce": nonce,
        "signature": b64.encode([7u8; 70]),
    }))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "a bad signature");
    let (status, _) = exchange(serde_json::json!({
        "device_key": b64.encode(&spki),
        "nonce": nonce,
        "signature": signed(&nonce),
    }))
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "and the nonce is spent");

    // The real thing.
    let nonce = challenge().await;
    let (status, grant) = exchange(serde_json::json!({
        "device_key": b64.encode(&spki),
        "nonce": nonce,
        "signature": signed(&nonce),
    }))
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(grant["fingerprint"].as_str(), Some(fp.as_str()));
    let token = grant["token"].as_str().unwrap().to_string();

    let with_token = |path: &str| {
        let (app, token) = (app.clone(), token.clone());
        let path = path.to_string();
        async move {
            let req = axum::http::Request::get(&path)
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            app.oneshot(req).await.unwrap().status()
        }
    };
    // Exactly the paired-device set: the library it came for, and not the roster that names
    // every other device.
    assert_eq!(with_token("/api/v1/library").await, StatusCode::OK);
    assert_eq!(with_token("/api/v1/status").await, StatusCode::OK);
    assert_ne!(
        with_token("/api/v1/native/clients").await,
        StatusCode::OK,
        "a device token must not reach the admin lane"
    );

    // The lane's writes are the device's too, whichever way it proved itself: its log upload,
    // and the power actions its grants allow.
    let upload = axum::http::Request::post("/api/v1/client-logs")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from("the page's own log"))
        .unwrap();
    assert_eq!(
        app.clone().oneshot(upload).await.unwrap().status(),
        StatusCode::CREATED,
        "a browser files its log under its device"
    );
    let list = axum::http::Request::get("/api/v1/actions")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(list).await.unwrap();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let actions: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let sleep = actions["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "power.sleep")
        .unwrap();
    assert_eq!(sleep["permitted"], true, "full access includes host power");

    // Unpairing revokes at once, rather than when the token lapses.
    np.remove(&fp).unwrap();
    assert_ne!(
        with_token("/api/v1/library").await,
        StatusCode::OK,
        "the store is re-read on every request"
    );
}

/// The second wall in front of a browser, after the certificate one. A preflight has to be
/// answered without a credential — `require_auth` would correctly refuse it — and the answer
/// must never claim credentials are allowed.
#[tokio::test]
async fn a_preflight_is_answered_and_never_allows_credentials() {
    use axum::http::header;

    let app = test_app_browser(test_state());
    let preflight = axum::http::Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/library")
        .header(header::ORIGIN, "https://web.punktfunk.io")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(preflight).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::NO_CONTENT,
        "answered, not refused"
    );
    let h = res.headers();
    assert_eq!(
        h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(),
        "https://web.punktfunk.io"
    );
    assert!(
        h.get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("authorization"),
        "the header that makes a preflight happen at all"
    );
    assert!(
        h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS).is_none(),
        "no ambient authority: see mgmt::cors"
    );

    // A real request carries the header too, or the browser discards the response it just got.
    let mut req = get_req("/api/v1/host");
    req.headers_mut()
        .insert(header::ORIGIN, "https://web.punktfunk.io".parse().unwrap());
    let res = app.clone().oneshot(req).await.unwrap();
    assert!(res
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_some());

    // A caller that sent no Origin is not a browser, and its response is left exactly as it was.
    let res = app.oneshot(get_req("/api/v1/health")).await.unwrap();
    assert!(res
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());
}

/// `/local/summary` is admitted by network position alone, so the same-origin policy was the
/// only thing keeping a page off it. A host that serves browsers must still not stamp it: a
/// page on a machine that trusts the host certificate reaches loopback like anything else.
#[tokio::test]
async fn the_loopback_lane_is_never_readable_cross_origin() {
    use axum::http::header;

    let app = test_app_browser(test_state());
    let origin = "https://evil.example";

    let preflight = axum::http::Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/local/summary")
        .header(header::ORIGIN, origin)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(preflight).await.unwrap();
    assert!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "a preflight here would tell a page it is welcome to try"
    );

    let mut req = summary_req();
    req.headers_mut()
        .insert(header::ORIGIN, origin.parse().unwrap());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "the tray still reads it");
    assert!(
        res.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "served, but the browser must discard it"
    );
}

/// The browser plane is off by default, so the headers that exist to serve it must be too.
/// Otherwise every host starts answering cross-origin on an upgrade, for a plane nobody enabled.
#[tokio::test]
async fn a_host_with_no_browser_plane_stamps_nothing() {
    use axum::http::header;

    let app = test_app(test_state(), None);
    let mut req = get_req("/api/v1/host");
    req.headers_mut()
        .insert(header::ORIGIN, "https://web.punktfunk.io".parse().unwrap());
    let res = app.clone().oneshot(req).await.unwrap();
    assert!(res
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .is_none());

    // And the preflight is not answered either — `require_auth` refuses it, as it did before.
    let preflight = axum::http::Request::builder()
        .method("OPTIONS")
        .uri("/api/v1/library")
        .header(header::ORIGIN, "https://web.punktfunk.io")
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(preflight).await.unwrap();
    assert_ne!(res.status(), StatusCode::NO_CONTENT, "no CORS answer");
}
