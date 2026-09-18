//! The Windows option set: Inno task checkboxes as engine choices.
//!
//! Defaults derive from `WinFacts` (`design/installer-v2-windows.md`). Fresh installs get
//! the fresh-install defaults; upgrades pre-fill from the box, not a remembered-checkbox
//! registry. GameStream and the public-firewall opt-in are `Option<bool>`: `None` means the
//! plan passes nothing and the box keeps its state. A default must never rewrite an upgrade.
//! An explicit task flag or the D12 network answer sets `Some` even on upgrades, which is
//! what lets Reconfigure change a box.

use std::path::PathBuf;

use crate::seam::Env;

use super::args::{InnoArgs, TaskFlag};
use super::plan::Artifact;
use super::{NetCategory, WinFacts};

/// What "this PC only" means to the console's listener.
pub const LOOPBACK_BIND: &str = "127.0.0.1";

/// The console's default listen address: every interface. The console answers only peers on the
/// local network or a VPN, never the internet.
pub const LAN_BIND: &str = "0.0.0.0";

/// `/WEBBIND` and its env twin, in the Linux installer's spelling.
fn parse_bind(raw: &str) -> Option<String> {
    match raw.trim() {
        "" => None,
        "localhost" | "loopback" => Some(LOOPBACK_BIND.to_string()),
        "lan" | "any" => Some(LAN_BIND.to_string()),
        v if v.parse::<std::net::IpAddr>().is_ok() => Some(v.to_string()),
        _ => None,
    }
}

/// D12. `Skip` is the silent default: a profile change needs a consent surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkAnswer {
    /// Set the named network's NLA category to Private.
    MakePrivate(String),
    /// Keep it Public and add the public-profile firewall rules.
    OpenPublicRules,
    Skip,
}

/// Same `1`/`0` reading as the Linux env twins.
const ENV_TWINS: [(&str, Task); 8] = [
    ("PUNKTFUNK_INSTALL_DRIVER", Task::Driver),
    ("PUNKTFUNK_INSTALL_GAMEPAD", Task::Gamepad),
    ("PUNKTFUNK_INSTALL_HDR_LAYER", Task::HdrLayer),
    ("PUNKTFUNK_INSTALL_GAMESTREAM", Task::Gamestream),
    ("PUNKTFUNK_INSTALL_PUBLIC_FIREWALL", Task::AllowPublicFw),
    ("PUNKTFUNK_INSTALL_START_SERVICE", Task::StartService),
    ("PUNKTFUNK_INSTALL_TRAY", Task::TrayIcon),
    ("PUNKTFUNK_INSTALL_DESKTOP_ICON", Task::DesktopIcon),
];

/// `/MERGETASKS` names are a published contract (winget, troubleshooting docs). Never rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Driver,
    Gamepad,
    HdrLayer,
    Gamestream,
    AllowPublicFw,
    StartService,
    TrayIcon,
    DesktopIcon,
}

impl Task {
    fn from_name(name: &str) -> Option<Task> {
        Some(match name {
            "installdriver" => Task::Driver,
            "installgamepad" => Task::Gamepad,
            "installhdrlayer" => Task::HdrLayer,
            "gamestream" => Task::Gamestream,
            "allowpublicfw" => Task::AllowPublicFw,
            "startservice" => Task::StartService,
            "trayicon" => Task::TrayIcon,
            "desktopicon" => Task::DesktopIcon,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinChoices {
    pub install_driver: bool,
    pub install_gamepad: bool,
    pub install_hdr_layer: bool,
    /// `None` = pass nothing, the box keeps its persisted state.
    pub gamestream: Option<bool>,
    /// `None` = pass nothing; the opt-in marker file persists the previous choice.
    pub allow_public_fw: Option<bool>,
    pub start_service: bool,
    pub tray_autostart: bool,
    /// Client artifact only; the host creates no shortcuts.
    pub desktop_icon: bool,
    /// Fresh only. `None` = executor generates 24 hex chars. Never render into a transcript or argv.
    pub web_password: Option<String>,
    /// Where the web console listens (`PUNKTFUNK_UI_BIND`). `None` = pass nothing: `service
    /// install` then keeps host.env's answer, which on an upgrade is the reach the box already had.
    pub web_bind: Option<String>,
    /// Upgrades pre-fill the ARP location and ignore `/DIR` (Inno `UsePreviousAppDir`).
    pub dir: Option<PathBuf>,
    pub network: NetworkAnswer,
}

impl WinChoices {
    /// Defaults for installing `artifact`.
    ///
    /// The artifact is not decoration: host and client are two products under two registry
    /// keys (D1), so reading `facts.installed` unconditionally made a client install inherit
    /// the HOST's upgrade verdict and its install directory.
    pub fn derive(facts: &WinFacts, artifact: Artifact) -> WinChoices {
        let installed = facts.installed_for(artifact);
        let upgrade = installed.is_some();
        WinChoices {
            install_driver: true,
            install_gamepad: true,
            install_hdr_layer: if upgrade {
                facts.vulkan_layer_registered
            } else {
                true
            },
            gamestream: if upgrade { None } else { Some(false) },
            allow_public_fw: if upgrade { None } else { Some(false) },
            start_service: true,
            tray_autostart: if upgrade { facts.tray_autostart } else { true },
            desktop_icon: false,
            web_password: None,
            // Fresh boxes are asked and answer the local network by default; an upgrade keeps
            // host.env's.
            web_bind: if upgrade {
                None
            } else {
                Some(LAN_BIND.to_string())
            },
            dir: installed
                .and_then(|i| i.location.clone())
                .map(PathBuf::from),
            network: NetworkAnswer::Skip,
        }
    }

    pub fn needs_network_step(&self, facts: &WinFacts) -> bool {
        self.allow_public_fw != Some(true)
            && self.network == NetworkAnswer::Skip
            && facts
                .networks
                .iter()
                .any(|n| n.category == NetCategory::Public)
    }

    /// Warnings only — unknown task names, an ignored `/DIR`. Never an error (D5).
    pub fn apply(&mut self, args: &InnoArgs, env: &Env) -> Vec<String> {
        let mut warnings = Vec::new();
        for (key, task) in ENV_TWINS {
            if let Some(v) = env.get(key) {
                self.set(task, v == "1");
            }
        }
        // /TASKS replaces defaults (Inno): everything off, then the list.
        if let Some(tasks) = &args.tasks {
            for task in [
                Task::Driver,
                Task::Gamepad,
                Task::HdrLayer,
                Task::Gamestream,
                Task::AllowPublicFw,
                Task::StartService,
                Task::TrayIcon,
                Task::DesktopIcon,
            ] {
                self.set(task, false);
            }
            self.apply_flags(tasks, &mut warnings);
        }
        self.apply_flags(&args.merge_tasks, &mut warnings);
        // Address, not a task flag, so it takes the same spelling as the Linux `--web-bind`:
        // an address, or `localhost` / `lan`. A value we cannot read warns and changes nothing —
        // an unattended install must not fail on it, and must not guess either (D5).
        for raw in [
            env.get("PUNKTFUNK_INSTALL_WEB_BIND"),
            args.web_bind.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            match parse_bind(raw) {
                Some(addr) => self.web_bind = Some(addr),
                None => warnings.push(format!(
                    "web console bind '{raw}' is not an address — ignored"
                )),
            }
        }
        if let Some(dir) = &args.dir {
            match &self.dir {
                Some(existing) => warnings.push(format!(
                    "/DIR ignored — an existing install stays in {}",
                    existing.display()
                )),
                None => self.dir = Some(dir.clone()),
            }
        }
        warnings
    }

    fn apply_flags(&mut self, flags: &[TaskFlag], warnings: &mut Vec<String>) {
        for flag in flags {
            match Task::from_name(&flag.name) {
                Some(task) => self.set(task, flag.selected),
                None => warnings.push(format!("unknown task '{}' ignored", flag.name)),
            }
        }
    }

    fn set(&mut self, task: Task, selected: bool) {
        match task {
            Task::Driver => self.install_driver = selected,
            Task::Gamepad => self.install_gamepad = selected,
            Task::HdrLayer => self.install_hdr_layer = selected,
            Task::Gamestream => self.gamestream = Some(selected),
            Task::AllowPublicFw => self.allow_public_fw = Some(selected),
            Task::StartService => self.start_service = selected,
            Task::TrayIcon => self.tray_autostart = selected,
            Task::DesktopIcon => self.desktop_icon = selected,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{NetProfile, WinInstall};
    use super::*;

    fn fresh_facts() -> WinFacts {
        WinFacts {
            os_build: 26200,
            arch: "x64".into(),
            installed: None,
            host_env_present: false,
            web_password_present: false,
            mgmt_bind_set: false,
            competing_hosts: vec![],
            mgmt_port_in_use: false,
            networks: vec![],
            steam_audio_drivers: true,
            tray_autostart: false,
            vulkan_layer_registered: false,
            web_task: super::super::TaskState::Absent,
            scripting_task: super::super::TaskState::Absent,
            inno_uninstaller: false,
            client_installed: None,
        }
    }

    fn upgrade_facts() -> WinFacts {
        WinFacts {
            installed: Some(WinInstall {
                version: Some("0.34.0".into()),
                location: Some(r"C:\Program Files\punktfunk\".into()),
            }),
            host_env_present: true,
            tray_autostart: false,
            vulkan_layer_registered: true,
            ..fresh_facts()
        }
    }

    #[test]
    fn fresh_defaults_match_the_iss_task_table() {
        let c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        assert!(c.install_driver && c.install_gamepad && c.install_hdr_layer);
        assert_eq!(c.gamestream, Some(false));
        assert_eq!(c.allow_public_fw, Some(false));
        assert!(c.start_service && c.tray_autostart);
        assert!(!c.desktop_icon);
        assert!(c.dir.is_none());
    }

    // Persisted settings pass nothing; observable ones pre-fill from the box.
    #[test]
    fn upgrade_defaults_leave_box_state_alone() {
        let c = WinChoices::derive(&upgrade_facts(), Artifact::Host);
        assert_eq!(c.gamestream, None);
        assert_eq!(c.allow_public_fw, None);
        assert!(!c.tray_autostart);
        assert!(c.install_hdr_layer);
        assert_eq!(
            c.dir.as_ref().unwrap().to_str().unwrap(),
            r"C:\Program Files\punktfunk\"
        );
    }

    #[test]
    fn an_explicit_task_overrides_even_on_upgrade() {
        let mut c = WinChoices::derive(&upgrade_facts(), Artifact::Host);
        let args = InnoArgs::parse(&[r#"/MERGETASKS="allowpublicfw""#.to_string()]);
        let warnings = c.apply(&args, &Env::default());
        assert!(warnings.is_empty());
        assert_eq!(c.allow_public_fw, Some(true));
        assert_eq!(c.gamestream, None);
    }

    #[test]
    fn tasks_replaces_and_mergetasks_merges_over_defaults() {
        let mut c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        let replace = InnoArgs::parse(&["/TASKS=installdriver".to_string()]);
        c.apply(&replace, &Env::default());
        assert!(c.install_driver);
        assert!(!c.install_gamepad && !c.start_service && !c.tray_autostart);

        let mut c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        let merge = InnoArgs::parse(&[r#"/MERGETASKS="!trayicon""#.to_string()]);
        c.apply(&merge, &Env::default());
        assert!(!c.tray_autostart);
        assert!(c.install_driver && c.start_service);
    }

    #[test]
    fn an_unknown_task_warns_and_changes_nothing() {
        let mut c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        let args = InnoArgs::parse(&["/MERGETASKS=frobnicate".to_string()]);
        let warnings = c.apply(&args, &Env::default());
        assert_eq!(warnings, ["unknown task 'frobnicate' ignored"]);
        assert_eq!(c, WinChoices::derive(&fresh_facts(), Artifact::Host));
    }

    #[test]
    fn dir_is_honoured_fresh_and_ignored_with_a_warning_on_upgrade() {
        let mut c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        let args = InnoArgs::parse(&[r"/DIR=D:\pf".to_string()]);
        assert!(c.apply(&args, &Env::default()).is_empty());
        assert_eq!(c.dir.as_ref().unwrap().to_str().unwrap(), r"D:\pf");

        let mut c = WinChoices::derive(&upgrade_facts(), Artifact::Host);
        let warnings = c.apply(&args, &Env::default());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("/DIR ignored"));
        assert_eq!(
            c.dir.as_ref().unwrap().to_str().unwrap(),
            r"C:\Program Files\punktfunk\"
        );
    }

    #[test]
    fn env_twins_read_one_and_zero_and_flags_overwrite_them() {
        let mut c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        let env = Env::of(&[("PUNKTFUNK_INSTALL_TRAY", "0")]);
        c.apply(&InnoArgs::parse(&[]), &env);
        assert!(!c.tray_autostart);

        // A task flag wins over the twin (env, then args).
        let mut c = WinChoices::derive(&fresh_facts(), Artifact::Host);
        let args = InnoArgs::parse(&[r#"/MERGETASKS="trayicon""#.to_string()]);
        c.apply(&args, &env);
        assert!(c.tray_autostart);
    }

    #[test]
    fn the_network_step_triggers_on_public_without_opted_rules() {
        let mut facts = fresh_facts();
        facts.networks = vec![NetProfile {
            name: "Cafe".into(),
            category: NetCategory::Public,
        }];
        let mut c = WinChoices::derive(&facts, Artifact::Host);
        assert!(c.needs_network_step(&facts));
        c.allow_public_fw = Some(true);
        assert!(!c.needs_network_step(&facts));
        c.allow_public_fw = Some(false);
        c.network = NetworkAnswer::MakePrivate("Cafe".into());
        assert!(!c.needs_network_step(&facts));
    }
}
