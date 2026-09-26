use super::*;

#[test]
fn the_os_field_compiles_for_both_modes() {
    for light in [false, true] {
        let t = crate::os_theme::OsTheme {
            light,
            background: if light {
                (0.99, 0.96, 0.89)
            } else {
                (0.02, 0.04, 0.12)
            },
            foreground: if light {
                (0.36, 0.42, 0.45)
            } else {
                (1.0, 0.81, 0.68)
            },
            accent: (0.49, 0.51, 0.85),
        };
        build_mesh_os(&t).unwrap();
    }
}
use crate::model::WakeStatus;
use crate::screens::home::HomeScreen;
use crate::screens::library::LibraryScreen;
use punktfunk_core::config::GamepadPref;

/// Pins `motion_spring` (vectors v2). v1 `motion` still exists for other clients; this
/// transition is a spring, not that ease-out, so sampling v1 would pass a curve we do not run.
///
/// The redesign's motion table vs `motion_springs` in `console-vectors.json`.
#[test]
fn the_motion_table_matches_the_shared_vectors() {
    use crate::anim::springs;
    let raw = include_str!("../../../../clients/shared/console-vectors.json");
    let file: serde_json::Value = serde_json::from_str(raw).unwrap();
    let m = &file["motion_springs"];
    let num = |a: &str, b: &str| m[a][b].as_f64().unwrap_or_else(|| panic!("{a}.{b}"));
    for (name, spec) in [
        ("focus", springs::FOCUS),
        ("press", springs::PRESS),
        ("nav", springs::NAV),
        ("modal", springs::MODAL),
    ] {
        assert_eq!(spec.response, num(name, "response"), "{name}");
        assert_eq!(spec.damping, num(name, "damping"), "{name}");
    }
    assert_eq!(crate::anim::PRESS_SCALE, num("press", "scale"));
    assert_eq!(TAB_SLIDE, num("tab", "slide_fraction"));
    let e = crate::anim::entrances::CARDS;
    assert_eq!(e.stagger, num("entrance", "stagger_s"));
    assert_eq!(crate::library::ENTER_SCALE, num("entrance", "scale"));
    assert_eq!(crate::library::ENTER_RISE, num("entrance", "rise_dp"));
}

/// Springs are integrator-dependent: two runtimes that honour `response`/`damping` agree
/// to the eye and disagree in the third decimal. Pin the parameters, not sampled positions.
#[test]
fn motion_matches_the_shared_vectors() {
    let raw = include_str!("../../../../clients/shared/console-vectors.json");
    let file: serde_json::Value =
        serde_json::from_str(raw).expect("console-vectors.json must parse");
    assert!(
        file["version"].as_u64() >= Some(3),
        "the motion table arrived with version 3"
    );

    let m = &file["motion_springs"]["nav"];
    let num = |key: &str| m[key].as_f64().unwrap_or_else(|| panic!("{key} missing"));
    let close = |what: &str, got: f64, want: f64| {
        assert!(
            (got - want).abs() < 1e-9,
            "{what} is {got}, vectors say {want}"
        );
    };
    close(
        "response",
        crate::anim::springs::NAV.response,
        num("response"),
    );
    close("damping", crate::anim::springs::NAV.damping, num("damping"));
    close("push slide", NAV_SLIDE_DP, num("push_slide_dp"));
    close("enter scale", NAV_ENTER_SCALE, num("enter_scale"));
    close("exit scale", NAV_EXIT_SCALE, num("exit_scale"));
    close("reveal alpha", NAV_REVEAL_ALPHA, num("reveal_alpha"));
    assert_eq!(
        m["interruptible"].as_bool(),
        Some(true),
        "this client's transitions accept Back mid-flight; the block must say so"
    );
}

/// Pins `shell_tabs` (vectors v3): the strip's ids, names and order.
#[test]
fn tabs_match_the_shared_vectors() {
    let raw = include_str!("../../../../clients/shared/console-vectors.json");
    let file: serde_json::Value = serde_json::from_str(raw).expect("vectors parse");
    let want: Vec<(&str, &str)> = file["shell_tabs"]
        .as_array()
        .expect("shell_tabs")
        .iter()
        .map(|t| (t["id"].as_str().unwrap(), t["name"].as_str().unwrap()))
        .collect();
    let have: Vec<(&str, &str)> = TABS.iter().map(|t| (t.id(), t.name())).collect();
    assert_eq!(have, want);
}

/// Shared throwaway config dir. Settings SAVE on adjust; a second `OnceLock` here
/// would pick a second directory and the other test's loads would miss its writes.
use crate::screens::settings::tests::fake_home;

fn hosts() -> Vec<HostRow> {
    let base = HostRow {
        key: String::new(),
        id: None,
        name: String::new(),
        addr: "10.0.0.20".into(),
        port: 9777,
        fp_hex: String::new(),
        paired: false,
        saved: true,
        online: false,
        mgmt_port: 47990,
        can_wake: false,
        clipboard_sync: false,
        last_used: None,
        os: String::new(),
        actions: Vec::new(),
        pin: None,
        bound_preset: None,
        running: String::new(),
        game_presets: Default::default(),
    };
    vec![
        HostRow {
            key: "aa11".into(),
            id: None,
            name: "Living Room PC".into(),
            fp_hex: "aa11".into(),
            paired: true,
            online: true,
            last_used: Some(1),
            ..base.clone()
        },
        HostRow {
            key: "bb22".into(),
            id: None,
            name: "Office Tower".into(),
            addr: "10.0.0.21".into(),
            fp_hex: "bb22".into(),
            paired: true,
            can_wake: true,
            ..base.clone()
        },
        HostRow {
            key: "10.0.0.30:9777".into(),
            id: None,
            name: "steambox".into(),
            addr: "10.0.0.30".into(),
            saved: false,
            online: true,
            ..base
        },
    ]
}

/// `ConsoleOptions::desktop` leaves `store` unset, and the shell then resolves it to the file
/// store — the developer's real settings on the desktop, and a bail off it. Every test gets its
/// own in-memory store instead: same screens, and no shell racing another test's whole-file save.
fn test_options() -> ConsoleOptions {
    let mut opts = ConsoleOptions::desktop("deck".into(), false);
    opts.store = Some(std::sync::Arc::new(crate::store::SnapshotStore::new(
        pf_client_core::trust::Settings::default(),
        Vec::new(),
    )));
    opts
}

fn shell(stack: Vec<Screen>) -> (Shell, ConsoleShared, LibraryShared) {
    fake_home();
    let console = ConsoleShared::default();
    console.set_hosts(hosts());
    let library = LibraryShared::default();
    let bus = ConsoleBus::default();
    let shell = Shell::new(console.clone(), library.clone(), bus, test_options(), stack).unwrap();
    (shell, console, library)
}

/// A tab switch slides the content only: mid-flight the strip's pixels match the settled
/// frame's. Hosts and Players share the aurora, so the backdrop does not change under it.
#[test]
fn the_strip_holds_still_across_a_tab_switch() {
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1280, 800);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.fake_clock = Some((100.0, 1.0 / 60.0));
    // The strip's line (k = 1 at 800 tall), left half: the tabs, not the chip.
    let (bw, bh) = (w / 2, 36);
    let mut band = |s: &mut Shell, frames: usize| {
        for _ in 0..frames {
            s.render(
                surface.canvas(),
                w as u32,
                h as u32,
                &fonts,
                None,
                None,
                &[],
            );
        }
        let info = skia_safe::ImageInfo::new_n32_premul((bw, bh), None);
        let mut px = vec![0u8; (bw * bh * 4) as usize];
        assert!(surface.read_pixels(&info, &mut px, (bw * 4) as usize, (0, 32)));
        px
    };
    band(&mut s, 30);
    assert!(s.switch_tab(Tab::Players));
    let mid = band(&mut s, 5);
    assert!(
        matches!(s.motion, Motion::Tab { .. }),
        "still mid-switch after five frames"
    );
    // Settle, then read the strip at the mid frame's field time: the field moves under it,
    // and only the strip is on trial.
    let clock = s.fake_clock;
    band(&mut s, 90);
    s.fake_clock = clock.map(|(t, step)| (t - step, step));
    let settled = band(&mut s, 1);
    let worst = mid
        .iter()
        .zip(&settled)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(
        worst <= 12,
        "the strip moved mid-switch (max channel diff {worst})"
    );
}

/// A remote's whole lap: Up from the host row lands on the strip, Left and Right walk the
/// tabs, Down returns to the screen, L1/R1 jump from content, and Back at a root leaves.
#[test]
fn navigation_lap() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    assert_eq!(
        s.handle_menu(MenuEvent::Move(MenuDir::Up))
            .map(|p| format!("{p:?}")),
        Some("Move".into())
    );
    assert!(s.strip_focus, "up from the host row lands on its tab");
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    assert_eq!(s.tab, Tab::Games);
    assert!(matches!(s.stack.as_slice(), [Screen::Library(_)]));
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    assert_eq!(s.tab, Tab::Players);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    assert_eq!(s.tab, Tab::Settings);
    finish_motion(&mut s);
    assert!(matches!(
        s.handle_menu(MenuEvent::Move(MenuDir::Right)),
        Some(MenuPulse::Boundary)
    ));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    assert!(!s.strip_focus, "down returns to the screen");
    assert!(
        matches!(s.stack.as_slice(), [Screen::Settings(st)] if st.strip_focus_for_test()),
        "on Settings, to its sections first"
    );
    for _ in 0..3 {
        s.handle_menu(MenuEvent::JumpBack);
        finish_motion(&mut s);
    }
    assert!(matches!(s.stack.as_slice(), [Screen::Home(_)]));
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(s.take_action(), Some(OverlayAction::Quit)));
}

/// A tab root that places nothing to focus parks focus on its tab, where Down stays;
/// once the root places targets, the parked focus goes back in by itself.
#[test]
fn a_root_with_nothing_to_focus_parks_focus_on_the_strip() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    frame(&mut s);
    s.root_targets = Some(0);
    s.sync();
    assert!(s.strip_focus, "focus parks on the tab");
    assert!(matches!(
        s.handle_menu(MenuEvent::Move(MenuDir::Down)),
        Some(MenuPulse::Boundary)
    ));
    assert!(s.strip_focus, "nothing below takes it");

    s.strip_parked = true;
    s.root_targets = Some(3);
    s.sync();
    assert!(!s.strip_focus, "the root's targets take it back");
}

/// Up that a root screen bumps reaches its tab.
#[test]
fn a_bumped_up_at_a_root_reaches_the_strip() {
    let (mut s, _console, _library) = shell(vec![Screen::Players(
        crate::screens::players::PlayersScreen::new(),
    )]);
    frame(&mut s);
    s.sync();
    assert!(!s.strip_focus);
    assert!(matches!(
        s.handle_menu(MenuEvent::Move(MenuDir::Up)),
        Some(MenuPulse::Move)
    ));
    assert!(s.strip_focus);
}

/// OK on a remote acts on release; held, it is the card's menu and the release does
/// nothing more.
#[test]
fn a_held_ok_opens_the_card_menu() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.fake_clock = Some((10.0, 0.0));
    s.ok(true);
    s.fake_clock = Some((10.2, 0.0));
    s.ok(false);
    assert!(
        matches!(s.take_action(), Some(OverlayAction::Launch { .. })),
        "a click connects"
    );
    s.connecting = None;
    s.ok(true);
    s.fake_clock = Some((10.8, 0.0));
    s.tick_ok();
    assert!(matches!(s.stack.last(), Some(Screen::CardMenu(_))));
    s.ok(false);
    assert!(
        s.take_action().is_none(),
        "the release after a hold does nothing"
    );
}

/// A warm-up tours the tabs and leaves the shell exactly where it was: same tab and stack,
/// nothing parked, the real library, and nothing asked of the host.
#[test]
fn a_warm_up_tours_the_tabs_and_changes_nothing() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.fake_clock = Some((100.0, 1.0 / 60.0));
    s.bus.drain();
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((480, 300)).unwrap();
    let viewport = crate::console::Viewport::plain(480, 300);
    s.warm_up(surface.canvas(), &viewport, &fonts);
    assert_eq!(s.tab, Tab::Hosts);
    assert!(matches!(s.stack.as_slice(), [Screen::Home(_)]));
    assert!(s.parked.iter().all(Option::is_none), "nothing parked");
    assert!(matches!(s.motion, Motion::None));
    assert!(
        library.snapshot().games.is_empty(),
        "the stand-in never reached the model"
    );
    assert!(
        s.library.snapshot().games.is_empty(),
        "the real library is back"
    );
    assert!(s.bus.drain().is_empty(), "nothing asked of the host");
}

#[test]
fn connect_flow_raises_launch_and_cancel() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Confirm); // paired+online host focused first
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::Launch { launch: None, .. })
    ));
    assert!(s.connecting.is_some());
    // Cancel drops the takeover immediately. Waiting for a session phase burns the
    // connect budget; an embedder that drops a canceled dial sends no phase at all.
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::CancelConnect)
    ));
    assert!(s.connecting.is_none(), "cancel drops the takeover itself");
    s.session_ended(None);
    assert!(s.connecting.is_none());
}

fn finish_motion(s: &mut Shell) {
    // Seat the spring and run the real settle. Skipping it drops the bookkeeping
    // that pops a reversed push off the stack.
    if let Motion::Nav { spring, target, .. } = &mut s.motion {
        spring.pos = *target;
        spring.vel = 0.0;
    }
    s.finish_nav();
}

/// Step at a fixed `dt` until settle. Bound so a spring that never settles fails
/// instead of hanging.
fn run_motion(s: &mut Shell) -> Vec<f64> {
    let mut path = Vec::new();
    for _ in 0..600 {
        match s.advance_nav(1.0 / 120.0) {
            Some(p) => path.push(p),
            None => return path,
        }
    }
    panic!("transition never settled");
}

/// The combined home: the games of the host the row rests on sit under it. Down past the
/// card's verbs lands on them without the desktop tile (the card above is the desk), OK
/// launches from there, and Up from their top row returns to the verbs.
#[test]
fn down_from_a_card_lands_on_its_games_and_launches_there() {
    fake_home();
    let console = ConsoleShared::default();
    console.set_hosts(hosts());
    let library = LibraryShared::default();
    let bus = ConsoleBus::default();
    let home = vec![Screen::Home(HomeScreen::new())];
    let mut s = Shell::new(console, library.clone(), bus.clone(), test_options(), home).unwrap();
    let fetched = |bus: &ConsoleBus| -> Vec<String> {
        (bus.drain().into_iter())
            .filter_map(|c| match c {
                ConsoleCmd::FetchLibrary { fp_hex, .. } => Some(fp_hex),
                _ => None,
            })
            .collect()
    };
    s.sync();
    assert_eq!(fetched(&bus), vec![hosts()[0].fp_hex.clone()]);
    s.sync();
    assert!(fetched(&bus).is_empty(), "asked once");

    library.set_games(vec![crate::library::LibraryGame {
        id: "steam:570".into(),
        title: "Dota 2".into(),
        store: "steam".into(),
        launcher: false,
        icon: String::new(),
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
    }]);
    frame(&mut s);
    let below = |s: &Shell| matches!(s.stack.last(), Some(Screen::Home(h)) if h.shelf().is_some());
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    assert!(!below(&s), "the first Down lands on the card's verbs");
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    assert!(below(&s), "the second lands on the games");
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    assert!(!below(&s), "Up from the top row returns to the verbs");
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Confirm);
    match s.take_action() {
        Some(OverlayAction::Launch { launch, .. }) => {
            assert_eq!(launch.as_deref(), Some("steam:570"));
        }
        _ => panic!("OK on the games launches the title"),
    }
}

/// Y on a pinned card must carry that preset into the library. Falling back to the
/// host default would ignore the pin, which is why the card exists.
#[test]
fn a_pinned_cards_library_launches_with_its_preset() {
    let mut rows = hosts();
    let card = HostRow {
        key: "aa11\u{0}hdr".into(),
        pin: Some(crate::model::PresetChip {
            id: "hdr".into(),
            name: "HDR".into(),
            accent: None,
            bitrate_kbps: None,
        }),
        ..rows[0].clone()
    };
    rows.insert(1, card);
    let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    console.set_hosts(rows);
    s.sync();

    // Pinned card sits immediately after its host's primary tile.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::JumpForward);
    finish_motion(&mut s);
    match s.stack.last() {
        Some(Screen::Library(l)) => assert_eq!(
            l.title(),
            "Living Room PC \u{b7} HDR",
            "the shelf names the preset it will launch with"
        ),
        _ => panic!("Games on a pinned card opens its shelf"),
    }

    library.set_games(vec![crate::library::LibraryGame {
        id: "steam:570".into(),
        title: "Dota 2".into(),
        store: "steam".into(),
        launcher: false,
        icon: String::new(),
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
    }]);
    // Down past the Desktops band, which arrives focused and launches nothing.
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Confirm);
    match s.take_action() {
        Some(OverlayAction::Launch { launch, preset, .. }) => {
            assert_eq!(launch.as_deref(), Some("steam:570"));
            assert_eq!(
                preset.as_deref(),
                Some("hdr"),
                "the launch carries the pinned card's preset"
            );
        }
        _ => panic!("A on a title raises a launch"),
    }
}

/// Primary tile: no one-off preset. The resolver sees `None` and uses the host binding.
#[test]
fn a_primary_tiles_library_leaves_the_preset_to_the_binding() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::JumpForward); // Games, on the focused host's shelf
    finish_motion(&mut s);
    library.set_games(vec![crate::library::LibraryGame {
        id: "steam:570".into(),
        title: "Dota 2".into(),
        store: "steam".into(),
        launcher: false,
        icon: String::new(),
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
    }]);
    s.handle_menu(MenuEvent::Confirm);
    assert!(matches!(
        s.take_action(),
        Some(OverlayAction::Launch { preset: None, .. })
    ));
}

#[test]
fn wake_gates_input_in_the_same_press() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Office Tower is the second tile: offline and wakeable.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Confirm);
    let w = s
        .wake
        .as_ref()
        .expect("Waking card raised in the SAME call as the A press");
    assert_eq!(w.name, "Office Tower");
    assert!(!w.online);
    // Gate the next input. `sync` (first in handle_menu) must not clear the
    // placeholder before the service thread reports a real status.
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_none());
    assert!(
        s.wake.is_some(),
        "optimistic card survived a sync with no service status"
    );
    s.handle_menu(MenuEvent::Back);
    assert!(s.wake.is_none());
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Left)).is_some());
}

/// Tab / Shift+Tab walk the console's tabs, the keyboard's L1/R1.
#[test]
fn tab_and_shift_tab_change_tabs() {
    use crate::input::Key as Scancode;
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    assert!(s.key(Scancode::Tab, false, false), "Tab is consumed");
    assert_eq!(s.tab, Tab::Games, "Tab goes forward");
    s.motion = Motion::None;
    s.key(Scancode::Tab, false, false);
    assert_eq!(s.tab, Tab::Players);
    s.motion = Motion::None;
    assert!(s.key(Scancode::Tab, true, false));
    assert_eq!(s.tab, Tab::Games, "Shift+Tab goes back");
    s.motion = Motion::None;
    s.key(Scancode::Tab, false, true);
    assert_eq!(s.tab, Tab::Games, "held Tab doesn't skip tabs");
}

/// A right-click is Back on every screen, so a pointer always has a way out.
#[test]
fn a_secondary_press_goes_back() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary); // Y → the host's menu
    s.motion = Motion::None;
    assert_eq!(s.stack.len(), 2);
    assert!(s.pointer(crate::pointer::Pointer {
        x: 10.0,
        y: 10.0,
        kind: crate::pointer::PointerKind::Back,
    }));
    // Same transition a B press uses.
    assert!(matches!(
        s.motion,
        Motion::Nav {
            kind: NavKind::Pop,
            ..
        }
    ));
}

/// Replace recedes the swapped-out screen, not its parent. A push paints the
/// screen beneath as the leaving layer; replace already popped, so without carrying
/// the predecessor the renderer recedes the parent. Asserted on the carried screen:
/// a frame diff would pin the look of a transition, not which screen it holds.
#[test]
fn a_replace_carries_the_screen_it_replaced() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary);
    assert!(matches!(s.stack.last(), Some(Screen::CardMenu(_))));
    finish_motion(&mut s);

    // The first host's menu is [Connect with…, Browse games, Copy link, Host details…] —
    // three Downs. Pressed exactly so a reorder fails here, not on something destructive.
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        matches!(s.stack.last(), Some(Screen::CardMenu(m)) if m.title().ends_with("Details")),
        "Host details… opens the details"
    );
    assert_eq!(s.stack.len(), 2, "the menu was swapped out, not stacked on");
    match &s.motion {
        Motion::Nav {
            kind: NavKind::Push,
            leaving: Some(carried),
            ..
        } => assert!(
            matches!(carried.as_ref(), Screen::CardMenu(_)),
            "the receding layer must be the MENU; carrying nothing leaves the renderer to \
             recede the menu's parent, which is the reported flash"
        ),
        _ => panic!("a replace must be a push CARRYING its predecessor"),
    }

    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(
        matches!(s.stack.last(), Some(Screen::CardMenu(_))),
        "a reversed replace lands where the user actually was"
    );
}

#[test]
fn y_opens_a_menu_on_every_host_card_and_none_on_the_action_tiles() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary);
    assert!(matches!(s.stack.last(), Some(Screen::CardMenu(_))));
    s.motion = Motion::None;
    s.handle_menu(MenuEvent::Back);
    s.motion = Motion::None;
    // The third fixture host is discovered-only (`saved: false`): Pair… and Add host.
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Secondary);
    assert!(matches!(s.stack.last(), Some(Screen::CardMenu(_))));
    s.motion = Motion::None;
    s.handle_menu(MenuEvent::Back);
    s.motion = Motion::None;
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Secondary);
    assert!(
        matches!(s.stack.last(), Some(Screen::Home(_))),
        "Add Host is a tile, not a host"
    );
}

/// The next Settings section the remote's way: up onto the section strip, right, down.
fn next_section(s: &mut Shell) {
    while !matches!(s.stack.last(), Some(Screen::Settings(st)) if st.strip_focus_for_test()) {
        s.handle_menu(MenuEvent::Move(MenuDir::Up));
    }
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
}

/// The licences take a host's full notices once they arrive, draw them, and page with
/// Right; Back leaves.
#[test]
fn the_licences_page_through_a_hosts_notices() {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let mut fx = crate::screens::Outbox::default();
    let licenses = crate::screens::licenses::LicensesScreen::new(&mut fx);
    let (mut s, console, _library) = shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::Licenses(licenses),
    ]);
    let notices: String = (0..12_000)
        .map(|i| format!("crate-{i} 1.0.0 — MIT OR Apache-2.0 — https://example.com/{i}\n"))
        .collect();
    console.set_licenses(vec![crate::model::LicenseSection {
        heading: "Third-party software".into(),
        text: notices,
    }]);
    let mut frame = |s: &mut Shell| s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    frame(&mut s);
    let Some(Screen::Licenses(l)) = s.stack.last() else {
        panic!("the licences are on top");
    };
    assert!(!l.waiting(), "the host's sections reached the screen");
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    frame(&mut s);
    let Some(Screen::Licenses(l)) = s.stack.last() else {
        panic!("paging stays on the licences");
    };
    assert!(l.scrolled() > 1000.0, "three pages down: {}", l.scrolled());
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Home(_))));
}

/// The host's test mode follows the test screen: on while it is on top, drawn from the
/// host's readings, and off again once a held B takes it away.
#[test]
fn the_input_test_turns_the_hosts_test_mode_on_and_off() {
    use crate::model::{ConsoleCmd, PadTestState};
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let test = crate::screens::input_test::InputTestScreen::new();
    let (mut s, console, _library) = shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::InputTest(test),
    ]);
    s.fake_clock = Some((100.0, 1.0 / 60.0));
    let mut frame = |s: &mut Shell| s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    frame(&mut s);
    assert!(s.bus.drain().contains(&ConsoleCmd::PadTest { on: true }));
    for _ in 0..80 {
        console.set_pad_test(PadTestState {
            held: vec!["B".into()],
            axes: vec![("LX".into(), 0.5)],
        });
        frame(&mut s);
    }
    finish_motion(&mut s);
    frame(&mut s);
    assert!(
        matches!(s.stack.last(), Some(Screen::Home(_))),
        "a held B finished"
    );
    assert!(s.bus.drain().contains(&ConsoleCmd::PadTest { on: false }));
}

/// A preset's editor draws every row it can hold, and a step saves the override; the global
/// settings stay as they were.
#[test]
fn a_preset_editor_rasters_and_saves_over_the_global() {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let edit =
        crate::screens::preset::PresetEdit::new("p1".into(), "Couch".into(), Default::default());
    let (mut s, _console, _library) = shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::PresetEdit(edit),
    ]);
    let mut frame = |s: &mut Shell| s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    frame(&mut s);
    let global = s.settings.clone();
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    frame(&mut s);
    assert_eq!(s.settings, global, "the preset changes, not Settings");
}

/// A grouped row draws its band captions, and the shell re-arranges the hosts when the
/// setting moves, with no new host list to prompt it.
#[test]
fn a_grouped_host_row_rasters_in_its_new_order() {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    let mut frame = |s: &mut Shell| s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    frame(&mut s);
    let before: Vec<String> = s.hosts.iter().map(|h| h.name.clone()).collect();
    s.settings.extra.insert("host_sort".into(), "name".into());
    s.settings
        .extra
        .insert("host_grouping".into(), "status".into());
    for _ in 0..3 {
        frame(&mut s);
    }
    let after: Vec<String> = s.hosts.iter().map(|h| h.name.clone()).collect();
    assert_eq!(after.len(), before.len());
    assert!(
        s.hosts.windows(2).all(|w| w[0].online >= w[1].online),
        "online before offline: {after:?}"
    );
}

/// The search screen draws with its keyboard up, and a search that found nothing draws its
/// state line rather than an empty field.
#[test]
fn a_search_and_its_empty_result_raster() {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let host = hosts().remove(0);
    let search = crate::screens::search::SearchScreen::new(&host, &Default::default());
    let (mut s, _console, library) = shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::Search(search),
    ]);
    library.set_games(Vec::new());
    library.set_phase(crate::library::LibraryPhase::Ready);
    let mut frame = |s: &mut Shell| s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    for _ in 0..3 {
        frame(&mut s);
    }
    s.text_input("zz");
    s.handle_menu(MenuEvent::Back);
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Confirm);
    finish_motion(&mut s);
    frame(&mut s);
    let Some(Screen::Library(shelf)) = s.stack.last() else {
        panic!("the results replace the search");
    };
    assert!(shelf.no_match());
}

#[test]
fn every_settings_tab_rasters() {
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1280u32, 800u32);
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    finish_motion(&mut s);

    let mut frame = |s: &mut Shell| {
        s.render(
            surface.canvas(),
            w,
            h,
            &fonts,
            Some("Xbox Wireless Controller"),
            Some(GamepadPref::Xbox360),
            &pads,
        );
    };
    // One lap of the strip. Walk to the end first so both row paths run, then one
    // frame per tab — every tab's rows fit on 800-tall. CPU SkSL is ~1s/frame in
    // debug; this is a panic catch, not an eyeball pass.
    for _ in 0..crate::screens::settings::TAB_COUNT {
        for _ in 0..12 {
            s.handle_menu(MenuEvent::Move(MenuDir::Down));
        }
        frame(&mut s);
        next_section(&mut s);
    }
    // 640×400: pills are measured text, so a too-small width must clamp, not overflow.
    s.render(surface.canvas(), 640, 400, &fonts, None, None, &pads);
}

/// One rendered frame so the rows have real rects to press.
fn rendered_settings() -> (Shell, skia_safe::Rect) {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.handle_menu(MenuEvent::Tertiary); // X → Settings
    finish_motion(&mut s);
    s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    let row = match s.stack.last() {
        Some(Screen::Settings(scr)) => scr.row_rect_for_test(0).expect("the list drew its rows"),
        _ => panic!("settings is not on top"),
    };
    (s, row)
}

/// On a wide screen the Settings rows are a centred column at their full width.
#[test]
fn settings_rows_sit_centred() {
    let (_s, row) = rendered_settings();
    assert!(
        (row.center_x() - 640.0).abs() < 1.0,
        "row centre {}",
        row.center_x()
    );
    assert!((row.width() - crate::widgets::ROW_MAX_W as f32).abs() < 1.0);
}

/// A finger swipe across the list is a scroll and must not flip the landed-on value.
/// The same contact lifted in place is the tap, delivered on lift at the anchor.
#[test]
fn a_touch_swipe_scrolls_settings_without_changing_a_value() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let (cx, cy) = (row.center_x(), row.center_y());
    // Resolution's observable: Native → Match window flips the flag; size stays (0, 0).
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);

    s.pointer_input(PointerInput::Down {
        x: cx,
        y: cy,
        button: PointerButton::Primary,
        touch: true,
    });
    for i in 1..=6 {
        s.pointer_input(PointerInput::Move {
            x: cx,
            y: cy - (i as f32) * 40.0,
        });
    }
    s.pointer_input(PointerInput::Up {
        x: cx,
        y: cy - 240.0,
        button: PointerButton::Primary,
    });
    assert_eq!(
        state(&s),
        before,
        "a swipe across a row is a scroll, not a value change"
    );

    s.pointer_input(PointerInput::Down {
        x: cx,
        y: cy,
        button: PointerButton::Primary,
        touch: true,
    });
    assert_eq!(state(&s), before, "a touch press must not act on contact");
    s.pointer_input(PointerInput::Up {
        x: cx,
        y: cy,
        button: PointerButton::Primary,
    });
    assert_ne!(
        state(&s),
        before,
        "the tap lands on the lift, at the anchor"
    );
}

/// A finger drags the Settings rows one to one; a flick keeps them going after the lift,
/// and that lift is no tap.
#[test]
fn a_finger_pans_and_flings_the_settings_list() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, _) = rendered_settings();
    for _ in 0..5 {
        next_section(&mut s);
    }
    // A short window, so the nine rows overflow the list by a few rows.
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 400)).unwrap();
    let mut frame = |s: &mut Shell| s.render(surface.canvas(), 1280, 400, &fonts, None, None, &[]);
    s.fake_clock = Some((100.0, 1.0 / 60.0));
    frame(&mut s);
    frame(&mut s);
    // Row 3 stays on screen across the whole scroll range.
    let top = |s: &Shell| match s.stack.last() {
        Some(Screen::Settings(scr)) => scr.row_rect_for_test(3).map_or(f32::NAN, |r| r.top),
        _ => panic!("settings is not on top"),
    };
    let before = (top(&s), s.settings.clone());
    let (x, y) = match s.stack.last() {
        Some(Screen::Settings(scr)) => {
            assert_eq!(scr.tab_for_test(), 5, "Interface, the longest tab");
            let r = scr.row_rect_for_test(2).expect("row 2 drew");
            (r.center_x(), r.center_y())
        }
        _ => unreachable!(),
    };
    s.pointer_input(PointerInput::Down {
        x,
        y,
        button: PointerButton::Primary,
        touch: true,
    });
    // 20 px up a frame: the first step leaves slop, the next three move the rows.
    for i in 1..=4 {
        s.pointer_input(PointerInput::Move {
            x,
            y: y - 20.0 * i as f32,
        });
        frame(&mut s);
    }
    let panned = top(&s);
    assert!(
        (before.0 - 60.0 - panned).abs() < 0.01,
        "{} → {panned}",
        before.0
    );
    // Lifted mid-flick at 1200 px/s: the rows keep going, then stop.
    s.pointer_input(PointerInput::Up {
        x,
        y: y - 80.0,
        button: PointerButton::Primary,
    });
    for _ in 0..180 {
        frame(&mut s);
    }
    let flung = top(&s);
    assert!(
        flung < panned - 30.0,
        "the fling carried on: {panned} → {flung}"
    );
    frame(&mut s);
    assert_eq!(top(&s), flung, "and came to rest");
    assert_eq!(s.settings, before.1, "a drag's lift changes nothing");
}

/// A finger held still on a row is the pad's Secondary on that row: on Bitrate that
/// opens the typed field. The lift is no tap, or it would commit and close the field.
#[test]
fn a_long_press_is_secondary_on_the_row_under_the_finger() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, _) = rendered_settings();
    let bitrate = match s.stack.last() {
        Some(Screen::Settings(scr)) => scr.row_rect_for_test(3).expect("Bitrate drew"),
        _ => panic!("settings is not on top"),
    };
    let (x, y) = (bitrate.center_x(), bitrate.center_y());
    s.fake_clock = Some((10.0, 0.0));
    s.pointer_input(PointerInput::Down {
        x,
        y,
        button: PointerButton::Primary,
        touch: true,
    });
    s.fake_clock = Some((10.4, 0.0));
    s.tick_touch();
    assert!(!s.editing(), "not held long enough yet");
    s.fake_clock = Some((10.6, 0.0));
    s.tick_touch();
    assert!(s.editing(), "held on Bitrate opens its typed field");
    s.pointer_input(PointerInput::Up {
        x,
        y,
        button: PointerButton::Primary,
    });
    assert!(s.editing(), "the lift after a long press does not tap");
}

/// A mouse press still acts on contact. Only touch defers to the lift.
#[test]
fn a_mouse_press_still_acts_on_contact() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);
    s.pointer_input(PointerInput::Down {
        x: row.center_x(),
        y: row.center_y(),
        button: PointerButton::Primary,
        touch: false,
    });
    assert_ne!(state(&s), before, "a mouse click acts on the press");
}

/// One tick per `DRAG_TICK_DP` past slop; the lift after a drag presses nothing.
/// Ticks act on the cursor, not drawn rects, so no render.
#[test]
fn a_horizontal_drag_steps_the_home_carousel() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.pointer_input(PointerInput::Down {
        x: 640.0,
        y: 400.0,
        button: PointerButton::Primary,
        touch: true,
    });
    // First move leaves slop (locks X); the second is one tick left — next tile.
    s.pointer_input(PointerInput::Move { x: 620.0, y: 400.0 });
    s.pointer_input(PointerInput::Move {
        x: 620.0 - DRAG_TICK_DP as f32,
        y: 400.0,
    });
    s.pointer_input(PointerInput::Up {
        x: 620.0 - DRAG_TICK_DP as f32,
        y: 400.0,
        button: PointerButton::Primary,
    });
    // Second host is offline with a stored MAC: Confirm raises wake. That proves
    // the drag moved the cursor and the lift itself pressed nothing.
    assert!(
        s.wake.is_none(),
        "the drag itself must not activate anything"
    );
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        s.wake.is_some(),
        "Confirm after a one-tick drag lands on the second host's wake"
    );
}

/// A canceled touch is dropped whole: a stray lift after Cancel must not act.
#[test]
fn a_canceled_touch_never_acts() {
    use pf_client_core::console::{PointerButton, PointerInput};
    let (mut s, row) = rendered_settings();
    let state = |s: &Shell| (s.settings.match_window, s.settings.width, s.settings.height);
    let before = state(&s);
    s.pointer_input(PointerInput::Down {
        x: row.center_x(),
        y: row.center_y(),
        button: PointerButton::Primary,
        touch: true,
    });
    s.pointer_input(PointerInput::Cancel);
    s.pointer_input(PointerInput::Up {
        x: row.center_x(),
        y: row.center_y(),
        button: PointerButton::Primary,
    });
    assert_eq!(
        state(&s),
        before,
        "cancel dropped the gesture; the stray lift presses nothing"
    );
}

/// Back mid-push retargets the same spring rather than queuing a pop. Cancel-and-play
/// snaps because the two recipes disagree on position; carrying `pos` cannot.
/// Assert the first sample after retarget is within a frame of travel.
#[test]
fn back_mid_push_turns_the_screen_around() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary); // Y → the host's menu
    assert_eq!(s.stack.len(), 2);

    let mut before = 0.0;
    for _ in 0..12 {
        before = s.advance_nav(1.0 / 120.0).expect("still in flight");
    }
    assert!(before > 0.05 && before < 0.95, "mid-flight, got {before}");
    assert!(s.nav_back(), "Back is answered by the transition itself");
    assert_eq!(
        s.stack.len(),
        2,
        "the screen is still on the stack while it flies back"
    );

    let path = run_motion(&mut s);
    assert!(!path.is_empty(), "the reversal actually animated");
    assert!(
        (path[0] - before).abs() < 0.05,
        "jumped from {before} to {}",
        path[0]
    );
    assert!(*path.last().expect("non-empty") < before);
    assert_eq!(
        s.stack.len(),
        1,
        "the reversed push took its screen back off"
    );
    assert!(matches!(s.motion, Motion::None));
}

/// Back at the root is not a reversal: there is no parent, and B there means quit.
/// Decline it so the normal path can answer.
#[test]
fn back_mid_push_at_the_root_is_left_to_the_normal_path() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Replace at the root pushes without deepening the stack.
    s.apply_nav(crate::screens::Nav::Replace(Box::new(Screen::Home(
        HomeScreen::new(),
    ))));
    assert_eq!(s.stack.len(), 1);
    s.advance_nav(1.0 / 120.0);
    assert!(!s.nav_back(), "nothing to reverse into");
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1, "and the root survived");
}

/// Mid-pop Confirm is a mis-tap and is refused. Mid-pop Back is a held B: start
/// the next pop at once rather than queuing it behind the current one.
#[test]
fn mid_pop_refuses_confirm_but_honours_another_back() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary); // → the host's menu
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Move(MenuDir::Down)); // a row that would push if activated
    s.handle_menu(MenuEvent::Back); // start the pop
    assert!(matches!(s.motion, Motion::Nav { .. }));
    let mid = s.advance_nav(1.0 / 120.0).expect("in flight");
    assert!(mid < NAV_INPUT_OPENS, "the test needs an early sample");

    let depth = s.stack.len();
    assert!(
        s.handle_menu(MenuEvent::Confirm).is_none(),
        "A mid-pop does nothing"
    );
    assert_eq!(s.stack.len(), depth, "and pushes nothing");

    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(matches!(s.take_action(), Some(OverlayAction::Quit)));
}

/// A completed pop frees the carried screen. Hint rects publish only at
/// `Motion::None` — mid-transition they are slid and scaled.
#[test]
fn a_completed_pop_frees_its_screen_and_republishes_hints() {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let (w, h) = (640u32, 400u32);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Secondary);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Back);

    assert!(
        matches!(
            &s.motion,
            Motion::Nav {
                leaving: Some(_),
                ..
            }
        ),
        "the popped screen is parked on the motion"
    );
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);
    assert!(
        s.hint_rects.is_empty(),
        "mid-transition the drawn rects are slid and scaled, so none are published"
    );

    run_motion(&mut s);
    assert!(matches!(s.motion, Motion::None));
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);
    assert!(
        !s.hint_rects.is_empty(),
        "settled, the legend is clickable again"
    );
}

/// One small frame. A freshly pushed screen adopts the shared model on its first
/// sync; asserting on content before that is asking before the answer exists.
fn frame(s: &mut Shell) {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((480, 300)).unwrap();
    s.render(surface.canvas(), 480, 300, &fonts, None, None, &pads);
}

fn mixed_library(library: &LibraryShared) {
    let g = |id: &str, title: &str, store: &str, platform: Option<&str>, launcher: bool| {
        crate::library::LibraryGame {
            id: id.into(),
            title: title.into(),
            store: store.into(),
            launcher,
            icon: String::new(),
            platform: platform.map(str::to_string),
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        }
    };
    library.set_games(vec![
        g("l1", "Steam Big Picture", "steam", None, true),
        g("s1", "Dota 2", "steam", None, false),
        g("s2", "Half-Life", "steam", None, false),
        g("p1", "Demon's Souls", "custom", Some("PS3"), false),
        g("p2", "The Last of Us", "custom", Some("PS3"), false),
        g("n1", "Super Metroid", "custom", Some("SNES"), false),
    ]);
}

/// OK on a Collections tile opens that platform's shelf; Back returns to the tab.
#[test]
fn a_collection_tile_opens_one_platform_and_backs_out() {
    let (mut s, _console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    // The row above the field, so the tab seats on its first tile.
    s.settings.library_sections = "collections,games".into();
    s.sync();
    mixed_library(&library);
    s.handle_menu(MenuEvent::JumpForward); // R1 → Games, this host's shelf
    finish_motion(&mut s);
    assert!(matches!(s.stack.last(), Some(Screen::Library(_))));

    // Tiles sort A–Z with the launchers left out: PS3, SNES, Steam.
    s.handle_menu(MenuEvent::Confirm);
    finish_motion(&mut s);
    frame(&mut s); // the new shelf adopts the shared model on its first sync
    let Some(Screen::Library(shelf)) = s.stack.last() else {
        panic!("A on a collection tile opens a shelf");
    };
    assert_eq!(shelf.len_for_test(), 2, "PS3 has exactly its two titles");
    assert!(
        shelf.title().ends_with("PS3"),
        "the breadcrumb names the collection: {}",
        shelf.title()
    );

    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    frame(&mut s);
    let Some(Screen::Library(shelf)) = s.stack.last() else {
        panic!("back to the Games tab");
    };
    assert_eq!(
        shelf.len_for_test(),
        5,
        "the whole library again; the desktop and the launcher sit in their bands"
    );
}

/// Rescan sits past Add Host and must never start a session: accidental A on the
/// end of the strip raises a scan, not a Launch.
#[test]
fn the_rescan_tile_probes_and_never_connects() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    // Hosts, then Add Host, then Rescan — walk to the end.
    for _ in 0..12 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    assert!(
        s.stack
            .last()
            .is_some_and(|sc| matches!(sc, Screen::Home(_))),
        "still on the home carousel"
    );
    s.handle_menu(MenuEvent::Confirm);

    assert!(
        s.take_action().is_none(),
        "a scan must raise no Launch, and no Quit"
    );
    assert!(s.connecting.is_none(), "and must not open the connect card");
    assert_eq!(s.stack.len(), 1, "and must push no screen");
    assert!(s.toast.is_some(), "it says it is scanning");
    // One step back is Add Host, which does push — proof the walk reached the end.
    s.handle_menu(MenuEvent::Move(MenuDir::Left));
    s.handle_menu(MenuEvent::Confirm);
    assert!(
        matches!(s.stack.last(), Some(Screen::AddHost(_))),
        "the tile before Rescan is Add Host"
    );
}

/// Error tint is fixed, not palette-derived. `jade` accent is green; reporting
/// a failure in the colour the rest of the UI uses for "this is fine" is the bug.
#[test]
fn toast_kinds_are_visually_distinct() {
    use crate::shell::{ToastKind, ToastMark};
    let (info_c, info_m) = ToastKind::Info.look();
    let (ok_c, ok_m) = ToastKind::Success.look();
    let (err_c, err_m) = ToastKind::Error.look();
    assert_eq!(info_m, ToastMark::Dot);
    assert_eq!(ok_m, ToastMark::Check);
    assert_eq!(err_m, ToastMark::Bang);
    let rgb = |c: skia_safe::Color4f| (c.r, c.g, c.b);
    assert_ne!(rgb(info_c), rgb(ok_c));
    assert_ne!(rgb(ok_c), rgb(err_c));

    // Green-accented palette: Success follows it, Error must not.
    crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("jade")));
    let (ok_jade, _) = ToastKind::Success.look();
    let (err_jade, _) = ToastKind::Error.look();
    assert_ne!(
        rgb(ok_jade),
        rgb(ok_c),
        "Success takes the palette's accent, so it moved"
    );
    assert_eq!(
        rgb(err_jade),
        rgb(err_c),
        "Error is fixed and must NOT follow the palette"
    );
}

/// Reduced motion freezes `field_clock` and shortens the spring. Asserted on the
/// clock, not pixels: `draw_aurora` has one time read, and both callers go through it.
#[test]
fn reduce_motion_freezes_the_field_and_shortens_the_transition() {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let (w, h) = (1280u32, 800u32);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w as i32, h as i32)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    let _slot = crate::os_theme::REDUCE_MOTION_TEST.lock().unwrap();

    assert!(!s.settings.reduce_motion, "off by default");
    assert_eq!(s.field_clock(12.5), 12.5);
    assert_eq!(s.nav_spec().damping, crate::anim::springs::NAV.damping);

    s.settings.reduce_motion = true;
    assert_eq!(s.field_clock(12.5), 0.0, "the field stops drifting");
    let spec = s.nav_spec();
    assert_eq!(
        spec.damping, 1.0,
        "critically damped: it arrives, never bounces"
    );
    assert!(
        spec.response < crate::anim::springs::NAV.response,
        "and quicker"
    );
    // Shader still draws at t = 0.
    s.render(surface.canvas(), w, h, &fonts, None, None, &pads);

    // Persist through the store so a restart keeps it.
    s.store.save(&s.settings);
    assert!(s.store.load().reduce_motion, "persisted");
    s.settings.reduce_motion = false;
    s.store.save(&s.settings);
    assert!(!s.store.load().reduce_motion, "and back off again");
}

/// An OS that answers wins over the console's own row, which then leaves Settings; one that
/// stops answering hands both back.
#[test]
fn the_os_reduce_motion_wins_and_hides_the_row() {
    let (s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    let _slot = crate::os_theme::REDUCE_MOTION_TEST.lock().unwrap();
    assert!(!s.reduce_motion());
    crate::os_theme::set_os_reduce_motion(Some(true));
    assert!(s.reduce_motion(), "the OS asked for less motion");
    assert_eq!(s.field_clock(12.5), 0.0);
    crate::os_theme::set_os_reduce_motion(None);
    assert!(!s.reduce_motion(), "no answer: the stored setting again");
}

/// The backdrop keeps its offscreen and re-renders only when an input moves: a frame
/// inside `FIELD_STEP` blits the cached field, a bigger clock move or a new size
/// re-renders, and the reduced flag only picks the buffer's size.
#[test]
fn the_backdrop_caches_its_field() {
    let fonts = crate::theme::build_fonts().unwrap();
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.settings
        .extra
        .insert("android.reduce_ui_resolution".into(), true.into());
    // A fixed clock, advanced by hand: cache hits and misses must not hinge on
    // how slow a debug raster is.
    s.fake_clock = Some((0.0, 0.0));

    let mut frame = |s: &mut Shell, t: f64| {
        s.fake_clock = Some((t, 0.0));
        s.render(surface.canvas(), 1280, 800, &fonts, None, None, &pads);
        s.field.borrow().as_ref().map(|c| (c.size, c.t))
    };

    assert_eq!(frame(&mut s, 0.0), Some(((192, 120), 0.0)));
    // Inside FIELD_STEP the cached field is blitted, not re-rendered.
    assert_eq!(frame(&mut s, FIELD_STEP / 2.0), Some(((192, 120), 0.0)));
    // Past it the field re-renders at the new clock.
    assert_eq!(
        frame(&mut s, FIELD_STEP + 0.01),
        Some(((192, 120), FIELD_STEP + 0.01))
    );

    // A new target size invalidates the offscreen; 480 wide still scales to the edge.
    let mut small = skia_safe::surfaces::raster_n32_premul((480, 300)).unwrap();
    s.fake_clock = Some((2.0, 0.0));
    s.render(small.canvas(), 480, 300, &fonts, None, None, &pads);
    assert_eq!(s.field.borrow().as_ref().map(|c| c.size), Some((192, 120)));

    // Flag off: still the offscreen, now at the full edge — 1280 wide is 512.
    s.settings
        .extra
        .insert("android.reduce_ui_resolution".into(), false.into());
    s.fake_clock = Some((3.0, 0.0));
    s.render(surface.canvas(), 1280, 800, &fonts, None, None, &pads);
    assert_eq!(s.field.borrow().as_ref().map(|c| c.size), Some((512, 320)));
}

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
    s.platform = crate::platform::Platform::Android;
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
        s.platform = crate::platform::Platform::Android;
        s.settings.ui_palette = "violet".into();
        s.fake_clock = Some((0.0, 1.0 / 60.0));
        s
    };

    let mut s = store_shell(
        vec![Screen::Home(HomeScreen::new())],
        LibraryShared::default(),
    );
    frames(&mut s, 30);
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    save(frames(&mut s, 90), "tv-console-home");

    // Art before the list, so the shelf decodes it before the entrance arms.
    let shelf = || {
        let library = LibraryShared::default();
        for (i, (title, ..)) in STORE_TITLES.iter().enumerate() {
            library.push_art(format!("steam:{i}"), store_poster(i, title, &fonts));
        }
        library.set_games(store_games());
        let host = store_hosts()[2].clone();
        let mut s = store_shell(
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&host)),
            ],
            library,
        );
        frames(&mut s, 60);
        // Past the Desktop and Steam tiles onto the first title.
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
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
        name: name.into(),
        addr: format!("192.168.1.{octet}"),
        port: 9777,
        fp_hex: if paired {
            format!("fp{octet}")
        } else {
            String::new()
        },
        paired,
        saved: paired,
        online,
        mgmt_port: 47990,
        can_wake: paired && !online,
        clipboard_sync: false,
        last_used: None,
        os: os.into(),
        actions: Vec::new(),
        pin: None,
        bound_preset: None,
        running: String::new(),
        game_presets: Default::default(),
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

/// Bounding box of lit pixels: `(left, right, bottom)`. White ink on black, any channel.
fn ink_bounds(surface: &mut skia_safe::Surface, w: i32, h: i32) -> (i32, i32, i32) {
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    let (mut left, mut right, mut bottom) = (i32::MAX, i32::MIN, i32::MIN);
    for (i, px) in pixels.chunks_exact(4).enumerate() {
        if px[0] > 60 {
            let (x, y) = (i as i32 % w, i as i32 / w);
            left = left.min(x);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    assert!(left <= right, "nothing was drawn");
    (left, right, bottom)
}

/// Compared to an unclamped control of the same string so Geist defines the line
/// box: the clamped heading must occupy the same one line the unclamped one does.
#[test]
fn a_heading_starts_on_its_column_and_never_takes_a_second_line() {
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (1200, 200);
    let (x, y, size) = (crate::theme::EDGE_INSET, 18.0, 30.0);
    let title = "Living Room PC · Performance · PlayStation 3";
    let render = |max_w: f64| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        surface
            .canvas()
            .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
        fonts.heading(
            surface.canvas(),
            title,
            crate::theme::W::Bold,
            size,
            skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
            x,
            y,
            max_w,
        );
        ink_bounds(&mut surface, w, h)
    };

    // One line; a cap's left sidebearing sits a pixel or two right of origin, never left.
    let (loose_left, loose_right, loose_bottom) = render(1100.0);
    assert!(
        (loose_left as f64) >= x - 1.0 && (loose_left as f64) < x + 0.1 * 1100.0,
        "heading ink starts at {loose_left}, which is not the {x} column"
    );
    assert!(
        loose_right < w,
        "the control render was clipped by the surface"
    );

    // Ellipsize inside the budget instead of wrapping: right edge at the budget, same bottom.
    let budget = 300.0;
    let (tight_left, tight_right, tight_bottom) = render(budget);
    assert_eq!(
        tight_left, loose_left,
        "clamping the width must not move the heading's left edge"
    );
    assert!(
        (tight_right as f64) <= x + budget + 1.0,
        "heading ran to {tight_right}, past its {} budget",
        x + budget
    );
    assert!(
        tight_bottom <= loose_bottom + 1,
        "heading wrapped to a second line: it reaches {tight_bottom} where one line ends at \
         {loose_bottom}"
    );
}

/// Skia defaults `SkPaint::fAntiAlias` to false, so `Paint::new(colour, None)` hard-steps.
/// Asserted on a lone circle: a full render hides a few dozen jagged pixels in 1.02 M,
/// and no threshold that catches them survives a palette tweak. With AA the boundary
/// is a ring of partial coverage; without it every pixel is one of two values.
#[test]
fn geometry_is_anti_aliased() {
    let (w, h) = (64, 64);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
    // Off the pixel grid: a half-pixel centre has an edge that cannot be exact, so AA matters.
    surface.canvas().draw_circle(
        skia_safe::Point::new(31.5, 31.5),
        20.3,
        &crate::theme::fill(skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0)),
    );

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    // White on black: all three channels agree, so red alone is enough.
    let partial = pixels
        .chunks_exact(4)
        .filter(|px| (8..248).contains(&px[0]))
        .count();
    assert!(
        partial > 40,
        "an anti-aliased circle of r≈20 has a boundary ring of partially covered pixels; found \
         {partial}, which is what `Paint::new`'s aliased default looks like"
    );
}

/// Skia modulates a shader by the paint's alpha. `Paint::default` is opaque black so
/// gradients never noticed; a transparent "shader supplies the colour" placeholder
/// draws nothing. `theme::shaded` is opaque by construction; this holds it to that.
#[test]
fn a_shaded_paint_is_opaque_enough_to_draw() {
    let (w, h) = (32, 32);
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    surface
        .canvas()
        .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
    let mut p = crate::theme::shaded();
    let stops = [
        skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
        skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0),
    ];
    p.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (
            skia_safe::Point::new(0.0, 0.0),
            skia_safe::Point::new(0.0, h as f32),
        ),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(
                &stops,
                skia_safe::TileMode::Clamp,
                None,
            ),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    surface
        .canvas()
        .draw_rect(skia_safe::Rect::from_wh(w as f32, h as f32), &p);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    let lit = pixels.chunks_exact(4).filter(|px| px[0] > 200).count();
    assert_eq!(
        lit,
        (w * h) as usize,
        "an opaque white gradient over the whole surface should light every pixel; a paint \
         whose own alpha is 0 scales the shader away and leaves the field black"
    );
}

/// Every paint in the crate is built by `theme::fill`/`stroke`/`layer`. A pixel test
/// only witnesses the shapes it draws; this witnesses the class. `&Paint::new(c, None)`
/// is the natural inline spelling, so it reappears in whichever file is being written.
#[test]
fn paints_are_built_by_the_theme_constructors() {
    // Concat so the needles do not appear in this file — the scan reads its own source.
    let needles = [concat!("Paint", "::new("), concat!("Paint", "::default()")];
    let mut offenders = Vec::new();
    let mut stack = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src"
    ))];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("the crate's own src is readable") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            // theme.rs holds the sanctioned constructors; raw paints are allowed there only.
            if path.file_name().is_some_and(|f| f == "theme.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("source is UTF-8");
            for (n, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with('*') {
                    continue;
                }
                if needles.iter().any(|needle| code.contains(needle)) {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    offenders.push(format!("{name}:{}: {code}", n + 1));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these build a Skia paint directly, which means anti-aliasing is OFF on whatever they \
         draw — use `theme::fill`, `theme::stroke`, or `theme::layer` for a `save_layer` \
         paint:\n  {}",
        offenders.join("\n  ")
    );
}

// --- Launch hold -------------------------------------------------------------------

mod launch_hold {
    use super::*;
    use crate::library::LibraryGame;
    use pf_client_core::library::RunningGame;

    fn game(id: &str, title: &str, launcher: bool) -> LibraryGame {
        LibraryGame {
            id: id.into(),
            title: title.into(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: Some("PC".into()),
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        }
    }

    fn running(id: &str, state: &str) -> Vec<RunningGame> {
        vec![RunningGame {
            app_id: Some(id.into()),
            title: String::new(),
            state: state.into(),
            awaiting_window: false,
        }]
    }

    fn intent(id: &str) -> ConnectIntent {
        ConnectIntent {
            addr: "10.0.0.1".into(),
            port: 47989,
            fp_hex: "aa11".into(),
            launch: Some(id.into()),
            title: "Deck".into(),
            request_access: false,
            preset: None,
        }
    }

    /// A shell standing on a shelf, with the bus kept so the poll can be witnessed.
    fn on_shelf() -> (Shell, LibraryShared, ConsoleBus) {
        fake_home();
        let console = ConsoleShared::default();
        console.set_hosts(hosts());
        let library = LibraryShared::default();
        let bus = ConsoleBus::default();
        let mut s = Shell::new(
            console,
            library.clone(),
            bus.clone(),
            test_options(),
            vec![
                Screen::Home(HomeScreen::new()),
                Screen::Library(LibraryScreen::new(&hosts()[0])),
            ],
        )
        .unwrap();
        s.fake_clock = Some((100.0, 0.0));
        library.set_games(vec![
            game("steam:570", "Dota 2", false),
            game("steam:ui", "Big Picture", true),
        ]);
        (s, library, bus)
    }

    fn at(s: &mut Shell, t: f64) {
        s.fake_clock = Some((t, 0.0));
    }

    #[test]
    fn a_launched_title_holds_the_stream_until_the_host_says_running() {
        let (mut s, library, bus) = on_shelf();
        s.start_connect(intent("steam:570"));
        assert!(matches!(
            s.take_action(),
            Some(OverlayAction::Launch { .. })
        ));
        assert!(
            s.holds_stream(),
            "the hold is up from the press, not the first frame"
        );
        assert!(
            s.connecting.is_none(),
            "and it replaces the connect card rather than stacking on it"
        );
        assert_eq!(
            s.launching.as_ref().map(|l| l.facts.as_str()),
            Some("PC · Steam"),
            "platform, year, store — this mock has no year"
        );

        // Nothing to ask the host until there is a session behind the launch.
        s.sync();
        assert!(
            bus.drain().is_empty(),
            "no lease exists before the dial lands"
        );

        s.session_streaming();
        assert!(
            s.holds_stream() && !s.in_stream,
            "the handshake alone reveals nothing"
        );
        s.sync();
        assert!(
            bus.drain()
                .iter()
                .any(|c| matches!(c, ConsoleCmd::RefreshRunning { mgmt: 47990, .. })),
            "the hold asks the shelf's host"
        );
        // The next poll waits for that answer, then a second.
        at(&mut s, 101.5);
        s.sync();
        assert!(bus.drain().is_empty(), "no answer yet, no second question");
        library.set_running(&running("steam:570", "launching"));
        at(&mut s, 101.5);
        s.sync();
        assert!(s.holds_stream(), "launching is the wait itself");
        assert!(!bus.drain().is_empty(), "answer landed and a second passed");

        library.set_running(&running("steam:570", "running"));
        s.sync();
        assert!(!s.holds_stream() && s.in_stream);
    }

    /// B belongs to the dial while it is in flight: the hold stands where the connect
    /// card used to, so it has to answer for it.
    #[test]
    fn back_cancels_the_dial_while_the_hold_is_still_connecting() {
        let (mut s, _library, _bus) = on_shelf();
        s.start_connect(intent("steam:570"));
        assert!(matches!(
            s.take_action(),
            Some(OverlayAction::Launch { .. })
        ));
        s.handle_menu(MenuEvent::Back);
        assert!(matches!(
            s.take_action(),
            Some(OverlayAction::CancelConnect)
        ));
        assert!(
            !s.holds_stream() && !s.in_stream,
            "cancelled back onto the shelf, not into a stream"
        );
    }

    #[test]
    fn the_hold_ignores_state_read_before_the_launch_and_says_so_without_a_lease() {
        let (mut s, library, _bus) = on_shelf();
        // The shelf's own refresh, from before this launch: the previous copy exited.
        library.set_running(&running("steam:570", "exited"));
        s.start_connect(intent("steam:570"));
        s.session_streaming();
        s.sync();
        assert!(
            s.holds_stream(),
            "a read from before the launch is not this launch"
        );
        at(&mut s, 100.0 + LAUNCH_NO_LEASE);
        s.sync();
        // Sliding away here is what #1072 calls the silence: the player lands on a desktop
        // they did not ask for and cannot tell a refusal from a slow start.
        assert!(
            !s.in_stream && s.holds_stream(),
            "the hold keeps the screen"
        );
        assert_eq!(
            s.launching.as_ref().and_then(|l| l.failed.as_deref()),
            Some("The host didn't start Dota 2 — nothing is running for it.")
        );
        s.handle_menu(MenuEvent::Confirm);
        assert!(s.in_stream, "and a press is \"show the desktop anyway\"");
    }

    /// The three sentences, against the host's own `games[]` words. The touch shell's
    /// `launchGaveUp` answers the same way; a report quotes one line either way.
    #[test]
    fn the_hold_gives_up_on_the_states_that_produced_no_game() {
        let long = LAUNCH_HOLD_MAX + 1.0;
        assert_eq!(
            crate::shell::launch_gave_up("Eden", Some("exited"), 0.5).as_deref(),
            Some("Eden closed right after starting.")
        );
        assert_eq!(
            crate::shell::launch_gave_up("Eden", Some("launching"), long).as_deref(),
            Some("Eden is still starting after 2 minutes.")
        );
        assert!(crate::shell::launch_gave_up("Eden", Some("launching"), 1.0).is_none());
        assert!(crate::shell::launch_gave_up("Eden", None, 1.0).is_none());
        // A launch that worked never produces a sentence, whatever it is doing.
        for word in ["running", "window", "untracked", "grace"] {
            assert!(
                crate::shell::launch_gave_up("Eden", Some(word), long).is_none(),
                "{word} is a launch that worked"
            );
        }
    }

    /// A host that can see windows keeps the hold up past `running` until `window`, and a
    /// window that never comes still lets go at the cap.
    #[test]
    fn the_hold_waits_for_the_window_where_the_host_reports_one() {
        let (mut s, library, _bus) = on_shelf();
        let loading = |state: &str| {
            let mut g = running("steam:570", state);
            g[0].awaiting_window = true;
            g
        };
        s.start_connect(intent("steam:570"));
        s.session_streaming();
        library.set_running(&loading("running"));
        s.sync();
        assert!(s.holds_stream(), "running, with a window still to come");
        assert!(s.launching.as_ref().is_some_and(|l| l.window_wait));
        library.set_running(&running("steam:570", "window"));
        s.sync();
        assert!(s.in_stream && !s.holds_stream(), "the window is up");

        s.session_ended(None);
        s.start_connect(intent("steam:570"));
        s.session_streaming();
        library.set_running(&loading("running"));
        at(&mut s, 100.0 + LAUNCH_HOLD_MAX);
        s.sync();
        assert!(s.in_stream, "no window by the cap: show what is there");
    }

    #[test]
    fn launcher_tiles_skip_the_hold_and_a_press_ends_it() {
        let (mut s, _library, _bus) = on_shelf();
        s.start_connect(intent("steam:ui"));
        assert!(
            s.connecting.is_some(),
            "a launcher tile keeps the plain connect card"
        );
        s.session_streaming();
        assert!(
            s.in_stream && !s.holds_stream(),
            "the host never tracks a launcher"
        );

        s.session_ended(None);
        s.start_connect(intent("steam:570"));
        s.session_streaming();
        assert!(s.holds_stream());
        assert!(s.handle_menu(MenuEvent::Move(MenuDir::Left)).is_none());
        assert!(s.holds_stream(), "a nudge is not a request to see");
        s.handle_menu(MenuEvent::Confirm);
        assert!(s.in_stream && !s.holds_stream());
    }
}

/// The console writes the GLOBAL bitrate and has no preset editor, so a measurement is only
/// applicable when the tested host actually resolves bitrate from that layer. A preset that
/// PINS one makes the answer read-only; a preset that inherits does not.
#[test]
fn apply_is_offered_only_when_the_default_is_the_layer_that_wins() {
    let done = SpeedPhase::Done {
        throughput_kbps: 100_000,
        loss_pct: 0.3,
        recommended_kbps: 70_000,
    };
    let chip = |bitrate_kbps| {
        Some(crate::model::PresetChip {
            id: "work".into(),
            name: "Work".into(),
            accent: None,
            bitrate_kbps,
        })
    };
    for (bound, want) in [
        (None, Some(70_000)),
        // Inherits bitrate: the default is still what this host streams at.
        (chip(None), Some(70_000)),
        (chip(Some(20_000)), None),
    ] {
        let mut rows = hosts();
        rows[0].bound_preset = bound;
        let (mut s, console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
        console.set_hosts(rows);
        console.set_speed(Some(SpeedStatus::new(
            "aa11".into(),
            "Living Room PC".into(),
        )));
        console.advance_speed("aa11", done.clone());
        s.sync();
        assert_eq!(s.speed_recommendation(), want);
    }
}

/// Mid-burst reports build the graph's trace and keep the test measuring; one that lands
/// after the answer changes nothing.
#[test]
fn progress_reports_trace_the_burst() {
    let (mut s, console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    console.set_speed(Some(SpeedStatus::new(
        "aa11".into(),
        "Living Room PC".into(),
    )));
    for kbps in [200_000, 600_000, 850_000] {
        console.advance_speed("aa11", SpeedPhase::Progress { kbps });
    }
    s.sync();
    let sp = s.speed.clone().expect("measuring");
    assert_eq!(sp.phase, SpeedPhase::Measuring);
    let kbps: Vec<u32> = sp.trace.iter().map(|p| p.1).collect();
    assert_eq!(kbps, [200_000, 600_000, 850_000]);
    assert!(
        sp.trace.windows(2).all(|w| w[0].0 <= w[1].0),
        "stamped in order"
    );

    let done = SpeedPhase::Done {
        throughput_kbps: 840_000,
        loss_pct: 0.1,
        recommended_kbps: 588_000,
    };
    console.advance_speed("aa11", done.clone());
    console.advance_speed("aa11", SpeedPhase::Progress { kbps: 1 });
    s.sync();
    let sp = s.speed.clone().expect("done");
    assert_eq!((sp.phase, sp.trace.len()), (done, 3));
}

/// The burst outlives a dismiss: the host finishes it either way. Its report must not reopen
/// the takeover over whatever the player moved on to.
#[test]
fn a_dismissed_speed_test_drops_its_late_result() {
    let (mut s, console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    console.set_speed(Some(SpeedStatus::new(
        "aa11".into(),
        "Living Room PC".into(),
    )));
    s.sync();
    assert!(s.speed.is_some());

    s.handle_menu(MenuEvent::Back);
    assert!(s.speed.is_none());

    console.advance_speed(
        "aa11",
        SpeedPhase::Done {
            throughput_kbps: 100_000,
            loss_pct: 0.0,
            recommended_kbps: 70_000,
        },
    );
    s.sync();
    assert!(s.speed.is_none(), "a cleared slot must stay cleared");
}

/// Dismiss one test, start another, and the first burst still reports. Keyed, so it cannot
/// land under the second host's name — the number would be measured against the wrong box.
#[test]
fn a_superseded_speed_test_cannot_report_under_the_new_host() {
    let (mut s, console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    console.set_speed(Some(SpeedStatus::new("bb22".into(), "Bedroom".into())));
    s.sync();

    console.advance_speed(
        "aa11",
        SpeedPhase::Done {
            throughput_kbps: 100_000,
            loss_pct: 0.0,
            recommended_kbps: 70_000,
        },
    );
    s.sync();
    assert_eq!(
        s.speed.as_ref().map(|sp| sp.phase.clone()),
        Some(SpeedPhase::Connecting),
        "the abandoned host's report must not land here"
    );
}

/// A screen reader gets the focused tile, and a different string once focus moves — the whole
/// point of the seam: a constant would announce the same host forever.
#[test]
fn the_announcement_follows_the_home_carousel() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    let first = s.focus_announcement().expect("home names its focused tile");
    assert!(first.starts_with("Living Room PC"), "{first}");
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    let second = s
        .focus_announcement()
        .expect("…and the tile it stepped onto");
    assert!(second.starts_with("Office Tower"), "{second}");
    assert_ne!(first, second);
}

/// The trailing action tiles speak their own caption, not a host's.
#[test]
fn the_announcement_names_the_trailing_actions() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    for _ in 0..hosts().len() {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    assert_eq!(
        s.focus_announcement().as_deref(),
        Some("Add Host, Register a host by address")
    );
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    assert_eq!(
        s.focus_announcement().as_deref(),
        Some("Rescan, Look for hosts on this network again")
    );
}

/// A settings row is its label plus the value drawn beside it; the strip names the section.
#[test]
fn the_announcement_carries_a_settings_value() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Tertiary);
    finish_motion(&mut s);
    let row = s.focus_announcement().expect("a settings row names itself");
    assert!(row.starts_with("Aspect ratio, "), "{row}");
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    let below = s.focus_announcement().expect("…and so does the row below");
    assert!(below.starts_with("Resolution, "), "{below}");
    assert_ne!(row, below);
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    assert_eq!(s.focus_announcement().as_deref(), Some("Stream section"));
}

/// Silence, not the wrong row: a screen this driver does not describe, and a takeover that
/// owns the input, both say nothing.
#[test]
fn the_announcement_stays_quiet_where_it_cannot_name_the_focus() {
    let (mut s, _console, _library) = shell(vec![Screen::AddHost(
        crate::screens::add_host::AddHostScreen::new(),
    )]);
    s.sync();
    assert!(s.focus_announcement().is_none());

    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.sync();
    s.handle_menu(MenuEvent::Confirm);
    assert!(s.connecting.is_some());
    assert!(
        s.focus_announcement().is_none(),
        "a takeover owns the input"
    );
}

/// Mean red of each column over the top `rows`, where the takeover draws its field and
/// nothing else. Aurora plus vignette is a smooth gradient, so neighbouring columns differ
/// by a fraction of a level and a backdrop that stops somewhere shows up as one big step.
fn column_means(surface: &mut skia_safe::Surface, w: i32, h: i32, rows: i32) -> Vec<f64> {
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let info = skia_safe::ImageInfo::new_n32_premul((w, h), None);
    assert!(
        surface.read_pixels(&info, &mut pixels, (w * 4) as usize, (0, 0)),
        "raster surface read-back"
    );
    (0..w)
        .map(|x| {
            let sum: u32 = (0..rows)
                .map(|y| u32::from(pixels[((y * w + x) * 4) as usize]))
                .sum();
            f64::from(sum) / f64::from(rows)
        })
        .collect()
}

/// The takeover's backdrop covers the surface, not the safe rect. It used to paint at the
/// inset size under the layout translate, so the cutout strip kept the frame's first aurora
/// with no vignette over it and seamed down the inset edge — barely visible in a capture,
/// obvious on the glass.
#[test]
fn the_takeover_field_reaches_past_a_side_cutout() {
    let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.fake_clock = Some((100.0, 1.0 / 60.0));
    s.set_connecting(Some("Living Room PC".into()));
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h, left) = (400_i32, 240_i32, 60_i32);
    let viewport = crate::console::Viewport {
        width: w as u32,
        height: h as u32,
        insets: crate::console::Insets {
            left: left as f32,
            top: 0.0,
            right: 0.0,
            bottom: 0.0,
        },
        scale: None,
    };
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    // Past the takeover's fade-in: a partly-arrived field is drawn through one alpha layer,
    // which scales down every step it makes along with it.
    for _ in 0..90 {
        s.render_in(surface.canvas(), &viewport, &fonts, None, None, &[]);
    }

    let mean = column_means(&mut surface, w, h, 24);
    let step = |x: usize| (mean[x] - mean[x - 1]).abs();
    let seam = step(left as usize);
    let elsewhere = (1..w as usize)
        .filter(|x| x.abs_diff(left as usize) > 1)
        .map(step)
        .fold(0.0_f64, f64::max);
    assert!(
        seam <= elsewhere,
        "column {left} is the cutout edge and steps {seam:.3} levels from its neighbour, more \
         than the biggest step anywhere else ({elsewhere:.3}) — the field is stopping at the \
         safe rect again"
    );
}

/// Ignored phone dump at an iPhone Pro Max's landscape geometry, for the mockup check.
/// `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --release -- --ignored phone`.
#[test]
#[ignore]
fn dump_phone_home() {
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (2868_i32, 1320_i32);
    let viewport = crate::console::Viewport {
        width: w as u32,
        height: h as u32,
        insets: crate::console::Insets {
            left: 186.0,
            top: 0.0,
            right: 186.0,
            bottom: 63.0,
        },
        scale: Some(2.25),
    };
    let (mut s, console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.fake_clock = Some((0.0, 1.0 / 60.0));
    s.platform = crate::platform::Platform::Apple;
    let dump = |s: &mut Shell, frames: usize, name: &str| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
        for _ in 0..frames {
            s.render_in(surface.canvas(), &viewport, &fonts, None, None, &[]);
        }
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };
    console.set_hosts(Vec::new());
    dump(&mut s, 60, "p0-no-hosts");
    let (mut s, console, library) = shell(vec![Screen::Home(HomeScreen::new())]);
    s.fake_clock = Some((0.0, 1.0 / 60.0));
    s.platform = crate::platform::Platform::Apple;
    dump(&mut s, 60, "p1-home-empty");
    let games = (0..8)
        .map(|i| crate::library::LibraryGame {
            id: format!("steam:{i}"),
            title: [
                "Doom",
                "Hades",
                "Celeste",
                "Portal 2",
                "Tunic",
                "Inside",
                "Limbo",
                "Hollow Knight",
            ][i]
                .into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
        })
        .collect();
    library.set_games(games);
    dump(&mut s, 60, "p2-home-games");
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    dump(&mut s, 60, "p3-home-down");
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    dump(&mut s, 60, "p4-home-down2");
    let styles = |s: &mut Shell, name: &str| {
        use crate::blur::{set_style_override, Style};
        for (style, tag) in [
            (Style::Pixel, "pixel"),
            (Style::PixelFlat, "flat"),
            (Style::Off, "off"),
        ] {
            set_style_override(Some(style));
            dump(s, 2, &format!("{name}-{tag}"));
        }
        set_style_override(None);
    };
    styles(&mut s, "p4-home-down2");
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    s.handle_menu(MenuEvent::Move(MenuDir::Up));
    dump(&mut s, 60, "p5-home-strip");
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    dump(&mut s, 60, "p6-home-offline");
    // The card's menu and details, from the first card's verbs.
    s.handle_menu(MenuEvent::Move(MenuDir::Left));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    for _ in 0..3 {
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
    }
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 60, "p7-card-menu");
    s.handle_menu(MenuEvent::Back);
    // Input waits for the pop to land.
    dump(&mut s, 30, "_popped");
    s.handle_menu(MenuEvent::Move(MenuDir::Left));
    s.handle_menu(MenuEvent::Confirm);
    dump(&mut s, 60, "p8-host-details");
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 30, "_back");
    // The speed test mid-burst, then measured.
    console.set_speed(Some(SpeedStatus::new(
        "aa11".into(),
        "Living Room PC".into(),
    )));
    console.advance_speed("aa11", SpeedPhase::Measuring);
    s.sync();
    for kbps in [120_000, 410_000, 690_000, 810_000, 780_000, 860_000] {
        console.advance_speed("aa11", SpeedPhase::Progress { kbps });
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    dump(&mut s, 30, "pe-speed-measuring");
    console.advance_speed(
        "aa11",
        SpeedPhase::Done {
            throughput_kbps: 842_000,
            loss_pct: 0.3,
            recommended_kbps: 589_400,
        },
    );
    dump(&mut s, 30, "pf-speed-done");
    s.handle_menu(MenuEvent::Back);
    dump(&mut s, 30, "_speed-closed");
    for (tab, name) in [
        (Tab::Games, "p9-games"),
        (Tab::Players, "pa-players"),
        (Tab::Settings, "pb-settings"),
    ] {
        s.switch_tab(tab);
        dump(&mut s, 60, name);
    }
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    dump(&mut s, 60, "pc-settings-rows");
    styles(&mut s, "pc-settings-rows");
    // The typed bitrate: the keyboard over the rows, nothing between.
    s.handle_menu(MenuEvent::Move(MenuDir::Down));
    s.handle_menu(MenuEvent::Secondary);
    dump(&mut s, 60, "pd-settings-keyboard");
}

/// Every device mark at the chip's 15 dp and the card's 44 dp, at k = 1 and 2, then the
/// Controllers tab with the chip naming each of three pads.
/// `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui -- --ignored dump_device_marks`.
#[test]
#[ignore]
fn dump_device_marks() {
    use crate::theme::W;
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let save = |surface: &mut skia_safe::Surface, name: &str| {
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };
    let marks = crate::icons::DEVICE_MARKS;
    // (box dp, k, row height px)
    let rows = [
        (15.0, 1.0, 34.0),
        (44.0, 1.0, 64.0),
        (15.0, 2.0, 50.0),
        (44.0, 2.0, 110.0),
    ];
    let col = 132.0;
    let height = 40.0 + rows.iter().map(|r| r.2).sum::<f64>();
    let mut sheet =
        skia_safe::surfaces::raster_n32_premul(((col * marks.len() as f64) as i32, height as i32))
            .unwrap();
    let c = sheet.canvas();
    c.clear(skia_safe::Color4f::new(0.07, 0.08, 0.1, 1.0));
    let ink = skia_safe::Color4f::new(0.92, 0.93, 0.95, 1.0);
    for (i, (name, icon)) in marks.iter().enumerate() {
        let cx = col * (i as f64 + 0.5);
        let tw = f64::from(fonts.measure(name, W::Medium, 12.0));
        fonts.draw(c, name, cx - tw / 2.0, 24.0, W::Medium, 12.0, ink);
        let mut top = 40.0;
        for (size, k, row_h) in rows {
            let w = size * k;
            crate::glyphs::pad_mark(c, *icon, cx - w / 2.0, top + row_h / 2.0, w, k, ink);
            top += row_h;
        }
    }
    save(&mut sheet, "device-marks");

    let pad = |name: &str, id: &str, pref: GamepadPref| PadInfo {
        name: name.into(),
        key: format!("{}:{name}", id.to_lowercase()),
        pref,
        steam_virtual: false,
        battery: None,
        detail: format!("{id} · gamepad · dpad"),
        forwarded: true,
        rumble: true,
    };
    let pads = vec![
        pad(
            "DualSense Wireless Controller",
            "054C:0CE6",
            GamepadPref::DualSense,
        ),
        pad(
            "Xbox Wireless Controller",
            "045E:0B13",
            GamepadPref::XboxOne,
        ),
        pad("Pro Controller", "057E:2009", GamepadPref::SwitchPro),
    ];
    for (i, chip) in pads.iter().enumerate() {
        let (mut s, _console, _library) = shell(vec![Screen::Players(
            crate::screens::players::PlayersScreen::new(),
        )]);
        s.fake_clock = Some((0.0, 1.0 / 60.0));
        let mut surface = skia_safe::surfaces::raster_n32_premul((1920, 1080)).unwrap();
        for _ in 0..60 {
            s.render(
                surface.canvas(),
                1920,
                1080,
                &fonts,
                Some(&chip.name),
                Some(chip.pref),
                &pads,
            );
        }
        save(&mut surface, &format!("players-chip-{i}"));
    }
}

/// Ignored eyeball dump: the plate leaving the tab strip, one PNG a frame, on each tab.
/// `PF_CONSOLE_DUMP=<dir> cargo test -p pf-console-ui --release -- --ignored dump_plate_flight`.
#[test]
#[ignore]
fn dump_plate_flight() {
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    let fonts = crate::theme::build_fonts().unwrap();
    let (w, h) = (960_i32, 540_i32);
    let pads: Vec<PadInfo> = Vec::new();
    let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
    let mut run = |s: &mut Shell, frames: usize, name: Option<&str>| {
        for i in 0..frames {
            s.render(
                surface.canvas(),
                w as u32,
                h as u32,
                &fonts,
                None,
                None,
                &pads,
            );
            if let Some(name) = name {
                let png = surface
                    .image_snapshot()
                    .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                    .unwrap();
                std::fs::write(format!("{dir}/{name}-{i:02}.png"), png.as_bytes()).unwrap();
            }
        }
    };
    for (tab, name) in [
        (Tab::Hosts, "hosts"),
        (Tab::Players, "players"),
        (Tab::Settings, "settings"),
    ] {
        let (mut s, _console, _library) = shell(vec![Screen::Home(HomeScreen::new())]);
        s.fake_clock = Some((100.0, 1.0 / 60.0));
        run(&mut s, 30, None);
        s.switch_tab(tab);
        run(&mut s, 60, None);
        while !s.strip_focus {
            s.handle_menu(MenuEvent::Move(MenuDir::Up));
        }
        run(&mut s, 60, None);
        s.handle_menu(MenuEvent::Move(MenuDir::Down));
        run(&mut s, 24, Some(name));
        s.handle_menu(MenuEvent::Move(MenuDir::Right));
        run(&mut s, 16, Some(&format!("{name}-right")));
    }
}
