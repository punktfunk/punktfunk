//! The route table: every route's lanes, the seat proxy and the checked-in OpenAPI document.

use super::*;

/// Every live route has an explicit plugin/cert classification. A new route fails until a
/// row is added here; a removed route must not leave a stale row. The gates are allowlists:
/// unclassified means denied.
#[test]
fn every_route_is_classified_for_the_plugin_and_cert_lanes() {
    use axum::http::Method;

    // (method, path, plugin_ok, cert_ok). One row per live operation; no wildcards.
    const EXPECTED: &[(&str, &str, bool, bool)] = &[
        // Host/status: plugin-readable; the small read-only set is the cert lane's.
        ("GET", "/api/v1/health", true, false), // always open, handled before either gate
        // The browser plane's certificate hash. Neither gate admits it and neither needs to:
        // `require_auth` exempts the path outright, because a browser that has never paired holds
        // no credential and the response authorises nothing — a certificate hash is what any peer
        // learns by connecting. See `mgmt::webtransport`.
        ("GET", "/api/v1/webtransport", false, false),
        // The device exchange. Same reason as the row above: `require_auth` exempts both paths,
        // because they are what a browser calls in order to authenticate at all. See
        // `mgmt::device_auth`.
        ("POST", "/api/v1/auth/device/challenge", false, false),
        ("POST", "/api/v1/auth/device/token", false, false),
        ("GET", "/api/v1/host", true, true),
        ("GET", "/api/v1/status", true, true),
        ("GET", "/api/v1/local/summary", true, false), // loopback-only, handled before the gates
        ("GET", "/api/v1/compositors", true, true),
        // A client's picker reads the profiles, their pictures, and wakes a seat. Every edit is
        // the console's, and no plugin has a reason to name the people on the box.
        ("GET", "/api/v1/profiles/enumerate", false, true),
        ("GET", "/api/v1/profiles/{id}/avatar", false, true),
        ("POST", "/api/v1/profiles/{id}/wake", false, true),
        ("GET", "/api/v1/profiles", false, false),
        // Turning seats on installs a driver and opens Remote Desktop: the operator's alone.
        ("GET", "/api/v1/profiles/seating", false, false),
        ("PUT", "/api/v1/profiles/seating", false, false),
        ("GET", "/api/v1/profiles/doctor", false, false),
        // Moving the box's host between the owner's session and a system service is root work
        // the operator starts: neither lane.
        ("PUT", "/api/v1/profiles/door", false, false),
        ("POST", "/api/v1/profiles", false, false),
        ("PUT", "/api/v1/profiles/default", false, false),
        ("PUT", "/api/v1/profiles/{id}", false, false),
        ("DELETE", "/api/v1/profiles/{id}", false, false),
        ("PUT", "/api/v1/profiles/{id}/avatar", false, false),
        ("DELETE", "/api/v1/profiles/{id}/avatar", false, false),
        ("POST", "/api/v1/profiles/{id}/start", false, false),
        ("POST", "/api/v1/profiles/{id}/stop", false, false),
        ("POST", "/api/v1/profiles/{id}/end", false, false),
        // Mode + accent of the desktop, for a console that follows it. Operator
        // decoration, so it sits with the other host configuration rather than on
        // the cert lane — no streaming client asks what colour the desktop is.
        ("GET", "/api/v1/host/theme", true, false),
        ("GET", "/api/v1/events", true, false),
        // Unredacted host tracing (webhook URLs, hook command lines). Plugin access would void `/hooks`.
        ("GET", "/api/v1/logs", false, false),
        // Diagnostics name the host user, groups, and device nodes. Both lanes denied.
        ("GET", "/api/v1/diagnostics", false, false),
        ("POST", "/api/v1/diagnostics/refresh", false, false),
        // Upload is the cert lane's single write (size/quota-capped). List/fetch/delete are operator-only.
        ("POST", "/api/v1/client-logs", false, true),
        ("GET", "/api/v1/client-logs", false, false),
        ("GET", "/api/v1/client-logs/{id}", false, false),
        ("DELETE", "/api/v1/client-logs/{id}", false, false),
        // Rosters: plugin-readable, never another paired client. Removal is pairing admin in both lanes.
        ("GET", "/api/v1/clients", true, false),
        // Bulk DELETE shares the GET path; method+path match, so the read grant must not empty the roster.
        ("DELETE", "/api/v1/clients", false, false),
        ("DELETE", "/api/v1/clients/{fingerprint}", false, false),
        // PATCH shares the DELETE path. Labels distinguish Moonlight certs (same subject); setting
        // one is pairing administration, not a roster read.
        ("PATCH", "/api/v1/clients/{fingerprint}", false, false),
        ("GET", "/api/v1/native/clients", true, false),
        ("DELETE", "/api/v1/native/clients", false, false),
        (
            "DELETE",
            "/api/v1/native/clients/{fingerprint}",
            false,
            false,
        ),
        // Grant/expiry edits are pairing administration in both lanes.
        (
            "PATCH",
            "/api/v1/native/clients/{fingerprint}",
            false,
            false,
        ),
        // Pairing administration + PIN visibility: operator token alone.
        ("GET", "/api/v1/pair", false, false),
        ("POST", "/api/v1/pair/pin", false, false),
        ("GET", "/api/v1/native/pair", false, false),
        ("DELETE", "/api/v1/native/pair", false, false),
        ("POST", "/api/v1/native/pair/arm", false, false),
        ("GET", "/api/v1/native/pending", false, false),
        ("POST", "/api/v1/native/pending/{id}/approve", false, false),
        ("POST", "/api/v1/native/pending/{id}/deny", false, false),
        // GPU + display: host configuration, no privilege boundary.
        ("GET", "/api/v1/gpus", true, false),
        ("PUT", "/api/v1/gpus/preference", true, false),
        ("GET", "/api/v1/display/settings", true, false),
        ("PUT", "/api/v1/display/settings", true, false),
        ("GET", "/api/v1/display/state", true, false),
        ("GET", "/api/v1/display/monitors", true, false),
        ("PUT", "/api/v1/display/layout", true, false),
        ("POST", "/api/v1/display/release", true, false),
        ("GET", "/api/v1/display/presets", true, false),
        ("POST", "/api/v1/display/presets", true, false),
        ("PUT", "/api/v1/display/presets/{id}", true, false),
        ("DELETE", "/api/v1/display/presets/{id}", true, false),
        // Per-device display settings. Same lane as the host-wide policy above and
        // deliberately no stricter: this route changes ONE device's behaviour, while
        // `PUT /display/settings` changes every device's. It carries no device identity
        // either — the fingerprint is the key, and the body is display behaviour.
        ("GET", "/api/v1/display/clients/{fingerprint}", true, false),
        ("PUT", "/api/v1/display/clients/{fingerprint}", true, false),
        (
            "DELETE",
            "/api/v1/display/clients/{fingerprint}",
            true,
            false,
        ),
        // Session control. Per-session stop/keyframe/mute ride the host-wide lane; the live
        // access re-point does not — that is access administration.
        ("DELETE", "/api/v1/session", true, false),
        ("POST", "/api/v1/session/idr", true, false),
        ("DELETE", "/api/v1/session/{id}", true, false),
        ("POST", "/api/v1/session/{id}/idr", true, false),
        ("PUT", "/api/v1/session/{id}/audio", true, false),
        ("PUT", "/api/v1/session/{id}/access", false, false),
        // Placing a player writes the device's pairing record, so it is pairing
        // administration too — and a cert caller is not bound to a session id, so it
        // could re-seat another session's controllers.
        ("PUT", "/api/v1/session/{id}/player", false, false),
        // Finished sessions: the same facts `/status` already shows a plugin about a live
        // one. Not the cert lane — it names every other client that streamed here.
        ("GET", "/api/v1/session/last", true, false),
        // Live pad feed: console lane only. A cert caller is not bound to a session
        // id, so it could watch another session's controller. A plugin has no use
        // for a 250 Hz input tap.
        ("GET", "/api/v1/session/{id}/pads", false, false),
        ("GET", "/api/v1/session/settings", true, false),
        ("PUT", "/api/v1/session/settings", true, false),
        // A device ends only games it launched; the handler scopes it.
        ("POST", "/api/v1/game/end", true, true),
        // Library writes are plugin-lane (scanner job); privileged fields inside the payload
        // are refused in the handler — see `plugin_lane_cannot_set_command_execution_fields`.
        ("GET", "/api/v1/library", true, true),
        ("GET", "/api/v1/library/page", true, true),
        ("GET", "/api/v1/library/art/{id}/{kind}", true, true),
        ("GET", "/api/v1/library/scanners", true, false),
        ("PUT", "/api/v1/library/scanners/{id}", true, false),
        // Hide is operator curation; neither lane, unlike the scanner toggle.
        ("PUT", "/api/v1/library/hidden/{id}", false, false),
        ("POST", "/api/v1/library/custom", true, false),
        // The stored row names host paths (`detect`): operator only, like hide.
        ("GET", "/api/v1/library/custom/{id}", false, false),
        ("PUT", "/api/v1/library/custom/{id}", true, false),
        ("DELETE", "/api/v1/library/custom/{id}", true, false),
        ("PUT", "/api/v1/library/provider/{provider}", true, false),
        ("DELETE", "/api/v1/library/provider/{provider}", true, false),
        // A source writes its own result and reads its mode; order, switches and picks are
        // curation, operator-only.
        ("GET", "/api/v1/library/metadata", true, false),
        ("PUT", "/api/v1/library/metadata", false, false),
        ("PUT", "/api/v1/library/metadata/{source}", true, false),
        ("DELETE", "/api/v1/library/metadata/{source}", true, false),
        ("PUT", "/api/v1/library/picks/{id}", false, false),
        // A plugin reports its own downloads; the host maps them through the catalog.
        (
            "PUT",
            "/api/v1/library/provider/{provider}/downloads",
            true,
            false,
        ),
        // Installing is what launching a missing title does; the handler demands the launch
        // grant. Pause and remove demand the manage-games grant; cancel is the operator's.
        ("GET", "/api/v1/downloads", false, false),
        ("POST", "/api/v1/library/install/{id}", false, true),
        ("DELETE", "/api/v1/library/install/{id}", false, true),
        ("POST", "/api/v1/library/install/{id}/pause", false, true),
        ("POST", "/api/v1/library/install/{id}/cancel", false, false),
        // Provider liveness is plugin-lane like reconcile; the host maps through the catalog.
        // Never the cert lane — a streaming client has no titles of its own.
        (
            "PUT",
            "/api/v1/library/provider/{provider}/running",
            true,
            false,
        ),
        // Stats.
        ("POST", "/api/v1/stats/capture/start", true, false),
        ("POST", "/api/v1/stats/capture/stop", true, false),
        ("GET", "/api/v1/stats/capture/status", true, false),
        ("GET", "/api/v1/stats/capture/live", true, false),
        ("GET", "/api/v1/stats/recordings", true, false),
        ("GET", "/api/v1/stats/recordings/{id}", true, false),
        ("DELETE", "/api/v1/stats/recordings/{id}", true, false),
        // Plugins: own directory entry and log ingest, never another plugin's UI secret.
        ("GET", "/api/v1/plugins", true, false),
        ("POST", "/api/v1/plugins/logs", true, false),
        ("PUT", "/api/v1/plugins/{id}", true, false),
        ("DELETE", "/api/v1/plugins/{id}", true, false),
        ("GET", "/api/v1/plugins/{id}/ui-credential", false, false),
        // A plugin parks a connection for its own page; the handler checks the identity is its own.
        ("GET", "/api/v1/plugins/{id}/ui/attach", true, false),
        // A plugin asks for a folder and reads its own rows (its own token, not the shared
        // runner's — the handler 403s without a PluginIdentity). Deciding is operator-only,
        // so the overview and decide routes admit neither lane.
        ("POST", "/api/v1/plugin-access/requests", true, false),
        ("GET", "/api/v1/plugin-access/requests", true, false),
        ("GET", "/api/v1/plugin-access", false, false),
        // A managed emulator's program is what a plugin's launch template points at; installing is
        // the operator's, like every install.
        ("GET", "/api/v1/emulators", true, false),
        ("GET", "/api/v1/emulators/catalog", true, false),
        ("POST", "/api/v1/emulators/{id}/install", false, false),
        ("POST", "/api/v1/emulators/{id}/prepare", true, false),
        ("POST", "/api/v1/emulators/{id}/remove", false, false),
        ("POST", "/api/v1/emulators/{id}/adopt", false, false),
        ("GET", "/api/v1/emulators/{id}/firmware", true, false),
        ("POST", "/api/v1/emulators/{id}/content", true, false),
        ("GET", "/api/v1/emulators/{id}/saves", true, false),
        ("POST", "/api/v1/emulators/{id}/saves/export", true, false),
        ("POST", "/api/v1/emulators/{id}/saves/import", true, false),
        (
            "POST",
            "/api/v1/plugin-access/{plugin}/decide",
            false,
            false,
        ),
        (
            "POST",
            "/api/v1/plugin-access/{plugin}/release",
            false,
            false,
        ),
        // Hooks: write is command execution as the host user; read exposes webhook creds.
        ("GET", "/api/v1/hooks", false, false),
        ("PUT", "/api/v1/hooks", false, false),
        // Store: installing a plugin runs new code with operator privileges.
        ("GET", "/api/v1/store/catalog", false, false),
        ("POST", "/api/v1/store/refresh", false, false),
        ("GET", "/api/v1/store/installed", false, false),
        ("POST", "/api/v1/store/install", false, false),
        ("POST", "/api/v1/store/uninstall", false, false),
        ("GET", "/api/v1/store/jobs", false, false),
        ("GET", "/api/v1/store/jobs/{id}", false, false),
        ("GET", "/api/v1/store/sources", false, false),
        ("PUT", "/api/v1/store/sources/{name}", false, false),
        ("DELETE", "/api/v1/store/sources/{name}", false, false),
        ("GET", "/api/v1/store/runtime", false, false),
        ("POST", "/api/v1/store/runtime", false, false),
        // Updates: `apply` runs an installer / the root helper.
        // Host settings: operator only. A plugin or a device must not reopen GameStream.
        ("GET", "/api/v1/host/settings", false, false),
        ("PATCH", "/api/v1/host/settings", false, false),
        ("GET", "/api/v1/host/audio/apps", false, false),
        ("GET", "/api/v1/update/status", false, false),
        ("POST", "/api/v1/update/check", false, false),
        ("POST", "/api/v1/update/apply", false, false),
        // Host actions: cert lane; handler filters discovery and requires GRANT_POWER on invoke.
        // Plugin token gets neither — power is operator-hook, not a shared-token capability.
        ("GET", "/api/v1/actions", false, true),
        ("POST", "/api/v1/actions/{id}", false, true),
    ];

    /// Substitute a literal for every `{param}` so the gates see a real request path.
    fn concrete(template: &str) -> String {
        template
            .split('/')
            .map(|s| if s.starts_with('{') { "sample" } else { s })
            .collect::<Vec<_>>()
            .join("/")
    }

    // PIN routes exist only in gamestream-featured builds; `cfg!` keeps both sides type-checked.
    let expected: Vec<(&str, &str, bool, bool)> = EXPECTED
        .iter()
        .copied()
        .filter(|(_, p, _, _)| {
            cfg!(feature = "gamestream") || !matches!(*p, "/api/v1/pair" | "/api/v1/pair/pin")
        })
        .collect();
    let doc: serde_json::Value = serde_json::from_str(&openapi_json()).unwrap();
    let mut live: Vec<(String, String)> = Vec::new();
    for (path, ops) in doc["paths"].as_object().unwrap() {
        for method in ops.as_object().unwrap().keys() {
            if matches!(method.as_str(), "get" | "post" | "put" | "delete" | "patch") {
                live.push((method.to_uppercase(), path.clone()));
            }
        }
    }

    // 1. Every live route has a row.
    for (method, path) in &live {
        assert!(
            expected.iter().any(|(m, p, _, _)| m == method && p == path),
            "route {method} {path} has no lane classification — add a row to EXPECTED in this test \
             and decide, deliberately, whether the plugin token and a paired streaming cert may \
             reach it"
        );
    }
    // 2. No stale rows for removed routes.
    for (method, path, _, _) in &expected {
        assert!(
            live.iter().any(|(m, p)| m == method && p == path),
            "EXPECTED lists {method} {path}, which is not in the live route table — remove the row"
        );
    }
    // 3. Gates match the classification on both lanes.
    for (method, path, plugin_ok, cert_ok) in &expected {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        let concrete = concrete(path);
        assert_eq!(
            auth::plugin_may_access(&m, &concrete),
            *plugin_ok,
            "plugin lane: {method} {path} should be {}",
            if *plugin_ok { "reachable" } else { "denied" }
        );
        assert_eq!(
            auth::cert_may_access(&m, &concrete),
            *cert_ok,
            "cert lane: {method} {path} should be {}",
            if *cert_ok { "reachable" } else { "denied" }
        );
    }
}

/// The seat proxy is the operator's: no plugin and no paired device reaches a seat through it.
#[test]
fn the_seat_proxy_is_admin_only() {
    use axum::http::Method;
    for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
        for path in [
            "/api/v1/profiles/kid/proxy/library",
            "/api/v1/profiles/kid/proxy/status",
        ] {
            assert!(
                !auth::plugin_may_access(&method, path),
                "plugin {method} {path}"
            );
            assert!(
                !auth::cert_may_access(&method, path),
                "cert {method} {path}"
            );
        }
    }
}

/// Segment-wise match: a path that merely starts with an allowed one is not swallowed.
/// A prefix deny also covers routes that do not exist yet.
#[test]
fn plugin_allowlist_matches_whole_segments_only() {
    use axum::http::Method;
    // UI credential sits one segment below an allowed route and must stay denied.
    assert!(auth::plugin_may_access(
        &Method::PUT,
        "/api/v1/plugins/rom-manager"
    ));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/plugins/rom-manager/ui-credential"
    ));
    // Unclassified sub-route of an allowed path stays denied.
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/library/secrets"
    ));
    assert!(!auth::plugin_may_access(
        &Method::POST,
        "/api/v1/session/settings/x"
    ));
    assert!(auth::plugin_may_access(&Method::GET, "/api/v1/clients"));
    assert!(!auth::plugin_may_access(
        &Method::DELETE,
        "/api/v1/clients/aabbcc"
    ));
    // A letter-prefix that is not a segment prefix must not match.
    assert!(!auth::plugin_may_access(&Method::GET, "/api/v1/statuses"));
    assert!(!auth::plugin_may_access(
        &Method::GET,
        "/api/v1/library-secrets"
    ));
    // Whole-prefix: a later store/update route is denied by default.
    for path in [
        "/api/v1/store/some-route-that-does-not-exist-yet",
        "/api/v1/update",
        "/api/v1/update/apply-does-not-exist-yet",
    ] {
        assert!(
            !auth::plugin_may_access(&Method::GET, path),
            "plugin token must not reach {path}"
        );
        assert!(
            !auth::plugin_may_access(&Method::POST, path),
            "plugin token must not reach {path}"
        );
        assert!(
            !auth::cert_may_access(&Method::GET, path),
            "a paired streaming cert must not reach {path}"
        );
    }
}

/// Unique operationIds (codegen) and a current checked-in snapshot. `api/openapi.json` is
/// the default-features document; a native-only spec (no PIN routes) is intentionally not checked in.
#[cfg(feature = "gamestream")]
#[test]
fn openapi_document_is_complete_and_checked_in() {
    let json = openapi_json();
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();

    let paths = doc["paths"].as_object().unwrap();
    for p in [
        "/api/v1/health",
        "/api/v1/host",
        "/api/v1/status",
        "/api/v1/clients",
        "/api/v1/clients/{fingerprint}",
        "/api/v1/pair",
        "/api/v1/pair/pin",
        "/api/v1/session",
        "/api/v1/session/idr",
    ] {
        assert!(paths.contains_key(p), "spec is missing {p}");
    }

    let mut op_ids: Vec<&str> = paths
        .values()
        .flat_map(|ops| ops.as_object().unwrap().values())
        .filter_map(|op| op["operationId"].as_str())
        .collect();
    let total = op_ids.len();
    op_ids.sort_unstable();
    op_ids.dedup();
    assert_eq!(total, op_ids.len(), "duplicate operationIds");
    assert!(doc["components"]["securitySchemes"]["bearerAuth"].is_object());
    // Health overrides the document-global bearer; the spec must match `require_auth`.
    assert_eq!(
        doc["paths"]["/api/v1/health"]["get"]["security"],
        serde_json::json!([{}])
    );

    let checked_in = include_str!("../../../../../api/openapi.json");
    // Structural compare with `info.version` normalized: a version bump must not fail the snapshot.
    // JSON compare also ignores CRLF checkouts on Windows.
    let mut generated = doc;
    let mut snapshot: serde_json::Value = serde_json::from_str(checked_in).unwrap();
    generated["info"]["version"] = serde_json::json!("<any>");
    snapshot["info"]["version"] = serde_json::json!("<any>");
    assert_eq!(
        generated, snapshot,
        "api/openapi.json is stale — regenerate with: \
         cargo run -p punktfunk-host -- openapi > api/openapi.json"
    );
}
