//! Box takeover: what the host takes of the box's own session to stream a managed one, and the
//! hand-back that returns it (`design/gamemode-and-dedicated-sessions.md`).
//!
//! Every step is in the topology journal before it runs, so the next host hands back what a dead
//! one held. One mutex holds the state and one condvar signals its waits: a connect waits out a
//! running hand-back, a managed launch waits out another; the hand-back never waits on a launch.
//! Pin with the tests below. The state table is in the planning repo,
//! `design/component-health/17-gamescope-onto-journal.md`.

use super::*;
use crate::routing::{TakeoverInapplicable, TakeoverVerdict};
use crate::topology_journal as journal;

/// What this host took of the box, and where its hand-back stands. `std::sync::Mutex` is not
/// reentrant: never call anything that takes [`takeover()`] while holding it.
pub(super) struct Takeover {
    /// Steps taken, oldest first, each journaled before it ran.
    owed: Vec<Owed>,
    /// Mode the managed session was launched at, for warm reuse. `None` while it launches, or for
    /// a session adopted from a dead host: stop that one, never reuse it.
    pub(super) managed: Option<SessionState>,
    /// The device whose session took the box: its keep-alive times the hand-back. A joiner only
    /// watches, so it never lands here.
    pub(super) owner: Option<[u8; 32]>,
    hand_back: HandBack,
    /// A managed launch runs. Another waits on [`WAKE`]; nothing on the hand-back path does.
    launching: bool,
    /// The latest launch took the box and produced no session ([`launch_tracked`]), so the
    /// hand-back waits the debounce at most, whatever the keep-alive ([`hand_back_delay`]).
    launch_failed: bool,
    /// The managed `exclusive` darken. A bool, not a count: the session outlives connects.
    darkened: bool,
    /// Bumped by every hand-back, so a launch that began before one can tell.
    pub(super) generation: u64,
    /// An older host's runtime mask on the owed units. Missing one bars Game Mode until reboot;
    /// unmasking a unit nobody masked is a no-op.
    autologin_masked: bool,
    /// This host idled the autologin ([`install_idle_dropin`]): arms the in-stream switch gate.
    pub(super) idle_dropin_armed: bool,
    /// Sentinel mtime when the gate was armed. ChimeraOS-layout `os-session-select` writes
    /// `~/.config/steamos-session-select` in its USER pass, the only durable trace of an
    /// in-stream "Switch to Desktop". Bazzite and SteamOS write none; the session watcher
    /// follows their switch instead.
    select_baseline: Baseline,
    /// When [`honor_session_select_switch`] last ran. While recent, refuse a managed relaunch:
    /// gamescope+Steam come up faster than KWin, and a delivering pipeline ends re-detection.
    pub(super) switch_honored_at: Option<Instant>,
}

/// One step this host owes the box, and its journal seq.
struct Owed {
    seq: u64,
    step: TakeoverStep,
}

static TAKEOVER: std::sync::Mutex<Takeover> = std::sync::Mutex::new(Takeover::EMPTY);

/// Signalled when a hand-back or a managed launch ends.
static WAKE: std::sync::Condvar = std::sync::Condvar::new();

pub(super) fn takeover() -> std::sync::MutexGuard<'static, Takeover> {
    TAKEOVER.lock().unwrap_or_else(|e| e.into_inner())
}

fn wait(t: std::sync::MutexGuard<'static, Takeover>) -> std::sync::MutexGuard<'static, Takeover> {
    WAKE.wait(t).unwrap_or_else(|e| e.into_inner())
}

impl Takeover {
    const EMPTY: Self = Self {
        owed: Vec::new(),
        managed: None,
        owner: None,
        hand_back: HandBack::Idle,
        launching: false,
        launch_failed: false,
        darkened: false,
        generation: 0,
        autologin_masked: false,
        idle_dropin_armed: false,
        select_baseline: Baseline::Unset,
        switch_honored_at: None,
    };

    /// Anything left to hand back.
    fn live(&self) -> bool {
        !self.owed.is_empty()
    }

    pub(super) fn owes(&self, pick: impl Fn(&TakeoverStep) -> bool) -> bool {
        self.owed.iter().any(|o| pick(&o.step))
    }

    /// Journal `step` and owe it, before the change it reverses.
    fn owe(&mut self, step: TakeoverStep) {
        let undo = journal::Undo::Takeover {
            boot: crate::proc::boot_id(),
            step: step.clone(),
        };
        let seq = journal::push("gamescope", undo);
        self.owed.push(Owed { seq, step });
    }

    /// [`owe`](Self::owe) unless `step` is owed already. `true` when this call owed it.
    pub(super) fn owe_once(&mut self, step: TakeoverStep) -> bool {
        if self.owes(|s| *s == step) {
            return false;
        }
        self.owe(step);
        true
    }

    /// Drop the steps `pick` matches from memory and the journal: undone, or moot.
    pub(super) fn settle(&mut self, pick: impl Fn(&TakeoverStep) -> bool) {
        self.owed.retain(|o| {
            let hit = pick(&o.step);
            if hit {
                journal::done(o.seq);
            }
            !hit
        });
    }

    /// Drop the steps `pick` matches from memory only. The caller settles their seqs in the
    /// journal once their undo has run.
    fn take(&mut self, pick: impl Fn(&TakeoverStep) -> bool) -> Vec<Owed> {
        let (taken, kept) = std::mem::take(&mut self.owed)
            .into_iter()
            .partition(|o| pick(&o.step));
        self.owed = kept;
        taken
    }

    /// The owed autologin units, once each.
    fn units(&self) -> Vec<String> {
        hand_back_plan(self.owed.iter().map(|o| &o.step)).units
    }

    /// Owe exactly `units` as stopped: journaled before the steps they replace are settled.
    fn owe_units(&mut self, units: Vec<String>) {
        let old: Vec<u64> = self
            .owed
            .iter()
            .filter(|o| matches!(o.step, TakeoverStep::RestartAutologin { .. }))
            .map(|o| o.seq)
            .collect();
        if !units.is_empty() {
            self.owe(TakeoverStep::RestartAutologin { units });
        }
        self.owed.retain(|o| {
            let hit = old.contains(&o.seq);
            if hit {
                journal::done(o.seq);
            }
            !hit
        });
    }
}

/// What the session-plus hand-back does, from the steps it takes. The SteamOS hand-back is its
/// own branch ([`TakeoverStep::RestoreSteamos`]).
#[derive(Debug, Default, PartialEq, Eq)]
struct HandBackPlan {
    /// Stop our managed session.
    session: bool,
    /// Autologin units to restart, once each.
    units: Vec<String>,
    /// A display manager an older host stopped: restart it, and not the units (no seat without
    /// a DM login).
    dm: Option<String>,
}

fn hand_back_plan<'a>(steps: impl IntoIterator<Item = &'a TakeoverStep>) -> HandBackPlan {
    let mut plan = HandBackPlan::default();
    for step in steps {
        match step {
            TakeoverStep::RestartAutologin { units } => {
                for u in units {
                    if !plan.units.contains(u) {
                        plan.units.push(u.clone());
                    }
                }
            }
            TakeoverStep::StopSession => plan.session = true,
            TakeoverStep::StartDisplayManager { unit } => plan.dm = Some(unit.clone()),
            TakeoverStep::RestoreSteamos
            | TakeoverStep::UnsetScreenEnv
            | TakeoverStep::RemoveSessionDropin => {}
        }
    }
    plan
}

/// Where the hand-back stands. Pure: [`takeover()`] holds it, [`WAKE`] signals its ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandBack {
    /// None planned: nothing owed, a session holds the box, or keep-alive keeps it.
    Idle,
    /// Hand back at this instant unless a client returns first.
    Due(Instant),
    /// Running. A connect waits for it; `then` is a hand-back asked for meanwhile.
    Running { then: Option<Instant> },
}

/// What a (re)connect does about the hand-back.
#[derive(Debug, PartialEq, Eq)]
enum Cancel {
    Nothing,
    Cancelled,
    /// One is running: wait it out, then take a fully restored box.
    Wait,
}

impl HandBack {
    /// Hand back at `at`; `None` keeps the box (keep-alive forever).
    fn schedule(&mut self, at: Option<Instant>) {
        *self = match *self {
            HandBack::Running { .. } => HandBack::Running { then: at },
            _ => at.map_or(HandBack::Idle, HandBack::Due),
        };
    }

    /// Restoring under a fresh takeover is the relogin storm, so a running one is never raced.
    fn cancel(&mut self) -> Cancel {
        match *self {
            HandBack::Idle => Cancel::Nothing,
            HandBack::Due(_) => {
                *self = HandBack::Idle;
                Cancel::Cancelled
            }
            HandBack::Running { .. } => Cancel::Wait,
        }
    }

    /// The worker's tick: start the one that is due.
    fn pop(&mut self, now: Instant) -> bool {
        match *self {
            HandBack::Due(at) if now >= at => {
                *self = HandBack::Running { then: None };
                true
            }
            _ => false,
        }
    }

    fn running(&self) -> bool {
        matches!(self, HandBack::Running { .. })
    }

    fn finish(&mut self) {
        if let HandBack::Running { then } = *self {
            *self = then.map_or(HandBack::Idle, HandBack::Due);
        }
    }

    fn due_in(&self, now: Instant) -> Option<Duration> {
        match *self {
            HandBack::Due(at) | HandBack::Running { then: Some(at) } => {
                Some(at.saturating_duration_since(now))
            }
            _ => None,
        }
    }
}

/// Ends a running hand-back when dropped, a panic included, and wakes whoever waits on it.
struct HandBackRun;

impl Drop for HandBackRun {
    fn drop(&mut self) {
        takeover().hand_back.finish();
        WAKE.notify_all();
    }
}

/// One managed launch at a time, held across `launch_session` (~90 s). A second connect in the
/// window re-tests after the first records; otherwise its `stop_session` kills the unit the
/// first still polls. Dropping it wakes the next.
pub(super) struct Launching;

impl Launching {
    /// Wait out another launch, then cancel a hand-back scheduled meanwhile: run under this
    /// launch, it would stop the session being started.
    pub(super) fn begin() -> Launching {
        let mut t = takeover();
        while t.launching {
            t = wait(t);
        }
        t.launching = true;
        drop(t);
        let launching = Launching;
        cancel_pending_restore();
        launching
    }
}

impl Drop for Launching {
    fn drop(&mut self) {
        takeover().launching = false;
        WAKE.notify_all();
    }
}

/// After an in-stream desktop switch, refuse managed relaunch until the DM session can come up.
pub(super) const SWITCH_HONOR_GRACE: Duration = Duration::from_secs(120);

/// Managed-route [`crate::panel_dpms`] hold for `Topology::Exclusive`. Managed reports
/// `SessionManaged`, so the registry runs no display restore for it; the hand-back releases it.
/// The panel call runs under the takeover lock, so a release never overtakes an acquire.
pub(super) fn managed_darken_acquire(exclusive: bool) {
    let mut t = takeover();
    if managed_darken_acquire_edge(&mut t.darkened, exclusive) {
        crate::panel_dpms::acquire_stream_darken();
    }
}

/// Idempotent release: the hand-back calls this above every early return.
fn managed_darken_release() {
    let mut t = takeover();
    if managed_darken_release_edge(&mut t.darkened) {
        crate::panel_dpms::release_stream_darken();
    }
}

/// 0→1 edge: take a hold? Split so the balance is testable without a compositor.
fn managed_darken_acquire_edge(held: &mut bool, exclusive: bool) -> bool {
    if !exclusive || *held {
        return false;
    }
    *held = true;
    true
}

/// 1→0 edge: release a hold? Split so the balance is testable without a compositor.
fn managed_darken_release_edge(held: &mut bool) -> bool {
    if !*held {
        return false;
    }
    *held = false;
    true
}

/// Default hand-back delay: long enough that a controller hiccup reuses the warm session.
const RESTORE_DEBOUNCE: Duration = Duration::from_secs(5);

/// SteamOS hand-back with no panel connected: how soon to look for one again.
const NO_PANEL_RECHECK: Duration = Duration::from_secs(30);

/// A dead host's takeover waits this long after host start, so a client reconnecting right
/// after the restart keeps the box instead of watching its Game Mode bounce.
const ADOPT_GRACE: Duration = Duration::from_secs(15);

pub(super) fn restore_generation() -> u64 {
    takeover().generation
}

/// A dead host's step, reached through `topology_journal::reclaim`: this host owes it from now
/// on. Another boot already handed the box back. A host that held nothing hands it back after
/// [`ADOPT_GRACE`]; one mid-takeover hands it back with its own.
pub(super) fn adopt(undo: &journal::Undo) -> Result<()> {
    let journal::Undo::Takeover { boot, step } = undo else {
        bail!("no undo in gamescope for {undo:?}");
    };
    if *boot != crate::proc::boot_id() {
        return Ok(());
    }
    let mut t = takeover();
    if !t.live() {
        t.hand_back.schedule(Some(Instant::now() + ADOPT_GRACE));
    }
    if let TakeoverStep::RestartAutologin { units } = step {
        let mut all = t.units();
        let new: Vec<String> = units.iter().filter(|u| !all.contains(u)).cloned().collect();
        all.extend(new);
        t.owe_units(all);
    } else {
        t.owe_once(step.clone());
    }
    Ok(())
}

/// A pre-journal host's crash record, `$XDG_RUNTIME_DIR/punktfunk-session-takeover.json`. Read
/// once at startup into journal steps, then deleted. Its units were masked.
#[derive(serde::Deserialize, Default)]
struct LegacyRecord {
    stopped_autologin: Vec<String>,
    steamos: bool,
    #[serde(default)]
    stopped_dm: Option<String>,
    #[serde(default)]
    managed_session: bool,
    #[serde(default)]
    forced_screen_env: bool,
}

impl LegacyRecord {
    fn steps(self) -> Vec<TakeoverStep> {
        let mut steps = Vec::new();
        if !self.stopped_autologin.is_empty() {
            steps.push(TakeoverStep::RestartAutologin {
                units: self.stopped_autologin,
            });
        }
        if let Some(unit) = self.stopped_dm {
            steps.push(TakeoverStep::StartDisplayManager { unit });
        }
        // SteamOS's managed session is its own target, which its own step restarts.
        if self.steamos {
            steps.push(TakeoverStep::RestoreSteamos);
        } else if self.managed_session {
            steps.push(TakeoverStep::StopSession);
        }
        if self.forced_screen_env {
            steps.push(TakeoverStep::UnsetScreenEnv);
        }
        steps
    }
}

fn legacy_record_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-session-takeover.json")
}

/// Owe what an older host's record names, then delete it. A record that does not parse is
/// deleted too: it could only ever be read as no takeover.
fn adopt_legacy_record() {
    let path = legacy_record_path();
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    if let Ok(record) = serde_json::from_slice::<LegacyRecord>(&bytes) {
        let mut t = takeover();
        t.autologin_masked |= !record.stopped_autologin.is_empty();
        for step in record.steps() {
            t.owe_once(step);
        }
    }
    let _ = std::fs::remove_file(&path);
}

/// Restart autologin units left `active` under a swept idle drop-in. Gated on a dark box
/// ([`box_session_live`]): bouncing a live game mode or desktop is the bug. Active under the
/// drop-in means "running the sleep". They leave the takeover before the watch starts: a second
/// restart would kill a Steam still booting, and an owed unit reads to the watch as a new takeover.
fn hand_back_idled_units_after_crash() {
    if box_session_live() {
        return; // already drawing — the drop-in was inert
    }
    let units: Vec<String> = listed_autologin_units()
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, active)| active == "active")
        .map(|(unit, _)| unit)
        .collect();
    if units.is_empty() {
        return;
    }
    tracing::warn!(
        ?units,
        "gamescope: the box's Game Mode is running the dead host's idle placeholder and its panel \
         is dark — restarting it"
    );
    for unit in &units {
        restore_verb(Site::Idled(unit));
    }
    {
        let mut t = takeover();
        let owed = t.units();
        let left: Vec<String> = owed
            .iter()
            .filter(|u| !units.contains(u))
            .cloned()
            .collect();
        if left.len() != owed.len() {
            t.owe_units(left);
        }
    }
    ensure_box_session_or_escalate(&units);
}

/// Our managed unit is up. Unknown and `deactivating` read as not running.
pub(super) fn session_unit_running() -> bool {
    unit_state(SESSION_UNIT)
        .is_some_and(|s| matches!(s.as_str(), "active" | "activating" | "reloading"))
}

/// Hand back what a dead host left. `topology_journal::reclaim` runs first and [`adopt`]s its
/// journaled steps; this sweeps what no owed step names, then hands the rest back after
/// [`ADOPT_GRACE`]. Call once from `serve`, with [`start_restore_worker`].
pub fn restore_takeover_on_startup() {
    // The bind drop-in applies to the TEMPLATE. A leftover copy asks Game Mode for a mount
    // namespace whose tmpfs sources are gone, so the box cannot enter Game Mode.
    if remove_session_plus_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover gamescope-session-plus bind drop-in from a previous \
             host instance — it asks the box's OWN Game Mode session for a mount namespace, and \
             everything it binds lives in tmpfs, so after a reboot that unit could not start at all"
        );
        systemctl_user(&["daemon-reload"]);
    }
    takeover().settle(|s| matches!(s, TakeoverStep::RemoveSessionDropin));
    adopt_legacy_record();
    let (session, steamos) = {
        let t = takeover();
        (
            t.owes(|s| matches!(s, TakeoverStep::StopSession)),
            t.owes(|s| matches!(s, TakeoverStep::RestoreSteamos)),
        )
    };
    // A fresh host owns no session unit unless a dead one's is owed. One left running holds
    // Steam with nothing to stop it, and reads as the box's own session to the check below.
    if !session && session_unit_running() {
        tracing::warn!(
            "gamescope: stopping a managed session a previous host instance left running"
        );
        stop_session(SESSION_UNIT);
    }
    // Removing the FILE does not restart the unit still sleeping under it. Hand it back.
    if remove_idle_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover idle drop-in from a previous host instance — the box's \
             own Game Mode session would have started and then done nothing"
        );
        hand_back_idled_units_after_crash();
    }
    // Only an owed SteamOS step keeps its drop-in, for the hand-back to remove. Any other copy is
    // stranded; an older host's `$HOME` one also outlives reboots.
    if !steamos && remove_steamos_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover SteamOS headless drop-in from a previous host instance"
        );
        systemctl_user(&["daemon-reload"]);
    }
    {
        let mut t = takeover();
        if !t.live() {
            return;
        }
        let steps: Vec<&TakeoverStep> = t.owed.iter().map(|o| &o.step).collect();
        tracing::warn!(
            ?steps,
            "gamescope: found a stranded takeover from a previous host instance — scheduling TV \
             restore"
        );
        t.hand_back.schedule(Some(Instant::now() + ADOPT_GRACE));
    }
    // Launch-time baseline is gone; a long-existing sentinel must not read as a live switch.
    record_session_select_baseline();
}

/// Live managed-takeover marker (arms the in-stream switch gate). Process memory, not disk: a
/// drop-in we did not write belongs to a dead host.
pub(super) fn takeover_idled() -> bool {
    takeover().idle_dropin_armed
}

/// No-op unless a host set them: `unset-environment` is indiscriminate. Settled only once systemd
/// says done; otherwise the next hand-back, or the next host, asks again.
fn unset_forced_session_screen_env() {
    if !takeover().owes(|s| matches!(s, TakeoverStep::UnsetScreenEnv)) {
        return;
    }
    if matches!(restore_verb(Site::ScreenEnv), RestoreVerb::Done) {
        takeover().settle(|s| matches!(s, TakeoverStep::UnsetScreenEnv));
    }
}

/// `--runtime` mask so a reboot clears it. A mask while the DM is up is the relogin storm: the
/// session script `systemctl --user --wait start`s the unit, so a mask fails every autologin in
/// milliseconds and Relogin has no backoff. See `design/sddm-relogin-storm-starves-input-handoff.md`.
///
/// Live takeovers idle instead ([`install_idle_dropin`]). Kept so tests can build the adopted-mask
/// state [`lift_autologin_mask`] still cleans up. Lift on a mid-stream desktop switch or Game Mode
/// stays barred until reboot.
#[cfg(test)]
fn mask_unit(unit: &str) {
    systemctl_user(&["mask", "--runtime", unit]);
}

/// Every restore path must unmask before restarting, or Game Mode stays broken until reboot.
fn unmask_unit(unit: &str) -> RestoreVerb {
    restore_verb(Site::Unmask(unit))
}

/// Idempotent. Keeps the stopped units: mask lifetime is shorter than the takeover, and restore
/// still owes them a start. Holds [`takeover()`] across the unmask, so a restore never restarts a
/// unit that is still masked. A unit systemd did not unmask keeps the lift owed.
fn lift_autologin_mask() {
    let mut t = takeover();
    if !t.autologin_masked {
        return;
    }
    let units = t.units();
    // Every unit is asked, whatever the one before answered.
    let lifted = units
        .iter()
        .filter(|u| matches!(unmask_unit(u), RestoreVerb::Done))
        .count()
        == units.len();
    t.autologin_masked = !lifted;
    if lifted {
        tracing::info!(
            ?units,
            "gamescope: lifted the takeover's runtime mask — the box can enter its own game mode \
             again"
        );
    }
}

/// Only a desktop switch ends the mask window. Gaming is our own session; None is a relaunch gap.
fn switch_ends_mask_window(kind: crate::ActiveKind) -> bool {
    use crate::ActiveKind;
    matches!(
        kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
            | ActiveKind::DesktopNiri
            | ActiveKind::DesktopUmbriel
    )
}

/// Watcher half of the mid-stream hand-back (sentinel detector is the other). Both must run or
/// one distro family keeps an idled Game Mode. The box's display manager runs its own session
/// from here, so the takeover owes it nothing more: our session stops, its darken comes off,
/// and a launch in flight bails. Blocks for up to [`STEAM_SHUTDOWN_WAIT`] while our Steam quits
/// a game cleanly, so the watcher calls it after it has signalled the rebuild. A connect that
/// finds a desktop calls it too, for a switch no watcher saw.
pub fn release_autologin_mask(switched_to: crate::ActiveKind) {
    if !switch_ends_mask_window(switched_to) {
        return;
    }
    lift_autologin_mask();
    // Left on, the drop-in makes "Return to Gaming Mode" start a unit that only sleeps.
    if remove_idle_dropin() {
        tracing::info!(
            switched_to = ?switched_to,
            "gamescope: the box left our game session for a desktop — removed the takeover's idle \
             drop-in so its own Game Mode runs for real again"
        );
    }
    // steamos-manager ends the box's session, not ours. Our Steam would keep the single instance,
    // so the box's own Steam exits at once on "Return to Gaming Mode" until gamescope-session-plus
    // gives up and switches the box back to the desktop. Its units leave the takeover too: a
    // hand-back restart would kill the Game Mode the user returns to. Only a mask that did not
    // lift keeps them, for the next lift.
    let ours = {
        let mut t = takeover();
        t.managed = None;
        if !t.autologin_masked {
            t.settle(|s| matches!(s, TakeoverStep::RestartAutologin { .. }));
        }
        let ours = t.owes(|s| matches!(s, TakeoverStep::StopSession));
        t.settle(|s| matches!(s, TakeoverStep::StopSession));
        if ours {
            t.generation += 1;
        }
        ours
    };
    managed_darken_release();
    if ours {
        // A launch that starts meanwhile waits: our stop would kill the unit it starts.
        let _launching = Launching::begin();
        let our_steam = stop_our_session(true);
        takeover().managed = None;
        tracing::info!(
            "gamescope: stopped our game session — the box's own Steam can start when it returns \
             to Game Mode"
        );
        if our_steam {
            restart_swallowed_desktop_steam();
        }
    }
}

/// Stop our managed session; `true` when our Steam ran in it. `patient` gives Steam
/// [`STEAM_SHUTDOWN_WAIT`] to end a game and save before the kill; only the shutdown restore,
/// on its 20 s grace, keeps [`kill_unit`]'s 8 s.
fn stop_our_session(patient: bool) -> bool {
    let steam = steam_pid_in_unit(SESSION_UNIT);
    if let Some(pid) = steam.filter(|_| patient) {
        shut_steam_down(pid, STEAM_SHUTDOWN_WAIT);
    }
    stop_session(SESSION_UNIT);
    steam.is_some()
}

/// `unit`'s main process is our idle drop-in's `sleep`: a takeover idled it and nothing has
/// replaced it since. A unit the box stopped or restarted itself is its own again. An
/// unanswered query reads as idled, the restart the hand-back always made.
fn runs_idle_placeholder(unit: &str) -> bool {
    let Some(out) = crate::proc::output_within(
        Command::new("systemctl").args(["--user", "show", unit, "-p", "MainPID", "--value"]),
        UNIT_QUERY_BUDGET,
    )
    .ok()
    .filter(|o| o.status.success()) else {
        return true;
    };
    let pid: u32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    pid != 0
        && std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "sleep")
}

fn display_manager_unit() -> Option<String> {
    display_manager_unit_under(std::path::Path::new("/etc/systemd/system"))
}

fn display_manager_unit_under(base: &std::path::Path) -> Option<String> {
    let target = std::fs::read_link(base.join("display-manager.service")).ok()?;
    target.file_name().map(|n| n.to_string_lossy().into_owned())
}

/// Pure DM decision. Runtime guards stay with [`stop_autologin_sessions`]. Flavor is not an
/// input: a failed DM stop degrades to attach, never to mask-only (that *is* the storm).
struct DmPlan {
    /// No live gaming instance. Killing leftovers frees no Steam; stopping the DM would kill a desktop.
    skip: bool,
    /// Live gaming session behind a DM: idle it ([`install_idle_dropin`]). Stopping the DM leaves
    /// nothing that can start a desktop session.
    dm_relogins: bool,
}

fn dm_plan(dm: Option<&str>, any_live: bool) -> DmPlan {
    DmPlan {
        skip: !any_live,
        dm_relogins: dm.is_some() && any_live,
    }
}

/// Helper names the DM from the `display-manager.service` symlink — this process never names a
/// unit across the privilege boundary. Two layouts: rpm/deb `libexec`, Arch `/usr/lib/<pkg>`.
const DM_HELPER_PATHS: &[&str] = &[
    "/usr/libexec/punktfunk/pf-dm-helper",
    "/usr/lib/punktfunk/pf-dm-helper",
];

/// Helper's own gate: polkit is `allow_any` (lingering user unit has no session to classify).
/// Package creates the group and adds nobody — it also gates usbip attach.
const DM_HELPER_GROUP: &str = "punktfunk";

fn installed_dm_helper() -> Option<&'static str> {
    DM_HELPER_PATHS
        .iter()
        .copied()
        .find(|p| std::path::Path::new(p).exists())
}

/// Four shapes, four fixes. A helper that never executed must not read as one that ran and refused.
enum DmHelperError {
    /// No packaged helper. Polkit-rule route; the group does not apply.
    NotInstalled,
    /// `pkexec` could not be spawned. Nothing evaluated the request.
    NotExecutable { helper: &'static str, io: String },
    /// pkexec 126/127 — helper only exits 0/1/2, so this never reached its group gate.
    Denied {
        helper: &'static str,
        code: i32,
        stderr: String,
    },
    /// Helper ran; stderr names the user, group, and `usermod` line. Pass it through.
    Refused {
        helper: &'static str,
        code: Option<i32>,
        stderr: String,
    },
}

impl DmHelperError {
    fn shape(&self) -> &'static str {
        match self {
            Self::NotInstalled => "not-installed",
            Self::NotExecutable { .. } => "not-executable",
            Self::Denied { .. } => "denied",
            Self::Refused { .. } => "refused",
        }
    }
}

impl std::fmt::Display for DmHelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => write!(
                f,
                "no packaged pf-dm-helper on this box (looked in {}) — install the punktfunk \
                 package, or add a display-manager polkit rule for your user (see \
                 https://docs.punktfunk.unom.io/docs/gamescope)",
                DM_HELPER_PATHS.join(" and ")
            ),
            Self::NotExecutable { helper, io } => write!(
                f,
                "{helper} is installed but could not be run via pkexec ({io}) — this box appears \
                 to have no polkit; add a display-manager polkit rule for your user instead (see \
                 https://docs.punktfunk.unom.io/docs/gamescope)"
            ),
            Self::Denied {
                helper,
                code,
                stderr,
            } => write!(
                f,
                "pkexec never ran {helper} (exit {code}{}) — polkit did not authorize \
                 io.unom.punktfunk.dm-helper, so the action is missing or overridden; reinstall \
                 the punktfunk package, or add a display-manager polkit rule for your user (see \
                 https://docs.punktfunk.unom.io/docs/gamescope)",
                suffix(stderr)
            ),
            Self::Refused {
                helper,
                code,
                stderr,
            } if stderr.is_empty() => write!(
                f,
                "{helper} ran and failed (exit {}) without printing a reason",
                code.map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string())
            ),
            Self::Refused { helper, stderr, .. } => {
                write!(f, "{helper} ran and refused: {stderr}")
            }
        }
    }
}

fn suffix(stderr: &str) -> String {
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}

/// Unbounded: a budget would kill a legitimate `systemctl` stop mid-flight. `output()` so pkexec
/// prompting gets EOF instead of blocking a stream thread on a tty it can never satisfy.
fn dm_helper(verb: &str) -> std::result::Result<(), DmHelperError> {
    let Some(helper) = installed_dm_helper() else {
        return Err(DmHelperError::NotInstalled);
    };
    let out = Command::new("pkexec")
        .arg(helper)
        .arg(verb)
        .output()
        .map_err(|e| DmHelperError::NotExecutable {
            helper,
            io: e.to_string(),
        })?;
    if out.status.success() {
        return Ok(());
    }
    // One line: these land in a `tracing` field, and the helper's two-line refusal (reason +
    // `Grant it with: …`) has to survive the trip intact.
    let stderr = String::from_utf8_lossy(&out.stderr)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    match out.status.code() {
        // pkexec's own codes: 127 "not authorized / could not execute the program", 126
        // "authentication dialog dismissed". The helper only ever exits 0, 1 or 2, so either of
        // these means the request never reached its group gate.
        Some(c @ (126 | 127)) => Err(DmHelperError::Denied {
            helper,
            code: c,
            stderr,
        }),
        // `None` = killed by a signal, and [`DmHelperError::Refused`] renders that as "signal"
        // rather than inventing a plausible-looking exit code.
        code => Err(DmHelperError::Refused {
            helper,
            code,
            stderr,
        }),
    }
}

/// Missing `punktfunk` group is silent: takeover degrades to attach (black screen). Log it at
/// startup with the `usermod` line. Gated: root / no DM / no session infra / no helper never need
/// the group. Reads the user database (`id -nG`), not `getgroups()` — that is what the helper
/// reads. Log-back-in is for usbip: this process's supplementary groups were frozen at start.
pub fn preflight_takeover_privilege() {
    let TakeoverVerdict::MissingMembership {
        user,
        dm,
        helper,
        group,
    } = takeover_privilege_verdict()
    else {
        return; // gated out, or the user is already a member — either way, nothing to say
    };
    tracing::warn!(
        %user,
        %dm,
        helper,
        group,
        "gamescope: the managed takeover on this box has to stop {dm} for a stream, which runs \
         through {helper} — and that helper only serves members of the '{group}' group, which \
         '{user}' is not in. Every takeover will degrade silently: the stream mirrors the box's \
         own session instead, which with the panel off looks like a black screen on every \
         connect. Fix it once with `sudo usermod -aG {group} {user}`, then restart the computer — \
         a lingering `systemd --user` keeps the group set it started with, and the same group gates \
         the virtual Steam Deck pad's usbip nodes. It can present arbitrary emulated USB devices, \
         so join it only on a machine you trust."
    );
}

/// Same value the console check maps. Distinct `Inapplicable` reasons — a hidden row cannot answer
/// "why isn't this relevant here?".
pub fn takeover_privilege_verdict() -> TakeoverVerdict {
    if crate::proc::current_uid() == 0 {
        return TakeoverVerdict::Inapplicable {
            why: TakeoverInapplicable::Root,
        };
    }
    let Some(dm) = display_manager_unit() else {
        return TakeoverVerdict::Inapplicable {
            why: TakeoverInapplicable::NoDisplayManager,
        };
    };
    if !managed_session_available() {
        return TakeoverVerdict::Inapplicable {
            why: TakeoverInapplicable::NoManagedSession,
        };
    }
    let Some(helper) = installed_dm_helper() else {
        return TakeoverVerdict::Inapplicable {
            why: TakeoverInapplicable::NoPackagedHelper,
        };
    };
    let Some(user) = current_user_name() else {
        return TakeoverVerdict::Inapplicable {
            why: TakeoverInapplicable::UnknownUser,
        };
    };
    let group = DM_HELPER_GROUP;
    if user_in_group(&user, group) {
        return TakeoverVerdict::Ok { user, group };
    }
    TakeoverVerdict::MissingMembership {
        user,
        dm,
        helper,
        group,
    }
}

/// `id -un <uid>`, not `$USER`: a lingering unit's env is whatever the manager started with.
fn current_user_name() -> Option<String> {
    let out = crate::proc::output_within(
        Command::new("id").args(["-un", &uid_string()]),
        Duration::from_secs(5),
    )
    .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// Same question `pf-dm-helper` asks. Budgeted: NSS can block. Fail-open (don't accuse).
fn user_in_group(user: &str, group: &str) -> bool {
    let Ok(out) = crate::proc::output_within(
        Command::new("id").args(["-nG", user]),
        Duration::from_secs(5),
    ) else {
        return true; // couldn't ask ⇒ don't accuse: a false alarm here sends people down a wrong path
    };
    if !out.status.success() {
        return true;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .any(|g| g == group)
}

/// System bus, never interactive. `--no-ask-password` removes the dialog, not the wait; timeout
/// is `false` so callers fall through to pkexec. Stderr at DEBUG: refusal is the unprivileged
/// probe, not news.
fn systemctl_system(args: &[&str]) -> bool {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--no-ask-password").args(args);
    let Ok(out) = crate::proc::output_within(&mut cmd, DM_VERB_BUDGET) else {
        return false; // timed out / could not spawn — the helper path is next either way
    };
    if !out.status.success() {
        tracing::debug!(
            ?args,
            status = ?out.status.code(),
            stderr = %String::from_utf8_lossy(&out.stderr).trim(),
            "systemctl on the system bus was refused — falling through to the packaged pkexec \
             helper (expected on an unprivileged host)"
        );
    }
    out.status.success()
}

fn uid_string() -> String {
    crate::proc::current_uid().to_string()
}

/// `reset-failed` then `restart`: a relogin loop trips the start limit, and a plain restart is
/// refused until that clears. Helper `Err` is the no-graphical-session failure.
fn restore_display_manager(dm: &str) -> std::result::Result<(), DmHelperError> {
    let _ = systemctl_system(&["reset-failed", dm]);
    if systemctl_system(&["restart", dm]) {
        return Ok(());
    }
    dm_helper("restore")
}

/// USER pass records the sentinel; ROOT pass rewrites DM autologin only while the DM is running.
const OS_SESSION_SELECT: &str = "/usr/libexec/os-session-select";

fn session_select_sentinel() -> Option<std::path::PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(
        std::path::Path::new(&home)
            .join(".config")
            .join("steamos-session-select"),
    )
}

fn session_select_mtime() -> Option<std::time::SystemTime> {
    let path = session_select_sentinel()?;
    std::fs::metadata(path).ok()?.modified().ok()
}

/// At takeover and again at launch: the switch *into* game mode writes the sentinel on the way in.
/// Baselining only at launch treats a months-old file as a live request after a failed launch.
pub(super) fn record_session_select_baseline() {
    let baseline = session_select_mtime().map_or(Baseline::Absent, Baseline::At);
    takeover().select_baseline = baseline;
}

pub(super) fn session_select_requested() -> bool {
    let baseline = takeover().select_baseline;
    sentinel_advanced(baseline, session_select_mtime())
}

/// The sentinel's mtime when the switch gate was armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Baseline {
    /// Never armed: an ancient write is not a live request.
    Unset,
    /// No sentinel then: any one now was written during the session.
    Absent,
    /// Only a newer write is a request.
    At(std::time::SystemTime),
}

fn sentinel_advanced(baseline: Baseline, now: Option<std::time::SystemTime>) -> bool {
    match (baseline, now) {
        (Baseline::At(base), Some(now)) => now > base,
        (Baseline::Absent, Some(_)) => true,
        _ => false,
    }
}

/// Hand the box back and follow the desktop. Caller refuses managed relaunch for
/// [`SWITCH_HONOR_GRACE`] so re-detection follows the desktop instead of racing it.
pub(super) fn honor_session_select_switch(adopted_dm: Option<String>) {
    tracing::info!(
        adopted_dm = ?adopted_dm,
        "gamescope: in-stream session-select detected — handing the box's own game mode back and \
         following the desktop session the user selected"
    );
    // Mask first, while the owed units still name what it covers. Forced SCREEN_* and the bind
    // stay owed: only the units and the session go.
    lift_autologin_mask();
    {
        let mut t = takeover();
        t.owe_units(Vec::new());
        t.settle(|s| matches!(s, TakeoverStep::StopSession));
        t.managed = None;
    }
    // The switch already killed Steam; clear the unit.
    stop_session(SESSION_UNIT);
    // A switch is not a disconnect; skip this and "Return to Gaming Mode" starts a sleep.
    if remove_idle_dropin() {
        tracing::info!(
            "gamescope: removed the takeover's idle drop-in — the box's own Game Mode runs for \
             real again"
        );
    }
    // Live takeovers leave the DM up; only an adopted pre-idle stop still owes a DM restore.
    if let Some(dm) = adopted_dm {
        replay_switch_under_restored_dm(&dm);
    }
    record_session_select_baseline();
    takeover().switch_honored_at = Some(Instant::now());
}

/// The display manager an older host stopped, settled: the caller restarts it.
pub(super) fn take_adopted_dm() -> Option<String> {
    let mut t = takeover();
    let dm = t.owed.iter().find_map(|o| match &o.step {
        TakeoverStep::StartDisplayManager { unit } => Some(unit.clone()),
        _ => None,
    });
    t.settle(|s| matches!(s, TakeoverStep::StartDisplayManager { .. }));
    dm
}

/// Adopted pre-idle takeover only: start the stopped DM, run `os-session-select desktop`, stop
/// the autologin unit so Relogin enters the desktop. Nothing live stops a DM any more.
fn replay_switch_under_restored_dm(dm: &str) {
    if let Err(e) = restore_display_manager(dm) {
        tracing::warn!(
            %dm,
            reason = %e,
            "gamescope: display-manager start was denied — the desktop switch may need a manual \
             `systemctl restart` of the DM"
        );
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // 10 s loop: one unbounded `is-active` against a mid-restart manager eats the whole window.
        let active = crate::proc::output_within(
            Command::new("systemctl").args(["is-active", dm]),
            UNIT_STATE_BUDGET,
        )
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "active")
        .unwrap_or(false);
        if active {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    // Absent helper: plain DM restore, no black screen.
    if std::path::Path::new(OS_SESSION_SELECT).exists() {
        // Helper self-pkexecs and rewrites DM config — DM-verb budget, on the stream thread.
        // `plasma` is what Steam's own switch sends; Bazzite's helper rejects `desktop`.
        let mut select = Command::new(OS_SESSION_SELECT);
        select.arg("plasma");
        match crate::proc::status_within(&mut select, DM_VERB_BUDGET) {
            Ok(s) if s.success() => {
                // Relogin fires when the current login exits. Never mask — that start-limit-kills the DM.
                let deadline = Instant::now() + Duration::from_secs(15);
                loop {
                    if let Some(unit) = running_autologin_gamescope_unit() {
                        systemctl_user(&["stop", &unit]);
                        tracing::info!(
                            %unit,
                            "gamescope: desktop selected — stopped the game-mode session so the \
                             DM relogs into the desktop"
                        );
                        break;
                    }
                    if Instant::now() >= deadline {
                        tracing::warn!(
                            "gamescope: game-mode session never appeared after the DM restart — \
                             the desktop switch may need a manual session exit"
                        );
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
            other => tracing::warn!(
                status = ?other,
                "gamescope: os-session-select failed — leaving the box in its configured session"
            ),
        }
    } else {
        tracing::warn!(
            "gamescope: no {OS_SESSION_SELECT} on this box — restored the DM into its configured \
             session instead of switching to the desktop"
        );
    }
}

/// `(unit, active)` from `--plain` (UNIT LOAD ACTIVE …); `None` when the user manager did not
/// answer. Read as none listed, a takeover would start our Steam beside the box's booting one.
fn listed_autologin_units() -> Option<Vec<(String, String)>> {
    let out = crate::proc::output_within(
        Command::new("systemctl").args([
            "--user",
            "list-units",
            "--type=service",
            "--all",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ]),
        UNIT_QUERY_BUDGET,
    )
    .ok()
    .filter(|o| o.status.success())?;
    Some(parse_listed_units(&String::from_utf8_lossy(&out.stdout)))
}

/// Wrong ACTIVE column is silent both ways: live-as-dead collides Steam; dead-as-live idles nobody.
fn parse_listed_units(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let unit = cols.next()?;
            let active = cols.nth(1).unwrap_or("");
            (unit.starts_with("gamescope-session-plus@") && unit.ends_with(".service"))
                .then(|| (unit.to_string(), active.to_string()))
        })
        .collect()
}

/// Free Steam. Our `punktfunk-gamescope` unit is not a `@`-instance, so it is never matched.
/// SIGKILL ([`kill_unit`]) avoids the NVIDIA GPU-context leak. A failed DM stop is `Err` (caller
/// degrades to attach) — never mask-only; a mask while the DM is up is the relogin storm. So is
/// a user manager that twice does not list its units.
pub(super) fn stop_autologin_sessions() -> Result<()> {
    // Asked twice: the user manager is busiest right after a relogin.
    let listed = listed_autologin_units()
        .or_else(listed_autologin_units)
        .context("list the box's autologin game sessions")?;
    if listed.is_empty() {
        return Ok(()); // nothing autologged in — Steam is already free
    }
    let dm = display_manager_unit();
    // Negative: only `inactive`/`failed` are not-running. Listing live ones misses `deactivating`.
    let any_live = listed
        .iter()
        .any(|(_, active)| !matches!(active.as_str(), "inactive" | "failed"));
    let plan = dm_plan(dm.as_deref(), any_live);
    if plan.skip {
        return Ok(());
    }
    if takeover().idle_dropin_armed {
        return Ok(());
    }
    if plan.dm_relogins {
        install_idle_dropin().context("idling the box's autologin game session for the stream")?;
        // Arming the idle drop-in arms the honor gate; an unbaselined sentinel would read as a switch.
        record_session_select_baseline();
    }
    let units: Vec<String> = listed.into_iter().map(|(u, _)| u).collect();
    takeover().owe_units(units.clone());
    let logins_before = max_logind_session_id();
    for unit in units {
        kill_unit(&unit);
        if plan.dm_relogins {
            // Restart ourselves: drop-in is loaded, so what comes back runs nothing. Closes the
            // window where the DM sees a dead session and churns.
            systemctl_user(&["restart", &unit]);
        }
        tracing::info!(
            %unit,
            idled = plan.dm_relogins,
            "freed Steam: the box's autologin gaming session is idled for this stream (its \
             display manager stays up, so the box can still switch sessions)"
        );
    }
    watch_for_relogin_storm(logins_before);
    Ok(())
}

/// Long enough that one legitimate login racing teardown cannot trip it.
const STORM_PROBE_WINDOW: Duration = Duration::from_secs(5);

/// Healthy takeover creates 0 logins/s; a storm is 4–5. An order of magnitude clear of both.
const STORM_LOGINS_PER_SEC: f64 = 1.0;

/// Monotonic login counter: logind names `/run/systemd/sessions/` files after the id.
fn max_logind_session_id() -> Option<u64> {
    std::fs::read_dir("/run/systemd/sessions")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u64>().ok()))
        .max()
}

/// Detect-and-report only. A storm presents as a dead pad (~1.4 Hz vs 250 Hz), not as the DM;
/// every audio/input/PipeWire measurement taken during one is invalid. No self-mitigate: tearing
/// our session down if the detector is wrong is worse than the storm. `before` is read ahead of
/// the kill: once the autologin's session file is gone the max id can fall back to 1, and every
/// later login then counts as new.
fn watch_for_relogin_storm(before: Option<u64>) {
    let Some(before) = before else {
        return; // no logind — nothing relogins here
    };
    std::thread::spawn(move || {
        std::thread::sleep(STORM_PROBE_WINDOW);
        let Some(after) = max_logind_session_id() else {
            return;
        };
        let logins = after.saturating_sub(before);
        let per_sec = logins as f64 / STORM_PROBE_WINDOW.as_secs_f64();
        if per_sec < STORM_LOGINS_PER_SEC {
            return;
        }
        tracing::error!(
            logins,
            window_s = STORM_PROBE_WINDOW.as_secs(),
            rate = %format!("{per_sec:.1}/s"),
            "this box is in a display-manager RELOGIN STORM — logind is opening sessions faster \
             than once a second. Every udev consumer on the box is drowning in the fallout: \
             expect the gamepad to read at a few Hz instead of 250, WirePlumber to burn CPU \
             re-enumerating, and iio-sensor-proxy to crash-loop. NO audio, input or PipeWire \
             measurement taken now is valid — find what is relogging first. Usual cause: a \
             gamescope session unit left masked while the display manager is running, so every \
             autologin fails instantly (`systemctl --user list-unit-files 'gamescope-session*'`); \
             `systemctl --user unmask --runtime <unit>` clears it, a reboot clears it too"
        );
    });
}

/// Keep-alive reuse never calls `create_managed_session`; skip this and a reconnect inside the
/// linger window restarts autologin under the live session. A running hand-back is waited out:
/// racing past it brings Game Mode back under a fresh takeover.
pub fn cancel_pending_restore() {
    let mut t = takeover();
    let mut told = false;
    loop {
        match t.hand_back.cancel() {
            Cancel::Nothing => return,
            Cancel::Cancelled => {
                tracing::info!(
                    "gamescope: client (re)connected — cancelled the pending TV-session restore"
                );
                return;
            }
            Cancel::Wait => {
                if !std::mem::replace(&mut told, true) {
                    tracing::info!(
                        "gamescope: a TV-session restore is in flight — the (re)connect waits for \
                         it, then takes the restored session over from scratch"
                    );
                }
                t = wait(t);
            }
        }
    }
}

/// Same linger policy as pooled backends, `owner`'s overlay included. Unconfigured →
/// [`RESTORE_DEBOUNCE`]. Forever → `None`.
fn restore_delay(owner: Option<[u8; 32]>) -> Option<Duration> {
    use crate::policy::{self, Linger};
    let fp = policy::fp_hex(owner);
    match policy::prefs()
        .configured()
        .map(|p| p.effective_for(fp.as_deref()).keep_alive.linger())
    {
        Some(Linger::Immediate) => Some(Duration::from_secs(0)),
        Some(Linger::For(d)) => Some(d),
        Some(Linger::Forever) => None,
        None => Some(RESTORE_DEBOUNCE),
    }
}

/// The hand-back's delay: the keep-alive's, `None` (forever) keeping the box. A keep-alive keeps
/// a session; a failed launch left none, so its box goes back after the debounce at most.
fn hand_back_delay(keep_alive: Option<Duration>, launch_failed: bool) -> Option<Duration> {
    if launch_failed {
        Some(keep_alive.map_or(RESTORE_DEBOUNCE, |d| d.min(RESTORE_DEBOUNCE)))
    } else {
        keep_alive
    }
}

/// Debounced restore so a reconnect reuses the warm session, on the keep-alive's delay
/// (`forever` schedules none), or the debounce after a launch that produced no session.
pub fn schedule_restore_tv_session() {
    let owner = takeover().owner;
    let keep_alive = restore_delay(owner);
    let mut t = takeover();
    if !t.live() {
        return; // nothing was taken over → nothing to restore (also the non-managed path)
    }
    let delay = hand_back_delay(keep_alive, t.launch_failed);
    t.hand_back.schedule(delay.map(|d| Instant::now() + d));
    match delay {
        None => tracing::info!(
            "gamescope: keep-alive=forever — managed session held (no TV-restore scheduled; \
             return to gaming mode or restart the host to free it)"
        ),
        Some(delay) => tracing::info!(
            secs = delay.as_secs(),
            launch_failed = t.launch_failed,
            "gamescope: scheduled TV-session restore (keep-alive policy; cancelled on reconnect)"
        ),
    }
}

/// A launch that took the box failed: hand it back after the debounce at most.
pub(super) fn hand_back_after_failed_launch() {
    takeover().launch_failed = true;
    schedule_restore_tv_session();
}

/// Run one create and record what it left. A session clears [`Takeover::launch_failed`]; a
/// failure that took a box no takeover held sets it. A refused join takes nothing.
pub(super) fn launch_tracked<T>(create: impl FnOnce() -> Result<T>) -> Result<T> {
    let fresh = !takeover_live();
    let out = create();
    let mut t = takeover();
    if out.is_ok() {
        t.launch_failed = false;
    } else if fresh && t.live() {
        t.launch_failed = true;
    }
    out
}

/// A takeover the host still holds, as the console lists it.
pub struct HeldTakeover {
    /// The managed session's `(width, height, refresh_hz)`, when one is tracked.
    pub mode: Option<(u32, u32, u32)>,
    /// Time left before the hand-back runs; `None` = held until released.
    pub restore_in: Option<Duration>,
}

/// What is held now. Also `Some` mid-session; the caller knows whether one is live.
pub fn held_takeover() -> Option<HeldTakeover> {
    let t = takeover();
    if !t.live() {
        return None;
    }
    Some(HeldTakeover {
        mode: t
            .managed
            .as_ref()
            .map(|s| (s.width, s.height, s.refresh_hz)),
        restore_in: t.hand_back.due_in(Instant::now()),
    })
}

/// Console Release: hand the box's own session back now, whatever the keep-alive. The restore
/// worker runs it, so a reconnect still cancels it. `false` = nothing held.
pub fn release_takeover() -> bool {
    let mut t = takeover();
    if !t.live() {
        return false;
    }
    t.hand_back.schedule(Some(Instant::now()));
    tracing::info!("gamescope: released from the console — handing the box's own session back");
    true
}

/// True while anything taken over is still ours to hand back.
fn takeover_live() -> bool {
    takeover().live()
}

/// Synchronous: the host is exiting and a live takeover must not outlive it. Ignores keep-alive
/// (`forever` is for the next client). A running hand-back is waited out first, so this finds
/// nothing live after it: one hand-back, not two interleaved.
pub fn restore_takeover_now() {
    {
        let mut t = takeover();
        while t.hand_back.running() {
            t = wait(t);
        }
        if !t.live() {
            return;
        }
        t.hand_back = HandBack::Running { then: None };
    }
    let _end = HandBackRun;
    tracing::info!("gamescope: host is shutting down — restoring the box's own session first");
    // `verify: false`: the ladder waits up to a minute; shutdown grace is 20 s then `exit(0)`.
    do_restore_tv_session(false);
}

/// What a bounded `systemctl --user` lifecycle verb on the restore path actually did. Three states:
/// the log line is the only thing an operator sees about a box that may or may not have its screen back.
pub(super) enum RestoreVerb {
    Done,
    /// Budget expired: `status_within` kills the systemctl client, not the queued job.
    StillRunning,
    /// systemd said no, or the helper could not be spawned. The only outcome an operator acts on.
    Failed(String),
}

/// Every hand-back verb: what it runs ([`Site::args`]) and what the operator reads of how it went
/// ([`report`]).
#[derive(Clone, Copy)]
enum Site<'a> {
    /// The SteamOS hand-back's restart of [`STEAMOS_SESSION_TARGET`].
    SteamosTarget,
    /// The session-plus hand-back's restart of an autologin unit.
    Autologin(&'a str),
    /// An autologin unit a dead host left idled, restarted at startup.
    Idled(&'a str),
    /// The escalation's first rung: stop the unit so the display manager relogins.
    Escalate(&'a str),
    /// The forced `SCREEN_*` unset.
    ScreenEnv,
    /// One owed unit's runtime unmask.
    Unmask(&'a str),
}

impl<'a> Site<'a> {
    fn args(self) -> Vec<&'a str> {
        match self {
            Site::SteamosTarget => vec!["restart", STEAMOS_SESSION_TARGET],
            // `restart`, not `start`: the idle takeover leaves the unit ACTIVE, and `start` on an
            // active unit is a no-op that would report success over a session still running nothing.
            Site::Autologin(unit) | Site::Idled(unit) => vec!["restart", unit],
            Site::Escalate(unit) => vec!["stop", unit],
            Site::ScreenEnv => vec![
                "unset-environment",
                "SCREEN_WIDTH",
                "SCREEN_HEIGHT",
                "CUSTOM_REFRESH_RATES",
            ],
            Site::Unmask(unit) => vec!["unmask", "--runtime", unit],
        }
    }
}

/// Run `site`'s verb and log how it went. Timeout is StillRunning, not Failed: a `restart` of
/// `gamescope-session.target` blocks on Steam tearing a game down, which routinely exceeds
/// [`UNIT_VERB_BUDGET`]. The bound stays — shutdown grace is 20 s, and an unbounded verb costs
/// the DM restore that follows.
fn restore_verb(site: Site<'_>) -> RestoreVerb {
    let verb = issue_verb_within(&site.args(), UNIT_VERB_BUDGET);
    report(site, &verb);
    verb
}

/// The operator's line for each hand-back verb's outcome, silence included.
fn report(site: Site<'_>, verb: &RestoreVerb) {
    use {RestoreVerb as V, Site as S};
    let secs = UNIT_VERB_BUDGET.as_secs();
    match (site, verb) {
        (S::SteamosTarget, V::Done) => tracing::info!(
            "gamescope (SteamOS): restored the physical gaming session (removed headless override)"
        ),
        (S::SteamosTarget, V::StillRunning) => tracing::info!(
            "gamescope (SteamOS): the {STEAMOS_SESSION_TARGET} restart is still running after \
             {secs}s (Steam closing a game is the usual reason) — systemd owns the job from here; \
             the panel comes back when it completes"
        ),
        (S::SteamosTarget, V::Failed(why)) => tracing::error!(
            status = %why,
            "gamescope (SteamOS): could not restart {STEAMOS_SESSION_TARGET} — the Deck's panel \
             stays dark until someone runs `systemctl --user restart {STEAMOS_SESSION_TARGET}` \
             (the headless override is already removed, so that restart is all it needs)"
        ),
        (S::Autologin(unit), V::Done) => tracing::info!(
            unit,
            "restored the TV's autologin gaming session (debounce elapsed, no client)"
        ),
        // A `--user start` of a gamescope-session-plus unit waits for its Exec to signal, and
        // Steam's own start routinely exceeds the bound. Queued is not failed.
        (S::Autologin(unit), V::StillRunning) => tracing::info!(
            unit,
            "the TV's autologin gaming session is still starting after {secs}s — systemd owns the \
             job from here"
        ),
        (S::Autologin(unit), V::Failed(why)) => tracing::error!(
            unit,
            status = %why,
            "could not restart the TV's autologin gaming session — the box is left out of game \
             mode until someone runs `systemctl --user start {unit}` (a masked unit or a tripped \
             start limit are the usual causes: \
             `systemctl --user unmask --runtime {unit} && systemctl --user reset-failed {unit}`)"
        ),
        (S::Idled(unit), V::Failed(why)) => {
            tracing::error!(unit, status = %why, "gamescope: not restarted")
        }
        (S::Escalate(unit), V::Failed(why)) => {
            tracing::warn!(unit, status = %why, "gamescope: autologin unit not stopped")
        }
        (S::ScreenEnv, V::Done) => tracing::info!(
            "gamescope: unset the forced SCREEN_WIDTH/SCREEN_HEIGHT/CUSTOM_REFRESH_RATES — the \
             box's own game mode is back on its own resolution"
        ),
        (S::ScreenEnv, V::StillRunning) => tracing::warn!(
            secs,
            "gamescope: the forced SCREEN_* unset is still running — owed to the next hand-back"
        ),
        (S::ScreenEnv, V::Failed(why)) => tracing::warn!(
            status = %why,
            "gamescope: the forced SCREEN_* not unset — owed to the next hand-back"
        ),
        (S::Unmask(unit), V::StillRunning) => {
            tracing::warn!(unit, "gamescope: the unmask is still running — owed")
        }
        (S::Unmask(unit), V::Failed(why)) => {
            tracing::warn!(unit, status = %why, "gamescope: runtime mask not lifted — owed")
        }
        (S::Idled(_) | S::Escalate(_), _) | (S::Unmask(_), V::Done) => {}
    }
}

/// `systemctl --user <args>` under `budget`, read as a [`RestoreVerb`].
pub(super) fn issue_verb_within(args: &[&str], budget: Duration) -> RestoreVerb {
    match crate::proc::status_within(Command::new("systemctl").arg("--user").args(args), budget) {
        Ok(s) if s.success() => RestoreVerb::Done,
        Ok(s) => RestoreVerb::Failed(format!("systemctl exited with {s}")),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => RestoreVerb::StillRunning,
        Err(e) => RestoreVerb::Failed(format!("run systemctl: {e}")),
    }
}

/// Errors read as headless: keep the working session rather than restore to a panel that isn't there.
pub(super) fn physical_display_connected() -> bool {
    connected_connector_under(std::path::Path::new("/sys/class/drm"))
}

fn connected_connector_under(base: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(base) else {
        return false;
    };
    entries.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "connected")
    })
}

/// How long a hand-back waits for the box to show something on its own panel before it starts
/// escalating. The unit's `ExecStart` is a whole gamescope + Steam start; a false escalation
/// costs a session bounce.
const HANDBACK_GRACE: Duration = Duration::from_secs(25);

/// How long each escalation rung gets. Shorter than [`HANDBACK_GRACE`]: by the time a rung runs,
/// the ordinary start has already had its full grace and not delivered.
const HANDBACK_RUNG_GRACE: Duration = Duration::from_secs(15);

const HANDBACK_POLL: Duration = Duration::from_millis(500);

/// Only sound after `stop_session(SESSION_UNIT)`: that SIGKILL is synchronous, so our gamescope
/// cannot still be answering for the box.
fn box_session_live() -> bool {
    crate::detect_active_session().kind != crate::ActiveKind::None
}

/// Poll [`box_session_live`] until it is true or `grace` runs out. [`HandbackWait::Superseded`]
/// means a client reconnected and took the box over again — the hand-back we were checking is moot,
/// and every remedy below would now be fighting a live stream for the box's session.
enum HandbackWait {
    Live,
    Superseded,
    TimedOut,
}

fn wait_for_box_session(grace: Duration) -> HandbackWait {
    let deadline = Instant::now() + grace;
    loop {
        if takeover_live() {
            return HandbackWait::Superseded;
        }
        if box_session_live() {
            return HandbackWait::Live;
        }
        if Instant::now() >= deadline {
            return HandbackWait::TimedOut;
        }
        std::thread::sleep(HANDBACK_POLL);
    }
}

/// After [`HANDBACK_GRACE`], if nothing is drawing: `stop` the autologin (releases the parked
/// `--wait start`; `restart` does not), then restart the DM, then `PUNKTFUNK_RECOVER_SESSION_CMD`.
/// Detached: a hand-back running for a minute would block every reconnect. Call once the units
/// are settled, or the first poll reads our own finished takeover as a new one.
fn ensure_box_session_or_escalate(units: &[String]) {
    let units: Vec<String> = units.to_vec();
    std::thread::spawn(move || handback_watch(&units));
}

fn handback_watch(units: &[String]) {
    match wait_for_box_session(HANDBACK_GRACE) {
        HandbackWait::Live => {
            tracing::info!(
                "gamescope: the box is driving its own panel again — hand-back complete"
            );
            return;
        }
        HandbackWait::Superseded => return,
        HandbackWait::TimedOut => {}
    }
    tracing::warn!(
        secs = HANDBACK_GRACE.as_secs(),
        units = ?units,
        "gamescope: NOTHING is driving the box's panel {}s after the hand-back — its screen is \
         dark. Escalating: stopping the autologin unit so the display manager relogins into a \
         session with a seat",
        HANDBACK_GRACE.as_secs()
    );
    // Rung 1: release the login session's parked `--wait start` and let the DM relogin.
    for unit in units {
        restore_verb(Site::Escalate(unit));
    }
    match wait_for_box_session(HANDBACK_RUNG_GRACE) {
        HandbackWait::Live => {
            tracing::info!(
                "gamescope: the display manager relogged the box into its own session — panel back"
            );
            return;
        }
        HandbackWait::Superseded => return,
        HandbackWait::TimedOut => {}
    }
    // Rung 2: restart the display manager.
    if let Some(dm) = display_manager_unit() {
        tracing::warn!(
            %dm,
            "gamescope: the box is still dark — restarting its display manager"
        );
        match restore_display_manager(&dm) {
            Ok(()) => match wait_for_box_session(HANDBACK_RUNG_GRACE) {
                HandbackWait::Live => {
                    tracing::info!(%dm, "gamescope: the display manager brought the box back");
                    return;
                }
                HandbackWait::Superseded => return,
                HandbackWait::TimedOut => {}
            },
            Err(why) => tracing::warn!(
                %dm,
                shape = why.shape(),
                reason = %why,
                "gamescope: display manager not restarted"
            ),
        }
    }
    // Rung 3: operator escape hatch, then say what is left to do by hand.
    if crate::try_recover_session() {
        tracing::warn!(
            "gamescope: fired PUNKTFUNK_RECOVER_SESSION_CMD to bring the box's session back"
        );
        return;
    }
    tracing::error!(
        units = ?units,
        "gamescope: the box has NO session driving its panel and every automatic remedy failed — \
         its screen stays dark until someone runs `systemctl --user restart <unit>` for one of \
         these, or `sudo systemctl restart display-manager.service`. Set \
         PUNKTFUNK_RECOVER_SESSION_CMD to let the host do this itself"
    );
}

/// Hand back what the takeover owes. Each step leaves the journal only once its undo is issued:
/// a host killed before that leaves it for the next. A box already on its desktop keeps it, and
/// gets back the desktop Steam our session swallowed. Only units still idled are restarted, and
/// none while no panel is connected. `verify` is false on the shutdown restore alone.
fn do_restore_tv_session(verify: bool) {
    takeover().generation += 1;
    // Only managed `exclusive` holds one. Above every early return. Idempotent.
    managed_darken_release();
    if hand_back_steamos(verify) {
        return;
    }
    // Before taking the units (it reads them) and before any early return.
    lift_autologin_mask();
    // No panel: the box's Game Mode would crash-loop gamescope into a forced desktop. Keep it
    // idled and ours running, and look again: a TV in standby reports disconnected.
    if verify && !physical_display_connected() {
        let mut t = takeover();
        if !t.units().is_empty() {
            tracing::debug!(
                secs = NO_PANEL_RECHECK.as_secs(),
                "gamescope: no physical display connected — keeping the box idled and checking \
                 again"
            );
            t.hand_back
                .schedule(Some(Instant::now() + NO_PANEL_RECHECK));
            return;
        }
    }
    // Taken in one short hold: nothing below runs under the takeover lock. They stay journaled
    // until `settle_taken` runs.
    let taken = {
        let mut t = takeover();
        t.managed = None;
        t.take(|s| {
            matches!(
                s,
                TakeoverStep::RestartAutologin { .. }
                    | TakeoverStep::StartDisplayManager { .. }
                    | TakeoverStep::StopSession
            )
        })
    };
    let settle_taken = || taken.iter().for_each(|o| journal::done(o.seq));
    let HandBackPlan { session, units, dm } = hand_back_plan(taken.iter().map(|o| &o.step));
    if units.is_empty() && dm.is_none() {
        if session {
            stop_our_session(verify);
            tracing::info!(
                "gamescope: stopped the idle managed session (nothing was taken over — no box \
                 session to restore)"
            );
        }
        // Attach re-mode leaves the bind drop-in and SCREEN_*. Undo without bouncing Game Mode.
        disarm_session_plus_dropin();
        unset_forced_session_screen_env();
        settle_taken();
        return;
    }
    // Our gamescope/Steam session, so Steam is free for the autologin.
    let our_steam = stop_our_session(verify);

    // Before every early return: a leftover bind puts our gamescope under ordinary Game Mode;
    // a leftover idle drop-in leaves Game Mode as a sleep.
    disarm_session_plus_dropin();
    if remove_idle_dropin() {
        tracing::info!(
            "gamescope: removed the takeover's idle drop-in — the box's own Game Mode runs for \
             real again"
        );
    }
    unset_forced_session_screen_env();
    use crate::ActiveKind;
    if matches!(
        crate::detect_active_session().kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
            | ActiveKind::DesktopNiri
            | ActiveKind::DesktopUmbriel
    ) {
        tracing::info!(
            "gamescope: a desktop session is active — not restoring the TV gaming session"
        );
        if our_steam {
            restart_swallowed_desktop_steam();
        }
        settle_taken();
        return;
    }
    // DM-stop takeover ([`dm_plan`]): restore the DM (`reset-failed` then `restart`); its
    // autologin Exec brings gaming mode back. Do not `--user start` the unit: without a DM
    // login there is no seat, so gamescope never gets DRM master and the unit goes `failed`.
    if let Some(dm) = dm {
        match restore_display_manager(&dm) {
            Ok(()) => {
                tracing::info!(%dm, "restored the display manager (its autologin brings gaming mode back)")
            }
            Err(why) if crate::try_recover_session() => tracing::warn!(
                %dm,
                shape = why.shape(),
                reason = %why,
                "display-manager restart lost its privilege — fired PUNKTFUNK_RECOVER_SESSION_CMD \
                 to bring the session back"
            ),
            // No graphical session. The helper's own reason rides along: the two root commands
            // fix the symptom once, and the reason is what stops it happening again.
            Err(why) => tracing::error!(
                %dm,
                shape = why.shape(),
                reason = %why,
                "could not restart the display manager and no PUNKTFUNK_RECOVER_SESSION_CMD is \
                 configured — the box has no graphical session until someone runs \
                 `systemctl reset-failed {dm} && systemctl restart {dm}` as root"
            ),
        }
        // LAST, not first: the journal is the only thing that heals a box whose DM is down after
        // this process dies, and the steps above are unbounded work on a 20 s shutdown grace.
        settle_taken();
        return;
    }
    // Idle drop-in already gone (removed above every early return), so these restarts bring
    // the box's real session back rather than another idle one. A unit the box stopped for a
    // switch, or already runs for real, is left as it is.
    for unit in &units {
        if runs_idle_placeholder(unit) {
            restore_verb(Site::Autologin(unit));
        } else {
            tracing::info!(
                %unit,
                "gamescope: the box's game session no longer runs our idle placeholder — left as \
                 the box has it"
            );
        }
    }
    settle_taken(); // only now, with the restarts actually issued
    if verify {
        ensure_box_session_or_escalate(&units);
    }
}

/// The SteamOS hand-back: shrink the drop-in to its kill signal and restart the target, unless a
/// desktop is already up. `false` when no SteamOS step is owed. The takeover lock spans it, so a
/// SteamOS launch waits for the restart.
fn hand_back_steamos(verify: bool) -> bool {
    let mut t = takeover();
    if !t.owes(|s| matches!(s, TakeoverStep::RestoreSteamos)) {
        return false;
    }
    // No panel: restarting the target crash-loops gamescope. Keep headless and look again
    // later: a TV in standby reports disconnected until it is switched on.
    if !physical_display_connected() {
        tracing::debug!(
            secs = NO_PANEL_RECHECK.as_secs(),
            "gamescope (SteamOS): no physical display connected — keeping the headless session \
             and checking again"
        );
        t.hand_back
            .schedule(Some(Instant::now() + NO_PANEL_RECHECK));
        return true;
    }
    t.managed = None;
    // The restart below stops our headless gamescope; it must go by SIGKILL too.
    if write_steamos_handback_dropin().is_err() {
        remove_steamos_dropin();
    }
    systemctl_user(&["daemon-reload"]);
    use crate::ActiveKind;
    if matches!(
        crate::detect_active_session().kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
            | ActiveKind::DesktopNiri
            | ActiveKind::DesktopUmbriel
    ) {
        if remove_steamos_dropin() {
            systemctl_user(&["daemon-reload"]);
        }
        tracing::info!(
            "gamescope (SteamOS): a desktop session is active — removed the headless override, \
             not restarting the gaming session"
        );
        t.settle(|s| matches!(s, TakeoverStep::RestoreSteamos));
        return true;
    }
    // Our headless Steam may be under 60 s old; its stop must not count as a failure.
    forget_host_short_sessions();
    let restarted = restore_verb(Site::SteamosTarget);
    forget_host_short_sessions();
    // A restart still running has yet to stop gamescope; its kill-signal drop-in stays until the
    // next takeover or hand-back, or the runtime dir goes.
    if matches!(restarted, RestoreVerb::Done) && remove_steamos_dropin() {
        systemctl_user(&["daemon-reload"]);
    }
    t.settle(|s| matches!(s, TakeoverStep::RestoreSteamos)); // after the restart
    if verify {
        ensure_box_session_or_escalate(&[STEAMOS_SESSION_TARGET.to_string()]);
    }
    true
}

/// Drop the returned handle to stop the worker.
pub fn start_restore_worker() -> std::sync::Arc<()> {
    let handle = std::sync::Arc::new(());
    let weak = std::sync::Arc::downgrade(&handle);
    if let Err(e) = std::thread::Builder::new()
        .name("punktfunk-restore-worker".into())
        .spawn(move || {
            while weak.upgrade().is_some() {
                std::thread::sleep(Duration::from_millis(100));
                // Due and running in one step, so a connect never slips between them.
                if takeover().hand_back.pop(Instant::now()) {
                    let _end = HandBackRun;
                    do_restore_tv_session(true);
                }
            }
        })
    {
        tracing::error!(error = %e, "restore-worker spawn failed — TV session won't auto-restore on idle");
    }
    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owed(steps: &[TakeoverStep]) -> Takeover {
        Takeover {
            owed: (0u64..)
                .zip(steps)
                .map(|(seq, step)| Owed {
                    seq,
                    step: step.clone(),
                })
                .collect(),
            ..Takeover::EMPTY
        }
    }

    fn units(names: &[&str]) -> TakeoverStep {
        TakeoverStep::RestartAutologin {
            units: names.iter().map(|n| n.to_string()).collect(),
        }
    }

    /// Each field of an older host's record alone is a takeover to hand back; a record without
    /// the newer fields still parses (refusing the file is refusing the restore).
    #[test]
    fn a_pre_journal_record_becomes_the_steps_it_names() {
        let steps = |json: &str| {
            serde_json::from_str::<LegacyRecord>(json)
                .expect("an older record parses")
                .steps()
        };
        let unit = "gamescope-session-plus@steam.service";
        assert!(steps(r#"{"stopped_autologin":[],"steamos":false}"#).is_empty());
        assert_eq!(
            steps(&format!(
                r#"{{"stopped_autologin":["{unit}"],"steamos":false}}"#
            )),
            [units(&[unit])]
        );
        assert_eq!(
            steps(r#"{"stopped_autologin":[],"steamos":false,"managed_session":true}"#),
            [TakeoverStep::StopSession]
        );
        // SteamOS's managed session is its own target: one step restarts it.
        assert_eq!(
            steps(r#"{"stopped_autologin":[],"steamos":true,"managed_session":true}"#),
            [TakeoverStep::RestoreSteamos]
        );
        assert_eq!(
            steps(r#"{"stopped_autologin":[],"steamos":false,"stopped_dm":"sddm.service"}"#),
            [TakeoverStep::StartDisplayManager {
                unit: "sddm.service".into()
            }]
        );
        assert_eq!(
            steps(r#"{"stopped_autologin":[],"steamos":false,"forced_screen_env":true}"#),
            [TakeoverStep::UnsetScreenEnv]
        );
    }

    /// Live is exactly the owed steps, the bind drop-in among them: a step the journal does not
    /// hold is one a crash forgets.
    #[test]
    fn every_owed_step_is_live_the_bind_dropin_included() {
        assert!(!Takeover::EMPTY.live());
        assert!(owed(&[TakeoverStep::RemoveSessionDropin]).live());
        assert!(owed(&[TakeoverStep::StopSession]).live());
        let mode_alone = Takeover {
            managed: Some(SessionState {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                hdr: false,
            }),
            ..Takeover::EMPTY
        };
        assert!(!mode_alone.live(), "a mode is not a step");
    }

    #[test]
    fn session_select_sentinel_needs_a_baseline() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let t1 = t0 + std::time::Duration::from_secs(1);
        // Never baselined: the sentinel is permanent; an ancient write is not a live request.
        assert!(!sentinel_advanced(Baseline::Unset, Some(t0)));
        assert!(!sentinel_advanced(Baseline::Unset, None));
        // Baselined with no sentinel yet, then one appeared inside the session: a real request.
        assert!(sentinel_advanced(Baseline::Absent, Some(t0)));
        assert!(!sentinel_advanced(Baseline::Absent, None));
        // Baselined at an mtime: only a newer one is the user's in-stream switch. The write that
        // brought the box into game mode is the baseline itself, so it reads as no request.
        assert!(sentinel_advanced(Baseline::At(t0), Some(t1)));
        assert!(!sentinel_advanced(Baseline::At(t0), Some(t0)));
        assert!(!sentinel_advanced(Baseline::At(t1), Some(t0)));
        assert!(!sentinel_advanced(Baseline::At(t0), None));
    }

    /// The hand-back's own table: each trigger in each phase.
    #[test]
    fn the_hand_back_follows_its_table() {
        use HandBack::{Due, Idle, Running};
        let now = Instant::now();
        let at = now + Duration::from_secs(5);
        let later = now + Duration::from_secs(30);
        // Last session out: hand back at `at`, or keep the box (keep-alive forever).
        let mut h = Idle;
        h.schedule(Some(at));
        assert_eq!(h, Due(at));
        let mut h = Idle;
        h.schedule(None);
        assert_eq!(h, Idle);
        // A client returns before `at`; one with nothing pending changes nothing.
        let mut h = Due(at);
        assert_eq!(h.cancel(), Cancel::Cancelled);
        assert_eq!(h, Idle);
        assert_eq!(h.cancel(), Cancel::Nothing);
        // The worker's tick: nothing before `at`, then it runs.
        let mut h = Due(at);
        assert!(!h.pop(now));
        assert!(h.pop(at));
        assert_eq!(h, Running { then: None });
        // While it runs a connect waits, and a new deadline waits its turn: never two at once.
        assert_eq!(h.cancel(), Cancel::Wait);
        h.schedule(Some(later));
        assert_eq!(h, Running { then: Some(later) });
        assert!(!h.pop(later));
        // SteamOS with no panel, or a disconnect meanwhile: due again once it ends.
        h.finish();
        assert_eq!(h, Due(later));
        let mut h = Running { then: Some(at) };
        h.schedule(None);
        h.finish();
        assert_eq!(h, Idle, "keep-alive forever drops the follow-up");
        // Console Release: due now.
        let mut h = Idle;
        h.schedule(Some(now));
        assert!(h.pop(now));
        // What the console shows.
        assert_eq!(Due(at).due_in(now), Some(Duration::from_secs(5)));
        assert_eq!(
            Running { then: Some(at) }.due_in(now),
            Some(Duration::from_secs(5))
        );
        assert_eq!(Running { then: None }.due_in(now), None);
        assert_eq!(Idle.due_in(now), None);
    }

    /// Keep-alive decides the hand-back of a session. A failed launch left none, so forever and
    /// a timed keep-alive alike hand its box back after the debounce; a shorter one stays.
    #[test]
    fn keep_alive_keeps_a_session_not_a_failed_launch() {
        let linger = Some(Duration::from_secs(600));
        assert_eq!(hand_back_delay(None, false), None);
        assert_eq!(hand_back_delay(None, true), Some(RESTORE_DEBOUNCE));
        assert_eq!(hand_back_delay(linger, false), linger);
        assert_eq!(hand_back_delay(linger, true), Some(RESTORE_DEBOUNCE));
        assert_eq!(
            hand_back_delay(Some(Duration::ZERO), true),
            Some(Duration::ZERO)
        );
    }

    /// The device that took the box decides its keep-alive, as a device decides for its pooled
    /// display. Any other falls back to the host's. Runs in a child: the console's policy is a
    /// process global.
    #[test]
    fn the_owning_device_times_the_hand_back() {
        let name = "the_owning_device_times_the_hand_back";
        crate::fake::in_child(module_path!(), name, &[], || {
            let tablet = [0xaa; 32];
            let doc = format!(
                r#"{{ "keep_alive": {{ "mode": "forever" }}, "clients": {{ "{}":
                    {{ "keep_alive": {{ "mode": "duration", "seconds": 30 }} }} }} }}"#,
                hex::encode(tablet)
            );
            let policy = pf_paths::config_dir().join("display-settings.json");
            std::fs::write(policy, doc).expect("write the policy");
            assert_eq!(restore_delay(Some(tablet)), Some(Duration::from_secs(30)));
            assert_eq!(restore_delay(Some([0xbb; 32])), None);
            assert_eq!(restore_delay(None), None);
        });
    }

    /// Under keep-alive forever a launch that took the box and produced no session hands it
    /// back after the debounce; a session keeps it, and a refused join changes nothing. Runs in
    /// a child: the takeover, the journal and the console's policy are process globals.
    #[test]
    fn forever_hands_back_a_box_a_failed_launch_left_dark() {
        let name = "forever_hands_back_a_box_a_failed_launch_left_dark";
        crate::fake::in_child(module_path!(), name, &[], || {
            let forever = r#"{ "keep_alive": { "mode": "forever" } }"#;
            let policy = pf_paths::config_dir().join("display-settings.json");
            std::fs::write(policy, forever).expect("write the policy");
            let due = || takeover().hand_back.due_in(Instant::now());
            let launch = |ok: bool| {
                launch_tracked(|| {
                    takeover().owe_units(vec![UNIT.into()]);
                    ok.then_some(()).ok_or_else(|| anyhow!("no node"))
                })
            };
            // The launch frees the box and fails; the session's last hold then schedules.
            assert!(launch(false).is_err());
            schedule_restore_tv_session();
            assert!(due().is_some_and(|d| d <= RESTORE_DEBOUNCE), "{:?}", due());
            // The client returns and its launch streams: forever keeps the box.
            cancel_pending_restore();
            assert!(launch(true).is_ok());
            schedule_restore_tv_session();
            assert_eq!(due(), None);
            assert!(launch_tracked(|| Err::<(), _>(anyhow!("join refused"))).is_err());
            schedule_restore_tv_session();
            assert_eq!(due(), None, "a refused join took nothing");
            // A managed relaunch on the taken box fails.
            hand_back_after_failed_launch();
            assert!(due().is_some_and(|d| d <= RESTORE_DEBOUNCE), "{:?}", due());
        });
    }

    /// What each state of the table gets back from the session-plus hand-back. SteamOS has its
    /// own branch, and SCREEN_* and the bind settle themselves, so neither is in the plan.
    #[test]
    fn each_state_hands_back_what_it_took() {
        let unit = "gamescope-session-plus@steam.service";
        let plan = |steps: &[TakeoverStep]| hand_back_plan(steps);
        let units_only = HandBackPlan {
            session: false,
            units: vec![unit.into()],
            dm: None,
        };
        assert_eq!(plan(&[units(&[unit])]), units_only, "spawn, Freed");
        assert_eq!(
            plan(&[units(&[unit]), TakeoverStep::StopSession]),
            HandBackPlan {
                session: true,
                ..units_only
            },
            "managed, Running"
        );
        assert_eq!(
            plan(&[TakeoverStep::StopSession]),
            HandBackPlan {
                session: true,
                ..HandBackPlan::default()
            },
            "managed on a box whose Game Mode was down"
        );
        let remoded = [
            TakeoverStep::UnsetScreenEnv,
            TakeoverStep::RemoveSessionDropin,
        ];
        assert_eq!(plan(&remoded), HandBackPlan::default(), "attach, Remoded");
        assert_eq!(
            plan(&[TakeoverStep::RestoreSteamos]),
            HandBackPlan::default(),
            "SteamOS"
        );
        let dm = TakeoverStep::StartDisplayManager {
            unit: "sddm.service".into(),
        };
        assert_eq!(
            plan(&[units(&[unit]), dm]).dm.as_deref(),
            Some("sddm.service"),
            "an older host's DM stop: the DM comes back, its autologin brings the units"
        );
        assert_eq!(
            plan(&[units(&["a", "b"]), units(&["b", "c"])]).units,
            ["a", "b", "c"],
            "adopted beside our own, each unit restarts once"
        );
    }

    fn sorted(steps: impl IntoIterator<Item = TakeoverStep>) -> Vec<String> {
        let mut v: Vec<String> = steps.into_iter().map(|s| format!("{s:?}")).collect();
        v.sort();
        v
    }

    fn journal_files() -> usize {
        std::fs::read_dir(pf_paths::state_dir().join("topology-journal")).map_or(0, |d| d.count())
    }

    const UNIT: &str = "gamescope-session-plus@steam.service";

    /// A takeover state as the live code leaves it: the steps it took, in its order.
    type Took = fn(&mut Takeover);

    /// A host killed in each state of the table, then started again: the next one owes exactly
    /// what the dead one did, hands it back after the grace, and sweeps the drop-ins it does not
    /// owe. Runs in a child: the takeover and the journal are process globals.
    #[test]
    fn a_crash_in_every_state_is_handed_back_by_the_next_host() {
        let name = "a_crash_in_every_state_is_handed_back_by_the_next_host";
        crate::fake::in_child(module_path!(), name, &[], || {
            let rows: [(&str, Took, Vec<TakeoverStep>); 9] = [
                ("Free", |_| {}, vec![]),
                (
                    "spawn, Freed",
                    |t| t.owe_units(vec![UNIT.into()]),
                    vec![units(&[UNIT])],
                ),
                (
                    "managed, Running",
                    |t| {
                        t.owe_units(vec![UNIT.into()]);
                        t.owe_once(TakeoverStep::StopSession);
                    },
                    vec![units(&[UNIT]), TakeoverStep::StopSession],
                ),
                (
                    "managed on a box whose Game Mode was down",
                    |t| {
                        t.owe_once(TakeoverStep::StopSession);
                    },
                    vec![TakeoverStep::StopSession],
                ),
                (
                    "SteamOS, Running",
                    |t| {
                        t.owe_once(TakeoverStep::RestoreSteamos);
                    },
                    vec![TakeoverStep::RestoreSteamos],
                ),
                (
                    "attach, Remoded: the startup sweep removes the bind",
                    |t| {
                        t.owe_once(TakeoverStep::UnsetScreenEnv);
                        t.owe_once(TakeoverStep::RemoveSessionDropin);
                    },
                    vec![TakeoverStep::UnsetScreenEnv],
                ),
                (
                    "SwitchHonored with SCREEN_* still owed",
                    |t| {
                        t.owe_units(vec![UNIT.into()]);
                        t.owe_once(TakeoverStep::StopSession);
                        t.owe_once(TakeoverStep::UnsetScreenEnv);
                        t.owe_units(Vec::new());
                        t.settle(|s| matches!(s, TakeoverStep::StopSession));
                    },
                    vec![TakeoverStep::UnsetScreenEnv],
                ),
                (
                    "HandingBack: taken, its restarts not yet issued",
                    |t| {
                        t.owe_units(vec![UNIT.into()]);
                        t.owe_once(TakeoverStep::StopSession);
                        drop(t.take(|_| true));
                    },
                    vec![units(&[UNIT]), TakeoverStep::StopSession],
                ),
                (
                    "a takeover from another boot",
                    |_| {
                        journal::push(
                            "gamescope",
                            journal::Undo::Takeover {
                                boot: "a boot that is over".into(),
                                step: units(&[UNIT]),
                            },
                        );
                    },
                    vec![],
                ),
            ];
            for (state, took, want) in rows {
                took(&mut takeover());
                journal::crash();
                *takeover() = Takeover::EMPTY;
                for path in [steamos_dropin_path(), session_plus_dropin_path()] {
                    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                    std::fs::write(&path, "[Service]\n").unwrap();
                }
                let before = Instant::now();
                journal::reclaim(&crate::backend_by_id);
                restore_takeover_on_startup();

                let t = takeover();
                assert_eq!(
                    sorted(t.owed.iter().map(|o| o.step.clone())),
                    sorted(want.clone()),
                    "{state}"
                );
                match t.hand_back {
                    HandBack::Due(at) => assert!(
                        !want.is_empty() && at > before && at <= Instant::now() + ADOPT_GRACE,
                        "{state}"
                    ),
                    other => assert!(
                        want.is_empty() && other == HandBack::Idle,
                        "{state}: {other:?}"
                    ),
                }
                assert_eq!(
                    journal_files(),
                    usize::from(!want.is_empty()),
                    "{state}: the dead file goes"
                );
                let steamos = want.contains(&TakeoverStep::RestoreSteamos);
                assert_eq!(steamos_dropin_path().exists(), steamos, "{state}");
                assert!(!session_plus_dropin_path().exists(), "{state}");
                drop(t);
                takeover().settle(|_| true);
                *takeover() = Takeover::EMPTY;
                let _ = std::fs::remove_file(steamos_dropin_path());
            }

            // An older host's runtime-dir record: its steps, its masks, and the file goes.
            let record = format!(
                r#"{{"stopped_autologin":["{UNIT}"],"steamos":false,"managed_session":true,"forced_screen_env":true}}"#
            );
            std::fs::write(legacy_record_path(), record).unwrap();
            restore_takeover_on_startup();
            {
                let t = takeover();
                let want = [
                    units(&[UNIT]),
                    TakeoverStep::StopSession,
                    TakeoverStep::UnsetScreenEnv,
                ];
                assert_eq!(sorted(t.owed.iter().map(|o| o.step.clone())), sorted(want));
                assert!(t.autologin_masked);
                assert!(matches!(t.hand_back, HandBack::Due(_)));
            }
            assert!(!legacy_record_path().exists());
            takeover().settle(|_| true);
            *takeover() = Takeover::EMPTY;

            // A host mid-takeover adopts a dead one's steps beside its own; its hand-back stays.
            takeover().owe_units(vec![UNIT.into()]);
            journal::crash();
            *takeover() = Takeover::EMPTY;
            takeover().owe_once(TakeoverStep::UnsetScreenEnv);
            journal::reclaim(&crate::backend_by_id);
            let t = takeover();
            assert_eq!(
                sorted(t.owed.iter().map(|o| o.step.clone())),
                sorted([units(&[UNIT]), TakeoverStep::UnsetScreenEnv])
            );
            assert_eq!(t.hand_back, HandBack::Idle);
        });
    }

    /// `--plain` ACTIVE is the third column. Wrong column is silent both ways.
    #[test]
    fn listed_units_take_the_active_column_not_the_load_column() {
        // UNIT LOAD ACTIVE SUB DESCRIPTION.
        let out = "gamescope-session-plus@ogui-steam.service loaded active running Gamescope Session Plus\n\
                   gamescope-session-plus@steam.service loaded inactive dead Gamescope Session Plus\n";
        assert_eq!(
            parse_listed_units(out),
            vec![
                (
                    "gamescope-session-plus@ogui-steam.service".to_string(),
                    "active".to_string()
                ),
                (
                    "gamescope-session-plus@steam.service".to_string(),
                    "inactive".to_string()
                ),
            ]
        );
        // `loaded` is the LOAD column and must never be mistaken for the state — that is the
        // off-by-one this pins.
        assert!(parse_listed_units(out).iter().all(|(_, a)| a != "loaded"));
        // Anything that is not one of our template's instances is not ours to touch.
        assert!(
            parse_listed_units("plasma-plasmashell.service loaded active running Shell\n")
                .is_empty()
        );
        assert!(parse_listed_units("").is_empty());
    }

    #[test]
    fn display_manager_flavor_detection() {
        let base = std::env::temp_dir().join(format!("pf-dm-scan-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // No alias symlink (no DM installed — getty autologin boxes) → None.
        assert_eq!(display_manager_unit_under(&base), None);
        // The Fedora-style alias symlink resolves to its target's basename (read_link, not
        // canonicalize — the target needn't exist on the build box).
        std::os::unix::fs::symlink(
            "/usr/lib/systemd/system/plasmalogin.service",
            base.join("display-manager.service"),
        )
        .unwrap();
        assert_eq!(
            display_manager_unit_under(&base).as_deref(),
            Some("plasmalogin.service")
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// A failure has to say which of the four things went wrong: they need four different fixes.
    /// The helper's own words survive to the operator, and a helper that never ran never reads as
    /// one that ran and refused.
    #[test]
    fn dm_helper_failures_stay_distinguishable() {
        let refusal = "pf-dm-helper: user 'nobara-user' is not in the 'punktfunk' group — \
                       refusing. Grant it with: sudo usermod -aG punktfunk nobara-user";
        let ran = DmHelperError::Refused {
            helper: "/usr/libexec/punktfunk/pf-dm-helper",
            code: Some(1),
            stderr: refusal.to_string(),
        }
        .to_string();
        // Verbatim: the helper already names the user, the group and the exact command.
        assert!(ran.contains(refusal), "{ran}");
        assert!(ran.contains("ran and refused"), "{ran}");

        // None of the three "it never got that far" shapes may claim a refusal, or the operator
        // goes looking for a group problem that isn't there.
        for e in [
            DmHelperError::NotInstalled,
            DmHelperError::NotExecutable {
                helper: "/usr/libexec/punktfunk/pf-dm-helper",
                io: "No such file or directory (os error 2)".into(),
            },
            DmHelperError::Denied {
                helper: "/usr/libexec/punktfunk/pf-dm-helper",
                code: 127,
                stderr: "Error executing command as another user: Not authorized".into(),
            },
        ] {
            let s = e.to_string();
            assert!(!s.contains("ran and refused"), "{s}");
            // None of them may send the operator after group membership, which is only ever the
            // answer when the helper actually evaluated it.
            assert!(!s.contains("group"), "{s}");
            // Every one of them still ends in something the operator can act on.
            assert!(s.contains("polkit") || s.contains("install"), "{s}");
        }

        // A reinstall and a polkit rule must appear only where they can actually help, never on
        // the path that ran and was refused.
        assert!(!ran.contains("reinstall"), "{ran}");
    }

    /// On glass: managed hold against real DRM, gaming session idled so nothing holds master.
    #[test]
    #[ignore = "on glass: needs a connected head and no compositor holding /dev/dri/card*"]
    fn live_the_managed_hold_darkens_a_real_panel() {
        fn lit() -> Vec<(String, String)> {
            let mut v = Vec::new();
            let Ok(rd) = std::fs::read_dir("/sys/class/drm") else {
                return v;
            };
            for e in rd.flatten() {
                let p = e.path();
                let f = |n: &str| {
                    std::fs::read_to_string(p.join(n))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default()
                };
                if f("status") == "connected" {
                    v.push((e.file_name().to_string_lossy().into_owned(), f("dpms")));
                }
            }
            v.sort();
            v
        }

        let before = lit();
        println!("before: {before:?}");
        assert!(
            !before.is_empty(),
            "needs a connected head to mean anything"
        );

        super::managed_darken_acquire(true);
        std::thread::sleep(std::time::Duration::from_secs(2));
        let during = lit();
        println!("during: {during:?}");

        // A reconnect must not take a second hold — if it did, the release below would leave the
        // panel dark. The pure test models this; here it is against the real refcount.
        super::managed_darken_acquire(true);

        super::managed_darken_release();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let after = lit();
        println!("after:  {after:?}");

        let went_dark: Vec<&String> = during
            .iter()
            .zip(&before)
            .filter(|((_, now), (_, was))| was == "On" && now == "Off")
            .map(|((n, _), _)| n)
            .collect();
        if went_dark.is_empty() {
            println!("nothing was ours to darken (card already mastered?) — skipping");
            return;
        }
        // At least one went dark, not all: a box can carry a connected head we do not manage.
        println!("went dark: {went_dark:?}");
        assert_eq!(after, before, "the release must restore what we found");
    }

    #[test]
    fn the_managed_darken_hold_is_taken_once_and_released_once() {
        // Session, not connect: a reconnect must not take a second hold.
        let mut held = false;
        assert!(managed_darken_acquire_edge(&mut held, true), "0→1 darkens");
        assert!(!managed_darken_acquire_edge(&mut held, true), "reconnect");
        assert!(!managed_darken_acquire_edge(&mut held, true));

        // Restore releases unconditionally: must be idempotent.
        assert!(managed_darken_release_edge(&mut held), "1→0 re-lights");
        assert!(!managed_darken_release_edge(&mut held), "already released");
        assert!(!managed_darken_release_edge(&mut held));

        // It re-arms: a later stream on the same host lifetime darkens again.
        assert!(managed_darken_acquire_edge(&mut held, true));
        assert!(managed_darken_release_edge(&mut held));

        // Not exclusive ⇒ never a hold, so the restore's unconditional release stays a no-op.
        // This is what makes `extend` / `SharedDesktop` ("never blank the real monitors") mean
        // what they say on the managed route.
        let mut held = false;
        assert!(!managed_darken_acquire_edge(&mut held, false));
        assert!(!held);
        assert!(!managed_darken_release_edge(&mut held));
    }

    #[test]
    fn dm_plan_idles_any_dm_that_drove_a_live_session() {
        // Live gaming behind a DM: idle it. Mask is the storm; stopping the DM bars a desktop switch.
        let p = dm_plan(Some("sddm.service"), true);
        assert!(!p.skip && p.dm_relogins);
        // Flavor is not an input: plasmalogin gets the same plan as sddm.
        let q = dm_plan(Some("plasmalogin.service"), true);
        assert!(q.skip == p.skip && q.dm_relogins == p.dm_relogins);
        // Nothing live, DM present: hands off entirely, on every flavor. Killing loaded-but-
        // inactive leftovers frees no Steam; masking them while the DM is up is the storm; and
        // stopping the DM would kill the user's live desktop for it.
        assert!(dm_plan(Some("sddm.service"), false).skip);
        assert!(dm_plan(Some("plasmalogin.service"), false).skip);
        // No DM at all (getty autologin), live: kill and leave it stopped. Nothing relogins, so
        // there is no autologin to idle — and no reason to leave a drop-in on the box.
        let p = dm_plan(None, true);
        assert!(!p.skip && !p.dm_relogins);
        assert!(dm_plan(None, false).skip);
    }

    /// The four [`DmHelperError`] shapes need four different fixes, so the `shape` field must keep
    /// them apart — a helper that could not be executed must never read as one that ran and refused.
    #[test]
    fn dm_helper_error_shapes_stay_distinct() {
        let shapes = [
            DmHelperError::NotInstalled.shape(),
            DmHelperError::NotExecutable {
                helper: "h",
                io: String::new(),
            }
            .shape(),
            DmHelperError::Denied {
                helper: "h",
                code: 127,
                stderr: String::new(),
            }
            .shape(),
            DmHelperError::Refused {
                helper: "h",
                code: Some(1),
                stderr: String::new(),
            }
            .shape(),
        ];
        let unique: std::collections::HashSet<_> = shapes.iter().collect();
        assert_eq!(unique.len(), shapes.len(), "shapes collided: {shapes:?}");
    }

    /// The SCREEN_* unset and the unmask settle only once systemd says done: refused, both stay
    /// owed for the next hand-back. `systemctl` is a fake on the child's PATH.
    #[test]
    fn a_refused_unset_or_unmask_stays_owed() {
        let name = "a_refused_unset_or_unmask_stays_owed";
        let (fake, path) = crate::gamescope::tests::fake_systemctl(name);
        crate::fake::in_child(module_path!(), name, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let exit = |code: &str| std::fs::write(home.join("systemctl-exit"), code).unwrap();
            let screen_env_owed = || takeover().owes(|s| matches!(s, TakeoverStep::UnsetScreenEnv));
            {
                let mut t = takeover();
                t.owe_once(TakeoverStep::UnsetScreenEnv);
                t.owe_units(vec![UNIT.into()]);
                t.autologin_masked = true;
            }
            exit("1");
            unset_forced_session_screen_env();
            lift_autologin_mask();
            assert!(screen_env_owed());
            assert!(takeover().autologin_masked);
            exit("0");
            unset_forced_session_screen_env();
            lift_autologin_mask();
            assert!(!screen_env_owed());
            assert!(!takeover().autologin_masked);
        });
        let _ = std::fs::remove_dir_all(&fake);
    }

    /// A hand-back scheduled while a launch waited on another is cancelled as it begins. Runs in
    /// a child: the takeover is a process global.
    #[test]
    fn a_launch_cancels_a_hand_back_scheduled_while_it_waited() {
        let name = "a_launch_cancels_a_hand_back_scheduled_while_it_waited";
        crate::fake::in_child(module_path!(), name, &[], || {
            let first = Launching::begin();
            let retry = std::thread::spawn(|| drop(Launching::begin()));
            // The first launch fails and schedules the hand-back; the retry waits on it.
            takeover()
                .hand_back
                .schedule(Some(Instant::now() + RESTORE_DEBOUNCE));
            drop(first);
            retry.join().expect("the retry's launch");
            assert_eq!(takeover().hand_back, HandBack::Idle);
        });
    }

    #[test]
    fn reconnect_cancel_waits_out_an_in_flight_restore() {
        // Under keep_alive=off the worker pops before cancel; a running hand-back makes it wait.
        takeover().hand_back = HandBack::Running {
            then: Some(Instant::now() + Duration::from_secs(300)),
        };
        let cancel = std::thread::spawn(cancel_pending_restore);
        // The cancel must not race past the in-flight restore. A sleep-based "still running"
        // probe is the wrong shape (a slow scheduler passes it vacuously). The pending one
        // survives while the hand-back runs; cancel completes and clears it once that ends.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            takeover().hand_back.running(),
            "cancel cleared the pending restore while the restore was still in flight"
        );
        drop(HandBackRun);
        cancel.join().expect("cancel thread panicked");
        assert_eq!(
            takeover().hand_back,
            HandBack::Idle,
            "cancel returned without clearing the pending restore"
        );
    }

    /// A desktop switch hands the box's units to its display manager: the hand-back that follows
    /// a stream must not restart the Game Mode the user returns to. A mask systemd refused to
    /// lift keeps them owed for the next lift. `systemctl` is a fake on the child's PATH.
    #[test]
    fn a_desktop_switch_settles_the_units_once_the_mask_is_off() {
        let name = "a_desktop_switch_settles_the_units_once_the_mask_is_off";
        let (fake, path) = crate::gamescope::tests::fake_systemctl(name);
        crate::fake::in_child(module_path!(), name, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let exit = |code: &str| std::fs::write(home.join("systemctl-exit"), code).unwrap();
            {
                let mut t = takeover();
                t.owe_units(vec![UNIT.into()]);
                t.autologin_masked = true;
            }
            exit("1");
            release_autologin_mask(crate::ActiveKind::DesktopKde);
            assert_eq!(
                takeover().units(),
                [UNIT],
                "a refused unmask keeps its units"
            );
            exit("0");
            release_autologin_mask(crate::ActiveKind::DesktopKde);
            assert!(takeover().units().is_empty());
            assert!(!takeover().live(), "nothing is left to hand back");
        });
        let _ = std::fs::remove_dir_all(&fake);
    }

    /// The hand-back restarts only a unit still running our `sleep`: one the box stopped for a
    /// switch, or runs for real, is the box's. No answer keeps the old restart. `systemctl` is a
    /// fake on the child's PATH, printing the MainPID of a real child.
    #[test]
    fn only_a_unit_still_idled_is_restarted() {
        let name = "only_a_unit_still_idled_is_restarted";
        let (fake, path) = crate::gamescope::tests::fake_systemctl(name);
        crate::fake::in_child(module_path!(), name, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let main_pid =
                |pid: u32| std::fs::write(home.join("systemctl-show"), format!("{pid}\n"));
            std::fs::write(home.join("systemctl-exit"), "0").unwrap();
            let mut idle = Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep");
            let mut real = Command::new("tail")
                .args(["-f", "/dev/null"])
                .spawn()
                .expect("spawn tail");
            main_pid(idle.id()).unwrap();
            assert!(runs_idle_placeholder(UNIT));
            main_pid(real.id()).unwrap();
            assert!(
                !runs_idle_placeholder(UNIT),
                "the box runs this unit for real"
            );
            main_pid(0).unwrap();
            assert!(!runs_idle_placeholder(UNIT), "the box stopped this unit");
            std::fs::write(home.join("systemctl-exit"), "1").unwrap();
            let _ = std::fs::remove_file(home.join("systemctl-show"));
            assert!(runs_idle_placeholder(UNIT), "no answer keeps the restart");
            for child in [&mut idle, &mut real] {
                let _ = child.kill();
                let _ = child.wait();
            }
        });
        let _ = std::fs::remove_dir_all(&fake);
    }

    #[test]
    fn only_a_desktop_switch_ends_the_mask_window() {
        use crate::ActiveKind;
        // The user switched the box to a desktop session mid-stream: our managed game session is
        // over, so the mask defends nothing — and the "Return to Gaming Mode" that follows has to
        // be able to start the unit (the distro session script starts exactly it).
        for kind in [
            ActiveKind::DesktopKde,
            ActiveKind::DesktopGnome,
            ActiveKind::DesktopWlroots,
            ActiveKind::DesktopHyprland,
            ActiveKind::DesktopNiri,
            ActiveKind::DesktopUmbriel,
        ] {
            assert!(switch_ends_mask_window(kind), "{kind:?}");
        }
        // A takeover's own managed session reads as Gaming, so lifting here would void the mask for
        // the whole stream — in exactly the SDDM-relogin window it exists for. Coming back to
        // gaming needs no lift either: it already started.
        assert!(!switch_ends_mask_window(ActiveKind::Gaming));
        // A managed session momentarily down between relaunches reads as None. That is
        // mid-takeover, not the end of one.
        assert!(!switch_ends_mask_window(ActiveKind::None));
    }

    /// End-to-end against real systemd: the decision is wired to the mask, a lift leaves the
    /// restart list intact, and `--runtime` is what comes off (a plain `unmask` does not clear a
    /// runtime mask).
    ///
    /// Ignored by default: it needs a live `systemd --user` manager. Uses a unit name nothing owns
    /// — `mask` is a symlink to `/dev/null`, so this never goes near the box's real gaming session.
    #[test]
    #[ignore = "needs a live systemd --user manager (run explicitly on a Linux box with a session)"]
    fn the_mask_comes_off_only_when_the_box_takes_itself_back() {
        const PROBE: &str = "punktfunk-mask-probe@lifetime-test.service";
        let is_enabled = || {
            let out = std::process::Command::new("systemctl")
                .args(["--user", "is-enabled", PROBE])
                .output()
                .expect("systemctl --user");
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        unmask_unit(PROBE); // a previous failed run must not decide this one

        // Lay an older host's mask on an owed unit, as its record is adopted, under a live managed
        // session of ours.
        {
            let mut t = takeover();
            t.owed = vec![
                Owed {
                    seq: 0,
                    step: TakeoverStep::RestartAutologin {
                        units: vec![PROBE.to_string()],
                    },
                },
                Owed {
                    seq: 0,
                    step: TakeoverStep::StopSession,
                },
            ];
            t.managed = Some(SessionState {
                width: 1280,
                height: 720,
                refresh_hz: 60,
                hdr: false,
            });
            t.autologin_masked = true;
        }
        mask_unit(PROBE);
        assert_eq!(is_enabled(), "masked-runtime");

        // Mid-stream, with the box still ours: the mask is doing its job and must stay. `Gaming` is
        // what our own managed session reads as, and `None` is one momentarily down between
        // relaunches — lifting on either would void the mask for the whole stream.
        release_autologin_mask(crate::ActiveKind::Gaming);
        release_autologin_mask(crate::ActiveKind::None);
        assert_eq!(is_enabled(), "masked-runtime");

        // Idle drop-in shares the window: left on, "Return to Gaming Mode" starts a sleep.
        install_idle_dropin().expect("arm the takeover's idle drop-in");
        assert!(idle_dropin_path().exists());

        // Mid-stream, with the box still ours: the mask is doing its job and must stay. `Gaming` is
        // what our own managed session reads as, and `None` is one momentarily down between
        // relaunches — lifting on either would void the mask for the whole stream.
        release_autologin_mask(crate::ActiveKind::Gaming);
        release_autologin_mask(crate::ActiveKind::None);
        assert_eq!(is_enabled(), "masked-runtime");
        assert!(
            idle_dropin_path().exists(),
            "the idle drop-in must survive a switch that is not to a desktop"
        );
        assert!(
            takeover().managed.is_some(),
            "our session is the box's game mode here"
        );

        // The user switched the box to its own desktop mid-stream: the window is over, and the way
        // back into game mode has to be clear before they ask for it.
        release_autologin_mask(crate::ActiveKind::DesktopKde);
        assert_ne!(is_enabled(), "masked-runtime");
        assert!(
            !idle_dropin_path().exists(),
            "the idle drop-in outlived the switch — the box's Game Mode is a sleep now"
        );
        // The lift ends the units' debt: the display manager runs the box's sessions now, and a
        // hand-back restart would kill the Game Mode the user returns to.
        assert!(takeover().units().is_empty());
        // Our session is stopped and forgotten: its Steam would hold the single instance the box's
        // own Game Mode needs, and a rebuild must not reuse it.
        assert!(takeover().managed.is_none());
        assert!(!takeover().owes(|s| matches!(s, TakeoverStep::StopSession)));
        // Idempotent — the watcher calls it on every switch it confirms.
        release_autologin_mask(crate::ActiveKind::DesktopGnome);
        assert_ne!(is_enabled(), "masked-runtime");

        unmask_unit(PROBE);
        remove_idle_dropin();
        takeover().owed.clear();
        takeover().autologin_masked = false;
    }

    #[test]
    fn connector_status_scan() {
        let base = std::env::temp_dir().join(format!("pf-drm-scan-{}", std::process::id()));
        let mk = |name: &str, status: Option<&str>| {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            if let Some(s) = status {
                std::fs::write(dir.join("status"), s).unwrap();
            }
        };
        // Headless layout: device + render nodes only (no status files) → not connected.
        mk("card0", None);
        mk("renderD128", None);
        assert!(!connected_connector_under(&base));
        // Connectors present but nothing plugged in → still not connected.
        mk("card0-HDMI-A-1", Some("disconnected\n"));
        assert!(!connected_connector_under(&base));
        // A live panel → connected.
        mk("card0-eDP-1", Some("connected\n"));
        assert!(connected_connector_under(&base));
        // A missing base dir (no DRM at all) reads as headless.
        assert!(!connected_connector_under(&base.join("nope")));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
