//! A row on a person's own account, held to what the supervisor may do there. The backend runs
//! against [`fake`], which records every program it asks for, so a test names exactly what a
//! stop, a delete or a refused start did to the machine.

use super::*;
use crate::model::CreateSeat;
use std::os::unix::fs::PermissionsExt as _;

/// The unit's state as `systemctl show` prints it.
const SHOW: &str = "systemctl show punktfunk-seat@jdoe.service -p ActiveState -p SubState \
     -p Result -p ExecMainCode -p ExecMainStatus";
const STOP: &str = "systemctl stop punktfunk-seat@jdoe.service";
const RESET: &str = "systemctl reset-failed punktfunk-seat@jdoe.service";

/// A person's row for `jdoe`, uid 1500, as adopt records it.
fn person() -> Seat {
    let request = CreateSeat {
        name: "Jane".into(),
        account: "jdoe".into(),
        autostart: false,
    };
    Ledger::new().allocate_adopted_user(request, 1500).unwrap()
}

/// `jdoe` as the machine knows them.
fn passwd(home: &Path, uid: u32) -> accounts::Passwd {
    accounts::Passwd {
        name: "jdoe".into(),
        uid,
        gid: uid,
        gecos: "Jane Doe".into(),
        home: home.into(),
    }
}

/// The machine: `jdoe` is user `uid` at `home`, their unit is `unit`, and `at_desk` says whether
/// they have a session on a seat.
fn machine(
    home: &Path,
    uid: u32,
    unit: &'static str,
    at_desk: bool,
) -> impl Fn(&str, &[&str]) -> (i32, String) + use<> {
    let row = format!("jdoe:x:{uid}:{uid}:Jane Doe:{}:/bin/bash\n", home.display());
    move |program, args| match (program, args) {
        ("getent", ["passwd", "jdoe"]) => (0, row.clone()),
        ("getent", _) => (2, String::new()),
        ("systemctl", ["show", ..]) => (
            0,
            format!("ActiveState={unit}\nSubState=running\nResult=success\n"),
        ),
        ("loginctl", ["list-sessions", ..]) if at_desk => (0, "7 1500 jdoe seat0 tty2\n".into()),
        ("loginctl", ["show-session", ..]) => (
            0,
            "Id=7\nName=jdoe\nSeat=seat0\nClass=user\nRemote=no\nState=active\n".into(),
        ),
        _ => (0, String::new()),
    }
}

/// Every path under `root` with its mode and, for a file, its bytes.
fn tree(root: &Path) -> Vec<(String, u32, Vec<u8>)> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(path) = todo.pop() {
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let bytes = if meta.is_file() {
            std::fs::read(&path).unwrap()
        } else {
            Vec::new()
        };
        out.push((path.display().to_string(), meta.permissions().mode(), bytes));
        if meta.is_dir() {
            todo.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
        }
    }
    out.sort();
    out
}

/// A person's home with what a person keeps there, and a state directory holding the row's
/// own files.
struct Rig {
    _temp: tempfile::TempDir,
    backend: LinuxBackend,
    seat: Seat,
    home: PathBuf,
    state: PathBuf,
}

fn rig() -> Rig {
    let temp = tempfile::tempdir().unwrap();
    let (box_dir, state, home) = (
        temp.path().join("box"),
        temp.path().join("state"),
        temp.path().join("home"),
    );
    std::fs::create_dir_all(&box_dir).unwrap();
    std::fs::create_dir_all(home.join(".config/punktfunk")).unwrap();
    std::fs::write(home.join(".config/punktfunk/host-settings.json"), "{}").unwrap();
    std::fs::write(home.join("notes.txt"), "mine").unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o750)).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    shared::ensure_layout(&state).unwrap();
    let seat = person();
    for dir in [
        shared::person_config_dir(&state, &seat.id),
        shared::trust_dir(&state, &seat.id),
        shared::host_dir(&state, &seat.id),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let backend = LinuxBackend::detached(&box_dir, &state);
    Rig {
        _temp: temp,
        backend,
        seat,
        home,
        state,
    }
}

/// The trap: a person's account is theirs, and may have a login at the desk and a shell over
/// SSH. Stopping the row ends its unit and nothing else; deleting it also removes what the row
/// made under the state directory. Neither asks `loginctl`, neither ends the user's processes,
/// and nothing in the account changes.
#[test]
fn stopping_or_deleting_a_person_row_touches_only_its_own_unit_and_files() {
    let rig = rig();
    let (backend, seat) = (&rig.backend, &rig.seat);
    let _machine = fake::install(machine(&rig.home, 1500, "active", true));
    let before = tree(&rig.home);
    backend.track(&seat.id, Tracked::of(seat, &passwd(&rig.home, 1500)));
    assert_eq!(backend.running_account(1500).as_deref(), Some("jdoe"));

    assert_eq!(backend.stop(seat).unwrap(), RuntimeStatus::stopped());
    assert_eq!(fake::calls(), [SHOW, STOP, RESET]);
    assert_eq!(backend.running_account(1500), None, "no pads once it stops");

    backend.remove(seat).unwrap();
    assert_eq!(fake::calls()[3..], [SHOW, STOP, RESET]);
    for gone in [
        shared::seat_dir(&rig.state, &seat.id),
        shared::trust_dir(&rig.state, &seat.id),
        shared::host_dir(&rig.state, &seat.id),
    ] {
        assert!(!gone.exists(), "{}", gone.display());
    }
    assert_eq!(tree(&rig.home), before, "their home is as they left it");
}

/// With them at the machine a start is refused before anything is made, with a sentence for
/// the console.
#[test]
fn a_person_row_does_not_start_while_they_sit_at_the_machine() {
    let rig = rig();
    let _machine = fake::install(machine(&rig.home, 1500, "inactive", true));
    let before = (tree(&rig.home), tree(&rig.state));

    let refused = rig.backend.start(&rig.seat).unwrap_err();
    assert_eq!(refused.code, "account_at_desk");
    assert_eq!(
        refused.message,
        "jdoe is signed in at the machine. Try again once they have signed out."
    );
    assert!(!fake::calls().iter().any(|c| c.contains("start")));
    assert_eq!((tree(&rig.home), tree(&rig.state)), before);
    assert_eq!(rig.backend.running_account(1500), None);
}

/// A name given to another user since the row was made is not the row's: no start, no adopt, no
/// pin read, and the doctor says so. Deleting the row still works, and still touches only its
/// unit.
#[test]
fn a_name_that_is_another_user_now_is_not_the_row() {
    let rig = rig();
    let _machine = fake::install(machine(&rig.home, 2000, "inactive", false));
    let before = tree(&rig.home);

    for refused in [
        rig.backend.start(&rig.seat).unwrap_err(),
        rig.backend
            .adopt_account(&rig.seat, &Secret::new("pw".into()))
            .unwrap_err(),
    ] {
        assert_eq!(refused.code, "account_not_owned");
    }
    assert_eq!(rig.backend.host_config_dir(&rig.seat), None);
    let mut ledger = Ledger::new();
    ledger.seats.push(rig.seat.clone());
    let report = rig.backend.doctor(&ledger).unwrap();
    let marker = report.iter().find(|d| d.code == "account_marker").unwrap();
    assert_eq!(marker.level, DiagnosticLevel::Error);

    rig.backend.remove(&rig.seat).unwrap();
    assert_eq!(tree(&rig.home), before);
    assert!(fake::calls()
        .iter()
        .all(|c| c.starts_with("getent ") || c.starts_with("systemctl ")));
}

/// Only a made seat's account is the supervisor's to close, delete and clear; the owner's and a
/// person's end with their unit alone, and yield to their own login.
#[test]
fn only_a_made_seat_is_ours_to_end() {
    let made = Seat {
        adopted_uid: None,
        ..person()
    };
    let owner = Seat {
        owner: true,
        adopted_uid: None,
        ..person()
    };
    assert_eq!(Own::of(&made), Own::Seat);
    assert_eq!(Own::of(&owner), Own::Owner);
    assert_eq!(Own::of(&person()), Own::Person);
    for (seat, ours, yields) in [
        (made, true, false),
        (owner, false, true),
        (person(), false, true),
    ] {
        let kind = Own::of(&seat);
        assert_eq!(
            (kind.ours(), kind.yields_to_the_desk()),
            (ours, yields),
            "{kind:?}"
        );
    }
}

/// The login helper gives the session the shell the passwd row names, and none for a user that
/// doesn't exist.
#[test]
fn the_login_shell_is_the_passwd_row_s_last_field() {
    let rig = rig();
    let _machine = fake::install(machine(&rig.home, 1500, "inactive", false));
    assert_eq!(
        accounts::shell("jdoe").unwrap().as_deref(),
        Some("/bin/bash")
    );
    assert_eq!(accounts::shell("nobody-here").unwrap(), None);
}

/// The start choice: a person's password starts their session through the login helper, and a
/// start without one, a key proof's, through systemd's own login, as does every start once the
/// helper is off.
#[test]
fn only_a_password_start_goes_through_the_login_helper() {
    let password = Secret::new("hunter2 Bärbel".into());
    assert_eq!(login_with(Some(&password), false, false), Some(&password));
    assert_eq!(login_with(None, false, false), None);
    assert_eq!(login_with(Some(&password), false, true), None, "helper off");
    assert_eq!(
        login_with(Some(&password), true, false),
        None,
        "SELinux on: today's start"
    );
}

/// The helper's end decides what a failed start through it does: every end before a session
/// falls back, PAM's refusal of what the gate admitted also trips the helper off, anything else
/// is the seat's failure. A plain start never reads the helper's codes.
#[test]
fn a_start_through_the_login_helper_reads_its_end() {
    let seat = person();
    let ended = |code| session::UnitState {
        active: "failed".into(),
        result: "exit-code".into(),
        ended: Some(session::Ended::Exited(code)),
        ..Default::default()
    };
    assert_eq!(unit_ended(&seat, &ended(91), true).code, LOGIN_TRIPPED);
    let fallback = unit_ended(&seat, &ended(95), true);
    assert_eq!(fallback.code, LOGIN_FALLBACK);
    assert_eq!(fallback.message, "the login helper got no password");
    for (code, login) in [(1, true), (91, false), (95, false), (203, false)] {
        assert_eq!(unit_ended(&seat, &ended(code), login).code, "seat_exited");
    }
}

/// The login drop-in runs the helper as root (`!`: the unit's `User=` stays, only the switch to
/// it is skipped) with no restart, and carries no secret; a path a unit file can't hold verbatim
/// gets none.
#[test]
fn the_login_drop_in_runs_the_helper_and_nothing_else() {
    let dropin =
        session::render_login(Path::new("/usr/libexec/punktfunk/punktfunk-seats")).unwrap();
    assert_eq!(
        dropin,
        "[Service]\nExecStart=\n\
         ExecStart=!/usr/libexec/punktfunk/punktfunk-seats login %i\nRestart=no\n\
         SyslogIdentifier=seat-session\n"
    );
    let nix = "/nix/store/0abc-punktfunk-seats-0.47.0/libexec/punktfunk/punktfunk-seats";
    assert!(session::render_login(Path::new(nix)).is_some());
    for bad in [
        "relative/punktfunk-seats",
        "/opt/a b/x",
        "/opt/%i/x",
        "/opt/$HOME/x",
        "/a\nb",
    ] {
        assert_eq!(session::render_login(Path::new(bad)), None, "{bad:?}");
    }
    assert!(
        login_dropin().is_some(),
        "this test binary's own path renders"
    );
}

/// Held by each test that writes `jdoe`'s real drop-ins: two at once remove each other's files.
static REAL_DROPINS: Mutex<()> = Mutex::new(());

/// A start that wrote the login drop-in takes it away on every way out, a failed `systemctl
/// start` included, and leaves no password waiting.
#[test]
#[ignore = "needs root: writes the row's drop-ins under /run/systemd/system and /run/punktfunk"]
fn a_failed_start_leaves_no_login_drop_in() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _alone = REAL_DROPINS.lock().unwrap_or_else(|e| e.into_inner());
    let rig = rig();
    let dropin = session::dropin_dir("jdoe").join("login.conf");
    let seen = Arc::new(AtomicBool::new(false));
    let (base, at_start, path) = (
        machine(&rig.home, 1500, "inactive", false),
        Arc::clone(&seen),
        dropin.clone(),
    );
    let _machine = fake::install(move |program, args| match (program, args) {
        ("systemctl", ["start", _]) => {
            at_start.store(path.exists(), Ordering::SeqCst);
            (1, String::new())
        }
        _ => base(program, args),
    });
    let password = Secret::new("hunter2 Bärbel".into());
    let error = rig.backend.start_with_password(&rig.seat, &password);
    let left = dropin.exists();
    let _ = std::fs::remove_dir_all(session::dropin_dir("jdoe"));
    let _ = std::fs::remove_file(session::env_path("jdoe"));
    assert_eq!(error.unwrap_err().code, "systemctl_failed");
    assert!(
        seen.load(Ordering::SeqCst),
        "the start ran with the drop-in"
    );
    assert!(!left, "the drop-in went with the failed start");
    assert!(!rig.backend.inner.handover.waits("jdoe"));
}

/// PAM refusing in the helper what the gate admitted falls back to today's start, and the
/// supervisor runs no helper again until it restarts.
#[test]
#[ignore = "needs root: writes the row's drop-ins under /run/systemd/system and /run/punktfunk"]
fn a_helper_refusal_falls_back_and_turns_the_helper_off() {
    let _alone = REAL_DROPINS.lock().unwrap_or_else(|e| e.into_inner());
    let rig = rig();
    let dropin = session::dropin_dir("jdoe").join("login.conf");
    let starts = Arc::new(Mutex::new(Vec::new()));
    let (base, at_start, at_show, path) = (
        machine(&rig.home, 1500, "inactive", false),
        Arc::clone(&starts),
        Arc::clone(&starts),
        dropin.clone(),
    );
    let refused = "ActiveState=failed\nSubState=failed\nResult=exit-code\nExecMainCode=1\n\
                   ExecMainStatus=91\n";
    let _machine = fake::install(move |program, args| match (program, args) {
        ("systemctl", ["start", _]) => {
            let mut starts = at_start.lock().unwrap();
            starts.push(path.exists());
            (i32::from(starts.len() > 1), String::new())
        }
        ("systemctl", ["show", ..]) if !at_show.lock().unwrap().is_empty() => (0, refused.into()),
        _ => base(program, args),
    });
    let password = Secret::new("hunter2 Bärbel".into());
    let first = rig.backend.start_with_password(&rig.seat, &password);
    let second = rig.backend.start_with_password(&rig.seat, &password);
    let left = dropin.exists();
    let _ = std::fs::remove_dir_all(session::dropin_dir("jdoe"));
    let _ = std::fs::remove_file(session::env_path("jdoe"));
    assert_eq!(
        first.unwrap_err().code,
        "systemctl_failed",
        "the plain start ran"
    );
    assert_eq!(second.unwrap_err().code, "systemctl_failed");
    assert_eq!(*starts.lock().unwrap(), [true, false, false]);
    assert!(!left);
}

/// The password check runs as `punktfunk-seats pam-check <account>` once the account still names
/// the row's uid: the account alone is its argument, and the code it prints reads as the
/// console's sentence. An account that moved never reaches the check.
#[test]
fn the_password_check_names_the_account_and_never_the_password() {
    let rig = rig();
    let password = Secret::new("hunter2 Bärbel".into());
    for (code, refused) in [
        ("ok\n", None),
        ("credential_wrong\n", Some("That isn't jdoe's password.")),
    ] {
        let machine = machine(&rig.home, 1500, "inactive", false);
        let _fake = fake::install(move |program, args| match program {
            "punktfunk-seats" => (0, code.into()),
            _ => machine(program, args),
        });
        let checked = rig.backend.check_password(&rig.seat, &password);
        assert_eq!(checked.err().map(|e| e.message).as_deref(), refused);
        assert_eq!(
            fake::calls(),
            ["getent passwd jdoe", "punktfunk-seats pam-check jdoe"]
        );
    }
    let moved = machine(&rig.home, 2000, "inactive", false);
    let _fake = fake::install(moved);
    let refused = rig
        .backend
        .check_password(&rig.seat, &password)
        .unwrap_err();
    assert_eq!(refused.code, "account_not_owned");
    assert_eq!(fake::calls(), ["getent passwd jdoe"]);
}
