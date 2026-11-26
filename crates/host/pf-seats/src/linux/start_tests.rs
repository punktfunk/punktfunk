//! Tests of what each start, stop and delete asks of the machine, per row kind, against
//! [`fake`]: every program that runs, in order, and the files a cold start leaves under `/run`.
//!
//! Some need root, because the code under test hands folders to other users or writes under
//! `/run`:
//!
//! - a made seat's provision, and any row's cold start
//! - a warm start of a made seat or of the owner's row
//! - the owner's desk start, and a supervisor start over the owner's row
//! - a delete's checks of its `/run` files and of the owner's library drives
//!
//! These run anywhere: a person's warm start, every yield, the owner's release, and the stop of
//! the owner's or a person's row. A made seat's stop and delete need `/proc`.

use super::sync::yield_to_the_desk;
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::atomic::Ordering;

const INACTIVE: &str = "ActiveState=inactive\nSubState=dead\nResult=success\n";
const ACTIVE: &str = "ActiveState=active\nSubState=running\nResult=success\n";
const FAILED: &str =
    "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=1\n";
/// The login helper's end with no password handed to it: the start falls back.
const NO_PASSWORD: &str =
    "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=95\n";

/// The games group's id, which root gives real folders. Like [`MADE_UID`] it is below 65536,
/// so the chown also works where a container's user namespace maps only those ids.
const GAMES_GID: u32 = 59_971;

/// Held by each test that writes a row's real files under `/run`.
static RUN: Mutex<()> = Mutex::new(());

fn row(account: &str, id: &str, owner: bool, adopted_uid: Option<u32>) -> Seat {
    let kind = match (owner, adopted_uid) {
        (true, _) => RowKind::owner(),
        (false, Some(uid)) => RowKind::linux_person(uid),
        (false, None) => RowKind::made(),
    };
    Seat {
        id: SeatId::parse(id).unwrap(),
        name: "Desk".into(),
        account: account.into(),
        display_slot: 12,
        native_port: 9779,
        mgmt_port: 48_123,
        autostart: false,
        runtime: RuntimeStatus::stopped(),
        kind,
        fingerprint: None,
        lent: Vec::new(),
    }
}

/// The machine: `passwd` is the row's account as `getent` prints it, its primary group is its
/// own, and `systemctl show` answers `shows` in turn, the last one from then on. Every
/// `systemctl start` records whether the login drop-in is in place.
fn machine(
    passwd: String,
    shows: &[&'static str],
    logins: Arc<Mutex<Vec<bool>>>,
) -> impl Fn(&str, &[&str]) -> (i32, String) + use<> {
    let fields: Vec<String> = passwd.split(':').map(str::to_owned).collect();
    let (account, gid) = (fields[0].clone(), fields[3].clone());
    let shows = RefCell::new(shows.iter().copied().collect::<VecDeque<_>>());
    move |program, args| match (program, args) {
        ("getent", ["passwd"]) => (0, passwd.clone()),
        ("getent", ["passwd", who]) if *who == account => (0, passwd.clone()),
        ("getent", ["group", group]) if *group == gid => (0, format!("{account}:x:{gid}:\n")),
        ("getent", ["group", accounts::GAMES_GROUP]) => (
            0,
            format!("{}:x:{GAMES_GID}:{account}\n", accounts::GAMES_GROUP),
        ),
        ("getent", _) => (2, String::new()),
        ("systemctl", ["show", ..]) => {
            let mut shows = shows.borrow_mut();
            let next = if shows.len() > 1 {
                shows.pop_front()
            } else {
                shows.front().copied()
            };
            (0, next.unwrap_or(INACTIVE).to_owned())
        }
        ("systemctl", ["start", _]) => {
            let login = session::dropin_dir(&account).join("login.conf");
            logins.lock().unwrap().push(login.exists());
            (0, String::new())
        }
        ("loginctl", ["show-user", ..]) => (1, String::new()),
        _ => (0, String::new()),
    }
}

/// [`fake::calls`] from `from` on, with the per-run numbers of a `/proc/<pid>/fd/<n>` path
/// replaced.
fn calls_from(from: usize) -> Vec<String> {
    fake::calls()[from..]
        .iter()
        .map(|call| {
            call.split(' ')
                .map(|word| match word.strip_prefix("/proc/") {
                    Some(rest) if rest.contains("/fd/") => "/proc/PID/fd/N",
                    _ => word,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Every file a start leaves for `account` and the manager of user `uid` under `/run`: its path,
/// mode and text, with `state` and the management token masked.
fn run_files(account: &str, uid: u32, state: &Path) -> Vec<(String, u32, String)> {
    let mut paths = vec![session::env_path(account), session::manager_dropin(uid)];
    if let Ok(dir) = std::fs::read_dir(session::dropin_dir(account)) {
        paths.extend(dir.map(|entry| entry.unwrap().path()));
    }
    paths.sort();
    let state = state.display().to_string();
    paths
        .into_iter()
        .filter(|path| path.exists())
        .map(|path| {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
            let text = std::fs::read_to_string(&path)
                .unwrap()
                .replace(&state, "<state>");
            let text = text
                .lines()
                .map(|line| match line.strip_prefix("PUNKTFUNK_MGMT_TOKEN=") {
                    Some(token) if token.len() == 64 => "PUNKTFUNK_MGMT_TOKEN=<token>",
                    _ => line,
                })
                .map(|line| format!("{line}\n"))
                .collect();
            (path.display().to_string(), mode, text)
        })
        .collect()
}

/// Removes what a start of `account` wrote under `/run`.
fn clear_run(account: &str, uid: u32) {
    let _ = std::fs::remove_file(session::env_path(account));
    let _ = std::fs::remove_dir_all(session::dropin_dir(account));
    let manager = session::manager_dropin(uid);
    let _ = std::fs::remove_dir_all(manager.parent().unwrap());
    let _ = std::fs::remove_dir_all(session::runtime_dir(account));
}

/// A box and state directory, and the backend over them.
struct Rig {
    _temp: tempfile::TempDir,
    backend: LinuxBackend,
    state: PathBuf,
    home: PathBuf,
}

fn rig() -> Rig {
    let temp = tempfile::tempdir().unwrap();
    let (box_dir, state, home) = (
        temp.path().join("box"),
        temp.path().join("state"),
        temp.path().join("home"),
    );
    for dir in [&box_dir, &state, &home] {
        std::fs::create_dir_all(dir).unwrap();
    }
    layout::ensure_layout(&state).unwrap();
    std::fs::write(box_dir.join("punktfunk1-paired.json"), "{\"clients\":[]}").unwrap();
    let backend = LinuxBackend::detached(&box_dir, &state);
    Rig {
        _temp: temp,
        backend,
        state,
        home,
    }
}

const MADE_ID: &str = "0123456789abcdef0123456789abcd07";
/// No account or process has this id, and root gives it real folders.
const MADE_UID: u32 = 59_970;
/// A made seat whose delete shares nothing under `/run` with the cold starts of [`MADE_ID`].
const DELETED_ID: &str = "0123456789abcdef0123456789abcd06";
const DELETED_UID: u32 = 59_960;
const OWNER_ID: &str = "0123456789abcdef0123456789abcd08";
const OWNER_UID: u32 = 1700;
const PERSON_ID: &str = "0123456789abcdef0123456789abcd09";
const PERSON_UID: u32 = 1800;

const SHOW: &str = "systemctl show punktfunk-seat@ACCOUNT.service -p ActiveState -p SubState \
     -p Result -p ExecMainCode -p ExecMainStatus";

/// `lines`, each with `ACCOUNT` and `UID` filled in.
fn expect(lines: &[&str], account: &str, uid: u32) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.replace("ACCOUNT", account)
                .replace("UID", &uid.to_string())
        })
        .collect()
}

/// A made seat's account, under `state` as `useradd` made it.
fn made_passwd(state: &Path) -> String {
    made_passwd_of(state, "pf-seat-7", MADE_ID, MADE_UID)
}

/// `account` as `useradd` made it for the seat `id`, its home under `state`.
fn made_passwd_of(state: &Path, account: &str, id: &str, uid: u32) -> String {
    let id = SeatId::parse(id).unwrap();
    let home = layout::seat_dir(state, &id).join("home");
    std::fs::create_dir_all(&home).unwrap();
    format!(
        "{account}:x:{uid}:{uid}:{}:{}:/bin/sh\n",
        accounts::marker(&id),
        home.display()
    )
}

/// `answer`, with `who` also holding a session on `seat0`: the physical login that wins over a
/// row on their account.
fn with_a_login_at_the_desk(
    answer: impl Fn(&str, &[&str]) -> (i32, String) + 'static,
    who: &'static str,
) -> impl Fn(&str, &[&str]) -> (i32, String) {
    move |program, args| match (program, args) {
        ("loginctl", ["list-sessions", ..]) => (0, format!("7 1 {who} seat0 tty2\n")),
        ("loginctl", ["show-session", ..]) => (
            0,
            format!("Id=7\nName={who}\nSeat=seat0\nClass=user\nRemote=no\nState=active\n"),
        ),
        _ => answer(program, args),
    }
}

/// The proxy hook, logging each call into [`fake`]'s list as `hook <account> <uid or none>`, so a
/// test sees where it runs among the programs.
fn logged_hook() -> UnitHook {
    Box::new(|account, uid| {
        let uid = uid.map_or("none".to_owned(), |uid| uid.to_string());
        let _ = fake::answer("hook", &[account, &uid]);
    })
}

/// A pad lent to a row.
fn lent_pad() -> crate::model::LentPad {
    crate::model::LentPad {
        key: "usb-28de-1102-s-SC1".into(),
        name: "Pad".into(),
        vendor_id: 0x28de,
        product_id: 0x1102,
        port: None,
    }
}

/// The holder the backend's lend record names for `pad`, `None` without one.
fn lend_holder(state: &Path, pad: &crate::model::LentPad) -> Option<String> {
    lent::record(&state.join(lent::DIR.trim_start_matches('/')), &pad.key).map(|r| r.holder)
}

/// [`calls_from`] `0`, with `state` named `<state>`.
fn calls_under(state: &Path) -> Vec<String> {
    let state = state.display().to_string();
    calls_from(0)
        .into_iter()
        .map(|call| call.replace(&state, "<state>"))
        .collect()
}

/// The made seat's stop: its unit, then its user, whose linger goes first. `/proc` decides when
/// the user is gone, so this runs where Linux does.
#[cfg(target_os = "linux")]
#[test]
fn a_made_seat_s_stop_runs_exactly_these() {
    let rig = rig();
    let seat = row("pf-seat-7", MADE_ID, false, None);
    let passwd = made_passwd(&rig.state);
    let _fake = fake::install(machine(passwd, &[ACTIVE], Arc::default()));
    rig.backend.stop(&seat).unwrap();
    let stop = [
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "getent passwd ACCOUNT",
        "getent passwd ACCOUNT",
        "loginctl disable-linger ACCOUNT",
        "loginctl show-user ACCOUNT",
    ];
    assert_eq!(calls_from(0), expect(&stop, "pf-seat-7", MADE_UID));
}

/// The owner's and a person's stop end their unit and nothing else.
#[test]
fn an_owner_s_or_person_s_stop_runs_exactly_these() {
    let rig = rig();
    let stop = [
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
    ];
    for (seat, uid) in [
        (row("pfowner", OWNER_ID, true, None), OWNER_UID),
        (
            row("pfperson", PERSON_ID, false, Some(PERSON_UID)),
            PERSON_UID,
        ),
    ] {
        let passwd = format!("{0}:x:{uid}:{uid}::{1}:/bin/sh\n", seat.account, "/h");
        let _fake = fake::install(machine(passwd, &[ACTIVE], Arc::default()));
        rig.backend.stop(&seat).unwrap();
        assert_eq!(calls_from(0), expect(&stop, &seat.account, uid));
    }
}

/// A stop asked of one seat ends only that seat's start: another's start in flight goes on to its
/// unit's end, and a stop of its own seat ends it.
#[test]
fn a_stop_ends_only_its_own_seat_s_start() {
    let rig = rig();
    let (other, starting) = (
        row("pfowner", OWNER_ID, true, None),
        row("pfperson", PERSON_ID, false, Some(PERSON_UID)),
    );
    let passwd = owner_passwd("pfowner", &rig.home);
    let _fake = fake::install(machine(passwd, &[FAILED], Arc::default()));
    let asked = rig.backend.stops(&starting.id);
    rig.backend.stop(&other).unwrap();
    let ended = rig.backend.wait_ready(&starting, asked, false).unwrap_err();
    assert_eq!(ended.code, "seat_exited", "the other seat's stop");
    rig.backend.stop(&starting).unwrap();
    let ended = rig.backend.wait_ready(&starting, asked, false).unwrap_err();
    assert_eq!(ended.code, "seat_stopped", "its own seat's stop");
}

/// A person's stop untracks its user before the unit stops, so the pad broker refuses it while
/// the unit ends. The owner's row stays tracked across the stop: it lives with the row.
#[test]
fn a_stop_untracks_a_person_before_its_unit_stops() {
    let rig = rig();
    for (seat, uid, tracked) in [
        (row("pfowner", OWNER_ID, true, None), OWNER_UID, true),
        (
            row("pfperson", PERSON_ID, false, Some(PERSON_UID)),
            PERSON_UID,
            false,
        ),
    ] {
        let passwd = format!("{0}:x:{uid}:{uid}::{1}:/bin/sh\n", seat.account, "/h");
        let found = accounts::parse_passwd(&passwd).unwrap();
        rig.backend.track(&seat.id, Tracked::of(&seat, &found));
        let at_stop = Arc::new(Mutex::new(Vec::new()));
        let (inner, id, log) = (
            Arc::clone(&rig.backend.inner),
            seat.id.clone(),
            Arc::clone(&at_stop),
        );
        let machine = machine(passwd, &[ACTIVE], Arc::default());
        let _fake = fake::install(move |program, args| {
            if let ("systemctl", ["stop", _]) = (program, args) {
                log.lock()
                    .unwrap()
                    .push(inner.running.lock().unwrap().contains_key(&id));
            }
            machine(program, args)
        });
        rig.backend.stop(&seat).unwrap();
        assert_eq!(*at_stop.lock().unwrap(), [tracked], "{}", seat.account);
        assert_eq!(rig.backend.running_account(uid).is_some(), tracked);
    }
}

/// A person's start with their unit already up writes nothing under `/run` and keeps its token:
/// it refreshes the row's copies, then waits on the unit, which here ends. Last it asks whether
/// the person sat down at the machine meanwhile.
#[test]
fn a_person_s_warm_start_runs_exactly_these() {
    let rig = rig();
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let seat = row("pfperson", PERSON_ID, false, Some(uid));
    let passwd = format!("pfperson:x:{uid}:{gid}::{}:/bin/sh\n", rig.home.display());
    let _fake = fake::install(machine(passwd, &[ACTIVE, FAILED], Arc::default()));
    let ended = rig.backend.start(&seat).unwrap_err();
    assert_eq!(ended.code, "seat_exited");
    let warm = [
        "getent passwd ACCOUNT",
        "loginctl list-sessions --no-legend",
        SHOW,
        SHOW,
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        SESSIONS,
    ];
    assert_eq!(calls_from(0), expect(&warm, "pfperson", uid));
    assert_eq!(rig.backend.running_account(uid), None);
}

/// `pfperson`'s machine: their unit runs until a `systemctl stop`, and they sit at the machine
/// once `seated` is set.
fn sitting_down(
    passwd: String,
    up: Arc<AtomicBool>,
    seated: Arc<AtomicBool>,
) -> impl Fn(&str, &[&str]) -> (i32, String) {
    let at_desk = with_a_login_at_the_desk(machine(passwd, &[ACTIVE], Arc::default()), "pfperson");
    move |program, args| match (program, args) {
        ("systemctl", ["show", ..]) if !up.load(Ordering::SeqCst) => (0, INACTIVE.into()),
        ("systemctl", ["stop", _]) => {
            up.store(false, Ordering::SeqCst);
            (0, String::new())
        }
        ("loginctl", _) if !seated.load(Ordering::SeqCst) => (0, String::new()),
        _ => at_desk(program, args),
    }
}

/// The person sits down at the machine while their start waits. The sync thread's yield, on a
/// thread of its own, ends the unit; the start then answers as a start at the desk does.
#[test]
fn a_person_who_sits_down_during_their_start_is_refused_at_the_desk() {
    let rig = rig();
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    let seat = row("pfperson", PERSON_ID, false, Some(uid));
    let passwd = format!("pfperson:x:{uid}:{gid}::{}:/bin/sh\n", rig.home.display());
    let (up, seated) = (Arc::new(AtomicBool::new(true)), Arc::default());
    let desk = {
        let (inner, seat) = (Arc::clone(&rig.backend.inner), seat.clone());
        let machine = (passwd.clone(), Arc::clone(&up), Arc::clone(&seated));
        std::thread::spawn(move || {
            let seated = Arc::clone(&machine.2);
            let _fake = fake::install(sitting_down(machine.0, machine.1, machine.2));
            let deadline = Instant::now() + Duration::from_secs(10);
            while !inner.running.lock().unwrap().contains_key(&seat.id) {
                assert!(Instant::now() < deadline, "the start never tracked the row");
                std::thread::sleep(Duration::from_millis(10));
            }
            seated.store(true, Ordering::SeqCst);
            yield_to_the_desk(&inner, &seat);
            fake::calls()
        })
    };
    let _fake = fake::install(sitting_down(passwd, up, seated));
    let ended = rig.backend.start(&seat).unwrap_err();
    let yielded = desk.join().unwrap();
    let stop = "systemctl stop punktfunk-seat@pfperson.service".to_owned();
    assert!(yielded.contains(&stop), "{yielded:?}");
    assert_eq!(ended.code, "account_at_desk", "{ended}");
    assert_eq!(rig.backend.running_account(uid), None);
}

/// A made seat's cold start, then its warm one: the programs each runs and the files under `/run`
/// the cold one leaves.
#[test]
#[ignore = "needs root: hands folders to the seat's user and writes under /run"]
fn a_made_seat_s_cold_and_warm_start_run_exactly_these() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pf-seat-7", MADE_UID);
    let rig = rig();
    let seat = row("pf-seat-7", MADE_ID, false, None);
    let passwd = made_passwd(&rig.state);
    let shows = [INACTIVE, FAILED, FAILED, ACTIVE, FAILED, FAILED];
    let _fake = fake::install(machine(passwd, &shows, Arc::default()));

    assert_eq!(rig.backend.start(&seat).unwrap_err().code, "seat_exited");
    let cold = calls_from(0);
    let files = run_files("pf-seat-7", MADE_UID, &rig.state);
    assert_eq!(rig.backend.start(&seat).unwrap_err().code, "seat_exited");
    let warm = calls_from(cold.len());
    clear_run("pf-seat-7", MADE_UID);

    let prepare = [
        "getent passwd ACCOUNT",
        "getent group UID",
        "getent group punktfunk-games",
        "getent group render",
        "getent group punktfunk-games",
        "getent group punktfunk-games",
        "setfacl -m g:punktfunk-games:rwX,d:g:punktfunk-games:rwX /proc/PID/fd/N /proc/PID/fd/N \
         /proc/PID/fd/N /proc/PID/fd/N /proc/PID/fd/N /proc/PID/fd/N",
    ];
    let ended = [
        SHOW,
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "getent passwd ACCOUNT",
        "getent passwd ACCOUNT",
        "loginctl disable-linger ACCOUNT",
        "loginctl show-user ACCOUNT",
    ];
    let up = [
        SHOW,
        "getent passwd punktfunk",
        "getent passwd",
        "getent group punktfunk-door",
        "systemctl daemon-reload",
        "systemctl stop user@UID.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "systemctl start punktfunk-seat@ACCOUNT.service",
    ];
    let expected_cold: Vec<&str> = [&prepare[..], &up, &ended].concat();
    assert_eq!(cold, expect(&expected_cold, "pf-seat-7", MADE_UID));
    let expected_warm: Vec<&str> = [&prepare[..], &[SHOW], &ended].concat();
    assert_eq!(warm, expect(&expected_warm, "pf-seat-7", MADE_UID));

    let id = MADE_ID;
    let seat_dir = format!("<state>/seats/{id}");
    let binds = format!(
        "[Service]\n\
         TemporaryFileSystem=/run/media:ro\n\
         BindPaths=<state>/games:/run/media/punktfunk/games\n\
         BindPaths={seat_dir}/compatdata:<state>/games/steamapps/compatdata\n\
         BindPaths={seat_dir}/shadercache:<state>/games/steamapps/shadercache\n\
         BindPaths={seat_dir}/downloading:<state>/games/steamapps/downloading\n\
         BindPaths={seat_dir}/compatdata:/run/media/punktfunk/games/steamapps/compatdata\n\
         BindPaths={seat_dir}/shadercache:/run/media/punktfunk/games/steamapps/shadercache\n\
         BindPaths={seat_dir}/downloading:/run/media/punktfunk/games/steamapps/downloading\n\
         BindPaths=-/dev/punktfunk/uinput-pf-seat-7:/dev/uinput\n"
    );
    let manager = binds.replacen("[Service]\n", "", 1);
    let env = format!(
        "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
         PUNKTFUNK_SEAT_ID=\"{id}\"\n\
         PUNKTFUNK_CONFIG_DIR=\"{seat_dir}/home/.config/punktfunk\"\n\
         PUNKTFUNK_TRUST_DIR=\"<state>/trust/{id}\"\n\
         PUNKTFUNK_PAIRING=\"refused\"\n\
         PUNKTFUNK_LIBRARY_DIR=\"<state>/trust/{id}\"\n\
         PUNKTFUNK_NATIVE_PORT=\"9779\"\n\
         PUNKTFUNK_MGMT_BIND=\"127.0.0.1:48123\"\n\
         PUNKTFUNK_HOST_NAME=\"Desk\"\n\
         PUNKTFUNK_GAMESTREAM=\"0\"\n\
         PUNKTFUNK_MDNS=\"0\"\n\
         PUNKTFUNK_WEBTRANSPORT=\"0\"\n\
         PUNKTFUNK_MGMT_TOKEN=<token>\n"
    );
    assert_eq!(
        files,
        [
            ("/run/punktfunk/seats/pf-seat-7.env".into(), 0o600, env),
            (
                "/run/systemd/system/punktfunk-seat@pf-seat-7.service.d/binds.conf".into(),
                0o644,
                binds
            ),
            (
                format!("/run/systemd/system/user@{MADE_UID}.service.d/punktfunk-seat.conf"),
                0o644,
                format!("[Service]\n{manager}")
            ),
        ]
    );
}

/// The owner's cold start, then its warm one, with nobody at the machine.
#[test]
#[ignore = "needs root: hands the trust copy to the owner and writes under /run"]
fn the_owner_s_cold_and_warm_start_run_exactly_these() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pfowner", OWNER_UID);
    let rig = rig();
    std::fs::write(rig.state.join("owner"), "pfowner\n").unwrap();
    let seat = row("pfowner", OWNER_ID, true, None);
    let passwd = format!(
        "pfowner:x:{OWNER_UID}:{OWNER_UID}:Owner:{}:/bin/bash\n",
        rig.home.display()
    );
    let shows = [INACTIVE, FAILED, FAILED, ACTIVE, FAILED, FAILED];
    let _fake = fake::install(machine(passwd, &shows, Arc::default()));

    assert_eq!(rig.backend.start(&seat).unwrap_err().code, "seat_exited");
    let cold = calls_from(0);
    let files = run_files("pfowner", OWNER_UID, &rig.state);
    let mut acl = [0; 64];
    let read = rustix::fs::lgetxattr(session::env_path("pfowner"), acl::ACL_ACCESS, &mut acl);
    let acl = acl[..read.unwrap_or(0)].to_vec();
    assert_eq!(rig.backend.start(&seat).unwrap_err().code, "seat_exited");
    let warm = calls_from(cold.len());
    clear_run("pfowner", OWNER_UID);

    let prepare = [
        "getent passwd ACCOUNT",
        "getent group punktfunk-door",
        "loginctl list-sessions --no-legend",
    ];
    let first_files = ["getent passwd punktfunk", "systemctl daemon-reload"];
    let ended = [
        SHOW,
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        SESSIONS,
    ];
    let up = [
        SHOW,
        "getent passwd punktfunk",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "systemctl start punktfunk-seat@ACCOUNT.service",
    ];
    let expected_cold: Vec<&str> =
        [&prepare[..2], &first_files, &prepare[2..], &up, &ended].concat();
    assert_eq!(cold, expect(&expected_cold, "pfowner", OWNER_UID));
    let expected_warm: Vec<&str> = [&prepare[..], &[SHOW], &ended].concat();
    assert_eq!(warm, expect(&expected_warm, "pfowner", OWNER_UID));

    let id = OWNER_ID;
    let env = format!(
        "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
         PUNKTFUNK_SEAT_ID=\"{id}\"\n\
         PUNKTFUNK_SEAT_OWNER=\"1\"\n\
         PUNKTFUNK_TRUST_DIR=\"<state>/trust/{id}\"\n\
         PUNKTFUNK_PAIRING=\"refused\"\n\
         PUNKTFUNK_NATIVE_PORT=\"9779\"\n\
         PUNKTFUNK_MGMT_BIND=\"127.0.0.1:48123\"\n\
         PUNKTFUNK_MDNS=\"0\"\n\
         PUNKTFUNK_WEBTRANSPORT=\"0\"\n\
         PUNKTFUNK_MGMT_TOKEN=<token>\n"
    );
    assert_eq!(
        files,
        [
            ("/run/punktfunk/seats/pfowner.env".into(), 0o640, env),
            (
                "/run/systemd/system/punktfunk-seat@pfowner.service.d/class.conf".into(),
                0o644,
                "[Service]\nEnvironment=XDG_SESSION_CLASS=background\n".into()
            ),
        ]
    );
    use acl::{ACL_GROUP_OBJ, ACL_MASK, ACL_NO_ID, ACL_OTHER, ACL_USER, ACL_USER_OBJ};
    let reader = acl::acl_bytes(&[
        (ACL_USER_OBJ, 6, ACL_NO_ID),
        (ACL_USER, 4, OWNER_UID),
        (ACL_GROUP_OBJ, 0, ACL_NO_ID),
        (ACL_MASK, 4, ACL_NO_ID),
        (ACL_OTHER, 0, ACL_NO_ID),
    ]);
    assert_eq!(
        acl, reader,
        "the owner reads the file and only root writes it"
    );
}

/// A person's cold start with a key proof, which opens their session through systemd's own login:
/// no login drop-in, no password kept for a helper.
#[test]
#[ignore = "needs root: hands the row's folders to the person and writes under /run"]
fn a_person_s_cold_start_runs_exactly_these() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pfperson", PERSON_UID);
    let rig = rig();
    let seat = row("pfperson", PERSON_ID, false, Some(PERSON_UID));
    let passwd = format!(
        "pfperson:x:{PERSON_UID}:{PERSON_UID}:Per:{}:/bin/bash\n",
        rig.home.display()
    );
    let logins = Arc::new(Mutex::new(Vec::new()));
    let shows = [INACTIVE, FAILED, FAILED];
    let _fake = fake::install(machine(passwd, &shows, Arc::clone(&logins)));

    assert_eq!(rig.backend.start(&seat).unwrap_err().code, "seat_exited");
    let cold = calls_from(0);
    let files = run_files("pfperson", PERSON_UID, &rig.state);
    clear_run("pfperson", PERSON_UID);

    let expected = [
        "getent passwd ACCOUNT",
        "loginctl list-sessions --no-legend",
        SHOW,
        "getent passwd punktfunk",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "systemctl start punktfunk-seat@ACCOUNT.service",
        SHOW,
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        SESSIONS,
    ];
    assert_eq!(cold, expect(&expected, "pfperson", PERSON_UID));
    assert_eq!(*logins.lock().unwrap(), [false]);
    let id = PERSON_ID;
    let env = format!(
        "PUNKTFUNK_SEAT_SESSION=\"1\"\n\
         PUNKTFUNK_SEAT_ID=\"{id}\"\n\
         PUNKTFUNK_SEAT_OWN_ACCOUNT=\"1\"\n\
         PUNKTFUNK_CONFIG_DIR=\"<state>/seats/{id}/config\"\n\
         PUNKTFUNK_TRUST_DIR=\"<state>/trust/{id}\"\n\
         PUNKTFUNK_PAIRING=\"refused\"\n\
         PUNKTFUNK_LIBRARY_DIR=\"<state>/trust/{id}\"\n\
         PUNKTFUNK_NATIVE_PORT=\"9779\"\n\
         PUNKTFUNK_MGMT_BIND=\"127.0.0.1:48123\"\n\
         PUNKTFUNK_HOST_NAME=\"Desk\"\n\
         PUNKTFUNK_GAMESTREAM=\"0\"\n\
         PUNKTFUNK_MDNS=\"0\"\n\
         PUNKTFUNK_WEBTRANSPORT=\"0\"\n\
         PUNKTFUNK_MGMT_TOKEN=<token>\n"
    );
    assert_eq!(
        files,
        [("/run/punktfunk/seats/pfperson.env".into(), 0o600, env)]
    );
}

/// A person's cold start with their password goes through the login helper; a helper that got no
/// password falls back to the plain start, whose unit runs without the login drop-in.
#[test]
#[ignore = "needs root: hands the row's folders to the person and writes under /run"]
fn a_person_s_password_start_runs_exactly_these() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pfperson", PERSON_UID);
    let rig = rig();
    rig.backend.login_socket(login_password::Label::Off);
    let seat = row("pfperson", PERSON_ID, false, Some(PERSON_UID));
    let passwd = format!(
        "pfperson:x:{PERSON_UID}:{PERSON_UID}:Per:{}:/bin/bash\n",
        rig.home.display()
    );
    let logins = Arc::new(Mutex::new(Vec::new()));
    let shows = [
        INACTIVE,
        NO_PASSWORD,
        NO_PASSWORD,
        NO_PASSWORD,
        FAILED,
        FAILED,
    ];
    let _fake = fake::install(machine(passwd, &shows, Arc::clone(&logins)));

    let password = Secret::new("hunter2".into());
    let ended = rig.backend.start_with_password(&seat, &password);
    let left = session::dropin_dir("pfperson").join("login.conf").exists();
    clear_run("pfperson", PERSON_UID);
    assert_eq!(ended.unwrap_err().code, "seat_exited");

    let expected = [
        "getent passwd ACCOUNT",
        "loginctl list-sessions --no-legend",
        SHOW,
        "getent passwd punktfunk",
        "systemctl daemon-reload",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "systemctl start punktfunk-seat@ACCOUNT.service",
        SHOW,
        SHOW,
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "systemctl daemon-reload",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        "systemctl start punktfunk-seat@ACCOUNT.service",
        SHOW,
        SHOW,
        "systemctl stop punktfunk-seat@ACCOUNT.service",
        "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
        SESSIONS,
    ];
    assert_eq!(calls_from(0), expect(&expected, "pfperson", PERSON_UID));
    assert_eq!(*logins.lock().unwrap(), [true, false]);
    assert!(!left, "the drop-in went with the helper's start");
    assert!(!rig.backend.inner.login_passwords.waits("pfperson"));
}

/// The folders a made seat's provision leaves in its seat directory and in `saves`, each with its
/// mode and owner; the home is apart.
fn seat_folders(state: &Path, id: &str) -> Vec<(String, u32, u32)> {
    use std::os::unix::fs::MetadataExt as _;
    let dir = layout::seat_dir(state, &SeatId::parse(id).unwrap());
    let mut out = Vec::new();
    for sub in ["", "saves"] {
        for entry in std::fs::read_dir(dir.join(sub)).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let name = path.strip_prefix(&dir).unwrap().display().to_string();
            if meta.is_dir() && name != "home" {
                out.push((name, meta.mode() & 0o7777, meta.uid()));
            }
        }
    }
    out.sort();
    out
}

/// A made seat's provision when the account doesn't exist yet. First the games group and the
/// games folder. Then the account, with a group of its own and those of its groups that exist.
/// Then the account's home is closed, and the seat's folders and Steam files are handed to the
/// account. Provisioning a second time finds the account and makes none.
#[test]
#[ignore = "needs root: hands the seat's folders to its user"]
fn a_made_seat_s_provision_runs_exactly_these() {
    use std::os::unix::fs::MetadataExt as _;
    let rig = rig();
    let seat = row("pf-seat-4", MADE_ID, false, None);
    let made = Cell::new(false);
    let passwd = made_passwd_of(&rig.state, "pf-seat-4", MADE_ID, MADE_UID);
    let base = machine(passwd, &[INACTIVE], Arc::default());
    let _fake = fake::install(move |program, args| match (program, args) {
        ("useradd", _) => {
            made.set(true);
            (0, String::new())
        }
        ("getent", ["passwd", "pf-seat-4"]) if !made.get() => (2, String::new()),
        _ => base(program, args),
    });

    // Only the names the door gives a seat are made, and the check comes before any program.
    let stranger = Seat {
        account: "pf-seat-7".into(),
        ..seat.clone()
    };
    assert_eq!(
        rig.backend.provision(&stranger).unwrap_err().code,
        "account_invalid"
    );
    assert!(fake::calls().is_empty());

    rig.backend.provision(&seat).unwrap();
    let first = calls_under(&rig.state);
    let folders = seat_folders(&rig.state, MADE_ID);
    let home = layout::seat_dir(&rig.state, &seat.id).join("home");
    assert_eq!(std::fs::metadata(home).unwrap().mode() & 0o7777, 0o700);
    rig.backend.provision(&seat).unwrap();
    let again = calls_under(&rig.state)[first.len()..].to_vec();

    let games = [
        "getent group punktfunk-games",
        "setfacl -m g:punktfunk-games:rwX,d:g:punktfunk-games:rwX /proc/PID/fd/N /proc/PID/fd/N \
         /proc/PID/fd/N /proc/PID/fd/N /proc/PID/fd/N /proc/PID/fd/N",
    ];
    let lookup = ["getent passwd ACCOUNT"];
    let create = [
        "getent group render",
        "getent group punktfunk-games",
        "getent group ACCOUNT",
        "useradd -r -m -d <state>/seats/SEATID/home -c punktfunk-seat=SEATID -G punktfunk-games \
         -U ACCOUNT",
        "getent passwd ACCOUNT",
    ];
    let steam = [
        "getent passwd",
        "chown -R UID:UID <state>/seats/SEATID/home/.local",
    ];
    let ours = |lines: &[&str]| {
        expect(lines, "pf-seat-4", MADE_UID)
            .into_iter()
            .map(|line| line.replace("SEATID", MADE_ID))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        first,
        ours(&[&games[..], &lookup, &create, &steam].concat())
    );
    assert_eq!(again, ours(&[&games[..], &lookup, &steam].concat()));

    let private = |name: &str| (name.to_owned(), 0o700, MADE_UID);
    assert_eq!(
        folders,
        [
            private("compatdata"),
            private("downloading"),
            ("saves".into(), 0o711, 0),
            ("saves/in".into(), 0o770, MADE_UID),
            ("saves/out".into(), 0o770, MADE_UID),
            private("shadercache"),
        ]
    );
}

/// A made seat never yields to its user at the machine, though they have a session there: its
/// account is the supervisor's, nobody else's. The sync thread is not even offered the row,
/// since only a seat on someone's own account is tracked with an account to yield to.
#[test]
fn a_made_seat_never_yields_to_the_desk() {
    let rig = rig();
    let seat = row("pf-seat-7", MADE_ID, false, None);
    let passwd = made_passwd(&rig.state);
    let found = accounts::parse_passwd(&passwd).unwrap();
    let machine = machine(passwd, &[ACTIVE], Arc::default());
    let _fake = fake::install(with_a_login_at_the_desk(machine, "pf-seat-7"));

    yield_to_the_desk(&rig.backend.inner, &seat);
    assert_eq!(calls_from(0), expect(&[SHOW], "pf-seat-7", MADE_UID));
    assert!(owner_at_desk("pf-seat-7"), "the session is there");
    assert!(Tracked::of(&seat, &found).own_account.is_none());
}

/// What deleting a made seat's row asked of the machine.
struct Deleted {
    rig: Rig,
    seat: Seat,
    calls: Vec<String>,
    /// Whether the broker still admitted the seat's user each time its unit was stopped.
    tracked_at_stop: Vec<bool>,
}

/// Deletes the made seat `account`: its unit up, its user tracked, its pad live and its proxy
/// hook set. `before` sets up what else stands when it goes.
fn delete_a_made_seat(
    account: &str,
    id: &str,
    uid: u32,
    before: impl FnOnce(&Rig, &Seat),
) -> Deleted {
    let mut rig = rig();
    rig.backend = rig.backend.on_unit(logged_hook());
    let seat = Seat {
        lent: vec![lent_pad()],
        ..row(account, id, false, None)
    };
    let passwd = made_passwd_of(&rig.state, account, id, uid);
    let found = accounts::parse_passwd(&passwd).unwrap();
    for dir in [
        layout::trust_dir(&rig.state, &seat.id),
        layout::host_dir(&rig.state, &seat.id),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    before(&rig, &seat);
    rig.backend.track(&seat.id, Tracked::of(&seat, &found));
    rig.backend.inner.lent.set_live(&seat, true, true);
    assert_eq!(
        lend_holder(&rig.state, &lent_pad()).as_deref(),
        Some(account)
    );

    let tracked_at_stop = Arc::new(Mutex::new(Vec::new()));
    let (inner, id, log) = (
        Arc::clone(&rig.backend.inner),
        seat.id.clone(),
        Arc::clone(&tracked_at_stop),
    );
    let machine = machine(passwd, &[ACTIVE], Arc::default());
    let _fake = fake::install(move |program, args| {
        if let ("systemctl", ["stop", _]) = (program, args) {
            let tracked = inner.running.lock().unwrap().contains_key(&id);
            log.lock().unwrap().push(tracked);
        }
        machine(program, args)
    });
    rig.backend.remove(&seat).unwrap();
    let calls = calls_from(0);
    let tracked_at_stop = tracked_at_stop.lock().unwrap().clone();
    Deleted {
        rig,
        seat,
        calls,
        tracked_at_stop,
    }
}

/// What a made seat's delete asks of the machine before and after its account goes.
const DELETE: [&str; 17] = [
    "getent passwd ACCOUNT",
    SHOW,
    "systemctl stop punktfunk-seat@ACCOUNT.service",
    "systemctl reset-failed punktfunk-seat@ACCOUNT.service",
    "getent passwd ACCOUNT",
    "getent passwd ACCOUNT",
    "loginctl disable-linger ACCOUNT",
    "loginctl show-user ACCOUNT",
    "hook ACCOUNT none",
    "getent passwd",
    "getent passwd ACCOUNT",
    "getent passwd ACCOUNT",
    "getent passwd ACCOUNT",
    "loginctl disable-linger ACCOUNT",
    "loginctl show-user ACCOUNT",
    "userdel -r ACCOUNT",
    "getent passwd ACCOUNT",
];

/// A made seat's delete, in order:
///
/// 1. the foreign-user check
/// 2. the seat is untracked, and only then its unit stops
/// 3. the seat's user is ended, and the proxy hook lets go
/// 4. the account is removed with its home
/// 5. the seat's directories are removed
/// 6. the seat's lent pad belongs to the desk again
///
/// `/proc` decides when the user is gone, so this test runs only on Linux.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "reads /proc, which only Linux has")]
fn a_made_seat_s_delete_runs_exactly_these() {
    let deleted = delete_a_made_seat("pf-seat-6", DELETED_ID, DELETED_UID, |_, _| {});
    let (rig, seat) = (&deleted.rig, &deleted.seat);
    assert_eq!(deleted.calls, expect(&DELETE, "pf-seat-6", DELETED_UID));
    assert_eq!(deleted.tracked_at_stop, [false]);
    assert_eq!(rig.backend.running_account(DELETED_UID), None);
    for gone in [
        layout::seat_dir(&rig.state, &seat.id),
        layout::trust_dir(&rig.state, &seat.id),
        layout::host_dir(&rig.state, &seat.id),
        rig.state.join(lent::DIR.trim_start_matches('/')),
    ] {
        assert!(!gone.exists(), "{}", gone.display());
    }
}

/// The same delete with the files its last start left under `/run`: the environment file, the
/// seat's drop-ins and its user manager's go, and systemd reloads once, last.
#[test]
#[ignore = "needs root and Linux: writes the row's files under /run, reads /proc"]
fn a_made_seat_s_delete_takes_its_files_under_run_and_reloads() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pf-seat-7", MADE_UID);
    let deleted = delete_a_made_seat("pf-seat-7", MADE_ID, MADE_UID, |_, _| {
        let dropin = session::dropin_dir("pf-seat-7").join("binds.conf");
        for file in [
            session::env_path("pf-seat-7"),
            dropin,
            session::manager_dropin(MADE_UID),
        ] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "x\n").unwrap();
        }
    });
    let left = [
        session::env_path("pf-seat-7"),
        session::dropin_dir("pf-seat-7"),
        session::manager_dropin(MADE_UID).parent().unwrap().into(),
    ];
    let left: Vec<_> = left.iter().filter(|path| path.exists()).collect();
    clear_run("pf-seat-7", MADE_UID);
    let reload = [&DELETE[..], &["systemctl daemon-reload"]].concat();
    assert_eq!(deleted.calls, expect(&reload, "pf-seat-7", MADE_UID));
    assert!(left.is_empty(), "{left:?}");
}

/// A made seat's delete on a box where the owner's row is prepared. The owner's Steam is known
/// without a look at every account. The seat's folders on the drives of the owner's libraries
/// are removed first, and as the owner.
#[test]
#[ignore = "needs root and Linux: hands the owner's Steam to the owner, reads /proc"]
fn a_made_seat_s_delete_forgets_its_folders_on_the_owner_s_library_drives() {
    let deleted = delete_a_made_seat("pf-seat-6", DELETED_ID, DELETED_UID, |rig, seat| {
        let owner = accounts::parse_passwd(&owner_passwd("pfowner", &rig.home)).unwrap();
        let steam = rig.home.join(".local/share/Steam");
        let anchor = steam.join(crate::steam::SEAT_ROOTS).join(seat.id.as_str());
        std::fs::create_dir_all(&anchor).unwrap();
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        let list = "\"libraryfolders\"\n{\n\t\"0\"\n\t{\n\t\t\"path\"\t\t\"/elsewhere\"\n\t}\n}\n";
        std::fs::write(steam.join("steamapps/libraryfolders.vdf"), list).unwrap();
        std::os::unix::fs::chown(&steam, Some(OWNER_UID), Some(OWNER_UID)).unwrap();
        *rig.backend.inner.owner.lock().unwrap() = Some(owner);
    });
    let home = deleted.rig.home.display().to_string();
    let calls: Vec<_> = (deleted.calls.iter())
        .map(|call| call.replace(&home, "<home>"))
        .collect();

    let owner = format!("setpriv --reuid {OWNER_UID} --regid {OWNER_UID} --clear-groups");
    let seats = "<home>/.local/share/Steam/.punktfunk-seats";
    let forget = [
        format!("{owner} rm -rf -- {seats}/{DELETED_ID}"),
        format!("{owner} rmdir -- {seats}"),
    ];
    let known: Vec<_> = DELETE[..9] // up to the hook, then no look at every account
        .iter()
        .map(|line| (*line).to_owned())
        .chain(forget)
        .chain(DELETE[10..].iter().map(|line| (*line).to_owned()))
        .collect();
    let known: Vec<&str> = known.iter().map(String::as_str).collect();
    assert_eq!(calls, expect(&known, "pf-seat-6", DELETED_UID));
}

/// The owner's (or another ordinary user's) account, at `home`.
fn owner_passwd(account: &str, home: &Path) -> String {
    format!(
        "{account}:x:{OWNER_UID}:{OWNER_UID}:Owner:{}:/bin/bash\n",
        home.display()
    )
}

const STOP: &str = "systemctl stop punktfunk-seat@ACCOUNT.service";
const RESET: &str = "systemctl reset-failed punktfunk-seat@ACCOUNT.service";
/// `loginctl`'s two questions, with one session listed.
const SESSIONS: &str = "loginctl list-sessions --no-legend";
const SHOW_SESSION: &str =
    "loginctl show-session -p Id -p Name -p Seat -p Class -p Remote -p State 7";
/// The owner's own host, reached through their user manager: `setpriv` becomes them without a PAM
/// session.
const RESTART_HOST: &str = "setpriv --reuid UID --regid UID --init-groups env \
     XDG_RUNTIME_DIR=/run/user/UID systemctl --user try-restart punktfunk-host.service";
const KDE_ENABLED: &str = "setpriv --reuid UID --regid UID --init-groups env \
     XDG_RUNTIME_DIR=/run/user/UID systemctl --user is-enabled --quiet \
     punktfunk-kde-session.service";
const START_HOST: &str = "setpriv --reuid UID --regid UID --init-groups env \
     XDG_RUNTIME_DIR=/run/user/UID systemctl --user start punktfunk-host.service \
     punktfunk-kde-session.service";

/// Seats outlive the supervisor. So when the supervisor starts, it may find the owner's row with
/// the row's unit already up. It writes the row's files again and tracks the row for as long as
/// the row exists; a stop leaves it tracked. The owner's own host restarts onto files that
/// changed. A made seat's row and a person's row in the same ledger get none of this.
#[test]
#[ignore = "needs root: hands the trust copy to the owner and writes under /run"]
fn the_owner_row_is_tracked_after_a_supervisor_restart() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pfowner", OWNER_UID);
    let rig = rig();
    std::fs::write(rig.state.join("owner"), "pfowner\n").unwrap();
    let seat = row("pfowner", OWNER_ID, true, None);
    let mut ledger = Ledger::new();
    ledger.seats = vec![
        row("pf-seat-7", MADE_ID, false, None),
        seat.clone(),
        row("pfperson", PERSON_ID, false, Some(PERSON_UID)),
    ];
    let passwd = owner_passwd("pfowner", &rig.home);
    let _fake = fake::install(machine(passwd, &[ACTIVE], Arc::default()));
    assert_eq!(rig.backend.running_account(OWNER_UID), None);

    rig.backend.prepare_owners(&ledger);
    let cold = calls_from(0);
    rig.backend.prepare_owners(&ledger);
    let warm = calls_from(cold.len());
    let tracked = rig
        .backend
        .inner
        .running
        .lock()
        .unwrap()
        .get(&seat.id)
        .cloned();
    let others = [MADE_UID, PERSON_UID].map(|uid| rig.backend.running_account(uid));
    rig.backend.stop(&seat).unwrap();
    let after_stop = rig.backend.running_account(OWNER_UID);
    clear_run("pfowner", OWNER_UID);

    let prepare = ["getent passwd ACCOUNT", "getent group punktfunk-door"];
    let first_files = ["getent passwd punktfunk", "systemctl daemon-reload"];
    let restart = ["getent passwd ACCOUNT", RESTART_HOST];
    let expected_cold = [&prepare[..], &first_files, &restart].concat();
    assert_eq!(cold, expect(&expected_cold, "pfowner", OWNER_UID));
    assert_eq!(warm, expect(&prepare, "pfowner", OWNER_UID));
    let tracked = tracked.expect("the owner's row is tracked");
    assert_eq!((tracked.kind, tracked.uid), (SeatAccount::Owner, OWNER_UID));
    assert!(
        tracked.own_account.is_some(),
        "the sync thread can yield it"
    );
    assert_eq!(others, [None, None]);
    assert_eq!(after_stop.as_deref(), Some("pfowner"));
}

/// An owner row for an account `door-helper` no longer records is skipped at a supervisor start:
/// it is looked up, writes nothing and is not tracked, and the backend has no owner.
#[test]
fn an_owner_row_the_door_no_longer_records_is_not_tracked_after_a_restart() {
    let rig = rig();
    std::fs::write(rig.state.join("owner"), "someone-else\n").unwrap();
    let mut ledger = Ledger::new();
    ledger.seats = vec![row("pfowner", OWNER_ID, true, None)];
    let passwd = owner_passwd("pfowner", &rig.home);
    let _fake = fake::install(machine(passwd, &[INACTIVE], Arc::default()));

    rig.backend.prepare_owners(&ledger);
    let lookup = ["getent passwd ACCOUNT"];
    assert_eq!(calls_from(0), expect(&lookup, "pfowner", OWNER_UID));
    assert_eq!(rig.backend.running_account(OWNER_UID), None);
    assert!(rig.backend.owner().is_none());
}

/// The owner's start with them at the machine: their files are written, the row is tracked and its
/// pad goes live, and their own host starts through their user manager. The headless unit never
/// starts. The wait for the host's port then ends at once, as a stop that landed during it would
/// end it.
#[test]
#[ignore = "needs root: hands the trust copy to the owner and writes under /run"]
fn the_owner_s_start_at_the_desk_starts_its_own_host() {
    let _alone = RUN.lock().unwrap_or_else(|e| e.into_inner());
    clear_run("pfowner", OWNER_UID);
    let rig = rig();
    std::fs::write(rig.state.join("owner"), "pfowner\n").unwrap();
    let seat = Seat {
        lent: vec![lent_pad()],
        ..row("pfowner", OWNER_ID, true, None)
    };
    let passwd = owner_passwd("pfowner", &rig.home);
    let machine = machine(passwd, &[INACTIVE], Arc::default());
    let _fake = fake::install(with_a_login_at_the_desk(machine, "pfowner"));

    let asked = rig.backend.stops(&seat.id);
    rig.backend.ask_stop(&seat.id);
    let ended = rig.backend.start_row(&seat, asked, None).unwrap_err();
    let calls = calls_from(0);
    let written = session::env_path("pfowner").exists();
    clear_run("pfowner", OWNER_UID);

    assert_eq!(ended.code, "seat_stopped");
    let expected = [
        "getent passwd ACCOUNT",
        "getent group punktfunk-door",
        "getent passwd punktfunk",
        "systemctl daemon-reload",
        SESSIONS,
        SHOW_SESSION,
        "getent passwd ACCOUNT",
        KDE_ENABLED,
        START_HOST,
        "getent passwd ACCOUNT",
    ];
    assert_eq!(calls, expect(&expected, "pfowner", OWNER_UID));
    assert!(written, "the owner's files are written at the desk too");
    let account = rig.backend.running_account(OWNER_UID);
    assert_eq!(account.as_deref(), Some("pfowner"));
    let holder = lend_holder(&rig.state, &lent_pad());
    assert_eq!(holder.as_deref(), Some("pfowner"));
}

/// The owner sat down at the machine: the headless unit ends and their own host starts through
/// their user manager. With nobody there the unit is only looked at. The row stays tracked.
#[test]
fn the_owner_yields_to_its_own_host_at_the_desk() {
    let rig = rig();
    let seat = row("pfyield", OWNER_ID, true, None);
    let passwd = owner_passwd("pfyield", &rig.home);
    let found = accounts::parse_passwd(&passwd).unwrap();
    rig.backend.track(&seat.id, Tracked::of(&seat, &found));
    {
        let _fake = fake::install(machine(passwd.clone(), &[ACTIVE], Arc::default()));
        yield_to_the_desk(&rig.backend.inner, &seat);
        let alone = [SHOW, SESSIONS];
        assert_eq!(calls_from(0), expect(&alone, "pfyield", OWNER_UID));
    }

    let machine = machine(passwd, &[ACTIVE], Arc::default());
    let _fake = fake::install(with_a_login_at_the_desk(machine, "pfyield"));
    yield_to_the_desk(&rig.backend.inner, &seat);
    let yielded = [
        SHOW,
        SESSIONS,
        SHOW_SESSION,
        SHOW,
        STOP,
        RESET,
        "getent passwd ACCOUNT",
        KDE_ENABLED,
        START_HOST,
    ];
    assert_eq!(calls_from(0), expect(&yielded, "pfyield", OWNER_UID));
    let account = rig.backend.running_account(OWNER_UID);
    assert_eq!(account.as_deref(), Some("pfyield"));
}

/// The owner row `pfold` released while the backend's owner is `current`.
struct Released {
    rig: Rig,
    seat: Seat,
    calls: Vec<String>,
}

fn release_owner_row(current: Option<&str>) -> Released {
    let rig = rig();
    let seat = Seat {
        lent: vec![lent_pad()],
        ..row("pfold", OWNER_ID, true, None)
    };
    let passwd = owner_passwd("pfold", &rig.home);
    let found = accounts::parse_passwd(&passwd).unwrap();
    let library = layout::box_library_dir(&rig.state);
    std::fs::create_dir_all(library.join("profiles")).unwrap();
    std::fs::write(library.join("profiles/kept"), "x").unwrap();
    std::fs::write(rig.home.join("notes.txt"), "mine").unwrap();
    for dir in [
        layout::seat_dir(&rig.state, &seat.id),
        layout::trust_dir(&rig.state, &seat.id),
        layout::host_dir(&rig.state, &seat.id),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    rig.backend.track(&seat.id, Tracked::of(&seat, &found));
    rig.backend.inner.lent.set_live(&seat, true, true);
    let owner = current.map(|name| accounts::Passwd {
        name: name.into(),
        ..found.clone()
    });
    *rig.backend.inner.owner.lock().unwrap() = owner;

    let _fake = fake::install(machine(passwd, &[ACTIVE], Arc::default()));
    rig.backend.remove(&seat).unwrap();
    let calls = calls_from(0);
    Released { rig, seat, calls }
}

impl Released {
    /// What every owner row's release does: ends its unit and nothing else of the account,
    /// forgets the row and hands its pad back.
    fn assert_the_row_is_gone(&self) {
        assert_eq!(self.calls, expect(&[SHOW, STOP, RESET], "pfold", OWNER_UID));
        assert_eq!(self.rig.backend.running_account(OWNER_UID), None);
        assert!(self.rig.home.join("notes.txt").exists());
        assert_eq!(lend_holder(&self.rig.state, &lent_pad()), None);
        for gone in [
            layout::seat_dir(&self.rig.state, &self.seat.id),
            layout::trust_dir(&self.rig.state, &self.seat.id),
            layout::host_dir(&self.rig.state, &self.seat.id),
        ] {
            assert!(!gone.exists(), "{}", gone.display());
        }
    }

    fn library(&self) -> bool {
        layout::box_library_dir(&self.rig.state)
            .join("profiles/kept")
            .exists()
    }
}

/// The owner's row takes the box's library with it: the door it was for goes too. That holds when
/// the backend's owner is this row's account, and when it has none.
#[test]
fn the_owner_s_release_takes_the_box_library() {
    for current in [Some("pfold"), None] {
        let released = release_owner_row(current);
        released.assert_the_row_is_gone();
        assert!(!released.library(), "{current:?}");
        assert!(released.rig.backend.owner().is_none());
    }
}

/// A row the owner's new row has replaced leaves the library, which is the new one's, and leaves
/// the backend's owner as it is.
#[test]
fn a_replaced_owner_s_release_keeps_the_box_library() {
    let released = release_owner_row(Some("pfnew"));
    released.assert_the_row_is_gone();
    assert!(released.library());
    let owner = released.rig.backend.owner().map(|owner| owner.name);
    assert_eq!(owner.as_deref(), Some("pfnew"));
}
