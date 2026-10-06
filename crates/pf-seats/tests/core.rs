use pf_seats::backend::{BackendError, PlatformBackend};
use pf_seats::ipc::{Command, CommandResult, Diagnostic, ErrorCode};
use pf_seats::model::{CreateSeat, Ledger, RuntimeState, RuntimeStatus, Seat};
use pf_seats::persistence::{LedgerStore, LEDGER_FILE};
use pf_seats::service::SeatService;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct FakeBackend {
    state: Arc<FakeState>,
}

#[derive(Default)]
struct FakeState {
    calls: Mutex<Vec<String>>,
    statuses: Mutex<HashMap<String, RuntimeStatus>>,
}

impl FakeBackend {
    fn calls(&self) -> Vec<String> {
        self.state.calls.lock().unwrap().clone()
    }

    fn clear_calls(&self) {
        self.state.calls.lock().unwrap().clear();
    }

    fn record(&self, verb: &str, seat: &Seat) {
        self.state
            .calls
            .lock()
            .unwrap()
            .push(format!("{verb}:{}", seat.id));
    }
}

impl PlatformBackend for FakeBackend {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.record("provision", seat);
        self.state
            .statuses
            .lock()
            .unwrap()
            .insert(seat.id.to_string(), RuntimeStatus::stopped());
        Ok(())
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.record("start", seat);
        let status = RuntimeStatus::running();
        self.state
            .statuses
            .lock()
            .unwrap()
            .insert(seat.id.to_string(), status.clone());
        Ok(status)
    }

    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.record("stop", seat);
        let status = RuntimeStatus::stopped();
        self.state
            .statuses
            .lock()
            .unwrap()
            .insert(seat.id.to_string(), status.clone());
        Ok(status)
    }

    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.record("remove", seat);
        self.state.statuses.lock().unwrap().remove(seat.id.as_str());
        Ok(())
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.record("status", seat);
        Ok(self
            .state
            .statuses
            .lock()
            .unwrap()
            .get(seat.id.as_str())
            .cloned()
            .unwrap_or_default())
    }

    fn doctor(&self, _ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        Ok(vec![Diagnostic::info("fake", "fake backend is healthy")])
    }

    fn adopt(&self, seat: &Seat) -> Result<(), BackendError> {
        self.record("adopt", seat);
        if seat.account == "ghost" {
            return Err(BackendError::new("account_missing", "no such account"));
        }
        Ok(())
    }
}

fn adopt(account: &str) -> Command {
    Command::AdoptOwner {
        account: account.into(),
    }
}

fn create(name: &str, account: &str, autostart: bool) -> Command {
    Command::Create(CreateSeat {
        name: name.into(),
        account: account.into(),
        autostart,
    })
}

#[test]
fn command_dispatch_persists_runtime_and_calls_only_the_backend_seam() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();

    let created = service.dispatch(create("Desk", "pf-desk", true)).unwrap();
    let seat = match created {
        CommandResult::Created { seat } => seat,
        other => panic!("unexpected create result: {other:?}"),
    };
    assert_eq!(seat.display_slot, 12);
    assert_eq!(seat.runtime.state, RuntimeState::Stopped);

    let started = service
        .dispatch(Command::Start {
            id: seat.id.clone(),
        })
        .unwrap();
    assert!(matches!(
        started,
        CommandResult::Started { ref seat } if seat.runtime.state == RuntimeState::Running
    ));
    let listed = service.dispatch(Command::List).unwrap();
    assert!(matches!(
        listed,
        CommandResult::List { ref seats }
            if seats.len() == 1 && seats[0].runtime.state == RuntimeState::Running
    ));
    service
        .dispatch(Command::Stop {
            id: seat.id.clone(),
        })
        .unwrap();
    let doctor = service.dispatch(Command::Doctor).unwrap();
    assert!(matches!(
        doctor,
        CommandResult::Doctor { ref report } if report.healthy
    ));
    service
        .dispatch(Command::Delete {
            id: seat.id.clone(),
        })
        .unwrap();
    assert!(service.ledger().seats.is_empty());
    assert!(temp.path().join(LEDGER_FILE).exists());

    let calls = backend.calls();
    assert!(calls.iter().any(|call| call.starts_with("provision:")));
    assert!(calls.iter().any(|call| call.starts_with("start:")));
    assert!(calls.iter().any(|call| call.starts_with("stop:")));
    assert!(calls.iter().any(|call| call.starts_with("remove:")));
}

#[test]
fn startup_starts_autostart_only_and_refreshes_other_seats() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();
    let auto = match service.dispatch(create("Auto", "pf-auto", true)).unwrap() {
        CommandResult::Created { seat } => seat,
        other => panic!("unexpected result: {other:?}"),
    };
    let manual = match service
        .dispatch(create("Manual", "pf-manual", false))
        .unwrap()
    {
        CommandResult::Created { seat } => seat,
        other => panic!("unexpected result: {other:?}"),
    };
    backend.clear_calls();

    service.reconcile_startup().unwrap();
    let calls = backend.calls();
    assert!(calls.contains(&format!("start:{}", auto.id)));
    assert!(calls.contains(&format!("status:{}", manual.id)));
    assert!(!calls.contains(&format!("start:{}", manual.id)));
    assert_eq!(
        service.ledger().seat(&auto.id).unwrap().runtime.state,
        RuntimeState::Running
    );
}

/// Adopting the owner twice answers the one row; another account is a conflict; an account the
/// backend refuses leaves no row; deleting the row asks the backend to remove it.
#[test]
fn the_owner_row_is_adopted_once_and_released_by_delete() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();
    let owner = |result: Result<CommandResult, _>| match result.unwrap() {
        CommandResult::Created { seat } => seat,
        other => panic!("unexpected result: {other:?}"),
    };

    assert!(service.dispatch(adopt("ghost")).is_err());
    assert!(service.ledger().seats.is_empty());

    let first = owner(service.dispatch(adopt("enrico")));
    assert!(first.owner);
    let again = owner(service.dispatch(adopt("enrico")));
    assert_eq!(again.id, first.id);
    assert_eq!(
        backend
            .calls()
            .iter()
            .filter(|c| c.starts_with("adopt:"))
            .count(),
        2,
        "the refused account and the first adoption reach the backend; the repeat does not"
    );
    let other = service.dispatch(adopt("ben")).unwrap_err();
    assert_eq!(other.code, ErrorCode::Conflict);

    let seat = service.dispatch(create("Kid", "pf-kid", false)).unwrap();
    assert!(matches!(seat, CommandResult::Created { seat } if !seat.owner));
    service
        .dispatch(Command::Delete {
            id: first.id.clone(),
        })
        .unwrap();
    assert!(service.ledger().owner().is_none());
    assert_eq!(service.ledger().seats.len(), 1);
    assert!(backend.calls().contains(&format!("remove:{}", first.id)));
}

#[test]
fn duplicate_create_is_a_structured_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let service = SeatService::open(temp.path(), FakeBackend::default()).unwrap();
    service.dispatch(create("Desk", "pf-desk", false)).unwrap();
    let error = service
        .dispatch(create("desk", "PF-DESK-2", false))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Conflict);
}

#[test]
fn ledger_recovers_the_last_valid_backup_and_newer_temporary() {
    let temp = tempfile::tempdir().unwrap();
    let store = LedgerStore::open(temp.path()).unwrap();
    let mut first = Ledger::new();
    first.generation = 1;
    first
        .allocate(CreateSeat {
            name: "First".into(),
            account: "pf-first".into(),
            autostart: false,
        })
        .unwrap();
    store.save(&first).unwrap();

    let mut second = first.clone();
    second.generation = 2;
    second.seats[0].name = "Second".into();
    store.save(&second).unwrap();
    std::fs::write(temp.path().join(LEDGER_FILE), b"{truncated").unwrap();
    let recovered = store.load().unwrap();
    assert_eq!(recovered.generation, 1);
    assert_eq!(recovered.seats[0].name, "First");

    let mut newer = recovered.clone();
    newer.generation = 3;
    newer.seats[0].name = "Temporary".into();
    std::fs::write(
        temp.path().join(format!("{LEDGER_FILE}.tmp")),
        serde_json::to_vec(&newer).unwrap(),
    )
    .unwrap();
    let recovered = store.load().unwrap();
    assert_eq!(recovered.generation, 3);
    assert_eq!(recovered.seats[0].name, "Temporary");
    assert!(!temp.path().join(format!("{LEDGER_FILE}.tmp")).exists());
}

#[cfg(unix)]
#[test]
fn persistence_rejects_linked_roots_and_ledger_files() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let linked_root = parent.path().join("linked-root");
    symlink(&target, &linked_root).unwrap();
    assert!(LedgerStore::open(&linked_root).is_err());

    let real_root = parent.path().join("real-root");
    let store = LedgerStore::open(&real_root).unwrap();
    let outside = parent.path().join("outside");
    std::fs::write(&outside, b"do not touch").unwrap();
    symlink(&outside, real_root.join(LEDGER_FILE)).unwrap();
    assert!(store.load().is_err());
    assert_eq!(std::fs::read(outside).unwrap(), b"do not touch");
}

#[cfg(unix)]
#[test]
fn persisted_secrets_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().unwrap();
    let store = LedgerStore::open(temp.path()).unwrap();
    store.save(&Ledger::new()).unwrap();
    let root_mode = std::fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777;
    let file_mode = std::fs::metadata(temp.path().join(LEDGER_FILE))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(root_mode, 0o700);
    assert_eq!(file_mode, 0o600);
}

/// A start in the backend while the ledger answers: the start gate is the test's own.
#[derive(Clone, Default)]
struct SlowStart {
    inner: FakeBackend,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl PlatformBackend for SlowStart {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.inner.provision(seat)
    }
    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let (open, wake) = &*self.gate;
        let mut open = open.lock().unwrap();
        while !*open {
            open = wake.wait(open).unwrap();
        }
        drop(open);
        self.inner.start(seat)
    }
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.stop(seat)
    }
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.inner.remove(seat)
    }
    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.status(seat)
    }
}

#[test]
fn the_ledger_answers_while_a_seat_starts() {
    let temp = tempfile::tempdir().unwrap();
    let backend = SlowStart::default();
    let service = Arc::new(SeatService::open(temp.path(), backend.clone()).unwrap());
    let CommandResult::Created { seat } =
        service.dispatch(create("Desk", "pf-desk", false)).unwrap()
    else {
        panic!("create");
    };
    let starter = {
        let service = Arc::clone(&service);
        let id = seat.id.clone();
        std::thread::spawn(move || service.dispatch(Command::Start { id }))
    };
    // The start is parked in the backend; List still answers, and says starting.
    let mut seen = None;
    for _ in 0..200 {
        if let CommandResult::List { seats } = service.dispatch(Command::List).unwrap()
            && seats[0].runtime.state == RuntimeState::Starting
        {
            seen = Some(());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(seen.is_some(), "List never showed the seat starting");
    let (open, wake) = &*backend.gate;
    *open.lock().unwrap() = true;
    wake.notify_all();
    assert!(matches!(
        starter.join().unwrap(),
        Ok(CommandResult::Started { ref seat }) if seat.runtime.state == RuntimeState::Running
    ));
}
