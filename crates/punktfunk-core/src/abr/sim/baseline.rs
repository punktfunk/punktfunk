//! The checked-in baseline: what today's controller does on every scenario.
//!
//! The simulator is integer-only and seeded, so the test is equality, not a
//! tolerance. A change that moves a cell re-blesses `baseline.tsv` in the same
//! diff (`ABR_SIM_BLESS=1 cargo test …`) and the reviewer reads which rows
//! moved and why.

use super::{run, scenarios};
use crate::abr::metrics::HEADER;

const BASELINE: &str = include_str!("baseline.tsv");

fn table() -> String {
    let mut out = String::from(HEADER);
    for sc in scenarios::all() {
        out.push('\n');
        out.push_str(&run(&sc).metrics.row(sc.name));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every cell, compared for equality. `ABR_SIM_BLESS=1` rewrites the file.
    #[test]
    fn the_baseline_is_what_todays_controller_does() {
        let got = table();
        if std::env::var("ABR_SIM_BLESS").is_ok_and(|v| v != "0") {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/abr/sim/baseline.tsv");
            std::fs::write(path, &got).expect("rewrite the baseline");
            return;
        }
        for (want, got) in BASELINE.lines().zip(got.lines()) {
            assert_eq!(want, got, "baseline row moved — re-bless it deliberately");
        }
        assert_eq!(
            BASELINE.lines().count(),
            got.lines().count(),
            "the scenario table and the baseline have different rows"
        );
    }

    /// Rows where `Smooth` costs frames: paths under about three times the bitrate,
    /// where pacing at 3× the budget is itself a sustained overrun (the shared
    /// 18 Mbit/s tunnel: 0 → 432 lost per 10 min, queue p95 84 → 329 ms; the WAN
    /// rows by a few frames). `Smooth` is for a LAN receiver that drops the head of a
    /// line-rate burst; the health check never offers it for a queue build-up.
    const SMOOTH_COSTS: &[&str] = &[
        "shared_fixed_plus_auto",
        "wan_brownout",
        "wan_shallow_wall",
        "wan_wg_12_legacy",
    ];

    /// The same table with every host on the `Smooth` delivery profile: on every
    /// LAN-class row, spreading each frame costs the controller nothing — no more lost
    /// frames, no longer under 5 Mbit/s than bursting. [`SMOOTH_COSTS`] names the rest.
    #[test]
    fn smooth_delivery_costs_no_lan_scenario() {
        let mut smooth = scenarios::all();
        for sc in &mut smooth {
            for s in &mut sc.sessions {
                s.host.smooth_delivery = true;
            }
        }
        let mut worse = Vec::new();
        for (burst, smooth) in scenarios::all().iter().zip(&smooth) {
            if SMOOTH_COSTS.contains(&burst.name) {
                continue;
            }
            let b = run(burst).metrics;
            let s = run(smooth).metrics;
            if s.lost_per_10min > b.lost_per_10min || s.under5_pct > b.under5_pct {
                worse.push(format!(
                    "{}: lost {} vs {}, under5 {} vs {}, queue_p95 {} vs {}",
                    burst.name,
                    s.lost_per_10min,
                    b.lost_per_10min,
                    s.under5_pct,
                    b.under5_pct,
                    s.queue_p95_ms,
                    b.queue_p95_ms
                ));
            }
        }
        assert!(
            worse.is_empty(),
            "smooth is worse on:\n{}",
            worse.join("\n")
        );
    }
}
