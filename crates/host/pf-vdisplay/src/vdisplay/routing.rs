//! Gamescope sub-mode, injector id, dedicated-session, and TV-restore routing.
//!
//! Decisions travel as [`GamescopeRoute`] values on the backend instance — never
//! through process env, or a concurrent session retargets this one. Operator
//! knobs (`PUNKTFUNK_GAMESCOPE_*`) are sampled once. Pin: tests below.
//! Design: `design/gamemode-and-dedicated-sessions.md`.

use super::*;

/// Resolved gamescope sub-mode plus the payload `GamescopeDisplay::create` needs.
///
/// Carried on the backend instance (`VirtualDisplay::set_gamescope_route`), never
/// through process env — two sessions connecting at once would otherwise clobber
/// each other. Operator knobs stay operator overrides, sampled once
/// (`operator_gamescope`); nothing writes them.
///
/// Defined on every platform: the host's `SessionContext` carries it beside
/// `compositor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GamescopeRoute {
    /// Host-managed session-plus / SteamOS at the client's mode. `client` is the
    /// flavour (`steam`), from `PUNKTFUNK_GAMESCOPE_SESSION` if set.
    Managed { client: String },
    /// Capture an already-running gamescope. `node` is a PipeWire id, or `auto` to
    /// discover the box's own session (`PUNKTFUNK_GAMESCOPE_NODE` if set).
    Attach { node: String },
    /// Bare-spawn a headless gamescope, nesting this session's launch command.
    Spawn,
}

/// Ladder verdict before the [`GamescopeRoute`] payload is attached.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GamescopeMode {
    Managed,
    /// Capture + inject; no lifecycle ownership.
    Attach,
    Spawn,
}

/// Pure ladder. Inputs are parameters so tests can pin it.
///
/// Managed only when session-plus/SteamOS is actually on the box — an
/// unconditional managed default kills the session on a plain distro.
/// Dedicated spawn is a body rung, below the operator MANAGED/ATTACH/NODE
/// overrides.
#[cfg(target_os = "linux")]
fn pick_gamescope_mode(
    dedicated_launch: bool,
    force_managed: bool,
    attach_env: bool,
    node_env: bool,
    session_env: bool,
    managed_infra: bool,
    foreign_gamescope: bool,
) -> GamescopeMode {
    if force_managed {
        GamescopeMode::Managed
    } else if attach_env || node_env {
        GamescopeMode::Attach
    } else if dedicated_launch {
        // Dedicated spawn outranks managed-infra / foreign-attach, not the
        // operator MANAGED/ATTACH/NODE overrides above (debug/CI).
        GamescopeMode::Spawn
    } else if session_env || managed_infra {
        GamescopeMode::Managed
    } else if foreign_gamescope {
        GamescopeMode::Attach
    } else {
        GamescopeMode::Spawn
    }
}

/// The `managed_infra` rung of [`pick_gamescope_mode`]. The SteamOS-session route restarts
/// `gamescope-session.target`; off SteamOS that ends a live desktop session, the host with it.
#[cfg(target_os = "linux")]
fn managed_infra_usable(
    installed: bool,
    target_restart: bool,
    desktop_active: bool,
    steamos: bool,
) -> bool {
    installed && !(target_restart && desktop_active && !steamos)
}

/// The two env knobs the host itself may publish, sampled once at first use.
///
/// `node_env` sits above `dedicated_launch` in [`pick_gamescope_mode`]. A live
/// re-read of a published `_NODE=auto` would latch Attach for the host's life
/// and override `game_session=dedicated`. Sampling once means "the operator
/// set this before we ran". The console's two switches are not latched: nothing
/// in the host writes them, and their rows promise the next session.
///
/// Pin: `operator_overrides_do_not_see_our_own_writes`.
#[cfg(target_os = "linux")]
static OPERATOR_ENV: std::sync::OnceLock<(Option<String>, Option<String>)> =
    std::sync::OnceLock::new();

#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
struct OperatorGamescope {
    managed: bool,
    attach: bool,
    /// `PUNKTFUNK_GAMESCOPE_NODE` if set. The ladder needs presence; the route needs the value.
    node: Option<String>,
    /// `PUNKTFUNK_GAMESCOPE_SESSION` — the managed session flavour.
    session: Option<String>,
}

/// The operator's gamescope knobs as they stand for this connect: the console's switches
/// read live, the two env knobs from the one-time sample ([`OPERATOR_ENV`]).
#[cfg(target_os = "linux")]
fn operator_gamescope() -> OperatorGamescope {
    operator_gamescope_from(|k| std::env::var(k).ok())
}

/// [`operator_gamescope`] with `get` as the environment. `get` runs under the env lock for the
/// one-time sample only; once [`OPERATOR_ENV`] holds it, no call consults `get` again.
#[cfg(target_os = "linux")]
fn operator_gamescope_from(get: impl Fn(&str) -> Option<String>) -> OperatorGamescope {
    let (node, session) = OPERATOR_ENV.get_or_init(|| {
        let env = with_env_lock(|| {
            let v = |k: &str| get(k).filter(|v| !v.is_empty());
            (
                v("PUNKTFUNK_GAMESCOPE_NODE"),
                v("PUNKTFUNK_GAMESCOPE_SESSION"),
            )
        });
        if env.0.is_some() || env.1.is_some() {
            tracing::info!(
                node = ?env.0,
                session = ?env.1,
                "gamescope: operator sub-mode overrides sampled from the environment"
            );
        }
        env
    });
    // Presence is not the grammar: `PUNKTFUNK_GAMESCOPE_ATTACH=0` must be off, as the
    // console shows it. This rung outranks dedicated spawn, so a silent inversion costs
    // the client its own display.
    OperatorGamescope {
        managed: pf_host_config::row_bool("PUNKTFUNK_GAMESCOPE_MANAGED"),
        attach: pf_host_config::row_bool("PUNKTFUNK_GAMESCOPE_ATTACH"),
        node: node.clone(),
        session: session.clone(),
    }
}

/// Injector id that matches this video backend — the one place the pair is
/// chosen, so they cannot diverge. Caller publishes it via
/// `pf_inject::set_backend_id` next to [`resolve_gamescope_route`].
///
/// `&'static str` because this crate must not depend on pf-inject; the five
/// Linux ids are the injector's `PUNKTFUNK_INPUT_BACKEND` vocabulary.
///
/// A return value, not a `setenv`: `pf_inject::default_backend` `getenv`s on
/// the injector thread, and a per-session write races `environ`. The operator
/// knob is still read there; this function does not write it.
///
/// Pin: `every_compositor_names_the_injector_backend_that_matches_it`;
/// `pf-inject`'s matching table is the other half.
pub fn input_backend_id(chosen: Compositor) -> &'static str {
    match chosen {
        Compositor::Gamescope => "gamescope",
        // org_kde_kwin_fake_input — no RemoteDesktop portal. Headless krdpserver
        // path; the shipped .desktop authorizes it.
        Compositor::Kwin => "kwin",
        // Neither fake_input nor wlr virtual-input → RemoteDesktop portal via libei.
        Compositor::Mutter => "libei",
        // Hyprland still speaks `zwlr_virtual_pointer_v1` + `zwp_virtual_keyboard_v1`
        // — same wlr injector as sway/river.
        // niri serves both too, on stock builds and biri alike.
        Compositor::Wlroots | Compositor::Hyprland | Compositor::Niri => "wlr",
        // Same protocols, but Umbriel maps absolute motion over the whole layout and ignores
        // the pointer's output: the injector aims in layout space.
        Compositor::Umbriel => "wlr-layout",
        // pf-inject ignores the id off Linux; Windows has one injector.
        Compositor::Windows => "windows",
    }
}

/// Resolve the gamescope route when `chosen` is gamescope.
///
/// Return value, not an env write: two sessions connecting at once would
/// otherwise clobber each other. Caller must put it on the backend via
/// `VirtualDisplay::set_gamescope_route`.
///
/// The operator-pinned (`PUNKTFUNK_COMPOSITOR`) path calls this alone — it
/// leaves input routing to the operator's knob, but `create` still needs a
/// route or it falls through to a bare spawn on a box pinned to managed.
/// A managed override without gamescope-session-plus on the box is dropped:
/// its launch cannot start, and it would outrank a dedicated spawn that can.
/// A seat's own session ([`pf_paths::seat::HostRole::runs_seat_session`]) always spawns,
/// whatever the box runs or the operator set.
#[cfg(target_os = "linux")]
#[must_use = "the resolved gamescope route must reach the backend instance (set_gamescope_route)"]
pub fn resolve_gamescope_route(
    chosen: Compositor,
    dedicated_launch: bool,
) -> Option<GamescopeRoute> {
    if chosen != Compositor::Gamescope {
        return None;
    }
    // A seat owns no box session to manage or attach to: its gamescope is always its own. The
    // owner's host at the desk routes as the box's does.
    if pf_paths::seat::role().runs_seat_session() {
        tracing::info!("gamescope sub-mode: spawn, as every seat does");
        return Some(GamescopeRoute::Spawn);
    }
    {
        // `operator_gamescope` takes ENV_LOCK itself; the mutex is not reentrant.
        // Nothing on this path writes the env.
        let ov = operator_gamescope();
        let installed = gamescope::managed_session_available();
        let target_restart = gamescope::steamos_session_present();
        // Only the target-restart route needs the `/proc` session walk.
        let desktop_active =
            target_restart && session::is_desktop_kind(crate::detect_active_session().kind);
        let steamos = pf_host_config::os_release::os_release().is("steamos");
        let managed_infra =
            managed_infra_usable(installed, target_restart, desktop_active, steamos);
        if installed && !managed_infra {
            tracing::info!(
                "gamescope: a desktop session is live and the SteamOS session takeover would end \
                 it — not taking over"
            );
        }
        // Managed needs session-plus. Forced without it, every launch dies in `launch_session`,
        // and the override sits above the dedicated spawn that would have worked.
        let managed_override = ov.managed || ov.session.is_some();
        if managed_override && !installed {
            tracing::warn!(
                "gamescope: PUNKTFUNK_GAMESCOPE_MANAGED / PUNKTFUNK_GAMESCOPE_SESSION ignored — \
                 gamescope-session-plus is not installed on this box"
            );
        }
        let mode = pick_gamescope_mode(
            dedicated_launch,
            ov.managed && installed,
            ov.attach,
            ov.node.is_some(),
            ov.session.is_some() && installed,
            managed_infra,
            gamescope::foreign_gamescope_running(),
        );
        tracing::info!(?mode, "gamescope sub-mode");
        // Operator knobs stay inputs. Publishing `_NODE=auto` here latches Attach
        // on the next sample (`node_env` outranks dedicated).
        Some(match mode {
            GamescopeMode::Attach => GamescopeRoute::Attach {
                node: ov.node.clone().unwrap_or_else(|| "auto".to_string()),
            },
            GamescopeMode::Managed => GamescopeRoute::Managed {
                client: ov.session.clone().unwrap_or_else(|| "steam".to_string()),
            },
            GamescopeMode::Spawn => GamescopeRoute::Spawn,
        })
    }
}

#[cfg(not(target_os = "linux"))]
pub fn resolve_gamescope_route(
    _chosen: Compositor,
    _dedicated_launch: bool,
) -> Option<GamescopeRoute> {
    None
}

/// Dedicated headless gamescope for this session.
///
/// A seat profile always gets one, launch or not: a second person never takes the box's own
/// session. Otherwise it needs a launch and dedicated policy for this `client` (its overlay
/// over the host's; `None` is the host policy). Either way gamescope must be available, else
/// it degrades to `auto`. Handshake value, threaded into [`resolve_gamescope_route`] /
/// [`resolve_compositor`]; no new env knob.
pub fn wants_dedicated_game_session(
    has_launch: bool,
    client: Option<[u8; 32]>,
    seat_profile: bool,
) -> bool {
    use policy::GameSession;
    let session = policy::prefs()
        .get()
        .game_session_for(policy::fp_hex(client).as_deref());
    if !seat_profile && (!has_launch || session != GameSession::Dedicated) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if gamescope::is_available() {
            true
        } else {
            tracing::warn!(
                seat_profile,
                "a dedicated game session needs gamescope, which is unavailable; using auto routing"
            );
            false
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        false // one desktop; no gamescope
    }
}

/// True when `vd.create` nests the launch (gamescope bare-spawn). The session
/// must not also spawn it — the game would start twice.
///
/// Takes this session's [`GamescopeRoute`], not process env, so a concurrent
/// session cannot change the answer.
#[cfg(target_os = "linux")]
pub fn launch_is_nested(compositor: Compositor, route: Option<&GamescopeRoute>) -> bool {
    compositor == Compositor::Gamescope && matches!(route, Some(GamescopeRoute::Spawn))
}

/// Launch `cmd` into a live managed/attach session, or into a kept bare spawn. A fresh spawn
/// nests instead ([`launch_is_nested`]).
#[cfg(target_os = "linux")]
pub fn launch_into_gamescope_session(cmd: &str, seat: Option<&str>) -> Result<std::process::Child> {
    gamescope::launch_into_session(cmd, seat)
}

/// Put compositor focus on streamed head `name` so a window mapping now lands
/// where the client is looking.
///
/// Only EXTEND backends act: the streamed head sits beside the operator's, and
/// a new window goes to whichever monitor holds focus. Other backends are
/// no-ops by construction — KWin/Mutter promote the virtual output primary;
/// gamescope nests the app; a mirror pin *is* a physical head, and stealing
/// its focus mid-session is unasked.
///
/// `name` is checked against the backend's mint first: a physical connector
/// is not ours to focus.
///
/// Returns whether focus was asserted (for logs). Best-effort: a miss costs
/// placement, never the session.
#[cfg(target_os = "linux")]
pub fn focus_streamed_output(compositor: Compositor, name: &str) -> bool {
    match compositor {
        Compositor::Hyprland if hyprland::is_managed_output(name) => {
            hyprland::focus_output(name);
            true
        }
        Compositor::Wlroots if wlroots::is_managed_output(name) => {
            wlroots::focus_output(name);
            true
        }
        // niri mints its outputs from Hyprland's counter.
        Compositor::Niri if hyprland::is_managed_output(name) => {
            niri::focus_output(name);
            true
        }
        Compositor::Umbriel if hyprland::is_managed_output(name) => {
            umbriel::focus_output(name);
            true
        }
        // No `_` arm: a new backend must decide here. Silent "no focus" only
        // shows up as a game on the wrong screen.
        Compositor::Hyprland
        | Compositor::Wlroots
        | Compositor::Kwin
        | Compositor::Mutter
        | Compositor::Gamescope
        | Compositor::Niri
        | Compositor::Umbriel
        | Compositor::Windows => false,
    }
}

/// A launch's own workspace on the streamed head.
///
/// Owned by the launch, not by the session: a keep-alive reconnect re-focuses
/// the same id instead of claiming a second one. The lease holds it and its
/// drop calls [`WorkspaceClaim::release`].
#[cfg(target_os = "linux")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceClaim {
    compositor: Compositor,
    /// The streamed head: Umbriel numbers workspaces per output.
    output: String,
    /// Workspace the launch opens on.
    id: i64,
    /// What the head showed before the claim. Equal to `id` when the head was
    /// already on an empty workspace, and then release has nothing to undo.
    restore: i64,
}

#[cfg(target_os = "linux")]
impl WorkspaceClaim {
    /// Id a later session re-focuses for the same launch ([`claim_workspace`]).
    pub fn id(&self) -> i64 {
        self.id
    }

    /// Put the head back where the operator left it. Idempotent: a repeat, or
    /// a head that is already gone, is a refused dispatch and one log line.
    /// An emptied workspace disappears on its own.
    pub fn release(&self) {
        if !self.needs_restore() {
            return;
        }
        let done = match self.compositor {
            Compositor::Hyprland => hyprland::focus_workspace(self.restore),
            Compositor::Wlroots => wlroots::focus_workspace(self.restore),
            Compositor::Niri => niri::desktop::focus_workspace(self.restore),
            Compositor::Umbriel => umbriel::desktop::focus_workspace(&self.output, self.restore),
            // No `_` arm: only a backend that claims can release.
            Compositor::Kwin | Compositor::Mutter | Compositor::Gamescope | Compositor::Windows => {
                return
            }
        };
        match done {
            Ok(()) => tracing::info!(
                workspace = self.restore,
                "the launch let its workspace go — the streamed head is back on the operator's"
            ),
            Err(e) => tracing::warn!(
                workspace = self.restore, error = %format!("{e:#}"),
                "workspace not restored after the launch — the streamed head stays on the one the \
                 game had (switch back by hand)"
            ),
        }
    }

    /// Anything to undo? A claim on the workspace the head already showed owns
    /// nothing, so release stays a no-op however often it runs.
    fn needs_restore(&self) -> bool {
        self.restore != self.id
    }
}

/// One workspace as both backends report it, reduced to what the pick needs.
#[cfg(target_os = "linux")]
pub(crate) struct WsSlot {
    pub id: i64,
    /// Lives on the streamed head.
    pub on_head: bool,
    /// Holds no windows. Unknown reads as occupied: a free id is always safe.
    pub empty: bool,
}

/// Workspace a launch gets on the streamed head: the head's own when that is
/// already empty (no switch at all), else another empty one there, else the
/// lowest free id — focusing an id nothing owns mints it empty on the focused
/// head. `active` is the head's current workspace.
#[cfg(target_os = "linux")]
pub(crate) fn pick_workspace(slots: &[WsSlot], active: i64) -> i64 {
    if slots.iter().any(|w| w.id == active && w.on_head && w.empty) {
        return active;
    }
    if let Some(w) = slots.iter().find(|w| w.on_head && w.empty) {
        return w.id;
    }
    // Special workspaces carry negative ids; a launch never wants one.
    (1..)
        .find(|n| !slots.iter().any(|w| w.id == *n))
        .unwrap_or(1)
}

/// Another head holds workspace `id`. Switching to it would switch that head,
/// so a claim moves it onto the streamed head first.
#[cfg(target_os = "linux")]
pub(crate) fn held_elsewhere(slots: &[WsSlot], id: i64) -> bool {
    slots.iter().any(|w| w.id == id && !w.on_head)
}

/// Give a launch its own empty workspace on streamed head `name`, or bring back
/// `want` — the workspace an earlier session already claimed for this launch.
///
/// Only the backends whose new windows map on the focused workspace act.
/// `name` is checked against the backend's mint first, as in
/// [`focus_streamed_output`]: a physical connector is not ours to switch.
/// Best-effort: `None` leaves the launch on whatever the head shows.
#[cfg(target_os = "linux")]
pub fn claim_workspace(
    compositor: Compositor,
    name: &str,
    want: Option<i64>,
) -> Option<WorkspaceClaim> {
    let placed = match compositor {
        Compositor::Hyprland if hyprland::is_managed_output(name) => {
            hyprland::claim_workspace(name, want)
        }
        Compositor::Wlroots if wlroots::is_managed_output(name) => {
            wlroots::claim_workspace(name, want)
        }
        Compositor::Niri if hyprland::is_managed_output(name) => {
            niri::desktop::claim_workspace(name, want)
        }
        Compositor::Umbriel if hyprland::is_managed_output(name) => {
            umbriel::desktop::claim_workspace(name, want)
        }
        // No `_` arm: a new backend must decide here. KWin is a later step (its
        // workspaces are a D-Bus script away); Mutter/GNOME has no per-output
        // workspace to aim at; gamescope already gives the game the whole
        // session; Windows has no workspaces at all.
        Compositor::Hyprland
        | Compositor::Wlroots
        | Compositor::Kwin
        | Compositor::Mutter
        | Compositor::Gamescope
        | Compositor::Niri
        | Compositor::Umbriel
        | Compositor::Windows => {
            tracing::info!(
                compositor = compositor.id(), output = %name,
                "this compositor does not place a launch on a workspace of its own — the game \
                 opens on whatever the streamed head is showing"
            );
            None
        }
    }?;
    let (id, restore) = placed;
    tracing::info!(
        workspace = id, restore, output = %name,
        "the launch has the streamed head's workspace to itself"
    );
    Some(WorkspaceClaim {
        compositor,
        output: name.to_string(),
        id,
        restore,
    })
}

/// Nested Xwayland `(DISPLAY, XAUTHORITY)` pairs for the XFixes cursor source.
/// Gamescope can run several; the pointer is on the focused one. Empty when
/// none are exposed — the host then leaves gamescope cursorless.
#[cfg(target_os = "linux")]
pub fn gamescope_xwayland_cursor_targets(seat: Option<&str>) -> Vec<(String, Option<String>)> {
    gamescope::xwayland_cursor_targets(seat)
}

/// This seat's gamescope is presenting Steam `appid`: its focused-app atom names it. Steam's own
/// launch screen is its client's appid, so this turns true only once the game is up.
#[cfg(target_os = "linux")]
pub fn gamescope_presenting(appid: u32, seat: Option<&str>) -> bool {
    gamescope::display_presenting(appid, seat).is_some()
}

/// Dedicated game gone: `node_id` does not reappear shortly after capture loss.
/// `true` ends the session cleanly instead of a capture-loss rebuild. Scoped
/// to this session's node so a coexisting gamescope cannot mask the exit.
/// Always `false` off Linux.
#[cfg(target_os = "linux")]
pub fn dedicated_game_exited(node_id: u32) -> bool {
    gamescope::game_session_exited(node_id)
}

#[cfg(not(target_os = "linux"))]
pub fn dedicated_game_exited(_node_id: u32) -> bool {
    false
}

/// Does this launch go through Steam? Steam owns what it starts, so the game lands under
/// whichever gamescope that Steam runs in, reuse included.
#[cfg(target_os = "linux")]
pub fn launch_is_steam(cmd: &str) -> bool {
    gamescope::is_steam_launch(cmd)
}

/// Steam appid a dedicated launch targets, for the exit watcher. `None` for a
/// non-Steam launch — those use [`dedicated_game_exited`]: gamescope's nested
/// child *is* the game.
#[cfg(target_os = "linux")]
pub fn steam_appid_from_launch(cmd: &str) -> Option<u32> {
    gamescope::steam_appid_from_launch(cmd)
}

/// Block until Steam `appid` has started and then exited.
///
/// `true` → end the session cleanly. `false` if `cancel` is set or the game
/// never started within the startup grace (leave the session up). Runs on
/// the per-session watch thread; `cancel` is the session stop flag.
#[cfg(target_os = "linux")]
pub fn watch_steam_game_exit(
    appid: u32,
    seat: Option<&str>,
    cancel: &std::sync::atomic::AtomicBool,
) -> bool {
    matches!(
        gamescope::wait_for_steam_game_exit(appid, seat, cancel),
        gamescope::SteamGameWatch::Exited
    )
}

/// Cancel a pending TV-session restore: a client (re)connected. No-op off Linux.
#[cfg(target_os = "linux")]
pub fn cancel_pending_tv_restore() {
    gamescope::cancel_pending_restore();
}

#[cfg(not(target_os = "linux"))]
pub fn cancel_pending_tv_restore() {}

/// Whether managed gamescope can stand a session up from nothing (SteamOS
/// `gamescope-session` or Bazzite `gamescope-session-plus`). Lets connect
/// route a box with no live graphical session to takeover instead of failing.
/// Always `false` off Linux.
#[cfg(target_os = "linux")]
pub fn managed_session_available() -> bool {
    gamescope::managed_session_available()
}

#[cfg(not(target_os = "linux"))]
pub fn managed_session_available() -> bool {
    false
}

/// Schedule a debounced TV restore after a managed takeover session ends.
/// A reconnect inside the window reuses the warm session (no per-connect
/// gamescope stop/relaunch). No-op when nothing was taken. Needs
/// [`start_restore_worker`] running to fire. A box already on its desktop gets
/// back what our session held at once, whatever the keep-alive. `quit` says the
/// last session was stopped on purpose, `slept` how long the box slept under it.
#[cfg(target_os = "linux")]
pub fn restore_managed_session(quit: bool, slept: std::time::Duration) {
    gamescope::release_autologin_mask(crate::detect_active_session().kind);
    gamescope::schedule_restore_tv_session(quit, slept);
}

#[cfg(not(target_os = "linux"))]
pub fn restore_managed_session(_quit: bool, _slept: std::time::Duration) {}

/// The takeover the host still holds, for the console's display list. `None` = nothing held.
#[cfg(target_os = "linux")]
pub fn held_managed_session() -> Option<gamescope::HeldTakeover> {
    gamescope::held_takeover()
}

/// Console Release of a held takeover: the box's own session comes back now. Call only with no
/// session live. `false` = nothing held.
#[cfg(target_os = "linux")]
pub fn release_managed_session() -> bool {
    gamescope::release_takeover()
}

/// Host-lifetime worker for debounced [`restore_managed_session`]. Hold the
/// returned handle for the host's life; dropping it stops the worker. Call
/// once from `serve()`.
#[cfg(target_os = "linux")]
pub fn start_restore_worker() -> std::sync::Arc<()> {
    gamescope::start_restore_worker()
}

#[cfg(not(target_os = "linux"))]
pub fn start_restore_worker() -> std::sync::Arc<()> {
    std::sync::Arc::new(())
}

/// Recover a stranded TV takeover from a crashed previous host. Call once at
/// `serve` startup, after [`reclaim_topology_journal`] adopts its steps and beside
/// [`start_restore_worker`]. No-op when nothing was journaled.
#[cfg(target_os = "linux")]
pub fn restore_takeover_on_startup() {
    gamescope::restore_takeover_on_startup();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_takeover_on_startup() {}

/// Take back what a dead host left: the desk changes it journaled ([`crate::topology_journal`])
/// and the xdph picker shim its config still names. Call at `serve` startup before
/// [`restore_takeover_on_startup`], and when a compositor instance appears. With nothing left,
/// a directory listing and one config read.
#[cfg(target_os = "linux")]
pub fn reclaim_topology_journal() {
    crate::topology_journal::reclaim(&crate::backend_by_id);
    hyprland::reconcile_picker_shim();
}

#[cfg(not(target_os = "linux"))]
pub fn reclaim_topology_journal() {}

/// Warn once at startup when this box will need managed takeover but the user
/// is not in the `punktfunk` group the helper gates on — the one prerequisite
/// that fails silently mid-stream. Gated so a box that never attempts takeover
/// stays quiet. Call once at `serve` startup.
#[cfg(target_os = "linux")]
pub fn preflight_takeover_privilege() {
    gamescope::preflight_takeover_privilege();
}

#[cfg(not(target_os = "linux"))]
pub fn preflight_takeover_privilege() {}

/// Why the `punktfunk`-group prerequisite does not apply. Each variant alone
/// makes the group irrelevant — do not nag — but keep `why` so a diagnostics
/// UI can name which gate, not hide the row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakeoverInapplicable {
    /// Root: plain system-bus `systemctl` succeeds, so the helper is never reached.
    Root,
    /// No DM drives logins (getty autologin / an enabled user unit).
    NoDisplayManager,
    /// No session-plus/SteamOS infra ⇒ no autologin gaming session to free.
    NoManagedSession,
    /// Tarball/source/Nix: no packaged helper or group; the docs' polkit rule is the route.
    NoPackagedHelper,
    /// Login name unresolved, so no usable `usermod` line.
    UnknownUser,
    /// Managed takeover is a Linux path.
    NotLinux,
}

/// The takeover's one un-automatable prerequisite, as data.
///
/// Defined on every platform (the host maps it to a wire check). Off Linux
/// it is always `Inapplicable { why: NotLinux }`. Membership is the user
/// database's answer, matching `pf-dm-helper`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TakeoverVerdict {
    Inapplicable {
        why: TakeoverInapplicable,
    },
    Ok {
        user: String,
        group: &'static str,
    },
    /// Takeover applies and the user is **not** a member — every takeover
    /// degrades silently to mirroring the box's own session.
    MissingMembership {
        user: String,
        dm: String,
        helper: &'static str,
        group: &'static str,
    },
}

/// Gated verdict behind [`preflight_takeover_privilege`], for a diagnostics
/// UI. Computing it does not log.
#[cfg(target_os = "linux")]
pub fn takeover_privilege_verdict() -> TakeoverVerdict {
    gamescope::takeover_privilege_verdict()
}

#[cfg(not(target_os = "linux"))]
pub fn takeover_privilege_verdict() -> TakeoverVerdict {
    TakeoverVerdict::Inapplicable {
        why: TakeoverInapplicable::NotLinux,
    }
}

/// Restore the box's own session and the portal pickers now — host is exiting.
/// Blocks on `systemctl`, so call off the async runtime. A takeover that
/// outlives the host leaves the box with no display manager and nobody to
/// restart it. No-op when nothing was taken.
#[cfg(target_os = "linux")]
pub fn restore_takeover_now() {
    gamescope::restore_takeover_now();
    // Not per cast: the rewrite restarts the portal and orphans its cached D-Bus,
    // a stream that never delivers a buffer.
    // Shutdown has no live cast. Each is a no-op if we never took it.
    hyprland::restore_picker_on_shutdown();
    wlroots::restore_chooser_on_shutdown();
}

#[cfg(not(target_os = "linux"))]
pub fn restore_takeover_now() {}

/// Mid-stream switch: drop the autologin-unit mask if it no longer applies.
///
/// The box left our managed session for its desktop: hand back what the
/// takeover holds of it, our Steam included, so "Return to Gaming Mode" gets
/// the box's own. Call on every confirmed switch, and from a connect or a
/// stream end that finds a desktop. A no-op for any other session kind.
#[cfg(target_os = "linux")]
pub fn release_autologin_mask(switched_to: crate::ActiveKind) {
    gamescope::release_autologin_mask(switched_to);
}

#[cfg(not(target_os = "linux"))]
pub fn release_autologin_mask(_switched_to: crate::ActiveKind) {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn gamescope_mode_ladder() {
        use GamescopeMode::*;
        let pick = pick_gamescope_mode;
        // (dedicated_launch, force_managed, attach_env, node_env, session_env, managed_infra, foreign_gamescope)
        // Plain distro, nothing running: bare spawn — the path that nests the launch command.
        assert_eq!(pick(false, false, false, false, false, false, false), Spawn);
        // Session infra present: managed.
        assert_eq!(
            pick(false, false, false, false, false, true, false),
            Managed
        );
        assert_eq!(pick(false, false, false, false, false, true, true), Managed);
        // Foreign gamescope on an infra-less box: attach and mirror it.
        assert_eq!(pick(false, false, false, false, false, false, true), Attach);
        // Operator-set PUNKTFUNK_GAMESCOPE_SESSION keeps managed even without detected infra.
        assert_eq!(
            pick(false, false, false, false, true, false, false),
            Managed
        );
        // Explicit attach/node wins over infra…
        assert_eq!(pick(false, false, true, false, false, true, false), Attach);
        assert_eq!(pick(false, false, false, true, true, true, false), Attach);
        // …and force-managed wins over everything.
        assert_eq!(pick(false, true, true, true, false, false, false), Managed);
        // Dedicated launch forces Spawn, outranking managed-infra + foreign-attach…
        assert_eq!(pick(true, false, false, false, false, true, true), Spawn);
        // …but the explicit operator overrides still win over dedicated.
        assert_eq!(pick(true, true, false, false, false, true, false), Managed);
        assert_eq!(pick(true, false, true, false, false, false, false), Attach);
        assert_eq!(pick(true, false, false, true, false, false, false), Attach);
    }

    #[test]
    fn a_live_desktop_bars_the_steamos_takeover_off_steamos() {
        // (installed, target_restart, desktop_active, steamos)
        let usable = managed_infra_usable;
        // Valve's session on a Plasma box: restarting its target would end Plasma.
        assert!(!usable(true, true, true, false));
        let infra = usable(true, true, true, false);
        assert_eq!(
            pick_gamescope_mode(false, false, false, false, false, infra, false),
            GamescopeMode::Spawn
        );
        // Real SteamOS, no desktop, and session-plus's own unit stay managed.
        assert!(usable(true, true, true, true));
        assert!(usable(true, true, false, false));
        assert!(usable(true, false, true, false));
        assert!(!usable(false, false, true, false));
    }

    /// Injector id is a return value, never a `PUNKTFUNK_INPUT_BACKEND` `set_var`
    /// (that raced `pf_inject::default_backend`'s `getenv` on the injector
    /// thread). Whole table: this crate cannot depend on pf-inject; the five
    /// ids are one half of the contract (`every_id_the_video_side_emits_maps_to_a_backend`
    /// is the other).
    #[test]
    fn every_compositor_names_the_injector_backend_that_matches_it() {
        assert_eq!(input_backend_id(Compositor::Gamescope), "gamescope");
        assert_eq!(input_backend_id(Compositor::Kwin), "kwin");
        assert_eq!(input_backend_id(Compositor::Mutter), "libei");
        // Hyprland shares sway's wlr virtual-input protocols — same injector on purpose.
        assert_eq!(input_backend_id(Compositor::Wlroots), "wlr");
        assert_eq!(input_backend_id(Compositor::Hyprland), "wlr");
        assert_eq!(input_backend_id(Compositor::Niri), "wlr");
        assert_eq!(input_backend_id(Compositor::Umbriel), "wlr-layout");
        assert_eq!(input_backend_id(Compositor::Windows), "windows");
    }

    #[cfg(target_os = "linux")]
    fn slot(id: i64, on_head: bool, empty: bool) -> WsSlot {
        WsSlot { id, on_head, empty }
    }

    /// The pick in all four shapes. A launch must never land on a workspace
    /// that holds windows, and never on one belonging to another head.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_launch_takes_an_empty_workspace_on_its_own_head_or_a_free_id() {
        // The head already shows an empty one: stay, so nothing switches.
        let slots = [slot(1, false, false), slot(2, true, true)];
        assert_eq!(pick_workspace(&slots, 2), 2);
        // Occupied active, another empty one on the head.
        let slots = [slot(1, true, false), slot(2, true, true)];
        assert_eq!(pick_workspace(&slots, 1), 2);
        // Every workspace on the head is occupied: lowest free id, minted empty.
        let slots = [
            slot(1, true, false),
            slot(2, false, true),
            slot(3, true, false),
        ];
        assert_eq!(pick_workspace(&slots, 1), 4);
        // An empty workspace on somebody else's head is not ours to take.
        let slots = [slot(1, true, false), slot(2, false, true)];
        assert_eq!(pick_workspace(&slots, 1), 3);
    }

    /// A reconnect's head is new, so the game's workspace sits on another one:
    /// it is moved, not switched to. One on the head, or one nobody has, is switched to.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_workspace_on_another_head_is_brought_over() {
        let slots = [slot(2, false, false), slot(3, true, true)];
        assert!(held_elsewhere(&slots, 2));
        assert!(!held_elsewhere(&slots, 3));
        assert!(!held_elsewhere(&slots, 9));
    }

    /// Special workspaces sit at negative ids; the free-id scan starts at 1.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_free_id_scan_skips_hyprland_special_workspaces() {
        let slots = [slot(-99, true, false), slot(1, true, false)];
        assert_eq!(pick_workspace(&slots, 1), 2);
    }

    /// A claim on the workspace the head already showed owns nothing, so
    /// release stays a no-op however many times a crashed launch runs it.
    #[test]
    #[cfg(target_os = "linux")]
    fn releasing_a_claim_that_switched_nothing_restores_nothing() {
        let stay = WorkspaceClaim {
            compositor: Compositor::Hyprland,
            output: "PF-1".into(),
            id: 2,
            restore: 2,
        };
        assert!(!stay.needs_restore());
        stay.release();
        stay.release();
        let switched = WorkspaceClaim {
            compositor: Compositor::Hyprland,
            output: "PF-1".into(),
            id: 4,
            restore: 1,
        };
        assert!(switched.needs_restore());
    }

    /// A physical connector is not ours to focus or to switch, whatever the compositor.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_physical_connector_is_never_focused_or_claimed_on_niri() {
        assert!(!focus_streamed_output(Compositor::Niri, "DP-1"));
        assert!(claim_workspace(Compositor::Niri, "DP-1", None).is_none());
        assert!(!focus_streamed_output(Compositor::Umbriel, "DP-1"));
        assert!(claim_workspace(Compositor::Umbriel, "DP-1", None).is_none());
    }

    /// The console's two gamescope switches are "applies to the next session" rows: a
    /// toggle must reach the next connect, not the next host restart.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_console_gamescope_switches_apply_to_the_next_session() {
        crate::fake::in_child(
            module_path!(),
            "the_console_gamescope_switches_apply_to_the_next_session",
            &[],
            || {
                use super::operator_gamescope;
                assert!(!operator_gamescope().managed);
                let mut patch = serde_json::Map::new();
                patch.insert("gamescope_managed".into(), serde_json::Value::Bool(true));
                pf_host_config::save(&patch).expect("save the console switch");
                assert!(
                    operator_gamescope().managed,
                    "the next read sees the switch"
                );
                patch.insert("gamescope_managed".into(), serde_json::Value::Bool(false));
                pf_host_config::save(&patch).expect("clear it");
                assert!(!operator_gamescope().managed);
            },
        );
    }

    /// Sample must not move when `PUNKTFUNK_GAMESCOPE_NODE` reads differently afterwards.
    /// `node_env` outranks `dedicated_launch`; a live re-read of a published
    /// `=auto` would latch Attach for the host's life.
    #[test]
    #[cfg(target_os = "linux")]
    fn operator_overrides_do_not_see_our_own_writes() {
        use super::{operator_gamescope, operator_gamescope_from};
        let first = operator_gamescope();
        let published = |k: &str| (k == "PUNKTFUNK_GAMESCOPE_NODE").then(|| "auto".to_string());
        let second = operator_gamescope_from(published);
        assert_eq!(
            second.node, first.node,
            "writing the key we publish must not turn into an operator override"
        );
    }
}
