//! The copy writers' lock, and the device lists that lie behind it.
//!
//! A removal holds the level throughout, the keep-level thread for each seat it refreshes, and a
//! start or adoption while it writes a trust copy, an environment file or the box's library: a
//! refresh never writes a removed seat's files back, and two writers never share a temp file or
//! put back an environment file's old token. Each person's row's device list is reached only
//! through a held [`Level`], so a change, its save and the trust copy it rewrites always run
//! under the level first, then the lists' own lock. The level is not re-entrant: a path that
//! already holds one is handed it ([`CopyLocks::under`]).

use super::devices;
use crate::model::SeatId;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

type Lists = HashMap<SeatId, devices::Held>;

#[derive(Default)]
pub(super) struct CopyLocks {
    level: Mutex<()>,
    devices: Mutex<Lists>,
}

/// The level, held.
pub(super) struct Level<'a> {
    _held: MutexGuard<'a, ()>,
    devices: &'a Mutex<Lists>,
}

impl CopyLocks {
    pub(super) fn level(&self) -> Level<'_> {
        Level {
            _held: self.level.lock().unwrap_or_else(|e| e.into_inner()),
            devices: &self.devices,
        }
    }

    /// The level once it is free, or `None` when it stays held past `wait`: a removal holds it
    /// while a unit stops.
    pub(super) fn level_within(&self, wait: Duration) -> Option<Level<'_>> {
        let until = Instant::now() + wait;
        loop {
            let held = match self.level.try_lock() {
                Ok(held) => held,
                Err(TryLockError::Poisoned(held)) => held.into_inner(),
                Err(TryLockError::WouldBlock) if Instant::now() < until => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(TryLockError::WouldBlock) => return None,
            };
            return Some(Level {
                _held: held,
                devices: &self.devices,
            });
        }
    }

    /// `f` under `held`, or under the level taken for `f` alone.
    pub(super) fn under<T>(
        &self,
        held: Option<&mut Level<'_>>,
        f: impl FnOnce(&mut Level<'_>) -> T,
    ) -> T {
        match held {
            Some(level) => f(level),
            None => f(&mut self.level()),
        }
    }
}

impl Level<'_> {
    /// Each person's row's device list, loaded on first use.
    pub(super) fn devices(&mut self) -> MutexGuard<'_, Lists> {
        self.devices.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A held level is waited for and then given up on; a free one is taken at once.
    #[test]
    fn a_held_level_is_given_up_on_past_the_wait() {
        let locks = CopyLocks::default();
        let held = locks.level();
        assert!(locks.level_within(Duration::from_millis(60)).is_none());
        drop(held);
        assert!(locks.level_within(Duration::ZERO).is_some());
    }
}
