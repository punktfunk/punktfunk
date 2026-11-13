//! Display-tagged management endpoints: policy, live state, physical monitors, layout, and
//! custom presets.
//!
//! A PUT stores the next-session policy; a running session keeps the display it opened on.
//! `keep_alive: forever` pins until `POST /display/release`. A host with no mirror backend
//! (neither Linux nor Windows) drops a monitor `screen` on write.
//!
//! See `design/display-management.md` and `design/per-monitor-portal-capture.md`.

use super::auth::PairedDevice;
use super::shared::*;
use crate::vdisplay::policy::{ClientOverlay, DisplayTarget, SetBy, Topology};
use crate::vdisplay::Compositor;
use axum::Extension;
use punktfunk_core::quic::GRANT_DISPLAY;

/// Picker row. `fields` is the expansion so the console does not hardcode it.
#[derive(Serialize, ToSchema)]
pub(crate) struct PresetInfo {
    /// `default` | `gaming-rig` | `shared-desktop` | `hotdesk` | `workstation` | `play-together`.
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
    /// What `topology: auto` resolves to on this host, never `auto` (`effective_topology`).
    auto_topology: crate::vdisplay::policy::Topology,
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
        "play-together" => "For playing or watching with friends. Everyone sees and uses one screen, sized for the first to connect.",
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

/// What the session's backends act on: a listed monitor kept lit under `exclusive`, a launch
/// on a workspace of its own (`vdisplay::claim_workspace`), a device's screen at its own scale.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct SessionTerms {
    keep_monitors: bool,
    launch_workspace: bool,
    scale: bool,
}

/// [`SessionTerms`] as of the current session epoch. `available()` walks /proc and forks, so
/// the answer is kept until the epoch moves: a new desktop instance asks again.
#[cfg(target_os = "linux")]
fn session_terms() -> SessionTerms {
    static LAST: std::sync::Mutex<Option<(u64, SessionTerms)>> = std::sync::Mutex::new(None);
    let epoch = crate::vdisplay::session_epoch();
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match *last {
        Some((at, terms)) if at == epoch => terms,
        _ => {
            let backends = crate::vdisplay::available();
            let terms = SessionTerms {
                keep_monitors: backends.iter().any(|c| c.honours_keep_monitors()),
                launch_workspace: backends.iter().any(|c| c.places_launch_workspace()),
                scale: backends.iter().any(|c| c.applies_device_scale()),
            };
            *last = Some((epoch, terms));
            terms
        }
    }
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
        ("play-together", Preset::PlayTogether),
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
    // (`compositor_route.rs`), so without the binary the axis stores and does nothing.
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
    // A monitor `screen` needs the mirror backend (`vdisplay::open_for`): Linux and Windows.
    // Do not advertise it elsewhere — a stored monitor would never take effect.
    let mirrors = cfg!(any(target_os = "linux", target_os = "windows"));
    if mirrors {
        enforced.push("screen".into());
        enforced.push("capture_monitor".into());
    }
    #[cfg(target_os = "linux")]
    let session = session_terms();
    // Hyprland and sway only. KWin is a later step, and Mutter/GNOME, gamescope and Windows
    // have no per-output workspace to aim a launch at, so the axis would store and do nothing
    // there — the dead control this gate exists to prevent.
    #[cfg(target_os = "linux")]
    if session.launch_workspace {
        enforced.push("launch_workspace".into());
    }
    // KWin, Hyprland and sway. Mutter and the Windows CCD isolate darken every head they
    // find, so the keep-list would store and do nothing there.
    #[cfg(target_os = "linux")]
    if session.keep_monitors {
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
    // A Linux backend reads it at `create`, after `set_client_identity` named the device. The
    // Windows isolate is one topology per group, so the device that opens the group decides it.
    client_enforced.push("topology".into());
    // Mutter mints a fresh EDID serial per session, so the host remembers the scale for it.
    #[cfg(target_os = "linux")]
    if session.scale {
        client_enforced.push("scale".into());
    }
    // Every session opens its device's screen (`vdisplay::open_for`).
    if mirrors {
        client_enforced.push("screen".into());
    }
    // A contract seat has nothing to set: the contract fixes its desktop, and only a device's
    // cap, scale and second-device rule follow the device here.
    if store.follows_contract() {
        enforced.clear();
        client_enforced.retain(|f| matches!(f.as_str(), "max_mode" | "scale" | "mode_conflict"));
    }
    // Overlays ride their own field, never `settings`. The console PUTs `settings`
    // back whole, and the PUT refuses a body carrying `clients` — so leaving them
    // in here would make every host-wide save fail the moment one device had an
    // overlay. The file keeps them; the wire does not.
    let mut settings = settings;
    let clients = std::mem::take(&mut settings.clients);
    DisplaySettingsState {
        effective: settings.effective(),
        // Unconfigured, `effective_topology` honours the legacy `*_VIRTUAL_PRIMARY` pins first.
        auto_topology: if configured {
            crate::vdisplay::resolve_topology(policy::Topology::Auto)
        } else {
            crate::vdisplay::effective_topology(None)
        },
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
pub(crate) async fn set_display_settings(ApiJson(body): ApiJson<serde_json::Value>) -> Response {
    if let Some(refused) = refused_on_seat() {
        return refused;
    }
    // The body is merged key by key onto the stored policy ([`merged`]), so it is an object.
    let serde_json::Value::Object(mut body) = body else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Couldn't read the display policy — it isn't a JSON object",
        );
    };
    // The overlay map never rides the policy object. A console that fetched
    // `/display/settings`, sat on it, and PUT it back would otherwise revert every
    // per-device change made in between.
    if body
        .remove("clients")
        .is_some_and(|c| c != serde_json::json!({}))
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "Per-device display settings are saved on their own — this request carried them \
             with the host policy.",
        );
    }
    if let Some(refused) = write(body) {
        return refused;
    }
    tracing::info!("management API: display policy updated");
    Json(display_settings_state()).into_response()
}

/// A contract seat has no display settings of its own: every write is the box's to make.
fn refused_on_seat() -> Option<Response> {
    crate::vdisplay::policy::prefs()
        .follows_contract()
        .then(|| {
            api_error_coded(
                StatusCode::CONFLICT,
                "seat_display_read_only",
                "A seat's display can't be changed here. Change display settings on the box.",
            )
        })
}

/// Lay `body` over the stored policy in one store transaction ([`merged`]), then re-aim
/// absolute input at its pin (or clear the anchor) without a restart. `Some` is the answer
/// when nothing was stored: 400 for a body that does not read as a policy, 500 for the write.
///
/// A host with no mirror backend drops a monitor `screen`: it would never take effect there.
fn write(body: serde_json::Map<String, serde_json::Value>) -> Option<Response> {
    let mut malformed = None;
    let stored = crate::vdisplay::policy::prefs().update(|p| match merged(p, body) {
        Ok(next) => {
            *p = next.sanitized();
            #[cfg(not(any(target_os = "linux", target_os = "windows")))]
            if let Some(dropped) = p.screen.take().and_then(DisplayTarget::into_monitor) {
                p.capture_monitor = None;
                tracing::warn!(
                    "management API: ignoring screen={dropped:?} — this host cannot \
                     stream a chosen physical monitor; the pin was NOT stored"
                );
            }
            true
        }
        Err(e) => {
            malformed = Some(e);
            false
        }
    });
    if let Some(e) = malformed {
        return Some(api_error(
            StatusCode::BAD_REQUEST,
            &format!("Couldn't read the display policy — {e}"),
        ));
    }
    if let Err(e) = stored {
        return Some(api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "display_policy_not_saved",
            "Couldn't save the display policy",
            &format!("{e:#}"),
        ));
    }
    #[cfg(target_os = "linux")]
    crate::refresh_capture_monitor_anchor("display policy updated");
    None
}

/// One edit of the stored host policy, read and written in one store transaction, then the
/// anchor re-aim [`write`] does.
pub(super) fn write_with(
    edit: impl FnOnce(&mut crate::vdisplay::policy::DisplayPolicy),
) -> anyhow::Result<()> {
    crate::vdisplay::policy::prefs().update(|p| {
        edit(p);
        true
    })?;
    #[cfg(target_os = "linux")]
    crate::refresh_capture_monitor_anchor("display policy updated");
    Ok(())
}

/// `body` laid over `stored` key by key: a key the body omits keeps its stored value, and
/// `null` clears an optional one. The per-device overlays ride across, since the PUT strips
/// `clients` from the body first.
///
/// `screen` and its alias `capture_monitor` are one setting: a body naming one of them drops
/// the stored other, so an older console's `capture_monitor` write is not outvoted.
pub(super) fn merged(
    stored: &crate::vdisplay::policy::DisplayPolicy,
    body: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Result<crate::vdisplay::policy::DisplayPolicy> {
    let mut base = serde_json::to_value(stored)?;
    if let Some(keys) = base.as_object_mut() {
        match (
            body.contains_key("screen"),
            body.contains_key("capture_monitor"),
        ) {
            (true, false) => keys.remove("capture_monitor"),
            (false, true) => keys.remove("screen"),
            _ => None,
        };
        keys.extend(body);
    }
    serde_json::from_value(base)
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
/// The operator writes the WHOLE overlay: a field absent from the body stops being pinned and
/// the device follows the host again. An overlay that pins nothing is dropped, which is the
/// same as `DELETE`. A paired device writes its pick on its own row while its access holds
/// `GRANT_DISPLAY` (`0x100`): `screen` alone, keeping the row's `topology`, or `screen` with
/// `topology` (`extend`, `primary` or `exclusive`). A monitor drops the row's topology, and a
/// null screen clears the device's pick. Its other fields stay, and it is answered 204.
/// `set_by` records who last wrote the pick; the operator's write keeps it while the row's
/// screen and topology are unchanged.
#[utoipa::path(
    put,
    path = "/display/clients/{fingerprint}",
    tag = "display",
    operation_id = "setDisplayClient",
    params(("fingerprint" = String, Path, description = "Pairing fingerprint (hex)")),
    request_body = crate::vdisplay::policy::ClientOverlay,
    responses(
        (status = OK, description = "Overlay stored; the new settings state", body = DisplaySettingsState),
        (status = NO_CONTENT, description = "A device's pick stored"),
        (status = BAD_REQUEST, description = "Empty fingerprint", body = ApiError),
        (status = FORBIDDEN, description = "A device wrote another row or more than its screen and topology, or may not choose its screen (`screen_choice_not_granted`)", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "The body is not an overlay, or a device's topology is not `extend`, `primary` or `exclusive`", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Overlay could not be persisted", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_display_client(
    State(st): State<Arc<MgmtState>>,
    device: Option<Extension<PairedDevice>>,
    Path(fingerprint): Path<String>,
    ApiJson(body): ApiJson<serde_json::Value>,
) -> Response {
    let key = fingerprint.trim().to_ascii_lowercase();
    if let Some(Extension(PairedDevice(me))) = device {
        return set_own_pick(&st, &me, &key, body);
    }
    if let Some(refused) = refused_on_seat() {
        return refused;
    }
    if key.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "no device named");
    }
    let overlay = match serde_json::from_value::<ClientOverlay>(body) {
        Ok(o) => o.sanitized(),
        Err(e) => {
            return api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                &format!("read the display overlay: {e}"),
            )
        }
    };
    let store = crate::vdisplay::policy::prefs();
    // An empty overlay is "follow the host", which is what an absent key already means. On an
    // UNCONFIGURED host writing one would materialise a whole default policy as a side effect:
    // `configured()` flips process-wide, which retires the legacy env knobs and moves a Linux
    // host to per-client identity. Nothing to store, so store nothing.
    if overlay.is_empty() && store.configured().is_none() {
        return Json(display_settings_state()).into_response();
    }
    // `sanitized` drops an overlay that pins nothing, so this covers the reset
    // case too without a second path. A pick the body leaves as it was keeps who chose it.
    let saved = store.update(|p| {
        let overlay = overlay.console_write(p.clients.get(&key));
        p.clients.insert(key.clone(), overlay);
        true
    });
    if let Err(e) = saved {
        return api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_display_not_saved",
            "Couldn't save the display settings for this device",
            &format!("{e:#}"),
        );
    }
    tracing::info!(fingerprint = %key, "management API: per-device display overlay updated");
    Json(display_settings_state()).into_response()
}

/// What a device without `GRANT_DISPLAY` is told when it picks a screen.
pub(super) fn screen_choice_refused() -> Response {
    api_error_coded(
        StatusCode::FORBIDDEN,
        "screen_choice_not_granted",
        "This host decides the screen for this device.",
    )
}

/// A paired device `me` writes the pick of its row `key` ([`device_pick`]), while its live
/// access holds `GRANT_DISPLAY`. The answer carries no settings: the state names every other
/// device.
fn set_own_pick(st: &MgmtState, me: &str, key: &str, body: serde_json::Value) -> Response {
    let (screen, topology) = match device_pick(me, key, body) {
        Ok(pick) => pick,
        Err((status, why)) => return api_error(status, &why),
    };
    if !super::downloads::granted(st, Some(me), GRANT_DISPLAY) {
        return screen_choice_refused();
    }
    if let Some(refused) = refused_on_seat() {
        return refused;
    }
    let store = crate::vdisplay::policy::prefs();
    // A clear on an unconfigured host has nothing to remove, and a write would configure it.
    if screen.is_none() && store.configured().is_none() {
        return StatusCode::NO_CONTENT.into_response();
    }
    let saved = store.update(|p| {
        let row = p.clients.entry(key.to_string()).or_default();
        row.set_device_pick(screen, topology);
        true
    });
    if let Err(e) = saved {
        return api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "device_display_not_saved",
            "Couldn't save the display settings for this device",
            &format!("{e:#}"),
        );
    }
    tracing::info!(fingerprint = %key, "management API: a device set its screen");
    StatusCode::NO_CONTENT.into_response()
}

/// The pick a paired device `me` may write to the row `key`: its own row, and a body of
/// `screen` alone or `screen` with `topology`. Another row or any other key is 403. A `screen`
/// that does not read, a `topology` without one, or a `topology` other than `extend`,
/// `primary` or `exclusive` is 422.
pub(super) fn device_pick(
    me: &str,
    key: &str,
    body: serde_json::Value,
) -> Result<(Option<DisplayTarget>, Option<Topology>), (StatusCode, String)> {
    let refused = || {
        (
            StatusCode::FORBIDDEN,
            "a device may set only its own screen".into(),
        )
    };
    let unreadable = |why: String| (StatusCode::UNPROCESSABLE_ENTITY, why);
    let serde_json::Value::Object(mut body) = body else {
        return Err(refused());
    };
    let screen = body.remove("screen");
    let topology = body.remove("topology");
    if !me.eq_ignore_ascii_case(key) || !body.is_empty() {
        return Err(refused());
    }
    let topology = match topology.map(serde_json::from_value) {
        None => None,
        Some(Ok(t @ (Topology::Extend | Topology::Primary | Topology::Exclusive))) => Some(t),
        Some(_) => {
            return Err(unreadable(
                "a device's topology is extend, primary or exclusive".into(),
            ))
        }
    };
    if topology.is_some() && screen.is_none() {
        return Err(unreadable("a topology needs a screen".into()));
    }
    let screen = serde_json::from_value(screen.unwrap_or(serde_json::Value::Null))
        .map_err(|e| unreadable(format!("read the screen: {e}")))?;
    Ok((screen, topology))
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
    if let Some(refused) = refused_on_seat() {
        return refused;
    }
    let key = fingerprint.trim().to_ascii_lowercase();
    let store = crate::vdisplay::policy::prefs();
    // Already following the host: nothing to write, and a no-op write would
    // rewrite the file on every unpair.
    match store.update(|p| p.clients.remove(&key).is_some()) {
        Ok(false) => return Json(display_settings_state()).into_response(),
        Ok(true) => {}
        Err(e) => {
            return api_error_logged(
                StatusCode::INTERNAL_SERVER_ERROR,
                "device_display_not_cleared",
                "Couldn't clear the display settings for this device",
                &format!("{e:#}"),
            );
        }
    }
    tracing::info!(fingerprint = %key, "management API: per-device display overlay cleared");
    Json(display_settings_state()).into_response()
}

/// Drop a device's overlay when it unpairs, so a later device that is granted the
/// same fingerprint cannot inherit settings the operator made for another one.
pub(crate) fn forget_display_overlay(fingerprint: &str) {
    let key = fingerprint.trim().to_ascii_lowercase();
    let store = crate::vdisplay::policy::prefs();
    match store.update(|p| p.clients.remove(&key).is_some()) {
        Ok(false) => {}
        Ok(true) => tracing::info!(fingerprint = %key,
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
    /// One of our virtual displays, not a real head. Best-effort on Sway, whose
    /// `HEADLESS-N` may be the operator's own.
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
    /// True when this build can stream a chosen physical head: Linux and Windows.
    ///
    /// Enumeration and capture are separate, so a host can list heads it cannot stream. The
    /// console treats `false` as a read-only picker.
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
pub(crate) async fn get_display_monitors() -> Result<Json<MonitorsResponse>, Response> {
    let pin_supported = cfg!(any(target_os = "linux", target_os = "windows"));
    // Effective pin (env override, else stored policy): highlight what sessions will mirror.
    // With no mirror backend report `None` even if a pin is stored — highlighting a head
    // nothing will capture is the false signal this field exists to avoid.
    let pinned = pin_supported
        .then(crate::vdisplay::capture_monitor)
        .flatten();
    // Shells out / D-Bus / Wayland, and on Windows walks CCD (can serialize on the display-config
    // lock). Off the async worker.
    let (compositor, listed) = blocking("monitor_list", || {
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
    .await?;
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
    Ok(Json(MonitorsResponse {
        compositor,
        monitors,
        pinned,
        pin_supported,
        error,
    }))
}

/// One row of the screen picker. The first is always `own`; the rest are the host's heads.
#[derive(Serialize, ToSchema)]
pub(crate) struct ScreenRow {
    /// `own`, else the connector.
    id: String,
    /// The monitor's name (`make model`, else the connector). `Own screen` on the first row,
    /// which clients word themselves.
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    connector: Option<String>,
    /// `WIDTHxHEIGHT@HZ` of the current mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    primary: Option<bool>,
    /// Driven now. A head another device's exclusive screen switched off is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled: Option<bool>,
    /// The pairing label of the other device streaming this monitor, else null. Absent on `own`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>)]
    in_use_by: Option<Option<String>>,
}

/// What a device may stream (`GET /display/targets`).
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplayTargets {
    /// `PUNKTFUNK_CAPTURE_MONITOR` fixes the screen for every device.
    locked: bool,
    /// What a device with no screen of its own streams.
    host_default: DisplayTarget,
    /// What the calling device is set to, before a taken or missing monitor falls back to its
    /// own screen. A screen it set itself counts only while `may_choose`. Null on the operator
    /// and plugin lanes.
    yours: Option<DisplayTarget>,
    /// Whether the caller may choose the screen: a paired device's `GRANT_DISPLAY`, always on
    /// the operator and plugin lanes. A device's write without it is 403.
    may_choose: bool,
    targets: Vec<ScreenRow>,
    /// The roles a device's own screen takes on this desk. Absent on a contract seat, a host
    /// with no mirror backend, a gamescope session, and a desktop that lists no physical
    /// monitor.
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<ScreenRoles>,
}

/// How a device's own screen sits among the host's monitors: its `topology`, from its side.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ScreenRole {
    /// `exclusive`: the host's monitors turn off.
    Only,
    /// `primary`: the monitors stay on and apps open on this screen.
    Main,
    /// `extend`: one more screen beside the monitors.
    Second,
}

impl ScreenRole {
    /// The role `topology` gives ([`crate::vdisplay::role_of`]).
    fn of(topology: Topology, primary: bool) -> ScreenRole {
        use punktfunk_core::quic::v2::hello::Role;
        match crate::vdisplay::role_of(topology, primary) {
            Role::Only => ScreenRole::Only,
            Role::Main => ScreenRole::Main,
            Role::Second => ScreenRole::Second,
        }
    }
}

/// The roles a device's own screen takes on the desk.
#[derive(Debug, PartialEq, Serialize, ToSchema)]
pub(crate) struct ScreenRoles {
    /// The role of the calling device's own pick while it counts and its screen is its own.
    /// Null with no pick, a monitor pick, no `GRANT_DISPLAY`, and on the operator lane.
    yours: Option<ScreenRole>,
    /// What a connect gets with no pick of the device's: the console's row for it, else the
    /// host's topology.
    host_default: ScreenRole,
    /// The roles this host gives. `main` only where the desktop has a main screen.
    offered: Vec<ScreenRole>,
}

/// The device's [`ScreenRoles`], or `None` off the desk ([`crate::vdisplay::on_the_desk`]).
/// `main` is offered where `compositor` has a main screen.
pub(super) fn screen_roles(
    seat: bool,
    compositor: Option<Compositor>,
    physical: Option<bool>,
    yours: Option<Topology>,
    host_default: Topology,
) -> Option<ScreenRoles> {
    if !crate::vdisplay::on_the_desk(seat, compositor, physical) {
        return None;
    }
    let primary = compositor.is_some_and(Compositor::has_primary);
    let offered = [ScreenRole::Only, ScreenRole::Main, ScreenRole::Second]
        .into_iter()
        .filter(|r| *r != ScreenRole::Main || primary)
        .collect();
    Some(ScreenRoles {
        yours: yours.map(|t| ScreenRole::of(t, primary)),
        host_default: ScreenRole::of(host_default, primary),
        offered,
    })
}

/// The topology of the device `fp`'s own pick, when it set its own screen itself. The caller
/// checks that the pick counts (`GRANT_DISPLAY`).
fn own_pick_topology(fp: &str) -> Option<Topology> {
    let policy = crate::vdisplay::policy::prefs().get();
    let row = policy.overlay_for(Some(fp))?;
    if row.set_by != Some(SetBy::Device) || row.screen != Some(DisplayTarget::Own) {
        return None;
    }
    crate::vdisplay::wanted_topology(Some(fp), true)
}

/// Screen picker
///
/// The device's own screen, then each of the host's monitors with the device streaming it
/// now. A paired device also reads what it is set to and whether it may choose. A host with no
/// mirror backend lists its own screen alone. `role` names the roles its own screen takes on
/// the desk.
#[utoipa::path(
    get,
    path = "/display/targets",
    tag = "display",
    operation_id = "getDisplayTargets",
    responses(
        (status = OK, description = "What a device may stream", body = DisplayTargets),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn get_display_targets(
    State(st): State<Arc<MgmtState>>,
    device: Option<Extension<PairedDevice>>,
) -> Result<Json<DisplayTargets>, Response> {
    let me = device.map(|Extension(PairedDevice(fp))| fp.to_ascii_lowercase());
    let may_choose = me
        .as_deref()
        .is_none_or(|fp| super::downloads::granted(&st, Some(fp), GRANT_DISPLAY));
    let own = ScreenRow {
        id: "own".into(),
        label: "Own screen".into(),
        connector: None,
        mode: None,
        primary: None,
        enabled: None,
        in_use_by: None,
    };
    if !cfg!(any(target_os = "linux", target_os = "windows")) {
        return Ok(Json(DisplayTargets {
            locked: false,
            host_default: DisplayTarget::Own,
            yours: me.map(|_| DisplayTarget::Own),
            may_choose,
            targets: vec![own],
            role: None,
        }));
    }
    let (host_default, locked) = crate::vdisplay::screen_target(None, false);
    let yours = me
        .as_deref()
        .map(|fp| crate::vdisplay::screen_target(Some(fp), may_choose).0);
    // Shells out / D-Bus / Wayland, or walks CCD on Windows: off the async worker.
    let (compositor, heads) = blocking("monitor_list", || {
        #[cfg(windows)]
        let compositor = Some(Compositor::Windows);
        #[cfg(not(windows))]
        let compositor = crate::vdisplay::detect().ok();
        (compositor, crate::vdisplay::heads(compositor))
    })
    .await?;
    let role = screen_roles(
        crate::vdisplay::policy::prefs().follows_contract(),
        compositor,
        heads.as_ref().ok().map(|h| h.iter().any(|m| !m.managed)),
        me.as_deref()
            .filter(|_| may_choose)
            .and_then(own_pick_topology),
        crate::vdisplay::wanted_topology(me.as_deref(), false)
            .unwrap_or_else(|| crate::vdisplay::effective_topology(None)),
    );
    let streamed = crate::vdisplay::streamed_monitors();
    let in_use_by = |connector: &str| {
        streamed.iter().find_map(|(c, owner)| {
            let owner = owner.as_deref()?;
            let other = me.as_deref() != Some(owner);
            (other && c.eq_ignore_ascii_case(connector))
                .then(|| crate::vdisplay::device_label(owner))
        })
    };
    let heads = heads.unwrap_or_else(|e| {
        tracing::debug!(error = %format!("{e:#}"), "screen picker lists no monitors");
        Vec::new()
    });
    let mut targets = vec![own];
    for m in heads.into_iter().filter(|m| !m.managed) {
        targets.push(ScreenRow {
            id: m.connector.clone(),
            mode: Some(m.mode_label()),
            in_use_by: Some(in_use_by(&m.connector)),
            label: m.description,
            connector: Some(m.connector),
            primary: Some(m.primary),
            enabled: Some(m.enabled),
        });
    }
    Ok(Json(DisplayTargets {
        locked,
        host_default,
        yours,
        may_choose,
        targets,
        role,
    }))
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
    let displays = snap.displays.into_iter().map(|d| ApiDisplayInfo {
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
    });
    #[cfg(target_os = "linux")]
    let displays = displays.chain(held_game_mode());
    Json(DisplayStateResponse {
        displays: displays.collect(),
    })
}

/// The `slot` of a held gamescope takeover. Not a pool generation, and safe as a JS number.
#[cfg(target_os = "linux")]
const GAME_MODE_SLOT: u64 = (1 << 53) - 1;

/// The box's own Game Mode, held between sessions by a takeover: a kept row, so Release reaches it.
#[cfg(target_os = "linux")]
fn held_game_mode() -> Option<ApiDisplayInfo> {
    if crate::compositor_route::gamescope_sessions_live() {
        return None;
    }
    let held = crate::vdisplay::held_managed_session()?;
    let (w, h, hz) = held.mode.unwrap_or_default();
    Some(ApiDisplayInfo {
        slot: GAME_MODE_SLOT,
        backend: "gamescope".into(),
        mode: format!("{w}x{h}@{hz}"),
        state: if held.restore_in.is_some() {
            "lingering"
        } else {
            "pinned"
        }
        .into(),
        expires_in_ms: held.restore_in.map(|d| d.as_millis() as u64),
        sessions: 0,
        client: None,
        group: 0,
        display_index: 0,
        x: 0,
        y: 0,
        identity_slot: None,
        topology: crate::vdisplay::registry::topology_str(),
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
    let released = tokio::task::spawn_blocking(move || {
        let released = crate::vdisplay::registry::release(slot);
        #[cfg(target_os = "linux")]
        if slot.is_none_or(|s| s == GAME_MODE_SLOT)
            && !crate::compositor_route::gamescope_sessions_live()
            && crate::vdisplay::release_managed_session()
        {
            return released + 1;
        }
        released
    })
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
    if let Some(refused) = refused_on_seat() {
        return refused;
    }
    let store = crate::vdisplay::policy::prefs();
    let saved = store.update(|p| {
        *p = p.clone().with_manual_layout(req.positions);
        true
    });
    if let Err(e) = saved {
        return api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "display_layout_not_saved",
            "Couldn't save the display layout",
            &format!("{e:#}"),
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
        Err(e) => api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "display_preset_not_saved",
            "Couldn't save the preset",
            &e,
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
        Err(e) => api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "display_preset_not_saved",
            "Couldn't save the preset",
            &e,
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
        Err(e) => api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "display_preset_not_deleted",
            "Couldn't delete the preset",
            &e,
        ),
    }
}
