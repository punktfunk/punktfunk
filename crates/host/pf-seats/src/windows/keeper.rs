//! What `punktfunk-seat-keeper.exe` takes from the supervisor.
//!
//! The keeper links IronRDP, whose pinned release-candidate crypto cannot share a lockfile
//! with the host, so it builds in its own workspace and runs as its own process. It reads the
//! bootstrap the supervisor wrote, asks WTS whether its session still exists, and stores the
//! RDP leaf pin; this module is that surface and nothing else.

use super::util::{backend_error, require_elevated_admin, WinResult};
use std::path::{Path, PathBuf};

pub use super::rdp::store_pin;
pub use crate::bootstrap::RdpBootstrap;

/// The keeper's file name, beside `punktfunk-host.exe`.
pub const KEEPER_EXE: &str = "punktfunk-seat-keeper.exe";

/// The keeper beside `host_path`.
pub fn keeper_path(host_path: &Path) -> PathBuf {
    host_path.with_file_name(KEEPER_EXE)
}

/// The id of a live or disconnected session signed in as `account`, if one exists.
pub fn session_of(account: &str) -> WinResult<Option<u32>> {
    Ok(super::wts::any_for_account(account)?.map(|session| session.id))
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
