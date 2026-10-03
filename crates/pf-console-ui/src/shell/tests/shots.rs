use super::*;

/// Ignored eyeball dump. `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --release -- --ignored dump`.
/// CPU raster: SkSL aurora, layers, and text run without a GPU.
#[test]
#[ignore]
fn dump_console_screens() {
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1280, 800);
    let pads: Vec<PadInfo> = Vec::new();
    let dump = |shell: &mut Shell, frames: usize, sleep_ms: u64, name: &str, pad: bool| {
        // Fixed step = sleep + ~4 ms raster, so two dumps compare independent of load.
        let step = sleep_ms as f64 / 1000.0 + 0.004;
        shell.fake_clock = Some((shell.fake_clock.map_or(0.0, |(t, _)| t), step));
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        for _ in 0..frames {
            shell.render(
                surface.canvas(),
                w as u32,
                h as u32,
                &fonts,
                pad.then_some("Xbox Wireless Controller"),
                pad.then_some(GamepadPref::Xbox360),
                &pads,
            );
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };

    let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    dump(&mut s, 40, 8, "01-home", true);
    // The focus plate between two tiles, then landed with the sweep on its rim.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    dump(&mut s, 6, 8, "01c-home-plate-travel", true);
    dump(&mut s, 44, 8, "01d-home-plate-sweep", true);
    s.handle_menu(MenuEvent::Move(MenuDir::Left));
    dump(&mut s, 40, 8, "_settle-plate", true);

    // Y on the focused saved tile. Eyeball with 01-home: that frame carries the Options hint.
    s.handle_menu(MenuEvent::Secondary);
    dump(&mut s, 40, 8, "01b-host-options", true);
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Move(MenuDir::Down));
    }
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "01f-host-details", true);
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 20, 8, "_settle0", true);
    // Up from the row: the plate on the Hosts pill.
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    dump(&mut s, 40, 8, "01e-strip", true);
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    dump(&mut s, 20, 8, "_settle-strip", true);

    // A few fast frames land around p ≈ 0.4 — both layers visible.
    s.handle_menu(MenuEvent::Tertiary);
    dump(&mut s, 3, 25, "02-transition", true);
    dump(&mut s, 40, 8, "03-settings", true);

    // Interface tab leads with Background. Palettes are set by id, not Confirm counts,
    // so reordering the table cannot shoot the wrong one. Accent, ink and scrim move
    // together: pale palettes need dark text on them.
    for _ in 0..5 {
        next_section(&mut s);
    }
    // Confirm on Background: the cards. A pick recolours the field behind them.
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "03d-background", true);
    for _ in 0..2 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "03e-background-pick", true);
    // Five rows down at four across lands on a pale card: ink and scrims flip live.
    for _ in 0..5 {
        s.handle_menu(MenuEvent::Move(MenuDir::Down));
    }
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "03f-background-pale", true);
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 20, 8, "_settle-background", true);
    for id in [
        "violet", "oled", "crimson", "midnight", "paper", "coral", "sky",
    ] {
        s.settings.ui_palette = id.to_string();
        dump(&mut s, 40, 8, &format!("03-settings-{id}"), true);
    }
    // Deep in the longest section: the rows run on under the strips, blurring as they go.
    s.settings.ui_palette = "violet".to_string();
    for _ in 0..12 {
        s.handle_menu(MenuEvent::Move(MenuDir::Down));
    }
    // A short window, so the rows overflow into both bands.
    let mut short = skia_safe::surfaces::raster_n32_premul((w, 480)).unwrap();
    for _ in 0..60 {
        s.render(short.canvas(), w as u32, 480, &fonts, None, None, &pads);
    }
    let png = short
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap();
    std::fs::write(format!("{dir}/03b-settings-blur.png"), png.as_bytes()).unwrap();
    for _ in 0..6 {
        s.handle_menu(MenuEvent::Move(MenuDir::Up));
    }
    for _ in 0..60 {
        s.render(short.canvas(), w as u32, 480, &fonts, None, None, &pads);
    }
    let png = short
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap();
    std::fs::write(format!("{dir}/03c-settings-blur-mid.png"), png.as_bytes()).unwrap();
    for _ in 5..crate::screens::settings::TAB_COUNT {
        next_section(&mut s);
    }
    // Home at full contrast under a few palettes: the backdrop's loudest form.
    s.switch_tab(Tab::Hosts);
    dump(&mut s, 20, 8, "_settle", true);
    for id in ["dusk", "coral", "paper"] {
        s.settings.ui_palette = id.to_string();
        dump(&mut s, 40, 8, &format!("01-home-{id}"), true);
    }
    s.settings.ui_palette = "violet".to_string();
    dump(&mut s, 20, 8, "_settle2", true);
    s.handle_menu(MenuEvent::Tertiary); // back into Settings for the scenes below
    dump(&mut s, 20, 8, "_settle3", true);

    // Add Host with the keyboard tray; no pad so the glyphs are keyboard-style.
    s.switch_tab(Tab::Hosts);
    dump(&mut s, 40, 8, "_back", true);
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "04-addhost", false);
    s.handle_menu(MenuEvent::Confirm); // open the Name keyboard
    for ev in [
        MenuEvent::Move(MenuDir::Down),
        MenuEvent::Confirm,
        MenuEvent::Confirm,
    ] {
        s.handle_menu(ev);
    }
    dump(&mut s, 40, 8, "05-addhost-keyboard", false);

    s.handle_menu(MenuEvent::Back); // close keyboard
    s.handle_menu(MenuEvent::Back); // leave add-host
    dump(&mut s, 40, 8, "_back2", true);
    s.handle_menu(MenuEvent::Move(MenuDir::Left)); // onto "steambox"
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 40, 8, "06-pair", true);

    library.set_games(
        [
            "Hades II",
            "Elden Ring",
            "Hollow Knight",
            "Baldur's Gate 3",
            "Celeste",
            "Deep Rock Galactic",
            "Portal 2",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| crate::library::LibraryGame {
            id: format!("steam:{i}"),
            title: (*t).to_string(),
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
        })
        .collect(),
    );
    // The combined home: the resting card's games under the row, then focus on them.
    {
        let console7 = ConsoleShared::default();
        console7.set_hosts(hosts());
        let home = vec![Screen::Home(HomeScreen::new())];
        let bus7 = ConsoleBus::default();
        let mut s7 = Shell::new(console7, library.clone(), bus7, test_options(), home).unwrap();
        dump(&mut s7, 80, 8, "01g-home-games", true);
        s7.handle_menu(MenuEvent::Move(MenuDir::Down));
        dump(&mut s7, 40, 8, "01h-home-games-focus", true);
    }

    // Fresh shell per scene: entrance and bar focus are per-shell and cannot be rewound.
    // The coverflow, which these scenes were drawn against; the Games tab has its own.
    let shelf_shell = || {
        let console2 = ConsoleShared::default();
        console2.set_hosts(hosts());
        let mut shell = Shell::new(
            console2,
            library.clone(),
            ConsoleBus::default(),
            test_options(),
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&hosts()[0])),
            ],
        )
        .unwrap();
        shell.settings.library_view = "shelf".into();
        shell
    };
    let mut s2 = shelf_shell();
    s2.handle_menu(MenuEvent::Move(MenuDir::Right));
    s2.handle_menu(MenuEvent::Move(MenuDir::Right));
    // 80 frames, not 40: no art means the 400 ms art-wait deadline, and 40×8 ms can
    // finish inside it and dump the spinner as the coverflow.
    dump(&mut s2, 80, 8, "07-library", true);

    // The launch hold, mid-flight and settled. Confirm on a settled shelf raises it, so
    // these two frames are the cover leaving its tile and the screen it lands on — the
    // one sequence a still cannot show by itself.
    {
        let mut s5 = shelf_shell();
        s5.handle_menu(MenuEvent::Move(MenuDir::Right));
        dump(&mut s5, 80, 8, "_07d-settle", true);
        s5.handle_menu(MenuEvent::Confirm);
        // ~90 ms in: the spring is a third of the way over and a third of the way round.
        dump(&mut s5, 3, 30, "07d-launch-hold-flight", true);
        dump(&mut s5, 40, 16, "07e-launch-hold", true);
        s5.session_streaming();
        dump(&mut s5, 20, 16, "07f-launch-hold-streaming", true);
    }

    // Sort/view bar focused: the only state that draws the accent wash. Both palette
    // poles — `accent(0.14)` reads differently over dark than pale.
    for (name, palette) in [
        ("07c-library-bar", "violet"),
        ("07c-library-bar-sky", "sky"),
    ] {
        let mut s4 = shelf_shell();
        s4.settings.ui_palette = palette.to_string();
        // Settle the shelf first (same 400 ms art-wait). Up before the field exists is swallowed.
        dump(&mut s4, 80, 8, "_07c-settle", true);
        s4.handle_menu(MenuEvent::Move(MenuDir::Up));
        dump(&mut s4, 20, 8, name, true);
    }

    // The Games tab with every section full: two played titles, a favorite, a launcher.
    // Then Customize, a row picked up.
    {
        let games_lib = LibraryShared::default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let played = |ago: u64| {
            Some(pf_client_core::library::GameStats {
                last_played_unix_ms: now - ago,
                ..Default::default()
            })
        };
        let titles = [
            "Steam",
            "Hades II",
            "Elden Ring",
            "Hollow Knight",
            "Celeste",
            "Tunic",
        ];
        let mut list: Vec<crate::library::LibraryGame> = (titles.iter().enumerate())
            .map(|(i, t)| crate::library::LibraryGame {
                id: format!("steam:{i}"),
                title: (*t).to_string(),
                store: "steam".into(),
                launcher: i == 0,
                icon: if i == 0 {
                    "steam".into()
                } else {
                    String::new()
                },
                platform: None,
                developer: None,
                year: None,
                genres: Vec::new(),
                stats: None,
                running: false,
                endable: false,
                install: None,
            })
            .collect();
        list[2].stats = played(2 * 3_600_000);
        list[3].stats = played(3 * 86_400_000);
        games_lib.set_games(list);
        let console6 = ConsoleShared::default();
        console6.set_hosts(hosts());
        let mut s6 = Shell::new(
            console6,
            games_lib,
            ConsoleBus::default(),
            test_options(),
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&hosts()[0])),
            ],
        )
        .unwrap();
        crate::library::toggle_favorite(&mut s6.settings, &hosts()[0].fp_hex, "steam:4");
        dump(&mut s6, 80, 8, "_07g-settle", true);
        s6.handle_menu(MenuEvent::Move(MenuDir::Down)); // Recently played
        dump(&mut s6, 40, 8, "07g-games-tab", true);
        s6.handle_menu(MenuEvent::Move(MenuDir::Up));
        s6.handle_menu(MenuEvent::Move(MenuDir::Up)); // the chips
        for _ in 0..3 {
            s6.handle_menu(MenuEvent::Move(MenuDir::Right)); // to Customize
        }
        s6.handle_menu(MenuEvent::Confirm);
        finish_motion(&mut s6);
        s6.handle_menu(MenuEvent::Move(MenuDir::Down));
        s6.handle_menu(MenuEvent::Confirm); // pick up Recently played
        dump(&mut s6, 40, 8, "07h-customize", true);
    }

    // `adopt_art` is a one-shot at the Y press: push art and give the shelf frames to
    // decode it first, or every tile is a monogram. Art-before-list is also what the
    // fake-library hook does, which masks the entrance defect — these scenes are about
    // the collection tile, not the entrance.
    for (name, palette) in [
        ("07b-collections", "violet"),
        ("07b-collections-sky", "sky"),
    ] {
        let (mut s3, _c3, _l3) = collections_shell();
        s3.settings.ui_palette = palette.to_string();
        dump(&mut s3, 12, 8, &format!("_{name}-decode"), true);
        s3.handle_menu(MenuEvent::Secondary);
        dump(&mut s3, 40, 8, name, true);
    }
    // Nothing decoded: ghost slots and the monogram badge — the permanent look of
    // art-less ROM entries. Pale, where a hardcoded face strands its initials.
    {
        let (mut s3, _c3, _l3) = collections_shell_no_art();
        s3.settings.ui_palette = "sky".to_string();
        dump(&mut s3, 12, 8, "_noart-settle", true);
        s3.handle_menu(MenuEvent::Secondary);
        dump(&mut s3, 40, 8, "07b-collections-noart", true);
    }

    console.set_wake(Some(WakeStatus {
        key: "bb22".into(),
        name: "Office Tower".into(),
        seconds: 12,
        timed_out: false,
        online: false,
        then_connect: true,
    }));
    dump(&mut s, 10, 8, "08-waking", true);
    console.set_wake(Some(WakeStatus {
        key: "bb22".into(),
        name: "Office Tower".into(),
        seconds: 90,
        timed_out: true,
        online: false,
        then_connect: true,
    }));
    dump(&mut s, 10, 8, "08b-wake-timed-out", true);
    console.set_wake(None);
    s.set_connecting(Some("Elden Ring".into()));
    dump(&mut s, 10, 8, "09-connecting", true);
    s.set_connecting(None);
    s.session_failed("Connection timed out");
    dump(&mut s, 10, 8, "10-toast", true);

    // Android + keys: OK/↩ badges, section pointer, hidden Y/X, remote chip. Platform
    // flip is legends-only; the stack was built desktop.
    dump(&mut s, 30, 8, "_remote-settle", true);
    s.device.platform = crate::platform::Platform::Android;
    s.note_input_source(crate::console::InputSource::Keys);
    dump(&mut s, 40, 8, "11-home-remote", false);
    s.handle_menu(MenuEvent::Tertiary);
    dump(&mut s, 40, 8, "11b-settings-remote", false);
}

/// A 2:3 poster, PNG-encoded, colour from `seed`. Real bytes: `LibraryScreen` feeds
/// these to `Image::from_encoded`, and a decode miss looks like a tile with no cover.
fn poster_png(seed: usize) -> Vec<u8> {
    let mut surface = skia_safe::surfaces::raster_n32_premul((60, 90)).unwrap();
    let hue = [
        (0.85, 0.30, 0.35),
        (0.30, 0.55, 0.85),
        (0.35, 0.75, 0.45),
        (0.85, 0.65, 0.25),
    ][seed % 4];
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(hue.0, hue.1, hue.2, 1.0));
    // Darker band on the lower third so a flipped or wrong-aspect cover is visible.
    surface.canvas().draw_rect(
        skia_safe::Rect::from_xywh(0.0, 62.0, 60.0, 28.0),
        &crate::theme::fill(skia_safe::Color4f::new(
            hue.0 * 0.45,
            hue.1 * 0.45,
            hue.2 * 0.45,
            1.0,
        )),
    );
    surface
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap()
        .as_bytes()
        .to_vec()
}

fn platform_games() -> Vec<crate::library::LibraryGame> {
    [
        ("Gran Turismo 6", "PlayStation 3"),
        ("The Last of Us", "PlayStation 3"),
        ("Demon's Souls", "PlayStation 3"),
        ("Halo 3", "Xbox 360"),
        ("Fable II", "Xbox 360"),
        ("Super Metroid", "SNES"),
        ("Chrono Trigger", "SNES"),
        ("Sonic 2", "Mega Drive"),
    ]
    .iter()
    .enumerate()
    .map(|(i, (title, platform))| crate::library::LibraryGame {
        id: format!("rom:{i}"),
        title: (*title).to_string(),
        store: "rom-manager".into(),
        launcher: false,
        icon: String::new(),
        platform: Some((*platform).to_string()),
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
        endable: false,
        install: None,
    })
    .collect()
}

fn collections_shell_inner(
    with_art: bool,
) -> (Shell, ConsoleShared, crate::library::LibraryShared) {
    fake_home();
    let library = crate::library::LibraryShared::default();
    let games = platform_games();
    if with_art {
        for (i, g) in games.iter().enumerate() {
            library.push_art(g.id.clone(), poster_png(i));
        }
    }
    library.set_games(games);
    let console = ConsoleShared::default();
    console.set_hosts(hosts());
    let sh = Shell::new(
        console.clone(),
        library.clone(),
        ConsoleBus::default(),
        test_options(),
        vec![
            Screen::Home(HomeScreen::new()),
            Screen::Library(LibraryScreen::new(&hosts()[0])),
        ],
    )
    .unwrap();
    (sh, console, library)
}

fn collections_shell() -> (Shell, ConsoleShared, crate::library::LibraryShared) {
    collections_shell_inner(true)
}

fn collections_shell_no_art() -> (Shell, ConsoleShared, crate::library::LibraryShared) {
    collections_shell_inner(false)
}

/// Play Store TV captures. `PF_CONSOLE_STORE=<dir> cargo test -p pf-console-ui -- --ignored store_shots`.
/// Android TV passes scale 0 (`SkiaConsoleShell.kt`), so a 1080p panel takes the couch
/// formula: k = 1080 / 800 = 1.35, the same `render` derives for a plain 1920×1080 viewport.
#[test]
#[ignore]
fn store_shots() {
    let dir = std::env::var("PF_CONSOLE_STORE").expect("set PF_CONSOLE_STORE to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let pads = store_pads();
    let frames = |s: &mut Shell, n: usize| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((1920, 1080)).unwrap();
        for _ in 0..n {
            s.render(
                surface.canvas(),
                1920,
                1080,
                &fonts,
                Some(&pads[0].name),
                Some(GamepadPref::Xbox360),
                &pads,
            );
        }
        surface
    };
    let save = |mut surface: skia_safe::Surface, name: &str| {
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };
    // Fixed 60 Hz step: springs and the aurora land on the same frame every run.
    let store_shell = |stack: Vec<Screen>, library: LibraryShared| {
        fake_home();
        let console = ConsoleShared::default();
        console.set_hosts(store_hosts());
        let mut s = Shell::new(
            console,
            library,
            ConsoleBus::default(),
            test_options(),
            stack,
        )
        .unwrap();
        s.device.platform = crate::platform::Platform::Android;
        s.settings.ui_palette = "violet".into();
        s.fake_clock = Some((0.0, 1.0 / 60.0));
        s
    };

    // Art before the list, so the shelf decodes it before the entrance arms.
    let store_library = || {
        let library = LibraryShared::default();
        for (i, (title, ..)) in STORE_TITLES.iter().enumerate() {
            library.push_art(format!("steam:{i}"), store_poster(i, title, &fonts));
        }
        library.set_games(store_games());
        library
    };

    // The home draws the focused host's games under its row, so it needs the catalog too.
    let mut s = store_shell(vec![Screen::Home(HomeScreen::new())], store_library());
    frames(&mut s, 30);
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    save(frames(&mut s, 90), "tv-console-home");

    let shelf = || {
        let host = store_hosts()[2].clone();
        let mut s = store_shell(
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&host)),
            ],
            store_library(),
        );
        frames(&mut s, 60);
        // Down past the Desktops, Launchers and Collections bands onto the first title.
        for _ in 0..3 {
            s.handle_menu(MenuEvent::Move(MenuDir::Down));
        }
        s
    };
    let mut s = shelf();
    save(frames(&mut s, 90), "tv-library");

    let mut s = shelf();
    frames(&mut s, 90);
    s.handle_menu(MenuEvent::Confirm);
    save(frames(&mut s, 120), "tv-console-launch");

    let mut s = store_shell(
        vec![
            Screen::Home(HomeScreen::new()),
            Screen::Players(crate::screens::players::PlayersScreen::new()),
        ],
        LibraryShared::default(),
    );
    save(frames(&mut s, 60), "tv-console-controllers");

    // Settings opens on the Stream tab. Explicit values, the stream shot's mode, over Native.
    let mut s = store_shell(
        vec![Screen::Home(HomeScreen::new())],
        LibraryShared::default(),
    );
    (s.settings.width, s.settings.height, s.settings.refresh_hz) = (1920, 1080, 60);
    s.settings.bitrate_kbps = 50_000;
    frames(&mut s, 30);
    s.handle_menu(MenuEvent::Tertiary);
    save(frames(&mut s, 90), "tv-console-settings");

    // Name filled, the address mid-entry on the keyboard tray.
    let mut s = store_shell(
        vec![
            Screen::Home(HomeScreen::new()),
            Screen::AddHost(crate::screens::add_host::AddHostScreen::new()),
        ],
        LibraryShared::default(),
    );
    frames(&mut s, 30);
    s.handle_menu(MenuEvent::Confirm);
    s.text_input("Guest Room PC");
    s.handle_menu(MenuEvent::Back);
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Confirm);
    s.text_input("192.168.1.40");
    save(frames(&mut s, 60), "tv-console-addhost");

    // PIN typed, focus on Pair.
    let studio = store_hosts()[6].clone();
    let mut s = store_shell(
        vec![
            Screen::Home(HomeScreen::new()),
            Screen::Pair(crate::screens::pair::PairScreen::new(
                &studio,
                "Living Room TV",
            )),
        ],
        LibraryShared::default(),
    );
    frames(&mut s, 30);
    s.handle_menu(MenuEvent::Confirm);
    s.text_input("4827");
    s.handle_menu(MenuEvent::Back);
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    save(frames(&mut s, 60), "tv-console-pair");
}

/// Battlestation sits third so the focused tile has neighbours on both sides.
fn store_hosts() -> Vec<HostRow> {
    let host = |name: &str, os: &str, octet: u8, paired: bool, online: bool| HostRow {
        key: if paired {
            format!("fp{octet}")
        } else {
            format!("192.168.1.{octet}:9777")
        },
        id: paired.then(|| format!("id{octet}")),
        addr: format!("192.168.1.{octet}"),
        fp_hex: if paired {
            format!("fp{octet}")
        } else {
            String::new()
        },
        paired,
        saved: paired,
        online,
        can_wake: paired && !online,
        os: os.into(),
        ..HostRow::fixture("", name)
    };
    let mut hosts = vec![
        host("Living Room PC", "linux/fedora/bazzite", 21, true, true),
        host("Office NUC", "linux/debian/ubuntu", 22, true, false),
        host("Battlestation", "windows", 20, true, true),
        host("Workshop", "linux/arch/cachyos", 23, true, true),
        host("Editing Rig", "windows", 24, true, false),
        host("Bedroom Mini", "linux/arch/steamos", 25, true, true),
        host("Studio PC", "windows", 30, false, true),
    ];
    hosts[2].running = "Aurora Drift".into();
    hosts
}

fn store_pads() -> Vec<PadInfo> {
    let pad = |name: &str, id: &str, pref: GamepadPref, percent: u8| PadInfo {
        name: name.into(),
        key: format!("{}:{name}", id.to_lowercase()),
        pref,
        steam_virtual: false,
        battery: Some(pf_client_core::menu_nav::PadBattery {
            percent,
            charging: false,
        }),
        detail: format!("{id} · gamepad · dpad"),
        forwarded: true,
        rumble: true,
    };
    vec![
        pad(
            "Xbox Wireless Controller",
            "045E:0B13",
            GamepadPref::Xbox360,
            80,
        ),
        pad(
            "DualSense Wireless Controller",
            "054C:0CE6",
            GamepadPref::DualSense,
            65,
        ),
    ]
}

/// Fictional titles and studios only: these ship in store listings.
const STORE_TITLES: [(&str, &str, u16, [&str; 2]); 7] = [
    ("Aurora Drift", "Lumen Forge", 2025, ["Racing", "Arcade"]),
    (
        "Starfall Vale",
        "Hollowpine",
        2024,
        ["Adventure", "Exploration"],
    ),
    ("Neon Circuit", "Relay Nine", 2025, ["Action", "Cyberpunk"]),
    ("Ember Keep", "Emberlight", 2023, ["Strategy", "Fantasy"]),
    (
        "Tidebound",
        "Tidewater Games",
        2024,
        ["Sailing", "Open world"],
    ),
    ("Glacier Run", "Northwind", 2022, ["Platformer", "Speedrun"]),
    ("Echo Station", "Signal Hill", 2025, ["Puzzle", "Sci-fi"]),
];

fn store_games() -> Vec<crate::library::LibraryGame> {
    let game = |id: String, title: &str, launcher: bool| crate::library::LibraryGame {
        id,
        title: title.into(),
        store: "steam".into(),
        launcher,
        icon: if launcher {
            "steam".into()
        } else {
            String::new()
        },
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
        endable: false,
        install: None,
    };
    let mut games = vec![game("steam:launcher".into(), "Steam", true)];
    games.extend(
        STORE_TITLES
            .iter()
            .enumerate()
            .map(
                |(i, (title, studio, year, genres))| crate::library::LibraryGame {
                    developer: Some((*studio).into()),
                    year: Some(*year),
                    genres: genres.iter().map(|g| (*g).into()).collect(),
                    ..game(format!("steam:{i}"), title, false)
                },
            ),
    );
    games
}

/// 600×900 poster in the Apple harness's style (`ShotPosterArt.swift`): dark sky, glowing
/// strokes, the title over a soft floor. Deterministic per `i`.
fn store_poster(i: usize, title: &str, fonts: &crate::theme::Fonts) -> Vec<u8> {
    use crate::theme::W;
    use skia_safe::{gradient, BlendMode, Canvas, Color4f, MaskFilter, Path, PathBuilder, Point};
    use std::f32::consts::PI;

    fn rgb(hex: u32, a: f32) -> Color4f {
        let ch = |s: u32| ((hex >> s) & 0xff) as f32 / 255.0;
        Color4f::new(ch(16), ch(8), ch(0), a)
    }
    fn vertical(c: &Canvas, y0: f32, y1: f32, colors: &[Color4f]) {
        let mut p = crate::theme::fill(Color4f::new(0.0, 0.0, 0.0, 1.0));
        p.set_shader(gradient::shaders::linear_gradient(
            (Point::new(0.0, y0), Point::new(0.0, y1)),
            &gradient::Gradient::new(
                gradient::Colors::new_evenly_spaced(colors, skia_safe::TileMode::Clamp, None),
                gradient::Interpolation::default(),
            ),
            None,
        ));
        c.draw_rect(skia_safe::Rect::from_ltrb(0.0, y0, 600.0, y1), &p);
    }
    /// Wide-faint to thin-bright, screen-blended: the neon trick every poster leans on.
    fn glow(c: &Canvas, path: &Path, width: f32, color: Color4f) {
        for (mult, alpha, blur) in [(2.6, 0.2, true), (1.3, 0.4, true), (0.55, 0.95, false)] {
            let mut p = crate::theme::stroke(Color4f { a: alpha, ..color }, width * mult);
            p.set_blend_mode(BlendMode::Screen);
            p.set_stroke_cap(skia_safe::PaintCap::Round);
            p.set_stroke_join(skia_safe::PaintJoin::Round);
            if blur {
                p.set_mask_filter(MaskFilter::blur(
                    skia_safe::BlurStyle::Normal,
                    width * mult * 0.5,
                    None,
                ));
            }
            c.draw_path(path, &p);
        }
    }
    fn dot(c: &Canvas, x: f32, y: f32, r: f32, color: Color4f) {
        let mut p = crate::theme::fill(color);
        p.set_shader(gradient::shaders::radial_gradient(
            (Point::new(x, y), r),
            &gradient::Gradient::new(
                gradient::Colors::new_evenly_spaced(
                    &[color, Color4f { a: 0.0, ..color }],
                    skia_safe::TileMode::Clamp,
                    None,
                ),
                gradient::Interpolation::default(),
            ),
            None,
        ));
        c.draw_circle((x, y), r, &p);
    }
    fn ridge(c: &Canvas, rnd: &mut dyn FnMut(f32, f32) -> f32, base: f32, color: Color4f) {
        let mut p = PathBuilder::new();
        p.move_to((0.0, 900.0));
        for s in 0..=10 {
            p.line_to((s as f32 * 60.0, base + rnd(-36.0, 36.0)));
        }
        p.line_to((600.0, 900.0));
        p.close();
        c.draw_path(&p.detach(), &crate::theme::fill(color));
    }

    // (sky top, horizon, glow) per title.
    let (top, horizon, hue) = [
        (0x0B0830, 0x2B2475, 0x8F7BFF),
        (0x1D0818, 0xB8467E, 0xFFD3E6),
        (0x04161C, 0x0C3440, 0x35D0C5),
        (0x1A0703, 0x9A3E16, 0xFFB067),
        (0x03132A, 0x0E4A6E, 0x6FD8F5),
        (0x0A1530, 0x3A6FA0, 0xDDF4FF),
        (0x14052A, 0x6A1B7A, 0xFF6AD5),
    ][i % 7];
    let (glow_c, sky) = (rgb(hue, 1.0), rgb(top, 1.0));
    let shade = |f: f32| Color4f::new(sky.r * f, sky.g * f, sky.b * f, 1.0);
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ i as u64;
    let mut rnd = move |lo: f32, hi: f32| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        lo + (seed >> 40) as f32 / (1u64 << 24) as f32 * (hi - lo)
    };

    let mut surface = skia_safe::surfaces::raster_n32_premul((600, 900)).unwrap();
    let c = surface.canvas();
    vertical(c, 0.0, 900.0, &[sky, rgb(horizon, 1.0)]);
    for _ in 0..40 {
        let (x, y, r, a) = (
            rnd(0.0, 600.0),
            rnd(0.0, 520.0),
            rnd(1.5, 3.5),
            rnd(0.3, 0.9),
        );
        dot(c, x, y, r, rgb(0xFFFFFF, a));
    }
    match i % 5 {
        // Ribbons.
        0 => {
            for (base, amp, freq, phase, w, col) in [
                (560.0, 55.0, 1.15, 0.4, 26.0, glow_c),
                (480.0, 70.0, 1.4, 2.2, 20.0, rgb(0x35D0C5, 1.0)),
                (400.0, 45.0, 0.95, 4.1, 14.0, rgb(0xFFFFFF, 1.0)),
            ] {
                let mut p = PathBuilder::new();
                for s in 0..=60 {
                    let t = s as f32 / 60.0;
                    let pt = (
                        t * 600.0,
                        base + amp * (t * PI * freq + phase).sin() - 40.0 * t,
                    );
                    if s == 0 {
                        p.move_to(pt);
                    } else {
                        p.line_to(pt);
                    }
                }
                glow(c, &p.detach(), w, col);
            }
            ridge(c, &mut rnd, 700.0, shade(1.6));
            ridge(c, &mut rnd, 770.0, shade(0.7));
        }
        // Falling stars.
        1 => {
            for _ in 0..6 {
                let (x, y, len) = (rnd(80.0, 560.0), rnd(140.0, 540.0), rnd(90.0, 170.0));
                let mut p = PathBuilder::new();
                p.move_to((x, y));
                p.line_to((x - 0.55 * len, y - 0.83 * len));
                glow(c, &p.detach(), 4.0, glow_c);
                dot(c, x, y, 12.0, rgb(0xFFFFFF, 0.9));
            }
            ridge(c, &mut rnd, 640.0, shade(2.2));
            ridge(c, &mut rnd, 730.0, shade(0.8));
        }
        // Circuit: a ring and right-angle traces on a 40 px grid.
        2 => {
            glow(c, &Path::circle((300.0, 380.0), 105.0, None), 10.0, glow_c);
            for t in 0..9 {
                let (mut x, mut y) = if t < 4 {
                    (
                        300.0 + [-105.0, 105.0, 0.0, 0.0][t],
                        380.0 + [0.0, 0.0, -105.0, 105.0][t],
                    )
                } else {
                    (40.0 * rnd(1.0, 14.0).round(), 40.0 * rnd(1.0, 21.0).round())
                };
                let mut p = PathBuilder::new();
                p.move_to((x, y));
                let mut horizontal = rnd(0.0, 1.0) > 0.5;
                for _ in 0..rnd(3.0, 6.0) as usize {
                    let step =
                        40.0 * rnd(1.0, 4.0).round() * if rnd(0.0, 1.0) > 0.5 { 1.0 } else { -1.0 };
                    if horizontal {
                        x = (x + step).clamp(20.0, 580.0);
                    } else {
                        y = (y + step).clamp(20.0, 880.0);
                    }
                    p.line_to((x, y));
                    horizontal = !horizontal;
                }
                glow(c, &p.detach(), 5.0, glow_c);
                dot(c, x, y, 12.0, Color4f { a: 0.9, ..glow_c });
            }
        }
        // Sun behind a keep.
        3 => {
            dot(c, 300.0, 470.0, 190.0, Color4f { a: 0.85, ..glow_c });
            ridge(c, &mut rnd, 600.0, shade(3.0));
            let keep = crate::theme::fill(shade(1.8));
            for (x, y, w) in [
                (250.0, 520.0, 100.0),
                (205.0, 590.0, 45.0),
                (350.0, 590.0, 45.0),
            ] {
                c.draw_rect(skia_safe::Rect::from_xywh(x, y, w, 900.0 - y), &keep);
                for m in 0..(w / 20.0) as usize {
                    let mx = x + m as f32 * 20.0 + 3.0;
                    c.draw_rect(skia_safe::Rect::from_xywh(mx, y - 14.0, 12.0, 14.0), &keep);
                }
            }
            ridge(c, &mut rnd, 720.0, shade(1.2));
            for _ in 0..20 {
                let (x, y, r, a) = (
                    rnd(30.0, 570.0),
                    rnd(300.0, 700.0),
                    rnd(2.5, 6.0),
                    rnd(0.35, 0.9),
                );
                dot(c, x, y, r, Color4f { a, ..glow_c });
            }
        }
        // Moon over rolling waves.
        _ => {
            dot(c, 420.0, 230.0, 170.0, Color4f { a: 0.35, ..glow_c });
            dot(c, 420.0, 230.0, 58.0, rgb(0xFFFFFF, 0.95));
            for w in 0..6 {
                let (base, amp) = (480.0 + w as f32 * 56.0, 22.0 - w as f32 * 2.0);
                let mut p = PathBuilder::new();
                for s in 0..=60 {
                    let t = s as f32 / 60.0;
                    let pt = (t * 600.0, base + amp * (t * PI * 3.0 + w as f32).sin());
                    if s == 0 {
                        p.move_to(pt);
                    } else {
                        p.line_to(pt);
                    }
                }
                glow(c, &p.detach(), 6.0 - w as f32 * 0.5, glow_c);
            }
        }
    }
    // Soft floor under the caption keeps it legible over any art.
    vertical(c, 620.0, 900.0, &[rgb(0x000000, 0.0), rgb(0x000000, 0.75)]);
    let caption = title.to_uppercase();
    let size = 46.0 * (540.0 / f64::from(fonts.measure(&caption, W::Bold, 46.0))).min(1.0);
    let width = f64::from(fonts.measure(&caption, W::Bold, size));
    fonts.draw(
        c,
        &caption,
        (600.0 - width) / 2.0,
        836.0,
        W::Bold,
        size,
        rgb(0xFFFFFF, 0.94),
    );
    surface
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap()
        .as_bytes()
        .to_vec()
}
