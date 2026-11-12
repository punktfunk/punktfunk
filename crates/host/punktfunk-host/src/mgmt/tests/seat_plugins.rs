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

/// Runs the test `name` of this module as a child on a made seat, then on a person's own
/// account.
fn on_both_seats(name: &str) {
    let module = module_path!().split_once("::").expect("crate::module").1;
    let path = format!("{module}::{name}");
    super::own_account::on_seat(&path, CHILD, false);
    super::own_account::on_seat(&path, CHILD, true);
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

/// What a seat host answers a provider's rows on each lane: the operator, the seat's person,
/// the plugin under its own id and under another's, and a device.
#[tokio::test]
async fn a_seat_s_plugins_publish_nothing_on_it() {
    if std::env::var_os(CHILD).is_none() {
        return on_both_seats("a_seat_s_plugins_publish_nothing_on_it");
    }
    let app = test_app_seat(test_state(), TOKEN, MINE);
    let person = person_session(&app).await;
    let rows = json!([{"external_id": "q1", "title": "Quake",
        "launch": {"kind": "steam_appid", "value": "2310"}}]);
    let put = |provider: &str| {
        let path = format!("/api/v1/library/provider/{provider}");
        json_req(Method::PUT, &path, rows.clone())
    };
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
        (
            "another plugin",
            send(&app, with_bearer(put("other"), "demo-secret")).await,
        ),
    ];
    for (lane, (status, why)) in lanes {
        assert_eq!(status, StatusCode::CONFLICT, "{lane}: {why}");
        assert_eq!(why["code"], "seat_library_read_only", "{lane}");
    }
    let device = send_cert(&app, put("demo"), DEVICE).await;
    assert_eq!(device, StatusCode::UNAUTHORIZED);
    let view = crate::library::View::here();
    assert!(crate::library::all_games_for(&view)
        .iter()
        .all(|g| g.title != "Quake"));
}
