//! `--demo`: the whole flow against a canned box, with nothing able to touch this machine.
//!
//! The guarantee is structural: demo mode hands the plan a `DemoRunner`, and
//! the runner is the only thing in the crate that can spawn. There is no flag
//! inside `exec` to get wrong. See `design/installer-v2.md`.
//!
//! Presets are built here rather than parsed from embedded JSON so they cannot
//! drift from `Facts` — adding a field breaks this file instead of silently
//! defaulting a preset. `--facts <file.json>` is the escape hatch for an
//! arbitrary box.
//!
//! `--fail <phase>` resolves, at plan time, which command index that phase
//! starts at and tells the runner to fail there, so failure rendering is
//! reviewable without `exec` knowing demo mode exists.

use std::cell::Cell;

use crate::facts::{Channel, Facts, Family, Firewall, Nvidia, OsRelease};
use crate::plan::{Plan, StepAction};
use crate::seam::{BasePaths, CommandRunner, Output, RunFailed, Stdin};

/// Demo mode's filesystem root.
///
/// `SetEnv` and `SetSetting` steps write with `std::fs`, not a spawn, so a fake runner would still
/// reach `~/.config/punktfunk`. `BasePaths` is the seam that covers filesystem reach.
pub fn sandbox_paths() -> BasePaths {
    let root = std::env::temp_dir().join(format!("punktfunk-setup-demo-{}", std::process::id()));
    BasePaths::rooted(&root)
}

pub const PRESETS: [&str; 9] = [
    "arch-fresh",
    "debian-fresh",
    "fedora-sunshine",
    "bazzite-couch",
    "omarchy",
    "arch-canary-installed",
    "debian-noweb",
    "ubuntu-old",
    "unsupported-client",
];

/// A box with nothing punktfunk on it — every preset is this with fields moved.
fn box_of(id: &str, pretty: &str, version: &str, family: Family, docs: &str) -> Facts {
    Facts {
        os: OsRelease {
            id: id.to_string(),
            id_like: String::new(),
            version_id: version.to_string(),
            pretty: pretty.to_string(),
        },
        family,
        omarchy: id == "omarchy",
        docs_page: format!("https://docs.punktfunk.unom.io/docs/{docs}"),
        host_punt: None,
        rpm_group: (family == Family::Dnf).then(|| "fedora-44".to_string()),
        floor: None,
        has_flatpak_client: false,
        couch_box: id == "bazzite" || id == "nobara",
        graphical_seat: true,
        desktop_sessions: true,
        sunshine_active: false,
        current_channel: None,
        installed_pf: vec![],
        missing: vec!["host".into(), "web-console".into(), "plugin-runner".into()],
        host_version: None,
        has_web_server: false,
        has_omarchy_bin: id == "omarchy",
        has_ujust: false,
        in_input_group: false,
        in_punktfunk_group: false,
        has_input_group: true,
        nvidia: Nvidia::Absent,
        firewall: Firewall::None,
        systemd_pid1: true,
        user_manager: true,
        web_unit_present: true,
        web_password_present: false,
        web_bind: None,
        scripting_unit_disabled: false,
        ip: Some("192.168.1.24".into()),
        user: "you".into(),
    }
}

fn installed(mut facts: Facts, channel: Channel) -> Facts {
    facts.installed_pf = match facts.family {
        Family::Dnf => vec![
            "punktfunk".into(),
            "punktfunk-web".into(),
            "punktfunk-scripting".into(),
        ],
        Family::Sysext => vec![],
        _ => vec![
            "punktfunk-host".into(),
            "punktfunk-web".into(),
            "punktfunk-scripting".into(),
        ],
    };
    facts.missing = vec![];
    facts.current_channel = Some(channel);
    facts.host_version = Some("punktfunk-host 0.34.0".into());
    facts.has_web_server = true;
    // A box that has run the console already has a password; only a fresh one is asked.
    facts.web_password_present = true;
    facts.in_input_group = true;
    facts
}

pub fn preset(name: &str) -> Option<Facts> {
    let facts = match name {
        "arch-fresh" => box_of("arch", "Arch Linux", "", Family::Pacman, "arch"),
        "debian-fresh" => box_of(
            "debian",
            "Debian GNU/Linux 13 (trixie)",
            "13",
            Family::Apt,
            "debian",
        ),
        "fedora-sunshine" => {
            let mut f = box_of(
                "fedora",
                "Fedora Linux 44 (Workstation Edition)",
                "44",
                Family::Dnf,
                "fedora",
            );
            f.sunshine_active = true;
            f.firewall = Firewall::Firewalld;
            f.nvidia = Nvidia::Ok;
            f
        }
        "bazzite-couch" => {
            let mut f = box_of("bazzite", "Bazzite 43", "43", Family::Sysext, "bazzite");
            f.graphical_seat = false;
            f.has_ujust = true;
            f
        }
        "omarchy" => {
            let mut f = box_of("omarchy", "Omarchy", "", Family::Pacman, "omarchy");
            f.firewall = Firewall::Ufw;
            f
        }
        "arch-canary-installed" => {
            let f = box_of("arch", "Arch Linux", "", Family::Pacman, "arch");
            let mut f = installed(f, Channel::Canary);
            // The stranding trap, visible in the demo: a package the installer never installed.
            f.installed_pf.push("punktfunk-gamescope".into());
            f
        }
        "debian-noweb" => {
            let f = box_of(
                "debian",
                "Debian GNU/Linux 13 (trixie)",
                "13",
                Family::Apt,
                "debian",
            );
            let mut f = installed(f, Channel::Stable);
            f.has_web_server = false;
            f.web_unit_present = false;
            f.missing = vec!["web-console".into()];
            f.installed_pf = vec!["punktfunk-host".into(), "punktfunk-scripting".into()];
            f
        }
        "ubuntu-old" => {
            let mut f = box_of(
                "ubuntu",
                "Ubuntu 24.04.3 LTS",
                "24.04",
                Family::Apt,
                "ubuntu",
            );
            f.floor = crate::facts::floors(&f.os, Family::Apt).1;
            f
        }
        // A distro with no punktfunk repo. The host punt stands; the client still installs.
        "unsupported-client" => {
            let mut f = box_of("void", "Void Linux", "", Family::Flatpak, "install");
            f.host_punt = Some(
                "no package repo for 'Void Linux' yet — https://docs.punktfunk.unom.io/docs/developers/build-from-source"
                    .into(),
            );
            f
        }
        _ => return None,
    };
    Some(facts)
}

/// The only thing demo mode hands a `Plan`. It spawns nothing, ever.
pub struct DemoRunner {
    latency_ms: u64,
    /// Command index to fail at, resolved from `--fail <phase>`.
    fail_at: Option<usize>,
    seen: Cell<usize>,
}

impl DemoRunner {
    pub fn new(latency_ms: u64, fail_at: Option<usize>) -> DemoRunner {
        DemoRunner {
            latency_ms,
            fail_at,
            seen: Cell::new(0),
        }
    }
}

impl CommandRunner for DemoRunner {
    fn run_shell(&self, _cmd: &str, _stdin: Stdin) -> Result<(), RunFailed> {
        let index = self.seen.get();
        self.seen.set(index + 1);
        if self.latency_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(self.latency_ms));
        }
        if self.fail_at == Some(index) {
            return Err(RunFailed);
        }
        Ok(())
    }

    /// A demo box has everything, so no step is skipped for a missing binary.
    fn probe(&self, _program: &str, _args: &[&str]) -> Option<Output> {
        Some(Output {
            code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    fn which(&self, _program: &str) -> bool {
        true
    }
}

/// Command index of the named phase's first command. Title prefix, case-insensitive.
pub fn fail_index(plan: &Plan, phase: &str) -> Option<usize> {
    let needle = phase.to_lowercase();
    let mut command = 0usize;
    for p in &plan.phases {
        let matches = p.title.to_lowercase().contains(&needle);
        for step in &p.steps {
            if matches!(step.action, StepAction::Note(..)) {
                continue;
            }
            if matches {
                return Some(command);
            }
            command += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choices::{Choices, Pins};
    use crate::plan;

    #[test]
    fn every_advertised_preset_exists() {
        for name in PRESETS {
            assert!(preset(name).is_some(), "{name} is advertised but not built");
        }
        assert!(preset("nope").is_none());
    }

    #[test]
    fn every_preset_builds_a_plan() {
        for name in PRESETS {
            let facts = preset(name).expect(name);
            let choices = Choices::derive(&facts, &Pins::default());
            let built = plan::build(&facts, &choices);
            assert!(!built.phases.is_empty(), "{name} produced no phases");
        }
    }

    #[test]
    fn the_demo_runner_fails_exactly_where_it_was_told() {
        let r = DemoRunner::new(0, Some(1));
        assert!(r.run_shell("first", Stdin::Null).is_ok());
        assert!(r.run_shell("second", Stdin::Null).is_err());
        assert!(r.run_shell("third", Stdin::Null).is_ok());
    }

    #[test]
    fn a_phase_name_resolves_to_its_first_command() {
        let facts = preset("arch-fresh").unwrap();
        let choices = Choices::derive(&facts, &Pins::default());
        let built = plan::build(&facts, &choices);
        assert_eq!(
            fail_index(&built, "installing"),
            Some(0),
            "the repo write starts it"
        );
        let groups = fail_index(&built, "controller").expect("a controller phase");
        assert!(groups > 0);
        assert_eq!(fail_index(&built, "nonsense"), None);
    }

    /// A `SetEnv` step writes with `std::fs`, so a fake runner alone does not make demo mode safe.
    #[test]
    fn the_sandbox_never_points_at_the_real_config_directory() {
        let sandbox = sandbox_paths().host_env();
        assert!(
            sandbox.starts_with(std::env::temp_dir()),
            "{} escaped /tmp",
            sandbox.display()
        );
        if let Some(home) = std::env::var_os("HOME") {
            let real = std::path::Path::new(&home).join(".config/punktfunk/host.env");
            assert_ne!(sandbox, real);
        }
    }

    /// The version floor must reach the demo, or `ubuntu-old` reviews nothing.
    #[test]
    fn the_ubuntu_preset_carries_its_floor() {
        let f = preset("ubuntu-old").unwrap();
        assert!(matches!(f.floor, Some(crate::facts::Floor::Confirm(_))));
    }
}
