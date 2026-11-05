//! A locked profile's password through the shell: the prompt before the dial, the unlock the
//! host takes with the `Launch`, the prompt a `ProfileLocked` close reopens, and `Pending` as
//! the desktop starting.

use super::*;
use crate::model::ProfilesAnswer;
use pf_client_core::console::SessionPhase;
use pf_client_core::profiles::{ListedProfile, ProfilePick, Seat, SeatState, UnlockNeed};
use std::sync::atomic::Ordering;

/// Ben, and Kid on a person's own account with its seat stopped.
fn locked_list() -> ProfilesAnswer {
    let row = |id: &str, name: &str| ListedProfile {
        id: id.into(),
        display_name: name.into(),
        ..Default::default()
    };
    ProfilesAnswer::Listed(vec![
        row("own", "Ben"),
        ListedProfile {
            unlock: UnlockNeed::Password,
            seat: Some(Seat {
                state: SeatState::Stopped,
                ..Default::default()
            }),
            ..row("kid", "Kid")
        },
    ])
}

/// A shell that checks profiles, its first card remembering `pick`; `prompts` is the host's
/// [`ConsoleOptions::prompts_unlock`].
fn shell_for(pick: Option<&str>, prompts: bool) -> (Shell, ConsoleShared, ConsoleBus, HostRow) {
    fake_home();
    let console = ConsoleShared::default();
    let mut rows = hosts();
    rows[0].profile = pick.map(|id| ProfilePick {
        id: id.into(),
        display_name: id.into(),
        remember: true,
        ..ProfilePick::default()
    });
    console.set_hosts(rows.clone());
    let bus = ConsoleBus::default();
    let mut opts = test_options();
    opts.profiles = true;
    opts.prompts_unlock = prompts;
    let stack = vec![Screen::Home(HomeScreen::new())];
    let mut s = Shell::new(
        console.clone(),
        LibraryShared::default(),
        bus.clone(),
        opts,
        stack,
    )
    .unwrap();
    s.fake_clock = Some((10.0, 0.0));
    s.sync();
    (s, console, bus, rows[0].clone())
}

/// Connect to the card, its list answering with Kid locked.
fn connect_locked(s: &mut Shell, console: &ConsoleShared, row: &HostRow) {
    s.start_connect(ConnectIntent::to_host(row, None));
    console.set_profiles("aa11", locked_list());
    s.sync();
}

fn prompt(s: &Shell) -> Option<&crate::screens::unlock::UnlockScreen> {
    match s.stack.last() {
        Some(Screen::Unlock(u)) => Some(u),
        _ => None,
    }
}

fn launched(s: &mut Shell) -> Option<Option<String>> {
    match s.take_action() {
        Some(OverlayAction::Launch { profile, .. }) => Some(profile),
        _ => None,
    }
}

/// Type `pw` into the prompt's field and press Done.
fn sign_in(s: &mut Shell, pw: &str) {
    finish_motion(s);
    s.handle_menu(MenuEvent::Confirm);
    s.text_input(pw);
    s.handle_menu(MenuEvent::Secondary);
}

/// A locked pick prompts before anything dials, and its stopped seat is never woken. Sign In
/// dials once; the unlock waits in the model for the host that takes the `Launch`.
#[test]
fn a_locked_profile_prompts_then_dials_with_the_unlock() {
    let (mut s, console, bus, row) = shell_for(Some("kid"), true);
    connect_locked(&mut s, &console, &row);
    assert!(prompt(&s).is_some(), "the prompt is up");
    assert!(s.take_action().is_none(), "nothing dials first");
    assert!(
        !(bus.drain().iter()).any(|c| matches!(c, ConsoleCmd::WakeProfile { .. })),
        "a locked seat is not woken"
    );
    sign_in(&mut s, "hunter2!");
    assert_eq!(launched(&mut s), Some(Some("kid".into())));
    let u = console
        .take_unlock()
        .expect("the unlock waits for the host");
    assert_eq!(u.secret.as_slice(), b"hunter2!");
    assert!(!u.stay_signed_in);
    assert!(console.take_unlock().is_none(), "taken once");
}

/// Back on the prompt dials nothing and leaves no unlock behind.
#[test]
fn a_cancelled_prompt_dials_nothing() {
    let (mut s, console, _bus, row) = shell_for(Some("kid"), true);
    connect_locked(&mut s, &console, &row);
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Confirm);
    s.text_input("typed");
    s.handle_menu(MenuEvent::Back);
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert!(prompt(&s).is_none());
    assert!(s.take_action().is_none() && console.take_unlock().is_none());
}

/// A `ProfileLocked` close reopens the prompt with the host's sentence; one that said nothing
/// opens it bare. Nothing dials until the player signs in again.
#[test]
fn a_refusal_reopens_the_prompt_with_the_hosts_sentence() {
    for (said, line) in [
        ("Wrong password.", Some("Wrong password.")),
        ("This profile is locked. Enter its password.", None),
    ] {
        let (mut s, console, _bus, row) = shell_for(Some("kid"), true);
        connect_locked(&mut s, &console, &row);
        sign_in(&mut s, "wrong");
        assert!(launched(&mut s).is_some());
        finish_motion(&mut s);
        console.locked_flag().store(true, Ordering::Relaxed);
        s.session_phase(SessionPhase::Failed(said));
        let p = prompt(&s).expect("the prompt is back");
        assert_eq!(p.error(), line);
        assert_eq!(p.title(), "Sign in as Kid");
        assert!(s.toast.is_none(), "no toast over it");
        assert!(s.take_action().is_none(), "never a dial on its own");
        sign_in(&mut s, "right");
        assert_eq!(launched(&mut s), Some(Some("kid".into())));
    }
}

/// Any other failure is the toast it always was, and no flag is left raised for the next one.
#[test]
fn another_failure_is_a_toast() {
    let (mut s, console, _bus, row) = shell_for(Some("kid"), true);
    connect_locked(&mut s, &console, &row);
    sign_in(&mut s, "pw");
    launched(&mut s);
    finish_motion(&mut s);
    s.session_phase(SessionPhase::Failed("The host didn't answer"));
    assert!(prompt(&s).is_none());
    assert!(s.toast.is_some());
}

/// `Pending` on a dial that carries an unlock is the desktop starting; on a bare dial it is
/// the approval wait.
#[test]
fn pending_on_an_unlock_dial_is_the_desktop_starting() {
    let (mut s, console, _bus, row) = shell_for(Some("kid"), true);
    connect_locked(&mut s, &console, &row);
    sign_in(&mut s, "pw");
    launched(&mut s);
    s.session_phase(SessionPhase::AwaitingApproval);
    let card = s.connect.card().expect("the connect card");
    assert_eq!(
        card.starting.as_deref(),
        Some("Starting your desktop on Living Room PC\u{2026}")
    );
    assert!(!card.request_access);

    let (mut s, _console, _bus, row) = shell_for(None, true);
    s.start_connect(ConnectIntent::to_host(&row, None));
    s.fake_clock = Some((10.0 + 60.0, 0.0));
    s.sync();
    launched(&mut s);
    s.session_phase(SessionPhase::AwaitingApproval);
    let card = s.connect.card().expect("the connect card");
    assert!(card.request_access && card.starting.is_none());
}

/// A host that can't carry an unlock gets today's dial: no prompt, the host answers.
#[test]
fn a_host_without_the_prompt_dials_as_it_stands() {
    let (mut s, console, _bus, row) = shell_for(Some("kid"), false);
    connect_locked(&mut s, &console, &row);
    assert!(prompt(&s).is_none());
    assert_eq!(launched(&mut s), Some(Some("kid".into())));
    console.locked_flag().store(true, Ordering::Relaxed);
    finish_motion(&mut s);
    s.session_phase(SessionPhase::Failed("Wrong password."));
    assert!(prompt(&s).is_none() && s.toast.is_some());
}

/// Picked from the circles, a locked profile's prompt takes the picker's place.
#[test]
fn the_picker_hands_a_locked_pick_to_the_prompt() {
    let (mut s, console, _bus, row) = shell_for(None, true);
    connect_locked(&mut s, &console, &row);
    assert!(matches!(s.stack.last(), Some(Screen::Profiles(_))));
    finish_motion(&mut s);
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    s.render(surface.canvas(), 1280, 800, &fonts, None, None, &[]);
    s.handle_menu(MenuEvent::Move(MenuDir::Right));
    s.handle_menu(MenuEvent::Confirm);
    assert!(prompt(&s).is_some());
    assert_eq!(s.stack.len(), 2, "home, then the prompt");
    assert!(s.take_action().is_none());
    // Back returns to the cards, not to the picker.
    finish_motion(&mut s);
    s.handle_menu(MenuEvent::Back);
    finish_motion(&mut s);
    assert_eq!(s.stack.len(), 1);
}
