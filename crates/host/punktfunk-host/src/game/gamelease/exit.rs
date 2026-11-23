//! Whether a launched game has exited, decided once per poll.
//!
//! Pure: no clock of its own and no process table. The watcher gathers a [`Sighting`], hands
//! it to [`Presence::step`], and carries out the [`Verdict`] (`Watcher::watch_exit`).
//!
//! The order of the signals is the contract:
//! 1. The launch's own process leaving is the exit when nothing else names the game.
//! 2. A live process outranks every report.
//! 3. A provider's word is final both ways while its report is fresh.
//! 4. Otherwise the game exits once unseen for [`EXIT_CONFIRM`]; its launcher's running
//!    flag holds that off, up to [`VETO_LIMIT`].
//!
//! Pin with the table tests below. Design: `design/session-game-lifetime.md`.

// macOS has no watcher: only the tests read this module there.

use super::{EXIT_CONFIRM, VETO_LIMIT};
use std::time::{Duration, Instant};

/// The game between two polls of phase 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Presence {
    Up,
    /// Unseen since `since`. `vetoed` once the launcher's flag has held an exit off.
    Missing {
        since: Instant,
        vetoed: bool,
    },
}

/// What one poll saw of the game.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Sighting {
    /// The process this launch started left, and no signal names anything else as the game.
    OwnLeft,
    /// A matched process, or the launch's own child, is up.
    Alive,
    /// Nothing of it on the box. `said_running` is its provider's word; `None` when no
    /// provider speaks for the title.
    Unseen { said_running: Option<bool> },
}

/// What the watcher does with a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// Seen: record what it runs as.
    Running,
    /// Unseen, and its provider says it runs.
    Held,
    /// Unseen, not long enough to call.
    Waiting,
    /// The launcher's flag holds the exit off, from this poll on.
    Vetoed,
    /// Gone. `stale_for` is how long it was unseen when the launcher's flag still said
    /// running; `None` when the flag did not oppose the exit.
    Exited {
        why: &'static str,
        stale_for: Option<Duration>,
    },
}

/// Exited: gone ≥ [`EXIT_CONFIRM`], and either unopposed or gone ≥ [`VETO_LIMIT`] despite
/// the launcher's flag.
fn exit_confirmed(gone_for: Duration, flag_running: bool) -> bool {
    gone_for >= EXIT_CONFIRM && (!flag_running || gone_for >= VETO_LIMIT)
}

impl Presence {
    /// `flag_running` reads the launcher's own running flag ([`crate::procscan::running_hint`]).
    /// It is asked only once the game has been unseen for [`EXIT_CONFIRM`]: on Windows it is a
    /// registry read, and no earlier verdict depends on it.
    pub(super) fn step(
        &mut self,
        seen: Sighting,
        now: Instant,
        flag_running: impl FnOnce() -> bool,
    ) -> Verdict {
        let exited = |why| Verdict::Exited {
            why,
            stale_for: None,
        };
        let said_running = match seen {
            Sighting::OwnLeft => return exited("the launched process exited"),
            Sighting::Alive => {
                *self = Presence::Up;
                return Verdict::Running;
            }
            Sighting::Unseen { said_running } => said_running,
        };
        match said_running {
            Some(false) => return exited("its provider reported the game stopped"),
            Some(true) => {
                *self = Presence::Up;
                return Verdict::Held;
            }
            None => {}
        }
        // Continuous absence. A veto does not restart it: the absence is the veto's bound.
        let (since, vetoed) = match *self {
            Presence::Missing { since, vetoed } => (since, vetoed),
            Presence::Up => (now, false),
        };
        *self = Presence::Missing { since, vetoed };
        let gone_for = now.saturating_duration_since(since);
        if gone_for < EXIT_CONFIRM {
            return Verdict::Waiting;
        }
        let flag = flag_running();
        if exit_confirmed(gone_for, flag) {
            return Verdict::Exited {
                why: "the game exited",
                stale_for: flag.then_some(gone_for),
            };
        }
        if vetoed {
            return Verdict::Waiting;
        }
        *self = Presence::Missing {
            since,
            vetoed: true,
        };
        Verdict::Vetoed
    }
}

#[cfg(test)]
mod tests {
    use super::Sighting::{Alive, OwnLeft, Unseen};
    use super::*;

    const NO_WORD: Sighting = Unseen { said_running: None };
    const SAYS_UP: Sighting = Unseen {
        said_running: Some(true),
    };
    const SAYS_GONE: Sighting = Unseen {
        said_running: Some(false),
    };

    fn gone(why: &'static str) -> Verdict {
        Verdict::Exited {
            why,
            stale_for: None,
        }
    }

    /// One poll per row, in order, from a game that is up: seconds since the watch began,
    /// what the poll saw, the launcher's flag, and the verdict.
    fn run(rows: &[(u64, Sighting, bool, Verdict)]) {
        let t0 = Instant::now();
        let mut state = Presence::Up;
        for (i, &(at, seen, flag, want)) in rows.iter().enumerate() {
            let got = state.step(seen, t0 + Duration::from_secs(at), || flag);
            assert_eq!(got, want, "row {i} at {at}s: {seen:?}, flag {flag}");
        }
    }

    /// `running_hint` may delay an exit; it must not delay it forever.
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
    fn a_game_is_gone_once_unseen_for_the_confirm_window() {
        run(&[
            (0, Alive, false, Verdict::Running),
            (1, NO_WORD, false, Verdict::Waiting),
            (3, NO_WORD, false, Verdict::Waiting),
            (4, NO_WORD, false, gone("the game exited")),
        ]);
    }

    /// A re-exec or a launcher restart is shorter than the window, and starts it over.
    #[test]
    fn a_sighting_inside_the_window_starts_the_absence_over() {
        run(&[
            (0, NO_WORD, false, Verdict::Waiting),
            (2, NO_WORD, false, Verdict::Waiting),
            (3, Alive, false, Verdict::Running),
            (4, NO_WORD, false, Verdict::Waiting),
            (6, NO_WORD, false, Verdict::Waiting),
            (7, NO_WORD, false, gone("the game exited")),
        ]);
    }

    /// The flag delays the exit, says so once, and is overruled at the limit.
    #[test]
    fn the_launchers_flag_holds_an_exit_off_only_to_its_limit() {
        let limit = VETO_LIMIT.as_secs();
        let stale = Verdict::Exited {
            why: "the game exited",
            stale_for: Some(VETO_LIMIT),
        };
        run(&[
            (0, NO_WORD, true, Verdict::Waiting),
            (3, NO_WORD, true, Verdict::Vetoed),
            (4, NO_WORD, true, Verdict::Waiting),
            (limit - 1, NO_WORD, true, Verdict::Waiting),
            (limit, NO_WORD, true, stale),
        ]);
        // The flag clearing ends the veto at once.
        run(&[
            (0, NO_WORD, true, Verdict::Waiting),
            (3, NO_WORD, true, Verdict::Vetoed),
            (4, NO_WORD, false, gone("the game exited")),
        ]);
    }

    /// The flag is a registry read on Windows: never before the window has passed, and never
    /// while something else already decides.
    #[test]
    fn the_launchers_flag_is_read_only_when_it_can_change_the_verdict() {
        let t0 = Instant::now();
        let unread = || -> bool { panic!("the flag was read") };
        let mut state = Presence::Up;
        for (at, seen) in [(0, Alive), (1, NO_WORD), (2, NO_WORD), (9, SAYS_UP)] {
            state.step(seen, t0 + Duration::from_secs(at), unread);
        }
        assert_eq!(
            state.step(SAYS_GONE, t0, unread),
            gone("its provider reported the game stopped")
        );
        assert_eq!(
            state.step(OwnLeft, t0, unread),
            gone("the launched process exited")
        );
    }

    /// A fresh report decides both ways, and a live one starts the absence over.
    #[test]
    fn a_providers_word_outranks_the_scan_finding_nothing() {
        run(&[
            (0, SAYS_UP, false, Verdict::Held),
            (60, SAYS_UP, false, Verdict::Held),
            // The report expired: the absence counts from here, not from the first poll.
            (61, NO_WORD, false, Verdict::Waiting),
            (63, NO_WORD, false, Verdict::Waiting),
            (64, NO_WORD, false, gone("the game exited")),
        ]);
        let stopped = gone("its provider reported the game stopped");
        run(&[(0, SAYS_GONE, true, stopped)]);
    }

    #[test]
    fn the_launchs_own_process_leaving_is_the_exit_when_nothing_else_names_the_game() {
        run(&[
            (0, Alive, false, Verdict::Running),
            (1, OwnLeft, true, gone("the launched process exited")),
        ]);
    }
}
