//! Serialized command dispatch over a validated ledger and platform backend.
//!
//! Each mutation is applied to a clone, persisted with a new generation, then
//! published in memory. Provisioning precedes the create commit; a create that
//! fails in either removes what it made through the backend's marker-checked
//! remove, and only an explicit Delete removes a committed seat's account.
//! Runtime observations, including backend failures, are persisted. Startup
//! reconciliation starts autostart seats and refreshes the rest. One mutex
//! serializes lifecycle calls because four seats do not need rollback races; the
//! ledger's own lock is never held across a backend call that takes time (a
//! create, start, stop, delete or Enable), only for the edit and its write, so
//! `List` answers while a seat takes its tens of seconds to come up.

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{
    ApiError, Command, CommandResult, Diagnostic, DiagnosticLevel, DoctorReport, ErrorCode,
    SeatingStatus,
};
use crate::model::{CreateSeat, Ledger, RuntimeState, RuntimeStatus, Seat, SeatId};
use crate::persistence::{LedgerStore, StoreError};
use crate::secret::Secret;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub struct SeatService<B> {
    backend: B,
    store: LedgerStore,
    ledger: Mutex<Ledger>,
    /// Held across every create, start, stop, delete, adopt, credential, Enable and Disable. Only
    /// a call under it adds or drops a row.
    ops: Mutex<()>,
    /// The start, stop or delete in flight; `List` reports its seat's published state meanwhile.
    changing: Mutex<Option<Change>>,
    /// Held by the one box-wide Flatpak change allowed at a time.
    flatpak: Mutex<()>,
}

/// A change to one seat's unit or account, and whether a stop has reached it as a start.
struct Change {
    id: SeatId,
    start: bool,
    cancelled: bool,
}

impl<B: PlatformBackend> SeatService<B> {
    pub fn open(root: impl AsRef<Path>, backend: B) -> Result<Self, StoreError> {
        Self::with_store(LedgerStore::open(root)?, backend)
    }

    /// A service over `store`, already open: the caller may reshape its root's mode before the
    /// ledger loads.
    pub fn with_store(store: LedgerStore, backend: B) -> Result<Self, StoreError> {
        let ledger = store.load()?;
        Ok(Self {
            backend,
            store,
            ledger: Mutex::new(ledger),
            ops: Mutex::new(()),
            changing: Mutex::new(None),
            flatpak: Mutex::new(()),
        })
    }

    pub fn store(&self) -> &LedgerStore {
        &self.store
    }

    pub fn ledger(&self) -> Ledger {
        self.lock().clone()
    }

    pub fn dispatch(&self, command: Command) -> Result<CommandResult, ApiError> {
        match command {
            Command::List => self.list(),
            Command::Create(request) => self.create(request),
            Command::Start { id } => self.start(&id),
            Command::Stop { id } => self.stop(&id),
            Command::Delete { id } => self.delete(&id),
            Command::Doctor => Ok(self.doctor()),
            Command::Seating => self.seating(|backend| backend.seating()),
            Command::Enable {
                allow_rdp_from_network,
                prepare,
            } => {
                // Installing a Windows role takes minutes; the ledger stays readable meanwhile.
                let prepared = if prepare {
                    self.backend.prepare()
                } else {
                    Vec::new()
                };
                let _op = self.op();
                self.seating(|backend| backend.enable(allow_rdp_from_network, prepared))
            }
            Command::Disable { keep_accounts } => self.disable(keep_accounts),
            Command::AdoptOwner { account } => self.adopt_owner(&account),
            Command::Adopt {
                seat,
                sid,
                password,
            } => self.adopt(seat, sid, &password),
            Command::Credential { id, password } => self.credential(&id, &password),
            Command::InstallFlatpak { id } => self.flatpak(&id, |b| b.install_flatpak(&id)),
            Command::RemoveFlatpak { id, purge } => {
                self.flatpak(&id, |b| b.remove_flatpak(&id, purge).map(|()| None))
            }
        }
    }

    /// A box-wide Flatpak change for `id`, checked before `run` sees it. It holds neither the
    /// ledger nor `ops`: an install takes minutes. A second change meanwhile answers Conflict, so
    /// installs never fill the socket's connections.
    fn flatpak(
        &self,
        id: &str,
        run: impl FnOnce(&B) -> Result<Option<String>, BackendError>,
    ) -> Result<CommandResult, ApiError> {
        if !crate::ipc::valid_flatpak_id(id) {
            return Err(ApiError::new(
                ErrorCode::InvalidRequest,
                "not a Flatpak app id",
            ));
        }
        let _one = match self.flatpak.try_lock() {
            Ok(one) => one,
            Err(std::sync::TryLockError::Poisoned(one)) => one.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    "another Flatpak change is running",
                ));
            }
        };
        let version = run(&self.backend)?;
        Ok(CommandResult::Flatpak {
            id: id.to_owned(),
            version,
        })
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Records the pin of `seat`'s host while it runs. A host that hasn't written its identity
    /// yet leaves the last pin known.
    fn learn_pin(&self, seat: &mut Seat) {
        if seat.runtime.state != RuntimeState::Running {
            return;
        }
        let dir = self.backend.host_config_dir(seat);
        if let Some(pin) = dir.and_then(|dir| host_fingerprint(&dir)) {
            seat.fingerprint = Some(pin);
        }
    }

    /// Refresh every seat's state, then start the autostart seats one at a time. A start runs
    /// outside the ledger lock, as a `start` command does, so `list` answers meanwhile; a failed
    /// start is recorded on its row, and the first one is returned once all have run.
    pub fn reconcile_startup(&self) -> Result<(), ApiError> {
        let autostart: Vec<SeatId> = {
            let mut current = self.lock();
            let mut next = current.clone();
            for seat in next.seats.iter_mut().filter(|seat| !seat.autostart) {
                seat.runtime = self.observe(seat);
                self.learn_pin(seat);
            }
            let ids = next
                .seats
                .iter()
                .filter(|seat| seat.autostart)
                .map(|seat| seat.id.clone())
                .collect();
            if next != *current {
                self.commit(&mut current, next)?;
            }
            ids
        };
        let mut first = None;
        for id in autostart {
            if let Err(error) = self.change_runtime(&id, true) {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// `seat`'s state as its backend reads it, after the backend has brought what runs beside
    /// the unit level with it.
    fn observe(&self, seat: &Seat) -> RuntimeStatus {
        match self.backend.status(seat) {
            Ok(status) => {
                self.backend.reconcile(seat, &status);
                status
            }
            Err(error) => RuntimeStatus::failed(error.to_string()),
        }
    }

    fn list(&self) -> Result<CommandResult, ApiError> {
        let mut current = self.lock();
        // Read under the ledger lock: a change marks its seat under it, before its backend runs.
        let changing = self
            .changing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|change| change.id.clone());
        let mut next = current.clone();
        for seat in &mut next.seats {
            if changing.as_ref() == Some(&seat.id) {
                // A start in flight shows the step its backend is on.
                if let Ok(status) = self.backend.status(seat)
                    && status.state == RuntimeState::Starting
                    && status.detail.is_some()
                {
                    seat.runtime = status;
                }
                continue;
            }
            seat.runtime = self.observe(seat);
            self.learn_pin(seat);
        }
        if next != *current {
            self.commit(&mut current, next)?;
        }
        Ok(CommandResult::List {
            seats: current.seats.clone(),
        })
    }

    fn create(&self, request: CreateSeat) -> Result<CommandResult, ApiError> {
        let _op = self.op();
        // Allocated on a copy: provisioning takes minutes, and the ledger answers meanwhile.
        let seat = self
            .lock()
            .clone()
            .allocate(request)
            .map_err(ApiError::from)?;
        if let Err(error) = self.backend.provision(&seat) {
            // A name the backend refused, or an account that is not the seat's, made nothing of
            // ours. Any other failure may have made the account, and no ledger row will name it.
            if !matches!(error.code.as_str(), "account_invalid" | "account_not_owned") {
                let _ = self.backend.remove(&seat);
            }
            return Err(ApiError::from(error));
        }
        self.land(seat)
    }

    /// Puts `seat`, whose account the backend has readied, in the ledger. A row that no longer
    /// fits is taken back from the backend: no delete could reach what no row names.
    fn land(&self, seat: Seat) -> Result<CommandResult, ApiError> {
        if let Err(error) = self.amend(|next| next.insert(seat.clone()).map_err(ApiError::from)) {
            let _ = self.backend.remove(&seat);
            return Err(error);
        }
        Ok(CommandResult::Created { seat })
    }

    /// The owner's row: the one that exists when it names `account`, else a new one the backend
    /// has checked. The box has one owner, whom the backend names: a row for another account is
    /// stale, from an owner since changed, and the new one replaces it. An account the backend
    /// doesn't accept leaves that row and answers Conflict.
    fn adopt_owner(&self, account: &str) -> Result<CommandResult, ApiError> {
        let _op = self.op();
        let (stale, seat) = {
            let current = self.lock();
            let stale = current.owner().cloned();
            if let Some(owner) = stale.as_ref().filter(|owner| owner.account == account) {
                return Ok(CommandResult::Created {
                    seat: owner.clone(),
                });
            }
            let mut next = current.clone();
            if let Some(owner) = &stale {
                next.remove(&owner.id);
            }
            let seat = next.allocate_owner(account).map_err(ApiError::from)?;
            (stale, seat)
        };
        if let Err(error) = self.backend.adopt(&seat) {
            return Err(match &stale {
                Some(owner) => ApiError::new(
                    ErrorCode::Conflict,
                    format!("the box owner is already {}: {error}", owner.account),
                ),
                None => ApiError::from(error),
            });
        }
        // Released after the new row is adopted, so what they share stays with the new owner.
        if let Some(owner) = &stale
            && let Err(error) = self.backend.remove(owner)
        {
            let _ = self.backend.remove(&seat);
            return Err(ApiError::from(error));
        }
        let replaced = self.amend(|next| {
            if let Some(owner) = &stale {
                next.remove(&owner.id);
            }
            next.insert(seat.clone()).map_err(ApiError::from)
        });
        if let Err(error) = replaced {
            // Releasing the row removes what adopting prepared and never the account.
            let _ = self.backend.remove(&seat);
            return Err(error);
        }
        Ok(CommandResult::Created { seat })
    }

    /// A seat on a person's own account, whose SID is `sid`. Allocated on a copy, as `create`
    /// is; a backend that refuses has undone its own steps.
    fn adopt(
        &self,
        request: CreateSeat,
        sid: String,
        password: &Secret,
    ) -> Result<CommandResult, ApiError> {
        let _op = self.op();
        let mut seat = self
            .lock()
            .clone()
            .allocate_adopted(request, sid)
            .map_err(ApiError::from)?;
        seat.rdp_granted = self
            .backend
            .adopt_account(&seat, password)
            .map_err(ApiError::from)?;
        self.land(seat)
    }

    /// Stores `password` for `id`, an adopted row, once the backend has verified it. A made
    /// row's password is the backend's own and is refused before the backend sees it.
    fn credential(&self, id: &SeatId, password: &Secret) -> Result<CommandResult, ApiError> {
        let _op = self.op();
        let seat = self.lock().seat(id).cloned().ok_or_else(|| not_found(id))?;
        if seat.adopted_sid.is_none() {
            return Err(ApiError::new(
                ErrorCode::InvalidRequest,
                "only an adopted account's password can be updated",
            ));
        }
        self.backend
            .reseal(&seat, password)
            .map_err(ApiError::from)?;
        Ok(CommandResult::Resealed { seat })
    }

    fn start(&self, id: &SeatId) -> Result<CommandResult, ApiError> {
        self.change_runtime(id, true)
    }

    fn stop(&self, id: &SeatId) -> Result<CommandResult, ApiError> {
        self.change_runtime(id, false)
    }

    fn change_runtime(&self, id: &SeatId, start: bool) -> Result<CommandResult, ApiError> {
        if !start {
            self.interrupt_start(id);
        }
        let _op = self.op();
        let original = self.begin(id, start)?;
        if start {
            let published = self.publish_runtime(
                id,
                RuntimeStatus {
                    state: RuntimeState::Starting,
                    detail: None,
                },
            );
            if let Err(error) = published {
                self.end();
                return Err(error);
            }
        }
        let result = if !start {
            self.backend.stop(&original)
        } else if self.start_cancelled() {
            Err(BackendError::new(
                "seat_stopped",
                "the seat was stopped before it began to start",
            ))
        } else {
            self.backend.start(&original)
        };
        let cancelled = self.end();
        let mut current = self.lock();
        let mut next = current.clone();
        let seat = next
            .seats
            .iter_mut()
            .find(|seat| &seat.id == id)
            .expect("cloned ledger retains the selected seat");
        match result {
            Ok(status) => {
                seat.runtime = status;
                self.learn_pin(seat);
            }
            Err(_) if cancelled => {
                // The stop that reached this start records the seat's state next.
                return Err(ApiError::new(
                    ErrorCode::Conflict,
                    "the seat was stopped while it started",
                ));
            }
            Err(error) => {
                // The backend's own account of the failure, when it has one, is the sentence
                // the console shows; the code and the operator's message go to the caller.
                // What the failed change left running beside the unit is taken back now.
                let observed = self.backend.status(&original);
                if let Ok(status) = &observed {
                    self.backend.reconcile(&original, status);
                }
                seat.runtime = match observed {
                    Ok(status)
                        if status.state == RuntimeState::Failed && status.detail.is_some() =>
                    {
                        status
                    }
                    _ => RuntimeStatus::failed(error.to_string()),
                };
                self.commit(&mut current, next)?;
                return Err(ApiError::from(error));
            }
        }
        let changed = seat.clone();
        self.commit(&mut current, next)?;
        if start {
            Ok(CommandResult::Started { seat: changed })
        } else {
            Ok(CommandResult::Stopped { seat: changed })
        }
    }

    fn delete(&self, id: &SeatId) -> Result<CommandResult, ApiError> {
        let _op = self.op();
        let seat = self.begin(id, false)?;
        let removed = self.backend.remove(&seat);
        self.end();
        removed.map_err(ApiError::from)?;
        self.amend(|next| next.remove(id).map(drop).ok_or_else(|| not_found(id)))?;
        Ok(CommandResult::Deleted { id: id.clone() })
    }

    /// Marks `id` as the seat a change is on and returns its row. The mark is made under the
    /// ledger lock a `List` holds while it reads each seat, so a `List` either finishes with the
    /// seat first or skips it.
    fn begin(&self, id: &SeatId, start: bool) -> Result<Seat, ApiError> {
        let current = self.lock();
        let seat = current.seat(id).cloned().ok_or_else(|| not_found(id))?;
        *self.changing.lock().unwrap_or_else(|e| e.into_inner()) = Some(Change {
            id: id.clone(),
            start,
            cancelled: false,
        });
        Ok(seat)
    }

    /// Ends the change in flight. `true` when a stop reached it as a start.
    fn end(&self) -> bool {
        let change = self
            .changing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        change.is_some_and(|change| change.cancelled)
    }

    fn start_cancelled(&self) -> bool {
        let changing = self.changing.lock().unwrap_or_else(|e| e.into_inner());
        changing.as_ref().is_some_and(|change| change.cancelled)
    }

    /// A stop reaches the seat whose start is in flight at once, instead of queueing behind the
    /// start for as long as it runs. The backend's stop ends what the start waits on, so the start
    /// returns; the stop then runs in turn, for a start that hadn't reached its backend yet.
    fn interrupt_start(&self, id: &SeatId) {
        {
            let mut changing = self.changing.lock().unwrap_or_else(|e| e.into_inner());
            match changing.as_mut() {
                Some(change) if change.start && &change.id == id => change.cancelled = true,
                _ => return,
            }
        }
        let seat = self.lock().seat(id).cloned();
        if let Some(seat) = seat {
            let _ = self.backend.stop(&seat);
        }
    }

    /// `edit` applied to a copy of the ledger as it is now, then written. The ledger lock is held
    /// for that and no longer; the seats can't change meanwhile, since the caller holds `ops`.
    fn amend(
        &self,
        edit: impl FnOnce(&mut Ledger) -> Result<(), ApiError>,
    ) -> Result<(), ApiError> {
        let mut current = self.lock();
        let mut next = current.clone();
        edit(&mut next)?;
        self.commit(&mut current, next)
    }

    fn seating(
        &self,
        run: impl FnOnce(&B) -> Result<SeatingStatus, BackendError>,
    ) -> Result<CommandResult, ApiError> {
        run(&self.backend)
            .map(|status| CommandResult::Seating { status })
            .map_err(ApiError::from)
    }

    /// Stops every seat before the backend turns seats off, so none runs without its display
    /// driver. Only Windows turns seats off; elsewhere the backend's refusal answers before any
    /// seat stops.
    fn disable(&self, keep_accounts: bool) -> Result<CommandResult, ApiError> {
        if !keep_accounts {
            return Err(ApiError::new(
                ErrorCode::InvalidRequest,
                "seat accounts are removed with their profile, not when seats go off",
            ));
        }
        if !cfg!(windows) {
            return self.seating(|backend| backend.disable(keep_accounts));
        }
        let ids: Vec<SeatId> = self.lock().seats.iter().map(|s| s.id.clone()).collect();
        for id in &ids {
            self.change_runtime(id, false)?;
        }
        let _op = self.op();
        self.seating(|backend| backend.disable(keep_accounts))
    }

    fn doctor(&self) -> CommandResult {
        // A copy: the backend's checks take seconds, and the ledger stays readable meanwhile.
        let current = self.lock().clone();
        let mut diagnostics = vec![Diagnostic::info(
            "ledger",
            format!("Seats set up: {} of 4.", current.seats.len()),
        )];
        match self.backend.doctor(&current) {
            Ok(mut backend) => diagnostics.append(&mut backend),
            Err(error) => diagnostics.push(Diagnostic::error("backend", error.to_string())),
        }
        for seat in &current.seats {
            if seat.runtime.state == RuntimeState::Failed {
                diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    code: "runtime_failed".into(),
                    message: seat
                        .runtime
                        .detail
                        .clone()
                        .unwrap_or_else(|| "seat runtime failed".into()),
                    seat_id: Some(seat.id.clone()),
                });
            }
        }
        let healthy = diagnostics
            .iter()
            .all(|diagnostic| diagnostic.level != DiagnosticLevel::Error);
        CommandResult::Doctor {
            report: DoctorReport {
                healthy,
                diagnostics,
            },
        }
    }

    fn commit(&self, current: &mut Ledger, mut next: Ledger) -> Result<(), ApiError> {
        next.generation = current.generation.checked_add(1).ok_or_else(|| {
            ApiError::new(ErrorCode::Persistence, "ledger generation is exhausted")
        })?;
        self.store.save(&next).map_err(ApiError::from)?;
        *current = next;
        Ok(())
    }

    /// Persists `status` as `id`'s runtime so `List` shows it before a long backend call.
    fn publish_runtime(&self, id: &SeatId, status: RuntimeStatus) -> Result<(), ApiError> {
        let mut current = self.lock();
        let mut next = current.clone();
        if let Some(seat) = next.seats.iter_mut().find(|seat| &seat.id == id) {
            seat.runtime = status;
        }
        self.commit(&mut current, next)
    }

    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn op(&self) -> MutexGuard<'_, ()> {
        self.ops.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl From<crate::model::ValidationError> for ApiError {
    fn from(error: crate::model::ValidationError) -> Self {
        use crate::model::ValidationError::{Duplicate, NoAllocation, TooManySeats};
        let code = match &error {
            TooManySeats(_) | NoAllocation(_) => ErrorCode::Capacity,
            Duplicate { .. } => ErrorCode::Conflict,
            _ => ErrorCode::InvalidRequest,
        };
        Self::new(code, error.to_string())
    }
}

impl From<BackendError> for ApiError {
    fn from(error: BackendError) -> Self {
        Self::new(ErrorCode::Backend, error.to_string())
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self::new(ErrorCode::Persistence, error.to_string())
    }
}

fn not_found(id: &SeatId) -> ApiError {
    ApiError::new(ErrorCode::NotFound, format!("seat {id} does not exist"))
}

/// SHA-256 of the certificate DER in `dir`'s `native-cert.pem`, lowercase hex: the pin a client
/// dials that host with.
fn host_fingerprint(dir: &Path) -> Option<String> {
    use rustls::pki_types::{pem::PemObject as _, CertificateDer};
    let der = CertificateDer::from_pem_file(dir.join("native-cert.pem")).ok()?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der.as_ref());
    Some(hex::encode(digest.as_ref()))
}
