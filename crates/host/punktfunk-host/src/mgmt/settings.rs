//! `/host/settings` — the [`pf_host_config::registry`] rows the console edits.
//!
//! GET carries everything the page renders: each row's kind and bounds, the value in
//! force, the console's own value, and what set it. A row this host does not act on is
//! left out, not flagged — the Seat defaults while seats are off included. PATCH merges into
//! `host-settings.json`; a value an env var or CLI flag overrides is still stored, and takes
//! effect once that override is gone. On a door, the rows of the host that streams are the owner's
//! host's ([`door_row`]).

use super::shared::*;
use pf_host_config::registry::{Apply, Group, Kind};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingKind {
    Bool,
    Int,
    Decimal,
    Enum,
    Text,
    List,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingGroup {
    Streaming,
    Video,
    Audio,
    Input,
    Network,
    GameMode,
    Session,
    Seats,
    System,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingSource {
    Default,
    Store,
    Env,
    Flag,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SettingApply {
    Now,
    NextSession,
    Restart,
}

/// One setting as the console renders it.
#[derive(Serialize, ToSchema)]
pub(crate) struct SettingState {
    /// Store key; the console's copy key is `setting_<id>`.
    id: String,
    group: SettingGroup,
    kind: SettingKind,
    /// `int` and `decimal` only.
    min: Option<f64>,
    /// `int` and `decimal` only.
    max: Option<f64>,
    /// `int` and `decimal` only, e.g. `fps`.
    unit: Option<String>,
    /// `enum` only, canonical spellings in display order.
    options: Option<Vec<String>>,
    /// `text` only.
    max_len: Option<u32>,
    default: Value,
    /// The value in force.
    value: Value,
    /// The console's value, even while an env var or flag overrides it.
    stored: Option<Value>,
    source: SettingSource,
    /// The env var or CLI flag that set `value` (`source` `env` or `flag`).
    origin: Option<String>,
    /// The name to set in `host.env` to pin this setting.
    env: String,
    apply: SettingApply,
    advanced: bool,
    /// English label, for a setting the console has no copy for.
    title: String,
    /// docs-site page slug under `/docs/`.
    docs: String,
    /// Changed since the host started, and applies only after a restart.
    restart_pending: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct HostSettingsState {
    /// Rows this host acts on, in page order.
    settings: Vec<SettingState>,
    /// Setting ids waiting for a restart.
    restart_pending: Vec<String>,
    /// The file an operator edits to pin a setting.
    #[schema(example = "/home/me/.config/punktfunk/host.env")]
    env_file: String,
}

/// Setting id → new value; `null` returns a setting to its default.
#[derive(Deserialize, ToSchema)]
#[schema(example = json!({"clipboard": "text", "max_fps": null}))]
pub(crate) struct HostSettingsPatch(BTreeMap<String, Value>);

/// Whether this box runs seats, for the Seat defaults. A seat host has none to set.
#[derive(Clone, Copy, Default)]
pub(crate) struct SeatsOn {
    full: bool,
}

pub(crate) async fn seats_on() -> SeatsOn {
    if pf_paths::seat::role().is_seat_host() {
        return SeatsOn::default();
    }
    let full = tokio::task::spawn_blocking(|| crate::seats::snapshot().on)
        .await
        .unwrap_or(false);
    SeatsOn { full }
}

fn shown(group: Group, seats: SeatsOn) -> bool {
    group != Group::Seats || seats.full
}

pub(crate) fn state(seats: SeatsOn) -> HostSettingsState {
    let snap = pf_host_config::snapshot();
    let pending = pf_host_config::restart_pending();
    let settings = snap
        .settings
        .iter()
        .filter(|r| r.setting.available() && shown(r.setting.group, seats))
        .map(|r| {
            let s = r.setting;
            let (kind, min, max, unit, options, max_len) = match s.kind {
                Kind::Bool => (SettingKind::Bool, None, None, None, None, None),
                Kind::Int { min, max, unit } => (
                    SettingKind::Int,
                    Some(min as f64),
                    Some(max as f64),
                    Some(unit.to_string()),
                    None,
                    None,
                ),
                Kind::Decimal { min, max, unit } => (
                    SettingKind::Decimal,
                    Some(min),
                    Some(max),
                    Some(unit.to_string()),
                    None,
                    None,
                ),
                Kind::Enum(o) => (
                    SettingKind::Enum,
                    None,
                    None,
                    None,
                    Some(o.iter().map(|x| x.to_string()).collect()),
                    None,
                ),
                Kind::Text { max_len } => (
                    SettingKind::Text,
                    None,
                    None,
                    None,
                    None,
                    Some(max_len as u32),
                ),
                Kind::List => (SettingKind::List, None, None, None, None, None),
            };
            SettingState {
                id: s.id.into(),
                group: match s.group {
                    Group::Streaming => SettingGroup::Streaming,
                    Group::Video => SettingGroup::Video,
                    Group::Audio => SettingGroup::Audio,
                    Group::Input => SettingGroup::Input,
                    Group::Network => SettingGroup::Network,
                    Group::GameMode => SettingGroup::GameMode,
                    Group::Session => SettingGroup::Session,
                    Group::Seats => SettingGroup::Seats,
                    Group::System => SettingGroup::System,
                },
                kind,
                min,
                max,
                unit,
                options,
                max_len,
                default: s.default.to_value(),
                value: r.value.clone(),
                stored: r.stored.clone(),
                source: match r.source {
                    pf_host_config::Source::Default => SettingSource::Default,
                    pf_host_config::Source::Store => SettingSource::Store,
                    pf_host_config::Source::Env => SettingSource::Env,
                    pf_host_config::Source::Flag => SettingSource::Flag,
                },
                origin: r.origin.map(str::to_string),
                env: s.env.into(),
                apply: match s.apply {
                    Apply::Now => SettingApply::Now,
                    Apply::NextSession => SettingApply::NextSession,
                    Apply::Restart => SettingApply::Restart,
                },
                advanced: s.advanced,
                title: s.title.into(),
                docs: s.docs.into(),
                restart_pending: pending.contains(&s.id),
            }
        })
        .collect();
    HostSettingsState {
        settings,
        restart_pending: pending.iter().map(|id| id.to_string()).collect(),
        env_file: pf_paths::config_dir()
            .join("host.env")
            .display()
            .to_string(),
    }
}

#[derive(Serialize, ToSchema)]
pub(crate) struct PlayingApps {
    /// Lowercased app names, as the voice-chat app list matches them.
    #[schema(example = json!(["discord", "firefox"]))]
    apps: Vec<String>,
}

/// List apps playing audio
///
/// Audio output streams on the host right now, by app. Empty on a host that cannot list them.
#[utoipa::path(
    get,
    path = "/host/audio/apps",
    tag = "host",
    operation_id = "getPlayingApps",
    responses(
        (status = OK, description = "Apps playing audio now", body = PlayingApps),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_playing_apps() -> Json<PlayingApps> {
    let apps = tokio::task::spawn_blocking(crate::audio::playing_apps)
        .await
        .unwrap_or_default();
    Json(PlayingApps { apps })
}

/// Get the host settings
///
/// Every setting this host acts on, with the value in force and what set it.
#[utoipa::path(
    get,
    path = "/host/settings",
    tag = "host",
    operation_id = "getHostSettings",
    responses(
        (status = OK, description = "The settings, in page order", body = HostSettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_host_settings(State(st): State<Arc<MgmtState>>) -> Response {
    answer(&st).await
}

/// What the door acts on itself: its name and discovery, the seats and every row their Seat
/// defaults carry, and the updates. Every other row is the owner's host's.
fn door_row(id: &str) -> bool {
    pf_host_config::registry::find(id).is_some_and(|s| s.group == Group::Seats)
        || pf_host_config::seat_defaults::ROWS.contains(&id)
        || matches!(id, "host_name" | "mdns" | "update_check" | "update_apply")
}

/// Before the door switches off, the rows it kept go to the owner's host, which serves the box
/// again: what the console set on the door stays set. A row the door never stored stays the
/// owner's. `Err` while the owner's host can't take them yet.
pub(crate) async fn hand_door_rows_home(st: &MgmtState) -> Result<(), Response> {
    let rows: serde_json::Map<String, Value> = pf_host_config::snapshot()
        .settings
        .iter()
        .filter(|r| r.setting.available() && door_row(r.setting.id))
        .filter_map(|r| Some((r.setting.id.to_string(), r.stored.clone()?)))
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let body = Value::Object(rows).to_string().into_bytes();
    match st
        .owner_call("PATCH", "host/settings", Some(body), true)
        .await
    {
        answer @ (super::owner::Answer::Starting | super::owner::Answer::Down) => {
            Err(answer.into_response())
        }
        super::owner::Answer::Reply(200, ..) | super::owner::Answer::NoSeat => Ok(()),
        // An owner's host that won't take a row never keeps the box behind the door.
        _ => {
            tracing::warn!("door settings not handed to the owner's host");
            Ok(())
        }
    }
}

/// This host's state; on a door, its own rows and the owner's host's others.
async fn answer(st: &MgmtState) -> Response {
    let own = state(seats_on().await);
    if st.door.is_none() {
        return Json(own).into_response();
    }
    match st.owner_call("GET", "host/settings", None, true).await {
        super::owner::Answer::NoSeat => Json(own).into_response(),
        super::owner::Answer::Reply(200, _, body) => match with_owner_rows(own, &body) {
            Some(merged) => Json(merged).into_response(),
            None => api_error_coded(
                StatusCode::BAD_GATEWAY,
                "owner_settings_unreadable",
                "The owner's host answered with settings this console can't read.",
            ),
        },
        other => other.into_response(),
    }
}

/// The door's rows of `own` and the other rows of the owner's `owner`, in page order. A row is
/// pinned in the owner's `host.env`, so that is the file named.
fn with_owner_rows(own: HostSettingsState, owner: &[u8]) -> Option<Value> {
    let owner: Value = serde_json::from_slice(owner).ok()?;
    let own = serde_json::to_value(own).ok()?;
    let keep = |state: &Value, key: &str, door: bool| -> Vec<Value> {
        state
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|v| {
                let id = v.get("id").unwrap_or(v).as_str().unwrap_or_default();
                door_row(id) == door
            })
            .cloned()
            .collect()
    };
    let mut settings = [
        keep(&own, "settings", true),
        keep(&owner, "settings", false),
    ]
    .concat();
    let page = |row: &Value| {
        let id = row.get("id").and_then(Value::as_str);
        pf_host_config::registry::SETTINGS
            .iter()
            .position(|s| Some(s.id) == id)
    };
    settings.sort_by_key(page);
    let pending = [
        keep(&own, "restart_pending", true),
        keep(&owner, "restart_pending", false),
    ]
    .concat();
    Some(serde_json::json!({
        "settings": settings,
        "restart_pending": pending,
        "env_file": owner.get("env_file").cloned().unwrap_or(Value::Null),
    }))
}

/// Change host settings
///
/// Partial: only the named settings change, and `null` resets one. Every value is checked
/// before anything is written. Applies per setting's `apply`: at once, the next session, or a
/// restart. `tray_autostart` rewrites the tray's autostart entry here.
#[utoipa::path(
    patch,
    path = "/host/settings",
    tag = "host",
    operation_id = "patchHostSettings",
    request_body = HostSettingsPatch,
    responses(
        (status = OK, description = "Stored; the new state", body = HostSettingsState),
        (status = BAD_REQUEST, description = "An unknown setting or a value it does not accept", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The settings file could not be written", body = ApiError),
    )
)]
pub(crate) async fn patch_host_settings(
    State(st): State<Arc<MgmtState>>,
    ApiJson(patch): ApiJson<HostSettingsPatch>,
) -> Response {
    let door = st.door.is_some();
    let (patch, owner): (
        serde_json::Map<String, Value>,
        serde_json::Map<String, Value>,
    ) = patch
        .0
        .into_iter()
        .partition(|(id, _)| !door || door_row(id));
    if !owner.is_empty() {
        let body = Value::Object(owner.clone()).to_string().into_bytes();
        match st
            .owner_call("PATCH", "host/settings", Some(body), true)
            .await
        {
            super::owner::Answer::Reply(200, ..) => {}
            super::owner::Answer::NoSeat => {
                return super::owner::no_owner(StatusCode::SERVICE_UNAVAILABLE)
            }
            other => return other.into_response(),
        }
    }
    save(&st, patch).await
}

/// Store `patch` in this host's own file, then answer with the state.
async fn save(st: &MgmtState, patch: serde_json::Map<String, Value>) -> Response {
    if patch.is_empty() {
        return answer(st).await;
    }
    if let Some(id) = patch
        .keys()
        .find(|id| pf_host_config::registry::find(id).is_some_and(|s| !s.available()))
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            &format!("{id} does not apply on this host"),
        );
    }
    match pf_host_config::save(&patch) {
        Ok(()) => {}
        Err(e @ pf_host_config::SaveError::Io(_)) => {
            tracing::warn!(error = %e, "host settings not saved");
            return api_error_coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "host_settings_not_saved",
                "Couldn't save the host settings",
            );
        }
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
    if patch.contains_key("tray_autostart") {
        crate::tray_autostart::apply();
    }
    let ids: Vec<String> = patch.keys().cloned().collect();
    tracing::info!(settings = ?ids, "management API: host settings updated");
    crate::diagnostics::registry().set(crate::diagnostics::catalog::restart_pending());
    crate::events::emit(crate::events::EventKind::SettingsChanged { ids });
    write_seat_defaults();
    // Seats read their copy within a tick; a grant made in that tick stays until the box restarts.
    #[cfg(target_os = "windows")]
    if patch.contains_key("seat_background_input") {
        crate::windows::background_input::apply();
    }
    answer(st).await
}

/// Hands every seat the box's Seat defaults: at start, and after each settings write.
pub(crate) fn write_seat_defaults() {
    if let Err(error) = pf_host_config::seat_defaults::write(&pf_paths::config_dir()) {
        tracing::warn!(%error, "seat defaults not written");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seats' copies come from the door's own file, so the door keeps every row they carry.
    #[test]
    fn every_seat_default_is_a_door_row() {
        for id in pf_host_config::seat_defaults::ROWS {
            assert!(pf_host_config::registry::find(id).is_some(), "{id}");
            assert!(door_row(id), "{id}");
        }
        assert!(!door_row("encoder"));
    }

    /// A door shows its own name and discovery, and the owner's host's value for what streams,
    /// pinned in the owner's `host.env`.
    #[test]
    fn a_door_shows_its_own_rows_and_the_owner_s_others() {
        let own = state(SeatsOn::default());
        let mut owner = serde_json::to_value(state(SeatsOn::default())).unwrap();
        for row in owner["settings"].as_array_mut().unwrap() {
            row["title"] = "owner".into();
        }
        owner["restart_pending"] = serde_json::json!(["mdns", "idle_timeout_ms"]);
        owner["env_file"] = "/home/me/.config/punktfunk/host.env".into();
        let merged = with_owner_rows(own, owner.to_string().as_bytes()).unwrap();
        let title = |id: &str| {
            merged["settings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == id)
                .map(|row| row["title"].clone())
        };
        assert_eq!(title("mdns"), Some("Local discovery".into()));
        assert_eq!(title("host_name"), Some("Host name".into()));
        assert_eq!(title("idle_timeout_ms"), Some("owner".into()));
        assert_eq!(
            merged["restart_pending"],
            serde_json::json!(["idle_timeout_ms"])
        );
        assert_eq!(merged["env_file"], "/home/me/.config/punktfunk/host.env");
    }
}
