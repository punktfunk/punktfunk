use pf_seats::backend::{BackendError, PlatformBackend};
use pf_seats::ipc::{Command, CommandResult, Diagnostic, ErrorCode, SeatingStatus};
use pf_seats::model::{CreateSeat, Ledger, RuntimeState, RuntimeStatus, Seat};
use pf_seats::persistence::{LedgerStore, LEDGER_FILE};
use pf_seats::service::SeatService;
use pf_seats::Secret;
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
    config_dir: Mutex<Option<std::path::PathBuf>>,
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
    fn host_config_dir(&self, _seat: &Seat) -> Option<std::path::PathBuf> {
        self.state.config_dir.lock().unwrap().clone()
    }

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
        if seat.account == "wedged" {
            return Err(BackendError::new("seat_exited", "the unit ended"));
        }
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

    fn reconcile(&self, seat: &Seat, _status: &RuntimeStatus) {
        self.record("reconcile", seat);
    }

    fn doctor(&self, _ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        Ok(vec![Diagnostic::info("fake", "fake backend is healthy")])
    }

    fn adopt(&self, seat: &Seat) -> Result<(), BackendError> {
        self.record("adopt", seat);
        if seat.account == "ghost" {
            return Err(BackendError::new("account_missing", "no such account"));
        }
        if seat.account == "ben" {
            return Err(BackendError::new(
                "owner_not_recorded",
                "not the recorded owner",
            ));
        }
        Ok(())
    }

    fn adopt_account(&self, seat: &Seat, _password: &Secret) -> Result<bool, BackendError> {
        self.record("adopt_account", seat);
        if seat.account == "Administrator" {
            return Err(BackendError::new(
                "account_is_administrator",
                "account is an administrator",
            ));
        }
        Ok(true)
    }

    fn reseal(&self, seat: &Seat, _password: &Secret) -> Result<(), BackendError> {
        self.record("reseal", seat);
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

/// Off Windows seats can't be turned off: a disable is refused and every seat keeps running.
#[cfg(not(windows))]
#[test]
fn a_refused_disable_leaves_the_seats_running() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();
    let CommandResult::Created { seat } =
        service.dispatch(create("Desk", "pf-desk", false)).unwrap()
    else {
        panic!("a seat is created")
    };
    service
        .dispatch(Command::Start {
            id: seat.id.clone(),
        })
        .unwrap();
    let refused = service.dispatch(Command::Disable {
        keep_accounts: true,
    });
    assert!(refused.is_err());
    assert!(!backend.calls().iter().any(|call| call.starts_with("stop:")));
    assert_eq!(
        service.ledger().seats[0].runtime.state,
        RuntimeState::Running
    );
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

/// A running seat's row carries the pin of the certificate its host wrote, once it wrote one.
#[test]
fn a_running_seat_records_its_host_pin() {
    let temp = tempfile::tempdir().unwrap();
    let host = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    *backend.state.config_dir.lock().unwrap() = Some(host.path().to_path_buf());
    let service = SeatService::open(temp.path(), backend).unwrap();
    let CommandResult::Created { seat } =
        service.dispatch(create("Desk", "pf-desk", false)).unwrap()
    else {
        panic!("create answered something else");
    };
    let start = Command::Start {
        id: seat.id.clone(),
    };
    service.dispatch(start).unwrap();
    assert_eq!(
        service.ledger().seats[0].fingerprint,
        None,
        "no identity yet"
    );

    let pem = "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";
    std::fs::write(host.path().join("native-cert.pem"), pem).unwrap();
    service.dispatch(Command::List).unwrap();
    assert_eq!(
        service.ledger().seats[0].fingerprint.as_deref(),
        Some("039058c6f2c0cb492c533b0a4d14ef77cc0f78abccced5287d84a1a2011cfb81"),
        "SHA-256 of the DER bytes 01 02 03"
    );
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

/// `open` closes the ledger root to its owner; `with_store` loads from a store the caller
/// opened and leaves the root as the caller set it, even when the ledger doesn't load.
#[cfg(unix)]
#[test]
fn a_service_over_an_opened_store_keeps_the_roots_mode() {
    use std::os::unix::fs::PermissionsExt;
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    let temp = tempfile::tempdir().unwrap();
    let store = LedgerStore::open(temp.path()).unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    SeatService::with_store(store, FakeBackend::default()).unwrap();
    assert_eq!(mode(temp.path()), 0o711);

    let store = LedgerStore::open(temp.path()).unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o711)).unwrap();
    std::fs::write(temp.path().join(LEDGER_FILE), b"{truncated").unwrap();
    assert!(SeatService::with_store(store, FakeBackend::default()).is_err());
    assert_eq!(mode(temp.path()), 0o711);
}

/// A backend whose provision fails with `code` after the fake made its account.
#[derive(Clone)]
struct FailsToProvision {
    inner: FakeBackend,
    code: &'static str,
}

impl PlatformBackend for FailsToProvision {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.inner.provision(seat)?;
        Err(BackendError::new(self.code, "provision stopped part-way"))
    }
    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
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

/// A create that fails after the account exists removes it, since no ledger row will; an account
/// that was never the seat's is left alone.
#[test]
fn a_failed_provision_removes_what_it_made_unless_the_account_was_not_ours() {
    for (code, removed) in [
        ("verify_failed", true),
        ("account_not_owned", false),
        ("account_invalid", false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let backend = FailsToProvision {
            inner: FakeBackend::default(),
            code,
        };
        let service = SeatService::open(temp.path(), backend.clone()).unwrap();
        let error = service
            .dispatch(create("Desk", "pf-desk", false))
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Backend);
        assert!(service.ledger().seats.is_empty());
        let calls = backend.inner.calls();
        assert_eq!(
            calls.iter().any(|call| call.starts_with("remove:")),
            removed,
            "{code}"
        );
    }
}

/// A row left for an owner since changed is replaced when the backend accepts the new account,
/// and stays, as a Conflict, when it doesn't.
#[test]
fn a_stale_owner_row_is_replaced_by_the_account_the_backend_accepts() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();
    let CommandResult::Created { seat: stale } = service.dispatch(adopt("enrico")).unwrap() else {
        panic!("adopt");
    };
    service.dispatch(create("Kid", "pf-kid", false)).unwrap();

    let refused = service.dispatch(adopt("ben")).unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict);
    assert_eq!(
        service.ledger().owner().map(|s| s.id.clone()),
        Some(stale.id.clone())
    );

    backend.clear_calls();
    let CommandResult::Created { seat: fresh } = service.dispatch(adopt("carol")).unwrap() else {
        panic!("adopt");
    };
    assert_ne!(fresh.id, stale.id);
    let ledger = service.ledger();
    assert_eq!(ledger.owner().map(|s| s.account.as_str()), Some("carol"));
    assert_eq!(
        ledger.seats.len(),
        2,
        "the profile stays, the stale row is gone"
    );
    assert_eq!(
        backend.calls(),
        [
            format!("adopt:{}", fresh.id),
            format!("remove:{}", stale.id)
        ],
        "the new row is adopted before the old one is released"
    );
}

fn adopt_existing(account: &str, sid: &str) -> Command {
    Command::Adopt {
        seat: CreateSeat {
            name: account.into(),
            account: account.into(),
            autostart: false,
        },
        sid: sid.into(),
        password: Secret::new("Bärbel!23".into()),
    }
}

/// An adopted row carries its SID and the Remote Desktop grant the backend reports; a refused
/// adopt leaves no row. Only an adopted row's password is resealed, and delete releases it
/// through `remove` once.
#[test]
fn an_adopted_account_is_resealed_and_released_through_the_backend() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();
    let created = |command| match service.dispatch(command).unwrap() {
        CommandResult::Created { seat } => seat,
        other => panic!("unexpected result: {other:?}"),
    };
    let credential = |id: &pf_seats::SeatId| Command::Credential {
        id: id.clone(),
        password: Secret::new("new one".into()),
    };

    let refused = service
        .dispatch(adopt_existing("Administrator", "S-1-5-21-1-2-3-1000"))
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Backend);
    assert!(service.ledger().seats.is_empty());
    assert_eq!(backend.calls().len(), 1, "the refusal calls nothing else");

    let sid = "S-1-5-21-1-2-3-1001";
    let adopted = created(adopt_existing("Bärbel Ott", sid));
    assert_eq!(adopted.adopted_sid.as_deref(), Some(sid));
    assert!(adopted.rdp_granted);
    assert_eq!(service.ledger().seats, std::slice::from_ref(&adopted));
    let made = created(create("Kid", "pf-kid", false));
    backend.clear_calls();

    let error = service.dispatch(credential(&made.id)).unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(backend.calls().is_empty(), "a made row reaches no backend");
    let resealed = service.dispatch(credential(&adopted.id)).unwrap();
    assert!(matches!(resealed, CommandResult::Resealed { seat } if seat.id == adopted.id));

    service
        .dispatch(Command::Delete {
            id: adopted.id.clone(),
        })
        .unwrap();
    assert_eq!(
        backend.calls(),
        [
            format!("reseal:{}", adopted.id),
            format!("remove:{}", adopted.id)
        ]
    );
    assert_eq!(service.ledger().seats, [made]);
    let gone = service.dispatch(credential(&adopted.id)).unwrap_err();
    assert_eq!(gone.code, ErrorCode::NotFound);
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
    fn reconcile(&self, seat: &Seat, status: &RuntimeStatus) {
        self.inner.reconcile(seat, status);
    }
}

/// A backend whose provision, remove, enable and disable park while the gate is shut.
#[derive(Clone)]
struct SlowWork {
    inner: FakeBackend,
    gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    parked: Arc<std::sync::atomic::AtomicUsize>,
}

impl SlowWork {
    fn new() -> Self {
        Self {
            inner: FakeBackend::default(),
            gate: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
            parked: Arc::default(),
        }
    }

    /// Shutting the gate also forgets who parked at it before.
    fn set_gate(&self, open: bool) {
        let (gate, wake) = &*self.gate;
        *gate.lock().unwrap() = open;
        if !open {
            self.parked.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        wake.notify_all();
    }

    fn park(&self) {
        let (gate, wake) = &*self.gate;
        let mut open = gate.lock().unwrap();
        if !*open {
            self.parked
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        while !*open {
            open = wake.wait(open).unwrap();
        }
    }

    fn wait_parked(&self) {
        while self.parked.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

impl PlatformBackend for SlowWork {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.park();
        self.inner.provision(seat)
    }
    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.start(seat)
    }
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.stop(seat)
    }
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.park();
        self.inner.remove(seat)
    }
    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.status(seat)
    }
    fn enable(
        &self,
        _allow_rdp_from_network: bool,
        _prepared: Vec<Diagnostic>,
    ) -> Result<SeatingStatus, BackendError> {
        self.park();
        Ok(SeatingStatus {
            enabled: true,
            checks: Vec::new(),
            allow_rdp_from_network: false,
        })
    }
    fn install_flatpak(&self, id: &str) -> Result<Option<String>, BackendError> {
        self.inner
            .state
            .calls
            .lock()
            .unwrap()
            .push(format!("install:{id}"));
        self.park();
        Ok(Some("1.0".into()))
    }
    fn remove_flatpak(&self, id: &str, _purge: bool) -> Result<(), BackendError> {
        self.inner
            .state
            .calls
            .lock()
            .unwrap()
            .push(format!("remove:{id}"));
        Ok(())
    }
}

/// An id that isn't a Flatpak app id never reaches the backend. An install keeps the ledger
/// readable while it runs, and a second change meanwhile answers Conflict.
#[test]
fn a_flatpak_change_is_checked_and_runs_one_at_a_time() {
    let temp = tempfile::tempdir().unwrap();
    let backend = SlowWork::new();
    let service = Arc::new(SeatService::open(temp.path(), backend.clone()).unwrap());
    let refused = service
        .dispatch(Command::InstallFlatpak {
            id: "--system".into(),
        })
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidRequest);
    assert!(backend.inner.calls().is_empty());

    backend.set_gate(false);
    let installing = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            service.dispatch(Command::InstallFlatpak {
                id: "net.pcsx2.PCSX2".into(),
            })
        })
    };
    backend.wait_parked();
    assert!(list_answers(&service), "List stalled behind an install");
    let busy = service
        .dispatch(Command::RemoveFlatpak {
            id: "org.DolphinEmu.dolphin-emu".into(),
            purge: false,
        })
        .unwrap_err();
    assert_eq!(busy.code, ErrorCode::Conflict);
    backend.set_gate(true);
    assert_eq!(
        installing.join().unwrap().unwrap(),
        CommandResult::Flatpak {
            id: "net.pcsx2.PCSX2".into(),
            version: Some("1.0".into()),
        }
    );
    assert_eq!(
        service
            .dispatch(Command::RemoveFlatpak {
                id: "net.pcsx2.PCSX2".into(),
                purge: true,
            })
            .unwrap(),
        CommandResult::Flatpak {
            id: "net.pcsx2.PCSX2".into(),
            version: None,
        }
    );
    assert_eq!(
        backend.inner.calls(),
        ["install:net.pcsx2.PCSX2", "remove:net.pcsx2.PCSX2"]
    );
}

/// Whether a `List` is answered while another command is parked in the backend.
fn list_answers<B: PlatformBackend>(service: &Arc<SeatService<B>>) -> bool {
    let (answer, answered) = std::sync::mpsc::channel();
    let service = Arc::clone(service);
    std::thread::spawn(move || {
        let _ = answer.send(service.dispatch(Command::List).is_ok());
    });
    answered.recv_timeout(std::time::Duration::from_secs(5)) == Ok(true)
}

/// Create, delete and Enable keep the ledger readable while their backend work runs, and the
/// ledger they commit afterwards is the one `List` has been keeping current.
#[test]
fn the_ledger_answers_while_a_seat_is_made_removed_or_seats_turn_on() {
    let temp = tempfile::tempdir().unwrap();
    let backend = SlowWork::new();
    let service = Arc::new(SeatService::open(temp.path(), backend.clone()).unwrap());
    let CommandResult::Created { seat: kept } =
        service.dispatch(create("Kept", "pf-kept", false)).unwrap()
    else {
        panic!("create");
    };

    backend.set_gate(false);
    let making = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || service.dispatch(create("Made", "pf-made", false)))
    };
    backend.wait_parked();
    assert!(list_answers(&service), "List stalled behind a create");
    backend.set_gate(true);
    let CommandResult::Created { seat: made } = making.join().unwrap().unwrap() else {
        panic!("create");
    };
    assert_eq!(service.ledger().seats.len(), 2);

    backend.set_gate(false);
    let removing = {
        let service = Arc::clone(&service);
        let id = made.id.clone();
        std::thread::spawn(move || service.dispatch(Command::Delete { id }))
    };
    backend.wait_parked();
    assert!(list_answers(&service), "List stalled behind a delete");
    backend.set_gate(true);
    removing.join().unwrap().unwrap();
    assert_eq!(service.ledger().seats.len(), 1);
    assert_eq!(service.ledger().seats[0].id, kept.id);

    backend.set_gate(false);
    let turning_on = {
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            service.dispatch(Command::Enable {
                allow_rdp_from_network: false,
                prepare: false,
            })
        })
    };
    backend.wait_parked();
    assert!(list_answers(&service), "List stalled behind Enable");
    backend.set_gate(true);
    turning_on.join().unwrap().unwrap();
}

/// A start that waits for a stop to end it, as a seat's unit does.
#[derive(Clone, Default)]
struct StopEndsStart {
    inner: FakeBackend,
    entered: Arc<std::sync::atomic::AtomicBool>,
    stopped: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl PlatformBackend for StopEndsStart {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.inner.provision(seat)
    }
    fn start(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let (stopped, wake) = &*self.stopped;
        let mut stopped = stopped.lock().unwrap();
        while !*stopped {
            stopped = wake.wait(stopped).unwrap();
        }
        Err(BackendError::new("seat_exited", "the unit ended"))
    }
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let (stopped, wake) = &*self.stopped;
        *stopped.lock().unwrap() = true;
        wake.notify_all();
        self.inner.stop(seat)
    }
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.inner.remove(seat)
    }
    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.status(seat)
    }
}

/// A stop reaches a seat mid-start at once rather than queueing behind it; the start answers
/// that it was stopped, and the seat ends up stopped, not failed.
#[test]
fn a_stop_reaches_a_seat_that_is_starting() {
    let temp = tempfile::tempdir().unwrap();
    let backend = StopEndsStart::default();
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
    while !backend.entered.load(std::sync::atomic::Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let (answer, answered) = std::sync::mpsc::channel();
    {
        let service = Arc::clone(&service);
        let id = seat.id.clone();
        std::thread::spawn(move || {
            let _ = answer.send(service.dispatch(Command::Stop { id }));
        });
    }
    let stopped = answered
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the stop queued behind the start");
    assert!(matches!(
        stopped,
        Ok(CommandResult::Stopped { ref seat }) if seat.runtime.state == RuntimeState::Stopped
    ));
    let started = starter.join().unwrap().unwrap_err();
    assert_eq!(started.code, ErrorCode::Conflict);
    assert_eq!(
        service.ledger().seats[0].runtime.state,
        RuntimeState::Stopped
    );
    let stops = backend
        .inner
        .calls()
        .iter()
        .filter(|call| call.starts_with("stop:"))
        .count();
    assert_eq!(stops, 2, "once to end the start, once in turn");
}

/// A `List` reconciles a seat that is at rest and leaves one that is starting: its unit is down
/// until the start brings it up, and reading that as a stopped seat would tear its proxy down.
#[test]
fn a_list_reconciles_a_seat_at_rest_and_never_one_that_starts() {
    let temp = tempfile::tempdir().unwrap();
    let backend = SlowStart::default();
    let service = Arc::new(SeatService::open(temp.path(), backend.clone()).unwrap());
    let CommandResult::Created { seat } =
        service.dispatch(create("Desk", "pf-desk", false)).unwrap()
    else {
        panic!("create");
    };
    let reconciled = format!("reconcile:{}", seat.id);
    service.dispatch(Command::List).unwrap();
    assert!(backend.inner.calls().contains(&reconciled));

    let starter = {
        let service = Arc::clone(&service);
        let id = seat.id.clone();
        std::thread::spawn(move || service.dispatch(Command::Start { id }))
    };
    loop {
        let CommandResult::List { seats } = service.dispatch(Command::List).unwrap() else {
            panic!("list");
        };
        if seats[0].runtime.state == RuntimeState::Starting {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    backend.inner.clear_calls();
    service.dispatch(Command::List).unwrap();
    assert!(!backend.inner.calls().contains(&reconciled));

    let (open, wake) = &*backend.gate;
    *open.lock().unwrap() = true;
    wake.notify_all();
    starter.join().unwrap().unwrap();
    service.dispatch(Command::List).unwrap();
    assert!(backend.inner.calls().contains(&reconciled));
}

/// A start that fails is followed by a reconcile of its seat, so what the start brought up
/// beside the unit does not wait for the next `List` to be taken back.
#[test]
fn a_failed_start_reconciles_its_seat() {
    let temp = tempfile::tempdir().unwrap();
    let backend = FakeBackend::default();
    let service = SeatService::open(temp.path(), backend.clone()).unwrap();
    let CommandResult::Created { seat } =
        service.dispatch(create("Wedged", "wedged", false)).unwrap()
    else {
        panic!("create");
    };
    backend.clear_calls();
    let failed = service
        .dispatch(Command::Start {
            id: seat.id.clone(),
        })
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::Backend);
    let calls = backend.calls();
    let at = |call: String| calls.iter().position(|c| *c == call);
    let (start, reconcile) = (
        at(format!("start:{}", seat.id)),
        at(format!("reconcile:{}", seat.id)),
    );
    assert!(start.is_some() && reconcile > start, "{calls:?}");
}

/// Clients that create, list, start, stop and delete at once leave exactly the seats that were
/// created and not deleted, and the file on disk is the ledger in memory. A create, a delete and a
/// `List` each write the ledger without the others' lock held across their backend work.
#[test]
fn concurrent_commands_never_lose_or_duplicate_a_row() {
    let temp = tempfile::tempdir().unwrap();
    let service = Arc::new(SeatService::open(temp.path(), FakeBackend::default()).unwrap());
    let made = Arc::new(Mutex::new(Vec::new()));
    let gone = Arc::new(Mutex::new(Vec::new()));
    let clients: Vec<_> = (0..4)
        .map(|client| {
            let (service, made, gone) =
                (Arc::clone(&service), Arc::clone(&made), Arc::clone(&gone));
            std::thread::spawn(move || {
                for round in 0..60 {
                    let first = service.ledger().seats.first().map(|seat| seat.id.clone());
                    let command = match (client + round) % 5 {
                        0 => create(
                            &format!("S{client}-{round}"),
                            &format!("pf-{client}-{round}"),
                            false,
                        ),
                        1 => Command::List,
                        2 => first.map_or(Command::List, |id| Command::Start { id }),
                        3 => first.map_or(Command::List, |id| Command::Stop { id }),
                        _ => first.map_or(Command::List, |id| Command::Delete { id }),
                    };
                    match service.dispatch(command) {
                        Ok(CommandResult::Created { seat }) => made.lock().unwrap().push(seat.id),
                        Ok(CommandResult::Deleted { id }) => gone.lock().unwrap().push(id),
                        _ => {}
                    }
                }
            })
        })
        .collect();
    for client in clients {
        client.join().unwrap();
    }

    let gone = gone.lock().unwrap();
    let mut expected: Vec<_> = made
        .lock()
        .unwrap()
        .iter()
        .filter(|id| !gone.contains(id))
        .cloned()
        .collect();
    let mut found: Vec<_> = service
        .ledger()
        .seats
        .iter()
        .map(|seat| seat.id.clone())
        .collect();
    expected.sort_by_key(|id| id.to_string());
    found.sort_by_key(|id| id.to_string());
    assert_eq!(found, expected);
    assert_eq!(
        LedgerStore::open(temp.path()).unwrap().load().unwrap(),
        service.ledger()
    );
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
