//! Windows plan goldens: every `WinFacts` preset renders its `WinPlan` to
//! dry-run text under `tests/golden/win-*.txt`.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p punktfunk-setup`. Runs on
//! every OS — the plans are pure data.
//!
//! Named tests below assert on the command list and step order so they keep
//! meaning when wording moves. Traps live in `design/installer-v2-windows.md`.

use std::path::Path;

use punktfunk_setup::plan::Level;
use punktfunk_setup::platform::windows::args::InnoArgs;
use punktfunk_setup::platform::windows::choices::{NetworkAnswer, WinChoices};
use punktfunk_setup::platform::windows::plan::{self, Artifact, WinAction, WinPlan};
use punktfunk_setup::platform::windows::{
    exec, report, NetCategory, NetProfile, TaskState, WinFacts, WinInstall,
};
use punktfunk_setup::seam::Env;
use punktfunk_setup::ui::Plain;

fn fresh() -> WinFacts {
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

fn upgrade() -> WinFacts {
    WinFacts {
        installed: Some(WinInstall {
            version: Some("0.34.0".into()),
            location: Some(r"C:\Program Files\punktfunk\".into()),
        }),
        host_env_present: true,
        web_password_present: true,
        tray_autostart: true,
        vulkan_layer_registered: true,
        web_task: TaskState::Disabled,
        scripting_task: TaskState::Enabled,
        inno_uninstaller: true,
        ..fresh()
    }
}

fn sunshine() -> WinFacts {
    WinFacts {
        competing_hosts: vec!["SunshineService".into()],
        mgmt_port_in_use: true,
        ..fresh()
    }
}

fn public_network() -> WinFacts {
    WinFacts {
        networks: vec![NetProfile {
            name: "Netzwerk 2".into(),
            category: NetCategory::Public,
        }],
        ..fresh()
    }
}

fn render(facts: &WinFacts, choices: &WinChoices, artifact: Artifact, uninstall: bool) -> String {
    let (ui, buf) = Plain::capture();
    report::detected(&ui, facts, artifact);
    if !uninstall {
        report::choices_summary(&ui, choices, artifact);
    }
    let plan = plan::build(facts, choices, artifact, uninstall);
    exec::render(&plan, &ui);
    if !uninstall {
        report::outro(&ui, facts, choices, artifact);
    }
    buf.borrow().clone()
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("no golden for {name} — run UPDATE_GOLDEN=1 cargo test -p punktfunk-setup")
    });
    assert_eq!(
        actual, expected,
        "golden {name} changed (UPDATE_GOLDEN=1 to accept)"
    );
}

fn host_plan(facts: &WinFacts, choices: &WinChoices) -> WinPlan {
    plan::build(facts, choices, Artifact::Host, false)
}

#[test]
fn golden_win11_fresh() {
    let facts = fresh();
    let choices = WinChoices::derive(&facts, Artifact::Host);
    golden(
        "win-fresh",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_upgrade() {
    let facts = upgrade();
    let choices = WinChoices::derive(&facts, Artifact::Host);
    golden(
        "win-upgrade",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_sunshine() {
    let facts = sunshine();
    let choices = WinChoices::derive(&facts, Artifact::Host);
    golden(
        "win-sunshine",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_public_all_three_branches() {
    let facts = public_network();
    let mut choices = WinChoices::derive(&facts, Artifact::Host);
    golden(
        "win-public-skip",
        &render(&facts, &choices, Artifact::Host, false),
    );
    choices.network = NetworkAnswer::MakePrivate("Netzwerk 2".into());
    golden(
        "win-public-private",
        &render(&facts, &choices, Artifact::Host, false),
    );
    choices.network = NetworkAnswer::OpenPublicRules;
    choices.allow_public_fw = Some(true);
    golden(
        "win-public-open",
        &render(&facts, &choices, Artifact::Host, false),
    );
}

#[test]
fn golden_win11_uninstall() {
    let facts = upgrade();
    let choices = WinChoices::derive(&facts, Artifact::Host);
    golden(
        "win-uninstall",
        &render(&facts, &choices, Artifact::Host, true),
    );
}

#[test]
fn golden_client_fresh() {
    let facts = fresh();
    let choices = WinChoices::derive(&facts, Artifact::Client);
    golden(
        "win-client-fresh",
        &render(&facts, &choices, Artifact::Client, false),
    );
}

/// A Windows 10 box must be refused before anything moves, or it takes the whole install and
/// then fails every session on a virtual display that never starts. The plan holds NOTHING else:
/// a refusal that still deploys files is worse than none.
#[test]
fn a_host_install_below_the_os_floor_refuses_and_touches_nothing() {
    let facts = WinFacts {
        os_build: plan::MIN_HOST_BUILD - 1,
        ..fresh()
    };
    let choices = WinChoices::derive(&facts, Artifact::Host);
    let built = plan::build(&facts, &choices, Artifact::Host, false);
    let steps: Vec<&WinAction> = built.steps().collect();
    assert!(
        matches!(steps.as_slice(), [WinAction::Refuse(_)]),
        "{steps:?}"
    );

    // The client runs on Windows 10, and an uninstall must work whatever the box is.
    let client = WinChoices::derive(&facts, Artifact::Client);
    assert!(!plan::build(&facts, &client, Artifact::Client, false)
        .steps()
        .any(|s| matches!(s, WinAction::Refuse(_))));
    assert!(!plan::build(&facts, &choices, Artifact::Host, true)
        .steps()
        .any(|s| matches!(s, WinAction::Refuse(_))));
}

fn client_upgrade() -> WinFacts {
    WinFacts {
        client_installed: Some(WinInstall {
            version: Some("0.35.0".into()),
            location: Some(r"C:\Users\me\AppData\Local\Programs\Punktfunk".into()),
        }),
        ..fresh()
    }
}

#[test]
fn golden_client_uninstall() {
    let facts = client_upgrade();
    let choices = WinChoices::derive(&facts, Artifact::Client);
    golden(
        "win-client-uninstall",
        &render(&facts, &choices, Artifact::Client, true),
    );
}

/// Inno's uninstaller swept the `[Icons]` it had created; the engine creates them itself and
/// so owes the sweep. Every link the install can lay down is deleted, `desktop_icon` or not —
/// the box records that answer nowhere. A miss leaves a Start-menu tile aimed at a dead exe.
#[test]
fn the_client_uninstall_removes_every_shortcut_the_install_can_create() {
    let facts = fresh();
    let mut choices = WinChoices::derive(&facts, Artifact::Client);
    choices.desktop_icon = true;
    let created: Vec<String> = plan::build(&facts, &choices, Artifact::Client, false)
        .phases
        .iter()
        .flat_map(|p| &p.steps)
        .filter_map(|a| match a {
            WinAction::Shortcut { link, .. } => Some(link.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(created.len(), 3, "{created:?}");

    let deleted: Vec<String> = plan::build(&facts, &choices, Artifact::Client, true)
        .phases
        .iter()
        .flat_map(|p| &p.steps)
        .filter_map(|a| match a {
            WinAction::DeleteFiles { paths } => Some(paths.clone()),
            _ => None,
        })
        .flatten()
        .collect();
    for link in &created {
        assert!(deleted.contains(link), "{link} is never deleted");
    }
}

/// The host lays down one Start-menu link, every user's, starting the tray with `--start-host`,
/// and its uninstall removes it. No desktop link.
#[test]
fn the_host_start_menu_link_is_created_then_removed() {
    let links = |facts: &WinFacts, uninstall: bool| -> Vec<String> {
        let choices = WinChoices::derive(facts, Artifact::Host);
        plan::build(facts, &choices, Artifact::Host, uninstall)
            .steps()
            .filter_map(|a| match a {
                WinAction::Shortcut { link, target, args } => {
                    assert!(target.ends_with(r"\punktfunk-tray.exe"), "{target}");
                    assert_eq!(args, "--start-host");
                    Some(link.clone())
                }
                WinAction::DeleteFiles { paths } => paths.first().cloned(),
                _ => None,
            })
            .collect()
    };
    let link = r"<common start menu>\Punktfunk Host.lnk";
    assert_eq!(links(&fresh(), false), [link]);
    assert_eq!(links(&upgrade(), true), [link]);
}

/// Host and client are two products under two registry keys (D1). On a box where the HOST is
/// installed and the client is not, deriving the client's choices must read the CLIENT key:
/// `installed` is the host's, and taking it made the client a false upgrade pinned to
/// `C:\Program Files\punktfunk\`. Fresh facts cannot catch this — both keys are empty there.
#[test]
fn a_client_install_does_not_inherit_the_hosts_directory() {
    let facts = upgrade(); // host installed, client absent
    let client = WinChoices::derive(&facts, Artifact::Client);
    assert_eq!(
        client.dir, None,
        "client must not adopt the host's location"
    );
    let host = WinChoices::derive(&facts, Artifact::Host);
    assert!(host.dir.is_some(), "the host still pre-fills its own");
    // `upgrade` also drives the fresh-only defaults, so the client sees fresh ones.
    assert_eq!(client.gamestream, Some(false));
    assert_eq!(host.gamestream, None);
}

// WP3.2: the first upgrade over an Inno install retires Inno's uninstaller data AFTER our
// files (incl. the new unins000.exe) landed and BEFORE the ARP entry is rewritten in place
// — the same key, so `winget upgrade` keeps tracking the box.
#[test]
fn an_upgrade_over_inno_retires_its_uninstaller_between_files_and_arp() {
    let facts = upgrade();
    let plan = host_plan(&facts, &WinChoices::derive(&facts, Artifact::Host));
    let steps: Vec<&WinAction> = plan.steps().collect();
    let at = |pred: &dyn Fn(&WinAction) -> bool| steps.iter().position(|s| pred(s)).unwrap();
    let deploy = at(&|s| matches!(s, WinAction::DeployFiles { .. }));
    let retire = at(&|s| matches!(s, WinAction::DeleteFiles { .. }));
    let arp = at(&|s| matches!(s, WinAction::ArpRegister { .. }));
    assert!(deploy < retire && retire < arp);
    match steps[retire] {
        WinAction::DeleteFiles { paths } => assert_eq!(
            paths,
            &[
                r"C:\Program Files\punktfunk\unins000.dat".to_string(),
                r"C:\Program Files\punktfunk\unins000.msg".to_string(),
            ]
        ),
        other => panic!("{other:?}"),
    }
    // An upgrade over OUR installer has nothing to retire.
    let ours = WinFacts {
        inno_uninstaller: false,
        ..upgrade()
    };
    assert!(
        !host_plan(&ours, &WinChoices::derive(&ours, Artifact::Host))
            .steps()
            .any(|s| matches!(s, WinAction::DeleteFiles { .. }))
    );
}

// ------------------------------------------------------------------- named traps (§5, D11, D12)

// Coexistence is a `service install` flag, never an abort. The value is the Linux one.
#[test]
fn a_sunshine_box_coexists_by_moving_the_management_port() {
    let facts = sunshine();
    let plan = host_plan(&facts, &WinChoices::derive(&facts, Artifact::Host));
    assert!(plan
        .commands()
        .iter()
        .any(|c| c.contains("service install") && c.contains("--mgmt-bind=0.0.0.0:47991")));
}

#[test]
fn an_operator_mgmt_bind_is_never_rewritten() {
    let facts = WinFacts {
        mgmt_bind_set: true,
        ..sunshine()
    };
    let plan = host_plan(&facts, &WinChoices::derive(&facts, Artifact::Host));
    assert!(!plan.commands().iter().any(|c| c.contains("--mgmt-bind")));
}

// Silent installs never touch a network profile. Skip is a warning only.
#[test]
fn silent_with_a_public_network_warns_but_never_touches_the_profile() {
    let facts = public_network();
    let plan = host_plan(&facts, &WinChoices::derive(&facts, Artifact::Host));
    assert!(!plan
        .steps()
        .any(|s| matches!(s, WinAction::MakeNetworkPrivate { .. })));
    assert!(plan
        .steps()
        .any(|s| matches!(s, WinAction::Note(Level::Warn, text) if text.contains("Public"))));
}

#[test]
fn service_install_params_are_fresh_only_by_default() {
    let fresh_cmds = host_plan(&fresh(), &WinChoices::derive(&fresh(), Artifact::Host)).commands();
    let install = fresh_cmds
        .iter()
        .find(|c| c.contains("service install"))
        .unwrap();
    assert!(install.contains("--gamestream=off"));
    assert!(install.contains("--allow-public-network=off"));

    let up_cmds = host_plan(&upgrade(), &WinChoices::derive(&upgrade(), Artifact::Host)).commands();
    let install = up_cmds
        .iter()
        .find(|c| c.contains("service install"))
        .unwrap();
    assert!(!install.contains("--gamestream"));
    assert!(!install.contains("--allow-public-network"));
}

// Omitting the flags on upgrade is not a no-op when the operator asked.
#[test]
fn an_explicit_public_fw_task_reaches_an_upgrade_plan() {
    let facts = upgrade();
    let mut choices = WinChoices::derive(&facts, Artifact::Host);
    let args = InnoArgs::parse(&[r#"/MERGETASKS="allowpublicfw""#.to_string()]);
    choices.apply(&args, &Env::default());
    let cmds = host_plan(&facts, &choices).commands();
    let install = cmds.iter().find(|c| c.contains("service install")).unwrap();
    assert!(install.contains("--allow-public-network=on"));
}

// Service first — it supervises the tray — then tray, then all three driver legs.
#[test]
fn uninstall_order_is_service_tray_then_all_three_driver_legs() {
    let facts = upgrade();
    let cmds = plan::build(
        &facts,
        &WinChoices::derive(&facts, Artifact::Host),
        Artifact::Host,
        true,
    )
    .commands();
    let pos = |needle: &str| {
        cmds.iter()
            .position(|c| c.contains(needle))
            .unwrap_or_else(|| panic!("missing: {needle}"))
    };
    assert!(pos("service uninstall") < pos("--quit"));
    assert!(pos("--quit") < pos("taskkill"));
    assert!(pos("taskkill") < pos("driver uninstall"));
    assert!(pos("driver uninstall --gamepad") < pos("driver uninstall --audio"));
}

// Restore carries the pre-install task states, so an upgrade puts back what was there.
#[test]
fn restore_carries_the_pre_install_task_states() {
    let facts = upgrade();
    let plan = host_plan(&facts, &WinChoices::derive(&facts, Artifact::Host));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::RestoreTasks {
            web_enabled: Some(false),
            scripting_enabled: Some(true),
        }
    )));
    let fresh_plan = host_plan(&fresh(), &WinChoices::derive(&fresh(), Artifact::Host));
    assert!(!fresh_plan.steps().any(|s| matches!(
        s,
        WinAction::StopHostRuntime { .. } | WinAction::RestoreTasks { .. }
    )));
}

// The re-created runner task comes back enabled: an operator's off is put back, not started.
#[test]
fn an_upgrade_keeps_a_switched_off_runner_off() {
    let facts = WinFacts {
        scripting_task: TaskState::Disabled,
        ..upgrade()
    };
    let text = render(
        &facts,
        &WinChoices::derive(&facts, Artifact::Host),
        Artifact::Host,
        false,
    );
    assert!(text.contains("schtasks /Change /TN PunktfunkScripting /DISABLE"));
    assert!(!text.contains("schtasks /Run /TN PunktfunkScripting"));
}

// Unchecking a row on upgrade deletes it. Inno's unchecked box is a no-op; do not copy that.
#[test]
fn deselecting_tray_on_an_upgrade_deletes_the_run_key() {
    let facts = upgrade();
    let mut choices = WinChoices::derive(&facts, Artifact::Host);
    choices.tray_autostart = false;
    let cmds = host_plan(&facts, &choices).commands();
    assert!(cmds
        .iter()
        .any(|c| c.starts_with("reg delete") && c.contains("PunktfunkTray")));
}

#[test]
fn web_password_file_is_fresh_only() {
    let plan = host_plan(&fresh(), &WinChoices::derive(&fresh(), Artifact::Host));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::WebSetup {
            fresh_password: true,
            ..
        }
    )));
    let plan = host_plan(&upgrade(), &WinChoices::derive(&upgrade(), Artifact::Host));
    assert!(plan.steps().any(|s| matches!(
        s,
        WinAction::WebSetup {
            fresh_password: false,
            ..
        }
    )));
}
