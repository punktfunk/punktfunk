//! The people on this box (`design/profiles-and-seats.md` §7).
//!
//! Three routes are on the cert lane, for every paired device: the list a client's picker
//! shows, a profile's picture, and waking its seat. Everything that changes a profile is the
//! console's, among them turning seats on. A profile is not a trust boundary: any paired
//! device may pick any profile.

use super::auth::PairedDevice;
use super::shared::*;
use crate::profiles::{
    EditError, Home, OsAccount, Profile, ProfileCreate, ProfileUpdate, Profiles,
};
use axum::http::header;
use axum::Extension;
use pf_seats::ipc::{Command, CommandResult, Diagnostic, DiagnosticLevel, ErrorCode};

/// One profile as a client's picker shows it.
#[derive(Serialize, ToSchema, Clone)]
pub(crate) struct ProfilePublic {
    #[schema(example = "9a3f1c2b7e40")]
    id: String,
    display_name: String,
    /// `#RRGGBB` behind the initials when there is no picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    accent: Option<String>,
    /// Host-relative URL of the picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    avatar: Option<String>,
    /// The box's own session.
    owner: bool,
    /// What a bare connect opens.
    home: Home,
    /// Its own seat; absent when it plays on the box's own session.
    #[serde(skip_serializing_if = "Option::is_none")]
    seat: Option<SeatPublic>,
    /// When a session last played as it; `0`: never.
    last_used_unix: u64,
    /// The asking device's old seat became this profile. A client takes it without a picker.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    legacy_seat: bool,
}

/// A profile's seat right now.
#[derive(Serialize, ToSchema, Clone)]
pub(crate) struct SeatPublic {
    state: SeatState,
    /// The progress line while `starting`, or why while `unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// The port to dial for this profile.
    port: u16,
    /// The device playing on it while `occupied`.
    #[serde(skip_serializing_if = "Option::is_none")]
    occupant: Option<String>,
    /// Its Steam has no account yet: the first connect shows Steam's sign-in. `null` where the
    /// host can't tell.
    steam_sign_in: Option<bool>,
}

#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SeatState {
    Ready,
    Starting,
    Stopped,
    Occupied,
    Unavailable,
}

/// One profile as the console manages it.
#[derive(Serialize, ToSchema)]
pub(crate) struct ProfileAdmin {
    #[serde(flatten)]
    public: ProfilePublic,
    /// The device whose seat became this profile, lowercase hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_device: Option<String>,
    /// Where a device that names no profile lands.
    default: bool,
}

/// `PUT /profiles/default`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DefaultProfile {
    /// `null` is the owner.
    id: Option<String>,
}

/// `DELETE /profiles/{id}` options.
#[derive(Deserialize, utoipa::IntoParams)]
pub(crate) struct DeleteQuery {
    /// Also delete its Steam home and picture.
    #[serde(default)]
    erase: bool,
}

fn no_profiles() -> Response {
    api_error(StatusCode::NOT_FOUND, "this host has no profiles")
}

fn public(st: &MgmtState, p: &Profile, owner: Option<&str>, device: Option<&str>) -> ProfilePublic {
    let is_owner = owner == Some(p.id.as_str());
    ProfilePublic {
        id: p.id.clone(),
        display_name: p.display_name.clone(),
        accent: p.accent.clone(),
        avatar: p.avatar.clone(),
        owner: is_owner,
        home: p.home,
        seat: (!matches!(p.os_account, OsAccount::Operator)).then(|| seat(st, p)),
        last_used_unix: p.last_used_unix,
        legacy_seat: device
            .zip(p.legacy_device.as_deref())
            .is_some_and(|(d, l)| d.eq_ignore_ascii_case(l)),
    }
}

/// A light seat starts with its first connect, so it is always ready to dial.
fn seat(st: &MgmtState, p: &Profile) -> SeatPublic {
    let port = st.app.native_port.get().copied().unwrap_or(0);
    let unavailable = |why: &str| SeatPublic {
        state: SeatState::Unavailable,
        detail: Some(why.to_string()),
        port,
        occupant: None,
        steam_sign_in: None,
    };
    match &p.os_account {
        OsAccount::Seat { .. } | OsAccount::Operator => SeatPublic {
            state: SeatState::Ready,
            detail: None,
            port,
            occupant: None,
            steam_sign_in: steam_sign_in(&p.id),
        },
        OsAccount::Linux { .. } | OsAccount::Windows { .. } => {
            unavailable("This profile signs in to an account this host can't start yet.")
        }
    }
}

/// Whether the seat home still needs a Steam account. `None` with **Steam per seat** off.
fn steam_sign_in(id: &str) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        pf_host_config::config().steam_seat_home.then(|| {
            crate::vdisplay::seat_needs_sign_in(&crate::vdisplay::SessionIsolation {
                id: String::new(),
                ei_relay: std::path::PathBuf::new(),
                sink: None,
                mic_source: None,
                steam_home: Some(pf_paths::seat_home(id)),
            })
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        None
    }
}

fn admin(st: &MgmtState, profiles: &Profiles, p: &Profile) -> ProfileAdmin {
    let owner = profiles.owner_id();
    ProfileAdmin {
        public: public(st, p, owner.as_deref(), None),
        legacy_device: p.legacy_device.clone(),
        default: profiles.default_profile_id().as_deref() == Some(p.id.as_str()),
    }
}

fn edit_error(e: EditError) -> Response {
    let status = match e {
        EditError::NotFound => StatusCode::NOT_FOUND,
        EditError::Invalid(_) => StatusCode::BAD_REQUEST,
        EditError::NameTaken | EditError::Owner => StatusCode::CONFLICT,
        EditError::Full => StatusCode::UNPROCESSABLE_ENTITY,
        EditError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    api_error(status, &e.to_string())
}

/// List the profiles a device can pick
///
/// Every profile on the box, the owner first. A client shows this as its picker.
#[utoipa::path(
    get,
    path = "/profiles/enumerate",
    tag = "profiles",
    operation_id = "enumerateProfiles",
    responses(
        (status = OK, description = "The profiles", body = Vec<ProfilePublic>),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = NOT_FOUND, description = "This host has no profiles", body = ApiError),
    )
)]
pub(crate) async fn enumerate_profiles(
    State(st): State<Arc<MgmtState>>,
    device: Option<Extension<PairedDevice>>,
) -> Response {
    let device = device.map(|Extension(PairedDevice(fp))| fp);
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    let owner = profiles.owner_id();
    let rows: Vec<ProfilePublic> = profiles
        .list()
        .iter()
        .map(|p| public(&st, p, owner.as_deref(), device.as_deref()))
        .collect();
    Json(rows).into_response()
}

/// A profile's picture
///
/// The stored image with an `ETag` of its bytes; a request whose `If-None-Match` names that tag
/// gets 304 with no body.
#[utoipa::path(
    get,
    path = "/profiles/{id}/avatar",
    tag = "profiles",
    operation_id = "getProfileAvatar",
    params(
        ("id" = String, Path, description = "The profile id"),
        ("If-None-Match" = Option<String>, Header, description = "An `ETag` from an earlier response"),
    ),
    responses(
        (status = OK, description = "Image bytes", content_type = "image/png"),
        (status = NOT_MODIFIED, description = "The tag in `If-None-Match` is current"),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = NOT_FOUND, description = "No picture for that profile", body = ApiError),
    )
)]
pub(crate) async fn get_profile_avatar(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(profiles) = st.app.profiles.get().cloned() else {
        return no_profiles();
    };
    let found = tokio::task::spawn_blocking(move || profiles.avatar(&id)).await;
    let Ok(Some(avatar)) = found else {
        return api_error(StatusCode::NOT_FOUND, "no picture for that profile");
    };
    let etag = format!("\"{}\"", avatar.etag);
    let cache = [
        (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
        (header::ETAG, etag.clone()),
    ];
    let sent = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok());
    if super::library::not_modified(sent, &etag) {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    let [cc, et] = cache;
    (
        [cc, et, (header::CONTENT_TYPE, avatar.mime.to_string())],
        avatar.bytes,
    )
        .into_response()
}

/// Get a profile's seat ready
///
/// Starts a stopped seat so the next connect lands in it. A light seat starts with its first
/// connect, so on Linux this only answers the row.
#[utoipa::path(
    post,
    path = "/profiles/{id}/wake",
    tag = "profiles",
    operation_id = "wakeProfile",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The profile, its seat as it is now", body = ProfilePublic),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
    )
)]
pub(crate) async fn wake_profile(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    match profiles.get(&id) {
        Some(p) => Json(public(&st, &p, profiles.owner_id().as_deref(), None)).into_response(),
        None => api_error(StatusCode::NOT_FOUND, "no profile with that id"),
    }
}

/// List the profiles with what the console manages
#[utoipa::path(
    get,
    path = "/profiles",
    tag = "profiles",
    operation_id = "listProfiles",
    responses(
        (status = OK, description = "The profiles", body = Vec<ProfileAdmin>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "This host has no profiles", body = ApiError),
    )
)]
pub(crate) async fn list_profiles(State(st): State<Arc<MgmtState>>) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    let rows: Vec<ProfileAdmin> = profiles
        .list()
        .iter()
        .map(|p| admin(&st, profiles, p))
        .collect();
    Json(rows).into_response()
}

/// Add a profile
///
/// A seat profile (the default) plays in a seat of its own; one with `seat: false` plays the
/// box's own session under its own name.
#[utoipa::path(
    post,
    path = "/profiles",
    tag = "profiles",
    operation_id = "createProfile",
    request_body = ProfileCreate,
    responses(
        (status = CREATED, description = "The new profile", body = ProfileAdmin),
        (status = BAD_REQUEST, description = "No name, or an accent that is not #RRGGBB", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "Another profile has that name", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "The box already has eight profiles", body = ApiError),
    )
)]
pub(crate) async fn create_profile(
    State(st): State<Arc<MgmtState>>,
    ApiJson(input): ApiJson<ProfileCreate>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    match profiles.create(input) {
        Ok(p) => (StatusCode::CREATED, Json(admin(&st, profiles, &p))).into_response(),
        Err(e) => edit_error(e),
    }
}

/// Rename, recolour or rehome a profile
#[utoipa::path(
    put,
    path = "/profiles/{id}",
    tag = "profiles",
    operation_id = "updateProfile",
    params(("id" = String, Path, description = "The profile id")),
    request_body = ProfileUpdate,
    responses(
        (status = OK, description = "The profile", body = ProfileAdmin),
        (status = BAD_REQUEST, description = "An empty name, or an accent that is not #RRGGBB", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "Another profile has that name", body = ApiError),
    )
)]
pub(crate) async fn update_profile(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    ApiJson(input): ApiJson<ProfileUpdate>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    match profiles.update(&id, input) {
        Ok(p) => Json(admin(&st, profiles, &p)).into_response(),
        Err(e) => edit_error(e),
    }
}

/// Remove a profile
///
/// Sessions playing as it end with `SEAT_UNAVAILABLE`. With `erase`, its Steam home and picture
/// go too; without, the home stays.
#[utoipa::path(
    delete,
    path = "/profiles/{id}",
    tag = "profiles",
    operation_id = "deleteProfile",
    params(("id" = String, Path, description = "The profile id"), DeleteQuery),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "The owner profile can't be removed", body = ApiError),
    )
)]
pub(crate) async fn delete_profile(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    match profiles.delete(&id, q.erase) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

/// Set a profile's picture
///
/// PNG, JPEG or WebP, at most 1 MiB. The console crops it square first.
#[utoipa::path(
    put,
    path = "/profiles/{id}/avatar",
    tag = "profiles",
    operation_id = "setProfileAvatar",
    params(("id" = String, Path, description = "The profile id")),
    request_body(content = Vec<u8>, content_type = "image/png"),
    responses(
        (status = NO_CONTENT, description = "Stored"),
        (status = BAD_REQUEST, description = "Not a PNG, JPEG or WebP image, or over 1 MiB", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
    )
)]
pub(crate) async fn set_profile_avatar(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let Some(profiles) = st.app.profiles.get().cloned() else {
        return no_profiles();
    };
    if profiles.get(&id).is_none() {
        return api_error(StatusCode::NOT_FOUND, "no profile with that id");
    }
    let stored = tokio::task::spawn_blocking(move || profiles.put_avatar(&id, &body)).await;
    match stored {
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Remove a profile's picture
#[utoipa::path(
    delete,
    path = "/profiles/{id}/avatar",
    tag = "profiles",
    operation_id = "deleteProfileAvatar",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = NO_CONTENT, description = "Removed"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
    )
)]
pub(crate) async fn delete_profile_avatar(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    if profiles.get(&id).is_none() {
        return api_error(StatusCode::NOT_FOUND, "no profile with that id");
    }
    match profiles.remove_avatar(&id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:#}")),
    }
}

/// Where a device that names no profile lands
#[utoipa::path(
    put,
    path = "/profiles/default",
    tag = "profiles",
    operation_id = "setDefaultProfile",
    request_body = DefaultProfile,
    responses(
        (status = NO_CONTENT, description = "Set"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
    )
)]
pub(crate) async fn set_default_profile(
    State(st): State<Arc<MgmtState>>,
    ApiJson(input): ApiJson<DefaultProfile>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    match profiles.set_default(input.id.as_deref()) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}

/// What a seat check found. An `error` stops seats from turning on.
#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckLevel {
    Info,
    Warning,
    Error,
}

/// One thing the seat checks found.
#[derive(Serialize, ToSchema)]
pub(crate) struct SeatCheck {
    level: CheckLevel,
    /// A stable id such as `rds_role`.
    #[schema(example = "rds_role")]
    code: String,
    /// One plain sentence for the operator.
    message: String,
    /// The seat it is about; absent when it is about the box.
    #[serde(skip_serializing_if = "Option::is_none")]
    seat_id: Option<String>,
}

impl From<Diagnostic> for SeatCheck {
    fn from(d: Diagnostic) -> Self {
        Self {
            level: match d.level {
                DiagnosticLevel::Info => CheckLevel::Info,
                DiagnosticLevel::Warning => CheckLevel::Warning,
                DiagnosticLevel::Error => CheckLevel::Error,
            },
            code: d.code,
            message: d.message,
            seat_id: d.seat_id.map(|id| id.to_string()),
        }
    }
}

#[derive(Serialize, ToSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SeatingPlatform {
    Windows,
    Other,
}

/// Whether this box runs its profiles in seats of their own, with what turning that on needs.
#[derive(Serialize, ToSchema)]
pub(crate) struct Seating {
    enabled: bool,
    /// `other` has no seats to turn on.
    platform: SeatingPlatform,
    /// The prerequisites, one each. Empty on `other`.
    checks: Vec<SeatCheck>,
}

/// `PUT /profiles/seating`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct SeatingChange {
    enabled: bool,
    /// Leave Remote Desktop reachable from the network. Off keeps it on this machine.
    #[serde(default)]
    #[schema(required = false)]
    allow_rdp_from_network: bool,
}

/// The seat supervisor's own checks.
#[derive(Serialize, ToSchema)]
pub(crate) struct SeatsDoctor {
    /// No check is an `error`.
    healthy: bool,
    diagnostics: Vec<SeatCheck>,
}

const SEATS_NEED_WINDOWS: &str = "Seats need Windows Server.";

/// One request to the seat supervisor on a blocking thread, or the response that says why not.
async fn supervisor(command: Command) -> Result<CommandResult, Response> {
    match tokio::task::spawn_blocking(move || crate::seats::call(command)).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(e)) => {
            tracing::warn!(code = ?e.code, error = %e.message, "seat supervisor request failed");
            Err(if e.code == ErrorCode::Transport {
                api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Couldn't reach the seat supervisor. Restart the Punktfunk service, then try again.",
                )
            } else {
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Couldn't change seats on this machine. Check the host log.",
                )
            })
        }
        Err(e) => Err(api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())),
    }
}

async fn seating(command: Command) -> Response {
    match supervisor(command).await {
        Ok(CommandResult::Seating { status }) => Json(Seating {
            enabled: status.enabled,
            platform: SeatingPlatform::Windows,
            checks: status.checks.into_iter().map(SeatCheck::from).collect(),
        })
        .into_response(),
        Ok(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The seat supervisor answered something unexpected.",
        ),
        Err(response) => response,
    }
}

/// Whether seats are on
///
/// Windows reports whether the box's seats are on and what turning them on needs: Windows
/// Server, the Remote Desktop Session Host role, licensing and a graphics card. Other
/// platforms answer `enabled: false` with `platform: other`. Admin lane only.
#[utoipa::path(
    get,
    path = "/profiles/seating",
    tag = "profiles",
    operation_id = "getSeating",
    responses(
        (status = OK, description = "Seats now", body = Seating),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor doesn't answer", body = ApiError),
    )
)]
pub(crate) async fn get_seating() -> Response {
    if !cfg!(windows) {
        return Json(Seating {
            enabled: false,
            platform: SeatingPlatform::Other,
            checks: Vec::new(),
        })
        .into_response();
    }
    seating(Command::Seating).await
}

/// Turn seats on or off
///
/// On runs the checks, installs the seat display driver, turns Remote Desktop on for this
/// machine and records its certificate. A check that fails changes nothing: the answer is 200
/// with `enabled: false` and the failed check as an `error`. The driver replaces the display of
/// every Remote Desktop session on the machine. Off stops every seat, keeps their accounts
/// and restores what on changed. Admin lane only.
#[utoipa::path(
    put,
    path = "/profiles/seating",
    tag = "profiles",
    operation_id = "setSeating",
    request_body = SeatingChange,
    responses(
        (status = OK, description = "Seats as they are after the change", body = Seating),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "This host has no seats to turn on", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor doesn't answer", body = ApiError),
    )
)]
pub(crate) async fn put_seating(ApiJson(input): ApiJson<SeatingChange>) -> Response {
    if !cfg!(windows) {
        return api_error(StatusCode::CONFLICT, SEATS_NEED_WINDOWS);
    }
    seating(if input.enabled {
        Command::Enable {
            allow_rdp_from_network: input.allow_rdp_from_network,
        }
    } else {
        Command::Disable {
            keep_accounts: true,
        }
    })
    .await
}

/// Check the seats
///
/// The supervisor's own report: the ledger, Windows, Remote Desktop, the certificate pin and
/// each seat's account, session and ports. Admin lane only.
#[utoipa::path(
    get,
    path = "/profiles/doctor",
    tag = "profiles",
    operation_id = "getSeatsDoctor",
    responses(
        (status = OK, description = "The report", body = SeatsDoctor),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "This host has no seats to check", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor doesn't answer", body = ApiError),
    )
)]
pub(crate) async fn get_seats_doctor() -> Response {
    if !cfg!(windows) {
        return api_error(StatusCode::CONFLICT, SEATS_NEED_WINDOWS);
    }
    match supervisor(Command::Doctor).await {
        Ok(CommandResult::Doctor { report }) => Json(SeatsDoctor {
            healthy: report.healthy,
            diagnostics: report
                .diagnostics
                .into_iter()
                .map(SeatCheck::from)
                .collect(),
        })
        .into_response(),
        Ok(_) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "The seat supervisor answered something unexpected.",
        ),
        Err(response) => response,
    }
}
