//! A person's row unlocked through the service, against [`fake`]. The backend is the real Linux
//! one, with its device list, trust copy and box pin, behind [`Warm`]: a real start writes into
//! `/run` and waits on a port the person's host would open, so `Warm` runs the real start only
//! where it refuses, at the desk, and otherwise brings the unit up itself.

use super::*;
use crate::ipc::{
    ApiError, Command, CommandResult, Device, DeviceKind, ErrorCode, KeyAlg, Needs, Proof,
    UnlockKey,
};
use crate::model::CreateSeat;
use crate::persistence::LedgerStore;
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair as _, ECDSA_P256_SHA256_ASN1_SIGNING};
use std::collections::BTreeSet;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::sync::atomic::AtomicUsize;

/// The machine the fake answers for.
#[derive(Default)]
struct Machine {
    active: AtomicBool,
    at_desk: AtomicBool,
    starts: AtomicUsize,
    /// What `pam-check` prints.
    verdict: Mutex<&'static str>,
}

impl Machine {
    fn verdict(&self, code: &'static str) {
        *self.verdict.lock().unwrap() = code;
    }
}

/// The Linux backend, with a start that brings the unit up unless the real start refuses.
struct Warm {
    linux: LinuxBackend,
    machine: Arc<Machine>,
}

impl PlatformBackend for Warm {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        self.linux.provision(seat)
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.machine.starts.fetch_add(1, Ordering::SeqCst);
        if self.machine.at_desk.load(Ordering::SeqCst) {
            return self.linux.start(seat);
        }
        self.machine.active.store(true, Ordering::SeqCst);
        Ok(RuntimeStatus::running())
    }

    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let stopped = self.linux.stop(seat);
        self.machine.active.store(false, Ordering::SeqCst);
        stopped
    }

    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.linux.remove(seat)
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.linux.status(seat)
    }

    fn reconcile(&self, seat: &Seat, status: &RuntimeStatus) {
        self.linux.reconcile(seat, status);
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        self.linux.doctor(ledger)
    }

    fn check_password(&self, seat: &Seat, password: &Secret) -> Result<(), BackendError> {
        self.linux.check_password(seat, password)
    }

    fn devices(
        &self,
        seat: &Seat,
        edit: &mut dyn FnMut(&mut DeviceList, i64),
    ) -> Result<DeviceList, BackendError> {
        self.linux.devices(seat, edit)
    }

    fn box_fingerprint(&self) -> Result<[u8; 32], BackendError> {
        self.linux.box_fingerprint()
    }
}

/// The box's certificate: three bytes of DER are all the pin reads.
const CERT: &str = "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";

struct Rig {
    temp: tempfile::TempDir,
    service: SeatService<Warm>,
    machine: Arc<Machine>,
    seat: Seat,
    /// A made seat beside it.
    made: Seat,
    _fake: fake::Installed,
}

/// A person's row for `jdoe`, uid 1500, its unit down, its host's directories made for this test's
/// user, beside a made seat; the box pairs devices 1 to 9.
fn rig() -> Rig {
    rig_for("jdoe", 1500, None)
}

/// What `getent` answers: its exit code and stdout.
type Getent = fn(&[&str]) -> (i32, String);

/// [`rig`] for `account` as user `uid`. `getent`, when given, answers in place of the fake's.
fn rig_for(account: &str, uid: u32, getent: Option<Getent>) -> Rig {
    let temp = tempfile::tempdir().unwrap();
    let (box_dir, state, home) = (
        temp.path().join("box"),
        temp.path().join("state"),
        temp.path().join("home"),
    );
    for dir in [&box_dir, &state, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    shared::ensure_layout(&state).unwrap();
    let records: Vec<_> = (1..=9_u8)
        .map(|n| serde_json::json!({ "name": format!("Device {n}"), "fingerprint": device(n).fingerprint, "grants": 7 }))
        .collect();
    let paired = serde_json::json!({ "clients": records }).to_string();
    std::fs::write(box_dir.join("punktfunk1-paired.json"), paired).unwrap();
    std::fs::write(box_dir.join("native-cert.pem"), CERT).unwrap();

    let request = CreateSeat {
        name: "Jane".into(),
        account: account.into(),
        autostart: true,
    };
    let mut ledger = Ledger::new();
    let seat = ledger.allocate_adopted_user(request, uid).unwrap();
    let made = CreateSeat {
        name: "Made".into(),
        account: "pf-seat-1".into(),
        autostart: false,
    };
    let made = ledger.allocate(made).unwrap();
    for dir in [
        shared::person_config_dir(&state, &seat.id),
        shared::trust_dir(&state, &seat.id),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let ledger_dir = temp.path().join("ledger");
    LedgerStore::open(&ledger_dir)
        .unwrap()
        .save(&ledger)
        .unwrap();

    let machine = Arc::new(Machine::default());
    machine.verdict("ok");
    let answers = Arc::clone(&machine);
    let row = format!(
        "{account}:x:{uid}:{uid}:Jane:{}:/bin/bash\n",
        home.display()
    );
    let name = account.to_owned();
    let installed = fake::install(move |program, args| match (program, args) {
        ("getent", _) if getent.is_some() => getent.unwrap()(args),
        ("getent", ["passwd", who]) if *who == name => (0, row.clone()),
        ("getent", _) => (2, String::new()),
        ("systemctl", ["show", ..]) => {
            let active = answers.active.load(Ordering::SeqCst);
            let state = if active { "active" } else { "inactive" };
            (
                0,
                format!("ActiveState={state}\nSubState=running\nResult=success\n"),
            )
        }
        ("loginctl", ["list-sessions", ..]) if answers.at_desk.load(Ordering::SeqCst) => {
            (0, format!("7 {uid} {name} seat0 tty2\n"))
        }
        ("loginctl", ["show-session", ..]) => (
            0,
            format!("Id=7\nName={name}\nSeat=seat0\nClass=user\nRemote=no\nState=active\n"),
        ),
        ("punktfunk-seats", ["pam-check", _]) => {
            (0, format!("{}\n", answers.verdict.lock().unwrap()))
        }
        _ => (0, String::new()),
    });
    let warm = Warm {
        linux: LinuxBackend::detached(&box_dir, &state),
        machine: Arc::clone(&machine),
    };
    let service = SeatService::open(&ledger_dir, warm).unwrap();
    Rig {
        temp,
        service,
        machine,
        seat,
        made,
        _fake: installed,
    }
}

fn device(n: u8) -> Device {
    Device {
        fingerprint: hex::encode([n; 32]),
        name: format!("Device {n}"),
    }
}

/// The number [`device`] made `fingerprint` from.
fn number(fingerprint: &str) -> u8 {
    hex::decode(&fingerprint[..2]).unwrap()[0]
}

fn n(device: &Device) -> u8 {
    number(&device.fingerprint)
}

fn password(stay: bool, enroll: Option<UnlockKey>) -> Proof {
    Proof::Password {
        password: Secret::new("hunter2 Bärbel".into()),
        stay_signed_in: stay,
        enroll,
    }
}

/// The code an error leads with, as the door reads it.
fn code(error: &ApiError) -> &str {
    match error.code {
        ErrorCode::Backend => error.message.split_once(": ").map_or("", |(c, _)| c),
        ErrorCode::InvalidRequest => "invalid_request",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Capacity => "capacity",
        _ => "other",
    }
}

impl Rig {
    fn state(&self) -> PathBuf {
        self.temp.path().join("state")
    }

    fn box_dir(&self) -> PathBuf {
        self.temp.path().join("box")
    }

    fn inner(&self) -> &Inner {
        &self.service.backend().linux.inner
    }

    fn ask(&self, command: Command) -> Result<CommandResult, ApiError> {
        self.service.dispatch(command)
    }

    fn unlock(&self, device: &Device, proof: Proof) -> Result<(Seat, bool), ApiError> {
        let id = self.seat.id.clone();
        match self.ask(Command::Unlock {
            id,
            device: device.clone(),
            proof,
        })? {
            CommandResult::Unlocked { seat, started } => Ok((seat, started)),
            other => panic!("an unlock answers unlocked: {other:?}"),
        }
    }

    fn refused(&self, device: &Device, proof: Proof) -> String {
        code(&self.unlock(device, proof).unwrap_err()).to_owned()
    }

    fn needs(&self, device: &Device) -> Needs {
        let id = self.seat.id.clone();
        let fingerprint = device.fingerprint.clone();
        match self.ask(Command::UnlockState { id, fingerprint }).unwrap() {
            CommandResult::UnlockState { needs } => needs,
            other => panic!("{other:?}"),
        }
    }

    /// The row's devices, by number.
    fn devices(&self) -> Vec<(u8, DeviceKind)> {
        let id = self.seat.id.clone();
        match self.ask(Command::Devices { id }).unwrap() {
            CommandResult::Devices { devices } => devices
                .into_iter()
                .map(|d| (number(&d.fingerprint), d.kind))
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    /// The devices the seat's host admits, by number: its trust copy's pairing list.
    fn on_seat(&self) -> Vec<u8> {
        let path = shared::trust_dir(&self.state(), &self.seat.id).join("punktfunk1-paired.json");
        let paired: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let clients = paired["clients"].as_array().unwrap();
        clients
            .iter()
            .map(|c| number(c["fingerprint"].as_str().unwrap()))
            .collect()
    }

    fn pam_calls(&self) -> usize {
        fake::calls()
            .iter()
            .filter(|c| c.starts_with("punktfunk-seats pam-check"))
            .count()
    }

    fn starts(&self) -> usize {
        self.machine.starts.load(Ordering::SeqCst)
    }

    fn challenge(&self, device: &Device) -> [u8; 32] {
        let id = self.seat.id.clone();
        let fingerprint = device.fingerprint.clone();
        match self.ask(Command::Challenge { id, fingerprint }).unwrap() {
            CommandResult::Challenge { challenge, seat } => {
                assert_eq!(seat, self.seat.id);
                hex::decode(challenge).unwrap().try_into().unwrap()
            }
            other => panic!("{other:?}"),
        }
    }

    fn stop(&self) {
        let id = self.seat.id.clone();
        self.ask(Command::Stop { id }).unwrap();
    }
}

/// SHA-256 of [`CERT`]'s DER.
fn box_fp() -> [u8; 32] {
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, &[1, 2, 3]);
    digest.as_ref().try_into().unwrap()
}

/// A P-256 key made for the test, as a device enrols it.
fn p256() -> (EcdsaKeyPair, UnlockKey) {
    let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
    let public = hex::encode(pair.public_key().as_ref());
    (
        pair,
        UnlockKey {
            alg: KeyAlg::P256,
            public,
        },
    )
}

fn signed(
    pair: &EcdsaKeyPair,
    challenge: &[u8; 32],
    seat: &SeatId,
    device: &Device,
    box_fp: &[u8; 32],
) -> Proof {
    let fp = crate::unlock::fingerprint_bytes(&device.fingerprint).unwrap();
    let message = crate::unlock::message(challenge, seat, &fp, box_fp);
    let signature = pair.sign(&SystemRandom::new(), &message).unwrap();
    Proof::Key {
        signature: hex::encode(signature.as_ref()),
    }
}

/// A password starts the stopped session and lists the device: the seat's host admits it and no
/// other, it rejoins with no proof, and a `start` of the row is refused.
#[test]
fn a_password_starts_the_session_and_admits_that_device_alone() {
    let rig = rig();
    let phone = device(1);
    let (seat, started) = rig.unlock(&phone, password(false, None)).unwrap();
    assert!(started);
    assert_eq!(seat.runtime.state, RuntimeState::Running);
    assert_eq!((rig.starts(), rig.pam_calls()), (1, 1));
    assert_eq!(rig.on_seat(), [n(&phone)]);
    assert_eq!(rig.devices(), [(n(&phone), DeviceKind::ForNow)]);
    assert_eq!(rig.needs(&phone), Needs::None);
    let file = devices::path(&rig.state(), &rig.seat.id);
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(!text.contains("hunter2"), "{text}");

    assert!(!rig.unlock(&phone, Proof::None).unwrap().1);
    let stranger = device(2);
    assert_eq!(rig.refused(&stranger, Proof::None), "unlock_required");
    assert_eq!(rig.needs(&stranger), Needs::Password);
    assert_eq!(rig.on_seat(), [n(&phone)], "never unlocked: absent");

    let id = rig.seat.id.clone();
    let start = rig.ask(Command::Start { id }).unwrap_err();
    assert_eq!(code(&start), "unlock_required");
    assert_eq!(rig.starts(), 1, "only an unlock starts a person's row");
}

/// Without a proof nothing starts. A device that stays signed in outlives the session and rejoins
/// the next one with no proof, but never starts one.
#[test]
fn a_device_that_stays_signed_in_rejoins_but_never_starts() {
    let rig = rig();
    let (tv, phone) = (device(1), device(2));
    assert_eq!(rig.refused(&tv, Proof::None), "unlock_required");
    assert_eq!(rig.starts(), 0);
    rig.unlock(&tv, password(true, None)).unwrap();
    rig.unlock(&phone, password(false, None)).unwrap();
    assert_eq!(rig.on_seat(), [n(&tv), n(&phone)]);

    rig.stop();
    assert_eq!(rig.on_seat(), [n(&tv)], "presence ended with the session");
    assert_eq!(rig.devices(), [(n(&tv), DeviceKind::Stay)]);
    assert_eq!(rig.needs(&tv), Needs::Password, "nothing runs to rejoin");
    assert_eq!(rig.refused(&tv, Proof::None), "unlock_required");
    assert_eq!(rig.starts(), 1);

    rig.unlock(&phone, password(false, None)).unwrap();
    assert_eq!(rig.needs(&tv), Needs::None);
    assert!(!rig.unlock(&tv, Proof::None).unwrap().1);
}

/// A wrong password lists nothing and starts nothing; the device then waits, and the account's
/// third wrong password in the window never reaches PAM.
#[test]
fn wrong_passwords_are_limited_per_device_and_per_account() {
    let rig = rig();
    rig.machine.verdict("credential_wrong");
    let wrong = rig.unlock(&device(1), password(false, None)).unwrap_err();
    assert_eq!(
        wrong.message,
        "credential_wrong: That isn't jdoe's password."
    );
    assert_eq!(
        rig.refused(&device(1), password(false, None)),
        "unlock_locked_out"
    );
    assert_eq!(rig.pam_calls(), 1, "the device waits before PAM");
    assert_eq!(
        rig.refused(&device(2), password(false, None)),
        "credential_wrong"
    );
    rig.machine.verdict("ok");
    assert_eq!(
        rig.refused(&device(3), password(false, None)),
        "unlock_locked_out"
    );
    assert_eq!(rig.pam_calls(), 2, "the account's third never reached PAM");
    assert_eq!((rig.starts(), rig.devices().len()), (0, 0));
}

/// Each PAM refusal answers its own code; one that judged no password costs nothing, one that
/// gave no verdict counts.
#[test]
fn each_pam_refusal_answers_its_code() {
    let rig = rig();
    for (verdict, answer) in [
        ("account_locked", "account_locked"),
        ("credential_must_change", "credential_must_change"),
        ("account_locked", "account_locked"),
        ("pam_unavailable", "pam_unavailable"),
    ] {
        rig.machine.verdict(verdict);
        assert_eq!(rig.refused(&device(1), password(false, None)), answer);
    }
    assert_eq!(
        rig.refused(&device(1), password(false, None)),
        "unlock_locked_out"
    );
    assert!(rig.devices().is_empty());
}

/// A cold start goes through the row's real start, so a person at the machine is refused and no
/// device is let on.
#[test]
fn a_person_at_the_machine_is_not_unlocked() {
    let rig = rig();
    rig.machine.at_desk.store(true, Ordering::SeqCst);
    let refused = rig.unlock(&device(1), password(true, None)).unwrap_err();
    assert_eq!(code(&refused), "account_at_desk");
    assert!(rig.devices().is_empty());
    assert!(!rig.machine.active.load(Ordering::SeqCst));
}

/// Every way the session ends ends every presence: a stop, the person's own login at the machine
/// and a unit seen down. The pairing list follows each time.
#[test]
fn every_end_of_the_session_ends_every_presence() {
    let rig = rig();
    let (phone, tv) = (device(1), device(2));
    rig.unlock(&phone, password(false, None)).unwrap();
    rig.unlock(&tv, password(true, None)).unwrap();
    rig.machine.at_desk.store(true, Ordering::SeqCst);
    yield_to_the_desk(rig.inner(), &rig.seat);
    assert!(fake::calls().contains(&"systemctl stop punktfunk-seat@jdoe.service".to_owned()));
    rig.machine.active.store(false, Ordering::SeqCst);
    assert_eq!(rig.on_seat(), [n(&tv)]);
    assert_eq!(rig.devices(), [(n(&tv), DeviceKind::Stay)]);

    rig.machine.at_desk.store(false, Ordering::SeqCst);
    rig.unlock(&phone, password(false, None)).unwrap();
    assert_eq!(rig.on_seat().len(), 2);
    let backend = &rig.service.backend().linux;
    backend.reconcile(&rig.seat, &RuntimeStatus::running());
    assert_eq!(rig.on_seat().len(), 2, "a running unit keeps them");
    backend.reconcile(
        &rig.seat,
        &RuntimeStatus::failed("seat unit failed: exit-code"),
    );
    assert_eq!(rig.on_seat(), [n(&tv)]);
}

/// `left` gives a device ten minutes; a rejoin inside them clears the end again.
#[test]
fn a_device_that_left_keeps_its_presence_for_a_while() {
    let rig = rig();
    let phone = device(1);
    rig.unlock(&phone, password(false, None)).unwrap();
    let presence = || {
        let file = devices::path(&rig.state(), &rig.seat.id);
        let list: DeviceList = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        list.devices[0].present.unwrap()
    };
    let (id, fingerprint) = (rig.seat.id.clone(), phone.fingerprint.clone());
    rig.ask(Command::Left { id, fingerprint }).unwrap();
    let ends = presence().expires_unix.unwrap();
    assert!((ends - presence().unlocked_unix - crate::unlock::GRACE_SECS).abs() <= 2);
    assert_eq!(rig.on_seat(), [n(&phone)], "inside its grace");
    rig.unlock(&phone, Proof::None).unwrap();
    assert_eq!(presence().expires_unix, None);
}

/// Signing a device out removes its record from the seat's host; signing out all empties it.
#[test]
fn signing_out_removes_the_record() {
    let rig = rig();
    let (tv, phone) = (device(1), device(2));
    rig.unlock(&tv, password(true, None)).unwrap();
    rig.unlock(&phone, password(true, None)).unwrap();
    let id = rig.seat.id.clone();
    let one = Some(tv.fingerprint.clone());
    rig.ask(Command::SignOut {
        id: id.clone(),
        fingerprint: one,
    })
    .unwrap();
    assert_eq!(rig.on_seat(), [n(&phone)]);
    rig.ask(Command::SignOut {
        id,
        fingerprint: None,
    })
    .unwrap();
    assert!(rig.on_seat().is_empty() && rig.devices().is_empty());
}

/// The seat's pairing list is the box's records for the devices admitted, kept as they are; the
/// other trust files copy as before. A box list that doesn't parse, or is over the size cap,
/// admits nobody.
#[test]
fn the_seat_s_pairing_list_is_the_box_s_filtered() {
    let rig = rig();
    let (state, box_dir, id) = (rig.state(), rig.box_dir(), rig.seat.id.clone());
    std::fs::write(box_dir.join("profiles.json"), "{\"profiles\":[]}").unwrap();
    let me = std::fs::metadata(&state).unwrap();
    let reader = shared::Reader::person(me.uid(), me.gid());
    let tablet = device(0xab);
    let records = serde_json::json!({ "clients": [
        { "name": "Phone", "fingerprint": device(1).fingerprint, "grants": 7, "extra": [1] },
        { "name": "Laptop", "fingerprint": device(2).fingerprint },
        { "name": "Tablet", "fingerprint": tablet.fingerprint.to_ascii_uppercase() },
        { "name": "No print" },
    ]});
    std::fs::write(box_dir.join("punktfunk1-paired.json"), records.to_string()).unwrap();
    let admitted = BTreeSet::from([
        device(1).fingerprint,
        tablet.fingerprint,
        device(4).fingerprint,
    ]);
    shared::refresh_person_trust(&box_dir, &state, &id, reader, &admitted).unwrap();
    let trust = shared::trust_dir(&state, &id);
    let copy: serde_json::Value =
        serde_json::from_slice(&std::fs::read(trust.join("punktfunk1-paired.json")).unwrap())
            .unwrap();
    assert_eq!(
        copy["clients"],
        serde_json::json!([records["clients"][0], records["clients"][2]])
    );
    assert_eq!(
        std::fs::read_to_string(trust.join("profiles.json")).unwrap(),
        "{\"profiles\":[]}"
    );

    let empty = serde_json::json!({ "clients": [] });
    let oversized = vec![b' '; shared::MAX_TRUST_BYTES as usize + 1];
    for bad in [b"not json".to_vec(), b"[1]".to_vec(), oversized] {
        std::fs::write(box_dir.join("punktfunk1-paired.json"), bad).unwrap();
        assert!(shared::rewrite_admitted(&box_dir, &state, &id, &admitted).unwrap());
        let copy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(trust.join("punktfunk1-paired.json")).unwrap())
                .unwrap();
        assert_eq!(copy, empty);
        std::fs::write(box_dir.join("punktfunk1-paired.json"), records.to_string()).unwrap();
        assert!(shared::rewrite_admitted(&box_dir, &state, &id, &admitted).unwrap());
    }
    assert!(
        !shared::rewrite_admitted(&box_dir, &state, &id, &admitted).unwrap(),
        "unchanged"
    );
}

/// A supervisor that restarts keeps presence for a session that still runs, drops presence that
/// ran out meanwhile and ends it for a session that stopped.
#[test]
fn a_restart_keeps_presence_only_for_a_running_session() {
    let rig = rig();
    let (phone, tv) = (device(1), device(2));
    rig.unlock(&phone, password(false, None)).unwrap();
    rig.unlock(&tv, password(false, None)).unwrap();
    let file = devices::path(&rig.state(), &rig.seat.id);
    let mut list: DeviceList = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    let day_ago = unix_now() - crate::unlock::CAP_SECS - 1;
    list.devices[1].present.as_mut().unwrap().unlocked_unix = day_ago;
    std::fs::write(&file, serde_json::to_vec(&list).unwrap()).unwrap();

    let mut ledger = Ledger::new();
    ledger.seats.push(rig.seat.clone());
    let restarted = LinuxBackend::detached(&rig.box_dir(), &rig.state());
    restarted.load_devices(&ledger);
    assert_eq!(rig.on_seat(), [n(&phone)], "the expired one is gone");
    restarted.reconcile(&rig.seat, &RuntimeStatus::running());
    assert_eq!(rig.on_seat(), [n(&phone)]);
    restarted.reconcile(&rig.seat, &RuntimeStatus::stopped());
    assert!(rig.on_seat().is_empty());
}

/// A device list that can't be read admits nobody, and the doctor says so.
#[test]
fn an_unreadable_device_list_admits_nobody() {
    let rig = rig();
    rig.unlock(&device(1), password(true, None)).unwrap();
    std::fs::write(
        devices::path(&rig.state(), &rig.seat.id),
        "{\"devices\":[{\"fingerprint\":1}]}",
    )
    .unwrap();
    let mut ledger = Ledger::new();
    ledger.seats.push(rig.seat.clone());
    let restarted = LinuxBackend::detached(&rig.box_dir(), &rig.state());
    restarted.load_devices(&ledger);
    assert!(rig.on_seat().is_empty());
    let rows = restarted.doctor(&ledger).unwrap();
    let row = rows.iter().find(|d| d.code == "device_list").unwrap();
    assert_eq!(row.level, DiagnosticLevel::Error);
    assert_eq!(row.seat_id.as_ref(), Some(&rig.seat.id));
}

/// An enrolled key proves the device with a fresh challenge, may start the session, and is the
/// only way back on once the session ended: stay and a key together still need the proof.
#[test]
fn a_key_proof_opens_the_seat_once_per_challenge() {
    let rig = rig();
    let phone = device(1);
    let (pair, key) = p256();
    rig.unlock(&phone, password(true, Some(key))).unwrap();
    assert_eq!(rig.devices(), [(n(&phone), DeviceKind::Key)]);
    rig.stop();
    assert!(
        rig.on_seat().is_empty(),
        "a key with no presence is not admitted"
    );
    assert_eq!(rig.needs(&phone), Needs::Key);
    assert!(!rig.state().join("box-pin").exists());

    let challenge = rig.challenge(&phone);
    let pin = std::fs::read_to_string(rig.state().join("box-pin")).unwrap();
    assert_eq!(pin.trim(), hex::encode(box_fp()));
    let proof = signed(&pair, &challenge, &rig.seat.id, &phone, &box_fp());
    let pam = rig.pam_calls();
    let (_, started) = rig.unlock(&phone, proof.clone()).unwrap();
    assert!(started, "a key proof may start the session");
    assert_eq!(rig.pam_calls(), pam, "a key proof never reaches PAM");
    assert_eq!(rig.on_seat(), [n(&phone)]);
    assert_eq!(
        rig.refused(&phone, proof),
        "proof_invalid",
        "the challenge is used up"
    );
    assert_eq!(rig.needs(&phone), Needs::None);

    let id = rig.seat.id.clone();
    let none = Command::Challenge {
        id,
        fingerprint: device(2).fingerprint,
    };
    assert_eq!(
        code(&rig.ask(none).unwrap_err()),
        "unlock_required",
        "no key, no challenge"
    );
}

/// A proof is refused for another seat, device, box or challenge, for a challenge used up, and
/// for a signature cut short. Each costs its device's allowance and never the account's.
#[test]
fn a_key_proof_for_anything_else_is_refused() {
    let rig = rig();
    let (pair, key) = p256();
    let phones: Vec<Device> = (1..=6).map(device).collect();
    for phone in &phones {
        rig.unlock(phone, password(false, Some(key.clone())))
            .unwrap();
    }
    let seat = rig.seat.id.clone();
    let cut = |proof: Proof| match proof {
        Proof::Key { signature } => Proof::Key {
            signature: signature[..signature.len() - 2].to_owned(),
        },
        other => other,
    };
    for (n, phone) in phones.iter().enumerate() {
        let challenge = rig.challenge(phone);
        let proof = match n {
            0 => signed(&pair, &challenge, &SeatId::random(), phone, &box_fp()),
            1 => signed(&pair, &challenge, &seat, &device(9), &box_fp()),
            2 => signed(&pair, &challenge, &seat, phone, &[0xbb; 32]),
            3 => signed(&pair, &[0; 32], &seat, phone, &box_fp()),
            4 => cut(signed(&pair, &challenge, &seat, phone, &box_fp())),
            _ => {
                let used = signed(&pair, &challenge, &seat, phone, &box_fp());
                rig.unlock(phone, used.clone()).unwrap();
                used
            }
        };
        assert_eq!(rig.refused(phone, proof), "proof_invalid", "case {n}");
    }
    let fresh = rig.challenge(&phones[0]);
    let good = signed(&pair, &fresh, &seat, &phones[0], &box_fp());
    assert_eq!(
        rig.refused(&phones[0], good),
        "unlock_locked_out",
        "the device's allowance"
    );
    rig.machine.verdict("credential_wrong");
    let pam = rig.pam_calls();
    assert_eq!(
        rig.refused(&device(7), password(false, None)),
        "credential_wrong"
    );
    assert_eq!(
        rig.refused(&device(8), password(false, None)),
        "credential_wrong"
    );
    assert_eq!(rig.pam_calls(), pam + 2, "the account's allowance is whole");
}

/// A key or a signature of the wrong size is refused before anything checks it, and before PAM.
#[test]
fn a_malformed_key_or_signature_is_refused_before_use() {
    let rig = rig();
    let phone = device(1);
    let (pair, key) = p256();
    let short = UnlockKey {
        public: key.public[..128].to_owned(),
        ..key.clone()
    };
    let long = UnlockKey {
        public: format!("{}00", key.public),
        ..key.clone()
    };
    let as_rsa = UnlockKey {
        alg: KeyAlg::Rsa2048,
        ..key.clone()
    };
    for bad in [short, long, as_rsa] {
        assert_eq!(
            rig.refused(&phone, password(false, Some(bad))),
            "invalid_request"
        );
    }
    assert_eq!(rig.pam_calls(), 0);
    rig.unlock(&phone, password(false, Some(key))).unwrap();
    let huge = Proof::Key {
        signature: "00".repeat(1025),
    };
    assert_eq!(rig.refused(&phone, huge), "invalid_request");
    assert_eq!(rig.refused(&phone, odd_proof()), "invalid_request");
    let upper = Proof::Key {
        signature: "AB".repeat(40),
    };
    assert_eq!(rig.refused(&phone, upper), "invalid_request");

    let challenge = rig.challenge(&phone);
    assert_eq!(rig.refused(&phone, odd_proof()), "invalid_request");
    let late = signed(&pair, &challenge, &rig.seat.id, &phone, &box_fp());
    assert_eq!(
        rig.refused(&phone, late),
        "proof_invalid",
        "used up by the refusal"
    );
    let keyless = signed(&pair, &challenge, &rig.seat.id, &device(2), &box_fp());
    assert_eq!(rig.refused(&device(2), keyless), "unlock_required");
    assert_eq!(rig.pam_calls(), 1, "a key proof never reaches PAM");
}

fn odd_proof() -> Proof {
    Proof::Key {
        signature: "abc".into(),
    }
}

/// An RSA 2048 key enrols and proves the device; one of another size is refused.
#[test]
fn an_rsa_2048_key_proves_the_device() {
    use aws_lc_rs::rsa::{KeyPair, KeySize};
    use aws_lc_rs::signature::RSA_PKCS1_SHA256;
    let rig = rig();
    let laptop = device(1);
    let rsa = |size| {
        let pair = KeyPair::generate(size).unwrap();
        let public = hex::encode(pair.public_key().as_ref());
        (
            pair,
            UnlockKey {
                alg: KeyAlg::Rsa2048,
                public,
            },
        )
    };
    let (_, big) = rsa(KeySize::Rsa3072);
    assert_eq!(
        rig.refused(&laptop, password(false, Some(big))),
        "invalid_request"
    );
    let (pair, key) = rsa(KeySize::Rsa2048);
    rig.unlock(&laptop, password(false, Some(key))).unwrap();
    let challenge = rig.challenge(&laptop);
    let fp = crate::unlock::fingerprint_bytes(&laptop.fingerprint).unwrap();
    let message = crate::unlock::message(&challenge, &rig.seat.id, &fp, &box_fp());
    let mut signature = vec![0; pair.public_modulus_len()];
    pair.sign(
        &RSA_PKCS1_SHA256,
        &SystemRandom::new(),
        &message,
        &mut signature,
    )
    .unwrap();
    let proof = Proof::Key {
        signature: hex::encode(signature),
    };
    rig.unlock(&laptop, proof).unwrap();
}

/// Once the box's certificate no longer matches its pin, no challenge is handed out, no key
/// proof passes, and the doctor says so.
#[test]
fn a_changed_box_certificate_refuses_key_proofs() {
    let rig = rig();
    let phone = device(1);
    let (pair, key) = p256();
    rig.unlock(&phone, password(false, Some(key))).unwrap();
    let challenge = rig.challenge(&phone);
    let other = CERT.replace("AQID", "BAUG");
    std::fs::write(rig.box_dir().join("native-cert.pem"), other).unwrap();
    let proof = signed(&pair, &challenge, &rig.seat.id, &phone, &box_fp());
    let refused = rig.unlock(&phone, proof).unwrap_err();
    assert!(refused
        .message
        .starts_with("proof_invalid: This machine's identity changed"));
    let (id, fingerprint) = (rig.seat.id.clone(), phone.fingerprint.clone());
    let again = rig.ask(Command::Challenge { id, fingerprint }).unwrap_err();
    assert_eq!(code(&again), "proof_invalid");
    let mut ledger = Ledger::new();
    ledger.seats.push(rig.seat.clone());
    let rows = rig.service.backend().doctor(&ledger).unwrap();
    let row = rows.iter().find(|d| d.code == "box_pin").unwrap();
    assert_eq!(row.level, DiagnosticLevel::Error);
}

/// Every command is a person's row's: any other row is an invalid request, a missing one is not
/// found, and a malformed device never reaches PAM.
#[test]
fn only_a_person_s_row_unlocks_and_only_a_well_formed_device() {
    let rig = rig();
    let fp = || device(1).fingerprint;
    let commands = |id: &SeatId| {
        vec![
            Command::Unlock {
                id: id.clone(),
                device: device(1),
                proof: Proof::None,
            },
            Command::UnlockState {
                id: id.clone(),
                fingerprint: fp(),
            },
            Command::Challenge {
                id: id.clone(),
                fingerprint: fp(),
            },
            Command::Left {
                id: id.clone(),
                fingerprint: fp(),
            },
            Command::SignOut {
                id: id.clone(),
                fingerprint: None,
            },
            Command::Devices { id: id.clone() },
        ]
    };
    for command in commands(&rig.made.id) {
        assert_eq!(
            rig.ask(command).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }
    for command in commands(&SeatId::random()) {
        assert_eq!(rig.ask(command).unwrap_err().code, ErrorCode::NotFound);
    }
    let backend = &rig.service.backend().linux;
    let refused = backend.devices(&rig.made, &mut |_, _| {}).unwrap_err();
    assert_eq!(refused.code, "not_a_person");

    let id = rig.seat.id.clone();
    let bad = [
        Device {
            fingerprint: "AB".repeat(32),
            ..device(1)
        },
        Device {
            fingerprint: "ab".repeat(31),
            ..device(1)
        },
        Device {
            name: String::new(),
            ..device(1)
        },
        Device {
            name: "x".repeat(65),
            ..device(1)
        },
        Device {
            name: "TV\u{202E}".into(),
            ..device(1)
        },
    ];
    for device in bad {
        let unlock = Command::Unlock {
            id: id.clone(),
            device,
            proof: password(true, None),
        };
        assert_eq!(rig.ask(unlock).unwrap_err().code, ErrorCode::InvalidRequest);
    }
    let left = Command::Left {
        id,
        fingerprint: "nope".into(),
    };
    assert_eq!(rig.ask(left).unwrap_err().code, ErrorCode::InvalidRequest);
    assert_eq!(rig.pam_calls(), 0);
}

/// Autostart never starts a person's row: the supervisor's own start reads its state instead.
#[test]
fn autostart_never_starts_a_person_s_row() {
    let rig = rig();
    assert!(rig.seat.autostart);
    rig.service.reconcile_startup().unwrap();
    assert_eq!(rig.starts(), 0);
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Each unlock writes one audit line with the seat, account, device, proof and result, and no
/// line of a failed or a passed unlock carries the password.
#[test]
fn the_audit_line_never_carries_the_secret() {
    let rig = rig();
    let log = Captured::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        rig.machine.verdict("credential_wrong");
        rig.unlock(&device(1), password(false, None)).unwrap_err();
        rig.machine.verdict("ok");
        rig.unlock(&device(2), password(true, None)).unwrap();
    });
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    let audit: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("punktfunk::audit"))
        .collect();
    assert_eq!(audit.len(), 2, "{text}");
    for (line, (fp, result)) in audit
        .iter()
        .zip([(device(1), "credential_wrong"), (device(2), "unlocked")])
    {
        for field in [
            format!("seat={}", rig.seat.id),
            "account=\"jdoe\"".to_owned(),
            format!("device=\"{}\"", fp.fingerprint),
            "proof=\"password\"".to_owned(),
            format!("result={result}"),
        ] {
            assert!(line.contains(&field), "{field} in {line}");
        }
    }
    assert!(audit[0].contains("account_failures=1"), "{}", audit[0]);
    assert!(
        !text.contains("hunter2") && !text.contains("Bärbel"),
        "{text}"
    );
}

/// Against the box's own `login` stack, through the `pam-check` child of the built
/// `punktfunk-seats`: the right password unlocks, a wrong one is refused, and the account's third
/// wrong one in the window is refused without a check. Each child appends to a count file before
/// it runs the real check, so the count is the checks PAM saw. Makes and removes the account
/// `pfunlock`, so it needs root.
#[test]
#[ignore = "needs root, libpam, the login service and a built punktfunk-seats; makes the account pfunlock"]
fn the_login_stack_unlocks_a_person_s_row() {
    use pam::tests::{run, seats_binary, REAL};
    const ACCOUNT: &str = "pfunlock";
    const PASSWORD: &str = "right-password-1";
    let binary = seats_binary();
    struct Gone;
    impl Drop for Gone {
        fn drop(&mut self) {
            let _ = std::process::Command::new("userdel")
                .args(["-r", ACCOUNT])
                .status();
            REAL.with(|real| *real.borrow_mut() = None);
        }
    }
    let _ = std::process::Command::new("userdel")
        .args(["-r", ACCOUNT])
        .status();
    run("useradd", &["-m", ACCOUNT]);
    let _gone = Gone;
    let mut chpasswd = std::process::Command::new("chpasswd")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let entry = format!("{ACCOUNT}:{PASSWORD}\n");
    std::io::Write::write_all(chpasswd.stdin.as_mut().unwrap(), entry.as_bytes()).unwrap();
    assert!(chpasswd.wait().unwrap().success());
    let id = std::process::Command::new("id")
        .args(["-u", ACCOUNT])
        .output()
        .unwrap();
    let uid: u32 = String::from_utf8(id.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    let scripts = tempfile::tempdir().unwrap();
    let wrapper = scripts.path().join("pam-check");
    let script = format!(
        "#!/bin/sh\necho x >> \"$0.count\"\nexec {} \"$@\"\n",
        binary.display()
    );
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    REAL.with(|real| *real.borrow_mut() = Some(wrapper.clone()));
    let real_getent = |args: &[&str]| {
        let out = std::process::Command::new("getent")
            .args(args)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    };
    let rig = rig_for(ACCOUNT, uid, Some(real_getent));
    rig.machine.active.store(true, Ordering::SeqCst);
    let checks = || {
        std::fs::read_to_string(scripts.path().join("pam-check.count"))
            .map_or(0, |c| c.lines().count())
    };
    let typed = |password: &str| Proof::Password {
        password: Secret::new(password.into()),
        stay_signed_in: false,
        enroll: None,
    };

    let (_, started) = rig.unlock(&device(1), typed(PASSWORD)).unwrap();
    assert!(!started, "the unit already ran");
    assert_eq!(rig.on_seat(), [1]);
    let wrong = rig
        .unlock(&device(2), typed("wrong-password-2"))
        .unwrap_err();
    assert_eq!(
        wrong.message,
        "credential_wrong: That isn't pfunlock's password."
    );
    assert_eq!(
        rig.refused(&device(3), typed("wrong-password-3")),
        "credential_wrong"
    );
    assert_eq!(checks(), 3);
    assert_eq!(
        rig.refused(&device(4), typed("wrong-password-4")),
        "unlock_locked_out"
    );
    assert_eq!(checks(), 3, "the third wrong password never reached PAM");
}
