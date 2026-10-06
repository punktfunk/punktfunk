//! The box's seats as its own host sees them: the supervisor's ledger over its pipe, who plays
//! on each seat host, and [`placement`], which host a profile's connect goes to.
//!
//! Windows runs the supervisor in the service (`windows/service/seats.rs`); Linux runs it as the
//! root `punktfunk-seats` daemon behind a Unix socket, which only the door (`serve --door`) can
//! reach: a user's own host never lists seats. Elsewhere there is none, every call answers so,
//! and no connect is placed.

#[cfg(any(windows, target_os = "linux"))]
pub(crate) mod lifecycle;
pub(crate) mod placement;

use crate::profiles::OsAccount;
use pf_seats::ipc::{ApiError, Command, CommandResult, ErrorCode};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One request to the seat supervisor. Blocking: call it off the async workers.
pub(crate) fn call(command: Command) -> Result<CommandResult, ApiError> {
    #[cfg(windows)]
    {
        pf_seats::windows::pipe::request(pf_seats::ipc::PIPE_NAME, command)
    }
    #[cfg(target_os = "linux")]
    {
        pf_seats::linux::socket::request(
            std::path::Path::new(pf_seats::linux::SOCKET_PATH),
            command,
        )
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = command;
        Err(ApiError::new(
            ErrorCode::Backend,
            "seats run on a Windows Server or Linux host",
        ))
    }
}

/// Whether seats are on: on Windows the operator's marker, on Linux a supervisor that answers.
pub(crate) fn enabled() -> bool {
    #[cfg(windows)]
    {
        pf_seats::windows::seats_enabled()
    }
    #[cfg(target_os = "linux")]
    {
        matches!(call(Command::Seating), Ok(CommandResult::Seating { status }) if status.enabled)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

fn desktop_edition() -> bool {
    #[cfg(windows)]
    {
        pf_seats::windows::server_edition() == Some(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// The ledger: every seat, its ports and whether its host runs.
pub(crate) fn list() -> Result<Vec<pf_seats::Seat>, ApiError> {
    match call(Command::List)? {
        CommandResult::List { seats } => Ok(seats),
        other => Err(ApiError::new(
            ErrorCode::Transport,
            format!("seats list answered {other:?}"),
        )),
    }
}

/// The ledger and who plays on each running seat. A profile list reads every seat at once, and
/// reads within [`FRESH`] of the last one share it.
#[derive(Default)]
pub(crate) struct Snapshot {
    /// The operator turned seats on.
    pub on: bool,
    /// A desktop edition of Windows, which serves one session and so no seat.
    pub desktop_edition: bool,
    pub seats: Vec<pf_seats::Seat>,
    /// Seat id → who streams there, for each running seat whose host answered.
    pub occupants: BTreeMap<String, Vec<Occupant>>,
}

impl Snapshot {
    /// A seat's row and its number, counted from 1 in ledger order.
    pub(crate) fn seat(&self, id: &str) -> Option<(&pf_seats::Seat, u8)> {
        let at = self.seats.iter().position(|s| s.id.as_str() == id)?;
        Some((&self.seats[at], at as u8 + 1))
    }

    /// The box owner's row, on a door.
    pub(crate) fn owner(&self) -> Option<&pf_seats::Seat> {
        self.seats.iter().find(|s| s.owner)
    }

    /// The ledger row a profile plays on. A full seat names its own. On a door the owner, a
    /// profile that shares the owner's desktop and a light seat all play in the owner's host, so
    /// on the owner's row. Anywhere else the rest play on the host that was asked.
    pub(crate) fn row_of<'a>(&'a self, account: &'a OsAccount) -> Option<&'a str> {
        row_in(account, self.owner(), is_door())
    }
}

/// [`Snapshot::row_of`] against a ledger's owner row and whether this is a door.
pub(crate) fn row_in<'a>(
    account: &'a OsAccount,
    owner: Option<&'a pf_seats::Seat>,
    door: bool,
) -> Option<&'a str> {
    match account {
        OsAccount::Seat { seat: Some(id), .. } => Some(id),
        OsAccount::Operator | OsAccount::Seat { seat: None, .. } if door => {
            owner.map(|s| s.id.as_str())
        }
        _ => None,
    }
}

/// Whether this host is the door.
pub(crate) fn is_door() -> bool {
    pf_paths::seat::is_door()
}

/// How long a snapshot answers for the box: a picker polls every 2 s.
const FRESH: Duration = Duration::from_secs(2);

static LAST: Mutex<Option<(Instant, Arc<Snapshot>)>> = Mutex::new(None);

/// The seats as they are now, at most [`FRESH`] old. Blocking: call it off the async workers.
pub(crate) fn snapshot() -> Arc<Snapshot> {
    if !(cfg!(windows) || is_door()) {
        return Arc::default();
    }
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, snap)) = last.as_ref() {
        if at.elapsed() < FRESH {
            return Arc::clone(snap);
        }
    }
    let seats = list().unwrap_or_else(|e| {
        tracing::debug!(code = ?e.code, "seats ledger did not load: {}", e.message);
        Vec::new()
    });
    let occupants = seats
        .iter()
        .filter(|s| s.runtime.state == pf_seats::RuntimeState::Running)
        .filter_map(|s| Some((s.id.as_str().to_string(), occupants(s)?)))
        .collect();
    let snap = Arc::new(Snapshot {
        on: enabled(),
        desktop_edition: desktop_edition(),
        seats,
        occupants,
    });
    *last = Some((Instant::now(), Arc::clone(&snap)));
    snap
}

/// The next [`snapshot`] reads afresh: a seat was just started or stopped.
pub(crate) fn invalidate() {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// A seat host's own config dir, where it keeps its management token.
fn host_dir(seat: &pf_seats::Seat) -> PathBuf {
    pf_seats::persistence::default_root()
        .join("hosts")
        .join(seat.id.as_str())
}

/// The seat host's certificate pin from the ledger, `None` until the supervisor learned it.
fn seat_pin(seat: &pf_seats::Seat) -> Option<[u8; 32]> {
    punktfunk_core::fp::parse_hex32(seat.fingerprint.as_deref()?)
}

/// A device streaming on a seat host, as its `/status` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occupant {
    /// Fingerprint prefix, or the peer address of an anonymous client.
    pub client: String,
    pub name: Option<String>,
}

/// Loopback is local: a seat that does not answer in this long is not serving.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// Who streams on `seat` right now, from its host's loopback `/status`. `None` when the seat
/// host does not answer.
pub(crate) fn occupants(seat: &pf_seats::Seat) -> Option<Vec<Occupant>> {
    let client = crate::ctl::client::Client::seat(
        seat_pin(seat)?,
        &host_dir(seat),
        seat.mgmt_port,
        Some(STATUS_TIMEOUT),
    )
    .ok()?;
    let status = client.get("/api/v1/status").ok()?;
    let rows = status.get("sessions")?.as_array()?;
    Some(
        rows.iter()
            .filter_map(|row| {
                Some(Occupant {
                    client: row.get("client")?.as_str()?.to_string(),
                    name: row
                        .get("client_name")
                        .and_then(|n| n.as_str())
                        .map(str::to_string),
                })
            })
            .collect(),
    )
}

/// Ends every session on `seat` through its host's loopback API. `false` when it didn't answer.
pub(crate) fn end_sessions(seat: &pf_seats::Seat) -> bool {
    let Some(pin) = seat_pin(seat) else {
        return false;
    };
    crate::ctl::client::Client::seat(pin, &host_dir(seat), seat.mgmt_port, Some(STATUS_TIMEOUT))
        .and_then(|client| client.delete("/api/v1/session"))
        .is_ok()
}

/// A seat host's API, for the console's proxy: `path_and_query` under `/api/v1/`. `None` when
/// the seat host doesn't answer.
pub(crate) fn forward(
    seat: &pf_seats::Seat,
    method: &str,
    path_and_query: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Option<(u16, Option<String>, Vec<u8>)> {
    let client = crate::ctl::client::Client::seat(
        seat_pin(seat)?,
        &host_dir(seat),
        seat.mgmt_port,
        Some(PROXY_TIMEOUT),
    )
    .ok()?;
    client
        .raw(
            method,
            &format!("/api/v1/{path_and_query}"),
            content_type,
            body,
        )
        .ok()
}

/// A proxied library page with art can take a while on a cold seat.
const PROXY_TIMEOUT: Duration = Duration::from_secs(30);
