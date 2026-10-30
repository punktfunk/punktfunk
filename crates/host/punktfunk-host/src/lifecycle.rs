//! One session's life: where it is, whether it is ending, and why.
//!
//! Every thread and task of a session holds a clone. [`Lifecycle::end`] is the one writer:
//! it latches the reason, marks a deliberate end, raises `stop` and moves the phase to
//! [`Phase::Ending`]. A reader that only polls a flag takes [`Lifecycle::stop_flag`] or
//! [`Lifecycle::quit_flag`]: the display lease, the game lease, admission, the side threads.

use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::events::SessionEndReason;

/// Where a session is. It only moves forward; `/status` reports it per row.
#[derive(Serialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum Phase {
    /// Past the slot and the trust record; nothing built yet.
    Admitted = 0,
    /// The handshake runs; the display prep thread may already be building.
    Negotiating = 1,
    /// Side threads, data plane, display, pipeline, launch, send thread, registry entry.
    BringUp = 2,
    /// The tick loop.
    Streaming = 3,
    /// `end` was called or the loop left: drain, joins, tally.
    Ending = 4,
    /// The connection is closed and the side threads joined.
    Done = 5,
}

impl Phase {
    /// In order; the discriminant is the index, which is what `enter` stores.
    pub const ALL: [Phase; 6] = [
        Phase::Admitted,
        Phase::Negotiating,
        Phase::BringUp,
        Phase::Streaming,
        Phase::Ending,
        Phase::Done,
    ];
}

/// The flags every thread of a session reads, and the one place that writes them.
#[derive(Clone, Debug)]
pub struct Lifecycle {
    stop: Arc<AtomicBool>,
    /// Set with `stop` on a deliberate end: the display skips its keep-alive linger and the
    /// game lease opens no reconnect window.
    quit: Arc<AtomicBool>,
    /// Reached [`Phase::Streaming`], which `phase` forgets once it moves on.
    streamed: Arc<AtomicBool>,
    /// [`SessionEndReason`] as `u8`; 0 until `end`. The first writer wins.
    reason: Arc<AtomicU8>,
    phase: Arc<AtomicU8>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Self::with_quit(Arc::new(AtomicBool::new(false)))
    }

    /// A lifecycle whose deliberate-end flag is `quit`: a plane that keeps one host-wide
    /// (GameStream's `AppState::quit`) shares it with every session it serves.
    pub fn with_quit(quit: Arc<AtomicBool>) -> Self {
        Lifecycle {
            stop: Arc::new(AtomicBool::new(false)),
            quit,
            streamed: Arc::new(AtomicBool::new(false)),
            reason: Arc::new(AtomicU8::new(0)),
            phase: Arc::new(AtomicU8::new(Phase::Admitted as u8)),
        }
    }

    /// End the session for `why`. The first reason wins, so the path that knows (a game
    /// exit, an operator stop, the peer's own close) calls before the generic close that
    /// follows it. Every call raises `stop`; a deliberate `why` raises `quit` too.
    pub fn end(&self, why: SessionEndReason) {
        let _ = self
            .reason
            .compare_exchange(0, why as u8, Ordering::SeqCst, Ordering::Relaxed);
        if why.deliberate() {
            self.quit.store(true, Ordering::SeqCst);
        }
        self.stop.store(true, Ordering::SeqCst);
        self.enter(Phase::Ending);
    }

    /// Move forward to `phase`. A later phase already reached stays.
    pub fn enter(&self, phase: Phase) {
        if phase == Phase::Streaming {
            self.streamed.store(true, Ordering::SeqCst);
        }
        self.phase.fetch_max(phase as u8, Ordering::SeqCst);
    }

    pub fn phase(&self) -> Phase {
        Phase::ALL[self.phase.load(Ordering::SeqCst) as usize]
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// `None` until `end`: a session summarised without one never reached its tail.
    pub fn reason(&self) -> Option<SessionEndReason> {
        SessionEndReason::from_u8(self.reason.load(Ordering::SeqCst))
    }

    pub fn stop_flag(&self) -> &Arc<AtomicBool> {
        &self.stop
    }

    pub fn quit_flag(&self) -> &Arc<AtomicBool> {
        &self.quit
    }

    /// Someone stopped this stream on purpose. A title that exited is deliberate too, but nobody
    /// left: its client may start the next one. A session that never streamed held nothing: a
    /// cancelled handshake and a network check close with the quit code as well.
    pub fn stopped_on_purpose(&self) -> bool {
        self.streamed.load(Ordering::SeqCst)
            && self.quit.load(Ordering::SeqCst)
            && self.reason() != Some(SessionEndReason::GameExited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHASES: [Phase; 6] = Phase::ALL;

    const REASONS: [SessionEndReason; 6] = [
        SessionEndReason::Local,
        SessionEndReason::GameExited,
        SessionEndReason::HostEnded,
        SessionEndReason::HostError,
        SessionEndReason::Lost,
        SessionEndReason::StoppedByOperator,
    ];

    /// Whatever phase a session is in, `end` stops it, records why, and leaves it at
    /// `Ending` or later.
    #[test]
    fn every_phase_can_end() {
        for phase in PHASES {
            let life = Lifecycle::new();
            life.enter(phase);
            assert_eq!(life.phase(), phase);
            life.end(SessionEndReason::Lost);
            assert!(life.stopping(), "{phase:?}");
            assert_eq!(life.reason(), Some(SessionEndReason::Lost), "{phase:?}");
            assert_eq!(life.phase(), phase.max(Phase::Ending), "{phase:?}");
        }
    }

    /// Phases never go backwards: a late `BringUp` after `Streaming` is a no-op.
    #[test]
    fn phases_only_move_forward() {
        let life = Lifecycle::new();
        life.enter(Phase::Streaming);
        life.enter(Phase::BringUp);
        assert_eq!(life.phase(), Phase::Streaming);
        life.enter(Phase::Done);
        life.end(SessionEndReason::HostEnded);
        assert_eq!(life.phase(), Phase::Done);
    }

    /// The first reason is the one the summary reports; a later `end` still raises the flags.
    #[test]
    fn the_first_reason_wins_and_each_is_recorded_once() {
        for first in REASONS {
            for second in REASONS {
                let life = Lifecycle::new();
                assert!(!life.stopping());
                life.end(first);
                life.end(second);
                assert!(life.stopping(), "{first:?} then {second:?}");
                assert_eq!(life.reason(), Some(first), "{first:?} then {second:?}");
            }
        }
    }

    /// `quit` is what the display lease and the game lease read: a client's own stop, a
    /// game exit and an operator stop mean it; a lost link, a fault and a plain finish do not.
    #[test]
    fn a_deliberate_end_raises_quit_and_the_rest_do_not() {
        for why in REASONS {
            let life = Lifecycle::new();
            life.end(why);
            assert_eq!(
                life.quit_flag().load(Ordering::SeqCst),
                matches!(
                    why,
                    SessionEndReason::Local
                        | SessionEndReason::GameExited
                        | SessionEndReason::StoppedByOperator
                ),
                "{why:?}"
            );
        }
    }

    /// Game Mode reads this for its hand-back: a client's or an operator's stop of a stream,
    /// not a title that exited. A plane that only raises the shared flag (`/cancel`) still
    /// counts.
    #[test]
    fn a_title_that_exited_is_not_a_stop_on_purpose() {
        for why in REASONS {
            let life = Lifecycle::new();
            life.enter(Phase::Streaming);
            life.end(why);
            assert_eq!(
                life.stopped_on_purpose(),
                matches!(
                    why,
                    SessionEndReason::Local | SessionEndReason::StoppedByOperator
                ),
                "{why:?}"
            );
        }
        let shared = Lifecycle::new();
        shared.enter(Phase::Streaming);
        assert!(!shared.stopped_on_purpose());
        shared.quit_flag().store(true, Ordering::SeqCst);
        assert!(shared.stopped_on_purpose());
    }

    /// A probe-only session and a cancelled handshake close with the quit code too. Neither
    /// held the box, so neither ends a Game Mode session kept for someone else.
    #[test]
    fn a_session_that_never_streamed_stops_nothing() {
        for phase in [Phase::Admitted, Phase::Negotiating, Phase::BringUp] {
            let life = Lifecycle::new();
            life.enter(phase);
            life.end(SessionEndReason::Local);
            life.enter(Phase::Done);
            assert!(!life.stopped_on_purpose(), "{phase:?}");
        }
    }
}
