//! The pseudo-language gate: the screens the dump walks, drawn in `en-XA` at the sizes the kit
//! ships. It fails on text that skips pf-l10n and on text wider than the room it was given.

use super::*;
use crate::console::{Insets, Viewport};
use crate::platform::Platform;
use crate::textprobe::{self, Run};
use std::collections::BTreeMap;

/// Frames a scene is drawn for, 0.4 s apart: long enough for the entrances to land and the
/// library's art-wait to lapse. Only the last frame is read.
const FRAMES: usize = 3;
const STEP: f64 = 0.4;
const PAD: &str = "Xbox Wireless Controller";
/// A pixel of slack for the rounding between a measure and a clip.
const SLACK: f32 = 1.0;

/// A size the kit ships, and the platform that draws at it.
struct Size {
    name: &'static str,
    viewport: Viewport,
    platform: Platform,
}

fn sizes() -> [Size; 3] {
    [
        Size {
            name: "desk 1280x800",
            viewport: Viewport::plain(1280, 800),
            platform: Platform::Desktop,
        },
        // `store_shots`: Android TV at 1080p.
        Size {
            name: "tv 1920x1080",
            viewport: Viewport::plain(1920, 1080),
            platform: Platform::Android,
        },
        // `dump_phone_home`: an iPhone Pro Max in landscape.
        Size {
            name: "phone 2868x1320",
            viewport: Viewport {
                width: 2868,
                height: 1320,
                insets: Insets {
                    left: 186.0,
                    top: 0.0,
                    right: 186.0,
                    bottom: 63.0,
                },
                scale: Some(2.25),
            },
            platform: Platform::Apple,
        },
    ]
}

// --- What the gate lets through -------------------------------------------------------------

/// Why a run may be a literal, and the test for it.
type Rule = (&'static str, fn(&str) -> bool);

/// A run that is not a message is allowed to be one of these. One line of reason each.
const LITERAL: &[Rule] = &[
    ("a number, port or version the code formats", is_number),
    (
        "a name the scene's data brings: host, title, device, profile, controller",
        is_data,
    ),
    (
        "a button or key name, or a chord of them, drawn as a cap",
        is_key,
    ),
    ("an initial, a dash or a keyboard cap", is_cap),
    ("a product or store name, with a version", is_product),
    ("the licence text the build embeds", is_legal),
    ("a word of a message drawn around a badge", is_fragment),
];

/// Text the kit still draws without a message. Each entry is a gap to close; the table only
/// shrinks, and an entry that no longer draws fails the test.
const LITERAL_DEBT: &[(&str, &str)] = &[];

/// Messages that overflow under the pseudo-language today, by key. Waits for the wording pass.
const OVERFLOW_DEBT: &[(&str, &str)] = &[];

const UNITS: &[&str] = &[
    "s", "ms", "Hz", "fps", "Mbit/s", "Gbit/s", "kbit/s", "MB", "GB", "%", "px",
];

fn is_number(t: &str) -> bool {
    t.split_whitespace().all(|w| {
        UNITS.contains(&w)
            || (w.chars().any(|c| c.is_ascii_digit())
                && w.chars()
                    .all(|c| c.is_ascii_digit() || ".,:/-+×x%".contains(c)))
    })
}

/// What the scenes put in: hosts, titles, platforms, profiles, a controller, the app's own
/// question and a driver's reason.
const DATA: &[&str] = &[
    "Living Room PC",
    "Office Tower",
    "steambox",
    "deck",
    PAD,
    "DualSense Wireless Controller",
    "Hades II",
    "Elden Ring",
    "Hollow Knight",
    "Celeste",
    "Tunic",
    "PlayStation 3",
    "Xbox 360",
    "SNES",
    "Mega Drive",
    "Theo",
    "Ben",
    "Mia Rossi",
    "Kid",
    "Guest",
    "Ana",
    "Ben's Apple TV",
    "Needs seats turned on",
    "Gaming rig",
    "Delete preset",
    "This cannot be undone.",
    "Remove it",
    "Keep",
    "No answer",
];

fn is_data(t: &str) -> bool {
    DATA.contains(&t) || t.parse::<std::net::IpAddr>().is_ok()
}

fn is_key(t: &str) -> bool {
    const KEYS: &[&str] = &[
        "A", "B", "X", "Y", "L1", "R1", "L2", "R2", "LB", "RB", "LT", "RT", "Tab", "Esc", "Enter",
        "Alt", "Ctrl", "Shift", "Win", "Select", "Start", "STEAM",
    ];
    let function = |k: &str| {
        k.strip_prefix('F')
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    t.split(" + ").all(|k| KEYS.contains(&k) || function(k))
}

fn is_cap(t: &str) -> bool {
    let n = t.chars().count();
    n == 1 || (n == 2 && t.chars().all(char::is_uppercase)) || t == "#+=" || t == "ABC"
}

fn is_product(t: &str) -> bool {
    const PRODUCTS: &[&str] = &[
        "steam",
        "punktfunk",
        "xbox",
        "playstation",
        "heroic",
        "lutris",
        "v-sync",
    ];
    let (name, version) = t.split_once(' ').unwrap_or((t, ""));
    PRODUCTS.contains(&name.to_lowercase().as_str()) && (version.is_empty() || is_number(version))
}

/// The licence files the Licenses screen embeds, words in one run of single spaces.
fn legal() -> &'static str {
    static LEGAL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        let files = [
            include_str!("../../../../../../LICENSE-MIT"),
            include_str!("../../../../../../LICENSE-APACHE"),
            include_str!("../../../assets/fonts/Geist-OFL.txt"),
            "MIT License Apache License 2.0",
        ];
        files
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    });
    &LEGAL
}

fn is_legal(t: &str) -> bool {
    let words = t.split_whitespace().collect::<Vec<_>>().join(" ");
    !words.is_empty() && legal().contains(&words)
}

/// Letters `en-XA` accents, which no English word the scenes put in has.
const ACCENTED: &str = "áçďéğĥíĵķĺñöŕšťüŵýžÁÇĎÉĞĤÍĴĶĹÑÖŔŠŤÜŴÝŽ";

/// A message with a badge in the middle is drawn as its words either side of the badge.
fn is_fragment(t: &str) -> bool {
    t.contains('~') || t.chars().any(|c| ACCENTED.contains(c))
}

/// A message under `en-XA` is wrapped in brackets. A run cut short keeps one of them.
fn reads_pseudo(t: &str) -> bool {
    (t.starts_with('[') || t.starts_with('…')) && (t.ends_with(']') || t.ends_with('…'))
}

/// The part of `text` that is neither a message nor allowed, if there is one. A `·` or a `—`
/// between pieces is the layout's.
fn stray(text: &str) -> Option<&str> {
    if text.is_empty() || reads_pseudo(text) {
        return None;
    }
    text.split(" · ")
        .flat_map(|p| p.split(" — "))
        .find(|p| !reads_pseudo(p) && !LITERAL.iter().any(|(_, ok)| ok(p)))
}

// --- The overflow rules ---------------------------------------------------------------------

/// How a run fails as `(allotted, measured)` in px, or `None` when it fits.
fn overflow(r: &Run) -> Option<(f32, f32)> {
    let measured = r.x1 - r.x0;
    if measured <= 0.0 {
        return None;
    }
    match (r.box_w, r.clip) {
        // Given a width, a run is as wide as that or it is wrong. One that wraps or ends in
        // `…` has already stopped at the box.
        (Some(w), _) => (measured > w + SLACK).then_some((w, measured)),
        // Given none, the clip is the room: a run that starts inside it and ends past it.
        (None, Some((left, right))) if r.x0 >= left && r.x0 < right => {
            (r.x1 > right + SLACK).then_some((right - r.x0, measured))
        }
        _ => None,
    }
}

/// Two runs of one pass that sit on each other: a label grown into its value.
fn collides(a: &Run, b: &Run) -> bool {
    let across = a.x1.min(b.x1) - a.x0.max(b.x0);
    let down = a.y1.min(b.y1) - a.y0.max(b.y0);
    let line = (a.y1 - a.y0).min(b.y1 - b.y0);
    a.layer == b.layer && across > SLACK && down > line / 2.0 && a.text != b.text
}

/// One scene's problems: each problem run and what is wrong with it.
fn problems(runs: &[&Run]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, r) in runs.iter().enumerate() {
        if let Some((allotted, measured)) = overflow(r) {
            out.push((i, format!("{measured:.0} px in {allotted:.0}")));
        }
        if let Some(under) = runs[..i].iter().find(|o| collides(o, r)) {
            out.push((i, format!("on {:?}", under.text)));
        }
    }
    out
}

// --- Message keys, for the findings ---------------------------------------------------------

/// Every compiled message under `en-XA`, as `(key, literal pieces between its placeholders)`.
fn templates() -> Vec<(&'static str, Vec<String>)> {
    let parts = [
        include_str!("../../../../../../l10n/messages/common/en.json"),
        include_str!("../../../../../../l10n/messages/client/en.json"),
        include_str!("../../../../../../l10n/messages/console/en.json"),
        include_str!("../../../../../../l10n/messages/host/en.json"),
    ];
    let mut out = Vec::new();
    for part in parts {
        let file: serde_json::Value = serde_json::from_str(part).expect("a message file parses");
        for key in file
            .as_object()
            .expect("a message file is an object")
            .keys()
        {
            let Some(msg) = pf_l10n::get(key) else {
                continue;
            };
            // A plural reads `other` bare, so ask for the singular as well.
            for text in [msg.text().into_owned(), msg.format_count(1, &[])] {
                let mut pieces = vec![String::new()];
                let mut rest = text.as_str();
                while let Some(open) = rest.find('{') {
                    let Some(close) = rest[open..].find('}') else {
                        break;
                    };
                    pieces.last_mut().unwrap().push_str(&rest[..open]);
                    pieces.push(String::new());
                    rest = &rest[open + close + 1..];
                }
                pieces.last_mut().unwrap().push_str(rest);
                out.push((msg.key(), pieces.iter().map(|p| p.to_uppercase()).collect()));
            }
        }
    }
    out
}

/// The key whose message, filled in, reads as `text`: the one sharing the most letters with it.
/// `?` when none does, as for a line made of several messages.
fn key_of(text: &str) -> &'static str {
    let cut = text.ends_with('…');
    let text = text.to_uppercase();
    let mut best = ("?", 5);
    for (key, pieces) in &pf_l10n::with_language("en-XA", templates) {
        if let Some(score) = fit(pieces, &text, cut).filter(|s| *s > best.1) {
            best = (key, score);
        }
    }
    best.0
}

/// The bytes of `pieces` that read in `text`, in order, or `None` when they do not. A text cut
/// short may stop before the last piece.
fn fit(pieces: &[String], text: &str, cut: bool) -> Option<usize> {
    let (first, rest) = pieces.split_first()?;
    let body = text.strip_suffix('…').filter(|_| cut).unwrap_or(text);
    if cut && first.starts_with(body) {
        return Some(body.len());
    }
    if !text.starts_with(first.as_str()) {
        return None;
    }
    let (mut at, mut score) = (first.len(), first.len());
    for (i, piece) in rest.iter().enumerate() {
        let tail = &text[at..];
        if i + 1 == rest.len() && !cut {
            tail.ends_with(piece.as_str()).then_some(())?;
        } else if let Some(p) = tail.find(piece.as_str()) {
            at += p + piece.len();
        } else if cut {
            break;
        } else {
            return None;
        }
        score += piece.len();
    }
    Some(score)
}

// --- Drawing the screens --------------------------------------------------------------------

/// One run, where and at what size it was drawn.
struct Seen {
    size: &'static str,
    scene: String,
    run: Run,
}

struct Gate {
    fonts: crate::theme::Fonts,
    size: &'static str,
    viewport: Viewport,
    platform: Platform,
    surface: skia_safe::Surface,
    pads: Vec<PadInfo>,
    seen: Vec<Seen>,
}

impl Gate {
    fn new(size: &Size) -> Gate {
        let mut g = Gate {
            fonts: crate::theme::build_fonts().unwrap(),
            size: size.name,
            viewport: size.viewport,
            platform: size.platform,
            surface: skia_safe::surfaces::raster_n32_premul((1, 1)).unwrap(),
            pads: Vec::new(),
            seen: Vec::new(),
        };
        g.resize(size.viewport);
        g
    }

    fn resize(&mut self, viewport: Viewport) {
        self.viewport = viewport;
        let size = (viewport.width as i32, viewport.height as i32);
        self.surface = skia_safe::surfaces::raster_n32_premul(size).unwrap();
    }

    /// `s` on this size's platform and a fixed clock.
    fn adopt(&self, s: &mut Shell) {
        s.device.platform = self.platform;
        s.fake_clock = Some((0.0, STEP));
    }

    fn shell(&self, stack: Vec<Screen>) -> (Shell, ConsoleShared, LibraryShared) {
        let (mut s, console, library) = shell(stack);
        self.adopt(&mut s);
        (s, console, library)
    }

    /// Draws `s` for [`FRAMES`] frames and keeps every run of the last.
    fn look(&mut self, s: &mut Shell, scene: &str, pad: bool) {
        let t = s.fake_clock.map_or(0.0, |(t, _)| t);
        s.fake_clock = Some((t, STEP));
        let pref = pad.then_some(GamepadPref::Xbox360);
        let name = pad.then_some(PAD);
        for frame in 0..FRAMES {
            if frame + 1 == FRAMES {
                textprobe::begin();
            }
            let canvas = self.surface.canvas();
            s.render_in(canvas, &self.viewport, &self.fonts, name, pref, &self.pads);
        }
        for run in textprobe::end() {
            self.seen.push(Seen {
                size: self.size,
                scene: scene.to_owned(),
                run,
            });
        }
    }
}

fn game(i: usize, title: &str) -> crate::library::LibraryGame {
    crate::library::LibraryGame {
        id: format!("steam:{i}"),
        title: title.to_owned(),
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
    }
}

fn games() -> Vec<crate::library::LibraryGame> {
    let titles = [
        "Hades II",
        "Elden Ring",
        "Hollow Knight",
        "Celeste",
        "Tunic",
    ];
    titles.iter().enumerate().map(|(i, t)| game(i, t)).collect()
}

fn pad_info(name: &str, id: &str, pref: GamepadPref) -> PadInfo {
    PadInfo {
        name: name.into(),
        key: format!("{}:{name}", id.to_lowercase()),
        pref,
        steam_virtual: false,
        battery: Some(pf_client_core::menu_nav::PadBattery {
            percent: 80,
            charging: false,
        }),
        detail: format!("{id} · gamepad · dpad"),
        forwarded: true,
        rumble: true,
    }
}

fn press(s: &mut Shell, events: &[MenuEvent]) {
    for ev in events {
        s.handle_menu(*ev);
    }
}

const DOWN: MenuEvent = MenuEvent::Move(MenuDir::Down);
const UP: MenuEvent = MenuEvent::Move(MenuDir::Up);
const RIGHT: MenuEvent = MenuEvent::Move(MenuDir::Right);

/// The hosts tab: cards, their menus, the empty home, the takeovers.
fn home(g: &mut Gate) {
    let (mut s, console, library) = g.shell(vec![Screen::Home(HomeScreen::new())]);
    g.look(&mut s, "home", true);
    library.set_games(&library.req(), games());
    g.look(&mut s, "home: games", true);
    press(&mut s, &[DOWN]);
    g.look(&mut s, "home: games focus", true);
    press(&mut s, &[UP, MenuEvent::Secondary]);
    g.look(&mut s, "home: host options", true);
    press(&mut s, &[DOWN, DOWN, DOWN, MenuEvent::Confirm]);
    g.look(&mut s, "home: host details", true);
    press(&mut s, &[MenuEvent::Back, UP]);
    g.look(&mut s, "home: tab strip", true);

    let wake = |seconds, timed_out| WakeStatus {
        req: Default::default(),
        key: "bb22".into(),
        name: "Office Tower".into(),
        seconds,
        timed_out,
        online: false,
        then_connect: true,
    };
    console.set_wake(Some(wake(12, false)));
    g.look(&mut s, "waking", true);
    console.set_wake(Some(wake(90, true)));
    g.look(&mut s, "wake timed out", true);
    console.set_wake(None);
    s.set_connecting(Some("Elden Ring".into()));
    g.look(&mut s, "connecting", true);
    s.set_connecting(None);
    s.session_failed("Connection timed out");
    g.look(&mut s, "toast", true);

    use crate::model::{SpeedPhase, SpeedStatus};
    let mut speed = SpeedStatus::new("aa11".into(), "Living Room PC".into());
    for (scene, phase) in [
        ("speed: connecting", SpeedPhase::Connecting),
        ("speed: measuring", SpeedPhase::Progress { kbps: 62_000 }),
        ("speed: failed", SpeedPhase::Failed("No answer".into())),
        (
            "speed: done",
            SpeedPhase::Done {
                throughput_kbps: 142_000,
                wall: false,
                clean: None,
                recommended_kbps: 98_000,
                findings: Vec::new(),
            },
        ),
    ] {
        speed.phase = phase;
        console.set_speed(Some(speed.clone()));
        g.look(&mut s, scene, true);
    }
    console.set_speed(None);

    // The remote's legends: keys, no pad.
    let (mut s, _console, _library) = g.shell(vec![Screen::Home(HomeScreen::new())]);
    s.note_input_source(crate::console::InputSource::Keys);
    g.look(&mut s, "home: remote", false);

    let (mut s, console, _library) = g.shell(vec![Screen::Home(HomeScreen::new())]);
    console.set_hosts(Vec::new());
    g.look(&mut s, "home: no hosts", false);
}

/// Every Settings section, on a viewport tall enough to draw all its rows at once.
fn settings(g: &mut Gate) {
    let desk = g.viewport;
    let k = desk
        .scale
        .unwrap_or_else(|| (f64::from(desk.height) / 800.0).clamp(0.75, 3.0));
    // The longest section ends near 1200 dp, plus its footer.
    let insets = desk.insets.top + desk.insets.bottom;
    g.resize(Viewport {
        height: (1500.0 * k).round() as u32 + insets as u32,
        scale: Some(k),
        ..desk
    });
    let (mut s, _console, _library) = g.shell(vec![Screen::Home(HomeScreen::new())]);
    // Show advanced: every row a section has.
    s.settings.show_advanced = true;
    s.switch_tab(Tab::Settings);
    finish_motion(&mut s);
    for section in 0..crate::screens::settings::rows::TAB_COUNT {
        g.look(&mut s, &format!("settings: section {section}"), true);
        next_section(&mut s);
    }
    g.resize(desk);

    let background = crate::screens::palette::PaletteScreen::new("violet");
    let (mut s, _console, _library) = g.shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::Palette(background),
    ]);
    g.look(&mut s, "settings: background", true);
}

/// The forms: add host, its keyboard, pair, the profile picker.
fn forms(g: &mut Gate) {
    let (mut s, _console, _library) = g.shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::AddHost(crate::screens::add_host::AddHostScreen::new()),
    ]);
    g.look(&mut s, "add host", false);
    press(&mut s, &[MenuEvent::Confirm]);
    g.look(&mut s, "add host: keyboard", false);
    // A Steam Deck types on Steam's own keyboard; its legend names the chord.
    s.device.system_keyboard = true;
    g.look(&mut s, "add host: system keyboard", false);

    let steambox = hosts()[2].clone();
    let (mut s, _console, _library) = g.shell(vec![
        Screen::Home(HomeScreen::new()),
        Screen::Pair(crate::screens::pair::PairScreen::new(&steambox, "deck")),
    ]);
    g.look(&mut s, "pair", true);
    press(&mut s, &[MenuEvent::Confirm]);
    g.look(&mut s, "pair: keyboard", true);
}

/// Who plays: the saved pick on the card, the picker, its switch.
fn profiles(g: &mut Gate) {
    use crate::model::ProfilesAnswer;
    use pf_client_core::profiles::{ListedProfile, ProfilePick, Seat, SeatState};
    fake_home();
    let console = ConsoleShared::default();
    let mut rows = hosts();
    rows[0].profile = Some(ProfilePick {
        id: "theo".into(),
        display_name: "Theo".into(),
        remember: true,
        ..Default::default()
    });
    console.set_hosts(rows.clone());
    let mut opts = test_options();
    opts.profiles = true;
    let stack = vec![Screen::Home(HomeScreen::new())];
    let library = LibraryShared::default();
    let mut s = Shell::new(console.clone(), library, ConsoleBus::default(), opts, stack).unwrap();
    g.adopt(&mut s);
    g.look(&mut s, "profiles: card", true);
    let seat = |state, detail: Option<&str>, occupant: Option<&str>, steam| {
        Some(Seat {
            state,
            detail: detail.map(Into::into),
            occupant: occupant.map(Into::into),
            steam_sign_in: steam,
            ..Default::default()
        })
    };
    let p = |id: &str, name: &str, seat| ListedProfile {
        id: id.into(),
        display_name: name.into(),
        seat,
        ..Default::default()
    };
    let listed = vec![
        p("own", "Ben", None),
        p(
            "mia",
            "Mia Rossi",
            seat(SeatState::Ready, None, None, Some(true)),
        ),
        p(
            "kid",
            "Kid",
            seat(SeatState::Occupied, None, Some("Ben's Apple TV"), None),
        ),
        p(
            "guest",
            "Guest",
            seat(SeatState::Starting, None, None, None),
        ),
        p(
            "ana",
            "Ana",
            seat(
                SeatState::Unavailable,
                Some("Needs seats turned on"),
                None,
                None,
            ),
        ),
    ];
    let answer = ProfilesAnswer::Listed(listed.clone());
    s.start_connect(crate::screens::ConnectIntent::to_host(&rows[0], None));
    console.set_profiles("aa11", ProfilesAnswer::Listed(listed));
    g.look(&mut s, "profiles: picker", true);
    press(&mut s, &[RIGHT, RIGHT, DOWN, DOWN, MenuEvent::Confirm]);
    g.look(&mut s, "profiles: remember", true);
    press(&mut s, &[MenuEvent::Back]);
    finish_motion(&mut s);
    s.profiles.learn("aa11", &answer);
    press(&mut s, &[DOWN, RIGHT, RIGHT]);
    g.look(&mut s, "profiles: card verb", true);
    press(&mut s, &[MenuEvent::Confirm]);
    g.look(&mut s, "profiles: switch", true);
}

/// The Games tab: the shelf, the launch hold, the sections, Customize, collections.
fn library(g: &mut Gate) {
    let host = hosts()[0].clone();
    let shelf = |g: &Gate| {
        let (s, _console, library) = g.shell(vec![
            Screen::Home(HomeScreen::new()),
            Screen::Library(LibraryScreen::new(&host)),
        ]);
        (s, library)
    };
    let (mut s, library) = shelf(g);
    g.look(&mut s, "library: loading", true);
    library.set_games(&library.req(), Vec::new());
    g.look(&mut s, "library: empty", true);

    let (mut s, library) = shelf(g);
    let mut list = games();
    list[0].running = true;
    list[1].stats = Some(pf_client_core::library::GameStats {
        last_played_unix_ms: 1_000,
        ..Default::default()
    });
    let mut launcher = game(9, "Steam");
    launcher.launcher = true;
    launcher.icon = "steam".into();
    list.insert(0, launcher);
    library.set_games(&library.req(), list);
    g.look(&mut s, "library", true);
    press(&mut s, &[DOWN]);
    g.look(&mut s, "library: sections", true);
    press(&mut s, &[UP, UP, RIGHT, MenuEvent::Confirm]);
    finish_motion(&mut s);
    g.look(&mut s, "library: customize", true);

    let (mut s, library) = shelf(g);
    library.set_games(&library.req(), games());
    g.look(&mut s, "library: settle", true);
    press(&mut s, &[RIGHT, MenuEvent::Confirm]);
    g.look(&mut s, "library: launch hold", true);
    s.session_streaming();
    g.look(&mut s, "library: streaming", true);

    let (mut s, library) = shelf(g);
    let platforms = ["PlayStation 3", "Xbox 360", "SNES", "Mega Drive", "SNES"];
    let roms = games()
        .into_iter()
        .zip(platforms)
        .map(|(game, platform)| crate::library::LibraryGame {
            platform: Some(platform.into()),
            store: "rom-manager".into(),
            ..game
        })
        .collect();
    library.set_games(&library.req(), roms);
    g.look(&mut s, "library: collections", true);
    press(&mut s, &[MenuEvent::Secondary]);
    g.look(&mut s, "library: collections open", true);
}

/// Screens the dump does not reach, pushed straight onto the stack.
fn pushed(g: &mut Gate) {
    g.pads = vec![
        pad_info(PAD, "045E:0B13", GamepadPref::Xbox360),
        pad_info(
            "DualSense Wireless Controller",
            "054C:0CE6",
            GamepadPref::DualSense,
        ),
    ];
    let (mut s, _console, _library) = g.shell(vec![Screen::Home(HomeScreen::new())]);
    s.switch_tab(Tab::Players);
    finish_motion(&mut s);
    g.look(&mut s, "players", true);
    let pad = g.pads[0].clone();

    let host = hosts()[0].clone();
    let mut settings = pf_client_core::trust::Settings::default();
    let library = LibraryShared::default();
    let ctx = crate::screens::Ctx::test(&mut settings, &library);
    let mut fx = crate::screens::Outbox::default();
    let screens: Vec<(&str, Screen)> = vec![
        (
            "grants",
            Screen::Grants(crate::screens::grants::GrantsScreen::new()),
        ),
        (
            "pad menu",
            Screen::PadMenu(crate::screens::pad_menu::PadMenuScreen::new(
                &pad, true, true,
            )),
        ),
        (
            "input test",
            Screen::InputTest(crate::screens::input_test::InputTestScreen::new()),
        ),
        (
            "controls",
            Screen::Controls(crate::screens::controls::ControlsScreen::new(&ctx)),
        ),
        (
            "licenses",
            Screen::Licenses(crate::screens::licenses::LicensesScreen::new(&mut fx)),
        ),
        (
            "pin hosts",
            Screen::PinHosts(crate::screens::pin_hosts::PinHostsScreen::new(
                "p1".into(),
                "Gaming rig".into(),
            )),
        ),
        (
            "bind preset",
            Screen::BindPreset(crate::screens::bind_preset::BindPresetScreen::new(
                "aa11".into(),
                "Living Room PC".into(),
                vec![("p1".into(), "Gaming rig".into())],
            )),
        ),
        (
            "card menu: game",
            Screen::CardMenu(crate::screens::card_menu::CardMenu::for_game(
                &host,
                &game(0, "Hades II"),
                None,
            )),
        ),
        (
            "search",
            Screen::Search(crate::screens::search::SearchScreen::new(
                &host,
                &Default::default(),
            )),
        ),
        (
            "card menu: connect with",
            Screen::CardMenu(crate::screens::card_menu::CardMenu::connect_with(&host)),
        ),
        (
            "ring editor",
            Screen::RingEditor(Box::new(
                crate::screens::ring_editor::RingEditorScreen::new(&ctx),
            )),
        ),
        (
            "shortcut editor",
            Screen::ShortcutEditor(crate::screens::shortcut_editor::ShortcutEditorScreen::new(
                &ctx, None,
            )),
        ),
        (
            "prompt",
            Screen::Prompt(crate::screens::prompt::PromptScreen::new(crate::Prompt {
                id: "q".into(),
                title: "Delete preset".into(),
                message: "This cannot be undone.".into(),
                choices: vec!["Remove it".into(), "Keep".into()],
            })),
        ),
    ];
    for (name, screen) in screens {
        let (mut s, _console, _library) = g.shell(vec![Screen::Home(HomeScreen::new()), screen]);
        g.look(&mut s, name, true);
    }
    g.pads.clear();
}

/// Everything the gate walks, at the size `g` draws.
fn walk(g: &mut Gate) {
    home(g);
    settings(g);
    forms(g);
    profiles(g);
    library(g);
    pushed(g);
}

/// One scene's runs at one size, in the order they were drawn.
fn by_scene(all: &[Seen]) -> BTreeMap<(&str, &str), Vec<&Run>> {
    let mut scenes: BTreeMap<(&str, &str), Vec<&Run>> = BTreeMap::new();
    for s in all {
        scenes.entry((s.size, &s.scene)).or_default().push(&s.run);
    }
    scenes
}

#[test]
fn every_screen_reads_the_pseudo_language() {
    textprobe::set_bare(true);
    let seen = pf_l10n::with_language("en-XA", || {
        let mut seen = Vec::new();
        for size in sizes() {
            let mut g = Gate::new(&size);
            walk(&mut g);
            seen.append(&mut g.seen);
        }
        seen
    });
    textprobe::set_bare(false);
    let seen_scenes = by_scene(&seen);

    // Findings by what they say: the same text on many screens is one finding.
    let mut literal: BTreeMap<&str, String> = BTreeMap::new();
    let mut over: BTreeMap<(&str, String), String> = BTreeMap::new();
    for (scene, runs) in &seen_scenes {
        let at = format!("{} / {}", scene.0, scene.1);
        for r in runs {
            if let Some(text) = stray(&r.text) {
                literal.entry(text).or_insert_with(|| at.clone());
            }
        }
        for (i, what) in problems(runs) {
            let key = key_of(&runs[i].text);
            over.entry((key, what))
                .or_insert_with(|| format!("{at}: {:?}", runs[i].text));
        }
    }

    let mut report = String::new();
    for (text, at) in &literal {
        if !LITERAL_DEBT.iter().any(|(t, _)| t == text) {
            report += &format!("not a message: {text:?} at {at}\n");
        }
    }
    for ((key, what), at) in &over {
        if !OVERFLOW_DEBT.iter().any(|(k, _)| k == key) {
            report += &format!("overflow: {key} {what} at {at}\n");
        }
    }
    for (text, _) in LITERAL_DEBT {
        if !literal.contains_key(text) {
            report += &format!("LITERAL_DEBT: {text:?} no longer draws; remove it\n");
        }
    }
    for (key, _) in OVERFLOW_DEBT {
        if !over.keys().any(|(k, _)| k == key) {
            report += &format!("OVERFLOW_DEBT: {key} fits now; remove it\n");
        }
    }
    assert!(report.is_empty(), "\n{report}");
}

/// The probe sees what the kit draws, and the rules call a run that spills out of its room.
#[test]
fn the_gate_sees_a_run_that_does_not_fit() {
    use crate::theme::W;
    let fonts = crate::theme::build_fonts().unwrap();
    let mut surface = skia_safe::surfaces::raster_n32_premul((200, 120)).unwrap();
    let ink = skia_safe::Color4f::new(1.0, 1.0, 1.0, 1.0);
    let c = surface.canvas();
    let draw = |text, x, y| fonts.draw(c, text, x, y, W::Regular, 16.0, ink);
    textprobe::begin();
    draw("A label too long for the edge", 120.0, 20.0);
    fonts.draw_clipped(
        c,
        "A title too long for its box",
        10.0,
        45.0,
        W::Regular,
        16.0,
        ink,
        80.0,
    );
    draw("Label", 10.0, 70.0);
    draw("Value", 40.0, 70.0);
    // Wrapped into a box, the paragraph runs down onto the line under it.
    fonts.leading(
        c,
        "A caption that wraps",
        W::Regular,
        16.0,
        ink,
        120.0,
        40.0,
        60.0,
    );
    draw("Below", 130.0, 100.0);
    let runs = textprobe::end();
    assert!(overflow(&runs[0]).is_some(), "past the edge: {:?}", runs[0]);
    assert!(
        overflow(&runs[1]).is_none(),
        "cut to its box: {:?}",
        runs[1]
    );
    assert!(runs[1].text.ends_with('…'), "{:?}", runs[1]);
    assert!(collides(&runs[2], &runs[3]), "{:?} {:?}", runs[2], runs[3]);
    assert!(collides(&runs[4], &runs[5]), "{:?} {:?}", runs[4], runs[5]);
    assert!(!collides(&runs[0], &runs[1]));
}
