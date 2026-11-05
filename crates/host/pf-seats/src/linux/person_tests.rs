//! A row on a person's own account, held to what the supervisor may do there. The backend runs
//! against [`fake`], which records every program it asks for, so a test names exactly what a
//! stop, a delete or a refused start did to the machine.

use super::*;
use crate::model::CreateSeat;
use std::os::unix::fs::PermissionsExt as _;

/// The unit's state as `systemctl show` prints it.
const SHOW: &str =
    "systemctl show punktfunk-seat@jdoe.service -p ActiveState -p SubState -p Result";
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
