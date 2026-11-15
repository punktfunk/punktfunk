//! Desk pads through the service: the ledger says who holds a pad, the backend hands it over.

use pf_seats::backend::{BackendError, PlatformBackend, PresentPad};
use pf_seats::ipc::{ApiError, Command, CommandResult, DeskPad, ErrorCode, PadState};
use pf_seats::model::{CreateSeat, Ledger, RuntimeStatus, Seat, SeatId, MAX_LENT_PADS};
use pf_seats::service::SeatService;
use std::sync::{Arc, Mutex};

/// A box with pads plugged in. `settled` records each hand-over: the key and its holder's
/// account, `desk` for none.
#[derive(Clone, Default)]
struct Box_ {
    plugged: Arc<Mutex<Vec<PresentPad>>>,
    settled: Arc<Mutex<Vec<String>>>,
    removed: Arc<Mutex<Vec<usize>>>,
    refuse_settle: Arc<Mutex<bool>>,
}

fn pad(key: &str, name: &str) -> PresentPad {
    PresentPad {
        key: key.into(),
        name: name.into(),
        vendor_id: 0x28de,
        product_id: 0x1304,
        port: Some(format!("port-of-{key}")),
    }
}

impl Box_ {
    fn with(pads: &[PresentPad]) -> Box_ {
        let here = Box_::default();
        *here.plugged.lock().unwrap() = pads.to_vec();
        here
    }

    fn settled(&self) -> Vec<String> {
        std::mem::take(&mut self.settled.lock().unwrap())
    }
}

impl PlatformBackend for Box_ {
    fn provision(&self, _seat: &Seat) -> Result<(), BackendError> {
        Ok(())
    }

    fn start(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Ok(RuntimeStatus::running())
    }

    fn stop(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Ok(RuntimeStatus::stopped())
    }

    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.removed.lock().unwrap().push(seat.lent.len());
        Ok(())
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        Ok(seat.runtime.clone())
    }

    fn desk_pads(&self, _ledger: &Ledger) -> Result<Vec<PresentPad>, BackendError> {
        Ok(self.plugged.lock().unwrap().clone())
    }

    fn lendable_pad(&self, key: &str, _ledger: &Ledger) -> Result<PresentPad, BackendError> {
        let plugged = self.plugged.lock().unwrap();
        plugged
            .iter()
            .find(|pad| pad.key == key)
            .cloned()
            .ok_or_else(|| BackendError::new("pad_not_present", "not plugged in"))
    }

    fn settle_pads(&self, ledger: &Ledger, keys: &[String]) -> Result<(), BackendError> {
        if *self.refuse_settle.lock().unwrap() {
            return Err(BackendError::new("pad_handover", "the device said no"));
        }
        for key in keys {
            let holder = ledger.holder_of(key).map_or("desk", |s| s.account.as_str());
            self.settled.lock().unwrap().push(format!("{key}>{holder}"));
        }
        Ok(())
    }
}

fn seat(service: &SeatService<Box_>, name: &str, account: &str) -> SeatId {
    let made = service.dispatch(Command::Create(CreateSeat {
        name: name.into(),
        account: account.into(),
        autostart: false,
    }));
    match made.unwrap() {
        CommandResult::Created { seat } => seat.id,
        other => panic!("unexpected create result: {other:?}"),
    }
}

fn pads(answer: Result<CommandResult, ApiError>) -> Vec<DeskPad> {
    match answer.unwrap() {
        CommandResult::Pads { pads } => pads,
        other => panic!("unexpected pads result: {other:?}"),
    }
}

fn lend(id: &SeatId, key: &str) -> Command {
    Command::Lend {
        id: id.clone(),
        key: key.into(),
    }
}

fn take_back(key: &str) -> Command {
    Command::TakeBack { key: key.into() }
}

/// A lent pad is its seat's: parked while the seat is stopped, live once it runs, and the
/// desk's again when taken back. Each change hands the hardware over once.
#[test]
fn a_pad_is_lent_parked_live_and_taken_back() {
    let temp = tempfile::tempdir().unwrap();
    let here = Box_::with(&[pad("usb-a", "Puck"), pad("usb-b", "Wheel")]);
    let service = SeatService::open(temp.path(), here.clone()).unwrap();
    let kid = seat(&service, "Kid", "pf-seat-1");

    let listed = pads(service.dispatch(Command::Pads));
    assert!(listed
        .iter()
        .all(|p| p.state == PadState::Desk && p.present));

    let after = pads(service.dispatch(lend(&kid, "usb-a")));
    let puck = after.iter().find(|p| p.key == "usb-a").unwrap();
    assert_eq!(
        (puck.state, puck.seat.as_ref()),
        (PadState::Parked, Some(&kid))
    );
    assert_eq!(here.settled(), ["usb-a>pf-seat-1"]);
    let kept = &service.ledger().seats[0].lent[0];
    assert_eq!(
        (kept.name.as_str(), kept.port.as_deref()),
        ("Puck", Some("port-of-usb-a"))
    );

    service
        .dispatch(Command::Start { id: kid.clone() })
        .unwrap();
    let running = pads(service.dispatch(Command::Pads));
    let puck = running.iter().find(|p| p.key == "usb-a").unwrap();
    assert_eq!(puck.state, PadState::Live);

    // Lending it to the seat that holds it changes nothing and hands nothing over.
    pads(service.dispatch(lend(&kid, "usb-a")));
    assert!(here.settled().is_empty());

    let back = pads(service.dispatch(take_back("usb-a")));
    assert!(back.iter().all(|p| p.state == PadState::Desk));
    assert_eq!(here.settled(), ["usb-a>desk"]);
    pads(service.dispatch(take_back("usb-a")));
    assert!(here.settled().is_empty(), "a pad nobody holds stays put");
}

/// A lend to another seat moves the pad in one step, and a lent pad that is unplugged stays
/// listed with its seat.
#[test]
fn a_pad_moves_between_seats_and_stays_lent_unplugged() {
    let temp = tempfile::tempdir().unwrap();
    let here = Box_::with(&[pad("usb-a", "Puck")]);
    let service = SeatService::open(temp.path(), here.clone()).unwrap();
    let kid = seat(&service, "Kid", "pf-seat-1");
    let guest = seat(&service, "Guest", "pf-seat-2");

    pads(service.dispatch(lend(&kid, "usb-a")));
    pads(service.dispatch(lend(&guest, "usb-a")));
    assert_eq!(here.settled(), ["usb-a>pf-seat-1", "usb-a>pf-seat-2"]);
    let ledger = service.ledger();
    assert!(ledger.seat(&kid).unwrap().lent.is_empty());
    assert_eq!(ledger.holder_of("usb-a").unwrap().id, guest);

    here.plugged.lock().unwrap().clear();
    let unplugged = pads(service.dispatch(Command::Pads));
    assert_eq!(unplugged.len(), 1);
    let puck = &unplugged[0];
    assert!(!puck.present);
    assert_eq!(
        (puck.name.as_str(), puck.vendor_id, puck.seat.as_ref()),
        ("Puck", 0x28de, Some(&guest))
    );
    // Taking back an unplugged pad works; lending one doesn't.
    let refused = service.dispatch(lend(&kid, "usb-a")).unwrap_err();
    assert!(
        refused.message.starts_with("pad_not_present"),
        "{refused:?}"
    );
    assert!(pads(service.dispatch(take_back("usb-a"))).is_empty());
}

/// A seat holds four pads; the fifth is refused in words for the operator and changes nothing.
#[test]
fn a_seat_holds_four_pads() {
    let temp = tempfile::tempdir().unwrap();
    let keys: Vec<String> = (0..=MAX_LENT_PADS).map(|n| format!("usb-{n}")).collect();
    let plugged: Vec<PresentPad> = keys.iter().map(|k| pad(k, "Pad")).collect();
    let here = Box_::with(&plugged);
    let service = SeatService::open(temp.path(), here.clone()).unwrap();
    let kid = seat(&service, "Kid", "pf-seat-1");
    for key in &keys[..MAX_LENT_PADS] {
        pads(service.dispatch(lend(&kid, key)));
    }
    here.settled();
    let refused = service
        .dispatch(lend(&kid, &keys[MAX_LENT_PADS]))
        .unwrap_err();
    assert_eq!(refused.said.as_deref(), Some("pads_full"));
    assert_eq!(service.ledger().seats[0].lent.len(), MAX_LENT_PADS);
    assert!(here.settled().is_empty());
}

/// What isn't a key, a seat or a plugged-in pad is refused before any row changes.
#[test]
fn a_lend_names_a_real_key_seat_and_pad() {
    let temp = tempfile::tempdir().unwrap();
    let here = Box_::with(&[pad("usb-a", "Puck")]);
    let service = SeatService::open(temp.path(), here.clone()).unwrap();
    let kid = seat(&service, "Kid", "pf-seat-1");

    for key in ["", "../etc", "a/b", "-x", "a b"] {
        let refused = service.dispatch(lend(&kid, key)).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidRequest, "{key:?}");
        let refused = service.dispatch(take_back(key)).unwrap_err();
        assert_eq!(refused.code, ErrorCode::InvalidRequest, "{key:?}");
    }
    let nobody = SeatId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let refused = service.dispatch(lend(&nobody, "usb-a")).unwrap_err();
    assert_eq!(refused.code, ErrorCode::NotFound);
    let refused = service.dispatch(lend(&kid, "usb-gone")).unwrap_err();
    assert!(
        refused.message.starts_with("pad_not_present"),
        "{refused:?}"
    );
    assert!(service.ledger().seats[0].lent.is_empty());
    assert!(here.settled().is_empty());
}

/// A backend that can't hand a pad over leaves every row as it was.
#[test]
fn a_failed_hand_over_restores_the_rows() {
    let temp = tempfile::tempdir().unwrap();
    let here = Box_::with(&[pad("usb-a", "Puck")]);
    let service = SeatService::open(temp.path(), here.clone()).unwrap();
    let kid = seat(&service, "Kid", "pf-seat-1");
    let guest = seat(&service, "Guest", "pf-seat-2");
    pads(service.dispatch(lend(&kid, "usb-a")));

    *here.refuse_settle.lock().unwrap() = true;
    let refused = service.dispatch(lend(&guest, "usb-a")).unwrap_err();
    assert!(refused.message.starts_with("pad_handover"), "{refused:?}");
    assert_eq!(service.ledger().holder_of("usb-a").unwrap().id, kid);
    let refused = service.dispatch(take_back("usb-a")).unwrap_err();
    assert!(refused.message.starts_with("pad_handover"), "{refused:?}");
    assert_eq!(service.ledger().holder_of("usb-a").unwrap().id, kid);
}

/// A deleted seat's pads go with its row, and the backend's remove sees them to release.
#[test]
fn a_deleted_seat_releases_its_pads() {
    let temp = tempfile::tempdir().unwrap();
    let here = Box_::with(&[pad("usb-a", "Puck")]);
    let service = SeatService::open(temp.path(), here.clone()).unwrap();
    let kid = seat(&service, "Kid", "pf-seat-1");
    pads(service.dispatch(lend(&kid, "usb-a")));
    service.dispatch(Command::Delete { id: kid }).unwrap();
    assert_eq!(*here.removed.lock().unwrap(), [1]);
    let listed = pads(service.dispatch(Command::Pads));
    assert_eq!(listed[0].state, PadState::Desk);
}

/// A backend with no lending answers so, in words for the operator.
#[test]
fn a_backend_without_lending_says_so() {
    struct Plain;
    impl PlatformBackend for Plain {
        fn provision(&self, _seat: &Seat) -> Result<(), BackendError> {
            Ok(())
        }
        fn start(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
            Ok(RuntimeStatus::running())
        }
        fn stop(&self, _seat: &Seat) -> Result<RuntimeStatus, BackendError> {
            Ok(RuntimeStatus::stopped())
        }
        fn remove(&self, _seat: &Seat) -> Result<(), BackendError> {
            Ok(())
        }
        fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
            Ok(seat.runtime.clone())
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let service = SeatService::open(temp.path(), Plain).unwrap();
    let refused = service.dispatch(Command::Pads).unwrap_err();
    assert_eq!(refused.said.as_deref(), Some("pads_unsupported"));
}

/// A ledger written before lends existed loads, a key is one row's only, and a name is kept
/// clean.
#[test]
fn the_ledger_keeps_lends_valid() {
    use pf_seats::model::{pad_name, valid_pad_key, LentPad};
    let mut ledger = Ledger::new();
    for n in 1..=2 {
        ledger
            .allocate(CreateSeat {
                name: format!("Player {n}"),
                account: format!("pf-seat-{n}"),
                autostart: false,
            })
            .unwrap();
    }
    let old = serde_json::to_string(&ledger).unwrap();
    assert!(!old.contains("lent"), "an empty list is not written");
    let loaded: Ledger = serde_json::from_str(&old).unwrap();
    assert!(loaded.seats.iter().all(|seat| seat.lent.is_empty()));

    let lent = |key: &str, name: &str| LentPad {
        key: key.into(),
        name: name.into(),
        vendor_id: 0x054c,
        product_id: 0x0ce6,
        port: None,
    };
    ledger.seats[0].lent.push(lent("usb-a", "DualSense"));
    ledger.validate().unwrap();
    ledger.seats[1].lent.push(lent("usb-a", "DualSense"));
    assert!(ledger.validate().is_err(), "one key on two rows");
    ledger.seats[1].lent[0] = lent("usb-b", " padded ");
    assert!(
        ledger.validate().is_err(),
        "a name the cleaner would change"
    );
    ledger.seats[1].lent[0] = lent("usb b", "DualSense");
    assert!(ledger.validate().is_err(), "a key with a space");
    ledger.seats[1].lent[0] = LentPad {
        port: Some("0000:00:14.0/2".into()),
        ..lent("usb-b", "DualSense")
    };
    assert!(ledger.validate().is_err(), "a port that is no key");

    assert_eq!(
        pad_name("  Wireless\u{7} Controller\n"),
        "Wireless Controller"
    );
    assert_eq!(pad_name("\u{1}\u{2}"), "Controller");
    assert_eq!(pad_name(&"x".repeat(200)).chars().count(), 64);
    assert!(valid_pad_key("usb-28de-1304-s-FXA99614007BD"));
    assert!(valid_pad_key("usb-054c-0ce6-p-0000_00_14.0-2.1"));
    for bad in ["", ".hidden", "-opt", "a/b", "a:b", "a b", &"k".repeat(129)] {
        assert!(!valid_pad_key(bad), "{bad:?}");
    }
}
