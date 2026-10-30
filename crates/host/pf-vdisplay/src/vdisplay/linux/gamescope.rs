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
use crate::gamescope_launch as launch;
use crate::topology_journal::TakeoverStep;
use anyhow::{anyhow, bail, ensure, Context, Result};
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
mod sandbox;
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
    gamescope_can_composite_cursor, gamescope_captures_sdr10, gamescope_hdr_capable,
    gamescope_offers_tiled_capture, gamescope_planar_capture_in_vram, is_available,
    steam_appid_from_launch, wait_for_steam_game_exit, xwayland_cursor_targets, SteamGameWatch,
};
pub(crate) use heads::list_monitors;
pub(crate) use splash::run as splash_run;
pub(crate) use steam_launch::{is_steam_launch, launch_into_session};
pub(crate) use takeover::{
    cancel_pending_restore, held_takeover, preflight_takeover_privilege, release_autologin_mask,
    release_takeover, restore_takeover_now, restore_takeover_on_startup,
    schedule_restore_tv_session, start_restore_worker, takeover_privilege_verdict, HeldTakeover,
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
/// The unit under [`STEAMOS_SESSION_TARGET`] whose cgroup holds SteamOS's gamescope.
const STEAMOS_SESSION_UNIT: &str = "gamescope-session.service";

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
        self.isolation.as_ref().map(crate::SessionIsolation::key)
    }

    fn take_topology_restore(&mut self) -> Option<Box<dyn FnOnce() + Send>> {
        // Every spawn is its own group; cross-session ordering is `panel_dpms`'s refcount, not the group float.
        self.pending_restore.take()
    }

    /// A dead host's takeover step, from `topology_journal::reclaim`: adopted, not run here.
    /// Never reached through `apply`: the takeover hands the registry no seqs.
    fn undo(&mut self, undo: &crate::topology_journal::Undo) -> Result<()> {
        adopt(undo)
    }

    fn ownership(&self) -> DisplayOwnership {
        // Must agree with what `create` does with the same route — not [`crate::launch_is_nested`],
        // which is `false` for `None`. `None` falls through to bare spawn. A managed route that
        // degrades to attach reports `External`; neither is pooled.
        match self.route {
            Some(crate::GamescopeRoute::Managed { .. }) => DisplayOwnership::SessionManaged,
            Some(crate::GamescopeRoute::Attach { .. }) => DisplayOwnership::External,
            Some(crate::GamescopeRoute::Spawn) | None => DisplayOwnership::Spawned,
        }
    }

    fn nested_launch_started(&self) -> bool {
        self.spawned_nested_launch
    }

    fn sole_instance(&self) -> bool {
        // Bare spawn only; managed/attach do not own the socket name. A second spawn is what
        // breaks the `gamescope-N` lock and Steam.
        self.ownership() == DisplayOwnership::Spawned
    }

    fn kept_display_alive(&mut self, node_id: u32) -> bool {
        // Nested gamescope dies with its game. `false` makes the registry recreate instead of a ~10 s
        // first-frame retry on a dead node.
        gamescope_node_present(node_id)
    }

    fn can_resize_kept(&self) -> bool {
        // Only a spawn of ours owns the seat whose atom we set.
        self.ownership() == DisplayOwnership::Spawned && gamescope_can_resize_output()
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
        // Sampled once so managed hold, exclusive session-free, and spawn darken cannot disagree.
        let exclusive =
            crate::effective_topology(self.client_fp) == crate::policy::Topology::Exclusive;
        // This session's route — never the process env, or a second connect retargets this one.
        let route = self.route.clone();
        launch_tracked(|| match route {
            Some(crate::GamescopeRoute::Managed { client }) => {
                // A joiner shares the running session. Relaunching it at another mode or HDR
                // would end the owner's stream.
                if self.join_live && !managed_session_matches(mode, self.hdr) {
                    bail!(
                        "join the running gamescope session: it runs at another mode or HDR, \
                         and relaunching it would end the owner's stream"
                    );
                }
                let out = create_managed_session(&client, mode, self.hdr)?;
                if !self.join_live {
                    takeover().owner = self.client_fp;
                }
                // Idling autologin leaves the CRTC configured. The hold cannot ride
                // `pending_restore`: this route is `SessionManaged`, so the registry never picks
                // it up. Release is [`do_restore_tv_session`].
                managed_darken_acquire(exclusive);
                Ok(out)
            }
            Some(crate::GamescopeRoute::Attach { node }) => create_attach(&node, mode, self.hdr),
            // No resolver on this path: bare spawn, the ladder default.
            Some(crate::GamescopeRoute::Spawn) | None => self.create_spawn(mode, exclusive),
        })
    }
}

impl GamescopeDisplay {
    /// Bare headless gamescope at `mode`, running [`VirtualDisplay::set_launch_command`]. The
    /// output owns the process. `exclusive` is the topology `create` sampled.
    fn create_spawn(&mut self, mode: Mode, exclusive: bool) -> Result<VirtualOutput> {
        // Diagnostic only: warns on known-deadlock-prone versions.
        check_gamescope_version();
        // Resolve once before the gate and hand the same answer to [`spawn`]. Gating on `self.cmd`
        // alone while spawn fell back to `PUNKTFUNK_GAMESCOPE_APP` would pass `--steam` with no
        // instance free.
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
            steam_home: steam
                .then(|| std::env::var_os("HOME").map(std::path::PathBuf::from))
                .flatten(),
            started: Instant::now(),
        };
        // Give up early if the process is already gone: a `vkCreateDevice` failure exits in under
        // a second, and waiting 15 s on its corpse would blame the GPU. Dropping `proc` logs the
        // tail of gamescope's own log, which says which.
        let node_id =
            wait_for_node(Duration::from_secs(15), &log, &mut proc.child).ok_or_else(|| {
                anyhow!(
                    "gamescope published no PipeWire node within 15s (or exited first) — it may \
                     have failed to start, or headless capture may be unsupported on this \
                     GPU/driver; the tail of its log just above says which"
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
        out.ownership = DisplayOwnership::Spawned;
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

/// Capture + inject an already-running gamescope; no lifecycle ownership. `auto` is the box's own
/// session, set to `mode` first.
fn create_attach(node: &str, mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    let auto = node.trim().eq_ignore_ascii_case("auto");
    let node_id: u32 = if auto {
        // Headless box: game-mode resolution is ours. Skip and the client gets the box default.
        ensure_box_gamescope_mode(mode, hdr)?
    } else {
        node.parse()
            .context("PUNKTFUNK_GAMESCOPE_NODE must be a node id or 'auto'")?
    };
    Ok(attach_output(node_id, mode, auto))
}

/// The output of an attach to `node_id`. `own`: the node is the box's session, so a head it
/// lights sets the stream's mode; the client's `mode` stands only for a headless re-mode or an
/// operator-named node. Attach mirrors a gamescope that may be lighting the panel: darkening
/// it would darken the picture streamed, so Exclusive cannot be served on this route.
fn attach_output(node_id: u32, mode: Mode, own: bool) -> VirtualOutput {
    point_injector_at_eis(Scope::Box);
    let (w, h, hz) =
        own.then(heads::own_mode)
            .flatten()
            .unwrap_or((mode.width, mode.height, mode.refresh_hz));
    tracing::info!(
        node_id,
        w,
        h,
        hz,
        "gamescope: attaching to existing PipeWire node"
    );
    VirtualOutput {
        node_id,
        remote_fd: None,
        preferred_mode: Some((w, h, hz)),
        keepalive: Box::new(()),
        ownership: DisplayOwnership::External,
        reused_gen: None,
        pool_gen: None,
        expect_exact_dims: false,
        output_name: None, // EIS seat, not a wlr virtual pointer to aim by name
        input_output: None,
        seat: None,
        pid: None,
    }
}

/// Host-managed session at the client's mode, state in [`Takeover::managed`]. Reuse if mode and
/// node are live; otherwise relaunch — gamescope cannot change output mode live.
fn create_managed_session(client: &str, mode: Mode, hdr: bool) -> Result<VirtualOutput> {
    // Not a bare clear: cancel also waits out a restore that already popped (`keep_alive=off` is
    // 0 s debounce).
    cancel_pending_restore();
    if steamos_session_present() {
        return create_managed_session_steamos(mode, hdr);
    }
    // Gated on the idled takeover, not an adopted DM stop: live takeovers leave the DM up, and the
    // idle drop-in is what armed this. Skip and capture loss relaunches game mode over the booting
    // desktop.
    if takeover_idled() && session_select_requested() {
        // Consume an adopted DM stop exactly once; a live takeover has none.
        honor_session_select_switch(take_adopted_dm());
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
            if let Some(node_id) = find_gamescope_node(Scope::Unit(SESSION_UNIT)) {
                point_injector_at_eis(Scope::Unit(SESSION_UNIT));
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
        return Ok(attach_output(node_id, mode, true));
    }
    // Desktop Steam also holds the instance; SESSION_UNIT's own Steam is exempt via cgroup.
    free_desktop_steam()?;
    // Decide under the lock, act outside it. Holding the takeover lock across `launch_session`
    // (~90 s) pins shutdown restore behind `native.rs`'s 20 s grace. [`Launching`] is the
    // exclusion: taken before the decision so a second connect re-tests after the first records.
    let _launching = Launching::begin();
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
        if let Some(node_id) = find_gamescope_node(Scope::Unit(SESSION_UNIT)) {
            point_injector_at_eis(Scope::Unit(SESSION_UNIT));
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
    takeover().owe_once(TakeoverStep::StopSession);
    // Holding nothing: `launch_session` stops the old unit first, so discovery sees one node.
    let node_id = match launch_session(client, SESSION_UNIT, mode, hdr) {
        Ok(id) => id,
        Err(e) => {
            if !session_unit_running() {
                takeover().settle(|s| matches!(s, TakeoverStep::StopSession));
            }
            // No session came up, and the old one was stopped first: hand the box back.
            hand_back_after_failed_launch();
            return Err(e);
        }
    };
    // Only a write from inside this session should read as a switch, not the one that led here.
    record_session_select_baseline();
    point_injector_at_eis(Scope::Unit(SESSION_UNIT));
    takeover().managed = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
    });
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
pub(crate) fn steamos_session_present() -> bool {
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
pub(crate) fn systemctl_user(args: &[&str]) {
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
    // One launch at a time, through the restart and the node wait: a second connect that
    // restarted the target meanwhile would kill this session and hand its own node to this wait.
    // The session-plus path takes the same guard.
    let _launching = Launching::begin();
    let mut t = takeover();
    if t.managed.as_ref().is_some_and(|s| s.matches(mode, hdr)) {
        if let Some(node_id) = find_gamescope_node(Scope::Unit(STEAMOS_SESSION_UNIT)) {
            point_injector_at_eis(Scope::Unit(STEAMOS_SESSION_UNIT));
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
    // Owed before the drop-in exists: a crash from here on still owes the box its panel.
    let owed_here = t.owe_once(TakeoverStep::RestoreSteamos);
    if let Err(e) = write_steamos_dropin(&shim_dir, mode, hdr) {
        if owed_here {
            t.settle(|s| matches!(s, TakeoverStep::RestoreSteamos));
        }
        return Err(e);
    }
    // A hand-back bumps this and ends the wait. Not held across the restart: its budget would
    // pin a shutdown hand-back past its grace.
    let launch_gen = t.generation;
    drop(t);
    let node_id = relaunch_steamos_target(hdr, launch_gen)?;
    point_injector_at_eis(Scope::Unit(STEAMOS_SESSION_UNIT));
    takeover().managed = Some(SessionState {
        width: mode.width,
        height: mode.height,
        refresh_hz: mode.refresh_hz,
        hdr,
    });
    tracing::info!(
        node_id,
        w = mode.width,
        h = mode.height,
        hz = mode.refresh_hz,
        "gamescope (SteamOS): took over gamescope-session.target headless at the client's mode"
    );
    Ok(managed_output(node_id, mode))
}

/// The target restart's budget: Steam's clean stop, then the headless shim's 40 s wait for
/// gamescope's ready line. A restart past it is a failed launch.
const STEAMOS_RESTART_BUDGET: Duration = Duration::from_secs(60);

/// How long a restarted target has to publish its node.
const STEAMOS_NODE_WAIT: Duration = Duration::from_secs(30);

/// Restart [`STEAMOS_SESSION_TARGET`] onto the drop-in and wait for its node. Any failure hands
/// the box back: the target may already run headless.
fn relaunch_steamos_target(hdr: bool, launch_gen: u64) -> Result<u32> {
    systemctl_user(&["daemon-reload"]);
    // The restart's stop logs a line when Steam is under 60 s old; its start reads the count.
    forget_host_short_sessions();
    // Any gamescope that started before this is the previous session's.
    let restarted_at = crate::proc::uptime_ticks();
    let restarted = issue_verb_within(&["restart", STEAMOS_SESSION_TARGET], STEAMOS_RESTART_BUDGET);
    forget_host_short_sessions();
    let verb = match restarted {
        RestoreVerb::Done => launch::Verb::Done,
        RestoreVerb::StillRunning => launch::Verb::StillRunning,
        RestoreVerb::Failed(why) => {
            launch::Verb::Failed(format!("restart {STEAMOS_SESSION_TARGET}: {why}"))
        }
    };
    let out = await_steamos_node(&verb, restarted_at, hdr, launch_gen);
    if out.is_err() {
        hand_back_after_failed_launch();
    }
    out
}

/// Look at the restarted target until [`launch::step`] stops waiting. `restarted_at` is the
/// restart's instant in [`crate::proc::uptime_ticks`].
fn await_steamos_node(
    restarted: &launch::Verb,
    restarted_at: Option<u64>,
    hdr: bool,
    launch_gen: u64,
) -> Result<u32> {
    use launch::{Flags, Node, Verb, Verdict, Why};
    let scope = Scope::Unit(STEAMOS_SESSION_UNIT);
    let deadline = Instant::now() + STEAMOS_NODE_WAIT;
    loop {
        let verb = match restarted {
            Verb::Done if restore_generation() != launch_gen => Verb::Failed(format!(
                "the box was handed back while {STEAMOS_SESSION_TARGET} restarted"
            )),
            Verb::Done if unit_gone(STEAMOS_SESSION_UNIT) => Verb::Failed(format!(
                "{STEAMOS_SESSION_UNIT} exited after the restart — check \
                 `journalctl --user -u {STEAMOS_SESSION_UNIT}`"
            )),
            Verb::Done if Instant::now() >= deadline => Verb::TimedOut,
            v => v.clone(),
        };
        let found = matches!(verb, Verb::Done | Verb::TimedOut)
            .then(|| poll_managed_node(STEAMOS_SESSION_UNIT))
            .flatten();
        let node = found
            .as_ref()
            .map_or(Node::None, |n| Node::aged(n.started, restarted_at));
        // Only a younger node is checked: the check latches the flags off for the host's life.
        let checked = (node == Node::Younger).then(|| verify_managed_spawn_flags(hdr, scope));
        let flags = match &checked {
            None => Flags::Unverifiable,
            Some(Ok(())) => Flags::Present,
            Some(Err(_)) => Flags::Missing,
        };
        match (launch::step(&verb, node, flags), found, checked) {
            (Verdict::Use, Some(n), _) => return Ok(n.id),
            (Verdict::LatchLost, _, Some(Err(e))) => return Err(e),
            (Verdict::HandBack(Why::Failed(why)), ..) => bail!("{why}"),
            (Verdict::HandBack(Why::StillRunning), ..) => bail!(
                "restart {STEAMOS_SESSION_TARGET}: still running after {}s (Steam closing a \
                 game is the usual cause)",
                STEAMOS_RESTART_BUDGET.as_secs()
            ),
            (Verdict::HandBack(Why::NoNode), ..) => bail!(
                "SteamOS headless gamescope node did not appear within {}s after restarting \
                 {STEAMOS_SESSION_TARGET} — check `journalctl --user -u {STEAMOS_SESSION_UNIT}`",
                STEAMOS_NODE_WAIT.as_secs()
            ),
            _ => std::thread::sleep(Duration::from_millis(300)),
        }
    }
}

/// Attach at the client's resolution: reuse if the box session already matches; otherwise restart
/// the box's own autologin unit. Never spawn a competing one. Steam restarts only on a real change.
/// A unit that exits after the restart ends the wait, as the 45 s deadline does.
fn ensure_box_gamescope_mode(mode: Mode, hdr: bool) -> Result<u32> {
    let target = (mode.width, mode.height);
    // Three-state: collapsing unknown with a known size would restart the box's session.
    let size = box_output_size(Scope::Box);
    if size == BoxOutputSize::Known(target) {
        if let Some(node) = find_gamescope_node(Scope::Box) {
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
        if let Some(node) = find_gamescope_node(Scope::Box) {
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
        let node = find_gamescope_node(Scope::Box).ok_or_else(|| {
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
        let node = find_gamescope_node(Scope::Box).ok_or_else(|| {
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
        return find_gamescope_node(Scope::Box).ok_or_else(|| {
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
    // Owed before they are set: they outlive the process.
    takeover().owe_once(TakeoverStep::UnsetScreenEnv);
    systemctl_user(&[
        "set-environment",
        &format!("SCREEN_WIDTH={}", mode.width),
        &format!("SCREEN_HEIGHT={}", mode.height),
        &format!("CUSTOM_REFRESH_RATES={}", mode.refresh_hz.max(1)),
    ]);
    // Owed before it is written: an unowed drop-in outlives the takeover, and Game Mode then runs
    // our patched gamescope.
    let owed_here = takeover().owe_once(TakeoverStep::RemoveSessionDropin);
    let mut bound = match write_gamescope_bin_wrapper()
        .and_then(|w| write_session_plus_dropin(&w, mode, hdr, WsiPlan::resolve()))
    {
        Ok(true) => {
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
            // No bind to arm also removes; the takeover must follow or restore owes a gone drop-in.
            takeover().settle(|s| matches!(s, TakeoverStep::RemoveSessionDropin));
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, "gamescope: box-session drop-in not written");
            if owed_here {
                takeover().settle(|s| matches!(s, TakeoverStep::RemoveSessionDropin));
            }
            false
        }
    };
    // Also reloads a removal: a drop-in systemd has not reloaded still applies at next start.
    systemctl_user(&["daemon-reload"]);
    systemctl_user(&["restart", &unit]);
    let mut deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if any_output_size_is(&scoped_argvs(Scope::Unit(&unit)), target) {
            if let Some(node) = find_gamescope_node(Scope::Unit(&unit)) {
                tracing::info!(
                    node,
                    w = mode.width,
                    h = mode.height,
                    "gamescope: box game-mode session relaunched at the client's resolution"
                );
                return Ok(node);
            }
        }
        let exited = unit_gone(&unit);
        if exited || Instant::now() >= deadline {
            // Bind killing the box's own session hands the seat to the desktop. One more try without it.
            if bound {
                note_bind_hazard(&unit);
                disarm_session_plus_dropin(); // also clears the flag: there is nothing left to undo
                systemctl_user(&["restart", &unit]);
                bound = false;
                deadline = Instant::now() + Duration::from_secs(45);
                continue;
            }
            ensure!(
                !exited,
                "box game-mode session {unit} exited after its relaunch at {}x{} — check \
                 `journalctl --user -u {unit}`",
                mode.width,
                mode.height
            );
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
        if !shut_steam_down(pid, STEAM_STOP_WAIT) {
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

/// Point the libei injector at the EIS socket of the gamescope in `scope` (it reads the relay
/// file [`ei_socket_file`]). Best-effort — video still works without it (input just won't reach
/// the session). Shared by the attach and host-managed-session paths.
fn point_injector_at_eis(scope: Scope<'_>) {
    match find_gamescope_eis_socket(scope) {
        Some(sock) => {
            // Line 2 is WxH: EIS advertises INT32_MAX, so the injector cannot learn geometry.
            // Socket and size come from different sources; omit the hint unless every gamescope agrees.
            let size = current_gamescope_output_size(scope);
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
    let node_id = find_gamescope_node(Scope::Box).ok_or_else(|| {
        anyhow!(
            "gamescope is driving {connector:?} but publishes no PipeWire Video/Source node — the \
             session may still be starting, or this gamescope was built without PipeWire support"
        )
    })?;
    // EIS advertises INT32_MAX; the output-size hint here is what scales client positions.
    point_injector_at_eis(Scope::Box);
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
                             // Headless `--nested-refresh` IS the output refresh. `CUSTOM_REFRESH_RATES` is the offered set,
                             // inert on stock gamescope; it cannot fix a wrong nested-refresh.
    let game = game_hz(mode.refresh_hz);
    let offered = refresh_rate_list(
        mode.refresh_hz.max(1),
        &pf_host_config::config().gamescope_refresh_rates,
    );
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
        let env = bind
            .map(SessionBind::run_args)
            .unwrap_or_default()
            .into_iter()
            .chain(wsi.setenv_args(hdr))
            .chain(xkb_setenv_args())
            .chain(discovery::reaper_path_env().map(|path| format!("--setenv=PATH={path}")))
            .collect();
        let flags = our_flags(hdr, game);
        let mut cmd = Command::new("systemd-run");
        cmd.args(session_unit_args(
            unit_name, client, mode, game, &offered, &wrapper, &flags, env,
        ));
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
        if let Some(id) = find_gamescope_node(Scope::Unit(unit_name)) {
            // Convention, not a guarantee. Stop on rejection so the retry relaunches.
            if let Err(e) = verify_managed_spawn_flags(hdr, Scope::Unit(unit_name)) {
                stop_session(unit_name);
                return Err(e);
            }
            warn_if_mode_lost(mode, game, Scope::Unit(unit_name));
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
        // Unknown and `deactivating` count as alive, so a hiccup cannot start a relaunch storm.
        let alive = unit_state(unit_name).is_none_or(|s| {
            matches!(
                s.as_str(),
                "active" | "activating" | "reloading" | "deactivating"
            )
        });
        if !alive {
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

/// `systemd-run` argv for the managed session's transient unit. `env` is the part read from the box
/// (bind, WSI, XKB, `PATH`), in that order; `flags` is [`our_flags`].
#[allow(clippy::too_many_arguments)] // one unit spec, one call site
fn session_unit_args(
    unit: &str,
    client: &str,
    mode: Mode,
    game: u32,
    offered: &str,
    wrapper: &std::path::Path,
    flags: &[String],
    env: Vec<String>,
) -> Vec<String> {
    let mut args = vec![
        "--user".to_string(),
        "--collect".to_string(),
        format!("--unit={unit}"),
    ];
    args.extend(env);
    args.extend([
        // Stale desktop DISPLAY/WAYLAND_DISPLAY in the manager env would abort gamescope.
        "--property=UnsetEnvironment=DISPLAY WAYLAND_DISPLAY".to_string(),
        "--setenv=BACKEND=headless".to_string(),
        format!("--setenv=SCREEN_WIDTH={}", mode.width),
        format!("--setenv=SCREEN_HEIGHT={}", mode.height),
        format!("--setenv=PF_HZ={game}"),
        // Unquoted: wrapper word-splits. Empty for stock-gamescope SDR.
        format!("--setenv=PF_HDR_ARGS={}", flags.join(" ")),
        format!("--setenv=GAMESCOPE_BIN={}", wrapper.display()),
        "--setenv=DRM_MODE=cvt".to_string(),
        format!("--setenv=CUSTOM_REFRESH_RATES={offered}"),
        "--".to_string(),
        SESSION_PLUS_BIN.to_string(),
        client.to_string(),
    ]);
    args
}

/// `systemctl --user is-active`'s word for `unit`; `None` when the manager did not answer in
/// budget. Each caller picks how an unknown state and `deactivating` read.
fn unit_state(unit: &str) -> Option<String> {
    let out = crate::proc::output_within(
        Command::new("systemctl").args(["--user", "is-active", unit]),
        UNIT_STATE_BUDGET,
    )
    .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A restart job passes through `inactive` between its stop and its start, so a stopped unit
/// counts as gone only when it reads stopped again this much later.
const UNIT_GONE_CONFIRM: Duration = Duration::from_millis(500);

/// `unit` is `inactive` or `failed`, and stays so across [`UNIT_GONE_CONFIRM`]. An unknown state
/// is alive: a wait that reads it keeps waiting.
fn unit_gone(unit: &str) -> bool {
    let stopped = || unit_state(unit).is_some_and(|s| matches!(s.as_str(), "inactive" | "failed"));
    stopped() && {
        std::thread::sleep(UNIT_GONE_CONFIRM);
        stopped()
    }
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
    // The shared `pf_paths::gamescope_ei_socket_file` contract: the libei injector reads it
    // from the same stored session.
    pf_paths::gamescope_ei_socket_file(pf_host_config::session::runtime_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake `systemctl` for `test`'s [`crate::fake::in_child`]: its directory, and the `PATH`
    /// that puts it first. `systemctl --user <verb>` appends its arguments to
    /// `$HOME/systemctl-calls`, prints `$HOME/systemctl-<verb>` and exits with the code in
    /// `$HOME/systemctl-exit`, 1 when there is none.
    pub(super) fn fake_systemctl(test: &str) -> (std::path::PathBuf, String) {
        use std::os::unix::fs::PermissionsExt;
        let fake = std::env::temp_dir().join(format!("pf-systemctl-{test}-{}", std::process::id()));
        std::fs::create_dir_all(&fake).expect("fake bin dir");
        let systemctl = fake.join("systemctl");
        let body = "#!/bin/sh\necho \"$*\" >> \"$HOME/systemctl-calls\"\n\
                    cat \"$HOME/systemctl-$2\" 2>/dev/null\n\
                    exit \"$(cat \"$HOME/systemctl-exit\" 2>/dev/null || echo 1)\"\n";
        std::fs::write(&systemctl, body).expect("write the fake");
        std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755))
            .expect("chmod the fake");
        let path = format!(
            "{}:{}",
            fake.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        (fake, path)
    }

    /// A restart systemd refuses hands the box back at once with systemd's word, and no node
    /// wait. `systemctl` is a fake on the child's PATH that exits 1.
    #[test]
    fn a_refused_restart_hands_the_box_back_at_once() {
        let name = "a_refused_restart_hands_the_box_back_at_once";
        let (fake, path) = fake_systemctl(name);
        crate::fake::in_child(module_path!(), name, &[("PATH", &path)], || {
            takeover().owe_once(TakeoverStep::RestoreSteamos);
            let started = Instant::now();
            let err = relaunch_steamos_target(false, restore_generation())
                .expect_err("a refused restart is a failed launch");
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
            assert_eq!(
                format!("{err:#}"),
                "restart gamescope-session.target: systemctl exited with exit status: 1"
            );
            let due = held_takeover().and_then(|h| h.restore_in);
            assert!(due.is_some_and(|d| d <= Duration::from_secs(5)), "{due:?}");
        });
        let _ = std::fs::remove_dir_all(&fake);
    }

    /// A session unit that dies after a restart systemd accepted ends the node wait at once, and
    /// the box goes back. The fake `systemctl` restarts fine and reports the unit `failed`.
    #[test]
    fn a_unit_that_dies_after_the_restart_ends_the_wait() {
        let name = "a_unit_that_dies_after_the_restart_ends_the_wait";
        let (fake, path) = fake_systemctl(name);
        crate::fake::in_child(module_path!(), name, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            std::fs::write(home.join("systemctl-exit"), "0").unwrap();
            std::fs::write(home.join("systemctl-is-active"), "failed\n").unwrap();
            takeover().owe_once(TakeoverStep::RestoreSteamos);
            let started = Instant::now();
            let err = relaunch_steamos_target(false, restore_generation())
                .expect_err("a dead unit publishes no node");
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{:?}",
                started.elapsed()
            );
            assert!(
                format!("{err:#}").starts_with("gamescope-session.service exited"),
                "{err:#}"
            );
            let due = held_takeover().and_then(|h| h.restore_in);
            assert!(due.is_some_and(|d| d <= Duration::from_secs(5)), "{due:?}");
        });
        let _ = std::fs::remove_dir_all(&fake);
    }

    #[test]
    fn the_managed_session_unit_argv_keeps_its_shape() {
        let args = session_unit_args(
            "punktfunk-gamescope",
            "steam",
            Mode {
                width: 2560,
                height: 1440,
                refresh_hz: 120,
            },
            60,
            "60,120",
            std::path::Path::new("/run/user/1000/gamescope-bin"),
            &["--hdr-enabled".to_string(), "--adaptive-sync".to_string()],
            vec![
                "--property=BindReadOnlyPaths=/w:/usr/bin/gamescope".to_string(),
                "--setenv=XKB_DEFAULT_LAYOUT=de".to_string(),
            ],
        );
        assert_eq!(
            args,
            [
                "--user",
                "--collect",
                "--unit=punktfunk-gamescope",
                "--property=BindReadOnlyPaths=/w:/usr/bin/gamescope",
                "--setenv=XKB_DEFAULT_LAYOUT=de",
                "--property=UnsetEnvironment=DISPLAY WAYLAND_DISPLAY",
                "--setenv=BACKEND=headless",
                "--setenv=SCREEN_WIDTH=2560",
                "--setenv=SCREEN_HEIGHT=1440",
                "--setenv=PF_HZ=60",
                "--setenv=PF_HDR_ARGS=--hdr-enabled --adaptive-sync",
                "--setenv=GAMESCOPE_BIN=/run/user/1000/gamescope-bin",
                "--setenv=DRM_MODE=cvt",
                "--setenv=CUSTOM_REFRESH_RATES=60,120",
                "--",
                SESSION_PLUS_BIN,
                "steam",
            ]
        );
    }
}
