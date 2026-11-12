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
use crate::secret::Secret;
use crate::unlock::DeviceList;
use std::collections::BTreeMap;

pub trait PlatformBackend: Send + Sync + 'static {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError>;
    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError>;

    /// [`Self::start`] of a seat its person proved the start for with `password`. Linux may open
    /// their session with it; nothing keeps it past the start.
    fn start_with_password(
        &self,
        seat: &Seat,
        _password: &Secret,
    ) -> Result<RuntimeStatus, BackendError> {
        self.start(seat)
    }

    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError>;
    fn remove(&self, seat: &Seat) -> Result<(), BackendError>;
    /// A pure read of `seat`'s state.
    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError>;

    /// Brings what runs beside `seat`'s unit level with `status`, a reading just taken. The
    /// service calls it only for a seat that no start, stop or delete is changing.
    fn reconcile(&self, _seat: &Seat, _status: &RuntimeStatus) {}

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

    /// Linux: the uid of the owner whose row this backend last prepared, by [`Self::adopt`] or at
    /// the supervisor's start. `None` before then, and on other platforms.
    fn owner_uid(&self) -> Option<u32> {
        None
    }

    /// Linux: the uid of `account`, an existing user a new row may adopt, or why it may not. The
    /// uid is the backend's own reading, never the caller's. `ledger` holds the rows that exist:
    /// the box owner's account and any account another row holds are refused.
    fn account_uid(&self, _account: &str, _ledger: &Ledger) -> Result<u32, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Linux: whether `password` opens `seat`'s account, a person's, before the row exists. The
    /// service calls it outside its lock and under the attempt limits; nothing keeps the password.
    fn check_password(&self, _seat: &Seat, _password: &Secret) -> Result<(), BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Make `seat`, a new adopted row, ready to run on its person's account with `password`. A
    /// refusal undoes its own steps. `Ok(true)`: adopt put the account in Remote Desktop Users.
    /// Windows verifies the password here and seals it; Linux has had [`Self::check_password`]
    /// pass and ignores it.
    fn adopt_account(&self, _seat: &Seat, _password: &Secret) -> Result<bool, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Verify `password`, an adopted row's new one, and store it in place of the old.
    fn reseal(&self, _seat: &Seat, _password: &Secret) -> Result<(), BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Linux, a person's row: its account still resolves to the uid the row recorded, so a device
    /// let on reaches the person it adopted. Elsewhere there is no such row.
    fn account_owned(&self, _seat: &Seat) -> Result<(), BackendError> {
        Ok(())
    }

    /// Linux, a person's row: its device list after `edit`, which gets the list with expired
    /// presence dropped and the time in unix seconds. A changed list is saved, and the seat's
    /// trust copy follows who it admits, before this returns.
    fn devices(
        &self,
        _seat: &Seat,
        _edit: &mut dyn FnMut(&mut DeviceList, i64),
    ) -> Result<DeviceList, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Linux: the box certificate's SHA-256, pinned the first time it is asked for. A
    /// certificate that no longer matches the pin is refused.
    fn box_fingerprint(&self) -> Result<[u8; 32], BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Whether seats are on, with the checks that gate turning them on.
    fn seating(&self) -> Result<SeatingStatus, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Set up what turning seats on needs and the platform can install itself, before the
    /// ledger lock. One row per thing done or refused, for [`Self::enable`] to lead with.
    fn prepare(&self) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// Run the checks, then turn seats on, after the rows [`Self::prepare`] gave. A refusal is
    /// `Ok` with an error-level check, so the operator reads why; `Err` is a failure the
    /// operator can't act on.
    fn enable(
        &self,
        _allow_rdp_from_network: bool,
        _prepared: Vec<Diagnostic>,
    ) -> Result<SeatingStatus, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Turn seats off after every seat has stopped.
    fn disable(&self, _keep_accounts: bool) -> Result<SeatingStatus, BackendError> {
        Err(UnsupportedBackend::unavailable())
    }

    /// Install the Flatpak app `id`, already checked, from Flathub for every account on the box,
    /// and answer its version.
    fn install_flatpak(&self, _id: &str) -> Result<Option<String>, BackendError> {
        Err(no_flatpaks())
    }

    /// Remove the box-wide Flatpak app `id`, already checked; `purge` deletes its data too.
    fn remove_flatpak(&self, _id: &str, _purge: bool) -> Result<(), BackendError> {
        Err(no_flatpaks())
    }
}

fn no_flatpaks() -> BackendError {
    BackendError::new("not_supported", "box-wide Flatpaks are Linux's")
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct BackendError {
    pub code: String,
    pub message: String,
    /// When `message` is a sentence for a person: the code of the host message
    /// `apierr_<said>` it is the English of, worded with `params`.
    pub said: Option<String>,
    pub params: BTreeMap<String, String>,
}

impl BackendError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            said: None,
            params: BTreeMap::new(),
        }
    }

    /// Marks `message` as the English of the host message `apierr_<code>`.
    pub fn said(mut self, code: &str) -> Self {
        self.said = Some(code.to_string());
        self
    }

    /// Fills the placeholder `name` of that message.
    pub fn param(mut self, name: &str, value: impl ToString) -> Self {
        self.params.insert(name.to_string(), value.to_string());
        self
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
