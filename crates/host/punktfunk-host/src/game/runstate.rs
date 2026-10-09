//! Provider-plugin liveness reports, used when process scanning cannot decide.
//!
//! A provider PUTs its complete running set, with a pid when known. Whole-set
//! replacement makes missed events and restarts converge on the next update.
//! Reports expire after [`REPORT_TTL`] so a dead plugin cannot keep a session
//! alive; expired entries fall back to [`crate::procscan`]. Complements the
//! static hints in [`crate::library::DetectHint`].

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// How long a provider's report stays authoritative without being restated.
///
/// 90 s: a plugin refreshing every 30 s survives a slow reconcile; a dead
/// plugin stops vetoing a session end within a couple of minutes. Expiring
/// early falls back to scan-only; never expiring leaves a session that cannot end.
pub const REPORT_TTL: Duration = Duration::from_secs(90);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Liveness {
    pub running: bool,
    /// Never trusted as a bare number — every use re-verifies it through
    /// [`crate::procscan`], which pins it to its start time.
    pub pid: Option<u32>,
}

struct Report {
    at: Instant,
    /// Every library id this provider speaks for. Without this, an omitted title
    /// is indistinguishable from a title belonging to some other provider.
    owned: HashSet<String>,
    running: HashMap<String, Option<u32>>,
}

impl Report {
    fn fresh(&self) -> bool {
        self.at.elapsed() < REPORT_TTL
    }
}

fn table() -> MutexGuard<'static, HashMap<String, Report>> {
    static TABLE: OnceLock<Mutex<HashMap<String, Report>>> = OnceLock::new();
    TABLE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Record a provider's report, replacing whatever it said before.
pub fn report(provider: &str, owned: HashSet<String>, running: HashMap<String, Option<u32>>) {
    table().insert(
        provider.to_string(),
        Report {
            at: Instant::now(),
            owned,
            running,
        },
    );
}

pub fn forget(provider: &str) {
    table().remove(provider);
}

/// What a *fresh* provider says about this library id, or `None` when none
/// speaks for it. `None` for every title with no reporting plugin: inert until opt-in.
pub fn opinion(app_id: &str) -> Option<Liveness> {
    let table = table();
    table
        .values()
        .filter(|r| r.fresh())
        .find(|r| r.owned.contains(app_id))
        .map(|r| match r.running.get(app_id) {
            Some(pid) => Liveness {
                running: true,
                pid: *pid,
            },
            None => Liveness {
                running: false,
                pid: None,
            },
        })
}

/// Whether any fresh provider reports liveness for this title, regardless of
/// the current value. Asked when a lease opens: a title whose provider will
/// say when it stops is trackable with no detect signals
/// ([`crate::gamelease::LeaseKind::Reported`]).
pub fn speaks_for(app_id: Option<&str>) -> bool {
    app_id.is_some_and(|id| opinion(id).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    fn running(ids: &[(&str, Option<u32>)]) -> HashMap<String, Option<u32>> {
        ids.iter().map(|(s, p)| ((*s).to_string(), *p)).collect()
    }

    // The table is process-global and these tests run in parallel. Each case uses
    // its own provider and app ids, and cleans up only its own row.

    /// Omitted by its provider is *not running*; a title nobody speaks for has
    /// *no opinion*. Conflating them would make every unreported game look gone.
    #[test]
    fn omitted_is_not_running_but_unknown_is_no_opinion() {
        report(
            "answers-test",
            owned(&["answers:a", "answers:b"]),
            running(&[("answers:a", Some(4242))]),
        );
        assert_eq!(
            opinion("answers:a"),
            Some(Liveness {
                running: true,
                pid: Some(4242)
            })
        );
        assert_eq!(
            opinion("answers:b"),
            Some(Liveness {
                running: false,
                pid: None
            })
        );
        assert_eq!(opinion("answers:never-published"), None);
        assert!(speaks_for(Some("answers:b")));
        assert!(!speaks_for(Some("answers:never-published")));
        assert!(!speaks_for(None));
        forget("answers-test");
    }

    /// A report replaces its predecessor wholesale. A title that dropped out has
    /// stopped; carrying the old entry forward is the stuck-running state.
    #[test]
    fn a_report_replaces_the_previous_one() {
        report(
            "replace-test",
            owned(&["replace:a"]),
            running(&[("replace:a", None)]),
        );
        report("replace-test", owned(&["replace:a"]), running(&[]));
        assert_eq!(
            opinion("replace:a"),
            Some(Liveness {
                running: false,
                pid: None
            })
        );
        forget("replace-test");
        assert_eq!(opinion("replace:a"), None);
    }

    /// A stale report stops counting, so a plugin's claim cannot hold a session
    /// open forever. Seeded with an aged timestamp rather than sleeping 90 s.
    #[test]
    fn a_stale_report_has_no_opinion() {
        table().insert(
            "stale-test".to_string(),
            Report {
                at: Instant::now() - REPORT_TTL - Duration::from_secs(1),
                owned: owned(&["stale:a"]),
                running: running(&[("stale:a", Some(7))]),
            },
        );
        assert_eq!(opinion("stale:a"), None);
        assert!(!speaks_for(Some("stale:a")));
        forget("stale-test");
    }
}
