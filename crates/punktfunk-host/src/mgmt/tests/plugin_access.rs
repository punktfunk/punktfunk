//! Plugin access requests: ownership, decisions, form grants and refusals.

use super::*;

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
