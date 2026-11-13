//! A Linux seat's own plugins, lane by lane (admin, person, plugin, device), on a made seat and
//! on a person's own account. Each test runs again as a child on each kind of seat host.

use super::*;
use serde_json::json;

/// Set on the child only: its library dir.
const CHILD: &str = "PF_SEAT_PLUGINS_CHILD";
/// The person token of the seat host a child runs.
const TOKEN: &str = "5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e";
/// That seat host's own profile.
const MINE: &str = "a1b2c3d4e5f6";
/// A paired device's fingerprint; a seat host pairs none.
const DEVICE: &str = "d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4d4";
/// The line a made seat's child prints its `GET /library/own` answer on.
const ANSWER: &str = "own titles: ";

/// Runs the test `name` of this module as a child on a made seat, then on a person's own
/// account, and returns what the first printed.
fn on_both_seats(name: &str) -> String {
    let module = module_path!().split_once("::").expect("crate::module").1;
    let path = format!("{module}::{name}");
    let made = super::own_account::on_seat(&path, CHILD, false);
    super::own_account::on_seat(&path, CHILD, true);
    made
}

/// A person session on `app`, opened from a ticket its person token signed.
async fn person_session(app: &Router) -> String {
    let now = crate::clock::unix_secs_u64();
    let ticket = crate::person_token::sign(TOKEN, now, &"0".repeat(32), MINE);
    let open = json_req(
        Method::POST,
        "/api/v1/auth/person/session",
        json!({ "ticket": ticket }),
    );
    let resp = app.clone().oneshot(open).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let grant: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    grant["token"].as_str().unwrap().to_string()
}

/// A Linux seat's plugins publish into its own titles. On a made seat the operator, the seat's
/// person and the plugin under its own id write them, under `own:` ids with no store claim, and
/// the box learns their names and pictures alone. On a person's own account the person and their
/// plugin write them and the operator is told they are the person's; a provider row planted
/// there lists nothing.
#[tokio::test]
async fn a_seat_s_plugins_publish_on_its_own_shelf() {
    if std::env::var_os(CHILD).is_some() {
        return seat_half().await;
    }
    let made = on_both_seats("a_seat_s_plugins_publish_on_its_own_shelf");
    let answer = made
        .lines()
        .find_map(|l| l.strip_prefix(ANSWER))
        .expect("the made seat printed its answer");
    let own: serde_json::Value = serde_json::from_str(answer).unwrap();
    if !cfg!(target_os = "linux") {
        return assert_eq!(own, json!([]));
    }
    assert_eq!(own[0]["title"], "Quake", "{own}");
    assert!(own[0].get("launch").is_none(), "{own}");

    // The box keeps the names for the seat's profile, and starts none of them.
    use super::profile_view::{profiles, profiles_state, write_fixture, SEAT_P};
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let store = axum::http::Request::put(format!("/api/v1/library/own?as={SEAT_P}"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(Body::from(answer.to_string()))
        .unwrap();
    assert_eq!(send(&app, store).await.0, StatusCode::NO_CONTENT);
    let (_, json) = send(&app, get_req(&format!("/api/v1/library?as={SEAT_P}"))).await;
    let rows = json.get("items").unwrap_or(&json).as_array().unwrap();
    let quake = rows.iter().find(|g| g["title"] == "Quake").expect("listed");
    assert_eq!((&quake["id"], &quake["own"]), (&own[0]["id"], &json!(true)));
    assert!(quake.get("launch").is_none(), "{quake}");
    let view = crate::library::View::of(SEAT_P, false, crate::library::Reach::seat(false));
    let id = quake["id"].as_str().unwrap();
    assert!(crate::library::resolve_launch(&view, id).is_none());
}

/// The seat host's half of [`a_seat_s_plugins_publish_on_its_own_shelf`].
async fn seat_half() {
    let library = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
    let own = pf_paths::seat::own_account();
    let publishes = cfg!(target_os = "linux");
    let app = test_app_seat(test_state(), TOKEN, MINE);
    let person = person_session(&app).await;
    let rows = json!([{"external_id": "q1", "title": "Quake",
        "launch": {"kind": "steam_appid", "value": "2310"},
        "art": {"hero": "https://cdn.example/quake.jpg"}}]);
    let put = |provider: &str| {
        let path = format!("/api/v1/library/provider/{provider}?store=steam");
        json_req(Method::PUT, &path, rows.clone())
    };
    let refused = (StatusCode::CONFLICT, json!("seat_library_read_only"));
    let theirs = (StatusCode::FORBIDDEN, json!("plugins_own_account_person"));
    let lanes = [
        ("admin", send(&app, put("demo")).await),
        (
            "person",
            send(&app, with_bearer(put("demo"), &person)).await,
        ),
        (
            "plugin",
            send(&app, with_bearer(put("demo"), "demo-secret")).await,
        ),
    ];
    for (lane, (status, why)) in lanes {
        let code = &why["code"];
        match (publishes, own && lane == "admin") {
            (true, true) => assert_eq!((status, code), (theirs.0, &theirs.1), "{lane}"),
            (true, false) => assert_eq!(status, StatusCode::OK, "{lane}: {why}"),
            (false, _) => assert_eq!((status, code), (refused.0, &refused.1), "{lane}"),
        }
    }
    let (status, _) = send(&app, with_bearer(put("other"), "demo-secret")).await;
    let other = if publishes {
        StatusCode::FORBIDDEN
    } else {
        refused.0
    };
    assert_eq!(status, other, "another plugin's id");
    let device = send_cert(&app, put("demo"), DEVICE).await;
    assert_eq!(device, StatusCode::UNAUTHORIZED);
    let box_file = std::fs::read_to_string(library.join("library.json"));
    assert!(
        !box_file.is_ok_and(|f| f.contains("Quake")),
        "the box's file was written"
    );

    let view = crate::library::View::here();
    if publishes {
        let games = crate::library::all_games_for(&view);
        let quake = games.iter().find(|g| g.title == "Quake").expect("listed");
        assert!(crate::library::is_own_id(&quake.id), "{}", quake.id);
        let running = json!({"running": [{"external_id": "q1"}]});
        let report = json_req(
            Method::PUT,
            "/api/v1/library/provider/demo/running",
            running,
        );
        let (_, accepted) = send(&app, with_bearer(report, "demo-secret")).await;
        assert_eq!(accepted["matched"], 1, "{accepted}");
    }
    let (_, answer) = send(&app, get_req("/api/v1/library/own")).await;
    println!("{ANSWER}{answer}");
    if publishes && !own {
        return;
    }

    // A provider row planted on the seat's own shelf lists on a made seat alone.
    let planted = json!({"entries": [{"id": "own:aaaaaaaaaaaa", "title": "Quake",
        "provider": "demo", "external_id": "q1",
        "launch": {"kind": "steam_appid", "value": "2310"}}]});
    let shelf = pf_paths::config_dir().join("library.json");
    std::fs::write(shelf, planted.to_string()).unwrap();
    let listed = crate::library::all_games_for(&view)
        .iter()
        .any(|g| g.title == "Quake");
    assert_eq!(listed, !own, "a planted row's listing");
}

/// A seat's own plugin UI is its seat host's to hand out: its plugin registers it, and the
/// seat's person reads its credential there, a device never. The operator reads it on a made
/// seat; on a person's own account the page and its sign-ins are the person's alone.
#[tokio::test]
async fn a_seat_s_plugin_ui_stays_on_its_seat_host() {
    if std::env::var_os(CHILD).is_none() {
        on_both_seats("a_seat_s_plugin_ui_stays_on_its_seat_host");
        return;
    }
    let gated = pf_paths::seat::own_account() && cfg!(target_os = "linux");
    let app = test_app_seat(test_state(), TOKEN, MINE);
    let person = person_session(&app).await;
    let ui = json!({"title": "Demo", "ui": {"port": 49321, "secret": "s3cr3t-s3cr3t-s3cr3t"}});
    let register = json_req(Method::PUT, "/api/v1/plugins/demo", ui);
    let (status, why) = send(&app, with_bearer(register, "demo-secret")).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{why}");
    let credential = || get_req("/api/v1/plugins/demo/ui-credential");
    let (status, why) = send(&app, credential()).await;
    if gated {
        assert_eq!(status, StatusCode::FORBIDDEN, "admin");
        assert_eq!(why["code"], "plugins_own_account_person");
    } else {
        assert_eq!(status, StatusCode::OK, "admin");
    }
    let (status, body) = send(&app, with_bearer(credential(), &person)).await;
    assert_eq!(status, StatusCode::OK, "person");
    assert_eq!(body["port"], 49321);
    let device = send_cert(&app, credential(), DEVICE).await;
    assert_eq!(device, StatusCode::UNAUTHORIZED);
}

/// A package declaring plugin `id`, installed under `plugins` as its runner finds it.
fn install(plugins: &std::path::Path, id: &str, reads: &str) {
    let name = format!("plugin-{id}");
    let dir = plugins.join("node_modules").join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    let body = json!({"name": name, "punktfunk": {"schema": 1, "id": id, "reads": [reads]}});
    std::fs::write(dir.join("package.json"), body.to_string()).unwrap();
    let deps_file = plugins.join("package.json");
    let mut deps: serde_json::Value = std::fs::read_to_string(&deps_file)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({"dependencies": {}}));
    deps["dependencies"][&name] = json!("1");
    std::fs::write(deps_file, deps.to_string()).unwrap();
}

/// On a Linux seat a plugin of its own runner reads its manifest and grants from the seat's own
/// folder, never the box's for a plugin of the same id; a plugin the seat doesn't have reads
/// the box's copy as before. A grant names only a folder the seat's account can list. The
/// operator and the person grant on a made seat, the person alone on a person's own account.
#[tokio::test]
async fn a_seat_s_own_plugin_reads_its_own_grants() {
    use crate::plugins::access::AccessStore;
    use crate::plugins::manifest::{for_provider, granted_roots};
    if std::env::var_os(CHILD).is_none() {
        on_both_seats("a_seat_s_own_plugin_reads_its_own_grants");
        return;
    }
    let library = std::path::PathBuf::from(std::env::var_os(CHILD).unwrap());
    let config = pf_paths::config_dir();
    let linux = cfg!(target_os = "linux");
    install(&config.join("plugins"), "demo", "/seat/reads");
    // On a person's own account the seat's plugin is one their own lane installed.
    if pf_paths::seat::own_account() {
        crate::plugins::manifest::mark_person(&config, "plugin-demo", true).unwrap();
    }
    install(&library.join("plugins"), "demo", "/box/reads");
    install(&library.join("plugins"), "boxonly", "/box/only");
    let folders = tempfile::tempdir().unwrap();
    let folder = |name: &str| {
        let dir = folders.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    };
    let (seat_dir, box_dir, only_dir) = (folder("seat"), folder("box"), folder("only"));
    let box_store = AccessStore::open(library.clone());
    box_store.grant("demo", &box_dir, false, "console").unwrap();
    box_store
        .grant("boxonly", &only_dir, false, "console")
        .unwrap();
    AccessStore::open(config.clone())
        .grant("demo", &seat_dir, false, "console")
        .unwrap();
    let (reads, roots) = match linux {
        true => ("/seat/reads", vec![seat_dir.clone()]),
        false => ("/box/reads", vec![box_dir.clone()]),
    };
    assert_eq!(for_provider("demo").unwrap().reads, [reads]);
    assert_eq!(granted_roots("demo"), roots);
    assert_eq!(for_provider("boxonly").unwrap().reads, ["/box/only"]);
    assert_eq!(granted_roots("boxonly"), [only_dir]);

    // A folder the seat's account can't list is no grant. Root lists everything.
    #[cfg(unix)]
    if crate::geteuid() != 0 {
        use std::os::unix::fs::PermissionsExt;
        let shut = folder("shut");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();
        let refused = AccessStore::open(config.clone()).grant("demo", &shut, false, "console");
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(refused.unwrap_err().to_string().contains("not_readable"));
    }

    // Who decides a seat's grants.
    let gated = pf_paths::seat::own_account() && linux;
    let app = test_app_seat(test_state(), TOKEN, MINE);
    let person = person_session(&app).await;
    let allow = || {
        let body = json!({"path": seat_dir, "decision": "allow", "write": false});
        json_req(Method::POST, "/api/v1/plugin-access/demo/decide", body)
    };
    let (status, why) = send(&app, allow()).await;
    if gated {
        assert_eq!(
            (status, &why["code"]),
            (StatusCode::FORBIDDEN, &json!("plugins_own_account_person"))
        );
    } else {
        assert_eq!(status, StatusCode::OK, "admin: {why}");
    }
    let (status, why) = send(&app, with_bearer(allow(), &person)).await;
    assert_eq!(status, StatusCode::OK, "person: {why}");
    let (status, _) = send(&app, with_bearer(allow(), "demo-secret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "plugin");
    assert_eq!(
        send_cert(&app, allow(), DEVICE).await,
        StatusCode::UNAUTHORIZED
    );
}

/// On a person's own account the person adopts a copy of their own and prepares their
/// emulators; the operator keeps its refusal, and an install stays the box's. On a made seat the
/// operator and the person both may. The child's emulators live in its own temp dirs.
#[tokio::test]
async fn a_person_adopts_and_prepares_their_own_emulators() {
    if std::env::var_os(CHILD).is_none() {
        on_both_seats("a_person_adopts_and_prepares_their_own_emulators");
        return;
    }
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let ours = match cfg!(windows) {
        true => std::path::PathBuf::from(std::env::var_os(CHILD).unwrap()),
        false => home.clone(),
    };
    assert!(
        crate::emulators::prefix().starts_with(&ours),
        "the child's emulators"
    );
    let gated = pf_paths::seat::own_account() && cfg!(target_os = "linux");
    let app = test_app_seat(test_state(), TOKEN, MINE);
    let person = person_session(&app).await;
    let exe = home.join("apps/pcsx2-qt");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, "").unwrap();
    let post = |route: &str, body: serde_json::Value| {
        json_req(
            Method::POST,
            &format!("/api/v1/emulators/pcsx2/{route}"),
            body,
        )
    };
    let adopt = || post("adopt", json!({"exe": exe}));
    let prepare = || post("prepare", json!({}));
    // The Linux gate refuses both; elsewhere the adopt route refuses on its own.
    let own = pf_paths::seat::own_account();
    for (what, req, refused) in [("adopt", adopt(), own), ("prepare", prepare(), gated)] {
        let (status, why) = send(&app, req).await;
        if refused {
            assert_eq!(status, StatusCode::FORBIDDEN, "operator {what}");
            let code = why["code"].as_str().unwrap_or_default();
            assert!(code.ends_with("_own_account"), "operator {what}: {why}");
        } else {
            assert_ne!(status, StatusCode::FORBIDDEN, "operator {what}: {why}");
        }
    }
    let (status, copy) = send(&app, with_bearer(adopt(), &person)).await;
    assert_eq!(status, StatusCode::OK, "person adopt: {copy}");
    assert_eq!(copy["exe"], json!(exe));
    let (status, why) = send(&app, with_bearer(prepare(), &person)).await;
    assert_ne!(status, StatusCode::FORBIDDEN, "person prepare: {why}");
    if gated {
        let install = post("install", json!({}));
        let (status, why) = send(&app, with_bearer(install, &person)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(why["code"], "emulators_own_account_yet");
    }
    let (status, _) = send(&app, with_bearer(adopt(), "demo-secret")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "plugin adopt");
    assert_eq!(
        send_cert(&app, adopt(), DEVICE).await,
        StatusCode::UNAUTHORIZED
    );
}

/// On a person's own Linux account the store and the runner's switch answer their own lane: the
/// operator is told they are the person's and reads no runner, a plugin and a device reach
/// neither, and a catalog source stays closed to the person too. Only a package their lane
/// installed is the runner's: a planted one gets no manifest and no credential.
#[tokio::test]
async fn a_person_s_own_store_answers_their_lane_alone() {
    if std::env::var_os(CHILD).is_none() {
        let module = module_path!().split_once("::").expect("crate::module").1;
        let path = format!("{module}::a_person_s_own_store_answers_their_lane_alone");
        super::own_account::on_seat(&path, CHILD, true);
        return;
    }
    let linux = cfg!(target_os = "linux");
    let app = test_app_seat(test_state(), TOKEN, MINE);
    let person = person_session(&app).await;
    // Bodies every handler refuses, so what differs by lane is the gate's answer.
    let calls = [
        (Method::POST, "/api/v1/store/install", json!({})),
        (
            Method::POST,
            "/api/v1/store/uninstall",
            json!({"pkg": "@punktfunk/plugin-none"}),
        ),
        (Method::POST, "/api/v1/store/runtime", json!({})),
    ];
    let theirs = (StatusCode::FORBIDDEN, json!("plugins_own_account_person"));
    for (method, path, body) in calls {
        let req = || json_req(method.clone(), path, body.clone());
        let (status, why) = send(&app, req()).await;
        if linux {
            assert_eq!(
                (status, &why["code"]),
                (theirs.0, &theirs.1),
                "admin {path}"
            );
        }
        let (status, why) = send(&app, with_bearer(req(), &person)).await;
        assert!(
            status.is_client_error() && status != StatusCode::FORBIDDEN,
            "person {path}: {status} {why}"
        );
        for token in ["plugin-secret", "demo-secret"] {
            let status = send(&app, with_bearer(req(), token)).await.0;
            assert_eq!(status, StatusCode::FORBIDDEN, "{token} {path}");
        }
        let device = send_cert(&app, req(), DEVICE).await;
        assert_eq!(device, StatusCode::UNAUTHORIZED, "device {path}");
    }
    if linux {
        let url = json!({"url": "https://retro.example/index.json"});
        let source = json_req(Method::PUT, "/api/v1/store/sources/retro", url);
        let (status, why) = send(&app, with_bearer(source, &person)).await;
        let unreviewed = json!("plugin_own_account_unreviewed");
        assert_eq!((status, &why["code"]), (StatusCode::FORBIDDEN, &unreviewed));
        let (status, runner) = send(&app, get_req("/api/v1/store/runtime")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(runner["installed"], false, "the operator read their runner");
    }

    let config = pf_paths::config_dir();
    install(&config.join("plugins"), "planted", "/srv/planted");
    install(&config.join("plugins"), "theirs", "/srv/theirs");
    crate::plugins::manifest::mark_person(&config, "plugin-theirs", true).unwrap();
    let runs: Vec<String> = crate::plugins::manifest::runner_plugins()
        .into_keys()
        .collect();
    assert_eq!(runs, ["theirs"]);
    let tokens = crate::mgmt_token::load_or_generate_per_plugin().unwrap();
    assert_eq!(tokens.into_keys().collect::<Vec<_>>(), ["theirs"]);
}
