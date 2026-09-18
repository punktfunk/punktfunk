//! Headless gamescope virtual display: spawn or attach a nested compositor at the client's mode,
//! capture its PipeWire `Video/Source` node, inject through its EIS socket.
//!
//! Three routes, resolved per session by [`crate::resolve_gamescope_route`] and stored on
//! [`GamescopeDisplay`]. Never reread from the process env: a second connect would retarget
//! this instance between the decision and `create`. Dropping a spawned [`VirtualOutput`] kills
//! the process. Managed sessions live at host lifetime ([`MANAGED_SESSION`]); restore is this
//! module's job.
//!
//! Needs PipeWire + libei in gamescope, and a usable Vulkan device. Input: `inject/libei.rs`.
//! Takeover: `design/gamemode-and-dedicated-sessions.md`.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use crate::routing::{TakeoverInapplicable, TakeoverVerdict};
use anyhow::{anyhow, bail, Context, Result};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[path = "gamescope/discovery.rs"]
mod discovery;
#[path = "gamescope/heads.rs"]
mod heads;
#[path = "gamescope/splash.rs"]
mod splash;
use discovery::{
    check_gamescope_version, find_gamescope_eis_socket, find_gamescope_node, gamescope_bin,
    gamescope_can_composite_external_overlay, gamescope_can_offer_refresh_rates,
    gamescope_honours_xkb_env, gamescope_node_present, gamescope_paints_on_commit,
    poll_managed_node, wait_for_node, wayland_name_from_log,
};
pub(crate) use discovery::{
    display_presenting, game_session_exited, gamescope_can_composite_cursor, gamescope_hdr_capable,
    is_available, note_spawn_flags_lost, steam_appid_from_launch, wait_for_steam_game_exit,
    SteamGameWatch,
};
pub(crate) use heads::list_monitors;
pub(crate) use splash::run as splash_run;

/// Per-session gamescope driver. Route, launch command, HDR, and isolation live on this instance
/// — a concurrent connect must not retarget them through the process env.
///
/// Managed: host-manage `gamescope-session-plus` / SteamOS at the client's mode.
/// Attach: capture + inject an already-running gamescope; no lifecycle ownership.
/// Spawn: bare headless gamescope running [`VirtualDisplay::set_launch_command`].
///
/// Operator env (`PUNKTFUNK_GAMESCOPE_{MANAGED,ATTACH,NODE,SESSION}`) feeds
/// `routing::operator_gamescope` once; it is never republished here.
#[derive(Default)]
pub struct GamescopeDisplay {
    /// Whose display this is. Set by `set_client_identity` before `create`, so the
    /// per-device topology (`design/web-console-overhaul.md` §6.1) can be resolved here.
    client_fp: Option<[u8; 32]>,

    /// Bare-spawn command. Not the process-global `PUNKTFUNK_GAMESCOPE_APP`.
    cmd: Option<String>,
    /// Set before `create`. Gamescope cannot enable HDR live, so this is part of the reuse key.
    hdr: bool,
    /// `None` falls through to bare spawn. Must not be read from the process env.
    route: Option<crate::GamescopeRoute>,
    /// Bare-spawn only (`design/gamescope-multiuser.md`). Managed/attach stay shared-plane.
    isolation: Option<crate::SessionIsolation>,
    /// Exclusive darken-hold release, picked up by [`VirtualDisplay::take_topology_restore`].
    pending_restore: Option<Box<dyn FnOnce() + Send>>,
    /// This acquire spawned gamescope, so `cmd` is already its primary child. A keep-alive reuse
    /// leaves it `false` and the session must launch into the live compositor instead.
    spawned_nested_launch: bool,
    /// `mode_conflict: join` admitted this session.
    join_live: bool,
    /// Seat key of the last bare spawn (`gamescope-N`), which names it for a joiner.
    join_name: Option<String>,
}

/// Mode + HDR the managed session was launched at. HDR is in the reuse key: gamescope cannot
/// turn it on live.
struct SessionState {
    width: u32,
    height: u32,
    refresh_hz: u32,
    hdr: bool,
}

/// Host-lifetime managed session. `GamescopeDisplay` is recreated per client; storing the session
/// there would cold-start Steam on every reconnect. Same-mode reuse; different mode relaunches.
static MANAGED_SESSION: std::sync::Mutex<Option<SessionState>> = std::sync::Mutex::new(None);

/// Serialises the managed-session launch in [`create_managed_session`] — and nothing else.
///
/// LOCK ORDER: `MANAGED_LAUNCH` → [`MANAGED_SESSION`], never the reverse. No restore path may
/// take this lock: `do_restore_tv_session` needs [`MANAGED_SESSION`] and must not sit behind a
/// ~90 s launch. Two Managed connects in one launch window both relaunch otherwise, and the
/// second `stop_session(SESSION_UNIT)` kills the unit the first is still polling.
static MANAGED_LAUNCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Autologin `gamescope-session-plus@*` units stopped so Steam's single instance is free.
/// [`schedule_restore_tv_session`] restarts them on disconnect.
static STOPPED_AUTOLOGIN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Display-manager unit an *adopted* pre-idle takeover stopped. Restore is `reset-failed` +
/// `restart` of the DM: a `--user start` of the gamescope unit has no seat without a DM login,
/// so gamescope never gets DRM master.
///
/// Adoption-only: live takeovers idle the autologin ([`install_idle_dropin`]) and leave the DM
/// up. [`takeover_idled`] is the live marker; reading this as that marker skips the switch gate.
static STOPPED_DM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Mask left to lift on [`STOPPED_AUTOLOGIN`] units. Live takeovers idle instead of masking: a
/// masked unit fails, and a failing unit is the DM relogin-loop engine. True only for a takeover
/// adopted from a host that still masked. Unmasking a unit we never masked is a no-op; missing
/// one that is masked bars Game Mode until reboot.
static AUTOLOGIN_MASKED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Sentinel mtime at takeover. ChimeraOS-layout `os-session-select` writes
/// `~/.config/steamos-session-select` in its USER pass; that mtime is the only durable trace of
/// an in-stream "Switch to Desktop". Bazzite/SteamOS write none; [`is_steam_htpc_platform`]
/// follows the switch instead.
///
/// Two `Option`s, because the meanings invert:
/// * outer `None` — never baselined. A missing baseline treats an ancient write as a live request.
/// * `Some(None)` — no sentinel yet; a later file *is* a request.
/// * `Some(Some(t))` — anything newer than `t` is a request.
static SESSION_SELECT_BASELINE: std::sync::Mutex<Option<Option<std::time::SystemTime>>> =
    std::sync::Mutex::new(None);

/// When [`honor_session_select_switch`] last ran. While recent, refuse a managed relaunch:
/// gamescope+Steam come up faster than KWin, and a delivering pipeline ends re-detection.
static SWITCH_HONORED_AT: std::sync::Mutex<Option<Instant>> = std::sync::Mutex::new(None);

/// After an in-stream desktop switch, refuse managed relaunch until the DM session can come up.
const SWITCH_HONOR_GRACE: Duration = Duration::from_secs(120);

/// This host has an idle drop-in outstanding. Crash sweep: [`restore_takeover_on_startup`].
static IDLE_DROPIN_ARMED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Managed-route [`crate::panel_dpms`] hold for `Topology::Exclusive`.
///
/// Managed reports `SessionManaged`, so `registry::acquire` never picks up `take_topology_restore`.
/// Release lives in [`do_restore_tv_session`]. A bool, not a count: the session outlives connects,
/// and a per-connect acquire would pin the panel dark for the host's life.
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

fn managed_darken_acquire(exclusive: bool) {
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

/// Per-spawn id so two coexisting gamescopes never parse each other's log for a node id.
static SPAWN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// This spawn's log under [`crate::session::runtime_dir`]. Never `/tmp`: concurrent spawns must
/// not clobber each other's `stream available on node ID:` line, and `/tmp` is world-writable.
fn spawn_log_path(inst: u64) -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join(format!("punktfunk-gamescope-{inst}.log"))
}

const SESSION_UNIT: &str = "punktfunk-gamescope";
const SESSION_PLUS_BIN: &str = "/usr/share/gamescope-session-plus/gamescope-session-plus";

/// SteamOS session launcher (not Bazzite session-plus). `gamescope-session.service` execs
/// gamescope with hardcoded panel args. PATH-shim to `--backend headless -W <client> …` so
/// Steam starts inside that headless compositor.
const STEAMOS_SESSION_BIN: &str = "/usr/lib/steamos/gamescope-session";
const STEAMOS_SESSION_TARGET: &str = "gamescope-session.target";

/// SteamOS analogue of [`STOPPED_AUTOLOGIN`]: drop-in is in; restore must remove it and restart
/// the physical session.
static STEAMOS_TOOK_OVER: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Bind drop-in is on the box's own `gamescope-session-plus@` template. That path steals nothing,
/// so the other takeover statics stay empty. Skip this flag and the drop-in outlives the stream
/// and Game Mode runs our patched gamescope.
static SESSION_DROPIN_ARMED: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// This host pushed `SCREEN_WIDTH`/`SCREEN_HEIGHT`/`CUSTOM_REFRESH_RATES` into the user manager.
/// Those survive every unit restart for the rest of the login; restore may `unset-environment`
/// only values it set (an operator's own `set-environment` is theirs).
static FORCED_SESSION_SCREEN_ENV: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

/// Crash-restore snapshot of the takeover statics (`design/gamemode-and-dedicated-sessions.md`).
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

/// Best-effort crash-restore snapshot. Never call while holding any static it samples —
/// [`MANAGED_SESSION`] included. `std::sync::Mutex` is not reentrant.
fn persist_takeover() {
    let state = TakeoverState {
        stopped_autologin: STOPPED_AUTOLOGIN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        steamos: *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()),
        stopped_dm: STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        managed_session: MANAGED_SESSION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some(),
        forced_screen_env: *FORCED_SESSION_SCREEN_ENV
            .lock()
            .unwrap_or_else(|e| e.into_inner()),
    };
    if !takeover_state_is_live(&state) {
        clear_takeover();
        return;
    }
    if let Ok(bytes) = serde_json::to_vec(&state) {
        let _ = std::fs::write(takeover_state_path(), bytes);
    }
}

fn clear_takeover() {
    let _ = std::fs::remove_file(takeover_state_path());
}

/// File-side twin of [`takeover_live`]. Narrower by [`SESSION_DROPIN_ARMED`]: that drop-in is
/// swept unconditionally at startup, so persisting it would leave a restore with nothing to do.
/// [`FORCED_SESSION_SCREEN_ENV`] is the opposite — it lives in the user manager, nothing sweeps
/// it, and a crash must still know to unset it.
fn takeover_state_is_live(state: &TakeoverState) -> bool {
    !state.stopped_autologin.is_empty()
        || state.steamos
        || state.stopped_dm.is_some()
        || state.managed_session
        || state.forced_screen_env
}

/// Restart autologin units left `active` under a swept idle drop-in. Gated on a dark box
/// ([`box_session_live`]): bouncing a live game mode or desktop is the bug. Active under the
/// drop-in means "running the sleep".
fn hand_back_idled_units_after_crash() {
    if box_session_live() {
        return; // already drawing — the drop-in was inert
    }
    let units: Vec<String> = listed_autologin_units()
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
        if let RestoreVerb::Failed(why) = issue_restore_verb(&["restart", unit]) {
            tracing::error!(unit, status = %why, "gamescope: not restarted");
        }
    }
    ensure_box_session_or_escalate(&units);
}

/// Adopt a stranded takeover from a previous host and schedule restore after a reconnect grace.
/// Call once from `serve` with [`start_restore_worker`]. No-op when no file exists.
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
    // Removing the FILE does not restart the unit still sleeping under it; the takeover file may
    // be absent, so nothing below would. Hand the live idle unit back.
    if remove_idle_dropin() {
        tracing::warn!(
            "gamescope: removed a leftover idle drop-in from a previous host instance — the box's \
             own Game Mode session would have started and then done nothing"
        );
        hand_back_idled_units_after_crash();
    }
    let Ok(bytes) = std::fs::read(takeover_state_path()) else {
        return; // no takeover file — clean start
    };
    let Ok(state) = serde_json::from_slice::<TakeoverState>(&bytes) else {
        clear_takeover();
        return;
    };
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
    *AUTOLOGIN_MASKED.lock().unwrap_or_else(|e| e.into_inner()) =
        !state.stopped_autologin.is_empty();
    *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_autologin;
    *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()) = state.steamos;
    *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()) = state.stopped_dm;
    // Drop-in already swept above. SCREEN_* live in the user manager; only this flag authorises
    // [`unset_forced_session_screen_env`]. Adopting `false` is correct: the crashed host never
    // forced them, and unsetting an operator's values would be a bug.
    *FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = state.forced_screen_env;
    if state.managed_session {
        // Adopted session is something to STOP, never reuse: the launch mode is not persisted.
        // 0x0/0 Hz can never match `create_managed_session`, so every route relaunches, while
        // `takeover_live` still sees a session that owes a `stop`.
        *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
            width: 0,
            height: 0,
            refresh_hz: 0,
            hdr: false,
        });
    }
    // Launch-time baseline is gone; a long-existing sentinel must not read as a live switch.
    record_session_select_baseline();
    // 15 s: a client reconnecting right after restart cancels this and keeps the streamed session.
    *PENDING_RESTORE.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(Instant::now() + Duration::from_secs(15));
}

impl GamescopeDisplay {
    pub fn new() -> Result<Self> {
        Ok(GamescopeDisplay::default())
    }
}

impl VirtualDisplay for GamescopeDisplay {
    /// The trait calls this before every `create`, which is what lets the per-device
    /// topology be resolved from inside it (§6.1).
    fn set_client_identity(&mut self, fingerprint: Option<[u8; 32]>) {
        self.client_fp = fingerprint;
    }

    fn name(&self) -> &'static str {
        "gamescope"
    }

    fn set_launch_command(&mut self, cmd: Option<String>) {
        self.cmd = cmd;
    }

    fn set_hdr(&mut self, on: bool) {
        self.hdr = on;
    }

    fn hdr(&self) -> bool {
        // Reuse key: a kept SDR spawn has no HDR flags; handing it to HDR would negotiate PQ over SDR.
        self.hdr
    }

    fn set_gamescope_route(&mut self, route: Option<crate::GamescopeRoute>) {
        self.route = route;
    }

    fn set_session_isolation(&mut self, iso: Option<crate::SessionIsolation>) {
        self.isolation = iso;
    }

    fn isolation_key(&self) -> Option<String> {
        // Reuse key: a kept isolated spawn has this session's relay and Pulse env baked in.
        self.isolation.as_ref().map(|i| i.id.clone())
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        // Every spawn is its own group; cross-session ordering is `panel_dpms`'s refcount, not the group float.
        self.pending_restore.take()
    }

    fn poolable_now(&self) -> bool {
        // Must agree with what `create` does with the same route — not [`crate::launch_is_nested`],
        // which is `false` for `None`. `None` falls through to bare spawn, so it is poolable.
        matches!(self.route, None | Some(crate::GamescopeRoute::Spawn))
    }

    fn nested_launch_started(&self) -> bool {
        self.spawned_nested_launch
    }

    fn sole_instance(&self) -> bool {
        // Bare spawn only; managed/attach do not own the socket name. Same route test as
        // `poolable_now` — a second spawn is what breaks the `gamescope-N` lock and Steam.
        self.poolable_now()
    }

    fn kept_display_alive(&mut self, node_id: u32) -> bool {
        // Nested gamescope dies with its game. `false` makes the registry recreate instead of a ~10 s
        // first-frame retry on a dead node.
        gamescope_node_present(node_id)
    }

    fn set_join_live(&mut self, on: bool) {
        self.join_live = on;
    }

    fn join_live(&self) -> bool {
        self.join_live
    }

    fn last_join_name(&self) -> Option<crate::backend::JoinName> {
        self.join_name
            .clone()
            .map(|n| std::sync::Arc::new(std::sync::OnceLock::from(n)))
    }

    /// Gamescope publishes one PipeWire stream per spawn, so a joiner is its second consumer.
    fn join_cast(
        &mut self,
        _name: &str,
        node_id: u32,
    ) -> Result<Option<crate::backend::SessionCastParts>> {
        Ok(Some((node_id, None, Box::new(()))))
    }

    fn create(&mut self, mode: Mode) -> Result<VirtualOutput> {
        // This session's route — never the process env, or a second connect retargets this one.
        let (session_env, node_env) = match self.route.clone() {
            Some(crate::GamescopeRoute::Managed { client }) => (Some(client), None),
            Some(crate::GamescopeRoute::Attach { node }) => (None, Some(node)),
            Some(crate::GamescopeRoute::Spawn) => (None, None),
            None => (None, None), // no resolver on this path: bare spawn, the ladder default
        };
        // Sampled once so managed hold, exclusive session-free, and spawn darken cannot disagree.
        let exclusive =
            crate::effective_topology(self.client_fp) == crate::policy::Topology::Exclusive;
        if let Some(client) = session_env {
            // A joiner shares the running session. Relaunching it at another mode or HDR would
            // end the owner's stream.
            if self.join_live && !managed_session_matches(mode, self.hdr) {
                bail!(
                    "join the running gamescope session: it runs at another mode or HDR, and \
                     relaunching it would end the owner's stream"
                );
            }
            let out = create_managed_session(&client, mode, self.hdr)?;
            // Idling autologin leaves the CRTC configured. The hold cannot ride `pending_restore`:
            // this route is `SessionManaged`, so the registry never picks it up. Release is
            // [`do_restore_tv_session`].
            managed_darken_acquire(exclusive);
            return Ok(out);
        }
        if let Some(id) = node_env {
            let node_id: u32 = if id.trim().eq_ignore_ascii_case("auto") {
                // Headless box: game-mode resolution is ours. Skip and the client gets the box default.
                ensure_box_gamescope_mode(mode, self.hdr)?
            } else {
                id.parse()
                    .context("PUNKTFUNK_GAMESCOPE_NODE must be a node id or 'auto'")?
            };
            point_injector_at_eis();
            // Attach mirrors a gamescope that may be lighting the panel. Darkening it would
            // darken the picture being streamed. Exclusive cannot be served on this route.
            tracing::info!(node_id, "gamescope: attaching to existing PipeWire node");
            return Ok(VirtualOutput {
                node_id,
                remote_fd: None,
                preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
                keepalive: Box::new(()),
                ownership: DisplayOwnership::External,
                reused_gen: None,
                pool_gen: None,
                expect_exact_dims: false,
                output_name: None, // EIS seat, not a wlr virtual pointer to aim by name
                input_output: None,
                seat: None,
                pid: None,
            });
        }
        check_gamescope_version(); // diagnostic only — warns on known-deadlock-prone versions
                                   // Resolve once before the gate and hand the same answer to [`spawn`]. Gating on `self.cmd`
                                   // alone while spawn fell back to `PUNKTFUNK_GAMESCOPE_APP` would pass `--steam` with no instance free.
        let app = resolved_spawn_app(self.cmd.as_deref());
        let steam = app.as_deref().is_some_and(is_steam_launch);
        if steam {
            // No attach degrade here: a box without takeover privilege fails with the actionable error.
            stop_autologin_sessions()
                .context("dedicated Steam launch needs the box's gaming session freed")?;
            // Desktop Steam holds the instance too; autologin stop cannot see it.
            free_desktop_steam()?;
        } else if free_box_session_for_exclusive(steam, exclusive) {
            // Best-effort: on Game Mode the autologin session is DRM master, so Exclusive needs it
            // gone. A refusal costs the dark screen, not the game.
            if let Err(why) = stop_autologin_sessions() {
                tracing::warn!(
                    %why,
                    "exclusive topology: could not free the box's gaming session, so its own \
                     display keeps whatever it is showing for this stream"
                );
            }
        }
        let inst = SPAWN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log = spawn_log_path(inst);
        let child = spawn(
            mode.width,
            mode.height,
            mode.refresh_hz.max(1),
            app,
            &log,
            self.hdr,
            self.isolation.as_ref(),
        )?;
        let mut proc = GamescopeProc {
            child,
            log: log.clone(),
            relay: self
                .isolation
                .as_ref()
                .map(|i| i.ei_relay.clone())
                .unwrap_or_else(ei_socket_file),
        };
        // Give up early if the process is already gone: a `vkCreateDevice` failure exits in under
        // a second, and waiting 15 s on its corpse would blame the GPU.
        let node_id =
            wait_for_node(Duration::from_secs(15), &log, &mut proc.child).ok_or_else(|| {
                anyhow!(
                    "gamescope published no PipeWire node within 15s (or exited first) — it may \
                     have failed to start, or headless capture may be unsupported on this \
                     GPU/driver; its own log says which (see {})",
                    log.display()
                )
            })?;
        tracing::info!(
            node_id,
            w = mode.width,
            h = mode.height,
            hz = mode.refresh_hz,
            "gamescope virtual output ready"
        );
        // After spawn succeeds, so a failed create never blanks the screen. Refcounted in
        // `panel_dpms`: every spawn is its own group, so a group-float would re-light when the
        // first of two concurrent spawns ends. KWin refuses zero enabled outputs, so DPMS-off.
        if exclusive {
            crate::panel_dpms::acquire_stream_darken();
            self.pending_restore = Some(Box::new(crate::panel_dpms::release_stream_darken));
        }
        self.spawned_nested_launch = true;
        let pid = proc.child.id();
        let mut out = VirtualOutput::owned(
            node_id,
            Some((mode.width, mode.height, mode.refresh_hz)),
            Box::new(proc),
        );
        // From the same log `wait_for_node` just read. `None` only if gamescope changed the line;
        // every discovery then falls back to unscoped, which is what it did before seats.
        out.seat = wayland_name_from_log(&log);
        out.pid = Some(pid);
        self.join_name = out.seat.clone();
        tracing::info!(
            node_id,
            seat = out.seat.as_deref().unwrap_or("-"),
            "gamescope: seat key"
        );
        Ok(out)
    }
}

/// Host-managed session at the client's mode, state in [`MANAGED_SESSION`]. Reuse if mode and node
/// are live; otherwise relaunch — gamescope cannot change output mode live.
fn create_managed_session(client: &str, mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    // Not a bare `PENDING_RESTORE` clear: cancel also waits out a restore that already popped
    // (`keep_alive=off` is 0 s debounce).
    cancel_pending_restore();
    if steamos_session_present() {
        return create_managed_session_steamos(mode, hdr);
    }
    // Gated on the idled takeover, not [`STOPPED_DM`]: live takeovers leave the DM up, and that
    // static is what armed this. Skip and capture loss relaunches game mode over the booting desktop.
    if takeover_idled() && session_select_requested() {
        // Consume an adopted DM stop exactly once; a live takeover has none.
        let adopted_dm = std::mem::take(&mut *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()));
        honor_session_select_switch(adopted_dm);
        return Err(anyhow!(
            "the user switched the box to the desktop session — the box's own game mode is handed \
             back; re-detection follows the desktop compositor as it comes up"
        ));
    }
    // While the selected desktop boots, a managed relaunch wins the race (gamescope+Steam start
    // faster than KWin). A live autologin unit supersedes: the user already switched back.
    let honor_pending = SWITCH_HONORED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some_and(|t| t.elapsed() < SWITCH_HONOR_GRACE);
    if honor_pending {
        if running_autologin_gamescope_unit().is_some() {
            *SWITCH_HONORED_AT.lock().unwrap_or_else(|e| e.into_inner()) = None;
        } else {
            return Err(anyhow!(
                "waiting for the desktop session the user selected — refusing to relaunch game \
                 mode (re-detection follows the desktop once it's up)"
            ));
        }
    }
    // Never stop/relaunch here: post-capture-loss session detection can be stale.
    if crate::rebuild_probe_active() {
        // Don't hold MANAGED_SESSION across `pw-dump` / file write — that pins the restore worker.
        let same_mode = {
            let guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().is_some_and(|s| {
                s.width == mode.width
                    && s.height == mode.height
                    && s.refresh_hz == mode.refresh_hz
                    && s.hdr == hdr
            })
        };
        if same_mode {
            if let Some(node_id) = find_gamescope_node() {
                point_injector_at_eis();
                tracing::info!(
                    node_id,
                    "gamescope session: attach-only probe reusing live node"
                );
                return Ok(managed_output(node_id, mode));
            }
        }
        return Err(anyhow!(
            "gamescope session has no attachable live node — attach-only rebuild probe refuses \
             to stop/relaunch box sessions (re-detection follows the live session)"
        ));
    }
    // Autologin holds Steam and renders the TV's native mode. No privilege to stop the DM →
    // degrade to attach rather than destabilize the seat.
    if let Err(e) = stop_autologin_sessions() {
        tracing::warn!(
            error = %format!("{e:#}"),
            "gamescope: managed takeover unavailable — degrading to ATTACH (mirroring the box's \
             own game-mode session)"
        );
        let node_id = ensure_box_gamescope_mode(mode, hdr)?;
        point_injector_at_eis();
        return Ok(VirtualOutput {
            node_id,
            remote_fd: None,
            preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
            keepalive: Box::new(()),
            ownership: DisplayOwnership::External,
            reused_gen: None,
            pool_gen: None,
            expect_exact_dims: false,
            output_name: None, // EIS seat, not a wlr virtual pointer to aim by name
            input_output: None,
            seat: None,
            pid: None,
        });
    }
    // Desktop Steam also holds the instance; SESSION_UNIT's own Steam is exempt via cgroup.
    free_desktop_steam()?;
    // Decide under the lock, act outside it. Holding MANAGED_SESSION across `launch_session`
    // (~90 s) pins shutdown restore behind `native.rs`'s 20 s grace. [`MANAGED_LAUNCH`] is the
    // exclusion: held from before the decision so a second connect re-tests after the first
    // records, and touched by no restore path.
    let _launching = MANAGED_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = {
        let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
        let same = guard.as_ref().is_some_and(|s| {
            s.width == mode.width
                && s.height == mode.height
                && s.refresh_hz == mode.refresh_hz
                && s.hdr == hdr
        });
        // Mode change: drop the tracked session so a concurrent restore does not read it as live.
        // During launch a session that stole nothing is invisible to `takeover_live`; holding the
        // guard instead pins shutdown restore. Failure arms a restore; success re-records.
        if !same {
            *guard = None;
        }
        same
    };
    if same_mode {
        if let Some(node_id) = find_gamescope_node() {
            point_injector_at_eis();
            tracing::info!(
                node_id,
                w = mode.width,
                h = mode.height,
                hz = mode.refresh_hz,
                "gamescope session: reusing the running session (same mode — no Steam restart)"
            );
            return Ok(managed_output(node_id, mode));
        }
        tracing::warn!("gamescope session: tracked session has no live node — relaunching");
        *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    // Holding nothing: `launch_session` stops the old unit first, so discovery sees one node.
    let node_id = match launch_session(client, SESSION_UNIT, mode, hdr) {
        Ok(id) => id,
        Err(e) => {
            // Takeover already happened; arm restore or a failed launch leaves the box sessionless.
            schedule_restore_tv_session();
            return Err(e);
        }
    };
    // Only a write from inside this session should read as a switch, not the one that led here.
    record_session_select_baseline();
    point_injector_at_eis();
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
    });
    // After the guard dropped: `persist_takeover` samples the same mutex. A session that stole
    // nothing would otherwise write an empty state and delete the file.
    persist_takeover();
    tracing::info!(
        node_id,
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz,
        "gamescope session: launched gamescope-session-plus at the client's mode"
    );
    Ok(managed_output(node_id, mode))
}

/// Whether the tracked managed session runs at `mode` and `hdr`, so a `join` session can share it.
fn managed_session_matches(mode: Mode, hdr: bool) -> bool {
    MANAGED_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|s| {
            s.width == mode.width
                && s.height == mode.height
                && s.refresh_hz == mode.refresh_hz
                && s.hdr == hdr
        })
}

/// Box-level session: restore is this module's (`schedule_restore_tv_session`), so
/// [`DisplayOwnership::SessionManaged`] — the registry does not pool it.
fn managed_output(node_id: u32, mode: Mode) -> VirtualOutput {
    VirtualOutput {
        node_id,
        remote_fd: None,
        preferred_mode: Some((mode.width, mode.height, mode.refresh_hz)),
        keepalive: Box::new(()),
        ownership: DisplayOwnership::SessionManaged,
        reused_gen: None,
        pool_gen: None,
        expect_exact_dims: false,
        output_name: None, // EIS seat, not a wlr virtual pointer to aim by name
        input_output: None,
        seat: None,
        pid: None,
    }
}

/// SteamOS launcher present and Bazzite session-plus not: PATH-shim the Deck session, don't spawn
/// a separate unit.
fn steamos_session_present() -> bool {
    std::path::Path::new(STEAMOS_SESSION_BIN).exists()
        && !std::path::Path::new(SESSION_PLUS_BIN).exists()
}

/// Ladder defaults to managed only when this is true; otherwise bare-spawn, not a missing-script bail.
pub fn managed_session_available() -> bool {
    std::path::Path::new(SESSION_PLUS_BIN).exists()
        || std::path::Path::new(STEAMOS_SESSION_BIN).exists()
}

/// A gamescope we didn't spawn, for this uid. Our own bare-spawns are children of this process
/// (ppid walk), so one client's nested gamescope never makes the next client attach to it.
pub fn foreign_gamescope_running() -> bool {
    let uid = crate::proc::current_uid();
    let our_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let Ok(md) = std::fs::metadata(e.path()) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.uid() != uid {
            continue;
        }
        // Resolved name: nixpkgs wraps gamescope, so the kernel reports `.gamescope-wrap`.
        let Some(comm) = crate::proc::match_name(&e.path()) else {
            continue;
        };
        if !matches!(comm.as_str(), "gamescope" | "gamescope-wl") {
            continue;
        }
        if !descends_from(pid, our_pid) {
            return true;
        }
    }
    false
}

/// Walk `/proc/<pid>/stat` ppid. Hop cap so a racing/exiting process cannot loop us.
fn descends_from(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // Field 4 (ppid) follows parenthesized comm — split after the LAST ')' (comm may contain them).
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
            return false;
        };
        let Some(ppid) = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()) else {
            return false;
        };
        pid = ppid;
    }
    false
}

/// Run `cmd` inside the live session (managed / SteamOS / attach — [`spawn`]'s nesting does not
/// apply). Best-effort display env from a process already inside; without it, host env (a
/// `steam steam://…` still reaches the running Steam over its pipe).
pub fn launch_into_session(cmd: &str, seat: Option<&str>) -> Result<std::process::Child> {
    let mut c = Command::new("sh");
    c.arg("-c").arg(cmd);
    // Keeps AppImageLauncher's binfmt hook from replacing an .AppImage with its dialog.
    c.env("APPIMAGELAUNCHER_DISABLE", "1");
    match discover_session_display_env(seat) {
        Some((x11, wayland, _xauth)) => {
            tracing::info!(
                command = %cmd,
                x11_display = x11.as_deref().unwrap_or("-"),
                wayland = wayland.as_deref().unwrap_or("-"),
                "gamescope: launching into the live session"
            );
            if let Some(d) = x11 {
                c.env("DISPLAY", d);
            }
            if let Some(w) = wayland {
                c.env("WAYLAND_DISPLAY", w);
            }
        }
        None => tracing::warn!(
            command = %cmd,
            "gamescope: could not discover the session's display env — spawning with the host env \
             (a `steam steam://…` launch still reaches the running Steam; other apps may not land \
             in the session)"
        ),
    }
    c.spawn()
        .context("spawn launch command into gamescope session")
}

/// Every nested Xwayland `(DISPLAY, XAUTHORITY)` ONE seat exposes. Gaming Mode uses two
/// (`--xwayland-count`); the pointer lives on whichever is focused, so the XFixes source connects
/// to all of that seat's. `seat` is the instance's `GAMESCOPE_WAYLAND_DISPLAY`; `None` keeps every
/// gamescope, which is only right when the caller has no seat to be wrong about.
#[cfg(target_os = "linux")]
pub(crate) fn xwayland_cursor_targets(seat: Option<&str>) -> Vec<(String, Option<String>)> {
    let uid = crate::proc::current_uid();
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(md) = std::fs::metadata(e.path()) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.uid() != uid {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("environ")) else {
            continue;
        };
        let (mut display, mut is_gamescope, mut xauth) = (None, false, None);
        for kv in raw.split(|&b| b == 0) {
            let kv = String::from_utf8_lossy(kv);
            if let Some(v) = kv.strip_prefix("GAMESCOPE_WAYLAND_DISPLAY=") {
                // A sandboxed client rewrites this to a bind path (`/run/pressure-vessel/…`), so
                // it never matches a seat name. That is fine: the un-sandboxed members — the
                // wrapper, `steam.sh`, the splash — all carry the real name, and one is enough.
                is_gamescope = seat.is_none_or(|want| v == want);
            } else if let Some(v) = kv.strip_prefix("DISPLAY=") {
                if !v.is_empty() {
                    display = Some(v.to_string());
                }
            } else if let Some(v) = kv.strip_prefix("XAUTHORITY=") {
                if !v.is_empty() {
                    xauth = Some(v.to_string());
                }
            }
        }
        if let (true, Some(d)) = (is_gamescope, display) {
            // Distinct DISPLAY only; prefer the first non-empty XAUTHORITY seen for it.
            match out.iter_mut().find(|(dd, _)| *dd == d) {
                Some((_, xa)) if xa.is_none() => *xa = xauth,
                Some(_) => {}
                None => out.push((d, xauth)),
            }
        }
    }
    out
}

/// `(DISPLAY, WAYLAND_DISPLAY, XAUTHORITY)` from a same-uid process on `seat` — matched by its
/// `GAMESCOPE_WAYLAND_DISPLAY`. Any one can be absent. Without the key this took the first
/// gamescope in `/proc`, which on a multi-seat host launches into somebody else's session.
fn discover_session_display_env(
    seat: Option<&str>,
) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let uid = crate::proc::current_uid();
    for e in std::fs::read_dir("/proc").ok()?.flatten() {
        let name = e.file_name();
        let Some(pid_str) = name.to_str() else {
            continue;
        };
        if !pid_str.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(md) = std::fs::metadata(e.path()) else {
            continue;
        };
        use std::os::unix::fs::MetadataExt;
        if md.uid() != uid {
            continue;
        }
        let Ok(raw) = std::fs::read(e.path().join("environ")) else {
            continue;
        };
        let mut display = None;
        let mut gs_wayland = None;
        let mut xauth = None;
        for kv in raw.split(|&b| b == 0) {
            let kv = String::from_utf8_lossy(kv);
            if let Some(v) = kv.strip_prefix("GAMESCOPE_WAYLAND_DISPLAY=") {
                if seat.is_some_and(|want| v != want) {
                    continue;
                }
                if !v.is_empty() {
                    gs_wayland = Some(v.to_string());
                }
            } else if let Some(v) = kv.strip_prefix("DISPLAY=") {
                if !v.is_empty() {
                    display = Some(v.to_string());
                }
            } else if let Some(v) = kv.strip_prefix("XAUTHORITY=") {
                if !v.is_empty() {
                    xauth = Some(v.to_string());
                }
            }
        }
        if gs_wayland.is_some() {
            return Some((display, gs_wayland, xauth));
        }
    }
    None
}

/// In-memory `systemctl is-active` budget. Callers must time out into the safe answer (assume
/// active / keep looping). 300 ms is a manager-state read, not a D-Bus spawn.
const UNIT_STATE_BUDGET: Duration = Duration::from_millis(300);

/// Enumeration / linger write: walks loaded units and may go through polkit.
const UNIT_QUERY_BUDGET: Duration = Duration::from_secs(5);

/// User-manager lifecycle verb. Stop jobs wait on teardown; unbounded they pin the stream thread.
const UNIT_VERB_BUDGET: Duration = Duration::from_secs(10);

/// System-bus DM verb / session-switch helper. A premature kill abandons a half-done takeover;
/// timeout falls through to the pkexec helper.
const DM_VERB_BUDGET: Duration = Duration::from_secs(30);

/// Status-blind `systemctl --user` (callers fire-and-forget) but not time-blind: a wedged manager
/// must not pin the stream thread.
fn systemctl_user(args: &[&str]) {
    let _ = crate::proc::status_within(
        Command::new("systemctl").arg("--user").args(args),
        UNIT_VERB_BUDGET,
    );
}

fn headless_shim_dir() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gsbin")
}

/// Nested refresh: client's rate, capped by `PUNKTFUNK_MAX_FPS` (off by default). This is the
/// game's clamp, not the session — encode still repeats the held frame at the client's rate.
fn game_hz(session_hz: u32) -> u32 {
    pf_host_config::config().game_fps(session_hz).max(1)
}

/// PATH shim: rewrite SteamOS's hardcoded panel args to headless at `PF_W`/`PF_H`/`PF_HZ`.
/// `PF_HZ` is [`game_hz`] on `-r` only — it must not change the negotiated resolution.
fn write_headless_shim() -> Result<std::path::PathBuf> {
    // `$PF_HDR_ARGS` is unquoted for the same reason as in the GAMESCOPE_BIN wrapper: it is our
    // own flag list ([`hdr_args`]) and must word-split into separate argv entries.
    let shim_body = format!(
        r#"#!/bin/bash
W="${{PF_W:-1920}}"; H="${{PF_H:-1080}}"; HZ="${{PF_HZ:-60}}"
keep=()
while [ $# -gt 0 ]; do
  case "$1" in
    --generate-drm-mode|-w|-h|-W|-H|-O|--prefer-output) shift 2;;
    *) keep+=("$1"); shift;;
  esac
done
exec {bin} --backend headless -W "$W" -H "$H" -w "$W" -h "$H" -r "$HZ" ${{PF_HDR_ARGS}} "${{keep[@]}}"
"#,
        bin = gamescope_bin()
    );
    let dir = headless_shim_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let shim = dir.join("gamescope");
    std::fs::write(&shim, &shim_body).with_context(|| format!("write shim {}", shim.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod shim {}", shim.display()))?;
    Ok(dir)
}

/// `zz-` sorts last, overriding any distro drop-in.
fn steamos_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session.service.d/zz-punktfunk-headless.conf")
}

fn write_steamos_dropin(shim_dir: &std::path::Path, mode: Mode, hdr: bool) -> Result<()> {
    let path = steamos_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    // Stale desktop DISPLAY/WAYLAND_DISPLAY in the manager env would make gamescope attach instead
    // of becoming the display server.
    let body = format!(
        "[Service]\n\
         Environment=PATH={shim}:/usr/bin:/bin:/usr/local/bin\n\
         Environment=PF_W={w}\n\
         Environment=PF_H={h}\n\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {xkb}\
         UnsetEnvironment=DISPLAY WAYLAND_DISPLAY\n",
        shim = shim_dir.display(),
        xkb = xkb_unit_lines(),
        w = mode.width,
        h = mode.height,
        hz = game_hz(mode.refresh_hz),
        // Quoted: systemd `Environment=` with spaces otherwise keeps only the first flag.
        // SteamOS never reads `CUSTOM_REFRESH_RATES`; the shim only forwards `PF_HDR_ARGS`.
        hdr_args = hdr_args(hdr)
            .into_iter()
            .chain(cursor_args())
            .chain(adaptive_sync_args(game_hz(mode.refresh_hz)))
            // Advertised set vs `-r` = `PF_HZ` (frame-limited) — same split as `launch_session`.
            .chain(refresh_rate_args(mode.refresh_hz.max(1)))
            .collect::<Vec<_>>()
            .join(" "),
    );
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))
}

fn remove_steamos_dropin() {
    let _ = std::fs::remove_file(steamos_dropin_path());
}

/// Autologin-unit bind drop-in. Must live in `$XDG_RUNTIME_DIR`, not `$HOME`: it applies to the
/// whole `gamescope-session-plus@` template, and both paths it names are tmpfs. A `$HOME` copy
/// survives a reboot that deletes its sources, and a missing bind source fails Game Mode outright.
fn session_plus_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// `$HOME` copy of the bind drop-in: outlives the tmpfs paths it names. Swept on sight.
fn legacy_session_plus_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// Idle drop-in replaces Game Mode `ExecStart`. Runtime-dir so a dead host cannot leave Game Mode
/// as a sleep; [`restore_takeover_on_startup`] still sweeps it.
fn idle_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-idle.conf")
}

/// Idle `ExecStart` must actually execute: a unit that dies on start is the relogin storm.
fn sleep_binary() -> &'static str {
    ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("/usr/bin/sleep")
}

/// Idle autologin for the stream: replace `ExecStart` with sleep on the template. Steam is freed,
/// the autologin still succeeds (so the DM does not storm), and a user session-switch can still
/// be serviced — a stopped DM cannot.
fn install_idle_dropin() -> Result<()> {
    let path = idle_dropin_path();
    let dir = path
        .parent()
        .context("the idle drop-in path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, idle_dropin_body(sleep_binary()))
        .with_context(|| format!("write {}", path.display()))?;
    systemctl_user(&["daemon-reload"]);
    *IDLE_DROPIN_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = true;
    Ok(())
}

/// Empty `ExecStart=` first: the directive is list-valued, so an add-only drop-in would run the
/// real session *and* the sleep.
fn idle_dropin_body(sleep_bin: &str) -> String {
    format!("[Service]\nExecStart=\nExecStart={sleep_bin} infinity\n")
}

/// Live managed-takeover marker (arms the in-stream switch gate). Process memory, not disk: a
/// drop-in we did not write belongs to a dead host.
fn takeover_idled() -> bool {
    *IDLE_DROPIN_ARMED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Not gated on [`IDLE_DROPIN_ARMED`]: a drop-in that outlived a dead host still has to be swept.
fn remove_idle_dropin() -> bool {
    let removed = std::fs::remove_file(idle_dropin_path()).is_ok();
    *IDLE_DROPIN_ARMED.lock().unwrap_or_else(|e| e.into_inner()) = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
    removed
}

/// Box-session drop-in: bind + WSI opt-out. No bind to arm → remove any drop-in (`Ok(false)`);
/// keeping a bind the host decided against is the crash-loop the backstop exists to prevent.
fn write_session_plus_dropin(
    wrapper: &std::path::Path,
    mode: Mode,
    hdr: bool,
    wsi: WsiPlan,
) -> Result<bool> {
    let Some(bind) = arm_session_bind(wrapper) else {
        remove_session_plus_dropin();
        return Ok(false);
    };
    let path = session_plus_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = format!(
        "[Service]\n\
         {binds}\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {xkb}\
         {wsi}",
        binds = bind.unit_lines(),
        xkb = xkb_unit_lines(),
        hz = game_hz(mode.refresh_hz),
        hdr_args = hdr_args(hdr)
            .into_iter()
            .chain(cursor_args())
            .chain(adaptive_sync_args(game_hz(mode.refresh_hz)))
            .collect::<Vec<_>>()
            .join(" "),
        wsi = wsi.unit_lines(),
    );
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))?;
    Ok(true)
}

/// Both homes: runtime and [`legacy_session_plus_dropin_path`]. Caller owes `daemon-reload` if
/// anything was removed — a removal that isn't reloaded still applies at next boot.
fn remove_session_plus_dropin() -> bool {
    // Both paths every time; short-circuit would leave the `$HOME` copy that outlives a reboot.
    let mut removed = false;
    for path in [
        session_plus_dropin_path(),
        legacy_session_plus_dropin_path(),
    ] {
        removed |= std::fs::remove_file(&path).is_ok();
    }
    removed
}

/// Remove + clear [`SESSION_DROPIN_ARMED`] + `daemon-reload`, so the flag and the template cannot disagree.
fn disarm_session_plus_dropin() {
    let removed = remove_session_plus_dropin();
    *SESSION_DROPIN_ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
}

/// No-op unless we set them: `unset-environment` is indiscriminate.
fn unset_forced_session_screen_env() {
    let mut forced = FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if !*forced {
        return;
    }
    *forced = false;
    drop(forced);
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

/// SteamOS: PATH-shim + drop-in, restart `gamescope-session.target`. Restart kills any prior
/// gamescope, so discovery sees one node. Same-mode reconnect reuses.
fn create_managed_session_steamos(mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    let mut guard = MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = guard.as_ref().is_some_and(|s| {
        s.width == mode.width
            && s.height == mode.height
            && s.refresh_hz == mode.refresh_hz
            && s.hdr == hdr
    });
    if same_mode {
        if let Some(node_id) = find_gamescope_node() {
            point_injector_at_eis();
            tracing::info!(
                node_id,
                w = mode.width,
                h = mode.height,
                hz = mode.refresh_hz,
                "gamescope (SteamOS): reusing the headless session (same mode — no Steam restart)"
            );
            return Ok(managed_output(node_id, mode));
        }
        *guard = None; // tracked session lost its node — fall through to a clean restart
    }
    // Reuse may attach; restarting the target would steal the seat from a session the user switched to.
    if crate::rebuild_probe_active() {
        return Err(anyhow!(
            "gamescope has no live node and this is an attach-only rebuild probe — refusing to \
             restart {STEAMOS_SESSION_TARGET} (the box may be mid-switch to another session; \
             re-detection follows it)"
        ));
    }
    let shim_dir = write_headless_shim()?;
    write_steamos_dropin(&shim_dir, mode, hdr)?;
    systemctl_user(&["daemon-reload"]);
    systemctl_user(&["restart", STEAMOS_SESSION_TARGET]);
    // LOCK ORDER: restore takes STEAMOS_TOOK_OVER then MANAGED_SESSION. Reverse here is AB/BA
    // with the restore worker. Nothing below reads the tracked session.
    drop(guard);
    *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner()) = true;
    persist_takeover();
    // Takeover already happened; a bare `?` would leave the box headless with PENDING_RESTORE unset.
    let node_id = match poll_managed_node(Duration::from_secs(30)) {
        Some(id) => id,
        None => {
            schedule_restore_tv_session();
            bail!(
                "SteamOS headless gamescope node did not appear within 30s after restarting \
                 {STEAMOS_SESSION_TARGET} — check `journalctl --user -u gamescope-session.service`"
            );
        }
    };
    // Stock gamescope here means no HDR and a silently pointerless stream. Leave tracked state
    // unset on failure so the retry restarts rather than reusing what we rejected.
    if let Err(e) = verify_managed_spawn_flags(hdr) {
        schedule_restore_tv_session();
        return Err(e);
    }
    point_injector_at_eis();
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
    });
    persist_takeover();
    tracing::info!(
        node_id,
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz,
        "gamescope (SteamOS): took over gamescope-session.target headless at the client's mode"
    );
    Ok(managed_output(node_id, mode))
}

/// Attach at the client's resolution: reuse if the box session already matches; otherwise restart
/// the box's own autologin unit. Never spawn a competing one. Steam restarts only on a real change.
fn ensure_box_gamescope_mode(mode: Mode, hdr: bool) -> Result<u32> {
    let target = (mode.width, mode.height);
    // Three-state: collapsing unknown with a known size would restart the box's session.
    let size = box_output_size();
    if size == BoxOutputSize::Known(target) {
        if let Some(node) = find_gamescope_node() {
            tracing::info!(
                w = mode.width,
                h = mode.height,
                node,
                "gamescope: box game-mode session already at the client's resolution — reusing"
            );
            return Ok(node);
        }
    }
    // Post-capture-loss detection can be stale; a restart would fight the session the user switched to.
    if crate::rebuild_probe_active() {
        if let Some(node) = find_gamescope_node() {
            tracing::info!(
                node,
                "gamescope: attach-only rebuild probe — mirroring the live node at its own mode"
            );
            return Ok(node);
        }
        return Err(anyhow!(
            "no live gamescope node — attach-only rebuild probe refuses to restart the box's \
             session (re-detection follows the live session)"
        ));
    }
    // Physical display: mirror at its own mode. Guard the decision, not the node lookup — a
    // momentarily absent node must refuse, not fall through into `set-environment` + restart.
    if physical_display_connected() {
        let node = find_gamescope_node().ok_or_else(|| {
            anyhow!(
                "the box drives a physical display, so its game-mode session is mirrored at its \
                 OWN mode — and it publishes no gamescope Video/Source node right now. Refusing to \
                 re-mode it to {}x{}: that would flip the screen someone is looking at and, on a \
                 DM-driven box, bounce the login session with it",
                mode.width,
                mode.height
            )
        })?;
        tracing::info!(
            node,
            client_w = mode.width,
            client_h = mode.height,
            "gamescope: box drives a physical display — attaching at its own mode (no re-mode)"
        );
        return Ok(node);
    }
    // Two gamescopes, different sizes: cannot say which the session unit owns. Restarting would
    // kill a nested per-title game that may already be at the client resolution. Mirror instead.
    if size == BoxOutputSize::Ambiguous {
        let node = find_gamescope_node().ok_or_else(|| {
            anyhow!(
                "two gamescopes are running at different output sizes and neither publishes a \
                 Video/Source node right now — refusing to re-mode the box's session to {}x{} \
                 without knowing which one it is (re-detection follows the live session)",
                mode.width,
                mode.height
            )
        })?;
        tracing::warn!(
            node,
            client_w = mode.width,
            client_h = mode.height,
            "gamescope: two coexisting gamescopes disagree on the output size (a game nested in the \
             session is the usual cause) — attaching at the live node's own mode instead of \
             restarting the box's session under it"
        );
        return Ok(node);
    }
    let Some(unit) = running_autologin_gamescope_unit() else {
        return find_gamescope_node().ok_or_else(|| {
            anyhow!(
                "no running gamescope Video/Source node — is the headless game mode up? \
                 (put the box into Steam Game Mode)"
            )
        });
    };
    tracing::info!(
        from = ?size,
        to_w = mode.width,
        to_h = mode.height,
        hz = mode.refresh_hz,
        %unit,
        "gamescope: relaunching the box game-mode session at the client's resolution"
    );
    // Manager keeps these for the rest of the login; restore owes [`unset_forced_session_screen_env`].
    *FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = true;
    systemctl_user(&[
        "set-environment",
        &format!("SCREEN_WIDTH={}", mode.width),
        &format!("SCREEN_HEIGHT={}", mode.height),
        &format!("CUSTOM_REFRESH_RATES={}", mode.refresh_hz.max(1)),
    ]);
    persist_takeover(); // no static held; these SCREEN_* outlive the process
    let mut bound = match write_gamescope_bin_wrapper()
        .and_then(|w| write_session_plus_dropin(&w, mode, hdr, WsiPlan::resolve()))
    {
        Ok(true) => {
            // Before the restart: skip this flag and `takeover_live()` is false, so the drop-in outlives us.
            *SESSION_DROPIN_ARMED
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = true;
            tracing::info!(
                bin = %gamescope_bin(),
                %unit,
                "gamescope: dropped in a bind over {DISTRO_GAMESCOPE_PATH} for the box's own \
                 session unit — a session script that hardcodes that path (Nobara) gets the \
                 patched build on this restart too"
            );
            true
        }
        Ok(false) => {
            // No bind to arm also removes; the flag must follow or restore owes a gone drop-in.
            *SESSION_DROPIN_ARMED
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = false;
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "gamescope: box-session drop-in not written");
            false
        }
    };
    // Also reloads a removal: a drop-in systemd has not reloaded still applies at next start.
    systemctl_user(&["daemon-reload"]);
    systemctl_user(&["restart", &unit]);
    let mut deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if any_output_size_is(&gamescope_argvs(), target) {
            if let Some(node) = find_gamescope_node() {
                tracing::info!(
                    node,
                    w = mode.width,
                    h = mode.height,
                    "gamescope: box game-mode session relaunched at the client's resolution"
                );
                return Ok(node);
            }
        }
        if Instant::now() >= deadline {
            // Bind killing the box's own session hands the seat to the desktop. One more try without it.
            if bound {
                note_bind_hazard(&unit);
                disarm_session_plus_dropin(); // also clears the flag: there is nothing left to undo
                systemctl_user(&["restart", &unit]);
                bound = false;
                deadline = Instant::now() + Duration::from_secs(45);
                continue;
            }
            bail!(
                "box game-mode session did not come up at {}x{} within 45s after relaunch \
                 (Steam may still be booting)",
                mode.width,
                mode.height
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Compositor argv from `/proc/<pid>/cmdline`. Basename `ends_with("gamescope")` — `/proc/…/exe`
/// is often unreadable, and `==` would miss `punktfunk-gamescope` while still excluding helpers.
fn gamescope_argvs() -> Vec<Vec<String>> {
    let mut found = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str() else { continue };
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if args
            .first()
            .is_some_and(|a0| a0.rsplit('/').next().unwrap_or(a0).ends_with("gamescope"))
        {
            found.push(args);
        }
    }
    found
}

/// `-W`/`-H` of one argv. `None` if either is missing — also the compositor vs helper filter.
fn gamescope_output_size(argv: &[String]) -> Option<(u32, u32)> {
    match (
        argv_u32(argv, &["-W", "--output-width"]),
        argv_u32(argv, &["-H", "--output-height"]),
    ) {
        (Some(w), Some(h)) => Some((w, h)),
        _ => None,
    }
}

/// Three states: Game Mode routinely runs a session compositor plus a nested per-title gamescope.
/// Collapsing unknown with a different size would restart the box unit and kill the running game.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoxOutputSize {
    /// Unanimous `-W`/`-H`. The only state a caller may act on.
    Known((u32, u32)),
    /// No output size reported. Re-mode may proceed: there is no second opinion to be wrong about.
    Unreported,
    /// Disagreeing sizes. Callers take the non-destructive branch; we do not guess via ppid.
    Ambiguous,
}

fn box_output_size() -> BoxOutputSize {
    classify_output_size(&gamescope_argvs())
}

/// Agreed size, or `None`. Sound only for the libei hint (unknown → raw client pixels). Anything
/// that would act on Unreported vs Ambiguous must call [`box_output_size`].
fn current_gamescope_output_size() -> Option<(u32, u32)> {
    match box_output_size() {
        BoxOutputSize::Known(size) => Some(size),
        BoxOutputSize::Unreported | BoxOutputSize::Ambiguous => None,
    }
}

fn classify_output_size(argvs: &[Vec<String>]) -> BoxOutputSize {
    let mut agreed: Option<(u32, u32)> = None;
    for argv in argvs {
        let Some(size) = gamescope_output_size(argv) else {
            continue;
        };
        match agreed {
            None => agreed = Some(size),
            Some(seen) if seen == size => {}
            Some(seen) => {
                tracing::debug!(
                    ?seen,
                    ?size,
                    "gamescope: two coexisting gamescopes report different output sizes — \
                     answering 'ambiguous' rather than picking one of them"
                );
                return BoxOutputSize::Ambiguous;
            }
        }
    }
    match agreed {
        Some(size) => BoxOutputSize::Known(size),
        None => BoxOutputSize::Unreported,
    }
}

/// After a restart: did `target` come up? Unanimity is wrong — a kept bare spawn at another size
/// would hold `Ambiguous` for the whole wait.
fn any_output_size_is(argvs: &[Vec<String>], target: (u32, u32)) -> bool {
    argvs
        .iter()
        .any(|argv| gamescope_output_size(argv) == Some(target))
}

fn argv_u32(argv: &[String], names: &[&str]) -> Option<u32> {
    argv.iter().enumerate().find_map(|(i, a)| {
        names
            .contains(&a.as_str())
            .then(|| argv.get(i + 1).and_then(|v| v.parse().ok()))
            .flatten()
    })
}

/// Headless `--nested-refresh` is the session's only refresh (defaults to 60 Hz). The wrapper can
/// lose it; refusing would loop (same env). Warn and carry on. Silent when `/proc` cannot be read.
fn warn_if_mode_lost(mode: Mode, want_hz: u32) {
    let argvs = gamescope_argvs();
    let lost = mode_mismatch(mode.width, mode.height, want_hz, &argvs);
    if lost.is_empty() {
        return;
    }
    tracing::warn!(
        lost = %lost.join(", "),
        "gamescope: the session did not start at the mode we asked for — the session script \
         dropped GAMESCOPE_BIN / SCREEN_WIDTH / SCREEN_HEIGHT. A headless gamescope reports \
         `--nested-refresh` as its ONE refresh rate (60 Hz when the flag never arrives), so games \
         and Steam will believe the display runs at that rate however fast the stream is. Install \
         punktfunk-gamescope, or check /etc/gamescope-session-plus/sessions.d/ for a file that \
         overrides GAMESCOPE_BIN or sets GAMESCOPECMD"
    );
}

/// Fail-open like [`missing_flags`]: empty argvs means we could not look.
fn mode_mismatch(want_w: u32, want_h: u32, want_hz: u32, argvs: &[Vec<String>]) -> Vec<String> {
    if argvs.is_empty() {
        return Vec::new();
    }
    let mut lost = Vec::new();
    let sizes: Vec<(u32, u32)> = argvs
        .iter()
        .filter_map(|a| {
            Some((
                argv_u32(a, &["-W", "--output-width"])?,
                argv_u32(a, &["-H", "--output-height"])?,
            ))
        })
        .collect();
    // No output size at all: cannot tell ours from a nested one — stay quiet.
    if !sizes.is_empty() && !sizes.contains(&(want_w, want_h)) {
        lost.push(format!(
            "resolution asked={want_w}x{want_h}, got={}",
            sizes
                .iter()
                .map(|(w, h)| format!("{w}x{h}"))
                .collect::<Vec<_>>()
                .join("/")
        ));
    }
    let rates: Vec<u32> = argvs
        .iter()
        .filter_map(|a| argv_u32(a, &["-r", "--nested-refresh"]))
        .collect();
    if !rates.contains(&want_hz) {
        lost.push(match rates.as_slice() {
            [] => format!(
                "refresh asked={want_hz}Hz, got=no --nested-refresh at all (gamescope defaults to \
                 60Hz headless)"
            ),
            got => format!(
                "refresh asked={want_hz}Hz, got={}Hz",
                got.iter().map(u32::to_string).collect::<Vec<_>>().join("/")
            ),
        });
    }
    lost
}

/// Managed modes can lose flags (session script / PATH shim). A lost cursor flag is silent: the
/// host was told the compositor would paint the pointer. Latch off ([`note_spawn_flags_lost`]) and
/// refuse; the retry plans host-composited SDR. Fail open if we cannot look. Any one gamescope
/// carrying the flags is enough — demanding every one would reject a good session beside a nested.
fn verify_managed_spawn_flags(hdr: bool) -> Result<()> {
    let expected: Vec<String> = hdr_args(hdr)
        .into_iter()
        .chain(cursor_args())
        // The rate value is a placeholder: the filter below keeps flag NAMES only, and
        // `--adaptive-sync` is what proves the VRR half of the plan reached the compositor.
        .chain(adaptive_sync_args(1))
        .filter(|a| a.starts_with("--")) // flag names only — their values are bare words
        .collect();
    if expected.is_empty() {
        return Ok(());
    }
    let missing = missing_flags(&expected, &gamescope_argvs());
    if missing.is_empty() {
        tracing::debug!(flags = ?expected, "gamescope: the session's compositor carries our flags");
        return Ok(());
    }
    note_spawn_flags_lost();
    // Warn as well as erroring: the latch is a process-wide capability change, and whichever
    // caller consumes this error decides on its own how loudly to report it.
    tracing::warn!(
        missing = %missing.join(" "),
        "gamescope: the session ignored GAMESCOPE_BIN / the PATH shim and ran a stock gamescope — \
         HDR and the in-node cursor are now off for this host process"
    );
    Err(anyhow!(
        "the gamescope session started without {} — it ignored GAMESCOPE_BIN / the PATH shim and \
         ran a stock gamescope. Refusing it rather than streaming a session whose shape was \
         planned around flags that never arrived (a missing cursor flag has no symptom but an \
         absent pointer). Those capabilities are off for this host now; reconnect for a plain SDR \
         session, or install punktfunk-gamescope as the box's `gamescope`",
        missing.join(" ")
    ))
}

/// Empty `argvs` = could not look (silence). Empty result after looking = fine. Opposite meanings.
fn missing_flags<'a>(expected: &'a [String], argvs: &[Vec<String>]) -> Vec<&'a str> {
    if argvs.is_empty() {
        return Vec::new();
    }
    expected
        .iter()
        .filter(|f| !argvs.iter().any(|argv| argv.iter().any(|a| a == *f)))
        .map(String::as_str)
        .collect()
}

fn running_autologin_gamescope_unit() -> Option<String> {
    let out = crate::proc::output_within(
        Command::new("systemctl").args([
            "--user",
            "list-units",
            "--type=service",
            "--state=running",
            "--no-legend",
            "--plain",
            "gamescope-session-plus@*.service",
        ]),
        UNIT_QUERY_BUDGET,
    )
    .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .find(|u| u.starts_with("gamescope-session-plus@") && u.ends_with(".service"))
        .map(|u| u.to_string())
}

/// SIGKILL, not SIGTERM: gamescope's SIGTERM handler leaks the NVIDIA GPU context, after which
/// every later `vkCreateDevice` fails until reboot. Then `stop` + `reset-failed` so relaunch is clean.
fn kill_unit(unit: &str) {
    // All three budgeted: this runs on disconnect restore and on shutdown, where the whole
    // sequence has ~20 s before `native.rs` gives up. Three unbounded `systemctl` calls against
    // a busy user manager spend that budget on their own.
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "kill", "--signal=SIGKILL", unit]),
        UNIT_VERB_BUDGET,
    );
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "stop", unit]),
        UNIT_VERB_BUDGET,
    );
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "reset-failed", unit]),
        UNIT_VERB_BUDGET,
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
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "mask", "--runtime", unit]),
        UNIT_VERB_BUDGET,
    );
}

/// Every restore path must unmask before restarting, or Game Mode stays broken until reboot.
fn unmask_unit(unit: &str) {
    let _ = crate::proc::status_within(
        Command::new("systemctl").args(["--user", "unmask", "--runtime", unit]),
        UNIT_VERB_BUDGET,
    );
}

/// Idempotent. Does not consume [`STOPPED_AUTOLOGIN`]: mask lifetime is shorter than the takeover;
/// restore still owes that list a start.
fn lift_autologin_mask() {
    let mut masked = AUTOLOGIN_MASKED.lock().unwrap_or_else(|e| e.into_inner());
    if !*masked {
        return;
    }
    *masked = false;
    let units = STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner());
    for unit in units.iter() {
        unmask_unit(unit);
    }
    tracing::info!(
        units = ?*units,
        "gamescope: lifted the takeover's runtime mask — the box can enter its own game mode again"
    );
}

/// Only a desktop switch ends the mask window. Gaming is our own session; None is a relaunch gap.
fn switch_ends_mask_window(kind: super::ActiveKind) -> bool {
    use super::ActiveKind;
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
pub fn release_autologin_mask(switched_to: super::ActiveKind) {
    if !switch_ends_mask_window(switched_to) {
        return;
    }
    lift_autologin_mask();
    // Not [`clear_takeover`]: restore still owes [`STOPPED_AUTOLOGIN`] a start. Left on, "Return
    // to Gaming Mode" starts a unit that only sleeps.
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
         connect. Fix it once with `sudo usermod -aG {group} {user}`, then log out and back in — \
         a `systemd --user` session keeps the group set it started with, and the same group gates \
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
fn record_session_select_baseline() {
    *SESSION_SELECT_BASELINE
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(session_select_mtime());
}

fn session_select_requested() -> bool {
    let baseline = *SESSION_SELECT_BASELINE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
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
fn honor_session_select_switch(adopted_dm: Option<String>) {
    tracing::info!(
        adopted_dm = ?adopted_dm,
        "gamescope: in-stream session-select detected — handing the box's own game mode back and \
         following the desktop session the user selected"
    );
    // Mask first, while the unit list still exists — this path discards that list.
    lift_autologin_mask();
    std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    clear_takeover();
    *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
    *SWITCH_HONORED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
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
        match crate::proc::status_within(
            Command::new(OS_SESSION_SELECT).arg("desktop"),
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
fn stop_autologin_sessions() -> Result<()> {
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
    if *IDLE_DROPIN_ARMED.lock().unwrap_or_else(|e| e.into_inner()) {
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
    *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()) = stopped;
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

/// Steam tears down a running game (Proton included) on the way out.
const STEAM_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// Half of [`STEAM_SHUTDOWN_WAIT`]. `/usr/bin/steam` is `steam.sh`, not a thin IPC forwarder;
/// `status_within` killpg's the group, so a 5 s bound kills the request before it leaves.
const STEAM_SHUTDOWN_SEND_BUDGET: Duration = Duration::from_secs(STEAM_SHUTDOWN_WAIT.as_secs() / 2);

/// Desktop Steam holds the single instance; autologin stop cannot see it. Ours (host tree /
/// SESSION_UNIT) are exempt. Timeout is an actionable error, not a no-frames retry loop.
fn free_desktop_steam() -> Result<()> {
    let Some(pid) = desktop_steam_pid() else {
        return Ok(());
    };
    tracing::info!(
        pid,
        "freeing Steam: a desktop-session Steam holds the single instance — sending `steam -shutdown`"
    );
    // Reaped: dropping Child does not wait, and the loop below polls the TARGET, not this helper.
    let _ = crate::proc::status_within(
        Command::new("steam")
            .arg("-shutdown")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        STEAM_SHUTDOWN_SEND_BUDGET,
    );
    let deadline = Instant::now() + STEAM_SHUTDOWN_WAIT;
    while Instant::now() < deadline {
        if !pid_running(pid) {
            tracing::info!(pid, "desktop Steam exited — single instance free");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    bail!(
        "Steam is already running in the host's desktop session (pid {pid}) and did not exit \
         within {}s of `steam -shutdown` — close Steam on the host, then launch again",
        STEAM_SHUTDOWN_WAIT.as_secs()
    )
}

/// Desktop Steam via `~/.steam/steam.pid`. `None` if stale, our descendant, or in SESSION_UNIT.
fn desktop_steam_pid() -> Option<u32> {
    let home = std::env::var("HOME").ok()?;
    let pid = std::fs::read_to_string(format!("{home}/.steam/steam.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    // Steam's own processes report comm `steam` (the ubuntu12_32 binary) or `steam.sh`; anything
    // else means the pid was recycled since Steam last ran.
    if !matches!(comm.trim(), "steam" | "steam.sh") || !pid_running(pid) {
        return None;
    }
    if descends_from(pid, std::process::id()) {
        return None; // our own dedicated spawn's Steam
    }
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    if cgroup_is_punktfunk_owned(&cgroup) {
        return None; // the host service's tree or the managed session unit
    }
    Some(pid)
}

fn cgroup_is_punktfunk_owned(cgroup: &str) -> bool {
    cgroup.contains("punktfunk-host.service") || cgroup.contains(&format!("{SESSION_UNIT}.service"))
}

/// A zombie keeps `/proc` but has already released Steam; waiting would burn the full deadline.
fn pid_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // Field 3 (state) follows the parenthesized comm — split after the LAST ')' since comm can
    // itself contain parentheses.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| state != "Z")
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

/// True while any takeover static is live. One lock at a time: a `||` chain of `.lock()`
/// temporaries lives to the end of the statement.
fn takeover_live() -> bool {
    let autologin = !STOPPED_AUTOLOGIN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    let steamos = *STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner());
    let dm = STOPPED_DM
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    // Attach re-mode steals nothing, but it did rewrite the template and pin SCREEN_*.
    let dropin = *SESSION_DROPIN_ARMED
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let screen_env = *FORCED_SESSION_SCREEN_ENV
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    autologin
        || steamos
        || dm
        || dropin
        || screen_env
        // Managed session beside a live desktop still owns SESSION_UNIT; restore must stop it.
        || MANAGED_SESSION
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
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
fn physical_display_connected() -> bool {
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
    super::detect_active_session().kind != super::ActiveKind::None
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
    // Only release for managed Exclusive: SessionManaged never rides `take_topology_restore`.
    // Above every early return. Idempotent.
    managed_darken_release();
    // SteamOS restore: remove drop-in + restart the target, unless a desktop is already up.
    {
        let mut took = STEAMOS_TOOK_OVER.lock().unwrap_or_else(|e| e.into_inner());
        if *took {
            // No panel: restarting the target crash-loops gamescope. Keep headless. Check at
            // restore time so plugging a panel in later restores.
            if !physical_display_connected() {
                tracing::info!(
                    "gamescope (SteamOS): no physical display connected — keeping the headless \
                     session (nothing to restore to)"
                );
                return;
            }
            *took = false;
            *MANAGED_SESSION.lock().unwrap_or_else(|e| e.into_inner()) = None;
            remove_steamos_dropin();
            systemctl_user(&["daemon-reload"]);
            use super::ActiveKind;
            if matches!(
                super::detect_active_session().kind,
                ActiveKind::DesktopKde
                    | ActiveKind::DesktopGnome
                    | ActiveKind::DesktopWlroots
                    | ActiveKind::DesktopHyprland
            ) {
                tracing::info!(
                    "gamescope (SteamOS): a desktop session is active — removed the headless \
                     override, not restarting the gaming session"
                );
                clear_takeover();
                return;
            }
            match issue_restore_verb(&["restart", STEAMOS_SESSION_TARGET]) {
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
    let units = std::mem::take(&mut *STOPPED_AUTOLOGIN.lock().unwrap_or_else(|e| e.into_inner()));
    let dm = std::mem::take(&mut *STOPPED_DM.lock().unwrap_or_else(|e| e.into_inner()));
    // Don't hold MANAGED_SESSION across the work below — launch can hold it ~90 s.
    let managed_was_running = MANAGED_SESSION
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .is_some();
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
    use super::ActiveKind;
    if matches!(
        super::detect_active_session().kind,
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

/// Point the libei injector at the running gamescope's EIS socket (it reads the relay file
/// [`ei_socket_file`]). Best-effort — video still works without it (input just won't reach the
/// session). Shared by the attach and host-managed-session paths.
fn point_injector_at_eis() {
    match find_gamescope_eis_socket() {
        Some(sock) => {
            // Line 2 is WxH: EIS advertises INT32_MAX, so the injector cannot learn geometry.
            // Socket and size come from different sources; omit the hint unless every gamescope agrees.
            let size = current_gamescope_output_size();
            let body = match size {
                Some((w, h)) => format!("{sock}\n{w}x{h}"),
                None => sock.clone(),
            };
            match std::fs::write(ei_socket_file(), body) {
                Ok(()) => {
                    tracing::info!(
                        socket = %sock,
                        output = ?size,
                        "gamescope: pointed injector at the session's EIS socket"
                    )
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "gamescope: EIS relay file not written — input may not reach the session"
                ),
            }
        }
        None => tracing::warn!(
            "gamescope: no connectable gamescope EIS socket found — input won't reach the session"
        ),
    }
    sync_session_keyboard_layout();
}

/// Explicit-off kill switch for [`sync_session_keyboard_layout`].
const LAYOUT_SYNC_ENV: &str = "PUNKTFUNK_SESSION_LAYOUT";
/// `setxkbmap` talks to a local X server; anything slower than this is a server that is not
/// answering, and the connecting client is waiting on us.
const LAYOUT_SYNC_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Attach path: autologin Xwayland is born `us`; [`xkb_env`] never reaches it. Xwayland only —
/// Wayland-native clients take the compositor keymap (`+pfhdr8`). Off via `PUNKTFUNK_SESSION_LAYOUT`.
fn sync_session_keyboard_layout() {
    if pf_host_config::env_on(LAYOUT_SYNC_ENV) == Some(false) {
        return;
    }
    let resolved = pf_host_config::layout::system_layout();
    let Some(layout) = resolved.names.layout.as_deref() else {
        return;
    };
    let targets = xwayland_cursor_targets(None);
    if targets.is_empty() {
        return;
    }
    let non_empty = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    for (dpy, xauth) in targets {
        let mut cmd = Command::new("setxkbmap");
        cmd.args(["-display", &dpy, "-layout", layout]);
        if let Some(v) = non_empty(&resolved.names.variant) {
            cmd.args(["-variant", &v]);
        }
        if let Some(m) = non_empty(&resolved.names.model) {
            cmd.args(["-model", &m]);
        }
        // Only when configured: `-option ""` is setxkbmap's CLEAR, not its no-op.
        if let Some(o) = non_empty(&resolved.names.options) {
            cmd.args(["-option", &o]);
        }
        if let Some(xa) = &xauth {
            cmd.env("XAUTHORITY", xa);
        }
        match crate::proc::status_within(&mut cmd, LAYOUT_SYNC_BUDGET) {
            Ok(st) if st.success() => tracing::info!(
                display = %dpy,
                layout = %resolved.names.describe(),
                source = %resolved.source,
                "gamescope: aligned the session's keyboard layout with the box"
            ),
            Ok(st) => tracing::warn!(
                display = %dpy,
                status = ?st.code(),
                "gamescope: setxkbmap rejected the box's layout — the session keeps its own"
            ),
            // Typically "setxkbmap is not installed". Not fatal: only a non-US keyboard notices,
            // and +pfhdr8 gamescope handles that without this path.
            Err(e) => tracing::warn!(
                display = %dpy,
                error = %e,
                layout = %resolved.names.describe(),
                "gamescope: session keyboard layout not set (is setxkbmap installed?)"
            ),
        }
    }
}

/// Attach to the session's existing PipeWire node. Nothing is stopped or re-moded — Managed would
/// rebuild headless, which is wrong for a panel pin. `hw_cursor` is spawn-flag, not per-cast.
pub(crate) fn stream_existing_output(
    connector: &str,
    hw_cursor: bool,
) -> Result<crate::mirror::MirrorStream> {
    let node_id = find_gamescope_node().ok_or_else(|| {
        anyhow!(
            "gamescope is driving {connector:?} but publishes no PipeWire Video/Source node — the \
             session may still be starting, or this gamescope was built without PipeWire support"
        )
    })?;
    // EIS advertises INT32_MAX; the output-size hint here is what scales client positions.
    point_injector_at_eis();
    tracing::info!(
        connector,
        node_id,
        hw_cursor,
        "gamescope: mirroring the session's own head (attach — the gaming session is untouched)"
    );
    Ok(crate::mirror::MirrorStream {
        node_id,
        remote_fd: None,
        // No xdg portal in this path (gamescope publishes the node itself), and no pointer in
        // the node either way — nothing to report.
        cursor_mode: None,
        keepalive: Box::new(()),
    })
}

/// Path of the host-written `GAMESCOPE_BIN` wrapper (per-user, in tmpfs).
fn gamescope_bin_wrapper_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gamescope-bin")
}

/// Injects `--nested-refresh $PF_HZ` (session-plus does not expose it). `$PF_HDR_ARGS` unquoted:
/// our own flag list, must word-split.
fn write_gamescope_bin_wrapper() -> Result<std::path::PathBuf> {
    let path = gamescope_bin_wrapper_path();
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\nexec {} --nested-refresh \"${{PF_HZ:-60}}\" ${{PF_HDR_ARGS}} \"$@\"\n",
            gamescope_bin()
        ),
    )
    .with_context(|| format!("write GAMESCOPE_BIN wrapper {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod the GAMESCOPE_BIN wrapper {}", path.display()))?;
    Ok(path)
}

/// Hardcoded by some session scripts instead of `GAMESCOPE_BIN`. Env, PATH shim, and
/// `PUNKTFUNK_GAMESCOPE_BIN` all miss at once.
const DISTRO_GAMESCOPE_PATH: &str = "/usr/bin/gamescope";

/// The socket directory every Xwayland — and so every gamescope — insists on before it will open a
/// display. See [`SessionBind`] for why the host has to care about a path it never reads itself.
const X11_SOCKET_DIR: &str = "/tmp/.X11-unix";

/// Mentions `GAMESCOPE_BIN` ⇒ env lever exists, no bind. Names the absolute path and not the var
/// ⇒ bind is the only lever. Main script only; `sessions.d` still lands in [`verify_managed_spawn_flags`].
fn script_hardcodes_gamescope(script: &str) -> bool {
    !script.contains("GAMESCOPE_BIN") && names_the_distro_binary(script)
}

/// Complete path, not prefix of `gamescopectl` / `gamescope-session-plus`.
fn names_the_distro_binary(script: &str) -> bool {
    script.match_indices(DISTRO_GAMESCOPE_PATH).any(|(at, _)| {
        let after = &script[at + DISTRO_GAMESCOPE_PATH.len()..];
        !after
            .starts_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    })
}

/// The session script's text, read once per process. `None` when it cannot be read at all, which
/// [`plan_bind`] treats as "do not arm" — a box we cannot inspect keeps the behaviour that works.
fn session_script() -> Option<&'static str> {
    static SCRIPT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    SCRIPT
        .get_or_init(|| {
            // Bounded read: this is a shell script (~30 KiB). An unbounded read of an arbitrary
            // path a package could replace is not a thing to do on the connect path.
            let bytes = std::fs::read(SESSION_PLUS_BIN).ok()?;
            if bytes.len() > 1 << 20 {
                return None;
            }
            Some(String::from_utf8_lossy(&bytes).into_owned())
        })
        .as_deref()
}

/// Why the host is NOT redirecting [`DISTRO_GAMESCOPE_PATH`] for this launch. Each arm is a
/// different sentence to an operator reading the log, which is the whole reason it is an enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindOff {
    /// The resolved binary IS the distro path — a bind over itself redirects nothing.
    SameBinary,
    /// The resolved gamescope is a bare NAME, not an absolute path, so the wrapper would resolve it
    /// through `PATH` inside the unit — onto the path we just bound the wrapper over.
    UnresolvedBinary,
    /// The session script honours `GAMESCOPE_BIN` (or never names the absolute path): the ordinary
    /// env lever already reaches gamescope, so the namespace would be cost without benefit.
    EnvLeverSuffices,
    /// The session script could not be read — fail closed rather than arm a mechanism we cannot
    /// show is needed.
    ScriptUnreadable,
    /// `PUNKTFUNK_GAMESCOPE_BIND=0`.
    OperatorOff,
    /// [`note_bind_hazard`] fired: a session launched with the bind armed never came up.
    Disarmed,
}

/// What the session unit's `/usr/bin/gamescope` redirect should be for this launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindPlan {
    /// No redirect, and therefore NO mount namespace for the unit at all.
    Off(BindOff),
    /// Redirect. `x11` additionally binds a user-owned socket directory over [`X11_SOCKET_DIR`],
    /// which is what stops the namespace from killing the session (see [`SessionBind`]).
    Arm { x11: bool },
}

/// Correctness first (no-op / re-exec), then operator knob, then backstop, then need.
/// `Some(true)` skips the script probe but does not outrank `disarmed`.
fn plan_bind(
    resolved_bin: &str,
    script: Option<&str>,
    enabled: Option<bool>,
    disarmed: bool,
    x11_dir_uid: Option<u32>,
    our_uid: u32,
) -> BindPlan {
    if resolved_bin == DISTRO_GAMESCOPE_PATH {
        return BindPlan::Off(BindOff::SameBinary);
    }
    // Bare name resolves through PATH inside the unit onto the wrapper we just bound. Re-exec loop.
    if !resolved_bin.starts_with('/') {
        return BindPlan::Off(BindOff::UnresolvedBinary);
    }
    if enabled == Some(false) {
        return BindPlan::Off(BindOff::OperatorOff);
    }
    if disarmed {
        return BindPlan::Off(BindOff::Disarmed);
    }
    if enabled != Some(true) {
        let Some(script) = script else {
            return BindPlan::Off(BindOff::ScriptUnreadable);
        };
        if !script_hardcodes_gamescope(script) {
            return BindPlan::Off(BindOff::EnvLeverSuffices);
        }
    }
    // Replace only a root-owned socket dir; ours maps, absent is created inside by us.
    BindPlan::Arm {
        x11: x11_dir_uid.is_some_and(|uid| uid != our_uid),
    }
}

/// Shared systemd spelling for the `/usr/bin/gamescope` redirect, so the transient unit and the
/// box drop-in cannot drift.
///
/// A user-unit mount namespace is also a user namespace (`uid_map` maps one id). Root-owned
/// `/tmp/.X11-unix` then reads as `nobody`; wlroots refuses every display and the short-session
/// tracker rewrites Game Mode to plasma. Bind a 0700 `$XDG_RUNTIME_DIR` dir read-write over it.
/// The XFixes reader is never spawned on this bind (patch paints the pointer); attach never arms
/// it. x11rb 0.14 dropped the abstract socket, so the filesystem path is the only one.
struct SessionBind {
    wrapper: std::path::PathBuf,
    /// The user-owned directory bound over [`X11_SOCKET_DIR`], or `None` when the real one is
    /// already ours and needs no replacing.
    x11_dir: Option<std::path::PathBuf>,
}

impl SessionBind {
    /// The `[Service]` settings this bind is, one per line — the shared spelling behind both
    /// renderers below.
    fn properties(&self) -> Vec<String> {
        let mut props = vec![format!(
            "BindReadOnlyPaths={}:{DISTRO_GAMESCOPE_PATH}",
            self.wrapper.display()
        )];
        if let Some(dir) = &self.x11_dir {
            // Read-WRITE: Xwayland creates its socket in here.
            props.push(format!("BindPaths={}:{X11_SOCKET_DIR}", dir.display()));
        }
        props
    }

    /// `systemd-run --property=` arguments (the transient unit).
    fn run_args(&self) -> Vec<String> {
        self.properties()
            .into_iter()
            .map(|p| format!("--property={p}"))
            .collect()
    }

    /// Drop-in body lines (the box's own unit).
    fn unit_lines(&self) -> String {
        self.properties()
            .into_iter()
            .map(|p| format!("{p}\n"))
            .collect()
    }
}

/// Owner of [`X11_SOCKET_DIR`] as the host sees it. `lstat`, matching wlroots' own check — a
/// symlink there is not a directory it will accept either.
fn x11_socket_dir_owner() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(X11_SOCKET_DIR)
        .ok()
        .map(|md| md.uid())
}

/// 0700: wlroots wants root-or-us and not group/other-writable.
fn session_x11_dir() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-x11")
}

/// Create (or re-assert) [`session_x11_dir`] and drop dead sockets from it.
fn ensure_session_x11_dir() -> Result<std::path::PathBuf> {
    let dir = session_x11_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    prune_stale_x11_sockets(&dir);
    Ok(dir)
}

/// SIGKILL never unlinks Xwayland sockets. wlroots walks 0..32 and then gives up.
fn prune_stale_x11_sockets(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::symlink_metadata(&path).is_ok_and(|md| md.file_type().is_socket()) {
            continue;
        }
        if std::os::unix::net::UnixStream::connect(&path).is_ok() {
            continue; // somebody is listening — not ours to remove
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Probe inside a throwaway unit: our uid on [`X11_SOCKET_DIR`] ⇒ wlroots will accept it.
/// Anything else (including a probe that cannot answer) is a refusal. Cached; budgeted.
fn bind_survives_namespace(bind: &SessionBind) -> bool {
    static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *OK.get_or_init(|| {
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--wait", "--collect", "--pipe", "--quiet"]);
        for arg in bind.run_args() {
            cmd.arg(arg);
        }
        // Absolute: the probe unit's PATH is the user manager's, not ours, and a bare name could
        // resolve through the bind we just made.
        cmd.args(["--", "/usr/bin/stat", "-c", "%u", X11_SOCKET_DIR]);
        let out = crate::proc::output_within(&mut cmd, BIND_PROBE_BUDGET);
        let owner = match &out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u32>()
                .ok(),
            _ => None,
        };
        let ours = crate::proc::current_uid();
        match owner {
            Some(uid) if uid == ours => {
                tracing::debug!(
                    uid,
                    "gamescope: probed the session unit's namespace — {X11_SOCKET_DIR} reads as \
                     ours inside it, so gamescope's Xwayland will accept it"
                );
                true
            }
            Some(uid) => {
                tracing::warn!(
                    uid,
                    ours,
                    "gamescope: probed the session unit's namespace and {X11_SOCKET_DIR} reads as \
                     uid {uid} inside it, not ours — gamescope's Xwayland would refuse it and the \
                     session would never start. NOT redirecting {DISTRO_GAMESCOPE_PATH}; the \
                     session runs the distro's stock gamescope instead (no HDR, no in-node cursor, \
                     games see 60 Hz) but it starts."
                );
                false
            }
            None => {
                tracing::warn!(
                    error = ?out.as_ref().err(),
                    status = ?out.as_ref().ok().map(|o| o.status),
                    "gamescope: could not probe the session unit's namespace (systemd-run --user \
                     failed, or it rejected one of the bind properties) — NOT redirecting \
                     {DISTRO_GAMESCOPE_PATH}, since whatever stopped the probe would have stopped \
                     the session too"
                );
                false
            }
        }
    })
}

/// How long [`bind_survives_namespace`] may take. One transient unit that runs `stat`; a wedged
/// user manager costs a connect a moment, not the session.
const BIND_PROBE_BUDGET: Duration = Duration::from_secs(10);

/// One-way latch: a crash-loop here rewrites Game Mode to the desktop.
fn bind_disarmed() -> bool {
    BIND_DISARMED.load(std::sync::atomic::Ordering::Relaxed)
}

fn note_bind_hazard(failed_unit: &str) {
    BIND_DISARMED.store(true, std::sync::atomic::Ordering::Relaxed);
    match session_log_refusal() {
        Some(marker) => tracing::error!(
            unit = failed_unit,
            evidence = marker,
            "gamescope: the session did not come up with the {DISTRO_GAMESCOPE_PATH} bind armed, \
             and its log carries the signature of the reason — a mount namespace in a systemd USER \
             unit is also a USER namespace, in which only this uid is mapped, so root-owned \
             {X11_SOCKET_DIR} reads as `nobody` and gamescope's Xwayland refuses to open a display. \
             Disarming the bind and relaunching without it: the session runs the distro's stock \
             gamescope (no HDR, no in-node cursor, games see 60 Hz) but it STARTS. The bind stays \
             off until this host process restarts."
        ),
        None => tracing::warn!(
            unit = failed_unit,
            "gamescope: the session did not come up with the {DISTRO_GAMESCOPE_PATH} bind armed. \
             Nothing in the session log names the known cause (the user namespace that comes with \
             the unit's mount namespace, which makes root-owned {X11_SOCKET_DIR} read as `nobody` \
             to gamescope's Xwayland), so this may be an unrelated failure — disarming and \
             relaunching without the bind anyway, because a session that will not start is worse \
             than one without our flags. The bind stays off until this host process restarts."
        ),
    }
}

static BIND_DISARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The lines wlroots prints when the socket-directory check fails, and the one it prints after it
/// has failed for every display number. Either is proof this is the namespace bug and not, say, a
/// Steam that would not start.
const XWAYLAND_REFUSAL_MARKERS: [&str; 2] = [
    "not owned by root or us",
    "No display available in the first",
];

/// Which refusal marker (if any) a session log carries. Pure so the matching is testable against
/// the exact field lines.
fn xwayland_refusal_marker(log: &str) -> Option<&'static str> {
    XWAYLAND_REFUSAL_MARKERS
        .into_iter()
        .find(|marker| log.contains(marker))
}

/// Scan the logs `gamescope-session-plus` writes for the Xwayland refusal — evidence that a failed
/// launch was THIS bug. Best-effort and bounded: a missing or unreadable log just means we cannot
/// confirm it, never that we assume the opposite.
fn session_log_refusal() -> Option<&'static str> {
    let home = std::env::var("HOME").ok()?;
    for name in [".gamescope-stderr.log", ".gamescope-stdout.log"] {
        let path = std::path::Path::new(&home).join(name);
        if let Some(marker) = tail_of(&path, 64 << 10)
            .as_deref()
            .and_then(xwayland_refusal_marker)
        {
            return Some(marker);
        }
    }
    None
}

/// The last `max` bytes of a file, lossily as text. Bounded because these logs are written by
/// someone else's script and a chatty gamescope can make them large.
fn tail_of(path: &std::path::Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.take(max).read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Resolve [`plan_bind`] against the live box, log the outcome, and return the armed bind.
/// `None` means the unit gets no mount namespace — the state every non-hardcoding distro stays in.
fn arm_session_bind(wrapper: &std::path::Path) -> Option<SessionBind> {
    let plan = plan_bind(
        gamescope_bin(),
        session_script(),
        pf_host_config::config().gamescope_bind,
        bind_disarmed(),
        x11_socket_dir_owner(),
        crate::proc::current_uid(),
    );
    let x11 = match plan {
        BindPlan::Arm { x11 } => x11,
        // Only the arms an operator could act on are worth a line each; `SameBinary` and
        // `EnvLeverSuffices` are the ordinary answer on almost every box.
        BindPlan::Off(BindOff::SameBinary | BindOff::EnvLeverSuffices) => {
            tracing::debug!(
                bin = %gamescope_bin(),
                "gamescope: no {DISTRO_GAMESCOPE_PATH} redirect needed for this session"
            );
            return None;
        }
        BindPlan::Off(BindOff::UnresolvedBinary) => {
            tracing::warn!(
                bin = %gamescope_bin(),
                "gamescope: the resolved gamescope is a bare name, not an absolute path — not \
                 redirecting {DISTRO_GAMESCOPE_PATH}, because the wrapper resolves that name \
                 through PATH inside the unit and would land back on the redirect itself. Put the \
                 binary on the host's PATH, or set PUNKTFUNK_GAMESCOPE_BIN to an absolute path."
            );
            return None;
        }
        BindPlan::Off(BindOff::ScriptUnreadable) => {
            tracing::debug!(
                script = SESSION_PLUS_BIN,
                "gamescope: cannot read the session script, so cannot show a {DISTRO_GAMESCOPE_PATH} \
                 redirect is needed — not arming one"
            );
            return None;
        }
        BindPlan::Off(BindOff::OperatorOff) => {
            tracing::info!(
                "gamescope: PUNKTFUNK_GAMESCOPE_BIND=0 — never redirecting {DISTRO_GAMESCOPE_PATH}. \
                 On a distro whose gamescope-session-plus hardcodes that path the session runs the \
                 distro's stock gamescope: no HDR, no in-node cursor, and games see gamescope's \
                 60 Hz headless default."
            );
            return None;
        }
        BindPlan::Off(BindOff::Disarmed) => {
            tracing::info!(
                "gamescope: the {DISTRO_GAMESCOPE_PATH} redirect is disarmed for this host process \
                 — an earlier session did not come up with it armed. Running the distro's stock \
                 gamescope (no HDR, no in-node cursor). Restart punktfunk-host to try it again."
            );
            return None;
        }
    };
    let x11_dir = if x11 {
        match ensure_session_x11_dir() {
            Ok(dir) => Some(dir),
            // No socket directory we own ⇒ no way to survive the user namespace the redirect
            // costs ⇒ do not arm it. Degraded beats a session that cannot start.
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "gamescope: could not prepare a user-owned {X11_SOCKET_DIR} for the session's \
                     mount namespace — NOT redirecting {DISTRO_GAMESCOPE_PATH}, because the \
                     namespace alone would stop gamescope's Xwayland from opening a display. The \
                     session runs the distro's stock gamescope instead."
                );
                return None;
            }
        }
    } else {
        None
    };
    let bind = SessionBind {
        wrapper: wrapper.to_path_buf(),
        x11_dir,
    };
    // Everything above is a decision about what SHOULD work. This is the one check that asks the
    // box, and it is the last gate before a unit whose failure costs the user their session.
    if !bind_survives_namespace(&bind) {
        return None;
    }
    match &bind.x11_dir {
        Some(dir) => tracing::info!(
            bin = %gamescope_bin(),
            x11_dir = %dir.display(),
            "gamescope: binding the patched build over {DISTRO_GAMESCOPE_PATH} inside the session \
             unit — this box's gamescope-session-plus hardcodes that path and reads GAMESCOPE_BIN \
             nowhere (Nobara), so nothing else reaches it. Nothing outside this unit is affected. A \
             user-owned {X11_SOCKET_DIR} rides along because the mount namespace that redirect \
             costs brings a USER namespace with it, in which the real (root-owned) socket directory \
             reads as `nobody` and gamescope's Xwayland refuses to open a display."
        ),
        None => tracing::info!(
            bin = %gamescope_bin(),
            "gamescope: binding the patched build over {DISTRO_GAMESCOPE_PATH} inside the session \
             unit — this box's gamescope-session-plus hardcodes that path and reads GAMESCOPE_BIN \
             nowhere (Nobara), so nothing else reaches it. Nothing outside this unit is affected. \
             {X11_SOCKET_DIR} is already ours, so the unit's user namespace maps it unchanged and \
             it needs no replacing."
        ),
    }
    Some(bind)
}

/// Session script does `export ENABLE_GAMESCOPE_WSI=1`, clobbering a unit `--setenv` of 0.
/// `DISABLE_GAMESCOPE_WSI` is presence-based and last in the loader, so it survives. Wrong here
/// is a game with sound and input on a black screen — Steam UI is not a Vulkan client. Keep
/// `ENABLE_GAMESCOPE_WSI=0` for a layer with no `disable_environment`.
const WSI_OFF_ENV: [(&str, &str); 2] = [
    ("DISABLE_GAMESCOPE_WSI", "1"),
    ("ENABLE_GAMESCOPE_WSI", "0"),
];

/// Same tree/rev as `punktfunk-gamescope`, own layer name — coexists with the distro's.
const OUR_WSI_LAYER_DIR_DEFAULT: &str = "/usr/lib/punktfunk/vulkan/implicit_layer.d";
const OUR_WSI_LAYER_MANIFEST_NAME: &str = "punktfunk_gamescope_wsi.json";

/// FHS default; `PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR` for a store with no `/usr` (NixOS).
fn our_wsi_layer_dir() -> String {
    std::env::var("PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| OUR_WSI_LAYER_DIR_DEFAULT.to_string())
}

/// Resolve once: [`WsiPlan::resolve`] can spawn `--version` probes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum WsiPlan {
    /// Our own matching layer is installed: enable it, suppress the distro's. Games get HDR.
    Ours,
    /// No layer of ours, and the distro's version triple matches the gamescope we run, so it is
    /// probably built against the same protocol. Leave the box exactly as it is.
    DistroKept,
    /// No layer of ours, and the distro's cannot be trusted. Disable it — a mismatched layer kills
    /// every Vulkan client — and accept that no game in this session can get an HDR10 swapchain.
    DistroDisabled,
}

impl WsiPlan {
    /// Spawns up to two `gamescope --version` probes in the fallback arms, so resolve once and
    /// pass the result around rather than calling this per use site.
    fn resolve() -> Self {
        let manifest = std::path::Path::new(&our_wsi_layer_dir()).join(OUR_WSI_LAYER_MANIFEST_NAME);
        if manifest.is_file() {
            Self::Ours
        } else if wsi_layer_matches_our_gamescope() {
            Self::DistroKept
        } else {
            Self::DistroDisabled
        }
    }

    /// Nested-client environment. Implicit-layer search belongs here, never on the compositor:
    /// gamescope's own Vulkan loads every implicit layer in its process env.
    fn env(self) -> Vec<(&'static str, String)> {
        match self {
            // `VK_ADD_IMPLICIT_LAYER_PATH` ADDS to the loader's implicit-layer search (loader
            // 1.3.234+), so the box's own layer directories keep working; the distro's gamescope
            // layer is then switched off by name, leaving exactly one gamescope WSI layer live — ours.
            Self::Ours => vec![
                ("VK_ADD_IMPLICIT_LAYER_PATH", our_wsi_layer_dir()),
                ("PUNKTFUNK_GAMESCOPE_WSI", "1".to_string()),
                ("DISABLE_GAMESCOPE_WSI", "1".to_string()),
                ("ENABLE_GAMESCOPE_WSI", "0".to_string()),
            ],
            Self::DistroKept => Vec::new(),
            Self::DistroDisabled => WSI_OFF_ENV
                .iter()
                .map(|(name, value)| (*name, (*value).to_string()))
                .collect(),
        }
    }

    /// Compositor process: force the distro layer off. Do not add our layer path.
    fn compositor_env(self) -> Vec<(&'static str, String)> {
        match self {
            Self::DistroKept => Vec::new(),
            Self::Ours | Self::DistroDisabled => WSI_OFF_ENV
                .iter()
                .map(|(name, value)| (*name, (*value).to_string()))
                .collect(),
        }
    }

    /// As `systemd-run` arguments, for the transient unit.
    fn setenv_args(self) -> Vec<String> {
        self.env()
            .iter()
            .map(|(name, value)| format!("--setenv={name}={value}"))
            .collect()
    }

    /// As unit-file lines, for the box-session drop-in. Trailing newline included, so whatever the
    /// body puts after it still parses — same contract as [`SessionBind::unit_lines`].
    fn unit_lines(self) -> String {
        self.env()
            .iter()
            .map(|(name, value)| format!("Environment={name}={value}\n"))
            .collect()
    }
}

/// Fallback only: version triples equal ⇒ keep the distro layer. A guess in both directions —
/// same tag with a patched protocol keeps a killer; different tag with identical protocol loses
/// HDR. Prefer [`WsiPlan::Ours`]. Unreadable either side ⇒ leave it (fail open).
fn wsi_layer_matches_our_gamescope() -> bool {
    let ours = discovery::gamescope_version_of(std::path::Path::new(gamescope_bin()));
    let distro = discovery::gamescope_version_of(std::path::Path::new(DISTRO_GAMESCOPE_PATH));
    match (ours, distro) {
        // Same upstream triple ⇒ the layer was built from the same protocol. Keep it.
        (Some(a), Some(b)) => a == b,
        // Either side unreadable: leave the layer alone rather than degrade a box that works.
        _ => true,
    }
}

/// Transient `--user` unit at `mode`. Blocks until the PipeWire node appears; timeout stops the unit.
fn launch_session(client: &str, unit_name: &str, mode: Mode, hdr: bool) -> Result<u32> {
    if !std::path::Path::new(SESSION_PLUS_BIN).exists() {
        anyhow::bail!(
            "PUNKTFUNK_GAMESCOPE_SESSION is set but {SESSION_PLUS_BIN} is missing — the host-managed \
             session needs gamescope-session-plus (a Bazzite / SteamOS-like host)"
        );
    }
    let wrapper = write_gamescope_bin_wrapper()?;
    stop_session(unit_name); // clear any stale unit + relay so a relaunch is clean
    let hz = mode.refresh_hz.max(1);
    // Headless `--nested-refresh` IS the output refresh. `CUSTOM_REFRESH_RATES` is the offered set,
    // inert on stock gamescope; it cannot fix a wrong nested-refresh.
    let game = game_hz(mode.refresh_hz);
    let offered = {
        let mut r = pf_host_config::config().gamescope_refresh_rates.clone();
        if !r.contains(&hz) {
            r.push(hz);
        }
        r.sort_unstable();
        r.dedup();
        r.iter().map(u32::to_string).collect::<Vec<_>>().join(",")
    };
    // `mut`: the backstop drops the bind and relaunches; an armed bind can stop the session starting.
    let mut bind = arm_session_bind(&wrapper);
    let wsi = WsiPlan::resolve();
    if wsi == WsiPlan::DistroDisabled {
        tracing::warn!(
            "gamescope: this box's VkLayer_FROG_gamescope_wsi was built for a different gamescope \
             than the one we run, and no punktfunk layer is installed to use instead — disabling \
             it for this session (DISABLE_GAMESCOPE_WSI=1, which the session script cannot clobber \
             the way it clobbers ENABLE_GAMESCOPE_WSI). Left enabled it rejects the client's \
             swapchain_feedback and every Vulkan client dies; Steam's own UI is not one, so what \
             you see is a game that runs with sound and input on a black screen, with no other \
             symptom. Upgrading the punktfunk-gamescope package fixes this properly — it ships a \
             layer built from the same tree as the compositor."
        );
        // `hdr_args` never consults the layer plan — say so when we advertise HDR with no game HDR.
        if hdr {
            tracing::warn!(
                "gamescope: this session negotiated HDR, but with the WSI layer disabled no game \
                 in it can get an HDR10 swapchain — that layer is the only route to one. The \
                 stream itself stays HDR (the capture really is PQ/BT.2020, and Steam's UI and the \
                 desktop ride the same container), so what breaks is GAME HDR specifically: a \
                 title told to render HDR renders it into an SDR swapchain and looks washed out."
            );
        }
    }
    let start_unit = |bind: Option<&SessionBind>| -> Result<()> {
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--collect", &format!("--unit={unit_name}")]);
        for arg in bind.map(SessionBind::run_args).unwrap_or_default() {
            cmd.arg(arg);
        }
        for arg in wsi.setenv_args() {
            cmd.arg(arg);
        }
        for arg in xkb_setenv_args() {
            cmd.arg(arg);
        }
        if let Some(path) = discovery::reaper_path_env() {
            cmd.arg(format!("--setenv=PATH={path}"));
        }
        // Stale desktop DISPLAY/WAYLAND_DISPLAY in the manager env would abort gamescope.
        cmd.arg("--property=UnsetEnvironment=DISPLAY WAYLAND_DISPLAY")
            .arg("--setenv=BACKEND=headless")
            .arg(format!("--setenv=SCREEN_WIDTH={}", mode.width))
            .arg(format!("--setenv=SCREEN_HEIGHT={}", mode.height))
            .arg(format!("--setenv=PF_HZ={game}"))
            // Unquoted: wrapper word-splits. Empty for stock-gamescope SDR.
            .arg(format!(
                "--setenv=PF_HDR_ARGS={}",
                hdr_args(hdr)
                    .into_iter()
                    .chain(cursor_args())
                    .chain(adaptive_sync_args(game))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))
            .arg(format!("--setenv=GAMESCOPE_BIN={}", wrapper.display()))
            .arg("--setenv=DRM_MODE=cvt")
            .arg(format!("--setenv=CUSTOM_REFRESH_RATES={offered}"))
            .arg("--")
            .arg(SESSION_PLUS_BIN)
            .arg(client);
        // Without `--wait`, seconds here means a wedged manager — unbounded would pin the connect.
        let status = crate::proc::status_within(&mut cmd, UNIT_VERB_BUDGET).context(
            "launch gamescope-session-plus via `systemd-run --user` (is the user systemd \
             manager up with XDG_RUNTIME_DIR + DBUS_SESSION_BUS_ADDRESS set?)",
        )?;
        if !status.success() {
            anyhow::bail!(
                "`systemd-run --user` did not start the gamescope session (exit {status})"
            );
        }
        Ok(())
    };
    start_unit(bind.as_ref())?;
    let mut deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(id) = find_gamescope_node() {
            // Convention, not a guarantee. Stop on rejection so the retry relaunches.
            if let Err(e) = verify_managed_spawn_flags(hdr) {
                stop_session(unit_name);
                return Err(e);
            }
            warn_if_mode_lost(mode, game);
            return Ok(id);
        }
        if Instant::now() >= deadline {
            stop_session(unit_name);
            // Bind can crash-loop gamescope until the short-session tracker rewrites Game Mode to
            // the desktop. Second 45 s without it: stock gamescope still starts.
            if bind.take().is_some() {
                note_bind_hazard(unit_name);
                start_unit(None)?;
                deadline = Instant::now() + Duration::from_secs(45);
                continue;
            }
            anyhow::bail!(
                "gamescope-session-plus '{client}' did not publish a Video/Source node within 45s \
                 (Steam failed to start? — `journalctl --user -u {unit_name}`)"
            );
        }
        // Wrapper SIGKILLs a gamescope that missed its 5 s handshake; no Restart=. Don't wait on a corpse.
        if !unit_starting_or_active(unit_name) {
            tracing::warn!(
                unit = unit_name,
                "gamescope session: transient unit died (missed the wrapper's 5 s gamescope \
                 readiness window?) — relaunching"
            );
            // NVIDIA reclaims GPU context asynchronously; instant relaunch misses the 5 s window again.
            std::thread::sleep(Duration::from_millis(1500));
            let _ = crate::proc::status_within(
                Command::new("systemctl").args(["--user", "reset-failed", unit_name]),
                UNIT_VERB_BUDGET,
            );
            start_unit(bind.as_ref())?;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Unknown reports `true` so a hiccup cannot trigger a relaunch storm. Timeout is that same answer.
fn unit_starting_or_active(unit: &str) -> bool {
    let Ok(out) = crate::proc::output_within(
        Command::new("systemctl").args(["--user", "is-active", unit]),
        UNIT_STATE_BUDGET,
    ) else {
        return true;
    };
    matches!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "active" | "activating" | "reloading" | "deactivating"
    )
}

fn stop_session(unit_name: &str) {
    kill_unit(unit_name);
    let _ = std::fs::remove_file(ei_socket_file());
}

/// `$XDG_RUNTIME_DIR`, never world-writable `/tmp`: a second local user must not plant a rogue
/// EIS path. Reader also rejects a symlink.
pub fn ei_socket_file() -> std::path::PathBuf {
    // The path itself is the shared `pf_paths::gamescope_ei_socket_file` contract (also read by the
    // libei injector). Compute it under the session env lock so a concurrent session handshake's
    // `apply_session_env` XDG_RUNTIME_DIR retarget can't race this producer-side read.
    crate::with_env_lock(pf_paths::gamescope_ei_socket_file)
}

/// First token, not a `steam://` URI: a bare `steam -gamepadui` needs the instance free more, not less.
fn is_steam_launch(cmd: &str) -> bool {
    cmd.split_whitespace().next() == Some("steam")
}

/// Non-Steam Exclusive: the box session is DRM master. Steam is handled by the failing arm above.
fn free_box_session_for_exclusive(steam: bool, exclusive: bool) -> bool {
    !steam && exclusive
}

/// Steam URI → insert `-gamepadui` so nested Steam is Big Picture. Idempotent. Custom cmds unchanged.
fn shape_dedicated_command(app: &str) -> String {
    let mut it = app.split_whitespace();
    if it.next() == Some("steam") {
        let rest: Vec<&str> = it.collect();
        if !rest.contains(&"-gamepadui") && rest.iter().any(|t| t.starts_with("steam://")) {
            return format!("steam -gamepadui {}", rest.join(" "));
        }
    }
    app.to_string()
}

/// Add the compositor-side arguments shared by every bare gamescope spawn. `steam_mode` belongs
/// before the `--` terminator; [`PUNKTFUNK_GAMESCOPE_APP`](spawn) configures the nested command
/// after it and therefore cannot enable gamescope's Steam integration itself.
///
/// `-r` is the rate the GAME sees and is clamped to, which is why the frame limiter lives here
/// (see [`game_hz`]) and nowhere near the session: capping it makes the game stop rendering
/// frames nobody asked for, while capture and the wire keep running at the client's own rate.
fn add_bare_gamescope_args(
    command: &mut Command,
    w: u32,
    h: u32,
    hz: u32,
    steam_mode: bool,
    grab_cursor: bool,
    hdr: bool,
) {
    command
        .args(["--backend", "headless"])
        .args(["-W", &w.to_string()])
        .args(["-H", &h.to_string()])
        .args(["-r", &game_hz(hz).to_string()]);
    if steam_mode {
        command.arg("--steam");
    }
    if grab_cursor {
        command.arg("--force-grab-cursor");
    }
    // `-r` is already the reported refresh. This adds the rest of the advertised set.
    for arg in hdr_args(hdr)
        .into_iter()
        .chain(cursor_args())
        .chain(adaptive_sync_args(game_hz(hz)))
        .chain(refresh_rate_args(hz))
    {
        command.arg(arg);
    }
    command.args(["--xwayland-count", "1", "--"]);
}

/// Shared by all three spawn paths — a kept display is keyed on `hdr`. Headless hardcodes
/// `SupportsHDR() == false`; `--hdr-debug-force-support` is the bypass. SDR nits: see
/// [`SDR_REFERENCE_WHITE_NITS`].
fn hdr_args(hdr: bool) -> Vec<String> {
    if !hdr {
        return Vec::new();
    }
    let nits = pf_host_config::config()
        .gamescope_sdr_nits
        .unwrap_or(SDR_REFERENCE_WHITE_NITS);
    vec![
        "--hdr-enabled".to_string(),
        "--hdr-debug-force-support".to_string(),
        "--hdr-sdr-content-nits".to_string(),
        nits.to_string(),
    ]
}

/// BT.2408 HDR Reference White, the level clients anchor SDR white at. It is the starting value:
/// from `+pfhdr16` the capture follows Steam's SDR brightness setting when Steam changes it.
const SDR_REFERENCE_WHITE_NITS: u32 = 203;

/// Must agree with [`crate::gamescope_composites_cursor`] — both read the same probe.
fn cursor_args() -> Vec<String> {
    let mut args = Vec::new();
    if gamescope_can_composite_cursor() {
        args.push("--pipewire-composite-cursor".to_string());
    }
    // No host-side fallback: the host cannot reconstruct another process's overlay window.
    if gamescope_can_composite_external_overlay() {
        args.push("--pipewire-composite-external-overlay".to_string());
    }
    args
}

/// Paint-on-commit + `--framerate-limit` at the same rate as `-r`. The two travel together: VRR
/// stops pacing to the refresh grid, so a FIFO game would otherwise run unbounded. Gated on the
/// probe so argv means what it says. `PUNKTFUNK_GAMESCOPE_VRR=0` opts out.
fn adaptive_sync_args(game_hz: u32) -> Vec<String> {
    if !pf_host_config::config().gamescope_vrr || !gamescope_paints_on_commit() {
        return Vec::new();
    }
    vec![
        "--adaptive-sync".to_string(),
        "--framerate-limit".to_string(),
        game_hz.to_string(),
    ]
}

/// gamescope reads only `XKB_DEFAULT_*`, never `localectl`'s xorg.conf.d. Empty when unconfigured
/// so we do not invent a layout. Headless still needs the stub-keyboard patch or Xwayland stays US.
fn xkb_env() -> Vec<(&'static str, String)> {
    let resolved = pf_host_config::layout::system_layout();
    let pairs = resolved.names.env_pairs();
    if pairs.is_empty() {
        return pairs;
    }
    if gamescope_honours_xkb_env() {
        tracing::info!(
            layout = %resolved.names.describe(),
            source = %resolved.source,
            "gamescope session: handing it the box's keyboard layout"
        );
    } else {
        tracing::warn!(
            layout = %resolved.names.describe(),
            source = %resolved.source,
            "gamescope session: this build ignores XKB_DEFAULT_* (needs punktfunk-gamescope \
             +pfhdr8) — the session will type US characters whatever the box is configured for"
        );
    }
    pairs
}

fn xkb_setenv_args() -> Vec<String> {
    xkb_env()
        .into_iter()
        .map(|(name, value)| format!("--setenv={name}={value}"))
        .collect()
}

/// Trailing newline so whatever follows in the drop-in body still parses.
fn xkb_unit_lines() -> String {
    xkb_env()
        .into_iter()
        .map(|(name, value)| format!("Environment={name}={value}\n"))
        .collect()
}

/// Headless advertises one rate unless this is passed. `session_hz` is always in the list.
fn refresh_rate_args(session_hz: u32) -> Vec<String> {
    if !gamescope_can_offer_refresh_rates() {
        return Vec::new();
    }
    vec![
        "--custom-refresh-rates".to_string(),
        refresh_rate_list(
            session_hz,
            &pf_host_config::config().gamescope_refresh_rates,
        ),
    ]
}

/// No whitespace: both managed paths interpolate this into unquoted `${PF_HDR_ARGS}`.
fn refresh_rate_list(session_hz: u32, configured: &[u32]) -> String {
    let mut rates = configured.to_vec();
    if !rates.contains(&session_hz) {
        rates.push(session_hz);
    }
    rates.sort_unstable();
    rates.dedup();
    rates
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

/// Same answer `create` gates Steam-free on and `spawn` turns into `--steam`. Blank cmd falls
/// through to env.
fn resolved_spawn_app(cmd: Option<&str>) -> Option<String> {
    cmd.map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        // Read the env fallback under the shared env lock so it can't race a concurrent session's
        // `set_var` of the same key.
        .or_else(|| crate::with_env_lock(|| std::env::var("PUNKTFUNK_GAMESCOPE_APP").ok()))
        .filter(|s| !s.trim().is_empty())
}

/// `None` app is `sleep infinity`. The wrapper relays `LIBEI_SOCKET`, applies the
/// nested environment, and runs the launch value as the shell command its source promises.
/// The WSI layer stays out of gamescope's own Vulkan process.
fn spawn(
    w: u32,
    h: u32,
    hz: u32,
    app: Option<String>,
    log: &std::path::Path,
    hdr: bool,
    iso: Option<&crate::SessionIsolation>,
) -> Result<Child> {
    // Real app vs `sleep infinity` keep-alive: scopes the game-only cursor-grab flag below.
    let game_launch = app.is_some();
    let app = app.unwrap_or_else(|| "sleep infinity".to_string());
    let app = shape_dedicated_command(&app);
    // Isolated: per-session relay so a concurrent spawn cannot overwrite the injector's socket.
    let relay = iso
        .map(|i| i.ei_relay.clone())
        .unwrap_or_else(ei_socket_file);
    let _ = std::fs::remove_file(&relay); // stale socket path from a previous session
                                          // `--steam` when launching Steam; the global knob still forces it on for every spawn.
    let steam_mode = pf_host_config::config().gamescope_steam || is_steam_launch(&app);
    // Default off: forces relative mode, which would break absolute-pointer games/menus.
    let grab_cursor = game_launch && pf_host_config::config().gamescope_grab_cursor;
    // Without a painting client gamescope pushes no capture buffers.
    let splash_exe = pf_host_config::config()
        .gamescope_splash
        .then(std::env::current_exe)
        .and_then(|r| {
            r.map_err(|e| tracing::warn!(error = %e, "gamescope: current_exe failed — no splash"))
                .ok()
        });
    let mut cmd = Command::new(gamescope_bin());
    if let Some(path) = discovery::reaper_path_env() {
        cmd.env("PATH", path);
    }
    add_bare_gamescope_args(&mut cmd, w, h, hz, steam_mode, grab_cursor, hdr);
    let wsi = WsiPlan::resolve();
    if wsi == WsiPlan::DistroDisabled {
        // `hdr` is a field, not a gate: the layer is missing either way, and so is the fix.
        tracing::warn!(
            hdr,
            "gamescope: this box's VkLayer_FROG_gamescope_wsi was built for a different gamescope \
             than the one we run, so it is disabled for this session and no nested game can get \
             an HDR10 swapchain. The punktfunk-gamescope package ships a matching layer."
        );
    }
    let script = nested_wrapper_script(&relay, splash_exe.is_some(), &wsi.env());
    cmd.args(["sh", "-c", &script, "sh"]);
    if let Some(exe) = &splash_exe {
        cmd.arg(exe);
    }
    // Env-pinned Pulse does not follow default-sink churn across concurrent sessions.
    if let Some(iso) = iso {
        if let Some(sink) = &iso.sink {
            cmd.env("PULSE_SINK", sink);
        }
        if let Some(src) = &iso.mic_source {
            cmd.env("PULSE_SOURCE", src);
        }
    }
    cmd.arg(&app)
        // Prefer the NVIDIA GL vendor for the nested session (harmless on a pure-NVIDIA box).
        .env("__GLX_VENDOR_LIBRARY_NAME", "nvidia")
        // The box's keyboard layout — see [`xkb_env`]. Empty on an unconfigured box.
        .envs(xkb_env())
        // Distro WSI off on the compositor. Ours is in the nested wrapper, not here.
        .envs(wsi.compositor_env())
        // Headless must not attach. Stale WAYLAND_DISPLAY in the manager env aborts gamescope
        // before its PipeWire node appears. Nested apps get gamescope's own DISPLAY.
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY");
    if let Ok(logf) = std::fs::File::create(log) {
        if let Ok(log2) = logf.try_clone() {
            cmd.stdout(Stdio::from(logf)).stderr(Stdio::from(log2));
        }
    } else {
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
    }
    tracing::info!(
        w, h, hz, steam_mode, hdr, ?wsi,
        bin = %gamescope_bin(),
        splash = splash_exe.is_some(),
        %app,
        log = %log.display(),
        "spawning gamescope (headless)"
    );
    cmd.spawn()
        .context("spawn gamescope (is it installed? `apt install gamescope`)")
}

/// Builds the nested wrapper. Its first remaining argument is one shell command,
/// kept intact until the inner shell parses quoting and operators.
fn nested_wrapper_script(
    relay: &std::path::Path,
    with_splash: bool,
    nested_env: &[(&'static str, String)],
) -> String {
    let env_kv = nested_env
        .iter()
        .map(|(k, v)| shell_word(&format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join(" ");
    let relay = shell_word(&relay.to_string_lossy());
    let run = if env_kv.is_empty() {
        "exec sh -c \"$1\"".to_string()
    } else {
        format!("exec env {env_kv} sh -c \"$1\"")
    };
    if with_splash {
        let splash = if env_kv.is_empty() {
            "\"$1\" gamescope-splash &".to_string()
        } else {
            format!("env {env_kv} \"$1\" gamescope-splash &")
        };
        format!("printf %s \"$LIBEI_SOCKET\" > {relay}; {splash} shift; {run}")
    } else {
        format!("printf %s \"$LIBEI_SOCKET\" > {relay}; {run}")
    }
}

/// Quotes one POSIX shell word, including embedded apostrophes.
fn shell_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

struct GamescopeProc {
    child: Child,
    log: std::path::PathBuf,
    /// The relay file THIS spawn's wrapper wrote — the global path, or the session's per-instance
    /// one when isolated — so teardown clears its own file and never a concurrent session's.
    relay: std::path::PathBuf,
}

impl Drop for GamescopeProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Clear the relayed EIS socket name so an injector can't reconnect to this now-dead
        // session's socket between sessions (the stale path is the "Connection refused").
        let _ = std::fs::remove_file(&self.relay);
        // Drop this spawn's per-instance log so `$XDG_RUNTIME_DIR` doesn't accumulate them.
        let _ = std::fs::remove_file(&self.log);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        any_output_size_is, cancel_pending_restore, cgroup_is_punktfunk_owned,
        classify_output_size, connected_connector_under, display_manager_unit_under, dm_plan,
        free_box_session_for_exclusive, game_hz, gamescope_output_size, hdr_args, idle_dropin_body,
        idle_dropin_path, install_idle_dropin, is_steam_launch, managed_darken_acquire_edge,
        managed_darken_release_edge, mask_unit, missing_flags, mode_mismatch,
        nested_wrapper_script, our_wsi_layer_dir, parse_listed_units, plan_bind, refresh_rate_list,
        release_autologin_mask, remove_idle_dropin, script_hardcodes_gamescope, sentinel_advanced,
        shape_dedicated_command, shell_word, switch_ends_mask_window, takeover_state_is_live,
        unmask_unit, xwayland_refusal_marker, BindOff, BindPlan, BoxOutputSize, DmHelperError,
        SessionBind, TakeoverState, WsiPlan, AUTOLOGIN_MASKED, DISTRO_GAMESCOPE_PATH,
        PENDING_RESTORE, RESTORE_FLIGHT, STOPPED_AUTOLOGIN, WSI_OFF_ENV, X11_SOCKET_DIR,
    };
    use std::time::{Duration, Instant};

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    /// Two gamescopes disagree: "cannot tell" is handleable; a confident wrong number is not.
    #[test]
    fn the_output_size_probe_refuses_to_pick_between_disagreeing_gamescopes() {
        let session = argv("/usr/bin/gamescope -W 1920 -H 1080 --prefer-output HDMI-A-1");
        let nested = argv("gamescope --backend wayland -W 1280 -H 800");
        // One compositor, or several that agree — a plain answer.
        assert_eq!(
            classify_output_size(std::slice::from_ref(&session)),
            BoxOutputSize::Known((1920, 1080))
        );
        assert_eq!(
            classify_output_size(&[session.clone(), session.clone()]),
            BoxOutputSize::Known((1920, 1080))
        );
        // Disagreement is AMBIGUOUS, not a coin flip — in either enumeration order.
        assert_eq!(
            classify_output_size(&[session.clone(), nested.clone()]),
            BoxOutputSize::Ambiguous
        );
        assert_eq!(
            classify_output_size(&[nested.clone(), session.clone()]),
            BoxOutputSize::Ambiguous
        );
        // Unreported ≠ Ambiguous: re-mode on the first, never the second.
        assert_eq!(classify_output_size(&[]), BoxOutputSize::Unreported);
        assert_eq!(
            classify_output_size(&[argv("gamescope --steam")]),
            BoxOutputSize::Unreported
        );
        assert_ne!(BoxOutputSize::Unreported, BoxOutputSize::Ambiguous);
    }

    /// After restart: is `target` present? Unanimity would hold Ambiguous for a kept stray spawn.
    #[test]
    fn the_post_restart_wait_asks_whether_the_target_size_is_present() {
        let session = argv("/usr/bin/gamescope -W 1920 -H 1080");
        let stray = argv("punktfunk-gamescope --backend headless -W 1280 -H 720");
        assert!(any_output_size_is(
            std::slice::from_ref(&session),
            (1920, 1080)
        ));
        // The stray one neither satisfies nor blocks the answer.
        assert!(any_output_size_is(
            &[stray.clone(), session.clone()],
            (1920, 1080)
        ));
        assert!(!any_output_size_is(
            std::slice::from_ref(&stray),
            (1920, 1080)
        ));
        assert!(!any_output_size_is(&[], (1920, 1080)));
        // Unanimity would have blocked it — the regression this predicate replaces.
        assert_eq!(
            classify_output_size(&[stray, session]),
            BoxOutputSize::Ambiguous
        );
    }

    /// `-W`/`-H` must be read as a pair off ONE argv, and the long spellings count: a half-answer
    /// would otherwise be published as a monitor row (`heads_under`) or a pointer scale.
    #[test]
    fn output_size_needs_both_flags_from_the_same_argv() {
        assert_eq!(
            gamescope_output_size(&argv("gamescope -W 2560 -H 1440")),
            Some((2560, 1440))
        );
        assert_eq!(
            gamescope_output_size(&argv("gamescope --output-width 800 --output-height 600")),
            Some((800, 600))
        );
        assert_eq!(gamescope_output_size(&argv("gamescope -W 2560")), None);
        assert_eq!(gamescope_output_size(&argv("gamescope -H 1440")), None);
        // The NESTED size (`-w`/`-h`) is a different thing and must never stand in for the output.
        assert_eq!(
            gamescope_output_size(&argv("gamescope -w 1280 -h 800")),
            None
        );
    }

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

    /// Both HDR spawn flags are required: `--hdr-enabled` alone does nothing on the headless
    /// backend, whose connector hardcodes `SupportsHDR() == false`. Their absence is
    /// indistinguishable from a capture negotiation failure.
    #[test]
    fn hdr_spawn_flags_are_both_present_and_absent_for_sdr() {
        assert!(
            hdr_args(false).is_empty(),
            "an SDR spawn takes no HDR flags"
        );
        let args = hdr_args(true);
        assert!(args.iter().any(|a| a == "--hdr-enabled"));
        assert!(
            args.iter().any(|a| a == "--hdr-debug-force-support"),
            "without the force flag the headless connector reports no HDR support, so the WSI \
             layer advertises no HDR surfaces and games render SDR"
        );
    }

    /// The rate set rides into both managed sessions inside an unquoted `${PF_HDR_ARGS}`, which the
    /// shim's shell word-splits. Whitespace would split one flag into two argv entries and gamescope
    /// would reject the launch. The session rate must also survive: it is the rate the session runs at.
    #[test]
    fn the_refresh_rate_list_is_word_split_safe_and_keeps_the_session_rate() {
        let list = refresh_rate_list(240, &[60, 120]);
        assert_eq!(list, "60,120,240", "sorted, deduped, session rate appended");
        assert!(
            !list.contains(char::is_whitespace),
            "an unquoted ${{PF_HDR_ARGS}} word-splits on whitespace"
        );
        assert_eq!(
            refresh_rate_list(60, &[60]),
            "60",
            "a configured set that already holds the session rate gains no duplicate"
        );
        assert_eq!(
            refresh_rate_list(90, &[]),
            "90",
            "unset, we advertise exactly the rate the client asked for"
        );
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
    fn idle_dropin_replaces_exec_start_rather_than_appending() {
        let body = idle_dropin_body("/usr/bin/sleep");
        assert_eq!(
            body, "[Service]\nExecStart=\nExecStart=/usr/bin/sleep infinity\n",
            "{body}"
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[1], "ExecStart=", "the reset must come first: {body}");
        // The path is resolved per box ([`sleep_binary`]) and must reach the unit verbatim — a
        // bare `sleep` would depend on the unit's PATH, and an ExecStart that fails to execute is
        // the failing unit the display manager relogin-loops against.
        assert!(idle_dropin_body("/bin/sleep").contains("ExecStart=/bin/sleep infinity"));
    }

    #[test]
    fn nested_wrapper_script_shapes() {
        let relay = std::path::Path::new("/run/user/1000/pf-ei");
        // Plain: relay + shell command, no splash machinery.
        let plain = nested_wrapper_script(relay, false, &[]);
        assert!(plain.contains("/run/user/1000/pf-ei"));
        assert!(plain.ends_with("exec sh -c \"$1\""));
        assert!(!plain.contains("gamescope-splash"));
        // Splash shifts its executable, leaving the command in `$1`.
        let splash = nested_wrapper_script(relay, true, &[]);
        assert!(splash.contains("\"$1\" gamescope-splash &"));
        assert!(splash.contains("shift; exec sh -c \"$1\""));
        let wsi = nested_wrapper_script(
            relay,
            false,
            &[("PUNKTFUNK_GAMESCOPE_WSI", "1".to_string())],
        );
        assert!(wsi.contains("exec env 'PUNKTFUNK_GAMESCOPE_WSI=1' sh -c \"$1\""));
        assert_eq!(shell_word("a b'c;$HOME"), "'a b'\"'\"'c;$HOME'");

        let out = std::process::Command::new("sh")
            .args([
                "-c",
                &nested_wrapper_script(std::path::Path::new("/dev/null"), false, &[]),
                "sh",
            ])
            .arg("printf '%s' 'quoted command stays whole'")
            .env("LIBEI_SOCKET", "test")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"quoted command stays whole");
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
    fn exclusive_frees_the_box_session_for_a_non_steam_launch_too() {
        // Non-Steam Exclusive: the box session is DRM master of the TV.
        assert!(free_box_session_for_exclusive(false, true));
        // A Steam launch is already handled by the arm above this one — and that arm is the
        // failing one (the single instance is not optional), so this gate must not also fire and
        // free the session a second time.
        assert!(!free_box_session_for_exclusive(true, true));
        // Not exclusive: the operator did not ask for their screens to go dark, so a non-Steam
        // launch must keep leaving the box's session strictly alone. This is what makes `extend`
        // and the `SharedDesktop` preset ("never blank the real monitors") mean what they say.
        assert!(!free_box_session_for_exclusive(false, false));
        assert!(!free_box_session_for_exclusive(true, false));
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
        *STOPPED_AUTOLOGIN.lock().unwrap() = vec![PROBE.to_string()];
        *AUTOLOGIN_MASKED.lock().unwrap() = true;
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
        assert_eq!(STOPPED_AUTOLOGIN.lock().unwrap().as_slice(), [PROBE]);
        // Idempotent — the watcher calls it on every switch it confirms.
        release_autologin_mask(crate::ActiveKind::DesktopGnome);
        assert_ne!(is_enabled(), "masked-runtime");

        unmask_unit(PROBE);
        remove_idle_dropin();
        STOPPED_AUTOLOGIN.lock().unwrap().clear();
        *AUTOLOGIN_MASKED.lock().unwrap() = false;
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

    #[test]
    fn steam_launch_detection() {
        assert!(is_steam_launch("steam steam://rungameid/570"));
        assert!(is_steam_launch("steam -silent steam://rungameid/570"));
        assert!(!is_steam_launch("vkcube"));
        assert!(!is_steam_launch("lutris lutris:rungameid/42"));
        // A `steam_ui` launcher entry carries no URI, and must still count: it needs the single
        // instance freed and gamescope's `--steam` mode on. Gating on `steam://` would skip both
        // for the one launch that is Big Picture itself.
        assert!(is_steam_launch("steam -gamepadui"));
        assert!(is_steam_launch("steam"));
        // A command that merely mentions steam elsewhere is not a Steam client launch.
        assert!(!is_steam_launch("mygame --steam-overlay"));
    }

    #[test]
    fn dedicated_command_shaping() {
        // Steam URI → -gamepadui inserted so the nested Steam is Big Picture (not the desktop UI).
        assert_eq!(
            shape_dedicated_command("steam steam://rungameid/570"),
            "steam -gamepadui steam://rungameid/570"
        );
        // Idempotent: an already-gamepadui command is left alone.
        assert_eq!(
            shape_dedicated_command("steam -gamepadui steam://rungameid/570"),
            "steam -gamepadui steam://rungameid/570"
        );
        // Non-Steam launches and operator custom commands are untouched.
        assert_eq!(shape_dedicated_command("vkcube"), "vkcube");
        assert_eq!(
            shape_dedicated_command("lutris lutris:rungameid/42"),
            "lutris lutris:rungameid/42"
        );
        // A bare `steam` with no URI is left alone (not a game launch).
        assert_eq!(
            shape_dedicated_command("steam -bigpicture"),
            "steam -bigpicture"
        );
        // The `steam_ui` launcher entries pass through untouched — the shaping only ever fires on
        // a `steam://` game launch, so there is no way to end up with `-gamepadui` twice.
        assert_eq!(
            shape_dedicated_command("steam -gamepadui"),
            "steam -gamepadui"
        );
        assert_eq!(shape_dedicated_command("steam"), "steam");
    }

    #[test]
    fn game_hz_is_the_session_rate_until_the_limiter_is_set() {
        // The env is process-wide and `config()` is parsed once, so this asserts the default
        // (nothing set): every host must keep handing gamescope the client's own rate. `game_fps`'s
        // own unit test in pf-host-config covers the capping arithmetic without needing the env.
        if pf_host_config::config().max_fps.is_none() {
            for hz in [30, 60, 120, 144, 240] {
                assert_eq!(game_hz(hz), hz);
            }
        }
        // Never zero, whatever the inputs: gamescope would reject `-r 0`.
        assert!(game_hz(0) >= 1);
    }

    #[test]
    fn desktop_steam_cgroup_ownership() {
        // A desktop-launched Steam (the instance-conflict case, as observed on a GNOME host).
        assert!(!cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-steam-48605.scope"
        ));
        // KDE spawns app scopes too; still foreign.
        assert!(!cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@0f3a.service"
        ));
        // Our own dedicated spawn tree (Steam nested under the host service).
        assert!(cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-host.service"
        ));
        // The host-managed gamescope session unit (SESSION_UNIT).
        assert!(cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-gamescope.service"
        ));
        assert!(!cgroup_is_punktfunk_owned(""));
    }

    /// A headless gamescope reports `--nested-refresh` as its one refresh rate and falls back to
    /// 60 Hz when the flag never arrives, so a session that lost the `GAMESCOPE_BIN` wrapper
    /// streams at the client's rate while telling every game it is 60.
    #[test]
    fn mode_mismatch_names_what_the_session_actually_got() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };

        // The good case: our own managed spawn, carrying everything we asked for.
        let ok = vec![argv(
            "/usr/bin/gamescope --backend headless -W 1920 -H 1080 --nested-refresh 120 --steam",
        )];
        assert!(mode_mismatch(1920, 1080, 120, &ok).is_empty());

        // Wrapper dropped: no `--nested-refresh` anywhere and gamescope silently ran its 60 Hz
        // default. Size still landed (SCREEN_WIDTH survived).
        let lost = vec![argv(
            "/usr/bin/gamescope --backend headless -W 1920 -H 1080 --steam",
        )];
        let got = mode_mismatch(1920, 1080, 120, &lost);
        assert_eq!(got.len(), 1, "only the refresh is wrong: {got:?}");
        assert!(got[0].contains("asked=120Hz"), "{got:?}");
        assert!(got[0].contains("no --nested-refresh at all"), "{got:?}");

        // A wrong rate is reported with the number it actually got, not just "missing".
        let wrong = vec![argv("gamescope -W 1920 -H 1080 --nested-refresh 60")];
        let got = mode_mismatch(1920, 1080, 120, &wrong);
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("got=60Hz"), "{got:?}");

        // Resolution lost too (SCREEN_WIDTH/HEIGHT dropped as well) — both are named.
        let both = vec![argv("gamescope -W 1280 -H 720")];
        assert_eq!(mode_mismatch(1920, 1080, 120, &both).len(), 2);

        // Fail open, exactly like `missing_flags`: nothing to compare against says nothing. A box
        // with a second gamescope that carries no output size must not produce a false alarm.
        assert!(mode_mismatch(1920, 1080, 120, &[]).is_empty());

        // Any running gamescope carrying the mode satisfies it — a Deck commonly runs a nested one
        // beside the session, and demanding that every gamescope match would reject a good session.
        let two = vec![
            argv("gamescope -W 1280 -H 800 --nested-refresh 60"),
            argv("gamescope -W 1920 -H 1080 --nested-refresh 120"),
        ];
        assert!(mode_mismatch(1920, 1080, 120, &two).is_empty());

        // The long spellings are read too.
        let long = vec![argv(
            "gamescope --output-width 1920 --output-height 1080 --nested-refresh 120",
        )];
        assert!(mode_mismatch(1920, 1080, 120, &long).is_empty());

        // A flag with no value after it must not panic or read past the end.
        let truncated = vec![argv("gamescope -W 1920 -H 1080 --nested-refresh")];
        assert_eq!(mode_mismatch(1920, 1080, 120, &truncated).len(), 1);
    }

    /// A managed session that ignored `GAMESCOPE_BIN` / the PATH shim runs a stock gamescope, and
    /// the host — already told the compositor would paint the pointer — paints none either. Only a
    /// compositor we can see, missing a flag we can name, may fail.
    #[test]
    fn spawn_flag_verification_fails_closed_only_on_evidence() {
        let argv = |s: &str| -> Vec<String> { s.split(' ').map(str::to_string).collect() };
        let want: Vec<String> = ["--hdr-enabled", "--pipewire-composite-cursor"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // The flags arrived: nothing to report.
        assert!(missing_flags(
            &want,
            &[argv(
                "/usr/bin/punktfunk-gamescope --backend headless -W 1920 -H 1080 \
                 --hdr-enabled --hdr-debug-force-support --pipewire-composite-cursor"
            )]
        )
        .is_empty());

        // Distro binary: both flags lost. A lost cursor flag is silent.
        assert_eq!(
            missing_flags(
                &want,
                &[argv(
                    "/usr/bin/gamescope --backend headless -W 1920 -H 1080"
                )]
            ),
            vec!["--hdr-enabled", "--pipewire-composite-cursor"]
        );

        // A stock gamescope can take `--hdr-enabled` (it predates our patches) — so the HDR flag
        // alone proves nothing, and the cursor flag must be checked on its own.
        assert_eq!(
            missing_flags(
                &want,
                &[argv("/usr/bin/gamescope --hdr-enabled -W 1920 -H 1080")]
            ),
            vec!["--pipewire-composite-cursor"]
        );

        // Fail open when we could not look: an unreadable `/proc` is not evidence of anything, and
        // treating it as a miss would fail every managed session on a hardened box.
        assert!(missing_flags(&want, &[]).is_empty());

        // Several gamescopes running (a nested game under the session): the flags need only be on
        // one of them — the session compositor.
        assert!(missing_flags(
            &want,
            &[
                argv("/usr/bin/gamescope -W 800 -H 600"),
                argv("/usr/bin/punktfunk-gamescope --hdr-enabled --pipewire-composite-cursor"),
            ]
        )
        .is_empty());
    }

    /// A script that hardcodes `/usr/bin/gamescope` needs a bind; one that honours `GAMESCOPE_BIN`
    /// does not. Getting that backwards costs every other distro a mount namespace it has no use for.
    #[test]
    fn only_a_script_that_hardcodes_the_path_needs_the_bind() {
        let nobara = "if [ -z \"$GAMESCOPECMD\" ]; then\n    \
                      GAMESCOPECMD=\"/usr/bin/gamescope \\\n";
        assert!(script_hardcodes_gamescope(nobara));

        // Upstream / Bazzite: the env var is right there in the script.
        let upstream = "GAMESCOPE_BIN=${GAMESCOPE_BIN:-/usr/bin/gamescope}\n\
                        GAMESCOPECMD=\"$GAMESCOPE_BIN \\\n";
        assert!(
            !script_hardcodes_gamescope(upstream),
            "a script that reads GAMESCOPE_BIN needs no namespace to reach our binary"
        );

        // Mentions the var but not the path, and vice versa in the other direction: neither is the
        // shape the bind fixes, so neither arms it.
        assert!(!script_hardcodes_gamescope(
            "exec ${GAMESCOPE_BIN} \"$@\"\n"
        ));
        assert!(!script_hardcodes_gamescope("exec gamescope \"$@\"\n"));

        // The two longer paths that start with the one we redirect. A plain `contains` would read
        // either as a hardcode and buy the box a mount namespace for nothing.
        assert!(!script_hardcodes_gamescope(
            "/usr/bin/gamescopectl takescreenshot\n"
        ));
        assert!(!script_hardcodes_gamescope(
            "exec /usr/bin/gamescope-session-plus \"$@\"\n"
        ));
        // The real path still counts, whether a quote, a space or a newline follows it.
        assert!(script_hardcodes_gamescope("GS=\"/usr/bin/gamescope\"\n"));
        assert!(script_hardcodes_gamescope("exec /usr/bin/gamescope\n"));
        assert!(script_hardcodes_gamescope(
            "exec /usr/bin/gamescope -W 1920\n"
        ));
    }

    /// The decision matrix, in the order the arms are meant to win. Every `Off` arm here is a box
    /// that gets no mount namespace — the state every box was in before the redirect existed.
    #[test]
    fn bind_is_armed_only_where_it_is_needed_and_survivable() {
        const OURS: &str = "/usr/bin/punktfunk-gamescope";
        const AUTO: Option<bool> = None;
        const OFF: Option<bool> = Some(false);
        const FORCE: Option<bool> = Some(true);
        let hardcoded = Some("GAMESCOPECMD=\"/usr/bin/gamescope -W\"");
        let honours = Some("GAMESCOPECMD=\"${GAMESCOPE_BIN} -W\"");

        // The case the mechanism exists for: hardcoding script, our binary, root-owned socket dir
        // ⇒ redirect AND replace the socket directory, or the namespace kills Xwayland.
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );

        // A socket directory already owned by us maps to us inside the user namespace too, so
        // there is nothing to compensate for — and an absent one is created, inside, by us.
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, Some(1000), 1000),
            BindPlan::Arm { x11: false }
        );
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, false, None, 1000),
            BindPlan::Arm { x11: false }
        );

        // Nothing to redirect: the resolved binary IS the distro path. Checked before everything
        // else, because it is true regardless of what the script or the operator says.
        assert_eq!(
            plan_bind(
                DISTRO_GAMESCOPE_PATH,
                hardcoded,
                FORCE,
                false,
                Some(0),
                1000
            ),
            BindPlan::Off(BindOff::SameBinary)
        );

        // A bare name — `gamescope_bin`'s fallback when its PATH walk finds nothing. Binding
        // around it makes the wrapper `exec` a name that now resolves to the wrapper, so not even
        // a forcing operator gets it.
        assert_eq!(
            plan_bind("gamescope", hardcoded, FORCE, false, Some(0), 1000),
            BindPlan::Off(BindOff::UnresolvedBinary)
        );

        // The ordinary answer on Bazzite/SteamOS-likes: the env lever lands, so no namespace.
        assert_eq!(
            plan_bind(OURS, honours, AUTO, false, Some(0), 1000),
            BindPlan::Off(BindOff::EnvLeverSuffices)
        );

        // Fail closed on a box we cannot inspect.
        assert_eq!(
            plan_bind(OURS, None, AUTO, false, Some(0), 1000),
            BindPlan::Off(BindOff::ScriptUnreadable)
        );

        // Both retreats outrank the need: an operator's `=0`, and the runtime backstop's latch.
        assert_eq!(
            plan_bind(OURS, hardcoded, OFF, false, Some(0), 1000),
            BindPlan::Off(BindOff::OperatorOff)
        );
        assert_eq!(
            plan_bind(OURS, hardcoded, AUTO, true, Some(0), 1000),
            BindPlan::Off(BindOff::Disarmed)
        );

        // `=1` skips the script probe (for a GAMESCOPE_BIN defeated in a `sessions.d` fragment the
        // host cannot read) — but it does not outrank the backstop, or a forced box that
        // crash-loops would never stop.
        assert_eq!(
            plan_bind(OURS, honours, FORCE, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );
        assert_eq!(
            plan_bind(OURS, None, FORCE, false, Some(0), 1000),
            BindPlan::Arm { x11: true }
        );
        assert_eq!(
            plan_bind(OURS, honours, FORCE, true, Some(0), 1000),
            BindPlan::Off(BindOff::Disarmed)
        );
    }

    /// The two renderers must spell the same settings — the transient unit takes them as
    /// `systemd-run --property=`, the box's own unit as drop-in lines. A session that got only half
    /// of them is the crash this mechanism is guarding.
    #[test]
    fn both_bind_renderers_carry_the_socket_directory() {
        let bind = SessionBind {
            wrapper: std::path::PathBuf::from("/run/user/1000/punktfunk-gamescope-bin"),
            x11_dir: Some(std::path::PathBuf::from("/run/user/1000/punktfunk-x11")),
        };
        assert_eq!(
            bind.run_args(),
            vec![
                format!(
                    "--property=BindReadOnlyPaths=/run/user/1000/punktfunk-gamescope-bin:{DISTRO_GAMESCOPE_PATH}"
                ),
                format!("--property=BindPaths=/run/user/1000/punktfunk-x11:{X11_SOCKET_DIR}"),
            ]
        );
        assert_eq!(
            bind.unit_lines(),
            format!(
                "BindReadOnlyPaths=/run/user/1000/punktfunk-gamescope-bin:{DISTRO_GAMESCOPE_PATH}\n\
                 BindPaths=/run/user/1000/punktfunk-x11:{X11_SOCKET_DIR}\n"
            )
        );
        // The socket-directory bind must be read-WRITE (Xwayland creates its socket in there); a
        // read-only one would fail exactly as loudly as no bind at all.
        assert!(bind.unit_lines().contains("BindPaths="));

        // No compensation needed ⇒ exactly one setting, and the unit line ends in a newline so the
        // `Environment=` lines after it in the drop-in body still parse.
        let plain = SessionBind {
            wrapper: std::path::PathBuf::from("/run/user/1000/punktfunk-gamescope-bin"),
            x11_dir: None,
        };
        assert_eq!(plain.run_args().len(), 1);
        assert!(plain.unit_lines().ends_with('\n'));
        assert!(!plain.unit_lines().contains("BindPaths="));
    }

    /// The lines wlroots prints when the socket-directory check fails. They turn the backstop's
    /// message from "something went wrong" into a named cause.
    #[test]
    fn the_xwayland_refusal_is_recognised_from_a_real_log() {
        assert_eq!(
            xwayland_refusal_marker(
                "Error wlserver: [xwayland/sockets.c:100] /tmp/.X11-unix not owned by root or us\n"
            ),
            Some("not owned by root or us")
        );
        assert_eq!(
            xwayland_refusal_marker(
                "Error wlserver: [xwayland/sockets.c:217] No display available in the first 33\n"
            ),
            Some("No display available in the first")
        );
        // A session that failed for some other reason must not be reported as this bug — the
        // backstop still disarms, but it says so differently.
        assert_eq!(
            xwayland_refusal_marker("steam.sh: line 1: pipewire: command not found\n"),
            None
        );
    }

    /// `gamescope-session-plus` runs `export ENABLE_GAMESCOPE_WSI=1` before it launches anything,
    /// so that variable alone cannot turn the layer off. Only `DISABLE_GAMESCOPE_WSI`, which the
    /// script never mentions and which the Vulkan loader treats as an unconditional force-off,
    /// survives. Dropping it leaves a game with sound and input on a black screen. See [`WSI_OFF_ENV`].
    #[test]
    fn the_wsi_opt_out_carries_the_variable_the_session_script_cannot_clobber() {
        assert!(
            WSI_OFF_ENV.contains(&("DISABLE_GAMESCOPE_WSI", "1")),
            "the clobber-proof variable is the whole point of the opt-out"
        );

        // Both spellings reach both launch paths, and neither may lose the other.
        let args = WsiPlan::DistroDisabled.setenv_args();
        let lines = WsiPlan::DistroDisabled.unit_lines();
        for (name, value) in WSI_OFF_ENV {
            assert!(args.contains(&format!("--setenv={name}={value}")), "{name}");
            assert!(
                lines.contains(&format!("Environment={name}={value}\n")),
                "{name}"
            );
        }

        // Trailing newline: the drop-in body appends nothing after this block today, but the bind
        // lines above it rely on the same contract and the order has changed before.
        assert!(lines.ends_with('\n'));
    }

    /// Shipping our own layer means both halves happen in one session: ours is switched on AND
    /// the distro's is forced off. Enabling ours while leaving theirs live would put two gamescope
    /// WSI layers in the loader's implicit set. Assert the pair, not either half.
    #[test]
    fn our_own_layer_is_enabled_and_the_distro_one_forced_off_together() {
        let env = WsiPlan::Ours.env();
        let get = |k: &str| {
            env.iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| panic!("{k} missing from the Ours plan"))
        };

        assert_eq!(get("VK_ADD_IMPLICIT_LAYER_PATH"), our_wsi_layer_dir());
        assert_eq!(get("PUNKTFUNK_GAMESCOPE_WSI"), "1");
        // The clobber-proof one, for exactly the reason the test above states.
        assert_eq!(get("DISABLE_GAMESCOPE_WSI"), "1");
        assert_eq!(get("ENABLE_GAMESCOPE_WSI"), "0");

        // `DistroKept` must stay genuinely inert: it is the arm that runs on a box we decided not
        // to touch, so a stray variable there would change behaviour we promised not to change.
        assert!(WsiPlan::DistroKept.env().is_empty());
        assert!(WsiPlan::DistroKept.unit_lines().is_empty());
    }

    #[test]
    fn ours_layer_does_not_load_into_the_compositor() {
        let env = WsiPlan::Ours.compositor_env();
        assert!(
            !env.iter()
                .any(|(k, _)| *k == "VK_ADD_IMPLICIT_LAYER_PATH" || *k == "PUNKTFUNK_GAMESCOPE_WSI"),
            "the compositor must not search or enable our WSI layer"
        );
        assert_eq!(
            env.iter()
                .find(|(k, _)| *k == "DISABLE_GAMESCOPE_WSI")
                .map(|(_, v)| v.as_str()),
            Some("1")
        );
    }
}
