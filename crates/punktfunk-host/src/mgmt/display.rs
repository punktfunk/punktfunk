//! Display-tagged management endpoints: policy, live state, physical monitors, layout, and
//! custom presets.
//!
//! A PUT stores the next-session policy; a running session keeps the display it opened on.
//! `keep_alive: forever` pins until `POST /display/release`. Off Linux, `capture_monitor` is
//! dropped on write — there is no mirror backend.
//!
//! See `design/display-management.md` and `design/per-monitor-portal-capture.md`.

use super::shared::*;

/// Picker row. `fields` is the expansion so the console does not hardcode it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PresetInfo {
    /// `default` | `gaming-rig` | `shared-desktop` | `hotdesk` | `workstation`.
    id: String,
    summary: String,
    /// Same fields a `Custom` policy carries.
    fields: crate::vdisplay::policy::EffectivePolicy,
}

/// Stored policy, preset expansions, effective policy, and which options this build enforces.
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplaySettingsState {
    /// Stored policy, or the built-in default when unconfigured.
    settings: crate::vdisplay::policy::DisplayPolicy,
    /// True once `display-settings.json` exists.
    configured: bool,
    effective: crate::vdisplay::policy::EffectivePolicy,
    presets: Vec<PresetInfo>,
    /// Saved custom presets (`display-presets.json`). Apply via a `Custom` policy of their fields.
    custom_presets: Vec<crate::vdisplay::policy::CustomPreset>,
    /// Names this build acts on (live vs coming-soon). Per-backend nuance is on `/display/state`.
    enforced: Vec<String>,
    /// Overlay fields this build acts on PER DEVICE. A strict subset of
    /// `enforced`: an axis can be host-wide and not yet per-device, and the
    /// console must not offer a device control the host would store and ignore
    /// (`design/web-console-overhaul.md` D1).
    client_enforced: Vec<String>,
    /// Stored per-device overlays, so one fetch paints the device rows.
    /// READ-ONLY here — writes go to `/display/clients/{fingerprint}`, and the
    /// PUT below refuses a body carrying this key.
    clients: std::collections::BTreeMap<String, crate::vdisplay::policy::ClientOverlay>,
}

pub(crate) fn preset_summary(id: &str) -> &'static str {
    match id {
        "default" => "Good for most setups. Reconnects resume quickly, the stream is the whole desktop, and extra viewers each get their own screen.",
        "gaming-rig" => "For a machine with no monitor that you only stream from. The game keeps running when you disconnect, and whoever connects next takes it over.",
        "shared-desktop" => "For a PC you also use in person. Your real monitors are never blanked or left with a leftover display, and extra viewers each get their own screen.",
        "hotdesk" => "One person at a time — roam between your own devices with an instant reconnect. Anyone else is told the box is busy.",
        "workstation" => "Your multi-monitor daily driver. Displays come back exactly where you arranged them, each client keeps its own settings, and the desktop is yours alone.",
        _ => "",
    }
}

/// Whether the AMD driver's control library loads — the EDID lever's only precondition.
///
/// The lever is that driver's connector-emulation call, so the DLL is the honest question; a
/// GPU-vendor check is a proxy for it, and was the console's own gate until this moved here.
fn edid_lock_available() -> bool {
    #[cfg(target_os = "windows")]
    {
        pf_win_display::adl_emul::available()
    }
    #[cfg(not(target_os = "windows"))]
    false
}

/// Whether KWin is the backend this host will drive. Cached like the gamescope probe beside
/// it, and for the same reason: `available()` walks /proc and forks.
#[cfg(target_os = "linux")]
fn kwin_available() -> bool {
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PRESENT
        .get_or_init(|| crate::vdisplay::available().contains(&crate::vdisplay::Compositor::Kwin))
}

/// Can any backend here put a launch on a workspace of its own
/// (`vdisplay::claim_workspace`)? Cached: see [`kwin_available`].
#[cfg(target_os = "linux")]
fn workspace_placement_available() -> bool {
    use crate::vdisplay::Compositor;
    static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PRESENT.get_or_init(|| {
        crate::vdisplay::available()
            .iter()
            .any(|c| matches!(c, Compositor::Hyprland | Compositor::Wlroots))
    })
}

/// Whether a gamescope backend is usable on this host. Cached: see the call site.
fn gamescope_present() -> bool {
    #[cfg(target_os = "linux")]
    {
        static PRESENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *PRESENT.get_or_init(|| {
            crate::vdisplay::available().contains(&crate::vdisplay::Compositor::Gamescope)
        })
    }
    #[cfg(not(target_os = "linux"))]
    false
}

pub(crate) fn display_settings_state() -> DisplaySettingsState {
    use crate::vdisplay::policy::{self, Preset};
    let store = policy::prefs();
    let settings = store.get();
    let configured = store.configured().is_some();
    let presets = [
        ("default", Preset::Default),
        ("gaming-rig", Preset::GamingRig),
        ("shared-desktop", Preset::SharedDesktop),
        ("hotdesk", Preset::Hotdesk),
        ("workstation", Preset::Workstation),
    ]
    .into_iter()
    .filter_map(|(id, p)| {
        policy::preset_fields(p).map(|e| PresetInfo {
            id: id.to_string(),
            summary: preset_summary(id).to_string(),
            fields: e,
        })
    })
    .collect();
    let mut enforced: Vec<String> = vec![
        "keep_alive".into(),
        "topology".into(),
        "mode_conflict".into(),
        "identity".into(),
        "layout".into(),
    ];
    // `game_session: dedicated` routes a launch to its own headless gamescope
    // (`native/compositor.rs`), so without the binary the axis stores and does nothing.
    // Probed once: `available()` forks `gamescope --version` and walks /proc, and an
    // install mid-run is a host restart away either way.
    if gamescope_present() {
        enforced.push("game_session".into());
    }
    // Windows-only: both levers live in the exclusive isolate (`vdisplay/windows/manager.rs`).
    // A field this build cannot act on is not advertised — the console renders `enforced`
    // verbatim, so a name kept here is a dead control there.
    if cfg!(target_os = "windows") {
        enforced.push("ddc_power_off".into());
        enforced.push("pnp_disable_monitors".into());
    }
    if edid_lock_available() {
        enforced.push("edid_lock".into());
    }
    // Linux-only: `capture_monitor` needs the MIRROR backend (`vdisplay::open`). Do not
    // advertise it off Linux — a stored pin would never take effect.
    if cfg!(target_os = "linux") {
        enforced.push("capture_monitor".into());
    }
    // Hyprland and sway only. KWin is a later step, and Mutter/GNOME, gamescope and Windows
    // have no per-output workspace to aim a launch at, so the axis would store and do nothing
    // there — the dead control this gate exists to prevent.
    #[cfg(target_os = "linux")]
    if workspace_placement_available() {
        enforced.push("launch_workspace".into());
    }
    // KWin only. wlroots, Hyprland, Mutter and the Windows CCD isolate all darken every head
    // they find, so the keep-list would store and do nothing there — the dead control this
    // whole gate exists to prevent.
    #[cfg(target_os = "linux")]
    if kwin_available() {
        enforced.push("keep_monitors".into());
    }
    // What acts per device. The rest are stored and served but read inside a backend
    // `create`, which takes no client — advertising one would put a control on the
    // device sheet that this host would store and ignore.
    let mut client_enforced: Vec<String> = vec![
        "keep_alive".into(),
        "mode_conflict".into(),
        "identity".into(),
    ];
    // The cap is applied in the native handshake, before Welcome, so the client is told the
    // mode it actually gets rather than the one it asked for.
    client_enforced.push("max_mode".into());
    // Linux only. Topology: the backend reads it at `create`, where `set_client_identity`
    // has already named the device — the Windows CCD isolate is one topology for the whole
    // managed group, so there is no per-device answer to give. Scale: Mutter mints a fresh
    // EDID serial per session, so it is the backend that cannot remember one on its own.
    if cfg!(target_os = "linux") {
        client_enforced.push("topology".into());
        client_enforced.push("scale".into());
    }
    // Overlays ride their own field, never `settings`. The console PUTs `settings`
    // back whole, and the PUT refuses a body carrying `clients` — so leaving them
    // in here would make every host-wide save fail the moment one device had an
    // overlay. The file keeps them; the wire does not.
    let mut settings = settings;
    let clients = std::mem::take(&mut settings.clients);
    DisplaySettingsState {
        effective: settings.effective(),
        clients,
        client_enforced,
        settings,
        configured,
        presets,
        custom_presets: policy::load_custom_presets(),
        enforced,
    }
}

/// Display-management policy
///
/// Stored policy, preset expansions, and which options this build enforces.
/// See `design/display-management.md`.
#[utoipa::path(
    get,
    path = "/display/settings",
    tag = "display",
    operation_id = "getDisplaySettings",
    responses(
        (status = OK, description = "Stored policy + preset expansions + enforced options", body = DisplaySettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_settings() -> Json<DisplaySettingsState> {
    Json(display_settings_state())
}

/// Set the display-management policy
///
/// Persists (validated + clamped). Applies on the next connect/teardown; a running session keeps
/// the display it opened on. `keep_alive: forever` pins until `POST /display/release`.
#[utoipa::path(
    put,
    path = "/display/settings",
    tag = "display",
    operation_id = "setDisplaySettings",
    request_body = crate::vdisplay::policy::DisplayPolicy,
    responses(
        (status = OK, description = "Policy stored; the new state", body = DisplaySettingsState),
        (status = BAD_REQUEST, description = "Malformed policy body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Policy could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_display_settings(
    ApiJson(policy): ApiJson<crate::vdisplay::policy::DisplayPolicy>,
) -> Response {
    // The overlay map never rides the policy object. A console that fetched
    // `/display/settings`, sat on it, and PUT it back would otherwise revert every
    // per-device change made in between — the class of bug `serverCaptureMonitor()`
    // used to paper over one field at a time.
    if !policy.clients.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Per-device display settings are saved on their own — this request carried them \
             with the host policy.",
        );
    }
    if let Err(e) = write(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the display policy — {e:#}"),
        );
    }
    tracing::info!("management API: display policy updated");
    Json(display_settings_state()).into_response()
}

/// Store a host-wide policy, keeping the stored overlays, then re-aim absolute input at its
/// pin (or clear the anchor) without a restart. The PUT and `display.next` both write here.
///
/// Off Linux there is no mirror backend, so `capture_monitor` is dropped rather than refused:
/// the PUT is whole-object, and a stored pin would reject every later save over a field the
/// operator cannot see.
pub(super) fn write(policy: crate::vdisplay::policy::DisplayPolicy) -> anyhow::Result<()> {
    #[cfg_attr(target_os = "linux", allow(unused_mut))]
    let mut policy = policy;
    #[cfg(not(target_os = "linux"))]
    if let Some(dropped) = policy.capture_monitor.take() {
        tracing::warn!(
            "management API: ignoring capture_monitor={dropped:?} — streaming a chosen physical \
             monitor is Linux-only (no Windows mirror backend); the pin was NOT stored"
        );
    }
    let store = crate::vdisplay::policy::prefs();
    let policy = with_stored_overlays(policy, &store.get());
    store.set(policy)?;
    #[cfg(target_os = "linux")]
    crate::refresh_capture_monitor_anchor("display policy updated");
    Ok(())
}

/// Carry the stored per-device overlays across a host-wide write.
///
/// `clients` never rides the wire — the state this route answers with strips it and the PUT
/// refuses a body that carries it — so a host-wide save always arrives with an empty map. The
/// store replaces the whole policy, so without this every preset click and every axis change
/// would silently revert every device to the host policy.
pub(super) fn with_stored_overlays(
    mut incoming: crate::vdisplay::policy::DisplayPolicy,
    stored: &crate::vdisplay::policy::DisplayPolicy,
) -> crate::vdisplay::policy::DisplayPolicy {
    incoming.clients = stored.clients.clone();
    incoming
}

/// Read one device's overlay
///
/// Absent fields follow the host policy; an unknown device answers with an empty
/// overlay rather than 404 — "follows host" is a real answer, not a missing one.
#[utoipa::path(
    get,
    path = "/display/clients/{fingerprint}",
    tag = "display",
    operation_id = "getDisplayClient",
    params(("fingerprint" = String, Path, description = "Pairing fingerprint (hex)")),
    responses(
        (status = OK, description = "The device's overlay", body = crate::vdisplay::policy::ClientOverlay),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_client(
    Path(fingerprint): Path<String>,
) -> Json<crate::vdisplay::policy::ClientOverlay> {
    let key = fingerprint.trim().to_ascii_lowercase();
    Json(
        crate::vdisplay::policy::prefs()
            .get()
            .clients
            .get(&key)
            .cloned()
            .unwrap_or_default(),
    )
}

/// Save one device's overlay
///
/// The WHOLE overlay: a field absent from the body stops being pinned and the
/// device follows the host again. An overlay that pins nothing is dropped, which
/// is the same as `DELETE`.
#[utoipa::path(
    put,
    path = "/display/clients/{fingerprint}",
    tag = "display",
    operation_id = "setDisplayClient",
    params(("fingerprint" = String, Path, description = "Pairing fingerprint (hex)")),
    request_body = crate::vdisplay::policy::ClientOverlay,
    responses(
        (status = OK, description = "Overlay stored; the new settings state", body = DisplaySettingsState),
        (status = BAD_REQUEST, description = "Empty fingerprint", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Overlay could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_display_client(
    Path(fingerprint): Path<String>,
    ApiJson(overlay): ApiJson<crate::vdisplay::policy::ClientOverlay>,
) -> Response {
    let key = fingerprint.trim().to_ascii_lowercase();
    if key.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "no device named");
    }
    let store = crate::vdisplay::policy::prefs();
    // An empty overlay is "follow the host", which is what an absent key already means. On an
    // UNCONFIGURED host writing one would materialise a whole default policy as a side effect
    // — flipping `configured()` process-wide, adopting a 10 s linger where there was none, and
    // silently retiring the legacy topology env knobs. Nothing to store, so store nothing.
    let overlay = overlay.sanitized();
    if overlay.is_empty() && store.configured().is_none() {
        return Json(display_settings_state()).into_response();
    }
    let mut policy = store.get();
    // `sanitized` drops an overlay that pins nothing, so this covers the reset
    // case too without a second path.
    policy.clients.insert(key.clone(), overlay);
    if let Err(e) = store.set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the display settings for this device — {e:#}"),
        );
    }
    tracing::info!(fingerprint = %key, "management API: per-device display overlay updated");
    Json(display_settings_state()).into_response()
}

/// Make one device follow the host again
#[utoipa::path(
    delete,
    path = "/display/clients/{fingerprint}",
    tag = "display",
    operation_id = "deleteDisplayClient",
    params(("fingerprint" = String, Path, description = "Pairing fingerprint (hex)")),
    responses(
        (status = OK, description = "Overlay cleared; the new settings state", body = DisplaySettingsState),
        (status = INTERNAL_SERVER_ERROR, description = "Overlay could not be cleared", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn delete_display_client(Path(fingerprint): Path<String>) -> Response {
    let key = fingerprint.trim().to_ascii_lowercase();
    let store = crate::vdisplay::policy::prefs();
    let mut policy = store.get();
    // Already following the host: nothing to write, and a no-op write would
    // rewrite the file on every unpair.
    if policy.clients.remove(&key).is_none() {
        return Json(display_settings_state()).into_response();
    }
    if let Err(e) = store.set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't clear the display settings for this device — {e:#}"),
        );
    }
    tracing::info!(fingerprint = %key, "management API: per-device display overlay cleared");
    Json(display_settings_state()).into_response()
}

/// Drop a device's overlay when it unpairs, so a later device that is granted the
/// same fingerprint cannot inherit settings the operator made for another one.
pub(crate) fn forget_display_overlay(fingerprint: &str) {
    let key = fingerprint.trim().to_ascii_lowercase();
    let store = crate::vdisplay::policy::prefs();
    let mut policy = store.get();
    if policy.clients.remove(&key).is_none() {
        return;
    }
    match store.set(policy) {
        Ok(()) => tracing::info!(fingerprint = %key,
            "unpaired: dropped this device's display settings"),
        Err(e) => tracing::warn!(fingerprint = %key,
            "unpaired: could not drop this device's display settings ({e:#}) — a device \
             re-paired to the same fingerprint would inherit them"),
    }
}

/// One live or kept virtual display.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiDisplayInfo {
    /// Stable-enough id for the `/display/release` `slot` argument.
    slot: u64,
    /// `pf-vdisplay`, `kwin`, …
    backend: String,
    /// `WIDTHxHEIGHT@HZ`.
    mode: String,
    /// `active` | `lingering` | `pinned`.
    state: String,
    /// Milliseconds until a lingering display is torn down (absent when active/pinned).
    expires_in_ms: Option<u64>,
    sessions: u32,
    client: Option<String>,
    /// Shared-desktop group id; same group = one desktop.
    group: u32,
    /// Ordinal within the group, acquire order, 0-based.
    display_index: u32,
    /// Desktop-space top-left (auto-row or manual layout).
    x: i32,
    y: i32,
    /// Per-client identity slot (absent = shared/anonymous). Keys persistent config and manual layout.
    identity_slot: Option<u32>,
    /// Group topology: `extend` | `primary` | `exclusive`.
    topology: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DisplayStateResponse {
    displays: Vec<ApiDisplayInfo>,
}

/// Physical monitor as the compositor reports it.
#[derive(Serialize, ToSchema)]
pub(crate) struct ApiMonitorInfo {
    /// Connector (`DP-1`, `HDMI-A-2`) — the value `PUNKTFUNK_CAPTURE_MONITOR` takes.
    connector: String,
    /// Picker label (`make model`, else the connector).
    description: String,
    /// `WIDTHxHEIGHT@HZ` of the current mode (size only when the refresh is unknown).
    mode: String,
    /// Desktop-space top-left. Distinguishes two heads of the same size.
    x: i32,
    y: i32,
    scale: f64,
    primary: bool,
    /// Driven right now. Disabled heads stay listed so they are not missing from the picker.
    enabled: bool,
    /// Best-effort: one of our virtual displays, not a real head. Reliable on KWin only.
    managed: bool,
    /// True when `PUNKTFUNK_CAPTURE_MONITOR` currently names this monitor.
    selected: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MonitorsResponse {
    /// Enumeration source (`kwin`, `mutter`, `windows`), when resolved.
    compositor: Option<String>,
    /// Heads, ordered left-to-right by desktop position.
    monitors: Vec<ApiMonitorInfo>,
    /// Configured pin, even when it matches no head (console can show a dangling pin).
    pinned: Option<String>,
    /// True when this build can stream a chosen physical head.
    ///
    /// Enumeration and capture are separate. Off Linux, heads are listed but there is no
    /// mirror backend (`vdisplay::open` has no Windows arm; `pf-capture` only has
    /// `open_idd_push`). The console treats `false` as a read-only picker.
    pin_supported: bool,
    /// Enumeration failure. `None` with an empty list means the host has no heads.
    error: Option<String>,
}

/// Physical monitors
///
/// Heads this host has, for a capture-pin picker. Read-only; does not create, move, or disable.
/// Managed virtual displays are `/display/state`. See `design/per-monitor-portal-capture.md`.
#[utoipa::path(
    get,
    path = "/display/monitors",
    tag = "display",
    operation_id = "getDisplayMonitors",
    responses(
        (status = OK, description = "The host's physical monitors", body = MonitorsResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_monitors() -> Json<MonitorsResponse> {
    // Effective pin (env override, else stored policy): highlight what sessions will mirror.
    #[cfg(target_os = "linux")]
    let pinned = crate::vdisplay::capture_monitor();
    // No mirror backend. Report `None` even if a pin is stored — highlighting a head nothing will
    // capture is the false signal this field exists to avoid. `pin_supported: false` is the flag.
    #[cfg(not(target_os = "linux"))]
    let pinned: Option<String> = None;
    let pin_supported = cfg!(target_os = "linux");
    // Shells out / D-Bus / Wayland, and on Windows walks CCD (can serialize on the display-config
    // lock). Off the async worker.
    let (compositor, listed) = tokio::task::spawn_blocking(|| {
        // No compositor to detect. Label the CCD walk as `windows` instead of Linux XDG advice.
        #[cfg(windows)]
        {
            (
                Some("windows".to_string()),
                crate::vdisplay::monitors::list_windows(),
            )
        }
        #[cfg(not(windows))]
        match crate::vdisplay::detect() {
            Ok(c) => (Some(c.id().to_string()), crate::vdisplay::monitors::list(c)),
            Err(e) => (None, Err(e)),
        }
    })
    .await
    .unwrap_or_else(|e| (None, Err(anyhow::anyhow!("enumeration task failed: {e}"))));
    let (monitors, error) = match listed {
        Ok(ms) => (
            ms.into_iter()
                .map(|m| ApiMonitorInfo {
                    mode: m.mode_label(),
                    selected: pinned
                        .as_deref()
                        .is_some_and(|p| p.eq_ignore_ascii_case(&m.connector)),
                    connector: m.connector,
                    description: m.description,
                    x: m.x,
                    y: m.y,
                    scale: m.scale,
                    primary: m.primary,
                    enabled: m.enabled,
                    managed: m.managed,
                })
                .collect(),
            None,
        ),
        Err(e) => (Vec::new(), Some(format!("{e:#}"))),
    };
    Json(MonitorsResponse {
        compositor,
        monitors,
        pinned,
        pin_supported,
        error,
    })
}

/// Request body for `releaseDisplay`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ReleaseDisplayRequest {
    /// Slot to release (see `state`); omit to release **all** kept displays.
    #[serde(default)]
    slot: Option<u64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ReleaseDisplayResult {
    released: usize,
}

/// Live virtual displays
///
/// Active (streaming), lingering (countdown to teardown), or pinned.
/// See `design/display-management.md`.
#[utoipa::path(
    get,
    path = "/display/state",
    tag = "display",
    operation_id = "getDisplayState",
    responses(
        (status = OK, description = "The live/kept virtual displays", body = DisplayStateResponse),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_display_state() -> Json<DisplayStateResponse> {
    let snap = crate::vdisplay::registry::snapshot();
    Json(DisplayStateResponse {
        displays: snap
            .displays
            .into_iter()
            .map(|d| ApiDisplayInfo {
                slot: d.slot,
                backend: d.backend,
                mode: format!("{}x{}@{}", d.mode.0, d.mode.1, d.mode.2),
                state: d.state,
                expires_in_ms: d.expires_in_ms,
                sessions: d.sessions,
                client: d.client,
                group: d.group,
                display_index: d.display_index,
                x: d.position.0,
                y: d.position.1,
                identity_slot: d.identity_slot,
                topology: d.topology,
            })
            .collect(),
    })
}

/// Release kept virtual displays
///
/// Tear down lingering/pinned displays now. `slot` releases one; omit to release all.
/// Active (streaming) displays are never torn down here — that is session control.
#[utoipa::path(
    post,
    path = "/display/release",
    tag = "display",
    operation_id = "releaseDisplay",
    request_body = ReleaseDisplayRequest,
    responses(
        (status = OK, description = "The number of kept displays released", body = ReleaseDisplayResult),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn release_display(
    ApiJson(req): ApiJson<ReleaseDisplayRequest>,
) -> Json<ReleaseDisplayResult> {
    // Teardown restores the topology: CCD commits, a driver IOCTL and, on the slow paths, a
    // PowerShell shell-out — seconds of blocking work. Off the async worker, as the listing
    // above already does.
    let slot = req.slot;
    let released = tokio::task::spawn_blocking(move || crate::vdisplay::registry::release(slot))
        .await
        .unwrap_or(0);
    tracing::info!(slot = ?req.slot, released, "management API: display release");
    Json(ReleaseDisplayResult { released })
}

/// Manual layout: identity-slot id as string (same id `/display/state` reports) → desktop offset.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DisplayLayoutRequest {
    /// `{"<identity_slot>": {"x": …, "y": …}}` desktop top-left per slot.
    #[serde(default)]
    positions: std::collections::BTreeMap<String, crate::vdisplay::policy::Position>,
}

/// Arrange virtual displays
///
/// Persist per-identity-slot `(x, y)` offsets and switch the layout to manual. Applies on the next
/// connect (a live group re-applies on its next acquire). Locks current effective behavior into
/// explicit fields so arranging never silently changes keep-alive/topology/conflict/identity.
/// See `design/display-management.md`.
#[utoipa::path(
    put,
    path = "/display/layout",
    tag = "display",
    operation_id = "setDisplayLayout",
    request_body = DisplayLayoutRequest,
    responses(
        (status = OK, description = "Layout stored; the new settings state", body = DisplaySettingsState),
        (status = INTERNAL_SERVER_ERROR, description = "Layout could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_display_layout(ApiJson(req): ApiJson<DisplayLayoutRequest>) -> Response {
    let store = crate::vdisplay::policy::prefs();
    let policy = store.get().with_manual_layout(req.positions);
    if let Err(e) = store.set(policy) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the display layout — {e:#}"),
        );
    }
    tracing::info!(
        positions = display_settings_state().settings.layout.positions.len(),
        "management API: display layout updated"
    );
    Json(display_settings_state()).into_response()
}

/// List the saved custom presets
///
/// Named field-bundles in `display-presets.json`. Also on `GET /display/settings` as `custom_presets`.
#[utoipa::path(
    get,
    path = "/display/presets",
    tag = "display",
    operation_id = "listCustomPresets",
    responses(
        (status = OK, description = "The saved custom presets", body = Vec<crate::vdisplay::policy::CustomPreset>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_custom_presets() -> Json<Vec<crate::vdisplay::policy::CustomPreset>> {
    Json(crate::vdisplay::policy::load_custom_presets())
}

/// Save a custom preset
///
/// Named bundle of the display-behavior axes. Host assigns a stable id in the body. Apply with
/// `PUT /display/settings` carrying a `Custom` policy of its `fields` — no separate apply route.
#[utoipa::path(
    post,
    path = "/display/presets",
    tag = "display",
    operation_id = "createCustomPreset",
    request_body = crate::vdisplay::policy::CustomPresetInput,
    responses(
        (status = CREATED, description = "Preset created", body = crate::vdisplay::policy::CustomPreset),
        (status = BAD_REQUEST, description = "Empty name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn create_custom_preset(
    ApiJson(input): ApiJson<crate::vdisplay::policy::CustomPresetInput>,
) -> Response {
    if input.name.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "preset name must not be empty");
    }
    match crate::vdisplay::policy::add_custom_preset(input) {
        Ok(preset) => (StatusCode::CREATED, Json(preset)).into_response(),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the preset — {e}"),
        ),
    }
}

/// Update a custom preset
#[utoipa::path(
    put,
    path = "/display/presets/{id}",
    tag = "display",
    operation_id = "updateCustomPreset",
    params(("id" = String, Path, description = "The custom preset id")),
    request_body = crate::vdisplay::policy::CustomPresetInput,
    responses(
        (status = OK, description = "Preset updated", body = crate::vdisplay::policy::CustomPreset),
        (status = BAD_REQUEST, description = "Empty name", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom preset with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn update_custom_preset(
    Path(id): Path<String>,
    ApiJson(input): ApiJson<crate::vdisplay::policy::CustomPresetInput>,
) -> Response {
    if input.name.trim().is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "preset name must not be empty");
    }
    match crate::vdisplay::policy::update_custom_preset(&id, input) {
        Ok(Some(preset)) => Json(preset).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "no custom preset with that id"),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the preset — {e}"),
        ),
    }
}

/// Delete a custom preset
///
/// Removes it from the catalog. The active policy is untouched — catalog and
/// `display-settings.json` are decoupled.
#[utoipa::path(
    delete,
    path = "/display/presets/{id}",
    tag = "display",
    operation_id = "deleteCustomPreset",
    params(("id" = String, Path, description = "The custom preset id")),
    responses(
        (status = NO_CONTENT, description = "Preset deleted"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No custom preset with that id", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the catalog", body = ApiError),
    )
)]
pub(crate) async fn delete_custom_preset(Path(id): Path<String>) -> Response {
    match crate::vdisplay::policy::delete_custom_preset(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "no custom preset with that id"),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't delete the preset — {e}"),
        ),
    }
}
