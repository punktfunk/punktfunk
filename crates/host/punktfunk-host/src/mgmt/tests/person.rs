//! The person lane: what a device on a locked profile's list may change of that profile, and
//! what every other caller of those routes gets.

use super::profile_view::{profiles, profiles_state, write_fixture, OWNER, SEAT_P, SHARER};
use super::*;
use crate::mgmt::person::person_route;
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
            "/api/v1/library/overrides/sources/steam",
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
pub(super) fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
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
fn locked_box(dir: &Path) -> (Router, Arc<crate::profiles::Profiles>) {
    write_fixture(dir);
    let state = profiles_state(dir, profiles());
    let store = state.profiles.get().unwrap().clone();
    add(&store, SEAT_P, LISTED);
    let np = crate::native_pairing::NativePairing::load_with(
        Some(crate::test_support::scratch().join("pf-person.json")),
        None,
        false,
    )
    .unwrap();
    np.add("Kid's tablet", LISTED).unwrap();
    np.add("Other", OTHER).unwrap();
    (test_app_native(state, Arc::new(np)), store)
}

/// Locks profile `id` with device `fp` on its list, as the operator adds one.
fn add(store: &crate::profiles::Profiles, id: &str, fp: &str) {
    let device = pf_seats::ipc::Device {
        fingerprint: fp.into(),
        name: "Kid's tablet".into(),
    };
    let added = store.locks().edit(id, |lock, now| {
        lock.locked = true;
        lock.add(&device, now)
    });
    assert!(added.unwrap());
}

/// The routes on the lane, from [`person_writes`].
fn lane() -> Vec<(Method, &'static str, Option<serde_json::Value>)> {
    let on = |(m, p, _): &(Method, &str, _)| person_route(m.as_str(), p);
    person_writes().into_iter().filter(on).collect()
}

/// `files` but those in profile `id`'s own layer.
pub(super) fn outside(files: &BTreeMap<PathBuf, Vec<u8>>, id: &str) -> BTreeMap<PathBuf, Vec<u8>> {
    let layer = Path::new("library-profiles").join(id);
    let theirs = |(p, _): &(&PathBuf, &Vec<u8>)| !p.starts_with(&layer);
    let kept = files.iter().filter(theirs);
    kept.map(|(p, b)| (p.clone(), b.clone())).collect()
}

/// `req` as the door sends it: this host's token, vouching for device `fp` and, when named, for
/// it being `person`'s person.
fn vouched(
    mut req: axum::http::Request<Body>,
    fp: &str,
    person: Option<&str>,
) -> axum::http::Request<Body> {
    let value = |v: &str| axum::http::HeaderValue::from_str(v).unwrap();
    let headers = req.headers_mut();
    headers.insert(crate::mgmt::auth::VOUCH_HEADER, value(fp));
    if let Some(person) = person {
        headers.insert(crate::mgmt::person::VOUCH_HEADER, value(person));
    }
    req
}

/// The answer to a device that is not the person.
fn is_refusal((status, body): &(StatusCode, serde_json::Value)) -> bool {
    *status == StatusCode::FORBIDDEN && body["code"] == "profile_change_not_allowed"
}

/// A device that names no profile reaches none of the person's routes, as before the lane: it is
/// not admitted. The plugin lane writes no profile's layer, and keeps what it reached without
/// `as`. Nothing of the box changes.
#[tokio::test]
async fn a_device_without_as_and_a_plugin_change_no_profile() {
    let tmp = ConfigDirOverride::new();
    let (app, _) = locked_box(tmp.path());
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

/// A profile's own route left off the lane, an art pick, takes no device that names a profile:
/// the host fetches a pick's URL itself.
#[tokio::test]
async fn a_route_off_the_lane_takes_no_device() {
    let tmp = ConfigDirOverride::new();
    let (app, _) = locked_box(tmp.path());
    let before = files(tmp.path());
    for (method, path, body) in person_writes() {
        if person_route(method.as_str(), path) {
            continue;
        }
        for fp in [LISTED, OTHER] {
            let req = call(&method, path, Some(SEAT_P), &body);
            let (status, _) = as_device(&app, req, fp).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        }
    }
    assert_eq!(files(tmp.path()), before);
}

/// A device on a locked profile's list changes that profile's own layer and nothing of the
/// box's. A device off the list, the owner's id, an open profile and an unknown id change
/// nothing.
#[tokio::test]
async fn a_locked_profile_s_device_changes_its_layer_alone() {
    let tmp = ConfigDirOverride::new();
    let (app, _) = locked_box(tmp.path());
    assert!(!lane().is_empty());
    for (method, path, body) in lane() {
        let at = |profile| call(&method, path, Some(profile), &body);
        let before = files(tmp.path());
        let refused = as_device(&app, at(SEAT_P), OTHER).await;
        assert!(is_refusal(&refused), "{method} {path}: {refused:?}");
        // The owner is never a person; the sharer opens with the owner's lock, which is open.
        for profile in [OWNER, SHARER] {
            let answer = as_device(&app, at(profile), LISTED).await;
            assert_eq!(answer, refused, "{method} {path} as {profile}");
        }
        let (status, unknown) = as_device(&app, at("0123456789ab"), LISTED).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {path}");
        assert_eq!(unknown["code"], "profile_unknown");
        assert_eq!(files(tmp.path()), before, "{method} {path}");

        let (status, j) = as_device(&app, at(SEAT_P), LISTED).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {j}");
        let after = files(tmp.path());
        assert_eq!(outside(&after, SEAT_P), outside(&before, SEAT_P), "{path}");
        assert_ne!(after, before, "{method} {path} wrote the profile's layer");
    }
}

/// What a device adds stays bounded by the library: it hides a title or sets a title's or a
/// source's entry only when its profile's library lists it, and takes any of them away whatever
/// the title. The console still writes a title that is gone for now.
#[tokio::test]
async fn a_device_adds_only_what_its_library_lists() {
    use serde_json::json;
    let tmp = ConfigDirOverride::new();
    let (app, _) = locked_box(tmp.path());
    let ghost = "steam:999999";
    let hide = format!("/api/v1/library/hidden/{ghost}");
    let game = format!("/api/v1/library/overrides/games/{ghost}");
    let source = "/api/v1/library/overrides/sources/nosuchstore".to_string();
    for (path, adds, takes) in [
        (&hide, json!({"hidden": true}), json!({"hidden": false})),
        (&game, json!({"hz": 50}), json!({})),
        (&source, json!({"hz": 50}), json!({})),
    ] {
        let at = |body| call(&Method::PUT, path, Some(SEAT_P), &Some(body));
        let before = files(tmp.path());
        let (status, j) = as_device(&app, at(adds), LISTED).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {j}");
        assert_eq!(files(tmp.path()), before, "{path}");
        let (status, j) = as_device(&app, at(takes), LISTED).await;
        assert_eq!(status, StatusCode::OK, "{path}: {j}");
    }
    let console = call(
        &Method::PUT,
        &hide,
        Some(SEAT_P),
        &Some(json!({"hidden": true})),
    );
    assert_eq!(send(&app, console).await.0, StatusCode::OK);
}

/// An open profile, a device off a locked one's list, and one that needs its PIN hear the same
/// sentence: no device learns which profiles are locked.
#[tokio::test]
async fn one_answer_for_an_open_profile_a_stranger_and_a_pin() {
    let tmp = ConfigDirOverride::new();
    let (app, store) = locked_box(tmp.path());
    let but_lock = |dir: &Path| {
        let mut all = files(dir);
        all.remove(Path::new(pf_seats::locks::FILE));
        all
    };
    let before = but_lock(tmp.path());
    let pin = || pf_seats::locks::PinHash::new("2468");
    for (method, path, body) in lane() {
        let at = || call(&method, path, Some(SEAT_P), &body);
        let stranger = as_device(&app, at(), OTHER).await;
        assert!(is_refusal(&stranger), "{path}: {stranger:?}");
        store.locks().edit(SEAT_P, |l, _| l.pin = pin()).unwrap();
        assert_eq!(
            store.opens(SEAT_P, Some(OTHER)),
            crate::profiles::Opens::Pin
        );
        assert_eq!(as_device(&app, at(), OTHER).await, stranger, "{path}: PIN");
        store.locks().edit(SEAT_P, |l, _| l.locked = false).unwrap();
        assert_eq!(
            as_device(&app, at(), LISTED).await,
            stranger,
            "{path}: open"
        );
        store
            .locks()
            .edit(SEAT_P, |l, _| (l.locked, l.pin) = (true, None))
            .unwrap();
    }
    assert_eq!(but_lock(tmp.path()), before, "only the lock changed");
}

/// A profile that shares the owner's desktop opens with the owner's lock: the owner's device is
/// that profile's person, and never the owner's own.
#[tokio::test]
async fn a_sharer_s_person_changes_the_sharer_never_the_owner() {
    let tmp = ConfigDirOverride::new();
    let (app, store) = locked_box(tmp.path());
    add(&store, OWNER, OTHER);
    for (method, path, body) in lane() {
        let at = |profile| call(&method, path, Some(profile), &body);
        let before = files(tmp.path());
        let owner = as_device(&app, at(OWNER), OTHER).await;
        assert!(is_refusal(&owner), "{method} {path}: {owner:?}");
        assert!(is_refusal(&as_device(&app, at(SHARER), LISTED).await));
        assert_eq!(files(tmp.path()), before, "{method} {path}");
        let (status, j) = as_device(&app, at(SHARER), OTHER).await;
        // A sharer plays with the host's session settings: it has none of its own.
        let session = path == "/api/v1/session/settings";
        let want = if session {
            StatusCode::CONFLICT
        } else {
            StatusCode::OK
        };
        assert_eq!(status, want, "{method} {path}: {j}");
        let after = files(tmp.path());
        assert_eq!(outside(&after, SHARER), outside(&before, SHARER), "{path}");
        assert_eq!(after != before, !session, "{method} {path}");
    }
}

/// Signing the device out, or its presence from a PIN ending, takes the lane away on the next
/// call.
#[tokio::test]
async fn signing_out_takes_the_lane_away_at_once() {
    let tmp = ConfigDirOverride::new();
    let (app, store) = locked_box(tmp.path());
    let (method, path, body) = lane().remove(0);
    let at = || call(&method, path, Some(SEAT_P), &body);
    assert_eq!(as_device(&app, at(), LISTED).await.0, StatusCode::OK);
    let out = format!("/api/v1/profiles/{SEAT_P}/devices/{LISTED}");
    let out = axum::http::Request::delete(out)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, out).await.0, StatusCode::NO_CONTENT);
    assert!(is_refusal(&as_device(&app, at(), LISTED).await));

    // A PIN let it in for now: while present it is the person, after its grace it isn't.
    let device = pf_seats::ipc::Device {
        fingerprint: LISTED.into(),
        name: "Kid's tablet".into(),
    };
    let admitted = store
        .locks()
        .edit(SEAT_P, |l, now| l.admit(&device, now, false));
    assert!(admitted.unwrap());
    assert_eq!(as_device(&app, at(), LISTED).await.0, StatusCode::OK);
    let left = |l: &mut pf_seats::locks::Lock, now: i64| l.list.left(LISTED, now - 601);
    store.locks().edit(SEAT_P, left).unwrap();
    assert!(is_refusal(&as_device(&app, at(), LISTED).await));
}

/// The owner's host, which reads the box's profiles, decides no person itself: it takes the
/// door's word, with its own token, for the profile `as` names and never for the owner. It needs
/// no pairing of its own for that device. The door's word means nothing on any other route.
#[tokio::test]
async fn the_owner_s_host_takes_the_door_s_word_alone() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let trust = tempfile::tempdir().unwrap();
    let copy = trust.path().join("profiles.json");
    std::fs::write(&copy, profiles().to_string()).unwrap();
    let state = test_state();
    let store = crate::profiles::Profiles::load_box(copy, Some("0".repeat(32)));
    assert!(store.reads_box());
    let _ = state.profiles.set(Arc::new(store));
    // Its pairing copy holds the owner's devices alone, as while the owner's profile is locked.
    let np = crate::native_pairing::NativePairing::load_with(
        Some(crate::test_support::scratch().join("pf-person-owner.json")),
        None,
        false,
    )
    .unwrap();
    np.add("Other", OTHER).unwrap();
    let app = test_app_native(state, Arc::new(np));
    for (method, path, body) in lane() {
        let at = |profile| call(&method, path, profile, &body);
        let before = files(tmp.path());
        for (fp, profile, person) in [
            (LISTED, Some(SHARER), Some(SEAT_P)),
            (LISTED, Some(OWNER), Some(OWNER)),
            (OTHER, Some(SEAT_P), None),
        ] {
            let answer = send(&app, vouched(at(profile), fp, person)).await;
            assert!(
                is_refusal(&answer),
                "{path} {profile:?} {person:?}: {answer:?}"
            );
        }
        let direct = as_device(&app, at(Some(SEAT_P)), OTHER).await;
        assert!(is_refusal(&direct), "{path}: {direct:?}");
        for (fp, profile, person) in [(LISTED, Some(SEAT_P), None), (LISTED, None, Some(SEAT_P))] {
            let (status, _) = send(&app, vouched(at(profile), fp, person)).await;
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "{path} {profile:?} {person:?}"
            );
        }
        let plugin = vouched(at(Some(SEAT_P)), LISTED, Some(SEAT_P));
        let plugin = with_bearer(plugin, "plugin-secret");
        assert_eq!(send(&app, plugin).await.0, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(files(tmp.path()), before, "{method} {path}");

        let (status, j) = send(&app, vouched(at(Some(SEAT_P)), LISTED, Some(SEAT_P))).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {j}");
        let after = files(tmp.path());
        assert_eq!(outside(&after, SEAT_P), outside(&before, SEAT_P), "{path}");
        assert_ne!(after, before, "{method} {path}");
    }
    // An install the door vouches a person for is still the device's own, by its pairing here.
    let install = axum::http::Request::post("/api/v1/library/install/steam:400")
        .body(Body::empty())
        .unwrap();
    let (status, _) = send(&app, vouched(install, LISTED, Some(SEAT_P))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// The box's own host decides for itself: a door's word changes nothing there.
#[tokio::test]
async fn the_box_s_host_hears_no_door() {
    let tmp = ConfigDirOverride::new();
    let (app, _) = locked_box(tmp.path());
    let before = files(tmp.path());
    for (method, path, body) in lane() {
        let req = vouched(
            call(&method, path, Some(SEAT_P), &body),
            OTHER,
            Some(SEAT_P),
        );
        let answer = send(&app, req).await;
        assert!(is_refusal(&answer), "{method} {path}: {answer:?}");
    }
    assert_eq!(files(tmp.path()), before);
}

/// `enumerate` as device `fp`: each row's id and whether it carries `person`.
async fn flags(app: &Router, fp: &str) -> Vec<(String, bool)> {
    let (status, rows) = as_device(app, get_req("/api/v1/profiles/enumerate"), fp).await;
    assert_eq!(status, StatusCode::OK, "{rows}");
    let rows = rows.as_array().expect("rows").clone();
    let flag = |r: &serde_json::Value| match r.get("person") {
        None => false,
        Some(v) => v == true,
    };
    let id = |r: &serde_json::Value| r["id"].as_str().unwrap().to_string();
    rows.iter().map(|r| (id(r), flag(r))).collect()
}

/// Whether the person lane takes a write from `fp` as profile `id`: a show, which any title takes.
async fn takes(app: &Router, fp: &str, id: &str) -> bool {
    let body = Some(serde_json::json!({"hidden": false}));
    let req = call(
        &Method::PUT,
        "/api/v1/library/hidden/steam:400",
        Some(id),
        &body,
    );
    as_device(app, req, fp).await.0 == StatusCode::OK
}

/// Each row's `person` is the lane's own answer for that device: on exactly the rows whose write
/// the lane takes, never the owner's, never an open profile's, gone with a PIN or a sign-out, and
/// never on the operator's list.
#[tokio::test]
async fn enumerate_marks_exactly_the_rows_whose_writes_the_lane_takes() {
    let tmp = ConfigDirOverride::new();
    let (app, store) = locked_box(tmp.path());
    let agree = |app: Router, fp: &'static str| async move {
        let rows = flags(&app, fp).await;
        for (id, person) in &rows {
            assert_eq!(*person, takes(&app, fp, id).await, "{fp} as {id}");
        }
        rows.into_iter()
            .filter(|(_, p)| *p)
            .map(|(id, _)| id)
            .collect::<Vec<_>>()
    };
    assert_eq!(agree(app.clone(), LISTED).await, [SEAT_P]);
    assert!(agree(app.clone(), OTHER).await.is_empty());
    // The owner's lock lets OTHER in: it is the sharer's person, never the owner's.
    add(&store, OWNER, OTHER);
    assert_eq!(agree(app.clone(), OTHER).await, [SHARER]);
    // A device that needs the PIN is listed for it, and is no person.
    store
        .locks()
        .edit(SEAT_P, |l, _| l.pin = pf_seats::locks::PinHash::new("2468"))
        .unwrap();
    let out = format!("/api/v1/profiles/{SEAT_P}/devices/{LISTED}");
    let out = axum::http::Request::delete(out)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, out).await.0, StatusCode::NO_CONTENT);
    assert!(agree(app.clone(), LISTED).await.is_empty());
    let (_, operator) = send(&app, get_req("/api/v1/profiles/enumerate")).await;
    let rows = operator.as_array().expect("rows");
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|r| r.get("person").is_none()), "{operator}");
}

/// `(title, hidden)` of a page's rows.
fn page_rows(page: &serde_json::Value) -> Vec<(String, bool)> {
    let rows = page["items"].as_array().cloned().unwrap_or_default();
    let row = |g: &serde_json::Value| {
        (
            g["title"].as_str().unwrap().to_string(),
            g["hidden"] == true,
        )
    };
    rows.iter().map(row).collect()
}

/// A person lists what its profile hides, and none the box hides. Every other caller of either
/// library read gets the answer it gets without the flag, whatever value it sends.
#[tokio::test]
async fn only_a_person_lists_what_its_profile_hides() {
    let tmp = ConfigDirOverride::new();
    let (app, _) = locked_box(tmp.path());
    let page = |q: &str| format!("/api/v1/library/page?{q}");
    let (status, mine) = as_device(
        &app,
        get_req(&page(&format!("as={SEAT_P}&hidden=true"))),
        LISTED,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(page_rows(&mine), [("Half-Life".to_string(), true)]);
    assert_eq!(mine["total"], 1);

    let today = |fp: &'static str, path: String| {
        let app = app.clone();
        async move {
            let plain = as_device(&app, get_req(&path), fp).await;
            for flag in ["hidden=true", "hidden=1", "hidden=TRUE", "hidden="] {
                let sep = if path.contains('?') { '&' } else { '?' };
                let asked = as_device(&app, get_req(&format!("{path}{sep}{flag}")), fp).await;
                assert_eq!(asked, plain, "{fp} {path} {flag}");
            }
        }
    };
    for route in ["/api/v1/library", "/api/v1/library/page"] {
        for fp in [LISTED, OTHER] {
            today(fp, route.to_string()).await;
            for id in [SEAT_P, OWNER, SHARER, "0123456789ab"] {
                let path = format!("{route}?as={id}");
                if !(fp == LISTED && id == SEAT_P && route.ends_with("page")) {
                    today(fp, path).await;
                }
            }
        }
    }
    // The person's own flag only reads `true`, and the paged route only.
    let plain = page(&format!("as={SEAT_P}"));
    let (_, without) = as_device(&app, get_req(&plain), LISTED).await;
    assert!(!page_rows(&without)
        .iter()
        .any(|(t, _)| t == "Half-Life" || t == "Dota"));
    for flag in ["hidden=1", "hidden=TRUE", "hidden="] {
        let asked = as_device(&app, get_req(&format!("{plain}&{flag}")), LISTED).await;
        assert_eq!(asked.1, without, "{flag}");
    }

    // The operator's lane narrows to what it already sees.
    let (_, ops) = send(&app, get_req(&page(&format!("as={SEAT_P}&hidden=true")))).await;
    assert_eq!(page_rows(&ops), [("Half-Life".to_string(), true)]);
    let (_, ops) = send(&app, get_req(&page("hidden=true"))).await;
    assert_eq!(page_rows(&ops), [("Dota".to_string(), true)]);

    // Signed out, the person's list is gone with its lane.
    let out = format!("/api/v1/profiles/{SEAT_P}/devices/{LISTED}");
    let out = axum::http::Request::delete(out)
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, out).await.0, StatusCode::NO_CONTENT);
    today(LISTED, plain).await;
}

/// The lane's pins. Each route is a profile's data, takes `as`, and is the owner's host's on a
/// door; none was a route a device could write before the lane, and a device writes nothing
/// else. A route added to `PERSON` fails here until it has a lane test.
#[test]
fn the_person_lane_is_pinned() {
    use crate::mgmt::owner::{device_route, Side};
    use crate::mgmt::scope::{scope_of, Scope};
    use punktfunk_core::mgmt_routes::PERSON;
    use std::collections::BTreeSet;
    // Every write a paired device reached before the person's routes.
    const BEFORE: &[(&str, &str)] = &[
        ("POST", "/api/v1/actions/{id}"),
        ("POST", "/api/v1/client-logs"),
        ("PUT", "/api/v1/display/clients/{fingerprint}"),
        ("POST", "/api/v1/game/end"),
        ("POST", "/api/v1/library/install/{id}"),
        ("DELETE", "/api/v1/library/install/{id}"),
        ("POST", "/api/v1/library/install/{id}/pause"),
        ("POST", "/api/v1/profiles/{id}/unlock-challenge"),
        ("POST", "/api/v1/profiles/{id}/wake"),
    ];
    let doc: serde_json::Value = serde_json::from_str(&openapi_json()).unwrap();
    let mut writes = BTreeSet::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for (method, op) in ops.as_object().unwrap() {
            let m = Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            if m == Method::GET || !auth::cert_may_access(&m, path) {
                continue;
            }
            writes.insert((m.to_string(), path.clone()));
            let params = op["parameters"].as_array().cloned().unwrap_or_default();
            let takes_as = params
                .iter()
                .any(|p| p["name"] == "as" && p["in"] == "query");
            assert!(takes_as || !person_route(m.as_str(), path), "{m} {path}");
        }
    }
    let concrete = |path: &str| path.replace("{id}", "steam:400");
    for &(method, path) in PERSON {
        assert!(!BEFORE.contains(&(method, path)), "{method} {path}");
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        let scope = scope_of(&m, &concrete(path));
        assert_eq!(scope, Some(Scope::ProfileData), "{method} {path}");
        let rest = path.strip_prefix("/api/v1/").unwrap();
        assert_eq!(device_route(&m, rest), Some(Side::Owner), "{method} {path}");
        let matches = punktfunk_core::mgmt_routes::path_matches;
        let tested = |(tm, tp, _): &(Method, &str, _)| *tm == m && matches(path, tp);
        let tested = person_writes().iter().any(tested);
        assert!(tested, "{method} {path} has no row in person_writes");
    }
    let rows = BEFORE.iter().chain(PERSON);
    let want: BTreeSet<_> = rows.map(|(m, p)| (m.to_string(), p.to_string())).collect();
    assert_eq!(writes, want, "what a paired device may write");
}
