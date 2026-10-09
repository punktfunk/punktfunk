//! Handler + auth tests for the management API, exercised through `app()`.

/// Pins the published `KEY=VALUE` line against both consumers: systemd `EnvironmentFile=`
/// and `windows::service::read_env_file_value`. Re-implements the Windows split so a format
/// change fails here rather than on the platform CI cannot run.
#[test]
fn published_endpoint_line_parses_the_way_both_consumers_read_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = super::write_endpoint(dir.path(), 47991).unwrap();
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
}

mod auth_lanes;
mod clients;
mod plugins;
mod routes;
mod sessions;

use super::*;

/// Knocks in these tests come from the LAN unless the test is about a WAN knock. An unknown
/// source classifies as WAN, which the approve endpoint refuses.
const LAN_KNOCK: std::net::IpAddr = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 44));
#[cfg(feature = "gamestream")]
use crate::gamestream::cert::ServerIdentity;
use crate::gamestream::tls::{PeerAddr, PeerCertFingerprint};
use crate::gamestream::{LaunchSession, HTTPS_PORT, HTTP_PORT};
use crate::host::Host;
use axum::body::Body;
use axum::http::{Method, StatusCode};
use http_body_util::BodyExt;
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use tower::ServiceExt;

/// The access store's dir, one per call; never the host config dir.
fn test_access_dir() -> std::path::PathBuf {
    crate::test_support::scratch()
}

/// One dir per call; never the host config dir.
fn test_client_logs_dir() -> std::path::PathBuf {
    crate::test_support::scratch()
}

/// One dir per call; never the host config dir.
fn test_stats() -> Arc<crate::stats_recorder::StatsRecorder> {
    crate::stats_recorder::StatsRecorder::new(crate::test_support::scratch())
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
    Arc::new(AppState::new(
        host,
        test_stats(),
        #[cfg(feature = "gamestream")]
        crate::gamestream::GsState::new(ServerIdentity::ephemeral().expect("ephemeral identity")),
    ))
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
    test_app_on(state, token, false)
}

/// `box_library`: the app of a seat host reading the box's library, which refuses its writes.
fn test_app_on(state: Arc<AppState>, token: Option<&str>, box_library: bool) -> Router {
    let stats = state.stats.clone();
    app(
        state,
        Some(token.unwrap_or("test-secret").to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(test_plugin_tokens()),
        Some(TRAY_TOKEN.to_string()),
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
        box_library,
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
        Some(TRAY_TOKEN.to_string()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        None,
        true,
        false,
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
        Some(TRAY_TOKEN.to_string()),
        DEFAULT_PORT,
        Some(np),
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        // A fixed binding, so a device test signs what the host will check.
        Some([0x5a; 32]),
        false,
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

fn json_req(method: Method, path: &str, body: serde_json::Value) -> axum::http::Request<Body> {
    axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// `req` carrying `token` as its bearer, so `send` leaves it as it is.
fn with_bearer(mut req: axum::http::Request<Body>, token: &str) -> axum::http::Request<Body> {
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
    req
}

/// Cert-only: inject `PeerCertFingerprint` and omit the bearer so `require_auth` takes the cert branch.
async fn send_cert(app: &Router, mut req: axum::http::Request<Body>, fp: &str) -> StatusCode {
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    app.clone().oneshot(req).await.expect("infallible").status()
}

/// The bearer every test app mints for `/local/summary`; distinct from the admin and plugin ones.
const TRAY_TOKEN: &str = "tray-secret";

/// The tray's own call: its token, from loopback.
fn summary_req() -> axum::http::Request<Body> {
    summary_req_from("127.0.0.1:40000")
}

fn summary_req_from(peer: &str) -> axum::http::Request<Body> {
    let mut req = get_req("/api/v1/local/summary");
    req.extensions_mut().insert(PeerAddr(peer.parse().unwrap()));
    req.headers_mut().insert(
        axum::http::header::AUTHORIZATION,
        axum::http::HeaderValue::from_static("Bearer tray-secret"),
    );
    req
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
        stop: stop.clone(),
        quit: quit.clone(),
        force_idr: idr.clone(),
        controls,
        ..crate::session_status::Registration::fake(client)
    });
    (guard, stop, quit, idr)
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
    assert_eq!(body["door"], false, "a test host is no door");
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
        Some(TRAY_TOKEN.to_string()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        Some([0xab; 32]),
        false,
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

/// Overrides `PUNKTFUNK_CONFIG_DIR` for one test and restores it on drop, even on panic.
/// [`ConfigDirOverride::seat`] also points `PUNKTFUNK_LIBRARY_DIR` at a box's library.
///
/// One helper for every mgmt test: `check-unsafe-hygiene.sh` greps this file for a fixed
/// count of `set_var` sites (and prose mentions), all in [`write_env`]. The lock is a field so
/// Drop restores the env while still holding it — fields drop after `Drop::drop`.
struct ConfigDirOverride {
    tmp: tempfile::TempDir,
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl ConfigDirOverride {
    fn new() -> ConfigDirOverride {
        Self::with_library(None)
    }

    /// A Windows seat host's view: its own config dir, and the box's library in `library`.
    fn seat(library: &std::path::Path) -> ConfigDirOverride {
        Self::with_library(Some(library))
    }

    fn with_library(library: Option<&std::path::Path>) -> ConfigDirOverride {
        let _serial = crate::identity::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let vars = [
            ("PUNKTFUNK_CONFIG_DIR", Some(tmp.path().as_os_str())),
            (
                "PUNKTFUNK_LIBRARY_DIR",
                library.map(std::path::Path::as_os_str),
            ),
        ];
        let prev = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (key, value) in vars {
            // SAFETY: `_serial` holds CONFIG_DIR_TEST_LOCK, which serializes every test in this
            // binary that reads or writes these variables.
            unsafe { write_env(key, value) };
        }
        ConfigDirOverride { tmp, prev, _serial }
    }

    /// Config dir used verbatim by `pf_paths`; no `punktfunk` subdirectory is appended.
    fn path(&self) -> &std::path::Path {
        self.tmp.path()
    }
}

impl Drop for ConfigDirOverride {
    fn drop(&mut self) {
        for (key, value) in self.prev.drain(..) {
            // SAFETY: `self._serial` is still alive here (fields drop after `Drop::drop`), so this
            // runs under the same serialization as `with_library`.
            unsafe { write_env(key, value.as_deref()) };
        }
    }
}

/// # Safety
/// The caller holds `CONFIG_DIR_TEST_LOCK`: the process environment is global, and unsound to
/// change while another thread reads it.
unsafe fn write_env(key: &str, value: Option<&std::ffi::OsStr>) {
    match value {
        // SAFETY: the caller's lock (this function's contract) serializes every reader and writer.
        Some(v) => unsafe { std::env::set_var(key, v) },
        // SAFETY: as above.
        None => unsafe { std::env::remove_var(key) },
    }
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
        !enforced.contains(&"capture_monitor")
            || cfg!(any(target_os = "linux", target_os = "windows")),
        "pinning a real monitor needs a mirror backend"
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
        json_req(
            Method::PUT,
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
        json_req(
            Method::POST,
            "/api/v1/display/release",
            serde_json::json!({}),
        ),
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
        json_req(
            Method::POST,
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

    np.note_pending("Enrico's MacBook", "aa11", Some(LAN_KNOCK), None);
    np.note_pending("device bb22cc33", "bb22", Some(LAN_KNOCK), None);
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b.as_array().unwrap().len(), 2);
    assert_eq!(b[0]["name"], "Enrico's MacBook");
    let approve_id = b[0]["id"].as_u64().unwrap();
    let deny_id = b[1]["id"].as_u64().unwrap();

    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
            &format!("/api/v1/native/pending/{approve_id}/approve"),
            serde_json::json!({"name": "Office MacBook"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["name"], "Office MacBook");
    assert_eq!(b["fingerprint"], "aa11");
    assert!(np.is_paired("AA11"), "approval pins the fingerprint");

    let deny = json_req(
        Method::POST,
        &format!("/api/v1/native/pending/{deny_id}/deny"),
        serde_json::json!({}),
    );
    assert_eq!(send(&app, deny).await.0, StatusCode::NO_CONTENT);
    assert!(!np.is_paired("bb22"));
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
            "/api/v1/native/pending/123/approve",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
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
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
            "/api/v1/native/clients/bb22",
            serde_json::json!({"expires_in_secs": 60, "clear_expiry": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(b["error"].as_str().unwrap().contains("clear_expiry"));

    let (s, _) = send(
        &app,
        json_req(
            Method::PATCH,
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
        json_req(
            Method::PATCH,
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
    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK), None);
    let id = np.pending()[0].id;

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
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
    np.note_pending("Friend's Deck", "dd88", Some(wan), None);
    np.note_pending("Living Room", "ee99", Some(LAN_KNOCK), None);

    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    let rows = b.as_array().unwrap();
    let wan_row = rows.iter().find(|r| r["fingerprint"] == "dd88").unwrap();
    let lan_row = rows.iter().find(|r| r["fingerprint"] == "ee99").unwrap();
    assert_eq!(wan_row["source"], "wan");
    assert_eq!(lan_row["source"], "lan");

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
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

    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK), None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    assert!(pend[0]["grants"].is_null());
    assert!(pend[0]["access_level"].is_null());
    let id = pend[0]["id"].as_u64().unwrap();

    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
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
        json_req(Method::POST,
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
    np.note_pending("Guest Phone", "cc33", Some(LAN_KNOCK), None);
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
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({"ttl_secs": 60}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(np.armed_access(), None, "no fields = no choice");

    np.note_pending("Old Laptop", "ee55", Some(LAN_KNOCK), None);
    let (_, pend) = send(&app, get_req("/api/v1/native/pending")).await;
    let id = pend[0]["id"].as_u64().unwrap();
    let (s, b) = send(
        &app,
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
            "/api/v1/native/pair/arm",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // Pending list is `[]`, not 503 (same as `/native/clients`).
    let (s, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.as_array().unwrap().len(), 0);
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pending/0/approve",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, _) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/native/pending/0/deny",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
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
        json_req(
            Method::PUT,
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "fastest"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        json_req(
            Method::PUT,
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        json_req(
            Method::PUT,
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
    let uniq = format!("evt-{}", std::process::id());
    let m1 = format!("{uniq}-one");

    crate::events::emit(EventKind::DisplayReleased { count: 424_242 });
    crate::events::emit(EventKind::LibraryChanged { source: m1.clone() });

    let resp = app
        .clone()
        .oneshot(with_bearer(
            get_req("/api/v1/events?kinds=library.changed"),
            "test-secret",
        ))
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
                    .oneshot(with_bearer(
                        get_req(&format!(
                            "/api/v1/events?since={m1_seq}&kinds=library.changed"
                        )),
                        "test-secret",
                    ))
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
        .oneshot(with_bearer(
            get_req(&format!(
                "/api/v1/events?since={m1_seq}&kinds=library.changed"
            )),
            "test-secret",
        ))
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
    let mut req = with_bearer(
        get_req("/api/v1/events?since=0&kinds=library.changed"),
        "test-secret",
    );
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
        .oneshot(with_bearer(
            get_req("/api/v1/events?since=1"),
            "test-secret",
        ))
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
        .oneshot(with_bearer(get_req("/api/v1/events"), "test-secret"))
        .await
        .expect("infallible");
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(slots);

    let resp = app
        .clone()
        .oneshot(with_bearer(get_req("/api/v1/events"), "test-secret"))
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
/// Seeds one custom title on a platform.
fn seed_title(title: &str, platform: &str) {
    crate::library::add_custom(crate::library::CustomInput {
        title: title.into(),
        art: Default::default(),
        launch: None,
        prep: None,
        role: Default::default(),
        icon: None,
        detect: None,
        on_window: None,
        audio: None,
        meta: crate::library::GameMeta {
            platform: Some(platform.into()),
            ..Default::default()
        },
    })
    .expect("seed a title");
}

fn titles(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|g| g["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Pages follow title order, the cursor names a place rather than an offset, and the filters
/// and counts agree with what the pages hold.
#[tokio::test]
async fn library_pages_by_cursor_with_search_and_counts() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    for (t, p) in [
        ("Delta", "PS2"),
        ("alpha", "PS2"),
        ("Charlie", "N64"),
        ("bravo", "PS2"),
        ("Echo", "N64"),
    ] {
        seed_title(t, p);
    }

    let (s, first) = send(&app, get_req("/api/v1/library/page?limit=2")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(titles(&first), ["alpha", "bravo"]);
    assert_eq!(first["total"], 5);
    assert_eq!(first["platforms"][0]["platform"], "PS2");
    assert_eq!(first["platforms"][0]["count"], 3);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("more pages")
        .to_string();

    // A title that sorts before the cursor arrives between two pages: nothing repeats.
    seed_title("Able", "PS2");
    let (_, second) = send(
        &app,
        get_req(&format!("/api/v1/library/page?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(titles(&second), ["Charlie", "Delta"]);
    let cursor = second["next_cursor"]
        .as_str()
        .expect("one more")
        .to_string();
    let (_, last) = send(
        &app,
        get_req(&format!("/api/v1/library/page?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(titles(&last), ["Echo"]);
    assert!(last.get("next_cursor").is_none(), "{last}");

    let (_, found) = send(&app, get_req("/api/v1/library/page?q=HA")).await;
    assert_eq!(titles(&found), ["alpha", "Charlie"]);
    assert_eq!(found["total"], 2);

    // The platform filter narrows the page, not the counts beside it.
    let (_, n64) = send(&app, get_req("/api/v1/library/page?platform=n64")).await;
    assert_eq!(titles(&n64), ["Charlie", "Echo"]);
    assert_eq!(n64["platforms"].as_array().map(Vec::len), Some(2));

    let (s, _) = send(&app, get_req("/api/v1/library/page?cursor=not-a-cursor")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let id = first["items"][1]["id"].as_str().expect("an id");
    let (_, one) = send(&app, get_req(&format!("/api/v1/library/page?id={id}"))).await;
    assert_eq!(titles(&one), ["bravo"]);

    // A hidden title leaves every page but the operator's, where it is flagged.
    crate::library::set_entry_hidden(id, true).expect("hide");
    let (_, all) = send(&app, get_req("/api/v1/library/page")).await;
    assert_eq!(all["total"], 6);
    let flagged: Vec<_> = all["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|g| g["hidden"] == true)
        .map(|g| g["title"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(flagged, ["bravo"]);
}

/// The built list is kept between requests and dropped when a library file moves, whether
/// this process wrote it or someone else did.
#[test]
fn the_built_library_is_kept_until_an_input_moves() {
    let _tmp = ConfigDirOverride::new();
    seed_title("alpha", "PS2");
    let first = crate::library::sorted_games();
    assert!(Arc::ptr_eq(&first, &crate::library::sorted_games()));

    seed_title("bravo", "PS2");
    let second = crate::library::sorted_games();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(second.len(), 2);

    let by_hand = pf_paths::config_dir().join("library-stats.json");
    std::fs::write(by_hand, r#"{"games":{}}"#).expect("write the stats file");
    assert!(!Arc::ptr_eq(&second, &crate::library::sorted_games()));
}

/// A Windows seat lists and resolves the box's titles from `PUNKTFUNK_LIBRARY_DIR`, leaves one
/// account's sources out, rebuilds on its own play stats, and answers a library write with 409.
#[tokio::test]
async fn a_seat_plays_the_boxs_library_and_changes_none_of_it() {
    let boxdir = tempfile::tempdir().unwrap();
    let catalog = serde_json::json!({
        "entries": [
            {"id": "s1", "title": "Portal", "provider": "steam", "store": "steam",
             "external_id": "400", "launch": {"kind": "steam_appid", "value": "400"}},
            {"id": "p1", "title": "The owner's", "provider": "playnite", "external_id": "x1"},
        ],
        "claims": {"steam": "steam"},
    });
    std::fs::write(boxdir.path().join("library.json"), catalog.to_string()).unwrap();
    let seat = ConfigDirOverride::seat(boxdir.path());
    let app = test_app_on(test_state(), None, true);

    let games = crate::library::sorted_games();
    let ids: Vec<&str> = games.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["steam:400"]);
    assert!(crate::library::resolve_launch("steam:400").is_some());
    let sources: Vec<String> = crate::library::list_scanners()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(sources, ["steam"]);

    std::fs::write(seat.path().join("library-stats.json"), r#"{"games":{}}"#).unwrap();
    assert!(!Arc::ptr_eq(&games, &crate::library::sorted_games()));

    let hide = json_req(
        Method::PUT,
        "/api/v1/library/hidden/steam:400",
        serde_json::json!({"hidden": true}),
    );
    let (s, _) = send(&app, hide).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(!boxdir.path().join("library-hidden.json").exists());
    let (s, _) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(s, StatusCode::OK);
}

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

    crate::library::record_launch(&id, Some("kid"));
    crate::library::record_run_time(&id, Some("kid"), std::time::Duration::from_millis(1_500));
    crate::library::record_run_time(&id, Some("kid"), std::time::Duration::from_millis(500));
    // A run credited to an id with no entry is kept but never surfaces.
    crate::library::record_run_time("steam:404", None, std::time::Duration::from_secs(1));

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
    assert!(stats.get("mine").is_none(), "no `as`, no `mine`: {json}");

    // Another profile's launch moves the totals, not the kid's own numbers.
    crate::library::record_launch(&id, Some("enrico"));
    for path in ["/api/v1/library?as=kid", "/api/v1/library/page?as=kid"] {
        let (s, json) = send(&app, get_req(path)).await;
        assert_eq!(s, StatusCode::OK);
        let stats = json
            .get("items")
            .map_or(&json[0]["stats"], |items| &items[0]["stats"]);
        assert_eq!(stats["launch_count"], 2, "{path}: {json}");
        assert_eq!(stats["mine"]["launch_count"], 1, "{path}: {json}");
        assert_eq!(stats["mine"]["play_time_ms"], 2_000, "{path}: {json}");
    }
    let (_, json) = send(&app, get_req("/api/v1/library?as=nobody")).await;
    assert!(json[0]["stats"].get("mine").is_none(), "{json}");
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
            preset: None,
            plane: crate::events::Plane::Native,
            profile: Some(crate::events::ProfileRef {
                id: "kid".into(),
                display_name: "Kid".into(),
            }),
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
    let title = stats.get("custom:stats-run").expect("the run was credited");
    assert_eq!(title.by_profile["kid"], title.totals, "one profile ran it");
    let s = title.totals;
    assert!(s.play_time_ms >= 500, "seen running for a while: {s:?}");
    assert_eq!(s.last_run_ms, s.play_time_ms, "one run: {s:?}");
    assert_eq!(s.launch_count, 0, "the lease never counts launches: {s:?}");
    assert_eq!(s.last_played_unix_ms, 0);
}

/// A metadata source fills a gap, a pick beats it, the replace switch beats own art, and
/// DELETE forgets the source. The env override must cover the whole body.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn metadata_sources_fill_pick_replace_and_forget() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    let json_req = |method: &str, uri: &str, body: serde_json::Value| {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let added = crate::library::add_custom(crate::library::CustomInput {
        title: "Hades".into(),
        art: crate::library::Artwork {
            portrait: Some("https://own/p.png".into()),
            ..Default::default()
        },
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

    let (s, json) = send(
        &app,
        json_req(
            "PUT",
            "/api/v1/library/metadata/sgdb",
            serde_json::json!({
                "matching": "search",
                "entries": [
                    {"id": id, "art": {"portrait": "https://sgdb/p.png", "logo": "https://sgdb/l.png",
                     "hero": "file:///etc/passwd"}, "meta": {"developer": "Supergiant"}},
                    {"id": "not-an-id", "art": {"logo": "https://sgdb/x.png"}}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(
        (json["entries"].as_u64(), json["dropped"].as_u64()),
        (Some(1), Some(2))
    );

    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    let g = &json[0];
    assert_eq!(g["filled"]["logo"], "sgdb", "{json}");
    assert_eq!(g["developer"], "Supergiant");
    assert!(
        g["filled"].get("portrait").is_none(),
        "own art stays: {json}"
    );
    assert!(g["art"]["logo"]
        .as_str()
        .unwrap()
        .starts_with("/api/v1/library/art/"));

    let pick = format!("/api/v1/library/picks/{id}");
    let (s, _) = send(
        &app,
        json_req(
            "PUT",
            &pick,
            serde_json::json!({"kind": "logo", "url": "file:///x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a pick is http(s) only");
    let (s, _) = send(
        &app,
        json_req(
            "PUT",
            &pick,
            serde_json::json!({"kind": "logo", "url": "https://pick/l.png"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(json[0]["filled"]["logo"], "pick");

    let (s, json) = send(
        &app,
        json_req(
            "PUT",
            "/api/v1/library/metadata",
            serde_json::json!([{"id": "sgdb", "enabled": true, "replace": true}]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json[0]["replace"], true, "{json}");
    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(
        json[0]["filled"]["portrait"], "sgdb",
        "replace beats own art: {json}"
    );

    let del = axum::http::Request::delete("/api/v1/library/metadata/sgdb")
        .body(Body::empty())
        .unwrap();
    let (s, json) = send(&app, del).await;
    assert_eq!((s, json["removed"].as_bool()), (StatusCode::OK, Some(true)));
    let (_, json) = send(&app, get_req("/api/v1/library/metadata")).await;
    assert_eq!(json.as_array().map(Vec::len), Some(0), "{json}");
    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(
        json[0]["filled"]["logo"], "pick",
        "the pick outlives the source: {json}"
    );
    assert!(json[0].get("developer").is_none());
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

/// Plain HTTP on the management port answers the plane's route and nothing else, with the
/// cross-origin headers a page served over `http://` needs to read it.
#[tokio::test]
async fn the_plaintext_router_carries_one_route() {
    let app = super::bootstrap_app();
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
        json_req(
            Method::PUT,
            &path,
            serde_json::json!({ "title": "Eden II" }),
        ),
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
    let (s, json) = send(&app, json_req(Method::PUT, &path, body)).await;
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
        Some(TRAY_TOKEN.to_string()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        access_dir.to_path_buf(),
        false,
        None,
        false,
        false,
    )
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
        with_bearer(
            json_req(Method::POST, "/api/v1/plugin-access/requests", body),
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
        with_bearer(get_req("/api/v1/plugin-access/requests"), "demo-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["plugin"], "demo");
    assert_eq!(json["pending"][0]["path"], path);
    assert_eq!(json["pending"][0]["reason"], "library folder");

    for req in [
        json_req(
            Method::POST,
            "/api/v1/plugin-access/requests",
            serde_json::json!({ "paths": [{ "path": path }] }),
        ),
        json_req(
            Method::POST,
            "/api/v1/plugin-access/requests",
            serde_json::json!({ "paths": [] }),
        ),
    ] {
        let (s, _) = send(&app, with_bearer(req, "plugin-secret")).await;
        assert_eq!(
            s,
            StatusCode::FORBIDDEN,
            "the shared runner token has no identity"
        );
    }
    let (s, _) = send(
        &app,
        with_bearer(get_req("/api/v1/plugin-access/requests"), "plugin-secret"),
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
        with_bearer(
            json_req(
                Method::POST,
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
        with_bearer(get_req("/api/v1/plugin-access/requests"), "other-secret"),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["plugin"], "other");
    assert!(json["pending"].as_array().unwrap().is_empty());

    // And no plugin token may decide — demo's own row included.
    let decide = |token: &str| {
        with_bearer(
            json_req(
                Method::POST,
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
        with_bearer(get_req("/api/v1/plugin-access"), "demo-secret"),
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
        with_bearer(
            json_req(
                Method::POST,
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
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
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
        json_req(
            Method::POST,
            "/api/v1/plugin-access/demo/decide",
            serde_json::json!({ "path": deny_path, "decision": "allow" }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// A file a form hands over grants its folder, and the grant goes when the form lets go.
#[tokio::test]
async fn plugin_access_form_grants_go_with_the_form() {
    let dir = tempfile::tempdir().unwrap();
    let saves = tempfile::tempdir().unwrap();
    let app = test_app_access(test_state(), dir.path());
    let folder = saves.path().canonicalize().unwrap();
    let ini = folder.join("game.ini");
    std::fs::write(&ini, "x").unwrap();
    let ini = ini.to_string_lossy().into_owned();

    let (s, json) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/plugin-access/demo/decide",
            serde_json::json!({
                "path": ini, "decision": "allow", "write": true, "form": "game:steam:1",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["grants"][0]["path"], folder.to_string_lossy().as_ref());
    assert_eq!(
        json["grants"][0]["forms"],
        serde_json::json!(["game:steam:1"])
    );

    let release = |form: &str, keep: &[&str]| {
        json_req(
            Method::POST,
            "/api/v1/plugin-access/demo/release",
            serde_json::json!({ "form": form, "keep": keep }),
        )
    };
    let (s, json) = send(&app, release("game:steam:1", &[&ini])).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["grants"].as_array().unwrap().len(), 1, "{json}");
    let (s, json) = send(&app, release("game:steam:1", &[])).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["grants"], serde_json::json!([]));
    assert_eq!(
        send(&app, release("", &[])).await.0,
        StatusCode::BAD_REQUEST
    );
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
        with_bearer(
            json_req(
                Method::POST,
                "/api/v1/plugin-access/requests",
                serde_json::json!({
                    "paths": [
                        { "path": "/" },
                        { "path": "relative/dir" },
                        { "path": path },
                    ],
                    "reason": "games\u{7}\u{2066}\n library"
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
        with_bearer(get_req("/api/v1/plugin-access/requests"), "demo-secret"),
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
        with_bearer(
            json_req(
                Method::POST,
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
        with_bearer(get_req("/api/v1/plugin-access/requests"), "demo-secret"),
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

/// A state whose profile store holds the owner, in a temp file.
fn state_with_profiles(tag: &str) -> Arc<AppState> {
    let state = test_state();
    let path = std::env::temp_dir().join(format!(
        "pf-mgmt-profiles-{tag}-{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let profiles = crate::profiles::Profiles::load_with(Some(path), None);
    profiles.ensure_owner("box").unwrap();
    let _ = state.profiles.set(Arc::new(profiles));
    let _ = state.native_port.set(9777);
    state
}

/// A paired device reads the picker's list and a picture; the console's list and every edit
/// stay the bearer's.
#[tokio::test]
async fn the_cert_lane_reads_profiles_but_never_edits_them() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-prof-cert-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("couch", fp).unwrap();
    let app = test_app_native(state_with_profiles("cert"), np);
    assert_eq!(
        send_cert(&app, get_req("/api/v1/profiles/enumerate"), fp).await,
        StatusCode::OK
    );
    assert_eq!(
        send_cert(&app, get_req("/api/v1/profiles"), fp).await,
        StatusCode::UNAUTHORIZED
    );
    let create = axum::http::Request::post("/api/v1/profiles")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"display_name":"Kid"}"#))
        .unwrap();
    assert_eq!(send_cert(&app, create, fp).await, StatusCode::UNAUTHORIZED);
}

/// Where there are no seats the read says so and a change is a plain 409. The lanes are
/// pinned by `every_route_is_classified_for_the_plugin_and_cert_lanes`.
#[cfg(not(windows))]
#[tokio::test]
async fn seating_off_windows_reads_off_and_refuses_a_change() {
    let app = test_app(state_with_profiles("seating"), None);
    let put = |body: &str| {
        axum::http::Request::put("/api/v1/profiles/seating")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (status, body) = send(&app, get_req("/api/v1/profiles/seating")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({
            "enabled": false,
            "platform": "other",
            "checks": [],
            "allow_rdp_from_network": false,
        })
    );
    for body in [
        r#"{"enabled":true,"allow_rdp_from_network":false}"#,
        r#"{"enabled":false}"#,
    ] {
        let (status, answer) = send(&app, put(body)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(answer["error"], "Seats need Windows Server.");
    }
    assert_eq!(
        send(&app, get_req("/api/v1/profiles/doctor")).await.0,
        StatusCode::CONFLICT
    );
}

/// Create, rename, a picture, the default, then removal; the owner can't be removed.
#[tokio::test]
async fn the_console_edits_profiles() {
    let app = test_app(state_with_profiles("edit"), None);
    let json = |method: &str, path: &str, body: &str| {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let (status, kid) = send(
        &app,
        json(
            "POST",
            "/api/v1/profiles",
            r##"{"display_name":"Kid","accent":"#F97316"}"##,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = kid["id"].as_str().unwrap().to_string();
    assert_eq!(kid["home"], "bigpicture");
    assert_eq!(kid["accent"], "#f97316");
    assert_eq!(kid["seat"]["port"], 9777);

    let (status, sharer) = send(
        &app,
        json(
            "POST",
            "/api/v1/profiles",
            r#"{"display_name":"Ben","seat":false}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(sharer["home"], "desktop");
    assert!(sharer.get("seat").is_none(), "plays on the box: {sharer}");

    let (status, _) = send(
        &app,
        json("POST", "/api/v1/profiles", r#"{"display_name":"kid"}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "names are unique ignoring case"
    );
    let (status, _) = send(
        &app,
        json(
            "POST",
            "/api/v1/profiles",
            r#"{"display_name":"Max","accent":"red"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, renamed) = send(
        &app,
        json(
            "PUT",
            &format!("/api/v1/profiles/{id}"),
            r#"{"display_name":"Lea"}"#,
        ),
    )
    .await;
    assert_eq!(
        (status, renamed["display_name"].as_str()),
        (StatusCode::OK, Some("Lea"))
    );

    let png = axum::http::Request::put(format!("/api/v1/profiles/{id}/avatar"))
        .body(Body::from(b"\x89PNG\r\n\x1a\n picture".to_vec()))
        .unwrap();
    assert_eq!(send(&app, png).await.0, StatusCode::NO_CONTENT);
    let resp = app
        .clone()
        .oneshot({
            let mut r = get_req(&format!("/api/v1/profiles/{id}/avatar"));
            r.headers_mut().insert(
                axum::http::header::AUTHORIZATION,
                axum::http::HeaderValue::from_static("Bearer test-secret"),
            );
            r
        })
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");

    let (status, _) = send(
        &app,
        json(
            "PUT",
            "/api/v1/profiles/default",
            &format!(r#"{{"id":"{id}"}}"#),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = send(&app, get_req("/api/v1/profiles")).await;
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let owner = rows.iter().find(|r| r["owner"] == true).unwrap();
    assert!(owner.get("seat").is_none(), "the owner has no seat");
    assert!(rows
        .iter()
        .any(|r| r["id"] == id.as_str() && r["default"] == true));

    let owner_id = owner["id"].as_str().unwrap();
    let del = |path: String| {
        axum::http::Request::delete(path)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send(&app, del(format!("/api/v1/profiles/{owner_id}")))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        send(&app, del(format!("/api/v1/profiles/{id}?erase=true")))
            .await
            .0,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(&app, del(format!("/api/v1/profiles/{id}"))).await.0,
        StatusCode::NOT_FOUND
    );
}

/// On the cert lane another device's session shows no profile, like its name; one's own does.
#[tokio::test]
async fn status_blanks_another_devices_profile() {
    let _serial = crate::session_status::tests::REGISTRY.lock().await;
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir()
                    .join(format!("pf-mgmt-prof-status-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("couch", fp).unwrap();
    let app = test_app_native(state_with_profiles("status"), np);
    let profile = |name: &str| {
        let mut controls = crate::session_status::SessionControls::open();
        controls.profile = Some(crate::events::ProfileRef {
            id: format!("{name}-id"),
            display_name: name.to_string(),
        });
        controls
    };
    let _mine = crate::session_status::register(crate::session_status::Registration {
        controls: profile("Kid"),
        ..crate::session_status::Registration::fake("deadbeef")
    });
    let _theirs = crate::session_status::register(crate::session_status::Registration {
        controls: profile("Ben"),
        ..crate::session_status::Registration::fake("01234567")
    });
    let mut req = get_req("/api/v1/status");
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    let resp = app.clone().oneshot(req).await.unwrap();
    let body: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let rows = body["sessions"].as_array().unwrap();
    let names: Vec<_> = rows
        .iter()
        .map(|r| r["profile"]["display_name"].as_str())
        .collect();
    assert!(names.contains(&Some("Kid")));
    assert!(!names.contains(&Some("Ben")), "{body}");
}

/// A knock that names a profile shows it to the console; one this host doesn't know shows none.
#[tokio::test]
async fn a_knock_shows_the_profile_it_named() {
    let state = state_with_profiles("knock");
    let store = state.profiles.get().unwrap();
    let owner = store.owner_id().unwrap();
    let owner_name = store.get(&owner).unwrap().display_name;
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(std::env::temp_dir().join(format!("pf-mgmt-knock-{}.json", std::process::id()))),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(state, np.clone());
    np.note_pending("Kid's iPad", "aa11", Some(LAN_KNOCK), Some(&owner));
    np.note_pending("Stranger", "bb22", Some(LAN_KNOCK), Some("ffffffffffff"));
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b[0]["profile"]["id"], owner.as_str(), "{b}");
    assert_eq!(b[0]["profile"]["display_name"], owner_name.as_str());
    assert!(b[1].get("profile").is_none(), "{b}");
}

/// Enumerate marks the profile the asking device's old seat became, and only for that device.
#[tokio::test]
async fn enumerate_marks_the_devices_old_seat() {
    let path = std::env::temp_dir().join(format!("pf-mgmt-legacy-{}.json", std::process::id()));
    std::fs::write(
        &path,
        r#"{"version":1,"profiles":[
          {"id":"4f1c3a9b0e27","display_name":"Enrico","os_account":{"kind":"operator"},"home":"desktop","created_unix":1,"updated_unix":1},
          {"id":"9a3f1c2b7e40","display_name":"Kid","os_account":{"kind":"seat"},"home":"bigpicture","legacy_device":"deadbeefcafe","created_unix":1,"updated_unix":1}
        ]}"#,
    )
    .unwrap();
    let state = test_state();
    let _ = state
        .profiles
        .set(Arc::new(crate::profiles::Profiles::load_with(
            Some(path),
            None,
        )));
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(
                std::env::temp_dir().join(format!("pf-mgmt-legacy-np-{}.json", std::process::id())),
            ),
            None,
            false,
        )
        .unwrap(),
    );
    np.add("couch", "deadbeefcafe").unwrap();
    np.add("phone", "0123456789ab").unwrap();
    let app = test_app_native(state, np);
    let list = |fp: &'static str| {
        let app = app.clone();
        async move {
            let mut req = get_req("/api/v1/profiles/enumerate");
            req.extensions_mut()
                .insert(PeerCertFingerprint(Some(fp.to_string())));
            let resp = app.oneshot(req).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        }
    };
    let couch = list("deadbeefcafe").await;
    assert_eq!(couch[1]["legacy_seat"], true, "{couch}");
    assert!(couch[0].get("legacy_seat").is_none(), "{couch}");
    let phone = list("0123456789ab").await;
    assert!(phone[1].get("legacy_seat").is_none(), "{phone}");
}
