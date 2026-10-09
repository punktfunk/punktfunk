//! Stage three, Windows: `(WinFacts, WinChoices) → WinPlan`. Pure — no I/O, no spawns.
//!
//! Own types, not an extension of Linux `plan.rs`: Linux `StepAction` threads
//! `Facts`/`Choices` and is golden-locked. Same `Reporter` vocabulary and golden
//! harness (`design/installer-v2-windows.md`).
//!
//! Every path in a step is a literal string, never a `PathBuf::join` — goldens
//! must render byte-identically on every OS. Phase order is load-bearing:
//! stop → files → registry → network → coexistence → drivers →
//! service → web → plugin runner → restore → shortcut → tray. `<staging>` and `<temp>`
//! are placeholders the executor substitutes; dry-run renders them verbatim.

use serde::{Deserialize, Serialize};

use super::choices::{NetworkAnswer, WinChoices};
use super::{TaskState, WinFacts};
use crate::choices::DEFAULT_MGMT_PORT;
use crate::facts::DOCS;
use crate::plan::Level;

/// Default without `/DIR`. `pf-update-check` classifies by this path; a different
/// dir silently downgrades the box to notify-only updates.
pub const DEFAULT_HOST_DIR: &str = r"C:\Program Files\punktfunk";

/// Rendered, not resolved: the executor expands the per-user profile path.
pub const DEFAULT_CLIENT_DIR: &str = r"%LocalAppData%\Programs\Punktfunk";

/// Client ARP key. Keep Inno's `_is1`: winget ProductCode tracks this exact name.
pub const CLIENT_ARP_KEY: &str = r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{52464E61-68A1-4621-B6B3-5B8BBB823D1A}_is1";

/// The host's OS floor. pf-vdisplay is built against IddCx 1.10, which first shipped in
/// Windows 11 22H2: below this the driver package installs and the device never starts.
/// The client has no floor — it runs on Windows 10.
pub const MIN_HOST_BUILD: u32 = 22621;

/// Every link the client install can lay down. The uninstall deletes all three unconditionally:
/// `desktop_icon` is recorded nowhere on the box, and a link that was never made is a no-op.
const CLIENT_LINKS: [&str; 3] = [
    r"<start menu>\Punktfunk.lnk",
    r"<start menu>\Punktfunk Console.lnk",
    r"<desktop>\Punktfunk.lnk",
];

/// The host's one link, in every user's Start menu: the tray, which starts a stopped host
/// behind one UAC prompt. No desktop link.
const HOST_LINK: &str = r"<common start menu>\Punktfunk Host.lnk";

/// Anything in `{app}` that can hold the tree open. Install stops them to replace the files,
/// uninstall to delete them.
const CLIENT_EXES: [&str; 4] = [
    "punktfunk-client.exe",
    "punktfunk-session.exe",
    "punktfunk-console.exe",
    "punktfunk.exe",
];

/// Which payload this exe carries; the embedded manifest decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Artifact {
    Host,
    Client,
}

/// A step. `Run` is an argv, never a shell string — there is no `sh`, and
/// re-deriving argv from a display string is a quoting bug farm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WinAction {
    Run(Vec<String>),
    /// Same spawn; a non-zero exit is fine. Absence (no process, no task, no key) is the goal.
    RunLenient(Vec<String>),
    /// Stop the run before it touches the box. A dry run renders it and carries on.
    Refuse(String),
    Note(Level, String),
    DeployFiles {
        dest: String,
    },
    /// The uninstaller lives in `dir`; the executor owns the self-delete. The plan states intent.
    RemoveFiles {
        dir: String,
    },
    /// Delete specific files; absent is fine (the client's shortcuts, the WP3.2 Inno leftovers).
    /// Paths may carry the `<start menu>` placeholders — the executor substitutes them.
    DeleteFiles {
        paths: Vec<String>,
    },
    /// Containment-checked PATH append / rebuild. `REG_EXPAND_SZ`, never a substring
    /// delete. HKLM when `machine`, else HKCU.
    PathAdd {
        machine: bool,
        dir: String,
    },
    PathRemove {
        machine: bool,
        dir: String,
    },
    /// ARP entry. The key name is frozen: winget ProductCode.
    ArpRegister {
        key: String,
        display_name: String,
        version: String,
        location: String,
    },
    ArpRemove {
        key: String,
    },
    /// A `.lnk` via `IShellLink` — no compiled tool writes one. `args` empty = none.
    Shortcut {
        link: String,
        target: String,
        args: String,
    },
    /// Flip one network to Private. Wizard consent only; never in a silent run.
    MakeNetworkPrivate {
        network: String,
    },
    /// Stop the service (SCM, waited), every tray, the bun tasks, and every bun whose image
    /// lives under `app_dir`, waited on so the copy that follows finds nothing mapped.
    /// Captures nothing: Facts already hold the restore data.
    StopHostRuntime {
        app_dir: String,
    },
    /// Re-enable only what was enabled before the stop. `None` = the task did not exist.
    RestoreTasks {
        web_enabled: Option<bool>,
        scripting_enabled: Option<bool>,
    },
    /// The password travels in an ACL'd temp file, never argv.
    WebSetup {
        app_dir: String,
        fresh_password: bool,
    },
    /// Restart backoff needs task XML (inexpressible in flags); the executor writes it.
    /// `start_now` only on a fresh registration.
    RegisterScriptingTask {
        app_dir: String,
        start_now: bool,
    },
    /// Non-elevated, current user, skipped in silent installs (the host supervises one).
    LaunchTray {
        exe: String,
    },
    /// Best-effort: a missing runtime downloads `windowsappruntimeinstall --quiet`.
    /// Failure warns and points at the docs; it never aborts.
    EnsureAppRuntime {
        arch: String,
    },
    /// Reap listeners a stop may have missed (the legacy console migration).
    KillPortListeners {
        ports: Vec<u16>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinPhase {
    pub title: String,
    pub steps: Vec<WinAction>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WinPlan {
    pub phases: Vec<WinPhase>,
}

impl WinPlan {
    pub fn steps(&self) -> impl Iterator<Item = &WinAction> {
        self.phases.iter().flat_map(|p| p.steps.iter())
    }

    /// Argv this plan would spawn, joined for display. Trap tests assert on this string.
    pub fn commands(&self) -> Vec<String> {
        self.steps()
            .filter_map(|s| match s {
                WinAction::Run(argv) | WinAction::RunLenient(argv) => Some(join_argv(argv)),
                _ => None,
            })
            .collect()
    }

    fn push(&mut self, title: impl Into<String>, steps: Vec<WinAction>) {
        let steps: Vec<WinAction> = steps;
        if steps.is_empty() {
            return;
        }
        self.phases.push(WinPhase {
            title: title.into(),
            steps,
        });
    }
}

/// Args with spaces are quoted for display; execution uses the vector, not this string.
pub fn join_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn run(argv: &[&str]) -> WinAction {
    WinAction::Run(argv.iter().map(|s| (*s).to_string()).collect())
}

fn run_lenient(argv: &[&str]) -> WinAction {
    WinAction::RunLenient(argv.iter().map(|s| (*s).to_string()).collect())
}

fn note(level: Level, text: impl Into<String>) -> WinAction {
    WinAction::Note(level, text.into())
}

pub fn build(
    facts: &WinFacts,
    choices: &WinChoices,
    artifact: Artifact,
    uninstall: bool,
) -> WinPlan {
    match (artifact, uninstall) {
        (Artifact::Host, false) => host_install(facts, choices),
        (Artifact::Host, true) => host_uninstall(facts, choices),
        (Artifact::Client, false) => client_install(facts, choices),
        (Artifact::Client, true) => client_uninstall(choices),
    }
}

fn app_dir(choices: &WinChoices, artifact: Artifact) -> String {
    choices
        .dir
        .as_ref()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| {
            match artifact {
                Artifact::Host => DEFAULT_HOST_DIR,
                Artifact::Client => DEFAULT_CLIENT_DIR,
            }
            .to_string()
        })
        .trim_end_matches('\\')
        .to_string()
}

fn host_install(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    let mut plan = WinPlan::default();
    // Before anything else, and before any file moves: an older Windows takes the whole install
    // and then fails every session on a driver that will not start.
    if facts.os_build < MIN_HOST_BUILD {
        plan.push(
            "Checking Windows",
            vec![WinAction::Refuse(format!(
                "The Punktfunk host needs {} — this PC reports build {}, where the virtual \
                 display can't start. Nothing was installed.",
                crate::platform::floor("windows"),
                facts.os_build
            ))],
        );
        return plan;
    }
    let app = app_dir(choices, Artifact::Host);
    let host_exe = format!("{app}\\punktfunk-host.exe");
    let upgrade = facts.installed.is_some();

    if upgrade {
        plan.push(
            "Stopping the running host for the upgrade",
            vec![WinAction::StopHostRuntime {
                app_dir: app.clone(),
            }],
        );
    }

    plan.push(
        format!("Files → {app}"),
        vec![WinAction::DeployFiles { dest: app.clone() }],
    );

    // WP3.2: the first upgrade over an Inno install. Our unins000.exe already replaced
    // Inno's in the files phase; its data files go, and the ARP entry below is rewritten in
    // place under the same key — never Inno's uninstaller (D6).
    if upgrade && facts.inno_uninstaller {
        plan.push(
            "Retiring the Inno Setup uninstaller",
            vec![
                WinAction::DeleteFiles {
                    paths: vec![
                        format!("{app}\\unins000.dat"),
                        format!("{app}\\unins000.msg"),
                    ],
                },
                note(
                    Level::Ok,
                    "the Add/Remove Programs entry keeps its key — winget and the updater keep tracking this install",
                ),
            ],
        );
    }

    // Right after the files: whatever fails later, the box has an uninstaller registered and
    // the version the tree now holds — the updater would otherwise re-run this forever.
    plan.push(
        "Register the uninstaller",
        vec![WinAction::ArpRegister {
            key: super::HOST_ARP_KEY.into(),
            display_name: "Punktfunk Host".into(),
            version: "<version>".into(),
            location: app.clone(),
        }],
    );

    plan.push("Registry", registry_steps(facts, choices, &app));
    plan.push("Network", network_steps(facts, choices));
    plan.push(
        "Checking for Sunshine / Apollo / Vibeshine",
        coexist_steps(facts, choices),
    );

    // Lenient, as the title promises: a driver leg that fails must not strand the box between
    // the stopped old host and the service/uninstaller legs below.
    let mut drivers = vec![];
    if choices.install_driver {
        drivers.push(run_lenient(&[
            &host_exe,
            "driver",
            "install",
            "--dir",
            r"<staging>\pfvdisplay",
        ]));
    }
    if choices.install_gamepad {
        drivers.push(run_lenient(&[
            &host_exe,
            "driver",
            "install",
            "--gamepad",
            "--dir",
            r"<staging>\gamepad",
        ]));
    }
    if upgrade && !choices.install_driver {
        // The host and its driver ship as a pair; a host that outruns the driver fails every
        // session with "driver outdated", and nothing else says why.
        drivers.push(note(
            Level::Warn,
            "the virtual-display driver is NOT being updated with this host — the two must match, or every session ends with 'driver outdated'",
        ));
    }
    plan.push("Drivers (a hiccup warns and never aborts)", drivers);

    // `None` omits the flag and the box keeps its state. Only a `Some` rewrites.
    let mut service = vec![host_exe.clone(), "service".into(), "install".into()];
    if let Some(on) = choices.gamestream {
        service.push(format!("--gamestream={}", if on { "on" } else { "off" }));
    }
    if let Some(on) = choices.allow_public_fw {
        service.push(format!(
            "--allow-public-network={}",
            if on { "on" } else { "off" }
        ));
    }
    // The host writes host.env and opens the firewall for the port in one place.
    if facts.needs_coexistence() {
        service.push(format!("--mgmt-bind=0.0.0.0:{DEFAULT_MGMT_PORT}"));
    }
    if let Some(addr) = &choices.web_bind {
        service.push(format!("--web-bind={addr}"));
    }
    let mut service_steps = vec![WinAction::Run(service)];
    if choices.start_service {
        service_steps.push(run(&[&host_exe, "service", "start"]));
    }
    plan.push("Service", service_steps);

    plan.push(
        "Web console",
        vec![WinAction::WebSetup {
            app_dir: app.clone(),
            fresh_password: !facts.web_password_present,
        }],
    );

    plan.push(
        "Plugin runner",
        vec![WinAction::RegisterScriptingTask {
            app_dir: app.clone(),
            start_now: facts.scripting_task == TaskState::Absent,
        }],
    );

    if upgrade {
        plan.push(
            "Restoring what the stop disabled",
            vec![WinAction::RestoreTasks {
                web_enabled: enabled(facts.web_task),
                scripting_enabled: enabled(facts.scripting_task),
            }],
        );
    }

    plan.push(
        "Shortcut",
        vec![WinAction::Shortcut {
            link: HOST_LINK.into(),
            target: format!("{app}\\punktfunk-tray.exe"),
            args: "--start-host".into(),
        }],
    );

    if choices.tray_autostart {
        plan.push(
            "Tray",
            vec![WinAction::LaunchTray {
                exe: format!("{app}\\punktfunk-tray.exe"),
            }],
        );
    }

    // Strict and last: the driver leg only warns, and a host that meets the old driver refuses
    // every session. Failing here leaves every other leg done.
    if choices.install_driver {
        plan.push(
            "Checking the virtual display driver",
            vec![run(&[&host_exe, "driver", "check"])],
        );
    }
    plan
}

fn enabled(state: TaskState) -> Option<bool> {
    match state {
        TaskState::Absent => None,
        TaskState::Enabled => Some(true),
        TaskState::Disabled => Some(false),
    }
}

fn registry_steps(facts: &WinFacts, choices: &WinChoices, app: &str) -> Vec<WinAction> {
    // The host owns the tray's `Run` value: its `settings set` stores the choice, where the
    // console can change it, and writes or deletes the value to match.
    let mut steps = vec![run_lenient(&[
        &format!("{app}\\punktfunk-host.exe"),
        "settings",
        "set",
        "tray_autostart",
        &choices.tray_autostart.to_string(),
    ])];
    // Toast AUMID; must stay in lockstep with `punktfunk-tray/src/win.rs`.
    let aumid = r"HKLM\SOFTWARE\Classes\AppUserModelId\unom.punktfunk.tray";
    steps.push(run(&[
        "reg",
        "add",
        aumid,
        "/v",
        "DisplayName",
        "/t",
        "REG_SZ",
        "/d",
        "Punktfunk",
        "/f",
    ]));
    steps.push(run(&[
        "reg",
        "add",
        aumid,
        "/v",
        "IconUri",
        "/t",
        "REG_SZ",
        "/d",
        &format!("{app}\\punktfunk.ico"),
        "/f",
    ]));
    let layers = r"HKLM\SOFTWARE\Khronos\Vulkan\ImplicitLayers";
    let layer_json = format!("{app}\\vklayer\\pf_vkhdr_layer.json");
    if choices.install_hdr_layer {
        steps.push(run(&[
            "reg",
            "add",
            layers,
            "/v",
            &layer_json,
            "/t",
            "REG_DWORD",
            "/d",
            "0",
            "/f",
        ]));
    } else if facts.vulkan_layer_registered {
        steps.push(run_lenient(&[
            "reg",
            "delete",
            layers,
            "/v",
            &layer_json,
            "/f",
        ]));
    }
    steps.push(WinAction::PathAdd {
        machine: true,
        dir: app.to_string(),
    });
    steps
}

/// Silent installs never touch a profile: Skip renders the warning and stops.
fn network_steps(facts: &WinFacts, choices: &WinChoices) -> Vec<WinAction> {
    match &choices.network {
        NetworkAnswer::MakePrivate(name) => vec![
            WinAction::MakeNetworkPrivate {
                network: name.clone(),
            },
            note(
                Level::Ok,
                format!("network '{name}' becomes Private — the standard firewall rules apply there"),
            ),
        ],
        NetworkAnswer::OpenPublicRules => vec![note(
            Level::Ok,
            "keeping the network Public and opening the firewall for it (--allow-public-network=on)",
        )],
        NetworkAnswer::Skip => {
            let public: Vec<String> = facts
                .public_networks()
                .iter()
                .map(|n| n.name.clone())
                .collect();
            if public.is_empty() || choices.allow_public_fw == Some(true) {
                return vec![];
            }
            vec![note(
                Level::Warn,
                format!(
                    "network '{}' is set to Public, and the firewall rules below don't apply there — the host will be unreachable until the network is Private or the public rules are added ({DOCS}/troubleshooting#windows-firewall)",
                    public.join("', '")
                ),
            )]
        }
    }
}

/// Mirror of the Linux conflict steps; same wording where the situation matches.
fn coexist_steps(facts: &WinFacts, choices: &WinChoices) -> Vec<WinAction> {
    if !facts.needs_coexistence() {
        if facts.mgmt_bind_set {
            return vec![note(
                Level::Ok,
                "management port already moved in host.env — leaving the operator's value alone",
            )];
        }
        return vec![note(
            Level::Ok,
            "No conflicting game-streaming host detected.",
        )];
    }
    let who = facts
        .competing_hosts
        .first()
        .map(String::as_str)
        .unwrap_or("another streaming host");
    vec![
        note(
            Level::Warn,
            format!("{who} is active on this box — both want TCP 47990 (its web UI, punktfunk's management API)"),
        ),
        note(
            Level::Ok,
            format!("PUNKTFUNK_MGMT_BIND=0.0.0.0:{DEFAULT_MGMT_PORT} — the service step writes it to host.env and opens the firewall for it"),
        ),
        note(
            Level::Ok,
            format!("Clients learn the port from discovery; the console and plugins read it from mgmt-endpoint. Details: {DOCS}/switching-from-sunshine"),
        ),
        if choices.gamestream == Some(true) {
            note(
                Level::Warn,
                "with another GameStream host running, only one can bind the Moonlight ports — stop the other first or skip this",
            )
        } else {
            note(
                Level::Ok,
                "Moonlight compatibility stays off — the other host owns those ports while it runs",
            )
        },
    ]
}

/// `[UninstallRun]` order, verbatim. Each position is load-bearing.
fn host_uninstall(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    let app = facts
        .installed
        .as_ref()
        .and_then(|i| i.location.clone())
        .unwrap_or_else(|| app_dir(choices, Artifact::Host))
        .trim_end_matches('\\')
        .to_string();
    let host_exe = format!("{app}\\punktfunk-host.exe");
    let mut plan = WinPlan::default();
    plan.push(
        format!("Uninstalling the host ({DOCS}/uninstall)"),
        vec![
            // Service first: the host supervises the tray and would respawn it. Lenient: a
            // service already gone (or a deleted {app}) must not stop the sweep below.
            run_lenient(&[&host_exe, "service", "uninstall"]),
            run_lenient(&[&format!("{app}\\punktfunk-tray.exe"), "--quit"]),
            run_lenient(&["taskkill", "/F", "/IM", "punktfunk-tray.exe"]),
            // All three legs even if this install never laid them down — an earlier
            // upgrade may have. `driver uninstall` also purges the trusted certs.
            run_lenient(&[&host_exe, "driver", "uninstall"]),
            run_lenient(&[&host_exe, "driver", "uninstall", "--gamepad"]),
            run_lenient(&[&host_exe, "driver", "uninstall", "--audio"]),
            run_lenient(&["schtasks", "/End", "/TN", "PunktfunkWeb"]),
            run_lenient(&["schtasks", "/Delete", "/TN", "PunktfunkWeb", "/F"]),
            run_lenient(&["schtasks", "/End", "/TN", "PunktfunkScripting"]),
            run_lenient(&["schtasks", "/Delete", "/TN", "PunktfunkScripting", "/F"]),
            WinAction::KillPortListeners {
                ports: vec![47992, 3000],
            },
            run_lenient(&[
                "netsh", "advfirewall", "firewall", "delete", "rule",
                "name=Punktfunk web console (TCP 47992)",
            ]),
            run_lenient(&[
                "netsh", "advfirewall", "firewall", "delete", "rule",
                "name=Punktfunk plugin UIs (TCP 47993)",
            ]),
            WinAction::PathRemove {
                machine: true,
                dir: app.clone(),
            },
            WinAction::ArpRemove {
                key: super::HOST_ARP_KEY.into(),
            },
            // The tray autostart, its toast AUMID, the HDR layer's registration. Lenient —
            // a Custom install may never have laid one down.
            run_lenient(&[
                "reg",
                "delete",
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "PunktfunkTray",
                "/f",
            ]),
            run_lenient(&[
                "reg",
                "delete",
                r"HKLM\SOFTWARE\Classes\AppUserModelId\unom.punktfunk.tray",
                "/f",
            ]),
            run_lenient(&[
                "reg",
                "delete",
                r"HKLM\SOFTWARE\Khronos\Vulkan\ImplicitLayers",
                "/v",
                &format!("{app}\\vklayer\\pf_vkhdr_layer.json"),
                "/f",
            ]),
            WinAction::DeleteFiles {
                paths: vec![HOST_LINK.into()],
            },
            WinAction::RemoveFiles { dir: app.clone() },
            note(
                Level::Ok,
                r"kept on purpose: %ProgramData%\punktfunk (identity, passwords, update cache) and any VB-CABLE install",
            ),
        ],
    );
    plan
}

fn client_install(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    let mut plan = WinPlan::default();
    let app = app_dir(choices, Artifact::Client);
    let client_exe = format!("{app}\\punktfunk-client.exe");

    plan.push(
        "Stopping running punktfunk apps",
        CLIENT_EXES
            .iter()
            .map(|exe| run_lenient(&["taskkill", "/F", "/IM", exe]))
            .collect(),
    );
    plan.push(
        format!("Files → {app}"),
        vec![
            WinAction::DeployFiles { dest: app.clone() },
            // The firewall service (LOCAL SERVICE) reads the exe to name it in its prompt; a
            // profile dir denies it that. Lenient: a refused grant costs only the name.
            run_lenient(&["icacls", &app, "/grant", "*S-1-5-19:(OI)(CI)RX", "/Q"]),
        ],
    );

    let proto = r"HKCU\Software\Classes\punktfunk";
    let mut registry = vec![
        run(&[
            "reg",
            "add",
            proto,
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            "URL:Punktfunk stream link",
            "/f",
        ]),
        run(&[
            "reg",
            "add",
            proto,
            "/v",
            "URL Protocol",
            "/t",
            "REG_SZ",
            "/d",
            "",
            "/f",
        ]),
        run(&[
            "reg",
            "add",
            &format!(r"{proto}\DefaultIcon"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &format!("{client_exe},0"),
            "/f",
        ]),
        run(&[
            "reg",
            "add",
            &format!(r"{proto}\shell\open\command"),
            "/ve",
            "/t",
            "REG_SZ",
            "/d",
            &format!("\"{client_exe}\" \"%1\""),
            "/f",
        ]),
        WinAction::PathAdd {
            machine: false,
            dir: app.clone(),
        },
    ];
    plan.push("Registry", std::mem::take(&mut registry));

    let [start, console, desktop] = CLIENT_LINKS;
    let mut shortcuts = vec![WinAction::Shortcut {
        link: start.into(),
        target: client_exe.clone(),
        args: String::new(),
    }];
    // The ARM64 session is built without the console (no Skia for the target), so its
    // shortcut would open nothing.
    if facts.arch != "arm64" {
        shortcuts.push(WinAction::Shortcut {
            link: console.into(),
            target: format!("{app}\\punktfunk-console.exe"),
            args: String::new(),
        });
    }
    if choices.desktop_icon {
        shortcuts.push(WinAction::Shortcut {
            link: desktop.into(),
            target: client_exe.clone(),
            args: String::new(),
        });
    }
    plan.push("Shortcuts", shortcuts);

    plan.push(
        "Windows App Runtime",
        vec![WinAction::EnsureAppRuntime {
            arch: facts.arch.clone(),
        }],
    );
    plan.push(
        "Register the uninstaller",
        vec![WinAction::ArpRegister {
            key: CLIENT_ARP_KEY.into(),
            display_name: "Punktfunk".into(),
            version: "<version>".into(),
            location: app,
        }],
    );
    plan
}

fn client_uninstall(choices: &WinChoices) -> WinPlan {
    let app = app_dir(choices, Artifact::Client);
    let mut plan = WinPlan::default();
    plan.push(
        format!("Uninstalling the client ({DOCS}/uninstall)"),
        CLIENT_EXES
            .iter()
            .map(|exe| run_lenient(&["taskkill", "/F", "/IM", exe]))
            .chain([
                run_lenient(&["reg", "delete", r"HKCU\Software\Classes\punktfunk", "/f"]),
                // Inno tracked its `[Icons]` and swept them for free; the engine lays them
                // down itself, so it owes the same sweep — a link left behind points at a
                // deleted exe.
                WinAction::DeleteFiles {
                    paths: CLIENT_LINKS.iter().map(|l| (*l).to_string()).collect(),
                },
                WinAction::PathRemove {
                    machine: false,
                    dir: app.clone(),
                },
                WinAction::ArpRemove {
                    key: CLIENT_ARP_KEY.into(),
                },
                WinAction::RemoveFiles { dir: app },
            ])
            .collect(),
    );
    plan
}

#[cfg(test)]
mod tests {
    use super::super::choices::WinChoices;
    use super::*;

    fn facts(arch: &str) -> WinFacts {
        WinFacts {
            arch: arch.into(),
            ..crate::fixtures::fresh_win()
        }
    }

    fn console_shortcut(arch: &str) -> bool {
        let f = facts(arch);
        let choices = WinChoices::derive(&f, Artifact::Client);
        build(&f, &choices, Artifact::Client, false)
            .steps()
            .any(|s| matches!(s, WinAction::Shortcut { link, .. } if link == CLIENT_LINKS[1]))
    }

    /// The ARM64 session has no console, so its install offers no shortcut to one.
    #[test]
    fn only_a_build_with_the_console_links_it() {
        assert!(console_shortcut("x64"));
        assert!(!console_shortcut("arm64"));
    }
}
