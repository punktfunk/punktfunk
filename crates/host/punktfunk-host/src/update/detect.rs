//! Host install-kind and channel: [`Product::Host`], this binary's version, and
//! a process-wide cache around `pf-update-check`.
//!
//! The ladder, version compare, and per-kind command hints live in that crate.
//! This module re-exports the names the rest of the host already uses.

// Named re-exports only: an unused one is a clippy `-D warnings` failure.
// Tests reach the rest through `pf_update_check` directly.
pub(crate) use pf_update_check::detect::InstallKind;
pub(crate) use pf_update_check::version::{is_newer, Channel};

use pf_update_check::detect::{classify as classify_shared, gather, Product};
use std::sync::OnceLock;

pub(crate) fn detect() -> (InstallKind, Channel) {
    static DETECTED: OnceLock<(InstallKind, Channel)> = OnceLock::new();
    *DETECTED.get_or_init(|| {
        classify_shared(&gather(Product::Host, crate::version::get()), Product::Host)
    })
}

/// Test seam: host ladder over an explicit probe.
#[cfg(test)]
fn classify(p: &pf_update_check::detect::Probe) -> (InstallKind, Channel) {
    classify_shared(p, Product::Host)
}

/// One copy-pastable update command for this install kind. No placeholders.
pub(crate) fn channel_hint(kind: InstallKind) -> String {
    // Omarchy: same pacman delivery, different command. `omarchy update` snapshots
    // first. Lives here, not in `pf-update-check` — client-on-Omarchy is out of scope.
    #[cfg(target_os = "linux")]
    if kind == InstallKind::Pacman && crate::osinfo::is_omarchy() {
        return "omarchy update   (snapshots first; punktfunk rides the same transaction)".into();
    }
    pf_update_check::detect::update_command(kind, Product::Host)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Product::Host` is what makes a Deck build report the source-rebuild leg.
    #[test]
    fn host_binding_reports_the_deck_source_leg() {
        let p = pf_update_check::detect::Probe {
            exe: "/home/deck/punktfunk/target-steamos/release/punktfunk-host".into(),
            home: Some("/home/deck".into()),
            ..Default::default()
        };
        assert_eq!(classify(&p).0, InstallKind::SteamosSource);
    }

    #[test]
    fn hints_upgrade_every_punktfunk_package() {
        assert!(channel_hint(InstallKind::Apt).contains("'punktfunk*'"));
        assert!(channel_hint(InstallKind::Sysext).contains("punktfunk-sysext update"));
    }
}
