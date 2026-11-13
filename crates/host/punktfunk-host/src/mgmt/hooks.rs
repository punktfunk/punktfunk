//! Read and replace `hooks.json`. Validated on write; the runner rereads the store per event
//! so a PUT applies without a restart. On a person's own account they are that person's alone,
//! written and read on their own lane ([`AuthLane::Person`]).

use super::auth::AuthLane;
use super::shared::*;
use axum::Extension;

/// Whether `lane` may read or write the hooks here: anyone the route lets in, but on a person's
/// own account their own lane alone.
fn theirs(lane: AuthLane) -> bool {
    !pf_paths::seat::own_account() || lane == AuthLane::Person
}

/// Whether `cfg`, written on `lane` of a `windows` seat host or a Linux one, names a secret file
/// its host would read for the person: on Windows the host is SYSTEM.
fn refuses_secret_file(windows: bool, lane: AuthLane, cfg: &crate::hooks::HooksConfig) -> bool {
    windows && lane == AuthLane::Person && cfg.hooks.iter().any(|h| h.hmac_secret_file.is_some())
}

/// What any other lane reads where the person's lane is open: the code of its host message
/// `apierr_<code>`, and its English. Where it is closed, [`crate::hooks::own_account_refusal`].
const SET_BY_PERSON: (&str, &str) = (
    "hooks_own_account_person",
    "Hooks on a profile that uses an existing account are set by that person, from their own \
     desktop.",
);

/// What a person on a Windows seat reads for a hook that names a secret file: their seat host is
/// SYSTEM, and would read any file as the key of a webhook they direct.
const PERSON_SECRET_FILE: (&str, &str) = (
    "hooks_secret_file_person",
    "Hooks you set yourself can't sign a webhook with a secret file on this PC. Remove the \
     secret file, then save again.",
);

/// Get the hook configuration
///
/// Empty document when none is stored. On a seat host on a person's own account, empty on
/// every lane but that person's own: their hooks may carry their webhooks' secrets.
#[utoipa::path(
    get,
    path = "/hooks",
    tag = "hooks",
    operation_id = "getHooks",
    responses(
        (status = OK, description = "The stored hook configuration", body = crate::hooks::HooksConfig),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_hooks(
    Extension(lane): Extension<AuthLane>,
) -> Json<crate::hooks::HooksConfig> {
    if !theirs(lane) {
        return Json(Default::default());
    }
    Json(crate::hooks::store().get())
}

/// Replace the hook configuration
///
/// Whole-document PUT, not a patch. Applies from the next event. Commands run as the host
/// user (the interactive session on Windows). An `elevated` hook is refused until an
/// administrator turns elevated commands on at the host (`HostInfo.elevated_commands`). A
/// seat host on a person's own account takes hooks from that person's own session alone, and
/// runs only those. On a Windows seat that session names no `hmac_secret_file`.
#[utoipa::path(
    put,
    path = "/hooks",
    tag = "hooks",
    operation_id = "setHooks",
    request_body = crate::hooks::HooksConfig,
    responses(
        (status = OK, description = "Configuration stored; the new state", body = crate::hooks::HooksConfig),
        (status = BAD_REQUEST, description = "Structurally invalid configuration, an `elevated` hook this host won't run, or a person's secret file on Windows", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "A seat host on a person's own account, from any lane but that person's", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Configuration could not be persisted", body = ApiError),
    )
)]
pub(crate) async fn set_hooks(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    ApiJson(cfg): ApiJson<crate::hooks::HooksConfig>,
) -> Response {
    if !theirs(lane) {
        if let Some(why) = crate::hooks::own_account_refusal() {
            let (code, why) = match st.person {
                Some(_) => SET_BY_PERSON,
                None => ("hooks_own_account", why),
            };
            return api_error_coded(StatusCode::FORBIDDEN, code, why);
        }
    }
    if let Err(e) = cfg.validate() {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    if refuses_secret_file(cfg!(windows), lane, &cfg) {
        let (code, why) = PERSON_SECRET_FILE;
        return api_error_coded(StatusCode::BAD_REQUEST, code, why);
    }
    if let Some(why) = cfg
        .hooks
        .iter()
        .any(|h| h.elevated)
        .then(crate::hooks::elevated_refused)
        .flatten()
    {
        return api_error_coded(StatusCode::BAD_REQUEST, why.0, why.1);
    }
    match crate::hooks::store().set(cfg, lane == AuthLane::Person) {
        Ok(()) => {
            tracing::info!("management API: hook configuration updated");
            Json(crate::hooks::store().get()).into_response()
        }
        Err(e) => api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "hooks_not_saved",
            "Couldn't save the hooks",
            &format!("{e:#}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A person on a Windows seat names no secret file; the operator there, and anyone on a
    /// Linux seat, whose host runs as the account, may.
    #[test]
    fn a_windows_person_names_no_secret_file() {
        let with = |file: Option<&str>| -> crate::hooks::HooksConfig {
            let hook = serde_json::json!({"on": "game.*", "webhook": "https://x.example/h",
                "hmac_secret_file": file});
            serde_json::from_value(serde_json::json!({ "hooks": [hook] })).unwrap()
        };
        let secret = with(Some(r"C:\ProgramData\punktfunk\mgmt-token"));
        assert!(refuses_secret_file(true, AuthLane::Person, &secret));
        assert!(!refuses_secret_file(true, AuthLane::Admin, &secret));
        assert!(!refuses_secret_file(false, AuthLane::Person, &secret));
        assert!(!refuses_secret_file(true, AuthLane::Person, &with(None)));
    }
}
