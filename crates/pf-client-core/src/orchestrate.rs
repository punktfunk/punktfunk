//! Connect plans and the orchestrator that runs them
//! (`design/client-architecture-split.md`).
//!
//! A [`ConnectPlan`] is built from a card click, a CLI verb, or a URL. Front-ends
//! render; they do not decide when to prompt, how long to wait for a sleeping host,
//! or what counts as a refusal. A session exit reaches them as a [`ConnectOutcome`].
//!
//! Wake cadence lives on [`WAKE_TIMEOUT_SECS`] / [`WAKE_RESEND_SECS`].

use crate::deeplink::{DeepLink, HostResolution, Route};
use crate::presets::{PresetsFile, Resolution, StreamPreset};
use crate::session::{Dial, Probes, SessionParams};
use crate::trust::{KnownHost, KnownHosts, Settings};
use serde::{Deserialize, Serialize};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

/// Dial target as values. A plan-holder has no [`KnownHost`] in hand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostTarget {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// `None` = no pin. The session refuses that; only a completed trust ceremony may produce one.
    pub fp_hex: Option<String>,
    pub mac: Vec<String>,
    pub id: Option<String>,
    /// Management-API port (library), distinct from `port` (QUIC). Carried like `mac`:
    /// a plan-holder has no [`KnownHost`]. `None` = unknown, fall back to
    /// [`crate::library::DEFAULT_MGMT_PORT`].
    pub mgmt_port: Option<u16>,
}

impl From<&KnownHost> for HostTarget {
    fn from(h: &KnownHost) -> HostTarget {
        HostTarget {
            name: h.name.clone(),
            addr: h.addr.clone(),
            port: h.port,
            fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
            mac: h.mac.clone(),
            id: h.id.clone(),
            mgmt_port: h.mgmt_port,
        }
    }
}

/// Where a connect goes before it dials. Each front-end opens its own surface per arm.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustRoute {
    /// A stored pin: dial silently.
    Pinned(String),
    /// A new fingerprint where another is pinned: PIN only, even under `pair=optional`.
    /// It may be an impostor; the other OS of a dual-boot box pairs once with a PIN.
    FingerprintChanged,
    /// A new host advertising `pair=optional`: this fingerprint may be trusted on first use.
    OfferTofu(String),
    /// PIN or delegated approval.
    NeedsPairing,
}

/// The connect trust gate. `advertised_fp` follows [`KnownHosts::resolve_index`]: `None`
/// is a typed address and takes the record pinned there; `Some("")` is a card saved
/// without a pin and has none. A placeholder is no pin, so a fingerprint arriving at one
/// is a new host, not a changed one.
pub fn trust_route(
    known: &KnownHosts,
    advertised_fp: Option<&str>,
    addr: &str,
    port: u16,
    pair_optional: bool,
) -> TrustRoute {
    // `find_by_addr` prefers a pinned record, so this misses only when none is pinned here.
    let pinned_here = || {
        known
            .find_by_addr(addr, port)
            .filter(|h| !h.fp_hex.is_empty())
    };
    match advertised_fp {
        None => pinned_here().map_or(TrustRoute::NeedsPairing, |h| {
            TrustRoute::Pinned(h.fp_hex.clone())
        }),
        Some("") => TrustRoute::NeedsPairing,
        Some(fp) if known.find_by_fp(fp).is_some() => TrustRoute::Pinned(fp.to_string()),
        Some(_) if pinned_here().is_some() => TrustRoute::FingerprintChanged,
        Some(fp) if pair_optional => TrustRoute::OfferTofu(fp.to_string()),
        Some(_) => TrustRoute::NeedsPairing,
    }
}

/// One session, every policy question already answered. Front-ends do not re-decide.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectPlan {
    pub host: HostTarget,
    pub launch: Option<String>,
    pub preset: Option<StreamPreset>,
    /// One-off override handed to the session: `Some(id)` picks that preset,
    /// `Some("")` forces the defaults, `None` lets the session resolve the host binding.
    /// Both paths use the same resolver, so they cannot disagree.
    pub preset_override: Option<String>,
    pub settings: Settings,
    /// Magic packet first; wake-and-wait if the dial fails. Off with no MAC, and when
    /// auto-wake is off — VPN hosts look offline when they aren't.
    pub wake: bool,
    /// Handshake budget override. Request-access passes ~185 s: the host PARKS until
    /// an operator approves.
    pub connect_timeout_secs: Option<u64>,
    /// Pin came from an advert, not the store. Persist only after `ready` — that proves
    /// the host holds this identity.
    pub tofu: bool,
    /// Per-host trust decision, not a preset setting. Resolved here so the renderer
    /// does not look it up again.
    pub clipboard: bool,
}

/// Handshake budget when the plan and `--connect-timeout` both omit one.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 15;

impl ConnectPlan {
    /// Card-click plan. `one_off_preset`: `Some("")` forces the global defaults on a
    /// bound host; `None` honors the binding. Loads the catalog and settings; use
    /// [`ConnectPlan::resolve`] when the caller already holds them. The binding is
    /// `host`'s own: its address may also name the other OS of a dual-boot box.
    pub fn for_host(
        host: &KnownHost,
        launch: Option<&str>,
        one_off_preset: Option<&str>,
    ) -> ConnectPlan {
        Self::resolve(
            host,
            launch,
            one_off_preset,
            &PresetsFile::load(),
            &Settings::load(),
        )
    }

    /// Plan for a host the front-end already holds as values, not a stored [`KnownHost`].
    /// Resolves settings, preset, and clipboard through the same helpers as
    /// [`ConnectPlan::for_host`]. Hand-building the struct is a trap: [`spawn_session`]
    /// writes `settings` into `--resolved-spec`, and a spec-mode session reads no stores,
    /// so `..Settings::default()` silently streams at every default.
    pub fn for_target(
        host: HostTarget,
        launch: Option<String>,
        one_off_preset: Option<String>,
    ) -> ConnectPlan {
        let known = KnownHosts::load();
        // First connect off an advert: no record. Default = no binding, no clipboard. A
        // pin names only its own record, never the other OS saved at this address.
        let fallback = KnownHost::default();
        let stored = known
            .resolve(host.fp_hex.as_deref(), &host.addr, host.port)
            .unwrap_or(&fallback);
        let mut plan = ConnectPlan::resolve(
            stored,
            launch.as_deref(),
            one_off_preset.as_deref(),
            &PresetsFile::load(),
            &Settings::load(),
        );
        // Caller's target wins: its fingerprint may be TOFU (not yet stored), and `wake`
        // must follow this MAC, not the record's.
        plan.wake = plan.settings.auto_wake && !host.mac.is_empty();
        plan.host = host;
        plan
    }

    /// Same plan from stores the caller already holds — no disk, no clock, no
    /// environment. Precedence is `trust::resolve_preset`'s, called rather than
    /// restated: this path and [`effective_settings`] must not be able to disagree
    /// about which preset a launch gets.
    pub fn resolve(
        host: &KnownHost,
        launch: Option<&str>,
        one_off_preset: Option<&str>,
        catalog: &PresetsFile,
        base: &Settings,
    ) -> ConnectPlan {
        let preset = crate::trust::resolve_preset(
            catalog,
            host.preset_id.as_deref(),
            launch.and_then(|game| host.preset_for_game(game)),
            one_off_preset,
        );
        let settings = match &preset {
            Some(p) => p.overrides.apply(base),
            None => base.clone(),
        };
        ConnectPlan {
            host: HostTarget::from(host),
            launch: launch.map(str::to_string),
            preset,
            preset_override: one_off_preset.map(str::to_string),
            wake: settings.auto_wake && !host.mac.is_empty(),
            settings,
            connect_timeout_secs: None,
            tofu: false,
            clipboard: host.clipboard_sync,
        }
    }

    /// Spec for a first-party spawner so the session performs no store reads.
    pub fn spec(&self, clipboard: bool) -> ResolvedSpec {
        ResolvedSpec {
            settings: self.settings.clone(),
            clipboard,
            preset: self.preset.as_ref().map(|p| p.name.clone()),
            preset_id: self.preset.as_ref().map(|p| p.id.clone()),
        }
    }

    /// Session argv for this plan. Assembled once so shells cannot spawn different sessions.
    pub fn session_args(&self) -> Vec<String> {
        let mut args = vec![
            "--connect".into(),
            format!("{}:{}", self.host.addr, self.host.port),
        ];
        if let Some(fp) = &self.host.fp_hex {
            args.push("--fp".into());
            args.push(fp.clone());
        }
        if let Some(launch) = &self.launch {
            args.push("--launch".into());
            args.push(launch.clone());
        }
        // Only a one-off rides the flag. Without it the session resolves the host binding
        // through the same helper this plan used.
        if let Some(preset) = &self.preset_override {
            args.push("--preset".into());
            args.push(preset.clone());
        }
        if let Some(secs) = self.connect_timeout_secs {
            args.push("--connect-timeout".into());
            args.push(secs.to_string());
        }
        if self.settings.fullscreen_on_stream || self.settings.fullscreen_always() {
            args.push("--fullscreen".into());
        }
        // No `--window-pos`: Wayland compositors own placement, so the flag is a silent
        // no-op from GTK/CLI. Windows appends its own. An X11-only special case is drift.
        args
    }

    /// Pump parameters for this plan. Device probes stay off it: the plan is
    /// what a shell serialises, and it must not carry a device handle.
    ///
    /// The pin is the parsed form of [`HostTarget::fp_hex`]. An unset
    /// [`Self::connect_timeout_secs`] is [`DEFAULT_CONNECT_TIMEOUT_SECS`].
    pub fn session_params(&self, pin: [u8; 32], probes: Probes) -> SessionParams {
        self.spec(self.clipboard).session_params(
            Dial {
                host: self.host.addr.clone(),
                port: self.host.port,
                pin,
                launch: self.launch.clone(),
                connect_timeout: Duration::from_secs(
                    self.connect_timeout_secs
                        .unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS),
                ),
            },
            probes,
        )
    }
}

/// What a URL turned into. Unknown host = prompt, never connect; unimplemented
/// route = notice, never a silent no-op.
#[derive(Clone, Debug, PartialEq)]
pub enum PlanOutcome {
    Connect(Box<ConnectPlan>),
    /// Same plan, but the link named the host by a guessable label, address, or `host=`.
    /// `x-scheme-handler/punktfunk` lets any page emit that, so the front-end asks first.
    /// Not [`PlanOutcome::ConfirmUnknown`]: this host is saved and pinned; pairing would
    /// drop the pin.
    ConfirmConnect(Box<ConnectPlan>),
    /// No local record. Front-end shows the confirmation sheet; pairing/TOFU proceeds
    /// under the user.
    ConfirmUnknown(Box<UnknownHost>),
    Unsupported(Route),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownHost {
    pub addr: String,
    pub port: u16,
    /// Label the link claimed. Shown as claimed, never trusted.
    pub name: Option<String>,
    /// Fingerprint the link expects. Pre-fills the sheet so the first connect is
    /// verified, not blind TOFU.
    pub fp: Option<String>,
    pub launch: Option<String>,
    pub preset: Option<String>,
}

/// Why a link cannot become a plan. Each is a notice, never a degraded connect
/// (`design/client-deep-links.md`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanError {
    AmbiguousHost(String),
    UnresolvableHost(String),
    PinConflict { host: String },
    UnknownPreset(String),
    AmbiguousPreset(String),
}

impl PlanError {
    /// Notice text. Names the reference that failed — "it didn't work" on a
    /// shortcut is unactionable.
    pub fn message(&self) -> String {
        match self {
            PlanError::AmbiguousHost(r) => {
                format!("More than one saved host is called \"{r}\" — open Punktfunk and pick one.")
            }
            PlanError::UnresolvableHost(r) => {
                format!("No saved host matches \"{r}\".")
            }
            PlanError::PinConflict { host } => format!(
                "That link's fingerprint doesn't match the one saved for {host} — it's out of \
                 date, or it isn't that host. Nothing was connected."
            ),
            PlanError::UnknownPreset(p) => {
                format!("That link asks for a settings preset called \"{p}\", which doesn't exist here.")
            }
            PlanError::AmbiguousPreset(p) => {
                format!("More than one settings preset is called \"{p}\" — rename one, or use its id in the link.")
            }
        }
    }
}

/// Plan from a `punktfunk://` link against this device's stores. Shared URL-router
/// half (`design/client-architecture-split.md`): no pairing, no silent trust, no
/// dial on a guessable reference — only the stable record id yields
/// [`PlanOutcome::Connect`]; a name or address yields [`PlanOutcome::ConfirmConnect`].
///
/// Preempting a live session stays with the caller: only the front-end knows
/// whether a session is running, and "focus it" / "end that one first" is UI.
pub fn plan_from_link(
    link: &DeepLink,
    known: &KnownHosts,
    catalog: &PresetsFile,
    base: &Settings,
) -> Result<PlanOutcome, PlanError> {
    if link.route != Route::Connect {
        return Ok(PlanOutcome::Unsupported(link.route));
    }
    // Preset first: a link that cannot honor its preset must refuse rather than
    // stream with the wrong settings.
    if let Some(reference) = &link.preset {
        match catalog.resolve(reference) {
            (Some(_), _) => {}
            (_, Resolution::Ambiguous) => {
                return Err(PlanError::AmbiguousPreset(reference.clone()))
            }
            _ => return Err(PlanError::UnknownPreset(reference.clone())),
        }
    }
    let resolution = crate::deeplink::resolve_host(link, known);
    let confirm = matches!(resolution, HostResolution::Confirm(_));
    match resolution {
        HostResolution::Known(i) | HostResolution::Confirm(i) => {
            let host = &known.hosts[i];
            if link.pin_conflict(host) {
                return Err(PlanError::PinConflict {
                    host: host.name.clone(),
                });
            }
            let mut plan = ConnectPlan::resolve(
                host,
                link.launch.as_deref(),
                link.preset.as_deref(),
                catalog,
                base,
            );
            // Known but never pinned: the session refuses without a pin. Hand back as
            // ConfirmUnknown so the front-end runs its trust flow.
            if plan.host.fp_hex.is_none() {
                return Ok(PlanOutcome::ConfirmUnknown(Box::new(UnknownHost {
                    addr: plan.host.addr,
                    port: plan.host.port,
                    name: Some(plan.host.name),
                    fp: link.fp.clone(),
                    launch: link.launch.clone(),
                    preset: link.preset.clone(),
                })));
            }
            if plan.host.name.is_empty() {
                // Address-only record has no label. The link's claimed name is fine for
                // a window title; it names nothing that is trusted.
                plan.host.name = link.name.clone().unwrap_or_else(|| plan.host.addr.clone());
            }
            Ok(if confirm {
                PlanOutcome::ConfirmConnect(Box::new(plan))
            } else {
                PlanOutcome::Connect(Box::new(plan))
            })
        }
        HostResolution::Unknown {
            addr,
            port,
            name,
            fp,
        } => Ok(PlanOutcome::ConfirmUnknown(Box::new(UnknownHost {
            addr,
            port,
            name,
            fp,
            launch: link.launch.clone(),
            preset: link.preset.clone(),
        }))),
        HostResolution::Ambiguous => Err(PlanError::AmbiguousHost(link.host_ref.clone())),
        HostResolution::Unresolvable => Err(PlanError::UnresolvableHost(link.host_ref.clone())),
    }
}

/// Bound on the wake wait. A cold boot plus service start is routinely a minute-plus.
pub const WAKE_TIMEOUT_SECS: u64 = 90;
/// Magic-packet re-send while waiting. A single packet is missed, and some NICs
/// only wake on a fresh packet after dropping into a deeper sleep.
pub const WAKE_RESEND_SECS: u64 = 6;

/// Wake-and-wait as a one-second step so every front-end drives its own loop
/// and still agrees on the timings — and so the behavior is testable without
/// waiting 90 s.
#[derive(Clone, Debug)]
pub struct WakeWait {
    elapsed_secs: u64,
    timeout_secs: u64,
    resend_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeTick {
    pub send_packet: bool,
    pub seconds: u64,
    /// `None` = keep waiting (sleep one second, tick again).
    pub outcome: Option<WakeOutcome>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeOutcome {
    Online,
    /// Budget ran out. The UI PARKS (Try again / Cancel); it does not error out —
    /// "didn't wake in 90 s" is often "give it 10 more".
    TimedOut,
}

impl Default for WakeWait {
    fn default() -> WakeWait {
        WakeWait {
            elapsed_secs: 0,
            timeout_secs: WAKE_TIMEOUT_SECS,
            resend_secs: WAKE_RESEND_SECS,
        }
    }
}

impl WakeWait {
    pub fn new() -> WakeWait {
        WakeWait::default()
    }

    /// One second of the wait. `online` is this tick's presence reading (mDNS or
    /// a reachability probe). Packet before presence so an already-awake host
    /// costs one wasted packet, not a lost second; timeout after, so a host that
    /// appears on the last tick still wins.
    pub fn tick(&mut self, online: bool) -> WakeTick {
        let send_packet = self.elapsed_secs % self.resend_secs == 0;
        let seconds = self.elapsed_secs;
        let outcome = if online {
            Some(WakeOutcome::Online)
        } else if self.elapsed_secs >= self.timeout_secs {
            Some(WakeOutcome::TimedOut)
        } else {
            self.elapsed_secs += 1;
            None
        };
        WakeTick {
            send_packet,
            seconds,
            outcome,
        }
    }

    /// Replay the same wait. "Try again" after a timeout.
    pub fn restart(&mut self) {
        self.elapsed_secs = 0;
    }

    pub fn seconds(&self) -> u64 {
        self.elapsed_secs
    }
}

/// Wake-and-wait on this thread, with a reachability probe as the presence reading. Ticks
/// are paced to wall-clock seconds, so the probe's own wait does not stretch the budget.
/// `each` sees every tick before its packet goes out and returns `false` to stop; the
/// result is the last tick, `None` when `each` stopped it.
pub fn wake_by_probe(
    addr: &str,
    port: u16,
    fp_hex: &str,
    mac: &[String],
    mut each: impl FnMut(&WakeTick) -> bool,
) -> Option<WakeTick> {
    let last_ip = addr.parse().ok();
    let started = std::time::Instant::now();
    let mut wait = WakeWait::new();
    loop {
        let online = crate::trust::probe_one(addr, port, fp_hex, Duration::from_millis(900));
        let tick = wait.tick(online);
        if !each(&tick) {
            return None;
        }
        if tick.send_packet {
            crate::wol::wake(mac, last_ip);
        }
        if tick.outcome.is_some() {
            return Some(tick);
        }
        let next = Duration::from_secs(wait.seconds());
        std::thread::sleep(next.saturating_sub(started.elapsed()));
    }
}

/// How a connect finished. Front-ends map this onto their own surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectOutcome {
    /// Stream ended cleanly. `Some` is the host's stated reason.
    Ended(Option<String>),
    ConnectFailed(String),
    /// No pin, or the pin no longer matches. Never retried silently.
    TrustRejected(String),
    /// The session died without a contract line. `-1` = no exit code (a Unix signal).
    RendererFailed {
        code: i32,
    },
    /// Our own kill: Disconnect or a cancelled request.
    Cancelled,
}

impl ConnectOutcome {
    /// Classify a session exit: its code, the `error`/`ended` lines it spoke, and whether
    /// we killed it. A contract line says more than a code. `cancelled` covers whatever
    /// code our kill leaves: `-1` from a Unix signal, `1` from Windows' TerminateProcess.
    pub fn from_exit(
        code: i32,
        error: Option<(String, bool)>,
        ended: Option<String>,
        cancelled: bool,
    ) -> ConnectOutcome {
        match (code, error) {
            (_, Some((msg, true))) => ConnectOutcome::TrustRejected(msg),
            (_, Some((msg, false))) => ConnectOutcome::ConnectFailed(msg),
            (0, None) => ConnectOutcome::Ended(ended),
            _ if cancelled => ConnectOutcome::Cancelled,
            (code, None) => ConnectOutcome::RendererFailed { code },
        }
    }

    /// Whether the dial-first wake fallback runs: the dial failed, or the session died
    /// without a word. A trust rejection means the host answered; `-1` is a system kill.
    pub fn warrants_wake(&self) -> bool {
        match self {
            ConnectOutcome::ConnectFailed(_) => true,
            ConnectOutcome::RendererFailed { code } => *code != -1,
            _ => false,
        }
    }

    /// How a session that died silently went, for a banner. An NTSTATUS crash reads in
    /// hex, the form the Event Log and the crash filter use.
    pub fn exit_phrase(code: i32) -> String {
        match code as u32 {
            0xC000_0005 => "crashed with an access violation (0xC0000005)".to_string(),
            c if code < 0 => format!("died with exception 0x{c:08X}"),
            _ => format!("exited with code {code}"),
        }
    }
}

/// Everything a session needs, resolved by the caller — what `--resolved-spec`
/// carries (`design/client-architecture-split.md`).
///
/// The session is a renderer: given this, it performs no store reads. A hand-run
/// `punktfunk-session --connect` with no spec still resolves through the same
/// helper (`effective_settings`), so the two modes cannot drift.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResolvedSpec {
    pub settings: Settings,
    /// Per-host trust decision, resolved by the spawner — not re-looked-up here.
    pub clipboard: bool,
    /// Preset name for the stats overlay. `None` = the global defaults.
    #[serde(default, alias = "profile", skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The preset's stable id, which the dial names to the host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
}

impl ResolvedSpec {
    /// Write the spec somewhere the child can read, returning the path. A file, not
    /// a pipe: the session already takes a path, and a crashed spawner leaves
    /// something inspectable.
    ///
    /// CSPRNG name, `create_new` + 0600. On Linux this is `$XDG_RUNTIME_DIR`
    /// (per-user 0700), not shared `/tmp` — a predictable `punktfunk-spec-<pid>-<n>`
    /// name lets another local user pre-create the path as a symlink or swap the
    /// spec before the child reads it. A collision fails the exclusive create;
    /// the caller already falls back to letting the child resolve for itself.
    pub fn write_temp(&self) -> std::io::Result<std::path::PathBuf> {
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        #[cfg(target_os = "linux")]
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        // Windows %TEMP% and macOS $TMPDIR are already per-user private directories.
        #[cfg(not(target_os = "linux"))]
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "punktfunk-spec-{:032x}.json",
            u128::from_le_bytes(rand::random::<[u8; 16]>())
        ));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        use std::io::Write as _;
        opts.open(&path)?.write_all(&json)?;
        Ok(path)
    }

    pub fn read(path: &std::path::Path) -> std::io::Result<ResolvedSpec> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Pump parameters for a direct connect. Same fill as
    /// [`ConnectPlan::session_params`]. Host, pin, launch, and timeout come from
    /// the command line; this spec already holds settings, clipboard, and preset.
    pub fn session_params(&self, dial: Dial, probes: Probes) -> SessionParams {
        SessionParams::from_plan(
            &self.settings,
            self.clipboard,
            self.preset.clone(),
            self.preset_id.clone(),
            dial,
            probes,
        )
    }
}

/// One event from the session child's stdout contract (`{"ready":true}`, `stats-json:`,
/// `{"error":…}`, `{"ended":…}`, then EOF and an exit code). Parsed once so
/// shells cannot disagree about what "ready" or "trust rejected" means.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// First frame presented — the stream is up.
    Ready,
    /// One `stats-json:` window, once a second.
    Stats(Box<punktfunk_core::hud::StatsSnapshot>),
    Error {
        msg: String,
        trust_rejected: bool,
    },
    Ended(String),
    /// Session window logical size under match-window. The SPAWNER persists it:
    /// a renderer that load-modify-saves settings was a concurrent writer for a
    /// value only the parent needs.
    Window {
        w: u32,
        h: u32,
    },
    /// EOF: the child is gone. `-1` = killed by a signal.
    Exited(i32),
}

/// Parse one stdout line of the session contract. `None` for the text `stats:` line, which
/// is for a person reading a log, and for stray output.
pub fn parse_session_line(line: &str) -> Option<SessionEvent> {
    if let Some(json) = line.strip_prefix("stats-json: ") {
        return serde_json::from_str(json)
            .ok()
            .map(|s| SessionEvent::Stats(Box::new(s)));
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("ready").and_then(|r| r.as_bool()) == Some(true) {
        return Some(SessionEvent::Ready);
    }
    if let Some(msg) = v.get("error").and_then(|m| m.as_str()) {
        return Some(SessionEvent::Error {
            msg: msg.to_string(),
            trust_rejected: v.get("trust_rejected").and_then(|t| t.as_bool()) == Some(true),
        });
    }
    if let Some(msg) = v.get("ended").and_then(|m| m.as_str()) {
        return Some(SessionEvent::Ended(msg.to_string()));
    }
    if let Some(win) = v.get("window") {
        let dim = |k: &str| win.get(k).and_then(|n| n.as_u64()).map(|n| n as u32);
        if let (Some(w), Some(h)) = (dim("w"), dim("h")) {
            return Some(SessionEvent::Window { w, h });
        }
    }
    None
}

/// The session's exit codes, beside its stdout lines. 0 is a clean end.
pub mod exit {
    pub const CONNECT_FAILED: u8 = 2;
    /// No pin, the pin no longer matches, or pairing is required.
    pub const TRUST_REJECTED: u8 = 3;
    /// The stream window or the console did not start.
    pub const RENDERER_FAILED: u8 = 4;
}

/// One line the session writes on stdout; [`parse_session_line`] reads each back.
pub enum SessionLine<'a> {
    Ready,
    /// `trust_rejected: None` leaves the field out, which readers take as `false`.
    Error {
        msg: &'a str,
        trust_rejected: Option<bool>,
    },
    Ended(&'a str),
    Window {
        w: u32,
        h: u32,
    },
    /// `stats:` for a person reading a log, then `stats-json:` for a program.
    Stats {
        text: &'a str,
        snap: &'a punktfunk_core::hud::StatsSnapshot,
    },
}

impl SessionLine<'_> {
    fn render(&self) -> String {
        use serde_json::json;
        match self {
            SessionLine::Ready => json!({ "ready": true }).to_string(),
            SessionLine::Error {
                msg,
                trust_rejected: None,
            } => json!({ "error": msg }).to_string(),
            SessionLine::Error {
                msg,
                trust_rejected: Some(t),
            } => json!({ "error": msg, "trust_rejected": t }).to_string(),
            SessionLine::Ended(msg) => json!({ "ended": msg }).to_string(),
            // Not `json!`: its map sorts keys, and this line has always been `w` first.
            SessionLine::Window { w, h } => format!(r#"{{"window":{{"w":{w},"h":{h}}}}}"#),
            SessionLine::Stats { text, snap } => format!(
                "stats: {text}\nstats-json: {}",
                serde_json::to_string(snap).unwrap_or_default()
            ),
        }
    }
}

/// Write one contract line to stdout. Not `println!`: it panics on EPIPE, and the shell
/// can exit mid-stream. Status nobody is left to read costs nothing to lose.
pub fn emit(line: SessionLine) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{}", line.render());
    let _ = out.flush();
}

/// Persist a window size the session reported. The spawner's job, not the
/// renderer's — and only on a real change, so a session that never resizes
/// never touches the file.
pub fn persist_window_size(w: u32, h: u32) {
    let mut s = Settings::load();
    if (s.last_window_w, s.last_window_h) != (w, h) {
        s.last_window_w = w;
        s.last_window_h = h;
        s.save();
    }
}

/// Forget `known.hosts[i]` and save. Once saved, what is keyed on the record goes too: its
/// cached game catalog and action rows, and a default-host pointer that a later re-pair of
/// another box would otherwise inherit.
pub fn forget_host(known: &mut KnownHosts, i: usize) -> anyhow::Result<KnownHost> {
    let gone = known.hosts.remove(i);
    known.save()?;
    crate::library_cache::forget(&gone.fp_hex);
    crate::host_actions::invalidate(&gone.fp_hex);
    let mut settings = Settings::load();
    if crate::start::clear_default(&mut settings, gone.id.as_deref()) {
        settings.save();
    }
    Ok(gone)
}

/// Session binary: installed next to this executable, else `$PATH` (a dev run
/// out of `target/…` lands on the sibling).
pub fn session_binary() -> std::path::PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name(SESSION_BIN);
        if sibling.exists() {
            return sibling;
        }
    }
    SESSION_BIN.into()
}

#[cfg(windows)]
const SESSION_BIN: &str = "punktfunk-session.exe";
#[cfg(not(windows))]
const SESSION_BIN: &str = "punktfunk-session";

/// Records cancellation and kills the spawned session child. Cancellation remains
/// visible when it arrives before the child is armed or after an event is queued.
#[derive(Clone, Debug, Default)]
pub struct CancelHandle {
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl CancelHandle {
    pub fn kill(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    /// End the session the way its own window does: SIGTERM, which the session's SDL turns
    /// into a quit event and so a quit-close of the host. A child still up after `grace` is
    /// killed. Windows has no SIGTERM, so it kills at once.
    pub fn terminate(&self, grace: std::time::Duration) {
        self.cancelled.store(true, Ordering::SeqCst);
        #[cfg(unix)]
        {
            let slot = self.child.lock().unwrap();
            let Some(child) = slot.as_ref() else { return };
            // SAFETY: the lock is held, and the reaper takes the child out under it before
            // `wait`, so this pid is still our unreaped child and names no other process.
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
            drop(slot);
            let me = self.clone();
            let _ = std::thread::Builder::new()
                .name("pf-session-term".into())
                .spawn(move || {
                    std::thread::sleep(grace);
                    me.kill();
                });
        }
        #[cfg(not(unix))]
        {
            let _ = grace;
            self.kill();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Whether a child is spawned and not yet reaped.
    pub fn is_running(&self) -> bool {
        self.child.lock().unwrap().is_some()
    }
}

/// The session command for `plan`, and the `--resolved-spec` temp it names. Spec mode: the
/// child reads no stores and cannot disagree about a file either of us might write. A failed
/// write is not fatal — the child's compat path resolves the same values through the same helper.
pub fn session_command(plan: &ConnectPlan) -> (Command, Option<std::path::PathBuf>) {
    let mut cmd = Command::new(session_binary());
    let mut args = plan.session_args();
    let spec_path = match plan.spec(plan.clipboard).write_temp() {
        Ok(path) => {
            args.push("--resolved-spec".into());
            args.push(path.to_string_lossy().into_owned());
            Some(path)
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't write the resolved spec; the session will resolve for itself");
            None
        }
    };
    cmd.args(args);
    (cmd, spec_path)
}

/// Spawns the session for `plan` and supervises it. See [`spawn_child`].
pub fn spawn_session(
    plan: &ConnectPlan,
    cancel: Option<CancelHandle>,
    on_event: impl FnMut(SessionEvent) + Send + 'static,
) -> Result<CancelHandle, String> {
    let (cmd, spec_path) = session_command(plan);
    let slot = spawn_child(cmd, spec_path, cancel, std::io::stderr(), on_event)?;
    tracing::info!(
        host = %plan.host.addr, port = plan.host.port,
        preset = plan.preset.as_ref().map(|p| p.name.as_str()).unwrap_or("-"),
        "session binary spawned"
    );
    Ok(slot)
}

/// Spawns a session command and supervises its stdout contract on a reader thread.
/// Stderr goes to `stderr_sink` and the log ring. `spec_path` is deleted once the child
/// exits, or at once if it never starts. `cancel` accepts cancellation before or after the
/// child is armed; `None` creates a fresh handle. [`SessionEvent::Exited`] always arrives.
pub fn spawn_child(
    mut cmd: Command,
    spec_path: Option<std::path::PathBuf>,
    cancel: Option<CancelHandle>,
    stderr_sink: impl std::io::Write + Send + 'static,
    on_event: impl FnMut(SessionEvent) + Send + 'static,
) -> Result<CancelHandle, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Piped through the ring forwarder, not inherited: a GUI-only log export
        // otherwise holds everything except the stream it was exported about.
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            if let Some(path) = &spec_path {
                let _ = std::fs::remove_file(path);
            }
            return Err(format!("couldn't start {}: {e}", SESSION_BIN));
        }
    };
    if let Some(stderr) = child.stderr.take() {
        crate::logring::forward_child_stderr(stderr, stderr_sink);
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let slot = cancel.unwrap_or_default();
    *slot.child.lock().unwrap() = Some(child);
    if slot.is_cancelled() {
        slot.kill();
    }

    let reader_slot = slot.clone();
    let mut on_event = on_event;
    std::thread::Builder::new()
        .name("pf-session-io".into())
        .spawn(move || {
            use std::io::BufRead as _;
            for line in std::io::BufReader::new(stdout).lines() {
                let line = match line {
                    Ok(line) => line,
                    // One undecodable line must not end the contract — the child streams on,
                    // and the shell would simply stop hearing about it.
                    Err(e) if e.kind() == std::io::ErrorKind::InvalidData => continue,
                    Err(_) => break,
                };
                if let Some(ev) = parse_session_line(&line) {
                    if let SessionEvent::Window { w, h } = ev {
                        persist_window_size(w, h);
                    }
                    on_event(ev);
                }
            }
            // The child has read the spec by EOF. Leftover temps accumulate.
            if let Some(path) = &spec_path {
                let _ = std::fs::remove_file(path);
            }
            // Reap. A cancel-killed child lands here too; -1 = died on a signal.
            let code = reader_slot
                .child
                .lock()
                .unwrap()
                .take()
                .and_then(|mut c| c.wait().ok())
                .and_then(|s| s.code())
                .unwrap_or(-1);
            tracing::info!(code, "session binary exited");
            on_event(SessionEvent::Exited(code));
        })
        .map_err(|e| format!("session reader thread: {e}"))?;
    Ok(slot)
}

/// Become the session process (`--exec`): gamescope-wrapper needs the streaming
/// identity — a supervising parent would break focus and lifecycle. Never
/// returns on success. Windows has no `exec`, so this runs the child to
/// completion and exits with its code.
pub fn exec_session(plan: &ConnectPlan) -> std::io::Error {
    let mut cmd = Command::new(session_binary());
    cmd.args(plan.session_args());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.exec()
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(s) => std::process::exit(s.code().unwrap_or(1)),
            Err(e) => e,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deeplink;
    use crate::trust::StatsVerbosity;
    use punktfunk_core::config::{GamepadPref, Mode};
    use punktfunk_core::quic::HdrMeta;

    fn host(name: &str, addr: &str, id: &str, fp: &str) -> KnownHost {
        KnownHost {
            name: name.into(),
            addr: addr.into(),
            port: 9777,
            fp_hex: fp.into(),
            paired: true,
            mac: vec!["aa:bb:cc:dd:ee:ff".into()],
            id: Some(id.into()),
            ..Default::default()
        }
    }

    /// A stored pin dials; a new fingerprint where another is pinned asks for a PIN even
    /// under `pair=optional`; a placeholder holds no pin to change.
    #[test]
    fn trust_route_sends_a_changed_fingerprint_to_the_pin() {
        use TrustRoute::*;
        let (desk, other, fresh) = ("a".repeat(64), "b".repeat(64), "c".repeat(64));
        let (desk, other, fresh) = (desk.as_str(), other.as_str(), fresh.as_str());
        let known = KnownHosts {
            hosts: vec![
                host("Desk", "10.0.0.2", "1", desk),
                host("Typed", "10.0.0.3", "2", ""),
            ],
        };
        for (fp, addr, optional, want) in [
            (Some(desk), "10.0.0.2", false, Pinned(desk.into())),
            (Some(desk), "10.0.0.9", false, Pinned(desk.into())), // a moved lease
            (Some(other), "10.0.0.2", true, FingerprintChanged),
            (Some(other), "10.0.0.2", false, FingerprintChanged),
            (Some(fresh), "10.0.0.9", true, OfferTofu(fresh.into())),
            (Some(fresh), "10.0.0.9", false, NeedsPairing),
            (Some(fresh), "10.0.0.3", true, OfferTofu(fresh.into())),
            (Some(fresh), "10.0.0.3", false, NeedsPairing),
            // A card saved without a pin never borrows the one pinned at its address.
            (Some(""), "10.0.0.2", false, NeedsPairing),
            (Some(""), "10.0.0.3", true, NeedsPairing),
            // A typed address takes whatever is pinned there.
            (None, "10.0.0.2", false, Pinned(desk.into())),
            (None, "10.0.0.3", false, NeedsPairing),
            (None, "10.0.0.9", true, NeedsPairing),
        ] {
            let got = trust_route(&known, fp, addr, 9777, optional);
            assert_eq!(got, want, "{fp:?} at {addr}, optional {optional}");
        }
    }

    /// Packet at 0 and every 6 s, presence each second, 90 s of budget, park (not
    /// an error) at the end.
    #[test]
    fn wake_wait_matches_the_reference_cadence() {
        let mut w = WakeWait::new();
        let t = w.tick(false);
        assert!(t.send_packet);
        assert_eq!(t.seconds, 0);
        assert_eq!(t.outcome, None);
        for s in 1..6 {
            let t = w.tick(false);
            assert!(!t.send_packet, "no packet at {s}s");
            assert_eq!(t.seconds, s);
        }
        assert!(w.tick(false).send_packet);
        assert_eq!(w.seconds(), 7);

        let mut w = WakeWait::new();
        w.tick(false);
        let t = w.tick(true);
        assert_eq!(t.outcome, Some(WakeOutcome::Online));

        let mut w = WakeWait::new();
        for _ in 0..WAKE_TIMEOUT_SECS {
            assert_eq!(w.tick(false).outcome, None);
        }
        assert_eq!(w.seconds(), WAKE_TIMEOUT_SECS);
        let t = w.tick(false);
        assert_eq!(t.outcome, Some(WakeOutcome::TimedOut));
        // Parked, not advanced. A host that appears while parked still wins.
        assert_eq!(w.tick(false).outcome, Some(WakeOutcome::TimedOut));
        assert_eq!(w.tick(true).outcome, Some(WakeOutcome::Online));
        w.restart();
        assert_eq!(w.seconds(), 0);
        assert!(w.tick(false).send_packet);
    }

    /// One-off preset rides the flag; a host binding does not — the session
    /// resolves it with the same helper, so passing it would be a second source of truth.
    #[test]
    fn session_args_are_assembled_in_one_place() {
        let h = host(
            "Desk",
            "192.168.1.50",
            "11111111-2222-4333-8444-555555555555",
            &"a".repeat(64),
        );
        let mut plan = ConnectPlan {
            host: HostTarget::from(&h),
            launch: Some("steam:570".into()),
            preset: None,
            preset_override: None,
            settings: Settings {
                fullscreen_on_stream: false,
                ..Default::default()
            },
            wake: true,
            connect_timeout_secs: None,
            tofu: false,
            clipboard: false,
        };
        assert_eq!(
            plan.session_args(),
            vec![
                "--connect",
                "192.168.1.50:9777",
                "--fp",
                &"a".repeat(64),
                "--launch",
                "steam:570"
            ]
        );

        plan.preset_override = Some("aaaaaaaaaaaa".into());
        plan.connect_timeout_secs = Some(185);
        plan.settings.fullscreen_on_stream = true;
        let args = plan.session_args();
        assert!(args.windows(2).any(|w| w == ["--preset", "aaaaaaaaaaaa"]));
        assert!(args.windows(2).any(|w| w == ["--connect-timeout", "185"]));
        assert!(args.contains(&"--fullscreen".to_string()));
        // Fullscreen Always outranks a preset that turned streams fullscreen off.
        plan.settings.fullscreen_on_stream = false;
        plan.settings.set_fullscreen_always(true);
        assert!(plan.session_args().contains(&"--fullscreen".to_string()));

        // "Connect with ▸ Default settings" on a bound host is an empty override, not
        // the same as no override — it has to survive as a flag.
        plan.preset_override = Some(String::new());
        let args = plan.session_args();
        let i = args.iter().position(|a| a == "--preset").unwrap();
        assert_eq!(args[i + 1], "");
    }

    /// Unknown host is a prompt, a contradicted pin is a refusal, an unhonorable
    /// preset is a refusal, and an ambiguous reference is never guessed at.
    #[test]
    fn link_plans_refuse_rather_than_degrade() {
        let fp = "a".repeat(64);
        let known = KnownHosts {
            hosts: vec![
                host(
                    "Desk",
                    "192.168.1.50",
                    "11111111-2222-4333-8444-555555555555",
                    &fp,
                ),
                host(
                    "Couch",
                    "192.168.1.60",
                    "22222222-3333-4444-8555-666666666666",
                    "",
                ),
                host(
                    "Couch",
                    "192.168.1.61",
                    "33333333-4444-4555-8666-777777777777",
                    "",
                ),
            ],
        };
        // Pure inputs — the test never touches the config directory.
        let catalog = PresetsFile::default();
        let base = Settings::default();
        let plan =
            |url: &str| plan_from_link(&deeplink::parse(url).unwrap(), &known, &catalog, &base);

        let out = plan("punktfunk://connect/11111111-2222-4333-8444-555555555555").unwrap();
        match out {
            PlanOutcome::Connect(p) => {
                assert_eq!(p.host.addr, "192.168.1.50");
                assert_eq!(p.preset_override, None);
                assert!(p.host.fp_hex.is_some());
            }
            other => panic!("expected a connect, got {other:?}"),
        }

        // Same host named by its label: any page can guess that, so the shell must ask.
        // Not ConfirmUnknown — that would re-run pairing on an already-pinned host.
        match plan("punktfunk://connect/Desk").unwrap() {
            PlanOutcome::ConfirmConnect(p) => {
                assert_eq!(p.host.addr, "192.168.1.50");
                assert!(p.host.fp_hex.is_some());
            }
            other => panic!("expected a confirm-connect, got {other:?}"),
        }
        match plan("punktfunk://connect/192.168.1.50?launch=steam:570").unwrap() {
            PlanOutcome::ConfirmConnect(p) => assert_eq!(p.launch.as_deref(), Some("steam:570")),
            other => panic!("expected a confirm-connect, got {other:?}"),
        }

        assert_eq!(
            plan(&format!("punktfunk://connect/Desk?fp={}", "b".repeat(64))),
            Err(PlanError::PinConflict {
                host: "Desk".into()
            })
        );
        assert_eq!(
            plan("punktfunk://connect/Couch"),
            Err(PlanError::AmbiguousHost("Couch".into()))
        );
        assert_eq!(
            plan("punktfunk://connect/00000000-0000-4000-8000-000000000000"),
            Err(PlanError::UnresolvableHost(
                "00000000-0000-4000-8000-000000000000".into()
            ))
        );
        assert_eq!(
            plan("punktfunk://connect/Desk?profile=NoSuchProfile"),
            Err(PlanError::UnknownPreset("NoSuchProfile".into()))
        );
        // Unknown address: confirmation sheet, never auto-connect. Carries the claimed
        // name and expected pin so the first connect is verified, not TOFU.
        match plan(&format!(
            "punktfunk://connect/10.0.0.9:7000?name=Studio&fp={fp}"
        ))
        .unwrap()
        {
            PlanOutcome::ConfirmUnknown(u) => assert_eq!(
                *u,
                UnknownHost {
                    addr: "10.0.0.9".into(),
                    port: 7000,
                    name: Some("Studio".into()),
                    fp: Some(fp.clone()),
                    launch: None,
                    preset: None,
                }
            ),
            other => panic!("expected a confirmation, got {other:?}"),
        }
        // Saved but never pinned: known ≠ trusted.
        match plan("punktfunk://connect/192.168.1.60").unwrap() {
            PlanOutcome::ConfirmUnknown(u) => {
                assert_eq!(u.addr, "192.168.1.60");
                assert_eq!(u.name.as_deref(), Some("Couch"));
            }
            other => panic!("expected a confirmation, got {other:?}"),
        }
        assert!(matches!(
            plan("punktfunk://wake/Desk").unwrap(),
            PlanOutcome::Unsupported(Route::Wake)
        ));
    }

    /// Spec round-trips. A field lost here is a setting the stream silently doesn't get.
    #[test]
    fn resolved_spec_round_trips() {
        let spec = ResolvedSpec {
            settings: Settings {
                width: 2560,
                height: 1440,
                bitrate_kbps: 55000,
                codec: "av1".into(),
                present_priority: "smooth".into(),
                smooth_buffer: 2,
                vsync: false,
                allow_vrr: false,
                ..Default::default()
            },
            clipboard: true,
            preset: Some("Work".into()),
            preset_id: Some("3f9a0c11e2b4".into()),
        };
        let json = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<ResolvedSpec>(&json).unwrap(), spec);

        // No preset: the key is absent, not null.
        let plain = ResolvedSpec {
            preset: None,
            preset_id: None,
            ..spec.clone()
        };
        let json = serde_json::to_string(&plain).unwrap();
        assert!(!json.contains("preset"));
        assert_eq!(serde_json::from_str::<ResolvedSpec>(&json).unwrap(), plain);
    }

    /// Plan spec carries the resolved settings, the overlay name, and the host's
    /// clipboard decision — the renderer must not re-derive them.
    #[test]
    fn plan_spec_carries_what_the_session_may_not_re_derive() {
        let h = KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.50".into(),
            fp_hex: "a".repeat(64),
            clipboard_sync: true,
            preset_id: Some("aaaaaaaaaaaa".into()),
            ..Default::default()
        };
        let catalog = PresetsFile {
            version: 1,
            presets: vec![crate::presets::StreamPreset {
                id: "aaaaaaaaaaaa".into(),
                name: "Game".into(),
                overrides: crate::presets::SettingsOverlay {
                    bitrate_kbps: Some(80000),
                    ..Default::default()
                },
                ..crate::presets::StreamPreset::new("")
            }],
        };
        let plan = ConnectPlan::resolve(&h, None, None, &catalog, &Settings::default());
        let spec = plan.spec(plan.clipboard);
        assert_eq!(spec.settings.bitrate_kbps, 80000, "the overlay is baked in");
        assert_eq!(spec.preset.as_deref(), Some("Game"));
        assert!(spec.clipboard, "the host's decision, resolved once");
    }

    #[test]
    fn session_exits_classify_once() {
        use ConnectOutcome as O;
        // A contract line says more than a code.
        let trust = O::from_exit(3, Some(("pin".into(), true)), None, false);
        assert_eq!(trust, O::TrustRejected("pin".into()));
        assert!(!trust.warrants_wake(), "the host answered");
        let failed = O::from_exit(2, Some(("no route".into(), false)), None, false);
        assert_eq!(failed, O::ConnectFailed("no route".into()));
        assert!(failed.warrants_wake());
        assert_eq!(O::from_exit(0, None, None, false), O::Ended(None));
        assert_eq!(
            O::from_exit(0, None, Some("Host ended".into()), false),
            O::Ended(Some("Host ended".into()))
        );
        // Our own kill is silent whatever code it leaves: a Unix signal, or TerminateProcess's 1.
        assert_eq!(O::from_exit(-1, None, None, true), O::Cancelled);
        assert_eq!(O::from_exit(1, None, None, true), O::Cancelled);
        // Anything else that died silently is a failure, never a blank return.
        let crashed = O::from_exit(1, None, None, false);
        assert_eq!(crashed, O::RendererFailed { code: 1 });
        assert!(crashed.warrants_wake());
        assert!(
            !O::RendererFailed { code: -1 }.warrants_wake(),
            "a system kill"
        );
    }

    #[test]
    fn exit_phrase_names_an_ntstatus_in_hex() {
        assert_eq!(ConnectOutcome::exit_phrase(2), "exited with code 2");
        let av = ConnectOutcome::exit_phrase(-1073741819);
        assert!(av.contains("access violation (0xC0000005)"), "{av}");
        let other = ConnectOutcome::exit_phrase(0xC000_0409u32 as i32);
        assert!(other.contains("0xC0000409"), "{other}");
    }

    #[test]
    fn session_contract_lines() {
        assert_eq!(
            parse_session_line(r#"{"ready":true}"#),
            Some(SessionEvent::Ready)
        );
        assert_eq!(
            parse_session_line(r#"{"error":"no route","trust_rejected":false}"#),
            Some(SessionEvent::Error {
                msg: "no route".into(),
                trust_rejected: false
            })
        );
        assert_eq!(
            parse_session_line(r#"{"error":"pin","trust_rejected":true}"#),
            Some(SessionEvent::Error {
                msg: "pin".into(),
                trust_rejected: true
            })
        );
        assert_eq!(
            parse_session_line(r#"{"ended":"Host ended the session"}"#),
            Some(SessionEvent::Ended("Host ended the session".into()))
        );
        assert_eq!(
            parse_session_line(r#"{"window":{"w":1600,"h":900}}"#),
            Some(SessionEvent::Window { w: 1600, h: 900 })
        );
        // Half a window line is not an event — persisting half a size is worse than
        // ignoring it.
        assert_eq!(parse_session_line(r#"{"window":{"w":1600}}"#), None);
        assert_eq!(parse_session_line("stats: 1280×800@60 · 60 fps"), None);
        // The snapshot is an event; the text line is for a person reading a log.
        match parse_session_line(r#"stats-json: {"width":1280,"received":60}"#) {
            Some(SessionEvent::Stats(s)) => assert_eq!((s.width, s.received), (1280, 60)),
            other => panic!("stats line parsed as {other:?}"),
        }
        assert_eq!(parse_session_line(""), None);
        assert_eq!(parse_session_line(r#"{"other":1}"#), None);
    }

    /// Every line the session writes parses back to the event it meant, byte for byte
    /// where the wire form is pinned above.
    #[test]
    fn emitted_lines_parse_back() {
        let parse = |l: SessionLine| parse_session_line(&l.render());
        assert_eq!(SessionLine::Ready.render(), r#"{"ready":true}"#);
        assert_eq!(parse(SessionLine::Ready), Some(SessionEvent::Ready));
        let pin = SessionLine::Error {
            msg: "pin \"x\"\n\tno",
            trust_rejected: Some(true),
        };
        assert_eq!(
            parse(pin),
            Some(SessionEvent::Error {
                msg: "pin \"x\"\n\tno".into(),
                trust_rejected: true
            })
        );
        let bare = SessionLine::Error {
            msg: "no window",
            trust_rejected: None,
        };
        assert_eq!(bare.render(), r#"{"error":"no window"}"#);
        assert_eq!(
            parse(bare),
            Some(SessionEvent::Error {
                msg: "no window".into(),
                trust_rejected: false
            })
        );
        assert_eq!(
            parse(SessionLine::Ended("Host ended")),
            Some(SessionEvent::Ended("Host ended".into()))
        );
        let win = SessionLine::Window { w: 1600, h: 900 };
        assert_eq!(win.render(), r#"{"window":{"w":1600,"h":900}}"#);
        assert_eq!(parse(win), Some(SessionEvent::Window { w: 1600, h: 900 }));
        let snap = punktfunk_core::hud::StatsSnapshot {
            width: 1280,
            received: 60,
            ..Default::default()
        };
        let stats = SessionLine::Stats {
            text: "1280×800@60",
            snap: &snap,
        }
        .render();
        let mut lines = stats.lines();
        assert_eq!(lines.next(), Some("stats: 1280×800@60"));
        match lines.next().and_then(parse_session_line) {
            Some(SessionEvent::Stats(s)) => assert_eq!((s.width, s.received), (1280, 60)),
            other => panic!("stats-json parsed as {other:?}"),
        }
    }

    /// No GPU and no store. Host, launch, clipboard, timeout, bitrate, and codec
    /// come from the plan. Caps are the wire bits, not whatever the helper returns.
    #[test]
    fn session_params_carry_the_plan_without_a_gpu() {
        let h = host(
            "Desk",
            "192.168.1.50",
            "11111111-2222-4333-8444-555555555555",
            &"ab".repeat(32),
        );
        let plan = ConnectPlan {
            host: HostTarget::from(&h),
            launch: Some("steam:570".into()),
            preset: None,
            preset_override: None,
            settings: Settings {
                bitrate_kbps: 42_000,
                codec: "av1".into(),
                enable_444: true,
                ..Default::default()
            },
            wake: false,
            connect_timeout_secs: Some(45),
            tofu: false,
            clipboard: true,
        };
        let force_software = Arc::new(AtomicBool::new(false));
        let latch_grid = Arc::new(crate::session::LatchGrid::default());
        let probes = |hdr_enabled: bool, hevc_444_hardware: bool| Probes {
            mode: Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 10,
            },
            identity: ("client".into(), "desk".into()),
            vulkan: None,
            force_software: Arc::clone(&force_software),
            gamepad: GamepadPref::Xbox360,
            hdr_enabled,
            display_hdr: Some(HdrMeta::default()),
            hevc_444_hardware,
            stats_verbosity: StatsVerbosity::Detailed,
            latch_grid: Arc::clone(&latch_grid),
        };
        let params = plan.session_params([9; 32], probes(false, false));
        assert_eq!(params.host, plan.host.addr);
        assert_eq!(params.port, plan.host.port);
        assert_eq!(params.launch, plan.launch);
        assert_eq!(params.clipboard, plan.clipboard);
        assert_eq!(
            params.connect_timeout,
            Duration::from_secs(plan.connect_timeout_secs.unwrap())
        );
        assert_eq!(params.bitrate_kbps, plan.settings.bitrate_kbps);
        assert_ne!(plan.settings.bitrate_kbps, Settings::default().bitrate_kbps);
        assert_eq!(params.preferred_codec, punktfunk_core::quic::CODEC_AV1);
        assert_ne!(plan.settings.codec, Settings::default().codec);
        assert_eq!(params.exclude_codecs, 0);
        assert_eq!(params.want_444, plan.settings.enable_444);
        assert!(params.vulkan.is_none());
        assert!(params.display_hdr.is_none());
        assert!(!params.phase_lock);
        assert_eq!(
            params.video_caps,
            punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE
        );
        assert_eq!(
            params.mode,
            Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 30,
            }
        );
        assert_eq!(params.pin, Some([9; 32]));
        assert_eq!(params.identity, ("client".into(), "desk".into()));
        assert_eq!(params.gamepad, GamepadPref::Xbox360);
        assert_eq!(params.stats_verbosity, StatsVerbosity::Detailed);
        assert!(Arc::ptr_eq(&params.force_software, &force_software));
        assert!(Arc::ptr_eq(&params.latch_grid, &latch_grid));

        let hdr = plan.session_params([9; 32], probes(true, false));
        assert_eq!(
            hdr.video_caps,
            punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE
                | punktfunk_core::quic::VIDEO_CAP_10BIT
                | punktfunk_core::quic::VIDEO_CAP_HDR
        );
        assert!(hdr.display_hdr.is_some());

        let chroma = plan.session_params([9; 32], probes(false, true));
        assert_eq!(
            chroma.video_caps,
            punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE | punktfunk_core::quic::VIDEO_CAP_444
        );
        assert!(chroma.want_444);
        assert!(chroma.display_hdr.is_none());

        let mut declined = plan.clone();
        declined.settings.hdr_enabled = false;
        let off = declined.session_params([9; 32], probes(true, false));
        assert_eq!(off.video_caps, punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE);
        assert!(
            off.display_hdr.is_none(),
            "the volume stays off with HDR off"
        );
    }
}
