//! The Windows installer wizard's entry: a packed exe first re-runs itself from an admin-only
//! extract dir (`bootstrap`); then parse the Inno dialect, pick the mode — the extracted
//! payload's manifest (D1/D6), a `--demo` preset, or the silent `--dry-run` probe — and hand
//! the box to the reactor shell or, under `/VERYSILENT`, to the windowless silent path.
//!
//! A real silent run without a payload refuses with exit 1: a silent no-op that exits 0 is
//! exactly the fielded-updater bug D5 exists to prevent.

// No console window on double-click. Errors still reach redirected stderr (a GUI-subsystem
// child inherits pipes); the S2 lesson about PowerShell's `&` not waiting is a caller trap,
// which is why every tool verb lives in the console sibling `punktfunk-setup-pack`.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use punktfunk_setup_win::{bootstrap, payload, real::Seams, silent, wizard};

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use punktfunk_setup::platform::windows::args::InnoArgs;
    use punktfunk_setup::platform::windows::demo::{win_preset, WinPreset, WIN_PRESETS};
    use punktfunk_setup::platform::windows::{
        base_paths, launched_as_uninstaller, SystemNet, WinFacts,
    };
    use punktfunk_setup::seam::{Env, SystemRunner};
    use std::process::ExitCode;

    if let Some(code) = bootstrap::relaunch_if_packed() {
        return code;
    }

    const USAGE: &str = "punktfunk setup wizard\n\n\
        usage: punktfunk-host-setup-<ver>.exe [/VERYSILENT] [/LOG=<file>] [/MERGETASKS=...]\n\
               punktfunk-setup-win --demo <preset>\n\
               punktfunk-setup-win /VERYSILENT [/LOG=<file>] (--dry-run | --demo <preset>)\n\
        presets: win11-fresh win11-upgrade win11-sunshine win11-public win11-uninstall client-fresh client-win10 client-upgrade client-uninstall";

    let args: Vec<String> = std::env::args().skip(1).collect();
    let inno = InnoArgs::parse(&args);

    let mut demo = None;
    let mut dry = false;
    let mut it = inno.rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--demo" => demo = it.next().cloned(),
            "--dry-run" => dry = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown option: {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    // D6: the payload-less copy packed as unins000.exe is the uninstaller — the same wizard,
    // teardown path only. The name is the switch, so a demo copied to that name walks it.
    let uninstaller = std::env::current_exe().is_ok_and(|p| launched_as_uninstaller(&p));

    // The mode: a canned preset, or this box probed for real with the extracted payload
    // (`None` root = no payload: only the silent dry-run may proceed).
    let (mut preset, seams) = match demo {
        Some(name) => match win_preset(&name) {
            Some(preset) => (
                preset,
                Seams::Demo {
                    latency_ms: wizard::DEMO_LATENCY_MS,
                },
            ),
            None => {
                eprintln!(
                    "unknown --demo preset '{name}'. Try: {}",
                    WIN_PRESETS.join(", ")
                );
                return ExitCode::from(2);
            }
        },
        None => {
            let root = std::env::var_os(bootstrap::ROOT_ENV).map(std::path::PathBuf::from);
            let manifest = match root.as_deref().map(payload::manifest_at) {
                Some(Ok(m)) => Some(m),
                Some(Err(e)) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
                None => None,
            };
            let env = Env::from_env();
            let facts = WinFacts::probe(&base_paths(&env), &SystemRunner::new(), &env, &SystemNet);
            let (artifact, uninstall, version) = match &manifest {
                Some(m) => (m.artifact, m.uninstaller, m.version.clone()),
                None => (
                    punktfunk_setup::platform::windows::plan::Artifact::Host,
                    false,
                    env!("CARGO_PKG_VERSION").to_string(),
                ),
            };
            (
                WinPreset {
                    facts,
                    artifact,
                    uninstall,
                },
                Seams::Real { root, version },
            )
        }
    };
    preset.uninstall |= uninstaller;

    if inno.silence.is_silent() {
        return silent::main(&inno, preset, &seams, dry);
    }
    if dry {
        eprintln!("--dry-run is the silent path's flag — add /VERYSILENT\n\n{USAGE}");
        return ExitCode::from(2);
    }
    if matches!(&seams, Seams::Real { root: None, .. }) {
        eprintln!(
            "no payload in this build — walk a preset instead: --demo win11-fresh\n\n{USAGE}"
        );
        return ExitCode::from(2);
    }

    match wizard::run(preset, seams) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wizard failed: {e:?}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(windows))]
fn main() {
    // Windows-gated like clients/windows: a stub keeps `cargo build --workspace` green
    // elsewhere; the engine it drives tests cross-OS in punktfunk-setup itself.
    eprintln!("punktfunk-setup-win is Windows-only");
}
