//! The box's seats as its own host sees them: the supervisor's ledger over its pipe, who plays
//! on each seat host, and [`placement`], which host a profile's connect goes to.
//!
//! Windows runs the supervisor in the service (`windows/service/seats.rs`); elsewhere there is
//! none yet, every call answers so, and no connect is placed.

pub(crate) mod placement;

use pf_seats::ipc::{ApiError, Command, CommandResult, ErrorCode};
use std::path::PathBuf;
use std::time::Duration;

/// One request to the seat supervisor. Blocking: call it off the async workers.
pub(crate) fn call(command: Command) -> Result<CommandResult, ApiError> {
    #[cfg(windows)]
    {
        pf_seats::windows::pipe::request(pf_seats::ipc::PIPE_NAME, command)
    }
    #[cfg(not(windows))]
    {
        let _ = command;
        Err(ApiError::new(
            ErrorCode::Backend,
            "seats run on a Windows Server host",
        ))
    }
}

/// Whether the operator turned seats on.
pub(crate) fn enabled() -> bool {
    #[cfg(windows)]
    {
        pf_seats::windows::seats_enabled()
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

/// A seat host's own config dir, where it keeps its management token.
fn host_dir(seat: &pf_seats::Seat) -> PathBuf {
    pf_seats::persistence::default_root()
        .join("hosts")
        .join(seat.id.as_str())
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
        &pf_paths::config_dir(),
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
