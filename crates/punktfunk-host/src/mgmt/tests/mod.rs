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
mod display;
mod events;
mod library;
mod native_pairing;
mod plugin_access;
mod plugins;
mod routes;
mod sessions;
mod tunnel;

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

/// Serializes events-route tests: they share the process-global bus and the connection-cap
/// counter, so the cap test must never 503 a concurrently running stream test.
static EVENTS_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
