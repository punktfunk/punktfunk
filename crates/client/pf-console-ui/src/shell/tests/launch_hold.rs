//! The launch hold, from the press on a title to its window.

use super::*;
use crate::library::LibraryGame;
use crate::shell::connect::{
    launch_gave_up, LAUNCH_HOLD_MAX, LAUNCH_NO_LEASE, LAUNCH_POLL_STALL, LOCAL_GONE,
};
use pf_client_core::library::{LocalLaunch, RunningGame};

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
        overrides: None,
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
        plane: String::new(),
        outcome: None,
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
        local: false,
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
    library.set_games(
        &library.req(),
        vec![
            game("steam:570", "Dota 2", false),
            game("steam:ui", "Big Picture", true),
        ],
    );
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
        s.connect.card().is_none(),
        "and it replaces the connect card rather than stacking on it"
    );
    assert_eq!(
        s.connect.launching().map(|l| l.facts.as_str()),
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

/// The hold says what the host set for the game; a title it set nothing for shows no line.
#[test]
fn the_hold_names_what_the_host_set_for_the_game() {
    let (mut s, library, _bus) = on_shelf();
    let mut dota = game("steam:570", "Dota 2", false);
    dota.overrides = Some(pf_client_core::library::GameOverrides {
        lines: Some(1080),
        hz: Some(60),
        gamepad: Some("dualsense".into()),
        ..Default::default()
    });
    library.set_games(&library.req(), vec![dota]);
    s.start_connect(intent("steam:570"));
    assert_eq!(
        s.connect.launching().map(|l| l.host_line.as_str()),
        Some("Host: 1080p60, DualSense")
    );
    let (mut plain, _library, _bus) = on_shelf();
    plain.start_connect(intent("steam:570"));
    assert_eq!(
        plain.connect.launching().map(|l| l.host_line.as_str()),
        Some("")
    );
}

/// B belongs to the dial while it is in flight: the hold takes the connect card's place,
/// so it has to answer for it.
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
    // Sliding away here would land the player on a desktop they did not ask for, unable to
    // tell a refusal from a slow start.
    assert!(
        !s.in_stream && s.holds_stream(),
        "the hold keeps the screen"
    );
    assert_eq!(
        s.connect.launching().and_then(|l| l.failed.as_deref()),
        Some("The host didn't start Dota 2 — nothing is running for it.")
    );
    s.handle_menu(MenuEvent::Confirm);
    assert!(s.in_stream, "and a press is \"show the desktop anyway\"");
}

/// The host's own sentence for a launch it did not start ends the hold at once; one that lands
/// with no hold up is dropped, never said on the next launch.
#[test]
fn the_hosts_refusal_is_the_holds_sentence_at_once() {
    let (mut s, _library, bus) = on_shelf();
    s.console.set_launch_refused("stale".into());
    s.sync();
    s.start_connect(intent("steam:570"));
    s.session_streaming();
    s.sync();
    assert!(s.connect.launching().is_some_and(|l| l.failed.is_none()));
    bus.drain();
    let hidden = "Couldn't start that game \u{2014} it isn't in this host's library anymore.";
    s.console.set_launch_refused(hidden.into());
    at(&mut s, 100.0 + LAUNCH_POLL_STALL);
    s.sync();
    assert_eq!(
        s.connect.launching().and_then(|l| l.failed.as_deref()),
        Some(hidden)
    );
    assert!(bus.drain().is_empty(), "and it asks the host nothing more");
    assert!(
        !s.in_stream && s.holds_stream(),
        "the hold keeps the screen"
    );
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
    assert!(s.connect.launching().is_some_and(|l| l.window_wait));
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
    let l = s.connect.launching().expect("still holding");
    assert_eq!(l.download.as_ref().map(|d| d.done_bytes), Some(5));
    assert!(l.failed.is_none(), "no cap runs while the files come");

    library.set_downloads(&[], None);
    s.session_streaming();
    assert!(s.connect.launching().is_some_and(|l| l.download.is_none()));
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
        s.connect.launching().and_then(|l| l.failed.as_deref()),
        Some("Dota 2 didn't download \u{2014} the server is down")
    );
}

#[test]
fn launcher_tiles_skip_the_hold_and_a_press_ends_it() {
    let (mut s, _library, _bus) = on_shelf();
    s.start_connect(intent("steam:ui"));
    assert!(
        s.connect.card().is_some(),
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

/// A host that asks the shell whether it holds (Android's `ShowStream` wait) must read false for
/// every session the shell did not launch off its shelf, or that session is never released.
#[test]
fn only_a_game_launched_off_the_shelf_holds() {
    let (mut s, _library, _bus) = on_shelf();
    // A link dials with no `Launch` from the shell: the handshake finds no hold to wait on.
    s.session_streaming();
    assert!(!s.holds_stream() && s.in_stream);

    s.session_ended(None);
    let mut desktop = intent("steam:570");
    desktop.launch = None;
    s.start_connect(desktop);
    s.session_streaming();
    assert!(
        !s.holds_stream(),
        "a desktop connect has no title to wait for"
    );

    s.session_ended(None);
    s.start_connect(intent("steam:unlisted"));
    s.session_streaming();
    assert!(
        !s.holds_stream(),
        "a title the shelf does not list is not held"
    );

    s.session_ended(None);
    s.start_connect(intent("steam:570"));
    assert!(s.holds_stream(), "a listed game off the shelf is");
}

/// The same title on this device's own host.
fn here(id: &str) -> ConnectIntent {
    ConnectIntent {
        local: true,
        ..intent(id)
    }
}

/// The host's row for a title on its own screen.
fn desk(id: &str, state: &str) -> Vec<RunningGame> {
    let mut rows = running(id, state);
    rows[0].plane = "local".into();
    rows
}

/// A shell on its shelf whose hold for Dota 2 on this device has the host's answer.
fn started_here() -> (Shell, LibraryShared, ConsoleBus) {
    let (mut s, library, bus) = on_shelf();
    s.start_connect(here("steam:570"));
    s.console.set_local_launch(LocalLaunch::Started);
    s.sync();
    (s, library, bus)
}

/// A title on this device starts there: the shell asks the host and never dials. The hold
/// covers no stream; `running` lets go, `exited` brings the shelf back.
#[test]
fn a_title_on_this_device_starts_there_and_never_dials() {
    let (mut s, library, bus) = on_shelf();
    s.start_connect(here("steam:570"));
    assert!(s.take_action().is_none(), "nothing dials");
    let cmds = bus.drain();
    assert!(cmds.iter().any(|c| matches!(c,
        ConsoleCmd::LaunchLocal { mgmt: 47990, port: 47989, fp_hex, app_id }
            if fp_hex == "aa11" && app_id == "steam:570")));
    assert!(s
        .connect
        .launching()
        .is_some_and(|l| l.local && !l.connected));
    assert!(!s.holds_stream(), "a hold on this device covers no stream");

    s.console.set_local_launch(LocalLaunch::Started);
    s.sync();
    assert!(s.connect.launching().is_some_and(|l| l.connected));
    library.set_running(&desk("steam:570", "launching"));
    s.sync();
    assert!(s.connect.launching().is_some() && !s.playing_here());
    library.set_running(&desk("steam:570", "running"));
    s.sync();
    assert!(s.playing_here(), "running lets go");
    assert!(s.take_action().is_none() && !s.in_stream);

    library.set_running(&desk("steam:570", "exited"));
    s.sync();
    assert!(s.connect.takeover().is_none() && !s.playing_here());
    assert!(
        matches!(s.top(), Some(Screen::Library(_))),
        "on the shelf it left"
    );
    assert!(
        bus.drain()
            .iter()
            .any(|c| matches!(c, ConsoleCmd::RefreshRunning { .. })),
        "which asks the host what it has up now"
    );
}

/// Focus alone decides nothing. Back after leaving puts up the card, whose one action ends
/// the game; leaving again lets go again.
#[test]
fn focus_back_after_leaving_shows_the_card_with_end_game() {
    let (mut s, library, bus) = started_here();
    library.set_running(&desk("steam:570", "window"));
    s.sync();
    assert!(s.playing_here());
    s.focus(true);
    assert!(s.playing_here(), "a focus that never left changes nothing");
    s.handle_menu(MenuEvent::Confirm);
    bus.drain();
    s.focus(false);
    s.focus(true);
    assert!(
        !s.playing_here(),
        "the card is up and the pads are the console's"
    );
    s.handle_menu(MenuEvent::Back);
    assert!(
        s.connect.playing_here().is_some(),
        "Back is no action of the card"
    );
    s.handle_menu(MenuEvent::Confirm);
    let ends = |c: &ConsoleCmd| {
        matches!(c, ConsoleCmd::EndGame { app_id, title, .. }
            if app_id == "steam:570" && title == "Dota 2")
    };
    assert!(bus.drain().iter().any(ends));
    s.focus(false);
    assert!(s.playing_here(), "leaving again lets go again");
}

/// A title the host lists no more for a while is gone; one empty read is not, in the hold or
/// once it plays.
#[test]
fn a_title_the_host_lists_no_more_returns_to_the_shelf() {
    let (mut s, library, _bus) = started_here();
    library.set_running(&desk("steam:570", "launching"));
    s.sync();
    at(&mut s, 100.0 + 2.0 * LOCAL_GONE);
    library.set_running(&[]);
    s.sync();
    assert!(
        s.connect.launching().is_some(),
        "one empty read late in the hold"
    );
    library.set_running(&desk("steam:570", "running"));
    s.sync();
    library.set_running(&[]);
    s.sync();
    assert!(s.playing_here(), "one empty read is not gone");
    at(&mut s, 100.0 + 3.0 * LOCAL_GONE);
    s.sync();
    assert!(s.connect.takeover().is_none());
}

/// A title Steam starts has no row until Steam hands it over: before the host first lists it
/// the hold waits [`LAUNCH_NO_LEASE`]; once listed, [`LOCAL_GONE`] without it is gone.
#[test]
fn a_hold_waits_for_a_row_that_has_not_appeared_yet() {
    let (mut s, library, _bus) = started_here();
    library.set_running(&[]);
    s.sync();
    at(&mut s, 104.0);
    library.set_running(&[]);
    s.sync();
    assert!(
        s.connect.launching().is_some(),
        "nothing listed for 4 s is still a start"
    );
    library.set_running(&desk("steam:570", "launching"));
    s.sync();
    library.set_running(&[]);
    s.sync();
    at(&mut s, 104.0 + LOCAL_GONE);
    s.sync();
    assert!(s.connect.takeover().is_none(), "listed once, then gone");

    let (mut s, library, _bus) = started_here();
    library.set_running(&[]);
    s.sync();
    at(&mut s, 100.0 + LAUNCH_NO_LEASE - 1.0);
    s.sync();
    assert!(s.connect.launching().is_some());
    at(&mut s, 100.0 + LAUNCH_NO_LEASE);
    s.sync();
    assert!(s.connect.takeover().is_none(), "never listed");
}

/// A device streaming the same title is another row: it neither lets go for the hold on this
/// device nor ends the title playing here.
#[test]
fn a_stream_of_the_same_title_is_not_the_title_playing_here() {
    let (mut s, library, _bus) = started_here();
    let mut rows = desk("steam:570", "launching");
    rows.extend(running("steam:570", "running"));
    library.set_running(&rows);
    s.sync();
    assert!(s.connect.launching().is_some() && !s.playing_here());
    let mut rows = desk("steam:570", "window");
    rows.extend(running("steam:570", "exited"));
    library.set_running(&rows);
    s.sync();
    assert!(s.playing_here(), "the stream's exit is not this title's");
}

/// The host's refusal, an old host and a launch that died at the start each end the hold
/// with their sentence; OK then leaves, and nothing is ever dialed or cancelled.
#[test]
fn a_launch_on_this_device_that_did_not_happen_says_why() {
    let (mut s, _library, _bus) = on_shelf();
    s.start_connect(here("steam:570"));
    let no_desk = "This host has no screen of its own to play on.";
    s.console
        .set_local_launch(LocalLaunch::Refused(no_desk.into()));
    s.sync();
    assert_eq!(
        s.connect.launching().and_then(|l| l.failed.as_deref()),
        Some(no_desk)
    );
    s.handle_menu(MenuEvent::Confirm);
    assert!(s.connect.takeover().is_none() && s.take_action().is_none());

    s.start_connect(here("steam:570"));
    s.console.set_local_launch(LocalLaunch::HostTooOld);
    s.sync();
    assert_eq!(
        s.connect.launching().and_then(|l| l.failed.as_deref()),
        Some("Update Punktfunk on this device to play here.")
    );

    let (mut s, library, _bus) = started_here();
    let mut died = desk("steam:570", "exited");
    died[0].outcome = Some(pf_client_core::library::LaunchVerdict {
        code: "launch_download_failed".into(),
        ..Default::default()
    });
    library.set_running(&died);
    s.sync();
    assert_eq!(
        s.connect.launching().and_then(|l| l.failed.as_deref()),
        Some("Dota 2 didn't download.")
    );

    let (mut s, library, _bus) = started_here();
    library.set_running(&desk("steam:570", "exited"));
    s.sync();
    assert!(
        s.connect.takeover().is_none(),
        "an exit with no outcome is the shelf"
    );
}

/// Back while it starts leaves the hold, cancels nothing, and gives the shelf its input back;
/// a refusal that lands after it is a toast.
#[test]
fn back_leaves_a_hold_on_this_device_and_cancels_nothing() {
    let (mut s, _library, _bus) = on_shelf();
    s.start_connect(here("steam:570"));
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_none());
    s.handle_menu(MenuEvent::Back);
    assert!(s.takeover().is_none());
    assert!(
        s.take_action().is_none(),
        "no CancelConnect: nothing dialed"
    );
    assert!(
        s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_some(),
        "the shelf takes input"
    );
    s.console
        .set_local_launch(LocalLaunch::Refused("Nobody is signed in.".into()));
    s.sync();
    assert_eq!(
        s.toast.as_ref().map(|t| t.text.as_str()),
        Some("Nobody is signed in.")
    );
    assert!(matches!(s.connect, Connect::Idle), "and the watch ends");
}

/// A shell whose launch of Dota 2 on this device started, with its hold left by Back.
fn watching() -> (Shell, LibraryShared, ConsoleBus) {
    let (mut s, library, bus) = started_here();
    s.handle_menu(MenuEvent::Back);
    assert!(matches!(s.connect, Connect::Watching(_)));
    (s, library, bus)
}

/// The title coming up after Back lets go as it does from the hold; it keeps being asked after
/// meanwhile, and exits back to whatever the console shows.
#[test]
fn a_title_up_after_back_still_lets_go() {
    let (mut s, library, bus) = watching();
    bus.drain();
    library.set_running(&desk("steam:570", "launching"));
    at(&mut s, 100.0 + LAUNCH_POLL_STALL);
    s.sync();
    assert!(s.takeover().is_none() && !s.playing_here());
    assert!(
        bus.drain()
            .iter()
            .any(|c| matches!(c, ConsoleCmd::RefreshRunning { mgmt: 47990, .. })),
        "the watch polls as the hold does"
    );
    library.set_running(&desk("steam:570", "running"));
    s.sync();
    assert!(s.playing_here(), "the console lets go");
    assert!(s.take_action().is_none() && !s.in_stream, "no frame asked");
    library.set_running(&desk("steam:570", "exited"));
    s.sync();
    assert!(s.takeover().is_none() && !s.playing_here());
}

/// After Back an outcome is a toast, and a title that never shows or is gone ends the watch
/// with nothing said.
#[test]
fn a_watch_ends_with_a_toast_or_silently() {
    let (mut s, library, _bus) = watching();
    let mut died = desk("steam:570", "exited");
    died[0].outcome = Some(pf_client_core::library::LaunchVerdict {
        code: "launch_download_failed".into(),
        ..Default::default()
    });
    library.set_running(&died);
    s.sync();
    assert_eq!(
        s.toast.as_ref().map(|t| t.text.as_str()),
        Some("Dota 2 didn't download.")
    );
    assert!(matches!(s.connect, Connect::Idle));

    let (mut s, library, _bus) = watching();
    library.set_running(&[]);
    s.sync();
    at(&mut s, 100.0 + LAUNCH_NO_LEASE);
    s.sync();
    assert!(matches!(s.connect, Connect::Idle), "never listed");
    assert!(s.toast.is_none());
}

/// Starting anything else replaces the watch: a stream, a title on this device, or one the
/// shelf does not hold for.
#[test]
fn another_launch_replaces_the_watch() {
    let (mut s, library, _bus) = watching();
    s.start_connect(intent("steam:570"));
    assert!(s.holds_stream());
    library.set_running(&desk("steam:570", "running"));
    s.sync();
    assert!(s.holds_stream() && !s.playing_here(), "the stream's hold");

    let (mut s, _library, _bus) = watching();
    s.start_connect(here("steam:570"));
    assert_eq!(s.takeover(), Some(Takeover::Launching), "a new hold");

    let (mut s, library, _bus) = watching();
    s.start_connect(here("steam:ui"));
    assert!(matches!(s.connect, Connect::Idle), "a launcher has no hold");
    library.set_running(&desk("steam:570", "running"));
    s.sync();
    assert!(!s.playing_here());
}

/// A connect to this device's own host with no title has nothing to start and dials nothing.
#[test]
fn a_connect_with_no_title_on_this_device_starts_nothing() {
    let (mut s, _library, bus) = on_shelf();
    let mut desk = here("steam:570");
    desk.launch = None;
    s.start_connect(desk);
    assert!(s.take_action().is_none() && s.connect.takeover().is_none());
    let asks = |c: &ConsoleCmd| matches!(c, ConsoleCmd::LaunchLocal { .. });
    assert!(!bus.drain().iter().any(asks));
    assert_eq!(
        s.toast.as_ref().map(|t| t.text.as_str()),
        Some("This is this device's own host — there is nothing to stream.")
    );
}
