//! Conflicting game-streaming host detection.
//!
//! Punktfunk shares NVIDIA GameStream ports, `_nvstream._tcp`, and often a
//! virtual-display driver with Sunshine and its forks. Running another host
//! alongside is unsupported.
//!
//! Fingerprint by name against [`KNOWN`]: processes, OS services/units, on-disk
//! markers. Platform back-ends (`detect/windows.rs`, `detect/linux.rs`) supply
//! facts; matching and rendering here are portable and unit-tested.
//!
//! Only a host that is running or will start on its own can take ports or load
//! a second driver. Leftover files and disabled services are dormant.
//! [`Evidence::is_active`] / [`Detection::is_active`] draw that line. Startup
//! log, `/local/summary`, and `detect-conflicts` report active detections only;
//! [`render_report`] still lists dormant ones for support. Pin the table at
//! [`KNOWN`]; behaviour is locked by the tests in this file.

use std::sync::OnceLock;

#[cfg(target_os = "windows")]
#[path = "detect/windows.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "detect/linux.rs"]
mod platform;
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod platform {
    //! Stub: the host ships on Windows/Linux; macOS (dev builds) has nothing to scan.
    pub fn running_processes() -> Vec<String> {
        Vec::new()
    }
    pub fn static_evidence(_known: &super::Known) -> Vec<super::Evidence> {
        Vec::new()
    }
}

/// A known competing GameStream/Moonlight host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Product {
    Sunshine,
    Apollo,
    Vibeshine,
    Vibepollo,
    Luminalshine,
}

impl Product {
    pub fn label(self) -> &'static str {
        match self {
            Product::Sunshine => "Sunshine",
            Product::Apollo => "Apollo",
            Product::Vibeshine => "Vibeshine",
            Product::Vibepollo => "Vibepollo",
            Product::Luminalshine => "LuminalShine",
        }
    }
}

/// How a conflicting host was observed on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(target_os = "macos", allow(dead_code, reason = "macOS scans nothing"))]
pub enum Evidence {
    /// Process basename observed live.
    Running { process: String },
    /// Registered OS service/unit. `autostart` is load-bearing: boot/system/auto
    /// (Windows) or an enabled systemd unit will clash; disabled/manual is inert
    /// until someone starts it, then `Running` catches the next scan.
    Service { name: String, autostart: bool },
    /// On-disk marker (Program Files, flatpak id, `PATH` binary). Always dormant:
    /// files that nothing launches bind no ports.
    Installed { at: String },
}

impl Evidence {
    /// True when this observation means a host will take ports or load a second
    /// virtual-display driver. See the module docs.
    pub fn is_active(&self) -> bool {
        match self {
            Evidence::Running { .. } => true,
            Evidence::Service { autostart, .. } => *autostart,
            Evidence::Installed { .. } => false,
        }
    }

    fn render(&self) -> String {
        match self {
            Evidence::Running { process } => format!("running now ({process})"),
            Evidence::Service {
                name,
                autostart: true,
            } => format!("service {name} (starts automatically)"),
            Evidence::Service {
                name,
                autostart: false,
            } => format!("service {name} (disabled/manual — dormant)"),
            Evidence::Installed { at } => format!("installed at {at}"),
        }
    }
}

/// One competing host plus every corroborating observation.
#[derive(Clone, Debug)]
pub struct Detection {
    pub product: Product,
    pub evidence: Vec<Evidence>,
}

impl Detection {
    /// True when a matching process is live — a guaranteed bind clash.
    pub fn is_running(&self) -> bool {
        self.evidence
            .iter()
            .any(|e| matches!(e, Evidence::Running { .. }))
    }

    /// True when this host is running or will start on its own. Files-only or a
    /// disabled service is dormant (`false`); see the module docs.
    pub fn is_active(&self) -> bool {
        self.evidence.iter().any(Evidence::is_active)
    }

    /// One-line console label, e.g. `Sunshine (running)`. The qualifier is the
    /// observation, so a card cannot claim a dormant install is running.
    pub fn label(&self) -> String {
        let name = self.product.label();
        if self.is_running() {
            format!("{name} (running)")
        } else if self.is_active() {
            format!("{name} (starts automatically)")
        } else {
            format!("{name} (installed, not running)")
        }
    }
}

/// One row of the known-conflicting-host table. Names match case-insensitively;
/// process/binary basenames have **no** extension (platform code lowercases and
/// strips `.exe`). Add forks here; runtime, subcommand, and tray/console share it.
pub struct Known {
    pub product: Product,
    /// Lowercase executable basenames, no extension.
    pub processes: &'static [&'static str],
    /// SCM keys under `HKLM\SYSTEM\CurrentControlSet\Services`.
    #[cfg_attr(not(windows), allow(dead_code, reason = "the Windows scan reads it"))]
    pub win_services: &'static [&'static str],
    /// Install-dir basenames under `%ProgramFiles%` / `%ProgramFiles(x86)%`.
    #[cfg_attr(not(windows), allow(dead_code, reason = "the Windows scan reads it"))]
    pub win_dirs: &'static [&'static str],
    /// systemd unit basenames without `.service`.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux scans it"))]
    pub linux_units: &'static [&'static str],
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux scans it"))]
    pub flatpaks: &'static [&'static str],
}

/// Sunshine and forks that clash with Punktfunk. Add a row; every surface picks it up.
pub const KNOWN: &[Known] = &[
    Known {
        product: Product::Sunshine,
        processes: &["sunshine"],
        win_services: &["SunshineService"],
        win_dirs: &["Sunshine"],
        linux_units: &["sunshine"],
        flatpaks: &["dev.lizardbyte.app.Sunshine"],
    },
    Known {
        product: Product::Apollo,
        processes: &["apollo"],
        win_services: &["ApolloService"],
        win_dirs: &["Apollo"],
        linux_units: &["apollo"],
        flatpaks: &["dev.lizardbyte.app.Apollo"],
    },
    Known {
        product: Product::Vibeshine,
        processes: &["vibeshine"],
        win_services: &["VibeshineService"],
        win_dirs: &["Vibeshine"],
        linux_units: &["vibeshine"],
        flatpaks: &[],
    },
    Known {
        product: Product::Vibepollo,
        processes: &["vibepollo"],
        win_services: &["VibepolloService"],
        win_dirs: &["Vibepollo"],
        linux_units: &["vibepollo"],
        flatpaks: &[],
    },
    Known {
        product: Product::Luminalshine,
        processes: &["luminalshine"],
        win_services: &["LuminalShineService"],
        win_dirs: &["LuminalShine"],
        linux_units: &["luminalshine"],
        flatpaks: &[],
    },
];

/// Why running side-by-side breaks — shared by every surface (log, subcommand, installers).
pub const UNSUPPORTED_BLURB: &str =
    "Running Punktfunk alongside another Moonlight-compatible host \
(Sunshine and its forks) is UNSUPPORTED: they bind the same GameStream ports (47984/47989, \
47998-48010), advertise the same _nvstream mDNS name, and often install a conflicting \
virtual-display driver. Expect \"address already in use\" errors, failed pairing, and capture \
glitches. Stop and uninstall the other host, or don't run them at the same time.";

/// Scan for conflicting hosts. Real OS work — prefer cached [`init`]/[`snapshot`]
/// on hot paths.
pub fn scan() -> Vec<Detection> {
    let procs = platform::running_processes();
    let mut out = Vec::new();
    for known in KNOWN {
        let mut evidence: Vec<Evidence> = Vec::new();
        for p in &procs {
            if known.processes.iter().any(|n| p == n) {
                evidence.push(Evidence::Running { process: p.clone() });
            }
        }
        evidence.extend(platform::static_evidence(known));
        if !evidence.is_empty() {
            out.push(Detection {
                product: known.product,
                evidence,
            });
        }
    }
    out
}

static SNAPSHOT: OnceLock<Vec<Detection>> = OnceLock::new();

/// Scan once and cache for process lifetime. The conflict set does not change at
/// streaming granularity; `/local/summary` must not re-scan per poll.
pub fn init() -> &'static [Detection] {
    SNAPSHOT.get_or_init(scan)
}

/// Cached snapshot, or empty if [`init`] has not run. Does not scan.
pub fn snapshot() -> &'static [Detection] {
    SNAPSHOT.get().map(Vec::as_slice).unwrap_or(&[])
}

/// True if any detection is active. Shared by startup log, `detect-conflicts`,
/// and the console card.
pub fn any_active(detections: &[Detection]) -> bool {
    detections.iter().any(Detection::is_active)
}

/// Compact labels for the web-console summary (e.g. `["Sunshine (running)"]`).
///
/// Active detections only. Dormant leftovers stay off the conflicts card;
/// [`render_report`] still lists them for support.
pub fn summary_labels(detections: &[Detection]) -> Vec<String> {
    detections
        .iter()
        .filter(|d| d.is_active())
        .map(Detection::label)
        .collect()
}

/// Full report, split active vs dormant. Empty string when nothing was detected
/// (callers gate on `is_empty()`). The dormant section names leftover paths that
/// [`summary_labels`] omits, and says they need no action.
pub fn render_report(detections: &[Detection]) -> String {
    if detections.is_empty() {
        return String::new();
    }
    let bullet = |d: &Detection| {
        let ev = d
            .evidence
            .iter()
            .map(Evidence::render)
            .collect::<Vec<_>>()
            .join("; ");
        format!("  \u{2022} {} \u{2014} {ev}\n", d.product.label())
    };
    let (active, dormant): (Vec<_>, Vec<_>) = detections.iter().partition(|d| d.is_active());
    let mut s = String::new();
    if !active.is_empty() {
        s.push_str("Detected another game-streaming host on this machine.\n");
        s.push_str(UNSUPPORTED_BLURB);
        s.push_str("\n\nDetected:\n");
        for d in &active {
            s.push_str(&bullet(d));
        }
    }
    if !dormant.is_empty() {
        if !active.is_empty() {
            s.push('\n');
        }
        s.push_str(
            "Also present but DORMANT — not running and not set to start on its own, so it clashes \
with nothing and needs no action (typically leftovers from an uninstall):\n",
        );
        for d in &dormant {
            s.push_str(&bullet(d));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det(product: Product, evidence: Vec<Evidence>) -> Detection {
        Detection { product, evidence }
    }

    #[test]
    fn empty_report_and_labels() {
        assert!(render_report(&[]).is_empty());
        assert!(summary_labels(&[]).is_empty());
    }

    #[test]
    fn running_detection_is_flagged_and_labelled() {
        let d = det(
            Product::Sunshine,
            vec![
                Evidence::Running {
                    process: "sunshine".into(),
                },
                Evidence::Service {
                    name: "SunshineService".into(),
                    autostart: true,
                },
            ],
        );
        assert!(d.is_running());
        assert!(d.is_active());
        assert_eq!(d.label(), "Sunshine (running)");
    }

    #[test]
    fn a_leftover_install_dir_is_dormant_and_never_surfaces() {
        let d = det(
            Product::Apollo,
            vec![Evidence::Installed {
                at: "C:\\Program Files\\Apollo".into(),
            }],
        );
        assert!(!d.is_running());
        assert!(!d.is_active(), "files on disk cannot bind a port");
        assert_eq!(d.label(), "Apollo (installed, not running)");
        assert!(summary_labels(std::slice::from_ref(&d)).is_empty());
        assert!(!any_active(&[d]));
    }

    #[test]
    fn a_disabled_service_is_dormant_but_an_autostart_one_is_not() {
        let disabled = det(
            Product::Sunshine,
            vec![Evidence::Service {
                name: "SunshineService".into(),
                autostart: false,
            }],
        );
        assert!(!disabled.is_active());
        assert!(summary_labels(&[disabled]).is_empty());

        let auto = det(
            Product::Sunshine,
            vec![Evidence::Service {
                name: "SunshineService".into(),
                autostart: true,
            }],
        );
        assert!(auto.is_active());
        assert!(!auto.is_running(), "registered to start != started");
        assert_eq!(auto.label(), "Sunshine (starts automatically)");
        assert_eq!(
            summary_labels(&[auto]),
            vec!["Sunshine (starts automatically)".to_string()]
        );
    }

    #[test]
    fn report_separates_active_from_dormant_and_keeps_the_blurb() {
        let active = det(
            Product::Sunshine,
            vec![Evidence::Running {
                process: "sunshine".into(),
            }],
        );
        let dormant = det(
            Product::Apollo,
            vec![Evidence::Installed {
                at: "/usr/bin/apollo".into(),
            }],
        );
        let report = render_report(&[active.clone(), dormant.clone()]);
        assert!(report.contains("UNSUPPORTED"));
        // Bullets name the product, not `Detection::label` — that qualifier would
        // restate the evidence after the dash. Qualifier is for `summary_labels`.
        assert!(report.contains("Sunshine \u{2014} running now (sunshine)"));
        assert!(report.contains("DORMANT"));
        assert!(report.contains("Apollo \u{2014} installed at /usr/bin/apollo"));
        assert_eq!(
            summary_labels(&[active, dormant.clone()]),
            vec!["Sunshine (running)".to_string()]
        );

        let dormant_only = render_report(&[dormant]);
        assert!(dormant_only.contains("DORMANT"));
        assert!(
            !dormant_only.contains("UNSUPPORTED"),
            "a leftover folder must not read as an unsupported dual-host setup:\n{dormant_only}"
        );
    }

    #[test]
    fn known_table_rows_are_well_formed() {
        // Process name plus Windows service: runtime scan and installer registry check agree.
        for k in KNOWN {
            assert!(
                !k.processes.is_empty(),
                "{:?} has no process name",
                k.product
            );
            assert!(
                !k.win_services.is_empty(),
                "{:?} has no Windows service name",
                k.product
            );
        }
    }
}
