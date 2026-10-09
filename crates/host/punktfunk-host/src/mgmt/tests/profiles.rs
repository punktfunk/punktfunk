//! Profiles and seats over the console and cert lanes.

use super::*;

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
