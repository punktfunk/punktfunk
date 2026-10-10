//! Library routes: scanners, paging, stats, metadata, custom entries and providers.

use super::*;

// ------------------------------------------------------------------ library scanners

/// Toggle 404s unknown ids. A successful PUT would write `library-scanners.json` in the
/// real config dir, so only the rejection path is exercised here. Every row is a plugin;
/// an empty list is legitimate when no library plugins are installed.
#[tokio::test]
async fn library_scanner_list_and_unknown_toggle() {
    let app = test_app(test_state(), None);

    let (s, json) = send(&app, get_req("/api/v1/library/scanners")).await;
    assert_eq!(s, StatusCode::OK);
    let scanners = json.as_array().expect("a scanner array");
    assert!(
        scanners.iter().all(|sc| sc["origin"] == "plugin"),
        "no host build reports a builtin source any more: {json}"
    );
    assert!(
        scanners.iter().all(|sc| sc["id"].is_string()
            && sc["label"].is_string()
            && sc["enabled"].is_boolean()),
        "every source row must carry the shape the console renders: {json}"
    );
    // `custom` is a store, never a source — the toggle must not offer it.
    assert!(scanners.iter().all(|sc| sc["id"] != "custom"));

    let (s, json) = send(
        &app,
        axum::http::Request::put("/api/v1/library/scanners/not-a-store")
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                serde_json::json!({"enabled": false}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "unknown source id must 404: {json}"
    );
}

/// Library ids are `<store>:<external_id>` (Heroic has two colons). If the router split on
/// `:`, hide would 404 an id the host produced. The body is invalid on purpose so we never
/// write `library-hidden.json` into the real config dir.
#[tokio::test]
async fn hide_route_matches_ids_containing_colons() {
    let app = test_app(test_state(), None);
    let put = |id: &str| {
        axum::http::Request::put(format!("/api/v1/library/hidden/{id}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            // Not a `HiddenToggle` — rejected before the handler runs.
            .body(Body::from(serde_json::json!({"nope": 1}).to_string()))
            .unwrap()
    };

    for id in ["steam:70", "custom:abc", "heroic:legendary:fc0b13b7"] {
        let (s, json) = send(&app, put(id)).await;
        assert_ne!(
            s,
            StatusCode::NOT_FOUND,
            "`{id}` must ROUTE — a colon is a legal path character and every library id has one: {json}"
        );
        assert!(
            s.is_client_error(),
            "a body that is not a HiddenToggle must be refused, not accepted: {s} {json}"
        );
    }
}

/// Stats ride on the entry: absent until the first launch, then the four numbers as recorded.
/// The env override must cover the whole body (`paired_clients_list_and_unpair`).
#[allow(clippy::await_holding_lock)]
/// Seeds one custom title on a platform.
fn seed_title(title: &str, platform: &str) {
    crate::library::add_custom(crate::library::CustomInput {
        title: title.into(),
        art: Default::default(),
        launch: None,
        prep: None,
        role: Default::default(),
        icon: None,
        detect: None,
        on_window: None,
        audio: None,
        meta: crate::library::GameMeta {
            platform: Some(platform.into()),
            ..Default::default()
        },
    })
    .expect("seed a title");
}

fn titles(page: &serde_json::Value) -> Vec<String> {
    page["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|g| g["title"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Pages follow title order, the cursor names a place rather than an offset, and the filters
/// and counts agree with what the pages hold.
#[tokio::test]
async fn library_pages_by_cursor_with_search_and_counts() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    for (t, p) in [
        ("Delta", "PS2"),
        ("alpha", "PS2"),
        ("Charlie", "N64"),
        ("bravo", "PS2"),
        ("Echo", "N64"),
    ] {
        seed_title(t, p);
    }

    let (s, first) = send(&app, get_req("/api/v1/library/page?limit=2")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(titles(&first), ["alpha", "bravo"]);
    assert_eq!(first["total"], 5);
    assert_eq!(first["platforms"][0]["platform"], "PS2");
    assert_eq!(first["platforms"][0]["count"], 3);
    let cursor = first["next_cursor"]
        .as_str()
        .expect("more pages")
        .to_string();

    // A title that sorts before the cursor arrives between two pages: nothing repeats.
    seed_title("Able", "PS2");
    let (_, second) = send(
        &app,
        get_req(&format!("/api/v1/library/page?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(titles(&second), ["Charlie", "Delta"]);
    let cursor = second["next_cursor"]
        .as_str()
        .expect("one more")
        .to_string();
    let (_, last) = send(
        &app,
        get_req(&format!("/api/v1/library/page?limit=2&cursor={cursor}")),
    )
    .await;
    assert_eq!(titles(&last), ["Echo"]);
    assert!(last.get("next_cursor").is_none(), "{last}");

    let (_, found) = send(&app, get_req("/api/v1/library/page?q=HA")).await;
    assert_eq!(titles(&found), ["alpha", "Charlie"]);
    assert_eq!(found["total"], 2);

    // The platform filter narrows the page, not the counts beside it.
    let (_, n64) = send(&app, get_req("/api/v1/library/page?platform=n64")).await;
    assert_eq!(titles(&n64), ["Charlie", "Echo"]);
    assert_eq!(n64["platforms"].as_array().map(Vec::len), Some(2));

    let (s, _) = send(&app, get_req("/api/v1/library/page?cursor=not-a-cursor")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let id = first["items"][1]["id"].as_str().expect("an id");
    let (_, one) = send(&app, get_req(&format!("/api/v1/library/page?id={id}"))).await;
    assert_eq!(titles(&one), ["bravo"]);

    // A hidden title leaves every page but the operator's, where it is flagged.
    crate::library::set_entry_hidden(id, true).expect("hide");
    let (_, all) = send(&app, get_req("/api/v1/library/page")).await;
    assert_eq!(all["total"], 6);
    let flagged: Vec<_> = all["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|g| g["hidden"] == true)
        .map(|g| g["title"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(flagged, ["bravo"]);
}

/// The built list is kept between requests and dropped when a library file moves, whether
/// this process wrote it or someone else did.
#[test]
fn the_built_library_is_kept_until_an_input_moves() {
    let _tmp = ConfigDirOverride::new();
    seed_title("alpha", "PS2");
    let first = crate::library::sorted_games();
    assert!(Arc::ptr_eq(&first, &crate::library::sorted_games()));

    seed_title("bravo", "PS2");
    let second = crate::library::sorted_games();
    assert!(!Arc::ptr_eq(&first, &second));
    assert_eq!(second.len(), 2);

    let by_hand = pf_paths::config_dir().join("library-stats.json");
    std::fs::write(by_hand, r#"{"games":{}}"#).expect("write the stats file");
    assert!(!Arc::ptr_eq(&second, &crate::library::sorted_games()));
}

/// A Windows seat lists and resolves the box's titles from `PUNKTFUNK_LIBRARY_DIR`, leaves one
/// account's sources out, rebuilds on its own play stats, and answers a library write with 409.
#[tokio::test]
async fn a_seat_plays_the_boxs_library_and_changes_none_of_it() {
    let boxdir = tempfile::tempdir().unwrap();
    let catalog = serde_json::json!({
        "entries": [
            {"id": "s1", "title": "Portal", "provider": "steam", "store": "steam",
             "external_id": "400", "launch": {"kind": "steam_appid", "value": "400"}},
            {"id": "p1", "title": "The owner's", "provider": "playnite", "external_id": "x1"},
        ],
        "claims": {"steam": "steam"},
    });
    std::fs::write(boxdir.path().join("library.json"), catalog.to_string()).unwrap();
    let seat = ConfigDirOverride::seat(boxdir.path());
    let app = test_app_on(test_state(), None, true);

    let games = crate::library::sorted_games();
    let ids: Vec<&str> = games.iter().map(|g| g.id.as_str()).collect();
    assert_eq!(ids, ["steam:400"]);
    assert!(crate::library::resolve_launch("steam:400").is_some());
    let sources: Vec<String> = crate::library::list_scanners()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(sources, ["steam"]);

    std::fs::write(seat.path().join("library-stats.json"), r#"{"games":{}}"#).unwrap();
    assert!(!Arc::ptr_eq(&games, &crate::library::sorted_games()));

    let hide = json_req(
        Method::PUT,
        "/api/v1/library/hidden/steam:400",
        serde_json::json!({"hidden": true}),
    );
    let (s, _) = send(&app, hide).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(!boxdir.path().join("library-hidden.json").exists());
    let (s, _) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn library_stats_ride_on_the_entry() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    let added = crate::library::add_custom(crate::library::CustomInput {
        title: "Chrono Trigger".into(),
        art: Default::default(),
        launch: None,
        prep: None,
        role: Default::default(),
        icon: None,
        detect: None,
        on_window: None,
        audio: None,
        meta: Default::default(),
    })
    .expect("seed one custom title");
    let id = crate::library::library_id_for(&added);

    let (s, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json[0]["id"], id.as_str());
    assert!(
        json[0].get("stats").is_none(),
        "a never-launched title carries no stats key: {json}"
    );

    crate::library::record_launch(&id, Some("kid"));
    crate::library::record_run_time(&id, Some("kid"), std::time::Duration::from_millis(1_500));
    crate::library::record_run_time(&id, Some("kid"), std::time::Duration::from_millis(500));
    // A run credited to an id with no entry is kept but never surfaces.
    crate::library::record_run_time("steam:404", None, std::time::Duration::from_secs(1));

    let (s, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json.as_array().map(Vec::len), Some(1), "{json}");
    let stats = &json[0]["stats"];
    assert_eq!(stats["launch_count"], 1, "{json}");
    assert_eq!(stats["play_time_ms"], 2_000);
    assert_eq!(stats["last_run_ms"], 2_000);
    assert!(stats["last_played_unix_ms"]
        .as_u64()
        .is_some_and(|ms| ms > 0));
    assert!(stats.get("mine").is_none(), "no `as`, no `mine`: {json}");

    // Another profile's launch moves the totals, not the kid's own numbers.
    crate::library::record_launch(&id, Some("enrico"));
    for path in ["/api/v1/library?as=kid", "/api/v1/library/page?as=kid"] {
        let (s, json) = send(&app, get_req(path)).await;
        assert_eq!(s, StatusCode::OK);
        let stats = json
            .get("items")
            .map_or(&json[0]["stats"], |items| &items[0]["stats"]);
        assert_eq!(stats["launch_count"], 2, "{path}: {json}");
        assert_eq!(stats["mine"]["launch_count"], 1, "{path}: {json}");
        assert_eq!(stats["mine"]["play_time_ms"], 2_000, "{path}: {json}");
    }
    let (_, json) = send(&app, get_req("/api/v1/library?as=nobody")).await;
    assert!(json[0]["stats"].get("mine").is_none(), "{json}");
}

/// The lease watcher credits a recorded launch's run: seen running, then gone, lands on disk.
/// Launches are counted by the planes, never by the lease — the count stays zero here.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "drives a real process for ~12s (shim window + exit confirmation)"]
fn a_recorded_launch_credits_its_run_to_the_library_stats() {
    use std::os::unix::process::CommandExt;
    let tmp = ConfigDirOverride::new();
    let script = tmp.path().join("game.sh");
    std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
    let launch_stamp = crate::gamelease::launch_clock();
    let child = std::process::Command::new("/bin/sh")
        .arg(&script)
        .process_group(0)
        .spawn()
        .expect("spawn the fake game");

    let lease = crate::gamelease::open(
        crate::gamelease::LeaseRequest {
            game: crate::gamelease::GameRef {
                id: Some("custom:stats-run".into()),
                store: Some("custom".into()),
                title: "Stats Run".into(),
            },
            client: "test".into(),
            fingerprint: None,
            preset: None,
            plane: crate::events::Plane::Native,
            profile: Some(crate::events::ProfileRef {
                id: "kid".into(),
                display_name: "Kid".into(),
            }),
            spec: crate::library::DetectSpec::dir(tmp.path()),
            nested: false,
            scope_pid: None,
            launcher: false,
            child: Some((child, true)),
            spawned: None,
            launch_stamp,
            // Recorded: this is what makes the run count.
            procs: Some(std::sync::Arc::new(std::sync::Mutex::new(Vec::new()))),
            #[cfg(target_os = "linux")]
            workspace: None,
            window: None,
            outcome: None,
        },
        Box::new(|| {}),
    );
    let shared = lease.shared();
    let wait_for = |state: crate::gamelease::GameState, secs: u64| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline && shared.state() != state {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(shared.state(), state);
    };
    // Past the shim window the child is the game.
    wait_for(crate::gamelease::GameState::Running, 15);
    crate::gamelease::terminate(shared.clone(), "test asked");
    wait_for(crate::gamelease::GameState::Exited, 30);

    let stats = crate::library::game_stats();
    let title = stats.get("custom:stats-run").expect("the run was credited");
    assert_eq!(title.by_profile["kid"], title.totals, "one profile ran it");
    let s = title.totals;
    assert!(s.play_time_ms >= 500, "seen running for a while: {s:?}");
    assert_eq!(s.last_run_ms, s.play_time_ms, "one run: {s:?}");
    assert_eq!(s.launch_count, 0, "the lease never counts launches: {s:?}");
    assert_eq!(s.last_played_unix_ms, 0);
}

/// A metadata source fills a gap, a pick beats it, the replace switch beats own art, and
/// DELETE forgets the source. The env override must cover the whole body.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn metadata_sources_fill_pick_replace_and_forget() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    let json_req = |method: &str, uri: &str, body: serde_json::Value| {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let added = crate::library::add_custom(crate::library::CustomInput {
        title: "Hades".into(),
        art: crate::library::Artwork {
            portrait: Some("https://own/p.png".into()),
            ..Default::default()
        },
        launch: None,
        prep: None,
        role: Default::default(),
        icon: None,
        detect: None,
        on_window: None,
        audio: None,
        meta: Default::default(),
    })
    .expect("seed one custom title");
    let id = crate::library::library_id_for(&added);

    let (s, json) = send(
        &app,
        json_req(
            "PUT",
            "/api/v1/library/metadata/sgdb",
            serde_json::json!({
                "matching": "search",
                "entries": [
                    {"id": id, "art": {"portrait": "https://sgdb/p.png", "logo": "https://sgdb/l.png",
                     "hero": "file:///etc/passwd"}, "meta": {"developer": "Supergiant"}},
                    {"id": "not-an-id", "art": {"logo": "https://sgdb/x.png"}}
                ]
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(
        (json["entries"].as_u64(), json["dropped"].as_u64()),
        (Some(1), Some(2))
    );

    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    let g = &json[0];
    assert_eq!(g["filled"]["logo"], "sgdb", "{json}");
    assert_eq!(g["developer"], "Supergiant");
    assert!(
        g["filled"].get("portrait").is_none(),
        "own art stays: {json}"
    );
    assert!(g["art"]["logo"]
        .as_str()
        .unwrap()
        .starts_with("/api/v1/library/art/"));

    let pick = format!("/api/v1/library/picks/{id}");
    let (s, _) = send(
        &app,
        json_req(
            "PUT",
            &pick,
            serde_json::json!({"kind": "logo", "url": "file:///x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a pick is http(s) only");
    let (s, _) = send(
        &app,
        json_req(
            "PUT",
            &pick,
            serde_json::json!({"kind": "logo", "url": "https://pick/l.png"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(json[0]["filled"]["logo"], "pick");

    let (s, json) = send(
        &app,
        json_req(
            "PUT",
            "/api/v1/library/metadata",
            serde_json::json!([{"id": "sgdb", "enabled": true, "replace": true}]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json[0]["replace"], true, "{json}");
    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(
        json[0]["filled"]["portrait"], "sgdb",
        "replace beats own art: {json}"
    );

    let del = axum::http::Request::delete("/api/v1/library/metadata/sgdb")
        .body(Body::empty())
        .unwrap();
    let (s, json) = send(&app, del).await;
    assert_eq!((s, json["removed"].as_bool()), (StatusCode::OK, Some(true)));
    let (_, json) = send(&app, get_req("/api/v1/library/metadata")).await;
    assert_eq!(json.as_array().map(Vec::len), Some(0), "{json}");
    let (_, json) = send(&app, get_req("/api/v1/library")).await;
    assert_eq!(
        json[0]["filled"]["logo"], "pick",
        "the pick outlives the source: {json}"
    );
    assert!(json[0].get("developer").is_none());
}

/// The stored row's `detect` and `prep` come back on the operator lane, and an update that
/// omits them keeps them — the console form never showed them and used to clear them on
/// every save (#1064). Sending the field, even empty, still replaces it.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn custom_entry_hints_round_trip_and_survive_an_update() {
    let _tmp = ConfigDirOverride::new();
    let app = test_app(test_state(), None);
    let added = crate::library::add_custom(crate::library::CustomInput {
        title: "Eden".into(),
        art: Default::default(),
        launch: None,
        prep: Some(vec![crate::hooks::PrepCmd {
            run: "true".into(),
            undo: None,
        }]),
        role: Default::default(),
        icon: None,
        detect: Some(crate::library::DetectHint {
            exe: Some("/usr/bin/eden".into()),
            ..Default::default()
        }),
        on_window: None,
        audio: None,
        meta: Default::default(),
    })
    .expect("seed one custom title");
    let path = format!("/api/v1/library/custom/{}", added.id);

    let (s, json) = send(&app, get_req(&path)).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    assert_eq!(json["detect"]["exe"], "/usr/bin/eden");
    assert_eq!(json["prep"][0]["do"], "true");

    let (s, json) = send(
        &app,
        json_req(
            Method::PUT,
            &path,
            serde_json::json!({ "title": "Eden II" }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{json}");
    let (_, json) = send(&app, get_req(&path)).await;
    assert_eq!(json["title"], "Eden II");
    assert_eq!(
        json["detect"]["exe"], "/usr/bin/eden",
        "omitted detect is kept: {json}"
    );
    assert_eq!(
        json["prep"][0]["do"], "true",
        "omitted prep is kept: {json}"
    );

    let body = serde_json::json!({ "title": "Eden II", "detect": {}, "prep": [] });
    let (s, json) = send(&app, json_req(Method::PUT, &path, body)).await;
    assert_eq!(s, StatusCode::OK, "{json}");
    let (_, json) = send(&app, get_req(&path)).await;
    assert!(
        json.get("detect").is_none(),
        "an explicit empty hint clears it: {json}"
    );
    assert!(
        json.get("prep").is_none(),
        "an explicit empty list clears it: {json}"
    );

    let (s, _) = send(&app, get_req("/api/v1/library/custom/nonesuch")).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ library providers

/// Validation only; a successful PUT would touch the real catalog (`library::custom` covers writes).
#[tokio::test]
async fn provider_reconcile_validation() {
    let app = test_app(test_state(), None);
    let put = |provider: &str, body: serde_json::Value| {
        axum::http::Request::put(format!("/api/v1/library/provider/{provider}"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, json) = send(&app, put("manual", serde_json::json!([]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("reserved"));
    let (s, _) = send(&app, put("Bad%2FName", serde_json::json!([]))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        put(
            "romm",
            serde_json::json!([{"external_id": "", "title": "X"}]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, json) = send(
        &app,
        put(
            "romm",
            serde_json::json!([
                {"external_id": "a", "title": "A"},
                {"external_id": "a", "title": "B"}
            ]),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("duplicate"));

    let del = axum::http::Request::delete("/api/v1/library/provider/manual")
        .body(Body::empty())
        .unwrap();
    let (s, _) = send(&app, del).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// The plugin runner starts every store at once, so their first syncs land together.
#[test]
fn concurrent_provider_syncs_keep_every_row() {
    let _dir = ConfigDirOverride::new();
    std::thread::scope(|s| {
        for p in 0..8 {
            s.spawn(move || {
                for _ in 0..20 {
                    let row = serde_json::json!({"external_id": "a", "title": "A"});
                    let inputs = vec![serde_json::from_value(row).unwrap()];
                    crate::library::reconcile_provider(&format!("p{p}"), None, inputs)
                        .expect("sync saved");
                }
            });
        }
    });
    let rows = crate::library::load_custom();
    assert_eq!(rows.len(), 8, "one row per provider: {rows:?}");
}

/// Unknown titles are counted, not refused: a report races its own reconcile, and 400-ing
/// the whole report would drop every other running title. Catalog is untouched, so every
/// id here is unknown by construction.
#[tokio::test]
async fn provider_running_report_validation() {
    let app = test_app(test_state(), None);
    let put = |provider: &str, body: serde_json::Value| {
        axum::http::Request::put(format!("/api/v1/library/provider/{provider}/running"))
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };

    let (s, json) = send(&app, put("manual", serde_json::json!({"running": []}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(json["error"].as_str().unwrap().contains("reserved"));
    let (s, _) = send(&app, put("Bad%2FName", serde_json::json!({"running": []}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Unreported provider: a legitimate "nothing is running".
    let (s, json) = send(&app, put("playnite", serde_json::json!({"running": []}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json["matched"], 0);
    assert_eq!(json["unknown"], 0);
    assert!(json["ttl_s"].as_u64().unwrap() > 0);

    let (s, json) = send(
        &app,
        put(
            "playnite",
            serde_json::json!({"running": [{"external_id": "no-such-title", "pid": 4242}]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(json["matched"], 0);
    assert_eq!(json["unknown"], 1);

    // A report of unpublished titles must not hold a real lease open.
    assert!(!crate::runstate::speaks_for(Some("playnite:no-such-title")));
    crate::runstate::forget("playnite");
}
