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

/// A seat profile called `name`, as the console asks for one.
fn new(name: &str) -> ProfileCreate {
    ProfileCreate {
        display_name: name.into(),
        accent: None,
        home: None,
        seat: true,
        windows_account: None,
        seat_account: None,
        seat_password: None,
    }
}

#[test]
fn a_profile_create_would_refuse_is_vetted_before_its_seat_is_made() {
    let (p, _) = store("vet", &box_file(), None);
    assert_eq!(p.vet_create(&new("  New\u{7}Kid  ")).unwrap(), "NewKid");
    assert_eq!(
        p.vet_create(&new(&"x".repeat(80))).unwrap().len(),
        DISPLAY_NAME_MAX
    );
    assert!(matches!(
        p.vet_create(&new("kid")),
        Err(EditError::NameTaken)
    ));
    assert!(matches!(
        p.vet_create(&new(" \u{7} ")),
        Err(EditError::NameMissing)
    ));
    let red = ProfileCreate {
        accent: Some("red".into()),
        ..new("Fresh")
    };
    assert!(matches!(p.vet_create(&red), Err(EditError::Invalid(_))));

    let mut full = box_file();
    full.profiles = (0..PROFILES_MAX)
        .map(|i| profile(&format!("{i:012x}"), &format!("P{i}"), OsAccount::Operator))
        .collect();
    let (p, _) = store("vet-full", &full, None);
    assert!(matches!(p.vet_create(&new("Fresh")), Err(EditError::Full)));
}

/// A store that can't save refuses a create before its seat is made: a file that didn't parse,
/// and a seat host's copy of the box's.
#[test]
fn a_store_that_cannot_save_refuses_a_create_up_front() {
    let path = temp_dir("vet-unparsed").join("profiles.json");
    std::fs::write(&path, b"{ not json").unwrap();
    let unparsed = Profiles::load_with(Some(path), None);
    assert!(matches!(
        unparsed.vet_create(&new("Fresh")),
        Err(EditError::Store(_))
    ));
    let path = temp_dir("vet-copy").join("profiles.json");
    std::fs::write(&path, serde_json::to_vec(&box_file()).unwrap()).unwrap();
    let copy = Profiles::load_box(path, Some("seat-a".into()));
    assert!(matches!(
        copy.vet_create(&new("Fresh")),
        Err(EditError::Store(_))
    ));
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
    // The owner's session is not this seat's to play either.
    assert_eq!(
        p.resolve(None, Some("4f1c3a9b0e27")),
        Err(ProfileError::NotThisSeat)
    );
}

/// The owner's host, behind the door, plays the owner and the profiles that share the owner's
/// desktop, and still refuses a full seat that is another host's.
#[test]
fn the_owner_s_host_serves_the_owner_and_its_sharers() {
    let mut file = box_file();
    file.profiles
        .push(profile("cccccccccccc", "Sibling", OsAccount::Operator));
    file.profiles
        .push(seat("dddddddddddd", "Theirs", Some("seat-b")));
    let (mut p, _) = store("owner-host", &file, Some("owner-row"));
    p.owner_host = true;
    // No ask: the owner, not a seat that names this row.
    assert_eq!(p.resolve(None, None).unwrap().id, "4f1c3a9b0e27");
    for id in ["4f1c3a9b0e27", "cccccccccccc"] {
        assert_eq!(p.resolve(None, Some(id)).unwrap().id, id);
    }
    assert_eq!(
        p.resolve(None, Some("dddddddddddd")),
        Err(ProfileError::NotThisSeat)
    );
}

/// The box rewrites `path`; a later mtime than any earlier write, whatever the clock granularity.
fn box_writes(path: &Path, file: &ProfilesFile) {
    std::fs::write(path, serde_json::to_vec(file).unwrap()).unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(later).unwrap();
}

#[test]
fn a_seat_host_reads_the_box_file_and_plays_its_own_profile() {
    let mut file = box_file();
    file.profiles
        .push(seat("aaaaaaaaaaaa", "Mine", Some("seat-a")));
    let path = temp_dir("box-copy").join("profiles.json");
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    let p = Profiles::load_box(path.clone(), Some("seat-a".into()));
    // No ask, even from a device with an old seat: this seat's own profile.
    assert_eq!(p.resolve(Some(DEVICE), None).unwrap().id, "aaaaaaaaaaaa");
    assert!(p.set_default(None).is_err(), "a seat never writes profiles");

    // The box removes the seat's profile: the seat plays for nobody.
    box_writes(&path, &box_file());
    assert_eq!(p.resolve(None, None), Err(ProfileError::SessionUnavailable));
}

/// A profile the box deletes never runs `delete` on a seat host: the seat's watcher announces
/// it, so a session playing as it ends.
#[test]
fn a_seat_host_announces_a_profile_the_box_removed() {
    fn heard(rx: &mut tokio::sync::broadcast::Receiver<ProfileId>) -> Vec<ProfileId> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }
    let sharer = "eeeeeeeeeeee".to_string();
    let mut file = box_file();
    file.profiles.push(seat(&sharer, "Sharer", Some("seat-a")));
    let path = temp_dir("announce").join("profiles.json");
    std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
    let p = Profiles::load_box(path.clone(), Some("seat-a".into()));
    let mut rx = removed();
    let mut known = std::collections::HashSet::new();

    // The first look only takes the ids.
    p.announce_removals(&mut known);
    assert_eq!(known.len(), 4);
    assert!(!heard(&mut rx).contains(&sharer));

    // A file that doesn't parse says nothing.
    std::fs::write(&path, b"{").unwrap();
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
    let f = std::fs::File::options().write(true).open(&path).unwrap();
    f.set_modified(later).unwrap();
    p.announce_removals(&mut known);
    assert_eq!(known.len(), 4);
    assert!(!heard(&mut rx).contains(&sharer));

    box_writes(&path, &box_file());
    p.announce_removals(&mut known);
    assert_eq!(known.len(), 3);
    assert!(heard(&mut rx).contains(&sharer));
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

/// A file an older host wrote, with the keys this one never read, loads whole; its next save
/// drops those keys and keeps everything else, the take-turns credential an older host needs.
#[test]
fn a_file_with_keys_no_longer_read_loads_and_saves_without_them() {
    let unread = r#""assigned_fingerprints": ["ab12"], "passcode": {"phc": "$argon2id$x"},
        "require_passcode_even_when_assigned": true, "allow_shared_view": true,
        "tvos_user_ids": ["u1"], "library_scope": {"mode": "deny", "ids": ["steam:570"]},
        "custom_entries": [], "session_defaults": {"max_width": 1920},"#;
    let text = format!(
        r#"{{"version": 1, "profiles": [
          {{"id": "4f1c3a9b0e27", "display_name": "Enrico", "os_account": {{"kind": "operator"}},
            {unread} "created_unix": 1, "updated_unix": 1}},
          {{"id": "9a3f1c2b7e40", "display_name": "Ben", "os_account": {{"kind": "windows",
            "account_name": "Ben", "credential": {{"source": "none"}}}},
            {unread} "home": "bigpicture", "created_unix": 1, "updated_unix": 1}}
        ]}}"#
    );
    let path = temp_dir("unread-keys").join("profiles.json");
    std::fs::write(&path, text).unwrap();
    let p = Profiles::load_with(Some(path.clone()), None);
    let ben = p.get("9a3f1c2b7e40").expect("the file parsed");
    assert_eq!(
        (ben.display_name.as_str(), ben.home),
        ("Ben", Home::Bigpicture)
    );
    p.set_default(Some("9a3f1c2b7e40")).unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    for key in [
        "assigned_fingerprints",
        "passcode",
        "allow_shared_view",
        "tvos_user_ids",
        "library_scope",
        "custom_entries",
        "session_defaults",
    ] {
        assert!(!saved.contains(key), "{key}: {saved}");
    }
    let file: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(
        file["profiles"][1]["os_account"],
        serde_json::json!({"kind": "windows", "account_name": "Ben",
            "credential": {"source": "none"}})
    );
    assert_eq!(file["default_profile_id"], "9a3f1c2b7e40");
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
            seat: Some("c0ffee".into())
        }
    );
    assert_eq!(file.profiles[1].home, Home::Bigpicture);
}

/// An older host wrote a seat's tier. The file still reads, and a light seat is still the seat
/// with no ledger row that [`Profiles::share_light_seats`] looks for.
#[test]
fn a_seat_tier_an_older_host_wrote_still_reads() {
    let text = r#"[
        { "kind": "seat", "tier": "light" },
        { "kind": "seat", "seat": "c0ffee", "tier": "full" }
    ]"#;
    let accounts: Vec<OsAccount> = serde_json::from_str(text).unwrap();
    assert_eq!(
        accounts,
        [
            OsAccount::Seat { seat: None },
            OsAccount::Seat {
                seat: Some("c0ffee".into())
            }
        ]
    );
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
    let listed = p.get(id).unwrap().avatar.unwrap();
    assert_eq!(
        listed,
        format!("/api/v1/profiles/9a3f1c2b7e40/avatar?v={tag}")
    );

    p.put_avatar(id, JPEG).unwrap();
    assert_eq!(p.avatar(id).unwrap().mime, "image/jpeg");
    assert_ne!(p.get(id).unwrap().avatar.unwrap(), listed);
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

/// Names are one whatever their case, past ASCII too, as the seat ledger compares its rows.
#[test]
fn names_fold_case_as_the_seat_ledger_does() {
    let mut file = box_file();
    file.profiles
        .push(profile("dddddddddddd", "Émile", OsAccount::Operator));
    let (p, _) = store("fold", &file, None);
    assert!(matches!(
        p.vet_create(&new("émile")),
        Err(EditError::NameTaken)
    ));
    assert_eq!(unique_name(&file.profiles, "ÉMILE"), "ÉMILE 2");
}

/// A light seat shares the owner's desktop and keeps what makes it the player's. A profile with
/// a ledger row is a desktop of its own, and the owner stays the owner.
#[test]
fn a_light_seat_becomes_a_profile_that_shares_the_desktop() {
    let mut file = box_file();
    let mut kid = file.profiles.remove(1);
    kid.avatar = Some("/api/v1/profiles/9a3f1c2b7e40/avatar".into());
    kid.last_used_unix = 42;
    // Hand-edited ahead of the owner.
    file.profiles.insert(0, kid);
    let desk = seat("aaaaaaaaaaaa", "Desk", Some("seat-a"));
    file.profiles.push(desk.clone());
    let (p, path) = store("share-light", &file, None);
    p.share_light_seats();

    let list = p.list();
    assert_eq!(list[0].id, "4f1c3a9b0e27", "the owner stays first");
    assert_eq!(p.owner_id().as_deref(), Some("4f1c3a9b0e27"));
    let kid = p.get("9a3f1c2b7e40").unwrap();
    assert_eq!(
        (&kid.os_account, kid.home),
        (&OsAccount::Operator, Home::Desktop)
    );
    assert_eq!((kid.display_name.as_str(), kid.last_used_unix), ("Kid", 42));
    assert!(kid.avatar.is_some());
    assert_eq!(p.resolve(Some(DEVICE), None).unwrap().id, kid.id);
    assert_eq!(
        p.get("0123456789ab").unwrap().os_account,
        OsAccount::Operator
    );
    assert_eq!(p.get("aaaaaaaaaaaa").unwrap().os_account, desk.os_account);

    // Saved: a restart reads it back and finds nothing left to change.
    let again = Profiles::load_with(Some(path.clone()), None);
    assert_eq!(json(&again.list()), json(&list));
    let saved = std::fs::read(&path).unwrap();
    again.share_light_seats();
    assert_eq!(std::fs::read(&path).unwrap(), saved);
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
    assert_eq!(
        (&ipad.os_account, ipad.home),
        (&OsAccount::Operator, Home::Desktop),
        "it shares the owner's desktop"
    );
    assert_eq!(p.owner_id(), Some(p.list()[0].id.clone()));
    assert!(seats
        .join(&ipad.id)
        .join(".local/share/Steam/steam.sh")
        .exists());
    assert!(!seats.join("ab12cd34").exists());
    assert!(
        !seats.join("ab12cd34.json").exists() && !seats.join(format!("{}.json", ipad.id)).exists(),
        "nothing is left to pre-warm"
    );
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

/// A connect stamps a profile once a minute: a reconnect burst saves nothing.
#[test]
fn touch_saves_only_a_stamp_older_than_the_granularity() {
    let now = now_unix();
    let mut file = box_file();
    file.profiles[0].last_used_unix = now - 5;
    file.profiles[1].last_used_unix = now - 3600;
    let (recent, old) = (file.profiles[0].id.clone(), file.profiles[1].id.clone());
    let (p, path) = store("touch", &file, None);

    p.touch(&recent);
    assert_eq!(p.get(&recent).unwrap().last_used_unix, now - 5);
    p.touch(&old);
    assert!(p.get(&old).unwrap().last_used_unix >= now);
    let saved: ProfilesFile = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let stamp = |id: &str| {
        saved
            .profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.last_used_unix)
    };
    assert_eq!(stamp(&recent), Some(now - 5));
    assert!(stamp(&old) >= Some(now));
}
