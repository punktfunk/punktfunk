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
use crate::seats::Snapshot;
use axum::http::header;
use axum::Extension;
use pf_seats::ipc::{
    ApiError as SeatError, Command, CommandResult, Diagnostic, DiagnosticLevel, ErrorCode,
};
use pf_seats::RuntimeState;

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
    /// Which kind of seat it is.
    kind: SeatKind,
}

/// What a profile's seat is. A door places every profile on a seat, so a profile on the box's own
/// session has one too: the owner's.
#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SeatKind {
    /// The box's own session under another name; only on a door, which places it on the
    /// owner's seat.
    Shared,
    /// A Steam of its own in the owner's session.
    Steam,
    /// A desktop of its own: a seat with its own user and host.
    Desktop,
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

fn public(
    st: &MgmtState,
    p: &Profile,
    owner: Option<&str>,
    device: Option<&str>,
    seats: &Snapshot,
) -> ProfilePublic {
    let is_owner = owner == Some(p.id.as_str());
    ProfilePublic {
        id: p.id.clone(),
        display_name: p.display_name.clone(),
        accent: p.accent.clone(),
        avatar: p.avatar.clone(),
        owner: is_owner,
        home: p.home,
        // A door plays every profile on a seat, the owner's included.
        seat: (crate::seats::is_door() || !matches!(p.os_account, OsAccount::Operator))
            .then(|| seat(st, p, seats)),
        last_used_unix: p.last_used_unix,
        legacy_seat: device
            .zip(p.legacy_device.as_deref())
            .is_some_and(|(d, l)| d.eq_ignore_ascii_case(l)),
    }
}

/// A light seat starts with its first connect, so it is always ready to dial. A seat of its own
/// reads its ledger row, and so does everything on a door.
fn seat(st: &MgmtState, p: &Profile, seats: &Snapshot) -> SeatPublic {
    let kind = match &p.os_account {
        OsAccount::Seat { seat: None, .. } => SeatKind::Steam,
        OsAccount::Operator => SeatKind::Shared,
        OsAccount::Seat { .. } | OsAccount::Linux { .. } | OsAccount::Windows { .. } => {
            SeatKind::Desktop
        }
    };
    SeatPublic {
        kind,
        ..seat_line(st, p, seats)
    }
}

/// The seat's state and port, with a placeholder kind that [`seat`] replaces.
fn seat_line(st: &MgmtState, p: &Profile, seats: &Snapshot) -> SeatPublic {
    let port = st.app.native_port.get().copied().unwrap_or(0);
    let unavailable = |why: &str| SeatPublic {
        state: SeatState::Unavailable,
        detail: Some(why.to_string()),
        port,
        occupant: None,
        steam_sign_in: None,
        kind: SeatKind::Desktop,
    };
    if crate::seats::is_door() {
        return match seats.row_of(&p.os_account) {
            Some(id) => seated(seats, id),
            None => unavailable("This host has no seat for the owner yet."),
        };
    }
    match &p.os_account {
        OsAccount::Seat { seat: Some(id), .. } => seated(seats, id),
        OsAccount::Seat { .. } | OsAccount::Operator => SeatPublic {
            state: SeatState::Ready,
            detail: None,
            port,
            occupant: None,
            steam_sign_in: steam_sign_in(&p.id),
            kind: SeatKind::Desktop,
        },
        OsAccount::Linux { .. } | OsAccount::Windows { .. } => {
            unavailable("This profile signs in to an account this host can't start yet.")
        }
    }
}

/// A Windows seat's line: its ledger row, and who plays there.
fn seated(seats: &Snapshot, id: &str) -> SeatPublic {
    let line = |state, port, detail: Option<&str>| SeatPublic {
        state,
        detail: detail.map(str::to_string),
        port,
        occupant: None,
        steam_sign_in: None,
        kind: SeatKind::Desktop,
    };
    let Some((seat, _)) = seats.seat(id) else {
        let why = if !seats.on && crate::seats::is_door() {
            "The box's seat service isn't answering."
        } else {
            "This profile's seat is gone. Remove the profile and add it again."
        };
        return line(SeatState::Unavailable, 0, Some(why));
    };
    let port = seat.native_port;
    if !seats.on {
        let why = if seats.desktop_edition {
            "This Windows edition serves one person at a time."
        } else {
            "This host needs seats turned on for a second profile."
        };
        return line(SeatState::Unavailable, port, Some(why));
    }
    let detail = seat.runtime.detail.as_deref();
    match seat.runtime.state {
        RuntimeState::Running => match seats.occupants.get(id).and_then(|o| o.first()) {
            Some(o) => SeatPublic {
                occupant: o.name.clone(),
                ..line(SeatState::Occupied, port, None)
            },
            None => line(SeatState::Ready, port, None),
        },
        RuntimeState::Starting => line(SeatState::Starting, port, detail),
        RuntimeState::Failed => line(
            SeatState::Unavailable,
            port,
            Some(detail.unwrap_or("This seat didn't start.")),
        ),
        RuntimeState::Stopping | RuntimeState::Stopped | RuntimeState::Unknown => {
            line(SeatState::Stopped, port, None)
        }
    }
}

/// The seats as they are now; the empty snapshot where the supervisor doesn't answer.
async fn seats_now() -> Arc<Snapshot> {
    tokio::task::spawn_blocking(crate::seats::snapshot)
        .await
        .unwrap_or_default()
}

/// A refusal to answer with: the status and its sentence.
type Refusal = (StatusCode, String);

fn refused((status, message): Refusal) -> Response {
    api_error(status, &message)
}

/// The status a supervisor refusal reads as.
fn seat_error(e: SeatError) -> Refusal {
    let status = match e.code {
        ErrorCode::Capacity => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, e.message)
}

/// The account of seat `n`, never shown to a player: `pf_seat<n>` on Windows, a Linux user name
/// `pf-seat-<n>` elsewhere.
fn seat_account(n: usize) -> String {
    if cfg!(windows) {
        format!("pf_seat{n}")
    } else {
        format!("pf-seat-{n}")
    }
}

/// A seat of its own for a new profile. Its account joins the seats group, so the host closes
/// what that group may not reach.
fn new_seat(name: &str) -> Result<String, Refusal> {
    if !crate::seats::enabled() {
        return Err((
            StatusCode::CONFLICT,
            "Turn seats on before adding a profile with a desktop of its own.".into(),
        ));
    }
    let taken: Vec<String> = crate::seats::list()
        .map_err(seat_error)?
        .into_iter()
        .map(|s| s.account)
        .collect();
    let account = (1..=4)
        .map(seat_account)
        .find(|a| !taken.contains(a))
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "This host already serves four seats, the most it can.".to_string(),
            )
        })?;
    let made = crate::seats::call(Command::Create(pf_seats::CreateSeat {
        name: name.trim().to_string(),
        account,
        autostart: false,
    }));
    crate::seats::invalidate();
    match made.map_err(seat_error)? {
        CommandResult::Created { seat } => {
            crate::plugins::converge_seat_denies();
            Ok(seat.id.as_str().to_string())
        }
        other => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("seats create answered {other:?}"),
        )),
    }
}

/// Stops a seat and removes it with its Windows account. A seat already gone is no error.
fn drop_seat(id: &str) -> Result<(), Refusal> {
    let Ok(id) = pf_seats::SeatId::parse(id) else {
        return Ok(());
    };
    let _ = crate::seats::call(Command::Stop { id: id.clone() });
    let removed = crate::seats::call(Command::Delete { id });
    crate::seats::invalidate();
    match removed {
        Ok(_) => Ok(()),
        Err(e) if e.code == ErrorCode::NotFound => Ok(()),
        Err(e) => Err(seat_error(e)),
    }
}

/// Starts a stopped seat in the background: a first logon takes tens of seconds, and the
/// caller answers with the seat as it is now, `starting`.
fn start_seat(id: &str) {
    let Ok(id) = pf_seats::SeatId::parse(id) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("seat-wake".into())
        .spawn(move || {
            let seats = crate::seats::snapshot();
            let idle = seats.seat(id.as_str()).is_some_and(|(s, _)| {
                matches!(
                    s.runtime.state,
                    RuntimeState::Stopped | RuntimeState::Failed | RuntimeState::Unknown
                )
            });
            if !idle || !seats.on {
                return;
            }
            crate::seats::invalidate();
            if let Err(e) = crate::seats::call(Command::Start { id }) {
                tracing::warn!(code = ?e.code, "seat did not start: {}", e.message);
            }
            crate::seats::invalidate();
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "seat wake thread did not start");
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

fn admin(st: &MgmtState, profiles: &Profiles, p: &Profile, seats: &Snapshot) -> ProfileAdmin {
    let owner = profiles.owner_id();
    ProfileAdmin {
        public: public(st, p, owner.as_deref(), None, seats),
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
    let seats = seats_now().await;
    let rows: Vec<ProfilePublic> = profiles
        .list()
        .iter()
        .map(|p| public(&st, p, owner.as_deref(), device.as_deref(), &seats))
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
/// Starts a stopped seat so the next connect lands in it, and answers at once with the seat
/// `starting`; poll `enumerate` until it is `ready`. A light seat starts with its first
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
    let Some(p) = profiles.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "no profile with that id");
    };
    let seats = seats_now().await;
    let on_a_row = match seats.row_of(&p.os_account) {
        Some(row) => {
            start_seat(row);
            true
        }
        None => false,
    };
    let mut row = public(&st, &p, profiles.owner_id().as_deref(), None, &seats);
    // The wake thread may not have published `starting` yet; the answer says it anyway.
    if on_a_row {
        if let Some(seat) = row.seat.as_mut().filter(|s| s.state == SeatState::Stopped) {
            seat.state = SeatState::Starting;
        }
    }
    Json(row).into_response()
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
    let seats = seats_now().await;
    let rows: Vec<ProfileAdmin> = profiles
        .list()
        .iter()
        .map(|p| admin(&st, profiles, p, &seats))
        .collect();
    Json(rows).into_response()
}

/// Add a profile
///
/// A seat profile (the default) plays in a seat of its own; one with `seat: false` plays the
/// box's own session under its own name. On Windows a seat is a desktop of its own: its account
/// is made here, and seats must be on. On Linux a seat is a Steam of its own in the owner's
/// session, or with `desktop` a desktop of its own, which needs the door.
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
        (status = CONFLICT, description = "Another profile has that name, seats are off (Windows), or the door is off (a Linux desktop)", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "The box already has eight profiles, or four seats", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn create_profile(
    State(st): State<Arc<MgmtState>>,
    ApiJson(input): ApiJson<ProfileCreate>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    // A Windows seat is always a desktop of its own. On Linux that is the door's: without it a
    // seat is a Steam of its own in the owner's session, and a desktop can't be had yet.
    let own_desktop = cfg!(windows) || input.desktop;
    if input.seat && own_desktop && !cfg!(windows) && !crate::seats::is_door() {
        return api_error(
            StatusCode::CONFLICT,
            "Turn on Reachable without logging in before adding a profile with a desktop of its own.",
        );
    }
    let seat = if input.seat && own_desktop {
        let name = input.display_name.clone();
        match tokio::task::spawn_blocking(move || new_seat(&name)).await {
            Ok(Ok(id)) => Some(id),
            Ok(Err(refusal)) => return refused(refusal),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    } else {
        None
    };
    match profiles.create(input, seat.clone()) {
        Ok(p) => {
            let seats = seats_now().await;
            (StatusCode::CREATED, Json(admin(&st, profiles, &p, &seats))).into_response()
        }
        Err(e) => {
            // No profile names the seat just made: take it back with its account.
            if let Some(id) = seat {
                let _ = tokio::task::spawn_blocking(move || drop_seat(&id)).await;
            }
            edit_error(e)
        }
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
        Ok(p) => Json(admin(&st, profiles, &p, &*seats_now().await)).into_response(),
        Err(e) => edit_error(e),
    }
}

/// Remove a profile
///
/// Sessions playing as it end with `SEAT_UNAVAILABLE`. With `erase`, its Steam home and picture
/// go too; without, the home stays. A Windows seat's account goes with its profile, so removing
/// one needs `erase`.
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
        (status = CONFLICT, description = "The owner profile can't be removed, or a Windows seat's without `erase`", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer (Windows)", body = ApiError),
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
    if let Some(OsAccount::Seat {
        seat: Some(seat), ..
    }) = profiles.get(&id).map(|p| p.os_account)
    {
        if !q.erase {
            return api_error(
                StatusCode::CONFLICT,
                if cfg!(windows) {
                    "Removing this profile also removes its Windows account. Confirm with erase."
                } else {
                    "Removing this profile also removes its user account and files. Confirm with erase."
                },
            );
        }
        match tokio::task::spawn_blocking(move || drop_seat(&seat)).await {
            Ok(Ok(())) => {}
            Ok(Err(refusal)) => return refused(refusal),
            Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
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
    /// A Linux door: seats are on while it runs.
    Linux,
    Other,
}

/// Whether this box runs its profiles in seats of their own, with what turning that on needs.
#[derive(Serialize, ToSchema)]
pub(crate) struct Seating {
    enabled: bool,
    /// `other` has no seats to turn on; `linux` has them while the door is on.
    platform: SeatingPlatform,
    /// The prerequisites, one each. Empty on `other`.
    checks: Vec<SeatCheck>,
    /// Windows: seats are on and the network may reach Remote Desktop. Off elsewhere.
    allow_rdp_from_network: bool,
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

/// Whether the seat supervisor is this host's to ask: Windows, or a Linux door.
fn has_supervisor() -> bool {
    cfg!(windows) || crate::seats::is_door()
}

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
            platform: if cfg!(windows) {
                SeatingPlatform::Windows
            } else {
                SeatingPlatform::Linux
            },
            checks: status.checks.into_iter().map(SeatCheck::from).collect(),
            allow_rdp_from_network: status.allow_rdp_from_network,
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
/// Server, the Remote Desktop Session Host role, licensing and a graphics card. A Linux door
/// reports its supervisor's checks with `platform: linux`. Other hosts answer `enabled: false`
/// with `platform: other`. Admin lane only.
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
    if !has_supervisor() {
        return Json(Seating {
            enabled: false,
            platform: SeatingPlatform::Other,
            checks: Vec::new(),
            allow_rdp_from_network: false,
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
        return api_error(
            StatusCode::CONFLICT,
            if crate::seats::is_door() {
                "Seats stay on while Reachable without logging in is on."
            } else {
                SEATS_NEED_WINDOWS
            },
        );
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
    if !has_supervisor() {
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

/// `PUT /profiles/door`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DoorChange {
    /// `true` makes the box reachable without anyone logging in; `false` hands it back.
    on: bool,
}

/// Turn Reachable without logging in on or off
///
/// Linux. On moves the box's host to a system service that runs from boot, places every connect
/// on a seat of the box and keeps the box's files in `/var/lib/punktfunk`; the owner plays on a
/// seat of their own. Off puts the files and the host back. The answer is 202 once the switch is
/// queued: the console reloads and polls `GET /host`, whose `door` says which host answers. The
/// owner's user must be in the `punktfunk-update` group. Admin lane only.
#[utoipa::path(
    put,
    path = "/profiles/door",
    tag = "profiles",
    operation_id = "setDoor",
    request_body = DoorChange,
    responses(
        (status = ACCEPTED, description = "The switch is under way; poll `GET /host` for `door`"),
        (status = NO_CONTENT, description = "The host is already so"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "The owner's user isn't in the punktfunk-update group", body = ApiError),
        (status = CONFLICT, description = "Not a Linux host, or this install lacks the switch", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "systemd didn't take the switch", body = ApiError),
    )
)]
pub(crate) async fn put_door(ApiJson(input): ApiJson<DoorChange>) -> Response {
    match tokio::task::spawn_blocking(move || crate::door::change(input.on)).await {
        Ok(Ok(crate::door::Outcome::Started)) => StatusCode::ACCEPTED.into_response(),
        Ok(Ok(crate::door::Outcome::Already)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(refusal)) => refused(refusal),
        Err(e) => api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// The ledger row the profile plays on, or the answer for a profile that has none.
fn seat_of(st: &MgmtState, id: &str, seats: &Snapshot) -> Result<(Profile, String), Refusal> {
    let Some(profiles) = st.app.profiles.get() else {
        return Err((StatusCode::NOT_FOUND, "this host has no profiles".into()));
    };
    let Some(p) = profiles.get(id) else {
        return Err((StatusCode::NOT_FOUND, "no profile with that id".into()));
    };
    match seats.row_of(&p.os_account) {
        Some(row) => {
            let row = row.to_string();
            Ok((p, row))
        }
        None => Err((
            StatusCode::CONFLICT,
            "This profile plays on the host's own desktop; it has no seat to start or stop.".into(),
        )),
    }
}

/// Start a profile's seat
///
/// The console's **Start**: as `wake`, from the console.
#[utoipa::path(
    post,
    path = "/profiles/{id}/start",
    tag = "profiles",
    operation_id = "startProfileSeat",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The profile, its seat `starting`", body = ProfileAdmin),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "The profile has no seat of its own", body = ApiError),
    )
)]
pub(crate) async fn start_profile_seat(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let seats = seats_now().await;
    let (p, seat) = match seat_of(&st, &id, &seats) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    start_seat(&seat);
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    let mut row = admin(&st, profiles, &p, &seats);
    if let Some(s) = row
        .public
        .seat
        .as_mut()
        .filter(|s| s.state == SeatState::Stopped)
    {
        s.state = SeatState::Starting;
    }
    Json(row).into_response()
}

/// Stop a profile's seat
///
/// Logs the seat off; whoever plays there is disconnected. The next pick starts it again.
#[utoipa::path(
    post,
    path = "/profiles/{id}/stop",
    tag = "profiles",
    operation_id = "stopProfileSeat",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The profile, its seat stopped", body = ProfileAdmin),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "The profile has no seat of its own", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn stop_profile_seat(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let (p, seat) = match seat_of(&st, &id, &*seats_now().await) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    let stopped = tokio::task::spawn_blocking(move || {
        let id = pf_seats::SeatId::parse(seat)
            .map_err(|e| SeatError::new(ErrorCode::InvalidRequest, e.to_string()))?;
        let done = crate::seats::call(Command::Stop { id });
        crate::seats::invalidate();
        done
    })
    .await;
    match stopped {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => return refused(seat_error(e)),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    Json(admin(&st, profiles, &p, &*seats_now().await)).into_response()
}

/// End the session on a profile's seat
///
/// Disconnects whoever plays there and keeps the seat running for the next connect.
#[utoipa::path(
    post,
    path = "/profiles/{id}/end",
    tag = "profiles",
    operation_id = "endProfileSession",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The profile, its seat free", body = ProfileAdmin),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "The profile has no seat of its own", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat's host didn't answer", body = ApiError),
    )
)]
pub(crate) async fn end_profile_session(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let (p, seat) = match seat_of(&st, &id, &*seats_now().await) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    let ended = tokio::task::spawn_blocking(move || {
        let snap = crate::seats::snapshot();
        let row = snap.seat(&seat).map(|(s, _)| s.clone());
        let done = row.as_ref().is_some_and(crate::seats::end_sessions);
        crate::seats::invalidate();
        done
    })
    .await
    .unwrap_or(false);
    if !ended {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The seat didn't answer. Stop it instead to disconnect whoever plays there.",
        );
    }
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    Json(admin(&st, profiles, &p, &*seats_now().await)).into_response()
}

/// Paths the seat proxy never passes: pairing, trust and profiles are the box's, and a seat host
/// updates with the box. A `.` or `..` segment is refused outright.
fn proxy_refuses(rest: &str) -> bool {
    let path = rest.split('?').next().unwrap_or("");
    let first = path.trim_start_matches('/').split('/').next().unwrap_or("");
    path.split('/').any(|s| s == "." || s == "..")
        || matches!(
            first,
            "native" | "pair" | "profiles" | "update" | "actions" | "clients"
        )
}

/// `ANY /api/v1/profiles/{id}/proxy/{*rest}`: the console reaching a seat profile's own host for
/// what is per seat (library, game sources, plugins). Admin lane only: neither the plugin nor
/// the device allowlist names it. Undocumented in the spec, because it forwards any method.
pub(crate) async fn proxy_profile_seat(
    State(st): State<Arc<MgmtState>>,
    Path((id, rest)): Path<(String, String)>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if proxy_refuses(&rest) {
        return api_error(
            StatusCode::FORBIDDEN,
            "That part of a seat is the box's. Change it on this page, not through a seat.",
        );
    }
    let seats = seats_now().await;
    let (_, seat) = match seat_of(&st, &id, &seats) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    let Some(row) = seats.seat(&seat).map(|(s, _)| s.clone()) else {
        return api_error(StatusCode::NOT_FOUND, "This profile's seat is gone.");
    };
    // The owner's session starts on demand: the console asked for its library, and nobody has
    // logged in to make it.
    if row.owner
        && matches!(
            row.runtime.state,
            RuntimeState::Stopped | RuntimeState::Failed | RuntimeState::Unknown
        )
    {
        start_seat(&seat);
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The owner's session is starting. Try again in a moment.",
        );
    }
    let target = match uri.query() {
        Some(q) => format!("{rest}?{q}"),
        None => rest,
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let method = method.as_str().to_string();
    let answer = tokio::task::spawn_blocking(move || {
        crate::seats::forward(
            &row,
            &method,
            &target,
            content_type.as_deref(),
            body.to_vec(),
        )
    })
    .await
    .ok()
    .flatten();
    let Some((status, content_type, bytes)) = answer else {
        return api_error(
            StatusCode::BAD_GATEWAY,
            "The seat didn't answer. Start it, then try again.",
        );
    };
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut resp = (status, bytes).into_response();
    if let Some(ct) = content_type.and_then(|c| header::HeaderValue::from_str(&c).ok()) {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seats::Occupant;

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn snap(on: bool, state: RuntimeState, occupied: bool) -> Snapshot {
        let seat: pf_seats::Seat = serde_json::from_value(serde_json::json!({
            "id": ID, "name": "Kid", "account": "pf_seat1", "display_slot": 12,
            "native_port": 9778, "mgmt_port": 47991, "runtime": { "state": state },
        }))
        .unwrap();
        let mut occupants = std::collections::BTreeMap::new();
        if occupied {
            occupants.insert(
                ID.to_string(),
                vec![Occupant {
                    client: "aa11bb22".into(),
                    name: Some("Ben's Apple TV".into()),
                }],
            );
        }
        Snapshot {
            on,
            desktop_edition: false,
            seats: vec![seat],
            occupants,
        }
    }

    #[test]
    fn the_seat_proxy_keeps_the_box_s_own_routes_to_itself() {
        assert!(!proxy_refuses("library"));
        assert!(!proxy_refuses("library/page?as=kid"));
        assert!(!proxy_refuses("plugins/rom-manager"));
        for refused in [
            "native/pair/arm",
            "profiles",
            "pair/pin",
            "update",
            "actions/power.sleep/invoke",
            "clients",
            "library/../native/pair/arm",
            "./native",
        ] {
            assert!(proxy_refuses(refused), "{refused}");
        }
    }

    #[test]
    fn a_windows_seat_reads_its_ledger_row() {
        let ready = seated(&snap(true, RuntimeState::Running, false), ID);
        assert_eq!((ready.state, ready.port), (SeatState::Ready, 9778));
        let busy = seated(&snap(true, RuntimeState::Running, true), ID);
        assert_eq!(
            (busy.state, busy.occupant.as_deref()),
            (SeatState::Occupied, Some("Ben's Apple TV"))
        );
        let state = |on, s| seated(&snap(on, s, false), ID).state;
        assert_eq!(state(true, RuntimeState::Starting), SeatState::Starting);
        assert_eq!(state(true, RuntimeState::Stopped), SeatState::Stopped);
        assert_eq!(state(true, RuntimeState::Failed), SeatState::Unavailable);
        assert_eq!(state(false, RuntimeState::Running), SeatState::Unavailable);
        let gone = seated(&snap(true, RuntimeState::Running, false), &"f".repeat(32));
        assert_eq!(gone.state, SeatState::Unavailable);
        let desktop = Snapshot {
            desktop_edition: true,
            ..snap(false, RuntimeState::Stopped, false)
        };
        assert_eq!(
            seated(&desktop, ID).detail.as_deref(),
            Some("This Windows edition serves one person at a time.")
        );
    }
}
