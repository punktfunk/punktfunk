//! The host installer exe. build.rs links it with the requireAdministrator manifest.

// No console window on double-click. Errors still reach redirected stderr (a GUI-subsystem
// child inherits pipes); the S2 lesson about PowerShell's `&` not waiting is a caller trap,
// which is why every tool verb lives in the console sibling `punktfunk-setup-pack`.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> std::process::ExitCode {
    punktfunk_setup_win::entry::run()
}
