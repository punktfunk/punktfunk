//! Cross-crate topology-churn deadline and the topology TRANSACTION generation (immunity plan
//! WP10 item 5).
//!
//! The exclusive-topology reassert (`pf-vdisplay` manager) opens a window before it
//! evicts or restores displays. The IDD-push descriptor follower (`pf-capture`) skips
//! samples while [`held`] is true: a reassert bounce is a transient mode, and acting
//! on it recreates the capture ring at a mode the recovery is about to undo. The
//! generation-keyed recovery rebuild (`restart_presentation_in_place`) is not gated — only
//! passive following is.
//!
//! [`hold`] is a deadline, not a flag: it self-expires so a holder that dies mid-churn
//! cannot wedge following off. [`release`] expires early when the watchdog sees a
//! stable topology. The plan keeps this latch until the transaction path has a fault test.
//!
//! On top of the latch, a mutation runs as a [`Txn`]: [`begin`] opens the hold for the
//! transaction's deadline and names it; [`finish`] records the outcome and bumps
//! [`generation`] ONLY when the topology was observed to change — never merely because a
//! `SetDisplayConfig` was attempted. Descriptor samples name the generation they were
//! observed under, so two samples straddling a transaction never pass a debounce together.
//!
//! Contract tests: `hold_release_expire`, `generation_moves_only_on_observed_change`.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, OnceLock,
};
use std::time::{Duration, Instant};

/// `0` = no hold.
static HOLD_UNTIL_MS: AtomicU64 = AtomicU64::new(0);
/// Bumps on every transaction that OBSERVED a topology change.
static GENERATION: AtomicU64 = AtomicU64::new(0);
/// Hands out transaction ids.
static NEXT_TXN: AtomicU64 = AtomicU64::new(1);
/// The most recent finished transaction, for diagnostics.
static LAST: Mutex<Option<Finished>> = Mutex::new(None);

/// `Instant` cannot live in an atomic.
fn clock() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// `fetch_max`: a reassert racing a slot transition must not shorten the other hold.
pub fn hold(dur: Duration) {
    let until = clock().saturating_add(dur.as_millis() as u64);
    HOLD_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
}

pub fn release() {
    HOLD_UNTIL_MS.store(0, Ordering::Relaxed);
}

#[must_use]
pub fn held() -> bool {
    clock() < HOLD_UNTIL_MS.load(Ordering::Relaxed)
}

/// One topology mutation in flight. Dropping it without [`finish`] leaves the outcome unknown:
/// the hold expires on its own (the dead-holder escape) and the generation does not move.
#[derive(Debug)]
#[must_use = "finish() records the outcome; a dropped transaction is an unknown one"]
pub struct Txn {
    pub id: u64,
    pub reason: &'static str,
    pub started: Instant,
    pub deadline: Instant,
}

/// What a finished transaction found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A verification read showed the topology changed as intended.
    Changed,
    /// Verified: nothing needed to change (already in the desired state).
    Unchanged,
    /// The verification read failed or the deadline passed — state unknown.
    Unknown,
}

/// A finished transaction, for diagnostics ([`last`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Finished {
    pub id: u64,
    pub reason: &'static str,
    pub outcome: Outcome,
    pub took: Duration,
    /// The generation after this transaction.
    pub generation: u64,
}

/// Open a transaction: hold descriptor-following for `deadline` (the same latch as [`hold`]) and
/// name the mutation. The caller performs its `SetDisplayConfig`s, then [`finish`]es with what
/// its verification read observed.
pub fn begin(reason: &'static str, deadline: Duration) -> Txn {
    hold(deadline);
    let now = Instant::now();
    Txn {
        id: NEXT_TXN.fetch_add(1, Ordering::Relaxed),
        reason,
        started: now,
        deadline: now + deadline,
    }
}

/// Close a transaction. `Changed` — and only `Changed` — bumps [`generation`]; a deadline that
/// already passed downgrades any outcome to `Unknown` (the mutation may still land later, and a
/// later snapshot reconciles it). The hold is NOT released here: the swap-chain bounce a
/// topology write causes follows the write, and the deadline covers it.
pub fn finish(txn: Txn, outcome: Outcome) -> Finished {
    let now = Instant::now();
    let outcome = if now > txn.deadline && outcome == Outcome::Changed {
        Outcome::Unknown
    } else {
        outcome
    };
    let generation = if outcome == Outcome::Changed {
        GENERATION.fetch_add(1, Ordering::AcqRel) + 1
    } else {
        GENERATION.load(Ordering::Acquire)
    };
    let f = Finished {
        id: txn.id,
        reason: txn.reason,
        outcome,
        took: now.saturating_duration_since(txn.started),
        generation,
    };
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    f
}

/// The topology generation: the count of transactions that OBSERVED a change.
#[must_use]
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The most recently finished transaction, if any.
#[must_use]
pub fn last() -> Option<Finished> {
    *LAST.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two tests share process-global state; serialize them.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn hold_release_expire() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        release();
        assert!(!held());
        hold(Duration::from_secs(60));
        assert!(held());
        // A shorter overlapping hold must not shorten the window.
        hold(Duration::from_millis(1));
        assert!(held());
        release();
        assert!(!held());
        // A millisecond-scale hold lapses on its own.
        hold(Duration::from_millis(30));
        assert!(held());
        let deadline = Instant::now() + Duration::from_millis(200);
        while held() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!held());
    }

    #[test]
    fn generation_moves_only_on_observed_change() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        release();
        let g0 = generation();
        // Attempted, verified unchanged: the generation stays.
        let t = begin("noop", Duration::from_secs(5));
        assert!(held(), "a transaction holds following for its deadline");
        let f = finish(t, Outcome::Unchanged);
        assert_eq!((f.outcome, f.generation), (Outcome::Unchanged, g0));
        // Attempted, verification failed: UNKNOWN, generation stays.
        let f = finish(begin("blind", Duration::from_secs(5)), Outcome::Unknown);
        assert_eq!((f.outcome, generation()), (Outcome::Unknown, g0));
        // Observed change: exactly one bump.
        let f = finish(begin("isolate", Duration::from_secs(5)), Outcome::Changed);
        assert_eq!(
            (f.outcome, f.generation, generation()),
            (Outcome::Changed, g0 + 1, g0 + 1)
        );
        assert_eq!(last().map(|l| (l.reason, l.id)), Some(("isolate", f.id)));
        // A claimed change past the deadline is Unknown, not a generation bump.
        let t = begin("late", Duration::from_millis(1));
        while Instant::now() <= t.deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let f = finish(t, Outcome::Changed);
        assert_eq!((f.outcome, generation()), (Outcome::Unknown, g0 + 1));
        release();
    }
}
