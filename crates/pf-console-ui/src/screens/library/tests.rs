use super::*;
use crate::coverflow::POSTER_W;
use crate::screens::Screen;
use crate::theme::{contrast, over};
use art::{
    art_cache_size, decode_poster, decode_poster_off_thread, placeholder_face, ART_CACHE_H,
    ART_CACHE_W,
};
use skia_safe::{Color4f, Point};

#[test]
fn a_cover_already_at_cache_size_decodes_here_with_mips() {
    let mut surface = skia_safe::surfaces::raster_n32_premul((60, 90)).unwrap();
    surface
        .canvas()
        .clear(skia_safe::Color::from_rgb(200, 40, 40));
    let png = surface
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap();
    let img = decode_poster(png.as_bytes(), 1.0).expect("decodes");
    assert_eq!((img.width(), img.height()), (60, 90));
    assert!(!img.is_lazy_generated());
    assert!(img.has_mipmaps());
}

fn host() -> HostRow {
    HostRow {
        addr: "10.0.0.5".into(),
        mgmt_port: 9778,
        ..HostRow::fixture("aa", "Desk")
    }
}

/// The coverflow arrangement, which these tests were written against.
fn shelf_settings() -> pf_client_core::trust::Settings {
    pf_client_core::trust::Settings {
        library_view: LibraryView::Shelf.id().to_string(),
        ..Default::default()
    }
}

/// Live model + armed entrance, in the coverflow. `menu` re-syncs; a hand-built shelf
/// is wiped. Titles are not A–Z, so a sort actually changes display order.
fn live_shelf() -> (LibraryScreen, LibraryShared) {
    // Bar steps save settings; point the store at a throwaway HOME first.
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    library.set_games(
        ["Zeta", "Alpha", "Nimbus", "Bravo", "Kilo", "Delta"]
            .iter()
            .enumerate()
            .map(|(i, t)| LibraryGame {
                id: format!("g{i}"),
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
    let mut s = LibraryScreen::new(&host());
    s.view_mode = LibraryView::Shelf;
    s.sync(&library);
    s.entrance_armed = true;
    (s, library)
}

fn press(
    s: &mut LibraryScreen,
    library: &LibraryShared,
    settings: &mut pf_client_core::trust::Settings,
    ev: MenuEvent,
) -> (Option<MenuPulse>, Outbox) {
    let mut fx = Outbox::default();
    let pulse = s.menu(ev, &mut Ctx::test(settings, library), &mut fx);
    (pulse, fx)
}

fn hint_keys(
    s: &LibraryScreen,
    library: &LibraryShared,
    settings: &mut pf_client_core::trust::Settings,
) -> Vec<HintKey> {
    s.hints(&Ctx::test(settings, library))
        .iter()
        .map(|h| h.key)
        .collect()
}

/// A drilled shelf's coverflow: its pills, then its field.
fn plain_shelf() -> (LibraryScreen, LibraryShared) {
    let (mut s, library) = live_shelf();
    s.all_titles();
    (s, library)
}

/// A query keeps the titles containing it, any case; one that matches nothing says so
/// on the state line instead of an empty field.
#[test]
fn a_search_keeps_only_the_matching_titles() {
    let (mut s, _library) = live_shelf();
    s.set_query("A");
    let titles: Vec<&str> = (0..s.len())
        .filter_map(|i| s.game(i).map(|g| g.title.as_str()))
        .collect();
    assert_eq!(titles.len(), 4, "{titles:?}");
    assert!(titles.iter().all(|t| t.to_lowercase().contains('a')));
    assert!(!s.no_match());

    let (mut s, _library) = live_shelf();
    s.set_query("xyz");
    assert!(s.no_match());
    assert!(s.lines(&[], 0).contains(&Line::State));
}

fn up() -> MenuEvent {
    MenuEvent::Move(MenuDir::Up)
}

fn down() -> MenuEvent {
    MenuEvent::Move(MenuDir::Down)
}

fn right() -> MenuEvent {
    MenuEvent::Move(MenuDir::Right)
}

/// Up from the field reaches the pills like any line. OK on a pill writes the setting
/// and never reaches `ready_action`; Up past the pills is the edge, Down returns.
#[test]
fn the_pills_are_a_line_above_the_field() {
    let (mut s, library) = plain_shelf();
    let mut settings = shelf_settings();
    press(&mut s, &library, &mut settings, right());
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(matches!(pulse, Some(MenuPulse::Move)));
    assert_eq!(s.zone, Zone::Bar(0), "up from the shelf reaches the pills");

    let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(
        matches!(pulse, Some(MenuPulse::Boundary)),
        "Default is applied"
    );
    assert!(fx.connect.is_none() && fx.nav.is_none());
    press(&mut s, &library, &mut settings, right());
    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(fx.connect.is_none(), "OK on a pill launched a game");
    assert!(fx.nav.is_none(), "and pushed a screen");
    assert_eq!(settings.library_sort, crate::collate::SortKey::Title.id());
    assert_eq!(s.zone, Zone::Bar(1), "the pill keeps the pad");

    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(
        matches!(pulse, Some(MenuPulse::Boundary)),
        "up past the pills is the edge the shell hands to the tabs"
    );
    press(&mut s, &library, &mut settings, down());
    assert_eq!(s.zone, Zone::Grid);
    s.adopt_settings(&Ctx::test(&mut settings, &library));
    assert_eq!(s.sort, crate::collate::SortKey::Title);
    assert_eq!(
        s.game(1).map(|g| g.title.as_str()),
        Some("Alpha"),
        "the display order follows the sort"
    );
}

/// Walk to the VIEW group: Grid is one press of OK. Search ends the row, and OK there
/// opens the search.
#[test]
fn the_view_pills_swap_the_arrangement() {
    let (mut s, library) = plain_shelf();
    let mut settings = shelf_settings();
    press(&mut s, &library, &mut settings, up());
    for _ in 0..8 {
        press(&mut s, &library, &mut settings, right());
    }
    assert_eq!(s.zone, Zone::Bar(8));
    let (pulse, _) = press(&mut s, &library, &mut settings, right());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)), "the last pill");
    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(
        matches!(&fx.nav, Some(crate::screens::Nav::Push(b)) if matches!(**b, Screen::Search(_))),
        "Search opens its screen"
    );
    press(
        &mut s,
        &library,
        &mut settings,
        MenuEvent::Move(MenuDir::Left),
    );
    assert_eq!(s.zone, Zone::Bar(7));
    press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert_eq!(settings.library_view, LibraryView::Grid.id());
    s.adopt_settings(&Ctx::test(&mut settings, &library));
    assert_eq!(s.view_mode, LibraryView::Grid);
    assert!(s.snap_scroll, "a new arrangement seats rather than glides");
    assert_eq!(s.applied(), [0, 7]);
}

/// A screen reader hears the tile it stepped onto, then the pill and whether it is
/// the one applied.
#[test]
fn the_announcement_names_the_tile_then_the_pill() {
    let (mut s, library) = plain_shelf();
    let mut settings = shelf_settings();
    // The leading tile is the desktop, and it speaks the caption it draws.
    assert_eq!(
        s.announcement(&Ctx::test(&mut settings, &library))
            .as_deref(),
        Some("Desktop")
    );
    press(&mut s, &library, &mut settings, right());
    assert_eq!(
        s.announcement(&Ctx::test(&mut settings, &library))
            .as_deref(),
        Some("Zeta")
    );
    press(&mut s, &library, &mut settings, up());
    assert_eq!(
        s.announcement(&Ctx::test(&mut settings, &library))
            .as_deref(),
        Some("Sort Default, selected")
    );
    press(&mut s, &library, &mut settings, right());
    assert_eq!(
        s.announcement(&Ctx::test(&mut settings, &library))
            .as_deref(),
        Some("Sort A–Z")
    );
}

/// B on a pill leaves the screen, as it does from any other line.
#[test]
fn back_on_a_pill_leaves_like_any_line() {
    let (mut s, library) = plain_shelf();
    let mut settings = shelf_settings();
    press(&mut s, &library, &mut settings, up());
    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Back);
    assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
}

/// A pointer leaves the Hosts shelf from any row. Quiet, the shelf rests on its top
/// row in the same column, so it scrolls home instead of over the returning row.
#[test]
fn a_quiet_shelf_rests_on_its_top_row() {
    let (_, library) = live_shelf();
    let mut s = LibraryScreen::embedded(&host());
    s.sync(&library);
    s.grid_cols_last = Some(3);
    s.cursor = 4;
    s.seat_grid_col();
    s.set_quiet(false);
    assert!(!s.at_top());
    s.follow = false;
    s.set_quiet(true);
    assert!(s.at_top(), "cursor {}", s.cursor);
    assert_eq!(s.cursor, 1, "the column stays");
    assert!(s.follow, "the scroll chases it");
}

/// On a plain grid, Up from row 0 reaches the pills; anywhere else it is a row move.
#[test]
fn up_reaches_the_pills_from_the_grids_top_row_only() {
    let (mut s, library) = plain_shelf();
    let mut settings = pf_client_core::trust::Settings {
        library_view: LibraryView::Grid.id().to_string(),
        ..Default::default()
    };
    // No test draws a frame; navigation reads this.
    s.grid_cols_last = Some(3);
    press(&mut s, &library, &mut settings, down());
    assert_eq!(s.view_mode, LibraryView::Grid);
    // The desktop tile is a one-tile lead band, so it owns row 0 and the games
    // section restarts at column 0 below it — the same split launchers get.
    assert_eq!(s.cursor, 1, "down moved a row");
    press(&mut s, &library, &mut settings, down());
    assert_eq!(s.cursor, 4, "and a second row inside the games section");

    press(&mut s, &library, &mut settings, up());
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(matches!(pulse, Some(MenuPulse::Move)));
    assert_eq!(s.zone, Zone::Grid, "up out of the second row is a row move");
    assert_eq!(s.cursor, 0);

    press(&mut s, &library, &mut settings, up());
    assert!(
        matches!(s.zone, Zone::Bar(_)),
        "…and from the top row it reaches the pills"
    );
}

/// The Games tab's lines: the desktop leaves the grid for its band, focus arrives on
/// the band, Down enters the grid's top row, and above the band sit the chips, then
/// the pills, then the edge. Every step is a direction.
#[test]
fn the_games_tab_walks_chips_bands_and_grid_as_lines() {
    let (mut s, library) = live_shelf();
    let mut settings = pf_client_core::trust::Settings {
        library_view: LibraryView::Grid.id().to_string(),
        ..Default::default()
    };
    s.grid_cols_last = Some(3);
    press(&mut s, &library, &mut settings, down());
    assert_eq!(
        s.zone,
        Zone::Grid,
        "arrival on the Desktops band, Down into the grid"
    );
    assert_eq!(s.focused().map(|g| g.title.as_str()), Some("Zeta"));
    assert!(
        !s.view
            .iter()
            .any(|&i| s.games[i].id == crate::library::DESKTOP_ID),
        "the desktop tile left the grid for its band"
    );
    press(&mut s, &library, &mut settings, down());
    assert_eq!(s.cursor, 3, "inside the grid, Down is a row");
    press(&mut s, &library, &mut settings, up());
    press(&mut s, &library, &mut settings, up());
    assert_eq!(
        s.zone,
        Zone::Band { band: 0, item: 0 },
        "the top row hands up"
    );
    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    let desk = fx.connect.expect("OK on a desktop tile streams");
    assert_eq!(desk.launch, None, "and launches nothing");
    press(&mut s, &library, &mut settings, up());
    assert_eq!(s.zone, Zone::Chip(0));
    press(&mut s, &library, &mut settings, up());
    assert!(
        matches!(s.zone, Zone::Bar(_)),
        "Up from the chips reaches the pills"
    );
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)), "then the tabs");
    press(&mut s, &library, &mut settings, down());
    assert_eq!(s.zone, Zone::Chip(0), "and Down comes back");
    // No other paired host here, so the only chip is Customize.
    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(matches!(
        fx.nav,
        Some(crate::screens::Nav::Push(ref screen)) if matches!(**screen, Screen::Customize(_))
    ));
}

/// The shelf is the Games section's other arrangement: the tab keeps its rows, and the
/// coverflow is one line — Left and Right walk it, Up and Down leave it.
#[test]
fn the_shelf_is_one_line_among_the_tabs_rows() {
    let (mut s, library) = live_shelf();
    let mut settings = shelf_settings();
    press(&mut s, &library, &mut settings, down());
    assert_eq!(
        s.zone,
        Zone::Grid,
        "Down from the Desktops band enters the shelf"
    );
    press(&mut s, &library, &mut settings, right());
    assert_eq!(s.cursor, 1, "Right walks the covers");
    let (pulse, _) = press(&mut s, &library, &mut settings, down());
    assert!(
        matches!(pulse, Some(MenuPulse::Boundary)),
        "nothing below the shelf"
    );
    press(&mut s, &library, &mut settings, up());
    assert_eq!(s.zone, Zone::Band { band: 0, item: 0 });
    assert_eq!(s.cursor, 1, "and the shelf keeps its place");
}

/// A failed list keeps the chips and the Desktops row, with Retry where the field was.
/// Focus lands on Retry; Up walks to the tabs. A drilled shelf has only the button.
#[test]
fn a_failed_list_offers_retry_and_a_way_up() {
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    library.set_phase(LibraryPhase::Error {
        title: "Couldn't load the library".into(),
        body: "The host refused.".into(),
        can_retry: true,
    });
    let mut settings = pf_client_core::trust::Settings::default();
    let mut s = LibraryScreen::new(&host());
    let (pulse, _) = press(&mut s, &library, &mut settings, right());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    assert_eq!(s.zone, Zone::State, "focus lands on Retry");
    assert!(hint_keys(&s, &library, &mut settings).contains(&HintKey::Confirm));
    press(&mut s, &library, &mut settings, up());
    assert_eq!(
        s.zone,
        Zone::Band { band: 0, item: 0 },
        "the desk still streams"
    );
    press(&mut s, &library, &mut settings, up());
    assert_eq!(s.zone, Zone::Chip(0));
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(
        matches!(pulse, Some(MenuPulse::Boundary)),
        "the tabs are one more Up"
    );
    press(&mut s, &library, &mut settings, down());
    press(&mut s, &library, &mut settings, down());
    let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(matches!(pulse, Some(MenuPulse::Confirm)));
    assert!(
        matches!(fx.cmds.as_slice(), [ConsoleCmd::FetchLibrary { .. }]),
        "Retry fetches again"
    );

    let mut s = LibraryScreen::new(&host());
    s.all_titles();
    library.set_phase(LibraryPhase::Error {
        title: "Couldn't load the library".into(),
        body: "The host refused.".into(),
        can_retry: true,
    });
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    assert_eq!(s.zone, Zone::State);
    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(!fx.cmds.is_empty());
}

/// While the list loads the tab's Desktops row holds focus, and Up still leaves.
#[test]
fn a_loading_list_keeps_focus_on_the_desktops_row() {
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    let mut settings = pf_client_core::trust::Settings::default();
    let mut s = LibraryScreen::new(&host());
    let (pulse, _) = press(&mut s, &library, &mut settings, down());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    assert_eq!(s.zone, Zone::Band { band: 0, item: 0 });
    press(&mut s, &library, &mut settings, up());
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    assert_eq!(s.zone, Zone::Chip(0));
}

/// A band holds its titles out of the grid only while it shows; Recently played is
/// newest first and skips what was never played.
#[test]
fn a_band_holds_its_titles_out_of_the_grid_only_while_it_shows() {
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    let mut list = games(&[
        ("Steam", None),
        ("Old", None),
        ("New", None),
        ("Never", None),
    ]);
    list[0].launcher = true;
    let played = |at| {
        Some(pf_client_core::library::GameStats {
            last_played_unix_ms: at,
            ..Default::default()
        })
    };
    list[1].stats = played(5);
    list[2].stats = played(9);
    library.set_games(list);
    let mut s = LibraryScreen::new(&host());
    s.sync(&library);
    let mut settings = pf_client_core::trust::Settings::default();
    let titles = |s: &LibraryScreen, band: &games::Band| -> Vec<String> {
        (band.items.iter())
            .map(|it| match it {
                games::Item::Game(i) => s.games[*i].title.clone(),
                games::Item::Desktop(h) => h.name.clone(),
                games::Item::Collection(c) => s.collections[*c].label.clone(),
            })
            .collect()
    };
    s.adopt_settings(&Ctx::test(&mut settings, &library));
    let (bands, before) = s.bands(&Ctx::test(&mut settings, &library));
    let sections: Vec<_> = bands.iter().map(|b| b.section).collect();
    use crate::library::Section;
    assert_eq!(
        sections,
        vec![Section::Desktops, Section::Recent, Section::Launchers]
    );
    assert_eq!(
        before, 3,
        "all three sit above the grid, favorites hidden empty"
    );
    assert_eq!(titles(&s, &bands[1]), vec!["New", "Old"]);
    assert!(!s.view.iter().any(|&i| s.games[i].launcher));

    settings.library_sections = "-launchers,-desktops".into();
    s.adopt_settings(&Ctx::test(&mut settings, &library));
    let (bands, _) = s.bands(&Ctx::test(&mut settings, &library));
    assert!(bands
        .iter()
        .all(|b| !matches!(b.section, Section::Launchers | Section::Desktops)));
    assert!(
        !s.view.iter().any(|&i| s.games[i].leads()),
        "switched off, the launchers and the desktop stay off"
    );
    assert!(!s.view.is_empty(), "the titles still fill the grid");
}

#[test]
fn the_legend_swaps_with_the_focus() {
    let (mut s, library) = plain_shelf();
    let mut settings = shelf_settings();
    let field = hint_keys(&s, &library, &mut settings);
    assert!(field.contains(&HintKey::Confirm) && field.contains(&HintKey::Secondary));
    assert!(
        !field.contains(&HintKey::Up),
        "the pills are in sight, not a hint"
    );

    press(&mut s, &library, &mut settings, up());
    let pills = hint_keys(&s, &library, &mut settings);
    assert!(pills.contains(&HintKey::Confirm) && pills.contains(&HintKey::Back));
    assert!(
        !pills.contains(&HintKey::Secondary),
        "the pills' legend still offered the field's options"
    );
    assert!(
        pills.len() <= 4,
        "the legend is read from a sofa: {} entries",
        pills.len()
    );
}

/// A plain shelf in its art wait draws a spinner, so it offers no pills to walk onto.
#[test]
fn a_plain_shelf_offers_no_pills_until_its_field_shows() {
    let (mut s, library) = plain_shelf();
    s.entrance_armed = false;
    let mut settings = shelf_settings();
    let (pulse, _) = press(&mut s, &library, &mut settings, up());
    assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    assert_eq!(s.zone, Zone::Grid);
}

/// List landed, no art yet.
fn waiting_shelf() -> LibraryScreen {
    let mut s = LibraryScreen::new(&host());
    s.phase = LibraryPhase::Ready;
    s.games = (0..6)
        .map(|i| LibraryGame {
            id: format!("g{i}"),
            title: format!("Game {i}"),
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
        .collect();
    s.recollate();
    s
}

/// Unarmed `None` is not settled. First draw is entrance frame 1 (fade 0).
#[test]
fn a_shelf_is_hidden_until_its_entrance_begins_not_settled() {
    let mut s = waiting_shelf();
    s.arm_entrance(10.0);
    assert!(!s.entrance_armed, "armed with nothing to show");
    for steps in 0..s.len() {
        let at = s.entrance_at(steps, 10.0);
        assert_eq!(
            (at.travel, at.fade),
            (0.0, 0.0),
            "card {steps} was drawn before the entrance began"
        );
    }
    s.arm_entrance(10.39);
    assert!(!s.entrance_armed, "the deadline is 400 ms");

    s.arm_entrance(10.4);
    assert!(s.entrance_armed);
    assert_eq!(s.entrance_at(0, 10.4).fade, 0.0, "the shelf flashed");

    s.entrance = None;
    assert_eq!(s.entrance_at(3, 99.0), EntranceAt::SETTLED);
}

#[test]
fn decoded_art_arms_the_entrance_without_waiting_out_the_deadline() {
    let mut s = waiting_shelf();
    s.arm_entrance(4.0);
    assert!(!s.entrance_armed);
    let mut surface = skia_safe::surfaces::raster_n32_premul((2, 2)).expect("2×2 raster");
    s.art.insert("g1".into(), surface.image_snapshot());
    s.arm_entrance(4.05);
    assert!(s.entrance_armed, "a decoded poster is the whole point");
}

/// Coverless monogram vs face must contrast on every palette. Side cards overlap: alpha leaks.
#[test]
fn a_coverless_card_reads_on_every_palette() {
    for p in &crate::palette::PALETTES {
        crate::theme::set_ink(crate::theme::Ink::of(p));
        for launcher in [false, true] {
            let face = placeholder_face(launcher);
            assert_eq!(face.a, 1.0, "{} face is translucent", p.id);
            let c = contrast(over(fg(0.85), face), face);
            assert!(c > 3.0, "the monogram is unreadable on {}: {c:.2}:1", p.id);
        }
    }
    crate::theme::set_ink(crate::theme::Ink::of(crate::palette::palette("violet")));
}

/// Stamp after draw. Arrival-order LRU drops the neighbourhood the cursor is in.
#[test]
fn eviction_drops_the_coldest_and_keeps_the_focused_neighbourhood() {
    let live: Vec<String> = (0..ArtBudget::DESKTOP.held + 40)
        .map(|i| format!("g{i}"))
        .collect();
    let mut seen = HashMap::new();
    for id in &live {
        seen.insert(id.clone(), 10u64);
    }
    let hot: Vec<String> = (100..112).map(|i| format!("g{i}")).collect();
    for id in &hot {
        seen.insert(id.clone(), 9_000);
    }
    let dropped = art_to_evict(&live, &seen, ArtBudget::DESKTOP.held);
    assert_eq!(dropped.len(), 40, "trimmed back to exactly the budget");
    for id in &hot {
        assert!(!dropped.contains(id), "{id} was on screen and got evicted");
    }
}

#[test]
fn eviction_does_nothing_under_the_budget() {
    let live: Vec<String> = (0..ArtBudget::DESKTOP.held)
        .map(|i| format!("g{i}"))
        .collect();
    assert!(art_to_evict(&live, &HashMap::new(), ArtBudget::DESKTOP.held).is_empty());
}

/// Cache ≥ focused shelf card, ≤ source, source aspect. Smaller magnifies; larger wastes GPU.
#[test]
fn a_cached_poster_is_bounded_by_the_size_it_is_drawn_at() {
    for k in [0.75, 1.0, 1.25, 1.5, 1.8, 2.7, 3.0] {
        for src in [(600, 900), (1000, 1500), (460, 215), (300, 450), (64, 64)] {
            let (w, h) = art_cache_size(src, k);
            assert!(
                w <= src.0 && h <= src.1,
                "{src:?} at k={k} was enlarged to {w}×{h}"
            );
            let drawn = (POSTER_W * k).min(f64::from(src.0));
            assert!(
                f64::from(w) >= drawn - 1.0,
                "{src:?} at k={k} cached {w} px wide, under the {drawn:.0} px drawn"
            );
            assert!(
                f64::from(w) <= ART_CACHE_W * k + 1.0 && f64::from(h) <= ART_CACHE_H * k + 1.0,
                "{src:?} at k={k} cached {w}×{h}, outside the cache box"
            );
            let (want, got) = (
                f64::from(src.0) / f64::from(src.1),
                f64::from(w) / f64::from(h),
            );
            assert!(
                (want - got).abs() < 0.02,
                "{src:?} at k={k} came back {w}×{h} — aspect {got:.3} for a {want:.3} source"
            );
        }
    }
}

/// 8-col clamp × ~3 visible rows; 6 is generous. `k` is height/800, so row count is stable.
const SCREENFUL: usize = 8 * 6;

/// What one screen of covers plus the render targets holds at `k`, bytes.
fn screenful_bytes(src: (i32, i32), k: f64) -> usize {
    // RGBA + 1/3 for the mip chain `decode_poster` bakes.
    let bytes = |(w, h): (i32, i32)| (w as usize) * (h as usize) * 4 * 4 / 3;
    bytes(art_cache_size(src, k)) * SCREENFUL + 2 * (1280.0 * k) as usize * (800.0 * k) as usize * 4
}

/// Over budget, Skia purges and the next frame re-decodes JPEG on the render thread.
#[test]
fn a_screenful_of_covers_fits_the_gpu_budget() {
    // Through 1440p. 4K + 1000×1500 art is outside `DEFAULT_GPU_CACHE_BYTES`.
    for k in [0.75, 1.0, 1.35, 1.8] {
        for src in [(600, 900), (1000, 1500)] {
            let need = screenful_bytes(src, k);
            let budget = crate::shell::DEFAULT_GPU_CACHE_BYTES;
            assert!(
                need < budget,
                "{src:?} art at k={k}: {} MB needed over a {} MB budget",
                need >> 20,
                budget >> 20
            );
        }
    }
}

/// The floor a memory-tight host may pass, pinned to what 1080p actually needs.
///
/// A television is `k` 1.35, and this is the number a TV client copies instead of
/// guessing a fraction of the desktop default — a guess that put webOS on 64 MB,
/// under the working set, re-decoding covers on the render thread every frame.
#[test]
fn the_gpu_budget_floor_covers_a_1080p_screenful() {
    for src in [(600, 900), (1000, 1500)] {
        let need = screenful_bytes(src, 1.35);
        let floor = crate::shell::MIN_GPU_CACHE_BYTES;
        assert!(
            need < floor,
            "{src:?} art at 1080p: {} MB needed under a {} MB floor",
            need >> 20,
            floor >> 20
        );
    }
}

#[test]
fn never_drawn_posters_are_evicted_before_merely_old_ones() {
    let live: Vec<String> = (0..ArtBudget::DESKTOP.held + 2)
        .map(|i| format!("g{i}"))
        .collect();
    let mut seen: HashMap<String, u64> = live.iter().map(|id| (id.clone(), 5)).collect();
    seen.remove("g7");
    seen.remove("g9");
    let dropped = art_to_evict(&live, &seen, ArtBudget::DESKTOP.held);
    assert_eq!(dropped.len(), 2);
    assert!(dropped.contains(&"g7".to_string()));
    assert!(dropped.contains(&"g9".to_string()));
}

/// Platform-less Steam collates as store: `[None, None]` is one collection, `[Some, None]` two.
/// 200 titles in the grid, the desktop tile's band above them.
fn long_grid() -> (
    LibraryScreen,
    LibraryShared,
    pf_client_core::trust::Settings,
) {
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    let titles: Vec<String> = (0..200).map(|i| format!("Title {i:03}")).collect();
    let spec: Vec<(&str, Option<&str>)> = titles.iter().map(|t| (t.as_str(), None)).collect();
    library.set_games(games(&spec));
    let mut s = LibraryScreen::new(&host());
    // A drilled shelf's plain grid: no sections above.
    s.all_titles();
    s.sync(&library);
    s.entrance_armed = true;
    let settings = pf_client_core::trust::Settings {
        library_view: LibraryView::Grid.id().to_string(),
        ..Default::default()
    };
    (s, library, settings)
}

/// Frames at 60 Hz; the drawn cells' indices. `PF_GRID_DUMP=<dir>` also writes the frame.
fn grid_frames(
    s: &mut LibraryScreen,
    library: &LibraryShared,
    settings: &mut pf_client_core::trust::Settings,
    frames: usize,
    name: &str,
) -> Vec<usize> {
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
    let rect = Rect::from_xywh(0.0, 0.0, 1280.0, 800.0);
    for _ in 0..frames {
        surface
            .canvas()
            .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
        s.render(
            surface.canvas(),
            rect,
            1.0,
            1.0 / 60.0,
            &fonts,
            &mut Ctx::test(settings, library),
        );
    }
    if let Ok(dir) = std::env::var("PF_GRID_DUMP") {
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, None)
            .expect("png");
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).expect("write");
    }
    (0..s.geom.len())
        .filter(|i| !s.geom[*i].is_empty())
        .collect()
}

#[test]
fn a_long_grid_draws_only_the_rows_in_view() {
    let (mut s, library, mut settings) = long_grid();
    let drawn = grid_frames(&mut s, &library, &mut settings, 60, "grid-top");
    assert_eq!(drawn.first(), Some(&0), "the desktop tile");
    assert!(drawn.len() < 40, "a screenful, not the library: {drawn:?}");
    assert!(!drawn.contains(&200));

    s.cursor = 57;
    s.seat_grid_col();
    let drawn = grid_frames(&mut s, &library, &mut settings, 90, "grid-mid");
    assert!(drawn.contains(&57) && !drawn.contains(&0), "{drawn:?}");

    s.cursor = 200;
    s.seat_grid_col();
    let drawn = grid_frames(&mut s, &library, &mut settings, 90, "grid-end");
    assert_eq!(drawn.last(), Some(&200), "the last title in view");
    assert!(drawn.len() < 40, "{drawn:?}");
}

/// A finger drags the grid one to one and a flick carries on; the pad's next move
/// brings the focus row back into view.
#[test]
fn a_finger_pans_the_grid_until_the_pad_moves_focus() {
    let (mut s, library, mut settings) = long_grid();
    grid_frames(&mut s, &library, &mut settings, 30, "pan-0");
    let offset = |s: &LibraryScreen| s.grid.borrow().offset(Id::new(GRID, 0));
    // Title 3 sits in the grid's second row, on screen.
    let cell = s.geom[3];
    assert!(!cell.is_empty());
    let finger = |kind| Pointer {
        x: f64::from(cell.center_x()),
        y: f64::from(cell.center_y()),
        kind,
    };
    assert!(s.pan(finger(PointerKind::PanStart { horizontal: false })));
    assert!(s.pan(finger(PointerKind::Pan {
        dx: 0.0,
        dy: -120.0
    })));
    grid_frames(&mut s, &library, &mut settings, 1, "pan-1");
    assert_eq!(offset(&s), 120.0);
    assert_eq!(
        s.geom[3].top,
        cell.top - 120.0,
        "the cards follow the finger"
    );

    assert!(s.pan(finger(PointerKind::Fling {
        vx: 0.0,
        vy: -1500.0
    })));
    grid_frames(&mut s, &library, &mut settings, 120, "pan-2");
    assert!(offset(&s) > 500.0, "the flick carried on: {}", offset(&s));
    assert!(
        s.geom[0].is_empty() || s.geom[0].bottom <= 0.0,
        "the focus row is left behind"
    );

    let mut fx = Outbox::default();
    s.menu(
        MenuEvent::Move(MenuDir::Down),
        &mut Ctx::test(&mut settings, &library),
        &mut fx,
    );
    grid_frames(&mut s, &library, &mut settings, 120, "pan-3");
    let focus = s.cursor as usize;
    assert!(!s.geom[focus].is_empty(), "focus {focus} back in view");
}

fn games(spec: &[(&str, Option<&str>)]) -> Vec<LibraryGame> {
    spec.iter()
        .enumerate()
        .map(|(i, (title, platform))| LibraryGame {
            id: format!("g{i}"),
            title: (*title).to_string(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: platform.map(str::to_string),
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

/// Covers already held do not push the decode-ahead further down the library.
#[test]
fn decode_ahead_is_a_window_of_places() {
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    let titles: Vec<String> = (0..300).map(|i| format!("Title {i:03}")).collect();
    let spec: Vec<(&str, Option<&str>)> = titles.iter().map(|t| (t.as_str(), None)).collect();
    library.set_games(games(&spec));
    let mut s = LibraryScreen::new(&host());
    s.all_titles();
    s.sync(&library);
    let poster = skia_safe::surfaces::raster_n32_premul((4, 6))
        .expect("a raster surface")
        .image_snapshot();
    let held: Vec<String> = s
        .view
        .iter()
        .take(48)
        .map(|&g| s.games[g].id.clone())
        .collect();
    for id in held {
        s.art.insert(id, poster.clone());
    }
    assert_eq!(s.art_wanted(), Vec::<String>::new());
}

/// First list is always "fresh" on an empty screen; adopted art must survive that, not a later one.
#[test]
fn handed_over_posters_survive_the_first_list_and_only_that_one() {
    let poster = || {
        skia_safe::surfaces::raster_n32_premul((4, 6))
            .expect("a raster surface")
            .image_snapshot()
    };
    let library = LibraryShared::default();
    library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
    let mut s = LibraryScreen::new(&host());
    s.adopt_art(HashMap::from([("g0".to_string(), poster())]));
    s.sync(&library);
    assert_eq!(s.art.len(), 1, "the hand-over was wiped by the first list");

    library.set_games(games(&[("Rez", Some("PS2"))]));
    s.sync(&library);
    assert!(s.art.is_empty(), "a different library kept the old covers");
}

/// The tile is the host, not a title: Confirm streams it with no launch id, and Y
/// opens the HOST's options — a title menu here would offer a per-title preset
/// binding for a title that does not exist.
#[test]
fn confirm_on_the_desktop_tile_launches_nothing() {
    let (mut s, library) = plain_shelf();
    let mut settings = shelf_settings();
    assert_eq!(
        s.focused().map(|g| g.id.as_str()),
        Some(crate::library::DESKTOP_ID),
        "the tile is where the cursor starts"
    );

    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    let intent = fx.connect.expect("a connect intent");
    assert_eq!(intent.launch, None, "the desktop tile launched a title");
    assert_eq!(intent.title, "Desk", "the takeover names the host");

    let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Secondary);
    assert!(
        matches!(fx.nav, Some(crate::screens::Nav::Push(_))),
        "Y on the tile opens the host's options"
    );
}

/// A host with no plugins is still one press from its desk. The catalog's verdict
/// stands — the empty copy is what explains the missing shelf.
#[test]
fn an_empty_library_still_streams_the_desktop() {
    crate::screens::settings::tests::fake_home();
    let library = LibraryShared::default();
    library.set_games(Vec::new());
    let mut s = LibraryScreen::new(&host());
    s.sync(&library);
    s.entrance_armed = true;
    assert!(matches!(s.phase, LibraryPhase::Empty));

    let mut settings = shelf_settings();
    let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
    assert!(matches!(pulse, Some(MenuPulse::Confirm)));
    let intent = fx.connect.expect("a connect intent");
    assert_eq!(intent.launch, None);
    assert_eq!(intent.addr, "10.0.0.5");
    assert!(
        hint_keys(&s, &library, &mut settings).contains(&HintKey::Confirm),
        "the legend must offer the press that works"
    );
}

/// The caption is the verb: it names the game the host already has up, so the tile
/// does not read "Desktop" while pressing it resumes Elden Ring.
#[test]
fn the_desktop_tile_says_resume_when_the_host_has_a_game_up() {
    let busy = HostRow {
        running: "Elden Ring".into(),
        ..host()
    };
    let s = LibraryScreen::new(&busy);
    assert_eq!(s.desktop_caption(), "Resume Elden Ring");
    assert_eq!(s.desktop_intent().title, "Elden Ring");

    let idle = LibraryScreen::new(&host());
    assert_eq!(idle.desktop_caption(), "Desktop");
    assert_eq!(idle.desktop_intent().title, "Desk");
}

/// A cover one screen took off the decoded queue still reaches a second screen: its bytes
/// stay in the model, and the second screen decodes them for itself.
#[test]
fn a_cover_one_screen_took_reaches_another() {
    let library = LibraryShared::default();
    library.set_games(games(&[("Alpha", None)]));
    let bytes = poster_png(1);
    library.push_art("g0".into(), bytes.clone());
    library.push_decoded("g0".into(), decode_poster_off_thread(&bytes, 1.0).unwrap());
    let mut first = LibraryScreen::new(&host());
    let mut second = LibraryScreen::new(&host());
    first.sync(&library);
    assert!(
        first.art.contains_key("g0"),
        "the first takes the decoded cover"
    );
    second.sync(&library);
    second.sync(&library);
    assert!(
        second.art.contains_key("g0"),
        "the second decodes the bytes"
    );
}

/// Two screens on one host share a cover: the second takes the first's image, with no
/// bytes to decode from and nothing sent to its decoder.
#[test]
fn screens_on_one_host_share_a_cover() {
    let library = LibraryShared::default();
    library.set_games(games(&[("Alpha", None)]));
    let poster = decode_poster_off_thread(&poster_png(2), 1.0).unwrap();
    library.push_decoded("g0".into(), poster);
    let mut first = LibraryScreen::new(&host());
    let mut second = LibraryScreen::new(&host());
    first.sync(&library);
    second.sync(&library);
    assert!(second.art.contains_key("g0"), "shared, not decoded");
    assert!(!second.decoder.pending("g0"));
}

/// A 2:3 poster, PNG-encoded: a two-hue gradient with a pale disc, colour from `seed`.
fn poster_png(seed: usize) -> Vec<u8> {
    let hues = [
        (0.85, 0.30, 0.35),
        (0.25, 0.50, 0.85),
        (0.30, 0.72, 0.45),
        (0.88, 0.62, 0.22),
        (0.55, 0.30, 0.80),
        (0.20, 0.70, 0.75),
    ];
    let (a, b) = (hues[seed % hues.len()], hues[(seed + 2) % hues.len()]);
    let mut surface = skia_safe::surfaces::raster_n32_premul((300, 450)).unwrap();
    let canvas = surface.canvas();
    let mut p = crate::theme::shaded();
    p.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (Point::new(0.0, 0.0), Point::new(300.0, 450.0)),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(
                &[
                    Color4f::new(a.0, a.1, a.2, 1.0),
                    Color4f::new(b.0 * 0.4, b.1 * 0.4, b.2 * 0.4, 1.0),
                ],
                skia_safe::TileMode::Clamp,
                None,
            ),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rect(Rect::from_wh(300.0, 450.0), &p);
    let disc = Color4f::new(1.0, 1.0, 1.0, 0.35);
    canvas.draw_circle((150.0, 170.0 + (seed % 3) as f32 * 30.0), 80.0, &fill(disc));
    surface
        .image_snapshot()
        .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
        .unwrap()
        .as_bytes()
        .to_vec()
}

/// Ignored eyeball dump of the Games tab, `PF_CONSOLE_DUMP=<dir> cargo test -p
/// pf-console-ui --release --lib -- --ignored dump_library`: the rows, the pills, the
/// shelf, the three list states, the Collections row, and a phone.
#[test]
#[ignore]
fn dump_library() {
    use crate::shell::{ConsoleOptions, Shell};
    let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
    crate::screens::settings::tests::fake_home();
    let fonts = crate::theme::build_fonts().unwrap();
    let hosts = || {
        let one = HostRow {
            key: "aa11".into(),
            name: "Living Room PC".into(),
            fp_hex: "aa11".into(),
            os: "windows".into(),
            ..host()
        };
        let two = HostRow {
            key: "bb22".into(),
            name: "Office Tower".into(),
            fp_hex: "bb22".into(),
            os: "fedora".into(),
            online: false,
            running: "Hades II".into(),
            ..host()
        };
        vec![one, two]
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let titles = [
        "Steam",
        "Hades II",
        "Elden Ring",
        "Hollow Knight",
        "Celeste",
        "Tunic",
        "Baldur's Gate 3",
        "Deep Rock Galactic",
        "Portal 2",
        "Outer Wilds",
        "Disco Elysium",
        "Stardew Valley",
        "Cyberpunk 2077",
        "Inside",
    ];
    let list = || -> Vec<LibraryGame> {
        let mut list: Vec<LibraryGame> = (titles.iter().enumerate())
            .map(|(i, t)| LibraryGame {
                id: format!("steam:{i}"),
                title: (*t).to_string(),
                store: if i % 5 == 3 { "epic" } else { "steam" }.into(),
                launcher: i == 0,
                icon: if i == 0 {
                    "steam".into()
                } else {
                    String::new()
                },
                platform: (i % 4 == 2).then(|| "PC".to_string()),
                developer: None,
                year: None,
                genres: Vec::new(),
                stats: None,
                running: i == 1,
                endable: false,
                install: None,
            })
            .collect();
        for (i, hours) in [(2, 2), (3, 30), (5, 80)] {
            list[i].stats = Some(pf_client_core::library::GameStats {
                last_played_unix_ms: now - hours * 3_600_000,
                play_time_ms: hours * 1_900_000,
                ..Default::default()
            });
        }
        list
    };
    let shell = |settings: pf_client_core::trust::Settings, library: &LibraryShared, stack| {
        let console = crate::model::ConsoleShared::default();
        console.set_hosts(hosts());
        let mut opts = ConsoleOptions::desktop("deck".into(), false);
        opts.store = Some(std::sync::Arc::new(crate::store::SnapshotStore::new(
            settings,
            Vec::new(),
        )));
        let bus = crate::model::ConsoleBus::default();
        let mut s = Shell::new(console, library.clone(), bus, opts, stack).unwrap();
        s.fake_clock = Some((0.0, 1.0 / 60.0));
        s
    };
    let dump = |s: &mut Shell, frames: usize, name: &str| {
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        for _ in 0..frames {
            s.render(
                surface.canvas(),
                1280,
                800,
                &fonts,
                Some("Xbox Wireless Controller"),
                Some(punktfunk_core::config::GamepadPref::Xbox360),
                &[],
            );
        }
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
    };
    let tab = |view: LibraryView, sort: &str, palette: &str, library: &LibraryShared| {
        let mut settings = pf_client_core::trust::Settings {
            library_view: view.id().to_string(),
            library_sort: sort.to_string(),
            ui_palette: palette.to_string(),
            ..Default::default()
        };
        crate::library::toggle_favorite(&mut settings, "aa11", "steam:4");
        let root = Screen::Library(LibraryScreen::new(&hosts()[0]));
        shell(settings, library, vec![root])
    };
    let games_tab = |view: LibraryView, library: &LibraryShared| tab(view, "", "violet", library);
    let full = || {
        let library = LibraryShared::default();
        library.set_games(list());
        for i in 1..titles.len() {
            if i != 6 {
                library.push_art(format!("steam:{i}"), poster_png(i));
            }
        }
        library
    };
    let menu = |s: &mut Shell, evs: &[MenuEvent]| {
        for ev in evs {
            s.handle_menu(*ev);
        }
    };

    let library = full();
    let mut s = games_tab(LibraryView::Grid, &library);
    dump(&mut s, 90, "L1-games-arrival");
    menu(&mut s, &[down(), down(), down()]);
    dump(&mut s, 50, "L2-games-favorites");
    menu(&mut s, &[down(), down(), right()]);
    dump(&mut s, 50, "L3-games-grid");
    // Under the Recent sort each card captions when it was played; a pale palette.
    let mut s = tab(LibraryView::Grid, "recent", "sky", &full());
    dump(&mut s, 60, "_settle");
    menu(&mut s, &[down(), down(), down(), down()]);
    dump(&mut s, 50, "L3b-games-recent-mint");
    let mut s = games_tab(LibraryView::Grid, &full());
    dump(&mut s, 60, "_settle");
    menu(&mut s, &[up(), up(), right()]);
    dump(&mut s, 12, "L4-games-pills-travel");
    dump(&mut s, 50, "L4-games-pills");

    let mut s = games_tab(LibraryView::Shelf, &full());
    dump(&mut s, 60, "_settle");
    menu(&mut s, &[down(), down(), down(), down(), right(), right()]);
    dump(&mut s, 60, "L5-games-shelf");

    for (name, phase) in [
        (
            "L6-games-error",
            Some(LibraryPhase::Error {
                title: "Couldn't load the library".into(),
                body: "The host didn't answer. Check that it is awake and on this network.".into(),
                can_retry: true,
            }),
        ),
        ("L7-games-loading", None),
        ("L8-games-empty", Some(LibraryPhase::Empty)),
    ] {
        let library = LibraryShared::default();
        match phase {
            Some(LibraryPhase::Empty) => library.set_games(Vec::new()),
            Some(p) => library.set_phase(p),
            None => {}
        }
        let mut s = games_tab(LibraryView::Grid, &library);
        dump(&mut s, 60, name);
    }

    // Drilled: the pills and a coverflow of the whole library.
    let settings = pf_client_core::trust::Settings {
        library_view: LibraryView::Shelf.id().to_string(),
        ..Default::default()
    };
    let mut drilled = LibraryScreen::new(&hosts()[0]);
    drilled.all_titles();
    let root = Screen::Library(LibraryScreen::new(&hosts()[0]));
    let mut s = shell(settings, &full(), vec![root, Screen::Library(drilled)]);
    dump(&mut s, 60, "_settle");
    menu(&mut s, &[right(), right(), right()]);
    dump(&mut s, 60, "L9-shelf-drilled");

    let mixed = LibraryShared::default();
    let mut games = list();
    for (i, g) in games.iter_mut().enumerate() {
        g.platform = Some(["PC", "PS2", "SNES"][i % 3].into());
    }
    mixed.set_games(games);
    for i in 1..titles.len() {
        mixed.push_art(format!("steam:{i}"), poster_png(i));
    }
    let mut s = games_tab(LibraryView::Grid, &mixed);
    dump(&mut s, 60, "LA-collections-row");

    // A phone in landscape, as the shell's own phone dump sizes it.
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
    let mut s = games_tab(LibraryView::Grid, &full());
    s.device.platform = crate::platform::Platform::Apple;
    let phone = |s: &mut Shell, frames: usize, name: &str| {
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
    phone(&mut s, 90, "LP1-phone-games");
    menu(&mut s, &[down(), down(), down(), down()]);
    phone(&mut s, 60, "LP2-phone-grid");
}
