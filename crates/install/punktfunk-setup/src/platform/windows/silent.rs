//! The silent path (WP2.4): the D5 contract under `/VERYSILENT` and `/SILENT`. No window,
//! the plain transcript into `/LOG=` (and the launching console when there is one), and
//! exit 0 only when every step ran — a silent no-op exiting 0 is the fielded-updater bug D5
//! exists to prevent, which is why the caller refuses a real run without a payload.
//!
//! Pure glue over pieces that already exist: `InnoArgs` → `WinChoices::apply` → `WinPlan` →
//! `WinExecutor { silent: true }`, reported through `Plain` so the log reads like the Linux
//! installer's transcript. Goldens under `tests/golden/win-silent-*.txt` pin it on every OS.
//! D12: a Public network only ever warns here — a profile change needs the wizard's consent.

use super::args::InnoArgs;
use super::choices::WinChoices;
use super::exec::WinExecutor;
use super::plan::{self, Artifact};
use super::{report, WinFacts};
use crate::exec::Failed;
use crate::seam::Env;
use crate::ui::Reporter;

/// The same line into every sink: `/LOG=` and the console. Empty is legal — Inno's silent
/// run without `/LOG=` says nothing either.
pub struct Tee<'a>(pub Vec<&'a dyn Reporter>);

impl Reporter for Tee<'_> {
    fn say(&self, msg: &str) {
        self.0.iter().for_each(|r| r.say(msg));
    }
    fn ok(&self, msg: &str) {
        self.0.iter().for_each(|r| r.ok(msg));
    }
    fn warn(&self, msg: &str) {
        self.0.iter().for_each(|r| r.warn(msg));
    }
    fn die(&self, msg: &str) {
        self.0.iter().for_each(|r| r.die(msg));
    }
    fn plus(&self, cmd: &str) {
        self.0.iter().for_each(|r| r.plus(cmd));
    }
    fn detail(&self, msg: &str) {
        self.0.iter().for_each(|r| r.detail(msg));
    }
    fn line(&self, msg: &str) {
        self.0.iter().for_each(|r| r.line(msg));
    }
    fn blank(&self) {
        self.0.iter().for_each(|r| r.blank());
    }
}

/// Facts → choices → plan → run, on the caller's executor (it carries the seams and `dry`).
/// The die line reaches the log before the error returns, so `/LOG=` always ends honestly.
pub fn run(
    exec: &WinExecutor,
    facts: &WinFacts,
    artifact: Artifact,
    uninstall: bool,
    args: &InnoArgs,
    env: &Env,
) -> Result<(), Failed> {
    let ui = exec.ui;
    report::detected(ui, facts, artifact);
    if !args.unknown.is_empty() {
        ui.warn(&format!(
            "ignoring unknown flags: {}",
            args.unknown.join(" ")
        ));
    }
    let mut choices = WinChoices::derive(facts, artifact);
    for warning in choices.apply(args, env) {
        ui.warn(&warning);
    }
    if !uninstall {
        report::choices_summary(ui, &choices, artifact);
    }
    let plan = plan::build(facts, &choices, artifact, uninstall);
    if let Err(failed) = exec.execute(&plan) {
        ui.die(&failed.0);
        return Err(failed);
    }
    if uninstall {
        ui.ok("punktfunk was removed");
    } else {
        report::outro(ui, facts, &choices, artifact);
    }
    Ok(())
}
