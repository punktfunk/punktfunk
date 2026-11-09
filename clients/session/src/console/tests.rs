use super::*;
use pf_client_core::presets::{PresetsFile, StreamPreset};
use pf_console_ui::model::pinned_key;
use std::net::TcpListener;

/// A fingerprint of one repeated digit: readable, and distinct per digit.
fn fp(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

fn saved(name: &str, addr: &str, fp_hex: &str) -> trust::KnownHost {
    trust::KnownHost {
        name: name.into(),
        addr: addr.into(),
        port: 9777,
        fp_hex: fp_hex.into(),
        paired: !fp_hex.is_empty(),
        ..Default::default()
    }
}

fn advert(name: &str, addr: &str, fp_hex: &str) -> discovery::DiscoveredHost {
    discovery::DiscoveredHost {
        key: name.into(),
        fullname: name.into(),
        name: name.into(),
        addr: addr.into(),
        port: 9777,
        fp_hex: fp_hex.into(),
        pair: "optional".into(),
        mgmt_port: None,
        mac: Vec::new(),
        os: String::new(),
        wire: Vec::new(),
    }
}

fn preset(name: &str) -> StreamPreset {
    let mut p = StreamPreset::new(name);
    p.id = format!("id-{name}");
    p
}

/// The rows for a store, a catalog, the visible adverts and the hosts the sweep found up.
fn rows_of(
    hosts: Vec<trust::KnownHost>,
    catalog: &PresetsFile,
    adverts: Vec<discovery::DiscoveredHost>,
    up: &[&str],
    seed: Option<&HostRow>,
) -> Vec<HostRow> {
    let discovered = adverts
        .into_iter()
        .map(|a| (a.fullname.clone(), a))
        .collect();
    let probed = up.iter().map(|k| (k.to_string(), true)).collect();
    build_rows(
        &trust::KnownHosts { hosts },
        catalog,
        &discovered,
        &probed,
        seed,
    )
}

fn rows(hosts: Vec<trust::KnownHost>, adverts: Vec<discovery::DiscoveredHost>) -> Vec<HostRow> {
    rows_of(hosts, &PresetsFile::default(), adverts, &[], None)
}

/// A pinned record keys on its fingerprint, a placeholder on `addr:port`, and the store's
/// order is the list's order.
#[test]
fn saved_hosts_keep_store_order_and_key_on_the_pin() {
    let r = rows(
        vec![
            saved("Pinned", "10.0.0.2", &fp('a')),
            saved("Typed", "10.0.0.3", ""),
        ],
        vec![],
    );
    let keys: Vec<&str> = r.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(keys, [fp('a').as_str(), "10.0.0.3:9777"]);
    assert!(r.iter().all(|r| r.saved));
    assert!(r[0].paired && !r[1].paired);
}

/// A live advert wins where it speaks; the record's own value stands where it is silent, and
/// the default port is the last rung.
#[test]
fn an_advert_wins_where_it_speaks_and_the_record_where_it_is_silent() {
    let known = trust::KnownHost {
        mgmt_port: Some(47993),
        os: "linux/arch".into(),
        ..saved("Desk", "10.0.0.2", &fp('a'))
    };
    let speaks = discovery::DiscoveredHost {
        mgmt_port: Some(47991),
        os: "windows".into(),
        ..advert("Desk", "10.0.0.2", &fp('a'))
    };
    let r = rows(vec![known.clone()], vec![speaks]);
    assert_eq!((r[0].mgmt_port, r[0].os.as_str()), (47991, "windows"));

    let silent = advert("Desk", "10.0.0.2", &fp('a'));
    let r = rows(vec![known], vec![silent]);
    assert_eq!((r[0].mgmt_port, r[0].os.as_str()), (47993, "linux/arch"));

    let r = rows(vec![saved("Bare", "10.0.0.4", &fp('b'))], vec![]);
    assert_eq!(r[0].mgmt_port, library::DEFAULT_MGMT_PORT);
}

/// The other OS of a dual-boot box answers at the saved host's address with its own pin. It
/// is a host to add, not the one already saved; the saved host's own advert adds nothing.
#[test]
fn a_second_os_at_one_address_is_discovered_not_covered() {
    let r = rows(
        vec![saved("Windows", "10.0.0.2", &fp('a'))],
        vec![
            advert("Windows", "10.0.0.2", &fp('a')),
            advert("Linux", "10.0.0.2", &fp('b')),
        ],
    );
    assert_eq!(r.len(), 2);
    assert_eq!((r[1].key.as_str(), r[1].saved), (fp('b').as_str(), false));
    assert!(!r[1].paired && r[1].actions.is_empty());
}

/// An advert whose host has moved to a new lease is still the saved host: the pin decides.
#[test]
fn a_saved_host_that_moved_is_not_listed_twice() {
    let r = rows(
        vec![saved("Desk", "10.0.0.2", &fp('a'))],
        vec![advert("Desk", "10.0.0.77", &fp('a'))],
    );
    assert_eq!(r.len(), 1);
}

#[test]
fn discovered_hosts_follow_the_saved_ones_sorted_by_name() {
    let r = rows(
        vec![saved("Saved", "10.0.0.2", &fp('a'))],
        vec![
            advert("zeta", "10.0.0.5", &fp('c')),
            advert("Alpha", "10.0.0.6", &fp('d')),
            advert("beta", "10.0.0.7", &fp('e')),
        ],
    );
    let names: Vec<&str> = r.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["Saved", "Alpha", "beta", "zeta"]);
}

/// Presence is the sweep's answer. An advert outlives a host that went to sleep, and a
/// sleeping saved host with a MAC is the one that can be woken.
#[test]
fn presence_comes_from_the_sweep_and_waking_needs_a_mac() {
    let with_mac = trust::KnownHost {
        mac: vec!["aa:bb:cc:dd:ee:ff".into()],
        ..saved("Desk", "10.0.0.2", &fp('a'))
    };
    let no_mac = saved("Den", "10.0.0.3", &fp('b'));
    let catalog = PresetsFile::default();
    let adverts = vec![advert("Desk", "10.0.0.2", &fp('a'))];

    let r = rows_of(
        vec![with_mac.clone(), no_mac.clone()],
        &catalog,
        adverts.clone(),
        &[],
        None,
    );
    assert_eq!((r[0].online, r[0].can_wake), (false, true));
    assert_eq!((r[1].online, r[1].can_wake), (false, false));

    let up = [fp('a')];
    let up: Vec<&str> = up.iter().map(String::as_str).collect();
    let r = rows_of(vec![with_mac, no_mac], &catalog, adverts, &up, None);
    assert_eq!((r[0].online, r[0].can_wake), (true, false));
}

/// A host's pinned cards follow it, share its live state under their own key, and drop a
/// preset that is gone or listed twice. The binding shows on the host's own tile only.
#[test]
fn pinned_cards_follow_their_host_and_share_its_state() {
    let (night, day, bound) = (preset("night"), preset("day"), preset("bound"));
    let catalog = PresetsFile {
        presets: vec![night.clone(), day.clone(), bound.clone()],
        ..Default::default()
    };
    let host = trust::KnownHost {
        preset_id: Some(bound.id.clone()),
        pinned_presets: vec![
            night.id.clone(),
            "gone".into(),
            night.id.clone(),
            day.id.clone(),
        ],
        ..saved("Desk", "10.0.0.2", &fp('a'))
    };
    let up = [fp('a')];
    let up: Vec<&str> = up.iter().map(String::as_str).collect();
    let r = rows_of(vec![host], &catalog, vec![], &up, None);

    let keys: Vec<String> = r.iter().map(|r| r.key.clone()).collect();
    assert_eq!(
        keys,
        [
            fp('a'),
            pinned_key(&fp('a'), &night.id),
            pinned_key(&fp('a'), &day.id)
        ]
    );
    assert_eq!(r[0].pin, None);
    assert_eq!(
        r[0].bound_preset.as_ref().map(|p| p.id.as_str()),
        Some("id-bound")
    );
    assert_eq!(r[1].pin.as_ref().map(|p| p.id.as_str()), Some("id-night"));
    assert!(r[1].bound_preset.is_none());
    assert!(r.iter().all(|r| r.online && r.fp_hex == fp('a')));
}

/// A `--browse` target that nothing covers yet stays on the list, up when the sweep says so.
/// A record or an advert at its address takes its place.
#[test]
fn a_seed_stays_until_the_store_or_an_advert_covers_it() {
    let seed = HostRow {
        key: "10.0.0.9:9777".into(),
        addr: "10.0.0.9".into(),
        port: 9777,
        ..Default::default()
    };
    let catalog = PresetsFile::default();
    let r = rows_of(vec![], &catalog, vec![], &["10.0.0.9:9777"], Some(&seed));
    assert_eq!(r.len(), 1);
    assert!(r[0].online);

    let covered = rows_of(
        vec![saved("Typed", "10.0.0.9", "")],
        &catalog,
        vec![],
        &[],
        Some(&seed),
    );
    assert_eq!(covered.len(), 1);
    assert!(covered[0].saved);

    let advertised = rows_of(
        vec![],
        &catalog,
        vec![advert("Desk", "10.0.0.9", &fp('a'))],
        &[],
        Some(&seed),
    );
    assert_eq!(advertised.len(), 1);
    assert!(!advertised[0].saved);
}

/// A row key names a pinned record by its fingerprint and a placeholder by `addr:port`; a
/// pinned record at that address is another identity. A card's key carries its preset past
/// a NUL, which the lookup drops.
#[test]
fn a_row_key_finds_its_record() {
    let known = trust::KnownHosts {
        hosts: vec![
            saved("Pinned", "10.0.0.2", &fp('a')),
            saved("Typed", "10.0.0.2", ""),
            saved("Other", "10.0.0.3", &fp('b')),
        ],
    };
    assert_eq!(index_for_key(&known, &fp('a')), Some(0));
    assert_eq!(index_for_key(&known, "10.0.0.2:9777"), Some(1));
    assert_eq!(
        index_for_key(&known, &pinned_key(&fp('b'), "night")),
        Some(2)
    );
    assert_eq!(index_for_key(&known, "10.0.0.3:9777"), None);
    assert_eq!(index_for_key(&known, &fp('c')), None);
}

/// The rows every client builds from its saved and discovered hosts, from the vector file the
/// Apple and Android producers replay: store order, pins, advert fallbacks, a dual-boot
/// neighbour.
#[test]
fn the_shared_host_row_vectors_agree() {
    let raw = include_str!("../../../shared/host-row-vectors.json");
    let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
    let cases = file["cases"].as_array().expect("cases");
    assert!(!cases.is_empty());
    let text = |v: &serde_json::Value, k: &str| v[k].as_str().unwrap().to_string();
    let texts = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect()
    };
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let catalog = PresetsFile {
            presets: texts(&case["presets"])
                .into_iter()
                .map(|id| StreamPreset {
                    id: id.clone(),
                    ..StreamPreset::new(id)
                })
                .collect(),
            ..Default::default()
        };
        let hosts = case["saved"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| trust::KnownHost {
                id: Some(text(h, "id")),
                mac: texts(&h["mac"]),
                os: text(h, "os"),
                mgmt_port: h["mgmt_port"].as_u64().map(|p| p as u16),
                last_used: h["last_used"].as_u64(),
                pinned_presets: texts(&h["pins"]),
                ..saved(&text(h, "name"), &text(h, "addr"), &text(h, "fp"))
            })
            .collect();
        let adverts = case["discovered"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| discovery::DiscoveredHost {
                port: d["port"].as_u64().unwrap() as u16,
                mgmt_port: d["mgmt_port"].as_u64().map(|p| p as u16),
                os: text(d, "os"),
                ..advert(&text(d, "name"), &text(d, "addr"), &text(d, "fp"))
            })
            .collect();
        // The vector lists the saved records that answer by id; every advert is up, as it is
        // when the sweep reaches the host it found. The sweep keys its answers by row key.
        let reachable = texts(&case["online"]);
        let key_of = |fp: &str, addr: &str| -> String {
            if fp.is_empty() {
                format!("{addr}:9777")
            } else {
                fp.to_string()
            }
        };
        let mut up: Vec<String> = case["saved"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| reachable.contains(&text(h, "id")))
            .map(|h| key_of(&text(h, "fp"), &text(h, "addr")))
            .collect();
        up.extend(
            case["discovered"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| key_of(&text(d, "fp"), &text(d, "addr"))),
        );
        let up: Vec<&str> = up.iter().map(String::as_str).collect();
        let got = rows_of(hosts, &catalog, adverts, &up, None);

        let want = case["rows"].as_array().unwrap();
        assert_eq!(got.len(), want.len(), "{name} row count");
        for (i, (got, want)) in got.iter().zip(want).enumerate() {
            let at = format!("{name} row {i}");
            assert_eq!(got.key, want["key"].as_str().unwrap(), "{at} key");
            assert_eq!(got.saved, want["saved"].as_bool().unwrap(), "{at} saved");
            assert_eq!(got.online, want["online"].as_bool().unwrap(), "{at} online");
            assert_eq!(
                got.can_wake,
                want["can_wake"].as_bool().unwrap(),
                "{at} can_wake"
            );
            assert_eq!(
                u64::from(got.mgmt_port),
                want["mgmt_port"].as_u64().unwrap(),
                "{at} mgmt"
            );
            assert_eq!(got.os, want["os"].as_str().unwrap(), "{at} os");
            assert_eq!(got.last_used, want["last_used"].as_u64(), "{at} last_used");
            let pin = got.pin.as_ref().map(|p| p.id.as_str());
            assert_eq!(pin, want["pin"].as_str(), "{at} pin");
        }
    }
}

const LOOPBACK: &str = "127.0.0.1";

/// A service over `library`, with an identity of its own, so its workers dial for real.
fn service(library: &LibraryShared) -> ServiceState {
    ServiceState {
        console: ConsoleShared::default(),
        library: library.clone(),
        bus: pf_console_ui::ConsoleBus::default(),
        identity: punktfunk_core::quic::endpoint::generate_identity().expect("identity"),
        seed: None,
        discovered: HashMap::new(),
        probed: Arc::default(),
        probe_inflight: Arc::default(),
        last_probe: Some(Instant::now()),
        wake_cancel: None,
        pair_gen: Arc::default(),
        rescan: None,
        stores: Stores::default(),
        learn_due: false,
    }
}

const TITLE: &str = "steam:1";

/// `req`'s shelf, as the shell opens it: one title, which its host reports up.
fn open_shelf(library: &LibraryShared, req: &RequestKey) {
    library.begin_for_test(req);
    let title = LibraryGame {
        id: TITLE.into(),
        title: "One".into(),
        store: "steam".into(),
        launcher: false,
        icon: String::new(),
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
        endable: false,
        install: None,
        overrides: None,
    };
    library.set_games(req, vec![title]);
    library.set_running(&[library::RunningGame {
        app_id: Some(TITLE.into()),
        title: "One".into(),
        state: "running".into(),
        awaiting_window: false,
        session_id: None,
        endable: false,
    }]);
}

/// What a test compares: the phase, and each title's Resume badge.
fn shelf(library: &LibraryShared) -> (LibraryPhase, Vec<(String, bool)>) {
    let (phase, games) = library.shelf_for_test();
    (
        phase,
        games.into_iter().map(|g| (g.id, g.running)).collect(),
    )
}

/// Close each connection the worker opens until it ends: every request fails, as against a
/// host that dropped off the network.
fn hang_up(host: TcpListener, worker: Worker) {
    let worker = worker.expect("the worker starts");
    host.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !worker.is_finished() {
        assert!(Instant::now() < deadline, "the worker never gave up");
        match host.accept() {
            Ok((conn, _)) => drop(conn),
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    worker.join().unwrap();
}

/// Starts one worker against host A: the service, A's management port, A's fetch.
type Start = fn(&ServiceState, u16, &RequestKey) -> Worker;

/// Whether the worker `start` spawns writes the shelf once host A fails to answer. The player
/// is on A's shelf when it starts; with `moved` they open B's, the same title up, before A
/// gives up.
fn writes_shelf(moved: bool, start: Start) -> bool {
    let library = LibraryShared::default();
    let svc = service(&library);
    let a = RequestKey {
        host: fp('a'),
        epoch: 1,
    };
    open_shelf(&library, &a);
    // Listening, so the worker connects and waits for a handshake until `hang_up`.
    let host = TcpListener::bind((LOOPBACK, 0)).unwrap();
    let port = host.local_addr().unwrap().port();
    let worker = start(&svc, port, &a);
    if moved {
        let b = RequestKey {
            host: fp('b'),
            epoch: 2,
        };
        open_shelf(&library, &b);
    }
    let before = shelf(&library);
    hang_up(host, worker);
    shelf(&library) != before
}

/// The worker writes the shelf it was started for, and nothing once the player opened another.
fn lands_only_on_its_own_shelf(start: Start) {
    assert!(
        writes_shelf(false, start),
        "its own shelf kept its old state"
    );
    assert!(
        !writes_shelf(true, start),
        "another host's shelf took its answer"
    );
}

/// A `/status` read writes the badges of the shelf it was asked for, and nothing once the
/// player has opened another host's.
#[test]
fn a_running_refresh_a_newer_fetch_outlived_writes_nothing() {
    lands_only_on_its_own_shelf(|svc, port, _| svc.refresh_running(LOOPBACK.into(), port, fp('a')));
}

/// The re-read after ending a game, likewise.
#[test]
fn an_end_game_a_newer_fetch_outlived_writes_nothing() {
    lands_only_on_its_own_shelf(|svc, port, _| {
        svc.end_game(LOOPBACK.into(), port, fp('a'), TITLE.into(), "One".into())
    });
}

/// The re-read after an install change, likewise.
#[test]
fn an_install_change_a_newer_fetch_outlived_writes_nothing() {
    lands_only_on_its_own_shelf(|svc, port, _| {
        svc.change_install(
            LOOPBACK.into(),
            port,
            fp('a'),
            TITLE.into(),
            "One".into(),
            library::InstallAction::Install,
        )
    });
}

/// A library fetch that fails shows its error on its own shelf, and leaves another host's
/// alone.
#[test]
fn a_library_fetch_a_newer_fetch_outlived_writes_nothing() {
    lands_only_on_its_own_shelf(|svc, port, a| {
        spawn_fetch(
            svc.library.clone(),
            LOOPBACK.into(),
            (0, port),
            svc.identity.clone(),
            fp('a'),
            Vec::new(),
            a.clone(),
        )
    });
}

/// Where the console opens: an explicit target opens its library when paired and Home seeded
/// into the list when not; a bare launch follows the start policy, which labels and seeds
/// nothing; the fake-library hook makes any target paired and, bare, opens the demo host.
#[test]
fn the_console_opens_where_the_target_or_the_policy_says() {
    let known = trust::KnownHosts {
        hosts: vec![
            saved("Desk", "10.0.0.2", &fp('a')),
            saved("Spare", "10.0.0.3", ""),
        ],
    };
    let policy = |start_in: &str| {
        let start_in = start_in.to_string();
        move || trust::Settings {
            start_in,
            ..Default::default()
        }
    };
    let e = console_entry(Some("10.0.0.2"), &known, false, policy("hosts"));
    assert!(matches!(e.entry, ConsoleEntry::Library(ref r) if r.name == "Desk" && r.paired));
    assert_eq!(e.label.as_deref(), Some("Desk"));
    assert!(e.seed.is_none(), "a saved host needs no seed");
    let e = console_entry(Some("10.0.0.3"), &known, false, policy("hosts"));
    assert!(matches!(e.entry, ConsoleEntry::Home));
    assert_eq!(e.label.as_deref(), Some("Spare"));
    assert!(e.seed.is_none());
    let e = console_entry(Some("10.0.0.9:9778"), &known, false, policy("hosts"));
    assert!(matches!(e.entry, ConsoleEntry::Home));
    assert_eq!(e.label.as_deref(), Some("10.0.0.9"));
    let seed = e.seed.expect("an unknown target is seeded");
    assert_eq!(
        (seed.addr.as_str(), seed.port, seed.paired, seed.saved),
        ("10.0.0.9", 9778, false, false)
    );
    let e = console_entry(Some("10.0.0.9"), &known, true, policy("hosts"));
    assert!(matches!(e.entry, ConsoleEntry::Library(ref r) if r.paired && !r.saved));
    assert!(
        e.seed.is_some_and(|r| r.paired),
        "the seed carries the fake pairing"
    );
    let e = console_entry(None, &known, true, policy("hosts"));
    assert!(matches!(e.entry, ConsoleEntry::Library(ref r) if r.name == "Demo Host"));
    assert!(e.label.is_none() && e.seed.is_none());
    let e = console_entry(None, &known, false, policy("hosts"));
    assert!(matches!(e.entry, ConsoleEntry::Home));
    let e = console_entry(None, &known, false, policy("library"));
    assert!(matches!(e.entry, ConsoleEntry::Library(ref r) if r.name == "Desk"));
    let e = console_entry(None, &known, false, policy("stream"));
    assert!(matches!(e.entry, ConsoleEntry::Stream(ref r) if r.name == "Desk"));
    assert!(e.label.is_none() && e.seed.is_none());
}

/// The shell's titles keep what the host set for each, so the hold and the settings rows
/// can say so.
#[test]
fn the_shell_model_keeps_a_titles_overrides() {
    let games: Vec<library::GameEntry> = serde_json::from_str(
        r#"[{"id":"steam:570","store":"steam","title":"Dota 2","overrides":{"lines":1080}},
            {"id":"steam:70","store":"steam","title":"Half-Life"}]"#,
    )
    .unwrap();
    let model = to_model(&games);
    assert_eq!(
        model[0].overrides.as_ref().and_then(|o| o.lines),
        Some(1080)
    );
    assert!(model[1].overrides.is_none());
}

/// The input test hears Xbox names, a paddle by its drawn name, and a trigger past half as held.
#[test]
fn a_pad_reading_names_what_the_input_test_draws() {
    let r = PadReading {
        buttons: wire::BTN_B | wire::BTN_BACK | wire::BTN_PADDLE2,
        axes: [0.0, -1.0, 0.0, 0.0, 0.8, 0.3],
    };
    let s = pad_test_state(r);
    assert_eq!(s.held, ["B", "Back", "L4", "LT"]);
    assert_eq!(s.axes[1], ("LY".to_string(), -1.0));
    assert_eq!(s.axes[5], ("RT".to_string(), 0.3));
}
