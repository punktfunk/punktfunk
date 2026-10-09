//! Launched games as `/status` lists them: live sessions' leases, the compat plane's
//! one slot, and launches still waiting for their session.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use super::registry;

/// One launched game as `/status` reports it.
#[derive(Clone)]
pub struct GameSnapshot {
    /// Streaming session, or `None` if the session is gone and the game is in
    /// its reconnect window.
    pub session_id: Option<u64>,
    pub client: String,
    pub app_id: Option<String>,
    pub title: String,
    pub store: Option<String>,
    pub plane: crate::events::Plane,
    /// `launching` / `running` / `window` / `exited` / `untracked`, `grace` on
    /// the reconnect window, or `detached`: still running, no session holds it.
    pub state: &'static str,
    /// `running`, and `window` will follow once the game's window is up.
    pub awaiting_window: bool,
    /// Seconds left before the game is ended. Set only on a `grace` row.
    pub grace_remaining_s: Option<u64>,
    /// Hex fingerprint of the device that launched it; `None` if anonymous.
    pub launched_by: Option<String>,
}

/// Compat plane's launched game, while it has one.
///
/// GameStream is not in the native registry: that holds the loop's `Arc`
/// handles, which the compat plane does not have. One `AppState.launch`
/// means one slot; this is what keeps a Moonlight game on `/status`
/// alongside a native session.
fn gs_game() -> &'static Mutex<Option<Arc<crate::gamelease::LeaseShared>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<crate::gamelease::LeaseShared>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Publish the compat plane's game. The guard retracts it on any stream-loop
/// exit ([`super::LiveSessionGuard`]'s counterpart).
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub fn publish_gamestream_game(shared: Arc<crate::gamelease::LeaseShared>) -> GamestreamGameGuard {
    *gs_game().lock().unwrap_or_else(|e| e.into_inner()) = Some(shared);
    GamestreamGameGuard
}

/// Retracts the compat plane's published game on drop.
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub struct GamestreamGameGuard;

impl Drop for GamestreamGameGuard {
    fn drop(&mut self) {
        *gs_game().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Every launched game the host currently knows: live sessions first, then
/// the compat plane, then games waiting out a reconnect window, then launches
/// still running with no session ([`crate::launchreg::detached`]).
///
/// Sources stay separate — a grace-pending or detached game has no session
/// to hang off, and omitting it would hide a game the player can still end.
pub fn games() -> Vec<GameSnapshot> {
    let mut out: Vec<GameSnapshot> = registry()
        .iter()
        .filter_map(|s| {
            let g = s.game.as_ref()?;
            Some(GameSnapshot {
                session_id: Some(s.id),
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: g.state().as_str(),
                awaiting_window: g.awaits_window(),
                grace_remaining_s: None,
                launched_by: g.fingerprint.clone(),
            })
        })
        .collect();
    out.extend(
        gs_game()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|g| GameSnapshot {
                // Compat plane has no session id. State is never `grace` while
                // streaming, so the console tells this from a grace row by state.
                session_id: None,
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: g.state().as_str(),
                awaiting_window: g.awaits_window(),
                grace_remaining_s: None,
                launched_by: g.fingerprint.clone(),
            }),
    );
    out.extend(
        crate::gamelease::pending_snapshot()
            .into_iter()
            .map(|(g, remaining)| GameSnapshot {
                session_id: None,
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: "grace",
                awaiting_window: false,
                grace_remaining_s: Some(remaining),
                launched_by: g.fingerprint.clone(),
            }),
    );
    out.extend(
        crate::launchreg::detached()
            .into_iter()
            .map(|d| GameSnapshot {
                session_id: None,
                client: d.client().to_string(),
                app_id: d.game.id.clone(),
                title: d.game.title.clone(),
                store: d.game.store.clone(),
                plane: d.plane,
                state: "detached",
                awaiting_window: false,
                grace_remaining_s: None,
                launched_by: Some(d.fingerprint),
            }),
    );
    // A waiting launch's row stands in until its session lists the game itself.
    for (_, w) in waiting()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        if !out
            .iter()
            .any(|g| g.app_id == w.app_id && g.launched_by == w.launched_by)
        {
            out.push(w.clone());
        }
    }
    out
}

/// Launches waiting for their title's files: `launching` rows before their session has a
/// lease, so a client polling `/status` sees the launch it asked for.
fn waiting() -> &'static Mutex<Vec<(u64, GameSnapshot)>> {
    static W: OnceLock<Mutex<Vec<(u64, GameSnapshot)>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

/// One waiting launch's row in [`games`]; dropping it takes the row away.
pub struct WaitingRow(u64);

impl Drop for WaitingRow {
    fn drop(&mut self) {
        waiting()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|(token, _)| *token != self.0);
    }
}

/// List `row` (state `launching`) in [`games`] while the guard lives.
pub fn waiting_launch(row: GameSnapshot) -> WaitingRow {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let token = NEXT.fetch_add(1, Ordering::Relaxed);
    waiting()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((token, row));
    WaitingRow(token)
}

/// Leases on games that are still on a streaming session, filtered by `app_id`
/// (`None` = all of them). What `POST /game/end` reaches when a title is up
/// rather than waiting out a reconnect window.
pub fn live_games(app_id: Option<&str>) -> Vec<Arc<crate::gamelease::LeaseShared>> {
    let mine =
        |g: &Arc<crate::gamelease::LeaseShared>| app_id.is_none() || g.game.id.as_deref() == app_id;
    let mut out: Vec<Arc<crate::gamelease::LeaseShared>> = registry()
        .iter()
        .filter_map(|s| s.game.clone())
        .filter(&mine)
        .collect();
    out.extend(
        gs_game()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|g| mine(g))
            .cloned(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_status::tests::registry_lock;

    /// A Moonlight game has no live-session entry, so it is only on `/status`
    /// while [`publish_gamestream_game`]'s guard is alive.
    #[test]
    fn a_gamestream_game_is_visible_only_while_its_stream_runs() {
        let _registry = registry_lock();
        let id = "steam:1701";
        let mine = || {
            games()
                .into_iter()
                .find(|g| g.app_id.as_deref() == Some(id))
        };

        let lease = crate::gamelease::open(
            crate::gamelease::LeaseRequest {
                game: crate::gamelease::GameRef {
                    id: Some(id.to_string()),
                    store: Some("steam".into()),
                    title: "Test Title".into(),
                },
                client: "192.0.2.7".into(),
                fingerprint: None,
                preset: None,
                plane: crate::events::Plane::Gamestream,
                profile: None,
                // No signals: inert lease, so no watcher thread races the assertions.
                spec: crate::library::DetectSpec::default(),
                nested: false,
                scope_pid: None,
                launcher: false,
                child: None,
                spawned: None,
                launch_stamp: None,
                procs: None,
                #[cfg(target_os = "linux")]
                workspace: None,
                window: None,
                outcome: None,
            },
            Box::new(|| {}),
        );
        assert!(mine().is_none(), "not published yet");

        {
            let _pub = publish_gamestream_game(lease.shared());
            let row = mine().expect("the compat plane's game is reported");
            assert_eq!(
                row.session_id, None,
                "no live-session entry to attribute it to"
            );
            assert_eq!(row.plane, crate::events::Plane::Gamestream);
            assert_eq!(row.client, "192.0.2.7");
            assert_eq!(row.title, "Test Title");
            // Not `grace` while the stream is up: the console keys
            // countdown / End now off that state.
            assert_ne!(row.state, "grace");
            // The same row is what `POST /game/end` reaches with `streaming`,
            // and only ever under its own id.
            assert_eq!(live_games(Some(id)).len(), 1);
            assert!(live_games(Some("steam:9999")).is_empty());
        }
        assert!(mine().is_none(), "the row goes with the stream");
        assert!(
            live_games(Some(id)).is_empty(),
            "a game with no stream left is the grace registry's, not this list's"
        );
    }
}
