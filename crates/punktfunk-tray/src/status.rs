//! Host status model and the poller that feeds the platform trays.
//!
//! Service-manager state is first: SCM (Windows) / systemd user unit (Linux)
//! decides stopped-vs-running. A listener on the mgmt port while the service is
//! down cannot make the tray say Running. After Running, the poller reads
//! loopback `GET /api/v1/local/summary` for streaming detail, with the bearer
//! the host leaves in `<config_dir>/tray-token` for every local account. A
//! denied read means this account is not the host's: the tray exits.
//!
//! Linux pins the mgmt agent to the host identity cert when the same-user file
//! is readable. Windows cannot: the cert is SYSTEM/Admins-DACL'd. Platform
//! trays: `linux.rs`, `win.rs`.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum ServiceState {
    NotInstalled,
    Stopped,
    StartPending,
    StopPending,
    Running,
    /// systemd `ActiveState=failed` (SubState in the string), or a Windows stop with a non-clean exit.
    Failed(String),
}

/// `GET /api/v1/local/summary` (`LocalSummary` in mgmt.rs). Unknown fields are ignored.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Summary {
    pub version: String,
    pub video_streaming: bool,
    pub audio_streaming: bool,
    pub session: Option<SessionInfo>,
    /// Display name for the connect toast. Absent when idle or nameless.
    #[serde(default)]
    pub client_name: Option<String>,
    pub paired_clients: u32,
    pub native_paired_clients: u32,
    pub pin_pending: bool,
    pub pending_approvals: u32,
    /// Lingering/pinned virtual displays; 0 when omitted.
    #[serde(default)]
    pub kept_displays: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
pub struct SessionInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TrayStatus {
    NotInstalled,
    Stopped,
    /// StartPending, or Running with no summary yet (within [`START_GRACE`]).
    Starting,
    Running(Summary),
    /// Running, summary unreachable past [`START_GRACE`]. Not a service failure:
    /// a custom `PUNKTFUNK_HOST_CMD` or relocated `--mgmt-bind` is legitimate.
    Degraded,
    Error(String),
}

impl TrayStatus {
    pub fn headline(&self) -> String {
        match self {
            TrayStatus::NotInstalled => "punktfunk host — not installed".into(),
            TrayStatus::Stopped => "punktfunk host — stopped".into(),
            TrayStatus::Starting => "punktfunk host — starting…".into(),
            TrayStatus::Degraded => "punktfunk host — running (status unavailable)".into(),
            TrayStatus::Error(_) => "punktfunk host — stopped unexpectedly".into(),
            TrayStatus::Running(s) => match (&s.session, self.is_streaming()) {
                (Some(sess), true) => format!(
                    "punktfunk host {} — streaming {}×{}@{}",
                    s.version, sess.width, sess.height, sess.fps
                ),
                (_, true) => format!("punktfunk host {} — streaming", s.version),
                // A kept display can hold physical monitors dark (exclusive topology).
                _ if s.kept_displays > 0 => format!(
                    "punktfunk host {} — idle · {} display{} kept",
                    s.version,
                    s.kept_displays,
                    if s.kept_displays == 1 { "" } else { "s" }
                ),
                _ => format!("punktfunk host {} — idle", s.version),
            },
        }
    }

    /// A live `session` counts even when `video_streaming` is false.
    pub fn is_streaming(&self) -> bool {
        matches!(self, TrayStatus::Running(s) if s.video_streaming || s.session.is_some())
    }

    /// Lingering/pinned virtual displays (0 unless Running). Holding one can keep physical monitors dark.
    pub fn kept_displays(&self) -> u32 {
        match self {
            TrayStatus::Running(s) => s.kept_displays,
            _ => 0,
        }
    }

    /// Pin or pending approval; the tray adds a menu entry.
    pub fn pairing_attention(&self) -> bool {
        matches!(self, TrayStatus::Running(s) if s.pin_pending || s.pending_approvals > 0)
    }

    /// The service is up or coming up, so Stop applies.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            TrayStatus::Running(_) | TrayStatus::Starting | TrayStatus::Degraded
        )
    }

    /// The service is installed and down, so Start applies.
    pub fn can_start(&self) -> bool {
        matches!(self, TrayStatus::Stopped | TrayStatus::Error(_))
    }

    /// Running, or stopped unexpectedly.
    pub fn can_restart(&self) -> bool {
        self.is_running() || matches!(self, TrayStatus::Error(_))
    }

    /// The release-kept-displays menu entry, `None` when nothing is kept.
    pub fn release_label(&self) -> Option<String> {
        match self.kept_displays() {
            0 => None,
            1 => Some("Release kept display…".into()),
            n => Some(format!("Release {n} kept displays…")),
        }
    }
}

/// Always shown: a dead console changes the label, never hides the row.
pub fn console_label(responding: bool) -> &'static str {
    if responding {
        "Open web console"
    } else {
        "Open web console (not responding)"
    }
}

/// The service restart. Clients' host-power "Restart host" reboots the machine
/// (`design/host-actions.md`), so one phrase must not mean both.
#[cfg(windows)]
pub const RESTART_LABEL: &str = "Restart Punktfunk";

/// Unreachable-summary window before Starting becomes Degraded. Re-armed while
/// Running so a child restart shows Starting, not Degraded.
pub const START_GRACE: Duration = Duration::from_secs(15);

pub fn map_status(svc: &ServiceState, summary: Option<Summary>, grace_expired: bool) -> TrayStatus {
    match svc {
        ServiceState::NotInstalled => TrayStatus::NotInstalled,
        ServiceState::Stopped | ServiceState::StopPending => TrayStatus::Stopped,
        ServiceState::StartPending => TrayStatus::Starting,
        ServiceState::Failed(e) => TrayStatus::Error(e.clone()),
        ServiceState::Running => match summary {
            Some(s) => TrayStatus::Running(s),
            None if !grace_expired => TrayStatus::Starting,
            None => TrayStatus::Degraded,
        },
    }
}

pub struct Poller {
    shared: Arc<Shared>,
}

struct Shared {
    poked: Mutex<bool>,
    cv: Condvar,
}

impl Poller {
    /// `on_change(status, console_up, companions)` from the poll thread. `console_up` is a
    /// loopback probe of `web_port`; it annotates "Open web console" rather than
    /// hiding the entry. `companions` follow the host in `UNITS` order; Windows has none.
    pub fn spawn(
        mgmt_addr: String,
        mgmt_port: Option<u16>,
        web_port: u16,
        on_change: Box<dyn Fn(TrayStatus, bool, Vec<TrayStatus>) + Send>,
    ) -> Poller {
        let shared = Arc::new(Shared {
            poked: Mutex::new(false),
            cv: Condvar::new(),
        });
        let thread_shared = shared.clone();
        std::thread::Builder::new()
            .name("status-poll".into())
            .spawn(move || poll_loop(&thread_shared, &mgmt_addr, mgmt_port, web_port, on_change))
            .expect("spawn status-poll thread");
        Poller { shared }
    }

    /// Wake the poller after a start/stop/restart menu action.
    pub fn poke(&self) {
        *self.shared.poked.lock().unwrap() = true;
        self.shared.cv.notify_one();
    }
}

fn poll_loop(
    shared: &Shared,
    mgmt_addr: &str,
    mgmt_port: Option<u16>,
    web_port: u16,
    on_change: Box<dyn Fn(TrayStatus, bool, Vec<TrayStatus>) + Send>,
) {
    // Per tick, not once: a captured port misses a republished
    // `PUNKTFUNK_MGMT_BIND` after restart.
    let summary_url = || {
        let port = mgmt_port
            .or_else(pf_paths::published_mgmt_port)
            .unwrap_or(47990);
        // IPv6 literals must be bracketed.
        if mgmt_addr.contains(':') {
            format!("https://[{mgmt_addr}]:{port}/api/v1/local/summary")
        } else {
            format!("https://{mgmt_addr}:{port}/api/v1/local/summary")
        }
    };
    // `/login`, not `/`: `/` 302s, and `max_redirects(0)` does not follow it.
    let console_url = format!("https://127.0.0.1:{web_port}/login");
    // Not `agent`: that name shadows the fn and the next call would bind this value.
    let mut pin = load_pin();
    let mut mgmt_agent = agent(pin);
    // Unpinned: the console is a different server and may present a different cert.
    let console_agent = agent(None);
    let mut last: Option<(TrayStatus, bool, Vec<TrayStatus>)> = None;
    // Grace timer for an unreachable summary while Running.
    let mut unreachable_since: Option<Instant> = None;
    // One miss is not down: a cold SSR can outrun the 2 s timeout.
    let mut console_misses = 0u32;
    loop {
        // The host can switch certificates under a running tray: a legacy-cert migration, an
        // identity reset. A stale pin would fail every fetch until the next login.
        let fresh = load_pin();
        if fresh != pin {
            pin = fresh;
            mgmt_agent = agent(pin);
        }
        let (svc, companions) = probe_services();
        let summary = if svc == ServiceState::Running {
            let s = fetch_summary(&mgmt_agent, &summary_url(), tray_token().as_deref());
            match s {
                Some(_) => unreachable_since = None,
                None if unreachable_since.is_none() => unreachable_since = Some(Instant::now()),
                None => {}
            }
            s
        } else {
            unreachable_since = None;
            None
        };
        let grace_expired = unreachable_since.is_some_and(|t| t.elapsed() >= START_GRACE);
        let status = map_status(&svc, summary, grace_expired);
        let console_up = if probe_console(&console_agent, &console_url) {
            console_misses = 0;
            true
        } else {
            console_misses += 1;
            console_misses < 2
        };
        // A companion serves no summary, so a running one maps to Degraded: running, no detail.
        let companions = companions
            .iter()
            .map(|c| map_status(c, None, true))
            .collect();
        let snapshot = (status, console_up, companions);
        if last.as_ref() != Some(&snapshot) {
            let (status, console_up, companions) = snapshot.clone();
            on_change(status, console_up, companions);
            last = Some(snapshot);
        }
        let cadence = match last.as_ref().map(|(s, _, _)| s) {
            Some(TrayStatus::Stopped) | Some(TrayStatus::NotInstalled) => Duration::from_secs(10),
            _ => Duration::from_secs(3),
        };
        let mut poked = shared.poked.lock().unwrap();
        if !*poked {
            (poked, _) = shared.cv.wait_timeout(poked, cadence).unwrap();
        }
        *poked = false;
    }
}

/// Any HTTP status (302, 401 included) is up; only a transport failure is down.
fn probe_console(agent: &ureq::Agent, url: &str) -> bool {
    match agent.get(url).call() {
        Ok(_) => true,
        Err(ureq::Error::StatusCode(..)) => true,
        Err(_) => false,
    }
}

/// Per poll, like the endpoint: the host mints a new token at every start. `None` is a host
/// that has not written one yet, or one older than the token; the request then goes bare.
/// A denied read is another account's host, so the tray has nothing to show and exits.
fn tray_token() -> Option<String> {
    let path = pf_paths::config_dir().join("tray-token");
    match std::fs::read_to_string(&path) {
        Ok(text) => pf_paths::env_file::get(&text, "PUNKTFUNK_TRAY_TOKEN").map(str::to_owned),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => std::process::exit(0),
        Err(_) => None,
    }
}

fn fetch_summary(agent: &ureq::Agent, url: &str, token: Option<&str>) -> Option<Summary> {
    let mut req = agent.get(url);
    if let Some(token) = token {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let body = req.call().ok()?.body_mut().read_to_string().ok()?;
    serde_json::from_str(&body).ok()
}

/// SHA-256 of the host identity cert when readable (Linux, same-user file).
/// Windows: `None` — the cert file is SYSTEM/Administrators-DACL'd.
fn load_pin() -> Option<[u8; 32]> {
    use rustls::pki_types::pem::PemObject;
    let dir = punktfunk_config_dir()?;
    // Prefer `native-cert.pem`; `cert.pem` is the GameStream identity still served when native is absent.
    let pem = std::fs::read(dir.join("native-cert.pem"))
        .or_else(|_| std::fs::read(dir.join("cert.pem")))
        .ok()?;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(&pem).ok()?;
    Some(punktfunk_core::tls::cert_fingerprint(der.as_ref()))
}

/// The host's [`pf_paths::config_dir`] where the tray may read it: on Linux, or wherever
/// `PUNKTFUNK_CONFIG_DIR` points. `None` otherwise: Windows' files are SYSTEM/Admins-DACL'd.
pub fn punktfunk_config_dir() -> Option<std::path::PathBuf> {
    let overridden = std::env::var_os("PUNKTFUNK_CONFIG_DIR").is_some_and(|d| !d.is_empty());
    (cfg!(target_os = "linux") || overridden).then(pf_paths::config_dir)
}

/// Sync HTTPS agent over [`punktfunk_core::tls::pinned_builder`]; `None` accepts any cert.
fn agent(pin: Option<[u8; 32]>) -> ureq::Agent {
    let cfg = punktfunk_core::tls::pinned_builder(punktfunk_core::tls::PinVerify::new(pin))
        .expect("rustls default protocol versions")
        .with_no_client_auth();
    // ureq `TlsConfig` cannot install a custom verifier; wrap `ClientConfig` via punktfunk-core.
    punktfunk_core::tls::ureq_agent::agent(
        Arc::new(cfg),
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_global(Some(Duration::from_secs(2)))
            // No redirects: the summary is a terminal JSON route; the console probe treats any HTTP answer as up.
            .max_redirects(0)
            .build(),
    )
}

/// SCM name written by `punktfunk-host service install` (`windows/service.rs`).
#[cfg(windows)]
pub const SERVICE_NAME: &str = "PunktfunkHost";

/// The host's state and its companions'. Windows has none: the service runs the console itself.
#[cfg(windows)]
fn probe_services() -> (ServiceState, Vec<ServiceState>) {
    (probe_service(), Vec::new())
}

#[cfg(windows)]
pub fn probe_service() -> ServiceState {
    use windows_service::service::{ServiceAccess, ServiceExitCode, ServiceState as Scm};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    // CONNECT + QUERY_STATUS are unprivileged. Re-open every poll: a reinstall invalidates old handles.
    let Ok(manager) = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    else {
        return ServiceState::NotInstalled;
    };
    let Ok(svc) = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS) else {
        return ServiceState::NotInstalled; // ERROR_SERVICE_DOES_NOT_EXIST and other open failures.
    };
    let Ok(status) = svc.query_status() else {
        return ServiceState::NotInstalled;
    };
    match status.current_state {
        Scm::StartPending => ServiceState::StartPending,
        Scm::StopPending => ServiceState::StopPending,
        Scm::Running | Scm::ContinuePending | Scm::PausePending | Scm::Paused => {
            ServiceState::Running
        }
        Scm::Stopped => match status.exit_code {
            // 0 = clean stop; 1077 = never started since boot. Both are Stopped, not Failed.
            ServiceExitCode::Win32(0) | ServiceExitCode::Win32(1077) => ServiceState::Stopped,
            ServiceExitCode::Win32(code) => ServiceState::Failed(format!("exit code {code}")),
            ServiceExitCode::ServiceSpecific(code) => {
                ServiceState::Failed(format!("service error {code}"))
            }
        },
    }
}

/// Systemd user units the Linux packages install (`scripts/punktfunk-*.service`), with their
/// menu names: the host, then its companions.
#[cfg(target_os = "linux")]
pub const UNITS: [(&str, &str); 3] = [
    ("punktfunk-host.service", "Host service"),
    ("punktfunk-web.service", "Web console"),
    ("punktfunk-scripting.service", "Plugin runner"),
];

#[cfg(target_os = "linux")]
pub const UNIT_NAME: &str = UNITS[0].0;

/// The host's state and its companions', from one `systemctl show` over [`UNITS`].
#[cfg(target_os = "linux")]
fn probe_services() -> (ServiceState, Vec<ServiceState>) {
    let out = std::process::Command::new("systemctl")
        .args([
            "--user",
            "show",
            "--property=LoadState,ActiveState,SubState",
        ])
        .args(UNITS.map(|(unit, _)| unit))
        .output();
    // No systemctl → nothing to watch.
    let text = out
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut states = parse_show(&text);
    let host = states.remove(0);
    (host, states)
}

/// One blank-line-separated block per unit, in argument order. `systemctl show` exits 0 for
/// unknown units (`LoadState=not-found`), so the text decides, not the exit code.
#[cfg(target_os = "linux")]
fn parse_show(text: &str) -> Vec<ServiceState> {
    let mut states: Vec<ServiceState> = text
        .split("\n\n")
        .map(|block| {
            let prop = |key: &str| {
                block
                    .lines()
                    .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                    .unwrap_or("")
            };
            match (prop("LoadState"), prop("ActiveState")) {
                // `systemctl --user mask` is how a user opts out of a unit.
                ("" | "not-found" | "masked", _) => ServiceState::NotInstalled,
                (_, "active" | "reloading") => ServiceState::Running,
                (_, "activating") => ServiceState::StartPending,
                (_, "deactivating") => ServiceState::StopPending,
                (_, "failed") => ServiceState::Failed(prop("SubState").into()),
                _ => ServiceState::Stopped, // "inactive" and anything new
            }
        })
        .collect();
    states.resize(UNITS.len(), ServiceState::NotInstalled);
    states
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(streaming: bool) -> Summary {
        Summary {
            version: "0.5.1".into(),
            video_streaming: streaming,
            audio_streaming: streaming,
            session: streaming.then_some(SessionInfo {
                width: 2560,
                height: 1440,
                fps: 120,
            }),
            client_name: streaming.then(|| "studio-deck".into()),
            paired_clients: 1,
            native_paired_clients: 2,
            pin_pending: false,
            pending_approvals: 0,
            kept_displays: 0,
        }
    }

    #[test]
    fn status_mapping_table() {
        use ServiceState as S;
        use TrayStatus as T;
        let cases: Vec<(S, Option<Summary>, bool, T)> = vec![
            (S::NotInstalled, None, false, T::NotInstalled),
            (S::Stopped, None, false, T::Stopped),
            (S::StopPending, None, false, T::Stopped),
            (S::StartPending, None, false, T::Starting),
            (
                S::Failed("code 3".into()),
                None,
                false,
                T::Error("code 3".into()),
            ),
            (
                S::Running,
                Some(summary(false)),
                true,
                T::Running(summary(false)),
            ),
            (S::Running, None, false, T::Starting),
            (S::Running, None, true, T::Degraded),
            // Stopped + a summary cannot happen in the poller; the mapping still trusts the service manager.
            (S::Stopped, Some(summary(true)), false, T::Stopped),
        ];
        for (svc, sum, grace, want) in cases {
            assert_eq!(
                map_status(&svc, sum.clone(), grace),
                want,
                "{svc:?} {sum:?} grace={grace}"
            );
        }
    }

    /// `conflicts` is host-side; the tray ignores it and must still deserialize.
    #[test]
    fn a_summary_carrying_conflicts_still_deserializes_and_is_ignored() {
        let json = r#"{"version":"0.5.1","video_streaming":false,"audio_streaming":false,
            "session":null,"paired_clients":1,"native_paired_clients":2,"pin_pending":false,
            "pending_approvals":0,"kept_displays":0,"conflicts":["Sunshine (installed)"]}"#;
        let s: Summary = serde_json::from_str(json).expect("unknown fields are ignored");
        assert_eq!(
            TrayStatus::Running(s).headline(),
            "punktfunk host 0.5.1 — idle"
        );
    }

    #[test]
    fn headline_shows_session_and_reason() {
        assert_eq!(
            TrayStatus::Running(summary(true)).headline(),
            "punktfunk host 0.5.1 — streaming 2560×1440@120"
        );
        assert_eq!(
            TrayStatus::Running(summary(false)).headline(),
            "punktfunk host 0.5.1 — idle"
        );
        assert!(TrayStatus::Error("exit code 3".into())
            .headline()
            .contains("stopped unexpectedly"));
        assert!(TrayStatus::Degraded
            .headline()
            .contains("status unavailable"));
    }

    /// A live session is streaming even when `video_streaming` is false.
    #[test]
    fn a_live_session_reads_as_streaming_without_the_flag() {
        let mut s = summary(true);
        s.video_streaming = false;
        let st = TrayStatus::Running(s);
        assert!(st.is_streaming());
        assert_eq!(
            st.headline(),
            "punktfunk host 0.5.1 — streaming 2560×1440@120"
        );
        assert!(!TrayStatus::Running(summary(false)).is_streaming());
    }

    #[test]
    fn kept_displays_are_reported_for_the_release_action() {
        assert_eq!(TrayStatus::Running(summary(false)).kept_displays(), 0);
        let mut s = summary(false);
        s.kept_displays = 2;
        assert_eq!(TrayStatus::Running(s).kept_displays(), 2);
        assert_eq!(TrayStatus::Degraded.kept_displays(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn systemctl_show_blocks_map_in_unit_order() {
        use ServiceState as S;
        let text = "LoadState=loaded\nActiveState=active\nSubState=running\n\n\
                    ActiveState=inactive\nLoadState=masked\nSubState=dead\n\n\
                    LoadState=loaded\nActiveState=failed\nSubState=exit-code\n";
        assert_eq!(
            parse_show(text),
            [S::Running, S::NotInstalled, S::Failed("exit-code".into())]
        );
        // No systemctl, or a short answer: whatever is missing is not installed.
        assert_eq!(
            parse_show(""),
            [S::NotInstalled, S::NotInstalled, S::NotInstalled]
        );
        assert_eq!(
            parse_show("LoadState=loaded\nActiveState=activating\n"),
            [S::StartPending, S::NotInstalled, S::NotInstalled]
        );
    }

    #[test]
    fn menu_rules_per_status() {
        use TrayStatus as T;
        // (status, running, start, restart)
        for (st, running, start, restart) in [
            (T::NotInstalled, false, false, false),
            (T::Stopped, false, true, false),
            (T::Starting, true, false, true),
            (T::Running(summary(false)), true, false, true),
            (T::Degraded, true, false, true),
            (T::Error("exit code 3".into()), false, true, true),
        ] {
            assert_eq!(st.is_running(), running, "{st:?}");
            assert_eq!(st.can_start(), start, "{st:?}");
            assert_eq!(st.can_restart(), restart, "{st:?}");
        }
    }

    #[test]
    fn release_label_counts_the_kept_displays() {
        let with = |n| {
            let mut s = summary(false);
            s.kept_displays = n;
            TrayStatus::Running(s).release_label()
        };
        assert_eq!(with(0), None);
        assert_eq!(with(1).as_deref(), Some("Release kept display…"));
        assert_eq!(with(3).as_deref(), Some("Release 3 kept displays…"));
        assert_eq!(TrayStatus::Stopped.release_label(), None);
    }

    #[test]
    fn pairing_attention_flags() {
        let mut s = summary(false);
        assert!(!TrayStatus::Running(s.clone()).pairing_attention());
        s.pending_approvals = 1;
        assert!(TrayStatus::Running(s.clone()).pairing_attention());
        s.pending_approvals = 0;
        s.pin_pending = true;
        assert!(TrayStatus::Running(s).pairing_attention());
        assert!(!TrayStatus::Degraded.pairing_attention());
    }
}
