//! `--demo` for the Windows wizard: canned boxes, and a runner that spawns nothing.
//!
//! The Linux rule holds verbatim (`demo.rs`): presets are built code, not embedded JSON, so
//! adding a `WinFacts` field breaks this file instead of silently defaulting a preset; and
//! the demo's safety is structural — the wizard hands the executor `WinDemoRunner` +
//! `FakeNet` + `FakePayload` and `demo::sandbox_paths()`, so nothing can reach the machine.
//!
//! One Windows-only extra: `WinExecutor`'s `RemoveFiles` deletes with `std::fs`, not through
//! the runner, so the uninstall preset's install location MUST be a sandbox path — a canned
//! `C:\Program Files\punktfunk` would point a real `remove_dir_all` at a real install.
//! `sandbox_app_dir` is that path; the wizard stages a marker file there so the removal has
//! something honest to remove.

use std::cell::Cell;

use crate::seam::{CommandRunner, Output, RunFailed, Stdin};

use super::plan::Artifact;
use super::{NetCategory, NetProfile, TaskState, WinFacts, WinInstall};

/// One per flow worth reviewing (WP2.3's list, plus M4's client manage mode).
pub const WIN_PRESETS: [&str; 9] = [
    "win11-fresh",
    "win11-upgrade",
    "win11-sunshine",
    "win11-public",
    "win11-uninstall",
    "client-fresh",
    "client-win10",
    "client-upgrade",
    "client-uninstall",
];

/// A canned box plus the mode the payload manifest would have carried.
#[derive(Debug, Clone, PartialEq)]
pub struct WinPreset {
    pub facts: WinFacts,
    pub artifact: Artifact,
    pub uninstall: bool,
}

/// The demo install dir — inside the same per-process sandbox root as `demo::sandbox_paths`.
pub fn sandbox_app_dir() -> String {
    std::env::temp_dir()
        .join(format!("punktfunk-setup-demo-{}", std::process::id()))
        .join("app")
        .display()
        .to_string()
}

/// A Win11 box with nothing punktfunk on it — every preset is this with fields moved.
fn fresh_box() -> WinFacts {
    WinFacts {
        os_build: 26200,
        arch: "x64".into(),
        installed: None,
        host_env_present: false,
        web_password_present: false,
        mgmt_bind_set: false,
        competing_hosts: vec![],
        mgmt_port_in_use: false,
        networks: vec![NetProfile {
            name: "Home".into(),
            category: NetCategory::Private,
        }],
        steam_audio_drivers: true,
        tray_autostart: false,
        vulkan_layer_registered: false,
        web_task: TaskState::Absent,
        scripting_task: TaskState::Absent,
        inno_uninstaller: false,
        client_installed: None,
    }
}

fn installed_box(location: String) -> WinFacts {
    WinFacts {
        installed: Some(WinInstall {
            version: Some("0.34.0".into()),
            location: Some(location),
        }),
        host_env_present: true,
        web_password_present: true,
        tray_autostart: true,
        vulkan_layer_registered: true,
        web_task: TaskState::Enabled,
        scripting_task: TaskState::Enabled,
        inno_uninstaller: true,
        ..fresh_box()
    }
}

pub fn win_preset(name: &str) -> Option<WinPreset> {
    let host = |facts| WinPreset {
        facts,
        artifact: Artifact::Host,
        uninstall: false,
    };
    Some(match name {
        "win11-fresh" => host(fresh_box()),
        "win11-upgrade" => host(installed_box(sandbox_app_dir())),
        "win11-sunshine" => host(WinFacts {
            competing_hosts: vec!["SunshineService".into()],
            mgmt_port_in_use: true,
            ..fresh_box()
        }),
        "win11-public" => host(WinFacts {
            networks: vec![NetProfile {
                name: "Cafe".into(),
                category: NetCategory::Public,
            }],
            ..fresh_box()
        }),
        "win11-uninstall" => WinPreset {
            facts: installed_box(sandbox_app_dir()),
            artifact: Artifact::Host,
            uninstall: true,
        },
        "client-fresh" => WinPreset {
            facts: fresh_box(),
            artifact: Artifact::Client,
            uninstall: false,
        },
        "client-win10" => WinPreset {
            facts: WinFacts {
                os_build: 17763,
                ..fresh_box()
            },
            artifact: Artifact::Client,
            uninstall: false,
        },
        // The client's key, not the host's: this box has no host and an old client.
        "client-upgrade" => WinPreset {
            facts: WinFacts {
                client_installed: Some(WinInstall {
                    version: Some("0.33.1".into()),
                    location: Some(sandbox_app_dir()),
                }),
                ..fresh_box()
            },
            artifact: Artifact::Client,
            uninstall: false,
        },
        "client-uninstall" => WinPreset {
            facts: WinFacts {
                client_installed: Some(WinInstall {
                    version: Some("0.35.0".into()),
                    location: Some(sandbox_app_dir()),
                }),
                ..fresh_box()
            },
            artifact: Artifact::Client,
            uninstall: true,
        },
        _ => return None,
    })
}

/// The demo's only runner. Every probe answers success after a beat of latency, so the
/// install page reads as work happening; `fail_at` fails the nth spawn for reviewing the
/// failure rendering. `WinExecutor` spawns through `probe` (there is no shell on Windows),
/// which is why the Linux `DemoRunner`'s `run_shell` latency does not carry over.
pub struct WinDemoRunner {
    latency_ms: u64,
    fail_at: Option<usize>,
    seen: Cell<usize>,
}

impl WinDemoRunner {
    pub fn new(latency_ms: u64, fail_at: Option<usize>) -> WinDemoRunner {
        WinDemoRunner {
            latency_ms,
            fail_at,
            seen: Cell::new(0),
        }
    }
}

impl CommandRunner for WinDemoRunner {
    fn run_shell(&self, _cmd: &str, _stdin: Stdin) -> Result<(), RunFailed> {
        Err(RunFailed) // nothing in a WinPlan shells; reaching here is a bug
    }

    fn probe(&self, _program: &str, _args: &[&str]) -> Option<Output> {
        let index = self.seen.get();
        self.seen.set(index + 1);
        if self.latency_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.latency_ms));
        }
        let code = i32::from(self.fail_at == Some(index));
        Some(Output {
            code,
            stdout: String::new(),
            stderr: if code == 0 {
                String::new()
            } else {
                "demo failure (--fail)".into()
            },
        })
    }

    fn which(&self, _program: &str) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::choices::WinChoices;
    use super::super::plan;
    use super::*;

    #[test]
    fn every_advertised_preset_exists_and_builds_a_plan() {
        for name in WIN_PRESETS {
            let preset =
                win_preset(name).unwrap_or_else(|| panic!("{name} is advertised but not built"));
            let choices = WinChoices::derive(&preset.facts, preset.artifact);
            let built = plan::build(&preset.facts, &choices, preset.artifact, preset.uninstall);
            assert!(!built.phases.is_empty(), "{name} produced no phases");
        }
        assert!(win_preset("nope").is_none());
    }

    /// The RemoveFiles trap: no preset that can reach the teardown — the uninstaller, or any
    /// installed box's manage Welcome — may aim it at a real install dir.
    #[test]
    fn every_installed_preset_lives_in_the_sandbox() {
        for name in WIN_PRESETS {
            let preset = win_preset(name).unwrap();
            let Some(installed) = preset.facts.installed_for(preset.artifact).cloned() else {
                continue;
            };
            let location = installed.location.unwrap();
            assert!(
                std::path::Path::new(&location).starts_with(std::env::temp_dir()),
                "{name}: {location} escaped the sandbox"
            );
            assert!(!location.contains("Program Files"));
        }
    }

    #[test]
    fn the_demo_runner_fails_exactly_where_told_and_never_shells() {
        let r = WinDemoRunner::new(0, Some(1));
        assert!(r.probe("reg", &[]).unwrap().ok());
        assert!(!r.probe("reg", &[]).unwrap().ok());
        assert!(r.probe("reg", &[]).unwrap().ok());
        assert!(r.run_shell("anything", Stdin::Null).is_err());
    }

    #[test]
    fn the_sunshine_preset_coexists_and_the_public_preset_triggers_d12() {
        let sunshine = win_preset("win11-sunshine").unwrap();
        assert!(sunshine.facts.needs_coexistence());
        let public = win_preset("win11-public").unwrap();
        let choices = WinChoices::derive(&public.facts, Artifact::Host);
        assert!(choices.needs_network_step(&public.facts));
        assert_eq!(public.facts.networks[0].category, NetCategory::Public);
        // The fresh box's Private network must NOT trigger it.
        let fresh = win_preset("win11-fresh").unwrap();
        let choices = WinChoices::derive(&fresh.facts, Artifact::Host);
        assert!(!choices.needs_network_step(&fresh.facts));
    }
}
