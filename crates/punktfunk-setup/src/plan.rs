//! Stage three: `(Facts, Choices) → Plan`. Pure — no I/O, no spawns, no env reads.
//!
//! A `Step` is data. `--dry-run` renders the Plan; uninstall and channel-switch are Plans
//! from a different `Action`, not modes with their own I/O.
//!
//! Four actions cannot be command strings: apt madison pins, the pacman availability split,
//! linger, and unit enable each need something the previous step created. They stay
//! `StepAction` variants so `exec` owns the trap. `design/installer-v2.md`.

use serde::{Deserialize, Serialize};

use crate::choices::{Action, Choices, Components};
use crate::facts::{Channel, Facts, Family, Firewall, DOCS};
use crate::platform;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Ok,
    Warn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Uninstall,
    Switch,
    Install,
    Password,
    Omarchy,
    Conflicts,
    Groups,
    Options,
    Firewall,
    Linger,
    Start,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepAction {
    Run(String),
    /// Replaced or appended in `host.env`: a pin the console cannot change.
    SetEnv {
        key: String,
        value: String,
    },
    /// Merged into `host-settings.json`: a default the operator can change in the console.
    SetSetting {
        id: String,
        value: serde_json::Value,
    },
    Note(Level, String),
    /// apt will not walk back to a lower candidate; madison after the repo rewrite.
    AptSwitch {
        pkgs: Vec<String>,
    },
    /// Split `-Rdd` / `-S` from `pacman -Si` against the repo the previous step just added.
    PacmanSwitch {
        pkgs: Vec<String>,
    },
    /// Dry-run renders this even when `program` is missing: the Omarchy hand-off is
    /// planned before the install that ships the binary.
    RunIfPresent {
        program: String,
        cmd: String,
        warn_if_missing: Option<String>,
    },
    /// No-op with a warning where systemd is not PID 1: a container has no logind.
    Linger,
    /// Re-probes the user manager linger may have just created.
    StartUnits {
        units: Vec<String>,
    },
    /// Writes the console login password the user typed to `~/.config/punktfunk/web-password`,
    /// 0600. The value stays in `Choices`: a plan is echoed, dry-run printed and pinned in
    /// goldens, and a password belongs in none of the three.
    WebPassword,
    /// Files the console's certificate in the user's NSS store. Resolved in `exec`: the host
    /// mints the certificate on its first start, so the step has to wait for the file.
    TrustCert,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub action: StepAction,
    /// On success, skip every later phase. Only Omarchy sets it: that command already did
    /// groups, firewall, and autostart.
    pub ends_run: bool,
}

impl Step {
    pub fn run(cmd: impl Into<String>) -> Step {
        Step {
            action: StepAction::Run(cmd.into()),
            ends_run: false,
        }
    }

    pub fn note(level: Level, text: impl Into<String>) -> Step {
        Step {
            action: StepAction::Note(level, text.into()),
            ends_run: false,
        }
    }

    pub fn set_env(key: &str, value: impl Into<String>) -> Step {
        Step {
            action: StepAction::SetEnv {
                key: key.to_string(),
                value: value.into(),
            },
            ends_run: false,
        }
    }

    pub fn set_setting(id: &str, value: impl Into<serde_json::Value>) -> Step {
        Step {
            action: StepAction::SetSetting {
                id: id.to_string(),
                value: value.into(),
            },
            ends_run: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanPhase {
    pub kind: Phase,
    pub title: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub phases: Vec<PlanPhase>,
}

impl Plan {
    pub fn steps(&self) -> impl Iterator<Item = &Step> {
        self.phases.iter().flat_map(|p| p.steps.iter())
    }

    pub fn commands(&self) -> Vec<String> {
        self.steps()
            .filter_map(|s| match &s.action {
                StepAction::Run(c) | StepAction::RunIfPresent { cmd: c, .. } => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    fn push(&mut self, kind: Phase, title: impl Into<String>, steps: Vec<Step>) {
        self.phases.push(PlanPhase {
            kind,
            title: title.into(),
            steps,
        });
    }
}

pub fn build(facts: &Facts, choices: &Choices) -> Plan {
    let mut plan = Plan::default();
    let backend = platform::backend(facts.family);

    if choices.action == Action::Uninstall {
        let mut steps = backend.uninstall(facts);
        // The family sweep cannot see a flatpak client.
        if facts.has_flatpak_client && facts.family != Family::Flatpak {
            steps.extend(platform::backend(Family::Flatpak).uninstall(facts));
        }
        plan.push(
            Phase::Uninstall,
            format!("Uninstalling the host ({DOCS}/uninstall)"),
            steps,
        );
        return plan;
    }

    // A switch reinstalls what the box has; the unit probe already saw its console.
    let console_installed = match choices.switch_from {
        Some(from) => {
            plan.push(
                Phase::Switch,
                format!(
                    "Channel switch: {} → {} ({DOCS}/channels)",
                    from.as_str(),
                    choices.channel.as_str()
                ),
                backend.switch(facts, choices),
            );
            false
        }
        None => install_phase(&mut plan, facts, choices, backend),
    };

    // Before Omarchy, whose hand-off ends the run, and long before the console starts: the
    // console's own first start generates a password only when the file is not there yet.
    if choices.components.host && choices.web_password.is_some() {
        plan.push(
            Phase::Password,
            "Web console password",
            vec![Step {
                action: StepAction::WebPassword,
                ends_run: false,
            }],
        );
    }

    // What the Omarchy hand-off does not do lands before it: it starts the host and ends the
    // run. Next to Sunshine the first bind must already be on the moved mgmt port.
    if choices.components.host {
        plan.push(
            Phase::Conflicts,
            "Checking for Sunshine / Apollo / Vibeshine",
            conflict_steps(facts, choices),
        );
        plan.push(
            Phase::Options,
            "Options (each one can be changed later)",
            option_steps(facts, choices),
        );
        // Linger is configuration, not start, so --no-start still honours it. It also creates
        // the user manager on a seatless box, so it must land before the unit enable below.
        if choices.linger {
            plan.push(
                Phase::Linger,
                "Starting at boot with nobody logged in",
                vec![Step {
                    action: StepAction::Linger,
                    ends_run: false,
                }],
            );
        }
    }

    if facts.omarchy {
        plan.push(Phase::Omarchy, "Omarchy", omarchy_steps(facts, choices));
    }

    // A client listens on nothing fixed, so skip groups, firewall, and start.
    if !choices.components.host {
        return plan;
    }

    plan.push(
        Phase::Groups,
        "Controller access",
        group_steps(facts, choices),
    );
    plan.push(Phase::Firewall, "Firewall", firewall_steps(facts, choices));
    if choices.start {
        plan.push(
            Phase::Start,
            "Starting the host and the web console",
            start_steps(facts, choices, console_installed),
        );
    }
    plan
}

/// Whether this run puts the web console on the box.
fn install_phase(
    plan: &mut Plan,
    facts: &Facts,
    choices: &Choices,
    backend: &dyn platform::PkgBackend,
) -> bool {
    // Host, console, and plugin runner are three packages. A weak-deps-off box never
    // grows a console if we only ask "is the host there".
    if facts.fully_installed() && choices.components.host {
        let version = facts.host_version.clone().unwrap_or_default();
        // No repo means this build came from somewhere else — source, a hand-placed package —
        // so there is nothing to upgrade against, and adding a repo now would install over it.
        let Some(channel) = facts.current_channel else {
            plan.push(
                Phase::Install,
                format!(
                    "host, web console and plugin runner are already installed ({version}) — skipping the install, continuing with setup"
                ),
                vec![Step::note(
                    Level::Warn,
                    format!(
                        "--channel {} had nothing to act on: no punktfunk package repo is configured here, so this install did not come from one (built from source?). Channels: {DOCS}/channels",
                        choices.channel.as_str()
                    ),
                )],
            );
            return false;
        };
        // The install line is the only step that moves an installed box onto the channel's
        // current build; skipping it stranded one on whatever it had, so a re-run could never
        // deliver a fix and only an uninstall could. Minus the repo write: having a channel is
        // how we got here, so re-importing the signing key every time buys nothing.
        let repo = backend.write_repo(facts, choices);
        let mut steps = flatpak_client(facts, choices);
        steps.extend(
            backend
                .install(facts, choices)
                .into_iter()
                .filter(|step| !repo.contains(step)),
        );
        plan.push(
            Phase::Install,
            format!(
                "host, web console and plugin runner are already installed ({version}, {} channel) — updating to the current build",
                channel.as_str()
            ),
            steps,
        );
        return facts.family.installs_console();
    }
    let mut what = if choices.components.host {
        facts.missing.join(" ")
    } else {
        String::new()
    };
    let mut steps = flatpak_client(facts, choices);
    // The family backend also installs a native client in the same transaction, including
    // a client-only run on apt, dnf, and pacman.
    let native_client = choices.components.client && facts.family.has_native_client();
    if choices.components.host || native_client {
        steps.extend(backend.install(facts, choices));
    }
    if choices.components.client {
        if !what.is_empty() {
            what.push(' ');
        }
        what.push_str("client");
    }
    // SteamOS compiles `main` on the device, so naming a package channel here would be a lie.
    let how = if facts.family == Family::Steamos {
        "built on this device".to_string()
    } else {
        format!("{} channel", choices.channel.as_str())
    };
    plan.push(Phase::Install, format!("Installing: {what} ({how})"), steps);
    choices.components.host && facts.family.installs_console()
}

/// The user-scope flatpak for a family with no `punktfunk-client`, unless it is already there.
/// Planned before the host steps: SteamOS's on-device build is the step that ends the run.
fn flatpak_client(facts: &Facts, choices: &Choices) -> Vec<Step> {
    if choices.components.client && !facts.family.has_native_client() && !facts.has_flatpak_client {
        platform::backend(Family::Flatpak).install(facts, choices)
    } else {
        vec![]
    }
}

/// Everything from here to the start phase is generic Linux wiring — a group, a wide-open
/// firewall, a user unit. Omarchy has a better local answer for each and one command that does
/// them all and knows how to reverse itself, so offer that instead of a weaker second version.
///
/// Every optional part is passed explicitly. `punktfunk-omarchy setup` used to ask for four of
/// them itself, which is how an Omarchy install grew a second round of questions after this one
/// had finished — and how `--yes` skipped the integration entirely, since that script's prompt
/// defaulted to no on a pipe.
fn omarchy_steps(_facts: &Facts, choices: &Choices) -> Vec<Step> {
    if !choices.omarchy_setup {
        return vec![Step::note(
            Level::Ok,
            format!("Run it later with: punktfunk-omarchy setup   ({DOCS}/omarchy)"),
        )];
    }
    let cmd = format!(
        "punktfunk-omarchy setup --groups={} --cert={} --toasts={} --idle-guard={} --theme={}",
        bit(choices.punktfunk_group),
        bit(choices.console_cert),
        bit(choices.omarchy_toasts),
        bit(choices.omarchy_idle),
        bit(choices.omarchy_theme),
    );
    vec![Step {
        action: StepAction::RunIfPresent {
            program: "punktfunk-omarchy".into(),
            cmd,
            warn_if_missing: Some(format!(
                "punktfunk-omarchy is not on PATH — the host package should ship it; see {DOCS}/omarchy"
            )),
        },
        ends_run: true,
    }]
}

/// The hand-off takes 1/0, never a bare flag: a missing one must be a parse error there, not a
/// silent no.
fn bit(on: bool) -> &'static str {
    if on {
        "1"
    } else {
        "0"
    }
}

fn conflict_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    if !facts.sunshine_active {
        return vec![Step::note(
            Level::Ok,
            "No conflicting game-streaming host detected.",
        )];
    }
    let mut steps = vec![Step::note(
        Level::Warn,
        "another streaming host is active on this box — both want TCP 47990 (its web UI, punktfunk's management API)",
    )];
    if choices.move_mgmt_port {
        steps.push(Step::set_env(
            "PUNKTFUNK_MGMT_BIND",
            format!("0.0.0.0:{}", choices.mgmt_port),
        ));
        steps.push(Step::note(
            Level::Ok,
            format!("Clients learn the port from discovery; the console and plugins read it from mgmt-endpoint. Details: {DOCS}/switching-from-sunshine"),
        ));
    } else {
        steps.push(Step::note(
            Level::Warn,
            format!("Stop it before you start punktfunk (e.g. sudo systemctl disable --now sunshine) — {DOCS}/switching-from-sunshine"),
        ));
    }
    steps
}

fn group_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    let mut steps = vec![];
    if facts.in_input_group {
        steps.push(Step::note(Level::Ok, "already in the input group"));
    } else if facts.has_ujust {
        // Bazzite's input group is recipe-managed; usermod is the wrong tool. With no action
        // the recipe opens a gum menu, which the captured progress line hides while the step
        // waits on stdin. A recipe older than that action takes none, and usermod is its body.
        steps.push(Step::run(
            r#"ujust add-user-to-input-group add || sudo usermod -aG input "$USER""#,
        ));
    } else if !facts.has_input_group {
        steps.push(Step::note(
            Level::Warn,
            format!(
                "no 'input' group on this system — virtual gamepads need /dev/uinput access; see {}",
                facts.docs_page
            ),
        ));
    } else {
        steps.push(Step::run(r#"sudo usermod -aG input "$USER""#));
    }
    if choices.punktfunk_group {
        if facts.in_punktfunk_group {
            steps.push(Step::note(Level::Ok, "already in the punktfunk group"));
        } else {
            steps.push(Step::run(r#"sudo usermod -aG punktfunk "$USER""#));
        }
    }
    steps
}

fn option_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    let mut steps = vec![];
    // Always written, never conditional: the line is what tells web-init.sh this box has already
    // answered the question, so an upgrade never has to guess the console's reach for it.
    steps.push(Step::set_env("PUNKTFUNK_UI_BIND", choices.web_bind.clone()));
    if choices.gamestream {
        if facts.sunshine_active {
            steps.push(Step::note(
                Level::Warn,
                "with another GameStream host running, only one can bind the Moonlight ports — stop the other first or skip this",
            ));
        }
        steps.push(Step::set_setting("gamestream", true));
    }
    if choices.clipboard {
        steps.push(Step::set_setting("clipboard", "files"));
    }
    // A box with no desktop installed cannot stand a session up for the host; gamescope
    // brings its own. A seat that is merely not logged in (ssh) keeps the host's detection.
    if !facts.graphical_seat && !facts.desktop_sessions && !facts.couch_box {
        steps.push(Step::note(
            Level::Ok,
            "no desktop session is installed here, so the host will spawn a headless gamescope per connect",
        ));
        steps.push(Step::set_env("PUNKTFUNK_COMPOSITOR", "gamescope"));
    }
    steps
}

/// Packages install firewalld services and ufw profiles by name; they never open ports.
fn firewall_steps(facts: &Facts, choices: &Choices) -> Vec<Step> {
    let moved = choices.move_mgmt_port.then_some(choices.mgmt_port);
    match facts.firewall {
        Firewall::Firewalld => {
            let mut svcs =
                String::from("--add-service=punktfunk-native --add-service=punktfunk-web");
            if choices.gamestream {
                svcs.push_str(" --add-service=punktfunk-gamestream");
            }
            if let Some(port) = moved {
                svcs.push_str(&format!(" --add-port={port}/tcp"));
            }
            vec![
                Step::run("sudo firewall-cmd --reload"),
                Step::run(format!("sudo firewall-cmd --permanent {svcs}")),
                Step::run("sudo firewall-cmd --reload"),
            ]
        }
        Firewall::Ufw => {
            let mut steps = vec![
                Step::run("sudo ufw allow punktfunk-native"),
                Step::run("sudo ufw allow punktfunk-web"),
            ];
            if choices.gamestream {
                steps.push(Step::run("sudo ufw allow punktfunk-gamestream"));
            }
            if let Some(port) = moved {
                steps.push(Step::run(format!("sudo ufw allow {port}/tcp")));
            }
            steps
        }
        Firewall::None => vec![Step::note(
            Level::Ok,
            format!(
                "no active firewall found — nothing to open ({DOCS}/ports if you add one later)"
            ),
        )],
    }
}

fn start_steps(facts: &Facts, choices: &Choices, console_installed: bool) -> Vec<Step> {
    let mut steps = vec![];
    let mut units = vec!["punktfunk-host".to_string()];
    // The install phase puts the console on every package family, so its unit exists by
    // the time this runs even when the pre-install probe found none.
    if facts.web_unit_present || console_installed {
        units.push("punktfunk-web".to_string());
    } else {
        steps.push(Step::note(
            Level::Warn,
            format!(
                "no punktfunk-web.service on this box — the console is not installed, so nothing will answer on 47992 ({})",
                facts.docs_page
            ),
        ));
    }
    // apt/dnf/sysext already start the plugin runner; Arch does not.
    if facts.scripting_unit_disabled {
        units.push("punktfunk-scripting".to_string());
    }
    steps.push(Step {
        action: StepAction::StartUnits { units },
        ends_run: false,
    });
    // After the start, never before: the certificate exists only once the host has run.
    if choices.console_cert {
        steps.push(Step {
            action: StepAction::TrustCert,
            ends_run: false,
        });
    }
    steps
}

/// What this run's components name (the family's three for a host, the client) plus anything
/// already installed, or `punktfunk-gamescope` stays on the channel the box just left. A
/// client-only switch never pulls the host in.
pub fn switch_pkgs(base: &[&str], installed: &[String], components: Components) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if components.host {
        out.extend(base.iter().map(|s| (*s).to_string()));
    }
    if components.client {
        out.push("punktfunk-client".to_string());
    }
    for pkg in installed {
        if !out.contains(pkg) {
            out.push(pkg.clone());
        }
    }
    out
}

/// One suffix per family so `Channel` does not fork the write path.
pub fn repo_channel_suffix(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "",
        Channel::Canary => "-canary",
    }
}
