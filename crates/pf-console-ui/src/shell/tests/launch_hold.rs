//! The launch hold, from the press on a title to its window.

use super::*;
use crate::library::LibraryGame;
use crate::shell::connect::{launch_gave_up, LAUNCH_HOLD_MAX, LAUNCH_NO_LEASE};
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
        endable: false,
        install: None,
    }
}

fn running(id: &str, state: &str) -> Vec<RunningGame> {
    vec![RunningGame {
        app_id: Some(id.into()),
        title: String::new(),
        state: state.into(),
        awaiting_window: false,
        session_id: None,
        endable: false,
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
        profile: None,
        ask: None,
        seat: None,
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

    // Before the dial lands the host may be fetching the title: the hold watches for that.
    s.sync();
    assert!(
        bus.drain()
            .iter()
            .any(|c| matches!(c, ConsoleCmd::RefreshRunning { .. })),
        "the hold asks from the press"
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
        launch_gave_up("Eden", Some("exited"), 0.5).as_deref(),
        Some("Eden closed right after starting.")
    );
    assert_eq!(
        launch_gave_up("Eden", Some("launching"), long).as_deref(),
        Some("Eden is still starting after 2 minutes.")
    );
    assert!(launch_gave_up("Eden", Some("launching"), 1.0).is_none());
    assert!(launch_gave_up("Eden", None, 1.0).is_none());
    // A launch that worked never produces a sentence, whatever it is doing.
    for word in ["running", "window", "untracked", "grace"] {
        assert!(
            launch_gave_up("Eden", Some(word), long).is_none(),
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

fn downloading(id: &str, state: &str) -> Vec<pf_client_core::library::DownloadProgress> {
    vec![pf_client_core::library::DownloadProgress {
        app_id: id.into(),
        state: state.into(),
        done_bytes: 5,
        total_bytes: Some(10),
        error: Some("the server is down".into()),
        ..Default::default()
    }]
}

/// The host fetches a title's files before it opens the stream: the hold shows them coming,
/// with no cap, and nothing read meanwhile counts as this launch's answer.
#[test]
fn a_title_still_downloading_holds_with_its_progress() {
    let (mut s, library, _bus) = on_shelf();
    s.start_connect(intent("steam:570"));
    library.set_downloads(&downloading("steam:570", "downloading"), None);
    library.set_running(&running("steam:570", "launching"));
    at(&mut s, 100.0 + LAUNCH_HOLD_MAX * 3.0);
    s.sync();
    let l = s.launching.as_ref().expect("still holding");
    assert_eq!(l.download.as_ref().map(|d| d.done_bytes), Some(5));
    assert!(l.failed.is_none(), "no cap runs while the files come");

    library.set_downloads(&[], None);
    s.session_streaming();
    assert!(s.launching.as_ref().is_some_and(|l| l.download.is_none()));
    s.sync();
    assert!(
        s.holds_stream(),
        "the pre-dial `launching` is not this session's"
    );
    library.set_running(&running("steam:570", "running"));
    s.sync();
    assert!(s.in_stream);
}

/// A download that stopped is why nothing started: the hold says so at once, rather than
/// waiting out the no-lease clock.
#[test]
fn a_download_that_stopped_is_the_holds_sentence() {
    let (mut s, library, _bus) = on_shelf();
    s.start_connect(intent("steam:570"));
    s.session_streaming();
    library.set_downloads(&downloading("steam:570", "failed"), None);
    library.set_running(&[]);
    s.sync();
    assert_eq!(
        s.launching.as_ref().and_then(|l| l.failed.as_deref()),
        Some("Dota 2 didn't download \u{2014} the server is down")
    );
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
