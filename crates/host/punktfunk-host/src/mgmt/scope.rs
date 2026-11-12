//! Whose each management route is: the box's, one host process's, or a profile's. One row per
//! operation of the API document, spelled as `api/openapi.json` spells it, and the document
//! carries each row as `x-scope`.
//!
//! The seat proxy refuses what is the box's; a seat on a person's own Linux account refuses a
//! write to what runs code as them. `mgmt::tests::routes` pins the table against the document.
//! The door's `DOOR` and `DEVICE` tables answer another question, which host answers, and stay
//! apart. Decision: `docs/adr/0003-scope-table.md`.

use axum::http::Method;
use punktfunk_core::mgmt_routes::path_matches;

/// Whose a route is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The machine's: exists once, the operator's alone. Never reached through a seat.
    Box,
    /// One host process's own report or switch (status, logs, diagnostics, encoder facts). The
    /// operator's on every host, a seat's through the proxy. Never a person's.
    Host,
    /// A profile's own data, on the host it plays on: runs nothing.
    ProfileData,
    /// Runs code as the profile's account, or decides what does.
    ProfileExec,
}

impl Scope {
    /// The document's `x-scope` value.
    pub(crate) fn wire(self) -> &'static str {
        match self {
            Scope::Box => "box",
            Scope::Host => "host",
            Scope::ProfileData => "profile-data",
            Scope::ProfileExec => "profile-exec",
        }
    }
}

/// `(method, path, scope)`. An operation with no row fails `mgmt::tests::routes`.
pub(crate) const SCOPES: &[(&str, &str, Scope)] = &[
    ("GET", "/api/v1/actions", Scope::Box),
    ("POST", "/api/v1/actions/{id}", Scope::Box),
    // A device proving its pairing to the box.
    ("POST", "/api/v1/auth/device/challenge", Scope::Box),
    ("POST", "/api/v1/auth/device/token", Scope::Box),
    // The bundles devices send the box.
    ("GET", "/api/v1/client-logs", Scope::Box),
    ("POST", "/api/v1/client-logs", Scope::Box),
    ("GET", "/api/v1/client-logs/{id}", Scope::Box),
    ("DELETE", "/api/v1/client-logs/{id}", Scope::Box),
    ("GET", "/api/v1/clients", Scope::Box),
    ("DELETE", "/api/v1/clients", Scope::Box),
    ("DELETE", "/api/v1/clients/{fingerprint}", Scope::Box),
    ("PATCH", "/api/v1/clients/{fingerprint}", Scope::Box),
    ("GET", "/api/v1/compositors", Scope::Host),
    ("GET", "/api/v1/diagnostics", Scope::Host),
    ("POST", "/api/v1/diagnostics/refresh", Scope::Host),
    // A device's own row on the host it streams from.
    (
        "GET",
        "/api/v1/display/clients/{fingerprint}",
        Scope::ProfileData,
    ),
    (
        "PUT",
        "/api/v1/display/clients/{fingerprint}",
        Scope::ProfileData,
    ),
    (
        "DELETE",
        "/api/v1/display/clients/{fingerprint}",
        Scope::ProfileData,
    ),
    ("PUT", "/api/v1/display/layout", Scope::Box),
    ("GET", "/api/v1/display/monitors", Scope::Box),
    ("GET", "/api/v1/display/presets", Scope::Box),
    ("POST", "/api/v1/display/presets", Scope::Box),
    ("PUT", "/api/v1/display/presets/{id}", Scope::Box),
    ("DELETE", "/api/v1/display/presets/{id}", Scope::Box),
    ("POST", "/api/v1/display/release", Scope::Box),
    ("GET", "/api/v1/display/settings", Scope::Box),
    ("PUT", "/api/v1/display/settings", Scope::Box),
    // What this host's displays show: the console reads a seat's through the proxy.
    ("GET", "/api/v1/display/state", Scope::Host),
    ("GET", "/api/v1/display/targets", Scope::ProfileData),
    ("GET", "/api/v1/downloads", Scope::ProfileData),
    ("GET", "/api/v1/emulators", Scope::ProfileData),
    ("GET", "/api/v1/emulators/catalog", Scope::ProfileData),
    ("POST", "/api/v1/emulators/{id}/adopt", Scope::ProfileExec),
    ("POST", "/api/v1/emulators/{id}/content", Scope::ProfileExec),
    ("GET", "/api/v1/emulators/{id}/firmware", Scope::ProfileData),
    ("POST", "/api/v1/emulators/{id}/install", Scope::ProfileExec),
    ("POST", "/api/v1/emulators/{id}/prepare", Scope::ProfileExec),
    ("POST", "/api/v1/emulators/{id}/remove", Scope::ProfileExec),
    ("GET", "/api/v1/emulators/{id}/saves", Scope::ProfileData),
    // Moves files in the account's emulator folders, as every emulator write does.
    (
        "POST",
        "/api/v1/emulators/{id}/saves/export",
        Scope::ProfileExec,
    ),
    (
        "POST",
        "/api/v1/emulators/{id}/saves/import",
        Scope::ProfileExec,
    ),
    ("GET", "/api/v1/events", Scope::Host),
    ("POST", "/api/v1/events/relay", Scope::Host),
    ("POST", "/api/v1/game/end", Scope::ProfileData),
    // A held launch waits on the plugins that release it.
    (
        "POST",
        "/api/v1/game/holds/{token}/release",
        Scope::ProfileExec,
    ),
    ("GET", "/api/v1/gpus", Scope::Host),
    ("PUT", "/api/v1/gpus/preference", Scope::Box),
    ("GET", "/api/v1/health", Scope::Host),
    ("GET", "/api/v1/hooks", Scope::ProfileExec),
    ("PUT", "/api/v1/hooks", Scope::ProfileExec),
    ("GET", "/api/v1/host", Scope::Host),
    ("GET", "/api/v1/host/audio/apps", Scope::Host),
    // The box's file on the box, a seat's own on a seat.
    ("GET", "/api/v1/host/settings", Scope::Host),
    ("PATCH", "/api/v1/host/settings", Scope::Host),
    ("GET", "/api/v1/host/theme", Scope::Host),
    ("GET", "/api/v1/library", Scope::ProfileData),
    ("GET", "/api/v1/library/art/{id}/{kind}", Scope::ProfileData),
    ("POST", "/api/v1/library/custom", Scope::ProfileData),
    ("GET", "/api/v1/library/custom/{id}", Scope::ProfileData),
    ("PUT", "/api/v1/library/custom/{id}", Scope::ProfileData),
    ("DELETE", "/api/v1/library/custom/{id}", Scope::ProfileData),
    ("PUT", "/api/v1/library/hidden/{id}", Scope::ProfileData),
    ("POST", "/api/v1/library/install/{id}", Scope::ProfileData),
    ("DELETE", "/api/v1/library/install/{id}", Scope::ProfileData),
    (
        "POST",
        "/api/v1/library/install/{id}/cancel",
        Scope::ProfileData,
    ),
    (
        "POST",
        "/api/v1/library/install/{id}/pause",
        Scope::ProfileData,
    ),
    ("GET", "/api/v1/library/metadata", Scope::ProfileData),
    ("PUT", "/api/v1/library/metadata", Scope::ProfileData),
    (
        "PUT",
        "/api/v1/library/metadata/{source}",
        Scope::ProfileData,
    ),
    (
        "DELETE",
        "/api/v1/library/metadata/{source}",
        Scope::ProfileData,
    ),
    ("GET", "/api/v1/library/overrides", Scope::ProfileData),
    (
        "PUT",
        "/api/v1/library/overrides/games/{id}",
        Scope::ProfileData,
    ),
    ("POST", "/api/v1/library/overrides/sets", Scope::ProfileData),
    (
        "PUT",
        "/api/v1/library/overrides/sets/{id}",
        Scope::ProfileData,
    ),
    (
        "DELETE",
        "/api/v1/library/overrides/sets/{id}",
        Scope::ProfileData,
    ),
    (
        "PUT",
        "/api/v1/library/overrides/sources/{source}",
        Scope::ProfileData,
    ),
    ("GET", "/api/v1/library/page", Scope::ProfileData),
    ("PUT", "/api/v1/library/picks/{id}", Scope::ProfileData),
    // What a plugin publishes: its titles launch through its manifest.
    (
        "PUT",
        "/api/v1/library/provider/{provider}",
        Scope::ProfileExec,
    ),
    (
        "DELETE",
        "/api/v1/library/provider/{provider}",
        Scope::ProfileExec,
    ),
    (
        "PUT",
        "/api/v1/library/provider/{provider}/downloads",
        Scope::ProfileExec,
    ),
    (
        "PUT",
        "/api/v1/library/provider/{provider}/running",
        Scope::ProfileExec,
    ),
    ("GET", "/api/v1/library/scanners", Scope::ProfileData),
    ("PUT", "/api/v1/library/scanners/{id}", Scope::ProfileData),
    ("GET", "/api/v1/local/summary", Scope::Host),
    ("GET", "/api/v1/logs", Scope::Host),
    ("GET", "/api/v1/native/clients", Scope::Box),
    ("DELETE", "/api/v1/native/clients", Scope::Box),
    ("DELETE", "/api/v1/native/clients/{fingerprint}", Scope::Box),
    ("PATCH", "/api/v1/native/clients/{fingerprint}", Scope::Box),
    ("GET", "/api/v1/native/pair", Scope::Box),
    ("DELETE", "/api/v1/native/pair", Scope::Box),
    ("POST", "/api/v1/native/pair/arm", Scope::Box),
    ("GET", "/api/v1/native/pending", Scope::Box),
    ("POST", "/api/v1/native/pending/{id}/approve", Scope::Box),
    ("POST", "/api/v1/native/pending/{id}/deny", Scope::Box),
    ("GET", "/api/v1/pair", Scope::Box),
    ("POST", "/api/v1/pair/pin", Scope::Box),
    ("GET", "/api/v1/plugin-access", Scope::ProfileExec),
    ("GET", "/api/v1/plugin-access/requests", Scope::ProfileExec),
    ("POST", "/api/v1/plugin-access/requests", Scope::ProfileExec),
    (
        "POST",
        "/api/v1/plugin-access/{plugin}/decide",
        Scope::ProfileExec,
    ),
    (
        "POST",
        "/api/v1/plugin-access/{plugin}/release",
        Scope::ProfileExec,
    ),
    ("GET", "/api/v1/plugins", Scope::ProfileExec),
    ("POST", "/api/v1/plugins/logs", Scope::ProfileExec),
    ("PUT", "/api/v1/plugins/{id}", Scope::ProfileExec),
    ("DELETE", "/api/v1/plugins/{id}", Scope::ProfileExec),
    (
        "GET",
        "/api/v1/plugins/{id}/ui-credential",
        Scope::ProfileExec,
    ),
    ("GET", "/api/v1/plugins/{id}/ui/attach", Scope::ProfileExec),
    ("GET", "/api/v1/profiles", Scope::Box),
    ("POST", "/api/v1/profiles", Scope::Box),
    ("PUT", "/api/v1/profiles/default", Scope::Box),
    ("GET", "/api/v1/profiles/doctor", Scope::Box),
    ("PUT", "/api/v1/profiles/door", Scope::Box),
    ("GET", "/api/v1/profiles/enumerate", Scope::Box),
    ("GET", "/api/v1/profiles/seating", Scope::Box),
    ("PUT", "/api/v1/profiles/seating", Scope::Box),
    ("GET", "/api/v1/profiles/windows-accounts", Scope::Box),
    ("PUT", "/api/v1/profiles/{id}", Scope::Box),
    ("DELETE", "/api/v1/profiles/{id}", Scope::Box),
    ("GET", "/api/v1/profiles/{id}/avatar", Scope::Box),
    ("PUT", "/api/v1/profiles/{id}/avatar", Scope::Box),
    ("DELETE", "/api/v1/profiles/{id}/avatar", Scope::Box),
    ("GET", "/api/v1/profiles/{id}/devices", Scope::Box),
    ("DELETE", "/api/v1/profiles/{id}/devices", Scope::Box),
    (
        "DELETE",
        "/api/v1/profiles/{id}/devices/{fingerprint}",
        Scope::Box,
    ),
    ("POST", "/api/v1/profiles/{id}/end", Scope::Box),
    ("POST", "/api/v1/profiles/{id}/start", Scope::Box),
    ("POST", "/api/v1/profiles/{id}/stop", Scope::Box),
    ("POST", "/api/v1/profiles/{id}/unlock-challenge", Scope::Box),
    ("POST", "/api/v1/profiles/{id}/wake", Scope::Box),
    // A profile's own session preferences. Before `session/{id}`, which `settings` also fits: the
    // first row that fits wins, as the router's literal segment does.
    ("DELETE", "/api/v1/session/settings", Scope::ProfileData),
    // This host's live sessions: the console steers a seat's through the proxy.
    ("DELETE", "/api/v1/session", Scope::Host),
    ("POST", "/api/v1/session/idr", Scope::Host),
    ("DELETE", "/api/v1/session/{id}", Scope::Host),
    ("PUT", "/api/v1/session/{id}/access", Scope::Host),
    ("PUT", "/api/v1/session/{id}/audio", Scope::Host),
    ("POST", "/api/v1/session/{id}/idr", Scope::Host),
    ("GET", "/api/v1/session/{id}/pads", Scope::Host),
    ("PUT", "/api/v1/session/{id}/player", Scope::Host),
    // A profile's recents and session preferences.
    ("GET", "/api/v1/session/last", Scope::ProfileData),
    ("GET", "/api/v1/session/settings", Scope::ProfileData),
    ("PUT", "/api/v1/session/settings", Scope::ProfileData),
    // A capture records every session on this host.
    ("GET", "/api/v1/stats/capture/live", Scope::Host),
    ("POST", "/api/v1/stats/capture/start", Scope::Host),
    ("GET", "/api/v1/stats/capture/status", Scope::Host),
    ("POST", "/api/v1/stats/capture/stop", Scope::Host),
    ("GET", "/api/v1/stats/recordings", Scope::Host),
    ("GET", "/api/v1/stats/recordings/{id}", Scope::Host),
    ("DELETE", "/api/v1/stats/recordings/{id}", Scope::Host),
    ("GET", "/api/v1/status", Scope::Host),
    ("GET", "/api/v1/store/catalog", Scope::ProfileExec),
    ("POST", "/api/v1/store/install", Scope::ProfileExec),
    ("GET", "/api/v1/store/installed", Scope::ProfileExec),
    ("GET", "/api/v1/store/jobs", Scope::ProfileExec),
    ("GET", "/api/v1/store/jobs/{id}", Scope::ProfileExec),
    ("POST", "/api/v1/store/refresh", Scope::ProfileExec),
    ("GET", "/api/v1/store/runtime", Scope::ProfileExec),
    ("POST", "/api/v1/store/runtime", Scope::ProfileExec),
    ("GET", "/api/v1/store/sources", Scope::ProfileExec),
    ("PUT", "/api/v1/store/sources/{name}", Scope::ProfileExec),
    ("DELETE", "/api/v1/store/sources/{name}", Scope::ProfileExec),
    ("POST", "/api/v1/store/uninstall", Scope::ProfileExec),
    ("POST", "/api/v1/update/apply", Scope::Box),
    ("POST", "/api/v1/update/check", Scope::Box),
    ("GET", "/api/v1/update/status", Scope::Box),
    // The browser plane's certificate hash.
    ("GET", "/api/v1/webtransport", Scope::Box),
];

/// The row `method` on `path` (`/api/v1/…`) runs, read as the router reads it: each segment
/// literal, a dot one too, and `HEAD` the `GET` route's.
pub(crate) fn row_of(method: &Method, path: &str) -> Option<Scope> {
    let method = if *method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    SCOPES
        .iter()
        .find(|(m, p, _)| *m == method && path_matches(p, path))
        .map(|&(.., scope)| scope)
}

/// [`row_of`], except that a path with a `.` or `..` segment, however escaped, has no scope: a
/// forward resolves it to another route.
pub(crate) fn scope_of(method: &Method, path: &str) -> Option<Scope> {
    let dot = path.split('/').any(|s| {
        let s = s.to_ascii_lowercase().replace("%2e", ".");
        s == "." || s == ".."
    });
    if dot {
        None
    } else {
        row_of(method, path)
    }
}

/// Writes each operation's row into `api` as `x-scope`. An operation with no row stays bare,
/// which `mgmt::tests::routes` fails.
pub(crate) fn stamp(api: &mut utoipa::openapi::OpenApi) {
    for (path, item) in api.paths.paths.iter_mut() {
        let ops = [
            ("GET", &mut item.get),
            ("PUT", &mut item.put),
            ("POST", &mut item.post),
            ("DELETE", &mut item.delete),
            ("PATCH", &mut item.patch),
        ];
        for (method, op) in ops {
            let row = SCOPES.iter().find(|(m, p, _)| *m == method && p == path);
            if let (Some(op), Some(&(.., scope))) = (op.as_mut(), row) {
                op.extensions
                    .get_or_insert_with(Default::default)
                    .insert("x-scope".into(), scope.wire().into());
            }
        }
    }
}
