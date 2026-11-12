//! Profiles and seats over the console and cert lanes.

use super::*;

/// A state whose profile store holds the owner, in a temp file.
fn state_with_profiles(tag: &str) -> Arc<AppState> {
    let state = test_state();
    let path = crate::test_support::scratch().join(format!("pf-mgmt-profiles-{tag}.json"));
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
            Some(crate::test_support::scratch().join("pf-mgmt-prof-cert.json")),
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

/// A plugin reads each profile's id, name and picture, and nothing of seats, devices or
/// defaults the console's list carries.
#[tokio::test]
async fn a_plugin_reads_who_is_on_the_box_and_nothing_more() {
    let app = test_app(state_with_profiles("plugin"), None);
    let req = with_bearer(get_req("/api/v1/profiles"), "plugin-secret");
    let (status, list) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 1);
    let keys: Vec<&str> = rows[0]
        .as_object()
        .unwrap()
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(keys, ["display_name", "id"], "{list}");
}

/// `?profile=` points the save routes at that profile's seat: one with none here, the owner's
/// included, is 404 before any copy or file is touched. The env lock keeps a seat test's
/// library dir from making this host a seat host.
#[tokio::test]
async fn a_save_route_for_a_profile_without_a_seat_is_404() {
    let _env = ConfigDirOverride::new();
    let state = state_with_profiles("saves");
    let owner = state.profiles.get().unwrap().owner_id().unwrap();
    let app = test_app(state, None);
    for profile in ["nobody", owner.as_str()] {
        let q = format!("platform=ps2&profile={profile}");
        for path in ["saves", "firmware"] {
            let req = get_req(&format!("/api/v1/emulators/pcsx2/{path}?{q}"));
            let (status, body) = send(&app, req).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path} {profile}: {body}");
        }
        let dir = crate::test_support::scratch().join("pf-mgmt-saves-never-made");
        let body = serde_json::json!({"platform": "ps2", "units": [], "dir": dir});
        let path = format!("/api/v1/emulators/pcsx2/saves/export?profile={profile}");
        let (status, _) = send(&app, json_req(Method::POST, &path, body)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "export {profile}");
        assert!(!dir.exists(), "no folder made for a seat that isn't there");
    }
}

/// A paired device wakes the profile it lands in when it names none; another profile's seat
/// needs the launch grant. The console wakes any.
#[tokio::test]
async fn waking_another_profiles_seat_needs_the_launch_grant() {
    use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_LAUNCH};
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-mgmt-prof-wake.json")),
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
    let (guest, full) = ("aaaa00000049", "bbbb00000049");
    np.add_with_access("guest", guest, access(GRANT_GAMEPAD))
        .unwrap();
    np.add_with_access("full", full, access(GRANT_GAMEPAD | GRANT_LAUNCH))
        .unwrap();
    let state = state_with_profiles("wake");
    let owner = state.profiles.get().unwrap().owner_id().unwrap();
    let app = test_app_native(state, np);
    let create = json_req(
        Method::POST,
        "/api/v1/profiles",
        serde_json::json!({ "display_name": "Kid", "seat": false }),
    );
    let (status, kid) = send(&app, create).await;
    assert_eq!(status, StatusCode::CREATED, "{kid}");
    let kid = kid["id"].as_str().unwrap().to_string();
    let wake = |id: &str| {
        axum::http::Request::post(format!("/api/v1/profiles/{id}/wake"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send_cert(&app, wake(&kid), guest).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(send_cert(&app, wake(&owner), guest).await, StatusCode::OK);
    assert_eq!(send_cert(&app, wake(&kid), full).await, StatusCode::OK);
    assert_eq!(send(&app, wake(&kid)).await.0, StatusCode::OK);
}

/// A locked profile wakes only for a device on its list, its own default profile and a launch
/// grant included; the console wakes it.
#[tokio::test]
async fn a_device_wakes_a_locked_profile_only_from_its_list() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-mgmt-prof-wake-lock.json")),
            None,
            false,
        )
        .unwrap(),
    );
    let (listed, other) = ("ab".repeat(32), "cd".repeat(32));
    np.add("listed", &listed).unwrap();
    np.add("other", &other).unwrap();
    let state = state_with_profiles("wake-lock");
    let profiles = state.profiles.get().unwrap().clone();
    let owner = profiles.owner_id().unwrap();
    let app = test_app_native(state, np);
    let wake = || {
        axum::http::Request::post(format!("/api/v1/profiles/{owner}/wake"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        send_cert(&app, wake(), &other).await,
        StatusCode::OK,
        "open"
    );
    let device = pf_seats::ipc::Device {
        fingerprint: listed.clone(),
        name: "listed".into(),
    };
    let locked = profiles.locks().edit(&owner, |lock, now| {
        lock.locked = true;
        lock.admit(&device, now, true)
    });
    assert!(locked.unwrap());
    assert_eq!(send_cert(&app, wake(), &other).await, StatusCode::FORBIDDEN);
    assert_eq!(send_cert(&app, wake(), &listed).await, StatusCode::OK);
    assert_eq!(send(&app, wake()).await.0, StatusCode::OK);
}

/// A device's own take-turns profile is off the console exactly when it needs a wake: the device
/// wakes it without the launch grant, and still not the owner's.
#[tokio::test]
async fn a_device_wakes_its_own_take_turns_profile_off_the_console() {
    const OWNER: &str = "4f1c3a9b0e27";
    const BEN: &str = "7a77e7a77e01";
    let path = crate::test_support::scratch().join("pf-mgmt-wake-turns.json");
    let file = serde_json::json!({"version": 1, "default_profile_id": BEN, "profiles": [
        {"id": OWNER, "display_name": "Enrico", "os_account": {"kind": "operator"}},
        {"id": BEN, "display_name": "Ben", "os_account": {"kind": "windows",
            "account_name": "Ben", "credential": {"source": "none"}}},
    ]});
    std::fs::write(&path, file.to_string()).unwrap();
    let state = test_state();
    let _ = state
        .profiles
        .set(Arc::new(crate::profiles::Profiles::load_with(
            Some(path),
            None,
        )));
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-mgmt-wake-turns-np.json")),
            None,
            false,
        )
        .unwrap(),
    );
    let guest = "aaaa00000050";
    let access = crate::native_pairing::Access {
        grants: punktfunk_core::quic::GRANT_GAMEPAD,
        expires_unix: None,
        until_disconnect: false,
    };
    np.add_with_access("guest", guest, Some(access)).unwrap();
    let app = test_app_native(state, np);
    let wake = |id: &str| {
        axum::http::Request::post(format!("/api/v1/profiles/{id}/wake"))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send_cert(&app, wake(BEN), guest).await, StatusCode::OK);
    assert_eq!(
        send_cert(&app, wake(OWNER), guest).await,
        StatusCode::FORBIDDEN
    );
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

/// Create, rename, a picture, the default, then removal; the owner can't be removed. A seat
/// needs seats or the door, which a test host has neither of.
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
    let (status, refused) = send(
        &app,
        json("POST", "/api/v1/profiles", r#"{"display_name":"Kid"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");

    let (status, kid) = send(
        &app,
        json(
            "POST",
            "/api/v1/profiles",
            r##"{"display_name":"Kid","accent":"#F97316","seat":false}"##,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = kid["id"].as_str().unwrap().to_string();
    assert_eq!(kid["home"], "desktop");
    assert_eq!(kid["accent"], "#f97316");
    assert!(kid.get("seat").is_none(), "plays on the box: {kid}");

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

    let (status, _) = send(
        &app,
        json(
            "POST",
            "/api/v1/profiles",
            r#"{"display_name":"kid","seat":false}"#,
        ),
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
            r#"{"display_name":"Max","accent":"red","seat":false}"#,
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
            Some(crate::test_support::scratch().join("pf-mgmt-prof-status.json")),
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
            Some(crate::test_support::scratch().join("pf-mgmt-knock.json")),
            None,
            false,
        )
        .unwrap(),
    );
    let app = test_app_native(state, np.clone());
    np.note_pending(
        "Kid's iPad",
        "aa11",
        Some(LAN_KNOCK),
        &Via::DIRECT,
        Some(&owner),
    );
    np.note_pending(
        "Stranger",
        "bb22",
        Some(LAN_KNOCK),
        &Via::DIRECT,
        Some("ffffffffffff"),
    );
    let (_, b) = send(&app, get_req("/api/v1/native/pending")).await;
    assert_eq!(b[0]["profile"]["id"], owner.as_str(), "{b}");
    assert_eq!(b[0]["profile"]["display_name"], owner_name.as_str());
    assert!(b[1].get("profile").is_none(), "{b}");
}

/// The console locks a profile, sets and clears its PIN, puts paired devices on its list, Punktfunk
/// or Moonlight, signs them out and unlocks it; the file holds the PIN's hash, never the PIN.
#[tokio::test]
async fn the_console_locks_a_profile_for_the_devices_it_chooses() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-mgmt-lock-routes.json")),
            None,
            false,
        )
        .unwrap(),
    );
    use crate::profiles::Opens;
    let tv = "ab".repeat(32);
    np.add("Living room TV", &tv).unwrap();
    let dir = crate::test_support::scratch();
    let state = test_state();
    let store = crate::profiles::Profiles::load_with(Some(dir.join("profiles.json")), None);
    store.ensure_owner("box").unwrap();
    let _ = state.profiles.set(Arc::new(store));
    let der = b"moonlight-cert".to_vec();
    let moonlight = hex::encode(crate::https::sha256(&der));
    state.paired.lock().unwrap().push(der);
    let profiles = state.profiles.get().unwrap().clone();
    let owner = profiles.owner_id().unwrap();
    let app = test_app_native(state, np);
    let at = |tail: &str| format!("/api/v1/profiles/{owner}{tail}");
    let call = |method: Method, tail: &str, body: serde_json::Value| {
        let (app, path) = (app.clone(), at(tail));
        async move { send(&app, json_req(method, &path, body)).await }
    };
    let none = serde_json::Value::Null;
    let lock = |locked: bool, pin: bool| serde_json::json!({"locked": locked, "pin": pin, "failed_pins": 0});

    assert_eq!(
        call(Method::GET, "/lock", none.clone()).await.1,
        lock(false, false)
    );
    assert_eq!(
        call(Method::GET, "/devices", none.clone()).await.1,
        serde_json::json!([])
    );
    assert_eq!(
        call(Method::POST, "/lock", none.clone()).await.1,
        lock(true, false)
    );
    assert_eq!(profiles.opens(&owner, Some(&tv)), Opens::No);

    let (status, body) = call(Method::PUT, "/lock/pin", serde_json::json!({"pin": "12a4"})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "profile_pin_invalid");
    let (status, body) = call(Method::PUT, "/lock/pin", serde_json::json!({"pin": "2468"})).await;
    assert_eq!((status, body), (StatusCode::OK, lock(true, true)));
    assert_eq!(profiles.opens(&owner, Some(&tv)), Opens::Pin);
    let file = dir.join(pf_seats::locks::FILE);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        !text.contains("2468") && text.contains("\"salt\""),
        "{text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let add = |fp: &str| serde_json::json!({ "fingerprint": fp });
    let (status, body) = call(Method::POST, "/devices", add(&"cd".repeat(32))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "profile_device_not_paired");
    let (status, body) = call(Method::POST, "/devices", add("not-a-fingerprint")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = call(Method::POST, "/devices", add(&tv.to_uppercase())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["fingerprint"], tv.as_str());
    assert_eq!(body[0]["name"], "Living room TV");
    assert_eq!(body[0]["kind"], "stay");
    let (_, body) = call(Method::POST, "/devices", add(&moonlight)).await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(profiles.opens(&owner, Some(&tv)), Opens::Yes);
    assert_eq!(profiles.opens(&owner, Some(&moonlight)), Opens::Yes);

    let (status, _) = call(Method::DELETE, &format!("/devices/{tv}"), none.clone()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(profiles.opens(&owner, Some(&tv)), Opens::Pin);
    let (status, _) = call(Method::DELETE, "/devices", none.clone()).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(Method::GET, "/devices", none.clone()).await.1,
        serde_json::json!([])
    );
    assert_eq!(
        call(Method::DELETE, "/lock/pin", none.clone()).await.1,
        lock(true, false)
    );
    assert_eq!(profiles.opens(&owner, Some(&moonlight)), Opens::No);
    assert_eq!(
        call(Method::DELETE, "/lock", none.clone()).await.1,
        lock(false, false)
    );
    assert_eq!(profiles.opens(&owner, Some(&tv)), Opens::Yes);
    let (status, _) = send(
        &app,
        json_req(Method::GET, "/api/v1/profiles/c0ffee000000/lock", none),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A lock file that doesn't read keeps every profile locked: the console reads why, and every
/// change is refused until it is fixed.
#[tokio::test]
async fn an_unreadable_lock_file_refuses_every_change() {
    let dir = crate::test_support::scratch();
    std::fs::write(dir.join(pf_seats::locks::FILE), "not a lock file").unwrap();
    let state = test_state();
    let profiles = crate::profiles::Profiles::load_with(Some(dir.join("profiles.json")), None);
    profiles.ensure_owner("box").unwrap();
    let owner = profiles.owner_id().unwrap();
    let _ = state.profiles.set(Arc::new(profiles));
    let app = test_app(state, None);
    for (method, tail) in [
        (Method::GET, "/lock"),
        (Method::DELETE, "/lock"),
        (Method::GET, "/devices"),
        (Method::DELETE, "/devices"),
    ] {
        let path = format!("/api/v1/profiles/{owner}{tail}");
        let (status, body) = send(
            &app,
            json_req(method.clone(), &path, serde_json::Value::Null),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {tail}");
        assert_eq!(body["code"], "profile_locks_unreadable", "{method} {tail}");
    }
}

/// The owner, Kid and Teen on seats of their own, and Guest sharing the owner's desktop, in a
/// store of its own.
fn locked_family(state: &AppState) -> Arc<crate::profiles::Profiles> {
    let dir = crate::test_support::scratch();
    let seat = |id: &str, name: &str, row: &str| {
        serde_json::json!({"id": id, "display_name": name,
            "os_account": {"kind": "seat", "seat": row.repeat(16)}})
    };
    let file = serde_json::json!({"version": 1, "profiles": [
        {"id": FAMILY_OWNER, "display_name": "Enrico", "os_account": {"kind": "operator"}},
        seat(FAMILY_KID, "Kid", "a1"),
        seat(FAMILY_TEEN, "Teen", "b2"),
        {"id": FAMILY_GUEST, "display_name": "Guest", "os_account": {"kind": "operator"}},
    ]});
    std::fs::write(dir.join("profiles.json"), file.to_string()).unwrap();
    let profiles = crate::profiles::Profiles::load_with(Some(dir.join("profiles.json")), None);
    let profiles = Arc::new(profiles);
    let _ = state.profiles.set(profiles.clone());
    profiles
}

const FAMILY_OWNER: &str = "4f1c3a9b0e27";
const FAMILY_KID: &str = "a1a1a1a1a1a1";
const FAMILY_TEEN: &str = "b2b2b2b2b2b2";
const FAMILY_GUEST: &str = "5a1e00000001";

/// A device's picker lists the profiles it may open, and a locked one with a PIN marked `pin`; a
/// locked one with no PIN is absent unless the device is on its list. A profile sharing the
/// owner's desktop shows exactly as the owner does. The console lists all.
#[tokio::test]
async fn the_picker_lists_what_the_device_may_open() {
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-mgmt-picker-lock.json")),
            None,
            false,
        )
        .unwrap(),
    );
    let (listed, other) = ("ab".repeat(32), "cd".repeat(32));
    np.add("listed", &listed).unwrap();
    np.add("other", &other).unwrap();
    let state = test_state();
    let profiles = locked_family(&state);
    let app = test_app_native(state, np);
    let (owner, kid, teen, guest) = (FAMILY_OWNER, FAMILY_KID, FAMILY_TEEN, FAMILY_GUEST);
    let device = pf_seats::ipc::Device {
        fingerprint: listed.clone(),
        name: "listed".into(),
    };
    for (id, pin) in [(owner, true), (kid, false), (teen, true)] {
        let edited = profiles.locks().edit(id, |lock, now| {
            lock.locked = true;
            lock.pin = pin.then(|| pf_seats::locks::PinHash::new("2468")).flatten();
            if id != teen {
                assert!(lock.admit(&device, now, true));
            }
        });
        edited.unwrap();
    }
    let rows = |body: serde_json::Value| {
        let rows = body.as_array().unwrap().iter();
        rows.map(|r| (r["id"].as_str().unwrap().to_string(), r["unlock"].clone()))
            .collect::<Vec<_>>()
    };
    let picker = |fp: String| {
        let app = app.clone();
        async move {
            let mut req = get_req("/api/v1/profiles/enumerate");
            req.extensions_mut().insert(PeerCertFingerprint(Some(fp)));
            let resp = app.oneshot(req).await.unwrap();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            rows(serde_json::from_slice(&body).unwrap())
        }
    };
    let row = |id: &str, word: &str| (id.to_string(), serde_json::json!(word));
    assert_eq!(
        picker(listed).await,
        [
            row(owner, "none"),
            row(kid, "none"),
            row(teen, "pin"),
            row(guest, "none")
        ]
    );
    assert_eq!(
        picker(other).await,
        [row(owner, "pin"), row(teen, "pin"), row(guest, "pin")]
    );
    profiles
        .locks()
        .edit(owner, |lock, _| lock.pin = None)
        .unwrap();
    let rows_now = picker("cd".repeat(32)).await;
    assert_eq!(rows_now, [row(teen, "pin")], "no owner, no guest");
    let (status, all) = send(&app, get_req("/api/v1/profiles/enumerate")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rows(all).len(), 4, "the console sees every profile");
    let (_, console) = send(&app, get_req("/api/v1/profiles")).await;
    assert_eq!(console.as_array().unwrap().len(), 4);
}

/// A profile sharing the owner's desktop has no lock of its own: each lock route on it names
/// whose lock it opens with.
#[tokio::test]
async fn a_profile_sharing_the_owner_s_desktop_takes_no_lock() {
    let state = test_state();
    let profiles = locked_family(&state);
    let app = test_app(state, None);
    let at = |tail: &str| format!("/api/v1/profiles/{FAMILY_GUEST}{tail}");
    let null = serde_json::Value::Null;
    for (method, tail, body) in [
        (Method::GET, "/lock", null.clone()),
        (Method::POST, "/lock", null.clone()),
        (Method::DELETE, "/lock", null.clone()),
        (Method::PUT, "/lock/pin", serde_json::json!({"pin": "2468"})),
        (Method::DELETE, "/lock/pin", null.clone()),
        (Method::GET, "/devices", null.clone()),
        (
            Method::POST,
            "/devices",
            serde_json::json!({"fingerprint": "ab".repeat(32)}),
        ),
        (Method::DELETE, "/devices", null.clone()),
    ] {
        let (status, body) = send(&app, json_req(method.clone(), &at(tail), body)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {tail}");
        assert_eq!(body["code"], "profile_lock_shared", "{method} {tail}");
        assert_eq!(body["params"]["owner"], "Enrico");
    }
    assert!(profiles.locks().get(FAMILY_GUEST) == pf_seats::locks::Lock::default());
}

/// Enumerate marks the profile the asking device's old seat became, and only for that device.
#[tokio::test]
async fn enumerate_marks_the_devices_old_seat() {
    let path = crate::test_support::scratch().join("pf-mgmt-legacy.json");
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
            Some(crate::test_support::scratch().join("pf-mgmt-legacy-np.json")),
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

mod person {
    //! A profile on a person's own Linux account, on a door whose supervisor is a fake.

    use super::*;
    use crate::native::unlock::fake::{self, refused, Fake};
    use pf_seats::ipc::{Command, CommandResult, DeviceEntry, DeviceKind, Needs};

    const PERSON_SEAT: &str = "7a11ce007a11ce007a11ce007a11ce00";
    const WINDOWS_SEAT: &str = "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";
    const OWNER: &str = "4f1c3a9b0e27";
    const PERSON: &str = "7a11ce00e001";
    const WINDOWS: &str = "7a11ce00e002";

    /// A person's own Linux account and a Windows seat on an existing one, both stopped.
    fn ledger() -> Vec<pf_seats::Seat> {
        let mut person = fake::person(PERSON_SEAT, 9780);
        person.runtime = pf_seats::RuntimeStatus::stopped();
        let mut windows = fake::person(WINDOWS_SEAT, 9781);
        (windows.adopted_uid, windows.adopted_sid) = (None, Some("S-1-5-21-1-2-3-1001".into()));
        windows.runtime = pf_seats::RuntimeStatus::stopped();
        vec![person, windows]
    }

    /// A door asking `sup`, with the owner, a profile on each row of [`ledger`], `default` the
    /// profile a device lands in, and `fps` paired with full control.
    fn door(tag: &str, sup: Arc<Fake>, fps: &[&str], default: &str) -> Router {
        door_with(tag, sup, fps, default).0
    }

    /// [`door`], and its profile store.
    fn door_with(
        tag: &str,
        sup: Arc<Fake>,
        fps: &[&str],
        default: &str,
    ) -> (Router, Arc<crate::profiles::Profiles>) {
        let path = crate::test_support::scratch().join(format!("pf-mgmt-person-{tag}.json"));
        let seat = |id: &str, name: &str, row: &str| {
            serde_json::json!({"id": id, "display_name": name,
                "os_account": {"kind": "seat", "seat": row}})
        };
        let file = serde_json::json!({"version": 1, "default_profile_id": default, "profiles": [
            {"id": OWNER, "display_name": "Enrico", "os_account": {"kind": "operator"}},
            seat(PERSON, "Jane", PERSON_SEAT),
            seat(WINDOWS, "Sam", WINDOWS_SEAT),
        ]});
        std::fs::write(&path, file.to_string()).unwrap();
        let state = test_state();
        let profiles = Arc::new(crate::profiles::Profiles::load_with(Some(path), None));
        let _ = state.profiles.set(profiles.clone());
        let np = crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join(format!("pf-mgmt-person-np-{tag}.json"))),
            None,
            false,
        )
        .unwrap();
        for fp in fps {
            np.add("Jane's phone", fp).unwrap();
        }
        (test_app_door(state, Some(Arc::new(np)), sup), profiles)
    }

    /// [`send_cert`] with the answer's body.
    async fn as_device(
        app: &Router,
        mut req: axum::http::Request<Body>,
        fp: &str,
    ) -> (StatusCode, serde_json::Value) {
        req.extensions_mut()
            .insert(PeerCertFingerprint(Some(fp.to_string())));
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    fn post(path: &str) -> axum::http::Request<Body> {
        axum::http::Request::post(path).body(Body::empty()).unwrap()
    }

    fn delete(path: &str) -> axum::http::Request<Body> {
        axum::http::Request::delete(path)
            .body(Body::empty())
            .unwrap()
    }

    /// Each row says what the asking device sends to open it: the supervisor's answer for a
    /// person's own account, `password` when it gives none or no device asks, and `none` for
    /// every other row, a Windows seat on an existing account included.
    #[tokio::test]
    async fn enumerate_says_what_the_asking_device_sends() {
        let (keyed, silent) = ("ab".repeat(32), "cd".repeat(32));
        let key_fp = keyed.clone();
        let sup = Arc::new(Fake::new(ledger(), move |c| match c {
            Command::UnlockState { fingerprint, .. } if *fingerprint == key_fp => {
                Ok(CommandResult::UnlockState { needs: Needs::Key })
            }
            _ => Err(refused("device_list", "read devices.json: denied")),
        }));
        let app = door("enumerate", sup.clone(), &[&keyed, &silent], OWNER);
        let unlock = |rows: &serde_json::Value| {
            let rows = rows.as_array().unwrap().iter();
            rows.map(|r| (r["id"].as_str().unwrap().to_string(), r["unlock"].clone()))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let (status, rows) = as_device(&app, get_req("/api/v1/profiles/enumerate"), &keyed).await;
        assert_eq!(status, StatusCode::OK, "{rows}");
        let want = |person: &str| {
            std::collections::BTreeMap::from([
                (OWNER.to_string(), serde_json::json!("none")),
                (PERSON.to_string(), serde_json::json!(person)),
                (WINDOWS.to_string(), serde_json::json!("none")),
            ])
        };
        assert_eq!(unlock(&rows), want("key"));
        let (_, rows) = as_device(&app, get_req("/api/v1/profiles/enumerate"), &silent).await;
        assert_eq!(unlock(&rows), want("password"), "a supervisor error");
        let (_, rows) = send(&app, get_req("/api/v1/profiles/enumerate")).await;
        assert_eq!(unlock(&rows), want("password"), "no device asks");
        let asked = sup.asked();
        assert_eq!(
            asked.len(),
            2,
            "the person's row alone, once per device: {asked:?}"
        );
        assert!(asked
            .iter()
            .all(|c| matches!(c, Command::UnlockState { id, .. } if id.as_str() == PERSON_SEAT)));
    }

    /// A paired device's key unlock gets its challenge; a device with no key, another row, an
    /// unknown profile and the console are refused.
    #[tokio::test]
    async fn a_device_asks_for_its_own_unlock_challenge() {
        let (keyed, bare) = ("ab".repeat(32), "cd".repeat(32));
        let key_fp = keyed.clone();
        let sup = Arc::new(Fake::new(ledger(), move |c| match c {
            Command::Challenge { id, fingerprint } if *fingerprint == key_fp => {
                Ok(CommandResult::Challenge {
                    challenge: "07".repeat(32),
                    seat: id.clone(),
                })
            }
            _ => Err(refused(
                "unlock_required",
                "This profile needs its account password.",
            )),
        }));
        let app = door("challenge", sup.clone(), &[&keyed, &bare], OWNER);
        let ask = |profile: &str| post(&format!("/api/v1/profiles/{profile}/unlock-challenge"));
        let (status, body) = as_device(&app, ask(PERSON), &keyed).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            serde_json::json!({"challenge": "07".repeat(32), "seat": PERSON_SEAT})
        );
        let (status, body) = as_device(&app, ask(PERSON), &bare).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "This profile needs its account password.");
        let (status, _) = as_device(&app, ask(WINDOWS), &keyed).await;
        assert_eq!(status, StatusCode::CONFLICT, "not a person's own account");
        let (status, _) = as_device(&app, ask("ffffffffffff"), &keyed).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(send(&app, ask(PERSON)).await.0, StatusCode::FORBIDDEN);
        let challenged: Vec<_> = sup.asked();
        assert_eq!(challenged.len(), 2, "{challenged:?}");
        assert!(challenged.iter().all(|c| matches!(c,
            Command::Challenge { id, .. } if id.as_str() == PERSON_SEAT)));
    }

    /// The console lists who is signed in and signs one device or all of them out; a device
    /// can do neither.
    #[tokio::test]
    async fn the_console_lists_and_signs_out_signed_in_devices() {
        let fp = "ab".repeat(32);
        let entry = DeviceEntry {
            fingerprint: fp.clone(),
            name: "Jane's phone".into(),
            kind: DeviceKind::Stay,
            added_unix: 10,
            last_unix: 20,
        };
        let sup = Arc::new(Fake::new(ledger(), move |c| match c {
            Command::Devices { .. } => Ok(CommandResult::Devices {
                devices: vec![entry.clone()],
            }),
            Command::SignOut { .. } => Ok(CommandResult::SignedOut),
            _ => Err(refused("x", "y")),
        }));
        let app = door("devices", sup.clone(), &[&fp], OWNER);
        let devices = format!("/api/v1/profiles/{PERSON}/devices");
        let (status, body) = send(&app, get_req(&devices)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body,
            serde_json::json!([{"fingerprint": fp, "name": "Jane's phone", "kind": "stay",
                "added_unix": 10, "last_unix": 20}])
        );
        let one = format!("{devices}/{fp}");
        assert_eq!(send(&app, delete(&one)).await.0, StatusCode::NO_CONTENT);
        assert_eq!(send(&app, delete(&devices)).await.0, StatusCode::NO_CONTENT);
        let upper = format!("{devices}/{}", fp.to_uppercase());
        assert_eq!(send(&app, delete(&upper)).await.0, StatusCode::BAD_REQUEST);
        // Any other profile answers its lock's list, and the supervisor is not asked.
        let windows = format!("/api/v1/profiles/{WINDOWS}/devices");
        let (status, body) = send(&app, get_req(&windows)).await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!([])));
        // A person's own account opens with its password and takes no lock.
        let add = serde_json::json!({ "fingerprint": fp });
        for req in [
            get_req(&format!("/api/v1/profiles/{PERSON}/lock")),
            post(&format!("/api/v1/profiles/{PERSON}/lock")),
            json_req(Method::POST, &devices, add),
        ] {
            let (status, body) = send(&app, req).await;
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(body["code"], "profile_lock_own_account");
        }
        for req in [get_req(&devices), delete(&devices), delete(&one)] {
            assert_eq!(send_cert(&app, req, &fp).await, StatusCode::UNAUTHORIZED);
        }
        let id = pf_seats::SeatId::parse(PERSON_SEAT).unwrap();
        assert_eq!(
            sup.asked(),
            [
                Command::Devices { id: id.clone() },
                Command::SignOut {
                    id: id.clone(),
                    fingerprint: Some(fp.clone())
                },
                Command::SignOut {
                    id,
                    fingerprint: None
                },
            ]
        );
    }

    /// No route of the door starts a person's own account or sends a device there: a wake from
    /// a device or the console and the console's start are refused, and a device's own play,
    /// with that profile its default, stays on the door. The supervisor and every seat host go
    /// unasked, while a Windows seat on an existing account still wakes as before.
    #[tokio::test]
    async fn no_route_starts_or_reaches_a_person_s_own_account() {
        let fp = "ab".repeat(32);
        let sup = Arc::new(Fake::new(ledger(), |_| Err(refused("x", "y"))));
        let app = door("no-way-round", sup.clone(), &[&fp], PERSON);
        let wake = format!("/api/v1/profiles/{PERSON}/wake");
        let start = format!("/api/v1/profiles/{PERSON}/start");
        let password = "This profile opens with its account password.";
        for (what, (status, body)) in [
            ("a device's wake", as_device(&app, post(&wake), &fp).await),
            ("the console's wake", send(&app, post(&wake)).await),
            ("the console's start", send(&app, post(&start)).await),
        ] {
            assert_eq!(status, StatusCode::CONFLICT, "{what}");
            assert_eq!(body["error"], password, "{what}");
        }
        let end_game = json_req(Method::POST, "/api/v1/game/end", serde_json::json!({}));
        let (status, _) = as_device(&app, end_game, &fp).await;
        assert_ne!(status, StatusCode::BAD_GATEWAY, "answered on the door");
        let windows = format!("/api/v1/profiles/{WINDOWS}/wake");
        let (status, body) = send(&app, post(&windows)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(sup.asked().is_empty(), "{:?}", sup.asked());
        assert!(sup.called().is_empty(), "{:?}", sup.called());
    }

    /// A device's own play goes to its default profile's seat only while it may open that
    /// profile; off a locked profile's list it stays on the door.
    #[tokio::test]
    async fn a_device_s_own_play_reaches_a_locked_profile_s_seat_only_from_its_list() {
        let (listed, other) = ("ab".repeat(32), "cd".repeat(32));
        let sup = Arc::new(Fake::new(ledger(), |_| Err(refused("x", "y"))));
        let (app, profiles) = door_with("own-play-lock", sup.clone(), &[&listed, &other], WINDOWS);
        let end_game = || json_req(Method::POST, "/api/v1/game/end", serde_json::json!({}));
        let (status, _) = as_device(&app, end_game(), &other).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "open: sent to its seat");
        let device = pf_seats::ipc::Device {
            fingerprint: listed.clone(),
            name: "listed".into(),
        };
        let locked = profiles.locks().edit(WINDOWS, |lock, now| {
            lock.locked = true;
            lock.admit(&device, now, true)
        });
        assert!(locked.unwrap());
        let (status, _) = as_device(&app, end_game(), &other).await;
        assert_ne!(status, StatusCode::BAD_GATEWAY, "answered on the door");
        let (status, _) = as_device(&app, end_game(), &listed).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "sent to its seat");
        let seat = crate::mgmt::owner::Target::Seat(WINDOWS_SEAT.into());
        assert_eq!(sup.called(), [seat.clone(), seat]);
    }
}

mod seat_profiles {
    //! Making and removing a profile with a desktop of its own, on a door whose supervisor is a
    //! fake.

    use super::*;
    use crate::native::unlock::fake::{refused, Fake};
    use crate::profiles::{OsAccount, Profiles};
    use pf_seats::ipc::{Command, CommandResult};

    const MADE: &str = "c0ffee00c0ffee00c0ffee00c0ffee00";

    /// The row the supervisor makes for a seat called `name`.
    fn made(name: &str) -> pf_seats::Seat {
        serde_json::from_value(serde_json::json!({
            "id": MADE, "name": name, "account": "pf-seat-1", "display_slot": 12,
            "native_port": 9778, "mgmt_port": 47991, "runtime": { "state": "stopped" },
        }))
        .unwrap()
    }

    /// A door over `store`, asking `sup`.
    fn door(store: Arc<Profiles>, sup: Arc<Fake>) -> Router {
        let state = test_state();
        let _ = state.profiles.set(store);
        test_app_door(state, None, sup)
    }

    fn create(name: &str) -> axum::http::Request<Body> {
        let body = serde_json::json!({ "display_name": name });
        with_bearer(
            json_req(Method::POST, "/api/v1/profiles", body),
            "test-secret",
        )
    }

    /// A profile the store refuses after its seat was made takes the seat back. A seat that won't
    /// go is logged, and the answer is still the store's.
    #[tokio::test]
    async fn a_profile_the_store_refuses_takes_its_seat_back() {
        let dir = crate::test_support::scratch();
        // The store's parent is a file: it reads as empty and every save fails.
        std::fs::write(dir.join("blocker"), b"").unwrap();
        let path = dir.join("blocker").join("profiles.json");
        let store = Arc::new(Profiles::load_with(Some(path), None));
        let sup = Arc::new(Fake::new(Vec::new(), |c| match c {
            Command::Create(seat) => Ok(CommandResult::Created {
                seat: made(&seat.name),
            }),
            _ => Err(refused("account_signed_in", "Someone is signed in there.")),
        }));
        let (status, body) = send(&door(store, sup.clone()), create("Kid")).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        let id = pf_seats::SeatId::parse(MADE).unwrap();
        let asked = sup.asked();
        assert!(matches!(asked[0], Command::Create(_)), "{asked:?}");
        assert_eq!(
            asked[1..],
            [Command::Stop { id: id.clone() }, Command::Delete { id }]
        );
    }

    /// A console that stops waiting while the seat is made still gets the profile written beside
    /// it.
    #[tokio::test]
    async fn a_create_the_caller_stopped_waiting_for_still_writes_the_profile() {
        let path = crate::test_support::scratch().join("profiles.json");
        let store = Arc::new(Profiles::load_with(Some(path), None));
        let began = Arc::new(tokio::sync::Notify::new());
        let ask = began.clone();
        let sup = Arc::new(Fake::new(Vec::new(), move |c| match c {
            Command::Create(seat) => {
                ask.notify_one();
                std::thread::sleep(std::time::Duration::from_millis(300));
                Ok(CommandResult::Created {
                    seat: made(&seat.name),
                })
            }
            _ => Err(refused("x", "y")),
        }));
        let call = door(store.clone(), sup.clone()).oneshot(create("Kid"));
        tokio::select! {
            _ = call => panic!("answered before the seat was made"),
            _ = began.notified() => {}
        }
        let mut kid = None;
        for _ in 0..50 {
            kid = store.list().into_iter().find(|p| p.display_name == "Kid");
            if kid.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let kid = kid.expect("the profile is written");
        assert_eq!(
            kid.os_account,
            OsAccount::Seat {
                seat: Some(MADE.into())
            }
        );
        assert_eq!(sup.asked().len(), 1, "{:?}", sup.asked());
    }

    /// A seat's row keeps its name through its profile's rename, and the ledger takes no second
    /// row by it: the host refuses that name before the supervisor, in its own words. The
    /// owner's row, named for their login, holds no name.
    #[tokio::test]
    async fn a_name_a_seat_row_still_holds_is_refused_before_the_supervisor() {
        let path = crate::test_support::scratch().join("profiles.json");
        let store = Arc::new(Profiles::load_with(Some(path), None));
        let mut owner = made("jane");
        owner.id = pf_seats::SeatId::parse("0e".repeat(16)).unwrap();
        owner.owner = true;
        let sup = Arc::new(Fake::new(vec![made("Kid"), owner], |c| match c {
            Command::Create(seat) => Ok(CommandResult::Created {
                seat: made(&seat.name),
            }),
            _ => Err(refused("x", "y")),
        }));
        let app = door(store, sup.clone());
        let (status, body) = send(&app, create("kid")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "profile_name_taken");
        assert!(sup.asked().is_empty(), "{:?}", sup.asked());
        let (status, body) = send(&app, create("Jane")).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    /// A seat profile leaves the box's file before its seat stops: the seat's host reads that
    /// file and closes each session as removed. A seat that won't go puts the profile back as
    /// it was, and the next try removes both.
    #[tokio::test]
    async fn a_seat_profile_leaves_the_store_before_its_seat_stops() {
        const KID: &str = "9a3f1c2b7e40";
        let path = crate::test_support::scratch().join("profiles.json");
        let file = serde_json::json!({"version": 1, "default_profile_id": KID, "profiles": [
            {"id": "4f1c3a9b0e27", "display_name": "Enrico", "os_account": {"kind": "operator"}},
            {"id": KID, "display_name": "Kid", "os_account": {"kind": "seat", "seat": MADE}},
        ]});
        std::fs::write(&path, file.to_string()).unwrap();
        let store = Arc::new(Profiles::load_with(Some(path.clone()), None));
        let listed_at_stop = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = listed_at_stop.clone();
        let deletes = std::sync::atomic::AtomicUsize::new(0);
        let sup = Arc::new(Fake::new(vec![made("Kid")], move |c| match c {
            Command::Stop { .. } => {
                let on_disk = std::fs::read_to_string(&path).unwrap();
                seen.lock().unwrap().push(on_disk.contains(KID));
                Ok(CommandResult::Stopped { seat: made("Kid") })
            }
            Command::Delete { .. } if deletes.fetch_add(1, Ordering::SeqCst) == 0 => {
                Err(refused("account_signed_in", "Someone is signed in there."))
            }
            Command::Delete { id } => Ok(CommandResult::Deleted { id: id.clone() }),
            _ => Err(refused("x", "y")),
        }));
        let app = door(store.clone(), sup);
        let remove = || {
            axum::http::Request::delete(format!("/api/v1/profiles/{KID}?erase=true"))
                .body(Body::empty())
                .unwrap()
        };
        let (status, body) = send(&app, remove()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(store.get(KID).is_some(), "put back");
        assert_eq!(store.default_profile_id().as_deref(), Some(KID));
        let (status, body) = send(&app, remove()).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
        assert!(store.get(KID).is_none());
        assert_eq!(*listed_at_stop.lock().unwrap(), [false, false]);
    }
}
