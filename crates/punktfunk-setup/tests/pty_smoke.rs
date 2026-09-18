//! The TUI in a real pseudo-terminal.
//!
//! `tui_frames` pins what the renderer builds. This drives the binary under a
//! pty — raw mode, cursor control, key decoding, the intro repaint — which no
//! in-process test can reach.
//!
//! Always `--demo`: the plan gets a runner that cannot spawn. Unix only.
//! ConPTY re-encodes the stream and the reads hang. Windows uses the WinUI
//! wizard; the silent path is a separate smoke.
#![cfg(unix)]

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// 45 s: intro animation plus ~12 steps on a loaded CI box.
const DEADLINE: Duration = Duration::from_secs(45);

fn run(args: &[&str], keys: &[u8], until: &str) -> String {
    run_with_home(args, keys, until, None)
}

fn run_with_home(args: &[&str], keys: &[u8], until: &str, home: Option<&str>) -> String {
    let pty = native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_punktfunk-setup"));
    for arg in args {
        cmd.arg(arg);
    }
    // A terminal that claims truecolor, so the mark and the rail are not degraded away.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env_remove("NO_COLOR");
    if let Some(home) = home {
        cmd.env("HOME", home);
        cmd.env_remove("XDG_CONFIG_HOME");
    }
    let mut child = pty.slave.spawn_command(cmd).expect("spawn");
    drop(pty.slave);

    let mut reader = pty.master.try_clone_reader().expect("reader");
    let mut writer = pty.master.take_writer().expect("writer");
    let collected = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = collected.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            sink.lock()
                .unwrap()
                .push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    });

    // The settings screen must be drawn before a keystroke means anything.
    let start = Instant::now();
    while start.elapsed() < DEADLINE {
        if collected.lock().unwrap().contains("Install now") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = writer.write_all(keys);
    let _ = writer.flush();

    while start.elapsed() < DEADLINE {
        if collected.lock().unwrap().contains(until) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.wait();
    let text = collected.lock().unwrap().clone();
    drop(writer);
    text
}

/// Strip CSI so an assertion reads the text, not the paint.
fn plain(raw: &str) -> String {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // CSI, plus the two-character sequences the renderer emits.
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    out
}

#[test]
fn the_demo_walks_from_the_settings_screen_to_the_outro() {
    let text = plain(&run(
        &["--demo", "debian-fresh", "-v"],
        b"\r\r\r",
        "Done. Next",
    ));
    assert!(
        text.contains("Install now with these settings"),
        "no settings screen:\n{text}"
    );
    assert!(
        text.contains("Full controller"),
        "the rows did not render:\n{text}"
    );
    // Under `-v` the commands still echo. That transparency is the trust feature, and it is
    // what the flag exists to preserve now that a run collapses to a progress line by default.
    assert!(
        text.contains("+ sudo apt install -y punktfunk-host punktfunk-web punktfunk-scripting"),
        "the command echo is missing:\n{text}"
    );
    // The password step stands between the settings screen and the install, which is the
    // point of it: the console asks for one, so nobody may walk past the line that prints it.
    assert!(
        text.contains("asks for a password"),
        "the password step never showed:\n{text}"
    );
    assert!(
        text.contains("PUNKTFUNK_UI_PASSWORD"),
        "the step did not say how to print the password:\n{text}"
    );
    // And the bind question right after it. Taking its default leaves the console on the local
    // network, where devices pair from; asking is how an operator keeps it to this machine.
    assert!(
        text.contains("Where should the web console be reachable?"),
        "the bind step never showed:\n{text}"
    );
    assert!(
        text.contains("PUNKTFUNK_UI_BIND=0.0.0.0"),
        "the default answer did not reach host.env:\n{text}"
    );
    assert!(
        text.contains("Done. Next"),
        "never reached the outro:\n{text}"
    );
}

#[test]
fn a_failed_step_renders_the_failure_and_points_at_the_docs() {
    let text = plain(&run(
        &["--demo", "debian-fresh", "--fail", "installing"],
        b"\r\r\r",
        "that step failed",
    ));
    assert!(
        text.contains("that step failed"),
        "no failure text:\n{text}"
    );
    assert!(
        text.contains("docs.punktfunk.unom.io/docs/debian"),
        "no docs pointer:\n{text}"
    );
    assert!(
        !text.contains("Done. Next"),
        "a failed run printed the success outro:\n{text}"
    );
}

/// `SetEnv` writes `host.env` with `std::fs`, not a spawn, so the fake runner never sees it.
/// `fedora-sunshine` is the preset that sets two of those keys.
#[test]
fn the_demo_writes_nothing_into_the_users_home() {
    let home = tempfile::tempdir().expect("tempdir");
    let text = plain(&run_with_home(
        &["--demo", "fedora-sunshine", "-v"],
        b"\r\r\r",
        "Done. Next",
        home.path().to_str(),
    ));
    assert!(
        text.contains("PUNKTFUNK_MGMT_BIND"),
        "the preset did not reach a SetEnv step"
    );
    let touched = home.path().join(".config/punktfunk");
    assert!(
        !touched.exists(),
        "demo mode created {} — it must reach the filesystem only through BasePaths",
        touched.display()
    );
}

#[test]
fn quitting_the_settings_screen_changes_nothing() {
    let text = plain(&run(&["--demo", "arch-fresh"], b"q", "Nothing was changed"));
    assert!(
        text.contains("Nothing was changed"),
        "no cancel outro:\n{text}"
    );
    assert!(
        !text.contains("+ sudo pacman"),
        "a cancelled run echoed a command:\n{text}"
    );
}

/// The default run shows progress, not a transcript: luxus counted the lines on Omarchy and
/// the wall of them is what hid the one warning that mattered.
#[test]
fn the_default_run_collapses_to_a_progress_line() {
    let text = plain(&run(&["--demo", "debian-fresh"], b"\r\r\r", "Done. Next"));
    assert!(
        !text.contains("+ sudo apt install"),
        "the command echo should be behind -v:\n{text}"
    );
    assert!(
        text.contains("Done. Next"),
        "never reached the outro:\n{text}"
    );
}

/// A hand-off ends the plan early. Its own summary goes into the progress view's capture buffer,
/// so this outro is the only thing that tells the user the install worked — or did not.
#[test]
fn a_hand_off_still_reports_the_outcome() {
    let text = plain(&run(&["--demo", "omarchy", "-v"], b"\r\r\r", "Done. Next"));
    assert!(
        text.contains("punktfunk-omarchy setup"),
        "the hand-off never ran:\n{text}"
    );
    assert!(
        text.contains("Done. Next"),
        "the hand-off swallowed the outro:\n{text}"
    );
}

/// The typed password all the way through: prompt → `Choices` → the plan step → the file the
/// console's unit reads. `--demo` writes into its own sandbox root, which the transcript names.
#[test]
fn a_typed_password_lands_in_the_file_the_console_reads() {
    // Enter installs, ↓ Enter picks "Set my own now", the password and Enter, then Enter
    // again to take the bind question's default.
    let text = plain(&run(
        &["--demo", "debian-fresh", "-v"],
        b"\r\x1b[B\rhunter2-and-then-some\r\r",
        "Done. Next",
    ));
    let line = text
        .lines()
        .find(|l| l.contains("your password →"))
        .unwrap_or_else(|| panic!("the password step never wrote anything:\n{text}"));
    let path = line.rsplit(' ').next().expect("a path on the line");
    assert_eq!(
        std::fs::read_to_string(path).expect("the password file"),
        "PUNKTFUNK_UI_PASSWORD=hunter2-and-then-some\n"
    );
    assert!(
        text.contains("Password: the one you typed"),
        "the outro forgot which password is in force:\n{text}"
    );
    let _ = std::fs::remove_dir_all(std::path::Path::new(path).ancestors().nth(2).expect("root"));
}
