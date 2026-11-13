//! Ending a launched game: the polite-then-kill ladder per platform, and ending a game an
//! earlier launch left running.

#[cfg(any(target_os = "linux", windows))]
use super::reported_proc;
#[cfg(target_os = "linux")]
use super::OwnedChild;
use super::{report_exit, LeaseKind, LeaseShared, POLL, TERM_GRACE};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
            report_exit(&shared);
        }
    });
}

/// Blocking ladder, so a test can drive it synchronously.
pub(super) fn terminate_blocking(shared: &LeaseShared) {
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
        }
        LeaseKind::Child | LeaseKind::Matched | LeaseKind::Reported | LeaseKind::Adopted => {
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

/// Whether the host-spawned child is gone, reaping it when it has exited.
/// `waitpid` answers for our own child only: already reaped elsewhere
/// ([`super::reap_later`], the watcher's `try_wait`), or never ours, is gone too.
#[cfg(target_os = "linux")]
fn reap_child(owned: Option<OwnedChild>) -> bool {
    let Some(c) = owned else {
        return true;
    };
    let mut status = 0;
    // SAFETY: WNOHANG never blocks; the only write is the status word handed in.
    unsafe { libc::waitpid(c.pid as i32, &mut status, libc::WNOHANG) != 0 }
}

/// SIGTERM, wait, SIGKILL, reap. Targets are fixed at entry. Each matcher hit is
/// re-verified by start time before a signal ([`crate::procscan::Scanner::alive`]),
/// the child only while the lease still holds it.
#[cfg(target_os = "linux")]
fn unix_term_ladder(shared: &LeaseShared) {
    let scanner = crate::procscan::Scanner::system();
    let owned = shared.owned_child();

    // Group-signal the child when it leads one: that reaches a shell
    // wrapper's grandchildren (the game). A per-pid sweep would miss them.
    // A non-leader's pid is free once the lease forgets it (reaped); a leader's
    // group id stays reserved while the group has members.
    let signal_child = |sig: i32| -> bool {
        let Some(c) = owned else { return false };
        if !c.group_leader && shared.owned_child().is_none() {
            return false;
        }
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
    // Matcher hits, `reported_proc` (the only member for Reported) and an adopted
    // launch's tree (all there is for Adopted: no child handle to group-signal). Taken
    // once: a process that starts after this point belongs to a session that
    // claimed the title while the ladder ran, and must outlive it.
    let targets = {
        let mut procs = shared.find_procs(&scanner);
        for p in reported_proc(shared)
            .into_iter()
            .chain(shared.adopted_tree())
        {
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
    if closed_within(TERM_GRACE, || {
        scanner.alive(&targets).is_empty() && child_gone()
    }) {
        tracing::info!(title = %shared.game.title, "the game closed when asked");
        return;
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
/// [`LeaseShared::spawned`] and an adopted launch's tree, or an empty spec has nothing to end.
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
        shared.adopted_tree().into_iter().for_each(&mut fold);
        procs
    };

    // Taken once: a process started after this belongs to a newer session (as on Unix).
    let targets = live();
    if targets.is_empty() {
        tracing::info!(title = %shared.game.title, "the game is already gone — nothing to end");
        return;
    }
    // WM_CLOSE is the window X; the game can save.
    let asked = signal(&targets, Force::Polite);
    tracing::debug!(
        title = %shared.game.title,
        windows_asked = asked,
        procs = targets.len(),
        grace_s = TERM_GRACE.as_secs(),
        "asked the game to close"
    );
    if closed_within(TERM_GRACE, || scanner.alive(&targets).is_empty()) {
        tracing::info!(title = %shared.game.title, "the game closed when asked");
        return;
    }
    // Fresh (pid, creation) before kill: Windows recycles pids quickly.
    let killed = signal(&scanner.alive(&targets), Force::Kill);
    tracing::warn!(
        title = %shared.game.title,
        killed,
        grace_s = TERM_GRACE.as_secs(),
        "the game did not close when asked — killed it"
    );
}

/// End pids a launch with no lease left adopted: the set it published to
/// [`crate::launchreg`] and what descends from it ([`crate::procscan::tree`]), nothing
/// else. A shell the host started is in the set; the game is its child. For
/// [`crate::session_settings::GameOnNewLaunch::End`] and [`super::end_detached`].
/// No lease reports this exit. The emulator bindings go back in the caller:
/// a new launch's prepare reverts them itself, after which a revert here
/// would undo that launch's own.
///
/// Blocking, bounded by [`TERM_GRACE`].
pub fn end_previous_launch(title: &str, procs: &[crate::procscan::ProcRef], why: &str) -> usize {
    // Taken once, then re-verified before every signal (rule 2): remembered pids recycle.
    let first = crate::procscan::tree(procs);
    if first.is_empty() {
        return 0;
    }
    let live = || crate::procscan::alive(&first);
    tracing::info!(
        title,
        procs = first.len(),
        reason = why,
        "ending a game this host launched earlier"
    );
    signal(&first, Force::Polite);
    if closed_within(TERM_GRACE, || live().is_empty()) {
        tracing::info!(title, "the game closed when asked");
        return first.len();
    }
    let remaining = live();
    tracing::warn!(
        title,
        remaining = remaining.len(),
        grace_s = TERM_GRACE.as_secs(),
        "the game did not close when asked — killing it"
    );
    signal(&remaining, Force::Kill);
    first.len()
}

/// Polls `closed` every [`POLL`] until it holds or `grace` runs out; whether it held. The first
/// look comes after one poll, so a game just asked to close is given time to start.
fn closed_within(grace: Duration, mut closed: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        if closed() {
            return true;
        }
    }
    false
}

/// How hard [`signal`] asks a game to go.
#[derive(Clone, Copy)]
enum Force {
    /// WM_CLOSE / SIGTERM, so the game can save.
    Polite,
    /// TerminateProcess / SIGKILL, for whatever ignored the polite ask.
    Kill,
}

/// Signal each of `procs`; returns how many it reached (windows asked, on Windows). Every pid is
/// matcher-adopted and re-verified by the caller just before: a group signal never comes here.
fn signal(procs: &[crate::procscan::ProcRef], force: Force) -> usize {
    #[cfg(windows)]
    {
        let pids: Vec<u32> = procs.iter().map(|p| p.pid).collect();
        match force {
            Force::Polite => crate::game_term::request_close(&pids),
            Force::Kill => crate::game_term::kill(&pids),
        }
    }
    #[cfg(target_os = "linux")]
    {
        let sig = match force {
            Force::Polite => libc::SIGTERM,
            Force::Kill => libc::SIGKILL,
        };
        procs
            .iter()
            // SAFETY: `kill` returns a status and touches no memory of ours. Always a POSITIVE
            // pid — matcher-adopted, not a group we lead. A negative target would signal an
            // unrelated process group.
            .filter(|p| unsafe { libc::kill(p.pid as i32, sig) == 0 })
            .count()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (procs, force);
        0
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_close_inside_the_grace_stops_the_wait() {
        let mut looks = 0;
        assert!(closed_within(Duration::from_secs(60), || {
            looks += 1;
            looks == 2
        }));
        assert_eq!(looks, 2);
    }

    #[test]
    fn a_game_still_open_at_the_deadline_reads_as_not_closed() {
        let mut looks = 0;
        assert!(!closed_within(Duration::from_millis(1), || {
            looks += 1;
            false
        }));
        assert_eq!(looks, 1);
    }
}
