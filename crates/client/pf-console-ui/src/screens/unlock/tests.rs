use super::*;
use crate::model::HostRow;
use pf_client_core::trust::Settings;
use punktfunk_core::quic::v2::msg::UNLOCK_PASSWORD;

const TYPED: &str = " Tr0ub4dor&3 ";

fn prompt() -> UnlockScreen {
    let intent = ConnectIntent {
        profile: Some("kid".into()),
        ..ConnectIntent::to_host(&HostRow::fixture("aa11", "Den"), None)
    };
    UnlockScreen::new(intent, "Kid".into(), "Den".into(), None)
}

/// Runs `f` with a desktop context over an empty store.
fn with_ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> R {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx::test(&mut settings, &library);
    f(&mut ctx)
}

fn press(s: &mut UnlockScreen, ev: MenuEvent) -> Outbox {
    with_ctx(|ctx| {
        let mut fx = Outbox::default();
        s.menu(ev, ctx, &mut fx);
        fx
    })
}

/// Everything the prompt hands anyone else to show or speak.
fn shown(s: &UnlockScreen) -> Vec<String> {
    let mut out: Vec<String> = s.rows().into_iter().flat_map(|r| r.value).collect();
    out.push(s.title());
    out.extend(with_ctx(|ctx| s.announcement(ctx)));
    out.extend(s.edit_field().map(|f| format!("{f:?}")));
    out.extend(s.edit_field().map(|f| serde_json::to_string(&f).unwrap()));
    out
}

/// The buffer holds what was typed, as typed; the row shows one dot per character, and no
/// row, title, announcement or host field ever carries the text.
#[test]
fn the_field_holds_the_password_and_shows_only_dots() {
    let mut s = prompt();
    press(&mut s, MenuEvent::Confirm);
    assert!(s.editing, "OK on the field opens it");
    s.text_input(TYPED);
    assert_eq!(s.password.as_str(), TYPED, "never trimmed");
    let dots = "\u{2022}".repeat(TYPED.chars().count());
    assert_eq!(s.rows()[PASSWORD].value.as_deref(), Some(dots.as_str()));
    let field = s.edit_field().unwrap();
    assert!(field.secure && field.text.is_empty() && !field.digits);
    for line in shown(&s) {
        assert!(!line.contains("Tr0ub"), "{line:?}");
    }
    // Drawn on the CPU too: the pixels come from the same rows.
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let rect = Rect::from_wh(1280.0, 800.0);
    with_ctx(|ctx| s.render(surface.canvas(), rect, 1.0, 1.0 / 60.0, &fonts, ctx));
}

/// Back closes the field, then leaves without a dial; nothing typed goes anywhere.
#[test]
fn cancel_dials_nothing() {
    let mut s = prompt();
    press(&mut s, MenuEvent::Confirm);
    s.text_input("secret");
    let fx = press(&mut s, MenuEvent::Back);
    assert!(
        !s.editing && fx.nav.is_none(),
        "Back puts the field away first"
    );
    let fx = press(&mut s, MenuEvent::Back);
    assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    assert!(fx.connect.is_none() && fx.unlock.is_none());
}

/// Sign In hands the shell the dial and a kind-1 unlock of the password as typed; the field
/// is empty after, and the switch decides "stay signed in".
#[test]
fn sign_in_dials_with_the_password_as_typed() {
    for stay in [false, true] {
        let mut s = prompt();
        press(&mut s, MenuEvent::Confirm);
        s.text_input(TYPED);
        press(&mut s, MenuEvent::Back);
        press(
            &mut s,
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Down),
        );
        if stay {
            press(&mut s, MenuEvent::Confirm);
            assert!(s.stay);
        }
        press(
            &mut s,
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Down),
        );
        let fx = press(&mut s, MenuEvent::Confirm);
        let u = fx.unlock.expect("an unlock");
        assert_eq!(u.kind, UNLOCK_PASSWORD);
        assert_eq!(u.secret.as_slice(), TYPED.as_bytes());
        assert_eq!(u.stay_signed_in, stay);
        let intent = fx.connect.expect("a dial");
        assert_eq!(intent.profile.as_deref(), Some("kid"));
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
        assert!(s.password.is_empty());
        assert!(!format!("{u:?}").contains("Tr0ub"), "Debug prints lengths");
    }
}

/// The keyboard's Done (Y) signs in from the open field.
#[test]
fn done_signs_in_from_the_field() {
    let mut s = prompt();
    press(&mut s, MenuEvent::Confirm);
    s.text_input("pw");
    let fx = press(&mut s, MenuEvent::Secondary);
    assert_eq!(fx.unlock.map(|u| u.secret.to_vec()), Some(b"pw".to_vec()));
}

/// Sign In with nothing typed opens the field instead of dialing; Done on an empty field
/// shows the shared line and stays.
#[test]
fn an_empty_password_never_dials() {
    let mut s = prompt();
    s.list.cursor = SIGN_IN;
    let fx = press(&mut s, MenuEvent::Confirm);
    assert!(s.editing && fx.connect.is_none());
    assert_eq!(s.list.cursor, PASSWORD);
    let fx = press(&mut s, MenuEvent::Secondary);
    assert!(fx.connect.is_none() && fx.unlock.is_none());
    assert_eq!(s.error.as_deref(), Some("Enter the password."));
}

/// The field stops at the wire's 512 bytes, so the buffer never grows away a copy.
#[test]
fn the_buffer_stops_at_the_wire_bound_without_growing() {
    let mut s = prompt();
    s.editing = true;
    let room = s.password.capacity();
    assert!(room >= UNLOCK_PASSWORD_MAX);
    s.text_input(&"\u{e9}".repeat(300));
    assert_eq!(s.password.len(), UNLOCK_PASSWORD_MAX);
    s.text_input("x");
    assert_eq!(s.password.len(), UNLOCK_PASSWORD_MAX);
    assert_eq!(s.password.capacity(), room);
}

/// A refusal's sentence stands under the field until the next try.
#[test]
fn the_hosts_sentence_is_the_error_line() {
    let intent = prompt().intent;
    let s = UnlockScreen::new(
        intent,
        "Kid".into(),
        "Den".into(),
        Some("Wrong password.".into()),
    );
    assert_eq!(s.error.as_deref(), Some("Wrong password."));
    assert_eq!(s.title(), "Sign in as Kid");
}

/// The pad walks the three rows and stops at both ends.
#[test]
fn focus_walks_the_rows() {
    use pf_client_core::menu_nav::MenuDir::{Down, Up};
    let mut s = prompt();
    let at = |s: &mut UnlockScreen, dir| {
        press(s, MenuEvent::Move(dir));
        s.list.cursor
    };
    assert_eq!(at(&mut s, Up), PASSWORD);
    assert_eq!(at(&mut s, Down), STAY);
    assert_eq!(at(&mut s, Down), SIGN_IN);
    assert_eq!(at(&mut s, Down), SIGN_IN);
    assert_eq!(
        with_ctx(|ctx| s.announcement(ctx)).as_deref(),
        Some("Sign In")
    );
}
