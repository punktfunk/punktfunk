//! Box takeover: what the host stopped or rewrote on the box to stream a managed session,
//! and the restore that hands it back. The state lives in process memory, mirrored to a
//! crash-restore file (`design/gamemode-and-dedicated-sessions.md`).

use super::*;
use crate::routing::{TakeoverInapplicable, TakeoverVerdict};

/// What this host took over on the box. One lock for all of it, so a crash-restore record or a
/// liveness check never mixes two moments. `std::sync::Mutex` is not reentrant: never call
/// anything that takes [`takeover()`] while holding it.
pub(super) struct Takeover {
    /// Host-lifetime managed session. `GamescopeDisplay` is recreated per client; storing the
    /// session there would cold-start Steam on every reconnect. Same-mode reuse; different mode
    /// relaunches.
    pub(super) managed: Option<SessionState>,
    /// Autologin `gamescope-session-plus@*` units stopped so Steam's single instance is free.
    /// [`schedule_restore_tv_session`] restarts them on disconnect.
    stopped_autologin: Vec<String>,
    /// Display-manager unit an *adopted* pre-idle takeover stopped. Restore is `reset-failed` +
    /// `restart` of the DM: a `--user start` of the gamescope unit has no seat without a DM login,
    /// so gamescope never gets DRM master.
    ///
    /// Adoption-only: live takeovers idle the autologin ([`install_idle_dropin`]) and leave the
    /// DM up. [`takeover_idled`] is the live marker; reading this as that marker skips the switch
    /// gate.
    pub(super) stopped_dm: Option<String>,
    /// Mask left to lift on `stopped_autologin`. Live takeovers idle instead of masking: a masked
    /// unit fails, and a failing unit is the DM relogin-loop engine. True only for a takeover
    /// adopted from a host that still masked. Unmasking a unit we never masked is a no-op; missing
    /// one that is masked bars Game Mode until reboot.
    autologin_masked: bool,
    /// Sentinel mtime at takeover. ChimeraOS-layout `os-session-select` writes
    /// `~/.config/steamos-session-select` in its USER pass; that mtime is the only durable trace of
    /// an in-stream "Switch to Desktop". Bazzite/SteamOS write none; [`is_steam_htpc_platform`]
    /// follows the switch instead.
    ///
    /// Two `Option`s, because the meanings invert:
    /// * outer `None` — never baselined. A missing baseline treats an ancient write as a live
    ///   request.
    /// * `Some(None)` — no sentinel yet; a later file *is* a request.
    /// * `Some(Some(t))` — anything newer than `t` is a request.
    select_baseline: Option<Option<std::time::SystemTime>>,
    /// When [`honor_session_select_switch`] last ran. While recent, refuse a managed relaunch:
    /// gamescope+Steam come up faster than KWin, and a delivering pipeline ends re-detection.
    pub(super) switch_honored_at: Option<Instant>,
    /// This host has an idle drop-in outstanding. Crash sweep: [`restore_takeover_on_startup`].
    pub(super) idle_dropin_armed: bool,
    /// SteamOS analogue of `stopped_autologin`: drop-in is in; restore must remove it and restart
    /// the physical session.
    pub(super) steamos: bool,
    /// Bind drop-in is on the box's own `gamescope-session-plus@` template. That path steals
    /// nothing, so the other fields stay empty. Skip this flag and the drop-in outlives the stream
    /// and Game Mode runs our patched gamescope.
    pub(super) session_dropin_armed: bool,
    /// This host pushed `SCREEN_WIDTH`/`SCREEN_HEIGHT`/`CUSTOM_REFRESH_RATES` into the user
    /// manager. Those survive every unit restart for the rest of the login; restore may
    /// `unset-environment` only values it set (an operator's own `set-environment` is theirs).
    pub(super) forced_screen_env: bool,
}

static TAKEOVER: std::sync::Mutex<Takeover> = std::sync::Mutex::new(Takeover::EMPTY);

pub(super) fn takeover() -> std::sync::MutexGuard<'static, Takeover> {
    TAKEOVER.lock().unwrap_or_else(|e| e.into_inner())
}

impl Takeover {
    const EMPTY: Self = Self {
        managed: None,
        stopped_autologin: Vec::new(),
        stopped_dm: None,
        autologin_masked: false,
        select_baseline: None,
        switch_honored_at: None,
        idle_dropin_armed: false,
        steamos: false,
        session_dropin_armed: false,
        forced_screen_env: false,
    };

    fn record(&self) -> TakeoverState {
        TakeoverState {
            stopped_autologin: self.stopped_autologin.clone(),
            steamos: self.steamos,
            stopped_dm: self.stopped_dm.clone(),
            managed_session: self.managed.is_some(),
            forced_screen_env: self.forced_screen_env,
        }
    }

    /// Anything left to hand back. Wider than the record by the bind drop-in: attach re-mode
    /// steals nothing, but it did rewrite the template.
    fn live(&self) -> bool {
        self.session_dropin_armed || takeover_state_is_live(&self.record())
    }
}

/// After an in-stream desktop switch, refuse managed relaunch until the DM session can come up.
pub(super) const SWITCH_HONOR_GRACE: Duration = Duration::from_secs(120);

/// Managed-route [`crate::panel_dpms`] hold for `Topology::Exclusive`.
///
/// Managed reports `SessionManaged`, so `registry::acquire` never picks up `take_topology_restore`.
/// Release lives in [`do_restore_tv_session`]. A bool, not a count: the session outlives connects,
/// and a per-connect acquire would pin the panel dark for the host's life. Not in [`Takeover`]: the
/// panel call runs under this lock so a release never overtakes an acquire.
static MANAGED_DARKEN_HELD: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

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

pub(super) fn managed_darken_acquire(exclusive: bool) {
    let mut held = MANAGED_DARKEN_HELD
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if managed_darken_acquire_edge(&mut held, exclusive) {
        crate::panel_dpms::acquire_stream_darken();
    }
}

/// Idempotent release: the restore calls this above every early return.
fn managed_darken_release() {
    let mut held = MANAGED_DARKEN_HELD
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if managed_darken_release_edge(&mut held) {
        crate::panel_dpms::release_stream_darken();
    }
}

/// Debounced restore deadline after the last disconnect. A reconnect inside the window clears it
/// and reuses the warm session. Per-connect teardown leaks NVIDIA GPU context.
static PENDING_RESTORE: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// In-flight restore vs (re)connect. Clearing [`PENDING_RESTORE`] only cancels a restore that has
/// not started; `keep_alive=off` is 0 s debounce, so the worker often pops first.
///
/// Hold across [`do_restore_tv_session`]: cancel wins (warm reuse) or restore wins (connect waits,
/// then takes a fully restored box). Restore under a fresh mask is the Relogin storm
/// (the mask never stops SDDM's helper loop — see `mask_unit`).
///
/// LOCK ORDER: OUTERMOST — taken only at [`start_restore_worker`]'s pop,
/// [`cancel_pending_restore`], [`restore_takeover_now`]. Never while another static is held.
static RESTORE_FLIGHT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Default restore delay: long enough that a controller hiccup reuses the warm session.
const RESTORE_DEBOUNCE: Duration = Duration::from_secs(5);

/// SteamOS hand-back with no panel connected: how soon to look for one again.
const NO_PANEL_RECHECK: Duration = Duration::from_secs(30);

/// Bumped by every hand-back, so a launch that began before one can tell.
static RESTORE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) fn restore_generation() -> u64 {
    RESTORE_GEN.load(std::sync::atomic::Ordering::SeqCst)
}

/// Crash-restore record of [`Takeover`] (`design/gamemode-and-dedicated-sessions.md`).
/// Process memory dies with the host; this file lets [`restore_takeover_on_startup`] heal the box.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct TakeoverState {
    stopped_autologin: Vec<String>,
    steamos: bool,
    /// `default` so older takeover files still parse.
    #[serde(default)]
    stopped_dm: Option<String>,
    /// A host-managed [`SESSION_UNIT`] was running. It steals nothing but is still ours to stop.
    /// Restored as an impossible-mode marker, never reused — see [`restore_takeover_on_startup`].
    #[serde(default)]
    managed_session: bool,
    /// Forced `SCREEN_*` into the user manager. Unlike the drop-in (runtime-dir, swept
    /// unconditionally), these outlive the process; this flag is the only crash-safe record they
    /// are ours. `default` so older files still parse.
    #[serde(default)]
    forced_screen_env: bool,
}

/// `$XDG_RUNTIME_DIR` (0700 tmpfs). Cleared on reboot, which restarts autologin itself.
fn takeover_state_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-session-takeover.json")
}

/// Best-effort crash-restore snapshot. Never call while holding [`takeover()`].
pub(super) fn persist_takeover() {
    let state = takeover().record();
    write_takeover_record(&state);
}

/// [`persist_takeover`] for a caller that holds [`takeover()`] across the change it records.
pub(super) fn persist_takeover_held(t: &Takeover) {
    write_takeover_record(&t.record());
}

/// Temp file + rename: a crash mid-write must not leave a record that no longer parses.
fn write_takeover_record(state: &TakeoverState) {
    if !takeover_state_is_live(state) {
        clear_takeover();
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(state) {
        let path = takeover_state_path();
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn clear_takeover() {
    let _ = std::fs::remove_file(takeover_state_path());
}

/// Whether a crash-restore record still owes the box something. No bind drop-in here: startup
/// sweeps it unconditionally, so persisting it would leave a restore with nothing to do. Forced
/// `SCREEN_*` is the opposite — it lives in the user manager, nothing sweeps it, and a crash must
/// still know to unset it.
fn takeover_state_is_live(state: &TakeoverState) -> bool {
    !state.stopped_autologin.is_empty()
        || state.steamos
        || state.stopped_dm.is_some()
        || state.managed_session
        || state.forced_screen_env
}

/// Restart autologin units left `active` under a swept idle drop-in. Gated on a dark box
/// ([`box_session_live`]): bouncing a live game mode or desktop is the bug. Active under the
/// drop-in means "running the sleep". Returns the units restarted.
fn hand_back_idled_units_after_crash() -> Vec<String> {
    if box_session_live() {
        return Vec::new(); // already drawing — the drop-in was inert
    }
    let units: Vec<String> = listed_autologin_units()
        .into_iter()
        .filter(|(_, active)| active == "active")
        .map(|(unit, _)| unit)
        .collect();
    if units.is_empty() {
        return units;
    }
    tracing::warn!(
        ?units,
        "gamescope: the box's Game Mode is running the dead host's idle placeholder and its panel \
         is dark — restarting it"
    );
    for unit in &units {
        if let RestoreVerb::Failed(why) = issue_restore_verb(&["restart", unit]) {
            tracing::error!(unit, status = %why, "gamescope: not restarted");
        }
    }
    ensure_box_session_or_escalate(&units);
    units
}

/// The persisted crash-restore record, when there is one that parses.
fn read_takeover_record() -> Option<TakeoverState> {
    let bytes = std::fs::read(takeover_state_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Adopt a stranded takeover from a previous host and schedule restore after a reconnect grace.
/// Call once from `serve` with [`start_restore_worker`]. Sweeps what a dead host left either way.
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
    let record = read_takeover_record();
    // A fresh host owns no session unit. One left running holds Steam with nothing to stop it, and
    // reads as the box's own session to the hand-back check below.
    // Unknown and `deactivating` read as not running.
    if !record.as_ref().is_some_and(|s| s.managed_session)
        && unit_state(SESSION_UNIT)
            .is_some_and(|s| matches!(s.as_str(), "active" | "activating" | "reloading"))
    {
        tracing::warn!(
            "gamescope: stopping a managed session a previous host instance left running"
        );
        stop_session(SESSION_UNIT);
    }
    // Removing the FILE does not restart the unit still sleeping under it; the takeover file may
    // be absent, so nothing below would. Hand the live idle unit back.
    let handed_back = if remove_idle_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover idle drop-in from a previous host instance — the box's \
             own Game Mode session would have started and then done nothing"
        );
        hand_back_idled_units_after_crash()
    } else {
        Vec::new()
    };
    // Only a SteamOS record keeps its drop-in, for restore to remove. Any other copy is stranded;
    // an older host's `$HOME` one also outlives reboots.
    if !record.as_ref().is_some_and(|s| s.steamos) && remove_steamos_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover SteamOS headless drop-in from a previous host instance"
        );
        systemctl_user(&["daemon-reload"]);
    }
    let Some(mut state) = record else {
        clear_takeover();
        return;
    };
    // Already restarted above; a second restart would kill a Steam still booting.
    state.stopped_autologin.retain(|u| !handed_back.contains(u));
    if !takeover_state_is_live(&state) {
        clear_takeover();
        return;
    }
    tracing::warn!(
        units = ?state.stopped_autologin,
        steamos = state.steamos,
        stopped_dm = ?state.stopped_dm,
        managed_session = state.managed_session,
        forced_screen_env = state.forced_screen_env,
        "gamescope: found a stranded takeover from a previous host instance — scheduling TV restore"
    );
    // Mask presence is not persisted. Unmasking a unit we never masked is a no-op; skipping one
    // that is masked bars Game Mode until reboot.
    {
        let mut t = takeover();
        t.autologin_masked = !state.stopped_autologin.is_empty();
        t.stopped_autologin = state.stopped_autologin;
        t.steamos = state.steamos;
        t.stopped_dm = state.stopped_dm;
        // Drop-in already swept above. SCREEN_* live in the user manager; only this flag
        // authorises [`unset_forced_session_screen_env`]. Adopting `false` is correct: the crashed
        // host never forced them, and unsetting an operator's values would be a bug.
        t.forced_screen_env = state.forced_screen_env;
        if state.managed_session {
            // Adopted session is something to STOP, never reuse: the launch mode is not persisted.
            // 0x0/0 Hz can never match `create_managed_session`, so every route relaunches, while
            // `takeover_live` still sees a session that owes a `stop`.
            t.managed = Some(SessionState {
                width: 0,
                height: 0,
                refresh_hz: 0,
                hdr: false,
            });
        }
    }
    // Launch-time baseline is gone; a long-existing sentinel must not read as a live switch.
    record_session_select_baseline();
    // 15 s: a client reconnecting right after restart cancels this and keeps the streamed session.
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Instant::now() + Duration::from_secs(15));
}

/// Live managed-takeover marker (arms the in-stream switch gate). Process memory, not disk: a
/// drop-in we did not write belongs to a dead host.
pub(super) fn takeover_idled() -> bool {
    takeover().idle_dropin_armed
}

/// No-op unless we set them: `unset-environment` is indiscriminate.
fn unset_forced_session_screen_env() {
    if !std::mem::take(&mut takeover().forced_screen_env) {
        return;
    }
    systemctl_user(&[
        "unset-environment",
        "SCREEN_WIDTH",
        "SCREEN_HEIGHT",
        "CUSTOM_REFRESH_RATES",
    ]);
    tracing::info!(
        "gamescope: unset the forced SCREEN_WIDTH/SCREEN_HEIGHT/CUSTOM_REFRESH_RATES — the box's \
         own game mode is back on its own resolution"
    );
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
fn unmask_unit(unit: &str) {
    systemctl_user(&["unmask", "--runtime", unit]);
}

/// Idempotent. Keeps the stopped units: mask lifetime is shorter than the takeover, and restore
/// still owes them a start. Holds [`takeover()`] across the unmask, so a restore never restarts a
/// unit that is still masked.
fn lift_autologin_mask() {
    let mut t = takeover();
    if !std::mem::take(&mut t.autologin_masked) {
        return;
    }
    for unit in &t.stopped_autologin {
        unmask_unit(unit);
    }
    tracing::info!(
        units = ?t.stopped_autologin,
        "gamescope: lifted the takeover's runtime mask — the box can enter its own game mode again"
    );
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
    )
}

/// Watcher half of the mid-stream hand-back (sentinel detector is the other). Both must run or
/// one distro family keeps an idled Game Mode.
pub fn release_autologin_mask(switched_to: crate::ActiveKind) {
    if !switch_ends_mask_window(switched_to) {
        return;
    }
    lift_autologin_mask();
    // Not [`clear_takeover`]: restore still owes the stopped units a start. Left on, "Return to
    // Gaming Mode" starts a unit that only sleeps.
    if remove_idle_dropin() {
        tracing::info!(
            switched_to = ?switched_to,
            "gamescope: the box left our game session for a desktop — removed the takeover's idle \
             drop-in so its own Game Mode runs for real again"
        );
    }
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
    takeover().select_baseline = Some(session_select_mtime());
}

pub(super) fn session_select_requested() -> bool {
    let baseline = takeover().select_baseline;
    sentinel_advanced(baseline, session_select_mtime())
}

/// No baseline ⇒ no request: a missing baseline must not mean "the sentinel appeared".
fn sentinel_advanced(
    baseline: Option<Option<std::time::SystemTime>>,
    now: Option<std::time::SystemTime>,
) -> bool {
    match (baseline, now) {
        (Some(Some(base)), Some(now)) => now > base,
        (Some(None), Some(_)) => true, // no sentinel at baseline — created during the session
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
    // Mask first, while the unit list still exists — this path discards that list.
    lift_autologin_mask();
    takeover().stopped_autologin.clear();
    clear_takeover();
    takeover().managed = None;
    stop_session(SESSION_UNIT); // switch already killed Steam — clear the unit
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
        match crate::proc::status_within(
            Command::new(OS_SESSION_SELECT).arg("plasma"),
            DM_VERB_BUDGET,
        ) {
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

/// `(unit, active)` from `--plain` (UNIT LOAD ACTIVE …). Unanswered query = none listed (safe).
fn listed_autologin_units() -> Vec<(String, String)> {
    let Ok(out) = crate::proc::output_within(
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
    ) else {
        return Vec::new();
    };
    parse_listed_units(&String::from_utf8_lossy(&out.stdout))
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
/// degrades to attach) — never mask-only; a mask while the DM is up is the relogin storm.
pub(super) fn stop_autologin_sessions() -> Result<()> {
    let listed = listed_autologin_units();
    if listed.is_empty() {
        return Ok(()); // nothing autologged in (or the query failed) — Steam is already free
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
    let logins_before = max_logind_session_id();
    let mut stopped = Vec::new();
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
        stopped.push(unit);
    }
    takeover().stopped_autologin = stopped;
    persist_takeover();
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
/// linger window restarts autologin under the live session.
pub fn cancel_pending_restore() {
    // Once restore is running, wait it out. Racing past it brings the DM back under a fresh mask.
    let _flight = match RESTORE_FLIGHT.try_lock() {
        Ok(g) => g,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            tracing::info!(
                "gamescope: a TV-session restore is in flight — the (re)connect waits for it, \
                 then takes the restored session over from scratch"
            );
            RESTORE_FLIGHT.lock().unwrap_or_else(|e| e.into_inner())
        }
    };
    let mut g = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
    if g.is_some() {
        *g = None;
        tracing::info!(
            "gamescope: client (re)connected — cancelled the pending TV-session restore"
        );
    }
}

/// Same linger policy as pooled backends. Unconfigured → [`RESTORE_DEBOUNCE`]. Forever → `None`.
fn restore_delay() -> Option<Duration> {
    use crate::policy::{self, Linger};
    match policy::prefs()
        .configured_effective()
        .map(|e| e.keep_alive.linger())
    {
        Some(Linger::Immediate) => Some(Duration::from_secs(0)),
        Some(Linger::For(d)) => Some(d),
        Some(Linger::Forever) => None,
        None => Some(RESTORE_DEBOUNCE),
    }
}

/// Debounced restore so a reconnect reuses the warm session. `keep_alive=forever` schedules none.
pub fn schedule_restore_tv_session() {
    if !takeover_live() {
        return; // nothing was taken over → nothing to restore (also the non-managed path)
    }
    match restore_delay() {
        None => {
            // keep_alive=forever → pin the managed session; leave PENDING_RESTORE unset.
            *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None;
            tracing::info!(
                "gamescope: keep-alive=forever — managed session held (no TV-restore scheduled; \
                 return to gaming mode or restart the host to free it)"
            );
        }
        Some(delay) => {
            *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(Instant::now() + delay);
            tracing::info!(
                secs = delay.as_secs(),
                "gamescope: scheduled TV-session restore (keep-alive policy; cancelled on reconnect)"
            );
        }
    }
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
    let mode = {
        let t = takeover();
        if !t.live() {
            return None;
        }
        t.managed
            .as_ref()
            .map(|s| (s.width, s.height, s.refresh_hz))
    };
    let restore_in = PENDING_RESTORE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|at| at.saturating_duration_since(Instant::now()));
    Some(HeldTakeover { mode, restore_in })
}

/// Console Release: hand the box's own session back now, whatever the keep-alive. The restore
/// worker runs it, so a reconnect still cancels it. `false` = nothing held.
pub fn release_takeover() -> bool {
    if !takeover_live() {
        return false;
    }
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    tracing::info!("gamescope: released from the console — handing the box's own session back");
    true
}

/// True while anything taken over is still ours to hand back.
fn takeover_live() -> bool {
    takeover().live()
}

/// Synchronous: the host is exiting and a live takeover must not outlive it. Ignores keep-alive
/// (`forever` is for the next client). Crash-restore lives in `$XDG_RUNTIME_DIR`, which dies with
/// the user manager.
pub fn restore_takeover_now() {
    // Take the flight lock BEFORE reading the takeover state: if the worker's debounced restore is
    // mid-run, this waits it out and then finds `takeover_live()` false — one restore, not two
    // interleaved ones. The worker is bounded by the same verb budgets this path would use.
    let _flight = RESTORE_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if !takeover_live() {
        return;
    }
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) = None; // doing it right here
    tracing::info!("gamescope: host is shutting down — restoring the box's own session first");
    // `verify: false`: the ladder waits up to a minute; shutdown grace is 20 s then `exit(0)`.
    do_restore_tv_session(false);
}

/// What a bounded `systemctl --user` lifecycle verb on the restore path actually did. Three states:
/// the log line is the only thing an operator sees about a box that may or may not have its screen back.
enum RestoreVerb {
    Done,
    /// Budget expired: `status_within` kills the systemctl client, not the queued job.
    StillRunning,
    /// systemd said no, or the helper could not be spawned. The only outcome an operator acts on.
    Failed(String),
}

/// Timeout is StillRunning, not Failed: a `restart` of `gamescope-session.target` blocks on Steam
/// tearing a game down, which routinely exceeds [`UNIT_VERB_BUDGET`]. The bound stays — shutdown
/// grace is 20 s, and an unbounded verb costs the DM restore that follows.
fn issue_restore_verb(args: &[&str]) -> RestoreVerb {
    match crate::proc::status_within(
        Command::new("systemctl").arg("--user").args(args),
        UNIT_VERB_BUDGET,
    ) {
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
/// Detached: holding [`RESTORE_FLIGHT`] for a minute would block every reconnect. Call after
/// `clear_takeover()`, or the first poll reads our own finished takeover as a new one.
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
        if let RestoreVerb::Failed(why) = issue_restore_verb(&["stop", unit]) {
            tracing::warn!(unit, status = %why, "gamescope: autologin unit not stopped");
        }
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

fn do_restore_tv_session(verify: bool) {
    RESTORE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // Only release for managed Exclusive: SessionManaged never rides `take_topology_restore`.
    // Above every early return. Idempotent.
    managed_darken_release();
    // SteamOS restore: shrink the drop-in to its kill signal + restart the target, unless a
    // desktop is already up. The takeover lock spans it, so a SteamOS launch waits for the restart.
    {
        let mut t = takeover();
        if t.steamos {
            // No panel: restarting the target crash-loops gamescope. Keep headless and look again
            // later: a TV in standby reports disconnected until it is switched on.
            if !physical_display_connected() {
                tracing::debug!(
                    secs = NO_PANEL_RECHECK.as_secs(),
                    "gamescope (SteamOS): no physical display connected — keeping the headless \
                     session and checking again"
                );
                *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(Instant::now() + NO_PANEL_RECHECK);
                return;
            }
            t.steamos = false;
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
            ) {
                if remove_steamos_dropin() {
                    systemctl_user(&["daemon-reload"]);
                }
                tracing::info!(
                    "gamescope (SteamOS): a desktop session is active — removed the headless \
                     override, not restarting the gaming session"
                );
                clear_takeover();
                return;
            }
            // Our headless Steam may be under 60 s old; its stop must not count as a failure.
            forget_host_short_sessions();
            let restarted = issue_restore_verb(&["restart", STEAMOS_SESSION_TARGET]);
            forget_host_short_sessions();
            // A restart still running has yet to stop gamescope; its kill-signal drop-in stays
            // until the next takeover or hand-back, or the runtime dir goes.
            if matches!(restarted, RestoreVerb::Done) && remove_steamos_dropin() {
                systemctl_user(&["daemon-reload"]);
            }
            match restarted {
                RestoreVerb::Done => tracing::info!(
                    "gamescope (SteamOS): restored the physical gaming session (removed headless \
                     override)"
                ),
                RestoreVerb::StillRunning => tracing::info!(
                    "gamescope (SteamOS): the {STEAMOS_SESSION_TARGET} restart is still running \
                     after {}s (Steam closing a game is the usual reason) — systemd owns the job \
                     from here; the panel comes back when it completes",
                    UNIT_VERB_BUDGET.as_secs()
                ),
                RestoreVerb::Failed(why) => tracing::error!(
                    status = %why,
                    "gamescope (SteamOS): could not restart {STEAMOS_SESSION_TARGET} — the Deck's \
                     panel stays dark until someone runs \
                     `systemctl --user restart {STEAMOS_SESSION_TARGET}` (the headless override is \
                     already removed, so that restart is all it needs)"
                ),
            }
            clear_takeover(); // after the restart, not before it
            if verify {
                ensure_box_session_or_escalate(&[STEAMOS_SESSION_TARGET.to_string()]);
            }
            return;
        }
    }
    // Before taking the list (it reads that list) and before any early return.
    lift_autologin_mask();
    // Drained in one short hold: nothing below runs under the takeover lock.
    let (units, dm, managed_was_running) = {
        let mut t = takeover();
        (
            std::mem::take(&mut t.stopped_autologin),
            t.stopped_dm.take(),
            t.managed.take().is_some(),
        )
    };
    if units.is_empty() && dm.is_none() {
        if managed_was_running {
            stop_session(SESSION_UNIT);
            tracing::info!(
                "gamescope: stopped the idle managed session (nothing was taken over — no box \
                 session to restore)"
            );
        }
        // Attach re-mode leaves the bind drop-in and SCREEN_*. Undo without bouncing Game Mode.
        disarm_session_plus_dropin();
        unset_forced_session_screen_env();
        clear_takeover();
        return;
    }
    stop_session(SESSION_UNIT); // our gamescope/Steam session, so Steam is free for the autologin

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
    ) {
        tracing::info!(
            "gamescope: a desktop session is active — not restoring the TV gaming session"
        );
        clear_takeover(); // units/DM records already drained into locals
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
        // LAST, not first. The persisted marker is the only thing that heals a box whose DM is
        // down after this process dies. Every step above is unbounded work on a 20 s shutdown
        // grace (`native.rs` then `exit(0)`, no destructors). Delete before the restart and an
        // expiry in between leaves the box dark with nothing on disk saying so.
        clear_takeover();
        return;
    }
    // Idle drop-in already gone (removed above every early return), so these restarts bring
    // the box's real session back rather than another idle one.
    for unit in &units {
        // `restart`, not `start`: the idle takeover leaves the unit ACTIVE, and `start` on an
        // active unit is a no-op that would report success over a session still running nothing.
        match issue_restore_verb(&["restart", unit]) {
            RestoreVerb::Done => tracing::info!(
                unit,
                "restored the TV's autologin gaming session (debounce elapsed, no client)"
            ),
            // A `--user start` of a gamescope-session-plus unit waits for its Exec to signal, and
            // Steam's own start routinely exceeds the bound. Queued is not failed.
            RestoreVerb::StillRunning => tracing::info!(
                unit,
                "the TV's autologin gaming session is still starting after {}s — systemd owns the \
                 job from here",
                UNIT_VERB_BUDGET.as_secs()
            ),
            RestoreVerb::Failed(why) => tracing::error!(
                unit,
                status = %why,
                "could not restart the TV's autologin gaming session — the box is left out of \
                 game mode until someone runs `systemctl --user start {unit}` (a masked unit or a \
                 tripped start limit are the usual causes: \
                 `systemctl --user unmask --runtime {unit} && systemctl --user reset-failed {unit}`)"
            ),
        }
    }
    clear_takeover(); // only now, with the restarts actually issued
    if verify {
        ensure_box_session_or_escalate(&units);
    }
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
                // Peek first, pop only under RESTORE_FLIGHT: popping here re-opens the cancel window.
                let due = PENDING_RESTORE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some_and(|deadline| Instant::now() >= deadline);
                if due {
                    let _flight = RESTORE_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
                    let still_due = {
                        let mut g = PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner());
                        match *g {
                            Some(deadline) if Instant::now() >= deadline => {
                                *g = None;
                                true
                            }
                            _ => false,
                        }
                    };
                    if still_due {
                        do_restore_tv_session(true);
                    }
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

    /// A managed session that stole nothing is still a takeover: the transient unit outlives a crash.
    #[test]
    fn a_managed_session_alone_is_a_takeover_worth_persisting() {
        let nothing = TakeoverState::default();
        assert!(!takeover_state_is_live(&nothing));
        let managed = TakeoverState {
            managed_session: true,
            ..Default::default()
        };
        assert!(takeover_state_is_live(&managed));
        // The three original fields keep their meaning, each on its own.
        assert!(takeover_state_is_live(&TakeoverState {
            stopped_autologin: vec!["gamescope-session-plus@steam.service".into()],
            ..Default::default()
        }));
        assert!(takeover_state_is_live(&TakeoverState {
            steamos: true,
            ..Default::default()
        }));
        assert!(takeover_state_is_live(&TakeoverState {
            stopped_dm: Some("sddm.service".into()),
            ..Default::default()
        }));
    }

    /// Live is the record's answer plus the bind drop-in, which startup sweeps and so is never
    /// persisted.
    #[test]
    fn only_the_bind_dropin_is_live_without_being_persisted() {
        assert!(!Takeover::EMPTY.live());
        let dropin = Takeover {
            session_dropin_armed: true,
            ..Takeover::EMPTY
        };
        assert!(dropin.live());
        assert!(!takeover_state_is_live(&dropin.record()));
        let managed = Takeover {
            managed: Some(SessionState {
                width: 0,
                height: 0,
                refresh_hz: 0,
                hdr: false,
            }),
            ..Takeover::EMPTY
        };
        assert!(managed.live());
        assert!(managed.record().managed_session);
    }

    /// SCREEN_* live in the user manager; the persisted flag is the only crash-safe record they are ours.
    #[test]
    fn a_forced_session_resolution_alone_is_a_takeover_worth_persisting() {
        assert!(takeover_state_is_live(&TakeoverState {
            forced_screen_env: true,
            ..Default::default()
        }));
    }

    /// An older host's takeover file has neither new field; it must still parse (the box it
    /// describes is mid-takeover, and refusing the file is refusing the restore).
    #[test]
    fn an_older_takeover_file_still_parses() {
        let old =
            r#"{"stopped_autologin":["gamescope-session-plus@steam.service"],"steamos":false}"#;
        let state: TakeoverState = serde_json::from_str(old).expect("older file parses");
        assert_eq!(state.stopped_autologin.len(), 1);
        assert!(!state.managed_session, "absent field defaults to false");
        assert!(takeover_state_is_live(&state));
    }

    #[test]
    fn session_select_sentinel_needs_a_baseline() {
        let t0 = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let t1 = t0 + std::time::Duration::from_secs(1);
        // Never baselined: the sentinel is permanent; an ancient write is not a live request.
        assert!(!sentinel_advanced(None, Some(t0)));
        assert!(!sentinel_advanced(None, None));
        // Baselined with no sentinel yet, then one appeared inside the session: a real request.
        assert!(sentinel_advanced(Some(None), Some(t0)));
        assert!(!sentinel_advanced(Some(None), None));
        // Baselined at an mtime: only a newer one is the user's in-stream switch. The write that
        // brought the box into game mode is the baseline itself, so it reads as no request.
        assert!(sentinel_advanced(Some(Some(t0)), Some(t1)));
        assert!(!sentinel_advanced(Some(Some(t0)), Some(t0)));
        assert!(!sentinel_advanced(Some(Some(t1)), Some(t0)));
        assert!(!sentinel_advanced(Some(Some(t0)), None));
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

    #[test]
    fn reconnect_cancel_waits_out_an_in_flight_restore() {
        // Under keep_alive=off the worker pops before cancel; RESTORE_FLIGHT makes cancel wait.
        let flight = RESTORE_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(Instant::now() + Duration::from_secs(300));
        let cancel = std::thread::spawn(cancel_pending_restore);
        // The cancel must not race past the in-flight restore. A sleep-based "still running"
        // probe is the wrong shape (a slow scheduler passes it vacuously). Pending survives while
        // the flight lock is held; cancel completes and clears it once released.
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            PENDING_RESTORE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some(),
            "cancel cleared the pending restore while the restore was still in flight"
        );
        drop(flight);
        cancel.join().expect("cancel thread panicked");
        assert!(
            PENDING_RESTORE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none(),
            "cancel returned without clearing the pending restore"
        );
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

        // Lay the takeover's mask exactly as `stop_autologin_sessions` does.
        {
            let mut t = takeover();
            t.stopped_autologin = vec![PROBE.to_string()];
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

        // The user switched the box to its own desktop mid-stream: the window is over, and the way
        // back into game mode has to be clear before they ask for it.
        release_autologin_mask(crate::ActiveKind::DesktopKde);
        assert_ne!(is_enabled(), "masked-runtime");
        assert!(
            !idle_dropin_path().exists(),
            "the idle drop-in outlived the switch — the box's Game Mode is a sleep now"
        );
        // The restart list survives the lift: the mask's lifetime is shorter than the takeover's,
        // and the disconnect restore still owes these units a `start`.
        assert_eq!(takeover().stopped_autologin.as_slice(), [PROBE]);
        // Idempotent — the watcher calls it on every switch it confirms.
        release_autologin_mask(crate::ActiveKind::DesktopGnome);
        assert_ne!(is_enabled(), "masked-runtime");

        unmask_unit(PROBE);
        remove_idle_dropin();
        takeover().stopped_autologin.clear();
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
