//! The lock every writer of a seat's copied files takes, and the device lists it guards.
//!
//! A seat's copied files are its trust copy (its read-only copy of the box's pairing store and
//! profiles), its environment file and the box's library copy. Three callers write them:
//!
//! - Deleting a seat holds the lock for the whole delete, so the sync thread can't write a
//!   deleted seat's files back.
//! - The sync thread holds it for each seat it refreshes.
//! - A start or an adopt holds it while it writes.
//!
//! So two writers never share a temp file, and none puts back an environment file's old token.
//!
//! Lock order: [`CopyLock`] first, then the device lists' own mutex. The lists can only be
//! reached through a held [`CopyGuard`], so a change to a list, its save and the trust copy it
//! rewrites always happen in that order. The lock is not re-entrant: a caller that already
//! holds it passes its guard down ([`CopyLock::with`]).

use super::devices;
use crate::model::SeatId;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

/// The device list of each person's row.
type Lists = HashMap<SeatId, devices::Held>;

#[derive(Default)]
pub(super) struct CopyLock {
    writers: Mutex<()>,
    devices: Mutex<Lists>,
}

/// The copy lock, held.
pub(super) struct CopyGuard<'a> {
    _held: MutexGuard<'a, ()>,
    devices: &'a Mutex<Lists>,
}

impl CopyLock {
    pub(super) fn lock(&self) -> CopyGuard<'_> {
        CopyGuard {
            _held: self.writers.lock().unwrap_or_else(|e| e.into_inner()),
            devices: &self.devices,
        }
    }

    /// The lock once it is free, or `None` when it stays held past `wait`. Deleting a seat
    /// holds it while the seat's unit stops, which can take seconds.
    pub(super) fn lock_within(&self, wait: Duration) -> Option<CopyGuard<'_>> {
        let until = Instant::now() + wait;
        loop {
            let held = match self.writers.try_lock() {
                Ok(held) => held,
                Err(TryLockError::Poisoned(held)) => held.into_inner(),
                Err(TryLockError::WouldBlock) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(TryLockError::WouldBlock) => return None,
            };
            return Some(CopyGuard {
                _held: held,
                devices: &self.devices,
            });
        }
    }

    /// Runs `f` with the guard the caller already `held`, or takes the lock for `f` alone.
    pub(super) fn with<T>(
        &self,
        held: Option<&mut CopyGuard<'_>>,
        f: impl FnOnce(&mut CopyGuard<'_>) -> T,
    ) -> T {
        match held {
            Some(guard) => f(guard),
            None => f(&mut self.lock()),
        }
    }
}

impl CopyGuard<'_> {
    /// The device list of each person's row. A list is loaded the first time it is asked for.
    pub(super) fn devices(&mut self) -> MutexGuard<'_, Lists> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held lock is waited for and then given up on. A free one is taken at once.
    #[test]
    fn a_held_lock_is_given_up_on_past_the_wait() {
        let lock = CopyLock::default();
        let held = lock.lock();
        assert!(lock.lock_within(Duration::from_millis(60)).is_none());
        drop(held);
        assert!(lock.lock_within(Duration::ZERO).is_some());
    }
}
