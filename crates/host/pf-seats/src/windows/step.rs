//! One Windows seat's lifecycle as a pure step function.
//!
//! [`State`] is everything a seat's worker and its callers know. [`State::step`] takes one
//! [`Observation`] and the time, moves the state and answers the [`Action`]s the worker
//! performs. No I/O and no OS types, so the tests run on every target; `supervisor.rs`
//! probes what [`State::look`] names and performs.
//!
//! A cycle is one keeper and its session: sign in, pass the display gate, start the host,
//! watch both. The gate is owed once per seat start. A host that exits starts again in the
//! same session. A fault ends the cycle, and the next one signs in again unless the fault is
//! permanent ([`Code::permanent`]) or a stop was asked. The wait before it grows with each
//! cycle in a row: one that ended after its session was active counts apart from one that
//! never signed in.
//!
//! A [`BackendError`]'s code is a string on the wire. [`Fault::from`] is the one place it is
//! compared: the retry rule and the console's sentence match on [`Code`].

// Off Windows only the tests read this module.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::backend::BackendError;
use crate::backoff::{Backoff, Curve, Reset};
use crate::model::{RuntimeState, RuntimeStatus};
use std::time::{Duration, Instant};

const SESSION_TIMEOUT: Duration = Duration::from_secs(45);
const QUALITY_TIMEOUT: Duration = Duration::from_secs(60);
/// Fresh sessions a failing gate gets. A session whose display misses its path never
/// gets one; a fresh logon passes about 40% of the time on 9300.
const QUALITY_SESSIONS: u32 = 5;
const HOST_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// Room for a slow first logon plus two gate sessions, the usual worst healthy start.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(180);
const FIRST_RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(30);
/// The wait before a seat's next cycle when no session was active, and before its host starts
/// again in one. A host that served a minute clears it, as the web console's run does.
const RESTART: Backoff = Backoff::new(
    Curve::Doubling {
        first: FIRST_RESTART_DELAY,
        cap: MAX_RESTART_DELAY,
    },
    Reset::AfterRun(Duration::from_secs(60)),
);
/// The longest wait after a session ended, and the session that clears the wait.
const MAX_SESSION_RESTART_DELAY: Duration = Duration::from_secs(120);
/// The wait before the next cycle once a session was active. A player ends their own session
/// at will, and each end and each sign-in disturbs the other seats' displays: the wait doubles
/// to two minutes, and only a session that lasted that long starts it over.
const SESSION_RESTART: Backoff = Backoff::new(
    Curve::Doubling {
        first: FIRST_RESTART_DELAY,
        cap: MAX_SESSION_RESTART_DELAY,
    },
    Reset::AfterRun(MAX_SESSION_RESTART_DELAY),
);

/// How long the worker sleeps between two looks of one kind.
const SESSION_POLL: Duration = Duration::from_millis(250);
const GATE_POLL: Duration = Duration::from_millis(100);
const READY_POLL: Duration = Duration::from_millis(200);
const WATCH_POLL: Duration = Duration::from_millis(250);
const DELAY_POLL: Duration = Duration::from_millis(100);

/// A cycle error's code, for every code the supervisor acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Code {
    WtsTimeout,
    KeeperExited,
    QualityFailed,
    /// Any other `quality_` code: the gate reached no verdict.
    Quality,
    HostReadyTimeout,
    HostExited,
    AccountNotOwned,
    AccountIsAdministrator,
    AccountPolicy,
    CredentialMissing,
    CredentialInvalid,
    CredentialVerify,
    /// An adopted account's stored password no longer signs in.
    CredentialChanged,
    CredentialWrong,
    CredentialMustChange,
    AccountLocked,
    AccountSignedIn,
    HostMissing,
    RdpPinMissing,
    RdpPinInvalid,
    RdpPinChanged,
    LocalSystemRequired,
    Other,
}

impl Code {
    fn of(code: &str) -> Self {
        match code {
            "wts_timeout" => Self::WtsTimeout,
            "rdp_keeper_exited" => Self::KeeperExited,
            "quality_failed" => Self::QualityFailed,
            "host_ready_timeout" => Self::HostReadyTimeout,
            "host_exited" => Self::HostExited,
            "account_not_owned" => Self::AccountNotOwned,
            "account_is_administrator" => Self::AccountIsAdministrator,
            "account_policy" => Self::AccountPolicy,
            "credential_missing" => Self::CredentialMissing,
            "credential_invalid" => Self::CredentialInvalid,
            "credential_verify" => Self::CredentialVerify,
            "credential_changed" => Self::CredentialChanged,
            "credential_wrong" => Self::CredentialWrong,
            "credential_must_change" => Self::CredentialMustChange,
            "account_locked" => Self::AccountLocked,
            "account_signed_in" => Self::AccountSignedIn,
            "host_missing" => Self::HostMissing,
            "rdp_pin_missing" => Self::RdpPinMissing,
            "rdp_pin_invalid" => Self::RdpPinInvalid,
            "rdp_pin_changed" => Self::RdpPinChanged,
            "local_system_required" => Self::LocalSystemRequired,
            _ if code.starts_with("quality_") => Self::Quality,
            _ => Self::Other,
        }
    }

    /// Whether a new cycle can't help: the gate gave its answer, or the operator has to fix
    /// the account, its credential, the host binary or the pin first. Another sign-in with a
    /// refused password only counts toward the account's lockout.
    pub(crate) fn permanent(self) -> bool {
        match self {
            Self::QualityFailed
            | Self::Quality
            | Self::AccountNotOwned
            | Self::AccountIsAdministrator
            | Self::AccountPolicy
            | Self::CredentialMissing
            | Self::CredentialInvalid
            | Self::CredentialVerify
            | Self::CredentialChanged
            | Self::CredentialWrong
            | Self::CredentialMustChange
            | Self::AccountLocked
            | Self::AccountSignedIn
            | Self::HostMissing
            | Self::RdpPinMissing
            | Self::RdpPinInvalid
            | Self::RdpPinChanged
            | Self::LocalSystemRequired => true,
            Self::WtsTimeout
            | Self::KeeperExited
            | Self::HostReadyTimeout
            | Self::HostExited
            | Self::Other => false,
        }
    }
}

/// A cycle error with its code read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    pub(crate) code: Code,
    pub(crate) error: BackendError,
}

impl From<BackendError> for Fault {
    fn from(error: BackendError) -> Self {
        Self {
            code: Code::of(&error.code),
            error,
        }
    }
}

impl Fault {
    /// The sentence the console shows for a seat that didn't start, on its card and in
    /// Doctor. The log keeps the code and the operator's message.
    pub(crate) fn plain(&self) -> String {
        let text = match self.code {
            Code::WtsTimeout | Code::KeeperExited => {
                "Windows didn't sign in to the seat's account. Remote Desktop may be off or out of licenses."
            }
            Code::QualityFailed | Code::Quality => {
                "The seat's screen didn't start. The graphics card or the seat display driver failed its check."
            }
            Code::HostReadyTimeout | Code::HostExited => {
                "Punktfunk didn't start on the seat. The seat's log has the cause."
            }
            Code::AccountNotOwned
            | Code::AccountIsAdministrator
            | Code::AccountPolicy
            | Code::CredentialMissing
            | Code::CredentialInvalid
            | Code::CredentialVerify
            | Code::CredentialChanged
            | Code::CredentialWrong
            | Code::CredentialMustChange
            | Code::AccountLocked
            | Code::AccountSignedIn
            | Code::HostMissing
            | Code::RdpPinMissing
            | Code::RdpPinInvalid
            | Code::RdpPinChanged
            | Code::LocalSystemRequired
            | Code::Other => return self.error.message.clone(),
        };
        text.into()
    }

    /// [`Self::plain`] as a failed seat's status, with its message.
    pub(crate) fn status(&self) -> RuntimeStatus {
        let failed = RuntimeStatus::failed(self.plain());
        match self.code {
            Code::WtsTimeout | Code::KeeperExited => failed.worded("seatline_signin_failed"),
            Code::QualityFailed | Code::Quality => failed.worded("seatline_screen_failed"),
            Code::HostReadyTimeout | Code::HostExited => failed.worded("seatline_host_failed"),
            _ => match &self.error.said {
                Some(code) => RuntimeStatus {
                    detail_params: self.error.params.clone(),
                    ..failed.worded(&format!("apierr_{code}"))
                },
                None => failed,
            },
        }
    }
}

/// The stages of a seat start, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    SignIn,
    Display,
    Host,
}

impl Stage {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::SignIn => "sign_in",
            Self::Display => "display_check",
            Self::Host => "host_ready",
        }
    }

    fn phrase(self) -> &'static str {
        match self {
            Self::SignIn => "signing in to the seat's Windows account",
            Self::Display => "checking the seat's screen and graphics card",
            Self::Host => "starting Punktfunk on the seat",
        }
    }

    /// What the console shows while the seat is on this stage. `attempt` counts gate sessions.
    fn detail(self, attempt: u32) -> String {
        let phrase = self.phrase();
        let mut text = phrase[..1].to_uppercase() + &phrase[1..];
        if attempt > 1 {
            text += &format!(" (try {attempt} of {QUALITY_SESSIONS})");
        }
        text + "…"
    }

    /// [`Self::detail`] as a starting seat's status, with its message.
    fn status(self, attempt: u32) -> RuntimeStatus {
        let id = match (self, attempt > 1) {
            (Self::SignIn, false) => "seatline_stage_sign_in",
            (Self::SignIn, true) => "seatline_stage_sign_in_try",
            (Self::Display, false) => "seatline_stage_display",
            (Self::Display, true) => "seatline_stage_display_try",
            (Self::Host, false) => "seatline_stage_host",
            (Self::Host, true) => "seatline_stage_host_try",
        };
        let status = starting(self.detail(attempt)).worded(id);
        if attempt > 1 {
            status
                .param("attempt", attempt)
                .param("total", QUALITY_SESSIONS)
        } else {
            status
        }
    }

    /// The console's line for a start that ran out of time on this stage.
    fn timed_out(self) -> RuntimeStatus {
        let id = match self {
            Self::SignIn => "seatline_timeout_sign_in",
            Self::Display => "seatline_timeout_display",
            Self::Host => "seatline_timeout_host",
        };
        RuntimeStatus::failed(format!(
            "The seat didn't start in time. It stopped while {}.",
            self.phrase()
        ))
        .worded(id)
    }
}

/// What the worker probes for the next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Look {
    /// Nothing: only the time has moved.
    Clock,
    /// The keeper, and the account's session while the keeper lives.
    Session,
    /// The gate and the keeper.
    Gate,
    /// The keeper, the host, and whether the host holds its two ports.
    Ports,
    /// The keeper and the host.
    Exits,
    /// The keeper alone.
    Keeper,
}

/// What the worker saw. An exit code is `None` while its process runs or was not looked at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Probe {
    pub(crate) keeper: Option<u32>,
    pub(crate) gate: Option<u32>,
    pub(crate) host: Option<u32>,
    /// The id of the account's active session.
    pub(crate) session: Option<u32>,
    /// The host holds its native port and answers on its management port.
    pub(crate) ready: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Observation {
    /// What the worker saw of what [`State::look`] named.
    Probe(Probe),
    /// A probe or an action failed.
    Failed(Fault),
    /// A caller asked the seat to stop.
    Stop,
    /// `start` waited [`START_TIMEOUT`] without an answer.
    StartTimedOut,
}

/// What the worker does next, in order. An action that fails comes back as
/// [`Observation::Failed`] and the actions after it are dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// Log that the start entered this stage, on this gate session.
    Announce(Stage, u32),
    /// Start a keeper in a job of its own.
    StartKeeper,
    /// Record this session and build what its children start with.
    EnterSession(u32),
    StartGate,
    /// Read the gate's output and remove it: empty output fails the gate.
    CollectGate,
    /// Start the host, in place of one that exited.
    StartHost,
    /// Log that the host exited with this code and starts again.
    LogHostExit(u32),
    /// End the host and the gate, then the keeper and its job, then log the session off.
    EndCycle,
    /// Log a cycle that failed, and whether another follows.
    Report {
        error: BackendError,
        retry: bool,
    },
    /// Sleep, then look again.
    Wait(Duration),
    /// Leave the worker loop.
    Finish,
}

/// Why the seat is stopping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Asked,
    /// The start ran out of time on this stage: the seat then reads failed, not stopped.
    TimedOut(Stage),
}

/// The display gate, owed once per seat start. `failures` counts the sessions it failed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    Owed { failures: u32 },
    Passed,
}

/// When a wait began: at its first look, which follows the action that started the child.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Since(Option<Instant>);

impl Since {
    fn over(&mut self, now: Instant, limit: Duration) -> bool {
        now >= *self.0.get_or_insert(now) + limit
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    /// No keeper. The next cycle starts once `until` passes; `None` starts it at once.
    Resting { until: Option<Instant> },
    /// The keeper is up and the account's session is awaited.
    SigningIn { since: Since },
    /// The session is active and its children's roots and environment are being built.
    Entering,
    /// The gate runs in the session.
    Gating { since: Since },
    /// The gate exited clean and its output is being read.
    Collecting,
    /// The host runs and its ports are awaited.
    Readying { since: Since },
    /// The host serves. The keeper and the host are watched.
    Running,
    /// The host exited and starts again at `until`.
    HostDown { until: Instant },
    /// The cycle is being torn down, for this fault or for a stop.
    Ending { fault: Option<Fault> },
    /// The worker has left.
    Finished,
}

/// Everything known about one seat's runtime. Only [`State::step`] changes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct State {
    account: String,
    phase: Phase,
    /// The start stage last entered, where a start that runs out of time stood.
    stage: Stage,
    status: RuntimeStatus,
    session: Option<u32>,
    /// The latest outcome: the first start waits for one, a repeated start answers it.
    answer: Option<Result<(), BackendError>>,
    stop: Option<Stop>,
    gate: Gate,
    /// Cycles that failed before a session was active, for the wait before the next one.
    restart: Backoff,
    /// Cycles that ended after a session was active.
    session_restart: Backoff,
    /// Host exits in this cycle, for the wait before the host starts again.
    host_restart: Backoff,
}

impl State {
    pub(crate) fn new(account: String) -> Self {
        Self {
            account,
            phase: Phase::Resting { until: None },
            stage: Stage::SignIn,
            status: Stage::SignIn.status(0),
            session: None,
            answer: None,
            stop: None,
            gate: Gate::Owed { failures: 0 },
            restart: RESTART,
            session_restart: SESSION_RESTART,
            host_restart: RESTART,
        }
    }

    pub(crate) fn status(&self) -> &RuntimeStatus {
        &self.status
    }

    /// The seat's session, from the moment it is active until its cycle has ended.
    pub(crate) fn session(&self) -> Option<u32> {
        self.session
    }

    /// The latest outcome: `Ok` once the host serves, the fault of a cycle that failed, or
    /// the start's timeout. A failed gate that gets another session leaves it as it was.
    pub(crate) fn answer(&self) -> Option<&Result<(), BackendError>> {
        self.answer.as_ref()
    }

    pub(crate) fn finished(&self) -> bool {
        self.phase == Phase::Finished
    }

    /// What `start` answers when it has waited [`START_TIMEOUT`].
    pub(crate) fn start_timeout(&self) -> BackendError {
        BackendError::new(
            "start_timeout",
            format!(
                "seat did not become ready within {} seconds while {}",
                START_TIMEOUT.as_secs(),
                self.stage.phrase()
            ),
        )
    }

    pub(crate) fn look(&self) -> Look {
        match self.phase {
            Phase::SigningIn { .. } => Look::Session,
            Phase::Gating { .. } => Look::Gate,
            Phase::Readying { .. } => Look::Ports,
            Phase::Running => Look::Exits,
            Phase::HostDown { .. } => Look::Keeper,
            Phase::Resting { .. }
            | Phase::Entering
            | Phase::Collecting
            | Phase::Ending { .. }
            | Phase::Finished => Look::Clock,
        }
    }

    /// Take one observation at `now`. A caller's request only marks the state: the worker
    /// acts on it at its next look, so those two answer no actions.
    pub(crate) fn step(&mut self, observation: Observation, now: Instant) -> Vec<Action> {
        match observation {
            Observation::Stop => {
                self.stop.get_or_insert(Stop::Asked);
                Vec::new()
            }
            Observation::StartTimedOut => {
                self.stop = Some(Stop::TimedOut(self.stage));
                self.status = self.stage.timed_out();
                self.answer = Some(Err(self.start_timeout()));
                Vec::new()
            }
            Observation::Failed(_) if self.finished() => vec![Action::Finish],
            Observation::Failed(fault) => self.end(Some(fault)),
            Observation::Probe(probe) => self.probed(probe, now),
        }
    }

    /// One look, in the phase's own order: which of a stop, an exit and a limit wins there
    /// decides the code the caller reads.
    fn probed(&mut self, probe: Probe, now: Instant) -> Vec<Action> {
        match self.phase {
            Phase::Resting { until } => self.resting(until, now),
            Phase::SigningIn { ref mut since } => {
                let late = since.over(now, SESSION_TIMEOUT);
                self.signing_in(probe, late)
            }
            Phase::Entering => self.entered(),
            Phase::Gating { ref mut since } => {
                let late = since.over(now, QUALITY_TIMEOUT);
                self.gating(probe, late)
            }
            Phase::Collecting => {
                self.gate = Gate::Passed;
                self.start_host()
            }
            Phase::Readying { ref mut since } => {
                let late = since.over(now, HOST_READY_TIMEOUT);
                self.readying(probe, late, now)
            }
            Phase::Running => self.running(probe, now),
            Phase::HostDown { until } => self.host_down(probe, until, now),
            Phase::Ending { ref mut fault } => {
                let fault = fault.take();
                self.ended(fault, now)
            }
            Phase::Finished => vec![Action::Finish],
        }
    }

    fn stopping(&self) -> bool {
        self.stop.is_some()
    }

    /// A stop, then the rest. A cycle starts once the rest is over.
    fn resting(&mut self, until: Option<Instant>, now: Instant) -> Vec<Action> {
        if self.stopping() {
            self.finish()
        } else if until.is_some_and(|until| now < until) {
            vec![Action::Wait(DELAY_POLL)]
        } else {
            self.host_restart.reset();
            self.phase = Phase::SigningIn {
                since: Since::default(),
            };
            self.enter(Stage::SignIn, 0, Action::StartKeeper)
        }
    }

    /// A stop, a lost keeper, then the session, then the limit.
    fn signing_in(&mut self, probe: Probe, late: bool) -> Vec<Action> {
        if self.stopping() {
            self.end(None)
        } else if probe.keeper.is_some() {
            self.fail(
                "rdp_keeper_exited",
                "RDP keeper exited before its WTS session became active",
            )
        } else if let Some(id) = probe.session {
            self.restart.reset();
            self.session = Some(id);
            self.phase = Phase::Entering;
            vec![Action::EnterSession(id)]
        } else if late {
            let account = &self.account;
            let message = format!("no active non-console WTS session appeared for '{account}'");
            self.fail("wts_timeout", message)
        } else {
            vec![Action::Wait(SESSION_POLL)]
        }
    }

    /// The session is prepared: the gate while it is owed, else the host. No stop is read.
    fn entered(&mut self) -> Vec<Action> {
        match self.gate {
            Gate::Owed { failures } => {
                self.phase = Phase::Gating {
                    since: Since::default(),
                };
                self.enter(Stage::Display, failures + 1, Action::StartGate)
            }
            Gate::Passed => self.start_host(),
        }
    }

    /// The gate's exit first, then a stop, a lost keeper, then the limit. Only the gate's
    /// exit or its limit is the gate's verdict.
    fn gating(&mut self, probe: Probe, late: bool) -> Vec<Action> {
        match (probe.gate, probe.keeper) {
            (Some(0), _) => {
                self.phase = Phase::Collecting;
                vec![Action::CollectGate]
            }
            (Some(code), _) => self.fail(
                "quality_failed",
                format!("same-session virtual-display quality gate exited with code {code}"),
            ),
            (None, _) if self.stopping() => self.end(None),
            (None, Some(code)) => self.fail(
                "rdp_keeper_exited",
                format!("RDP keeper exited with code {code} during the display check"),
            ),
            (None, None) if late => self.fail(
                "quality_timeout",
                format!(
                    "same-session virtual-display quality gate did not finish within {} seconds",
                    QUALITY_TIMEOUT.as_secs()
                ),
            ),
            (None, None) => vec![Action::Wait(GATE_POLL)],
        }
    }

    /// A stop, a lost keeper, a host that exited, the ports, then the limit.
    fn readying(&mut self, probe: Probe, late: bool, now: Instant) -> Vec<Action> {
        if self.stopping() {
            self.fail(
                "seat_stopping",
                "seat stopped while waiting for host readiness",
            )
        } else if let Some(code) = probe.keeper {
            self.fail(
                "rdp_keeper_exited",
                format!("RDP keeper exited with code {code} during host startup"),
            )
        } else if let Some(code) = probe.host {
            self.fail(
                "host_exited",
                format!("punktfunk-host exited with code {code} before readiness"),
            )
        } else if probe.ready {
            self.status = RuntimeStatus::running();
            self.answer = Some(Ok(()));
            self.session_restart.started(now);
            self.host_restart.started(now);
            self.phase = Phase::Running;
            Vec::new()
        } else if late {
            self.fail(
                "host_ready_timeout",
                format!(
                    "punktfunk-host did not open both assigned ports within {} seconds",
                    HOST_READY_TIMEOUT.as_secs()
                ),
            )
        } else {
            vec![Action::Wait(READY_POLL)]
        }
    }

    /// A stop, a lost keeper, then a host that exited, which starts again after a wait.
    fn running(&mut self, probe: Probe, now: Instant) -> Vec<Action> {
        if self.stopping() {
            self.status = RuntimeStatus {
                state: RuntimeState::Stopping,
                detail: None,
                ..RuntimeStatus::default()
            };
            self.end(None)
        } else if let Some(code) = probe.keeper {
            self.fail(
                "rdp_keeper_exited",
                format!("RDP keeper process exited with code {code}"),
            )
        } else if let Some(code) = probe.host {
            let delay = self.host_restart.failed(now);
            self.status = starting(format!(
                "punktfunk stopped on the seat. Restarting it in {} s…",
                delay.as_secs()
            ));
            self.phase = Phase::HostDown { until: now + delay };
            vec![Action::LogHostExit(code)]
        } else {
            vec![Action::Wait(WATCH_POLL)]
        }
    }

    /// The wait's end first: a host that is due starts even with a stop asked, and its ready
    /// wait reads the stop. Then a stop, then a lost keeper.
    fn host_down(&mut self, probe: Probe, until: Instant, now: Instant) -> Vec<Action> {
        if now >= until {
            self.phase = Phase::Readying {
                since: Since::default(),
            };
            vec![Action::StartHost]
        } else if self.stopping() {
            self.end(None)
        } else if probe.keeper.is_some() {
            self.fail(
                "rdp_keeper_exited",
                "RDP keeper exited during host restart backoff",
            )
        } else {
            vec![Action::Wait(DELAY_POLL)]
        }
    }

    /// Enter `stage` of a start: the console shows its detail and the log names it.
    fn enter(&mut self, stage: Stage, attempt: u32, action: Action) -> Vec<Action> {
        self.stage = stage;
        self.status = stage.status(attempt);
        vec![Action::Announce(stage, attempt), action]
    }

    fn start_host(&mut self) -> Vec<Action> {
        self.phase = Phase::Readying {
            since: Since::default(),
        };
        self.enter(Stage::Host, 0, Action::StartHost)
    }

    fn fail(&mut self, code: &str, message: impl Into<String>) -> Vec<Action> {
        self.end(Some(BackendError::new(code, message).into()))
    }

    /// Tear the cycle down. What the fault means is decided once that is done, so the
    /// console reads the old status until the session is logged off.
    fn end(&mut self, fault: Option<Fault>) -> Vec<Action> {
        self.phase = Phase::Ending { fault };
        vec![Action::EndCycle]
    }

    /// The cycle is torn down: rest before another one, or finish.
    fn ended(&mut self, fault: Option<Fault>, now: Instant) -> Vec<Action> {
        let entered = self.session.take().is_some();
        let Some(fault) = fault else {
            return self.finish();
        };
        let stopping = self.stopping();
        // A failed gate gets a fresh session, and the start waits for those.
        let mut gate_retry = false;
        if fault.code == Code::QualityFailed
            && let Gate::Owed { failures } = &mut self.gate
        {
            *failures += 1;
            gate_retry = *failures < QUALITY_SESSIONS;
        }
        let retry = !stopping && (gate_retry || !fault.code.permanent());
        let mut actions = Vec::new();
        // The first start's caller logs the first outcome; the log has every later one.
        if !stopping && (self.answer.is_some() || gate_retry) {
            actions.push(Action::Report {
                error: fault.error.clone(),
                retry,
            });
        }
        if !gate_retry {
            self.status = fault.status();
            self.answer = Some(Err(fault.error));
        }
        if retry {
            let wait = if entered {
                self.session_restart.failed(now)
            } else {
                self.restart.failed(now)
            };
            self.phase = Phase::Resting {
                until: Some(now + wait),
            };
        } else {
            actions.extend(self.finish());
        }
        actions
    }

    /// The worker leaves. A stopped seat reads stopped, or failed when its start ran out of
    /// time; a seat that failed for good keeps the sentence of its fault.
    fn finish(&mut self) -> Vec<Action> {
        if let Some(stop) = self.stop {
            self.status = match stop {
                Stop::Asked => RuntimeStatus::stopped(),
                Stop::TimedOut(stage) => stage.timed_out(),
            };
            self.answer.get_or_insert_with(|| {
                Err(BackendError::new(
                    "seat_stopping",
                    "seat stopped before startup completed",
                ))
            });
        }
        self.session = None;
        self.phase = Phase::Finished;
        vec![Action::Finish]
    }
}

fn starting(detail: String) -> RuntimeStatus {
    RuntimeStatus {
        state: RuntimeState::Starting,
        detail: Some(detail),
        ..RuntimeStatus::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGN_IN: &str = "Windows didn't sign in to the seat's account.";
    const SCREEN: &str = "The seat's screen didn't start.";
    const HOST: &str = "Punktfunk didn't start on the seat.";
    const MESSAGE: &str = "the operator's message";

    /// Code, whether a retry is pointless, and how the console's sentence starts.
    const CODES: [(&str, bool, &str); 33] = [
        ("wts_timeout", false, SIGN_IN),
        ("rdp_keeper_exited", false, SIGN_IN),
        ("quality_failed", true, SCREEN),
        ("quality_timeout", true, SCREEN),
        ("quality_empty", true, SCREEN),
        ("quality_output", true, SCREEN),
        ("quality_anything_else", true, SCREEN),
        ("host_ready_timeout", false, HOST),
        ("host_exited", false, HOST),
        ("port_in_use", false, MESSAGE),
        ("seats_off", false, MESSAGE),
        ("termservice_stopped", false, MESSAGE),
        ("seat_display_driver", false, MESSAGE),
        ("account_not_owned", true, MESSAGE),
        ("account_is_administrator", true, MESSAGE),
        ("account_policy", true, MESSAGE),
        ("credential_missing", true, MESSAGE),
        ("credential_invalid", true, MESSAGE),
        ("credential_verify", true, MESSAGE),
        ("credential_changed", true, MESSAGE),
        ("credential_wrong", true, MESSAGE),
        ("credential_must_change", true, MESSAGE),
        ("account_locked", true, MESSAGE),
        ("account_signed_in", true, MESSAGE),
        ("host_missing", true, MESSAGE),
        ("rdp_pin_missing", true, MESSAGE),
        ("rdp_pin_invalid", true, MESSAGE),
        ("rdp_pin_changed", true, MESSAGE),
        ("local_system_required", true, MESSAGE),
        ("seat_stopping", false, MESSAGE),
        ("process_wait", false, MESSAGE),
        ("wts_logoff", false, MESSAGE),
        ("", false, MESSAGE),
    ];

    #[test]
    fn a_code_keeps_its_retry_rule_and_its_sentence() {
        for (code, permanent, sentence) in CODES {
            let fault = Fault::from(BackendError::new(code, MESSAGE));
            assert_eq!(fault.code.permanent(), permanent, "{code}");
            assert!(fault.plain().starts_with(sentence), "{code}");
            assert_eq!(fault.error.code, code);
        }
    }

    #[test]
    fn only_a_failed_gate_counts_toward_the_gate_sessions() {
        for (code, ..) in CODES {
            let counted = Fault::from(BackendError::new(code, MESSAGE)).code == Code::QualityFailed;
            assert_eq!(counted, code == "quality_failed", "{code}");
        }
    }

    const SEAT: &str = "pf-seat-1";
    const MS: Duration = Duration::from_millis(1);
    const HOUR: Duration = Duration::from_secs(3600);
    const NOTHING: Probe = Probe {
        keeper: None,
        gate: None,
        host: None,
        session: None,
        ready: false,
    };
    const SESSION: Probe = Probe {
        session: Some(7),
        ..NOTHING
    };
    const READY: Probe = Probe {
        ready: true,
        ..NOTHING
    };
    const SIGNING_IN: &str = "Signing in to the seat's Windows account…";
    const CHECKING: &str = "Checking the seat's screen and graphics card…";
    const STARTING_HOST: &str = "Starting Punktfunk on the seat…";
    const NO_SCREEN: &str = "The seat's screen didn't start. The graphics card or the seat display driver failed its check.";
    const NO_HOST: &str = "Punktfunk didn't start on the seat. The seat's log has the cause.";
    const GATE_OUT_OF_TIME: &str =
        "same-session virtual-display quality gate did not finish within 60 seconds";
    const STOPPED_EARLY: (&str, &str) = ("seat_stopping", "seat stopped before startup completed");
    const STOPPED_READYING: (&str, &str) = (
        "seat_stopping",
        "seat stopped while waiting for host readiness",
    );

    fn keeper(code: u32) -> Probe {
        Probe {
            keeper: Some(code),
            ..NOTHING
        }
    }

    fn gate(code: u32) -> Probe {
        Probe {
            gate: Some(code),
            ..NOTHING
        }
    }

    fn host(code: u32) -> Probe {
        Probe {
            host: Some(code),
            ..NOTHING
        }
    }

    fn error(code: &str, message: &str) -> BackendError {
        BackendError::new(code, message)
    }

    fn shows(state: RuntimeState, detail: &str) -> RuntimeStatus {
        RuntimeStatus {
            state,
            detail: Some(detail.into()),
            ..RuntimeStatus::default()
        }
    }

    /// `status` without its message id, as [`shows`] builds one.
    fn bare(status: &RuntimeStatus) -> RuntimeStatus {
        RuntimeStatus {
            state: status.state.clone(),
            detail: status.detail.clone(),
            ..RuntimeStatus::default()
        }
    }

    #[test]
    fn a_seat_line_names_its_message() {
        let first = Stage::Display.status(1);
        assert_eq!(first.detail_id.as_deref(), Some("seatline_stage_display"));
        assert!(first.detail_params.is_empty());
        let again = Stage::Display.status(2);
        assert_eq!(
            again.detail_id.as_deref(),
            Some("seatline_stage_display_try")
        );
        assert_eq!(again.detail_params["attempt"], "2");
        let late = Stage::Host.timed_out();
        assert_eq!(late.detail_id.as_deref(), Some("seatline_timeout_host"));
        let refused =
            Fault::from(BackendError::new("credential_wrong", MESSAGE).said("credential_wrong"));
        assert_eq!(
            refused.status().detail_id.as_deref(),
            Some("apierr_credential_wrong")
        );
        assert_eq!(
            Fault::from(error("other", MESSAGE)).status().detail_id,
            None
        );
    }

    fn start_cycle() -> Vec<Action> {
        vec![Action::Announce(Stage::SignIn, 0), Action::StartKeeper]
    }

    fn start_host() -> Vec<Action> {
        vec![Action::Announce(Stage::Host, 0), Action::StartHost]
    }

    fn report(code: &str, message: &str, retry: bool) -> Vec<Action> {
        vec![Action::Report {
            error: error(code, message),
            retry,
        }]
    }

    /// A seat's state and a clock that only the test moves.
    struct Bench {
        state: State,
        now: Instant,
    }

    impl Bench {
        fn see(&mut self, probe: Probe) -> Vec<Action> {
            self.state.step(Observation::Probe(probe), self.now)
        }

        fn later(&mut self, by: Duration) -> &mut Self {
            self.now += by;
            self
        }

        fn fails(&mut self, code: &str) -> Vec<Action> {
            let fault = error(code, MESSAGE).into();
            self.state.step(Observation::Failed(fault), self.now)
        }

        /// A caller's request only marks the state.
        fn asks(&mut self, request: Observation) {
            assert_eq!(self.state.step(request, self.now), []);
        }

        /// The start's answer as its code and message.
        fn answer(&self) -> Option<Result<(), (&str, &str)>> {
            self.state.answer().map(|answer| match answer {
                Ok(()) => Ok(()),
                Err(error) => Err((error.code.as_str(), error.message.as_str())),
            })
        }

        fn ending(&self) -> Option<(&str, &str)> {
            match &self.state.phase {
                Phase::Ending { fault: Some(fault) } => {
                    Some((fault.error.code.as_str(), fault.error.message.as_str()))
                }
                _ => None,
            }
        }

        /// The seat rests exactly `delay`, then starts its next cycle.
        fn rests(&mut self, delay: Duration) {
            self.later(delay - MS);
            assert_eq!(self.see(NOTHING), [Action::Wait(DELAY_POLL)]);
            assert_eq!(self.later(MS).see(NOTHING), start_cycle());
        }

        /// Look at nothing until the worker leaves: every action it was given on the way.
        fn run_out(&mut self) -> Vec<Action> {
            let mut all = Vec::new();
            for _ in 0..8 {
                all.extend(self.see(NOTHING));
                if all.last() == Some(&Action::Finish) {
                    assert!(self.state.finished());
                    return all;
                }
            }
            panic!("the worker did not leave: {all:?}");
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum At {
        Fresh,
        SigningIn,
        Entering,
        Gating,
        Collecting,
        Readying,
        Running,
        HostDown,
        Ending,
        Resting,
    }

    /// What a clean start sees and is told to do on its way into each phase.
    fn road() -> [(At, Probe, Vec<Action>); 6] {
        let check = vec![Action::Announce(Stage::Display, 1), Action::StartGate];
        [
            (At::SigningIn, NOTHING, start_cycle()),
            (At::Entering, SESSION, vec![Action::EnterSession(7)]),
            (At::Gating, NOTHING, check),
            (At::Collecting, gate(0), vec![Action::CollectGate]),
            (At::Readying, NOTHING, start_host()),
            (At::Running, READY, vec![]),
        ]
    }

    /// A seat driven to `at` by a clean start. `HostDown` follows a host that exited while it
    /// served; `Ending` and `Resting` follow a keeper lost then.
    fn bench(at: At) -> Bench {
        let mut bench = Bench {
            state: State::new(SEAT.into()),
            now: Instant::now(),
        };
        if at == At::Fresh {
            return bench;
        }
        for (reached, probe, actions) in road() {
            assert_eq!(bench.see(probe), actions, "into {reached:?}");
            if reached == at {
                return bench;
            }
        }
        if at == At::HostDown {
            assert_eq!(bench.see(host(3)), [Action::LogHostExit(3)]);
            return bench;
        }
        assert_eq!(bench.see(keeper(5)), [Action::EndCycle]);
        if at == At::Resting {
            let lost = report(
                "rdp_keeper_exited",
                "RDP keeper process exited with code 5",
                true,
            );
            assert_eq!(bench.see(NOTHING), lost);
        }
        bench
    }

    #[test]
    fn the_limits_keep_their_values() {
        assert_eq!(SESSION_TIMEOUT, Duration::from_secs(45));
        assert_eq!(QUALITY_TIMEOUT, Duration::from_secs(60));
        assert_eq!(QUALITY_SESSIONS, 5);
        assert_eq!(HOST_READY_TIMEOUT, Duration::from_secs(30));
        assert_eq!(START_TIMEOUT, Duration::from_secs(180));
        assert_eq!(FIRST_RESTART_DELAY, Duration::from_secs(1));
        assert_eq!(MAX_RESTART_DELAY, Duration::from_secs(30));
        assert_eq!(MAX_SESSION_RESTART_DELAY, Duration::from_secs(120));
        let polls = [SESSION_POLL, GATE_POLL, READY_POLL, WATCH_POLL, DELAY_POLL];
        assert_eq!(
            polls.map(|poll| poll.as_millis()),
            [250, 100, 200, 250, 100]
        );
    }

    #[test]
    fn a_clean_start_runs_each_stage_once() {
        let mut bench = bench(At::Fresh);
        assert_eq!(
            bare(bench.state.status()),
            shows(RuntimeState::Starting, SIGNING_IN)
        );
        let after = [
            shows(RuntimeState::Starting, SIGNING_IN),
            shows(RuntimeState::Starting, SIGNING_IN),
            shows(RuntimeState::Starting, CHECKING),
            shows(RuntimeState::Starting, CHECKING),
            shows(RuntimeState::Starting, STARTING_HOST),
            RuntimeStatus::running(),
        ];
        for ((reached, probe, actions), status) in road().into_iter().zip(after) {
            assert_eq!(bench.answer(), None, "before {reached:?}");
            assert_eq!(bench.see(probe), actions, "into {reached:?}");
            assert_eq!(bare(bench.state.status()), status, "in {reached:?}");
        }
        assert_eq!(bench.answer(), Some(Ok(())));
        assert_eq!(bench.state.session(), Some(7));
        // The host serves: the worker only watches.
        assert_eq!(bench.later(HOUR).see(NOTHING), [Action::Wait(WATCH_POLL)]);
        assert!(!bench.state.finished());
    }

    #[test]
    fn the_worker_looks_at_what_the_phase_waits_for() {
        let looks = [
            (At::Fresh, Look::Clock),
            (At::SigningIn, Look::Session),
            (At::Entering, Look::Clock),
            (At::Gating, Look::Gate),
            (At::Collecting, Look::Clock),
            (At::Readying, Look::Ports),
            (At::Running, Look::Exits),
            (At::HostDown, Look::Keeper),
            (At::Ending, Look::Clock),
            (At::Resting, Look::Clock),
        ];
        for (at, look) in looks {
            assert_eq!(bench(at).state.look(), look, "{at:?}");
        }
    }

    #[test]
    fn a_wait_runs_out_at_its_limit_counted_from_its_first_look() {
        let rows = [
            (
                At::SigningIn,
                45,
                SESSION_POLL,
                "wts_timeout",
                "no active non-console WTS session appeared for 'pf-seat-1'",
            ),
            (
                At::Gating,
                60,
                GATE_POLL,
                "quality_timeout",
                GATE_OUT_OF_TIME,
            ),
            (
                At::Readying,
                30,
                READY_POLL,
                "host_ready_timeout",
                "punktfunk-host did not open both assigned ports within 30 seconds",
            ),
        ];
        for (at, limit, poll, code, message) in rows {
            let mut bench = bench(at);
            // The action that started the child took a while; the wait starts after it.
            bench.later(Duration::from_secs(20));
            assert_eq!(bench.see(NOTHING), [Action::Wait(poll)], "{at:?}");
            bench.later(Duration::from_secs(limit) - MS);
            assert_eq!(bench.see(NOTHING), [Action::Wait(poll)], "{at:?}");
            assert_eq!(bench.later(MS).see(NOTHING), [Action::EndCycle], "{at:?}");
            assert_eq!(bench.ending(), Some((code, message)), "{at:?}");
        }
    }

    #[test]
    fn a_failed_gate_gets_five_sessions() {
        let failed = "same-session virtual-display quality gate exited with code 1";
        let mut bench = bench(At::Gating);
        for session in 1..QUALITY_SESSIONS {
            assert_eq!(bench.see(gate(1)), [Action::EndCycle]);
            // Another session follows: the start is not answered and the console still
            // reads the check.
            assert_eq!(bench.see(NOTHING), report("quality_failed", failed, true));
            assert_eq!(bench.answer(), None);
            let detail = match session {
                1 => CHECKING.to_owned(),
                n => format!("Checking the seat's screen and graphics card (try {n} of 5)…"),
            };
            assert_eq!(
                bare(bench.state.status()),
                shows(RuntimeState::Starting, &detail)
            );
            bench.rests(Duration::from_secs(1 << (session - 1)));
            assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
            let check = [
                Action::Announce(Stage::Display, session + 1),
                Action::StartGate,
            ];
            assert_eq!(bench.see(NOTHING), check);
        }
        // The fifth failure is the gate's answer, and the start's.
        assert_eq!(bench.see(gate(1)), [Action::EndCycle]);
        assert_eq!(bench.see(NOTHING), [Action::Finish]);
        assert_eq!(bench.answer(), Some(Err(("quality_failed", failed))));
        assert_eq!(
            bare(bench.state.status()),
            shows(RuntimeState::Failed, NO_SCREEN)
        );
        assert!(bench.state.finished());
    }

    #[test]
    fn a_gate_that_passed_is_not_run_again() {
        let mut bench = bench(At::Resting);
        bench.rests(Duration::from_secs(1));
        assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
        assert_eq!(bench.see(NOTHING), start_host());
    }

    #[test]
    fn a_host_that_exits_before_it_is_ready_ends_the_cycle() {
        let exited = "punktfunk-host exited with code 1 before readiness";
        let mut bench = bench(At::Readying);
        assert_eq!(bench.see(host(1)), [Action::EndCycle]);
        // The console reads the old status until the session is logged off.
        assert_eq!(
            bare(bench.state.status()),
            shows(RuntimeState::Starting, STARTING_HOST)
        );
        assert_eq!(bench.state.session(), Some(7));
        // The first failure is the start's answer, so its caller logs it.
        assert_eq!(bench.see(NOTHING), []);
        assert_eq!(bench.answer(), Some(Err(("host_exited", exited))));
        assert_eq!(
            bare(bench.state.status()),
            shows(RuntimeState::Failed, NO_HOST)
        );
        assert_eq!(bench.state.session(), None);
        bench.rests(Duration::from_secs(1));
        assert_eq!(
            bare(bench.state.status()),
            shows(RuntimeState::Starting, SIGNING_IN)
        );
        // The gate passed in the first cycle. A second failure has only the log.
        assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
        assert_eq!(bench.see(NOTHING), start_host());
        assert_eq!(bench.see(host(1)), [Action::EndCycle]);
        assert_eq!(bench.see(NOTHING), report("host_exited", exited, true));
        bench.rests(Duration::from_secs(2));
    }

    #[test]
    fn a_host_that_exits_while_it_serves_starts_again_in_the_session() {
        let mut bench = bench(At::Running);
        // A host that served less than a minute doesn't shorten the next wait; one that served
        // a minute starts the doubling over.
        for (served, secs) in [
            (59, 1),
            (59, 2),
            (59, 4),
            (59, 8),
            (59, 16),
            (59, 30),
            (59, 30),
            (60, 1),
        ] {
            let served = Duration::from_secs(served);
            assert_eq!(bench.later(served).see(host(3)), [Action::LogHostExit(3)]);
            let detail = format!("punktfunk stopped on the seat. Restarting it in {secs} s…");
            assert_eq!(
                bare(bench.state.status()),
                shows(RuntimeState::Starting, &detail)
            );
            bench.later(Duration::from_secs(secs) - MS);
            assert_eq!(bench.see(NOTHING), [Action::Wait(DELAY_POLL)]);
            // No stage is announced: the start was answered long ago.
            assert_eq!(bench.later(MS).see(NOTHING), [Action::StartHost]);
            assert_eq!(
                bare(bench.state.status()),
                shows(RuntimeState::Starting, &detail)
            );
            assert_eq!(bench.see(READY), []);
            assert_eq!(bare(bench.state.status()), RuntimeStatus::running());
        }
        assert_eq!(bench.answer(), Some(Ok(())));
        assert_eq!(bench.state.session(), Some(7));
    }

    #[test]
    fn the_session_and_host_delays_are_told_when_the_host_comes_up() {
        let mut bench = bench(At::Readying);
        let up = bench.later(Duration::from_secs(3)).now;
        assert_eq!(bench.see(READY), []);
        let mut session = SESSION_RESTART;
        session.started(up);
        let mut host = RESTART;
        host.started(up);
        assert_eq!(bench.state.session_restart, session);
        assert_eq!(bench.state.host_restart, host);
        assert_eq!(bench.state.restart, RESTART);
    }

    #[test]
    fn the_host_delay_starts_over_with_each_cycle() {
        let mut bench = bench(At::Running);
        for _ in 0..3 {
            assert_eq!(bench.see(host(3)), [Action::LogHostExit(3)]);
            bench.later(Duration::from_secs(30));
            assert_eq!(bench.see(NOTHING), [Action::StartHost]);
            assert_eq!(bench.see(READY), []);
        }
        assert_eq!(bench.see(keeper(5)), [Action::EndCycle]);
        bench.see(NOTHING);
        bench.rests(Duration::from_secs(1));
        assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
        assert_eq!(bench.see(NOTHING), start_host());
        assert_eq!(bench.see(READY), []);
        assert_eq!(bench.see(host(3)), [Action::LogHostExit(3)]);
        let detail = "punktfunk stopped on the seat. Restarting it in 1 s…";
        assert_eq!(
            bare(bench.state.status()),
            shows(RuntimeState::Starting, detail)
        );
    }

    #[test]
    fn a_lost_keeper_ends_the_cycle_from_every_phase() {
        const EARLY: &str = "RDP keeper exited before its WTS session became active";
        const STARTING: &str = "RDP keeper exited with code 5 during host startup";
        const SERVING: &str = "RDP keeper process exited with code 5";
        const DOWN: &str = "RDP keeper exited during host restart backoff";
        const CHECKING: &str = "RDP keeper exited with code 5 during the display check";
        // Phase, the fault, and what follows the teardown: another cycle, every time.
        let rows = [
            (At::SigningIn, "rdp_keeper_exited", EARLY, vec![]),
            (At::Gating, "rdp_keeper_exited", CHECKING, vec![]),
            (At::Readying, "rdp_keeper_exited", STARTING, vec![]),
            (
                At::Running,
                "rdp_keeper_exited",
                SERVING,
                report("rdp_keeper_exited", SERVING, true),
            ),
            (
                At::HostDown,
                "rdp_keeper_exited",
                DOWN,
                report("rdp_keeper_exited", DOWN, true),
            ),
        ];
        for (at, code, message, then) in rows {
            let mut bench = bench(at);
            assert_eq!(bench.see(keeper(5)), [Action::EndCycle], "{at:?}");
            assert_eq!(bench.ending(), Some((code, message)), "{at:?}");
            assert_eq!(bench.see(NOTHING), then, "{at:?}");
            assert!(!bench.state.finished(), "{at:?}");
        }
    }

    #[test]
    fn a_keeper_lost_under_the_gate_leaves_the_gate_owed() {
        let mut bench = bench(At::Gating);
        assert_eq!(bench.see(keeper(5)), [Action::EndCycle]);
        assert_eq!(bench.see(NOTHING), []);
        bench.rests(Duration::from_secs(1));
        assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
        let check = [Action::Announce(Stage::Display, 1), Action::StartGate];
        assert_eq!(bench.see(NOTHING), check);
    }

    #[test]
    fn a_failed_action_ends_the_cycle_and_its_code_decides_the_retry() {
        // Phase, the code an action or a probe failed with, whether a cycle follows, and
        // whether the start had been answered, which puts the failure in the log.
        let rows = [
            (At::SigningIn, "keeper_spawn", true, false),
            (At::SigningIn, "credential_missing", false, false),
            (At::SigningIn, "wts_enumerate", true, false),
            (At::Entering, "seat_root", true, false),
            (At::Gating, "process_spawn", true, false),
            (At::Collecting, "quality_empty", false, false),
            (At::Readying, "process_spawn", true, false),
            (At::Running, "process_wait", true, true),
            (At::HostDown, "process_spawn", true, true),
        ];
        for (at, code, retry, answered) in rows {
            let mut bench = bench(at);
            assert_eq!(bench.fails(code), [Action::EndCycle], "{at:?} {code}");
            let mut then = match answered {
                true => report(code, MESSAGE, retry),
                false => vec![],
            };
            if !retry {
                then.push(Action::Finish);
            }
            assert_eq!(bench.see(NOTHING), then, "{at:?} {code}");
            assert_eq!(bench.answer(), Some(Err((code, MESSAGE))), "{at:?} {code}");
            assert_eq!(bench.state.finished(), !retry, "{at:?} {code}");
        }
    }

    #[test]
    fn a_gate_whose_output_fails_stays_owed() {
        let mut bench = bench(At::Collecting);
        bench.fails("quality_output");
        assert_eq!(bench.state.gate, Gate::Owed { failures: 0 });
    }

    #[test]
    fn the_cycle_delay_doubles_to_its_cap_until_a_session_is_active() {
        let mut bench = bench(At::SigningIn);
        for secs in [1, 2, 4, 8, 16, 30, 30] {
            assert_eq!(bench.see(keeper(1)), [Action::EndCycle]);
            bench.see(NOTHING);
            bench.rests(Duration::from_secs(secs));
        }
        // A session that comes up starts the doubling over for the cycles that fail before one.
        assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
        let check = vec![Action::Announce(Stage::Display, 1), Action::StartGate];
        assert_eq!(bench.see(NOTHING), check);
        assert_eq!(bench.see(keeper(1)), [Action::EndCycle]);
        bench.see(NOTHING);
        bench.rests(Duration::from_secs(1));
        assert_eq!(bench.see(keeper(1)), [Action::EndCycle]);
        bench.see(NOTHING);
        bench.rests(Duration::from_secs(1));
    }

    /// A player can end their own session at will, so the seat waits longer after each one that
    /// ends young: the first restart is not slowed, a session of two minutes starts over.
    #[test]
    fn a_session_that_ends_young_makes_the_next_cycle_wait_longer() {
        let mut bench = bench(At::Running);
        // How long the session lasted, and the wait after it.
        let ends = [
            (59, 1),
            (59, 2),
            (59, 4),
            (59, 8),
            (59, 16),
            (59, 32),
            (59, 64),
            (59, 120),
            (59, 120),
            (120, 1),
        ];
        for (lasted, secs) in ends {
            bench.later(Duration::from_secs(lasted));
            assert_eq!(bench.see(keeper(1)), [Action::EndCycle], "{lasted} {secs}");
            bench.see(NOTHING);
            bench.rests(Duration::from_secs(secs));
            assert_eq!(bench.see(SESSION), [Action::EnterSession(7)]);
            assert_eq!(bench.see(NOTHING), start_host());
            assert_eq!(bench.see(READY), []);
        }
    }

    #[test]
    fn a_later_cycle_that_serves_replaces_the_answer() {
        let mut bench = bench(At::SigningIn);
        assert_eq!(bench.see(keeper(1)), [Action::EndCycle]);
        assert_eq!(bench.see(NOTHING), []);
        let first = bench
            .answer()
            .map(|answer| answer.map_err(|(code, _)| code));
        assert_eq!(first, Some(Err("rdp_keeper_exited")));
        bench.rests(Duration::from_secs(1));
        for (_, probe, actions) in road().into_iter().skip(1) {
            assert_eq!(bench.see(probe), actions);
        }
        assert_eq!(bare(bench.state.status()), RuntimeStatus::running());
        assert_eq!(bench.answer(), Some(Ok(())));
    }

    #[test]
    fn a_stop_in_every_phase_ends_stopped() {
        const LOST: (&str, &str) = ("rdp_keeper_exited", "RDP keeper process exited with code 5");
        let check = vec![Action::Announce(Stage::Display, 1), Action::StartGate];
        let end = [Action::EndCycle, Action::Finish];
        // Phase, what the worker still does, and what the start is answered. The two phases
        // that only wait for an action to return start the next child before the stop is read.
        let rows = [
            (At::Fresh, vec![Action::Finish], Err(STOPPED_EARLY)),
            (At::SigningIn, end.to_vec(), Err(STOPPED_EARLY)),
            (
                At::Entering,
                [check, end.to_vec()].concat(),
                Err(STOPPED_EARLY),
            ),
            (At::Gating, end.to_vec(), Err(STOPPED_EARLY)),
            (
                At::Collecting,
                [start_host(), end.to_vec()].concat(),
                Err(STOPPED_READYING),
            ),
            (At::Readying, end.to_vec(), Err(STOPPED_READYING)),
            (At::Running, end.to_vec(), Ok(())),
            (At::HostDown, end.to_vec(), Ok(())),
            (At::Ending, vec![Action::Finish], Err(LOST)),
            (At::Resting, vec![Action::Finish], Err(LOST)),
        ];
        for (at, actions, answer) in rows {
            let mut bench = bench(at);
            let before = bench.state.clone();
            bench.asks(Observation::Stop);
            // The request is only marked: the phase and the status move when the worker looks.
            assert_eq!(bench.state.phase, before.phase, "{at:?}");
            assert_eq!(bench.state.status(), before.status(), "{at:?}");
            assert_eq!(bench.run_out(), actions, "{at:?}");
            assert_eq!(
                bare(bench.state.status()),
                RuntimeStatus::stopped(),
                "{at:?}"
            );
            assert_eq!(bench.answer(), Some(answer), "{at:?}");
            assert_eq!(bench.state.session(), None, "{at:?}");
            // A second stop, and a look after the worker left, change nothing.
            bench.asks(Observation::Stop);
            assert_eq!(bench.see(NOTHING), [Action::Finish], "{at:?}");
            assert_eq!(
                bare(bench.state.status()),
                RuntimeStatus::stopped(),
                "{at:?}"
            );
        }
    }

    #[test]
    fn a_serving_seat_reads_stopping_while_it_is_torn_down() {
        let stopping = RuntimeStatus {
            state: RuntimeState::Stopping,
            detail: None,
            ..RuntimeStatus::default()
        };
        let mut bench = bench(At::Running);
        bench.asks(Observation::Stop);
        assert_eq!(bench.see(NOTHING), [Action::EndCycle]);
        assert_eq!(bare(bench.state.status()), stopping);
        // A host waiting to start again keeps its line instead.
        let mut bench = self::bench(At::HostDown);
        let before = bare(bench.state.status());
        bench.asks(Observation::Stop);
        assert_eq!(bench.see(NOTHING), [Action::EndCycle]);
        assert_eq!(bare(bench.state.status()), before);
    }

    #[test]
    fn a_gate_that_exits_wins_over_a_stop() {
        let mut bench = bench(At::Gating);
        bench.asks(Observation::Stop);
        assert_eq!(bench.see(gate(1)), [Action::EndCycle]);
        // The failed gate was owed another session, so it is not the start's answer.
        assert_eq!(bench.see(NOTHING), [Action::Finish]);
        assert_eq!(bench.answer(), Some(Err(STOPPED_EARLY)));
        assert_eq!(bare(bench.state.status()), RuntimeStatus::stopped());
    }

    #[test]
    fn a_restart_wait_that_is_over_wins_over_a_stop() {
        let mut bench = bench(At::HostDown);
        bench.asks(Observation::Stop);
        // The host starts once more; its ready wait is where the stop is read.
        bench.later(Duration::from_secs(1));
        assert_eq!(bench.see(NOTHING), [Action::StartHost]);
        assert_eq!(bench.see(READY), [Action::EndCycle]);
        assert_eq!(bench.ending(), Some(STOPPED_READYING));
        assert_eq!(bench.run_out(), [Action::Finish]);
        assert_eq!(bench.answer(), Some(Err(STOPPED_READYING)));
    }

    #[test]
    fn a_stage_keeps_the_name_the_log_carries() {
        let stages = [Stage::SignIn, Stage::Display, Stage::Host];
        let names = ["sign_in", "display_check", "host_ready"];
        assert_eq!(stages.map(Stage::name), names);
    }

    #[test]
    fn a_start_out_of_time_reads_failed_from_every_stage() {
        const SIGN_IN: &str = "signing in to the seat's Windows account";
        const DISPLAY: &str = "checking the seat's screen and graphics card";
        const HOST: &str = "starting Punktfunk on the seat";
        let rows = [
            (At::Fresh, SIGN_IN),
            (At::SigningIn, SIGN_IN),
            (At::Entering, SIGN_IN),
            (At::Gating, DISPLAY),
            (At::Collecting, DISPLAY),
            (At::Readying, HOST),
        ];
        for (at, stage) in rows {
            // A stop asked before or after the start gave up doesn't make it a stopped seat.
            for stop_first in [false, true] {
                let mut bench = bench(at);
                let refused = bench.state.start_timeout();
                let message = format!("seat did not become ready within 180 seconds while {stage}");
                assert_eq!(refused, error("start_timeout", &message), "{at:?}");
                if stop_first {
                    bench.asks(Observation::Stop);
                }
                bench.asks(Observation::StartTimedOut);
                let why = format!("The seat didn't start in time. It stopped while {stage}.");
                assert_eq!(
                    bare(bench.state.status()),
                    shows(RuntimeState::Failed, &why)
                );
                // A second start before the worker leaves answers the timeout too.
                let timed_out = Err(("start_timeout", message.as_str()));
                assert_eq!(bench.answer(), Some(timed_out), "{at:?}");
                if !stop_first {
                    bench.asks(Observation::Stop);
                }
                bench.run_out();
                assert_eq!(
                    bare(bench.state.status()),
                    shows(RuntimeState::Failed, &why)
                );
                assert!(matches!(bench.answer(), Some(Err(_))), "{at:?}");
            }
        }
    }
}
