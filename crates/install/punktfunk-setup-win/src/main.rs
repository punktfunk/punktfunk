//! The host installer exe. build.rs links it with the requireAdministrator manifest.

// No console window on double-click. Errors still reach redirected stderr (a GUI-subsystem
// child inherits pipes). PowerShell's `&` neither waits for nor captures a GUI exe, which is
// why every tool verb lives in the console sibling `punktfunk-setup-pack`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    punktfunk_setup_win::entry::run()
}
