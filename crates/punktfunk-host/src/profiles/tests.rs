use super::*;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n rest of a picture";
const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
const DEVICE: &str = "AB12CD34000000000000000000000000000000000000000000000000000000ff";

fn json<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pf-profiles-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn profile(id: &str, name: &str, os_account: OsAccount) -> Profile {
    Profile {
        id: id.into(),
        display_name: name.into(),
        accent: None,
        avatar: None,
        os_account,
        home: Home::Desktop,
        legacy_device: None,
        assigned_fingerprints: Vec::new(),
        passcode: None,
        require_passcode_even_when_assigned: false,
        allow_shared_view: false,
        tvos_user_ids: Vec::new(),
        library_scope: LibraryScope::All,
        custom_entries: Vec::new(),
        session_defaults: SessionDefaults::default(),
        created_unix: 1,
        updated_unix: 1,
        last_used_unix: 0,
    }
}

fn seat(id: &str, name: &str, seat: Option<&str>) -> Profile {
    let mut p = profile(
        id,
        name,
        OsAccount::Seat {
            seat: seat.map(Into::into),
            tier: SeatTier::Light,
        },
    );
    p.home = Home::Bigpicture;
    p
}

/// A store over `file`, written to disk first so the load path is the real one.
fn store(name: &str, file: &ProfilesFile, seat_host: Option<&str>) -> (Profiles, PathBuf) {
    let path = temp_dir(name).join("profiles.json");
    std::fs::write(&path, serde_json::to_vec(file).unwrap()).unwrap();
    (
        Profiles::load_with(Some(path.clone()), seat_host.map(Into::into)),
        path,
    )
}

fn box_file() -> ProfilesFile {
    let mut kid = seat("9a3f1c2b7e40", "Kid", None);
    kid.legacy_device = Some(DEVICE.to_ascii_lowercase());
    ProfilesFile {
        version: 1,
        profiles: vec![
            profile("4f1c3a9b0e27", "Enrico", OsAccount::Operator),
            kid,
            seat("0123456789ab", "Guest", None),
        ],
        default_profile_id: None,
    }
}

#[test]
fn no_ask_from_a_device_with_an_old_seat_lands_in_that_profile() {
    let (p, _) = store("legacy", &box_file(), None);
    let r = p.resolve(Some(DEVICE), None).unwrap();
    assert_eq!(
        (r.id.as_str(), r.via),
        ("9a3f1c2b7e40", ResolveVia::LegacyDevice)
    );
    assert_eq!(r.home, Home::Bigpicture);
}

#[test]
fn no_ask_otherwise_lands_on_the_default_then_the_owner() {
    let (p, _) = store("owner", &box_file(), None);
    let r = p.resolve(Some("ee"), None).unwrap();
    assert_eq!(
        (r.id.as_str(), r.via),
        ("4f1c3a9b0e27", ResolveVia::Default)
    );

    let mut file = box_file();
    file.default_profile_id = Some("0123456789ab".into());
    let (p, _) = store("default", &file, None);
    assert_eq!(p.resolve(None, None).unwrap().id, "0123456789ab");

    file.default_profile_id = Some("gone".into());
    let (p, _) = store("dangling", &file, None);
    assert_eq!(p.resolve(None, None).unwrap().id, "4f1c3a9b0e27");
}

#[test]
fn a_known_ask_wins_over_the_device_seat() {
    let (p, _) = store("asked", &box_file(), None);
    let r = p.resolve(Some(DEVICE), Some("0123456789ab")).unwrap();
    assert_eq!((r.id.as_str(), r.via), ("0123456789ab", ResolveVia::Asked));
}

#[test]
fn an_unknown_or_oversized_ask_is_refused() {
    let (p, _) = store("unknown", &box_file(), None);
    assert_eq!(
        p.resolve(None, Some("ffffffffffff")),
        Err(ProfileError::Unknown)
    );
    let long = "9".repeat(PROFILE_ID_MAX + 1);
    assert_eq!(p.resolve(None, Some(&long)), Err(ProfileError::Unknown));
}

#[test]
fn a_seat_host_refuses_another_seats_profile() {
    let mut file = box_file();
    file.profiles
        .push(seat("aaaaaaaaaaaa", "Mine", Some("seat-a")));
    file.profiles
        .push(seat("bbbbbbbbbbbb", "Theirs", Some("seat-b")));
    let (p, _) = store("seat-host", &file, Some("seat-a"));
    assert_eq!(
        p.resolve(None, Some("aaaaaaaaaaaa")).unwrap().id,
        "aaaaaaaaaaaa"
    );
    assert_eq!(
        p.resolve(None, Some("bbbbbbbbbbbb")),
        Err(ProfileError::NotThisSeat)
    );
}

#[test]
fn an_os_user_profile_has_no_session_yet() {
    let mut file = box_file();
    file.profiles.push(profile(
        "cccccccccccc",
        "Alice",
        OsAccount::Linux {
            username: "alice".into(),
            uid: Some(1001),
        },
    ));
    let (p, _) = store("linux", &file, None);
    assert_eq!(
        p.resolve(None, Some("cccccccccccc")),
        Err(ProfileError::SessionUnavailable)
    );
}

#[test]
fn a_store_without_an_owner_still_resolves_the_box_session() {
    let p = Profiles::load_with(Some(temp_dir("empty").join("profiles.json")), None);
    let r = p.resolve(None, None).unwrap();
    assert_eq!(r.id, OPERATOR_PROFILE_ID);
    assert_eq!(r.os_account, OsAccount::Operator);
}

#[test]
fn version_zero_reads_as_one() {
    let path = temp_dir("v0").join("profiles.json");
    std::fs::write(&path, br#"{"profiles":[]}"#).unwrap();
    let p = Profiles::load_with(Some(path), None);
    assert_eq!(p.lock().file.version, 1);
}

#[test]
fn an_unparsed_file_keeps_its_bytes_and_refuses_changes() {
    let path = temp_dir("unparsed").join("profiles.json");
    std::fs::write(&path, b"{ not json").unwrap();
    let p = Profiles::load_with(Some(path.clone()), None);
    assert!(p.ensure_owner("box").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
    assert_eq!(p.resolve(None, None).unwrap().id, OPERATOR_PROFILE_ID);
}

#[test]
fn a_failed_save_leaves_memory_as_it_was() {
    let dir = temp_dir("failed-save");
    // The store's parent is a file, so every save fails.
    std::fs::write(dir.join("blocker"), b"").unwrap();
    let p = Profiles::load_with(Some(dir.join("blocker").join("profiles.json")), None);
    assert!(p.ensure_owner("box").is_err());
    assert!(p.list().is_empty());
}

#[test]
fn the_owner_is_created_once_across_restarts() {
    let path = temp_dir("owner-once").join("profiles.json");
    let first = Profiles::load_with(Some(path.clone()), None);
    first.ensure_owner("box").unwrap();
    first.ensure_owner("box").unwrap();
    let owners = first.list();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].os_account, OsAccount::Operator);
    assert_eq!(owners[0].id.len(), 12);

    let again = Profiles::load_with(Some(path), None);
    again.ensure_owner("other").unwrap();
    assert_eq!(json(&again.list()), json(&owners));
}

#[test]
fn a_file_round_trips_its_unused_fields() {
    let mut file = box_file();
    file.profiles[0].passcode = Some(PasscodeHash {
        phc: "$argon2id$x".into(),
    });
    file.profiles[0].library_scope = LibraryScope::Deny {
        ids: vec!["steam:570".into()],
    };
    file.profiles[0].os_account = OsAccount::Windows {
        account_name: r".\enrico".into(),
        sid: None,
        credential: CredentialRef::DpapiBlob {
            path: "cred/x.bin".into(),
        },
    };
    let text = serde_json::to_string(&file).unwrap();
    assert_eq!(
        json(&serde_json::from_str::<ProfilesFile>(&text).unwrap()),
        json(&file)
    );
}

#[test]
fn the_design_example_parses() {
    let text = r##"{
      "version": 1,
      "profiles": [
        { "id": "4f1c3a9b0e27", "display_name": "Enrico", "accent": "#3b82f6",
          "os_account": { "kind": "operator" }, "home": "desktop",
          "created_unix": 1790000000, "updated_unix": 1790500000 },
        { "id": "9a3f1c2b7e40", "display_name": "Kid", "accent": "#f97316",
          "avatar": "/api/v1/profiles/9a3f1c2b7e40/avatar",
          "os_account": { "kind": "seat", "seat": "c0ffee" },
          "home": "bigpicture", "legacy_device": "ab12",
          "created_unix": 1790100000, "updated_unix": 1790100000 }
      ],
      "default_profile_id": "4f1c3a9b0e27"
    }"##;
    let file: ProfilesFile = serde_json::from_str(text).unwrap();
    assert_eq!(
        file.profiles[1].os_account,
        OsAccount::Seat {
            seat: Some("c0ffee".into()),
            tier: SeatTier::Light
        }
    );
    assert_eq!(file.profiles[1].home, Home::Bigpicture);
}

#[test]
fn an_avatar_is_stored_by_its_magic_and_replaces_the_last() {
    let (p, path) = store("avatar", &box_file(), None);
    let id = "9a3f1c2b7e40";
    let tag = p.put_avatar(id, PNG).unwrap();
    let a = p.avatar(id).unwrap();
    assert_eq!(
        (a.mime, a.etag.as_str(), a.bytes.as_slice()),
        ("image/png", tag.as_str(), PNG)
    );
    assert_eq!(
        p.get(id).unwrap().avatar.as_deref(),
        Some("/api/v1/profiles/9a3f1c2b7e40/avatar")
    );

    p.put_avatar(id, JPEG).unwrap();
    assert_eq!(p.avatar(id).unwrap().mime, "image/jpeg");
    assert!(!path
        .with_file_name("profiles")
        .join(format!("{id}.png"))
        .exists());

    p.remove_avatar(id).unwrap();
    assert!(p.avatar(id).is_none());
    assert!(p.get(id).unwrap().avatar.is_none());
}

#[test]
fn an_avatar_is_refused_when_not_an_image_too_large_or_unknown() {
    let (p, _) = store("avatar-refused", &box_file(), None);
    assert!(p.put_avatar("9a3f1c2b7e40", b"GIF89a").is_err());
    let mut big = PNG.to_vec();
    big.resize(AVATAR_MAX_BYTES + 1, 0);
    assert!(p.put_avatar("9a3f1c2b7e40", &big).is_err());
    assert!(p.put_avatar("ffffffffffff", PNG).is_err());
}

#[test]
fn a_hand_edited_id_never_reaches_a_path() {
    let mut file = box_file();
    file.profiles.push(seat("../escape", "Odd", None));
    let (p, _) = store("unsafe-id", &file, None);
    assert!(p.put_avatar("../escape", PNG).is_err());
    assert!(p.avatar("../escape").is_none());
}

#[test]
fn names_are_trimmed_capped_and_unique() {
    assert_eq!(clean_name("  Enrico\u{7}  ").as_deref(), Some("Enrico"));
    assert_eq!(clean_name(" \n ").as_deref(), None);
    assert_eq!(
        clean_name(&"x".repeat(40)).unwrap().chars().count(),
        DISPLAY_NAME_MAX
    );
    let taken = [profile("a", "Enrico", OsAccount::Operator)];
    assert_eq!(unique_name(&taken, "enrico"), "enrico 2");
}

#[test]
fn a_device_seat_becomes_a_profile_its_device_lands_in() {
    let root = temp_dir("migrate");
    let seats = root.join("seats");
    let steam = |hex: &str| {
        let dir = seats.join(hex).join(".local/share/Steam");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("steam.sh"), b"#!/bin/sh").unwrap();
    };
    steam("ab12cd34");
    steam("deadbeef");
    std::fs::create_dir_all(seats.join("0badf00d")).unwrap(); // no Steam: not a seat
    std::fs::write(seats.join("ab12cd34.json"), b"{}").unwrap();
    let fp = format!("ab12cd34{}", "0".repeat(56));
    let p = Profiles::load_with(Some(root.join("profiles.json")), None);
    p.ensure_owner("box").unwrap();
    p.migrate_device_seats(&seats, &[("Enrico's iPad".into(), fp.clone())]);

    let ipad = p
        .list()
        .into_iter()
        .find(|x| x.display_name == "Enrico's iPad")
        .expect("the device's seat is a profile");
    assert_eq!(ipad.legacy_device.as_deref(), Some(fp.as_str()));
    assert_eq!(ipad.home, Home::Bigpicture);
    assert!(seats
        .join(&ipad.id)
        .join(".local/share/Steam/steam.sh")
        .exists());
    assert!(seats.join(format!("{}.json", ipad.id)).exists());
    assert!(!seats.join("ab12cd34").exists());
    assert!(p
        .list()
        .iter()
        .any(|x| x.display_name == "Seat deadbeef" && x.legacy_device.is_none()));
    assert!(
        seats.join("0badf00d").exists(),
        "a directory with no Steam stays"
    );

    let r = p.resolve(Some(&fp.to_ascii_uppercase()), None).unwrap();
    assert_eq!((r.id, r.via), (ipad.id, ResolveVia::LegacyDevice));

    // A second start finds nothing left to move.
    let before = p.list().len();
    p.migrate_device_seats(&seats, &[("Enrico's iPad".into(), fp)]);
    assert_eq!(p.list().len(), before);
}
