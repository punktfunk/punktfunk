//! A seat host on a person's own account: what the box may no longer make it run.

use super::*;

/// Set on the child only: the config dir it reads.
const CHILD: &str = "PF_OWN_ACCOUNT_TEST_CHILD";

/// Hooks, an unreviewed plugin and an emulator copy named by hand would run as the person, so
/// the host refuses each, and a hook already stored never runs. The marker is process
/// environment, so the body runs in a child of this test binary started with it.
#[tokio::test]
async fn a_person_s_own_account_refuses_what_would_run_code_as_them() {
    let Some(config) = std::env::var_os(CHILD) else {
        let config = tempfile::tempdir().unwrap();
        let stored = serde_json::json!({"hooks": [
            {"on": "game.launching", "run": "true", "hold": true},
            {"on": "stream.*", "run": "true"},
        ]});
        std::fs::write(config.path().join("hooks.json"), stored.to_string()).unwrap();
        let module = module_path!().split_once("::").expect("crate::module").1;
        let test = format!("{module}::a_person_s_own_account_refuses_what_would_run_code_as_them");
        let mut child =
            std::process::Command::new(std::env::current_exe().expect("this test binary"));
        child.args(["--exact", &test, "--nocapture"]);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PUNKTFUNK_") {
                child.env_remove(key);
            }
        }
        let out = child
            .env(CHILD, config.path())
            .env("PUNKTFUNK_CONFIG_DIR", config.path())
            .env("PUNKTFUNK_SEAT_SESSION", "1")
            .env("PUNKTFUNK_SEAT_ID", "0123456789abcdef0123456789abcdef")
            .env("PUNKTFUNK_SEAT_OWN_ACCOUNT", "1")
            .output()
            .expect("run the child");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("1 passed"),
            "the child ran no test: {stdout}"
        );
        return;
    };
    assert!(pf_paths::seat::own_account());
    let hooks_file = std::path::Path::new(&config).join("hooks.json");
    let stored = std::fs::read(&hooks_file).unwrap();
    assert_eq!(
        crate::hooks::store().get().hooks.len(),
        2,
        "hooks are stored"
    );
    let launching = crate::events::HostEvent {
        seq: 1,
        ts_ms: 0,
        schema: crate::events::SCHEMA_VERSION,
        seat: None,
        kind: crate::events::EventKind::GameLaunching {
            game: crate::events::GameRefPayload {
                app: Some("steam:570".into()),
                title: "Dota 2".into(),
                store: Some("steam".into()),
                client: "Living Room TV".into(),
                fingerprint: None,
                plane: crate::events::Plane::Native,
                preset: None,
                profile: None,
            },
            hold: None,
        },
    };
    assert!(
        crate::hooks::holding(&launching).is_empty(),
        "a stored hold ran"
    );

    let app = test_app(test_state(), None);
    let hooks = serde_json::json!({"hooks": [{"on": "stream.*", "run": "true"}]});
    let (s, body) = send(&app, json_req(Method::PUT, "/api/v1/hooks", hooks)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        std::fs::read(&hooks_file).unwrap(),
        stored,
        "hooks.json changed"
    );

    let raw = serde_json::json!({"spec": "punktfunk-plugin-x", "accept_unverified": true});
    let (s, body) = send(&app, json_req(Method::POST, "/api/v1/store/install", raw)).await;
    if cfg!(windows) {
        // A Windows seat runs no plugin of its own, so it refuses every install first.
        assert_eq!(s, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["code"], "seat_runs_no_plugins", "{body}");
    } else {
        assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
    }

    let copy = serde_json::json!({"exe": "/bin/sh"});
    let adopt = "/api/v1/emulators/rpcs3/adopt";
    let (s, body) = send(&app, json_req(Method::POST, adopt, copy)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");

    // On Linux the console manages no plugin at all there, and the runner reads as absent.
    if cfg!(target_os = "linux") {
        let on = serde_json::json!({"enabled": true});
        let (s, body) = send(&app, json_req(Method::POST, "/api/v1/store/runtime", on)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
        let (s, body) = send(&app, get_req("/api/v1/store/runtime")).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(body["installed"], false, "{body}");
    }
}

/// Set on the child only: the box's library dir it reads.
const LANES_CHILD: &str = "PF_OWN_ACCOUNT_LANES_CHILD";
/// Paired with the seat host the lanes test runs.
const DEVICE: &str = "c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9";

/// Runs the test `name` of this module again as a child on a person's own account, with its
/// config and library dirs. `marker` names the child its library dir.
fn on_own_account(name: &str, marker: &str) {
    let config = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let module = module_path!().split_once("::").expect("crate::module").1;
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test binary"));
    child.args(["--exact", &format!("{module}::{name}"), "--nocapture"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PUNKTFUNK_") {
            child.env_remove(key);
        }
    }
    let out = child
        .env(marker, library.path())
        .env("PUNKTFUNK_CONFIG_DIR", config.path())
        .env("PUNKTFUNK_LIBRARY_DIR", library.path())
        .env("PUNKTFUNK_SEAT_SESSION", "1")
        .env("PUNKTFUNK_SEAT_ID", "0123456789abcdef0123456789abcdef")
        .env("PUNKTFUNK_SEAT_OWN_ACCOUNT", "1")
        .output()
        .expect("run the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(
        stdout.contains("1 passed"),
        "the child ran no test: {stdout}"
    );
}

/// What each lane that existed before the person's own got on a person's own account: hooks,
/// command titles, the store, plugin access and emulator writes are refused on every one, a
/// prep step runs nothing, and the box's command tile is not listed.
#[tokio::test]
async fn every_lane_keeps_its_refusals_on_a_person_s_own_account() {
    use serde_json::json;
    let Some(library) = std::env::var_os(LANES_CHILD) else {
        return on_own_account(
            "every_lane_keeps_its_refusals_on_a_person_s_own_account",
            LANES_CHILD,
        );
    };
    assert!(pf_paths::seat::own_account());
    let library = std::path::Path::new(&library);
    let entries = json!({"entries": [
        {"id": "aaaaaaaaaaaa", "title": "Backup", "launch": {"kind": "command", "value": "true"}},
        {"id": "bbbbbbbbbbbb", "title": "Dota 2", "launch": {"kind": "steam_appid", "value": "570"}},
    ]});
    std::fs::write(library.join("library.json"), entries.to_string()).unwrap();
    let np = crate::native_pairing::NativePairing::load_with(
        Some(crate::test_support::scratch().join("pf-own-lanes.json")),
        None,
        false,
    )
    .unwrap();
    np.add("Deck", DEVICE).unwrap();
    let app = test_app_native(test_state(), Arc::new(np));

    let command = json!({"title": "Shell", "launch": {"kind": "command", "value": "true"}});
    let hooks = json!({"hooks": [{"on": "stream.*", "run": "true"}]});
    let mut writes = vec![
        (Method::PUT, "/api/v1/hooks", hooks),
        (Method::POST, "/api/v1/library/custom", command.clone()),
        (
            Method::PUT,
            "/api/v1/library/custom/own:aaaaaaaaaaaa",
            command,
        ),
        (
            Method::DELETE,
            "/api/v1/library/custom/aaaaaaaaaaaa",
            json!({}),
        ),
        (
            Method::POST,
            "/api/v1/emulators/rpcs3/adopt",
            json!({"exe": "/bin/sh"}),
        ),
    ];
    // The store, plugin access and emulator writes are refused by the Linux gate.
    if cfg!(target_os = "linux") {
        writes.extend([
            (
                Method::POST,
                "/api/v1/store/install",
                json!({"spec": "punktfunk-plugin-x"}),
            ),
            (
                Method::POST,
                "/api/v1/store/runtime",
                json!({"enabled": true}),
            ),
            (
                Method::POST,
                "/api/v1/plugin-access/requests",
                json!({"path": "/srv"}),
            ),
            (
                Method::POST,
                "/api/v1/plugin-access/rom/decide",
                json!({"allow": true}),
            ),
            (Method::POST, "/api/v1/emulators/rpcs3/install", json!({})),
            (Method::POST, "/api/v1/emulators/rpcs3/prepare", json!({})),
        ]);
    }
    for (method, path, body) in writes {
        let req = || json_req(method.clone(), path, body.clone());
        let (admin, why) = send(&app, req()).await;
        assert_eq!(admin, StatusCode::FORBIDDEN, "admin {method} {path}: {why}");
        let code = why["code"].as_str().unwrap_or_default();
        assert!(
            code.ends_with("_own_account"),
            "admin {method} {path}: {why}"
        );
        for token in ["plugin-secret", "demo-secret"] {
            let (status, why) = send(&app, with_bearer(req(), token)).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{token} {method} {path}: {why}"
            );
        }
        let device = send_cert(&app, req(), DEVICE).await;
        assert_eq!(device, StatusCode::UNAUTHORIZED, "device {method} {path}");
    }
    assert!(
        crate::hooks::store().get().hooks.is_empty(),
        "a hook was stored"
    );

    let ran = library.join("prep-ran");
    let step = crate::hooks::PrepCmd {
        run: format!("touch {}", ran.display()),
        undo: None,
        elevated: false,
    };
    drop(crate::hooks::run_prep(&[step], &[]));
    assert!(!ran.exists(), "a prep step ran");

    let view = crate::library::View::here();
    assert!(
        !crate::library::lists(&view, "custom:aaaaaaaaaaaa"),
        "the box's command is listed"
    );
    assert!(crate::library::lists(&view, "custom:bbbbbbbbbbbb"));
}

/// On a person's own Linux account every write to plugins or emulators is refused, each group
/// with its sentence; every read, and a catalog refresh, passes.
#[test]
fn a_person_s_own_linux_account_refuses_every_plugin_and_emulator_write() {
    use crate::mgmt::own_account::refusal;
    let plugins = Some("Plugins don't run on a profile that uses an existing Linux account yet.");
    let emulators = Some(
        "The console doesn't manage emulators inside a profile that uses an existing Linux account.",
    );
    let writes = [
        (Method::POST, "/api/v1/store/install", plugins),
        (Method::POST, "/api/v1/store/uninstall", plugins),
        (Method::POST, "/api/v1/store/runtime", plugins),
        (Method::PUT, "/api/v1/store/sources/retro", plugins),
        (Method::DELETE, "/api/v1/store/sources/retro", plugins),
        (Method::POST, "/api/v1/plugin-access/rom/decide", plugins),
        (Method::POST, "/api/v1/plugin-access/rom/release", plugins),
        (Method::POST, "/api/v1/plugin-access/requests", plugins),
        (Method::POST, "/api/v1/emulators/rpcs3/install", emulators),
        (Method::POST, "/api/v1/emulators/rpcs3/adopt", emulators),
        (Method::POST, "/api/v1/emulators/rpcs3/prepare", emulators),
        (Method::POST, "/api/v1/emulators/rpcs3/content", emulators),
        (Method::POST, "/api/v1/emulators/rpcs3/remove", emulators),
        (
            Method::POST,
            "/api/v1/emulators/rpcs3/saves/import",
            emulators,
        ),
        (
            Method::POST,
            "/api/v1/emulators/rpcs3/saves/export",
            emulators,
        ),
    ];
    for (method, path, why) in writes {
        assert_eq!(refusal(&method, path).map(|r| r.1), why, "{method} {path}");
    }
    for (method, path) in [
        (Method::GET, "/api/v1/store/catalog"),
        (Method::GET, "/api/v1/store/installed"),
        (Method::GET, "/api/v1/store/runtime"),
        (Method::GET, "/api/v1/store/sources"),
        (Method::GET, "/api/v1/store/jobs/1"),
        (Method::POST, "/api/v1/store/refresh"),
        (Method::GET, "/api/v1/plugin-access"),
        (Method::GET, "/api/v1/emulators"),
        (Method::GET, "/api/v1/emulators/catalog"),
        (Method::GET, "/api/v1/emulators/rpcs3/saves"),
        (Method::HEAD, "/api/v1/emulators/rpcs3/firmware"),
        (Method::PUT, "/api/v1/hooks"),
        (Method::POST, "/api/v1/storefront"),
        (Method::PATCH, "/api/v1/host/settings"),
    ] {
        assert_eq!(refusal(&method, path), None, "{method} {path}");
    }
}

/// The rule before the scope table: every write under three route groups.
fn refused_by_group(method: &Method, path: &str) -> Option<&'static str> {
    if matches!(*method, Method::GET | Method::HEAD) || path == "/api/v1/store/refresh" {
        return None;
    }
    [
        ("/api/v1/store", "plugins_own_account"),
        ("/api/v1/plugin-access", "plugins_own_account"),
        ("/api/v1/emulators", "emulators_own_account"),
    ]
    .into_iter()
    .find(|(group, _)| {
        path.strip_prefix(group)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
    .map(|(_, code)| code)
}

/// The table refuses every write the three groups did, with the same sentence, and the other
/// writes that run code as the person, each listed here. No other answer changed.
#[test]
fn the_table_refuses_what_the_groups_did_and_the_rest_of_what_runs_code() {
    use crate::mgmt::own_account::refusal;
    const NEWLY_REFUSED: &[(&str, &str)] = &[
        ("DELETE", "/api/v1/library/provider/{provider}"),
        ("DELETE", "/api/v1/plugins/{id}"),
        ("POST", "/api/v1/game/holds/{token}/release"),
        ("POST", "/api/v1/plugins/logs"),
        ("PUT", "/api/v1/library/provider/{provider}"),
        ("PUT", "/api/v1/library/provider/{provider}/downloads"),
        ("PUT", "/api/v1/library/provider/{provider}/running"),
        ("PUT", "/api/v1/plugins/{id}"),
    ];
    let doc: serde_json::Value = serde_json::from_str(&openapi_json()).unwrap();
    let mut changed = Vec::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for method in ops.as_object().unwrap().keys() {
            let method = Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
            let now = refusal(&method, path).map(|(code, _)| code);
            let before = refused_by_group(&method, path);
            if now != before {
                assert_eq!(before, None, "{method} {path}: refused differently");
                assert_eq!(now, Some("plugins_own_account"), "{method} {path}");
                changed.push((method.to_string(), path.clone()));
            }
        }
    }
    changed.sort();
    let want: Vec<(String, String)> = NEWLY_REFUSED
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(changed, want);
}
