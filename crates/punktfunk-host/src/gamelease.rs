//! Lease on a launched game: when it started, when it exits, how to end it
//! (`design/session-game-lifetime.md`).
//!
//! Defined by how to recognize the game ([`crate::library::DetectSpec`]), not by a
//! pid. Store URIs hand off to a launcher that exits; a held [`LeaseKind::Child`]
//! falls back to recognition the moment that child is a shim. A process the host
//! did start is a second signal ([`LeaseRequest::spawned`] on Windows, a `Child`
//! on Linux).
//!
//! Ending a game costs unsaved progress. Three rules:
//!
//! 1. **Opt-in.** [`GameOnSessionEnd::Keep`] is the default
//!    ([`crate::session_settings`]).
//! 2. **Never a surprise on a network blip.** `Always` waits a reconnect grace
//!    window; a reconnecting client re-adopts.
//! 3. **Only this session's game.** Adopt only pids that started after this
//!    launch, and re-verify start time before signalling ([`crate::procscan`]).

use crate::library::DetectSpec;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 300 s cold-start budget. A Steam boot plus first-run shader compile can take
/// that long. A miss leaves the session streaming the launcher, which is valid.
const START_GRACE: Duration = Duration::from_secs(300);
/// 1 s poll. Imperceptible against startup/shutdown; a `/proc` sweep stays a
/// fraction of one core.
const POLL: Duration = Duration::from_secs(1);
/// 3 s unseen before exit. A launcher re-exec or mid-game restart is shorter.
const EXIT_CONFIRM: Duration = Duration::from_secs(3);
/// 5 s successful child exit = launcher hand-off, never the game. Matches
/// Apollo's `auto_detach`.
const SHIM_WINDOW: Duration = Duration::from_secs(5);
/// 10 s after SIGTERM / WM_CLOSE before SIGKILL. Enough to save.
const TERM_GRACE: Duration = Duration::from_secs(10);
/// Cap on [`crate::procscan::running_hint`] holding off an exit after every
/// process is gone.
///
/// The hint covers a re-exec gap of seconds. Past 30 s the game is gone
/// whatever the hint says. Steam's Windows `Running` flag can stay set after
/// an unclean exit; an unbounded veto never ends the session. Ending a moment
/// early (reconnect; `finish` does not kill) is cheaper than ending never.
const VETO_LIMIT: Duration = Duration::from_secs(30);
/// 60 s between play-time flushes ([`crate::library::record_run_time`]). A
/// host crash loses at most this much of the current run.
const STATS_FLUSH: Duration = Duration::from_secs(60);

/// Spawned child, and whether a group signal is safe for it.
#[derive(Clone, Copy, Debug)]
pub struct OwnedChild {
    pub pid: u32,
    /// Group leader? `kill(-pid)` hits the group whose id is `pid`. A
    /// non-leader shares the host's group — signal that pid only.
    pub group_leader: bool,
}

/// Display identity for a launched title.
#[derive(Clone, Debug, Default)]
pub struct GameRef {
    /// Store-qualified id (`steam:570`). `None` = typed `apps.json` command.
    pub id: Option<String>,
    pub store: Option<String>,
    /// Display title; never empty in the UI (falls back to id, then command).
    pub title: String,
}

#[derive(Clone, Debug)]
pub enum LeaseKind {
    /// Bare-spawn gamescope: node death is the exit; releasing the display
    /// ends the game. Detection and termination stay with the display layer.
    Nested,
    /// Host-spawned child. Process group is the game until it proves a shim,
    /// then [`LeaseKind::Matched`].
    Child,
    /// Recognized by [`DetectSpec`]; a launcher owns the process.
    Matched,
    /// Launcher reports start/stop ([`crate::runstate`]); the host has no
    /// process of its own. Covers a title with nothing to scan (`playnite://`).
    Reported,
    /// No detect signals, no owned child, no provider. Both lifetime
    /// behaviours stay inert; the host logs once rather than guessing.
    Untracked,
}

impl LeaseKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Nested => "nested",
            Self::Child => "child",
            Self::Matched => "matched",
            Self::Reported => "reported",
            Self::Untracked => "untracked",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum GameState {
    Launching = 0,
    Running = 1,
    Exited = 2,
    /// [`LeaseKind::Untracked`]: never observes start or stop. Terminal.
    /// Distinct from [`Running`](Self::Running) — that would claim liveness
    /// the host cannot back up, and `session_on_game_exit` can never fire.
    Untracked = 3,
    /// The game's own window is on screen. Follows
    /// [`Running`](Self::Running), often far later: the process is up while
    /// Proton builds a prefix or a splash sits on a black window.
    Window = 4,
}

impl GameState {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Running,
            2 => Self::Exited,
            3 => Self::Untracked,
            4 => Self::Window,
            _ => Self::Launching,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Launching => "launching",
            Self::Running => "running",
            Self::Exited => "exited",
            Self::Untracked => "untracked",
            Self::Window => "window",
        }
    }
}

/// Identity, live state, and enough to end the game. Outlives the session:
/// held in an `Arc` by the watcher and, after a disconnect, the grace registry.
pub struct LeaseShared {
    pub game: GameRef,
    pub client: String,
    /// Stable id of the device that launched it, for the game events' hook filter.
    pub fingerprint: Option<String>,
    /// The launching session's preset, on the game events.
    pub preset: Option<crate::events::PresetRef>,
    pub plane: crate::events::Plane,
    kind: LeaseKind,
    state: AtomicU8,
    /// Watcher stop: session ended, or the lease was terminated.
    cancel: Arc<AtomicBool>,
    /// Recognition. Empty for [`LeaseKind::Nested`] / [`LeaseKind::Untracked`].
    spec: DetectSpec,
    /// Process tree recognition may look in ([`LeaseRequest::scope_pid`]).
    scope_pid: Option<u32>,
    /// Seconds-since-boot at launch: adopt floor. `None` = no uptime clock,
    /// so only detect signals are used.
    launch_stamp: Option<f64>,
    /// Host-spawned child ([`LeaseKind::Child`]). Cleared once reaped.
    child: Mutex<Option<OwnedChild>>,
    /// Spawned pid with no `Child` handle ([`LeaseRequest::spawned`]), pinned
    /// to start time. Never cleared: every read re-verifies via
    /// [`crate::procscan::Scanner::alive`], so a recycled pid reads as gone.
    spawned: Option<crate::procscan::ProcRef>,
    /// This launch's registry slot ([`crate::launchreg::LiveProcs`]): [`terminate`]
    /// marks the record ending, then drops it. `None` if unrecorded.
    procs: Option<crate::launchreg::LiveProcs>,
    /// Asked to end: a second request is a no-op, and the watcher's exit is
    /// not the player quitting.
    terminating: AtomicBool,
    /// Unix ms at create, for the status surface.
    pub created_ms: u64,
    /// Seen running at least once. Distinguishes "exited" from "never started".
    was_running: AtomicBool,
    /// Last seen running, unix ms. 0 = never.
    last_seen_ms: AtomicU64,
    /// Client to tell when this launch dies on the spot ([`LeaseRequest::outcome`]).
    outcome: Option<OutcomeTx>,
    /// The window stage is watching a compositor that lists windows, and has
    /// not found the game's yet. A launch hold waits on it past `running`.
    awaiting_window: AtomicBool,
}

impl LeaseShared {
    pub fn state(&self) -> GameState {
        GameState::from_u8(self.state.load(Ordering::Relaxed))
    }

    pub fn kind(&self) -> &LeaseKind {
        &self.kind
    }

    pub fn is_terminating(&self) -> bool {
        self.terminating.load(Ordering::Relaxed)
    }

    pub fn is_trackable(&self) -> bool {
        !matches!(self.kind, LeaseKind::Untracked)
    }

    /// Everything this lease's signals match, inside its scope if it has one.
    #[cfg(any(target_os = "linux", windows))]
    fn find_procs(&self, scanner: &crate::procscan::Scanner) -> Vec<crate::procscan::ProcRef> {
        let live = scanner.find(&self.spec, self.launch_stamp);
        match self.scope_pid {
            Some(root) => crate::procscan::under(&live, root),
            None => live,
        }
    }

    /// Stop every client holding this title's cover: report `running` now, and wait for no
    /// window.
    ///
    /// For a launch whose stream already shows what the player has to act on — a seat's Steam
    /// at its sign-in screen. Only out of `launching`: the watcher owns every state after it,
    /// so a game the player then starts is followed as any other.
    pub fn launch_hold_ends(&self) {
        let _ = self.state.compare_exchange(
            GameState::Launching as u8,
            GameState::Running as u8,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        self.awaiting_window.store(false, Ordering::Relaxed);
    }

    /// Running, and this host will say `window` once the game's window is up.
    pub fn awaits_window(&self) -> bool {
        self.awaiting_window.load(Ordering::Relaxed) && self.state() == GameState::Running
    }

    fn set_state(&self, s: GameState) {
        self.state.store(s as u8, Ordering::Relaxed);
    }

    fn owned_child(&self) -> Option<OwnedChild> {
        *self.child.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// After reap: never signal this pid; the kernel may reuse the number.
    fn forget_child(&self) {
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Unix ms; same clock as the event bus and status API.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Session handle. Drop stops the watcher; the *game* is the caller's
/// decision via [`crate::session_settings`] at teardown ([`on_session_end`]).
pub struct GameLease {
    shared: Arc<LeaseShared>,
    /// Dropped, not joined: the watcher exits on `cancel` within `POLL`.
    watcher: Option<std::thread::JoinHandle<()>>,
    /// Workspace this launch owns on the streamed head
    /// ([`crate::vdisplay::WorkspaceClaim`]). Released in `Drop`, so a game
    /// that crashed leaves the operator's workspace back where it was.
    #[cfg(target_os = "linux")]
    workspace: Option<crate::vdisplay::WorkspaceClaim>,
}

impl GameLease {
    pub fn shared(&self) -> Arc<LeaseShared> {
        self.shared.clone()
    }
}

/// Ends the watch and gives the workspace back. Runs on every exit, panic
/// included: the session guard owns this handle.
impl Drop for GameLease {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::SeqCst);
        // Drop the JoinHandle; do not join. The watcher exits within `POLL`.
        drop(self.watcher.take());
        #[cfg(target_os = "linux")]
        if let Some(ws) = self.workspace.take() {
            ws.release();
        }
    }
}

/// The process tree a lease may narrow its scan to ([`LeaseRequest::scope_pid`]).
///
/// Only where the game is guaranteed to descend from `gamescope`: the launch is that
/// compositor's own primary child, or it goes through the Steam running inside it. Anything else
/// a keep-alive reuse starts is spawned by the host, beside gamescope rather than under it, and a
/// scoped scan would never find it.
pub fn scan_scope(nested_spawn: bool, steam_launch: bool, gamescope: Option<u32>) -> Option<u32> {
    (nested_spawn || steam_launch)
        .then_some(gamescope)
        .flatten()
}

/// Inputs for [`open`]. Only the launch site has all of them.
pub struct LeaseRequest {
    pub game: GameRef,
    pub client: String,
    /// Stable id of the device that launched it. `None` for an anonymous client.
    pub fingerprint: Option<String>,
    /// The launching session's preset; `None` for plain settings and on GameStream.
    pub preset: Option<crate::events::PresetRef>,
    pub plane: crate::events::Plane,
    pub spec: DetectSpec,
    /// `true` when a bare-spawn gamescope owns the game.
    pub nested: bool,
    /// That gamescope's pid, so recognition stays inside this seat's process tree
    /// ([`crate::procscan::under`]). `None` scans the whole uid, as every other lease does.
    /// [`scan_scope`] owns when it may be set.
    pub scope_pid: Option<u32>,
    /// Opens a launcher, not a game: always [`LeaseKind::Untracked`].
    ///
    /// A launcher has no exit to detect (Big Picture is a mode of Steam, not
    /// a process). Without the flag, "Steam was already running" vs not
    /// would pick `Child` vs shim-untracked for the same tile.
    pub launcher: bool,
    /// Host-spawned child and whether it leads its process group
    /// ([`OwnedChild::group_leader`]).
    pub child: Option<(std::process::Child, bool)>,
    /// Pid with no `Child` handle (Windows `CreateProcessAsUserW`). Same
    /// role as [`Self::child`]: without it an empty [`DetectSpec`] is
    /// [`LeaseKind::Untracked`]. Pinned to start time at [`open`]
    /// ([`crate::procscan::Scanner::resolve`]).
    pub spawned: Option<u32>,
    /// Seconds-since-boot from **before** spawn ([`launch_clock`]): adopt
    /// floor, so a copy already open is not this session. `None` disables
    /// the filter. A reconnect inherits it from [`crate::launchreg`].
    pub launch_stamp: Option<f64>,
    /// Adopted pids, published so the launch record outlives this watcher
    /// ([`crate::launchreg::LiveProcs`]). `None` if unrecorded.
    pub procs: Option<crate::launchreg::LiveProcs>,
    /// Workspace the streamed head gave this launch, if any
    /// ([`crate::library::adopt_launch_workspace`]). Released with the lease.
    #[cfg(target_os = "linux")]
    pub workspace: Option<crate::vdisplay::WorkspaceClaim>,
    /// Where to look for this game's window. `None` where there is no source: the
    /// lease never reports `window`, and a launch hold ends at `running`.
    pub window: Option<WindowSource>,
    /// Where to say this launch died on the spot. `None` leaves the finding in
    /// the log, as it was before the client could be told.
    pub outcome: Option<OutcomeTx>,
}

/// Where the window stage looks, and what it does with the first window it
/// recognizes as the game's.
#[cfg(target_os = "linux")]
#[derive(Clone, Debug)]
pub struct WindowStage {
    pub head: crate::session_status::StreamedHead,
    pub on_window: crate::library::OnWindow,
}

/// Where the watcher looks for the game's window.
pub enum WindowSource {
    /// The compositor's own window list: Hyprland and sway over IPC, KWin over
    /// `org_kde_plasma_window_management`, GNOME through punktfunk's shell extension. `stage`
    /// places the first window, where a head is known and the compositor allows it.
    #[cfg(target_os = "linux")]
    Toplevels {
        compositor: crate::vdisplay::Compositor,
        stage: Option<WindowStage>,
    },
    /// This seat's gamescope presenting Steam `appid`, per its focused-app control atom.
    #[cfg(target_os = "linux")]
    Gamescope { seat: Option<String>, appid: u32 },
    /// The interactive desktop's top-level windows.
    #[cfg(windows)]
    Desktop,
}

/// Seconds since boot for adopt-against. Call **before** spawn
/// ([`LeaseRequest::launch_stamp`]).
pub fn launch_clock() -> Option<f64> {
    // Always delegate: `procscan::launch_stamp` is already `None` with no
    // matcher. A Linux-only gate skipped the Windows floor, and `find(spec,
    // None)` then adopted a copy the player already had open.
    crate::procscan::launch_stamp()
}

pub type OnExit = Box<dyn Fn() + Send + Sync>;

/// Where a launch outcome goes: the session's control task writes it to the
/// client. `None` for a lease with nobody streaming to tell.
pub type OutcomeTx = tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::LaunchOutcome>;

/// Did this launch die on the spot, or is it a hand-off to a game still coming up?
///
/// Only a *failing* exit inside [`SHIM_WINDOW`] with nothing left to recognize.
/// A launcher hands off cleanly, and a Proton prefix build never reaches here at
/// all: its shell exits with success and the scan waits out [`START_GRACE`].
fn died_on_the_spot(quick: bool, success: bool, fallback: &LeaseKind) -> bool {
    quick && !success && matches!(fallback, LeaseKind::Untracked)
}

/// What the player is told when a launch exits on the spot.
///
/// 127 and 126 are the shell's own "no such command" / "cannot execute"; named,
/// because a number is not a next move. Anything else gets how long it lasted.
fn early_exit_message(title: &str, code: Option<i32>, secs: f32) -> String {
    let cause = match code {
        Some(127) => "this host doesn't have that command".to_string(),
        Some(126) => "this host can't run that command".to_string(),
        _ => format!("it closed again after {secs:.1} s"),
    };
    format!("Couldn't start {title} — {cause}.")
}

/// Open a lease and start watching. `on_exit` fires **at most once**: game
/// was seen running, then confirmed gone. Never for never-started, a shim,
/// or a host-requested end.
pub fn open(req: LeaseRequest, on_exit: OnExit) -> GameLease {
    let LeaseRequest {
        game,
        client,
        fingerprint,
        preset,
        plane,
        spec,
        nested,
        scope_pid,
        launcher,
        child,
        spawned,
        launch_stamp,
        procs,
        #[cfg(target_os = "linux")]
        workspace,
        window,
        outcome,
    } = req;

    // Pin pid to start time before recycle. Unresolvable is dropped: a bare
    // number never proceeds (procscan rule 2).
    let spawned = spawned.and_then(crate::procscan::resolve);

    // Launcher is Untracked before `child` is considered: a just-started
    // launcher leaves a live child that must not become a `Child` lease.
    let kind = if launcher {
        LeaseKind::Untracked
    } else if nested {
        LeaseKind::Nested
    } else if child.is_some() || spawned.is_some() {
        // Spawned pid is a Child for lifetime rules, shim reclass included.
        LeaseKind::Child
    } else if !spec.is_empty() {
        LeaseKind::Matched
    } else if crate::runstate::speaks_for(game.id.as_deref()) {
        // Provider reports liveness: tracked. Kind is asked once; flipping
        // mid-flight would couple both lifetime behaviours to plugin uptime.
        LeaseKind::Reported
    } else {
        LeaseKind::Untracked
    };

    let owned = child.as_ref().map(|(c, leader)| OwnedChild {
        pid: c.id(),
        group_leader: *leader,
    });
    let child = child.map(|(c, _)| c);
    let shared = Arc::new(LeaseShared {
        game,
        client,
        fingerprint,
        preset,
        plane,
        kind: kind.clone(),
        state: AtomicU8::new(GameState::Launching as u8),
        cancel: Arc::new(AtomicBool::new(false)),
        spec,
        scope_pid,
        launch_stamp,
        child: Mutex::new(owned),
        spawned,
        procs: procs.clone(),
        terminating: AtomicBool::new(false),
        created_ms: now_ms(),
        was_running: AtomicBool::new(false),
        last_seen_ms: AtomicU64::new(0),
        outcome,
        awaiting_window: AtomicBool::new(false),
    });

    if launcher {
        tracing::info!(
            title = %shared.game.title,
            app = shared.game.id.as_deref().unwrap_or("-"),
            "this entry opens a launcher, not a game — the session stays up until the client \
             leaves, and closing the launcher does not end it"
        );
    } else if matches!(kind, LeaseKind::Untracked) {
        tracing::info!(
            title = %shared.game.title,
            app = shared.game.id.as_deref().unwrap_or("-"),
            "this title exposes nothing the host can use to recognize its process — \
             game-exit detection and end-game-on-disconnect stay off for it"
        );
    } else {
        tracing::info!(
            title = %shared.game.title,
            app = shared.game.id.as_deref().unwrap_or("-"),
            kind = kind.as_str(),
            "watching the launched game"
        );
    }

    // Set before the watcher runs, so a window it finds on its first tick stays found.
    shared
        .awaiting_window
        .store(window.is_some(), Ordering::Relaxed);
    let watcher = spawn_watcher(shared.clone(), child, procs, on_exit, window);
    if watcher.is_none() {
        shared.awaiting_window.store(false, Ordering::Relaxed);
        // No watcher: Nested still reports Running (gamescope node-death
        // watches it). Everything else, including spawn failure and no
        // matcher, is Untracked — keyed on Nested so those land honest too.
        shared.set_state(if matches!(shared.kind, LeaseKind::Nested) {
            GameState::Running
        } else {
            GameState::Untracked
        });
    }
    GameLease {
        shared,
        watcher,
        #[cfg(target_os = "linux")]
        workspace,
    }
}

/// Watch thread. No thread for Untracked, Nested-with-empty-spec, or a
/// platform with no matcher.
fn spawn_watcher(
    shared: Arc<LeaseShared>,
    child: Option<std::process::Child>,
    procs: Option<crate::launchreg::LiveProcs>,
    on_exit: OnExit,
    window: Option<WindowSource>,
) -> Option<std::thread::JoinHandle<()>> {
    // Untracked: nothing to observe. Nested with a spec: watch the game;
    // node-death misses a Steam launch that nests the resident client.
    // Nested with empty spec: node-death is the backstop.
    if matches!(shared.kind, LeaseKind::Untracked) {
        return None;
    }
    if matches!(shared.kind, LeaseKind::Nested) && shared.spec.is_empty() {
        return None;
    }
    // No matcher (macOS has no launch path): status lease, no poll.
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (child, procs, on_exit, window);
        None
    }
    #[cfg(any(target_os = "linux", windows))]
    {
        std::thread::Builder::new()
            .name("pf1-gamelease".into())
            .spawn(move || watch(shared, child, procs, on_exit, window))
            .ok()
    }
}

/// Looks for the game's own window each poll, until it finds one.
///
/// Shares phase 2's tick rather than a thread of its own. A compositor with a change token keeps
/// an idle desk from being re-read every second; every other source is read each time.
#[cfg(any(target_os = "linux", windows))]
struct WindowWatch {
    source: WindowSource,
    /// Last toplevels token seen. `None` re-reads.
    token: Option<u64>,
    /// Roots of the last read. New roots re-read an unchanged desk: a late report can name the
    /// owner of a window that is already up.
    roots: Vec<u32>,
}

#[cfg(any(target_os = "linux", windows))]
impl WindowWatch {
    fn new(source: WindowSource) -> Self {
        WindowWatch {
            source,
            token: None,
            roots: Vec::new(),
        }
    }

    /// One tick. `true` once the stage is spent: the window was found (placed once), or this
    /// source cannot list windows.
    fn poll(&mut self, shared: &Arc<LeaseShared>, live: &[crate::procscan::ProcRef]) -> bool {
        let roots = window_roots(
            &shared.spec,
            live,
            shared.owned_child().map(|c| c.pid),
            shared.spawned.map(|s| s.pid),
            reported_proc(shared).map(|p| p.pid),
        );
        let found: Option<(String, String)> = match &self.source {
            #[cfg(target_os = "linux")]
            WindowSource::Toplevels { compositor, stage } => {
                let now = crate::vdisplay::toplevels_token(*compositor);
                // A token that has not moved means no window opened, closed or was
                // retitled since the last read, so the answer cannot have changed.
                if now.is_some() && now == self.token && roots == self.roots {
                    return false;
                }
                self.token = now;
                self.roots.clone_from(&roots);
                let pids = crate::procscan::with_descendants(&roots);
                let Some(all) = crate::vdisplay::list_all_toplevels(*compositor) else {
                    // KWin without the grant, GNOME before the extension loads: waiting would hold
                    // the cover up to the client's cap on a game that is already playing.
                    shared.awaiting_window.store(false, Ordering::Relaxed);
                    tracing::debug!(
                        title = %shared.game.title,
                        compositor = compositor.id(),
                        "this compositor lists no windows yet — the launch hold ends at running"
                    );
                    return true;
                };
                let Some(win) = all.iter().find(|w| is_game_window(w, &pids, &shared.spec)) else {
                    return false;
                };
                on_screen(shared, &win.title, &win.app_id);
                if let Some(stage) = stage {
                    apply_on_window(stage, win);
                }
                return true;
            }
            #[cfg(target_os = "linux")]
            WindowSource::Gamescope { seat, appid } => {
                // Steam's own launch screen is its client's appid, so the game's shows only
                // once it is up.
                crate::vdisplay::gamescope_presenting(*appid, seat.as_deref())
                    .then(|| (shared.game.title.clone(), format!("steam_app_{appid}")))
            }
            #[cfg(windows)]
            WindowSource::Desktop => {
                let pids = crate::procscan::with_descendants(&roots);
                crate::game_term::visible_window(&pids).map(|title| (title, String::new()))
            }
        };
        let Some((title, class)) = found else {
            return false;
        };
        on_screen(shared, &title, &class);
        true
    }
}

/// Processes whose subtree may own the game's window.
///
/// The ones the lease matched and the one its provider reported, always: the entry's rule and
/// the provider each say those are the game. A `playnite://` hand-off has only the report. What
/// the host spawned counts only when the entry names nothing to match — a launcher command
/// (`steam steam://rungameid/…`) starts the launcher when it is not already up, and every
/// window the launcher draws would otherwise end the hold before the game drew anything.
#[cfg(any(target_os = "linux", windows))]
fn window_roots(
    spec: &crate::library::DetectSpec,
    live: &[crate::procscan::ProcRef],
    owned: Option<u32>,
    spawned: Option<u32>,
    reported: Option<u32>,
) -> Vec<u32> {
    let mut roots: Vec<u32> = live.iter().map(|r| r.pid).collect();
    roots.extend(reported.filter(|pid| !roots.contains(pid)));
    if spec.is_empty() {
        roots.extend(owned);
        roots.extend(spawned);
    }
    roots
}

/// The game's window is up: say so on `/status`, the event bus and the log.
#[cfg(any(target_os = "linux", windows))]
fn on_screen(shared: &LeaseShared, title: &str, class: &str) {
    shared.set_state(GameState::Window);
    shared.awaiting_window.store(false, Ordering::Relaxed);
    crate::events::emit(crate::events::EventKind::GameWindow {
        game: game_event_ref(shared),
        title: title.to_string(),
        app_id: class.to_string(),
    });
    tracing::info!(
        title = %shared.game.title,
        window = %title,
        app_id = %class,
        "the launched game is on screen"
    );
}

/// Is this toplevel the game's, rather than the launcher that opened it?
///
/// Pid first — that is the distinction the lease already makes, and Steam or
/// Faugus opening before the game is not in `pids`. Class is the fallback for
/// a game whose window belongs to a pid the scan never adopted.
#[cfg(target_os = "linux")]
fn is_game_window(
    win: &crate::vdisplay::Toplevel,
    pids: &[u32],
    spec: &crate::library::DetectSpec,
) -> bool {
    if win.pid.is_some_and(|p| pids.contains(&p)) {
        return true;
    }
    // Steam names an Xwayland game's class for its appid; a Wayland-native one
    // usually sets the same app_id.
    if let Some(app) = spec.steam_appid {
        if win.app_id.eq_ignore_ascii_case(&format!("steam_app_{app}")) {
            return true;
        }
    }
    // The operator-typed name, matched the way `procscan` matches it: the image
    // name, case-insensitively, with a Windows-style suffix tolerated.
    spec.process_name.as_deref().is_some_and(|want| {
        let want = want.strip_suffix(".exe").unwrap_or(want);
        !want.is_empty() && win.app_id.eq_ignore_ascii_case(want)
    })
}

/// Place the game's first window per the entry's `on_window`, on a compositor
/// the host can place windows on.
///
/// Best-effort and once: every step is a log line on refusal, and none of them
/// is worth failing a launch that is already on screen.
#[cfg(target_os = "linux")]
fn apply_on_window(stage: &WindowStage, win: &crate::vdisplay::Toplevel) {
    let head = &stage.head;
    let on = &stage.on_window;
    if !crate::vdisplay::places_windows(head.compositor) {
        return;
    }
    if on.wants_stream_output() && win.output != head.output {
        match crate::vdisplay::move_toplevel_to_output(head.compositor, &win.id, &head.output) {
            Ok(()) => tracing::info!(
                from = %win.output, to = %head.output,
                "carried the game's window onto the streamed head"
            ),
            Err(e) => tracing::warn!(
                from = %win.output, to = %head.output, error = %format!("{e:#}"),
                "game window not moved onto the streamed head — the player sees the desktop it \
                 opened on instead"
            ),
        }
    }
    for (want, verb) in [
        (on.wants_focus(), crate::vdisplay::WindowVerb::Focus),
        (
            on.wants_fullscreen(),
            crate::vdisplay::WindowVerb::Fullscreen,
        ),
    ] {
        if !want {
            continue;
        }
        if let Err(e) = crate::vdisplay::window_action(head.compositor, &head.output, verb, &win.id)
        {
            tracing::warn!(
                verb = verb.as_str(), error = %format!("{e:#}"),
                "on_window step not applied to the game's window"
            );
        }
    }
}

/// The launch exited on the spot: say so, and un-launch the registry record so
/// the next attempt starts the title instead of adopting a corpse. That drop is
/// what turns "needs a host restart" into "press it again".
///
/// Not [`finish`]: nothing ran, so there is no play time to credit and no game
/// exit to end the session on.
fn report_early_exit(shared: &LeaseShared, code: Option<i32>, ran: Duration) {
    let said = early_exit_message(&shared.game.title, code, ran.as_secs_f32());
    tracing::warn!(
        title = %shared.game.title,
        status = ?code,
        ran_s = ran.as_secs_f32(),
        "the launch command exited on the spot and nothing of this title is on the box — \
         reporting the launch as failed and un-launching its record"
    );
    shared.set_state(GameState::Exited);
    if let Some(p) = &shared.procs {
        crate::launchreg::unlaunched(p);
    }
    if let Some(tx) = &shared.outcome {
        let _ = tx.send(punktfunk_core::quic::LaunchOutcome::new(
            punktfunk_core::quic::LaunchOutcomeKind::Failed,
            &said,
        ));
    }
}

/// Wait for the game to appear, then for its window, then for it to stay gone.
#[cfg(any(target_os = "linux", windows))]
fn watch(
    shared: Arc<LeaseShared>,
    mut child: Option<std::process::Child>,
    procs: Option<crate::launchreg::LiveProcs>,
    on_exit: OnExit,
    window: Option<WindowSource>,
) {
    let scanner = crate::procscan::Scanner::system();
    let cancelled = || shared.cancel.load(Ordering::Relaxed);
    // Play time credits a recorded launch's title. Unrecorded — no client key,
    // no library id, a test lease — keeps none ([`crate::launchreg::Claim`]).
    let credit: Option<String> = procs.as_ref().and(shared.game.id.clone());
    // Concrete, non-empty pids only — never the spec (rule 1: a later
    // re-scan would adopt a copy the player started). Never cleared: last
    // set is how the record answers `Gone` instead of "no opinion".
    let publish = |live: &[crate::procscan::ProcRef]| {
        if live.is_empty() {
            return;
        }
        if let Some(slot) = procs.as_ref() {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = live.to_vec();
        }
    };
    let spawned_at = Instant::now();
    let mut kind = shared.kind.clone();

    // Last-seen pids. Phase 2 uses `alive` (one query each); a full `find`
    // only when they all vanish, to catch a re-exec. Uninitialized: phase 1
    // is the only assigner.
    let mut known: Vec<crate::procscan::ProcRef>;

    // Spawned pid (no Child handle). Cleared when gone, so `is_some()` is
    // "ours is up" — same one-way transition as a reaped `child`.
    let mut spawned = shared.spawned;
    // Re-verify each time: `alive` checks (pid, start), so a recycle is gone.
    let spawned_up = |s: &Option<crate::procscan::ProcRef>| -> bool {
        s.is_some_and(|p| !scanner.alive(&[p]).is_empty())
    };

    // Provider opinion ([`crate::runstate`]); `None` with no plugin. Re-read
    // each poll — the value is that it changes while the lease is alive.
    let reported = || shared.game.id.as_deref().and_then(crate::runstate::opinion);

    // After a shim: spec, else provider report, else Untracked. Same ladder
    // as [`open`], minus the child that just left.
    let fallback_kind = || {
        if !shared.spec.is_empty() {
            LeaseKind::Matched
        } else if crate::runstate::speaks_for(shared.game.id.as_deref()) {
            LeaseKind::Reported
        } else {
            LeaseKind::Untracked
        }
    };

    // Phase 1: wait for the game to appear.
    let start_deadline = spawned_at + START_GRACE;
    // Continuous scan sighting; scan-side twin of [`SHIM_WINDOW`].
    let mut seen_since: Option<Instant> = None;
    loop {
        if cancelled() {
            reap_later(child.take());
            return;
        }
        // Spawned pid gone (no handle, no exit status). "Quick" alone means
        // hand-off: every launch on this path goes through a launcher/shell.
        if matches!(kind, LeaseKind::Child)
            && child.is_none()
            && spawned.is_some()
            && !spawned_up(&spawned)
        {
            spawned = None;
            let quick = spawned_at.elapsed() < SHIM_WINDOW;
            kind = fallback_kind();
            if quick {
                if matches!(kind, LeaseKind::Untracked) {
                    tracing::info!(
                        title = %shared.game.title,
                        "the launch command exited immediately (a launcher handing off) and this \
                         title has no detect signals — stopping game tracking for it"
                    );
                    // Same Untracked `open` uses when it starts no watcher.
                    shared.set_state(GameState::Untracked);
                    return;
                }
                tracing::debug!(
                    title = %shared.game.title,
                    kind = kind.as_str(),
                    "the launch command handed off and exited — recognizing the game another way"
                );
            } else if matches!(kind, LeaseKind::Untracked) {
                // Outlived the shim window; nothing else identifies it.
                shared.was_running.store(true, Ordering::Relaxed);
                let run = credit.clone().map(|id| RunClock::since(id, spawned_at));
                finish(&shared, &on_exit, "the launched process exited", run);
                return;
            }
            // Signals exist: the scan decides; this may have been a wrapper.
        }
        if matches!(kind, LeaseKind::Child) {
            match child.as_mut().map(|c| c.try_wait()) {
                Some(Ok(Some(status))) => {
                    let quick = spawned_at.elapsed() < SHIM_WINDOW;
                    child = None; // reaped
                    shared.forget_child();
                    if quick && status.success() {
                        // Hand-off. Fall back to spec/report; with neither,
                        // stop tracking — do not treat the shim exit as the game.
                        kind = fallback_kind();
                        if matches!(kind, LeaseKind::Untracked) {
                            tracing::info!(
                                title = %shared.game.title,
                                "the launch command exited immediately (a launcher handing off) and \
                                 this title has no detect signals — stopping game tracking for it"
                            );
                            shared.set_state(GameState::Untracked);
                            return;
                        }
                        tracing::debug!(
                            title = %shared.game.title,
                            kind = kind.as_str(),
                            "the launch command handed off and exited — recognizing the game \
                             another way"
                        );
                    } else {
                        // Outlived the window, or failed. Game is gone; only
                        // a success after a real run counts as "played".
                        kind = fallback_kind();
                        if died_on_the_spot(quick, status.success(), &kind) {
                            report_early_exit(&shared, status.code(), spawned_at.elapsed());
                            return;
                        }
                        if matches!(kind, LeaseKind::Untracked) {
                            if spawned_at.elapsed() >= SHIM_WINDOW {
                                shared.was_running.store(true, Ordering::Relaxed);
                                let run = credit.clone().map(|id| RunClock::since(id, spawned_at));
                                finish(&shared, &on_exit, "the launched process exited", run);
                            }
                            return;
                        }
                        // Signals exist: the scan decides; this may have been a wrapper.
                    }
                }
                Some(Err(e)) => {
                    tracing::debug!(error = %e, "launched child not pollable — falling back to scanning");
                    child = None;
                    kind = fallback_kind();
                    if matches!(kind, LeaseKind::Untracked) {
                        shared.set_state(GameState::Untracked);
                        return;
                    }
                }
                _ => {}
            }
        }

        // Child-alive counts as running, but only after [`SHIM_WINDOW`]: a
        // hand-off looks like the game for a few seconds. Skipping the
        // window when `spec` is empty is the trap — that is the hand-off
        // shape, and the first poll would latch then treat the exit as the game.
        let child_alive = matches!(kind, LeaseKind::Child)
            && (child.is_some() || spawned.is_some())
            && spawned_at.elapsed() >= SHIM_WINDOW;
        let live = shared.find_procs(&scanner);
        // Same window for a scan hit: a pre-launch tree (Steam shader
        // reaper) carries the game's signals. One poll would latch into
        // phase 2 (`EXIT_CONFIRM` then ends the session). A window, not a
        // proof; sharp exclusions belong in [`crate::procscan`].
        let scan_settled = if live.is_empty() {
            seen_since = None;
            false
        } else {
            seen_since.get_or_insert_with(Instant::now).elapsed() >= SHIM_WINDOW
        };
        // A provider report is the launcher's statement, not an inferred
        // process: not gated by the shim window. Only way Reported leaves
        // this phase.
        let said_running = reported().is_some_and(|l| l.running);
        if scan_settled || child_alive || said_running {
            known = live.clone();
            publish(&live);
            shared.was_running.store(true, Ordering::Relaxed);
            shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
            shared.set_state(GameState::Running);
            crate::events::emit(crate::events::EventKind::GameRunning {
                game: game_event_ref(&shared),
            });
            tracing::info!(
                title = %shared.game.title,
                kind = kind.as_str(),
                procs = live.len(),
                // Names, not just a count ([`crate::procscan::names`]).
                names = ?crate::procscan::names(&live),
                "the launched game is running"
            );
            break;
        }
        if Instant::now() >= start_deadline {
            tracing::info!(
                title = %shared.game.title,
                grace_s = START_GRACE.as_secs(),
                "the launched game never appeared — leaving the session alone"
            );
            return;
        }
        std::thread::sleep(POLL);
    }

    // Phase 2: wait for the window, then for it to stay gone across
    // [`EXIT_CONFIRM`]. The window watch shares this poll: a game that never
    // shows one must still be exit-watched.
    let mut stage = window.map(WindowWatch::new);
    let mut run = credit.map(|id| RunClock::since(id, Instant::now()));
    let mut gone_since: Option<Instant> = None;
    let mut vetoed = false;
    loop {
        if cancelled() {
            // The session left; the game may live on unwatched.
            if let Some(run) = run.as_mut() {
                run.flush();
            }
            reap_later(child.take());
            return;
        }
        if matches!(kind, LeaseKind::Child) {
            if let Some(Ok(Some(_))) = child.as_mut().map(|c| c.try_wait()) {
                child = None;
                shared.forget_child();
                if shared.spec.is_empty() {
                    finish(&shared, &on_exit, "the launched process exited", run);
                    return;
                }
            }
            // Past start, a spawned pid going away is the game if nothing
            // else identifies it.
            if spawned.is_some() && !spawned_up(&spawned) {
                spawned = None;
                if shared.spec.is_empty() {
                    finish(&shared, &on_exit, "the launched process exited", run);
                    return;
                }
            }
        }
        let child_alive =
            matches!(kind, LeaseKind::Child) && (child.is_some() || spawned.is_some());
        // `alive` first; full `find` only when all known pids vanish — that
        // is also how a re-exec into a new pid is noticed.
        let live = {
            let still = scanner.alive(&known);
            if still.is_empty() {
                shared.find_procs(&scanner)
            } else {
                still
            }
        };
        if let Some(w) = stage.as_mut() {
            if w.poll(&shared, &live) {
                stage = None;
            }
        }
        if !live.is_empty() || child_alive {
            publish(&live);
            known = live;
            gone_since = None;
            vetoed = false;
            shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
        } else if let Some(said) = reported() {
            // Provider report is decisive both ways; `running_hint` may only
            // delay. A live report may hold a scan-invisible game; it dies
            // at [`crate::runstate::REPORT_TTL`], then the scan path resumes.
            if said.running {
                gone_since = None;
                vetoed = false;
                shared.last_seen_ms.store(now_ms(), Ordering::Relaxed);
            } else {
                finish(
                    &shared,
                    &on_exit,
                    "its provider reported the game stopped",
                    run,
                );
                return;
            }
        } else {
            // Continuous absence. Not reset by the veto — that is the bound.
            let gone_for = gone_since.get_or_insert_with(Instant::now).elapsed();
            if gone_for >= EXIT_CONFIRM {
                // Outside-scan veto only; never a reason to call it running
                // (`procscan::running_hint`).
                let hint_running = crate::procscan::running_hint(&shared.spec) == Some(true);
                if !exit_confirmed(gone_for, hint_running) {
                    if !vetoed {
                        vetoed = true;
                        tracing::info!(
                            title = %shared.game.title,
                            veto_limit_s = VETO_LIMIT.as_secs(),
                            "no game processes found, but its launcher still reports it running — \
                             holding off on ending the session"
                        );
                    }
                } else {
                    if hint_running {
                        // Hint still set after VETO_LIMIT: stale, not early.
                        tracing::warn!(
                            title = %shared.game.title,
                            gone_for_s = gone_for.as_secs(),
                            "its launcher still reports the game running, but nothing of it has \
                             been on the box for {}s — treating that as a stale flag and ending \
                             the session",
                            VETO_LIMIT.as_secs()
                        );
                    }
                    finish(&shared, &on_exit, "the game exited", run);
                    return;
                }
            }
        }
        if let Some(run) = run.as_mut() {
            run.tick();
        }
        std::thread::sleep(POLL);
    }
}

/// Exited: gone ≥ [`EXIT_CONFIRM`], and either unopposed or gone ≥
/// [`VETO_LIMIT`] despite [`crate::procscan::running_hint`]. Pure so the
/// bound is testable; the watch loop is not.
#[cfg(any(target_os = "linux", windows))]
fn exit_confirmed(gone_for: Duration, hint_running: bool) -> bool {
    gone_for >= EXIT_CONFIRM && (!hint_running || gone_for >= VETO_LIMIT)
}

/// Play-time clock for one run, credited to a library id. Flushes deltas, so
/// a reconnect's second watcher adds to the same run instead of restarting it.
#[cfg(any(target_os = "linux", windows))]
struct RunClock {
    id: String,
    started: Instant,
    /// How much of `started.elapsed()` is already on disk.
    flushed: Duration,
    last_flush: Instant,
}

#[cfg(any(target_os = "linux", windows))]
impl RunClock {
    fn since(id: String, started: Instant) -> Self {
        Self {
            id,
            started,
            flushed: Duration::ZERO,
            last_flush: Instant::now(),
        }
    }

    /// Once per [`STATS_FLUSH`]; call every poll.
    fn tick(&mut self) {
        if self.last_flush.elapsed() >= STATS_FLUSH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let total = self.started.elapsed();
        crate::library::record_run_time(&self.id, total.saturating_sub(self.flushed));
        self.flushed = total;
        self.last_flush = Instant::now();
    }
}

/// Record the exit: flush the run's play time, then the state. Skip `on_exit`
/// when the host itself ended the game.
#[cfg(any(target_os = "linux", windows))]
fn finish(shared: &Arc<LeaseShared>, on_exit: &OnExit, why: &str, run: Option<RunClock>) {
    if let Some(mut run) = run {
        run.flush();
    }
    shared.set_state(GameState::Exited);
    let terminated = shared.is_terminating();
    crate::events::emit(crate::events::EventKind::GameExited {
        game: game_event_ref(shared),
        reason: if terminated {
            crate::events::GameEndReason::Terminated
        } else {
            crate::events::GameEndReason::Exited
        },
    });
    if terminated {
        // Host-requested: the caller is already tearing down (or is not).
        tracing::info!(title = %shared.game.title, "the game the host asked to end is gone");
        return;
    }
    tracing::info!(title = %shared.game.title, reason = why, "the launched game exited");
    on_exit();
}

pub fn game_event_ref(shared: &LeaseShared) -> crate::events::GameRefPayload {
    crate::events::GameRefPayload {
        app: shared.game.id.clone(),
        title: shared.game.title.clone(),
        store: shared.game.store.clone(),
        client: shared.client.clone(),
        fingerprint: shared.fingerprint.clone(),
        plane: shared.plane,
        preset: shared.preset.clone(),
    }
}

/// Polite then kill, on a detached thread: callers are teardown/`Drop` and
/// must not block. Idempotent.
pub fn terminate(shared: Arc<LeaseShared>, why: &'static str) {
    if !shared.is_trackable() {
        tracing::debug!(
            title = %shared.game.title,
            "asked to end an untracked title — nothing to end"
        );
        return;
    }
    if shared.terminating.swap(true, Ordering::SeqCst) {
        return;
    }
    tracing::info!(title = %shared.game.title, reason = why, "ending the launched game");
    let name = "pf1-gameterm".to_string();
    let _ = std::thread::Builder::new().name(name).spawn(move || {
        terminate_blocking(&shared);
        // Live watcher reports the exit. Grace expiry / `POST /game/end`
        // already cancelled it, so this is the only reporter left.
        if shared.cancel.load(Ordering::Relaxed) {
            shared.set_state(GameState::Exited);
            crate::events::emit(crate::events::EventKind::GameExited {
                game: game_event_ref(&shared),
                reason: crate::events::GameEndReason::Terminated,
            });
        }
    });
}

/// Blocking ladder, so a test can drive it synchronously.
fn terminate_blocking(shared: &LeaseShared) {
    match shared.kind {
        // Releasing the display ends gamescope and the nested game together.
        LeaseKind::Nested => {
            // Force-release is not per-display. Another live session: skip;
            // the cost of being wrong is disturbing an unrelated client.
            let others = crate::session_status::count();
            if others > 0 {
                tracing::info!(
                    live_sessions = others,
                    title = %shared.game.title,
                    "not releasing kept displays to end this game — another session is streaming and \
                     the release is not per-display"
                );
                return;
            }
            let released = crate::vdisplay::registry::release(None);
            tracing::info!(
                released,
                title = %shared.game.title,
                "released the nested session's kept display to end its game"
            );
            // That release takes every kept display, a pre-warmed seat included, and nothing
            // else stands one back up before the next session ends — which is the player who
            // left a game running, the one the warm launch is for.
            #[cfg(target_os = "linux")]
            if released > 0 {
                crate::native::prewarm::spawn_run("game ended");
            }
        }
        LeaseKind::Child | LeaseKind::Matched | LeaseKind::Reported => {
            // A claim that lands while the ladder runs starts the title afresh; the
            // record goes once the ladder is through, so no claim adopts a corpse.
            if let Some(p) = &shared.procs {
                crate::launchreg::ending(p);
            }
            #[cfg(target_os = "linux")]
            unix_term_ladder(shared);
            #[cfg(windows)]
            windows_term_ladder(shared);
            if let Some(p) = &shared.procs {
                crate::launchreg::ended(p);
            }
        }
        LeaseKind::Untracked => {}
    }
}

/// Provider pid, resolved and start-pinned at use, or `None`.
///
/// [`LeaseKind::Reported`] has nothing for the matcher; without this, End
/// has no target. Rule 1 applies: this pid arrives from a plugin, so a
/// start before [`LeaseShared::launch_stamp`] is never a target, and on
/// Windows neither is a process outside this host's session.
#[cfg(any(target_os = "linux", windows))]
fn reported_proc(shared: &LeaseShared) -> Option<crate::procscan::ProcRef> {
    let pid = shared
        .game
        .id
        .as_deref()
        .and_then(crate::runstate::opinion)
        .filter(|l| l.running)?
        .pid?;
    let proc = crate::procscan::resolve(pid)?;
    if let Some(min) = shared.launch_stamp {
        let started = start_secs(proc);
        if started + crate::procscan::START_SLACK_SECS < min {
            return None; // predates this launch (rule 1)
        }
    }
    // The report comes from a LocalService plugin; the kill runs as SYSTEM.
    #[cfg(windows)]
    if !crate::game_term::in_our_session(pid) {
        return None;
    }
    Some(proc)
}

/// `ProcRef::start` in seconds on the [`LeaseShared::launch_stamp`] scale.
/// Duplicated because the matcher never sees a reported pid; the
/// predates-launch test pins the conversion.
#[cfg(any(target_os = "linux", windows))]
fn start_secs(p: crate::procscan::ProcRef) -> f64 {
    #[cfg(target_os = "linux")]
    let per_sec = {
        // SAFETY: `sysconf` reads a static limit by name; no memory of ours.
        // A non-positive answer is the documented failure and is handled.
        let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        // Same fallback as the matcher: non-positive would poison the compare.
        if ticks > 0 {
            ticks as f64
        } else {
            100.0
        }
    };
    // 10_000_000 FILETIME ticks per second (100 ns).
    #[cfg(windows)]
    let per_sec = 10_000_000.0;
    p.start as f64 / per_sec
}

/// The session left but the game runs on: wait for the child on a thread of
/// its own so it never lingers as a zombie under the host. One thread per
/// unwatched game, gone with it.
#[cfg(any(target_os = "linux", windows))]
fn reap_later(child: Option<std::process::Child>) {
    let Some(mut c) = child else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("pf1-childwait".into())
        .spawn(move || {
            let _ = c.wait();
        });
}

/// Whether the host-spawned child is gone, reaping it when it has exited.
/// `waitpid` answers for our own child only: already reaped elsewhere
/// ([`reap_later`], the watcher's `try_wait`), or never ours, is gone too.
#[cfg(target_os = "linux")]
fn reap_child(owned: Option<OwnedChild>) -> bool {
    let Some(c) = owned else {
        return true;
    };
    let mut status = 0;
    // SAFETY: WNOHANG never blocks; the only write is the status word handed in.
    unsafe { libc::waitpid(c.pid as i32, &mut status, libc::WNOHANG) != 0 }
}

/// SIGTERM, wait, SIGKILL, reap. Targets are fixed at entry and re-verified
/// by start time before each signal ([`crate::procscan::Scanner::alive`]).
#[cfg(target_os = "linux")]
fn unix_term_ladder(shared: &LeaseShared) {
    let scanner = crate::procscan::Scanner::system();
    let owned = shared.owned_child();

    // Group-signal the child when it leads one: that reaches a shell
    // wrapper's grandchildren (the game). A per-pid sweep would miss them.
    let signal_child = |sig: i32| -> bool {
        let Some(c) = owned else { return false };
        let target = if c.group_leader {
            -(c.pid as i32)
        } else {
            c.pid as i32
        };
        // SAFETY: `kill` returns a status and touches no memory of ours. A
        // negative target is only a group this host created with the child
        // as leader (`OwnedChild::group_leader`) — never the host's group.
        unsafe { libc::kill(target, sig) == 0 }
    };
    // Matcher hits plus `reported_proc` (the only member for Reported). Taken
    // once: a process that starts after this point belongs to a session that
    // claimed the title while the ladder ran, and must outlive it.
    let targets = {
        let mut procs = shared.find_procs(&scanner);
        if let Some(p) = reported_proc(shared) {
            if !procs.iter().any(|q| q.pid == p.pid) {
                procs.push(p);
            }
        }
        procs
    };
    let signal_matched = |sig: i32| -> usize {
        // Re-verify immediately; a recycle since last sweep is out.
        scanner
            .alive(&targets)
            .into_iter()
            // SAFETY: as above, for a single pid just re-verified as adopted.
            .filter(|p| unsafe { libc::kill(p.pid as i32, sig) == 0 })
            .count()
    };
    let signal_all = |sig: i32| -> usize { usize::from(signal_child(sig)) + signal_matched(sig) };
    // Reaped (or never ours), and the group it led, if any, empty too. A zombie
    // still answers signal 0; `waitpid` is what tells exited from present.
    let child_gone =
        || reap_child(owned) && !(owned.is_some_and(|c| c.group_leader) && signal_child(0));

    let asked = signal_all(libc::SIGTERM);
    tracing::debug!(
        title = %shared.game.title,
        signalled = asked,
        grace_s = TERM_GRACE.as_secs(),
        "asked the game to close"
    );
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if scanner.alive(&targets).is_empty() && child_gone() {
            tracing::info!(title = %shared.game.title, "the game closed when asked");
            return;
        }
    }
    let killed = signal_all(libc::SIGKILL);
    tracing::warn!(
        title = %shared.game.title,
        killed,
        grace_s = TERM_GRACE.as_secs(),
        "the game did not close when asked — killed it"
    );
    // Kill lands within milliseconds. Unreaped, the child would sit as a zombie
    // the registry counted as running until the host exited.
    for _ in 0..20 {
        if child_gone() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// WM_CLOSE, wait, then terminate. No `Child` on Windows; fold in
/// [`LeaseShared::spawned`] or an empty spec has nothing to end.
#[cfg(windows)]
fn windows_term_ladder(shared: &LeaseShared) {
    let scanner = crate::procscan::Scanner::system();
    let live = || {
        let mut procs = scanner.alive(&shared.find_procs(&scanner));
        // Re-verify and de-dupe. `spawned` and `reported_proc` join on the
        // same terms; Reported has only the latter.
        let mut fold = |p: crate::procscan::ProcRef| {
            if !scanner.alive(&[p]).is_empty() && !procs.iter().any(|q| q.pid == p.pid) {
                procs.push(p);
            }
        };
        if let Some(p) = shared.spawned {
            fold(p);
        }
        if let Some(p) = reported_proc(shared) {
            fold(p);
        }
        procs
    };

    // Taken once: a process started after this belongs to a newer session (as on Unix).
    let targets = live();
    let pids: Vec<u32> = targets.iter().map(|p| p.pid).collect();
    if pids.is_empty() {
        tracing::info!(title = %shared.game.title, "the game is already gone — nothing to end");
        return;
    }
    // WM_CLOSE is the window X; the game can save.
    let asked = crate::game_term::request_close(&pids);
    tracing::debug!(
        title = %shared.game.title,
        windows_asked = asked,
        procs = pids.len(),
        grace_s = TERM_GRACE.as_secs(),
        "asked the game to close"
    );
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if scanner.alive(&targets).is_empty() {
            tracing::info!(title = %shared.game.title, "the game closed when asked");
            return;
        }
    }
    // Fresh (pid, creation) before kill: Windows recycles pids quickly.
    let remaining: Vec<u32> = scanner.alive(&targets).into_iter().map(|p| p.pid).collect();
    let killed = crate::game_term::kill(&remaining);
    tracing::warn!(
        title = %shared.game.title,
        killed,
        grace_s = TERM_GRACE.as_secs(),
        "the game did not close when asked — killed it"
    );
}

/// End pids an earlier launch adopted
/// ([`crate::session_settings::GameOnNewLaunch::End`]).
///
/// Not [`terminate`]: the previous game usually has no lease left, only
/// the set published to [`crate::launchreg`]. Signals that set only.
/// Blocking — the caller is about to spawn — and bounded by [`TERM_GRACE`].
pub fn end_previous_launch(title: &str, procs: &[crate::procscan::ProcRef], why: &str) -> usize {
    // Re-verify before every signal (rule 2): remembered pids recycle.
    let live = || crate::procscan::alive(procs);

    let first = live();
    if first.is_empty() {
        return 0;
    }
    tracing::info!(
        title,
        procs = first.len(),
        reason = why,
        "ending the previous game before launching the new one"
    );
    ask_to_close(&first);
    let deadline = Instant::now() + TERM_GRACE;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if live().is_empty() {
            tracing::info!(title, "the previous game closed when asked");
            return first.len();
        }
    }
    let remaining = live();
    tracing::warn!(
        title,
        remaining = remaining.len(),
        grace_s = TERM_GRACE.as_secs(),
        "the previous game did not close when asked — killing it"
    );
    force_close(&remaining);
    first.len()
}

/// WM_CLOSE / SIGTERM so the game can save.
fn ask_to_close(procs: &[crate::procscan::ProcRef]) {
    #[cfg(windows)]
    {
        let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        crate::game_term::request_close(&pids);
    }
    #[cfg(target_os = "linux")]
    for p in procs {
        // SAFETY: `kill` returns a status and touches no memory of ours.
        // Always a POSITIVE pid — matcher-adopted, not a group we lead.
        // A negative target would signal an unrelated process group.
        unsafe {
            libc::kill(p.pid as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = procs;
}

/// SIGKILL / TerminateProcess for whatever ignored [`ask_to_close`].
fn force_close(procs: &[crate::procscan::ProcRef]) {
    #[cfg(windows)]
    {
        let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        crate::game_term::kill(&pids);
    }
    #[cfg(target_os = "linux")]
    for p in procs {
        // SAFETY: as above — positive pid, matcher-adopted.
        unsafe {
            libc::kill(p.pid as i32, libc::SIGKILL);
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = procs;
}

/// [`crate::session_settings::GameOnNewLaunch`] for a session about to launch
/// `game_id`.
///
/// Call **before** spawn. No-op on the default, or with no launch records.
/// Bare-spawn gamescope nests earlier, so the previous game closes *after*
/// the new one starts; it has its own display, so the contention is moot.
pub fn end_others_for_new_launch(fingerprint: Option<&str>, game_id: Option<&str>) {
    if crate::session_settings::get().game_on_new_launch
        != crate::session_settings::GameOnNewLaunch::End
    {
        return;
    }
    for other in crate::launchreg::others_still_running(fingerprint, game_id) {
        end_previous_launch(
            &other.game_id,
            &other.procs,
            "the player launched a different title",
        );
    }
}

/// Reconnect window. Come back before the deadline: drop termination.
/// Miss it: the game ends.
///
/// The [`GameLease`] is already dropped (watcher cancelled, `on_exit` dead).
/// The new session re-adopts the *game* via [`crate::launchreg`], which
/// carries the original launch stamp.
pub struct Pending {
    pub shared: Arc<LeaseShared>,
    pub deadline: Instant,
    /// Client that may re-adopt (identity match on reconnect).
    pub fingerprint: Option<String>,
}

fn registry() -> &'static Mutex<Vec<Pending>> {
    static REG: OnceLock<Mutex<Vec<Pending>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

/// Game ends in `grace` unless the same client reconnects first.
pub fn arm_grace(shared: Arc<LeaseShared>, fingerprint: Option<String>, grace: Duration) {
    if !shared.is_trackable() {
        return;
    }
    tracing::info!(
        title = %shared.game.title,
        grace_s = grace.as_secs(),
        "the client is gone — the game ends when the reconnect window closes"
    );
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Pending {
            shared,
            deadline: Instant::now() + grace,
            fingerprint,
        });
    start_reaper();
}

/// Drop pending termination for `fingerprint` + `app`.
///
/// Returns the reprieved leases (**corpses**: watchers cancelled, see
/// [`Pending`]). The new session's launch stamp comes from
/// [`crate::launchreg`], not from here.
pub fn readopt(fingerprint: Option<&str>, app: Option<&str>) -> Vec<Arc<LeaseShared>> {
    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut reprieved = Vec::new();
    reg.retain(|p| {
        let same_client = match (&p.fingerprint, fingerprint) {
            (Some(a), Some(b)) => a == b,
            // No fingerprint: keep pending. Any reconnect must not reprieve
            // any game.
            _ => false,
        };
        let same_app = p.shared.game.id.as_deref() == app;
        if same_client && same_app {
            tracing::info!(
                title = %p.shared.game.title,
                "the client reconnected inside the window — the game keeps running"
            );
            reprieved.push(p.shared.clone());
            false
        } else {
            true
        }
    });
    reprieved
}

pub fn pending_snapshot() -> Vec<(Arc<LeaseShared>, u64)> {
    let now = Instant::now();
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|p| {
            (
                p.shared.clone(),
                p.deadline.saturating_duration_since(now).as_secs(),
            )
        })
        .collect()
}

/// End pending games now. `app` filters; `None` ends all. Returns count.
pub fn end_pending(app: Option<&str>) -> usize {
    let taken: Vec<Arc<LeaseShared>> = {
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
        let (hit, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut *reg)
            .into_iter()
            .partition(|p| app.is_none() || p.shared.game.id.as_deref() == app);
        *reg = keep;
        hit.into_iter().map(|p| p.shared).collect()
    };
    let n = taken.len();
    for shared in taken {
        terminate(shared, "ended from the management API");
    }
    n
}

/// One process-lifetime thread; started on demand; sleeps while empty.
fn start_reaper() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let _ = std::thread::Builder::new()
            .name("pf1-gamegrace".into())
            .spawn(|| loop {
                std::thread::sleep(POLL);
                let now = Instant::now();
                let due: Vec<Arc<LeaseShared>> = {
                    let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
                    let (due, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut *reg)
                        .into_iter()
                        .partition(|p| now >= p.deadline);
                    *reg = keep;
                    due.into_iter().map(|p| p.shared).collect()
                };
                for shared in due {
                    // Player may already have quit; terminate is then a no-op.
                    terminate(shared, "the reconnect window closed with no client");
                }
            });
    });
}

/// Session-end policy from [`crate::session_settings`], applied once so
/// both planes match. `launch` answers only: has a newer session claimed it?
pub fn on_session_end(
    lease: &GameLease,
    deliberate: bool,
    fingerprint: Option<&str>,
    launch: Option<&crate::launchreg::Claim>,
) {
    use crate::session_settings::GameOnSessionEnd;
    let settings = crate::session_settings::get();
    let shared = lease.shared();
    if !shared.is_trackable() || shared.state() == GameState::Exited {
        return; // untracked, or already exited
    }
    // Newer session already owns this launch. Concurrent with handshake:
    // if teardown wins, they adopt the released record; if handshake wins,
    // `superseded` is set here so `Always` cannot `arm_grace` a game
    // `readopt` already missed.
    if launch.is_some_and(|c| c.superseded()) {
        tracing::info!(
            title = %shared.game.title,
            "this client already came back for this game — leaving it to the session that has it now"
        );
        return;
    }
    let end_now = |shared: Arc<LeaseShared>| {
        // Nested: display teardown already ends gamescope. Asking again
        // races a still-live session, which the registry refuses anyway.
        if matches!(shared.kind, LeaseKind::Nested) {
            tracing::debug!(
                title = %shared.game.title,
                "the nested session's display teardown ends its game — nothing extra to do"
            );
            return;
        }
        terminate(shared, "the session was stopped deliberately");
    };
    match settings.game_on_session_end {
        GameOnSessionEnd::Keep => {}
        GameOnSessionEnd::OnQuit => {
            if deliberate {
                end_now(shared);
            }
        }
        GameOnSessionEnd::Always => {
            if deliberate {
                end_now(shared);
            } else {
                // A drop is not a quit. Nested: the display may outlive
                // this window under keep-alive; expiry releases it.
                arm_grace(
                    shared,
                    fingerprint.map(str::to_string),
                    Duration::from_secs(settings.disconnect_grace_seconds.into()),
                );
            }
        }
    }
}

/// RAII: [`on_session_end`] on every stream-loop exit, including panic.
///
/// `quit` is read at **drop**: vanish gets a reconnect window, a
/// deliberate stop does not
/// ([`GameOnSessionEnd::Always`](crate::session_settings::GameOnSessionEnd::Always)).
/// Both planes.
pub struct SessionGuard {
    lease: GameLease,
    quit: Arc<AtomicBool>,
    /// Hex fingerprint: only this client may reclaim the game.
    fingerprint: Option<String>,
    /// Launch claim. Dropped **after** `Drop` reads it (field drop order),
    /// which is what opens the reconnect window.
    launch: Option<crate::launchreg::Claim>,
}

impl SessionGuard {
    /// Bind `lease` to this session. `quit` is read at drop.
    pub fn new(
        lease: GameLease,
        quit: Arc<AtomicBool>,
        fingerprint: Option<String>,
        launch: Option<crate::launchreg::Claim>,
    ) -> Self {
        Self {
            lease,
            quit,
            fingerprint,
            launch,
        }
    }

    pub fn shared(&self) -> Arc<LeaseShared> {
        self.lease.shared()
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        on_session_end(
            &self.lease,
            self.quit.load(Ordering::SeqCst),
            self.fingerprint.as_deref(),
            self.launch.as_ref(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture. Unique `id`: the grace registry is process-wide and tests
    /// run in parallel.
    fn req(id: &str, spec: DetectSpec, nested: bool) -> LeaseRequest {
        LeaseRequest {
            game: GameRef {
                id: Some(id.to_string()),
                store: Some("steam".into()),
                title: format!("Test Title {id}"),
            },
            client: "Deck".into(),
            fingerprint: None,
            preset: None,
            plane: crate::events::Plane::Native,
            spec,
            nested,
            scope_pid: None,
            launcher: false,
            child: None,
            spawned: None,
            // No floor: these leases never match real processes.
            launch_stamp: None,
            // Unrecorded; nothing here spawns.
            procs: None,
            #[cfg(target_os = "linux")]
            workspace: None,
            window: None,
            outcome: None,
        }
    }

    /// Scoping is safe only where the game must descend from that gamescope.
    #[test]
    fn only_a_nested_spawn_or_a_steam_launch_narrows_the_scan() {
        // gamescope's own primary child, and anything Steam starts inside it.
        assert_eq!(scan_scope(true, false, Some(42)), Some(42));
        assert_eq!(scan_scope(false, true, Some(42)), Some(42));
        // A kept session's Lutris/Heroic/custom launch is the host's child, beside gamescope.
        assert_eq!(scan_scope(false, false, Some(42)), None);
        // No compositor of ours: every other backend keeps the scan it has today.
        assert_eq!(scan_scope(true, true, None), None);
    }

    /// The state a client's launch hold ends on, and the one it never overwrites.
    ///
    /// `running` with no window owed is what every client reads as "the host has said all it
    /// will" — a seat at Steam's sign-in screen needs that within seconds, not after two
    /// minutes of cover.
    #[test]
    fn a_launch_with_nothing_to_wait_for_reports_running_at_once() {
        // Recognized by a name no process has: the scan leaves this lease `launching`.
        let spec = DetectSpec {
            process_name: Some("pf-no-such-game".into()),
            ..DetectSpec::default()
        };
        let lease = open(req("steam:signin", spec, false), Box::new(|| {}));
        let shared = lease.shared();
        assert_eq!(shared.state(), GameState::Launching);
        shared.awaiting_window.store(true, Ordering::Relaxed);
        shared.launch_hold_ends();
        assert_eq!(shared.state(), GameState::Running);
        assert!(!shared.awaits_window(), "no client waits for a window here");

        // The watcher owns every state after `launching`: a window already found stays found.
        shared.set_state(GameState::Window);
        shared.launch_hold_ends();
        assert_eq!(shared.state(), GameState::Window);
    }

    /// A launcher entry is Untracked regardless of how it was started.
    ///
    /// Same tile, two hidden states: Steam not running → live child (`Child`,
    /// quit ends the session); already running → shim exit (untracked).
    /// Untracked is the one that always applies.
    #[test]
    fn a_launcher_entry_is_untracked_however_it_was_started() {
        let mut r = req("steam:big-picture", DetectSpec::default(), false);
        r.launcher = true;
        let lease = open(r, Box::new(|| {}));
        assert!(matches!(lease.shared().kind, LeaseKind::Untracked));
        assert!(!lease.shared().is_trackable());

        // Signals would be Matched; `launcher` outranks them.
        let mut r = req(
            "steam:big-picture-2",
            DetectSpec::exe("/usr/bin/steam"),
            false,
        );
        r.launcher = true;
        assert!(
            !r.spec.is_empty(),
            "the guard is only meaningful with signals"
        );
        let lease = open(r, Box::new(|| {}));
        assert!(matches!(lease.shared().kind, LeaseKind::Untracked));
        assert!(!lease.shared().is_trackable());

        // Control: without the flag, the same request is Matched.
        let plain = open(
            req("steam:570", DetectSpec::exe("/usr/bin/steam"), false),
            Box::new(|| {}),
        );
        assert!(matches!(plain.shared().kind, LeaseKind::Matched));
        assert!(plain.shared().is_trackable());
    }

    fn is_pending(id: &str) -> bool {
        pending_snapshot()
            .iter()
            .any(|(s, _)| s.game.id.as_deref() == Some(id))
    }

    /// `running_hint` may delay an exit; it must not delay it forever.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_launcher_veto_expires_instead_of_pinning_a_session_open() {
        let brief = EXIT_CONFIRM / 2;
        let confirmed = EXIT_CONFIRM + Duration::from_secs(1);
        let long = VETO_LIMIT + Duration::from_secs(1);

        // Inside EXIT_CONFIRM: a process swap is still plausible.
        assert!(!exit_confirmed(brief, false));
        assert!(!exit_confirmed(brief, true));
        assert!(exit_confirmed(confirmed, false));
        // Hint still set: that is what the veto is for.
        assert!(!exit_confirmed(confirmed, true));
        // Past VETO_LIMIT the hint is stale.
        assert!(exit_confirmed(long, true));
        assert!(exit_confirmed(long, false));
        // Also pins VETO_LIMIT > EXIT_CONFIRM.
    }

    #[test]
    fn kind_follows_what_the_launch_gave_us() {
        // Nested wins: the display layer owns the lifetime.
        let l = open(
            req("steam:100", DetectSpec::steam(100), true),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Nested));
        let l = open(
            req("steam:101", DetectSpec::steam(101), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Matched));
        let l = open(
            req("steam:102", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Untracked));
        assert!(!l.shared().is_trackable());
    }

    /// Empty spec, but the provider reports: [`LeaseKind::Reported`].
    /// Covers a `playnite://` hand-off with nothing to scan.
    #[test]
    fn a_reported_title_is_tracked_where_it_used_to_be_untracked() {
        // Control: no provider → Untracked.
        let l = open(
            req("playnite:lease-test", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Untracked));
        assert!(!l.shared().is_trackable());
        drop(l);

        // Provider speaks, even "not running" (the launch-time report).
        // Kind follows *reporting*, not the current answer.
        crate::runstate::report(
            "playnite-lease-test",
            ["playnite:lease-test".to_string()].into_iter().collect(),
            std::collections::HashMap::new(),
        );
        let l = open(
            req("playnite:lease-test", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Reported));
        assert!(
            l.shared().is_trackable(),
            "so its exit is noticed and `POST /game/end` has a target"
        );
        drop(l);
        crate::runstate::forget("playnite-lease-test");

        // Provider gone → Untracked. A stale report must not pin a session.
        let l = open(
            req("playnite:lease-test", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Untracked));
    }

    /// A reported pid that started before this launch is never a terminate
    /// target. Same start-time floor as every other adopted process.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_reported_pid_that_predates_the_launch_is_never_a_target() {
        const ID: &str = "playnite:reported-pid";
        const PROVIDER: &str = "playnite-reported-pid";

        // Exists before either stamp: a pid this session never launched.
        let mut victim = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn the process a plugin would point at");
        crate::runstate::report(
            PROVIDER,
            [ID.to_string()].into_iter().collect(),
            [(ID.to_string(), Some(victim.id()))].into_iter().collect(),
        );

        let l = open(
            LeaseRequest {
                launch_stamp: launch_clock().map(|s| s + 60.0),
                ..req(ID, DetectSpec::default(), false)
            },
            Box::new(|| {}),
        );
        assert!(matches!(l.shared().kind(), LeaseKind::Reported));
        assert!(
            reported_proc(&l.shared()).is_none(),
            "a reported pid that predates the launch must not be signallable"
        );
        drop(l);

        // Same pid, stamp after it exists: pins START_SLACK_SECS (exact
        // compare would reject the real game).
        let l = open(
            LeaseRequest {
                launch_stamp: launch_clock(),
                ..req(ID, DetectSpec::default(), false)
            },
            Box::new(|| {}),
        );
        assert_eq!(
            reported_proc(&l.shared()).map(|p| p.pid),
            Some(victim.id()),
            "the pid a provider started for this launch is still what `End` aims at"
        );
        drop(l);

        crate::runstate::forget(PROVIDER);
        let _ = victim.kill();
        let _ = victim.wait();
    }

    /// The one rule that separates the reporter's dead launch from a Proton
    /// prefix build: a *failing* exit inside the shim window with nothing left
    /// to recognize. A launcher hands off with success and keeps its grace; a
    /// title with detect signals keeps its grace whatever its wrapper did.
    #[test]
    fn only_a_failing_exit_with_nothing_left_to_watch_is_a_dead_launch() {
        assert!(died_on_the_spot(true, false, &LeaseKind::Untracked));

        // A launcher handing off. This is every Steam and Heroic launch.
        assert!(!died_on_the_spot(true, true, &LeaseKind::Untracked));
        // The game is recognizable another way — the scan gets START_GRACE.
        assert!(!died_on_the_spot(true, false, &LeaseKind::Matched));
        assert!(!died_on_the_spot(true, false, &LeaseKind::Reported));
        // Past the shim window it ran; that exit is the game's, not a failure
        // to start, and `finish` reports it with the play time.
        assert!(!died_on_the_spot(false, false, &LeaseKind::Untracked));
        assert!(!died_on_the_spot(false, true, &LeaseKind::Untracked));
    }

    /// The shell's own two codes are named; anything else gets how long it lasted.
    #[test]
    fn a_dead_launch_says_what_the_player_can_act_on() {
        assert_eq!(
            early_exit_message("Quail", Some(127), 0.2),
            "Couldn't start Quail \u{2014} this host doesn't have that command."
        );
        assert_eq!(
            early_exit_message("Quail", Some(126), 0.2),
            "Couldn't start Quail \u{2014} this host can't run that command."
        );
        assert_eq!(
            early_exit_message("Quail", Some(1), 0.24),
            "Couldn't start Quail \u{2014} it closed again after 0.2 s."
        );
        // Killed by a signal: no code, same shape.
        assert!(early_exit_message("Quail", None, 1.0).starts_with("Couldn't start Quail"));
    }

    /// The whole of ask 2 in one place: the client hears `failed`, and the
    /// record stops covering — so the retry that follows starts the title
    /// instead of adopting the launch that just died.
    #[test]
    fn a_dead_launch_is_reported_and_stops_covering_the_next_attempt() {
        let (fp, app) = (Some("fp-dead"), Some("custom:dead"));
        let first = crate::launchreg::claim(fp, app, false, Some(10.0));
        first.launched();
        let procs = first.procs().expect("recorded");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let lease = open(
            LeaseRequest {
                procs: Some(procs),
                outcome: Some(tx),
                ..req(app.unwrap(), DetectSpec::default(), false)
            },
            Box::new(|| {}),
        );
        let shared = lease.shared();
        report_early_exit(&shared, Some(127), Duration::from_millis(200));

        let said = rx.try_recv().expect("the client is told");
        assert_eq!(said.kind, punktfunk_core::quic::LaunchOutcomeKind::Failed);
        assert!(
            said.message.contains("doesn't have that command"),
            "{said:?}"
        );
        assert_eq!(shared.state(), GameState::Exited);

        let retry = crate::launchreg::claim(fp, app, false, Some(99.0));
        assert!(
            retry.must_spawn(),
            "the attempt after a dead launch must start the title, not adopt it"
        );
        retry.abandon();
        drop(first);
    }

    #[test]
    fn an_untracked_lease_is_never_terminated() {
        let l = open(
            req("steam:110", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        let shared = l.shared();
        terminate(shared.clone(), "test");
        assert!(!shared.is_terminating(), "untracked leases must stay inert");
    }

    #[test]
    fn terminate_is_idempotent() {
        // Nested: ladder is a display release, not a signal to a process.
        let l = open(
            req("steam:120", DetectSpec::steam(120), true),
            Box::new(|| {}),
        );
        let shared = l.shared();
        terminate(shared.clone(), "first");
        assert!(shared.is_terminating());
        // Second call must not re-run the ladder.
        terminate(shared.clone(), "second");
        assert!(shared.is_terminating());
    }

    #[test]
    fn grace_readoption_matches_client_and_title() {
        let id = "steam:130";
        let l = open(req(id, DetectSpec::steam(130), false), Box::new(|| {}));
        arm_grace(
            l.shared(),
            Some("fp-130".into()),
            Duration::from_secs(3_600),
        );
        assert!(readopt(Some("fp-other"), Some(id)).is_empty());
        assert!(readopt(Some("fp-130"), Some("steam:9999")).is_empty());
        // Missing fingerprint must not reprieve: any reconnect would.
        assert!(readopt(None, Some(id)).is_empty());
        assert!(is_pending(id), "none of those should have reprieved it");
        let saved = readopt(Some("fp-130"), Some(id));
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].game.id.as_deref(), Some(id));
        assert!(!is_pending(id));
    }

    #[test]
    fn pending_snapshot_reports_the_window_left() {
        let id = "steam:140";
        let l = open(req(id, DetectSpec::steam(140), false), Box::new(|| {}));
        arm_grace(l.shared(), Some("fp-140".into()), Duration::from_secs(300));
        let mine = pending_snapshot()
            .into_iter()
            .find(|(s, _)| s.game.id.as_deref() == Some(id))
            .expect("armed lease is pending");
        assert!(mine.1 > 290 && mine.1 <= 300, "remaining was {}", mine.1);
        // Clear this entry so a sibling test cannot see it.
        assert_eq!(readopt(Some("fp-140"), Some(id)).len(), 1);
    }

    #[test]
    fn end_pending_only_takes_the_named_title() {
        let (a, b) = ("steam:150", "steam:151");
        let la = open(req(a, DetectSpec::steam(150), true), Box::new(|| {}));
        let lb = open(req(b, DetectSpec::steam(151), true), Box::new(|| {}));
        arm_grace(
            la.shared(),
            Some("fp-150".into()),
            Duration::from_secs(3_600),
        );
        arm_grace(
            lb.shared(),
            Some("fp-151".into()),
            Duration::from_secs(3_600),
        );
        assert_eq!(end_pending(Some(a)), 1);
        assert!(!is_pending(a));
        assert!(is_pending(b));
        assert!(la.shared().is_terminating());
        assert!(!lb.shared().is_terminating());
        assert_eq!(end_pending(Some("steam:99999")), 0);
        assert_eq!(readopt(Some("fp-151"), Some(b)).len(), 1);
    }

    /// Quick successful child exit is a hand-off, not the game exiting.
    /// Spec matches nothing so the game never appears.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_launcher_that_hands_off_and_exits_is_not_the_game_exiting() {
        use std::sync::atomic::AtomicUsize;

        static EXITS: AtomicUsize = AtomicUsize::new(0);
        EXITS.store(0, Ordering::SeqCst);
        // PATH, not `/bin/true`: NixOS has only `/bin/sh` in `/bin`.
        let child = std::process::Command::new("true")
            .spawn()
            .expect("spawn the fake launcher");
        let lease = open(
            LeaseRequest {
                game: GameRef {
                    id: Some("steam:999001".into()),
                    store: Some("steam".into()),
                    title: "Handoff".into(),
                },
                client: "test".into(),
                fingerprint: None,
                preset: None,
                plane: crate::events::Plane::Native,
                // Real signal nothing will match: the game never shows up.
                spec: DetectSpec::steam(999_001),
                nested: false,
                scope_pid: None,
                launcher: false,
                child: Some((child, false)),
                spawned: None,
                launch_stamp: None,
                procs: None,
                #[cfg(target_os = "linux")]
                workspace: None,
                window: None,
                outcome: None,
            },
            Box::new(|| {
                EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        // Past SHIM_WINDOW + EXIT_CONFIRM: longer than a false exit took.
        std::thread::sleep(SHIM_WINDOW + EXIT_CONFIRM + Duration::from_secs(2));
        assert_eq!(
            EXITS.load(Ordering::SeqCst),
            0,
            "a launcher handing off must not end the session"
        );
        assert_ne!(
            lease.shared().state(),
            GameState::Exited,
            "the game never ran, so it cannot have exited"
        );
    }

    /// Pid, no `Child`, empty spec: still [`LeaseKind::Child`].
    /// Windows `CreateProcessAsUserW` shape, driven on Linux.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pid_only_launch_is_tracked_like_a_child() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn the fake game");
        let pid = child.id();
        let lease = open(
            LeaseRequest {
                spawned: Some(pid),
                // Empty spec: nothing to recognize the game by.
                spec: DetectSpec::default(),
                launch_stamp: launch_clock(),
                ..req("custom:pid-only", DetectSpec::default(), false)
            },
            Box::new(|| {}),
        );
        assert!(
            matches!(lease.shared().kind(), LeaseKind::Child),
            "a pid we spawned is our own child, whatever the spec says"
        );
        assert!(
            lease.shared().is_trackable(),
            "and therefore endable — `POST /game/end` had no pid to signal before this"
        );
        drop(lease);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Pid-only launch: process death is the game exiting.
    ///
    /// Fixture must outlive [`SHIM_WINDOW`]; shorter is a hand-off. Ignored:
    /// shim window plus [`EXIT_CONFIRM`], ~12 s.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~12s (shim window + exit confirmation)"]
    fn a_pid_only_launch_reports_its_exit() {
        use std::sync::atomic::AtomicUsize;

        // Reap on another thread: a zombie keeps `/proc/<pid>` and looks alive.
        let mut child = std::process::Command::new("sleep")
            .arg("8")
            .spawn()
            .expect("spawn the fake game");
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        static PID_EXITS: AtomicUsize = AtomicUsize::new(0);
        PID_EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                spawned: Some(pid),
                spec: DetectSpec::default(),
                launch_stamp: launch_clock(),
                ..req("custom:pid-only-exit", DetectSpec::default(), false)
            },
            Box::new(|| {
                PID_EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();

        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline && shared.state() != GameState::Exited {
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(shared.state(), GameState::Exited, "the game should be gone");
        assert_eq!(
            PID_EXITS.load(Ordering::SeqCst),
            1,
            "the player quitting must end the session exactly once"
        );
    }

    /// Pid-only hand-off with empty spec must not end the session.
    ///
    /// Forwarder dies inside [`SHIM_WINDOW`]; that is not the game. Ignored:
    /// must outlive shim + [`EXIT_CONFIRM`].
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~10s (shim window + exit confirmation)"]
    fn a_pid_only_handoff_with_no_signals_never_ends_the_session() {
        use std::sync::atomic::AtomicUsize;

        // Reap on another thread: a zombie looks alive.
        let mut child = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .expect("spawn the fake forwarder");
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        static HANDOFF_EXITS: AtomicUsize = AtomicUsize::new(0);
        HANDOFF_EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                spawned: Some(pid),
                spec: DetectSpec::default(),
                launch_stamp: launch_clock(),
                ..req("playnite:handoff", DetectSpec::default(), false)
            },
            Box::new(|| {
                HANDOFF_EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();

        std::thread::sleep(SHIM_WINDOW + EXIT_CONFIRM + Duration::from_secs(2));
        assert_eq!(
            HANDOFF_EXITS.load(Ordering::SeqCst),
            0,
            "a launch command handing off must not end the session — this is the field report"
        );
        // Untracked, not Launching or Running: nothing is watching.
        assert_eq!(
            shared.state(),
            GameState::Untracked,
            "nothing is watching this title any more, and the row has to say so"
        );
    }

    /// A matched tree that dies inside [`SHIM_WINDOW`] must not arm the
    /// exit watch. Covers pre-launch jobs that share the game's signals.
    /// Ignored: shim + [`EXIT_CONFIRM`], ~11 s.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~11s (shim window + exit confirmation)"]
    fn a_pre_launch_tree_that_exits_never_ends_the_session() {
        use std::sync::atomic::AtomicUsize;

        // Must be named `sleep`: coreutils dispatches on argv[0]; any other
        // name exits instantly (same trap as [`crate::procscan`]).
        let td = tempfile::tempdir().expect("tempdir");
        let stand_in = td.path().join("sleep");
        std::fs::copy("/bin/sleep", &stand_in).expect("copy a stand-in pre-launch binary");
        let launch_stamp = launch_clock();

        // Shorter than SHIM_WINDOW: a pre-launch job.
        let mut child = std::process::Command::new(&stand_in)
            .arg("3")
            .spawn()
            .expect("spawn the fake pre-launch tree");
        // Reap on another thread: a zombie looks alive.
        std::thread::spawn(move || {
            let _ = child.wait();
        });

        static PRE_EXITS: AtomicUsize = AtomicUsize::new(0);
        PRE_EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                launch_stamp,
                // Scan is the only signal: child already handed off and left.
                ..req("steam:pre-launch", DetectSpec::dir(td.path()), false)
            },
            Box::new(|| {
                PRE_EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();
        assert!(matches!(shared.kind(), LeaseKind::Matched));

        std::thread::sleep(SHIM_WINDOW + EXIT_CONFIRM + Duration::from_secs(3));
        assert_eq!(
            PRE_EXITS.load(Ordering::SeqCst),
            0,
            "a tree that ran before the game must not end the session when it exits — this is the \
             field report"
        );
        assert_ne!(
            shared.state(),
            GameState::Exited,
            "the game never started, so nothing of it can have exited"
        );
    }

    /// Child lease: running, then one exit. Ignored: outlives [`SHIM_WINDOW`]
    /// plus [`EXIT_CONFIRM`], ~12 s.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "drives a real process for ~12s (shim window + exit confirmation)"]
    fn a_real_child_is_tracked_from_running_to_exited() {
        use std::os::unix::process::CommandExt;
        use std::sync::atomic::AtomicUsize;

        let td = tempfile::tempdir().expect("tempdir");
        let script = td.path().join("game.sh");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        let launch_stamp = launch_clock();
        let child = std::process::Command::new("/bin/sh")
            .arg(&script)
            .process_group(0)
            .spawn()
            .expect("spawn the fake game");

        static EXITS: AtomicUsize = AtomicUsize::new(0);
        EXITS.store(0, Ordering::SeqCst);
        let lease = open(
            LeaseRequest {
                game: GameRef {
                    id: Some("custom:live".into()),
                    store: Some("custom".into()),
                    title: "Live Child".into(),
                },
                client: "test".into(),
                fingerprint: None,
                preset: None,
                plane: crate::events::Plane::Native,
                spec: DetectSpec::dir(td.path()),
                nested: false,
                scope_pid: None,
                launcher: false,
                child: Some((child, true)),
                spawned: None,
                launch_stamp,
                procs: None,
                #[cfg(target_os = "linux")]
                workspace: None,
                window: None,
                outcome: None,
            },
            Box::new(|| {
                EXITS.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let shared = lease.shared();
        assert!(matches!(shared.kind(), LeaseKind::Child));

        // `exec` leaves the install dir, so the child is the only signal;
        // the shim window therefore gates it. Waiting it out is the point.
        std::thread::sleep(SHIM_WINDOW + Duration::from_millis(1_500));
        assert_eq!(
            shared.state(),
            GameState::Running,
            "a child that outlives the shim window IS the game"
        );
        assert_eq!(EXITS.load(Ordering::SeqCst), 0, "nothing has exited yet");

        terminate(shared.clone(), "test asked");

        // `sleep` dies on SIGTERM; watcher then confirms gone.
        let deadline = Instant::now() + TERM_GRACE + EXIT_CONFIRM + Duration::from_secs(4);
        while Instant::now() < deadline && shared.state() != GameState::Exited {
            std::thread::sleep(Duration::from_millis(250));
        }
        assert_eq!(shared.state(), GameState::Exited, "the game should be gone");
        // Host-requested end must not fire the session-ending action.
        assert_eq!(
            EXITS.load(Ordering::SeqCst),
            0,
            "a host-requested end must not fire the session-ending action"
        );
    }

    /// Matcher platforms must have a launch stamp. `None` silently disables
    /// the floor ([`crate::procscan::Scanner::find`]).
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn a_launch_always_gets_a_reference_instant_to_adopt_against() {
        assert!(
            launch_clock().is_some(),
            "no launch reference on a platform that matches processes — every pre-existing \
             instance of a title would be adoptable, and endable"
        );
    }

    #[test]
    fn state_strings_are_stable() {
        // Wire strings; renaming one is a protocol change.
        assert_eq!(GameState::Launching.as_str(), "launching");
        assert_eq!(GameState::Running.as_str(), "running");
        assert_eq!(GameState::Exited.as_str(), "exited");
        assert_eq!(GameState::Untracked.as_str(), "untracked");
        assert_eq!(GameState::from_u8(1), GameState::Running);
        assert_eq!(GameState::Window.as_str(), "window");
        assert_eq!(GameState::from_u8(3), GameState::Untracked);
        assert_eq!(GameState::from_u8(4), GameState::Window);
        assert_eq!(GameState::from_u8(99), GameState::Launching);
    }

    /// A toplevel fixture; only the fields the matcher reads matter.
    #[cfg(target_os = "linux")]
    fn win(pid: Option<u32>, app_id: &str) -> crate::vdisplay::Toplevel {
        crate::vdisplay::Toplevel {
            id: "0x1".into(),
            title: "t".into(),
            app_id: app_id.into(),
            pid,
            focused: false,
            fullscreen: false,
            workspace: "1".into(),
            output: "PF-1".into(),
        }
    }

    /// The launcher that opens before the game is not the game's window: its
    /// pid is not in the lease's, and its class is Steam's, not the appid's.
    /// This is the whole point of the stage — landing on Steam's own window
    /// would fire `game.window` while the game is still building a prefix.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_launchers_window_is_never_mistaken_for_the_games() {
        let live = [4242];
        let spec = crate::library::DetectSpec::steam(570);
        // The game: its pid was adopted by the scan.
        assert!(is_game_window(
            &win(Some(4242), "steam_app_570"),
            &live,
            &spec
        ));
        // Steam itself: live pid, but not one of ours.
        assert!(!is_game_window(&win(Some(99), "steam"), &live, &spec));
        // The game, on a pid the scan never adopted: the appid class carries it.
        assert!(is_game_window(
            &win(Some(77), "steam_app_570"),
            &live,
            &spec
        ));
        // A different game's window, same launcher.
        assert!(!is_game_window(
            &win(Some(78), "steam_app_1234"),
            &live,
            &spec
        ));
        // A window with no pid and no class we know is nobody's.
        assert!(!is_game_window(&win(None, ""), &live, &spec));
    }

    /// The operator-typed name matches the way `procscan` matches it, and an
    /// empty `process_name` must never match every window on the desk.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_typed_process_name_matches_a_class_without_matching_everything() {
        let spec = crate::library::DetectSpec {
            process_name: Some("Celeste.exe".into()),
            ..Default::default()
        };
        assert!(is_game_window(&win(Some(5), "celeste"), &[], &spec));
        assert!(!is_game_window(&win(Some(5), "steam"), &[], &spec));
        let blank = crate::library::DetectSpec {
            process_name: Some(String::new()),
            ..Default::default()
        };
        assert!(!is_game_window(&win(Some(5), ""), &[], &blank));
    }

    /// Empty spec must report Untracked, never Running.
    #[test]
    fn a_lease_with_nothing_to_watch_says_so_instead_of_claiming_to_run() {
        let lease = open(
            req("custom:no-signals", DetectSpec::default(), false),
            Box::new(|| {}),
        );
        assert!(!lease.shared().is_trackable());
        assert_eq!(
            lease.shared().state(),
            GameState::Untracked,
            "an unwatchable lease must not report `running`"
        );
    }

    /// Nested with empty spec still reports Running: gamescope node-death
    /// watches it. Must not collapse into Untracked.
    #[test]
    fn a_nested_lease_still_reports_running_because_something_else_watches_it() {
        let lease = open(
            req("steam:nested-no-signals", DetectSpec::default(), true),
            Box::new(|| {}),
        );
        assert_eq!(lease.shared().state(), GameState::Running);
    }

    /// The ladder reaps the child it ended. Unreaped, a zombie answered signal 0, so the
    /// ladder waited the whole grace, and it read as alive to the launch registry, which
    /// then adopted a corpse on every relaunch until a host restart (#1065).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_term_ladder_reaps_the_child() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        let mut r = req("custom:reap", DetectSpec::default(), false);
        r.child = Some((child, false));
        let lease = open(r, Box::new(|| {}));
        let shared = lease.shared();
        drop(lease); // the session left; the watcher hands the child to a waiter
        let started = Instant::now();
        terminate_blocking(&shared);
        assert!(
            started.elapsed() < TERM_GRACE,
            "sleep dies on SIGTERM; no grace to wait out"
        );
        let mut status = 0;
        // SAFETY: probing our own dead child with WNOHANG; ECHILD (reaped) is the pass.
        let r = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        assert_eq!(r, -1, "the child must have been reaped");
    }

    /// A Steam title is launched with `steam steam://rungameid/…`, and with Steam not already
    /// up that process becomes the Steam client. Sweeping its subtree made every Steam window
    /// the game's, so the hold ended on the library window rather than on the game.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_launcher_the_host_started_is_not_the_game_when_the_entry_names_one() {
        let live = [crate::procscan::ProcRef {
            pid: 4242,
            start: 0,
        }];
        let named = window_roots(&DetectSpec::steam(570), &live, Some(88), Some(99), None);
        assert_eq!(
            named,
            vec![4242],
            "the entry says which process is the game; the launcher's is not it"
        );
        // Nothing to match on: the command the host ran is the only handle there is.
        let bare = window_roots(&DetectSpec::default(), &live, Some(88), Some(99), None);
        assert_eq!(bare, vec![4242, 88, 99]);
    }

    /// A `playnite://` title with no detect hint: the matcher finds nothing and the hand-off's
    /// pid is not tracked, so the provider's pid is the only way to its window.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn the_reported_pid_roots_the_window_search() {
        let handoff = window_roots(&DetectSpec::default(), &[], None, None, Some(21056));
        assert_eq!(handoff, vec![21056]);
        let live = [crate::procscan::ProcRef {
            pid: 4242,
            start: 0,
        }];
        let both = window_roots(&DetectSpec::steam(570), &live, None, None, Some(4242));
        assert_eq!(
            both,
            vec![4242],
            "a pid both matched and reported is one root"
        );
    }
}
