//! WP2.4's acceptance: the silent path's transcript, pinned under
//! `tests/golden/win-silent-*.txt` on every OS (`UPDATE_GOLDEN=1 cargo test` regenerates),
//! plus the D5/D12 rules a silent run must keep whatever the wording does.

mod common;

use std::path::Path;

use common::{fresh, golden, public_network, upgrade_without_inno};
use punktfunk_setup::platform::windows::args::InnoArgs;
use punktfunk_setup::platform::windows::exec::{FakePayload, Subst, WinExecutor};
use punktfunk_setup::platform::windows::plan::Artifact;
use punktfunk_setup::platform::windows::{silent, FakeNet, WinFacts};
use punktfunk_setup::seam::{BasePaths, Env, FakeRunner};
use punktfunk_setup::ui::Plain;

/// The exact spawn from `update/windows.rs`: `SILENT_ARGS` plus `/LOG=`.
const UPDATER_ARGS: [&str; 5] = [
    "/VERYSILENT",
    "/SUPPRESSMSGBOXES",
    "/NORESTART",
    "/SP-",
    r"/LOG=C:\ProgramData\punktfunk\logs\update-0.36.0.log",
];

fn args(list: &[&str]) -> InnoArgs {
    InnoArgs::parse(&list.iter().map(|s| (*s).to_string()).collect::<Vec<_>>())
}

/// A silent run over seams that cannot touch anything — `exec::render`'s executor with
/// `silent: true`. `dry: false` against the answerless `FakeRunner` is the failure path.
fn transcript(
    facts: &WinFacts,
    artifact: Artifact,
    uninstall: bool,
    args: &InnoArgs,
    dry: bool,
) -> (Result<(), String>, String) {
    let (ui, buf) = Plain::capture();
    let run = FakeRunner::new();
    let net = FakeNet {
        networks: facts.networks.clone(),
        ..FakeNet::default()
    };
    let payload = FakePayload::default();
    let paths = BasePaths::rooted(Path::new("/nowhere"));
    let exec = WinExecutor {
        run: &run,
        net: &net,
        payload: &payload,
        paths: &paths,
        ui: &ui,
        dry,
        silent: true,
        web_password: None,
        subst: Subst::default(),
    };
    let outcome =
        silent::run(&exec, facts, artifact, uninstall, args, &Env::default()).map_err(|f| f.0);
    let text = buf.borrow().clone();
    (outcome, text)
}

// The fielded spawn, dry: the subcommand lines the runner smoke (WP3.3) will assert on.
#[test]
fn golden_silent_fresh_is_the_updater_spawn() {
    let (outcome, text) = transcript(&fresh(), Artifact::Host, false, &args(&UPDATER_ARGS), true);
    assert_eq!(outcome, Ok(()));
    golden("win-silent-fresh", &text);
    assert!(text.contains("service install"));
    assert!(text.contains("--gamestream=off"));
    assert!(text.contains("web setup"));
}

// D12 under silence: the WARN line, never a profile change.
#[test]
fn golden_silent_public_warns_and_never_touches_the_profile() {
    let (outcome, text) = transcript(
        &public_network(),
        Artifact::Host,
        false,
        &args(&UPDATER_ARGS),
        true,
    );
    assert_eq!(outcome, Ok(()));
    golden("win-silent-public", &text);
    assert!(text.contains("!! network 'Netzwerk 2' is set to Public"));
    assert!(!text.contains("would set network"));
    assert!(!text.contains("--allow-public-network=on"));
}

// The troubleshooting docs' exact reconfigure line, on an installed box: the explicit task
// reaches the plan, the deselect removes the tray key, and /DIR is ignored with a warning.
#[test]
fn golden_silent_mergetasks_reconfigures_an_upgrade() {
    let (outcome, text) = transcript(
        &upgrade_without_inno(),
        Artifact::Host,
        false,
        &args(&[
            "/VERYSILENT",
            r#"/MERGETASKS="allowpublicfw,!trayicon""#,
            r"/DIR=D:\pf",
        ]),
        true,
    );
    assert_eq!(outcome, Ok(()));
    golden("win-silent-reconfigure", &text);
    assert!(text.contains("--allow-public-network=on"));
    assert!(text.contains("settings set tray_autostart false"));
    assert!(text.contains("/DIR ignored"));
}

#[test]
fn golden_silent_uninstall() {
    let (outcome, text) = transcript(
        &upgrade_without_inno(),
        Artifact::Host,
        true,
        &args(&UPDATER_ARGS),
        true,
    );
    assert_eq!(outcome, Ok(()));
    golden("win-silent-uninstall", &text);
    assert!(text.contains("service uninstall"));
    assert!(text.contains("punktfunk was removed"));
}

// D5's tolerance rule end to end: a flag a future updater might pass warns and the run
// still completes.
#[test]
fn an_unknown_flag_warns_and_the_run_completes() {
    let (outcome, text) = transcript(
        &fresh(),
        Artifact::Host,
        false,
        &args(&["/VERYSILENT", "/FUTUREFLAG=2"]),
        true,
    );
    assert_eq!(outcome, Ok(()));
    assert!(text.contains("!! ignoring unknown flags: /FUTUREFLAG=2"));
}

// A step that cannot start dies INTO the log and comes back as the error the exe maps to
// exit 1 — never a quiet success.
#[test]
fn a_step_that_cannot_start_dies_into_the_log_and_fails() {
    let (outcome, text) = transcript(&fresh(), Artifact::Host, false, &args(&UPDATER_ARGS), false);
    let err = outcome.expect_err("the answerless runner cannot start reg");
    assert!(err.contains("did not start"), "{err}");
    assert!(text.contains(&format!("  xx {err}")));
    assert!(!text.contains("Done. Next:"));
}
