//! `adl-emul` — the AMD ADL EDID-emulation probe (design: punktfunk-planning
//! `design/vdisplay-disturbance-immunity.md` §9 T1).
//!
//! The implementation lives in `pf_win_display::adl_emul` — one FFI surface shared with the
//! host's `edid_lock` display-policy axis, so the probe a reporter runs and the toggle the
//! console flips exercise byte-identical driver calls. This subcommand is the bench-line
//! printer + exit-code contract around it:
//!
//! * `adl-emul`                       — read-only: caps, board layout, per-connector state.
//! * `adl-emul --lock [--connector N]` — pin the live EDID + `ADL_EMUL_MODE_ALWAYS` (occupied
//!   connectors only unless `--connector` names one).
//! * `adl-emul --unlock [--connector N]` — `ADL_EMUL_MODE_OFF` + remove the pinned EDID.
//!
//! Every call prints the bench's `epoch_ms op target took_ms ok` line plus `rc=` (decoded): the
//! rc IS the deliverable — `ADL2_Adapter_EDIDManagement_Caps` answering `supported=1` with
//! `--lock` returning `ADL_OK` on a consumer RX card kills the Pro-gating assumption, and a
//! locked connector during a stream with the sink asleep is the direct A/B for the metronomic
//! stall. `--unlock` (or a driver reinstall) restores; emulation state can persist across
//! reboots, so a `--lock` run must always be paired with a later `--unlock`.

use std::time::{SystemTime, UNIX_EPOCH};

pub use pf_win_display::adl_emul::EmulAction;
use pf_win_display::adl_emul::{run as adl_run, RunOutcome};

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn run(action: EmulAction, connector_filter: Option<i32>) -> ! {
    let outcome = adl_run(action, connector_filter);
    for r in outcome.records() {
        println!("{} {r}", epoch_ms());
    }
    match outcome {
        RunOutcome::NoAdl => {
            eprintln!(
                "atiadlxx.dll not loadable (or an export is missing) — not an AMD driver \
                 install; the ADL emulation lever does not exist on this box"
            );
            std::process::exit(2);
        }
        RunOutcome::InitFailed(_) => std::process::exit(1),
        RunOutcome::Done(_) => std::process::exit(0),
    }
}
