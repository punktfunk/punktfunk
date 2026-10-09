//! The profile check before a dial (`profiles-and-seats.md` §10.1), through the shell.

use super::*;
use crate::model::ProfilesAnswer;
use crate::shell::connect::{ASKING_CARD_AFTER, PROFILES_WAIT, SEAT_POLL};
use pf_client_core::profiles::{ListedProfile, ProfilePick, Seat, SeatState};

fn listed(ids: &[(&str, &str)]) -> ProfilesAnswer {
    ProfilesAnswer::Listed(
        ids.iter()
            .map(|(id, name)| ListedProfile {
                id: (*id).into(),
                display_name: (*name).into(),
                ..Default::default()
            })
            .collect(),
    )
}

fn pick(id: &str, name: &str) -> Option<ProfilePick> {
    Some(ProfilePick {
        id: id.into(),
        display_name: name.into(),
    })
}

/// A shell that checks profiles, over `hosts()` with the first card's pick set to `saved`.
fn asking(saved: Option<ProfilePick>) -> (Shell, ConsoleShared, ConsoleBus, HostRow) {
    fake_home();
    let console = ConsoleShared::default();
    let mut rows = hosts();
    rows[0].profile = saved;
    console.set_hosts(rows.clone());
    let bus = ConsoleBus::default();
    let mut opts = test_options();
    opts.profiles = true;
    let mut s = Shell::new(
        console.clone(),
        LibraryShared::default(),
        bus.clone(),
        opts,
        vec![Screen::Home(HomeScreen::new())],
    )
    .unwrap();
    s.fake_clock = Some((10.0, 0.0));
    s.sync();
    (s, console, bus, rows[0].clone())
}

fn launched_as(s: &mut Shell) -> Option<Option<String>> {
    match s.take_action() {
        Some(OverlayAction::Launch { profile, .. }) => Some(profile),
        _ => None,
    }
}

/// The connect waits for the list, asks once, and holds input meanwhile.
#[test]
fn a_connect_asks_for_the_list_before_it_dials() {
    let (mut s, _console, bus, row) = asking(None);
    s.start_connect(ConnectIntent::to_host(&row, None));
    assert!(s.take_action().is_none(), "nothing dials before the answer");
    assert!(bus.drain().iter().any(|c| matches!(
        c,
        ConsoleCmd::FetchProfiles { fp_hex, .. } if fp_hex == "aa11"
    )));
    assert!(s.handle_menu(MenuEvent::Move(MenuDir::Right)).is_none());
    assert!(!s.at_root());
    // Back drops it; nothing was dialed, so nothing is cancelled.
    s.handle_menu(MenuEvent::Back);
    assert!(s.asking.is_none() && s.take_action().is_none());
}

/// Two profiles and no saved pick: the picker, whose pick dials as it and is saved.
#[test]
fn two_profiles_and_no_pick_raise_the_picker() {
    let (mut s, console, bus, row) = asking(None);
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", listed(&[("own", "Ben"), ("kid", "Kid")]));
    s.sync();
    assert!(matches!(s.stack.last(), Some(Screen::Profiles(_))));
    assert!(s.take_action().is_none());
    finish_motion(&mut s);
    // Focus moves over the drawn grid.
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    bus.drain();
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Confirm);
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
    assert!(bus.drain().contains(&ConsoleCmd::SetProfile {
        key: "aa11".into(),
        profile: pick("kid", "Kid"),
    }));
}

/// A pick the box drops between its list and the dial: the card forgets it and asks the
/// list once more. A second miss fails as any refusal does.
#[test]
fn a_profile_gone_at_the_dial_forgets_the_pick_and_asks_once_more() {
    let (mut s, console, bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", listed(&[("own", "Ben"), ("kid", "Kid")]));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
    bus.drain();
    s.profile_gone();
    s.session_failed("That profile is gone from this host. Pick another one.");
    let sent = bus.drain();
    assert!(sent.contains(&ConsoleCmd::SetProfile {
        key: "aa11".into(),
        profile: None,
    }));
    assert!(sent
        .iter()
        .any(|c| matches!(c, ConsoleCmd::FetchProfiles { .. })));
    console.set_profiles("aa11", listed(&[("own", "Ben")]));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("own".into())));
    bus.drain();
    s.profile_gone();
    s.session_failed("gone again");
    assert!(
        !bus.drain()
            .iter()
            .any(|c| matches!(c, ConsoleCmd::FetchProfiles { .. })),
        "the second miss does not ask again"
    );
}

/// A saved pick still listed dials as it; a single profile dials as that one.
#[test]
fn a_listed_pick_or_a_lone_profile_dials_at_once() {
    let (mut s, console, _bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", listed(&[("own", "Ben"), ("kid", "Kid")]));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));

    let (mut s, console, _bus, row) = asking(None);
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", listed(&[("own", "Ben")]));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("own".into())));
}

/// A gone pick raises the picker with its line; a box without profiles sends none.
#[test]
fn a_gone_pick_asks_again_and_no_profiles_sends_none() {
    let (mut s, console, _bus, row) = asking(pick("theo", "Theo"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", listed(&[("own", "Ben"), ("kid", "Kid")]));
    s.sync();
    assert!(matches!(s.stack.last(), Some(Screen::Profiles(_))));

    let (mut s, console, _bus, row) = asking(pick("theo", "Theo"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", ProfilesAnswer::NoProfiles);
    s.sync();
    assert_eq!(launched_as(&mut s), Some(None));
}

/// The legacy seat is preselected and saved without a picker.
#[test]
fn a_legacy_seat_is_picked_and_saved() {
    let (mut s, console, bus, row) = asking(None);
    s.start_connect(ConnectIntent::to_host(&row, None));
    let ProfilesAnswer::Listed(mut l) = listed(&[("own", "Ben"), ("kid", "Kid")]) else {
        unreachable!()
    };
    l[1].legacy_seat = true;
    console.set_profiles("aa11", ProfilesAnswer::Listed(l));
    bus.drain();
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
    assert!(bus.drain().contains(&ConsoleCmd::SetProfile {
        key: "aa11".into(),
        profile: pick("kid", "Kid"),
    }));
}

/// No answer in time, or a failed one, dials with the card's pick: never a dead end.
#[test]
fn a_silent_or_failed_host_dials_with_the_cards_pick() {
    let (mut s, _console, _bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    s.fake_clock = Some((10.0 + ASKING_CARD_AFTER, 0.0));
    s.sync();
    assert!(s.connecting.is_some(), "a slow list puts the card up");
    s.fake_clock = Some((10.0 + PROFILES_WAIT, 0.0));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));

    let (mut s, console, _bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles(
        "aa11",
        ProfilesAnswer::Failed("couldn't reach the host".into()),
    );
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
}

/// A host that can't answer never gets asked: the connect dials as before.
#[test]
fn a_host_without_profile_support_dials_at_once() {
    let (mut s, _console, bus, row) = asking(pick("kid", "Kid"));
    s.device.profiles = false;
    s.start_connect(ConnectIntent::to_host(&row, None));
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
    assert!(!bus
        .drain()
        .iter()
        .any(|c| matches!(c, ConsoleCmd::FetchProfiles { .. })));
}

fn seated(state: SeatState, detail: Option<&str>) -> ProfilesAnswer {
    let ProfilesAnswer::Listed(mut l) = listed(&[("own", "Ben"), ("kid", "Kid")]) else {
        unreachable!()
    };
    l[1].seat = Some(Seat {
        state,
        detail: detail.map(Into::into),
        ..Default::default()
    });
    ProfilesAnswer::Listed(l)
}

fn woke(row: &HostRow) -> ConsoleCmd {
    ConsoleCmd::WakeProfile {
        addr: row.addr.clone(),
        mgmt: row.mgmt_port,
        fp_hex: "aa11".into(),
        id: "kid".into(),
    }
}

fn polled(bus: &ConsoleBus) -> bool {
    bus.drain()
        .iter()
        .any(|c| matches!(c, ConsoleCmd::FetchProfiles { fp_hex, .. } if fp_hex == "aa11"))
}

/// A stopped seat is woken, then the list is read every 2 s until it is ready.
#[test]
fn a_stopped_seat_is_woken_and_waited_for_before_the_dial() {
    let (mut s, console, bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", seated(SeatState::Stopped, None));
    bus.drain();
    s.sync();
    assert!(s.take_action().is_none(), "a stopped seat is not dialed");
    assert!(bus.drain().contains(&woke(&row)));
    assert!(!s.at_root(), "the wait holds input");

    s.fake_clock = Some((10.0 + SEAT_POLL, 0.0));
    s.sync();
    assert!(polled(&bus));
    console.set_profiles("aa11", seated(SeatState::Starting, Some("Signing in")));
    s.sync();
    assert!(s.take_action().is_none());
    let wait = s.seat_wait.as_ref().expect("still waiting");
    assert_eq!(wait.detail.as_deref(), Some("Signing in"));

    s.fake_clock = Some((10.0 + 2.0 * SEAT_POLL, 0.0));
    s.sync();
    assert!(polled(&bus));
    console.set_profiles("aa11", seated(SeatState::Ready, None));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
    assert!(s.seat_wait.is_none());
}

/// A starting seat is not woken again; an occupied one is dialed as it is.
#[test]
fn a_starting_seat_waits_and_an_occupied_one_dials() {
    let (mut s, console, bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", seated(SeatState::Starting, Some("Setting up")));
    bus.drain();
    s.sync();
    assert!(s.take_action().is_none());
    assert!(!bus.drain().contains(&woke(&row)));
    assert_eq!(
        s.seat_wait.as_ref().and_then(|w| w.detail.as_deref()),
        Some("Setting up")
    );
    s.fake_clock = Some((10.0 + SEAT_POLL, 0.0));
    console.set_profiles("aa11", seated(SeatState::Occupied, None));
    s.sync();
    assert_eq!(launched_as(&mut s), Some(Some("kid".into())));
}

/// Cancel, by Back or by a click on the hint, drops the wait: nothing dials, nothing polls.
#[test]
fn cancel_leaves_the_wait_without_a_dial() {
    for by_click in [false, true] {
        let (mut s, console, bus, row) = asking(pick("kid", "Kid"));
        s.start_connect(ConnectIntent::to_host(&row, None));
        console.set_profiles("aa11", seated(SeatState::Stopped, None));
        s.sync();
        assert!(s.seat_wait.is_some());
        if by_click {
            let fonts = crate::theme::build_fonts().unwrap();
            let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
            s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
            let (_, rect) = *s
                .hint_rects
                .iter()
                .find(|(key, _)| *key == crate::glyphs::HintKey::Back)
                .expect("the wait draws a Cancel");
            assert!(s.pointer(crate::pointer::Pointer {
                x: f64::from(rect.center_x()),
                y: f64::from(rect.center_y()),
                kind: crate::pointer::PointerKind::Press,
            }));
        } else {
            s.handle_menu(MenuEvent::Back);
        }
        assert!(s.seat_wait.is_none(), "by_click = {by_click}");
        bus.drain();
        s.fake_clock = Some((10.0 + 2.0 * SEAT_POLL, 0.0));
        console.set_profiles("aa11", seated(SeatState::Ready, None));
        s.sync();
        assert!(s.take_action().is_none() && !polled(&bus));
    }
}

/// A seat that can't play says why and does not dial, at the gate or mid-wait.
#[test]
fn an_unavailable_seat_shows_its_line_and_stops() {
    let (mut s, console, _bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles(
        "aa11",
        seated(SeatState::Unavailable, Some("Seats are off.")),
    );
    s.sync();
    assert!(s.take_action().is_none() && s.seat_wait.is_none());
    assert_eq!(
        s.toast.as_ref().map(|t| t.text.as_str()),
        Some("Seats are off.")
    );

    let (mut s, console, _bus, row) = asking(pick("kid", "Kid"));
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", seated(SeatState::Starting, None));
    s.sync();
    s.fake_clock = Some((10.0 + SEAT_POLL, 0.0));
    console.set_profiles(
        "aa11",
        seated(SeatState::Unavailable, Some("Sign-in failed.")),
    );
    s.sync();
    assert!(s.take_action().is_none() && s.seat_wait.is_none());
    assert_eq!(
        s.toast.as_ref().map(|t| t.text.as_str()),
        Some("Sign-in failed.")
    );
}

/// A pick made on the picker goes through the same gate.
#[test]
fn a_picked_stopped_seat_is_woken() {
    let (mut s, console, bus, row) = asking(None);
    s.start_connect(ConnectIntent::to_host(&row, None));
    console.set_profiles("aa11", seated(SeatState::Stopped, None));
    s.sync();
    assert!(matches!(s.stack.last(), Some(Screen::Profiles(_))));
    finish_motion(&mut s);
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    bus.drain();
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Confirm);
    assert!(s.take_action().is_none(), "the seat is not up yet");
    assert!(bus.drain().contains(&woke(&row)));
    assert!(s.seat_wait.is_some());
}

/// Switch profile's list arrives through the shell, keyed on the host.
#[test]
fn the_switch_screen_takes_its_hosts_answer() {
    let (mut s, console, _bus, row) = asking(pick("kid", "Kid"));
    let screen = crate::screens::profiles::ProfilesScreen::switch(&row).unwrap();
    s.push_screen(Screen::Profiles(screen));
    console.set_profiles("bb22", listed(&[("x", "Other")]));
    s.sync();
    assert!(matches!(s.stack.last(), Some(Screen::Profiles(p)) if p.waiting()));
    console.set_profiles("aa11", listed(&[("own", "Ben"), ("kid", "Kid")]));
    s.sync();
    assert!(matches!(s.stack.last(), Some(Screen::Profiles(p)) if !p.waiting()));
}
