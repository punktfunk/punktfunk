//! A profile's own layer of the library (`library-profiles/<id>/`), and the box with none.

use super::*;

/// A profile with a seat of its own.
const SEAT_P: &str = "a1b2c3d4e5f6";

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

    let hz = crate::library::overrides_for("steam:400").and_then(|o| o.hz);
    assert_eq!(hz, Some(60));
    assert!(
        crate::library::resolve_launch("steam:570").is_none(),
        "hidden"
    );
    assert!(crate::library::resolve_launch("steam:70").is_some());
}
