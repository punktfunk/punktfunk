//! What a start and a stop of each row kind ask of the machine, against [`fake`]: every program
//! in order, and the files a cold start leaves under `/run`. A cold or warm start of a made seat
//! or the owner's row writes under `/run` and hands folders to other users, so those tests need
//! root; a person's warm start and every stop run anywhere.

use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::os::unix::fs::PermissionsExt as _;

const INACTIVE: &str = "ActiveState=inactive\nSubState=dead\nResult=success\n";
const ACTIVE: &str = "ActiveState=active\nSubState=running\nResult=success\n";
const FAILED: &str =
    "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=1\n";
/// The login helper's end with no password handed to it: the start falls back.
const NO_PASSWORD: &str =
    "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\nExecMainStatus=95\n";

const GAMES_GID: u32 = 3_999_999_971;

/// Held by each test that writes a row's real files under `/run`.
static RUN: Mutex<()> = Mutex::new(());

fn row(account: &str, id: &str, owner: bool, adopted_uid: Option<u32>) -> Seat {
    Seat {
        id: SeatId::parse(id).unwrap(),
        name: "Desk".into(),
        account: account.into(),
        display_slot: 12,
        native_port: 9779,
        mgmt_port: 48_123,
        autostart: false,
        runtime: RuntimeStatus::stopped(),
        owner,
        owner_uid: None,
        fingerprint: None,
        adopted_sid: None,
        adopted_uid,
        rdp_granted: false,
        console_deny_kept: false,
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
    shared::ensure_layout(&state).unwrap();
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
const MADE_UID: u32 = 3_999_999_970;
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
    let id = SeatId::parse(MADE_ID).unwrap();
    let home = shared::seat_dir(state, &id).join("home");
    std::fs::create_dir_all(&home).unwrap();
    format!(
        "pf-seat-7:x:{MADE_UID}:{MADE_UID}:{}:{}:/bin/sh\n",
        accounts::marker(&id),
        home.display()
    )
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

/// A person's start with their unit already up writes nothing under `/run` and keeps its token:
/// it refreshes the row's copies, then waits on the unit, which here ends.
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
    ];
    assert_eq!(calls_from(0), expect(&warm, "pfperson", uid));
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
    let read = rustix::fs::lgetxattr(session::env_path("pfowner"), shared::ACL_ACCESS, &mut acl);
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
    use shared::{ACL_GROUP_OBJ, ACL_MASK, ACL_NO_ID, ACL_OTHER, ACL_USER, ACL_USER_OBJ};
    let reader = shared::acl_bytes(&[
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
/// no login drop-in, no handover.
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
    rig.backend.login_socket(handover::Label::Off);
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
    ];
    assert_eq!(calls_from(0), expect(&expected, "pfperson", PERSON_UID));
    assert_eq!(*logins.lock().unwrap(), [true, false]);
    assert!(!left, "the drop-in went with the helper's start");
    assert!(!rig.backend.inner.handover.waits("pfperson"));
}
