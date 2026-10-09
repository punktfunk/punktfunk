//! Host info, diagnostics, display and host settings, and the GPU preference.

use super::*;

#[tokio::test]
async fn host_info_reports_identity_and_ports() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/host")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["hostname"], "test-host");
    assert_eq!(body["uniqueid"], "deadbeef");
    // `os` is the icon-walk chain; `os_name` is the human label. Both copied from Host.
    assert_eq!(body["os"], "linux/arch/steamos");
    assert_eq!(body["os_name"], "SteamOS");
    assert_eq!(body["ports"]["http"], HTTP_PORT);
    assert_eq!(body["ports"]["mgmt"], DEFAULT_PORT);
    // Assert against `host_wire_caps`, not a fixed set. HEVC serializes as "hevc", never "h265".
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
    let caps = crate::encode::host_wire_caps();
    let expected: Vec<&str> = [
        (CODEC_H264, "h264"),
        (CODEC_HEVC, "hevc"),
        (CODEC_AV1, "av1"),
        (CODEC_PYROWAVE, "pyrowave"),
    ]
    .into_iter()
    .filter(|(bit, _)| caps & bit != 0)
    .map(|(_, name)| name)
    .collect();
    assert_eq!(body["codecs"], serde_json::json!(expected));
    assert!(caps & CODEC_H264 != 0, "H.264 is always encodable");
    assert_eq!(body["gamestream"], false);
    assert_eq!(body["door"], false, "a test host is no door");
}

/// A device sent a connect link has to be able to check what it reached, so the host publishes
/// its own leaf fingerprint. Public by construction — every client reads it off the handshake.
#[tokio::test]
async fn host_info_publishes_the_hosts_own_fingerprint() {
    let stats = test_stats();
    let state = test_state();
    let app = crate::mgmt::app(
        state,
        Some("test-secret".to_string()),
        Some("plugin-secret".to_string()),
        shared_plugin_tokens(test_plugin_tokens()),
        Some(TRAY_TOKEN.to_string()),
        DEFAULT_PORT,
        None,
        stats,
        test_client_logs_dir(),
        test_access_dir(),
        false,
        Some([0xab; 32]),
        false,
        false,
    );
    let (status, body) = send(&app, get_req("/api/v1/host")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["fingerprint"], "ab".repeat(32));

    // An identity that could not be parsed says so rather than publishing a wrong pin.
    let app = test_app(test_state(), None);
    let (_, body) = send(&app, get_req("/api/v1/host")).await;
    assert!(body["fingerprint"].is_null());
}

#[tokio::test]
async fn compositors_lists_all_backends_with_flags() {
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/compositors")).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().expect("array");
    // Compositors are Linux-only; elsewhere the list is empty so the console can say N/A.
    #[cfg(not(target_os = "linux"))]
    assert!(arr.is_empty(), "non-Linux hosts advertise no compositors");
    #[cfg(target_os = "linux")]
    {
        let ids: Vec<&str> = arr.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["kwin", "gamescope", "mutter", "wlroots", "hyprland"]);
    }
    for c in arr {
        assert!(c["available"].is_boolean());
        assert!(c["default"].is_boolean());
        assert!(c["label"].as_str().is_some_and(|s| !s.is_empty()));
    }
    // At most one auto-detect default; none if the test env has no desktop.
    assert!(arr.iter().filter(|c| c["default"] == true).count() <= 1);
}

/// Verdicts carry the host user, group layout, and device-node state.
#[tokio::test]
async fn diagnostics_require_the_operator_token() {
    let app = test_app(test_state(), Some("sekrit"));

    for req in [
        get_req("/api/v1/diagnostics"),
        axum::http::Request::post("/api/v1/diagnostics/refresh")
            .body(Body::empty())
            .unwrap(),
    ] {
        let (status, body) = send(&app, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].as_str().unwrap().contains("bearer"));
    }
}

/// Worst-first list; every registered check appears, including `ok` and `inapplicable`.
#[tokio::test]
async fn diagnostics_report_the_registered_checks() {
    // Process-global registry; take a first reading here rather than whatever a sibling left.
    crate::diagnostics::preflight();
    let app = test_app(test_state(), None);
    let (status, body) = send(&app, get_req("/api/v1/diagnostics")).await;
    assert_eq!(status, StatusCode::OK);

    assert!(body["ran_at_unix"].is_number(), "the report is stamped");
    let checks = body["checks"].as_array().expect("checks array");
    assert!(
        !checks.is_empty(),
        "the v1 catalog is registered at startup"
    );

    // Ids are console i18n keys; a rename silently drops every translation.
    let ids: Vec<&str> = checks.iter().filter_map(|c| c["id"].as_str()).collect();
    for expected in [
        "takeover_privilege",
        "virtual_deck_vhci",
        "uinput_access",
        "server_conflict",
    ] {
        assert!(ids.contains(&expected), "missing check {expected}: {ids:?}");
    }

    // N/N−1 on the wire: an older console has only these strings to render.
    for check in checks {
        let id = check["id"].as_str().unwrap();
        assert!(
            !check["summary"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .is_empty(),
            "{id}: summary must never be empty"
        );
        assert!(
            matches!(
                check["status"].as_str(),
                Some("ok" | "warn" | "fail" | "inapplicable")
            ),
            "{id}: unexpected status {:?}",
            check["status"]
        );
        if check["status"] == "fail" {
            assert!(
                !check["remedy"]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .is_empty(),
                "{id}: a failing check must tell the operator what to do"
            );
        }
    }
}

#[tokio::test]
async fn diagnostics_refresh_reruns_and_returns_the_report() {
    let app = test_app(test_state(), None);
    let req = axum::http::Request::post("/api/v1/diagnostics/refresh")
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);

    let checks = body["checks"].as_array().expect("checks array");
    assert!(!checks.is_empty(), "refresh answers with the full catalog");
    // Do not assert `source: "refresh"`: the registry is process-global and a sibling may have
    // primed a startup reading. Isolated pin: `diagnostics::tests::refresh_reruns_every_probe`.
    assert!(checks.iter().all(|c| c["id"].is_string()));
}

/// GET-only: `prefs()` is a process-global `OnceLock`, so a PUT would race other tests.
/// `keep_alive: forever` is read off the gaming-rig preset without writing.
#[tokio::test]
async fn display_settings_surface() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/settings")).await;
    assert_eq!(status, StatusCode::OK);
    let presets = body["presets"].as_array().expect("presets array");
    assert_eq!(
        presets.len(),
        5,
        "all five named presets are surfaced for the console picker"
    );
    assert!(
        body["effective"]["keep_alive"].is_object(),
        "the effective policy is echoed"
    );
    let gaming = presets
        .iter()
        .find(|p| p["id"] == "gaming-rig")
        .expect("gaming-rig preset surfaced");
    assert_eq!(
        gaming["fields"]["keep_alive"]["mode"], "forever",
        "gaming-rig is keep_alive: forever"
    );
    let enforced: Vec<&str> = body["enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(enforced.contains(&"keep_alive"));
    assert!(enforced.contains(&"topology"));
    assert!(enforced.contains(&"mode_conflict"));
    assert!(enforced.contains(&"identity"));
    assert!(enforced.contains(&"layout"));
    // The console renders this list verbatim and hides anything absent from it
    // (design/web-console-overhaul.md D1), so a name here is a control an operator can
    // click. A build that cannot act on a field must not advertise it.
    assert_eq!(
        enforced.contains(&"ddc_power_off"),
        cfg!(target_os = "windows"),
        "DDC/CI power-off is the Windows exclusive-isolate lever"
    );
    assert_eq!(
        enforced.contains(&"pnp_disable_monitors"),
        cfg!(target_os = "windows"),
        "PnP monitor-disable is the Windows exclusive-isolate lever"
    );
    // These three are additionally conditional ON their platform — an AMD driver, the
    // MIRROR backend, a gamescope binary — so only the negative direction is universal.
    assert!(
        !enforced.contains(&"edid_lock") || cfg!(target_os = "windows"),
        "EDID lock is the AMD driver's connector emulation, which exists only on Windows"
    );
    assert!(
        !enforced.contains(&"capture_monitor")
            || cfg!(any(target_os = "linux", target_os = "windows")),
        "pinning a real monitor needs a mirror backend"
    );
    assert!(
        !enforced.contains(&"game_session") || cfg!(target_os = "linux"),
        "a dedicated game session is a headless gamescope spawn"
    );
}

/// `/host/settings`: a PATCH stores, `null` resets, and a refused value writes nothing and names
/// the setting. Env beating the store is `pf-host-config`'s test, against a fake environment.
/// Uses only restart-class rows, so a concurrent session test never sees a changed value.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn host_settings_surface() {
    let dir = ConfigDirOverride::new();
    pf_host_config::reload();
    let app = test_app(test_state(), None);
    let patch = |body: serde_json::Value| {
        axum::http::Request::patch("/api/v1/host/settings")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let row = |body: &serde_json::Value, id: &str| {
        body["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("no {id} row"))
    };

    let (status, body) = send(&app, get_req("/api/v1/host/settings")).await;
    assert_eq!(status, StatusCode::OK);
    let name = row(&body, "host_name");
    assert_eq!(name["source"], "default");
    assert_eq!(name["kind"], "text");
    assert_eq!(name["apply"], "restart");
    assert_eq!(name["env"], "PUNKTFUNK_HOST_NAME");
    let ids: Vec<&str> = body["settings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .collect();
    assert_eq!(
        ids.contains(&"max_fps"),
        cfg!(target_os = "linux"),
        "a row this host does not act on is absent, not disabled"
    );

    let (status, body) = send(
        &app,
        patch(serde_json::json!({"host_name": "Den", "webtransport": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row(&body, "host_name")["value"], "Den");
    assert_eq!(row(&body, "host_name")["source"], "store");
    let file: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("host-settings.json")).unwrap())
            .unwrap();
    assert_eq!(file["webtransport"], true);

    let (status, err) = send(
        &app,
        patch(serde_json::json!({"webtransport": false, "host_name": "x".repeat(64)})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err["error"].as_str().unwrap().contains("host_name"));
    let (status, _) = send(&app, patch(serde_json::json!({"no_such_setting": 1}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, body) = send(&app, get_req("/api/v1/host/settings")).await;
    assert_eq!(
        row(&body, "webtransport")["value"],
        true,
        "a refused patch writes nothing"
    );

    let (status, body) = send(
        &app,
        patch(serde_json::json!({"host_name": null, "webtransport": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(row(&body, "host_name")["source"], "default");
    assert!(row(&body, "webtransport")["stored"].is_null());
    drop(dir);
    pf_host_config::reload();
}

/// The per-device overlay routes (`design/web-console-overhaul.md` §6.1).
///
/// **Read-only on purpose.** `policy::prefs()` is a process-global `OnceLock`
/// bound to whatever `PUNKTFUNK_CONFIG_DIR` said at its FIRST use anywhere in
/// this binary, so `ConfigDirOverride` cannot move it afterwards — a test that
/// PUT a policy here wrote to the developer's real `display-settings.json`.
/// Resolution, sanitisation and the insert/remove round-trip are covered
/// against in-memory policies in `pf-vdisplay`'s `policy::tests::client_overlay`;
/// what is worth asserting HERE is the wiring the console depends on.
#[tokio::test]
async fn display_client_overlay_is_served_beside_the_policy_never_inside_it() {
    let app = test_app(test_state(), None);

    // An unknown device is "follows host", not 404: absent fields ARE the answer.
    let (status, body) = send(&app, get_req("/api/v1/display/clients/aa11")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, serde_json::json!({}));

    let (status, body) = send(&app, get_req("/api/v1/display/settings")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("clients").is_some(),
        "overlays ride their own field so one fetch paints the device rows"
    );
    assert!(
        body["settings"].get("clients").is_none(),
        "and never inside `settings`, which the console PUTs back whole"
    );

    // Only fields this build actually acts on per device, and never one the host
    // does not act on at all — the console renders this list verbatim (D1).
    let per_device: Vec<&str> = body["client_enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(per_device.contains(&"mode_conflict"));
    let host_wide: Vec<&str> = body["enforced"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    // A field that IS a host axis must be enforced host-wide too — offering it per device
    // while the host ignores it everywhere would be the dead control D1 is about. Fields
    // that exist only per device (a mode cap, a scale) have no host-wide twin to check.
    const HOST_AXES: [&str; 4] = ["keep_alive", "topology", "mode_conflict", "identity"];
    for field in per_device.iter().filter(|f| HOST_AXES.contains(f)) {
        assert!(
            host_wide.contains(field),
            "{field} is offered per-device but this build does not act on it at all"
        );
    }
}

/// The bug this exists for: `clients` never rides the wire, so a host-wide PUT always arrives
/// with an empty map, and the store replaces the whole policy. Without the carry-across, every
/// preset click silently reverted every device to the host policy.
///
/// Pure on purpose — `policy::prefs()` is a process-global bound to the config dir at its first
/// use anywhere in this binary, so a test that drove the real route would write to the
/// developer's own `display-settings.json`.
#[test]
fn a_host_wide_save_carries_the_stored_overlays_across() {
    use crate::vdisplay::policy::{ClientOverlay, DisplayPolicy, KeepAlive, Preset};
    let mut stored = DisplayPolicy::default();
    stored.clients.insert(
        "aa11".into(),
        ClientOverlay {
            keep_alive: Some(KeepAlive::Forever),
            ..ClientOverlay::default()
        },
    );
    // What the console sends back: the policy it was served, which carries no overlays.
    let incoming = DisplayPolicy {
        preset: Preset::Workstation,
        ..DisplayPolicy::default()
    };
    assert!(incoming.clients.is_empty(), "the wire never carries them");

    let merged = crate::mgmt::display::with_stored_overlays(incoming, &stored);
    assert_eq!(
        merged.preset,
        Preset::Workstation,
        "the operator's change lands"
    );
    assert_eq!(
        merged.effective_for(Some("aa11")).keep_alive,
        KeepAlive::Forever,
        "and the device keeps its own pin"
    );
}

/// A stale console must not be able to revert per-device work it never saw by
/// PUTting back the whole policy object it fetched earlier. Refused before any
/// write, so this asserts the guard without touching the store.
#[tokio::test]
async fn the_host_wide_policy_put_refuses_to_carry_overlays() {
    let app = test_app(test_state(), None);
    let (status, _) = send(
        &app,
        json_req(
            Method::PUT,
            "/api/v1/display/settings",
            serde_json::json!({
                "preset": "default",
                "clients": {"aa11": {"mode_conflict": "join"}}
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// No backend has created a display here (non-Windows reports none): empty `/state`, no-op `/release`.
#[tokio::test]
async fn display_state_and_release_empty() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/state")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["displays"].as_array().map(|a| a.len()),
        Some(0),
        "no managed displays on an idle test host"
    );

    let (status, body) = send(
        &app,
        json_req(
            Method::POST,
            "/api/v1/display/release",
            serde_json::json!({}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["released"], 0);
}

/// Always 200 with a well-formed envelope, even with no compositor. Enumeration failure is
/// an `error` string beside an empty list, never a 5xx.
#[tokio::test]
async fn display_monitors_answers_even_with_no_compositor() {
    let app = test_app(test_state(), None);

    let (status, body) = send(&app, get_req("/api/v1/display/monitors")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["monitors"].is_array(), "monitors is always an array");
    let listed = body["monitors"].as_array().map(|a| a.len()).unwrap_or(0);
    // gamescope owns no physical heads, so empty-and-silent is correct. A leftover
    // `gamescope-0` socket makes `detect()` resolve gamescope on a dev box that was in game mode.
    let nested = body["compositor"] == "gamescope";
    assert!(
        listed > 0 || !body["error"].is_null() || body["compositor"].is_null() || nested,
        "an empty list must carry an error, an absent compositor, or a nested one: {body}"
    );
    // Pin is reported so the console can flag a missing monitor; unset here.
    assert!(body["pinned"].is_null(), "no PUNKTFUNK_CAPTURE_MONITOR set");
}

/// Inventory GET always answers (empty on a GPU-less box). Preference PUT validates
/// mode + gpu_id before touching the store.
#[tokio::test]
async fn gpu_endpoints_list_and_validate() {
    let app = test_app(test_state(), None);

    let (s, b) = send(&app, get_req("/api/v1/gpus")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b["gpus"].is_array());
    assert!(b["mode"].is_string());
    // `encoder_pin` is null when nothing is pinned; the console warns when a pin contradicts
    // the selected GPU (the pin is overridden at session open).
    assert!(
        b.as_object().unwrap().contains_key("encoder_pin"),
        "listGpus must carry encoder_pin"
    );

    let (s, _) = send(
        &app,
        json_req(
            Method::PUT,
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "fastest"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        json_req(
            Method::PUT,
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, _) = send(
        &app,
        json_req(
            Method::PUT,
            "/api/v1/gpus/preference",
            serde_json::json!({"mode": "manual", "gpu_id": "ffff-ffff-9"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
