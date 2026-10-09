//! Handler + auth tests for the management API, exercised through `app()`: one file per
//! domain, and here the fixtures they share.

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
mod profiles;
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
