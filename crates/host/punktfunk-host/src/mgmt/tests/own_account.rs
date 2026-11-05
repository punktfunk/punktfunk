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
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");

    let copy = serde_json::json!({"exe": "/bin/sh"});
    let adopt = "/api/v1/emulators/rpcs3/adopt";
    let (s, body) = send(&app, json_req(Method::POST, adopt, copy)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{body}");
}
