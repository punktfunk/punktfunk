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
}
