//! Serialized command dispatch over a validated ledger and platform backend.
//!
//! Each mutation is applied to a clone, persisted with a new generation, then
//! published in memory. Provisioning precedes the create commit, but a failed
//! commit never deletes an account: only an explicit Delete command may do so.
//! Runtime observations, including backend failures, are persisted. Startup
//! reconciliation starts autostart seats and refreshes the rest. One mutex
//! serializes backend calls because four seats do not need rollback races.

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{
    ApiError, Command, CommandResult, Diagnostic, DiagnosticLevel, DoctorReport, ErrorCode,
};
use crate::model::{CreateSeat, Ledger, RuntimeState, RuntimeStatus, SeatId};
use crate::persistence::{LedgerStore, StoreError};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

pub struct SeatService<B> {
    backend: B,
    store: LedgerStore,
    ledger: Mutex<Ledger>,
}

impl<B: PlatformBackend> SeatService<B> {
    pub fn open(root: impl AsRef<Path>, backend: B) -> Result<Self, StoreError> {
        let store = LedgerStore::open(root)?;
        let ledger = store.load()?;
        Ok(Self {
            backend,
            store,
            ledger: Mutex::new(ledger),
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
        }
    }

    pub fn reconcile_startup(&self) -> Result<(), ApiError> {
        let mut current = self.lock();
        let mut next = current.clone();
        for seat in &mut next.seats {
            let result = if seat.autostart {
                self.backend.start(seat)
            } else {
                self.backend.status(seat)
            };
            seat.runtime = result.unwrap_or_else(|error| RuntimeStatus::failed(error.to_string()));
        }
        if next != *current {
            self.commit(&mut current, next)?;
        }
        Ok(())
    }

    fn list(&self) -> Result<CommandResult, ApiError> {
        let mut current = self.lock();
        let mut next = current.clone();
        for seat in &mut next.seats {
            seat.runtime = self
                .backend
                .status(seat)
                .unwrap_or_else(|error| RuntimeStatus::failed(error.to_string()));
        }
        if next != *current {
            self.commit(&mut current, next)?;
        }
        Ok(CommandResult::List {
            seats: current.seats.clone(),
        })
    }

    fn create(&self, request: CreateSeat) -> Result<CommandResult, ApiError> {
        let mut current = self.lock();
        let mut next = current.clone();
        let seat = next.allocate(request).map_err(ApiError::from)?;
        self.backend.provision(&seat).map_err(ApiError::from)?;
        if let Err(error) = self.commit(&mut current, next) {
            // The account exists but no ledger row does, so delete could never reach it.
            let _ = self.backend.remove(&seat);
            return Err(error);
        }
        Ok(CommandResult::Created { seat })
    }

    fn start(&self, id: &SeatId) -> Result<CommandResult, ApiError> {
        self.change_runtime(id, true)
    }

    fn stop(&self, id: &SeatId) -> Result<CommandResult, ApiError> {
        self.change_runtime(id, false)
    }

    fn change_runtime(&self, id: &SeatId, start: bool) -> Result<CommandResult, ApiError> {
        let mut current = self.lock();
        let original = current.seat(id).cloned().ok_or_else(|| not_found(id))?;
        let result = if start {
            self.backend.start(&original)
        } else {
            self.backend.stop(&original)
        };
        let mut next = current.clone();
        let seat = next
            .seats
            .iter_mut()
            .find(|seat| &seat.id == id)
            .expect("cloned ledger retains the selected seat");
        match result {
            Ok(status) => seat.runtime = status,
            Err(error) => {
                seat.runtime = RuntimeStatus::failed(error.to_string());
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
        let mut current = self.lock();
        let seat = current.seat(id).cloned().ok_or_else(|| not_found(id))?;
        self.backend.remove(&seat).map_err(ApiError::from)?;
        let mut next = current.clone();
        next.remove(id)
            .expect("cloned ledger retains the selected seat");
        self.commit(&mut current, next)?;
        Ok(CommandResult::Deleted { id: id.clone() })
    }

    fn doctor(&self) -> CommandResult {
        let current = self.lock();
        let mut diagnostics = vec![Diagnostic::info(
            "ledger",
            format!(
                "schema {} generation {} contains {} of 4 seats",
                current.schema_version,
                current.generation,
                current.seats.len()
            ),
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

    fn lock(&self) -> MutexGuard<'_, Ledger> {
        self.ledger
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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
