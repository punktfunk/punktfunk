//! Runs the REAL `punktfunk-session` binary and asserts it speaks the stdout contract.
//!
//! A build gate cannot tell which program sits behind the binary's name: any program can
//! compile cleanly under it. Only running it shows what it does, and whatever it does, it
//! must SAY so on stdout.
#![cfg(any(target_os = "linux", windows))]

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A refused loopback connect emits one `{"error"…}` line on stdout. Presenter
/// initialization may fail first on a headless runner, but either path satisfies
/// the same contract before the deadline.
#[test]
fn a_failing_connect_still_speaks_the_contract() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_punktfunk-session"))
        .args([
            "--connect",
            "127.0.0.1:1",
            "--fp",
            &"0".repeat(64),
            "--connect-timeout",
            "5",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn punktfunk-session");

    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf);
        buf
    });

    // Generous bound: presenter init on a cold CI runner can be slow, but a healthy
    // binary answers in seconds. Only a hang (or a stub that inherited our stdout and
    // blocked) gets anywhere near it.
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match child.try_wait().expect("wait on punktfunk-session") {
            Some(_) => break,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("punktfunk-session neither exited nor spoke within 120 s");
            }
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }

    let out = reader.join().expect("stdout reader");
    let spoke = out.lines().map(str::trim).any(|l| {
        l.starts_with('{')
            && (l.contains("\"error\"") || l.contains("\"ready\"") || l.contains("\"ended\""))
    });
    assert!(
        spoke,
        "no stdout-contract line — the wrong program may be wearing this binary's name. \
         stdout was:\n{out}"
    );
}
