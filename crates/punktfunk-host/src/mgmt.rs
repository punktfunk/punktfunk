//! Management REST API — control-plane surface for a console or CLI.
//!
//! Host identity and capabilities, runtime status, paired-client management,
//! pairing PIN, and session control. `tokio`/`axum` live here; the per-frame
//! pipeline never enters this module.
//!
//! Versioned under `/api/v1`. `utoipa` generates OpenAPI 3.1 at compile time:
//! `punktfunk-host openapi` prints it, the server serves `/api/v1/openapi.json`
//! and `/api/docs`, and `api/openapi.json` is the checked-in copy (a test fails
//! on drift).
//!
//! HTTPS on the host identity cert. Auth is [`auth`]: paired-cert (LAN) or
//! bearer (loopback). `/health` is open; OpenAPI and `/api/docs` are
//! unauthenticated (the spec is in-tree). Default bind is all interfaces;
//! `--mgmt-bind 127.0.0.1:47990` restores loopback-only.

use crate::gamestream::{tls::serve_https, AppState};
use anyhow::{Context, Result};
use axum::{middleware, routing::get, Json, Router};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use utoipa::{Modify, OpenApi};
use utoipa_axum::{router::OpenApiRouter, routes};
use utoipa_scalar::{Scalar, Servable};

mod actions;
mod auth;
mod client_logs;
mod clients;
mod cors;
mod device_auth;
mod diagnostics;
mod display;
mod events;
mod gpu;
mod hooks;
mod host;
mod library;
mod native;
mod plugin_access;
mod plugins;
mod session;
mod settings;
mod shared;
mod stats;
mod store;
#[cfg(test)]
mod tests;
mod theme;
mod update;
mod webtransport;

/// Default management port — next to the GameStream block (47984…48010).
///
/// Sunshine's web UI is also 47990, so a Sunshine fork and a GameStream-off
/// host collide here. Move it via [`publish_endpoint`] / `PUNKTFUNK_MGMT_BIND`;
/// consumers read the bound port rather than assuming this constant.
pub const DEFAULT_PORT: u16 = 47990;

/// Filename [`publish_endpoint`] writes next to `mgmt-token`.
const ENDPOINT_FILE: &str = "mgmt-endpoint";

/// Bound port, recorded once by [`publish_endpoint`].
static EFFECTIVE_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

/// Bound mgmt port, or `0` when this process has no management API.
///
/// The native handshake puts this in every `Welcome` so a client learns it
/// on the already-authenticated connection. Same value as the endpoint file.
/// `0` is required — advertising [`DEFAULT_PORT`] with no listener is worse
/// than omitting the port.
pub fn effective_port() -> u16 {
    EFFECTIVE_PORT.get().copied().unwrap_or(0)
}

/// Write the bound loopback URL to `<config-dir>/mgmt-endpoint`.
///
/// `KEY=VALUE` so the console can `EnvironmentFile` it (Windows:
/// `read_env_file_value`). The host is the source of truth for the port;
/// consumers keep a 47990 fallback for an older host. Always loopback —
/// a `0.0.0.0` bind must not become a LAN URL (bearer admin is loopback-only).
/// Best-effort: a write failure must not stop the host from serving.
pub fn publish_endpoint(bind: SocketAddr) {
    // Set [`effective_port`] before the write: a failed file write must not
    // also drop the in-band Welcome port.
    let _ = EFFECTIVE_PORT.set(bind.port());
    let dir = pf_paths::config_dir();
    if let Err(e) = pf_paths::create_private_dir(&dir) {
        tracing::warn!(error = %e, "config dir for the mgmt endpoint not created");
        return;
    }
    match write_endpoint(&dir, bind.port()) {
        Ok(path) => {
            tracing::debug!(path = %path.display(), port = bind.port(), "published mgmt endpoint")
        }
        Err(e) => tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "mgmt endpoint not published — a console on another port will fall back to 47990"
        ),
    }
}

/// IO half of [`publish_endpoint`]. Directory is injected so tests skip
/// `PUNKTFUNK_CONFIG_DIR`.
///
/// Not `pf_paths::write_secret_file`: the port is already in mDNS TXT, and
/// a SYSTEM/Administrators DACL would hide it from a user-session console.
/// The 0700 config dir is the access control.
fn write_endpoint(dir: &std::path::Path, port: u16) -> std::io::Result<std::path::PathBuf> {
    let path = dir.join(ENDPOINT_FILE);
    // Write-then-rename: systemd may source this mid-rewrite. A torn read
    // yields empty `PUNKTFUNK_MGMT_URL`; a set-but-blank var is not the
    // built-in default. `rename` is atomic on Unix and replace on Windows.
    let tmp = dir.join(format!("{ENDPOINT_FILE}.tmp"));
    std::fs::write(&tmp, endpoint_line(port))?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// One `KEY=VALUE` line: systemd `EnvironmentFile` and
/// `windows::service::read_env_file_value`. No quoting; a URL has no `=`.
fn endpoint_line(port: u16) -> String {
    format!("PUNKTFUNK_MGMT_URL=https://127.0.0.1:{port}\n")
}

#[derive(Clone, Debug)]
pub struct Options {
    pub bind: SocketAddr,
    /// Bearer on `/api/v1` except `/health`. `None` is unauthenticated.
    pub token: Option<String>,
    /// Scripting-runner bearer: [`auth::plugin_may_access`] only. `None` disables the lane.
    pub plugin_token: Option<String>,
    /// Per-plugin bearers by plugin id — the same route set, plus an identity the handlers use
    /// to refuse a plugin writing another plugin's registration.
    pub plugin_tokens: std::collections::BTreeMap<String, String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            token: None,
            plugin_token: None,
            plugin_tokens: std::collections::BTreeMap::new(),
        }
    }
}

pub(crate) struct MgmtState {
    app: Arc<AppState>,
    /// Native pairing, shared with the QUIC host. `None` ⇒ GameStream-only
    /// (`enabled: false` on the native endpoints).
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    /// Same recorder the streaming loops emit into.
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    /// Uploaded log bundles. Mgmt-only; nothing streams in.
    client_logs: Arc<crate::client_logs::ClientLogStore>,
    /// `--gamestream`. [`HostInfo`] hides Moonlight pairing when this is off.
    gamestream_enabled: bool,
    token: Option<String>,
    /// Plugin-lane token ([`Options::plugin_token`]). Checked after the admin
    /// token misses; gated per route by `auth::plugin_may_access`.
    plugin_token: Option<String>,
    /// Per-plugin tokens by id ([`Options::plugin_tokens`]). A match also stamps
    /// [`auth::PluginIdentity`], which is what the id-scoped routes check.
    pub(crate) plugin_tokens: PluginTokens,
    /// Folder grants, denials, and pending requests — the `/plugin-access` routes' store.
    access: Arc<crate::plugins::access::AccessStore>,
    /// Where `plugin-run/` lives: auth re-reads the token file there on a miss.
    config_dir: std::path::PathBuf,
    /// Bound port, echoed in [`PortMap`].
    port: u16,
    /// Live device challenges and the tokens they became. See [`mgmt::device_auth`].
    device_auth: device_auth::DeviceAuth,
    /// SHA-256 of this host's own leaf certificate — the channel binding a device signs, and the
    /// fingerprint a paired browser stored. `None` only if the identity could not be parsed, in
    /// which case the device lane refuses rather than binding to nothing.
    identity_fingerprint: Option<[u8; 32]>,
}

pub(crate) type PluginTokens = Arc<RwLock<std::collections::BTreeMap<String, String>>>;

/// `native` is the shared punktfunk/1 pairing handle when the unified host
/// runs the native QUIC server.
pub async fn run(
    state: Arc<AppState>,
    opts: Options,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    gamestream_enabled: bool,
    // Same identity as native QUIC — paired clients pin one fingerprint for
    // both. The caller resolves once and hands it to both.
    identity: crate::identity::NativeIdentity,
    // Whether the browser plane is running, which is the only reason to answer
    // a cross-origin call at all. See `mgmt::cors`.
    browser_plane: bool,
) -> Result<()> {
    // Close a leftover apply-intent from the previous boot (`update/jobs.rs`).
    // Once per process, before serving.
    crate::update::reconcile_at_boot();
    // The tray has no supervisor — HKLM `Run` is a sign-in trigger — so
    // `StopTrays` and a crash leave no icon until the next logon.
    #[cfg(target_os = "windows")]
    crate::tray::supervise();

    // HTTPS + token always, including loopback. `parse_serve` always supplies
    // one. Blank is none — fail rather than serve unauthenticated.
    let token = opts
        .token
        .filter(|t| !t.trim().is_empty())
        .context("management API has no token — internal error: parse_serve must provide one")?;
    // Native identity (the cert clients already pin). Client cert optional:
    // a paired client presents a fingerprint; a browser presents none and
    // uses the bearer. See `require_auth`.
    let tls = crate::gamestream::tls::server_config_optional_client(
        &identity.cert_pem,
        &identity.key_pem,
    )
    .context("management API TLS config")?;
    tracing::info!(
        addr = %opts.bind,
        auth = "mTLS (paired cert) or bearer (required)",
        "management API listening over HTTPS (docs at /api/docs, spec at /api/v1/openapi.json)"
    );
    // The channel binding a device signs: this host's own leaf, which is the fingerprint a
    // browser stored when it paired. Parsed once here rather than per request.
    let identity_fingerprint = x509_parser::pem::parse_x509_pem(identity.cert_pem.as_bytes())
        .ok()
        .map(|(_, pem)| crate::webtransport::sha256(&pem.contents));
    if identity_fingerprint.is_none() {
        tracing::warn!("the host identity did not parse — browsers cannot use the device lane");
    }
    let app = app(
        state,
        Some(token),
        opts.plugin_token.filter(|t| !t.trim().is_empty()),
        Arc::new(RwLock::new(opts.plugin_tokens)),
        opts.bind.port(),
        native,
        stats,
        crate::client_logs::default_dir(),
        pf_paths::config_dir(),
        gamestream_enabled,
        identity_fingerprint,
        browser_plane,
    );
    serve_https(opts.bind, app, tls).await
}

/// Handler tests call this directly (not only [`run`]).
#[allow(clippy::too_many_arguments)] // composition root: a param per `MgmtState` field, plus CORS
fn app(
    state: Arc<AppState>,
    token: Option<String>,
    plugin_token: Option<String>,
    plugin_tokens: PluginTokens,
    port: u16,
    native: Option<Arc<crate::native_pairing::NativePairing>>,
    stats: Arc<crate::stats_recorder::StatsRecorder>,
    // Injected so handler tests use a temp dir, not the real config dir.
    client_logs_dir: std::path::PathBuf,
    // Root for `plugin-run/` and `plugin-access-pending.json`; test-injected.
    config_dir: std::path::PathBuf,
    gamestream_enabled: bool,
    identity_fingerprint: Option<[u8; 32]>,
    // Whether the WebTransport plane is running. State only for `cors::enabled`, so it is not
    // on `MgmtState` — no handler asks.
    browser_plane: bool,
) -> Router {
    let shared = Arc::new(MgmtState {
        app: state,
        native,
        stats,
        client_logs: crate::client_logs::ClientLogStore::new(client_logs_dir),
        gamestream_enabled,
        token,
        plugin_token,
        plugin_tokens,
        port,
        device_auth: device_auth::DeviceAuth::default(),
        identity_fingerprint,
        access: Arc::new(crate::plugins::access::AccessStore::open(
            config_dir.clone(),
        )),
        config_dir,
    });
    let (api_routes, api) = api_router_parts();
    let routed = api_routes.route_layer(middleware::from_fn_with_state(
        shared.clone(),
        auth::require_auth,
    ));
    // Outside the auth gate, because a CORS preflight carries no credential and must be
    // answered rather than refused. Absent entirely on a host that serves no browsers, so a
    // plane nobody enabled cannot widen what a page may read. See `mgmt::cors`.
    let routed = if cors::enabled(
        browser_plane,
        &pf_host_config::config().webtransport_origins,
    ) {
        routed.layer(middleware::from_fn(cors::cors))
    } else {
        routed
    };
    routed
        .with_state(shared)
        .merge(Scalar::with_url("/api/docs", api.clone()))
        .route(
            "/api/v1/openapi.json",
            get(move || {
                let spec = api.clone();
                async move { Json(spec) }
            }),
        )
}

/// Shared by the live server and the `openapi` subcommand.
fn api_router_parts() -> (Router<Arc<MgmtState>>, utoipa::openapi::OpenApi) {
    let api_v1 = OpenApiRouter::new()
        .routes(routes!(host::get_health))
        .routes(routes!(host::get_host_info))
        .routes(routes!(host::list_compositors))
        .routes(routes!(theme::get_host_theme))
        .routes(routes!(
            settings::get_host_settings,
            settings::patch_host_settings
        ))
        .routes(routes!(settings::get_playing_apps))
        .routes(routes!(gpu::list_gpus))
        .routes(routes!(gpu::set_gpu_preference))
        .routes(routes!(display::get_display_settings))
        .routes(routes!(display::set_display_settings))
        .routes(routes!(display::get_display_state))
        .routes(routes!(display::get_display_monitors))
        .routes(routes!(display::release_display))
        .routes(routes!(display::set_display_layout))
        .routes(routes!(
            display::list_custom_presets,
            display::create_custom_preset
        ))
        .routes(routes!(
            display::update_custom_preset,
            display::delete_custom_preset
        ))
        // Three methods, one path — one `routes!` (same-path merge).
        .routes(routes!(
            display::get_display_client,
            display::set_display_client,
            display::delete_display_client
        ))
        .routes(routes!(host::get_status))
        .routes(routes!(host::get_local_summary))
        // Two paths → two `routes!`. The macro merges METHODS of one path;
        // two calls that name the same path collide.
        .routes(routes!(diagnostics::get_diagnostics))
        .routes(routes!(diagnostics::refresh_diagnostics))
        // GET and DELETE share `/clients` — one `routes!` (same-path merge).
        .routes(routes!(
            clients::list_paired_clients,
            clients::unpair_all_clients
        ))
        // DELETE and PATCH share `/clients/{fingerprint}` — one `routes!`.
        .routes(routes!(clients::unpair_client, clients::rename_client));
    // GameStream PIN routes exist only with the compat planes — a native-only
    // build's API (and OpenAPI) has no such endpoints.
    #[cfg(feature = "gamestream")]
    let api_v1 = api_v1
        .routes(routes!(clients::get_pairing_status))
        .routes(routes!(clients::submit_pairing_pin));
    let api_v1 = api_v1
        .routes(routes!(webtransport::get_webtransport))
        .routes(routes!(device_auth::post_device_challenge))
        .routes(routes!(device_auth::post_device_token))
        .routes(routes!(native::get_native_pairing))
        .routes(routes!(native::arm_native_pairing))
        .routes(routes!(native::disarm_native_pairing))
        // Same-path pair as `/clients` — one `routes!` for both methods.
        .routes(routes!(
            native::list_native_clients,
            native::unpair_all_native_clients
        ))
        // DELETE and PATCH share `/native/clients/{fingerprint}` — one `routes!`.
        .routes(routes!(
            native::unpair_native_client,
            native::update_native_client_access
        ))
        .routes(routes!(native::list_pending_devices))
        .routes(routes!(native::approve_pending_device))
        .routes(routes!(native::deny_pending_device))
        .routes(routes!(session::stop_session))
        .routes(routes!(session::request_idr))
        // Per-session forms of the same three verbs; the id-less ones above stay host-wide.
        .routes(routes!(session::stop_one_session))
        .routes(routes!(session::request_session_idr))
        .routes(routes!(session::set_session_audio))
        .routes(routes!(session::set_session_access))
        .routes(routes!(session::set_session_player))
        // The registry's other side: what finished sessions came to.
        .routes(routes!(session::get_recent_sessions))
        .routes(routes!(session::stream_session_pads))
        .routes(routes!(
            session::get_session_settings,
            session::set_session_settings
        ))
        .routes(routes!(session::end_game))
        .routes(routes!(library::get_library))
        .routes(routes!(library::list_library_scanners))
        .routes(routes!(library::set_library_scanner))
        .routes(routes!(library::set_library_entry_hidden))
        .routes(routes!(library::create_custom_game))
        .routes(routes!(
            library::get_custom_game,
            library::update_custom_game,
            library::delete_custom_game
        ))
        // A plugin sends its whole set in one PUT; descriptions across a big library outgrow
        // axum's 2 MB default and a 413 fails the entire sync.
        .merge(
            OpenApiRouter::new()
                .routes(routes!(
                    library::reconcile_provider_entries,
                    library::delete_provider_entries
                ))
                .routes(routes!(
                    library::put_library_metadata,
                    library::delete_library_metadata
                ))
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .routes(routes!(
            library::list_library_metadata,
            library::set_library_metadata
        ))
        .routes(routes!(library::set_library_art_pick))
        .routes(routes!(library::report_provider_running))
        .routes(routes!(library::get_library_art))
        .routes(routes!(stats::stats_capture_start))
        .routes(routes!(stats::stats_capture_stop))
        .routes(routes!(stats::stats_capture_status))
        .routes(routes!(stats::stats_capture_live))
        .routes(routes!(stats::stats_recordings_list))
        .routes(routes!(
            stats::stats_recording_get,
            stats::stats_recording_delete
        ))
        .routes(routes!(stats::logs_get))
        .routes(routes!(
            client_logs::client_logs_upload,
            client_logs::client_logs_list
        ))
        .routes(routes!(
            client_logs::client_logs_get,
            client_logs::client_logs_delete
        ))
        .routes(routes!(events::stream_events))
        .routes(routes!(hooks::get_hooks, hooks::set_hooks))
        .routes(routes!(plugins::list_plugins))
        .routes(routes!(plugins::register_plugin, plugins::delete_plugin))
        .routes(routes!(plugins::get_ui_credential))
        .routes(routes!(plugins::ingest_plugin_logs))
        // GET and POST share the path — one `routes!` (same-path merge). The plugin lane
        // reaches these two only; the overview and the decision stay admin by allowlist.
        .routes(routes!(
            plugin_access::get_plugin_access_requests,
            plugin_access::request_plugin_access
        ))
        .routes(routes!(plugin_access::get_plugin_access))
        .routes(routes!(plugin_access::decide_plugin_access))
        .routes(routes!(store::get_catalog))
        .routes(routes!(store::refresh_catalog))
        .routes(routes!(store::list_installed))
        .routes(routes!(store::install_plugin))
        .routes(routes!(store::uninstall_plugin))
        .routes(routes!(store::list_jobs))
        .routes(routes!(store::get_job))
        .routes(routes!(store::list_sources))
        .routes(routes!(store::put_source, store::delete_source))
        .routes(routes!(store::get_runtime, store::set_runtime))
        .routes(routes!(update::get_update_status))
        .routes(routes!(update::force_update_check))
        .routes(routes!(update::apply_update))
        .routes(routes!(actions::list_actions))
        .routes(routes!(actions::invoke_action));
    OpenApiRouter::with_openapi(ApiDoc::openapi())
        .nest("/api/v1", api_v1)
        .split_for_parts()
}

/// `punktfunk-host openapi`; checked in at `api/openapi.json`.
pub fn openapi_json() -> String {
    let (_, api) = api_router_parts();
    let mut json = api.to_pretty_json().expect("serialize OpenAPI document");
    json.push('\n');
    json
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "punktfunk management API",
        description = "Control-plane API for managing a punktfunk streaming host: host \
                       capabilities, runtime status, paired clients, the pairing PIN flow, \
                       and session control. Authentication: HTTP bearer token, enforced on \
                       every route except `/api/v1/health` when the host is started with a \
                       management token (mandatory for non-loopback binds)."
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "host", description = "Host identity, capabilities, and liveness"),
        (name = "diagnostics", description = "Host health checks: what is wrong, what it breaks, and how to fix it (admin lane only)"),
        (name = "gpu", description = "GPU inventory and selection: list the host's GPUs, choose automatic or a preferred GPU, see the one in use"),
        (name = "display", description = "Virtual-display management policy: lifecycle (keep-alive), topology (primary/exclusive), conflict handling, identity, and layout"),
        (name = "clients", description = "Paired Moonlight client management"),
        (name = "pairing", description = "Pairing PIN delivery (the out-of-band half of the GameStream pairing handshake)"),
        (name = "native", description = "Native punktfunk/1 pairing: arm a window, display the host PIN, manage paired devices"),
        (name = "session", description = "Active streaming session control"),
        (name = "library", description = "Game library: the titles each installed library plugin syncs, plus user-curated custom entries"),
        (name = "stats", description = "Streaming performance-stats capture: arm/stop a recording, read the live + saved time-series for graphing"),
        (name = "logs", description = "Host log stream: the newest in-memory log entries, cursor-paged for live following"),
        (name = "events", description = "Host lifecycle events: an SSE stream (client/session/stream lifecycle, pairing, displays, library, host) with Last-Event-ID resume and server-side kind filters"),
        (name = "hooks", description = "Operator hooks: commands and webhooks fired on lifecycle events (fire-and-forget — hooks observe, never veto)"),
        (name = "plugins", description = "Plugin directory: running `punktfunk-plugin-*` processes register a lease and, optionally, a loopback UI the web console proxies and adds to its nav"),
        (name = "plugin-access", description = "Plugin folder access: a plugin requests a directory (its own token), the operator grants or denies it (admin lane only)"),
        (name = "store", description = "Plugin store: browse signed catalogs (verified first-party entries, attributed third-party sources), install/uninstall as tracked jobs, and switch the plugin runner on"),
        (name = "update", description = "Host update check: install kind + channel, the last verified release manifest, and whether a newer host exists (admin lane only)"),
        (name = "actions", description = "Host actions: discover what this host offers (per-caller availability + permission) and invoke one by id — v1: sleep, restart, shut down the machine, gated per device by the Host power grant"),
    )
)]
struct ApiDoc;

/// Registers `bearerAuth` globally (utoipa has no "all operations" shorthand).
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
        openapi
            .components
            .get_or_insert_with(Default::default)
            .add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
            );
        openapi.security = Some(vec![utoipa::openapi::security::SecurityRequirement::new(
            "bearerAuth",
            Vec::<String>::new(),
        )]);
    }
}
