//! The people on this box (`design/profiles-and-seats.md` §7).
//!
//! Four routes are on the cert lane, for every paired device: the list a client's picker
//! shows, a profile's picture, waking its seat and a key unlock's challenge. Everything that
//! changes a profile is the console's, among them turning seats on, locking a profile and
//! signing devices out. A paired device may pick an open profile, and a locked one whose list
//! holds it or whose PIN it brings (`profiles/locks.rs`); starting another profile's seat takes
//! the launch grant. A profile on a person's own Linux account is the exception: only the seat
//! supervisor lets a device onto it, through the unlock at connect (`native/unlock.rs`), and
//! nothing here starts it.

use super::auth::{AuthLane, PairedDevice};
use super::owner::{LiveSeats, Seats};
use super::shared::*;
use crate::profiles::take_turns::{self, LocalAccount};
use crate::profiles::{
    EditError, Home, Opens, OsAccount, Profile, ProfileCreate, ProfileUpdate, Profiles,
};
use crate::seats::Snapshot;
use axum::http::header;
use axum::Extension;
use pf_seats::ipc::{ApiError as SeatError, Command, CommandResult, DeviceKind, ErrorCode, Needs};
use pf_seats::{RuntimeState, Secret};
use std::collections::BTreeMap;

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
    /// What the asking device sends to open it. Every row of `enumerate` has it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    unlock: Option<UnlockNeed>,
}

/// What a device sends with its connect to open a profile: a profile on a person's own Linux
/// account, or a locked one. A client that doesn't know a word reads it as `password`.
#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnlockNeed {
    /// Nothing: an open profile, a locked one that lists this device, or a device already
    /// signed in to a running desktop.
    None,
    /// The account's password.
    Password,
    /// A signature from this device's unlock key over a fresh `unlock-challenge`.
    Key,
    /// A locked profile's PIN, 4 to 8 digits, sent as the unlock's secret.
    Pin,
    /// A signature from this device's unlock key over a fresh `unlock-challenge`, or the locked
    /// profile's PIN.
    PinKey,
}

impl From<Needs> for UnlockNeed {
    fn from(needs: Needs) -> Self {
        match needs {
            Needs::None => UnlockNeed::None,
            Needs::Password => UnlockNeed::Password,
            Needs::Key => UnlockNeed::Key,
        }
    }
}

/// `POST /profiles/{id}/unlock-challenge`: `seat` for a profile on a person's own Linux account,
/// `profile` for a locked one, never both.
#[derive(Serialize, ToSchema)]
pub(crate) struct UnlockChallenge {
    /// 32 random bytes, lowercase hex: good for one proof within two minutes.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    pub(super) challenge: String,
    /// The seat's id, 32 lowercase hex characters, as the signed message names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = String, required = false, example = "0123456789abcdef0123456789abcdef")]
    pub(super) seat: Option<String>,
    /// The locked profile's id, as its signed message names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = String, required = false, example = "9a3f1c2b7e40")]
    pub(super) profile: Option<String>,
}

/// A device signed in to a profile on a person's own Linux account, or on a locked profile's
/// list.
#[derive(Serialize, ToSchema)]
pub(crate) struct SignedInDevice {
    /// SHA-256 of the device's certificate, lowercase hex.
    fingerprint: String,
    /// The name it paired with.
    name: String,
    kind: SignedInKind,
    /// A Moonlight pairing rather than a Punktfunk one. Never on a person's own account.
    pub(super) moonlight: bool,
    /// When it first signed in, unix seconds.
    added_unix: i64,
    /// When it last unlocked or rejoined, unix seconds.
    last_unix: i64,
}

impl From<pf_seats::ipc::DeviceEntry> for SignedInDevice {
    fn from(d: pf_seats::ipc::DeviceEntry) -> Self {
        SignedInDevice {
            fingerprint: d.fingerprint,
            name: d.name,
            kind: match d.kind {
                DeviceKind::ForNow => SignedInKind::ForNow,
                DeviceKind::Stay => SignedInKind::Stay,
                DeviceKind::Added => SignedInKind::Added,
                DeviceKind::Key => SignedInKind::Key,
            },
            moonlight: false,
            added_unix: d.added_unix,
            last_unix: d.last_unix,
        }
    }
}

impl SignedInDevice {
    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

/// How a signed-in device gets back on.
#[derive(Serialize, ToSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SignedInKind {
    /// Until ten minutes after it left, at most a day.
    ForNow,
    /// Gets back on without the password or PIN until it is signed out.
    Stay,
    /// The operator put it on a locked profile's list: it opens it until signed out.
    Added,
    /// Proves itself with its unlock key on every connect.
    Key,
}

/// A profile's seat right now.
#[derive(Serialize, ToSchema, Clone)]
pub(crate) struct SeatPublic {
    state: SeatState,
    /// The progress line while `starting`, or why while `unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// The host message `detail` is the English of, worded with `detail_params`.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail_id: Option<String>,
    /// The values of that message's placeholders, by name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(required = false)]
    detail_params: BTreeMap<String, String>,
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
    /// The account of a seat on an existing one, for the console's card and its Remove
    /// sentence. Absent for an account punktfunk made, and to a paired device.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    account: Option<String>,
    /// The management port of the host on its ledger row, for the console. Absent to a paired
    /// device, and for a profile with no row.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    mgmt_port: Option<u16>,
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
    /// Its lock. Absent for a profile on a person's own account, which opens with its password.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    lock: Option<super::locks::LockSummary>,
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

pub(super) fn no_profiles() -> Response {
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
        unlock: None,
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
        OsAccount::Seat { .. } | OsAccount::Windows { .. } => SeatKind::Desktop,
    };
    SeatPublic {
        kind,
        ..seat_line(st, p, seats, operator)
    }
}

/// Attach `account` to the console when its seat reads switchable — the switch a take-turns
/// `wake` triggers. Any other seat is left alone: on screen, signed in nowhere, switching, or
/// held by someone else's live stream; its seat line tells the client why. Blocking (WTS +
/// `tscon`).
fn take_turns_switch(account: &str) {
    let (action, seat) = take_turns::seat_now(account);
    match take_turns::wake_switch(&action, &seat) {
        Some(session) => {
            if let Err(e) = take_turns::switch_to(account, session) {
                tracing::warn!(%account, error = %format!("{e:#}"), "take-turns switch did not run");
            }
        }
        None => {
            tracing::debug!(%account, ?seat, "take-turns wake: no switch");
        }
    }
}

/// A take-turns profile's seat from the console state: `ready` when its account is on screen,
/// `stopped` when it can be switched in (the client's `wake` does it), `starting` while a switch
/// runs, `unavailable` when it must sign in once or someone else holds the console
/// (`design/windows-take-turns.md` §2). Not a managed seat — no pin, no per-seat host.
fn take_turns_seat(st: &MgmtState, display_name: &str, account: &str) -> SeatPublic {
    use crate::profiles::take_turns::TurnSeat;
    let (state, detail) = match take_turns::seat_now(account).1 {
        TurnSeat::Ready => (SeatState::Ready, None),
        TurnSeat::Switching => (
            SeatState::Starting,
            Some((
                "seatline_switching",
                format!("Switching to {display_name}…"),
            )),
        ),
        TurnSeat::Switchable => (SeatState::Stopped, None),
        TurnSeat::SignIn => (
            SeatState::Unavailable,
            Some((
                "seatline_sign_in_once",
                format!("Sign {display_name} in on the PC once."),
            )),
        ),
        TurnSeat::Occupied => (
            SeatState::Unavailable,
            Some((
                "seatline_pc_occupied",
                "Someone else is playing on this PC right now.".to_string(),
            )),
        ),
        TurnSeat::Gone => (
            SeatState::Unavailable,
            Some((
                "seatline_windows_account_gone",
                "This Windows account is gone.".to_string(),
            )),
        ),
    };
    let detail_params = match &detail {
        Some(("seatline_switching" | "seatline_sign_in_once", _)) => {
            BTreeMap::from([("profile".to_string(), display_name.to_string())])
        }
        _ => BTreeMap::new(),
    };
    SeatPublic {
        state,
        detail_id: detail.as_ref().map(|(id, _)| id.to_string()),
        detail: detail.map(|(_, text)| text),
        detail_params,
        port: st.app.native_port.get().copied().unwrap_or(0),
        occupant: None,
        pin: None,
        kind: SeatKind::Desktop,
        take_turns: true,
        account: None,
        mgmt_port: None,
    }
}

/// The seat's state and port, with a placeholder kind that [`seat`] replaces. The `operator`
/// also reads the Windows account of a seat on an existing one.
fn seat_line(st: &MgmtState, p: &Profile, seats: &Snapshot, operator: bool) -> SeatPublic {
    let port = st.app.native_port.get().copied().unwrap_or(0);
    let unavailable = |(id, why): (&str, &str)| SeatPublic {
        state: SeatState::Unavailable,
        detail: Some(why.to_string()),
        detail_id: Some(id.to_string()),
        detail_params: BTreeMap::new(),
        port,
        occupant: None,
        pin: None,
        kind: SeatKind::Desktop,
        take_turns: false,
        account: None,
        mgmt_port: None,
    };
    if let Some(id) = seats.row_of(&p.os_account) {
        let account = seats
            .seat(id)
            .filter(|(row, _)| operator && row.adopted())
            .map(|(row, _)| row.account.clone());
        return SeatPublic {
            account,
            ..seated(seats, id, operator)
        };
    }
    if pf_paths::seat::role().is_door() {
        return unavailable((
            "seatline_no_owner_seat",
            "This host has no seat for the owner yet.",
        ));
    }
    match &p.os_account {
        OsAccount::Seat { .. } | OsAccount::Operator => SeatPublic {
            state: SeatState::Ready,
            detail: None,
            detail_id: None,
            detail_params: BTreeMap::new(),
            port,
            occupant: None,
            pin: None,
            kind: SeatKind::Desktop,
            take_turns: false,
            account: None,
            mgmt_port: None,
        },
        OsAccount::Windows { .. } => unavailable((
            "seatline_account_not_startable",
            "This profile signs in to an account this host can't start yet.",
        )),
    }
}

/// A seat's line: its ledger row, who plays there, and its host's pin. Why a seat failed is the
/// supervisor's own text (unit, account, ports): only the `operator` reads it.
fn seated(seats: &Snapshot, id: &str, operator: bool) -> SeatPublic {
    let line = |state, port, detail: Option<(&str, &str)>| SeatPublic {
        state,
        detail: detail.map(|(_, text)| text.to_string()),
        detail_id: detail.map(|(id, _)| id.to_string()),
        detail_params: BTreeMap::new(),
        port,
        occupant: None,
        pin: None,
        kind: SeatKind::Desktop,
        take_turns: false,
        account: None,
        mgmt_port: None,
    };
    let Some((seat, _)) = seats.seat(id) else {
        let why = if !seats.on && pf_paths::seat::role().is_door() {
            (
                "seatline_seat_service_silent",
                "The box's seat service isn't answering.",
            )
        } else {
            (
                "seatline_seat_gone",
                "This profile's seat is gone. Remove the profile and add it again.",
            )
        };
        return line(SeatState::Unavailable, 0, Some(why));
    };
    let port = seat.native_port;
    if !seats.on {
        let why = if seats.single_session {
            (
                "seatline_single_session",
                "This Windows edition serves one person at a time.",
            )
        } else {
            (
                "seatline_seats_off",
                "This host needs seats turned on for a second profile.",
            )
        };
        return line(SeatState::Unavailable, port, Some(why));
    }
    // The supervisor's own line, with its message where it names one.
    let said = |state| SeatPublic {
        detail: seat.runtime.detail.clone(),
        detail_id: seat.runtime.detail_id.clone(),
        detail_params: seat.runtime.detail_params.clone(),
        ..line(state, port, None)
    };
    let mut out = match seat.runtime.state {
        RuntimeState::Running => match seats.occupants.get(id).and_then(|o| o.first()) {
            Some(o) => SeatPublic {
                occupant: o.name.clone(),
                ..line(SeatState::Occupied, port, None)
            },
            None => line(SeatState::Ready, port, None),
        },
        RuntimeState::Starting => said(SeatState::Starting),
        RuntimeState::Failed if operator && seat.runtime.detail.is_some() => {
            said(SeatState::Unavailable)
        }
        RuntimeState::Failed => line(
            SeatState::Unavailable,
            port,
            Some(("seatline_not_started", "This seat didn't start.")),
        ),
        RuntimeState::Stopping | RuntimeState::Stopped | RuntimeState::Unknown => {
            line(SeatState::Stopped, port, None)
        }
    };
    out.pin = seat.fingerprint.clone();
    out
}

/// The seats as they are now, as a door reaches them or else the supervisor; the empty snapshot
/// where nothing answers.
pub(super) async fn seats_now(st: &MgmtState) -> Arc<Snapshot> {
    if st.door.is_some() {
        return st.seats_snapshot().await;
    }
    tokio::task::spawn_blocking(crate::seats::snapshot)
        .await
        .unwrap_or_default()
}

/// Why a profile on a person's own Linux account is never started from here.
fn opens_with_password() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "profile_opens_with_password",
        "This profile opens with its account password.",
    )
}

fn not_a_person() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "profile_not_password",
        "This profile doesn't open with an account password.",
    )
}

/// The person's own Linux account `id` plays on, from the door's ledger; `None` for a profile
/// that is not on a person's own account.
pub(super) async fn person_seat(
    st: &MgmtState,
    id: &str,
) -> Result<Option<pf_seats::SeatId>, Response> {
    let Some(profiles) = st.app.profiles.get() else {
        return Err(no_profiles());
    };
    let Some(p) = profiles.get(id) else {
        return Err(api_error(StatusCode::NOT_FOUND, "no profile with that id"));
    };
    let ledger = match st.door.clone() {
        Some(door) => blocking("seat_ledger", move || door.ledger()).await?,
        None => Snapshot::default(),
    };
    Ok(ledger.person_row(&p.os_account).map(|row| row.id.clone()))
}

/// One request to the door's supervisor about a person's own account. A refusal reads as its
/// sentence; a supervisor that didn't answer is a 503.
async fn ask_door(st: &MgmtState, command: Command) -> Result<CommandResult, Response> {
    let Some(door) = st.door.clone() else {
        return Err(not_a_person());
    };
    match blocking("seat_supervisor", move || door.ask(command)).await? {
        Ok(answer) => Ok(answer),
        Err(e) if e.code == ErrorCode::Backend => Err(refused(seat_error(e))),
        Err(e) => {
            tracing::warn!(code = ?e.code, "seat supervisor request failed");
            Err(api_error_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "seat_supervisor_unreachable",
                "Couldn't reach the seat supervisor. Try again in a moment.",
            ))
        }
    }
}

pub(super) fn unexpected() -> Response {
    api_error_coded(
        StatusCode::INTERNAL_SERVER_ERROR,
        "seat_supervisor_unexpected",
        "The seat supervisor answered something unexpected.",
    )
}

/// What `device` sends to open each profile on a person's own Linux account, by profile id:
/// the supervisor's answer, `password` when it gives none or no device asks.
async fn unlock_needs(
    st: &MgmtState,
    seats: &Snapshot,
    profiles: &[Profile],
    device: Option<&str>,
) -> std::collections::BTreeMap<String, UnlockNeed> {
    let rows: Vec<(String, pf_seats::SeatId)> = profiles
        .iter()
        .filter_map(|p| Some((p.id.clone(), seats.person_row(&p.os_account)?.id.clone())))
        .collect();
    let mut needs: std::collections::BTreeMap<_, _> = rows
        .iter()
        .map(|(profile, _)| (profile.clone(), UnlockNeed::Password))
        .collect();
    let (Some(door), Some(fp)) = (st.door.clone(), device.map(str::to_ascii_lowercase)) else {
        return needs;
    };
    if rows.is_empty() {
        return needs;
    }
    let asked = blocking("seat_supervisor", move || {
        let state = |id| Command::UnlockState {
            id,
            fingerprint: fp.clone(),
        };
        rows.into_iter()
            .filter_map(|(profile, id)| match door.ask(state(id)) {
                Ok(CommandResult::UnlockState { needs }) => Some((profile, needs.into())),
                _ => None,
            })
            .collect::<Vec<_>>()
    })
    .await;
    needs.extend(asked.unwrap_or_default());
    needs
}

/// A refusal to answer with: the status and its body.
type Refusal = (StatusCode, ApiError);

pub(super) fn refused((status, error): Refusal) -> Response {
    error.respond(status)
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
    let sentence = sentence_of(&e.message);
    let error = match e.said {
        Some(code) => ApiError {
            params: (!e.params.is_empty()).then_some(e.params),
            ..ApiError::coded(&code, sentence)
        },
        None => ApiError::plain(sentence),
    };
    (status, error)
}

/// A backend refusal's status, by the code its message leads with: the person can fix a password
/// or a key proof (400), wait out too many wrong ones (429), or pick another account, sign out at
/// the machine or unlock with the password first (409); anything else is the box's (503).
fn backend_status(message: &str) -> StatusCode {
    match message.split_once(": ").map(|(code, _)| code) {
        Some("unlock_locked_out") => StatusCode::TOO_MANY_REQUESTS,
        Some(
            "credential_wrong"
            | "credential_must_change"
            | "credential_invalid"
            | "account_locked"
            | "account_missing"
            | "account_invalid"
            | "proof_invalid",
        ) => StatusCode::BAD_REQUEST,
        // `account_at_console` is the older name of `account_signed_in`.
        Some(
            "account_is_administrator"
            | "account_signed_in"
            | "account_at_console"
            | "account_at_desk"
            | "account_policy"
            | "account_not_owned"
            | "account_not_ordinary"
            | "account_is_owner"
            | "account_taken"
            | "unlock_required",
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

fn seats_off() -> Refusal {
    (
        StatusCode::CONFLICT,
        ApiError::coded(
            "seats_off_for_desktop",
            "Turn seats on before adding a profile with a desktop of its own.",
        ),
    )
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

/// The seat supervisor: the door's, or this host's own.
fn seats_of(st: &MgmtState) -> Arc<dyn Seats> {
    st.door.clone().unwrap_or_else(|| Arc::new(LiveSeats))
}

/// A seat of its own for a new profile, on an account made for it that no row in `ledger` holds.
fn new_seat(sup: &dyn Seats, ledger: &[pf_seats::Seat], name: &str) -> Result<String, Refusal> {
    let account = (1..=4)
        .map(seat_account)
        .find(|a| !ledger.iter().any(|s| s.account == *a))
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiError::coded(
                    "seats_full",
                    "This host already serves four seats, the most it can.",
                ),
            )
        })?;
    created(sup.ask(Command::Create(pf_seats::CreateSeat {
        name: name.trim().to_string(),
        account,
        autostart: false,
    })))
}

/// A seat for a new profile on `account`, an existing account no other profile holds. On
/// Windows a local account no row in `ledger` and no take-turns profile (`bound`) holds, which
/// it signs in to with `password`; on Linux a person's login, whose `password` the supervisor
/// checks once and forgets.
fn adopt_seat(
    sup: &dyn Seats,
    ledger: &[pf_seats::Seat],
    name: &str,
    account: &str,
    password: Secret,
    bound: &[String],
) -> Result<String, Refusal> {
    if !cfg!(windows) {
        return adopt_user(sup, name, account, password);
    }
    let account = bindable_account(account, ledger, bound)?;
    created(sup.ask(Command::Adopt {
        seat: pf_seats::CreateSeat {
            name: name.trim().to_string(),
            account: account.name,
            autostart: false,
        },
        sid: account.sid,
        password,
    }))
}

/// A seat for a new profile on the Linux login `account`. The supervisor reads its uid and
/// refuses the owner, a system account and one a row holds; the host only keeps a name the
/// supervisor can pass to the system.
fn adopt_user(
    sup: &dyn Seats,
    name: &str,
    account: &str,
    password: Secret,
) -> Result<String, Refusal> {
    let account = account.trim();
    if !pf_seats::model::valid_owner_account(account) {
        return Err((
            StatusCode::BAD_REQUEST,
            ApiError::coded(
                "account_name_lowercase",
                "Enter the account's Linux user name, in lowercase.",
            ),
        ));
    }
    created(sup.ask(Command::AdoptUser {
        seat: pf_seats::CreateSeat {
            name: name.trim().to_string(),
            account: account.to_string(),
            autostart: false,
        },
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
            ApiError::plain(format!("seats create answered {other:?}")),
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
            ApiError::coded(
                "windows_account_missing",
                "No Windows account by that name on this PC.",
            ),
        ));
    };
    if !take_turns::bindable(&account, ledger, bound) {
        return Err((
            StatusCode::CONFLICT,
            ApiError::coded(
                "windows_account_taken",
                "Another profile already uses that Windows account.",
            ),
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
        ApiError::coded(
            "profile_no_password_to_update",
            "Only a profile with a desktop of its own on an existing Windows account has a password to update.",
        ),
    )
}

/// Stops a seat and removes it: an account made for it goes too, an existing one is released.
/// A seat already gone is no error. Blocking.
fn drop_seat(sup: &dyn Seats, id: &str) -> Result<(), Refusal> {
    let Ok(id) = pf_seats::SeatId::parse(id) else {
        return Ok(());
    };
    let _ = sup.ask(Command::Stop { id: id.clone() });
    let removed = sup.ask(Command::Delete { id });
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

/// Whether the door may start `seat`: one that is down, and never a person's own Linux account,
/// which starts only for its person's unlock.
fn may_start(seat: &pf_seats::Seat) -> bool {
    needs_start(&seat.runtime.state) && seat.adopted_uid.is_none()
}

/// Starts a stopped seat in the background when [`may_start`] lets it: a first logon takes tens
/// of seconds, and the caller answers with the seat as it is now, `starting`.
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
            let idle = seats.seat(id.as_str()).is_some_and(|(s, _)| may_start(s));
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

/// A profile as the operator reads it: why its seat failed, and its seat host's management port.
fn admin(st: &MgmtState, profiles: &Profiles, p: &Profile, seats: &Snapshot) -> ProfileAdmin {
    let owner = profiles.owner_id();
    // As `person_seat` decides it: only a door's ledger names a person's own account.
    let own_account = st.door.is_some() && seats.person_row(&p.os_account).is_some();
    let mut public = public(st, p, owner.as_deref(), None, seats, true);
    if let Some(seat) = public.seat.as_mut() {
        let row = seats.row_of(&p.os_account).and_then(|id| seats.seat(id));
        seat.mgmt_port = row.map(|(row, _)| row.mgmt_port);
    }
    ProfileAdmin {
        public,
        legacy_device: p.legacy_device.clone(),
        default: profiles.default_profile_id().as_deref() == Some(p.id.as_str()),
        steam_home: keeps_steam_home(&p.id),
        lock: (!own_account).then(|| super::locks::summary(profiles, &p.id)),
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

pub(super) fn edit_error(e: EditError) -> Response {
    let text = e.to_string();
    match e {
        EditError::NotFound => api_error(StatusCode::NOT_FOUND, &text),
        EditError::Invalid(_) => api_error(StatusCode::BAD_REQUEST, &text),
        EditError::NameMissing => {
            api_error_coded(StatusCode::BAD_REQUEST, "profile_name_missing", &text)
        }
        EditError::NameTaken => api_error_coded(StatusCode::CONFLICT, "profile_name_taken", &text),
        EditError::Owner => {
            api_error_coded(StatusCode::CONFLICT, "profile_owner_not_removable", &text)
        }
        EditError::Full => ApiError::coded("profile_limit", text)
            .param("count", crate::profiles::PROFILES_MAX)
            .respond(StatusCode::UNPROCESSABLE_ENTITY),
        EditError::Store(e) => host_error("profile store refused", &format!("{e:#}")),
    }
}

/// List the profiles a device can pick
///
/// The profiles the asking device may open, the owner first, and the locked ones that take a
/// PIN; the console lists every profile. A client shows this as its picker. Each row's `unlock`
/// says what the asking device sends with its connect: a profile on a person's own Linux account
/// needs the account's password, or a key proof from a device that enrolled one, until the
/// device is signed in to its running desktop; a locked profile whose list lacks the device
/// needs its PIN.
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
    let seats = seats_now(&st).await;
    let operator = lane == AuthLane::Admin;
    let list = profiles.list();
    let needs = unlock_needs(&st, &seats, &list, device.as_deref()).await;
    let rows: Vec<ProfilePublic> = list
        .iter()
        .filter_map(|p| {
            let need = needs.get(&p.id).copied().unwrap_or(UnlockNeed::None);
            let unlock = match profiles.opens(&p.id, device.as_deref()) {
                _ if operator => need,
                Opens::Yes => need,
                Opens::Pin => UnlockNeed::Pin,
                Opens::Key => UnlockNeed::PinKey,
                Opens::No => return None,
            };
            let row = public(
                &st,
                p,
                owner.as_deref(),
                device.as_deref(),
                &seats,
                operator,
            );
            Some(ProfilePublic {
                unlock: Some(unlock),
                ..row
            })
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
/// needs its launch grant, and a locked one a device on its list. A profile on a person's own
/// Linux account is never woken: its desktop starts when the connect brings the account's
/// password.
#[utoipa::path(
    post,
    path = "/profiles/{id}/wake",
    tag = "profiles",
    operation_id = "wakeProfile",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The profile, its seat as it is now", body = ProfilePublic),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = FORBIDDEN, description = "This device may not start that profile's seat", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "The profile opens with its account password", body = ApiError),
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
    let seats = seats_now(&st).await;
    if seats.person_row(&p.os_account).is_some() {
        return opens_with_password();
    }
    let fp = device.map(|Extension(PairedDevice(fp))| fp);
    if lane != AuthLane::Admin && !may_wake(&st, profiles, &p.id, fp.as_deref()) {
        return api_error_coded(
            StatusCode::FORBIDDEN,
            "device_not_allowed_to_wake_seat",
            "This device isn't allowed to start another profile's seat.",
        );
    }
    // A take-turns profile has no ledger row: waking a switchable seat attaches its Windows
    // account to the console. The host that runs the switch does not survive it, so the client
    // polls the relaunched host; the switch marker carries the "Switching to …" line meanwhile.
    if let OsAccount::Windows { account_name, .. } = &p.os_account {
        let account = account_name.clone();
        let _ = tokio::task::spawn_blocking(move || take_turns_switch(&account)).await;
    }
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

/// A paired device wakes only a profile it opens with nothing asked ([`Profiles::opens`]): the
/// one it lands in when it names none, one that can't play yet included, and another only while
/// its live mask (expiry-aware) carries the launch grant.
fn may_wake(st: &MgmtState, profiles: &Profiles, id: &str, fp: Option<&str>) -> bool {
    let Some(fp) = fp else {
        return false;
    };
    if profiles.opens(id, Some(fp)) != Opens::Yes {
        return false;
    }
    let own = profiles.pick(Some(fp), None).is_ok_and(|r| r.id == id);
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
    let seats = seats_now(&st).await;
    let rows: Vec<ProfileAdmin> = profiles
        .list()
        .iter()
        .map(|p| admin(&st, profiles, p, &seats))
        .collect();
    Json(rows).into_response()
}

/// Add a profile
///
/// A seat profile (the default) gets a desktop of its own, whose account is made here, or runs
/// on `seat_account`, an existing account, once `seat_password` is its password: a local account
/// on Windows Server, a person's own login on a Linux door. A Linux row never starts while that
/// person sits at the machine, and the host keeps no password. One with `seat: false` plays the
/// box's own session under its own name. A seat needs seats on (Windows) or the door (Linux).
#[utoipa::path(
    post,
    path = "/profiles",
    tag = "profiles",
    operation_id = "createProfile",
    request_body = ProfileCreate,
    responses(
        (status = CREATED, description = "The new profile", body = ProfileAdmin),
        (status = BAD_REQUEST, description = "No name, an accent that is not #RRGGBB, an unknown account, or a password missing or refused", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "Another profile has that name or account, seats are off (Windows), the door is off (Linux), or an account a seat can't use (an administrator, a system account, the box owner)", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "The box already has eight profiles, or four seats", body = ApiError),
        (status = TOO_MANY_REQUESTS, description = "Too many wrong passwords for that account; the sentence says when to try again", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn create_profile(
    State(st): State<Arc<MgmtState>>,
    ApiJson(input): ApiJson<ProfileCreate>,
) -> Response {
    let Some(profiles) = st.app.profiles.get().cloned() else {
        return no_profiles();
    };
    // A take-turns profile binds to an existing Windows account; no seat is made.
    if let Some(account) = input.windows_account.clone() {
        return create_take_turns(&st, &profiles, input, account).await;
    }
    if let Some(refusal) = adopt_refusal(&input) {
        return refused(refusal);
    }
    // A Linux seat is the door's; elsewhere the ledger says whether seats are on.
    if input.seat && cfg!(target_os = "linux") && st.door.is_none() {
        return api_error_coded(
            StatusCode::CONFLICT,
            "profile_seat_needs_door",
            "Turn on Reachable without logging in before adding a profile with a desktop of its own.",
        );
    }
    let made = if input.seat {
        // Vetted before the seat's account exists: a refused profile must leave none behind.
        let name = match profiles.vet_create(&input) {
            Ok(name) => name,
            Err(e) => return edit_error(e),
        };
        let (sup, store) = (seats_of(&st), profiles.clone());
        let made = blocking("seat_supervisor", move || {
            seat_profile(&*sup, &store, input, &name)
        });
        match made.await {
            Ok(made) => made.map_err(|resp| *resp),
            Err(resp) => return resp,
        }
    } else {
        profiles
            .create(input, OsAccount::Operator)
            .map_err(edit_error)
    };
    match made {
        Ok(p) => {
            let seats = seats_now(&st).await;
            (StatusCode::CREATED, Json(admin(&st, &profiles, &p, &seats))).into_response()
        }
        Err(resp) => resp,
    }
}

/// Held from the check that no profile or seat holds an account to the write that takes it, so
/// a second create for that account sees the first.
static BINDING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Makes the seat a new profile plays on, then the profile; a profile the store refuses takes
/// the seat back. Blocking, and one task under [`BINDING`]: a caller that stops waiting still
/// leaves the two together or neither.
fn seat_profile(
    sup: &dyn Seats,
    profiles: &Profiles,
    input: ProfileCreate,
    name: &str,
) -> Result<Profile, Box<Response>> {
    let _binding = BINDING.lock().unwrap_or_else(|e| e.into_inner());
    let bound = take_turns_bound(profiles);
    let ledger = sup.ledger();
    if !ledger.on {
        return Err(refused(seats_off()).into());
    }
    // A row keeps the name it was made with through a rename, and the ledger takes no second.
    if ledger
        .seats
        .iter()
        .any(|s| !s.owner && crate::profiles::same_name(&s.name, name))
    {
        return Err(edit_error(EditError::NameTaken).into());
    }
    let seat = match input.seat_account.clone().zip(input.seat_password.clone()) {
        Some((account, password)) => {
            adopt_seat(sup, &ledger.seats, name, &account, password, &bound)
        }
        None => new_seat(sup, &ledger.seats, name),
    }
    .map_err(refused)?;
    let os_account = OsAccount::Seat {
        seat: Some(seat.clone()),
    };
    profiles.create(input, os_account).map_err(|e| {
        if let Err((_, why)) = drop_seat(sup, &seat) {
            tracing::error!(%seat, error = %why.error, "seat of a refused profile not removed");
        }
        edit_error(e).into()
    })
}

/// Why a seat can't sign in to the existing account the input names, if it names one.
fn adopt_refusal(input: &ProfileCreate) -> Option<Refusal> {
    input.seat_account.as_ref()?;
    let (status, code, why) = if !input.seat {
        (
            StatusCode::BAD_REQUEST,
            "existing_account_needs_desktop",
            "An existing account needs a desktop of its own.",
        )
    } else if !cfg!(any(windows, target_os = "linux")) {
        (
            StatusCode::CONFLICT,
            "existing_account_seat_platforms",
            "A seat on an existing account is only on Windows Server and Linux.",
        )
    } else if input.seat_password.is_none() {
        (
            StatusCode::BAD_REQUEST,
            "account_password_missing",
            "Enter the account's password.",
        )
    } else {
        return None;
    };
    Some((status, ApiError::coded(code, why)))
}

/// Make a take-turns profile bound to the local Windows account `account`, by its SID. Windows
/// only; the account must exist and no other profile or seat may hold it, checked and taken
/// under [`BINDING`]. No seat is created — the profile rides the one console
/// (`design/windows-take-turns.md`).
async fn create_take_turns(
    st: &MgmtState,
    profiles: &Arc<Profiles>,
    input: ProfileCreate,
    account: String,
) -> Response {
    if !cfg!(windows) {
        return api_error_coded(
            StatusCode::CONFLICT,
            "take_turns_windows_only",
            "Take-turns profiles are only on Windows.",
        );
    }
    let store = profiles.clone();
    let made = blocking("windows_accounts", move || {
        let _binding = BINDING.lock().unwrap_or_else(|e| e.into_inner());
        let ledger = crate::seats::list().unwrap_or_default();
        let account = bindable_account(&account, &ledger, &take_turns_bound(&store))
            .map_err(|refusal| Box::new(refused(refusal)))?;
        let os_account = OsAccount::Windows {
            account_name: account.name,
            sid: Some(account.sid),
            credential: crate::profiles::CredentialRef::None,
        };
        store
            .create(input, os_account)
            .map_err(|e| Box::new(edit_error(e)))
    });
    match made.await {
        Ok(Ok(p)) => {
            let seats = seats_now(st).await;
            (StatusCode::CREATED, Json(admin(st, profiles, &p, &seats))).into_response()
        }
        Ok(Err(resp)) => *resp,
        Err(resp) => resp,
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
        match blocking("seat_supervisor", move || reseal(&seat, password)).await {
            Ok(Ok(())) => {}
            Ok(Err(refusal)) => return refused(refusal),
            Err(resp) => return resp,
        }
    }
    match profiles.update(&id, input) {
        Ok(p) => Json(admin(&st, profiles, &p, &*seats_now(&st).await)).into_response(),
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
    let Some(profiles) = st.app.profiles.get().cloned() else {
        return no_profiles();
    };
    let Some(OsAccount::Seat {
        seat: Some(seat), ..
    }) = profiles.get(&id).map(|p| p.os_account)
    else {
        // `erase` can remove a Steam home of hundreds of GB.
        return match blocking("profile_store", move || profiles.delete(&id, q.erase)).await {
            Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
            Ok(Err(e)) => edit_error(e),
            Err(resp) => resp,
        };
    };
    if !q.erase {
        let adopted = seats_now(&st)
            .await
            .seat(&seat)
            .is_some_and(|(row, _)| row.adopted());
        return api_error(
            StatusCode::CONFLICT,
            if adopted {
                "Removing this profile releases its account; the account and its files stay. Confirm with erase."
            } else if cfg!(windows) {
                "Removing this profile also removes its Windows account. Confirm with erase."
            } else {
                "Removing this profile also removes its user account and files. Confirm with erase."
            },
        );
    }
    let sup = seats_of(&st);
    let removed = blocking("seat_supervisor", move || {
        remove_seat_profile(&*sup, &profiles, &id, &seat)
    });
    match removed.await {
        Ok(resp) | Err(resp) => resp,
    }
}

/// How long a seat's sessions get to close as removed: its host reads the box's profiles once a
/// second, on Linux from a copy the supervisor refreshes every 2 s.
const CLOSE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Takes `id` out of the store before its seat stops, so the seat's host closes each session as
/// removed. A seat that won't go puts the profile back for another try; its home and picture go
/// only once the seat has. Blocking.
fn remove_seat_profile(sup: &dyn Seats, profiles: &Profiles, id: &str, seat: &str) -> Response {
    let default = profiles.default_profile_id().as_deref() == Some(id);
    let gone = match profiles.delete(id, false) {
        Ok(gone) => gone,
        Err(e) => return edit_error(e),
    };
    let by = std::time::Instant::now() + CLOSE_GRACE;
    while std::time::Instant::now() < by {
        crate::seats::invalidate();
        if sup.snapshot().occupants.get(seat).is_none_or(Vec::is_empty) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    if let Err(refusal) = drop_seat(sup, seat) {
        if let Err(e) = profiles.restore(gone, default) {
            tracing::error!(%seat, error = %e, "seat left with no profile");
        }
        return refused(refusal);
    }
    profiles.erase(id);
    StatusCode::NO_CONTENT.into_response()
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
    let stored = blocking("profile_store", move || profiles.put_avatar(&id, &body)).await;
    match stored {
        Ok(Ok(_)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error = %format!("{e:#}"), "profile picture refused");
            api_error_coded(
                StatusCode::BAD_REQUEST,
                "profile_picture_invalid",
                "Couldn't save the picture. Use a PNG or JPEG under 1 MB.",
            )
        }
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
        Err(e) => host_error("profile picture not removed", &format!("{e:#}")),
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

/// The ledger row the profile plays on, or the answer for a profile that has none.
pub(super) fn seat_of(
    st: &MgmtState,
    id: &str,
    seats: &Snapshot,
) -> Result<(Profile, String), Refusal> {
    let Some(profiles) = st.app.profiles.get() else {
        return Err((
            StatusCode::NOT_FOUND,
            ApiError::plain("this host has no profiles"),
        ));
    };
    let Some(p) = profiles.get(id) else {
        return Err((
            StatusCode::NOT_FOUND,
            ApiError::plain("no profile with that id"),
        ));
    };
    match seats.row_of(&p.os_account) {
        Some(row) => {
            let row = row.to_string();
            Ok((p, row))
        }
        None => Err((
            StatusCode::CONFLICT,
            ApiError::coded(
                "profile_has_no_seat",
                "This profile plays on the host's own desktop; it has no seat to start or stop.",
            ),
        )),
    }
}

/// Start a profile's seat
///
/// The console's **Start**: as `wake`, from the console. A profile on a person's own Linux
/// account is refused: only its person's password at connect starts it.
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
        (status = CONFLICT, description = "The profile has no seat of its own, or opens with its account password", body = ApiError),
    )
)]
pub(crate) async fn start_profile_seat(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let seats = seats_now(&st).await;
    let (p, seat) = match seat_of(&st, &id, &seats) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    if seats.person_row(&p.os_account).is_some() {
        return opens_with_password();
    }
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
    let (p, seat) = match seat_of(&st, &id, &*seats_now(&st).await) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    let stopped = blocking("seat_supervisor", move || {
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
    Json(admin(&st, profiles, &p, &*seats_now(&st).await)).into_response()
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
    let (p, seat) = match seat_of(&st, &id, &*seats_now(&st).await) {
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
            return api_error_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "seat_no_answer_stop",
                "The seat didn't answer. Stop it instead to disconnect whoever plays there.",
            )
        }
        Err(resp) => return resp,
    }
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    Json(admin(&st, profiles, &p, &*seats_now(&st).await)).into_response()
}

/// Ask for a key unlock's challenge
///
/// For a device that enrolled an unlock key: 32 random bytes for the key to sign, and what the
/// signed message names, the seat id for a profile on a person's own Linux account or the
/// profile id for a locked profile. It is good for one proof within two minutes, and a new one
/// replaces it. A device without a key is refused; it unlocks with the account's password or the
/// profile's PIN. Paired devices only.
#[utoipa::path(
    post,
    path = "/profiles/{id}/unlock-challenge",
    tag = "profiles",
    operation_id = "unlockChallenge",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The challenge", body = UnlockChallenge),
        (status = BAD_REQUEST, description = "This machine's identity changed: unlock with the password", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = FORBIDDEN, description = "Not a paired device", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "This device has no unlock key for the profile", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn unlock_challenge(
    State(st): State<Arc<MgmtState>>,
    device: Option<Extension<PairedDevice>>,
    Path(id): Path<String>,
) -> Response {
    let Some(Extension(PairedDevice(fp))) = device else {
        return api_error(
            StatusCode::FORBIDDEN,
            "Only a paired device asks for its own unlock challenge.",
        );
    };
    let seat = match person_seat(&st, &id).await {
        Ok(Some(seat)) => seat,
        Ok(None) => return super::locks::challenge(&st, &id, &fp.to_ascii_lowercase()),
        Err(resp) => return resp,
    };
    let command = Command::Challenge {
        id: seat,
        fingerprint: fp.to_ascii_lowercase(),
    };
    match ask_door(&st, command).await {
        Ok(CommandResult::Challenge { challenge, seat }) => Json(UnlockChallenge {
            challenge,
            seat: Some(seat.as_str().to_string()),
            profile: None,
        })
        .into_response(),
        Ok(_) => unexpected(),
        Err(resp) => resp,
    }
}

/// List the devices signed in to a profile
///
/// For a profile on a person's own Linux account: each device that unlocked it, and how it gets
/// back on. For any other profile: the devices its lock lets in, the same rows, kept while it is
/// open. Admin lane only.
#[utoipa::path(
    get,
    path = "/profiles/{id}/devices",
    tag = "profiles",
    operation_id = "listProfileDevices",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The signed-in devices", body = Vec<SignedInDevice>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn list_profile_devices(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    let seat = match person_seat(&st, &id).await {
        Ok(Some(seat)) => seat,
        Ok(None) => return super::locks::devices(&st, &id).await,
        Err(resp) => return resp,
    };
    let devices = match ask_door(&st, Command::Devices { id: seat }).await {
        Ok(CommandResult::Devices { devices }) => devices,
        Ok(_) => return unexpected(),
        Err(resp) => return resp,
    };
    let rows: Vec<SignedInDevice> = devices.into_iter().map(SignedInDevice::from).collect();
    Json(rows).into_response()
}

/// Sign a device out of a profile
///
/// For a profile on a person's own Linux account: the device needs the account's password again
/// and its unlock key is forgotten. For any other: the device leaves its lock's list, and a
/// session it plays there ends. Admin lane only.
#[utoipa::path(
    delete,
    path = "/profiles/{id}/devices/{fingerprint}",
    tag = "profiles",
    operation_id = "signOutProfileDevice",
    params(
        ("id" = String, Path, description = "The profile id"),
        ("fingerprint" = String, Path, description = "The device's certificate SHA-256, lowercase hex"),
    ),
    responses(
        (status = NO_CONTENT, description = "Signed out"),
        (status = BAD_REQUEST, description = "Not a device fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn sign_out_profile_device(
    State(st): State<Arc<MgmtState>>,
    Path((id, fingerprint)): Path<(String, String)>,
) -> Response {
    if !pf_seats::unlock::valid_fingerprint(&fingerprint) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a device fingerprint.");
    }
    sign_out(&st, &id, Some(fingerprint)).await
}

/// Sign every device out of a profile
///
/// For a profile on a person's own Linux account: every device needs the account's password
/// again and every unlock key is forgotten. For any other: its lock's list empties, and every
/// session on it from a device off the list ends. Admin lane only.
#[utoipa::path(
    delete,
    path = "/profiles/{id}/devices",
    tag = "profiles",
    operation_id = "signOutProfileDevices",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = NO_CONTENT, description = "Signed out"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn sign_out_profile_devices(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    sign_out(&st, &id, None).await
}

/// Signs `fingerprint`, or every device, out of profile `id`: its person's own account, else its
/// lock's list.
async fn sign_out(st: &MgmtState, id: &str, fingerprint: Option<String>) -> Response {
    let seat = match person_seat(st, id).await {
        Ok(Some(seat)) => seat,
        Ok(None) => return super::locks::sign_out(st, id, fingerprint.as_deref()).await,
        Err(resp) => return resp,
    };
    match ask_door(
        st,
        Command::SignOut {
            id: seat,
            fingerprint,
        },
    )
    .await
    {
        Ok(CommandResult::SignedOut) => StatusCode::NO_CONTENT.into_response(),
        Ok(_) => unexpected(),
        Err(resp) => resp,
    }
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

    /// A seat on an existing account needs a desktop of its own and a password, and Linux takes
    /// only a name it can hand to the system: the supervisor is not asked about any other.
    #[test]
    fn an_existing_account_needs_a_seat_a_password_and_a_name_linux_can_have() {
        let input = |body: serde_json::Value| {
            let mut base = serde_json::json!({ "display_name": "Kid" });
            base.as_object_mut()
                .unwrap()
                .extend(body.as_object().unwrap().clone());
            serde_json::from_value::<ProfileCreate>(base).unwrap()
        };
        assert!(adopt_refusal(&input(serde_json::json!({}))).is_none());
        let no_seat = input(serde_json::json!({ "seat": false, "seat_account": "jdoe" }));
        assert_eq!(adopt_refusal(&no_seat).unwrap().0, StatusCode::BAD_REQUEST);
        let no_password = input(serde_json::json!({ "seat_account": "jdoe" }));
        let refusal = adopt_refusal(&no_password).unwrap();
        if cfg!(any(windows, target_os = "linux")) {
            assert_eq!(refusal.0, StatusCode::BAD_REQUEST);
            assert_eq!(refusal.1.code.as_deref(), Some("account_password_missing"));
            assert_eq!(refusal.1.error, "Enter the account's password.");
            let ready = input(serde_json::json!({ "seat_account": "jdoe", "seat_password": "pw" }));
            assert!(adopt_refusal(&ready).is_none());
        } else {
            assert_eq!(refusal.0, StatusCode::CONFLICT);
        }
        let sup =
            crate::native::unlock::fake::Fake::new(Vec::new(), |_| Ok(CommandResult::SignedOut));
        for bad in ["", "Jdoe", "a b", "../root", "-r", "j\ndoe", "corp\\jdoe"] {
            let refusal = adopt_user(&sup, "Kid", bad, Secret::new("pw".into())).unwrap_err();
            assert_eq!(refusal.0, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        assert!(sup.asked().is_empty(), "{:?}", sup.asked());
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

    /// The door starts a stopped seat, a Windows one on an existing account too, and never a
    /// person's own Linux account, whatever asks.
    #[test]
    fn the_door_never_starts_a_person_s_own_account() {
        let mut seat = snap(true, RuntimeState::Stopped, false).seats.remove(0);
        assert!(may_start(&seat));
        seat.adopted_sid = Some("S-1-5-21-1-2-3-1001".into());
        assert!(may_start(&seat));
        (seat.adopted_sid, seat.adopted_uid) = (None, Some(1500));
        assert!(!may_start(&seat));
        seat.runtime.state = RuntimeState::Running;
        assert!(!may_start(&seat));
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
                ApiError::plain("That isn't Ben's Windows password.")
            )
        );
        let locked_out = seat_error(SeatError::new(
            ErrorCode::Backend,
            "unlock_locked_out: Too many wrong passwords. Try again in 12 minutes.",
        ));
        assert_eq!(
            locked_out,
            (
                StatusCode::TOO_MANY_REQUESTS,
                ApiError::plain("Too many wrong passwords. Try again in 12 minutes.")
            )
        );
        let coded = seat_error(
            SeatError::new(ErrorCode::Backend, "account_taken: Another profile.")
                .said("account_taken"),
        );
        assert_eq!(coded.0, StatusCode::CONFLICT);
        assert_eq!(
            coded.1,
            ApiError::coded("account_taken", "Another profile.")
        );
        for (message, status) in [
            ("account_locked: Wait ten minutes.", StatusCode::BAD_REQUEST),
            (
                "unlock_locked_out: Try again in 12 minutes.",
                StatusCode::TOO_MANY_REQUESTS,
            ),
            ("account_missing: No such account.", StatusCode::BAD_REQUEST),
            ("account_is_owner: Use the owner.", StatusCode::CONFLICT),
            ("account_taken: Another profile.", StatusCode::CONFLICT),
            ("account_not_ordinary: A system user.", StatusCode::CONFLICT),
            (
                "pam_unavailable: No login service.",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
            ("account_signed_in: Sign Ben out.", StatusCode::CONFLICT),
            ("account_at_console: Sign Ben out.", StatusCode::CONFLICT),
            (
                "credential_wrong: That isn't jdoe's.",
                StatusCode::BAD_REQUEST,
            ),
            ("proof_invalid: Type the password.", StatusCode::BAD_REQUEST),
            ("unlock_required: Needs its password.", StatusCode::CONFLICT),
            (
                "account_at_desk: jdoe is at the desk.",
                StatusCode::CONFLICT,
            ),
            (
                "challenge_random: draw a challenge",
                StatusCode::SERVICE_UNAVAILABLE,
            ),
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
