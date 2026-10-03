//! `/downloads` and `/library/install/{id}`: titles a plugin installs on this host. The plugin
//! moves the bytes and reports progress here; the operator installs, pauses, cancels and
//! removes, and a paired device allowed to launch may install, since launching would anyway.
use super::auth::{AuthLane, OwnedId, PairedDevice, ProviderId};
use super::shared::*;
use crate::library::downloads::{self, Action, Download, DownloadReport, Refusal};
use axum::Extension;

/// A plugin's downloads: every title it is downloading, paused or just finished.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DownloadsReport {
    downloads: Vec<DownloadReport>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DownloadsAccepted {
    /// Rows naming one of the provider's titles.
    matched: usize,
    /// Rows naming a title the provider doesn't list (the report raced a reconcile).
    unknown: usize,
}

/// Report a provider's downloads
///
/// The body restates every title the plugin is downloading, has paused, or just finished,
/// keyed by `external_id`. Send it on change at most once a second, and every 5 s while any
/// row is `queued`, `downloading` or `installing`: a row not restated for 30 s counts as
/// stalled, and a launch waiting on it gives up.
#[utoipa::path(
    put,
    path = "/library/provider/{provider}/downloads",
    tag = "library",
    operation_id = "reportProviderDownloads",
    params(("provider" = String, Path, description = "The provider id ([a-z0-9._-], `manual` reserved)")),
    request_body = DownloadsReport,
    responses(
        (status = OK, description = "The report was applied", body = DownloadsAccepted),
        (status = BAD_REQUEST, description = "Invalid provider id or payload", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "A plugin reported another plugin's titles", body = ApiError),
    )
)]
pub(crate) async fn report_provider_downloads(
    OwnedId(provider, _): OwnedId<ProviderId>,
    ApiJson(input): ApiJson<DownloadsReport>,
) -> Response {
    let sent = input.downloads.len();
    let matched = downloads::report(&provider, input.downloads);
    Json(DownloadsAccepted {
        matched,
        unknown: sent - matched,
    })
    .into_response()
}

/// List downloads
///
/// Every title downloading, queued, paused, or finished in the last ten minutes, live ones
/// first, with speed and time left while downloading.
#[utoipa::path(
    get,
    path = "/downloads",
    tag = "library",
    operation_id = "getDownloads",
    responses(
        (status = OK, description = "The downloads", body = [Download]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_downloads() -> Response {
    Json(downloads::snapshot()).into_response()
}

/// Install a title
///
/// Asks the plugin that lists the title to download it, or to resume a paused download. The
/// operator always may; a paired device may when it holds the launch grant. Answers the
/// download's row.
#[utoipa::path(
    post,
    path = "/library/install/{id}",
    tag = "library",
    operation_id = "installLibraryEntry",
    params(("id" = String, Path, description = "The library entry id")),
    responses(
        (status = ACCEPTED, description = "The plugin started or resumed the download", body = Download),
        (status = FORBIDDEN, description = "This device may not launch titles", body = ApiError),
        (status = NOT_FOUND, description = "No such title", body = ApiError),
        (status = CONFLICT, description = "Already installed, not a title a plugin installs, or the plugin refused (its sentence)", body = ApiError),
        (status = BAD_GATEWAY, description = "The plugin didn't answer", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn install_library_entry(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    device: Option<Extension<PairedDevice>>,
    Path(id): Path<String>,
) -> Response {
    let fp = device.as_ref().map(|e| e.0 .0.clone());
    let by = match lane {
        AuthLane::Admin => "console".to_string(),
        AuthLane::Cert if launch_permitted(&st, fp.as_deref()) => fp
            .as_deref()
            .map(|fp| fp.chars().take(12).collect())
            .unwrap_or_default(),
        _ => {
            return api_error(
                StatusCode::FORBIDDEN,
                "This device isn't allowed to start games on this host.",
            )
        }
    };
    act(id, Action::Start, Some(by)).await
}

/// Pause a title's download
///
/// The plugin stops and keeps what it has; installing again resumes.
#[utoipa::path(
    post,
    path = "/library/install/{id}/pause",
    tag = "library",
    operation_id = "pauseLibraryInstall",
    params(("id" = String, Path, description = "The library entry id")),
    responses(
        (status = NO_CONTENT, description = "Paused"),
        (status = NOT_FOUND, description = "No such title", body = ApiError),
        (status = CONFLICT, description = "Not downloading, or the plugin refused (its sentence)", body = ApiError),
        (status = BAD_GATEWAY, description = "The plugin didn't answer", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn pause_library_install(Path(id): Path<String>) -> Response {
    act(id, Action::Pause, None).await
}

/// Cancel a title's download
///
/// The plugin stops and discards what it downloaded.
#[utoipa::path(
    post,
    path = "/library/install/{id}/cancel",
    tag = "library",
    operation_id = "cancelLibraryInstall",
    params(("id" = String, Path, description = "The library entry id")),
    responses(
        (status = NO_CONTENT, description = "Cancelled"),
        (status = NOT_FOUND, description = "No such title", body = ApiError),
        (status = CONFLICT, description = "Not downloading or paused, or the plugin refused (its sentence)", body = ApiError),
        (status = BAD_GATEWAY, description = "The plugin didn't answer", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn cancel_library_install(Path(id): Path<String>) -> Response {
    act(id, Action::Cancel, None).await
}

/// Remove a title's files
///
/// The plugin deletes what it downloaded for the title; saves stay. Refused while the title
/// runs or a launch waits for it.
#[utoipa::path(
    delete,
    path = "/library/install/{id}",
    tag = "library",
    operation_id = "uninstallLibraryEntry",
    params(("id" = String, Path, description = "The library entry id")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = NOT_FOUND, description = "No such title", body = ApiError),
        (status = CONFLICT, description = "Running, downloading, not installed, or the plugin refused (its sentence)", body = ApiError),
        (status = BAD_GATEWAY, description = "The plugin didn't answer", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn uninstall_library_entry(Path(id): Path<String>) -> Response {
    act(id, Action::Uninstall, None).await
}

/// A paired device whose live mask carries the launch grant.
fn launch_permitted(st: &MgmtState, fp: Option<&str>) -> bool {
    fp.is_some_and(|fp| {
        st.native
            .as_ref()
            .and_then(|n| n.effective(fp, crate::clock::unix_secs()))
            .is_some_and(|mask| mask & punktfunk_core::quic::GRANT_LAUNCH != 0)
    })
}

fn conflict(message: &str) -> Response {
    api_error(StatusCode::CONFLICT, message)
}

async fn act(id: String, action: Action, by: Option<String>) -> Response {
    let Some(entry) = crate::library::entry_for_library_id(&id) else {
        return api_error(StatusCode::NOT_FOUND, "That title isn't in the library.");
    };
    let (Some(provider), Some(external)) = (entry.provider.clone(), entry.external_id.clone())
    else {
        return conflict("That title isn't one a plugin installs.");
    };
    let title = entry.title.clone();
    let missing = entry.install.as_ref().is_some_and(|i| i.missing());
    let pending = downloads::pending(&id);
    let refused = match action {
        Action::Start if !missing && !pending => Some("That title is already on this host.".into()),
        Action::Pause if !downloads::get(&id).is_some_and(|d| d.state.live()) => {
            Some("That title isn't downloading.".into())
        }
        Action::Cancel if !pending => Some("That title isn't downloading.".into()),
        Action::Uninstall if entry.install.is_none() => {
            Some("That title's files aren't ones its plugin can remove.".into())
        }
        Action::Uninstall if pending => {
            Some("That title is still downloading. Cancel the download instead.".into())
        }
        Action::Uninstall if missing => Some("That title isn't on this host.".into()),
        Action::Uninstall if in_use(&id) => Some(format!("Quit {title} first.")),
        _ => None,
    };
    if let Some(message) = refused {
        return conflict(&message);
    }
    let call = {
        let (provider, id, external) = (provider.clone(), id.clone(), external.clone());
        tokio::task::spawn_blocking(move || downloads::call(&provider, &id, &external, action))
            .await
    };
    match call {
        Ok(Ok(())) => {}
        Ok(Err(Refusal::NotServed)) => {
            return conflict(
                "The plugin that lists this title isn't running, or can't install titles.",
            )
        }
        Ok(Err(Refusal::NotMine)) => {
            return conflict("The plugin that listed this title doesn't know it any more.")
        }
        Ok(Err(Refusal::Said(message))) => return conflict(&message),
        Ok(Err(Refusal::Unreachable(e))) => {
            tracing::warn!(provider = %provider, action = ?action, error = %e, "plugin install call did not answer");
            return api_error(StatusCode::BAD_GATEWAY, "The plugin didn't answer.");
        }
        Err(e) => {
            tracing::error!("install worker panicked: {e}");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The install stopped responding.",
            );
        }
    }
    match action {
        Action::Start => {
            downloads::begin(&id, &title, &provider, &external, by);
            let row = downloads::get(&id);
            (StatusCode::ACCEPTED, Json(row)).into_response()
        }
        Action::Uninstall => {
            downloads::removed(&id, &title);
            StatusCode::NO_CONTENT.into_response()
        }
        Action::Pause | Action::Cancel => StatusCode::NO_CONTENT.into_response(),
    }
}

/// A session plays the title, waits to launch it, or it runs on with no session.
fn in_use(id: &str) -> bool {
    crate::session_status::games()
        .iter()
        .any(|g| g.app_id.as_deref() == Some(id))
}
