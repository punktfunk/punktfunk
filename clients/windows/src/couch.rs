//! The couch/HTPC hand-off: run the session binary's `--browse` mode (the complete
//! controller-driven client — host list, discovery, PIN pairing, settings, Wake-on-LAN,
//! library) and mirror its exit code.
//!
//! Shared by BOTH couch entry points — `punktfunk-console.exe`, which needs its own
//! executable because an MSIX `<Application>` cannot pass arguments, and this shell's
//! `--console` flag — so the spawn flags below are stated once.

use pf_client_core::orchestrate::session_binary;
use std::process::{Command, Stdio};

/// Run `punktfunk-session --browse` (fullscreen unless `--windowed`) and exit with the
/// child's code, so whatever supervises this process sees the real result. Never returns.
pub(crate) fn run_browse() -> ! {
    use std::os::windows::process::CommandExt as _;
    // `punktfunk-session` keeps the CONSOLE subsystem for its stdout contract, and both couch
    // entry points are GUI processes with no console to lend it — so without this flag Windows
    // mints one, and the couch UI comes up in front of a black terminal window.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let mut cmd = Command::new(session_binary());
    cmd.arg("--browse");
    // A couch UI is fullscreen unless explicitly told otherwise.
    if !std::env::args().any(|a| a == "--windowed") {
        cmd.arg("--fullscreen");
    }
    cmd.stdin(Stdio::null())
        // Nothing here parses the stdout contract (no `--json-status`), but with Match-window
        // on the session still writes `window` lines to stdout, and this GUI parent has no
        // console stdout for it to inherit. A null sink gives them somewhere to go.
        .stdout(Stdio::null())
        // Piped through the log tee: a couch launch (Start-menu tile, Steam shortcut) has no
        // console either, so the session's whole receive/decode/present log would otherwise
        // evaporate exactly when a user hits something worth reporting.
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);

    // Spawn (not `status()`) so the stderr pipe can be drained into the client log.
    let run = cmd.spawn().and_then(|mut child| {
        if let Some(stderr) = child.stderr.take() {
            pf_client_core::logring::forward_child_stderr(stderr, crate::logfile::Tee);
        }
        child.wait()
    });
    match run {
        Ok(st) => std::process::exit(st.code().unwrap_or(0)),
        Err(e) => {
            eprintln!("starting the console UI: {e}");
            std::process::exit(1);
        }
    }
}
