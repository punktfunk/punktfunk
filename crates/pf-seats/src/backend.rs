//! Platform boundary for account, session, and process ownership.
//!
//! The portable core allocates and persists seat identity and resources, then
//! asks this trait to provision, start, stop, remove, and inspect them. Methods
//! receive complete validated seat records; platform secrets never enter the
//! ledger or control responses. Windows supplies the production implementation.
//! Tests use an in-memory fake, while other targets return a stable unsupported
//! diagnostic without pretending that a seat is running.

use crate::ipc::{Diagnostic, SeatingStatus};
use crate::model::{Ledger, RuntimeStatus, Seat};

pub trait PlatformBackend: Send + Sync + 'static {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError>;
    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError>;
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError>;
    fn remove(&self, seat: &Seat) -> Result<(), BackendError>;
    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError>;

    fn doctor(&self, _ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        Ok(Vec::new())
    }

    /// The config dir `seat`'s host runs with, where it keeps its own identity.
    fn host_config_dir(&self, _seat: &Seat) -> Option<std::path::PathBuf> {
        None
    }

    /// Check that `seat`, the owner's new row, names an ordinary account that already exists,
    /// and prepare what its host reads. Nothing is created: the account is the owner's own.
    fn adopt(&self, _seat: &Seat) -> Result<(), BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Whether seats are on, with the checks that gate turning them on.
    fn seating(&self) -> Result<SeatingStatus, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Run the checks, then turn seats on. A refusal is `Ok` with an error-level check, so the
    /// operator reads why; `Err` is a failure the operator can't act on.
    fn enable(&self, _allow_rdp_from_network: bool) -> Result<SeatingStatus, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Turn seats off after every seat has stopped.
    fn disable(&self, _keep_accounts: bool) -> Result<SeatingStatus, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct BackendError {
    pub code: String,
    pub message: String,
}

impl BackendError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedBackend;

impl UnsupportedBackend {
    fn unavailable() -> BackendError {
        BackendError::new(
            "platform_unavailable",
            "seat account, RDP, WTS, and process supervision require Windows.",
        )
    }
}

impl PlatformBackend for UnsupportedBackend {
    fn provision(&self, _seat: &Seat) -> Result<(), BackendError> {
        Err(Self::unavailable())
    }

    fn start(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Err(Self::unavailable())
    }

    fn stop(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Err(Self::unavailable())
    }

    fn remove(&self, _seat: &Seat) -> Result<(), BackendError> {
        Err(Self::unavailable())
    }

    fn status(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Ok(RuntimeStatus::failed(Self::unavailable().to_string()))
    }

    fn doctor(&self, _ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        Ok(vec![Diagnostic::error(
            "platform_unavailable",
            Self::unavailable().message,
        )])
    }
}
