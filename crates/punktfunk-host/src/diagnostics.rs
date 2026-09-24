//! Host diagnostics: structured health verdicts this host already computes.
//!
//! The registry lives here. Probes stay in `pf-inject` / `pf-vdisplay` / [`crate::detect`] and
//! export plain verdict enums — those crates never learn [`HostCheck`]. This module maps
//! verdict → check and owns every wire string.
//!
//! * English fallback is mandatory. Console and host ship as separate packages (N with N±1),
//!   so an unknown `id` renders the wire text. [`tests::every_non_ok_check_carries_fallback_text`].
//! * `inapplicable` is a status, not an absent row: the troubleshooting page still answers
//!   why the check does not apply here.
//!
//! Served by `mgmt/diagnostics.rs` on the authenticated admin lane. The unauthenticated
//! loopback summary is counts only ([`Diagnostics::attention_counts`]).

use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};
use utoipa::ToSchema;

pub(crate) mod catalog;

/// Console i18n keys. Renaming one drops the translation back to the wire fallback.
pub mod ids {
    pub const TAKEOVER_PRIVILEGE: &str = "takeover_privilege";
    pub const VIRTUAL_DECK_VHCI: &str = "virtual_deck_vhci";
    pub const UINPUT_ACCESS: &str = "uinput_access";
    pub const SERVER_CONFLICT: &str = "server_conflict";
    pub const HYPRLAND_PERMISSIONS: &str = "hyprland_permissions";
    pub const OMARCHY_UPDATES: &str = "omarchy_updates";
    pub const VDISPLAY_DRIVER: &str = "vdisplay_driver";
    pub const PAD_AUDIO: &str = "pad_audio";
    pub const PAD_DRIVER: &str = "pad_driver";
    pub const PLUGIN_SANDBOX: &str = "plugin_sandbox";
    pub const RESTART_PENDING: &str = "restart_pending";
}

/// Probe result. `Inapplicable` is not `Ok`: "never on this box" and "works here" are different
/// answers on the troubleshooting page.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Ok,
    Warn,
    Fail,
    Inapplicable,
}

impl CheckStatus {
    pub fn needs_attention(self) -> bool {
        matches!(self, CheckStatus::Warn | CheckStatus::Fail)
    }
}

/// Weight of a non-ok status. Orthogonal to [`CheckStatus`]: a check can `warn` about something
/// `critical` (degraded, not dead). The console sorts by both.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    fn rank(self) -> u8 {
        match self {
            Severity::Critical => 0,
            Severity::Warning => 1,
            Severity::Info => 2,
        }
    }
}

/// Operator action. Copy-paste only: the host is unprivileged and does not run the command.
/// Joining `punktfunk` is opt-in — write on the vhci `attach` node materialises arbitrary USB.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct Remedy {
    /// English fallback. The console overrides it by check id.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Fix takes effect only after logout. `systemd --user` keeps the group set it started with.
    pub relogin_required: bool,
}

/// Origin of a verdict. `Event` is for live feeds (push on transition); v1 emits `Startup` and
/// `Refresh` only.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CheckSource {
    Startup,
    Event,
    Refresh,
}

/// One health verdict — the wire shape.
#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct HostCheck {
    /// Console i18n key. See [`ids`].
    pub id: String,
    pub status: CheckStatus,
    /// Meaningless on `ok`/`inapplicable`; carried so the wire shape never changes as status flips.
    pub severity: Severity,
    /// English fallback. Console localizes when it knows `id`.
    pub summary: String,
    /// What breaks. Empty only on `ok`/`inapplicable`.
    pub impact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<Remedy>,
    /// Interpolation for localized strings (`{user}`, `{group}`). Only the host can see these.
    pub params: BTreeMap<String, String>,
    /// First non-ok observation this run. Per-run stamp, not a history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_unix: Option<u64>,
    pub source: CheckSource,
}

impl HostCheck {
    pub fn ok(id: &str, summary: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: CheckStatus::Ok,
            severity: Severity::Info,
            summary: summary.into(),
            impact: String::new(),
            remedy: None,
            params: BTreeMap::new(),
            since_unix: None,
            source: CheckSource::Startup,
        }
    }

    /// Does not apply here. `why` is the gate, so the troubleshooting page can still show the row.
    pub fn inapplicable(id: &str, why: impl Into<String>) -> Self {
        Self {
            id: id.to_string(),
            status: CheckStatus::Inapplicable,
            severity: Severity::Info,
            summary: why.into(),
            impact: String::new(),
            remedy: None,
            params: BTreeMap::new(),
            since_unix: None,
            source: CheckSource::Startup,
        }
    }

    /// A problem. `impact` is required: a warning without a consequence is un-actionable.
    pub fn problem(
        id: &str,
        status: CheckStatus,
        severity: Severity,
        summary: impl Into<String>,
        impact: impl Into<String>,
    ) -> Self {
        Self {
            id: id.to_string(),
            status,
            severity,
            summary: summary.into(),
            impact: impact.into(),
            remedy: None,
            params: BTreeMap::new(),
            since_unix: None,
            source: CheckSource::Startup,
        }
    }

    pub fn with_remedy(mut self, remedy: Remedy) -> Self {
        self.remedy = Some(remedy);
        self
    }

    pub fn with_param(mut self, key: &str, value: impl Into<String>) -> Self {
        self.params.insert(key.to_string(), value.into());
        self
    }

    /// Id last so the list is stable across refresh.
    fn order_key(&self) -> (u8, u8, u8, &str) {
        let bucket = match self.status {
            CheckStatus::Fail | CheckStatus::Warn => 0,
            CheckStatus::Ok => 1,
            CheckStatus::Inapplicable => 2,
        };
        let status_rank = match self.status {
            CheckStatus::Fail => 0,
            CheckStatus::Warn => 1,
            _ => 2,
        };
        (bucket, self.severity.rank(), status_rank, &self.id)
    }
}

/// The `GET /diagnostics` body.
#[derive(Serialize, ToSchema, Clone, Debug)]
pub struct DiagnosticsReport {
    /// Last probe run, unix seconds.
    pub ran_at_unix: u64,
    /// Every registered check, worst-first, including `ok` and `inapplicable`. The console decides
    /// what to hide.
    pub checks: Vec<HostCheck>,
}

type Probe = Box<dyn Fn() -> HostCheck + Send + Sync>;

#[derive(Default)]
pub struct Diagnostics {
    probes: RwLock<Vec<Probe>>,
    checks: RwLock<BTreeMap<String, HostCheck>>,
    ran_at_unix: RwLock<u64>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a probe. Cheap by contract (`open`, `getgrnam`, a stat): `POST /diagnostics/refresh`
    /// re-runs every probe synchronously.
    pub fn register(&self, probe: impl Fn() -> HostCheck + Send + Sync + 'static) {
        self.probes.write().unwrap().push(Box::new(probe));
    }

    /// Re-run every probe. `since_unix` is kept for a check that was already non-ok.
    pub fn run_all(&self, source: CheckSource) {
        // Probes run without the checks lock: they hit the filesystem and `id`. A slow NSS
        // lookup must not block a concurrent GET.
        let fresh: Vec<HostCheck> = {
            let probes = self.probes.read().unwrap();
            probes.iter().map(|p| p()).collect()
        };
        let now = now_unix();
        let mut checks = self.checks.write().unwrap();
        for mut check in fresh {
            let previous_since = prior_since(checks.get(&check.id));
            check.source = source;
            carry_since(&mut check, previous_since, now);
            checks.insert(check.id.clone(), check);
        }
        *self.ran_at_unix.write().unwrap() = now;
    }

    /// Push one event-source verdict. Returns whether *status* changed so the caller emits SSE
    /// on a transition, not once per backoff retry.
    pub fn set(&self, mut check: HostCheck) -> bool {
        let now = now_unix();
        let mut checks = self.checks.write().unwrap();
        let previous = checks.get(&check.id);
        let changed = previous.is_none_or(|p| p.status != check.status);
        let previous_since = prior_since(previous);
        check.source = CheckSource::Event;
        carry_since(&mut check, previous_since, now);
        checks.insert(check.id.clone(), check);
        changed
    }

    pub fn report(&self) -> DiagnosticsReport {
        let checks = self.checks.read().unwrap();
        let mut checks: Vec<HostCheck> = checks.values().cloned().collect();
        checks.sort_by(|a, b| a.order_key().cmp(&b.order_key()));
        DiagnosticsReport {
            ran_at_unix: *self.ran_at_unix.read().unwrap(),
            checks,
        }
    }

    /// Unauthenticated loopback summary: counts by severity, never details.
    #[allow(dead_code)] // tray `LocalSummary`
    pub fn attention_counts(&self) -> (u32, u32) {
        let checks = self.checks.read().unwrap();
        let mut warning = 0;
        let mut critical = 0;
        for c in checks.values().filter(|c| c.status.needs_attention()) {
            match c.severity {
                Severity::Critical => critical += 1,
                _ => warning += 1,
            }
        }
        (warning, critical)
    }
}

/// Stamp to inherit while still unhealthy. `None` after recovery so a later relapse is dated
/// from the relapse, not the original.
fn prior_since(previous: Option<&HostCheck>) -> Option<u64> {
    previous
        .filter(|p| p.status.needs_attention())
        .and_then(|p| p.since_unix)
}

fn carry_since(check: &mut HostCheck, previous_since: Option<u64>, now: u64) {
    check.since_unix = check
        .status
        .needs_attention()
        .then(|| previous_since.unwrap_or(now));
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Process-wide registry, not an `AppState` field: one set of device nodes and group membership
/// per host, and mgmt handlers stay free of a state extractor (same shape as `hooks::store()`).
pub fn registry() -> &'static Diagnostics {
    static REGISTRY: OnceLock<Diagnostics> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let reg = Diagnostics::new();
        catalog::register_all(&reg);
        reg
    })
}

/// First reading, from `native::serve` after the probed subsystems are up. [`registry`] registers
/// the catalog lazily, so an early GET still sees the known set rather than empty.
pub fn preflight() {
    let reg = registry();
    reg.run_all(CheckSource::Startup);
    let report = reg.report();
    let attention = report
        .checks
        .iter()
        .filter(|c| c.status.needs_attention())
        .count();
    tracing::debug!(
        checks = report.checks.len(),
        attention,
        "diagnostics: startup probes complete"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn fail(id: &str) -> HostCheck {
        HostCheck::problem(
            id,
            CheckStatus::Fail,
            Severity::Critical,
            "broken",
            "nothing works",
        )
        .with_remedy(Remedy {
            text: "fix it".into(),
            command: None,
            relogin_required: false,
        })
    }

    #[test]
    fn refresh_reruns_every_probe() {
        let reg = Diagnostics::new();
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        reg.register(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            HostCheck::ok("probe", "fine")
        });

        reg.run_all(CheckSource::Startup);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(reg.report().checks[0].source, CheckSource::Startup);

        reg.run_all(CheckSource::Refresh);
        assert_eq!(runs.load(Ordering::SeqCst), 2, "refresh must re-run probes");
        assert_eq!(reg.report().checks[0].source, CheckSource::Refresh);
    }

    #[test]
    fn report_is_worst_first_with_healthy_and_inapplicable_last() {
        let reg = Diagnostics::new();
        reg.register(|| HostCheck::ok("b_ok", "fine"));
        reg.register(|| HostCheck::inapplicable("a_na", "no display manager"));
        reg.register(|| {
            HostCheck::problem(
                "c_warn",
                CheckStatus::Warn,
                Severity::Warning,
                "degraded",
                "half works",
            )
        });
        reg.register(|| fail("d_fail"));
        reg.run_all(CheckSource::Startup);

        let ids: Vec<String> = reg.report().checks.into_iter().map(|c| c.id).collect();
        assert_eq!(ids, ["d_fail", "c_warn", "b_ok", "a_na"]);
    }

    #[test]
    fn since_is_stamped_once_and_cleared_on_recovery() {
        let reg = Diagnostics::new();
        reg.set(fail("flapper"));
        let first = reg.report().checks[0].since_unix;
        assert!(first.is_some(), "a non-ok check records when it started");

        reg.set(fail("flapper"));
        assert_eq!(reg.report().checks[0].since_unix, first);

        reg.set(HostCheck::ok("flapper", "fine"));
        assert_eq!(reg.report().checks[0].since_unix, None);
    }

    #[test]
    fn set_reports_only_real_transitions() {
        let reg = Diagnostics::new();
        assert!(reg.set(fail("pads")), "first observation is a transition");
        assert!(
            !reg.set(fail("pads")),
            "a repeated identical verdict is not a transition — one SSE event per flip, not per retry"
        );
        assert!(
            reg.set(HostCheck::ok("pads", "fine")),
            "fail → ok is a transition"
        );
    }

    #[test]
    fn attention_counts_ignore_healthy_and_inapplicable_rows() {
        let reg = Diagnostics::new();
        reg.register(|| fail("bad"));
        reg.register(|| {
            HostCheck::problem(
                "meh",
                CheckStatus::Warn,
                Severity::Warning,
                "degraded",
                "half works",
            )
        });
        reg.register(|| HostCheck::ok("good", "fine"));
        reg.register(|| HostCheck::inapplicable("na", "not here"));
        reg.run_all(CheckSource::Startup);

        assert_eq!(reg.attention_counts(), (1, 1));
    }

    /// Console N with host N±1: unknown ids render the wire text, so every non-ok row must ship
    /// fallback copy.
    #[test]
    fn every_non_ok_check_carries_fallback_text() {
        let reg = Diagnostics::new();
        catalog::register_all(&reg);
        reg.run_all(CheckSource::Startup);

        for check in reg.report().checks {
            assert!(
                !check.summary.trim().is_empty(),
                "{}: every check needs a summary — it is the only text an older console has",
                check.id
            );
            if check.status.needs_attention() {
                assert!(
                    !check.impact.trim().is_empty(),
                    "{}: a non-ok check must say what breaks",
                    check.id
                );
            }
            if check.status == CheckStatus::Fail {
                let remedy = check
                    .remedy
                    .as_ref()
                    .unwrap_or_else(|| panic!("{}: a failing check must carry a remedy", check.id));
                assert!(
                    !remedy.text.trim().is_empty(),
                    "{}: remedy text must not be empty",
                    check.id
                );
            }
        }
    }

    /// Ids are API (console i18n keys). A rename drops every translation back to English.
    #[test]
    fn catalog_registers_the_documented_ids() {
        let reg = Diagnostics::new();
        catalog::register_all(&reg);
        reg.run_all(CheckSource::Startup);

        let ids: Vec<String> = reg.report().checks.into_iter().map(|c| c.id).collect();
        for expected in [
            ids::TAKEOVER_PRIVILEGE,
            ids::VIRTUAL_DECK_VHCI,
            ids::UINPUT_ACCESS,
            ids::SERVER_CONFLICT,
            ids::HYPRLAND_PERMISSIONS,
            ids::OMARCHY_UPDATES,
            ids::VDISPLAY_DRIVER,
        ] {
            assert!(
                ids.iter().any(|i| i == expected),
                "missing check {expected}"
            );
        }
    }
}
