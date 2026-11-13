//! The client installer exe: the host's wizard, without the admin manifest. The client
//! installs per user and never elevates.

// No console window on double-click, as in `main.rs`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    punktfunk_setup_win::entry::run()
}
