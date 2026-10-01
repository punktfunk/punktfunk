//! Headless gamescope virtual display: spawn or attach a nested compositor at the client's mode,
//! capture its PipeWire `Video/Source` node, inject through its EIS socket.
//!
//! Three routes, resolved per session by [`crate::resolve_gamescope_route`] and stored on
//! [`GamescopeDisplay`]. Never reread from the process env: a second connect would retarget
//! this instance between the decision and `create`. Dropping a spawned [`VirtualOutput`] kills
//! the process. Managed sessions live at host lifetime ([`Takeover`]); restore is this
//! module's job.
//!
//! Needs PipeWire + libei in gamescope, and a usable Vulkan device. Input: `inject/libei.rs`.
//! Takeover: `design/gamemode-and-dedicated-sessions.md`.

use super::{DisplayOwnership, Mode, VirtualDisplay, VirtualOutput};
use anyhow::{anyhow, bail, Context, Result};
use std::process::Command;
use std::time::{Duration, Instant};

#[path = "gamescope/argv.rs"]
mod argv;
#[path = "gamescope/bind.rs"]
mod bind;
#[path = "gamescope/discovery.rs"]
mod discovery;
#[path = "gamescope/dropins.rs"]
mod dropins;
#[path = "gamescope/heads.rs"]
mod heads;
#[path = "gamescope/sandbox.rs"]
pub(crate) mod sandbox;
#[path = "gamescope/seat.rs"]
pub(crate) mod seat;
#[path = "gamescope/spawn.rs"]
mod spawn;
#[path = "gamescope/splash.rs"]
mod splash;
#[path = "gamescope/steam_launch.rs"]
mod steam_launch;
#[path = "gamescope/takeover.rs"]
mod takeover;
#[path = "gamescope/wsi.rs"]
mod wsi;
// One namespace across the split: the submodules `use super::*` and see each other through here.
pub(crate) use discovery::{
    display_presenting, foreign_gamescope_running, game_session_exited,
    gamescope_can_composite_cursor, gamescope_hdr_capable, gamescope_offers_tiled_capture,
    is_available, steam_appid_from_launch, wait_for_steam_game_exit, xwayland_cursor_targets,
    SteamGameWatch,
};
pub(crate) use heads::list_monitors;
pub(crate) use splash::run as splash_run;
pub(crate) use steam_launch::{is_steam_launch, launch_into_session};
pub(crate) use takeover::{
    cancel_pending_restore, preflight_takeover_privilege, release_autologin_mask,
    restore_takeover_now, restore_takeover_on_startup, schedule_restore_tv_session,
    start_restore_worker, takeover_privilege_verdict,
};
use {argv::*, bind::*, discovery::*, dropins::*, spawn::*, steam_launch::*, takeover::*, wsi::*};

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

impl SessionState {
    fn matches(&self, mode: Mode, hdr: bool) -> bool {
        self.width == mode.width
            && self.height == mode.height
            && self.refresh_hz == mode.refresh_hz
            && self.hdr == hdr
    }
}

/// Serialises the managed-session launch in [`create_managed_session`] — and nothing else.
///
/// LOCK ORDER: `MANAGED_LAUNCH` → [`takeover()`], never the reverse. No restore path may take this
/// lock: `do_restore_tv_session` needs [`takeover()`] and must not sit behind a ~90 s launch. Two
/// Managed connects in one launch window both relaunch otherwise, and the second
/// `stop_session(SESSION_UNIT)` kills the unit the first is still polling.
static MANAGED_LAUNCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

const SESSION_UNIT: &str = "punktfunk-gamescope";
const SESSION_PLUS_BIN: &str = "/usr/share/gamescope-session-plus/gamescope-session-plus";

/// Game Mode's crash counters: a line per run that ends inside 60 s. [`SESSION_PLUS_BIN`]
/// re-bootstraps Steam and switches to desktop at five; SteamOS's `steam-short-session-tracker`
/// moves `~/.steam` aside at three. A run the host ends or restarts is never Steam failing.
const SHORT_SESSION_TRACKERS: [&str; 2] = [
    "/tmp/chimeraos-short-session-tracker",
    "/tmp/steamos-short-session-tracker",
];

fn forget_host_short_sessions() {
    for tracker in SHORT_SESSION_TRACKERS {
        let _ = std::fs::remove_file(tracker);
    }
}

/// Steam's reboot / power-off requests, which [`SESSION_PLUS_BIN`] honours only after Steam exits.
/// A run the host kills first leaves them behind, and the box's next Game Mode exit would obey.
const POWER_SENTINELS: [&str; 2] = [
    "/tmp/steamos-reboot-sentinel",
    "/tmp/steamos-shutdown-sentinel",
];

/// SteamOS session launcher (not Bazzite session-plus). `gamescope-session.service` execs
/// gamescope with hardcoded panel args. PATH-shim to `--backend headless -W <client> …` so
/// Steam starts inside that headless compositor.
const STEAMOS_SESSION_BIN: &str = "/usr/lib/steamos/gamescope-session";
const STEAMOS_SESSION_TARGET: &str = "gamescope-session.target";

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
        // Reuse key: a kept isolated spawn has this session's relay, Pulse and Steam-home env
        // baked in, and the seat home is a knob the operator can turn off between sessions.
        self.isolation.as_ref().map(crate::SessionIsolation::key)
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

    fn can_resize_kept(&self) -> bool {
        // Same route test as `poolable_now`: only a spawn of ours owns the seat whose atom we set.
        self.poolable_now() && gamescope_can_resize_output()
    }

    fn resize_kept(&mut self, seat: Option<&str>, mode: Mode) -> bool {
        self.can_resize_kept() && resize_kept_output(seat, mode)
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
        // This session's own Steam home, provisioned on first use. `None` keeps every path below
        // on the box's single Steam instance.
        let seat_home = self
            .isolation
            .as_ref()
            .filter(|_| steam)
            .and_then(|i| i.steam_home.as_deref())
            .and_then(seat::ensure_home);
        let box_steam = contends_for_box_steam(steam, seat_home.is_some());
        if box_steam {
            // No attach degrade here: a box without takeover privilege fails with the actionable error.
            stop_autologin_sessions()
                .context("dedicated Steam launch needs the box's gaming session freed")?;
            // Desktop Steam holds the instance too; autologin stop cannot see it.
            free_desktop_steam()?;
        } else if free_box_session_for_exclusive(box_steam, exclusive) {
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
            seat_home.as_deref(),
        )?;
        let mut proc = GamescopeProc {
            child,
            log: log.clone(),
            relay: self
                .isolation
                .as_ref()
                .map(|i| i.ei_relay.clone())
                .unwrap_or_else(ei_socket_file),
            steam_home: steam
                .then(|| {
                    seat_home
                        .clone()
                        .or_else(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
                })
                .flatten(),
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

/// Host-managed session at the client's mode, state in [`Takeover::managed`]. Reuse if mode and
/// node are live; otherwise relaunch — gamescope cannot change output mode live.
fn create_managed_session(client: &str, mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    // Not a bare `PENDING_RESTORE` clear: cancel also waits out a restore that already popped
    // (`keep_alive=off` is 0 s debounce).
    cancel_pending_restore();
    if steamos_session_present() {
        return create_managed_session_steamos(mode, hdr);
    }
    // Gated on the idled takeover, not `stopped_dm`: live takeovers leave the DM up, and that field
    // is what armed this. Skip and capture loss relaunches game mode over the booting desktop.
    if takeover_idled() && session_select_requested() {
        // Consume an adopted DM stop exactly once; a live takeover has none.
        let adopted_dm = takeover().stopped_dm.take();
        honor_session_select_switch(adopted_dm);
        return Err(anyhow!(
            "the user switched the box to the desktop session — the box's own game mode is handed \
             back; re-detection follows the desktop compositor as it comes up"
        ));
    }
    // While the selected desktop boots, a managed relaunch wins the race (gamescope+Steam start
    // faster than KWin). A live autologin unit supersedes: the user already switched back.
    let honor_pending = takeover()
        .switch_honored_at
        .is_some_and(|t| t.elapsed() < SWITCH_HONOR_GRACE);
    if honor_pending {
        if running_autologin_gamescope_unit().is_some() {
            takeover().switch_honored_at = None;
        } else {
            return Err(anyhow!(
                "waiting for the desktop session the user selected — refusing to relaunch game \
                 mode (re-detection follows the desktop once it's up)"
            ));
        }
    }
    // Never stop/relaunch here: post-capture-loss session detection can be stale.
    if crate::rebuild_probe_active() {
        // Not held across `pw-dump` / file write — that pins the restore worker.
        if managed_session_matches(mode, hdr) {
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
    // Decide under the lock, act outside it. Holding the takeover lock across `launch_session`
    // (~90 s) pins shutdown restore behind `native.rs`'s 20 s grace. [`MANAGED_LAUNCH`] is the
    // exclusion: held from before the decision so a second connect re-tests after the first
    // records, and touched by no restore path.
    let _launching = MANAGED_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    let same_mode = {
        let mut t = takeover();
        let same = t.managed.as_ref().is_some_and(|s| s.matches(mode, hdr));
        // Mode change: drop the tracked session so a concurrent restore does not read it as live.
        // During launch a session that stole nothing is invisible to `takeover_live`; holding the
        // guard instead pins shutdown restore. Failure arms a restore; success re-records.
        if !same {
            t.managed = None;
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
        takeover().managed = None;
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
    takeover().managed = Some(SessionState {
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
    takeover()
        .managed
        .as_ref()
        .is_some_and(|s| s.matches(mode, hdr))
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

/// SteamOS's session deletes the oldest installed games on every start while home has under
/// 500 MiB free. A takeover starts it twice, so it needs this much headroom.
const STEAMOS_MIN_FREE_MIB: u64 = 600;

fn refuse_low_disk_restart() -> Result<()> {
    match home_free_mib() {
        Some(free) if free < STEAMOS_MIN_FREE_MIB => bail!(
            "refusing the SteamOS takeover: home has {free} MiB free, and its Game Mode deletes \
             installed games on start to get back to 500 MiB"
        ),
        _ => Ok(()),
    }
}

fn home_free_mib() -> Option<u64> {
    use std::os::unix::ffi::OsStringExt;
    let home = std::ffi::CString::new(std::env::var_os("HOME")?.into_vec()).ok()?;
    // SAFETY: all-zero is a valid `statvfs`; the call only writes through the out-pointer.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `home` is NUL-terminated and outlives the call; `st` is a live out-pointer.
    if unsafe { libc::statvfs(home.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64 / (1024 * 1024))
}

/// SteamOS: PATH-shim + drop-in, restart `gamescope-session.target`. Restart kills any prior
/// gamescope, so discovery sees one node. Same-mode reconnect reuses.
fn create_managed_session_steamos(mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    // Held through the restart, so a second connect waits for this launch instead of restarting
    // the target again.
    let mut t = takeover();
    if t.managed.as_ref().is_some_and(|s| s.matches(mode, hdr)) {
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
        t.managed = None; // tracked session lost its node — fall through to a clean restart
    }
    // Reuse may attach; restarting the target would steal the seat from a session the user switched to.
    if crate::rebuild_probe_active() {
        return Err(anyhow!(
            "gamescope has no live node and this is an attach-only rebuild probe — refusing to \
             restart {STEAMOS_SESSION_TARGET} (the box may be mid-switch to another session; \
             re-detection follows it)"
        ));
    }
    refuse_low_disk_restart()?;
    let shim_dir = write_headless_shim()?;
    // Recorded before the drop-in exists: a crash from here on still owes the box its panel.
    t.steamos = true;
    persist_takeover_held(&t);
    if let Err(e) = write_steamos_dropin(&shim_dir, mode, hdr) {
        t.steamos = false;
        persist_takeover_held(&t);
        return Err(e);
    }
    systemctl_user(&["daemon-reload"]);
    // The restart's stop logs a line when Steam is under 60 s old; its start reads the count.
    forget_host_short_sessions();
    systemctl_user(&["restart", STEAMOS_SESSION_TARGET]);
    forget_host_short_sessions();
    drop(t);
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
    takeover().managed = Some(SessionState {
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
    takeover().forced_screen_env = true;
    systemctl_user(&[
        "set-environment",
        &format!("SCREEN_WIDTH={}", mode.width),
        &format!("SCREEN_HEIGHT={}", mode.height),
        &format!("CUSTOM_REFRESH_RATES={}", mode.refresh_hz.max(1)),
    ]);
    persist_takeover(); // takeover lock not held; these SCREEN_* outlive the process
    let mut bound = match write_gamescope_bin_wrapper()
        .and_then(|w| write_session_plus_dropin(&w, mode, hdr, WsiPlan::resolve()))
    {
        Ok(true) => {
            // Before the restart: skip this flag and `takeover_live()` is false, so the drop-in outlives us.
            takeover().session_dropin_armed = true;
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
            takeover().session_dropin_armed = false;
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

/// Steam first, asked to quit: a SIGKILL mid-write loses `config.vdf` or `registry.vdf`. Then
/// SIGKILL, not SIGTERM: gamescope's SIGTERM handler leaks the NVIDIA GPU context, after which
/// every later `vkCreateDevice` fails until reboot. Then `stop` + `reset-failed` so relaunch is clean.
fn kill_unit(unit: &str) {
    if let Some(pid) = steam_pid_in_unit(unit) {
        if !shut_steam_down(pid, STEAM_STOP_WAIT, None) {
            tracing::warn!(
                unit,
                pid,
                secs = STEAM_STOP_WAIT.as_secs(),
                "gamescope: Steam did not quit when asked — killing its session anyway"
            );
        }
    }
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

/// Attach path: autologin Xwayland is born `us`; `spawn::xkb_env` never reaches it.
/// Xwayland only — Wayland-native clients take the compositor keymap (`+pfhdr8`). Off via
/// `PUNKTFUNK_SESSION_LAYOUT`.
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
    let launch_gen = restore_generation();
    let start_unit = |bind: Option<&SessionBind>| -> Result<()> {
        // A hand-back since this launch began stopped the unit on purpose; relaunching it would
        // start a second Steam beside the box's own.
        if restore_generation() != launch_gen {
            bail!("the box's own session was handed back while this launch ran");
        }
        // A relaunch follows our own failed run; its line must not count toward the box's reset.
        forget_host_short_sessions();
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--collect", &format!("--unit={unit_name}")]);
        for arg in bind.map(SessionBind::run_args).unwrap_or_default() {
            cmd.arg(arg);
        }
        for arg in wsi.setenv_args(hdr) {
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
                our_flags(hdr, game).join(" ")
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

/// [`unit_starting_or_active`]'s opposite bias: unknown and timeout report `false`.
fn unit_known_active(unit: &str) -> bool {
    crate::proc::output_within(
        Command::new("systemctl").args(["--user", "is-active", unit]),
        UNIT_STATE_BUDGET,
    )
    .is_ok_and(|out| {
        matches!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "active" | "activating" | "reloading"
        )
    })
}

fn stop_session(unit_name: &str) {
    kill_unit(unit_name);
    let _ = std::fs::remove_file(ei_socket_file());
    forget_host_short_sessions();
    for sentinel in POWER_SENTINELS {
        let _ = std::fs::remove_file(sentinel);
    }
}

/// `$XDG_RUNTIME_DIR`, never world-writable `/tmp`: a second local user must not plant a rogue
/// EIS path. Reader also rejects a symlink.
pub fn ei_socket_file() -> std::path::PathBuf {
    // The path itself is the shared `pf_paths::gamescope_ei_socket_file` contract (also read by the
    // libei injector). Compute it under the session env lock so a concurrent session handshake's
    // `apply_session_env` XDG_RUNTIME_DIR retarget can't race this producer-side read.
    crate::with_env_lock(pf_paths::gamescope_ei_socket_file)
}
