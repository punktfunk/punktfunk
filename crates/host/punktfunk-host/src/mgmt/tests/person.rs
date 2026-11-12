//! The person lane: what a device on a locked profile's list may change of that profile, and
//! what every other caller of those routes gets.

use super::profile_view::{profiles, profiles_state, write_fixture, SEAT_P};
use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// On [`SEAT_P`]'s list, kept signed in.
const LISTED: &str = "a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7a7";
/// Paired, on no list.
const OTHER: &str = "b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8b8";

/// Each route a person reaches, with a body it takes.
fn person_writes() -> Vec<(Method, &'static str, Option<serde_json::Value>)> {
    use serde_json::json;
    vec![
        (
            Method::PUT,
            "/api/v1/library/hidden/steam:400",
            Some(json!({"hidden": true})),
        ),
        (
            Method::PUT,
            "/api/v1/library/picks/steam:400",
            Some(json!({"kind": "logo", "url": "https://p.example/l.png"})),
        ),
        (
            Method::PUT,
            "/api/v1/library/overrides/games/steam:400",
            Some(json!({"hz": 50})),
        ),
        (
            Method::PUT,
            "/api/v1/library/scanners/steam",
            Some(json!({"enabled": false})),
        ),
        (
            Method::PUT,
            "/api/v1/session/settings",
            Some(json!({"session_on_game_exit": true})),
        ),
        (Method::DELETE, "/api/v1/session/settings", None),
    ]
}

/// `method` `path` with `?as=<profile>` when one is named.
fn call(
    method: &Method,
    path: &str,
    profile: Option<&str>,
    body: &Option<serde_json::Value>,
) -> axum::http::Request<Body> {
    let uri = match profile {
        Some(p) => format!("{path}?as={p}"),
        None => path.to_string(),
    };
    match body {
        Some(body) => json_req(method.clone(), &uri, body.clone()),
        None => axum::http::Request::builder()
            .method(method.clone())
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    }
}

/// `req` from paired device `fp`, by its certificate.
async fn as_device(
    app: &Router,
    mut req: axum::http::Request<Body>,
    fp: &str,
) -> (StatusCode, serde_json::Value) {
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// Every file under `dir`, by its path there.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(at) = todo.pop() {
        for entry in std::fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                out.insert(path.strip_prefix(dir).unwrap().to_path_buf(), bytes);
            }
        }
    }
    out
}

/// The box of `profile_view` with [`SEAT_P`] locked to [`LISTED`], and both devices paired.
fn locked_box(dir: &Path) -> Router {
    write_fixture(dir);
    let state = profiles_state(dir, profiles());
    let store = state.profiles.get().unwrap().clone();
    let listed = pf_seats::ipc::Device {
        fingerprint: LISTED.into(),
        name: "Kid's tablet".into(),
    };
    let locked = store.locks().edit(SEAT_P, |lock, now| {
        lock.locked = true;
        lock.add(&listed, now)
    });
    assert!(locked.unwrap());
    let np = crate::native_pairing::NativePairing::load_with(
        Some(crate::test_support::scratch().join("pf-person.json")),
        None,
        false,
    )
    .unwrap();
    np.add("Kid's tablet", LISTED).unwrap();
    np.add("Other", OTHER).unwrap();
    test_app_native(state, Arc::new(np))
}

/// A device that names no profile reaches none of the person's routes, as before the lane: it is
/// not admitted. The plugin lane writes no profile's layer, and keeps what it reached without
/// `as`. Nothing of the box changes.
#[tokio::test]
async fn a_device_without_as_and_a_plugin_change_no_profile() {
    let tmp = ConfigDirOverride::new();
    let app = locked_box(tmp.path());
    let before = files(tmp.path());
    for (method, path, body) in person_writes() {
        for fp in [LISTED, OTHER] {
            for profile in [None, Some("")] {
                let (status, _) = as_device(&app, call(&method, path, profile, &body), fp).await;
                assert_eq!(
                    status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} {profile:?}"
                );
            }
        }
        for token in ["plugin-secret", "demo-secret"] {
            let req = with_bearer(call(&method, path, Some(SEAT_P), &body), token);
            let (status, _) = send(&app, req).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{token} {method} {path}");
        }
        if !auth::plugin_may_access(&method, path) {
            let req = with_bearer(call(&method, path, None, &body), "plugin-secret");
            assert_eq!(send(&app, req).await.0, StatusCode::FORBIDDEN, "{path}");
        }
    }
    assert_eq!(files(tmp.path()), before, "nothing changed");
    // Without `as` the plugin lane switches the box's own source and settings, as it did.
    let plugin = |method: Method, path: &str, body| {
        with_bearer(call(&method, path, None, &body), "plugin-secret")
    };
    let off = Some(serde_json::json!({"enabled": false}));
    let exit = Some(serde_json::json!({"session_on_game_exit": true}));
    let calls = [
        (
            Method::PUT,
            "/api/v1/library/scanners/steam",
            off,
            StatusCode::OK,
        ),
        (
            Method::PUT,
            "/api/v1/session/settings",
            exit,
            StatusCode::OK,
        ),
        (
            Method::DELETE,
            "/api/v1/session/settings",
            None,
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (method, path, body, want) in calls {
        let (status, j) = send(&app, plugin(method.clone(), path, body)).await;
        assert_eq!(status, want, "{method} {path}: {j}");
    }
}

/// Before the lane: a device that names a profile changes none of its routes.
#[tokio::test]
async fn a_device_naming_a_profile_changes_nothing() {
    let tmp = ConfigDirOverride::new();
    let app = locked_box(tmp.path());
    let before = files(tmp.path());
    for (method, path, body) in person_writes() {
        for fp in [LISTED, OTHER] {
            let req = call(&method, path, Some(SEAT_P), &body);
            let (status, _) = as_device(&app, req, fp).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }
    assert_eq!(files(tmp.path()), before);
}
