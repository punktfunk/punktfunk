//! The people on this box (`design/profiles-and-seats.md` §7).
//!
//! Three routes are on the cert lane, for every paired device: the list a client's picker
//! shows, a profile's picture, and waking its seat. Everything that changes a profile is the
//! console's, among them turning seats on. A profile is not a trust boundary: any paired
//! device may pick any profile, but starting another profile's seat takes the launch grant.

use super::auth::{AuthLane, PairedDevice};
use super::shared::*;
use crate::profiles::take_turns::{self, LocalAccount};
use crate::profiles::{
    EditError, Home, OsAccount, Profile, ProfileCreate, ProfileUpdate, Profiles,
};
use crate::seats::Snapshot;
use axum::http::header;
use axum::Extension;
use pf_seats::ipc::{
    ApiError as SeatError, Command, CommandResult, Diagnostic, DiagnosticLevel, ErrorCode,
};
use pf_seats::{RuntimeState, Secret};

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
    /// The certificate fingerprint of the host at `port`, hex, when it is a seat's own: a
    /// client dialing `port` directly pins it. Absent: the pin of this host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pin: Option<String>,
    /// Which kind of seat it is.
    kind: SeatKind,
    /// A take-turns profile: its own Windows account on the one console, not a host of its own.
    /// It dials this host like the owner does, so it has no per-seat library of its own and
    /// removing it leaves the Windows account alone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    take_turns: bool,
    /// The Windows account of a seat on an existing one, for the console's card and its Remove
    /// sentence. Absent for an account punktfunk made, and to a paired device.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    account: Option<String>,
}

/// What a profile's seat is. A door places every profile on a seat, so a profile on the box's own
/// session has one too: the owner's.
#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SeatKind {
    /// The box's own session under another name; only on a door, which places it on the
    /// owner's seat.
    Shared,
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
    /// This host keeps a Steam home for it, with that Steam's sign-in and saved games. A removal
    /// with `erase` deletes it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    steam_home: bool,
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
    operator: bool,
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
        seat: (pf_paths::seat::role().is_door() || !matches!(p.os_account, OsAccount::Operator))
            .then(|| seat(st, p, seats, operator)),
        last_used_unix: p.last_used_unix,
        legacy_seat: device
            .zip(p.legacy_device.as_deref())
            .is_some_and(|(d, l)| d.eq_ignore_ascii_case(l)),
    }
}

/// A seat of its own reads its ledger row, and so does everything on a door. `operator`: the
/// console is asking, and may read why a seat failed.
fn seat(st: &MgmtState, p: &Profile, seats: &Snapshot, operator: bool) -> SeatPublic {
    if let OsAccount::Windows { account_name, .. } = &p.os_account {
        return take_turns_seat(st, &p.display_name, account_name);
    }
    let kind = match &p.os_account {
        OsAccount::Operator => SeatKind::Shared,
        OsAccount::Seat { .. } | OsAccount::Linux { .. } | OsAccount::Windows { .. } => {
            SeatKind::Desktop
        }
    };
    SeatPublic {
        kind,
        ..seat_line(st, p, seats, operator)
    }
}

/// Attach `account` to the console if it isn't already and can be — the switch a take-turns
/// `wake` triggers. A no-op when the account is already on screen or has no session to attach;
/// the seat state then tells the client to wait or to sign in. Blocking (WTS + `tscon`).
fn take_turns_switch(account: &str) {
    use crate::profiles::take_turns::{self, ConsoleAction};
    match take_turns::decide(&take_turns::account_state(account)) {
        ConsoleAction::Switch(session) => {
            if let Err(e) = take_turns::switch_to(account, session) {
                tracing::warn!(%account, error = %format!("{e:#}"), "take-turns switch did not run");
            }
        }
        other => {
            tracing::debug!(%account, ?other, "take-turns wake: no switch needed");
        }
    }
}

/// A take-turns profile's seat from the console state: `ready` when its account is on screen,
/// `stopped` when it can be switched in (the client's `wake` does it), `starting` while a switch
/// runs, `unavailable` when it must sign in once or someone else holds the console
/// (`design/windows-take-turns.md` §2). Not a managed seat — no pin, no per-seat host.
fn take_turns_seat(st: &MgmtState, display_name: &str, account: &str) -> SeatPublic {
    use crate::profiles::take_turns::{self, TurnSeat};
    let action = take_turns::decide(&take_turns::account_state(account));
    // A stream on the console as another account: a switch would cut it, so hold off.
    let console_busy =
        take_turns::console_account().is_some() && crate::telemetry::session_status::count() > 0;
    let switching = take_turns::switch_in_flight(
        take_turns::switch_marker()
            .as_ref()
            .map(|(a, t)| (a.as_str(), *t)),
        account,
        crate::clock::unix_secs(),
    );
    let (state, detail) = match take_turns::turn_seat(&action, console_busy, switching) {
        TurnSeat::Ready => (SeatState::Ready, None),
        TurnSeat::Switching => (
            SeatState::Starting,
            Some(format!("Switching to {display_name}…")),
        ),
        TurnSeat::Switchable => (SeatState::Stopped, None),
        TurnSeat::SignIn => (
            SeatState::Unavailable,
            Some(format!("Sign {display_name} in on the PC once.")),
        ),
        TurnSeat::Occupied => (
            SeatState::Unavailable,
            Some("Someone else is playing on this PC right now.".to_string()),
        ),
        TurnSeat::Gone => (
            SeatState::Unavailable,
            Some("This Windows account is gone.".to_string()),
        ),
    };
    SeatPublic {
        state,
        detail,
        port: st.app.native_port.get().copied().unwrap_or(0),
        occupant: None,
        pin: None,
        kind: SeatKind::Desktop,
        take_turns: true,
        account: None,
    }
}

/// The seat's state and port, with a placeholder kind that [`seat`] replaces. The `operator`
/// also reads the Windows account of a seat on an existing one.
fn seat_line(st: &MgmtState, p: &Profile, seats: &Snapshot, operator: bool) -> SeatPublic {
    let port = st.app.native_port.get().copied().unwrap_or(0);
    let unavailable = |why: &str| SeatPublic {
        state: SeatState::Unavailable,
        detail: Some(why.to_string()),
        port,
        occupant: None,
        pin: None,
        kind: SeatKind::Desktop,
        take_turns: false,
        account: None,
    };
    if let Some(id) = seats.row_of(&p.os_account) {
        let account = seats
            .seat(id)
            .filter(|(row, _)| operator && row.adopted_sid.is_some())
            .map(|(row, _)| row.account.clone());
        return SeatPublic {
            account,
            ..seated(seats, id, operator)
        };
    }
    if pf_paths::seat::role().is_door() {
        return unavailable("This host has no seat for the owner yet.");
    }
    match &p.os_account {
        OsAccount::Seat { .. } | OsAccount::Operator => SeatPublic {
            state: SeatState::Ready,
            detail: None,
            port,
            occupant: None,
            pin: None,
            kind: SeatKind::Desktop,
            take_turns: false,
            account: None,
        },
        OsAccount::Linux { .. } | OsAccount::Windows { .. } => {
            unavailable("This profile signs in to an account this host can't start yet.")
        }
    }
}

/// A seat's line: its ledger row, who plays there, and its host's pin. Why a seat failed is the
/// supervisor's own text (unit, account, ports): only the `operator` reads it.
fn seated(seats: &Snapshot, id: &str, operator: bool) -> SeatPublic {
    let line = |state, port, detail: Option<&str>| SeatPublic {
        state,
        detail: detail.map(str::to_string),
        port,
        occupant: None,
        pin: None,
        kind: SeatKind::Desktop,
        take_turns: false,
        account: None,
    };
    let Some((seat, _)) = seats.seat(id) else {
        let why = if !seats.on && pf_paths::seat::role().is_door() {
            "The box's seat service isn't answering."
        } else {
            "This profile's seat is gone. Remove the profile and add it again."
        };
        return line(SeatState::Unavailable, 0, Some(why));
    };
    let port = seat.native_port;
    if !seats.on {
        let why = if seats.single_session {
            "This Windows edition serves one person at a time."
        } else {
            "This host needs seats turned on for a second profile."
        };
        return line(SeatState::Unavailable, port, Some(why));
    }
    let detail = seat.runtime.detail.as_deref();
    let mut out = match seat.runtime.state {
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
            Some(
                detail
                    .filter(|_| operator)
                    .unwrap_or("This seat didn't start."),
            ),
        ),
        RuntimeState::Stopping | RuntimeState::Stopped | RuntimeState::Unknown => {
            line(SeatState::Stopped, port, None)
        }
    };
    out.pin = seat.fingerprint.clone();
    out
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

/// The status a supervisor refusal reads as, and its sentence for the console.
fn seat_error(e: SeatError) -> Refusal {
    let status = match e.code {
        ErrorCode::Capacity => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorCode::Backend => backend_status(&e.message),
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, sentence_of(&e.message).to_string())
}

/// A backend refusal's status, by the code its message leads with: the person can fix a password
/// (400) or pick another account (409); anything else is the box's (503).
fn backend_status(message: &str) -> StatusCode {
    match message.split_once(": ").map(|(code, _)| code) {
        Some(
            "credential_wrong" | "credential_must_change" | "credential_invalid" | "account_locked",
        ) => StatusCode::BAD_REQUEST,
        // `account_at_console` is the older name of `account_signed_in`.
        Some(
            "account_is_administrator"
            | "account_signed_in"
            | "account_at_console"
            | "account_policy"
            | "account_not_owned",
        ) => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

/// A supervisor message without the `<code>: ` a backend refusal leads with.
fn sentence_of(message: &str) -> &str {
    match message.split_once(": ") {
        Some((code, sentence))
            if !code.is_empty() && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            sentence
        }
        _ => message,
    }
}

const SEATS_OFF: &str = "Turn seats on before adding a profile with a desktop of its own.";

/// The account of seat `n`, never shown to a player: `pf_seat<n>` on Windows, a Linux user name
/// `pf-seat-<n>` elsewhere.
fn seat_account(n: usize) -> String {
    if cfg!(windows) {
        format!("pf_seat{n}")
    } else {
        format!("pf-seat-{n}")
    }
}

/// A seat of its own for a new profile, on an account made for it.
fn new_seat(name: &str) -> Result<String, Refusal> {
    if !crate::seats::enabled() {
        return Err((StatusCode::CONFLICT, SEATS_OFF.into()));
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
    created(crate::seats::call(Command::Create(pf_seats::CreateSeat {
        name: name.trim().to_string(),
        account,
        autostart: false,
    })))
}

/// A seat for a new profile on `account`, an existing local Windows account no other profile
/// holds (`bound`: the take-turns ones), which it signs in to with `password`.
fn adopt_seat(
    name: &str,
    account: &str,
    password: Secret,
    bound: &[String],
) -> Result<String, Refusal> {
    if !crate::seats::enabled() {
        return Err((StatusCode::CONFLICT, SEATS_OFF.into()));
    }
    let ledger = crate::seats::list().map_err(seat_error)?;
    let account = bindable_account(account, &ledger, bound)?;
    created(crate::seats::call(Command::Adopt {
        seat: pf_seats::CreateSeat {
            name: name.trim().to_string(),
            account: account.name,
            autostart: false,
        },
        sid: account.sid,
        password,
    }))
}

/// The new row's id. Its account joins the seats group, so the host closes what that group may
/// not reach.
fn created(made: Result<CommandResult, SeatError>) -> Result<String, Refusal> {
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

/// `name` as Windows spells it, with its SID, when a new profile may bind it.
fn bindable_account(
    name: &str,
    ledger: &[pf_seats::Seat],
    bound: &[String],
) -> Result<LocalAccount, Refusal> {
    let Some(account) = take_turns::local_account(name) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "No Windows account by that name on this PC.".into(),
        ));
    };
    if !take_turns::bindable(&account, ledger, bound) {
        return Err((
            StatusCode::CONFLICT,
            "Another profile already uses that Windows account.".into(),
        ));
    }
    Ok(account)
}

/// The Windows accounts take-turns profiles hold.
fn take_turns_bound(profiles: &Profiles) -> Vec<String> {
    profiles
        .list()
        .into_iter()
        .filter_map(|p| match p.os_account {
            OsAccount::Windows { account_name, .. } => Some(account_name),
            _ => None,
        })
        .collect()
}

/// Verifies `password` for seat `id`, a seat on an existing Windows account, and seals it in
/// place of the old one.
fn reseal(id: &str, password: Secret) -> Result<(), Refusal> {
    let id = pf_seats::SeatId::parse(id).map_err(|_| reseal_refusal())?;
    match crate::seats::call(Command::Credential { id, password }) {
        Ok(_) => Ok(()),
        // The supervisor's refusal for a seat on an account made for it.
        Err(e) if e.code == ErrorCode::InvalidRequest => Err(reseal_refusal()),
        Err(e) => Err(seat_error(e)),
    }
}

/// A password update for a profile with no existing Windows account.
fn reseal_refusal() -> Refusal {
    (
        StatusCode::CONFLICT,
        "Only a profile with a desktop of its own on an existing Windows account has a password to update."
            .to_string(),
    )
}

/// Stops a seat and removes it: an account made for it goes too, an existing one is released.
/// A seat already gone is no error.
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

/// Seats with a wake thread running: a second wake for one has nothing to add.
static WAKING: std::sync::Mutex<std::collections::BTreeSet<String>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A seat's place in [`WAKING`], held until the wake ends or its thread is gone.
struct Waking(String);

impl Waking {
    fn claim(id: &str) -> Option<Self> {
        let mut waking = WAKING.lock().unwrap_or_else(|e| e.into_inner());
        waking.insert(id.to_string()).then(|| Self(id.to_string()))
    }
}

impl Drop for Waking {
    fn drop(&mut self) {
        WAKING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// Whether a seat in `state` wants a start. A `Stopping` seat does: the supervisor runs the
/// start after the stop.
pub(super) fn needs_start(state: &RuntimeState) -> bool {
    !matches!(state, RuntimeState::Running | RuntimeState::Starting)
}

/// Starts a stopped seat in the background: a first logon takes tens of seconds, and the
/// caller answers with the seat as it is now, `starting`.
pub(super) fn start_seat(id: &str) {
    let Ok(id) = pf_seats::SeatId::parse(id) else {
        return;
    };
    let Some(waking) = Waking::claim(id.as_str()) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("seat-wake".into())
        .spawn(move || {
            let _waking = waking;
            let seats = crate::seats::snapshot();
            let idle = seats
                .seat(id.as_str())
                .is_some_and(|(s, _)| needs_start(&s.runtime.state));
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

fn admin(st: &MgmtState, profiles: &Profiles, p: &Profile, seats: &Snapshot) -> ProfileAdmin {
    let owner = profiles.owner_id();
    ProfileAdmin {
        public: public(st, p, owner.as_deref(), None, seats, true),
        legacy_device: p.legacy_device.clone(),
        default: profiles.default_profile_id().as_deref() == Some(p.id.as_str()),
        steam_home: keeps_steam_home(&p.id),
    }
}

/// A Steam home is a Linux host's, under its own data dir.
fn keeps_steam_home(id: &str) -> bool {
    #[cfg(target_os = "linux")]
    return pf_paths::seat_home(id).is_dir();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = id;
        false
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
    Extension(lane): Extension<AuthLane>,
    device: Option<Extension<PairedDevice>>,
) -> Response {
    let device = device.map(|Extension(PairedDevice(fp))| fp);
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    let owner = profiles.owner_id();
    let seats = seats_now().await;
    let operator = lane == AuthLane::Admin;
    let rows: Vec<ProfilePublic> = profiles
        .list()
        .iter()
        .map(|p| {
            public(
                &st,
                p,
                owner.as_deref(),
                device.as_deref(),
                &seats,
                operator,
            )
        })
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
/// `starting`; poll `enumerate` until it is `ready`. A profile with no seat to start only gets
/// its row back. A paired device wakes the profile it lands in when it names none; any other
/// needs its launch grant.
#[utoipa::path(
    post,
    path = "/profiles/{id}/wake",
    tag = "profiles",
    operation_id = "wakeProfile",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The profile, its seat as it is now", body = ProfilePublic),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = FORBIDDEN, description = "This device may not start another profile's seat", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
    )
)]
pub(crate) async fn wake_profile(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    device: Option<Extension<PairedDevice>>,
    Path(id): Path<String>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    let Some(p) = profiles.get(&id) else {
        return api_error(StatusCode::NOT_FOUND, "no profile with that id");
    };
    let fp = device.map(|Extension(PairedDevice(fp))| fp);
    if lane != AuthLane::Admin && !may_wake(&st, profiles, &p.id, fp.as_deref()) {
        return api_error(
            StatusCode::FORBIDDEN,
            "This device isn't allowed to start another profile's seat.",
        );
    }
    // A take-turns profile has no ledger row: waking it attaches its Windows account to the
    // console. The host that runs the switch does not survive the console change, so the client
    // polls the relaunched host; the switch marker carries the "Switching to …" line meanwhile.
    if let OsAccount::Windows { account_name, .. } = &p.os_account {
        let account = account_name.clone();
        let _ = tokio::task::spawn_blocking(move || take_turns_switch(&account)).await;
    }
    let seats = seats_now().await;
    let on_a_row = match seats.row_of(&p.os_account) {
        Some(row) => {
            start_seat(row);
            true
        }
        None => false,
    };
    let operator = lane == AuthLane::Admin;
    let mut row = public(
        &st,
        &p,
        profiles.owner_id().as_deref(),
        None,
        &seats,
        operator,
    );
    // The wake thread may not have published `starting` yet; the answer says it anyway.
    if on_a_row {
        if let Some(seat) = row.seat.as_mut().filter(|s| s.state == SeatState::Stopped) {
            seat.state = SeatState::Starting;
        }
    }
    Json(row).into_response()
}

/// A paired device wakes the profile it lands in when it names none, and another only while
/// its live mask (expiry-aware) carries the launch grant.
fn may_wake(st: &MgmtState, profiles: &Profiles, id: &str, fp: Option<&str>) -> bool {
    let Some(fp) = fp else {
        return false;
    };
    let own = profiles.resolve(Some(fp), None).is_ok_and(|r| r.id == id);
    own || st
        .native
        .as_ref()
        .and_then(|n| n.effective(fp, crate::clock::unix_secs()))
        .is_some_and(|mask| mask & punktfunk_core::quic::GRANT_LAUNCH != 0)
}

/// List the profiles with what the console manages
///
/// On the plugin lane each row is `id`, `display_name` and `avatar` alone.
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
pub(crate) async fn list_profiles(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    if lane == AuthLane::Plugin {
        let rows: Vec<_> = profiles
            .list()
            .iter()
            .map(|p| {
                let mut row = serde_json::json!({ "id": p.id, "display_name": p.display_name });
                if let Some(avatar) = &p.avatar {
                    row["avatar"] = avatar.as_str().into();
                }
                row
            })
            .collect();
        return Json(rows).into_response();
    }
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
/// A seat profile (the default) gets a desktop of its own, whose account is made here, or on
/// Windows Server signs in to `seat_account`, an existing local account, with `seat_password`.
/// One with `seat: false` plays the box's own session under its own name. A seat needs seats on
/// (Windows) or the door (Linux).
#[utoipa::path(
    post,
    path = "/profiles",
    tag = "profiles",
    operation_id = "createProfile",
    request_body = ProfileCreate,
    responses(
        (status = CREATED, description = "The new profile", body = ProfileAdmin),
        (status = BAD_REQUEST, description = "No name, an accent that is not #RRGGBB, an unknown Windows account, or a password missing or refused", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "Another profile has that name or Windows account, seats are off (Windows), the door is off (Linux), or a Windows account a seat can't use", body = ApiError),
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
    // A take-turns profile binds to an existing Windows account; no seat is made.
    if let Some(account) = input.windows_account.clone() {
        return create_take_turns(&st, profiles, input, account).await;
    }
    if let Some(refusal) = adopt_refusal(&input) {
        return refused(refusal);
    }
    // A Linux seat is the door's; elsewhere `new_seat` asks whether seats are on.
    if input.seat && cfg!(target_os = "linux") && !pf_paths::seat::role().is_door() {
        return api_error(
            StatusCode::CONFLICT,
            "Turn on Reachable without logging in before adding a profile with a desktop of its own.",
        );
    }
    // Vetted before the seat's account exists: a refused profile must leave none behind.
    let seat = if input.seat {
        let name = match profiles.vet_create(&input) {
            Ok(name) => name,
            Err(e) => return edit_error(e),
        };
        let adopt = input.seat_account.clone().zip(input.seat_password.clone());
        let bound = take_turns_bound(profiles);
        let made = blocking("seat supervisor", move || match adopt {
            Some((account, password)) => adopt_seat(&name, &account, password, &bound),
            None => new_seat(&name),
        });
        match made.await {
            Ok(Ok(id)) => Some(id),
            Ok(Err(refusal)) => return refused(refusal),
            Err(resp) => return resp,
        }
    } else {
        None
    };
    let os_account = match &seat {
        Some(row) => OsAccount::Seat {
            seat: Some(row.clone()),
        },
        None => OsAccount::Operator,
    };
    match profiles.create(input, os_account) {
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

/// Why a seat can't sign in to the existing account the input names, if it names one.
fn adopt_refusal(input: &ProfileCreate) -> Option<Refusal> {
    input.seat_account.as_ref()?;
    let (status, why) = if !input.seat {
        (
            StatusCode::BAD_REQUEST,
            "An existing account needs a desktop of its own.",
        )
    } else if !cfg!(windows) {
        (
            StatusCode::CONFLICT,
            "A seat on an existing account is only on Windows Server.",
        )
    } else if input.seat_password.is_none() {
        (StatusCode::BAD_REQUEST, "Enter the account's password.")
    } else {
        return None;
    };
    Some((status, why.to_string()))
}

/// Make a take-turns profile bound to the local Windows account `account`. Windows only; the
/// account must exist and no other profile or seat may hold it. No seat is created — the profile
/// rides the one console (`design/windows-take-turns.md`).
async fn create_take_turns(
    st: &MgmtState,
    profiles: &Profiles,
    input: ProfileCreate,
    account: String,
) -> Response {
    if !cfg!(windows) {
        return api_error(
            StatusCode::CONFLICT,
            "Take-turns profiles are only on Windows.",
        );
    }
    let bound = take_turns_bound(profiles);
    let vetted = blocking("Windows accounts", move || {
        let ledger = crate::seats::list().unwrap_or_default();
        bindable_account(&account, &ledger, &bound)
    });
    let account = match vetted.await {
        Ok(Ok(account)) => account,
        Ok(Err(refusal)) => return refused(refusal),
        Err(resp) => return resp,
    };
    let os_account = OsAccount::Windows {
        account_name: account.name,
        sid: None,
        credential: crate::profiles::CredentialRef::None,
    };
    match profiles.create(input, os_account) {
        Ok(p) => {
            let seats = seats_now().await;
            (StatusCode::CREATED, Json(admin(st, profiles, &p, &seats))).into_response()
        }
        Err(e) => edit_error(e),
    }
}

/// Windows accounts a new profile can bind to
///
/// The enabled local Windows accounts on this PC that no profile holds: none a seat signs in to,
/// none a take-turns profile uses, none of punktfunk's own seat accounts. Empty on every other
/// OS. The console's **Add profile** reads this for **Own account (take turns)** and for **Own
/// desktop** on an existing account.
#[utoipa::path(
    get,
    path = "/profiles/windows-accounts",
    tag = "profiles",
    operation_id = "windowsAccounts",
    responses(
        (status = OK, description = "Bindable local Windows account names", body = Vec<String>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_windows_accounts(State(st): State<Arc<MgmtState>>) -> Response {
    let bound = st
        .app
        .profiles
        .get()
        .map(|profiles| take_turns_bound(profiles))
        .unwrap_or_default();
    let accounts = tokio::task::spawn_blocking(move || {
        let ledger = crate::seats::list().unwrap_or_default();
        take_turns::local_accounts()
            .into_iter()
            .filter(|a| take_turns::bindable(a, &ledger, &bound))
            .map(|a| a.name)
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(accounts).into_response()
}

/// Rename, recolour or rehome a profile
///
/// `seat_password` gives a seat on an existing Windows account that account's new password.
/// The seat supervisor checks it with Windows before it keeps it, then the rest applies.
#[utoipa::path(
    put,
    path = "/profiles/{id}",
    tag = "profiles",
    operation_id = "updateProfile",
    params(("id" = String, Path, description = "The profile id")),
    request_body = ProfileUpdate,
    responses(
        (status = OK, description = "The profile", body = ProfileAdmin),
        (status = BAD_REQUEST, description = "An empty name, an accent that is not #RRGGBB, or a password Windows refused", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "Another profile has that name, or a password for a profile with no existing Windows account", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
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
    if let Some(password) = input.seat_password.clone() {
        let seat = match profiles.get(&id).map(|p| p.os_account) {
            None => return edit_error(EditError::NotFound),
            Some(OsAccount::Seat { seat: Some(seat) }) => seat,
            Some(_) => return refused(reseal_refusal()),
        };
        match blocking("seat supervisor", move || reseal(&seat, password)).await {
            Ok(Ok(())) => {}
            Ok(Err(refusal)) => return refused(refusal),
            Err(resp) => return resp,
        }
    }
    match profiles.update(&id, input) {
        Ok(p) => Json(admin(&st, profiles, &p, &*seats_now().await)).into_response(),
        Err(e) => edit_error(e),
    }
}

/// Remove a profile
///
/// Sessions playing as it end with `SEAT_UNAVAILABLE`. With `erase`, its Steam home and picture
/// go too; without, the home stays. A Windows seat's account goes with its profile, and an
/// existing account a seat signs in to is released and stays; removing either needs `erase`.
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
            let adopted = seats_now()
                .await
                .seat(&seat)
                .is_some_and(|(row, _)| row.adopted_sid.is_some());
            return api_error(
                StatusCode::CONFLICT,
                if adopted {
                    "Removing this profile releases its Windows account; the account and its files stay. Confirm with erase."
                } else if cfg!(windows) {
                    "Removing this profile also removes its Windows account. Confirm with erase."
                } else {
                    "Removing this profile also removes its user account and files. Confirm with erase."
                },
            );
        }
        match blocking("seat supervisor", move || drop_seat(&seat)).await {
            Ok(Ok(())) => {}
            Ok(Err(refusal)) => return refused(refusal),
            Err(resp) => return resp,
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
    let stored = blocking("profile store", move || profiles.put_avatar(&id, &body)).await;
    match stored {
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => api_error(StatusCode::BAD_REQUEST, &format!("{e:#}")),
        Err(resp) => resp,
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
    /// The name of the profile that plays on that seat, when one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    seat_name: Option<String>,
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
            seat_name: None,
        }
    }
}

/// Seat id → the display name of the profile playing on it.
fn seat_names() -> std::collections::HashMap<String, String> {
    crate::profiles::Profiles::load_with(None, None)
        .list()
        .into_iter()
        .filter_map(|p| match p.os_account {
            crate::profiles::OsAccount::Seat { seat: Some(id), .. } => Some((id, p.display_name)),
            _ => None,
        })
        .collect()
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
    /// Windows Server: first install the Remote Desktop Session Host role and set licensing to
    /// Per User where they are missing. Installing the role can take minutes and asks for a
    /// restart.
    #[serde(default)]
    #[schema(required = false)]
    prepare: bool,
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
    match blocking("seat supervisor", move || crate::seats::call(command)).await? {
        Ok(result) => Ok(result),
        Err(e) => {
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
    if !crate::seats::has_supervisor() {
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
/// machine and records its certificate. With `prepare` it first installs the Session Host role
/// and sets licensing to Per User where they are missing. A check that fails changes nothing:
/// the answer is 200 with `enabled: false` and the failed check as an `error`. The driver
/// replaces the display of every Remote Desktop session on the machine. Off stops every seat,
/// keeps their accounts and restores what on changed. Admin lane only.
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
            if pf_paths::seat::role().is_door() {
                "Seats stay on while Reachable without logging in is on."
            } else {
                SEATS_NEED_WINDOWS
            },
        );
    }
    seating(if input.enabled {
        Command::Enable {
            allow_rdp_from_network: input.allow_rdp_from_network,
            prepare: input.prepare,
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
/// The supervisor's own report: Windows, Remote Desktop, the seat display driver, Steam, and
/// each seat's account, Steam, state and ports, each row one sentence with the next move.
/// Admin lane only.
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
    if !crate::seats::has_supervisor() {
        return api_error(StatusCode::CONFLICT, SEATS_NEED_WINDOWS);
    }
    match supervisor(Command::Doctor).await {
        Ok(CommandResult::Doctor { report }) => {
            let names = seat_names();
            Json(SeatsDoctor {
                healthy: report.healthy,
                diagnostics: report
                    .diagnostics
                    .into_iter()
                    .map(|d| {
                        let mut check = SeatCheck::from(d);
                        check.seat_name =
                            check.seat_id.as_ref().and_then(|id| names.get(id)).cloned();
                        check
                    })
                    .collect(),
            })
            .into_response()
        }
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
/// seat of their own. Off hands the door's own settings to the owner's host, then puts the files
/// and the host back. The answer is 202 once the switch is
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
        (status = BAD_GATEWAY, description = "The owner's host didn't take the door's settings", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "systemd didn't take the switch, or the owner's host is starting", body = ApiError),
    )
)]
pub(crate) async fn put_door(
    State(st): State<Arc<MgmtState>>,
    ApiJson(input): ApiJson<DoorChange>,
) -> Response {
    if !input.on && st.door.is_some() {
        if let Err(not_yet) = super::settings::hand_door_rows_home(&st).await {
            return not_yet;
        }
    }
    let answer = blocking("switch", move || crate::door::change(input.on)).await;
    // Whatever the answer, the console reads the blockers again next.
    super::host::forget_door_probe();
    match answer {
        Ok(Ok(crate::door::Outcome::Started)) => StatusCode::ACCEPTED.into_response(),
        Ok(Ok(crate::door::Outcome::Already)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(refusal)) => refused(refusal),
        Err(resp) => resp,
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
    let stopped = blocking("seat supervisor", move || {
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
        Err(resp) => return resp,
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
    let ended = blocking("seat", move || {
        let snap = crate::seats::snapshot();
        let row = snap.seat(&seat).map(|(s, _)| s.clone());
        let done = row.as_ref().is_some_and(crate::seats::end_sessions);
        crate::seats::invalidate();
        done
    })
    .await;
    match ended {
        Ok(true) => {}
        Ok(false) => {
            return api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The seat didn't answer. Stop it instead to disconnect whoever plays there.",
            )
        }
        Err(resp) => return resp,
    }
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    Json(admin(&st, profiles, &p, &*seats_now().await)).into_response()
}

/// Paths the seat proxy never passes: pairing, trust and profiles are the box's, a seat host
/// updates with the box, and a plugin UI's credential stays on its host. A `.` or `..` segment
/// is refused outright. `rest` is the decoded path; the query travels apart.
fn proxy_refuses(rest: &str) -> bool {
    let first = rest
        .trim_start_matches('/')
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    segments.iter().any(|s| *s == "." || *s == "..")
        || matches!(
            segments[..],
            [plugins, _, credential]
                if plugins.eq_ignore_ascii_case("plugins")
                    && credential.eq_ignore_ascii_case("ui-credential")
        )
        || matches!(
            first,
            "native" | "pair" | "profiles" | "update" | "actions" | "clients"
        )
}

/// `rest` and `query` as the seat's host reads them. The router hands `rest` over decoded, so each
/// segment is escaped again: an encoded `?` stays part of its segment and starts no query.
fn seat_target(rest: &str, query: Option<&str>) -> String {
    let path = rest
        .split('/')
        .map(super::library::path_segment)
        .collect::<Vec<_>>()
        .join("/");
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
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
    if row.owner && needs_start(&row.runtime.state) {
        start_seat(&seat);
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "The owner's session is starting. Try again in a moment.",
        );
    }
    let call = super::owner::Forwarded {
        method: method.as_str().to_string(),
        target: seat_target(&rest, uri.query()),
        content_type: headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: body.to_vec(),
        device: None,
        stream: super::owner::tail(&rest),
        start: false,
    };
    let forwarded = blocking("seat", move || crate::seats::forward(&row, call));
    let answer = match forwarded.await {
        Ok(Some(answer)) => answer,
        Ok(None) => {
            return api_error(
                StatusCode::BAD_GATEWAY,
                "The seat didn't answer. Start it, then try again.",
            )
        }
        Err(resp) => return resp,
    };
    match answer {
        super::owner::Answer::Reply(status, content_type, bytes) => seat_answer(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            content_type.as_deref(),
            bytes,
        ),
        // A tail is the seat host's own event frames: text, never a page.
        super::owner::Answer::Stream(status, _, body) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut resp = (status, body).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/event-stream"),
            );
            resp
        }
        other => other.into_response(),
    }
}

/// A seat host's answer, relayed on the console's origin. Only JSON and images keep their
/// type, SVG excepted because it runs script; the rest is served as inert bytes, so a seat
/// cannot run a page here.
fn seat_answer(status: StatusCode, content_type: Option<&str>, bytes: Vec<u8>) -> Response {
    let passes = |c: &&str| {
        let media = c
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        media == "application/json" || (media.starts_with("image/") && !media.contains("svg"))
    };
    let content_type = content_type
        .filter(passes)
        .and_then(|c| header::HeaderValue::from_str(c).ok())
        .unwrap_or(header::HeaderValue::from_static("application/octet-stream"));
    let mut resp = (status, bytes).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
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
            single_session: false,
            seats: vec![seat],
            occupants,
            statuses: Default::default(),
        }
    }

    #[test]
    fn the_seat_proxy_keeps_the_box_s_own_routes_to_itself() {
        assert!(!proxy_refuses("library"));
        assert!(!proxy_refuses("library/page?as=kid"));
        assert!(!proxy_refuses("plugins/rom-manager"));
        assert!(!proxy_refuses("plugins/steam/ui"));
        for refused in [
            "native/pair/arm",
            "profiles",
            "pair/pin",
            "update",
            "actions/power.sleep/invoke",
            "clients",
            "library/../native/pair/arm",
            "./native",
            "plugins/steam/ui-credential",
            "Plugins/steam/UI-credential/",
            "x?/../update",
        ] {
            assert!(proxy_refuses(refused), "{refused}");
        }
    }

    /// The router hands `rest` over decoded. An encoded `?` or `%` must reach the seat as the
    /// character it is, not as a query or an escape the seat decodes a second time.
    #[tokio::test]
    async fn the_router_decodes_the_path_the_seat_proxy_re_escapes() {
        use tower::ServiceExt;
        let app = axum::Router::new().route(
            "/profiles/{id}/proxy/{*rest}",
            axum::routing::any(|Path((_, rest)): Path<(String, String)>| async move { rest }),
        );
        let req = axum::http::Request::builder()
            .uri("/profiles/a/proxy/store/install%3F/%2e%2e/a%252Fb")
            .body(axum::body::Body::empty())
            .unwrap();
        let body = axum::body::to_bytes(app.oneshot(req).await.unwrap().into_body(), 1024)
            .await
            .unwrap();
        let rest = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(rest, "store/install?/../a%2Fb");
        assert!(proxy_refuses(&rest));
    }

    #[test]
    fn a_decoded_path_reaches_the_seat_as_the_segments_it_had() {
        assert_eq!(seat_target("library/steam:570", None), "library/steam:570");
        assert_eq!(
            seat_target("store/sources/my src", Some("as=kid&x=1")),
            "store/sources/my%20src?as=kid&x=1"
        );
        assert_eq!(seat_target("store/install?", None), "store/install%3F");
        assert_eq!(seat_target("a%2Fb", None), "a%252Fb");
        assert_eq!(seat_target("rom/é#1", None), "rom/%C3%A9%231");
    }

    /// The console asks for its password on some calls and relays the rest with the admin
    /// bearer. A gated row with no seat handler in its table must be a path this proxy refuses.
    #[test]
    fn a_call_the_console_gates_only_on_the_box_never_passes_the_seat_proxy() {
        let table = include_str!("../../../../../web/server/util/gates.ts");
        let rows: Vec<Vec<&str>> = table
            .split("gated(")
            .skip(1)
            .filter_map(|row| row.split_once(')'))
            .map(|(args, _)| {
                args.split(',')
                    .map(|a| a.trim().trim_matches('"'))
                    .filter(|a| !a.is_empty())
                    .collect()
            })
            .collect();
        let box_only: Vec<&str> = rows
            .iter()
            .filter(|args| args.len() == 2)
            .map(|args| args[1])
            .collect();
        assert!(rows.len() > box_only.len() && !box_only.is_empty());
        for pattern in box_only {
            assert!(proxy_refuses(pattern), "{pattern}");
        }
    }

    #[test]
    fn a_seat_cannot_serve_a_page_from_the_console_s_origin() {
        let served = |ct: Option<&str>| {
            let resp = seat_answer(StatusCode::OK, ct, b"<script>1</script>".to_vec());
            let get = |h| resp.headers()[h].to_str().unwrap().to_string();
            (
                get(header::CONTENT_TYPE),
                get(header::CONTENT_SECURITY_POLICY),
                get(header::X_CONTENT_TYPE_OPTIONS),
            )
        };
        for kept in [
            "application/json",
            "Application/JSON; charset=utf-8",
            "image/webp",
        ] {
            let (ct, csp, sniff) = served(Some(kept));
            assert_eq!(
                (ct.as_str(), csp.as_str(), sniff.as_str()),
                (kept, "sandbox", "nosniff")
            );
        }
        for page in [
            "text/html",
            "text/html; charset=utf-8",
            "application/xhtml+xml",
            "image/svg+xml",
            "IMAGE/SVG+XML; charset=utf-8",
            "image",
            "",
        ] {
            assert_eq!(served(Some(page)).0, "application/octet-stream", "{page}");
        }
        let (ct, csp, sniff) = served(None);
        assert_eq!(
            (ct.as_str(), csp.as_str(), sniff.as_str()),
            ("application/octet-stream", "sandbox", "nosniff")
        );
    }

    #[test]
    fn a_seat_wakes_once_at_a_time_and_a_stopping_one_wakes_too() {
        let first = Waking::claim("wake-test-seat").expect("nothing is waking it");
        assert!(Waking::claim("wake-test-seat").is_none());
        assert!(Waking::claim("wake-test-other").is_some());
        drop(first);
        assert!(Waking::claim("wake-test-seat").is_some());

        for state in [
            RuntimeState::Stopped,
            RuntimeState::Stopping,
            RuntimeState::Failed,
            RuntimeState::Unknown,
        ] {
            assert!(needs_start(&state), "{state:?}");
        }
        assert!(!needs_start(&RuntimeState::Running));
        assert!(!needs_start(&RuntimeState::Starting));
    }

    /// A backend refusal reaches the console as its sentence, under a status the person can act
    /// on when its code says so; any other message stays whole.
    #[test]
    fn a_supervisor_refusal_loses_its_code_before_the_console() {
        let refusal = seat_error(SeatError::new(
            ErrorCode::Backend,
            "credential_wrong: That isn't Ben's Windows password.",
        ));
        assert_eq!(
            refusal,
            (
                StatusCode::BAD_REQUEST,
                "That isn't Ben's Windows password.".to_string()
            )
        );
        for (message, status) in [
            ("account_locked: Wait ten minutes.", StatusCode::BAD_REQUEST),
            ("account_signed_in: Sign Ben out.", StatusCode::CONFLICT),
            ("account_at_console: Sign Ben out.", StatusCode::CONFLICT),
            (
                "lsa_failed: LsaAddAccountRights",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            (
                "seats run on a Windows Server",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            assert_eq!(backend_status(message), status, "{message}");
        }
        for whole in [
            "seat 0123 does not exist",
            "duplicate account 'pf_seat1'",
            "Turn seats on: then try again",
            "pf-seat: not a code",
            ": no code",
        ] {
            assert_eq!(sentence_of(whole), whole);
        }
    }

    #[test]
    fn only_the_console_reads_why_a_seat_failed() {
        let mut failed = snap(true, RuntimeState::Failed, false);
        failed.seats[0].runtime.detail =
            Some("systemctl_failed: systemctl start punktfunk-seat@pf-seat-2".into());
        let detail = |operator| seated(&failed, ID, operator).detail;
        assert_eq!(detail(false).as_deref(), Some("This seat didn't start."));
        assert!(detail(true).unwrap().starts_with("systemctl_failed"));
    }

    #[test]
    fn a_windows_seat_reads_its_ledger_row() {
        let ready = seated(&snap(true, RuntimeState::Running, false), ID, false);
        assert_eq!((ready.state, ready.port), (SeatState::Ready, 9778));
        let busy = seated(&snap(true, RuntimeState::Running, true), ID, false);
        assert_eq!(
            (busy.state, busy.occupant.as_deref()),
            (SeatState::Occupied, Some("Ben's Apple TV"))
        );
        let state = |on, s| seated(&snap(on, s, false), ID, false).state;
        assert_eq!(state(true, RuntimeState::Starting), SeatState::Starting);
        assert_eq!(state(true, RuntimeState::Stopped), SeatState::Stopped);
        assert_eq!(state(true, RuntimeState::Failed), SeatState::Unavailable);
        assert_eq!(state(false, RuntimeState::Running), SeatState::Unavailable);
        let gone = seated(
            &snap(true, RuntimeState::Running, false),
            &"f".repeat(32),
            false,
        );
        assert_eq!(gone.state, SeatState::Unavailable);
        let desktop = Snapshot {
            single_session: true,
            ..snap(false, RuntimeState::Stopped, false)
        };
        assert_eq!(
            seated(&desktop, ID, false).detail.as_deref(),
            Some("This Windows edition serves one person at a time.")
        );
    }
}
