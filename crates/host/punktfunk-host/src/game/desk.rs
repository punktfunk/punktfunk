//! The desk's own launches: a title the box's own client asked this host to start on the box's
//! own screen, with no stream. The host holds each one here from its claim until the game is
//! gone: the launch record keyed by [`KEY`], play time credited to the owner profile, the game
//! events on [`Plane::Local`] with no device. A desk launch runs the download wait, the launch
//! hold and emulator prepare, and nothing that belongs to a stream: no prep steps, no session
//! guard, no stream marker, no session-end policy.

use crate::events::{Plane, ProfileRef};
use crate::gamelease::{GameLease, GameRef, GameState};
use crate::library::{LaunchTarget, View};
use crate::session_launch::{LaunchOwner, Pads};
use crate::session_status::GameSnapshot;
use crate::vdisplay::Compositor;
use pf_paths::seat::{HostRole, SeatRole};
use punktfunk_core::quic::LaunchOutcome;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The launch record's key for the desk and its rows' client. A key, never a device.
pub(crate) const KEY: &str = "desk";

/// How long a launch refused late, or dead at the start, keeps its `exited` row.
const KEEP: Duration = Duration::from_secs(60);

/// How a game on the host's own screen starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[schema(as = LaunchVia)]
pub(crate) enum Via {
    /// This host starts it.
    Host,
    /// The Steam that owns the box's screen was asked to.
    Steam,
}

/// Why the desk starts nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    NoSuchGame,
    #[cfg_attr(not(windows), allow(dead_code, reason = "Windows only"))]
    NobodySignedIn,
    NoSession,
    NotInSteam,
}

/// One launch, from its claim until the game is gone.
struct Row {
    /// This launch's own number: an exit removes its own row, never a later launch's.
    n: u64,
    game: GameRef,
    /// End called the launch off before it had a lease: the row is hidden, and its worker stops
    /// waiting and starts nothing.
    cancel: Arc<AtomicBool>,
    /// Held while the lease lives: a dropped claim would list the game again as `detached`.
    claim: Option<crate::launchreg::Claim>,
    lease: Option<GameLease>,
    /// The lease's word on a launch that died on the spot.
    said: Option<tokio::sync::mpsc::UnboundedReceiver<LaunchOutcome>>,
    outcome: Option<(LaunchOutcome, Instant)>,
}

impl Row {
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Still listed: launching, a game still up, or an outcome under [`KEEP`] old.
    fn live(&self, now: Instant) -> bool {
        match (&self.outcome, &self.lease) {
            (Some((_, at)), _) => now.saturating_duration_since(*at) < KEEP,
            (None, Some(lease)) => watched(lease.shared().state(), lease.watching()),
            (None, None) => true,
        }
    }

    fn refuse(&mut self, outcome: LaunchOutcome, now: Instant) {
        self.outcome = Some((outcome, now));
        self.claim = None;
        self.lease = None;
        self.said = None;
    }

    fn snapshot(&self) -> GameSnapshot {
        let (state, awaiting_window) = match (&self.outcome, &self.lease) {
            (Some(_), _) => ("exited", false),
            (None, Some(lease)) => {
                let shared = lease.shared();
                (shared.state().as_str(), shared.awaits_window())
            }
            (None, None) => ("launching", false),
        };
        GameSnapshot {
            session_id: None,
            client: KEY.into(),
            app_id: self.game.id.clone(),
            title: self.game.title.clone(),
            store: self.game.store.clone(),
            plane: Plane::Local,
            state,
            awaiting_window,
            grace_remaining_s: None,
            launched_by: Some(KEY.into()),
            outcome: self.outcome.as_ref().map(|(o, _)| o.clone()),
        }
    }
}

/// Whether a lease in `state` still holds its row: a game not yet seen gone, and a watcher
/// alive to see it go. A watcher that ended without reporting an exit leaves a game nobody
/// watches, which the launch record then lists as `detached`.
fn watched(state: GameState, watching: bool) -> bool {
    match state {
        GameState::Launching | GameState::Running | GameState::Window => watching,
        GameState::Exited | GameState::Untracked => false,
    }
}

static DESK: Mutex<Vec<Row>> = Mutex::new(Vec::new());

fn rows() -> MutexGuard<'static, Vec<Row>> {
    DESK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Takes the lease's word on a launch that died on the spot, then drops the rows that are over.
fn sweep(rows: &mut Vec<Row>, now: Instant) {
    for row in rows.iter_mut() {
        if let Some(outcome) = row.said.as_mut().and_then(|rx| rx.try_recv().ok()) {
            row.refuse(outcome, now);
        }
    }
    rows.retain(|row| row.live(now));
}

/// What [`begin`] found.
enum Begin {
    /// A new row, by its number, and the flag End sets to call it off.
    Started(u64, Arc<AtomicBool>),
    /// The desk already runs the title.
    Running,
}

/// A row for `game`, unless one still holds it. A finished or called-off row of the title
/// gives way.
fn begin(game: &GameRef) -> Begin {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut rows = rows();
    sweep(&mut rows, Instant::now());
    if rows
        .iter()
        .any(|r| r.game.id == game.id && r.outcome.is_none() && !r.cancelled())
    {
        return Begin::Running;
    }
    rows.retain(|r| r.game.id != game.id);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(AtomicBool::new(false));
    rows.push(Row {
        n,
        game: game.clone(),
        cancel: cancel.clone(),
        claim: None,
        lease: None,
        said: None,
        outcome: None,
    });
    Begin::Started(n, cancel)
}

/// Row `n` holds its claim and lease. A row already gone drops them.
fn opened(
    n: u64,
    claim: Option<crate::launchreg::Claim>,
    lease: GameLease,
    said: tokio::sync::mpsc::UnboundedReceiver<LaunchOutcome>,
) {
    if let Some(row) = rows().iter_mut().find(|r| r.n == n) {
        row.claim = claim;
        row.lease = Some(lease);
        row.said = Some(said);
    }
}

/// Row `n`'s launch never started: its row says why for [`KEEP`].
fn refused(n: u64, outcome: LaunchOutcome) {
    if let Some(row) = rows().iter_mut().find(|r| r.n == n) {
        row.refuse(outcome, Instant::now());
    }
}

/// Row `n`'s game is gone, or its launch was called off.
fn ended(n: u64) {
    rows().retain(|r| r.n != n);
}

/// The desk's launches as `/status` lists them.
pub(crate) fn snapshot() -> Vec<GameSnapshot> {
    let mut rows = rows();
    sweep(&mut rows, Instant::now());
    let listed = rows.iter().filter(|r| !r.cancelled());
    listed.map(Row::snapshot).collect()
}

/// Ends the desk's game of title `app`, or calls off its launch that has no game yet. Returns
/// how many it asked to end. Always one title: someone at the box plays these.
pub(crate) fn end(app: &str) -> usize {
    let mut called_off = 0;
    let leases: Vec<_> = rows()
        .iter()
        .filter(|r| r.game.id.as_deref() == Some(app))
        .filter_map(|r| match &r.lease {
            Some(lease) => Some(lease.shared()),
            None => {
                let open = r.outcome.is_none() && !r.cancel.swap(true, Ordering::Relaxed);
                called_off += usize::from(open);
                None
            }
        })
        .filter(|s| s.is_trackable() && !s.is_terminating() && s.state() != GameState::Exited)
        .collect();
    let n = leases.len();
    for shared in leases {
        crate::gamelease::terminate(shared, "ended from the management API");
    }
    n + called_off
}

/// Whether this host has a screen of its own to play on: a plain host, or the owner's host at
/// the desk behind the door. A seat, the owner's headless row and the door have none.
pub(crate) fn has_desk(role: HostRole) -> bool {
    matches!(role, HostRole::Plain | HostRole::Seat(SeatRole::Owner))
}

/// How a desk launch starts on this box. Windows starts every title itself. Linux needs a live
/// session (`session`); in the gamescope Steam owns, a Steam title goes to that Steam and any
/// other title has no way in yet.
fn route(
    windows: bool,
    session: Option<Compositor>,
    steam_owns: bool,
    steam_cmd: bool,
) -> Result<Via, Refused> {
    if windows {
        return Ok(Via::Host);
    }
    match session {
        None => Err(Refused::NoSession),
        Some(Compositor::Gamescope) if steam_owns && steam_cmd => Ok(Via::Steam),
        Some(Compositor::Gamescope) if steam_owns => Err(Refused::NotInSteam),
        Some(_) => Ok(Via::Host),
    }
}

/// The live desktop session's compositor, its environment stored for the launch, or the
/// operator's pin. `None` with no live session: a managed one is never stood up for this.
#[cfg(target_os = "linux")]
fn live_session() -> Option<Compositor> {
    let kind = crate::vdisplay::detect_active_session().kind;
    if kind == crate::vdisplay::ActiveKind::None {
        return None;
    }
    crate::vdisplay::detect().ok()
}

#[cfg(not(target_os = "linux"))]
fn live_session() -> Option<Compositor> {
    None
}

/// Starts library title `id` from `view` on the box's own screen, credited to `profile`: how it
/// starts, and `true` when the desk already runs it and nothing new starts. Answers once the
/// title has its row; the download wait, the hold and the spawn run on a thread of their own.
/// Blocking: resolving a title may ask a plugin, and finding the session reads `/proc` and X.
pub(crate) fn start(
    view: &View,
    id: &str,
    profile: Option<ProfileRef>,
) -> Result<(Via, bool), Refused> {
    let target = crate::library::resolve_launch(view, id).ok_or(Refused::NoSuchGame)?;
    #[cfg(windows)]
    if !crate::interactive::signed_in() {
        return Err(Refused::NobodySignedIn);
    }
    let session = live_session();
    #[cfg(target_os = "linux")]
    let (steam_owns, steam_cmd) = (
        session == Some(Compositor::Gamescope) && crate::vdisplay::steam_owns_desk(),
        (target.command.as_deref()).is_some_and(crate::vdisplay::launch_is_steam),
    );
    #[cfg(not(target_os = "linux"))]
    let (steam_owns, steam_cmd) = (false, false);
    let via = route(cfg!(windows), session, steam_owns, steam_cmd)?;
    #[cfg(windows)]
    let at = view.clone();
    #[cfg(target_os = "linux")]
    let at = session.ok_or(Refused::NoSession)?;
    #[cfg(not(any(windows, target_os = "linux")))]
    let at = ();
    let Begin::Started(n, cancel) = begin(&target.game) else {
        return Ok((via, true));
    };
    let owner = LaunchOwner {
        client: KEY.into(),
        fingerprint: Some(KEY.into()),
        plane: Plane::Local,
        preset: None,
        profile,
        pads: Pads::Desk(Vec::new()),
    };
    let title = target.game.title.clone();
    let worker = std::thread::Builder::new()
        .name("pf1-desk".into())
        .spawn(move || run(n, target, owner, at, &cancel));
    if let Err(error) = worker {
        tracing::warn!(%error, "desk launch thread did not start");
        refused(n, not_started(&title));
    }
    Ok((via, false))
}

/// Where a desk launch starts: Windows looks the title up again in the owner's view, Linux
/// spawns into the live session's compositor.
#[cfg(windows)]
type At = View;
#[cfg(target_os = "linux")]
type At = Compositor;
#[cfg(not(any(windows, target_os = "linux")))]
type At = ();

/// Row `n` turns `exited` with [`not_started`] when its worker leaves it with neither a lease nor
/// an outcome, a panic included.
struct Unsettled {
    n: u64,
    title: String,
}

impl Drop for Unsettled {
    fn drop(&mut self) {
        let mut rows = rows();
        let open = |r: &&mut Row| r.n == self.n && r.lease.is_none() && r.outcome.is_none();
        if let Some(row) = rows.iter_mut().find(open) {
            row.refuse(not_started(&self.title), Instant::now());
        }
    }
}

/// Row `n`'s launch: prepare, start, then the lease in the row until the game is gone. A launch
/// that never starts leaves its outcome on the row. Once End sets `cancel`, the download wait
/// stops, nothing spawns, and the row goes with no outcome.
#[cfg_attr(
    not(any(windows, target_os = "linux")),
    allow(unused_variables, reason = "no launch path")
)]
fn run(n: u64, target: LaunchTarget, owner: LaunchOwner, at: At, cancel: &AtomicBool) {
    let _unsettled = Unsettled {
        n,
        title: target.game.title.clone(),
    };
    let called_off = || cancel.load(Ordering::Relaxed);
    let crate::session_launch::Prepared {
        claim,
        stamp,
        declined,
        ..
    } = crate::session_launch::prepare(Some(&target), &owner, &[], &[], &called_off);
    if called_off() {
        // A retry starts the title, as after a session that left during the wait.
        if let Some(c) = &claim {
            c.abandon();
        }
        return ended(n);
    }
    if let Some(refusal) = declined {
        return refused(n, refusal);
    }
    let spawned = crate::session_launch::spawn(
        Some(&target),
        &owner,
        claim.as_ref(),
        #[cfg(target_os = "linux")]
        crate::session_launch::SpawnAt {
            compositor: at,
            nested_spawn: false,
            seat: None,
            place: crate::library::Place::Desk,
        },
        #[cfg(windows)]
        &at,
    );
    if !spawned.now && claim.as_ref().is_none_or(|c| c.must_spawn()) {
        return refused(n, not_started(&target.game.title));
    }
    #[cfg(target_os = "linux")]
    let window = window(
        at,
        target.detect.steam_appid.or_else(|| {
            (target.command.as_deref()).and_then(crate::vdisplay::steam_appid_from_launch)
        }),
    );
    #[cfg(windows)]
    let window = Some(crate::gamelease::WindowSource::Desktop);
    #[cfg(not(any(windows, target_os = "linux")))]
    let window = None;
    let (tx, said) = tokio::sync::mpsc::unbounded_channel();
    let lease = crate::session_launch::lease(
        &target,
        &owner,
        stamp,
        claim.as_ref(),
        spawned,
        crate::session_launch::LeaseExtras {
            window,
            outcome: Some(tx),
            ..Default::default()
        },
        Box::new(move || ended(n)),
    );
    opened(n, claim, lease, said);
}

/// Where the lease looks for the game's window, with no placement: the desktop's windows, or
/// the focused Steam app of a gamescope.
#[cfg(target_os = "linux")]
fn window(
    compositor: Compositor,
    steam_appid: Option<u32>,
) -> Option<crate::gamelease::WindowSource> {
    use crate::gamelease::WindowSource;
    match compositor {
        Compositor::Gamescope => {
            steam_appid.map(|appid| WindowSource::Gamescope { seat: None, appid })
        }
        Compositor::Windows => None,
        compositor => Some(WindowSource::Toplevels {
            compositor,
            stage: None,
        }),
    }
}

/// The refusal for a title the host started nothing for.
fn not_started(title: &str) -> LaunchOutcome {
    use punktfunk_core::quic::LaunchOutcomeKind;
    let text = format!("Couldn't start {title} — this host had nothing to run for it.");
    LaunchOutcome::new(LaunchOutcomeKind::Refused, &text)
        .worded("launch_nothing_to_run", &[("title", title)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every path a desk launch can take, as the box decides it.
    #[test]
    fn a_desk_launch_takes_the_path_its_session_allows() {
        use Compositor::{Gamescope, Kwin, Mutter};
        let host = Ok(Via::Host);
        assert_eq!(route(true, None, false, false), host, "Windows");
        assert_eq!(
            route(true, None, true, true),
            host,
            "Windows has no Game Mode"
        );
        assert_eq!(route(false, None, false, false), Err(Refused::NoSession));
        assert_eq!(route(false, None, false, true), Err(Refused::NoSession));
        for desktop in [Kwin, Mutter] {
            assert_eq!(route(false, Some(desktop), false, false), host);
            assert_eq!(route(false, Some(desktop), false, true), host);
        }
        assert_eq!(
            route(false, Some(Gamescope), false, false),
            host,
            "a gamescope of its own"
        );
        assert_eq!(route(false, Some(Gamescope), false, true), host);
        assert_eq!(route(false, Some(Gamescope), true, true), Ok(Via::Steam));
        assert_eq!(
            route(false, Some(Gamescope), true, false),
            Err(Refused::NotInSteam)
        );
    }

    /// A row lives while its game is up and a watcher is alive to see it go: a watcher that
    /// ended early leaves no row behind, whatever state it left.
    #[test]
    fn a_lease_nobody_watches_holds_no_row() {
        use GameState::{Exited, Launching, Running, Untracked, Window};
        for state in [Launching, Running, Window] {
            assert!(watched(state, true), "{state:?}");
            assert!(!watched(state, false), "{state:?} with its watcher gone");
        }
        for state in [Exited, Untracked] {
            assert!(!watched(state, true) && !watched(state, false), "{state:?}");
        }
    }

    /// A plain host and the owner's at the desk have a screen; a seat, the owner's headless
    /// row and the door do not.
    #[test]
    fn only_the_owner_s_desk_plays_here() {
        for role in HostRole::ALL {
            let want = matches!(role, HostRole::Plain | HostRole::Seat(SeatRole::Owner));
            assert_eq!(has_desk(role), want, "{role:?}");
        }
    }

    fn game(id: &str) -> GameRef {
        GameRef {
            id: Some(id.into()),
            store: Some("custom".into()),
            title: format!("Title {id}"),
        }
    }

    /// `/status` lists a desk launch as a source of its own, as no device's, and the row its
    /// download wait put up gives way to it.
    #[test]
    fn status_lists_a_desk_launch_once() {
        let id = "custom:desk-status-row";
        let Begin::Started(n, _) = begin(&game(id)) else {
            panic!("a fresh title starts");
        };
        // What a download wait for this launch puts up: the same title, under the desk's key.
        let row = snapshot()
            .into_iter()
            .find(|g| g.app_id.as_deref() == Some(id));
        let waiting = crate::session_status::waiting_launch(row.expect("the desk lists it"));
        let listed: Vec<_> = crate::session_status::games()
            .into_iter()
            .filter(|g| g.app_id.as_deref() == Some(id))
            .collect();
        assert_eq!(listed.len(), 1, "one row, not the waiting one beside it");
        let row = &listed[0];
        assert_eq!(
            (row.plane, row.state, row.session_id),
            (Plane::Local, "launching", None)
        );
        assert_eq!(
            (row.client.as_str(), row.launched_by.as_deref()),
            (KEY, Some(KEY))
        );
        drop(waiting);
        ended(n);
        assert!(crate::session_status::games()
            .iter()
            .all(|g| g.app_id.as_deref() != Some(id)));
    }

    /// A refused launch keeps its row for a minute, then it goes; a launching row stays.
    #[test]
    fn a_refused_row_says_why_for_a_minute() {
        let id = "custom:desk-refused-row";
        let Begin::Started(n, _) = begin(&game(id)) else {
            panic!("a fresh title starts");
        };
        assert!(
            matches!(begin(&game(id)), Begin::Running),
            "it is launching"
        );
        refused(n, not_started("Title"));
        let mine = || {
            snapshot()
                .into_iter()
                .find(|g| g.app_id.as_deref() == Some(id))
        };
        let row = mine().expect("the refused row is listed");
        assert_eq!((row.state, row.plane), ("exited", Plane::Local));
        assert_eq!(row.launched_by.as_deref(), Some(KEY));
        let said = row.outcome.expect("its outcome");
        assert_eq!(said.message_id, "launch_nothing_to_run");
        let later = Instant::now() + KEEP;
        let gone = rows().iter().any(|r| r.n == n && !r.live(later));
        assert!(gone, "gone after a minute");
        let Begin::Started(again, _) = begin(&game(id)) else {
            panic!("a refused title starts again");
        };
        ended(again);
        assert!(mine().is_none(), "its row went with it");
    }

    /// A worker that dies before its launch settles leaves the row `exited` with why, and the
    /// title starts again.
    #[test]
    fn a_dead_worker_settles_its_row() {
        let id = "custom:desk-dead-worker";
        let Begin::Started(n, _) = begin(&game(id)) else {
            panic!("a fresh title starts");
        };
        let worker = std::thread::spawn(move || {
            let _unsettled = Unsettled {
                n,
                title: "Title".into(),
            };
            panic!("the worker dies mid-launch");
        });
        assert!(worker.join().is_err(), "the worker panicked");
        let row = snapshot()
            .into_iter()
            .find(|g| g.app_id.as_deref() == Some(id));
        let row = row.expect("the row is still listed");
        assert_eq!(row.state, "exited");
        let said = row.outcome.expect("its outcome");
        assert_eq!(said.message_id, "launch_nothing_to_run");
        let Begin::Started(again, _) = begin(&game(id)) else {
            panic!("the title starts again");
        };
        ended(again);
    }
}

/// A desk launch with a real child: no session holds it, and the registry is process-wide, so
/// each test has a title of its own.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod lease_tests {
    use super::*;
    use crate::gamelease::SHIM_WINDOW;

    pub(crate) fn target(id: &str, command: &str) -> LaunchTarget {
        LaunchTarget {
            game: GameRef {
                id: Some(id.into()),
                store: Some("custom".into()),
                title: format!("Desk {id}"),
            },
            launcher: false,
            detect: crate::library::DetectSpec::default(),
            command: Some(command.into()),
            own_workspace: false,
            on_window: crate::library::OnWindow::default(),
        }
    }

    fn owner() -> LaunchOwner {
        LaunchOwner {
            client: KEY.into(),
            fingerprint: Some(KEY.into()),
            plane: Plane::Local,
            preset: None,
            profile: Some(ProfileRef {
                id: "0d0d0d0d0d0d".into(),
                display_name: "Owner".into(),
            }),
            pads: Pads::Desk(Vec::new()),
        }
    }

    /// Starts `target` as [`start`] does once it has a row, on a desktop that is not there.
    pub(crate) fn launch(target: LaunchTarget) -> u64 {
        let Begin::Started(n, cancel) = begin(&target.game) else {
            panic!("{} is already running", target.game.title);
        };
        std::thread::spawn(move || run(n, target, owner(), Compositor::Kwin, &cancel));
        n
    }

    pub(crate) fn state(id: &str) -> Option<&'static str> {
        snapshot()
            .into_iter()
            .find(|g| g.app_id.as_deref() == Some(id))
            .map(|g| g.state)
    }

    pub(crate) fn wait_for(id: &str, want: Option<&str>, within: Duration) {
        let deadline = Instant::now() + within;
        while state(id) != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(state(id), want, "{id}");
    }

    fn stats(id: &str) -> crate::library::TitleStats {
        crate::library::game_stats().remove(id).unwrap_or_default()
    }

    /// The game runs with no session and the row holds it; a second launch adopts it; quitting
    /// it takes the row away and credits the run to the owner.
    #[test]
    fn a_desk_game_is_held_until_it_exits_and_its_play_time_counts() {
        let _config = crate::identity::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let id = "custom:desk-held";
        let before = stats(id).totals;
        launch(target(id, "sleep 8"));
        wait_for(id, Some("running"), SHIM_WINDOW + Duration::from_secs(5));
        assert!(
            matches!(begin(&target(id, "sleep 8").game), Begin::Running),
            "a second launch adopts the running one"
        );
        wait_for(id, None, Duration::from_secs(15));
        let after = stats(id);
        assert_eq!(after.totals.launch_count, before.launch_count + 1);
        assert!(
            after.totals.play_time_ms > before.play_time_ms,
            "the run counts"
        );
        let mine = after.by_profile.get("0d0d0d0d0d0d").expect("the owner's");
        assert!(mine.play_time_ms > 0);
    }

    /// End reaches a desk game nobody streams, and its row goes with it.
    #[test]
    fn end_game_ends_a_desk_game() {
        let _config = crate::identity::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let id = "custom:desk-ended";
        launch(target(id, "sleep 60"));
        wait_for(id, Some("running"), SHIM_WINDOW + Duration::from_secs(5));
        assert_eq!(end("custom:desk-other"), 0, "another title");
        assert_eq!(end(id), 1);
        assert_eq!(end(id), 0, "already ending");
        wait_for(id, None, Duration::from_secs(15));
    }

    /// End calls off a launch that has no game yet: it counts, the row goes at once, and the
    /// worker starts nothing and leaves no row behind.
    #[test]
    fn end_calls_off_a_launch_before_its_spawn() {
        let _config = crate::identity::CONFIG_DIR_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let id = "custom:desk-called-off";
        let target = target(id, "exit 0");
        let Begin::Started(n, cancel) = begin(&target.game) else {
            panic!("a fresh title starts");
        };
        assert_eq!(end(id), 1);
        assert_eq!(end(id), 0, "already called off");
        assert_eq!(state(id), None, "the row goes at once");
        run(n, target, owner(), Compositor::Kwin, &cancel);
        // A spawn would have left the row holding a lease, a refusal its outcome.
        let left = rows().iter().any(|r| r.n == n);
        assert!(!left, "the worker dropped its row");
    }
}
