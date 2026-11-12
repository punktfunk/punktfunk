//! A profile's own layer of the library (`library-profiles/<id>/`), and the box with none.

use super::*;

/// The owner, the box's own session.
const OWNER: &str = "0f0f0f0f0f0f";
/// A profile with a seat of its own.
const SEAT_P: &str = "a1b2c3d4e5f6";
/// A profile that shares the owner's desktop.
const SHARER: &str = "0a0b0c0d0e0f";
/// [`SEAT_P`]'s row in the seats ledger.
const SEAT: &str = "0123456789abcdef0123456789abcdef";

/// The owner, [`SEAT_P`] and [`SHARER`].
fn profiles() -> serde_json::Value {
    let p = |id: &str, account: serde_json::Value| serde_json::json!({"id": id, "display_name": id, "os_account": account});
    serde_json::json!({"profiles": [
        p(OWNER, serde_json::json!({"kind": "operator"})),
        p(SEAT_P, serde_json::json!({"kind": "seat", "seat": SEAT})),
        p(SHARER, serde_json::json!({"kind": "operator"})),
    ]})
}

/// A state whose profile store is `profiles`, written to `dir/profiles.json`.
pub(super) fn profiles_state(dir: &std::path::Path, profiles: serde_json::Value) -> Arc<AppState> {
    let path = dir.join("profiles.json");
    std::fs::write(&path, profiles.to_string()).unwrap();
    let state = test_state();
    let store = crate::profiles::Profiles::load_with(Some(path), None);
    let _ = state.profiles.set(Arc::new(store));
    state
}

/// The box's library in `dir`, and a layer for [`SEAT_P`] that changes every row it touches.
fn write_fixture(dir: &std::path::Path) {
    let write = |rel: &str, v: serde_json::Value| {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, v.to_string()).unwrap();
    };
    let steam = |id: &str, title: &str, appid: &str| {
        serde_json::json!({"id": id, "title": title, "provider": "steam", "store": "steam",
            "external_id": appid, "launch": {"kind": "steam_appid", "value": appid}})
    };
    write(
        "library.json",
        serde_json::json!({
            "entries": [
                steam("s1", "Portal", "400"),
                steam("s2", "Dota", "570"),
                steam("s3", "Half-Life", "70"),
                {"id": "l1", "title": "Syndicate", "provider": "lutris", "store": "lutris",
                 "external_id": "4", "launch": {"kind": "lutris_id", "value": "4"}},
                {"id": "c1", "title": "Backup", "launch": {"kind": "command", "value": "true"}},
                {"id": "p1", "title": "Owner's Playnite", "provider": "playnite", "external_id": "x1"},
                {"id": "r1", "title": "Zelda", "provider": "romm", "external_id": "z1",
                 "launch": {"kind": "emulator", "value": "dolphin"}},
            ],
            "claims": {"steam": "steam", "lutris": "lutris"},
        }),
    );
    write(
        "library-hidden.json",
        serde_json::json!({"hidden": ["steam:570"]}),
    );
    write(
        "library-scanners.json",
        serde_json::json!({"disabled": ["lutris"]}),
    );
    write(
        "library-overrides.json",
        serde_json::json!({"version": 1, "sets": {},
            "games": {"steam:400": {"hz": 60}}, "sources": {"steam": {"lines": 1080}}}),
    );
    write(
        "library-picks.json",
        serde_json::json!({"picks": {"steam:400": {"logo": "https://box.example/l.png"}}}),
    );
    let layer = |file: &str| format!("library-profiles/{SEAT_P}/{file}");
    write(
        &layer("library-hidden.json"),
        serde_json::json!({"hidden": ["steam:70"]}),
    );
    write(
        &layer("library-scanners.json"),
        serde_json::json!({"disabled": ["romm"]}),
    );
    write(
        &layer("library-overrides.json"),
        serde_json::json!({"games": {"steam:400": {"hz": 30}}}),
    );
    write(
        &layer("library-picks.json"),
        serde_json::json!({"picks": {"steam:400": {"portrait": "https://p.example/p.png"}}}),
    );
    write(
        &layer("session-settings.json"),
        serde_json::json!({"session_on_game_exit": false}),
    );
}

/// `(title, hidden)` of each row, in order.
fn listed(json: &serde_json::Value) -> Vec<(String, bool)> {
    json.get("items")
        .unwrap_or(json)
        .as_array()
        .expect("rows")
        .iter()
        .map(|g| {
            let title = g["title"].as_str().unwrap_or_default().to_string();
            (title, g["hidden"] == true)
        })
        .collect()
}

fn row<'a>(json: &'a serde_json::Value, title: &str) -> &'a serde_json::Value {
    json.get("items")
        .unwrap_or(json)
        .as_array()
        .expect("rows")
        .iter()
        .find(|g| g["title"] == title)
        .unwrap_or_else(|| panic!("no {title} in {json}"))
}

/// With no `as`, a layer on disk changes nothing: every read is the box's files alone.
#[tokio::test]
async fn with_no_profile_in_view_the_box_reads_as_today() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(test_state(), None);

    let box_rows = vec![
        ("Backup".to_string(), false),
        ("Dota".to_string(), true),
        ("Half-Life".to_string(), false),
        ("Owner's Playnite".to_string(), false),
        ("Portal".to_string(), false),
        ("Zelda".to_string(), false),
    ];
    for path in ["/api/v1/library", "/api/v1/library/page"] {
        let (s, json) = send(&app, get_req(path)).await;
        assert_eq!(s, StatusCode::OK, "{path}");
        assert_eq!(listed(&json), box_rows, "{path}");
        let portal = row(&json, "Portal");
        assert_eq!(portal["overrides"], serde_json::json!({"hz": 60}), "{path}");
        assert_eq!(
            portal["filled"],
            serde_json::json!({"logo": "pick"}),
            "{path}"
        );
        assert!(portal["art"].get("portrait").is_none(), "{path}: {portal}");
        assert!(portal.get("profile").is_none(), "{path}: {portal}");
        let half_life = row(&json, "Half-Life");
        assert_eq!(half_life["overrides"], serde_json::json!({"lines": 1080}));
    }

    let (_, scanners) = send(&app, get_req("/api/v1/library/scanners")).await;
    let states: Vec<(String, bool)> = scanners
        .as_array()
        .expect("sources")
        .iter()
        .map(|s| (s["id"].as_str().unwrap().to_string(), s["enabled"] == true))
        .collect();
    assert_eq!(
        states,
        [
            ("lutris".to_string(), false),
            ("playnite".to_string(), true),
            ("romm".to_string(), true),
            ("steam".to_string(), true),
        ]
    );
    assert!(
        scanners
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s.get("off_by").is_none()),
        "{scanners}"
    );

    let (_, doc) = send(&app, get_req("/api/v1/library/overrides")).await;
    assert_eq!(doc["games"], serde_json::json!({"steam:400": {"hz": 60}}));
    assert_eq!(
        doc["sources"],
        serde_json::json!({"steam": {"lines": 1080}})
    );

    let hz = crate::library::overrides_for(&crate::library::View::here(), "steam:400")
        .and_then(|o| o.hz);
    assert_eq!(hz, Some(60));
    assert!(
        crate::library::resolve_launch(&crate::library::View::here(), "steam:570").is_none(),
        "hidden"
    );
    assert!(crate::library::resolve_launch(&crate::library::View::here(), "steam:70").is_some());
}

/// `GET` on the paired-cert lane, with its body.
async fn get_cert(app: &Router, path: &str, fp: &str) -> (StatusCode, serde_json::Value) {
    let mut req = get_req(path);
    req.extensions_mut()
        .insert(PeerCertFingerprint(Some(fp.to_string())));
    let resp = app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

/// The rows of `json` but the command tile, which [`backup_listed`] answers for.
fn titles_but_backup(json: &serde_json::Value) -> Vec<(String, bool)> {
    let mut rows = listed(json);
    rows.retain(|(t, _)| t != "Backup");
    rows
}

fn backup_listed(json: &serde_json::Value) -> bool {
    listed(json).iter().any(|(t, _)| t == "Backup")
}

/// Each row of the combination table for a profile with a seat of its own: hidden and sources
/// are the box's or the profile's, an override or pick of its own wins over the box's.
#[tokio::test]
async fn a_profile_s_layer_adds_and_narrows_never_widens() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    // A Linux seat plays only what it starts itself; on Windows the box, with no seat service
    // to ask, counts the seat as a person's own.
    let seat_lists_backup = !cfg!(target_os = "linux") && !cfg!(windows);

    for path in ["/api/v1/library", "/api/v1/library/page"] {
        let path = format!("{path}?as={SEAT_P}");
        let (s, json) = send(&app, get_req(&path)).await;
        assert_eq!(s, StatusCode::OK, "{path}: {json}");
        // Dota the box hides, Syndicate's source the box turned off, Zelda's the profile did,
        // and a seat leaves the owner's Playnite out. Half-Life is the profile's own hide.
        let want = [
            ("Half-Life".to_string(), true),
            ("Portal".to_string(), false),
        ];
        assert_eq!(titles_but_backup(&json), want, "{path}");
        assert_eq!(backup_listed(&json), seat_lists_backup, "{path}");
        let portal = row(&json, "Portal");
        assert_eq!(portal["overrides"], serde_json::json!({"hz": 30}), "{path}");
        let own = serde_json::json!({"overrides": true, "picks": ["portrait"]});
        assert_eq!(portal["profile"], own, "{path}");
        let filled = serde_json::json!({"portrait": "pick", "logo": "pick"});
        assert_eq!(portal["filled"], filled, "{path}");
        let art = portal["art"]["portrait"].as_str().unwrap_or_default();
        assert!(art.ends_with(&format!("&as={SEAT_P}")), "{art}");
        let half_life = row(&json, "Half-Life");
        assert_eq!(half_life["overrides"], serde_json::json!({"lines": 1080}));
        assert!(half_life.get("profile").is_none(), "{half_life}");
    }

    let path = format!("/api/v1/library/scanners?as={SEAT_P}");
    let (_, scanners) = send(&app, get_req(&path)).await;
    let states: Vec<_> = scanners
        .as_array()
        .expect("sources")
        .iter()
        .map(|s| {
            (
                s["id"].clone(),
                s["enabled"].clone(),
                s.get("off_by").cloned(),
            )
        })
        .collect();
    let want = |id: &str, on: bool, by: Option<&str>| (id.into(), on.into(), by.map(Into::into));
    // A Linux seat lists no source it can play nothing of: Lutris starts from the owner's home.
    let mut wanted = vec![
        want("romm", false, Some("profile")),
        want("steam", true, None),
    ];
    if !cfg!(target_os = "linux") {
        wanted.insert(0, want("lutris", false, Some("box")));
    }
    assert_eq!(states, wanted);

    let path = format!("/api/v1/library/overrides?as={SEAT_P}");
    let (_, doc) = send(&app, get_req(&path)).await;
    assert_eq!(doc["games"], serde_json::json!({"steam:400": {"hz": 30}}));
    assert_eq!(doc["sources"], serde_json::json!({}));

    let path = format!("/api/v1/session/settings?as={SEAT_P}");
    let (s, state) = send(&app, get_req(&path)).await;
    assert_eq!(s, StatusCode::OK, "{state}");
    assert_eq!(state["configured"], true);
    assert_eq!(state["settings"]["session_on_game_exit"], false);
    let path = format!("/api/v1/session/settings?as={SHARER}");
    let (s, refused) = send(&app, get_req(&path)).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(refused["code"], "profile_session_settings_host");

    // A paired device naming the profile sees what it plays: no hidden title, flagged or not.
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-profile-view-cert.json")),
            None,
            false,
        )
        .unwrap(),
    );
    let fp = "deadbeefcafe";
    np.add("couch", fp).unwrap();
    let device = test_app_native(profiles_state(tmp.path(), profiles()), np);
    let (s, json) = get_cert(&device, &format!("/api/v1/library?as={SEAT_P}"), fp).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(titles_but_backup(&json), [("Portal".to_string(), false)]);
    assert!(json[0].get("profile").is_none(), "{json}");
}

/// On every read that takes `as` and the cert lane reaches, a device that may not open a locked
/// profile hears what an unknown id gets, and a device on its list reads it. The operator reads
/// every profile, as before.
#[tokio::test]
async fn a_device_reads_only_a_profile_it_may_open() {
    use axum::http::Method;
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let np = Arc::new(
        crate::native_pairing::NativePairing::load_with(
            Some(crate::test_support::scratch().join("pf-profile-view-lock.json")),
            None,
            false,
        )
        .unwrap(),
    );
    let (listed, other) = ("a7".repeat(32), "b8".repeat(32));
    np.add("listed", &listed).unwrap();
    np.add("other", &other).unwrap();
    let state = profiles_state(tmp.path(), profiles());
    let store = state.profiles.get().unwrap().clone();
    let device = pf_seats::ipc::Device {
        fingerprint: listed.clone(),
        name: "listed".into(),
    };
    let locked = store.locks().edit(SEAT_P, |lock, now| {
        lock.locked = true;
        lock.admit(&device, now, true)
    });
    assert!(locked.unwrap());
    let app = test_app_native(state, np);
    let doc: serde_json::Value = serde_json::from_str(&crate::mgmt::openapi_json()).unwrap();
    let mut reads = Vec::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        let Some(op) = ops.get("get") else {
            continue;
        };
        let takes_as = op["parameters"]
            .as_array()
            .is_some_and(|ps| ps.iter().any(|p| p["name"] == "as" && p["in"] == "query"));
        if takes_as && auth::cert_may_access(&Method::GET, path) {
            reads.push(path.clone());
        }
    }
    assert!(reads.contains(&"/api/v1/library".to_string()), "{reads:?}");
    for path in &reads {
        // A route's own arguments name a title and an art kind the fixture has.
        let concrete = path.replace("{kind}", "portrait").replace("{id}", "s1");
        let at = |id: &str| format!("{concrete}?as={id}");
        let (status, body) = get_cert(&app, &at(SEAT_P), &other).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
        assert_eq!(body["code"], "profile_unknown", "{path}");
        let (_, unknown) = get_cert(&app, &at("0123456789ab"), &other).await;
        assert_eq!(body, unknown, "{path}: no hint that it exists");
        let (status, body) = get_cert(&app, &at(SEAT_P), &listed).await;
        assert_ne!(body["code"], "profile_unknown", "{path}: {status}");
        let (status, body) = send(&app, get_req(&at(SEAT_P))).await;
        assert_ne!(body["code"], "profile_unknown", "{path}: {status}");
    }
}

/// A profile that shares the owner's desktop lists what the owner lists, less what it hides;
/// the owner's view is the box's.
#[tokio::test]
async fn the_owner_s_view_is_the_box_s_and_a_sharer_narrows_it() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let (_, plain) = send(&app, get_req("/api/v1/library")).await;
    let (_, owner) = send(&app, get_req(&format!("/api/v1/library?as={OWNER}"))).await;
    assert_eq!(owner, plain);
    let (_, sharer) = send(&app, get_req(&format!("/api/v1/library?as={SHARER}"))).await;
    let mut want = listed(&plain);
    want.retain(|(t, _)| t != "Dota");
    assert_eq!(
        listed(&sharer),
        want,
        "a title the box hides is out of a profile's view"
    );
}

/// With `as`, every write lands in the profile's own files and leaves the box's as they were;
/// a profile cannot bring back what the box hides or turns off.
#[tokio::test]
async fn writes_with_as_land_in_the_profile_s_own_files() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let box_files = [
        "library-hidden.json",
        "library-scanners.json",
        "library-picks.json",
    ];
    let read_box = || -> Vec<Vec<u8>> {
        box_files
            .iter()
            .map(|f| std::fs::read(tmp.path().join(f)).unwrap())
            .collect()
    };
    let before = read_box();
    let put = |path: &str, body: serde_json::Value| {
        json_req(Method::PUT, &format!("{path}?as={SHARER}"), body)
    };
    for (path, body) in [
        (
            "/api/v1/library/hidden/steam:400",
            serde_json::json!({"hidden": true}),
        ),
        (
            "/api/v1/library/hidden/steam:570",
            serde_json::json!({"hidden": false}),
        ),
        (
            "/api/v1/library/scanners/lutris",
            serde_json::json!({"enabled": true}),
        ),
        (
            "/api/v1/library/scanners/steam",
            serde_json::json!({"enabled": false}),
        ),
        (
            "/api/v1/library/picks/steam:70",
            serde_json::json!({"kind": "logo", "url": "https://sharer.example/l.png"}),
        ),
    ] {
        let (s, j) = send(&app, put(path, body)).await;
        assert_eq!(s, StatusCode::OK, "{path}: {j}");
    }
    let (_, scanners) = send(
        &app,
        get_req(&format!("/api/v1/library/scanners?as={SHARER}")),
    )
    .await;
    let lutris = &scanners.as_array().unwrap()[0];
    assert_eq!(lutris["id"], "lutris");
    assert_eq!(
        (&lutris["enabled"], &lutris["off_by"]),
        (&false.into(), &"box".into())
    );

    // The profile's entry may name the box's shared override.
    let set = serde_json::json!({"name": "4K", "lines": 2160});
    let (s, set) = send(
        &app,
        json_req(Method::POST, "/api/v1/library/overrides/sets", set),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{set}");
    let named = serde_json::json!({"set": set["id"]});
    let (s, j) = send(&app, put("/api/v1/library/overrides/sources/steam", named)).await;
    assert_eq!(s, StatusCode::OK, "{j}");
    let mine = serde_json::json!({"name": "Mine", "hz": 30});
    let path = format!("/api/v1/library/overrides/sets?as={SHARER}");
    let (s, j) = send(&app, json_req(Method::POST, &path, mine)).await;
    assert_eq!(s, StatusCode::CREATED, "{j}");

    assert_eq!(read_box(), before, "the box's files are as they were");
    let (_, box_doc) = send(&app, get_req("/api/v1/library/overrides")).await;
    let lines = serde_json::json!({"steam": {"lines": 1080}});
    assert_eq!(box_doc["sources"], lines);
    assert_eq!(box_doc["sets"].as_object().map(|s| s.len()), Some(1));

    let layer = tmp.path().join(format!("library-profiles/{SHARER}"));
    let read = |f: &str| -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(layer.join(f)).unwrap()).unwrap()
    };
    let hidden = &read("library-hidden.json")["hidden"];
    assert_eq!(hidden, &serde_json::json!(["steam:400"]));
    let disabled = &read("library-scanners.json")["disabled"];
    assert_eq!(disabled, &serde_json::json!(["steam"]));
    let picked = &read("library-picks.json")["picks"]["steam:70"]["logo"];
    assert_eq!(picked, "https://sharer.example/l.png");
    let sets = read("library-overrides.json")["sets"]
        .as_object()
        .map(|s| s.len());
    assert_eq!(sets, Some(1));

    // Its own switch turned Steam off; the box's hide of Dota stands.
    let (_, json) = send(&app, get_req(&format!("/api/v1/library?as={SHARER}"))).await;
    let titles: Vec<String> = listed(&json).into_iter().map(|(t, _)| t).collect();
    assert_eq!(titles, ["Backup", "Owner's Playnite", "Zelda"]);

    let settings = serde_json::json!({"session_on_game_exit": false});
    let (s, j) = send(&app, put("/api/v1/session/settings", settings)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{j}");
    let path = format!("/api/v1/session/settings?as={SEAT_P}");
    let on = serde_json::json!({"session_on_game_exit": true});
    let (s, state) = send(&app, json_req(Method::PUT, &path, on)).await;
    assert_eq!(s, StatusCode::OK, "{state}");
    assert_eq!(state["settings"]["session_on_game_exit"], true);
    let file = tmp
        .path()
        .join(format!("library-profiles/{SEAT_P}/session-settings.json"));
    let stored: serde_json::Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    assert_eq!(stored["session_on_game_exit"], true);
}

/// Dropping a profile's own session settings answers what `GET` then answers, `configured`
/// false, and again when there is nothing left to drop. Only a profile with a seat of its own
/// has settings to drop.
#[tokio::test]
async fn a_profile_s_own_session_settings_drop() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let drop = |query: &str| {
        axum::http::Request::delete(format!("/api/v1/session/settings{query}"))
            .body(Body::empty())
            .unwrap()
    };
    let file = tmp
        .path()
        .join(format!("library-profiles/{SEAT_P}/session-settings.json"));
    assert!(file.exists());
    let get = format!("/api/v1/session/settings?as={SEAT_P}");
    for _ in 0..2 {
        let (s, dropped) = send(&app, drop(&format!("?as={SEAT_P}"))).await;
        assert_eq!(s, StatusCode::OK, "{dropped}");
        assert_eq!(dropped["configured"], false);
        assert_eq!(dropped, send(&app, get_req(&get)).await.1);
        assert!(!file.exists());
    }
    let need = (StatusCode::BAD_REQUEST, "session_settings_need_profile");
    for (query, (status, code)) in [
        (String::new(), need),
        ("?as=".into(), need),
        (format!("?as={OWNER}"), need),
        (
            format!("?as={SHARER}"),
            (StatusCode::CONFLICT, "profile_session_settings_host"),
        ),
        (
            "?as=0123456789ab".into(),
            (StatusCode::NOT_FOUND, "profile_unknown"),
        ),
    ] {
        let (s, j) = send(&app, drop(&query)).await;
        assert_eq!((s, j["code"].as_str()), (status, Some(code)), "{query}");
    }
}

/// A profile's entry naming a shared override the box then deletes sets nothing: the box's
/// entry applies, and the row does not call it the profile's. Its `none` still wins.
#[tokio::test]
async fn a_profile_s_entry_on_a_deleted_shared_override_leaves_the_box_s() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let set = serde_json::json!({"name": "4K", "lines": 2160});
    let (s, set) = send(
        &app,
        json_req(Method::POST, "/api/v1/library/overrides/sets", set),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{set}");
    let set = set["id"].as_str().unwrap().to_string();
    for (id, entry) in [
        ("steam:400", serde_json::json!({"set": set})),
        ("steam:70", serde_json::json!({"none": true})),
    ] {
        let path = format!("/api/v1/library/overrides/games/{id}?as={SHARER}");
        let (s, j) = send(&app, json_req(Method::PUT, &path, entry)).await;
        assert_eq!(s, StatusCode::OK, "{id}: {j}");
    }
    let delete = axum::http::Request::delete(format!("/api/v1/library/overrides/sets/{set}"))
        .body(Body::empty())
        .unwrap();
    let (s, j) = send(&app, delete).await;
    assert!(s.is_success(), "{s}: {j}");

    let (_, json) = send(&app, get_req(&format!("/api/v1/library/page?as={SHARER}"))).await;
    let portal = row(&json, "Portal");
    assert_eq!(
        portal["overrides"],
        serde_json::json!({"hz": 60}),
        "the box's"
    );
    assert!(portal.get("profile").is_none(), "{portal}");
    let half_life = row(&json, "Half-Life");
    assert!(half_life["overrides"].is_null(), "{half_life}");
    assert_eq!(half_life["profile"], serde_json::json!({"overrides": true}));
    let hz = crate::library::overrides_for(
        &crate::library::View::of(SHARER, false, Default::default()),
        "steam:400",
    );
    assert_eq!(hz.and_then(|o| o.hz), Some(60), "a launch takes the box's");
}

/// `as` is a profile the box holds, by a minted id, or 404 before it reaches a path.
#[tokio::test]
async fn as_names_a_minted_profile_or_nothing() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    for bad in [
        "..%2F..%2Fetc",
        "..",
        "A1B2C3D4E5F6",
        "a1b2c3d4e5f",
        "a1b2c3d4e5f60",
        "a1b2c3d4e5f6%2F",
        "a1b2c3d4e5f6%00",
        "0123456789ab",
        "operator",
    ] {
        for (method, path) in [
            (Method::GET, "/api/v1/library"),
            (Method::GET, "/api/v1/library/page"),
            (Method::GET, "/api/v1/library/scanners"),
            (Method::GET, "/api/v1/library/overrides"),
            (Method::GET, "/api/v1/session/settings"),
            (Method::PUT, "/api/v1/library/hidden/steam:400"),
        ] {
            let body = serde_json::json!({"hidden": true});
            let uri = format!("{path}?as={bad}");
            let (s, j) = send(&app, json_req(method.clone(), &uri, body)).await;
            let refused = (StatusCode::NOT_FOUND, &"profile_unknown".into());
            assert_eq!((s, &j["code"]), refused, "{method} {uri}");
        }
    }
    let layers: Vec<_> = std::fs::read_dir(tmp.path().join("library-profiles"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(layers, [SEAT_P], "no refused id made a folder");
    let (s, _) = send(&app, get_req("/api/v1/library?as=")).await;
    assert_eq!(s, StatusCode::OK, "an empty `as` names nobody");

    // A host with no profile store knows no profile.
    let bare = test_app(test_state(), None);
    let (s, _) = send(&bare, get_req(&format!("/api/v1/library?as={SEAT_P}"))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// Removing a profile removes its layer, and only its own.
#[tokio::test]
async fn removing_a_profile_removes_its_layer() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let put = json_req(
        Method::PUT,
        &format!("/api/v1/library/hidden/steam:70?as={SHARER}"),
        serde_json::json!({"hidden": true}),
    );
    assert_eq!(send(&app, put).await.0, StatusCode::OK);
    let layer = tmp.path().join(format!("library-profiles/{SHARER}"));
    assert!(layer.exists());
    let delete = axum::http::Request::delete(format!("/api/v1/profiles/{SHARER}"))
        .body(Body::empty())
        .unwrap();
    let (s, j) = send(&app, delete).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{j}");
    assert!(!layer.exists());
    assert!(tmp
        .path()
        .join(format!("library-profiles/{SEAT_P}"))
        .exists());
}

/// Set on the child only: where the box's answers for [`SEAT_P`] are.
const CHILD: &str = "PF_PROFILE_VIEW_TEST_CHILD";

/// The library routes `a_seat_host_lists_what_the_box_lists_as_its_profile` compares.
const SEAT_ROUTES: [&str; 4] = [
    "/api/v1/library",
    "/api/v1/library/page",
    "/api/v1/library/scanners",
    "/api/v1/library/overrides",
];

/// [`SEAT_P`]'s own host, with no `as`, answers what the box answers for `?as=` it: one fixture,
/// read as the box and as the seat from a trust dir. The seat's writes stay refused. The role is
/// process environment, so the seat runs in a child of this test binary.
#[tokio::test]
async fn a_seat_host_lists_what_the_box_lists_as_its_profile() {
    let Some(answers) = std::env::var_os(CHILD) else {
        return run_as_seat().await;
    };
    let answers: serde_json::Value =
        serde_json::from_slice(&std::fs::read(answers).unwrap()).unwrap();
    let own = pf_paths::seat::own_account();
    assert_eq!(
        crate::library::Reach::here(),
        crate::library::Reach::seat(own)
    );
    let app = test_app_on(test_state(), None, true);
    for path in SEAT_ROUTES {
        let (s, json) = send(&app, get_req(path)).await;
        assert_eq!(s, StatusCode::OK, "{path}: {json}");
        assert_eq!(json, answers[path], "{path}");
    }
    let (_, state) = send(&app, get_req("/api/v1/session/settings")).await;
    for key in ["settings", "configured"] {
        assert_eq!(state[key], answers["session"][key], "{key}");
    }

    let here = crate::library::View::here();
    assert_eq!(here.layer(), Some(SEAT_P));
    let hidden = crate::library::resolve_launch(&here, "steam:70");
    assert!(hidden.is_none(), "the profile hides it");
    let hz = crate::library::overrides_for(&here, "steam:400").and_then(|o| o.hz);
    assert_eq!(hz, Some(30));
    assert!(!crate::session_settings::get().session_on_game_exit);

    let hide = json_req(
        Method::PUT,
        "/api/v1/library/hidden/steam:400",
        serde_json::json!({"hidden": true}),
    );
    assert_eq!(send(&app, hide).await.0, StatusCode::CONFLICT);
    let put = json_req(
        Method::PUT,
        "/api/v1/session/settings",
        serde_json::json!({}),
    );
    assert_eq!(send(&app, put).await.0, StatusCode::CONFLICT);
    let drop = axum::http::Request::delete(format!("/api/v1/session/settings?as={SEAT_P}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&app, drop).await.0, StatusCode::CONFLICT);
    assert!(crate::session_settings::of_profile(&here).is_some());

    // A launch it can't resolve: held back by a hide or a source switch, the profile's or the
    // box's, or a title this desktop can't start (Playnite is one account's), or none at all.
    let why = |id: &str| crate::library::launch_refusal(&here, id).0;
    for id in ["steam:70", "steam:570", "lutris:4", "custom:r1"] {
        assert_eq!(why(id), "launch_title_not_in_library", "{id}");
    }
    for id in ["custom:p1", "steam:999999"] {
        assert_eq!(why(id), "launch_title_not_on_desktop", "{id}");
    }
}

/// The box says its library lacks every title it doesn't start: it has no desktop of a seat's
/// to blame. [`crate::library::held_back`] is the view's own hide and switch rules.
#[tokio::test]
async fn the_box_refuses_a_title_it_does_not_list_by_its_library() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let view = crate::library::View::of(SEAT_P, false, crate::library::Reach::seat(false));
    for id in ["steam:70", "steam:570", "lutris:4", "custom:r1"] {
        assert!(crate::library::held_back(&view, id), "{id}");
    }
    for id in ["custom:p1", "steam:400", "steam:999999"] {
        assert!(!crate::library::held_back(&view, id), "{id}");
    }
    for id in ["steam:70", "custom:p1", "steam:999999"] {
        let why = crate::library::launch_refusal(&view, id).0;
        assert_eq!(why, "launch_title_not_in_library", "{id}");
    }
}

/// The box's answers for [`SEAT_P`], then the seat's half of the test in a child.
async fn run_as_seat() {
    let tmp = ConfigDirOverride::new();
    write_fixture(tmp.path());
    let app = test_app(profiles_state(tmp.path(), profiles()), None);
    let mut answers = serde_json::Map::new();
    for path in SEAT_ROUTES {
        let (s, json) = send(&app, get_req(&format!("{path}?as={SEAT_P}"))).await;
        assert_eq!(s, StatusCode::OK, "{path}: {json}");
        answers.insert(path.into(), json);
    }
    let path = format!("/api/v1/session/settings?as={SEAT_P}");
    answers.insert("session".into(), send(&app, get_req(&path)).await.1);
    let file = tmp.path().join("answers.json");
    std::fs::write(&file, serde_json::Value::Object(answers).to_string()).unwrap();

    let seat = tempfile::tempdir().unwrap();
    let module = module_path!().split_once("::").expect("crate::module").1;
    let test = format!("{module}::a_seat_host_lists_what_the_box_lists_as_its_profile");
    let mut child = std::process::Command::new(std::env::current_exe().expect("this test binary"));
    child.args(["--exact", &test, "--nocapture"]);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("PUNKTFUNK_") {
            child.env_remove(key);
        }
    }
    // With no seat service to ask, the box counts a Windows seat as a person's own.
    if cfg!(windows) {
        child.env("PUNKTFUNK_SEAT_OWN_ACCOUNT", "1");
    }
    let out = child
        .env(CHILD, &file)
        .env("PUNKTFUNK_CONFIG_DIR", seat.path())
        .env("PUNKTFUNK_SEAT_SESSION", "1")
        .env("PUNKTFUNK_SEAT_ID", SEAT)
        .env("PUNKTFUNK_TRUST_DIR", tmp.path())
        .env("PUNKTFUNK_LIBRARY_DIR", tmp.path())
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
