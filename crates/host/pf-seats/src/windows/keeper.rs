//! What `punktfunk-seat-keeper.exe` takes from the supervisor.
//!
//! The keeper links IronRDP, whose pinned release-candidate crypto cannot share a lockfile
//! with the host, so it builds in its own workspace and runs as its own process. It reads the
//! bootstrap the supervisor wrote, asks WTS whether its session still exists, reads the seat
//! host's disconnect mark in the host root the bootstrap names, and stores the RDP leaf pin;
//! this module is that surface and nothing else.

use super::util::{backend_error, require_elevated_admin, WinResult};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

pub use super::rdp::store_pin;
pub use crate::bootstrap::RdpBootstrap;

/// The keeper's file name, beside `punktfunk-host.exe`.
pub const KEEPER_EXE: &str = "punktfunk-seat-keeper.exe";

/// The seat listener's loopback port, where a session also gets its pads' audio endpoints.
pub const SEAT_LISTENER_PORT: u16 = pf_driver_proto::seat_audio::LISTENER_PORT;

/// The keeper's own handshake gives up after 15 s.
pub(super) const OBSERVE_LIMIT: Duration = Duration::from_secs(20);

/// The keeper beside `host_path`.
pub fn keeper_path(host_path: &Path) -> PathBuf {
    host_path.with_file_name(KEEPER_EXE)
}

/// The leaf TermService presents now, as the keeper beside `host_path` reads it. `None` when it
/// can't say: TermService is down or the keeper didn't run.
pub fn live_pin(host_path: &Path) -> Option<[u8; 32]> {
    let output = pf_paths::run_within(
        Command::new(keeper_path(host_path))
            .arg("observe")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        OBSERVE_LIMIT,
    )
    .ok()?;
    if !output.status.success() {
        return None;
    }
    super::rdp::parse_pin(&String::from_utf8_lossy(&output.stdout))
}

/// The id of a live or disconnected session signed in as `account`, if one exists.
pub fn session_of(account: &str) -> WinResult<Option<u32>> {
    Ok(super::wts::any_for_account(account)?.map(|session| session.id))
}

/// Whether the seat host dropped the connection that began at `since` itself: the mark in
/// `host_root` ([`pf_paths::seat::SEAT_DISCONNECT_MARK`]) passes
/// [`pf_paths::seat::mark_excuses_drop`]. The read deletes the mark, so one mark excuses one drop.
pub fn host_disconnected(host_root: &Path, since: SystemTime) -> bool {
    let path = host_root.join(pf_paths::seat::SEAT_DISCONNECT_MARK);
    let Ok(mark) = std::fs::read_to_string(&path) else {
        return false;
    };
    let _ = std::fs::remove_file(&path);
    pf_paths::seat::mark_excuses_drop(&mark, since, SystemTime::now())
}

/// The machine name the keeper's NLA login uses as its domain.
pub fn computer_name() -> WinResult<String> {
    super::util::computer_name()
}

/// Trust needs an elevated operator and a listening TermService.
pub fn require_trust_prerequisites() -> WinResult<()> {
    require_elevated_admin()?;
    if super::termservice_running()? {
        Ok(())
    } else {
        Err(backend_error(
            "termservice_stopped",
            "TermService must be running and listening before RDP trust",
        ))
    }
}
