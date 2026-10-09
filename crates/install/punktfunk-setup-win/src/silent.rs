//! The wizard exe's silent entry (WP2.4): no window. The engine's `silent::run` over the box
//! `main.rs` resolved — a `--demo` preset on the fake seams, or the real box on the system
//! seams with the extracted payload — `Plain` into `/LOG=` and, when a terminal launched us,
//! the parent console. A GUI-subsystem exe owns no console: `sys::attach_parent_console`
//! binds the parent's and hands back `CONOUT$`, because the std handles were fixed at startup
//! and stdout is not it.
//!
//! No payload (a plain wizard build): only `--dry-run` may proceed; a real run refuses with
//! exit 1 — never a silent no-op exiting 0 (D5).

use std::process::ExitCode;

use punktfunk_setup::platform::windows::args::InnoArgs;
use punktfunk_setup::platform::windows::demo::WinPreset;
use punktfunk_setup::platform::windows::{silent, sys};
use punktfunk_setup::seam::Env;
use punktfunk_setup::ui::{Plain, Reporter};

use crate::real::Seams;
use crate::wizard::{DemoSeams, LiveSeams, RealSeams};

pub fn main(inno: &InnoArgs, preset: WinPreset, seams: &Seams, dry: bool) -> ExitCode {
    let log = inno
        .log
        .as_ref()
        .and_then(|path| {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            std::fs::File::create(path).ok()
        })
        .map(|file| Plain::to_writer(file, false));
    let console = sys::attach_parent_console().map(|file| Plain::to_writer(file, false));
    let ui = silent::Tee(
        log.iter()
            .chain(console.iter())
            .map(|p| p as &dyn Reporter)
            .collect(),
    );

    if !dry && matches!(seams, Seams::Real { root: None, .. }) {
        ui.die("no payload in this build — nothing was changed (add --dry-run, or run the packed installer)");
        return ExitCode::FAILURE;
    }
    let env = Env::from_env();
    let live = match seams {
        Seams::Demo { .. } => LiveSeams::Demo(DemoSeams::new(&preset, 0)),
        Seams::Real { root, version } => LiveSeams::Real(RealSeams::new(root.as_deref(), version)),
    };
    let exec = live.executor(&ui, dry, true, None);
    let outcome = silent::run(
        &exec,
        &preset.facts,
        preset.artifact,
        preset.uninstall,
        inno,
        &env,
    );
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
