//! Golden dry-run transcripts for every Facts preset, pinned under `tests/golden/*.txt`.
//!
//! Regenerate with `UPDATE_GOLDEN=1 cargo test -p punktfunk-setup`.
//!
//! Goldens embed `data/platforms.json` install lines. An edit there must fail this suite
//! until the golden is accepted — that is the drift alarm.
//!
//! Named `trap_*` tests assert on the command list, not the rendering, so they stay
//! meaningful when the text moves. See `design/installer-v2.md`.

use std::path::Path;

use punktfunk_setup::choices::{Action, Choices, Pins};
use punktfunk_setup::exec::{Executor, Opts};
use punktfunk_setup::facts::{Channel, Facts, Family, Firewall, Nvidia, OsRelease};
use punktfunk_setup::plan::{self, Phase, Plan, PlanPhase, Step, StepAction};
use punktfunk_setup::report;
use punktfunk_setup::seam::{BasePaths, FakeRunner};
use punktfunk_setup::ui::Plain;

/// Baseline: no punktfunk packages. Other presets overlay this so a golden names one change.
fn fresh(id: &str, family: Family) -> Facts {
    let docs = match family {
        Family::Apt if id == "ubuntu" => "ubuntu",
        Family::Apt => "debian",
        Family::Dnf => "fedora",
        Family::Sysext => "bazzite",
        Family::Pacman if id == "omarchy" => "omarchy",
        Family::Pacman => "arch",
        Family::Steamos => "steamos-host",
        Family::Flatpak => "install",
    };
    Facts {
        os: OsRelease {
            id: id.to_string(),
            id_like: String::new(),
            version_id: String::new(),
            pretty: id.to_string(),
        },
        family,
        omarchy: id == "omarchy",
        docs_page: format!("https://docs.punktfunk.unom.io/docs/{docs}"),
        host_punt: None,
        has_flatpak_client: false,
        rpm_group: (family == Family::Dnf).then(|| "fedora-44".to_string()),
        floor: None,
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
        // Nothing installed, so no unit: the start phase has to reason from the install.
        web_unit_present: false,
        web_password_present: false,
        web_bind: None,
        mgmt_bind: None,
        ip: Some("192.168.1.10".into()),
        user: "pf".into(),
    }
}

fn installed(id: &str, family: Family, channel: Channel) -> Facts {
    let pkgs = match family {
        Family::Dnf => vec!["punktfunk", "punktfunk-web", "punktfunk-scripting"],
        Family::Sysext => vec![],
        _ => vec!["punktfunk-host", "punktfunk-web", "punktfunk-scripting"],
    };
    Facts {
        missing: vec![],
        installed_pf: pkgs.into_iter().map(str::to_string).collect(),
        current_channel: Some(channel),
        host_version: Some("punktfunk-host 0.34.0".into()),
        has_web_server: true,
        web_unit_present: true,
        web_password_present: true,
        web_bind: None,
        mgmt_bind: None,
        in_input_group: true,
        ..fresh(id, family)
    }
}

fn pins() -> Pins {
    Pins::default()
}

/// `--dry-run` text without the constant banner.
fn render(facts: &Facts, choices: &Choices) -> String {
    let (ui, buf) = Plain::capture();
    let paths = BasePaths::rooted(Path::new("/box"));
    let run =
        FakeRunner::new()
            .with_path("systemctl")
            .answer("systemctl --user show-environment", 0, "");
    // The goldens carry platforms.json's lines in their no-confirm form — the rewrite is a
    // fixed prefix table with its own test in exec, so a platforms.json edit still shows here.
    let opts = Opts {
        dry: true,
        quiet: false,
        tty: false,
    };

    report::detected(&ui, facts);
    report::choices_summary(&ui, choices);
    let plan = plan::build(facts, choices);
    let exec = Executor {
        paths: &paths,
        run: &run,
        ui: &ui,
        opts,
    };
    let outcome = exec
        .execute(&plan, facts, choices)
        .expect("a dry run cannot fail");
    if choices.action == Action::Uninstall {
        report::uninstall_outro(&ui);
    } else {
        report::verify(&ui, &run, facts, choices, &outcome, opts);
    }
    buf.borrow().clone()
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("golden dir")).expect("create golden dir");
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

fn check(name: &str, facts: &Facts, pins: &Pins) {
    let choices = Choices::derive(facts, pins);
    golden(name, &render(facts, &choices));
}

fn plan_for(facts: &Facts, pins: &Pins) -> Plan {
    plan::build(facts, &Choices::derive(facts, pins))
}

#[test]
fn fresh_installs() {
    check("arch-fresh", &fresh("arch", Family::Pacman), &pins());
    check("debian-fresh", &fresh("debian", Family::Apt), &pins());
    check("fedora-fresh", &fresh("fedora", Family::Dnf), &pins());
    check("bazzite-couch", &fresh("bazzite", Family::Sysext), &pins());
    check("omarchy-fresh", &fresh("omarchy", Family::Pacman), &pins());
    check("steamos-fresh", &fresh("steamos", Family::Steamos), &pins());
}

#[test]
fn fresh_installs_on_canary() {
    let canary = Pins {
        channel: Some(Channel::Canary),
        ..pins()
    };
    check("arch-fresh-canary", &fresh("arch", Family::Pacman), &canary);
    check(
        "debian-fresh-canary",
        &fresh("debian", Family::Apt),
        &canary,
    );
    check(
        "fedora-fresh-canary",
        &fresh("fedora", Family::Dnf),
        &canary,
    );
    check(
        "bazzite-fresh-canary",
        &fresh("bazzite", Family::Sysext),
        &canary,
    );
    check(
        "steamos-fresh-canary",
        &fresh("steamos", Family::Steamos),
        &canary,
    );
}

/// An installed Deck: its binaries are never on PATH, so it is never "fully installed".
fn deck_on(channel: Channel) -> Facts {
    Facts {
        current_channel: Some(channel),
        web_unit_present: true,
        ..fresh("steamos", Family::Steamos)
    }
}

/// `rpm_group = "bazzite"` is a sed of the written repo file, not the Bazzite distro.
#[test]
fn fedora_43_uses_the_bazzite_rpm_group() {
    let mut facts = fresh("fedora", Family::Dnf);
    facts.rpm_group = Some("bazzite".into());
    check("fedora-43-group", &facts, &pins());
}

#[test]
fn channel_switches_in_both_directions() {
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let to_canary = Pins {
        channel: Some(Channel::Canary),
        ..pins()
    };
    check(
        "arch-switch-to-stable",
        &installed("arch", Family::Pacman, Channel::Canary),
        &to_stable,
    );
    check(
        "debian-switch-to-canary",
        &installed("debian", Family::Apt, Channel::Stable),
        &to_canary,
    );
    check(
        "fedora-switch-to-stable",
        &installed("fedora", Family::Dnf, Channel::Canary),
        &to_stable,
    );
    check(
        "bazzite-switch-to-canary",
        &installed("bazzite", Family::Sysext, Channel::Stable),
        &to_canary,
    );
    check(
        "steamos-switch-to-stable",
        &deck_on(Channel::Canary),
        &to_stable,
    );
}

/// A Deck's channel is its checkout's branch. A bare re-run rebuilds whatever it follows; a
/// switch moves the branch first, fast-forward only, then rebuilds through the same hand-off.
#[test]
fn trap_a_deck_switch_moves_the_branch_before_the_build() {
    let rerun = plan_for(&deck_on(Channel::Canary), &pins()).commands();
    assert!(
        !rerun
            .iter()
            .any(|c| c.contains("checkout") || c.contains("merge")),
        "a bare re-run moved a canary Deck: {rerun:?}"
    );
    for (from, to, branch) in [
        (Channel::Canary, Channel::Stable, "stable"),
        (Channel::Stable, Channel::Canary, "main"),
    ] {
        let pins = Pins {
            channel: Some(to),
            ..pins()
        };
        let cmds = plan_for(&deck_on(from), &pins).commands();
        let at = |needle: &str| {
            cmds.iter()
                .position(|c| c.contains(needle))
                .unwrap_or_else(|| panic!("{needle} missing: {cmds:?}"))
        };
        assert!(at("fetch origin") < at(&format!("checkout {branch}")));
        assert!(
            at(&format!("checkout {branch}")) < at(&format!("merge --ff-only origin/{branch}"))
        );
        assert!(at("--ff-only") < at("scripts/steamdeck/install.sh"));
        assert!(!cmds.iter().any(|c| c.contains("git clone")), "{cmds:?}");
    }
}

#[test]
fn uninstalls() {
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    check(
        "debian-uninstall",
        &installed("debian", Family::Apt, Channel::Stable),
        &un,
    );
    check(
        "fedora-uninstall",
        &installed("fedora", Family::Dnf, Channel::Stable),
        &un,
    );
    check(
        "arch-uninstall",
        &installed("arch", Family::Pacman, Channel::Stable),
        &un,
    );
    check(
        "omarchy-uninstall",
        &installed("omarchy", Family::Pacman, Channel::Stable),
        &un,
    );
    check(
        "bazzite-uninstall",
        &installed("bazzite", Family::Sysext, Channel::Stable),
        &un,
    );
    check(
        "steamos-uninstall",
        &installed("steamos", Family::Steamos, Channel::Stable),
        &un,
    );
}

/// An already-complete box: the packages are updated in place, then the setup continues.
#[test]
fn a_re_run_on_a_complete_box_updates_in_place() {
    check(
        "arch-installed-rerun",
        &installed("arch", Family::Pacman, Channel::Canary),
        &pins(),
    );
}

/// The install line carries the console, so the start phase enables it even though the
/// pre-install probe found no unit.
#[test]
fn a_box_without_the_console_gets_it_installed_and_started() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.has_web_server = false;
    facts.web_unit_present = false;
    facts.missing = vec!["web-console".into()];
    facts.installed_pf = vec!["punktfunk-host".into(), "punktfunk-scripting".into()];
    check("debian-noweb", &facts, &pins());
}

/// A server with no desktop installed: the console still starts, no certutil for a browser
/// it will never run, and the host is pinned to the one backend that stands a session up.
#[test]
fn a_desktopless_box_pins_gamescope_and_starts_the_console() {
    let mut facts = fresh("ubuntu", Family::Apt);
    facts.graphical_seat = false;
    facts.desktop_sessions = false;
    check("ubuntu-headless", &facts, &pins());
}

#[test]
fn a_sunshine_box_moves_the_management_port_and_keeps_gamestream_off() {
    let mut facts = fresh("fedora", Family::Dnf);
    facts.sunshine_active = true;
    facts.firewall = Firewall::Firewalld;
    check("fedora-sunshine", &facts, &pins());
}

#[test]
fn a_ufw_box_opens_the_named_profiles() {
    let mut facts = fresh("debian", Family::Apt);
    facts.firewall = Firewall::Ufw;
    let with_gs = Pins {
        gamestream: Some(true),
        clipboard: Some(true),
        ..pins()
    };
    check("debian-ufw-gamestream", &facts, &with_gs);
}

#[test]
fn an_nvidia_box_without_a_driver_is_warned_after_a_successful_install() {
    let mut facts = installed("fedora", Family::Dnf, Channel::Stable);
    facts.nvidia = Nvidia::NoDriver;
    check("fedora-nvidia-nodriver", &facts, &pins());
}

/// Client shares the family repo, so host+client is one transaction.
#[test]
fn client_installs_per_family() {
    let client = Pins {
        client: true,
        ..pins()
    };
    let both = Pins {
        host: true,
        client: true,
        ..pins()
    };
    // `-Syu`, not `-S`: a client-only run adds the repo and installs in one go. `-S` against
    // an unfetched database dies with "target not found".
    for (name, facts, line) in [
        (
            "debian-client-only",
            fresh("debian", Family::Apt),
            "sudo apt install punktfunk-client",
        ),
        (
            "arch-client-only",
            fresh("arch", Family::Pacman),
            "sudo pacman -Syu punktfunk-client",
        ),
        (
            "fedora-client-only",
            fresh("fedora", Family::Dnf),
            "sudo dnf install punktfunk-client",
        ),
    ] {
        check(name, &facts, &client);
        let cmds = plan_for(&facts, &client).commands();
        assert!(
            cmds.iter().any(|c| c == line),
            "{name} should run `{line}`: {cmds:?}"
        );
    }

    check(
        "debian-host-and-client",
        &fresh("debian", Family::Apt),
        &both,
    );
    let cmds = plan_for(&fresh("debian", Family::Apt), &both).commands();
    assert!(
        cmds.iter()
            .any(|c| c.ends_with("punktfunk-scripting punktfunk-client")),
        "host and client should be one transaction: {cmds:?}"
    );
}

/// Couch images have no `punktfunk-client` package; the client is a user flatpak.
#[test]
fn a_couch_box_gets_the_client_as_a_flatpak() {
    let both = Pins {
        host: true,
        client: true,
        ..pins()
    };
    check(
        "bazzite-host-and-client",
        &fresh("bazzite", Family::Sysext),
        &both,
    );
    let cmds = plan_for(&fresh("bazzite", Family::Sysext), &both).commands();
    assert!(
        cmds.iter()
            .any(|c| c.contains("punktfunk-sysext.sh install")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("flatpak install --user")),
        "{cmds:?}"
    );
}

/// No punktfunk repo: client is still a flatpak plan. Host punt is `main`'s job.
#[test]
fn an_unsupported_distro_still_installs_a_client() {
    let mut facts = fresh("voidlinux", Family::Flatpak);
    facts.host_punt = Some("no package repo for 'Void Linux' yet".into());
    let client = Pins {
        client: true,
        ..pins()
    };
    check("unsupported-client-only", &facts, &client);
    let cmds = plan_for(&facts, &client).commands();
    assert_eq!(
        cmds.len(),
        1,
        "a client-only install wires nothing else: {cmds:?}"
    );
    assert!(cmds[0].starts_with("flatpak install --user"), "{cmds:?}");
}

/// Family uninstall does not see a flatpak client.
#[test]
fn uninstall_sweeps_a_flatpak_client_too() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.has_flatpak_client = true;
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    assert!(
        cmds.iter()
            .any(|c| c == "flatpak uninstall --user io.unom.Punktfunk"),
        "the flatpak client was left behind: {cmds:?}"
    );
    let mut bare = installed("debian", Family::Apt, Channel::Stable);
    bare.has_flatpak_client = false;
    assert!(!plan_for(&bare, &un)
        .commands()
        .iter()
        .any(|c| c.contains("flatpak")));
}

#[test]
fn a_client_only_plan_wires_none_of_the_host_setup() {
    let client = Pins {
        client: true,
        ..pins()
    };
    let cmds = plan_for(&fresh("debian", Family::Apt), &client).commands();
    assert!(!cmds.iter().any(|c| c.contains("usermod")), "{cmds:?}");
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("ufw") || c.contains("firewall-cmd")),
        "{cmds:?}"
    );
    assert!(!cmds.iter().any(|c| c.contains("loginctl")), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.contains("systemctl --user enable")),
        "{cmds:?}"
    );
}

#[test]
fn the_flatpak_line_is_carried_verbatim() {
    let client = Pins {
        client: true,
        ..pins()
    };
    let facts = fresh("bazzite", Family::Sysext);
    let text = render(&facts, &Choices::derive(&facts, &client));
    for line in punktfunk_setup::platform::install_lines("linux-client") {
        let line = punktfunk_setup::exec::noninteractive(&line);
        assert!(
            text.contains(&line),
            "the flatpak line drifted:\n  {line}\n\n{text}"
        );
    }
}

// -------------------------------------------------------- design/installer-v2.md §4 traps

/// A bare re-run on a canary machine must never drag it to stable, and must leave the repo it
/// already has alone. It still upgrades: that is how a fixed build reaches a box carrying an
/// older one, and skipping it left an uninstall as the only way out.
#[test]
fn trap_channel_follows_the_box_without_an_explicit_flag() {
    let facts = installed("arch", Family::Pacman, Channel::Canary);
    let choices = Choices::derive(&facts, &pins());
    assert_eq!(choices.channel, Channel::Canary);
    assert_eq!(choices.switch_from, None);
    let cmds = plan_for(&facts, &pins()).commands();
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("pacman.conf") || c.contains("pacman-key")),
        "a re-run rewrote the repo it already had: {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.starts_with("sudo pacman -Syu punktfunk-host")),
        "a re-run must upgrade the packages it already has: {cmds:?}"
    );
}

/// apt will not walk to a lower candidate, so the switch pins versions and allows the
/// downgrade. Dry-run prints `<version>`, not a resolved pin.
#[test]
fn trap_apt_switch_pins_versions_and_allows_downgrades() {
    let facts = installed("debian", Family::Apt, Channel::Canary);
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let text = render(&facts, &Choices::derive(&facts, &to_stable));
    assert!(
        text.contains("sudo apt install -y --allow-downgrades"),
        "{text}"
    );
    assert!(text.contains("punktfunk-host=<version>"), "{text}");
    // Repo rewrite first: madison's first row is then the target channel's newest.
    let repo = text
        .find(" stable main")
        .expect("the sources line names the target channel");
    let install = text.find("--allow-downgrades").expect("the pinned install");
    assert!(
        repo < install,
        "the pins were resolved against the old channel:\n{text}"
    );
}

/// `-Sy` then `-S`, never `-Syu`: a sysupgrade sees a lower stable version and does nothing.
#[test]
fn trap_pacman_switch_uses_sy_then_s_and_never_syu() {
    let facts = installed("arch", Family::Pacman, Channel::Canary);
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let cmds = plan_for(&facts, &to_stable).commands();
    assert!(cmds.contains(&"sudo pacman -Sy".to_string()), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.starts_with("sudo pacman -Syu")),
        "{cmds:?}"
    );
    // Drop the old repo section first, or both stay enabled.
    assert!(
        cmds[0].contains("sed -i"),
        "the old repo section must be dropped first: {cmds:?}"
    );
}

/// The Omarchy report this came from: a box with everything installed got no package step at
/// all, so a build carrying a fix could not reach it and an uninstall was the only way out.
/// The db refresh has to survive too, or `-S` upgrades against a stale database.
#[test]
fn trap_a_complete_omarchy_box_still_refreshes_and_upgrades() {
    let facts = installed("omarchy", Family::Pacman, Channel::Canary);
    let cmds = plan_for(&facts, &pins()).commands();
    let pacman: Vec<&String> = cmds
        .iter()
        .filter(|c| c.starts_with("sudo pacman"))
        .collect();
    assert_eq!(pacman.len(), 2, "{cmds:?}");
    assert_eq!(pacman[0], "sudo pacman -Sy", "{cmds:?}");
    assert!(
        pacman[1].starts_with("sudo pacman -S punktfunk-host"),
        "{cmds:?}"
    );
}

/// The certificate is trusted on every host install without a question, after the start
/// (it exists only once the host has run), and only the opt-out or `--no-start` drops it.
#[test]
fn trap_every_host_install_trusts_the_console_cert_after_the_start() {
    let has_cert = |plan: &Plan| {
        plan.steps()
            .any(|s| matches!(s.action, StepAction::TrustCert))
    };
    for (id, family) in [
        ("debian", Family::Apt),
        ("fedora", Family::Dnf),
        ("arch", Family::Pacman),
        ("bazzite", Family::Sysext),
    ] {
        let plan = plan_for(&fresh(id, family), &pins());
        assert!(has_cert(&plan), "{id} does not trust the certificate");
        let start = plan
            .phases
            .iter()
            .position(|p| p.kind == plan::Phase::Start)
            .expect("a start phase");
        assert!(
            matches!(
                plan.phases[start].steps.last().map(|s| &s.action),
                Some(StepAction::TrustCert)
            ),
            "{id}: the trust step must close the start phase"
        );
    }
    let off = Pins {
        console_cert: Some(false),
        ..pins()
    };
    assert!(!has_cert(&plan_for(&fresh("arch", Family::Pacman), &off)));
    let no_start = Pins {
        no_start: true,
        ..pins()
    };
    assert!(!has_cert(&plan_for(
        &fresh("arch", Family::Pacman),
        &no_start
    )));
    let client = Pins {
        client: true,
        ..pins()
    };
    assert!(!has_cert(&plan_for(
        &fresh("arch", Family::Pacman),
        &client
    )));
}

/// Omarchy's libalpm hook aborts any transaction carrying both -S and -u.
#[test]
fn trap_omarchy_installs_with_sy_then_s_not_syu() {
    let cmds = plan_for(&fresh("omarchy", Family::Pacman), &pins()).commands();
    assert!(cmds.contains(&"sudo pacman -Sy".to_string()), "{cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.starts_with("sudo pacman -Syu")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.contains("punktfunk-client")),
        "omarchy gets the client too"
    );
}

/// `punktfunk-omarchy remove` ships in the host package, so it must run before pacman takes it.
#[test]
fn trap_omarchy_uninstall_runs_remove_before_pacman_rns() {
    let facts = installed("omarchy", Family::Pacman, Channel::Stable);
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    let remove = cmds
        .iter()
        .position(|c| c == "punktfunk-omarchy remove")
        .expect("remove step");
    let rns = cmds
        .iter()
        .position(|c| c.starts_with("sudo pacman -Rns"))
        .expect("pacman -Rns");
    assert!(
        remove < rns,
        "the wiring must come off before the binary that removes it: {cmds:?}"
    );
}

/// The hand-off does groups, firewall, and autostart; nothing generic runs after.
#[test]
fn trap_the_omarchy_hand_off_ends_the_run() {
    let plan = plan_for(&fresh("omarchy", Family::Pacman), &pins());
    let handoff = plan
        .steps()
        .find(|s| matches!(&s.action, StepAction::RunIfPresent { cmd, .. } if cmd.starts_with("punktfunk-omarchy setup")))
        .expect("the hand-off step");
    assert!(handoff.ends_run);
}

/// The on-device build does groups, linger, tuning and the unit start itself; nothing generic
/// runs after it. The clipboard setting lands AFTER, because that script writes host.env only
/// when the config dir is fresh and its defaults carry the Deck's `RADV_PERFTEST=video_encode`.
#[test]
fn trap_the_steamos_build_ends_the_run_and_sets_host_env_after_it() {
    let plan = plan_for(&fresh("steamos", Family::Steamos), &pins());
    let steps: Vec<&StepAction> = plan.steps().map(|s| &s.action).collect();
    let build = steps
        .iter()
        .position(|a| matches!(a, StepAction::Run(c) if c.contains("scripts/steamdeck/install.sh")))
        .expect("the on-device build step");
    let env = steps
        .iter()
        .position(|a| matches!(a, StepAction::SetSetting { id, .. } if id == "clipboard"))
        .expect("the clipboard step");
    assert!(build < env, "host.env would swallow the encoder default");

    let last = plan
        .steps()
        .find(|s| s.ends_run)
        .expect("a step that ends the run");
    assert!(
        matches!(&last.action, StepAction::SetSetting { id, .. } if id == "clipboard"),
        "the run must end on the last step of the hand-off, not before it: {:?}",
        last.action
    );
}

/// Both hand-offs end the run, so next to Sunshine the port move must land before they do.
#[test]
fn trap_a_hand_off_never_skips_the_mgmt_port_move() {
    for (id, family) in [("omarchy", Family::Pacman), ("steamos", Family::Steamos)] {
        let mut facts = fresh(id, family);
        facts.sunshine_active = true;
        let plan = plan_for(&facts, &pins());
        let steps: Vec<_> = plan.steps().collect();
        let end = steps
            .iter()
            .position(|s| s.ends_run)
            .expect("a step that ends the run");
        assert!(
            steps[..=end].iter().any(|s| matches!(
                &s.action,
                StepAction::SetEnv { key, value }
                    if key == "PUNKTFUNK_MGMT_BIND" && value == "0.0.0.0:47991"
            )),
            "{id}: the move is planned after the run ends"
        );
    }
}

/// A re-run is documented as safe, and `git clone` into an existing tree is a hard failure.
#[test]
fn trap_the_steamos_clone_tolerates_a_tree_that_is_already_there() {
    let cmds = plan_for(&fresh("steamos", Family::Steamos), &pins()).commands();
    let clone = cmds
        .iter()
        .find(|c| c.contains("git clone"))
        .expect("the clone command");
    assert!(clone.starts_with("[ -d ~/punktfunk/.git ] ||"), "{clone}");
}

/// The progress line captures the build's output, so the wait is announced before the script
/// starts; otherwise the run looks frozen for 20 minutes.
#[test]
fn trap_the_steamos_build_is_announced_before_it_runs() {
    let plan = plan_for(&fresh("steamos", Family::Steamos), &pins());
    let steps: Vec<&StepAction> = plan.steps().map(|s| &s.action).collect();
    let note = steps
        .iter()
        .position(|a| matches!(a, StepAction::Note(_, t) if t.contains("20 minutes")))
        .expect("the wait is announced");
    let build = steps
        .iter()
        .position(|a| matches!(a, StepAction::Run(c) if c.contains("scripts/steamdeck/install.sh")))
        .expect("the on-device build step");
    assert!(
        note < build,
        "the estimate must print before the build starts"
    );
}

/// The on-device build ends the run, so the client's flatpak has to come before it.
#[test]
fn trap_steamos_installs_the_client_before_the_build() {
    let both = Pins {
        host: true,
        client: true,
        ..pins()
    };
    let cmds = plan_for(&fresh("steamos", Family::Steamos), &both).commands();
    let flatpak = cmds.iter().position(|c| c.starts_with("flatpak install"));
    let build = cmds
        .iter()
        .position(|c| c.contains("scripts/steamdeck/install.sh"));
    assert!(
        matches!((flatpak, build), (Some(f), Some(b)) if f < b),
        "{cmds:?}"
    );
}

/// The build script takes `--gamestream` and stores it before it starts the host; a setting
/// written after the script would wait for a restart.
#[test]
fn trap_steamos_forwards_the_gamestream_choice_to_the_script() {
    let on = Pins {
        gamestream: Some(true),
        ..pins()
    };
    let cmds = plan_for(&fresh("steamos", Family::Steamos), &on).commands();
    assert!(
        cmds.iter()
            .any(|c| c.contains("scripts/steamdeck/install.sh --gamestream")),
        "{cmds:?}"
    );
    // The console there reads web.env, which only the script writes.
    assert!(
        cmds.iter().any(|c| c.ends_with(" --web-bind=0.0.0.0")),
        "{cmds:?}"
    );
}

/// Every optional part of the hand-off is passed explicitly, so the script never has to ask.
/// A row that stops being forwarded here becomes a question again on the next Omarchy box.
#[test]
fn the_hand_off_carries_every_row_it_used_to_ask_for() {
    let off = Pins {
        omarchy_toasts: Some(false),
        omarchy_theme: Some(false),
        ..pins()
    };
    let cmds = plan_for(&fresh("omarchy", Family::Pacman), &off).commands();
    let handoff = cmds
        .iter()
        .find(|c| c.starts_with("punktfunk-omarchy setup"))
        .expect("the hand-off command");
    assert!(handoff.contains("--toasts=0"), "{handoff}");
    assert!(handoff.contains("--theme=0"), "{handoff}");
    assert!(handoff.contains("--idle-guard=1"), "{handoff}");
    assert!(handoff.contains("--cert=1"), "{handoff}");
    assert!(handoff.contains("--groups=1"), "{handoff}");
}

/// The labels live in two files — the installer emits them, the script parses them — because the
/// script ships inside the host package and is not on disk when the settings screen is drawn.
/// This is the seam that keeps the duplication honest.
#[test]
fn trap_every_flag_the_installer_sends_is_one_the_script_accepts() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root");
    let script = std::fs::read_to_string(root.join("packaging/linux/omarchy/punktfunk-omarchy"))
        .expect("punktfunk-omarchy");
    let cmds = plan_for(&fresh("omarchy", Family::Pacman), &pins()).commands();
    let handoff = cmds
        .iter()
        .find(|c| c.starts_with("punktfunk-omarchy setup"))
        .expect("the hand-off command");
    for flag in handoff.split_whitespace().filter(|w| w.starts_with("--")) {
        let name = flag.split('=').next().unwrap();
        assert!(
            script.contains(&format!("{name}=*)")),
            "{name} is sent but punktfunk-omarchy has no case for it"
        );
    }
    assert!(
        !script.contains("read -r -p"),
        "punktfunk-omarchy setup must not prompt — the installer already asked"
    );
}

/// The smoke has to be able to decline the hand-off; the sh installer's question had no twin.
#[test]
fn trap_the_hand_off_can_be_declined() {
    let declined = Pins {
        omarchy_setup: Some(false),
        ..pins()
    };
    let cmds = plan_for(&fresh("omarchy", Family::Pacman), &declined).commands();
    assert!(
        !cmds
            .iter()
            .any(|c| c.starts_with("punktfunk-omarchy setup")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.contains("usermod -aG punktfunk")),
        "{cmds:?}"
    );
}

/// Packages this installer never added stay on the old channel unless the switch names them.
#[test]
fn trap_switch_pkgs_carries_packages_the_installer_never_installed() {
    let mut facts = installed("arch", Family::Pacman, Channel::Canary);
    facts.installed_pf.push("punktfunk-gamescope".into());
    facts.installed_pf.push("punktfunk-client".into());
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let text = render(&facts, &Choices::derive(&facts, &to_stable));
    assert!(
        text.contains("punktfunk-gamescope"),
        "gamescope was stranded:\n{text}"
    );
    assert!(
        text.contains("punktfunk-client"),
        "the client was stranded:\n{text}"
    );
}

/// An installed apt box updates against fresh lists, or `apt install` reports the old build
/// as the newest.
#[test]
fn trap_an_installed_apt_box_refreshes_before_it_updates() {
    let facts = installed("debian", Family::Apt, Channel::Stable);
    let cmds = plan_for(&facts, &pins()).commands();
    let update = cmds.iter().position(|c| c == "sudo apt update");
    let install = cmds.iter().position(|c| c.starts_with("sudo apt install"));
    assert!(
        matches!((update, install), (Some(u), Some(i)) if u < i),
        "{cmds:?}"
    );
}

/// A client-only box switching channel reinstalls its client, never the host.
#[test]
fn trap_a_client_only_switch_installs_no_host() {
    for (id, family) in [
        ("debian", Family::Apt),
        ("fedora", Family::Dnf),
        ("arch", Family::Pacman),
    ] {
        let mut facts = installed(id, family, Channel::Stable);
        facts.installed_pf = vec!["punktfunk-client".into()];
        let to_canary = Pins {
            host: false,
            client: true,
            channel: Some(Channel::Canary),
            ..pins()
        };
        let text = render(&facts, &Choices::derive(&facts, &to_canary));
        assert!(
            !text.contains("punktfunk-web") && !text.contains("punktfunk-scripting"),
            "{id}:\n{text}"
        );
        assert!(text.contains("punktfunk-client"), "{id}:\n{text}");
    }
}

/// dnf goes down with distro-sync; install alone only ever moves up.
#[test]
fn trap_dnf_switch_installs_then_distro_syncs() {
    let facts = installed("fedora", Family::Dnf, Channel::Canary);
    let to_stable = Pins {
        channel: Some(Channel::Stable),
        ..pins()
    };
    let cmds = plan_for(&facts, &to_stable).commands();
    let install = cmds
        .iter()
        .position(|c| c.starts_with("sudo dnf install"))
        .expect("install");
    let sync = cmds
        .iter()
        .position(|c| c.starts_with("sudo dnf distro-sync"))
        .expect("sync");
    assert!(install < sync, "{cmds:?}");
}

/// Linger creates the user manager on a seatless box, so it has to land before the enable.
#[test]
fn trap_linger_is_planned_before_the_unit_enable() {
    use punktfunk_setup::plan::Phase;
    let mut facts = fresh("debian", Family::Apt);
    facts.graphical_seat = false;
    let plan = plan_for(&facts, &pins());
    let linger = plan
        .phases
        .iter()
        .position(|p| p.kind == Phase::Linger)
        .expect("linger phase");
    let start = plan
        .phases
        .iter()
        .position(|p| p.kind == Phase::Start)
        .expect("start phase");
    assert!(linger < start, "the user manager would not exist yet");
}

/// Missing is per package; host present is not enough to skip install.
#[test]
fn trap_a_box_with_the_host_but_no_console_still_installs() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.missing = vec!["web-console".into()];
    let cmds = plan_for(&facts, &pins()).commands();
    assert!(
        cmds.iter()
            .any(|c| c == "sudo apt install punktfunk-host punktfunk-web punktfunk-scripting"),
        "a weak-deps-off box would never grow a console: {cmds:?}"
    );
}

/// Linger defaults only for Bazzite and Nobara. Silverblue and Bluefin are workstations.
#[test]
fn trap_couch_defaults_do_not_reach_a_workstation_image() {
    let couch = Choices::derive(&fresh("bazzite", Family::Sysext), &pins());
    assert!(couch.linger);
    let mut workstation = fresh("bluefin", Family::Sysext);
    workstation.couch_box = false;
    assert!(!Choices::derive(&workstation, &pins()).linger);
}

/// User units come off first: package removal cannot see the enable symlinks in `$HOME`.
#[test]
fn trap_uninstall_disables_user_units_before_removing_packages() {
    for (id, family, verb) in [
        ("debian", Family::Apt, "sudo apt purge"),
        ("fedora", Family::Dnf, "sudo dnf remove"),
        ("arch", Family::Pacman, "sudo pacman -Rns"),
    ] {
        let facts = installed(id, family, Channel::Stable);
        let un = Pins {
            action: Action::Uninstall,
            ..pins()
        };
        let cmds = plan_for(&facts, &un).commands();
        assert!(
            cmds[0].starts_with("systemctl --user disable --now"),
            "{id}: {cmds:?}"
        );
        assert!(cmds.iter().any(|c| c.starts_with(verb)), "{id}: {cmds:?}");
    }
}

#[test]
fn trap_uninstall_removes_only_what_is_installed() {
    let mut facts = installed("debian", Family::Apt, Channel::Stable);
    facts.installed_pf = vec!["punktfunk-host".into()];
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    assert!(
        cmds.contains(&"sudo apt purge punktfunk-host".to_string()),
        "{cmds:?}"
    );
}

/// A bare box has no purge line; a fixed package list would fail.
#[test]
fn trap_uninstall_on_a_bare_box_skips_the_package_removal() {
    let facts = fresh("debian", Family::Apt);
    let un = Pins {
        action: Action::Uninstall,
        ..pins()
    };
    let cmds = plan_for(&facts, &un).commands();
    assert!(
        !cmds.iter().any(|c| c.starts_with("sudo apt purge")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.starts_with("sudo rm -f")),
        "the repo still goes: {cmds:?}"
    );
}

/// Bazzite's input group is recipe-managed; usermod is the wrong tool there.
#[test]
fn trap_a_ujust_box_uses_the_recipe_not_usermod() {
    let mut facts = fresh("bazzite", Family::Sysext);
    facts.has_ujust = true;
    let cmds = plan_for(&facts, &pins()).commands();
    let recipe = cmds
        .iter()
        .find(|c| c.starts_with("ujust add-user-to-input-group"))
        .unwrap_or_else(|| panic!("{cmds:?}"));
    assert!(
        recipe.starts_with("ujust add-user-to-input-group add"),
        "a bare recipe call opens a prompt the run cannot answer: {recipe}"
    );
}

/// Every host platform's platforms.json lines must appear in the dry-run text, in the
/// no-confirm form the run uses. This is D6's drift gate, moved into the crate that embeds
/// the file.
#[test]
fn every_platforms_json_install_line_is_carried_verbatim() {
    let cases = [
        ("debian", fresh("debian", Family::Apt)),
        ("arch", fresh("arch", Family::Pacman)),
        ("omarchy", fresh("omarchy", Family::Pacman)),
        ("fedora", fresh("fedora", Family::Dnf)),
        ("bazzite", fresh("bazzite", Family::Sysext)),
        ("steamos", fresh("steamos", Family::Steamos)),
    ];
    for (id, facts) in cases {
        let text = render(&facts, &Choices::derive(&facts, &pins()));
        for line in punktfunk_setup::platform::install_lines(id) {
            let line = punktfunk_setup::exec::noninteractive(&line);
            assert!(
                text.contains(&line),
                "the {id} dry-run no longer carries platforms.json's line:\n  {line}\n\n{text}"
            );
        }
    }
}

/// A typed password reaches the file and nothing else. The plan step carries no value, so
/// the echo, the dry run and every golden stay free of it — and the phase lands before the
/// Omarchy hand-off, which ends the run before the later phases exist.
#[test]
fn a_typed_console_password_is_written_but_never_echoed() {
    let facts = fresh("omarchy", Family::Pacman);
    let mut choices = Choices::derive(&facts, &pins());
    choices.web_password = Some("correct-horse-battery".into());
    let text = render(&facts, &choices);
    assert!(
        !text.contains("correct-horse"),
        "the password reached the transcript:\n{text}"
    );
    assert!(
        text.contains("would write your password to /box/config/punktfunk/web-password"),
        "the dry run does not say where the password goes:\n{text}"
    );
    let kinds: Vec<Phase> = plan_for(&facts, &pins())
        .phases
        .iter()
        .map(|p| p.kind)
        .collect();
    assert!(!kinds.contains(&Phase::Password), "nobody asked for one");
    let phases = plan::build(&facts, &choices);
    let kinds: Vec<Phase> = phases.phases.iter().map(|p| p.kind).collect();
    let at = |k: Phase| kinds.iter().position(|p| *p == k);
    assert!(
        at(Phase::Password) < at(Phase::Omarchy),
        "the hand-off would end the run first: {kinds:?}"
    );
}

/// The file the console's unit reads as an `EnvironmentFile`, owner-only — a umask of 0022
/// would otherwise leave the login password readable by everyone on the box.
#[test]
fn the_password_file_is_written_owner_only() {
    let root = std::env::temp_dir().join(format!("pf-setup-pw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = BasePaths::rooted(&root);
    let facts = fresh("arch", Family::Pacman);
    let mut choices = Choices::derive(&facts, &pins());
    choices.web_password = Some("correct-horse-battery".into());
    let (ui, _buf) = Plain::capture();
    let run = FakeRunner::new();
    let exec = Executor {
        paths: &paths,
        run: &run,
        ui: &ui,
        opts: Opts {
            dry: false,
            quiet: false,
            tty: false,
        },
    };
    let plan = Plan {
        phases: vec![PlanPhase {
            kind: Phase::Password,
            title: "Web console password".into(),
            steps: vec![Step {
                action: StepAction::WebPassword,
                ends_run: false,
            }],
        }],
    };
    exec.execute(&plan, &facts, &choices).expect("one write");

    let file = root.join("config/punktfunk/web-password");
    assert_eq!(
        std::fs::read_to_string(&file).expect("the password file"),
        "PUNKTFUNK_UI_PASSWORD=correct-horse-battery\n"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600, "the password is readable by others");
        assert_eq!(mode(file.parent().expect("dir")), 0o700);
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Settings land through the host's own store: validated against the registry, merged with the
/// keys already there, and owner-only like every write the host makes to that file.
#[cfg(unix)]
#[test]
fn host_settings_go_through_the_hosts_store() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir().join(format!("pf-setup-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let paths = BasePaths::rooted(&root);
    let file = paths.host_settings();
    std::fs::create_dir_all(file.parent().expect("dir")).unwrap();
    std::fs::write(&file, r#"{"version":1,"from_a_newer_host":"kept"}"#).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    let facts = fresh("arch", Family::Pacman);
    let choices = Choices::derive(&facts, &pins());
    let (ui, buf) = Plain::capture();
    let run = FakeRunner::new();
    let exec = Executor {
        paths: &paths,
        run: &run,
        ui: &ui,
        opts: Opts {
            dry: false,
            quiet: false,
            tty: false,
        },
    };
    let plan = Plan {
        phases: vec![PlanPhase {
            kind: Phase::Options,
            title: "Host settings".into(),
            steps: vec![
                Step::set_setting("gamestream", true),
                Step::set_setting("clipboard", "files"),
                Step::set_setting("clipboard", "everything"),
            ],
        }],
    };
    exec.execute(&plan, &facts, &choices).expect("the writes");

    let stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(stored["gamestream"], true);
    assert_eq!(stored["clipboard"], "files", "an invalid value never lands");
    assert_eq!(stored["from_a_newer_host"], "kept");
    assert!(
        buf.borrow().contains("couldn't write"),
        "the refusal is reported"
    );
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
    let _ = std::fs::remove_dir_all(&root);
}
