//! Seats on the box as a whole: turning them on or off, the supervisor's own checks, and the
//! switch to Reachable without logging in. Mounted under `/profiles`, admin lane only.

use super::profiles::{refused, unexpected};
use super::shared::*;
use pf_seats::ipc::{Command, CommandResult, Diagnostic, DiagnosticLevel, ErrorCode};
use std::collections::BTreeMap;

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
    /// The host message `message` is the English of, worded with `params`.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    /// The values of that message's placeholders, by name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[schema(required = false)]
    params: BTreeMap<String, String>,
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
            id: d.id,
            params: d.params,
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

fn seats_need_windows() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "seats_need_windows_server",
        "Seats need Windows Server.",
    )
}

/// One request to the seat supervisor on a blocking thread, or the response that says why not.
async fn supervisor(command: Command) -> Result<CommandResult, Response> {
    match blocking("seat_supervisor", move || crate::seats::call(command)).await? {
        Ok(result) => Ok(result),
        Err(e) => {
            tracing::warn!(code = ?e.code, error = %e.message, "seat supervisor request failed");
            Err(if e.code == ErrorCode::Transport {
                api_error_coded(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "seat_supervisor_unreachable_restart",
                    "Couldn't reach the seat supervisor. Restart the Punktfunk service, then try again.",
                )
            } else {
                api_error_coded(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "seats_not_changed",
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
        Ok(_) => unexpected(),
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
        if pf_paths::seat::role().is_door() {
            return api_error_coded(
                StatusCode::CONFLICT,
                "seats_stay_on_with_door",
                "Seats stay on while Reachable without logging in is on.",
            );
        }
        return seats_need_windows();
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
        return seats_need_windows();
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
        Ok(_) => unexpected(),
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
