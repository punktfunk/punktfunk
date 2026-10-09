//! Injected-key TUI frames, pinned as goldens.
//!
//! The screen a user would see is a value a test can assert on. The PTY smoke
//! only has to prove a real terminal agrees.
//!
//! Rendered with `Colors::None` so the goldens are readable text. Escape
//! sequences have their own tests in `ui::theme`. Goldens live under
//! `tests/golden/tui-*.txt`; regenerate with `UPDATE_GOLDEN=1`.

mod common;

use common::golden;
use punktfunk_setup::choices::{Choices, Pins};
use punktfunk_setup::demo;
use punktfunk_setup::report;
use punktfunk_setup::ui::logo::MARK_TEXT_ROWS;
use punktfunk_setup::ui::summary::{Screen, Step};
use punktfunk_setup::ui::term::{Key, ScriptedTerm, Terminal};
use punktfunk_setup::ui::theme::{Caps, Colors};
use punktfunk_setup::ui::tui::Tui;
use punktfunk_setup::ui::Reporter;

fn caps() -> Caps {
    Caps {
        tty: true,
        colors: Colors::None,
        width: 100,
    }
}

fn drive(preset: &str, keys: &[Key]) -> (String, Step, Screen) {
    let (frame, step, screen, _) = drive_all(preset, keys);
    (frame, step, screen)
}

/// Every frame written, including ones the loop later cleared (a backed-out row editor).
fn drive_all(preset: &str, keys: &[Key]) -> (String, Step, Screen, Vec<String>) {
    let facts = demo::preset(preset).expect("preset");
    let choices = Choices::derive(&facts, &Pins::default());
    let mut screen = Screen::new(facts, choices);
    let mut term = ScriptedTerm::new(keys);
    let step = {
        let tui = Tui::new(&mut term as &mut dyn Terminal, caps(), 0);
        tui.settings(&mut screen, 0)
    };
    (term.screen().to_string(), step, screen, term.frames.clone())
}

#[test]
fn the_settings_screen_as_the_user_first_sees_it() {
    let (frame, step, _) = drive("arch-fresh", &[Key::Enter]);
    golden("tui-arch-fresh", &frame);
    assert!(matches!(step, Step::Run(_)), "Enter on arrival installs");
}

#[test]
fn a_couch_box_shows_its_derived_defaults() {
    let (frame, _, _) = drive("bazzite-couch", &[Key::Enter]);
    golden("tui-bazzite-couch", &frame);
}

/// Omarchy's own options are rows on this screen, not a second round of questions from
/// `punktfunk-omarchy setup` after this one has finished. The console certificate is not a
/// row: every host install trusts it, so there is nothing to ask.
#[test]
fn an_omarchy_box_asks_for_its_own_options_here() {
    let (frame, _, _) = drive("omarchy", &[Key::Enter]);
    golden("tui-omarchy", &frame);
    assert!(
        !frame.contains("Console certificate"),
        "the certificate became a question again"
    );
    for row in [
        "Desktop notifications",
        "Keep the screen awake",
        "Match the Omarchy theme",
    ] {
        assert!(frame.contains(row), "{row} is missing from the screen");
    }
}

/// Manage mode: the screen re-titles and grows an Uninstall row.
#[test]
fn an_installed_box_shows_the_manage_screen() {
    let (frame, _, _) = drive("arch-canary-installed", &[Key::Enter]);
    golden("tui-manage", &frame);
    assert!(frame.contains("Uninstall"));
    assert!(frame.contains("Apply these changes"));
}

#[test]
fn the_cursor_lands_on_the_row_it_was_moved_to() {
    let (frame, step, screen) = drive("arch-fresh", &[Key::Down, Key::Down]);
    golden("tui-cursor-on-channel", &frame);
    assert_eq!(screen.cursor, 2);
    assert_eq!(step, Step::Cancel);
}

#[test]
fn editing_moonlight_compat_adds_the_firewall_step() {
    // Down×4: Moonlight compat. Enter opens, Up picks yes, Enter accepts.
    // Cursor stays on that row; walk back up to the action and install.
    let keys = [
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
        Key::Up,
        Key::Enter,
        Key::Up,
        Key::Up,
        Key::Up,
        Key::Up,
        Key::Enter,
    ];
    let (_, step, screen) = drive("omarchy", &keys);
    assert!(screen.choices.gamestream, "the edit did not stick");
    assert!(
        screen
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("punktfunk-gamestream")),
        "the plan was not re-resolved: {:?}",
        screen.plan().commands()
    );
    assert!(
        matches!(step, Step::Run(_)),
        "the walk back to the action did not install: {step:?}"
    );
}

/// The editor must name the grant; accepting a default must not hide it.
#[test]
fn the_row_editor_frame_names_the_grant() {
    let keys = [Key::Down, Key::Down, Key::Down, Key::Enter];
    let (_, _, _, frames) = drive_all("arch-fresh", &keys);
    let editor = frames
        .iter()
        .rev()
        .find(|f| f.contains('●'))
        .expect("no radio list was ever drawn");
    golden("tui-editor-group", editor);
    assert!(
        editor.contains("usbip attach"),
        "the editor hid what the row grants"
    );
}

/// The mark is in the settings frame. Picking Client greys the host circle;
/// a separate banner would disagree with the row below it.
#[test]
fn the_mark_mutes_the_half_that_is_not_being_installed() {
    let colour = Caps {
        tty: true,
        colors: Colors::Truecolor,
        width: 100,
    };
    let render = |host: bool, client: bool| {
        let facts = demo::preset("arch-fresh").expect("preset");
        let mut choices = Choices::derive(&facts, &Pins::default());
        choices.components.host = host;
        choices.components.client = client;
        let mut screen = Screen::new(facts, choices);
        let mut term = ScriptedTerm::new(&[Key::Enter]);
        {
            let tui = Tui::new(&mut term as &mut dyn Terminal, colour, 0);
            tui.settings(&mut screen, 0);
        }
        term.screen().to_string()
    };

    let host_only = render(true, false);
    let both = render(true, true);
    let client_only = render(false, true);

    assert!(
        host_only.contains("\x1b[48;2;"),
        "the frame carries no mark at all"
    );
    assert_ne!(
        host_only, both,
        "selecting the client changed nothing in the mark"
    );
    assert_ne!(client_only, both);
    assert_ne!(host_only, client_only);

    // The mark paints backgrounds. The same colour is a highlight foreground
    // elsewhere, so only `\x1b[48;2` means the lens.
    let mark_of = |frame: &str| {
        frame
            .lines()
            .take(MARK_TEXT_ROWS)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let lens = "\x1b[48;2;210;201;251m";
    assert!(
        mark_of(&both).contains(lens),
        "both installed should light the lens"
    );
    assert!(
        !mark_of(&host_only).contains(lens),
        "a host-only install must not light the lens"
    );
}

fn columns(line: &str) -> usize {
    let mut cols = 0;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            cols += 1;
        }
    }
    cols
}

/// A wrapped line makes `clear_last_lines` rewind fewer rows than the frame
/// drew, so every keystroke would leave a stripe behind.
#[test]
fn no_frame_line_is_wider_than_the_terminal() {
    for width in [30u16, 40, 60, 80, 120] {
        let caps = Caps {
            tty: true,
            colors: Colors::Truecolor,
            width,
        };
        let facts = demo::preset("bazzite-couch").expect("preset");
        let choices = Choices::derive(&facts, &Pins::default());
        let mut screen = Screen::new(facts, choices);
        let mut term = ScriptedTerm::new(&[Key::Enter]);
        term.width = width;
        {
            let tui = Tui::new(&mut term as &mut dyn Terminal, caps, 0);
            tui.settings(&mut screen, 0);
        }
        for line in term.screen().lines() {
            assert!(
                columns(line) <= usize::from(width),
                "at width {width} a line ran to {} columns: {line:?}",
                columns(line)
            );
        }
    }
}

/// The row already shows the answer. A second edit must not add a line.
#[test]
fn repeated_edits_do_not_pile_up_lines() {
    let once = drive(
        "arch-fresh",
        &[Key::Down, Key::Enter, Key::Enter, Key::Char('q')],
    )
    .0;
    let twice = drive(
        "arch-fresh",
        &[
            Key::Down,
            Key::Enter,
            Key::Enter,
            Key::Enter,
            Key::Enter,
            Key::Char('q'),
        ],
    )
    .0;
    assert_eq!(
        once.lines().count(),
        twice.lines().count(),
        "a second edit left an extra line on screen"
    );
}

#[test]
fn q_cancels_without_running_anything() {
    let (_, step, _) = drive("arch-fresh", &[Key::Char('q')]);
    assert_eq!(step, Step::Cancel);
}

/// An exhausted key script must end the loop, not spin it.
#[test]
fn a_terminal_that_stops_answering_ends_the_screen() {
    let (_, step, _) = drive("arch-fresh", &[]);
    assert_eq!(step, Step::Cancel);
}

#[test]
fn every_preset_renders_a_screen_without_panicking() {
    for name in demo::PRESETS {
        let (frame, _, _) = drive(name, &[Key::Enter]);
        assert!(!frame.is_empty(), "{name} rendered nothing");
    }
}

/// The default run is one repainting line. Nothing is echoed while it is up — which is what
/// makes the repaint legal — but a warning still has to reach the scrollback.
#[test]
fn the_run_collapses_to_a_progress_line() {
    let mut term = ScriptedTerm::new(&[]);
    {
        let tui = Tui::new(&mut term as &mut dyn Terminal, caps(), 0);
        tui.begin_progress(3);
        tui.say("Repositories");
        tui.plus("sudo pacman -Syu punktfunk-host");
        tui.ok("installed");
        tui.say("Firewall");
        tui.warn("no active firewall found");
        tui.end_progress();
    }
    let all = term.frames.join("");
    assert!(!all.contains("pacman"), "a command was echoed: {all}");
    assert!(!all.contains("installed"), "an ok line was echoed: {all}");
    assert!(all.contains("1/3"), "no counter: {all}");
    assert!(all.contains("2/3  Firewall"), "phase not named: {all}");
    assert!(
        all.contains("no active firewall found"),
        "a warning was swallowed: {all}"
    );
}

/// `-v` never calls `begin_progress`, so the transcript is exactly what it was.
#[test]
fn verbose_keeps_the_command_transcript() {
    let mut term = ScriptedTerm::new(&[]);
    {
        let tui = Tui::new(&mut term as &mut dyn Terminal, caps(), 0);
        tui.say("Repositories");
        tui.plus("sudo pacman -Syu punktfunk-host");
        tui.ok("installed");
    }
    let all = term.frames.join("");
    assert!(all.contains("sudo pacman -Syu punktfunk-host"), "{all}");
    assert!(all.contains("installed"), "{all}");
}

/// The step a fresh host install ends on. Enter takes the generated password, and the frame
/// it was taken from names the command that prints it.
fn ask_password(keys: &[Key]) -> (Option<String>, Vec<String>) {
    let mut term = ScriptedTerm::new(keys);
    let answer = {
        let tui = Tui::new(&mut term as &mut dyn Terminal, caps(), 0);
        tui.web_password("https://192.168.1.10:47992", report::PASSWORD_READ)
    };
    (answer, term.frames.clone())
}

#[test]
fn the_password_step_offers_the_generated_one_and_says_how_to_print_it() {
    let (answer, frames) = ask_password(&[Key::Enter]);
    assert_eq!(
        answer, None,
        "Enter on arrival keeps the generated password"
    );
    golden("tui-web-password", &frames[0]);
    assert!(frames[0].contains(report::PASSWORD_READ));
}

#[test]
fn typing_a_password_returns_it() {
    let mut keys = vec![Key::Down, Key::Enter];
    keys.extend("s3cret-pw".chars().map(Key::Char));
    keys.push(Key::Enter);
    let (answer, _) = ask_password(&keys);
    assert_eq!(answer.as_deref(), Some("s3cret-pw"));
}

/// Two things the file cannot carry: too little to be a password, and a character systemd
/// unquotes back out of the value. Neither may reach it.
#[test]
fn a_short_or_unquotable_password_is_refused_at_the_prompt() {
    let mut keys = vec![Key::Down, Key::Enter];
    keys.extend("ab\"cd".chars().map(Key::Char));
    keys.push(Key::Enter);
    keys.extend("efgh".chars().map(Key::Char));
    keys.push(Key::Enter);
    let (answer, _) = ask_password(&keys);
    assert_eq!(
        answer.as_deref(),
        Some("abcdefgh"),
        "the quote landed in the value, or Enter accepted four characters"
    );
}

/// Backing out of either prompt is "generate one", never a cancelled install.
#[test]
fn escaping_the_prompt_falls_back_to_the_generated_password() {
    assert_eq!(ask_password(&[Key::Cancel]).0, None);
    assert_eq!(ask_password(&[Key::Down, Key::Enter, Key::Cancel]).0, None);
}
