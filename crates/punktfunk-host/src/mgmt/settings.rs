//! `/host/settings` — the [`pf_host_config::registry`] rows the console edits.
//!
//! GET carries everything the page renders: each row's kind and bounds, the value in
//! force, the console's own value, and what set it. A row this host does not act on is
//! left out, not flagged. PATCH merges into `host-settings.json`; a value an env var or
//! CLI flag overrides is still stored, and takes effect once that override is gone.

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

pub(crate) fn state() -> HostSettingsState {
    let snap = pf_host_config::snapshot();
    let pending = pf_host_config::restart_pending();
    let settings = snap
        .settings
        .iter()
        .filter(|r| r.setting.available())
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
pub(crate) async fn get_host_settings() -> Json<HostSettingsState> {
    Json(state())
}

/// Change host settings
///
/// Partial: only the named settings change, and `null` resets one. Every value is checked
/// before anything is written. Applies per setting's `apply`: the next session, or a restart.
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
pub(crate) async fn patch_host_settings(ApiJson(patch): ApiJson<HostSettingsPatch>) -> Response {
    let patch: serde_json::Map<String, Value> = patch.0.into_iter().collect();
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
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Couldn't save the host settings — {e}"),
            );
        }
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &e.to_string()),
    }
    let ids: Vec<String> = patch.keys().cloned().collect();
    tracing::info!(settings = ?ids, "management API: host settings updated");
    crate::diagnostics::registry().set(crate::diagnostics::catalog::restart_pending());
    crate::events::emit(crate::events::EventKind::SettingsChanged { ids });
    Json(state()).into_response()
}
