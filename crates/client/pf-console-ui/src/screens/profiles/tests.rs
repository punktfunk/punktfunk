use super::*;
use crate::screens::Nav;
use pf_client_core::profiles::{Seat, SeatState};
use pf_client_core::trust::Settings;

fn profile(id: &str, name: &str) -> ListedProfile {
    ListedProfile {
        id: id.into(),
        display_name: name.into(),
        ..Default::default()
    }
}

fn pick(id: &str, name: &str, remember: bool) -> ProfilePick {
    ProfilePick {
        id: id.into(),
        display_name: name.into(),
        remember,
        ..ProfilePick::default()
    }
}

fn host(saved: Option<ProfilePick>) -> HostRow {
    HostRow {
        profile: saved,
        ..HostRow::fixture("aa", "Desk")
    }
}

fn listed() -> Vec<ListedProfile> {
    vec![profile("own", "Ben"), profile("kid", "Kid")]
}

fn with_ctx<R>(hosts: &[HostRow], book: &ProfileBook, f: impl FnOnce(&mut Ctx) -> R) -> R {
    let mut settings = Settings::default();
    let library = crate::library::LibraryShared::default();
    let mut ctx = Ctx {
        hosts,
        profiles: book,
        ..Ctx::test(&mut settings, &library)
    };
    f(&mut ctx)
}

fn press(s: &mut ProfilesScreen, ev: MenuEvent, hosts: &[HostRow]) -> Outbox {
    let mut fx = Outbox::default();
    with_ctx(hosts, &NO_PROFILES, |ctx| s.menu(ev, ctx, &mut fx));
    fx
}

/// The remembered pick leads and wears the ring; OK remembers the focused one and says so.
#[test]
fn switch_leads_with_the_remembered_pick_and_remembers_the_choice() {
    let s = ProfilesScreen::switch(&host(Some(pick("kid", "Kid", true))), &NO_PROFILES);
    let mut s = s.expect("a paired host");
    assert!(s.waiting());
    s.set_answer(ProfilesAnswer::Listed(listed()));
    assert_eq!(s.listed()[0].id, "kid");
    let say = with_ctx(&[], &NO_PROFILES, |ctx| s.announcement(ctx));
    assert_eq!(say.as_deref(), Some("Kid, selected"));
    s.focus = Focus::Face(1);
    let fx = press(&mut s, MenuEvent::Confirm, &[]);
    assert_eq!(
        fx.cmds,
        vec![ConsoleCmd::SetProfile {
            key: "aa".into(),
            profile: Some(pick("own", "Ben", true)),
        }]
    );
    assert!(fx.connect.is_none());
    assert_eq!(fx.toast.as_deref(), Some("Playing as Ben on Desk"));
    assert!(matches!(fx.nav, Some(Nav::Pop)));
}

/// Ask every time follows the circles, is current while nothing is remembered, and forgets
/// a saved pick.
#[test]
fn ask_every_time_forgets_the_pick() {
    let book = ProfileBook::with("aa", listed());
    let mut s = ProfilesScreen::switch(&host(Some(pick("kid", "Kid", false))), &book).unwrap();
    assert!(!s.waiting(), "the book's list shows at once");
    assert_eq!(
        s.listed()[0].id,
        "own",
        "an unremembered pick does not lead"
    );
    s.focus = Focus::Face(2);
    let say = with_ctx(&[], &book, |ctx| s.announcement(ctx));
    assert_eq!(say.as_deref(), Some("Ask every time, selected"));
    let fx = press(&mut s, MenuEvent::Confirm, &[]);
    assert_eq!(
        fx.cmds,
        vec![ConsoleCmd::SetProfile {
            key: "aa".into(),
            profile: None,
        }]
    );
    assert_eq!(
        fx.toast.as_deref(),
        Some("Desk asks who\u{2019}s playing each time")
    );
}

/// Before a connect, OK plays as the pick. Remember is off until turned on; off, nothing is
/// saved and an old pick is cleared.
#[test]
fn a_pick_continues_the_waiting_connect_remembered_only_when_asked() {
    for remember in [false, true] {
        let h = host(Some(pick("gone", "Kid", true)));
        let intent = ConnectIntent::to_host(&h, None);
        let ask = intent.ask.clone().expect("a paired card asks");
        let mut s = ProfilesScreen::before(intent, ask, listed(), Some("Kid".into()), None);
        assert_eq!(s.line().as_deref(), Some("Kid is gone from this host."));
        if remember {
            s.focus = Focus::Remember;
            let fx = press(&mut s, MenuEvent::Confirm, &[]);
            assert!(
                fx.nav.is_none() && fx.connect.is_none(),
                "the switch only flips"
            );
            s.focus = Focus::Face(0);
        }
        let fx = press(&mut s, MenuEvent::Confirm, &[]);
        let go = fx.connect.expect("the connect goes on");
        assert_eq!(go.profile.as_deref(), Some("own"));
        assert!(go.ask.is_none());
        assert!(fx.toast.is_none());
        let saved = remember.then(|| pick("own", "Ben", true));
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetProfile {
                key: "aa".into(),
                profile: saved,
            }],
            "remember = {remember}"
        );
    }
}

/// Nothing saved and Remember off: the pick plays and nothing is written.
#[test]
fn an_unremembered_pick_writes_nothing() {
    let h = host(None);
    let intent = ConnectIntent::to_host(&h, None);
    let ask = intent.ask.clone().unwrap();
    let mut s = ProfilesScreen::before(intent, ask, listed(), None, Some("kid".into()));
    assert_eq!(s.listed()[0].id, "kid", "the lead goes first");
    let say = with_ctx(&[], &NO_PROFILES, |ctx| s.announcement(ctx));
    assert_eq!(say.as_deref(), Some("Kid"), "no ring before a connect");
    let fx = press(&mut s, MenuEvent::Confirm, &[]);
    assert!(fx.cmds.is_empty());
    assert_eq!(fx.connect.and_then(|c| c.profile).as_deref(), Some("kid"));
}

/// Back keeps the saved pick and drops a waiting connect.
#[test]
fn back_saves_nothing() {
    let h = host(None);
    let intent = ConnectIntent::to_host(&h, None);
    let ask = intent.ask.clone().unwrap();
    let mut s = ProfilesScreen::before(intent, ask, listed(), None, None);
    let fx = press(&mut s, MenuEvent::Back, &[]);
    assert!(fx.cmds.is_empty() && fx.connect.is_none());
    assert!(matches!(fx.nav, Some(Nav::Pop)));
}

/// The one line under a card is the shared core's; an empty, lone or missing list says so.
#[test]
fn notes_and_empty_states_read_as_one_line() {
    let mut s = ProfilesScreen::switch(&host(None), &NO_PROFILES).unwrap();
    let busy = ListedProfile {
        seat: Some(Seat {
            state: SeatState::Occupied,
            occupant: Some("Ben's Apple TV".into()),
            ..Default::default()
        }),
        ..profile("kid", "Kid")
    };
    s.set_answer(ProfilesAnswer::Listed(vec![busy, profile("own", "Ben")]));
    let say = with_ctx(&[], &NO_PROFILES, |ctx| s.announcement(ctx));
    assert_eq!(say.as_deref(), Some("Kid, In use by Ben's Apple TV"));
    s.set_answer(ProfilesAnswer::Listed(vec![profile("own", "Ben")]));
    assert_eq!(s.line().as_deref(), Some("Everyone on Desk plays as Ben."));
    s.set_answer(ProfilesAnswer::NoProfiles);
    assert_eq!(s.line().as_deref(), Some("Desk has no profiles."));
    s.set_answer(ProfilesAnswer::Failed("the host refused it (500)".into()));
    assert_eq!(
        s.line().as_deref(),
        Some("Couldn't load the profiles \u{2014} the host refused it (500)")
    );
    let fx = press(&mut s, MenuEvent::Confirm, &[]);
    assert!(fx.cmds.is_empty(), "nothing to pick");
}

#[test]
fn an_unpaired_card_has_no_profiles_to_switch() {
    let h = HostRow {
        paired: false,
        ..host(None)
    };
    assert!(ProfilesScreen::switch(&h, &NO_PROFILES).is_none());
}

/// The Games tab's chooser: focus starts on the shelf's host; another host's circles come
/// from the book or a fetch; OK on a host shows its games; a circle remembers and shows.
#[test]
fn the_games_chooser_follows_the_focused_host() {
    let desk = host(None);
    let attic = HostRow {
        addr: "10.0.0.7".into(),
        ..HostRow::fixture("bb", "Attic")
    };
    let hosts = [desk.clone(), attic.clone()];
    let book = ProfileBook::with("aa", listed());
    let mut s = ProfilesScreen::games(&desk, &hosts, &book);
    assert_eq!(s.title(), "Host and profile");
    assert_eq!(s.focus, Focus::Host(0));
    assert_eq!(s.faces().len(), 2);
    let mut fx = Outbox::default();
    with_ctx(&hosts, &book, |ctx| {
        s.focus_on(Focus::Host(1), ctx, &mut fx)
    });
    assert!(s.waiting(), "Attic is not in the book");
    assert_eq!(fx.cmds, vec![ProfilesScreen::fetch(&attic)]);
    s.set_answer(ProfilesAnswer::Listed(vec![
        profile("x", "Xan"),
        profile("y", "Yu"),
    ]));
    s.focus = Focus::Face(1);
    let mut fx = Outbox::default();
    with_ctx(&hosts, &book, |ctx| {
        s.menu(MenuEvent::Confirm, ctx, &mut fx)
    });
    assert_eq!(
        fx.cmds,
        vec![ConsoleCmd::SetProfile {
            key: "bb".into(),
            profile: Some(pick("y", "Yu", true)),
        }]
    );
    assert_eq!(fx.shelf.map(|h| h.key).as_deref(), Some("bb"));

    let mut s = ProfilesScreen::games(&desk, &hosts, &book);
    s.focus = Focus::Host(1);
    let mut fx = Outbox::default();
    with_ctx(&hosts, &book, |ctx| {
        s.menu(MenuEvent::Confirm, ctx, &mut fx)
    });
    assert!(fx.cmds.is_empty());
    assert_eq!(fx.shelf.map(|h| h.key).as_deref(), Some("bb"));
    assert!(matches!(fx.nav, Some(Nav::Pop)));
}

/// A shell that never asks: the Games chooser shows its hosts and waits on no list, and a
/// remembered pick offers no switch.
#[test]
fn a_shell_that_never_asks_never_waits_on_profiles() {
    let desk = host(Some(pick("kid", "Kid", true)));
    let attic = HostRow {
        addr: "10.0.0.7".into(),
        ..HostRow::fixture("bb", "Attic")
    };
    let hosts = [desk.clone(), attic];
    let book = ProfileBook::new(false);
    assert!(!book.choice(&desk));
    let mut fx = Outbox::default();
    let Screen::Profiles(mut s) =
        ProfilesScreen::games(&desk, &hosts, &book).opened(&desk, &mut fx)
    else {
        unreachable!()
    };
    assert!(!s.waiting());
    assert_eq!(s.line(), None);
    with_ctx(&hosts, &book, |ctx| {
        s.focus_on(Focus::Host(1), ctx, &mut fx)
    });
    assert!(!s.waiting());
    assert!(fx.cmds.is_empty(), "{:?}", fx.cmds);
}

/// The book learns lists and drops an ask once answered; a failure keeps the last list.
#[test]
fn the_book_learns_and_keeps_what_it_knew() {
    let mut book = ProfileBook::default();
    assert!(book.ask("aa"));
    assert!(!book.ask("aa"), "one ask at a time");
    book.learn("aa", &ProfilesAnswer::Listed(listed()));
    assert!(book.asked().is_empty());
    book.learn("aa", &ProfilesAnswer::Failed("down".into()));
    assert_eq!(book.listed("aa").map(<[_]>::len), Some(2));
    assert!(book.choice(&host(None)));
    book.learn("aa", &ProfilesAnswer::NoProfiles);
    assert!(!book.choice(&host(None)));
    assert!(book.choice(&host(Some(pick("kid", "Kid", true)))));
}

/// A picture fills the face's circle and nothing past it; bytes that won't decode leave none.
#[test]
fn a_picture_draws_once_it_decodes_and_fills_its_circle() {
    let mut art = skia_safe::surfaces::raster_n32_premul((40, 20)).unwrap();
    art.canvas().clear(skia_safe::Color::RED);
    let png = art
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((64, 64)).unwrap();
    assert!(!draw_picture(surface.canvas(), "/kid", 32.0, 32.0, 32.0));

    offer_picture("/kid".into(), png.as_bytes().into());
    offer_picture("/bad".into(), vec![1u8, 2, 3].into());
    adopt_pictures();
    assert!(picture_known("/bad"));
    assert!(!draw_picture(surface.canvas(), "/bad", 32.0, 32.0, 32.0));
    assert!(draw_picture(surface.canvas(), "/kid", 32.0, 32.0, 32.0));

    let pixmap = surface.peek_pixels().unwrap();
    assert_eq!(pixmap.get_color((32, 32)), skia_safe::Color::RED);
    assert_eq!(pixmap.get_color((1, 1)).a(), 0, "the corner is outside it");
}
