//! `punktfunk-console.exe` — the couch/HTPC entry point.
//!
//! Exists because an MSIX `<Application>` cannot pass ARGUMENTS to a full-trust executable:
//! a second Start-menu tile therefore cannot simply be "punktfunk-client.exe --console", it
//! needs its own executable. This is that executable, and it is deliberately nothing but a
//! hand-off — it starts the session binary's `--browse` mode (the complete controller-driven
//! client: host list, discovery, PIN pairing, settings, Wake-on-LAN, library) fullscreen and
//! mirrors its exit code, so whatever supervises this process sees the real result.
//!
//! `--windowed` keeps it in a window; everything else is the session binary's own business.

// No console window: this is launched from a Start-menu tile / shortcut, and a flashing
// console behind the couch UI looks like a crash.
#![cfg_attr(windows, windows_subsystem = "windows")]

// The shared client log sink (std-only): a couch launch has no console, so the session's
// stderr would otherwise evaporate — same reasoning as the shell's tee. This shim only
// initializes + forwards; the module's subscriber-side surface stays unused here.
#[cfg(windows)]
#[path = "../logfile.rs"]
#[allow(dead_code)]
mod logfile;

// The hand-off itself, shared verbatim with the shell's `--console` flag — including the
// CREATE_NO_WINDOW the console-subsystem session binary needs from a GUI parent. One file,
// so the flag is stated once.
#[cfg(windows)]
#[path = "../couch.rs"]
#[allow(dead_code)]
mod couch;

#[cfg(windows)]
fn main() {
    logfile::init();
    couch::run_browse();
}

/// The workspace builds on Linux/macOS too; there is nothing to launch there.
#[cfg(not(windows))]
fn main() {}
